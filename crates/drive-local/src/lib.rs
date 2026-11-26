//! Any Drive file as a real local path at disk speed, wherever it lives
//! (docs/app-files.md).
//!
//! The running Drive agent (`drive serve` or `drive mount`) serves a local
//! app API on loopback only: a Unix socket in its folder (`app.sock`, this
//! user only), or a `127.0.0.1` port with a token where Unix sockets are not
//! available. This crate is its client: blocking, std + serde only, so an
//! audio engine links it without an async runtime.
//!
//! ```no_run
//! let drive = drive_local::Client::connect("my-engine")?;
//! let f = drive.open("Samples:Kits/Acoustic/kick.wav")?;
//! // f.path is a real file: mmap it, stream it, hand it to a decoder.
//! println!("{} ({:?}, {} bytes)", f.path.display(), f.how, f.size);
//! # Ok::<(), drive_local::Error>(())
//! ```
//!
//! What an app opens or warms stays pinned in the agent's cache while its
//! [`Client`] lives (its lease): the agent never evicts it. Dropping the
//! client (or the process ending) ends every lease it held.

mod private;
pub mod proto;

pub use private::write_private;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use proto::{
    Event, FileProgress, FileState, FpChanges, FpFetched, FpItem, FpPage, Holder, How, LocalFile,
    OpenOptions, Pin, Priority, Progress, Resolved, Started, Status, Warm,
};
use proto::{Message, Op, PortFile, Request};

/// The socket's name in the agent's folder.
pub const SOCKET: &str = "app.sock";
/// The loopback port file's name in the agent's folder.
pub const PORT_FILE: &str = "app.port";

#[derive(Debug)]
pub enum Error {
    /// No agent answers in that folder.
    NotRunning(String),
    /// The agent would not let this connection in.
    Refused(String),
    /// The agent could not do what was asked (no such file, its computer
    /// is off, …): its reason.
    Failed(String),
    /// The connection to the agent ended.
    Closed,
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotRunning(why) => write!(f, "the Drive agent is not running: {why}"),
            Error::Refused(why) => write!(f, "the Drive agent refused: {why}"),
            Error::Failed(why) => f.write_str(why),
            Error::Closed => f.write_str("the connection to the Drive agent ended"),
            Error::Io(e) => write!(f, "talking to the Drive agent: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The agent's folder: `DRIVE_AGENT_DIR`, else the agent's default
/// (`$XDG_DATA_HOME/drive-agent`, else `~/.local/share/drive-agent`).
pub fn agent_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DRIVE_AGENT_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("drive-agent")
}

enum Conn {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

impl Conn {
    fn try_clone(&self) -> std::io::Result<Conn> {
        Ok(match self {
            #[cfg(unix)]
            Conn::Unix(s) => Conn::Unix(s.try_clone()?),
            Conn::Tcp(s) => Conn::Tcp(s.try_clone()?),
        })
    }

    fn shutdown(&self) {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            Conn::Tcp(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

impl std::io::Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => s.read(buf),
            Conn::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => s.write(buf),
            Conn::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => s.flush(),
            Conn::Tcp(s) => s.flush(),
        }
    }
}

type Answer = std::result::Result<serde_json::Value, String>;

struct Inner {
    write: Mutex<Conn>,
    /// The agent closed the connection (the reader stopped): nothing
    /// sent now would be answered.
    closed: std::sync::atomic::AtomicBool,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, Sender<Answer>>>,
    subs: Mutex<HashMap<u64, Sender<Event>>>,
}

impl Inner {
    fn send(&self, op: Op) -> Result<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.send_as(id, op)?;
        Ok(id)
    }

    fn send_as(&self, id: u64, op: Op) -> Result<()> {
        let mut line = serde_json::to_vec(&Request { id, op }).expect("a request serializes");
        line.push(b'\n');
        let mut w = self.write.lock().expect("conn");
        w.write_all(&line)?;
        w.flush()?;
        Ok(())
    }

