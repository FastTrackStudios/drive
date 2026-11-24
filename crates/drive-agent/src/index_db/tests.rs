use super::*;

fn e(path: &str, dir: bool, size: u64, sampled: &str) -> IndexEntry {
    IndexEntry {
        path: path.into(),
        dir,
        size,
        mtime: 7,
        sampled: sampled.into(),
    }
}

fn index(location: &str, entries: Vec<IndexEntry>) -> LocationIndex {
    LocationIndex {
        location: location.into(),
        root: "/music".into(),
        built: 42,
        entries,
    }
}

async fn db() -> (IndexDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(&dir.path().join("indexes.db")).await.unwrap();
    (db, dir)
}

fn songs() -> LocationIndex {
    index(
        "loc",
        vec![
            e("Album", true, 0, ""),
            e("Album/b.wav", false, 20, "hb"),
            e("Album/A.wav", false, 10, "ha"),
            e("Album/Stems", true, 0, ""),
            e("notes.txt", false, 3, "hn"),
        ],
    )
}

#[tokio::test]
async fn an_index_is_kept_and_read_back_by_folder_path_and_page() {
    let (db, _dir) = db().await;
    assert_eq!(db.put(&songs()).await.unwrap(), 5);
    assert_eq!(
        db.meta("loc").await.unwrap(),
        Some(Meta { root: "/music".into(), built: 42, files: 3, dirs: 2, bytes: 33 })
    );
    let names = |v: Vec<IndexEntry>| v.into_iter().map(|e| e.path).collect::<Vec<_>>();
    assert_eq!(names(db.list("loc", "").await.unwrap()), ["Album", "notes.txt"]);
    // Folders first, then by name whatever its case.
    assert_eq!(
        names(db.list("loc", "Album").await.unwrap()),
        ["Album/Stems", "Album/A.wav", "Album/b.wav"]
    );
    assert_eq!(db.get("loc", "Album/b.wav").await.unwrap().unwrap().size, 20);
    assert!(db.get("loc", "nope").await.unwrap().is_none());
    assert!(db.get("elsewhere", "notes.txt").await.unwrap().is_none());

    let first = db.page("loc", "", 2).await.unwrap();
    let rest = db.page("loc", &first[1].path, 10).await.unwrap();
    assert_eq!(names(first), ["Album", "Album/A.wav"]);
    assert_eq!(names(rest), ["Album/Stems", "Album/b.wav", "notes.txt"]);

    let under = db.page_under("loc", "Album", "", 10).await.unwrap();
    assert_eq!(names(under), ["Album/A.wav", "Album/Stems", "Album/b.wav"]);
    let more = db.page_under("loc", "Album", "Album/A.wav", 1).await.unwrap();
    assert_eq!(names(more), ["Album/Stems"]);

    let back = db.load("loc").await.unwrap().unwrap();
    let mut want = songs();
    want.entries.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(back, want);
    assert_eq!(db.locations().await.unwrap(), ["loc"]);
}

#[tokio::test]
async fn a_new_index_writes_only_its_differences() {
    let (db, _dir) = db().await;
    db.put(&songs()).await.unwrap();
    let mut next = songs();
    next.entries.retain(|e| e.path != "notes.txt");
    next.entries[1].size = 21;
    next.entries.push(e("Album/c.wav", false, 5, "hc"));
    assert_eq!(db.put(&next).await.unwrap(), 3, "one gone, one changed, one new");
    assert_eq!(db.put(&next).await.unwrap(), 0, "nothing changed: nothing written");
    let back = db.load("loc").await.unwrap().unwrap();
    let mut want = next.clone();
    want.entries.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(back.entries, want.entries);
    assert_eq!(db.meta("loc").await.unwrap().unwrap().files, 3);
}

#[tokio::test]
async fn copies_are_found_across_locations_and_a_removed_location_is_gone() {
    let (db, _dir) = db().await;
    db.put(&songs()).await.unwrap();
    db.put(&index("backup", vec![e("old/A.wav", false, 10, "ha"), e("old/x.wav", false, 99, "ha")]))
        .await
        .unwrap();
    let at = |v: Vec<(String, IndexEntry)>| v.into_iter().map(|(l, e)| format!("{l}:{}", e.path)).collect::<Vec<_>>();
    assert_eq!(
        at(db.copies("ha", Some(10)).await.unwrap()),
        ["backup:old/A.wav", "loc:Album/A.wav"]
    );
    assert_eq!(at(db.copies("ha", None).await.unwrap()).len(), 3);
    assert!(db.copies("", None).await.unwrap().is_empty());

    let held = db.locations_of(&["ha".into(), "hb".into(), "none".into()]).await.unwrap();
    let mut ha = held["ha"].clone();
    ha.sort();
    assert_eq!(ha, ["backup", "backup", "loc"], "one per file holding it");
    assert_eq!(held["hb"], ["loc"]);
    assert!(!held.contains_key("none"));
    assert_eq!(db.duplicates(1).await.unwrap(), [("ha".to_string(), 10, 2)]);
    assert!(db.duplicates(11).await.unwrap().is_empty());

    db.remove("backup").await.unwrap();
    assert_eq!(at(db.copies("ha", None).await.unwrap()), ["loc:Album/A.wav"]);
    assert!(db.meta("backup").await.unwrap().is_none());
    assert_eq!(db.locations().await.unwrap(), ["loc"]);
}

