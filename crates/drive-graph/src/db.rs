//! The graph in a peer's own Turso database (`graph.db` beside its other
//! local tables): `nodes`, `edges` and `aliases`, replaced whole from a
//! [`Graph`] whenever it is built again, and `extracts`, the notes'
//! extracts by content hash, kept across builds so an unchanged file is
//! never parsed twice.
//!
//! Everything in it is derived: delete the file and the next build makes
//! it again. Neighbours and "under a tag" are recursive CTEs; a shortest
//! path is a breadth-first walk over the indexed edges.

use std::collections::HashMap;
use std::path::Path;

use tokio::sync::Mutex;
use turso::Value;

use crate::extract::{Extract, Sidecar, VERSION};
use crate::graph::{Edge, EdgeKind, Graph, Node, NodeKind};
use crate::index::{Error, Found, Hit, Index, Near, Result, terms};

/// Rows a single `INSERT` carries.
const ROWS: usize = 200;

/// Changes when the tables change: a database of another version is
/// emptied and made again (everything in it is derived).
const SCHEMA_VERSION: &str = "4";

/// The tables, dropped when the schema version changes.
const TABLES: &[&str] = &[
    "nodes", "edges", "aliases", "texts", "contents", "vectors", "extracts",
];

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nodes (id TEXT PRIMARY KEY, kind TEXT NOT NULL, \
     label TEXT NOT NULL, path TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS edges (src TEXT NOT NULL, dst TEXT NOT NULL, kind TEXT NOT NULL, \
     raw TEXT NOT NULL, anchor TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS edges_src ON edges (src)",
    "CREATE INDEX IF NOT EXISTS edges_dst ON edges (dst)",
    "CREATE TABLE IF NOT EXISTS aliases (name TEXT NOT NULL, id TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS aliases_name ON aliases (name)",
    "CREATE TABLE IF NOT EXISTS texts (id TEXT PRIMARY KEY, key TEXT NOT NULL, text TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS texts_key ON texts (key)",
    "CREATE TABLE IF NOT EXISTS vectors (key TEXT NOT NULL, model TEXT NOT NULL, chunk INTEGER NOT NULL, \
     vec BLOB NOT NULL)",
    "CREATE INDEX IF NOT EXISTS vectors_key ON vectors (model, key)",
    "CREATE TABLE IF NOT EXISTS contents (id TEXT PRIMARY KEY, key TEXT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS contents_key ON contents (key)",
    "CREATE INDEX IF NOT EXISTS texts_fts ON texts USING fts (text)",
    "CREATE TABLE IF NOT EXISTS extracts (key TEXT PRIMARY KEY, version INTEGER NOT NULL, \
     data BLOB NOT NULL)",
];

fn db_err(e: turso::Error) -> Error {
    Error(format!("graph database: {e}"))
}

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn text(v: Value) -> String {
    match v {
        Value::Text(t) => t,
        _ => String::new(),
    }
}

/// A peer's graph database.
pub struct GraphDb {
    conn: Mutex<turso::Connection>,
}

impl GraphDb {
    /// Open (or make) the database at `path`, shared with the peer's
    /// other processes (a service and a command run beside it).
    ///
    /// # Errors
    ///
    /// The file cannot be opened or its tables made.
    pub async fn open(path: &Path) -> Result<GraphDb> {
        let db = turso::Builder::new_local(&path.to_string_lossy())
            .experimental_multiprocess_wal(true)
            // The full-text index (`USING fts`).
            .experimental_index_method(true)
            .build()
            .await
            .map_err(db_err)?;
        GraphDb::ready(db).await
    }

    /// A database in memory (tests; a peer with no disk).
    ///
    /// # Errors
    ///
    /// The engine cannot start.
    pub async fn memory() -> Result<GraphDb> {
        let db = turso::Builder::new_local(":memory:")
            .experimental_index_method(true)
            .build()
            .await
            .map_err(db_err)?;
        GraphDb::ready(db).await
    }

    async fn ready(db: turso::Database) -> Result<GraphDb> {
        let conn = db.connect().map_err(db_err)?;
        conn.execute("PRAGMA busy_timeout = 5000", ())
            .await
            .map_err(db_err)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
            (),
        )
        .await
        .map_err(db_err)?;
        if schema_version(&conn).await?.as_deref() != Some(SCHEMA_VERSION) {
            for table in TABLES {
                conn.execute(&format!("DROP TABLE IF EXISTS {table}"), ())
                    .await
                    .map_err(db_err)?;
            }
        }
        for sql in SCHEMA {
            conn.execute(sql, ()).await.map_err(db_err)?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO meta (k, v) VALUES ('schema', ?1)",
            vec![t(SCHEMA_VERSION)],
        )
        .await
        .map_err(db_err)?;
        Ok(GraphDb {
            conn: Mutex::new(conn),
        })
    }

    /// The content keys with vectors of `model`.
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn vector_keys(&self, model: &str) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT DISTINCT key FROM vectors WHERE model = ?1",
                vec![t(model)],
            )
            .await
            .map_err(db_err)?;
        let mut out = std::collections::HashSet::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.insert(text(row.get_value(0).map_err(db_err)?));
        }
        Ok(out)
    }

    /// Keep a text's vectors (one per chunk) under its key and `model`, in
    /// place of any it had.
    ///
    /// # Errors
    ///
    /// A write failed.
    pub async fn put_vectors(&self, model: &str, items: &[(String, Vec<Vec<f32>>)]) -> Result<()> {
        let conn = self.conn.lock().await;
        in_tx(&conn, async {
            for (key, vecs) in items {
                conn.execute(
                    "DELETE FROM vectors WHERE model = ?1 AND key = ?2",
                    vec![t(model), t(key)],
                )
                .await?;
                for (i, v) in vecs.iter().enumerate() {
                    conn.execute(
                        "INSERT INTO vectors (key, model, chunk, vec) VALUES (?1, ?2, ?3, vector32(?4))",
                        vec![t(key), t(model), Value::Integer(i as i64), t(&vector_text(v))],
                    )
                    .await?;
                }
            }
            Ok(())
        })
        .await
    }

    /// Forget vectors of texts no longer in the graph, and of other text
    /// models (image models' vectors are kept by content: [`Self::prune_content_vectors`]).
    ///
    /// # Errors
    ///
    /// A write failed.
    pub async fn prune_vectors(&self, model: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        in_tx(&conn, async {
            conn.execute(
                "DELETE FROM vectors WHERE model NOT LIKE 'image:%' \
                 AND (model <> ?1 OR key NOT IN (SELECT key FROM texts))",
                vec![t(model)],
            )
            .await
            .map(|_| ())
        })
        .await
    }

    /// Forget an image model's vectors of contents no longer in the graph.
    ///
    /// # Errors
    ///
    /// A write failed.
    pub async fn prune_content_vectors(&self, model: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        in_tx(&conn, async {
            conn.execute(
                "DELETE FROM vectors WHERE model = ?1 AND key NOT IN (SELECT key FROM contents)",
                vec![t(model)],
            )
            .await
            .map(|_| ())
        })
        .await
    }

    /// Each Drive file's node and content key whose name says it is one of
    /// `extensions` (lower case, without the dot).
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn contents_of(&self, extensions: &[&str]) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT c.id, c.key, n.path FROM contents c JOIN nodes n ON n.id = c.id ORDER BY c.key",
                (),
            )
            .await
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            let path = text(row.get_value(2).map_err(db_err)?).to_lowercase();
            let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
            if extensions.contains(&ext) {
                out.push((
                    text(row.get_value(0).map_err(db_err)?),
                    text(row.get_value(1).map_err(db_err)?),
                ));
            }
        }
        Ok(out)
    }

    /// The files whose content's vector (of an image `model`) is nearest
    /// `vector`, best first.
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn nearest_content(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<Found>> {
        self.nearest_by("contents", model, vector, limit).await
    }

    /// The nodes whose text has a chunk nearest `vector` (of `model`), best
    /// first: the score is the best chunk's cosine similarity.
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn nearest(&self, model: &str, vector: &[f32], limit: usize) -> Result<Vec<Found>> {
        self.nearest_by("texts", model, vector, limit).await
    }

    /// The nodes of `table` (texts or contents: `id`, `key`) whose key's
    /// vectors come nearest `vector`.
    async fn nearest_by(
        &self,
        table: &str,
        model: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<Found>> {
        let scored: Vec<(String, f64)> = {
            let conn = self.conn.lock().await;
            let mut rows = conn
                .query(
                    &format!(
                        "SELECT x.id, MIN(vector_distance_cos(v.vec, vector32(?1))) AS d \
                         FROM vectors v JOIN {table} x ON x.key = v.key WHERE v.model = ?2 \
                         GROUP BY x.id ORDER BY d, x.id LIMIT ?3"
                    ),
                    vec![
                        t(&vector_text(vector)),
                        t(model),
                        Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
                    ],
                )
                .await
                .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let d = match row.get_value(1).map_err(db_err)? {
                    Value::Real(f) => f,
                    Value::Integer(i) => i as f64,
                    _ => 1.0,
                };
                out.push((text(row.get_value(0).map_err(db_err)?), 1.0 - d));
            }
            out
        };
        let mut out = Vec::new();
        for (id, score) in scored {
            if let Some(node) = self.node(&id).await? {
                out.push(Found { node, score });
            }
        }
        Ok(out)
    }

    /// Each node's text and its key (what its vectors are kept under).
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn texts(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT DISTINCT key, text FROM texts ORDER BY key", ())
            .await
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.push((
                text(row.get_value(0).map_err(db_err)?),
                text(row.get_value(1).map_err(db_err)?),
            ));
        }
        Ok(out)
    }

    /// Keep `g` as the graph, in place of whatever was there (one
    /// transaction: readers see the old graph or the new one).
    ///
    /// # Errors
    ///
    /// A write failed; the old graph stays.
    pub async fn replace(&self, g: &Graph) -> Result<()> {
        let conn = self.conn.lock().await;
        in_tx(&conn, write_graph(&conn, g)).await
    }

    /// The extracts kept for these content keys (made by this version of
    /// the extractor; older ones are made again).
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn extracts(&self, keys: &[String]) -> Result<HashMap<String, Extract>> {
        let conn = self.conn.lock().await;
        let mut out = HashMap::new();
        for chunk in keys.chunks(ROWS) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut params: Vec<Value> = chunk.iter().map(|k| t(k)).collect();
            params.push(Value::Integer(i64::from(VERSION)));
            let mut rows = conn
                .query(
                    &format!(
                        "SELECT key, data FROM extracts WHERE key IN ({marks}) AND version = ?{}",
                        chunk.len() + 1
                    ),
                    params,
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let key = text(row.get_value(0).map_err(db_err)?);
                let Value::Blob(data) = row.get_value(1).map_err(db_err)? else {
                    continue;
                };
                // A row that does not read back is made again, not an error.
                if let Ok(e) = Sidecar::decode(&data, &key) {
                    out.insert(key, e);
                }
            }
        }
        Ok(out)
    }

    /// Keep extracts by content key (as sidecars, so they can be handed
    /// to another peer as they are).
    ///
    /// # Errors
    ///
    /// A write failed.
    pub async fn put_extracts(&self, items: &[(String, Extract)]) -> Result<()> {
        let conn = self.conn.lock().await;
        in_tx(&conn, async {
            for (key, e) in items {
                conn.execute(
                    "INSERT OR REPLACE INTO extracts (key, version, data) VALUES (?1, ?2, ?3)",
                    vec![
                        t(key),
                        Value::Integer(i64::from(VERSION)),
                        Value::Blob(Sidecar::new(key, e.clone()).encode()),
                    ],
                )
                .await?;
            }
            Ok(())
        })
        .await
    }

    /// A kept extract as a sidecar's bytes, to give another peer.
    ///
    /// # Errors
    ///
    /// A read failed.
    pub async fn sidecar(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT data FROM extracts WHERE key = ?1 AND version = ?2",
                vec![t(key), Value::Integer(i64::from(VERSION))],
            )
            .await
            .map_err(db_err)?;
        Ok(match rows.next().await.map_err(db_err)? {
            Some(row) => match row.get_value(0).map_err(db_err)? {
                Value::Blob(b) => Some(b),
                _ => None,
            },
            None => None,
        })
    }

    /// Forget extracts of content no longer in the space, and those made
    /// by another version. Returns how many went.
    ///
    /// # Errors
    ///
    /// A write failed.
    pub async fn prune_extracts(&self, keep: &[String]) -> Result<u64> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT key, version FROM extracts", ())
            .await
            .map_err(db_err)?;
        let keep: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
        let mut gone = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            let key = text(row.get_value(0).map_err(db_err)?);
            let version = match row.get_value(1).map_err(db_err)? {
                Value::Integer(v) => v,
                _ => -1,
            };
            if version != i64::from(VERSION) || !keep.contains(key.as_str()) {
                gone.push(key);
            }
        }
        drop(rows);
        let n = gone.len() as u64;
        in_tx(&conn, async {
            for k in &gone {
                conn.execute("DELETE FROM extracts WHERE key = ?1", vec![t(k)])
                    .await?;
            }
            Ok(())
        })
        .await?;
        Ok(n)
    }

    async fn nodes_where(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Node>> {
        let conn = self.conn.lock().await;
        let mut rows = conn.query(sql, params).await.map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.push(read_node(&row, 0)?);
        }
        Ok(out)
    }
}

