//! This device's part in handing graph data between the space's devices
//! (docs/graph.md, "Between peers"): it answers `drive/graph/1` from its
//! own `graph.db` (and its CLIP, if it has it), and asks the others for
//! what it cannot make itself: a note's extract when the bytes are out of
//! reach, the picture vectors of images when it has no CLIP, and a
//! search's words as CLIP sees them.
//!
//! Peers are the space's devices with an endpoint in the registry (the
//! same door lets them in both ways). Asked only while `drive serve` runs
//! its endpoint; a one-off command answers from what it has.

use std::sync::Arc;
use std::time::Duration;

use drive_files::space;
use drive_graph::db::GraphDb;
use drive_graph::share;
use drive_transfer::EndpointAddr;

use crate::agent::Agent;

/// How long one peer gets to answer.
const PATIENCE: Duration = Duration::from_secs(4);

/// The protocol, answering from this device's graph.
pub async fn protocol(a: &Agent) -> Result<drive_transfer::Extra, String> {
    let db = GraphDb::open(&a.dir.join("graph.db"))
        .await
        .map_err(|e| e.to_string())?;
    Ok(share::protocol(share::Answerer {
        db: Arc::new(db),
        images: crate::graph::image_model(&a.dir),
    }))
}

/// The other devices of the space that can be asked (an endpoint known).
fn peers(a: &Agent) -> Vec<EndpointAddr> {
    let me = a.device_id();
    let mut out: Vec<(String, EndpointAddr)> = space::endpoints(&a.space)
        .into_iter()
        .filter(|(device, _)| *device != me)
        .filter_map(|(device, (_, addr))| {
            Some((device, drive_transfer::addr_from_str(&addr).ok()?))
        })
        .collect();
    out.sort_by(|x, y| x.0.cmp(&y.0));
    out.into_iter().map(|(_, a)| a).collect()
}

/// The endpoint to ask from, while this device runs one.
fn endpoint(a: &Agent) -> Option<&iroh::Endpoint> {
    a.provider.get().map(|p| p.endpoint())
}

/// A peer's extract of a note's content (its sidecar's bytes, checked by
/// the caller).
pub async fn ask_extract(a: &Agent, key: &str) -> Option<Vec<u8>> {
    let peers = peers(a);
    let askable = share::Askable {
        endpoint: endpoint(a)?,
        peers: &peers,
        patience: PATIENCE,
    };
    askable.extract(key).await
}

/// The query as `model` sees it, made by a peer that has the model.
pub async fn ask_query(
    a: &Agent,
    model: &str,
    words: &str,
) -> Option<drive_graph::search::ImageQuery> {
    let peers = peers(a);
    let askable = share::Askable {
        endpoint: endpoint(a)?,
        peers: &peers,
        patience: PATIENCE,
    };
    let vector = askable.query(model, words).await?;
    Some(drive_graph::search::ImageQuery {
        model: model.to_string(),
        vector,
    })
}

/// CLIP's vector size (ViT-B/32).
const CLIP_DIMS: usize = 512;

/// The picture vectors of this space's images that this device has none
/// of, from the devices that made them (when it has no CLIP itself).
/// Returns how many images got theirs.
pub async fn fetch_image_vectors(a: &Agent, db: &GraphDb) -> Result<usize, String> {
    if crate::graph::image_model(&a.dir).is_some() {
        return Ok(0);
    }
    let Some(endpoint) = endpoint(a) else {
        return Ok(0);
    };
    let peers = peers(a);
    share::Askable {
        endpoint,
        peers: &peers,
        patience: PATIENCE,
    }
    .fetch_vectors(
        db,
        crate::graph::CLIP_ID,
        CLIP_DIMS,
        &drive_graph::embed::IMAGE_EXTENSIONS,
    )
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt::tests::{device, node, wait_until};
    use drive_graph::extract::{Sidecar, extract};
    use drive_graph::graph::{Graph, Node, NodeKind};

    async fn serving(a: &Agent) {
        let p = protocol(a).await.unwrap();
        a.start_transfer_with(
            drive_transfer::Settings {
                relay: None,
                local_only: true,
            },
            vec![p],
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_takes_what_another_derived() {
        let (url, _node) = node().await;
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let studio = device(&url, da.path(), "studio", "Studio PC").await;
        let laptop = device(&url, db.path(), "laptop", "Laptop").await;

        // The studio made a note's extract and an image's picture vector.
        let made = GraphDb::open(&studio.dir.join("graph.db")).await.unwrap();
        made.put_extracts(&[("h-note".into(), extract("[[Elsewhere]]"))])
            .await
            .unwrap();
        let v: Vec<f32> = (0..CLIP_DIMS).map(|i| (i as f32 / 512.0).sin()).collect();
        made.put_vectors(crate::graph::CLIP_ID, &[("h-img".into(), vec![v.clone()])])
            .await
            .unwrap();
        serving(&studio).await;
        serving(&laptop).await;
        wait_until("the laptop sees the studio's endpoint", || {
            space::endpoints(&laptop.space).contains_key("studio")
        })
        .await;

        // The laptop has the image in its graph but no CLIP: it takes the
        // studio's vector.
        let mine = GraphDb::open(&laptop.dir.join("graph.db")).await.unwrap();
        let mut g = Graph::default();
        g.nodes.insert(
            "file:1@1".into(),
            Node {
                id: "file:1@1".into(),
                kind: NodeKind::File,
                label: "IMG_1.jpg".into(),
                path: "Photos/IMG_1.jpg".into(),
            },
        );
        g.contents.insert("file:1@1".into(), "h-img".into());
        mine.replace(&g).await.unwrap();
        assert_eq!(fetch_image_vectors(&laptop, &mine).await.unwrap(), 1);
        let got = mine
            .vectors_of(crate::graph::CLIP_ID, &["h-img".into()])
            .await
            .unwrap();
        assert_eq!(got[0].1[0].len(), CLIP_DIMS);
        assert_eq!(
            fetch_image_vectors(&laptop, &mine).await.unwrap(),
            0,
            "once"
        );

        // And a note's extract when the bytes are out of reach.
        let bytes = ask_extract(&laptop, "h-note").await.unwrap();
        assert_eq!(
            Sidecar::decode(&bytes, "h-note").unwrap(),
            extract("[[Elsewhere]]")
        );
        assert!(ask_extract(&laptop, "unknown").await.is_none());
    }
}
