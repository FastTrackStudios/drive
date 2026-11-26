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
//!
//! Listing documents: [`ToNode::ListSubscribe`] answers once with
//! [`ToClient::Docs`], then sends [`ToClient::Listed`] whenever a document
//! under the prefix appears, goes, or changes what [`DocInfo`] says of it
//! (drive#159). [`ToNode::List`] answers once and is kept for clients of
//! version 2, which asked again every few seconds.

use serde::{Deserialize, Serialize};

/// The protocol's version: what this build speaks.
///
/// 3 added [`ToNode::ListSubscribe`], [`ToNode::ListUnsubscribe`] and
/// [`ToClient::Listed`] (drive#159), at the ends of their enums, so every
/// message of version 2 is encoded as it was.
pub const VERSION: u32 = 3;

/// The oldest version still answered: a client of version 2 lists by
/// asking again ([`ToNode::List`]) and is never sent what it does not know.
pub const OLDEST: u32 = 2;

/// Whether a peer that says it speaks `version` is answered.
#[must_use]
pub fn speaks(version: u32) -> bool {
    (OLDEST..=VERSION).contains(&version)
}

/// Whether a document id is under a listing's prefix: `*/rest` matches
/// `rest` in every org (`*/studio:` is `studio:` everywhere), anything
/// else is a plain prefix of the full id.
#[must_use]
pub fn under(prefix: &str, id: &str) -> bool {
    match prefix.strip_prefix("*/") {
        Some(rest) => id.split_once('/').is_some_and(|(_, r)| r.starts_with(rest)),
        None => id.starts_with(prefix),
    }
}

/// The biggest websocket message (and frame) a node or a native client
/// takes. A document's join is answered with one message holding all of
/// it the joiner lacks, and websockets refuse frames over 16 MiB by
/// default: a device whose copy fell 52 MiB behind could never catch up,
/// and reconnected about once a second (drive#172). Both ends use this.
pub const MAX_MESSAGE: usize = 256 << 20;

/// A document's id: an app's namespace and a name (`canvas:board-1`).
pub type DocId = String;

