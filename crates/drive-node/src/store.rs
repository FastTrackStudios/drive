//! Where a node keeps things: document snapshots, and every location's
//! index (one row per file, so all of them can be searched at once, even
//! for devices that are off).

use std::path::Path;

use drive_files::index::LocationIndex;
use serde::Serialize;
use tokio::sync::Mutex;
use turso::Value;

pub struct Store {
    conn: Mutex<turso::Connection>,
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
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// A document's last saved snapshot.
    pub async fn load(&self, id: &str) -> turso::Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().await;
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
    pub async fn save(&self, id: &str, snapshot: Vec<u8>) -> turso::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO docs (id, snapshot) VALUES (?1, ?2)",
            vec![Value::Text(id.into()), Value::Blob(snapshot)],
        )
        .await?;
        Ok(())
    }

    /// Give a document a new id (nothing happens if `to` already exists).
    pub async fn rename(&self, from: &str, to: &str) -> turso::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE docs SET id = ?2 WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM docs WHERE id = ?2)",
            vec![Value::Text(from.into()), Value::Text(to.into())],
        )
        .await?;
        Ok(())
    }

    /// Move a space's index rows to a new space name.
    pub async fn rename_space(&self, from: &str, to: &str) -> turso::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE entries SET space = ?2 WHERE space = ?1",
            vec![Value::Text(from.into()), Value::Text(to.into())],
        )
        .await?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> turso::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM docs WHERE id = ?1",
            vec![Value::Text(id.into())],
        )
        .await?;
        Ok(())
    }

    /// Every stored document id with its snapshot size.
    pub async fn list(&self) -> turso::Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().await;
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

    /// Replace a location's rows with a new index.
    pub async fn put_index(&self, space: &str, index: &LocationIndex) -> turso::Result<usize> {
        let conn = self.conn.lock().await;
        // A request cut off mid-way (its client gone, an agent restarted
        // during a big upload) drops this future after BEGIN and leaves the
        // transaction open on the shared connection; every later BEGIN would
        // then fail. Roll any such leftover back first (with none open this
        // fails harmlessly): the leftover's DELETE is undone, so that
        // location keeps its previous rows.
        let _ = conn.execute("ROLLBACK", ()).await;
        conn.execute("BEGIN", ()).await?;
        let result = async {
            conn.execute(
                "DELETE FROM entries WHERE space = ?1 AND location = ?2",
                vec![Value::Text(space.into()), Value::Text(index.location.clone())],
            )
            .await?;
            let mut stmt = conn
                .prepare(
                    "INSERT INTO entries (space, location, parent, path, name, lname, dir, size, mtime, sampled, ext) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .await?;
            for e in &index.entries {
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
                    Value::Text(if e.dir { String::new() } else { ext_of(e.name()) }),
                ])
                .await?;
            }
            Ok::<_, turso::Error>(index.entries.len())
        }
        .await;
        match result {
            Ok(n) => {
                conn.execute("COMMIT", ()).await?;
                Ok(n)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                Err(e)
            }
        }
    }

    pub async fn drop_index(&self, space: &str, location: &str) -> turso::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM entries WHERE space = ?1 AND location = ?2",
            vec![Value::Text(space.into()), Value::Text(location.into())],
        )
        .await?;
        Ok(())
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
    #[tracing::instrument(name = "drive.store.search", skip_all)]
    pub async fn search_with(
        &self,
        space: &str,
        q: &str,
        exts: &[String],
        sampled: &[String],
        limit: i64,
    ) -> turso::Result<Vec<Found>> {
        let sampled: Vec<&String> = sampled
            .iter()
            .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
            .take(500)
            .collect();
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
            let conn = self.conn.lock().await;
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
        let mut sql = String::from(
            "SELECT location, path, name, dir, size, mtime, sampled FROM entries WHERE space = ?1",
        );
        let mut params = vec![Value::Text(space.into())];
        for w in &words {
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
        let conn = self.conn.lock().await;
        let mut rows = conn.query(&sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::found(&row)?);
        }
        Ok(out)
    }

    /// One entry of a location's index.
    pub async fn entry(
        &self,
        space: &str,
        location: &str,
        path: &str,
    ) -> turso::Result<Option<Found>> {
        let conn = self.conn.lock().await;
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
    #[tracing::instrument(name = "drive.store.list", skip_all)]
    pub async fn list_dir(
        &self,
        space: &str,
        location: &str,
        dir: &str,
    ) -> turso::Result<Vec<Found>> {
        let conn = self.conn.lock().await;
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
    pub async fn under(
        &self,
        space: &str,
        location: &str,
        prefix: &str,
    ) -> turso::Result<Vec<Found>> {
        let prefix = prefix.trim_matches('/');
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT location, path, name, dir, size, mtime, sampled FROM entries \
                 WHERE space = ?1 AND location = ?2 AND dir = 0 \
                 AND (?3 = '' OR substr(path, 1, length(?3) + 1) = ?3 || '/') ORDER BY path",
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
    pub async fn files(&self, space: &str) -> turso::Result<Vec<drive_files::protect::FileRow>> {
        let conn = self.conn.lock().await;
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
        }
        Ok(out)
    }

    /// Every location holding this content.
    #[tracing::instrument(name = "drive.store.copies", skip_all)]
    pub async fn copies(&self, space: &str, sampled: &str, size: i64) -> turso::Result<Vec<Found>> {
        let conn = self.conn.lock().await;
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
        store.conn.lock().await.execute("BEGIN", ()).await.unwrap();
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
            .conn
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
}
