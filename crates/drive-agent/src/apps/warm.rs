//! Warming: bring many files here ahead of use (a project's media before a
//! session opens), with progress as events: bytes, files, the rate and time
//! left, and each file's state as it changes. Cancelled, it stops where it
//! is; a file half-fetched resumes the next time anyone asks for it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drive_files::{projects, tree};
use drive_local::proto::Event;
use drive_local::{FileProgress, FileState, OpenOptions, Progress, Started, Warm};
use futures::StreamExt;

use super::resolve::Source;
use super::{Apps, ConnCtx, leases, now_ms};

/// Progress is sent at most this often while bytes move.
const TICK: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    files: u64,
    done: u64,
    failed: u64,
    bytes: u64,
    /// Bytes of files finished.
    bytes_done: u64,
    /// Bytes so far of files under way, and where each began (resumed).
    moving: HashMap<usize, (u64, u64)>,
    /// Bytes that were already here before the job (not part of its rate).
    here_bytes: u64,
}

impl State {
    fn progress(&self, job: u64, started: Instant, file: Option<FileProgress>) -> Progress {
        let moving: u64 = self.moving.values().map(|(n, _)| n).sum();
        let resumed: u64 = self.moving.values().map(|(_, s)| s).sum();
        let done = self.bytes_done + moving;
        let fetched = done.saturating_sub(self.here_bytes + resumed);
        let secs = started.elapsed().as_secs_f64();
        let rate = if secs > 0.05 {
            (fetched as f64 / secs) as u64
        } else {
            0
        };
        let left = self.bytes.saturating_sub(done);
        Progress {
            job,
            files: self.files,
            files_done: self.done,
            files_failed: self.failed,
            bytes: self.bytes,
            bytes_done: done,
            rate,
            eta_ms: (rate > 0).then(|| left * 1000 / rate),
            file,
        }
    }
}

/// Whether a source is here already (nothing to fetch), as far as can be
/// told without asking anyone.
fn is_here(apps: &Apps, src: &Source) -> bool {
    match src {
        Source::Local { .. } => true,
        Source::Tree { hash, .. } => apps.a.is_cached(hash),
        Source::Content { hash } => apps.a.is_cached(hash) || apps.cache.has(hash),
        Source::Remote { name, size, .. } => apps
            .cache
            .named(name)
            .is_some_and(|n| apps.cache.has(&n.hash) && size.is_none_or(|s| s == n.size)),
    }
}

/// The files a warm asks for: (how it is named, where it is).
pub async fn expand(apps: &Apps, w: &Warm) -> Result<Vec<(String, Source)>, String> {
    let mut out: Vec<(String, Source)> = Vec::new();
    // Other computers' folders list from their indexes: fetch any newer.
    apps.a.refresh_indexes().await;
    let mut names: Vec<String> = w.targets.clone();
    names.extend(w.folder.clone());
    for t in &names {
        out.extend(expand_target(apps, t).await?);
    }
    if let Some(p) = &w.project {
        out.extend(project_files(apps, p).await?);
    }
    if !w.kinds.is_empty() {
        let kinds = drive_spine::kinds::Kinds::load(&apps.a.spine);
        let exts: HashSet<String> = w
            .kinds
            .iter()
            .flat_map(|k| drive_spine::tags::query::kind_extensions(&kinds, k))
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        if exts.is_empty() {
            return Err(format!("no kind {} is known", w.kinds.join(", ")));
        }
        out.retain(|(label, _)| {
            label
                .rsplit_once('.')
                .is_some_and(|(_, e)| exts.contains(&e.to_ascii_lowercase()))
        });
    }
    let mut seen = HashSet::new();
    out.retain(|(label, _)| seen.insert(label.clone()));
    Ok(out)
}

