//! `drive adopt`: a folder that lives by itself on one of this device's
//! disks, taken over by Drive in place (`drive_files::adopt`;
//! docs/adopt.md).
//!
//! Adopting never copies, moves, renames or writes a file of the folder.
//! It finds the location the folder is in (or makes the folder one and
//! indexes it), then records where the folder shows in the Drive tree.
//! Folders on the way to that path are made in the tree if they are
//! missing (`Studio` for `Studio/Songs`): empty folders, nothing else.
//!
//! The mount shows an adopted folder's own files at its logical path
//! (`adopt_fs`); this module also answers what a logical path is and
//! lists a logical folder without a mount, for the command line and tests.

use std::path::{Path, PathBuf};

use drive_files::adopt::{self, Adoption, Place};
use drive_files::index::{IndexEntry, WalkStats};
use drive_files::space::{self, Location};
use drive_files::tree;

use crate::agent::Agent;
use crate::places::confined;

/// What adopting did.
pub struct Adopted {
    pub adoption: Adoption,
    pub location: Location,
    /// Whether the folder became a new location (rather than a folder of
    /// one this device already had).
    pub new_location: bool,
    /// A new location's first index, or why it was not built now (the
    /// agent's next pass builds it).
    pub indexed: Option<Result<WalkStats, String>>,
}

/// Where a logical path is.
#[derive(Clone, Debug, PartialEq)]
pub enum Spot {
    /// A folder on the way to adopted folders.
    Above,
    /// In an adopted folder on this device's disk: its real path, and
    /// the location's folder and the path inside it.
    Here {
        adoption: Adoption,
        path: PathBuf,
        root: PathBuf,
        rel: String,
    },
    /// In an adopted folder of another device, or of a storage: the
    /// location and the path in it.
    Elsewhere {
        adoption: Adoption,
        location: Location,
        path: String,
    },
}

/// One row of a logical folder: its name, and whether it is a folder.
pub type Row = (String, bool);

impl Agent {
    pub fn adoptions(&self) -> Vec<Adoption> {
        adopt::adoptions(&self.space)
    }

