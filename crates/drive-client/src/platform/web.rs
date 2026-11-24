//! In a browser: the page's WebSocket, its timers, `localStorage` for who
//! this is and IndexedDB for the documents' copies.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{BinaryType, MessageEvent, WebSocket};

use crate::{Drive, idb};

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub(crate) fn random() -> f64 {
    js_sys::Math::random()
}

pub(crate) async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

pub(crate) fn spawn(fut: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}

pub(crate) fn after(ms: u32, f: impl FnOnce() + 'static) {
    gloo_timers::callback::Timeout::new(ms, f).forget();
}

/// The page's own host.
pub(crate) fn default_url() -> String {
    let loc = web_sys::window().expect("a window").location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    format!("{proto}://{}/sync", loc.host().unwrap_or_default())
}

pub(crate) fn load_me() -> Option<String> {
    storage()?.get_item("drive:me").ok().flatten()
}

pub(crate) fn save_me(json: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item("drive:me", json);
    }
}

pub(crate) async fn load_doc(id: &str) -> Option<Vec<u8>> {
    idb::load(id).await
}

/// Save a document's copy soon (IndexedDB keeps the saves in order).
pub(crate) fn save_doc(id: &str, bytes: Vec<u8>) {
    let id = id.to_string();
    spawn(async move { idb::save(&id, &bytes).await });
}

/// Call `f` whenever the page is hidden or put away (`visibilitychange` to
/// hidden, `pagehide`): it may not come back.
pub(crate) fn on_hide(f: impl Fn() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let document = window.document();
    let f = std::rc::Rc::new(f);
    let hidden = {
        let f = f.clone();
        let document = document.clone();
        Closure::<dyn FnMut()>::new(move || {
            if document
                .as_ref()
                .is_some_and(|d| d.visibility_state() == web_sys::VisibilityState::Hidden)
            {
                f();
            }
        })
    };
    if let Some(d) = &document {
        let _ =
            d.add_event_listener_with_callback("visibilitychange", hidden.as_ref().unchecked_ref());
    }
    // Once per connection, kept for the page's life.
    hidden.forget();
    let pagehide = Closure::<dyn FnMut()>::new(move || f());
    let _ = window.add_event_listener_with_callback("pagehide", pagehide.as_ref().unchecked_ref());
    pagehide.forget();
}

/// Copies kept in `localStorage` before IndexedDB: dropped (they were
/// under ids from before orgs, and the node has every one of them).
pub(crate) fn drop_old_copies() {
    let Some(s) = storage() else { return };
    let n = s.length().unwrap_or(0);
    let old: Vec<String> = (0..n)
        .filter_map(|i| s.key(i).ok().flatten())
        .filter(|k| k.starts_with("drive:doc:"))
        .collect();
    for k in old {
        let _ = s.remove_item(&k);
    }
}

/// A socket's handlers, which live as long as it does (dropped, it stops
/// calling them). Kept here rather than in the socket, so that the client
/// stays `Send` (a closure is not).
struct Handlers {
    _onopen: Closure<dyn FnMut()>,
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
    _onclose: Closure<dyn FnMut()>,
}

thread_local! {
    static HANDLERS: RefCell<HashMap<u32, Handlers>> = RefCell::new(HashMap::new());
}

/// The page's WebSocket to the node (its handlers under its generation).
pub(crate) struct Socket {
    ws: WebSocket,
    generation: u32,
}

impl Socket {
    /// Open it, its events going to `drive`.
    pub(crate) fn open(url: &str, drive: Drive, generation: u32) -> Option<Socket> {
        let ws = WebSocket::new(url).ok()?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let me = drive.clone();
        let onopen = Closure::<dyn FnMut()>::new(move || me.socket_opened(generation));
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        let me = drive.clone();
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                me.socket_message(generation, &js_sys::Uint8Array::new(&buf).to_vec());
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        let me = drive;
        let onclose = Closure::<dyn FnMut()>::new(move || me.socket_closed(generation));
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        let handlers = Handlers {
            _onopen: onopen,
            _onmessage: onmessage,
            _onclose: onclose,
        };
        HANDLERS.with(|h| h.borrow_mut().insert(generation, handlers));
        Some(Socket { ws, generation })
    }

    pub(crate) fn is_open(&self) -> bool {
        self.ws.ready_state() == WebSocket::OPEN
    }

    pub(crate) fn send(&self, bytes: Vec<u8>) -> bool {
        self.ws.send_with_u8_array(&bytes).is_ok()
    }

    pub(crate) fn close(&self) {
        let _ = self.ws.close();
    }
}

impl Drop for Socket {
    /// A socket is dropped when another replaces it (it was closed then):
    /// the browser must not call its handlers once they are gone.
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
        let gone = HANDLERS.with(|h| h.borrow_mut().remove(&self.generation));
        drop(gone);
    }
}
