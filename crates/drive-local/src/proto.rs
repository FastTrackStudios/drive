//! The wire: one JSON object per line, both ways.
//!
//! The app sends requests, each with an `id` it chooses (unique on its
//! connection) and an `op`:
//!
//! ```text
//! {"id":1,"op":"hello","app":"studio","token":null}
//! {"id":2,"op":"open","target":"drive:Songs/Take 3.wav"}
//! {"id":3,"op":"resolve_many","targets":["Samples:Kits/kick.wav","content:…"]}
//! {"id":4,"op":"warm","project":"Amazing Grace","kinds":["audio"],"priority":"high"}
//! {"id":5,"op":"cancel","job":4}
//! {"id":6,"op":"watch","target":"drive:Songs/Take 3.wav"}
//! {"id":7,"op":"unwatch","watch":6}
//! {"id":8,"op":"release","target":"drive:Songs/Take 3.wav"}
//! {"id":9,"op":"pins"}
//! {"id":10,"op":"status"}
//! {"id":11,"op":"index","index":"list","location":"loc-1","dir":"Kits"}
//! ```
//!
//! The agent answers each request once, `{"id":N,"ok":…}` or
//! `{"id":N,"err":"…"}`, and sends events beside the answers:
//! `{"event":"progress",…}` / `{"event":"done",…}` for a warm job (whose
//! id is the warm request's id) and `{"event":"changed",…}` for a watch
//! (whose id is the watch request's id). A job's answer always comes
//! before its first event.

use serde::{Deserialize, Serialize};

/// A request: the app's id for it and what it asks.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// First on a connection: who the app is (shown in `drive pins`), and
    /// on a loopback port the token from the agent's `app.port` file.
    Hello {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// One file as a local path.
    Open {
        target: String,
        #[serde(default)]
        opts: OpenOptions,
    },
    /// Many files at once (a session's media list): one answer, in order.
    ResolveMany {
        targets: Vec<String>,
        #[serde(default)]
        opts: OpenOptions,
    },
    /// Bring files here ahead of use; progress comes as events.
    Warm(Warm),
    /// Stop a warm job (what came stays; a file half-fetched resumes later).
    Cancel {
        job: u64,
    },
    /// Notice when a file's content changes.
    Watch {
        target: String,
    },
    Unwatch {
        watch: u64,
    },
    /// Let go of one target's lease (`None`: every lease of this app).
    Release {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// What every app has pinned here, and why.
    Pins,
    /// The cache: its use, budget and pins.
    Status,
    // ── The Drive tree, for a system file provider (macOS File Provider:
    //    docs/fileprovider.md). Ids are tree ids; the top is `root`. ──
    /// One item. (The item's id is `item`: a request's own `id` is its
    /// number on the connection, and both are keys of one object.)
    FpItem { item: String },
    /// A folder's items, a page at a time: `after` is the last page's
    /// `next` (none: from the start).
    FpList {
        parent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// The tree's version now ([`FpChanges`] since it later).
    FpAnchor,
    /// What changed since `anchor`.
    FpChanges { anchor: String },
    /// A file's bytes, copied into `into` (a folder the asking process
    /// can read); the answer is the copy's path and the item.
    FpFetch { item: String, into: std::path::PathBuf },
    /// A new file or folder; a file's bytes are read from `from`.
    FpCreate {
        parent: String,
        name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        dir: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<std::path::PathBuf>,
    },
    /// Rename, move and/or replace a file's bytes (from `from`).
    FpModify {
        item: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<std::path::PathBuf>,
    },
    /// Into the Drive's Trash (nothing is ever deleted).
    FpDelete { item: String },
    /// The agent's location indexes (drive#157): a `drive` command run
    /// beside the agent asks it, so that only the running agent opens its
    /// index database.
    Index(IndexOp),
}

/// A question to (or a change of) the running agent's location indexes.
/// Answers are the agent's own types as JSON (entries as
/// `{path, dir, size, mtime, sampled}`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "index", rename_all = "snake_case")]
pub enum IndexOp {
    /// A location's totals (`null`: no index).
    Meta { location: String },
    /// One entry (`null`: none).
    Get { location: String, path: String },
    /// Entries whose name holds `words`, in every location.
    Find { words: String, limit: usize },
    /// A folder's entries (`""`: the root), folders first.
    List { location: String, dir: String },
    /// Up to `limit` entries under `dir` (`""`: all) in path order, after
    /// `after`.
    Page {
        location: String,
        dir: String,
        after: String,
        limit: usize,
    },
    /// Which locations hold each content.
    LocationsOf { sampled: Vec<String> },
    /// Contents held more than once, everywhere.
    Duplicates { min_size: u64 },
    /// Contents held more than once under a folder of a location.
    DuplicatesUnder { location: String, dir: String },
    /// Where a content is.
    Copies {
        sampled: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },
    /// Forget a location's index.
    Remove { location: String },
    /// Walk one of the agent's locations and publish its index.
    Reindex { location: String },
    /// Download the indexes newer on the node than the agent's copies.
    Sync,
}

/// One item of the Drive tree, as a file provider shows it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FpItem {
    pub id: String,
    /// `root` for what is at the top.
    pub parent: String,
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Changed, ms since the epoch.
    pub mtime: i64,
    /// Changes whenever the item's bytes do (its content hash).
    pub content: String,
    /// Changes whenever its name or place does.
    pub meta: String,
    /// Its bytes are on this device now (no download to open it).
    pub here: bool,
}

/// A page of a folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FpPage {
    pub items: Vec<FpItem>,
    /// Pass as `after` for the next page; none: that was the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// What changed since an anchor.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FpChanges {
    pub updated: Vec<FpItem>,
    pub deleted: Vec<String>,
    /// The anchor these changes bring the asker to.
    pub anchor: String,
    /// The anchor could not be gone back to: enumerate everything again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expired: bool,
}

