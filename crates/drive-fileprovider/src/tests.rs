//! The C calls against a stand-in agent: a socket that answers lines the
//! way the agent's app API does.

use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;

/// One test at a time: the library keeps one agent for the process.
static SERIAL: Mutex<()> = Mutex::new(());

/// Serve `path` like the agent: hello is fine, `fp_list` answers a page,
/// `fp_item` of `x` fails. After `drop_after` requests the connection is
/// closed (an agent that restarted). Returns how many connections came.
fn fake_agent(path: &Path, drop_after: usize) -> Arc<AtomicUsize> {
    let l = UnixListener::bind(path).unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let seen = conns.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut w = s.try_clone().unwrap();
            let mut served = 0;
            for line in BufReader::new(s).lines() {
                let Ok(line) = line else { break };
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = &req["id"];
                let answer = match req["op"].as_str() {
                    Some("hello") => serde_json::json!({ "id": id, "ok": null }),
                    Some("fp_list") => serde_json::json!({ "id": id, "ok": {
                        "items": [{ "id": "1@2", "parent": "root", "name": "Songs", "dir": true,
                                    "size": 0, "mtime": 5, "content": "", "meta": "root/Songs", "here": true }]
                    }}),
                    _ => serde_json::json!({ "id": id, "err": "no item x" }),
                };
                writeln!(w, "{answer}").unwrap();
                if req["op"] != "hello" {
                    served += 1;
                    if served >= drop_after {
                        break;
                    }
                }
            }
        }
    });
    conns
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: from dfp_call / dfp_last_error, freed once here.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { dfp_free(p) };
    Some(s)
}

fn last_error() -> String {
    take(dfp_last_error()).unwrap_or_default()
}

#[test]
fn a_request_goes_to_the_agent_and_its_answer_or_reason_comes_back() {
    let _one = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("app.sock");
    fake_agent(&sock, usize::MAX);
    assert_eq!(unsafe { dfp_connect(c(sock.to_str().unwrap()).as_ptr()) }, 0);

    let got = take(unsafe { dfp_call(c(r#"{"op":"fp_list","parent":"root"}"#).as_ptr()) }).unwrap();
    let page: drive_local::FpPage = serde_json::from_str(&got).unwrap();
    assert_eq!(page.items[0].name, "Songs");

    assert!(take(unsafe { dfp_call(c(r#"{"op":"fp_item","item":"x"}"#).as_ptr()) }).is_none());
    assert_eq!(last_error(), "no item x");
    assert!(take(unsafe { dfp_call(c("not json").as_ptr()) }).is_none());
    assert!(last_error().starts_with("not a request"));
    assert!(take(unsafe { dfp_call(std::ptr::null()) }).is_none());
    assert_eq!(last_error(), "no request");
}

#[test]
fn a_closed_connection_is_made_again_once() {
    let _one = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("app.sock");
    let conns = fake_agent(&sock, 1);
    connect(&sock).unwrap();
    for _ in 0..3 {
        let page = call(r#"{"op":"fp_list","parent":"root"}"#).unwrap();
        assert!(page.contains("Songs"));
    }
    assert!(conns.load(Ordering::SeqCst) >= 3, "reconnected after each close");
}

#[test]
fn no_agent_is_a_reason_not_a_crash() {
    let _one = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("app.sock");
    assert_eq!(unsafe { dfp_connect(c(missing.to_str().unwrap()).as_ptr()) }, -1);
    assert!(!last_error().is_empty());
}
