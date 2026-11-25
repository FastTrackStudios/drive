//! The graph itself: nodes and edges, worked out from what the space
//! already syncs. Nobody edits it; every peer builds its own ([`build`])
//! and throws it away whenever the inputs change.
//!
//! | Edge | From → to | Comes from |
//! |---|---|---|
//! | `link`, `embed` | a note → what it names | the note's text ([`Extract`]) |
//! | `related` | a note → what its frontmatter names | the note's text |
//! | `fm-tag` | a note → a tag its frontmatter names | the note's text |
//! | `tagged` | a thing → a tag | `<org>/drive:tags` |
//! | `parent` | a tag → its parent tag | `<org>/drive:tags` |
//! | `task`, `event` | a task or event → the document it links | the spine |
//! | `part` | a project → its documents and Drive folders | the space's registry |
//! | `linked` | a thing → a thing an app or agent linked it to | `<org>/drive:links` |
//! | `in` | a file or folder → the folder holding it | the Drive tree |

use std::collections::{BTreeMap, HashMap, HashSet};

use drive_files::projects::{Project, TREE};
use drive_spine::kinds::Kinds;
use drive_spine::links::Link as AppLink;
use drive_spine::tags::{Library, Target};
use drive_spine::{Event, Task};
use serde::{Deserialize, Serialize};

use crate::extract::{Extract, Form, Link};
use crate::markdown::{is_markdown, split_dir};
use crate::space::{DocRef, Resolved, Space, TreeItem, doc_node, file_node};

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A file of the Drive tree (`file:<tree id>`).
    File,
    /// A folder of the Drive tree (`file:<tree id>`).
    Folder,
    /// A file on some computer, by content (`content:<sampled hash>`).
    Content,
    /// An app document (`doc:<id>`).
    Doc,
    /// A project (`project:<id>`).
    Project,
    /// A Drive tag (`tag:<id>`).
    Tag,
    /// A frontmatter tag that names no Drive tag (`label:<name>`).
    Label,
    /// A task of the spine (`task:<id>`).
    Task,
    /// An event of the spine (`event:<id>`).
    Event,
    /// What a link names that does not exist (`missing:<name>`).
    Missing,
}

impl NodeKind {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Folder => "folder",
            NodeKind::Content => "content",
            NodeKind::Doc => "doc",
            NodeKind::Project => "project",
            NodeKind::Tag => "tag",
            NodeKind::Label => "label",
            NodeKind::Task => "task",
            NodeKind::Event => "event",
            NodeKind::Missing => "missing",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<NodeKind> {
        Some(match s {
            "file" => NodeKind::File,
            "folder" => NodeKind::Folder,
            "content" => NodeKind::Content,
            "doc" => NodeKind::Doc,
            "project" => NodeKind::Project,
            "tag" => NodeKind::Tag,
            "label" => NodeKind::Label,
            "task" => NodeKind::Task,
            "event" => NodeKind::Event,
            "missing" => NodeKind::Missing,
            _ => return None,
        })
    }

    /// The kind a node id's prefix says.
    fn of_id(id: &str) -> NodeKind {
        id.split_once(':')
            .and_then(|(k, _)| NodeKind::parse(k))
            .unwrap_or(NodeKind::Missing)
    }
}

/// What an edge says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeKind {
    Link,
    Embed,
    Related,
    FmTag,
    Tagged,
    Parent,
    Task,
    Event,
    Part,
    /// An app's or agent's link (`drive:links`); `raw` holds its kind.
    Linked,
    /// Inside a folder.
    In,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 11] = [
        EdgeKind::Link,
        EdgeKind::Embed,
        EdgeKind::Related,
        EdgeKind::FmTag,
        EdgeKind::Tagged,
        EdgeKind::Parent,
        EdgeKind::Task,
        EdgeKind::Event,
        EdgeKind::Part,
        EdgeKind::Linked,
        EdgeKind::In,
    ];

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            EdgeKind::Link => "link",
            EdgeKind::Embed => "embed",
            EdgeKind::Related => "related",
            EdgeKind::FmTag => "fm-tag",
            EdgeKind::Tagged => "tagged",
            EdgeKind::Parent => "parent",
            EdgeKind::Task => "task",
            EdgeKind::Event => "event",
            EdgeKind::Part => "part",
            EdgeKind::Linked => "linked",
            EdgeKind::In => "in",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<EdgeKind> {
        EdgeKind::ALL.into_iter().find(|k| k.id() == s)
    }

    /// An edge that puts a thing under a tag.
    #[must_use]
    pub fn is_tagging(self) -> bool {
        matches!(self, EdgeKind::Tagged | EdgeKind::FmTag)
    }
}

