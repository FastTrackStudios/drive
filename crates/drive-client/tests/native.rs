//! The client in a native app: two of them on a node in this process, an
//! edit made by one arriving at the other, and the copy kept on disk.

#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;

use drive_client::{Drive, Event, Status};
use futures_util::StreamExt;

/// The copies' folder, one for every test here (it is process-wide).
fn copies() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap()).path();
    drive_client::set_data_dir(dir);
    dir
}

async fn node() -> (String, tempfile::TempDir) {
    node_at("127.0.0.1:0").await
}

async fn node_at(addr: &str) -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let opts = drive_node::Options {
        no_backfill: true,
        ..Default::default()
    };
    let app = drive_node::router(data.path(), Vec::new(), opts).await;
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (format!("ws://{addr}/sync"), data)
}

/// Wait for an event that matches, up to a few seconds.
async fn until(rx: &mut drive_client::EventStream, want: impl Fn(&Event) -> bool) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = rx.next().await {
            if want(&e) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_reaches_the_other_app_and_is_kept_here() {
    let (url, _data) = node().await;
    let copies = copies();

    let (a, mut a_events) = Drive::connect(Some(url.clone()));
    let (b, mut b_events) = Drive::connect(Some(url));
    assert!(
        until(&mut a_events, |e| matches!(
            e,
            Event::Status(Status::Online)
        ))
        .await
    );
    assert!(
        until(&mut b_events, |e| matches!(
            e,
            Event::Status(Status::Online)
        ))
        .await
    );

    let id = "home/test:doc";
    let doc_b = b.open(id);
    let doc_a = a.open(id);
    assert!(until(&mut a_events, |e| matches!(e, Event::Changed(d) if d == id)).await);
    doc_a.get_map("song").insert("title", "Know Me").unwrap();
    a.commit(id);

    let arrived = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if doc_b.get_map("song").get("title").is_some() {
                return true;
            }
            if b_events.next().await.is_none() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(arrived, "b sees a's edit");
    assert!(a.is_synced(id));

    // The copy is saved here a moment after a change.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let file = copies.join("docs").join(drive_native::file_name(id));
    assert!(file.exists(), "{} is kept", file.display());
    assert!(!a.me().name.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_offline_is_sent_when_the_node_comes() {
    copies();
    // Nothing listens there yet.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let (a, _a_events) = Drive::connect(Some(format!("ws://{addr}/sync")));
    let id = "home/test:offline";
    let doc_a = a.open(id);
    doc_a.get_map("song").insert("title", "Offline").unwrap();
    a.commit(id);
    assert_ne!(a.status(), Status::Online);

    let (url, _data) = node_at(&addr.to_string()).await;
    let (b, mut b_events) = Drive::connect(Some(url));
    let doc_b = b.open(id);
    let arrived = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if doc_b.get_map("song").get("title").is_some() {
                return true;
            }
            if b_events.next().await.is_none() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(arrived, "b sees the edit a made offline");
}

#[tokio::test(flavor = "multi_thread")]
async fn save_now_keeps_the_copy_at_once() {
    let copies = copies();
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let (a, _a_events) = Drive::connect(Some(format!("ws://{addr}/sync")));
    let id = "home/test:save-now";
    let doc = a.open(id);
    doc.get_map("song").insert("title", "Quick").unwrap();
    a.commit(id);
    a.save_now();

    // Well before the usual save a moment later.
    let file = copies.join("docs").join(drive_native::file_name(id));
    let mut kept = false;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if file.exists() {
            kept = true;
            break;
        }
    }
    assert!(kept, "{} is kept at once", file.display());
}
