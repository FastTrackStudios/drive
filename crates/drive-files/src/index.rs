//! A location's index: every file and folder under a folder a device
//! keeps in place (a project drive, a home folder, a NAS share), with its
//! size, time and sampled hash. The bytes stay where they are; the index
//! travels: it is published as one blob (postcard, zstd), so every device
//! and the node can search and browse it, even while its device is off.
//!
//! Walking respects ignore rules: `.gitignore`, `.ignore` and
//! `.driveignore` files, hidden files, and [`DEFAULT_IGNORES`].

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// Never indexed: caches, build output, temp files, system clutter.
pub const DEFAULT_IGNORES: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".cache",
    ".Trash*",
    "__pycache__",
    ".DS_Store",
    "Thumbs.db",
    "*.tmp",
    "*.part",
    "*.swp",
    "*~",
    ".direnv",
    "result",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Relative to the location's root, `/`-separated.
    pub path: String,
    pub dir: bool,
    pub size: u64,
    /// Modified, ms since the epoch.
    pub mtime: i64,
    /// The sampled BLAKE3 hash (files only; empty when unreadable).
    pub sampled: String,
}

impl IndexEntry {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub fn parent(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(p, _)| p)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocationIndex {
    pub location: String,
    pub root: String,
    pub built: i64,
    pub entries: Vec<IndexEntry>,
}

/// What a walk found and how much work it was.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WalkStats {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// Files whose sampled hash was computed (the rest were unchanged).
    pub hashed: usize,
}