    /// Take a folder of this device over in place, showing at `at` in the
    /// Drive tree.
    #[tracing::instrument(name = "drive.adopt", skip_all, fields(
        adopt.at = tracing::field::Empty,
        adopt.new_location = tracing::field::Empty,
        adopt.outcome = tracing::field::Empty,
    ))]
    pub async fn adopt(&self, folder: &Path, at: &str) -> Result<Adopted, String> {
        let span = tracing::Span::current();
        let r = self.adopt_inner(folder, at).await;
        match &r {
            Ok(a) => {
                span.record("adopt.at", a.adoption.at.as_str());
                span.record("adopt.new_location", a.new_location);
                span.record("adopt.outcome", "adopted");
            }
            Err(e) => {
                span.record("adopt.outcome", "refused");
                tracing::warn!(adopt.why = %e, "an adoption was refused");
            }
        }
        r
    }

    async fn adopt_inner(&self, folder: &Path, at: &str) -> Result<Adopted, String> {
        let at = adopt::logical_path(at)?;
        let real =
            std::fs::canonicalize(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        if !std::fs::metadata(&real)
            .map_err(|e| e.to_string())?
            .is_dir()
        {
            return Err(format!("{} is not a folder", real.display()));
        }
        if real.starts_with(&self.dir) {
            return Err("that folder is the agent's own".into());
        }
        if on_drive_mount(&real) {
            return Err(format!(
                "{} is in a Drive mount already: adopt the folder on its own disk",
                real.display()
            ));
        }
        let existing = self.adoptions();
        if let Some(why) = adopt::conflict(&existing, &at) {
            return Err(why);
        }
        if let Some(Some(e)) = tree::resolve(&self.files, &at) {
            let what = if e.dir { "a folder" } else { "a file" };
            return Err(format!(
                "the Drive tree already has {what} at {at}: choose another place, or move it first"
            ));
        }
        // The folder of a location this device has, deepest first; else a
        // new location.
        let within = self
            .my_locations()
            .into_iter()
            .filter(|l| l.storage.is_empty() && real.starts_with(&l.path))
            .max_by_key(|l| l.path.len());
        let (location, sub, new_location) = match within {
            Some(l) => {
                let rel = real
                    .strip_prefix(&l.path)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                (l, adopt::sub_path(&rel)?, false)
            }
            None => {
                let name = self.free_location_name(&real, &at);
                (self.add_location(&name, &real)?, String::new(), true)
            }
        };
        // The folders on the way, in the tree (never the adopted one).
        let mut parent = None;
        let parts: Vec<&str> = at.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent = Some(match tree::lookup(&self.files, parent, part) {
                Some(e) if e.dir => e.id,
                Some(_) => {
                    return Err(format!(
                        "{part} on the way to {at} is a file in the Drive tree"
                    ));
                }
                None => tree::mkdir(&self.files, parent, part).map_err(|e| e.to_string())?,
            });
        }
        self.commit_files();
        let adoption = Adoption {
            at,
            location: location.id.clone(),
            sub,
            by: self.device_id(),
            added: drive_files::now_ms(),
        };
        adopt::adopt(&self.space, &adoption).map_err(|e| e.to_string())?;
        self.commit_space();
        let indexed = if new_location {
            Some(self.index_location(&location).await)
        } else {
            None
        };
        if let Some(Err(e)) = &indexed {
            tracing::warn!(location.name = %location.name, error = %e, "an adopted folder is indexed on the agent's next pass");
        }
        Ok(Adopted {
            adoption,
            location,
            new_location,
            indexed,
        })
    }

    /// A name for a new location no location of this device has: the
    /// folder's own name, else the logical path's last part.
    fn free_location_name(&self, real: &Path, at: &str) -> String {
        let base = real
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| at.rsplit('/').next().unwrap_or(at).to_string());
        let taken: Vec<String> = self.my_locations().into_iter().map(|l| l.name).collect();
        (1..)
            .map(|k| {
                if k == 1 {
                    base.clone()
                } else {
                    format!("{base} ({k})")
                }
            })
            .find(|n| !taken.contains(n))
            .expect("a free name")
    }

    /// Remove an adoption: the mapping only. The folder, its files, its
    /// location and its index stay as they are.
    #[tracing::instrument(name = "drive.unadopt", skip_all, fields(adopt.at = %at, adopt.outcome = tracing::field::Empty))]
    pub fn unadopt(&self, at: &str) -> Result<Adoption, String> {
        let r = adopt::unadopt(&self.space, at).map_err(|e| e.to_string())?;
        tracing::Span::current().record(
            "adopt.outcome",
            if r.is_some() { "removed" } else { "none" },
        );
        let a = r.ok_or(format!("nothing is adopted at {at}"))?;
        self.commit_space();
        Ok(a)
    }

    /// Where an adopted folder's bytes are, to show beside it:
    /// `Studio PC · Songs` (the device, or storage, and the location).
    pub fn whereabouts(&self, a: &Adoption) -> String {
        let Some(l) = space::locations(&self.space)
            .into_iter()
            .find(|l| l.id == a.location)
        else {
            return format!("a location that is gone ({})", a.location);
        };
        let holder = if l.storage.is_empty() {
            space::devices(&self.space)
                .into_iter()
                .find(|d| d.id == l.device)
                .map_or(l.device.clone(), |d| d.name)
        } else {
            space::storages(&self.space)
                .into_iter()
                .find(|s| s.id == l.storage)
                .map_or(l.storage.clone(), |s| s.name)
        };
        format!("{holder} · {}", l.name)
    }

    /// What a logical path is, as far as adoptions go (`None`: nothing
    /// adopted there; the Drive tree's own).
    pub fn spot(&self, logical: &str) -> Option<Spot> {
        let all = self.adoptions();
        let place = adopt::place(&all, logical);
        let path = place.location_path();
        match place {
            Place::Outside => None,
            Place::Above => Some(Spot::Above),
            Place::Inside { adoption, .. } => {
                let path = path.expect("inside");
                let location = space::locations(&self.space)
                    .into_iter()
                    .find(|l| l.id == adoption.location)?;
                if location.device == self.device_id() && location.storage.is_empty() {
                    let root = PathBuf::from(&location.path);
                    let real = confined(&root, &path, &self.dir)?;
                    Some(Spot::Here {
                        adoption: adoption.clone(),
                        path: real,
                        root,
                        rel: path,
                    })
                } else {
                    Some(Spot::Elsewhere {
                        adoption: adoption.clone(),
                        location,
                        path,
                    })
                }
            }
        }
    }

    /// A logical folder's rows, as the mount shows them: inside an adopted
    /// folder its own files (read on disk here, or from the index of
    /// another device's); elsewhere the Drive tree's entries with the
    /// adopted folders (and folders on the way to them) added. An adopted
    /// folder hides a tree entry of the same name.
    pub fn logical_list(&self, dir: &str) -> Result<Vec<Row>, String> {
        let dir = dir.trim_matches('/');
        match self.spot(dir) {
            Some(Spot::Here { path, .. }) => list_disk(&path),
            Some(Spot::Elsewhere { location, path, .. }) => {
                let idx = self
                    .cached_index(&location.id)
                    .ok_or("that folder's index is not here yet")?;
                Ok(index_children(&idx.entries, &path)
                    .map(|e| (e.name().to_string(), e.dir))
                    .collect())
            }
            None | Some(Spot::Above) => {
                let all = self.adoptions();
                let added = adopt::names_in(&all, dir);
                let mut rows: Vec<Row> = match tree::resolve(&self.files, dir) {
                    Some(None) => tree::children(&self.files, None),
                    Some(Some(e)) if e.dir => tree::children(&self.files, Some(e.id)),
                    _ if !added.is_empty() => Vec::new(),
                    _ => return Err(format!("no folder {dir}")),
                }
                .into_iter()
                .filter(|e| !added.iter().any(|(n, point)| *point && *n == e.name))
                .map(|e| (e.name, e.dir))
                .collect();
                for (n, _) in added {
                    if !rows.iter().any(|(r, _)| *r == n) {
                        rows.push((n, true));
                    }
                }
                rows.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
                Ok(rows)
            }
        }
    }

    /// Where a copy of another device's file is kept here (`remote/`, by
    /// its sampled hash); refused for a sampled hash that is not one.
    pub(crate) fn remote_copy(&self, loc: &Location, e: &IndexEntry) -> Result<PathBuf, String> {
        let key = if e.sampled.is_empty() {
            drive_files::hash::of_bytes(format!("{}/{}", loc.id, e.path).as_bytes())
        } else {
            crate::synced::hash(&e.sampled)?.to_string()
        };
        Ok(self.dir.join("remote").join(key))
    }

    /// A file of another device's location, on this disk: the copy kept
    /// from before, or brought now through the node (kept beside the
    /// Devices folder's copies).
    pub async fn bring_location_file(
        &self,
        loc: &Location,
        e: &IndexEntry,
    ) -> Result<PathBuf, String> {
        let cached = self.remote_copy(loc, e)?;
        if cached.exists() {
            return Ok(cached);
        }
        let (node, space) = {
            let c = self.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let r = self
            .http
            .get(format!("{node}/fetch/{space}/{}", loc.id))
            .query(&[("path", e.path.as_str())])
            .send()
            .await
            .map_err(|x| x.to_string())?;
        if !r.status().is_success() {
            return Err(format!(
                "{}: {}",
                r.status(),
                r.text().await.unwrap_or_default()
            ));
        }
        let bytes = r.bytes().await.map_err(|x| x.to_string())?;
        std::fs::create_dir_all(cached.parent().expect("a parent")).map_err(|x| x.to_string())?;
        crate::agent::write_atomic(&cached, &bytes).map_err(|x| x.to_string())?;
        Ok(cached)
    }
}