/// A node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// `file:<tree id>`, `tag:<id>`, `doc:<id>`, … (the tags' keys where
    /// there is one).
    pub id: String,
    pub kind: NodeKind,
    /// What it is called (a file's name, a tag's name, a title).
    pub label: String,
    /// A file's Drive path; empty for others.
    pub path: String,
}

/// An edge, from `src` to `dst`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
    /// What was written (`[[Scales|scales]]`), for links; empty otherwise.
    pub raw: String,
    /// The heading or block a link points at; empty for none.
    pub anchor: String,
}

/// The whole graph of a space.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Graph {
    pub nodes: BTreeMap<String, Node>,
    pub edges: Vec<Edge>,
    /// Lower-case names a node is found by (a note's title and aliases, a
    /// tag's name, shorthand and aliases, a file's name) → node ids.
    pub aliases: Vec<(String, String)>,
    /// What each node is searched by: its name, path and other names, and
    /// a note's text.
    pub texts: BTreeMap<String, String>,
}

/// What a graph is built from: everything a peer has in its synced
/// documents, plus the notes it could read.
pub struct Inputs<'a> {
    /// The Drive tree, every file and folder.
    pub tree: &'a [TreeItem],
    /// What each note says, by content hash. A note missing here (its
    /// bytes are on a computer this peer cannot reach, and nobody shared
    /// its sidecar) has no outgoing edges; links to it still arrive.
    pub extracts: &'a HashMap<String, Extract>,
    /// `<org>/drive:tags`.
    pub tags: &'a Library,
    pub tasks: &'a [Task],
    pub events: &'a [Event],
    pub projects: &'a [Project],
    pub kinds: &'a Kinds,
    /// App documents known some other way (a listing, with titles).
    pub docs: &'a [DocRef],
    /// `<org>/drive:links`.
    pub links: &'a [AppLink],
}

/// Work out the graph.
#[must_use]
pub fn build(i: &Inputs) -> Graph {
    let mut g = Graph::default();
    add_tree(&mut g, i.tree, i.extracts);
    add_projects(&mut g, i.projects, i.tree);
    add_tags(&mut g, i.tags);
    add_spine(&mut g, i.tasks, i.events, i.kinds);
    add_links(&mut g, i.links);
    let docs = known_docs(&g, i.docs);
    let space = Space::new(i.tree, i.extracts, &docs, i.kinds);
    add_notes(&mut g, i, &space);
    add_docs(&mut g, &docs);
    g.ensure_endpoints();
    g.edges.sort();
    g.edges.dedup();
    g.aliases.sort();
    g.aliases.dedup();
    g.texts = texts(&g, i);
    g
}

/// Each node's searchable text: its label and path, the names it is found
/// by, and for a note its text.
fn texts(g: &Graph, i: &Inputs) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, Vec<String>> = g
        .nodes
        .values()
        .map(|n| (n.id.clone(), vec![n.label.clone(), n.path.clone()]))
        .collect();
    for (name, id) in &g.aliases {
        if let Some(v) = out.get_mut(id) {
            v.push(name.clone());
        }
    }
    for t in i.tree.iter().filter(|t| !t.dir) {
        if let (Some(e), Some(v)) = (i.extracts.get(&t.hash), out.get_mut(&t.node())) {
            v.extend(e.tags.iter().cloned());
            v.push(e.text.clone());
        }
    }
    out.into_iter()
        .map(|(id, mut parts)| {
            parts.retain(|p| !p.trim().is_empty());
            parts.dedup();
            (id, parts.join("\n"))
        })
        .collect()
}

impl Graph {
    fn node(&mut self, id: String, kind: NodeKind, label: &str, path: &str) {
        self.nodes.entry(id.clone()).or_insert(Node {
            id,
            kind,
            label: label.to_string(),
            path: path.to_string(),
        });
    }

