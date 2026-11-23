//! In a browser: the page's own.

use wasm_bindgen::JsCast;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn location() -> Option<web_sys::Location> {
    Some(web_sys::window()?.location())
}

pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// (month 0–11, day, year) in the page's time zone.
pub fn date_parts(ms: f64) -> (u32, u32, i64) {
    let date = js_sys::Date::new(&ms.into());
    (date.get_month(), date.get_date(), i64::from(date.get_full_year()))
}

/// 32 random bytes, hex (`crypto.getRandomValues`: there on every page,
/// where `randomUUID` needs a secure one).
pub fn random_hex32() -> String {
    js_sys::eval("Array.from(crypto.getRandomValues(new Uint8Array(32)), b => b.toString(16).padStart(2, '0')).join('')")
        .ok()
        .and_then(|v| v.as_string())
        .filter(|s| s.len() == 64)
        .unwrap_or_default()
}

pub fn get(key: &str) -> Option<String> {
    storage()?.get_item(key).ok().flatten()
}

pub fn set(key: &str, value: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, value);
    }
}

pub fn remove(key: &str) {
    if let Some(s) = storage() {
        let _ = s.remove_item(key);
    }
}

pub fn origin() -> String {
    location()
        .and_then(|l| l.origin().ok())
        .unwrap_or_default()
}

pub fn href() -> String {
    location().and_then(|l| l.href().ok()).unwrap_or_default()
}

pub fn pathname() -> String {
    location()
        .and_then(|l| l.pathname().ok())
        .unwrap_or_default()
}

pub fn search() -> String {
    location().and_then(|l| l.search().ok()).unwrap_or_default()
}

pub fn go(url: &str) {
    if let Some(l) = location() {
        let _ = l.set_href(url);
    }
}

pub fn replace(url: &str) {
    if let Some(l) = location() {
        let _ = l.replace(url);
    }
}

pub fn reload() {
    if let Some(l) = location() {
        let _ = l.reload();
    }
}

/// Reload without the page's query (`?org=` and the like).
pub fn reload_without_query() {
    if let Some(l) = location() {
        let _ = l.set_search("");
        let _ = l.reload();
    }
}

pub fn set_cookie(cookie: &str) {
    if let Some(doc) = web_sys::window().and_then(|w| w.document())
        && let Ok(html) = doc.dyn_into::<web_sys::HtmlDocument>()
    {
        let _ = html.set_cookie(cookie);
    }
}

/// The node's sync socket for an app under a mount (`None`: the page's
/// own host, at the root).
pub fn sync_url() -> Option<String> {
    if crate::mount::get().is_empty() {
        return None;
    }
    let loc = location()?;
    let proto = if loc.protocol().ok()? == "https:" {
        "wss"
    } else {
        "ws"
    };
    Some(format!(
        "{proto}://{}{}",
        loc.host().ok()?,
        crate::mount::api("/sync")
    ))
}

/// A number a page script gives back (a media element's time).
pub fn eval_f64(js: &str) -> Option<f64> {
    js_sys::eval(js).ok()?.as_f64()
}

pub fn eval(js: &str) {
    let _ = js_sys::eval(js);
}

/// One HTTP request: its status and body. A root path is the node's,
/// reached under the app's mount.
pub async fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u16, String)> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    let h = web_sys::Headers::new().ok()?;
    for (k, v) in headers {
        h.set(k, v).ok()?;
    }
    init.set_headers(&h);
    if let Some(b) = body {
        init.set_body(&wasm_bindgen::JsValue::from_str(b));
    }
    let w = web_sys::window()?;
    let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(
        w.fetch_with_str_and_init(&crate::mount::api(url), &init),
    )
    .await
    .ok()?
    .dyn_into()
    .ok()?;
    let status = resp.status();
    let text = wasm_bindgen_futures::JsFuture::from(resp.text().ok()?)
        .await
        .ok()?
        .as_string()
        .unwrap_or_default();
    Some((status, text))
}
