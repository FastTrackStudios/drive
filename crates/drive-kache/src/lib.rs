//! kache build caches shared peer to peer through Drive (docs/kache.md).
//!
//! kache's remote cache speaks S3, GCS, OCI or a shared folder; it has no
//! peer-to-peer backend. So the agent offers it a **local S3-compatible
//! endpoint** on loopback ([`s3`]) and does the sharing behind it:
//!
//! - objects kache stores are kept in the agent's own folder, under a
//!   size budget, least recently used let go first ([`store`]);
//! - an object not kept here is asked of the device's **build group**
//!   (the other devices whose builds share keys: same OS and CPU) over
//!   the agent's iroh endpoint ([`peers`]), all of them at once with a
//!   short timeout, and fetched from the first that has it as a
//!   BLAKE3-verified blob (iroh-blobs, `drive-transfer`), then kept here;
//! - nothing is announced: a peer finds a key by asking when it misses.

pub mod peers;
pub mod s3;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use drive_transfer::{EndpointAddr, EndpointId, Fetcher};
use futures::future::{BoxFuture, FutureExt, Shared};

pub use store::{Commit, Cond, Obj, Store};

/// The bucket kache is told to use; any other is answered as missing.
pub const BUCKET: &str = "kache";

/// The port the agent listens on by default (127.0.0.1 only).
pub const DEFAULT_PORT: u16 = 9778;

/// The local copy's size by default: 20 GiB.
pub const DEFAULT_BUDGET: u64 = 20 << 30;

/// Make a kept file fetchable by peers: its iroh-blobs hash (hex).
pub type ProvideFn =
    Arc<dyn Fn(PathBuf) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;
/// Stop serving a file that was let go.
pub type ForgetFn = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, ()> + Send + Sync>;

/// Another device of this one's build group.
#[derive(Clone, Debug)]
pub struct Peer {
    pub id: EndpointId,
    pub addr: EndpointAddr,
    /// Its name, for logs.
    pub name: String,
}

/// This device's build group as it is now, itself left out.
pub type Members = Arc<dyn Fn() -> Vec<Peer> + Send + Sync>;

/// How this device reaches its peers: its own endpoint (so their doors
/// know it), a fetcher on it, and its provider.
pub struct Link {
    pub endpoint: iroh::Endpoint,
    pub fetcher: Arc<Fetcher>,
    pub provide: ProvideFn,
    pub forget: ForgetFn,
}

#[derive(Clone, Debug)]
pub struct Timeouts {
    /// Asking every peer whether it has a key: a miss answers within this.
    pub lookup: Duration,
    /// Asking every peer for its keys under a prefix (LIST).
    pub list: Duration,
    /// A peer that did not answer is not asked again for this long.
    pub down_for: Duration,
    /// A whole fetch from a peer.
    pub fetch: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            lookup: Duration::from_millis(600),
            list: Duration::from_secs(2),
            down_for: Duration::from_secs(30),
            fetch: Duration::from_secs(600),
        }
    }
}

/// What a peer said it has.
#[derive(Clone, Debug)]
pub struct Found {
    pub peer: Peer,
    pub size: u64,
    pub hash: String,
}

type Flight = Shared<BoxFuture<'static, Result<(), String>>>;

/// A flight's key leaves `inflight` when its task ends, however it ends
/// (a panic too): the next asker starts a fetch of its own.
struct Landed<'a>(&'a Kache, &'a str);

impl Drop for Landed<'_> {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.0.inflight.lock() {
            inflight.remove(self.1);
        }
    }
}

/// A peer listing kept for later pages: (prefix, when, (key, size, hash)).
type ListedPeers = (String, Instant, Vec<(String, u64, String)>);

