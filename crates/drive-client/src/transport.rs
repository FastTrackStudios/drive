//! What carries a [`Drive`]'s messages: a [`Transport`] opens
//! [`Connection`]s, each telling its [`Wire`] when it opens, what arrives
//! and when it closes. The client reconnects through the same transport
//! with a fresh wire whenever one closes.
//!
//! - [`WebSocket`]: to a node (`wss://host/sync`), in a browser or natively.
//! - natively, with the `p2p` feature, `crate::peers`: this device's own
//!   rooms, synced with the account's other devices over iroh.

use crate::Drive;

/// `Send + Sync` natively; nothing in a browser (its sockets are the
/// page's, on its one thread).
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSend for T {}

/// Something that opens connections for a [`Drive`].
pub trait Transport: MaybeSend + 'static {
    /// Start a connection, reporting on `wire`; `None` when it could not
    /// even start (the client tries again a while later).
    fn open(&self, wire: Wire) -> Option<Box<dyn Connection>>;
    /// What it connects to, for logs (`ws://…`, `peers`).
    fn describe(&self) -> String;
}

/// One connection a [`Transport`] opened.
pub trait Connection: MaybeSend {
    /// Whether it is open now (sends succeed).
    fn is_open(&self) -> bool;
    /// Send one frame (an encoded `drive_proto::ToNode`); `false` when it
    /// is not taken.
    fn send(&self, frame: Vec<u8>) -> bool;
    /// Close it (its wire then hears [`Wire::closed`]).
    fn close(&self);
}

/// Where a connection reports: the client it belongs to, and which of its
/// connections it is (what an older one says is ignored).
#[derive(Clone)]
pub struct Wire {
    pub(crate) drive: Drive,
    pub(crate) generation: u32,
}

impl Wire {
    /// It opened: the client says hello.
    pub fn opened(&self) {
        self.drive.socket_opened(self.generation);
    }

    /// A frame arrived (an encoded `drive_proto::ToClient`).
    pub fn message(&self, frame: &[u8]) {
        self.drive.socket_message(self.generation, frame);
    }

    /// It closed, or never opened: the client is offline, and tries again.
    pub fn closed(&self) {
        self.drive.socket_closed(self.generation);
    }
}

/// A WebSocket to a node.
pub struct WebSocket {
    url: String,
}

impl WebSocket {
    /// To the node at `url` (`wss://host/sync`).
    pub fn new(url: impl Into<String>) -> WebSocket {
        WebSocket { url: url.into() }
    }

    /// To the node this page came from (a browser), or natively the one
    /// `DRIVE_NODE` or this computer's agent names (default
    /// `ws://127.0.0.1:8096/sync`).
    pub fn default_node() -> WebSocket {
        WebSocket::new(crate::platform::default_url())
    }
}

impl Transport for WebSocket {
    fn open(&self, wire: Wire) -> Option<Box<dyn Connection>> {
        crate::platform::Socket::open(&self.url, wire.drive, wire.generation)
            .map(|s| Box::new(s) as Box<dyn Connection>)
    }

    fn describe(&self) -> String {
        self.url.clone()
    }
}

impl Connection for crate::platform::Socket {
    fn is_open(&self) -> bool {
        crate::platform::Socket::is_open(self)
    }
    fn send(&self, frame: Vec<u8>) -> bool {
        crate::platform::Socket::send(self, frame)
    }
    fn close(&self) {
        crate::platform::Socket::close(self)
    }
}
