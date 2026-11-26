//! In a native app (a desktop or phone build): a WebSocket on Tokio, its
//! timers, and a folder for who this is and the documents' copies.
//!
//! The client runs its socket and timers on a small runtime of its own, so
//! it works the same whether or not the app has one (a Blitz window runs
//! none of its own).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::Drive;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("drive-client")
            .enable_all()
            .build()
            .expect("drive-client's runtime starts")
    })
}

pub(crate) fn random() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

pub(crate) async fn sleep(ms: u32) {
    // On the client's runtime, so it works from any executor (a Blitz
    // window's, which has no Tokio timer of its own).
    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    runtime().spawn(async move {
        tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
        let _ = tx.send(());
    });
    let _ = rx.await;
}

pub(crate) fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    runtime().spawn(fut);
}

pub(crate) async fn on_runtime<T: Send + 'static>(
    fut: impl Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = futures_channel::oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.ok()
}

pub(crate) fn after(ms: u32, f: impl FnOnce() + Send + 'static) {
    runtime().spawn(async move {
        tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
        f();
    });
}

/// The node this computer uses: `DRIVE_NODE`, else the one this computer's
/// agent joined (its config.json's `node`), else a node on this computer.
/// A desktop app on a computer without a node of its own (a Mac beside
/// its agent) thus reaches the same node as its agent.
pub(crate) fn default_url() -> String {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let agent_config = agent_dir(env("DRIVE_AGENT_DIR"), env("XDG_DATA_HOME"), env("HOME"))
        .and_then(|d| std::fs::read_to_string(d.join("config.json")).ok());
    node_url(
        std::env::var("DRIVE_NODE").ok().as_deref(),
        agent_config.as_deref(),
    )
}

/// The agent's folder, found as the agent finds it: `DRIVE_AGENT_DIR`,
/// else `drive-agent` in the user's data folder.
fn agent_dir(
    dir: Option<std::ffi::OsString>,
    xdg_data: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(d) = dir {
        return Some(PathBuf::from(d));
    }
    let base = xdg_data
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("drive-agent"))
}

fn node_url(env: Option<&str>, agent_config: Option<&str>) -> String {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    agent_config
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .and_then(|c| c.get("node")?.as_str().map(str::to_owned))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "ws://127.0.0.1:8096/sync".to_string())
}

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

pub(crate) fn set_data_dir(dir: PathBuf) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// Where this device's copies are: set by the app, `DRIVE_CLIENT_DIR`, or
/// the user's data folder.
pub(crate) fn dir() -> Option<PathBuf> {
    if let Some(d) = DIR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(d);
    }
    if let Some(d) = std::env::var_os("DRIVE_CLIENT_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        home.map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".local").join("share")))
    };
    base.map(|b| b.join("drive").join("client"))
}

pub(crate) fn load_me() -> Option<String> {
    std::fs::read_to_string(dir()?.join("me.json")).ok()
}

pub(crate) fn save_me(json: &str) {
    if let Some(d) = dir() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join("me.json"), json);
    }
}

pub(crate) async fn load_doc(id: &str) -> Option<Vec<u8>> {
    let path = drive_native::copy_path(&dir()?.join("docs"), id);
    tokio::fs::read(path).await.ok()
}

/// Each document's newest save written so far, so saves of one document
/// are written one at a time and an older one never replaces a newer.
fn saves(id: &str) -> Arc<tokio::sync::Mutex<u64>> {
    static SAVES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<u64>>>>> = OnceLock::new();
    let mut saves = SAVES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    saves.entry(id.to_string()).or_default().clone()
}

/// Save a document's copy soon, in the order the saves are asked for;
/// while the process exits, now (its runtime's tasks will not run again).
pub(crate) fn save_doc(id: &str, bytes: Vec<u8>) {
    static STAMP: AtomicU64 = AtomicU64::new(0);
    let Some(d) = dir() else { return };
    let path = d.join("docs").join(drive_native::file_name(id));
    if EXITING.load(Ordering::SeqCst) {
        if let Err(e) = write_whole_now(&path, &bytes) {
            tracing::warn!(path = %path.display(), error = %e, "a document's copy was not saved");
        }
        return;
    }
    let stamp = STAMP.fetch_add(1, Ordering::SeqCst) + 1;
    let newest = saves(id);
    spawn(async move {
        let mut newest = newest.lock().await;
        if *newest > stamp {
            return;
        }
        match write_whole(&path, &bytes).await {
            Ok(()) => *newest = stamp,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "a document's copy was not saved")
            }
        }
    });
}

