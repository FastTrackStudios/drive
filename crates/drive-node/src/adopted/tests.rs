use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use drive_files::adopt::{self, Adoption};
use drive_files::index::{IndexEntry, LocationIndex};
use drive_files::projects::{self, Available};
use drive_files::space::{self, Device, Location};
use drive_files::tree;
use serde_json::Value;

use super::*;
use crate::rooms::Rooms;

struct World {
    c: Catalog,
    _dir: tempfile::TempDir,
    /// The bytes of the adopted file kept on the node.
    kept: Vec<u8>,
    /// The bytes of the tree's own file.
    own: Vec<u8>,
}

fn entry(path: &str, dir: bool, size: u64) -> IndexEntry {
    IndexEntry {
        path: path.into(),
        dir,
        size,
        mtime: 7,
        sampled: if dir {
            String::new()
        } else {
            format!("s-{path}")
        },
    }
}

/// A space whose tree has `Studio/Setlists/tour.setlist`, with
/// `Studio/Songs` adopted from a location's `Library` folder and
/// `Archive/Old/Sessions` from its `Old` folder (nothing of `Archive`
/// is in the tree).
async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("drive.db"))
        .await
        .unwrap();
    let rooms = Arc::new(Rooms::new(store, crate::auth::Auth::new(None)));
    let blobs = drive_files::storage::StorageConfig::Fs {
        root: dir.path().join("blobs").to_string_lossy().into_owned(),
    }
    .blobs()
    .unwrap();
    let kept = b"a take, kept on the node".to_vec();
    let own = b"Song A\nSong B\n".to_vec();
    let kept_hash = blobs.put(kept.clone()).await.unwrap();
    let own_hash = blobs.put(own.clone()).await.unwrap();
    let c = Catalog {
        rooms: rooms.clone(),
        blobs,
        names: Default::default(),
        reports: Default::default(),
    };
    rooms
        .edit(&drive_files::space_doc("home"), "t", |d| {
            space::add_device(
                d,
                &Device {
                    id: "pc".into(),
                    name: "Studio PC".into(),
                    kind: "desktop".into(),
                    os: "linux".into(),
                    roles: vec![],
                    approved_by: String::new(),
                    added: 1,
                },
            )
            .unwrap();
            space::add_location(
                d,
                &Location {
                    id: "songs".into(),
                    name: "Recital songs".into(),
                    device: "pc".into(),
                    path: "/srv/songs".into(),
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
            for (at, sub) in [("Studio/Songs", "Library"), ("Archive/Old/Sessions", "Old")] {
                adopt::adopt(
                    d,
                    &Adoption {
                        at: at.into(),
                        location: "songs".into(),
                        sub: sub.into(),
                        by: "pc".into(),
                        added: 1,
                    },
                )
                .unwrap();
            }
            projects::set_available(
                d,
                "songs",
                "Library/Song A/take.wav",
                Some(&Available {
                    hash: kept_hash.clone(),
                    size: kept.len() as i64,
                    mtime: 7,
                }),
            )
            .unwrap();
        })
        .await;
    rooms
        .edit(&drive_files::files_doc("home"), "t", |d| {
            let studio = tree::mkdir(d, None, "Studio").unwrap();
            let sets = tree::mkdir(d, Some(studio), "Setlists").unwrap();
            let f = tree::create_file(d, Some(sets), "tour.setlist").unwrap();
            tree::set_content(d, f, own.len() as u64, &own_hash).unwrap();
        })
        .await;
    rooms
        .store
        .put_index(
            "home",
            &LocationIndex {
                location: "songs".into(),
                root: String::new(),
                built: 1,
                entries: vec![
                    entry("Library", true, 0),
                    entry("Library/Song A", true, 0),
                    entry("Library/Song A/take.wav", false, kept.len() as u64),
                    entry("Library/Song A/lyrics.txt", false, 12),
                    entry("Library/Song B", true, 0),
                    entry("Old", true, 0),
                    entry("Old/2019.rpp", false, 3),
                    entry("Elsewhere.wav", false, 3),
                ],
            },
        )
        .await
        .unwrap();
    World {
        c,
        _dir: dir,
        kept,
        own,
    }
}

fn params(path: &str) -> Q {
    Query(HashMap::from([
        ("space".to_string(), "home".to_string()),
        ("path".to_string(), path.to_string()),
    ]))
}

async fn ls(w: &World, path: &str) -> Result<Value, StatusCode> {
    list(State(w.c.clone()), params(path)).await.map(|j| j.0)
}

fn names_of(v: &Value) -> Vec<(String, String)> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["source"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn read(w: &World, path: &str) -> (StatusCode, Vec<u8>) {
    let r = file(State(w.c.clone()), params(path), Default::default()).await;
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (status, body.to_vec())
}

#[tokio::test]
async fn folders_on_the_way_to_adoptions_are_listed() {
    let w = world().await;
    let top = ls(&w, "").await.unwrap();
    assert_eq!(top["place"], "above");
    assert_eq!(
        names_of(&top),
        vec![
            ("Archive".into(), "above".into()),
            ("Studio".into(), "tree".into())
        ]
    );
    // A folder only adoptions make, all the way down.
    assert_eq!(
        names_of(&ls(&w, "Archive").await.unwrap()),
        vec![("Old".into(), "above".into())]
    );
    let old = ls(&w, "Archive/Old").await.unwrap();
    assert_eq!(names_of(&old), vec![("Sessions".into(), "adopted".into())]);
    // The tree's own folder beside the adopted one, which says where it is.
    let studio = ls(&w, "/Studio/").await.unwrap();
    assert_eq!(
        names_of(&studio),
        vec![
            ("Setlists".into(), "tree".into()),
            ("Songs".into(), "adopted".into())
        ]
    );
    let songs = &studio["entries"][1];
    assert_eq!(songs["logical"], "Studio/Songs");
    assert_eq!(songs["where"]["device_name"], "Studio PC");
    assert_eq!(songs["where"]["location_name"], "Recital songs");
    assert_eq!(songs["where"]["real"], "/srv/songs/Library");
    // Nothing adopted: the tree as it is.
    assert_eq!(
        names_of(&ls(&w, "Studio/Setlists").await.unwrap()),
        vec![("tour.setlist".into(), "tree".into())]
    );
    assert_eq!(ls(&w, "Nowhere").await.unwrap_err(), StatusCode::NOT_FOUND);
    assert_eq!(
        ls(&w, "Studio/../x").await.unwrap_err(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_adopted_folder_lists_from_its_locations_index() {
    let w = world().await;
    let songs = ls(&w, "Studio/Songs").await.unwrap();
    assert_eq!(songs["place"], "inside");
    assert_eq!(songs["where"]["path"], "Library");
    assert_eq!(
        names_of(&songs),
        vec![
            ("Song A".into(), "adopted".into()),
            ("Song B".into(), "adopted".into())
        ]
    );
    assert_eq!(songs["entries"][0]["logical"], "Studio/Songs/Song A");
    assert_eq!(songs["entries"][0]["path"], "Library/Song A");
    assert_eq!(songs["entries"][0]["device_name"], "Studio PC");

    let a = ls(&w, "Studio/Songs/Song A").await.unwrap();
    assert_eq!(
        names_of(&a),
        vec![
            ("lyrics.txt".into(), "adopted".into()),
            ("take.wav".into(), "adopted".into())
        ]
    );
    let take = &a["entries"][1];
    assert_eq!(take["location"], "songs");
    assert_eq!(take["path"], "Library/Song A/take.wav");
    assert!(take["available"].is_string());
    // Another adoption of the same location shows its own folder only.
    assert_eq!(
        names_of(&ls(&w, "Archive/Old/Sessions").await.unwrap()),
        vec![("2019.rpp".into(), "adopted".into())]
    );
    assert_eq!(
        ls(&w, "Studio/Songs/Song C").await.unwrap_err(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ls(&w, "Studio/Songs/Song A/take.wav").await.unwrap_err(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn files_are_read_by_their_logical_path() {
    let w = world().await;
    // The tree's own file, from its blob.
    assert_eq!(
        read(&w, "Studio/Setlists/tour.setlist").await,
        (StatusCode::OK, w.own.clone())
    );
    // An adopted file kept on the node, without its computer.
    assert_eq!(
        read(&w, "Studio/Songs/Song A/take.wav").await,
        (StatusCode::OK, w.kept.clone())
    );
    // One that only its computer has goes through the location's fetch:
    // that computer is off here.
    assert_eq!(
        read(&w, "Studio/Songs/Song A/lyrics.txt").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        read(&w, "Studio/Songs/Song A").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(read(&w, "Studio/Nothing").await.0, StatusCode::NOT_FOUND);
}
