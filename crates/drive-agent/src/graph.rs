//! `drive graph …`: this peer's graph of the space (docs/graph.md), from
//! the command line, for scripts and agents. The graph is worked out here,
//! from what this device has synced and the notes it can read, into its own
//! `graph.db`; no other machine is asked for it.
//!
//! ```text
//! drive graph backlinks THING          # what links to it, and how
//! drive graph neighbors THING [--depth N]   # everything within N steps (default 1)
//! drive graph path THING THING [--max N]    # a shortest path between them
//! drive graph tag TAG                  # everything under a tag (child tags too)
//! drive graph refresh                  # bring the graph up to date, and say what it read
//! drive graph search WORDS… [--tag TAG] [--limit N]   # notes, files, tags, documents by their words
//! drive graph link FROM TO [--kind K]  # an app's kind of link (uses, built-on), in drive:links
//! drive graph unlink FROM TO [--kind K]
//! ```
//!
//! A THING is what `drive tag` takes (a file here, `drive:PATH`,
//! `LOCATION:PATH`, `project:NAME`, `doc:ID`, a raw key), a graph key
//! (`tag:<id>`, `missing:<name>`, `label:<name>`, `task:<id>`), or a name the
//! graph knows (a note's title or alias, a file's name).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use drive_files::tree;
use drive_graph::db::GraphDb;
use drive_graph::graph::{EdgeKind, Node, NodeKind, tag_node, target_node};
use drive_graph::index::{Hit, Index, MAX_PATH};
use drive_graph::refresh::{Got, Report, Sources, refresh};
use drive_graph::space::TreeItem;
use drive_sites::source::Blobs;
use drive_spine::kinds::Kinds;
use drive_spine::tags::Library;
use loro::{LoroDoc, TreeID};

use crate::agent::Agent;
use crate::tagging::Tags;
use crate::{flag, out};

/// Unreachable notes in a row after which a refresh stops asking for bytes
/// (offline: the rest keep the extracts they had, or none).
const GIVE_UP_AFTER: u32 = 3;

