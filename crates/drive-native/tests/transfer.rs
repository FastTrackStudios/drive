//! Files from a device over iroh, through a node (docs/transfer.md): a
//! node in this process with an endpoint of its own, and a device whose
//! agent serves a folder with `drive_transfer::Provider`, its endpoint in
//! the registry. Browsed files and location files stream from it by the
//! ranges HTTP clients ask for; a whole `/fetch` is kept on the node.

use std::sync::Arc;
use std::time::Duration;

use drive_native::{Drive, Event};
use drive_proto::{Browse, Browsed};
use drive_transfer::{Provider, Settings};

fn local() -> Settings {
    Settings {
        relay: None,
        local_only: true,
    }
}

async fn node() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let opts = drive_node::Options {
        no_backfill: true,
        iroh: local(),
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
    (format!("http://{addr}"), data)
}

fn noise(n: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A device serving `folder` (as its root `/Volumes/dev-drive`, and as
/// the location `loc-1`).
async fn device(
    url: &str,
    dir: &std::path::Path,
    folder: std::path::PathBuf,
) -> (Drive, Arc<Provider>) {
    let d = Drive::connect(
        &format!("{}/sync", url.replace("http", "ws")),
        "Voyager",
        dir,
    )
    .await;
    let space = drive_files::space_doc("home");
    let reg = d.open(&space).await;
    d.synced(&space).await;
    drive_files::space::add_device(
        &reg,
        &drive_files::space::Device {
            id: "voyager".into(),
            name: "Voyager".into(),
            kind: "laptop".into(),
            os: "macos".into(),
            roles: vec!["client".into()],
            approved_by: String::new(),
            added: 0,
        },
    )
    .unwrap();
    drive_files::space::add_location(
        &reg,
        &drive_files::space::Location {
            id: "loc-1".into(),
            name: "Memos".into(),
            device: "voyager".into(),
            path: folder.to_string_lossy().into_owned(),
            volume: "/".into(),
            storage: String::new(),
            mirror_of: String::new(),
            index: String::new(),
            files: 0,
            bytes: 0,
            built: 0,
        },
    )
    .unwrap();
    // As the agent: only the registry's node and devices get in.
    let doc = reg.clone();
    let allow: drive_transfer::Allow =
        Arc::new(move |id| drive_files::space::allowed_endpoints(&doc).contains(&id.to_string()));
    let p = Arc::new(
        Provider::start(&dir.join("transfer"), &local(), allow)
            .await
            .unwrap(),
    );
    drive_files::space::set_endpoint(
        &reg,
        "voyager",
        &p.id().to_string(),
        &drive_transfer::addr_to_string(&p.addr()),
    )
    .unwrap();
    d.commit(&space);
    d.register_agent("home", "voyager");
    let mut events = d.events();
    let (answer, prov) = (d.clone(), p.clone());
    tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let Event::Browse { req, ask } = ev {
                let file = match &ask {
                    Browse::Provide { root, path } if root == "/Volumes/dev-drive" => {
                        Some(folder.join(path))
                    }
                    Browse::ProvideLocation { location, path } if location == "loc-1" => {
                        Some(folder.join(path))
                    }
                    _ => None,
                };
                let r = match file {
                    Some(f) => prov.provide(&f).await.map(|got| {
                        Browsed::Provided(drive_proto::Provided {
                            hash: got.hash,
                            size: got.size,
                            mtime: got.mtime,
                            endpoint: drive_transfer::addr_to_string(&prov.addr()),
                        })
                    }),
                    None => Err("no such file or folder".into()),
                };
                answer.browsed(req, r);
            }
        }
    });
    // The node names its endpoint in the registry once the agent is in.
    for _ in 0..50 {
        if !drive_files::space::node_endpoints(&reg).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!drive_files::space::node_endpoints(&reg).is_empty());
    (d, p)
}

async fn get(url: String, range: Option<&str>) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let mut r = reqwest::Client::new().get(url);
    if let Some(h) = range {
        r = r.header("range", h);
    }
    let r = r.send().await.unwrap();
    let (st, h) = (r.status().as_u16(), r.headers().clone());
    (st, h, r.bytes().await.unwrap().to_vec())
}

#[tokio::test]
async fn ranges_stream_from_the_device_over_iroh() {
    let (url, _data) = node().await;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Memos");
    std::fs::create_dir_all(&folder).unwrap();
    let take = noise(3_000_017);
    std::fs::write(folder.join("take.wav"), &take).unwrap();
    let (_d, _p) = device(&url, dir.path(), folder.clone()).await;
    let hash = blake3_hex(&take);

    // Browsing: whole, then ranges (seeking).
    let base = format!("{url}/browse/home/voyager/file?root=%2FVolumes%2Fdev-drive&path=take.wav");
    let (st, h, body) = get(base.clone(), None).await;
    assert_eq!(st, 200);
    assert_eq!(h["content-type"], "audio/wav");
    assert_eq!(h["content-security-policy"], "sandbox");
    assert_eq!(h["content-length"], take.len().to_string().as_str());
    assert!(body == take);
    for (range, a, b) in [
        ("bytes=0-0", 0, 0),
        ("bytes=16383-16385", 16_383, 16_385),
        ("bytes=1000000-1999999", 1_000_000, 1_999_999),
        ("bytes=-10", take.len() - 10, take.len() - 1),
        ("bytes=2999990-", 2_999_990, take.len() - 1),
    ] {
        let (st, h, body) = get(base.clone(), Some(range)).await;
        assert_eq!(st, 206, "{range}");
        assert_eq!(
            h["content-range"],
            format!("bytes {a}-{b}/{}", take.len()).as_str()
        );
        assert!(body == take[a..=b], "{range}");
    }
    let (st, _, _) = get(base.clone(), Some("bytes=9999999-")).await;
    assert_eq!(st, 416);
    // Browsing keeps nothing on the node.
    let (st, _, _) = get(format!("{url}/blobs/{hash}"), None).await;
    assert_eq!(st, 404);

    // A location's file: a range streams, the whole file is kept.
    let fetch = format!("{url}/fetch/home/loc-1?path=take.wav");
    let (st, _, body) = get(fetch.clone(), Some("bytes=5-20")).await;
    assert_eq!(st, 206);
    assert!(body == take[5..=20]);
    let (st, _, body) = get(fetch.clone(), None).await;
    assert_eq!(st, 200);
    assert!(body == take);
    let mut kept = false;
    for _ in 0..50 {
        let (st, _, body) = get(format!("{url}/blobs/{hash}"), None).await;
        if st == 200 {
            assert!(body == take);
            kept = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(kept, "a whole fetch is kept on the node");
    // And then served from there, ranges too.
    let (st, _, body) = get(fetch, Some("bytes=100-199")).await;
    assert_eq!(st, 206);
    assert!(body == take[100..200]);

    // Outside what the device serves: refused, not served.
    let (st, _, _) = get(
        format!("{url}/browse/home/voyager/file?root=%2Fetc&path=passwd"),
        None,
    )
    .await;
    assert_eq!(st, 404);
}

fn blake3_hex(b: &[u8]) -> String {
    drive_transfer::Hash::new(b).to_hex().to_string()
}
