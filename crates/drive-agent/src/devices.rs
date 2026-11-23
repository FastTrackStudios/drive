//! Every device, as a read-only folder (`Devices/` at the top of the
//! mount):
//!
//! ```text
//! Devices/<device>/Locations/<location>/…   the folders it indexes
//! Devices/<device>/Disks/<disk>/…           every disk it has, live
//! ```
//!
//! **Locations** are built from the location indexes this device keeps a
//! copy of, so they list every file even while their computer is off.
//! Reading a file of this device's reads it in place; another computer's
//! is brought through the node from that computer, if it is on.
//!
//! **Disks** are the device's disks as browsing sees them
//! (docs/browse.md): its home folder, its system disk and every disk
//! mounted on it. This device's are read here, through the same confined,
//! read-only rules the node's browsing uses; another device's through the
//! node's `/browse` routes, with short timeouts, so an off or slow device
//! answers at once (its folders seen before, or "host is down") and never
//! holds the mount. The mount points of Drive itself are never entered.
//!
//! Extended attributes: `user.drive.state` (`here`, `cached`, `remote`,
//! `offline`) and, in locations, `user.drive.copies` (every location
//! holding the same content).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use drive_files::index::IndexEntry;
use drive_files::space::{self, Location};
use drive_proto::{DirEntry, LIST_MAX, Root};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request,
};
use tokio::runtime::Handle;

use crate::agent::Agent;
use crate::browse::Rules;
use crate::places::inside;
pub use crate::view::View;

const TTL: Duration = Duration::from_secs(5);
/// How long a disk folder's listing, and a device's disks, stand.
const LIST_KEEP: Duration = Duration::from_secs(10);
const ROOTS_KEEP: Duration = Duration::from_secs(30);
/// How long to wait for an online device's answer, and for the node's
/// memory of an offline one.
const QUICK: Duration = Duration::from_secs(4);
const OFF_QUICK: Duration = Duration::from_millis(1500);
/// A file of another device's disk is read this much at a time.
const CHUNK: u64 = 4 << 20;
const CHUNK_WAIT: Duration = Duration::from_secs(30);
const CHUNKS_KEPT: usize = 8;

pub const LOCATIONS: &str = "Locations";
pub const DISKS: &str = "Disks";

#[derive(Clone, Debug)]
enum Node {
    Root,
    Device,
    Locations(String),
    Location(Location),
    Entry(Location, IndexEntry),
    Disks(String),
    Disk(String, Root),
    DiskEntry {
        device: String,
        root: Root,
        rel: String,
        entry: DirEntry,
    },
}

type Disks = Arc<Vec<(String, Root)>>;
type Listed = Arc<Vec<DirEntry>>;

/// A piece of another device's file: (file key, start, bytes).
type Piece = (String, u64, Arc<Vec<u8>>);

pub struct DevicesFs {
    agent: Arc<Agent>,
    view: Arc<RwLock<View>>,
    rt: Handle,
    /// Added to every inode this hands out (its root is `base + 1`), so it
    /// can share one mount's inode space with the Drive tree.
    base: u64,
    paths: Mutex<(HashMap<u64, String>, HashMap<String, u64>)>,
    /// Never entered: the mount points of Drive itself.
    guard: Vec<PathBuf>,
    disks: Mutex<HashMap<String, (Instant, Disks)>>,
    lists: Mutex<HashMap<(String, String, String), (Instant, Listed)>>,
    pieces: Mutex<Vec<Piece>>,
    uid: u32,
    gid: u32,
}

impl DevicesFs {
    pub fn new(
        agent: Arc<Agent>,
        view: Arc<RwLock<View>>,
        rt: Handle,
        base: u64,
        guard: Vec<PathBuf>,
    ) -> DevicesFs {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(1, String::new());
        by_path.insert(String::new(), 1);
        DevicesFs {
            agent,
            view,
            rt,
            base,
            paths: Mutex::new((by_ino, by_path)),
            guard,
            disks: Mutex::new(HashMap::new()),
            lists: Mutex::new(HashMap::new()),
            pieces: Mutex::new(Vec::new()),
            uid,
            gid,
        }
    }