/// A fetched file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FpFetched {
    pub path: std::path::PathBuf,
    pub item: FpItem,
}

/// How to open.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenOptions {
    /// Hold the file pinned while this connection lives (default true).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub lease: bool,
    /// Also compute the full BLAKE3 of a file opened in place (one read of
    /// it, once per size and time). Without it a file of this device's own
    /// is opened without reading a byte, and its `hash` may be absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hash: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions::new()
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            lease: true,
            hash: false,
        }
    }
}

fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

/// How a path came to be here.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// The file itself, on this computer: one of its locations or disks.
    /// Read it; never write to it (it is the owner's file, not a copy).
    Local,
    /// A copy already in this device's cache.
    Cached,
    /// Fetched just now into this device's cache.
    Fetched,
}

/// A Drive file as a real local path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalFile {
    pub path: std::path::PathBuf,
    pub size: u64,
    /// The content's full BLAKE3 (hex). Always set for cached and fetched
    /// files; for a local file only when known or asked for
    /// ([`OpenOptions::hash`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub how: How,
}

/// One entry of a `resolve_many` answer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resolved {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<LocalFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What to warm. Any mix: the files of `targets` (a folder target warms
/// what is under it), of `folder`, and of `project`'s parts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Warm {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// `drive:FOLDER` or `LOCATION:FOLDER`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// A project by name or id (`ORG/NAME`: another org's): every file of
    /// its parts in this device's org, location folders and Drive-tree
    /// folders alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Only files of these kinds (`audio`, `video`, `image`, … as Drive's
    /// `kind:` search knows them). Empty: every file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub priority: Priority,
    /// Hold what is warmed pinned while this connection lives (default true).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub lease: bool,
}

impl Default for Warm {
    fn default() -> Self {
        Warm {
            targets: Vec::new(),
            folder: None,
            project: None,
            kinds: Vec::new(),
            priority: Priority::Normal,
            lease: true,
        }
    }
}

/// How hard a warm job pulls: how many files it fetches at once.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// One file at a time.
    Low,
    /// Three at a time.
    #[default]
    Normal,
    /// Eight at a time.
    High,
}

impl Priority {
    pub fn parallel(self) -> usize {
        match self {
            Priority::Low => 1,
            Priority::Normal => 3,
            Priority::High => 8,
        }
    }
}

/// A warm job's answer: its id (the request's) and what it will do.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Started {
    pub job: u64,
    pub files: u64,
    pub bytes: u64,
    /// Files already here (local or cached): nothing to fetch.
    pub here: u64,
}

/// A file's state in a warm job.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Queued,
    Fetching,
    /// Here: local, cached or fetched (see `how`).
    Ready,
    Failed,
    Cancelled,
}

/// One file's news in a progress event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileProgress {
    pub target: String,
    pub state: FileState,
    pub size: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<LocalFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where a warm job is.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Progress {
    pub job: u64,
    pub files: u64,
    pub files_done: u64,
    pub files_failed: u64,
    pub bytes: u64,
    pub bytes_done: u64,
    /// Bytes per second over the job so far.
    pub rate: u64,
    /// Time left at that rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_ms: Option<u64>,
    /// The file this event is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileProgress>,
}

