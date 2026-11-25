//! Working copies end to end in one process: a node, the device whose disk
//! has the folder, and a second device that sees the tree.

use std::path::Path;
use std::sync::Arc;

use drive_files::{adopt, tree, working};

use crate::adopt::tests::{device, node, snapshot, songs, wait_until};
use crate::agent::Agent;

/// The file at `rel` in `a`'s tree: its hash ("" if none).
fn tree_hash(a: &Agent, rel: &str) -> String {
    match tree::resolve(&a.files, rel) {
        Some(Some(e)) if !e.dir => e.hash,
        _ => String::new(),
    }
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// Two devices; `studio` has a song folder adopted at Studio/Songs and
/// owns it.
async fn owned() -> (
    Arc<Agent>,
    Arc<Agent>,
    std::path::PathBuf,
    Vec<tempfile::TempDir>,
) {
    let (url, node_dir) = node().await;
    let (da, db, disk) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = device(&url, da.path(), "studio", "Studio PC").await;
    let b = device(&url, db.path(), "laptop", "Laptop").await;
    let folder = songs(disk.path());
    a.adopt(&folder, "Studio/Songs").await.unwrap();
    (a, b, folder, vec![node_dir, da, db, disk])
}

#[tokio::test(flavor = "multi_thread")]
async fn owning_an_adoption_puts_its_files_in_the_tree_and_leaves_the_disk_alone() {
    let (a, b, folder, _keep) = owned().await;
    let before = snapshot(&folder);

    let (w, done) = a.own("Studio/Songs").await.unwrap();
    assert_eq!(done.up, 3, "{done:?}");
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!(snapshot(&folder), before, "owning touches no file on disk");

    // The adoption is gone; the tree has the files, with their bytes.
    assert!(adopt::adoptions(&a.space).is_empty());
    assert_eq!(working::working_copies(&a.space), vec![w.clone()]);
    let kick = tree_hash(&a, "Studio/Songs/Song A/Audio/kick.wav");
    assert_eq!(
        kick,
        drive_files::hash::of_bytes(&read(&folder.join("Song A/Audio/kick.wav")))
    );

    // Another device sees ordinary files and can read them.
    wait_until("the laptop sees the tree", || {
        !tree_hash(&b, "Studio/Songs/Song B/notes.txt").is_empty()
    })
    .await;
    let h = tree_hash(&b, "Studio/Songs/Song B/notes.txt");
    assert_eq!(b.fetch_bytes(&h).await.unwrap(), b"bridge twice");

    // The mount here still shows the folder in place.
    assert!(matches!(
        a.spot("Studio/Songs/Song A"),
        Some(crate::adopt::Spot::Here { .. })
    ));
    assert!(b.spot("Studio/Songs/Song A").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_on_either_side_reach_the_other() {
    let (a, b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();

    // Here: an edit and a new file go to the tree.
    std::fs::write(folder.join("Song B/notes.txt"), b"bridge three times").unwrap();
    std::fs::write(folder.join("Song B/lyrics.txt"), b"la la").unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.up, 2, "{done:?}");
    wait_until("the laptop sees the edit", || {
        tree_hash(&b, "Studio/Songs/Song B/notes.txt")
            == drive_files::hash::of_bytes(b"bridge three times")
    })
    .await;

    // Elsewhere: an edit made on the laptop comes down to the disk.
    let notes = tree::resolve(&b.files, "Studio/Songs/Song B")
        .unwrap()
        .unwrap();
    let src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(src.path(), b"from the laptop").unwrap();
    b.put(src.path(), Some(notes.id), "notes.txt")
        .await
        .unwrap();
    wait_until("the studio sees the laptop's edit", || {
        tree_hash(&a, "Studio/Songs/Song B/notes.txt")
            == drive_files::hash::of_bytes(b"from the laptop")
    })
    .await;
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.down, 1, "{done:?}");
    assert_eq!(read(&folder.join("Song B/notes.txt")), b"from the laptop");

    // Nothing more to do once level.
    assert_eq!(
        a.reconcile(&w).await.unwrap(),
        crate::working::Reconciled::default()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deletes_go_to_the_other_sides_trash_and_renames_follow() {
    let (a, b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();

    // Deleted here: to the Drive's Trash.
    std::fs::remove_file(folder.join("Song B/notes.txt")).unwrap();
    a.reconcile(&w).await.unwrap();
    assert!(tree::resolve(&a.files, "Studio/Songs/Song B/notes.txt").is_none());

    // Renamed here: the same tree file, renamed (not trashed and made anew).
    let id_before = tree::resolve(&a.files, "Studio/Songs/Song A/Song A.rpp")
        .unwrap()
        .unwrap()
        .id;
    std::fs::rename(
        folder.join("Song A/Song A.rpp"),
        folder.join("Song A/Song A v2.rpp"),
    )
    .unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.trashed, 0, "{done:?}");
    let renamed = tree::resolve(&a.files, "Studio/Songs/Song A/Song A v2.rpp")
        .unwrap()
        .unwrap();
    assert_eq!(renamed.id, id_before);

    // Renamed in the Drive (on the laptop): renamed on disk.
    wait_until("the laptop sees the rename", || {
        tree::resolve(&b.files, "Studio/Songs/Song A/Song A v2.rpp").is_some()
    })
    .await;
    let e = tree::resolve(&b.files, "Studio/Songs/Song A/Audio/kick.wav")
        .unwrap()
        .unwrap();
    let audio = tree::parent_of(&b.files, e.id);
    tree::rename(&b.files, e.id, audio, "kick-in.wav").unwrap();
    b.commit_files();
    wait_until("the studio sees it", || {
        tree::resolve(&a.files, "Studio/Songs/Song A/Audio/kick-in.wav").is_some()
    })
    .await;
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.moved, 1, "{done:?}");
    assert!(folder.join("Song A/Audio/kick-in.wav").exists());
    assert!(!folder.join("Song A/Audio/kick.wav").exists());

    // Removed in the Drive: into the folder's .drive-trash, not deleted.
    let e = tree::resolve(&b.files, "Studio/Songs/Song A/Song A v2.rpp")
        .unwrap()
        .unwrap();
    tree::trash(&b.files, e.id).unwrap();
    b.commit_files();
    wait_until("the studio sees it", || {
        tree::resolve(&a.files, "Studio/Songs/Song A/Song A v2.rpp").is_none()
    })
    .await;
    a.reconcile(&w).await.unwrap();
    assert!(!folder.join("Song A/Song A v2.rpp").exists());
    assert!(folder.join(".drive-trash/Song A/Song A v2.rpp").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_on_both_sides_keeps_both() {
    let (a, b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();
    wait_until("the laptop sees the tree", || {
        !tree_hash(&b, "Studio/Songs/Song B/notes.txt").is_empty()
    })
    .await;

    let song_b = tree::resolve(&b.files, "Studio/Songs/Song B")
        .unwrap()
        .unwrap();
    let src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(src.path(), b"laptop version").unwrap();
    b.put(src.path(), Some(song_b.id), "notes.txt")
        .await
        .unwrap();
    wait_until("the studio sees it", || {
        tree_hash(&a, "Studio/Songs/Song B/notes.txt")
            == drive_files::hash::of_bytes(b"laptop version")
    })
    .await;
    std::fs::write(folder.join("Song B/notes.txt"), b"studio version, longer").unwrap();

    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.conflicts, 1, "{done:?}");
    assert_eq!(read(&folder.join("Song B/notes.txt")), b"laptop version");
    let aside: Vec<_> = std::fs::read_dir(folder.join("Song B"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("(conflict Studio PC"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(
        read(&folder.join("Song B").join(&aside[0])),
        b"studio version, longer"
    );
    assert!(!tree_hash(&a, &format!("Studio/Songs/Song B/{}", aside[0])).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn owning_is_refused_off_the_device_that_has_the_folder() {
    let (_a, b, _folder, _keep) = owned().await;
    wait_until("the laptop sees the adoption", || {
        !adopt::adoptions(&b.space).is_empty()
    })
    .await;
    let e = b.own("Studio/Songs").await.unwrap_err();
    assert!(e.contains("run `drive own` there"), "{e}");
    assert!(e.contains("Studio PC"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pass_that_would_trash_many_files_is_refused() {
    let (a, _b, folder, _keep) = owned().await;
    for i in 0..30 {
        std::fs::write(
            folder.join(format!("Song B/take-{i:02}.wav")),
            format!("take {i}"),
        )
        .unwrap();
    }
    let (w, done) = a.own("Studio/Songs").await.unwrap();
    assert_eq!(done.up, 33, "{done:?}");
    // The folder's files all gone at once (a disk unmounted, a bad copy):
    // nothing goes to the Trash, and it says so.
    for i in 0..30 {
        std::fs::remove_file(folder.join(format!("Song B/take-{i:02}.wav"))).unwrap();
    }
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.trashed, 0, "{done:?}");
    assert!(
        done.errors
            .iter()
            .any(|e| e.contains("would go to a trash")),
        "{done:?}"
    );
    assert!(tree::resolve(&a.files, "Studio/Songs/Song B/take-07.wav").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_of_the_tree_behind_the_last_sync_does_nothing() {
    let (a, _b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();
    let before = snapshot(&folder);
    // The last sync names a file this copy of the tree does not have (it
    // was made by another process whose change has not arrived yet).
    let state = a
        .dir
        .join("working")
        .join(format!("{}.json", w.root.replace(['@', '/'], "_")));
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    v["Song B/from-elsewhere.txt"] =
        serde_json::json!({ "id": "999@424242", "hash": "x", "size": 1, "mtime": 1 });
    std::fs::write(&state, v.to_string()).unwrap();
    let e = a.reconcile(&w).await.unwrap_err();
    assert!(e.contains("behind the last sync"), "{e}");
    assert_eq!(snapshot(&folder), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_replaced_by_the_drives_version_is_kept_in_the_folders_trash() {
    let (a, b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();
    wait_until("the laptop sees the tree", || {
        !tree_hash(&b, "Studio/Songs/Song B/notes.txt").is_empty()
    })
    .await;
    let song_b = tree::resolve(&b.files, "Studio/Songs/Song B")
        .unwrap()
        .unwrap();
    let src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(src.path(), b"newer, from the laptop").unwrap();
    b.put(src.path(), Some(song_b.id), "notes.txt")
        .await
        .unwrap();
    wait_until("the studio sees it", || {
        tree_hash(&a, "Studio/Songs/Song B/notes.txt")
            == drive_files::hash::of_bytes(b"newer, from the laptop")
    })
    .await;
    assert_eq!(a.reconcile(&w).await.unwrap().down, 1);
    assert_eq!(
        read(&folder.join("Song B/notes.txt")),
        b"newer, from the laptop"
    );
    assert_eq!(
        read(&folder.join(".drive-trash/Song B/notes.txt")),
        b"bridge twice"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_a_markdown_file_on_disk_both_ways() {
    let (a, _b, folder, _keep) = owned().await;
    let (w, _) = a.own("Studio/Songs").await.unwrap();

    // A page made in the Drive comes down as a plain .md file.
    let p = crate::pages::create(&a, "Studio/Songs/Song B/Ideas", "# Ideas\n\nA key change.\n")
        .await
        .unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.down, 1, "{done:?}");
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    let file = folder.join("Song B/Ideas.md");
    assert_eq!(read(&file), b"# Ideas\n\nA key change.\n");

    // Saved in an editor (Obsidian): the page gets the change as a diff,
    // keeping what someone typed into the page meanwhile.
    drive_spine::pages::write(&p.doc, "# Ideas\n\nA key change.\n\nFrom the app.\n").unwrap();
    std::fs::write(&file, b"# Ideas\n\nA key change, up a tone.\n").unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    // The page changed in the Drive and on disk since the last sync: both
    // are kept (the disk's beside it as a conflict copy), as for any file.
    assert_eq!(done.conflicts, 1, "{done:?}");

    // Edited only on disk: the page follows.
    let level = a.reconcile(&w).await.unwrap();
    assert!(level.errors.is_empty(), "{:?}", level.errors);
    std::fs::write(&file, b"# Ideas\n\nKey change at the bridge.\n").unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.up, 1, "{done:?}");
    assert_eq!(
        drive_spine::pages::text(&p.doc),
        "# Ideas\n\nKey change at the bridge.\n"
    );
    // Edited only in the Drive: the file follows.
    drive_spine::pages::write(&p.doc, "# Ideas\n\nKey change at the bridge, then out.\n").unwrap();
    let done = a.reconcile(&w).await.unwrap();
    assert_eq!(done.down, 1, "{done:?}");
    assert_eq!(read(&file), b"# Ideas\n\nKey change at the bridge, then out.\n");
    assert_eq!(a.reconcile(&w).await.unwrap(), crate::working::Reconciled::default());
}
