//! The one mount: the Drive tree, with every device under `Devices/` at
//! its top (docs/mount.md).
//!
//! Two file systems share one inode space: the Drive tree's ([`DriveFs`])
//! below [`DEVICES_BASE`], every device's ([`DevicesFs`]) from it up, so
//! each call goes to the side its inode is on. Only the top folder is
//! both: its listing is the tree's top with `Devices` added, and looking
//! up `Devices` there opens the devices' side.
//!
//! The devices' side is read-only; a move between the two sides is
//! refused with `EXDEV` (a program then copies, as between two disks).
//!
//! An entry of the Drive tree's top named `Devices` is not lost: it shows
//! as [`ALIAS`] and every call made on that name reaches it.

use std::ffi::OsStr;
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileHandle, FileType, Filesystem, Generation, INodeNo, LockOwner, OpenFlags, Owner,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::devices::DevicesFs;
use crate::fs::DriveFs;

/// The devices' folder at the top of the mount.
pub const DEVICES: &str = "Devices";
/// What a Drive tree entry of the top named `Devices` shows as.
pub const ALIAS: &str = "Devices (in Drive)";
/// The first inode of the devices' side.
pub const DEVICES_BASE: u64 = 1 << 62;

const TTL: Duration = Duration::from_secs(5);

pub struct OneFs {
    drive: DriveFs,
    devices: Option<DevicesFs>,
}

/// Which side an inode is on.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Side {
    Drive,
    Devices,
}

pub fn side(ino: INodeNo) -> Side {
    if ino.0 >= DEVICES_BASE {
        Side::Devices
    } else {
        Side::Drive
    }
}

/// What a name in a folder of the Drive side means.
#[derive(Debug, PartialEq, Eq)]
pub enum Name<'a> {
    /// The devices' folder (only at the top, with the folder on).
    Devices,
    /// This name in the Drive tree.
    Tree(&'a OsStr),
}

/// `name` in folder `parent` of the Drive side: at the top, `Devices` is
/// the devices' folder, and [`ALIAS`] is the tree's own `Devices` when it
/// has one (`tree_has_devices`).
pub fn name_in<'a>(parent: INodeNo, name: &'a OsStr, folder: bool, tree_has_devices: bool) -> Name<'a> {
    if !folder || parent != INodeNo::ROOT {
        return Name::Tree(name);
    }
    if name == DEVICES {
        Name::Devices
    } else if name == ALIAS && tree_has_devices {
        Name::Tree(OsStr::new(DEVICES))
    } else {
        Name::Tree(name)
    }
}

/// The top folder's rows: the tree's (its own `Devices` shown as
/// [`ALIAS`]), with the devices' folder after `.` and `..`.
pub fn top_rows(
    mut rows: Vec<(INodeNo, FileType, String)>,
    devices: INodeNo,
) -> Vec<(INodeNo, FileType, String)> {
    for r in rows.iter_mut().skip(2) {
        if r.2 == DEVICES {
            r.2 = ALIAS.to_string();
        }
    }
    let at = rows.len().min(2);
    rows.insert(at, (devices, FileType::Directory, DEVICES.to_string()));
    rows
}

impl OneFs {
    /// The Drive tree alone (`devices: None`), or with the devices' folder,
    /// whose inodes must start at [`DEVICES_BASE`].
    pub fn new(drive: DriveFs, devices: Option<DevicesFs>) -> OneFs {
        OneFs { drive, devices }
    }

    fn folder(&self) -> bool {
        self.devices.is_some()
    }

    fn devices(&self) -> Result<&DevicesFs, Errno> {
        self.devices.as_ref().ok_or(Errno::ENOENT)
    }

    /// `name` in `parent` (of the Drive side), or the error a change to
    /// the devices' folder itself gets.
    fn tree_name<'a>(&self, parent: INodeNo, name: &'a OsStr, taken: Errno) -> Result<&'a OsStr, Errno> {
        let has = self.folder() && parent == INodeNo::ROOT && self.drive.has_top(DEVICES);
        match name_in(parent, name, self.folder(), has) {
            Name::Devices => Err(taken),
            Name::Tree(n) => Ok(n),
        }
    }
}

