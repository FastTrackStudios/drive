//! Where a node keeps things: document snapshots, and every location's
//! index (one row per file, so all of them can be searched at once, even
//! for devices that are off).

use std::collections::HashMap;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use drive_files::index::LocationIndex;
use serde::Serialize;
use tokio::sync::{Mutex, MutexGuard, Semaphore, SemaphorePermit};
use tracing::field::Empty;
use turso::Value;

/// How many connections serve reads. Writes have one of their own, so a
/// big index coming in never holds up a read, and a slow read holds up
/// only itself (until every read connection is busy).
const READERS: usize = 4;

pub struct Store {
    /// Every write, and the transactions, on one connection.
    write: Mutex<turso::Connection>,
    /// True while a write transaction is open on `write`, so if its future
    /// was dropped mid-way (a cancelled index upload), the very next writer
    /// rolls it back before any `save` can execute inside it.
    tx_open: AtomicBool,
    reads: ReadPool,
    /// Bumped each time a space's index rows change (put, drop, rename):
    /// what is worked out from them is good while it stays the same.
    generations: std::sync::Mutex<HashMap<String, u64>>,
    /// The last [`Store::list`], kept until a document is saved, renamed
    /// or deleted: every listing reads it (a view of version 2 asks every
    /// few seconds; one of 3 once, then hears changes, drive#159), and
    /// each used to read the whole `docs` table (drive#61).
    listing: std::sync::Mutex<Listing>,
}

#[derive(Default)]
struct Listing {
    /// Bumped by every write to `docs`.
    generation: u64,
    rows: Option<(u64, std::sync::Arc<Vec<(String, u64)>>)>,
}

/// Read connections, handed out one request at a time.
struct ReadPool {
    idle: std::sync::Mutex<Vec<turso::Connection>>,
    permits: Semaphore,
}

/// A connection held for one store call. On drop it records how long the
/// call held it (`store.query_ms`, beside the `store.wait_ms` it waited for
/// it) on the call's span, and a read connection goes back to the pool.
struct Held<'a> {
    conn: Option<turso::Connection>,
    hold: Hold<'a>,
    since: Instant,
}

enum Hold<'a> {
    Write(MutexGuard<'a, turso::Connection>),
    Read(&'a ReadPool, #[allow(dead_code)] SemaphorePermit<'a>),
}

impl Deref for Held<'_> {
    type Target = turso::Connection;
    fn deref(&self) -> &turso::Connection {
        match &self.hold {
            Hold::Write(g) => g,
            Hold::Read(..) => self.conn.as_ref().expect("held until dropped"),
        }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        tracing::Span::current().record("store.query_ms", ms(self.since));
        if let (Hold::Read(pool, _), Some(conn)) = (&self.hold, self.conn.take()) {
            pool.idle.lock().expect("read pool").push(conn);
        }
    }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// How many files and bytes of each extension and in each location of a
/// space (`/stats`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
    pub exts: HashMap<String, (u64, u64)>,
    pub locations: HashMap<String, (u64, u64)>,
}

impl Stats {
    fn add(&mut self, location: &str, ext: &str, files: u64, bytes: u64) {
        let e = self.exts.entry(ext.to_string()).or_default();
        e.0 += files;
        e.1 += bytes;
        let l = self.locations.entry(location.to_string()).or_default();
        l.0 += files;
        l.1 += bytes;
        self.files += files;
        self.bytes += bytes;
    }

    /// Counted file by file, from every row (what `/stats` did before the
    /// totals were kept): the reference the kept totals are tested against.
    pub fn of_rows(rows: &[drive_files::protect::FileRow]) -> Stats {
        let mut s = Stats::default();
        for r in rows {
            let name = r.path.rsplit('/').next().unwrap_or_default();
            s.add(&r.location, &ext_of(name), 1, r.size);
        }
        s
    }
}

/// A file or folder found in some location's index.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub location: String,
    pub path: String,
    pub name: String,
    pub dir: bool,
    pub size: i64,
    pub mtime: i64,
    pub sampled: String,
}

fn text(v: Value) -> String {
    match v {
        Value::Text(t) => t,
        _ => String::new(),
    }
}

fn int(v: Value) -> i64 {
    match v {
        Value::Integer(n) => n,
        _ => 0,
    }
}

/// Wait for a lock another connection holds (a checkpoint, the writer)
/// instead of failing at once.
async fn busy_wait(conn: &turso::Connection) {
    if let Err(e) = conn.execute("PRAGMA busy_timeout = 5000", ()).await {
        tracing::warn!(error = %e, "the store's busy timeout was not set");
    }
}

/// A file name's extension, lowercase, without the dot (`""` for none).
fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

