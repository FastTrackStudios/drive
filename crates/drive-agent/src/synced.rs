//! Names that arrive from the synced space (a blob's hash, a sampled hash,
//! a location's or a site's id) and become one file name in Drive's own
//! folder (`cache/`, `remote/`, `indexes/`, `mirror/`, `sites/`): checked
//! before they are joined, so another device cannot point one elsewhere.

/// A blob or sampled hash: 64 hex digits.
pub fn hash(h: &str) -> Result<&str, String> {
    if drive_files::blob::valid_hash(h) {
        Ok(h)
    } else {
        Err(format!("not a content hash: {h:?}"))
    }
}

/// An id: one plain path component (no separator, no `..`, not `.`).
pub fn id(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty() && s != "." && !s.contains("..") && !s.contains(['/', '\\', '\0']);
    if ok {
        Ok(s)
    } else {
        Err(format!("not an id: {s:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt::tests::{device, node, songs};

    #[test]
    fn only_plain_names_pass() {
        assert!(hash(&"a1".repeat(32)).is_ok());
        for bad in ["", "ab", "é", "../../config.json", &"../".repeat(16)] {
            assert!(hash(bad).is_err(), "{bad}");
        }
        assert!(id("3f9a01c2d4e5b6a7").is_ok());
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a\0b", "/etc"] {
            assert!(id(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn synced_names_stay_inside_drives_folder() {
        let (url, _node_dir) = node().await;
        let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = device(&url, dir.path(), "studio", "Studio PC").await;
        assert!(dir.path().join("config.json").exists());

        // A "hash" that walks out of cache/ to the config (tokens, keys).
        assert!(!a.is_cached("../config.json"));
        assert!(a.fetch("../config.json").await.is_err());

        // A sampled hash that walks out of remote/.
        let loc = a.add_location("Songs", &songs(disk.path())).unwrap();
        let mut theirs = loc.clone();
        theirs.device = "elsewhere".into();
        let e = drive_files::index::IndexEntry {
            path: "Song A/a.wav".into(),
            dir: false,
            size: 4,
            mtime: 0,
            sampled: "../config.json".into(),
        };
        assert!(a.bring_location_file(&theirs, &e).await.is_err());

        // A location id that walks out of indexes/.
        std::fs::write(dir.path().join("escape.idx"), b"keep").unwrap();
        a.forget_index("../escape");
        assert!(dir.path().join("escape.idx").exists());

        // A site id that walks out of sites/.
        assert!(crate::sites::work_dir(dir.path(), "../state").is_err());
    }
}