/// What a text's vectors are kept under: its BLAKE3 hash, so the same
/// text anywhere is embedded once.
#[must_use]
pub fn text_key(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// A vector as Turso's `vector32` reads it: `[0.1,0.2,…]`.
fn vector_text(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", parts.join(","))
}

async fn schema_version(conn: &turso::Connection) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT v FROM meta WHERE k = 'schema'", ())
        .await
        .map_err(db_err)?;
    Ok(match rows.next().await.map_err(db_err)? {
        Some(row) => Some(text(row.get_value(0).map_err(db_err)?)),
        None => None,
    })
}

/// Run `writes` in one transaction: committed if they all succeed.
async fn in_tx(
    conn: &turso::Connection,
    writes: impl std::future::Future<Output = turso::Result<()>>,
) -> Result<()> {
    conn.execute("BEGIN", ()).await.map_err(db_err)?;
    match writes.await {
        Ok(()) => conn.execute("COMMIT", ()).await.map(|_| ()).map_err(db_err),
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(db_err(e))
        }
    }
}

/// Write `g` over the graph in the tables: only the rows that differ
/// (a graph made again after one note changed writes that note's rows).
async fn write_graph(conn: &turso::Connection, g: &Graph) -> turso::Result<()> {
    let nodes: Vec<Vec<String>> = g
        .nodes
        .values()
        .map(|n| {
            vec![
                n.id.clone(),
                n.kind.id().into(),
                n.label.clone(),
                n.path.clone(),
            ]
        })
        .collect();
    let edges: Vec<Vec<String>> = g
        .edges
        .iter()
        .map(|e| {
            vec![
                e.src.clone(),
                e.dst.clone(),
                e.kind.id().into(),
                e.raw.clone(),
                e.anchor.clone(),
            ]
        })
        .collect();
    let aliases: Vec<Vec<String>> = g
        .aliases
        .iter()
        .map(|(n, id)| vec![n.clone(), id.clone()])
        .collect();
    // Texts are compared by their key (the text's hash), not the text.
    let texts: Vec<Vec<String>> = g
        .texts
        .iter()
        .map(|(id, x)| vec![id.clone(), text_key(x), x.clone()])
        .collect();
    sync_rows(conn, &Table::NODES, &nodes).await?;
    sync_rows(conn, &Table::EDGES, &edges).await?;
    sync_rows(conn, &Table::ALIASES, &aliases).await?;
    sync_rows(conn, &Table::TEXTS, &texts).await?;
    let contents: Vec<Vec<String>> = g
        .contents
        .iter()
        .map(|(id, key)| vec![id.clone(), key.clone()])
        .collect();
    sync_rows(conn, &Table::CONTENTS, &contents).await
}

