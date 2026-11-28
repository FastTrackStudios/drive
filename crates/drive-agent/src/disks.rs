//! This device's mounted file systems and their room, listed without asking
//! any FUSE file system anything.
//!
//! drive#210: `sysinfo::Disks` stats and `statvfs`es every mount point,
//! Drive's own `~/Drive` included. Asked from the agent that serves that
//! mount, the answer needs one of the mount's own threads; when they were all
//! busy waiting on the agent, the mount deadlocked for good. On Linux the
//! mounts are read from `/proc/self/mountinfo` and only real disks are
//! asked for their size: a FUSE mount (any program's, Drive's or another
//! agent's) or a network share is listed with no size and never touched.

use std::path::{Path, PathBuf};

/// One mounted file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// What is mounted: `/dev/nvme0n1p1`, `drive:codywright`, `server:/x`.
    pub name: String,
    pub mount: PathBuf,
    /// The file system type: `ext4`, `btrfs`, `fuse.drive`, `fuseblk`.
    pub fs: String,
    /// Bytes, 0 when not asked (a FUSE mount, a network share).
    pub total: u64,
    pub available: u64,
    pub removable: bool,
}

/// The mounts this device has now.
pub fn list() -> Vec<Disk> {
    imp::list()
}

/// The mount holding `path`: the longest mount point it starts with.
pub fn holding(path: &Path) -> Option<Disk> {
    longest(list(), path)
}

fn longest(disks: Vec<Disk>, path: &Path) -> Option<Disk> {
    disks
        .into_iter()
        .filter(|d| path.starts_with(&d.mount))
        .max_by_key(|d| d.mount.as_os_str().len())
}

