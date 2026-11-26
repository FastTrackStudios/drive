use super::*;
use drive_files::index::LocationIndex;

fn e(path: &str, dir: bool, size: u64, sampled: &str) -> IndexEntry {
    IndexEntry {
        path: path.into(),
        dir,
        size,
        mtime: 7,
        sampled: sampled.into(),
    }
}

fn index(entries: Vec<IndexEntry>) -> LocationIndex {
    let mut entries = entries;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    LocationIndex {
        location: "loc".into(),
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

/// Walk `entries` into the database (in the order given, in uneven
/// batches, as walking threads hand them over) and commit it.
async fn walk_in(db: &IndexDb, entries: &[IndexEntry]) -> (usize, Totals) {
    let walk = db.begin_walk("loc").await.unwrap();
    for batch in entries.chunks(777) {
        db.walk_put(&walk, batch).await.unwrap();
    }
    let done = db.commit_walk(&walk, "/music", 42).await.unwrap();
    db.discard_walk(walk).await.unwrap();
    done
}

fn many(n: usize) -> Vec<IndexEntry> {
    let mut v: Vec<IndexEntry> = (0..n)
        .map(|i| {
            e(
                &format!("Pack {:03}/f{i:06}.wav", i / 40),
                false,
                i as u64,
                &format!("{i:064x}"),
            )
        })
        .collect();
    v.extend((0..n.div_ceil(40)).map(|p| e(&format!("Pack {p:03}"), true, 0, "")));
    // Not in path order: as a walk finds them.
    v.reverse();
    v
}

#[tokio::test]
async fn a_walk_is_written_as_its_differences_page_by_page() {
    let (db, _dir) = db().await;
    let mut entries = many(PAGE + PAGE / 2);
    let (written, totals) = walk_in(&db, &entries).await;
    assert_eq!(written, entries.len());
    assert_eq!(
        (totals.files, totals.dirs),
        (
            (PAGE + PAGE / 2) as u64,
            (PAGE + PAGE / 2).div_ceil(40) as u64
        )
    );
    assert_eq!(
        db.load("loc").await.unwrap().unwrap(),
        index(entries.clone())
    );
    assert_eq!(db.meta("loc").await.unwrap().unwrap().files, totals.files);

    // Unchanged: nothing written.
    assert_eq!(walk_in(&db, &entries).await.0, 0);

    // One changed, two gone, one new.
    assert!(!entries[1000].dir);
    entries[1000].size += 1;
    let gone = [entries.remove(PAGE - 1), entries.remove(PAGE + 1)];
    entries.push(e("Pack 000/zz new.wav", false, 1, "hz"));
    let (written, totals) = walk_in(&db, &entries).await;
    assert_eq!(written, 4);
    assert_eq!(
        db.load("loc").await.unwrap().unwrap(),
        index(entries.clone())
    );
    assert_eq!(totals.bytes, entries.iter().map(|e| e.size).sum::<u64>());
    for g in gone {
        assert!(db.get("loc", &g.path).await.unwrap().is_none());
    }

    // Everything gone.
    assert_eq!(walk_in(&db, &[]).await.0, entries.len());
    assert!(db.list("loc", "").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_walk_and_a_whole_index_put_the_same_rows() {
    let (a, _da) = db().await;
    let (b, _db) = db().await;
    let entries = many(2500);
    walk_in(&a, &entries).await;
    b.put(&index(entries)).await.unwrap();
    assert_eq!(a.load("loc").await.unwrap(), b.load("loc").await.unwrap());
    assert_eq!(a.meta("loc").await.unwrap(), b.meta("loc").await.unwrap());
}

#[tokio::test]
async fn the_upload_encoded_from_the_rows_is_the_same_bytes() {
    let (db, _dir) = db().await;
    assert!(db.encode("loc").await.unwrap().is_none());
    walk_in(&db, &many(BATCH + 333)).await;
    let whole = db.load("loc").await.unwrap().unwrap();
    assert_eq!(
        db.encode("loc").await.unwrap().unwrap(),
        drive_files::index::encode(&whole)
    );
}

#[tokio::test]
async fn moved_folders_are_found_in_the_database_as_in_memory() {
    let (db, _dir) = db().await;
    let old = vec![
        e("Presets", true, 0, ""),
        e("Presets/Warm", true, 0, ""),
        e("Presets/Warm/a.fxp", false, 1, "h1"),
        e("Presets/Warm/sub", true, 0, ""),
        e("Presets/Warm/sub/b.fxp", false, 1, "h2"),
        e("Presets/Copied", true, 0, ""),
        e("Presets/Copied/c.fxp", false, 1, "h3"),
        e("Presets/Kept", true, 0, ""),
        e("Presets/Kept/k.fxp", false, 1, "h4"),
        e("Empty", true, 0, ""),
    ];
    let new = vec![
        e("Presets", true, 0, ""),
        e("Presets/Kept", true, 0, ""),
        e("Presets/Kept/k.fxp", false, 1, "h4"),
        e("Bass", true, 0, ""),
        e("Bass/Warm Bass", true, 0, ""),
        e("Bass/Warm Bass/a.fxp", false, 1, "h1"),
        e("Bass/Warm Bass/sub", true, 0, ""),
        e("Bass/Warm Bass/sub/b.fxp", false, 1, "h2"),
        // Holds a.fxp at the same place, but not b.fxp: no match.
        e("Other", true, 0, ""),
        e("Other/a.fxp", false, 1, "h1"),
        e("X", true, 0, ""),
        e("X/c.fxp", false, 1, "h3"),
        e("Y", true, 0, ""),
        e("Y/c.fxp", false, 1, "h3"),
    ];
    walk_in(&db, &old).await;
    let wanted: Vec<String> = [
        "Presets/Warm",
        "Presets/Copied",
        "Empty",
        "Presets",
        "Presets/Kept",
    ]
    .map(String::from)
    .to_vec();
    let walk = db.begin_walk("loc").await.unwrap();
    db.walk_put(&walk, &new).await.unwrap();
    let found = db.moved_folders(&walk, &wanted).await.unwrap();
    db.discard_walk(walk).await.unwrap();
    assert_eq!(
        found,
        drive_files::index::moved_folders(&old, &new, &wanted)
    );
    assert_eq!(
        found,
        [("Presets/Warm".to_string(), "Bass/Warm Bass".to_string())]
    );
}

#[tokio::test]
async fn a_walk_is_filled_from_what_the_walking_threads_send() {
    let (db, _dir) = db().await;
    let entries = many(BATCH + 1234);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let walk = db.begin_walk("loc").await.unwrap();
    let send = {
        let entries = entries.clone();
        tokio::spawn(async move {
            for batch in entries.chunks(drive_files::index::EMIT) {
                tx.send(batch.to_vec()).await.unwrap();
            }
        })
    };
    db.fill_walk(&walk, rx).await.unwrap();
    send.await.unwrap();
    db.commit_walk(&walk, "/music", 42).await.unwrap();
    db.discard_walk(walk).await.unwrap();
    assert_eq!(db.load("loc").await.unwrap().unwrap(), index(entries));
}

/// The walks there, and how many rows they hold between them.
async fn walks(db: &IndexDb) -> (Vec<(i64, i64)>, i64) {
    let r = db.reader().await;
    let mut rows = r
        .query("SELECT walk, pid FROM walks ORDER BY walk", ())
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        out.push((
            int(row.get_value(0).unwrap()),
            int(row.get_value(1).unwrap()),
        ));
    }
    let mut rows = r
        .query("SELECT count(*) FROM walk_entries", ())
        .await
        .unwrap();
    let n = int(rows.next().await.unwrap().unwrap().get_value(0).unwrap());
    (out, n)
}

#[tokio::test]
async fn a_walk_is_dropped_when_done_or_its_process_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    let db = IndexDb::open(&path).await.unwrap();
    let walk = db.begin_walk("loc").await.unwrap();
    db.walk_put(&walk, &[e("a", false, 1, "h")]).await.unwrap();
    assert_eq!(walks(&db).await.1, 1);
    db.discard_walk(walk).await.unwrap();
    assert_eq!(walks(&db).await, (Vec::new(), 0));

    // One left by a process that is gone, and one by this one (running).
    let me = i64::from(std::process::id());
    {
        let conn = db.write.lock().await;
        for (walk, pid) in [(7, i64::from(u32::MAX - 1)), (8, me)] {
            conn.execute(
                "INSERT INTO walks (walk, pid, location) VALUES (?1, ?2, 'loc')",
                vec![Value::Integer(walk), Value::Integer(pid)],
            )
            .await
            .unwrap();
            insert_walk(&conn, walk, &many(30)).await.unwrap();
        }
    }
    drop(db);
    let again = IndexDb::open(&path).await.unwrap();
    assert_eq!(walks(&again).await, (vec![(8, me)], many(30).len() as i64));
}

#[test]
fn pages_are_compared_as_far_as_both_are_known() {
    let p = |paths: &[&str]| {
        paths
            .iter()
            .map(|x| e(x, false, 1, "h"))
            .collect::<Vec<_>>()
    };
    let (a, b) = (p(&["a", "c", "e"]), p(&["b", "d"]));
    assert_eq!(
        comparable_upto(&a, &b, 3),
        Some("e"),
        "only the walk's page is full"
    );
    assert_eq!(comparable_upto(&b, &a, 3), Some("e"));
    assert_eq!(comparable_upto(&a, &p(&["b", "d", "f"]), 3), Some("e"));
    assert_eq!(
        comparable_upto(&b, &b, 3),
        None,
        "both end within their pages"
    );
    assert_eq!(up_to(a.clone(), Some("c")).len(), 2);
    assert_eq!(up_to(a, None).len(), 3);
    assert_eq!(container("a/b/x/y.wav", "x/y.wav"), Some("a/b"));
    assert_eq!(container("x/y.wav", "x/y.wav"), None);
    assert_eq!(container("a/bx/y.wav", "x/y.wav"), None);
}

#[test]
fn two_pages_differ_by_what_is_gone_changed_and_added() {
    let new = vec![
        e("a", false, 1, "h"),
        e("b", false, 2, "h"),
        e("d", false, 1, "h"),
    ];
    let old = vec![
        e("b", false, 1, "h"),
        e("c", false, 1, "h"),
        e("d", false, 1, "h"),
    ];
    let ops: Vec<String> = diff_pages(&new, old)
        .into_iter()
        .map(|op| match op {
            Op::Added(e) => format!("+{}", e.path),
            Op::Changed(e) => format!("~{}", e.path),
            Op::Gone(p) => format!("-{p}"),
        })
        .collect();
    assert_eq!(ops, ["+a", "~b", "-c"]);
}

/// This process's peak resident memory (MB) since the last reset (Linux).
fn peak_rss_mb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb >> 10)
}

fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// A sample library of `n` files as a walk finds them: a folder of 40 at
/// a time, in no path order across folders.
fn library(n: usize) -> impl Iterator<Item = IndexEntry> {
    (0..n.div_ceil(40)).rev().flat_map(move |d| {
        let dir = format!(
            "Vendor {:03}/Pack {:02} - Something Long/Category {:02}",
            d / 1250,
            d / 25 % 50,
            d % 25
        );
        let files = (d * 40..(d * 40 + 40).min(n)).map({
            let dir = dir.clone();
            move |i| {
                e(
                    &format!("{dir}/Sample_{i} Kick Hard.wav"),
                    false,
                    1_000_000 + i as u64,
                    &format!("{:064x}", i as u128 * 0x9e37_79b9_7f4a_7c15),
                )
            }
        });
        std::iter::once(e(&dir, true, 0, "")).chain(files)
    })
}

/// How much memory a first index of a big location takes in the agent,
/// as it was (the walk's whole result, then `put`, then `encode`) and as
/// it is (batches into a walk table, compared page by page, encoded from
/// the rows). Each in its own process, as it is measured from the
/// process's peak:
/// `DRIVE_REINDEX=whole cargo test --release -p drive-agent reindex_memory -- --ignored --nocapture`
/// and again with `DRIVE_REINDEX=streamed` (`DRIVE_REINDEX_FILES`: how many).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "minutes in release, by hand: see the doc comment"]
async fn reindex_memory() {
    let n: usize = std::env::var("DRIVE_REINDEX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_500_000);
    let how = std::env::var("DRIVE_REINDEX").unwrap_or_else(|_| "streamed".into());
    let (db, _dir) = db().await;
    let base = peak_rss_mb();
    reset_peak_rss();
    let t0 = std::time::Instant::now();
    let bytes = match how.as_str() {
        "whole" => {
            let mut entries: Vec<IndexEntry> = library(n).collect();
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            let idx = LocationIndex {
                location: "loc".into(),
                root: "/music".into(),
                built: 42,
                entries,
            };
            db.put(&idx).await.unwrap();
            drive_files::index::encode(&idx)
        }
        _ => {
            let walk = db.begin_walk("loc").await.unwrap();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let walking = tokio::task::spawn_blocking(move || {
                let mut batch = Vec::with_capacity(drive_files::index::EMIT);
                for entry in library(n) {
                    batch.push(entry);
                    if batch.len() == drive_files::index::EMIT {
                        tx.blocking_send(std::mem::take(&mut batch)).unwrap();
                    }
                }
                tx.blocking_send(batch).unwrap();
            });
            db.fill_walk(&walk, rx).await.unwrap();
            walking.await.unwrap();
            db.commit_walk(&walk, "/music", 42).await.unwrap();
            db.discard_walk(walk).await.unwrap();
            db.encode("loc").await.unwrap().unwrap()
        }
    };
    eprintln!(
        "{how}: {n} files, peak {} MB over a {base} MB start, {} MB on the wire, {:?}",
        peak_rss_mb().saturating_sub(base),
        bytes.len() >> 20,
        t0.elapsed()
    );
}
