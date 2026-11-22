//! Asking the build group: one small protocol on each agent's iroh endpoint
//! (ALPN `drive/kache/1`), behind the same door as its files (only the
//! space's devices and nodes get in), and then only for devices of the
//! same build group.
//!
//! One request per stream, JSON both ways:
//!
//! ```text
//! {"has":"<key>"}      -> {"size":N,"hash":"<blake3>"} | {}
//! {"list":"<prefix>"}  -> {"keys":[["<key>",N,"<blake3>"],…]}
//! ```
//!
//! A `has` that is answered makes the object fetchable as an iroh-blobs
//! blob (its BLAKE3 is its name), which the asker then fetches verified.

use std::sync::{Arc, Weak};
use std::time::Instant;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
use serde::{Deserialize, Serialize};

use crate::{Found, Kache, Peer};

pub const ALPN: &[u8] = b"drive/kache/1";

/// Requests and answers are small; a listing is the big one.
const MAX_REQUEST: usize = 64 * 1024;
const MAX_LISTING: usize = 64 * 1024 * 1024;
const MAX_LISTED_KEYS: usize = 200_000;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Ask {
    Has(String),
    List(String),
}

#[derive(Serialize, Deserialize, Default)]
struct Has {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Listed {
    keys: Vec<(String, u64, String)>,
}

/// The protocol, for `drive_transfer::Provider::start_with`. Holds the
/// cache weakly: the provider outlives nothing it should not.
pub fn protocol(k: &Arc<Kache>) -> drive_transfer::Extra {
    (
        ALPN.to_vec(),
        Box::new(Answer {
            k: Arc::downgrade(k),
        }) as Box<dyn DynProtocolHandler>,
    )
}

struct Answer {
    k: Weak<Kache>,
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("kache::Answer")
    }
}

impl ProtocolHandler for Answer {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some(k) = self.k.upgrade() else {
            conn.close(2u32.into(), b"no cache here");
            return Ok(());
        };
        let who = conn.remote_id();
        if !k.is_member(&who) {
            tracing::warn!(
                kache.outcome = "refused",
                kache.peer = %who.fmt_short(),
                "a device outside this one's build group asked for its build cache"
            );
            conn.close(3u32.into(), b"not in this build group");
            return Ok(());
        }
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let k = k.clone();
            tokio::spawn(async move {
                let Ok(req) = recv.read_to_end(MAX_REQUEST).await else {
                    return;
                };
                let answer = match serde_json::from_slice::<Ask>(&req) {
                    Ok(Ask::Has(key)) => serde_json::to_vec(&k.answer_has(&key).await),
                    Ok(Ask::List(prefix)) => {
                        let mut keys: Vec<(String, u64, String)> = k
                            .store
                            .list(&prefix)
                            .into_iter()
                            .map(|(key, o)| (key, o.size, o.hash))
                            .collect();
                        keys.truncate(MAX_LISTED_KEYS);
                        serde_json::to_vec(&Listed { keys })
                    }
                    Err(_) => return,
                };
                if let Ok(bytes) = answer {
                    let _ = send.write_all(&bytes).await;
                    let _ = send.finish();
                }
            });
        }
        Ok(())
    }
}

impl Kache {
    /// What this device says when a peer asks for `key`: kept here, and
    /// fetchable (the provider has hashed it, and the hash is the kept one).
    async fn answer_has(&self, key: &str) -> Has {
        let Some((path, o)) = self.store.get(key) else {
            return Has::default();
        };
        let Some(link) = self.link.get() else {
            return Has::default();
        };
        match (link.provide)(path).await {
            Ok(hash) if hash == o.hash => Has {
                size: Some(o.size),
                hash: Some(o.hash),
            },
            Ok(_) => {
                tracing::warn!(
                    kache.outcome = "corrupt",
                    "a kept cache object no longer matches its hash; it is let go"
                );
                if let Some(p) = self.store.remove(key) {
                    self.forget(vec![p]).await;
                }
                Has::default()
            }
            Err(_) => Has::default(),
        }
    }

