//! What a session works with: this device's agent (the space's documents),
//! the rules engine's documents, the node's HTTP API, the indexes, and the
//! session's limits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use drive_files::index::{IndexEntry, LocationIndex};
use drive_files::projects::{self, Project};
use drive_files::space::{self, Location};
use drive_files::tree::{self, Entry};
use drive_spine::tags::{Library, Target};
use serde_json::{Value, json};

use super::Options;
use super::plan::Plan;
use crate::agent::{Agent, Config};
use crate::rules::Engine;

pub struct Ctx {
    pub a: Agent,
    pub e: Engine,
    /// `agent: <client> on <device>`: every edit's author.
    pub author: String,
    pub space: String,
    pub node: String,
    token: Option<String>,
    http: reqwest::Client,
    pub opts: Options,
    writes: AtomicUsize,
    pub(super) plans: std::sync::Mutex<HashMap<String, Plan>>,
    indexes: tokio::sync::Mutex<Option<(Instant, HashMap<String, std::sync::Arc<LocationIndex>>)>>,
    /// When other devices' indexes were last downloaded for a lookup.
    synced: tokio::sync::Mutex<Option<Instant>>,
}

/// A thing an agent names: a Drive file, a computer's file, a project, a
/// document, or a raw content key.
#[derive(Clone, Debug)]
pub enum Thing {
    Drive {
        entry: Entry,
        path: String,
    },
    Located {
        loc: Location,
        entry: IndexEntry,
    },
    Local {
        path: PathBuf,
        sampled: String,
        size: u64,
    },
    Project(Project),
    Doc(String),
    Content(String),
}

impl Thing {
    /// What tags and fields are kept under.
    pub fn target(&self) -> Result<Target, String> {
        match self {
            Thing::Drive { entry, .. } => Ok(Target::File(tree::id_string(entry.id))),
            Thing::Located { loc, entry } => {
                if entry.dir || entry.sampled.is_empty() {
                    Err(format!(
                        "{}:{} has no content hash (a folder, or unreadable): only files on a computer can be tagged",
                        loc.name, entry.path
                    ))
                } else {
                    Ok(Target::Content(entry.sampled.clone()))
                }
            }
            Thing::Local { sampled, .. } => Ok(Target::Content(sampled.clone())),
            Thing::Project(p) => Ok(Target::Project(p.id.clone())),
            Thing::Doc(id) => Ok(Target::Doc(id.clone())),
            Thing::Content(h) => Ok(Target::Content(h.clone())),
        }
    }

    /// How an agent names it back.
    pub fn label(&self) -> String {
        match self {
            Thing::Drive { path, .. } => format!("drive:{path}"),
            Thing::Located { loc, entry } => format!("{}:{}", loc.name, entry.path),
            Thing::Local { path, .. } => path.display().to_string(),
            Thing::Project(p) => format!("project:{}", p.name),
            Thing::Doc(id) => format!("doc:{id}"),
            Thing::Content(h) => format!("content:{h}"),
        }
    }
}

/// A page of a listing.
pub fn page<T: serde::Serialize>(items: Vec<T>, offset: usize, limit: usize) -> Value {
    let total = items.len();
    let rows: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let next = (offset + rows.len() < total).then_some(offset + rows.len());
    json!({ "total": total, "offset": offset, "next_offset": next, "items": rows })
}

pub fn human(n: u64) -> String {
    crate::human(n)
}

impl Ctx {
    /// Open the space as this device, for `client`.
    pub async fn open(dir: &Path, opts: Options, client: &str) -> Result<Ctx, String> {
        let cfg = Config::load(dir)?;
        let author = format!("agent: {client} on {}", cfg.device_name);
        // The session's own copies of the documents (beside the agent's,
        // seeded from them: it works offline at once and never writes the
        // files a running `serve` writes).
        let state = dir.join("mcp").join("state");
        seed(&dir.join("state").join("docs"), &state.join("docs"));
        let a = Agent::start_with(dir, cfg, &author, state).await;
        Ok(Ctx::over(a, opts, author).await)
    }

