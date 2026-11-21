//! Browsing a device's disks through a node (docs/browse.md): a node in
//! this process, and a device whose agent answers from a small made-up
//! disk. Checks the node's side: folders a page at a time, files streamed
//! and by ranges, the offline answers, and what stays after the device
//! goes (the folder pages it saw).

use std::time::Duration;

use drive_native::{Drive, Event};
use drive_proto::{Browse, Browsed, Chunk, DirEntry, LIST_MAX, Listing, Root};

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
    (format!("http://{addr}"), data)
}

/// The made-up disk: `/Volumes/dev-drive` with `Songs/` (3000 files) and
/// `mix.wav` (300 000 bytes).
fn disk(ask: Browse) -> Result<Browsed, String> {
    let wav: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    match ask {
        Browse::Roots => Ok(Browsed::Roots(vec![Root {
            mount: "/Volumes/dev-drive".into(),
            name: "dev-drive".into(),
            fs: "apfs".into(),
            total: 1000,
            free: 400,
            removable: true,
            home: false,
        }])),
        Browse::List {
            root,
            path,
            offset,
            limit,
            ..
        } => {
            if root != "/Volumes/dev-drive" {
                return Err("not a volume of this device".into());
            }
            let all: Vec<DirEntry> = match path.as_str() {
                "" => vec![
                    DirEntry {
                        name: "Songs".into(),
                        dir: true,
                        size: 0,
                        mtime: 1,
                        link: false,
                    },
                    DirEntry {
                        name: "mix.wav".into(),
                        dir: false,
                        size: wav.len() as u64,
                        mtime: 2,
                        link: false,
                    },
                ],
                "Songs" => (0..3000)
                    .map(|i| DirEntry {
                        name: format!("s{i:04}.txt"),
                        dir: false,
                        size: 1,
                        mtime: 3,
                        link: false,
                    })
                    .collect(),
                _ => return Err("a path outside the volume".into()),
            };
            let limit = limit.min(LIST_MAX) as usize;
            Ok(Browsed::Listing(Listing {
                total: all.len() as u32,
                entries: all.into_iter().skip(offset as usize).take(limit).collect(),
                offset,
                hidden: 0,
            }))
        }
        Browse::Read {
            path, start, len, ..
        } if path == "mix.wav" => {
            let s = (start as usize).min(wav.len());
            let e = (s + len as usize).min(wav.len());
            Ok(Browsed::Chunk(Chunk {
                size: wav.len() as u64,
                mtime: 2,
                start,
                bytes: wav[s..e].to_vec(),
            }))
        }
        Browse::Index { path, .. } if path == "Songs" => Ok(Browsed::Key("loc-1".into())),
        _ => Err("no such file or folder".into()),
    }
}

async fn device(url: &str, dir: &std::path::Path) -> Drive {
    let d = Drive::connect(
        &format!("{}/sync", url.replace("http", "ws")),
        "Voyager",
        dir,
    )
    .await;
    let reg = d.open(&drive_files::space_doc("home")).await;
    d.synced(&drive_files::space_doc("home")).await;
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
    d.commit(&drive_files::space_doc("home"));
    d.register_agent("home", "voyager");
    let mut events = d.events();
    let answer = d.clone();
    tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let Event::Browse { req, ask } = ev {
                answer.browsed(req, disk(ask));
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    d
}

async fn json(url: String) -> (u16, serde_json::Value) {
    let r = reqwest::get(url).await.unwrap();
    let st = r.status().as_u16();
    (
        st,
        serde_json::from_str(&r.text().await.unwrap()).unwrap_or_default(),
    )
}

#[tokio::test]
async fn browse_a_device_then_while_it_is_off() {
    let (url, _data) = node().await;
    // The device runs on a runtime of its own, so stopping that runtime
    // closes its connection: the device goes offline.
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready) = std::sync::mpsc::channel::<()>();
    let u = url.clone();
    let dev = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _d = rt.block_on(device(&u, dir.path()));
        ready_tx.send(()).unwrap();
        let _ = stopped.recv();
        drop(rt);
    });
    tokio::task::spawn_blocking(move || ready.recv().unwrap())
        .await
        .unwrap();
    let base = format!("{url}/browse/home/voyager");
    let root = "%2FVolumes%2Fdev-drive";

    let (st, v) = json(format!("{base}/roots")).await;
    assert_eq!(st, 200);
    assert_eq!(v["online"], true);
    assert_eq!(v["roots"][0]["name"], "dev-drive");

    // A big folder, a page at a time.
    let (_, v) = json(format!("{base}/list?root={root}&path=Songs&limit=100000")).await;
    assert_eq!(v["total"], 3000);
    assert_eq!(v["entries"].as_array().unwrap().len(), LIST_MAX as usize);
    let (_, v) = json(format!(
        "{base}/list?root={root}&path=Songs&offset=2000&limit=2000"
    ))
    .await;
    assert_eq!(v["entries"].as_array().unwrap().len(), 1000);
    assert_eq!(v["entries"][0]["name"], "s2000.txt");
    let (_, top) = json(format!("{base}/list?root={root}&path=")).await;
    assert_eq!(top["entries"][1]["name"], "mix.wav");

    // Refusals pass through as such.
    let (st, _) = json(format!("{base}/list?root=%2Fetc&path=")).await;
    assert_eq!(st, 403);
    let (st, _) = json(format!("{url}/browse/home/nobody/roots")).await;
    assert_eq!(st, 404);

    // A whole file, streamed in chunks; then a range of it.
    let wav: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let r = reqwest::get(format!("{base}/file?root={root}&path=mix.wav"))
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "audio/wav");
    assert_eq!(r.headers()["content-security-policy"], "sandbox");
    assert_eq!(r.bytes().await.unwrap().to_vec(), wav);
    let r = reqwest::Client::new()
        .get(format!("{base}/file?root={root}&path=mix.wav"))
        .header("range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 1000-1999/300000");
    assert_eq!(r.bytes().await.unwrap().to_vec(), wav[1000..2000].to_vec());

    // Index this folder.
    let r = reqwest::Client::new()
        .post(format!("{base}/index"))
        .header("content-type", "application/json")
        .body(r#"{"root":"/Volumes/dev-drive","path":"Songs","name":"Songs"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    // The device goes: what was seen still lists, marked offline.
    stop.send(()).unwrap();
    tokio::task::spawn_blocking(move || dev.join().unwrap())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (st, v) = json(format!("{base}/list?root={root}&path=")).await;
    assert_eq!(st, 200);
    assert_eq!(v["online"], false);
    assert_eq!(v["why"], "Voyager is offline");
    assert_eq!(v["entries"][1]["name"], "mix.wav");
    let (st, v) = json(format!("{base}/list?root={root}&path=Never")).await;
    assert_eq!((st, v["state"].as_str()), (503, Some("offline")));
    let (_, v) = json(format!("{base}/roots")).await;
    assert_eq!(
        (v["online"].as_bool(), v["roots"][0]["name"].as_str()),
        (Some(false), Some("dev-drive"))
    );
    let r = reqwest::get(format!("{base}/file?root={root}&path=mix.wav"))
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);
}
