//! Blobs: bytes named by their BLAKE3 hash, in any storage OpenDAL can
//! reach (`blobs/ab/cd/<hash>`). The same bytes are one blob, wherever
//! they come from; reading checks the hash, so no storage is trusted.

use opendal::Operator;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage: {0}")]
    Storage(#[from] opendal::Error),
    #[error("blob {0} did not match its hash")]
    Corrupt(String),
    #[error("not a blob hash: {0}")]
    BadHash(String),
}

pub fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where a blob lives in a storage.
pub fn key(hash: &str) -> String {
    format!("blobs/{}/{}/{}", &hash[..2], &hash[2..4], hash)
}

/// One storage, holding blobs.
#[derive(Clone, Debug)]
pub struct BlobStore {
    op: Operator,
}

impl BlobStore {
    pub fn new(op: Operator) -> BlobStore {
        BlobStore { op }
    }

    pub async fn has(&self, hash: &str) -> Result<bool, Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        Ok(self.op.exists(&key(hash)).await?)
    }

    /// Keep these bytes; returns their hash. Already there: nothing written.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<String, Error> {
        let hash = crate::hash::of_bytes(&bytes);
        if !self.op.exists(&key(&hash)).await? {
            self.op.write(&key(&hash), bytes).await?;
        }
        Ok(hash)
    }

    /// Keep bytes that should have this hash; refuse them if they do not.
    pub async fn put_checked(&self, hash: &str, bytes: Vec<u8>) -> Result<(), Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        if crate::hash::of_bytes(&bytes) != hash {
            return Err(Error::Corrupt(hash.into()));
        }
        if !self.op.exists(&key(hash)).await? {
            self.op.write(&key(hash), bytes).await?;
        }
        Ok(())
    }

    /// A blob's bytes, checked against its hash.
    pub async fn get(&self, hash: &str) -> Result<Vec<u8>, Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        let bytes = self.op.read(&key(hash)).await?.to_vec();
        if crate::hash::of_bytes(&bytes) != hash {
            return Err(Error::Corrupt(hash.into()));
        }
        Ok(bytes)
    }

    /// Write a blob as its bytes arrive (a file fetched from a device):
    /// into a temporary key first, renamed to the blob's own only once the
    /// bytes match the hash, so a blob is never there half-written.
    pub async fn writer(&self, hash: &str) -> Result<BlobWriter, Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        let tmp = format!("tmp/{hash}.{}", crate::space::new_id());
        let w = self.op.writer(&tmp).await?;
        Ok(BlobWriter {
            op: self.op.clone(),
            w,
            tmp,
            hash: hash.into(),
            hasher: blake3::Hasher::new(),
        })
    }

    /// A blob's size, if this store has it.
    pub async fn size(&self, hash: &str) -> Result<Option<u64>, Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        match self.op.stat(&key(hash)).await {
            Ok(m) => Ok(Some(m.content_length())),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Bytes `start..end` of a blob as the storage has them. Not checked
    /// against the hash (a range cannot be without the blob's outboard):
    /// for a store whose blobs were checked as they were written (the
    /// node's own), so a player can seek without reading the whole blob.
    pub async fn read_range(
        &self,
        hash: &str,
        start: u64,
        end: u64,
    ) -> Result<bytes::Bytes, Error> {
        if !valid_hash(hash) {
            return Err(Error::BadHash(hash.into()));
        }
        Ok(self
            .op
            .read_with(&key(hash))
            .range(start..end)
            .await?
            .to_bytes())
    }

    pub async fn delete(&self, hash: &str) -> Result<(), Error> {
        Ok(self.op.delete(&key(hash)).await?)
    }

    /// Every blob hash this storage holds.
    pub async fn list(&self) -> Result<Vec<String>, Error> {
        let entries = self.op.list_with("blobs/").recursive(true).await?;
        Ok(entries
            .into_iter()
            .filter_map(|e| e.name().rsplit('/').next().map(str::to_string))
            .filter(|n| valid_hash(n))
            .collect())
    }
}

