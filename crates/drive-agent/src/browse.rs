//! Browsing this device's disks from elsewhere, live and read-only
//! (docs/browse.md): the node asks ([`drive_proto::Browse`]) and this
//! answers with a folder's entries, a file's bytes, its content key or its
//! sidecars, or makes a folder a location.
//!
//! Two rules hold everywhere in this module:
//!
//! - **Confined.** A request names a root (a volume's mount point, `/`, or
//!   the home folder, as [`roots`] lists them now) and a path inside it.
//!   The path is resolved on disk (`..`, symlinks) and refused unless it
//!   stays inside that root; `/proc`, `/sys`, `/dev` and this agent's own
//!   data folder (its credentials) are never listed or read.
//! - **Read-only.** Files are opened for read access only;
//!   nothing here creates, writes, renames, deletes or changes the
//!   permissions of anything (the `nothing_here_writes` test reads this
//!   file's source to keep it so).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use drive_files::space::Volume;
use drive_proto::{Browse, Browsed, Chunk, DirEntry, LIST_MAX, Listing, READ_MAX, Root};

use crate::agent::Agent;

/// Folders and files hidden unless asked for (besides dot files).
const SYSTEM_NAMES: &[&str] = &[
    "System Volume Information",
    "$RECYCLE.BIN",
    "lost+found",
    "Thumbs.db",
    "desktop.ini",
];

/// Never listed or read, wherever they are reached from.
const NEVER: &[&str] = &["/proc", "/sys", "/dev"];

/// Drive's own mount points on this device: never browsed (a listing of
/// one would ask the mount, which may be asking this).
static NEVER_BROWSED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Never browse these (Drive's own mount points).
pub fn never_browse(paths: &[PathBuf]) {
    NEVER_BROWSED
        .lock()
        .expect("never")
        .extend(paths.iter().cloned());
}

/// What this device lets be browsed now: its volumes (as the registry
/// gets them), `/`, and the home folder; one per mount point.
pub fn roots(volumes: &[Volume], home: Option<&Path>) -> Vec<Root> {
    let mut out: Vec<Root> = Vec::new();
    let mut add = |r: Root| {
        if !out.iter().any(|o| o.mount == r.mount) {
            out.push(r);
        }
    };
    if let Some(h) = home {
        // The disk it is on gives its size.
        let on = volumes
            .iter()
            .filter(|v| h.starts_with(&v.mount))
            .max_by_key(|v| v.mount.len());
        add(Root {
            mount: h.to_string_lossy().into_owned(),
            name: "Home".into(),
            fs: on.map(|v| v.fs.clone()).unwrap_or_default(),
            total: on.map_or(0, |v| v.total.max(0) as u64),
            free: on.map_or(0, |v| v.free.max(0) as u64),
            removable: false,
            home: true,
        });
    }
    let system = volumes.iter().find(|v| v.mount == "/");
    add(Root {
        mount: "/".into(),
        name: "System".into(),
        fs: system.map(|v| v.fs.clone()).unwrap_or_default(),
        total: system.map_or(0, |v| v.total.max(0) as u64),
        free: system.map_or(0, |v| v.free.max(0) as u64),
        removable: false,
        home: false,
    });
    for v in volumes {
        let name = Path::new(&v.mount)
            .file_name()
            .map_or_else(|| v.mount.clone(), |n| n.to_string_lossy().into_owned());
        add(Root {
            mount: v.mount.clone(),
            name,
            fs: v.fs.clone(),
            total: v.total.max(0) as u64,
            free: v.free.max(0) as u64,
            removable: v.removable,
            home: false,
        });
    }
    out
}

/// Where requests may and may not reach.
#[derive(Clone, Debug)]
pub struct Rules {
    /// The roots' mount points, as [`roots`] gives them.
    pub roots: Vec<String>,
    /// Folders never listed or read (canonical): [`NEVER`] and the agent's
    /// own data folder.
    pub deny: Vec<PathBuf>,
}

impl Rules {
    pub fn new(roots: &[Root], agent_dir: &Path) -> Rules {
        let mut deny: Vec<PathBuf> = NEVER.iter().map(PathBuf::from).collect();
        deny.push(std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf()));
        Rules {
            roots: roots.iter().map(|r| r.mount.clone()).collect(),
            deny,
        }
    }

    /// `rel` inside `root`, resolved on disk; `Err` when the root is not one
    /// of this device's, the path leaves it (`..`, an absolute path, a
    /// symlink pointing out), or it is somewhere never shown.
    pub fn resolve(&self, root: &str, rel: &str) -> Result<PathBuf, String> {
        if !self.roots.iter().any(|r| r == root) {
            return Err("not a volume of this device".into());
        }
        let rel = rel.trim_matches('/');
        let relp = Path::new(rel);
        if relp
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err("a path outside the volume".into());
        }
        // Judged by name first: resolving a path inside a denied mount
        // would ask that mount.
        let named = Path::new(root).join(relp);
        if self.deny.iter().any(|d| named.starts_with(d)) {
            return Err("not shown".into());
        }
        let base = std::fs::canonicalize(root).map_err(|e| format!("{root}: {e}"))?;
        let full = std::fs::canonicalize(base.join(relp)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "no such file or folder".to_string(),
            std::io::ErrorKind::PermissionDenied => "not readable here".to_string(),
            _ => e.to_string(),
        })?;
        if !full.starts_with(&base) {
            return Err("a path outside the volume".into());
        }
        if self.deny.iter().any(|d| full.starts_with(d)) {
            return Err("not shown".into());
        }
        Ok(full)
    }
}

