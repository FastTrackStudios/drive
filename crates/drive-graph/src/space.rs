//! What a link names across the whole space, not just one vault.
//!
//! A `[[Name]]` in a note is looked for, in order:
//!
//! 1. in the note's own folder, then the rest of its **vault** (the nearest
//!    folder above it holding `.obsidian/`; without one, the whole Drive),
//!    by path, file name, title and alias ([`Resolver`]'s rules);
//! 2. in every **other vault** (by path from that vault's top), then by
//!    the full Drive path (`[[Music/Basics/Scales]]`);
//! 3. among the space's **app documents**, by id (`[[studio:song:x]]`), by
//!    a kind's prefix and the name's slug (`[[Amazing Grace]]` →
//!    `studio:song:amazing-grace`, `drive_spine::kinds`), or by title.
//!
//! A relative Markdown link (`[x](../Other.md)`) is a path: it names the
//! file at that path, or nothing. What names nothing is kept as a
//! *missing* node, so "what links to a page that does not exist yet" can
//! be asked.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use drive_spine::kinds::Kinds;

use crate::extract::{Extract, Link};
use crate::markdown::{is_markdown, normalize, slug, split_dir, stem};
use crate::resolve::{Entry, Resolver};

/// One file or folder of the Drive tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeItem {
    /// Its tree id (`drive_files::tree::id_string`).
    pub id: String,
    /// Its path from the Drive's top (`Music/Basics/Scales.md`).
    pub path: String,
    pub dir: bool,
    /// Its content's full BLAKE3 hash; empty for a folder or a file not
    /// uploaded yet.
    pub hash: String,
}

impl TreeItem {
    /// Its graph node id (the tags' key for it: `file:<tree id>`).
    #[must_use]
    pub fn node(&self) -> String {
        file_node(&self.id)
    }

    /// Hidden itself or inside a hidden folder (`.obsidian/`, `.trash/`).
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.path.split('/').any(|p| p.starts_with('.'))
    }
}

/// The node id of a file or folder of the tree.
#[must_use]
pub fn file_node(tree_id: &str) -> String {
    format!("file:{tree_id}")
}

/// The node id of an app document.
#[must_use]
pub fn doc_node(doc_id: &str) -> String {
    format!("doc:{doc_id}")
}

/// The node id of a link that names nothing (lower case, so every
/// `[[Nowhere]]` and `[[nowhere]]` is one node).
#[must_use]
pub fn missing_node(target: &str) -> String {
    format!("missing:{}", target.trim().to_lowercase())
}

/// An app document the space knows of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocRef {
    /// Its id within the org (`studio:song:amazing-grace`).
    pub id: String,
    /// What it is called, if known.
    pub title: String,
}

/// What a link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A node that exists (`file:…`, `doc:…`).
    Node(String),
    /// Nothing: [`missing_node`] of what was written.
    Missing(String),
}

impl Resolved {
    #[must_use]
    pub fn node(&self) -> &str {
        match self {
            Resolved::Node(n) | Resolved::Missing(n) => n,
        }
    }
}

/// One vault's resolver (paths relative to its top).
struct Vault {
    root: String,
    names: Resolver<String>,
}

impl Vault {
    /// The path inside this vault, if `path` is in it.
    fn inside<'a>(&self, path: &'a str) -> Option<&'a str> {
        if self.root.is_empty() {
            return Some(path);
        }
        path.strip_prefix(&self.root)?.strip_prefix('/')
    }
}

/// Everything links in a space can name, ready to be asked.
pub struct Space {
    vaults: Vec<Vault>,
    /// The vault (its root) each item belongs to, by path.
    by_path: HashMap<String, String>,
    /// Lower-case full path → node.
    paths: HashMap<String, String>,
    docs: Docs,
}

/// The folders that are vaults: those holding `.obsidian/` (or any file
/// inside it), plus the Drive's top for notes in none.
fn vault_roots(items: &[TreeItem]) -> Vec<String> {
    let mut roots: BTreeSet<String> = BTreeSet::new();
    roots.insert(String::new());
    for i in items {
        let parts: Vec<&str> = i.path.split('/').collect();
        if let Some(at) = parts.iter().position(|p| *p == ".obsidian") {
            roots.insert(parts[..at].join("/"));
        }
    }
    roots.into_iter().collect()
}

/// The deepest of `roots` holding `path`.
fn vault_of<'a>(roots: &'a [String], path: &str) -> &'a str {
    roots
        .iter()
        .filter(|r| r.is_empty() || path.starts_with(&format!("{r}/")))
        .max_by_key(|r| r.len())
        .map_or("", String::as_str)
}

impl Space {
    /// The space's tree, what its notes say (by content hash), the app
    /// documents it knows and its kinds.
    #[must_use]
    pub fn new(
        items: &[TreeItem],
        extracts: &HashMap<String, Extract>,
        docs: &[DocRef],
        kinds: &Kinds,
    ) -> Space {
        let roots = vault_roots(items);
        let mut vaults: Vec<Vault> = roots
            .iter()
            .map(|r| Vault {
                root: r.clone(),
                names: Resolver::new(),
            })
            .collect();
        let visible: Vec<&TreeItem> = items.iter().filter(|i| !i.is_hidden()).collect();
        for v in &mut vaults {
            for i in &visible {
                // Each item in its own vault, and the top vault keeps every
                // item by its full path.
                let own = vault_of(&roots, &i.path) == v.root;
                if (own || v.root.is_empty())
                    && let Some(rel) = v.inside(&i.path)
                {
                    add_item(&mut v.names, rel, i, extracts.get(&i.hash), own);
                }
            }
        }
        let by_path = visible
            .iter()
            .map(|i| (i.path.clone(), vault_of(&roots, &i.path).to_string()))
            .collect();
        let paths = visible
            .iter()
            .map(|i| (i.path.to_lowercase(), i.node()))
            .collect();
        Space {
            vaults,
            by_path,
            paths,
            docs: Docs::new(docs, kinds),
        }
    }

