//! This browser's copy of each document, in IndexedDB (one database,
//! `drive`, one object store, `docs`: document id → snapshot bytes).
//! IndexedDB holds as much as the browser allows the site (gigabytes),
//! where `localStorage` stopped at a few megabytes.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{IdbDatabase, IdbRequest, IdbTransactionMode};

const DB: &str = "drive";
const STORE: &str = "docs";

thread_local! {
    static OPEN: RefCell<Option<IdbDatabase>> = const { RefCell::new(None) };
}

/// Wait for a request: its result, or `None` when it failed. Its handlers
/// live until it has answered, then are taken off it and dropped.
async fn done(r: &IdbRequest) -> Option<JsValue> {
    let (tx, rx) = futures_channel::oneshot::channel::<bool>();
    let tx = Rc::new(RefCell::new(Some(tx)));
    let answer = |ok: bool| {
        let tx = tx.clone();
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(ok);
            }
        })
    };
    let ok = answer(true);
    let err = answer(false);
    r.set_onsuccess(Some(ok.as_ref().unchecked_ref()));
    r.set_onerror(Some(err.as_ref().unchecked_ref()));
    let succeeded = rx.await.unwrap_or(false);
    r.set_onsuccess(None);
    r.set_onerror(None);
    if succeeded { r.result().ok() } else { None }
}

async fn db() -> Option<IdbDatabase> {
    if let Some(d) = OPEN.with(|o| o.borrow().clone()) {
        return Some(d);
    }
    let factory = web_sys::window()?.indexed_db().ok()??;
    let open = factory.open_with_u32(DB, 1).ok()?;
    let upgrade = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        let made = e
            .target()
            .and_then(|t| t.dyn_into::<IdbRequest>().ok())
            .and_then(|r| r.result().ok())
            .and_then(|r| r.dyn_into::<IdbDatabase>().ok());
        if let Some(d) = made
            && !d.object_store_names().contains(STORE)
        {
            let _ = d.create_object_store(STORE);
        }
    });
    open.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
    // An upgrade comes before success, so it is done with by then.
    let opened = done(&open).await;
    open.set_onupgradeneeded(None);
    drop(upgrade);
    let d: IdbDatabase = opened?.dyn_into().ok()?;
    OPEN.with(|o| *o.borrow_mut() = Some(d.clone()));
    Some(d)
}

/// The saved copy of a document, if there is one.
pub async fn load(id: &str) -> Option<Vec<u8>> {
    let d = db().await?;
    let store = d
        .transaction_with_str(STORE)
        .ok()?
        .object_store(STORE)
        .ok()?;
    let v = done(&store.get(&JsValue::from_str(id)).ok()?).await?;
    v.dyn_into::<Uint8Array>().ok().map(|a| a.to_vec())
}

/// Save a document's copy.
pub async fn save(id: &str, bytes: &[u8]) {
    let Some(d) = db().await else { return };
    let Ok(tx) = d.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite) else {
        return;
    };
    let Ok(store) = tx.object_store(STORE) else {
        return;
    };
    if let Ok(r) = store.put_with_key(&Uint8Array::from(bytes), &JsValue::from_str(id)) {
        let _ = done(&r).await;
    }
}
