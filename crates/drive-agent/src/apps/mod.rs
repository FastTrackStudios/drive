//! Apps get any Drive file as a real local path at disk speed, wherever it
//! lives (docs/app-files.md).
//!
//! The running agent serves a local app API, loopback only: a Unix socket
//! in its folder (`app.sock`, mode 600, peers of another user refused), or
//! a `127.0.0.1` port (`app.port` holds the port and a token, mode 600)
//! where Unix sockets are not available or when asked for. Apps speak to it
//! through `drive-local`; the wire is `drive_local::proto`.
//!
//! - [`resolve`]: a name to a local path (this device's own file in place;
//!   a tree file in the agent's cache; another computer's file fetched
//!   over iroh-blobs into the app cache);
//! - [`cache`]: the app cache (content-addressed, a budget, LRU of what no
//!   app holds);
//! - [`leases`]: what each connected app holds pinned;
//! - [`peer`]: asking another computer to make a file fetchable;
//! - [`warm`] and [`watch`]: bringing many files here ahead of use, and
//!   noticing new versions.

pub mod cache;
pub mod leases;
pub mod peer;
pub mod resolve;
pub mod warm;
pub mod watch;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use drive_local::proto::{Event, Message, Op, PortFile, Request};
use drive_local::{OpenOptions, Resolved, Status};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::Agent;
pub use leases::Leases;

/// The app cache's budget by default: 100 GiB.
pub const DEFAULT_BUDGET_GIB: u64 = 100;
/// A Unix socket's path must be shorter than this (macOS: 104 bytes).
const SOCKET_PATH_MAX: usize = 100;
/// Longest request line accepted.
const MAX_LINE: usize = 1 << 20;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// This device's app-files settings (in `config.json`; never synced).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// The app cache's budget, in GiB (default 100). What apps hold is
    /// kept even past it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_gib: Option<u64>,
    /// Also (on Windows: only) listen on this loopback port (0: any free
    /// one), written with a token to `app.port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Folders to put another `app.sock` in, for programs that cannot
    /// reach the agent's folder. (Drive's File Provider extension reaches
    /// the agent's own folder through a home-relative sandbox exception:
    /// macOS 15 refuses the app group's container to the agent.)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets_in: Vec<std::path::PathBuf>,
}

impl Settings {
    pub fn budget(&self) -> u64 {
        self.budget_gib
            .unwrap_or(DEFAULT_BUDGET_GIB)
            .saturating_mul(1 << 30)
    }

    /// Every folder an `app.sock` goes in besides the agent's own.
    pub fn socket_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = self.sockets_in.clone();
        dirs.dedup();
        dirs
    }
}

/// The app API's state in the running agent.
pub struct Apps {
    pub a: Arc<Agent>,
    pub cache: cache::AppCache,
    fetcher: OnceLock<Arc<drive_transfer::Fetcher>>,
    conns: Mutex<HashMap<drive_transfer::EndpointId, iroh::endpoint::Connection>>,
    /// One fetch per content at a time.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    next_conn: AtomicU64,
}

impl Apps {
    pub fn open(a: &Arc<Agent>) -> Result<Arc<Apps>, String> {
        let settings = a.cfg.lock().expect("cfg").apps.clone();
        let cache = cache::AppCache::open(&a.dir.join("app-cache"), settings.budget())
            .map_err(|e| format!("the app cache: {e}"))?;
        Ok(Arc::new(Apps {
            a: a.clone(),
            cache,
            fetcher: OnceLock::new(),
            conns: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        }))
    }

    fn lock(&self, hash: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("locks");
        locks.retain(|_, l| Arc::strong_count(l) > 1);
        locks.entry(hash.to_string()).or_default().clone()
    }

    pub fn status(&self) -> Status {
        let (files, bytes) = self.cache.stats();
        let (pf, pb) = self.cache.partial_stats();
        let pins = self.a.leases.pins();
        Status {
            cache_dir: self.cache.root().to_path_buf(),
            cache_files: files,
            cache_bytes: bytes,
            budget: self.cache.budget(),
            pinned_bytes: pins.iter().map(|p| p.size).sum(),
            partial_files: pf,
            partial_bytes: pb,
            apps: self.a.leases.apps(),
            pins,
        }
    }

