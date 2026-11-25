//! Rooms: each open document, the peers that have it open, and their
//! presence.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use drive_proto::{DocId, DocInfo, Peer, ToClient};
use loro::{ExportMode, LoroDoc, VersionVector};
use tokio::sync::{Mutex, mpsc};

use crate::Store;

/// Where a connection's messages to its peer go.
pub type Outbox = mpsc::UnboundedSender<ToClient>;

/// One open document.
struct Room {
    doc: LoroDoc,
    /// Who has it open: their outbox.
    peers: HashMap<Peer, Outbox>,
    /// Their latest presence: (name, json).
    presence: HashMap<Peer, (String, String)>,
    /// Bumped on every edit/update; saved tracks the latest flushed revision.
    dirty: u64,
    saved: u64,
    /// When it was last read, changed or joined: a room nobody has open,
    /// with nothing left to save, goes once it has been idle for [`IDLE`].
    used: Instant,
}

/// How long a room nobody has open stays loaded after its last use.
pub const IDLE: Duration = Duration::from_secs(60);

impl Room {
    fn from_snapshot(id: &str, snapshot: Option<Vec<u8>>) -> Room {
        let doc = LoroDoc::new();
        if let Some(snapshot) = snapshot
            && let Err(e) = doc.import(&snapshot)
        {
            tracing::warn!(doc.id = %id, error = %e, "a stored snapshot did not import");
        }
        Room {
            doc,
            peers: HashMap::new(),
            presence: HashMap::new(),
            dirty: 0,
            saved: 0,
            used: Instant::now(),
        }
    }
}

/// What a link hears about this device's own interest in documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Interest {
    /// A document got its first peer on this device (a window opened it).
    Opened(DocId),
    /// Its last one here left.
    Closed(DocId),
}

/// Who each peer is: which other device its connection is with (none:
/// this device, a window of the app), and which are links.
#[derive(Default)]
struct Book {
    via: HashMap<Peer, String>,
    links: HashSet<Peer>,
    watchers: Vec<mpsc::UnboundedSender<Interest>>,
}

impl Book {
    fn local(&self, p: Peer) -> bool {
        !self.via.contains_key(&p) && !self.links.contains(&p)
    }

    fn tell(&mut self, i: Interest) {
        self.watchers.retain(|w| w.send(i.clone()).is_ok());
    }
}

pub struct Rooms {
    rooms: Mutex<HashMap<DocId, Room>>,
    /// Serialises flushes so an older snapshot never overwrites a newer one.
    flushing: Mutex<()>,
    store: Arc<dyn Store>,
    next_peer: AtomicU64,
    book: std::sync::Mutex<Book>,
}

impl Rooms {
    pub fn new(store: Arc<dyn Store>) -> Rooms {
        Rooms {
            rooms: Mutex::new(HashMap::new()),
            flushing: Mutex::new(()),
            store,
            next_peer: AtomicU64::new(1),
            book: std::sync::Mutex::new(Book::default()),
        }
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new connection's peer number. `via`: the other device it is with
    /// (its endpoint id), when it is not a window on this one.
    pub fn new_peer(&self, via: Option<&str>) -> Peer {
        let p = self.next_peer.fetch_add(1, Ordering::Relaxed);
        if let Some(v) = via {
            self.book().via.insert(p, v.to_string());
        }
        p
    }

    /// A link's peer number (it stands for `via` in every room it joins).
    pub(crate) fn new_link(&self, via: &str) -> Peer {
        let p = self.new_peer(Some(via));
        self.book().links.insert(p);
        p
    }

    /// A connection is over: forget who it was.
    pub fn forget_peer(&self, p: Peer) {
        let mut b = self.book();
        b.via.remove(&p);
        b.links.remove(&p);
    }

    /// Hear when this device's own interest in a document starts and ends.
    pub(crate) fn watch(&self) -> mpsc::UnboundedReceiver<Interest> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.book().watchers.push(tx);
        rx
    }

