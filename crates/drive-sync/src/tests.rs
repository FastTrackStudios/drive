//! Peers in one process, linked over channels: what every transport
//! carries, without one.

use std::sync::Arc;
use std::time::Duration;

use drive_proto::{ToClient, ToNode};
use loro::{ExportMode, LoroDoc};
use tokio::sync::mpsc;

use crate::{Access, Hello, MemStore, Rooms, link, serve};

use crate::frames::Rx;

fn rooms() -> Arc<Rooms> {
    Arc::new(Rooms::new(Arc::new(MemStore::default())))
}

fn all() -> Access {
    Arc::new(|_, _| true)
}

/// A window on `rooms`: the client side of [`serve`], by hand.
struct Window {
    to: mpsc::UnboundedSender<ToNode>,
    from: mpsc::UnboundedReceiver<ToClient>,
    doc: LoroDoc,
}

impl Window {
    async fn open(rooms: &Arc<Rooms>, id: &str) -> Window {
        let (to, rx) = mpsc::unbounded_channel();
        let (out, from) = mpsc::unbounded_channel();
        tokio::spawn(serve(rooms.clone(), Rx(rx), out, None, all()));
        let mut w = Window {
            to,
            from,
            doc: LoroDoc::new(),
        };
        w.send(ToNode::Hello {
            version: drive_proto::VERSION,
            name: "w".into(),
            token: None,
        });
        w.send(ToNode::Join {
            doc: id.into(),
            version_vector: w.doc.oplog_vv().encode(),
        });
        w.pump().await;
        w
    }

    fn send(&self, m: ToNode) {
        self.to.send(m).unwrap();
    }

    /// Take in what has arrived.
    async fn pump(&mut self) {
        tokio::time::sleep(Duration::from_millis(30)).await;
        while let Ok(m) = self.from.try_recv() {
            if let ToClient::Synced { bytes, .. } | ToClient::Update { bytes, .. } = m
                && !bytes.is_empty()
            {
                self.doc.import(&bytes).unwrap();
            }
        }
    }

    /// Set `key` in the document's map and send the edit.
    fn edit(&self, id: &str, key: &str, value: &str) {
        let before = self.doc.oplog_vv();
        self.doc.get_map("m").insert(key, value).unwrap();
        self.doc.commit();
        let bytes = self.doc.export(ExportMode::updates(&before)).unwrap();
        self.send(ToNode::Update {
            doc: id.into(),
            bytes,
        });
    }

    fn get(&self, key: &str) -> Option<String> {
        self.doc
            .get_map("m")
            .get(key)
            .and_then(|v| v.into_value().ok())
            .and_then(|v| v.into_string().ok())
            .map(|s| s.to_string())
    }
}

/// Link `a` to `b` (as `a`'s client of `b`) over channels; aborting the
/// returned task ends the link.
fn connect(
    a: &Arc<Rooms>,
    a_name: &str,
    b: &Arc<Rooms>,
    b_name: &str,
) -> tokio::task::JoinHandle<()> {
    let (up, up_rx) = mpsc::unbounded_channel::<ToNode>();
    let (down, down_rx) = mpsc::unbounded_channel::<ToClient>();
    // Ending the link drops `up`: the serving side sees its input end,
    // as it would a dropped connection.
    tokio::spawn(serve(
        b.clone(),
        Rx(up_rx),
        down,
        Some(a_name.into()),
        all(),
    ));
    tokio::spawn(link(
        a.clone(),
        b_name.into(),
        Hello {
            name: a_name.into(),
            token: None,
        },
        Rx(down_rx),
        up,
    ))
}

