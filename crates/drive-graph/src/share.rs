//! What one peer derived, handed to another: one small protocol on a
//! device's iroh endpoint (ALPN `drive/graph/1`), behind the same door as
//! its files (only the space's own devices get in).
//!
//! One request per stream, JSON both ways:
//!
//! ```text
//! {"extract":"<key>"}                         -> a Sidecar's bytes | nothing
//! {"vectors":{"model":"…","keys":["…",…]}}    -> {"vectors":[["<key>",[[f32…]…]],…]}
//! {"query":{"model":"…","text":"…"}}          -> {"vector":[f32…]} | {}
//! ```
//!
//! Nothing received is an authority. An extract is used only when it is
//! for the content asked and this extractor's version
//! ([`crate::extract::Sidecar::decode`]), and never over the bytes when the
//! peer has them. Vectors come only from the space's own devices (the
//! door), and only for a model this peer asked for by id. A `query` lets
//! a peer without a heavy model (CLIP on a phone) search the vectors it
//! was given: the words go to a device that has the model, the vector
//! comes back, and the search runs here.

use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};

use crate::db::GraphDb;
use crate::embed::ImageEmbedder;

pub const ALPN: &[u8] = b"drive/graph/1";

/// Requests are small; an answer of vectors is the big one.
const MAX_REQUEST: usize = 256 * 1024;
const MAX_ANSWER: usize = 64 * 1024 * 1024;
/// Keys asked for in one `vectors` request at most.
pub const MAX_KEYS: usize = 256;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Ask {
    Extract(String),
    Vectors { model: String, keys: Vec<String> },
    Query { model: String, text: String },
}

#[derive(Serialize, Deserialize, Default)]
struct Vectors {
    vectors: Vec<(String, Vec<Vec<f32>>)>,
}

#[derive(Serialize, Deserialize, Default)]
struct QueryVector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
}

/// What a peer answers from: its graph, and the image model it has.
pub struct Answerer {
    pub db: Arc<GraphDb>,
    pub images: Option<&'static dyn ImageEmbedder>,
}

impl std::fmt::Debug for Answerer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("graph::Answerer")
    }
}

/// The protocol, for an endpoint's router (`drive_transfer`'s extra
/// protocols, behind its door).
pub fn protocol(a: Answerer) -> (Vec<u8>, Box<dyn DynProtocolHandler>) {
    (
        ALPN.to_vec(),
        Box::new(Handler(Arc::new(a))) as Box<dyn DynProtocolHandler>,
    )
}

#[derive(Debug)]
struct Handler(Arc<Answerer>);

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let me = self.0.clone();
            tokio::spawn(async move {
                let Ok(req) = recv.read_to_end(MAX_REQUEST).await else {
                    return;
                };
                let Ok(ask) = serde_json::from_slice::<Ask>(&req) else {
                    return;
                };
                let answer = me.answer(ask).await;
                let _ = send.write_all(&answer).await;
                let _ = send.finish();
            });
        }
        Ok(())
    }
}

impl Answerer {
    async fn answer(&self, ask: Ask) -> Vec<u8> {
        match ask {
            Ask::Extract(key) => self
                .db
                .sidecar(&key)
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            Ask::Vectors { model, mut keys } => {
                keys.truncate(MAX_KEYS);
                let vectors = self.db.vectors_of(&model, &keys).await.unwrap_or_default();
                serde_json::to_vec(&Vectors { vectors }).unwrap_or_default()
            }
            Ask::Query { model, text } => {
                let vector = self
                    .images
                    .filter(|m| m.id() == model)
                    .and_then(|m| m.embed_query(&text).ok());
                serde_json::to_vec(&QueryVector { vector }).unwrap_or_default()
            }
        }
    }
}

