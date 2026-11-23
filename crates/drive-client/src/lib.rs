//! `drive-client`: what an app embeds to use Drive — in a browser, or in a
//! native app (a desktop or phone build of the same app).
//!
//! - [`Drive::connect`] opens one connection to a node; it reconnects by
//!   itself and says so in [`Status`].
//! - [`Drive::open`] opens a document (a [`LoroDoc`]): first from this
//!   device's own copy (so it works offline), then synced live with the
//!   node. Edit it, then call [`Drive::commit`].
//! - [`Drive::set_presence`] says where you are in a document (any JSON);
//!   every peer's presence arrives as [`Event::Presence`].
//! - Everything that happens comes out of [`Drive::events`], one stream,
//!   for the UI layer to turn into its own state.
//!
//! A copy of each open document is kept here (in a browser, IndexedDB;
//! natively, a folder); edits made offline are sent when the connection
//! comes back (the join handshake exchanges version vectors, so nothing is
//! lost or repeated).
//!
//! One implementation for both: the protocol and the documents here, what
//! differs (the socket, timers, storage) in `platform`.

#[cfg(target_arch = "wasm32")]
mod idb;
mod platform;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use drive_proto::{DocId, DocInfo, Peer, ToClient, ToNode};
use futures_channel::mpsc;
pub use loro;
use loro::{ExportMode, LoroDoc, Subscription, VersionVector};

/// The stream of [`Event`]s a [`Drive`] produces.
pub type EventStream = mpsc::UnboundedReceiver<Event>;

/// Wait (a browser's timer, or Tokio's natively).
pub async fn sleep(ms: u32) {
    platform::sleep(ms).await;
}

/// Where a native build keeps its copies of documents and who it is, when
/// set before connecting (default: `DRIVE_CLIENT_DIR`, else the user's data
/// folder). A browser keeps them in its own storage.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_data_dir(dir: impl Into<std::path::PathBuf>) {
    platform::set_data_dir(dir.into());
}

/// Where a native build keeps what it keeps (see [`set_data_dir`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn data_dir() -> Option<std::path::PathBuf> {
    platform::dir()
}

/// Run `fut` on the client's own Tokio runtime and wait for it, from any
/// executor (a Blitz window's has no Tokio): for what needs Tokio, such as
/// an HTTP client. `None` if it panicked.
#[cfg(not(target_arch = "wasm32"))]
pub async fn on_runtime<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    platform::on_runtime(fut).await
}

/// The node's HTTP address natively (`DRIVE_NODE` as http/https, without
/// `/sync`): where an app's calls to the node go.
#[cfg(not(target_arch = "wasm32"))]
pub fn node_http() -> String {
    let ws = platform::default_url();
    let base = ws.strip_suffix("/sync").unwrap_or(&ws);
    if let Some(rest) = base.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = base.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        base.to_string()
    }
}

/// The connection's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Online,
    /// Disconnected; edits stay here and are sent on reconnect.
    Offline,
}

/// What happened, for the UI.
#[derive(Clone, Debug)]
pub enum Event {
    Status(Status),
    /// A document changed (here or elsewhere).
    Changed(DocId),
    /// A peer's presence in a document; `None` when they left.
    Presence {
        doc: DocId,
        peer: Peer,
        name: String,
        json: Option<String>,
    },
    /// The node's answer to [`Drive::list`].
    Docs {
        prefix: String,
        docs: Vec<DocInfo>,
    },
}

/// Who this device is: a display name and a colour, kept here
/// (`localStorage` in a browser, a file natively), shared by every app on
/// the same address or machine.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Me {
    pub name: String,
    pub hue: u32,
}

struct OpenDoc {
    doc: LoroDoc,
    /// Updates made here, waiting to be sent.
    outbox: Arc<Mutex<Vec<Vec<u8>>>>,
    _sub: Subscription,
    my_presence: Option<String>,
    save_pending: bool,
    /// The node has answered this document's join at least once.
    synced: bool,
}

struct Inner {
    url: String,
    me: Mutex<Me>,
    ws: Mutex<Option<platform::Socket>>,
    status: Mutex<Status>,
    peer: Mutex<Option<Peer>>,
    docs: Mutex<HashMap<DocId, OpenDoc>>,
    events: mpsc::UnboundedSender<Event>,
    retry_ms: AtomicU32,
    /// The issuer's access token, sent in the handshake (none: the node
    /// is open, or this is its own computer).
    token: Mutex<Option<String>>,
    /// Who this device's changes are by (in every change's history).
    author: Mutex<String>,
}

