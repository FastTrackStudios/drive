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
//! drive graph search WORDS… [--tag TAG] [--near THING] [--limit N]
//!                                      # by words and meaning, fused; near a thing first
//! drive graph model fetch              # the embedding model (once per device)
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
use drive_graph::embed::{Embedder, MODEL_FILES, StaticModel};
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

/// Every file and folder of the Drive tree, with paths, and the page id
/// of each that is a page (empty for the others).
pub fn tree_items(files: &LoroDoc) -> Vec<(TreeItem, String)> {
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
            out.push((
                TreeItem {
                    id: tree::id_string(e.id),
                    path,
                    dir: e.dir,
                    hash: e.hash,
                },
                e.page,
            ));
        }
    }
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    out
}

/// A page's content key: its text's hash (a page has no blob), so its
/// extract is kept and found again as any note's.
pub fn page_key(text: &str) -> String {
    format!("page:{}", blake3::hash(text.as_bytes()).to_hex())
}

/// The tree, each page given its text's key, and the pages' texts by key.
async fn tree_and_pages(a: &Agent) -> (Vec<TreeItem>, std::collections::HashMap<String, String>) {
    let space = a.cfg.lock().expect("cfg").space.clone();
    let mut texts = std::collections::HashMap::new();
    let mut items = Vec::new();
    for (mut item, page) in tree_items(&a.files) {
        if !page.is_empty() {
            let doc = a
                .drive
                .open(&drive_spine::pages::page_doc(&space, &page))
                .await;
            let text = drive_spine::pages::text(&doc);
            item.hash = page_key(&text);
            texts.insert(item.hash.clone(), text);
        }
        items.push(item);
    }
    (items, texts)
}

/// The embedding model every peer runs (docs/graph.md): Model2Vec's
/// potion-base-8M, where it is fetched from, and the id its vectors are
/// kept under.
pub const MODEL_ID: &str = "potion-base-8M@1";
const MODEL_URL: &str = "https://huggingface.co/minishlab/potion-base-8M/resolve/main";

/// Where this device keeps the model: `$DRIVE_EMBED_MODEL`, else
/// `models/potion-base-8M` in the agent's folder.
pub fn model_dir(agent_dir: &Path) -> std::path::PathBuf {
    std::env::var_os("DRIVE_EMBED_MODEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| agent_dir.join("models").join("potion-base-8M"))
}

/// The model, when this device has it (none: search is by words only).
pub fn model(agent_dir: &Path) -> Option<&'static StaticModel> {
    static MODEL: std::sync::OnceLock<Option<StaticModel>> = std::sync::OnceLock::new();
    MODEL
        .get_or_init(|| StaticModel::load(MODEL_ID, &model_dir(agent_dir)).ok())
        .as_ref()
}