    fn edge(&mut self, src: &str, dst: &str, kind: EdgeKind) {
        self.edges.push(Edge {
            src: src.to_string(),
            dst: dst.to_string(),
            kind,
            raw: String::new(),
            anchor: String::new(),
        });
    }

    fn alias(&mut self, name: &str, id: &str) {
        if !name.trim().is_empty() {
            self.aliases
                .push((name.trim().to_lowercase(), id.to_string()));
        }
    }

    /// Every edge's ends are nodes (a tagged file no longer in the tree,
    /// a document known only by a link).
    fn ensure_endpoints(&mut self) {
        let ends: Vec<String> = self
            .edges
            .iter()
            .flat_map(|e| [e.src.clone(), e.dst.clone()])
            .filter(|id| !self.nodes.contains_key(id))
            .collect();
        for id in ends {
            let kind = NodeKind::of_id(&id);
            let label = id
                .split_once(':')
                .map_or(id.as_str(), |(_, r)| r)
                .to_string();
            self.node(id, kind, &label, "");
        }
    }
}

fn add_tree(g: &mut Graph, tree: &[TreeItem], extracts: &HashMap<String, Extract>) {
    let parents: HashMap<&str, String> = tree
        .iter()
        .filter(|t| t.dir)
        .map(|t| (t.path.as_str(), t.node()))
        .collect();
    for t in tree.iter().filter(|t| !t.is_hidden()) {
        let name = split_dir(&t.path).1;
        let kind = if t.dir {
            NodeKind::Folder
        } else {
            NodeKind::File
        };
        let label = extracts
            .get(&t.hash)
            .filter(|_| is_markdown(&t.path))
            .and_then(|e| e.title.clone())
            .unwrap_or_else(|| name.to_string());
        g.node(t.node(), kind, &label, &t.path);
        if let Some(parent) = parents.get(split_dir(&t.path).0) {
            g.edge(&t.node(), parent, EdgeKind::In);
        }
        g.alias(name, &t.node());
        if let Some(stem) = name.strip_suffix(".md") {
            g.alias(stem, &t.node());
        }
        if let Some(e) = extracts.get(&t.hash).filter(|_| is_markdown(&t.path)) {
            for a in e.title.iter().chain(&e.aliases) {
                g.alias(a, &t.node());
            }
        }
    }
}

fn add_links(g: &mut Graph, links: &[AppLink]) {
    for l in links {
        g.edges.push(Edge {
            src: target_node(&l.from),
            dst: target_node(&l.to),
            kind: EdgeKind::Linked,
            raw: l.kind.clone(),
            anchor: String::new(),
        });
    }
}

fn add_projects(g: &mut Graph, projects: &[Project], tree: &[TreeItem]) {
    for p in projects {
        let id = format!("project:{}", p.id);
        g.node(id.clone(), NodeKind::Project, &p.name, "");
        g.alias(&p.name, &id);
        for d in &p.docs {
            g.edge(&id, &doc_node(d), EdgeKind::Part);
        }
        for part in p
            .parts
            .iter()
            .filter(|x| x.location == TREE && x.space.is_empty())
        {
            let path = part.path.trim_matches('/');
            if let Some(t) = tree.iter().find(|t| t.path == path) {
                g.edge(&id, &t.node(), EdgeKind::Part);
            }
        }
    }
}

fn add_tags(g: &mut Graph, lib: &Library) {
    for t in lib.all() {
        let id = tag_node(&t.id);
        g.node(id.clone(), NodeKind::Tag, &lib.label(&t.id), "");
        for name in t.names() {
            g.alias(name, &id);
        }
        // The library's hierarchy, with cycles already dropped.
        for c in lib.children(&t.id) {
            g.edge(&tag_node(&c.id), &id, EdgeKind::Parent);
        }
    }
    for (target, on) in lib.targets() {
        let thing = target_node(&target);
        for a in &on.tags {
            g.edge(&thing, &tag_node(&a.tag), EdgeKind::Tagged);
        }
    }
}