    pub(crate) async fn conn(&self, p: &Peer) -> Result<Connection, String> {
        if let Some(c) = self.conns.lock().expect("conns").get(&p.id)
            && c.close_reason().is_none()
        {
            return Ok(c.clone());
        }
        let link = self.link.get().ok_or("not linked")?;
        let c = link
            .endpoint
            .connect(p.addr.clone(), ALPN)
            .await
            .map_err(|e| format!("connecting: {e}"))?;
        self.conns.lock().expect("conns").insert(p.id, c.clone());
        Ok(c)
    }

    async fn ask_one(&self, p: &Peer, ask: &Ask, limit: usize) -> Result<Vec<u8>, String> {
        let c = self.conn(p).await?;
        let (mut send, mut recv) = c.open_bi().await.map_err(|e| e.to_string())?;
        let req = serde_json::to_vec(ask).expect("a request serializes");
        send.write_all(&req).await.map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())?;
        recv.read_to_end(limit).await.map_err(|e| e.to_string())
    }

    /// Peers worth asking now: the group, less those that just failed.
    fn askable(&self) -> Vec<Peer> {
        if self.link.get().is_none() {
            return Vec::new();
        }
        let down = self.down.lock().expect("down");
        self.members()
            .into_iter()
            .filter(|p| {
                down.get(&p.id)
                    .is_none_or(|at| at.elapsed() >= self.timeouts.down_for)
            })
            .collect()
    }

    fn failed(&self, p: &Peer) {
        self.down.lock().expect("down").insert(p.id, Instant::now());
        self.conns.lock().expect("conns").remove(&p.id);
    }

    /// The first peer that has `key`, all asked at once; none within the
    /// lookup timeout is a miss.
    pub async fn find(&self, key: &str) -> Option<Found> {
        let peers = self.askable();
        if peers.is_empty() {
            return None;
        }
        let ask = Ask::Has(key.to_string());
        let deadline = tokio::time::Instant::now() + self.timeouts.lookup;
        let mut asks: FuturesUnordered<_> = peers
            .into_iter()
            .map(|p| {
                let ask = &ask;
                async move {
                    let r = tokio::time::timeout_at(deadline, self.ask_one(&p, ask, MAX_REQUEST))
                        .await
                        .unwrap_or_else(|_| Err("no answer in time".into()));
                    (p, r)
                }
            })
            .collect();
        while let Some((p, r)) = asks.next().await {
            match r.map(|b| serde_json::from_slice::<Has>(&b)) {
                Ok(Ok(Has {
                    size: Some(size),
                    hash: Some(hash),
                })) => {
                    return Some(Found {
                        peer: p,
                        size,
                        hash,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(kache.peer = %p.name, error = %e, "a peer did not answer a cache lookup");
                    self.failed(&p);
                }
            }
        }
        None
    }

    /// Every askable peer's keys under `prefix`, as many as answer in time.
    pub(crate) async fn list_peers(&self, prefix: &str) -> Vec<(String, u64, String)> {
        let peers = self.askable();
        let ask = Ask::List(prefix.to_string());
        let deadline = tokio::time::Instant::now() + self.timeouts.list;
        let asks: FuturesUnordered<_> = peers
            .into_iter()
            .map(|p| {
                let ask = &ask;
                async move {
                    let r = tokio::time::timeout_at(deadline, self.ask_one(&p, ask, MAX_LISTING))
                        .await
                        .unwrap_or_else(|_| Err("no answer in time".into()));
                    (p, r)
                }
            })
            .collect();
        let answers: Vec<_> = asks.collect().await;
        let mut out = Vec::new();
        for (p, r) in answers {
            match r.map(|b| serde_json::from_slice::<Listed>(&b)) {
                Ok(Ok(l)) => out.extend(
                    l.keys
                        .into_iter()
                        .filter(|(k, _, _)| k.starts_with(prefix))
                        .take(MAX_LISTED_KEYS),
                ),
                Ok(Err(_)) => {}
                Err(_) => self.failed(&p),
            }
        }
        out
    }
}
