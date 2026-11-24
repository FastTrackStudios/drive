//! The sidecar store: data derived from files (thumbnails, waveform
//! peaks; `drive-sidecars`), as blobs in their own folder
//! (`data/sidecars`, apart from the files' blobs so purging can never touch
//! a file), listed in a table of their own database (`data/sidecars.db`):
//! one row per (content key, kind), with the producer's version, the blob,
//! its MIME type and size, when it was made and last read.
//!
//! - `GET /sidecar/{key}/{kind}`: the sidecar; missing, it is made at once
//!   (from the node's own copy of the bytes, or by asking the agent of the
//!   device that has the file: `?space=&location=&path=&name=`) and then
//!   served. `?make=0` only looks. Not there: `404` with
//!   `{"state": "missing" | "no-generator" | "offline" | "failed"}`.
//! - `PUT /sidecar/{key}/{kind}?version=&mime=&also=`: an agent's sidecar
//!   (`also`: a second key for the same content, its full hash).
//! - `POST /sidecars/missing`: which of these (key, kind) are not made yet.
//! - `GET /sidecars/stats`, `POST /sidecars/purge`.
//!
//! A cache: anything here can go, and is made again when asked for.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use drive_files::blob::BlobStore;
use drive_sidecars::produce::{self, Limits};
use drive_sidecars::{Kind, full_hash_of, kinds_for, valid_key};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use turso::Value;

use crate::rooms::{FetchError, Rooms};

/// A miss ("no generator") is believed this long before it is tried again.
const MISS_FOR_MS: i64 = 24 * 3600 * 1000;
/// MIME types a sidecar may have (the node serves them from its own
/// origin: never anything a browser would run).
const MIMES: &[&str] = &["image/jpeg", "image/png", "application/x-drive-peaks"];

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub source: String,
    pub kind: String,
    pub version: u32,
    pub blob: String,
    pub mime: String,
    pub size: u64,
    pub made: i64,
    pub used: i64,
}

