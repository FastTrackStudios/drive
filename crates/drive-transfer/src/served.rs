//! What this device has hashed for serving: (path, size, mtime) → full
//! BLAKE3 hash, one JSON line per file appended to `served.jsonl` in the
//! agent's own folder (the last line for a path wins). This file is the
//! only thing the provider writes, and only there.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub hash: String,
}

pub(crate) struct Served {
    file: PathBuf,
    map: Mutex<HashMap<String, Line>>,
}

impl Served {
    pub fn open(dir: &Path) -> Served {
        let file = dir.join("served.jsonl");
        let mut map = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&file) {
            for l in text.lines() {
                if let Ok(line) = serde_json::from_str::<Line>(l) {
                    map.insert(line.path.clone(), line);
                }
            }
        }
        Served {
            file,
            map: Mutex::new(map),
        }
    }

    /// The hash for this path, if it was hashed at this size and time.
    pub fn get(&self, path: &str, size: u64, mtime: i64) -> Option<String> {
        self.map
            .lock()
            .expect("served")
            .get(path)
            .filter(|l| l.size == size && l.mtime == mtime)
            .map(|l| l.hash.clone())
    }

    pub fn put(&self, line: Line) {
        let mut map = self.map.lock().expect("served");
        let text = serde_json::to_string(&line).expect("a line serializes");
        let wrote = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .and_then(|mut f| writeln!(f, "{text}"));
        if let Err(e) = wrote {
            tracing::warn!(error = %e, "the served-files list could not be saved; the file is hashed again next time");
        }
        map.insert(line.path.clone(), line);
    }
}
