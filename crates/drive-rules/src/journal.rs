//! The journal: every action a rule took (device, rule, before and after),
//! with how to undo it; the latest dry run of each rule on each device
//! (the app's preview); each run's counts; and copies waiting for a
//! device that is off.
//!
//! One document per org, [`JOURNAL`] (`<org>/drive:rules:journal`), apart
//! from the rules so the rules stay small. Layout: `entries`: id → {
//! run, device, rule, rule_name, at, action, says, before, after,
//! content, status, error, undo, undo_requested, for_device, blob, tries
//! }; `previews`: `<rule>@<device>` → { at, matched, rows }; `runs`:
//! `<rule>@<device>` → { at, live, matched, done, failed, waiting,
//! skipped }; `available`: item key → { rule, at, device, location, path }.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};
use serde::{Deserialize, Serialize};

use crate::eval::{Item, Plan};

pub const JOURNAL: &str = "drive:rules:journal";

pub fn journal_doc(org: &str) -> String {
    format!("{org}/{JOURNAL}")
}

/// How far a journalled action got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Done,
    Failed,
    /// For a device that is off, or the node out of reach: retried.
    Waiting,
    /// A safety rule held it back.
    Skipped,
    Undone,
    /// Undo was refused (the file changed since; it is said why).
    UndoRefused,
}

impl Status {
    pub fn id(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Waiting => "waiting",
            Status::Skipped => "skipped",
            Status::Undone => "undone",
            Status::UndoRefused => "undo-refused",
        }
    }

    pub fn parse(s: &str) -> Status {
        match s {
            "failed" => Status::Failed,
            "waiting" => Status::Waiting,
            "skipped" => Status::Skipped,
            "undone" => Status::Undone,
            "undo-refused" => Status::UndoRefused,
            _ => Status::Done,
        }
    }
}

/// How to take an action back. Undo never deletes either: a copy it takes
/// back goes to a trash folder too.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "undo")]
pub enum Undo {
    /// Not reversible (sidecars: derived data, harmless to keep).
    #[default]
    None,
    /// Take a tag off again / put it back.
    Tag {
        target: String,
        tag: String,
        on: bool,
    },
    /// Put a field back as it was (`old` None: remove the field).
    Field {
        target: String,
        field: String,
        old: Option<String>,
    },
    /// Remove the part a rule added to a project.
    Part { project: String, part: String },
    /// A copy on a device: put it into the location's `.drive-trash` if it
    /// is still what was copied (`full`: its BLAKE3).
    DeviceCopy {
        location: String,
        path: String,
        full: String,
    },
    /// A move on a device: the original back from `.drive-trash` to where
    /// it was (or a free name beside it), and the copy into `.drive-trash`
    /// if it is unchanged.
    DeviceMove {
        location: String,
        from: String,
        trashed: String,
        to_location: String,
        to: String,
        full: String,
    },
    /// A Drive file moved or renamed: back to its folder and name.
    TreeMove {
        id: String,
        folder: String,
        name: String,
    },
    /// A Drive copy: into the Drive's Trash.
    TreeCopy { id: String },
    /// A Drive file trashed: back where it was.
    TreeTrash {
        id: String,
        folder: String,
        name: String,
    },
    /// Forget the wish to keep it highly available.
    Available { key: String },
}

impl Undo {
    pub fn possible(&self) -> bool {
        !matches!(self, Undo::None)
    }
}

/// One action, done (or tried).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub run: String,
    pub device: String,
    pub rule: String,
    pub rule_name: String,
    pub at: i64,
    /// `tag`, `copy`, `move`, ….
    pub action: String,
    pub says: String,
    pub before: Option<Item>,
    pub after: Option<Item>,
    pub content: String,
    pub status: Option<Status>,
    pub error: String,
    pub undo: Undo,
    /// Someone asked for it to be undone (the device that did it does it).
    pub undo_requested: i64,
    /// A copy waiting for this device to fetch it from the node.
    pub for_device: String,
    /// The blob (full BLAKE3) on the node a waiting copy fetches.
    pub blob: String,
    pub tries: i64,
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn n(m: &LoroMap, k: &str) -> i64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

