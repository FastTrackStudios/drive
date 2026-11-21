//! Backups of every document the node keeps: one compressed file holding
//! each document's latest snapshot, written every day into the data
//! folder's `backups/` (the newest few kept) and, when a backup storage is
//! configured (the R2 bucket, Starcommand), there too. File indexes are not
//! in it: the computers publish them again. Blobs are not either: they are
//! content kept by the computers and storages that hold it.
//!
//! Format: `DRIVEBK1`, then per document a u32 id length, the id, a u64
//! snapshot length and the snapshot; all of it zstd-compressed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drive_files::storage::StorageConfig;

use crate::store::Store;

const MAGIC: &[u8] = b"DRIVEBK1";

/// Every document's snapshot, in one compressed file's bytes.
pub async fn archive(store: &Store) -> Result<(Vec<u8>, usize), String> {
    let mut raw = MAGIC.to_vec();
    let ids = store.list().await.map_err(|e| e.to_string())?;
    let mut n = 0;
    for (id, _) in ids {
        let Some(snap) = store.load(&id).await.map_err(|e| e.to_string())? else {
            continue;
        };
        raw.extend_from_slice(&(id.len() as u32).to_le_bytes());
        raw.extend_from_slice(id.as_bytes());
        raw.extend_from_slice(&(snap.len() as u64).to_le_bytes());
        raw.extend_from_slice(&snap);
        n += 1;
    }
    let packed = zstd::encode_all(raw.as_slice(), 9).map_err(|e| e.to_string())?;
    Ok((packed, n))
}

/// The documents in a backup file's bytes.
pub fn unpack(packed: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let raw = zstd::decode_all(packed).map_err(|e| format!("not a backup: {e}"))?;
    let mut rest = raw.strip_prefix(MAGIC).ok_or("not a Drive backup")?;
    let mut out = Vec::new();
    let take = |r: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        if r.len() < n {
            return Err("the backup is cut short".into());
        }
        let (a, b) = r.split_at(n);
        *r = b;
        Ok(a.to_vec())
    };
    while !rest.is_empty() {
        let len =
            u32::from_le_bytes(take(&mut rest, 4)?.try_into().map_err(|_| "bad length")?) as usize;
        let id =
            String::from_utf8(take(&mut rest, len)?).map_err(|_| "a document id is not text")?;
        let len =
            u64::from_le_bytes(take(&mut rest, 8)?.try_into().map_err(|_| "bad length")?) as usize;
        out.push((id, take(&mut rest, len)?));
    }
    Ok(out)
}

/// Merge a backup's documents into the store (nothing newer is lost:
/// snapshots merge, they do not replace).
pub async fn restore(store: &Store, packed: &[u8]) -> Result<usize, String> {
    let docs = unpack(packed)?;
    for (id, snap) in &docs {
        let doc = loro::LoroDoc::new();
        if let Some(have) = store.load(id).await.map_err(|e| e.to_string())? {
            doc.import(&have).map_err(|e| format!("{id}: {e}"))?;
        }
        doc.import(snap).map_err(|e| format!("{id}: {e}"))?;
        let merged = doc
            .export(loro::ExportMode::Snapshot)
            .map_err(|e| e.to_string())?;
        store.save(id, merged).await.map_err(|e| e.to_string())?;
    }
    Ok(docs.len())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Write one backup now: into `dir` (keeping the newest `keep`) and into
/// the backup storage if there is one. Returns the file written.
pub async fn write(
    store: &Store,
    dir: &Path,
    keep: usize,
    to: Option<&StorageConfig>,
) -> Result<PathBuf, String> {
    let (packed, n) = archive(store).await?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = format!("drive-{}.drvbk", now_secs());
    let path = dir.join(&name);
    std::fs::write(&path, &packed).map_err(|e| e.to_string())?;
    let mut all: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "drvbk"))
        .collect();
    all.sort();
    while all.len() > keep {
        let _ = std::fs::remove_file(all.remove(0));
    }
    if let Some(cfg) = to {
        let op = cfg.operator().map_err(|e| e.to_string())?;
        op.write(&format!("backups/{name}"), packed.clone())
            .await
            .map_err(|e| format!("the backup storage: {e}"))?;
    }
    tracing::info!(backup.file = %name, backup.docs = n, backup.bytes = packed.len(), backup.remote = to.is_some(), "backed up every document");
    Ok(path)
}

/// Back up once a day (and at start, when the newest backup is older).
pub async fn daily(
    store: std::sync::Arc<crate::rooms::Rooms>,
    dir: PathBuf,
    to: Option<StorageConfig>,
) {
    loop {
        let newest = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok()?.modified().ok())
            .max();
        let due =
            newest.is_none_or(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(24 * 3600));
        if due {
            store.flush_now().await;
            if let Err(e) = write(&store.store, &dir, 14, to.as_ref()).await {
                tracing::warn!(error = %e, "a backup failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
