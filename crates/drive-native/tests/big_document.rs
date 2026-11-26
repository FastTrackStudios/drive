//! A document bigger than a websocket frame's default limit (16 MiB) still
//! syncs both ways (drive#172). voyager's copy of its space document was
//! 52 MiB behind the node's; the node answered its join with one 52 MiB
//! message, the agent's websocket refused it as too big, and the agent
//! reconnected about once a second without ever catching up.

use std::time::Duration;

use drive_native::Drive;

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

/// `len` bytes nothing compresses (xorshift).
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_document_past_sixteen_mib_reaches_another_device() {
    let (url, _data) = node().await;
    let id = "home/drive:space";
    let blob = noise(24 << 20);

    let a_dir = tempfile::tempdir().unwrap();
    let a = Drive::connect(&url, "airlock", a_dir.path()).await;
    let doc = a.open(id).await;
    a.synced(id).await;
    doc.get_map("m")
        .insert("blob", loro::LoroValue::Binary(blob.clone().into()))
        .unwrap();
    a.commit(id);

    let b_dir = tempfile::tempdir().unwrap();
    let b = Drive::connect(&url, "voyager", b_dir.path()).await;
    let got = b.open(id).await;
    let arrived = tokio::time::timeout(Duration::from_secs(60), async {
        // Through the join's answer, or (if the join came first) the
        // update the node passes on.
        while got.get_map("m").get("blob").is_none() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        arrived.is_ok(),
        "the 24 MiB document never reached the second device"
    );
    let value = got.get_map("m").get("blob").unwrap().into_value().unwrap();
    assert_eq!(value, loro::LoroValue::Binary(blob.into()));
}