    fn call_value(&self, op: Op, timeout: Option<Duration>) -> Result<serde_json::Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().expect("waiting").insert(id, tx);
        // Checked after the waiter is in: a reader that stopped before it
        // is seen here; one that stops after drops it, ending the wait.
        if self.closed.load(Ordering::SeqCst) {
            self.waiting.lock().expect("waiting").remove(&id);
            return Err(Error::Closed);
        }
        if let Err(e) = self.send_as(id, op) {
            self.waiting.lock().expect("waiting").remove(&id);
            return Err(e);
        }
        let got = match timeout {
            Some(t) => match rx.recv_timeout(t) {
                Ok(a) => a,
                Err(RecvTimeoutError::Timeout) => {
                    self.waiting.lock().expect("waiting").remove(&id);
                    return Err(Error::Failed("no answer in time".into()));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Closed),
            },
            None => rx.recv().map_err(|_| Error::Closed)?,
        };
        got.map_err(Error::Failed)
    }

    /// A request whose id also names its later events (a warm job, a
    /// watch): the events' channel is ready before the request is sent.
    fn call_sub(
        &self,
        op: impl FnOnce(u64) -> Op,
    ) -> Result<(u64, serde_json::Value, Receiver<Event>)> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (etx, erx) = mpsc::channel();
        self.subs.lock().expect("subs").insert(id, etx);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().expect("waiting").insert(id, tx);
        if self.closed.load(Ordering::SeqCst) {
            self.waiting.lock().expect("waiting").remove(&id);
            self.subs.lock().expect("subs").remove(&id);
            return Err(Error::Closed);
        }
        if let Err(e) = self.send_as(id, op(id)) {
            self.waiting.lock().expect("waiting").remove(&id);
            self.subs.lock().expect("subs").remove(&id);
            return Err(e);
        }
        match rx.recv().map_err(|_| Error::Closed)? {
            Ok(v) => Ok((id, v, erx)),
            Err(e) => {
                self.subs.lock().expect("subs").remove(&id);
                Err(Error::Failed(e))
            }
        }
    }
}

fn read_loop(inner: Arc<Inner>, conn: Conn) {
    let reader = BufReader::new(conn);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Message>(&line) else {
            continue;
        };
        match msg {
            Message::Answer { id, ok, err } => {
                if let Some(tx) = inner.waiting.lock().expect("waiting").remove(&id) {
                    let _ = tx.send(match err {
                        Some(e) => Err(e),
                        None => Ok(ok.unwrap_or(serde_json::Value::Null)),
                    });
                }
            }
            Message::Event(ev) => {
                let (id, last) = match &ev {
                    Event::Progress(p) => (p.job, false),
                    Event::Done(p) => (p.job, true),
                    Event::Changed { watch, .. } => (*watch, false),
                    Event::Gone { watch, .. } => (*watch, false),
                };
                let mut subs = inner.subs.lock().expect("subs");
                if let Some(tx) = subs.get(&id)
                    && (tx.send(ev).is_err() || last)
                {
                    subs.remove(&id);
                }
            }
        }
    }
    // The agent went away: everyone waiting learns it, and later calls
    // fail at once instead of waiting for an answer that cannot come.
    inner.closed.store(true, Ordering::SeqCst);
    inner.waiting.lock().expect("waiting").clear();
    inner.subs.lock().expect("subs").clear();
}

/// A connection to the running agent. Its leases last as long as it does.
pub struct Client {
    inner: Arc<Inner>,
    reader: Option<std::thread::JoinHandle<()>>,
    timeout: Option<Duration>,
}

impl Client {
    /// Connect to the agent in [`agent_dir`], as `app` (shown in `drive pins`).
    pub fn connect(app: &str) -> Result<Client> {
        Client::connect_in(&agent_dir(), app)
    }

    /// Connect to the agent whose folder is `dir`.
    pub fn connect_in(dir: &Path, app: &str) -> Result<Client> {
        let (conn, token) = dial(dir)?;
        Client::over(conn, token, app)
    }

