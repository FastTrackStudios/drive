//! Bringing a peer's graph up to date: the one routine every peer runs
//! (an agent, an app's embedded client, an always-on machine), so all of
//! them build the same graph from the same inputs.
//!
//! 1. The notes' content keys, from the tree.
//! 2. The extracts already kept for them ([`GraphDb::extracts`]).
//! 3. For each one missing, what the peer can get ([`Got`]): the bytes
//!    (parsed here), else another peer's sidecar (used only when it is
//!    for that content and this extractor's version), else nothing (the
//!    note then has no outgoing edges; links to it still arrive).
//! 4. The graph built ([`build`]) and kept in place of the old one, and
//!    extracts of content no longer in the space forgotten.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;

use drive_files::projects::Project;
use drive_spine::kinds::Kinds;
use drive_spine::tags::Library;
use drive_spine::{Event, Task};

use crate::db::GraphDb;
use crate::extract::{Extract, Sidecar, extract};
use crate::graph::{Inputs, build};
use crate::index::Result;
use crate::markdown::is_markdown;
use crate::space::{DocRef, TreeItem};

/// What the space syncs: everything the graph is built from except the
/// notes' text.
pub struct Sources<'a> {
    pub tree: &'a [TreeItem],
    pub tags: &'a Library,
    pub tasks: &'a [Task],
    pub events: &'a [Event],
    pub projects: &'a [Project],
    pub kinds: &'a Kinds,
    pub docs: &'a [DocRef],
}

/// What a peer could get for a note's content.
#[derive(Debug)]
pub enum Got {
    /// The file's bytes.
    Bytes(Vec<u8>),
    /// Another peer's sidecar for it (checked before it is used).
    Sidecar(Vec<u8>),
    /// Neither: the bytes are on a computer out of reach.
    Nothing,
}

/// What a refresh did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Distinct note contents in the space.
    pub notes: usize,
    /// Already kept from an earlier refresh.
    pub cached: usize,
    /// Parsed from their bytes now.
    pub parsed: usize,
    /// Taken from another peer's sidecar.
    pub shared: usize,
    /// Neither bytes nor a usable sidecar: no outgoing edges.
    pub unread: usize,
    pub nodes: usize,
    pub edges: usize,
}

/// The content keys of the space's notes, each once.
#[must_use]
pub fn note_keys(tree: &[TreeItem]) -> Vec<String> {
    tree.iter()
        .filter(|t| !t.dir && !t.hash.is_empty() && !t.is_hidden() && is_markdown(&t.path))
        .map(|t| t.hash.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Bring the graph in `db` up to date with `s`, getting each note not
/// read before with `get`.
///
/// # Errors
///
/// The database could not be read or written; the old graph stays.
pub async fn refresh<F, Fut>(db: &GraphDb, s: &Sources<'_>, mut get: F) -> Result<Report>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Got>,
{
    let keys = note_keys(s.tree);
    let mut extracts = db.extracts(&keys).await?;
    let mut report = Report {
        notes: keys.len(),
        cached: extracts.len(),
        ..Report::default()
    };
    let mut fresh: Vec<(String, Extract)> = Vec::new();
    for key in keys.iter().filter(|k| !extracts.contains_key(*k)) {
        match read(key, get(key.clone()).await) {
            Some((e, Source::Bytes)) => {
                report.parsed += 1;
                fresh.push((key.clone(), e));
            }
            Some((e, Source::Peer)) => {
                report.shared += 1;
                fresh.push((key.clone(), e));
            }
            None => report.unread += 1,
        }
    }
    db.put_extracts(&fresh).await?;
    extracts.extend(fresh);
    let g = build(&inputs(s, &extracts));
    report.nodes = g.nodes.len();
    report.edges = g.edges.len();
    db.replace(&g).await?;
    db.prune_extracts(&keys).await?;
    Ok(report)
}

enum Source {
    Bytes,
    Peer,
}

/// An extract from what was got for content `key`.
fn read(key: &str, got: Got) -> Option<(Extract, Source)> {
    match got {
        Got::Bytes(b) => Some((extract(&String::from_utf8_lossy(&b)), Source::Bytes)),
        Got::Sidecar(b) => Sidecar::decode(&b, key).ok().map(|e| (e, Source::Peer)),
        Got::Nothing => None,
    }
}

fn inputs<'a>(s: &Sources<'a>, extracts: &'a HashMap<String, Extract>) -> Inputs<'a> {
    Inputs {
        tree: s.tree,
        extracts,
        tags: s.tags,
        tasks: s.tasks,
        events: s.events,
        projects: s.projects,
        kinds: s.kinds,
        docs: s.docs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use crate::space::tests::tree;

    fn sources<'a>(items: &'a [TreeItem], lib: &'a Library, kinds: &'a Kinds) -> Sources<'a> {
        Sources {
            tree: items,
            tags: lib,
            tasks: &[],
            events: &[],
            projects: &[],
            kinds,
            docs: &[],
        }
    }

    #[tokio::test]
    async fn notes_are_parsed_once_shared_sidecars_are_checked() {
        let (items, _) = tree(&[
            ("A.md", ""),
            ("B.md", ""),
            ("C.md", ""),
            ("D.md", ""),
            ("img.png", ""),
        ]);
        let text = |k: &str| match k {
            "h:A.md" => "[[B]] [[C]] [[D]]",
            _ => "[[A]]",
        };
        let (lib, kinds) = (Library::default(), Kinds::from(vec![]));
        let db = GraphDb::memory().await.unwrap();
        let mut asked = Vec::new();
        let r = refresh(&db, &sources(&items, &lib, &kinds), |k| {
            asked.push(k.clone());
            let got = match k.as_str() {
                // B's bytes are elsewhere, but a peer shared its sidecar.
                "h:B.md" => Got::Sidecar(Sidecar::new(&k, extract("[[A]]")).encode()),
                // C's sidecar is for other content: not used.
                "h:C.md" => Got::Sidecar(Sidecar::new("h:other", extract("[[A]]")).encode()),
                "h:D.md" => Got::Nothing,
                _ => Got::Bytes(text(&k).as_bytes().to_vec()),
            };
            async move { got }
        })
        .await
        .unwrap();
        assert_eq!(
            asked,
            ["h:A.md", "h:B.md", "h:C.md", "h:D.md"],
            "never the image"
        );
        assert_eq!(
            (r.notes, r.cached, r.parsed, r.shared, r.unread),
            (4, 0, 1, 1, 2)
        );
        let a = items[0].node();
        let back = db.backlinks(&a).await.unwrap();
        assert_eq!(back.len(), 1, "B's edge came from the shared sidecar");
        assert_eq!(
            db.outgoing(&a).await.unwrap().len(),
            3,
            "C and D still have A's links in"
        );

        // Again: nothing is parsed twice; C and D are asked for again.
        let mut again = Vec::new();
        let r = refresh(&db, &sources(&items, &lib, &kinds), |k| {
            again.push(k);
            async { Got::Nothing }
        })
        .await
        .unwrap();
        assert_eq!(again, ["h:C.md", "h:D.md"]);
        assert_eq!((r.cached, r.parsed, r.unread), (2, 0, 2));
    }
}