/// Whether an entry is hidden by default: a dot file, or a system folder
/// (`.Spotlight-V100`, `.fseventsd`, `.Trashes` are dot files; Windows'
/// and fsck's are named).
pub fn hidden_name(name: &str) -> bool {
    name.starts_with('.') || SYSTEM_NAMES.contains(&name)
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// One page of a folder: folders first, then by name (case folded). Only
/// the page's entries are stat'ed, so a folder of 100 000 files costs one
/// directory read and a page of stats.
pub fn list(
    rules: &Rules,
    dir: &Path,
    offset: u32,
    limit: u32,
    hidden: bool,
) -> Result<Listing, String> {
    let limit = limit.clamp(1, LIST_MAX) as usize;
    let mut names: Vec<(bool, String, PathBuf)> = Vec::new();
    let mut left_out = 0u32;
    for e in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if (!hidden && hidden_name(&name)) || rules.deny.iter().any(|d| path == *d) {
            left_out += 1;
            continue;
        }
        // A link counts as what it points at (a folder or a file).
        let is_dir = match e.file_type() {
            Ok(t) if t.is_symlink() => path.is_dir(),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        names.push((is_dir, name, path));
    }
    names.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            .then_with(|| a.1.cmp(&b.1))
    });
    let total = names.len() as u32;
    let entries = names
        .into_iter()
        .skip(offset as usize)
        .take(limit)
        .map(|(dir, name, path)| {
            let link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            let meta = std::fs::metadata(&path).or_else(|_| std::fs::symlink_metadata(&path));
            let (size, mtime) = match &meta {
                Ok(m) => (if dir { 0 } else { m.len() }, ms(m.modified())),
                Err(_) => (0, 0),
            };
            DirEntry {
                name,
                dir,
                size,
                mtime,
                link,
            }
        })
        .collect();
    Ok(Listing {
        entries,
        total,
        offset,
        hidden: left_out,
    })
}

/// Open for reading without waiting for a writer if a path resolves to a pipe.
pub(crate) fn open_read_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options.open(path)
}

/// Up to `len` bytes of a regular file from `start` (opened for reading
/// only); `len` 0 answers its size and time alone.
pub fn read(path: &Path, start: u64, len: u32) -> Result<Chunk, String> {
    let mut f = open_read_only(path).map_err(|e| e.to_string())?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    let size = meta.len();
    let want = u64::from(len.min(READ_MAX)).min(size.saturating_sub(start));
    let mut bytes = vec![0u8; want as usize];
    if want > 0 {
        f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        f.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    }
    Ok(Chunk {
        size,
        mtime: ms(meta.modified()),
        start,
        bytes,
    })
}

