//! A hook's value made for its argument, made again when the argument
//! changes. `use_hook` alone keeps the first one forever: a component
//! whose document id changes between renders (a song's owner known only
//! after the first render, a route reusing the component) would go on
//! reading and editing the first document, its edits shown here and never
//! where they belong (drive#53).

use std::cell::RefCell;

/// The value for the last key asked for.
pub(crate) struct Keyed<K, T>(RefCell<Option<(K, T)>>);

impl<K, T> Default for Keyed<K, T> {
    fn default() -> Self {
        Keyed(RefCell::new(None))
    }
}

impl<K: PartialEq + Clone, T: Clone> Keyed<K, T> {
    /// The value kept for `key`, or a new one from `make` when the key is
    /// not the one it was made for.
    pub(crate) fn get(&self, key: &K, make: impl FnOnce() -> T) -> T {
        if let Some((k, v)) = self.0.borrow().as_ref()
            && k == key
        {
            return v.clone();
        }
        let v = make();
        *self.0.borrow_mut() = Some((key.clone(), v.clone()));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_new_key_makes_a_new_value_and_the_same_key_keeps_it() {
        let made = Cell::new(0);
        let slot = Keyed::default();
        let make = |id: &str| {
            made.set(made.get() + 1);
            format!("doc {id}")
        };
        assert_eq!(slot.get(&"me/studio:files:a", || make("a")), "doc a");
        assert_eq!(slot.get(&"me/studio:files:a", || make("a")), "doc a");
        assert_eq!(made.get(), 1, "the same id keeps its document");
        // The owner became known: the hook now names another document.
        assert_eq!(slot.get(&"church/studio:files:a", || make("b")), "doc b");
        assert_eq!(made.get(), 2, "a new id opens the new document");
    }
}
