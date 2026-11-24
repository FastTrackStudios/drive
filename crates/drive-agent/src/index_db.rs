//! Location indexes on disk (drive#47): every location this device knows,
//! its own and those downloaded from other devices, as rows of one turso
//! database in the agent's folder (`indexes.db`). Nothing keeps a whole
//! index in memory. A folder is listed, a path looked up and a content's
//! copies found by indexed queries, and a whole location is read a page
//! at a time.
//!
//! A new index is written as its differences from the rows already there
//! (as the node does): a reindex that found one new file writes one row.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use drive_files::index::{IndexEntry, LocationIndex};
use tokio::sync::{Mutex, Semaphore};
use turso::Value;

/// Read connections kept open.
const READERS: usize = 4;

/// Rows written in one transaction: well inside what the multi-process
/// log can hold (64 x 4096 pages, about 1 GB) whatever the rows are.
const BATCH: usize = 10_000;

/// What the database knows of a location's index as a whole.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub root: String,
    /// When the index was built (ms since the epoch).
    pub built: i64,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

pub struct IndexDb {
    write: Mutex<turso::Connection>,
    /// A write transaction was left open by a cancelled `put`: the next
    /// writer rolls it back first.
    tx_open: AtomicBool,
    readers: std::sync::Mutex<Vec<turso::Connection>>,
    permits: Semaphore,
}

/// A read connection, back in the pool when dropped.
struct Reader<'a> {
    db: &'a IndexDb,
    conn: Option<turso::Connection>,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = turso::Connection;
    fn deref(&self) -> &turso::Connection {
        self.conn.as_ref().expect("held")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.db.readers.lock().expect("readers").push(c);
        }
    }
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

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// The columns an entry is read from, in order.
const ENTRY: &str = "path, dir, size, mtime, sampled";

fn entry(row: &turso::Row, at: usize) -> turso::Result<IndexEntry> {
    Ok(IndexEntry {
        path: text(row.get_value(at)?),
        dir: int(row.get_value(at + 1)?) != 0,
        size: int(row.get_value(at + 2)?).max(0) as u64,
        mtime: int(row.get_value(at + 3)?),
        sampled: text(row.get_value(at + 4)?),
    })
}

