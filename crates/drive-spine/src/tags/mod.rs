//! Tags and fields: names people put on files, documents and projects, and
//! named values (text, dates) beside them, for finding things again.
//!
//! One document per org, [`TAGS`] (`<org>/drive:tags`), holds every tag and
//! every assignment, so tagging is live, works offline and merges when
//! several people tag at once (a Loro document; each assignment records who
//! made it and when, and every change carries its author in the document's
//! history, [`crate::history`]).
//!
//! **What a tag is put on** ([`Target`]) is keyed so the tag follows the
//! thing, not where it happens to be:
//!
//! | Target | Key | Follows |
//! |---|---|---|
//! | A file of the space's Drive tree | `file:<tree id>` | moves and renames inside the Drive |
//! | A file on a computer (a location's index) | `content:<sampled hash>` | moves, renames, copies, every device: every copy shows the same tags |
//! | An app's document | `doc:<id>` | the document |
//! | A project | `project:<id>` | the project |
//!
//! **Tags** have a name, a shorthand, aliases, a colour from a fixed palette
//! ([`Color`]), any number of parent tags (searching a parent finds its
//! children; a parent that would make a cycle is refused), "category" (the
//! inspector groups a thing's tags under the category tags they descend
//! from) and "hidden" (things with a hidden tag stay out of search results
//! unless the search names it). Two tags are built in: Archived (hidden)
//! and Favorite.
//!
//! **Fields** have a name and a kind ([`FieldKind`]: text, multiline text,
//! date, date and time); a thing may have several, named freely, or from a
//! few [`templates`].
//!
//! Layout: `tags` map: tag id → { name, shorthand, aliases: {alias → true},
//! color, parents: {tag id → true}, category, hidden, created, by };
//! `targets` map: target key → { tags: {tag id → { by, at }}, fields:
//! {field id → { name, kind, value, by, at }} }.
//!
//! The search language over all of this is [`query`].

pub mod query;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// The tags document's id within an org (`use_doc` adds the org).
pub const TAGS: &str = "drive:tags";

/// The tags document's full id in an org.
pub fn tags_doc(org: &str) -> String {
    format!("{org}/{TAGS}")
}

/// The built-in hidden tag: things tagged with it leave search results.
pub const ARCHIVED: &str = "archived";
/// The built-in Favorite tag.
pub const FAVORITE: &str = "favorite";

// ── targets ─────────────────────────────────────────────────────────────

/// What a tag or field is put on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// A file or folder of the space's Drive tree, by its tree id.
    File(String),
    /// A file on any computer, by its content (the sampled BLAKE3 hash,
    /// which includes the size): every copy everywhere.
    Content(String),
    /// An app's document, by its id within the org.
    Doc(String),
    /// A project of the space's registry.
    Project(String),
    /// A folder of a location (a computer's indexed folder), by its path
    /// inside it. The device that walks the location moves its tags and
    /// links to the new path when the folder moves ([`rekey`]).
    Folder { location: String, path: String },
}

impl Target {
    /// The key it is stored under.
    pub fn key(&self) -> String {
        match self {
            Target::File(id) => format!("file:{id}"),
            Target::Content(h) => format!("content:{h}"),
            Target::Doc(id) => format!("doc:{id}"),
            Target::Project(id) => format!("project:{id}"),
            Target::Folder { location, path } => format!("folder:{location}:{path}"),
        }
    }

    /// A key back into a target.
    pub fn parse(key: &str) -> Option<Target> {
        let (kind, rest) = key.split_once(':')?;
        if rest.is_empty() {
            return None;
        }
        Some(match kind {
            "file" => Target::File(rest.into()),
            "content" => Target::Content(rest.into()),
            "doc" => Target::Doc(rest.into()),
            "project" => Target::Project(rest.into()),
            "folder" => {
                let (location, path) = rest.split_once(':')?;
                if location.is_empty() {
                    return None;
                }
                Target::Folder {
                    location: location.into(),
                    path: path.into(),
                }
            }
            _ => return None,
        })
    }
}

// ── colours ─────────────────────────────────────────────────────────────

