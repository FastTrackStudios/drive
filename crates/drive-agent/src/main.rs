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
//! drive-agent [--dir DIR] mount MOUNTPOINT             # the space as a folder (runs until stopped)
//! ```
//!
//! What this command line prints is its output, on stdout; the mount's
//! diagnostics go through `tracing` to stderr.

mod agent;
mod fs;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "drive_agent=info".into()),
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
            out(format!("Created space {} with this device, {name} ({}).", cfg.space, cfg.device_id));
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
                out(format!("{:<20} {:<9} {:<8} {}{you}", d.name, d.kind, d.os, d.id));
            }
        }
        "storages" => {
            let a = start(dir).await?;
            let devices = space::devices(&a.space);
            out("the node             node     always");
            for s in space::storages(&a.space) {
                let by = devices.iter().find(|d| d.id == s.device).map_or("?".into(), |d| d.name.clone());
                out(format!("{:<20} {:<8} {} (run by {by})", s.name, s.scheme, s.location));
            }
        }
        "storage" if args.get(1).map(String::as_str) == Some("add") => {
            let kind = args.get(2).ok_or("storage add fs|s3|webdav NAME …")?;
            let name = args.get(3).ok_or("a name")?.clone();
            let get = |i: usize| args.get(i).cloned().unwrap_or_default();
            let config = match kind.as_str() {
                "fs" => {
                    let root = std::fs::canonicalize(get(4)).map_err(|e| format!("{}: {e}", get(4)))?;
                    StorageConfig::Fs { root: root.to_string_lossy().into_owned() }
                }
                "s3" => StorageConfig::S3 {
                    bucket: get(4),
                    endpoint: get(5),
                    access_key_id: get(6),
                    secret_access_key: get(7),
                    region: get(8),
                    root: "drive".into(),
                },
                "webdav" => StorageConfig::Webdav { endpoint: get(4), username: get(5), password: get(6), root: "drive".into() },
                other => return Err(format!("unknown storage kind {other}")),
            };
            // Check it works before registering it.
            let blobs = config.blobs().map_err(|e| e.to_string())?;
            blobs.put(b"drive".to_vec()).await.map_err(|e| format!("the storage did not take a test blob: {e}"))?;
            let mut cfg = Config::load(dir)?;
            let id = space::new_id();
            cfg.storages.push(OwnStorage { id: id.clone(), name: name.clone(), config: config.clone() });
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
            out(format!("Added {name} ({}: {}). Its credentials stay on this device.", config.scheme(), config.public()));
        }
        "put" => {
            let src = PathBuf::from(args.get(1).ok_or("put LOCAL_FILE [FOLDER]")?);
            let a = start(dir).await?;
            let folder = match args.get(2) {
                Some(p) => tree::resolve(&a.files, p).ok_or(format!("no folder {p}"))?.map(|e| e.id),
                None => None,
            };
            let name = src.file_name().and_then(|n| n.to_str()).ok_or("a file name")?.to_string();
            let e = a.put(&src, folder, &name).await?;
            settle(&a).await;
            out(format!("{} · {} bytes · {}", tree::path_of(&a.files, e.id), e.size, &e.hash[..12]));
        }
        "ls" => {
            let a = start(dir).await?;
            let folder = match args.get(1) {
                Some(p) => tree::resolve(&a.files, p).ok_or(format!("no folder {p}"))?.map(|e| e.id),
                None => None,
            };
            for e in tree::children(&a.files, folder) {
                let st = a.state(&e);
                let size = if e.dir { "".into() } else { human(e.size) };
                out(format!("{} {:<11} {:>9}  {}{}", st.glyph(), st.as_str(), size, e.name, if e.dir { "/" } else { "" }));
            }
        }
        "status" => {
            let a = start(dir).await?;
            let p = args.get(1).ok_or("status PATH")?;
            let e = tree::resolve(&a.files, p).ok_or(format!("no {p}"))?.ok_or("the top folder")?;
            out(format!("{}  {}", a.state(&e).as_str(), tree::path_of(&a.files, e.id)));
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
            let e = tree::resolve(&a.files, p).ok_or(format!("no {p}"))?.ok_or("the top folder")?;
            a.pin(&e, cmd == "pin").await?;
            settle(&a).await;
            out(format!("{}  {}", a.state(&e).as_str(), p));
        }
        "mount" => {
            let at = PathBuf::from(args.get(1).ok_or("mount MOUNTPOINT")?);
            // A mount left behind by an agent that stopped: clear it first.
            if let Err(e) = std::fs::metadata(&at)
                && e.raw_os_error() == Some(libc::ENOTCONN)
            {
                let _ = std::process::Command::new("fusermount3").args(["-uz"]).arg(&at).status();
            }
            std::fs::create_dir_all(&at).map_err(|e| e.to_string())?;
            let a = Arc::new(start(dir).await?);
            let space = a.cfg.lock().expect("cfg").space.clone();
            let fs = fs::DriveFs::new(a.clone(), tokio::runtime::Handle::current());
            let mut config = fuser::Config::default();
            config.mount_options = vec![
                fuser::MountOption::FSName(format!("drive:{space}")),
                fuser::MountOption::Subtype("drive".into()),
                fuser::MountOption::DefaultPermissions,
            ];
            // Right after a stale mount is cleared the kernel may still be
            // letting go: try again for a moment.
            let mut fs = Some(fs);
            let mut tries = 0;
            let session = loop {
                match fuser::spawn_mount(fs.take().expect("one try at a time"), &at, &config) {
                    Ok(s) => break s,
                    Err(e) if tries < 5 => {
                        tracing::warn!(error = %e, "mount failed; trying again");
                        tries += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        fs = Some(fs::DriveFs::new(a.clone(), tokio::runtime::Handle::current()));
                    }
                    Err(e) => return Err(format!("mount: {e}")),
                }
            };
            tracing::info!(mount.at = %at.display(), space.name = %space, "mounted");
            // Re-announce now and then (presence is per connection).
            // Stopped (Ctrl-C, or SIGTERM from systemd): unmount cleanly.
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|e| e.to_string())?;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = term.recv() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => a.announce(),
                }
            }
            drop(session);
            tracing::info!(mount.at = %at.display(), "unmounted");
        }
        _ => {
            out(include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")).map(|l| l.trim_start_matches("//!").trim_start_matches(' ')).collect::<Vec<_>>().join("\n"));
        }
    }
    Ok(())
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
