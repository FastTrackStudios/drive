//! `drive-native`: Drive's client for native programs — the agent, servers,
//! command-line tools. The same protocol as the browser client
//! (`drive-client`), on tokio and a websocket, with each open document's
//! copy kept on disk so it opens offline and edits made offline are sent
//! on reconnect.
//!
//! ```ignore
//! let drive = Drive::connect("ws://127.0.0.1:8096/sync", "laptop", dir).await;
//! let doc = drive.open("drive:files:me").await;
//! drive.synced("drive:files:me").await;          // the node's copy is in
//! doc.get_map("x").insert("k", 1)?;
//! drive.commit("drive:files:me");                // send (and save) it
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_proto::{DocId, DocInfo, Peer, ToClient, ToNode};
use futures_util::{SinkExt, StreamExt};
pub use loro;
use loro::{ExportMode, LoroDoc, Subscription, VersionVector};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

mod connection;

/// What happened.
#[derive(Clone, Debug)]
pub enum Event {
    Online(bool),
    /// A document changed, here or elsewhere.
    Changed(DocId),
    Presence {
        doc: DocId,
        peer: Peer,
        name: String,
        json: Option<String>,
    },
    Docs {
        prefix: String,
        docs: Vec<DocInfo>,
    },
    /// The node asks this agent for a file in one of its locations; answer
    /// with [`Drive::fetched`].
    Fetch {
        req: u64,
        location: String,
        path: String,
    },
    /// The node asks this agent to make sidecars of a file in one of its
    /// locations; answer with [`Drive::fetched`] (the content key).
    Sidecars {
        req: u64,
        location: String,
        path: String,
        kinds: Vec<String>,
        force: bool,
    },
    /// The node asks to look at this device's disks (read-only); answer
    /// with [`Drive::browsed`].
    Browse {
        req: u64,
        ask: drive_proto::Browse,
    },
}

struct OpenDoc {
    doc: LoroDoc,
    outbox: Arc<Mutex<Vec<Vec<u8>>>>,
    _sub: Subscription,
    synced: watch::Sender<bool>,
    presence: Option<String>,
    /// Other peers' presence here: peer → (name, json).
    others: HashMap<Peer, (String, String)>,
}

struct Inner {
    name: String,
    dir: PathBuf,
    docs: Mutex<HashMap<DocId, OpenDoc>>,
    /// To the connection task, while connected.
    tx: Mutex<Option<mpsc::UnboundedSender<ToNode>>>,
    online: watch::Sender<bool>,
    events: broadcast::Sender<Event>,
    save: Notify,
    /// Set by [`Drive::register_agent`]; sent again on every reconnect.
    agent: Mutex<Option<(String, String)>>,
    /// The issuer's token for the handshake (a device signed in as its
    /// person); none on the node's own computer.
    token: Option<String>,
    /// Set when the last [`Drive`] handed out is dropped: the connection
    /// and saver tasks end.
    stop: watch::Sender<bool>,
}

/// A connection to a Drive node and the documents open on it.
///
/// The second field is held by every handle given to callers, not by the
/// background tasks: when the last caller's handle goes, the tasks stop
/// and the connection closes, instead of running (and holding every open
/// document) for as long as the process does (drive#63).
#[derive(Clone)]
pub struct Drive(
    Arc<Inner>,
    #[expect(dead_code, reason = "held for its Drop")] Option<Arc<Life>>,
);

/// Ends the background tasks when dropped.
struct Life(std::sync::Weak<Inner>);

impl Drop for Life {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.stop.send_replace(true);
            *inner.tx.lock().expect("tx") = None;
        }
    }
}

/// The file a document's copy is kept in (also how `drive-client` names
/// a native app's copies): lowercase letters, digits and `-` as they are,
/// every other byte `%` and its hex, so two ids never share a file (on a
/// case-insensitive disk either).
pub fn file_name(id: &str) -> String {
    let mut name = String::with_capacity(id.len() + 5);
    for b in id.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' {
            name.push(b as char);
        } else {
            name.push_str(&format!("%{b:02x}"));
        }
    }
    name + ".loro"
}

