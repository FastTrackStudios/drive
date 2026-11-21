//! Drive's files.
//!
//! - [`tree`]: one space's file tree, a Loro tree in the document
//!   `drive:files:<space>`: folders and files with their names, sizes,
//!   times and content hashes, never their bytes.
//! - [`blob`]: bytes by their BLAKE3 hash, in any storage OpenDAL reaches.
//! - [`hash`]: BLAKE3, sampled (to find likely duplicates fast) and full
//!   (a blob's name, checked before anything is trusted).
//! - [`space`]: the space's registry (`drive:space:<space>`): its devices,
//!   storages, pairing invites, and which device holds which blob.
//! - [`storage`]: a storage's full configuration, credentials included, as
//!   kept on the one device that runs it (never synced).

pub mod blob;
pub mod hash;
pub mod space;
pub mod storage;
pub mod tree;

/// The file tree's document id for a space.
pub fn files_doc(space: &str) -> String {
    format!("drive:files:{space}")
}

/// The registry's document id for a space.
pub fn space_doc(space: &str) -> String {
    format!("drive:space:{space}")
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
