//! The cache objects this device keeps: files in the agent's own folder
//! (`<agent>/kache/objects`), named by the BLAKE3 of their S3 key, listed
//! in an append-only journal (`kache/index.jsonl`, compacted when opened),
//! under a size budget with least-recently-used eviction.
//!
//! This is derived data (kache rebuilds any of it by compiling), so it is
//! the one place Drive deletes files on its own: only files this module
//! made, only under `objects/` and `tmp/`, never anything else. Every path
//! deleted goes through [`Store::remove_owned`], which checks that.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Milliseconds since 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A kept object: its size, its content's BLAKE3 (hex; also its iroh-blobs
/// name and, quoted, its ETag), when it was stored and when last used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obj {
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "h")]
    pub hash: String,
    #[serde(rename = "m")]
    pub modified: u64,
    #[serde(rename = "u")]
    pub used: u64,
}

/// One journal line: an object as it is now, or its removal.
#[derive(Serialize, Deserialize)]
struct Line {
    k: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    o: Option<Obj>,
}

/// What a write must find first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cond {
    Any,
    /// `If-None-Match: *`: only when the key is absent.
    IfAbsent,
    /// `If-Match: <etag>`: only when the object still has this hash.
    IfMatch(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Commit {
    Stored,
    /// The condition did not hold; nothing was written.
    Precondition,
}

/// A use is written to the journal at most this often per object.
const TOUCH_EVERY_MS: u64 = 60_000;

struct Inner {
    map: HashMap<String, Obj>,
    total: u64,
    journal: Option<File>,
    lines: u64,
}

pub struct Store {
    root: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
    budget: AtomicU64,
    seq: AtomicU64,
    inner: Mutex<Inner>,
}

fn key_name(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

fn is_hex_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Store {
    /// Open (or make) the store in `root`, keeping at most `budget` bytes.
    /// Objects whose file is gone are forgotten; files in `objects/` that
    /// no entry names, and leftovers in `tmp/`, are removed.
    pub fn open(root: &Path, budget: u64) -> io::Result<Store> {
        let objects = root.join("objects");
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&objects)?;
        std::fs::create_dir_all(&tmp)?;
        let store = Store {
            root: root.to_path_buf(),
            objects,
            tmp,
            budget: AtomicU64::new(budget),
            seq: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                total: 0,
                journal: None,
                lines: 0,
            }),
        };
        for e in std::fs::read_dir(&store.tmp)?.flatten() {
            store.remove_owned(&e.path());
        }
        let mut map = read_journal(&root.join("index.jsonl"));
        map.retain(|key, o| {
            std::fs::symlink_metadata(store.object_path(key))
                .is_ok_and(|m| m.is_file() && m.len() == o.size)
        });
        let named: std::collections::HashSet<String> = map.keys().map(|k| key_name(k)).collect();
        for shard in std::fs::read_dir(&store.objects)?.flatten() {
            let Ok(ft) = shard.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(shard.path())?.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if f.file_type().is_ok_and(|t| t.is_file())
                    && is_hex_name(&name)
                    && !named.contains(&name)
                {
                    store.remove_owned(&f.path());
                }
            }
        }
        {
            let mut inner = store.inner.lock().expect("store");
            inner.total = map.values().map(|o| o.size).sum();
            inner.map = map;
            store.compact(&mut inner)?;
        }
        store.evict(None);
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_budget(&self, budget: u64) {
        self.budget.store(budget, Ordering::Relaxed);
        self.evict(None);
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Where an object's bytes are (whether or not it is kept now).
    pub fn object_path(&self, key: &str) -> PathBuf {
        let name = key_name(key);
        self.objects.join(&name[..2]).join(name)
    }

    /// A kept object and its file, marked used now.
    pub fn get(&self, key: &str) -> Option<(PathBuf, Obj)> {
        let mut inner = self.inner.lock().expect("store");
        let now = now_ms();
        let o = inner.map.get_mut(key)?;
        let touch = now.saturating_sub(o.used) > TOUCH_EVERY_MS;
        o.used = now;
        let o = o.clone();
        if touch {
            self.append(&mut inner, key, Some(&o));
        }
        Some((self.object_path(key), o))
    }

    /// Keys under `prefix`, in order.
    pub fn list(&self, prefix: &str) -> Vec<(String, Obj)> {
        let inner = self.inner.lock().expect("store");
        let mut out: Vec<(String, Obj)> = inner
            .map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, o)| (k.clone(), o.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// (objects, bytes) kept now.
    pub fn stats(&self) -> (usize, u64) {
        let inner = self.inner.lock().expect("store");
        (inner.map.len(), inner.total)
    }

    /// A new file to write an object into before [`Store::commit`].
    pub fn temp(&self) -> io::Result<(PathBuf, File)> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self
            .tmp
            .join(format!("{}-{}-{n}", std::process::id(), now_ms()));
        let f = File::create(&path)?;
        Ok((path, f))
    }

    /// Drop a temporary file that will not be committed.
    pub fn discard(&self, tmp: &Path) {
        self.remove_owned(tmp);
    }

    /// Keep the bytes in `tmp` (made by [`Store::temp`]; `size` bytes whose
    /// BLAKE3 is `hash`) as `key`, if `cond` holds. Returns the files let go
    /// to stay within the budget, for whoever serves them to forget.
    pub fn commit(
        &self,
        key: &str,
        tmp: &Path,
        size: u64,
        hash: &str,
        cond: &Cond,
    ) -> io::Result<(Commit, Vec<PathBuf>)> {
        let dest = self.object_path(key);
        {
            let mut inner = self.inner.lock().expect("store");
            let held = inner.map.get(key);
            let ok = match cond {
                Cond::Any => true,
                Cond::IfAbsent => held.is_none(),
                Cond::IfMatch(h) => held.is_some_and(|o| &o.hash == h),
            };
            if !ok {
                drop(inner);
                self.discard(tmp);
                return Ok((Commit::Precondition, Vec::new()));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(tmp, &dest)?;
            let now = now_ms();
            let o = Obj {
                size,
                hash: hash.to_string(),
                modified: now,
                used: now,
            };
            if let Some(old) = inner.map.insert(key.to_string(), o.clone()) {
                inner.total -= old.size;
            }
            inner.total += size;
            self.append(&mut inner, key, Some(&o));
        }
        Ok((Commit::Stored, self.evict(Some(key))))
    }

    /// Forget an object (its file is removed).
    pub fn remove(&self, key: &str) -> Option<PathBuf> {
        let mut inner = self.inner.lock().expect("store");
        let o = inner.map.remove(key)?;
        inner.total -= o.size;
        self.append(&mut inner, key, None);
        let path = self.object_path(key);
        // A commit for this key must wait until the old file is gone.
        self.remove_owned(&path);
        drop(inner);
        Some(path)
    }

    /// Let go of the least recently used objects until the store is at 90%
    /// of its budget, never `keep`. Returns the files removed.
    fn evict(&self, keep: Option<&str>) -> Vec<PathBuf> {
        let budget = self.budget();
        let mut inner = self.inner.lock().expect("store");
        if inner.total <= budget {
            return Vec::new();
        }
        let target = budget / 10 * 9;
        let mut by_use: Vec<(u64, String, u64)> = inner
            .map
            .iter()
            .filter(|(k, _)| Some(k.as_str()) != keep)
            .map(|(k, o)| (o.used, k.clone(), o.size))
            .collect();
        by_use.sort();
        let mut gone = Vec::new();
        for (_, key, size) in by_use {
            if inner.total <= target {
                break;
            }
            inner.map.remove(&key);
            inner.total -= size;
            self.append(&mut inner, &key, None);
            let path = self.object_path(&key);
            self.remove_owned(&path);
            gone.push(path);
        }
        drop(inner);
        if !gone.is_empty() {
            tracing::info!(
                kache.evicted = gone.len(),
                kache.budget = budget,
                "cache objects let go to stay within the budget"
            );
        }
        gone
    }

    /// Remove a file this store made: under `objects/` or `tmp/`, a plain
    /// file (a symlink is never followed). Anything else is left alone and
    /// reported.
    fn remove_owned(&self, path: &Path) {
        let inside = (path.starts_with(&self.objects) || path.starts_with(&self.tmp))
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        let plain = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
        if !inside || !plain {
            if inside && std::fs::symlink_metadata(path).is_err() {
                return; // already gone
            }
            tracing::warn!(
                kache.outcome = "refused",
                "a cache removal outside the cache folder, or of something not a plain file, was refused"
            );
            return;
        }
        #[cfg(test)]
        tests::check_unlink_lock(self, path);
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!(error = %e, "a cache object could not be removed");
        }
    }

    fn append(&self, inner: &mut Inner, key: &str, o: Option<&Obj>) {
        let line = serde_json::to_string(&Line {
            k: key.to_string(),
            o: o.cloned(),
        })
        .expect("a line serializes");
        let ok = inner
            .journal
            .as_mut()
            .is_some_and(|f| writeln!(f, "{line}").is_ok());
        if !ok {
            tracing::warn!(
                "the cache index could not be written; it is rebuilt from what is on disk next time"
            );
        }
        inner.lines += 1;
        if inner.lines > 4 * inner.map.len() as u64 + 10_000 {
            let _ = self.compact(inner);
        }
    }

    /// Rewrite the journal as one line per object.
    fn compact(&self, inner: &mut Inner) -> io::Result<()> {
        let path = self.root.join("index.jsonl");
        let next = self.root.join("index.jsonl.next");
        {
            let mut f = io::BufWriter::new(File::create(&next)?);
            for (k, o) in &inner.map {
                let line = serde_json::to_string(&Line {
                    k: k.clone(),
                    o: Some(o.clone()),
                })
                .expect("a line serializes");
                writeln!(f, "{line}")?;
            }
            f.flush()?;
        }
        std::fs::rename(&next, &path)?;
        inner.journal = Some(std::fs::OpenOptions::new().append(true).open(&path)?);
        inner.lines = inner.map.len() as u64;
        Ok(())
    }
}