/// Write `path` whole or not at all: to a temporary file of this write's
/// own (other apps on this machine keep their copies here too), then
/// renamed over it.
async fn write_whole(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{:016x}.tmp", (random() * u64::MAX as f64) as u64));
    let tmp = path.with_file_name(name);
    let written = async {
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    written
}

/// [`write_whole`], blocking: for the exit hook.
fn write_whole_now(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{:016x}.tmp", (random() * u64::MAX as f64) as u64));
    let tmp = path.with_file_name(name);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Set once the process is exiting: saves are written at once.
static EXITING: AtomicBool = AtomicBool::new(false);

type Hook = Box<dyn Fn() + Send>;

fn exit_hooks() -> &'static Mutex<Vec<Hook>> {
    static HOOKS: OnceLock<Mutex<Vec<Hook>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

extern "C" fn run_exit_hooks() {
    EXITING.store(true, Ordering::SeqCst);
    let hooks = std::mem::take(&mut *exit_hooks().lock().unwrap_or_else(|e| e.into_inner()));
    // On a thread of their own: by now this thread's thread-locals may be
    // gone, and a panic here could not unwind.
    let _ = std::thread::Builder::new()
        .name("drive-client-exit".into())
        .spawn(move || {
            for h in hooks {
                h();
            }
        })
        .map(|t| t.join());
}

/// Nothing hides natively; a native app's last moment is its exit. `f`
/// runs then (`exit`, or returning from `main`), so edits from just
/// before quitting are kept even when the app never calls
/// `Drive::save_now` (drive#62).
pub(crate) fn on_hide(f: impl Fn() + Send + 'static) {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    exit_hooks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(f));
    REGISTERED.call_once(|| {
        // SAFETY: `run_exit_hooks` is a plain `extern "C" fn()` that lives
        // as long as the process.
        unsafe {
            libc::atexit(run_exit_hooks);
        }
    });
}

/// Nothing older to drop natively.
pub(crate) fn drop_old_copies() {}

enum Out {
    Bytes(Vec<u8>),
    Close,
}

/// A WebSocket to the node, on the client's runtime.
pub(crate) struct Socket {
    tx: mpsc::UnboundedSender<Out>,
    open: Arc<AtomicBool>,
}

impl Socket {
    /// Open it, its events going to `drive`.
    pub(crate) fn open(url: &str, drive: Drive, generation: u32) -> Option<Socket> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
        let open = Arc::new(AtomicBool::new(false));
        let flag = open.clone();
        let url = url.to_string();
        runtime().spawn(async move {
            // wss:// to a node behind TLS: rustls has two crypto backends
            // compiled in here, so name one.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let ws = match tokio_tungstenite::connect_async_with_config(
                url.as_str(),
                Some(
                    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                        .max_message_size(Some(drive_proto::MAX_MESSAGE))
                        .max_frame_size(Some(drive_proto::MAX_MESSAGE)),
                ),
                false,
            )
            .await
            {
                Ok((ws, _)) => ws,
                Err(e) => {
                    tracing::debug!(drive.url = %url, error = %e, "connect failed");
                    drive.socket_closed(generation);
                    return;
                }
            };
            let (mut sink, mut stream) = ws.split();
            flag.store(true, Ordering::SeqCst);
            drive.socket_opened(generation);
            loop {
                tokio::select! {
                    out = rx.recv() => match out {
                        Some(Out::Bytes(b)) => {
                            if sink.send(Message::binary(b)).await.is_err() {
                                break;
                            }
                        }
                        Some(Out::Close) | None => {
                            let _ = sink.close().await;
                            break;
                        }
                    },
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Binary(b))) => drive.socket_message(generation, &b),
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
            flag.store(false, Ordering::SeqCst);
            drive.socket_closed(generation);
        });
        Some(Socket { tx, open })
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    pub(crate) fn send(&self, bytes: Vec<u8>) -> bool {
        self.tx.send(Out::Bytes(bytes)).is_ok()
    }

    pub(crate) fn close(&self) {
        let _ = self.tx.send(Out::Close);
    }
}

#[cfg(test)]
mod node_tests {
    use super::{agent_dir, node_url};
    use std::path::PathBuf;

    const LOCAL: &str = "ws://127.0.0.1:8096/sync";

    #[test]
    fn drive_node_wins_over_the_agents_node() {
        let config = r#"{"node":"wss://node.example/sync"}"#;
        assert_eq!(node_url(Some("ws://other/sync"), Some(config)), "ws://other/sync");
    }

    #[test]
    fn without_drive_node_the_agents_node_is_used() {
        let config = r#"{"device_id":"d","node":"wss://node.example/sync"}"#;
        assert_eq!(node_url(None, Some(config)), "wss://node.example/sync");
        assert_eq!(node_url(Some("  "), Some(config)), "wss://node.example/sync");
    }

    #[test]
    fn without_either_it_is_this_computers_node() {
        assert_eq!(node_url(None, None), LOCAL);
        assert_eq!(node_url(None, Some("not json")), LOCAL);
        assert_eq!(node_url(None, Some(r#"{"node":""}"#)), LOCAL);
    }

    #[test]
    fn the_agents_folder_is_found_as_the_agent_finds_it() {
        assert_eq!(
            agent_dir(Some("/d".into()), Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/d"))
        );
        assert_eq!(
            agent_dir(None, Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/x/drive-agent"))
        );
        assert_eq!(
            agent_dir(None, None, Some("/h".into())),
            Some(PathBuf::from("/h/.local/share/drive-agent"))
        );
        assert_eq!(agent_dir(None, None, None), None);
    }
}
