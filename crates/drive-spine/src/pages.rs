//! Pages: Markdown notes that live as Loro text, so several people and
//! agents edit one note at once (Google Docs' way), offline included, and
//! every device merges the same.
//!
//! A page is one document, `<org>/drive:page:<id>`, its text in `body`
//! (the whole note as Markdown, frontmatter included, so it reads exactly
//! as an Obsidian note). It sits in a vault at a path of the Drive tree: a
//! tree file whose `page` names it (`drive_files::tree::create_page`), so
//! wikilinks, folders, tags and the graph treat it as any note.
//!
//! [`write`] takes a whole new text and applies only what changed (a text
//! diff), so an agent that rewrites one line does not overwrite what
//! someone else is typing elsewhere in the page.

use loro::{ExportMode, Frontiers, LoroDoc, LoroText, UpdateOptions};

/// A page document's id within an org, before the page's own id.
pub const PAGE_PREFIX: &str = "drive:page:";

/// A page's document id in an org.
pub fn page_doc(org: &str, id: &str) -> String {
    format!("{org}/{PAGE_PREFIX}{id}")
}

fn body(doc: &LoroDoc) -> LoroText {
    doc.get_text("body")
}

/// The page's text.
pub fn text(doc: &LoroDoc) -> String {
    body(doc).to_string()
}

/// Make the page's text `new`, changing only what differs (so concurrent
/// edits elsewhere in it survive).
///
/// # Errors
///
/// The diff could not be applied.
pub fn write(doc: &LoroDoc, new: &str) -> Result<(), String> {
    body(doc)
        .update(new, UpdateOptions::default())
        .map_err(|e| e.to_string())
}

/// The page's version now, to merge against later ([`merge_since`]).
pub fn version(doc: &LoroDoc) -> Vec<u8> {
    doc.state_frontiers().encode()
}

/// The text `text`, written somewhere that started from the page as it was
/// at `version` (a file saved on disk since the last sync), merged into the
/// page as it is now: both the page's edits since then and `text`'s stay
/// (a three-way merge, by the CRDT itself: the edit is made on a fork at
/// that version and imported).
///
/// # Errors
///
/// A version this page does not have, or the merge failed.
pub fn merge_since(doc: &LoroDoc, version: &[u8], text: &str) -> Result<(), String> {
    let base = Frontiers::decode(version).map_err(|e| e.to_string())?;
    let since = doc
        .frontiers_to_vv(&base)
        .ok_or("the page does not have that version")?;
    let fork = doc.fork_at(&base).map_err(|e| e.to_string())?;
    // Its own peer: the fork's edit must not reuse this document's ids.
    fork.set_peer_id(LoroDoc::new().peer_id())
        .map_err(|e| e.to_string())?;
    write(&fork, text)?;
    fork.commit();
    let edit = fork
        .export(ExportMode::updates(&since))
        .map_err(|e| e.to_string())?;
    doc.import(&edit).map_err(|e| e.to_string())?;
    Ok(())
}

/// A new page id: time-ordered and unique across devices.
pub fn new_id(doc: &LoroDoc, now: i64) -> String {
    format!("{now:x}-{:x}", doc.peer_id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::ExportMode;

    #[test]
    fn two_writers_at_once_both_land() {
        let a = LoroDoc::new();
        write(&a, "# Scales\n\nMajor.\n\nMinor.\n").unwrap();
        a.commit();
        let b = LoroDoc::new();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        // An agent rewrites the whole page with one line changed; a person
        // types at the end of it at the same time.
        write(&a, "# Scales\n\nMajor, Ionian.\n\nMinor.\n").unwrap();
        write(&b, "# Scales\n\nMajor.\n\nMinor.\n\nModes next.\n").unwrap();
        a.commit();
        b.commit();
        a.import(&b.export(ExportMode::Snapshot).unwrap()).unwrap();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        assert_eq!(text(&a), text(&b));
        assert_eq!(
            text(&a),
            "# Scales\n\nMajor, Ionian.\n\nMinor.\n\nModes next.\n"
        );
        assert!(page_doc("home", "x").ends_with("/drive:page:x"));
    }

    #[test]
    fn a_file_edited_since_a_version_merges_with_the_pages_edits() {
        let d = LoroDoc::new();
        write(&d, "# Ideas\n\nA key change.\n").unwrap();
        d.commit();
        let synced = version(&d);
        // Live, in the app: a line added at the end.
        write(&d, "# Ideas\n\nA key change.\n\nFrom the app.\n").unwrap();
        d.commit();
        // Meanwhile on disk (Obsidian), from the synced text: a word changed.
        merge_since(&d, &synced, "# Ideas\n\nA key change, up a tone.\n").unwrap();
        assert_eq!(
            text(&d),
            "# Ideas\n\nA key change, up a tone.\n\nFrom the app.\n"
        );
        assert!(merge_since(&d, b"nonsense", "x").is_err());
    }
}
