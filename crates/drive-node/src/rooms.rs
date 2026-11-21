//! Rooms: each open document, the peers that have it open, and their
//! presence. The node is one more peer of every document: it merges what
//! it is sent, keeps it, and passes it on; it never decides who wins.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use drive_proto::{DocId, DocInfo, Peer, ToClient, ToNode};
use futures::{SinkExt, StreamExt};
use loro::{ExportMode, LoroDoc, VersionVector};
use tokio::sync::{Mutex, mpsc};

use crate::store::Store;

/// One open document.
struct Room {
    doc: LoroDoc,
    /// Who has it open: their outbox.
    peers: HashMap<Peer, Outbox>,
    /// Their latest presence: (name, json).
    presence: HashMap<Peer, (String, String)>,
    /// Changed since it was last saved.
    dirty: bool,
}

type Outbox = mpsc::UnboundedSender<ToClient>;

pub struct Rooms {
    rooms: Mutex<HashMap<DocId, Room>>,
    store: Store,
    next_peer: AtomicU64,
}

impl Rooms {
    pub fn new(store: Store) -> Rooms {
        Rooms { rooms: Mutex::new(HashMap::new()), store, next_peer: AtomicU64::new(1) }
    }

    /// The room for a document, loaded from the store the first time.
    async fn ensure<'a>(&self, rooms: &'a mut HashMap<DocId, Room>, id: &str) -> &'a mut Room {
        if !rooms.contains_key(id) {
            let doc = LoroDoc::new();
            match self.store.load(id).await {
                Ok(Some(snapshot)) => {
                    if let Err(e) = doc.import(&snapshot) {
                        tracing::warn!(doc.id = %id, error = %e, "a stored snapshot did not import");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(doc.id = %id, error = %e, "the store could not load a document"),
            }
            rooms.insert(
                id.to_string(),
                Room { doc, peers: HashMap::new(), presence: HashMap::new(), dirty: false },
            );
        }
        rooms.get_mut(id).expect("just inserted")
    }

    /// Save every changed document, every two seconds.
    pub async fn flush_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.flush().await;
        }
    }

    async fn flush(&self) {
        let dirty: Vec<(DocId, Vec<u8>)> = {
            let mut rooms = self.rooms.lock().await;
            rooms
                .iter_mut()
                .filter(|(_, r)| r.dirty)
                .filter_map(|(id, r)| {
                    r.dirty = false;
                    r.doc.export(ExportMode::Snapshot).ok().map(|s| (id.clone(), s))
                })
                .collect()
        };
        for (id, snapshot) in dirty {
            if let Err(e) = self.store.save(&id, snapshot).await {
                tracing::warn!(doc.id = %id, error = %e, "a document did not save");
            }
        }
    }

    async fn handle(&self, peer: Peer, name: &str, out: &Outbox, joined: &mut HashSet<DocId>, msg: ToNode) {
        match msg {
            ToNode::Hello { .. } => {}
            ToNode::Join { doc: id, version_vector } => {
                let mut rooms = self.rooms.lock().await;
                let room = self.ensure(&mut rooms, &id).await;
                let theirs = VersionVector::decode(&version_vector).unwrap_or_default();
                let bytes = room.doc.export(ExportMode::updates(&theirs)).unwrap_or_default();
                let _ = out.send(ToClient::Synced {
                    doc: id.clone(),
                    bytes,
                    version_vector: room.doc.oplog_vv().encode(),
                });
                for (p, (n, json)) in &room.presence {
                    let _ = out.send(ToClient::Presence {
                        doc: id.clone(),
                        peer: *p,
                        name: n.clone(),
                        json: Some(json.clone()),
                    });
                }
                room.peers.insert(peer, out.clone());
                joined.insert(id);
            }
            ToNode::Update { doc: id, bytes } => {
                let mut rooms = self.rooms.lock().await;
                let room = self.ensure(&mut rooms, &id).await;
                match room.doc.import(&bytes) {
                    Ok(_) => {
                        room.dirty = true;
                        for (p, o) in &room.peers {
                            if *p != peer {
                                let _ = o.send(ToClient::Update { doc: id.clone(), bytes: bytes.clone() });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(doc.id = %id, peer, error = %e, "an update did not import");
                        let _ = out.send(ToClient::Error { message: format!("update refused: {e}") });
                    }
                }
            }
            ToNode::Presence { doc: id, json } => {
                let mut rooms = self.rooms.lock().await;
                if let Some(room) = rooms.get_mut(&id) {
                    room.presence.insert(peer, (name.to_string(), json.clone()));
                    for (p, o) in &room.peers {
                        if *p != peer {
                            let _ = o.send(ToClient::Presence {
                                doc: id.clone(),
                                peer,
                                name: name.to_string(),
                                json: Some(json.clone()),
                            });
                        }
                    }
                }
            }
            ToNode::Leave { doc: id } => {
                self.leave(peer, name, &id).await;
                joined.remove(&id);
            }
            ToNode::List { prefix } => {
                let open: HashMap<DocId, u32> = {
                    let rooms = self.rooms.lock().await;
                    rooms.iter().map(|(id, r)| (id.clone(), r.peers.len() as u32)).collect()
                };
                let mut docs: Vec<DocInfo> = match self.store.list().await {
                    Ok(list) => list
                        .into_iter()
                        .filter(|(id, _)| id.starts_with(&prefix))
                        .map(|(id, bytes)| DocInfo { peers: open.get(&id).copied().unwrap_or(0), id, bytes })
                        .collect(),
                    Err(_) => Vec::new(),
                };
                for (id, peers) in open {
                    if id.starts_with(&prefix) && !docs.iter().any(|d| d.id == id) {
                        docs.push(DocInfo { id, peers, bytes: 0 });
                    }
                }
                docs.sort_by(|a, b| a.id.cmp(&b.id));
                let _ = out.send(ToClient::Docs { prefix, docs });
            }
        }
    }

    async fn leave(&self, peer: Peer, name: &str, id: &str) {
        let mut rooms = self.rooms.lock().await;
        if let Some(room) = rooms.get_mut(id) {
            room.peers.remove(&peer);
            if room.presence.remove(&peer).is_some() {
                for o in room.peers.values() {
                    let _ = o.send(ToClient::Presence {
                        doc: id.to_string(),
                        peer,
                        name: name.to_string(),
                        json: None,
                    });
                }
            }
        }
    }
}

/// One client's connection, from its first message to its last.
pub async fn serve(rooms: Arc<Rooms>, socket: WebSocket) {
    let peer = rooms.next_peer.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (out, mut outbox) = mpsc::unbounded_channel::<ToClient>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbox.recv().await {
            if sink.send(Message::Binary(drive_proto::encode(&msg).into())).await.is_err() {
                break;
            }
        }
    });

    let mut name = String::from("Guest");
    let mut joined = HashSet::new();
    while let Some(Ok(frame)) = stream.next().await {
        let Message::Binary(bytes) = frame else { continue };
        let msg = match drive_proto::decode::<ToNode>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(peer, error = %e, "a message did not decode");
                continue;
            }
        };
        if let ToNode::Hello { version, name: n } = &msg {
            if *version != drive_proto::VERSION {
                let _ = out.send(ToClient::Error { message: "protocol version mismatch".into() });
                break;
            }
            name = n.clone();
            let _ = out.send(ToClient::Welcome { peer });
            continue;
        }
        rooms.handle(peer, &name, &out, &mut joined, msg).await;
    }
    for id in joined {
        rooms.leave(peer, &name, &id).await;
    }
    drop(out);
    let _ = writer.await;
}
