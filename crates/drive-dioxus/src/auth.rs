//! Signing in through the auth server (the issuer): OIDC authorization
//! code with PKCE (S256), client `drive`.
//!
//! In the browser it comes back to `<this node>/auth/callback`; the tokens
//! are kept in this address's `localStorage`, so one sign-in serves every
//! app the node serves (Drive, Studio, Season, …), and refreshed before
//! they run out.
//!
//! The node says whether it needs a sign-in and where the issuer is
//! (`/orgs`: `auth`, `signed_in`); without an issuer nothing here runs.
//!
//! A native app (a desktop or phone build) keeps its tokens in a file
//! beside its copies (`crate::env`, the same keys the page uses). Beside a
//! node of its own it is "this computer" and needs no sign-in; reaching a
//! node on another computer it signs in in the system's browser with a
//! loopback redirect (RFC 8252 §7.3, [`sign_in`]): a port of its own on
//! `127.0.0.1` catches the code, traded here as [`finish`] does. The
//! issuer takes any port on a loopback redirect registered for `drive`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::env;

#[cfg(not(target_arch = "wasm32"))]
mod loopback;

const TOKENS: &str = "drive:auth";
const PENDING: &str = "drive:auth-pending";
const SCOPE: &str = "openid email profile offline_access";

/// How to reach the issuer, as the node says.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Issuer {
    pub issuer: String,
    pub client_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

/// What [`fresh_token`] will give, when that is known without asking
/// anyone: `Some(None)` with no token stored, `Some(Some(t))` with one good
/// for `margin_ms` more, `None` when it would have to refresh.
pub fn settled_token(margin_ms: f64) -> Option<Option<String>> {
    match load() {
        None => Some(None),
        Some(t) => (t.expires_at - now() > margin_ms).then_some(Some(t.access_token)),
    }
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

/// One sign-in's secrets: the PKCE verifier and the `state` the issuer
/// must hand back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Attempt {
    verifier: String,
    state: String,
}

impl Attempt {
    /// A new attempt from two draws of `random` (each 32 random bytes,
    /// hex). `None` without randomness: a guessable verifier is no PKCE.
    pub(crate) fn new(mut random: impl FnMut() -> String) -> Option<Self> {
        let (verifier, state) = (random(), random());
        (verifier.len() >= 43 && state.len() >= 16).then_some(Self { verifier, state })
    }

    /// The S256 challenge the issuer is shown (RFC 7636 §4.2).
    pub(crate) fn challenge(&self) -> String {
        base64_url(&Sha256::digest(self.verifier.as_bytes()))
    }
}

/// The issuer's page that signs in and comes back to `redirect_uri`.
pub(crate) fn authorize_url(issuer: &Issuer, redirect_uri: &str, attempt: &Attempt) -> String {
    format!(
        "{}/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        issuer.issuer.trim_end_matches('/'),
        enc(&issuer.client_id),
        enc(redirect_uri),
        enc(SCOPE),
        enc(&attempt.challenge()),
        enc(&attempt.state)
    )
}

/// The token request that trades a code for tokens.
fn code_body(code: &str, redirect_uri: &str, client_id: &str, verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        enc(code),
        enc(redirect_uri),
        enc(client_id),
        enc(verifier)
    )
}

/// Why the issuer's answer at the callback is not a code to trade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CallbackError {
    /// Nothing here started a sign-in.
    NotStarted,
    /// A `state` other than the one this sign-in sent: another attempt's,
    /// or someone else's.
    StateMismatch,
    /// The person declined (`error=access_denied`).
    Denied,
    /// The issuer answered with another `error`.
    Failed(String),
    /// Neither a code nor an error.
    NoCode,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted => f.write_str("this browser did not start that sign-in"),
            Self::StateMismatch => {
                f.write_str("the sign-in does not match the one this browser started")
            }
            Self::Denied => f.write_str("the sign-in was declined"),
            Self::Failed(e) => write!(f, "the sign-in was not completed ({e})"),
            Self::NoCode => f.write_str("the issuer sent no code"),
        }
    }
}

/// The code in the issuer's answer at the callback (`search`, its query),
/// checked against the `state` this sign-in sent (`None`: none started).
/// The state is checked first, so an answer that is not this sign-in's,
/// even a refusal, never ends it.
pub(crate) fn parse_callback(
    search: &str,
    expected_state: Option<&str>,
) -> Result<String, CallbackError> {
    let param = |name| env::query_param(search, name);
    let expected = expected_state.ok_or(CallbackError::NotStarted)?;
    if param("state").as_deref() != Some(expected) {
        return Err(CallbackError::StateMismatch);
    }
    match param("error") {
        Some(e) if e == "access_denied" => return Err(CallbackError::Denied),
        Some(e) => return Err(CallbackError::Failed(e)),
        None => {}
    }
    param("code")
        .filter(|c| !c.is_empty())
        .ok_or(CallbackError::NoCode)
}

