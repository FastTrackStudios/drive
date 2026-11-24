//! The only file-system changes a rule makes on a computer, written so
//! that nothing is ever lost:
//!
//! - **Copies never overwrite.** A copy is written to a hidden temporary
//!   name beside its destination, checked byte for byte (its BLAKE3 against
//!   the original's), and only then given its name, by a hard link that
//!   fails if the name is taken (then " (2)", " (3)", … are tried).
//! - **Nothing is unlinked.** A "removed" original is renamed into the
//!   location's `.drive-trash` folder (a hidden folder the indexer skips),
//!   at its own relative path, under a free name. The only files ever
//!   unlinked are this module's own temporary names: the extra link once
//!   a copy has its real name, and a copy that failed its check.
//! - **Undo checks first.** A copy is taken back (into `.drive-trash`) only
//!   while it is still what was copied.

use std::io;
use std::path::{Path, PathBuf};

use drive_rules::names::free_name;

/// The folder at a location's root where a rule's "removed" files go.
pub const TRASH_DIR: &str = ".drive-trash";

/// The BLAKE3 of a file, or of a folder (every file's relative path and
/// hash, in order), so a bundle is checked as a whole.
pub fn full_hash(p: &Path) -> io::Result<String> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.is_file() {
        return drive_files::hash::of_file(p);
    }
    if !meta.is_dir() {
        return Err(io::Error::other("not a file or folder"));
    }
    let mut h = blake3::Hasher::new();
    let mut stack = vec![PathBuf::new()];
    let mut files = Vec::new();
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(p.join(&rel))? {
            let e = e?;
            let r = rel.join(e.file_name());
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push(r);
            } else if t.is_file() {
                files.push(r);
            }
        }
    }
    files.sort();
    for r in files {
        h.update(r.to_string_lossy().as_bytes());
        h.update(&[0]);
        h.update(drive_files::hash::of_file(&p.join(&r))?.as_bytes());
        h.update(&[0]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// `root/rel` for a relative path that came from elsewhere (the synced
/// journal): refused unless it is plain names only (no `..`, `.`, root
/// or drive prefix) and nothing along it under `root` is a symlink, so it
/// can never lead outside `root`.
pub fn under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = root.to_path_buf();
    for c in Path::new(rel).components() {
        let std::path::Component::Normal(name) = c else {
            return Err(format!("{rel} is not a path inside the folder"));
        };
        p.push(name);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{rel} goes through a symlink; it is left alone"));
        }
    }
    // `components()` drops a lone `.` in the middle; refuse it too.
    if rel.split('/').any(|s| s == "." || s == "..") {
        return Err(format!("{rel} is not a path inside the folder"));
    }
    Ok(p)
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn copy_into(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            let t = e.file_type()?;
            if t.is_dir() || t.is_file() {
                copy_into(&e.path(), &dst.join(e.file_name()))?;
            }
        }
        return Ok(());
    }
    // create_new: never over anything.
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let mut inp = std::fs::File::open(src)?;
    io::copy(&mut inp, &mut out)?;
    out.sync_all()?;
    if let Ok(m) = meta.modified() {
        let _ = out.set_modified(m);
    }
    Ok(())
}

fn temp_beside(dir: &Path, name: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    dir.join(format!(".{name}.drive-copy-{}-{n:x}", std::process::id()))
}

fn remove_own_temp(p: &Path) {
    // Only ever called on a name `temp_beside` made.
    if std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(p);
    } else {
        let _ = std::fs::remove_file(p);
    }
}

