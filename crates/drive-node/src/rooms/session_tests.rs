//! A live connection's sign-in, asked about again (drive#56), and what a
//! connected peer may not pretend to be.

use std::sync::Arc;
use std::time::Duration;

use drive_proto::{ToClient, ToNode};
use tokio::sync::mpsc;
use tokio_stream_shim::Incoming;

use super::{Rooms, session};
use crate::auth::{Auth, Principal};

/// A minimal `Stream` over an mpsc receiver (no tokio-stream dependency).
mod tokio_stream_shim {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Incoming(pub tokio::sync::mpsc::UnboundedReceiver<drive_proto::ToNode>);

    impl futures::Stream for Incoming {
        type Item = drive_proto::ToNode;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }
}

const FAST: Duration = Duration::from_millis(50);

fn person(who: &str, orgs: &[&str]) -> Principal {
    Principal {
        who: who.into(),
        orgs: orgs.iter().map(|o| o.to_string()).collect(),
        all: false,
    }
}

/// An issuer that refuses every token (as it does an expired or revoked one).
async fn refusing_issuer() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(|| async { axum::http::StatusCode::UNAUTHORIZED });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn rooms(dir: &std::path::Path, issuer: String) -> Arc<Rooms> {
    let store = crate::store::Store::open(&dir.join("drive.db"))
        .await
        .unwrap();
    Arc::new(Rooms::new(store, Auth::new(Some(issuer))))
}

struct Client {
    send: mpsc::UnboundedSender<ToNode>,
    recv: mpsc::UnboundedReceiver<ToClient>,
    done: tokio::task::JoinHandle<()>,
}

impl Client {
    async fn connect(rooms: &Arc<Rooms>, token: &str, recheck: Duration) -> Client {
        let (send, rx) = mpsc::unbounded_channel();
        let (out, recv) = mpsc::unbounded_channel();
        let done = tokio::spawn(session(rooms.clone(), Incoming(rx), out, false, recheck));
        let mut c = Client { send, recv, done };
        c.send(ToNode::Hello {
            version: drive_proto::VERSION,
            name: "t".into(),
            token: Some(token.into()),
        });
        assert!(matches!(c.next().await, ToClient::Welcome { .. }));
        c
    }

    fn send(&self, m: ToNode) {
        self.send.send(m).unwrap();
    }

    async fn next(&mut self) -> ToClient {
        tokio::time::timeout(Duration::from_secs(5), self.recv.recv())
            .await
            .expect("an answer in time")
            .expect("the session is still sending")
    }

    async fn join(&mut self, doc: &str) {
        self.send(ToNode::Join {
            doc: doc.into(),
            version_vector: Vec::new(),
        });
        assert!(matches!(self.next().await, ToClient::Synced { .. }));
    }

    /// The session ends by itself, after saying `why`.
    async fn ended(mut self, why: &str) {
        assert!(
            matches!(self.next().await, ToClient::Error { message } if message == why),
            "expected {why}"
        );
        tokio::time::timeout(Duration::from_secs(5), self.done)
            .await
            .expect("the session ended")
            .unwrap();
    }
}

async fn peers_of(rooms: &Rooms, doc: &str) -> usize {
    rooms.sync.peers_in(doc).await
}

#[tokio::test]
async fn an_expired_or_revoked_sign_in_ends_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path(), refusing_issuer().await).await;
    rooms.auth.remember("t", person("alice", &["a"])).await;
    let mut c = Client::connect(&rooms, "t", FAST).await;
    c.join("a/notes").await;
    assert_eq!(peers_of(&rooms, "a/notes").await, 1);
    // The issuer's minute is up and it no longer accepts the token.
    rooms.auth.forget("t").await;
    c.ended("sign in").await;
    assert_eq!(peers_of(&rooms, "a/notes").await, 0);
}

#[tokio::test]
async fn leaving_an_org_ends_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path(), refusing_issuer().await).await;
    rooms.auth.remember("t", person("alice", &["a", "b"])).await;
    let mut c = Client::connect(&rooms, "t", FAST).await;
    c.join("a/notes").await;
    rooms.auth.remember("t", person("alice", &["b"])).await;
    c.ended("access changed: reconnect").await;
    assert_eq!(peers_of(&rooms, "a/notes").await, 0);
}

