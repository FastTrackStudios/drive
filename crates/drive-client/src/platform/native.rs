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

/// This computer's node (`DRIVE_NODE`, default the local one).
pub(crate) fn default_url() -> String {
    std::env::var("DRIVE_NODE")
        .ok()
        .filter(|v| !v.trim().is_empty())
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

/// Save a document's copy soon, in the order the saves are asked for.
pub(crate) fn save_doc(id: &str, bytes: Vec<u8>) {
    static STAMP: AtomicU64 = AtomicU64::new(0);
    let Some(d) = dir() else { return };
    let stamp = STAMP.fetch_add(1, Ordering::SeqCst) + 1;
    let newest = saves(id);
    let path = d.join("docs").join(drive_native::file_name(id));
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

/// Nothing hides natively (an app calls `Drive::save_now` before it quits).
pub(crate) fn on_hide(_f: impl Fn()) {}

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
            let ws = match tokio_tungstenite::connect_async(url.as_str()).await {
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
