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

pub mod auth;
pub mod comments;
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
    /// Who is signed in (their email), when the node has an issuer.
    pub who: String,
    /// Signed in through the issuer (the node asks for it).
    pub signed_in: bool,
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

    /// Sign out of every app on this address.
    pub fn sign_out(&self) {
        auth::sign_out();
        if let Some(w) = web_sys::window() {
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

/// What the provider is doing before the apps can run.
#[derive(Clone, Debug, PartialEq)]
enum Boot {
    Loading,
    /// The node needs a sign-in: through this issuer.
    SignIn(auth::Issuer),
    Failed(String),
    Ready {
        org: String,
        token: Option<String>,
        who: String,
        issuer: bool,
    },
}

/// Where the provider starts: finish a sign-in coming back from the
/// issuer, refresh the token, ask the node who this is and which orgs they
/// are in, and pick the org.
async fn boot() -> Boot {
    if auth::is_callback() {
        return match auth::finish().await {
            Ok(()) => Boot::Loading, // going back to where the sign-in began
            Err(e) => Boot::Failed(e),
        };
    }
    let token = auth::fresh_token(60_000.0).await;
    let bearer = token.as_ref().map(|t| format!("Bearer {t}"));
    let headers: Vec<(&str, &str)> = bearer
        .iter()
        .map(|b| ("authorization", b.as_str()))
        .collect();
    let Some((_, v)) = auth::fetch("GET", "/orgs", &headers, None).await else {
        return Boot::Failed("the node did not answer".into());
    };
    let issuer: Option<auth::Issuer> = serde_json::from_value(v["auth"].clone()).ok();
    if v["signed_in"] == serde_json::Value::Bool(false) {
        return match issuer {
            Some(i) => Boot::SignIn(i),
            None => Boot::Failed("the node refused this browser".into()),
        };
    }
    let orgs: Vec<String> = v["orgs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| o.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let default = v["default"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    // The org asked for (or last used), when this person is in it.
    let org = known_org()
        .filter(|o| orgs.is_empty() || orgs.contains(o))
        .or(default)
        .or_else(|| orgs.first().cloned())
        .unwrap_or_else(|| "home".to_string());
    if let Some(s) = local() {
        let _ = s.set_item(ORG_KEY, &org);
    }
    Boot::Ready {
        org,
        token: if issuer.is_some() { token } else { None },
        // Who is signed in; without an issuer the node says "anyone", and
        // the browser's own name is the author instead.
        who: if issuer.is_some() {
            v["who"].as_str().unwrap_or_default().to_string()
        } else {
            String::new()
        },
        issuer: issuer.is_some(),
    }
}

/// Connects to the Drive node this page was served from, and provides it,
/// once the org is known; signs in first when the node asks for it.
#[component]
pub fn DriveProvider(children: Element) -> Element {
    let mut state = use_signal(|| Boot::Loading);
    use_future(move || async move {
        let b = boot().await;
        state.set(b);
    });
    match state() {
        Boot::Ready {
            org,
            token,
            who,
            issuer,
        } => rsx! {
            Connected { key: "{org}", org, token, who, issuer, {children} }
        },
        Boot::SignIn(i) => rsx! {
            SignInScreen { issuer: i }
        },
        Boot::Failed(why) => rsx! {
            BootMessage { why }
        },
        Boot::Loading => rsx! {},
    }
}

const CARD: &str = "min-height:100vh;display:grid;place-items:center;background:#000;color:#f4f4f5;font:15px/1.5 -apple-system,system-ui,sans-serif;padding:24px;";
const PANEL: &str = "max-width:360px;width:100%;display:flex;flex-direction:column;gap:12px;padding:28px;background:#141417;border:1px solid #2b2b31;border-radius:18px;text-align:center;";
const BUTTON: &str = "height:44px;border-radius:12px;border:0;background:#f4f4f5;color:#000;font-weight:650;font-size:15px;cursor:pointer;";

#[component]
fn SignInScreen(issuer: auth::Issuer) -> Element {
    let host = issuer
        .issuer
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    rsx! {
        div { style: CARD,
            div { style: PANEL,
                b { style: "font-size:22px;letter-spacing:-0.02em;", "Sign in" }
                span { style: "color:#a1a1aa;font-size:14px;",
                    "This Drive is for its orgs' members. Sign in with your account at {host}."
                }
                button { style: BUTTON, onclick: move |_| auth::begin(&issuer), "Continue" }
            }
        }
    }
}

#[component]
fn BootMessage(why: String) -> Element {
    rsx! {
        div { style: CARD,
            div { style: PANEL,
                b { style: "font-size:18px;", "Could not open Drive" }
                span { style: "color:#a1a1aa;font-size:14px;", "{why}" }
                button {
                    style: BUTTON,
                    onclick: move |_| {
                        if let Some(w) = web_sys::window() {
                            let _ = w.location().set_href("/");
                        }
                    },
                    "Try again"
                }
            }
        }
    }
}

#[component]
fn Connected(
    org: String,
    token: Option<String>,
    who: String,
    issuer: bool,
    children: Element,
) -> Element {
    let ctx = use_context_provider(|| {
        let (drive, rx) = Drive::connect_with(None, token.clone());
        let ctx = DriveCtx {
            status: Signal::new(drive.status()),
            me: Signal::new(drive.me()),
            drive,
            org: org.clone(),
            who: who.clone(),
            signed_in: issuer,
            docs: Rc::new(RefCell::new(HashMap::new())),
            lists: Rc::new(RefCell::new(HashMap::new())),
        };
        // Every change made here is by the signed-in person (or this
        // browser's name).
        let author = if who.is_empty() {
            ctx.me.peek().name.clone()
        } else {
            who.clone()
        };
        ctx.drive.set_author(&author);
        pump(ctx.clone(), rx);
        ctx
    });
    // Keep the token fresh: a new one reconnects with it; a refused
    // refresh (signed out at the issuer) asks for a sign-in again.
    use_future(move || {
        let ctx = ctx.clone();
        async move {
            if !ctx.signed_in {
                return;
            }
            loop {
                drive_client::sleep(60_000).await;
                match auth::fresh_token(5.0 * 60_000.0).await {
                    Some(t) => ctx.drive.set_token(Some(t)),
                    None => {
                        if let Some(w) = web_sys::window() {
                            let _ = w.location().reload();
                        }
                    }
                }
            }
        }
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
                        // Ids within this org, as the app named them (a
                        // list of every org keeps them whole).
                        let everywhere = prefix.starts_with("*/");
                        let docs = docs
                            .into_iter()
                            .map(|mut d| {
                                if !everywhere {
                                    d.id = drive_proto::unqualify(&ctx.org, &d.id).to_string();
                                }
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
    list_docs(ctx, prefix)
}

/// The documents under a prefix in every org the node keeps, with full ids
/// (`fasttrackaudio/studio:song:x`), for "All orgs" views. `use_doc`
/// opens any of them as it is.
pub fn use_docs_everywhere(prefix: impl Into<String>) -> Signal<Vec<DocInfo>> {
    let ctx = use_drive();
    list_docs(ctx, format!("*/{}", prefix.into()))
}

fn list_docs(ctx: DriveCtx, prefix: String) -> Signal<Vec<DocInfo>> {
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
