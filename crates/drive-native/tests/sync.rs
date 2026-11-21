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