/// Go to the issuer to sign in; it comes back to `/auth/callback`. A page
/// only: a native app signs in with [`sign_in`].
pub fn begin(issuer: &Issuer) {
    let Some(attempt) = Attempt::new(random) else {
        return;
    };
    let return_to = Some(env::href())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "/".into());
    let url = authorize_url(issuer, &redirect_uri(), &attempt);
    let pending = Pending {
        state: attempt.state,
        verifier: attempt.verifier,
        return_to,
        issuer: issuer.issuer.clone(),
        client_id: issuer.client_id.clone(),
    };
    if let Ok(j) = serde_json::to_string(&pending) {
        env::set(PENDING, &j);
    }
    env::go(&url);
}

/// Sign in natively (a desktop or phone app reaching a node on another
/// computer): the issuer in the system's browser, back on a loopback port
/// of this app's (see `loopback`); the tokens kept in the app's store as a
/// page keeps them. Gives up after five minutes without an answer.
///
/// # Errors
///
/// Why not, for the person: declined, timed out, refused by the issuer.
#[cfg(not(target_arch = "wasm32"))]
#[tracing::instrument(
    name = "drive.app.sign_in",
    skip_all,
    fields(sign_in.issuer = %issuer.issuer, sign_in.outcome = tracing::field::Empty, sign_in.error = tracing::field::Empty)
)]
pub async fn sign_in(issuer: &Issuer) -> Result<(), String> {
    let span = tracing::Span::current();
    let path = crate::mount::page("/auth/callback");
    let result = match Attempt::new(random) {
        Some(attempt) => {
            loopback::sign_in(issuer, attempt, &path, loopback::TIMEOUT, env::go).await
        }
        None => Err(loopback::SignInError::NoRandomness),
    };
    match result {
        Ok(tokens) => {
            save(&tokens);
            span.record("sign_in.outcome", "signed-in");
            Ok(())
        }
        Err(e) => {
            span.record("sign_in.outcome", e.kind());
            span.record("sign_in.error", tracing::field::display(&e));
            Err(e.to_string())
        }
    }
}

/// Whether this page is the issuer coming back.
pub fn is_callback() -> bool {
    cfg!(target_arch = "wasm32") && env::pathname() == crate::mount::page("/auth/callback")
}

/// Finish signing in (on `/auth/callback`): trade the code for tokens and
/// go back where the sign-in began. `Err`: why not (shown on the page).
pub async fn finish() -> Result<(), String> {
    let pending: Option<Pending> = env::get(PENDING).and_then(|j| serde_json::from_str(&j).ok());
    let code = parse_callback(&env::search(), pending.as_ref().map(|p| p.state.as_str()))
        .map_err(|e| e.to_string())?;
    let pending = pending.ok_or("this browser did not start that sign-in")?;
    let body = code_body(
        &code,
        &redirect_uri(),
        &pending.client_id,
        &pending.verifier,
    );
    let t = token_request(&pending.issuer, &pending.client_id, &body, None)
        .await
        .map_err(|error| error.message)?;
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

/// The usable access token, refreshed if it is within `margin_ms` of expiry.
pub async fn fresh_token(margin_ms: f64) -> Option<String> {
    match refresh(margin_ms).await {
        Refresh::Ready(token) => Some(token),
        Refresh::SignedOut | Refresh::Retry => None,
    }
}

pub(crate) enum Refresh {
    Ready(String),
    SignedOut,
    Retry,
}

pub(crate) async fn refresh(margin_ms: f64) -> Refresh {
    let Some(t) = load() else {
        return Refresh::SignedOut;
    };
    if t.expires_at - now() > margin_ms {
        return Refresh::Ready(t.access_token);
    }
    let Some(refresh) = &t.refresh_token else {
        return if t.expires_at > now() {
            Refresh::Ready(t.access_token)
        } else {
            Refresh::SignedOut
        };
    };
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(refresh),
        enc(&t.client_id)
    );
    let result = token_request(&t.issuer, &t.client_id, &body, Some(refresh)).await;
    let current = load();
    // Another refresh or a sign-out may have changed storage during the request.
    match current {
        None => return Refresh::SignedOut,
        Some(current) if current != t => {
            return if current.expires_at > now() {
                Refresh::Ready(current.access_token)
            } else {
                Refresh::Retry
            };
        }
        Some(_) => {}
    }

    match result {
        Ok(new) => {
            save(&new);
            Refresh::Ready(new.access_token)
        }
        Err(error) if error.rejected_grant => {
            sign_out();
            Refresh::SignedOut
        }
        Err(_) if t.expires_at > now() => Refresh::Ready(t.access_token),
        Err(_) => Refresh::Retry,
    }
}

struct TokenRequestError {
    message: String,
    rejected_grant: bool,
}

impl TokenRequestError {
    fn unavailable(message: &str) -> Self {
        Self {
            message: message.into(),
            rejected_grant: false,
        }
    }

    fn response(status: u16, body: &serde_json::Value) -> Self {
        let error = body["error"].as_str();
        let description = body["error_description"].as_str().or(error).unwrap_or("");
        Self {
            message: format!("the issuer refused ({status}): {description}"),
            rejected_grant: matches!(status, 400 | 401) && error == Some("invalid_grant"),
        }
    }
}