impl Store {
    pub async fn open(path: &Path) -> turso::Result<Store> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .build()
            .await?;
        let conn = db.connect()?;
        busy_wait(&conn).await;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS docs (id TEXT PRIMARY KEY, snapshot BLOB NOT NULL)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS entries (space TEXT NOT NULL, location TEXT NOT NULL, parent TEXT NOT NULL, \
             path TEXT NOT NULL, name TEXT NOT NULL, lname TEXT NOT NULL, dir INTEGER NOT NULL, size INTEGER NOT NULL, \
             mtime INTEGER NOT NULL, sampled TEXT NOT NULL)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS entries_dir ON entries (space, location, parent)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS entries_name ON entries (space, lname)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS entries_content ON entries (space, sampled, size)",
            (),
        )
        .await?;
        // A file's extension, lowercase, so a search by kind (`.setlist`,
        // `.kf`) reads its rows by an index instead of matching every name.
        // Added to older databases (the error when it is there already is
        // the column existing), and filled in once for their rows.
        let _ = conn
            .execute(
                "ALTER TABLE entries ADD COLUMN ext TEXT NOT NULL DEFAULT ''",
                (),
            )
            .await;
        // The extension first: a lookup by kind is then a plain index
        // range, whatever the planner makes of the space.
        let _ = conn.execute("DROP INDEX IF EXISTS entries_ext", ()).await;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS entries_kind ON entries (ext, space)",
            (),
        )
        .await?;
        conn.execute(
            "UPDATE entries SET ext = replace(lname, rtrim(lname, replace(lname, '.', '')), '') \
             WHERE ext = '' AND dir = 0 AND lname LIKE '%.%'",
            (),
        )
        .await?;
        // Each location's files and bytes by extension, kept as its index
        // comes in, so `/stats` reads a few rows instead of every file.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS entry_stats (space TEXT NOT NULL, location TEXT NOT NULL, \
             ext TEXT NOT NULL, files INTEGER NOT NULL, bytes INTEGER NOT NULL, \
             PRIMARY KEY (space, location, ext))",
            (),
        )
        .await?;
        // A database from before the totals: counted once from its rows.
        let has = |sql: &'static str| {
            let conn = &conn;
            async move {
                let mut rows = conn.query(sql, ()).await?;
                Ok::<_, turso::Error>(rows.next().await?.is_some())
            }
        };
        if !has("SELECT 1 FROM entry_stats LIMIT 1").await?
            && has("SELECT 1 FROM entries WHERE dir = 0 LIMIT 1").await?
        {
            conn.execute(
                "INSERT INTO entry_stats (space, location, ext, files, bytes) \
                 SELECT space, location, ext, count(*), sum(size) FROM entries WHERE dir = 0 \
                 GROUP BY space, location, ext",
                (),
            )
            .await?;
        }
        let mut idle = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let c = db.connect()?;
            busy_wait(&c).await;
            idle.push(c);
        }
        Ok(Store {
            write: Mutex::new(conn),
            tx_open: AtomicBool::new(false),
            reads: ReadPool {
                idle: std::sync::Mutex::new(idle),
                permits: Semaphore::new(READERS),
            },
            generations: Default::default(),
            listing: Default::default(),
        })
    }

    /// The write connection (waited for: `store.wait_ms`).
    async fn writer(&self) -> Held<'_> {
        let asked = Instant::now();
        let g = self.write.lock().await;
        // If an earlier transactional write (such as `put_index`) was dropped
        // mid-way, roll its open transaction back before *any* later write
        // (`save`, `delete`, another `put_index`, …) runs inside it.
        if self.tx_open.swap(false, Ordering::AcqRel) {
            let _ = g.execute("ROLLBACK", ()).await;
        }
        let span = tracing::Span::current();
        span.record("store.wait_ms", ms(asked));
        span.record("store.conn", "write");
        Held {
            conn: None,
            hold: Hold::Write(g),
            since: Instant::now(),
        }
    }

    /// A read connection (waited for: `store.wait_ms`).
    async fn reader(&self) -> Held<'_> {
        let asked = Instant::now();
        let permit = self
            .reads
            .permits
            .acquire()
            .await
            .expect("the read pool is never closed");
        let conn = self.reads.idle.lock().expect("read pool").pop();
        let span = tracing::Span::current();
        span.record("store.wait_ms", ms(asked));
        span.record("store.conn", "read");
        Held {
            conn: Some(conn.expect("a permit means an idle connection")),
            hold: Hold::Read(&self.reads, permit),
            since: Instant::now(),
        }
    }

    /// Where a space's index rows are: changes each time they do.
    pub fn generation(&self, space: &str) -> u64 {
        self.generations
            .lock()
            .expect("generations")
            .get(space)
            .copied()
            .unwrap_or(0)
    }

    fn changed(&self, space: &str) {
        *self
            .generations
            .lock()
            .expect("generations")
            .entry(space.to_string())
            .or_default() += 1;
    }

    /// A document's last saved snapshot.
    #[tracing::instrument(name = "drive.store.load", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn load(&self, id: &str) -> turso::Result<Option<Vec<u8>>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT snapshot FROM docs WHERE id = ?1",
                vec![Value::Text(id.into())],
            )
            .await?;
        match rows.next().await? {
            Some(row) => match row.get_value(0)? {
                Value::Blob(b) => Ok(Some(b)),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Replace a document's snapshot.
    #[tracing::instrument(name = "drive.store.save", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn save(&self, id: &str, snapshot: Vec<u8>) -> turso::Result<()> {
        let conn = self.writer().await;
        conn.execute(
            "INSERT OR REPLACE INTO docs (id, snapshot) VALUES (?1, ?2)",
            vec![Value::Text(id.into()), Value::Blob(snapshot)],
        )
        .await?;
        self.docs_changed();
        Ok(())
    }

    /// A write to `docs`: the kept listing is stale.
    fn docs_changed(&self) {
        let mut l = self.listing.lock().expect("listing");
        l.generation += 1;
        l.rows = None;
    }

    /// Give a document a new id (nothing happens if `to` already exists).
    pub async fn rename(&self, from: &str, to: &str) -> turso::Result<()> {
        let conn = self.writer().await;
        conn.execute(
            "UPDATE docs SET id = ?2 WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM docs WHERE id = ?2)",
            vec![Value::Text(from.into()), Value::Text(to.into())],
        )
        .await?;
        self.docs_changed();
        Ok(())
    }

    /// Move a space's index rows to a new space name.
    ///
    /// All or nothing. A location `to` already has rows for keeps them
    /// (they came in under the new name), and `to`'s totals are counted
    /// again from its rows, so a rename onto a space that has an index
    /// (or a rename that stopped half-way before) never collides.
    pub async fn rename_space(&self, from: &str, to: &str) -> turso::Result<()> {
        if from == to {
            return Ok(());
        }
        let conn = self.writer().await;
        conn.execute("BEGIN", ()).await?;
        self.tx_open.store(true, Ordering::Release);
        let names = || vec![Value::Text(from.into()), Value::Text(to.into())];
        let result = async {
            conn.execute(
                "DELETE FROM entries WHERE space = ?1 \
                 AND location IN (SELECT DISTINCT location FROM entries WHERE space = ?2)",
                names(),
            )
            .await?;
            conn.execute("UPDATE entries SET space = ?2 WHERE space = ?1", names())
                .await?;
            conn.execute("DELETE FROM entry_stats WHERE space IN (?1, ?2)", names())
                .await?;
            conn.execute(
                "INSERT INTO entry_stats (space, location, ext, files, bytes) \
                 SELECT space, location, ext, count(*), sum(size) FROM entries \
                 WHERE space = ?1 AND dir = 0 GROUP BY space, location, ext",
                vec![Value::Text(to.into())],
            )
            .await?;
            conn.execute("COMMIT", ()).await
        }
        .await;
        if result.is_err() {
            let _ = conn.execute("ROLLBACK", ()).await;
        }
        self.tx_open.store(false, Ordering::Release);
        result?;
        self.changed(from);
        self.changed(to);
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> turso::Result<()> {
        let conn = self.writer().await;
        conn.execute(
            "DELETE FROM docs WHERE id = ?1",
            vec![Value::Text(id.into())],
        )
        .await?;
        self.docs_changed();
        Ok(())
    }

    /// Every stored document id with its snapshot size.
    #[tracing::instrument(name = "drive.store.docs", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn list(&self) -> turso::Result<std::sync::Arc<Vec<(String, u64)>>> {
        let generation = {
            let l = self.listing.lock().expect("listing");
            if let Some((g, rows)) = &l.rows
                && *g == l.generation
            {
                return Ok(rows.clone());
            }
            l.generation
        };
        let rows = std::sync::Arc::new(self.list_now().await?);
        let mut l = self.listing.lock().expect("listing");
        // Kept only if nothing was written while it was read.
        if l.generation == generation {
            l.rows = Some((generation, rows.clone()));
        }
        Ok(rows)
    }

    async fn list_now(&self) -> turso::Result<Vec<(String, u64)>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query("SELECT id, length(snapshot) FROM docs", ())
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((
                text(row.get_value(0)?),
                int(row.get_value(1)?).max(0) as u64,
            ));
        }
        Ok(out)
    }

    /// Every stored document id (without reading the snapshots).
    #[tracing::instrument(name = "drive.store.ids", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn ids(&self) -> turso::Result<Vec<String>> {
        let conn = self.reader().await;
        let mut rows = conn.query("SELECT id FROM docs", ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(text(row.get_value(0)?));
        }
        Ok(out)
    }

    /// Replace a location's rows with a new index.
    #[tracing::instrument(name = "drive.store.put_index", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty, index.written = Empty))]
    pub async fn put_index(&self, space: &str, index: &LocationIndex) -> turso::Result<usize> {
        let conn = self.writer().await;
        // Defensive rollback in case a raw transaction was left open on the write connection.
        let _ = conn.execute("ROLLBACK", ()).await;
        conn.execute("BEGIN", ()).await?;
        self.tx_open.store(true, Ordering::Release);
        let result = async {
            conn.execute(
                "DELETE FROM entry_stats WHERE space = ?1 AND location = ?2",
                vec![Value::Text(space.into()), Value::Text(index.location.clone())],
            )
            .await?;
            let mut totals: HashMap<String, (u64, u64)> = HashMap::new();
            for e in index.entries.iter().filter(|e| !e.dir) {
                let t = totals.entry(ext_of(e.name())).or_default();
                t.0 += 1;
                t.1 += e.size;
            }
            // Only what changed is written: the rows already there are read
            // in path order beside the new entries, and each difference
            // becomes one delete, update or insert. A reindex that found
            // one new file wrote every row of the location again before
            // (millions, drive#47).
            let mut order: Vec<u32> = (0..index.entries.len() as u32).collect();
            order.sort_by(|a, b| {
                index.entries[*a as usize]
                    .path
                    .cmp(&index.entries[*b as usize].path)
            });
            let mut gone: Vec<i64> = Vec::new();
            let mut changed: Vec<(i64, u32)> = Vec::new();
            let mut added: Vec<u32> = Vec::new();
            {
                let mut rows = conn
                    .query(
                        "SELECT rowid, path, dir, size, mtime, sampled FROM entries \
                         WHERE space = ?1 AND location = ?2 ORDER BY path",
                        vec![Value::Text(space.into()), Value::Text(index.location.clone())],
                    )
                    .await?;
                let mut next = order.iter().copied().peekable();
                while let Some(row) = rows.next().await? {
                    let rowid = int(row.get_value(0)?);
                    let path = text(row.get_value(1)?);
                    while let Some(&i) = next.peek() {
                        if index.entries[i as usize].path.as_str() < path.as_str() {
                            added.push(i);
                            next.next();
                        } else {
                            break;
                        }
                    }
                    match next.peek() {
                        Some(&i) if index.entries[i as usize].path == path => {
                            next.next();
                            let e = &index.entries[i as usize];
                            let same = int(row.get_value(2)?) == e.dir as i64
                                && int(row.get_value(3)?) == e.size as i64
                                && int(row.get_value(4)?) == e.mtime
                                && text(row.get_value(5)?) == e.sampled;
                            if !same {
                                changed.push((rowid, i));
                            }
                        }
                        // In the table, no longer in the index (or a second
                        // row for one path: only the first is kept).
                        _ => gone.push(rowid),
                    }
                }
                added.extend(next);
            }
            for rowid in &gone {
                conn.execute("DELETE FROM entries WHERE rowid = ?1", vec![Value::Integer(*rowid)])
                    .await?;
            }
            let mut update = conn
                .prepare(
                    "UPDATE entries SET dir = ?2, size = ?3, mtime = ?4, sampled = ?5, ext = ?6 WHERE rowid = ?1",
                )
                .await?;
            for (rowid, i) in &changed {
                let e = &index.entries[*i as usize];
                let ext = if e.dir { String::new() } else { ext_of(e.name()) };
                update
                    .execute(vec![
                        Value::Integer(*rowid),
                        Value::Integer(e.dir as i64),
                        Value::Integer(e.size as i64),
                        Value::Integer(e.mtime),
                        Value::Text(e.sampled.clone()),
                        Value::Text(ext),
                    ])
                    .await?;
            }
            let mut stmt = conn
                .prepare(
                    "INSERT INTO entries (space, location, parent, path, name, lname, dir, size, mtime, sampled, ext) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .await?;
            for i in &added {
                let e = &index.entries[*i as usize];
                let ext = if e.dir { String::new() } else { ext_of(e.name()) };
                stmt.execute(vec![
                    Value::Text(space.into()),
                    Value::Text(index.location.clone()),
                    Value::Text(e.parent().into()),
                    Value::Text(e.path.clone()),
                    Value::Text(e.name().into()),
                    Value::Text(e.name().to_lowercase()),
                    Value::Integer(e.dir as i64),
                    Value::Integer(e.size as i64),
                    Value::Integer(e.mtime),
                    Value::Text(e.sampled.clone()),
                    Value::Text(ext),
                ])
                .await?;
            }
            tracing::Span::current().record("index.written", gone.len() + changed.len() + added.len());
            let mut stmt = conn
                .prepare(
                    "INSERT INTO entry_stats (space, location, ext, files, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .await?;
            for (ext, (files, bytes)) in totals {
                stmt.execute(vec![
                    Value::Text(space.into()),
                    Value::Text(index.location.clone()),
                    Value::Text(ext),
                    Value::Integer(files as i64),
                    Value::Integer(bytes as i64),
                ])
                .await?;
            }
            Ok::<_, turso::Error>(index.entries.len())
        }
        .await;
        match result {
            Ok(n) => {
                let committed = conn.execute("COMMIT", ()).await;
                if committed.is_ok() {
                    self.tx_open.store(false, Ordering::Release);
                    self.changed(space);
                    Ok(n)
                } else {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    self.tx_open.store(false, Ordering::Release);
                    Err(committed.unwrap_err())
                }
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                self.tx_open.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    #[tracing::instrument(name = "drive.store.drop_index", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn drop_index(&self, space: &str, location: &str) -> turso::Result<()> {
        let conn = self.writer().await;
        for table in ["entries", "entry_stats"] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE space = ?1 AND location = ?2"),
                vec![Value::Text(space.into()), Value::Text(location.into())],
            )
            .await?;
        }
        self.changed(space);
        Ok(())
    }

    /// A space's files and bytes, by extension and by location: read from
    /// the totals kept as indexes come in, not from the files.
    #[tracing::instrument(name = "drive.store.stats", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn stats(&self, space: &str) -> turso::Result<Stats> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, ext, files, bytes FROM entry_stats WHERE space = ?1",
                vec![Value::Text(space.into())],
            )
            .await?;
        let mut s = Stats::default();
        while let Some(row) = rows.next().await? {
            s.add(
                &text(row.get_value(0)?),
                &text(row.get_value(1)?),
                int(row.get_value(2)?).max(0) as u64,
                int(row.get_value(3)?).max(0) as u64,
            );
        }
        Ok(s)
    }

    fn found(row: &turso::Row) -> turso::Result<Found> {
        Ok(Found {
            location: text(row.get_value(0)?),
            path: text(row.get_value(1)?),
            name: text(row.get_value(2)?),
            dir: int(row.get_value(3)?) != 0,
            size: int(row.get_value(4)?),
            mtime: int(row.get_value(5)?),
            sampled: text(row.get_value(6)?),
        })
    }

    /// Files and folders whose name contains every word of `q`, and (when
    /// `exts` is not empty) files with one of those extensions.
    pub async fn search(
        &self,
        space: &str,
        q: &str,
        exts: &[String],
        limit: i64,
    ) -> turso::Result<Vec<Found>> {
        self.search_with(space, q, exts, &[], limit).await
    }

    /// As [`Store::search`], and (when `sampled` is not empty) only files
    /// with one of those contents (sampled hashes): every copy of the
    /// files a tag is on.
    #[tracing::instrument(name = "drive.store.search", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn search_with(
        &self,
        space: &str,
        q: &str,
        exts: &[String],
        sampled: &[String],
        limit: i64,
    ) -> turso::Result<Vec<Found>> {
        let mut sampled: Vec<&String> = sampled
            .iter()
            .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
            // Bounds one search's work: 40 statements at most.
            .take(20_000)
            .collect();
        sampled.sort_unstable();
        sampled.dedup();
        let words: Vec<String> = q
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .take(6)
            .collect();
        let exts: Vec<String> = exts
            .iter()
            .map(|e| {
                e.trim()
                    .trim_start_matches('.')
                    .to_lowercase()
                    .replace(['%', '_'], "")
            })
            .filter(|e| !e.is_empty())
            .take(64)
            .collect();
        if words.is_empty() && exts.is_empty() && sampled.is_empty() {
            return Ok(Vec::new());
        }
        // Files of some kinds, nothing else asked: one index range per kind.
        if words.is_empty() && sampled.is_empty() {
            let conn = self.reader().await;
            let mut out = Vec::new();
            for e in &exts {
                let mut rows = conn
                    .query(
                        "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                         WHERE ext = ?1 AND space = ?2 AND dir = 0",
                        vec![Value::Text(e.clone()), Value::Text(space.into())],
                    )
                    .await?;
                while let Some(row) = rows.next().await? {
                    out.push(Self::found(&row)?);
                }
            }
            out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
            out.truncate(limit.clamp(1, 1000) as usize);
            return Ok(out);
        }
        // A tag can be on thousands of contents: ask in bounded chunks
        // (each one statement's worth of parameters), newest first overall.
        const CHUNK: usize = 500;
        if sampled.len() <= CHUNK {
            return self
                .query_entries(space, &words, &exts, &sampled, limit)
                .await;
        }
        let mut out = Vec::new();
        for chunk in sampled.chunks(CHUNK) {
            out.extend(
                self.query_entries(space, &words, &exts, chunk, limit)
                    .await?,
            );
        }
        out.sort_by(|a, b| b.dir.cmp(&a.dir).then(b.mtime.cmp(&a.mtime)));
        out.truncate(limit.clamp(1, 1000) as usize);
        Ok(out)
    }

    /// One statement of [`Store::search_with`]: names holding every word,
    /// with one of `exts`, with one of the `sampled` contents.
    async fn query_entries(
        &self,
        space: &str,
        words: &[String],
        exts: &[String],
        sampled: &[&String],
        limit: i64,
    ) -> turso::Result<Vec<Found>> {
        let mut sql = String::from(
            "SELECT location, path, name, dir, size, mtime, sampled FROM entries WHERE space = ?1",
        );
        let mut params = vec![Value::Text(space.into())];
        for w in words {
            params.push(Value::Text(format!("%{}%", w.replace(['%', '_'], ""))));
            sql.push_str(&format!(" AND lname LIKE ?{}", params.len()));
        }
        if !exts.is_empty() {
            sql.push_str(" AND dir = 0 AND ext IN (");
            for (i, e) in exts.iter().enumerate() {
                params.push(Value::Text(e.clone()));
                sql.push_str(&format!(
                    "{}?{}",
                    if i > 0 { ", " } else { "" },
                    params.len()
                ));
            }
            sql.push(')');
        }
        if !sampled.is_empty() {
            sql.push_str(" AND dir = 0 AND sampled IN (");
            for (i, h) in sampled.iter().enumerate() {
                params.push(Value::Text((*h).clone()));
                sql.push_str(&format!(
                    "{}?{}",
                    if i > 0 { ", " } else { "" },
                    params.len()
                ));
            }
            sql.push(')');
        }
        params.push(Value::Integer(limit.clamp(1, 1000)));
        sql.push_str(&format!(
            " ORDER BY dir DESC, mtime DESC LIMIT ?{}",
            params.len()
        ));
        let conn = self.reader().await;
        let mut rows = conn.query(&sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::found(&row)?);
        }
        Ok(out)
    }

    /// One entry of a location's index.
    #[tracing::instrument(name = "drive.store.entry", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn entry(
        &self,
        space: &str,
        location: &str,
        path: &str,
    ) -> turso::Result<Option<Found>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                 WHERE space = ?1 AND location = ?2 AND path = ?3 LIMIT 1",
                vec![
                    Value::Text(space.into()),
                    Value::Text(location.into()),
                    Value::Text(path.into()),
                ],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::found(&row)?)),
            None => Ok(None),
        }
    }

    /// A folder's entries in a location's index (`""`: its root).
    #[tracing::instrument(name = "drive.store.list", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn list_dir(
        &self,
        space: &str,
        location: &str,
        dir: &str,
    ) -> turso::Result<Vec<Found>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                 WHERE space = ?1 AND location = ?2 AND parent = ?3 ORDER BY dir DESC, lname",
                vec![
                    Value::Text(space.into()),
                    Value::Text(location.into()),
                    Value::Text(dir.into()),
                ],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::found(&row)?);
        }
        Ok(out)
    }

    /// Every file of a location under a folder (`""`: all of it), by path.
    #[tracing::instrument(name = "drive.store.under", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn under(
        &self,
        space: &str,
        location: &str,
        prefix: &str,
    ) -> turso::Result<Vec<Found>> {
        let prefix = prefix.trim_matches('/');
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                 WHERE space = ?1 AND location = ?2 AND dir = 0 \
                 AND (?3 = '' OR path = ?3 OR substr(path, 1, length(?3) + 1) = ?3 || '/') ORDER BY path",
                vec![
                    Value::Text(space.into()),
                    Value::Text(location.into()),
                    Value::Text(prefix.into()),
                ],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::found(&row)?);
        }
        Ok(out)
    }

    /// Every indexed file in a space, for copies and protection.
    #[tracing::instrument(name = "drive.store.files", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty, store.rows = Empty))]
    pub async fn files(&self, space: &str) -> turso::Result<Vec<drive_files::protect::FileRow>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, path, size, sampled FROM entries WHERE space = ?1 AND dir = 0",
                vec![Value::Text(space.into())],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(drive_files::protect::FileRow {
                location: text(row.get_value(0)?),
                path: text(row.get_value(1)?),
                size: int(row.get_value(2)?).max(0) as u64,
                sampled: text(row.get_value(3)?),
            });
            // A whole space is a long read: let the worker's other tasks run.
            if out.len() % 4096 == 0 {
                tokio::task::yield_now().await;
            }
        }
        tracing::Span::current().record("store.rows", out.len());
        Ok(out)
    }

    /// Every location holding this content.
    /// How many locations of a space hold each `(sampled, size)` content:
    /// one query per few hundred contents instead of one per file. A
    /// content no location holds is not in the map.
    #[tracing::instrument(name = "drive.store.copy_counts", skip_all, fields(copies.asked = keys.len(), store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn copy_counts(
        &self,
        space: &str,
        keys: &[(String, i64)],
    ) -> turso::Result<HashMap<(String, i64), usize>> {
        let wanted: std::collections::HashSet<(&str, i64)> =
            keys.iter().map(|(s, n)| (s.as_str(), *n)).collect();
        let mut sampled: Vec<&str> = wanted.iter().map(|(s, _)| *s).collect();
        sampled.sort_unstable();
        sampled.dedup();
        let mut out = HashMap::new();
        let conn = self.reader().await;
        for chunk in sampled.chunks(500) {
            let marks: Vec<String> = (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
            let mut params = vec![Value::Text(space.into())];
            params.extend(chunk.iter().map(|s| Value::Text((*s).into())));
            let mut rows = conn
                .query(
                    &format!(
                        "SELECT sampled, size, count(DISTINCT location) FROM entries \
                         WHERE space = ?1 AND dir = 0 AND sampled IN ({}) GROUP BY sampled, size",
                        marks.join(", ")
                    ),
                    params,
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let s = text(row.get_value(0)?);
                let size = int(row.get_value(1)?);
                if wanted.contains(&(s.as_str(), size)) {
                    out.insert((s, size), int(row.get_value(2)?).max(0) as usize);
                }
            }
        }
        Ok(out)
    }

    #[tracing::instrument(name = "drive.store.copies", skip_all, fields(store.conn = Empty, store.wait_ms = Empty, store.query_ms = Empty))]
    pub async fn copies(&self, space: &str, sampled: &str, size: i64) -> turso::Result<Vec<Found>> {
        let conn = self.reader().await;
        let mut rows = conn
            .query(
                "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                 WHERE space = ?1 AND sampled = ?2 AND size = ?3 AND dir = 0",
                vec![
                    Value::Text(space.into()),
                    Value::Text(sampled.into()),
                    Value::Integer(size),
                ],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::found(&row)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_files::index::IndexEntry;
    use std::sync::Arc;

    fn file(path: &str, sampled: &str) -> IndexEntry {
        IndexEntry {
            path: path.into(),
            dir: false,
            size: 10,
            mtime: 1,
            sampled: sampled.into(),
        }
    }

    #[tokio::test]
    async fn copy_counts_match_asking_for_each_files_copies() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        let entries = |n: usize| -> Vec<IndexEntry> {
            (0..n)
                .map(|i| file(&format!("f{i}.wav"), &format!("h{i}")))
                .collect()
        };
        // h0..h599 in l1, h0..h299 in l2 too (twice in l2: one location),
        // and h0 again with another size (another content).
        store
            .put_index("home", &index("l1", entries(600)))
            .await
            .unwrap();
        let mut l2 = entries(300);
        l2.push(file("dup/f0.wav", "h0"));
        l2.push(sized("other.wav", 99));
        l2.last_mut().unwrap().sampled = "h0".into();
        store.put_index("home", &index("l2", l2)).await.unwrap();
        store
            .put_index("work", &index("l3", entries(5)))
            .await
            .unwrap();

        let mut keys: Vec<(String, i64)> = (0..650).map(|i| (format!("h{i}"), 10)).collect();
        keys.push(("h0".into(), 99));
        keys.push(("h0".into(), 10)); // asked twice
        let counts = store.copy_counts("home", &keys).await.unwrap();
        for (sampled, size) in &keys {
            let mut locs: Vec<String> = store
                .copies("home", sampled, *size)
                .await
                .unwrap()
                .into_iter()
                .map(|f| f.location)
                .collect();
            locs.sort();
            locs.dedup();
            let got = counts.get(&(sampled.clone(), *size)).copied().unwrap_or(0);
            assert_eq!(got, locs.len(), "{sampled} {size}");
        }
        assert_eq!(counts[&("h0".to_string(), 10)], 2);
        assert_eq!(counts[&("h0".to_string(), 99)], 1);
        assert_eq!(counts[&("h400".to_string(), 10)], 1);
        assert!(!counts.contains_key(&("h640".to_string(), 10)));
    }

    #[tokio::test]
    async fn an_abandoned_transaction_does_not_block_later_indexes() {
        let dir = std::env::temp_dir().join(format!("drive-store-tx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("node.db")).await.unwrap();
        let index = |loc: &str| LocationIndex {
            location: loc.into(),
            root: String::new(),
            built: 1,
            entries: vec![file("a.wav", "aa11")],
        };
        store.put_index("home", &index("l1")).await.unwrap();
        // A cut-off request left this open.
        store.write.lock().await.execute("BEGIN", ()).await.unwrap();
        assert_eq!(store.put_index("home", &index("l2")).await.unwrap(), 1);
        let found = store.search_with("home", "a", &[], &[], 10).await.unwrap();
        assert_eq!(found.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_by_kind_reads_the_extension() {
        let dir = std::env::temp_dir().join(format!("drive-store-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("node.db")).await.unwrap();
        let mut entries = vec![
            file("Set/JT's Recital.SETLIST", "a1"),
            file("Charts/Know Me.kf", "b2"),
            file("Charts/kf-notes.txt", "c3"),
            file("Makefile", "d4"),
        ];
        entries.push(IndexEntry {
            dir: true,
            ..file("Old.setlist", "")
        });
        store
            .put_index(
                "home",
                &LocationIndex {
                    location: "l1".into(),
                    root: String::new(),
                    built: 1,
                    entries,
                },
            )
            .await
            .unwrap();
        let names = |found: Vec<Found>| {
            let mut v: Vec<String> = found.into_iter().map(|f| f.path).collect();
            v.sort();
            v
        };
        let found = store
            .search_with("home", "", &[".setlist".into(), "KF".into()], &[], 10)
            .await
            .unwrap();
        assert_eq!(
            names(found),
            vec!["Charts/Know Me.kf", "Set/JT's Recital.SETLIST"]
        );
        // A database from before the column: filled in when it opens.
        store
            .write
            .lock()
            .await
            .execute("UPDATE entries SET ext = ''", ())
            .await
            .unwrap();
        drop(store);
        let store = Store::open(&dir.join("node.db")).await.unwrap();
        let found = store
            .search_with("home", "", &["kf".into()], &[], 10)
            .await
            .unwrap();
        assert_eq!(names(found), vec!["Charts/Know Me.kf"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn index(location: &str, entries: Vec<IndexEntry>) -> LocationIndex {
        LocationIndex {
            location: location.into(),
            root: String::new(),
            built: 1,
            entries,
        }
    }

    fn sized(path: &str, size: u64) -> IndexEntry {
        IndexEntry {
            size,
            ..file(path, "")
        }
    }

    /// The kept totals against counting every file, as `/stats` did.
    async fn assert_stats_match(store: &Store, space: &str) {
        let slow = Stats::of_rows(&store.files(space).await.unwrap());
        assert_eq!(store.stats(space).await.unwrap(), slow, "space {space}");
    }

    #[tokio::test]
    async fn kept_totals_match_counting_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        let folder = IndexEntry {
            dir: true,
            ..sized("Song", 0)
        };
        store
            .put_index(
                "home",
                &index(
                    "l1",
                    vec![
                        folder.clone(),
                        sized("Song/a.WAV", 100),
                        sized("Song/b.wav", 50),
                        sized("Song/chart.kf", 7),
                        sized("Makefile", 3),
                        sized("Song/.hidden", 1),
                    ],
                ),
            )
            .await
            .unwrap();
        store
            .put_index(
                "home",
                &index("l2", vec![sized("a.wav", 100), sized("x.pdf", 9)]),
            )
            .await
            .unwrap();
        store
            .put_index("work", &index("l3", vec![sized("w.wav", 1)]))
            .await
            .unwrap();
        assert_stats_match(&store, "home").await;
        assert_stats_match(&store, "work").await;
        let home = store.stats("home").await.unwrap();
        assert_eq!((home.files, home.bytes), (7, 270));
        assert_eq!(home.exts["wav"], (3, 250));
        assert_eq!(home.exts[""], (1, 3));
        assert_eq!(home.locations["l2"], (2, 109));

        // A location's new index replaces its totals; a dropped one goes.
        store
            .put_index("home", &index("l1", vec![sized("only.flac", 5)]))
            .await
            .unwrap();
        assert_stats_match(&store, "home").await;
        store.drop_index("home", "l2").await.unwrap();
        assert_stats_match(&store, "home").await;
        assert_eq!(store.stats("home").await.unwrap().files, 1);

        // A renamed space takes its totals along.
        store.rename_space("work", "studio").await.unwrap();
        assert_stats_match(&store, "studio").await;
        assert_eq!(store.stats("work").await.unwrap(), Stats::default());
        assert_eq!(store.stats("studio").await.unwrap().files, 1);
    }

    #[tokio::test]
    async fn renaming_onto_a_space_with_rows_keeps_its_index_and_totals_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        // Both names have a location with `.wav` files: the same totals key.
        store
            .put_index("work", &index("l1", vec![sized("a.wav", 10)]))
            .await
            .unwrap();
        store
            .put_index("work", &index("l2", vec![sized("b.wav", 3)]))
            .await
            .unwrap();
        store
            .put_index(
                "studio",
                &index("l1", vec![sized("c.wav", 7), sized("d.kf", 1)]),
            )
            .await
            .unwrap();

        store.rename_space("work", "studio").await.unwrap();

        assert_stats_match(&store, "studio").await;
        assert_eq!(store.stats("work").await.unwrap(), Stats::default());
        assert!(store.files("work").await.unwrap().is_empty());
        // The location both had keeps the index it has under the new name.
        let studio = store.stats("studio").await.unwrap();
        assert_eq!(studio.locations["l1"], (2, 8));
        assert_eq!(studio.locations["l2"], (1, 3));

        // Renaming a space to its own name leaves it as it is.
        store.rename_space("studio", "studio").await.unwrap();
        assert_eq!(store.stats("studio").await.unwrap(), studio);
    }

    #[tokio::test]
    async fn a_database_from_before_the_totals_counts_them_when_it_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.db");
        let store = Store::open(&path).await.unwrap();
        for (space, loc) in [("home", "l1"), ("home", "l2"), ("work", "l3")] {
            store
                .put_index(
                    space,
                    &index(
                        loc,
                        vec![sized("a.wav", 10), sized("b.kf", 2), sized("c", 1)],
                    ),
                )
                .await
                .unwrap();
        }
        // As an older node left it: rows, no totals.
        store
            .write
            .lock()
            .await
            .execute("DROP TABLE entry_stats", ())
            .await
            .unwrap();
        drop(store);
        let store = Store::open(&path).await.unwrap();
        assert_stats_match(&store, "home").await;
        assert_stats_match(&store, "work").await;
        assert_eq!(store.stats("home").await.unwrap().files, 6);
    }

    #[tokio::test]
    async fn index_writes_move_the_generation_and_reads_see_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        let g0 = store.generation("home");
        store
            .put_index("home", &index("l1", vec![sized("a.wav", 1)]))
            .await
            .unwrap();
        let g1 = store.generation("home");
        assert_ne!(g0, g1);
        assert_eq!(store.generation("work"), 0);
        // Written on the write connection, read on a read one.
        assert_eq!(store.files("home").await.unwrap().len(), 1);
        store.drop_index("home", "l1").await.unwrap();
        assert_ne!(store.generation("home"), g1);
        assert!(store.files("home").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_long_read_does_not_hold_up_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("node.db")).await.unwrap());
        store.save("org/drive:space", vec![1, 2, 3]).await.unwrap();
        // Every read connection but one is held, and the writer too.
        let mut held = Vec::new();
        for _ in 0..READERS - 1 {
            held.push(store.reader().await);
        }
        let writer = store.writer().await;
        let quick = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            store.ids().await.unwrap()
        })
        .await
        .expect("a read with a connection free does not wait");
        assert_eq!(quick, ["org/drive:space"]);
        drop(writer);
        drop(held);
    }

    #[tokio::test]
    async fn search_by_content_finds_every_copy() {
        let dir = std::env::temp_dir().join(format!("drive-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("node.db")).await.unwrap();
        for (loc, path) in [("l1", "Song/a.wav"), ("l2", "Copies/renamed.wav")] {
            store
                .put_index(
                    "home",
                    &LocationIndex {
                        location: loc.into(),
                        root: String::new(),
                        built: 1,
                        entries: vec![file(path, "aa11"), file("other.wav", "bb22")],
                    },
                )
                .await
                .unwrap();
        }
        let found = store
            .search_with("home", "", &[], &["aa11".into(), "not hex!".into()], 100)
            .await
            .unwrap();
        let mut paths: Vec<String> = found.into_iter().map(|f| f.path).collect();
        paths.sort();
        assert_eq!(paths, ["Copies/renamed.wav", "Song/a.wav"]);
        // Words and contents together.
        let found = store
            .search_with("home", "renamed", &[], &["aa11".into()], 100)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_by_content_takes_every_hash_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        let hashes: Vec<String> = (0..1200).map(|i| format!("{i:064x}")).collect();
        store
            .put_index(
                "home",
                &LocationIndex {
                    location: "l1".into(),
                    root: String::new(),
                    built: 1,
                    entries: hashes
                        .iter()
                        .enumerate()
                        .map(|(i, h)| IndexEntry {
                            mtime: i as i64,
                            ..file(&format!("f{i}.wav"), h)
                        })
                        .collect(),
                },
            )
            .await
            .unwrap();
        let found = store
            .search_with("home", "", &[], &hashes, 1000)
            .await
            .unwrap();
        assert_eq!(found.len(), 1000);
        // Newest first across every chunk of hashes.
        assert_eq!(found[0].path, "f1199.wav");
        assert_eq!(found[999].path, "f200.wav");
        let found = store
            .search_with("home", "f7", &[], &hashes, 1000)
            .await
            .unwrap();
        assert_eq!(found.len(), 111); // f7, f70..f79, f700..f799
    }

    #[tokio::test]
    async fn abandoned_transaction_does_not_roll_back_later_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.db");
        let store = Store::open(&path).await.unwrap();
        // Simulate an aborted put_index leaving a BEGIN open on the writer connection.
        {
            let conn = store.write.lock().await;
            conn.execute("BEGIN", ()).await.unwrap();
            store.tx_open.store(true, Ordering::Release);
        }
        // The next save must roll back the abandoned transaction first so it commits on its own.
        store.save("org/doc", vec![1, 2, 3, 4]).await.unwrap();
        drop(store);

        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            reopened.load("org/doc").await.unwrap(),
            Some(vec![1, 2, 3, 4])
        );
    }

    #[tokio::test]
    async fn under_matches_single_file_part_and_folder_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("node.db")).await.unwrap();
        store
            .put_index(
                "church",
                &index(
                    "l1",
                    vec![
                        sized("Charts/a.pdf", 10),
                        sized("Charts/sub/b.pdf", 20),
                        sized("Charts2/c.pdf", 30),
                    ],
                ),
            )
            .await
            .unwrap();
        let single: Vec<String> = store
            .under("church", "l1", "Charts/a.pdf")
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(single, ["Charts/a.pdf"]);

        let folder: Vec<String> = store
            .under("church", "l1", "Charts")
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(folder, ["Charts/a.pdf", "Charts/sub/b.pdf"]);
    }

    #[tokio::test]
    async fn the_listing_is_kept_until_a_document_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("drive.db")).await.unwrap();
        store.save("me/a", vec![1, 2, 3]).await.unwrap();
        let first = store.list().await.unwrap();
        assert_eq!(*first, [("me/a".to_string(), 3)]);
        assert!(
            std::sync::Arc::ptr_eq(&first, &store.list().await.unwrap()),
            "nothing changed: the same listing, not another scan"
        );
        store.save("me/b", vec![9]).await.unwrap();
        let ids = |l: &[(String, u64)]| l.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>();
        let mut after_save = ids(&store.list().await.unwrap());
        after_save.sort();
        assert_eq!(after_save, ["me/a", "me/b"]);
        store.rename("me/a", "me/c").await.unwrap();
        let mut after_rename = ids(&store.list().await.unwrap());
        after_rename.sort();
        assert_eq!(after_rename, ["me/b", "me/c"]);
        store.delete("me/b").await.unwrap();
        assert_eq!(ids(&store.list().await.unwrap()), ["me/c"]);
    }


    async fn location_rows(store: &Store) -> Vec<(i64, String, i64, String)> {
        let conn = store.reader().await;
        let mut r = conn
            .query(
                "SELECT rowid, path, size, ext FROM entries WHERE space = 'me' AND location = 'l' ORDER BY path",
                (),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(row) = r.next().await.unwrap() {
            out.push((
                int(row.get_value(0).unwrap()),
                text(row.get_value(1).unwrap()),
                int(row.get_value(2).unwrap()),
                text(row.get_value(3).unwrap()),
            ));
        }
        out
    }

    #[tokio::test]
    async fn a_new_index_writes_only_what_changed() {
        use drive_files::index::{IndexEntry, LocationIndex};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("drive.db")).await.unwrap();
        let e = |path: &str, size: u64| IndexEntry {
            path: path.into(),
            dir: false,
            size,
            mtime: 1,
            sampled: format!("{size:064x}"),
        };
        let idx = |entries| LocationIndex { location: "l".into(), root: "/".into(), built: 0, entries };
        store.put_index("me", &idx(vec![e("a.wav", 1), e("b.wav", 2), e("c.wav", 3)])).await.unwrap();
        let before = location_rows(&store).await;
        // b changes, c goes, d comes; out of order on purpose.
        store.put_index("me", &idx(vec![e("d.mp3", 4), e("b.wav", 20), e("a.wav", 1)])).await.unwrap();
        let after = location_rows(&store).await;
        let paths: Vec<(&str, i64, &str)> = after.iter().map(|r| (r.1.as_str(), r.2, r.3.as_str())).collect();
        assert_eq!(paths, [("a.wav", 1, "wav"), ("b.wav", 20, "wav"), ("d.mp3", 4, "mp3")]);
        assert_eq!(after[0].0, before[0].0, "an unchanged row is left as it was");
        assert_eq!(after[1].0, before[1].0, "a changed row is updated in place");
        // The totals follow the new index.
        let conn = store.reader().await;
        let mut r = conn
            .query("SELECT ext, files, bytes FROM entry_stats WHERE space = 'me' AND location = 'l' ORDER BY ext", ())
            .await
            .unwrap();
        let mut stats = Vec::new();
        while let Some(row) = r.next().await.unwrap() {
            stats.push((text(row.get_value(0).unwrap()), int(row.get_value(1).unwrap()), int(row.get_value(2).unwrap())));
        }
        assert_eq!(stats, [("mp3".to_string(), 1, 4), ("wav".to_string(), 2, 21)]);
    }


    #[tokio::test]
    #[ignore = "timing, by hand"]
    async fn timing_a_big_reindex() {
        use drive_files::index::{IndexEntry, LocationIndex};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("drive.db")).await.unwrap();
        let n = 300_000;
        let mut entries: Vec<IndexEntry> = (0..n)
            .map(|i| IndexEntry { path: format!("Pack {:04}/Sample {i:07}.wav", i / 40), dir: false, size: i as u64, mtime: 1, sampled: format!("{i:064x}") })
            .collect();
        let idx = |entries: Vec<IndexEntry>| LocationIndex { location: "l".into(), root: "/".into(), built: 0, entries };
        let t = std::time::Instant::now();
        store.put_index("me", &idx(entries.clone())).await.unwrap();
        eprintln!("first: {:?}", t.elapsed());
        entries[1234].size += 1;
        entries.push(IndexEntry { path: "Pack 9999/new.wav".into(), dir: false, size: 1, mtime: 1, sampled: "a".repeat(64) });
        let t = std::time::Instant::now();
        store.put_index("me", &idx(entries)).await.unwrap();
        eprintln!("one change: {:?}", t.elapsed());
    }

}
