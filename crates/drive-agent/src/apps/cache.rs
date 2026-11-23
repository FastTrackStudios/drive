//! The app cache: files fetched from other computers for apps, kept in the
//! agent's own folder (`<agent>/app-cache`), content-addressed by their
//! full BLAKE3 (`objects/<h[..2]>/<h>`), listed in an append-only journal
//! (`index.jsonl`), under a size budget with least-recently-used eviction
//! of entries no app holds.
//!
//! A fetch writes `partial/<h>.part` and is renamed into `objects/` only
//! once every byte came and the hash checks out, so an app is never handed
//! a partial file, and a fetch that stopped (the agent stopped, the app
//! cancelled, the network went) resumes from the partial's length.
//!
//! This is derived data, so it is the one place (with the kache store) the
//! agent deletes files on its own: only plain files under `objects/` and
//! `partial/` of this folder, through [`AppCache::remove_owned`], which
//! checks it. Never a location's file, never anything an app holds.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::now_ms;

/// A use is written to the journal at most this often per object.
const TOUCH_EVERY_MS: u64 = 60_000;
/// A partial file not touched for this long is let go when the cache opens.
const PARTIAL_KEEP_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obj {
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "a")]
    pub added: u64,
    #[serde(rename = "u")]
    pub used: u64,
}

/// What a name (`LOCATION:path`, `DEVICE:/path`) was when last fetched:
/// lets an app open a cached copy while that computer is off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    #[serde(rename = "h")]
    pub hash: String,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "m")]
    pub mtime: i64,
}

#[derive(Serialize, Deserialize)]
struct Line {
    k: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    o: Option<Obj>,
    /// A name line instead of an object line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<Named>,
}

struct Inner {
    map: HashMap<String, Obj>,
    names: HashMap<String, Named>,
    total: u64,
    journal: Option<File>,
    lines: u64,
}

pub struct AppCache {
    root: PathBuf,
    objects: PathBuf,
    partial: PathBuf,
    budget: AtomicU64,
    inner: Mutex<Inner>,
}

