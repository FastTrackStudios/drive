//! Linking to another peer: these rooms are its client. Every document a
//! window here has open is joined there; what each side lacks is
//! exchanged by version vector; from then on updates and presence flow
//! both ways until the connection ends.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use drive_proto::{DocId, Peer, ToClient, ToNode};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::Rooms;
use crate::rooms::Interest;

/// Who this device says it is to the peer.
#[derive(Clone, Debug, Default)]
pub struct Hello {
    /// The name others see beside its presence.
    pub name: String,
    /// An access token from the issuer, for a peer that asks for one (a
    /// node with an issuer).
    pub token: Option<String>,
}

/// Link `rooms` to the peer at the other end (`via`: its endpoint id, or
/// any name that is the same for every connection to it), until
/// `incoming` ends or `up` closes. `incoming` is what the peer sends,
/// `up` what goes to it.
pub async fn link(
    rooms: Arc<Rooms>,
    via: String,
    hello: Hello,
    mut incoming: impl Stream<Item = ToClient> + Unpin,
    up: mpsc::UnboundedSender<ToNode>,
) {
    let peer = rooms.new_link(&via);
    let (tx, mut rx) = mpsc::unbounded_channel::<ToClient>();
    let mut interest = rooms.watch();
    let mut online = false;
    let mut joined: HashSet<DocId> = HashSet::new();
    // The peer's peers, as numbers of this side's own.
    let mut theirs: HashMap<Peer, (Peer, String)> = HashMap::new();
    let send = |m: ToNode| up.send(m).is_ok();

    async fn join(
        rooms: &Rooms,
        peer: Peer,
        tx: &mpsc::UnboundedSender<ToClient>,
        up: &mpsc::UnboundedSender<ToNode>,
        joined: &mut HashSet<DocId>,
        id: DocId,
    ) {
        let version_vector = rooms.attach(peer, tx, &id).await;
        let _ = up.send(ToNode::Join {
            doc: id.clone(),
            version_vector,
        });
        joined.insert(id);
    }

    if !send(ToNode::Hello {
        version: drive_proto::VERSION,
        name: hello.name,
        token: hello.token,
    }) {
        rooms.forget_peer(peer);
        return;
    }
    loop {
        tokio::select! {
            m = incoming.next() => {
                let Some(m) = m else { break };
                match m {
                    ToClient::Welcome { .. } => {
                        online = true;
                        for id in rooms.open_here().await {
                            if !joined.contains(&id) {
                                join(&rooms, peer, &tx, &up, &mut joined, id).await;
                            }
                        }
                    }
                    ToClient::Synced { doc, bytes, version_vector } => {
                        if !joined.contains(&doc) {
                            continue;
                        }
                        if !bytes.is_empty() {
                            rooms.update(peer, None, &doc, &bytes).await;
                        }
                        if let Some(ours) = rooms.since(&doc, &version_vector).await {
                            send(ToNode::Update { doc: doc.clone(), bytes: ours });
                        }
                        // One presence per connection: this device's.
                        if let Some(json) = rooms.presence_here(&doc).await.pop() {
                            send(ToNode::Presence { doc, json });
                        }
                    }
                    ToClient::Update { doc, bytes } => {
                        if joined.contains(&doc) {
                            rooms.update(peer, None, &doc, &bytes).await;
                        }
                    }
                    ToClient::Presence { doc, peer: them, name, json } => {
                        let (who, _) = theirs
                            .entry(them)
                            .or_insert_with(|| (rooms.new_peer(Some(&via)), name.clone()))
                            .clone();
                        rooms.presence(peer, who, &name, &doc, json).await;
                    }
                    ToClient::Error { message } => {
                        tracing::warn!(sync.via = %via, error = %message, "a linked peer refused a message");
                    }
                    ToClient::Docs { .. }
                    | ToClient::Fetch { .. }
                    | ToClient::Sidecars { .. }
                    | ToClient::Browse { .. } => {}
                }
            }
            Some(m) = rx.recv() => match m {
                ToClient::Update { doc, bytes } => {
                    if !send(ToNode::Update { doc, bytes }) {
                        break;
                    }
                }
                ToClient::Presence { doc, peer: who, json: Some(json), .. } if rooms.is_local(who) => {
                    send(ToNode::Presence { doc, json });
                }
                _ => {}
            },
            Some(i) = interest.recv() => match i {
                Interest::Opened(id) => {
                    if online && !joined.contains(&id) {
                        join(&rooms, peer, &tx, &up, &mut joined, id).await;
                    }
                }
                Interest::Closed(id) => {
                    if joined.remove(&id) {
                        send(ToNode::Leave { doc: id.clone() });
                        for (who, name) in theirs.values() {
                            rooms.presence(peer, *who, name, &id, None).await;
                        }
                        rooms.leave(peer, "", &id).await;
                    }
                }
            },
            else => break,
        }
    }
    for id in &joined {
        for (who, name) in theirs.values() {
            rooms.presence(peer, *who, name, id, None).await;
        }
        rooms.leave(peer, "", id).await;
    }
    for (who, _) in theirs.values() {
        rooms.forget_peer(*who);
    }
    rooms.forget_peer(peer);
}