impl IndexDb {
    /// Open (or make) the database at `path`, shared with the agent's other
    /// processes (the daemon and a `drive` command run beside it).
    ///
    /// Everything in it can be built again (a walk, a download from the
    /// node), so a file that will not open after a few tries is set aside
    /// and a fresh one made: a missing index is rebuilt, never fatal.
    pub async fn open(path: &Path) -> turso::Result<IndexDb> {
        let mut last = None;
        for attempt in 0..5u64 {
            match Self::open_once(path).await {
                Ok(db) => return Ok(db),
                // Two processes creating it at the same moment race on its
                // coordination file: the loser opens it a moment later.
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
                }
            }
        }
        let aside = path.with_extension(format!("db.broken-{}", drive_files::now_ms()));
        tracing::warn!(
            path = %path.display(),
            error = %last.map(|e| e.to_string()).unwrap_or_default(),
            "the index database would not open: set aside, starting a new one"
        );
        for suffix in ["", "-wal", "-shm", "-tshm"] {
            let from = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            let to = std::path::PathBuf::from(format!("{}{suffix}", aside.display()));
            let _ = std::fs::rename(from, to);
        }
        Self::open_once(path).await
    }

    async fn open_once(path: &Path) -> turso::Result<IndexDb> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .experimental_multiprocess_wal(true)
            .build()
            .await?;
        let conn = db.connect()?;
        for sql in [
            "PRAGMA busy_timeout = 5000",
            "CREATE TABLE IF NOT EXISTS locations (location TEXT PRIMARY KEY, root TEXT NOT NULL, \
             built INTEGER NOT NULL, files INTEGER NOT NULL, dirs INTEGER NOT NULL, bytes INTEGER NOT NULL)",
            "CREATE TABLE IF NOT EXISTS entries (location TEXT NOT NULL, path TEXT NOT NULL, \
             parent TEXT NOT NULL, lname TEXT NOT NULL, dir INTEGER NOT NULL, size INTEGER NOT NULL, \
             mtime INTEGER NOT NULL, sampled TEXT NOT NULL)",
            "CREATE UNIQUE INDEX IF NOT EXISTS entries_path ON entries (location, path)",
            "CREATE INDEX IF NOT EXISTS entries_parent ON entries (location, parent)",
            "CREATE INDEX IF NOT EXISTS entries_content ON entries (sampled, size)",
        ] {
            conn.execute(sql, ()).await?;
        }
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let r = db.connect()?;
            r.execute("PRAGMA busy_timeout = 5000", ()).await?;
            readers.push(r);
        }
        Ok(IndexDb {
            write: Mutex::new(conn),
            tx_open: AtomicBool::new(false),
            readers: std::sync::Mutex::new(readers),
            permits: Semaphore::new(READERS),
        })
    }

    async fn reader(&self) -> Reader<'_> {
        let permit = self.permits.acquire().await.expect("never closed");
        let conn = self.readers.lock().expect("readers").pop();
        Reader {
            db: self,
            conn,
            _permit: permit,
        }
    }

    /// Keep `index` as its location's: only the rows that differ are
    /// written. Returns how many were (deleted, updated and inserted).
    ///
    /// The differences go in [`BATCH`] rows a transaction, the log folded
    /// into the database after each: in the multi-process mode the log
    /// holds at most about 1 GB of pages, and one transaction larger than
    /// that (a first index of a million files) can never commit. The
    /// location's totals go last, so until they do it reads as the index
    /// it was, only partly replaced, and the next `put` finishes the work.
    pub async fn put(&self, index: &LocationIndex) -> turso::Result<usize> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        let ops = diff_rows(&conn, index).await?;
        for batch in ops.chunks(BATCH) {
            self.in_tx(&conn, apply(&conn, index.location.as_str(), batch)).await?;
            checkpoint(&conn).await;
        }
        self.in_tx(&conn, put_meta(&conn, index)).await?;
        if !ops.is_empty() {
            checkpoint(&conn).await;
        }
        Ok(ops.len())
    }

    /// Forget a location's index ([`BATCH`] rows a transaction, as `put`).
    pub async fn remove(&self, location: &str) -> turso::Result<()> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        conn.execute(
            "DELETE FROM locations WHERE location = ?1",
            vec![t(location)],
        )
        .await?;
        loop {
            let gone = conn
                .execute(
                    &format!(
                        "DELETE FROM entries WHERE rowid IN \
                         (SELECT rowid FROM entries WHERE location = ?1 LIMIT {BATCH})"
                    ),
                    vec![t(location)],
                )
                .await?;
            checkpoint(&conn).await;
            if gone == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Roll back a write transaction a cancelled writer left open.
    async fn abandon(&self, conn: &turso::Connection) {
        if self.tx_open.swap(false, Ordering::AcqRel) {
            let _ = conn.execute("ROLLBACK", ()).await;
        }
    }

    /// Run `writes` in one transaction: committed if they all succeed,
    /// rolled back otherwise.
    async fn in_tx(
        &self,
        conn: &turso::Connection,
        writes: impl std::future::Future<Output = turso::Result<()>>,
    ) -> turso::Result<()> {
        conn.execute("BEGIN", ()).await?;
        self.tx_open.store(true, Ordering::Release);
        let done = match writes.await {
            Ok(()) => conn.execute("COMMIT", ()).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if done.is_err() {
            let _ = conn.execute("ROLLBACK", ()).await;
        }
        self.tx_open.store(false, Ordering::Release);
        done
    }

    pub async fn meta(&self, location: &str) -> turso::Result<Option<Meta>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                "SELECT root, built, files, dirs, bytes FROM locations WHERE location = ?1",
                vec![t(location)],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(Meta {
            root: text(row.get_value(0)?),
            built: int(row.get_value(1)?),
            files: int(row.get_value(2)?).max(0) as u64,
            dirs: int(row.get_value(3)?).max(0) as u64,
            bytes: int(row.get_value(4)?).max(0) as u64,
        }))
    }

    /// Every location with an index here.
    pub async fn locations(&self) -> turso::Result<Vec<String>> {
        let r = self.reader().await;
        let mut rows = r
            .query("SELECT location FROM locations ORDER BY location", ())
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(text(row.get_value(0)?));
        }
        Ok(out)
    }

    pub async fn get(&self, location: &str, path: &str) -> turso::Result<Option<IndexEntry>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                &format!("SELECT {ENTRY} FROM entries WHERE location = ?1 AND path = ?2"),
                vec![t(location), t(path)],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(entry(&row, 0)?)),
            None => Ok(None),
        }
    }

    /// A folder's entries (`""`: the location's root), folders first, by
    /// name.
    pub async fn list(&self, location: &str, dir: &str) -> turso::Result<Vec<IndexEntry>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                &format!(
                    "SELECT {ENTRY} FROM entries WHERE location = ?1 AND parent = ?2 \
                     ORDER BY dir DESC, lname, path"
                ),
                vec![t(location), t(dir)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(entry(&row, 0)?);
        }
        Ok(out)
    }

    /// Up to `limit` entries of a location in path order, after `after`
    /// (`""`: from the start). Read a whole location a page at a time by
    /// passing the last path of each page as the next `after`.
    pub async fn page(
        &self,
        location: &str,
        after: &str,
        limit: usize,
    ) -> turso::Result<Vec<IndexEntry>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                &format!(
                    "SELECT {ENTRY} FROM entries WHERE location = ?1 AND path > ?2 \
                     ORDER BY path LIMIT ?3"
                ),
                vec![t(location), t(after), Value::Integer(limit as i64)],
            )
            .await?;
        let mut out = Vec::with_capacity(limit.min(4096));
        while let Some(row) = rows.next().await? {
            out.push(entry(&row, 0)?);
        }
        Ok(out)
    }

    /// [`IndexDb::page`] of only what is under `dir` (`""`: everything).
    pub async fn page_under(
        &self,
        location: &str,
        dir: &str,
        after: &str,
        limit: usize,
    ) -> turso::Result<Vec<IndexEntry>> {
        if dir.is_empty() {
            return self.page(location, after, limit).await;
        }
        // Everything under `dir/` sorts at or after `dir/` and before
        // `dir0` ('0' is the byte after '/').
        let low = format!("{dir}/");
        let after = if after < low.as_str() { low.as_str() } else { after };
        let r = self.reader().await;
        let mut rows = r
            .query(
                &format!(
                    "SELECT {ENTRY} FROM entries WHERE location = ?1 AND path > ?2 AND path < ?3 \
                     ORDER BY path LIMIT ?4"
                ),
                vec![t(location), t(after), t(&format!("{dir}0")), Value::Integer(limit as i64)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(entry(&row, 0)?);
        }
        Ok(out)
    }

    /// Which locations hold each of `sampled` (one per file holding it, so
    /// a location twice when it has two copies), in as few queries as the
    /// list's length allows.
    pub async fn locations_of(&self, sampled: &[String]) -> turso::Result<HashMap<String, Vec<String>>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let r = self.reader().await;
        for chunk in sampled.chunks(500) {
            let marks = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
            let mut rows = r
                .query(
                    &format!("SELECT sampled, location FROM entries WHERE dir = 0 AND sampled IN ({marks})"),
                    chunk.iter().map(|h| t(h)).collect::<Vec<_>>(),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                out.entry(text(row.get_value(0)?)).or_default().push(text(row.get_value(1)?));
            }
        }
        Ok(out)
    }

    /// Contents found more than once (by sampled hash and size), files of at
    /// least `min_size` bytes: (sampled, size, how many).
    pub async fn duplicates(&self, min_size: u64) -> turso::Result<Vec<(String, u64, u64)>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                "SELECT sampled, size, count(*) FROM entries WHERE dir = 0 AND sampled != '' \
                 AND size >= ?1 GROUP BY sampled, size HAVING count(*) > 1",
                vec![Value::Integer(min_size as i64)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((
                text(row.get_value(0)?),
                int(row.get_value(1)?).max(0) as u64,
                int(row.get_value(2)?).max(0) as u64,
            ));
        }
        Ok(out)
    }

    /// Where a content is (a file's sampled hash, and its size if known):
    /// (location, entry), in every location.
    pub async fn copies(
        &self,
        sampled: &str,
        size: Option<u64>,
    ) -> turso::Result<Vec<(String, IndexEntry)>> {
        if sampled.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.reader().await;
        let (sql, args) = match size {
            Some(n) => (
                format!(
                    "SELECT location, {ENTRY} FROM entries WHERE sampled = ?1 AND size = ?2 AND dir = 0"
                ),
                vec![t(sampled), Value::Integer(n as i64)],
            ),
            None => (
                format!(
                    "SELECT location, {ENTRY} FROM entries WHERE sampled = ?1 AND dir = 0"
                ),
                vec![t(sampled)],
            ),
        };
        // Sorted here: an ORDER BY made turso scan the whole table instead
        // of using `entries_content` (95 ms at 500k rows, not 0.1).
        let mut rows = r.query(&sql, args).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((text(row.get_value(0)?), entry(&row, 1)?));
        }
        out.sort_by(|a, b| (&a.0, &a.1.path).cmp(&(&b.0, &b.1.path)));
        Ok(out)
    }

    /// A location's whole index, read back (for what still needs it whole:
    /// publishing it, until that streams too).
    pub async fn load(&self, location: &str) -> turso::Result<Option<LocationIndex>> {
        let Some(meta) = self.meta(location).await? else {
            return Ok(None);
        };
        let mut entries = Vec::with_capacity((meta.files + meta.dirs) as usize);
        let mut after = String::new();
        loop {
            let page = self.page(location, &after, 10_000).await?;
            let Some(last) = page.last() else { break };
            after = last.path.clone();
            entries.extend(page);
        }
        Ok(Some(LocationIndex {
            location: location.to_string(),
            root: meta.root,
            built: meta.built,
            entries,
        }))
    }
}