/// A full BLAKE3 in hex: the only names the cache makes files for.
pub fn valid_hash(h: &str) -> bool {
    h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn fits_budget(used: u64, need: u64, budget: u64) -> bool {
    used.checked_add(need).is_some_and(|total| total <= budget)
}

impl AppCache {
    /// Open (or make) the cache in `root`, keeping at most `budget` bytes of
    /// entries no app holds. Entries whose file is gone are forgotten;
    /// stray files in `objects/` and old partials are removed.
    pub fn open(root: &Path, budget: u64) -> io::Result<AppCache> {
        let objects = root.join("objects");
        let partial = root.join("partial");
        std::fs::create_dir_all(&objects)?;
        std::fs::create_dir_all(&partial)?;
        let c = AppCache {
            root: root.to_path_buf(),
            objects,
            partial,
            budget: AtomicU64::new(budget),
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                names: HashMap::new(),
                total: 0,
                journal: None,
                lines: 0,
            }),
        };
        let (mut map, names) = read_journal(&root.join("index.jsonl"));
        map.retain(|h, o| {
            valid_hash(h)
                && std::fs::symlink_metadata(c.object_path(h))
                    .is_ok_and(|m| m.is_file() && m.len() == o.size)
        });
        for shard in std::fs::read_dir(&c.objects)?.flatten() {
            if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for f in std::fs::read_dir(shard.path())?.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if f.file_type().is_ok_and(|t| t.is_file()) && !map.contains_key(&name) {
                    c.remove_owned(&f.path());
                }
            }
        }
        let now = std::time::SystemTime::now();
        for f in std::fs::read_dir(&c.partial)?.flatten() {
            let old = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|d| d.as_millis() as u64 > PARTIAL_KEEP_MS);
            let name = f.file_name().to_string_lossy().into_owned();
            let ours = name.strip_suffix(".part").is_some_and(valid_hash);
            if old || !ours {
                c.remove_owned(&f.path());
            }
        }
        {
            let mut inner = c.inner.lock().expect("cache");
            inner.total = map.values().map(|o| o.size).sum();
            inner.map = map;
            inner.names = names;
            c.compact(&mut inner)?;
        }
        Ok(c)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    pub fn set_budget(&self, budget: u64, pinned: &dyn Fn(&str) -> bool) {
        self.budget.store(budget, Ordering::Relaxed);
        self.evict(None, 0, pinned);
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.objects.join(&hash[..2.min(hash.len())]).join(hash)
    }

    pub fn partial_path(&self, hash: &str) -> PathBuf {
        self.partial.join(format!("{hash}.part"))
    }

    /// A kept file, marked used now.
    pub fn get(&self, hash: &str) -> Option<(PathBuf, u64)> {
        let mut inner = self.inner.lock().expect("cache");
        let now = now_ms();
        let o = inner.map.get_mut(hash)?;
        let touch = now.saturating_sub(o.used) > TOUCH_EVERY_MS;
        o.used = now;
        let o = o.clone();
        if touch {
            self.append(&mut inner, hash, Some(&o), None);
        }
        Some((self.object_path(hash), o.size))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.inner.lock().expect("cache").map.contains_key(hash)
    }

    /// (files, bytes) kept now.
    pub fn stats(&self) -> (u64, u64) {
        let inner = self.inner.lock().expect("cache");
        (inner.map.len() as u64, inner.total)
    }

    /// (files, bytes) of fetches that stopped part way.
    pub fn partial_stats(&self) -> (u64, u64) {
        let mut n = (0, 0);
        if let Ok(rd) = std::fs::read_dir(&self.partial) {
            for f in rd.flatten() {
                n.0 += 1;
                n.1 += f.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        n
    }

    /// Keep the finished partial of `hash` (`size` bytes, already checked
    /// against the hash), then let go of what the budget asks, never what
    /// `pinned` holds.
    pub fn commit(
        &self,
        hash: &str,
        size: u64,
        pinned: &dyn Fn(&str) -> bool,
    ) -> io::Result<PathBuf> {
        if !valid_hash(hash) {
            return Err(io::Error::other("not a content hash"));
        }
        let dest = self.object_path(hash);
        {
            let mut inner = self.inner.lock().expect("cache");
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(self.partial_path(hash), &dest)?;
            let now = now_ms();
            let o = Obj {
                size,
                added: now,
                used: now,
            };
            if let Some(old) = inner.map.insert(hash.to_string(), o.clone()) {
                inner.total -= old.size;
            }
            inner.total += size;
            self.append(&mut inner, hash, Some(&o), None);
        }
        self.evict(Some(hash), 0, pinned);
        Ok(dest)
    }

    /// Make room for `need` more bytes before a fetch (never what `pinned`
    /// holds). Returns whether there is room now.
    pub fn make_room(&self, need: u64, pinned: &dyn Fn(&str) -> bool) -> bool {
        self.evict(None, need, pinned);
        let inner = self.inner.lock().expect("cache");
        fits_budget(inner.total, need, self.budget())
    }

    /// Remember what a name was when it was fetched.
    pub fn remember(&self, name: &str, named: Named) {
        let mut inner = self.inner.lock().expect("cache");
        if inner.names.get(name) == Some(&named) {
            return;
        }
        inner.names.insert(name.to_string(), named.clone());
        self.append(&mut inner, name, None, Some(&named));
    }

    pub fn named(&self, name: &str) -> Option<Named> {
        self.inner.lock().expect("cache").names.get(name).cloned()
    }

    /// Let go of the least recently used entries no app holds until the
    /// cache (plus `need`) is at 90% of its budget, never `keep`.
    fn evict(&self, keep: Option<&str>, need: u64, pinned: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
        let budget = self.budget();
        let mut inner = self.inner.lock().expect("cache");
        if fits_budget(inner.total, need, budget) {
            return Vec::new();
        }
        let target = (budget / 10 * 9).saturating_sub(need);
        let mut by_use: Vec<(u64, String, u64)> = inner
            .map
            .iter()
            .filter(|(h, _)| Some(h.as_str()) != keep && !pinned(h))
            .map(|(h, o)| (o.used, h.clone(), o.size))
            .collect();
        by_use.sort();
        let mut gone = Vec::new();
        for (_, h, size) in by_use {
            if inner.total <= target {
                break;
            }
            inner.map.remove(&h);
            inner.total -= size;
            self.append(&mut inner, &h, None, None);
            let path = self.object_path(&h);
            // A commit for the same hash must wait until this unlink finishes.
            self.remove_owned(&path);
            gone.push(path);
        }
        let over = !fits_budget(inner.total, need, budget);
        drop(inner);
        if !gone.is_empty() {
            tracing::info!(
                apps.evicted = gone.len(),
                apps.budget = budget,
                "app cache files let go to stay within the budget"
            );
        }
        if over {
            tracing::warn!(
                apps.budget = budget,
                apps.need = need,
                "the app cache is over its budget: what is left is held by apps"
            );
        }
        gone
    }

    /// Remove a file this cache made: a plain file (never a symlink) under
    /// `objects/` or `partial/`. Anything else is refused and reported.
    pub(crate) fn remove_owned(&self, path: &Path) {
        let inside = (path.starts_with(&self.objects) || path.starts_with(&self.partial))
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        let meta = std::fs::symlink_metadata(path);
        if inside && meta.is_err() {
            return; // already gone
        }
        if !inside || !meta.is_ok_and(|m| m.is_file()) {
            tracing::warn!(
                apps.outcome = "refused",
                "an app cache removal outside the cache folder, or of something not a plain file, was refused"
            );
            return;
        }
        #[cfg(test)]
        tests::check_unlink_lock(self, path);
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!(error = %e, "an app cache file could not be removed");
        }
    }

    fn append(&self, inner: &mut Inner, key: &str, o: Option<&Obj>, n: Option<&Named>) {
        let line = serde_json::to_string(&Line {
            k: key.to_string(),
            o: o.cloned(),
            n: n.cloned(),
        })
        .expect("a line serializes");
        let ok = inner
            .journal
            .as_mut()
            .is_some_and(|f| writeln!(f, "{line}").is_ok());
        if !ok {
            tracing::warn!(
                "the app cache index could not be written; it is rebuilt from disk next time"
            );
        }
        inner.lines += 1;
        if inner.lines > 4 * (inner.map.len() + inner.names.len()) as u64 + 10_000 {
            let _ = self.compact(inner);
        }
    }

    fn compact(&self, inner: &mut Inner) -> io::Result<()> {
        let path = self.root.join("index.jsonl");
        let next = self.root.join("index.jsonl.next");
        {
            let mut f = io::BufWriter::new(File::create(&next)?);
            for (k, o) in &inner.map {
                let l = Line {
                    k: k.clone(),
                    o: Some(o.clone()),
                    n: None,
                };
                writeln!(f, "{}", serde_json::to_string(&l).expect("a line"))?;
            }
            for (k, n) in &inner.names {
                let l = Line {
                    k: k.clone(),
                    o: None,
                    n: Some(n.clone()),
                };
                writeln!(f, "{}", serde_json::to_string(&l).expect("a line"))?;
            }
            f.flush()?;
        }
        std::fs::rename(&next, &path)?;
        inner.journal = Some(std::fs::OpenOptions::new().append(true).open(&path)?);
        inner.lines = (inner.map.len() + inner.names.len()) as u64;
        Ok(())
    }
}

