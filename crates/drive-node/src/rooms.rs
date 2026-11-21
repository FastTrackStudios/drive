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
use tokio::sync::{Mutex, mpsc, oneshot};

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
    pub store: Store,
    next_peer: AtomicU64,
    /// Connected agents, by (space, device): their peer and outbox.
    agents: Mutex<HashMap<(String, String), (Peer, Outbox)>>,
    /// Fetches waiting for an agent's answer.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    next_req: AtomicU64,
    /// Who may read and write what (the issuer's orgs).
    pub auth: crate::auth::Auth,
}

/// Why a file could not be fetched from its device.
#[derive(Debug)]
pub enum FetchError {
    /// No such location in the space.
    NoLocation,
    /// The location's device has no agent connected.
    Offline(String),
    /// It did not answer in time.
    Timeout,
    /// It answered that it could not.
    Refused(String),
}

impl Rooms {
    pub fn new(store: Store, auth: crate::auth::Auth) -> Rooms {
        Rooms {
            auth,
            rooms: Mutex::new(HashMap::new()),
            store,
            next_peer: AtomicU64::new(1),
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_req: AtomicU64::new(1),
        }
    }

    /// Whether someone may read and write a document: their org's, or one
    /// of a project another org shared with one of their orgs (that org's
    /// registry, to see the project, and the project's documents).
    pub async fn may(&self, p: &crate::auth::Principal, id: &str) -> bool {
        if p.all {
            return true;
        }
        let Some(org) = drive_proto::org_of(id) else {
            return false;
        };
        if p.member_of(org) {
            return true;
        }
        let registry = drive_files::space_doc(org);
        let shared: Vec<drive_files::projects::Project> = self
            .read(&registry, drive_files::projects::projects)
            .await
            .into_iter()
            .filter(|pr| pr.shared_with.iter().any(|o| p.member_of(o)))
            .collect();
        if shared.is_empty() {
            return false;
        }
        if id == registry {
            return true;
        }
        let rest = drive_proto::unqualify(org, id);
        shared.iter().any(|pr| pr.docs.iter().any(|d| d == rest))
    }

    fn refuse(out: &Outbox, who: &crate::auth::Principal, id: &str) {
        tracing::warn!(auth.outcome = "denied", auth.who = %who.who, doc.org = drive_proto::org_of(id).unwrap_or(""), "a document outside the person's orgs was asked for");
        let _ = out.send(ToClient::Error {
            message: format!("not allowed: {id}"),
        });
    }

