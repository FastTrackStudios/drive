//! Working copies: a folder of the Drive tree whose files one device also
//! keeps as plain files in a folder on its disk, synced both ways
//! (docs/working-copies.md). Unlike an adoption, the files are the Drive
//! tree's own (every device sees them as ordinary files, the File Provider
//! included); the folder on disk stays where it was, so programs on that
//! device keep editing it in place.
//!
//! The bindings live in the space's registry (`drive:space:<space>`), under
//! `working`: the tree folder's id → the device, the location the folder is
//! in, and the folder inside it (`sub`). Keyed by the tree id, not a path:
//! moving or renaming the folder in the Drive keeps its working copy.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};

/// One working copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingCopy {
    /// The Drive tree folder (its tree id, as `tree::id_string`).
    pub root: String,
    /// The device whose disk has the folder.
    pub device: String,
    /// The location the folder is in (on that device).
    pub location: String,
    /// The folder inside the location, `/`-separated (empty: its root).
    pub sub: String,
    /// When it was made (ms since the epoch).
    pub added: i64,
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
        _ => 0,
    }
}

/// Record a working copy.
pub fn bind(doc: &LoroDoc, w: &WorkingCopy) -> loro::LoroResult<()> {
    let m = doc.get_map("working").ensure_mergeable_map(&w.root)?;
    m.insert("root", w.root.as_str())?;
    m.insert("device", w.device.as_str())?;
    m.insert("location", w.location.as_str())?;
    m.insert("sub", w.sub.as_str())?;
    m.insert("added", w.added)?;
    Ok(())
}

/// Every working copy, by tree folder id.
pub fn working_copies(doc: &LoroDoc) -> Vec<WorkingCopy> {
    let map = doc.get_map("working");
    let mut out: Vec<WorkingCopy> = map
        .keys()
        .filter_map(|k| match map.get(&k) {
            Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
            _ => None,
        })
        .map(|m| WorkingCopy {
            root: s(&m, "root"),
            device: s(&m, "device"),
            location: s(&m, "location"),
            sub: s(&m, "sub"),
            added: n(&m, "added"),
        })
        .filter(|w| !w.root.is_empty() && !w.device.is_empty() && !w.location.is_empty())
        .collect();
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

/// Forget a working copy: the binding only (the tree's files and the
/// folder on disk both stay). `None`: there was none.
pub fn unbind(doc: &LoroDoc, root: &str) -> loro::LoroResult<Option<WorkingCopy>> {
    let Some(w) = working_copies(doc).into_iter().find(|w| w.root == root) else {
        return Ok(None);
    };
    doc.get_map("working").delete(root)?;
    Ok(Some(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wc(root: &str) -> WorkingCopy {
        WorkingCopy {
            root: root.into(),
            device: "ws".into(),
            location: "loc".into(),
            sub: "Fire".into(),
            added: 7,
        }
    }

    #[test]
    fn a_binding_is_kept_listed_and_forgotten_by_its_tree_folder() {
        let doc = LoroDoc::new();
        bind(&doc, &wc("2@1")).unwrap();
        bind(&doc, &wc("1@1")).unwrap();
        assert_eq!(working_copies(&doc), vec![wc("1@1"), wc("2@1")]);
        assert_eq!(unbind(&doc, "1@1").unwrap(), Some(wc("1@1")));
        assert_eq!(unbind(&doc, "1@1").unwrap(), None);
        assert_eq!(working_copies(&doc), vec![wc("2@1")]);
    }

    #[test]
    fn bindings_sync_between_copies_of_the_registry() {
        let a = LoroDoc::new();
        bind(&a, &wc("3@1")).unwrap();
        let b = LoroDoc::new();
        b.import(&a.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        assert_eq!(working_copies(&b), vec![wc("3@1")]);
    }
}