/// Give a checked temporary its name in `dir`: a free one, never over
/// anything. Returns the final path.
fn place(tmp: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    let is_dir = std::fs::symlink_metadata(tmp)?.is_dir();
    loop {
        let free = free_name(name, |n| exists(&dir.join(n)));
        let fin = dir.join(&free);
        if is_dir {
            // A folder: renamed only while the name is free (rename over a
            // non-empty folder fails; over nothing it is the placement).
            match rename_noreplace(tmp, &fin) {
                Ok(()) => return Ok(fin),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        match std::fs::hard_link(tmp, &fin) {
            Ok(()) => {
                remove_own_temp(tmp);
                return Ok(fin);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // A file system without hard links: rename while free.
                match rename_noreplace(tmp, &fin) {
                    Ok(()) => return Ok(fin),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// Rename `from` to `to` only if nothing is at `to`: `AlreadyExists`
/// otherwise. A plain rename replaces an empty folder (or, for a file, any
/// file) that appeared at `to` after it was checked (drive#63). Where the
/// system cannot refuse atomically, it checks first.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
        };
        let (a, b) = (c(from)?, c(to)?);
        // SAFETY: both are NUL-terminated paths that outlive the call.
        #[cfg(target_os = "linux")]
        let r = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        // SAFETY: as above.
        #[cfg(target_os = "macos")]
        let r = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // A file system (or kernel) that cannot: check, then rename.
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ENOTSUP) => {}
            _ => return Err(e),
        }
    }
    if exists(to) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// Copy `src` (a file or a bundle folder) into `dir` as `name` (or a free
/// name beside it), verified byte for byte. Returns the copy's path and
/// the BLAKE3 both share.
pub fn copy_verified(src: &Path, dir: &Path, name: &str) -> Result<(PathBuf, String), String> {
    let want = full_hash(src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = temp_beside(dir, name);
    if let Err(e) = copy_into(src, &tmp) {
        remove_own_temp(&tmp);
        return Err(format!("copying: {e}"));
    }
    let got = full_hash(&tmp).map_err(|e| e.to_string());
    if got.as_deref() != Ok(want.as_str()) {
        remove_own_temp(&tmp);
        return Err(
            "the copy did not match the original byte for byte; nothing was changed".into(),
        );
    }
    let fin = place(&tmp, dir, name).map_err(|e| {
        remove_own_temp(&tmp);
        format!("naming the copy: {e}")
    })?;
    Ok((fin, want))
}

/// Write bytes (already checked against `hash` by whoever fetched them)
/// into `dir` as `name` or a free name beside it, checked again on disk.
pub fn write_verified(bytes: &[u8], hash: &str, dir: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = temp_beside(dir, name);
    let written = (|| -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        remove_own_temp(&tmp);
        return Err(format!("writing: {e}"));
    }
    if full_hash(&tmp).ok().as_deref() != Some(hash) {
        remove_own_temp(&tmp);
        return Err("the copy did not match its hash; nothing was changed".into());
    }
    place(&tmp, dir, name).map_err(|e| {
        remove_own_temp(&tmp);
        format!("naming the copy: {e}")
    })
}

/// Move `root/rel` into `root/.drive-trash/rel` (a free name there).
/// Renames only: if that cannot be done (another file system), it stays
/// where it is and this says so. Returns the path inside `root`.
pub fn to_trash(root: &Path, rel: &str) -> Result<String, String> {
    let src = under(root, rel)?;
    if !exists(&src) {
        return Err(format!("{rel} is not there"));
    }
    let (folder, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let dir = under(root, &format!("{TRASH_DIR}/{folder}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let free = free_name(name, |n| exists(&dir.join(n)));
    std::fs::rename(&src, dir.join(&free))
        .map_err(|e| format!("moving {rel} into {TRASH_DIR}: {e}"))?;
    Ok(if folder.is_empty() {
        format!("{TRASH_DIR}/{free}")
    } else {
        format!("{TRASH_DIR}/{folder}/{free}")
    })
}

/// Put `root/from` back at `root/to` (or a free name beside it). Returns
/// where it went (inside `root`).
pub fn restore(root: &Path, from: &str, to: &str) -> Result<String, String> {
    let src = under(root, from)?;
    if !exists(&src) {
        return Err(format!("{from} is no longer there"));
    }
    under(root, to)?;
    let (folder, name) = to.rsplit_once('/').unwrap_or(("", to));
    let dir = under(root, folder)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let free = free_name(name, |n| exists(&dir.join(n)));
    std::fs::rename(&src, dir.join(&free)).map_err(|e| format!("putting {to} back: {e}"))?;
    Ok(if folder.is_empty() {
        free
    } else {
        format!("{folder}/{free}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// Everything under a folder, as (relative path, contents).
    fn all(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push((
                        p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                        read(&p),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_copy_never_overwrites_and_is_verified() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("in/take.wav");
        write(&src, "new take");
        write(&t.path().join("out/take.wav"), "OLD TAKE");
        write(&t.path().join("out/take (2).wav"), "OLDER");
        let (fin, h) = copy_verified(&src, &t.path().join("out"), "take.wav").unwrap();
        assert_eq!(fin, t.path().join("out/take (3).wav"));
        assert_eq!(read(&fin), "new take");
        assert_eq!(h, drive_files::hash::of_bytes(b"new take"));
        // The taken names still hold what they held; the original is there.
        assert_eq!(read(&t.path().join("out/take.wav")), "OLD TAKE");
        assert_eq!(read(&t.path().join("out/take (2).wav")), "OLDER");
        assert_eq!(read(&src), "new take");
        // No temporary is left behind.
        assert_eq!(all(&t.path().join("out")).len(), 3);
    }

    #[test]
    fn a_bundle_is_copied_and_checked_whole() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path().join("Song.logicx");
        write(&b.join("Alternatives/000/ProjectData"), "data");
        write(&b.join("Resources/x"), "x");
        let (fin, h) = copy_verified(&b, &t.path().join("out"), "Song.logicx").unwrap();
        assert_eq!(full_hash(&fin).unwrap(), h);
        assert_eq!(all(&fin), all(&b));
        let (again, _) = copy_verified(&b, &t.path().join("out"), "Song.logicx").unwrap();
        assert_eq!(again, t.path().join("out/Song (2).logicx"));
    }

    #[test]
    fn written_bytes_must_match_their_hash() {
        let t = tempfile::tempdir().unwrap();
        let err = write_verified(b"abc", "not-the-hash", t.path(), "a.txt").unwrap_err();
        assert!(err.contains("did not match"));
        assert!(all(t.path()).is_empty(), "nothing left behind");
        let p = write_verified(
            b"abc",
            &drive_files::hash::of_bytes(b"abc"),
            t.path(),
            "a.txt",
        )
        .unwrap();
        assert_eq!(read(&p), "abc");
    }

    #[test]
    fn synced_paths_stay_under_the_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("loc");
        let outside = t.path().join("outside");
        write(&root.join("in/a.wav"), "A");
        write(&outside.join("secret"), "S");
        assert_eq!(under(&root, "in/a.wav").unwrap(), root.join("in/a.wav"));
        assert_eq!(under(&root, "").unwrap(), root);
        for bad in [
            "../outside/secret",
            "in/../../outside/secret",
            "./in/a.wav",
            "/etc/passwd",
            &outside.join("secret").to_string_lossy(),
        ] {
            assert!(under(&root, bad).is_err(), "{bad} was let through");
        }
        // A symlink inside the root does not lead out of it.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(under(&root, "link/secret").is_err());
        assert!(under(&root, "link").is_err());
        // Trash and restore refuse the same paths and touch nothing.
        assert!(to_trash(&root, "../outside/secret").is_err());
        assert!(to_trash(&root, "link/secret").is_err());
        assert!(restore(&root, "../outside/secret", "in/s").is_err());
        assert!(restore(&root, "in/a.wav", "../outside/a.wav").is_err());
        assert!(restore(&root, "in/a.wav", "link/a.wav").is_err());
        assert_eq!(read(&outside.join("secret")), "S");
        assert_eq!(read(&root.join("in/a.wav")), "A");
        assert!(!outside.join("a.wav").exists());
        // A trash folder replaced by a symlink is not followed either.
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(TRASH_DIR)).unwrap();
        assert!(to_trash(&root, "in/a.wav").is_err());
        assert_eq!(read(&root.join("in/a.wav")), "A");
    }

    #[test]
    fn trash_is_a_rename_and_restore_never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        write(&root.join("in/a.wav"), "A");
        write(&root.join(".drive-trash/in/a.wav"), "earlier A");
        let trashed = to_trash(root, "in/a.wav").unwrap();
        assert_eq!(trashed, ".drive-trash/in/a (2).wav");
        assert_eq!(read(&root.join(&trashed)), "A");
        assert_eq!(read(&root.join(".drive-trash/in/a.wav")), "earlier A");
        assert!(!root.join("in/a.wav").exists());
        // Something new took its name meanwhile: it comes back beside it.
        write(&root.join("in/a.wav"), "someone else's");
        let back = restore(root, &trashed, "in/a.wav").unwrap();
        assert_eq!(back, "in/a (2).wav");
        assert_eq!(read(&root.join("in/a.wav")), "someone else's");
        assert_eq!(read(&root.join("in/a (2).wav")), "A");
        // Every byte that was ever there is still somewhere.
        let contents: Vec<String> = all(root).into_iter().map(|(_, c)| c).collect();
        for c in ["A", "earlier A", "someone else's"] {
            assert!(contents.iter().any(|x| x == c), "{c} is gone: {contents:?}");
        }
    }

    #[test]
    fn rename_noreplace_keeps_a_folder_that_appeared_at_the_name() {
        let d = tempfile::tempdir().unwrap();
        let (from, to) = (d.path().join("tmp"), d.path().join("Song.logicx"));
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("a"), b"new").unwrap();
        // Another writer made the bundle (still empty) after the check: a
        // plain rename would replace it.
        std::fs::create_dir(&to).unwrap();
        let e = rename_noreplace(&from, &to).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(from.join("a").exists() && std::fs::read_dir(&to).unwrap().next().is_none());

        let file = d.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(d.path().join("g"), b"keep").unwrap();
        assert!(rename_noreplace(&file, &d.path().join("g")).is_err());
        assert_eq!(std::fs::read(d.path().join("g")).unwrap(), b"keep");
        rename_noreplace(&file, &d.path().join("h")).unwrap();
        assert!(!file.exists());
    }

}