    /// Open one target and hold it for `conn`.
    pub async fn open_for(
        &self,
        conn: u64,
        target: &str,
        opts: &OpenOptions,
        why: &str,
        on_bytes: resolve::OnBytes<'_>,
    ) -> Result<drive_local::LocalFile, String> {
        let src = self.resolve(target).await?;
        let f = self.materialize(&src, opts, on_bytes).await?;
        if opts.lease {
            self.hold_file(conn, target, why, &f)?;
        }
        Ok(f)
    }

    fn hold_file(
        &self,
        conn: u64,
        target: &str,
        why: &str,
        file: &drive_local::LocalFile,
    ) -> Result<(), String> {
        self.cache.hold_file(
            &self.a.leases,
            conn,
            leases::Item {
                target: target.to_string(),
                path: file.path.clone(),
                hash: file.hash.clone(),
                size: file.size,
                how: file.how,
                why: why.to_string(),
                since: now_ms(),
            },
        )
    }
}

// ── listening ───────────────────────────────────────────────────────────

/// Start serving apps: the socket (Unix), and the loopback port when set
/// (or where there are no Unix sockets). Runs until the process stops.
pub async fn serve(apps: Arc<Apps>) -> Result<(), String> {
    let dir = apps.a.dir.clone();
    // Serving the socket makes this process the running agent: `drive`
    // commands ask it for the location indexes, so it holds them itself.
    apps.a
        .index_db
        .claim()
        .await
        .map_err(|e| format!("the index database: {e}"))?;
    #[allow(unused_mut)]
    let mut port = apps.a.cfg.lock().expect("cfg").apps.port;
    // On macOS also a loopback port: Drive's File Provider extension is
    // sandboxed, and its sandbox refuses connect() to a Unix socket outside
    // its app group's container (EPERM), which macOS 15 in turn refuses to
    // the agent. It reads app.port (port and token) through its
    // home-relative exception and dials the port instead.
    if cfg!(target_os = "macos") {
        port = port.or(Some(0));
    }
    #[cfg(unix)]
    {
        let sock = dir.join(drive_local::SOCKET);
        if sock.as_os_str().len() >= SOCKET_PATH_MAX {
            // Socket paths are short (104 bytes on macOS): a folder this
            // deep serves on the loopback port instead.
            tracing::info!(
                apps.socket_len = sock.as_os_str().len(),
                "the agent's folder is too deep for a socket; apps reach it on a loopback port"
            );
            port = port.or(Some(0));
        } else {
            let l = bind_socket(&dir)?;
            tracing::info!(apps.socket = %sock.display(), "serving files to apps");
            let a = apps.clone();
            tokio::spawn(async move { accept_unix(a, l).await });
        }
        // The same API in the folders sandboxed programs can reach. One that
        // cannot be bound is logged and skipped: the agent's own socket
        // still serves everyone else.
        let extra = apps.a.cfg.lock().expect("cfg").apps.socket_dirs();
        for d in extra {
            if d.join(drive_local::SOCKET).as_os_str().len() >= SOCKET_PATH_MAX {
                tracing::warn!(apps.socket_dir = %d.display(), "too deep for a socket; skipped");
                continue;
            }
            match bind_socket(&d) {
                Ok(l) => {
                    tracing::info!(apps.socket = %d.join(drive_local::SOCKET).display(), "serving files to sandboxed apps");
                    let a = apps.clone();
                    tokio::spawn(async move { accept_unix(a, l).await });
                }
                Err(e) => {
                    tracing::warn!(apps.socket_dir = %d.display(), error = %e, "no socket there")
                }
            }
        }
    }
    #[cfg(not(unix))]
    let port = port.or(Some(0));
    if let Some(port) = port {
        let (l, token) = bind_port(&dir, port).await?;
        tracing::info!(
            apps.port = l.local_addr().map(|a| a.port()).unwrap_or(0),
            "serving files to apps on loopback"
        );
        let a = apps.clone();
        tokio::spawn(async move { accept_tcp(a, l, token).await });
    }
    Ok(())
}

