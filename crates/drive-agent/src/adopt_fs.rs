//! Adopted folders in the mount (docs/adopt.md): a layer over the Drive
//! tree's file system that shows each adopted folder's own files at its
//! logical path.
//!
//! Two sides in one inode space, as `Devices/` does it: the inner file
//! system's inodes below [`ADOPTED_BASE`], the adopted side's from it up
//! (and below `1 << 62`, where the devices' side starts). A folder of the
//! inner side whose logical path has adoptions in or under it lists them
//! too; looking one up opens the adopted side.
//!
//! On the adopted side, a folder of this device is its folder on disk,
//! read and written in place (zero-copy). What would delete or replace a
//! file there (unlink, rmdir, a rename onto an existing name) moves it
//! into the location's `.drive-trash` instead, as file rules do
//! ([`crate::rules_fs::to_trash`]: a rename, never a copy or a delete).
//! That trash is hidden in the mount; the app restores from it. Another
//! device's adopted folder is listed from its index and read through the
//! node, read-only. A move between the sides is refused with `EXDEV`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drive_files::adopt::{self, join};
use drive_files::tree;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, LockOwner, OpenAccMode, OpenFlags, Owner, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use tokio::runtime::Handle;

use crate::adopt::Spot;
use crate::agent::Agent;

/// The first inode of the adopted side.
pub const ADOPTED_BASE: u64 = 1 << 61;

const TTL: Duration = Duration::from_secs(1);

/// What the adopted side needs of the file system it covers.
pub trait TreeSide {
    /// The logical path of one of its folders (`""`: the top); `None` for
    /// files, unknown inodes and folders that are not the Drive tree's.
    fn folder_path(&self, ino: INodeNo) -> Option<String>;
    /// A folder's rows (`.`, `..`, then its entries): (inode, kind, name).
    fn rows(&self, ino: INodeNo) -> Result<Rows, Errno>;
}

/// A folder's rows: (inode, kind, name).
pub type Rows = Vec<(INodeNo, FileType, String)>;

/// What a path of the adopted side is.
enum Node {
    /// A folder on the way to adopted folders, not in the Drive tree.
    Above,
    /// In an adopted folder here: its real path and what it is now.
    Here(PathBuf, std::fs::Metadata),
    /// In another device's adopted folder (read-only, from its index).
    Elsewhere(drive_files::space::Location, drive_files::index::IndexEntry),
}

#[derive(Default)]
struct Paths {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next: u64,
}

pub struct AdoptFs<F> {
    inner: F,
    agent: Arc<Agent>,
    rt: Handle,
    paths: Mutex<Paths>,
    open: Mutex<HashMap<u64, File>>,
    next_fh: AtomicU64,
}

fn errno(e: std::io::Error) -> Errno {
    e.raw_os_error().map_or(Errno::EIO, Errno::from_i32)
}

fn name_of(name: &OsStr) -> Result<&str, Errno> {
    name.to_str()
        .filter(|n| !n.contains('/') && *n != "." && *n != "..")
        .ok_or(Errno::EINVAL)
}

