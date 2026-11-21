//! Signing in through the auth server (the issuer), in the browser: OIDC
//! authorization code with PKCE, client `drive`, coming back to
//! `<this node>/auth/callback`. The tokens are kept in this address's
//! `localStorage`, so one sign-in serves every app the node serves (Drive,
//! Studio, Season, …), and refreshed before they run out.
//!
//! The node says whether it needs a sign-in and where the issuer is
//! (`/orgs`: `auth`, `signed_in`); without an issuer nothing here runs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::JsCast;

const TOKENS: &str = "drive:auth";
const PENDING: &str = "drive:auth-pending";
const SCOPE: &str = "openid email profile offline_access";

/// How to reach the issuer, as the node says.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Issuer {
    pub issuer: String,
    pub client_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tokens {
    issuer: String,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    /// When the access token runs out (ms since the epoch).
    expires_at: f64,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    state: String,
    verifier: String,
    return_to: String,
    issuer: String,
    client_id: String,
}

fn local() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn now() -> f64 {
    js_sys::Date::now()
}

fn origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

fn redirect_uri() -> String {
    format!("{}/auth/callback", origin())
}

/// 32 random bytes, hex (`crypto.getRandomValues`: there on every page,
/// where `randomUUID` needs a secure one).
fn random() -> String {
    js_sys::eval("Array.from(crypto.getRandomValues(new Uint8Array(32)), b => b.toString(16).padStart(2, '0')).join('')")
        .ok()
        .and_then(|v| v.as_string())
        .filter(|s| s.len() == 64)
        .unwrap_or_default()
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

fn load() -> Option<Tokens> {
    serde_json::from_str(&local()?.get_item(TOKENS).ok()??).ok()
}

fn save(t: &Tokens) {
    if let (Some(s), Ok(j)) = (local(), serde_json::to_string(t)) {
        let _ = s.set_item(TOKENS, &j);
    }
    set_cookie(
        &t.access_token,
        ((t.expires_at - now()) / 1000.0).max(0.0) as u64,
    );
}

/// The token as a same-site cookie too, so plain links (a download, a
/// file opened in a new tab) carry it; `max_age` 0 removes it.
fn set_cookie(token: &str, max_age: u64) {
    let secure = origin().starts_with("https:");
    let cookie = format!(
        "drive_token={}; Path=/; Max-Age={max_age}; SameSite=Strict{}",
        enc(token),
        if secure { "; Secure" } else { "" }
    );
    if let Some(doc) = web_sys::window().and_then(|w| w.document())
        && let Ok(html) = doc.dyn_into::<web_sys::HtmlDocument>()
    {
        let _ = html.set_cookie(&cookie);
    }
}

/// The access token to send, if signed in (it may be about to run out:
/// [`fresh_token`] refreshes it).
pub fn token() -> Option<String> {
    load().map(|t| t.access_token)
}

/// `Authorization` header value for the node's HTTP routes, if signed in.
pub fn bearer() -> Option<String> {
    token().map(|t| format!("Bearer {t}"))
}

/// Forget the tokens (signing out of every app on this address).
pub fn sign_out() {
    if let Some(s) = local() {
        let _ = s.remove_item(TOKENS);
    }
    set_cookie("", 0);
}

/// Go to the issuer to sign in; it comes back to `/auth/callback`.
pub fn begin(issuer: &Issuer) {
    let (verifier, state) = (random(), random());
    if verifier.is_empty() || state.is_empty() {
        // No randomness, no sign-in: a guessable verifier is no PKCE.
        return;
    }
    let challenge = base64_url(&Sha256::digest(verifier.as_bytes()));
    let return_to = web_sys::window()
        .and_then(|w| w.location().href().ok())
        .unwrap_or_else(|| "/".into());
    let pending = Pending {
        state: state.clone(),
        verifier,
        return_to,
        issuer: issuer.issuer.clone(),
        client_id: issuer.client_id.clone(),
    };
    if let (Some(s), Ok(j)) = (local(), serde_json::to_string(&pending)) {
        let _ = s.set_item(PENDING, &j);
    }
    let url = format!(
        "{}/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        issuer.issuer.trim_end_matches('/'),
        enc(&issuer.client_id),
        enc(&redirect_uri()),
        enc(SCOPE),
        enc(&challenge),
        enc(&state)
    );
    if let Some(w) = web_sys::window() {
        let _ = w.location().set_href(&url);
    }
}

/// Whether this page is the issuer coming back.
pub fn is_callback() -> bool {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .is_some_and(|p| p == "/auth/callback")
}

fn query(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    search.trim_start_matches('?').split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| {
            js_sys::decode_uri_component(v)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| v.to_string())
        })
    })
}