/// Download the model's files (once; `drive graph model fetch`).
async fn fetch_model(a: &Agent) -> Result<std::path::PathBuf, String> {
    let dir = model_dir(&a.dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for f in MODEL_FILES {
        let to = dir.join(f);
        if to.exists() {
            continue;
        }
        let resp = a
            .http
            .get(format!("{MODEL_URL}/{f}"))
            .send()
            .await
            .map_err(|e| format!("{f}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{f}: {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("{f}: {e}"))?;
        let part = dir.join(format!("{f}.part"));
        std::fs::write(&part, &bytes).map_err(|e| format!("{}: {e}", part.display()))?;
        std::fs::rename(&part, &to).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    StaticModel::load(MODEL_ID, &dir)?;
    Ok(dir)
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
    let (items, pages) = tree_and_pages(a).await;
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
        let page = pages.get(&hash).cloned();
        async move {
            if let Some(text) = page {
                return Got::Bytes(text.into_bytes());
            }
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
    if let Some(m) = model(&a.dir) {
        drive_graph::search::embed_missing(&db, m)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok((db, report))
}

/// Whether a change to document `id` (of `space`) changes the graph.
pub fn feeds_graph(space: &str, id: &str) -> bool {
    let Some(rest) = id.strip_prefix(space).and_then(|r| r.strip_prefix('/')) else {
        return false;
    };
    matches!(
        rest,
        "drive:files" | "drive:space" | "drive:spine" | "drive:tags" | "drive:links"
    ) || rest.starts_with(drive_spine::pages::PAGE_PREFIX)
}

/// How long the documents must be quiet before the graph is made again.
const QUIET: std::time::Duration = std::time::Duration::from_secs(2);

/// Keep this peer's graph up to date while the service runs: made again a
/// moment after the documents it is built from change (here or on another
/// device), so a page's new link or a new tag is in it within seconds.
pub async fn keep_up(a: std::sync::Arc<Agent>) {
    let space = a.cfg.lock().expect("cfg").space.clone();
    let mut events = a.drive.events();
    let tags_id = drive_spine::tags::tags_doc(&space);
    let mut dirty = true;
    loop {
        let next = if dirty {
            tokio::time::timeout(QUIET, events.recv()).await.ok()
        } else {
            Some(events.recv().await)
        };
        match next {
            Some(Ok(drive_native::Event::Changed(id))) if feeds_graph(&space, &id) => dirty = true,
            Some(Ok(_)) => {}
            // Missed some: make it again to be sure.
            Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => dirty = true,
            Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
            // Quiet for a while after a change: now.
            None => {
                let tags = a.drive.open(&tags_id).await;
                if let Err(e) = up_to_date(&a, &tags).await {
                    tracing::warn!(error = %e, "the graph was not brought up to date");
                }
                dirty = false;
            }
        }
    }
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
    let usage = "graph search WORDS… [--tag TAG] [--near THING] [--limit N] | model fetch | backlinks THING | neighbors THING [--depth N] | path THING THING [--max N] | tag TAG | refresh | link FROM TO [--kind K] | unlink FROM TO [--kind K]";
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
            let near = match flag(&rest, "--near") {
                Some(n) => Some(node_key(&t, &db, &n).await?),
                None => None,
            };
            let lib = Library::load(&t.doc);
            let m = model(&t.a.dir).map(|m| m as &dyn Embedder);
            let q = Query {
                words: &words.join(" "),
                tag: tag.as_deref(),
                near: near.as_deref(),
                limit,
            };
            let found = search(&db, m, &lib, &q).await?;
            for f in &found {
                out(format!("  {:>6.3}  {}", f.score, shown(&f.node)));
            }
            out(format!(
                "{} found{}",
                found.len(),
                if m.is_none() {
                    " (by words: `drive graph model fetch` adds meaning)"
                } else {
                    ""
                }
            ));
        }
        ("model", ["fetch"]) => {
            let dir = fetch_model(&t.a).await?;
            out(format!("{MODEL_ID} in {}", dir.display()));
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

/// What a search finds, best first: words and meaning (when this device
/// has the model) fused, things near `near` moved up, and only things
/// under `tag` when one is given (child tags included).
pub async fn search(
    db: &GraphDb,
    model: Option<&dyn Embedder>,
    lib: &Library,
    q: &Query<'_>,
) -> Result<Vec<drive_graph::index::Found>, String> {
    let err = |e: drive_graph::index::Error| e.to_string();
    let under: Option<std::collections::HashSet<String>> = match q.tag {
        Some(tag) => Some(
            db.under_tag(&tag_key(lib, tag)?)
                .await
                .map_err(err)?
                .into_iter()
                .map(|h| h.node.id)
                .collect(),
        ),
        None => None,
    };
    // Enough of the best to fill `limit` after a tag narrows them.
    let wide = if under.is_some() {
        q.limit.saturating_mul(20).max(200)
    } else {
        q.limit
    };
    let ask = drive_graph::search::Ask {
        query: q.words,
        model,
        near: q.near,
        limit: wide,
    };
    let mut found = drive_graph::search::hybrid(db, &ask).await.map_err(err)?;
    if let Some(under) = under {
        found.retain(|f| under.contains(&f.node.id));
    }
    found.truncate(q.limit);
    Ok(found)
}

/// A search, as asked.
pub struct Query<'a> {
    pub words: &'a str,
    pub tag: Option<&'a str>,
    /// A node id to rank things near it higher.
    pub near: Option<&'a str>,
    pub limit: usize,
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
        let p = tree::create_page(&d, Some(n), "Live.md", "pg1").unwrap();
        let items = tree_items(&d);
        let paths: Vec<(&str, bool, &str)> = items
            .iter()
            .map(|(i, page)| (i.path.as_str(), i.dir, page.as_str()))
            .collect();
        assert_eq!(
            paths,
            [
                ("Notes", true, ""),
                ("Notes/A.md", false, ""),
                ("Notes/Live.md", false, "pg1")
            ]
        );
        assert_eq!(items[1].0.hash, "a".repeat(64));
        assert_eq!(items[1].0.node(), format!("file:{}", tree::id_string(f)));
        assert_eq!(items[2].0.node(), format!("file:{}", tree::id_string(p)));
    }

    #[test]
    fn what_feeds_the_graph() {
        assert!(feeds_graph("home", "home/drive:tags"));
        assert!(feeds_graph("home", "home/drive:page:abc"));
        assert!(feeds_graph("home", "home/drive:files"));
        assert!(!feeds_graph("home", "home/drive:rules"));
        assert!(!feeds_graph("home", "other/drive:tags"));
        assert!(!feeds_graph("home", "homeX/drive:tags"));
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
