//! Answering one connection: its other end is the client.

use std::collections::HashSet;
use std::sync::Arc;

use drive_proto::{ToClient, ToNode};
use futures::{Stream, StreamExt};

use crate::{Outbox, Rooms};

/// Whether a connection may open (`false`) or change (`true`) a document.
pub type Access = Arc<dyn Fn(&str, bool) -> bool + Send + Sync>;

/// Answer one connection's messages on `out` until `incoming` ends: its
/// handshake ([`ToNode::Hello`], answered [`ToClient::Welcome`]), then
/// joins, updates, presence and listings of the documents `access` lets
/// it have. `via`: the other device the connection is with (its endpoint
/// id), when it is not a window on this one. What only a node does (an
/// agent's files) is not answered here.
pub async fn serve(
    rooms: Arc<Rooms>,
    mut incoming: impl Stream<Item = ToNode> + Unpin,
    out: Outbox,
    via: Option<String>,
    access: Access,
) {
    let peer = rooms.new_peer(via.as_deref());
    let mut name: Option<String> = None;
    let mut joined = HashSet::new();
    while let Some(msg) = incoming.next().await {
        if let ToNode::Hello {
            version, name: n, ..
        } = &msg
        {
            if *version != drive_proto::VERSION {
                let _ = out.send(ToClient::Error {
                    message: "protocol version mismatch".into(),
                });
                break;
            }
            if name.is_some() {
                let _ = out.send(ToClient::Error {
                    message: "already signed in: reconnect".into(),
                });
                break;
            }
            name = Some(n.clone());
            let _ = out.send(ToClient::Welcome { peer });
            continue;
        }
        let Some(name) = &name else { continue };
        let refuse = |id: &str| {
            tracing::warn!(sync.outcome = "denied", doc.id = %id, "a peer asked for a document it may not have");
            let _ = out.send(ToClient::Error {
                message: format!("not allowed: {id}"),
            });
        };
        match msg {
            ToNode::Join {
                doc,
                version_vector,
            } => {
                if !access(&doc, false) {
                    refuse(&doc);
                    continue;
                }
                rooms.join(peer, &out, &doc, &version_vector).await;
                joined.insert(doc);
            }
            ToNode::Update { doc, bytes } => {
                if !access(&doc, true) {
                    refuse(&doc);
                    continue;
                }
                rooms.update(peer, Some(&out), &doc, &bytes).await;
            }
            ToNode::Presence { doc, json } => {
                if access(&doc, false) {
                    rooms.presence(peer, peer, name, &doc, Some(json)).await;
                }
            }
            ToNode::Leave { doc } => {
                rooms.leave(peer, name, &doc).await;
                joined.remove(&doc);
            }
            ToNode::List { prefix } => {
                let docs = rooms
                    .list(&prefix)
                    .await
                    .into_iter()
                    .filter(|d| access(&d.id, false))
                    .collect();
                let _ = out.send(ToClient::Docs { prefix, docs });
            }
            ToNode::Hello { .. }
            | ToNode::Agent { .. }
            | ToNode::Fetched { .. }
            | ToNode::Browsed { .. } => {}
        }
    }
    let name = name.unwrap_or_default();
    for id in joined {
        rooms.leave(peer, &name, &id).await;
    }
    rooms.forget_peer(peer);
}
