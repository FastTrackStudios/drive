//! `drive`: this device in a Drive space.
//!
//! ```text
//! drive [--dir DIR] init  --node ws://host:8096/sync --space home --name "Studio PC"
//! drive [--dir DIR] invite                       # a pairing code for another device
//! drive [--dir DIR] sign-in | sign-out             # this device signs in as its person (the node's issuer)
//! drive [--dir DIR] sign-in --node URL --space NAME  # before joining a node that asks for a sign-in
//! drive [--dir DIR] join  --node URL --space home --name Laptop --code K7Q2XM
//! drive [--dir DIR] devices | storages
//! drive [--dir DIR] device rm NAME                # forget a device, its locations and storages
//! drive [--dir DIR] storage add fs     NAME PATH
//! drive [--dir DIR] storage add s3     NAME BUCKET ENDPOINT KEY SECRET [REGION]
//! drive [--dir DIR] storage add webdav NAME ENDPOINT [USER PASSWORD]
//! drive [--dir DIR] put LOCAL_FILE_OR_FOLDER [FOLDER]
//! drive [--dir DIR] ls [FOLDER] | status PATH | pin PATH | unpin PATH
//! drive [--dir DIR] mount MOUNTPOINT [--no-devices] [--computers PATH]
//!                                   # the space as a folder, every device under
//!                                   # MOUNTPOINT/Devices (its locations and disks;
//!                                   # --no-devices leaves it out), and the devices
//!                                   # alone at PATH too (runs until stopped)
//! drive [--dir DIR] serve     # no mount: index, answer the node, watch
//!
//! Every computer's files (indexed in place, searchable while it is off):
//! drive [--dir DIR] location add NAME PATH        # a folder on this device
//! drive [--dir DIR] location pause|resume NAME    # keep its last index, stop walking it
//! drive [--dir DIR] location rm NAME_OR_ID [--force]   # stop tracking it (alias remove);
//!                                   # its folder and files stay; refused while a
//!                                   # folder is adopted from it, and for another
//!                                   # device's location without --force
//! drive [--dir DIR] location add NAME PATH --storage STORAGE   # in a storage it runs
//! drive [--dir DIR] locations | volumes | index [NAME]
//! drive [--dir DIR] search WORDS…                 # every device's files
//! drive [--dir DIR] copies LOCATION PATH          # where else a file is
//! drive [--dir DIR] where DRIVE/PATH              # its device and real path, from this
//!                                   # computer's copies alone: works with Drive down
//! drive [--dir DIR] where --find WORDS…           # by name: the tree and every index
//!
//! Adopting a folder in place (docs/adopt.md; nothing is copied or moved):
//! drive [--dir DIR] adopt FOLDER --as LOGICAL/PATH # FOLDER shows at MOUNT/LOGICAL/PATH
//! drive [--dir DIR] adopt --list                  # every adopted folder
//! drive [--dir DIR] unadopt LOGICAL/PATH          # the mapping only; the files stay
//!
//! Working copies (docs/working-copies.md): the Drive's own files, kept in a
//! folder on one device too and synced both ways:
//! drive [--dir DIR] own LOGICAL/PATH | own --all  # an adopted folder becomes the
//!                                   # Drive's own; its folder here, its working copy
//! drive [--dir DIR] working                       # every working copy
//!
//! Tags and fields (docs/tags.md; THING: a file here, drive:PATH, LOCATION:PATH,
//! project:NAME, doc:ID):
//! drive [--dir DIR] tag ls [THING] | add THING TAG… | rm THING TAG…
//! drive [--dir DIR] tag new NAME [--parent TAG]… [--color C] [--shorthand S] [--alias A]… [--category] [--hidden]
//! drive [--dir DIR] tag field THING NAME VALUE [--kind text|multiline|date|datetime]
//! drive [--dir DIR] tag search QUERY…              # drums NOT archived, path:*.wav, kind:audio …
//!
//! File rules (docs/rules.md; every rule starts off and as a dry run):
//! drive [--dir DIR] rules ls | show NAME | journal [NAME] | undo ID
//! drive [--dir DIR] rules run NAME [--live]       # here now, and asked of every device
//! drive [--dir DIR] rules add FILE.json | on|off NAME | live|dry-run NAME | rm NAME
//!
//! What this device keeps:
//! drive [--dir DIR] role storage-server|workstation   # keep everything, or the work at hand
//! drive [--dir DIR] active [add|rm PATH]          # folders this device is working on
//! drive [--dir DIR] place                         # one placement pass now
//! drive [--dir DIR] storage public STORAGE URL|off # files kept available go there (R2), links point at URL
//! drive [--dir DIR] available                     # put what should be available on the node now
//!
//! Protection (3-2-1 and the like):
//! drive [--dir DIR] site DEVICE_OR_STORAGE SITE   # where it physically is
//! drive [--dir DIR] policy add NAME [--copies 3 --media 2 --offsite 1] [--scope LOC,LOC]
//! drive [--dir DIR] protection                    # every policy, checked
//! drive [--dir DIR] backup LOCATION STORAGE       # copy a location into a storage
//!
//! Static sites (built here with dodeca's `ddc`, served by the node under
//! /sites/<space>/<id>/, and copied to a public storage if they have one):
//! drive [--dir DIR] site theme-init [FOLDER]      # the starter theme into the space (Sites/Themes/Base)
//! drive [--dir DIR] site create TITLE --source FOLDER [--id ID] [--theme FOLDER]
//!                         [--overrides FOLDER] [--storage STORAGE] [--base-url URL] [--manual]
//! drive [--dir DIR] site ls | show ID | rm ID
//! drive [--dir DIR] site build ID [--force]       # build and publish now
//! ```
//!
//! kache build caches shared with this device's build group (docs/kache.md):
//! drive [--dir DIR] kache group NAME|auto|off     # e.g. linux-x86_64, darwin-arm64
//! drive [--dir DIR] kache status | config         # the group, what is kept; kache's [cache.remote]
//! drive [--dir DIR] kache budget GIB | port PORT
//!
//! Files for apps (docs/app-files.md; the running agent serves them):
//! drive [--dir DIR] open TARGET [--hash]          # a real local path for any Drive file
//! drive [--dir DIR] warm TARGET… [--project NAME|ORG/NAME] [--kind audio]… [--priority low|normal|high]
//! drive [--dir DIR] pins                          # what apps hold pinned here, and why
//! drive [--dir DIR] status                        # the app cache and its pins
//! drive [--dir DIR] apps budget GIB               # the app cache's size
//!
//! AI agents (docs/agents.md): an MCP server on stdio, as this device:
//! drive [--dir DIR] mcp [--as NAME] [--max-writes N] [--page-max N] [--timeout SECONDS]
//!
//! Sidecars (thumbnails, waveform peaks; made here from this device's files):
//! drive [--dir DIR] sidecars [--force] [--kind thumb|peaks] [--max N]
//!                                   # make what the node lacks now (`serve` does it in the background)
//!
//! What this command line prints is its output, on stdout; the mount's
//! diagnostics go through `tracing` to stderr.

mod adopt;
#[cfg(feature = "mount")]
mod adopt_fs;
mod agent;
mod apps;
mod available;
mod browse;
mod daemon;
#[cfg(feature = "mount")]
mod devices;
#[cfg(feature = "mount")]
mod fs;
mod fileprovider;
// Written by every index; read as the agent moves onto it (drive#47).
#[allow(dead_code)]
mod index_db;
mod locate;
mod kache;
mod location_rm;
mod mcp;
#[cfg(feature = "mount")]
mod one;
mod placement;
mod places;
mod rules;
mod rules_fs;
mod sidecars;
mod sites;
mod synced;
mod tagging;
mod telemetry;
mod transfer;
mod view;
mod working;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent::{Agent, Config, OwnStorage, this_device};
use drive_files::space;
use drive_files::storage::StorageConfig;
use drive_files::tree;

fn out(line: impl AsRef<str>) {
    let _ = writeln!(std::io::stdout(), "{}", line.as_ref());
}

fn resolve_dir(
    cli: Option<PathBuf>,
    env_dir: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(d) = cli {
        return d;
    }
    if let Some(d) = env_dir.filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    let base = xdg
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("drive-agent")
}