#[tokio::test]
async fn the_index_outlives_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    IndexDb::open(&path).await.unwrap().put(&songs()).await.unwrap();
    let again = IndexDb::open(&path).await.unwrap();
    assert_eq!(again.list("loc", "").await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_write_leaves_no_log_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    let db = IndexDb::open(&path).await.unwrap();
    let entries = (0..5000).map(|i| e(&format!("f{i:05}.wav"), false, i, &format!("{i:064x}"))).collect();
    db.put(&index("loc", entries)).await.unwrap();
    let wal = std::fs::metadata(dir.path().join("indexes.db-wal")).map_or(0, |m| m.len());
    assert_eq!(wal, 0, "the log is folded into the database after a write");
    assert_eq!(db.meta("loc").await.unwrap().unwrap().files, 5000);
}

#[tokio::test]
async fn an_index_larger_than_a_batch_is_written_changed_and_removed() {
    let (db, _dir) = db().await;
    let n = BATCH * 2 + 500;
    let mut entries: Vec<IndexEntry> =
        (0..n).map(|i| e(&format!("f{i:06}.wav"), false, i as u64, &format!("{i:064x}"))).collect();
    assert_eq!(db.put(&index("loc", entries.clone())).await.unwrap(), n);
    assert_eq!(db.meta("loc").await.unwrap().unwrap().files, n as u64);
    // One changed, one gone, one new: three rows written.
    entries[7].size += 1;
    entries.remove(BATCH + 3);
    entries.push(e("zz-new.wav", false, 1, "hz"));
    assert_eq!(db.put(&index("loc", entries)).await.unwrap(), 3);
    assert_eq!(db.list("loc", "").await.unwrap().len(), n);
    assert_eq!(db.get("loc", "f000007.wav").await.unwrap().unwrap().size, 8);
    assert!(db.get("loc", &format!("f{:06}.wav", BATCH + 3)).await.unwrap().is_none());
    db.remove("loc").await.unwrap();
    assert!(db.meta("loc").await.unwrap().is_none());
    assert!(db.list("loc", "").await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "about a minute in release: cargo test --release -p drive-agent a_million -- --ignored --nocapture"]
async fn a_million_files_fit_where_one_transaction_would_not() {
    // One location this size was more than the multi-process log holds
    // (64 x 4096 pages) in one transaction, and its first index could
    // never be put (THEBATTLESHIP, 2026-11-24: "database is locked").
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    let db = IndexDb::open(&path).await.unwrap();
    let n = 1_200_000usize;
    let entries: Vec<IndexEntry> = (0..n)
        .map(|i| {
            e(
                &format!("Library {:03}/Pack {:05}/Some Long Sample Name {i:08}.wav", i / 50_000, i / 40),
                false,
                i as u64,
                &format!("{i:064x}"),
            )
        })
        .collect();
    let t0 = std::time::Instant::now();
    assert_eq!(db.put(&index("big", entries)).await.unwrap(), n);
    eprintln!("put {n}: {:?}", t0.elapsed());
    let wal = std::fs::metadata(dir.path().join("indexes.db-wal")).map_or(0, |m| m.len());
    eprintln!("log after: {wal} bytes");
    assert_eq!(db.meta("big").await.unwrap().unwrap().files, n as u64);
    let t0 = std::time::Instant::now();
    db.remove("big").await.unwrap();
    eprintln!("remove: {:?}", t0.elapsed());
    assert!(db.meta("big").await.unwrap().is_none());
}

#[tokio::test]
async fn two_handles_share_one_database() {
    // As the daemon and a `drive` command do (each its own process: this
    // is the closest one test gets).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    let (a, b) = tokio::join!(IndexDb::open(&path), IndexDb::open(&path));
    let (a, b) = (a.unwrap(), b.unwrap());
    a.put(&songs()).await.unwrap();
    assert_eq!(b.list("loc", "").await.unwrap().len(), 2);
}

#[tokio::test]
#[ignore = "timing, by hand: cargo test --release -p drive-agent index_db_timing -- --ignored --nocapture"]
async fn index_db_timing() {
    let (db, _dir) = db().await;
    let n = 500_000usize;
    let mut entries: Vec<IndexEntry> = (0..n)
        .map(|i| e(&format!("Pack {:04}/Sample {i:07}.wav", i / 40), false, i as u64, &format!("{i:064x}")))
        .collect();
    entries.extend((0..n / 40).map(|p| e(&format!("Pack {p:04}"), true, 0, "")));
    let t0 = std::time::Instant::now();
    db.put(&index("big", entries.clone())).await.unwrap();
    eprintln!("first put of {}: {:?}", entries.len(), t0.elapsed());
    entries[77].size += 1;
    let t0 = std::time::Instant::now();
    db.put(&index("big", entries)).await.unwrap();
    eprintln!("one change: {:?}", t0.elapsed());
    for round in 0..2 {
        let t0 = std::time::Instant::now();
        let l = db.list("big", "Pack 0123").await.unwrap();
        let tl = t0.elapsed();
        let t0 = std::time::Instant::now();
        let g = db.get("big", "Pack 0123/Sample 0004930.wav").await.unwrap();
        let tg = t0.elapsed();
        let t0 = std::time::Instant::now();
        let c = db.copies(&format!("{:064x}", 4930), None).await.unwrap();
        let tc = t0.elapsed();
        eprintln!("round {round}: list {} in {tl:?}, get {} in {tg:?}, copies {} in {tc:?}", l.len(), g.is_some(), c.len());
    }
    let t0 = std::time::Instant::now();
    let all = db.load("big").await.unwrap().unwrap();
    eprintln!("load {}: {:?}", all.entries.len(), t0.elapsed());
}
