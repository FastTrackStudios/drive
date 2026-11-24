//! How long a walk of a real folder takes on one thread and on the
//! default number (drive#47):
//! `cargo run --release -p drive-files --example walk_time -- <folder>`.

use drive_files::index::{Previous, Tree, walk_with};

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("a folder"));
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    // Once first so both timed runs find the disk's metadata cached.
    let (warm, _) = walk_with("w", &root, None, threads);
    for t in [1, threads] {
        let start = std::time::Instant::now();
        let (idx, st) = walk_with("w", &root, None, t);
        let first = start.elapsed();
        let start = std::time::Instant::now();
        let tree = Tree::new(idx);
        let (_, again) = walk_with("w", &root, Some(&tree as &dyn Previous), t);
        println!(
            "{t} thread(s): first index {first:?} ({} files, {} hashed), reindex {:?} ({} hashed)",
            st.files,
            st.hashed,
            start.elapsed(),
            again.hashed
        );
    }
    drop(warm);
}
