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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blobs_round_trip_and_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(opendal::services::Fs::default().root(&dir.path().to_string_lossy())).unwrap();
        let s = BlobStore::new(op.clone());
        let h = s.put(b"a stem".to_vec()).await.unwrap();
        assert!(s.has(&h).await.unwrap());
        assert_eq!(s.get(&h).await.unwrap(), b"a stem");
        assert_eq!(s.list().await.unwrap(), vec![h.clone()]);
        // Bytes changed behind its back are refused.
        op.write(&key(&h), b"tampered".to_vec()).await.unwrap();
        assert!(matches!(s.get(&h).await, Err(Error::Corrupt(_))));
        assert!(matches!(s.put_checked(&h, b"wrong".to_vec()).await, Err(Error::Corrupt(_))));
    }
}
