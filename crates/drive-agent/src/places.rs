//! This device's locations (folders indexed in place, on its disks or in
//! storages it runs), its volumes, publishing their indexes, and answering
//! the node when someone elsewhere opens one of their files.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use drive_files::index::{self, IndexEntry, WalkStats};
use drive_files::space::{self, Location, Volume};

use crate::agent::Agent;
use crate::index_db::IndexDb;

/// Pseudo and system file systems: never volumes worth showing.
const SKIP_FS: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "overlay",
    "squashfs",
    "proc",
    "sysfs",
    "efivarfs",
    "ramfs",
    "fuse.drive",
    "fuse.portal",
    "nsfs",
];

/// The disks this device has now.
pub fn volumes(device: &str) -> Vec<Volume> {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    disks
        .iter()
        .filter(|d| {
            let fs = d.file_system().to_string_lossy();
            let mount = d.mount_point().to_string_lossy();
            !SKIP_FS.iter().any(|s| fs.starts_with(s))
                && !mount.starts_with("/nix/store")
                && !mount.starts_with("/boot")
                && !mount.starts_with("/run/user")
                && d.total_space() > 0
        })
        // One volume per device (btrfs subvolumes show once per mount).
        .filter(|d| seen.insert(d.name().to_os_string()))
        .map(|d| Volume {
            device: device.to_string(),
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            fs: d.file_system().to_string_lossy().into_owned(),
            total: d.total_space() as i64,
            free: d.available_space() as i64,
            removable: d.is_removable(),
            seen: drive_files::now_ms(),
        })
        .collect()
}

/// The volume a path is on: the longest mount point containing it, of all
/// mounts (including the ones `volumes` leaves out, then mapped back).
pub fn volume_of(path: &Path) -> String {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
        return "/".into();
    };
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map_or("/".into(), |d| {
            d.mount_point().to_string_lossy().into_owned()
        })
}

/// Folders a location may hold and still be watched live: a recursive
/// watch keeps a watch (and the watcher's bookkeeping) per folder, which on
/// a big sample drive is gigabytes. Bigger locations are rescanned on the
/// agent's schedule instead.
pub const WATCH_MAX_DIRS: usize = 20_000;

/// File systems a live watch cannot follow (changes made by other machines
/// never arrive) or that would walk the network to set one up.
const UNWATCHABLE_FS: &[&str] = &[
    "nfs", "cifs", "smb", "afpfs", "webdav", "fuse", "autofs", "9p", "sshfs",
];