    /// What `link`, in the note at `from` (a Drive path), names.
    #[must_use]
    pub fn resolve(&self, from: &str, link: &Link) -> Resolved {
        let (dir, _) = split_dir(from);
        let found = if link.form.is_relative() {
            let joined = normalize(&format!("{dir}/{}", link.target));
            self.at_path(&joined)
                .or_else(|| self.by_name(from, &joined))
        } else {
            self.by_name(from, &link.target)
                .or_else(|| self.docs.find(&link.target))
        };
        match found {
            Some(n) => Resolved::Node(n),
            None => Resolved::Missing(missing_node(&link.target)),
        }
    }

    /// The item at a full Drive path (any case; a note also without `.md`).
    fn at_path(&self, path: &str) -> Option<String> {
        let lower = path.to_lowercase();
        self.paths
            .get(&lower)
            .or_else(|| self.paths.get(&format!("{lower}.md")))
            .cloned()
    }

    /// The note's folder, its own vault, then the others (each also by full Drive path, in
    /// the top vault). A link with a folder in it (`[[Wiki/Scales]]`) is
    /// first looked for as a path in every vault, so a note of the same
    /// name nearer by does not take it.
    fn by_name(&self, from: &str, target: &str) -> Option<String> {
        let own_root = self.by_path.get(from).map_or("", String::as_str);
        let own = self.vaults.iter().find(|v| v.root == own_root)?;
        let rel_dir = split_dir(own.inside(from).unwrap_or(from)).0;
        let order =
            || std::iter::once(own).chain(self.vaults.iter().filter(|v| v.root != own_root));
        let by_path = || {
            target
                .contains('/')
                .then(|| order().find_map(|v| v.names.find_path(target)))
                .flatten()
        };
        let here = || {
            (!rel_dir.is_empty())
                .then(|| own.names.find_path(&format!("{rel_dir}/{target}")))
                .flatten()
        };
        here()
            .or_else(by_path)
            .or_else(|| own.names.find(target, rel_dir))
            .or_else(|| order().skip(1).find_map(|v| v.names.find(target, "")))
            .map(|e| e.value.clone())
    }
}

/// One item in a vault's resolver: its path, its name, and for a note its
/// title and aliases. `own`: the item's own vault (the top vault also
/// keeps other vaults' items, by path only).
fn add_item(r: &mut Resolver<String>, rel: &str, i: &TreeItem, e: Option<&Extract>, own: bool) {
    let entry = Entry {
        value: i.node(),
        source: rel.to_string(),
    };
    let (_, file) = split_dir(rel);
    if i.dir {
        r.path_if_absent(rel, entry.clone());
        if own {
            r.name_if_absent(file, entry);
        }
        return;
    }
    if !is_markdown(rel) {
        r.path(rel, entry.clone());
        if own {
            r.name(file, entry);
        }
        return;
    }
    let lower = rel.to_lowercase();
    r.path(lower.strip_suffix(".md").unwrap_or(&lower), entry.clone());
    if !own {
        return;
    }
    r.name(stem(file), entry.clone());
    let Some(e) = e else { return };
    if let Some(t) = &e.title {
        r.title(t, entry.clone());
    }
    for a in &e.aliases {
        r.title(a, entry.clone());
    }
}

/// The app documents a link may name.
struct Docs {
    /// Lower-case id → id.
    ids: HashMap<String, String>,
    /// Lower-case title → id (the first by id).
    titles: BTreeMap<String, String>,
    prefixes: Vec<String>,
}

impl Docs {
    fn new(docs: &[DocRef], kinds: &Kinds) -> Docs {
        let mut sorted: Vec<&DocRef> = docs.iter().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        let mut titles = BTreeMap::new();
        for d in &sorted {
            if !d.title.is_empty() {
                titles
                    .entry(d.title.to_lowercase())
                    .or_insert_with(|| d.id.clone());
            }
        }
        let mut prefixes: Vec<String> = kinds
            .all()
            .iter()
            .filter(|k| !k.doc_prefix.is_empty())
            .map(|k| k.doc_prefix.clone())
            .collect();
        prefixes.sort();
        Docs {
            ids: sorted
                .iter()
                .map(|d| (d.id.to_lowercase(), d.id.clone()))
                .collect(),
            titles,
            prefixes,
        }
    }

    fn find(&self, target: &str) -> Option<String> {
        let t = target.trim();
        let t = t.strip_prefix("doc:").unwrap_or(t);
        let lower = t.to_lowercase();
        let known = |id: &str| self.ids.get(&id.to_lowercase()).cloned();
        known(t)
            .or_else(|| {
                // `[[studio:song:x]]`: an app's document by id, known or not.
                self.prefixes
                    .iter()
                    .any(|p| lower.starts_with(p.as_str()) && lower.len() > p.len())
                    .then(|| t.to_string())
            })
            .or_else(|| {
                let s = slug(t);
                self.prefixes.iter().find_map(|p| known(&format!("{p}{s}")))
            })
            .or_else(|| self.titles.get(&lower).cloned())
            .map(|id| doc_node(&id))
    }
}

#[cfg(test)]
mod tests;