/// A lock that a panic elsewhere did not poison for good.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A connection to a Drive node, and the documents open on it.
#[derive(Clone)]
pub struct Drive(Arc<Inner>);

impl PartialEq for Drive {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Drive {
    /// Connect to the node at `url` (`wss://host/sync`); `None` connects to
    /// the page's own host in a browser, natively to `DRIVE_NODE` (default
    /// `ws://127.0.0.1:8096/sync`, this computer's node). Returns the
    /// client and its event stream.
    pub fn connect(url: Option<String>) -> (Drive, mpsc::UnboundedReceiver<Event>) {
        Drive::connect_with(url, None)
    }

    /// [`Drive::connect`], signed in from the first handshake on.
    pub fn connect_with(
        url: Option<String>,
        token: Option<String>,
    ) -> (Drive, mpsc::UnboundedReceiver<Event>) {
        let url = url.unwrap_or_else(platform::default_url);
        let (tx, rx) = mpsc::unbounded();
        let me = platform::load_me()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| Me {
                name: guest_name(),
                hue: (platform::random() * 360.0) as u32,
            });
        let drive = Drive(Arc::new(Inner {
            url,
            me: Mutex::new(me),
            ws: Mutex::new(None),
            status: Mutex::new(Status::Connecting),
            peer: Mutex::new(None),
            docs: Mutex::new(HashMap::new()),
            events: tx,
            retry_ms: AtomicU32::new(500),
            token: Mutex::new(token),
            author: Mutex::new(String::new()),
        }));
        drive.save_me();
        platform::drop_old_copies();
        drive.open_socket();
        (drive, rx)
    }

    /// Use this access token from the next handshake on (a fresh one
    /// after a refresh reconnects, so the node sees it).
    pub fn set_token(&self, token: Option<String>) {
        let changed = {
            let mut t = lock(&self.0.token);
            let changed = *t != token;
            *t = token;
            changed
        };
        if changed && let Some(ws) = lock(&self.0.ws).as_ref() {
            ws.close();
        }
    }

    /// Who the changes made here are by: the signed-in person's email, or
    /// this device's name.
    pub fn set_author(&self, who: &str) {
        *lock(&self.0.author) = who.to_string();
        for d in lock(&self.0.docs).values() {
            d.doc.set_next_commit_message(who);
        }
    }

    fn author(&self) -> String {
        let a = lock(&self.0.author).clone();
        if a.is_empty() {
            lock(&self.0.me).name.clone()
        } else {
            a
        }
    }

    pub fn status(&self) -> Status {
        *lock(&self.0.status)
    }

    /// This connection's peer number, once the node has said it.
    pub fn peer(&self) -> Option<Peer> {
        *lock(&self.0.peer)
    }

    pub fn me(&self) -> Me {
        lock(&self.0.me).clone()
    }

    /// Change your display name and colour (every app on this address).
    pub fn set_me(&self, me: Me) {
        *lock(&self.0.me) = me;
        self.save_me();
        // A new name reaches the node on the next connection.
        if let Some(ws) = lock(&self.0.ws).as_ref() {
            ws.close();
        }
    }

    fn save_me(&self) {
        let json = serde_json::to_string(&*lock(&self.0.me));
        if let Ok(j) = json {
            platform::save_me(&j);
        }
    }

    /// Open a document: this device's copy now, the node's as it arrives.
    pub fn open(&self, id: &str) -> LoroDoc {
        if let Some(d) = lock(&self.0.docs).get(id) {
            return d.doc.clone();
        }
        let doc = LoroDoc::new();
        drive_spine::history::prepare(&doc, &self.author());
        // This device's copy arrives a moment later (storage is async); it
        // merges with whatever the node has sent by then.
        {
            let me = self.clone();
            let id = id.to_string();
            platform::spawn(async move {
                let Some(bytes) = platform::load_doc(&id).await else {
                    return;
                };
                let ok = lock(&me.0.docs)
                    .get(&id)
                    .map(|d| d.doc.import(&bytes).is_ok());
                if ok == Some(true) {
                    // Edits made offline last time: join again so the node
                    // says what it lacks and they are sent.
                    if me.status() == Status::Online {
                        me.join(&id);
                    }
                    me.emit(Event::Changed(id.clone()));
                }
            });
        }
        let outbox = Arc::new(Mutex::new(Vec::new()));
        let ob = outbox.clone();
        let sub = doc.subscribe_local_update(Box::new(move |bytes| {
            if let Ok(mut o) = ob.lock() {
                o.push(bytes.clone());
            }
            true
        }));
        lock(&self.0.docs).insert(
            id.to_string(),
            OpenDoc {
                doc: doc.clone(),
                outbox,
                _sub: sub,
                my_presence: None,
                save_pending: false,
                synced: false,
            },
        );
        if self.status() == Status::Online {
            self.join(id);
        }
        doc
    }