/// Walk a location. Files unchanged since `previous` (same size and time)
/// keep their hash; only new or changed files are read.
pub fn walk(
    location: &str,
    root: &Path,
    previous: Option<&LocationIndex>,
) -> (LocationIndex, WalkStats) {
    let old: HashMap<&str, &IndexEntry> = previous
        .map(|p| p.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pat in DEFAULT_IGNORES {
        let _ = overrides.add(&format!("!{pat}"));
    }
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .ignore(true)
        .add_custom_ignore_filename(".driveignore")
        .overrides(
            overrides
                .build()
                .unwrap_or_else(|_| ignore::overrides::Override::empty()),
        )
        .follow_links(false)
        .build();
    let mut stats = WalkStats::default();
    let mut entries = Vec::new();
    for dent in walker.flatten() {
        let Ok(rel) = dent.path().strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let Some(rel) = rel.to_str().map(|s| s.replace('\\', "/")) else {
            continue;
        };
        let Ok(meta) = dent.metadata() else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        if meta.is_dir() {
            stats.dirs += 1;
            entries.push(IndexEntry {
                path: rel,
                dir: true,
                size: 0,
                mtime,
                sampled: String::new(),
            });
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        stats.files += 1;
        stats.bytes += size;
        let sampled = match old.get(rel.as_str()) {
            Some(o) if !o.dir && o.size == size && o.mtime == mtime && !o.sampled.is_empty() => {
                o.sampled.clone()
            }
            _ => {
                stats.hashed += 1;
                crate::hash::sampled(dent.path()).unwrap_or_default()
            }
        };
        entries.push(IndexEntry {
            path: rel,
            dir: false,
            size,
            mtime,
            sampled,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let index = LocationIndex {
        location: location.to_string(),
        root: root.to_string_lossy().into_owned(),
        built: crate::now_ms(),
        entries,
    };
    (index, stats)
}

/// Walk a location that is in a storage (an S3 bucket, a WebDAV share),
/// through OpenDAL: listed recursively, sampled hashes read as ranges.
/// The default ignores apply; ignore files are not read.
pub async fn walk_storage(
    location: &str,
    op: &opendal::Operator,
    root: &str,
    previous: Option<&LocationIndex>,
) -> opendal::Result<(LocationIndex, WalkStats)> {
    let old: HashMap<&str, &IndexEntry> = previous
        .map(|p| p.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();
    let base = format!("{}/", root.trim_matches('/'))
        .trim_start_matches('/')
        .to_string();
    let mut globs = ignore::gitignore::GitignoreBuilder::new("");
    for pat in DEFAULT_IGNORES {
        let _ = globs.add_line(None, pat);
    }
    let globs = globs
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
    let mut stats = WalkStats::default();
    let mut entries = Vec::new();
    let mut dirs = std::collections::BTreeSet::new();
    for e in op.list_with(&base).recursive(true).await? {
        let full = e.path();
        let Some(rel) = full.strip_prefix(&base) else {
            continue;
        };
        let rel = rel.trim_end_matches('/');
        if rel.is_empty()
            || rel
                .split('/')
                .any(|seg| seg.starts_with('.') || globs.matched(seg, false).is_ignore())
        {
            continue;
        }
        // Every ancestor is a folder, listed or not (buckets have no folders).
        let mut p = rel;
        while let Some((parent, _)) = p.rsplit_once('/') {
            dirs.insert(parent.to_string());
            p = parent;
        }
        let meta = e.metadata();
        if meta.is_dir() {
            dirs.insert(rel.to_string());
            continue;
        }
        let size = meta.content_length();
        let mtime = meta
            .last_modified()
            .map_or(0, |t| t.into_inner().as_millisecond());
        stats.files += 1;
        stats.bytes += size;
        let sampled = match old.get(rel) {
            Some(o) if !o.dir && o.size == size && o.mtime == mtime && !o.sampled.is_empty() => {
                o.sampled.clone()
            }
            _ => {
                stats.hashed += 1;
                let mut parts = Vec::new();
                for r in crate::hash::sample_ranges(size) {
                    parts.push(op.read_with(full).range(r).await?.to_vec());
                }
                crate::hash::sampled_of(size, &parts.iter().map(Vec::as_slice).collect::<Vec<_>>())
            }
        };
        entries.push(IndexEntry {
            path: rel.to_string(),
            dir: false,
            size,
            mtime,
            sampled,
        });
    }
    stats.dirs = dirs.len();
    entries.extend(dirs.into_iter().map(|d| IndexEntry {
        path: d,
        dir: true,
        size: 0,
        mtime: 0,
        sampled: String::new(),
    }));
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let index = LocationIndex {
        location: location.to_string(),
        root: root.to_string(),
        built: crate::now_ms(),
        entries,
    };
    Ok((index, stats))
}

/// The published form: postcard, then zstd.
pub fn encode(index: &LocationIndex) -> Vec<u8> {
    let raw = postcard::to_stdvec(index).expect("an index serializes");
    zstd::encode_all(raw.as_slice(), 9).expect("zstd compresses in memory")
}

pub fn decode(bytes: &[u8]) -> Option<LocationIndex> {
    let raw = zstd::decode_all(bytes).ok()?;
    postcard::from_bytes(&raw).ok()
}

/// An index arranged for browsing: each folder's children.
pub struct Tree {
    pub index: LocationIndex,
    children: HashMap<String, Vec<usize>>,
    by_path: HashMap<String, usize>,
}

impl Tree {
    pub fn new(index: LocationIndex) -> Tree {
        let mut children: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_path = HashMap::new();
        for (i, e) in index.entries.iter().enumerate() {
            children.entry(e.parent().to_string()).or_default().push(i);
            by_path.insert(e.path.clone(), i);
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                let (x, y) = (&index.entries[*a], &index.entries[*b]);
                (!x.dir, x.name().to_lowercase()).cmp(&(!y.dir, y.name().to_lowercase()))
            });
        }
        Tree {
            index,
            children,
            by_path,
        }
    }

    /// A folder's entries (`""`: the location's root).
    pub fn list(&self, dir: &str) -> Vec<&IndexEntry> {
        self.children
            .get(dir)
            .map(|v| v.iter().map(|i| &self.index.entries[*i]).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.by_path.get(path).map(|i| &self.index.entries[*i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_ignores_reuses_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("Sessions/Song A")).unwrap();
        std::fs::create_dir_all(r.join("node_modules/x")).unwrap();
        std::fs::write(r.join("Sessions/Song A/kick.wav"), vec![1u8; 5000]).unwrap();
        std::fs::write(r.join("Sessions/Song A/render.tmp"), b"tmp").unwrap();
        std::fs::write(r.join("node_modules/x/a.js"), b"x").unwrap();
        std::fs::write(r.join("Sessions/notes.txt"), b"bridge").unwrap();
        std::fs::write(r.join(".driveignore"), "*.bak\n").unwrap();
        std::fs::write(r.join("Sessions/old.bak"), b"old").unwrap();

        let (idx, st) = walk("loc", r, None);
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Sessions",
                "Sessions/Song A",
                "Sessions/Song A/kick.wav",
                "Sessions/notes.txt"
            ]
        );
        assert_eq!((st.files, st.dirs, st.hashed), (2, 2, 2));

        // Nothing changed: nothing re-hashed.
        let (again, st2) = walk("loc", r, Some(&idx));
        assert_eq!(st2.hashed, 0);
        assert_eq!(again.entries, idx.entries);

        let back = decode(&encode(&idx)).unwrap();
        assert_eq!(back, idx);
        let t = Tree::new(back);
        assert_eq!(t.list("").len(), 1);
        assert_eq!(
            t.list("Sessions")
                .iter()
                .map(|e| e.name())
                .collect::<Vec<_>>(),
            ["Song A", "notes.txt"]
        );
        assert!(
            t.get("Sessions/Song A/kick.wav")
                .is_some_and(|e| e.size == 5000)
        );
    }

    #[tokio::test]
    async fn walks_a_storage_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("lib/Strings")).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(r.join("lib/Strings/legato.wav"), &big).unwrap();
        std::fs::write(r.join("lib/x.tmp"), b"t").unwrap();
        let op = crate::storage::StorageConfig::Fs {
            root: r.to_string_lossy().into(),
        }
        .operator()
        .unwrap();
        let (idx, st) = walk_storage("s", &op, "lib", None).await.unwrap();
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["Strings", "Strings/legato.wav"]);
        assert_eq!(st.hashed, 1);
        // The same sampled hash as walking it on disk.
        let (disk, _) = walk("d", &r.join("lib"), None);
        assert_eq!(idx.entries[1].sampled, disk.entries[1].sampled);
    }
}
