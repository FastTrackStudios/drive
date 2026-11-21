//! Drive for Dioxus apps.
//!
//! ```ignore
//! rsx! { DriveProvider { App {} } }
//!
//! fn Board() -> Element {
//!     let doc = use_doc("canvas:board-1");
//!     let shapes = doc.loro().get_map("shapes");   // re-renders on any change
//!     doc.edit(|d| d.get_map("shapes").insert("a", 1).unwrap()); // commit + send
//!     doc.set_presence(&json!({ "cursor": [x, y] }));
//!     for p in doc.others() { /* p.name, p.hue, p.data["cursor"] */ }
//! }
//! ```
//!
//! Every document is live (edits and presence from others arrive as they
//! are made) and local-first (this browser keeps a copy; edits made offline
//! are sent on reconnect).
//!
//! Documents belong to an org: `use_doc("studio:library")` opens
//! `<org>/studio:library` in the org this browser is in (the page's
//! `?org=`, then the last one used here, then the node's default), and
//! `use_docs` lists within it, ids without the org. An id that already
//! names an org (`other/studio:library`) is used as it is.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
pub use drive_client::{Drive, Event, Me, Status, loro};
pub use drive_proto::{DocInfo, Peer};
use futures_util::StreamExt;
use serde::Serialize;
use wasm_bindgen::JsCast;

pub mod text;
pub mod ui;

/// Another peer in a document.
#[derive(Clone, Debug, PartialEq)]
pub struct Presence {
    pub peer: Peer,
    pub name: String,
    pub hue: u32,
    /// What the app put there (a cursor, a selection, …).
    pub data: serde_json::Value,
}

#[derive(Clone, Copy)]
struct DocSignals {
    rev: Signal<u64>,
    presence: Signal<Vec<Presence>>,
}

/// The app-wide Drive state, in context.
#[derive(Clone)]
pub struct DriveCtx {
    pub drive: Drive,
    /// The org every document opened here belongs to.
    pub org: String,
    pub status: Signal<Status>,
    pub me: Signal<Me>,
    docs: Rc<RefCell<HashMap<String, DocSignals>>>,
    lists: Rc<RefCell<HashMap<String, Signal<Vec<DocInfo>>>>>,
}

impl DriveCtx {
    fn signals(&self, id: &str) -> DocSignals {
        *self
            .docs
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| DocSignals {
                rev: Signal::new_in_scope(0, ScopeId::ROOT),
                presence: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            })
    }

    /// A document's full id in this org.
    pub fn qualify(&self, id: &str) -> String {
        drive_proto::qualify(&self.org, id)
    }

    /// Move to another org: every app on this address follows (reloads).
    pub fn set_org(&self, org: &str) {
        if let Some(s) = local() {
            let _ = s.set_item(ORG_KEY, org);
        }
        if let Some(w) = web_sys::window() {
            let _ = w.location().set_search("");
            let _ = w.location().reload();
        }
    }

    /// Change your name and colour, in every app on this address.
    pub fn set_me(&self, me: Me) {
        self.drive.set_me(me.clone());
        let mut s = self.me;
        s.set(me);
    }
}

const ORG_KEY: &str = "drive:org";

fn local() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

/// The org named by the page (`?org=`) or last used here.
fn known_org() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let from_url = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("org="))
        .map(|v| {
            js_sys::decode_uri_component(v)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| v.to_string())
        })
        .filter(|v| !v.is_empty());
    if let Some(o) = &from_url
        && let Some(s) = local()
    {
        let _ = s.set_item(ORG_KEY, o);
    }
    from_url
        .or_else(|| local()?.get_item(ORG_KEY).ok().flatten())
        .filter(|v| !v.is_empty())
}

