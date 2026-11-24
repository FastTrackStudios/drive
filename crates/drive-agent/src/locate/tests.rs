use super::*;
use drive_files::space::{Device, Location, Storage};

const HASH: &str = "ab12000000000000000000000000000000000000000000000000000000000000";

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        kind: "desktop".into(),
        os: "linux".into(),
        roles: vec![],
        approved_by: String::new(),
        added: 0,
    }
}

/// A space with two devices, a NAS folder storage, a location on the
/// workstation adopted at Studio/Songs/Fire, and a tree file whose bytes
/// the laptop keeps and the NAS stores.
fn copies() -> Copies {
    let files = LoroDoc::new();
    let space_doc = LoroDoc::new();
    space::add_device(&space_doc, &device("ws", "Workstation")).unwrap();
    space::add_device(&space_doc, &device("lap", "Laptop")).unwrap();
    space::add_storage(
        &space_doc,
        &Storage {
            id: "nas".into(),
            name: "NAS".into(),
            scheme: "fs".into(),
            location: "/mnt/blobs".into(),
            device: "ws".into(),
            added: 0,
        },
    )
    .unwrap();
    space::add_location(
        &space_doc,
        &Location {
            id: "loc".into(),
            name: "Recital songs".into(),
            device: "ws".into(),
            path: "/media/dev/recital".into(),
            volume: "/media/dev".into(),
            storage: String::new(),
            mirror_of: String::new(),
            index: String::new(),
            files: 0,
            bytes: 0,
            built: 0,
        },
    )
    .unwrap();
    adopt::adopt(
        &space_doc,
        &adopt::Adoption {
            at: "Studio/Songs/Fire".into(),
            location: "loc".into(),
            sub: "Fire".into(),
            by: "ws".into(),
            added: 0,
        },
    )
    .unwrap();
    let studio = tree::mkdir(&files, None, "Studio").unwrap();
    let f = tree::create_file(&files, Some(studio), "notes.md").unwrap();
    tree::set_content(&files, f, 5, HASH).unwrap();
    space::set_held(&space_doc, "lap", HASH, Some("local")).unwrap();
    space::set_held(&space_doc, "nas", HASH, Some("stored")).unwrap();
    Copies {
        files,
        space: space_doc,
    }
}

#[test]
fn an_adopted_file_is_on_its_device_at_its_real_path() {
    let found = locate(&copies(), "/Studio/Songs/Fire/session.rpp");
    assert_eq!(
        found,
        Found::Adopted {
            path: "Studio/Songs/Fire/session.rpp".into(),
            device: "Workstation".into(),
            location: "Recital songs".into(),
            real: "/media/dev/recital/Fire/session.rpp".into(),
        }
    );
}

#[test]
fn a_tree_file_names_who_holds_its_bytes_and_where_a_folder_storage_keeps_them() {
    let Found::TreeFile {
        size,
        hash,
        holders,
        ..
    } = locate(&copies(), "Studio/notes.md")
    else {
        panic!("a tree file")
    };
    assert_eq!((size, hash.as_str()), (5, HASH));
    assert_eq!(
        holders,
        vec![
            Holder {
                who: "Laptop".into(),
                how: "cached".into(),
                path: None
            },
            Holder {
                who: "NAS (on Workstation)".into(),
                how: "stored".into(),
                path: Some(format!("/mnt/blobs/blobs/ab/12/{HASH}")),
            },
        ]
    );
}

#[test]
fn folders_and_missing_paths_say_so() {
    let c = copies();
    assert_eq!(
        locate(&c, "Studio"),
        Found::TreeFolder {
            path: "Studio".into()
        }
    );
    assert_eq!(
        locate(&c, ""),
        Found::TreeFolder {
            path: String::new()
        }
    );
    assert_eq!(
        locate(&c, "Nope/x"),
        Found::Missing {
            path: "Nope/x".into()
        }
    );
}

#[test]
fn the_copies_load_from_the_agents_folder_without_touching_them() {
    let dir = tempfile::tempdir().unwrap();
    let docs = dir.path().join("state").join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let c = copies();
    for (id, doc) in [
        (drive_files::files_doc("sp"), &c.files),
        (drive_files::space_doc("sp"), &c.space),
    ] {
        let bytes = doc.export(loro::ExportMode::Snapshot).unwrap();
        std::fs::write(drive_native::copy_path(&docs, &id), bytes).unwrap();
    }
    let before: Vec<_> = std::fs::read_dir(&docs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let loaded = load(dir.path(), "sp").unwrap();
    assert!(matches!(
        locate(&loaded, "Studio/notes.md"),
        Found::TreeFile { .. }
    ));
    let after: Vec<_> = std::fs::read_dir(&docs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(before.len(), after.len());
    assert!(load(dir.path(), "other").is_err());
}

#[test]
fn finding_by_name_in_the_tree_ignores_case_and_the_trash() {
    let c = copies();
    assert_eq!(
        find_in_tree(&c, "NOTES", 10),
        vec!["Studio/notes.md".to_string()]
    );
    let gone = tree::resolve(&c.files, "Studio/notes.md").unwrap().unwrap();
    tree::trash(&c.files, gone.id).unwrap();
    assert!(find_in_tree(&c, "notes", 10).is_empty());
}

#[test]
fn an_indexed_file_has_its_device_real_path_and_drive_path_when_adopted() {
    let c = copies();
    assert_eq!(
        on_disk(&c, "loc", "Fire/stems/kick.wav"),
        OnDisk {
            device: "Workstation".into(),
            location: "Recital songs".into(),
            real: "/media/dev/recital/Fire/stems/kick.wav".into(),
            drive: Some("Studio/Songs/Fire/stems/kick.wav".into()),
        }
    );
    // Beside the adopted folder: on disk, not in the Drive.
    assert_eq!(on_disk(&c, "loc", "Firestarter/a.wav").drive, None);
    assert_eq!(
        on_disk(&c, "loc", "Fire").drive,
        Some("Studio/Songs/Fire".into())
    );
}
