//! Where rooms keep their documents: a snapshot per document.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;

use drive_proto::DocId;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A store of documents' snapshots (a node's database, an app's folder).
pub trait Store: Send + Sync + 'static {
    /// A document's last saved snapshot, if it has one.
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>>;
    /// Replace a document's snapshot.
    fn save<'a>(&'a self, id: &'a str, snapshot: Vec<u8>) -> BoxFuture<'a, Result<(), String>>;
    /// Every stored document, with its snapshot's size.
    fn list(&self) -> BoxFuture<'_, Result<Vec<(DocId, u64)>, String>>;
}

/// A store in memory (tests, and peers that keep nothing).
#[derive(Default)]
pub struct MemStore(Mutex<HashMap<DocId, Vec<u8>>>);

impl Store for MemStore {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
        let v = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        Box::pin(async move { Ok(v) })
    }
    fn save<'a>(&'a self, id: &'a str, snapshot: Vec<u8>) -> BoxFuture<'a, Result<(), String>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), snapshot);
        Box::pin(async { Ok(()) })
    }
    fn list(&self) -> BoxFuture<'_, Result<Vec<(DocId, u64)>, String>> {
        let v = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.len() as u64))
            .collect();
        Box::pin(async move { Ok(v) })
    }
}

/// A folder of snapshots, a file per document, named as `drive-native`
/// names a native app's copies (lowercase letters, digits and `-` as they
/// are, every other byte `%` and its hex, then `.loro`).
pub struct DirStore {
    dir: PathBuf,
}

impl DirStore {
    pub fn new(dir: impl Into<PathBuf>) -> DirStore {
        DirStore { dir: dir.into() }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(file_name(id))
    }
}

/// The file a document is kept in.
pub fn file_name(id: &str) -> String {
    let mut name = String::with_capacity(id.len() + 5);
    for b in id.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' {
            name.push(b as char);
        } else {
            name.push_str(&format!("%{b:02x}"));
        }
    }
    name + ".loro"
}

/// The document a file keeps ([`file_name`] backwards).
fn id_of(file: &str) -> Option<DocId> {
    let stem = file.strip_suffix(".loro")?;
    let mut bytes = Vec::with_capacity(stem.len());
    let mut it = stem.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

impl Store for DirStore {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            match tokio::fs::read(self.path(id)).await {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn save<'a>(&'a self, id: &'a str, snapshot: Vec<u8>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.dir)
                .await
                .map_err(|e| e.to_string())?;
            // Whole or not at all: a temporary file, renamed over it.
            let path = self.path(id);
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
            let written = async {
                tokio::fs::write(&tmp, &snapshot).await?;
                tokio::fs::rename(&tmp, &path).await
            }
            .await;
            if written.is_err() {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            written.map_err(|e| e.to_string())
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<(DocId, u64)>, String>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let mut dir = match tokio::fs::read_dir(&self.dir).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
                Err(e) => return Err(e.to_string()),
            };
            while let Ok(Some(e)) = dir.next_entry().await {
                let Some(id) = e.file_name().to_str().and_then(id_of) else {
                    continue;
                };
                let len = e.metadata().await.map(|m| m.len()).unwrap_or(0);
                out.push((id, len));
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_go_back_to_their_ids() {
        for id in [
            "home/studio:library",
            "comments:loc:l1:Song A/Mix v2.wav",
            "a-b",
        ] {
            assert_eq!(id_of(&file_name(id)).as_deref(), Some(id));
        }
        assert_eq!(id_of("x.tmp"), None);
    }

    #[tokio::test]
    async fn a_folder_keeps_and_lists_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let s = DirStore::new(dir.path().join("docs"));
        assert_eq!(s.load("home/a:b").await.unwrap(), None);
        assert!(s.list().await.unwrap().is_empty());
        s.save("home/a:b", vec![1, 2, 3]).await.unwrap();
        assert_eq!(s.load("home/a:b").await.unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(s.list().await.unwrap(), vec![("home/a:b".to_string(), 3)]);
    }
}
