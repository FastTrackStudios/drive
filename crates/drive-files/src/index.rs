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
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    let tree = previous.map(|p| Tree::new(p.clone()));
    walk_with(
        location,
        root,
        tree.as_ref().map(|t| t as &dyn Previous),
        walk_threads(),
    )
}

/// What a location held at its last walk, asked a folder at a time (so
/// it can live on disk: the agent answers from its index database,
/// drive#47). A file unchanged since (same size and time) keeps its hash.
pub trait Previous: Sync {
    /// The entries directly in `dir` (`""`: the root).
    fn folder(&self, dir: &str) -> Vec<IndexEntry>;
}

impl Previous for Tree {
    fn folder(&self, dir: &str) -> Vec<IndexEntry> {
        self.list(dir).into_iter().cloned().collect()
    }
}

/// [`Previous`], asked from async code (a storage's walk, which runs on
/// the agent's runtime and so cannot block on it).
pub trait AsyncPrevious: Sync {
    /// The entries directly in `dir` (`""`: the root).
    fn folder(&self, dir: &str) -> impl std::future::Future<Output = Vec<IndexEntry>> + Send;
}

impl AsyncPrevious for Tree {
    async fn folder(&self, dir: &str) -> Vec<IndexEntry> {
        Previous::folder(self, dir)
    }
}

/// How many threads a walk uses: directories are listed and files hashed
/// on several at once (as `dua` and ripgrep do), which is most of a big
/// first index's time on an SSD or a network disk.
fn walk_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 8)
}

/// What the walk saw of one entry, before any hashing.
struct Seen {
    path: String,
    dir: bool,
    size: u64,
    mtime: i64,
    /// The hash it keeps (unchanged since the previous index), if any.
    kept: Option<String>,
}

/// Entries a walking thread hands over at once.
pub const EMIT: usize = 1024;

/// [`walk`] on `threads` threads, with the last walk as `previous`: the
/// whole index, in path order.
pub fn walk_with(
    location: &str,
    root: &Path,
    previous: Option<&dyn Previous>,
    threads: usize,
) -> (LocationIndex, WalkStats) {
    let found = std::sync::Mutex::new(Vec::new());
    let stats = walk_into(root, previous, threads, &|mut batch| {
        found.lock().expect("walk").append(&mut batch);
    });
    let mut entries = found.into_inner().expect("walk");
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let index = LocationIndex {
        location: location.to_string(),
        root: root.to_string_lossy().into_owned(),
        built: crate::now_ms(),
        entries,
    };
    (index, stats)
}

/// Walk a location on the default number of threads, handing what it
/// finds to `emit` [`EMIT`] entries at a time (in no order), so the walk
/// never holds the whole index: the agent writes them to its index
/// database as they come (drive#156). New and changed files are hashed
/// as they are found.
pub fn walk_streamed(
    root: &Path,
    previous: Option<&dyn Previous>,
    emit: &(dyn Fn(Vec<IndexEntry>) + Sync),
) -> WalkStats {
    walk_into(root, previous, walk_threads(), emit)
}

/// [`walk_streamed`] on `threads` threads.
pub fn walk_into(
    root: &Path,
    previous: Option<&dyn Previous>,
    threads: usize,
    emit: &(dyn Fn(Vec<IndexEntry>) + Sync),
) -> WalkStats {
    let total = std::sync::Mutex::new(WalkStats::default());
    walker(root, threads).run(|| {
        let mut out = Emitter {
            batch: Vec::with_capacity(EMIT),
            stats: WalkStats::default(),
            total: &total,
            emit,
        };
        let mut old = FolderCache::default();
        Box::new(move |dent| {
            let Ok(dent) = dent else {
                return ignore::WalkState::Continue;
            };
            let mut lookup = |path: &str| previous.and_then(|p| old.get(p, path));
            if let Some(seen) = seen(root, &dent, &mut lookup) {
                out.push(hashed(root, seen));
            }
            ignore::WalkState::Continue
        })
    });
    total.into_inner().expect("walk")
}

/// The parallel walker over `root`, with Drive's ignore rules.
fn walker(root: &Path, threads: usize) -> ignore::WalkParallel {
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pat in DEFAULT_IGNORES {
        let _ = overrides.add(&format!("!{pat}"));
    }
    ignore::WalkBuilder::new(root)
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
        .threads(threads)
        .build_parallel()
}

/// One walking thread's output: handed over [`EMIT`] entries at a time,
/// the rest (and its counts) when the thread ends.
struct Emitter<'a> {
    batch: Vec<IndexEntry>,
    stats: WalkStats,
    total: &'a std::sync::Mutex<WalkStats>,
    emit: &'a (dyn Fn(Vec<IndexEntry>) + Sync),
}