fn default_dir() -> PathBuf {
    resolve_dir(
        None,
        std::env::var_os("DRIVE_AGENT_DIR"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    let _telemetry = telemetry::init("drive-agent");
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dir = match args.iter().position(|a| a == "--dir") {
        Some(i) if i + 1 < args.len() => {
            let d = PathBuf::from(args.remove(i + 1));
            args.remove(i);
            d
        }
        _ => default_dir(),
    };
    if let Err(e) = run(&dir, &args).await {
        let _ = writeln!(std::io::stderr(), "drive: {e}");
        std::process::exit(1);
    }
}

async fn run(dir: &Path, args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "init" => {
            let node = flag(args, "--node").ok_or("--node ws://host:port/sync")?;
            let space = flag(args, "--space").ok_or("--space NAME")?;
            let name = flag(args, "--name").unwrap_or_else(|| "This computer".into());
            let cfg = Config {
                node,
                space,
                device_id: space::new_id(),
                device_name: name.clone(),
                storages: Vec::new(),
                pins: Vec::new(),
                token: None,
                token_expires: None,
                signed_in_as: None,
                allow_evict: false,
                relay: None,
                allow_rule_moves: false,
                kache: Default::default(),
                apps: Default::default(),
            };
            cfg.save(dir)?;
            let a = Agent::start(dir, cfg.clone()).await;
            let mut d = this_device(&cfg.device_id, &name);
            d.approved_by = "created the space".into();
            space::add_device(&a.space, &d).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "Created space {} with this device, {name} ({}).",
                cfg.space, cfg.device_id
            ));
        }
        "sign-in" => {
            // The node's issuer, from the node itself.
            // Before joining (a node that asks for a sign-in first), the
            // node and space come as flags and the config starts here.
            let mut cfg = match (Config::load(dir), flag(args, "--node"), flag(args, "--space")) {
                (Ok(c), _, _) => c,
                (Err(_), Some(node), Some(space)) => Config {
                    node,
                    space,
                    device_id: space::new_id(),
                    device_name: flag(args, "--name").unwrap_or_else(|| "Another computer".into()),
                    storages: Vec::new(),
                    pins: Vec::new(),
                    token: None,
                    token_expires: None,
                    signed_in_as: None,
                    allow_evict: false,
                    relay: None,
                    allow_rule_moves: false,
                    kache: Default::default(),
                    apps: Default::default(),
                },
                (Err(e), _, _) => return Err(format!("{e} (before joining: sign-in --node URL --space NAME)")),
            };
            let http = reqwest::Client::new();
            let orgs: serde_json::Value = http
                .get(format!("{}/orgs", cfg.node_http()))
                .bearer_auth(cfg.token.clone().unwrap_or_default())
                .send()
                .await
                .map_err(|e| format!("the node: {e}"))?
                .json()
                .await
                .map_err(|e| format!("the node: {e}"))?;
            let Some(issuer) = orgs["auth"]["issuer"].as_str().map(str::to_string) else {
                out("This node asks for no sign-in.");
                return Ok(());
            };
            let client_id = orgs["auth"]["client_id"].as_str().unwrap_or("drive").to_string();
            // The device-code flow: the person approves this device in a browser.
            let code: serde_json::Value = http
                .post(format!("{issuer}/auth/device/code"))
                .json(&serde_json::json!({ "client_id": client_id, "scope": "openid email profile" }))
                .send()
                .await
                .map_err(|e| format!("the issuer: {e}"))?
                .json()
                .await
                .map_err(|e| format!("the issuer: {e}"))?;
            let device_code = code["device_code"].as_str().ok_or("the issuer sent no device code")?.to_string();
            let link = code["verification_uri_complete"].as_str().unwrap_or("/auth/device");
            let link = if link.starts_with("http") { link.to_string() } else { format!("{issuer}{link}") };
            out(format!(
                "To sign this device ({}) in, open {link} and approve code {} (valid {} minutes).",
                cfg.device_name,
                code["user_code"].as_str().unwrap_or("?"),
                code["expires_in_seconds"].as_i64().unwrap_or(600) / 60
            ));
            let every = std::time::Duration::from_secs(code["interval_seconds"].as_u64().unwrap_or(5).max(1));
            let until = std::time::Instant::now() + std::time::Duration::from_secs(code["expires_in_seconds"].as_u64().unwrap_or(600));
            loop {
                if std::time::Instant::now() > until {
                    return Err("the code ran out before it was approved; run sign-in again".into());
                }
                tokio::time::sleep(every).await;
                let r = http
                    .post(format!("{issuer}/auth/device/token"))
                    .json(&serde_json::json!({ "device_code": device_code }))
                    .send()
                    .await;
                let Ok(r) = r else { continue };
                if !r.status().is_success() {
                    continue;
                }
                let bundle: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
                let token = bundle["token"].as_str().ok_or("the issuer sent no session")?.to_string();
                cfg.token = Some(token);
                cfg.token_expires = bundle["session"]["expires_at"].as_str().map(str::to_string);
                cfg.signed_in_as = bundle["user"]["email"].as_str().or(bundle["user"]["name"].as_str()).map(str::to_string);
                cfg.save(dir)?;
                out(format!(
                    "Signed in as {} until {}. Restart the agent (or it picks the session up on its next start).",
                    cfg.signed_in_as.clone().unwrap_or_default(),
                    cfg.token_expires.clone().unwrap_or_default()
                ));
                return Ok(());
            }
        }
        "sign-out" => {
            let mut cfg = Config::load(dir)?;
            cfg.token = None;
            cfg.token_expires = None;
            cfg.signed_in_as = None;
            cfg.save(dir)?;
            out("Signed out: this device's session is gone from its config.");
        }
        "invite" => {
            let a = start(dir).await?;
            let code = space::invite(&a.space, &a.device_id()).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            let cfg = a.cfg.lock().expect("cfg").clone();
            out(format!("Pairing code: {code}   (valid ten minutes)"));
            out(format!(
                "On the other device: drive join --node {} --space {} --name NAME --code {code}",
                cfg.node, cfg.space
            ));
        }
        "join" => {
            let node = flag(args, "--node").ok_or("--node ws://host:port/sync")?;
            let space_name = flag(args, "--space").ok_or("--space NAME")?;
            let name = flag(args, "--name").unwrap_or_else(|| "Another computer".into());
            let code = flag(args, "--code").ok_or("--code CODE")?;
            // A node that asks for a sign-in: `sign-in` first (it writes a
            // config with this node and the session), then `join` keeps it.
            let prior = Config::load(dir).ok().filter(|c| c.node == node);
            let cfg = Config {
                node,
                space: space_name,
                device_id: prior.as_ref().map_or_else(space::new_id, |c| c.device_id.clone()),
                device_name: name.clone(),
                storages: Vec::new(),
                pins: Vec::new(),
                token: prior.as_ref().and_then(|c| c.token.clone()),
                token_expires: prior.as_ref().and_then(|c| c.token_expires.clone()),
                signed_in_as: prior.as_ref().and_then(|c| c.signed_in_as.clone()),
                allow_evict: false,
                relay: prior.as_ref().and_then(|c| c.relay.clone()),
                allow_rule_moves: false,
                kache: Default::default(),
                apps: Default::default(),
            };
            let a = Agent::start(dir, cfg.clone()).await;
            let d = space::join(&a.space, &code, this_device(&cfg.device_id, &name))
                .map_err(|e| format!("that code did not work: {e:?}"))?;
            a.commit_space();
            cfg.save(dir)?;
            settle(&a).await;
            let by = space::devices(&a.space)
                .into_iter()
                .find(|x| x.id == d.approved_by)
                .map_or(d.approved_by.clone(), |x| x.name);
            out(format!("Joined {} as {name}, approved by {by}.", cfg.space));
        }
        "device" if args.get(1).map(String::as_str) == Some("rm") => {
            let who = args.get(2).ok_or("device rm NAME")?.clone();
            let a = start(dir).await?;
            let d = space::devices(&a.space)
                .into_iter()
                .find(|d| d.name == who || d.id == who)
                .ok_or(format!("no device {who}"))?;
            if d.id == a.device_id() {
                return Err("that is this device; remove it from another one".into());
            }
            let space_name = a.cfg.lock().expect("cfg").space.clone();
            let node = a.cfg.lock().expect("cfg").node_http();
            let storages: Vec<_> = space::storages(&a.space).into_iter().filter(|s| s.device == d.id).collect();
            let locations: Vec<_> = space::locations(&a.space)
                .into_iter()
                .filter(|l| l.device == d.id || storages.iter().any(|s| s.id == l.storage))
                .collect();
            for l in &locations {
                let _ = a.http.delete(format!("{node}/index/{space_name}/{}", l.id)).send().await;
                space::remove_location(&a.space, &l.id).map_err(|e| e.to_string())?;
            }
            for s in &storages {
                space::remove_storage(&a.space, &s.id).map_err(|e| e.to_string())?;
            }
            space::remove_device(&a.space, &d.id).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "Removed {} ({} locations, {} storages). Its files stay where they are; this space no longer knows it.",
                d.name,
                locations.len(),
                storages.len()
            ));
        }
        "devices" => {
            let a = start(dir).await?;
            let me = a.device_id();
            for d in space::devices(&a.space) {
                let you = if d.id == me { "  ← this device" } else { "" };
                out(format!(
                    "{:<20} {:<9} {:<8} {}{you}",
                    d.name, d.kind, d.os, d.id
                ));
            }
        }
        "storages" => {
            let a = start(dir).await?;
            let devices = space::devices(&a.space);
            out("the node             node     always");
            for s in space::storages(&a.space) {
                let by = devices
                    .iter()
                    .find(|d| d.id == s.device)
                    .map_or("?".into(), |d| d.name.clone());
                out(format!(
                    "{:<20} {:<8} {} (run by {by})",
                    s.name, s.scheme, s.location
                ));
            }
        }
        "storage" if args.get(1).map(String::as_str) == Some("add") => {
            let kind = args.get(2).ok_or("storage add fs|s3|webdav NAME …")?;
            let name = args.get(3).ok_or("a name")?.clone();
            let get = |i: usize| args.get(i).cloned().unwrap_or_default();
            let config = match kind.as_str() {
                "fs" => {
                    let root =
                        std::fs::canonicalize(get(4)).map_err(|e| format!("{}: {e}", get(4)))?;
                    StorageConfig::Fs {
                        root: root.to_string_lossy().into_owned(),
                    }
                }
                "s3" => StorageConfig::S3 {
                    bucket: get(4),
                    endpoint: get(5),
                    access_key_id: get(6),
                    secret_access_key: get(7),
                    region: get(8),
                    root: "drive".into(),
                },
                "webdav" => StorageConfig::Webdav {
                    endpoint: get(4),
                    username: get(5),
                    password: get(6),
                    root: "drive".into(),
                },
                other => return Err(format!("unknown storage kind {other}")),
            };
            // Check it works before registering it.
            let blobs = config.blobs().map_err(|e| e.to_string())?;
            blobs
                .put(b"drive".to_vec())
                .await
                .map_err(|e| format!("the storage did not take a test blob: {e}"))?;
            let mut cfg = Config::load(dir)?;
            let id = space::new_id();
            cfg.storages.push(OwnStorage {
                id: id.clone(),
                name: name.clone(),
                config: config.clone(),
            });
            cfg.save(dir)?;
            let a = Agent::start(dir, cfg.clone()).await;
            space::add_storage(
                &a.space,
                &space::Storage {
                    id,
                    name: name.clone(),
                    scheme: config.scheme().into(),
                    location: config.public(),
                    device: cfg.device_id.clone(),
                    added: drive_files::now_ms(),
                },
            )
            .map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "Added {name} ({}: {}). Its credentials stay on this device.",
                config.scheme(),
                config.public()
            ));
        }
        "put" => {
            let src = PathBuf::from(args.get(1).ok_or("put LOCAL_FILE [FOLDER]")?);
            let a = start(dir).await?;
            let folder = match args.get(2).filter(|_| !src.is_dir()) {
                Some(p) => tree::resolve(&a.files, p)
                    .ok_or(format!("no folder {p}"))?
                    .map(|e| e.id),
                None => None,
            };
            let name = src
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("a file name")?
                .to_string();
            if src.is_dir() {
                // A folder: everything in it, folders made as needed
                // (hidden files included: an Obsidian vault's .obsidian/).
                let base = match args.get(2) {
                    Some(p) => format!("{}/{name}", p.trim_matches('/')),
                    None => name.clone(),
                };
                let mut stack = vec![(src.clone(), base)];
                let mut n = 0;
                while let Some((local, at)) = stack.pop() {
                    let parent = a.ensure_folder(&at)?;
                    let entries = std::fs::read_dir(&local).map_err(|e| format!("{}: {e}", local.display()))?;
                    for entry in entries.flatten() {
                        let p = entry.path();
                        let file = entry.file_name().to_string_lossy().into_owned();
                        if p.is_dir() {
                            stack.push((p, format!("{at}/{file}")));
                        } else if p.is_file() {
                            a.put(&p, parent, &file).await?;
                            n += 1;
                        }
                    }
                }
                a.commit_files();
                settle(&a).await;
                out(format!("{n} files put under /{}", args.get(2).map_or(name.clone(), |p| format!("{}/{name}", p.trim_matches('/')))));
                return Ok(());
            }
            let e = a.put(&src, folder, &name).await?;
            settle(&a).await;
            out(format!(
                "{} · {} bytes · {}",
                tree::path_of(&a.files, e.id),
                e.size,
                &e.hash[..12]
            ));
        }
        "ls" => {
            let a = start(dir).await?;
            let p = args.get(1).map_or("", String::as_str).trim_matches('/');
            // Adopted folders show where the mount shows them (docs/adopt.md).
            let all = a.adoptions();
            if let drive_files::adopt::Place::Inside { adoption, .. } =
                drive_files::adopt::place(&all, p)
            {
                out(format!(
                    "# adopted at /{}: on {}",
                    adoption.at,
                    a.whereabouts(adoption)
                ));
                for (name, dir) in a.logical_list(p).await? {
                    out(format!(
                        "  {:<11} {:>9}  {name}{}",
                        "adopted",
                        "",
                        if dir { "/" } else { "" }
                    ));
                }
                return Ok(());
            }
            let added = drive_files::adopt::names_in(&all, p);
            let folder = match tree::resolve(&a.files, p) {
                Some(e) => e.map(|e| e.id),
                None if !added.is_empty() => None,
                None => return Err(format!("no folder {p}")),
            };
            let in_tree = p.is_empty() || folder.is_some();
            let mut rows = if in_tree {
                tree::children(&a.files, folder)
            } else {
                Vec::new()
            };
            // An adopted folder hides a tree entry of the same name.
            rows.retain(|e| !added.iter().any(|(n, point)| *point && *n == e.name));
            for (name, point) in &added {
                if rows.iter().any(|e| e.dir && e.name == *name) {
                    continue;
                }
                let at = drive_files::adopt::join(p, name);
                let note = match all.iter().find(|x| *point && x.at == at) {
                    Some(x) => format!("  (on {})", a.whereabouts(x)),
                    None => String::new(),
                };
                out(format!("  {:<11} {:>9}  {name}/{note}", "adopted", ""));
            }
            for e in rows {
                let st = a.state(&e);
                let size = if e.dir { "".into() } else { human(e.size) };
                out(format!(
                    "{} {:<11} {:>9}  {}{}",
                    st.glyph(),
                    st.as_str(),
                    size,
                    e.name,
                    if e.dir { "/" } else { "" }
                ));
            }
        }
        "status" if args.len() < 2 => app_status(dir)?,
        "open" | "warm" | "pins" | "apps" => app_command(dir, args)?,
        "status" => {
            let a = start(dir).await?;
            let p = args.get(1).ok_or("status PATH")?;
            let e = tree::resolve(&a.files, p)
                .ok_or(format!("no {p}"))?
                .ok_or("the top folder")?;
            out(format!(
                "{}  {}",
                a.state(&e).as_str(),
                tree::path_of(&a.files, e.id)
            ));
            if !e.hash.is_empty() {
                out(format!("  {} · blake3 {}", human(e.size), e.hash));
                for h in a.holders(&e.hash) {
                    out(format!("  held by {h}"));
                }
            }
        }
        "pin" | "unpin" => {
            let a = start(dir).await?;
            let p = args.get(1).ok_or("pin PATH")?;
            let e = tree::resolve(&a.files, p)
                .ok_or(format!("no {p}"))?
                .ok_or("the top folder")?;
            a.pin(&e, cmd == "pin").await?;
            settle(&a).await;
            out(format!("{}  {}", a.state(&e).as_str(), p));
        }
        #[cfg(not(feature = "mount"))]
        "mount" => return Err("this build has no mount (built without the `mount` feature); `serve` runs everything else".into()),
        #[cfg(feature = "mount")]
        "mount" => {
            let at = PathBuf::from(args.get(1).ok_or("mount MOUNTPOINT")?);
            let at = std::path::absolute(&at).map_err(|e| e.to_string())?;
            let folder = !args.iter().any(|x| x == "--no-devices");
            let alone = flag(args, "--computers")
                .map(|p| std::path::absolute(p).map_err(|e| e.to_string()))
                .transpose()?;
            let a = Arc::new(start(dir).await?);
            let view = Arc::new(RwLock::new(devices::View::default()));
            let space = a.cfg.lock().expect("cfg").space.clone();
            let rt = tokio::runtime::Handle::current();
            // Neither the devices' disks nor browsing from elsewhere ever
            // walks into Drive's own mounts.
            let guard: Vec<PathBuf> = std::iter::once(at.clone()).chain(alone.clone()).collect();
            browse::never_browse(&guard);
            let main = mount_fs(&at, &format!("drive:{space}"), || {
                let devices = folder.then(|| {
                    devices::DevicesFs::new(
                        a.clone(),
                        view.clone(),
                        rt.clone(),
                        one::DEVICES_BASE,
                        guard.clone(),
                    )
                });
                // Adopted folders show at their logical paths (adopt_fs).
                let one = one::OneFs::new(fs::DriveFs::new(a.clone(), rt.clone()), devices);
                adopt_fs::AdoptFs::new(one, a.clone(), rt.clone())
            })
            .await?;
            let others = match &alone {
                Some(p) => Some(
                    mount_fs(p, &format!("drive-devices:{space}"), || {
                        devices::DevicesFs::new(a.clone(), view.clone(), rt.clone(), 0, guard.clone())
                    })
                    .await?,
                ),
                None => None,
            };
            tracing::info!(mount.at = %at.display(), space.name = %space, "mounted");
            run_until_stopped(daemon::serve(a.clone(), view)).await?;
            drop(others);
            drop(main);
            tracing::info!(mount.at = %at.display(), "unmounted");
        }
        "sidecars" => {
            let a = start(dir).await?;
            let force = args.iter().any(|x| x == "--force");
            let only = flag(args, "--kind")
                .map(|k| drive_sidecars::Kind::of_family(&k))
                .unwrap_or_default();
            if flag(args, "--kind").is_some() && only.is_empty() {
                return Err("--kind takes thumb, peaks, thumb-256 or thumb-1024".into());
            }
            let max = flag(args, "--max")
                .and_then(|n| n.parse().ok())
                .unwrap_or(usize::MAX);
            let p = a.backfill_sidecars(&only, force, max).await;
            out(format!(
                "{} files read ({}): {} sidecars made, {} with no generator, {} failed",
                p.files,
                human(p.bytes),
                p.made,
                p.no_generator,
                p.failed
            ));
        }
        "serve" => {
            let a = Arc::new(start(dir).await?);
            let view = Arc::new(RwLock::new(view::View::default()));
            run_until_stopped(daemon::serve(a, view)).await?;
        }
        "location"
            if matches!(args.get(1).map(String::as_str), Some("pause" | "resume")) =>
        {
            let pause = args.get(1).map(String::as_str) == Some("pause");
            let who = args.get(2).ok_or("location pause|resume NAME")?.clone();
            let a = start(dir).await?;
            let l = space::locations(&a.space)
                .into_iter()
                .find(|l| l.name == who || l.id == who)
                .ok_or(format!("no location {who}"))?;
            space::set_paused(&a.space, &l.id, pause).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(if pause {
                format!("{} is paused: its last index stays searchable; it is not walked or watched until `drive location resume`.", l.name)
            } else {
                format!("{} resumes: it is walked again on the next pass.", l.name)
            });
        }
        "location" if matches!(args.get(1).map(String::as_str), Some("rm" | "remove")) => {
            let usage = "location rm NAME_OR_ID [--force]";
            let who = args[2..]
                .iter()
                .find(|x| !x.starts_with("--"))
                .ok_or(usage)?
                .clone();
            let force = args.iter().any(|x| x == "--force");
            let a = start(dir).await?;
            match a.remove_location(&who, force).await {
                Ok(r) => {
                    settle(&a).await;
                    let place = if r.location.storage.is_empty() {
                        r.location.path.clone()
                    } else {
                        format!("{} in its storage", r.location.path)
                    };
                    out(format!(
                        "{} is no longer a location: Drive stopped tracking {place}. Its folder and files are as they were.",
                        r.location.name
                    ));
                    if let Err(e) = r.node_dropped {
                        out(format!(
                            "The node still has its index ({e}): `drive location rm {} --force` drops it.",
                            r.location.id
                        ));
                    }
                    for m in r.mirrors {
                        out(format!("{} mirrored it and stays a location.", m.name));
                    }
                }
                // Out of the registry already (the node was unreachable
                // when it was removed): its index on the node, by id.
                Err(e) if force && e.starts_with("no location") => {
                    a.drop_node_index(&who).await?;
                    out(format!("The node's index of {who} is dropped."));
                }
                Err(e) => return Err(e),
            }
        }
        "location" if args.get(1).map(String::as_str) == Some("add") => {
            let name = args.get(2).ok_or("location add NAME PATH")?.clone();
            let raw = args.get(3).ok_or("location add NAME PATH")?.clone();
            let a = start(dir).await?;
            let me = a.device_id();
            let loc = match flag(args, "--storage") {
                Some(sname) => {
                    let own = a
                        .cfg
                        .lock()
                        .expect("cfg")
                        .storages
                        .iter()
                        .find(|s| s.name == sname || s.id == sname)
                        .cloned();
                    let own = own.ok_or(format!("this device runs no storage {sname}"))?;
                    let loc = space::Location {
                        id: space::new_id(),
                        name: name.clone(),
                        device: me,
                        path: raw.trim_matches('/').to_string(),
                        volume: String::new(),
                        storage: own.id,
                        mirror_of: String::new(),
                        index: String::new(),
                        files: 0,
                        bytes: 0,
                        built: 0,
                    };
                    space::add_location(&a.space, &loc).map_err(|e| e.to_string())?;
                    a.commit_space();
                    loc
                }
                None => a.add_location(&name, std::path::Path::new(&raw))?,
            };
            let st = a.index_location(&loc).await?;
            settle(&a).await;
            out(format!(
                "{name}: {} files, {} folders, {} ({} read)",
                st.files,
                st.dirs,
                human(st.bytes),
                st.hashed
            ));
        }
        "locations" => {
            let a = start(dir).await?;
            let devices = space::devices(&a.space);
            let storages = space::storages(&a.space);
            let on = daemon::online_devices(&a);
            for l in space::locations(&a.space) {
                let dev = devices
                    .iter()
                    .find(|d| d.id == l.device)
                    .map_or("?".into(), |d| d.name.clone());
                let place = match storages.iter().find(|s| s.id == l.storage) {
                    Some(s) => format!("{} · {}", s.name, l.path),
                    None => l.path.clone(),
                };
                let live = if on.contains(&l.device) {
                    "online "
                } else {
                    "offline"
                };
                out(format!(
                    "{:<18} {:<16} {live} {:>8} files {:>9}  {place}",
                    l.name,
                    dev,
                    l.files,
                    human(l.bytes.max(0) as u64)
                ));
            }
        }
        "volumes" => {
            let a = start(dir).await?;
            a.report_volumes();
            settle(&a).await;
            let devices = space::devices(&a.space);
            for v in space::volumes(&a.space) {
                let dev = devices
                    .iter()
                    .find(|d| d.id == v.device)
                    .map_or("?".into(), |d| d.name.clone());
                out(format!(
                    "{:<16} {:<24} {:<6} {:>9} free of {:>9}{}",
                    dev,
                    v.mount,
                    v.fs,
                    human(v.free.max(0) as u64),
                    human(v.total.max(0) as u64),
                    if v.removable { "  removable" } else { "" }
                ));
            }
        }
        "index" => {
            let a = start(dir).await?;
            for loc in a
                .my_locations()
                .into_iter()
                .filter(|l| args.get(1).is_none_or(|n| *n == l.name))
            {
                let st = a.index_location(&loc).await?;
                out(format!(
                    "{}: {} files, {} ({} read)",
                    loc.name,
                    st.files,
                    human(st.bytes),
                    st.hashed
                ));
            }
            settle(&a).await;
        }
        "adopt" if args.iter().any(|x| x == "--list") => {
            let a = start(dir).await?;
            let locations = space::locations(&a.space);
            let devices = space::devices(&a.space);
            for ad in a.adoptions() {
                let (dev, place) = match locations.iter().find(|l| l.id == ad.location) {
                    Some(l) => (
                        devices
                            .iter()
                            .find(|d| d.id == l.device)
                            .map_or("?".into(), |d| d.name.clone()),
                        drive_files::adopt::join(&l.path, &ad.sub),
                    ),
                    None => ("?".into(), format!("(location {} is gone)", ad.location)),
                };
                out(format!("{:<28} {:<16} {place}", ad.at, dev));
            }
        }
        "adopt" => {
            let usage = "adopt FOLDER --as LOGICAL/PATH | adopt --list";
            let at = flag(args, "--as").ok_or(usage)?;
            let mut rest = args[1..].iter();
            let mut folder = None;
            while let Some(x) = rest.next() {
                if x == "--as" {
                    rest.next();
                } else if folder.is_none() {
                    folder = Some(x.clone());
                }
            }
            let folder = folder.ok_or(usage)?;
            let a = start(dir).await?;
            let r = a.adopt(Path::new(&folder), &at).await?;
            settle(&a).await;
            let how = if r.new_location {
                format!("a new location, {}", r.location.name)
            } else {
                let sub = if r.adoption.sub.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", r.adoption.sub)
                };
                format!("in the location {}{sub}", r.location.name)
            };
            out(format!(
                "{folder} is adopted at {}: {how}. Nothing was copied or moved.",
                r.adoption.at
            ));
            match r.indexed {
                Some(Ok(st)) => out(format!(
                    "indexed: {} files, {} folders, {}",
                    st.files,
                    st.dirs,
                    human(st.bytes)
                )),
                Some(Err(e)) => out(format!(
                    "not indexed yet ({e}); the agent indexes it on its next pass"
                )),
                None => {}
            }
        }
        "unadopt" => {
            let at = args.get(1).ok_or("unadopt LOGICAL/PATH")?.clone();
            let a = start(dir).await?;
            let ad = a.unadopt(&at)?;
            settle(&a).await;
            out(format!(
                "{} is no longer adopted; its folder, files and location are as they were.",
                ad.at
            ));
        }
        // An adopted folder becomes the Drive tree's own, its folder here
        // its working copy (docs/working-copies.md).
        "own" => {
            let a = start(dir).await?;
            // The tree's edits must reach the node (the agent running beside
            // this command gets them from there): not while it is away.
            if tokio::time::timeout(std::time::Duration::from_secs(15), a.drive.online())
                .await
                .is_err()
            {
                return Err("the node is not reachable: `drive own` needs it, try again when it is".into());
            }
            let targets: Vec<String> = match args.get(1).map(String::as_str) {
                Some("--all") => {
                    let mine: Vec<String> = a.my_locations().into_iter().map(|l| l.id).collect();
                    a.adoptions()
                        .into_iter()
                        .filter(|ad| mine.contains(&ad.location))
                        .map(|ad| ad.at)
                        .collect()
                }
                Some(p) => vec![p.to_string()],
                None => return Err("own LOGICAL/PATH | own --all".into()),
            };
            if targets.is_empty() {
                out("nothing adopted from this device's folders");
            }
            let mut failed = 0;
            for at in targets {
                match a.own(&at).await {
                    Ok((w, r)) => {
                        let folder = a.working_folder(&w).map(|p| p.display().to_string()).unwrap_or_default();
                        out(format!(
                            "{at}: the Drive's own now; working copy {folder} ({} files in, {} errors)",
                            r.up,
                            r.errors.len()
                        ));
                        for e in r.errors.iter().take(5) {
                            out(format!("  {e}"));
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        out(format!("{at}: {e}"));
                    }
                }
            }
            settle(&a).await;
            // Let the last edits go out and the copies be saved.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if failed > 0 {
                return Err(format!("{failed} not owned"));
            }
        }
        "working" => {
            let a = start(dir).await?;
            let all = drive_files::working::working_copies(&a.space);
            if all.is_empty() {
                out("no working copies");
            }
            for w in all {
                let at = tree::parse_id(&w.root)
                    .map(|id| tree::path_of(&a.files, id))
                    .unwrap_or_default();
                let device = space::devices(&a.space)
                    .into_iter()
                    .find(|d| d.id == w.device)
                    .map_or(w.device.clone(), |d| d.name);
                let disk = space::locations(&a.space)
                    .into_iter()
                    .find(|l| l.id == w.location)
                    .map(|l| if w.sub.is_empty() { l.path } else { format!("{}/{}", l.path, w.sub) })
                    .unwrap_or_default();
                out(format!("drive:{at}  ←→  {device}:{disk}"));
            }
        }
        "tag" => tagging::command(dir, args).await?,
        // An MCP server on stdio for AI agents (docs/agents.md): stdout is
        // the protocol's, so nothing else here may print.
        "mcp" => mcp::serve(dir, args).await?,
        "rules" => rules::command(dir, args).await?,
        // From the copies on disk alone (docs/where.md): no agent, no node,
        // nothing written, so it answers with Drive down.
        "where" => {
            let cfg = Config::load(dir)?;
            let copies = locate::load(dir, &cfg.space)?;
            if args.get(1).map(String::as_str) == Some("--find") {
                let words = args[2..].join(" ");
                if words.trim().is_empty() {
                    return Err("where --find WORDS…".into());
                }
                let mut n = 0;
                for path in locate::find_in_tree(&copies, &words, 200) {
                    out(format!("drive:{path}"));
                    print_found(&locate::locate(&copies, &path));
                    n += 1;
                }
                match index_db::IndexDb::open_existing(&dir.join("indexes.db")).await {
                    Ok(Some(db)) => {
                        for (loc, e) in db.find(&words, 200).await.map_err(|e| e.to_string())? {
                            let d = locate::on_disk(&copies, &loc, &e.path);
                            let mark = if e.dir { "/" } else { "" };
                            match &d.drive {
                                Some(p) => out(format!("drive:{p}{mark}")),
                                None => out(format!("{}:{}{mark}", d.location, e.path)),
                            }
                            out(format!("  on {}: {}{mark}", d.device, d.real));
                            n += 1;
                        }
                    }
                    Ok(None) => out("# no location indexes on this computer"),
                    Err(e) => out(format!("# the location indexes would not open: {e}")),
                }
                if n == 0 {
                    return Err(format!("nothing called {words:?}"));
                }
                return Ok(());
            }
            let path = args.get(1).ok_or("where DRIVE/PATH | where --find WORDS…")?;
            let path = path.strip_prefix("drive:").unwrap_or(path);
            let found = locate::locate(&copies, path);
            if let locate::Found::Missing { path } = &found {
                return Err(format!("drive:{path} is not in the Drive (as this computer last saw it)"));
            }
            print_found(&found);
        }
        "search" => {
            let words = args[1..].join(" ");
            let cfg = Config::load(dir)?;
            let v = node_get(
                &cfg,
                "/search",
                &[
                    ("space", cfg.space.as_str()),
                    ("q", words.as_str()),
                    ("limit", "60"),
                ],
            )
            .await?;
            for h in v["results"].as_array().into_iter().flatten() {
                let dir_mark = if h["dir"].as_bool() == Some(true) {
                    "/"
                } else {
                    ""
                };
                let live = if h["online"].as_bool() == Some(true) {
                    "●"
                } else {
                    "○"
                };
                let copies = h["copies"].as_u64().unwrap_or(1);
                out(format!(
                    "{live} {:<14} {:<14} {:>9}  {}{dir_mark}{}",
                    h["device_name"].as_str().unwrap_or("?"),
                    h["location_name"].as_str().unwrap_or("?"),
                    if dir_mark.is_empty() {
                        human(h["size"].as_u64().unwrap_or(0))
                    } else {
                        String::new()
                    },
                    h["path"].as_str().unwrap_or(""),
                    if copies > 1 {
                        format!("   ×{copies} copies")
                    } else {
                        String::new()
                    }
                ));
            }
        }
        "copies" => {
            let a = start(dir).await?;
            let lname = args.get(1).ok_or("copies LOCATION PATH")?;
            let path = args.get(2).ok_or("copies LOCATION PATH")?;
            let loc = space::locations(&a.space)
                .into_iter()
                .find(|l| l.name == *lname)
                .ok_or(format!("no location {lname}"))?;
            a.sync_indexes().await;
            let e = a
                .index_entry(&loc.id, path)
                .await
                .ok_or(format!("{path} is not in {lname} (or it has no index yet)"))?;
            let cfg = a.cfg.lock().expect("cfg").clone();
            let size = e.size.to_string();
            let v = node_get(
                &cfg,
                "/copies",
                &[
                    ("space", cfg.space.as_str()),
                    ("sampled", e.sampled.as_str()),
                    ("size", size.as_str()),
                ],
            )
            .await?;
            for c in v["copies"].as_array().into_iter().flatten() {
                out(format!(
                    "{} {:<14} {:<14} {:<10} {}",
                    if c["online"].as_bool() == Some(true) {
                        "●"
                    } else {
                        "○"
                    },
                    c["device_name"].as_str().unwrap_or("?"),
                    c["location_name"].as_str().unwrap_or("?"),
                    c["site"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("(no site)"),
                    c["path"].as_str().unwrap_or("")
                ));
            }
        }
        "kache" => kache_command(dir, &args[1..]).await?,
        "role" => {
            let role = args.get(1).ok_or("role storage-server|workstation")?;
            if role != "storage-server" && role != "workstation" {
                return Err(format!("unknown role {role}"));
            }
            let a = start(dir).await?;
            let me = a.device_id();
            let mut roles: Vec<String> = space::devices(&a.space)
                .into_iter()
                .find(|d| d.id == me)
                .map(|d| d.roles)
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r != "storage-server" && r != "workstation")
                .collect();
            roles.push(role.clone());
            space::set_roles(&a.space, &me, &roles).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(match role.as_str() {
                "storage-server" => {
                    "This device keeps everything it has room for, and mirrors other computers' locations."
                }
                _ => {
                    "This device keeps its active folders and pins; other bytes go once a storage holds them."
                }
            });
        }
        "active" => {
            let a = start(dir).await?;
            let me = a.device_id();
            match (args.get(1).map(String::as_str), args.get(2)) {
                (Some(op @ ("add" | "rm")), Some(p)) => {
                    let p = p.trim_matches('/').to_string();
                    if op == "add"
                        && !p.is_empty()
                        && tree::resolve(&a.files, &p).flatten().is_none_or(|e| !e.dir)
                    {
                        return Err(format!("no folder {p}"));
                    }
                    space::set_active(&a.space, &me, &p, op == "add").map_err(|e| e.to_string())?;
                    a.commit_space();
                    settle(&a).await;
                }
                (None, _) => {}
                _ => return Err("active [add|rm PATH]".into()),
            }
            for p in space::active(&a.space, &me) {
                out(format!("/{p}"));
            }
        }
        "available" => {
            let a = start(dir).await?;
            let n = a.make_available().await;
            let p = a.publish_available().await;
            settle(&a).await;
            out(format!(
                "{n} files put on the node, {p} published to public storage"
            ));
        }
        "place" => {
            let a = start(dir).await?;
            let d = a.place().await;
            settle(&a).await;
            out(format!(
                "{:?}: {} fetched, {} let go, {} mirrored{}",
                a.role(),
                d.fetched,
                d.evicted,
                d.mirrored,
                if d.no_room > 0 {
                    format!(", {} did not fit", human(d.no_room))
                } else {
                    String::new()
                }
            ));
        }
        "storage" if args.get(1).map(String::as_str) == Some("public") => {
            let usage = "storage public STORAGE URL|off";
            let who = args.get(2).ok_or(usage)?;
            let url = args.get(3).ok_or(usage)?;
            let a = start(dir).await?;
            let id = space::storages(&a.space)
                .into_iter()
                .find(|s| s.name == *who || s.id == *who)
                .map(|s| s.id)
                .ok_or(format!("no storage {who}"))?;
            let url = if url == "off" { "" } else { url.as_str() };
            space::set_public(&a.space, &id, url).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(if url.is_empty() {
                format!("{who} no longer serves available files")
            } else {
                format!("{who} serves available files at {url}")
            });
        }
        "site"
            if matches!(
                args.get(1).map(String::as_str),
                Some("theme-init" | "create" | "ls" | "show" | "rm" | "build")
            ) =>
        {
            site_command(dir, args).await?;
        }
        "site" => {
            let who = args.get(1).ok_or("site DEVICE_OR_STORAGE SITE")?;
            let site = args.get(2).ok_or("site DEVICE_OR_STORAGE SITE")?;
            let a = start(dir).await?;
            let id = space::devices(&a.space)
                .into_iter()
                .find(|d| d.name == *who || d.id == *who)
                .map(|d| d.id)
                .or_else(|| {
                    space::storages(&a.space)
                        .into_iter()
                        .find(|s| s.name == *who || s.id == *who)
                        .map(|s| s.id)
                })
                .ok_or(format!("no device or storage {who}"))?;
            space::set_site(&a.space, &id, site).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!("{who} is at {site}"));
        }
        "policy" if args.get(1).map(String::as_str) == Some("add") => {
            let name = args.get(2).ok_or("policy add NAME")?.clone();
            let num = |f: &str, d: i64| flag(args, f).and_then(|v| v.parse().ok()).unwrap_or(d);
            let a = start(dir).await?;
            let locs = space::locations(&a.space);
            let scope = match flag(args, "--scope") {
                Some(list) => list
                    .split(',')
                    .map(|n| {
                        locs.iter()
                            .find(|l| l.name == n || l.id == n)
                            .map(|l| l.id.clone())
                            .ok_or(format!("no location {n}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                None => Vec::new(),
            };
            let p = space::Policy {
                id: space::new_id(),
                name: name.clone(),
                scope,
                copies: num("--copies", 3),
                media: num("--media", 2),
                offsite: num("--offsite", 1),
            };
            space::set_policy(&a.space, &p).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "{name}: {} copies, {} media, {} offsite",
                p.copies, p.media, p.offsite
            ));
        }
        "protection" => {
            let a = start(dir).await?;
            let cfg = a.cfg.lock().expect("cfg").clone();
            let locs = space::locations(&a.space);
            let lname = |id: &str| {
                locs.iter()
                    .find(|l| l.id == id)
                    .map_or(id.to_string(), |l| l.name.clone())
            };
            let v = node_get(
                &cfg,
                "/protection",
                &[("space", cfg.space.as_str()), ("limit", "8")],
            )
            .await?;
            for r in v["reports"].as_array().into_iter().flatten() {
                let p = &r["policy"];
                let (files, ok) = (
                    r["files"].as_u64().unwrap_or(0),
                    r["ok"].as_u64().unwrap_or(0),
                );
                out(format!(
                    "{} ({} copies, {} media, {} offsite): {ok} of {files} files kept so, {} of {} at risk",
                    p["name"].as_str().unwrap_or("?"),
                    p["copies"],
                    p["media"],
                    p["offsite"],
                    human(r["bytes"].as_u64().unwrap_or(0) - r["bytes_ok"].as_u64().unwrap_or(0)),
                    human(r["bytes"].as_u64().unwrap_or(0)),
                ));
                for l in r["locations"].as_array().into_iter().flatten() {
                    out(format!(
                        "  {:<18} {:>6} of {:>6} files",
                        lname(l["location"].as_str().unwrap_or("")),
                        l["ok"],
                        l["files"]
                    ));
                }
                for sh in r["short"].as_array().into_iter().flatten() {
                    let k = &sh["kept"];
                    out(format!(
                        "  ! {}/{}  ({} copies, {} media, {} offsite)",
                        lname(sh["location"].as_str().unwrap_or("")),
                        sh["path"].as_str().unwrap_or(""),
                        k["copies"],
                        k["media"],
                        k["offsite"]
                    ));
                }
            }
        }
        "backup" => {
            let lname = args.get(1).ok_or("backup LOCATION STORAGE")?;
            let sname = args.get(2).ok_or("backup LOCATION STORAGE")?;
            let a = start(dir).await?;
            let src = a
                .my_locations()
                .into_iter()
                .find(|l| l.name == *lname && l.storage.is_empty());
            let src = src.ok_or(format!("{lname} is not a folder on this device"))?;
            let own = a
                .cfg
                .lock()
                .expect("cfg")
                .storages
                .iter()
                .find(|s| s.name == *sname || s.id == *sname)
                .cloned();
            let own = own.ok_or(format!("this device runs no storage {sname}"))?;
            let op = own.config.operator().map_err(|e| e.to_string())?;
            let base = format!("backups/{}", src.name);
            if a.index_db.meta(&src.id).await.ok().flatten().is_none() {
                return Err("index the location first".into());
            }
            let (mut sent, mut kept, mut bytes) = (0usize, 0usize, 0u64);
            let mut pages = a.index_pages(&src.id, "");
            while let Some(page) = pages.next().await {
                for e in page.iter().filter(|e| !e.dir) {
                    let key = format!("{base}/{}", e.path);
                    if op
                        .stat(&key)
                        .await
                        .is_ok_and(|m| m.content_length() == e.size)
                    {
                        kept += 1;
                        continue;
                    }
                    let from = places::confined(Path::new(&src.path), &e.path, &a.dir)
                        .ok_or("a bad path")?;
                    let data = tokio::fs::read(&from)
                        .await
                        .map_err(|x| format!("{}: {x}", e.path))?;
                    bytes += data.len() as u64;
                    op.write(&key, data).await.map_err(|x| x.to_string())?;
                    sent += 1;
                }
            }
            // The copy is a location too: indexed, so it counts as a copy.
            let existing = space::locations(&a.space)
                .into_iter()
                .find(|l| l.storage == own.id && l.path == base);
            let loc = match existing {
                Some(l) => l,
                None => {
                    let l = space::Location {
                        id: space::new_id(),
                        name: format!("{} on {}", src.name, own.name),
                        device: a.device_id(),
                        path: base.clone(),
                        volume: String::new(),
                        storage: own.id.clone(),
                        mirror_of: String::new(),
                        index: String::new(),
                        files: 0,
                        bytes: 0,
                        built: 0,
                    };
                    space::add_location(&a.space, &l).map_err(|e| e.to_string())?;
                    a.commit_space();
                    l
                }
            };
            a.index_location(&loc).await?;
            settle(&a).await;
            out(format!(
                "{lname} → {}: {sent} files copied ({}), {kept} already there",
                own.name,
                human(bytes)
            ));
        }
        _ => {
            out(include_str!("main.rs")
                .lines()
                .take_while(|l| l.starts_with("//!"))
                .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
                .collect::<Vec<_>>()
                .join("\n"));
        }
    }
    Ok(())
}

