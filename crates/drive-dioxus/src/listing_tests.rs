//! `use_docs` follows a live listing (drive#159): it subscribes once,
//! never asks again on a timer, follows a changed prefix and lets go when
//! the component goes.

use std::sync::Mutex;
use std::time::Duration;

use dioxus::dioxus_core::NoOpMutations;
use drive_client::{Connection, Transport, Wire};
use drive_proto::{ToClient, ToNode};

use super::*;

/// Every message the app sent, in order.
static SENT: Mutex<Vec<ToNode>> = Mutex::new(Vec::new());

fn sent() -> Vec<ToNode> {
    SENT.lock().unwrap().clone()
}

/// A node in the test: it answers a hello and a subscription, and keeps
/// what it was sent.
struct Recorder;

struct Open(Wire);

/// Answer on another thread: the client sends while holding its socket.
fn later(wire: &Wire, f: impl FnOnce(&Wire) + Send + 'static) {
    let wire = wire.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        f(&wire);
    });
}

impl Transport for Recorder {
    fn open(&self, wire: Wire) -> Option<Box<dyn Connection>> {
        later(&wire, Wire::opened);
        Some(Box::new(Open(wire)))
    }
    fn describe(&self) -> String {
        "recorder".into()
    }
}

impl Connection for Open {
    fn is_open(&self) -> bool {
        true
    }
    fn send(&self, frame: Vec<u8>) -> bool {
        let msg: ToNode = drive_proto::decode(&frame).unwrap();
        SENT.lock().unwrap().push(msg.clone());
        let answer = match msg {
            ToNode::Hello { .. } => Some(ToClient::Welcome { peer: 1 }),
            ToNode::ListSubscribe { prefix } => Some(ToClient::Docs {
                prefix,
                docs: Vec::new(),
            }),
            _ => None,
        };
        if let Some(a) = answer {
            later(&self.0, move |w| w.message(&drive_proto::encode(&a)));
        }
        true
    }
    fn close(&self) {}
}

/// Which prefix the view lists (`None`: no view).
static PREFIX: Mutex<Option<&'static str>> = Mutex::new(Some("studio:"));

fn app() -> Element {
    use_context_provider(|| {
        let (drive, rx) = Drive::connect_via(Recorder, None);
        let ctx = DriveCtx {
            status: Signal::new(drive.status()),
            me: Signal::new(drive.me()),
            drive,
            org: "home".into(),
            who: String::new(),
            signed_in: false,
            docs: Rc::new(RefCell::new(HashMap::new())),
            lists: Rc::new(RefCell::new(HashMap::new())),
        };
        pump(ctx.clone(), rx);
        ctx
    });
    let mut tick = use_signal(|| 0u32);
    // Redraw now and then, so a changed PREFIX is read.
    use_future(move || async move {
        loop {
            drive_client::sleep(50).await;
            tick += 1;
        }
    });
    let _ = tick();
    let prefix = *PREFIX.lock().unwrap();
    match prefix {
        Some(p) => rsx! { View { prefix: p } },
        None => rsx! {},
    }
}

#[component]
fn View(prefix: &'static str) -> Element {
    let docs = use_docs(prefix);
    rsx! { "{docs.read().len()}" }
}

/// Run the app for a while.
async fn run(vdom: &mut VirtualDom, for_: Duration) {
    let until = tokio::time::Instant::now() + for_;
    while tokio::time::Instant::now() < until {
        let _ = tokio::time::timeout(Duration::from_millis(20), vdom.wait_for_work()).await;
        vdom.render_immediate(&mut NoOpMutations);
    }
}

fn of(kind: fn(&ToNode) -> Option<&str>) -> Vec<String> {
    sent().iter().filter_map(kind).map(str::to_string).collect()
}

fn subscribed(m: &ToNode) -> Option<&str> {
    match m {
        ToNode::ListSubscribe { prefix } => Some(prefix),
        _ => None,
    }
}

fn unsubscribed(m: &ToNode) -> Option<&str> {
    match m {
        ToNode::ListUnsubscribe { prefix } => Some(prefix),
        _ => None,
    }
}

fn asked(m: &ToNode) -> Option<&str> {
    match m {
        ToNode::List { prefix } => Some(prefix),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn use_docs_subscribes_once_and_never_polls() {
    let dir = tempfile::tempdir().unwrap();
    drive_client::set_data_dir(dir.path());
    let mut vdom = VirtualDom::new(app);
    vdom.rebuild_in_place();
    // Longer than the 3 s the old loop asked again after.
    run(&mut vdom, Duration::from_millis(3500)).await;
    assert_eq!(of(subscribed), ["home/studio:"]);
    assert!(of(asked).is_empty(), "no List on a timer: {:?}", sent());

    // The prefix changes: the new one followed, the old one let go.
    *PREFIX.lock().unwrap() = Some("canvas:");
    run(&mut vdom, Duration::from_millis(300)).await;
    assert_eq!(of(subscribed), ["home/studio:", "home/canvas:"]);
    assert_eq!(of(unsubscribed), ["home/studio:"]);

    // The view goes: so does its listing.
    *PREFIX.lock().unwrap() = None;
    run(&mut vdom, Duration::from_millis(300)).await;
    assert_eq!(of(unsubscribed), ["home/studio:", "home/canvas:"]);
    assert!(of(asked).is_empty());
}
