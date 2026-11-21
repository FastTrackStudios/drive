//! Files from devices over iroh (docs/transfer.md).
//!
//! The node has an iroh endpoint of its own (its key in `data/iroh.key`),
//! published in every space registry it keeps (`nodes`), which is how
//! devices know to let it in. When a device's registry entry has an
//! endpoint, the node asks it over the agent's connection to serve a file
//! (`Browse::Provide` / `ProvideLocation`: the device hashes it the first
//! time) and then fetches exactly the byte ranges an HTTP client asks for,
//! verified chunk by chunk, streaming them as they come. Devices without
//! one (agents from before) keep the old paths: `/fetch` through a whole
//! upload, browsing through 4 MiB reads over the agent's connection.
//!
//! What `/fetch`, share links and the zip used to keep on the node they
//! still keep: a response that is the whole file is written to the blob
//! store as it streams (a blob only once its hash checks out), and a range
//! starts a background fetch of the whole file, one per blob at a time.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use drive_files::blob::BlobStore;
use drive_proto::{Browse, Browsed, Provided};
use drive_transfer::{Fetcher, Settings, Source};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;

use crate::rooms::{FetchError, Rooms};

/// How long a device may take to start serving a file (it hashes it the
/// first time: a few seconds a GB from a fast disk, more from a slow one).
const PROVIDE: Duration = Duration::from_secs(600);

pub struct Transfer {
    pub fetcher: Arc<Fetcher>,
    /// Blobs being written to the store now.
    filling: Mutex<HashSet<String>>,
}

impl Transfer {
    pub async fn start(data: &Path, settings: &Settings) -> Result<Transfer, String> {
        let fetcher = Fetcher::start(&data.join("iroh.key"), settings).await?;
        tracing::info!(transfer.endpoint = %fetcher.id().fmt_short(), "fetching files from devices over iroh");
        Ok(Transfer {
            fetcher: Arc::new(fetcher),
            filling: Mutex::new(HashSet::new()),
        })
    }

    pub fn id(&self) -> String {
        self.fetcher.id().to_string()
    }
}

/// Say in a space's registry that this node keeps it (once).
pub async fn publish(rooms: &Rooms, space: &str) {
    let Some(t) = rooms.transfer.get() else {
        return;
    };
    let id = t.id();
    let doc = drive_files::space_doc(space);
    let known = rooms
        .read(&doc, |d| {
            drive_files::space::node_endpoints(d).contains(&id)
        })
        .await;
    if !known {
        rooms
            .edit(&doc, "node", |d| {
                drive_files::space::set_node_endpoint(d, &id, drive_files::now_ms())
            })
            .await
            .ok();
    }
}

/// The device of a location on its own disk (not in a storage).
pub async fn location_device(rooms: &Rooms, space: &str, location: &str) -> Option<String> {
    rooms
        .read(&drive_files::space_doc(space), |d| {
            drive_files::space::locations(d)
                .into_iter()
                .find(|l| l.id == location && l.storage.is_empty())
                .map(|l| l.device)
        })
        .await
}

/// Ask `device` to serve a file over iroh. `Ok(None)`: it cannot (no
/// endpoint in the registry: an agent from before, or its endpoint did
/// not start); the caller takes the old path.
pub async fn provide(
    rooms: &Rooms,
    space: &str,
    device: &str,
    ask: Browse,
) -> Result<Option<(Provided, Source)>, FetchError> {
    if rooms.transfer.get().is_none() {
        return Ok(None);
    }
    let known = rooms
        .read(&drive_files::space_doc(space), |d| {
            drive_files::space::endpoints(d).contains_key(device)
        })
        .await;
    if !known {
        return Ok(None);
    }
    match rooms.browse(space, device, ask, PROVIDE).await {
        Ok(Browsed::Provided(p)) => {
            let addr = drive_transfer::addr_from_str(&p.endpoint).map_err(FetchError::Refused)?;
            let hash = p
                .hash
                .parse()
                .map_err(|_| FetchError::Refused("not a blob hash".into()))?;
            let src = Source {
                addr,
                hash,
                size: p.size,
            };
            Ok(Some((p, src)))
        }
        Ok(_) => Err(FetchError::Refused("an answer of the wrong kind".into())),
        Err(FetchError::Refused(why))
            if why.contains("does not serve files over iroh") || why.contains("in a storage") =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// What the response says about the file.
pub struct Reply {
    pub name: String,
    /// Keep what is fetched in the blob store (`/fetch`, share links).
    pub cache: Option<BlobStore>,
}

/// The HTTP answer: the bytes a `Range` asks for (or all of them), fetched
/// from the device as the client reads them.
pub fn respond(
    t: &Arc<Transfer>,
    p: &Provided,
    src: Source,
    headers: &HeaderMap,
    reply: Reply,
) -> Response {
    let size = p.size;
    let range = headers.get("range").and_then(|v| v.to_str().ok());
    let (start, end, partial) = match range {
        _ if size == 0 => (0, 0, false),
        Some(h) => match crate::byte_range(h, size) {
            Some((a, b)) => (a, b, true),
            None => {
                let mut r = Response::new(Body::empty());
                *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                    r.headers_mut().insert("content-range", v);
                }
                return r;
            }
        },
        None => (0, size - 1, false),
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    let whole = !partial;
    let body = if len == 0 {
        Body::empty()
    } else {
        let stream = measured(
            t,
            &src,
            start,
            len,
            t.fetcher.stream(src.clone(), start, end),
        );
        match reply.cache {
            Some(blobs) if whole => Body::from_stream(tee(t.clone(), src, blobs, stream)),
            Some(blobs) => {
                fill(t.clone(), src, blobs);
                Body::from_stream(stream)
            }
            None => Body::from_stream(stream),
        }
    };
    let mut r = Response::new(body);
    if partial {
        *r.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
            r.headers_mut().insert("content-range", v);
        }
    }
    let safe: String = reply
        .name
        .chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(drive_files::tree::mime_of(&safe)) {
        h.insert("content-type", v);
    }
    // A device's file never shows as a page of the node's own.
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert("content-disposition", v);
    }
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(
        "cache-control",
        HeaderValue::from_static("private, max-age=60"),
    );
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert("content-length", v);
    }
    r
}