/// The palette tags are coloured from (fixed and named, so every app shows
/// a tag the same way).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Color {
    #[default]
    None,
    Red,
    Orange,
    Amber,
    Yellow,
    Lime,
    Green,
    Teal,
    Cyan,
    Blue,
    Indigo,
    Violet,
    Pink,
    Brown,
    Grey,
}

impl Color {
    pub const ALL: [Color; 15] = [
        Color::None,
        Color::Red,
        Color::Orange,
        Color::Amber,
        Color::Yellow,
        Color::Lime,
        Color::Green,
        Color::Teal,
        Color::Cyan,
        Color::Blue,
        Color::Indigo,
        Color::Violet,
        Color::Pink,
        Color::Brown,
        Color::Grey,
    ];

    /// Its stored name (`red`; `""` for none).
    pub fn id(self) -> &'static str {
        match self {
            Color::None => "",
            Color::Red => "red",
            Color::Orange => "orange",
            Color::Amber => "amber",
            Color::Yellow => "yellow",
            Color::Lime => "lime",
            Color::Green => "green",
            Color::Teal => "teal",
            Color::Cyan => "cyan",
            Color::Blue => "blue",
            Color::Indigo => "indigo",
            Color::Violet => "violet",
            Color::Pink => "pink",
            Color::Brown => "brown",
            Color::Grey => "grey",
        }
    }

    /// What people call it.
    pub fn label(self) -> &'static str {
        match self {
            Color::None => "No colour",
            Color::Red => "Red",
            Color::Orange => "Orange",
            Color::Amber => "Amber",
            Color::Yellow => "Yellow",
            Color::Lime => "Lime",
            Color::Green => "Green",
            Color::Teal => "Teal",
            Color::Cyan => "Cyan",
            Color::Blue => "Blue",
            Color::Indigo => "Indigo",
            Color::Violet => "Violet",
            Color::Pink => "Pink",
            Color::Brown => "Brown",
            Color::Grey => "Grey",
        }
    }

    /// Its colour (sRGB hex), readable as a chip's background on a dark
    /// sheet and as text on a light one.
    pub fn hex(self) -> &'static str {
        match self {
            Color::None => "#52525b",
            Color::Red => "#ef4444",
            Color::Orange => "#f97316",
            Color::Amber => "#f59e0b",
            Color::Yellow => "#eab308",
            Color::Lime => "#84cc16",
            Color::Green => "#22c55e",
            Color::Teal => "#14b8a6",
            Color::Cyan => "#06b6d4",
            Color::Blue => "#3b82f6",
            Color::Indigo => "#6366f1",
            Color::Violet => "#8b5cf6",
            Color::Pink => "#ec4899",
            Color::Brown => "#a16207",
            Color::Grey => "#71717a",
        }
    }

    /// A stored or typed name back into a colour (unknown: none).
    pub fn parse(s: &str) -> Color {
        let s = s.trim().to_lowercase();
        let s = if s == "gray" { "grey".to_string() } else { s };
        Color::ALL
            .into_iter()
            .find(|c| c.id() == s)
            .unwrap_or(Color::None)
    }
}

// ── tags ────────────────────────────────────────────────────────────────

/// A tag.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tag {
    pub id: String,
    pub name: String,
    pub shorthand: String,
    /// Other names it is found by, sorted.
    pub aliases: Vec<String>,
    pub color: Color,
    /// Its parent tags' ids, sorted (a search for a parent finds it).
    pub parents: Vec<String>,
    /// Groups a thing's tags in the inspector.
    pub category: bool,
    /// Things with it stay out of search results unless a search names it.
    pub hidden: bool,
    /// Milliseconds since the epoch (0 for the built-in tags).
    pub created: i64,
    pub by: String,
    /// One of Drive's own tags (cannot be deleted).
    pub builtin: bool,
}

impl Tag {
    /// The names it is found by: its name, shorthand and aliases.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str())
            .chain((!self.shorthand.is_empty()).then_some(self.shorthand.as_str()))
            .chain(self.aliases.iter().map(String::as_str))
    }
}

