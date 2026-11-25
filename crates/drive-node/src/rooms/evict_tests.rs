//! Rooms are not kept for ever (drive#61): a link to a site or a share
//! that does not exist opens none, and a room nobody has open, with
//! nothing left to save, goes once it has been idle.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tower::ServiceExt;

use super::Rooms;

async fn rooms(dir: &std::path::Path) -> Arc<Rooms> {
    let store = crate::store::Store::open(&dir.join("drive.db"))
        .await
        .unwrap();
    Arc::new(Rooms::new(store, crate::auth::Auth::new(None)))
}

async fn open_rooms(rooms: &Rooms) -> usize {
    rooms.sync.loaded().await
}

#[tokio::test]
async fn links_to_sites_and_shares_that_do_not_exist_open_no_room() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    let catalog = crate::catalog::Catalog {
        rooms: rooms.clone(),
        blobs: drive_files::storage::StorageConfig::Fs {
            root: dir.path().join("blobs").to_string_lossy().into_owned(),
        }
        .blobs()
        .unwrap(),
        names: Default::default(),
        reports: Default::default(),
    };
    let app = Router::new()
        .route("/sites/{space}/{id}/", get(crate::sites::index))
        .route("/s/{space}/{token}", get(crate::share::page))
        .with_state(catalog);
    for n in 0..20 {
        for url in [format!("/sites/r{n}/s{n}/"), format!("/s/r{n}/t{n}")] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::get(&url)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND, "{url}");
        }
    }
    assert_eq!(open_rooms(&rooms).await, 0);
}

#[tokio::test]
async fn an_idle_saved_room_nobody_has_open_is_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    rooms
        .edit("o/notes:a", "t", |d| {
            d.get_text("t").insert(0, "kept").unwrap();
        })
        .await;
    rooms.read("o/notes:b", |_| ()).await;

    // Only read, nothing to save: gone. Not saved yet: kept, whatever its age.
    rooms.evict_idle(Duration::ZERO).await;
    assert_eq!(open_rooms(&rooms).await, 1);

    // Saved, but used just now: kept.
    rooms.flush_now().await;
    rooms.evict_idle(Duration::from_secs(3600)).await;
    assert_eq!(open_rooms(&rooms).await, 1);

    // Saved and idle: gone, and what it held is read back from the store.
    rooms.evict_idle(Duration::ZERO).await;
    assert_eq!(open_rooms(&rooms).await, 0);
    let text = rooms
        .read("o/notes:a", |d| d.get_text("t").to_string())
        .await;
    assert_eq!(text, "kept");
}

#[tokio::test]
async fn a_room_someone_has_open_is_kept() {
    use drive_proto::ToNode;
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    let (send, rx) = tokio::sync::mpsc::unbounded_channel();
    let (out, mut recv) = tokio::sync::mpsc::unbounded_channel();
    let incoming =
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
    let done = tokio::spawn(super::session(
        rooms.clone(),
        Box::pin(incoming),
        out,
        true,
        Duration::from_secs(3600),
    ));
    send.send(ToNode::Hello {
        version: drive_proto::VERSION,
        name: "t".into(),
        token: None,
    })
    .unwrap();
    recv.recv().await.unwrap();
    send.send(ToNode::Join {
        doc: "o/notes:a".into(),
        version_vector: Vec::new(),
    })
    .unwrap();
    recv.recv().await.unwrap();

    rooms.evict_idle(Duration::ZERO).await;
    assert_eq!(open_rooms(&rooms).await, 1, "a joined room stays");

    drop(send);
    done.await.unwrap();
    rooms.evict_idle(Duration::ZERO).await;
    assert_eq!(open_rooms(&rooms).await, 0, "left and saved: let go");
}

#[tokio::test]
async fn listing_documents_opens_no_room_for_other_orgs() {
    use drive_files::projects;
    use drive_proto::{ToClient, ToNode};
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    // Their org: a registry lending one song to ours; another org with
    // nothing for us.
    let theirs = drive_files::space_doc("theirs");
    rooms
        .edit(&theirs, "t", |d| {
            let p = projects::add_project(d, "Song", "song").unwrap();
            projects::link_doc(d, &p, "notes:shared", true).unwrap();
            projects::share_with_org(d, &p, "ours", true).unwrap();
        })
        .await;
    rooms
        .edit(&drive_files::space_doc("else"), "t", |_| ())
        .await;
    for id in [
        "ours/notes:a",
        "theirs/notes:shared",
        "theirs/notes:private",
        "else/notes:x",
    ] {
        rooms.edit(id, "t", |_| ()).await;
    }
    rooms.flush_now().await;
    rooms.evict_idle(Duration::ZERO).await;
    assert_eq!(open_rooms(&rooms).await, 0);

    let who = crate::auth::Principal {
        who: "me".into(),
        orgs: vec!["ours".into()],
        all: false,
    };
    let (out, mut recv) = tokio::sync::mpsc::unbounded_channel();
    let mut joined = std::collections::HashSet::new();
    rooms
        .handle(
            1,
            "me",
            &who,
            &out,
            &mut joined,
            ToNode::List {
                prefix: "*/notes:".into(),
            },
        )
        .await;
    let ToClient::Docs { docs, .. } = recv.recv().await.unwrap() else {
        panic!("expected the documents");
    };
    let ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["ours/notes:a", "theirs/notes:shared"]);
    assert_eq!(open_rooms(&rooms).await, 0, "no registry was opened");
}
