//! `drive-client`: what an app embeds to use Drive, in the browser.
//!
//! - [`Drive::connect`] opens one connection to a node; it reconnects by
//!   itself and says so in [`Status`].
//! - [`Drive::open`] opens a document (a [`LoroDoc`]): first from this
//!   browser's own copy (so it works offline), then synced live with the
//!   node. Edit it, then call [`Drive::commit`].
//! - [`Drive::set_presence`] says where you are in a document (any JSON);
//!   every peer's presence arrives as [`Event::Presence`].
//! - Everything that happens comes out of [`Drive::events`], one stream,
//!   for the UI layer to turn into its own state.
//!
//! The browser keeps a snapshot of each open document in IndexedDB;
//! edits made offline are sent when the connection comes back (the join
//! handshake exchanges version vectors, so nothing is lost or repeated).

mod idb;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use drive_proto::{DocId, DocInfo, Peer, ToClient, ToNode};
use futures_channel::mpsc;
pub use loro;
use loro::{ExportMode, LoroDoc, Subscription, VersionVector};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{BinaryType, MessageEvent, WebSocket};

/// The stream of [`Event`]s a [`Drive`] produces.
pub type EventStream = mpsc::UnboundedReceiver<Event>;

/// Wait, in the browser.
pub async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
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

/// Who this browser is: a display name and a colour, kept in
/// `localStorage`, shared by every app on the same address.
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
    me: RefCell<Me>,
    ws: RefCell<Option<WebSocket>>,
    status: Cell<Status>,
    peer: Cell<Option<Peer>>,
    docs: RefCell<HashMap<DocId, OpenDoc>>,
    events: mpsc::UnboundedSender<Event>,
    retry_ms: Cell<u32>,
    /// The issuer's access token, sent in the handshake (none: the node
    /// is open, or this is its own computer).
    token: RefCell<Option<String>>,
    /// Who this browser's changes are by (in every change's history).
    author: RefCell<String>,
}

/// A connection to a Drive node, and the documents open on it.
#[derive(Clone)]
pub struct Drive(Rc<Inner>);

impl PartialEq for Drive {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

impl Drive {
    /// Connect to the node at `url` (`wss://host/sync`); `None` connects to
    /// the page's own host. Returns the client and its event stream.
    pub fn connect(url: Option<String>) -> (Drive, mpsc::UnboundedReceiver<Event>) {
        Drive::connect_with(url, None)
    }

    /// [`Drive::connect`], signed in from the first handshake on.
    pub fn connect_with(
        url: Option<String>,
        token: Option<String>,
    ) -> (Drive, mpsc::UnboundedReceiver<Event>) {
        let url = url.unwrap_or_else(|| {
            let loc = web_sys::window().expect("a window").location();
            let proto = if loc.protocol().unwrap_or_default() == "https:" {
                "wss"
            } else {
                "ws"
            };
            format!("{proto}://{}/sync", loc.host().unwrap_or_default())
        });
        let (tx, rx) = mpsc::unbounded();
        let me = storage()
            .and_then(|s| s.get_item("drive:me").ok().flatten())
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| Me {
                name: guest_name(),
                hue: (js_sys::Math::random() * 360.0) as u32,
            });
        let drive = Drive(Rc::new(Inner {
            url,
            me: RefCell::new(me),
            ws: RefCell::new(None),
            status: Cell::new(Status::Connecting),
            peer: Cell::new(None),
            docs: RefCell::new(HashMap::new()),
            events: tx,
            retry_ms: Cell::new(500),
            token: RefCell::new(token),
            author: RefCell::new(String::new()),
        }));
        drive.save_me();
        drop_old_copies();
        drive.open_socket();
        (drive, rx)
    }

    /// Use this access token from the next handshake on (a fresh one
    /// after a refresh reconnects, so the node sees it).
    pub fn set_token(&self, token: Option<String>) {
        let changed = *self.0.token.borrow() != token;
        *self.0.token.borrow_mut() = token;
        if changed && let Some(ws) = self.0.ws.borrow().as_ref() {
            let _ = ws.close();
        }
    }

    /// Who the changes made here are by: the signed-in person's email, or
    /// this browser's name.
    pub fn set_author(&self, who: &str) {
        *self.0.author.borrow_mut() = who.to_string();
        for d in self.0.docs.borrow().values() {
            d.doc.set_next_commit_message(who);
        }
    }

    fn author(&self) -> String {
        let a = self.0.author.borrow().clone();
        if a.is_empty() {
            self.0.me.borrow().name.clone()
        } else {
            a
        }
    }

    pub fn status(&self) -> Status {
        self.0.status.get()
    }

    /// This connection's peer number, once the node has said it.
    pub fn peer(&self) -> Option<Peer> {
        self.0.peer.get()
    }

    pub fn me(&self) -> Me {
        self.0.me.borrow().clone()
    }

    /// Change your display name and colour (every app on this address).
    pub fn set_me(&self, me: Me) {
        *self.0.me.borrow_mut() = me;
        self.save_me();
        // A new name reaches the node on the next connection.
        if let Some(ws) = self.0.ws.borrow().as_ref() {
            let _ = ws.close();
        }
    }

    fn save_me(&self) {
        if let (Some(s), Ok(j)) = (storage(), serde_json::to_string(&*self.0.me.borrow())) {
            let _ = s.set_item("drive:me", &j);
        }
    }

