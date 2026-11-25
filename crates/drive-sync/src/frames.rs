//! The exchange over a byte stream (a QUIC stream, a pipe): each message
//! a frame, its length (4 bytes, big-endian) then its `drive-proto`
//! encoding.

use std::sync::Arc;

use drive_proto::{ToClient, ToNode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::{Access, Hello, Rooms};

/// The largest frame read (a document's whole history in one update).
pub const MAX_FRAME: usize = 256 << 20;

/// Read one frame (`None`: the stream ended cleanly).
pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("a frame of {len} bytes")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Write one frame.
pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

/// The messages read off `r`, decoded, until it ends or fails (one that
/// does not decode is skipped: a newer peer's message).
fn incoming<T: DeserializeOwned + Send + 'static>(
    mut r: impl AsyncRead + Unpin + Send + 'static,
) -> impl futures::Stream<Item = T> + Unpin + Send {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut r).await {
            match drive_proto::decode::<T>(&frame) {
                Ok(m) => {
                    if tx.send(m).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::debug!(error = %e, "a frame did not decode"),
            }
        }
    });
    Rx(rx)
}

/// What is sent on the returned channel goes out on `w`, framed, until
/// the channel closes or `w` fails.
fn outgoing<T: Serialize + Send + 'static>(
    mut w: impl AsyncWrite + Unpin + Send + 'static,
) -> (mpsc::UnboundedSender<T>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<T>();
    let task = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write_frame(&mut w, &drive_proto::encode(&m)).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });
    (tx, task)
}

/// [`crate::serve`] over a byte stream.
pub async fn serve_stream(
    rooms: Arc<Rooms>,
    r: impl AsyncRead + Unpin + Send + 'static,
    w: impl AsyncWrite + Unpin + Send + 'static,
    via: Option<String>,
    access: Access,
) {
    let (out, writer) = outgoing::<ToClient>(w);
    crate::serve(rooms, incoming::<ToNode>(r), out, via, access).await;
    let _ = writer.await;
}

/// [`crate::link`] over a byte stream.
pub async fn link_stream(
    rooms: Arc<Rooms>,
    via: String,
    hello: Hello,
    r: impl AsyncRead + Unpin + Send + 'static,
    w: impl AsyncWrite + Unpin + Send + 'static,
) {
    let (up, writer) = outgoing::<ToNode>(w);
    crate::link(rooms, via, hello, incoming::<ToClient>(r), up).await;
    writer.abort();
}

/// An `UnboundedReceiver` as a `Stream`.
pub(crate) struct Rx<T>(pub mpsc::UnboundedReceiver<T>);

impl<T> futures::Stream for Rx<T> {
    type Item = T;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}
