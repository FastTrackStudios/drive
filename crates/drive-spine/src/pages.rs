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

use loro::{LoroDoc, LoroText, UpdateOptions};

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
}