    /// Change a document on the node itself (a guest's comment through a
    /// share link): the edit is committed under `author`, saved, and sent
    /// to everyone who has the document open.
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
            room.dirty = true;
            for o in room.peers.values() {
                let _ = o.send(ToClient::Update {
                    doc: id.to_string(),
                    bytes: bytes.clone(),
                });
            }
        }
        out
    }

    /// Read a document as the node holds it (loading it if needed).
    pub async fn read<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        f(&room.doc)
    }

    /// Which devices have an agent connected in `space`.
    pub async fn agents_online(&self, space: &str) -> Vec<String> {
        self.agents
            .lock()
            .await
            .keys()
            .filter(|(s, _)| s == space)
            .map(|(_, d)| d.clone())
            .collect()
    }

    /// Ask the device that owns `location` to put `path` on the node;
    /// returns the blob's hash.
    pub async fn fetch(
        &self,
        space: &str,
        location: &str,
        path: &str,
    ) -> Result<String, FetchError> {
        let device = self
            .read(&drive_files::space_doc(space), |d| {
                drive_files::space::locations(d)
                    .into_iter()
                    .find(|l| l.id == location)
                    .map(|l| l.device)
            })
            .await
            .ok_or(FetchError::NoLocation)?;
        let outbox = self
            .agents
            .lock()
            .await
            .get(&(space.to_string(), device.clone()))
            .map(|(_, o)| o.clone())
            .ok_or_else(|| FetchError::Offline(device.clone()))?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req, tx);
        if outbox
            .send(ToClient::Fetch {
                req,
                location: location.into(),
                path: path.into(),
            })
            .is_err()
        {
            self.pending.lock().await.remove(&req);
            return Err(FetchError::Offline(device));
        }
        let answer = tokio::time::timeout(Duration::from_secs(120), rx).await;
        self.pending.lock().await.remove(&req);
        match answer {
            Ok(Ok(Ok(hash))) => Ok(hash),
            Ok(Ok(Err(why))) => Err(FetchError::Refused(why)),
            Ok(Err(_)) => Err(FetchError::Offline(device)),
            Err(_) => Err(FetchError::Timeout),
        }
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
                Err(e) => {
                    tracing::warn!(doc.id = %id, error = %e, "the store could not load a document")
                }
            }
            rooms.insert(
                id.to_string(),
                Room {
                    doc,
                    peers: HashMap::new(),
                    presence: HashMap::new(),
                    dirty: false,
                },
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

    /// Save every changed document now (before a backup).
    pub async fn flush_now(&self) {
        self.flush().await;
    }

    async fn flush(&self) {
        let dirty: Vec<(DocId, Vec<u8>)> = {
            let mut rooms = self.rooms.lock().await;
            rooms
                .iter_mut()
                .filter(|(_, r)| r.dirty)
                .filter_map(|(id, r)| {
                    r.dirty = false;
                    r.doc
                        .export(ExportMode::Snapshot)
                        .ok()
                        .map(|s| (id.clone(), s))
                })
                .collect()
        };
        for (id, snapshot) in dirty {
            if let Err(e) = self.store.save(&id, snapshot).await {
                tracing::warn!(doc.id = %id, error = %e, "a document did not save");
            }
        }
    }

    async fn handle(
        &self,
        peer: Peer,
        name: &str,
        who: &crate::auth::Principal,
        out: &Outbox,
        joined: &mut HashSet<DocId>,
        msg: ToNode,
    ) {
        // Every document a message names must be the person's to touch.
        let named = match &msg {
            ToNode::Join { doc, .. }
            | ToNode::Update { doc, .. }
            | ToNode::Presence { doc, .. } => Some(doc.clone()),
            ToNode::Agent { space, .. } => Some(drive_files::space_doc(space)),
            _ => None,
        };
        if let Some(id) = named
            && !self.may(who, &id).await
        {
            Self::refuse(out, who, &id);
            return;
        }
        match msg {
            ToNode::Hello { .. } => {}
            ToNode::Join {
                doc: id,
                version_vector,
            } => {
                let mut rooms = self.rooms.lock().await;
                let room = self.ensure(&mut rooms, &id).await;
                let theirs = VersionVector::decode(&version_vector).unwrap_or_default();
                let bytes = room
                    .doc
                    .export(ExportMode::updates(&theirs))
                    .unwrap_or_default();
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
                                let _ = o.send(ToClient::Update {
                                    doc: id.clone(),
                                    bytes: bytes.clone(),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(doc.id = %id, peer, error = %e, "an update did not import");
                        let _ = out.send(ToClient::Error {
                            message: format!("update refused: {e}"),
                        });
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
            ToNode::Agent { space, device } => {
                tracing::info!(agent.space = %space, agent.device = %device, peer, "an agent connected");
                self.agents
                    .lock()
                    .await
                    .insert((space, device), (peer, out.clone()));
            }
            ToNode::Fetched { req, result } => {
                if let Some(tx) = self.pending.lock().await.remove(&req) {
                    let _ = tx.send(result);
                }
            }
            ToNode::Leave { doc: id } => {
                self.leave(peer, name, &id).await;
                joined.remove(&id);
            }
            ToNode::List { prefix } => {
                let open: HashMap<DocId, u32> = {
                    let rooms = self.rooms.lock().await;
                    rooms
                        .iter()
                        .map(|(id, r)| (id.clone(), r.peers.len() as u32))
                        .collect()
                };
                // `*/studio:` lists that prefix in every org.
                let matches = |id: &str| match prefix.strip_prefix("*/") {
                    Some(rest) => id.split_once('/').is_some_and(|(_, r)| r.starts_with(rest)),
                    None => id.starts_with(&prefix),
                };
                let mut docs: Vec<DocInfo> = match self.store.list().await {
                    Ok(list) => list
                        .into_iter()
                        .filter(|(id, _)| matches(id))
                        .map(|(id, bytes)| DocInfo {
                            peers: open.get(&id).copied().unwrap_or(0),
                            id,
                            bytes,
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
                if !who.all {
                    let mut kept = Vec::new();
                    for d in docs {
                        if self.may(who, &d.id).await {
                            kept.push(d);
                        }
                    }
                    docs = kept;
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
pub async fn serve(rooms: Arc<Rooms>, socket: WebSocket, local: bool) {
    let peer = rooms.next_peer.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (out, mut outbox) = mpsc::unbounded_channel::<ToClient>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbox.recv().await {
            if sink
                .send(Message::Binary(drive_proto::encode(&msg).into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut name = String::from("Guest");
    let mut who: Option<crate::auth::Principal> = None;
    let mut joined = HashSet::new();
    while let Some(Ok(frame)) = stream.next().await {
        let Message::Binary(bytes) = frame else {
            continue;
        };
        let msg = match drive_proto::decode::<ToNode>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(peer, error = %e, "a message did not decode");
                continue;
            }
        };
        if let ToNode::Hello {
            version,
            name: n,
            token,
        } = &msg
        {
            if *version != drive_proto::VERSION {
                let _ = out.send(ToClient::Error {
                    message: "protocol version mismatch".into(),
                });
                break;
            }
            match rooms.auth.principal(token.as_deref(), local).await {
                Some(p) => who = Some(p),
                None => {
                    let _ = out.send(ToClient::Error {
                        message: "sign in".into(),
                    });
                    break;
                }
            }
            name = n.clone();
            let _ = out.send(ToClient::Welcome { peer });
            continue;
        }
        let Some(p) = &who else { continue };
        rooms.handle(peer, &name, p, &out, &mut joined, msg).await;
    }
    for id in joined {
        rooms.leave(peer, &name, &id).await;
    }
    rooms.agents.lock().await.retain(|_, (p, _)| *p != peer);
    drop(out);
    let _ = writer.await;
}
