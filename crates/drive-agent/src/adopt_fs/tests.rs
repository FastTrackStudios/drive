//! The mount with an adopted folder, through a real FUSE mount in a temp
//! folder (skipped where FUSE is not available). The folder is read and
//! written in place; what would be deleted or replaced goes to the trash.

use std::os::unix::fs::MetadataExt;

use super::*;
use crate::adopt::tests::{device, node, snapshot, songs};

#[tokio::test(flavor = "multi_thread")]
async fn the_mount_shows_an_adopted_folder_in_place() {
    if !Path::new("/dev/fuse").exists() {
        return;
    }
    let (url, _node_dir) = node().await;
    let (dir, disk, mnt) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let a = device(&url, dir.path(), "studio", "Studio PC").await;
    let songs = songs(disk.path());
    a.adopt(&songs, "Studio/Songs").await.unwrap();
    tree::mkdir(&a.files, None, "Setlists").unwrap();

    let rt = Handle::current();
    // The mount as `drive mount` builds it: the tree with `Devices/`, and
    // the adopted folders over both.
    let view = Arc::new(std::sync::RwLock::new(crate::view::View::default()));
    let devices = crate::devices::DevicesFs::new(
        a.clone(),
        view,
        rt.clone(),
        crate::one::DEVICES_BASE,
        vec![mnt.path().to_path_buf()],
    );
    let one = crate::one::OneFs::new(
        crate::fs::DriveFs::new(a.clone(), rt.clone()),
        Some(devices),
    );
    let fs = AdoptFs::new(one, a.clone(), rt);
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("drive-adopt-test".into()),
        fuser::MountOption::DefaultPermissions,
    ];
    let session = match fuser::spawn_mount(fs, mnt.path(), &config) {
        Ok(s) => s,
        Err(_) => return, // no FUSE here (a CI container)
    };
    let m = mnt.path().to_path_buf();
    let songs2 = songs.clone();
    tokio::task::spawn_blocking(move || {
        let names = |p: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(p)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        let before = snapshot(&songs2);
        assert_eq!(names(&m), vec!["Devices", "Setlists", "Studio"]);
        assert_eq!(names(&m.join("Studio")), vec!["Songs"]);
        assert_eq!(names(&m.join("Studio/Songs")), vec!["Song A", "Song B"]);
        assert_eq!(
            std::fs::read(m.join("Studio/Songs/Song B/notes.txt")).unwrap(),
            b"bridge twice"
        );
        let real = std::fs::metadata(songs2.join("Song A/Audio/kick.wav")).unwrap();
        let seen = std::fs::metadata(m.join("Studio/Songs/Song A/Audio/kick.wav")).unwrap();
        assert_eq!((seen.len(), seen.mtime()), (real.len(), real.mtime()));
        assert_eq!(
            snapshot(&songs2),
            before,
            "reading through the mount touched a file"
        );

        // Written through the mount: on the disk, in place.
        std::fs::write(m.join("Studio/Songs/Song B/lyrics.txt"), b"verse one").unwrap();
        assert_eq!(
            std::fs::read(songs2.join("Song B/lyrics.txt")).unwrap(),
            b"verse one"
        );
        std::fs::create_dir(m.join("Studio/Songs/Song C")).unwrap();
        assert!(songs2.join("Song C").is_dir());

        // Saved the way editors save (a new file renamed over the old):
        // the old one goes to the trash.
        let rpp = m.join("Studio/Songs/Song A/Song A.rpp");
        std::fs::write(
            m.join("Studio/Songs/Song A/Song A.rpp.tmp"),
            b"<REAPER_PROJECT v2>",
        )
        .unwrap();
        std::fs::rename(m.join("Studio/Songs/Song A/Song A.rpp.tmp"), &rpp).unwrap();
        assert_eq!(std::fs::read(&rpp).unwrap(), b"<REAPER_PROJECT v2>");
        let trash = songs2.join(crate::rules_fs::TRASH_DIR);
        assert_eq!(
            std::fs::read(trash.join("Song A/Song A.rpp")).unwrap(),
            b"<REAPER_PROJECT>"
        );

        // Removed through the mount: in the trash, not deleted.
        std::fs::remove_file(m.join("Studio/Songs/Song B/lyrics.txt")).unwrap();
        assert!(!songs2.join("Song B/lyrics.txt").exists());
        assert_eq!(
            std::fs::read(trash.join("Song B/lyrics.txt")).unwrap(),
            b"verse one"
        );
        // The trash is kept on the disk but not shown in the mount.
        let trash_name = crate::rules_fs::TRASH_DIR.to_string();
        assert!(!names(&m.join("Studio/Songs")).contains(&trash_name));
        assert!(std::fs::metadata(m.join("Studio/Songs").join(&trash_name)).is_err());

        // The adopted folder itself can't be removed or renamed from above.
        assert!(std::fs::remove_dir(m.join("Studio/Songs")).is_err());
        assert!(std::fs::rename(m.join("Studio/Songs"), m.join("Studio/Old")).is_err());
        // Nor moved across to the Drive tree's own files.
        assert!(std::fs::rename(m.join("Studio/Songs/Song C"), m.join("Setlists/Song C")).is_err());
        assert!(songs2.join("Song C").is_dir());

        // The files nobody touched are as they were.
        let after = snapshot(&songs2);
        for k in ["Song A/Audio/kick.wav", "Song A/Audio", "Song B/notes.txt"] {
            let k = Path::new(k);
            assert_eq!(after.get(k), before.get(k), "{} changed", k.display());
        }
    })
    .await
    .unwrap();

    // Unadopted: gone from the mount, every file still on the disk.
    a.unadopt("Studio/Songs").unwrap();
    let m = mnt.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(
            std::fs::read_dir(m.join("Studio"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(std::fs::metadata(m.join("Studio/Songs")).is_err());
    })
    .await
    .unwrap();
    assert!(songs.join("Song A/Audio/kick.wav").exists());
    assert!(songs.join("Song C").is_dir());
    drop(session);
}
