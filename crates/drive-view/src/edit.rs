//! Quick edits to a PDF's pages, done natively (on the node, or a
//! computer's agent): rotate, delete, move. pdfcraft-organize does the
//! work; the result is a whole new file (a full save), so it is a new blob.
//!
//! The browser asks for them with [`PageEdit`]s as JSON:
//! `POST /pdf/edit/{hash}` on the node, body `[{"op":"rotate","pages":[0],"degrees":90}]`,
//! answer `{"hash": …, "size": …}`. The app then points the file at the new
//! hash, as an upload does.

use std::sync::Arc;

use pdfcraft_cos::{Document, SaveOptions, write_full};
pub use crate::PageEdit;

/// The largest number of edits one request may make.
pub const MAX_EDITS: usize = 256;

/// A PDF with `edits` made, in order: its new bytes.
pub fn apply(bytes: Vec<u8>, edits: &[PageEdit]) -> Result<Vec<u8>, String> {
    if edits.is_empty() {
        return Err("no edits".into());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!("at most {MAX_EDITS} edits at once"));
    }
    let mut doc = Document::open(Arc::new(bytes)).map_err(|e| e.to_string())?;
    for e in edits {
        let r = match e {
            PageEdit::Rotate { pages, degrees } => pdfcraft_organize::rotate_pages(&mut doc, pages, *degrees),
            PageEdit::Delete { pages } => pdfcraft_organize::delete_pages(&mut doc, pages),
            PageEdit::Move { pages, to } => pdfcraft_organize::move_pages(&mut doc, pages, *to),
        };
        r.map_err(|e| e.to_string())?;
    }
    write_full(&doc, &SaveOptions::default()).map_err(|e| e.to_string())
}

/// How many pages a PDF has.
pub fn page_count(bytes: Vec<u8>) -> Result<usize, String> {
    let doc = Document::open(Arc::new(bytes)).map_err(|e| e.to_string())?;
    pdfcraft_organize::page_count(&doc).map_err(|e| e.to_string())
}

#[cfg(all(test, feature = "pdf"))]
mod tests {
    use super::*;
    use crate::pdf::{PdfDoc, tests::hello};

    #[test]
    fn rotates_moves_and_deletes_pages() {
        let edits: Vec<PageEdit> = serde_json::from_str(
            r#"[{"op":"rotate","pages":[0],"degrees":90},{"op":"move","pages":[1],"to":0}]"#,
        )
        .unwrap();
        let out = apply(hello(), &edits).expect("edits");
        let d = PdfDoc::open(out.clone()).expect("still a PDF");
        // The small page first; the letter page turned on its side.
        assert_eq!(d.size(0), (300.0, 400.0));
        assert_eq!(d.size(1), (792.0, 612.0));

        let out = apply(out, &[PageEdit::Delete { pages: vec![0] }]).unwrap();
        assert_eq!(page_count(out.clone()).unwrap(), 1);
        assert!(apply(out, &[PageEdit::Delete { pages: vec![0] }]).is_err(), "the last page stays");
        assert!(apply(hello(), &[PageEdit::Rotate { pages: vec![0], degrees: 45 }]).is_err());
        assert!(apply(hello(), &[PageEdit::Delete { pages: vec![9] }]).is_err());
    }
}