/// A table as [`sync_rows`] writes it.
struct Table {
    name: &'static str,
    /// Every column, in insert order.
    columns: &'static [&'static str],
    /// How many leading columns tell rows apart (and are compared).
    compared: usize,
    /// How many leading columns a delete matches on.
    delete_by: usize,
}

impl Table {
    const NODES: Table = Table {
        name: "nodes",
        columns: &["id", "kind", "label", "path"],
        compared: 4,
        delete_by: 1,
    };
    const EDGES: Table = Table {
        name: "edges",
        columns: &["src", "dst", "kind", "raw", "anchor"],
        compared: 5,
        delete_by: 5,
    };
    const ALIASES: Table = Table {
        name: "aliases",
        columns: &["name", "id"],
        compared: 2,
        delete_by: 2,
    };
    const CONTENTS: Table = Table {
        name: "contents",
        columns: &["id", "key"],
        compared: 2,
        delete_by: 1,
    };
    const TEXTS: Table = Table {
        name: "texts",
        columns: &["id", "key", "text"],
        compared: 2,
        delete_by: 1,
    };
}

/// Make `table` hold exactly `rows`: the rows there that are not wanted
/// deleted, the wanted ones not there inserted.
async fn sync_rows(
    conn: &turso::Connection,
    table: &Table,
    rows: &[Vec<String>],
) -> turso::Result<()> {
    use std::collections::HashSet;
    let cmp = &table.columns[..table.compared];
    let mut have: HashSet<Vec<String>> = HashSet::new();
    let mut q = conn
        .query(
            &format!("SELECT {} FROM {}", cmp.join(", "), table.name),
            (),
        )
        .await?;
    while let Some(row) = q.next().await? {
        let mut r = Vec::with_capacity(cmp.len());
        for i in 0..cmp.len() {
            r.push(text(row.get_value(i)?));
        }
        have.insert(r);
    }
    drop(q);
    let want: HashSet<Vec<String>> = rows.iter().map(|r| r[..table.compared].to_vec()).collect();
    let by = &table.columns[..table.delete_by];
    let condition = by
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} = ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    for gone in have.iter().filter(|r| !want.contains(*r)) {
        let params: Vec<Value> = gone[..table.delete_by].iter().map(|v| t(v)).collect();
        conn.execute(
            &format!("DELETE FROM {} WHERE {condition}", table.name),
            params,
        )
        .await?;
    }
    let new: Vec<Vec<Value>> = rows
        .iter()
        .filter(|r| !have.contains(&r[..table.compared]))
        .map(|r| r.iter().map(|v| t(v)).collect())
        .collect();
    insert(
        conn,
        &format!("{} ({})", table.name, table.columns.join(", ")),
        &new,
    )
    .await
}