    /// Send `msg` from `from` to everyone else in `room`: once per other
    /// device, never back to the device it came from.
    fn fan(book: &Book, room: &Room, from: Peer, msg: &ToClient) {
        let source = book.via.get(&from);
        let mut sent: HashSet<&String> = HashSet::new();
        for (p, o) in &room.peers {
            if *p == from {
                continue;
            }
            if let Some(v) = book.via.get(p)
                && (Some(v) == source || !sent.insert(v))
            {
                continue;
            }
            let _ = o.send(msg.clone());
        }
    }

    /// The room for a document, loaded from the store the first time.
    async fn ensure<'a>(&self, rooms: &'a mut HashMap<DocId, Room>, id: &str) -> &'a mut Room {
        if !rooms.contains_key(id) {
            let snapshot = match self.store.load(id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(doc.id = %id, error = %e, "the store could not load a document");
                    None
                }
            };
            rooms.insert(id.to_string(), Room::from_snapshot(id, snapshot));
        }
        let room = rooms.get_mut(id).expect("just inserted");
        room.used = Instant::now();
        room
    }

    /// `peer` is in the room now; tell links if it is the first from here.
    fn enter(&self, room: &mut Room, peer: Peer, out: &Outbox, id: &str) {
        let mut book = self.book();
        let first_here = book.local(peer) && !room.peers.keys().any(|p| book.local(*p));
        room.peers.insert(peer, out.clone());
        if first_here {
            book.tell(Interest::Opened(id.to_string()));
        }
    }

    /// Open a document for `peer`, which has `version_vector` of it: it is
    /// sent what it lacks and this room's version
    /// ([`ToClient::Synced`]), then everyone's presence.
    pub async fn join(&self, peer: Peer, out: &Outbox, id: &str, version_vector: &[u8]) {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        let theirs = VersionVector::decode(version_vector).unwrap_or_default();
        let bytes = room
            .doc
            .export(ExportMode::updates(&theirs))
            .unwrap_or_default();
        let _ = out.send(ToClient::Synced {
            doc: id.to_string(),
            bytes,
            version_vector: room.doc.oplog_vv().encode(),
        });
        for (p, (n, json)) in &room.presence {
            let _ = out.send(ToClient::Presence {
                doc: id.to_string(),
                peer: *p,
                name: n.clone(),
                json: Some(json.clone()),
            });
        }
        self.enter(room, peer, out, id);
    }

    /// A link's side of a join: it is in the room (and hears its updates)
    /// from now on; the answer is the room's version, for the link to ask
    /// its peer for what this room lacks.
    pub(crate) async fn attach(&self, peer: Peer, out: &Outbox, id: &str) -> Vec<u8> {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        room.peers.insert(peer, out.clone());
        room.doc.oplog_vv().encode()
    }

    /// What a room has that a peer with `version_vector` lacks (`None`:
    /// nothing).
    pub(crate) async fn since(&self, id: &str, version_vector: &[u8]) -> Option<Vec<u8>> {
        let theirs = VersionVector::decode(version_vector).unwrap_or_default();
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        if theirs.includes_vv(&room.doc.oplog_vv()) {
            return None;
        }
        room.doc.export(ExportMode::updates(&theirs)).ok()
    }

    /// Whether `p` is a window on this device (not another device's peer,
    /// nor a link).
    pub(crate) fn is_local(&self, p: Peer) -> bool {
        self.book().local(p)
    }

    /// Updates from `peer`: merged, and what was new here passed on to
    /// everyone else. One that does not import is refused on `out`.
    /// Whether anything was new.
    pub async fn update(&self, peer: Peer, out: Option<&Outbox>, id: &str, bytes: &[u8]) -> bool {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        let before = room.doc.oplog_vv();
        match room.doc.import(bytes) {
            Ok(_) => {
                let new = room
                    .doc
                    .export(ExportMode::updates(&before))
                    .unwrap_or_default();
                if room.doc.oplog_vv() == before {
                    return false;
                }
                room.dirty += 1;
                let book = self.book();
                Self::fan(
                    &book,
                    room,
                    peer,
                    &ToClient::Update {
                        doc: id.to_string(),
                        bytes: new,
                    },
                );
                true
            }
            Err(e) => {
                tracing::warn!(doc.id = %id, peer, error = %e, "an update did not import");
                if let Some(out) = out {
                    let _ = out.send(ToClient::Error {
                        message: format!("update refused: {e}"),
                    });
                }
                false
            }
        }
    }

    /// `who`'s presence in a document (`None`: gone), from the connection
    /// `conn` (the same peer, or a link passing on a peer of another
    /// device's), which must have it open.
    pub async fn presence(
        &self,
        conn: Peer,
        who: Peer,
        name: &str,
        id: &str,
        json: Option<String>,
    ) {
        let mut rooms = self.rooms.lock().await;
        let Some(room) = rooms.get_mut(id) else {
            return;
        };
        if !room.peers.contains_key(&conn) {
            return;
        }
        match &json {
            Some(j) => {
                room.presence.insert(who, (name.to_string(), j.clone()));
            }
            None => {
                if room.presence.remove(&who).is_none() {
                    return;
                }
            }
        }
        let book = self.book();
        Self::fan(
            &book,
            room,
            conn,
            &ToClient::Presence {
                doc: id.to_string(),
                peer: who,
                name: name.to_string(),
                json,
            },
        );
    }

    /// `peer` closed a document: its presence goes, and links hear when
    /// it was the last one here.
    pub async fn leave(&self, peer: Peer, name: &str, id: &str) {
        let mut rooms = self.rooms.lock().await;
        let Some(room) = rooms.get_mut(id) else {
            return;
        };
        room.peers.remove(&peer);
        let mut book = self.book();
        if room.presence.remove(&peer).is_some() {
            Self::fan(
                &book,
                room,
                peer,
                &ToClient::Presence {
                    doc: id.to_string(),
                    peer,
                    name: name.to_string(),
                    json: None,
                },
            );
        }
        if book.local(peer) && !room.peers.keys().any(|p| book.local(*p)) {
            book.tell(Interest::Closed(id.to_string()));
        }
    }

    /// The documents a window on this device has open.
    pub(crate) async fn open_here(&self) -> Vec<DocId> {
        let rooms = self.rooms.lock().await;
        let book = self.book();
        rooms
            .iter()
            .filter(|(_, r)| r.peers.keys().any(|p| book.local(*p)))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The presence windows on this device have set in a document.
    pub(crate) async fn presence_here(&self, id: &str) -> Vec<String> {
        let rooms = self.rooms.lock().await;
        let book = self.book();
        rooms.get(id).map_or_else(Vec::new, |r| {
            r.presence
                .iter()
                .filter(|(p, _)| book.local(**p))
                .map(|(_, (_, j))| j.clone())
                .collect()
        })
    }

    /// Which documents are stored or open whose id starts with `prefix`
    /// (`*/studio:` lists that prefix in every org), by id.
    pub async fn list(&self, prefix: &str) -> Vec<DocInfo> {
        let open: HashMap<DocId, u32> = {
            let rooms = self.rooms.lock().await;
            rooms
                .iter()
                .map(|(id, r)| (id.clone(), r.peers.len() as u32))
                .collect()
        };
        let matches = |id: &str| match prefix.strip_prefix("*/") {
            Some(rest) => id.split_once('/').is_some_and(|(_, r)| r.starts_with(rest)),
            None => id.starts_with(prefix),
        };
        let mut docs: Vec<DocInfo> = match self.store.list().await {
            Ok(list) => list
                .iter()
                .filter(|(id, _)| matches(id))
                .map(|(id, bytes)| DocInfo {
                    peers: open.get(id).copied().unwrap_or(0),
                    id: id.clone(),
                    bytes: *bytes,
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        for (id, peers) in open {
            if matches(&id) && !docs.iter().any(|d| d.id == id) {
                docs.push(DocInfo {
                    id,
                    peers,
                    bytes: 0,
                });
            }
        }
        docs.sort_by(|a, b| a.id.cmp(&b.id));
        docs
    }

    /// Change a document here (a guest's comment through a share link):
    /// the edit is committed under `author`, saved, and sent to everyone
    /// who has the document open.
    pub async fn edit<T>(&self, id: &str, author: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        let before = room.doc.oplog_vv();
        drive_spine::history::prepare(&room.doc, author);
        let out = f(&room.doc);
        drive_spine::history::commit(&room.doc, author);
        if let Ok(bytes) = room.doc.export(ExportMode::updates(&before))
            && !bytes.is_empty()
        {
            room.dirty += 1;
            for o in room.peers.values() {
                let _ = o.send(ToClient::Update {
                    doc: id.to_string(),
                    bytes: bytes.clone(),
                });
            }
        }
        out
    }

    /// Read a document as it is here (loading it if needed).
    pub async fn read<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        f(&room.doc)
    }

    /// Read a document as [`Rooms::read`] does, except that one the store
    /// does not have is read empty and opens no room: for what anyone may
    /// ask for without signing in, so made-up ids cannot fill memory.
    pub async fn peek<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        let mut rooms = self.rooms.lock().await;
        if !rooms.contains_key(id) {
            match self.store.load(id).await {
                Ok(Some(snapshot)) => {
                    rooms.insert(id.to_string(), Room::from_snapshot(id, Some(snapshot)));
                }
                Ok(None) => return f(&LoroDoc::new()),
                Err(e) => {
                    tracing::warn!(doc.id = %id, error = %e, "the store could not load a document");
                    return f(&LoroDoc::new());
                }
            }
        }
        let room = self.ensure(&mut rooms, id).await;
        f(&room.doc)
    }

    /// Read a document without opening a room for it: its room if it has
    /// one open, else its stored snapshot (empty when there is none).
    pub async fn inspect<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        if let Some(room) = self.rooms.lock().await.get(id) {
            return f(&room.doc);
        }
        let doc = LoroDoc::new();
        match self.store.load(id).await {
            Ok(Some(snapshot)) => {
                if let Err(e) = doc.import(&snapshot) {
                    tracing::warn!(doc.id = %id, error = %e, "a stored snapshot did not import");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(doc.id = %id, error = %e, "the store could not load a document")
            }
        }
        f(&doc)
    }

    /// How many connections have a document open here.
    pub async fn peers_in(&self, id: &str) -> usize {
        self.rooms.lock().await.get(id).map_or(0, |r| r.peers.len())
    }

    /// How many rooms are loaded.
    pub async fn loaded(&self) -> usize {
        self.rooms.lock().await.len()
    }

    /// Save every changed document, every two seconds.
    pub async fn flush_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.flush().await;
            self.evict_idle(IDLE).await;
        }
    }

    /// Let go of every room nobody has open, with nothing left to save,
    /// unused for `idle`: it is loaded again from the store when asked for.
    pub async fn evict_idle(&self, idle: Duration) {
        self.rooms
            .lock()
            .await
            .retain(|_, r| !r.peers.is_empty() || r.dirty > r.saved || r.used.elapsed() < idle);
    }

    /// Save every changed document now (before a backup, before quitting).
    pub async fn flush_now(&self) {
        self.flush().await;
    }

    async fn flush(&self) {
        let _guard = self.flushing.lock().await;
        let dirty: Vec<(DocId, u64, Vec<u8>)> = {
            let rooms = self.rooms.lock().await;
            rooms
                .iter()
                .filter(|(_, r)| r.dirty > r.saved)
                .filter_map(|(id, r)| {
                    r.doc
                        .export(ExportMode::Snapshot)
                        .ok()
                        .map(|s| (id.clone(), r.dirty, s))
                })
                .collect()
        };
        for (id, rev, snapshot) in dirty {
            match self.store.save(&id, snapshot).await {
                Ok(()) => {
                    if let Some(r) = self.rooms.lock().await.get_mut(&id) {
                        r.saved = r.saved.max(rev);
                    }
                }
                Err(e) => {
                    tracing::warn!(doc.id = %id, error = %e, "a document did not save");
                }
            }
        }
    }
}
