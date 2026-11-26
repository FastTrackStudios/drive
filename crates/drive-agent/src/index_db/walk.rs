//! A walk written into the index database as it goes (drive#156): its
//! entries go into `walk_entries` under the walk's id a batch at a time, are compared with the location's rows a page at a time when the
//! walk is done, and the upload is encoded from the rows. No step holds
//! the whole index.

use drive_files::index::{Encoder, IndexEntry, Signature};
use turso::Value;

use super::{BATCH, IndexDb, Op, apply, checkpoint, entry, int, t, text, under_bounds};

/// Entries compared at once, of each side.
const PAGE: usize = BATCH / 2;

/// A walk in progress: its entries so far, rows of `walk_entries` under
/// its id. Made the location's index by [`IndexDb::commit_walk`];
/// dropped, whatever happened, by [`IndexDb::discard_walk`] (or, if the
/// process died, by the next agent to open the database).
///
/// One table for every walk, made once: a table a walk (made and dropped
/// with it) changed the database's schema, and every other process's
/// next statement failed with "Database schema changed" (drive#157).
pub struct Walk {
    id: i64,
    location: String,
}

impl Walk {
    pub fn location(&self) -> &str {
        &self.location
    }
}

/// What a walk holds, counted as its rows are compared.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

impl Totals {
    fn add(&mut self, entries: &[IndexEntry]) {
        for e in entries {
            if e.dir {
                self.dirs += 1;
            } else {
                self.files += 1;
                self.bytes += e.size;
            }
        }
    }
}

/// Why an index was not encoded from the database.
#[derive(Debug)]
pub enum EncodeFailed {
    Db(turso::Error),
    /// Its rows changed while they were read.
    Changed(drive_files::index::EncodeError),
}

impl std::fmt::Display for EncodeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeFailed::Db(e) => write!(f, "the index database: {e}"),
            EncodeFailed::Changed(e) => write!(f, "the index changed while it was encoded ({e})"),
        }
    }
}

impl From<turso::Error> for EncodeFailed {
    fn from(e: turso::Error) -> Self {
        EncodeFailed::Db(e)
    }
}

/// The tables walks are written to (made with the others, once).
pub(super) const SCHEMA: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS walks (walk INTEGER PRIMARY KEY, pid INTEGER NOT NULL, \
     location TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS walk_entries (walk INTEGER NOT NULL, path TEXT NOT NULL, \
     dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL, sampled TEXT NOT NULL)",
    "CREATE UNIQUE INDEX IF NOT EXISTS walk_entries_path ON walk_entries (walk, path)",
];

