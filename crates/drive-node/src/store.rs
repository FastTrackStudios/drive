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
        conn.execute("BEGIN", ()).await?;
        let result = async {
            conn.execute(
                "DELETE FROM entries WHERE space = ?1 AND location = ?2",
                vec![Value::Text(space.into()), Value::Text(index.location.clone())],
            )
            .await?;
            let mut stmt = conn
                .prepare(
                    "INSERT INTO entries (space, location, parent, path, name, lname, dir, size, mtime, sampled) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
        if words.is_empty() && exts.is_empty() {
            return Ok(Vec::new());
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
            sql.push_str(" AND dir = 0 AND (");
            for (i, e) in exts.iter().enumerate() {
                params.push(Value::Text(format!("%.{e}")));
                sql.push_str(&format!(
                    "{}lname LIKE ?{}",
                    if i > 0 { " OR " } else { "" },
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
