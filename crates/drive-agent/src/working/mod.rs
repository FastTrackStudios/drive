//! Working copies (docs/working-copies.md): a Drive tree folder that this
//! device also keeps as plain files in a folder on its disk, synced both
//! ways. The tree's files are the Drive's own (every device and the File
//! Provider see them as ordinary files); the folder on disk stays where it
//! was, so programs here keep editing it in place.
//!
//! One reconcile compares the folder on disk, the tree under the working
//! copy's root, and both as they were at the last sync ([`plan`]), then
//! does what that says. The last-sync view is this device's own
//! (`<agent dir>/working/<root>.json`), never synced.

pub mod plan;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use drive_files::working::{self, WorkingCopy};
use drive_files::{hash, space, tree};
use loro::{LoroDoc, TreeID};

use crate::agent::{Agent, write_atomic_async};
use plan::{Action, InTree, OnDisk, Synced};

/// The folder (in the agent's) where each working copy's last-sync view is.
const STATE_DIR: &str = "working";

/// What one reconcile did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciled {
    pub up: usize,
    pub down: usize,
    pub moved: usize,
    pub trashed: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

/// One reconcile at a time on this device: two would both act on the same
/// change.
static RECONCILING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl Agent {
    /// The working copies whose folder is on this device's disk.
    pub fn my_working_copies(&self) -> Vec<WorkingCopy> {
        let me = self.device_id();
        working::working_copies(&self.space)
            .into_iter()
            .filter(|w| w.device == me)
            .collect()
    }

    /// A working copy's folder on this disk.
    pub fn working_folder(&self, w: &WorkingCopy) -> Result<PathBuf, String> {
        let loc = self
            .my_locations()
            .into_iter()
            .find(|l| l.id == w.location)
            .ok_or_else(|| {
                format!(
                    "the working copy's location {} is not on this device",
                    w.location
                )
            })?;
        let mut p = PathBuf::from(loc.path);
        if !w.sub.is_empty() {
            p.push(&w.sub);
        }
        Ok(p)
    }

    /// Make the adopted folder at `at` (a logical path) the Drive tree's
    /// own, with its folder on this disk as its working copy: the adoption
    /// goes, a tree folder takes its place, and the first reconcile puts
    /// every file in the tree. Nothing on disk is moved or changed. Run on
    /// the device whose disk has the folder.
    pub async fn own(&self, at: &str) -> Result<(WorkingCopy, Reconciled), String> {
        let at = drive_files::adopt::logical_path(at)?;
        let ad = self
            .adoptions()
            .into_iter()
            .find(|a| a.at == at)
            .ok_or_else(|| format!("nothing is adopted at {at}"))?;
        let Some(loc) = self
            .my_locations()
            .into_iter()
            .find(|l| l.id == ad.location)
        else {
            let holder = space::locations(&self.space)
                .into_iter()
                .find(|l| l.id == ad.location)
                .map(|l| l.device)
                .and_then(|d| space::devices(&self.space).into_iter().find(|x| x.id == d))
                .map_or_else(|| "another device".to_string(), |d| d.name);
            return Err(format!(
                "{at} is a folder on {holder}: run `drive own` there"
            ));
        };
        if !loc.storage.is_empty() {
            return Err(format!("{at} is in a storage, not on a disk here"));
        }
        let mut real = PathBuf::from(&loc.path);
        if !ad.sub.is_empty() {
            real.push(&ad.sub);
        }
        if !real.is_dir() {
            return Err(format!("{} is not there", real.display()));
        }
        let (dir, name) = split(&at);
        let parent = match tree::resolve(&self.files, dir) {
            Some(Some(e)) if e.dir => Some(e.id),
            Some(None) => None,
            _ => ensure_top_dir(&self.files, dir)?,
        };
        drive_files::adopt::unadopt(&self.space, &at).map_err(|e| e.to_string())?;
        let root = match tree::lookup(&self.files, parent, name) {
            Some(e) if e.dir => e.id,
            Some(_) => return Err(format!("{at} is a file in the Drive")),
            None => tree::mkdir(&self.files, parent, name).map_err(|e| e.to_string())?,
        };
        let w = WorkingCopy {
            root: tree::id_string(root),
            device: self.device_id(),
            location: ad.location,
            sub: ad.sub,
            added: drive_files::now_ms(),
        };
        working::bind(&self.space, &w).map_err(|e| e.to_string())?;
        self.commit_files();
        self.commit_space();
        let done = self.reconcile(&w).await?;
        Ok((w, done))
    }

    /// Bring a working copy's folder and its tree folder level: what
    /// changed on one side since the last sync goes to the other.
    pub async fn reconcile(&self, w: &WorkingCopy) -> Result<Reconciled, String> {
        let _one = RECONCILING.lock().await;
        let root_id =
            tree::parse_id(&w.root).ok_or_else(|| format!("not a tree id: {}", w.root))?;
        if tree::entry(&self.files, root_id).is_none() || tree::in_trash(&self.files, root_id) {
            return Err(format!("the working copy's tree folder {} is gone", w.root));
        }
        let folder = self.working_folder(w)?;
        if !folder.is_dir() {
            return Err(format!("{} is not there", folder.display()));
        }
        let walk = folder.clone();
        let disk = tokio::task::spawn_blocking(move || walk_disk(&walk))
            .await
            .map_err(|e| e.to_string())??;
        let in_tree = walk_tree(&self.files, root_id);
        let state_file = self
            .dir
            .join(STATE_DIR)
            .join(format!("{}.json", w.root.replace(['@', '/'], "_")));
        // One reconcile of a working copy across processes too (the daemon,
        // and `drive own` run beside it): a lock on a file of its own.
        let _held = lock_file(state_file.with_extension("lock")).await?;
        let mut last = load_state(&state_file);
        let actions = plan::plan(&disk.files, &in_tree.files, &last);

        // A file new on disk with the bytes of one gone from it: renamed or
        // moved here, so the tree file moves rather than going to the Trash.
        let mut vanished: HashMap<String, (String, String)> = HashMap::new();
        for a in &actions {
            if let Action::TrashInTree { rel, id } = a
                && let Some(s) = last.get(rel)
            {
                vanished.insert(s.hash.clone(), (rel.clone(), id.clone()));
            }
        }

        let mut done = Reconciled::default();
        for dir in disk.dirs.iter().filter(|d| !in_tree.dirs.contains(*d)) {
            if let Err(e) = ensure_tree_dir(&self.files, root_id, dir) {
                done.errors.push(format!("{dir}: {e}"));
            }
        }
        for dir in in_tree.dirs.iter().filter(|d| !disk.dirs.contains(*d)) {
            if let Err(e) = std::fs::create_dir_all(folder.join(dir)) {
                done.errors.push(format!("{dir}: {e}"));
            }
        }
        // What goes to the Trash goes last, once every new file has had the
        // chance to claim it as moved.
        let (trash, rest): (Vec<Action>, Vec<Action>) = actions
            .into_iter()
            .partition(|a| matches!(a, Action::TrashInTree { .. }));
        for (n, a) in rest.iter().chain(trash.iter()).enumerate() {
            if let Err(e) = self
                .act(a, root_id, &folder, &mut last, &mut vanished, &mut done)
                .await
            {
                done.errors.push(e);
            }
            if n % 50 == 49 {
                save_state(&state_file, &last);
            }
        }
        save_state(&state_file, &last);
        self.commit_files();
        self.commit_space();
        Ok(done)
    }

    async fn act(
        &self,
        a: &Action,
        root: TreeID,
        folder: &Path,
        last: &mut BTreeMap<String, Synced>,
        vanished: &mut HashMap<String, (String, String)>,
        done: &mut Reconciled,
    ) -> Result<(), String> {
        match a {
            Action::Up { rel, id } => {
                self.up(rel, id.as_deref(), root, folder, last, vanished)
                    .await?;
                done.up += 1;
            }
            Action::Down { rel, id, hash } => {
                self.down(rel, id, hash, folder, last).await?;
                done.down += 1;
            }
            Action::Move { from, to, id } => {
                let (src, dst) = (folder.join(from), folder.join(to));
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p).map_err(|e| format!("{to}: {e}"))?;
                }
                std::fs::rename(&src, &dst).map_err(|e| format!("moving {from} to {to}: {e}"))?;
                if let Some(mut s) = last.remove(from) {
                    s.id = id.clone();
                    last.insert(to.clone(), s);
                }
                done.moved += 1;
            }
            Action::TrashInTree { rel, id } => {
                // Moved on disk instead (its bytes reappeared elsewhere): done
                // by the Up that found them.
                let still = vanished.values().any(|(r, _)| r == rel);
                if still {
                    let tid = tree::parse_id(id).ok_or("a bad tree id")?;
                    tree::trash(&self.files, tid).map_err(|e| format!("{rel}: {e}"))?;
                    done.trashed += 1;
                }
                last.remove(rel);
            }
            Action::TrashOnDisk { rel } => {
                crate::rules_fs::to_trash(folder, rel)?;
                last.remove(rel);
                done.trashed += 1;
            }
            Action::Conflict { rel, id, hash } => {
                let path = folder.join(rel);
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("{rel}: {e}"))?;
                if hash::of_bytes(&bytes) == *hash {
                    last.insert(rel.clone(), synced(id, hash, &path)?);
                    return Ok(());
                }
                // Both kept: the disk's version under a conflict name (and up
                // to the tree), the tree's at the name.
                let device = space::devices(&self.space)
                    .into_iter()
                    .find(|d| d.id == self.device_id())
                    .map_or_else(|| self.device_id(), |d| d.name);
                let date = chrono_date(drive_files::now_ms());
                let aside = plan::conflict_name(rel, &device, &date);
                std::fs::rename(&path, folder.join(&aside)).map_err(|e| format!("{rel}: {e}"))?;
                self.up(&aside, None, root, folder, last, vanished).await?;
                self.down(rel, id, hash, folder, last).await?;
                done.conflicts += 1;
            }
            Action::Forget { rel } => {
                last.remove(rel);
            }
        }
        Ok(())
    }

    /// `rel` on disk into the tree (`id`: the file to update).
    async fn up(
        &self,
        rel: &str,
        id: Option<&str>,
        root: TreeID,
        folder: &Path,
        last: &mut BTreeMap<String, Synced>,
        vanished: &mut HashMap<String, (String, String)>,
    ) -> Result<(), String> {
        let path = folder.join(rel);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("{rel}: {e}"))?;
        let h = hash::of_bytes(&bytes);
        let size = bytes.len() as u64;
        let (dir, name) = split(rel);
        let parent = ensure_tree_dir(&self.files, root, dir)?;
        let tid = match id.and_then(tree::parse_id) {
            Some(t) => t,
            None => match vanished.remove(&h) {
                // The same bytes left another path here: that tree file moves.
                Some((_, old)) => {
                    let t = tree::parse_id(&old).ok_or("a bad tree id")?;
                    tree::rename(&self.files, t, Some(parent), name).map_err(|e| e.to_string())?;
                    last.insert(rel.to_string(), synced(&old, &h, &path)?);
                    return Ok(());
                }
                None => match tree::lookup(&self.files, Some(parent), name) {
                    Some(e) if !e.dir => e.id,
                    Some(_) => return Err(format!("{rel} is a folder in the Drive")),
                    None => tree::create_file(&self.files, Some(parent), name)
                        .map_err(|e| e.to_string())?,
                },
            },
        };
        let current = tree::entry(&self.files, tid)
            .map(|e| e.hash)
            .unwrap_or_default();
        if current != h {
            self.keep_blob(&h, bytes).await;
            tree::set_content(&self.files, tid, size, &h).map_err(|e| e.to_string())?;
        }
        last.insert(rel.to_string(), synced(&tree::id_string(tid), &h, &path)?);
        Ok(())
    }

    /// The tree file's bytes onto disk at `rel`.
    async fn down(
        &self,
        rel: &str,
        id: &str,
        h: &str,
        folder: &Path,
        last: &mut BTreeMap<String, Synced>,
    ) -> Result<(), String> {
        let path = folder.join(rel);
        let bytes = if h.is_empty() {
            Vec::new()
        } else {
            self.fetch_bytes(h).await?
        };
        if !h.is_empty() && hash::of_bytes(&bytes) != h {
            return Err(format!("{rel}: the bytes came back changed"));
        }
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{rel}: {e}"))?;
        }
        write_atomic_async(path.clone(), bytes).await?;
        if !h.is_empty() {
            let _ = space::set_held(&self.space, &self.device_id(), h, Some("local"));
        }
        last.insert(rel.to_string(), synced(id, h, &path)?);
        Ok(())
    }
}