/// The shared cache of one device.
pub struct Kache {
    pub store: Store,
    members: Members,
    pub timeouts: Timeouts,
    link: OnceLock<Link>,
    inflight: Mutex<HashMap<String, Flight>>,
    down: Mutex<HashMap<EndpointId, Instant>>,
    conns: Mutex<HashMap<EndpointId, iroh::endpoint::Connection>>,
    listed_peers: Mutex<Option<ListedPeers>>,
}

impl Kache {
    pub fn new(store: Store, members: Members, timeouts: Timeouts) -> Arc<Kache> {
        Arc::new(Kache {
            store,
            members,
            timeouts,
            link: OnceLock::new(),
            inflight: Mutex::new(HashMap::new()),
            down: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            listed_peers: Mutex::new(None),
        })
    }

    /// Reach peers through this device's endpoint (once it is up). Before
    /// that, and without peers, the cache is this device's alone.
    pub fn attach(&self, link: Link) {
        let _ = self.link.set(link);
    }

    pub fn members(&self) -> Vec<Peer> {
        (self.members)()
    }

    /// Whether `id` may ask this device for its cache: one of its group.
    pub fn is_member(&self, id: &EndpointId) -> bool {
        self.members().iter().any(|p| &p.id == id)
    }

    /// An object kept here, or else found on a peer (not fetched).
    pub async fn head(&self, key: &str) -> Option<(u64, String, u64)> {
        if let Some((_, o)) = self.store.get(key) {
            return Some((o.size, o.hash, o.modified));
        }
        let f = self.find(key).await?;
        Some((f.size, f.hash, store::now_ms()))
    }

    /// An object's file here: kept, or fetched from a peer now. Peers'
    /// copies arrive verified (BLAKE3) and are kept even when the asker
    /// gave up waiting.
    pub async fn open(self: &Arc<Self>, key: &str) -> Result<Option<(PathBuf, Obj)>, String> {
        if let Some(got) = self.store.get(key) {
            return Ok(Some(got));
        }
        let me = self.clone();
        let k = key.to_string();
        self.flight(key, async move { me.fetch_from_group(&k).await })
            .await?;
        Ok(self.store.get(key))
    }

    /// The one fetch of `key` running now, or `work` started as it: every
    /// asker waits on the same one.
    fn flight(
        self: &Arc<Self>,
        key: &str,
        work: impl Future<Output = Result<(), String>> + Send + 'static,
    ) -> Flight {
        let mut inflight = self.inflight.lock().expect("inflight");
        if let Some(f) = inflight.get(key) {
            return f.clone();
        }
        let me = self.clone();
        let k = key.to_string();
        let task = tokio::spawn(async move {
            let _landed = Landed(&me, &k);
            work.await
        });
        let f: Flight = async move {
            task.await
                .unwrap_or_else(|e| Err(format!("fetch task: {e}")))
        }
        .boxed()
        .shared();
        inflight.insert(key.to_string(), f.clone());
        f
    }

    async fn fetch_from_group(&self, key: &str) -> Result<(), String> {
        let Some(found) = self.find(key).await else {
            return Ok(());
        };
        let started = Instant::now();
        let r = tokio::time::timeout(self.timeouts.fetch, self.fetch(key, &found))
            .await
            .unwrap_or_else(|_| Err("the fetch took too long".into()));
        match &r {
            Ok(()) => tracing::info!(
                kache.outcome = "fetched",
                kache.peer = %found.peer.name,
                kache.bytes = found.size,
                kache.ms = started.elapsed().as_millis() as u64,
                transfer.path = self.link.get().map_or("none", |l| l.fetcher.path_kind(&found.peer.id)),
                "a cache object came from a peer"
            ),
            Err(e) => tracing::warn!(
                kache.outcome = "fetch_failed",
                kache.peer = %found.peer.name,
                error = %e,
                "a cache object a peer has could not be fetched; kache compiles instead"
            ),
        }
        r
    }

