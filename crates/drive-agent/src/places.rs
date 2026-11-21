//! This device's locations (folders indexed in place, on its disks or in
//! storages it runs), its volumes, publishing their indexes, and answering
//! the node when someone elsewhere opens one of their files.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

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

/// A path inside a location, made safe: relative, no `..`.
pub fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

impl Agent {
    pub fn report_volumes(&self) {
        let me = self.device_id();
        let _ = space::report_volumes(&self.space, &me, &volumes(&me));
        self.commit_space();
    }

    /// This device's own locations.
    pub fn my_locations(&self) -> Vec<Location> {
        let me = self.device_id();
        space::locations(&self.space)
            .into_iter()
            .filter(|l| l.device == me)
            .collect()
    }

    fn index_cache(&self, location: &str) -> PathBuf {
        self.dir.join("indexes").join(format!("{location}.idx"))
    }

    /// A location's index as last downloaded or built here.
    pub fn cached_index(&self, location: &str) -> Option<LocationIndex> {
        std::fs::read(self.index_cache(location))
            .ok()
            .and_then(|b| index::decode(&b))
    }

    /// Walk one of this device's locations, publish its index to the node
    /// and record it in the registry.
    pub async fn index_location(&self, loc: &Location) -> Result<WalkStats, String> {
        let previous = self.cached_index(&loc.id);
        let (idx, stats) = if loc.storage.is_empty() {
            let (id, root) = (loc.id.clone(), PathBuf::from(&loc.path));
            tokio::task::spawn_blocking(move || index::walk(&id, &root, previous.as_ref()))
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
            index::walk_storage(&loc.id, &op, &loc.path, previous.as_ref())
                .await
                .map_err(|e| e.to_string())?
        };
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
        Ok(stats)
    }

    /// Every location's index, downloaded when the registry has a newer
    /// one than this device's copy (so browsing works offline after).
    pub async fn refresh_indexes(&self) -> HashMap<String, LocationIndex> {
        let mut out = HashMap::new();
        for loc in space::locations(&self.space) {
            let have = self.cached_index(&loc.id);
            // The registry records when the index was published; ours is
            // stale when it was built well before that.
            let stale = have.as_ref().is_none_or(|h| h.built + 60_000 < loc.built);
            let fresh = if stale && !loc.index.is_empty() {
                match self.fetch_bytes(&loc.index).await {
                    Ok(bytes) => {
                        let _ = std::fs::create_dir_all(self.dir.join("indexes"));
                        let _ = std::fs::write(self.index_cache(&loc.id), &bytes);
                        index::decode(&bytes)
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
        out
    }

    /// The node asks for a file in one of this device's locations: put its
    /// bytes on the node and say their hash.
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