async fn until(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

#[tokio::test]
async fn two_devices_linked_see_each_others_edits_live() {
    let (a, b) = (rooms(), rooms());
    let id = "home/test:doc";
    let mut wa = Window::open(&a, id).await;
    let mut wb = Window::open(&b, id).await;
    let _l = connect(&a, "A", &b, "B");

    wa.edit(id, "k", "from a");
    until("b sees a's edit", async || {
        wb.pump().await;
        wb.get("k").as_deref() == Some("from a")
    })
    .await;
    wb.edit(id, "j", "from b");
    until("a sees b's edit", async || {
        wa.pump().await;
        wa.get("j").as_deref() == Some("from b")
    })
    .await;
}

#[tokio::test]
async fn edits_made_apart_converge_when_linked() {
    let (a, b) = (rooms(), rooms());
    let id = "home/test:apart";
    let mut wa = Window::open(&a, id).await;
    let mut wb = Window::open(&b, id).await;
    wa.edit(id, "a", "1");
    wb.edit(id, "b", "2");
    wa.pump().await;
    wb.pump().await;
    assert_eq!(wa.get("b"), None, "not linked yet");

    let link = connect(&a, "A", &b, "B");
    until("both have both", async || {
        wa.pump().await;
        wb.pump().await;
        wa.get("b").as_deref() == Some("2") && wb.get("a").as_deref() == Some("1")
    })
    .await;

    // Apart again, then back.
    link.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    wa.edit(id, "a", "3");
    wb.edit(id, "b", "4");
    wa.pump().await;
    wb.pump().await;
    assert_eq!(wa.get("b").as_deref(), Some("2"), "apart");
    let _l = connect(&a, "A", &b, "B");
    until("converged again", async || {
        wa.pump().await;
        wb.pump().await;
        wa.get("b").as_deref() == Some("4") && wb.get("a").as_deref() == Some("3")
    })
    .await;
}

#[tokio::test]
async fn devices_linked_both_ways_in_a_ring_settle() {
    // Three devices, each linked to the others both ways: an update must
    // reach everyone and then stop (no echo round for ever).
    let (a, b, c) = (rooms(), rooms(), rooms());
    let id = "home/test:ring";
    let mut wa = Window::open(&a, id).await;
    let mut wc = Window::open(&c, id).await;
    let _wb = Window::open(&b, id).await;
    let _links = [
        connect(&a, "A", &b, "B"),
        connect(&b, "B", &a, "A"),
        connect(&b, "B", &c, "C"),
        connect(&c, "C", &b, "B"),
        connect(&c, "C", &a, "A"),
        connect(&a, "A", &c, "C"),
    ];
    tokio::time::sleep(Duration::from_millis(100)).await;
    wa.pump().await;
    wc.pump().await;
    wa.edit(id, "k", "v");
    until("c sees it", async || {
        wc.pump().await;
        wc.get("k").as_deref() == Some("v")
    })
    .await;
    // Settled: nothing more arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while wc.from.try_recv().is_ok() {}
    while wa.from.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(wc.from.try_recv().is_err(), "c hears nothing more");
    assert!(wa.from.try_recv().is_err(), "a hears nothing more");
}

#[tokio::test]
async fn presence_crosses_a_link_and_goes_when_it_ends() {
    let (a, b) = (rooms(), rooms());
    let id = "home/test:here";
    let wa = Window::open(&a, id).await;
    let mut wb = Window::open(&b, id).await;
    let l = connect(&a, "A", &b, "B");
    tokio::time::sleep(Duration::from_millis(50)).await;
    wa.send(ToNode::Presence {
        doc: id.into(),
        json: "{\"at\":1}".into(),
    });
    let mut seen = None;
    until("b sees a's presence", async || {
        while let Ok(m) = wb.from.try_recv() {
            if let ToClient::Presence { json, .. } = m {
                seen = Some(json);
            }
        }
        seen.as_ref()
            .is_some_and(|j| j.as_deref() == Some("{\"at\":1}"))
    })
    .await;
    l.abort();
    until("a's presence goes from b", async || {
        while let Ok(m) = wb.from.try_recv() {
            if let ToClient::Presence { json, .. } = m {
                seen = Some(json);
            }
        }
        seen == Some(None)
    })
    .await;
}

#[tokio::test]
async fn a_peer_refused_a_document_gets_nothing_of_it() {
    let a = rooms();
    a.edit("home/test:secret", "me", |d| {
        d.get_map("m").insert("k", "v").unwrap();
    })
    .await;
    let (to, rx) = mpsc::unbounded_channel();
    let (out, mut from) = mpsc::unbounded_channel();
    let only_mine: Access = Arc::new(|id, _| id.starts_with("mine/"));
    tokio::spawn(serve(a.clone(), Rx(rx), out, Some("X".into()), only_mine));
    to.send(ToNode::Hello {
        version: drive_proto::VERSION,
        name: "x".into(),
        token: None,
    })
    .unwrap();
    to.send(ToNode::Join {
        doc: "home/test:secret".into(),
        version_vector: LoroDoc::new().oplog_vv().encode(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut got = Vec::new();
    while let Ok(m) = from.try_recv() {
        got.push(m);
    }
    assert!(matches!(got[0], ToClient::Welcome { .. }));
    assert!(
        matches!(&got[1], ToClient::Error { message } if message.contains("not allowed")),
        "{got:?}"
    );
    assert_eq!(got.len(), 2);
}

/// The first milestone: two devices in one process, each with its own
/// rooms, over iroh (direct addresses, no relay), no node anywhere.
#[cfg(feature = "iroh")]
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_sync_over_iroh_with_no_node() {
    use crate::net::{Net, Options, SecretKey, Settings};

    let local = Settings {
        relay: None,
        local_only: true,
    };
    let start = async |rooms: &Arc<Rooms>, name: &str| {
        Net::start(
            rooms.clone(),
            Options {
                key: SecretKey::generate(),
                settings: local.clone(),
                mdns: false,
                allow: Arc::new(|_| true),
                access: all(),
                hello: Hello {
                    name: name.into(),
                    token: None,
                },
                protocols: Vec::new(),
            },
        )
        .await
        .unwrap()
    };
    let (a, b) = (rooms(), rooms());
    let id = "home/test:iroh";
    let mut wa = Window::open(&a, id).await;
    let mut wb = Window::open(&b, id).await;
    wa.edit(id, "before", "made apart");
    let (na, nb) = (start(&a, "A").await, start(&b, "B").await);
    na.dial(nb.addr());

    until("b has what a made before they met", async || {
        wb.pump().await;
        wb.get("before").as_deref() == Some("made apart")
    })
    .await;
    wb.edit(id, "live", "from b");
    until("a sees b's edit live", async || {
        wa.pump().await;
        wa.get("live").as_deref() == Some("from b")
    })
    .await;
    assert_eq!(nb.connected(), vec![na.id()]);
    na.shutdown().await;
    nb.shutdown().await;
}

/// A device `allow` does not let in gets nothing.
#[cfg(feature = "iroh")]
#[tokio::test(flavor = "multi_thread")]
async fn a_device_not_allowed_is_refused() {
    use crate::net::{Net, Options, SecretKey, Settings};

    let local = Settings {
        relay: None,
        local_only: true,
    };
    let (a, b) = (rooms(), rooms());
    let id = "home/test:closed";
    b.edit(id, "me", |d| {
        d.get_map("m").insert("k", "secret").unwrap();
    })
    .await;
    let mut wa = Window::open(&a, id).await;
    let opts = |allow: crate::net::Allow| Options {
        key: SecretKey::generate(),
        settings: local.clone(),
        mdns: false,
        allow,
        access: all(),
        hello: Hello::default(),
        protocols: Vec::new(),
    };
    let na = Net::start(a.clone(), opts(Arc::new(|_| true)))
        .await
        .unwrap();
    let nb = Net::start(b.clone(), opts(Arc::new(|_| false)))
        .await
        .unwrap();
    na.dial(nb.addr());
    tokio::time::sleep(Duration::from_millis(800)).await;
    wa.pump().await;
    assert_eq!(wa.get("k"), None);
    assert!(nb.connected().is_empty());
    na.shutdown().await;
    nb.shutdown().await;
}

#[cfg(feature = "iroh")]
mod another_protocol {
    use super::*;

    /// Another protocol beside documents: an echo, behind the same door.
    #[derive(Debug)]
    struct Echo;

    impl iroh::protocol::ProtocolHandler for Echo {
        async fn accept(
            &self,
            conn: iroh::endpoint::Connection,
        ) -> Result<(), iroh::protocol::AcceptError> {
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let got = recv.read_to_end(1024).await.unwrap_or_default();
                let _ = send.write_all(&got).await;
                let _ = send.finish();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn another_protocol_is_spoken_behind_the_same_door() {
        use crate::net::{Net, Options, SecretKey, Settings};
        const ECHO: &[u8] = b"test/echo/1";
        let local = Settings {
            relay: None,
            local_only: true,
        };
        let allowed = SecretKey::generate();
        let allowed_id = allowed.public();
        let opts = Options {
            key: SecretKey::generate(),
            settings: local.clone(),
            mdns: false,
            allow: Arc::new(move |id| *id == allowed_id),
            access: all(),
            hello: Hello::default(),
            protocols: vec![(
                ECHO.to_vec(),
                Box::new(Echo) as Box<dyn iroh::protocol::DynProtocolHandler>,
            )],
        };
        let net = Net::start(rooms(), opts).await.unwrap();
        let ask = async |key: SecretKey| -> Result<Vec<u8>, String> {
            let ep = drive_net::endpoint(key, &local, vec![]).await?;
            let conn = ep
                .connect(net.addr(), ECHO)
                .await
                .map_err(|e| e.to_string())?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            send.write_all(b"hello").await.map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            recv.read_to_end(1024).await.map_err(|e| e.to_string())
        };
        assert_eq!(ask(allowed).await.unwrap(), b"hello");
        assert!(
            ask(SecretKey::generate()).await.is_err(),
            "refused at the door"
        );
        net.shutdown().await;
    }
}
