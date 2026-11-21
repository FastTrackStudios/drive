//! Drive's files.
//!
//! - [`tree`]: one space's file tree, a Loro tree in the document
//!   `drive:files:<space>`: folders and files with their names, sizes,
//!   times and content hashes, never their bytes.
//! - [`blob`]: bytes by their BLAKE3 hash, in any storage OpenDAL reaches.
//! - [`hash`]: BLAKE3, sampled (to find likely duplicates fast) and full
//!   (a blob's name, checked before anything is trusted).
//! - [`index`]: a location's index (every file in a folder a device
//!   keeps, where it is), built by walking it and published as one blob.
//! - [`projects`]: projects whose parts are folders on any computer, and
//!   what of them is kept on the node to be had from anywhere.
//! - [`protect`]: copies across every index, and policies (3-2-1).
//! - [`space`]: the space's registry (`drive:space:<space>`): its devices,
//!   storages, pairing invites, and which device holds which blob.
//! - [`storage`]: a storage's full configuration, credentials included, as
//!   kept on the one device that runs it (never synced).

#[cfg(feature = "storage")]
pub mod blob;
pub mod hash;
#[cfg(feature = "storage")]
pub mod index;
pub mod placement;
pub mod projects;
pub mod protect;
pub mod space;
#[cfg(feature = "storage")]
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

/// Milliseconds since the Unix epoch (the browser's clock on the web,
/// where `SystemTime` is not available).
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Some randomness for ids (the browser's on the web).
pub(crate) fn entropy() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Math::random() * u64::MAX as f64) as u64
            ^ (js_sys::Math::random() * 4_294_967_296.0) as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}
