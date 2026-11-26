//! This device as a peer (the `p2p` feature): an app keeps rooms of its
//! own (`drive-sync`, its copies in a folder) and its [`Drive`] connects
//! to them in-process; the rooms sync with the account's other devices
//! over iroh, and with a node when one is named, which is then just
//! another peer that keeps copies. Nothing needs the Drive app, an agent
//! or a node.
//!
//! ```no_run
//! use drive_client::{Drive, peers::PeerOptions};
//! let (drive, events, peers) = Drive::peer_to_peer(PeerOptions::default(), None);
//! // `peers.dial_ticket(..)` another device's `peers.ticket()`. Until
//! // devices are vouched for by their account (drive#183), the device
//! // dialed must let the dialer in: list it in `PeerOptions::peers`, or
//! // trust the network (`PeerOptions::trusting_the_network`).
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use drive_proto::{ToClient, ToNode};
use drive_sync::frames::Rx;
use drive_sync::net::Net;
pub use drive_sync::net::{Allow, EndpointAddr, EndpointId, Settings};
use drive_sync::{Access, DirStore, Hello, Rooms};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::{Connection, Drive, EventStream, Transport, Wire, lock, platform};

/// How this device takes part.
pub struct PeerOptions {
    /// Where its copies for its peers and its key are kept (default:
    /// `peers` in the client's data folder, [`crate::data_dir`]).
    pub dir: Option<PathBuf>,
    /// Its relay (`Settings::relay`: ours; empty: n0's public ones), or
    /// direct addresses only.
    pub settings: Settings,
    /// Advertise on the local network, and dial the devices seen there
    /// that [`PeerOptions::allow`] lets in.
    pub mdns: bool,
    /// Devices to keep a connection to (from pairing, or the account's
    /// directory): always let in.
    pub peers: Vec<EndpointAddr>,
    /// Who else may connect, and be dialed when seen nearby. `None`: only
    /// the devices in `peers` and those dialed since.
    pub allow: Option<Allow>,
    /// A node to keep copies on too (`wss://host/sync`): one more peer.
    pub node: Option<String>,
    /// Other protocols this device's endpoint speaks, behind the same door
    /// as its documents: the graph's (`drive_graph::share::protocol`).
    pub protocols: Vec<drive_sync::net::Protocol>,
}

impl Default for PeerOptions {
    fn default() -> Self {
        PeerOptions {
            dir: None,
            settings: Settings::default(),
            mdns: true,
            peers: Vec::new(),
            allow: None,
            node: None,
            protocols: Vec::new(),
        }
    }
}

impl PeerOptions {
    /// Let in every Drive device on the network: for a network you trust,
    /// until devices are vouched for by their account (drive#183).
    pub fn trusting_the_network(mut self) -> Self {
        self.allow = Some(Arc::new(|_| true));
        self
    }
}

/// This device's peer: its rooms and its endpoint.
#[derive(Clone)]
pub struct Peers(Arc<Inner>);

struct Inner {
    rooms: Arc<Rooms>,
    net: OnceLock<Net>,
    /// Dialed before the endpoint was up: dialed once it is.
    waiting: Mutex<Vec<EndpointAddr>>,
    /// Devices always let in (listed, or dialed).
    trusted: Arc<Mutex<HashSet<EndpointId>>>,
    /// Why the endpoint did not start, if it did not.
    error: Mutex<Option<String>>,
}

