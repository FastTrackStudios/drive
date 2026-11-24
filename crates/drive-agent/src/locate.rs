//! `drive where`: where a file of the Drive really is, answered from this
//! computer's copies alone, so it works with Drive down (no agent, no node).
//!
//! It reads, never writes: the tree's and the registry's copies in
//! `state/docs/` (written whole, then renamed into place, so reading them
//! beside a running agent is safe) and, to find a file by name, the
//! location indexes (`indexes.db`, opened only if it is already there).
//!
//! For a path it says, per kind of file:
//! - **in an adopted folder:** the device and the real path on its disk;
//! - **in the Drive tree:** the file's hash, and who holds its bytes: a
//!   device's cache, or a storage (with the blob's real path when the
//!   storage is a folder on a device).

use std::path::{Path, PathBuf};

use drive_files::{adopt, space, tree};
use loro::LoroDoc;

/// This computer's copies of a space's tree and registry.
pub struct Copies {
    pub files: LoroDoc,
    pub space: LoroDoc,
}

/// Load the copies in `<dir>/state/docs` for `space`, read-only.
pub fn load(dir: &Path, space_id: &str) -> Result<Copies, String> {
    let docs = dir.join("state").join("docs");
    let read = |id: &str| -> Result<LoroDoc, String> {
        let path = drive_native::copy_path(&docs, id);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("no copy of {id} at {}: {e}", path.display()))?;
        let doc = LoroDoc::new();
        doc.import(&bytes)
            .map_err(|e| format!("{} does not read: {e}", path.display()))?;
        Ok(doc)
    };
    Ok(Copies {
        files: read(&drive_files::files_doc(space_id))?,
        space: read(&drive_files::space_doc(space_id))?,
    })
}

/// Who holds a file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// A device's or a storage's name.
    pub who: String,
    /// `cached`, `pinned`, `stored` (as the registry says).
    pub how: String,
    /// The blob's real path, when that is known from here (a storage that
    /// is a folder on a device).
    pub path: Option<String>,
}

/// What a path of the Drive is, and where its bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Inside an adopted folder: the files are on one device's disk.
    Adopted {
        path: String,
        device: String,
        location: String,
        real: String,
    },
    /// A folder of the Drive tree.
    TreeFolder { path: String },
    /// A file of the Drive tree.
    TreeFile {
        path: String,
        size: u64,
        hash: String,
        holders: Vec<Holder>,
    },
    /// Not in the Drive (as this computer last saw it).
    Missing { path: String },
}

/// Where `path` (a path of the Drive, `Studio/Songs/x.wav`) is, from the
/// copies alone.
pub fn locate(c: &Copies, path: &str) -> Found {
    let path = path.trim_matches('/').to_string();
    let adoptions = adopt::adoptions(&c.space);
    let place = adopt::place(&adoptions, &path);
    if let adopt::Place::Inside { adoption, .. } = &place {
        let sub = place.location_path().unwrap_or_default();
        let loc = space::locations(&c.space)
            .into_iter()
            .find(|l| l.id == adoption.location);
        let (device, location, real) = match loc {
            Some(l) => (
                device_name(&c.space, &l.device),
                l.name.clone(),
                join_disk(&l.path, &sub),
            ),
            None => (
                String::from("?"),
                format!("a location that is gone ({})", adoption.location),
                sub,
            ),
        };
        return Found::Adopted {
            path,
            device,
            location,
            real,
        };
    }
    match tree::resolve(&c.files, &path) {
        Some(None) => Found::TreeFolder { path },
        Some(Some(e)) if e.dir => Found::TreeFolder { path },
        Some(Some(e)) => {
            let holders = holders(&c.space, &e.hash);
            Found::TreeFile {
                path,
                size: e.size,
                hash: e.hash,
                holders,
            }
        }
        None => Found::Missing { path },
    }
}

/// Who holds `hash`, by name, with the blob's real path where known.
fn holders(space_doc: &LoroDoc, hash: &str) -> Vec<Holder> {
    if hash.is_empty() {
        return Vec::new();
    }
    let devices = space::devices(space_doc);
    let storages = space::storages(space_doc);
    let mut out: Vec<Holder> = space::holders(space_doc, hash)
        .into_iter()
        .map(|(holder, state)| {
            if let Some(st) = storages.iter().find(|s| s.id == holder) {
                let on = device_name(space_doc, &st.device);
                let path = (st.scheme == "fs"
                    && !st.location.is_empty()
                    && drive_files::blob::valid_hash(hash))
                .then(|| join_disk(&st.location, &drive_files::blob::key(hash)));
                Holder {
                    who: if st.device.is_empty() {
                        st.name.clone()
                    } else {
                        format!("{} (on {on})", st.name)
                    },
                    how: state,
                    path,
                }
            } else {
                let who = devices
                    .iter()
                    .find(|d| d.id == holder)
                    .map_or(holder.clone(), |d| d.name.clone());
                let how = if state == "local" {
                    "cached".to_string()
                } else {
                    state
                };
                Holder {
                    who,
                    how,
                    path: None,
                }
            }
        })
        .collect();
    out.sort_by(|a, b| a.who.cmp(&b.who));
    out
}

/// Files and folders of the Drive tree whose name contains `words` (any
/// case): their paths, at most `limit`.
pub fn find_in_tree(c: &Copies, words: &str, limit: usize) -> Vec<String> {
    let words = words.to_lowercase();
    let mut out: Vec<String> = tree::files(&c.files)
        .into_iter()
        .filter(|(path, e)| {
            !tree::in_trash(&c.files, e.id)
                && path
                    .rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .to_lowercase()
                    .contains(&words)
        })
        .map(|(path, _)| path)
        .collect();
    out.sort();
    out.truncate(limit);
    out
}

/// A file a location's index lists, as `drive where` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnDisk {
    pub device: String,
    pub location: String,
    /// Its real path on that device.
    pub real: String,
    /// Its path in the Drive, when it is inside an adopted folder.
    pub drive: Option<String>,
}

/// Where the file at `path` in location `location` (an index's entry) is.
pub fn on_disk(c: &Copies, location: &str, path: &str) -> OnDisk {
    let loc = space::locations(&c.space)
        .into_iter()
        .find(|l| l.id == location);
    let (device, name, root) = match loc {
        Some(l) => (device_name(&c.space, &l.device), l.name, l.path),
        None => (String::from("?"), location.to_string(), String::new()),
    };
    let drive = adopt::adoptions(&c.space).into_iter().find_map(|a| {
        if a.location != location {
            return None;
        }
        let rel = if a.sub.is_empty() {
            path.to_string()
        } else if path == a.sub {
            String::new()
        } else {
            path.strip_prefix(&format!("{}/", a.sub))?.to_string()
        };
        Some(adopt::join(&a.at, &rel))
    });
    OnDisk {
        device,
        location: name,
        real: join_disk(&root, path),
        drive,
    }
}

fn device_name(space_doc: &LoroDoc, id: &str) -> String {
    space::devices(space_doc)
        .into_iter()
        .find(|d| d.id == id)
        .map_or_else(|| id.to_string(), |d| d.name)
}

/// `/a/b` + `c/d` as a path on a disk.
fn join_disk(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    let mut p = PathBuf::from(root);
    p.push(rel);
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests;
