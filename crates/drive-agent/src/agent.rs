//! The agent's core, shared by its commands and its file system: this
//! device's config, its cache of blobs, moving blobs to and from storage,
//! and each file's sync state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use drive_files::blob::BlobStore;
use drive_files::space::{self, Device};
use drive_files::storage::StorageConfig;
use drive_files::tree::{self, Entry};
use drive_files::{files_doc, hash, space_doc};
use drive_native::Drive;
use loro::{LoroDoc, TreeID};
use serde::{Deserialize, Serialize};

/// One storage this device runs, with its credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnStorage {
    pub id: String,
    pub name: String,
    pub config: StorageConfig,
}

/// This device's config (in its data folder; never synced).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// The node's sync address: `ws://host:port/sync`.
    pub node: String,
    pub space: String,
    pub device_id: String,
    pub device_name: String,
    #[serde(default)]
    pub storages: Vec<OwnStorage>,
    /// Files kept here always (their tree ids).
    #[serde(default)]
    pub pins: Vec<String>,
    /// The issuer's session for this device (`drive sign-in`): the
    /// node asks for one when it is reached from another computer. Kept
    /// only in this file (mode 600).
    #[serde(default)]
    pub token: Option<String>,
    /// When that session runs out (RFC 3339), and whose it is.
    #[serde(default)]
    pub token_expires: Option<String>,
    #[serde(default)]
    pub signed_in_as: Option<String>,
    /// Whether this device may let go of its copies of Drive files once a
    /// storage holds them (eviction, unpinning). Off unless turned on by
    /// hand: until there are verified second copies, Drive keeps every
    /// byte it has.
    #[serde(default)]
    pub allow_evict: bool,
    /// Our own iroh relay (`https://relay.example.com`) for file transfers
    /// between devices; none: n0's public relays (docs/transfer.md).
    /// `DRIVE_RELAY` overrides it.
    #[serde(default)]
    pub relay: Option<String>,
    /// Whether file rules may move or rename files in this device's
    /// locations (docs/rules.md). Off unless turned on by hand; even on, a
    /// move is a verified copy and the original goes to the location's
    /// `.drive-trash`, never deleted.
    #[serde(default)]
    pub allow_rule_moves: bool,
    /// kache build caches shared with this device's build group
    /// (docs/kache.md): its port, how much is kept here.
    #[serde(default)]
    pub kache: crate::kache::Settings,
    /// Files for apps (docs/app-files.md): the app cache's budget, and a
    /// loopback port beside the socket.
    #[serde(default)]
    pub apps: crate::apps::Settings,
}

impl Config {
    pub fn load(dir: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(dir.join("config.json")).map_err(|_| {
            format!(
                "no config in {}: run `drive init` or `join` first",
                dir.display()
            )
        })?;
        serde_json::from_str(&text).map_err(|e| format!("config: {e}"))
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(self).expect("a config serializes"),
        )
        .map_err(|e| e.to_string())?;
        // Credentials inside: readable by this user only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The node's HTTP address, from its sync address.
    pub fn node_http(&self) -> String {
        self.node
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1)
            .trim_end_matches("/sync")
            .trim_end_matches('/')
            .to_string()
    }
}

/// A file's sync state on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// In the tree; its bytes are not here.
    Cloud,
    Downloading,
    /// Its bytes are here (may be evicted).
    Local,
    /// Always kept here.
    Pinned,
    /// Changed here; going to storage.
    Uploading,
    /// Made on another device; its bytes have not arrived anywhere yet.
    Waiting,
    Folder,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Cloud => "cloud",
            State::Downloading => "downloading",
            State::Local => "local",
            State::Pinned => "pinned",
            State::Uploading => "uploading",
            State::Waiting => "waiting",
            State::Folder => "folder",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            State::Cloud => "☁",
            State::Downloading => "↓",
            State::Local => "✓",
            State::Pinned => "●",
            State::Uploading => "↑",
            State::Waiting => "…",
            State::Folder => " ",
        }
    }
}