async fn token_request(
    issuer: &str,
    client_id: &str,
    body: &str,
    old_refresh: Option<&str>,
) -> Result<Tokens, TokenRequestError> {
    let (status, v) = fetch(
        "POST",
        &format!("{}/oauth2/token", issuer.trim_end_matches('/')),
        &[("content-type", "application/x-www-form-urlencoded")],
        Some(body),
    )
    .await
    .ok_or_else(|| TokenRequestError::unavailable("the issuer is unreachable"))?;
    if status != 200 {
        return Err(TokenRequestError::response(status, &v));
    }
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| TokenRequestError::unavailable("the issuer sent no access token"))?
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
    fn only_an_explicit_invalid_grant_rejects_the_refresh() {
        let invalid_grant = serde_json::json!({ "error": "invalid_grant" });
        assert!(TokenRequestError::response(400, &invalid_grant).rejected_grant);
        assert!(TokenRequestError::response(401, &invalid_grant).rejected_grant);
        assert!(!TokenRequestError::response(503, &invalid_grant).rejected_grant);
        let invalid_client = serde_json::json!({ "error": "invalid_client" });
        assert!(!TokenRequestError::response(401, &invalid_client).rejected_grant);
        assert!(!TokenRequestError::response(429, &serde_json::Value::Null).rejected_grant);
        assert!(!TokenRequestError::unavailable("offline").rejected_grant);
    }

    #[test]
    fn base64url_without_padding() {
        // RFC 4648's test vectors, padding dropped.
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(b"foob"), "Zm9vYg");
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
    }

    fn attempt() -> Attempt {
        let mut draws = ["v".repeat(64), "s".repeat(64)].into_iter();
        Attempt::new(|| draws.next().unwrap_or_default()).expect("an attempt")
    }

    #[test]
    fn a_callback_with_this_state_gives_its_code() {
        let state = "s".repeat(64);
        let search = format!("?code=abc%2F1&state={state}");
        assert_eq!(
            parse_callback(&search, Some(&state)),
            Ok("abc/1".to_string())
        );
    }

    #[test]
    fn a_callback_with_another_state_is_refused_even_a_refusal() {
        assert_eq!(
            parse_callback("?code=abc&state=other", Some("mine")),
            Err(CallbackError::StateMismatch)
        );
        assert_eq!(
            parse_callback("?code=abc", Some("mine")),
            Err(CallbackError::StateMismatch)
        );
        assert_eq!(
            parse_callback("?error=access_denied&state=other", Some("mine")),
            Err(CallbackError::StateMismatch)
        );
        assert_eq!(
            parse_callback("?code=abc&state=mine", None),
            Err(CallbackError::NotStarted)
        );
    }

    #[test]
    fn a_callback_with_an_error_says_which() {
        assert_eq!(
            parse_callback("?error=access_denied&state=mine", Some("mine")),
            Err(CallbackError::Denied)
        );
        assert_eq!(
            parse_callback(
                "?error=server_error&error_description=x&state=mine",
                Some("mine")
            ),
            Err(CallbackError::Failed("server_error".into()))
        );
        // An error wins over a code sent beside it.
        assert_eq!(
            parse_callback(
                "?code=abc&error=temporarily_unavailable&state=mine",
                Some("mine")
            ),
            Err(CallbackError::Failed("temporarily_unavailable".into()))
        );
    }

    #[test]
    fn a_callback_without_a_code_is_refused() {
        assert_eq!(
            parse_callback("?state=mine", Some("mine")),
            Err(CallbackError::NoCode)
        );
        assert_eq!(
            parse_callback("?code=&state=mine", Some("mine")),
            Err(CallbackError::NoCode)
        );
    }

    #[test]
    fn an_attempt_needs_randomness() {
        assert_eq!(Attempt::new(String::new), None);
        assert_eq!(Attempt::new(|| "short".into()), None);
        let a = attempt();
        assert_ne!(a.verifier, a.state);
    }

    #[test]
    fn two_attempts_differ() {
        let a = Attempt::new(env::random_hex32).expect("randomness");
        let b = Attempt::new(env::random_hex32).expect("randomness");
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        assert_eq!(a.verifier.len(), 64);
    }

    #[test]
    fn the_authorize_url_carries_the_s256_challenge_and_state() {
        let issuer = Issuer {
            issuer: "https://auth.example/".into(),
            client_id: "drive".into(),
        };
        let a = attempt();
        let url = authorize_url(&issuer, "http://127.0.0.1:5000/auth/callback", &a);
        assert!(url.starts_with(
            "https://auth.example/oauth2/authorize?response_type=code&client_id=drive&"
        ));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fauth%2Fcallback"));
        assert!(url.contains(&format!(
            "code_challenge={}&code_challenge_method=S256",
            a.challenge()
        )));
        assert!(url.contains(&format!("state={}", a.state)));
        assert!(!url.contains(&a.verifier));
        assert_eq!(
            a.challenge(),
            base64_url(&Sha256::digest(a.verifier.as_bytes()))
        );
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