fn read(id: &str, m: &LoroMap) -> Entry {
    Entry {
        id: id.to_string(),
        run: s(m, "run"),
        device: s(m, "device"),
        rule: s(m, "rule"),
        rule_name: s(m, "rule_name"),
        at: n(m, "at"),
        action: s(m, "action"),
        says: s(m, "says"),
        before: serde_json::from_str(&s(m, "before")).ok(),
        after: serde_json::from_str(&s(m, "after")).ok(),
        content: s(m, "content"),
        status: Some(Status::parse(&s(m, "status"))),
        error: s(m, "error"),
        undo: serde_json::from_str(&s(m, "undo")).unwrap_or_default(),
        undo_requested: n(m, "undo_requested"),
        for_device: s(m, "for_device"),
        blob: s(m, "blob"),
        tries: n(m, "tries"),
    }
}

/// Write an entry (new, or its new state).
pub fn record(doc: &LoroDoc, e: &Entry) -> loro::LoroResult<String> {
    let id = if e.id.is_empty() {
        crate::new_id(doc, "j", e.at)
    } else {
        e.id.clone()
    };
    let m = doc.get_map("entries").ensure_mergeable_map(&id)?;
    let j = |v: &Option<Item>| {
        v.as_ref()
            .map(|i| serde_json::to_string(i).expect("serializes"))
            .unwrap_or_default()
    };
    for (k, v) in [
        ("run", e.run.clone()),
        ("device", e.device.clone()),
        ("rule", e.rule.clone()),
        ("rule_name", e.rule_name.clone()),
        ("action", e.action.clone()),
        ("says", e.says.clone()),
        ("before", j(&e.before)),
        ("after", j(&e.after)),
        ("content", e.content.clone()),
        ("status", e.status.unwrap_or(Status::Done).id().to_string()),
        ("error", e.error.clone()),
        ("undo", serde_json::to_string(&e.undo).expect("serializes")),
        ("for_device", e.for_device.clone()),
        ("blob", e.blob.clone()),
    ] {
        if s(&m, k) != v {
            m.insert(k, v.as_str())?;
        }
    }
    for (k, v) in [
        ("at", e.at),
        ("undo_requested", e.undo_requested),
        ("tries", e.tries),
    ] {
        if n(&m, k) != v {
            m.insert(k, v)?;
        }
    }
    Ok(id)
}

/// Every entry, newest first.
pub fn entries(doc: &LoroDoc) -> Vec<Entry> {
    let map = doc.get_map("entries");
    let mut out: Vec<Entry> = map
        .keys()
        .filter_map(|k| child(&map, &k).map(|m| read(&k, &m)))
        .collect();
    out.sort_by(|a, b| b.at.cmp(&a.at).then(b.id.cmp(&a.id)));
    out
}

pub fn entry(doc: &LoroDoc, id: &str) -> Option<Entry> {
    child(&doc.get_map("entries"), id).map(|m| read(id, &m))
}

/// Ask the device that did an action to undo it.
pub fn request_undo(doc: &LoroDoc, id: &str, now: i64) -> loro::LoroResult<bool> {
    let Some(m) = child(&doc.get_map("entries"), id) else {
        return Ok(false);
    };
    m.insert("undo_requested", now)?;
    Ok(true)
}

/// One row of a dry run: a file and what would happen to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub item: Item,
    pub steps: Vec<RowStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowStep {
    pub kind: String,
    pub says: String,
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub noop: bool,
}

impl Row {
    pub fn of(plan: &Plan) -> Row {
        Row {
            item: plan.item.clone(),
            steps: plan
                .steps
                .iter()
                .map(|s| RowStep {
                    kind: s.effect.kind().to_string(),
                    says: s.says.clone(),
                    blocked: s.blocked.clone(),
                    noop: s.noop,
                })
                .collect(),
        }
    }
}

/// A rule's latest dry run on one device.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Preview {
    pub rule: String,
    pub device: String,
    pub at: i64,
    /// Files it matched (`rows` holds at most [`PREVIEW_ROWS`]).
    pub matched: usize,
    pub rows: Vec<Row>,
}

pub const PREVIEW_ROWS: usize = 200;