pub struct Agent {
    pub dir: PathBuf,
    pub cfg: Mutex<Config>,
    pub drive: Drive,
    pub files: LoroDoc,
    pub space: LoroDoc,
    /// The spine: the kinds apps register (which of them to keep available).
    pub spine: LoroDoc,
    pub files_id: String,
    pub space_id: String,
    pub(crate) stores: Vec<(String, BlobStore)>,
    pub http: reqwest::Client,
    busy: Mutex<HashSet<(String, State)>>,
    /// This device's iroh endpoint serving its files (docs/transfer.md),
    /// once the daemon has started it.
    pub provider: std::sync::OnceLock<drive_transfer::Provider>,
    /// What apps hold pinned here (docs/app-files.md): never evicted.
    pub leases: crate::apps::Leases,
    /// Every location's index, decoded once and shared until its file
    /// changes: a big disk's index is millions of entries, and placement,
    /// the view, the rules and the tags each read every index often.
    pub(crate) indexes: crate::places::IndexCache,
    /// Each other device's location: the published index (its registry
    /// `built`) this device last downloaded, so it is not fetched again.
    pub(crate) fetched: Mutex<std::collections::HashMap<String, i64>>,
}

impl Agent {
    /// Connect, open the space's documents, and wait for the node's copy
    /// (or go on with this device's own after a few seconds: offline).
    pub async fn start(dir: &Path, cfg: Config) -> Agent {
        let author = cfg.device_name.clone();
        let agent = Agent::start_with(dir, cfg, &author, dir.join("state")).await;
        agent.announce();
        agent
    }