impl Peers {
    /// Start this device's peer: its rooms now (a [`Drive`] can connect at
    /// once), its endpoint a moment later on the client's runtime. `name`:
    /// who it says it is to its peers.
    pub fn start(opts: PeerOptions, name: &str, token: Option<String>) -> Peers {
        let dir = opts
            .dir
            .clone()
            .or_else(|| crate::data_dir().map(|d| d.join("peers")))
            .unwrap_or_else(|| std::env::temp_dir().join("drive-peers"));
        let rooms = Arc::new(Rooms::new(Arc::new(DirStore::new(dir.join("docs")))));
        let trusted: HashSet<EndpointId> = opts.peers.iter().map(|a| a.id).collect();
        let peers = Peers(Arc::new(Inner {
            rooms: rooms.clone(),
            net: OnceLock::new(),
            waiting: Mutex::new(opts.peers.clone()),
            trusted: Arc::new(Mutex::new(trusted)),
            error: Mutex::new(None),
        }));
        platform::spawn(rooms.clone().flush_loop());
        let hello = Hello {
            name: name.to_string(),
            token,
        };
        if let Some(url) = opts.node.clone() {
            platform::spawn(node_link(rooms.clone(), url, hello.clone()));
        }
        let me = peers.clone();
        platform::spawn(async move {
            let started = async {
                let key = drive_net::secret_key(&dir.join("iroh.key"))?;
                let trusted = me.0.trusted.clone();
                let more = opts.allow.clone();
                let allow: Allow = Arc::new(move |id| {
                    lock(&trusted).contains(id) || more.as_ref().is_some_and(|a| a(id))
                });
                let all: Access = Arc::new(|_, _| true);
                Net::start(
                    rooms,
                    drive_sync::net::Options {
                        key,
                        settings: opts.settings,
                        mdns: opts.mdns,
                        allow,
                        access: all,
                        hello,
                        protocols: opts.protocols,
                    },
                )
                .await
            }
            .await;
            match started {
                Ok(net) => {
                    let waiting = std::mem::take(&mut *lock(&me.0.waiting));
                    for a in waiting {
                        net.dial(a);
                    }
                    let _ = me.0.net.set(net);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "this device's peer did not start: it syncs with no other device");
                    *lock(&me.0.error) = Some(e);
                }
            }
        });
        peers
    }

    /// What a [`Drive`] connects through to reach these rooms.
    pub fn transport(&self) -> Local {
        Local(self.0.rooms.clone())
    }

    /// The rooms themselves (to answer from, or to read without a window).
    pub fn rooms(&self) -> &Arc<Rooms> {
        &self.0.rooms
    }

    /// This device's endpoint id, once its endpoint is up.
    pub fn id(&self) -> Option<EndpointId> {
        self.0.net.get().map(Net::id)
    }

    /// Where it can be reached now, once its endpoint is up.
    pub fn addr(&self) -> Option<EndpointAddr> {
        self.0.net.get().map(Net::addr)
    }

    /// This device's address as text, to pair another device with
    /// ([`Peers::dial_ticket`] there).
    pub fn ticket(&self) -> Option<String> {
        self.addr().map(|a| drive_net::addr_to_string(&a))
    }

    /// Why its endpoint did not start, if it did not.
    pub fn error(&self) -> Option<String> {
        lock(&self.0.error).clone()
    }

    /// This device's endpoint, once it has started: to ask the connected
    /// devices over another protocol (the graph's).
    pub fn endpoint(&self) -> Option<drive_net::Endpoint> {
        self.0.net.get().map(|n| n.endpoint().clone())
    }

    /// The devices connected now.
    pub fn connected(&self) -> Vec<EndpointId> {
        self.0.net.get().map(Net::connected).unwrap_or_default()
    }

    /// Keep a connection to the device at `addr` (let in from now on).
    pub fn dial(&self, addr: EndpointAddr) {
        lock(&self.0.trusted).insert(addr.id);
        match self.0.net.get() {
            Some(net) => {
                let net = net.clone();
                platform::spawn(async move { net.dial(addr) });
            }
            None => lock(&self.0.waiting).push(addr),
        }
    }

    /// [`Peers::dial`] another device's [`Peers::ticket`] (or its bare
    /// endpoint id, found by mDNS or the relay).
    pub fn dial_ticket(&self, ticket: &str) -> Result<(), String> {
        self.dial(drive_net::addr_from_str(ticket.trim())?);
        Ok(())
    }

    /// Save every changed document here now (before quitting).
    pub async fn flush(&self) {
        let rooms = self.0.rooms.clone();
        platform::on_runtime(async move { rooms.flush_now().await }).await;
    }
}

