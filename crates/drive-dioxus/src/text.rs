//! Binding a plain text field to a collaborative [`LoroText`]: an edit in
//! the field becomes the smallest delete and insert, so two people typing
//! in the same text merge instead of overwriting each other.

use drive_client::loro::LoroText;

/// Make `text` read `new`, as one delete and one insert around the part
/// that changed. Positions are in Unicode scalar values, as Loro counts.
pub fn set_text(text: &LoroText, new: &str) {
    let old = text.to_string();
    if old == new {
        return;
    }
    let a: Vec<char> = old.chars().collect();
    let b: Vec<char> = new.chars().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let removed = a.len() - prefix - suffix;
    if removed > 0 {
        let _ = text.delete(prefix, removed);
    }
    let inserted: String = b[prefix..b.len() - suffix].iter().collect();
    if !inserted.is_empty() {
        let _ = text.insert(prefix, &inserted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drive_client::loro::LoroDoc;

    #[test]
    fn edits_are_minimal_and_merge() {
        let a = LoroDoc::new();
        let t = a.get_text("t");
        set_text(&t, "hello world");
        a.commit();
        let b = LoroDoc::new();
        b.import(&a.export(drive_client::loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        set_text(&a.get_text("t"), "hello brave world");
        set_text(&b.get_text("t"), "hello world!");
        a.commit();
        b.commit();
        a.import(&b.export(drive_client::loro::ExportMode::Snapshot).unwrap())
            .unwrap();
        assert_eq!(a.get_text("t").to_string(), "hello brave world!");
    }
}
