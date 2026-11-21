//! History: who changed a document, and when. Every Drive client records a
//! timestamp on each change (seconds) and puts its author in the change's
//! message (the signed-in person's email, or the device or guest name);
//! edits a minute apart or closer are one change, so the history reads
//! like a log, not a keystroke stream.
//!
//! Older changes (from before timestamps were recorded) have time 0 and no
//! author.

use loro::{ID, LoroDoc};

/// Edits this close together (seconds) by the same client are one change.
pub const MERGE_SECS: i64 = 60;

/// Get a document ready to record history: timestamps on, a minute's
/// grouping, and its author for the next change.
pub fn prepare(doc: &LoroDoc, author: &str) {
    doc.set_record_timestamp(true);
    doc.set_change_merge_interval(MERGE_SECS);
    doc.set_next_commit_message(author);
}

/// Commit with the author on the change (and on the next one).
pub fn commit(doc: &LoroDoc, author: &str) {
    doc.set_next_commit_message(author);
    doc.commit();
    doc.set_next_commit_message(author);
}

/// One change: when, who, from which client (peer), how many operations.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub id: ID,
    pub lamport: u32,
    /// Seconds since the epoch; 0 when it was not recorded.
    pub when: i64,
    /// The author it was committed with; empty when unknown.
    pub who: String,
    pub ops: usize,
}

/// Every change in the document, oldest first.
pub fn changes(doc: &LoroDoc) -> Vec<Change> {
    let vv = doc.oplog_vv();
    let mut out = Vec::new();
    for (peer, end) in vv.iter() {
        let mut counter = 0;
        while counter < *end {
            let Some(meta) = doc.get_change(ID::new(*peer, counter)) else {
                break;
            };
            out.push(Change {
                id: meta.id,
                lamport: meta.lamport,
                when: meta.timestamp,
                who: meta.message.as_deref().unwrap_or_default().to_string(),
                ops: meta.len,
            });
            counter = meta.id.counter + meta.len as i32;
        }
    }
    out.sort_by_key(|c| (c.when, c.lamport));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_say_who_and_when() {
        let a = LoroDoc::new();
        prepare(&a, "cody@example.com");
        a.get_text("body").insert(0, "Amazing").unwrap();
        commit(&a, "cody@example.com");
        a.get_text("body").insert(7, " Grace").unwrap();
        commit(&a, "cody@example.com");

        // Another device edits; its changes carry its own author.
        let b = LoroDoc::new();
        prepare(&b, "Voyager");
        b.import(&a.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        b.get_text("body").insert(0, "♪ ").unwrap();
        commit(&b, "Voyager");
        a.import(&b.export(loro::ExportMode::Snapshot).unwrap())
            .unwrap();

        let h = changes(&a);
        assert!(
            h.iter().all(|c| c.when > 1_700_000_000),
            "every change has a time: {h:?}"
        );
        let who: Vec<&str> = h.iter().map(|c| c.who.as_str()).collect();
        assert!(
            who.contains(&"cody@example.com") && who.contains(&"Voyager"),
            "{who:?}"
        );
        // Cody's two quick edits are one change.
        assert_eq!(
            h.iter().filter(|c| c.who == "cody@example.com").count(),
            1,
            "{h:?}"
        );
        assert_eq!(a.get_text("body").to_string(), "♪ Amazing Grace");
    }
}
