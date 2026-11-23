//! Signing in through the auth server (the issuer), in the browser: OIDC
//! authorization code with PKCE, client `drive`, coming back to
//! `<this node>/auth/callback`. The tokens are kept in this address's
//! `localStorage`, so one sign-in serves every app the node serves (Drive,
//! Studio, Season, …), and refreshed before they run out.
//!
//! The node says whether it needs a sign-in and where the issuer is
//! (`/orgs`: `auth`, `signed_in`); without an issuer nothing here runs.
//!
//! A native app (a desktop or phone build) keeps its tokens in a file
//! beside its copies (`crate::env`) and calls the node it runs beside,
//! which counts a request from its own computer as signed in ("this
//! computer"). TODO(native sign-in): signing in to a node on another
//! computer — open the issuer in the system browser with a redirect to
//! `http://127.0.0.1:<port>/auth/callback` (RFC 8252's loopback redirect),
//! catch the code on that port, trade it here as `finish` does. The issuer
//! must list a loopback redirect for the `drive` client. Until then
//! [`begin`] natively opens the node's own page in the browser instead.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::env;

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

fn now() -> f64 {
    env::now_ms()
}

fn origin() -> String {
    env::origin()
}

fn redirect_uri() -> String {
    format!("{}{}", origin(), crate::mount::page("/auth/callback"))
}

/// 32 random bytes, hex (`crypto.getRandomValues`: there on every page,
/// where `randomUUID` needs a secure one).
fn random() -> String {
    env::random_hex32()
}

fn enc(s: &str) -> String {
    env::encode_component(s)
}

fn load() -> Option<Tokens> {
    serde_json::from_str(&env::get(TOKENS)?).ok()
}

fn save(t: &Tokens) {
    if let Ok(j) = serde_json::to_string(t) {
        env::set(TOKENS, &j);
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
    env::set_cookie(&cookie);
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
    env::remove(TOKENS);
    set_cookie("", 0);
}

/// Go to the issuer to sign in; it comes back to `/auth/callback`.
pub fn begin(issuer: &Issuer) {
    // Natively there is no page to come back to yet (see the module's
    // TODO): the node's own page signs in, in the browser.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = issuer;
        env::go(&origin());
        return;
    }
    #[cfg_attr(not(target_arch = "wasm32"), allow(unreachable_code))]
    let (verifier, state) = (random(), random());
    if verifier.is_empty() || state.is_empty() {
        // No randomness, no sign-in: a guessable verifier is no PKCE.
        return;
    }
    let challenge = base64_url(&Sha256::digest(verifier.as_bytes()));
    let return_to = Some(env::href())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "/".into());
    let pending = Pending {
        state: state.clone(),
        verifier,
        return_to,
        issuer: issuer.issuer.clone(),
        client_id: issuer.client_id.clone(),
    };
    if let Ok(j) = serde_json::to_string(&pending) {
        env::set(PENDING, &j);
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
    env::go(&url);
}

/// Whether this page is the issuer coming back.
pub fn is_callback() -> bool {
    cfg!(target_arch = "wasm32") && env::pathname() == crate::mount::page("/auth/callback")
}

fn query(name: &str) -> Option<String> {
    env::query_param(&env::search(), name)
}

/// Finish signing in (on `/auth/callback`): trade the code for tokens and
/// go back where the sign-in began. `Err`: why not (shown on the page).
pub async fn finish() -> Result<(), String> {
    if let Some(e) = query("error") {
        return Err(format!("the sign-in was not completed ({e})"));
    }
    let code = query("code").ok_or("the issuer sent no code")?;
    let state = query("state").unwrap_or_default();
    let pending: Pending = env::get(PENDING)
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
    env::remove(PENDING);
    let back = if pending.return_to.contains("/auth/callback") {
        crate::mount::page("/")
    } else {
        pending.return_to
    };
    env::replace(&back);
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

/// One HTTP request (from the browser, or natively to the node): its
/// status and JSON body. A root path is the node's, reached under the
/// app's mount (`mount::api`).
pub async fn fetch(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<(u16, serde_json::Value)> {
    let (status, text) = env::http(method, url, headers, body).await?;
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