impl IndexDb {
    /// Start a walk of `location` (made by this process, `pid`).
    pub async fn begin_walk(&self, location: &str) -> turso::Result<Walk> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        conn.execute(
            "INSERT INTO walks (pid, location) VALUES (?1, ?2)",
            vec![Value::Integer(i64::from(std::process::id())), t(location)],
        )
        .await?;
        Ok(Walk {
            id: conn.last_insert_rowid(),
            location: location.to_string(),
        })
    }

    /// Add what the walk found to it ([`BATCH`] rows a transaction, the
    /// log folded in after each, as [`IndexDb::put`]). A path it already
    /// has keeps its first entry.
    pub async fn walk_put(&self, walk: &Walk, entries: &[IndexEntry]) -> turso::Result<()> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        for batch in entries.chunks(BATCH) {
            self.in_tx(&conn, insert_walk(&conn, walk.id, batch))
                .await?;
            checkpoint(&conn).await;
        }
        Ok(())
    }

    /// Everything sent on `found` (a walk's batches, as
    /// [`drive_files::index::walk_streamed`] hands them over), put in the
    /// walk [`BATCH`] rows at a time, until the sender is gone.
    ///
    /// # Errors
    /// The first write that failed. `found` is dropped then, so a sender
    /// blocked on it is let go (and what it sends after is lost).
    pub async fn fill_walk(
        &self,
        walk: &Walk,
        mut found: tokio::sync::mpsc::Receiver<Vec<IndexEntry>>,
    ) -> turso::Result<()> {
        let mut pending: Vec<IndexEntry> = Vec::with_capacity(BATCH);
        while let Some(mut batch) = found.recv().await {
            pending.append(&mut batch);
            if pending.len() >= BATCH {
                self.walk_put(walk, &pending).await?;
                pending.clear();
            }
        }
        self.walk_put(walk, &pending).await
    }

    /// Make the walk its location's index: only the rows that differ are
    /// written ([`BATCH`] a transaction, as [`IndexDb::put`]), the totals
    /// last. Returns how many rows were written and what the walk holds.
    /// The walk itself is left for [`IndexDb::discard_walk`].
    pub async fn commit_walk(
        &self,
        walk: &Walk,
        root: &str,
        built: i64,
    ) -> turso::Result<(usize, Totals)> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        let (mut written, mut totals) = (0usize, Totals::default());
        let mut after = String::new();
        loop {
            let new = self.walk_page(walk.id, "", &after, PAGE).await?;
            let old = self.page(&walk.location, &after, PAGE).await?;
            let upto = comparable_upto(&new, &old, PAGE).map(str::to_string);
            let new = up_to(new, upto.as_deref());
            let old = up_to(old, upto.as_deref());
            totals.add(&new);
            let ops = diff_pages(&new, old);
            if !ops.is_empty() {
                self.in_tx(&conn, apply(&conn, &walk.location, &ops))
                    .await?;
                checkpoint(&conn).await;
                written += ops.len();
            }
            match upto {
                Some(p) => after = p,
                None => break,
            }
        }
        let meta = vec![
            t(&walk.location),
            t(root),
            Value::Integer(built),
            Value::Integer(totals.files as i64),
            Value::Integer(totals.dirs as i64),
            Value::Integer(totals.bytes as i64),
        ];
        self.in_tx(&conn, async {
            conn.execute(
                "INSERT OR REPLACE INTO locations (location, root, built, files, dirs, bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                meta,
            )
            .await
            .map(|_| ())
        })
        .await?;
        Ok((written, totals))
    }

    /// Drop a walk's rows ([`BATCH`] a transaction), whether it was
    /// committed or not.
    pub async fn discard_walk(&self, walk: Walk) -> turso::Result<()> {
        self.drop_walk(walk.id).await
    }

    async fn drop_walk(&self, id: i64) -> turso::Result<()> {
        let conn = self.write.lock().await;
        self.abandon(&conn).await;
        loop {
            let gone = conn
                .execute(
                    &format!(
                        "DELETE FROM walk_entries WHERE rowid IN \
                         (SELECT rowid FROM walk_entries WHERE walk = ?1 LIMIT {BATCH})"
                    ),
                    vec![Value::Integer(id)],
                )
                .await?;
            checkpoint(&conn).await;
            if gone == 0 {
                break;
            }
        }
        conn.execute(
            "DELETE FROM walks WHERE walk = ?1",
            vec![Value::Integer(id)],
        )
        .await?;
        Ok(())
    }

    /// Walks left by a process no longer running (killed mid-walk): their
    /// rows are dropped. Returns how many walks.
    pub(super) async fn drop_dead_walks(
        &self,
        alive: impl Fn(u32) -> bool,
    ) -> turso::Result<usize> {
        let mut dead = Vec::new();
        {
            let r = self.reader().await;
            let mut rows = r.query("SELECT walk, pid FROM walks", ()).await?;
            while let Some(row) = rows.next().await? {
                let (id, pid) = (int(row.get_value(0)?), int(row.get_value(1)?));
                if u32::try_from(pid).is_ok_and(|pid| !alive(pid)) {
                    dead.push(id);
                }
            }
        }
        for id in &dead {
            self.drop_walk(*id).await?;
        }
        Ok(dead.len())
    }

    /// Up to `limit` of a walk's entries under `dir` (`""`: all) in path
    /// order, after `after`.
    async fn walk_page(
        &self,
        walk: i64,
        dir: &str,
        after: &str,
        limit: usize,
    ) -> turso::Result<Vec<IndexEntry>> {
        let (low, high) = under_bounds(dir);
        let after = after.max(low.as_str());
        let r = self.reader().await;
        let (sql, args) = match high {
            Some(high) => (
                format!(
                    "SELECT {} FROM walk_entries WHERE walk = ?1 AND path > ?2 AND path < ?3 \
                     ORDER BY path LIMIT ?4",
                    super::ENTRY
                ),
                vec![
                    Value::Integer(walk),
                    t(after),
                    t(&high),
                    Value::Integer(limit as i64),
                ],
            ),
            None => (
                format!(
                    "SELECT {} FROM walk_entries WHERE walk = ?1 AND path > ?2 ORDER BY path LIMIT ?3",
                    super::ENTRY
                ),
                vec![Value::Integer(walk), t(after), Value::Integer(limit as i64)],
            ),
        };
        let mut rows = r.query(&sql, args).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(entry(&row, 0)?);
        }
        Ok(out)
    }

    async fn walk_get(&self, walk: &Walk, path: &str) -> turso::Result<Option<IndexEntry>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                &format!(
                    "SELECT {} FROM walk_entries WHERE walk = ?1 AND path = ?2",
                    super::ENTRY
                ),
                vec![Value::Integer(walk.id), t(path)],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(entry(&row, 0)?)),
            None => Ok(None),
        }
    }

    /// The paths of the walk's files holding `sampled` (a scan of the walk:
    /// only asked when a tagged folder is gone).
    async fn walk_holding(&self, walk: &Walk, sampled: &str) -> turso::Result<Vec<String>> {
        let r = self.reader().await;
        let mut rows = r
            .query(
                "SELECT path FROM walk_entries WHERE walk = ?1 AND dir = 0 AND sampled = ?2",
                vec![Value::Integer(walk.id), t(sampled)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(text(row.get_value(0)?));
        }
        Ok(out)
    }

    /// The signature of `folder` (as [`drive_files::index::folder_signature`])
    /// in the walk (`Some(walk)`) or in the location's rows (`None`), read
    /// a page at a time, with its first readable file.
    async fn signature(
        &self,
        walk: &Walk,
        side: Side,
        folder: &str,
    ) -> turso::Result<Option<(String, IndexEntry)>> {
        let mut sig = Signature::new(folder);
        let mut first: Option<IndexEntry> = None;
        let mut after = String::new();
        loop {
            let page = match side {
                Side::Walk => self.walk_page(walk.id, folder, &after, PAGE).await?,
                Side::Rows => {
                    self.page_under(&walk.location, folder, &after, PAGE)
                        .await?
                }
            };
            let Some(last) = page.last() else { break };
            after = last.path.clone();
            page.iter().for_each(|e| sig.add(e));
            if first.is_none() {
                first = page.into_iter().find(|e| !e.dir && !e.sampled.is_empty());
            }
        }
        Ok(sig.finish().zip(first))
    }

    /// Where each of `wanted` (folders of the location's last walk) went
    /// in this one, as [`drive_files::index::moved_folders`] finds it: a
    /// folder no longer there is matched to the one new folder holding
    /// exactly the same files. Asked of the database: the folders that
    /// could match are those holding the gone folder's first file at the
    /// same place inside them.
    pub async fn moved_folders(
        &self,
        walk: &Walk,
        wanted: &[String],
    ) -> turso::Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for w in wanted {
            if self.walk_get(walk, w).await?.is_some_and(|e| e.dir) {
                continue;
            }
            let Some((sig, first)) = self.signature(walk, Side::Rows, w).await? else {
                continue;
            };
            let inside = &first.path[w.len() + 1..];
            let mut matches = Vec::new();
            for held in self.walk_holding(walk, &first.sampled).await? {
                let Some(candidate) = container(&held, inside) else {
                    continue;
                };
                if !self.is_fresh_folder(walk, candidate).await? {
                    continue;
                }
                let theirs = self.signature(walk, Side::Walk, candidate).await?;
                if theirs.is_some_and(|(s, _)| s == sig) {
                    matches.push(candidate.to_string());
                }
            }
            matches.dedup();
            if let [one] = matches.as_slice() {
                out.push((w.clone(), one.clone()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// A folder in the walk that the location's last walk did not have.
    async fn is_fresh_folder(&self, walk: &Walk, path: &str) -> turso::Result<bool> {
        if !self.walk_get(walk, path).await?.is_some_and(|e| e.dir) {
            return Ok(false);
        }
        Ok(!self.get(&walk.location, path).await?.is_some_and(|e| e.dir))
    }

    /// A location's index in its published form (as
    /// [`drive_files::index::encode`] makes it from the whole index),
    /// encoded from its rows a page at a time; `None` if it has none.
    ///
    /// # Errors
    /// The database could not be read, or the index changed meanwhile.
    pub async fn encode(&self, location: &str) -> Result<Option<Vec<u8>>, EncodeFailed> {
        let Some(meta) = self.meta(location).await? else {
            return Ok(None);
        };
        let mut enc = Encoder::new(location, &meta.root, meta.built, meta.files + meta.dirs);
        let mut after = String::new();
        loop {
            let page = self.page(location, &after, BATCH).await?;
            let Some(last) = page.last() else { break };
            after = last.path.clone();
            for e in &page {
                enc.push(e).map_err(EncodeFailed::Changed)?;
            }
        }
        enc.finish().map(Some).map_err(EncodeFailed::Changed)
    }
}

/// Which rows a folder's signature is read from.
#[derive(Clone, Copy)]
enum Side {
    Walk,
    Rows,
}

/// Whether process `pid` is running (here, or as another user).
pub(super) fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks that the process exists.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The folder holding `path` at `inside` (`a/b/x/y.wav` holds `x/y.wav`
/// in `a/b`); none at the root or when it does not end so.
fn container<'a>(path: &'a str, inside: &str) -> Option<&'a str> {
    let folder = path.strip_suffix(inside)?.strip_suffix('/')?;
    (!folder.is_empty()).then_some(folder)
}

/// How far two pages (each in path order, each at most `limit` long, read
/// after the same path) can be compared: to the end of the shorter full
/// one, as the other may go on past it; `None` when neither is full (both
/// sides end within them).
fn comparable_upto<'a>(
    new: &'a [IndexEntry],
    old: &'a [IndexEntry],
    limit: usize,
) -> Option<&'a str> {
    let end = |page: &'a [IndexEntry]| {
        (page.len() >= limit)
            .then(|| page.last().map(|e| e.path.as_str()))
            .flatten()
    };
    match (end(new), end(old)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The entries of a path-ordered page up to `upto` (all, for `None`).
fn up_to(mut page: Vec<IndexEntry>, upto: Option<&str>) -> Vec<IndexEntry> {
    if let Some(upto) = upto {
        let keep = page.partition_point(|e| e.path.as_str() <= upto);
        page.truncate(keep);
    }
    page
}

/// How the `old` rows differ from the `new` entries, both in path order
/// and covering the same paths: gone, changed and added.
fn diff_pages(new: &[IndexEntry], old: Vec<IndexEntry>) -> Vec<Op<'_>> {
    let mut ops = Vec::new();
    let mut new = new.iter().peekable();
    for have in old {
        while let Some(added) = new.next_if(|n| n.path < have.path) {
            ops.push(Op::Added(added));
        }
        match new.next_if(|n| n.path == have.path) {
            Some(n) if *n != have => ops.push(Op::Changed(n)),
            Some(_) => {}
            None => ops.push(Op::Gone(have.path)),
        }
    }
    ops.extend(new.map(Op::Added));
    ops
}

/// A batch of a walk's entries, inside a transaction.
async fn insert_walk(
    conn: &turso::Connection,
    walk: i64,
    batch: &[IndexEntry],
) -> turso::Result<()> {
    let mut insert = conn
        .prepare(
            "INSERT OR IGNORE INTO walk_entries (walk, path, dir, size, mtime, sampled) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .await?;
    for e in batch {
        insert
            .execute(vec![
                Value::Integer(walk),
                t(&e.path),
                Value::Integer(e.dir as i64),
                Value::Integer(e.size as i64),
                Value::Integer(e.mtime),
                t(&e.sampled),
            ])
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