/// The node's default org (`/orgs`).
async fn default_org() -> Option<String> {
    let w = web_sys::window()?;
    let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(w.fetch_with_str("/orgs"))
        .await
        .ok()?
        .dyn_into()
        .ok()?;
    let text = wasm_bindgen_futures::JsFuture::from(resp.text().ok()?)
        .await
        .ok()?
        .as_string()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["default"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Connects to the Drive node this page was served from, and provides it,
/// once the org is known (at once, unless this browser never chose one).
#[component]
pub fn DriveProvider(children: Element) -> Element {
    let mut org = use_signal(known_org);
    use_future(move || async move {
        if org.peek().is_none() {
            let o = default_org().await.unwrap_or_else(|| "home".to_string());
            if let Some(s) = local() {
                let _ = s.set_item(ORG_KEY, &o);
            }
            org.set(Some(o));
        }
    });
    match org() {
        Some(o) => rsx! {
            Connected { key: "{o}", org: o, {children} }
        },
        None => rsx! {},
    }
}

#[component]
fn Connected(org: String, children: Element) -> Element {
    use_context_provider(|| {
        let (drive, rx) = Drive::connect(None);
        let ctx = DriveCtx {
            status: Signal::new(drive.status()),
            me: Signal::new(drive.me()),
            drive,
            org: org.clone(),
            docs: Rc::new(RefCell::new(HashMap::new())),
            lists: Rc::new(RefCell::new(HashMap::new())),
        };
        pump(ctx.clone(), rx);
        ctx
    });
    rsx! {
        {children}
    }
}

/// Turn the client's events into signals.
fn pump(ctx: DriveCtx, mut rx: drive_client::EventStream) {
    spawn(async move {
        while let Some(e) = rx.next().await {
            match e {
                Event::Status(s) => {
                    let mut st = ctx.status;
                    st.set(s);
                }
                Event::Changed(id) => {
                    let mut rev = ctx.signals(&id).rev;
                    rev += 1;
                }
                Event::Presence {
                    doc,
                    peer,
                    name,
                    json,
                } => {
                    let mut p = ctx.signals(&doc).presence;
                    let mut list = p.write();
                    list.retain(|x| x.peer != peer);
                    if let Some(j) = json
                        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&j)
                    {
                        let hue = v.get("hue").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
                        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                        list.push(Presence {
                            peer,
                            name,
                            hue,
                            data,
                        });
                        list.sort_by_key(|x| x.peer);
                    }
                }
                Event::Docs { prefix, docs } => {
                    let sig = ctx.lists.borrow().get(&prefix).copied();
                    if let Some(mut s) = sig {
                        // Ids within this org, as the app named them.
                        let docs = docs
                            .into_iter()
                            .map(|mut d| {
                                d.id = drive_proto::unqualify(&ctx.org, &d.id).to_string();
                                d
                            })
                            .collect();
                        s.set(docs);
                    }
                }
            }
        }
    });
}

pub fn use_drive() -> DriveCtx {
    use_context::<DriveCtx>()
}

/// A live document.
#[derive(Clone)]
pub struct Doc {
    pub id: String,
    drive: Drive,
    doc: drive_client::loro::LoroDoc,
    rev: Signal<u64>,
    presence: Signal<Vec<Presence>>,
    me: Signal<Me>,
}

impl PartialEq for Doc {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.rev == other.rev && self.drive == other.drive
    }
}

impl Doc {
    /// The Loro document; reading it subscribes the component to changes.
    pub fn loro(&self) -> drive_client::loro::LoroDoc {
        let _ = (self.rev)();
        self.doc.clone()
    }

    /// The Loro document without subscribing (for event handlers).
    pub fn peek(&self) -> drive_client::loro::LoroDoc {
        self.doc.clone()
    }

    /// Edit, then commit and send.
    pub fn edit<R>(&self, f: impl FnOnce(&drive_client::loro::LoroDoc) -> R) -> R {
        let r = f(&self.doc);
        self.drive.commit(&self.id);
        r
    }

    /// Commit edits made directly on [`Doc::peek`] (or by an undo manager).
    pub fn commit(&self) {
        self.drive.commit(&self.id);
    }

    /// Where you are: any serialisable value, sent live, never stored.
    pub fn set_presence<T: Serialize>(&self, data: &T) {
        let me = self.me.peek().clone();
        let json = serde_json::json!({ "hue": me.hue, "data": data });
        self.drive.set_presence(&self.id, json.to_string());
    }

    /// Everyone else here, live.
    pub fn others(&self) -> Vec<Presence> {
        let me = self.drive.peer();
        (self.presence)()
            .into_iter()
            .filter(|p| Some(p.peer) != me)
            .collect()
    }

    /// Whether the node has answered (an empty document is really empty).
    pub fn synced(&self) -> bool {
        let _ = (self.rev)();
        self.drive.is_synced(&self.id)
    }

    /// Merge exported Loro bytes made elsewhere (a template) and send them.
    pub fn import(&self, bytes: &[u8]) {
        self.drive.import_local(&self.id, bytes);
    }

    /// A number that changes whenever the document does.
    pub fn rev(&self) -> u64 {
        (self.rev)()
    }
}

/// Open a document for this component (and keep it open).
pub fn use_doc(id: impl Into<String>) -> Doc {
    let ctx = use_drive();
    let id: String = ctx.qualify(&id.into());
    let me = ctx.me;
    use_hook(|| {
        let doc = ctx.drive.open(&id);
        let s = ctx.signals(&id);
        let d = Doc {
            id: id.clone(),
            drive: ctx.drive.clone(),
            doc,
            rev: s.rev,
            presence: s.presence,
            me,
        };
        // Being here is presence too: others see you as soon as you open it.
        let me = d.me.peek().clone();
        d.drive.announce(
            &d.id,
            serde_json::json!({ "hue": me.hue, "data": null }).to_string(),
        );
        d
    })
}

/// The documents the node holds under a prefix, refreshed every few seconds.
pub fn use_docs(prefix: impl Into<String>) -> Signal<Vec<DocInfo>> {
    let ctx = use_drive();
    let prefix: String = ctx.qualify(&prefix.into());
    let sig = use_hook(|| {
        *ctx.lists
            .borrow_mut()
            .entry(prefix.clone())
            .or_insert_with(|| Signal::new_in_scope(Vec::new(), ScopeId::ROOT))
    });
    let drive = ctx.drive.clone();
    let status = ctx.status;
    use_future(move || {
        let drive = drive.clone();
        let prefix = prefix.clone();
        async move {
            loop {
                if status() == Status::Online {
                    drive.list(&prefix);
                }
                gloo_sleep(3000).await;
            }
        }
    });
    sig
}

async fn gloo_sleep(ms: u32) {
    drive_client::sleep(ms).await;
}