/// A folder's entries on disk: (name, is a folder), folders first;
/// never the location's `.drive-trash`.
pub fn list_disk(path: &Path) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = std::fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .filter_map(Result::ok)
        // The location's trash is restored from the app, not browsed here.
        .filter(|d| d.file_name() != crate::rules_fs::TRASH_DIR)
        .map(|d| {
            let dir = std::fs::metadata(d.path()).is_ok_and(|m| m.is_dir());
            (d.file_name().to_string_lossy().into_owned(), dir)
        })
        .collect();
    rows.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
    Ok(rows)
}

/// The entries of an index directly in `dir` (`""`: the location's root).
pub fn index_children<'a>(
    entries: &'a [IndexEntry],
    dir: &'a str,
) -> impl Iterator<Item = &'a IndexEntry> + 'a {
    entries
        .iter()
        .filter(move |e| e.parent() == dir && !e.path.is_empty())
}

/// Whether a path is inside one of Drive's own mounts (adopting from
/// there would map Drive into itself).
fn on_drive_mount(path: &Path) -> bool {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
        return false;
    };
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .is_some_and(|d| {
            let fs = d.file_system().to_string_lossy();
            fs.starts_with("fuse.drive") || d.name().to_string_lossy().starts_with("drive")
        })
}

#[cfg(test)]
pub(crate) mod tests;