/// Whether a location should be watched live, and if not, why.
pub fn watch_refusal(path: &Path, dirs: Option<usize>) -> Option<String> {
    let fs = sysinfo::Disks::new_with_refreshed_list()
        .ok()
        .and_then(|disks| {
            disks
                .iter()
                .filter(|d| path.starts_with(d.mount_point()))
                .max_by_key(|d| d.mount_point().as_os_str().len())
                .map(|d| d.file_system().to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    if UNWATCHABLE_FS.iter().any(|u| fs.starts_with(u)) {
        return Some(format!("on a {fs} file system"));
    }
    match dirs {
        None => Some("not indexed yet".into()),
        Some(n) if n > WATCH_MAX_DIRS => Some(format!("{n} folders")),
        Some(_) => None,
    }
}

/// A path inside a location, made safe: relative, no `..`.
pub fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

/// A path inside a location as it really is on disk (symlinks resolved),
/// or `None` when it leads out of the location or into the agent's own
/// folder (its credentials and keys). A path not there yet is judged by
/// the deepest folder of it that is. The agent's mirrors live in its
/// folder and are locations too: a path in one is confined to the mirror.
pub fn confined(root: &Path, rel: &str, agent_dir: &Path) -> Option<PathBuf> {
    let named = inside(root, rel)?;
    let base = std::fs::canonicalize(root).ok()?;
    let mut real = None;
    let mut rest = Vec::new();
    for p in named.ancestors() {
        if let Ok(c) = std::fs::canonicalize(p) {
            real = Some(rest.iter().rev().fold(c, |acc: PathBuf, n| acc.join(n)));
            break;
        }
        if std::fs::symlink_metadata(p).is_ok() {
            // A dangling symlink: where it points is unknown.
            return None;
        }
        rest.push(p.file_name()?.to_owned());
    }
    let real = real?;
    let agent = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    let in_agent = real.starts_with(&agent) && !base.starts_with(agent.join("mirror"));
    (real.starts_with(&base) && !in_agent).then_some(real)
}

impl Agent {
    pub fn report_volumes(&self) {
        let me = self.device_id();
        let _ = space::report_volumes(&self.space, &me, &volumes(&me));
        self.commit_space();
    }

    /// Make a folder on this device a location (registered now, indexed
    /// by the caller or the service's next pass). Refused when it already
    /// is one.
    pub fn add_location(&self, name: &str, folder: &Path) -> Result<Location, String> {
        let p = std::fs::canonicalize(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        let path = p.to_string_lossy().into_owned();
        if let Some(l) = self
            .my_locations()
            .into_iter()
            .find(|l| l.storage.is_empty() && l.path == path)
        {
            return Err(format!("already a location: {}", l.name));
        }
        let loc = Location {
            id: space::new_id(),
            name: name.to_string(),
            device: self.device_id(),
            volume: volume_of(&p),
            path,
            storage: String::new(),
            mirror_of: String::new(),
            index: String::new(),
            files: 0,
            bytes: 0,
            built: 0,
        };
        space::add_location(&self.space, &loc).map_err(|e| e.to_string())?;
        self.commit_space();
        Ok(loc)
    }

    /// This device's own locations.
    pub fn my_locations(&self) -> Vec<Location> {
        let me = self.device_id();
        space::locations(&self.space)
            .into_iter()
            .filter(|l| l.device == me)
            .collect()
    }

    /// A location's index file here; refused for an id that is not a
    /// plain name.
    pub(crate) fn index_cache(&self, location: &str) -> Result<PathBuf, String> {
        let id = crate::synced::id(location)?;
        Ok(self.dir.join("indexes").join(format!("{id}.idx")))
    }

    /// Move the tags and links of this location's folders that moved since
    /// the last walk (found by what they hold, `IndexDb::moved_folders`:
    /// the last walk is the location's rows, the new one `walk`) to their
    /// new paths.
    async fn carry_folder_keys(&self, db: &IndexDb, walk: &crate::index_db::Walk) -> usize {
        let location = walk.location();
        use drive_spine::tags::Target;
        let space = self.cfg.lock().expect("cfg").space.clone();
        let tags_id = drive_spine::tags::tags_doc(&space);
        let links_id = drive_spine::links::links_doc(&space);
        let (tags, links) = (
            self.drive.open(&tags_id).await,
            self.drive.open(&links_id).await,
        );
        let lib = drive_spine::tags::Library::load(&tags);
        let all_links = drive_spine::links::all(&links);
        let keyed = lib.targets().map(|(t, _)| t).chain(
            all_links
                .iter()
                .flat_map(|l| [l.from.clone(), l.to.clone()]),
        );
        let mut wanted: Vec<String> = keyed
            .filter_map(|t| match t {
                Target::Folder { location: l, path } if l == location => Some(path),
                _ => None,
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() {
            return 0;
        }
        let moves = match db.moved_folders(walk, &wanted).await {
            Ok(moves) => moves,
            Err(e) => {
                tracing::warn!(location.id = %location, error = %e, "moved folders were not looked for");
                return 0;
            }
        };
        for (from, to) in &moves {
            let (f, t) = (
                Target::Folder {
                    location: location.into(),
                    path: from.clone(),
                },
                Target::Folder {
                    location: location.into(),
                    path: to.clone(),
                },
            );
            let _ = drive_spine::tags::rekey(&tags, &f, &t);
            let _ = drive_spine::links::rekey(&links, &f, &t);
        }
        if !moves.is_empty() {
            self.drive.commit(&tags_id);
            self.drive.commit(&links_id);
        }
        moves.len()
    }

    /// Walk one of this device's locations, publish its index to the node
    /// and record it in the registry.
    ///
    /// The walk goes into the index database as it is found (drive#156),
    /// is made the location's index there, and the upload is encoded from
    /// it: the whole index is never held.
    #[tracing::instrument(name = "drive.index", skip_all, fields(location.name = %loc.name, index.files = tracing::field::Empty, process.rss_mb = tracing::field::Empty, folders.moved = tracing::field::Empty))]
    pub async fn index_location(&self, loc: &Location) -> Result<WalkStats, String> {
        if space::is_paused(&self.space, &loc.id) {
            tracing::debug!(location.name = %loc.name, "paused: kept the last index");
            return Ok(WalkStats::default());
        }
        // A folder this process may not read (macOS privacy protection on a
        // background service, a drive not mounted, permissions) must never
        // publish an empty index over a full one: Drive would show the
        // files as gone.
        if loc.storage.is_empty() {
            unreadable_root(Path::new(&loc.path))?;
        }
        // A `drive` command beside the running agent: the agent walks it,
        // into the database it holds (drive#157).
        if let Some(done) = self.index_db.reindex_by_daemon(&loc.id).await {
            return done.map_err(|e| e.to_string());
        }
        let db = self
            .index_db
            .db()
            .await
            .map_err(|e| format!("the index database: {e}"))?;
        let walk = db
            .begin_walk(&loc.id)
            .await
            .map_err(|e| format!("the index database: {e}"))?;
        let done = self.walk_and_publish(db, loc, &walk).await;
        if let Err(e) = db.discard_walk(walk).await {
            tracing::debug!(location.id = %loc.id, error = %e, "a walk's table was left for the next start");
        }
        done
    }

    /// Walk `loc` into `walk`, make it the location's index and publish it.
    async fn walk_and_publish(
        &self,
        db: &Arc<IndexDb>,
        loc: &Location,
        walk: &crate::index_db::Walk,
    ) -> Result<WalkStats, String> {
        let previous = db.meta(&loc.id).await.ok().flatten();
        let had = self
            .space_files_of(&loc.id)
            .unwrap_or(0)
            .max(previous.as_ref().map_or(0, |p| (p.files + p.dirs) as usize));
        // The last walk, asked of the index database a folder at a time.
        let last = previous.is_some().then(|| crate::index_db::Previous {
            db: db.clone(),
            location: loc.id.clone(),
            rt: tokio::runtime::Handle::current(),
        });
        let (stats, built) = if loc.storage.is_empty() {
            self.walk_folder(db, loc, walk, last).await?
        } else {
            self.walk_storage(db, loc, walk, last.as_ref()).await?
        };
        if stats.files + stats.dirs == 0 && had > 0 {
            return Err(format!(
                "found nothing where {had} entries were before: kept the last index (is the folder readable by this process?)"
            ));
        }
        // Removed (`drive location rm`) while it was walked: nothing is
        // published for it.
        if !self.has_location(&loc.id) {
            return Err(format!("{} is no longer a location", loc.name));
        }
        // Tagged or linked folders that moved: their tags and links go with
        // them (this device walks the location, so it alone decides).
        let moved = self.carry_folder_keys(db, walk).await;
        if moved > 0 {
            tracing::Span::current().record("folders.moved", moved);
        }
        // Into the location's rows: only those that changed are written.
        let (written, _) = db
            .commit_walk(walk, &loc.path, built)
            .await
            .map_err(|e| format!("the index database: {e}"))?;
        // Nothing changed since the index the node has: publish nothing.
        // Every full pass used to send the whole index again (tens of MB
        // for millions of files), the node rewrote every row, and the new
        // `built` time made every other device download it again (drive#47).
        // One whose last publish failed is published again.
        let published = space::locations(&self.space)
            .into_iter()
            .any(|l| l.id == loc.id && !l.index.is_empty());
        let retry = self
            .unpublished
            .lock()
            .expect("unpublished")
            .contains(&loc.id);
        if published && written == 0 && !retry {
            tracing::debug!(location.name = %loc.name, index.files = stats.files, "unchanged: nothing published");
            return Ok(stats);
        }
        let bytes = db
            .encode(&loc.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the index database lost the index it was given")?;
        self.unpublished
            .lock()
            .expect("unpublished")
            .insert(loc.id.clone());
        let (node, space_name) = {
            let c = self.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let resp = self
            .http
            .put(format!("{node}/index/{space_name}/{}", loc.id))
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("the node refused the index ({})", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        self.unpublished
            .lock()
            .expect("unpublished")
            .remove(&loc.id);
        // Removed while the index was on its way: the node drops it again,
        // so no rows stay behind for a location nobody has.
        if !self.has_location(&loc.id) {
            self.forget_index(&loc.id);
            let _ = self.drop_node_index(&loc.id).await;
            return Err(format!("{} is no longer a location", loc.name));
        }
        let hash = v["hash"].as_str().unwrap_or_default();
        space::set_index(
            &self.space,
            &loc.id,
            hash,
            stats.files as i64,
            stats.bytes as i64,
        )
        .map_err(|e| e.to_string())?;
        self.commit_space();
        tracing::info!(
            location.name = %loc.name,
            index.files = stats.files,
            index.dirs = stats.dirs,
            index.hashed = stats.hashed,
            "indexed"
        );
        tracing::Span::current().record("index.files", stats.files);
        crate::telemetry::mark_rss();
        Ok(stats)
    }

    /// Walk a folder on this device into `walk` as it is found: the
    /// walking threads hand their batches over a short queue, so a walk
    /// waits for the database rather than piling up in memory. Returns
    /// what it found and when it finished.
    async fn walk_folder(
        &self,
        db: &IndexDb,
        loc: &Location,
        walk: &crate::index_db::Walk,
        last: Option<crate::index_db::Previous>,
    ) -> Result<(WalkStats, i64), String> {
        let root = PathBuf::from(&loc.path);
        let (found, batches) = tokio::sync::mpsc::channel::<Vec<IndexEntry>>(8);
        let walking = tokio::task::spawn_blocking(move || {
            // A refused send (the database failed and stopped taking them)
            // drops the batch: the walk runs on to its end, kept nowhere.
            let emit = |batch| drop(found.blocking_send(batch));
            index::walk_streamed(
                &root,
                last.as_ref().map(|p| p as &dyn index::Previous),
                &emit,
            )
        });
        let filled = db.fill_walk(walk, batches).await;
        let stats = walking.await.map_err(|e| e.to_string())?;
        filled.map_err(|e| format!("the index database: {e}"))?;
        Ok((stats, drive_files::now_ms()))
    }

    /// Walk a location in one of this device's storages into `walk`.
    async fn walk_storage(
        &self,
        db: &IndexDb,
        loc: &Location,
        walk: &crate::index_db::Walk,
        last: Option<&crate::index_db::Previous>,
    ) -> Result<(WalkStats, i64), String> {
        let own = self
            .cfg
            .lock()
            .expect("cfg")
            .storages
            .iter()
            .find(|s| s.id == loc.storage)
            .cloned();
        let own = own.ok_or("this device does not run that location's storage")?;
        let op = own.config.operator().map_err(|e| e.to_string())?;
        let (idx, stats) = index::walk_storage(&loc.id, &op, &loc.path, last)
            .await
            .map_err(|e| e.to_string())?;
        db.walk_put(walk, &idx.entries)
            .await
            .map_err(|e| format!("the index database: {e}"))?;
        Ok((stats, idx.built))
    }

    /// A location's entries (under `dir`; `""`: all), a page at a time.
    pub fn index_pages(&self, location: &str, dir: &str) -> crate::index_db::Pages {
        crate::index_db::Pages::new(self.index_db.clone(), location, dir)
    }

    /// One entry of a location's index, from the index database.
    pub async fn index_entry(&self, location: &str, path: &str) -> Option<IndexEntry> {
        self.index_db.get(location, path).await.ok().flatten()
    }

    /// A folder of a location's index (`""`: its root), folders first.
    pub async fn index_list(&self, location: &str, dir: &str) -> Vec<IndexEntry> {
        self.index_db.list(location, dir).await.unwrap_or_default()
    }

    /// Download every index the registry has newer than this device's
    /// copy into the index database (so browsing works offline after),
    /// holding each only while it is put there.
    #[tracing::instrument(name = "drive.indexes.sync", skip_all, fields(indexes.fetched = tracing::field::Empty, process.rss_mb = tracing::field::Empty))]
    pub async fn sync_indexes(&self) {
        // Beside the running agent: it downloads them, into its database.
        if let Some(done) = self.index_db.sync_by_daemon().await {
            if let Err(e) = done {
                tracing::debug!(error = %e, "the running agent did not download the indexes");
            }
            return;
        }
        let Ok(db) = self.index_db.db().await else {
            return;
        };
        let mut fetched = 0;
        for loc in space::locations(&self.space) {
            // This device's own locations are indexed here: its copy is the
            // one published.
            if loc.device == self.device_id() || loc.index.is_empty() {
                continue;
            }
            // Stale when the registry says it was published well after the
            // copy here was built.
            let have = self.index_db.meta(&loc.id).await.ok().flatten();
            let got = self.fetched.lock().expect("fetched").get(&loc.id).copied();
            let stale = got.is_none_or(|g| g < loc.built)
                && have.as_ref().is_none_or(|h| h.built + 60_000 < loc.built);
            if !stale {
                continue;
            }
            let index = match self.fetch_bytes(&loc.index).await {
                Ok(bytes) => index::decode(&bytes),
                Err(e) => {
                    tracing::debug!(location.id = %loc.id, error = %e, "an index could not be fetched");
                    continue;
                }
            };
            let Some(index) = index.filter(|i| i.location == loc.id) else {
                tracing::warn!(location.id = %loc.id, "a downloaded index did not decode as that location's");
                continue;
            };
            match db.put(&index).await {
                Ok(_) => {
                    fetched += 1;
                    self.fetched
                        .lock()
                        .expect("fetched")
                        .insert(loc.id.clone(), loc.built);
                }
                Err(e) => {
                    tracing::warn!(location.id = %loc.id, error = %e, "a downloaded index was not put in the index database")
                }
            }
        }
        tracing::Span::current().record("indexes.fetched", fetched);
        crate::telemetry::mark_rss();
    }

    /// The node asks for a file in one of this device's locations: put its
    /// bytes on the node and say their hash.
    #[tracing::instrument(name = "drive.fetch", skip_all, fields(fetch.bytes = tracing::field::Empty, process.rss_mb = tracing::field::Empty))]
    pub async fn answer_fetch(&self, location: &str, path: &str) -> Result<String, String> {
        let loc = self
            .my_locations()
            .into_iter()
            .find(|l| l.id == location)
            .ok_or("not one of this device's locations")?;
        let bytes = if loc.storage.is_empty() {
            let p = confined(Path::new(&loc.path), path, &self.dir)
                .ok_or("a path outside the location")?;
            tokio::fs::read(&p)
                .await
                .map_err(|e| format!("{path}: {e}"))?
        } else {
            let own = self
                .cfg
                .lock()
                .expect("cfg")
                .storages
                .iter()
                .find(|s| s.id == loc.storage)
                .cloned();
            let op = own
                .ok_or("its storage is not run here")?
                .config
                .operator()
                .map_err(|e| e.to_string())?;
            inside(Path::new("/"), path).ok_or("a path outside the location")?;
            let key = format!(
                "{}/{}",
                loc.path.trim_matches('/'),
                path.trim_start_matches('/')
            );
            op.read(key.trim_start_matches('/'))
                .await
                .map_err(|e| e.to_string())?
                .to_vec()
        };
        tracing::Span::current().record("fetch.bytes", bytes.len());
        crate::telemetry::mark_rss();
        let h = drive_files::hash::of_bytes(&bytes);
        let url = format!("{}/blobs/{h}", self.cfg.lock().expect("cfg").node_http());
        let r = self
            .http
            .put(&url)
            .body(bytes)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("the node refused the bytes ({})", r.status()));
        }
        Ok(h)
    }
}

/// Why a location's folder cannot be indexed now, if it cannot: missing, not
/// a folder, or not listable by this process.
fn unreadable_root(root: &Path) -> Result<(), String> {
    match std::fs::read_dir(root) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{} cannot be read: {e}", root.display())),
    }
}

impl Agent {
    /// How many files the space's registry says a location had at its last
    /// index (what other devices see), when it says.
    fn space_files_of(&self, location: &str) -> Option<usize> {
        space::locations(&self.space)
            .into_iter()
            .find(|l| l.id == location)
            .map(|l| l.files as usize)
            .filter(|n| *n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_paths_resolve_inside_the_location_and_never_into_the_agent() {
        let t = tempfile::tempdir().unwrap();
        let loc = t.path().join("loc");
        let agent = t.path().join("agent");
        let outside = t.path().join("outside");
        for d in [&loc, &agent, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(loc.join("a.wav"), "A").unwrap();
        std::fs::write(outside.join("secret"), "S").unwrap();
        std::fs::write(agent.join("config.json"), "{}").unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(
            confined(&loc, "a.wav", &agent),
            Some(canon(&loc.join("a.wav")))
        );
        // Not there yet: judged by the folder it would be in.
        assert_eq!(
            confined(&loc, "new/b.wav", &agent),
            Some(canon(&loc).join("new/b.wav"))
        );
        assert_eq!(confined(&loc, "../outside/secret", &agent), None);
        // A symlink inside the location pointing out of it.
        std::os::unix::fs::symlink(&outside, loc.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), loc.join("s")).unwrap();
        assert_eq!(confined(&loc, "out/secret", &agent), None);
        assert_eq!(confined(&loc, "s", &agent), None);
        assert_eq!(confined(&loc, "out/new", &agent), None);
        // A location over the agent's own folder never reaches it.
        assert_eq!(confined(t.path(), "agent/config.json", &agent), None);
        std::os::unix::fs::symlink(&agent, loc.join("ag")).unwrap();
        assert_eq!(confined(t.path(), "loc/ag/config.json", &agent), None);
        // The agent's mirrors are locations too, confined to themselves.
        let mirror = agent.join("mirror/m1");
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(mirror.join("c.wav"), "C").unwrap();
        assert_eq!(
            confined(&mirror, "c.wav", &agent),
            Some(canon(&mirror.join("c.wav")))
        );
        assert_eq!(confined(&mirror, "../../config.json", &agent), None);
    }
}

#[cfg(test)]
mod reindex_tests {
    use drive_files::space;

    use crate::adopt::tests::{device, node, songs};

    fn built(a: &crate::agent::Agent, id: &str) -> (String, i64) {
        let l = space::locations(&a.space)
            .into_iter()
            .find(|l| l.id == id)
            .unwrap();
        (l.index, l.built)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_location_publishes_nothing_and_a_change_publishes_again() {
        let (url, _node_dir) = node().await;
        let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = device(&url, dir.path(), "studio", "Studio PC").await;
        let songs = songs(disk.path());
        let loc = a.add_location("Songs", &songs).unwrap();
        a.index_location(&loc).await.unwrap();
        let first = built(&a, &loc.id);
        assert!(!first.0.is_empty());

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        a.index_location(&loc).await.unwrap();
        assert_eq!(
            built(&a, &loc.id),
            first,
            "nothing changed: not published again"
        );

        std::fs::write(songs.join("new take.wav"), b"new").unwrap();
        a.index_location(&loc).await.unwrap();
        let after = built(&a, &loc.id);
        assert!(
            after.1 > first.1 && after.0 != first.0,
            "a change is published"
        );
        assert!(
            a.index_db
                .get(&loc.id, "new take.wav")
                .await
                .unwrap()
                .is_some(),
            "and kept in the index database"
        );
        assert!(!dir.path().join("indexes").exists(), "and in no index file");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn index_files_move_into_the_database_and_a_removed_location_leaves_it() {
        let (url, _node_dir) = node().await;
        let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = device(&url, dir.path(), "studio", "Studio PC").await;
        let songs = songs(disk.path());
        let loc = a.add_location("Songs", &songs).unwrap();
        a.index_location(&loc).await.unwrap();
        // An agent from before the database: only the file.
        let idx = a
            .index_db
            .db()
            .await
            .unwrap()
            .load(&loc.id)
            .await
            .unwrap()
            .unwrap();
        a.index_db.remove(&loc.id).await.unwrap();
        let file = dir.path().join("indexes").join(format!("{}.idx", loc.id));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, drive_files::index::encode(&idx)).unwrap();
        drop(a);
        let again = device(&url, dir.path(), "studio", "Studio PC").await;
        assert_eq!(
            again
                .index_db
                .db()
                .await
                .unwrap()
                .load(&loc.id)
                .await
                .unwrap()
                .unwrap()
                .entries,
            idx.entries,
            "imported at start"
        );
        assert!(!file.exists(), "and the file removed");

        again.remove_location("Songs", false).await.unwrap();
        for _ in 0..100 {
            if again.index_db.meta(&loc.id).await.unwrap().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("a removed location's index stayed in the database");
    }
}