/// The view of a file now on disk, as synced at `id` and `hash`.
fn synced(id: &str, hash: &str, path: &Path) -> Result<Synced, String> {
    let md = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Synced {
        id: id.to_string(),
        hash: hash.to_string(),
        size: md.len(),
        mtime: mtime_ms(&md),
    })
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// `a/b/c` → (`a/b`, `c`); `c` → (``, `c`).
fn split(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

/// `YYYY-MM-DD` (UTC) of a time in ms.
fn chrono_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", if m <= 2 { y + 1 } else { y })
}

/// The tree folder at `rel` from the top, made as needed (`None`: the top).
fn ensure_top_dir(doc: &LoroDoc, rel: &str) -> Result<Option<TreeID>, String> {
    let mut at: Option<TreeID> = None;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        at = Some(match tree::lookup(doc, at, part) {
            Some(e) if e.dir => e.id,
            Some(_) => return Err(format!("{rel}: {part} is a file in the Drive")),
            None => tree::mkdir(doc, at, part).map_err(|e| e.to_string())?,
        });
    }
    Ok(at)
}

/// The tree folder at `rel` under `root`, made as needed.
fn ensure_tree_dir(doc: &LoroDoc, root: TreeID, rel: &str) -> Result<TreeID, String> {
    let mut at = root;
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        at = match tree::lookup(doc, Some(at), part) {
            Some(e) if e.dir => e.id,
            Some(_) => return Err(format!("{rel}: {part} is a file in the Drive")),
            None => tree::mkdir(doc, Some(at), part).map_err(|e| e.to_string())?,
        };
    }
    Ok(at)
}

