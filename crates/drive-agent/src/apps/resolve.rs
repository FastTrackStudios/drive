//! From a name to a local path.
//!
//! [`Apps::resolve`] reads a target (the names the MCP tools take) into a
//! [`Source`] without moving any bytes; [`Apps::materialize`] makes it a
//! local path:
//!
//! - this device's own file (one of its locations, a path on its disks):
//!   the file itself, never copied;
//! - a Drive tree file: the agent's cache copy (`<agent>/cache/<hash>`, the
//!   copy the mount and `drive status` see), fetched if missing from a
//!   device that keeps it (over iroh), else the node, else this device's
//!   storages; resumed by range, checked against the hash;
//! - a file on another computer: that device makes it fetchable and it
//!   comes over iroh-blobs into the app cache, verified, resumably.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use drive_files::space::{self, Location};
use drive_files::tree;
use drive_local::{How, LocalFile, OpenOptions};
use futures::StreamExt;

use super::Apps;
use super::cache::{Named, valid_hash};
use super::peer::Ask;

/// Told how many bytes of the file have arrived so far.
pub type OnBytes<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// Where a target's bytes are.
#[derive(Clone, Debug)]
pub enum Source {
    /// This device's own file.
    Local { path: PathBuf, size: u64 },
    /// A Drive tree file (its blob's full BLAKE3).
    Tree { hash: String, size: u64 },
    /// Content by its full BLAKE3: the caches, else as a tree blob.
    Content { hash: String },
    /// A file on another computer.
    Remote {
        device: String,
        device_name: String,
        ask: Ask,
        /// What the app cache remembers it as (`<location id>:path`,
        /// `<device id>:/path`).
        name: String,
        /// Its size as last indexed, when known.
        size: Option<u64>,
    },
}

