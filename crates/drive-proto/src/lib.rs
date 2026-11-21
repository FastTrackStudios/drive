//! Drive's sync protocol: the messages a client and a node exchange over
//! one connection. A connection carries many documents (rooms) at once.
//!
//! The exchange for one document:
//! 1. the client sends [`ToNode::Join`] with its Loro version vector;
//! 2. the node answers [`ToClient::Synced`] with what the client lacks and
//!    the node's own version vector, then every peer's presence;
//! 3. the client sends what the node lacks as one [`ToNode::Update`];
//! 4. from then on each side sends updates as they are made, and the node
//!    passes them to every other peer in the room.
//!
//! Presence (cursors, selections, who is here) is never stored: it is the
//! latest JSON each peer set, dropped when the peer leaves.

use serde::{Deserialize, Serialize};

/// The protocol's version; a node refuses a client that speaks another.
pub const VERSION: u32 = 1;

/// A document's id: an app's namespace and a name (`canvas:board-1`).
pub type DocId = String;

/// A document's full id in an org: `home/studio:library`. Ids that already
/// name their org (they hold a `/`) are left as they are.
pub fn qualify(org: &str, id: &str) -> String {
    if id.contains('/') || org.is_empty() {
        id.to_string()
    } else {
        format!("{org}/{id}")
    }
}

/// A document's id within its org (`studio:library` for
/// `home/studio:library`); ids of another org stay whole.
pub fn unqualify<'a>(org: &str, id: &'a str) -> &'a str {
    id.strip_prefix(org)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(id)
}

/// The org a full id belongs to (`home` for `home/studio:library`).
pub fn org_of(id: &str) -> Option<&str> {
    id.split_once('/').map(|(o, _)| o)
}

/// A connection's peer number, given by the node.
pub type Peer = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToNode {
    /// The first message: the protocol version, and who this is.
    Hello { version: u32, name: String },
    /// Open a document, saying what this client already has.
    Join { doc: DocId, version_vector: Vec<u8> },
    /// Loro updates made here.
    Update { doc: DocId, bytes: Vec<u8> },
    /// This peer's presence in a document (JSON), replacing the last.
    Presence { doc: DocId, json: String },
    /// Close a document.
    Leave { doc: DocId },
    /// Ask which documents the node holds whose id starts with `prefix`.
    List { prefix: String },
    /// This connection is a device's agent: the node may ask it for files
    /// in the locations it indexes.
    Agent { space: String, device: String },
    /// The answer to [`ToClient::Fetch`]: the file's blob is on the node
    /// (its BLAKE3 hash), or why not.
    Fetched {
        req: u64,
        result: Result<String, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToClient {
    /// The answer to Hello: this connection's peer number.
    Welcome { peer: Peer },
    /// The answer to Join: the updates the client lacks, and the node's
    /// version vector (so the client can send what the node lacks).
    Synced {
        doc: DocId,
        bytes: Vec<u8>,
        version_vector: Vec<u8>,
    },
    /// Another peer's updates.
    Update { doc: DocId, bytes: Vec<u8> },
    /// A peer's presence; `None` when it left.
    Presence {
        doc: DocId,
        peer: Peer,
        name: String,
        json: Option<String>,
    },
    /// The answer to List.
    Docs { prefix: String, docs: Vec<DocInfo> },
    /// Something went wrong with a request.
    Error { message: String },
    /// To an agent: put this file (from one of your locations) on the node
    /// as a blob, and answer with [`ToNode::Fetched`].
    Fetch {
        req: u64,
        location: String,
        path: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocInfo {
    pub id: DocId,
    /// How many peers have it open now.
    pub peers: u32,
    /// Its size, in bytes of its snapshot.
    pub bytes: u64,
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_stdvec(msg).expect("a message always encodes")
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let m = ToNode::Join {
            doc: "canvas:a".into(),
            version_vector: vec![1, 2, 3],
        };
        assert_eq!(decode::<ToNode>(&encode(&m)).unwrap(), m);
        let m = ToClient::Presence {
            doc: "x".into(),
            peer: 7,
            name: "Mia".into(),
            json: None,
        };
        assert_eq!(decode::<ToClient>(&encode(&m)).unwrap(), m);
        let m = ToNode::Fetched {
            req: 3,
            result: Err("offline".into()),
        };
        assert_eq!(decode::<ToNode>(&encode(&m)).unwrap(), m);
    }
}
