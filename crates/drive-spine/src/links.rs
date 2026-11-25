//! Links between things: an app's way to say "this refers to that".
//!
//! An episode uses a song, a preset is built on another preset, a chart
//! belongs to a setlist. One document per org, `<org>/drive:links`, keyed
//! by what things are (the same keys tags use, [`Target`]), so a link
//! follows a file through moves and renames and a content through every
//! copy. Every app and agent writes the same shape, and the graph
//! (docs/graph.md) reads it.
//!
//! Layout: `from`: source key → { `<kind>\t<target key>` → { to, kind,
//! by, at } }. One (source, kind, target) is one entry, so two devices
//! adding the same link at once leave one link, and removing it removes it
//! everywhere.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

use crate::tags::Target;

/// The links document's id within an org.
pub const LINKS: &str = "drive:links";

/// The links document's full id in an org.
pub fn links_doc(org: &str) -> String {
    format!("{org}/{LINKS}")
}

/// One link.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link {
    pub from: Target,
    pub to: Target,
    /// What the link says, in the app's words (`uses`, `built-on`,
    /// `in-setlist`); empty: a plain reference.
    pub kind: String,
    /// Who made it (the signed-in person, a device, `agent: …`).
    pub by: String,
    /// When (ms since the epoch).
    pub at: i64,
}

/// Why a link was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// A thing linking itself.
    ToItself(String),
    /// A kind with a tab or newline in it.
    BadKind(String),
    Loro(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::ToItself(k) => write!(f, "{k} cannot link itself"),
            LinkError::BadKind(k) => write!(f, "a link's kind is one line of text, not {k:?}"),
            LinkError::Loro(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<loro::LoroError> for LinkError {
    fn from(e: loro::LoroError) -> Self {
        LinkError::Loro(e.to_string())
    }
}

fn from_map(doc: &LoroDoc) -> LoroMap {
    doc.get_map("from")
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
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

fn entry_key(kind: &str, to: &Target) -> String {
    format!("{kind}\t{}", to.key())
}

/// Link `from` to `to` (`kind`: what the link says). Linking again
/// updates who and when.
///
/// # Errors
///
/// A thing linking itself, a kind of more than one line, or a write failed.
pub fn link(
    doc: &LoroDoc,
    from: &Target,
    to: &Target,
    kind: &str,
    by: &str,
    now: i64,
) -> Result<(), LinkError> {
    if from == to {
        return Err(LinkError::ToItself(from.key()));
    }
    let kind = kind.trim();
    if kind.contains(['\t', '\n', '\r']) {
        return Err(LinkError::BadKind(kind.into()));
    }
    let m = from_map(doc)
        .ensure_mergeable_map(&from.key())?
        .ensure_mergeable_map(&entry_key(kind, to))?;
    m.insert("to", to.key().as_str())?;
    m.insert("kind", kind)?;
    m.insert("by", by)?;
    m.insert("at", now)?;
    Ok(())
}

/// Remove the link of `kind` from `from` to `to` (no such link: nothing).
///
/// # Errors
///
/// The write failed.
pub fn unlink(doc: &LoroDoc, from: &Target, to: &Target, kind: &str) -> Result<(), LinkError> {
    if let Some(m) = child(&from_map(doc), &from.key()) {
        let k = entry_key(kind.trim(), to);
        if m.get(&k).is_some() {
            m.delete(&k)?;
        }
    }
    Ok(())
}

/// The links out of one thing, by target key then kind.
pub fn from(doc: &LoroDoc, thing: &Target) -> Vec<Link> {
    let mut out = child(&from_map(doc), &thing.key())
        .map(|m| read(thing, &m))
        .unwrap_or_default();
    out.sort();
    out
}

/// Every link, in a fixed order.
pub fn all(doc: &LoroDoc) -> Vec<Link> {
    let map = from_map(doc);
    let mut out: Vec<Link> = map
        .keys()
        .filter_map(|k| Some((Target::parse(&k)?, child(&map, &k)?)))
        .flat_map(|(t, m)| read(&t, &m))
        .collect();
    out.sort();
    out
}

fn read(from: &Target, m: &LoroMap) -> Vec<Link> {
    m.keys()
        .filter_map(|k| {
            let e = child(m, &k)?;
            Some(Link {
                from: from.clone(),
                to: Target::parse(&s(&e, "to"))?,
                kind: s(&e, "kind"),
                by: s(&e, "by"),
                at: n(&e, "at"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::ExportMode;

    fn song() -> Target {
        Target::Doc("studio:song:amazing-grace".into())
    }

    fn episode() -> Target {
        Target::Doc("season:episode:1".into())
    }

    #[test]
    fn links_are_kept_by_what_things_are() {
        let d = LoroDoc::new();
        link(&d, &episode(), &song(), "uses", "a", 1).unwrap();
        link(&d, &episode(), &Target::File("3@1".into()), "", "a", 2).unwrap();
        link(&d, &episode(), &song(), "uses", "b", 3).unwrap(); // again: one link
        let out = from(&d, &episode());
        assert_eq!(out.len(), 2);
        let uses = out.iter().find(|l| l.kind == "uses").unwrap();
        assert_eq!((uses.by.as_str(), uses.at), ("b", 3));
        assert_eq!(all(&d).len(), 2);
        unlink(&d, &episode(), &song(), "uses").unwrap();
        assert_eq!(from(&d, &episode()).len(), 1);
        unlink(&d, &episode(), &song(), "uses").unwrap(); // gone already: fine
        assert_eq!(
            link(&d, &song(), &song(), "", "a", 1),
            Err(LinkError::ToItself(song().key()))
        );
        assert!(matches!(
            link(&d, &song(), &episode(), "a\tb", "a", 1),
            Err(LinkError::BadKind(_))
        ));
    }

    #[test]
    fn two_devices_linking_at_once_merge() {
        let (a, b) = (LoroDoc::new(), LoroDoc::new());
        link(&a, &episode(), &song(), "uses", "a", 1).unwrap();
        link(&b, &episode(), &song(), "uses", "b", 2).unwrap();
        link(&b, &song(), &Target::Project("p".into()), "", "b", 2).unwrap();
        a.commit();
        b.commit();
        a.import(&b.export(ExportMode::Snapshot).unwrap()).unwrap();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        assert_eq!(all(&a), all(&b));
        assert_eq!(all(&a).len(), 2, "the same link from both is one");
    }
}