/// A location's last walk, from the database a folder at a time: what
/// [`drive_files::index::walk_from`] asks on its threads (blocking on the
/// agent's runtime, which those threads are not part of).
pub struct Previous {
    pub db: std::sync::Arc<IndexDb>,
    pub location: String,
    pub rt: tokio::runtime::Handle,
}

impl drive_files::index::Previous for Previous {
    fn folder(&self, dir: &str) -> Vec<IndexEntry> {
        self.rt
            .block_on(self.db.list(&self.location, dir))
            .unwrap_or_default()
    }
}

/// A location's entries a page at a time (in path order, under a folder
/// or all of it): `while let Some(page) = pages.next().await { .. }`.
pub struct Pages {
    db: std::sync::Arc<IndexDb>,
    location: String,
    under: String,
    after: String,
    done: bool,
}

/// Entries a page holds.
const PAGE: usize = 2000;

impl Pages {
    pub fn new(db: std::sync::Arc<IndexDb>, location: &str, under: &str) -> Pages {
        Pages {
            db,
            location: location.to_string(),
            under: under.trim_matches('/').to_string(),
            after: String::new(),
            done: false,
        }
    }

    /// The next page; `None` once there are no more (or the database
    /// cannot be read: a scan then sees what it got so far).
    pub async fn next(&mut self) -> Option<Vec<IndexEntry>> {
        if self.done {
            return None;
        }
        let page = match self
            .db
            .page_under(&self.location, &self.under, &self.after, PAGE)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(location.id = %self.location, error = %e, "an index page could not be read");
                Vec::new()
            }
        };
        match page.last() {
            Some(last) => self.after = last.path.clone(),
            None => {
                self.done = true;
                return None;
            }
        }
        if page.len() < PAGE {
            self.done = true;
        }
        Some(page)
    }
}

