//! Live listings (drive#159): a subscriber hears documents under its
//! prefix appear, change and go, only those it may see, until it stops
//! listening; a client of version 2 still lists by asking.

use std::sync::Arc;
use std::time::Duration;

use drive_proto::{DocInfo, ToClient, ToNode};
use tokio::sync::mpsc;

use super::{Rooms, session};
use crate::auth::{Auth, Principal};

struct Incoming(mpsc::UnboundedReceiver<ToNode>);

impl futures::Stream for Incoming {
    type Item = ToNode;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<ToNode>> {
        self.0.poll_recv(cx)
    }
}

const QUIET: Duration = Duration::from_millis(150);

async fn rooms(dir: &std::path::Path) -> Arc<Rooms> {
    let store = crate::store::Store::open(&dir.join("drive.db"))
        .await
        .unwrap();
    // An issuer nobody reaches: every token is one remembered here.
    Arc::new(Rooms::new(
        store,
        Auth::new(Some("http://127.0.0.1:9".into())),
    ))
}

struct Client {
    send: mpsc::UnboundedSender<ToNode>,
    recv: mpsc::UnboundedReceiver<ToClient>,
    done: tokio::task::JoinHandle<super::ConnectionEnd>,
}

impl Client {
    async fn connect(rooms: &Arc<Rooms>, who: &str, orgs: &[&str], version: u32) -> Client {
        let token = format!("token-{who}");
        let p = Principal {
            who: who.into(),
            orgs: orgs.iter().map(|o| o.to_string()).collect(),
            all: false,
        };
        rooms.auth.remember(&token, p).await;
        let (send, rx) = mpsc::unbounded_channel();
        let (out, recv) = mpsc::unbounded_channel();
        let recheck = Duration::from_secs(3600);
        let done = tokio::spawn(session(rooms.clone(), Incoming(rx), out, false, recheck));
        let mut c = Client { send, recv, done };
        c.send(ToNode::Hello {
            version,
            name: who.into(),
            token: Some(token),
        });
        assert!(matches!(c.next().await, Some(ToClient::Welcome { .. })));
        c
    }

    fn send(&self, m: ToNode) {
        self.send.send(m).unwrap();
    }

    /// The next message, if one comes soon.
    async fn next(&mut self) -> Option<ToClient> {
        tokio::time::timeout(Duration::from_secs(2), self.recv.recv())
            .await
            .ok()
            .flatten()
    }

    async fn join(&mut self, doc: &str) {
        self.send(ToNode::Join {
            doc: doc.into(),
            version_vector: Vec::new(),
        });
        assert!(matches!(self.next().await, Some(ToClient::Synced { .. })));
    }

    /// Subscribe, and the listing it is answered with.
    async fn subscribe(&mut self, prefix: &str) -> Vec<DocInfo> {
        self.send(ToNode::ListSubscribe {
            prefix: prefix.into(),
        });
        match self.listings().await.as_slice() {
            [ToClient::Docs { docs, .. }] => docs.clone(),
            other => panic!("one listing first: {other:?}"),
        }
    }

    /// Every listing message that arrives until it is quiet.
    async fn listings(&mut self) -> Vec<ToClient> {
        let mut out = Vec::new();
        while let Ok(Some(m)) = tokio::time::timeout(QUIET, self.recv.recv()).await {
            if matches!(m, ToClient::Docs { .. } | ToClient::Listed { .. }) {
                out.push(m);
            }
        }
        out
    }
}

/// The ids a run of deltas named: (entries, removed), each in order.
fn named(deltas: &[ToClient]) -> (Vec<String>, Vec<String>) {
    let mut entries = Vec::new();
    let mut removed = Vec::new();
    for m in deltas {
        let ToClient::Listed {
            docs, removed: r, ..
        } = m
        else {
            panic!("only deltas after the listing: {m:?}");
        };
        entries.extend(docs.iter().map(|d| d.id.clone()));
        removed.extend(r.iter().cloned());
    }
    entries.dedup();
    (entries, removed)
}

