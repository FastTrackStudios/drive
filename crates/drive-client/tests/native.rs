//! The client in a native app: two of them on a node in this process, an
//! edit made by one arriving at the other, and the copy kept on disk.

#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;

use drive_client::{Drive, Event, Status};
use futures_util::StreamExt;

async fn node() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let opts = drive_node::Options {
        no_backfill: true,
        ..Default::default()
    };
    let app = drive_node::router(data.path(), Vec::new(), opts).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
    let copies = tempfile::tempdir().unwrap();
    drive_client::set_data_dir(copies.path());

    let (a, mut a_events) = Drive::connect(Some(url.clone()));
    let (b, mut b_events) = Drive::connect(Some(url));
    assert!(until(&mut a_events, |e| matches!(e, Event::Status(Status::Online))).await);
    assert!(until(&mut b_events, |e| matches!(e, Event::Status(Status::Online))).await);

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
    let file = copies
        .path()
        .join("docs")
        .join(drive_native::file_name(id));
    assert!(file.exists(), "{} is kept", file.display());
    assert!(!a.me().name.is_empty());
}