/// A project's files: `NAME` (or id) of this device's org, or `ORG/NAME`
/// of another org (a church's song whose parts are on this computer). A
/// part counts here when it is in this org's locations (its own, or lent
/// to the other org's project, drive#52); parts on other orgs' computers
/// are theirs to warm. A part in the Drive tree ([`projects::TREE`]) warms
/// that folder of the tree.
async fn project_files(apps: &Apps, p: &str) -> Result<Vec<(String, Source)>, String> {
    let mine = apps
        .a
        .space_id
        .strip_suffix("/drive:space")
        .unwrap_or_default()
        .to_string();
    let own = projects::projects(&apps.a.space)
        .into_iter()
        .find(|x| x.id == p || x.name.eq_ignore_ascii_case(p));
    let (org, project) = match (own, p.split_once('/')) {
        (Some(x), _) => (mine.clone(), x),
        (None, Some((org, name))) if !org.is_empty() => {
            let id = drive_files::space_doc(org);
            let doc = apps.a.drive.open(&id).await;
            // Refused or offline: what this device has of it, if anything.
            let _ = tokio::time::timeout(Duration::from_secs(5), apps.a.drive.synced(&id)).await;
            let x = projects::projects(&doc)
                .into_iter()
                .find(|x| x.id == name || x.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| format!("no project {name} in {org}"))?;
            (org.to_string(), x)
        }
        _ => return Err(format!("no project {p}")),
    };
    let mut out = Vec::new();
    for part in &project.parts {
        if part.space_in(&org) != mine {
            tracing::debug!(
                part.space = part.space_in(&org),
                "a part on another org's computers is not warmed here"
            );
            continue;
        }
        // A folder of the Drive tree (an app's uploads), as `drive:FOLDER`.
        if part.in_tree() {
            out.extend(expand_target(apps, &format!("drive:{}", part.path)).await?);
            continue;
        }
        let Some(loc) = apps.location(&part.location) else {
            continue;
        };
        out.extend(location_folder(apps, &loc, &part.path)?);
    }
    Ok(out)
}

async fn expand_target(apps: &Apps, t: &str) -> Result<Vec<(String, Source)>, String> {
    let t = t.trim();
    if let Some(path) = t.strip_prefix("drive:") {
        let path = path.trim_matches('/');
        let start =
            tree::resolve(&apps.a.files, path).ok_or_else(|| format!("no {path} in the Drive"))?;
        if start.as_ref().is_none_or(|e| e.dir) {
            let mut out = Vec::new();
            let mut stack = vec![start.map(|e| e.id)];
            while let Some(dir) = stack.pop() {
                for e in tree::children(&apps.a.files, dir) {
                    if e.dir {
                        stack.push(Some(e.id));
                    } else if !e.hash.is_empty() {
                        out.push((
                            format!("drive:{}", tree::path_of(&apps.a.files, e.id)),
                            Source::Tree {
                                hash: e.hash.clone(),
                                size: e.size,
                            },
                        ));
                    }
                }
            }
            return Ok(out);
        }
    }
    if let Some((name, rest)) = t.split_once(':')
        && let Some(loc) = apps.location(name)
    {
        let files = location_folder(apps, &loc, rest)?;
        if !files.is_empty() {
            return Ok(files);
        }
    }
    let src = apps.resolve(t).await?;
    Ok(vec![(t.to_string(), src)])
}

/// The files of a location under `path` (all of it when empty), from its
/// index (so another computer's folder lists while it is off).
fn location_folder(
    apps: &Apps,
    loc: &drive_files::space::Location,
    path: &str,
) -> Result<Vec<(String, Source)>, String> {
    let path = path.trim_matches('/');
    let Some(idx) = apps.a.cached_index(&loc.id) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{path}/");
    let mut out = Vec::new();
    for e in idx.entries.iter().filter(|e| !e.dir) {
        if path.is_empty() || e.path.starts_with(&prefix) {
            match apps.location_source(loc, &e.path) {
                Ok(src) => out.push((format!("{}:{}", loc.name, e.path), src)),
                Err(err) => tracing::debug!(error = %err, "a file of a warmed folder is skipped"),
            }
        }
    }
    Ok(out)
}

/// Answer a warm request with its plan, then run it as a task of the
/// connection (cancelled by `cancel` or when the app goes).
pub(crate) async fn start(c: Arc<ConnCtx>, id: u64, w: Warm) {
    let apps = c.apps.clone();
    let items = match expand(&apps, &w).await {
        Ok(v) => v,
        Err(e) => return c.answer(id, Err(e)),
    };
    let mut st = State {
        files: items.len() as u64,
        ..Default::default()
    };
    let mut here = 0;
    for (_, src) in &items {
        st.bytes += src.size_hint();
        if is_here(&apps, src) {
            here += 1;
        }
    }
    c.answer(
        id,
        Ok(serde_json::to_value(Started {
            job: id,
            files: st.files,
            bytes: st.bytes,
            here,
        })
        .expect("started")),
    );
    let c2 = c.clone();
    let task = tokio::spawn(async move {
        run(c2.clone(), id, w, items, st).await;
        c2.tasks.lock().expect("tasks").remove(&id);
    });
    c.tasks
        .lock()
        .expect("tasks")
        .insert(id, task.abort_handle());
}