/// Rows into `table (columns)`, [`ROWS`] an `INSERT`.
async fn insert(conn: &turso::Connection, into: &str, rows: &[Vec<Value>]) -> turso::Result<()> {
    for chunk in rows.chunks(ROWS) {
        let width = chunk.first().map_or(0, Vec::len);
        let tuple = format!("({})", vec!["?"; width].join(","));
        let values = vec![tuple; chunk.len()].join(",");
        let params: Vec<Value> = chunk.iter().flatten().cloned().collect();
        conn.execute(&format!("INSERT INTO {into} VALUES {values}"), params)
            .await?;
    }
    Ok(())
}

/// Node columns `id, kind, label, path` from column `at`.
fn read_node(row: &turso::Row, at: usize) -> Result<Node> {
    let col = |i: usize| row.get_value(at + i).map(text).map_err(db_err);
    let kind = col(1)?;
    Ok(Node {
        id: col(0)?,
        kind: NodeKind::parse(&kind).ok_or_else(|| Error(format!("a node of kind {kind}?")))?,
        label: col(2)?,
        path: col(3)?,
    })
}

/// Edge columns `src, dst, kind, raw, anchor` from column `at`.
fn read_edge(row: &turso::Row, at: usize) -> Result<Edge> {
    let col = |i: usize| row.get_value(at + i).map(text).map_err(db_err);
    let kind = col(2)?;
    Ok(Edge {
        src: col(0)?,
        dst: col(1)?,
        kind: EdgeKind::parse(&kind).ok_or_else(|| Error(format!("an edge of kind {kind}?")))?,
        raw: col(3)?,
        anchor: col(4)?,
    })
}