/// Finish signing in (on `/auth/callback`): trade the code for tokens and
/// go back where the sign-in began. `Err`: why not (shown on the page).
pub async fn finish() -> Result<(), String> {
    if let Some(e) = query("error") {
        return Err(format!("the sign-in was not completed ({e})"));
    }
    let code = query("code").ok_or("the issuer sent no code")?;
    let state = query("state").unwrap_or_default();
    let pending: Pending = local()
        .and_then(|s| s.get_item(PENDING).ok().flatten())
        .and_then(|j| serde_json::from_str(&j).ok())
        .ok_or("this browser did not start that sign-in")?;
    if pending.state != state {
        return Err("the sign-in does not match the one this browser started".into());
    }
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        enc(&code),
        enc(&redirect_uri()),
        enc(&pending.client_id),
        enc(&pending.verifier)
    );
    let t = token_request(&pending.issuer, &pending.client_id, &body, None).await?;
    save(&t);
    if let Some(s) = local() {
        let _ = s.remove_item(PENDING);
    }
    if let Some(w) = web_sys::window() {
        let back = if pending.return_to.contains("/auth/callback") {
            "/".to_string()
        } else {
            pending.return_to
        };
        let _ = w.location().replace(&back);
    }
    Ok(())
}

/// The access token, refreshed first when it is about to run out (within
/// `margin_ms`). `None`: not signed in, or the refresh was refused (signed
/// out by the issuer): the caller asks for a sign-in.
pub async fn fresh_token(margin_ms: f64) -> Option<String> {
    let t = load()?;
    if t.expires_at - now() > margin_ms {
        return Some(t.access_token);
    }
    let Some(refresh) = t.refresh_token.clone() else {
        return (t.expires_at > now()).then_some(t.access_token);
    };
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(&refresh),
        enc(&t.client_id)
    );
    match token_request(&t.issuer, &t.client_id, &body, Some(&refresh)).await {
        Ok(new) => {
            save(&new);
            Some(new.access_token)
        }
        Err(_) => {
            sign_out();
            None
        }
    }
}

async fn token_request(
    issuer: &str,
    client_id: &str,
    body: &str,
    old_refresh: Option<&str>,
) -> Result<Tokens, String> {
    let (status, v) = fetch(
        "POST",
        &format!("{}/oauth2/token", issuer.trim_end_matches('/')),
        &[("content-type", "application/x-www-form-urlencoded")],
        Some(body),
    )
    .await
    .ok_or("the issuer is unreachable")?;
    if status != 200 {
        return Err(format!(
            "the issuer refused ({status}): {}",
            v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or("")
        ));
    }
    let access = v["access_token"]
        .as_str()
        .ok_or("the issuer sent no access token")?
        .to_string();
    let expires_in = v["expires_in"].as_f64().unwrap_or(3600.0);
    Ok(Tokens {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        access_token: access,
        // A refresh that does not rotate keeps the old refresh token.
        refresh_token: v["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or(old_refresh.map(str::to_string)),
        expires_at: now() + expires_in * 1000.0,
    })
}

/// One HTTP request from the browser: its status and JSON body.
pub async fn fetch(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u16, serde_json::Value)> {
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
    let resp: web_sys::Response =
        wasm_bindgen_futures::JsFuture::from(w.fetch_with_str_and_init(url, &init))
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
    Some((
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    ))
}

fn base64_url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_without_padding() {
        // RFC 4648's test vectors, padding dropped.
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(b"foob"), "Zm9vYg");
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_challenge_is_sha256_base64url() {
        // Checked against Python's hashlib + base64.urlsafe_b64encode.
        let verifier = "dBjftJeZ4CVP-mJ92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64_url(&Sha256::digest(verifier.as_bytes())),
            "ngF5GsXcbwljx6u133FFr3Xht9xooA_DuaX_3QwODtc"
        );
    }
}