/// The name copies had before [`file_name`] kept ids apart: every other
/// character `_` (so `a/b`, `a:b` and `a_b` shared one file).
fn legacy_file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        + ".loro"
}

/// The copy of `id` to read in `docs`: its own file, else one kept under
/// the name copies had before (written again under its own on save).
pub fn copy_path(docs: &std::path::Path, id: &str) -> PathBuf {
    let own = docs.join(file_name(id));
    let legacy = docs.join(legacy_file_name(id));
    if !own.exists() && legacy.exists() {
        legacy
    } else {
        own
    }
}

impl Drive {
    /// Connect (and keep reconnecting) to the node at `url`
    /// (`ws://host:port/sync`). Documents' copies live under `dir`.
    pub async fn connect(url: &str, name: &str, dir: impl Into<PathBuf>) -> Drive {
        Drive::connect_with(url, name, dir, None).await
    }

    /// [`Drive::connect`], signed in: `token` goes in every handshake.
    pub async fn connect_with(
        url: &str,
        name: &str,
        dir: impl Into<PathBuf>,
        token: Option<String>,
    ) -> Drive {
        let dir = dir.into();
        let _ = tokio::fs::create_dir_all(dir.join("docs")).await;
        let (events, _) = broadcast::channel(1024);
        let inner = Arc::new(Inner {
            name: name.to_string(),
            dir,
            docs: Mutex::new(HashMap::new()),
            tx: Mutex::new(None),
            online: watch::channel(false).0,
            events,
            save: Notify::new(),
            agent: Mutex::new(None),
            token,
            stop: watch::channel(false).0,
        });
        let tasks = Drive(inner.clone(), None);
        tokio::spawn(tasks.clone().run(url.to_string()));
        tokio::spawn(tasks.saver());
        let life = Arc::new(Life(Arc::downgrade(&inner)));
        Drive(inner, Some(life))
    }