fn add_spine(g: &mut Graph, tasks: &[Task], events: &[Event], kinds: &Kinds) {
    for t in tasks {
        let id = format!("task:{}", t.id);
        g.node(id.clone(), NodeKind::Task, &t.title, "");
        if let Some(doc) = doc_of_link(kinds, &t.link) {
            g.edge(&id, &doc_node(&doc), EdgeKind::Task);
        }
    }
    for e in events.iter().filter(|e| !e.is_generated()) {
        let id = format!("event:{}", e.id);
        g.node(id.clone(), NodeKind::Event, &e.title, "");
        if let Some(doc) = doc_of_link(kinds, &e.link) {
            g.edge(&id, &doc_node(&doc), EdgeKind::Event);
        }
    }
}

/// The app document a spine link (`/studio/song/amazing-grace`) opens, by
/// the kinds' `open` paths (`/studio/song/{id}` with prefix `studio:song:`).
#[must_use]
pub fn doc_of_link(kinds: &Kinds, link: &str) -> Option<String> {
    let link = link.split(['?', '#']).next().unwrap_or(link);
    kinds
        .all()
        .iter()
        .filter(|k| !k.doc_prefix.is_empty())
        .find_map(|k| {
            let (before, after) = k.open.split_once("{id}")?;
            let rest = link.strip_prefix(before)?.strip_suffix(after)?;
            (!rest.is_empty() && !rest.contains('/')).then(|| format!("{}{rest}", k.doc_prefix))
        })
}

/// App documents named anywhere so far, and those given (with titles).
fn known_docs(g: &Graph, given: &[DocRef]) -> Vec<DocRef> {
    let mut seen: HashSet<&str> = given.iter().map(|d| d.id.as_str()).collect();
    let mut out = given.to_vec();
    for e in &g.edges {
        for end in [&e.src, &e.dst] {
            if let Some(id) = end.strip_prefix("doc:")
                && seen.insert(id)
            {
                out.push(DocRef {
                    id: id.to_string(),
                    title: String::new(),
                });
            }
        }
    }
    out
}

fn add_docs(g: &mut Graph, docs: &[DocRef]) {
    for d in docs {
        let label = if d.title.is_empty() { &d.id } else { &d.title };
        g.node(doc_node(&d.id), NodeKind::Doc, label, "");
        if !d.title.is_empty() {
            g.alias(&d.title, &doc_node(&d.id));
        }
    }
}

fn add_notes(g: &mut Graph, i: &Inputs, space: &Space) {
    let notes = i
        .tree
        .iter()
        .filter(|t| !t.dir && !t.is_hidden() && is_markdown(&t.path));
    for t in notes {
        let Some(e) = i.extracts.get(&t.hash) else {
            continue;
        };
        for l in &e.links {
            let to = space.resolve(&t.path, l);
            if let Resolved::Missing(id) = &to {
                g.node(id.clone(), NodeKind::Missing, &l.target, "");
            }
            g.edges.push(link_edge(&t.node(), l, to.node()));
        }
        for name in &e.tags {
            let to = frontmatter_tag(i.tags, name);
            if to.starts_with("label:") {
                g.node(to.clone(), NodeKind::Label, name, "");
            }
            g.edge(&t.node(), &to, EdgeKind::FmTag);
        }
    }
}

fn link_edge(src: &str, l: &Link, dst: &str) -> Edge {
    Edge {
        src: src.to_string(),
        dst: dst.to_string(),
        kind: match l.form {
            Form::Frontmatter => EdgeKind::Related,
            f if f.is_embed() => EdgeKind::Embed,
            _ => EdgeKind::Link,
        },
        raw: l.raw.clone(),
        anchor: l.anchor.clone().unwrap_or_default(),
    }
}

/// The Drive tag a frontmatter tag names (by name, shorthand or alias;
/// `music/theory` by its last part too), else a label node of its own.
fn frontmatter_tag(lib: &Library, name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    [name, last]
        .into_iter()
        .find_map(|n| match lib.resolve(n).as_slice() {
            [one] => Some(tag_node(one)),
            _ => None,
        })
        .unwrap_or_else(|| format!("label:{}", name.to_lowercase()))
}

/// A tag's node id.
#[must_use]
pub fn tag_node(id: &str) -> String {
    format!("tag:{id}")
}

/// The node of a thing tags are put on (the same key the tags use).
#[must_use]
pub fn target_node(t: &Target) -> String {
    match t {
        Target::File(id) => file_node(id),
        other => other.key(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
