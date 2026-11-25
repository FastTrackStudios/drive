//! A standalone app's own peer (the `p2p` feature, native): its documents
//! sync with the account's other devices over iroh, with no Drive app,
//! agent or node. Call [`peer_to_peer`] before launching; `DriveProvider`
//! then connects to this device's peer instead of a node.

use std::sync::{Mutex, OnceLock};

pub use drive_client::peers::{EndpointAddr, EndpointId, PeerOptions, Peers, Settings};
use drive_client::{Drive, EventStream};

static OPTS: Mutex<Option<PeerOptions>> = Mutex::new(None);
static PEERS: OnceLock<Peers> = OnceLock::new();

/// Sync this app's documents peer to peer (call before `launch`). With
/// `opts.node` unset, nothing is asked of a node: the app opens at once
/// on this device's copies.
pub fn peer_to_peer(opts: PeerOptions) {
    *OPTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(opts);
}

/// This device's peer, once `DriveProvider` has started it: its ticket to
/// pair with, the devices connected, dialing another.
pub fn peers() -> Option<Peers> {
    PEERS.get().cloned()
}

/// Whether the app syncs peer to peer.
pub(crate) fn on() -> bool {
    PEERS.get().is_some() || OPTS.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Whether it does so with no node at all (nothing to ask at boot).
pub(crate) fn standalone() -> bool {
    on() && OPTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_none_or(|o| o.node.is_none())
}

/// A client of this device's peer (started the first time).
pub(crate) fn connect(token: Option<String>) -> Option<(Drive, EventStream)> {
    if let Some(p) = PEERS.get() {
        return Some(Drive::connect_via(p.transport(), token));
    }
    let opts = OPTS.lock().unwrap_or_else(|e| e.into_inner()).take()?;
    let (drive, events, peers) = Drive::peer_to_peer(opts, token);
    let _ = PEERS.set(peers);
    Some((drive, events))
}