impl Emitter<'_> {
    fn push(&mut self, (entry, was_hashed): (IndexEntry, bool)) {
        if entry.dir {
            self.stats.dirs += 1;
        } else {
            self.stats.files += 1;
            self.stats.bytes += entry.size;
            self.stats.hashed += usize::from(was_hashed);
        }
        self.batch.push(entry);
        if self.batch.len() >= EMIT {
            (self.emit)(std::mem::replace(&mut self.batch, Vec::with_capacity(EMIT)));
        }
    }
}

impl Drop for Emitter<'_> {
    fn drop(&mut self) {
        if !self.batch.is_empty() {
            (self.emit)(std::mem::take(&mut self.batch));
        }
        let mut t = self.total.lock().expect("walk");
        t.files += self.stats.files;
        t.dirs += self.stats.dirs;
        t.bytes += self.stats.bytes;
        t.hashed += self.stats.hashed;
    }
}

/// The last walk's entries of the folder being walked. A folder's entries
/// are visited together by the thread that read it, so its last walk is
/// asked for once and kept while they are.
#[derive(Default)]
struct FolderCache {
    dir: Option<String>,
    entries: HashMap<String, IndexEntry>,
}

impl FolderCache {
    fn get(&mut self, previous: &dyn Previous, path: &str) -> Option<IndexEntry> {
        let dir = parent_of(path);
        if self.dir.as_deref() != Some(dir) {
            self.entries = by_path(previous.folder(dir));
            self.dir = Some(dir.to_string());
        }
        self.entries.get(path).cloned()
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn by_path(entries: Vec<IndexEntry>) -> HashMap<String, IndexEntry> {
    entries.into_iter().map(|e| (e.path.clone(), e)).collect()
}

/// The entry a walk keeps for what it saw, its file hashed unless it kept
/// its last hash; and whether it was hashed.
fn hashed(root: &Path, s: Seen) -> (IndexEntry, bool) {
    let (sampled, was_hashed) = match (s.dir, s.kept) {
        (true, _) => (String::new(), false),
        (false, Some(kept)) => (kept, false),
        (false, None) => (
            crate::hash::sampled(&root.join(&s.path)).unwrap_or_default(),
            true,
        ),
    };
    let entry = IndexEntry {
        path: s.path,
        dir: s.dir,
        size: s.size,
        mtime: s.mtime,
        sampled,
    };
    (entry, was_hashed)
}

/// The hash a file keeps from its last walk: the same size and time, and
/// a hash it had.
fn kept_hash(old: Option<IndexEntry>, size: u64, mtime: i64) -> Option<String> {
    old.filter(|o| !o.dir && o.size == size && o.mtime == mtime && !o.sampled.is_empty())
        .map(|o| o.sampled)
}

/// One walked entry as the index keeps it (`None`: the root, a path that
/// is not UTF-8, something unreadable, or neither a file nor a folder).
fn seen(
    root: &Path,
    dent: &ignore::DirEntry,
    old: &mut impl FnMut(&str) -> Option<IndexEntry>,
) -> Option<Seen> {
    let rel = dent.path().strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let rel = rel.to_str()?.replace('\\', "/");
    let meta = dent.metadata().ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    if meta.is_dir() {
        return Some(Seen {
            path: rel,
            dir: true,
            size: 0,
            mtime,
            kept: None,
        });
    }
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let kept = kept_hash(old(&rel), size, mtime);
    Some(Seen {
        path: rel,
        dir: false,
        size,
        mtime,
        kept,
    })
}

/// The last walk's entries of the folders a storage's listing is in. A
/// listing goes depth first, so only the current folder's ancestors can
/// come up again: the rest are let go.
#[derive(Default)]
struct AncestorCache {
    folders: HashMap<String, HashMap<String, IndexEntry>>,
}

impl AncestorCache {
    async fn get(&mut self, previous: &impl AsyncPrevious, path: &str) -> Option<IndexEntry> {
        let dir = parent_of(path);
        self.folders.retain(|d, _| {
            d.is_empty()
                || dir == d
                || dir
                    .strip_prefix(d.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        });
        if !self.folders.contains_key(dir) {
            let entries = by_path(previous.folder(dir).await);
            self.folders.insert(dir.to_string(), entries);
        }
        self.folders[dir].get(path).cloned()
    }
}

/// Walk a location that is in a storage (an S3 bucket, a WebDAV share),
/// through OpenDAL: listed recursively, sampled hashes read as ranges.
/// The default ignores apply; ignore files are not read. Its last walk is
/// asked a folder at a time, as [`walk_streamed`]'s is.
pub async fn walk_storage(
    location: &str,
    op: &opendal::Operator,
    root: &str,
    previous: Option<&impl AsyncPrevious>,
) -> opendal::Result<(LocationIndex, WalkStats)> {
    let mut old = AncestorCache::default();
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
        let last = match previous {
            Some(p) => old.get(p, rel).await,
            None => None,
        };
        let sampled = match kept_hash(last, size, mtime) {
            Some(kept) => kept,
            None => {
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

/// Postcard's unsigned varint (LEB128), as it writes lengths and `u64`s.
fn varint(mut n: u64, out: &mut impl std::io::Write) -> std::io::Result<()> {
    let mut buf = [0u8; 10];
    let mut at = 0;
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            buf[at] = byte;
            at += 1;
            break;
        }
        buf[at] = byte | 0x80;
        at += 1;
    }
    out.write_all(&buf[..at])
}

/// Why a streamed encoding was not finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// More entries were pushed than the index said it has.
    TooMany,
    /// Fewer were (by how many): the index changed while it was read.
    TooFew(u64),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TooMany => write!(f, "more entries than the index has"),
            EncodeError::TooFew(n) => write!(f, "{n} entries fewer than the index has"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// [`encode`], an entry at a time: the same bytes as encoding the whole
/// [`LocationIndex`], without holding it (drive#156). Postcard is written
/// by hand where it differs from a value at a time: the fields, the
/// entry count, then each entry (pushed in path order, as
/// [`LocationIndex::entries`] is).
pub struct Encoder {
    out: std::io::BufWriter<zstd::stream::Encoder<'static, Vec<u8>>>,
    left: u64,
    /// One value's postcard, reused (`postcard::to_io` flushes after each
    /// value, and every flush would end a zstd block: other bytes).
    scratch: Vec<u8>,
}

impl Encoder {
    /// Start an index of `count` entries.
    pub fn new(location: &str, root: &str, built: i64, count: u64) -> Encoder {
        let z = zstd::stream::Encoder::new(Vec::new(), 9).expect("zstd starts in memory");
        let out = std::io::BufWriter::with_capacity(1 << 16, z);
        let mut enc = Encoder {
            out,
            left: count,
            scratch: Vec::new(),
        };
        enc.write(&location);
        enc.write(&root);
        enc.write(&built);
        varint(count, &mut enc.out).expect("zstd compresses in memory");
        enc
    }

    fn write(&mut self, value: &impl Serialize) {
        use std::io::Write;
        self.scratch.clear();
        let bytes = postcard::to_extend(value, std::mem::take(&mut self.scratch))
            .expect("an index serializes");
        self.out
            .write_all(&bytes)
            .expect("zstd compresses in memory");
        self.scratch = bytes;
    }

    /// The next entry, in path order.
    ///
    /// # Errors
    /// [`EncodeError::TooMany`] past the count it was started with.
    pub fn push(&mut self, entry: &IndexEntry) -> Result<(), EncodeError> {
        self.left = self.left.checked_sub(1).ok_or(EncodeError::TooMany)?;
        self.write(entry);
        Ok(())
    }

    /// The published bytes.
    ///
    /// # Errors
    /// [`EncodeError::TooFew`] when fewer entries were pushed than counted.
    pub fn finish(self) -> Result<Vec<u8>, EncodeError> {
        use std::io::Write;
        if self.left > 0 {
            return Err(EncodeError::TooFew(self.left));
        }
        let mut out = self.out;
        out.flush().expect("zstd compresses in memory");
        Ok(out
            .into_inner()
            .ok()
            .expect("flushed")
            .finish()
            .expect("zstd compresses in memory"))
    }
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
                orphans
                    .entry(parent.to_string())
                    .or_default()
                    .push(i as u32);
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

// ── folders that moved ──────────────────────────────────────────────────

/// What a folder holds, as one value: every file under it by its path
/// inside the folder and its sampled hash. The same after a move or a
/// rename of the folder; none for a folder with no readable file.
#[must_use]
pub fn folder_signature(entries: &[IndexEntry], folder: &str) -> Option<String> {
    let mut under: Vec<&IndexEntry> = entries.iter().collect();
    under.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let mut sig = Signature::new(folder);
    under.into_iter().for_each(|e| sig.add(e));
    sig.finish()
}

/// [`folder_signature`], built an entry at a time from entries given in
/// path order (a page at a time from a database, drive#156). Entries not
/// under the folder are passed over.
pub struct Signature {
    prefix: String,
    hasher: blake3::Hasher,
    files: usize,
}

impl Signature {
    pub fn new(folder: &str) -> Signature {
        Signature {
            prefix: format!("{}/", folder.trim_matches('/')),
            hasher: blake3::Hasher::new(),
            files: 0,
        }
    }

    /// The next entry, in path order.
    pub fn add(&mut self, e: &IndexEntry) {
        if e.dir || e.sampled.is_empty() {
            return;
        }
        let Some(path) = e.path.strip_prefix(&self.prefix) else {
            return;
        };
        self.hasher.update(path.as_bytes());
        self.hasher.update(b"\0");
        self.hasher.update(e.sampled.as_bytes());
        self.hasher.update(b"\n");
        self.files += 1;
    }

    #[must_use]
    pub fn finish(self) -> Option<String> {
        (self.files > 0).then(|| self.hasher.finalize().to_hex().to_string())
    }
}

/// Where each of `wanted` (folders of the `old` walk) went in the `new`
/// one: a folder no longer there is matched to the one new folder holding
/// exactly the same files. One with no match, or more than one, is left
/// out (a folder copied twice is not guessed at).
#[must_use]
pub fn moved_folders(
    old: &[IndexEntry],
    new: &[IndexEntry],
    wanted: &[String],
) -> Vec<(String, String)> {
    let dirs = |entries: &[IndexEntry]| -> std::collections::HashSet<String> {
        entries
            .iter()
            .filter(|e| e.dir)
            .map(|e| e.path.clone())
            .collect()
    };
    let (was, now) = (dirs(old), dirs(new));
    let fresh: Vec<&String> = now.iter().filter(|d| !was.contains(*d)).collect();
    let mut by_sig: HashMap<String, Vec<&String>> = HashMap::new();
    for d in fresh {
        if let Some(sig) = folder_signature(new, d) {
            by_sig.entry(sig).or_default().push(d);
        }
    }
    let mut out: Vec<(String, String)> = wanted
        .iter()
        .filter(|w| !now.contains(*w))
        .filter_map(
            |w| match by_sig.get(&folder_signature(old, w)?)?.as_slice() {
                [one] => Some((w.clone(), (*one).clone())),
                _ => None,
            },
        )
        .collect();
    out.sort();
    out
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
        let (idx, st) = walk_storage("s", &op, "lib", None::<&Tree>).await.unwrap();
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["Strings", "Strings/legato.wav", "empty.txt"]);
        assert_eq!(st.hashed, 2);
        // The same sampled hash as walking it on disk.
        let (disk, _) = walk("d", &r.join("lib"), None);
        assert_eq!(idx.entries[1].sampled, disk.entries[1].sampled);
        assert_eq!(idx.entries[2].size, 0);
        assert_eq!(idx.entries[2].sampled, disk.entries[2].sampled);

        // Walked again with that walk as its last, a folder at a time:
        // nothing is read again.
        let last = Tree::new(idx.clone());
        let (again, st) = walk_storage("s", &op, "lib", Some(&last)).await.unwrap();
        assert_eq!(st.hashed, 0);
        assert_eq!(again.entries, idx.entries);
    }

    #[test]
    fn a_streamed_walk_finds_what_a_whole_one_does() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for d in 0..7 {
            std::fs::create_dir_all(r.join(format!("d{d}/sub"))).unwrap();
            for f in 0..(EMIT / 3) {
                std::fs::write(r.join(format!("d{d}/sub/f{f}.wav")), format!("{d}-{f}")).unwrap();
            }
        }
        let (whole, stats) = walk_with("loc", r, None, 4);
        let batches = std::sync::Mutex::new(Vec::new());
        let streamed = walk_into(r, None, 4, &|b| batches.lock().unwrap().push(b));
        let batches = batches.into_inner().unwrap();
        assert!(batches.len() > 1 && batches.iter().all(|b| b.len() <= EMIT));
        let mut got: Vec<IndexEntry> = batches.into_iter().flatten().collect();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(got, whole.entries);
        assert_eq!(streamed, stats);
        assert_eq!((stats.dirs, stats.hashed), (14, 7 * (EMIT / 3)));

        // Again with the first walk as its last: nothing hashed.
        let tree = Tree::new(whole.clone());
        let again = walk_into(r, Some(&tree), 4, &|_| {});
        assert_eq!((again.files, again.hashed), (stats.files, 0));
    }

    #[test]
    fn an_index_encoded_an_entry_at_a_time_is_the_same_bytes() {
        let entries: Vec<IndexEntry> = (0..3000u64)
            .map(|i| IndexEntry {
                path: format!("Pack {:03}/Sample é {i:05}.wav", i / 40),
                dir: i % 41 == 0,
                size: i * 300_007,
                mtime: if i % 2 == 0 {
                    -(i as i64)
                } else {
                    1_700_000_000_000 + i as i64
                },
                sampled: if i % 41 == 0 {
                    String::new()
                } else {
                    format!("{:064x}", i * 77)
                },
            })
            .collect();
        for entries in [Vec::new(), entries] {
            let index = LocationIndex {
                location: "loc-1".into(),
                root: "/Volumes/Samples Ünïcode".into(),
                built: 1_764_000_000_123,
                entries,
            };
            let mut enc = Encoder::new(
                &index.location,
                &index.root,
                index.built,
                index.entries.len() as u64,
            );
            for e in &index.entries {
                enc.push(e).unwrap();
            }
            assert_eq!(enc.finish().unwrap(), encode(&index));
        }
        let mut short = Encoder::new("l", "/", 0, 2);
        short.push(&entry("a", false, "h")).unwrap();
        let mut over = Encoder::new("l", "/", 0, 0);
        assert_eq!(
            over.push(&entry("a", false, "h")),
            Err(EncodeError::TooMany)
        );
        assert_eq!(short.finish(), Err(EncodeError::TooFew(1)));
    }

    #[test]
    fn varints_are_postcards() {
        for n in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut ours = Vec::new();
            varint(n, &mut ours).unwrap();
            assert_eq!(ours, postcard::to_stdvec(&n).unwrap(), "{n}");
        }
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
            entries: vec![
                e("b/c/z.wav", false),
                e("a", true),
                e("b", true),
                e("a/y.wav", false),
                e("b/c/x.wav", false),
                e("top.txt", false),
            ],
        };
        let t = Tree::new(idx);
        let names = |dir: &str| {
            t.list(dir)
                .iter()
                .map(|e| e.name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["a", "b", "top.txt"]);
        assert_eq!(names("a"), ["y.wav"]);
        assert_eq!(names("b/c"), ["x.wav", "z.wav"]);
        assert!(names("b").is_empty());
        assert!(names("nowhere").is_empty());
        assert_eq!(t.get("b/c/z.wav").map(|e| e.size), Some(1));
        assert!(t.get("b/c").is_none());
    }

    fn entry(path: &str, dir: bool, sampled: &str) -> IndexEntry {
        IndexEntry {
            path: path.into(),
            dir,
            size: 1,
            mtime: 0,
            sampled: sampled.into(),
        }
    }

    #[test]
    fn a_moved_folder_is_found_by_what_it_holds() {
        let old = vec![
            entry("Presets", true, ""),
            entry("Presets/Warm", true, ""),
            entry("Presets/Warm/a.fxp", false, "h1"),
            entry("Presets/Warm/sub", true, ""),
            entry("Presets/Warm/sub/b.fxp", false, "h2"),
            entry("Presets/Copied", true, ""),
            entry("Presets/Copied/c.fxp", false, "h3"),
            entry("Empty", true, ""),
        ];
        let new = vec![
            entry("Presets", true, ""),
            entry("Bass", true, ""),
            entry("Bass/Warm Bass", true, ""),
            entry("Bass/Warm Bass/a.fxp", false, "h1"),
            entry("Bass/Warm Bass/sub", true, ""),
            entry("Bass/Warm Bass/sub/b.fxp", false, "h2"),
            entry("X", true, ""),
            entry("X/c.fxp", false, "h3"),
            entry("Y", true, ""),
            entry("Y/c.fxp", false, "h3"),
        ];
        let wanted: Vec<String> = ["Presets/Warm", "Presets/Copied", "Empty", "Presets"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            moved_folders(&old, &new, &wanted),
            [("Presets/Warm".to_string(), "Bass/Warm Bass".to_string())],
            "a copy made twice is not guessed; an empty folder has nothing to match; one still there did not move"
        );
        assert_eq!(
            folder_signature(&old, "Presets/Warm"),
            folder_signature(&new, "Bass/Warm Bass")
        );
        assert_eq!(folder_signature(&old, "Empty"), None);

        // Built a page at a time, in path order: the same value.
        let mut sig = Signature::new("Presets/Warm");
        let mut sorted = old.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        sorted.iter().for_each(|e| sig.add(e));
        assert_eq!(sig.finish(), folder_signature(&old, "Presets/Warm"));
    }
}