    /// Connect to an agent's socket at `path` (one it also serves outside
    /// its folder, such as in a macOS app group's container for a
    /// sandboxed extension).
    #[cfg(unix)]
    pub fn connect_socket(path: &Path, app: &str) -> Result<Client> {
        let s = std::os::unix::net::UnixStream::connect(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                Error::NotRunning(path.display().to_string())
            } else {
                Error::Io(e)
            }
        })?;
        Client::over(Conn::Unix(s), None, app)
    }

    fn over(conn: Conn, token: Option<String>, app: &str) -> Result<Client> {
        let read = conn.try_clone()?;
        let inner = Arc::new(Inner {
            write: Mutex::new(conn),
            closed: std::sync::atomic::AtomicBool::new(false),
            next: AtomicU64::new(1),
            waiting: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        });
        let r = inner.clone();
        let reader = std::thread::Builder::new()
            .name("drive-local".into())
            .spawn(move || read_loop(r, read))?;
        let client = Client {
            inner,
            reader: Some(reader),
            timeout: None,
        };
        match client.inner.call_value(
            Op::Hello {
                app: app.to_string(),
                token,
            },
            Some(Duration::from_secs(10)),
        ) {
            Ok(_) => Ok(client),
            Err(Error::Failed(why)) => Err(Error::Refused(why)),
            Err(Error::Closed) => Err(Error::Refused("the agent closed the connection".into())),
            Err(e) => Err(e),
        }
    }

    /// Any request, its answer as JSON (a bridge to another language
    /// passes ops through without a type for each).
    pub fn call_op(&self, op: Op) -> Result<serde_json::Value> {
        self.inner.call_value(op, self.timeout)
    }

    /// Give up on any one answer after `t` (none: wait as long as it takes;
    /// a fetch of a big file from another computer takes what it takes).
    pub fn set_timeout(&mut self, t: Option<Duration>) {
        self.timeout = t;
    }

    fn call<T: serde::de::DeserializeOwned>(&self, op: Op) -> Result<T> {
        let v = self.inner.call_value(op, self.timeout)?;
        serde_json::from_value(v)
            .map_err(|e| Error::Failed(format!("an answer of the wrong shape: {e}")))
    }

    /// A file as a local path: this device's own file where it is, else a
    /// verified copy in the agent's cache (fetched now if needed). Held
    /// pinned while this client lives.
    pub fn open(&self, target: &str) -> Result<LocalFile> {
        self.open_with(target, OpenOptions::new())
    }

    pub fn open_with(&self, target: &str, opts: OpenOptions) -> Result<LocalFile> {
        self.call(Op::Open {
            target: target.to_string(),
            opts,
        })
    }

    /// Many files in one call (a session's media list), in order; each
    /// with its file or why not. Files already here answer without any
    /// fetch; the rest are fetched before the answer.
    pub fn resolve_many<S: AsRef<str>>(&self, targets: &[S]) -> Result<Vec<Resolved>> {
        self.call(Op::ResolveMany {
            targets: targets.iter().map(|t| t.as_ref().to_string()).collect(),
            opts: OpenOptions::new(),
        })
    }

    /// Start bringing files here; follow the job's progress with
    /// [`Job::recv`] or wait for it with [`Job::wait`].
    pub fn warm(&self, warm: Warm) -> Result<Job> {
        let (id, v, rx) = self.inner.call_sub(|_| Op::Warm(warm))?;
        let started: Started = serde_json::from_value(v)
            .map_err(|e| Error::Failed(format!("an answer of the wrong shape: {e}")))?;
        Ok(Job {
            id,
            started,
            rx,
            inner: self.inner.clone(),
        })
    }

    /// Be told when a file's content changes.
    pub fn watch(&self, target: &str) -> Result<Watch> {
        let t = target.to_string();
        let (id, _, rx) = self.inner.call_sub(|_| Op::Watch { target: t })?;
        Ok(Watch {
            id,
            rx,
            inner: self.inner.clone(),
        })
    }

    /// Let go of one target's lease.
    pub fn release(&self, target: &str) -> Result<()> {
        self.call::<serde_json::Value>(Op::Release {
            target: Some(target.to_string()),
        })
        .map(|_| ())
    }

    /// Let go of every lease this client holds.
    pub fn release_all(&self) -> Result<()> {
        self.call::<serde_json::Value>(Op::Release { target: None })
            .map(|_| ())
    }

    /// What every app holds pinned here.
    pub fn pins(&self) -> Result<Vec<Pin>> {
        self.call(Op::Pins)
    }

    /// The agent's cache: its use, budget, pins.
    pub fn status(&self) -> Result<Status> {
        self.call(Op::Status)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.inner.write.lock().expect("conn").shutdown();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// A warm job.
pub struct Job {
    id: u64,
    pub started: Started,
    rx: Receiver<Event>,
    inner: Arc<Inner>,
}

impl Job {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The next progress event (`Event::Done` is the last); `None` once
    /// the job is over or the connection ended.
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    pub fn recv_timeout(&self, t: Duration) -> Option<Event> {
        self.rx.recv_timeout(t).ok()
    }

    /// Wait for the end: the last progress.
    pub fn wait(self) -> Result<Progress> {
        loop {
            match self.rx.recv() {
                Ok(Event::Done(p)) => return Ok(p),
                Ok(_) => {}
                Err(_) => return Err(Error::Closed),
            }
        }
    }

    /// Stop it (what came stays; a half-fetched file resumes next time).
    pub fn cancel(&self) -> Result<()> {
        self.inner
            .call_value(Op::Cancel { job: self.id }, Some(Duration::from_secs(10)))
            .map(|_| ())
    }
}

/// A watch on one file; dropped, it stops.
pub struct Watch {
    id: u64,
    rx: Receiver<Event>,
    inner: Arc<Inner>,
}

impl Watch {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The next change (`Event::Changed` or `Event::Gone`).
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    pub fn recv_timeout(&self, t: Duration) -> Option<Event> {
        self.rx.recv_timeout(t).ok()
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.inner.subs.lock().expect("subs").remove(&self.id);
        let _ = self.inner.send(Op::Unwatch { watch: self.id });
    }
}

/// Reach the agent: its socket, else its loopback port.
fn dial(dir: &Path) -> Result<(Conn, Option<String>)> {
    let sock = dir.join(SOCKET);
    #[allow(unused_mut)]
    let mut why = format!("nothing answers at {}", sock.display());
    #[allow(unused_mut)]
    let mut refused = false;
    #[cfg(unix)]
    match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => return Ok((Conn::Unix(s), None)),
        // Not there, or a path too long for a socket (the agent then
        // serves on a loopback port), or a sandbox that may not connect()
        // to a Unix socket (macOS's File Provider extension): try the port.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            why = format!("{}: {e}", sock.display());
            refused = true;
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            why = format!("{}: {e}", sock.display());
        }
        Err(_) => {}
    }
    let port_file = dir.join(PORT_FILE);
    if let Ok(text) = std::fs::read_to_string(&port_file)
        && let Ok(p) = serde_json::from_str::<PortFile>(&text)
    {
        match std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, p.port)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok((Conn::Tcp(s), Some(p.token)));
            }
            Err(e) => why = format!("{why}; loopback port {}: {e}", p.port),
        }
    }
    if refused {
        return Err(Error::Refused(why));
    }
    Err(Error::NotRunning(format!(
        "{why} (start it with `drive serve`)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandbox that may not connect() to the socket (macOS's File
    /// Provider extension) still reaches the agent on its loopback port.
    #[cfg(unix)]
    #[test]
    fn a_refused_socket_falls_back_to_the_loopback_port() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join(SOCKET);
        let _unix = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return; // root ignores the mode: nothing to show here
        }
        let tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = tcp.local_addr().unwrap().port();
        std::fs::write(
            dir.path().join(PORT_FILE),
            serde_json::json!({ "port": port, "token": "t" }).to_string(),
        )
        .unwrap();
        let (conn, token) = dial(dir.path()).unwrap();
        assert!(matches!(conn, Conn::Tcp(_)));
        assert_eq!(token.as_deref(), Some("t"));
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_socket_without_a_port_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join(SOCKET);
        let _unix = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return;
        }
        assert!(matches!(dial(dir.path()), Err(Error::Refused(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_request_after_the_agent_closed_fails_at_once() {
        use std::io::{BufRead, BufReader};
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("app.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut lines = BufReader::new(s).lines();
            let hello: serde_json::Value =
                serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({ "id": hello["id"], "ok": null })
            )
            .unwrap();
            // Then the agent goes away.
        });
        let c = Client::connect_socket(&path, "test").unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let _ = tx.send(c.call_op(Op::Status).map(|_| ()));
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(Error::Closed | Error::Io(_))) => {}
            Ok(other) => panic!("{other:?}"),
            Err(_) => panic!("a request to a closed connection waited forever"),
        }
    }

    #[test]
    fn no_agent_is_a_clear_error() {
        let d = tempfile::tempdir().unwrap();
        match Client::connect_in(d.path(), "test") {
            Err(Error::NotRunning(why)) => assert!(why.contains("drive serve"), "{why}"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("connected to nothing"),
        }
        // A stale socket file with no one behind it is the same.
        #[cfg(unix)]
        {
            let l = std::os::unix::net::UnixListener::bind(d.path().join(SOCKET)).unwrap();
            drop(l);
            assert!(matches!(
                Client::connect_in(d.path(), "test"),
                Err(Error::NotRunning(_))
            ));
        }
    }
}
