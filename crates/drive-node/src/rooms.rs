//! Rooms: each open document, the peers that have it open, and their
//! presence. The node is one more peer of every document: it merges what
//! it is sent, keeps it, and passes it on; it never decides who wins.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    /// Bumped on every edit/update; saved tracks the latest flushed revision.
    dirty: u64,
    saved: u64,
    /// When it was last read, changed or joined: a room nobody has open,
    /// with nothing left to save, goes once it has been idle for [`IDLE`].
    used: Instant,
}

/// How long a room nobody has open stays loaded after its last use.
const IDLE: Duration = Duration::from_secs(60);

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

type Outbox = mpsc::UnboundedSender<ToClient>;
type PendingFetch = (Peer, oneshot::Sender<Result<String, String>>);
type PendingBrowse = (Peer, oneshot::Sender<Result<drive_proto::Browsed, String>>);

pub struct Rooms {
    rooms: Mutex<HashMap<DocId, Room>>,
    /// Serialises flush_loop and flush_now so an older snapshot never overwrites a newer one.
    flushing: Mutex<()>,
    pub store: Store,
    next_peer: AtomicU64,
    /// Connected agents, by (space, device): their peer and outbox.
    agents: Mutex<HashMap<(String, String), (Peer, Outbox)>>,
    /// Fetches waiting for an agent's answer: (expected agent peer, sender).
    pending: Mutex<HashMap<u64, PendingFetch>>,
    /// Browse requests waiting for an agent's answer: (expected agent peer, sender).
    browsing: Mutex<HashMap<u64, PendingBrowse>>,
    next_req: AtomicU64,
    /// Who may read and write what (the issuer's orgs).
    pub auth: crate::auth::Auth,
    /// The node's iroh endpoint for files from devices (docs/transfer.md),
    /// when it runs one.
    pub transfer: std::sync::OnceLock<Arc<crate::transfer::Transfer>>,
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
            flushing: Mutex::new(()),
            store,
            next_peer: AtomicU64::new(1),
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            browsing: Mutex::new(HashMap::new()),
            next_req: AtomicU64::new(1),
            transfer: std::sync::OnceLock::new(),
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
        let projects = self
            .read(
                &drive_files::space_doc(org),
                drive_files::projects::projects,
            )
            .await;
        Self::lent(p, org, id, &projects)
    }

    /// Whether `org`'s `projects` (its registry's) let someone outside it
    /// see `id`: its registry, when a project is shared with one of their
    /// orgs, and that project's documents.
    fn lent(
        p: &crate::auth::Principal,
        org: &str,
        id: &str,
        projects: &[drive_files::projects::Project],
    ) -> bool {
        let mut shared = projects
            .iter()
            .filter(|pr| pr.shared_with.iter().any(|o| p.member_of(o)))
            .peekable();
        if shared.peek().is_none() {
            return false;
        }
        if id == drive_files::space_doc(org) {
            return true;
        }
        let rest = drive_proto::unqualify(org, id);
        shared.any(|pr| pr.docs.iter().any(|d| d == rest))
    }

    /// Whether someone may change a document: as [`Rooms::may`], except an
    /// org's registry, which only its members change (a project shared
    /// with another org is theirs to see, not to rewrite: the registry
    /// holds what the org lends to other orgs' projects, drive#52).
    pub async fn may_write(&self, p: &crate::auth::Principal, id: &str) -> bool {
        if p.all {
            return true;
        }
        if let Some(org) = drive_proto::org_of(id)
            && id == drive_files::space_doc(org)
        {
            return p.member_of(org);
        }
        self.may(p, id).await
    }

    /// Whether someone may read a file or folder (`path`) of a location of
    /// `space`: a member of that org may read anything of it; anyone else
    /// only what is inside a part of a project they may see (one of their
    /// orgs', or shared with one of their orgs), where the part is the
    /// org's own location or one it lends to that project
    /// ([`drive_files::projects::Lend`]). Nothing else of the org (drive#52).
    pub async fn may_read_file(
        &self,
        p: &crate::auth::Principal,
        space: &str,
        location: &str,
        path: &str,
    ) -> bool {
        use drive_files::projects::{self, Project};
        if p.member_of(space) {
            return true;
        }
        let path = path.trim_matches('/');
        if space.is_empty() || location.is_empty() || path.split('/').any(|c| c == ".." || c == ".")
        {
            return false;
        }
        let sees = |pr: &Project, org: &str| {
            p.member_of(org) || pr.shared_with.iter().any(|o| p.member_of(o))
        };
        let (own, lends) = self
            .read(&drive_files::space_doc(space), |d| {
                (projects::projects(d), projects::lends(d))
            })
            .await;
        // The org's own projects shared with one of theirs: their parts in
        // its own locations.
        let own_part = own.iter().filter(|pr| sees(pr, space)).any(|pr| {
            pr.parts.iter().any(|part| {
                part.space_in(space) == space && part.location == location && part.contains(path)
            })
        });
        if own_part {
            return true;
        }
        // What it lends to other orgs' projects they may see.
        for l in lends.iter().filter(|l| {
            l.org != space
                && l.location == location
                && (l.path.is_empty()
                    || path == l.path
                    || path.starts_with(&format!("{}/", l.path)))
        }) {
            let theirs = self
                .read(&drive_files::space_doc(&l.org), projects::projects)
                .await;
            let lent = theirs.iter().any(|pr| {
                sees(pr, &l.org) && pr.parts.iter().any(|part| l.matches(space, pr, part))
            });
            if lent {
                return true;
            }
        }
        false
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

    /// Read a document as the node holds it (loading it if needed).
    pub async fn read<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        let mut rooms = self.rooms.lock().await;
        let room = self.ensure(&mut rooms, id).await;
        f(&room.doc)
    }

    /// Read a document as [`Rooms::read`] does, except that one the store
    /// does not have is read empty and opens no room: for what anyone may
    /// ask for without signing in (a site, a share link), so made-up ids
    /// cannot fill the node's memory with rooms.
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
    async fn inspect<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
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

    /// Whether `device`'s agent is connected in `space`.
    pub async fn agent_online(&self, space: &str, device: &str) -> bool {
        self.agents
            .lock()
            .await
            .contains_key(&(space.to_string(), device.to_string()))
    }

    /// Ask `device`'s agent to look at its disks (docs/browse.md), and wait
    /// up to `wait` for its answer.
    pub async fn browse(
        &self,
        space: &str,
        device: &str,
        ask: drive_proto::Browse,
        wait: Duration,
    ) -> Result<drive_proto::Browsed, FetchError> {
        let (agent_peer, outbox) = self
            .agents
            .lock()
            .await
            .get(&(space.to_string(), device.to_string()))
            .map(|(p, o)| (*p, o.clone()))
            .ok_or_else(|| FetchError::Offline(device.to_string()))?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.browsing.lock().await.insert(req, (agent_peer, tx));
        if outbox.send(ToClient::Browse { req, ask }).is_err() {
            self.browsing.lock().await.remove(&req);
            return Err(FetchError::Offline(device.to_string()));
        }
        let answer = tokio::time::timeout(wait, rx).await;
        self.browsing.lock().await.remove(&req);
        match answer {
            Ok(Ok(Ok(b))) => Ok(b),
            Ok(Ok(Err(why))) => Err(FetchError::Refused(why)),
            Ok(Err(_)) => Err(FetchError::Offline(device.to_string())),
            Err(_) => Err(FetchError::Timeout),
        }
    }

    /// Ask the device that owns `location` to put `path` on the node;
    /// returns the blob's hash.
    pub async fn fetch(
        &self,
        space: &str,
        location: &str,
        path: &str,
    ) -> Result<String, FetchError> {
        self.ask_agent(space, location, Duration::from_secs(120), |req| {
            ToClient::Fetch {
                req,
                location: location.into(),
                path: path.into(),
            }
        })
        .await
    }

    /// Ask the device that owns `location` to make sidecars of `path` and
    /// put them on the node; returns the file's content key.
    pub async fn make_sidecars(
        &self,
        space: &str,
        location: &str,
        path: &str,
        kinds: Vec<String>,
        force: bool,
    ) -> Result<String, FetchError> {
        self.ask_agent(space, location, Duration::from_secs(180), |req| {
            ToClient::Sidecars {
                req,
                location: location.into(),
                path: path.into(),
                kinds,
                force,
            }
        })
        .await
    }

    /// Send the agent of the device that owns `location` a request, and
    /// wait for its [`ToNode::Fetched`] answer.
    async fn ask_agent(
        &self,
        space: &str,
        location: &str,
        wait: Duration,
        msg: impl FnOnce(u64) -> ToClient,
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
        let (agent_peer, outbox) = self
            .agents
            .lock()
            .await
            .get(&(space.to_string(), device.clone()))
            .map(|(p, o)| (*p, o.clone()))
            .ok_or_else(|| FetchError::Offline(device.clone()))?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(req, (agent_peer, tx));
        if outbox.send(msg(req)).is_err() {
            self.pending.lock().await.remove(&req);
            return Err(FetchError::Offline(device));
        }
        let answer = tokio::time::timeout(wait, rx).await;
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

    /// Save every changed document now (before a backup).
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
        let allowed = match (&named, &msg) {
            (Some(id), ToNode::Update { .. } | ToNode::Agent { .. }) => {
                self.may_write(who, id).await
            }
            (Some(id), _) => self.may(who, id).await,
            (None, _) => true,
        };
        if let Some(id) = named
            && !allowed
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
                        room.dirty += 1;
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
                if let Some(room) = rooms.get_mut(&id)
                    && room.peers.contains_key(&peer)
                {
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
                    .insert((space.clone(), device), (peer, out.clone()));
                // Its device lets this node fetch its files once the
                // registry names the node's endpoint.
                crate::transfer::publish(self, &space).await;
            }
            ToNode::Fetched { req, result } => {
                let mut map = self.pending.lock().await;
                if map.get(&req).is_some_and(|(expected, _)| *expected == peer)
                    && let Some((_, tx)) = map.remove(&req)
                {
                    let _ = tx.send(result);
                }
            }
            ToNode::Browsed { req, result } => {
                let mut map = self.browsing.lock().await;
                if map.get(&req).is_some_and(|(expected, _)| *expected == peer)
                    && let Some((_, tx)) = map.remove(&req)
                {
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
                if !who.all {
                    // Another org's registry is read once per listing, and
                    // without opening its room (a listing runs every few
                    // seconds per open view).
                    let mut registries: HashMap<String, Vec<drive_files::projects::Project>> =
                        HashMap::new();
                    let mut kept = Vec::new();
                    for d in docs {
                        let Some(org) = drive_proto::org_of(&d.id).map(str::to_string) else {
                            continue;
                        };
                        if who.member_of(&org) {
                            kept.push(d);
                            continue;
                        }
                        if !registries.contains_key(&org) {
                            let projects = self
                                .inspect(
                                    &drive_files::space_doc(&org),
                                    drive_files::projects::projects,
                                )
                                .await;
                            registries.insert(org.clone(), projects);
                        }
                        if Self::lent(who, &org, &d.id, &registries[&org]) {
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

/// How often a live connection's sign-in is asked about again: a token
/// that expired or was revoked, or a person who left an org, ends the
/// connection (the client reconnects, signing in afresh).
pub const RECHECK: Duration = Duration::from_secs(5 * 60);

/// One client's connection, from its first message to its last.
pub async fn serve(rooms: Arc<Rooms>, socket: WebSocket, local: bool) {
    let (mut sink, stream) = socket.split();
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
    let incoming = stream
        .take_while(|frame| std::future::ready(frame.is_ok()))
        .filter_map(|frame| {
            std::future::ready(match frame {
                Ok(Message::Binary(bytes)) => match drive_proto::decode::<ToNode>(&bytes) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!(error = %e, "a message did not decode");
                        None
                    }
                },
                _ => None,
            })
        });
    session(rooms, std::pin::pin!(incoming), out, local, RECHECK).await;
    let _ = writer.await;
}

/// A connection's messages, whatever carries them: answered on `out`,
/// its sign-in asked about again every `recheck`.
pub(crate) async fn session(
    rooms: Arc<Rooms>,
    mut incoming: impl futures::Stream<Item = ToNode> + Unpin,
    out: Outbox,
    local: bool,
    recheck: Duration,
) {
    let peer = rooms.next_peer.fetch_add(1, Ordering::Relaxed);
    let mut name = String::from("Guest");
    let mut who: Option<crate::auth::Principal> = None;
    let mut token: Option<String> = None;
    let mut joined = HashSet::new();
    let mut every = tokio::time::interval_at(tokio::time::Instant::now() + recheck, recheck);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let msg = tokio::select! {
            m = incoming.next() => match m {
                Some(m) => m,
                None => break,
            },
            _ = every.tick() => {
                let Some(old) = &who else { continue };
                match rooms.auth.recheck(token.as_deref(), local).await {
                    crate::auth::Recheck::Still(p) => {
                        // Gaining an org is fine; losing one (or everything)
                        // ends the connection, which may hold its documents.
                        let kept = p.all || (!old.all && old.orgs.iter().all(|o| p.member_of(o)));
                        if !kept {
                            tracing::info!(auth.outcome = "narrowed", auth.who = %old.who, peer, "a connection's access narrowed; ending it");
                            let _ = out.send(ToClient::Error {
                                message: "access changed: reconnect".into(),
                            });
                            break;
                        }
                        who = Some(p);
                    }
                    crate::auth::Recheck::Refused => {
                        tracing::info!(auth.outcome = "expired", auth.who = %old.who, peer, "a connection's sign-in is no longer accepted; ending it");
                        let _ = out.send(ToClient::Error {
                            message: "sign in".into(),
                        });
                        break;
                    }
                    crate::auth::Recheck::Unknown => {}
                }
                continue;
            }
        };
        if let ToNode::Hello {
            version,
            name: n,
            token: t,
        } = &msg
        {
            if *version != drive_proto::VERSION {
                let _ = out.send(ToClient::Error {
                    message: "protocol version mismatch".into(),
                });
                break;
            }
            match rooms.auth.principal(t.as_deref(), local).await {
                Some(p) => {
                    who = Some(p);
                    token = t.clone();
                }
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
}

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod evict_tests;
