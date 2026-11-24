//! Files for apps, end to end in one process: a node, two agents (Voyager
//! has the files, Battleship's apps want them) talking over iroh (direct
//! addresses only), and apps on the socket through `drive-local`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use drive_files::{projects, space, tree};
use drive_local::{Event, How, OpenOptions};

use super::resolve::Source;
use super::*;
use crate::agent::{Agent, Config};

#[cfg(unix)]
fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn port_tokens_are_full_length_and_unpredictable() {
    // Same process, same thread, same millisecond: still different tokens.
    let (a, b) = (new_token().unwrap(), new_token().unwrap());
    assert_ne!(a, b);
    for t in [&a, &b] {
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}

#[test]
fn large_app_cache_budgets_do_not_wrap() {
    let budget = Settings {
        budget_gib: Some(1 << 34),
        ..Default::default()
    };
    assert_eq!(budget.budget(), u64::MAX);
}

async fn node() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let opts = drive_node::Options {
        no_backfill: true,
        no_iroh: true,
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

struct Dev {
    a: Arc<Agent>,
    apps: Arc<Apps>,
    dir: tempfile::TempDir,
    disk: tempfile::TempDir,
}

async fn device(url: &str, id: &str, name: &str) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let disk = tempfile::tempdir().unwrap();
    let cfg = Config {
        node: format!("{}/sync", url.replace("http", "ws")),
        space: "home".into(),
        device_id: id.into(),
        device_name: name.into(),
        storages: Vec::new(),
        pins: Vec::new(),
        token: None,
        token_expires: None,
        signed_in_as: None,
        allow_evict: false,
        relay: None,
        allow_rule_moves: false,
        kache: Default::default(),
        apps: Default::default(),
    };
    cfg.save(dir.path()).unwrap();
    #[cfg(unix)]
    assert_eq!(mode_of(&dir.path().join("config.json")), 0o600);
    let a = Agent::start_with(dir.path(), cfg, name, dir.path().join("state")).await;
    a.drive.online().await;
    space::add_device(&a.space, &crate::agent::this_device(id, name)).unwrap();
    a.commit_space();
    let a = Arc::new(a);
    let apps = Apps::open(&a).unwrap();
    a.start_transfer_with(
        drive_transfer::Settings {
            relay: None,
            local_only: true,
        },
        vec![peer::protocol(&a, &apps)],
    )
    .await
    .unwrap();
    Dev { a, apps, dir, disk }
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| ((i as u64).wrapping_mul(2654435761) >> 7) as u8 ^ seed)
        .collect()
}

struct World {
    _node: tempfile::TempDir,
    url: String,
    v: Dev,
    b: Dev,
    loc: drive_files::space::Location,
}

/// Voyager has `Scratch` (a take, two drum samples, a note); Battleship
/// has nothing. Both see each other's endpoint and Voyager's location.
async fn world() -> World {
    let (url, node_dir) = node().await;
    let v = device(&url, "voyager", "Voyager").await;
    let b = device(&url, "battleship", "Battleship").await;
    let d = v.disk.path();
    std::fs::create_dir_all(d.join("Mixes")).unwrap();
    std::fs::create_dir_all(d.join("Kits")).unwrap();
    std::fs::write(d.join("Mixes/take.wav"), pattern(3_000_000, 1)).unwrap();
    std::fs::write(d.join("Kits/kick.wav"), pattern(40_000, 2)).unwrap();
    std::fs::write(d.join("Kits/snare.wav"), pattern(50_000, 3)).unwrap();
    std::fs::write(d.join("notes.txt"), b"bridge twice").unwrap();
    let loc = v.a.add_location("Scratch", d).unwrap();
    v.a.index_location(&loc).await.unwrap();
    let (va, ba) = (v.a.clone(), b.a.clone());
    wait_until(
        "each sees the other's endpoint and the location",
        move || {
            let eb = space::endpoints(&ba.space);
            let ev = space::endpoints(&va.space);
            eb.contains_key("voyager")
                && ev.contains_key("battleship")
                && space::locations(&ba.space)
                    .iter()
                    .any(|l| l.name == "Scratch")
        },
    )
    .await;
    World {
        _node: node_dir,
        url,
        v,
        b,
        loc,
    }
}

fn nothing(_: u64) {}

