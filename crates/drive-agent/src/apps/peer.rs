//! Asking another computer for a file: one small protocol on each agent's
//! iroh endpoint (ALPN `drive/apps/1`), behind the same door as its files
//! (only the space's devices and nodes get in at the handshake).
//!
//! One request per stream, JSON both ways:
//!
//! ```text
//! {"provide_location":{"location":"<id>","path":"Kits/kick.wav"}}
//! {"provide_path":{"path":"/Volumes/Samples/kick.wav"}}
//! {"blob":"<blake3>"}
//!   -> {"hash":"<blake3>","size":N,"mtime":MS} | {"err":"…"}
//! ```
//!
//! An answer means the file is fetchable from that device now as an
//! iroh-blobs blob named by its full BLAKE3, served read-only in place
//! (docs/transfer.md). Each request is confined exactly as the node's are:
//! a location's file inside its folder, a browsed file inside one of the
//! device's roots (never `/proc`, `/sys`, `/dev` or the agent's folder), a
//! blob only from the agent's own caches.

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler};
use serde::{Deserialize, Serialize};

use super::Apps;
use super::cache::valid_hash;
use crate::agent::Agent;

pub const ALPN: &[u8] = b"drive/apps/1";
const MAX_REQUEST: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Ask {
    ProvideLocation { location: String, path: String },
    ProvidePath { path: String },
    Blob(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

/// The protocol, for `drive_transfer::Provider::start_with`.
pub fn protocol(a: &Arc<Agent>, apps: &Arc<Apps>) -> drive_transfer::Extra {
    (
        ALPN.to_vec(),
        Box::new(Handler {
            a: Arc::downgrade(a),
            apps: Arc::downgrade(apps),
        }) as Box<dyn DynProtocolHandler>,
    )
}

struct Handler {
    a: Weak<Agent>,
    apps: Weak<Apps>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("apps::Handler")
    }
}

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let (a, apps) = (self.a.clone(), self.apps.clone());
            tokio::spawn(async move {
                let Ok(req) = recv.read_to_end(MAX_REQUEST).await else {
                    return;
                };
                let answer = match (serde_json::from_slice::<Ask>(&req), a.upgrade()) {
                    (Ok(ask), Some(a)) => answer(&a, apps.upgrade().as_deref(), ask).await,
                    (Err(_), _) => Answer {
                        err: Some("a request this agent does not understand".into()),
                        ..Default::default()
                    },
                    (_, None) => Answer {
                        err: Some("the agent is stopping".into()),
                        ..Default::default()
                    },
                };
                let bytes = serde_json::to_vec(&answer).expect("an answer serializes");
                let _ = send.write_all(&bytes).await;
                let _ = send.finish();
            });
        }
        Ok(())
    }
}

async fn answer(a: &Agent, apps: Option<&Apps>, ask: Ask) -> Answer {
    let r = match &ask {
        Ask::ProvideLocation { location, path } => a.provide_location(location, path).await,
        Ask::ProvidePath { path } => provide_path(a, path).await,
        Ask::Blob(hash) => provide_blob(a, apps, hash).await,
    };
    match r {
        Ok(p) => Answer {
            hash: Some(p.hash),
            size: p.size,
            mtime: p.mtime,
            err: None,
        },
        Err(e) => {
            tracing::debug!(apps.ask = ?ask, error = %e, "a peer's ask for a file was refused");
            Answer {
                err: Some(e),
                ..Default::default()
            }
        }
    }
}

/// A file of one of this device's disks, by its absolute path: confined to
/// the root it is under, as browsing is.
async fn provide_path(a: &Agent, path: &str) -> Result<drive_proto::Provided, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("not an absolute path".into());
    }
    let root = a
        .browse_roots()
        .into_iter()
        .filter(|r| p.starts_with(&r.mount))
        .max_by_key(|r| r.mount.len())
        .ok_or("not under any disk of this computer")?;
    let rel = p
        .strip_prefix(&root.mount)
        .map_err(|_| "not under that disk")?
        .to_string_lossy()
        .into_owned();
    a.provide_browsed(&a.browse_rules(), &root.mount, &rel)
        .await
}

/// A blob this device keeps in its own caches (the Drive tree's, or the
/// app cache).
async fn provide_blob(
    a: &Agent,
    apps: Option<&Apps>,
    hash: &str,
) -> Result<drive_proto::Provided, String> {
    if !valid_hash(hash) {
        return Err("not a content hash".into());
    }
    let tree = a.cache_path(hash);
    let file = if tree.is_file() {
        tree
    } else if let Some((p, _)) = apps.and_then(|x| x.cache.get(hash)) {
        p
    } else {
        return Err("not kept here".into());
    };
    let p = a
        .provider
        .get()
        .ok_or("this device does not serve files over iroh yet")?;
    let got = p.provide(&file).await?;
    if got.hash != hash {
        return Err("the kept copy no longer matches its hash".into());
    }
    Ok(drive_proto::Provided {
        hash: got.hash,
        size: got.size,
        mtime: got.mtime,
        endpoint: drive_transfer::addr_to_string(&p.addr()),
    })
}

impl Apps {
    async fn peer_conn(&self, addr: &drive_transfer::EndpointAddr) -> Result<Connection, String> {
        if let Some(c) = self.conns.lock().expect("conns").get(&addr.id)
            && c.close_reason().is_none()
        {
            return Ok(c.clone());
        }
        let ep = self
            .a
            .provider
            .get()
            .ok_or(
                "this device does not serve files over iroh, so it cannot reach other computers",
            )?
            .endpoint()
            .clone();
        let c = tokio::time::timeout(Duration::from_secs(10), ep.connect(addr.clone(), ALPN))
            .await
            .map_err(|_| "it could not be reached in time".to_string())?
            .map_err(|e| format!("connecting: {e}"))?;
        self.conns.lock().expect("conns").insert(addr.id, c.clone());
        Ok(c)
    }

    /// Ask device `device` to make a file fetchable.
    pub async fn ask(
        &self,
        device: &str,
        ask: &Ask,
    ) -> Result<(drive_transfer::EndpointAddr, Answer), String> {
        let addr = self.device_addr(device)?;
        let r = async {
            let c = self.peer_conn(&addr).await?;
            let (mut send, mut recv) = c.open_bi().await.map_err(|e| e.to_string())?;
            send.write_all(&serde_json::to_vec(ask).expect("an ask serializes"))
                .await
                .map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            // Answering may hash the file there first (once per version).
            let bytes =
                tokio::time::timeout(Duration::from_secs(300), recv.read_to_end(MAX_REQUEST))
                    .await
                    .map_err(|_| "no answer in time".to_string())?
                    .map_err(|e| e.to_string())?;
            serde_json::from_slice::<Answer>(&bytes).map_err(|e| e.to_string())
        }
        .await;
        match r {
            Ok(ans) => Ok((addr, ans)),
            Err(e) => {
                self.conns.lock().expect("conns").remove(&addr.id);
                Err(e)
            }
        }
    }

    /// Where a device's endpoint is (the registry's last word).
    pub fn device_addr(&self, device: &str) -> Result<drive_transfer::EndpointAddr, String> {
        let (id, addr) = drive_files::space::endpoints(&self.a.space)
            .remove(device)
            .ok_or("its agent does not serve files over iroh (an older agent, or never started)")?;
        let eid: drive_transfer::EndpointId = id.parse().map_err(|_| "a bad endpoint id")?;
        let addr = drive_transfer::addr_from_str(&addr)
            .unwrap_or_else(|_| drive_transfer::EndpointAddr::new(eid));
        Ok(addr)
    }
}
