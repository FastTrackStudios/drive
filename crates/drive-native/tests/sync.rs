//! Two native clients and a node in one process: edits made on one arrive
//! on the other, and edits made offline arrive after reconnecting.

use std::time::Duration;

use drive_native::Drive;

async fn node() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let app = drive_node::router(data.path(), Vec::new(), Default::default()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("ws://{addr}/sync"), data)
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("did not happen");
}

#[tokio::test]
async fn edits_reach_the_other_client() {
    let (url, _data) = node().await;
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Drive::connect(&url, "a", a_dir.path()).await;
    let b = Drive::connect(&url, "b", b_dir.path()).await;
    let da = a.open("test:doc").await;
    let db = b.open("test:doc").await;
    a.synced("test:doc").await;
    b.synced("test:doc").await;
    da.get_map("m").insert("k", "from a").unwrap();
    a.commit("test:doc");
    eventually(|| db.get_map("m").get("k").is_some()).await;
}

#[tokio::test]
async fn a_copy_on_disk_opens_and_syncs_later() {
    let (url, _data) = node().await;
    let dir = tempfile::tempdir().unwrap();
    // Offline: no node at this address.
    let offline = Drive::connect("ws://127.0.0.1:9/sync", "a", dir.path()).await;
    let d = offline.open("test:doc").await;
    d.get_map("m").insert("k", "made offline").unwrap();
    offline.commit("test:doc");
    tokio::time::sleep(Duration::from_millis(700)).await; // saved to disk
    // Later, connected, the same folder: the copy opens, and reaches others.
    let a = Drive::connect(&url, "a", dir.path()).await;
    let da = a.open("test:doc").await;
    assert!(
        da.get_map("m").get("k").is_some(),
        "the copy on disk opened"
    );
    a.synced("test:doc").await;
    let b_dir = tempfile::tempdir().unwrap();
    let b = Drive::connect(&url, "b", b_dir.path()).await;
    let db = b.open("test:doc").await;
    b.synced("test:doc").await;
    eventually(|| db.get_map("m").get("k").is_some()).await;
}

/// An edit of one document made (and committed) inside an edit of another
/// reaches the node, both of them (drive#53).
#[tokio::test]
async fn a_nested_edit_reaches_the_node() {
    let (url, _data) = node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = Drive::connect(&url, "a", dir.path()).await;
    let reg = a.open("o/drive:projects").await;
    let meta = a.open("other/studio:files:s").await;
    a.synced("o/drive:projects").await;
    a.synced("other/studio:files:s").await;
    meta.get_map("org").insert("id", "other").unwrap();
    // registry.edit(|reg| meta.edit(|m| …)): the inner commit first.
    reg.get_map("projects")
        .ensure_mergeable_map("p")
        .unwrap()
        .insert("part", "x")
        .unwrap();
    let m = meta.get_map("links").ensure_mergeable_map("part").unwrap();
    m.insert("kind", "chart").unwrap();
    a.commit("other/studio:files:s");
    a.commit("o/drive:projects");

    // Another client with nothing on disk: the node has both edits.
    let b_dir = tempfile::tempdir().unwrap();
    let b = Drive::connect(&url, "b", b_dir.path()).await;
    let reg_b = b.open("o/drive:projects").await;
    let meta_b = b.open("other/studio:files:s").await;
    b.synced("o/drive:projects").await;
    b.synced("other/studio:files:s").await;
    eventually(|| {
        meta_b.get_deep_value() == meta.get_deep_value()
            && reg_b.get_deep_value() == reg.get_deep_value()
    })
    .await;
}
