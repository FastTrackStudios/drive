//! The C ABI Drive's macOS File Provider extension is built on
//! (docs/fileprovider.md).
//!
//! The extension answers the system's callbacks in Swift and asks the
//! agent everything else: what is in a folder, what changed, a file's
//! bytes, the writes. This is the one way it asks: a request as JSON in,
//! the answer as JSON out, over the agent's local app API (`drive-local`).
//! The requests are `drive_local::proto::Op`'s `fp_*` ops, so a new one
//! needs no change here.
//!
//! Strings out are C strings the caller frees with [`dfp_free`]; strings
//! in are borrowed for the call. Failure is `NULL` or `-1`, with the
//! reason in [`dfp_last_error`] until the next call on that thread (the
//! errno convention: the caller is C).

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use drive_local::proto::Op;
use drive_local::{Client, Error};

thread_local! {
    /// Per thread: the File Provider host calls the extension from several,
    /// and a shared slot would hand one caller another's failure.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// The agent's socket, and the connection to it once made.
static AGENT: Mutex<(Option<PathBuf>, Option<Client>)> = Mutex::new((None, None));

/// The app name the agent shows for this connection (`drive pins`).
const APP: &str = "macos-file-provider";

fn set_error(msg: impl Into<String>) {
    let owned = CString::new(msg.into().replace('\0', " ")).unwrap_or_else(|_| c"error".into());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(owned));
}

fn clear_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Run a call so that a panic never crosses into C: through an
/// `extern "C"` frame it would abort the extension, which the system
/// reports only as "the connection to service was invalidated".
fn guard<T>(what: &str, failed: T, call: impl FnOnce() -> T + std::panic::UnwindSafe) -> T {
    match std::panic::catch_unwind(call) {
        Ok(v) => v,
        Err(p) => {
            let why = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".into());
            set_error(format!("{what} panicked: {why}"));
            failed
        }
    }
}

fn arg(p: *const c_char, what: &str) -> Result<String, String> {
    if p.is_null() {
        return Err(format!("no {what}"));
    }
    // SAFETY: the caller passes a NUL-terminated string that lives for
    // the call (the header's contract).
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(str::to_string)
        .map_err(|_| format!("{what} is not UTF-8"))
}

fn out(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// Where the agent is; connect now.
pub fn connect(socket: &Path) -> Result<(), String> {
    let c = Client::connect_socket(socket, APP).map_err(|e| e.to_string())?;
    *AGENT.lock().expect("agent") = (Some(socket.to_path_buf()), Some(c));
    Ok(())
}

/// One request (an `Op` as JSON), its answer as JSON. A connection the
/// agent closed (it restarted) is made again once.
pub fn call(request: &str) -> Result<String, String> {
    let op: Op = serde_json::from_str(request).map_err(|e| format!("not a request: {e}"))?;
    let mut agent = AGENT.lock().expect("agent");
    let path = agent.0.clone().ok_or("dfp_connect was not called")?;
    for attempt in 0..2 {
        if agent.1.is_none() {
            agent.1 = Some(Client::connect_socket(&path, APP).map_err(|e| e.to_string())?);
        }
        let c = agent.1.as_ref().expect("connected");
        match c.call_op(op.clone()) {
            Ok(v) => return Ok(v.to_string()),
            Err(Error::Closed | Error::Io(_)) if attempt == 0 => agent.1 = None,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("the agent closed the connection".into())
}

/// # Safety
/// `socket_path` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dfp_connect(socket_path: *const c_char) -> c_int {
    guard("dfp_connect", -1, || {
        clear_error();
        match arg(socket_path, "socket path").and_then(|p| connect(Path::new(&p))) {
            Ok(()) => 0,
            Err(e) => {
                set_error(e);
                -1
            }
        }
    })
}

/// # Safety
/// `request_json` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dfp_call(request_json: *const c_char) -> *mut c_char {
    guard("dfp_call", std::ptr::null_mut(), || {
        clear_error();
        match arg(request_json, "request").and_then(|r| call(&r)) {
            Ok(answer) => out(answer),
            Err(e) => {
                set_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn dfp_last_error() -> *mut c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|c| out(c.to_string_lossy().into_owned()))
            .unwrap_or(std::ptr::null_mut())
    })
}

/// # Safety
/// `s` is a string this library returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dfp_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: made by CString::into_raw in `out`, freed once.
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests;
