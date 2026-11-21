//! Every computer's files, as a read-only folder (`Computers/<device>/
//! <location>/…`): built from the location indexes this device keeps a
//! copy of, so it lists every file even while its computer is off. Reading
//! a file of this device's reads it in place; another computer's is
//! brought through the node from that computer, if it is on.
//!
//! Extended attributes: `user.drive.state` (`here`, `cached`, `remote`,
//! `offline`) and `user.drive.copies` (every location holding the same
//! content).

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH};

use drive_files::index::{IndexEntry, LocationIndex, Tree};
use drive_files::space::Location;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenAccMode,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use tokio::runtime::Handle;

use crate::agent::Agent;
use crate::places::inside;

const TTL: Duration = Duration::from_secs(5);

/// What the folder shows: refreshed by the agent's loop.
#[derive(Default)]
pub struct View {
    pub me: String,
    /// (device id, its name), in order.
    pub devices: Vec<(String, String)>,
    pub locations: Vec<Location>,
    pub trees: HashMap<String, Tree>,
    pub online: HashSet<String>,
    /// Content (sampled hash, size) → where it is: (location id, path).
    pub copies: HashMap<(String, u64), Vec<(String, String)>>,
}

impl View {
    pub fn build(me: &str, devices: Vec<(String, String)>, locations: Vec<Location>, indexes: HashMap<String, LocationIndex>, online: HashSet<String>) -> View {
        let mut copies: HashMap<(String, u64), Vec<(String, String)>> = HashMap::new();
        for idx in indexes.values() {
            for e in idx.entries.iter().filter(|e| !e.dir && !e.sampled.is_empty()) {
                copies.entry((e.sampled.clone(), e.size)).or_default().push((idx.location.clone(), e.path.clone()));
            }
        }
        let trees = indexes.into_iter().map(|(k, v)| (k, Tree::new(v))).collect();
        // Only devices that have a location.
        let devices = devices.into_iter().filter(|(id, _)| locations.iter().any(|l| &l.device == id)).collect();
        View { me: me.to_string(), devices, locations, trees, online, copies }
    }

    fn device_by_name(&self, name: &str) -> Option<&str> {
        self.devices.iter().find(|(_, n)| n == name).map(|(id, _)| id.as_str())
    }

    fn device_name(&self, id: &str) -> String {
        self.devices.iter().find(|(d, _)| d == id).map_or(id.to_string(), |(_, n)| n.clone())
    }
}

#[derive(Clone, Debug)]
enum Node {
    Root,
    Device(String),
    Location(Location),
    Entry(Location, IndexEntry),
}

pub struct ComputersFs {
    agent: Arc<Agent>,
    view: Arc<RwLock<View>>,
    rt: Handle,
    paths: Mutex<(HashMap<u64, String>, HashMap<String, u64>)>,
    uid: u32,
    gid: u32,
}

impl ComputersFs {
    pub fn new(agent: Arc<Agent>, view: Arc<RwLock<View>>, rt: Handle) -> ComputersFs {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(1, String::new());
        by_path.insert(String::new(), 1);
        ComputersFs { agent, view, rt, paths: Mutex::new((by_ino, by_path)), uid, gid }
    }

    fn ino(&self, path: &str) -> INodeNo {
        let mut p = self.paths.lock().expect("paths");
        if let Some(i) = p.1.get(path) {
            return INodeNo(*i);
        }
        let i = p.0.len() as u64 + 1;
        p.0.insert(i, path.to_string());
        p.1.insert(path.to_string(), i);
        INodeNo(i)
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        self.paths.lock().expect("paths").0.get(&ino.0).cloned().ok_or(Errno::ENOENT)
    }

