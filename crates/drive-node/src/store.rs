//! Where a node keeps its documents: one Turso table of snapshots.

use std::path::Path;

use turso::Value;

pub struct Store {
    conn: turso::Connection,
}

impl Store {
    pub async fn open(path: &Path) -> turso::Result<Store> {
        let db = turso::Builder::new_local(&path.to_string_lossy()).build().await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS docs (id TEXT PRIMARY KEY, snapshot BLOB NOT NULL)",
            (),
        )
        .await?;
        Ok(Store { conn })
    }

    /// A document's last saved snapshot.
    pub async fn load(&self, id: &str) -> turso::Result<Option<Vec<u8>>> {
        let mut rows = self
            .conn
            .query("SELECT snapshot FROM docs WHERE id = ?1", vec![Value::Text(id.into())])
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
        self.conn
            .execute(
                "INSERT OR REPLACE INTO docs (id, snapshot) VALUES (?1, ?2)",
                vec![Value::Text(id.into()), Value::Blob(snapshot)],
            )
            .await?;
        Ok(())
    }

    /// Every stored document id with its snapshot size.
    pub async fn list(&self) -> turso::Result<Vec<(String, u64)>> {
        let mut rows = self
            .conn
            .query("SELECT id, length(snapshot) FROM docs", ())
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let id = match row.get_value(0)? {
                Value::Text(t) => t,
                _ => continue,
            };
            let len = match row.get_value(1)? {
                Value::Integer(n) => n.max(0) as u64,
                _ => 0,
            };
            out.push((id, len));
        }
        Ok(out)
    }
}