fn read_journal(path: &Path) -> HashMap<String, Obj> {
    let mut map = HashMap::new();
    let Ok(f) = File::open(path) else {
        return map;
    };
    for line in io::BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(l) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        match l.o {
            Some(o) => {
                map.insert(l.k, o);
            }
            None => {
                map.remove(&l.k);
            }
        }
    }
    map
}

/// What a store in `root` keeps, read without opening it (for a command
/// line while the daemon runs): (objects, bytes).
pub fn summary(root: &Path) -> (usize, u64) {
    let map = read_journal(&root.join("index.jsonl"));
    (map.len(), map.values().map(|o| o.size).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static CHECK_UNLINK_LOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn check_unlink_lock(store: &Store, path: &Path) {
        if path.starts_with(&store.objects) && CHECK_UNLINK_LOCK.get() {
            assert!(
                matches!(
                    store.inner.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "an object must be unlinked while commits are excluded"
            );
        }
    }

    #[test]
    fn removal_unlinks_before_releasing_the_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), 1024).unwrap();
        put(&store, "key", b"old");
        CHECK_UNLINK_LOCK.set(true);
        let path = store.remove("key").unwrap();
        CHECK_UNLINK_LOCK.set(false);
        assert!(!path.exists());
    }

    #[test]
    fn eviction_unlinks_before_releasing_the_store_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), 1024).unwrap();
        put(&store, "key", b"old");
        CHECK_UNLINK_LOCK.set(true);
        store.set_budget(0);
        CHECK_UNLINK_LOCK.set(false);
        assert!(!store.object_path("key").exists());
    }

    fn put(s: &Store, key: &str, bytes: &[u8]) -> Vec<PathBuf> {
        let (tmp, mut f) = s.temp().unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        let hash = blake3::hash(bytes).to_hex().to_string();
        let (c, gone) = s
            .commit(key, &tmp, bytes.len() as u64, &hash, &Cond::Any)
            .unwrap();
        assert_eq!(c, Commit::Stored);
        gone
    }

    #[test]
    fn conditions() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path(), 1 << 20).unwrap();
        put(&s, "a", b"one");
        let h1 = blake3::hash(b"one").to_hex().to_string();
        let (tmp, _) = s.temp().unwrap();
        let (c, _) = s.commit("a", &tmp, 0, "x", &Cond::IfAbsent).unwrap();
        assert_eq!(c, Commit::Precondition);
        assert!(!tmp.exists(), "a refused write leaves no temporary file");
        let (tmp, mut f) = s.temp().unwrap();
        f.write_all(b"two").unwrap();
        let h2 = blake3::hash(b"two").to_hex().to_string();
        let (c, _) = s
            .commit("a", &tmp, 3, &h2, &Cond::IfMatch("stale".into()))
            .unwrap();
        assert_eq!(c, Commit::Precondition);
        let (tmp, mut f) = s.temp().unwrap();
        f.write_all(b"two").unwrap();
        let (c, _) = s.commit("a", &tmp, 3, &h2, &Cond::IfMatch(h1)).unwrap();
        assert_eq!(c, Commit::Stored);
        let (path, o) = s.get("a").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"two");
        assert_eq!(o.hash, h2);
    }

    #[test]
    fn least_recently_used_goes_first_and_only_cache_files_go() {
        let t = tempfile::tempdir().unwrap();
        let agent = t.path();
        let root = agent.join("kache");
        // Things beside and inside the cache folder that are not its objects.
        std::fs::write(agent.join("config.json"), b"{}").unwrap();
        std::fs::create_dir_all(agent.join("cache")).unwrap();
        std::fs::write(agent.join("cache/blob"), b"user bytes").unwrap();
        let s = Store::open(&root, 1000).unwrap();
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();
        #[cfg(unix)]
        {
            // A link planted where an object would be, pointing outside.
            let name = key_name("planted");
            let p = root.join("objects").join(&name[..2]);
            std::fs::create_dir_all(&p).unwrap();
            std::os::unix::fs::symlink(agent.join("config.json"), p.join(&name)).unwrap();
        }
        put(&s, "old", &[1; 400]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        put(&s, "mid", &[2; 400]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // `old` used again: `mid` is now the least recently used.
        s.inner.lock().unwrap().map.get_mut("old").unwrap().used = now_ms() + 1;
        let gone = put(&s, "new", &[3; 400]);
        assert_eq!(gone, vec![s.object_path("mid")]);
        assert!(s.get("mid").is_none() && s.get("old").is_some() && s.get("new").is_some());
        assert_eq!(s.stats(), (2, 800));
        // A smaller budget lets more go, never the files beside the cache.
        s.set_budget(100);
        assert_eq!(s.stats().0, 0);
        assert_eq!(std::fs::read(agent.join("config.json")).unwrap(), b"{}");
        assert_eq!(
            std::fs::read(agent.join("cache/blob")).unwrap(),
            b"user bytes"
        );
        assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"mine");
        // A removal outside the cache folder is refused.
        s.remove_owned(&agent.join("config.json"));
        s.remove_owned(&root.join("objects/../../config.json"));
        assert!(agent.join("config.json").exists());
        drop(s);
        // Reopened: the planted link is not a plain file, so it stays (and
        // what it points at is untouched); the index matches the disk.
        let s = Store::open(&root, 1000).unwrap();
        assert_eq!(s.stats(), (0, 0));
        assert!(agent.join("config.json").exists());
    }

    #[test]
    fn reopened_store_keeps_its_objects() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path(), 1 << 20).unwrap();
        put(&s, "a/b/c", b"hello");
        put(&s, "a/b/d", b"world");
        s.remove("a/b/d");
        // An orphan (a file no entry names) and a leftover temporary file.
        let orphan = s.object_path("orphan");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"x").unwrap();
        let (tmp, _) = s.temp().unwrap();
        drop(s);
        let s = Store::open(t.path(), 1 << 20).unwrap();
        assert_eq!(s.stats(), (1, 5));
        assert_eq!(std::fs::read(s.get("a/b/c").unwrap().0).unwrap(), b"hello");
        assert!(!orphan.exists() && !tmp.exists());
        assert_eq!(summary(t.path()), (1, 5));
    }
}
