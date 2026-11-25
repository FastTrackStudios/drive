//! Hybrid search over a peer's graph: the full-text index, the vectors and
//! the graph itself, fused into one ranking (reciprocal rank fusion, as
//! Omnigraph does, here on every peer). Also the pass that embeds what has
//! no vectors yet.

use std::collections::{HashMap, HashSet};

use crate::db::GraphDb;
use crate::embed::{Embedder, IMAGE_EXTENSIONS, ImageEmbedder, chunks, fuse};
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

/// Images embedded in one call.
const IMAGE_BATCH: usize = 8;

/// Embed up to `max` of the space's images that have no vectors of this
/// image model yet (`bytes`: an image's bytes by content key, `None` when
/// this peer cannot have them), and forget vectors of images gone.
/// Returns how many were embedded and how many could not be read.
///
/// # Errors
///
/// The database could not be read or written.
pub async fn embed_missing_images<F, Fut>(
    db: &GraphDb,
    model: &dyn ImageEmbedder,
    max: usize,
    mut bytes: F,
) -> Result<(usize, usize)>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    let have = db.vector_keys(model.id()).await?;
    let mut keys: Vec<String> = db
        .contents_of(&IMAGE_EXTENSIONS)
        .await?
        .into_iter()
        .map(|(_, k)| k)
        .filter(|k| !have.contains(k))
        .collect();
    keys.dedup();
    keys.truncate(max);
    let (mut done, mut unread) = (0, 0);
    for batch in keys.chunks(IMAGE_BATCH) {
        let mut got: Vec<(String, Vec<u8>)> = Vec::new();
        for k in batch {
            match bytes(k.clone()).await {
                Some(b) => got.push((k.clone(), b)),
                None => unread += 1,
            }
        }
        // One at a time when a batch fails, so one bad image costs only itself.
        let images: Vec<Vec<u8>> = got.iter().map(|(_, b)| b.clone()).collect();
        let vectors: Vec<(String, Vec<Vec<f32>>)> = match model.embed_images(&images) {
            Ok(v) => got
                .iter()
                .map(|(k, _)| k.clone())
                .zip(v.into_iter().map(|x| vec![x]))
                .collect(),
            Err(_) => got
                .iter()
                .filter_map(|(k, b)| {
                    let v = model.embed_images(std::slice::from_ref(b)).ok()?.pop()?;
                    Some((k.clone(), vec![v]))
                })
                .collect(),
        };
        unread += got.len() - vectors.len();
        done += vectors.len();
        db.put_vectors(model.id(), &vectors).await?;
    }
    db.prune_content_vectors(model.id()).await?;
    Ok((done, unread))
}

/// A search.
pub struct Ask<'a> {
    pub query: &'a str,
    /// The model for the meaning half; none: words only.
    pub model: Option<&'a dyn Embedder>,
    /// A model that finds images by what they show; none: images by
    /// their names only.
    pub images: Option<&'a dyn ImageEmbedder>,
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
    if let Some(images) = ask.images {
        let q = images.embed_query(ask.query).map_err(crate::index::Error)?;
        let pictures = db.nearest_content(images.id(), &q, wide).await?;
        lists.push(ids_of(&pictures, &mut nodes));
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

    /// Pictures as one-hot colours: "red" finds the red one.
    struct Colours;

    impl ImageEmbedder for Colours {
        fn id(&self) -> &str {
            "image:colours@1"
        }
        fn embed_images(&self, images: &[Vec<u8>]) -> std::result::Result<Vec<Vec<f32>>, String> {
            images
                .iter()
                .map(|b| match b.as_slice() {
                    b"red" => Ok(vec![1.0, 0.0]),
                    b"blue" => Ok(vec![0.0, 1.0]),
                    _ => Err("not an image".into()),
                })
                .collect()
        }
        fn embed_query(&self, text: &str) -> std::result::Result<Vec<f32>, String> {
            Ok(if text.contains("red") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            })
        }
    }

    #[tokio::test]
    async fn images_are_found_by_what_they_show() {
        let mut g = Graph::default();
        for (id, path, key) in [
            ("file:r", "IMG_1.jpg", "kr"),
            ("file:b", "IMG_2.png", "kb"),
            ("file:x", "broken.jpg", "kx"),
            ("file:t", "notes.txt", "kt"),
        ] {
            note(&mut g, id, path);
            g.nodes.get_mut(id).unwrap().path = path.into();
            g.contents.insert(id.into(), key.into());
        }
        let db = GraphDb::memory().await.unwrap();
        db.replace(&g).await.unwrap();
        let bytes = |k: String| async move {
            match k.as_str() {
                "kr" => Some(b"red".to_vec()),
                "kb" => Some(b"blue".to_vec()),
                "kx" => Some(b"garbage".to_vec()),
                _ => None,
            }
        };
        assert_eq!(
            embed_missing_images(&db, &Colours, 100, bytes)
                .await
                .unwrap(),
            (2, 1)
        );
        assert_eq!(
            embed_missing_images(&db, &Colours, 100, bytes)
                .await
                .unwrap(),
            (0, 1),
            "each image once; the broken one is asked again"
        );
        let found = hybrid(
            &db,
            &Ask {
                query: "a red sunset",
                model: None,
                images: Some(&Colours),
                near: None,
                limit: 5,
            },
        )
        .await
        .unwrap();
        assert_eq!(found[0].node.id, "file:r", "{found:?}");
        // Text vectors and image vectors are pruned apart.
        db.prune_vectors("tiny@1").await.unwrap();
        assert_eq!(db.vector_keys("image:colours@1").await.unwrap().len(), 2);
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
            images: None,
            near,
            limit: 5,
        };
        let found = hybrid(&db, &ask(None)).await.unwrap();
        assert_eq!(found[0].node.id, "file:a", "{found:?}");
        let words_only = hybrid(
            &db,
            &Ask {
                model: None,
                images: None,
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