fn builtins() -> Vec<Tag> {
    vec![
        Tag {
            id: ARCHIVED.into(),
            name: "Archived".into(),
            color: Color::Grey,
            hidden: true,
            builtin: true,
            ..Tag::default()
        },
        Tag {
            id: FAVORITE.into(),
            name: "Favorite".into(),
            color: Color::Amber,
            builtin: true,
            ..Tag::default()
        },
    ]
}

/// Why an edit was refused.
#[derive(Debug, PartialEq)]
pub enum TagError {
    /// The parent would make the tag its own ancestor.
    Cycle {
        tag: String,
        parent: String,
    },
    NotFound(String),
    /// Drive's own tags stay.
    Builtin(String),
    /// A tag needs a name.
    NoName,
    Loro(String),
}

impl std::fmt::Display for TagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TagError::Cycle { tag, parent } => {
                write!(
                    f,
                    "{parent} is already under {tag}: it cannot be its parent"
                )
            }
            TagError::NotFound(t) => write!(f, "no tag {t}"),
            TagError::Builtin(t) => write!(f, "{t} is one of Drive's own tags"),
            TagError::NoName => write!(f, "a tag needs a name"),
            TagError::Loro(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TagError {}

impl From<loro::LoroError> for TagError {
    fn from(e: loro::LoroError) -> Self {
        TagError::Loro(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, TagError>;

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

fn b(m: &LoroMap, k: &str) -> bool {
    matches!(
        m.get(k),
        Some(ValueOrContainer::Value(LoroValue::Bool(true)))
    )
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

fn keys(m: &LoroMap, k: &str) -> Vec<String> {
    let mut out: Vec<String> = child(m, k)
        .map(|c| c.keys().map(|k| k.to_string()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Time-ordered and unique across devices (as comments' ids are).
fn new_id(doc: &LoroDoc, prefix: &str, now: i64) -> String {
    let peer = doc.peer_id();
    let counter =
        doc.oplog_vv().get(&peer).copied().unwrap_or(0) + doc.get_pending_txn_len() as i32;
    format!("{prefix}{now:x}-{peer:x}-{counter:x}")
}

fn tags_map(doc: &LoroDoc) -> LoroMap {
    doc.get_map("tags")
}

fn read_tag(id: &str, m: &LoroMap) -> Tag {
    Tag {
        id: id.to_string(),
        name: s(m, "name"),
        shorthand: s(m, "shorthand"),
        aliases: child(m, "aliases")
            .map(|a| {
                let mut v: Vec<String> = a
                    .keys()
                    .filter_map(|k| match a.get(&k) {
                        Some(ValueOrContainer::Value(LoroValue::String(v))) => Some(v.to_string()),
                        _ => None,
                    })
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default(),
        color: Color::parse(&s(m, "color")),
        parents: keys(m, "parents"),
        category: b(m, "category"),
        hidden: b(m, "hidden"),
        created: n(m, "created"),
        by: s(m, "by"),
        builtin: false,
    }
}

/// The tag's map, the built-ins written out first if this is their first
/// edit here.
fn tag_map(doc: &LoroDoc, id: &str) -> Result<LoroMap> {
    let map = tags_map(doc);
    if let Some(m) = child(&map, id) {
        return Ok(m);
    }
    let Some(t) = builtins().into_iter().find(|t| t.id == id) else {
        return Err(TagError::NotFound(id.into()));
    };
    let m = map.ensure_mergeable_map(id)?;
    m.insert("name", t.name.as_str())?;
    m.insert("color", t.color.id())?;
    m.insert("hidden", t.hidden)?;
    Ok(m)
}

/// What a new tag is.
#[derive(Clone, Debug, Default)]
pub struct NewTag<'a> {
    pub name: &'a str,
    pub shorthand: &'a str,
    pub color: Color,
    pub parents: &'a [String],
    pub category: bool,
}

/// Make a tag; returns its id. `by`: who; `now`: milliseconds.
pub fn create(doc: &LoroDoc, t: NewTag, by: &str, now: i64) -> Result<String> {
    let name = t.name.trim();
    if name.is_empty() {
        return Err(TagError::NoName);
    }
    for p in t.parents {
        if Library::load(doc).get(p).is_none() {
            return Err(TagError::NotFound(p.clone()));
        }
    }
    let id = new_id(doc, "t", now);
    let m = tags_map(doc).ensure_mergeable_map(&id)?;
    m.insert("name", name)?;
    m.insert("shorthand", t.shorthand.trim())?;
    m.insert("color", t.color.id())?;
    m.insert("category", t.category)?;
    m.insert("created", now)?;
    m.insert("by", by)?;
    let parents = m.ensure_mergeable_map("parents")?;
    for p in t.parents {
        parents.insert(p, true)?;
    }
    Ok(id)
}

pub fn rename(doc: &LoroDoc, id: &str, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TagError::NoName);
    }
    tag_map(doc, id)?.insert("name", name)?;
    Ok(())
}

pub fn set_shorthand(doc: &LoroDoc, id: &str, shorthand: &str) -> Result<()> {
    tag_map(doc, id)?.insert("shorthand", shorthand.trim())?;
    Ok(())
}

pub fn set_color(doc: &LoroDoc, id: &str, color: Color) -> Result<()> {
    tag_map(doc, id)?.insert("color", color.id())?;
    Ok(())
}

pub fn set_category(doc: &LoroDoc, id: &str, on: bool) -> Result<()> {
    tag_map(doc, id)?.insert("category", on)?;
    Ok(())
}

pub fn set_hidden(doc: &LoroDoc, id: &str, on: bool) -> Result<()> {
    tag_map(doc, id)?.insert("hidden", on)?;
    Ok(())
}

/// Add another name the tag is found by (kept once, whatever its case).
pub fn add_alias(doc: &LoroDoc, id: &str, alias: &str) -> Result<()> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Ok(());
    }
    tag_map(doc, id)?
        .ensure_mergeable_map("aliases")?
        .insert(&alias.to_lowercase(), alias)?;
    Ok(())
}

pub fn remove_alias(doc: &LoroDoc, id: &str, alias: &str) -> Result<()> {
    let m = tag_map(doc, id)?.ensure_mergeable_map("aliases")?;
    let k = alias.trim().to_lowercase();
    if m.get(&k).is_some() {
        m.delete(&k)?;
    }
    Ok(())
}

/// Put the tag under a parent; refused when the parent is the tag or is
/// already under it (a cycle).
pub fn add_parent(doc: &LoroDoc, id: &str, parent: &str) -> Result<()> {
    let lib = Library::load(doc);
    if lib.get(parent).is_none() {
        return Err(TagError::NotFound(parent.into()));
    }
    if lib.get(id).is_none() {
        return Err(TagError::NotFound(id.into()));
    }
    if parent == id || lib.descendants(id).contains(parent) {
        return Err(TagError::Cycle {
            tag: lib.label(id),
            parent: lib.label(parent),
        });
    }
    tag_map(doc, id)?
        .ensure_mergeable_map("parents")?
        .insert(parent, true)?;
    Ok(())
}

pub fn remove_parent(doc: &LoroDoc, id: &str, parent: &str) -> Result<()> {
    let m = tag_map(doc, id)?.ensure_mergeable_map("parents")?;
    if m.get(parent).is_some() {
        m.delete(parent)?;
    }
    Ok(())
}

/// Delete a tag: it leaves everything it was on and every tag it was the
/// parent of. Drive's own tags stay.
pub fn delete(doc: &LoroDoc, id: &str) -> Result<()> {
    if builtins().iter().any(|t| t.id == id) {
        return Err(TagError::Builtin(id.into()));
    }
    let map = tags_map(doc);
    if map.get(id).is_none() {
        return Err(TagError::NotFound(id.into()));
    }
    map.delete(id)?;
    for k in map.keys() {
        if let Some(p) = child(&map, &k).and_then(|m| child(&m, "parents"))
            && p.get(id).is_some()
        {
            p.delete(id)?;
        }
    }
    let targets = targets_map(doc);
    for k in targets.keys() {
        if let Some(t) = child(&targets, &k).and_then(|m| child(&m, "tags"))
            && t.get(id).is_some()
        {
            t.delete(id)?;
        }
    }
    Ok(())
}

// ── assignments ─────────────────────────────────────────────────────────

/// Move everything on `from` (tags and fields) to `to`, as when a folder
/// moved: what `to` already has stays, `from` is left with nothing.
/// Returns whether `from` had anything.
pub fn rekey(doc: &LoroDoc, from: &Target, to: &Target) -> Result<bool> {
    let targets = targets_map(doc);
    let Some(old) = child(&targets, &from.key()) else {
        return Ok(false);
    };
    if from != to {
        copy_map(&old, &target_map(doc, to)?)?;
        targets.delete(&from.key())?;
    }
    Ok(true)
}

/// Every value and map of `src` written into `dst` (maps merged).
pub(crate) fn copy_map(src: &LoroMap, dst: &LoroMap) -> loro::LoroResult<()> {
    for k in src.keys() {
        match src.get(&k) {
            Some(ValueOrContainer::Container(c)) => {
                if let Ok(m) = c.into_map() {
                    copy_map(&m, &dst.ensure_mergeable_map(&k)?)?;
                }
            }
            Some(ValueOrContainer::Value(v)) => {
                dst.insert(&k, v)?;
            }
            None => {}
        }
    }
    Ok(())
}

fn targets_map(doc: &LoroDoc) -> LoroMap {
    doc.get_map("targets")
}

fn target_map(doc: &LoroDoc, t: &Target) -> Result<LoroMap> {
    Ok(targets_map(doc).ensure_mergeable_map(&t.key())?)
}

/// Put a tag on a thing.
pub fn tag(doc: &LoroDoc, target: &Target, tag_id: &str, by: &str, now: i64) -> Result<()> {
    if Library::load(doc).get(tag_id).is_none() {
        return Err(TagError::NotFound(tag_id.into()));
    }
    let m = target_map(doc, target)?
        .ensure_mergeable_map("tags")?
        .ensure_mergeable_map(tag_id)?;
    m.insert("by", by)?;
    m.insert("at", now)?;
    Ok(())
}

/// Take a tag off a thing.
pub fn untag(doc: &LoroDoc, target: &Target, tag_id: &str) -> Result<()> {
    if let Some(t) = child(&targets_map(doc), &target.key()).and_then(|m| child(&m, "tags"))
        && t.get(tag_id).is_some()
    {
        t.delete(tag_id)?;
    }
    Ok(())
}

/// A field's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FieldKind {
    #[default]
    Text,
    Multiline,
    /// `YYYY-MM-DD`.
    Date,
    /// `YYYY-MM-DDTHH:MM`.
    DateTime,
}

impl FieldKind {
    pub const ALL: [FieldKind; 4] = [
        FieldKind::Text,
        FieldKind::Multiline,
        FieldKind::Date,
        FieldKind::DateTime,
    ];

    pub fn id(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Multiline => "multiline",
            FieldKind::Date => "date",
            FieldKind::DateTime => "datetime",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FieldKind::Text => "Text",
            FieldKind::Multiline => "Long text",
            FieldKind::Date => "Date",
            FieldKind::DateTime => "Date and time",
        }
    }

    pub fn parse(s: &str) -> FieldKind {
        match s.trim().to_lowercase().as_str() {
            "multiline" | "long" | "long text" => FieldKind::Multiline,
            "date" => FieldKind::Date,
            "datetime" | "date and time" => FieldKind::DateTime,
            _ => FieldKind::Text,
        }
    }
}

/// A named value on a thing.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub id: String,
    pub name: String,
    pub kind: FieldKind,
    pub value: String,
    pub by: String,
    pub at: i64,
}

/// Fields people often want, ready to add (any name works too).
pub fn templates() -> Vec<(&'static str, FieldKind)> {
    vec![
        ("Title", FieldKind::Text),
        ("Artist", FieldKind::Text),
        ("Album", FieldKind::Text),
        ("Source", FieldKind::Text),
        ("Description", FieldKind::Multiline),
        ("Notes", FieldKind::Multiline),
        ("Date created", FieldKind::Date),
        ("Date released", FieldKind::Date),
        ("Recorded", FieldKind::DateTime),
    ]
}

/// Add a field to a thing; returns its id.
pub fn add_field(
    doc: &LoroDoc,
    target: &Target,
    name: &str,
    kind: FieldKind,
    value: &str,
    by: &str,
    now: i64,
) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TagError::NoName);
    }
    let id = new_id(doc, "f", now);
    let m = target_map(doc, target)?
        .ensure_mergeable_map("fields")?
        .ensure_mergeable_map(&id)?;
    m.insert("name", name)?;
    m.insert("kind", kind.id())?;
    m.insert("value", value)?;
    m.insert("by", by)?;
    m.insert("at", now)?;
    Ok(id)
}

