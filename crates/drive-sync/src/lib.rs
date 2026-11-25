//! `drive-sync`: Drive's document sync, whatever carries it.
//!
//! [`Rooms`] holds each open document (a room), who has it open, and their
//! presence. It is one more peer of every document: it merges what it is
//! sent, keeps it (in a [`Store`]), and passes on what was new to it; it
//! never decides who wins. A `drive-node` runs one for everyone it
//! serves; an app runs one of its own to answer its other devices from
//! its own copies.
//!
//! The exchange is `drive-proto`'s ([`ToNode`] / [`ToClient`]), over any
//! stream of frames:
//! - [`serve`] answers one connection whose other end is the client (a
//!   window of this app, another device, a browser);
//! - [`link`] is the other side: these rooms joining every document they
//!   have open on another peer's (a node, another device), and pumping
//!   updates and presence both ways.
//!
//! Updates are passed on only for what was new to a room (what an import
//! added to its version), so peers linked in a ring or a mesh settle
//! instead of echoing an update round for ever; and at most once per
//! device (a device both linked to and linking in hears each update once).
//!
//! With the `iroh` feature, [`net`] carries all this between devices over
//! iroh (ALPN [`net::ALPN`]), found by mDNS nearby and through a relay.

pub mod frames;
mod link;
mod rooms;
mod serve;
mod store;

#[cfg(feature = "iroh")]
pub mod net;

pub use drive_proto::{DocId, DocInfo, Peer, ToClient, ToNode};
pub use link::{Hello, link};
pub use rooms::{IDLE, Outbox, Rooms};
pub use serve::{Access, serve};
pub use store::{BoxFuture, DirStore, MemStore, Store};

#[cfg(test)]
mod tests;