/// Ask one peer: the answer's bytes.
async fn ask(endpoint: &Endpoint, peer: &EndpointAddr, ask: &Ask) -> Result<Vec<u8>, String> {
    let conn = endpoint
        .connect(peer.clone(), ALPN)
        .await
        .map_err(|e| format!("connecting: {e}"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let req = serde_json::to_vec(ask).expect("a request serializes");
    send.write_all(&req).await.map_err(|e| e.to_string())?;
    send.finish().map_err(|e| e.to_string())?;
    let out = recv
        .read_to_end(MAX_ANSWER)
        .await
        .map_err(|e| e.to_string());
    conn.close(0u32.into(), b"done");
    out
}

/// A peer's sidecar for a note's content, as its bytes (still to be
/// checked with `Sidecar::decode`); none when it has none.
pub async fn ask_extract(endpoint: &Endpoint, peer: &EndpointAddr, key: &str) -> Option<Vec<u8>> {
    ask(endpoint, peer, &Ask::Extract(key.to_string()))
        .await
        .ok()
        .filter(|b| !b.is_empty())
}

/// A peer's vectors of `model` for these keys (at most [`MAX_KEYS`]),
/// each of `dims` numbers (others are dropped).
pub async fn ask_vectors(
    endpoint: &Endpoint,
    peer: &EndpointAddr,
    model: &str,
    keys: &[String],
    dims: Option<usize>,
) -> Result<Vec<(String, Vec<Vec<f32>>)>, String> {
    let keys: Vec<String> = keys.iter().take(MAX_KEYS).cloned().collect();
    let wanted: std::collections::HashSet<&String> = keys.iter().collect();
    let bytes = ask(
        endpoint,
        peer,
        &Ask::Vectors {
            model: model.to_string(),
            keys: keys.clone(),
        },
    )
    .await?;
    let v: Vectors = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(v.vectors
        .into_iter()
        .filter(|(k, vs)| {
            wanted.contains(k)
                && !vs.is_empty()
                && vs
                    .iter()
                    .all(|x| dims.is_none_or(|d| x.len() == d) && x.iter().all(|f| f.is_finite()))
        })
        .collect())
}

/// The query's vector in `model`'s space, made by a peer that has it.
pub async fn ask_query(
    endpoint: &Endpoint,
    peer: &EndpointAddr,
    model: &str,
    text: &str,
) -> Option<Vec<f32>> {
    let bytes = ask(
        endpoint,
        peer,
        &Ask::Query {
            model: model.to_string(),
            text: text.to_string(),
        },
    )
    .await
    .ok()?;
    serde_json::from_slice::<QueryVector>(&bytes)
        .ok()?
        .vector
        .filter(|v| !v.is_empty() && v.iter().all(|f| f.is_finite()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Sidecar, extract};
    use crate::graph::Graph;

    struct Colours;
    impl ImageEmbedder for Colours {
        fn id(&self) -> &str {
            "image:colours@1"
        }
        fn embed_images(&self, _: &[Vec<u8>]) -> Result<Vec<Vec<f32>>, String> {
            Ok(vec![])
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>, String> {
            Ok(if text.contains("red") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            })
        }
    }

    async fn endpoint() -> Endpoint {
        let settings = drive_net::Settings {
            relay: None,
            local_only: true,
        };
        drive_net::endpoint(iroh::SecretKey::generate(), &settings, vec![ALPN.to_vec()])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_peer_hands_over_what_it_derived() {
        static COLOURS: Colours = Colours;
        // The giver: a note's extract and an image's vectors.
        let db = Arc::new(GraphDb::memory().await.unwrap());
        db.replace(&Graph::default()).await.unwrap();
        db.put_extracts(&[("h1".into(), extract("[[A]]"))])
            .await
            .unwrap();
        db.put_vectors("image:colours@1", &[("k1".into(), vec![vec![1.0, 0.0]])])
            .await
            .unwrap();
        let giver = endpoint().await;
        let (alpn, handler) = protocol(Answerer {
            db,
            images: Some(&COLOURS),
        });
        let router = iroh::protocol::Router::builder(giver.clone())
            .accept(alpn, handler)
            .spawn();
        let addr = giver.addr();

        let asker = endpoint().await;
        let got = ask_extract(&asker, &addr, "h1").await.unwrap();
        assert_eq!(Sidecar::decode(&got, "h1").unwrap(), extract("[[A]]"));
        assert!(ask_extract(&asker, &addr, "nope").await.is_none());

        let v = ask_vectors(
            &asker,
            &addr,
            "image:colours@1",
            &["k1".into(), "k2".into()],
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(v, vec![("k1".to_string(), vec![vec![1.0, 0.0]])]);
        let wrong = ask_vectors(&asker, &addr, "image:colours@1", &["k1".into()], Some(512))
            .await
            .unwrap();
        assert!(wrong.is_empty(), "vectors of another size are not taken");

        assert_eq!(
            ask_query(&asker, &addr, "image:colours@1", "red things").await,
            Some(vec![1.0, 0.0])
        );
        assert_eq!(ask_query(&asker, &addr, "image:other@1", "red").await, None);
        router.shutdown().await.unwrap();
    }
}