    /// Close a document (it stays saved here).
    pub fn close(&self, id: &str) {
        let gone = lock(&self.0.docs).remove(id).is_some();
        if gone {
            self.send(&ToNode::Leave {
                doc: id.to_string(),
            });
        }
    }

    /// Commit the edits made to a document and send them.
    pub fn commit(&self, id: &str) {
        let author = self.author();
        let pending = {
            let docs = lock(&self.0.docs);
            let Some(d) = docs.get(id) else { return };
            drive_spine::history::commit(&d.doc, &author);
            let mut o = d
                .outbox
                .lock()
                .map(|mut o| std::mem::take(&mut *o))
                .unwrap_or_default();
            o.retain(|u| !u.is_empty());
            o
        };
        if !pending.is_empty() {
            if self.status() == Status::Online {
                for bytes in pending {
                    self.send(&ToNode::Update {
                        doc: id.to_string(),
                        bytes,
                    });
                }
            }
            self.changed(id);
        }
    }

    /// Whether the node has answered this document's join yet (so an empty
    /// document is really empty, not just not arrived).
    pub fn is_synced(&self, id: &str) -> bool {
        lock(&self.0.docs).get(id).is_some_and(|d| d.synced)
    }

    /// Merge exported Loro bytes made elsewhere (a template, a file) into a
    /// document, and send them, as if they were edits made here.
    pub fn import_local(&self, id: &str, bytes: &[u8]) {
        let ok = lock(&self.0.docs)
            .get(id)
            .map(|d| d.doc.import(bytes).is_ok());
        if ok == Some(true) {
            self.send(&ToNode::Update {
                doc: id.to_string(),
                bytes: bytes.to_vec(),
            });
            self.changed(id);
        }
    }

    /// Say you are here, unless you already said something more: opening
    /// a document from a second component must not wipe your cursor.
    pub fn announce(&self, id: &str, json: String) {
        let fresh = lock(&self.0.docs)
            .get(id)
            .is_some_and(|d| d.my_presence.is_none());
        if fresh {
            self.set_presence(id, json);
        }
    }

    /// Say where you are in a document. Never stored; others see it live.
    pub fn set_presence(&self, id: &str, json: String) {
        if let Some(d) = lock(&self.0.docs).get_mut(id) {
            d.my_presence = Some(json.clone());
        }
        self.send(&ToNode::Presence {
            doc: id.to_string(),
            json,
        });
    }

    /// Ask the node which documents it holds under a prefix; the answer
    /// comes as [`Event::Docs`].
    pub fn list(&self, prefix: &str) {
        self.send(&ToNode::List {
            prefix: prefix.to_string(),
        });
    }

    // ── internals ───────────────────────────────────────────────────────

    fn emit(&self, e: Event) {
        let _ = self.0.events.unbounded_send(e);
    }

    fn set_status(&self, s: Status) {
        let changed = {
            let mut st = lock(&self.0.status);
            let changed = *st != s;
            *st = s;
            changed
        };
        if changed {
            self.emit(Event::Status(s));
        }
    }

    fn send(&self, msg: &ToNode) {
        if let Some(ws) = lock(&self.0.ws).as_ref()
            && ws.is_open()
        {
            ws.send(drive_proto::encode(msg));
        }
    }

    fn join(&self, id: &str) {
        let vv = lock(&self.0.docs)
            .get(id)
            .map(|d| d.doc.oplog_vv().encode());
        if let Some(vv) = vv {
            self.send(&ToNode::Join {
                doc: id.to_string(),
                version_vector: vv,
            });
        }
    }

    /// A document changed: tell the UI, and save this device's copy soon.
    fn changed(&self, id: &str) {
        self.emit(Event::Changed(id.to_string()));
        let schedule = {
            let mut docs = lock(&self.0.docs);
            match docs.get_mut(id) {
                Some(d) if !d.save_pending => {
                    d.save_pending = true;
                    true
                }
                _ => false,
            }
        };
        if schedule {
            let me = self.clone();
            let id = id.to_string();
            platform::after(600, move || me.save_local(&id));
        }
    }