/// Why something is pinned here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pin {
    pub path: std::path::PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub size: u64,
    pub how: How,
    /// The apps holding it, and the names they gave it.
    pub holders: Vec<Holder>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Holder {
    pub app: String,
    pub target: String,
    /// `open`, `warm` (with the job's id) or `resolve_many`.
    pub why: String,
    /// Since when, ms since 1970.
    pub since: u64,
}

/// The cache and its pins.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub cache_dir: std::path::PathBuf,
    pub cache_files: u64,
    pub cache_bytes: u64,
    pub budget: u64,
    pub pinned_bytes: u64,
    pub partial_files: u64,
    pub partial_bytes: u64,
    pub apps: Vec<String>,
    pub pins: Vec<Pin>,
}

/// What the agent sends: an answer or an event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Message {
    Answer {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        err: Option<String>,
    },
    Event(Event),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Progress(Progress),
    /// The job's last event.
    Done(Progress),
    /// A watched file's content changed (a new version): open it again.
    Changed {
        watch: u64,
        target: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
    },
    /// A watched file is gone (or no longer reachable as named).
    Gone {
        watch: u64,
        target: String,
    },
}

/// The `app.port` file a loopback-port agent writes in its folder (mode
/// 600): where it listens and the token a connection must say first.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortFile {
    pub port: u16,
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let r: Request =
            serde_json::from_str(r#"{"id":2,"op":"open","target":"drive:a.wav"}"#).unwrap();
        assert_eq!(
            r,
            Request {
                id: 2,
                op: Op::Open {
                    target: "drive:a.wav".into(),
                    opts: OpenOptions::new()
                }
            }
        );
        let w: Request = serde_json::from_str(
            r#"{"id":4,"op":"warm","project":"Song","kinds":["audio"],"priority":"high"}"#,
        )
        .unwrap();
        let Op::Warm(w) = w.op else { panic!() };
        assert!(w.lease && w.priority == Priority::High && w.targets.is_empty());
        // Every file-provider op survives the trip with its request id:
        // no field of an op may be called `id` (it is the request's).
        for op in [
            Op::FpItem { item: "1@2".into() },
            Op::FpList { parent: "root".into(), after: Some("5".into()), limit: Some(9) },
            Op::FpAnchor,
            Op::FpChanges { anchor: "00".into() },
            Op::FpFetch { item: "1@2".into(), into: "/tmp/x".into() },
            Op::FpCreate { parent: "root".into(), name: "a".into(), dir: true, from: None },
            Op::FpModify { item: "1@2".into(), name: Some("b".into()), parent: None, from: Some("/tmp/y".into()) },
            Op::FpDelete { item: "1@2".into() },
        ] {
            let req = Request { id: 77, op };
            let line = serde_json::to_string(&req).unwrap();
            assert_eq!(line.matches("\"id\"").count(), 1, "{line}");
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        }
        // Index questions too, nested under their own tag.
        let r: Request = serde_json::from_str(
            r#"{"id":11,"op":"index","index":"list","location":"loc-1","dir":"Kits"}"#,
        )
        .unwrap();
        assert_eq!(
            r.op,
            Op::Index(IndexOp::List {
                location: "loc-1".into(),
                dir: "Kits".into()
            })
        );
        for op in [
            IndexOp::Meta {
                location: "l".into(),
            },
            IndexOp::Page {
                location: "l".into(),
                dir: "".into(),
                after: "a".into(),
                limit: 5,
            },
            IndexOp::Copies {
                sampled: "h".into(),
                size: None,
            },
            IndexOp::LocationsOf {
                sampled: vec!["h".into()],
            },
            IndexOp::Reindex {
                location: "l".into(),
            },
            IndexOp::Sync,
        ] {
            let req = Request {
                id: 78,
                op: Op::Index(op),
            };
            let line = serde_json::to_string(&req).unwrap();
            assert_eq!(line.matches("\"id\"").count(), 1, "{line}");
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        }
        let m: Message = serde_json::from_str(r#"{"id":3,"err":"no"}"#).unwrap();
        assert!(matches!(m, Message::Answer { id: 3, .. }));
        let m: Message =
            serde_json::from_str(r#"{"event":"changed","watch":6,"target":"x","size":3}"#).unwrap();
        assert!(matches!(m, Message::Event(Event::Changed { watch: 6, .. })));
        let text =
            serde_json::to_string(&Message::Event(Event::Progress(Progress::default()))).unwrap();
        assert!(text.starts_with(r#"{"event":"progress""#), "{text}");
    }
}