fn key(rule: &str, device: &str) -> String {
    format!("{rule}@{device}")
}

pub fn put_preview(doc: &LoroDoc, p: &Preview) -> loro::LoroResult<()> {
    let m = doc
        .get_map("previews")
        .ensure_mergeable_map(&key(&p.rule, &p.device))?;
    m.insert("at", p.at)?;
    m.insert("matched", p.matched as i64)?;
    let rows: Vec<&Row> = p.rows.iter().take(PREVIEW_ROWS).collect();
    let json = serde_json::to_string(&rows).expect("serializes");
    if s(&m, "rows") != json {
        m.insert("rows", json.as_str())?;
    }
    Ok(())
}

/// Every device's latest dry run of a rule.
pub fn previews(doc: &LoroDoc, rule: &str) -> Vec<Preview> {
    let map = doc.get_map("previews");
    let prefix = format!("{rule}@");
    let mut out: Vec<Preview> = map
        .keys()
        .filter_map(|k| {
            let device = k.strip_prefix(&prefix)?.to_string();
            let m = child(&map, &k)?;
            Some(Preview {
                rule: rule.to_string(),
                device,
                at: n(&m, "at"),
                matched: n(&m, "matched") as usize,
                rows: serde_json::from_str(&s(&m, "rows")).unwrap_or_default(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// A run's counts.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Run {
    pub rule: String,
    pub device: String,
    pub at: i64,
    pub live: bool,
    pub matched: usize,
    pub done: usize,
    pub failed: usize,
    pub waiting: usize,
    pub skipped: usize,
    /// Why it did not run at all (a regular expression that does not compile).
    pub error: String,
}

pub fn put_run(doc: &LoroDoc, r: &Run) -> loro::LoroResult<()> {
    let m = doc
        .get_map("runs")
        .ensure_mergeable_map(&key(&r.rule, &r.device))?;
    m.insert("at", r.at)?;
    m.insert("live", r.live)?;
    for (k, v) in [
        ("matched", r.matched),
        ("done", r.done),
        ("failed", r.failed),
        ("waiting", r.waiting),
        ("skipped", r.skipped),
    ] {
        m.insert(k, v as i64)?;
    }
    m.insert("error", r.error.as_str())?;
    Ok(())
}

pub fn runs(doc: &LoroDoc, rule: &str) -> Vec<Run> {
    let map = doc.get_map("runs");
    let prefix = format!("{rule}@");
    let mut out: Vec<Run> = map
        .keys()
        .filter_map(|k| {
            let device = k.strip_prefix(&prefix)?.to_string();
            let m = child(&map, &k)?;
            Some(Run {
                rule: rule.to_string(),
                device,
                at: n(&m, "at"),
                live: matches!(
                    m.get("live"),
                    Some(ValueOrContainer::Value(LoroValue::Bool(true)))
                ),
                matched: n(&m, "matched") as usize,
                done: n(&m, "done") as usize,
                failed: n(&m, "failed") as usize,
                waiting: n(&m, "waiting") as usize,
                skipped: n(&m, "skipped") as usize,
                error: s(&m, "error"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// A rule's wish that a file be highly available (#13): recorded even
/// while there is no public storage, so it is carried out once there is.
pub fn wish_available(
    doc: &LoroDoc,
    item: &Item,
    rule: &str,
    now: i64,
) -> loro::LoroResult<String> {
    let k = item.key();
    let m = doc.get_map("available").ensure_mergeable_map(&k)?;
    m.insert("rule", rule)?;
    m.insert("at", now)?;
    m.insert(
        "item",
        serde_json::to_string(item).expect("serializes").as_str(),
    )?;
    Ok(k)
}

pub fn unwish_available(doc: &LoroDoc, key: &str) -> loro::LoroResult<()> {
    let map = doc.get_map("available");
    if map.get(key).is_some() {
        map.delete(key)?;
    }
    Ok(())
}

/// Every file a rule wants highly available.
pub fn wished_available(doc: &LoroDoc) -> Vec<Item> {
    let map = doc.get_map("available");
    map.keys()
        .filter_map(|k| child(&map, &k))
        .filter_map(|m| serde_json::from_str(&s(&m, "item")).ok())
        .collect()
}