impl Filesystem for OneFs {
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if side(parent) == Side::Devices {
            return match self.devices() {
                Ok(d) => d.lookup(req, parent, name, reply),
                Err(e) => reply.error(e),
            };
        }
        let has = self.folder() && parent == INodeNo::ROOT && self.drive.has_top(DEVICES);
        match (name_in(parent, name, self.folder(), has), &self.devices) {
            (Name::Devices, Some(d)) => reply.entry(&TTL, &d.root_attr(), Generation(0)),
            (Name::Devices, None) => reply.error(Errno::ENOENT),
            (Name::Tree(n), _) => self.drive.lookup(req, parent, n, reply),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match side(ino) {
            Side::Drive => self.drive.getattr(req, ino, fh, reply),
            Side::Devices => match self.devices() {
                Ok(d) => d.getattr(req, ino, fh, reply),
                Err(e) => reply.error(e),
            },
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
        if side(ino) == Side::Devices {
            return reply.error(Errno::EROFS);
        }
        self.drive.setattr(
            req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime,
            flags, kill_suid_gid, reply,
        );
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
        if side(parent) == Side::Devices {
            return reply.error(Errno::EROFS);
        }
        match self.tree_name(parent, name, Errno::EEXIST) {
            Ok(n) => self.drive.mkdir(req, parent, n, mode, umask, owner, reply),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if side(parent) == Side::Devices {
            return reply.error(Errno::EROFS);
        }
        match self.tree_name(parent, name, Errno::EISDIR) {
            Ok(n) => self.drive.unlink(req, parent, n, reply),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if side(parent) == Side::Devices {
            return reply.error(Errno::EROFS);
        }
        match self.tree_name(parent, name, Errno::EBUSY) {
            Ok(n) => self.drive.rmdir(req, parent, n, reply),
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
        match (side(parent), side(newparent)) {
            (Side::Drive, Side::Drive) => {}
            (Side::Devices, Side::Devices) => return reply.error(Errno::EROFS),
            _ => return reply.error(Errno::EXDEV),
        }
        let from = self.tree_name(parent, name, Errno::EBUSY);
        let to = self.tree_name(newparent, newname, Errno::EBUSY);
        match (from, to) {
            (Ok(f), Ok(t)) => self
                .drive
                .rename(req, parent, f, newparent, t, flags, owner, reply),
            (Err(e), _) | (_, Err(e)) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, kill: bool, reply: ReplyOpen) {
        match side(ino) {
            Side::Drive => self.drive.open(req, ino, flags, kill, reply),
            Side::Devices => match self.devices() {
                Ok(d) => d.open(req, ino, flags, kill, reply),
                Err(e) => reply.error(e),
            },
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
        match side(ino) {
            Side::Drive => self.drive.read(req, ino, fh, offset, size, flags, lock, reply),
            Side::Devices => match self.devices() {
                Ok(d) => d.read(req, ino, fh, offset, size, flags, lock, reply),
                Err(e) => reply.error(e),
            },
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
        match side(ino) {
            Side::Drive => self
                .drive
                .write(req, ino, fh, offset, data, wf, flags, lock, reply),
            Side::Devices => reply.error(Errno::EROFS),
        }
    }

    fn flush(&self, req: &Request, ino: INodeNo, fh: FileHandle, lock: LockOwner, reply: ReplyEmpty) {
        match side(ino) {
            Side::Drive => self.drive.flush(req, ino, fh, lock, reply),
            Side::Devices => reply.ok(),
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
        match side(ino) {
            Side::Drive => self.drive.release(req, ino, fh, flags, lock, flush, reply),
            Side::Devices => reply.ok(),
        }
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if side(ino) == Side::Devices {
            return match self.devices() {
                Ok(d) => d.readdir(req, ino, fh, offset, reply),
                Err(e) => reply.error(e),
            };
        }
        let (Some(d), true) = (&self.devices, ino == INodeNo::ROOT) else {
            return self.drive.readdir(req, ino, fh, offset, reply);
        };
        let rows = match self.drive.rows(ino) {
            Ok(r) => top_rows(r, d.root_ino()),
            Err(e) => return reply.error(e),
        };
        for (i, (ino, kind, name)) in rows.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
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
        if side(parent) == Side::Devices {
            return reply.error(Errno::EROFS);
        }
        match self.tree_name(parent, name, Errno::EISDIR) {
            Ok(n) => self
                .drive
                .create(req, parent, n, mode, umask, flags, kill, owner, reply),
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match side(ino) {
            Side::Drive => self.drive.getxattr(req, ino, name, size, reply),
            Side::Devices => match self.devices() {
                Ok(d) => d.getxattr(req, ino, name, size, reply),
                Err(e) => reply.error(e),
            },
        }
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match side(ino) {
            Side::Drive => self.drive.listxattr(req, ino, size, reply),
            Side::Devices => match self.devices() {
                Ok(d) => d.listxattr(req, ino, size, reply),
                Err(e) => reply.error(e),
            },
        }
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        self.drive.statfs(req, ino, reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inodes_say_their_side() {
        assert_eq!(side(INodeNo::ROOT), Side::Drive);
        assert_eq!(side(INodeNo(12345)), Side::Drive);
        assert_eq!(side(INodeNo(DEVICES_BASE - 1)), Side::Drive);
        assert_eq!(side(INodeNo(DEVICES_BASE + 1)), Side::Devices);
        assert_eq!(side(INodeNo(u64::MAX)), Side::Devices);
    }

    #[test]
    fn devices_is_the_folder_only_at_the_top() {
        let top = INodeNo::ROOT;
        let n = OsStr::new;
        assert_eq!(name_in(top, n("Devices"), true, false), Name::Devices);
        assert_eq!(name_in(top, n("Devices"), true, true), Name::Devices);
        assert_eq!(name_in(INodeNo(7), n("Devices"), true, true), Name::Tree(n("Devices")));
        assert_eq!(name_in(top, n("Devices"), false, true), Name::Tree(n("Devices")));
        assert_eq!(name_in(top, n("Studio"), true, true), Name::Tree(n("Studio")));
    }

    #[test]
    fn the_trees_own_devices_is_reached_by_its_alias() {
        let top = INodeNo::ROOT;
        let n = OsStr::new;
        assert_eq!(name_in(top, n(ALIAS), true, true), Name::Tree(n("Devices")));
        // Without a tree entry named Devices, the alias is just a name.
        assert_eq!(name_in(top, n(ALIAS), true, false), Name::Tree(n(ALIAS)));
        // Deeper down it is just a name too.
        assert_eq!(name_in(INodeNo(9), n(ALIAS), true, true), Name::Tree(n(ALIAS)));
    }

    #[test]
    fn the_top_lists_devices_and_keeps_the_trees_own() {
        let rows = vec![
            (INodeNo::ROOT, FileType::Directory, ".".to_string()),
            (INodeNo::ROOT, FileType::Directory, "..".to_string()),
            (INodeNo(2), FileType::Directory, "Studio".to_string()),
            (INodeNo(3), FileType::Directory, "Devices".to_string()),
            (INodeNo(4), FileType::RegularFile, "notes.md".to_string()),
        ];
        let out = top_rows(rows, INodeNo(DEVICES_BASE + 1));
        let names: Vec<&str> = out.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(names, [".", "..", "Devices", "Studio", ALIAS, "notes.md"]);
        assert_eq!(out[2].0, INodeNo(DEVICES_BASE + 1));
        assert_eq!(out[4].0, INodeNo(3));
    }
}