impl Drive {
    /// A client of this device's own peer ([`Peers`]): documents open at
    /// once from this device's copies and sync with every reachable
    /// device of the account, with no node. `token`: sent to a node named
    /// in `opts.node`.
    pub fn peer_to_peer(opts: PeerOptions, token: Option<String>) -> (Drive, EventStream, Peers) {
        let name = platform::load_me()
            .and_then(|j| serde_json::from_str::<crate::Me>(&j).ok())
            .map(|m| m.name)
            .unwrap_or_else(crate::guest_name);
        let peers = Peers::start(opts, &name, token.clone());
        let (drive, events) = Drive::connect_via(peers.transport(), token);
        (drive, events, peers)
    }
}

/// A [`Transport`] to rooms in this process.
pub struct Local(Arc<Rooms>);

struct LocalConn {
    tx: Mutex<Option<mpsc::UnboundedSender<ToNode>>>,
    open: Arc<AtomicBool>,
}

impl Transport for Local {
    fn open(&self, wire: Wire) -> Option<Box<dyn Connection>> {
        let (tx, rx) = mpsc::unbounded_channel::<ToNode>();
        let (out, mut out_rx) = mpsc::unbounded_channel::<ToClient>();
        let open = Arc::new(AtomicBool::new(true));
        let all: Access = Arc::new(|_, _| true);
        platform::spawn(drive_sync::serve(self.0.clone(), Rx(rx), out, None, all));
        let flag = open.clone();
        platform::spawn(async move {
            wire.opened();
            while let Some(m) = out_rx.recv().await {
                wire.message(&drive_proto::encode(&m));
            }
            flag.store(false, Ordering::SeqCst);
            wire.closed();
        });
        Some(Box::new(LocalConn {
            tx: Mutex::new(Some(tx)),
            open,
        }))
    }

    fn describe(&self) -> String {
        "peers".into()
    }
}

impl Connection for LocalConn {
    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn send(&self, frame: Vec<u8>) -> bool {
        let Ok(m) = drive_proto::decode::<ToNode>(&frame) else {
            return false;
        };
        lock(&self.tx).as_ref().is_some_and(|tx| tx.send(m).is_ok())
    }

    fn close(&self) {
        lock(&self.tx).take();
    }
}

