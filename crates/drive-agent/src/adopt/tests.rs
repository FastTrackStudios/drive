//! Adopting a folder in place, end to end in one process: a node and two
//! devices. Every test checks that no file of the adopted folder was
//! copied, moved or touched (same inode, same modified time, same bytes).

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use drive_files::{space, tree};

use super::*;
use crate::agent::{Agent, Config};

pub(crate) async fn node() -> (String, tempfile::TempDir) {
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

pub(crate) fn config(url: &str, id: &str, name: &str) -> Config {
    Config {
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
    }
}

pub(crate) async fn device(url: &str, dir: &Path, id: &str, name: &str) -> Arc<Agent> {
    let cfg = config(url, id, name);
    cfg.save(dir).unwrap();
    let a = Agent::start_with(dir, cfg, name, dir.join("state")).await;
    a.drive.online().await;
    space::add_device(&a.space, &crate::agent::this_device(id, name)).unwrap();
    a.commit_space();
    Arc::new(a)
}

pub(crate) async fn wait_until(what: &str, f: impl Fn() -> bool) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !f() {
        assert!(
            std::time::Instant::now() < until,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A song folder as a DAW leaves it, on a disk of its own.
pub(crate) fn songs(disk: &Path) -> PathBuf {
    let songs = disk.join("ProjectDrive/Songs");
    std::fs::create_dir_all(songs.join("Song A/Audio")).unwrap();
    std::fs::create_dir_all(songs.join("Song B")).unwrap();
    std::fs::write(songs.join("Song A/Song A.rpp"), b"<REAPER_PROJECT>").unwrap();
    std::fs::write(songs.join("Song A/Audio/kick.wav"), vec![7u8; 120_000]).unwrap();
    std::fs::write(songs.join("Song B/notes.txt"), b"bridge twice").unwrap();
    songs
}

/// Every file and folder under a folder: (inode, modified ns, bytes).
pub(crate) fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, i128, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let m = std::fs::symlink_metadata(&p).unwrap();
        let mtime = i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec());
        let bytes = if m.is_dir() {
            for e in std::fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
            Vec::new()
        } else {
            std::fs::read(&p).unwrap()
        };
        out.insert(
            p.strip_prefix(root).unwrap().to_path_buf(),
            (m.ino(), mtime, bytes),
        );
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_in_place_shows_at_the_logical_path_and_unadopt_keeps_every_file() {
    let (url, _node_dir) = node().await;
    let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = device(&url, dir.path(), "studio", "Studio PC").await;
    let songs = songs(disk.path());
    let before = snapshot(&songs);

    let r = a.adopt(&songs, "/Studio/Songs/").await.unwrap();
    let loc_id = r.location.id.clone();
    assert_eq!(r.adoption.at, "Studio/Songs");
    assert!(r.new_location);
    assert_eq!(r.adoption.sub, "");
    let st = r.indexed.expect("indexed now").unwrap();
    assert_eq!(st.files, 3);

    // The logical view: Studio (made in the tree, empty there) holds Songs,
    // which is the folder's own.
    assert!(matches!(tree::resolve(&a.files, "Studio"), Some(Some(e)) if e.dir));
    assert!(tree::resolve(&a.files, "Studio/Songs").is_none());
    assert!(
        a.logical_list("")
            .await
            .unwrap()
            .contains(&("Studio".into(), true))
    );
    assert_eq!(
        a.logical_list("Studio").await.unwrap(),
        vec![("Songs".to_string(), true)]
    );
    assert_eq!(
        a.logical_list("Studio/Songs").await.unwrap(),
        vec![("Song A".to_string(), true), ("Song B".to_string(), true)]
    );
    assert_eq!(
        a.logical_list("Studio/Songs/Song A").await.unwrap(),
        vec![
            ("Audio".to_string(), true),
            ("Song A.rpp".to_string(), false)
        ]
    );
    let real = songs.canonicalize().unwrap().join("Song A/Audio/kick.wav");
    match a.spot("Studio/Songs/Song A/Audio/kick.wav") {
        Some(Spot::Here {
            path,
            adoption,
            rel,
            ..
        }) => {
            assert_eq!(rel, "Song A/Audio/kick.wav");
            assert_eq!(path, real);
            assert_eq!(adoption.at, "Studio/Songs");
        }
        other => panic!("not here: {other:?}"),
    }
    assert_eq!(a.spot("Studio"), Some(Spot::Above));
    assert_eq!(a.spot("Elsewhere"), None);

    // Refused: the same place, inside or above it, Drive's own folders, a
    // place the tree has, a file.
    assert!(a.adopt(&songs, "Studio/Songs").await.is_err());
    assert!(
        a.adopt(&songs.join("Song B"), "Studio/Songs/B")
            .await
            .is_err()
    );
    assert!(a.adopt(&songs.join("Song B"), "Studio").await.is_err());
    assert!(a.adopt(&songs.join("Song B"), "Trash/B").await.is_err());
    assert!(a.adopt(&songs.join("Song B"), "Devices/B").await.is_err());
    tree::mkdir(&a.files, None, "Taken").unwrap();
    assert!(a.adopt(&songs.join("Song B"), "Taken").await.is_err());
    assert!(
        a.adopt(&songs.join("Song B/notes.txt"), "Notes")
            .await
            .is_err()
    );

    // A folder inside the location shows in a second place too (a view,
    // not a copy), from the same location.
    let r2 = a
        .adopt(&songs.join("Song A"), "Live/Tonight")
        .await
        .unwrap();
    assert!(!r2.new_location);
    assert_eq!(r2.location.id, r.location.id);
    assert_eq!(r2.adoption.sub, "Song A");
    assert_eq!(
        a.logical_list("Live/Tonight").await.unwrap(),
        vec![
            ("Audio".to_string(), true),
            ("Song A.rpp".to_string(), false)
        ]
    );
    assert_eq!(a.adoptions().len(), 2);
    assert_eq!(snapshot(&songs), before, "adopting touched a file");

    // Another device sees the mapping (from the registry) and the files
    // (from the index), as being elsewhere.
    let other_dir = tempfile::tempdir().unwrap();
    let b = device(&url, other_dir.path(), "laptop", "Laptop").await;
    wait_until("the laptop sees both adoptions", || {
        b.adoptions().len() == 2
    })
    .await;
    match b.spot("Studio/Songs/Song B/notes.txt") {
        Some(Spot::Elsewhere { location, path, .. }) => {
            assert_eq!(location.id, r.location.id);
            assert_eq!(path, "Song B/notes.txt");
        }
        other => panic!("not elsewhere: {other:?}"),
    }
    b.refresh_indexes().await;
    assert_eq!(
        b.logical_list("Studio/Songs/Song B").await.unwrap(),
        vec![("notes.txt".to_string(), false)]
    );

    // The mapping survives a restart of the agent.
    let id = a.device_id();
    drop(a);
    let a = Arc::new(
        Agent::start_with(
            dir.path(),
            config(&url, &id, "Studio PC"),
            "Studio PC",
            dir.path().join("state"),
        )
        .await,
    );
    a.drive.online().await;
    wait_until("the adoptions are back", || a.adoptions().len() == 2).await;
    assert_eq!(
        a.logical_list("Studio").await.unwrap(),
        vec![("Songs".to_string(), true)]
    );

    // Unadopting removes the mapping only.
    let gone = a.unadopt("Studio/Songs").unwrap();
    assert_eq!(gone.location, loc_id);
    assert!(a.unadopt("Studio/Songs").is_err());
    assert_eq!(a.spot("Studio/Songs/Song A"), None);
    assert!(a.logical_list("Studio").await.unwrap().is_empty());
    assert_eq!(a.adoptions().len(), 1);
    a.unadopt("Live/Tonight").unwrap();
    assert!(a.adoptions().is_empty());
    assert!(
        space::locations(&a.space)
            .iter()
            .any(|l| l.id == gone.location)
    );
    assert_eq!(snapshot(&songs), before, "unadopting touched a file");
    wait_until("the laptop sees them gone", || b.adoptions().is_empty()).await;
}

#[tokio::test]
async fn the_agents_own_folder_is_refused_however_it_is_named() {
    let (url, _node_dir) = node().await;
    let t = tempfile::tempdir().unwrap();
    let real = t.path().join("real");
    std::fs::create_dir_all(real.join("songs")).unwrap();
    let alias = t.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    // The agent knows its folder by the symlink's name.
    let a = device(&url, &alias, "studio", "Studio PC").await;
    let err = a.adopt(&real.join("songs"), "/Songs").await.err();
    assert_eq!(err.as_deref(), Some("that folder is the agent's own"));
}