/// Fold the write-ahead log into the database and empty it. Without this
/// nothing did, in the multi-process mode: indexing 208k files left an
/// 852 MB log beside a 134 MB database. A reader in the way (this process
/// or another) is waited for a moment; past that it is left for the next
/// write.
async fn checkpoint(conn: &turso::Connection) {
    for attempt in 0..20u64 {
        match conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await {
            Ok(mut rows) => {
                // (busy, log frames, frames checkpointed)
                let busy = match rows.next().await {
                    Ok(Some(row)) => matches!(row.get_value(0), Ok(Value::Integer(b)) if b != 0),
                    _ => false,
                };
                while let Ok(Some(_)) = rows.next().await {}
                if !busy {
                    return;
                }
            }
            Err(e) => tracing::debug!(error = %e, "the index database's log was not checkpointed"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(10 * (attempt + 1))).await;
    }
    tracing::debug!("the index database's log was not checkpointed: a reader stayed");
}

/// One row's change, from [`diff_rows`].
enum Op<'a> {
    Gone(String),
    Changed(&'a IndexEntry),
    Added(&'a IndexEntry),
}

/// How the rows there differ from `index`'s entries, found by reading
/// them beside the entries in the same (path) order: only the differences
/// are kept, never the rows.
async fn diff_rows<'a>(conn: &turso::Connection, index: &'a LocationIndex) -> turso::Result<Vec<Op<'a>>> {
    let mut order: Vec<u32> = (0..index.entries.len() as u32).collect();
    if !index.entries.is_sorted_by(|a, b| a.path <= b.path) {
        order.sort_by(|a, b| index.entries[*a as usize].path.cmp(&index.entries[*b as usize].path));
    }
    let mut ops = Vec::new();
    let mut added: Vec<u32> = Vec::new();
    let mut rows = conn
        .query(
            &format!("SELECT {ENTRY} FROM entries WHERE location = ?1 ORDER BY path"),
            vec![t(&index.location)],
        )
        .await?;
    let mut next = order.iter().copied().peekable();
    while let Some(row) = rows.next().await? {
        let have = entry(&row, 0)?;
        while let Some(&i) = next.peek() {
            if index.entries[i as usize].path < have.path {
                added.push(i);
                next.next();
            } else {
                break;
            }
        }
        match next.peek() {
            Some(&i) if index.entries[i as usize].path == have.path => {
                next.next();
                if index.entries[i as usize] != have {
                    ops.push(Op::Changed(&index.entries[i as usize]));
                }
            }
            _ => ops.push(Op::Gone(have.path)),
        }
    }
    added.extend(next);
    // Two entries with one path (never from a walk): the first is kept.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for i in added {
        let e = &index.entries[i as usize];
        if seen.insert(e.path.as_str()) {
            ops.push(Op::Added(e));
        }
    }
    Ok(ops)
}