/// A file's content key, as its sidecars are found by.
pub fn key(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err("not a file".into());
    }
    drive_files::hash::sampled(path)
        .map(|h| drive_sidecars::sampled_key(&h))
        .map_err(|e| e.to_string())
}

impl Agent {
    /// The roots this device lets be browsed now.
    pub fn browse_roots(&self) -> Vec<Root> {
        let me = self.device_id();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        roots(&crate::places::volumes(&me), home.as_deref())
    }

    pub(crate) fn browse_rules(&self) -> Rules {
        let mut rules = Rules::new(&self.browse_roots(), &self.dir);
        rules
            .deny
            .extend(NEVER_BROWSED.lock().expect("never").iter().cloned());
        rules
    }

    /// Answer the node's [`Browse`] request.
    pub async fn answer_browse(&self, ask: Browse) -> Result<Browsed, String> {
        let rules = self.browse_rules();
        match ask {
            Browse::Roots => Ok(Browsed::Roots(self.browse_roots())),
            Browse::List {
                root,
                path,
                offset,
                limit,
                hidden,
            } => {
                let dir = rules.resolve(&root, &path)?;
                tokio::task::spawn_blocking(move || list(&rules, &dir, offset, limit, hidden))
                    .await
                    .map_err(|e| e.to_string())?
                    .map(Browsed::Listing)
            }
            Browse::Read {
                root,
                path,
                start,
                len,
            } => {
                let file = rules.resolve(&root, &path)?;
                tokio::task::spawn_blocking(move || read(&file, start, len))
                    .await
                    .map_err(|e| e.to_string())?
                    .map(Browsed::Chunk)
            }
            Browse::Key { root, path } => {
                let file = rules.resolve(&root, &path)?;
                tokio::task::spawn_blocking(move || key(&file))
                    .await
                    .map_err(|e| e.to_string())?
                    .map(Browsed::Key)
            }
            Browse::Sidecars {
                root, path, kinds, ..
            } => {
                let file = rules.resolve(&root, &path)?;
                self.browse_sidecars(file, &kinds).await.map(Browsed::Key)
            }
            Browse::Provide { root, path } => self
                .provide_browsed(&rules, &root, &path)
                .await
                .map(Browsed::Provided),
            Browse::ProvideLocation { location, path } => self
                .provide_location(&location, &path)
                .await
                .map(Browsed::Provided),
            Browse::Index { root, path, name } => {
                let folder = rules.resolve(&root, &path)?;
                if !folder.is_dir() {
                    return Err("not a folder".into());
                }
                let loc = self.add_location(&name, &folder)?;
                Ok(Browsed::Key(loc.id))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, Rules) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("dev-drive");
        std::fs::create_dir_all(root.join("Songs/Mixes")).unwrap();
        std::fs::create_dir_all(root.join(".Spotlight-V100")).unwrap();
        std::fs::create_dir_all(root.join("lost+found")).unwrap();
        std::fs::write(root.join("Songs/a.wav"), vec![7u8; 300_000]).unwrap();
        std::fs::write(root.join("Songs/B.txt"), b"hello").unwrap();
        std::fs::write(root.join(".DS_Store"), b"x").unwrap();
        std::fs::write(t.path().join("secret.txt"), b"outside").unwrap();
        let agent_dir = root.join("agent-data");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("config.json"), b"{\"token\":\"t\"}").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(t.path().join("secret.txt"), root.join("out.txt")).unwrap();
            std::os::unix::fs::symlink(t.path(), root.join("up")).unwrap();
            std::os::unix::fs::symlink(root.join("Songs"), root.join("songs-link")).unwrap();
        }
        let r = roots(
            &[Volume {
                device: "d".into(),
                name: "sdb1".into(),
                mount: root.to_string_lossy().into_owned(),
                fs: "exfat".into(),
                total: 100,
                free: 40,
                removable: true,
                seen: 0,
            }],
            None,
        );
        let rules = Rules::new(&r, &agent_dir);
        (t, rules)
    }

    fn root_of(t: &tempfile::TempDir) -> String {
        t.path().join("dev-drive").to_string_lossy().into_owned()
    }

    #[test]
    fn roots_are_volumes_slash_and_home() {
        let r = roots(
            &[Volume {
                device: "d".into(),
                name: "disk3".into(),
                mount: "/Volumes/dev-drive".into(),
                fs: "apfs".into(),
                total: 10,
                free: 5,
                removable: true,
                seen: 0,
            }],
            Some(Path::new("/Users/me")),
        );
        let names: Vec<(&str, &str)> = r
            .iter()
            .map(|r| (r.mount.as_str(), r.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("/Users/me", "Home"),
                ("/", "System"),
                ("/Volumes/dev-drive", "dev-drive")
            ]
        );
        assert!(r[2].removable && r[0].home);
    }

    #[test]
    fn paths_stay_inside_their_volume() {
        let (t, rules) = tree();
        let root = root_of(&t);
        assert!(rules.resolve(&root, "").is_ok());
        assert!(rules.resolve(&root, "Songs/a.wav").is_ok());
        assert!(rules.resolve(&root, "/Songs/./a.wav").is_ok());
        // Not a root of this device.
        assert!(rules.resolve("/etc", "passwd").is_err());
        assert!(rules.resolve(&format!("{root}/Songs"), "a.wav").is_err());
        // `..`, however it is spelled.
        assert!(rules.resolve(&root, "../secret.txt").is_err());
        assert!(rules.resolve(&root, "Songs/../../secret.txt").is_err());
        assert!(rules.resolve(&root, "Songs/..").is_err());
        // Symlinks pointing out; one pointing inside is fine.
        #[cfg(unix)]
        {
            assert!(rules.resolve(&root, "out.txt").is_err());
            assert!(rules.resolve(&root, "up/secret.txt").is_err());
            assert!(rules.resolve(&root, "songs-link/a.wav").is_ok());
        }
        // The agent's own data (its credentials).
        assert_eq!(
            rules.resolve(&root, "agent-data/config.json").unwrap_err(),
            "not shown"
        );
        assert!(rules.resolve(&root, "missing").is_err());
    }

    #[test]
    fn listing_pages_and_hides() {
        let (t, rules) = tree();
        let root = root_of(&t);
        let dir = rules.resolve(&root, "").unwrap();
        let l = list(&rules, &dir, 0, 100, false).unwrap();
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        // Folders first (a link to one counts), hidden and system ones and
        // the agent's folder left out.
        assert_eq!(names, vec!["Songs", "songs-link", "up", "out.txt"]);
        assert_eq!(l.hidden, 4);
        let all = list(&rules, &dir, 0, 100, true).unwrap();
        assert!(all.entries.iter().any(|e| e.name == ".Spotlight-V100"));
        assert!(!all.entries.iter().any(|e| e.name == "agent-data"));

        // A big folder, a page at a time.
        let big = t.path().join("dev-drive/Many");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..2500 {
            std::fs::write(big.join(format!("f{i:05}.txt")), b"").unwrap();
        }
        let dir = rules.resolve(&root, "Many").unwrap();
        let p1 = list(&rules, &dir, 0, 100_000, false).unwrap();
        assert_eq!(p1.total, 2500);
        assert_eq!(p1.entries.len(), LIST_MAX as usize);
        let p2 = list(&rules, &dir, LIST_MAX, LIST_MAX, false).unwrap();
        assert_eq!(p2.entries.len(), 500);
        assert_eq!(p2.entries[0].name, "f02000.txt");
        assert_eq!(p2.offset, LIST_MAX);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reading_a_pipe_does_not_wait_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // The C string is valid for the duration of mkfifo.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (done, result) = mpsc::channel();
        let read_path = path.clone();
        let worker = std::thread::spawn(move || {
            done.send(read(&read_path, 0, 1)).unwrap();
        });
        let outcome = result.recv_timeout(Duration::from_secs(1));
        if outcome.is_err() {
            // Unblock the old implementation so a failure leaves no stuck worker.
            let _pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            worker.join().unwrap();
            panic!("read waited for a pipe writer");
        }
        worker.join().unwrap();
        assert_eq!(outcome.unwrap().err().as_deref(), Some("not a file"));
    }

    #[test]
    fn reads_ranges_of_files_only() {
        let (t, rules) = tree();
        let root = root_of(&t);
        let f = rules.resolve(&root, "Songs/B.txt").unwrap();
        let c = read(&f, 1, 3).unwrap();
        assert_eq!((c.size, c.bytes.as_slice()), (5, b"ell".as_slice()));
        assert_eq!(read(&f, 0, 0).unwrap().bytes.len(), 0);
        assert_eq!(read(&f, 4, 100).unwrap().bytes, b"o");
        assert!(read(&f, 9, 100).unwrap().bytes.is_empty());
        let big = rules.resolve(&root, "Songs/a.wav").unwrap();
        assert_eq!(read(&big, 0, u32::MAX).unwrap().bytes.len(), 300_000);
        let d = rules.resolve(&root, "Songs").unwrap();
        assert!(read(&d, 0, 10).is_err());
        assert!(key(&d).is_err());
        assert!(key(&big).unwrap().starts_with('s'));
    }

    /// Every browse operation on a tree made read-only, then the tree
    /// compared with before: nothing was created, changed or removed.
    #[cfg(unix)]
    #[test]
    fn browsing_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let (t, rules) = tree();
        let root = root_of(&t);
        let base = PathBuf::from(&root);
        let snapshot = |p: &Path| {
            let mut out = Vec::new();
            let mut stack = vec![p.to_path_buf()];
            while let Some(d) = stack.pop() {
                let mut es: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
                es.sort_by_key(|e| e.path());
                for e in es {
                    let m = std::fs::symlink_metadata(e.path()).unwrap();
                    let bytes = if m.is_file() {
                        std::fs::read(e.path()).unwrap()
                    } else {
                        Vec::new()
                    };
                    out.push((e.path(), m.len(), m.modified().unwrap(), bytes));
                    if m.is_dir() {
                        stack.push(e.path());
                    }
                }
            }
            out
        };
        let set_mode = |file: u32, dir: u32| {
            let mut stack = vec![base.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).unwrap().flatten() {
                    let m = std::fs::symlink_metadata(e.path()).unwrap();
                    if m.is_dir() {
                        stack.push(e.path());
                    } else if m.is_file() {
                        std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(file))
                            .unwrap();
                    }
                }
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(dir)).unwrap();
            }
        };
        let before = snapshot(&base);
        set_mode(0o444, 0o555);
        let dir = rules.resolve(&root, "Songs").unwrap();
        list(&rules, &dir, 0, 10, true).unwrap();
        let f = rules.resolve(&root, "Songs/a.wav").unwrap();
        read(&f, 0, 1000).unwrap();
        key(&f).unwrap();
        set_mode(0o644, 0o755);
        assert_eq!(snapshot(&base), before);
    }

    /// The module's own source holds no call that writes, renames, removes
    /// or opens a file for writing (outside these tests).
    #[test]
    fn nothing_here_writes() {
        let src = include_str!("browse.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for bad in [
            "fs::write",
            "File::create",
            "remove_file",
            "remove_dir",
            "fs::rename",
            "set_permissions",
            "create_dir",
            "hard_link",
            "fs::copy",
            ".write(true)",
            ".append(true)",
            ".create(true)",
            ".create_new(true)",
            ".truncate(true)",
            "set_len",
            "write_all",
        ] {
            assert!(!code.contains(bad), "browse.rs must not use {bad}");
        }
    }
}
