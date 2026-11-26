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
#[cfg(all(feature = "p2p", not(target_arch = "wasm32")))]
pub mod peers;
mod platform;
mod transport;

pub use transport::{Connection, MaybeSend, Transport, WebSocket, Wire};

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
    /// The documents under a prefix: the node's answer to [`Drive::list`],
    /// and a live listing ([`Drive::subscribe_list`]) whole, each time it
    /// changes.
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
    /// This device's own copy has been read (found or not): until then an
    /// empty document may only be one whose copy has not arrived yet.
    restored: bool,
    /// How many [`Drive::hold`]s are on it, and how often the count has
    /// fallen to none (a close waiting to happen checks it is still the
    /// latest).
    holders: u32,
    released: u64,
}

/// How long a document nobody holds stays open: moving between views
/// that show it should not leave and join it again.
const LINGER_MS: u32 = 30_000;

struct Inner {
    transport: Box<dyn Transport>,
    /// What the transport connects to, for logs.
    url: String,
    me: Mutex<Me>,
    ws: Mutex<Option<Box<dyn Connection>>>,
    status: Mutex<Status>,
    peer: Mutex<Option<Peer>>,
    docs: Mutex<HashMap<DocId, OpenDoc>>,
    events: mpsc::UnboundedSender<Event>,
    retry_ms: AtomicU32,
    /// Which socket is the current one: each opened bumps it, and what an
    /// older socket or its retry timer says is ignored.
    generation: AtomicU32,
    /// The issuer's access token, sent in the handshake (none: the node
    /// is open, or this is its own computer).
    token: Mutex<Option<String>>,
    /// Who this device's changes are by (in every change's history).
    author: Mutex<String>,
    /// The live listings asked for, by prefix: how many asked, and the
    /// listing as the node last said it.
    lists: Mutex<HashMap<String, Listing>>,
}