/// A batch of [`diff_rows`]'s changes, inside a transaction.
async fn apply(conn: &turso::Connection, loc: &str, ops: &[Op<'_>]) -> turso::Result<()> {
    let mut update = conn
        .prepare(
            "UPDATE entries SET dir = ?3, size = ?4, mtime = ?5, sampled = ?6 \
             WHERE location = ?1 AND path = ?2",
        )
        .await?;
    let mut insert = conn
        .prepare(
            "INSERT INTO entries (location, path, parent, lname, dir, size, mtime, sampled) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .await?;
    for op in ops {
        match op {
            Op::Gone(path) => {
                conn.execute(
                    "DELETE FROM entries WHERE location = ?1 AND path = ?2",
                    vec![t(loc), t(path)],
                )
                .await?;
            }
            Op::Changed(e) => {
                update
                    .execute(vec![
                        t(loc),
                        t(&e.path),
                        Value::Integer(e.dir as i64),
                        Value::Integer(e.size as i64),
                        Value::Integer(e.mtime),
                        t(&e.sampled),
                    ])
                    .await?;
            }
            Op::Added(e) => {
                insert
                    .execute(vec![
                        t(loc),
                        t(&e.path),
                        t(e.parent()),
                        t(&e.name().to_lowercase()),
                        Value::Integer(e.dir as i64),
                        Value::Integer(e.size as i64),
                        Value::Integer(e.mtime),
                        t(&e.sampled),
                    ])
                    .await?;
            }
        }
    }
    Ok(())
}

/// The location's totals, written after its rows.
async fn put_meta(conn: &turso::Connection, index: &LocationIndex) -> turso::Result<()> {
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    for e in &index.entries {
        if e.dir {
            dirs += 1;
        } else {
            files += 1;
            bytes += e.size;
        }
    }
    conn.execute(
        "INSERT OR REPLACE INTO locations (location, root, built, files, dirs, bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        vec![
            t(&index.location),
            t(&index.root),
            Value::Integer(index.built),
            Value::Integer(files as i64),
            Value::Integer(dirs as i64),
            Value::Integer(bytes as i64),
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
