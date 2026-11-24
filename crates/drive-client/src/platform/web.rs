//! In a browser: the page's WebSocket, its timers, `localStorage` for who
//! this is and IndexedDB for the documents' copies.

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

pub(crate) async fn save_doc(id: &str, bytes: &[u8]) {
    idb::save(id, bytes).await;
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

/// The page's WebSocket to the node.
pub(crate) struct Socket(WebSocket);

impl Socket {
    /// Open it, its events going to `drive`.
    pub(crate) fn open(url: &str, drive: Drive, generation: u32) -> Option<Socket> {
        let ws = WebSocket::new(url).ok()?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let me = drive.clone();
        let onopen = Closure::<dyn FnMut()>::new(move || me.socket_opened(generation));
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let me = drive.clone();
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                me.socket_message(generation, &js_sys::Uint8Array::new(&buf).to_vec());
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let me = drive;
        let onclose = Closure::<dyn FnMut()>::new(move || me.socket_closed(generation));
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        Some(Socket(ws))
    }

    pub(crate) fn is_open(&self) -> bool {
        self.0.ready_state() == WebSocket::OPEN
    }

    pub(crate) fn send(&self, bytes: Vec<u8>) -> bool {
        self.0.send_with_u8_array(&bytes).is_ok()
    }

    pub(crate) fn close(&self) {
        let _ = self.0.close();
    }
}
