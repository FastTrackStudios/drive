//! This browser's copy of each document, in IndexedDB (one database,
//! `drive`, one object store, `docs`: document id → snapshot bytes).
//! IndexedDB holds as much as the browser allows the site (gigabytes),
//! where `localStorage` stopped at a few megabytes.

use std::cell::RefCell;

use js_sys::{Promise, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbRequest, IdbTransactionMode};

const DB: &str = "drive";
const STORE: &str = "docs";

thread_local! {
    static OPEN: RefCell<Option<IdbDatabase>> = const { RefCell::new(None) };
}

/// Wait for a request: its result, or `None` when it failed.
async fn done(r: &IdbRequest) -> Option<JsValue> {
    let req = r.clone();
    let p = Promise::new(&mut |resolve, reject| {
        let ok_req = req.clone();
        let ok = Closure::once_into_js(move |_e: web_sys::Event| {
            let _ = resolve.call1(
                &JsValue::NULL,
                &ok_req.result().unwrap_or(JsValue::UNDEFINED),
            );
        });
        let err = Closure::once_into_js(move |_e: web_sys::Event| {
            let _ = reject.call0(&JsValue::NULL);
        });
        req.set_onsuccess(Some(ok.unchecked_ref()));
        req.set_onerror(Some(err.unchecked_ref()));
    });
    JsFuture::from(p).await.ok()
}

async fn db() -> Option<IdbDatabase> {
    if let Some(d) = OPEN.with(|o| o.borrow().clone()) {
        return Some(d);
    }
    let factory = web_sys::window()?.indexed_db().ok()??;
    let open = factory.open_with_u32(DB, 1).ok()?;
    let upgrade = Closure::once_into_js(move |e: web_sys::Event| {
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
    open.set_onupgradeneeded(Some(upgrade.unchecked_ref()));
    let d: IdbDatabase = done(&open).await?.dyn_into().ok()?;
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
