//! Hybrid search over a peer's graph: the full-text index, the vectors and
//! the graph itself, fused into one ranking (reciprocal rank fusion, as
//! Omnigraph does, here on every peer). Also the pass that embeds what has
//! no vectors yet.

use std::collections::{HashMap, HashSet};

use crate::db::GraphDb;
use crate::embed::{Embedder, chunks, fuse};
use crate::index::{Found, Index, Result};

/// The longest piece of a text one vector stands for (characters).
pub const CHUNK: usize = 1000;

/// Texts embedded in one call.
const BATCH: usize = 64;

/// Embed every text of the graph that has no vectors of this model yet,
/// and forget vectors of texts gone and of other models. Returns how many
/// texts were embedded.
///
/// # Errors
///
/// The database could not be read or written, or the model failed.
pub async fn embed_missing(db: &GraphDb, model: &dyn Embedder) -> Result<usize> {
    let have = db.vector_keys(model.id()).await?;
    let todo: Vec<(String, String)> = db
        .texts()
        .await?
        .into_iter()
        .filter(|(k, _)| !have.contains(k))
        .collect();
    for batch in todo.chunks(BATCH) {
        let vectors = embed_batch(model, batch)?;
        db.put_vectors(model.id(), &vectors).await?;
    }
    db.prune_vectors(model.id()).await?;
    Ok(todo.len())
}

/// Each text's chunks embedded in one call, grouped back by text.
fn embed_batch(
    model: &dyn Embedder,
    batch: &[(String, String)],
) -> Result<Vec<(String, Vec<Vec<f32>>)>> {
    let pieces: Vec<Vec<String>> = batch
        .iter()
        .map(|(_, text)| {
            let c = chunks(text, CHUNK);
            if c.is_empty() { vec![text.clone()] } else { c }
        })
        .collect();
    let flat: Vec<&str> = pieces.iter().flatten().map(String::as_str).collect();
    let mut vecs = model.embed(&flat).map_err(crate::index::Error)?.into_iter();
    Ok(batch
        .iter()
        .zip(&pieces)
        .map(|((key, _), p)| (key.clone(), vecs.by_ref().take(p.len()).collect()))
        .collect())
}

/// A search.
pub struct Ask<'a> {
    pub query: &'a str,
    /// The model for the meaning half; none: words only.
    pub model: Option<&'a dyn Embedder>,
    /// Rank things near this node higher (within two edges), among what
    /// the words and meaning found.
    pub near: Option<&'a str>,
    pub limit: usize,
}

/// What a search finds, best first: the words' ranking (full text) and the
/// meaning's (vectors), fused; then what is near `near` moved up.
///
/// # Errors
///
/// The database could not be read, or the model failed.
pub async fn hybrid(db: &GraphDb, ask: &Ask<'_>) -> Result<Vec<Found>> {
    let wide = ask.limit.saturating_mul(3).max(30);
    let mut nodes: HashMap<String, crate::graph::Node> = HashMap::new();
    let mut lists: Vec<Vec<String>> = Vec::new();
    let words = db.search(ask.query, wide).await?;
    lists.push(ids_of(&words, &mut nodes));
    if let Some(model) = ask.model {
        let q = model
            .embed(&[ask.query])
            .map_err(crate::index::Error)?
            .pop()
            .unwrap_or_default();
        if q.iter().any(|x| *x != 0.0) {
            let meaning = db.nearest(model.id(), &q, wide).await?;
            lists.push(ids_of(&meaning, &mut nodes));
        }
    }
    if let Some(near) = ask.near {
        let found: HashSet<&String> = lists.iter().flatten().collect();
        let close: Vec<String> = db
            .neighbors(near, 2)
            .await?
            .into_iter()
            .map(|n| n.node.id)
            .filter(|id| found.contains(id))
            .collect();
        lists.push(close);
    }
    Ok(fuse(&lists)
        .into_iter()
        .filter_map(|(id, score)| {
            Some(Found {
                node: nodes.get(&id)?.clone(),
                score,
            })
        })
        .take(ask.limit)
        .collect())
}

fn ids_of(found: &[Found], nodes: &mut HashMap<String, crate::graph::Node>) -> Vec<String> {
    found
        .iter()
        .map(|f| {
            nodes
                .entry(f.node.id.clone())
                .or_insert_with(|| f.node.clone());
            f.node.id.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::tests::tiny;
    use crate::graph::{Graph, Node, NodeKind};

    fn note(g: &mut Graph, id: &str, text: &str) {
        g.nodes.insert(
            id.into(),
            Node {
                id: id.into(),
                kind: NodeKind::File,
                label: id.into(),
                path: String::new(),
            },
        );
        g.texts.insert(id.into(), text.into());
    }

    #[tokio::test]
    async fn words_and_meaning_together() {
        let mut g = Graph::default();
        note(&mut g, "file:a", "the hymn");
        note(&mut g, "file:b", "warm bass preset");
        note(&mut g, "file:c", "grace grace grace");
        g.edges.push(crate::graph::Edge {
            src: "file:c".into(),
            dst: "file:b".into(),
            kind: crate::graph::EdgeKind::Linked,
            raw: String::new(),
            anchor: String::new(),
        });
        let db = GraphDb::memory().await.unwrap();
        db.replace(&g).await.unwrap();
        let m = tiny();
        assert_eq!(embed_missing(&db, &m).await.unwrap(), 3);
        assert_eq!(embed_missing(&db, &m).await.unwrap(), 0, "each text once");

        // "song" is in no note: only its meaning finds the hymn.
        let ask = |near| Ask {
            query: "song",
            model: Some(&m),
            near,
            limit: 5,
        };
        let found = hybrid(&db, &ask(None)).await.unwrap();
        assert_eq!(found[0].node.id, "file:a", "{found:?}");
        let words_only = hybrid(
            &db,
            &Ask {
                model: None,
                ..ask(None)
            },
        )
        .await
        .unwrap();
        assert!(words_only.is_empty());

        // Both halves: "bass" is a word of b and close in meaning to it.
        let found = hybrid(
            &db,
            &Ask {
                query: "bass",
                ..ask(None)
            },
        )
        .await
        .unwrap();
        assert_eq!(found[0].node.id, "file:b");

        // A text that changed is embedded again; one gone loses its vectors.
        g.texts.insert("file:a".into(), "preset".into());
        g.texts.remove("file:c");
        g.nodes.remove("file:c");
        db.replace(&g).await.unwrap();
        assert_eq!(embed_missing(&db, &m).await.unwrap(), 1);
        let found = hybrid(&db, &ask(None)).await.unwrap();
        assert!(!found.iter().any(|f| f.node.id == "file:c"));
    }
}