/// Whether asking `fs` for its size, or statting its root, is answered by a
/// program (which may be this one) or the network, and may never come back.
pub fn answered_elsewhere(fs: &str) -> bool {
    // `fuseblk` (NTFS, exFAT through FUSE) is a real disk: its driver is
    // not this agent, and its size is worth showing.
    (fs == "fuse" || fs.starts_with("fuse."))
        || matches!(
            fs,
            "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "autofs" | "9p" | "afs" | "ceph"
        )
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Disk, answered_elsewhere};
    use std::path::{Path, PathBuf};

    /// Pseudo file systems and system paths, never disks (as sysinfo had it).
    const PSEUDO: &[&str] = &[
        "rootfs",
        "sysfs",
        "proc",
        "devtmpfs",
        "cgroup",
        "cgroup2",
        "pstore",
        "squashfs",
        "rpc_pipefs",
        "iso9660",
        "devpts",
        "hugetlbfs",
        "mqueue",
        "tmpfs",
        "fusectl",
        "securityfs",
        "debugfs",
        "tracefs",
        "configfs",
        "bpf",
        "binfmt_misc",
        "efivarfs",
        "nsfs",
    ];

    pub fn list() -> Vec<Disk> {
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return Vec::new();
        };
        let removable = usb_devices();
        parse(&info)
            .into_iter()
            .map(|m| {
                let (total, available) = if answered_elsewhere(&m.fs) {
                    (0, 0)
                } else {
                    room(&m.mount).unwrap_or((0, 0))
                };
                Disk {
                    removable: removable.contains(&m.name),
                    name: m.name,
                    mount: m.mount,
                    fs: m.fs,
                    total,
                    available,
                }
            })
            .collect()
    }

    /// A mount as `/proc/self/mountinfo` says it.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Mount {
        pub name: String,
        pub mount: PathBuf,
        pub fs: String,
    }

    /// The disk-like mounts of a mountinfo table: no pseudo file systems,
    /// nothing under /sys, /proc or /run (but /run/media).
    pub fn parse(info: &str) -> Vec<Mount> {
        info.lines()
            .filter_map(|line| {
                // id parent major:minor root mount-point options [optional…] - fs source super
                let (left, right) = line.split_once(" - ")?;
                let mount = unescape(left.split(' ').nth(4)?);
                let mut r = right.split(' ');
                let fs = r.next()?.to_string();
                let name = unescape(r.next().unwrap_or(""));
                Some(Mount {
                    name,
                    mount: PathBuf::from(mount),
                    fs,
                })
            })
            .filter(|m| {
                let p = m.mount.to_string_lossy();
                !PSEUDO.contains(&m.fs.as_str())
                    && !p.starts_with("/sys")
                    && !p.starts_with("/proc")
                    && !(p.starts_with("/run") && !p.starts_with("/run/media"))
                    && !m.name.starts_with("sunrpc")
            })
            .collect()
    }

    /// mountinfo's octal escapes (`\040` a space, `\011` a tab, `\012` a
    /// newline, `\134` a backslash).
    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
            {
                let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
                out.push(v);
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// (total, available) bytes of the file system at `mount`.
    fn room(mount: &Path) -> Option<(u64, u64)> {
        let c = std::ffi::CString::new(mount.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: a valid C string and a zeroed out-parameter.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let unit = s.f_frsize as u64;
        Some((
            (s.f_blocks as u64).saturating_mul(unit),
            (s.f_bavail as u64).saturating_mul(unit),
        ))
    }

    /// The block devices plugged in over USB (`/dev/disk/by-id/usb-*`),
    /// as their `/dev/…` names. Only links are read: no mount is touched.
    fn usb_devices() -> Vec<String> {
        let Ok(dir) = std::fs::read_dir("/dev/disk/by-id") else {
            return Vec::new();
        };
        dir.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("usb-"))
            .filter_map(|e| {
                let to = std::fs::read_link(e.path()).ok()?;
                let to = Path::new("/dev/disk/by-id").join(to);
                // `../../sdb1` → `/dev/sdb1`, without asking the file system.
                let name = to.file_name()?.to_string_lossy().into_owned();
                Some(format!("/dev/{name}"))
            })
            .collect()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Disk;

    // No Drive FUSE mount here (the Macs' agent runs without one), so
    // sysinfo's way of asking every mount is safe.
    pub fn list() -> Vec<Disk> {
        let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
            return Vec::new();
        };
        disks
            .iter()
            .map(|d| Disk {
                name: d.name().to_string_lossy().into_owned(),
                mount: d.mount_point().to_path_buf(),
                fs: d.file_system().to_string_lossy().into_owned(),
                total: d.total_space(),
                available: d.available_space(),
                removable: d.is_removable(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_mount_holds_a_path() {
        let d = |m: &str| Disk {
            name: m.into(),
            mount: m.into(),
            fs: "ext4".into(),
            total: 1,
            available: 1,
            removable: false,
        };
        let disks = vec![d("/"), d("/run/media/Development"), d("/run/media")];
        assert_eq!(
            longest(disks.clone(), Path::new("/run/media/Development/x"))
                .unwrap()
                .mount,
            PathBuf::from("/run/media/Development")
        );
        assert_eq!(
            longest(disks, Path::new("/home/cody")).unwrap().mount,
            PathBuf::from("/")
        );
    }

    #[test]
    fn fuse_and_network_mounts_are_never_asked() {
        for fs in [
            "fuse.drive",
            "fuse.portal",
            "fuse.sshfs",
            "fuse",
            "nfs4",
            "cifs",
        ] {
            assert!(answered_elsewhere(fs), "{fs}");
        }
        for fs in ["ext4", "btrfs", "xfs", "fuseblk", "vfat", "apfs"] {
            assert!(!answered_elsewhere(fs), "{fs}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_is_read_as_the_kernel_writes_it() {
        let info = "\
22 1 259:2 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw
23 22 0:21 / /proc rw - proc proc rw
30 22 0:26 / /run/user/1000 rw - tmpfs tmpfs rw
31 22 259:5 / /run/media/Development rw shared:7 - btrfs /dev/nvme1n1p1 rw
40 22 0:226 / /home/cody/Drive rw,nosuid,nodev shared:9 - fuse.drive drive:codywright rw,user_id=1000
41 22 8:17 / /run/media/cody/My\\040Disk rw - exfat /dev/sdb1 rw
42 22 0:50 / /mnt/nas rw - nfs4 nas:/export rw
";
        let got = imp::parse(info);
        let mounts: Vec<(&str, &str)> = got
            .iter()
            .map(|m| (m.mount.to_str().unwrap(), m.fs.as_str()))
            .collect();
        assert_eq!(
            mounts,
            [
                ("/", "btrfs"),
                ("/run/media/Development", "btrfs"),
                ("/home/cody/Drive", "fuse.drive"),
                ("/run/media/cody/My Disk", "exfat"),
                ("/mnt/nas", "nfs4"),
            ]
        );
        assert_eq!(got[2].name, "drive:codywright");
    }

    /// drive#210: listing disks beside a FUSE mount whose answers never come
    /// (as `~/Drive` is while its own threads wait on the agent) returns at
    /// once, and lists that mount without asking it anything.
    #[cfg(all(target_os = "linux", feature = "mount"))]
    #[test]
    fn listing_disks_never_waits_on_a_fuse_mount() {
        use std::sync::mpsc;
        use std::time::Duration;

        let mnt = tempfile::tempdir().unwrap();
        let (release, held) = mpsc::channel::<()>();
        let fs = Unanswered {
            held: std::sync::Mutex::new(held),
        };
        let mut config = fuser::Config::default();
        config.mount_options = vec![fuser::MountOption::FSName("drive-210-test".into())];
        let session = match fuser::spawn_mount(fs, mnt.path(), &config) {
            Ok(s) => s,
            Err(_) => return, // no FUSE here (a CI container)
        };

        let (done, listed) = mpsc::channel();
        let at = mnt.path().to_path_buf();
        std::thread::spawn(move || {
            let disks = list();
            let here = holding(&at);
            let _ = done.send((disks, here));
        });
        let outcome = listed.recv_timeout(Duration::from_secs(10));
        // Let the mount answer, so a failure leaves nothing hung.
        drop(release);
        drop(session);
        let (disks, here) = outcome.expect("listing the disks waited on a FUSE mount");
        let ours = disks
            .iter()
            .find(|d| d.mount == mnt.path())
            .expect("the FUSE mount is listed");
        assert_eq!((ours.total, ours.available), (0, 0), "and never asked");
        assert_eq!(here.map(|d| d.mount), Some(mnt.path().to_path_buf()));
    }

    /// A FUSE file system that never answers a stat or a statfs (until
    /// `held`'s sender is dropped).
    #[cfg(all(target_os = "linux", feature = "mount"))]
    struct Unanswered {
        held: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    #[cfg(all(target_os = "linux", feature = "mount"))]
    impl fuser::Filesystem for Unanswered {
        fn getattr(
            &self,
            _req: &fuser::Request,
            _ino: fuser::INodeNo,
            _fh: Option<fuser::FileHandle>,
            reply: fuser::ReplyAttr,
        ) {
            let _ = self.held.lock().unwrap().recv();
            reply.error(fuser::Errno::EIO);
        }

        fn statfs(&self, _req: &fuser::Request, _ino: fuser::INodeNo, reply: fuser::ReplyStatfs) {
            let _ = self.held.lock().unwrap().recv();
            reply.error(fuser::Errno::EIO);
        }
    }
}
