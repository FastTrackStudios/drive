//! Asking the graph: backlinks, neighbours, a shortest path, everything
//! under a tag. [`Index`] is what any peer hosts: [`MemoryIndex`] holds a
//! [`Graph`] as it is (a browser, a test), and `GraphDb` (the `turso`
//! feature) keeps it in a peer's local database. Both answer the same.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;

use crate::graph::{Edge, EdgeKind, Graph, Node};

/// Why a question could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A node and the edge that reached it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub node: Node,
    pub edge: Edge,
}

/// A node some steps away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Near {
    pub node: Node,
    pub depth: u32,
}

/// One step of a path: the node, and the edge taken to it (none for the
/// first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub node: Node,
    pub edge: Option<Edge>,
}

/// The longest path [`Index::path`] looks for unless told.
pub const MAX_PATH: u32 = 8;

/// A peer's graph, asked. Only [`Index::node`], [`Index::find`] and
/// [`Index::touching`] must be given; the rest follow from them (a
/// database answers them in one query instead).
pub trait Index {
    /// A node by id.
    fn node(&self, id: &str) -> impl Future<Output = Result<Option<Node>>>;

    /// Nodes found by a name (a title, alias, file name, tag name; any case).
    fn find(&self, name: &str) -> impl Future<Output = Result<Vec<Node>>>;

    /// Every edge into or out of a node.
    fn touching(&self, id: &str) -> impl Future<Output = Result<Vec<Edge>>>;

    /// What points at a node: each source and its edge.
    fn backlinks(&self, id: &str) -> impl Future<Output = Result<Vec<Hit>>> {
        async move {
            let edges: Vec<Edge> = self
                .touching(id)
                .await?
                .into_iter()
                .filter(|e| e.dst == id)
                .collect();
            hits(self, edges, |e| &e.src).await
        }
    }

    /// What a node points at: each target and its edge.
    fn outgoing(&self, id: &str) -> impl Future<Output = Result<Vec<Hit>>> {
        async move {
            let edges: Vec<Edge> = self
                .touching(id)
                .await?
                .into_iter()
                .filter(|e| e.src == id)
                .collect();
            hits(self, edges, |e| &e.dst).await
        }
    }

    /// Every node within `depth` edges of `id` (either way), nearest
    /// first, without `id` itself.
    fn neighbors(&self, id: &str, depth: u32) -> impl Future<Output = Result<Vec<Near>>> {
        async move {
            let mut seen: HashMap<String, u32> = HashMap::from([(id.to_string(), 0)]);
            let mut frontier = vec![id.to_string()];
            for d in 1..=depth {
                let mut next = Vec::new();
                for n in &frontier {
                    for e in self.touching(n).await? {
                        let other = other_end(&e, n);
                        if !seen.contains_key(other) {
                            seen.insert(other.to_string(), d);
                            next.push(other.to_string());
                        }
                    }
                }
                frontier = next;
            }
            let mut out = Vec::new();
            for (n, d) in seen.into_iter().filter(|(_, d)| *d > 0) {
                if let Some(node) = self.node(&n).await? {
                    out.push(Near { node, depth: d });
                }
            }
            out.sort_by(|a, b| (a.depth, &a.node.id).cmp(&(b.depth, &b.node.id)));
            Ok(out)
        }
    }

    /// A shortest path from `from` to `to` (edges taken either way), at
    /// most `max` edges long; none when there is no such path.
    fn path(
        &self,
        from: &str,
        to: &str,
        max: u32,
    ) -> impl Future<Output = Result<Option<Vec<Step>>>> {
        async move {
            let mut came: HashMap<String, Option<Edge>> = HashMap::from([(from.to_string(), None)]);
            let mut queue = VecDeque::from([(from.to_string(), 0u32)]);
            while let Some((n, d)) = queue.pop_front() {
                if n == to {
                    return steps(self, &came, to).await.map(Some);
                }
                if d == max {
                    continue;
                }
                let mut edges = self.touching(&n).await?;
                edges.sort();
                for e in edges {
                    let other = other_end(&e, &n).to_string();
                    if !came.contains_key(&other) {
                        came.insert(other.clone(), Some(e));
                        queue.push_back((other, d + 1));
                    }
                }
            }
            Ok(None)
        }
    }