/// The stream, with one event when it ends: how many bytes, how fast,
/// and whether the connection was direct or through a relay.
fn measured(
    t: &Arc<Transfer>,
    src: &Source,
    start: u64,
    len: u64,
    s: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    struct Done {
        t: Arc<Transfer>,
        peer: drive_transfer::EndpointId,
        began: Instant,
        first: Option<Duration>,
        got: u64,
        start: u64,
        len: u64,
    }
    impl Drop for Done {
        fn drop(&mut self) {
            let ms = self.began.elapsed().as_millis() as u64;
            tracing::info!(
                transfer.peer = %self.peer.fmt_short(),
                transfer.path = self.t.fetcher.path_kind(&self.peer),
                transfer.start = self.start,
                transfer.asked = self.len,
                transfer.bytes = self.got,
                transfer.first_byte_ms = self.first.map(|d| d.as_millis() as u64),
                transfer.ms = ms,
                transfer.outcome = if self.got == self.len { "done" } else { "stopped" },
                "a range came from a device over iroh"
            );
        }
    }
    let mut done = Done {
        t: t.clone(),
        peer: src.addr.id,
        began: Instant::now(),
        first: None,
        got: 0,
        start,
        len,
    };
    s.map(move |item| {
        // The whole of `done` lives in the closure (its drop is the event).
        let d = &mut done;
        if let Ok(b) = &item {
            if d.first.is_none() {
                d.first = Some(d.began.elapsed());
            }
            d.got += b.len() as u64;
        }
        item
    })
}

/// The stream, also written to the blob store as it passes; kept as a blob
/// only when every byte came and the hash checks out. A client that leaves
/// early leaves a background fetch of the rest in its place.
pub fn tee(
    t: Arc<Transfer>,
    src: Source,
    blobs: BlobStore,
    s: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(32);
    let hash = src.hash.to_hex().to_string();
    tokio::spawn(async move {
        if !t.filling.lock().await.insert(hash.clone()) {
            return;
        }
        let ok = match blobs.writer(&hash).await {
            Ok(mut w) => {
                let mut n = 0u64;
                let mut ok = true;
                while let Some(b) = rx.recv().await {
                    n += b.len() as u64;
                    if w.write(b).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                if ok && n == src.size {
                    match w.finish().await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(blob.hash = %hash, error = %e, "a fetched file was not kept");
                            false
                        }
                    }
                } else {
                    w.abandon().await;
                    false
                }
            }
            Err(_) => false,
        };
        t.filling.lock().await.remove(&hash);
        if !ok && !blobs.has(&hash).await.unwrap_or(false) {
            fill(t, src, blobs);
        }
    });
    s.then(move |item| {
        let tx = tx.clone();
        async move {
            if let Ok(b) = &item {
                let _ = tx.send(b.clone()).await;
            }
            item
        }
    })
}

/// Fetch the whole file into the blob store in the background (once per
/// blob at a time).
pub fn fill(t: Arc<Transfer>, src: Source, blobs: BlobStore) {
    tokio::spawn(async move {
        let hash = src.hash.to_hex().to_string();
        if blobs.has(&hash).await.unwrap_or(false) || !t.filling.lock().await.insert(hash.clone()) {
            return;
        }
        let result = async {
            let mut w = blobs.writer(&hash).await.map_err(|e| e.to_string())?;
            let mut s =
                std::pin::pin!(t.fetcher.stream(src.clone(), 0, src.size.saturating_sub(1)));
            while let Some(b) = s.next().await {
                match b {
                    Ok(b) => {
                        if let Err(e) = w.write(b).await {
                            w.abandon().await;
                            return Err(e.to_string());
                        }
                    }
                    Err(e) => {
                        w.abandon().await;
                        return Err(e.to_string());
                    }
                }
            }
            w.finish().await.map_err(|e| e.to_string())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(blob.hash = %hash, error = %e, "a file from a device was not kept on the node");
        }
        t.filling.lock().await.remove(&hash);
    });
}