#[cfg(unix)]
pub fn bind_socket(dir: &Path) -> Result<tokio::net::UnixListener, String> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let path = dir.join(drive_local::SOCKET);
    if let Ok(m) = std::fs::symlink_metadata(&path) {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Err(format!(
                "another agent already serves apps at {}",
                path.display()
            ));
        }
        // A socket left by an agent that stopped; nothing else is removed.
        if !m.file_type().is_socket() {
            return Err(format!("{} is not a socket; leaving it", path.display()));
        }
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let l =
        tokio::net::UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(l)
}

/// Whether a socket peer may talk to this agent: the same user only.
pub fn uid_allowed(peer: u32, me: u32) -> bool {
    peer == me
}

#[cfg(unix)]
async fn accept_unix(apps: Arc<Apps>, l: tokio::net::UnixListener) {
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    loop {
        let Ok((s, _)) = l.accept().await else {
            continue;
        };
        let peer = s.peer_cred().map(|c| c.uid());
        if !peer.as_ref().is_ok_and(|uid| uid_allowed(*uid, me)) {
            tracing::warn!(
                apps.outcome = "refused",
                apps.peer_uid = peer.ok(),
                "another user's program tried the app socket"
            );
            continue;
        }
        let a = apps.clone();
        tokio::spawn(async move { connection(a, s, None).await });
    }
}

/// Bind `127.0.0.1:port` and write `app.port` (port and a fresh token).
pub async fn bind_port(dir: &Path, port: u16) -> Result<(tokio::net::TcpListener, String), String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    bind_loopback(addr).await.and_then(|l| {
        let port = l.local_addr().map_err(|e| e.to_string())?.port();
        let token = new_token()?;
        let file = dir.join(drive_local::PORT_FILE);
        drive_local::write_private(
            &file,
            serde_json::to_string(&PortFile {
                port,
                token: token.clone(),
            })
            .expect("a port file serializes")
            .as_bytes(),
        )
        .map_err(|e| format!("{}: {e}", file.display()))?;
        Ok((l, token))
    })
}

/// A fresh `app.port` token: 32 bytes from the OS CSPRNG, as hex. TCP has
/// no peer-user check, so this is all that keeps another local user out.
fn new_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("random token: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Only loopback addresses are ever bound.
pub async fn bind_loopback(addr: std::net::SocketAddr) -> Result<tokio::net::TcpListener, String> {
    if !addr.ip().is_loopback() {
        return Err(format!("{addr} is not a loopback address: refused"));
    }
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("{addr}: {e}"))
}

async fn accept_tcp(apps: Arc<Apps>, l: tokio::net::TcpListener, token: String) {
    loop {
        let Ok((s, peer)) = l.accept().await else {
            continue;
        };
        if !peer.ip().is_loopback() {
            tracing::warn!(apps.outcome = "refused", apps.peer = %peer.ip(), "a non-loopback peer tried the app port");
            continue;
        }
        let _ = s.set_nodelay(true);
        let (a, t) = (apps.clone(), token.clone());
        tokio::spawn(async move { connection(a, s, Some(t)).await });
    }
}

// ── one connection ──────────────────────────────────────────────────────

/// What a connection's tasks share.
pub(crate) struct ConnCtx {
    pub id: u64,
    pub app: String,
    pub tx: mpsc::UnboundedSender<String>,
    pub apps: Arc<Apps>,
    /// Running jobs and watches, by their request id.
    pub tasks: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
}

impl ConnCtx {
    pub fn send(&self, m: &Message) {
        let _ = self
            .tx
            .send(serde_json::to_string(m).expect("a message serializes"));
    }

    pub fn answer(&self, id: u64, r: Result<serde_json::Value, String>) {
        self.send(&match r {
            Ok(v) => Message::Answer {
                id,
                ok: Some(v),
                err: None,
            },
            Err(e) => Message::Answer {
                id,
                ok: None,
                err: Some(e),
            },
        });
    }