/// A blob being written ([`BlobStore::writer`]).
pub struct BlobWriter {
    op: Operator,
    w: opendal::Writer,
    tmp: String,
    hash: String,
    hasher: blake3::Hasher,
}

impl BlobWriter {
    pub async fn write(&mut self, bytes: bytes::Bytes) -> Result<(), Error> {
        self.hasher.update(&bytes);
        self.w.write(bytes).await?;
        Ok(())
    }

    /// Keep it, if the bytes written are the blob's.
    pub async fn finish(mut self) -> Result<(), Error> {
        self.w.close().await?;
        if self.hasher.finalize().to_hex().as_str() != self.hash {
            let _ = self.op.delete(&self.tmp).await;
            return Err(Error::Corrupt(self.hash));
        }
        let key = key(&self.hash);
        if self.op.exists(&key).await? {
            let _ = self.op.delete(&self.tmp).await;
            return Ok(());
        }
        if self.op.rename(&self.tmp, &key).await.is_err() {
            self.op.copy(&self.tmp, &key).await?;
            let _ = self.op.delete(&self.tmp).await;
        }
        Ok(())
    }

    /// Drop what was written (the bytes stopped coming).
    pub async fn abandon(mut self) {
        let _ = self.w.abort().await;
        let _ = self.op.delete(&self.tmp).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blobs_round_trip_and_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let op =
            Operator::new(opendal::services::Fs::default().root(&dir.path().to_string_lossy()))
                .unwrap();
        let s = BlobStore::new(op.clone());
        let h = s.put(b"a stem".to_vec()).await.unwrap();
        assert!(s.has(&h).await.unwrap());
        assert_eq!(s.get(&h).await.unwrap(), b"a stem");
        assert_eq!(s.list().await.unwrap(), vec![h.clone()]);
        // Bytes changed behind its back are refused.
        op.write(&key(&h), b"tampered".to_vec()).await.unwrap();
        assert!(matches!(s.get(&h).await, Err(Error::Corrupt(_))));
        assert!(matches!(
            s.put_checked(&h, b"wrong".to_vec()).await,
            Err(Error::Corrupt(_))
        ));
    }

    #[tokio::test]
    async fn blobs_written_as_they_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let op =
            Operator::new(opendal::services::Fs::default().root(&dir.path().to_string_lossy()))
                .unwrap();
        let s = BlobStore::new(op);
        let bytes = b"a long take".to_vec();
        let h = crate::hash::of_bytes(&bytes);
        let mut w = s.writer(&h).await.unwrap();
        w.write(bytes[..4].to_vec().into()).await.unwrap();
        w.write(bytes[4..].to_vec().into()).await.unwrap();
        w.finish().await.unwrap();
        assert_eq!(s.get(&h).await.unwrap(), bytes);
        // Half a blob is never kept; nor wrong bytes.
        let other = crate::hash::of_bytes(b"other");
        let mut w = s.writer(&other).await.unwrap();
        w.write(b"oth".to_vec().into()).await.unwrap();
        w.abandon().await;
        assert!(!s.has(&other).await.unwrap());
        let mut w = s.writer(&other).await.unwrap();
        w.write(b"nope!".to_vec().into()).await.unwrap();
        assert!(matches!(w.finish().await, Err(Error::Corrupt(_))));
        assert!(!s.has(&other).await.unwrap());
        assert_eq!(s.list().await.unwrap(), vec![h]);
    }

    #[tokio::test]
    async fn ranges_and_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let op =
            Operator::new(opendal::services::Fs::default().root(&dir.path().to_string_lossy()))
                .unwrap();
        let s = BlobStore::new(op);
        let h = s.put(b"0123456789".to_vec()).await.unwrap();
        assert_eq!(s.size(&h).await.unwrap(), Some(10));
        assert_eq!(s.size(&"a".repeat(64)).await.unwrap(), None);
        assert_eq!(&s.read_range(&h, 2, 5).await.unwrap()[..], b"234");
    }
}
