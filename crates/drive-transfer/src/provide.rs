//! Serving this device's files, read-only and in place.
//!
//! A file is added to the blob store with `ImportMode::TryReference`: the
//! store opens it for reading, computes its BLAKE3 outboard into the
//! store's own folder, and from then on reads the file where it is. The
//! store never writes, renames or deletes a referenced file (its deletes
//! only touch its own folder), push requests (a peer writing into the
//! store) are disabled, and the caller confines which files may be served.
//!
//! Nothing in this module opens a file for writing (the
//! `provider_never_writes` test reads this source to keep it so); what is
//! remembered about served files is `served.rs`'s, in the agent's folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iroh::endpoint::{Accepting, Connection};
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router};
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobFormat, BlobsProtocol};

use crate::served::{Line, Served};
use crate::{EndpointAddr, EndpointId, Settings};

/// Who may connect: called with each peer's endpoint id at the handshake.
pub type Allow = Arc<dyn Fn(&EndpointId) -> bool + Send + Sync>;

/// A file ready to be fetched: its full BLAKE3 hash (hex), size and time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provided {
    pub hash: String,
    pub size: u64,
    /// Modified, in ms since 1970.
    pub mtime: i64,
}

/// This device's endpoint serving its files.
pub struct Provider {
    router: Router,
    store: FsStore,
    served: Served,
}

/// Another protocol the endpoint speaks beside the blobs (its ALPN and its
/// handler), behind the same door: the kache lookups (`drive-kache`).
pub type Extra = (Vec<u8>, Box<dyn DynProtocolHandler>);

/// A protocol behind a door: only peers `allow` lets in.
struct Gate {
    inner: Box<dyn DynProtocolHandler>,
    allow: Allow,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

impl ProtocolHandler for Gate {
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let conn = accepting.await?;
        let who = conn.remote_id();
        if !(self.allow)(&who) {
            tracing::warn!(
                transfer.outcome = "refused",
                transfer.peer = %who.fmt_short(),
                "an endpoint that is not one of this org's devices or its node asked for files"
            );
            conn.close(1u32.into(), b"not one of this org's devices");
            return Err(AcceptError::from_err(std::io::Error::other(
                "not one of this org's devices",
            )));
        }
        Ok(conn)
    }

    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        self.inner.accept(conn).await
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await
    }
}

fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

impl Provider {
    /// Start serving: the endpoint (its key in `dir/iroh.key`), the blob
    /// store (outboards in `dir/blobs`), and what was hashed before.
    pub async fn start(dir: &Path, settings: &Settings, allow: Allow) -> Result<Provider, String> {
        Provider::start_with(dir, settings, allow, Vec::new()).await
    }

    /// [`Provider::start`], also speaking `extra` protocols, each behind
    /// the same door as the blobs.
    pub async fn start_with(
        dir: &Path,
        settings: &Settings,
        allow: Allow,
        extra: Vec<Extra>,
    ) -> Result<Provider, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let key = crate::secret_key(&dir.join("iroh.key"))?;
        let store = FsStore::load(dir.join("blobs"))
            .await
            .map_err(|e| format!("blob store: {e}"))?;
        let mut alpns = vec![iroh_blobs::ALPN.to_vec()];
        alpns.extend(extra.iter().map(|(alpn, _)| alpn.clone()));
        let endpoint = crate::endpoint(key, settings, alpns).await?;
        // Read-only: the default events refuse pushes (a peer writing into
        // the store) and observing; every get is served.
        let blobs = BlobsProtocol::new(&store, None);
        let mut router = Router::builder(endpoint).accept(
            iroh_blobs::ALPN,
            Gate {
                inner: blobs.into(),
                allow: allow.clone(),
            },
        );
        for (alpn, inner) in extra {
            router = router.accept(
                alpn,
                Gate {
                    inner,
                    allow: allow.clone(),
                },
            );
        }
        let router = router.spawn();
        Ok(Provider {
            router,
            store,
            served: Served::open(dir),
        })
    }

    /// This device's endpoint id.
    pub fn id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    /// The endpoint itself: to dial other devices from this device's own
    /// id ([`crate::Fetcher::on`]), which their doors know.
    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }

    /// Where it can be reached now (relay and direct addresses).
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Wait (up to `wait`) until the endpoint has a relay, so its address
    /// is dialable from anywhere.
    pub async fn online(&self, wait: std::time::Duration) {
        let _ = tokio::time::timeout(wait, self.router.endpoint().online()).await;
    }

    /// Make a file fetchable: its hash (computed the first time, and again
    /// when its size or time changed). `file` must already be confined by
    /// the caller (an absolute, resolved path it lets be read).
    pub async fn provide(&self, file: &Path) -> Result<Provided, String> {
        let file: PathBuf = file.to_path_buf();
        let meta = std::fs::metadata(&file).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a file".into());
        }
        let (size, mtime) = (meta.len(), mtime_ms(&meta));
        let key = file.to_string_lossy().into_owned();
        if let Some(hash) = self.served.get(&key, size, mtime)
            && let Ok(h) = hash.parse::<iroh_blobs::Hash>()
            && self.store.has(h).await.unwrap_or(false)
        {
            return Ok(Provided { hash, size, mtime });
        }
        let started = std::time::Instant::now();
        let tag = self
            .store
            .add_path_with_opts(AddPathOptions {
                path: file.clone(),
                format: BlobFormat::Raw,
                mode: ImportMode::TryReference,
            })
            .with_named_tag(format!("path:{key}"))
            .await
            .map_err(|e| format!("hashing {}: {e}", file.display()))?;
        // Changed while it was read: its hash is of no one version.
        let after = std::fs::metadata(&file).map_err(|e| e.to_string())?;
        if after.len() != size || mtime_ms(&after) != mtime {
            let _ = self.store.tags().delete(format!("path:{key}")).await;
            return Err("the file changed while it was hashed".into());
        }
        let hash = tag.hash.to_hex().to_string();
        tracing::debug!(
            transfer.size = size,
            transfer.hash_ms = started.elapsed().as_millis() as u64,
            "a file hashed for serving"
        );
        self.served.put(Line {
            path: key,
            size,
            mtime,
            hash: hash.clone(),
        });
        Ok(Provided { hash, size, mtime })
    }

    /// Stop serving a file this device no longer has (a cache object let
    /// go): its entry in the blob store and its remembered hash go; the
    /// file is not touched.
    pub async fn forget(&self, file: &Path) {
        let key = file.to_string_lossy().into_owned();
        self.served.remove(&key);
        let _ = self.store.tags().delete(format!("path:{key}")).await;
    }

    pub async fn shutdown(&self) {
        let _ = self.router.shutdown().await;
        let _ = self.store.shutdown().await;
    }
}
