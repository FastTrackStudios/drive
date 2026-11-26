//! Rooms: each open document, the peers that have it open, and their
//! presence (`drive-sync`'s, which an app runs too), and what only a node
//! does on top: who may open what (the issuer's orgs), and the agents
//! connected to it. The node is one more peer of every document: it
//! merges what it is sent, keeps it, and passes it on; it never decides
//! who wins.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use drive_proto::{DocId, Peer, ToClient, ToNode};
use futures::{SinkExt, StreamExt};
use loro::LoroDoc;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::store::Store;

pub use drive_sync::IDLE;

type Outbox = mpsc::UnboundedSender<ToClient>;
type PendingFetch = (Peer, oneshot::Sender<Result<String, String>>);
type PendingBrowse = (Peer, oneshot::Sender<Result<drive_proto::Browsed, String>>);

/// The node's database as `drive-sync`'s store of snapshots.
struct Docs(Arc<Store>);

impl drive_sync::Store for Docs {
    fn load<'a>(
        &'a self,
        id: &'a str,
    ) -> drive_sync::BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { self.0.load(id).await.map_err(|e| e.to_string()) })
    }
    fn save<'a>(
        &'a self,
        id: &'a str,
        snapshot: Vec<u8>,
    ) -> drive_sync::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.0.save(id, snapshot).await.map_err(|e| e.to_string()) })
    }
    fn list(&self) -> drive_sync::BoxFuture<'_, Result<Vec<(DocId, u64)>, String>> {
        Box::pin(async move {
            self.0
                .list()
                .await
                .map(|l| l.as_ref().clone())
                .map_err(|e| e.to_string())
        })
    }
}

pub struct Rooms {
    /// The documents, their peers and presence.
    pub sync: drive_sync::Rooms,
    pub store: Arc<Store>,
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
        let store = Arc::new(store);
        Rooms {
            auth,
            sync: drive_sync::Rooms::new(Arc::new(Docs(store.clone()))),
            store,
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
        self.sync.edit(id, author, f).await
    }

    /// Read a document as the node holds it (loading it if needed).
    pub async fn read<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        self.sync.read(id, f).await
    }

    /// Read a document as [`Rooms::read`] does, except that one the store
    /// does not have is read empty and opens no room: for what anyone may
    /// ask for without signing in (a site, a share link), so made-up ids
    /// cannot fill the node's memory with rooms.
    pub async fn peek<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        self.sync.peek(id, f).await
    }

    /// Read a document without opening a room for it: its room if it has
    /// one open, else its stored snapshot (empty when there is none).
    async fn inspect<T>(&self, id: &str, f: impl FnOnce(&LoroDoc) -> T) -> T {
        self.sync.inspect(id, f).await
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

    /// Save every changed document, every two seconds.
    pub async fn flush_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.sync.flush_now().await;
            self.sync.evict_idle(IDLE).await;
        }
    }

    /// Let go of every room nobody has open, with nothing left to save,
    /// unused for `idle`: it is loaded again from the store when asked for.
    pub async fn evict_idle(&self, idle: Duration) {
        self.sync.evict_idle(idle).await;
    }

    /// Save every changed document now (before a backup).
    pub async fn flush_now(&self) {
        self.sync.flush_now().await;
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
                self.sync.join(peer, out, &id, &version_vector).await;
                joined.insert(id);
            }
            ToNode::Update { doc: id, bytes } => {
                self.sync.update(peer, Some(out), &id, &bytes).await;
            }
            ToNode::Presence { doc: id, json } => {
                self.sync.presence(peer, peer, name, &id, Some(json)).await;
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
                let mut docs = self.sync.list(&prefix).await;
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
        self.sync.leave(peer, name, id).await;
    }
}

/// How often a live connection's sign-in is asked about again: a token
/// that expired or was revoked, or a person who left an org, ends the
/// connection (the client reconnects, signing in afresh).
pub const RECHECK: Duration = Duration::from_secs(5 * 60);

/// When a session signed in with `token` must end: the moment its token
/// expires (`token_expiry`), on the clock of `at`, which is `now`. `None`
/// when the token does not say (the periodic re-check still ends it).
fn session_end(
    token: Option<&str>,
    now: std::time::SystemTime,
    at: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let exp = crate::auth::token_expiry(token?)?;
    Some(at + exp.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Wait until `end`, or forever when there is none.
async fn until(end: Option<tokio::time::Instant>) {
    match end {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

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
/// its sign-in asked about again every `recheck`, and ended when its
/// token expires.
pub(crate) async fn session(
    rooms: Arc<Rooms>,
    mut incoming: impl futures::Stream<Item = ToNode> + Unpin,
    out: Outbox,
    local: bool,
    recheck: Duration,
) {
    let peer = rooms.sync.new_peer(None);
    let mut name = String::from("Guest");
    let mut who: Option<crate::auth::Principal> = None;
    let mut token: Option<String> = None;
    let mut ends: Option<tokio::time::Instant> = None;
    let mut joined = HashSet::new();
    let mut every = tokio::time::interval_at(tokio::time::Instant::now() + recheck, recheck);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let msg = tokio::select! {
            m = incoming.next() => match m {
                Some(m) => m,
                None => break,
            },
            _ = until(ends) => {
                let signed = who.as_ref().map(|p| p.who.as_str()).unwrap_or_default();
                tracing::info!(auth.outcome = "expired", auth.who = %signed, peer, "a connection's token expired; ending it");
                let _ = out.send(ToClient::Error {
                    message: "sign in".into(),
                });
                break;
            }
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
            // One sign-in per connection: a second would change who this is
            // while the documents the first could open stay joined. Clients
            // reconnect with a new token instead (drive#56).
            if who.is_some() {
                let _ = out.send(ToClient::Error {
                    message: "already signed in: reconnect".into(),
                });
                break;
            }
            match rooms.auth.principal(t.as_deref(), local).await {
                Some(p) => {
                    who = Some(p);
                    token = t.clone();
                    ends = session_end(
                        t.as_deref(),
                        std::time::SystemTime::now(),
                        tokio::time::Instant::now(),
                    );
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
    rooms.sync.forget_peer(peer);
}

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod evict_tests;
