//! Sidecars of this device's files: made here, where the bytes are, and
//! put on the node. The node asks for them when the app wants one that is
//! missing ([`Agent::answer_sidecars`]); the service also fills in what is
//! missing in the background ([`Agent::backfill_sidecars`]), a file at a
//! time on a low-priority thread, after indexing, never in its way.

use std::path::{Path, PathBuf};

use drive_files::space::Location;
use drive_sidecars::produce::{self, Limits, Made};
use drive_sidecars::{Kind, kinds_for, sampled_key};

use crate::agent::Agent;
use crate::places::inside;

/// Files up to this size are fully hashed as well, so their sidecars are
/// also found by full hash (a copy of the file in the space's tree).
const FULL_HASH_MAX: u64 = 1 << 30;
/// A file in a storage location (a bucket) is downloaded to be read, up to
/// this size.
const FROM_STORAGE_MAX: u64 = 256 << 20;

/// What a pass did.
#[derive(Clone, Debug, Default)]
pub struct Pass {
    pub files: usize,
    pub made: usize,
    pub no_generator: usize,
    pub failed: usize,
    pub bytes: u64,
}

/// A file to make sidecars of: where it is here, and its content key.
struct Job {
    path: PathBuf,
    name: String,
    key: String,
    size: u64,
    kinds: Vec<Kind>,
    /// A temporary copy (from a storage), removed after.
    temp: bool,
}

/// Run `f` on a thread of its own at a low priority (nice 10 on Linux),
/// so making sidecars never slows indexing, sync or the person's work.
async fn low_priority<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("drive-sidecars".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            // SAFETY: setpriority on this thread's own id only changes its
            // scheduling priority.
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
            }
            let _ = tx.send(f());
        })
        .ok()?;
    rx.await.ok()
}

impl Agent {
    fn node_and_token(&self) -> (String, Option<String>) {
        let c = self.cfg.lock().expect("cfg");
        (c.node_http(), c.token.clone())
    }

    fn with_token(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.node_and_token().1 {
            Some(t) if !t.is_empty() => rb.bearer_auth(t),
            _ => rb,
        }
    }

