//! This device's locations (folders indexed in place, on its disks or in
//! storages it runs), its volumes, publishing their indexes, and answering
//! the node when someone elsewhere opens one of their files.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use drive_files::index::{self, LocationIndex, WalkStats};
use drive_files::space::{self, Location, Volume};

use crate::agent::Agent;

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

/// Decoded indexes by location, with the file's stamp (modified, length)
/// they were read at.
pub(crate) type IndexCache =
    std::sync::Mutex<HashMap<String, ((Option<std::time::SystemTime>, u64), Arc<LocationIndex>)>>;

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

    pub(crate) fn index_cache(&self, location: &str) -> PathBuf {
        self.dir.join("indexes").join(format!("{location}.idx"))
    }

    /// A location's index as last downloaded or built here: decoded once,
    /// then shared until its file changes.
    pub fn cached_index(&self, location: &str) -> Option<Arc<LocationIndex>> {
        let path = self.index_cache(location);
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = (meta.modified().ok(), meta.len());
        if let Some((s, i)) = self.indexes.lock().expect("indexes").get(location)
            && *s == stamp
        {
            return Some(i.clone());
        }
        let _span = tracing::info_span!("drive.index.decode", location.id = %location).entered();
        let decoded = Arc::new(index::decode(&std::fs::read(&path).ok()?)?);
        self.indexes
            .lock()
            .expect("indexes")
            .insert(location.to_string(), (stamp, decoded.clone()));
        Some(decoded)
    }

    /// Walk one of this device's locations, publish its index to the node
    /// and record it in the registry.
    #[tracing::instrument(name = "drive.index", skip_all, fields(location.name = %loc.name, index.files = tracing::field::Empty, process.rss_mb = tracing::field::Empty))]
    pub async fn index_location(&self, loc: &Location) -> Result<WalkStats, String> {
        if space::is_paused(&self.space, &loc.id) {
            tracing::debug!(location.name = %loc.name, "paused: kept the last index");
            return Ok(WalkStats::default());
        }
        let previous = self.cached_index(&loc.id);
        // A folder this process may not read (macOS privacy protection on a
        // background service, a drive not mounted, permissions) must never
        // publish an empty index over a full one: Drive would show the
        // files as gone.
        if loc.storage.is_empty() {
            unreadable_root(Path::new(&loc.path))?;
        }
        let had = self
            .space_files_of(&loc.id)
            .unwrap_or(0)
            .max(previous.as_ref().map_or(0, |p| p.entries.len()));
        let (idx, stats) = if loc.storage.is_empty() {
            let (id, root) = (loc.id.clone(), PathBuf::from(&loc.path));
            tokio::task::spawn_blocking(move || index::walk(&id, &root, previous.as_deref()))
                .await
                .map_err(|e| e.to_string())?
        } else {
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
            index::walk_storage(&loc.id, &op, &loc.path, previous.as_deref())
                .await
                .map_err(|e| e.to_string())?
        };
        if idx.entries.is_empty() && had > 0 {
            return Err(format!(
                "found nothing where {had} entries were before: kept the last index (is the folder readable by this process?)"
            ));
        }
        // Removed (`drive location rm`) while it was walked: nothing is
        // published for it.
        if !self.has_location(&loc.id) {
            return Err(format!("{} is no longer a location", loc.name));
        }
        let bytes = index::encode(&idx);
        let _ = std::fs::create_dir_all(self.dir.join("indexes"));
        std::fs::write(self.index_cache(&loc.id), &bytes).map_err(|e| e.to_string())?;
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

    /// Every location's index, downloaded when the registry has a newer
    /// one than this device's copy (so browsing works offline after).
    #[tracing::instrument(name = "drive.indexes.refresh", skip_all, fields(indexes.count = tracing::field::Empty, process.rss_mb = tracing::field::Empty))]
    pub async fn refresh_indexes(&self) -> HashMap<String, Arc<LocationIndex>> {
        let mut out = HashMap::new();
        for loc in space::locations(&self.space) {
            let have = self.cached_index(&loc.id);
            // The registry records when the index was published; ours is
            // stale when it was built well before that. This device's own
            // locations are indexed here: its copy is the one published.
            let mine = loc.device == self.device_id();
            let got = self.fetched.lock().expect("fetched").get(&loc.id).copied();
            let stale = !mine
                && got.is_none_or(|g| g < loc.built)
                && have.as_ref().is_none_or(|h| h.built + 60_000 < loc.built);
            let fresh = if stale && !loc.index.is_empty() {
                match self.fetch_bytes(&loc.index).await {
                    Ok(bytes) => {
                        let _ = std::fs::create_dir_all(self.dir.join("indexes"));
                        let _ = std::fs::write(self.index_cache(&loc.id), &bytes);
                        self.fetched
                            .lock()
                            .expect("fetched")
                            .insert(loc.id.clone(), loc.built);
                        self.cached_index(&loc.id)
                    }
                    Err(e) => {
                        tracing::debug!(location.id = %loc.id, error = %e, "an index could not be fetched");
                        None
                    }
                }
            } else {
                None
            };
            if let Some(i) = fresh.or(have) {
                out.insert(loc.id.clone(), i);
            }
        }
        tracing::Span::current().record("indexes.count", out.len());
        crate::telemetry::mark_rss();
        out
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
            let p = inside(Path::new(&loc.path), path).ok_or("a path outside the location")?;
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