    fn resolve(&self, path: &str) -> Result<Node, Errno> {
        if path.is_empty() {
            return Ok(Node::Root);
        }
        let v = self.view.read().expect("view");
        let mut parts = path.splitn(3, '/');
        let dev = v.device_by_name(parts.next().unwrap_or_default()).ok_or(Errno::ENOENT)?.to_string();
        let Some(lname) = parts.next() else { return Ok(Node::Device(dev)) };
        let loc = v.locations.iter().find(|l| l.device == dev && l.name == lname).cloned().ok_or(Errno::ENOENT)?;
        let Some(rel) = parts.next() else { return Ok(Node::Location(loc)) };
        let e = v.trees.get(&loc.id).and_then(|t| t.get(rel)).cloned().ok_or(Errno::ENOENT)?;
        Ok(Node::Entry(loc, e))
    }

    fn children(&self, path: &str, node: &Node) -> Vec<(String, bool)> {
        let v = self.view.read().expect("view");
        match node {
            Node::Root => v.devices.iter().map(|(_, n)| (n.clone(), true)).collect(),
            Node::Device(d) => v.locations.iter().filter(|l| &l.device == d).map(|l| (l.name.clone(), true)).collect(),
            Node::Location(l) => v
                .trees
                .get(&l.id)
                .map(|t| t.list("").into_iter().map(|e| (e.name().to_string(), e.dir)).collect())
                .unwrap_or_default(),
            Node::Entry(l, e) if e.dir => v
                .trees
                .get(&l.id)
                .map(|t| t.list(&e.path).into_iter().map(|c| (c.name().to_string(), c.dir)).collect())
                .unwrap_or_default(),
            Node::Entry(..) => {
                let _ = path;
                Vec::new()
            }
        }
    }

    fn attr(&self, ino: INodeNo, node: &Node) -> FileAttr {
        let (dir, size, mtime) = match node {
            Node::Entry(_, e) => (e.dir, e.size, e.mtime),
            Node::Location(l) => (true, 0, l.built),
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
            kind: if dir { FileType::Directory } else { FileType::RegularFile },
            perm: if dir { 0o555 } else { 0o444 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

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
        let me = self.view.read().expect("view").me.clone();
        if loc.device == me && loc.storage.is_empty() {
            return inside(std::path::Path::new(&loc.path), &e.path).ok_or(Errno::EACCES);
        }
        let cached = self.remote_cache(loc, e);
        if cached.exists() {
            return Ok(cached);
        }
        let (node, space) = {
            let c = self.agent.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let url = format!("{node}/fetch/{space}/{}", loc.id);
        let bytes = self
            .rt
            .block_on(async {
                let r = self.agent.http.get(&url).query(&[("path", e.path.as_str())]).send().await.map_err(|x| x.to_string())?;
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
        std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &cached)).map_err(|_| Errno::EIO)?;
        Ok(cached)
    }

    fn xattr(&self, node: &Node, name: &str) -> Result<Vec<u8>, Errno> {
        let Node::Entry(loc, e) = node else { return Err(Errno::ENODATA) };
        match name {
            "user.drive.state" if !e.dir => Ok(self.state(loc, e).as_bytes().to_vec()),
            "user.drive.copies" if !e.dir => {
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
            _ => Err(Errno::ENODATA),
        }
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
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

impl Filesystem for ComputersFs {
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

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let (path, node) = match self.path(ino).and_then(|p| self.resolve(&p).map(|n| (p, n))) {
            Ok(x) => x,
            Err(e) => return reply.error(e),
        };
        let mut rows = vec![(ino, FileType::Directory, ".".to_string()), (ino, FileType::Directory, "..".to_string())];
        for (name, dir) in self.children(&path, &node) {
            let kind = if dir { FileType::Directory } else { FileType::RegularFile };
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
        let r = (|| {
            let Node::Entry(loc, e) = self.resolve(&self.path(ino)?)? else { return Err(Errno::EISDIR) };
            let path = self.local_file(&loc, &e)?;
            let mut buf = vec![0u8; size as usize];
            let n = File::open(path).and_then(|f| f.read_at(&mut buf, offset)).map_err(|_| Errno::EIO)?;
            buf.truncate(n);
            Ok(buf)
        })();
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.path(ino).and_then(|p| self.resolve(&p)).and_then(|n| self.xattr(&n, &name.to_string_lossy())) {
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

