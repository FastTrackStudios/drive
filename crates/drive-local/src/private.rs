//! Secret files (the agent's config, `app.port`, the endpoint key): readable
//! by this user only from the first byte, and never seen half written.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` so only this user can read it (mode 600 on Unix):
/// a fresh temp file beside it, made 600 when it is created, written, synced
/// and renamed over `path`. Readers see the old file or the new one, never a
/// partial or world-readable one.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{}: not a file path", path.display()),
        )
    })?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let written = create_private(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // The rename itself survives a crash once the folder is synced.
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// A new file (never an existing one, nor through a planted link), mode 600
/// from the moment it exists.
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_file_is_private_before_anything_is_written() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t");
        let _f = create_private(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        assert_eq!(mode(&p), 0o600);
        // Never an existing file.
        assert!(create_private(&p).is_err());
    }

    #[test]
    fn writes_replace_whole_files_and_leave_no_temp_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        write_private(&p, b"first").unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&p), 0o600);
        // An old world-readable file is replaced by a private one.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_private(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        #[cfg(unix)]
        assert_eq!(mode(&p), 0o600);
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"]);
    }

    #[test]
    fn a_missing_folder_is_an_error_and_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("gone/iroh.key");
        assert!(write_private(&p, b"k").is_err());
        assert!(!p.exists());
    }
}