/// Change a field's value.
pub fn set_field(
    doc: &LoroDoc,
    target: &Target,
    field: &str,
    value: &str,
    by: &str,
    now: i64,
) -> Result<()> {
    let Some(m) = child(&targets_map(doc), &target.key())
        .and_then(|t| child(&t, "fields"))
        .and_then(|f| child(&f, field))
    else {
        return Err(TagError::NotFound(field.into()));
    };
    m.insert("value", value)?;
    m.insert("by", by)?;
    m.insert("at", now)?;
    Ok(())
}

pub fn remove_field(doc: &LoroDoc, target: &Target, field: &str) -> Result<()> {
    if let Some(f) = child(&targets_map(doc), &target.key()).and_then(|t| child(&t, "fields"))
        && f.get(field).is_some()
    {
        f.delete(field)?;
    }
    Ok(())
}

/// Who put a tag on a thing, and when.
#[derive(Clone, Debug, PartialEq)]
pub struct Assigned {
    pub tag: String,
    pub by: String,
    pub at: i64,
}

/// What is on one thing.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct OnTarget {
    /// Its tags (only tags that exist), oldest first.
    pub tags: Vec<Assigned>,
    /// Its fields, in the order they were added.
    pub fields: Vec<Field>,
}

impl OnTarget {
    pub fn tag_ids(&self) -> Vec<String> {
        self.tags.iter().map(|a| a.tag.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.fields.is_empty()
    }
}

// ── the library: everything, read ───────────────────────────────────────

/// Every tag and assignment, read once, with the hierarchy worked out.
///
/// Parents merged from two devices can close a cycle (one made A a parent
/// of B while the other made B a parent of A); the library drops the
/// closing edges in a fixed order, so every device reads the same
/// hierarchy, and [`Library::broken`] lists them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Library {
    tags: BTreeMap<String, Tag>,
    /// Child id → its parents, cycles removed.
    parents: HashMap<String, Vec<String>>,
    children: HashMap<String, Vec<String>>,
    broken: Vec<(String, String)>,
    targets: BTreeMap<String, OnTarget>,
}

