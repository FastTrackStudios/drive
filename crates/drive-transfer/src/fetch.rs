//! Fetching byte ranges of a blob from the device that has it.
//!
//! [`Fetcher::stream`] asks for exactly the chunks that cover a byte range
//! (iroh-blobs rounds to 16 KiB chunk groups) and yields only bytes whose
//! BLAKE3 tree checked out against the blob's hash, trimmed to the range.
//! When the connection drops mid-way it dials again and asks for the rest
//! from the last byte it delivered, a few times before giving up.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bao_tree::io::BaoContentItem;
use bytes::Bytes;
use iroh::endpoint::Connection;
use iroh_blobs::get::fsm;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, GetRequest};

use crate::{EndpointAddr, EndpointId, Hash, Settings};

/// Tries per range after a failure that delivered nothing new.
const TRIES: u32 = 5;
/// Verified chunks buffered between the connection and the reader.
const BUFFER: usize = 16;

/// Where a blob is: the device's address, the blob's hash and size.
#[derive(Clone, Debug)]
pub struct Source {
    pub addr: EndpointAddr,
    pub hash: Hash,
    pub size: u64,
}

/// A dialing endpoint, keeping one connection per device it talks to.
pub struct Fetcher {
    endpoint: iroh::Endpoint,
    conns: Mutex<HashMap<EndpointId, Connection>>,
}

impl Fetcher {
    /// An endpoint with the key in `key_file` (made the first time).
    pub async fn start(key_file: &std::path::Path, settings: &Settings) -> Result<Fetcher, String> {
        let key = crate::secret_key(key_file)?;
        let endpoint = crate::endpoint(key, settings, Vec::new()).await?;
        Ok(Fetcher {
            endpoint,
            conns: Mutex::new(HashMap::new()),
        })
    }

    /// Dial from an endpoint that already exists (a device's provider), so
    /// the peers see this device's own id.
    pub fn on(endpoint: iroh::Endpoint) -> Fetcher {
        Fetcher {
            endpoint,
            conns: Mutex::new(HashMap::new()),
        }
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    async fn conn(&self, addr: &EndpointAddr) -> Result<Connection, String> {
        if let Some(c) = self.conns.lock().expect("conns").get(&addr.id)
            && c.close_reason().is_none()
        {
            return Ok(c.clone());
        }
        let c = tokio::time::timeout(
            Duration::from_secs(20),
            self.endpoint.connect(addr.clone(), iroh_blobs::ALPN),
        )
        .await
        .map_err(|_| "the device could not be reached in time".to_string())?
        .map_err(|e| format!("connecting: {e}"))?;
        self.conns.lock().expect("conns").insert(addr.id, c.clone());
        Ok(c)
    }

    fn forget(&self, id: &EndpointId) {
        self.conns.lock().expect("conns").remove(id);
    }

    /// Close every connection now (as a network drop would).
    pub fn drop_connections(&self) {
        for (_, c) in self.conns.lock().expect("conns").drain() {
            c.close(0u32.into(), b"dropped");
        }
    }

    /// How the connection to a device goes: `direct`, `relay`, or `none`.
    pub fn path_kind(&self, id: &EndpointId) -> &'static str {
        let Some(c) = self.conns.lock().expect("conns").get(id).cloned() else {
            return "none";
        };
        let paths = c.paths();
        let mut kind = "none";
        for p in paths.iter() {
            if p.is_selected() {
                kind = if p.is_relay() { "relay" } else { "direct" };
            }
        }
        kind
    }

    /// Bytes `start..=end` of the blob, verified, as they arrive.
    pub fn stream(
        self: &Arc<Self>,
        src: Source,
        start: u64,
        end: u64,
    ) -> impl futures::Stream<Item = io::Result<Bytes>> + Send + 'static {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(BUFFER);
        let me = self.clone();
        tokio::spawn(async move {
            me.run(src, start, end, tx).await;
        });
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
    }

    async fn run(
        &self,
        src: Source,
        mut pos: u64,
        end: u64,
        tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    ) {
        if src.size == 0 || pos > end {
            return;
        }
        let end = end.min(src.size - 1);
        let mut tries = 0;
        while pos <= end {
            let before = pos;
            let res = self.once(&src, &mut pos, end, &tx).await;
            if tx.is_closed() {
                return;
            }
            let why = match res {
                Ok(()) if pos > end => return,
                Ok(()) => "the device sent less than asked".to_string(),
                Err(e) => e,
            };
            if pos > before {
                tries = 0;
            }
            tries += 1;
            self.forget(&src.addr.id);
            if tries > TRIES {
                tracing::warn!(
                    transfer.outcome = "failed",
                    transfer.peer = %src.addr.id.fmt_short(),
                    transfer.at = pos,
                    transfer.why = %why,
                    "a range could not be fetched from its device"
                );
                let _ = tx.send(Err(io::Error::other(why))).await;
                return;
            }
            tracing::debug!(transfer.at = pos, transfer.why = %why, transfer.try = tries, "resuming a range");
            tokio::time::sleep(Duration::from_millis(200 * u64::from(tries))).await;
        }
    }

    /// One request for `pos..=end`; advances `pos` past every byte sent.
    async fn once(
        &self,
        src: &Source,
        pos: &mut u64,
        end: u64,
        tx: &tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    ) -> Result<(), String> {
        let conn = self.conn(&src.addr).await?;
        let request = GetRequest::blob_ranges(src.hash, ChunkRanges::bytes(*pos..end + 1));
        let connected = fsm::start(conn, request, Default::default())
            .next()
            .await
            .map_err(|e| e.to_string())?;
        let fsm::ConnectedNext::StartRoot(root) =
            connected.next().await.map_err(|e| e.to_string())?
        else {
            return Err("an answer of the wrong kind".into());
        };
        let (mut content, size) = root.next().next().await.map_err(|e| e.to_string())?;
        if size != src.size {
            return Err("the file changed on its device".into());
        }
        loop {
            match content.next().await {
                fsm::BlobContentNext::More((next, item)) => {
                    match item.map_err(|e| e.to_string())? {
                        BaoContentItem::Leaf(leaf) => {
                            let (a, b) = (leaf.offset, leaf.offset + leaf.data.len() as u64);
                            let from = (*pos).max(a);
                            let to = b.min(end + 1);
                            if from < to {
                                let piece = leaf.data.slice((from - a) as usize..(to - a) as usize);
                                if tx.send(Ok(piece)).await.is_err() {
                                    return Ok(());
                                }
                                *pos = to;
                            }
                        }
                        BaoContentItem::Parent(_) => {}
                    }
                    content = next;
                }
                fsm::BlobContentNext::Done(done) => {
                    if let fsm::EndBlobNext::Closing(c) = done.next() {
                        let _ = c.next().await;
                    }
                    return Ok(());
                }
            }
        }
    }
}
