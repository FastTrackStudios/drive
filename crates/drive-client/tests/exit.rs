//! A native app that exits right after an edit still keeps its copy
//! (drive#62): run in a child process, which edits and calls `exit`.

#![cfg(not(target_arch = "wasm32"))]

use drive_client::Drive;

const CHILD: &str = "DRIVE_CLIENT_EXIT_TEST_DIR";
const ID: &str = "home/test:exit";

#[test]
fn an_edit_just_before_exit_is_kept() {
    if let Some(dir) = std::env::var_os(CHILD) {
        // The child: no node to reach, an edit, and out at once (well
        // inside the usual 600 ms before a save).
        drive_client::set_data_dir(std::path::Path::new(&dir));
        let (a, _events) = Drive::connect(Some("ws://127.0.0.1:1/sync".to_string()));
        let doc = a.open(ID);
        doc.get_map("song").insert("title", "Last words").unwrap();
        a.commit(ID);
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "an_edit_just_before_exit_is_kept", "--nocapture"])
        .env(CHILD, dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let file = dir.path().join("docs").join(drive_native::file_name(ID));
    let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let doc = loro::LoroDoc::new();
    doc.import(&bytes).unwrap();
    assert_eq!(
        doc.get_map("song")
            .get("title")
            .and_then(|v| v.into_value().ok()),
        Some("Last words".into())
    );
}
