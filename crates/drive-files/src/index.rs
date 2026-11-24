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

/// The published form: postcard, then zstd. Streamed: postcard writes
/// straight into the compressor, never a whole uncompressed copy (for
/// millions of files that copy was most of an upload's memory, drive#47).
pub fn encode(index: &LocationIndex) -> Vec<u8> {
    use std::io::Write;
    let z = zstd::stream::Encoder::new(Vec::new(), 9).expect("zstd starts in memory");
    let mut w = std::io::BufWriter::with_capacity(1 << 16, z);
    postcard::to_io(index, &mut w).expect("an index serializes");
    w.flush().expect("zstd compresses in memory");
    w.into_inner()
        .ok()
        .expect("flushed")
        .finish()
        .expect("zstd compresses in memory")
}

/// The most an index may unpack to. A few hundred bytes an entry, so this
/// is millions of files; a few kilobytes of zstd can claim far more, and a
/// node unpacks what agents upload (drive#63).
pub const MAX_UNPACKED: u64 = 2 << 30;

pub fn decode(bytes: &[u8]) -> Option<LocationIndex> {
    decode_within(bytes, MAX_UNPACKED)
}

/// [`decode`], refusing anything that unpacks to more than `max` bytes.
/// Streamed: postcard reads straight from the decompressor (a path at a
/// time through a small scratch buffer), so the uncompressed form is
/// never held whole.
pub fn decode_within(bytes: &[u8], max: u64) -> Option<LocationIndex> {
    use std::io::Read;
    let unpacked = zstd::stream::Decoder::new(bytes)
        .ok()?
        .take(max.saturating_add(1));
    let reader = std::io::BufReader::with_capacity(1 << 16, unpacked);
    let mut scratch = vec![0u8; 1 << 16];
    let (index, (mut rest, _)) = postcard::from_io((reader, &mut scratch)).ok()?;
    // Whatever follows counts towards the bound too (read, not kept).
    std::io::copy(&mut rest, &mut std::io::sink()).ok()?;
    if rest.into_inner().limit() == 0 {
        return None;
    }
    Some(index)
}

/// An index arranged for browsing: each folder's children.
///
/// Positions into the index, not copies of its paths: with millions of
/// files the copies were a second index's worth of memory (drive#47).
pub struct Tree {
    /// Shared with whoever else holds the index (an agent keeps one copy
    /// of each and hands it round).
    pub index: std::sync::Arc<LocationIndex>,
    /// Every entry, by path (for [`Tree::get`]).
    by_path: Vec<u32>,
    /// Each folder's entries, by the folder's position ([`ROOT`]: the
    /// location's root), folders first, by name.
    children: HashMap<u32, Vec<u32>>,
    /// Entries whose folder has no entry of its own, by the folder's path.
    orphans: HashMap<String, Vec<u32>>,
}

/// The location's root, in [`Tree`]'s `children`.
const ROOT: u32 = u32::MAX;

impl Tree {
    pub fn new(index: impl Into<std::sync::Arc<LocationIndex>>) -> Tree {
        let index = index.into();
        let entries = &index.entries;
        let mut by_path: Vec<u32> = (0..entries.len() as u32).collect();
        if !entries.is_sorted_by(|a, b| a.path <= b.path) {
            by_path.sort_by(|a, b| entries[*a as usize].path.cmp(&entries[*b as usize].path));
        }
        let find = |path: &str| -> Option<u32> {
            by_path
                .binary_search_by(|i| entries[*i as usize].path.as_str().cmp(path))
                .ok()
                .map(|at| by_path[at])
        };
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut orphans: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            let parent = e.parent();
            if parent.is_empty() {
                children.entry(ROOT).or_default().push(i as u32);
            } else if let Some(p) = find(parent) {
                children.entry(p).or_default().push(i as u32);
            } else {
                orphans.entry(parent.to_string()).or_default().push(i as u32);
            }
        }
        let order = |a: &u32, b: &u32| {
            let (x, y) = (&entries[*a as usize], &entries[*b as usize]);
            (!x.dir, x.name().to_lowercase()).cmp(&(!y.dir, y.name().to_lowercase()))
        };
        for list in children.values_mut().chain(orphans.values_mut()) {
            list.sort_by(order);
            list.shrink_to_fit();
        }
        Tree {
            index,
            by_path,
            children,
            orphans,
        }
    }

    fn position(&self, path: &str) -> Option<u32> {
        let entries = &self.index.entries;
        self.by_path
            .binary_search_by(|i| entries[*i as usize].path.as_str().cmp(path))
            .ok()
            .map(|at| self.by_path[at])
    }

    /// A folder's entries (`""`: the location's root).
    pub fn list(&self, dir: &str) -> Vec<&IndexEntry> {
        let list = if dir.is_empty() {
            self.children.get(&ROOT)
        } else {
            match self.position(dir) {
                Some(p) => self.children.get(&p),
                None => self.orphans.get(dir),
            }
        };
        list.map(|v| v.iter().map(|i| &self.index.entries[*i as usize]).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.position(path).map(|i| &self.index.entries[i as usize])
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
        std::fs::write(r.join("lib/empty.txt"), []).unwrap();
        std::fs::write(r.join("lib/x.tmp"), b"t").unwrap();
        let op = crate::storage::StorageConfig::Fs {
            root: r.to_string_lossy().into(),
        }
        .operator()
        .unwrap();
        let (idx, st) = walk_storage("s", &op, "lib", None).await.unwrap();
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["Strings", "Strings/legato.wav", "empty.txt"]);
        assert_eq!(st.hashed, 2);
        // The same sampled hash as walking it on disk.
        let (disk, _) = walk("d", &r.join("lib"), None);
        assert_eq!(idx.entries[1].sampled, disk.entries[1].sampled);
        assert_eq!(idx.entries[2].size, 0);
        assert_eq!(idx.entries[2].sampled, disk.entries[2].sampled);
    }

    #[test]
    fn decode_refuses_an_index_that_unpacks_past_its_bound() {
        // A few kilobytes of zstd that unpack to 8 MiB of zeros, which
        // postcard would read as an empty index.
        let bomb = zstd::encode_all(vec![0u8; 8 << 20].as_slice(), 3).unwrap();
        assert!(bomb.len() < 4096);
        assert!(decode_within(&bomb, 1 << 20).is_none());
        assert!(decode_within(&bomb, 16 << 20).is_some());
    }


    #[test]
    fn a_tree_over_an_unsorted_index_with_missing_folders() {
        let e = |path: &str, dir| IndexEntry {
            path: path.into(),
            dir,
            size: 1,
            mtime: 0,
            sampled: String::new(),
        };
        // Out of order, and "b/c" has no entry of its own.
        let idx = LocationIndex {
            location: "l".into(),
            root: "/".into(),
            built: 0,
            entries: vec![e("b/c/z.wav", false), e("a", true), e("b", true), e("a/y.wav", false), e("b/c/x.wav", false), e("top.txt", false)],
        };
        let t = Tree::new(idx);
        let names = |dir: &str| t.list(dir).iter().map(|e| e.name().to_string()).collect::<Vec<_>>();
        assert_eq!(names(""), ["a", "b", "top.txt"]);
        assert_eq!(names("a"), ["y.wav"]);
        assert_eq!(names("b/c"), ["x.wav", "z.wav"]);
        assert!(names("b").is_empty());
        assert!(names("nowhere").is_empty());
        assert_eq!(t.get("b/c/z.wav").map(|e| e.size), Some(1));
        assert!(t.get("b/c").is_none());
    }

}