async fn run(c: Arc<ConnCtx>, job: u64, w: Warm, items: Vec<(String, Source)>, st: State) {
    let apps = c.apps.clone();
    let started = Instant::now();
    let state = Arc::new(Mutex::new(st));
    let opts = OpenOptions {
        lease: w.lease,
        hash: false,
    };
    let work = futures::stream::iter(items.into_iter().enumerate())
        .map(|(i, (label, src))| {
            let (apps, c, state, opts) = (apps.clone(), c.clone(), state.clone(), opts.clone());
            async move { one(&apps, &c, job, &state, started, &opts, i, label, src).await }
        })
        .buffer_unordered(w.priority.parallel())
        .collect::<Vec<()>>();
    let mut work = std::pin::pin!(work);
    let mut tick = tokio::time::interval(TICK);
    let mut last_sent = 0u64;
    loop {
        tokio::select! {
            _ = &mut work => break,
            _ = tick.tick() => {
                let p = state.lock().expect("state").progress(job, started, None);
                if p.bytes_done != last_sent {
                    last_sent = p.bytes_done;
                    c.event(Event::Progress(p));
                }
            }
        }
    }
    let p = state.lock().expect("state").progress(job, started, None);
    tracing::info!(
        apps.app = %c.app,
        apps.job = job,
        apps.files = p.files,
        apps.failed = p.files_failed,
        apps.bytes = p.bytes_done,
        apps.ms = started.elapsed().as_millis() as u64,
        "a warm job finished"
    );
    c.event(Event::Done(p));
}

#[allow(clippy::too_many_arguments)]
async fn one(
    apps: &Arc<Apps>,
    c: &ConnCtx,
    job: u64,
    state: &Arc<Mutex<State>>,
    started: Instant,
    opts: &OpenOptions,
    i: usize,
    label: String,
    src: Source,
) {
    let hint = src.size_hint();
    let here = is_here(apps, &src);
    if !here {
        let p = state.lock().expect("state").progress(
            job,
            started,
            Some(FileProgress {
                target: label.clone(),
                state: FileState::Fetching,
                size: hint,
                bytes: 0,
                file: None,
                error: None,
            }),
        );
        c.event(Event::Progress(p));
    }
    let first: Mutex<Option<u64>> = Mutex::new(None);
    let on_bytes = |n: u64| {
        let mut f = first.lock().expect("first");
        let begun = *f.get_or_insert(n);
        state.lock().expect("state").moving.insert(i, (n, begun));
    };
    let r = apps.materialize(&src, opts, &on_bytes).await;
    let mut s = state.lock().expect("state");
    s.moving.remove(&i);
    let fp = match r {
        Ok(f) => {
            if f.size > hint {
                s.bytes += f.size - hint;
            }
            s.bytes_done += f.size;
            if here {
                s.here_bytes += f.size;
            } else if let Some(b) = *first.lock().expect("first") {
                // Resumed: what was there before is not this job's rate.
                s.here_bytes += b;
            }
            s.done += 1;
            if opts.lease {
                apps.a.leases.hold(
                    c.id,
                    leases::Item {
                        target: label.clone(),
                        path: f.path.clone(),
                        hash: f.hash.clone(),
                        size: f.size,
                        how: f.how,
                        why: format!("warm #{job}"),
                        since: now_ms(),
                    },
                );
            }
            FileProgress {
                target: label,
                state: FileState::Ready,
                size: f.size,
                bytes: f.size,
                file: Some(f),
                error: None,
            }
        }
        Err(e) => {
            s.failed += 1;
            s.bytes = s.bytes.saturating_sub(hint);
            FileProgress {
                target: label,
                state: FileState::Failed,
                size: hint,
                bytes: 0,
                file: None,
                error: Some(e),
            }
        }
    };
    let p = s.progress(job, started, Some(fp));
    drop(s);
    c.event(Event::Progress(p));
}
