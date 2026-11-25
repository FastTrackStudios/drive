//! Notes' frontmatter tags as Drive tags, when the owner asks for it.
//!
//! A note's `tags: [theory, music/drums]` are its own words; Drive's tags
//! (`<org>/drive:tags`) are what every app and search shares. [`plan`]
//! says, without writing anything, which Drive tag each frontmatter tag
//! would be (an existing one, or one to make, nested as Obsidian nests:
//! `music/drums` is Drums under Music) and which notes it would go on. The
//! caller shows that, and writes only when told to: the tags document is
//! synced, so this is never done on its own.

use std::collections::HashMap;

use drive_spine::tags::{Library, Target};

use crate::extract::Extract;
use crate::markdown::is_markdown;
use crate::space::TreeItem;

/// One frontmatter tag of one note, as a Drive tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    /// The note (`file:<tree id>` as a tags target).
    pub note: Target,
    pub path: String,
    /// The tag as written, split at `/` (`["music", "drums"]`).
    pub tag: Vec<String>,
    /// The Drive tag it already is, if any.
    pub have: Option<String>,
    /// The note carries that tag already.
    pub already: bool,
    /// Several Drive tags have that name and nothing tells them apart:
    /// left alone (making one more would only add to the confusion).
    pub ambiguous: bool,
}

/// What adopting the space's frontmatter tags would do.
#[must_use]
pub fn plan(tree: &[TreeItem], extracts: &HashMap<String, Extract>, lib: &Library) -> Vec<Wanted> {
    let notes = tree
        .iter()
        .filter(|t| !t.dir && !t.is_hidden() && is_markdown(&t.path));
    let mut out = Vec::new();
    for t in notes {
        let Some(e) = extracts.get(&t.hash) else {
            continue;
        };
        let note = Target::File(t.id.clone());
        let on = lib.on(&note).tag_ids();
        for written in &e.tags {
            let tag: Vec<String> = written
                .split('/')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if tag.is_empty() {
                continue;
            }
            let have = existing(lib, &tag);
            let ambiguous = have.is_none() && tag.last().is_some_and(|l| lib.resolve(l).len() > 1);
            out.push(Wanted {
                ambiguous,
                already: have.as_ref().is_some_and(|h| on.contains(h)),
                note: note.clone(),
                path: t.path.clone(),
                tag,
                have,
            });
        }
    }
    out
}

/// The Drive tag a nested tag names: its last part by name, shorthand or
/// alias; of several, the one whose parent is named by the part before.
#[must_use]
pub fn existing(lib: &Library, tag: &[String]) -> Option<String> {
    let leaf = tag.last()?;
    let found = lib.resolve(leaf);
    match found.as_slice() {
        [] => None,
        [one] if tag.len() == 1 => Some(one.clone()),
        many => {
            let parent = tag.len().checked_sub(2).map(|i| tag[i].to_lowercase());
            many.iter()
                .find(|id| match &parent {
                    Some(p) => lib.get(id).is_some_and(|t| {
                        t.parents.iter().any(|pid| {
                            lib.get(pid)
                                .is_some_and(|pt| pt.names().any(|n| n.to_lowercase() == *p))
                        })
                    }),
                    None => false,
                })
                .cloned()
                .or_else(|| (many.len() == 1).then(|| many[0].clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::tests::tree;
    use drive_spine::tags::{self, NewTag};
    use loro::LoroDoc;

    #[test]
    fn frontmatter_tags_map_to_drive_tags_or_ones_to_make() {
        let (items, extracts) = tree(&[
            ("A.md", "---\ntags: [theory, music/drums, new/thing]\n---\n"),
            ("B.md", "---\ntags: drums\n---\n"),
            ("C.md", "no frontmatter"),
        ]);
        let d = LoroDoc::new();
        let new = |name, parents: &[String]| {
            tags::create(
                &d,
                NewTag {
                    name,
                    parents,
                    ..NewTag::default()
                },
                "t",
                1,
            )
            .unwrap()
        };
        let music = new("Music", &[]);
        let drums = new("Drums", std::slice::from_ref(&music));
        let kit = new("Kit", &[]);
        let _other_drums = new("Drums", std::slice::from_ref(&kit));
        let theory = new("Theory", &[]);
        tags::tag(&d, &Target::File(items[0].id.clone()), &theory, "t", 1).unwrap();
        let lib = Library::load(&d);
        let p = plan(&items, &extracts, &lib);
        let row = |path: &str, tag: &str| {
            p.iter()
                .find(|w| w.path == path && w.tag.join("/") == tag)
                .unwrap_or_else(|| panic!("{path} {tag}"))
        };
        assert_eq!(row("A.md", "theory").have.as_deref(), Some(theory.as_str()));
        assert!(row("A.md", "theory").already);
        assert_eq!(
            row("A.md", "music/drums").have.as_deref(),
            Some(drums.as_str()),
            "of two Drums, the one under Music"
        );
        assert_eq!(row("A.md", "new/thing").have, None, "to make");
        assert_eq!(
            row("B.md", "drums").have,
            None,
            "two Drums and no hint: not guessed"
        );
        assert!(row("B.md", "drums").ambiguous);
        assert!(!row("A.md", "new/thing").ambiguous);
        assert_eq!(p.len(), 4);
    }
}