    /// [`Agent::start`], with the name every edit records as its author in
    /// the documents' history, its own folder for the documents' copies,
    /// and without saying this device is here (the MCP server: an AI agent
    /// working through this device is not its agent).
    pub async fn start_with(dir: &Path, cfg: Config, author: &str, state: PathBuf) -> Agent {
        let drive = Drive::connect_with(&cfg.node, author, state, cfg.token.clone()).await;
        let files_id = files_doc(&cfg.space);
        let space_id = space_doc(&cfg.space);
        let files = drive.open(&files_id).await;
        let space = drive.open(&space_id).await;
        let spine = drive.open(&drive_spine::spine_doc(&cfg.space)).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            drive.synced(&files_id).await;
            drive.synced(&space_id).await;
        })
        .await;
        let stores = cfg
            .storages
            .iter()
            .filter_map(|s| s.config.blobs().ok().map(|b| (s.id.clone(), b)))
            .collect();
        let _ = std::fs::create_dir_all(dir.join("cache"));
        // Every HTTP call to the node carries this device's session, as the
        // sync connection does: a node reached from another computer refuses
        // uploads (an index, a blob) without it.
        let http = http_client(cfg.token.as_deref());
        Agent {
            dir: dir.to_path_buf(),
            cfg: Mutex::new(cfg),
            drive,
            files,
            space,
            spine,
            files_id,
            space_id,
            stores,
            http,
            busy: Mutex::new(HashSet::new()),
            provider: std::sync::OnceLock::new(),
            leases: Default::default(),
            indexes: Default::default(),
            fetched: Default::default(),
        }
    }

    /// Say this device is here (others see it online).
    pub fn announce(&self) {
        let cfg = self.cfg.lock().expect("cfg").clone();
        self.drive.set_presence(
            &self.space_id,
            serde_json::json!({ "hue": 175, "data": { "device": cfg.device_id, "agent": true } })
                .to_string(),
        );
    }

    pub fn device_id(&self) -> String {
        self.cfg.lock().expect("cfg").device_id.clone()
    }

    pub fn commit_files(&self) {
        self.drive.commit(&self.files_id);
    }

    pub fn commit_space(&self) {
        self.drive.commit(&self.space_id);
    }

    pub fn cache_path(&self, hash: &str) -> PathBuf {
        self.dir.join("cache").join(hash)
    }

    pub fn is_cached(&self, hash: &str) -> bool {
        !hash.is_empty() && self.cache_path(hash).exists()
    }

    /// Pinned here: read from the space's registry (so the mount, the
    /// command line and every app agree), or this device's own config.
    pub fn is_pinned(&self, e: &Entry) -> bool {
        let me = self.device_id();
        (!e.hash.is_empty()
            && space::holders(&self.space, &e.hash)
                .iter()
                .any(|(who, how)| *who == me && how == "pinned"))
            || space::pins(&self.space, &me).contains(&tree::id_string(e.id))
            || self
                .cfg
                .lock()
                .expect("cfg")
                .pins
                .contains(&tree::id_string(e.id))
    }

    fn mark(&self, key: &str, state: State, on: bool) {
        let mut b = self.busy.lock().expect("busy");
        if on {
            b.insert((key.to_string(), state));
        } else {
            b.remove(&(key.to_string(), state));
        }
    }

    fn is_busy(&self, key: &str, state: State) -> bool {
        self.busy
            .lock()
            .expect("busy")
            .contains(&(key.to_string(), state))
    }

    /// A file's state on this device.
    pub fn state(&self, e: &Entry) -> State {
        if e.dir {
            return State::Folder;
        }
        let id = tree::id_string(e.id);
        if self.is_busy(&id, State::Uploading) {
            return State::Uploading;
        }
        if e.hash.is_empty() {
            return State::Waiting;
        }
        if self.is_busy(&e.hash, State::Downloading) {
            return State::Downloading;
        }
        match (self.is_cached(&e.hash), self.is_pinned(e)) {
            (true, true) => State::Pinned,
            (true, false) => State::Local,
            (false, _) => State::Cloud,
        }
    }

    /// Who holds a file's bytes, by name (devices and storages).
    pub fn holders(&self, hash: &str) -> Vec<String> {
        let devices = space::devices(&self.space);
        let storages = space::storages(&self.space);
        space::holders(&self.space, hash)
            .into_iter()
            .map(|(who, how)| {
                let name = if who == "node" {
                    "the node".to_string()
                } else if let Some(d) = devices.iter().find(|d| d.id == who) {
                    d.name.clone()
                } else if let Some(s) = storages.iter().find(|s| s.id == who) {
                    s.name.clone()
                } else {
                    who
                };
                format!("{name} ({how})")
            })
            .collect()
    }

    /// A blob's bytes on this device's disk, fetching them if needed: from
    /// this device's own storages first, then the node. Checked against the
    /// hash either way.
    pub async fn fetch(&self, hash: &str) -> Result<PathBuf, String> {
        let path = self.cache_path(hash);
        if path.exists() {
            return Ok(path);
        }
        self.mark(hash, State::Downloading, true);
        let result = self.fetch_bytes(hash).await;
        self.mark(hash, State::Downloading, false);
        let bytes = result?;
        let tmp = path.with_extension("part");
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| e.to_string())?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| e.to_string())?;
        let _ = space::set_held(&self.space, &self.device_id(), hash, Some("local"));
        self.commit_space();
        Ok(path)
    }

    pub async fn fetch_bytes(&self, hash: &str) -> Result<Vec<u8>, String> {
        for (_, store) in &self.stores {
            if let Ok(b) = store.get(hash).await {
                return Ok(b);
            }
        }
        let url = format!("{}/blobs/{hash}", self.cfg.lock().expect("cfg").node_http());
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("no storage has blob {hash} ({})", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
        if hash::of_bytes(&bytes) != hash {
            return Err(format!("blob {hash} came back changed"));
        }
        Ok(bytes)
    }

    /// Keep a file's new bytes: into the cache, then every storage this
    /// device runs and the node. Returns the hash and size; the caller
    /// then commits the tree (the upload's commit point).
    pub async fn upload(&self, file_id: TreeID, src: &Path) -> Result<(String, u64), String> {
        let id = tree::id_string(file_id);
        self.mark(&id, State::Uploading, true);
        let result = self.upload_inner(src).await;
        self.mark(&id, State::Uploading, false);
        result
    }

    async fn upload_inner(&self, src: &Path) -> Result<(String, u64), String> {
        let h = hash::of_file(src).map_err(|e| e.to_string())?;
        let bytes = tokio::fs::read(src).await.map_err(|e| e.to_string())?;
        let size = bytes.len() as u64;
        let cached = self.cache_path(&h);
        if !cached.exists() {
            tokio::fs::write(&cached, &bytes)
                .await
                .map_err(|e| e.to_string())?;
        }
        let me = self.device_id();
        let _ = space::set_held(&self.space, &me, &h, Some("local"));
        for (sid, store) in &self.stores {
            match store.put_checked(&h, bytes.clone()).await {
                Ok(()) => {
                    let _ = space::set_held(&self.space, sid, &h, Some("stored"));
                }
                Err(e) => tracing::warn!(storage.id = %sid, error = %e, "a storage refused a blob"),
            }
        }
        let url = format!("{}/blobs/{h}", self.cfg.lock().expect("cfg").node_http());
        match self.http.put(&url).body(bytes).send().await {
            Ok(r) if r.status().is_success() => {
                let _ = space::set_held(&self.space, "node", &h, Some("stored"));
            }
            Ok(r) => tracing::warn!(status = %r.status(), "the node refused a blob"),
            Err(e) => tracing::warn!(error = %e, "the node is unreachable; the blob stays here"),
        }
        self.commit_space();
        Ok((h, size))
    }

    /// Keep a file here always (and fetch it now), or let it go.
    pub async fn pin(&self, e: &Entry, on: bool) -> Result<(), String> {
        let id = tree::id_string(e.id);
        {
            let mut cfg = self.cfg.lock().expect("cfg");
            cfg.pins.retain(|p| *p != id);
            if on {
                cfg.pins.push(id);
            }
            cfg.save(&self.dir)?;
        }
        if e.hash.is_empty() {
            return Ok(());
        }
        let me = self.device_id();
        if on {
            self.fetch(&e.hash).await?;
            let _ = space::set_held(&self.space, &me, &e.hash, Some("pinned"));
        } else {
            // Unpinned and evicted: back to the cloud (if a storage has it).
            let stored = space::holders(&self.space, &e.hash)
                .iter()
                .any(|(_, how)| how == "stored");
            let allowed = self.cfg.lock().expect("cfg").allow_evict;
            if stored && allowed {
                let _ = std::fs::remove_file(self.cache_path(&e.hash));
                let _ = space::set_held(&self.space, &me, &e.hash, None);
            }
        }
        self.commit_space();
        Ok(())
    }

    /// Add a file from this computer into the space.
    pub async fn put(
        &self,
        src: &Path,
        parent: Option<TreeID>,
        name: &str,
    ) -> Result<Entry, String> {
        let id = match tree::lookup(&self.files, parent, name) {
            Some(e) if !e.dir => e.id,
            Some(_) => return Err(format!("{name} is a folder")),
            None => tree::create_file(&self.files, parent, name).map_err(|e| e.to_string())?,
        };
        let (h, size) = self.upload(id, src).await?;
        tree::set_content(&self.files, id, size, &h).map_err(|e| e.to_string())?;
        self.commit_files();
        tree::entry(&self.files, id).ok_or_else(|| "the file vanished".to_string())
    }
}

/// This device's description, for the registry.
pub fn this_device(id: &str, name: &str) -> Device {
    Device {
        id: id.to_string(),
        name: name.to_string(),
        kind: "computer".into(),
        os: std::env::consts::OS.into(),
        roles: vec!["client".into()],
        approved_by: String::new(),
        added: drive_files::now_ms(),
    }
}

/// The agent's HTTP client: its session token, if it has one, on every
/// request.
fn http_client(token: Option<&str>) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(t) = token.filter(|t| !t.is_empty())
        && let Ok(mut v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
    {
        v.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap_or_default()
}