impl<F: Filesystem + TreeSide> AdoptFs<F> {
    pub fn new(inner: F, agent: Arc<Agent>, rt: Handle) -> AdoptFs<F> {
        AdoptFs {
            inner,
            agent,
            rt,
            paths: Mutex::new(Paths {
                next: ADOPTED_BASE,
                ..Paths::default()
            }),
            open: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn mine(ino: INodeNo) -> bool {
        ino.0 >= ADOPTED_BASE && ino.0 < (1 << 62)
    }

    fn ino(&self, path: &str) -> INodeNo {
        let mut p = self.paths.lock().expect("paths");
        if let Some(i) = p.by_path.get(path) {
            return INodeNo(*i);
        }
        let i = p.next;
        p.next += 1;
        p.by_ino.insert(i, path.to_string());
        p.by_path.insert(path.to_string(), i);
        INodeNo(i)
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        self.paths
            .lock()
            .expect("paths")
            .by_ino
            .get(&ino.0)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    /// After a rename on disk: every known path under `from` is under `to`.
    fn moved(&self, from: &str, to: &str) {
        let mut p = self.paths.lock().expect("paths");
        let hit: Vec<(String, u64)> = p
            .by_path
            .iter()
            .filter(|(k, _)| *k == from || k.starts_with(&format!("{from}/")))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (old, i) in hit {
            let new = format!("{to}{}", &old[from.len()..]);
            p.by_path.remove(&old);
            if let Some(prev) = p.by_path.insert(new.clone(), i) {
                p.by_ino.remove(&prev);
            }
            p.by_ino.insert(i, new);
        }
    }

    fn resolve(&self, path: &str) -> Result<Node, Errno> {
        match self.agent.spot(path) {
            None => Err(Errno::ENOENT),
            Some(Spot::Above) => Ok(Node::Above),
            Some(Spot::Here { path: real, .. }) => {
                let m = std::fs::metadata(&real).map_err(errno)?;
                Ok(Node::Here(real, m))
            }
            Some(Spot::Elsewhere { location, path, .. }) => {
                if path.is_empty() {
                    let root = drive_files::index::IndexEntry {
                        path: String::new(),
                        dir: true,
                        size: 0,
                        mtime: location.built,
                        sampled: String::new(),
                    };
                    return Ok(Node::Elsewhere(location, root));
                }
                let e = self
                    .rt
                    .block_on(self.agent.index_entry(&location.id, &path))
                    .ok_or(Errno::ENOENT)?;
                Ok(Node::Elsewhere(location, e))
            }
        }
    }

    fn attr(ino: INodeNo, n: &Node) -> FileAttr {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let at = |ms: i64| UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
        let (dir, size, t, perm) = match n {
            Node::Above => (true, 0, SystemTime::now(), 0o755),
            Node::Elsewhere(_, e) => (
                e.dir,
                e.size,
                at(e.mtime),
                if e.dir { 0o555 } else { 0o444 },
            ),
            Node::Here(_, m) => {
                let t = |s: i64, ns: i64| {
                    UNIX_EPOCH + Duration::new(s.max(0) as u64, ns.clamp(0, 999_999_999) as u32)
                };
                return FileAttr {
                    ino,
                    size: m.len(),
                    blocks: m.blocks(),
                    atime: t(m.atime(), m.atime_nsec()),
                    mtime: t(m.mtime(), m.mtime_nsec()),
                    ctime: t(m.ctime(), m.ctime_nsec()),
                    crtime: m.created().unwrap_or(t(m.mtime(), m.mtime_nsec())),
                    kind: if m.is_dir() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    perm: (m.mode() & 0o7777) as u16,
                    nlink: m.nlink() as u32,
                    uid: m.uid(),
                    gid: m.gid(),
                    rdev: 0,
                    blksize: m.blksize() as u32,
                    flags: 0,
                };
            }
        };
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
            perm,
            nlink: if dir { 2 } else { 1 },
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The adopted-side path of `name` in `parent`, when it is one: a
    /// folder of the adopted side, or a name adoptions add to a folder of
    /// the inner side (an adopted folder always; a folder on the way only
    /// when the Drive tree has none of that name).
    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<Option<String>, Errno> {
        if Self::mine(parent) {
            let name = name_of(name)?;
            // The location's trash is hidden here (restored from the app).
            if name == crate::rules_fs::TRASH_DIR {
                return Err(Errno::ENOENT);
            }
            return Ok(Some(join(&self.path(parent)?, name)));
        }
        let Some(dir) = self.inner.folder_path(parent) else {
            return Ok(None);
        };
        let Some(name) = name.to_str() else {
            return Ok(None);
        };
        let added = adopt::names_in(&self.agent.in_place(), &dir);
        Ok(added
            .into_iter()
            .find(|(n, _)| n == name)
            .and_then(|(n, point)| {
                let path = join(&dir, &n);
                let in_tree =
                    matches!(tree::resolve(&self.agent.files, &path), Some(Some(e)) if e.dir);
                (point || !in_tree).then_some(path)
            }))
    }

    /// A folder of this device's adopted folder, to change things in.
    fn here_dir(&self, parent: INodeNo) -> Result<PathBuf, Errno> {
        if !Self::mine(parent) {
            return Err(Errno::EXDEV);
        }
        match self.resolve(&self.path(parent)?)? {
            Node::Here(p, m) if m.is_dir() => Ok(p),
            Node::Here(..) => Err(Errno::ENOTDIR),
            _ => Err(Errno::EROFS),
        }
    }

    /// `name` in `parent` (of this device's adopted folder) into the
    /// location's trash.
    fn trash(&self, parent: INodeNo, name: &str) -> Result<(), Errno> {
        let path = join(&self.path(parent)?, name);
        let Some(Spot::Here { root, rel, .. }) = self.agent.spot(&path) else {
            return Err(Errno::EROFS);
        };
        crate::rules_fs::to_trash(&root, &rel).map(|_| ()).map_err(|why| {
            tracing::warn!(adopt.trash = %why, "a file of an adopted folder was not removed: it could not be moved into the trash");
            Errno::EPERM
        })
    }

    /// Changing an adopted folder itself, or a folder on the way to one,
    /// from the inner side: refused.
    fn guard_inner(&self, parent: INodeNo, name: &OsStr) -> Result<(), Errno> {
        match self.child(parent, name)? {
            Some(_) => Err(Errno::EPERM),
            None => Ok(()),
        }
    }

    fn rows_mine(&self, ino: INodeNo) -> Result<Rows, Errno> {
        let path = self.path(ino)?;
        let list = self
            .rt
            .block_on(self.agent.logical_list(&path))
            .map_err(|_| Errno::ENOENT)?;
        let mut rows = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];
        for (name, dir) in list {
            let kind = if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            rows.push((self.ino(&join(&path, &name)), kind, name));
        }
        Ok(rows)
    }

    /// An inner folder's rows with what adoptions add (an adopted folder
    /// in place of a tree entry of the same name); `None`: nothing added.
    fn rows_inner(&self, ino: INodeNo) -> Option<Result<Rows, Errno>> {
        let dir = self.inner.folder_path(ino)?;
        let added = adopt::names_in(&self.agent.in_place(), &dir);
        if added.is_empty() {
            return None;
        }
        Some(self.inner.rows(ino).map(|mut rows| {
            rows.retain(|(_, _, n)| !added.iter().any(|(a, point)| *point && a == n));
            for (n, _) in added {
                if !rows.iter().any(|(_, _, r)| *r == n) {
                    rows.push((self.ino(&join(&dir, &n)), FileType::Directory, n));
                }
            }
            rows
        }))
    }

    fn xattr(&self, ino: INodeNo, name: &str) -> Result<Vec<u8>, Errno> {
        let path = self.path(ino)?;
        let spot = self.agent.spot(&path).ok_or(Errno::ENOENT)?;
        match (name, spot) {
            ("user.drive.state", Spot::Above) => Ok(b"folder".to_vec()),
            ("user.drive.state", Spot::Here { .. }) => Ok(b"here".to_vec()),
            ("user.drive.state", Spot::Elsewhere { .. }) => Ok(b"remote".to_vec()),
            ("user.drive.adopted", Spot::Here { adoption, .. })
            | ("user.drive.adopted", Spot::Elsewhere { adoption, .. }) => {
                Ok(adoption.at.into_bytes())
            }
            // Where the bytes physically are.
            ("user.drive.where", Spot::Here { path, .. }) => {
                Ok(path.to_string_lossy().into_owned().into_bytes())
            }
            ("user.drive.where", Spot::Elsewhere { location, path, .. }) => {
                let device = drive_files::space::devices(&self.agent.space)
                    .into_iter()
                    .find(|d| d.id == location.device)
                    .map_or(location.device.clone(), |d| d.name);
                Ok(format!("{device}:{}", join(&location.path, &path)).into_bytes())
            }
            _ => Err(Errno::ENODATA),
        }
    }
}

impl TreeSide for crate::fs::DriveFs {
    fn folder_path(&self, ino: INodeNo) -> Option<String> {
        crate::fs::DriveFs::folder_path(self, ino)
    }

    fn rows(&self, ino: INodeNo) -> Result<Rows, Errno> {
        crate::fs::DriveFs::rows(self, ino)
    }
}

/// The one mount (the tree with `Devices/`): the tree's folders only.
impl TreeSide for crate::one::OneFs {
    fn folder_path(&self, ino: INodeNo) -> Option<String> {
        if crate::one::side(ino) == crate::one::Side::Devices {
            return None;
        }
        self.drive().folder_path(ino)
    }

    fn rows(&self, ino: INodeNo) -> Result<Rows, Errno> {
        if crate::one::side(ino) == crate::one::Side::Devices {
            return Err(Errno::ENOENT);
        }
        let rows = self.drive().rows(ino)?;
        Ok(match self.devices_root() {
            Some(d) if ino == INodeNo::ROOT => crate::one::top_rows(rows, d),
            _ => rows,
        })
    }
}

fn reply_rows(rows: Rows, offset: u64, mut reply: ReplyDirectory) {
    for (i, (ino, kind, name)) in rows.into_iter().enumerate().skip(offset as usize) {
        if reply.add(ino, (i + 1) as u64, kind, &name) {
            break;
        }
    }
    reply.ok();
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

fn system_time(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

impl<F: Filesystem + TreeSide> Filesystem for AdoptFs<F> {
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        self.inner.init(req, config)
    }

    fn destroy(&mut self) {
        self.inner.destroy();
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.child(parent, name) {
            Ok(Some(path)) => match self.resolve(&path) {
                Ok(n) => reply.entry(&TTL, &Self::attr(self.ino(&path), &n), Generation(0)),
                Err(e) => reply.error(e),
            },
            Ok(None) => self.inner.lookup(req, parent, name, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        if !Self::mine(ino) {
            return self.inner.getattr(req, ino, fh, reply);
        }
        match self.path(ino).and_then(|p| self.resolve(&p)) {
            Ok(n) => reply.attr(&TTL, &Self::attr(ino, &n)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<fuser::BsdFileFlags>,
        kill_suid_gid: bool,
        reply: ReplyAttr,
    ) {
        if !Self::mine(ino) {
            return self.inner.setattr(
                req,
                ino,
                mode,
                uid,
                gid,
                size,
                atime,
                mtime,
                ctime,
                fh,
                crtime,
                chgtime,
                bkuptime,
                flags,
                kill_suid_gid,
                reply,
            );
        }
        let r = (|| {
            let Node::Here(real, _) = self.resolve(&self.path(ino)?)? else {
                return Err(Errno::EROFS);
            };
            if let Some(m) = mode {
                std::fs::set_permissions(&real, std::fs::Permissions::from_mode(m & 0o7777))
                    .map_err(errno)?;
            }
            if let Some(size) = size {
                let open = self.open.lock().expect("open");
                match fh.and_then(|f| open.get(&f.0)) {
                    Some(f) => f.set_len(size).map_err(errno)?,
                    None => OpenOptions::new()
                        .write(true)
                        .open(&real)
                        .and_then(|f| f.set_len(size))
                        .map_err(errno)?,
                }
            }
            if atime.is_some() || mtime.is_some() {
                let mut times = std::fs::FileTimes::new();
                if let Some(t) = atime {
                    times = times.set_accessed(system_time(t));
                }
                if let Some(t) = mtime {
                    times = times.set_modified(system_time(t));
                }
                File::open(&real)
                    .and_then(|f| f.set_times(times))
                    .map_err(errno)?;
            }
            let m = std::fs::metadata(&real).map_err(errno)?;
            Ok(Self::attr(ino, &Node::Here(real, m)))
        })();
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        owner: Owner,
        reply: ReplyEntry,
    ) {
        if !Self::mine(parent) {
            return match self.guard_inner(parent, name) {
                Ok(()) => self
                    .inner
                    .mkdir(req, parent, name, mode, umask, owner, reply),
                Err(_) => reply.error(Errno::EEXIST),
            };
        }
        let r = (|| {
            let n = name_of(name)?;
            let real = self.here_dir(parent)?.join(n);
            std::fs::DirBuilder::new()
                .mode(mode & !umask & 0o7777)
                .create(&real)
                .map_err(errno)?;
            let path = join(&self.path(parent)?, n);
            let m = std::fs::metadata(&real).map_err(errno)?;
            Ok(Self::attr(self.ino(&path), &Node::Here(real, m)))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if !Self::mine(parent) {
            return match self.guard_inner(parent, name) {
                Ok(()) => self.inner.unlink(req, parent, name, reply),
                Err(e) => reply.error(e),
            };
        }
        let r = (|| {
            let real = self.here_dir(parent)?.join(name_of(name)?);
            let m = std::fs::symlink_metadata(&real).map_err(errno)?;
            if m.is_dir() {
                return Err(Errno::EISDIR);
            }
            self.trash(parent, name_of(name)?)
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if !Self::mine(parent) {
            return match self.guard_inner(parent, name) {
                Ok(()) => self.inner.rmdir(req, parent, name, reply),
                Err(e) => reply.error(e),
            };
        }
        let r = (|| {
            let real = self.here_dir(parent)?.join(name_of(name)?);
            if std::fs::read_dir(&real).map_err(errno)?.next().is_some() {
                return Err(Errno::ENOTEMPTY);
            }
            self.trash(parent, name_of(name)?)
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        owner: Option<Owner>,
        reply: ReplyEmpty,
    ) {
        match (Self::mine(parent), Self::mine(newparent)) {
            (false, false) => {
                if self.guard_inner(parent, name).is_err()
                    || self.guard_inner(newparent, newname).is_err()
                {
                    return reply.error(Errno::EPERM);
                }
                return self
                    .inner
                    .rename(req, parent, name, newparent, newname, flags, owner, reply);
            }
            (true, true) => {}
            _ => return reply.error(Errno::EXDEV),
        }
        let r = (|| {
            if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                return Err(Errno::EINVAL);
            }
            let (n, nn) = (name_of(name)?, name_of(newname)?);
            let from = self.here_dir(parent)?.join(n);
            let to = self.here_dir(newparent)?.join(nn);
            if from == to {
                return Ok(());
            }
            let fm = std::fs::symlink_metadata(&from).map_err(errno)?;
            if let Ok(tm) = std::fs::symlink_metadata(&to) {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    return Err(Errno::EEXIST);
                }
                match (fm.is_dir(), tm.is_dir()) {
                    (false, true) => return Err(Errno::EISDIR),
                    (true, false) => return Err(Errno::ENOTDIR),
                    (true, true) if std::fs::read_dir(&to).map_err(errno)?.next().is_some() => {
                        return Err(Errno::ENOTEMPTY);
                    }
                    _ => {}
                }
                // What is replaced goes to the trash, never lost.
                self.trash(newparent, nn)?;
            }
            std::fs::rename(&from, &to).map_err(errno)?;
            let (pp, np) = (self.path(parent)?, self.path(newparent)?);
            self.moved(&join(&pp, n), &join(&np, nn));
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, kill: bool, reply: ReplyOpen) {
        if !Self::mine(ino) {
            return self.inner.open(req, ino, flags, kill, reply);
        }
        let r = (|| {
            let write = !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY);
            let f = match self.resolve(&self.path(ino)?)? {
                Node::Here(real, m) => {
                    if m.is_dir() {
                        return Err(Errno::EISDIR);
                    }
                    OpenOptions::new()
                        .read(!matches!(flags.acc_mode(), OpenAccMode::O_WRONLY))
                        .write(write)
                        .append(flags.0 & libc::O_APPEND != 0)
                        .truncate(write && flags.0 & libc::O_TRUNC != 0)
                        .open(real)
                        .map_err(errno)?
                }
                Node::Elsewhere(loc, e) => {
                    if write {
                        return Err(Errno::EROFS);
                    }
                    if e.dir {
                        return Err(Errno::EISDIR);
                    }
                    let p = self
                        .rt
                        .block_on(self.agent.bring_location_file(&loc, &e))
                        .map_err(|why| {
                            tracing::warn!(fetch.location = %loc.name, fetch.why = %why, "a file of an adopted folder could not be brought here");
                            Errno::EIO
                        })?;
                    File::open(p).map_err(errno)?
                }
                Node::Above => return Err(Errno::EISDIR),
            };
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
            self.open.lock().expect("open").insert(fh, f);
            Ok(fh)
        })();
        match r {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if !Self::mine(ino) {
            return self
                .inner
                .read(req, ino, fh, offset, size, flags, lock, reply);
        }
        let open = self.open.lock().expect("open");
        let Some(f) = open.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        let mut buf = vec![0u8; size as usize];
        match f.read_at(&mut buf, offset) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        wf: WriteFlags,
        flags: OpenFlags,
        lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if !Self::mine(ino) {
            return self
                .inner
                .write(req, ino, fh, offset, data, wf, flags, lock, reply);
        }
        let open = self.open.lock().expect("open");
        let Some(f) = open.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        let r = if flags.0 & libc::O_APPEND != 0 {
            use std::io::Write;
            (&*f).write(data)
        } else {
            f.write_at(data, offset)
        };
        match r {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock: LockOwner,
        reply: ReplyEmpty,
    ) {
        if !Self::mine(ino) {
            return self.inner.flush(req, ino, fh, lock, reply);
        }
        reply.ok();
    }

    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        if !Self::mine(ino) {
            return self.inner.fsync(req, ino, fh, datasync, reply);
        }
        let open = self.open.lock().expect("open");
        match open.get(&fh.0).map(|f| {
            if datasync {
                f.sync_data()
            } else {
                f.sync_all()
            }
        }) {
            Some(Ok(())) | None => reply.ok(),
            Some(Err(e)) => reply.error(errno(e)),
        }
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        lock: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        if !Self::mine(ino) {
            return self.inner.release(req, ino, fh, flags, lock, flush, reply);
        }
        self.open.lock().expect("open").remove(&fh.0);
        reply.ok();
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        if Self::mine(ino) {
            return match self.rows_mine(ino) {
                Ok(rows) => reply_rows(rows, offset, reply),
                Err(e) => reply.error(e),
            };
        }
        match self.rows_inner(ino) {
            Some(Ok(rows)) => reply_rows(rows, offset, reply),
            Some(Err(e)) => reply.error(e),
            None => self.inner.readdir(req, ino, fh, offset, reply),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        kill: bool,
        owner: Owner,
        reply: ReplyCreate,
    ) {
        if !Self::mine(parent) {
            return match self.guard_inner(parent, name) {
                Ok(()) => self
                    .inner
                    .create(req, parent, name, mode, umask, flags, kill, owner, reply),
                Err(_) => reply.error(Errno::EEXIST),
            };
        }
        let r = (|| {
            let n = name_of(name)?;
            let real = self.here_dir(parent)?.join(n);
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .create_new(flags & libc::O_EXCL != 0)
                .truncate(flags & libc::O_TRUNC != 0)
                .mode(mode & !umask & 0o7777)
                .open(&real)
                .map_err(errno)?;
            let m = f.metadata().map_err(errno)?;
            let path = join(&self.path(parent)?, n);
            let attr = Self::attr(self.ino(&path), &Node::Here(real, m));
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
            self.open.lock().expect("open").insert(fh, f);
            Ok((attr, fh))
        })();
        match r {
            Ok((a, fh)) => {
                reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if !Self::mine(ino) {
            return self.inner.getxattr(req, ino, name, size, reply);
        }
        match self.xattr(ino, &name.to_string_lossy()) {
            Ok(v) => reply_xattr(reply, size, &v),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        if !Self::mine(ino) {
            return self.inner.listxattr(req, ino, size, reply);
        }
        reply_xattr(
            reply,
            size,
            b"user.drive.state\0user.drive.adopted\0user.drive.where\0",
        );
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let real = match self.path(ino).ok().map(|p| self.resolve(&p)) {
            Some(Ok(Node::Here(p, _))) if Self::mine(ino) => p,
            _ => return self.inner.statfs(req, ino, reply),
        };
        // The disk's own numbers: programs that check for room before
        // recording see the real free space.
        let Ok(c) = std::ffi::CString::new(real.as_os_str().as_encoded_bytes()) else {
            return self.inner.statfs(req, ino, reply);
        };
        // SAFETY: a valid C string and a zeroed out-parameter.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return self.inner.statfs(req, ino, reply);
        }
        reply.statfs(
            s.f_blocks as u64,
            s.f_bfree as u64,
            s.f_bavail as u64,
            s.f_files as u64,
            s.f_ffree as u64,
            s.f_bsize as u32,
            s.f_namemax as u32,
            s.f_frsize as u32,
        );
    }
}

#[cfg(test)]
mod tests;
