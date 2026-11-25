//! Documents between devices over iroh: one endpoint per device, speaking
//! [`ALPN`]. Every connection carries the exchange both ways (each side
//! opens a stream on which it is the client, [`crate::link`], and answers
//! the other's, [`crate::serve`]), so each device joins the documents it
//! has open on the other.
//!
//! Peers are found three ways: dialed by address ([`Net::dial`]: a
//! ticket, the account's directory), seen nearby (mDNS), or dialing in.
//! Who may connect is the caller's rule ([`Allow`], by endpoint id),
//! checked at the handshake both ways.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures::StreamExt;
use iroh::endpoint::{Accepting, Connection};
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, Router};

pub use drive_net::{EndpointAddr, EndpointId, SecretKey, Settings};

use crate::frames::{link_stream, serve_stream};
use crate::{Access, Hello, Rooms};

/// The protocol's name on the wire.
pub const ALPN: &[u8] = b"drive/sync/1";

/// Who may connect, by endpoint id.
pub type Allow = Arc<dyn Fn(&EndpointId) -> bool + Send + Sync>;

/// Another protocol the endpoint speaks beside documents (its ALPN and
/// handler), behind the same door: the graph's derived data
/// (`drive_graph::share`).
pub type Protocol = (Vec<u8>, Box<dyn DynProtocolHandler>);

/// How a device takes part.
pub struct Options {
    /// Its key: its endpoint id is the public half.
    pub key: SecretKey,
    pub settings: Settings,
    /// Advertise on the local network and dial the devices seen there
    /// (those `allow` lets in).
    pub mdns: bool,
    /// Who may connect, and be dialed.
    pub allow: Allow,
    /// What a connected device may open and change.
    pub access: Access,
    /// Who this device says it is.
    pub hello: Hello,
    /// Other protocols to speak, each behind the same door (`allow`).
    pub protocols: Vec<Protocol>,
}

/// This device on the network.
#[derive(Clone)]
pub struct Net(Arc<Inner>);

struct Inner {
    rooms: Arc<Rooms>,
    router: Router,
    allow: Allow,
    access: Access,
    hello: Hello,
    state: Mutex<State>,
    _mdns: Option<drive_net::MdnsAddressLookup>,
}

#[derive(Default)]
struct State {
    /// Each connected device, and how many connections are up to it.
    connected: HashMap<EndpointId, usize>,
    /// Devices to keep a connection to (dialed again when it drops).
    wanted: HashSet<EndpointId>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
struct Handler(Weak<Inner>);

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("drive-sync")
    }
}

fn refuse(conn: &Connection) {
    tracing::warn!(
        sync.outcome = "refused",
        sync.peer = %conn.remote_id().fmt_short(),
        "a device that is not allowed connected for documents"
    );
    conn.close(1u32.into(), b"not allowed");
}

/// Another protocol behind the door: only devices `allow` lets in.
struct Gate {
    inner: Box<dyn DynProtocolHandler>,
    allow: Allow,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("drive-sync gate")
    }
}

impl ProtocolHandler for Gate {
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let conn = accepting.await?;
        if !(self.allow)(&conn.remote_id()) {
            refuse(&conn);
            return Err(AcceptError::from_err(std::io::Error::other("not allowed")));
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

impl ProtocolHandler for Handler {
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let conn = accepting.await?;
        let allowed = self
            .0
            .upgrade()
            .is_some_and(|i| (i.allow)(&conn.remote_id()));
        if !allowed {
            refuse(&conn);
            return Err(AcceptError::from_err(std::io::Error::other("not allowed")));
        }
        Ok(conn)
    }

    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if let Some(inner) = self.0.upgrade() {
            run(inner, conn).await;
        }
        Ok(())
    }
}

/// One connection, both ways, until it ends.
async fn run(inner: Arc<Inner>, conn: Connection) {
    let remote = conn.remote_id();
    let via = remote.to_string();
    *inner.state().connected.entry(remote).or_default() += 1;
    tracing::info!(sync.peer = %remote.fmt_short(), "a device connected for documents");
    // Its streams: it is their client.
    let serving = {
        let (conn, rooms, access, via) = (
            conn.clone(),
            inner.rooms.clone(),
            inner.access.clone(),
            via.clone(),
        );
        tokio::spawn(async move {
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(serve_stream(
                    rooms.clone(),
                    recv,
                    send,
                    Some(via.clone()),
                    access.clone(),
                ));
            }
        })
    };
    // Ours: we are its client.
    if let Ok((send, recv)) = conn.open_bi().await {
        link_stream(inner.rooms.clone(), via, inner.hello.clone(), recv, send).await;
    }
    conn.close(0u32.into(), b"done");
    serving.abort();
    let mut st = inner.state();
    if let Some(n) = st.connected.get_mut(&remote) {
        *n -= 1;
        if *n == 0 {
            st.connected.remove(&remote);
        }
    }
    tracing::info!(sync.peer = %remote.fmt_short(), "a device disconnected");
}