const NODE_COLS: &str = "n.id, n.kind, n.label, n.path";

impl Index for GraphDb {
    async fn node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self
            .nodes_where(
                &format!("SELECT {NODE_COLS} FROM nodes n WHERE n.id = ?1"),
                vec![t(id)],
            )
            .await?
            .pop())
    }

    async fn find(&self, name: &str) -> Result<Vec<Node>> {
        self.nodes_where(
            &format!(
                "SELECT DISTINCT {NODE_COLS} FROM aliases a JOIN nodes n ON n.id = a.id \
                 WHERE a.name = ?1 ORDER BY n.id"
            ),
            vec![t(&name.trim().to_lowercase())],
        )
        .await
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Found>> {
        // Words only (letters and digits), so the engine's query syntax is
        // never reached by what a person typed.
        let q = terms(query).join(" ");
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // The shape the full-text index answers with scores (a single
        // table, by score, limited): the nodes are read after.
        let scored: Vec<(String, f64)> = {
            let conn = self.conn.lock().await;
            let mut rows = conn
                .query(
                    "SELECT fts_score(text, ?1) AS score, id FROM texts \
                     WHERE fts_match(text, ?1) ORDER BY score DESC LIMIT ?2",
                    vec![
                        t(&q),
                        Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
                    ],
                )
                .await
                .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let score = match row.get_value(0).map_err(db_err)? {
                    Value::Real(f) => f,
                    Value::Integer(i) => i as f64,
                    _ => 0.0,
                };
                out.push((text(row.get_value(1).map_err(db_err)?), score));
            }
            out
        };
        let mut out = Vec::new();
        for (id, score) in scored {
            if let Some(node) = self.node(&id).await? {
                out.push(Found { node, score });
            }
        }
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.node.id.cmp(&b.node.id))
        });
        Ok(out)
    }

    async fn touching(&self, id: &str) -> Result<Vec<Edge>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT src, dst, kind, raw, anchor FROM edges WHERE src = ?1 \
                 UNION ALL SELECT src, dst, kind, raw, anchor FROM edges WHERE dst = ?1 AND src <> ?1",
                vec![t(id)],
            )
            .await
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            out.push(read_edge(&row, 0)?);
        }
        Ok(out)
    }

    async fn neighbors(&self, id: &str, depth: u32) -> Result<Vec<Near>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                &format!(
                    "WITH RECURSIVE walk(id, d) AS ( \
                       SELECT ?1, 0 \
                       UNION \
                       SELECT CASE WHEN e.src = w.id THEN e.dst ELSE e.src END, w.d + 1 \
                       FROM walk w JOIN edges e ON e.src = w.id OR e.dst = w.id \
                       WHERE w.d < ?2) \
                     SELECT {NODE_COLS}, MIN(w.d) AS depth FROM walk w JOIN nodes n ON n.id = w.id \
                     WHERE w.id <> ?1 GROUP BY n.id ORDER BY depth, n.id"
                ),
                vec![t(id), Value::Integer(i64::from(depth))],
            )
            .await
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            let depth = match row.get_value(4).map_err(db_err)? {
                Value::Integer(d) => u32::try_from(d).unwrap_or(u32::MAX),
                _ => 0,
            };
            out.push(Near {
                node: read_node(&row, 0)?,
                depth,
            });
        }
        Ok(out)
    }

    async fn under_tag(&self, tag: &str) -> Result<Vec<Hit>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                &format!(
                    "WITH RECURSIVE sub(id) AS ( \
                       SELECT ?1 \
                       UNION \
                       SELECT e.src FROM edges e JOIN sub s ON e.dst = s.id WHERE e.kind = 'parent') \
                     SELECT {NODE_COLS}, e.src, e.dst, e.kind, e.raw, e.anchor \
                     FROM edges e JOIN sub s ON e.dst = s.id JOIN nodes n ON n.id = e.src \
                     WHERE e.kind IN ('tagged', 'fm-tag') \
                     ORDER BY n.id, e.src, e.dst, e.kind, e.raw, e.anchor"
                ),
                vec![t(tag)],
            )
            .await
            .map_err(db_err)?;
        let mut out: Vec<Hit> = Vec::new();
        while let Some(row) = rows.next().await.map_err(db_err)? {
            let hit = Hit {
                node: read_node(&row, 0)?,
                edge: read_edge(&row, 4)?,
            };
            if out.last().is_none_or(|h| h.node.id != hit.node.id) {
                out.push(hit);
            }
        }
        out.sort_by(|a, b| (&a.node.label, &a.node.id).cmp(&(&b.node.label, &b.node.id)));
        Ok(out)
    }
}