impl Library {
    pub fn load(doc: &LoroDoc) -> Library {
        let map = tags_map(doc);
        let mut tags: BTreeMap<String, Tag> = BTreeMap::new();
        for t in builtins() {
            tags.insert(t.id.clone(), t);
        }
        for k in map.keys() {
            if let Some(m) = child(&map, &k) {
                let mut t = read_tag(&k, &m);
                if let Some(b) = tags.get(k.as_str()).filter(|b| b.builtin) {
                    t.builtin = true;
                    if t.name.is_empty() {
                        t.name = b.name.clone();
                    }
                }
                tags.insert(k.to_string(), t);
            }
        }
        // The hierarchy without cycles: edges added in a fixed order, each
        // refused if its parent already descends from its child.
        let mut edges: Vec<(String, String)> = tags
            .values()
            .flat_map(|t| t.parents.iter().map(|p| (t.id.clone(), p.clone())))
            .filter(|(_, p)| tags.contains_key(p))
            .collect();
        edges.sort();
        let mut parents: HashMap<String, Vec<String>> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut broken = Vec::new();
        for (c, p) in edges {
            if c == p || reaches(&parents, &p, &c) {
                broken.push((c, p));
                continue;
            }
            parents.entry(c.clone()).or_default().push(p.clone());
            children.entry(p).or_default().push(c);
        }
        for t in tags.values_mut() {
            t.parents = parents.get(&t.id).cloned().unwrap_or_default();
        }

        let tmap = targets_map(doc);
        let mut targets = BTreeMap::new();
        for k in tmap.keys() {
            let Some(m) = child(&tmap, &k) else { continue };
            let mut on = OnTarget::default();
            if let Some(tg) = child(&m, "tags") {
                for id in tg.keys() {
                    if !tags.contains_key(id.as_str()) {
                        continue;
                    }
                    let a = child(&tg, &id);
                    on.tags.push(Assigned {
                        tag: id.to_string(),
                        by: a.as_ref().map(|a| s(a, "by")).unwrap_or_default(),
                        at: a.as_ref().map(|a| n(a, "at")).unwrap_or_default(),
                    });
                }
            }
            if let Some(fs) = child(&m, "fields") {
                for id in fs.keys() {
                    if let Some(f) = child(&fs, &id) {
                        on.fields.push(Field {
                            id: id.to_string(),
                            name: s(&f, "name"),
                            kind: FieldKind::parse(&s(&f, "kind")),
                            value: s(&f, "value"),
                            by: s(&f, "by"),
                            at: n(&f, "at"),
                        });
                    }
                }
            }
            on.tags.sort_by(|a, b| (a.at, &a.tag).cmp(&(b.at, &b.tag)));
            // Ids are time-ordered: fields stay in the order they were added.
            on.fields.sort_by(|a, b| a.id.cmp(&b.id));
            if !on.is_empty() {
                targets.insert(k.to_string(), on);
            }
        }
        Library {
            tags,
            parents,
            children,
            broken,
            targets,
        }
    }