    pub fn event(&self, e: Event) {
        self.send(&Message::Event(e));
    }
}

/// Serve one app until it disconnects; its leases, jobs and watches end
/// with it.
pub async fn connection<S>(apps: Arc<Apps>, stream: S, token: Option<String>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r);
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    let conn_id = apps.next_conn.fetch_add(1, Ordering::Relaxed);
    let mut buf = String::new();

    // The hello: who this is, and the token on a port.
    let first = read_line(&mut lines, &mut buf).await;
    let hello = first.and_then(|l| serde_json::from_str::<Request>(&l).ok());
    let app = match hello {
        Some(Request {
            id,
            op: Op::Hello { app, token: given },
        }) => {
            if token.is_some() && given != token {
                tracing::warn!(
                    apps.outcome = "refused",
                    "an app on the loopback port gave a wrong token"
                );
                let _ = tx.send(
                    serde_json::to_string(&Message::Answer {
                        id,
                        ok: None,
                        err: Some("a wrong token (read it from the agent's app.port)".into()),
                    })
                    .expect("a message"),
                );
                drop(tx);
                let _ = writer.await;
                return;
            }
            let _ = tx.send(
                serde_json::to_string(&Message::Answer {
                    id,
                    ok: Some(serde_json::json!({ "agent": env!("CARGO_PKG_VERSION"), "device": apps.a.device_id() })),
                    err: None,
                })
                .expect("a message"),
            );
            app
        }
        _ => {
            drop(tx);
            let _ = writer.await;
            return;
        }
    };
    apps.a.leases.open(conn_id, &app);
    tracing::info!(apps.app = %app, apps.conn = conn_id, "an app connected");
    let ctx = Arc::new(ConnCtx {
        id: conn_id,
        app: app.clone(),
        tx,
        apps: apps.clone(),
        tasks: Mutex::new(HashMap::new()),
    });
    let mut running = tokio::task::JoinSet::new();
    while let Some(line) = read_line(&mut lines, &mut buf).await {
        let Ok(req) = serde_json::from_str::<Request>(&line) else {
            continue;
        };
        while running.try_join_next().is_some() {}
        let c = ctx.clone();
        running.spawn(async move { handle(c, req).await });
    }
    // Gone: everything it started stops; its leases end.
    running.abort_all();
    for (_, t) in ctx.tasks.lock().expect("tasks").drain() {
        t.abort();
    }
    let ended = apps.a.leases.end(conn_id);
    tracing::info!(apps.app = %app, apps.conn = conn_id, apps.leases_ended = ended, "an app disconnected");
    drop(ctx);
    let _ = writer.await;
}

async fn read_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>, buf: &mut String) -> Option<String> {
    loop {
        buf.clear();
        let n = (&mut *r).take(MAX_LINE as u64).read_line(buf).await.ok()?;
        if n == 0 {
            return None;
        }
        let l = buf.trim();
        if !l.is_empty() {
            return Some(l.to_string());
        }
    }
}

use tokio::io::AsyncReadExt as _;

