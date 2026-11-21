//! Carrying out this device's role (see `drive_files::placement`): a
//! storage server fetches every file's bytes and mirrors other computers'
//! locations while they are on; a workstation keeps its active folders
//! and pins, and lets other bytes go once a storage holds them.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use drive_files::placement::{self, Inputs, Role};
use drive_files::{space, tree};

use crate::agent::Agent;

/// Keep this much of the disk free (a tenth).
const RESERVE: f64 = 0.10;
/// An opened file stays three days after its last use.
const GRACE_MS: i64 = 3 * 24 * 3600 * 1000;

#[derive(Debug, Default)]
pub struct Done {
    pub fetched: usize,
    pub evicted: usize,
    pub mirrored: usize,
    pub no_room: u64,
}

/// Bytes this device may still fill on the disk holding `dir`.
pub fn room(dir: &Path) -> u64 {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else {
        return 0;
    };
    disks
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map_or(0, |d| {
            d.available_space()
                .saturating_sub((d.total_space() as f64 * RESERVE) as u64)
        })
}

impl Agent {
    pub fn role(&self) -> Role {
        let me = self.device_id();
        space::devices(&self.space)
            .into_iter()
            .find(|d| d.id == me)
            .map_or(Role::Workstation, |d| Role::of(&d.roles))
    }

    fn cached(&self) -> HashMap<String, i64> {
        let ms = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64)
        };
        std::fs::read_dir(self.dir.join("cache"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                if !drive_files::blob::valid_hash(&name) {
                    return None;
                }
                let m = e.metadata().ok()?;
                Some((name, ms(m.accessed()).max(ms(m.modified()))))
            })
            .collect()
    }

    /// One pass of this device's role.
    pub async fn place(&self) -> Done {
        let me = self.device_id();
        let role = self.role();
        let files = tree::files(&self.files);
        let active = space::active(&self.space, &me);
        let pinned: HashSet<String> = files
            .iter()
            .filter(|(_, e)| !e.hash.is_empty() && self.is_pinned(e))
            .map(|(_, e)| e.hash.clone())
            .collect();
        let cached = self.cached();
        let space_doc = self.space.clone();
        let holders = move |h: &str| space::holders(&space_doc, h);
        let p = placement::plan(&Inputs {
            role,
            me: &me,
            files: &files,
            active: &active,
            pinned: &pinned,
            cached: &cached,
            holders: &holders,
            room: room(&self.dir),
            now: drive_files::now_ms(),
            grace_ms: GRACE_MS,
        });
        let mut done = Done {
            no_room: p.no_room,
            ..Done::default()
        };
        let how = if role == Role::StorageServer {
            "stored"
        } else {
            "local"
        };
        for h in &p.fetch {
            match self.fetch(h).await {
                Ok(_) => {
                    let _ = space::set_held(
                        &self.space,
                        &me,
                        h,
                        Some(if pinned.contains(h) { "pinned" } else { how }),
                    );
                    done.fetched += 1;
                }
                Err(e) => tracing::debug!(blob.hash = %h, error = %e, "not fetched this pass"),
            }
        }
        // Already here before this device became a storage server: say so.
        if role == Role::StorageServer {
            for h in cached.keys() {
                if space::holders(&self.space, h)
                    .iter()
                    .any(|(w, how)| *w == me && how == "local")
                {
                    let _ = space::set_held(&self.space, &me, h, Some("stored"));
                }
            }
        }
        // Letting copies go is off unless this device was told it may
        // (`allow_evict`): Drive keeps every byte it has until then.
        let may_evict = self.cfg.lock().expect("cfg").allow_evict;
        if !may_evict && !p.evict.is_empty() {
            tracing::info!(
                placement.kept = p.evict.len(),
                "eviction is off: copies kept"
            );
        }
        for h in p.evict.iter().filter(|_| may_evict) {
            if std::fs::remove_file(self.cache_path(h)).is_ok() {
                let _ = space::set_held(&self.space, &me, h, None);
                done.evicted += 1;
            }
        }
        if role == Role::StorageServer {
            done.mirrored = self.mirror(&mut done.no_room).await;
        }
        if done.fetched + done.evicted > 0 {
            self.commit_space();
        }
        if done.fetched + done.evicted + done.mirrored > 0 {
            tracing::info!(
                placement.role = ?role,
                placement.fetched = done.fetched,
                placement.evicted = done.evicted,
                placement.mirrored = done.mirrored,
                "placement pass"
            );
        }
        done
    }

    /// A storage server's copy of every other computer's locations (the
    /// ones whose computer is on now), each kept as a location of its own
    /// so its files count as copies.
    async fn mirror(&self, no_room: &mut u64) -> usize {
        let me = self.device_id();
        let online = crate::daemon::online_devices(self);
        let indexes = self.refresh_indexes().await;
        let (node, space_name) = {
            let c = self.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let mut copied = 0;
        let all = space::locations(&self.space);
        for src in all
            .iter()
            .filter(|l| l.device != me && l.mirror_of.is_empty() && online.contains(&l.device))
        {
            let Some(idx) = indexes.get(&src.id) else {
                continue;
            };
            let root = self.dir.join("mirror").join(&src.id);
            let mut room_left = room(&self.dir);
            let mut changed = false;
            for e in idx.entries.iter().filter(|e| !e.dir) {
                let Some(dest) = crate::places::inside(&root, &e.path) else {
                    continue;
                };
                if std::fs::metadata(&dest).is_ok_and(|m| m.len() == e.size) {
                    continue;
                }
                if e.size > room_left {
                    *no_room += e.size;
                    continue;
                }
                let got = async {
                    let r = self
                        .http
                        .get(format!("{node}/fetch/{space_name}/{}", src.id))
                        .query(&[("path", e.path.as_str())])
                        .send()
                        .await
                        .map_err(|x| x.to_string())?;
                    if !r.status().is_success() {
                        return Err(r.status().to_string());
                    }
                    r.bytes().await.map_err(|x| x.to_string())
                }
                .await;
                match got {
                    Ok(bytes) => {
                        let _ = std::fs::create_dir_all(dest.parent().expect("a parent"));
                        let tmp = dest.with_extension("drive-part");
                        if std::fs::write(&tmp, &bytes)
                            .and_then(|_| std::fs::rename(&tmp, &dest))
                            .is_ok()
                        {
                            room_left = room_left.saturating_sub(e.size);
                            copied += 1;
                            changed = true;
                        }
                    }
                    Err(why) => {
                        tracing::debug!(location.name = %src.name, why = %why, "a mirror copy failed; next pass");
                        break;
                    }
                }
            }
            let existing = all
                .iter()
                .find(|l| l.mirror_of == src.id && l.device == me)
                .cloned();
            let loc = match existing {
                Some(l) => l,
                None if changed => {
                    let l = space::Location {
                        id: space::new_id(),
                        name: format!("{} (mirror)", src.name),
                        device: me.clone(),
                        path: root.to_string_lossy().into_owned(),
                        volume: crate::places::volume_of(&root),
                        storage: String::new(),
                        mirror_of: src.id.clone(),
                        index: String::new(),
                        files: 0,
                        bytes: 0,
                        built: 0,
                    };
                    let _ = space::add_location(&self.space, &l);
                    self.commit_space();
                    l
                }
                None => continue,
            };
            if changed && let Err(e) = self.index_location(&loc).await {
                tracing::warn!(location.name = %loc.name, error = %e, "a mirror did not index");
            }
        }
        copied
    }
}