/// One live listing ([`Drive::subscribe_list`]).
#[derive(Default)]
struct Listing {
    holders: u32,
    /// `None` until the node has answered.
    docs: Option<Vec<DocInfo>>,
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
        let transport = match url {
            Some(u) => WebSocket::new(u),
            None => WebSocket::default_node(),
        };
        Drive::connect_via(transport, token)
    }

    /// Connect through any [`Transport`]: a node's [`WebSocket`], or
    /// (natively, with the `p2p` feature) this device's own peers
    /// ([`Drive::peer_to_peer`]).
    pub fn connect_via(
        transport: impl Transport,
        token: Option<String>,
    ) -> (Drive, mpsc::UnboundedReceiver<Event>) {
        let url = transport.describe();
        let (tx, rx) = mpsc::unbounded();
        let me = platform::load_me()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| Me {
                name: guest_name(),
                hue: (platform::random() * 360.0) as u32,
            });
        // On wasm the transport is not Send (one thread), but native needs the Arc.
        #[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
        let drive = Drive(Arc::new(Inner {
            transport: Box::new(transport),
            url,
            me: Mutex::new(me),
            ws: Mutex::new(None),
            status: Mutex::new(Status::Connecting),
            peer: Mutex::new(None),
            docs: Mutex::new(HashMap::new()),
            events: tx,
            retry_ms: AtomicU32::new(500),
            generation: AtomicU32::new(0),
            token: Mutex::new(token),
            author: Mutex::new(String::new()),
            lists: Mutex::new(HashMap::new()),
        }));
        drive.save_me();
        platform::drop_old_copies();
        drive.open_socket();
        {
            // A page being hidden may be closed next: save what is not yet.
            let me = drive.clone();
            platform::on_hide(move || me.save_now());
        }
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
                restored: false,
                holders: 0,
                released: 0,
            },
        );
        // This device's copy arrives a moment later (storage is async); it
        // merges with whatever the node has sent by then. Spawned only once
        // the document is in `docs`, or a quick load finds nothing to mark.
        {
            let me = self.clone();
            let id = id.to_string();
            platform::spawn(async move {
                let bytes = platform::load_doc(&id).await;
                let imported = {
                    let mut docs = lock(&me.0.docs);
                    let Some(d) = docs.get_mut(&id) else { return };
                    d.restored = true;
                    bytes.is_some_and(|b| d.doc.import(&b).is_ok())
                };
                // Edits made offline last time: join again so the node
                // says what it lacks and they are sent.
                if imported && me.status() == Status::Online {
                    me.join(&id);
                }
                // Said either way: found or not, the copy is in.
                me.emit(Event::Changed(id.clone()));
            });
        }
        if self.status() == Status::Online {
            self.join(id);
        }
        doc
    }

    /// Say something shows `id` ([`Drive::open`] it first). Once every
    /// hold is let go ([`Drive::let_go`]) and a while has passed, the
    /// document closes; without holds it stays open, as before.
    pub fn hold(&self, id: &str) {
        if let Some(d) = lock(&self.0.docs).get_mut(id) {
            d.holders += 1;
        }
    }

    /// One [`Drive::hold`] of `id` is over.
    pub fn let_go(&self, id: &str) {
        self.let_go_after(id, LINGER_MS);
    }

    fn let_go_after(&self, id: &str, ms: u32) {
        let stamp = {
            let mut docs = lock(&self.0.docs);
            let Some(d) = docs.get_mut(id) else { return };
            d.holders = d.holders.saturating_sub(1);
            if d.holders > 0 {
                return;
            }
            d.released += 1;
            d.released
        };
        let me = self.clone();
        let id = id.to_string();
        platform::after(ms, move || {
            let idle = lock(&me.0.docs)
                .get(&id)
                .is_some_and(|d| d.holders == 0 && d.released == stamp);
            if idle {
                me.close(&id);
            }
        });
    }

    /// Close a document (it stays saved here: a save still waiting is
    /// written first).
    pub fn close(&self, id: &str) {
        self.save_local(id);
        let gone = lock(&self.0.docs).remove(id).is_some();
        if gone {
            self.send(&ToNode::Leave {
                doc: id.to_string(),
            });
        }
    }

    /// Commit the edits made to a document and send them. What the socket
    /// does not take stays queued for the next commit (a reconnect's join
    /// sends whatever the node lacks either way).
    pub fn commit(&self, id: &str) {
        let author = self.author();
        let outbox = {
            let docs = lock(&self.0.docs);
            let Some(d) = docs.get(id) else { return };
            drive_spine::history::commit(&d.doc, &author);
            d.outbox.clone()
        };
        let pending = {
            let mut o = lock(&outbox);
            o.retain(|u| !u.is_empty());
            let any = !o.is_empty();
            if any && self.status() == Status::Online {
                let sent = o
                    .iter()
                    .take_while(|bytes| {
                        self.send(&ToNode::Update {
                            doc: id.to_string(),
                            bytes: bytes.to_vec(),
                        })
                    })
                    .count();
                o.drain(..sent);
            }
            any
        };
        if pending {
            self.changed(id);
        }
    }

    /// Save this device's copy of every document changed since it was last
    /// saved, now rather than a moment later: before an app quits (a
    /// browser page does it itself when it is hidden).
    pub fn save_now(&self) {
        let ids: Vec<DocId> = lock(&self.0.docs)
            .iter()
            .filter(|(_, d)| d.save_pending)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.save_local(&id);
        }
    }

    /// Whether the node has answered this document's join yet (so an empty
    /// document is really empty, not just not arrived).
    pub fn is_synced(&self, id: &str) -> bool {
        lock(&self.0.docs).get(id).is_some_and(|d| d.synced)
    }

    /// Whether this device's own copy of the document has been read, found
    /// or not (it loads a moment after [`Drive::open`]). Offline, an empty
    /// document that is restored is really empty here; one that is not may
    /// only be waiting for its copy. An [`Event::Changed`] says when.
    pub fn is_restored(&self, id: &str) -> bool {
        lock(&self.0.docs).get(id).is_some_and(|d| d.restored)
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

    /// Ask the node once which documents it holds under a prefix; the
    /// answer comes as [`Event::Docs`]. To follow a listing, use
    /// [`Drive::subscribe_list`].
    pub fn list(&self, prefix: &str) {
        self.send(&ToNode::List {
            prefix: prefix.to_string(),
        });
    }

    /// Follow the documents under a prefix: the listing comes as
    /// [`Event::Docs`] now (when it is known) and again whole each time the
    /// node says it changed, also after a reconnect. Each call is undone
    /// by one [`Drive::unsubscribe_list`]; the node is told once.
    pub fn subscribe_list(&self, prefix: &str) {
        let (first, known) = {
            let mut lists = lock(&self.0.lists);
            let l = lists.entry(prefix.to_string()).or_default();
            l.holders += 1;
            (l.holders == 1, l.docs.clone())
        };
        // Offline, the next welcome subscribes.
        if first && self.is_welcomed() {
            self.send(&ToNode::ListSubscribe {
                prefix: prefix.to_string(),
            });
        } else if let Some(docs) = known {
            // Asked already: the listing as it is (one still coming is
            // told to everyone who asked).
            self.emit(Event::Docs {
                prefix: prefix.to_string(),
                docs,
            });
        }
    }

    /// Undo one [`Drive::subscribe_list`]; the last tells the node to stop.
    pub fn unsubscribe_list(&self, prefix: &str) {
        let last = {
            let mut lists = lock(&self.0.lists);
            let Some(l) = lists.get_mut(prefix) else {
                return;
            };
            l.holders = l.holders.saturating_sub(1);
            let last = l.holders == 0;
            if last {
                lists.remove(prefix);
            }
            last
        };
        if last && self.is_welcomed() {
            self.send(&ToNode::ListUnsubscribe {
                prefix: prefix.to_string(),
            });
        }
    }

    /// Whether the node has welcomed this connection (what is sent before
    /// is sent again on the welcome, or not needed).
    fn is_welcomed(&self) -> bool {
        lock(&self.0.peer).is_some()
    }

    /// The prefixes followed now.
    fn followed(&self) -> Vec<String> {
        lock(&self.0.lists).keys().cloned().collect()
    }

    /// A listing from the node: kept when it is followed, and told.
    fn listed(&self, prefix: String, docs: Vec<DocInfo>, removed: Option<Vec<DocId>>) {
        let docs = {
            let mut lists = lock(&self.0.lists);
            match (lists.get_mut(&prefix), removed) {
                (Some(l), None) => {
                    l.docs = Some(docs.clone());
                    docs
                }
                (Some(l), Some(removed)) => {
                    let list = l.docs.get_or_insert_with(Vec::new);
                    drive_proto::apply_listed(list, docs, &removed);
                    list.clone()
                }
                // A delta for a listing no longer followed.
                (None, Some(_)) => return,
                (None, None) => docs,
            }
        };
        self.emit(Event::Docs { prefix, docs });
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

    /// Send a message; `false` when the socket is not open or refused it.
    fn send(&self, msg: &ToNode) -> bool {
        match lock(&self.0.ws).as_ref() {
            Some(ws) if ws.is_open() => ws.send(drive_proto::encode(msg)),
            _ => false,
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
            if !d.save_pending {
                return;
            }
            d.save_pending = false;
            d.doc.export(ExportMode::Snapshot).ok()
        };
        if let Some(bytes) = snapshot {
            platform::save_doc(id, bytes);
        }
    }

    /// Open a socket in place of the current one (closed, its callbacks
    /// and pending retry ignored from now on).
    fn open_socket(&self) {
        let generation = self
            .0
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        self.set_status(Status::Connecting);
        let mut ws = lock(&self.0.ws);
        if let Some(old) = ws.take() {
            old.close();
        }
        let wire = Wire {
            drive: self.clone(),
            generation,
        };
        match self.0.transport.open(wire) {
            Some(s) => *ws = Some(s),
            None => {
                drop(ws);
                self.retry(generation);
            }
        }
    }

    /// Whether `generation` is the current socket's.
    fn is_current(&self, generation: u32) -> bool {
        self.0.generation.load(Ordering::SeqCst) == generation
    }

    /// The socket opened: say who this is.
    pub(crate) fn socket_opened(&self, generation: u32) {
        if !self.is_current(generation) {
            return;
        }
        // The wait before a retry restarts only once the node lets this
        // connection in (Welcome): a socket that opens and is refused at
        // once (a token the node no longer takes) keeps backing off,
        // instead of redialling every half second.
        let name = lock(&self.0.me).name.clone();
        let token = lock(&self.0.token).clone();
        self.send(&ToNode::Hello {
            version: drive_proto::VERSION,
            name,
            token,
        });
    }

    /// The socket closed (or never opened): offline, and try again.
    pub(crate) fn socket_closed(&self, generation: u32) {
        if !self.is_current(generation) {
            return;
        }
        *lock(&self.0.peer) = None;
        self.set_status(Status::Offline);
        self.retry(generation);
    }

    /// A message from the node, as bytes off the socket.
    pub(crate) fn socket_message(&self, generation: u32, bytes: &[u8]) {
        if !self.is_current(generation) {
            return;
        }
        if let Ok(msg) = drive_proto::decode::<ToClient>(bytes) {
            self.receive(msg);
        }
    }

    /// Open a new socket a while after `generation`'s closed, unless another
    /// was opened meanwhile.
    fn retry(&self, generation: u32) {
        let ms = self.0.retry_ms.load(Ordering::Relaxed);
        self.0.retry_ms.store((ms * 2).min(8000), Ordering::Relaxed);
        let me = self.clone();
        platform::after(ms, move || {
            if me.is_current(generation) {
                me.open_socket();
            }
        });
    }

    fn receive(&self, msg: ToClient) {
        match msg {
            ToClient::Welcome { peer } => {
                self.0.retry_ms.store(500, Ordering::Relaxed);
                *lock(&self.0.peer) = Some(peer);
                self.set_status(Status::Online);
                let ids: Vec<DocId> = lock(&self.0.docs).keys().cloned().collect();
                for id in ids {
                    self.join(&id);
                }
                // A new connection follows nothing yet: what changed while
                // offline comes in each listing as it is now.
                for prefix in self.followed() {
                    self.send(&ToNode::ListSubscribe { prefix });
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
            ToClient::Docs { prefix, docs } => self.listed(prefix, docs, None),
            ToClient::Listed {
                prefix,
                docs,
                removed,
            } => self.listed(prefix, docs, Some(removed)),
            // What the node refused (a document outside your orgs, an
            // update it could not import): nothing to undo here, but said.
            ToClient::Error { message } => {
                tracing::warn!(drive.url = %self.0.url, error = %message, "the node refused a message");
            }
            ToClient::Fetch { .. } | ToClient::Sidecars { .. } | ToClient::Browse { .. } => {}
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// The copies' folder, one for every test here (it is process-wide).
    pub(crate) fn copies() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap()).path();
        set_data_dir(dir);
        dir
    }

    fn pending(drive: &Drive, id: &str) -> usize {
        lock(&drive.0.docs)
            .get(id)
            .map_or(0, |d| lock(&d.outbox).len())
    }

    /// A socket that opens but whose sign-in the node refuses keeps
    /// backing off; only the node letting it in starts the waits over.
    #[test]
    fn the_retry_wait_restarts_only_once_the_node_lets_the_connection_in() {
        copies();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (drive, _events) = Drive::connect(Some(format!("ws://127.0.0.1:{port}/sync")));
        let wait = || drive.0.retry_ms.load(Ordering::Relaxed);
        drive.0.retry_ms.store(4000, Ordering::Relaxed);
        let generation = drive.0.generation.load(Ordering::SeqCst);
        drive.socket_opened(generation);
        assert_eq!(wait(), 4000, "an open socket alone is not let in");
        drive.receive(ToClient::Error {
            message: "sign in".into(),
        });
        assert_eq!(wait(), 4000, "a refused sign-in keeps the wait");
        drive.receive(ToClient::Welcome { peer: 7 });
        assert_eq!(wait(), 500, "let in: the waits start over");
    }

    #[test]
    fn an_edit_made_offline_stays_queued() {
        copies();
        // Nothing listens here, so the client stays offline.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (drive, _events) = Drive::connect(Some(format!("ws://127.0.0.1:{port}/sync")));
        let id = "home/test:doc";
        let doc = drive.open(id);
        doc.get_map("song").insert("title", "Know Me").unwrap();
        drive.commit(id);
        assert_eq!(pending(&drive, id), 1, "the update waits to be sent");
    }

    pub(crate) async fn node() -> (String, tempfile::TempDir) {
        let data = tempfile::tempdir().unwrap();
        let opts = drive_node::Options {
            no_backfill: true,
            ..Default::default()
        };
        let app = drive_node::router(data.path(), Vec::new(), opts).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (format!("ws://{addr}/sync"), data)
    }

    async fn online(drive: &Drive) {
        for _ in 0..100 {
            if drive.status() == Status::Online {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("never online");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_superseded_socket_closing_leaves_the_connection_alone() {
        copies();
        let (url, _data) = node().await;
        let (drive, _events) = Drive::connect(Some(url));
        online(&drive).await;
        let peer = drive.peer();

        // A socket from before this one says it closed.
        let current = drive.0.generation.load(Ordering::SeqCst);
        drive.socket_closed(current.wrapping_sub(1));
        assert_eq!(drive.status(), Status::Online);
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(drive.peer(), peer, "no second socket was opened");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saves_made_together_leave_the_newest_whole() {
        let dir = copies();
        let id = "home/test:saves";
        let copies: Vec<Vec<u8>> = (0..40u8)
            .map(|i| vec![i; 200_000 + usize::from(i) * 997])
            .collect();
        for bytes in &copies {
            platform::save_doc(id, bytes.clone());
        }
        let docs = dir.join("docs");
        let file = docs.join(drive_native::file_name(id));
        let mut kept = Vec::new();
        for _ in 0..100 {
            kept = std::fs::read(&file).unwrap_or_default();
            if kept == *copies.last().unwrap() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            &kept,
            copies.last().unwrap(),
            "the newest copy is kept whole"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let stray: Vec<_> = std::fs::read_dir(&docs)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "no temporary files are left");
    }

    #[test]
    fn an_error_from_the_node_is_logged() {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                lock(&self.0).extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        copies();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (drive, _events) = Drive::connect(Some(format!("ws://127.0.0.1:{port}/sync")));
        let buf = Buf::default();
        let out = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            drive.receive(ToClient::Error {
                message: "not allowed: other/x:doc".into(),
            });
        });
        let logged = String::from_utf8(lock(&buf.0).clone()).unwrap();
        assert!(
            logged.contains("not allowed: other/x:doc"),
            "logged: {logged:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_nobody_holds_closes_after_a_while_and_is_saved() {
        let copies = copies();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (drive, _events) = Drive::connect(Some(format!("ws://127.0.0.1:{port}/sync")));
        let open = |id: &str| lock(&drive.0.docs).contains_key(id);

        let id = "home/test:held";
        let doc = drive.open(id);
        drive.hold(id);
        drive.hold(id);
        doc.get_map("song").insert("title", "Kept").unwrap();
        drive.commit(id);
        drive.let_go_after(id, 20);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(open(id), "still held once");

        drive.let_go_after(id, 20);
        // Held again before the linger ends: stays open.
        drive.hold(id);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(open(id), "held again in time");

        drive.let_go_after(id, 20);
        let file = copies.join("docs").join(drive_native::file_name(id));
        for _ in 0..50 {
            if !open(id) && file.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("closed: {}, copy saved: {}", !open(id), file.exists());
    }

    /// Wait until this device's copy of `id` has loaded (or was found
    /// missing).
    async fn restored(drive: &Drive, id: &str) {
        for _ in 0..100 {
            if drive.is_restored(id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("{id}: this device's copy never finished loading");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_says_when_this_devices_copy_has_loaded() {
        copies();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("ws://127.0.0.1:{port}/sync");

        // Nothing stored: restored, and really empty.
        let (drive, _events) = Drive::connect(Some(url.clone()));
        let fresh = "home/test:restored-none";
        let doc = drive.open(fresh);
        restored(&drive, fresh).await;
        assert!(doc.get_map("household").is_empty());
        assert!(!drive.is_restored("home/test:never-opened"));

        // A stored copy: once restored, it is in the document.
        let kept = "home/test:restored-kept";
        let made = LoroDoc::new();
        made.get_map("household").insert("name", "Ours").unwrap();
        platform::save_doc(kept, made.export(ExportMode::Snapshot).unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (drive, _events) = Drive::connect(Some(url));
        let doc = drive.open(kept);
        restored(&drive, kept).await;
        assert_eq!(
            doc.get_map("household")
                .get("name")
                .and_then(|v| v.into_value().ok())
                .and_then(|v| v.into_string().ok())
                .map(|s| s.to_string()),
            Some("Ours".to_string()),
            "the stored copy is in the document once it is restored"
        );
    }

    /// The next listing of `prefix` in `events` that `ok` accepts, within
    /// `within` (and nothing asked for in between).
    async fn listing(
        events: &mut EventStream,
        prefix: &str,
        within: std::time::Duration,
        ok: impl Fn(&[DocInfo]) -> bool,
    ) -> Option<Vec<DocInfo>> {
        use futures_util::StreamExt;
        tokio::time::timeout(within, async {
            while let Some(e) = events.next().await {
                if let Event::Docs { prefix: p, docs } = e
                    && p == prefix
                    && ok(&docs)
                {
                    return Some(docs);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// A followed listing is pushed by the node as documents come and go
    /// (drive#159): no asking again, before or after a reconnect.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_followed_listing_is_pushed_not_polled() {
        copies();
        let (url, _data) = node().await;
        let prefix = "home/test:live:";
        let (watcher, mut events) = Drive::connect(Some(url.clone()));
        let (maker, _maker_events) = Drive::connect(Some(url));
        watcher.subscribe_list(prefix);
        watcher.subscribe_list(prefix); // a second view of it: one listing
        online(&watcher).await;
        online(&maker).await;
        let quick = std::time::Duration::from_millis(1500);
        assert!(
            listing(&mut events, prefix, quick, <[_]>::is_empty)
                .await
                .is_some()
        );

        // Made elsewhere: heard well before a poll would have asked.
        let _a = maker.open("home/test:live:a");
        let has = |id: &'static str| move |d: &[DocInfo]| d.iter().any(|x| x.id == id);
        assert!(
            listing(&mut events, prefix, quick, has("home/test:live:a"))
                .await
                .is_some(),
            "a document made under the prefix is pushed"
        );

        // Reconnected (a new token closes the socket): followed again.
        watcher.set_token(Some("t".into()));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        online(&watcher).await;
        let _b = maker.open("home/test:live:b");
        let docs = listing(&mut events, prefix, quick, has("home/test:live:b"))
            .await
            .expect("still followed after a reconnect");
        assert!(docs.iter().any(|d| d.id == "home/test:live:a"), "{docs:?}");

        // Let go by both views: nothing more.
        watcher.unsubscribe_list(prefix);
        watcher.unsubscribe_list(prefix);
        // What was already on its way still arrives; then nothing.
        let settle = std::time::Duration::from_millis(300);
        while listing(&mut events, prefix, settle, |_| true)
            .await
            .is_some()
        {}
        let _c = maker.open("home/test:live:c");
        assert!(
            listing(
                &mut events,
                prefix,
                std::time::Duration::from_millis(500),
                |_| true
            )
            .await
            .is_none()
        );
    }
}