async fn handle(c: Arc<ConnCtx>, req: Request) {
    let id = req.id;
    let apps = c.apps.clone();
    match req.op {
        Op::Hello { .. } => c.answer(id, Ok(serde_json::Value::Null)),
        Op::Open { target, opts } => {
            let r = apps.open_for(c.id, &target, &opts, "open", &|_| {}).await;
            if let Err(e) = &r {
                tracing::debug!(apps.app = %c.app, error = %e, "an app's open failed");
            }
            c.answer(id, r.map(|f| serde_json::to_value(f).expect("a file")));
        }
        Op::ResolveMany { targets, opts } => {
            let out = resolve_many(&apps, c.id, &targets, &opts).await;
            c.answer(id, Ok(serde_json::to_value(out).expect("files")));
        }
        Op::Warm(w) => warm::start(c.clone(), id, w).await,
        Op::Cancel { job } => {
            let t = c.tasks.lock().expect("tasks").remove(&job);
            match t {
                Some(t) => {
                    t.abort();
                    c.event(Event::Done(drive_local::Progress {
                        job,
                        ..Default::default()
                    }));
                    c.answer(id, Ok(serde_json::json!({ "cancelled": job })));
                }
                None => c.answer(id, Err(format!("no job {job} running"))),
            }
        }
        Op::Watch { target } => watch::start(c.clone(), id, target).await,
        Op::Unwatch { watch } => {
            if let Some(t) = c.tasks.lock().expect("tasks").remove(&watch) {
                t.abort();
            }
            c.answer(id, Ok(serde_json::Value::Null));
        }
        Op::Release { target } => {
            let n = apps.a.leases.release(c.id, target.as_deref());
            c.answer(id, Ok(serde_json::json!({ "released": n })));
        }
        Op::Pins => c.answer(
            id,
            Ok(serde_json::to_value(apps.a.leases.pins()).expect("pins")),
        ),
        Op::Status => c.answer(id, Ok(serde_json::to_value(apps.status()).expect("status"))),
        // The Drive tree for a system file provider (crate::fileprovider).
        Op::FpItem { item } => c.answer(id, apps.a.fp_item(&item).map(json)),
        Op::FpList {
            parent,
            after,
            limit,
        } => c.answer(
            id,
            apps.a.fp_list(&parent, after.as_deref(), limit).map(json),
        ),
        Op::FpAnchor => c.answer(id, Ok(serde_json::json!({ "anchor": apps.a.fp_anchor() }))),
        Op::FpChanges { anchor } => c.answer(id, Ok(json(apps.a.fp_changes(&anchor)))),
        Op::FpFetch { item, into } => {
            let r = apps.a.fp_fetch(&item, &into).await;
            c.answer(id, r.map(json));
        }
        Op::FpCreate {
            parent,
            name,
            dir,
            from,
        } => {
            let r = apps.a.fp_create(&parent, &name, dir, from.as_deref()).await;
            c.answer(id, r.map(json));
        }
        Op::FpModify {
            item,
            name,
            parent,
            from,
        } => {
            let r = apps
                .a
                .fp_modify(&item, name.as_deref(), parent.as_deref(), from.as_deref())
                .await;
            c.answer(id, r.map(json));
        }
        Op::Index(q) => c.answer(id, crate::indexes::answer(&apps.a, q).await),
        Op::FpDelete { item } => c.answer(
            id,
            apps.a.fp_delete(&item).map(|()| serde_json::Value::Null),
        ),
    }
}

fn json<T: Serialize>(v: T) -> serde_json::Value {
    serde_json::to_value(v).expect("an answer serializes")
}

/// Every target in one go: what is here answers at once (in parallel);
/// the rest are fetched a few at a time.
pub async fn resolve_many(
    apps: &Arc<Apps>,
    conn: u64,
    targets: &[String],
    opts: &OpenOptions,
) -> Vec<Resolved> {
    use futures::StreamExt;
    let started = std::time::Instant::now();
    let out: Vec<Resolved> = futures::stream::iter(targets.iter().cloned())
        .map(|t| {
            let apps = apps.clone();
            let opts = opts.clone();
            async move {
                match apps
                    .open_for(conn, &t, &opts, "resolve_many", &|_| {})
                    .await
                {
                    Ok(f) => Resolved {
                        target: t,
                        file: Some(f),
                        error: None,
                    },
                    Err(e) => Resolved {
                        target: t,
                        file: None,
                        error: Some(e),
                    },
                }
            }
        })
        .buffered(16)
        .collect()
        .await;
    tracing::debug!(
        apps.files = out.len(),
        apps.failed = out.iter().filter(|r| r.error.is_some()).count(),
        apps.ms = started.elapsed().as_millis() as u64,
        "resolve_many"
    );
    out
}

/// The CLI's way to the running agent.
pub fn client(dir: &Path, app: &str) -> Result<drive_local::Client, String> {
    drive_local::Client::connect_in(dir, app).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