    async fn fetch(&self, key: &str, found: &Found) -> Result<(), String> {
        use futures::StreamExt;
        use std::io::Write;
        let link = self.link.get().ok_or("not linked")?;
        let hash: drive_transfer::Hash = found.hash.parse().map_err(|_| "a bad hash")?;
        let (tmp, mut file) = self.store.temp().map_err(|e| e.to_string())?;
        let mut hasher = blake3::Hasher::new();
        let mut n = 0u64;
        let result: Result<(), String> = async {
            if found.size > 0 {
                let src = drive_transfer::Source {
                    addr: found.peer.addr.clone(),
                    hash,
                    size: found.size,
                };
                let mut s = std::pin::pin!(link.fetcher.stream(src, 0, found.size - 1));
                while let Some(chunk) = s.next().await {
                    let chunk = chunk.map_err(|e| e.to_string())?;
                    hasher.update(&chunk);
                    file.write_all(&chunk).map_err(|e| e.to_string())?;
                    n += chunk.len() as u64;
                }
            }
            file.flush().map_err(|e| e.to_string())?;
            let got = hasher.finalize().to_hex().to_string();
            if n != found.size || got != found.hash {
                return Err("the bytes that came do not match their hash".into());
            }
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = result {
            self.store.discard(&tmp);
            return Err(e);
        }
        // Stored here meanwhile (kache put it): keep that one.
        let (_, gone) = self
            .store
            .commit(key, &tmp, n, &found.hash, &Cond::IfAbsent)
            .map_err(|e| e.to_string())?;
        self.forget(gone).await;
        Ok(())
    }

    /// Tell the provider to stop serving files let go.
    pub async fn forget(&self, gone: Vec<PathBuf>) {
        if let Some(link) = self.link.get() {
            for p in gone {
                (link.forget)(p).await;
            }
        }
    }

    /// Keys under `prefix` here and on every peer that answers in time:
    /// (key, size, hash), in order. Peer results are reused across pages.
    pub async fn list(&self, prefix: &str) -> Vec<(String, u64, String)> {
        let cached = self
            .listed_peers
            .lock()
            .expect("listed peers")
            .as_ref()
            .filter(|(p, at, _)| p == prefix && at.elapsed() < Duration::from_secs(10))
            .map(|(_, _, keys)| keys.clone());
        let peers = match cached {
            Some(keys) => keys,
            None => {
                let keys = self.list_peers(prefix).await;
                *self.listed_peers.lock().expect("listed peers") =
                    Some((prefix.to_string(), Instant::now(), keys.clone()));
                keys
            }
        };
        let mut all: std::collections::BTreeMap<String, (u64, String)> = self
            .store
            .list(prefix)
            .into_iter()
            .map(|(k, o)| (k, (o.size, o.hash)))
            .collect();
        for (k, size, hash) in peers {
            all.entry(k).or_insert((size, hash));
        }
        all.into_iter().map(|(k, (s, h))| (k, s, h)).collect()
    }

    /// Keep connections to the group open, so a lookup does not pay for
    /// dialing (every 20 s; until the process stops).
    pub async fn keep_warm(self: Arc<Self>) {
        loop {
            for p in self.members() {
                let me = self.clone();
                tokio::spawn(async move {
                    if tokio::time::timeout(Duration::from_secs(10), me.conn(&p))
                        .await
                        .is_ok_and(|r| r.is_ok())
                    {
                        me.down.lock().expect("down").remove(&p.id);
                    }
                });
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }
}

/// The objects' ETag: their BLAKE3, quoted (a strong tag).
pub fn etag(hash: &str) -> String {
    format!("\"{hash}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicked_fetch_does_not_wedge_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), 1 << 20).unwrap();
        let k = Kache::new(store, Arc::new(Vec::new), Timeouts::default());
        let r = k
            .flight("key", async { panic!("a peer fetch panicked") })
            .await;
        assert!(r.is_err());
        assert!(
            k.inflight.lock().unwrap().is_empty(),
            "the key is free again"
        );
        assert_eq!(k.flight("key", async { Ok(()) }).await, Ok(()));
    }
}