/// Every file and folder of the Drive tree, with paths.
pub fn tree_items(files: &LoroDoc) -> Vec<TreeItem> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<TreeID>, String)> = vec![(None, String::new())];
    while let Some((parent, prefix)) = stack.pop() {
        for e in tree::children(files, parent) {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.dir {
                stack.push((Some(e.id), path.clone()));
            }
            out.push(TreeItem {
                id: tree::id_string(e.id),
                path,
                dir: e.dir,
                hash: e.hash,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The space's `drive:links`, with the other peers' copy when they answer
/// within a few seconds (this device's own, offline).
pub async fn links_doc(a: &Agent) -> (String, LoroDoc) {
    let id = drive_spine::links::links_doc(&a.cfg.lock().expect("cfg").space);
    let doc = a.drive.open(&id).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(4), a.drive.synced(&id)).await;
    (id, doc)
}

/// This peer's graph, brought up to date from its documents (`tags`: the
/// space's `drive:tags`) and the notes it can read.
pub async fn up_to_date(a: &Agent, tags: &LoroDoc) -> Result<(GraphDb, Report), String> {
    let (_, links) = links_doc(a).await;
    let links = drive_spine::links::all(&links);
    let db = GraphDb::open(&a.dir.join("graph.db"))
        .await
        .map_err(|e| e.to_string())?;
    let items = tree_items(&a.files);
    let lib = Library::load(tags);
    let tasks = drive_spine::tasks(&a.spine);
    let events = drive_spine::events(&a.spine);
    let projects = drive_files::projects::projects(&a.space);
    let kinds = Kinds::load(&a.spine);
    let sources = Sources {
        tree: &items,
        tags: &lib,
        tasks: &tasks,
        events: &events,
        projects: &projects,
        kinds: &kinds,
        docs: &[],
        links: &links,
    };
    let misses = AtomicU32::new(0);
    let report = refresh(&db, &sources, |hash| {
        let misses = &misses;
        async move {
            if misses.load(Ordering::Relaxed) >= GIVE_UP_AFTER && !a.is_cached(&hash) {
                return Got::Nothing;
            }
            match Blobs::get(a, &hash).await {
                Ok(bytes) => {
                    misses.store(0, Ordering::Relaxed);
                    Got::Bytes(bytes)
                }
                Err(_) => {
                    misses.fetch_add(1, Ordering::Relaxed);
                    Got::Nothing
                }
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok((db, report))
}

/// A graph key given as it is, if `thing` is one (`project:` is a name in
/// THING syntax, so it is not).
pub fn raw_key(thing: &str) -> Option<&str> {
    let (kind, rest) = thing.split_once(':')?;
    let raw = matches!(
        kind,
        "file" | "content" | "doc" | "tag" | "label" | "missing" | "task" | "event"
    );
    (raw && !rest.is_empty()).then_some(thing)
}

/// The one node a name stands for in the graph.
pub async fn by_name(db: &GraphDb, name: &str) -> Result<Node, String> {
    let found = db.find(name).await.map_err(|e| e.to_string())?;
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("{name}: nothing in the graph by that name")),
        many => Err(format!(
            "{name} could be {}",
            many.iter().map(shown).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The tag node for a tag's name, shorthand, alias or id; a frontmatter
/// tag that names no Drive tag is its label node.
pub fn tag_key(lib: &Library, name: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches('#');
    if let Some(k) = raw_key(name) {
        return Ok(k.to_string());
    }
    if lib.get(name).is_some() {
        return Ok(tag_node(name));
    }
    match lib.resolve(name).as_slice() {
        [one] => Ok(tag_node(one)),
        [] => Ok(format!("label:{}", name.to_lowercase())),
        many => Err(format!(
            "{name} could be {}; give the id",
            many.iter()
                .map(|id| format!("{} ({id})", lib.label(id)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// How a node is named back: the THING syntax where it has one.
pub fn shown(n: &Node) -> String {
    match n.kind {
        NodeKind::File | NodeKind::Folder if !n.path.is_empty() => format!("drive:{}", n.path),
        NodeKind::Doc if n.label != n.id.trim_start_matches("doc:") => {
            format!("{} ({})", n.id, n.label)
        }
        NodeKind::Tag => format!("#{}", n.label),
        NodeKind::Label => format!("#{} (frontmatter only)", n.label),
        NodeKind::Project => format!("project:{}", n.label),
        NodeKind::Missing => format!("[[{}]] (no such page)", n.label),
        NodeKind::Task | NodeKind::Event => format!("{}: {}", n.kind.id(), n.label),
        _ => n.id.clone(),
    }
}

fn hit_line(h: &Hit) -> String {
    let how = if h.edge.raw.is_empty() {
        String::new()
    } else {
        format!("  {}", h.edge.raw)
    };
    format!("  {:<8} {}{how}", h.edge.kind.id(), shown(&h.node))
}

/// A THING (or a graph key, or a name) as a node id, the CLI's way.
async fn node_key(t: &Tags, db: &GraphDb, thing: &str) -> Result<String, String> {
    if let Some(k) = raw_key(thing) {
        return Ok(k.to_string());
    }
    match t.target(thing).await {
        Ok(target) => Ok(target_node(&target)),
        Err(e) => by_name(db, thing)
            .await
            .map(|n| n.id)
            .map_err(|n| format!("{e}; {n}")),
    }
}

fn depth_flag(rest: &[String], name: &str, default: u32) -> Result<u32, String> {
    match flag(rest, name) {
        Some(v) => v
            .parse()
            .map_err(|_| format!("{name} takes a number, not {v}")),
        None => Ok(default),
    }
}

/// The words of `rest` that are not flags or flag values.
fn words(rest: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in rest {
        if skip {
            skip = false;
        } else if a.starts_with("--") {
            skip = true;
        } else {
            out.push(a.as_str());
        }
    }
    out
}

pub async fn command(dir: &Path, args: &[String]) -> Result<(), String> {
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let rest: Vec<String> = args.iter().skip(2).cloned().collect();
    let usage = "graph search WORDS… [--tag TAG] [--limit N] | backlinks THING | neighbors THING [--depth N] | path THING THING [--max N] | tag TAG | refresh | link FROM TO [--kind K] | unlink FROM TO [--kind K]";
    if !matches!(
        sub,
        "search"
            | "backlinks"
            | "neighbors"
            | "neighbours"
            | "path"
            | "tag"
            | "refresh"
            | "link"
            | "unlink"
    ) {
        return Err(usage.into());
    }
    let t = Tags::open(dir).await?;
    if matches!(sub, "link" | "unlink") {
        return link_command(&t, sub == "link", &rest).await;
    }
    let (db, report) = up_to_date(&t.a, &t.doc).await?;
    let w = words(&rest);
    match (sub, w.as_slice()) {
        ("refresh", []) => {
            out(format!(
                "{} notes: {} kept, {} read now, {} from a peer's sidecar, {} out of reach",
                report.notes, report.cached, report.parsed, report.shared, report.unread
            ));
            out(format!("{} nodes, {} edges", report.nodes, report.edges));
        }
        ("search", words) if !words.is_empty() => {
            let limit = depth_flag(&rest, "--limit", 20)? as usize;
            let tag = flag(&rest, "--tag");
            let lib = Library::load(&t.doc);
            let found = search(&db, &lib, &words.join(" "), tag.as_deref(), limit).await?;
            for f in &found {
                out(format!("  {:>6.2}  {}", f.score, shown(&f.node)));
            }
            out(format!("{} found", found.len()));
        }
        ("backlinks", [thing]) => {
            let id = node_key(&t, &db, thing).await?;
            let hits = db.backlinks(&id).await.map_err(|e| e.to_string())?;
            for h in &hits {
                out(hit_line(h));
            }
            out(format!(
                "{} pointing at {}",
                hits.len(),
                shown_id(&db, &id).await
            ));
        }
        ("neighbors" | "neighbours", [thing]) => {
            let depth = depth_flag(&rest, "--depth", 1)?;
            let id = node_key(&t, &db, thing).await?;
            let near = db.neighbors(&id, depth).await.map_err(|e| e.to_string())?;
            for n in &near {
                out(format!("  {}  {}", n.depth, shown(&n.node)));
            }
            out(format!(
                "{} within {depth} of {}",
                near.len(),
                shown_id(&db, &id).await
            ));
        }
        ("path", [from, to]) => {
            let max = depth_flag(&rest, "--max", MAX_PATH)?;
            let (a, b) = (node_key(&t, &db, from).await?, node_key(&t, &db, to).await?);
            match db.path(&a, &b, max).await.map_err(|e| e.to_string())? {
                Some(steps) => {
                    for s in &steps {
                        match &s.edge {
                            None => out(format!("  {}", shown(&s.node))),
                            Some(e) => {
                                let arrow = if e.dst == s.node.id { "→" } else { "←" };
                                out(format!("  {arrow} {:<8} {}", e.kind.id(), shown(&s.node)));
                            }
                        }
                    }
                }
                None => out(format!("no path of at most {max} steps")),
            }
        }
        ("tag", [tag]) => {
            let key = tag_key(&Library::load(&t.doc), tag)?;
            let hits = db.under_tag(&key).await.map_err(|e| e.to_string())?;
            for h in &hits {
                let via = if h.edge.kind == EdgeKind::FmTag {
                    "  (frontmatter)"
                } else {
                    ""
                };
                out(format!("  {}{via}", shown(&h.node)));
            }
            out(format!(
                "{} under {}",
                hits.len(),
                shown_id(&db, &key).await
            ));
        }
        _ => return Err(usage.into()),
    }
    Ok(())
}

/// What a search finds, best first, only things under `tag` when one is
/// given (by a Drive tag or a frontmatter tag, child tags included).
pub async fn search(
    db: &GraphDb,
    lib: &Library,
    query: &str,
    tag: Option<&str>,
    limit: usize,
) -> Result<Vec<drive_graph::index::Found>, String> {
    let err = |e: drive_graph::index::Error| e.to_string();
    let Some(tag) = tag else {
        return db.search(query, limit).await.map_err(err);
    };
    let under: std::collections::HashSet<String> = db
        .under_tag(&tag_key(lib, tag)?)
        .await
        .map_err(err)?
        .into_iter()
        .map(|h| h.node.id)
        .collect();
    // Enough of the best to fill `limit` after the tag narrows them.
    let mut found = db
        .search(query, limit.saturating_mul(20).max(200))
        .await
        .map_err(err)?;
    found.retain(|f| under.contains(&f.node.id));
    found.truncate(limit);
    Ok(found)
}

/// `graph link|unlink FROM TO [--kind K]`.
async fn link_command(t: &Tags, add: bool, rest: &[String]) -> Result<(), String> {
    let [from, to] = words(rest)[..] else {
        return Err("graph link|unlink FROM TO [--kind K]".into());
    };
    let kind = flag(rest, "--kind").unwrap_or_default();
    let (from, to) = (t.target(from).await?, t.target(to).await?);
    let (id, doc) = links_doc(&t.a).await;
    let now = drive_files::now_ms();
    if add {
        drive_spine::links::link(&doc, &from, &to, &kind, &t.by, now)
    } else {
        drive_spine::links::unlink(&doc, &from, &to, &kind)
    }
    .map_err(|e| e.to_string())?;
    t.a.drive.commit(&id);
    crate::settle(&t.a).await;
    for l in drive_spine::links::from(&doc, &from) {
        let kind = if l.kind.is_empty() {
            "link".into()
        } else {
            l.kind
        };
        out(format!("{} --{kind}--> {}", l.from.key(), l.to.key()));
    }
    Ok(())
}

async fn shown_id(db: &GraphDb, id: &str) -> String {
    match db.node(id).await {
        Ok(Some(n)) => shown(&n),
        _ => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_with_folders_and_paths() {
        let d = LoroDoc::new();
        let n = tree::mkdir(&d, None, "Notes").unwrap();
        let f = tree::create_file(&d, Some(n), "A.md").unwrap();
        tree::set_content(&d, f, 3, &"a".repeat(64)).unwrap();
        let items = tree_items(&d);
        let paths: Vec<(&str, bool)> = items.iter().map(|i| (i.path.as_str(), i.dir)).collect();
        assert_eq!(paths, [("Notes", true), ("Notes/A.md", false)]);
        assert_eq!(items[1].hash, "a".repeat(64));
        assert_eq!(items[1].node(), format!("file:{}", tree::id_string(f)));
    }

    #[test]
    fn keys_and_tags() {
        assert_eq!(raw_key("missing:nowhere"), Some("missing:nowhere"));
        assert_eq!(raw_key("project:Easter"), None, "a project is named");
        assert_eq!(raw_key("drive:Notes/A.md"), None);
        assert_eq!(raw_key("tag:"), None);
        let d = LoroDoc::new();
        let id = drive_spine::tags::create(
            &d,
            drive_spine::tags::NewTag {
                name: "Drums",
                ..Default::default()
            },
            "t",
            1,
        )
        .unwrap();
        let lib = Library::load(&d);
        assert_eq!(tag_key(&lib, "#drums"), Ok(tag_node(&id)));
        assert_eq!(tag_key(&lib, "unknown-tag"), Ok("label:unknown-tag".into()));
        assert_eq!(
            words(&["a".into(), "--depth".into(), "2".into(), "b".into()]),
            ["a", "b"]
        );
    }
}