    pub fn get(&self, id: &str) -> Option<&Tag> {
        self.tags.get(id)
    }

    /// Every tag, by name.
    pub fn all(&self) -> Vec<&Tag> {
        let mut v: Vec<&Tag> = self.tags.values().collect();
        v.sort_by(|a, b| (a.name.to_lowercase(), &a.id).cmp(&(b.name.to_lowercase(), &b.id)));
        v
    }

    /// Tags with no parent, by name.
    pub fn roots(&self) -> Vec<&Tag> {
        self.all()
            .into_iter()
            .filter(|t| t.parents.is_empty())
            .collect()
    }

    /// A tag's children, by name.
    pub fn children(&self, id: &str) -> Vec<&Tag> {
        let mut v: Vec<&Tag> = self
            .children
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|c| self.tags.get(c))
            .collect();
        v.sort_by_key(|t| t.name.to_lowercase());
        v
    }

    /// The tag and every tag under it, however deep.
    pub fn descendants(&self, id: &str) -> BTreeSet<String> {
        walk(&self.children, id)
    }

    /// The tag and every tag above it.
    pub fn ancestors(&self, id: &str) -> BTreeSet<String> {
        walk(&self.parents, id)
    }

    /// Parent edges dropped because they closed a cycle: (child, parent).
    pub fn broken(&self) -> &[(String, String)] {
        &self.broken
    }

    /// How it is shown: its name, with a parent in brackets when another
    /// tag has the same name ("Freddy (FNAF)").
    pub fn label(&self, id: &str) -> String {
        let Some(t) = self.tags.get(id) else {
            return id.to_string();
        };
        let lname = t.name.to_lowercase();
        let clash = self
            .tags
            .values()
            .any(|o| o.id != t.id && o.name.to_lowercase() == lname);
        match t.parents.first().and_then(|p| self.tags.get(p)) {
            Some(p) if clash => format!("{} ({})", t.name, short(p)),
            _ => t.name.clone(),
        }
    }

    /// Tags whose name, shorthand or an alias is this text (any case;
    /// underscores read as spaces).
    pub fn resolve(&self, word: &str) -> Vec<String> {
        let w = word.trim().to_lowercase();
        let spaced = w.replace('_', " ");
        if w.is_empty() {
            return Vec::new();
        }
        self.all()
            .into_iter()
            .filter(|t| {
                t.names().any(|n| {
                    let n = n.to_lowercase();
                    n == w || n == spaced
                })
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Tags for a half-typed name, best first: exact, then name or
    /// shorthand starting with it, then containing it.
    pub fn suggest(&self, text: &str) -> Vec<&Tag> {
        let q = text.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(u8, &Tag)> = self
            .tags
            .values()
            .filter_map(|t| {
                let best = t
                    .names()
                    .map(str::to_lowercase)
                    .filter_map(|n| {
                        if n == q {
                            Some(0)
                        } else if n.starts_with(&q) {
                            Some(1)
                        } else if n.split_whitespace().any(|w| w.starts_with(&q)) {
                            Some(2)
                        } else if n.contains(&q) {
                            Some(3)
                        } else {
                            None
                        }
                    })
                    .min()?;
                Some((best, t))
            })
            .collect();
        scored.sort_by(|a, b| {
            (a.0, a.1.name.to_lowercase(), &a.1.id).cmp(&(b.0, b.1.name.to_lowercase(), &b.1.id))
        });
        scored.into_iter().map(|(_, t)| t).collect()
    }

    /// What is on a thing.
    pub fn on(&self, target: &Target) -> OnTarget {
        self.targets.get(&target.key()).cloned().unwrap_or_default()
    }

    /// Every thing with something on it.
    pub fn targets(&self) -> impl Iterator<Item = (Target, &OnTarget)> {
        self.targets
            .iter()
            .filter_map(|(k, v)| Target::parse(k).map(|t| (t, v)))
    }

    /// Things carrying the tag or any tag under it.
    pub fn tagged(&self, id: &str) -> Vec<Target> {
        let under = self.descendants(id);
        self.targets()
            .filter(|(_, on)| on.tags.iter().any(|a| under.contains(&a.tag)))
            .map(|(t, _)| t)
            .collect()
    }

    /// How many things carry each tag, counting a thing once under every
    /// tag above its own.
    pub fn counts(&self) -> HashMap<String, usize> {
        let mut out: HashMap<String, usize> = HashMap::new();
        for on in self.targets.values() {
            let mut seen = BTreeSet::new();
            for a in &on.tags {
                seen.extend(self.ancestors(&a.tag));
            }
            for t in seen {
                *out.entry(t).or_default() += 1;
            }
        }
        out
    }

    /// A thing's tags grouped for the inspector: each category tag above
    /// them (by name) with the tags under it, then the rest (`None`).
    pub fn grouped(&self, tag_ids: &[String]) -> Vec<(Option<String>, Vec<String>)> {
        let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let mut rest = Vec::new();
        for id in tag_ids {
            let cats: Vec<&Tag> = self
                .ancestors(id)
                .iter()
                .filter_map(|a| self.tags.get(a))
                .filter(|t| t.category && t.id != *id)
                .collect();
            if cats.is_empty() {
                rest.push(id.clone());
            }
            for c in cats {
                groups
                    .entry((c.name.to_lowercase(), c.id.clone()))
                    .or_default()
                    .push(id.clone());
            }
        }
        let mut out: Vec<(Option<String>, Vec<String>)> = groups
            .into_iter()
            .map(|((_, id), v)| (Some(id), v))
            .collect();
        if !rest.is_empty() {
            out.push((None, rest));
        }
        out
    }
}

fn short(t: &Tag) -> &str {
    if t.shorthand.is_empty() {
        &t.name
    } else {
        &t.shorthand
    }
}

fn walk(edges: &HashMap<String, Vec<String>>, from: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from.to_string()];
    while let Some(x) = stack.pop() {
        if seen.insert(x.clone())
            && let Some(next) = edges.get(&x)
        {
            stack.extend(next.iter().cloned());
        }
    }
    seen
}

/// Whether `to` is reachable from `from` going up through parents.
fn reaches(parents: &HashMap<String, Vec<String>>, from: &str, to: &str) -> bool {
    walk(parents, from).contains(to)
}

#[cfg(test)]
mod tests;
