//! What each device keeps, by its role:
//!
//! - a **storage server** keeps as much as it can: every file's bytes, as
//!   long as its disk has room beyond a reserve, and never lets any go;
//! - a **workstation** (or any other device) keeps what it is working on:
//!   the files under its active folders and its pinned files. Other bytes
//!   it fetched (opened once) go after a grace period, but only once some
//!   storage or storage server holds them, so nothing is ever the last
//!   copy to be dropped.

use std::collections::{HashMap, HashSet};

use crate::tree::Entry;

pub const STORAGE_SERVER: &str = "storage-server";
pub const WORKSTATION: &str = "workstation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    StorageServer,
    Workstation,
}

impl Role {
    pub fn of(roles: &[String]) -> Role {
        if roles.iter().any(|r| r == STORAGE_SERVER) {
            Role::StorageServer
        } else {
            Role::Workstation
        }
    }
}

/// What a device should do now.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Blobs to bring here (hashes), in order.
    pub fetch: Vec<String>,
    /// Blobs to let go of.
    pub evict: Vec<String>,
    /// Bytes that did not fit.
    pub no_room: u64,
}

pub struct Inputs<'a> {
    pub role: Role,
    pub me: &'a str,
    /// Every file in the tree, with its path.
    pub files: &'a [(String, Entry)],
    /// This device's active folders (paths, `/`-separated, no leading `/`).
    pub active: &'a [String],
    /// Hashes pinned here.
    pub pinned: &'a HashSet<String>,
    /// Hashes in this device's cache, and when each was last used (ms).
    pub cached: &'a HashMap<String, i64>,
    /// Who holds each hash, and how (`stored`, `local`, `pinned`).
    pub holders: &'a dyn Fn(&str) -> Vec<(String, String)>,
    /// Bytes this device may still fill.
    pub room: u64,
    pub now: i64,
    /// How long an opened file stays after its last use.
    pub grace_ms: i64,
}

/// Whether `path` is in `folder` (an active folder; empty: everything).
pub fn under(path: &str, folder: &str) -> bool {
    let folder = folder.trim_matches('/');
    folder.is_empty() || path == folder || path.starts_with(&format!("{folder}/"))
}

pub fn plan(i: &Inputs) -> Plan {
    let mut out = Plan::default();
    let mut want: Vec<(&str, u64)> = Vec::new();
    let mut wanted = HashSet::new();
    for (path, e) in i.files {
        if e.dir || e.hash.is_empty() {
            continue;
        }
        let keep = match i.role {
            Role::StorageServer => true,
            Role::Workstation => {
                i.pinned.contains(&e.hash) || i.active.iter().any(|f| under(path, f))
            }
        };
        if keep && wanted.insert(e.hash.as_str()) {
            want.push((&e.hash, e.size));
        }
    }
    // Smallest first: the most files for the room there is.
    want.sort_by_key(|(_, size)| *size);
    let mut room = i.room;
    for (h, size) in &want {
        if i.cached.contains_key(*h) {
            continue;
        }
        if *size <= room {
            room -= size;
            out.fetch.push(h.to_string());
        } else {
            out.no_room += size;
        }
    }
    if i.role == Role::Workstation {
        let mut evict: Vec<String> = i
            .cached
            .iter()
            .filter(|(h, used)| !wanted.contains(h.as_str()) && i.now - **used > i.grace_ms)
            .filter(|(h, _)| {
                (i.holders)(h)
                    .iter()
                    .any(|(who, how)| who != i.me && how == "stored")
            })
            .map(|(h, _)| h.clone())
            .collect();
        evict.sort();
        out.evict = evict;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::LoroDoc;

    #[test]
    fn active_copy_keeps_shared_content_when_an_inactive_copy_sorts_first() {
        let doc = LoroDoc::new();
        let active = crate::tree::mkdir(&doc, None, "Working").unwrap();
        let archived = crate::tree::create_file(&doc, None, "Archived.wav").unwrap();
        let working = crate::tree::create_file(&doc, Some(active), "Current.wav").unwrap();
        for file in [archived, working] {
            crate::tree::set_content(&doc, file, 10, "shared").unwrap();
        }
        let files = crate::tree::files(&doc);
        assert_eq!(files[0].0, "Archived.wav");
        let cached = HashMap::new();
        let inputs = Inputs {
            role: Role::Workstation,
            me: "workstation",
            files: &files,
            active: &["Working".into()],
            pinned: &HashSet::new(),
            cached: &cached,
            holders: &|_| vec![("nas".into(), "stored".into())],
            room: 10,
            now: 100,
            grace_ms: 0,
        };
        assert_eq!(plan(&inputs).fetch, ["shared"]);

        let cached = HashMap::from([("shared".into(), 0)]);
        let plan = plan(&Inputs {
            cached: &cached,
            ..inputs
        });
        assert!(plan.fetch.is_empty());
        assert!(plan.evict.is_empty());
    }

    #[test]
    fn servers_keep_everything_workstations_keep_their_work() {
        let d = LoroDoc::new();
        let songs = crate::tree::mkdir(&d, None, "Songs").unwrap();
        let a = crate::tree::mkdir(&d, Some(songs), "Song A").unwrap();
        let b = crate::tree::mkdir(&d, Some(songs), "Song B").unwrap();
        for (dir, name, h, size) in [
            (a, "drums.wav", "ha", 10u64),
            (b, "bass.wav", "hb", 20),
            (b, "keys.wav", "hc", 500),
        ] {
            let f = crate::tree::create_file(&d, Some(dir), name).unwrap();
            crate::tree::set_content(&d, f, size, h).unwrap();
        }
        let files = crate::tree::files(&d);
        let none = HashSet::new();
        let stored_on_nas = |h: &str| {
            if h == "hb" {
                vec![("nas".to_string(), "stored".to_string())]
            } else {
                vec![]
            }
        };

        // The NAS: everything that fits, smallest first.
        let cached = HashMap::new();
        let p = plan(&Inputs {
            role: Role::StorageServer,
            me: "nas",
            files: &files,
            active: &[],
            pinned: &none,
            cached: &cached,
            holders: &stored_on_nas,
            room: 100,
            now: 0,
            grace_ms: 0,
        });
        assert_eq!(p.fetch, ["ha", "hb"]);
        assert_eq!(p.no_room, 500);
        assert!(p.evict.is_empty());

        // Battleship works on Song A: it fetches that, and drops Song B's
        // bass (stored on the NAS) once unused, but keeps the keys (no
        // other copy).
        let cached: HashMap<String, i64> = [("hb".to_string(), 0), ("hc".to_string(), 0)].into();
        let p = plan(&Inputs {
            role: Role::Workstation,
            me: "battleship",
            files: &files,
            active: &["Songs/Song A".to_string()],
            pinned: &none,
            cached: &cached,
            holders: &stored_on_nas,
            room: u64::MAX,
            now: 10_000,
            grace_ms: 1_000,
        });
        assert_eq!(p.fetch, ["ha"]);
        assert_eq!(p.evict, ["hb"]);
    }
}