    /// The file at `path` in one of this device's locations, ready to read.
    async fn job_for(&self, loc: &Location, rel: &str, kinds: Vec<Kind>) -> Result<Job, String> {
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        if loc.storage.is_empty() {
            let path = inside(Path::new(&loc.path), rel).ok_or("a path outside the location")?;
            let p2 = path.clone();
            let (size, sampled) = tokio::task::spawn_blocking(move || {
                let size = std::fs::metadata(&p2).map(|m| m.len());
                (size, drive_files::hash::sampled(&p2))
            })
            .await
            .map_err(|e| e.to_string())?;
            return Ok(Job {
                path,
                name,
                key: sampled_key(&sampled.map_err(|e| format!("{rel}: {e}"))?),
                size: size.map_err(|e| format!("{rel}: {e}"))?,
                kinds,
                temp: false,
            });
        }
        // A bucket: brought here (only smaller files), then read as a file.
        let own = self
            .cfg
            .lock()
            .expect("cfg")
            .storages
            .iter()
            .find(|s| s.id == loc.storage)
            .cloned()
            .ok_or("its storage is not run here")?;
        let op = own.config.operator().map_err(|e| e.to_string())?;
        inside(Path::new("/"), rel).ok_or("a path outside the location")?;
        let key = format!(
            "{}/{}",
            loc.path.trim_matches('/'),
            rel.trim_start_matches('/')
        );
        let key = key.trim_start_matches('/').to_string();
        let meta = op.stat(&key).await.map_err(|e| e.to_string())?;
        if meta.content_length() > FROM_STORAGE_MAX {
            return Err(format!(
                "no generator: {} MB in a storage is over the {} MB cap",
                meta.content_length() >> 20,
                FROM_STORAGE_MAX >> 20
            ));
        }
        let bytes = op.read(&key).await.map_err(|e| e.to_string())?.to_vec();
        let dir = self.dir.join("tmp");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}-{name}", drive_files::hash::of_bytes(&bytes)));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        let sampled = drive_files::hash::sampled(&path).map_err(|e| e.to_string())?;
        Ok(Job {
            path,
            name,
            key: sampled_key(&sampled),
            size: bytes.len() as u64,
            kinds,
            temp: true,
        })
    }

    /// Make a job's sidecars and put them on the node. `Err` starts with
    /// `no generator` when nothing here reads the file.
    async fn run_job(&self, job: Job) -> Result<Vec<Kind>, String> {
        let limits = Limits::default();
        let (path, name, kinds, size) = (
            job.path.clone(),
            job.name.clone(),
            job.kinds.clone(),
            job.size,
        );
        let started = std::time::Instant::now();
        let made = low_priority(move || {
            let full = (size <= FULL_HASH_MAX)
                .then(|| drive_files::hash::of_file(&path).ok())
                .flatten();
            produce::produce(&path, &name, &kinds, &limits).map(|m| (m, full))
        })
        .await
        .ok_or("the producer stopped")?;
        if job.temp {
            let _ = std::fs::remove_file(&job.path);
        }
        let (made, full) = made.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for m in made {
            let kind = m.kind;
            self.upload_sidecar(&job.key, full.as_deref(), m).await?;
            out.push(kind);
        }
        tracing::info!(
            sidecar.source = %job.key,
            sidecar.kinds = out.len(),
            sidecar.ms = started.elapsed().as_millis() as u64,
            "sidecars made"
        );
        Ok(out)
    }

    async fn upload_sidecar(&self, key: &str, full: Option<&str>, m: Made) -> Result<(), String> {
        let (node, _) = self.node_and_token();
        let mut q = vec![
            ("version", m.kind.version().to_string()),
            ("mime", m.mime.to_string()),
        ];
        if let Some(f) = full {
            q.push(("also", drive_sidecars::full_key(f)));
        }
        let r = self
            .with_token(
                self.http
                    .put(format!("{node}/sidecar/{key}/{}", m.kind.as_str()))
                    .query(&q)
                    .body(m.bytes),
            )
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("the node refused a sidecar ({})", r.status()));
        }
        Ok(())
    }

    /// The node asks for sidecars of a file in one of this device's
    /// locations: make them, put them there, answer with its content key.
    pub async fn answer_sidecars(
        &self,
        location: &str,
        path: &str,
        kinds: &[String],
    ) -> Result<String, String> {
        let loc = self
            .my_locations()
            .into_iter()
            .find(|l| l.id == location)
            .ok_or("not one of this device's locations")?;
        let kinds: Vec<Kind> = kinds.iter().filter_map(|k| Kind::parse(k)).collect();
        let job = self.job_for(&loc, path, kinds).await?;
        let key = job.key.clone();
        self.run_job(job).await?;
        Ok(key)
    }

    /// Sidecars of a file browsed on this device's disks (not in a
    /// location; already resolved and confined by `browse`): made here,
    /// put on the node; the answer is its content key. The file is only
    /// read.
    pub async fn browse_sidecars(&self, path: PathBuf, kinds: &[String]) -> Result<String, String> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut kinds: Vec<Kind> = kinds.iter().filter_map(|k| Kind::parse(k)).collect();
        let fits = kinds_for(&name);
        kinds.retain(|k| fits.contains(k));
        if kinds.is_empty() {
            return Err("no generator: nothing derives that from this kind of file".into());
        }
        let p2 = path.clone();
        let (size, sampled) = tokio::task::spawn_blocking(move || {
            (
                std::fs::metadata(&p2).map(|m| m.len()),
                drive_files::hash::sampled(&p2),
            )
        })
        .await
        .map_err(|e| e.to_string())?;
        let job = Job {
            path,
            name,
            key: sampled_key(&sampled.map_err(|e| e.to_string())?),
            size: size.map_err(|e| e.to_string())?,
            kinds,
            temp: false,
        };
        let key = job.key.clone();
        self.run_job(job).await?;
        Ok(key)
    }

    /// Which of `keys` × `kinds` the node lacks.
    async fn missing(
        &self,
        keys: &[String],
        kinds: &[Kind],
    ) -> Result<Vec<(String, String)>, String> {
        let (node, _) = self.node_and_token();
        let body = serde_json::json!({
            "keys": keys,
            "kinds": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        });
        let r = self
            .with_token(
                self.http
                    .post(format!("{node}/sidecars/missing"))
                    .json(&body),
            )
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("the node answered {}", r.status()));
        }
        let v: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
        Ok(v["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
            .collect())
    }

    /// One pass over this device's locations: make the missing sidecars
    /// (of `only` kinds, or all), at most `max_files` files; `force` makes
    /// them again even when the node has them.
    #[tracing::instrument(name = "drive.sidecars.backfill", skip_all, fields(process.rss_mb = tracing::field::Empty))]
    pub async fn backfill_sidecars(&self, only: &[Kind], force: bool, max_files: usize) -> Pass {
        let _rss = crate::telemetry::RssOnDrop;
        let mut pass = Pass::default();
        for loc in self.my_locations() {
            // Buckets are read by downloading: not in the background.
            if !loc.storage.is_empty() && !force {
                continue;
            }
            let Some(index) = self.cached_index(&loc.id) else {
                continue;
            };
            let files: Vec<_> = index
                .entries
                .iter()
                .filter(|e| !e.dir && !e.sampled.is_empty())
                .filter_map(|e| {
                    let kinds: Vec<Kind> = kinds_for(&e.path)
                        .into_iter()
                        .filter(|k| only.is_empty() || only.contains(k))
                        .collect();
                    (!kinds.is_empty()).then_some((e, kinds))
                })
                .collect();
            for chunk in files.chunks(500) {
                let wanted: Vec<(String, Vec<Kind>)> = if force {
                    chunk
                        .iter()
                        .map(|(e, k)| (sampled_key(&e.sampled), k.clone()))
                        .collect()
                } else {
                    let keys: Vec<String> =
                        chunk.iter().map(|(e, _)| sampled_key(&e.sampled)).collect();
                    let kinds: Vec<Kind> = if only.is_empty() {
                        Kind::ALL.to_vec()
                    } else {
                        only.to_vec()
                    };
                    let Ok(missing) = self.missing(&keys, &kinds).await else {
                        return pass;
                    };
                    chunk
                        .iter()
                        .filter_map(|(e, k)| {
                            let key = sampled_key(&e.sampled);
                            let need: Vec<Kind> = k
                                .iter()
                                .copied()
                                .filter(|kind| {
                                    missing
                                        .iter()
                                        .any(|(mk, mn)| *mk == key && mn == kind.as_str())
                                })
                                .collect();
                            (!need.is_empty()).then_some((key, need))
                        })
                        .collect()
                };
                for (key, kinds) in wanted {
                    if pass.files >= max_files {
                        return pass;
                    }
                    let Some((e, _)) = chunk.iter().find(|(e, _)| sampled_key(&e.sampled) == key)
                    else {
                        continue;
                    };
                    pass.files += 1;
                    let job = match self.job_for(&loc, &e.path, kinds).await {
                        Ok(j) => j,
                        Err(w) if w.starts_with("no generator") => {
                            pass.no_generator += 1;
                            continue;
                        }
                        Err(w) => {
                            tracing::debug!(sidecar.path_len = e.path.len(), error = %w, "a file could not be read for its sidecars");
                            pass.failed += 1;
                            continue;
                        }
                    };
                    pass.bytes += job.size;
                    match self.run_job(job).await {
                        Ok(k) => pass.made += k.len(),
                        Err(w) if w.starts_with("no generator") => {
                            // Tell the node, so it does not ask again.
                            pass.no_generator += 1;
                            self.report_miss(&key, &w).await;
                        }
                        Err(w) => {
                            tracing::debug!(error = %w, "a sidecar could not be made");
                            pass.failed += 1;
                        }
                    }
                }
            }
        }
        pass
    }

    async fn report_miss(&self, key: &str, why: &str) {
        let (node, _) = self.node_and_token();
        let _ = self
            .with_token(
                self.http
                    .post(format!("{node}/sidecars/miss"))
                    .json(&serde_json::json!({ "key": key, "why": why })),
            )
            .send()
            .await;
    }
}