/// A folder's files and folders, by path under it.
#[derive(Debug)]
pub struct Walked<T> {
    pub files: BTreeMap<String, T>,
    pub dirs: std::collections::BTreeSet<String>,
}

impl<T> Default for Walked<T> {
    fn default() -> Self {
        Walked {
            files: BTreeMap::new(),
            dirs: std::collections::BTreeSet::new(),
        }
    }
}

/// The working copy's folder on disk. Left out: `.drive-trash` (where
/// deleted and replaced files go) and Drive's own temporary files (a write
/// in progress, `.name.pid.n.tmp`).
fn walk_disk(root: &Path) -> Result<Walked<OnDisk>, String> {
    let mut out = Walked::default();
    let mut todo = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = todo.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == crate::rules_fs::TRASH_DIR {
                continue;
            }
            if name.starts_with('.') && name.ends_with(".tmp") {
                continue;
            }
            let Ok(ft) = ent.file_type() else { continue };
            let sub = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                out.dirs.insert(sub.clone());
                todo.push((ent.path(), sub));
            } else if ft.is_file() {
                let Ok(md) = ent.metadata() else { continue };
                out.files.insert(
                    sub,
                    OnDisk {
                        size: md.len(),
                        mtime: mtime_ms(&md),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// The tree under `root`.
fn walk_tree(doc: &LoroDoc, root: TreeID) -> Walked<InTree> {
    let mut out = Walked::default();
    let mut todo = vec![(root, String::new())];
    while let Some((id, rel)) = todo.pop() {
        for e in tree::children(doc, Some(id)) {
            let sub = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            if e.dir {
                out.dirs.insert(sub.clone());
                todo.push((e.id, sub));
            } else {
                out.files.insert(
                    sub,
                    InTree {
                        id: tree::id_string(e.id),
                        hash: e.hash,
                        size: e.size,
                    },
                );
            }
        }
    }
    out
}

/// An exclusive lock on `path` (made if missing), held until dropped.
async fn lock_file(path: PathBuf) -> Result<std::fs::File, String> {
    tokio::task::spawn_blocking(move || {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.lock()
            .map_err(|e| format!("locking {}: {e}", path.display()))?;
        Ok(f)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn load_state(path: &Path) -> BTreeMap<String, Synced> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &BTreeMap<String, Synced>) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(text) = serde_json::to_vec(state) {
        let _ = crate::agent::write_atomic(path, &text);
    }
}

#[cfg(test)]
mod tests;
