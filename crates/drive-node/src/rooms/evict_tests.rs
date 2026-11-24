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
    rooms.rooms.lock().await.len()
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