impl Net {
    /// Bring this device onto the network, answering from `rooms`.
    pub async fn start(rooms: Arc<Rooms>, opts: Options) -> Result<Net, String> {
        let mut alpns = vec![ALPN.to_vec()];
        alpns.extend(opts.protocols.iter().map(|(a, _)| a.clone()));
        let endpoint = drive_net::endpoint(opts.key, &opts.settings, alpns).await?;
        let mdns = if opts.mdns && !opts.settings.local_only {
            match drive_net::mdns(&endpoint) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS is off: devices nearby are not found");
                    None
                }
            }
        } else {
            None
        };
        let allow = opts.allow.clone();
        let protocols = opts.protocols;
        let inner = Arc::new_cyclic(|weak: &Weak<Inner>| Inner {
            rooms,
            router: protocols
                .into_iter()
                .fold(
                    Router::builder(endpoint).accept(ALPN, Handler(weak.clone())),
                    |r, (alpn, inner)| {
                        r.accept(
                            alpn,
                            Gate {
                                inner,
                                allow: allow.clone(),
                            },
                        )
                    },
                )
                .spawn(),
            allow: opts.allow,
            access: opts.access,
            hello: opts.hello,
            state: Mutex::new(State::default()),
            _mdns: mdns.clone(),
        });
        let net = Net(inner);
        if let Some(mdns) = mdns {
            let weak = Arc::downgrade(&net.0);
            tokio::spawn(async move {
                let mut events = mdns.subscribe().await;
                while let Some(e) = events.next().await {
                    let Some(net) = weak.upgrade().map(Net) else {
                        break;
                    };
                    if let drive_net::DiscoveryEvent::Discovered { endpoint_info, .. } = e {
                        let id = endpoint_info.endpoint_id;
                        if id != net.id() && (net.0.allow)(&id) {
                            net.dial(EndpointAddr::new(id));
                        }
                    }
                }
            });
        }
        Ok(net)
    }

    /// This device's endpoint id.
    pub fn id(&self) -> EndpointId {
        self.0.router.endpoint().id()
    }

    /// Where it can be reached now (relay and direct addresses).
    pub fn addr(&self) -> EndpointAddr {
        self.0.router.endpoint().addr()
    }

    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.0.router.endpoint()
    }

    /// The devices connected now.
    pub fn connected(&self) -> Vec<EndpointId> {
        self.0.state().connected.keys().copied().collect()
    }

    /// Keep a connection to the device at `addr`: dialed now, and again
    /// (backing off) whenever it drops, until [`Net::forget`].
    pub fn dial(&self, addr: EndpointAddr) {
        let id = addr.id;
        if id == self.id() || !self.0.state().wanted.insert(id) {
            return;
        }
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut wait = Duration::from_millis(500);
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let (wanted, connected) = {
                    let st = inner.state();
                    (st.wanted.contains(&id), st.connected.contains_key(&id))
                };
                if !wanted {
                    return;
                }
                // Already connected (it dialed us): look again later.
                if connected {
                    drop(inner);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                let endpoint = inner.router.endpoint().clone();
                match endpoint.connect(addr.clone(), ALPN).await {
                    Ok(conn) => {
                        if (inner.allow)(&conn.remote_id()) {
                            wait = Duration::from_millis(500);
                            run(inner, conn).await;
                        } else {
                            refuse(&conn);
                        }
                    }
                    Err(e) => {
                        tracing::debug!(sync.peer = %id.fmt_short(), error = %e, "a device did not answer");
                        drop(inner);
                    }
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        });
    }

    /// Stop keeping a connection to `id` (one up now stays until it drops).
    pub fn forget(&self, id: &EndpointId) {
        self.0.state().wanted.remove(id);
    }

    /// Close every connection and the endpoint.
    pub async fn shutdown(&self) {
        self.0.state().wanted.clear();
        let _ = self.0.router.shutdown().await;
    }
}
