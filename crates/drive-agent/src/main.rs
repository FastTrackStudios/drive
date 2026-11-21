//! `drive-agent`: this device in a Drive space.
//!
//! ```text
//! drive-agent [--dir DIR] init  --node ws://host:8096/sync --space home --name "Studio PC"
//! drive-agent [--dir DIR] invite                       # a pairing code for another device
//! drive-agent [--dir DIR] join  --node URL --space home --name Laptop --code K7Q2XM
//! drive-agent [--dir DIR] devices | storages
//! drive-agent [--dir DIR] storage add fs     NAME PATH
//! drive-agent [--dir DIR] storage add s3     NAME BUCKET ENDPOINT KEY SECRET [REGION]
//! drive-agent [--dir DIR] storage add webdav NAME ENDPOINT [USER PASSWORD]
//! drive-agent [--dir DIR] put LOCAL_FILE [FOLDER]
//! drive-agent [--dir DIR] ls [FOLDER] | status PATH | pin PATH | unpin PATH
//! drive-agent [--dir DIR] mount MOUNTPOINT [--computers PATH]
//!                                   # the space as a folder, and every computer's
//!                                   # files under PATH (runs until stopped)
//! drive-agent [--dir DIR] serve     # no mount: index, answer the node, watch
//!
//! Every computer's files (indexed in place, searchable while it is off):
//! drive-agent [--dir DIR] location add NAME PATH        # a folder on this device
//! drive-agent [--dir DIR] location add NAME PATH --storage STORAGE   # in a storage it runs
//! drive-agent [--dir DIR] locations | volumes | index [NAME]
//! drive-agent [--dir DIR] search WORDS…                 # every device's files
//! drive-agent [--dir DIR] copies LOCATION PATH          # where else a file is
//!
//! What this device keeps:
//! drive-agent [--dir DIR] role storage-server|workstation   # keep everything, or the work at hand
//! drive-agent [--dir DIR] active [add|rm PATH]          # folders this device is working on
//! drive-agent [--dir DIR] place                         # one placement pass now
//! drive-agent [--dir DIR] available                     # put what should be available on the node now
//!
//! Protection (3-2-1 and the like):
//! drive-agent [--dir DIR] site DEVICE_OR_STORAGE SITE   # where it physically is
//! drive-agent [--dir DIR] policy add NAME [--copies 3 --media 2 --offsite 1] [--scope LOC,LOC]
//! drive-agent [--dir DIR] protection                    # every policy, checked
//! drive-agent [--dir DIR] backup LOCATION STORAGE       # copy a location into a storage
//! ```
//!
//! What this command line prints is its output, on stdout; the mount's
//! diagnostics go through `tracing` to stderr.

mod agent;
mod available;
mod computers;
mod daemon;
mod fs;
mod placement;
mod places;

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

fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("drive-agent")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "drive_agent=info".into()),
        )
        .init();
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
        let _ = writeln!(std::io::stderr(), "drive-agent: {e}");
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
        "invite" => {
            let a = start(dir).await?;
            let code = space::invite(&a.space, &a.device_id()).map_err(|e| e.to_string())?;
            a.commit_space();
            settle(&a).await;
            let cfg = a.cfg.lock().expect("cfg").clone();
            out(format!("Pairing code: {code}   (valid ten minutes)"));
            out(format!(
                "On the other device: drive-agent join --node {} --space {} --name NAME --code {code}",
                cfg.node, cfg.space
            ));
        }
        "join" => {
            let node = flag(args, "--node").ok_or("--node ws://host:port/sync")?;
            let space_name = flag(args, "--space").ok_or("--space NAME")?;
            let name = flag(args, "--name").unwrap_or_else(|| "Another computer".into());
            let code = flag(args, "--code").ok_or("--code CODE")?;
            let cfg = Config {
                node,
                space: space_name,
                device_id: space::new_id(),
                device_name: name.clone(),
                storages: Vec::new(),
                pins: Vec::new(),
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
            let folder = match args.get(2) {
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
            let folder = match args.get(1) {
                Some(p) => tree::resolve(&a.files, p)
                    .ok_or(format!("no folder {p}"))?
                    .map(|e| e.id),
                None => None,
            };
            for e in tree::children(&a.files, folder) {
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
        "mount" => {
            let at = PathBuf::from(args.get(1).ok_or("mount MOUNTPOINT")?);
            let a = Arc::new(start(dir).await?);
            let view = Arc::new(RwLock::new(computers::View::default()));
            let space = a.cfg.lock().expect("cfg").space.clone();
            let rt = tokio::runtime::Handle::current();
            let main = mount_fs(&at, &format!("drive:{space}"), || {
                fs::DriveFs::new(a.clone(), rt.clone())
            })
            .await?;
            let others = match flag(args, "--computers") {
                Some(p) => Some(
                    mount_fs(Path::new(&p), &format!("drive-computers:{space}"), || {
                        computers::ComputersFs::new(a.clone(), view.clone(), rt.clone())
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
        "serve" => {
            let a = Arc::new(start(dir).await?);
            let view = Arc::new(RwLock::new(computers::View::default()));
            run_until_stopped(daemon::serve(a, view)).await?;
        }
        "location" if args.get(1).map(String::as_str) == Some("add") => {
            let name = args.get(2).ok_or("location add NAME PATH")?.clone();
            let raw = args.get(3).ok_or("location add NAME PATH")?.clone();
            let a = start(dir).await?;
            let me = a.device_id();
            let (path, volume, storage) = match flag(args, "--storage") {
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
                    (raw.trim_matches('/').to_string(), String::new(), own.id)
                }
                None => {
                    let p = std::fs::canonicalize(&raw).map_err(|e| format!("{raw}: {e}"))?;
                    let vol = places::volume_of(&p);
                    (p.to_string_lossy().into_owned(), vol, String::new())
                }
            };
            let loc = space::Location {
                id: space::new_id(),
                name: name.clone(),
                device: me,
                path,
                volume,
                storage,
                mirror_of: String::new(),
                index: String::new(),
                files: 0,
                bytes: 0,
                built: 0,
            };
            space::add_location(&a.space, &loc).map_err(|e| e.to_string())?;
            a.commit_space();
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
            let idx = a
                .refresh_indexes()
                .await
                .remove(&loc.id)
                .ok_or("that location has no index yet")?;
            let e = drive_files::index::Tree::new(idx)
                .get(path)
                .cloned()
                .ok_or(format!("{path} is not in {lname}"))?;
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
            settle(&a).await;
            out(format!("{n} files put on the node"));
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
            let idx = a.cached_index(&src.id).ok_or("index the location first")?;
            let (mut sent, mut kept, mut bytes) = (0usize, 0usize, 0u64);
            for e in idx.entries.iter().filter(|e| !e.dir) {
                let key = format!("{base}/{}", e.path);
                if op
                    .stat(&key)
                    .await
                    .is_ok_and(|m| m.content_length() == e.size)
                {
                    kept += 1;
                    continue;
                }
                let from = places::inside(Path::new(&src.path), &e.path).ok_or("a bad path")?;
                let data = tokio::fs::read(&from)
                    .await
                    .map_err(|x| format!("{}: {x}", e.path))?;
                bytes += data.len() as u64;
                op.write(&key, data).await.map_err(|x| x.to_string())?;
                sent += 1;
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
    let r = reqwest::Client::new()
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