fn read_journal(path: &Path) -> (HashMap<String, Obj>, HashMap<String, Named>) {
    let (mut map, mut names) = (HashMap::new(), HashMap::new());
    let Ok(f) = File::open(path) else {
        return (map, names);
    };
    for line in io::BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(l) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        match (l.o, l.n) {
            (_, Some(n)) => {
                names.insert(l.k, n);
            }
            (Some(o), None) => {
                map.insert(l.k, o);
            }
            (None, None) => {
                map.remove(&l.k);
            }
        }
    }
    (map, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static CHECK_UNLINK_LOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn check_unlink_lock(cache: &AppCache, path: &Path) {
        if path.starts_with(&cache.objects) && CHECK_UNLINK_LOCK.get() {
            assert!(
                matches!(
                    cache.inner.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "eviction must exclude commits until the old file is unlinked"
            );
        }
    }

    #[test]
    fn eviction_keeps_the_cache_lock_until_the_file_is_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AppCache::open(dir.path(), 1024).unwrap();
        let none = |_: &str| false;
        let hash = put(&cache, b"old", &none);

        CHECK_UNLINK_LOCK.set(true);
        cache.set_budget(0, &none);
        CHECK_UNLINK_LOCK.set(false);

        assert!(!cache.object_path(&hash).exists());
        assert!(!cache.has(&hash));
    }

    #[test]
    fn eviction_handles_requests_near_the_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AppCache::open(dir.path(), u64::MAX).unwrap();
        let none = |_: &str| false;
        let hash = put(&cache, b"old", &none);

        assert!(cache.make_room(u64::MAX, &none));
        assert!(!cache.has(&hash));
    }

    fn put(c: &AppCache, bytes: &[u8], pinned: &dyn Fn(&str) -> bool) -> String {
        let h = blake3::hash(bytes).to_hex().to_string();
        std::fs::write(c.partial_path(&h), bytes).unwrap();
        c.commit(&h, bytes.len() as u64, pinned).unwrap();
        h
    }

    #[test]
    fn pinned_entries_are_never_evicted_and_only_cache_files_go() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("app-cache");
        // A user's file and a symlink planted beside and inside the cache.
        std::fs::write(t.path().join("user.wav"), b"mine").unwrap();
        let c = AppCache::open(&root, 3000).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("user.wav"), root.join("objects/link")).unwrap();
        let none = |_: &str| false;
        let a = put(&c, &[1u8; 1000], &none);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = put(&c, &[2u8; 1000], &none);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // `a` is the oldest but held by an app: `b` goes instead.
        let pin_a = |h: &str| h == a;
        let d = put(&c, &[3u8; 1500], &pin_a);
        assert!(c.has(&a) && !c.has(&b) && c.has(&d));
        assert!(!c.object_path(&b).exists());
        // Everything held: the cache stays over budget rather than evict.
        let all = |_: &str| true;
        let e = put(&c, &[4u8; 1000], &all);
        assert!(c.has(&a) && c.has(&d) && c.has(&e));
        assert!(c.stats().1 > c.budget());
        // A removal outside the cache folder is refused.
        c.remove_owned(&t.path().join("user.wav"));
        assert!(t.path().join("user.wav").exists());
        // Reopened: the same entries; the planted symlink is not a file of
        // ours and is not followed (the user's file stays).
        drop(c);
        let c = AppCache::open(&root, 1 << 30).unwrap();
        assert!(c.has(&a) && c.has(&d) && c.has(&e) && !c.has(&b));
        assert_eq!(std::fs::read(t.path().join("user.wav")).unwrap(), b"mine");
    }

    #[test]
    fn names_and_partials_survive_a_restart() {
        let t = tempfile::tempdir().unwrap();
        let c = AppCache::open(t.path(), 1 << 20).unwrap();
        let h = "ab".repeat(32);
        std::fs::write(c.partial_path(&h), b"half").unwrap();
        std::fs::write(t.path().join("partial/stray.bin"), b"x").unwrap();
        c.remember(
            "loc:Kits/kick.wav",
            Named {
                hash: h.clone(),
                size: 8,
                mtime: 1,
            },
        );
        drop(c);
        let c = AppCache::open(t.path(), 1 << 20).unwrap();
        assert_eq!(std::fs::read(c.partial_path(&h)).unwrap(), b"half");
        assert!(!t.path().join("partial/stray.bin").exists());
        assert_eq!(c.named("loc:Kits/kick.wav").unwrap().hash, h);
        assert!(!valid_hash("../../etc/passwd") && !valid_hash(&"AB".repeat(32)));
    }
}
