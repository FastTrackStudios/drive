//! Drive's file rules (Hazel and Forel's idea, on Drive): a rule says
//! which devices and places it looks at, which files it takes and what it
//! does to them. The rules are one synced document per org
//! ([`doc::RULES`]), so every device's agent runs the same rules over its
//! own files and the node runs them over the Drive tree.
//!
//! This crate is pure (no file system, no network) and builds for the
//! browser: the rules document ([`doc`]), the journal ([`journal`]), the
//! evaluator ([`eval`]: a file's facts and a rule in, a plan out),
//! templates, names, loop protection ([`guard`]) and the Drive tree's own
//! actions ([`tree`], Loro edits only). The app's dry run and the agents'
//! runs decide with the same code.
//!
//! **Safety** (docs/rules.md): nothing here deletes or overwrites. A new
//! rule is off and a dry run; moves of a computer's files are blocked
//! unless that device allows them; Trash is only offered for the Drive
//! tree (where it is a folder, never a deletion); copies take a free name.

pub mod doc;
pub mod eval;
pub mod guard;
pub mod journal;
pub mod model;
pub mod names;
pub mod template;
pub mod tree;

pub use eval::{
    DeviceInfo, Effect, Facts, Item, LocInfo, Matcher, Plan, Runner, Spot, Step, World,
};
pub use model::*;

use loro::LoroDoc;

/// A new id, time-ordered and unique across devices.
pub(crate) fn new_id(doc: &LoroDoc, prefix: &str, now: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let peer = doc.peer_id();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{now:x}-{peer:x}-{n:x}")
}

#[cfg(test)]
mod tests;
