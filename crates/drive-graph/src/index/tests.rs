//! Every question, asked of the in-memory index and of the database: the
//! two must answer the same.

use super::*;
use crate::graph::tests::{Ids, fixture};

fn ids_of(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.node.id.as_str()).collect()
}

fn near(n: &[Near]) -> Vec<(&str, u32)> {
    n.iter().map(|n| (n.node.id.as_str(), n.depth)).collect()
}

async fn questions<I: Index>(ix: &I, ids: &Ids) {
    // Backlinks: Circle links Scales, Chords links it, and Chords' related.
    let back = ix.backlinks(&ids.scales).await.unwrap();
    let mut want = vec![ids.chords.as_str(), ids.circle.as_str()];
    want.sort_by_key(|id| ix_label(id, ids));
    assert_eq!(ids_of(&back), want);
    let back = ix.backlinks("missing:nowhere").await.unwrap();
    assert_eq!(back.len(), 2, "both notes that link a page not written yet");

    // Neighbours, either way.
    let one = ix.neighbors(&ids.png, 1).await.unwrap();
    let mut want = vec![
        (ids.scales.as_str(), 1),
        (ids.drums.as_str(), 1),
        (ids.notes.as_str(), 1),
    ];
    want.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(near(&one), want);
    let two = ix.neighbors(&ids.png, 2).await.unwrap();
    assert!(near(&two).contains(&(ids.music.as_str(), 2)));
    assert!(
        near(&two).contains(&(ids.chords.as_str(), 2)),
        "via Scales, either way"
    );
    assert!(
        !near(&two).iter().any(|(id, _)| *id == ids.png),
        "not itself"
    );

    // A shortest path: the image to the project, through the song.
    let path = ix
        .path(&ids.png, "project:p1", MAX_PATH)
        .await
        .unwrap()
        .expect("a path");
    let steps: Vec<&str> = path.iter().map(|s| s.node.id.as_str()).collect();
    assert_eq!(
        steps,
        [
            ids.png.as_str(),
            ids.scales.as_str(),
            "doc:studio:song:amazing-grace",
            "project:p1"
        ]
    );
    assert!(path[0].edge.is_none() && path[1].edge.is_some());
    assert!(
        ix.path(&ids.png, "project:p1", 2).await.unwrap().is_none(),
        "too far"
    );
    assert!(
        ix.path(&ids.png, "nothing:here", 8)
            .await
            .unwrap()
            .is_none()
    );

    // Under Music: everything tagged Music, Theory or Drums, by either kind
    // of tag, each once.
    let under = ix.under_tag(&ids.music).await.unwrap();
    let mut got = ids_of(&under);
    got.sort();
    let mut want = vec![
        ids.png.as_str(),
        ids.scales.as_str(),
        ids.chords.as_str(),
        "doc:studio:song:amazing-grace",
        "content:abc",
    ];
    want.sort();
    assert_eq!(got, want);
    let under = ix.under_tag(&ids.drums).await.unwrap();
    let mut got = ids_of(&under);
    got.sort();
    let mut want = vec![ids.png.as_str(), ids.chords.as_str()];
    want.sort();
    assert_eq!(got, want);

    // By name: an alias, a tag, a file name.
    assert_eq!(ix.find("modes").await.unwrap()[0].id, ids.scales);
    assert_eq!(ix.find("DRUMS").await.unwrap()[0].id, ids.drums);
    assert_eq!(ix.find("circle.png").await.unwrap()[0].id, ids.png);
    assert!(ix.find("zzz").await.unwrap().is_empty());
    assert_eq!(
        ix.outgoing("task:t1").await.unwrap()[0].node.id,
        "doc:studio:song:amazing-grace"
    );

    // Search: a note's words, its title, a tag's name; syntax is just text.
    let found = ix.search("unknown-tag", 10).await.unwrap();
    assert!(
        found.iter().any(|f| f.node.id == ids.chords),
        "frontmatter tags are searched"
    );
    let found = ix.search("Fifths Scales", 10).await.unwrap();
    assert_eq!(
        found[0].node.id, ids.scales,
        "both words, and the title: {found:?}"
    );
    assert!(found.windows(2).all(|w| w[0].score >= w[1].score));
    assert!(found[0].score > 0.0, "ranked, not just matched");
    let found = ix.search("drums", 10).await.unwrap();
    assert!(found.iter().any(|f| f.node.id == ids.drums));
    assert!(ix.search("zzzz", 10).await.unwrap().is_empty());
    // Quotes and operators are only separators, never query syntax.
    ix.search("\"' OR 1=1 -- (", 10).await.unwrap();
    assert!(ix.search("", 10).await.unwrap().is_empty());
}

/// Backlinks come sorted by label: Chords before Circle.
fn ix_label(id: &str, ids: &Ids) -> u8 {
    u8::from(id != ids.chords)
}

#[tokio::test]
async fn in_memory() {
    let (g, ids) = fixture();
    questions(&MemoryIndex::new(g), &ids).await;
}

#[cfg(feature = "turso")]
#[tokio::test]
async fn in_the_database() {
    let (g, ids) = fixture();
    let db = crate::db::GraphDb::memory().await.unwrap();
    db.replace(&g).await.unwrap();
    questions(&db, &ids).await;
    // Replacing keeps nothing of the old graph.
    db.replace(&crate::graph::Graph::default()).await.unwrap();
    assert!(db.node(&ids.scales).await.unwrap().is_none());
    assert!(db.backlinks(&ids.scales).await.unwrap().is_empty());
}

#[cfg(feature = "turso")]
#[tokio::test]
async fn extracts_are_kept_by_content_and_version() {
    use crate::extract::{Sidecar, extract};
    let dir = tempdir();
    let path = dir.join("graph.db");
    let db = crate::db::GraphDb::open(&path).await.unwrap();
    let a = extract("[[A]]");
    db.put_extracts(&[("h1".into(), a.clone()), ("h2".into(), extract("[[B]]"))])
        .await
        .unwrap();
    let got = db.extracts(&["h1".into(), "h3".into()]).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["h1"], a);
    let bytes = db.sidecar("h1").await.unwrap().unwrap();
    assert_eq!(
        Sidecar::decode(&bytes, "h1").unwrap(),
        a,
        "shareable as it is"
    );
    assert_eq!(db.prune_extracts(&["h1".into()]).await.unwrap(), 1);
    assert!(db.extracts(&["h2".into()]).await.unwrap().is_empty());
    drop(db);
    // Kept across opens: a file is never parsed twice.
    let db = crate::db::GraphDb::open(&path).await.unwrap();
    assert_eq!(db.extracts(&["h1".into()]).await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "turso")]
fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "drive-graph-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
