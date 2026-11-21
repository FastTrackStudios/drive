//! Schema versions: every app document says which shape it is in, so an app
//! that changes its shape can bring old documents up to date the moment it
//! opens them, once, on whichever device gets there first.
//!
//! The version lives in the document's `meta` map as `version` (an integer;
//! a document that never had one is version 0). An app lists its upgrade
//! steps in order: step `i` takes a document from version `i` to `i + 1`.
//!
//! ```ignore
//! const STEPS: &[schema::Step] = &[add_pantry, split_prep_steps];
//! doc.edit(|d| schema::upgrade(d, STEPS));
//! ```
//!
//! Steps must be safe to run on two devices at once (they usually are:
//! both write the same values, and Loro keeps one). A document newer than
//! the app is left alone: [`upgrade`] says so, and the app should show it
//! read-only or ask to be updated rather than write an old shape into it.

use loro::{LoroDoc, LoroValue, ValueOrContainer};

/// One upgrade: from its index's version to the next.
pub type Step = fn(&LoroDoc);

/// The document's schema version (0: never set).
pub fn version(doc: &LoroDoc) -> i64 {
    match doc.get_map("meta").get("version") {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

/// What [`upgrade`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upgraded {
    /// Already current.
    Current,
    /// Brought from one version to another.
    From(i64),
    /// Written by a newer app (its version is ahead of this app's steps).
    Newer(i64),
}

/// Bring the document up to `steps.len()`, running each missing step.
pub fn upgrade(doc: &LoroDoc, steps: &[Step]) -> Upgraded {
    let target = steps.len() as i64;
    let at = version(doc);
    if at == target {
        return Upgraded::Current;
    }
    if at > target {
        return Upgraded::Newer(at);
    }
    for step in &steps[at.max(0) as usize..] {
        step(doc);
    }
    let _ = doc.get_map("meta").insert("version", target);
    Upgraded::From(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_title(d: &LoroDoc) {
        let _ = d.get_map("song").insert("title", "Untitled");
    }

    fn add_key(d: &LoroDoc) {
        let _ = d.get_map("song").insert("key", "C");
    }

    #[test]
    fn upgrades_run_once_in_order() {
        let d = LoroDoc::new();
        assert_eq!(upgrade(&d, &[add_title]), Upgraded::From(0));
        assert_eq!(version(&d), 1);
        let _ = d.get_map("song").insert("title", "Amazing Grace");
        // A newer app adds a step: only that one runs.
        assert_eq!(upgrade(&d, &[add_title, add_key]), Upgraded::From(1));
        assert_eq!(
            d.get_map("song")
                .get("title")
                .and_then(|v| v.into_value().ok()),
            Some("Amazing Grace".into())
        );
        assert_eq!(upgrade(&d, &[add_title, add_key]), Upgraded::Current);
        // The old app leaves the newer document alone.
        assert_eq!(upgrade(&d, &[add_title]), Upgraded::Newer(2));
    }
}
