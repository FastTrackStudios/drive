//! How deep a document's `Doc::edit` calls are nested, so only the
//! outermost commits: an `edit` of a document inside that same document's
//! `edit` would otherwise commit the outer one's changes halfway, before
//! the rest of its closure ran (drive#62). Editing *another* document
//! inside is unaffected: each commits its own.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static DEPTH: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
}

/// Run `f` as an edit of `id`; whether it was the outermost edit of `id`
/// (the one that should commit). The depth unwinds even if `f` panics.
pub(crate) fn edit<R>(id: &str, f: impl FnOnce() -> R) -> (R, bool) {
    struct Leave<'a>(&'a str);
    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            DEPTH.with(|d| {
                let mut d = d.borrow_mut();
                if let Some(n) = d.get_mut(self.0) {
                    *n -= 1;
                    if *n == 0 {
                        d.remove(self.0);
                    }
                }
            });
        }
    }
    let outermost = DEPTH.with(|d| {
        let mut d = d.borrow_mut();
        let n = d.entry(id.to_string()).or_insert(0);
        *n += 1;
        *n == 1
    });
    let _leave = Leave(id);
    (f(), outermost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_outermost_edit_of_a_document_commits() {
        let ((inner, other), outer) = edit("a", || {
            let (_, inner) = edit("a", || {});
            let (_, other) = edit("b", || {});
            (inner, other)
        });
        assert!(outer, "the outer edit commits");
        assert!(!inner, "an edit of the same document inside it does not");
        assert!(other, "another document's edit inside it commits its own");
        assert!(edit("a", || {}).1, "afterwards, an edit is outermost again");
    }

    #[test]
    fn a_panicking_edit_does_not_leave_the_document_nested() {
        let r = std::panic::catch_unwind(|| edit("p", || panic!("an edit failed")));
        assert!(r.is_err());
        assert!(edit("p", || {}).1);
    }
}