    /// Open a document: this browser's copy now, the node's as it arrives.
    pub fn open(&self, id: &str) -> LoroDoc {
        if let Some(d) = self.0.docs.borrow().get(id) {
            return d.doc.clone();
        }
        let doc = LoroDoc::new();
        drive_spine::history::prepare(&doc, &self.author());
        // This browser's copy arrives a moment later (IndexedDB is async);
        // it merges with whatever the node has sent by then.
        {
            let me = self.clone();
            let id = id.to_string();
            wasm_bindgen_futures::spawn_local(async move {
                let Some(bytes) = idb::load(&id).await else {
                    return;
                };
                let ok =
                    me.0.docs
                        .borrow()
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
        self.0.docs.borrow_mut().insert(
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
        if self.0.docs.borrow_mut().remove(id).is_some() {
            self.send(&ToNode::Leave {
                doc: id.to_string(),
            });
        }
    }

    /// Commit the edits made to a document and send them.
    pub fn commit(&self, id: &str) {
        let pending = {
            let docs = self.0.docs.borrow();
            let Some(d) = docs.get(id) else { return };
            drive_spine::history::commit(&d.doc, &self.author());
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
        self.0.docs.borrow().get(id).is_some_and(|d| d.synced)
    }

    /// Merge exported Loro bytes made elsewhere (a template, a file) into a
    /// document, and send them, as if they were edits made here.
    pub fn import_local(&self, id: &str, bytes: &[u8]) {
        let ok = self
            .0
            .docs
            .borrow()
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
        let fresh = self
            .0
            .docs
            .borrow()
            .get(id)
            .is_some_and(|d| d.my_presence.is_none());
        if fresh {
            self.set_presence(id, json);
        }
    }

    /// Say where you are in a document. Never stored; others see it live.
    pub fn set_presence(&self, id: &str, json: String) {
        if let Some(d) = self.0.docs.borrow_mut().get_mut(id) {
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
        if self.0.status.get() != s {
            self.0.status.set(s);
            self.emit(Event::Status(s));
        }
    }

    fn send(&self, msg: &ToNode) {
        if let Some(ws) = self.0.ws.borrow().as_ref()
            && ws.ready_state() == WebSocket::OPEN
        {
            let _ = ws.send_with_u8_array(&drive_proto::encode(msg));
        }
    }

    fn join(&self, id: &str) {
        let vv = self
            .0
            .docs
            .borrow()
            .get(id)
            .map(|d| d.doc.oplog_vv().encode());
        if let Some(vv) = vv {
            self.send(&ToNode::Join {
                doc: id.to_string(),
                version_vector: vv,
            });
        }
    }

    /// A document changed: tell the UI, and save this browser's copy soon.
    fn changed(&self, id: &str) {
        self.emit(Event::Changed(id.to_string()));
        let schedule = {
            let mut docs = self.0.docs.borrow_mut();
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
            gloo_timers::callback::Timeout::new(600, move || me.save_local(&id)).forget();
        }
    }

    fn save_local(&self, id: &str) {
        let snapshot = {
            let mut docs = self.0.docs.borrow_mut();
            let Some(d) = docs.get_mut(id) else { return };
            d.save_pending = false;
            d.doc.export(ExportMode::Snapshot).ok()
        };
        if let Some(bytes) = snapshot {
            let id = id.to_string();
            wasm_bindgen_futures::spawn_local(async move { idb::save(&id, &bytes).await });
        }
    }

    fn open_socket(&self) {
        self.set_status(Status::Connecting);
        let Ok(ws) = WebSocket::new(&self.0.url) else {
            self.retry();
            return;
        };
        ws.set_binary_type(BinaryType::Arraybuffer);

        let me = self.clone();
        let onopen = Closure::<dyn FnMut()>::new(move || {
            me.0.retry_ms.set(500);
            let name = me.0.me.borrow().name.clone();
            let token = me.0.token.borrow().clone();
            me.send(&ToNode::Hello {
                version: drive_proto::VERSION,
                name,
                token,
            });
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let me = self.clone();
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                if let Ok(msg) = drive_proto::decode::<ToClient>(&bytes) {
                    me.receive(msg);
                }
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let me = self.clone();
        let onclose = Closure::<dyn FnMut()>::new(move || {
            me.0.peer.set(None);
            me.set_status(Status::Offline);
            me.retry();
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        *self.0.ws.borrow_mut() = Some(ws);
    }

    fn retry(&self) {
        let ms = self.0.retry_ms.get();
        self.0.retry_ms.set((ms * 2).min(8000));
        let me = self.clone();
        gloo_timers::callback::Timeout::new(ms, move || me.open_socket()).forget();
    }

    fn receive(&self, msg: ToClient) {
        match msg {
            ToClient::Welcome { peer } => {
                self.0.peer.set(Some(peer));
                self.set_status(Status::Online);
                let ids: Vec<DocId> = self.0.docs.borrow().keys().cloned().collect();
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
                    let mut docs = self.0.docs.borrow_mut();
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
                let ok = self
                    .0
                    .docs
                    .borrow()
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
            ToClient::Error { .. } | ToClient::Fetch { .. } => {}
        }
    }
}

/// Copies kept in `localStorage` before IndexedDB: dropped (they were
/// under ids from before orgs, and the node has every one of them).
fn drop_old_copies() {
    let Some(s) = storage() else { return };
    let n = s.length().unwrap_or(0);
    let old: Vec<String> = (0..n)
        .filter_map(|i| s.key(i).ok().flatten())
        .filter(|k| k.starts_with("drive:doc:"))
        .collect();
    for k in old {
        let _ = s.remove_item(&k);
    }
}

fn guest_name() -> String {
    const N: [&str; 12] = [
        "Otter", "Heron", "Lynx", "Robin", "Maple", "Comet", "Pike", "Wren", "Fern", "Juniper",
        "Basil", "Sage",
    ];
    let i = (js_sys::Math::random() * N.len() as f64) as usize % N.len();
    format!("Guest {}", N[i])
}