/// A document's full id in an org: `home/studio:library`. Ids that already
/// name their org (they hold a `/`) are left as they are.
pub fn qualify(org: &str, id: &str) -> String {
    if org_of(id).is_some() || org.is_empty() {
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
    // An org slug has no ':'; an app's id always does before any '/'
    // (`studio:song:a/b` is not in an org `studio:song:a`).
    id.split_once('/')
        .map(|(o, _)| o)
        .filter(|o| !o.is_empty() && !o.contains(':'))
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn ids_and_their_orgs() {
        assert_eq!(qualify("cody", "studio:library"), "cody/studio:library");
        assert_eq!(
            qualify("cody", "other/studio:library"),
            "other/studio:library"
        );
        // A path inside an app id is not an org.
        assert_eq!(
            qualify("cody", "comments:loc:l1:Song A/Mixes/v2.wav"),
            "cody/comments:loc:l1:Song A/Mixes/v2.wav"
        );
        assert_eq!(org_of("cody/comments:loc:l1:a/b"), Some("cody"));
        assert_eq!(org_of("comments:loc:l1:a/b"), None);
        assert_eq!(unqualify("cody", "cody/x:y"), "x:y");
    }
}

/// A connection's peer number, given by the node.
pub type Peer = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToNode {
    /// The first message: the protocol version, and who this is.
    /// `token`: an access token from the issuer, when the node asks for
    /// one (a browser signed in; this computer's own agent needs none).
    Hello {
        version: u32,
        name: String,
        token: Option<String>,
    },
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
    /// The answer to [`ToClient::Browse`].
    Browsed {
        req: u64,
        result: Result<Browsed, String>,
    },
    /// Version 3: list the documents under `prefix` now
    /// ([`ToClient::Docs`]), then say what changes under it
    /// ([`ToClient::Listed`]) until [`ToNode::ListUnsubscribe`] or the
    /// connection ends. Subscribing to a prefix twice is once.
    ListSubscribe { prefix: String },
    /// Version 3: stop hearing about `prefix`.
    ListUnsubscribe { prefix: String },
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
    /// To an agent: make these sidecars (`thumb-256`, `peaks`, …: see
    /// `drive-sidecars`) of a file in one of your locations, put them on
    /// the node, and answer with [`ToNode::Fetched`] (the file's content
    /// key, or why not: `no generator: …` when nothing here reads it).
    /// `force`: make them again even if the node has them.
    Sidecars {
        req: u64,
        location: String,
        path: String,
        kinds: Vec<String>,
        force: bool,
    },
    /// To an agent: a look at this device's disks, live and read-only
    /// (docs/browse.md); answer with [`ToNode::Browsed`]. An agent that
    /// does not know this message skips it (the node times out).
    Browse { req: u64, ask: Browse },
    /// Version 3, to a [`ToNode::ListSubscribe`]r: what changed under
    /// `prefix` since the last listing. `docs`: documents that appeared or
    /// whose entry changed (replace them by id); `removed`: ids that went.
    /// A rename is its old id removed and its new one in `docs`.
    Listed {
        prefix: String,
        docs: Vec<DocInfo>,
        removed: Vec<DocId>,
    },
}

/// What the node asks of a device's disks. Every path is relative to a
/// `root`: the mount point of one of the device's volumes, the system
/// root `/`, or the home folder, exactly as [`Browsed::Roots`] names it.
/// The agent refuses anything that resolves (through `..` or symlinks)
/// outside that root, and never writes under it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Browse {
    /// The roots that can be browsed now.
    Roots,
    /// A folder's entries, folders first then by name, a page at a time
    /// (`limit` at most [`LIST_MAX`]); `hidden`: dot files and system
    /// folders too.
    List {
        root: String,
        path: String,
        offset: u32,
        limit: u32,
        hidden: bool,
    },
    /// Up to `len` bytes (at most [`READ_MAX`]) of a file from `start`;
    /// `len` 0: only its size and time.
    Read {
        root: String,
        path: String,
        start: u64,
        len: u32,
    },
    /// The file's content key (`drive-sidecars`' sampled key).
    Key { root: String, path: String },
    /// Make these sidecars of the file and put them on the node; the
    /// answer is its content key ([`Browsed::Key`]).
    Sidecars {
        root: String,
        path: String,
        kinds: Vec<String>,
        force: bool,
    },
    /// Make this folder a location (indexed in place, as `drive-agent
    /// location add` does); the answer is the location's id.
    Index {
        root: String,
        path: String,
        name: String,
    },
    /// Serve this file over iroh (docs/transfer.md): hash it if needed
    /// (once per size and time) and answer with [`Browsed::Provided`]. Sent
    /// only to devices whose registry entry has an iroh endpoint; an agent
    /// from before never sees it.
    Provide { root: String, path: String },
    /// The same for a file of one of the device's locations (`path`
    /// relative to the location's folder).
    ProvideLocation { location: String, path: String },
}

/// The most entries one [`Browse::List`] answers with.
pub const LIST_MAX: u32 = 2000;
/// The most bytes one [`Browse::Read`] answers with.
pub const READ_MAX: u32 = 4 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Browsed {
    Roots(Vec<Root>),
    Listing(Listing),
    Chunk(Chunk),
    /// A content key, or a new location's id.
    Key(String),
    Provided(Provided),
}

/// A file a device serves over iroh: fetch it from `endpoint` by `hash`.
/// A member of an org, as the issuer knows them (`GET /orgs/{org}/members`
/// on the node).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OrgMember {
    /// Their account at the issuer.
    pub account: String,
    #[serde(default)]
    pub email: String,
    /// Their display name; empty when they never gave one.
    #[serde(default)]
    pub name: String,
    /// A picture's URL, if any.
    #[serde(default)]
    pub image: String,
    /// `owner`, `admin` or `member`.
    #[serde(default)]
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provided {
    /// Its full BLAKE3 hash (hex): also its blob name on the node.
    pub hash: String,
    pub size: u64,
    /// Modified, in ms since 1970.
    pub mtime: i64,
    /// The device's iroh endpoint address now (JSON: id, relay, direct
    /// addresses).
    pub endpoint: String,
}