    fn save_local(&self, id: &str) {
        let snapshot = {
            let mut docs = lock(&self.0.docs);
            let Some(d) = docs.get_mut(id) else { return };
            d.save_pending = false;
            d.doc.export(ExportMode::Snapshot).ok()
        };
        if let Some(bytes) = snapshot {
            let id = id.to_string();
            platform::spawn(async move { platform::save_doc(&id, &bytes).await });
        }
    }

    fn open_socket(&self) {
        self.set_status(Status::Connecting);
        match platform::Socket::open(&self.0.url, self.clone()) {
            Some(s) => *lock(&self.0.ws) = Some(s),
            None => self.retry(),
        }
    }

    /// The socket opened: say who this is.
    pub(crate) fn socket_opened(&self) {
        self.0.retry_ms.store(500, Ordering::Relaxed);
        let name = lock(&self.0.me).name.clone();
        let token = lock(&self.0.token).clone();
        self.send(&ToNode::Hello {
            version: drive_proto::VERSION,
            name,
            token,
        });
    }

    /// The socket closed (or never opened): offline, and try again.
    pub(crate) fn socket_closed(&self) {
        *lock(&self.0.peer) = None;
        self.set_status(Status::Offline);
        self.retry();
    }

    /// A message from the node, as bytes off the socket.
    pub(crate) fn socket_message(&self, bytes: &[u8]) {
        if let Ok(msg) = drive_proto::decode::<ToClient>(bytes) {
            self.receive(msg);
        }
    }

    fn retry(&self) {
        let ms = self.0.retry_ms.load(Ordering::Relaxed);
        self.0.retry_ms.store((ms * 2).min(8000), Ordering::Relaxed);
        let me = self.clone();
        platform::after(ms, move || me.open_socket());
    }

    fn receive(&self, msg: ToClient) {
        match msg {
            ToClient::Welcome { peer } => {
                *lock(&self.0.peer) = Some(peer);
                self.set_status(Status::Online);
                let ids: Vec<DocId> = lock(&self.0.docs).keys().cloned().collect();
                for id in ids {
                    self.join(&id);
                }
            }
            ToClient::Synced {
                doc: id,
                bytes,
                version_vector,
            } => {
                let (ours, presence) = {
                    let mut docs = lock(&self.0.docs);
                    let Some(d) = docs.get_mut(&id) else { return };
                    d.synced = true;
                    if !bytes.is_empty() {
                        let _ = d.doc.import(&bytes);
                    }
                    // Whatever was made here (offline too) that the node lacks.
                    let theirs = VersionVector::decode(&version_vector).unwrap_or_default();
                    let ours = d
                        .doc
                        .export(ExportMode::updates(&theirs))
                        .unwrap_or_default();
                    if let Ok(mut o) = d.outbox.lock() {
                        o.clear();
                    }
                    (ours, d.my_presence.clone())
                };
                if !ours.is_empty() {
                    self.send(&ToNode::Update {
                        doc: id.clone(),
                        bytes: ours,
                    });
                }
                if let Some(json) = presence {
                    self.send(&ToNode::Presence {
                        doc: id.clone(),
                        json,
                    });
                }
                self.changed(&id);
            }
            ToClient::Update { doc: id, bytes } => {
                let ok = lock(&self.0.docs)
                    .get(&id)
                    .map(|d| d.doc.import(&bytes).is_ok());
                if ok == Some(true) {
                    self.changed(&id);
                }
            }
            ToClient::Presence {
                doc,
                peer,
                name,
                json,
            } => {
                self.emit(Event::Presence {
                    doc,
                    peer,
                    name,
                    json,
                });
            }
            ToClient::Docs { prefix, docs } => self.emit(Event::Docs { prefix, docs }),
            ToClient::Error { .. }
            | ToClient::Fetch { .. }
            | ToClient::Sidecars { .. }
            | ToClient::Browse { .. } => {}
        }
    }
}

fn guest_name() -> String {
    const N: [&str; 12] = [
        "Otter", "Heron", "Lynx", "Robin", "Maple", "Comet", "Pike", "Wren", "Fern", "Juniper",
        "Basil", "Sage",
    ];
    let i = (platform::random() * N.len() as f64) as usize % N.len();
    format!("Guest {}", N[i])
}
