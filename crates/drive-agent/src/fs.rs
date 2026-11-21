//! The space's files as a file system (FUSE, Linux): every file is listed
//! whether or not its bytes are here; reading fetches them; writing keeps
//! the new bytes in a scratch file and uploads them on close. Each file's
//! sync state is its `user.drive.state` extended attribute, and who holds
//! its bytes is `user.drive.holders`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drive_files::tree::{self, Entry};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenAccMode, OpenFlags, Owner, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};
use loro::TreeID;
use tokio::runtime::Handle;

use crate::agent::Agent;

/// Attributes are re-read often: others change the tree.
const TTL: Duration = Duration::from_secs(1);

const STATE: &str = "user.drive.state";
const HOLDERS: &str = "user.drive.holders";

/// An open file: a scratch copy when written to.
struct Open {
    id: TreeID,
    scratch: Option<(PathBuf, File)>,
    dirty: bool,
}

pub struct DriveFs {
    agent: Arc<Agent>,
    rt: Handle,
    inodes: Mutex<Inodes>,
    open: Mutex<HashMap<u64, Open>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, TreeID>,
    by_id: HashMap<TreeID, u64>,
    next: u64,
}

impl Inodes {
    fn ino(&mut self, id: TreeID) -> u64 {
        if let Some(i) = self.by_id.get(&id) {
            return *i;
        }
        self.next = self.next.max(1) + 1;
        let i = self.next;
        self.by_ino.insert(i, id);
        self.by_id.insert(id, i);
        i
    }
}

impl DriveFs {
    pub fn new(agent: Arc<Agent>, rt: Handle) -> DriveFs {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        DriveFs {
            agent,
            rt,
            inodes: Mutex::new(Inodes::default()),
            open: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
        }
    }

    /// The tree id behind an inode; `Ok(None)` is the top folder.
    fn id_of(&self, ino: INodeNo) -> Result<Option<TreeID>, Errno> {
        if ino == INodeNo::ROOT {
            return Ok(None);
        }
        self.inodes
            .lock()
            .expect("inodes")
            .by_ino
            .get(&ino.0)
            .copied()
            .map(Some)
            .ok_or(Errno::ENOENT)
    }

    fn ino_of(&self, id: TreeID) -> INodeNo {
        INodeNo(self.inodes.lock().expect("inodes").ino(id))
    }

    fn time(ms: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
    }