/// Every file under a folder: path → (bytes' BLAKE3, mtime, mode).
fn snapshot(root: &Path) -> BTreeMap<String, (String, std::time::SystemTime, u32)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let m = std::fs::metadata(&p).unwrap();
            #[cfg(unix)]
            let mode = std::os::unix::fs::PermissionsExt::mode(&m.permissions());
            #[cfg(not(unix))]
            let mode = 0;
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            if m.is_dir() {
                out.insert(rel, (String::new(), m.modified().unwrap(), mode));
                stack.push(p);
            } else {
                let b = std::fs::read(&p).unwrap();
                out.insert(
                    rel,
                    (
                        blake3::hash(&b).to_hex().to_string(),
                        m.modified().unwrap(),
                        mode,
                    ),
                );
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn open_resolves_each_kind_of_source() {
    let w = world().await;
    let opts = OpenOptions::new();

    // This device's own location: the file itself, never copied.
    let f =
        w.v.apps
            .open_for(1, "Scratch:Kits/kick.wav", &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(f.how, How::Local);
    assert_eq!(
        f.path,
        std::fs::canonicalize(w.v.disk.path().join("Kits/kick.wav")).unwrap()
    );
    assert_eq!(f.hash, None);
    assert_eq!(w.v.apps.cache.stats().0, 0, "nothing copied");
    // Asked for its hash: one read, the full BLAKE3.
    let mut with_hash = OpenOptions::new();
    with_hash.hash = true;
    let f =
        w.v.apps
            .open_for(1, "Scratch:Kits/kick.wav", &with_hash, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(
        f.hash.unwrap(),
        blake3::hash(&pattern(40_000, 2)).to_hex().to_string()
    );
    // An absolute path here, and this device by name with its path.
    let abs = w.v.disk.path().join("notes.txt").display().to_string();
    assert_eq!(
        w.v.apps
            .open_for(1, &abs, &opts, "open", &nothing)
            .await
            .unwrap()
            .how,
        How::Local
    );
    assert_eq!(
        w.v.apps
            .open_for(1, &format!("Voyager:{abs}"), &opts, "open", &nothing)
            .await
            .unwrap()
            .how,
        How::Local
    );

    // Another computer's location: fetched over iroh into the app cache.
    let take = pattern(3_000_000, 1);
    let take_hash = blake3::hash(&take).to_hex().to_string();
    let f =
        w.b.apps
            .open_for(1, "Scratch:Mixes/take.wav", &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(f.how, How::Fetched);
    assert_eq!(f.hash.as_deref(), Some(take_hash.as_str()));
    assert!(f.path.starts_with(w.b.dir.path().join("app-cache/objects")));
    assert_eq!(std::fs::read(&f.path).unwrap(), take);
    let again =
        w.b.apps
            .open_for(1, "Scratch:Mixes/take.wav", &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(
        (again.how, again.path.clone()),
        (How::Cached, f.path.clone())
    );
    // By content: its full hash, and its sampled hash (an index's).
    let c =
        w.b.apps
            .open_for(1, &format!("content:{take_hash}"), &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!((c.how, c.path), (How::Cached, f.path.clone()));
    let sampled = drive_files::hash::sampled(&w.v.disk.path().join("Kits/snare.wav")).unwrap();
    let s =
        w.b.apps
            .open_for(1, &format!("content:{sampled}"), &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(s.how, How::Fetched);
    assert_eq!(std::fs::read(&s.path).unwrap(), pattern(50_000, 3));
    // A browsed file of another computer, by its device and path.
    let browsed = format!("Voyager:{}", w.v.disk.path().join("notes.txt").display());
    let n =
        w.b.apps
            .open_for(1, &browsed, &opts, "open", &nothing)
            .await;
    // Only under the device's roots (its disks and home): a scratch folder
    // elsewhere is refused, which is the confinement working.
    let under_a_root = w.v.a.browse_roots().iter().any(|r| {
        std::fs::canonicalize(w.v.disk.path())
            .unwrap()
            .starts_with(&r.mount)
    });
    match n {
        Ok(n) => {
            assert!(under_a_root);
            assert_eq!(n.how, How::Fetched);
            assert_eq!(std::fs::read(&n.path).unwrap(), b"bridge twice");
        }
        Err(e) => assert!(!under_a_root, "{e}"),
    }

    // A Drive tree file whose blob is on the node: into the tree's cache.
    let lyrics = b"amazing grace, verse 2".to_vec();
    let lh = blake3::hash(&lyrics).to_hex().to_string();
    let put = reqwest::Client::new()
        .put(format!("{}/blobs/{lh}", w.url))
        .body(lyrics.clone())
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let songs = tree::mkdir(&w.v.a.files, None, "Songs").unwrap();
    let fid = tree::create_file(&w.v.a.files, Some(songs), "lyrics.txt").unwrap();
    tree::set_content(&w.v.a.files, fid, lyrics.len() as u64, &lh).unwrap();
    w.v.a.commit_files();
    let bf = w.b.a.clone();
    wait_until("the tree reaches Battleship", move || {
        tree::resolve(&bf.files, "Songs/lyrics.txt")
            .flatten()
            .is_some()
    })
    .await;
    let t =
        w.b.apps
            .open_for(1, "drive:Songs/lyrics.txt", &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(t.how, How::Fetched);
    assert_eq!(t.path, w.b.a.cache_path(&lh).unwrap());
    assert_eq!(std::fs::read(&t.path).unwrap(), lyrics);
    assert_eq!(
        w.b.apps
            .open_for(1, "drive:Songs/lyrics.txt", &opts, "open", &nothing)
            .await
            .unwrap()
            .how,
        How::Cached
    );

    // A tree file only another device keeps (not on the node): from it.
    let only = pattern(700_000, 9);
    let oh = blake3::hash(&only).to_hex().to_string();
    std::fs::write(w.v.a.cache_path(&oh).unwrap(), &only).unwrap();
    space::set_held(&w.v.a.space, "voyager", &oh, Some("local")).unwrap();
    w.v.a.commit_space();
    let bs = w.b.a.clone();
    let oh2 = oh.clone();
    wait_until("the holding reaches Battleship", move || {
        !space::holders(&bs.space, &oh2).is_empty()
    })
    .await;
    let o =
        w.b.apps
            .open_for(1, &format!("content:{oh}"), &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!(o.how, How::Fetched);
    assert_eq!(std::fs::read(&o.path).unwrap(), only);

    // Voyager goes away: what Battleship fetched still opens, from its cache.
    w.v.a.provider.get().unwrap().shutdown().await;
    let off =
        w.b.apps
            .open_for(1, "Scratch:Mixes/take.wav", &opts, "open", &nothing)
            .await
            .unwrap();
    assert_eq!((off.how, off.path), (How::Cached, f.path));
    let e =
        w.b.apps
            .open_for(1, "Scratch:notes.txt", &opts, "open", &nothing)
            .await
            .unwrap_err();
    assert!(e.contains("cannot be reached"), "{e}");
    // Nonsense is refused with a reason.
    assert!(w.b.apps.resolve("nowhere:thing").await.is_err());
    assert!(
        w.b.apps
            .resolve("drive:Songs")
            .await
            .unwrap_err()
            .contains("folder")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_complete_partial_ignores_the_range_error_body() {
    let (url, _node) = node().await;
    let device = device(&url, "range-test", "Range test").await;
    let bytes = b"already downloaded";
    let hash = blake3::hash(bytes).to_hex().to_string();
    let path = device.a.cache_path(&hash).unwrap();
    let partial = path.with_extension("part");
    std::fs::write(&partial, bytes).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new().route(
        "/blobs/{hash}",
        axum::routing::get(move |headers: axum::http::HeaderMap| async move {
            assert_eq!(headers["range"], format!("bytes={}-", bytes.len()));
            (
                axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
                "range outside the file",
            )
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    device.a.cfg.lock().unwrap().node = format!("ws://{address}/sync");

    let result = device
        .apps
        .materialize(
            &Source::Tree {
                hash,
                size: bytes.len() as u64,
            },
            &OpenOptions::new(),
            &nothing,
        )
        .await;
    server.abort();

    let file = result.unwrap();
    assert_eq!(file.path, path);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(!partial.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_fetch_resumes_and_partials_are_never_handed_out() {
    let w = world().await;
    let big = pattern(48 << 20, 7);
    std::fs::write(w.v.disk.path().join("Mixes/long.wav"), &big).unwrap();
    let h = blake3::hash(&big).to_hex().to_string();
    let src = w.b.apps.resolve("Scratch:Mixes/long.wav").await.unwrap();
    assert!(matches!(src, Source::Remote { .. }));

    // A fetch stopped part way (the app went, the agent stopped).
    let seen = Arc::new(AtomicU64::new(0));
    let (apps, s2, src2) = (w.b.apps.clone(), seen.clone(), src.clone());
    let task = tokio::spawn(async move {
        let on = move |n: u64| s2.store(n, Ordering::SeqCst);
        apps.materialize(&src2, &OpenOptions::new(), &on).await
    });
    let until = Instant::now() + Duration::from_secs(60);
    while seen.load(Ordering::SeqCst) < (4 << 20) && !task.is_finished() {
        assert!(Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    task.abort();
    let _ = task.await;
    let part = w.b.apps.cache.partial_path(&h);
    let had = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if had > 0 {
        assert!(
            !w.b.apps.cache.has(&h),
            "a partial file is never a cache entry"
        );
        assert!(had < big.len() as u64);
    }

    // A partial whose bytes are right resumes from where it stopped.
    std::fs::write(&part, &big[..(5 << 20) + 123]).unwrap();
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let f2 = first.clone();
    let on = move |n: u64| {
        let _ = f2.compare_exchange(u64::MAX, n, Ordering::SeqCst, Ordering::SeqCst);
    };
    let f =
        w.b.apps
            .materialize(&src, &OpenOptions::new(), &on)
            .await
            .unwrap();
    assert_eq!(
        first.load(Ordering::SeqCst),
        (5 << 20) + 123,
        "resumed, not restarted"
    );
    assert_eq!(f.how, How::Fetched);
    assert_eq!(std::fs::read(&f.path).unwrap(), big);
    assert!(!part.exists());

    // A partial whose bytes are wrong is caught by the hash and let go;
    // the next ask fetches it whole.
    std::fs::write(w.v.disk.path().join("Mixes/other.wav"), pattern(2 << 20, 8)).unwrap();
    let oh = blake3::hash(&pattern(2 << 20, 8)).to_hex().to_string();
    let other = w.b.apps.resolve("Scratch:Mixes/other.wav").await.unwrap();
    let opart = w.b.apps.cache.partial_path(&oh);
    std::fs::write(&opart, vec![0u8; 1 << 20]).unwrap();
    let e =
        w.b.apps
            .materialize(&other, &OpenOptions::new(), &nothing)
            .await
            .unwrap_err();
    assert!(e.contains("do not match"), "{e}");
    assert!(!w.b.apps.cache.has(&oh) && !opart.exists());
    let f =
        w.b.apps
            .materialize(&other, &OpenOptions::new(), &nothing)
            .await
            .unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), pattern(2 << 20, 8));
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_an_evicted_cache_entry_does_not_return_a_missing_path() {
    let (url, _node) = node().await;
    let device = device(&url, "lease-test", "Lease test").await;
    let bytes = b"cached bytes";
    let hash = blake3::hash(bytes).to_hex().to_string();
    let cache = &device.apps.cache;
    std::fs::write(cache.partial_path(&hash), bytes).unwrap();
    let path = cache.commit(&hash, bytes.len() as u64, &|_| false).unwrap();
    let conn = 1;
    device.a.leases.open(conn, "test app");

    let on_bytes = |_| cache.set_budget(0, &|h| device.a.leases.holds_hash(h));
    let result = device
        .apps
        .open_for(
            conn,
            &format!("content:{hash}"),
            &OpenOptions::new(),
            "open",
            &on_bytes,
        )
        .await;

    assert!(!path.exists(), "the callback must evict the selected file");
    assert!(result.is_err(), "the app must not receive a missing path");
    assert!(!device.a.leases.holds_hash(&hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_files_are_not_evicted_and_leases_end_with_the_app() {
    let w = world().await;
    serve(w.b.apps.clone()).await.unwrap();
    let dir = w.b.dir.path().to_path_buf();
    let take_hash = blake3::hash(&pattern(3_000_000, 1)).to_hex().to_string();
    let snare_hash = blake3::hash(&pattern(50_000, 3)).to_hex().to_string();

    // An app opens the take through the socket and holds it.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let d2 = dir.clone();
    let app = std::thread::spawn(move || {
        let c = drive_local::Client::connect_in(&d2, "studio").unwrap();
        let f = c.open("Scratch:Mixes/take.wav").unwrap();
        assert_eq!(f.how, How::Fetched);
        let pins = c.pins().unwrap();
        assert!(pins.iter().any(|p| p.path == f.path
            && p.holders[0].app == "studio"
            && p.holders[0].why == "open"));
        let st = c.status().unwrap();
        assert!(st.apps.contains(&"studio".to_string()) && st.cache_files >= 1);
        // Hold it until told to go.
        rx.recv().unwrap();
        drop(c);
    });
    let leases = w.b.a.leases.clone();
    let th = take_hash.clone();
    wait_until("the app holds the take", move || leases.holds_hash(&th)).await;
    // Another file comes into the cache, held by no one.
    let s =
        w.b.apps
            .open_for(
                99,
                "Scratch:Kits/snare.wav",
                &OpenOptions::new(),
                "open",
                &nothing,
            )
            .await
            .unwrap();
    assert!(w.b.apps.cache.has(&snare_hash) && s.how == How::Fetched);
    // The budget shrinks to nothing: only what no app holds goes.
    let l = w.b.a.leases.clone();
    w.b.apps.cache.set_budget(1, &move |h| l.holds_hash(h));
    assert!(w.b.apps.cache.has(&take_hash), "held by an app: kept");
    assert!(!w.b.apps.cache.has(&snare_hash), "held by no one: let go");
    // The placement pass skips what apps hold, too.
    assert!(w.b.a.leases.holds_hash(&take_hash));

    // The app goes: its lease ends, and the take can go.
    tx.send(()).unwrap();
    app.join().unwrap();
    let leases = w.b.a.leases.clone();
    let th = take_hash.clone();
    wait_until("the lease ended with the connection", move || {
        !leases.holds_hash(&th)
    })
    .await;
    assert!(w.b.a.leases.pins().is_empty());
    let l = w.b.a.leases.clone();
    w.b.apps.cache.set_budget(1, &move |h| l.holds_hash(h));
    assert!(!w.b.apps.cache.has(&take_hash));

    // Release by hand, without disconnecting.
    let d3 = dir.clone();
    tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&d3, "player").unwrap();
        c.open("Scratch:Kits/kick.wav").unwrap();
        assert_eq!(c.pins().unwrap().len(), 1);
        c.release("Scratch:Kits/kick.wav").unwrap();
        assert!(c.pins().unwrap().is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_this_user_on_loopback_gets_in() {
    // Never a non-loopback address.
    assert!(bind_loopback("0.0.0.0:0".parse().unwrap()).await.is_err());
    assert!(
        bind_loopback("192.168.1.20:0".parse().unwrap())
            .await
            .is_err()
    );
    // Another user on the socket is refused.
    assert!(uid_allowed(1000, 1000) && !uid_allowed(1001, 1000) && !uid_allowed(0, 1000));

    let (url, _node) = node().await;
    let d = device(&url, "battleship", "Battleship").await;
    std::fs::write(d.disk.path().join("a.wav"), b"RIFF").unwrap();
    let abs = d.disk.path().join("a.wav").display().to_string();
    // The socket is this user's only.
    serve(d.apps.clone()).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(d.dir.path().join(drive_local::SOCKET)).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        // A second agent in the same folder does not take the socket over.
        assert!(bind_socket(d.dir.path()).unwrap_err().contains("already"));
    }
    // The loopback port: the token in `app.port` lets an app in…
    let port_dir = tempfile::tempdir().unwrap();
    let (l, token) = bind_port(port_dir.path(), 0).await.unwrap();
    #[cfg(unix)]
    assert_eq!(mode_of(&port_dir.path().join(drive_local::PORT_FILE)), 0o600);
    tokio::spawn(accept_tcp(d.apps.clone(), l, token));
    let (pd, a2) = (port_dir.path().to_path_buf(), abs.clone());
    tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&pd, "tcp-app").unwrap();
        assert_eq!(c.open(&a2).unwrap().how, How::Local);
    })
    .await
    .unwrap();
    // …and a wrong one does not.
    let text = std::fs::read_to_string(port_dir.path().join(drive_local::PORT_FILE)).unwrap();
    let mut pf: drive_local::proto::PortFile = serde_json::from_str(&text).unwrap();
    pf.token = "guess".into();
    let bad = tempfile::tempdir().unwrap();
    std::fs::write(
        bad.path().join(drive_local::PORT_FILE),
        serde_json::to_string(&pf).unwrap(),
    )
    .unwrap();
    let bd = bad.path().to_path_buf();
    let r = tokio::task::spawn_blocking(move || {
        drive_local::Client::connect_in(&bd, "intruder").map(|_| ())
    })
    .await
    .unwrap();
    assert!(matches!(r, Err(drive_local::Error::Refused(_))), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn location_files_are_never_written() {
    let w = world().await;
    serve(w.v.apps.clone()).await.unwrap();
    let root = w.v.disk.path().to_path_buf();
    // Read-only files in read-only folders: anything that tried to write
    // would fail, and nothing may change either way.
    #[cfg(unix)]
    fn lock(root: &Path, on: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![root.to_path_buf()];
        let mut dirs = Vec::new();
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                    dirs.push(p);
                } else {
                    std::fs::set_permissions(
                        &p,
                        std::fs::Permissions::from_mode(if on { 0o444 } else { 0o644 }),
                    )
                    .unwrap();
                }
            }
        }
        dirs.push(root.to_path_buf());
        for d in dirs {
            std::fs::set_permissions(
                &d,
                std::fs::Permissions::from_mode(if on { 0o555 } else { 0o755 }),
            )
            .unwrap();
        }
    }
    #[cfg(unix)]
    lock(&root, true);
    let before = snapshot(&root);

    // This device's apps: open (with the hash), resolve a list, warm the
    // folder, watch a file.
    let vd = w.v.dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&vd, "studio").unwrap();
        let mut o = OpenOptions::new();
        o.hash = true;
        let f = c.open_with("Scratch:Mixes/take.wav", o).unwrap();
        assert_eq!(f.how, How::Local);
        let all = c
            .resolve_many(&[
                "Scratch:Kits/kick.wav",
                "Scratch:Kits/snare.wav",
                "Scratch:notes.txt",
                "Scratch:nope.wav",
            ])
            .unwrap();
        assert_eq!(
            all.iter()
                .filter(|r| r.file.as_ref().is_some_and(|f| f.how == How::Local))
                .count(),
            3
        );
        assert!(all[3].error.is_some());
        let job = c
            .warm(drive_local::Warm {
                targets: vec!["Scratch:Kits".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!((job.started.files, job.started.here), (2, 2));
        let done = job.wait().unwrap();
        assert_eq!(done.files_done, 2);
        let _w = c.watch("Scratch:notes.txt").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
    })
    .await
    .unwrap();
    // Another computer fetches them all.
    for f in [
        "Scratch:Mixes/take.wav",
        "Scratch:Kits/kick.wav",
        "Scratch:notes.txt",
    ] {
        w.b.apps
            .open_for(1, f, &OpenOptions::new(), "open", &nothing)
            .await
            .unwrap();
    }
    assert_eq!(
        snapshot(&root),
        before,
        "names, bytes, times and modes unchanged"
    );
    assert_eq!(
        w.v.apps.cache.stats().0,
        0,
        "nothing of its own copied into its cache"
    );
    #[cfg(unix)]
    lock(&root, false);
}

#[tokio::test(flavor = "multi_thread")]
async fn warming_a_project_streams_progress_and_watches_see_new_versions() {
    let w = world().await;
    serve(w.b.apps.clone()).await.unwrap();
    serve(w.v.apps.clone()).await.unwrap();
    // A song whose audio is Voyager's Scratch.
    let id = projects::add_project(&w.v.a.space, "Amazing Grace", "MusicRecording").unwrap();
    projects::add_part(&w.v.a.space, &id, "Audio", &w.loc.id, "").unwrap();
    w.v.a.commit_space();
    let bs = w.b.a.clone();
    wait_until("the project reaches Battleship", move || {
        projects::projects(&bs.space)
            .iter()
            .any(|p| p.name == "Amazing Grace")
    })
    .await;

    let bd = w.b.dir.path().to_path_buf();
    let events = tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&bd, "studio").unwrap();
        let job = c
            .warm(drive_local::Warm {
                project: Some("Amazing Grace".into()),
                kinds: vec!["audio".into()],
                priority: drive_local::Priority::High,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(job.started.files, 3, "the three .wav files, not the note");
        assert_eq!(job.started.here, 0);
        let mut events = Vec::new();
        while let Some(e) = job.recv() {
            let done = matches!(e, Event::Done(_));
            events.push(e);
            if done {
                break;
            }
        }
        // Warmed again: all here.
        let again = c
            .warm(drive_local::Warm {
                project: Some("Amazing Grace".into()),
                kinds: vec!["audio".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(again.started.here, 3);
        // The warmed files are held by this app.
        assert_eq!(c.pins().unwrap().len(), 3);
        events
    })
    .await
    .unwrap();
    let Some(Event::Done(last)) = events.last() else {
        panic!("no done event: {events:?}")
    };
    assert_eq!((last.files, last.files_done, last.files_failed), (3, 3, 0));
    assert_eq!(last.bytes_done, 3_000_000 + 40_000 + 50_000);
    let ready: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(p) => p
                .file
                .as_ref()
                .filter(|f| f.state == drive_local::FileState::Ready),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 3);
    assert!(
        ready
            .iter()
            .all(|f| f.file.as_ref().is_some_and(|x| x.how == How::Fetched))
    );
    assert!(events.iter().any(|e| matches!(e, Event::Progress(p) if p.file.as_ref().is_some_and(|f| f.state == drive_local::FileState::Fetching))));

    // Watches: Voyager's own file changes; Battleship hears of the tree's.
    let note = w.v.disk.path().join("notes.txt");
    let vd = w.v.dir.path().to_path_buf();
    let n2 = note.clone();
    let local = tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&vd, "studio").unwrap();
        let watch = c.watch("Scratch:notes.txt").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(&n2, b"bridge three times now").unwrap();
        watch.recv_timeout(Duration::from_secs(10))
    })
    .await
    .unwrap();
    assert!(
        matches!(local, Some(Event::Changed { size: 22, .. })),
        "{local:?}"
    );

    let songs = tree::mkdir(&w.v.a.files, None, "Songs").unwrap();
    let fid = tree::create_file(&w.v.a.files, Some(songs), "chart.pdf").unwrap();
    tree::set_content(&w.v.a.files, fid, 4, &"aa".repeat(32)).unwrap();
    w.v.a.commit_files();
    let bf = w.b.a.clone();
    wait_until("the tree reaches Battleship", move || {
        tree::resolve(&bf.files, "Songs/chart.pdf")
            .flatten()
            .is_some()
    })
    .await;
    let bd = w.b.dir.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let remote = tokio::task::spawn_blocking(move || {
        let c = drive_local::Client::connect_in(&bd, "studio").unwrap();
        let watch = c.watch("drive:Songs/chart.pdf").unwrap();
        tx.send(()).unwrap();
        watch.recv_timeout(Duration::from_secs(15))
    });
    tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    tree::set_content(&w.v.a.files, fid, 5, &"bb".repeat(32)).unwrap();
    w.v.a.commit_files();
    let got = remote.await.unwrap();
    assert!(
        matches!(&got, Some(Event::Changed { size: 5, hash: Some(h), .. }) if *h == "bb".repeat(32)),
        "{got:?}"
    );
}

/// `ORG/NAME`: another org's project whose part is lent from this one's
/// location (a church's song with its stems on Voyager, drive#52).
#[tokio::test(flavor = "multi_thread")]
async fn warming_another_orgs_project_takes_its_parts_on_this_orgs_computers() {
    let w = world().await;
    let mine =
        w.v.a
            .space_id
            .strip_suffix("/drive:space")
            .unwrap()
            .to_string();
    let church = w.v.a.drive.open("church/drive:space").await;
    w.v.a.drive.synced("church/drive:space").await;
    let p = projects::add_project(&church, "Amazing Grace", "MusicRecording").unwrap();
    let part = projects::add_part_in(&church, &p, "Kits", &mine, &w.loc.id, "Kits").unwrap();
    // A part on some other org's computer: not this device's to warm.
    projects::add_part_in(&church, &p, "Video", "editors", "their-disk", "").unwrap();
    w.v.a.drive.commit("church/drive:space");
    projects::lend(
        &w.v.a.space,
        &projects::Lend {
            org: "church".into(),
            project: p.clone(),
            part,
            location: w.loc.id.clone(),
            path: "Kits".into(),
        },
    )
    .unwrap();
    w.v.a.commit_space();

    let ask = |project: &str| drive_local::Warm {
        project: Some(project.into()),
        ..Default::default()
    };
    // Battleship's copies of both registries catch up.
    let mut got = Vec::new();
    for _ in 0..100 {
        got = match super::warm::expand(&w.b.apps, &ask("church/Amazing Grace")).await {
            Ok(v) => v.into_iter().map(|(label, _)| label).collect(),
            Err(_) => Vec::new(),
        };
        got.sort();
        if got.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(got, ["Scratch:Kits/kick.wav", "Scratch:Kits/snare.wav"]);
    assert!(
        super::warm::expand(&w.b.apps, &ask("church/No Such Song"))
            .await
            .is_err()
    );
}

/// A project's part in the Drive tree (an app's uploads) warms that folder
/// of the tree (drive#54).
#[tokio::test(flavor = "multi_thread")]
async fn warming_a_project_takes_its_drive_tree_parts() {
    let w = world().await;
    let songs = tree::mkdir(&w.v.a.files, None, "Uploads").unwrap();
    let ag = tree::mkdir(&w.v.a.files, Some(songs), "Amazing Grace").unwrap();
    let chart = tree::create_file(&w.v.a.files, Some(ag), "chart.pdf").unwrap();
    tree::set_content(&w.v.a.files, chart, 3, &"a".repeat(64)).unwrap();
    let other = tree::create_file(&w.v.a.files, Some(songs), "elsewhere.pdf").unwrap();
    tree::set_content(&w.v.a.files, other, 3, &"b".repeat(64)).unwrap();
    w.v.a.commit_files();
    let p = projects::add_project(&w.v.a.space, "Amazing Grace", "MusicRecording").unwrap();
    projects::add_part(
        &w.v.a.space,
        &p,
        "Charts",
        projects::TREE,
        "Uploads/Amazing Grace",
    )
    .unwrap();
    projects::add_part(&w.v.a.space, &p, "Kits", &w.loc.id, "Kits").unwrap();
    w.v.a.commit_space();
    let got: Vec<String> = super::warm::expand(
        &w.v.apps,
        &drive_local::Warm {
            project: Some("Amazing Grace".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_iter()
    .map(|(label, _)| label)
    .collect();
    let mut got = got;
    got.sort();
    assert_eq!(
        got,
        [
            "Scratch:Kits/kick.wav",
            "Scratch:Kits/snare.wav",
            "drive:Uploads/Amazing Grace/chart.pdf"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unpinning_keeps_bytes_other_files_and_apps_still_need() {
    let (url, _node) = node().await;
    let d = device(&url, "dev-1", "Voyager").await;
    d.a.cfg.lock().unwrap().allow_evict = true;
    let src = d.disk.path().join("take.wav");
    std::fs::write(&src, pattern(4096, 7)).unwrap();
    let folder = tree::mkdir(&d.a.files, None, "Live").unwrap();
    let a = d.a.put(&src, None, "a.wav").await.unwrap();
    let b = d.a.put(&src, Some(folder), "b.wav").await.unwrap();
    assert_eq!(a.hash, b.hash);
    let h = a.hash.clone();
    // A storage holds them, so unpinning may let the bytes go.
    space::set_held(&d.a.space, "storage-1", &h, Some("stored")).unwrap();
    d.a.pin(&a, true).await.unwrap();
    d.a.pin(&b, true).await.unwrap();

    // `b` is still pinned: its bytes stay.
    d.a.pin(&a, false).await.unwrap();
    assert!(d.a.is_cached(&h), "another pinned file has the same bytes");

    // An app holds them: they stay.
    d.a.leases.open(7, "studio");
    d.a.leases.hold(
        7,
        leases::Item {
            target: "drive:b.wav".into(),
            path: d.a.cache_path(&h).unwrap(),
            hash: Some(h.clone()),
            size: 4096,
            how: How::Cached,
            why: "open".into(),
            since: 0,
        },
    );
    d.a.pin(&b, false).await.unwrap();
    assert!(d.a.is_cached(&h), "an app holds the bytes");
    d.a.leases.end(7);

    // In an active folder: they stay.
    space::set_active(&d.a.space, "dev-1", "Live", true).unwrap();
    d.a.pin(&b, false).await.unwrap();
    assert!(d.a.is_cached(&h), "the file is in an active folder");

    // Wanted by nothing: they go.
    space::set_active(&d.a.space, "dev-1", "Live", false).unwrap();
    d.a.pin(&b, false).await.unwrap();
    assert!(!d.a.is_cached(&h));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetching_a_blob_leaves_an_apps_partial_alone() {
    let (url, _node) = node().await;
    let d = device(&url, "dev-1", "Voyager").await;
    let src = d.disk.path().join("take.wav");
    std::fs::write(&src, pattern(4096, 9)).unwrap();
    let e = d.a.put(&src, None, "take.wav").await.unwrap();
    std::fs::remove_file(d.a.cache_path(&e.hash).unwrap()).unwrap();
    // An app's resumable fetch of the same blob is half done.
    let part = d.a.cache_path(&e.hash).unwrap().with_extension("part");
    std::fs::write(&part, b"half").unwrap();
    let got = d.a.fetch(&e.hash).await.unwrap();
    assert_eq!(std::fs::read(got).unwrap(), pattern(4096, 9));
    assert_eq!(std::fs::read(&part).unwrap(), b"half", "the partial is the app's");
}

#[test]
fn concurrent_atomic_writes_each_land_whole() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("blob");
    let writers: Vec<_> = (0..8u8)
        .map(|i| {
            let dest = dest.clone();
            std::thread::spawn(move || crate::agent::write_atomic(&dest, &pattern(1 << 20, i)))
        })
        .collect();
    for w in writers {
        w.join().unwrap().unwrap();
    }
    let got = std::fs::read(&dest).unwrap();
    assert!((0..8u8).any(|i| got == pattern(1 << 20, i)), "one writer's bytes, whole");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp files left");
}