/// What to purge. Filters combine: `kind` narrows everything else.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Purge {
    /// One kind (`thumb-256`) or family (`thumb`, `peaks`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Everything (of `kind`), and every remembered miss.
    #[serde(default)]
    pub all: bool,
    /// Not read for this many days.
    #[serde(default)]
    pub older_than_days: Option<u64>,
    /// Made by an older producer than the current one.
    #[serde(default)]
    pub stale: bool,
    /// Then the least recently read, until the rest fits in this many bytes.
    #[serde(default)]
    pub budget_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Purged {
    pub rows: u64,
    pub blobs: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub count: u64,
    pub bytes: u64,
    pub kinds: HashMap<String, KindStats>,
    pub misses: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct KindStats {
    pub count: u64,
    pub bytes: u64,
}

pub struct Sidecars {
    conn: Mutex<turso::Connection>,
    /// The sidecars' blobs (`data/sidecars`).
    pub blobs: BlobStore,
    /// The node's own blobs (files), read to make sidecars from.
    files: BlobStore,
    files_root: PathBuf,
    /// Producers running at once on the node.
    making: Semaphore,
    /// One producer per content at a time.
    locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Read since the last flush: (key, kind).
    touched: std::sync::Mutex<HashSet<(String, String)>>,
    pub limits: Limits,
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

fn now() -> i64 {
    drive_files::now_ms()
}

impl Sidecars {
    /// The store in `data` (`sidecars.db`, `sidecars/`), making sidecars
    /// from the files kept in `files` (rooted at `files_root`).
    pub async fn open(
        data: &Path,
        files: BlobStore,
        files_root: PathBuf,
    ) -> Result<Sidecars, String> {
        let db = turso::Builder::new_local(&data.join("sidecars.db").to_string_lossy())
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let conn = db.connect().map_err(|e| e.to_string())?;
        for sql in [
            "CREATE TABLE IF NOT EXISTS sidecars (source TEXT NOT NULL, kind TEXT NOT NULL, version INTEGER NOT NULL, \
             blob TEXT NOT NULL, mime TEXT NOT NULL, size INTEGER NOT NULL, made INTEGER NOT NULL, used INTEGER NOT NULL, \
             PRIMARY KEY (source, kind))",
            "CREATE INDEX IF NOT EXISTS sidecars_blob ON sidecars (blob)",
            "CREATE INDEX IF NOT EXISTS sidecars_used ON sidecars (used)",
            "CREATE TABLE IF NOT EXISTS sidecar_misses (source TEXT NOT NULL, kind TEXT NOT NULL, version INTEGER NOT NULL, \
             why TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (source, kind))",
        ] {
            conn.execute(sql, ()).await.map_err(|e| e.to_string())?;
        }
        let blobs = drive_files::storage::StorageConfig::Fs {
            root: data.join("sidecars").to_string_lossy().into_owned(),
        }
        .blobs()
        .map_err(|e| e.to_string())?;
        Ok(Sidecars {
            conn: Mutex::new(conn),
            blobs,
            files,
            files_root,
            making: Semaphore::new(2),
            locks: Default::default(),
            touched: Default::default(),
            limits: Limits::default(),
        })
    }

    pub async fn row(&self, key: &str, kind: Kind) -> Option<Row> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT source, kind, version, blob, mime, size, made, used FROM sidecars WHERE source = ?1 AND kind = ?2",
                vec![Value::Text(key.into()), Value::Text(kind.as_str().into())],
            )
            .await
            .ok()?;
        let r = rows.next().await.ok()??;
        Some(Row {
            source: text(r.get_value(0).ok()?),
            kind: text(r.get_value(1).ok()?),
            version: int(r.get_value(2).ok()?) as u32,
            blob: text(r.get_value(3).ok()?),
            mime: text(r.get_value(4).ok()?),
            size: int(r.get_value(5).ok()?) as u64,
            made: int(r.get_value(6).ok()?),
            used: int(r.get_value(7).ok()?),
        })
    }

    /// Keep a sidecar under every key in `keys` (the same content's full
    /// and sampled keys). Replaces what was there.
    pub async fn put(
        &self,
        keys: &[String],
        kind: Kind,
        version: u32,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        if !MIMES.contains(&mime) {
            return Err(format!("a sidecar cannot be {mime}"));
        }
        let size = bytes.len() as i64;
        let blob = self.blobs.put(bytes).await.map_err(|e| e.to_string())?;
        let t = now();
        let mut replaced = Vec::new();
        {
            let conn = self.conn.lock().await;
            for key in keys.iter().filter(|k| valid_key(k)) {
                let mut old = conn
                    .query(
                        "SELECT blob FROM sidecars WHERE source = ?1 AND kind = ?2",
                        vec![Value::Text(key.clone()), Value::Text(kind.as_str().into())],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if let Ok(Some(r)) = old.next().await
                    && let Ok(v) = r.get_value(0)
                {
                    replaced.push(text(v));
                }
                drop(old);
                conn.execute(
                    "INSERT OR REPLACE INTO sidecars (source, kind, version, blob, mime, size, made, used) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    vec![
                        Value::Text(key.clone()),
                        Value::Text(kind.as_str().into()),
                        Value::Integer(version as i64),
                        Value::Text(blob.clone()),
                        Value::Text(mime.into()),
                        Value::Integer(size),
                        Value::Integer(t),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "DELETE FROM sidecar_misses WHERE source = ?1 AND kind = ?2",
                    vec![Value::Text(key.clone()), Value::Text(kind.as_str().into())],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        replaced.retain(|b| *b != blob);
        self.drop_unused_blobs(replaced).await;
        Ok(blob)
    }

    /// A remembered "no generator" for (key, kind), if it still holds.
    pub async fn miss(&self, key: &str, kind: Kind) -> Option<String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT version, why, at FROM sidecar_misses WHERE source = ?1 AND kind = ?2",
                vec![Value::Text(key.into()), Value::Text(kind.as_str().into())],
            )
            .await
            .ok()?;
        let r = rows.next().await.ok()??;
        let version = int(r.get_value(0).ok()?) as u32;
        let why = text(r.get_value(1).ok()?);
        let at = int(r.get_value(2).ok()?);
        (version >= kind.version() && now() - at < MISS_FOR_MS).then_some(why)
    }

    pub async fn record_miss(&self, keys: &[String], kinds: &[Kind], why: &str) {
        let conn = self.conn.lock().await;
        for key in keys {
            for k in kinds {
                let _ = conn
                    .execute(
                        "INSERT OR REPLACE INTO sidecar_misses (source, kind, version, why, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                        vec![
                            Value::Text(key.clone()),
                            Value::Text(k.as_str().into()),
                            Value::Integer(k.version() as i64),
                            Value::Text(why.into()),
                            Value::Integer(now()),
                        ],
                    )
                    .await;
            }
        }
    }

    /// Whether (key, kind) needs making: no sidecar by the current
    /// producer, and no miss that still holds.
    pub async fn needs(&self, key: &str, kind: Kind) -> bool {
        if self
            .row(key, kind)
            .await
            .is_some_and(|r| r.version >= kind.version())
        {
            return false;
        }
        self.miss(key, kind).await.is_none()
    }

    /// Note a read (the `used` times are written now and then, not on
    /// every read).
    pub fn touch(&self, key: &str, kind: &str) {
        self.touched
            .lock()
            .expect("touched")
            .insert((key.to_string(), kind.to_string()));
    }

    pub async fn flush_used(&self) {
        let touched: Vec<(String, String)> =
            self.touched.lock().expect("touched").drain().collect();
        if touched.is_empty() {
            return;
        }
        let t = now();
        let conn = self.conn.lock().await;
        for (key, kind) in touched {
            let _ = conn
                .execute(
                    "UPDATE sidecars SET used = ?3 WHERE source = ?1 AND kind = ?2",
                    vec![Value::Text(key), Value::Text(kind), Value::Integer(t)],
                )
                .await;
        }
    }

    /// Every row, least recently read first.
    async fn all_rows(&self) -> Result<Vec<Row>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT source, kind, version, blob, mime, size, made, used FROM sidecars ORDER BY used ASC",
                (),
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await.map_err(|e| e.to_string())? {
            let v = |i| r.get_value(i).unwrap_or(Value::Null);
            out.push(Row {
                source: text(v(0)),
                kind: text(v(1)),
                version: int(v(2)) as u32,
                blob: text(v(3)),
                mime: text(v(4)),
                size: int(v(5)) as u64,
                made: int(v(6)),
                used: int(v(7)),
            });
        }
        Ok(out)
    }

    /// How many sidecars, and the bytes their blobs take (a blob shared by
    /// two keys counted once).
    pub async fn stats(&self) -> Result<Stats, String> {
        let rows = self.all_rows().await?;
        let mut seen = HashSet::new();
        let mut st = Stats::default();
        for r in rows {
            if !seen.insert(r.blob.clone()) {
                continue;
            }
            st.count += 1;
            st.bytes += r.size;
            let k = st.kinds.entry(r.kind).or_default();
            k.count += 1;
            k.bytes += r.size;
        }
        let conn = self.conn.lock().await;
        if let Ok(mut m) = conn.query("SELECT COUNT(*) FROM sidecar_misses", ()).await
            && let Ok(Some(r)) = m.next().await
        {
            st.misses = int(r.get_value(0).unwrap_or(Value::Null)) as u64;
        }
        Ok(st)
    }

    /// Drop what `p` names; blobs no row uses any more go too.
    pub async fn purge(&self, p: &Purge) -> Result<Purged, String> {
        let kinds: Option<Vec<String>> = p.kind.as_ref().map(|k| {
            Kind::of_family(k)
                .into_iter()
                .map(|k| k.as_str().to_string())
                .collect()
        });
        let of_kind = |k: &str| kinds.as_ref().is_none_or(|ks| ks.iter().any(|x| x == k));
        let rows = self.all_rows().await?;
        // Saturating: an age too large to count back from now purges
        // nothing (and a `u64` past `i64::MAX` never wraps to the future).
        let cutoff = p.older_than_days.map(|d| {
            const DAY_MS: i64 = 24 * 3600 * 1000;
            now().saturating_sub(i64::try_from(d).unwrap_or(i64::MAX).saturating_mul(DAY_MS))
        });
        let mut drop: Vec<&Row> = Vec::new();
        let mut keep: Vec<&Row> = Vec::new();
        for r in &rows {
            let stale = Kind::parse(&r.kind).is_none_or(|k| r.version < k.version());
            let gone = of_kind(&r.kind)
                && (p.all || cutoff.is_some_and(|c| r.used < c) || (p.stale && stale));
            if gone { drop.push(r) } else { keep.push(r) }
        }
        if let Some(budget) = p.budget_bytes {
            // The rest by blob (one blob may be kept under a file's full
            // and sampled keys: it goes with all of them), least recently
            // read first, until what stays fits.
            let mut blobs: HashMap<&str, (i64, u64, bool)> = HashMap::new();
            for r in &keep {
                let b = blobs.entry(&r.blob).or_insert((r.used, r.size, true));
                b.0 = b.0.max(r.used);
                b.2 &= of_kind(&r.kind);
            }
            let mut total: u64 = blobs.values().map(|b| b.1).sum();
            let mut order: Vec<(&str, i64, u64, bool)> = blobs
                .into_iter()
                .map(|(k, (u, s, ok))| (k, u, s, ok))
                .collect();
            order.sort_by_key(|b| b.1);
            let mut going: HashSet<&str> = HashSet::new();
            for (blob, _, size, ok) in order {
                if total <= budget {
                    break;
                }
                if ok {
                    going.insert(blob);
                    total = total.saturating_sub(size);
                }
            }
            drop.extend(keep.into_iter().filter(|r| going.contains(r.blob.as_str())));
        }
        let mut out = Purged::default();
        let mut blobs = Vec::new();
        {
            let conn = self.conn.lock().await;
            for r in &drop {
                conn.execute(
                    "DELETE FROM sidecars WHERE source = ?1 AND kind = ?2",
                    vec![Value::Text(r.source.clone()), Value::Text(r.kind.clone())],
                )
                .await
                .map_err(|e| e.to_string())?;
                out.rows += 1;
                blobs.push(r.blob.clone());
            }
            if p.all {
                match &kinds {
                    None => {
                        conn.execute("DELETE FROM sidecar_misses", ())
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Some(ks) => {
                        for k in ks {
                            conn.execute(
                                "DELETE FROM sidecar_misses WHERE kind = ?1",
                                vec![Value::Text(k.clone())],
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
        }
        let sizes: HashMap<String, u64> = drop.iter().map(|r| (r.blob.clone(), r.size)).collect();
        for b in self.drop_unused_blobs(blobs).await {
            out.blobs += 1;
            out.bytes += sizes.get(&b).copied().unwrap_or(0);
        }
        tracing::info!(
            sidecars.purged_rows = out.rows,
            sidecars.purged_blobs = out.blobs,
            sidecars.purged_bytes = out.bytes,
            "sidecars purged"
        );
        Ok(out)
    }

    /// Delete those of `blobs` no row names any more; the ones deleted.
    async fn drop_unused_blobs(&self, mut blobs: Vec<String>) -> Vec<String> {
        blobs.sort();
        blobs.dedup();
        let mut gone = Vec::new();
        for b in blobs {
            let used = {
                let conn = self.conn.lock().await;
                match conn
                    .query(
                        "SELECT 1 FROM sidecars WHERE blob = ?1 LIMIT 1",
                        vec![Value::Text(b.clone())],
                    )
                    .await
                {
                    Ok(mut r) => !matches!(r.next().await, Ok(None)),
                    Err(_) => true,
                }
            };
            if !used && self.blobs.delete(&b).await.is_ok() {
                gone.push(b);
            }
        }
        gone
    }

    /// The lock that keeps one producer per content at a time.
    pub(crate) fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("locks");
        locks.retain(|_, l| Arc::strong_count(l) > 1);
        locks.entry(key.to_string()).or_default().clone()
    }

    /// Where a blob the node keeps is on its disk.
    fn file_path(&self, hash: &str) -> PathBuf {
        self.files_root.join(drive_files::blob::key(hash))
    }

    /// Whether the node keeps a file's bytes (by full hash).
    pub async fn keeps(&self, hash: &str) -> bool {
        self.files.has(hash).await.unwrap_or(false)
    }

    /// Make `kinds` of a file the node keeps (by its full hash), named
    /// `name`; kept under its full and sampled keys. `Ok` with the kinds
    /// made.
    pub async fn make_local(
        &self,
        hash: &str,
        name: &str,
        kinds: &[Kind],
    ) -> Result<Vec<Kind>, produce::Error> {
        let path = self.file_path(hash);
        let (name2, kinds2, limits) = (name.to_string(), kinds.to_vec(), self.limits.clone());
        let _permit = self
            .making
            .acquire()
            .await
            .map_err(|e| produce::Error::Failed(e.to_string()))?;
        let started = std::time::Instant::now();
        let p2 = path.clone();
        let made = tokio::task::spawn_blocking(move || {
            let sampled = drive_files::hash::sampled(&p2).ok();
            produce::produce(&p2, &name2, &kinds2, &limits).map(|m| (m, sampled))
        })
        .await
        .map_err(|e| produce::Error::Failed(e.to_string()))?;
        let keys_of = |sampled: Option<String>| {
            let mut keys = vec![drive_sidecars::full_key(hash)];
            keys.extend(sampled.map(|s| drive_sidecars::sampled_key(&s)));
            keys
        };
        match made {
            Ok((made, sampled)) => {
                let keys = keys_of(sampled);
                let mut out = Vec::new();
                for m in made {
                    if self
                        .put(&keys, m.kind, m.kind.version(), m.mime, m.bytes)
                        .await
                        .is_ok()
                    {
                        out.push(m.kind);
                    }
                }
                tracing::info!(
                    sidecar.source = %hash,
                    sidecar.kinds = out.len(),
                    sidecar.ms = started.elapsed().as_millis() as u64,
                    "sidecars made on the node"
                );
                Ok(out)
            }
            Err(e) => {
                if let produce::Error::NoGenerator(w) = &e {
                    self.record_miss(&keys_of(None), kinds, w).await;
                }
                Err(e)
            }
        }
    }
}

// ── routes ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Routes {
    pub sc: Arc<Sidecars>,
    pub rooms: Arc<Rooms>,
}

type Q = Query<HashMap<String, String>>;

pub(crate) fn state(status: StatusCode, s: &str, why: &str) -> Response {
    (status, Json(serde_json::json!({ "state": s, "why": why }))).into_response()
}

pub(crate) async fn serve(sc: &Sidecars, row: &Row) -> Response {
    let Ok(bytes) = sc.blobs.get(&row.blob).await else {
        return state(StatusCode::NOT_FOUND, "missing", "its blob is gone");
    };
    sc.touch(&row.source, &row.kind);
    let mut r = Response::new(axum::body::Body::from(bytes));
    let h = r.headers_mut();
    let mime = if row.mime.starts_with("image/") {
        row.mime.as_str()
    } else {
        "application/octet-stream"
    };
    if let Ok(v) = HeaderValue::from_str(mime) {
        h.insert("content-type", v);
    }
    h.insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=3600"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", row.blob)) {
        h.insert("etag", v);
    }
    r
}

/// `GET /sidecar/{key}/{kind}`.
pub async fn get(
    UrlPath((key, kind)): UrlPath<(String, String)>,
    State(s): State<Routes>,
    Query(q): Q,
) -> Response {
    let (Some(kind), true) = (Kind::parse(&kind), valid_key(&key)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let current = kind.version();
    if let Some(r) = s.sc.row(&key, kind).await
        && r.version >= current
    {
        return serve(&s.sc, &r).await;
    }
    if q.get("make").is_some_and(|m| m == "0") {
        return match s.sc.row(&key, kind).await {
            Some(r) => serve(&s.sc, &r).await,
            None => state(StatusCode::NOT_FOUND, "missing", ""),
        };
    }
    let lock = s.sc.lock_for(&key);
    let _one = lock.lock().await;
    // Made while this request waited for the lock.
    if let Some(r) = s.sc.row(&key, kind).await
        && r.version >= current
    {
        return serve(&s.sc, &r).await;
    }
    if let Some(why) = s.sc.miss(&key, kind).await {
        return state(StatusCode::NOT_FOUND, "no-generator", &why);
    }
    let name = q.get("name").map(String::as_str).unwrap_or_default();
    let all = kinds_for(name);
    if !all.contains(&kind) {
        return state(
            StatusCode::NOT_FOUND,
            "no-generator",
            "nothing derives this from that kind of file",
        );
    }
    // The bytes on the node (a file of a space's tree, or a computer's file
    // kept available), else on the device that has the file.
    let local = full_hash_of(&key)
        .map(str::to_string)
        .or_else(|| q.get("hash").cloned())
        .filter(|h| drive_files::blob::valid_hash(h));
    let outcome = match local {
        Some(h) if s.sc.keeps(&h).await => {
            s.sc.make_local(&h, name, &all)
                .await
                .map(|_| ())
                .map_err(|e| match e {
                    produce::Error::NoGenerator(w) => ("no-generator", w),
                    produce::Error::Failed(w) => ("failed", w),
                })
        }
        _ => match (q.get("space"), q.get("location"), q.get("path")) {
            (Some(space), Some(loc), Some(path)) => {
                let kinds = all.iter().map(|k| k.as_str().to_string()).collect();
                match s.rooms.make_sidecars(space, loc, path, kinds, false).await {
                    Ok(_) => Ok(()),
                    Err(FetchError::Refused(w)) if w.starts_with("no generator") => {
                        s.sc.record_miss(std::slice::from_ref(&key), &all, &w).await;
                        Err(("no-generator", w))
                    }
                    Err(FetchError::Refused(w)) => Err(("failed", w)),
                    Err(FetchError::Offline(_)) => {
                        Err(("offline", "the device that has this file is offline".into()))
                    }
                    Err(FetchError::Timeout) => {
                        Err(("offline", "its device did not answer".into()))
                    }
                    Err(FetchError::NoLocation) => Err(("missing", "no such location".into())),
                }
            }
            _ => Err(("missing", "no producer can reach the bytes".into())),
        },
    };
    if let Some(r) = s.sc.row(&key, kind).await {
        return serve(&s.sc, &r).await;
    }
    match outcome {
        Ok(()) => state(StatusCode::NOT_FOUND, "missing", "made, but not this kind"),
        Err((st, why)) => {
            if st == "failed" {
                tracing::warn!(sidecar.source = %key, sidecar.kind = kind.as_str(), sidecar.why = %why, "a sidecar could not be made");
            }
            state(StatusCode::NOT_FOUND, st, &why)
        }
    }
}

/// `PUT /sidecar/{key}/{kind}?version=&mime=&also=`: an agent's sidecar.
pub async fn put(
    UrlPath((key, kind)): UrlPath<(String, String)>,
    State(s): State<Routes>,
    Query(q): Q,
    body: Bytes,
) -> Response {
    let (Some(kind), true) = (Kind::parse(&kind), valid_key(&key)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let version = q
        .get("version")
        .and_then(|v| v.parse().ok())
        .unwrap_or(kind.version());
    let mime = q.get("mime").map(String::as_str).unwrap_or_default();
    let mut keys = vec![key];
    keys.extend(q.get("also").filter(|a| valid_key(a)).cloned());
    match s.sc.put(&keys, kind, version, mime, body.to_vec()).await {
        Ok(blob) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "blob": blob })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(sidecar.kind = kind.as_str(), error = %e, "a sidecar was refused");
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct MissingReq {
    pub keys: Vec<String>,
    pub kinds: Vec<String>,
}

/// `POST /sidecars/missing`: which (key, kind) still need making.
pub async fn missing(
    State(s): State<Routes>,
    Json(m): Json<MissingReq>,
) -> Json<serde_json::Value> {
    let kinds: Vec<Kind> = m.kinds.iter().filter_map(|k| Kind::parse(k)).collect();
    let mut out = Vec::new();
    for key in m.keys.iter().filter(|k| valid_key(k)).take(5000) {
        for k in &kinds {
            if s.sc.needs(key, *k).await {
                out.push((key.clone(), k.as_str()));
            }
        }
    }
    Json(serde_json::json!({ "missing": out }))
}

#[derive(Deserialize)]
pub struct MissReq {
    pub key: String,
    pub why: String,
}

/// `POST /sidecars/miss`: an agent found nothing that reads this content.
pub async fn miss(State(s): State<Routes>, Json(m): Json<MissReq>) -> StatusCode {
    if !valid_key(&m.key) {
        return StatusCode::BAD_REQUEST;
    }
    s.sc.record_miss(&[m.key], &Kind::ALL, &m.why).await;
    StatusCode::NO_CONTENT
}

/// `GET /sidecars/stats`.
pub async fn stats(State(s): State<Routes>) -> Response {
    match s.sc.stats().await {
        Ok(st) => Json(st).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "sidecar stats failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /sidecars/purge` with a [`Purge`].
pub async fn purge(State(s): State<Routes>, Json(p): Json<Purge>) -> Response {
    match s.sc.purge(&p).await {
        Ok(out) => Json(out).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "a sidecar purge failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── background ──────────────────────────────────────────────────────────

/// Write the read times every minute.
pub async fn flush_loop(sc: Arc<Sidecars>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        sc.flush_used().await;
    }
}

/// Now and then: make what is missing for the
/// files the node keeps (every space's tree), one file at a time, at most
/// `per_pass` files a pass, never more than the store's limits allow.
pub async fn backfill(sc: Arc<Sidecars>, rooms: Arc<Rooms>, every: Duration, per_pass: usize) {
    let mut tick = tokio::time::interval(every);
    tick.tick().await;
    loop {
        tick.tick().await;
        let made = backfill_once(&sc, &rooms, per_pass).await;
        if made > 0 {
            tracing::info!(
                sidecars.backfilled = made,
                "sidecars made in the background"
            );
        }
    }
}

/// One pass of [`backfill`]: the number of files given sidecars.
pub async fn backfill_once(sc: &Sidecars, rooms: &Rooms, per_pass: usize) -> usize {
    let spaces: Vec<String> = rooms
        .store
        .list()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|(id, _)| id.strip_suffix("/drive:files").map(str::to_string))
        .collect();
    let mut done = 0;
    let mut seen = HashSet::new();
    for space in spaces {
        let files = rooms
            .read(&drive_files::files_doc(&space), drive_files::tree::files)
            .await;
        for (_, e) in files {
            if done >= per_pass {
                return done;
            }
            if e.dir || e.hash.is_empty() || !seen.insert(e.hash.clone()) {
                continue;
            }
            let kinds = kinds_for(&e.name);
            if kinds.is_empty() {
                continue;
            }
            let mut need = false;
            for k in &kinds {
                need |= sc.needs(&e.hash, *k).await;
            }
            if !need || !sc.keeps(&e.hash).await {
                continue;
            }
            let lock = sc.lock_for(&e.hash);
            let _one = lock.lock().await;
            let _ = sc.make_local(&e.hash, &e.name, &kinds).await;
            done += 1;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(dir: &Path) -> Sidecars {
        let files = drive_files::storage::StorageConfig::Fs {
            root: dir.join("blobs").to_string_lossy().into_owned(),
        }
        .blobs()
        .unwrap();
        Sidecars::open(dir, files, dir.join("blobs")).await.unwrap()
    }

    fn key(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[tokio::test]
    async fn keeps_finds_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let sc = store(dir.path()).await;
        let (a, sa) = (key('a'), format!("s{}", key('b')));
        assert!(sc.needs(&a, Kind::Thumb256).await);
        let blob = sc
            .put(
                &[a.clone(), sa.clone()],
                Kind::Thumb256,
                1,
                "image/png",
                vec![1, 2, 3],
            )
            .await
            .unwrap();
        // Under both keys, one blob.
        assert_eq!(sc.row(&a, Kind::Thumb256).await.unwrap().blob, blob);
        assert_eq!(sc.row(&sa, Kind::Thumb256).await.unwrap().blob, blob);
        assert!(!sc.needs(&sa, Kind::Thumb256).await);
        let st = sc.stats().await.unwrap();
        assert_eq!((st.count, st.bytes), (1, 3));
        // An older producer's sidecar is stale: needed again.
        sc.put(
            &[a.clone()],
            Kind::Peaks,
            0,
            "application/x-drive-peaks",
            vec![9; 10],
        )
        .await
        .unwrap();
        assert!(sc.needs(&a, Kind::Peaks).await);
        // Nothing a browser would run.
        assert!(
            sc.put(&[a.clone()], Kind::Thumb256, 1, "text/html", vec![1])
                .await
                .is_err()
        );
        // Replacing drops the old blob once nothing uses it.
        let new = sc
            .put(
                &[a.clone(), sa.clone()],
                Kind::Thumb256,
                1,
                "image/png",
                vec![4, 5],
            )
            .await
            .unwrap();
        assert!(!sc.blobs.has(&blob).await.unwrap());
        assert!(sc.blobs.has(&new).await.unwrap());
    }

    #[tokio::test]
    async fn misses_hold_until_purged() {
        let dir = tempfile::tempdir().unwrap();
        let sc = store(dir.path()).await;
        let a = key('c');
        sc.record_miss(&[a.clone()], &[Kind::Thumb256], "no ffmpeg")
            .await;
        assert_eq!(
            sc.miss(&a, Kind::Thumb256).await.as_deref(),
            Some("no ffmpeg")
        );
        assert!(!sc.needs(&a, Kind::Thumb256).await);
        sc.purge(&Purge {
            all: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(sc.needs(&a, Kind::Thumb256).await);
    }

    #[tokio::test]
    async fn a_huge_age_purges_nothing_and_never_overflows() {
        let dir = tempfile::tempdir().unwrap();
        let sc = store(dir.path()).await;
        sc.put(&[key('1')], Kind::Thumb256, 1, "image/jpeg", vec![1; 10])
            .await
            .unwrap();
        // Days that overflow in milliseconds, and ones that do not fit an i64.
        for days in [i64::MAX as u64 / 1000, i64::MAX as u64 + 1, u64::MAX] {
            let out = sc
                .purge(&Purge {
                    older_than_days: Some(days),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(out.rows, 0, "{days} days");
        }
        assert_eq!(sc.stats().await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn purges_by_kind_age_staleness_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sc = store(dir.path()).await;
        for (i, c) in ['1', '2', '3', '4'].into_iter().enumerate() {
            sc.put(
                &[key(c)],
                Kind::Thumb256,
                1,
                "image/jpeg",
                vec![i as u8; 100],
            )
            .await
            .unwrap();
            sc.put(
                &[key(c)],
                Kind::Peaks,
                1,
                "application/x-drive-peaks",
                vec![i as u8 + 10; 50],
            )
            .await
            .unwrap();
        }
        // Peaks only.
        let out = sc
            .purge(&Purge {
                kind: Some("peaks".into()),
                all: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            out,
            Purged {
                rows: 4,
                blobs: 4,
                bytes: 200
            }
        );
        assert_eq!(sc.stats().await.unwrap().count, 4);
        // Nothing is older than a day yet.
        let out = sc
            .purge(&Purge {
                older_than_days: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.rows, 0);
        // Read '4' recently and '1' long ago: the budget keeps the newest.
        {
            let conn = sc.conn.lock().await;
            for (c, used) in [('1', 1i64), ('2', 2), ('3', 3), ('4', now())] {
                conn.execute(
                    "UPDATE sidecars SET used = ?2 WHERE source = ?1",
                    vec![Value::Text(key(c)), Value::Integer(used)],
                )
                .await
                .unwrap();
            }
        }
        // '4' is also kept under a second key (its sampled one), read long
        // ago: the blob counts as read when either key was.
        let s4 = format!("s{}", key('4'));
        sc.put(
            &[key('4'), s4.clone()],
            Kind::Thumb256,
            1,
            "image/jpeg",
            vec![3u8; 100],
        )
        .await
        .unwrap();
        {
            let conn = sc.conn.lock().await;
            conn.execute(
                "UPDATE sidecars SET used = 0 WHERE source = ?1",
                vec![Value::Text(s4.clone())],
            )
            .await
            .unwrap();
        }
        let out = sc
            .purge(&Purge {
                budget_bytes: Some(250),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.rows, 2);
        assert_eq!(out.bytes, 200);
        assert!(sc.row(&s4, Kind::Thumb256).await.is_some());
        assert!(sc.row(&key('1'), Kind::Thumb256).await.is_none());
        assert!(sc.row(&key('2'), Kind::Thumb256).await.is_none());
        assert!(sc.row(&key('4'), Kind::Thumb256).await.is_some());
        // Older than a day: '3' (read at t = 3 ms), and the old second key
        // of '4' (its blob stays: its first key was read just now).
        let out = sc
            .purge(&Purge {
                older_than_days: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!((out.rows, out.blobs), (2, 1));
        assert_eq!(sc.stats().await.unwrap().count, 1);
        // Stale: an older producer's.
        sc.put(&[key('5')], Kind::Thumb1024, 0, "image/jpeg", vec![7; 10])
            .await
            .unwrap();
        let out = sc
            .purge(&Purge {
                stale: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.rows, 1);
        // Everything; then it is made again on demand (the next test).
        let out = sc
            .purge(&Purge {
                all: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.rows, 1);
        assert!(sc.blobs.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn makes_from_the_nodes_own_files_and_again_after_a_purge() {
        let dir = tempfile::tempdir().unwrap();
        let sc = store(dir.path()).await;
        // A small WAV the node keeps.
        let mut wav = Vec::new();
        let frames = 8000u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for v in [
            16u32.to_le_bytes().to_vec(),
            1u16.to_le_bytes().to_vec(),
            1u16.to_le_bytes().to_vec(),
            8000u32.to_le_bytes().to_vec(),
            16000u32.to_le_bytes().to_vec(),
            2u16.to_le_bytes().to_vec(),
            16u16.to_le_bytes().to_vec(),
        ] {
            wav.extend_from_slice(&v);
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(frames * 2).to_le_bytes());
        for f in 0..frames {
            wav.extend_from_slice(&(((f as f32 * 0.1).sin() * 16000.0) as i16).to_le_bytes());
        }
        let h = sc.files.put(wav).await.unwrap();
        let made = sc
            .make_local(&h, "memo.wav", &kinds_for("memo.wav"))
            .await
            .unwrap();
        assert_eq!(made.len(), 3);
        let sampled = drive_files::hash::sampled(&sc.file_path(&h)).unwrap();
        // Under the full and the sampled key.
        let p = sc
            .row(&drive_sidecars::sampled_key(&sampled), Kind::Peaks)
            .await
            .unwrap();
        let peaks =
            drive_sidecars::peaks::Peaks::decode(&sc.blobs.get(&p.blob).await.unwrap()).unwrap();
        assert_eq!(peaks.frames, 8000);
        sc.purge(&Purge {
            all: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(sc.needs(&h, Kind::Peaks).await);
        sc.make_local(&h, "memo.wav", &[Kind::Peaks]).await.unwrap();
        assert!(!sc.needs(&h, Kind::Peaks).await);
        // Nothing reads it: remembered, not tried again.
        let z = sc.files.put(b"PK\x03\x04 zip".to_vec()).await.unwrap();
        assert!(matches!(
            sc.make_local(&z, "a.heic", &[Kind::Thumb256]).await,
            Err(produce::Error::NoGenerator(_))
        ));
        assert!(!sc.needs(&z, Kind::Thumb256).await);
    }
}