/// Keep these rooms linked to a node over its WebSocket, reconnecting
/// with a backoff.
async fn node_link(rooms: Arc<Rooms>, url: String, hello: Hello) {
    use tokio_tungstenite::tungstenite::Message;
    let mut wait = Duration::from_millis(500);
    loop {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        match tokio_tungstenite::connect_async_with_config(
            url.as_str(),
            Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(drive_proto::MAX_MESSAGE))
                    .max_frame_size(Some(drive_proto::MAX_MESSAGE)),
            ),
            false,
        )
        .await
        {
            Ok((ws, _)) => {
                wait = Duration::from_millis(500);
                let (mut sink, stream) = ws.split();
                let (up, mut up_rx) = mpsc::unbounded_channel::<ToNode>();
                let writer = tokio::spawn(async move {
                    while let Some(m) = up_rx.recv().await {
                        if sink
                            .send(Message::binary(drive_proto::encode(&m)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                let incoming = Box::pin(
                    stream
                        .take_while(|m| std::future::ready(m.is_ok()))
                        .filter_map(|m| {
                            std::future::ready(match m {
                                Ok(Message::Binary(b)) => drive_proto::decode::<ToClient>(&b).ok(),
                                _ => None,
                            })
                        }),
                );
                drive_sync::link(rooms.clone(), url.clone(), hello.clone(), incoming, up).await;
                writer.abort();
            }
            Err(e) => tracing::debug!(drive.url = %url, error = %e, "the node did not answer"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::LoroDoc;

    fn text(doc: &LoroDoc, key: &str) -> Option<String> {
        doc.get_map("m")
            .get(key)
            .and_then(|v| v.into_value().ok())
            .and_then(|v| v.into_string().ok())
            .map(|s| s.to_string())
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..250 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never: {what}");
    }

    fn device(dir: &std::path::Path, peers: Vec<EndpointAddr>) -> (Drive, EventStream, Peers) {
        // The device dialed lets everyone in; the dialer only its peer.
        let trust = peers.is_empty();
        let opts = PeerOptions {
            dir: Some(dir.to_path_buf()),
            settings: Settings {
                relay: None,
                local_only: true,
            },
            mdns: false,
            peers,
            ..Default::default()
        };
        let opts = if trust {
            opts.trusting_the_network()
        } else {
            opts
        };
        Drive::peer_to_peer(opts, None)
    }

    async fn up(p: &Peers) -> EndpointAddr {
        for _ in 0..250 {
            if let Some(a) = p.addr()
                && !a.addrs.is_empty()
            {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the endpoint never came up: {:?}", p.error());
    }

    /// Two apps, each with its own peer, no node anywhere: an edit on one
    /// shows on the other live; edits made apart converge.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_apps_sync_a_document_with_no_node() {
        crate::tests::copies();
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, _ea, pa) = device(da.path(), Vec::new());
        let addr = up(&pa).await;
        let (b, _eb, pb) = device(db.path(), vec![addr]);
        up(&pb).await;

        let id = "home/test:p2p";
        let doc_a = a.open(id);
        let doc_b = b.open(id);
        doc_a.get_map("m").insert("k", "from a").unwrap();
        a.commit(id);
        until("b sees a's edit", || {
            text(&doc_b, "k").as_deref() == Some("from a")
        })
        .await;
        doc_b.get_map("m").insert("j", "from b").unwrap();
        b.commit(id);
        until("a sees b's edit", || {
            text(&doc_a, "j").as_deref() == Some("from b")
        })
        .await;
        assert_eq!(pb.connected(), vec![pa.id().unwrap()]);
    }

    /// Apps on their own hand each other the graph's derived data: one
    /// mounts `drive/graph/1` beside its documents, the other asks the
    /// devices it is connected to.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_apps_hand_each_other_the_graphs_data() {
        use drive_graph::db::GraphDb;
        use drive_graph::extract::{Sidecar, extract};
        use drive_graph::share;
        crate::tests::copies();
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let made = std::sync::Arc::new(GraphDb::memory().await.unwrap());
        made.put_extracts(&[("h1".into(), extract("[[Elsewhere]]"))])
            .await
            .unwrap();
        let opts = PeerOptions {
            dir: Some(da.path().to_path_buf()),
            settings: Settings {
                relay: None,
                local_only: true,
            },
            mdns: false,
            protocols: vec![share::protocol(share::Answerer {
                db: made,
                images: None,
            })],
            ..Default::default()
        }
        .trusting_the_network();
        let (_a, _ea, pa) = Drive::peer_to_peer(opts, None);
        let addr = up(&pa).await;
        let (_b, _eb, pb) = device(db.path(), vec![addr]);
        up(&pb).await;
        until("connected", || !pb.connected().is_empty()).await;

        let endpoint = pb.endpoint().unwrap();
        let peers: Vec<EndpointAddr> = pb.connected().into_iter().map(EndpointAddr::new).collect();
        let askable = share::Askable {
            endpoint: &endpoint,
            peers: &peers,
            patience: Duration::from_secs(4),
        };
        let bytes = askable.extract("h1").await.expect("an answer");
        assert_eq!(
            Sidecar::decode(&bytes, "h1").unwrap(),
            extract("[[Elsewhere]]")
        );
    }

    /// A node is one more peer: what an app makes reaches it, and a client
    /// of the node (the Drive app) sees it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_named_keeps_a_copy() {
        crate::tests::copies();
        let (url, _data) = crate::tests::node().await;
        let dir = tempfile::tempdir().unwrap();
        let (app, _e, _p) = Drive::peer_to_peer(
            PeerOptions {
                dir: Some(dir.path().to_path_buf()),
                settings: Settings {
                    relay: None,
                    local_only: true,
                },
                mdns: false,
                node: Some(url.clone()),
                ..Default::default()
            },
            None,
        );
        let id = "home/test:kept";
        let doc = app.open(id);
        doc.get_map("m").insert("k", "saved to Drive").unwrap();
        app.commit(id);

        let (viewer, _ev) = Drive::connect(Some(url));
        let seen = viewer.open(id);
        until("the node's client sees it", || {
            text(&seen, "k").as_deref() == Some("saved to Drive")
        })
        .await;
    }
}