/// Something a device lets be browsed: a volume, `/`, or the home folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Root {
    /// What [`Browse`] requests name it by (its path on the device).
    pub mount: String,
    /// What to call it (`dev-drive`, `Home`, `System`).
    pub name: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
    pub removable: bool,
    pub home: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub dir: bool,
    /// Bytes (0 for a folder).
    pub size: u64,
    /// Modified, in ms since 1970.
    pub mtime: i64,
    /// A symbolic link (shown as what it points at, when that is inside
    /// the root).
    pub link: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub entries: Vec<DirEntry>,
    /// How many entries the folder has (shown ones), of which `entries`
    /// is the page from `offset`.
    pub total: u32,
    pub offset: u32,
    /// How many were left out as hidden.
    pub hidden: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// The whole file's size and time.
    pub size: u64,
    pub mtime: i64,
    pub start: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocInfo {
    pub id: DocId,
    /// How many peers have it open now.
    pub peers: u32,
    /// Its size, in bytes of its snapshot.
    pub bytes: u64,
}

/// A listing with a [`ToClient::Listed`] applied: `removed` ids go, each
/// of `docs` replaces the entry with its id (or joins), and it stays in
/// id order, as the node lists.
pub fn apply_listed(list: &mut Vec<DocInfo>, docs: Vec<DocInfo>, removed: &[DocId]) {
    list.retain(|d| !removed.contains(&d.id));
    for d in docs {
        match list.binary_search_by(|x| x.id.cmp(&d.id)) {
            Ok(i) => list[i] = d,
            Err(i) => list.insert(i, d),
        }
    }
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
        let m = ToClient::Sidecars {
            req: 4,
            location: "l1".into(),
            path: "Songs/a.wav".into(),
            kinds: vec!["peaks".into()],
            force: false,
        };
        assert_eq!(decode::<ToClient>(&encode(&m)).unwrap(), m);
        let m = ToClient::Browse {
            req: 5,
            ask: Browse::List {
                root: "/Volumes/dev-drive".into(),
                path: "Songs".into(),
                offset: 0,
                limit: 200,
                hidden: false,
            },
        };
        assert_eq!(decode::<ToClient>(&encode(&m)).unwrap(), m);
        let m = ToNode::Browsed {
            req: 5,
            result: Ok(Browsed::Chunk(Chunk {
                size: 10,
                mtime: 1,
                start: 2,
                bytes: vec![1, 2, 3],
            })),
        };
        assert_eq!(decode::<ToNode>(&encode(&m)).unwrap(), m);
    }

    /// An agent from before browsing decodes every message it knew, and
    /// fails (and so skips) only the new ones: no version bump needed.
    #[test]
    fn new_messages_leave_old_ones_alone() {
        // The old ToClient, as it was: the same variants in the same order.
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        enum OldToClient {
            Welcome {
                peer: Peer,
            },
            Synced {
                doc: DocId,
                bytes: Vec<u8>,
                version_vector: Vec<u8>,
            },
            Update {
                doc: DocId,
                bytes: Vec<u8>,
            },
            Presence {
                doc: DocId,
                peer: Peer,
                name: String,
                json: Option<String>,
            },
            Docs {
                prefix: String,
                docs: Vec<DocInfo>,
            },
            Error {
                message: String,
            },
            Fetch {
                req: u64,
                location: String,
                path: String,
            },
            Sidecars {
                req: u64,
                location: String,
                path: String,
                kinds: Vec<String>,
                force: bool,
            },
        }
        let fetch = encode(&ToClient::Fetch {
            req: 1,
            location: "l".into(),
            path: "p".into(),
        });
        assert!(decode::<OldToClient>(&fetch).is_ok());
        let browse = encode(&ToClient::Browse {
            req: 2,
            ask: Browse::Roots,
        });
        assert!(decode::<OldToClient>(&browse).is_err());
    }

    /// Provide is a new Browse at the end: the browse messages an agent
    /// from before knew decode as they did.
    #[test]
    fn provide_leaves_browsing_alone() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[allow(dead_code)]
        enum OldBrowse {
            Roots,
            List {
                root: String,
                path: String,
                offset: u32,
                limit: u32,
                hidden: bool,
            },
            Read {
                root: String,
                path: String,
                start: u64,
                len: u32,
            },
            Key {
                root: String,
                path: String,
            },
            Sidecars {
                root: String,
                path: String,
                kinds: Vec<String>,
                force: bool,
            },
            Index {
                root: String,
                path: String,
                name: String,
            },
        }
        let list = encode(&Browse::Index {
            root: "/".into(),
            path: "a".into(),
            name: "A".into(),
        });
        assert!(decode::<OldBrowse>(&list).is_ok());
        let provide = encode(&Browse::Provide {
            root: "/".into(),
            path: "a".into(),
        });
        assert!(decode::<OldBrowse>(&provide).is_err());
        let m = ToNode::Browsed {
            req: 9,
            result: Ok(Browsed::Provided(Provided {
                hash: "ab".repeat(32),
                size: 3,
                mtime: 4,
                endpoint: "{}".into(),
            })),
        };
        assert_eq!(decode::<ToNode>(&encode(&m)).unwrap(), m);
    }

    fn info(id: &str, peers: u32) -> DocInfo {
        DocInfo {
            id: id.into(),
            peers,
            bytes: 0,
        }
    }

    /// Version 3's messages sit at the ends of their enums: what a
    /// version-2 peer sends and is sent decodes as it did.
    #[test]
    fn listing_subscriptions_leave_version_2_alone() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[allow(dead_code)]
        enum OldToNode {
            Hello {
                version: u32,
                name: String,
                token: Option<String>,
            },
            Join {
                doc: DocId,
                version_vector: Vec<u8>,
            },
            Update {
                doc: DocId,
                bytes: Vec<u8>,
            },
            Presence {
                doc: DocId,
                json: String,
            },
            Leave {
                doc: DocId,
            },
            List {
                prefix: String,
            },
        }
        let list = encode(&ToNode::List {
            prefix: "home/".into(),
        });
        assert_eq!(
            decode::<OldToNode>(&list).unwrap(),
            OldToNode::List {
                prefix: "home/".into()
            }
        );
        for m in [
            ToNode::ListSubscribe {
                prefix: "home/".into(),
            },
            ToNode::ListUnsubscribe {
                prefix: "home/".into(),
            },
        ] {
            assert_eq!(decode::<ToNode>(&encode(&m)).unwrap(), m);
        }
        let listed = ToClient::Listed {
            prefix: "home/".into(),
            docs: vec![info("home/a:b", 1)],
            removed: vec!["home/a:c".into()],
        };
        assert_eq!(decode::<ToClient>(&encode(&listed)).unwrap(), listed);
        assert!(speaks(2) && speaks(VERSION) && !speaks(1) && !speaks(VERSION + 1));
    }

    #[test]
    fn a_prefix_is_in_one_org_or_every_org() {
        assert!(under("home/studio:", "home/studio:song:a"));
        assert!(!under("home/studio:", "work/studio:song:a"));
        assert!(under("*/studio:", "work/studio:song:a"));
        assert!(!under("*/studio:", "work/canvas:a"));
        assert!(!under("*/studio:", "studio:no-org"));
    }

    #[test]
    fn a_delta_replaces_adds_and_removes_in_id_order() {
        let mut list = vec![info("a", 0), info("c", 0)];
        apply_listed(&mut list, vec![info("b", 1), info("c", 2)], &["a".into()]);
        assert_eq!(list, vec![info("b", 1), info("c", 2)]);
        // A rename: the old id goes, the new one comes.
        apply_listed(&mut list, vec![info("a2", 0)], &["b".into()]);
        assert_eq!(list, vec![info("a2", 0), info("c", 2)]);
    }
}