    /// A context over an agent already started.
    pub async fn over(a: Agent, opts: Options, author: String) -> Ctx {
        let e = Engine::open(&a).await;
        let cfg = a.cfg.lock().expect("cfg").clone();
        Ctx {
            space: cfg.space.clone(),
            node: cfg.node_http(),
            token: cfg.token.clone(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            a,
            e,
            author,
            opts,
            writes: AtomicUsize::new(0),
            plans: std::sync::Mutex::new(HashMap::new()),
            indexes: tokio::sync::Mutex::new(None),
            synced: tokio::sync::Mutex::new(None),
        }
    }

    pub fn writes_used(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    /// Take `n` writes from the session's allowance, or refuse.
    pub fn charge(&self, n: usize) -> Result<(), String> {
        let max = self.opts.max_writes;
        let mut cur = self.writes.load(Ordering::SeqCst);
        loop {
            if cur + n > max {
                return Err(format!(
                    "refused: this session may make {max} writes and has made {cur}; this needs {n}. Ask the person to start a new session (or raise --max-writes)"
                ));
            }
            match self
                .writes
                .compare_exchange(cur, cur + n, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(()),
                Err(now) => cur = now,
            }
        }
    }

    /// `offset` and `limit` from a tool's arguments (`limit` at most the
    /// session's page size; 50 when not given).
    pub fn paging(&self, args: &Value) -> (usize, usize) {
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = (args["limit"].as_u64().unwrap_or(50) as usize).clamp(1, self.opts.page_max);
        (offset, limit)
    }

    // ── the node ────────────────────────────────────────────────────────

    async fn answer(r: reqwest::Response) -> Result<Value, String> {
        let status = r.status();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        let why = body["why"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| status.to_string());
        Err(match body["state"].as_str() {
            Some(state) => format!("{state}: {why}"),
            None => format!("the node answered {why}"),
        })
    }

    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let mut req = self.http.get(format!("{}{path}", self.node)).query(query);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        Ctx::answer(req.send().await.map_err(|e| format!("the node: {e}"))?).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let mut req = self.http.post(format!("{}{path}", self.node)).json(body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        Ctx::answer(req.send().await.map_err(|e| format!("the node: {e}"))?).await
    }

    /// Whether the node has something at `path` (a blob, a sidecar).
    pub async fn exists(&self, path: &str, query: &[(&str, &str)]) -> bool {
        let mut req = self.http.get(format!("{}{path}", self.node)).query(query);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.is_ok_and(|r| r.status().is_success())
    }

    /// Whether the node holds a blob (`HEAD`: no bytes move).
    pub async fn head(&self, path: &str) -> bool {
        let mut req = self.http.head(format!("{}{path}", self.node));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.is_ok_and(|r| r.status().is_success())
    }

    // ── the registry ────────────────────────────────────────────────────

    pub fn device_names(&self) -> HashMap<String, String> {
        space::devices(&self.a.space)
            .into_iter()
            .map(|d| (d.id, d.name))
            .chain([("node".to_string(), "the node".to_string())])
            .collect()
    }

    pub fn device_id(&self, name: &str) -> Result<String, String> {
        space::devices(&self.a.space)
            .into_iter()
            .find(|d| d.id == name || d.name.eq_ignore_ascii_case(name))
            .map(|d| d.id)
            .ok_or_else(|| format!("no device {name}; `devices` lists them"))
    }

    /// Devices whose agent is here now (their presence), and this one when
    /// its `serve` runs.
    pub fn online(&self) -> std::collections::HashSet<String> {
        let mut on: std::collections::HashSet<String> = self
            .a
            .drive
            .others(&self.a.space_id)
            .into_iter()
            .filter_map(|(_, _, json)| {
                serde_json::from_str::<Value>(&json).ok()?["data"]["device"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        if crate::rules::served(&self.a.dir) {
            on.insert(self.a.device_id());
        }
        on
    }

    pub fn location(&self, name: &str) -> Result<Location, String> {
        space::locations(&self.a.space)
            .into_iter()
            .find(|l| l.id == name || l.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("no location {name}; `locations` lists them"))
    }

    pub fn project(&self, name: &str) -> Result<Project, String> {
        projects::projects(&self.a.space)
            .into_iter()
            .find(|p| p.id == name || p.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("no project {name}; `projects` lists them"))
    }

    pub fn tags(&self) -> Library {
        Library::load(&self.e.tags)
    }

    /// A tag by id, name, shorthand or alias.
    pub fn tag_id(&self, lib: &Library, name: &str) -> Result<Option<String>, String> {
        if lib.get(name).is_some() {
            return Ok(Some(name.to_string()));
        }
        match lib.resolve(name).as_slice() {
            [one] => Ok(Some(one.clone())),
            [] => Ok(None),
            many => Err(format!(
                "{name} could be {}; give the id",
                many.iter()
                    .map(|id| format!("{} ({id})", lib.label(id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Every location's index (cached for a minute).
    pub async fn indexes(&self) -> HashMap<String, std::sync::Arc<LocationIndex>> {
        let mut c = self.indexes.lock().await;
        if let Some((at, idx)) = c.as_ref()
            && at.elapsed() < Duration::from_secs(60)
        {
            return idx.clone();
        }
        let idx = self.a.refresh_indexes().await;
        *c = Some((Instant::now(), idx.clone()));
        idx
    }

    /// That a location's index is here (other devices' downloaded at most
    /// once a minute), in the index database.
    pub async fn has_index(&self, loc: &Location) -> Result<(), String> {
        {
            let mut at = self.synced.lock().await;
            if at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                self.a.sync_indexes().await;
                *at = Some(Instant::now());
            }
        }
        match self.a.index_db.meta(&loc.id).await {
            Ok(Some(_)) => Ok(()),
            _ => Err(format!(
                "{} has no index yet (its device has not walked it)",
                loc.name
            )),
        }
    }

    /// One entry of a location's index: an error says why there is none.
    pub async fn entry_of(&self, loc: &Location, path: &str) -> Result<IndexEntry, String> {
        self.has_index(loc).await?;
        self.a
            .index_entry(&loc.id, path)
            .await
            .ok_or_else(|| format!("{path} is not in {} (as of its last index)", loc.name))
    }

    pub async fn index_of(&self, loc: &Location) -> Result<std::sync::Arc<LocationIndex>, String> {
        self.indexes().await.remove(&loc.id).ok_or_else(|| {
            format!(
                "{} has no index yet (its device has not walked it)",
                loc.name
            )
        })
    }

    /// A thing by how an agent names it: `drive:PATH`, `LOCATION:PATH`,
    /// `project:NAME`, `doc:ID`, `content:HASH`, `file:<tree id>`, or a
    /// file on this computer by its path.
    pub async fn thing(&self, name: &str) -> Result<Thing, String> {
        let name = name.trim();
        if let Some(path) = name.strip_prefix("drive:") {
            let path = path.trim_matches('/');
            // Inside an adopted folder: the location's file (docs/adopt.md).
            let all = self.a.adoptions();
            if let drive_files::adopt::Place::Inside { adoption, rel } =
                drive_files::adopt::place(&all, path)
            {
                let loc = self.location(&adoption.location)?;
                let lp = drive_files::adopt::join(&adoption.sub, &rel);
                let entry = if lp.is_empty() {
                    IndexEntry {
                        path: String::new(),
                        dir: true,
                        size: 0,
                        mtime: 0,
                        sampled: String::new(),
                    }
                } else {
                    self.entry_of(&loc, &lp).await.map_err(|_| {
                        format!("no {path} in the Drive (as of {}'s last index)", loc.name)
                    })?
                };
                return Ok(Thing::Located { loc, entry });
            }
            let entry = tree::resolve(&self.a.files, path)
                .ok_or_else(|| format!("no {path} in the Drive"))?
                .ok_or("the Drive's top folder is not a thing")?;
            return Ok(Thing::Drive {
                path: path.to_string(),
                entry,
            });
        }
        if let Some(p) = name.strip_prefix("project:") {
            return self.project(p).map(Thing::Project);
        }
        if let Some(d) = name.strip_prefix("doc:") {
            return Ok(Thing::Doc(d.to_string()));
        }
        if let Some(h) = name.strip_prefix("content:") {
            return Ok(Thing::Content(h.to_string()));
        }
        if let Some(id) = name.strip_prefix("file:") {
            let tid = tree::parse_id(id).ok_or_else(|| format!("{id} is not a tree id"))?;
            let entry = tree::entry(&self.a.files, tid).ok_or_else(|| format!("no file {id}"))?;
            return Ok(Thing::Drive {
                path: tree::path_of(&self.a.files, tid),
                entry,
            });
        }
        let local = Path::new(name);
        if local.is_absolute() {
            // Never the agent's own folder, the system's or Drive's mounts,
            // however it is spelled: resolved without looking inside them.
            let rules = self.a.browse_rules();
            let local = crate::browse::follow(local, &rules.deny)
                .filter(|p| p.is_file())
                .ok_or_else(|| format!("{name} is not a file on this computer"))?;
            let sampled = drive_files::hash::sampled(&local).map_err(|e| format!("{name}: {e}"))?;
            let size = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
            return Ok(Thing::Local {
                path: local,
                sampled,
                size,
            });
        }
        if let Some((lname, path)) = name.split_once(':')
            && let Ok(loc) = self.location(lname)
        {
            let path = path.trim_matches('/');
            let entry = self.entry_of(&loc, path).await?;
            return Ok(Thing::Located { loc, entry });
        }
        Err(format!(
            "{name}: name a thing as drive:PATH, LOCATION:PATH, project:NAME, doc:ID, content:HASH or an absolute path here"
        ))
    }

    /// The projects whose parts hold a computer's file.
    pub fn projects_holding(&self, location: &str, path: &str) -> Vec<String> {
        projects::projects(&self.a.space)
            .into_iter()
            .filter(|p| {
                p.parts.iter().any(|part| {
                    part.location == location
                        && (part.path.is_empty()
                            || path == part.path
                            || path.starts_with(&format!("{}/", part.path)))
                })
            })
            .map(|p| p.name)
            .collect()
    }

    pub fn commit_tags(&self) {
        self.a.drive.commit(&self.e.tags_id);
    }

    pub fn commit_rules(&self) {
        self.a.drive.commit(&self.e.rules_id);
    }
}

/// Copy the agent's document copies into the session's folder, once.
fn seed(from: &Path, to: &Path) {
    if to.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(to);
    let Ok(rd) = std::fs::read_dir(from) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "loro") {
            let _ = std::fs::copy(&p, to.join(e.file_name()));
        }
    }
}
