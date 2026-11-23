//! In a native app (a desktop or phone build of the same app): a JSON file
//! beside the client's copies for the store, the node's HTTP address for
//! calls, and a restart where a page would reload.
//!
//! There is no page, so no location: an app is always at its root, and
//! the issuer's sign-in (a redirect back to `/auth/callback`) is not
//! available here yet. See `crate::auth` for how a native app is signed in.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// (month 0–11, day, year), in UTC (no time zone database here).
pub fn date_parts(ms: f64) -> (u32, u32, i64) {
    super::date_parts_utc(ms)
}

pub fn random_hex32() -> String {
    let mut b = [0u8; 32];
    if getrandom::fill(&mut b).is_err() {
        return String::new();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The store: one JSON object in a file, read and written whole (it holds
/// a few small keys: the org, the tokens).
fn store_path() -> Option<PathBuf> {
    Some(drive_client::data_dir()?.join("app-store.json"))
}

static STORE: Mutex<()> = Mutex::new(());

fn read_store() -> BTreeMap<String, String> {
    store_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_store(m: &BTreeMap<String, String>) {
    let Some(p) = store_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(j) = serde_json::to_string(m) {
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

pub fn get(key: &str) -> Option<String> {
    let _g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    read_store().get(key).cloned()
}

pub fn set(key: &str, value: &str) {
    let _g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = read_store();
    m.insert(key.to_string(), value.to_string());
    write_store(&m);
}

pub fn remove(key: &str) {
    let _g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = read_store();
    if m.remove(key).is_some() {
        write_store(&m);
    }
}

/// The node's HTTP address stands in for the page's origin.
pub fn origin() -> String {
    drive_client::node_http()
}

pub fn href() -> String {
    format!("{}{}", origin(), crate::mount::page("/"))
}

/// An app is at its root.
pub fn pathname() -> String {
    crate::mount::page("/")
}

pub fn search() -> String {
    String::new()
}

/// Leaving for another address: an https page opens in the system's
/// browser; anything else (the app's own pages) is a restart.
pub fn go(url: &str) {
    if url.starts_with("https://") || url.starts_with("http://") {
        open_in_browser(url);
    } else {
        reload();
    }
}

pub fn replace(url: &str) {
    go(url);
}

/// A page reloads to start over (another org, signed out): the app starts
/// itself again.
pub fn reload() {
    if let Ok(exe) = std::env::current_exe() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if std::process::Command::new(exe).args(args).spawn().is_ok() {
            std::process::exit(0);
        }
    }
}

pub fn reload_without_query() {
    reload();
}

/// No cookies: a native app sends its token in a header.
pub fn set_cookie(_cookie: &str) {}

/// The client connects to its configured node.
pub fn sync_url() -> Option<String> {
    None
}

/// No page scripts natively.
pub fn eval_f64(_js: &str) -> Option<f64> {
    None
}

pub fn eval(_js: &str) {}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// No system clipboard here yet (the app's renderer has its own; see
/// app-shell). TODO(native clipboard).
pub fn copy_text(_text: &str) {}

/// One HTTP request with a body of bytes: its status and the answer's
/// bytes. A root path is the node's (`DRIVE_NODE`'s HTTP address), reached
/// under the app's mount.
pub async fn http_bytes(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Option<(u16, Vec<u8>)> {
    let full = if url.starts_with('/') {
        format!("{}{}", origin(), crate::mount::api(url))
    } else {
        url.to_string()
    };
    let method = reqwest::Method::from_bytes(method.as_bytes()).ok()?;
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    drive_client::on_runtime(async move {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        let client = CLIENT.get_or_init(reqwest::Client::new);
        let mut req = client.request(method, full);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req.send().await.ok()?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        Some((status, bytes))
    })
    .await
    .flatten()
}

/// One HTTP request: its status and body. A root path is the node's
/// (`DRIVE_NODE`'s HTTP address), reached under the app's mount.
pub async fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u16, String)> {
    let (status, bytes) =
        http_bytes(method, url, headers, body.map(|b| b.as_bytes().to_vec())).await?;
    Some((status, String::from_utf8_lossy(&bytes).into_owned()))
}