/// Mount a file system, clearing a stale mount first and retrying while
/// the kernel lets go of it.
#[cfg(feature = "mount")]
async fn mount_fs<F: fuser::Filesystem + Send + Sync + 'static>(
    at: &Path,
    name: &str,
    make: impl Fn() -> F,
) -> Result<fuser::BackgroundSession, String> {
    if let Err(e) = std::fs::metadata(at)
        && e.raw_os_error() == Some(libc::ENOTCONN)
    {
        let _ = std::process::Command::new("fusermount3")
            .args(["-uz"])
            .arg(at)
            .status();
    }
    std::fs::create_dir_all(at).map_err(|e| e.to_string())?;
    let mut config = fuser::Config::default();
    // A slow answer (another device's disk, a file brought from elsewhere)
    // holds one thread, not the whole mount.
    config.n_threads = Some(4);
    config.mount_options = vec![
        fuser::MountOption::FSName(name.to_string()),
        fuser::MountOption::Subtype("drive".into()),
        fuser::MountOption::DefaultPermissions,
    ];
    let mut tries = 0;
    loop {
        match fuser::spawn_mount(make(), at, &config) {
            Ok(s) => return Ok(s),
            Err(e) if tries < 5 => {
                tracing::warn!(error = %e, "mount failed; trying again");
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            Err(e) => return Err(format!("mount {}: {e}", at.display())),
        }
    }
}

/// `drive site …`: static sites.
async fn site_command(dir: &Path, args: &[String]) -> Result<(), String> {
    use drive_files::sites::{self as dsites, Site};
    let a = start(dir).await?;
    let node = a.cfg.lock().expect("cfg").node_http();
    match args.get(1).map(String::as_str).unwrap_or("") {
        "theme-init" => {
            let folder = args.get(2).map_or(sites::DEFAULT_THEME, String::as_str);
            let n = a.theme_init(folder).await?;
            settle(&a).await;
            out(format!(
                "{folder}: {n} theme files added (templates/, sass/, static/). Edit them there, or give a site an override folder."
            ));
        }
        "create" => {
            let title = args.get(2).filter(|t| !t.starts_with("--")).ok_or(
                "site create TITLE --source FOLDER [--id ID] [--theme FOLDER] [--overrides FOLDER] [--storage STORAGE] [--base-url URL] [--manual]",
            )?;
            let source = flag(args, "--source").ok_or("--source FOLDER (the Markdown pages)")?;
            let source = source.trim_matches('/').to_string();
            if tree::resolve(&a.files, &source)
                .flatten()
                .is_none_or(|e| !e.dir)
            {
                return Err(format!("no folder {source}"));
            }
            let id = flag(args, "--id").unwrap_or_else(|| dsites::id_from(title));
            if id.is_empty() {
                return Err("the site needs an id (--id)".into());
            }
            if dsites::listed(&a.space).contains_key(&id) {
                return Err(format!("there is already a site {id}"));
            }
            let theme = flag(args, "--theme").unwrap_or_else(|| sites::DEFAULT_THEME.into());
            if tree::resolve(&a.files, &theme).flatten().is_none() {
                let n = a.theme_init(&theme).await?;
                out(format!("{theme}: the starter theme ({n} files)"));
            }
            let storage = match flag(args, "--storage") {
                Some(s) => space::storages(&a.space)
                    .into_iter()
                    .find(|x| x.name == s || x.id == s)
                    .map(|x| x.id)
                    .ok_or(format!("no storage {s}"))?,
                None => String::new(),
            };
            let site = Site {
                id: id.clone(),
                title: title.clone(),
                source,
                theme: theme.trim_matches('/').to_string(),
                overrides: flag(args, "--overrides")
                    .map(|o| o.trim_matches('/').to_string())
                    .unwrap_or_default(),
                storage,
                base_url: flag(args, "--base-url").unwrap_or_default(),
                builder: a.device_id(),
                auto: !args.iter().any(|x| x == "--manual"),
                created: drive_files::now_ms(),
                ..Site::default()
            };
            let (doc_id, doc) = a.site_doc(&id).await;
            dsites::write(&doc, &site).map_err(|e| e.to_string())?;
            a.drive.commit(&doc_id);
            dsites::set_listed(&a.space, &id, Some(title)).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "Site {id} ({title}) from {}; build it with `drive site build {id}`{}.",
                site.source,
                if site.auto {
                    " (and this device's agent rebuilds it when its pages change)"
                } else {
                    ""
                }
            ));
        }
        "ls" => {
            for (id, title) in dsites::listed(&a.space) {
                let (_, doc) = a.site_doc(&id).await;
                let site = dsites::read(&doc).unwrap_or_default();
                let state = match dsites::last_build(&doc) {
                    None => "not built".to_string(),
                    Some(b) if b.ok => format!(
                        "built, {} pages, {} broken links",
                        b.pages,
                        b.broken_links.len()
                    ),
                    Some(b) => format!("failed: {}", b.message.lines().next().unwrap_or_default()),
                };
                out(format!("{id:<20} {title:<24} {:<28} {state}", site.source));
            }
        }
        "show" => {
            let id = args.get(2).ok_or("site show ID")?;
            let (_, doc) = a.site_doc(id).await;
            let site = dsites::read(&doc).ok_or(format!("no site {id}"))?;
            let builder = space::devices(&a.space)
                .into_iter()
                .find(|d| d.id == site.builder)
                .map_or(site.builder.clone(), |d| d.name);
            out(format!("{} ({})", site.title, site.id));
            out(format!("  pages      {}", site.source));
            out(format!("  theme      {}", site.theme));
            out(format!(
                "  overrides  {}",
                if site.overrides.is_empty() {
                    "-"
                } else {
                    &site.overrides
                }
            ));
            out(format!(
                "  storage    {}",
                if site.storage.is_empty() {
                    "the node only"
                } else {
                    &site.storage
                }
            ));
            out(format!(
                "  built by   {builder}{}",
                if site.auto {
                    ", rebuilt when it changes"
                } else {
                    ""
                }
            ));
            match dsites::last_build(&doc) {
                None => out("  not built yet"),
                Some(b) => {
                    out(format!(
                        "  last build {} ({} ms): {}",
                        if b.ok { "ok" } else { "failed" },
                        b.duration_ms,
                        b.message
                    ));
                    for at in sites::addresses(&node, &b) {
                        out(format!("  at         {at}"));
                    }
                    for l in &b.broken_links {
                        out(format!("  broken     {l}"));
                    }
                    for l in &b.unresolved {
                        out(format!("  unresolved {l}"));
                    }
                }
            }
        }
        "rm" => {
            let id = args.get(2).ok_or("site rm ID")?;
            dsites::set_listed(&a.space, id, None).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            out(format!(
                "{id} is no longer listed; what it published stays until it is deleted there."
            ));
        }
        "build" => {
            let id = args.get(2).ok_or("site build ID [--force]")?;
            let force = args.iter().any(|x| x == "--force");
            match a.site_build(id, force).await? {
                sites::Outcome::Unchanged => out(format!(
                    "{id}: nothing changed since its last build (--force builds it anyway)"
                )),
                sites::Outcome::Built(b) => {
                    out(format!("{id}: {} in {} ms", b.message, b.duration_ms));
                    for at in sites::addresses(&node, &b) {
                        out(format!("  {at}"));
                    }
                    for l in b.broken_links.iter().take(20) {
                        out(format!("  broken link {l}"));
                    }
                    for l in b.unresolved.iter().take(20) {
                        out(format!("  unresolved {l}"));
                    }
                }
            }
            settle(&a).await;
        }
        _ => return Err("site theme-init|create|ls|show|rm|build".into()),
    }
    Ok(())
}