#[tokio::test]
async fn a_subscriber_hears_documents_appear_change_and_go_under_its_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    let mut alice = Client::connect(&rooms, "alice", &["a"], drive_proto::VERSION).await;
    let mut bob = Client::connect(&rooms, "bob", &["a"], drive_proto::VERSION).await;
    assert!(alice.subscribe("a/studio:").await.is_empty());

    // Made under the prefix: heard, with who has it open.
    bob.join("a/studio:song:x").await;
    let heard = alice.listings().await;
    assert_eq!(named(&heard), (vec!["a/studio:song:x".into()], vec![]));
    let ToClient::Listed { docs, .. } = heard.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(docs[0].peers, 1);

    // Outside it: nothing.
    bob.join("a/canvas:board").await;
    bob.join("a/studio2:x").await;
    assert!(alice.listings().await.is_empty());

    // Saved: its size is heard.
    let doc = loro::LoroDoc::new();
    doc.get_map("m").insert("k", "v").unwrap();
    doc.commit();
    bob.send(ToNode::Update {
        doc: "a/studio:song:x".into(),
        bytes: doc.export(loro::ExportMode::all_updates()).unwrap(),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    rooms.flush_now().await;
    let heard = alice.listings().await;
    let ToClient::Listed { docs, .. } = heard.last().expect("the save is heard") else {
        panic!("{heard:?}")
    };
    assert!(docs[0].bytes > 0);

    // Closed: nobody has it open; a stored one stays listed.
    bob.send(ToNode::Leave {
        doc: "a/studio:song:x".into(),
    });
    let heard = alice.listings().await;
    let ToClient::Listed { docs, .. } = heard.last().expect("the close is heard") else {
        panic!("{heard:?}")
    };
    assert_eq!(docs[0].peers, 0);
    rooms.evict_idle(Duration::ZERO).await;
    assert!(
        alice.listings().await.is_empty(),
        "still stored, still listed"
    );

    // One never saved goes once nobody has it open.
    bob.join("a/studio:draft").await;
    bob.send(ToNode::Leave {
        doc: "a/studio:draft".into(),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    rooms.evict_idle(Duration::ZERO).await;
    let (_, removed) = named(&alice.listings().await);
    assert_eq!(removed, ["a/studio:draft"]);
    // The listing as it now is agrees with every delta applied.
    let mut by_deltas = vec![];
    drive_proto::apply_listed(
        &mut by_deltas,
        rooms
            .listing(&crate::auth::Principal::everyone("t"), "a/studio:")
            .await,
        &[],
    );
    assert_eq!(
        by_deltas.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        ["a/studio:song:x"]
    );
}

#[tokio::test]
async fn a_subscriber_hears_only_what_it_may_see() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    // Church lends one song to band's members.
    rooms
        .edit(&drive_files::space_doc("church"), "t", |d| {
            let p = drive_files::projects::add_project(d, "Sunday", "Event").unwrap();
            drive_files::projects::link_doc(d, &p, "studio:song:lent", true).unwrap();
            drive_files::projects::share_with_org(d, &p, "band", true).unwrap();
        })
        .await;
    rooms.flush_now().await;
    let mut carol = Client::connect(&rooms, "carol", &["band"], drive_proto::VERSION).await;
    let mut pastor = Client::connect(&rooms, "pastor", &["church"], drive_proto::VERSION).await;
    assert!(carol.subscribe("*/studio:").await.is_empty());

    pastor.join("church/studio:song:private").await;
    pastor.join("church/studio:song:lent").await;
    // Not his org: refused, and so never made.
    pastor.send(ToNode::Join {
        doc: "band/studio:song:theirs".into(),
        version_vector: Vec::new(),
    });
    assert!(matches!(pastor.next().await, Some(ToClient::Error { .. })));
    let (entries, _) = named(&carol.listings().await);
    assert_eq!(entries, ["church/studio:song:lent"]);

    // Asked again per delta: the lend withdrawn, its changes are not heard.
    rooms
        .edit(&drive_files::space_doc("church"), "t", |d| {
            let p = drive_files::projects::projects(d)[0].id.clone();
            drive_files::projects::share_with_org(d, &p, "band", false).unwrap();
        })
        .await;
    let _ = carol.listings().await;
    pastor.send(ToNode::Leave {
        doc: "church/studio:song:lent".into(),
    });
    assert!(carol.listings().await.is_empty());
}

#[tokio::test]
async fn unsubscribing_or_disconnecting_stops_the_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    let mut alice = Client::connect(&rooms, "alice", &["a"], drive_proto::VERSION).await;
    let mut bob = Client::connect(&rooms, "bob", &["a"], drive_proto::VERSION).await;
    alice.subscribe("a/").await;
    assert_eq!(rooms.sync.listening(), 1);

    // A prefix changed: the old one is dropped, the new one listed.
    alice.send(ToNode::ListUnsubscribe {
        prefix: "a/".into(),
    });
    assert!(alice.subscribe("a/canvas:").await.is_empty());
    bob.join("a/studio:x").await;
    assert!(
        alice.listings().await.is_empty(),
        "not under the new prefix"
    );
    alice.send(ToNode::ListUnsubscribe {
        prefix: "a/canvas:".into(),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        rooms.sync.listening(),
        0,
        "listing nothing, hearing nothing"
    );
    bob.join("a/canvas:y").await;
    assert!(alice.listings().await.is_empty());

    // A connection that ends lets go of its listings.
    alice.subscribe("a/").await;
    assert_eq!(rooms.sync.listening(), 1);
    drop(alice.send);
    tokio::time::timeout(Duration::from_secs(2), alice.done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rooms.sync.listening(), 0);
}

#[tokio::test]
async fn a_version_2_client_still_lists_by_asking_and_is_sent_no_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path()).await;
    let mut old = Client::connect(&rooms, "old", &["a"], 2).await;
    let mut bob = Client::connect(&rooms, "bob", &["a"], drive_proto::VERSION).await;
    bob.join("a/studio:x").await;
    old.send(ToNode::List {
        prefix: "a/studio:".into(),
    });
    let heard = old.listings().await;
    let [ToClient::Docs { docs, .. }] = heard.as_slice() else {
        panic!("{heard:?}")
    };
    assert_eq!(docs.len(), 1);
    bob.join("a/studio:y").await;
    assert!(
        old.listings().await.is_empty(),
        "nothing it did not ask for"
    );
    // A version from before 2 is still refused.
    let (send, rx) = mpsc::unbounded_channel();
    let (out, mut recv) = mpsc::unbounded_channel();
    tokio::spawn(session(rooms.clone(), Incoming(rx), out, true, QUIET));
    send.send(ToNode::Hello {
        version: 1,
        name: "older".into(),
        token: None,
    })
    .unwrap();
    assert!(matches!(
        recv.recv().await,
        Some(ToClient::Error { message }) if message == "protocol version mismatch"
    ));
}