    /// The top folder's inode.
    pub fn root_ino(&self) -> INodeNo {
        INodeNo(self.base + 1)
    }

    /// The top folder's attributes.
    pub fn root_attr(&self) -> FileAttr {
        self.attr(self.root_ino(), &Node::Root)
    }

    fn ino(&self, path: &str) -> INodeNo {
        let mut p = self.paths.lock().expect("paths");
        if let Some(i) = p.1.get(path) {
            return INodeNo(self.base + *i);
        }
        let i = p.0.len() as u64 + 1;
        p.0.insert(i, path.to_string());
        p.1.insert(path.to_string(), i);
        INodeNo(self.base + i)
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        let n = ino.0.checked_sub(self.base).ok_or(Errno::ENOENT)?;
        self.paths
            .lock()
            .expect("paths")
            .0
            .get(&n)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    fn me(&self) -> String {
        self.view.read().expect("view").me.clone()
    }

    fn online(&self, device: &str) -> bool {
        self.view.read().expect("view").online.contains(device)
    }

    fn resolve(&self, path: &str) -> Result<Node, Errno> {
        if path.is_empty() {
            return Ok(Node::Root);
        }
        let mut parts = path.splitn(4, '/');
        let dev = self
            .view
            .read()
            .expect("view")
            .device_by_name(parts.next().unwrap_or_default())
            .ok_or(Errno::ENOENT)?
            .to_string();
        match parts.next() {
            None => Ok(Node::Device),
            Some(LOCATIONS) => {
                let Some(lname) = parts.next() else {
                    return Ok(Node::Locations(dev));
                };
                let v = self.view.read().expect("view");
                let loc = v
                    .locations
                    .iter()
                    .find(|l| l.device == dev && l.name == lname)
                    .cloned()
                    .ok_or(Errno::ENOENT)?;
                let Some(rel) = parts.next() else {
                    return Ok(Node::Location(loc));
                };
                let e = v
                    .trees
                    .get(&loc.id)
                    .and_then(|t| t.get(rel))
                    .cloned()
                    .ok_or(Errno::ENOENT)?;
                Ok(Node::Entry(loc, e))
            }
            Some(DISKS) => {
                let Some(name) = parts.next() else {
                    return Ok(Node::Disks(dev));
                };
                let root = self
                    .disks(&dev)?
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, r)| r.clone())
                    .ok_or(Errno::ENOENT)?;
                let Some(rel) = parts.next() else {
                    return Ok(Node::Disk(dev, root));
                };
                let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
                let entry = self
                    .listing(&dev, &root, dir)?
                    .iter()
                    .find(|e| e.name == name)
                    .cloned()
                    .ok_or(Errno::ENOENT)?;
                Ok(Node::DiskEntry {
                    device: dev,
                    root,
                    rel: rel.to_string(),
                    entry,
                })
            }
            Some(_) => Err(Errno::ENOENT),
        }
    }

    fn children(&self, node: &Node) -> Result<Vec<(String, bool)>, Errno> {
        let tree_list = |loc: &Location, at: &str| {
            let v = self.view.read().expect("view");
            v.trees
                .get(&loc.id)
                .map(|t| {
                    t.list(at)
                        .into_iter()
                        .map(|e| (e.name().to_string(), e.dir))
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(match node {
            Node::Root => {
                let v = self.view.read().expect("view");
                v.devices.iter().map(|(_, n)| (n.clone(), true)).collect()
            }
            Node::Device => vec![(LOCATIONS.into(), true), (DISKS.into(), true)],
            Node::Locations(d) => {
                let v = self.view.read().expect("view");
                v.locations
                    .iter()
                    .filter(|l| &l.device == d)
                    .map(|l| (l.name.clone(), true))
                    .collect()
            }
            Node::Location(l) => tree_list(l, ""),
            Node::Entry(l, e) if e.dir => tree_list(l, &e.path),
            Node::Entry(..) => Vec::new(),
            Node::Disks(d) => self.disks(d)?.iter().map(|(n, _)| (n.clone(), true)).collect(),
            Node::Disk(d, r) => names(&self.listing(d, r, "")?),
            Node::DiskEntry {
                device,
                root,
                rel,
                entry,
            } if entry.dir => names(&self.listing(device, root, rel)?),
            Node::DiskEntry { .. } => Vec::new(),
        })
    }

    fn attr(&self, ino: INodeNo, node: &Node) -> FileAttr {
        let (dir, size, mtime) = match node {
            Node::Entry(_, e) => (e.dir, e.size, e.mtime),
            Node::Location(l) => (true, 0, l.built),
            Node::DiskEntry { entry, .. } => (entry.dir, entry.size, entry.mtime),
            _ => (true, 0, 0),
        };
        let t = UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64);
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if dir { 0o555 } else { 0o444 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    // ── disks ───────────────────────────────────────────────────────────

    /// A device's disks, each with the folder name it is shown by.
    fn disks(&self, device: &str) -> Result<Disks, Errno> {
        if let Some((at, d)) = self.disks.lock().expect("disks").get(device)
            && at.elapsed() < ROOTS_KEEP
        {
            return Ok(d.clone());
        }
        let roots = if device == self.me() {
            self.agent.browse_roots()
        } else {
            let wait = if self.online(device) { QUICK } else { OFF_QUICK };
            #[derive(serde::Deserialize)]
            struct Answer {
                roots: Vec<Root>,
            }
            match self.ask(device, "roots", &[], None, wait) {
                Ok((200, body)) => serde_json::from_slice::<Answer>(&body)
                    .map(|a| a.roots)
                    .unwrap_or_else(|_| self.registry_roots(device)),
                _ => self.registry_roots(device),
            }
        };
        let d = Arc::new(folder_names(roots));
        self.disks
            .lock()
            .expect("disks")
            .insert(device.to_string(), (Instant::now(), d.clone()));
        Ok(d)
    }

    /// What the registry says a device's disks are (when its node cannot).
    fn registry_roots(&self, device: &str) -> Vec<Root> {
        space::volumes(&self.agent.space)
            .into_iter()
            .filter(|v| v.device == device)
            .map(|v| Root {
                mount: v.mount,
                name: v.name,
                fs: v.fs,
                total: v.total.max(0) as u64,
                free: v.free.max(0) as u64,
                removable: v.removable,
                home: false,
            })
            .collect()
    }

    /// The rules this device's disks are read by: the node's own, and
    /// never Drive's mount points.
    fn rules(&self) -> Result<Rules, Errno> {
        let me = self.me();
        let roots: Vec<Root> = self.disks(&me)?.iter().map(|(_, r)| r.clone()).collect();
        let mut rules = Rules::new(&roots, &self.agent.dir);
        rules.deny.extend(self.guard.iter().cloned());
        Ok(rules)
    }

    /// `rel` on this device's disk `root`, on disk, inside the rules.
    fn local_path(&self, root: &Root, rel: &str) -> Result<PathBuf, Errno> {
        // Before anything on disk is looked at: resolving a path inside
        // this very mount would ask the mount itself.
        if guarded(&self.guard, Path::new(&root.mount), rel) {
            return Err(Errno::EACCES);
        }
        self.rules()?
            .resolve(&root.mount, rel)
            .map_err(|why| errno_of_refusal(&why))
    }

    /// A folder of a device's disk, every page of it.
    fn listing(&self, device: &str, root: &Root, rel: &str) -> Result<Listed, Errno> {
        let key = (device.to_string(), root.mount.clone(), rel.to_string());
        if let Some((at, l)) = self.lists.lock().expect("lists").get(&key)
            && at.elapsed() < LIST_KEEP
        {
            return Ok(l.clone());
        }
        let mut all: Vec<DirEntry> = Vec::new();
        if device == self.me() {
            let dir = self.local_path(root, rel)?;
            let rules = self.rules()?;
            loop {
                let page = crate::browse::list(&rules, &dir, all.len() as u32, LIST_MAX, true)
                    .map_err(|why| errno_of_refusal(&why))?;
                let done = page.entries.is_empty() || all.len() + page.entries.len() >= page.total as usize;
                all.extend(page.entries);
                if done {
                    break;
                }
            }
        } else {
            let wait = if self.online(device) { QUICK } else { OFF_QUICK };
            #[derive(serde::Deserialize)]
            struct Page {
                entries: Vec<DirEntry>,
                total: u32,
            }
            loop {
                let (offset, limit) = (all.len().to_string(), LIST_MAX.to_string());
                let q = [
                    ("root", root.mount.as_str()),
                    ("path", rel),
                    ("offset", offset.as_str()),
                    ("limit", limit.as_str()),
                    ("hidden", "1"),
                ];
                let page: Page = match self.ask(device, "list", &q, None, wait)? {
                    (200, body) => serde_json::from_slice(&body).map_err(|_| Errno::EIO)?,
                    (code, _) => return Err(errno_of_status(code)),
                };
                let done = page.entries.is_empty() || all.len() + page.entries.len() >= page.total as usize;
                all.extend(page.entries);
                if done {
                    break;
                }
            }
        }
        // Drive's own mount points are never shown (nor entered).
        let base = Path::new(&root.mount).join(rel);
        all.retain(|e| !self.guard.iter().any(|g| base.join(&e.name) == *g));
        let l = Arc::new(all);
        let mut lists = self.lists.lock().expect("lists");
        lists.retain(|_, (at, _)| at.elapsed() < LIST_KEEP);
        lists.insert(key, (Instant::now(), l.clone()));
        Ok(l)
    }

    /// Up to `size` bytes of a disk's file from `offset`.
    fn read_disk(
        &self,
        device: &str,
        root: &Root,
        rel: &str,
        entry: &DirEntry,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        if device == self.me() {
            let path = self.local_path(root, rel)?;
            let f = File::open(path).map_err(|_| Errno::EIO)?;
            if !f.metadata().is_ok_and(|m| m.is_file()) {
                return Err(Errno::EISDIR);
            }
            let mut buf = vec![0u8; size as usize];
            let n = f.read_at(&mut buf, offset).map_err(|_| Errno::EIO)?;
            buf.truncate(n);
            return Ok(buf);
        }
        if !self.online(device) {
            return Err(Errno::EHOSTDOWN);
        }
        if offset >= entry.size {
            return Ok(Vec::new());
        }
        let key = format!("{device}\n{}\n{rel}\n{}\n{}", root.mount, entry.size, entry.mtime);
        let end = (offset + u64::from(size)).min(entry.size);
        let hit = |p: &Piece| p.0 == key && p.1 <= offset && p.1 + p.2.len() as u64 >= end;
        let piece = match self.pieces.lock().expect("pieces").iter().find(|p| hit(p)) {
            Some(p) => p.clone(),
            None => {
                let start = offset - offset % CHUNK;
                let range = format!("bytes={start}-{}", start + CHUNK - 1);
                let q = [("root", root.mount.as_str()), ("path", rel)];
                let (start, body) = match self.ask(device, "file", &q, Some(range), CHUNK_WAIT)? {
                    (206, b) => (start, b),
                    (200, b) => (0, b),
                    (416, _) => return Ok(Vec::new()),
                    (code, _) => return Err(errno_of_status(code)),
                };
                let p = (key, start, Arc::new(body));
                let mut pieces = self.pieces.lock().expect("pieces");
                if pieces.len() >= CHUNKS_KEPT {
                    pieces.remove(0);
                }
                pieces.push(p.clone());
                p
            }
        };
        let from = (offset - piece.1) as usize;
        let to = ((end - piece.1) as usize).min(piece.2.len());
        Ok(piece.2.get(from..to).unwrap_or_default().to_vec())
    }

    /// Ask the node about another device's disks: `GET /browse/{space}/
    /// {device}/{route}`, waiting at most `wait`.
    fn ask(
        &self,
        device: &str,
        route: &str,
        query: &[(&str, &str)],
        range: Option<String>,
        wait: Duration,
    ) -> Result<(u16, Vec<u8>), Errno> {
        let (node, space) = {
            let c = self.agent.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let mut url = reqwest::Url::parse(&node).map_err(|_| Errno::EIO)?;
        url.path_segments_mut()
            .map_err(|()| Errno::EIO)?
            .pop_if_empty()
            .extend(["browse", space.as_str(), device, route]);
        let span = tracing::info_span!(
            "drive.devices.ask",
            browse.route = route,
            browse.status = tracing::field::Empty,
            browse.bytes = tracing::field::Empty,
            browse.outcome = tracing::field::Empty,
        );
        let _in = span.enter();
        let r = self.rt.block_on(async {
            let mut req = self.agent.http.get(url).query(query).timeout(wait);
            if let Some(r) = range {
                req = req.header(reqwest::header::RANGE, r);
            }
            let res = req.send().await?;
            let status = res.status().as_u16();
            let body = res.bytes().await?;
            Ok::<_, reqwest::Error>((status, body.to_vec()))
        });
        match r {
            Ok((status, body)) => {
                span.record("browse.status", status);
                span.record("browse.bytes", body.len() as u64);
                span.record("browse.outcome", if status < 400 { "ok" } else { "refused" });
                Ok((status, body))
            }
            Err(e) => {
                let timed_out = e.is_timeout();
                span.record("browse.outcome", if timed_out { "timeout" } else { "failed" });
                Err(if !self.online(device) {
                    Errno::EHOSTDOWN
                } else if timed_out {
                    Errno::ETIMEDOUT
                } else {
                    Errno::EIO
                })
            }
        }
    }

    // ── locations ───────────────────────────────────────────────────────

    fn remote_cache(&self, loc: &Location, e: &IndexEntry) -> PathBuf {
        let key = if e.sampled.is_empty() {
            drive_files::hash::of_bytes(format!("{}/{}", loc.id, e.path).as_bytes())
        } else {
            e.sampled.clone()
        };
        self.agent.dir.join("remote").join(key)
    }

    fn state(&self, loc: &Location, e: &IndexEntry) -> &'static str {
        let v = self.view.read().expect("view");
        if loc.device == v.me {
            "here"
        } else if self.remote_cache(loc, e).exists() {
            "cached"
        } else if v.online.contains(&loc.device) {
            "remote"
        } else {
            "offline"
        }
    }

    /// The file's bytes on this disk: in place, or brought from its device.
    fn local_file(&self, loc: &Location, e: &IndexEntry) -> Result<PathBuf, Errno> {
        let me = self.me();
        if loc.device == me && loc.storage.is_empty() {
            return inside(Path::new(&loc.path), &e.path).ok_or(Errno::EACCES);
        }
        let cached = self.remote_cache(loc, e);
        if cached.exists() {
            return Ok(cached);
        }
        if !self.online(&loc.device) && loc.storage.is_empty() {
            return Err(Errno::EHOSTDOWN);
        }
        let (node, space) = {
            let c = self.agent.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let url = format!("{node}/fetch/{space}/{}", loc.id);
        let bytes = self
            .rt
            .block_on(async {
                let r = self
                    .agent
                    .http
                    .get(&url)
                    .query(&[("path", e.path.as_str())])
                    .timeout(Duration::from_secs(300))
                    .send()
                    .await
                    .map_err(|x| x.to_string())?;
                if !r.status().is_success() {
                    return Err(format!("{}: {}", r.status(), r.text().await.unwrap_or_default()));
                }
                r.bytes().await.map_err(|x| x.to_string())
            })
            .map_err(|why| {
                tracing::warn!(fetch.location = %loc.name, fetch.why = %why, "a file could not be brought here");
                Errno::EIO
            })?;
        let _ = std::fs::create_dir_all(cached.parent().expect("a parent"));
        let tmp = cached.with_extension("part");
        std::fs::write(&tmp, &bytes)
            .and_then(|_| std::fs::rename(&tmp, &cached))
            .map_err(|_| Errno::EIO)?;
        Ok(cached)
    }

    fn xattr(&self, node: &Node, name: &str) -> Result<Vec<u8>, Errno> {
        match (node, name) {
            (Node::Entry(loc, e), "user.drive.state") if !e.dir => {
                Ok(self.state(loc, e).as_bytes().to_vec())
            }
            (Node::Entry(_, e), "user.drive.copies") if !e.dir => {
                let v = self.view.read().expect("view");
                let list: Vec<String> = v
                    .copies
                    .get(&(e.sampled.clone(), e.size))
                    .map(|c| {
                        c.iter()
                            .filter_map(|(lid, p)| {
                                let l = v.locations.iter().find(|l| &l.id == lid)?;
                                Some(format!("{}/{}/{p}", v.device_name(&l.device), l.name))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(list.join("\n").into_bytes())
            }
            (Node::DiskEntry { device, entry, .. }, "user.drive.state") if !entry.dir => {
                let state = if *device == self.me() {
                    "here"
                } else if self.online(device) {
                    "remote"
                } else {
                    "offline"
                };
                Ok(state.as_bytes().to_vec())
            }
            _ => Err(Errno::ENODATA),
        }
    }
}

/// The names of a listing's entries, and whether each is a folder.
fn names(l: &[DirEntry]) -> Vec<(String, bool)> {
    l.iter().map(|e| (e.name.clone(), e.dir)).collect()
}

/// A folder name for each disk: its own name, made unique (` (2)`, …) and
/// without slashes.
fn folder_names(roots: Vec<Root>) -> Vec<(String, Root)> {
    let mut out: Vec<(String, Root)> = Vec::new();
    for r in roots {
        let base = match r.name.replace('/', "-") {
            n if n.trim().is_empty() => "Disk".to_string(),
            n => n,
        };
        let mut name = base.clone();
        let mut n = 2;
        while out.iter().any(|(x, _)| *x == name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        out.push((name, r));
    }
    out
}

/// Whether `rel` under `root` is (or is inside) one of the guarded paths,
/// judged by name alone: nothing on disk is looked at.
fn guarded(guard: &[PathBuf], root: &Path, rel: &str) -> bool {
    let full = root.join(rel.trim_matches('/'));
    guard.iter().any(|g| full.starts_with(g))
}

/// The error a browsing refusal (docs/browse.md) is to a program.
fn errno_of_refusal(why: &str) -> Errno {
    if why.contains("no such") || why.contains("No such") {
        Errno::ENOENT
    } else if why.contains("not a folder") || why.contains("Not a directory") {
        Errno::ENOTDIR
    } else {
        Errno::EACCES
    }
}

/// The error a node's answer is to a program.
fn errno_of_status(code: u16) -> Errno {
    match code {
        404 => Errno::ENOENT,
        401 | 403 => Errno::EACCES,
        503 => Errno::EHOSTDOWN,
        504 => Errno::ETIMEDOUT,
        _ => Errno::EIO,
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn reply_xattr(reply: ReplyXattr, size: u32, data: &[u8]) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if (size as usize) < data.len() {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(data);
    }
}

impl Filesystem for DevicesFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let r = (|| {
            let path = join(&self.path(parent)?, name.to_str().ok_or(Errno::ENOENT)?);
            let node = self.resolve(&path)?;
            Ok::<_, Errno>(self.attr(self.ino(&path), &node))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.path(ino).and_then(|p| self.resolve(&p)) {
            Ok(n) => reply.attr(&TTL, &self.attr(ino, &n)),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let r = (|| {
            let path = self.path(ino)?;
            let node = self.resolve(&path)?;
            Ok::<_, Errno>((path.clone(), self.children(&node)?))
        })();
        let (path, children) = match r {
            Ok(x) => x,
            Err(e) => return reply.error(e),
        };
        let mut rows = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];
        for (name, dir) in children {
            let kind = if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            rows.push((self.ino(&join(&path, &name)), kind, name));
        }
        for (i, (ino, kind, name)) in rows.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, _kill: bool, reply: ReplyOpen) {
        if !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY) {
            return reply.error(Errno::EROFS);
        }
        match self.path(ino).and_then(|p| self.resolve(&p)) {
            Ok(Node::Entry(loc, e)) if !e.dir => match self.local_file(&loc, &e) {
                Ok(_) => reply.opened(FileHandle(0), FopenFlags::empty()),
                Err(err) => reply.error(err),
            },
            Ok(Node::DiskEntry { device, entry, .. }) if !entry.dir => {
                if device != self.me() && !self.online(&device) {
                    reply.error(Errno::EHOSTDOWN)
                } else {
                    reply.opened(FileHandle(0), FopenFlags::empty())
                }
            }
            Ok(_) => reply.error(Errno::EISDIR),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let r = (|| match self.resolve(&self.path(ino)?)? {
            Node::Entry(loc, e) if !e.dir => {
                let path = self.local_file(&loc, &e)?;
                let mut buf = vec![0u8; size as usize];
                let n = File::open(path)
                    .and_then(|f| f.read_at(&mut buf, offset))
                    .map_err(|_| Errno::EIO)?;
                buf.truncate(n);
                Ok(buf)
            }
            Node::DiskEntry {
                device,
                root,
                rel,
                entry,
            } if !entry.dir => self.read_disk(&device, &root, &rel, &entry, offset, size),
            _ => Err(Errno::EISDIR),
        })();
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self
            .path(ino)
            .and_then(|p| self.resolve(&p))
            .and_then(|n| self.xattr(&n, &name.to_string_lossy()))
        {
            Ok(v) => reply_xattr(reply, size, &v),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        reply_xattr(reply, size, b"user.drive.state\0user.drive.copies\0");
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(1 << 30, 0, 0, 1 << 20, 0, 4096, 255, 4096);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str, mount: &str) -> Root {
        Root {
            mount: mount.into(),
            name: name.into(),
            fs: String::new(),
            total: 0,
            free: 0,
            removable: false,
            home: false,
        }
    }

    #[test]
    fn disks_get_unique_folder_names() {
        let named = folder_names(vec![
            root("Home", "/home/x"),
            root("sda1", "/a"),
            root("sda1", "/b"),
            root("a/b", "/c"),
            root(" ", "/d"),
        ]);
        let names: Vec<&str> = named.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Home", "sda1", "sda1 (2)", "a-b", "Disk"]);
        assert_eq!(named[2].1.mount, "/b");
    }

    #[test]
    fn the_mount_itself_is_never_entered() {
        let guard = vec![PathBuf::from("/home/x/Drive")];
        assert!(guarded(&guard, Path::new("/home/x"), "Drive"));
        assert!(guarded(&guard, Path::new("/home/x"), "Drive/Devices/a"));
        assert!(guarded(&guard, Path::new("/"), "home/x/Drive"));
        assert!(!guarded(&guard, Path::new("/home/x"), "Drives"));
        assert!(!guarded(&guard, Path::new("/home/x"), "Music"));
    }

    #[test]
    fn refusals_and_answers_become_errors() {
        assert_eq!(errno_of_refusal("no such file or folder"), Errno::ENOENT);
        assert_eq!(errno_of_refusal("a path outside the volume"), Errno::EACCES);
        assert_eq!(errno_of_status(503), Errno::EHOSTDOWN);
        assert_eq!(errno_of_status(404), Errno::ENOENT);
        assert_eq!(errno_of_status(504), Errno::ETIMEDOUT);
    }
}