    fn attr(&self, ino: INodeNo, e: Option<&Entry>) -> FileAttr {
        let (dir, size, mtime) = match e {
            None => (true, 0, drive_files::now_ms()),
            Some(e) => {
                // A file being written shows its scratch size.
                let scratch = self
                    .open
                    .lock()
                    .expect("open")
                    .values()
                    .find(|o| o.id == e.id && o.scratch.is_some())
                    .and_then(|o| {
                        o.scratch
                            .as_ref()
                            .and_then(|(_, f)| f.metadata().ok())
                            .map(|m| m.len())
                    });
                (e.dir, scratch.unwrap_or(e.size), e.mtime)
            }
        };
        let t = Self::time(mtime);
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
            perm: if dir { 0o755 } else { 0o644 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn entry_of(&self, ino: INodeNo) -> Result<Option<Entry>, Errno> {
        match self.id_of(ino)? {
            None => Ok(None),
            Some(id) => tree::entry(&self.agent.files, id)
                .map(Some)
                .ok_or(Errno::ENOENT),
        }
    }

    fn name(name: &OsStr) -> Result<&str, Errno> {
        name.to_str()
            .filter(|n| !n.contains('/'))
            .ok_or(Errno::EINVAL)
    }

    /// A scratch copy of a file, holding its current bytes, for writing.
    fn scratch_for(&self, e: &Entry, truncate: bool) -> Result<(PathBuf, File), Errno> {
        let dir = self.agent.dir.join("scratch");
        std::fs::create_dir_all(&dir).map_err(|_| Errno::EIO)?;
        let path = dir.join(format!(
            "{}-{}",
            tree::id_string(e.id).replace('@', "_"),
            self.next_fh.load(Ordering::Relaxed)
        ));
        if !truncate && !e.hash.is_empty() {
            let src = self
                .rt
                .block_on(self.agent.fetch(&e.hash))
                .map_err(|_| Errno::EIO)?;
            std::fs::copy(src, &path).map_err(|_| Errno::EIO)?;
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate)
            .open(&path)
            .map_err(|_| Errno::EIO)?;
        Ok((path, f))
    }

    /// Upload a written file and record its new content.
    fn commit_scratch(&self, o: &Open) -> Result<(), Errno> {
        let Some((path, f)) = &o.scratch else {
            return Ok(());
        };
        let _ = f.sync_all();
        let (hash, size) = self
            .rt
            .block_on(self.agent.upload(o.id, path))
            .map_err(|e| {
                tracing::warn!(error = %e, "an upload failed");
                Errno::EIO
            })?;
        tree::set_content(&self.agent.files, o.id, size, &hash).map_err(|_| Errno::EIO)?;
        self.agent.commit_files();
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    fn xattr_value(&self, ino: INodeNo, name: &str) -> Result<Vec<u8>, Errno> {
        let e = self.entry_of(ino)?;
        match (name, e) {
            (STATE, None) => Ok(b"folder".to_vec()),
            (STATE, Some(e)) => Ok(self.agent.state(&e).as_str().as_bytes().to_vec()),
            (HOLDERS, Some(e)) if !e.hash.is_empty() => {
                Ok(self.agent.holders(&e.hash).join(", ").into_bytes())
            }
            (HOLDERS, _) => Ok(Vec::new()),
            _ => Err(Errno::ENODATA),
        }
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

impl Filesystem for DriveFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let r = (|| {
            let parent = self.id_of(parent)?;
            let name = Self::name(name)?;
            let e = tree::lookup(&self.agent.files, parent, name).ok_or(Errno::ENOENT)?;
            let ino = self.ino_of(e.id);
            Ok::<_, Errno>(self.attr(ino, Some(&e)))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.entry_of(ino) {
            Ok(e) => reply.attr(&TTL, &self.attr(ino, e.as_ref())),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        _kill_suid_gid: bool,
        reply: ReplyAttr,
    ) {
        let r = (|| {
            let e = self.entry_of(ino)?;
            if let (Some(size), Some(e)) = (size, e.as_ref()) {
                let mut open = self.open.lock().expect("open");
                let handle = fh
                    .and_then(|f| open.get_mut(&f.0))
                    .filter(|o| o.scratch.is_some());
                match handle {
                    Some(o) => {
                        if let Some((_, f)) = &o.scratch {
                            f.set_len(size).map_err(|_| Errno::EIO)?;
                        }
                        o.dirty = true;
                    }
                    None if size == 0 => {
                        drop(open);
                        let (hash, _) = self
                            .rt
                            .block_on(async {
                                let tmp = self.agent.dir.join("scratch-empty");
                                let _ = std::fs::write(&tmp, b"");
                                self.agent.upload(e.id, &tmp).await
                            })
                            .map_err(|_| Errno::EIO)?;
                        tree::set_content(&self.agent.files, e.id, 0, &hash)
                            .map_err(|_| Errno::EIO)?;
                        self.agent.commit_files();
                    }
                    None => return Err(Errno::ENOTSUP),
                }
            }
            let e = self.entry_of(ino)?;
            Ok::<_, Errno>(self.attr(ino, e.as_ref()))
        })();
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _owner: Owner,
        reply: ReplyEntry,
    ) {
        let r = (|| {
            let parent = self.id_of(parent)?;
            let name = Self::name(name)?;
            if tree::lookup(&self.agent.files, parent, name).is_some() {
                return Err(Errno::EEXIST);
            }
            let id = tree::mkdir(&self.agent.files, parent, name).map_err(|_| Errno::EIO)?;
            self.agent.commit_files();
            let e = tree::entry(&self.agent.files, id).ok_or(Errno::EIO)?;
            Ok(self.attr(self.ino_of(id), Some(&e)))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let parent = self.id_of(parent)?;
            let e =
                tree::lookup(&self.agent.files, parent, Self::name(name)?).ok_or(Errno::ENOENT)?;
            if e.dir {
                return Err(Errno::EISDIR);
            }
            tree::remove(&self.agent.files, e.id).map_err(|_| Errno::EIO)?;
            self.agent.commit_files();
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let parent = self.id_of(parent)?;
            let e =
                tree::lookup(&self.agent.files, parent, Self::name(name)?).ok_or(Errno::ENOENT)?;
            if !tree::children(&self.agent.files, Some(e.id)).is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
            tree::remove(&self.agent.files, e.id).map_err(|_| Errno::EIO)?;
            self.agent.commit_files();
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        _owner: Option<Owner>,
        reply: ReplyEmpty,
    ) {
        let r = (|| {
            let from = self.id_of(parent)?;
            let to = self.id_of(newparent)?;
            let e =
                tree::lookup(&self.agent.files, from, Self::name(name)?).ok_or(Errno::ENOENT)?;
            let newname = Self::name(newname)?;
            // Replacing an existing file (editors save this way).
            if let Some(old) = tree::lookup(&self.agent.files, to, newname)
                && old.id != e.id
            {
                tree::remove(&self.agent.files, old.id).map_err(|_| Errno::EIO)?;
            }
            tree::rename(&self.agent.files, e.id, to, newname).map_err(|_| Errno::EIO)?;
            self.agent.commit_files();
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, _kill: bool, reply: ReplyOpen) {
        let r = (|| {
            let e = self.entry_of(ino)?.ok_or(Errno::EISDIR)?;
            let write = !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY);
            let truncate = flags.0 & libc::O_TRUNC != 0;
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
            let scratch = if write {
                Some(self.scratch_for(&e, truncate)?)
            } else {
                // Reading fetches the bytes now (a placeholder becomes local).
                if !e.hash.is_empty() {
                    self.rt.block_on(self.agent.fetch(&e.hash)).map_err(|err| {
                        tracing::warn!(error = %err, "a download failed");
                        Errno::EIO
                    })?;
                }
                None
            };
            self.open.lock().expect("open").insert(
                fh,
                Open {
                    id: e.id,
                    scratch,
                    dirty: write && truncate,
                },
            );
            Ok(fh)
        })();
        match r {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let r = (|| {
            let mut buf = vec![0u8; size as usize];
            let n = {
                let open = self.open.lock().expect("open");
                match open.get(&fh.0).and_then(|o| o.scratch.as_ref()) {
                    Some((_, f)) => f.read_at(&mut buf, offset).map_err(|_| Errno::EIO)?,
                    None => {
                        drop(open);
                        let e = self.entry_of(ino)?.ok_or(Errno::EISDIR)?;
                        if e.hash.is_empty() {
                            0
                        } else {
                            let path = self
                                .rt
                                .block_on(self.agent.fetch(&e.hash))
                                .map_err(|_| Errno::EIO)?;
                            File::open(path)
                                .and_then(|f| f.read_at(&mut buf, offset))
                                .map_err(|_| Errno::EIO)?
                        }
                    }
                }
            };
            buf.truncate(n);
            Ok::<_, Errno>(buf)
        })();
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _wf: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut open = self.open.lock().expect("open");
        match open.get_mut(&fh.0) {
            Some(o) => match &o.scratch {
                Some((_, f)) => match f.write_at(data, offset) {
                    Ok(n) => {
                        o.dirty = true;
                        reply.written(n as u32);
                    }
                    Err(_) => reply.error(Errno::EIO),
                },
                None => reply.error(Errno::EBADF),
            },
            None => reply.error(Errno::EBADF),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let o = self.open.lock().expect("open").remove(&fh.0);
        match o {
            Some(o) if o.dirty => match self.commit_scratch(&o) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            },
            Some(o) => {
                if let Some((path, _)) = o.scratch {
                    let _ = std::fs::remove_file(path);
                }
                reply.ok();
            }
            None => reply.ok(),
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
        let id = match self.id_of(ino) {
            Ok(id) => id,
            Err(e) => return reply.error(e),
        };
        let parent_ino = match id.and_then(|i| tree::parent_of(&self.agent.files, i)) {
            Some(p) => self.ino_of(p),
            None => INodeNo::ROOT,
        };
        let mut rows: Vec<(INodeNo, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        for e in tree::children(&self.agent.files, id) {
            let kind = if e.dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            rows.push((self.ino_of(e.id), kind, e.name));
        }
        for (i, (ino, kind, name)) in rows.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        _kill: bool,
        _owner: Owner,
        reply: ReplyCreate,
    ) {
        let r = (|| {
            let parent = self.id_of(parent)?;
            let name = Self::name(name)?;
            let id = match tree::lookup(&self.agent.files, parent, name) {
                Some(e) => e.id,
                None => {
                    let id = tree::create_file(&self.agent.files, parent, name)
                        .map_err(|_| Errno::EIO)?;
                    self.agent.commit_files();
                    id
                }
            };
            let e = tree::entry(&self.agent.files, id).ok_or(Errno::EIO)?;
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
            let scratch = self.scratch_for(&e, true)?;
            self.open.lock().expect("open").insert(
                fh,
                Open {
                    id,
                    scratch: Some(scratch),
                    dirty: true,
                },
            );
            Ok((self.attr(self.ino_of(id), Some(&e)), fh))
        })();
        match r {
            Ok((a, fh)) => {
                reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.xattr_value(ino, &name.to_string_lossy()) {
            Ok(v) => reply_xattr(reply, size, &v),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = format!("{STATE}\0{HOLDERS}\0");
        reply_xattr(reply, size, names.as_bytes());
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Generous numbers: the space is as big as its storage.
        reply.statfs(1 << 30, 1 << 29, 1 << 29, 1 << 20, 1 << 19, 4096, 255, 4096);
    }
}