/// Run until Ctrl-C or SIGTERM (systemd's stop).
async fn run_until_stopped(work: impl std::future::Future<Output = ()>) -> Result<(), String> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = work => {}
    }
    Ok(())
}

async fn node_get(
    cfg: &Config,
    path: &str,
    query: &[(&str, &str)],
) -> Result<serde_json::Value, String> {
    // The device's session, as the agent itself sends it: a node reached
    // from another computer answers 401 without it (drive#63).
    let r = agent::http_client(cfg.token.as_deref())
        .get(format!("{}{path}", cfg.node_http()))
        .query(query)
        .send()
        .await
        .map_err(|e| format!("the node: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("the node answered {}", r.status()));
    }
    r.json().await.map_err(|e| e.to_string())
}

async fn kache_command(dir: &Path, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "group" => {
            let name = args.get(1).ok_or("kache group NAME|auto|off")?;
            let group = match name.as_str() {
                "off" => String::new(),
                "auto" => kache::default_group(),
                g => g.to_string(),
            };
            let a = start(dir).await?;
            space::set_build_group(&a.space, &a.device_id(), &group).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            if group.is_empty() {
                out(
                    "This device shares no build cache now; its agent stops serving kache's remote within a minute.",
                );
            } else {
                let peers = kache::group_peers(&a.space, &a.device_id());
                out(format!(
                    "Build group {group}: shared with {}.",
                    if peers.is_empty() {
                        "no other device yet".to_string()
                    } else {
                        peers
                            .iter()
                            .map(|p| p.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
                out(
                    "The agent serves kache's remote within a minute; `drive kache config` prints kache's settings.",
                );
            }
        }
        "status" => {
            let a = start(dir).await?;
            let settings = a.cfg.lock().expect("cfg").kache.clone();
            let group = kache::my_group(&a);
            out(format!(
                "Build group: {}",
                if group.is_empty() {
                    "none (`drive kache group auto`)"
                } else {
                    &group
                }
            ));
            if !group.is_empty() {
                let peers = kache::group_peers(&a.space, &a.device_id());
                let names: Vec<String> = peers.iter().map(|p| p.name.clone()).collect();
                out(format!(
                    "Shared with: {}",
                    if names.is_empty() {
                        "no other device yet".into()
                    } else {
                        names.join(", ")
                    }
                ));
            }
            let (n, bytes) = drive_kache::store::summary(&a.dir.join("kache"));
            out(format!(
                "Kept here: {n} objects, {} of {}",
                human(bytes),
                human(settings.budget())
            ));
            out(format!(
                "Endpoint: http://127.0.0.1:{} (bucket `{}`)",
                settings.port(),
                drive_kache::BUCKET
            ));
        }
        "config" => {
            let port = Config::load(dir)?.kache.port();
            out(kache::remote_block(port));
        }
        "budget" | "port" => {
            let what = args[0].as_str();
            let n: u64 = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .ok_or(format!("kache {what} NUMBER"))?;
            let mut cfg = Config::load(dir)?;
            if what == "budget" {
                cfg.kache.budget_gib = Some(n);
            } else {
                cfg.kache.port = Some(u16::try_from(n).map_err(|_| "a port is below 65536")?);
            }
            cfg.save(dir)?;
            out("Saved; the agent uses it when it next starts.");
        }
        other => return Err(format!("unknown: kache {other}")),
    }
    Ok(())
}

/// `drive status` with no file: the app cache and what apps hold.
fn app_status(dir: &Path) -> Result<(), String> {
    let c = apps::client(dir, "drive status")?;
    let st = c.status().map_err(|e| e.to_string())?;
    out(format!(
        "app cache: {} files, {} of {} ({})",
        st.cache_files,
        human(st.cache_bytes),
        human(st.budget),
        st.cache_dir.display()
    ));
    if st.partial_files > 0 {
        out(format!(
            "  {} fetches stopped part way ({}; they resume when asked for again)",
            st.partial_files,
            human(st.partial_bytes)
        ));
    }
    let others: Vec<&String> = st.apps.iter().filter(|a| *a != "drive status").collect();
    out(format!(
        "apps connected: {}",
        if others.is_empty() {
            "none".to_string()
        } else {
            others
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    print_pins(&st.pins);
    Ok(())
}

fn print_pins(pins: &[drive_local::Pin]) {
    if pins.is_empty() {
        out("nothing pinned by apps");
        return;
    }
    out(format!(
        "pinned by apps: {} files, {}",
        pins.len(),
        human(pins.iter().map(|p| p.size).sum())
    ));
    for p in pins {
        let how = match p.how {
            drive_local::How::Local => "in place",
            drive_local::How::Cached | drive_local::How::Fetched => "cached",
        };
        out(format!("  {} ({}, {how})", p.path.display(), human(p.size)));
        for h in &p.holders {
            out(format!("    {} · {} · {}", h.app, h.why, h.target));
        }
    }
}

/// `open`, `warm`, `pins`, `apps budget`: through the running agent.
fn app_command(dir: &Path, args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "open" => {
            let target = args.get(1).ok_or("open TARGET")?;
            let c = apps::client(dir, "drive open")?;
            let mut o = drive_local::OpenOptions::new();
            o.hash = args.iter().any(|a| a == "--hash");
            let started = std::time::Instant::now();
            let f = c.open_with(target, o).map_err(|e| e.to_string())?;
            out(f.path.display().to_string());
            let _ = writeln!(
                std::io::stderr(),
                "{:?} · {} · {}{} ms",
                f.how,
                human(f.size),
                f.hash
                    .as_deref()
                    .map(|h| format!("blake3 {h} · "))
                    .unwrap_or_default(),
                started.elapsed().as_millis()
            );
        }
        "warm" => {
            let mut w = drive_local::Warm::default();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--project" => {
                        w.project = args.get(i + 1).cloned();
                        i += 1;
                    }
                    "--kind" => {
                        w.kinds.extend(args.get(i + 1).cloned());
                        i += 1;
                    }
                    "--priority" => {
                        w.priority = match args.get(i + 1).map(String::as_str) {
                            Some("low") => drive_local::Priority::Low,
                            Some("high") => drive_local::Priority::High,
                            _ => drive_local::Priority::Normal,
                        };
                        i += 1;
                    }
                    t => w.targets.push(t.to_string()),
                }
                i += 1;
            }
            if w.targets.is_empty() && w.project.is_none() {
                return Err(
                    "warm TARGET… [--project NAME|ORG/NAME] [--kind KIND]… [--priority low|normal|high]"
                        .into(),
                );
            }
            // The command line holds nothing once it ends: what came stays
            // in the cache, unpinned.
            w.lease = false;
            let c = apps::client(dir, "drive warm")?;
            let started = std::time::Instant::now();
            let job = c.warm(w).map_err(|e| e.to_string())?;
            let s = &job.started;
            let _ = writeln!(
                std::io::stderr(),
                "{} files ({}), {} here already",
                s.files,
                human(s.bytes),
                s.here
            );
            let mut last = None;
            while let Some(ev) = job.recv() {
                match ev {
                    drive_local::Event::Progress(p) => {
                        if let Some(f) = &p.file
                            && f.state == drive_local::FileState::Failed
                        {
                            let _ = writeln!(
                                std::io::stderr(),
                                "  failed: {}: {}",
                                f.target,
                                f.error.as_deref().unwrap_or("")
                            );
                        }
                        let _ = write!(
                            std::io::stderr(),
                            "\r{}/{} files · {} / {} · {}/s{}        ",
                            p.files_done,
                            p.files,
                            human(p.bytes_done),
                            human(p.bytes),
                            human(p.rate),
                            p.eta_ms
                                .map(|e| format!(" · {} s left", e / 1000))
                                .unwrap_or_default()
                        );
                    }
                    drive_local::Event::Done(p) => {
                        last = Some(p);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = writeln!(std::io::stderr());
            let p = last.ok_or("the agent went away")?;
            out(format!(
                "{} of {} files here, {} failed, {} in {:.1} s",
                p.files_done,
                p.files,
                p.files_failed,
                human(p.bytes_done),
                started.elapsed().as_secs_f64()
            ));
        }
        "pins" => {
            let c = apps::client(dir, "drive pins")?;
            print_pins(&c.pins().map_err(|e| e.to_string())?);
        }
        "apps" => match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("budget"), Some(n)) => {
                let gib: u64 = n.parse().map_err(|_| "apps budget GIB")?;
                let mut cfg = Config::load(dir)?;
                cfg.apps.budget_gib = Some(gib);
                cfg.save(dir)?;
                out(format!(
                    "the app cache keeps {gib} GiB (from the agent's next start)"
                ));
            }
            _ => return Err("apps budget GIB".into()),
        },
        _ => unreachable!(),
    }
    Ok(())
}

/// One `drive where` answer, indented under its path.
fn print_found(f: &locate::Found) {
    match f {
        locate::Found::Adopted { device, location, real, .. } => {
            out(format!("  on {device}: {real}  (adopted from {location})"));
        }
        locate::Found::Working { device, location, real, file, .. } => {
            out(format!("  on {device}: {real}  (working copy, {location})"));
            if let Some((size, hash, holders)) = file {
                out(format!("  {}  blake3 {hash}", human(*size)));
                for h in holders {
                    match &h.path {
                        Some(p) => out(format!("  {} {}: {p}", h.how, h.who)),
                        None => out(format!("  {} on {}", h.how, h.who)),
                    }
                }
            }
        }
        locate::Found::TreeFolder { .. } => out("  a folder of the Drive tree"),
        locate::Found::TreeFile { size, hash, holders, .. } => {
            out(format!("  {}  blake3 {hash}", human(*size)));
            if holders.is_empty() {
                out("  no device or storage says it holds the bytes");
            }
            for h in holders {
                match &h.path {
                    Some(p) => out(format!("  {} {}: {p}", h.how, h.who)),
                    None => out(format!("  {} on {}", h.how, h.who)),
                }
            }
        }
        locate::Found::Missing { .. } => out("  not in the Drive"),
    }
}

async fn start(dir: &Path) -> Result<Agent, String> {
    Ok(Agent::start(dir, Config::load(dir)?).await)
}

/// Let edits reach the node before a one-off command exits.
async fn settle(a: &Agent) {
    if a.drive.is_online() {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

#[cfg(test)]
mod dir_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn dir_precedence_matches_drive_local() {
        let cli = Some(PathBuf::from("/cli"));
        let env = Some(OsString::from("/env-agent"));
        let xdg = Some(OsString::from("/xdg"));
        let home = Some(OsString::from("/home/u"));

        assert_eq!(
            resolve_dir(cli, env.clone(), xdg.clone(), home.clone()),
            PathBuf::from("/cli")
        );
        assert_eq!(
            resolve_dir(None, env, xdg.clone(), home.clone()),
            PathBuf::from("/env-agent")
        );
        assert_eq!(
            resolve_dir(None, Some(OsString::new()), xdg, home.clone()),
            PathBuf::from("/xdg/drive-agent")
        );
        assert_eq!(
            resolve_dir(None, None, None, home),
            PathBuf::from("/home/u/.local/share/drive-agent")
        );
    }
}

#[cfg(test)]
mod node_get_tests {
    use super::*;
    use axum::http::HeaderMap;

    #[tokio::test]
    async fn node_get_sends_the_device_session() {
        let app = axum::Router::new().route(
            "/search",
            axum::routing::get(|h: HeaderMap| async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                axum::Json(serde_json::json!({ "auth": auth }))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let cfg: Config = serde_json::from_value(serde_json::json!({
            "node": format!("ws://{addr}/sync"),
            "space": "me",
            "device_id": "d",
            "device_name": "d",
            "token": "secret-session",
        }))
        .unwrap();
        let v = node_get(&cfg, "/search", &[("q", "x")]).await.unwrap();
        assert_eq!(v["auth"], "Bearer secret-session");
    }
}