    /// Until the last caller's handle is dropped.
    async fn stopped(&self) {
        let mut rx = self.0.stop.subscribe();
        let _ = rx.wait_for(|s| *s).await;
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    pub fn is_online(&self) -> bool {
        *self.0.online.borrow()
    }

    /// Wait until connected.
    pub async fn online(&self) {
        let mut rx = self.0.online.subscribe();
        let _ = rx.wait_for(|o| *o).await;
    }

    /// Open a document: its copy on disk now, the node's as it arrives.
    pub async fn open(&self, id: &str) -> LoroDoc {
        if let Some(d) = self.0.docs.lock().expect("docs").get(id) {
            return d.doc.clone();
        }
        let bytes = tokio::fs::read(copy_path(&self.0.dir.join("docs"), id)).await;
        let mut docs = self.0.docs.lock().expect("docs");
        // Another open of the same id may have finished while this one
        // read the copy: both get its document, not one each (drive#63).
        if let Some(d) = docs.get(id) {
            return d.doc.clone();
        }
        let doc = LoroDoc::new();
        drive_spine::history::prepare(&doc, &self.0.name);
        if let Ok(bytes) = bytes {
            let _ = doc.import(&bytes);
        }
        let outbox = Arc::new(Mutex::new(Vec::new()));
        let ob = outbox.clone();
        let sub = doc.subscribe_local_update(Box::new(move |b| {
            if let Ok(mut o) = ob.lock() {
                o.push(b.clone());
            }
            true
        }));
        let vv = doc.oplog_vv().encode();
        docs.insert(
            id.to_string(),
            OpenDoc {
                doc: doc.clone(),
                outbox,
                _sub: sub,
                synced: watch::channel(false).0,
                presence: None,
                others: HashMap::new(),
            },
        );
        drop(docs);
        self.send(ToNode::Join {
            doc: id.to_string(),
            version_vector: vv,
        });
        doc
    }

    /// Wait until the node has answered this document's join (so it holds
    /// everything the node has).
    pub async fn synced(&self, id: &str) {
        let rx = self
            .0
            .docs
            .lock()
            .expect("docs")
            .get(id)
            .map(|d| d.synced.subscribe());
        if let Some(mut rx) = rx {
            let _ = rx.wait_for(|s| *s).await;
        }
    }

    /// Commit a document's edits, send them, and save its copy soon.
    pub fn commit(&self, id: &str) {
        let pending = {
            let docs = self.0.docs.lock().expect("docs");
            let Some(d) = docs.get(id) else { return };
            drive_spine::history::commit(&d.doc, &self.0.name);
            d.outbox
                .lock()
                .map(|mut o| std::mem::take(&mut *o))
                .unwrap_or_default()
        };
        for bytes in pending.into_iter().filter(|b| !b.is_empty()) {
            self.send(ToNode::Update {
                doc: id.to_string(),
                bytes,
            });
        }
        let _ = self.0.events.send(Event::Changed(id.to_string()));
        self.0.save.notify_one();
    }

    pub fn set_presence(&self, id: &str, json: String) {
        if let Some(d) = self.0.docs.lock().expect("docs").get_mut(id) {
            d.presence = Some(json.clone());
        }
        self.send(ToNode::Presence {
            doc: id.to_string(),
            json,
        });
    }

    /// Say this connection is `device`'s agent in `space`, so the node may
    /// ask it for files in its locations ([`Event::Fetch`]).
    pub fn register_agent(&self, space: &str, device: &str) {
        *self.0.agent.lock().expect("agent") = Some((space.to_string(), device.to_string()));
        self.send(ToNode::Agent {
            space: space.to_string(),
            device: device.to_string(),
        });
    }

    /// Answer a [`Event::Fetch`]: the blob's hash on the node, or why not.
    pub fn fetched(&self, req: u64, result: Result<String, String>) {
        self.send(ToNode::Fetched { req, result });
    }

    /// Answer a [`Event::Browse`].
    pub fn browsed(&self, req: u64, result: Result<drive_proto::Browsed, String>) {
        self.send(ToNode::Browsed { req, result });
    }

    /// Who else has a document open, with their presence: (peer, name, json).
    pub fn others(&self, id: &str) -> Vec<(Peer, String, String)> {
        self.0
            .docs
            .lock()
            .expect("docs")
            .get(id)
            .map(|d| {
                d.others
                    .iter()
                    .map(|(p, (n, j))| (*p, n.clone(), j.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn list(&self, prefix: &str) {
        self.send(ToNode::List {
            prefix: prefix.to_string(),
        });
    }

    fn send(&self, msg: ToNode) {
        if let Some(tx) = self.0.tx.lock().expect("tx").as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// Save every open document's copy, a moment after changes.
    async fn saver(self) {
        loop {
            tokio::select! {
                _ = self.0.save.notified() => {}
                _ = self.stopped() => return,
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let snapshots: Vec<(String, Vec<u8>)> = {
                let docs = self.0.docs.lock().expect("docs");
                docs.iter()
                    .filter_map(|(id, d)| {
                        d.doc
                            .export(ExportMode::Snapshot)
                            .ok()
                            .map(|s| (id.clone(), s))
                    })
                    .collect()
            };
            for (id, bytes) in snapshots {
                let path = self.0.dir.join("docs").join(file_name(&id));
                let tmp = path.with_extension("tmp");
                if tokio::fs::write(&tmp, bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &path).await;
                }
            }
        }
    }

    /// The connection, reconnecting with a capped, jittered backoff. Each
    /// connection leaves one event saying why it ended (drive#172).
    async fn run(self, url: String) {
        let mut backoff = connection::Backoff::default();
        loop {
            // wss:// to a node behind TLS (the tailnet, a domain): rustls
            // has two crypto backends compiled in here, so name one.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let connected = tokio::select! {
                c = tokio_tungstenite::connect_async_with_config(
                    url.as_str(),
                    Some(connection::ws_config()),
                    false,
                ) => c,
                _ = self.stopped() => return,
            };
            let mut summary = connection::Summary::default();
            let (ended, lasted) = match connected {
                Ok((ws, _)) => {
                    let opened = tokio::time::Instant::now();
                    let ended = tokio::select! {
                        e = self.session(ws, &mut summary) => e,
                        _ = self.stopped() => {
                            *self.0.tx.lock().expect("tx") = None;
                            return;
                        }
                    };
                    (ended, opened.elapsed())
                }
                Err(e) => (
                    connection::Ended::ConnectFailed(e.to_string()),
                    Duration::ZERO,
                ),
            };
            self.went_offline();
            let wait = backoff.next(lasted, connection::jitter());
            connection::log_ended(&url, &ended, &summary, lasted, wait);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.stopped() => return,
            }
        }
    }

    /// The connection is gone: nothing is synced until the next one.
    fn went_offline(&self) {
        *self.0.tx.lock().expect("tx") = None;
        self.0.online.send_replace(false);
        let _ = self.0.events.send(Event::Online(false));
        for d in self.0.docs.lock().expect("docs").values_mut() {
            d.synced.send_replace(false);
            d.others.clear();
        }
    }

    /// One connection, from the handshake until it ends: why it ended,
    /// with what it did in `summary`.
    async fn session(
        &self,
        ws: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        summary: &mut connection::Summary,
    ) -> connection::Ended {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<ToNode>();
        // This computer's own agent needs no token; an agent elsewhere
        // signs in (`drive sign-in`) and sends its token.
        let hello = ToNode::Hello {
            version: drive_proto::VERSION,
            name: self.0.name.clone(),
            token: self.0.token.clone(),
        };
        if let Err(e) = sink
            .send(Message::Binary(drive_proto::encode(&hello).into()))
            .await
        {
            return connection::Ended::HelloFailed(e.to_string());
        }
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink
                    .send(Message::Binary(drive_proto::encode(&m).into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        *self.0.tx.lock().expect("tx") = Some(tx);
        let ended = loop {
            let frame = match stream.next().await {
                None => break connection::Ended::Hangup,
                Some(Err(e)) => break connection::Ended::ReadFailed(e.to_string()),
                Some(Ok(Message::Close(close))) => {
                    break connection::Ended::NodeClosed {
                        code: close.as_ref().map_or(1005, |c| u16::from(c.code)),
                        reason: close.map(|c| c.reason.to_string()).unwrap_or_default(),
                    };
                }
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(_)) => continue,
            };
            summary.received(frame.len());
            let Ok(msg) = drive_proto::decode::<ToClient>(&frame) else {
                summary.undecoded += 1;
                continue;
            };
            match &msg {
                ToClient::Welcome { .. } => summary.welcomed = true,
                ToClient::Error { message } => summary.refused = Some(message.clone()),
                _ => {}
            }
            self.receive(msg);
        };
        *self.0.tx.lock().expect("tx") = None;
        writer.abort();
        ended
    }

    fn receive(&self, msg: ToClient) {
        match msg {
            ToClient::Welcome { .. } => {
                self.0.online.send_replace(true);
                let _ = self.0.events.send(Event::Online(true));
                let joins: Vec<ToNode> = self
                    .0
                    .docs
                    .lock()
                    .expect("docs")
                    .iter()
                    .map(|(id, d)| ToNode::Join {
                        doc: id.clone(),
                        version_vector: d.doc.oplog_vv().encode(),
                    })
                    .collect();
                for j in joins {
                    self.send(j);
                }
                let agent = self.0.agent.lock().expect("agent").clone();
                if let Some((space, device)) = agent {
                    self.send(ToNode::Agent { space, device });
                }
            }
            ToClient::Synced {
                doc: id,
                bytes,
                version_vector,
            } => {
                let (ours, presence) = {
                    let docs = self.0.docs.lock().expect("docs");
                    let Some(d) = docs.get(&id) else { return };
                    if !bytes.is_empty() {
                        let _ = d.doc.import(&bytes);
                    }
                    let theirs = VersionVector::decode(&version_vector).unwrap_or_default();
                    let ours = d
                        .doc
                        .export(ExportMode::updates(&theirs))
                        .unwrap_or_default();
                    if let Ok(mut o) = d.outbox.lock() {
                        o.clear();
                    }
                    d.synced.send_replace(true);
                    (ours, d.presence.clone())
                };
                if !ours.is_empty() {
                    self.send(ToNode::Update {
                        doc: id.clone(),
                        bytes: ours,
                    });
                }
                if let Some(json) = presence {
                    self.send(ToNode::Presence {
                        doc: id.clone(),
                        json,
                    });
                }
                let _ = self.0.events.send(Event::Changed(id));
                self.0.save.notify_one();
            }
            ToClient::Update { doc: id, bytes } => {
                let ok = self
                    .0
                    .docs
                    .lock()
                    .expect("docs")
                    .get(&id)
                    .map(|d| d.doc.import(&bytes).is_ok());
                if ok == Some(true) {
                    let _ = self.0.events.send(Event::Changed(id));
                    self.0.save.notify_one();
                }
            }
            ToClient::Presence {
                doc,
                peer,
                name,
                json,
            } => {
                if let Some(d) = self.0.docs.lock().expect("docs").get_mut(&doc) {
                    match &json {
                        Some(j) => d.others.insert(peer, (name.clone(), j.clone())),
                        None => d.others.remove(&peer),
                    };
                }
                let _ = self.0.events.send(Event::Presence {
                    doc,
                    peer,
                    name,
                    json,
                });
            }
            ToClient::Docs { prefix, docs } => {
                let _ = self.0.events.send(Event::Docs { prefix, docs });
            }
            ToClient::Fetch {
                req,
                location,
                path,
            } => {
                let _ = self.0.events.send(Event::Fetch {
                    req,
                    location,
                    path,
                });
            }
            ToClient::Sidecars {
                req,
                location,
                path,
                kinds,
                force,
            } => {
                let _ = self.0.events.send(Event::Sidecars {
                    req,
                    location,
                    path,
                    kinds,
                    force,
                });
            }
            ToClient::Browse { req, ask } => {
                let _ = self.0.events.send(Event::Browse { req, ask });
            }
            ToClient::Error { message } if message == "sign in" => {
                tracing::warn!(drive.error = %message, "the node asks this device to sign in: run `drive sign-in`")
            }
            ToClient::Error { message } => {
                tracing::warn!(drive.error = %message, "the node refused something")
            }
            // Only for live listings, which this client does not ask for.
            ToClient::Listed { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_that_differ_keep_copies_apart() {
        let ids = [
            "a/b",
            "a:b",
            "a_b",
            "a.b",
            "A_b",
            "a%2fb",
            "home/drive:files",
        ];
        let names: std::collections::HashSet<String> = ids.iter().map(|i| file_name(i)).collect();
        assert_eq!(names.len(), ids.len(), "{names:?}");
        // Case-insensitive disks (macOS, Windows) too.
        let lower: std::collections::HashSet<String> =
            names.iter().map(|n| n.to_lowercase()).collect();
        assert_eq!(lower.len(), ids.len());
        assert!(
            names
                .iter()
                .all(|n| n.ends_with(".loro") && !n.contains('/'))
        );
    }

    #[test]
    fn a_copy_under_the_old_name_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let id = "home/drive:files";
        std::fs::write(dir.path().join("home_drive_files.loro"), b"old").unwrap();
        assert_eq!(std::fs::read(copy_path(dir.path(), id)).unwrap(), b"old");
        std::fs::write(dir.path().join(file_name(id)), b"new").unwrap();
        assert_eq!(std::fs::read(copy_path(dir.path(), id)).unwrap(), b"new");
    }

    #[tokio::test]
    async fn opening_one_id_twice_at_once_gives_one_document() {
        let dir = tempfile::tempdir().unwrap();
        let drive = Drive::connect("ws://127.0.0.1:1/sync", "t", dir.path()).await;
        let (a, b) = tokio::join!(drive.open("me/x"), drive.open("me/x"));
        a.get_map("m").insert("k", 1).unwrap();
        drive.commit("me/x");
        assert!(
            b.get_map("m").get("k").is_some(),
            "both opens share the document"
        );
    }

    #[tokio::test]
    async fn dropping_the_last_handle_stops_the_background_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let drive = Drive::connect("ws://127.0.0.1:1/sync", "t", dir.path()).await;
        let other = drive.clone();
        let inner = Arc::downgrade(&drive.0);
        drop(drive);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inner.upgrade().is_some(), "a handle is still held");
        drop(other);
        for _ in 0..100 {
            if inner.upgrade().is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the connection and saver tasks kept the drive alive");
    }
}