impl Source {
    /// Bytes to fetch, as far as is known without asking anyone.
    pub fn size_hint(&self) -> u64 {
        match self {
            Source::Local { size, .. } | Source::Tree { size, .. } => *size,
            Source::Remote { size, .. } => size.unwrap_or(0),
            Source::Content { .. } => 0,
        }
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Apps {
    pub fn location(&self, name: &str) -> Option<Location> {
        space::locations(&self.a.space)
            .into_iter()
            .find(|l| l.id == name || l.name.eq_ignore_ascii_case(name))
    }

    fn device(&self, name: &str) -> Option<(String, String)> {
        space::devices(&self.a.space)
            .into_iter()
            .find(|d| d.id == name || d.name.eq_ignore_ascii_case(name))
            .map(|d| (d.id, d.name))
    }

    fn device_name(&self, id: &str) -> String {
        self.device(id).map_or_else(|| id.to_string(), |(_, n)| n)
    }

    /// A location's file, from wherever that location is.
    pub fn location_source(&self, loc: &Location, path: &str) -> Result<Source, String> {
        let path = path.trim_matches('/').to_string();
        if !loc.storage.is_empty() {
            return Err(format!(
                "{} is in a storage, not on a computer's disk: not reachable this way yet",
                loc.name
            ));
        }
        if loc.device == self.a.device_id() {
            let rules = crate::transfer::location_rules(&loc.path, &self.a.dir);
            let file = rules.resolve(&loc.path, &path)?;
            let meta = std::fs::metadata(&file).map_err(|e| format!("{path}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("{}:{path} is a folder", loc.name));
            }
            return Ok(Source::Local {
                path: file,
                size: meta.len(),
            });
        }
        let size = self
            .a
            .cached_index(&loc.id)
            .and_then(|i| i.entries.iter().find(|e| e.path == path && !e.dir).cloned())
            .map(|e| e.size);
        Ok(Source::Remote {
            device: loc.device.clone(),
            device_name: self.device_name(&loc.device),
            name: format!("{}:{path}", loc.id),
            ask: Ask::ProvideLocation {
                location: loc.id.clone(),
                path,
            },
            size,
        })
    }

    /// Read a target into where its bytes are. Moves nothing.
    pub async fn resolve(&self, target: &str) -> Result<Source, String> {
        let t = target.trim();
        if let Some(path) = t.strip_prefix("drive:") {
            let path = path.trim_matches('/');
            let e = tree::resolve(&self.a.files, path)
                .ok_or_else(|| format!("no {path} in the Drive"))?
                .ok_or("the Drive's top folder is not a file")?;
            return tree_source(&e, path);
        }
        if let Some(id) = t.strip_prefix("file:") {
            let tid = tree::parse_id(id).ok_or_else(|| format!("{id} is not a tree id"))?;
            let e = tree::entry(&self.a.files, tid).ok_or_else(|| format!("no file {id}"))?;
            return tree_source(&e, id);
        }
        if let Some(h) = t.strip_prefix("content:") {
            let h = h.trim().to_ascii_lowercase();
            if !is_hex64(&h) {
                return Err("content:HASH takes a 64-digit hex hash".into());
            }
            return self.content_source(&h).await;
        }
        let p = Path::new(t);
        if p.is_absolute() {
            let meta = std::fs::metadata(p).map_err(|e| format!("{t}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("{t} is not a file"));
            }
            return Ok(Source::Local {
                path: p.to_path_buf(),
                size: meta.len(),
            });
        }
        if let Some((name, rest)) = t.split_once(':') {
            if let Some(loc) = self.location(name) {
                return self.location_source(&loc, rest);
            }
            if rest.starts_with('/')
                && let Some((dev, dev_name)) = self.device(name)
            {
                if dev == self.a.device_id() {
                    return Box::pin(self.resolve(rest)).await;
                }
                return Ok(Source::Remote {
                    name: format!("{dev}:{rest}"),
                    device: dev,
                    device_name: dev_name,
                    ask: Ask::ProvidePath {
                        path: rest.to_string(),
                    },
                    size: None,
                });
            }
        }
        Err(format!(
            "{t}: name a file as drive:PATH, LOCATION:PATH, content:HASH, DEVICE:/path or an absolute path here"
        ))
    }

    /// `content:HASH`: a full BLAKE3 the caches or the tree know, else a
    /// sampled hash an index knows (this device's own copy first).
    async fn content_source(&self, h: &str) -> Result<Source, String> {
        if self.a.is_cached(h) || self.cache.has(h) {
            return Ok(Source::Content {
                hash: h.to_string(),
            });
        }
        let indexes = self.a.refresh_indexes().await;
        let me = self.a.device_id();
        let online = crate::daemon::online_devices(&self.a);
        let mut found: Vec<(Location, String)> = Vec::new();
        for loc in space::locations(&self.a.space) {
            if let Some(idx) = indexes.get(&loc.id) {
                for e in idx.entries.iter().filter(|e| !e.dir && e.sampled == h) {
                    found.push((loc.clone(), e.path.clone()));
                }
            }
        }
        // Here first, then a computer that is on.
        found.sort_by_key(|(l, _)| (l.device != me, !online.contains(&l.device)));
        if let Some((loc, path)) = found.into_iter().next() {
            return self.location_source(&loc, &path);
        }
        if !space::holders(&self.a.space, h).is_empty() {
            return Ok(Source::Content {
                hash: h.to_string(),
            });
        }
        Err(format!("no file with content {h} is known"))
    }

    /// Make a source a local path.
    pub async fn materialize(
        &self,
        src: &Source,
        opts: &OpenOptions,
        on_bytes: OnBytes<'_>,
    ) -> Result<LocalFile, String> {
        match src {
            Source::Local { path, size } => {
                let hash = if opts.hash {
                    Some(self.local_hash(path).await?)
                } else {
                    None
                };
                on_bytes(*size);
                Ok(LocalFile {
                    path: path.clone(),
                    size: *size,
                    hash,
                    how: How::Local,
                })
            }
            Source::Tree { hash, size } => self.tree_file(hash, Some(*size), on_bytes).await,
            Source::Content { hash } => {
                if let Some((path, size)) = self.cache.get(hash) {
                    on_bytes(size);
                    return Ok(LocalFile {
                        path,
                        size,
                        hash: Some(hash.clone()),
                        how: How::Cached,
                    });
                }
                self.tree_file(hash, None, on_bytes).await
            }
            Source::Remote {
                device,
                device_name,
                ask,
                name,
                ..
            } => {
                self.remote_file(device, device_name, ask, name, on_bytes)
                    .await
            }
        }
    }

    /// The full BLAKE3 of a file of this device (remembered per size and
    /// time by the provider when it runs).
    async fn local_hash(&self, path: &Path) -> Result<String, String> {
        if let Some(p) = self.a.provider.get() {
            return p.provide(path).await.map(|g| g.hash);
        }
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            drive_files::hash::of_file(&path).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// A tree blob in the agent's cache, fetched if missing.
    async fn tree_file(
        &self,
        hash: &str,
        size: Option<u64>,
        on_bytes: OnBytes<'_>,
    ) -> Result<LocalFile, String> {
        let path = self.a.cache_path(hash);
        if let Ok(m) = std::fs::metadata(&path) {
            on_bytes(m.len());
            return Ok(LocalFile {
                path,
                size: m.len(),
                hash: Some(hash.to_string()),
                how: How::Cached,
            });
        }
        let lock = self.lock(hash);
        let _g = lock.lock().await;
        if let Ok(m) = std::fs::metadata(&path) {
            on_bytes(m.len());
            return Ok(LocalFile {
                path,
                size: m.len(),
                hash: Some(hash.to_string()),
                how: How::Cached,
            });
        }
        let started = std::time::Instant::now();
        let part = path.with_extension("part");
        let (size, from, via) = self.fetch_tree(hash, size, &part, on_bytes).await?;
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        let me = self.a.device_id();
        let _ = space::set_held(&self.a.space, &me, hash, Some("local"));
        self.a.commit_space();
        tracing::info!(
            apps.outcome = "fetched",
            apps.source = via,
            apps.bytes = size,
            apps.resumed_from = from,
            apps.ms = started.elapsed().as_millis() as u64,
            "a Drive file came into this device's cache for an app"
        );
        Ok(LocalFile {
            path,
            size,
            hash: Some(hash.to_string()),
            how: How::Fetched,
        })
    }

    /// Fetch a tree blob into `part`: from a device that keeps it (iroh),
    /// else the node (HTTP, by range), else this device's storages.
    /// Returns (size, resumed from, where from).
    async fn fetch_tree(
        &self,
        hash: &str,
        size: Option<u64>,
        part: &Path,
        on_bytes: OnBytes<'_>,
    ) -> Result<(u64, u64, &'static str), String> {
        let me = self.a.device_id();
        let mut why = Vec::new();
        // Devices that keep it, asked one after another (rarely more than one).
        let keepers: Vec<String> = space::holders(&self.a.space, hash)
            .into_iter()
            .filter(|(who, how)| *who != me && (how == "local" || how == "pinned"))
            .map(|(who, _)| who)
            .filter(|who| space::endpoints(&self.a.space).contains_key(who))
            .collect();
        for dev in keepers {
            match self.ask(&dev, &Ask::Blob(hash.to_string())).await {
                Ok((addr, ans)) if ans.hash.as_deref() == Some(hash) => {
                    let src = drive_transfer::Source {
                        addr,
                        hash: hash.parse().map_err(|_| "a bad hash")?,
                        size: ans.size,
                    };
                    match self.fetch_iroh(src, part, on_bytes).await {
                        Ok(from) => return Ok((ans.size, from, "device")),
                        Err(e) => why.push(format!("{}: {e}", self.device_name(&dev))),
                    }
                }
                Ok((_, ans)) => why.push(format!(
                    "{}: {}",
                    self.device_name(&dev),
                    ans.err.unwrap_or_else(|| "another hash".into())
                )),
                Err(e) => why.push(format!("{}: {e}", self.device_name(&dev))),
            }
        }
        match self.fetch_node(hash, size, part, on_bytes).await {
            Ok((n, from)) => return Ok((n, from, "node")),
            Err(e) => why.push(format!("the node: {e}")),
        }
        for (_, store) in &self.a.stores {
            if let Ok(bytes) = store.get(hash).await
                && blake3::hash(&bytes).to_hex().as_str() == hash
            {
                std::fs::write(part, &bytes).map_err(|e| e.to_string())?;
                on_bytes(bytes.len() as u64);
                return Ok((bytes.len() as u64, 0, "storage"));
            }
        }
        Err(format!(
            "no copy of it could be fetched ({})",
            why.join("; ")
        ))
    }

    /// From the node, resuming by range from what `part` already has.
    async fn fetch_node(
        &self,
        hash: &str,
        size: Option<u64>,
        part: &Path,
        on_bytes: OnBytes<'_>,
    ) -> Result<(u64, u64), String> {
        let url = format!(
            "{}/blobs/{hash}",
            self.a.cfg.lock().expect("cfg").node_http()
        );
        let mut w = Partial::open(part)?;
        let from = w.len;
        let mut req = self.a.http.get(&url);
        if from > 0 {
            req = req.header("Range", format!("bytes={from}-"));
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        match resp.status().as_u16() {
            206 => {}
            200 if from > 0 => w.restart()?,
            200 => {}
            416 if Some(from) == size => {
                on_bytes(from);
                return w.finish(hash, size).map(|n| (n, from));
            }
            s => return Err(format!("answered {s}")),
        }
        let mut resp = resp;
        on_bytes(w.len);
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            w.push(&chunk)?;
            on_bytes(w.len);
        }
        let n = w.finish(hash, size)?;
        Ok((n, from))
    }

    /// From a device over iroh-blobs into `part`, resuming from what it
    /// already holds. Returns where it resumed from.
    pub async fn fetch_iroh(
        &self,
        src: drive_transfer::Source,
        part: &Path,
        on_bytes: OnBytes<'_>,
    ) -> Result<u64, String> {
        let fetcher = self.fetcher()?;
        let hash = src.hash.to_hex().to_string();
        let size = src.size;
        let mut w = Partial::open(part)?;
        if w.len > size {
            w.restart()?;
        }
        let from = w.len;
        on_bytes(from);
        if from < size {
            let mut s = std::pin::pin!(fetcher.stream(src, from, size - 1));
            while let Some(chunk) = s.next().await {
                let chunk = chunk.map_err(|e| e.to_string())?;
                w.push(&chunk)?;
                on_bytes(w.len);
            }
        }
        w.finish(&hash, Some(size))?;
        Ok(from)
    }

    /// A file of another computer, in the app cache.
    async fn remote_file(
        &self,
        device: &str,
        device_name: &str,
        ask: &Ask,
        name: &str,
        on_bytes: OnBytes<'_>,
    ) -> Result<LocalFile, String> {
        let asked = self.ask(device, ask).await;
        let (addr, ans) = match asked {
            Ok((addr, ans)) if ans.hash.is_some() => (addr, ans),
            Ok((_, ans)) => {
                return Err(format!(
                    "{device_name}: {}",
                    ans.err.unwrap_or_else(|| "no answer".into())
                ));
            }
            Err(e) => {
                // Off (or unreachable): the copy fetched last time, if any.
                if let Some(n) = self.cache.named(name)
                    && let Some((path, size)) = self.cache.get(&n.hash)
                {
                    tracing::info!(
                        apps.outcome = "cached_offline",
                        "a file of a computer that is off opened from the cache"
                    );
                    on_bytes(size);
                    return Ok(LocalFile {
                        path,
                        size,
                        hash: Some(n.hash),
                        how: How::Cached,
                    });
                }
                return Err(format!(
                    "{device_name} cannot be reached ({e}), and this file is not in this device's cache"
                ));
            }
        };
        let hash = ans.hash.expect("checked");
        if !valid_hash(&hash) {
            return Err(format!("{device_name} answered with a bad hash"));
        }
        let named = Named {
            hash: hash.clone(),
            size: ans.size,
            mtime: ans.mtime,
        };
        if let Some((path, size)) = self.cache.get(&hash) {
            self.cache.remember(name, named);
            on_bytes(size);
            return Ok(LocalFile {
                path,
                size,
                hash: Some(hash),
                how: How::Cached,
            });
        }
        // The same bytes already in the Drive tree's cache.
        let tree = self.a.cache_path(&hash);
        if tree.is_file() {
            on_bytes(ans.size);
            return Ok(LocalFile {
                path: tree,
                size: ans.size,
                hash: Some(hash),
                how: How::Cached,
            });
        }
        let lock = self.lock(&hash);
        let _g = lock.lock().await;
        if let Some((path, size)) = self.cache.get(&hash) {
            self.cache.remember(name, named);
            on_bytes(size);
            return Ok(LocalFile {
                path,
                size,
                hash: Some(hash),
                how: How::Cached,
            });
        }
        let leases = self.a.leases.clone();
        let pinned = move |h: &str| leases.holds_hash(h);
        if !self.cache.make_room(ans.size, &pinned) {
            tracing::warn!(
                apps.size = ans.size,
                "the app cache has no room within its budget; fetched anyway"
            );
        }
        let started = std::time::Instant::now();
        let part = self.cache.partial_path(&hash);
        let src = drive_transfer::Source {
            addr,
            hash: hash.parse().map_err(|_| "a bad hash")?,
            size: ans.size,
        };
        let from = match self.fetch_iroh(src, &part, on_bytes).await {
            Ok(f) => f,
            Err(e) => return Err(format!("{device_name}: {e}")),
        };
        let path = self
            .cache
            .commit(&hash, ans.size, &pinned)
            .map_err(|e| e.to_string())?;
        self.cache.remember(name, named);
        let link = self.fetcher.get().map_or("none", |f| {
            f.path_kind(&self.device_addr(device).map(|a| a.id).unwrap_or(f.id()))
        });
        tracing::info!(
            apps.outcome = "fetched",
            apps.source = "device",
            apps.bytes = ans.size,
            apps.resumed_from = from,
            apps.ms = started.elapsed().as_millis() as u64,
            transfer.path = link,
            "a file of another computer came into the app cache"
        );
        Ok(LocalFile {
            path,
            size: ans.size,
            hash: Some(hash),
            how: How::Fetched,
        })
    }

    fn fetcher(&self) -> Result<Arc<drive_transfer::Fetcher>, String> {
        if let Some(f) = self.fetcher.get() {
            return Ok(f.clone());
        }
        let p = self.a.provider.get().ok_or(
            "this device does not serve files over iroh, so it cannot fetch from other computers",
        )?;
        Ok(self
            .fetcher
            .get_or_init(|| Arc::new(drive_transfer::Fetcher::on(p.endpoint().clone())))
            .clone())
    }
}

fn tree_source(e: &tree::Entry, name: &str) -> Result<Source, String> {
    if e.dir {
        return Err(format!("{name} is a folder"));
    }
    if e.hash.is_empty() {
        return Err(format!("{name}: its bytes have not arrived anywhere yet"));
    }
    Ok(Source::Tree {
        hash: e.hash.clone(),
        size: e.size,
    })
}

/// A file being fetched: appended to, hashed as it grows (the part already
/// there is hashed first, from disk), and accepted only whole and matching.
struct Partial {
    path: PathBuf,
    file: std::fs::File,
    hasher: blake3::Hasher,
    len: u64,
}

impl Partial {
    fn open(path: &Path) -> Result<Partial, String> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut hasher = blake3::Hasher::new();
        let mut len = 0u64;
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                len += n as u64;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        Ok(Partial {
            path: path.to_path_buf(),
            file,
            hasher,
            len,
        })
    }

    fn restart(&mut self) -> Result<(), String> {
        self.file.set_len(0).map_err(|e| e.to_string())?;
        self.hasher = blake3::Hasher::new();
        self.len = 0;
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.file.write_all(bytes).map_err(|e| e.to_string())?;
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Whole and matching, synced to disk; else the partial goes.
    fn finish(mut self, hash: &str, size: Option<u64>) -> Result<u64, String> {
        self.file.flush().map_err(|e| e.to_string())?;
        if size.is_some_and(|s| s != self.len) {
            return Err(format!(
                "stopped at {} of {} bytes",
                self.len,
                size.unwrap_or(0)
            ));
        }
        if self.hasher.finalize().to_hex().as_str() != hash {
            let _ = std::fs::remove_file(&self.path);
            return Err("the bytes that came do not match their hash".into());
        }
        self.file.sync_all().map_err(|e| e.to_string())?;
        Ok(self.len)
    }
}