#[tokio::test]
async fn gaining_an_org_or_an_unreachable_issuer_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing listens on port 9: the issuer cannot be asked.
    let rooms = rooms(dir.path(), "http://127.0.0.1:9".into()).await;
    rooms.auth.remember("t", person("alice", &["a"])).await;
    let mut c = Client::connect(&rooms, "t", FAST).await;
    rooms.auth.remember("t", person("alice", &["a", "b"])).await;
    tokio::time::sleep(FAST * 3).await;
    // Now in b too: its documents open.
    c.join("b/notes").await;
    rooms.auth.forget("t").await;
    tokio::time::sleep(FAST * 3).await;
    c.join("a/notes").await;
    assert!(!c.done.is_finished());
}

#[tokio::test]
async fn another_org_cannot_register_as_an_orgs_agent() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path(), refusing_issuer().await).await;
    // Org a shares a project with b: b may read a's registry, not be its agent.
    rooms
        .edit("a/drive:space", "t", |d| {
            let p = drive_files::projects::add_project(d, "Song", "MusicRecording").unwrap();
            drive_files::projects::share_with_org(d, &p, "b", true).unwrap();
        })
        .await;
    rooms.auth.remember("tb", person("bob", &["b"])).await;
    assert!(rooms.may(&person("bob", &["b"]), "a/drive:space").await);
    let mut c = Client::connect(&rooms, "tb", Duration::from_secs(3600)).await;
    c.send(ToNode::Agent {
        space: "a".into(),
        device: "pc".into(),
    });
    assert!(
        matches!(c.next().await, ToClient::Error { message } if message.starts_with("not allowed"))
    );
    assert!(rooms.agents_online("a").await.is_empty());
}

#[tokio::test]
async fn only_the_asked_agent_answers_a_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path(), refusing_issuer().await).await;
    rooms
        .edit("a/drive:space", "t", |d| {
            drive_files::space::add_location(
                d,
                &drive_files::space::Location {
                    id: "music".into(),
                    name: "music".into(),
                    device: "pc".into(),
                    path: "/m".into(),
                    volume: String::new(),
                    storage: String::new(),
                    mirror_of: String::new(),
                    index: String::new(),
                    files: 0,
                    bytes: 0,
                    built: 0,
                },
            )
            .unwrap();
        })
        .await;
    rooms.auth.remember("ta", person("agent", &["a"])).await;
    rooms.auth.remember("tm", person("mallory", &["a"])).await;
    let hour = Duration::from_secs(3600);
    let mut agent = Client::connect(&rooms, "ta", hour).await;
    let spoofer = Client::connect(&rooms, "tm", hour).await;
    agent.send(ToNode::Agent {
        space: "a".into(),
        device: "pc".into(),
    });
    while !rooms.agent_online("a", "pc").await {
        tokio::task::yield_now().await;
    }
    let fetch = {
        let rooms = rooms.clone();
        tokio::spawn(async move { rooms.fetch("a", "music", "kick.wav").await })
    };
    let req = loop {
        if let ToClient::Fetch { req, .. } = agent.next().await {
            break req;
        }
    };
    spoofer.send(ToNode::Fetched {
        req,
        result: Ok("forged".into()),
    });
    tokio::time::sleep(FAST).await;
    agent.send(ToNode::Fetched {
        req,
        result: Ok("real".into()),
    });
    assert_eq!(fetch.await.unwrap().unwrap(), "real");
}

#[tokio::test]
async fn a_second_sign_in_on_one_connection_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = rooms(dir.path(), refusing_issuer().await).await;
    rooms.auth.remember("t", person("alice", &["a"])).await;
    rooms.auth.remember("u", person("bob", &["b"])).await;
    let mut c = Client::connect(&rooms, "t", Duration::from_secs(600)).await;
    c.join("a/notes").await;
    c.send(ToNode::Hello {
        version: drive_proto::VERSION,
        name: "t".into(),
        token: Some("u".into()),
    });
    c.ended("already signed in: reconnect").await;
    assert_eq!(peers_of(&rooms, "a/notes").await, 0);
}
