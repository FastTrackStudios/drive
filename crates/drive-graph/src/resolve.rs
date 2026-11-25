//! What a link names, within one folder of notes (a vault, or a site's
//! source folder): by path, then by file name (the linking note's folder
//! first, else the shortest path), then by title or alias. Obsidian's
//! rules, as the site generator has always applied them.
//!
//! A [`Resolver`] is filled once and then asked many times; it holds any
//! value per target (a site's URL, a graph node id).

use std::collections::HashMap;

use crate::markdown::split_dir;

/// A target and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry<T> {
    pub value: T,
    /// Its source path (to prefer targets in the linking note's folder).
    pub source: String,
}

/// Targets by path, name and title. Keys are lower case.
#[derive(Clone, Debug)]
pub struct Resolver<T> {
    by_path: HashMap<String, Entry<T>>,
    by_name: HashMap<String, Vec<Entry<T>>>,
    by_title: HashMap<String, Entry<T>>,
}

impl<T> Default for Resolver<T> {
    fn default() -> Self {
        Resolver {
            by_path: HashMap::new(),
            by_name: HashMap::new(),
            by_title: HashMap::new(),
        }
    }
}

impl<T: Clone> Resolver<T> {
    #[must_use]
    pub fn new() -> Resolver<T> {
        Resolver::default()
    }

    /// A path (`basics/scales`, Markdown without `.md`) naming `e`; a
    /// later one replaces an earlier.
    pub fn path(&mut self, path: &str, e: Entry<T>) {
        self.by_path.insert(path.to_lowercase(), e);
    }

    /// A path naming `e`, unless the path names something already.
    pub fn path_if_absent(&mut self, path: &str, e: Entry<T>) {
        self.by_path.entry(path.to_lowercase()).or_insert(e);
    }

    /// A file name (`scales`, `circle.png`) that may name `e`, among others.
    pub fn name(&mut self, name: &str, e: Entry<T>) {
        self.by_name.entry(name.to_lowercase()).or_default().push(e);
    }

    /// A name for `e` alone, unless the name names something already.
    pub fn name_if_absent(&mut self, name: &str, e: Entry<T>) {
        self.by_name
            .entry(name.to_lowercase())
            .or_insert_with(|| vec![e]);
    }

    /// A title or alias naming `e`; the first one given wins.
    pub fn title(&mut self, title: &str, e: Entry<T>) {
        self.by_title.entry(title.to_lowercase()).or_insert(e);
    }

    /// What `[[target]]` (or a relative link, already joined to the note's
    /// folder) names, for a note in `from_dir`.
    #[must_use]
    pub fn find(&self, target: &str, from_dir: &str) -> Option<&Entry<T>> {
        let t = target.trim().trim_start_matches('/').to_lowercase();
        let no_md = t.strip_suffix(".md").unwrap_or(&t);
        if let Some(x) = self.by_path.get(no_md) {
            return Some(x);
        }
        let name = no_md.rsplit('/').next().unwrap_or(no_md);
        if let Some(c) = self.by_name.get(name) {
            let here = from_dir.to_lowercase();
            return c
                .iter()
                .find(|x| split_dir(&x.source).0.to_lowercase() == here)
                .or_else(|| c.iter().min_by_key(|x| x.source.len()));
        }
        self.by_title.get(no_md)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(v: &str, source: &str) -> Entry<String> {
        Entry {
            value: v.into(),
            source: source.into(),
        }
    }

    #[test]
    fn path_then_name_then_title() {
        let mut r = Resolver::new();
        r.path("a/scales", e("a", "A/Scales.md"));
        r.name("scales", e("a", "A/Scales.md"));
        r.path("b/scales", e("b", "B/Scales.md"));
        r.name("scales", e("b", "B/Scales.md"));
        r.path("deep/x/scales", e("c", "Deep/X/Scales.md"));
        r.name("scales", e("c", "Deep/X/Scales.md"));
        r.title("modes", e("a", "A/Scales.md"));
        r.title("modes", e("b", "B/Scales.md"));
        assert_eq!(r.find("B/Scales.md", "").unwrap().value, "b");
        assert_eq!(r.find("Scales", "B").unwrap().value, "b", "same folder");
        assert_eq!(r.find("Scales", "Z").unwrap().value, "a", "shortest");
        assert_eq!(r.find("Modes", "").unwrap().value, "a", "first title");
        assert!(r.find("nothing", "").is_none());
    }
}
