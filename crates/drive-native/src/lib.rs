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
}

/// A connection to a Drive node and the documents open on it.
#[derive(Clone)]
pub struct Drive(Arc<Inner>);

/// The file a document's copy is kept in (also how `drive-client` names
/// a native app's copies).
pub fn file_name(id: &str) -> String {
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
        let drive = Drive(Arc::new(Inner {
            name: name.to_string(),
            dir,
            docs: Mutex::new(HashMap::new()),
            tx: Mutex::new(None),
            online: watch::channel(false).0,
            events,
            save: Notify::new(),
            agent: Mutex::new(None),
            token,
        }));
        tokio::spawn(drive.clone().run(url.to_string()));
        tokio::spawn(drive.clone().saver());
        drive
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
        let doc = LoroDoc::new();
        drive_spine::history::prepare(&doc, &self.0.name);
        if let Ok(bytes) = tokio::fs::read(self.0.dir.join("docs").join(file_name(id))).await {
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
        self.0.docs.lock().expect("docs").insert(
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
            self.0.save.notified().await;
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

    /// The connection, reconnecting with backoff.
    async fn run(self, url: String) {
        let mut wait = 500u64;
        loop {
            // wss:// to a node behind TLS (the tailnet, a domain): rustls
            // has two crypto backends compiled in here, so name one.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            match tokio_tungstenite::connect_async(url.as_str()).await {
                Ok((ws, _)) => {
                    wait = 500;
                    self.session(ws).await;
                    tracing::debug!(drive.url = %url, "connection closed");
                }
                Err(e) => tracing::debug!(drive.url = %url, error = %e, "connect failed"),
            }
            *self.0.tx.lock().expect("tx") = None;
            self.0.online.send_replace(false);
            let _ = self.0.events.send(Event::Online(false));
            for d in self.0.docs.lock().expect("docs").values_mut() {
                d.synced.send_replace(false);
                d.others.clear();
            }
            tokio::time::sleep(Duration::from_millis(wait)).await;
            wait = (wait * 2).min(8000);
        }
    }

    async fn session(
        &self,
        ws: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<ToNode>();
        // This computer's own agent needs no token; an agent elsewhere
        // signs in (`drive sign-in`) and sends its token.
        let hello = ToNode::Hello {
            version: drive_proto::VERSION,
            name: self.0.name.clone(),
            token: self.0.token.clone(),
        };
        if sink
            .send(Message::Binary(drive_proto::encode(&hello).into()))
            .await
            .is_err()
        {
            return;
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
        while let Some(Ok(frame)) = stream.next().await {
            let Message::Binary(bytes) = frame else {
                continue;
            };
            let Ok(msg) = drive_proto::decode::<ToClient>(&bytes) else {
                continue;
            };
            self.receive(msg);
        }
        *self.0.tx.lock().expect("tx") = None;
        writer.abort();
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
        }
    }
}
