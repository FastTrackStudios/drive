//! Sidecars: data derived from a file's content (thumbnails, waveform
//! peaks), stored as blobs and keyed by **(the source's content key, the
//! kind, the producer's version)**, so one is made once per content and
//! follows the file across moves, renames, copies and devices.
//!
//! - [`Kind`]: what a sidecar is, and the version of its producer (a newer
//!   producer makes them again).
//! - [`source_key`]: the content key a sidecar hangs off: a file's full
//!   BLAKE3 hash, or `s` + its sampled hash for a file only an index knows.
//! - [`generator_for`]: which producer can read a file, by its name.
//! - [`peaks`]: the waveform format (built natively, read in the browser).
//! - `produce` (feature): the native producers.
//!
//! The store itself (which sidecars exist, their blobs, purging) is the
//! node's (`drive-node`'s `sidecars` module); see `docs/sidecars.md`.

use serde::{Deserialize, Serialize};

pub mod peaks;
#[cfg(feature = "produce")]
pub mod produce;

/// What a sidecar is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Kind {
    /// A small picture, about 256 px on its longer side.
    Thumb256,
    /// A large picture, about 1024 px on its longer side.
    Thumb1024,
    /// An audio file's waveform peaks ([`peaks`]).
    Peaks,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Thumb256, Kind::Thumb1024, Kind::Peaks];

    /// Its name in URLs and the store.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Thumb256 => "thumb-256",
            Kind::Thumb1024 => "thumb-1024",
            Kind::Peaks => "peaks",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The version of the producer that makes it now. Sidecars made by an
    /// older one are made again when asked for (and purged in time).
    pub fn version(self) -> u32 {
        match self {
            Kind::Thumb256 | Kind::Thumb1024 => 1,
            Kind::Peaks => 1,
        }
    }

    /// The broader family a command line names (`--kind thumb|peaks`).
    pub fn family(self) -> &'static str {
        match self {
            Kind::Thumb256 | Kind::Thumb1024 => "thumb",
            Kind::Peaks => "peaks",
        }
    }

    /// The kinds of a family (`thumb`, `peaks`), or of a single kind name.
    pub fn of_family(f: &str) -> Vec<Kind> {
        match Kind::parse(f) {
            Some(k) => vec![k],
            None => Kind::ALL.into_iter().filter(|k| k.family() == f).collect(),
        }
    }
}

/// The content key of a file whose full BLAKE3 hash is known.
pub fn full_key(hash: &str) -> String {
    hash.to_ascii_lowercase()
}

/// The content key of a file only an index knows (its sampled hash, which
/// covers its size: see `drive_files::hash::sampled`).
pub fn sampled_key(sampled: &str) -> String {
    format!("s{}", sampled.to_ascii_lowercase())
}

/// The content key for a file: its full hash when there is one, else its
/// sampled hash.
pub fn source_key(full: Option<&str>, sampled: Option<&str>) -> Option<String> {
    match (
        full.filter(|h| !h.is_empty()),
        sampled.filter(|h| !h.is_empty()),
    ) {
        (Some(h), _) => Some(full_key(h)),
        (None, Some(s)) => Some(sampled_key(s)),
        _ => None,
    }
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether a string is a content key (a full hash, or `s` + a sampled one).
pub fn valid_key(key: &str) -> bool {
    hex64(key) || key.strip_prefix('s').is_some_and(hex64)
}

/// The full hash in a key, when it is one.
pub fn full_hash_of(key: &str) -> Option<&str> {
    hex64(key).then_some(key)
}

/// Where a sidecar is read on the node: `/sidecar/<key>/<kind>`.
pub fn url(key: &str, kind: Kind) -> String {
    format!("/sidecar/{key}/{}", kind.as_str())
}

/// The producer that can read a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Generator {
    Image,
    Pdf,
    Video,
    Audio,
    Text,
}

fn ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Raster images the image producer decodes. HEIC/HEIF are not here: no
/// pure-Rust HEVC decoder is good enough yet (`docs/sidecars.md`).
pub const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jfif", "gif", "webp", "bmp", "tif", "tiff",
];
pub const AUDIO_EXTS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "aifc", "flac", "mp3", "m4a", "aac", "alac", "ogg", "oga",
    "opus", "caf", "mka",
];
pub const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi", "mts", "m2ts"];
pub const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "toml", "yaml", "yml", "xml", "html",
    "css", "js", "ts", "rs", "py", "sh", "c", "h", "cpp", "swift", "go", "kf", "styx", "ini",
    "nix", "lua", "sql",
];

/// Which producer reads a file named `name`; `None`: nothing derives from
/// it (its icon stands in).
pub fn generator_for(name: &str) -> Option<Generator> {
    let e = ext(name);
    let e = e.as_str();
    if IMAGE_EXTS.contains(&e) {
        Some(Generator::Image)
    } else if e == "pdf" {
        Some(Generator::Pdf)
    } else if AUDIO_EXTS.contains(&e) {
        Some(Generator::Audio)
    } else if VIDEO_EXTS.contains(&e) {
        Some(Generator::Video)
    } else if TEXT_EXTS.contains(&e) {
        Some(Generator::Text)
    } else {
        None
    }
}

/// The sidecars a file gets: thumbnails for everything a producer reads,
/// and peaks for audio.
pub fn kinds_for(name: &str) -> Vec<Kind> {
    match generator_for(name) {
        None => Vec::new(),
        Some(Generator::Audio) => Kind::ALL.to_vec(),
        Some(_) => vec![Kind::Thumb256, Kind::Thumb1024],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_kinds() {
        let h = "a".repeat(64);
        assert_eq!(source_key(Some(&h), Some("x")), Some(h.clone()));
        assert_eq!(source_key(Some(""), Some(&h)), Some(format!("s{h}")));
        assert_eq!(source_key(None, None), None);
        assert!(valid_key(&h) && valid_key(&format!("s{h}")));
        assert!(!valid_key("s") && !valid_key("../etc") && !valid_key(&format!("x{h}")));
        assert_eq!(full_hash_of(&format!("s{h}")), None);
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        assert_eq!(
            Kind::of_family("thumb"),
            vec![Kind::Thumb256, Kind::Thumb1024]
        );
        assert_eq!(Kind::of_family("peaks"), vec![Kind::Peaks]);
        assert_eq!(Kind::of_family("thumb-1024"), vec![Kind::Thumb1024]);
        assert_eq!(generator_for("Take 3.WAV"), Some(Generator::Audio));
        assert_eq!(generator_for("IMG_1.HEIC"), None);
        assert_eq!(kinds_for("a.pdf"), vec![Kind::Thumb256, Kind::Thumb1024]);
        assert_eq!(kinds_for("a.flac").len(), 3);
        assert_eq!(url(&h, Kind::Peaks), format!("/sidecar/{h}/peaks"));
    }
}
