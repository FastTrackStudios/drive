//! The index database under load from several processes at once
//! (drive#157): a daemon-like writer walking a big location again and
//! again while `drive`-like commands, each its own process, read it and
//! write locations of their own. With no agent running, commands open the
//! database themselves, side by side: turso's multi-process log is what
//! keeps that whole.

use super::*;

const DB: &str = "DRIVE_STRESS_DB";
const ROLE: &str = "DRIVE_STRESS_ROLE";

/// Files of the big location.
const BIG: usize = 12_000;
/// Walks the daemon makes, and rounds each command makes.
const ROUNDS: usize = 4;

fn e(path: String, size: u64, sampled: String) -> IndexEntry {
    IndexEntry {
        path,
        dir: false,
        size,
        mtime: 7,
        sampled,
    }
}

/// The big location's entries at walk `round`: a tenth of them changed,
/// some gone and some new each time.
fn big(round: usize) -> Vec<IndexEntry> {
    (0..BIG)
        .filter(|i| !(i + round).is_multiple_of(50))
        .map(|i| {
            let size = if i % 10 == round % 10 {
                i as u64 + round as u64
            } else {
                i as u64
            };
            e(
                format!("Pack {:03}/s{i:06}.wav", i / 100),
                size,
                format!("{:064x}", i % 3000),
            )
        })
        .chain((0..round * 7).map(|k| e(format!("New/r{round}-{k}.wav"), 1, format!("{k:064x}"))))
        .collect()
}

/// A command's own location at its round `round`.
fn own(me: &str, round: usize) -> LocationIndex {
    LocationIndex {
        location: format!("cli-{me}"),
        root: "/cli".into(),
        built: round as i64,
        entries: (0..300 + round * 10)
            .map(|i| {
                e(
                    format!("d{}/f{i:04}", i % 7),
                    (i * round) as u64,
                    format!("{:064x}", i),
                )
            })
            .collect(),
    }
}

/// One walk of the big location, as the agent makes it.
async fn walk(db: &IndexDb, round: usize) {
    let w = db.begin_walk("big").await.unwrap();
    for batch in big(round).chunks(drive_files::index::EMIT) {
        db.walk_put(&w, batch).await.unwrap();
    }
    db.commit_walk(&w, "/big", round as i64).await.unwrap();
    db.discard_walk(w).await.unwrap();
}

/// A command reading: every page in path order, folders, copies, names.
async fn read(db: &IndexDb) {
    let mut after = String::new();
    let mut n = 0;
    loop {
        let page = db.page("big", &after, 2000).await.unwrap();
        let Some(last) = page.last() else { break };
        assert!(page.is_sorted_by(|a, b| a.path < b.path) && last.path > after);
        after = last.path.clone();
        n += page.len();
    }
    // Pages are read one after another while walks commit between them
    // (each in several transactions): about the location's size.
    assert!(n > BIG / 2 && n < BIG * 2, "{n}");
    assert!(!db.list("big", "Pack 007").await.unwrap().is_empty());
    assert!(
        !db.copies(&format!("{:064x}", 42), None)
            .await
            .unwrap()
            .is_empty()
    );
    db.find("s00012", 20).await.unwrap();
    db.meta("big").await.unwrap().expect("there");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_database_stays_whole_with_a_walking_daemon_and_commands_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("indexes.db");
    let db = IndexDb::open(&path).await.unwrap();
    walk(&db, 0).await;

    // The commands: two reading, two writing, each its own process (this
    // test binary again, running `command`).
    let exe = std::env::current_exe().unwrap();
    let children: Vec<std::process::Child> = ["read", "read", "write", "write"]
        .iter()
        .map(|role| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "index_db::stress::command",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(DB, &path)
                .env(ROLE, role)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for round in 1..=ROUNDS {
        walk(&db, round).await;
    }
    let pids: Vec<u32> = children.iter().map(|c| c.id()).collect();
    for c in children {
        let out = tokio::task::spawn_blocking(move || c.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.status.success(),
            "a command failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    drop(db);
    let db = IndexDb::open(&path).await.unwrap();
    assert_eq!(db.integrity_check().await.unwrap(), ["ok"]);
    let mut want = big(ROUNDS);
    want.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(db.load("big").await.unwrap().unwrap().entries, want);
    // Each writing command's last put is there, whole.
    for (pid, role) in pids.iter().zip(["read", "read", "write", "write"]) {
        let theirs = db.load(&format!("cli-{pid}")).await.unwrap();
        match role {
            "write" => {
                let mut want = own(&pid.to_string(), ROUNDS);
                want.entries.sort_by(|a, b| a.path.cmp(&b.path));
                assert_eq!(theirs, Some(want));
            }
            _ => assert!(theirs.is_none()),
        }
    }
}

/// One `drive`-like command beside the daemon: run only by the test
/// above, as a process of its own.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by the_database_stays_whole_with_a_walking_daemon_and_commands_beside_it"]
async fn command() {
    let (Some(path), Some(role)) = (std::env::var_os(DB), std::env::var(ROLE).ok()) else {
        return;
    };
    let db = IndexDb::open(std::path::Path::new(&path)).await.unwrap();
    let me = std::process::id().to_string();
    for round in 1..=ROUNDS {
        match role.as_str() {
            "read" => read(&db).await,
            _ => {
                // A location put, forgotten and put again (as `drive
                // location rm` and a download do).
                db.put(&own(&me, round)).await.unwrap();
                if round < ROUNDS {
                    db.remove(&format!("cli-{me}")).await.unwrap();
                }
                read(&db).await;
            }
        }
    }
}