    /// Everything carrying a tag or any tag under it (Drive tags and
    /// frontmatter tags), each with the edge that puts it there.
    fn under_tag(&self, tag: &str) -> impl Future<Output = Result<Vec<Hit>>> {
        async move {
            let mut tags = HashSet::from([tag.to_string()]);
            let mut todo = vec![tag.to_string()];
            let mut tagging = Vec::new();
            while let Some(t) = todo.pop() {
                for e in self.touching(&t).await? {
                    if e.dst != t {
                        continue;
                    }
                    if e.kind == EdgeKind::Parent && tags.insert(e.src.clone()) {
                        todo.push(e.src.clone());
                    } else if e.kind.is_tagging() {
                        tagging.push(e);
                    }
                }
            }
            tagging.sort();
            tagging.dedup_by(|a, b| a.src == b.src);
            hits(self, tagging, |e| &e.src).await
        }
    }
}

/// Edges with the node at the end `end` picks, by label.
async fn hits<I: Index + ?Sized>(
    ix: &I,
    edges: Vec<Edge>,
    end: impl Fn(&Edge) -> &String,
) -> Result<Vec<Hit>> {
    let mut out = Vec::new();
    for edge in edges {
        if let Some(node) = ix.node(end(&edge)).await? {
            out.push(Hit { node, edge });
        }
    }
    out.sort_by(|a, b| (&a.node.label, &a.node.id).cmp(&(&b.node.label, &b.node.id)));
    Ok(out)
}

/// The path to `to` that `came` (each node's edge from the previous)
/// records.
async fn steps<I: Index + ?Sized>(
    ix: &I,
    came: &HashMap<String, Option<Edge>>,
    to: &str,
) -> Result<Vec<Step>> {
    let mut rev: Vec<(String, Option<Edge>)> = Vec::new();
    let mut at = to.to_string();
    loop {
        let edge = came.get(&at).cloned().flatten();
        let prev = edge.as_ref().map(|e| other_end(e, &at).to_string());
        rev.push((at, edge));
        match prev {
            Some(p) => at = p,
            None => break,
        }
    }
    let mut out = Vec::new();
    for (id, edge) in rev.into_iter().rev() {
        let node = ix
            .node(&id)
            .await?
            .ok_or_else(|| Error(format!("{id} is on the path but not in the graph")))?;
        out.push(Step { node, edge });
    }
    Ok(out)
}

/// The end of `e` that is not `n`.
fn other_end<'a>(e: &'a Edge, n: &str) -> &'a str {
    if e.src == n { &e.dst } else { &e.src }
}

/// A graph held as it is.
pub struct MemoryIndex {
    graph: Graph,
    touching: HashMap<String, Vec<usize>>,
    names: HashMap<String, Vec<String>>,
}

impl MemoryIndex {
    #[must_use]
    pub fn new(graph: Graph) -> MemoryIndex {
        let mut touching: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in graph.edges.iter().enumerate() {
            touching.entry(e.src.clone()).or_default().push(i);
            if e.dst != e.src {
                touching.entry(e.dst.clone()).or_default().push(i);
            }
        }
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for (name, id) in &graph.aliases {
            names.entry(name.clone()).or_default().push(id.clone());
        }
        MemoryIndex {
            graph,
            touching,
            names,
        }
    }

    #[must_use]
    pub fn graph(&self) -> &Graph {
        &self.graph
    }
}

impl Index for MemoryIndex {
    async fn node(&self, id: &str) -> Result<Option<Node>> {
        Ok(self.graph.nodes.get(id).cloned())
    }

    async fn find(&self, name: &str) -> Result<Vec<Node>> {
        let mut ids: Vec<&String> = self
            .names
            .get(&name.trim().to_lowercase())
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        Ok(ids
            .into_iter()
            .filter_map(|id| self.graph.nodes.get(id).cloned())
            .collect())
    }

    async fn touching(&self, id: &str) -> Result<Vec<Edge>> {
        Ok(self
            .touching
            .get(id)
            .map(|v| v.iter().map(|i| self.graph.edges[*i].clone()).collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests;
