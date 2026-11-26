//! A native app's sign-in in the system's browser, back on a loopback
//! redirect (RFC 8252 §7.3, §8.3): the app listens on a port of its own
//! on `127.0.0.1`, sends the browser to the issuer with
//! `redirect_uri=http://127.0.0.1:<port>/auth/callback`, and the issuer's
//! answer (the code, or why not) arrives there. The browser tab is told
//! it can be closed; the code is traded for tokens as a page's is.
//!
//! An answer whose `state` is not this sign-in's is shown an error and
//! otherwise ignored (the listener keeps waiting), so another tab or
//! another program on this computer cannot end the sign-in. With no
//! answer in [`TIMEOUT`] the listener gives up.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::{Attempt, CallbackError, Issuer, Tokens, authorize_url, code_body, token_request};

/// How long the browser has to come back.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long one connection has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a request read: its line and headers (a GET has no body).
const MAX_REQUEST: usize = 16 * 1024;

/// Why a loopback sign-in did not end signed in.
#[derive(Debug)]
pub(crate) enum SignInError {
    /// No randomness for the verifier and state.
    NoRandomness,
    /// No port could be listened on.
    Listen(std::io::Error),
    /// The browser did not come back in time.
    TimedOut,
    /// The issuer came back without a code.
    Callback(CallbackError),
    /// The code could not be traded for tokens.
    Token(String),
}

impl SignInError {
    /// A short name for the span.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::NoRandomness => "no-randomness",
            Self::Listen(_) => "listen",
            Self::TimedOut => "timed-out",
            Self::Callback(CallbackError::Denied) => "denied",
            Self::Callback(_) => "callback",
            Self::Token(_) => "token",
        }
    }
}

impl std::fmt::Display for SignInError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRandomness => f.write_str("this computer gave no randomness to sign in with"),
            Self::Listen(e) => write!(f, "the app could not wait for the browser ({e})"),
            Self::TimedOut => f.write_str("the browser did not come back in time; try again"),
            Self::Callback(e) => write!(f, "{e}"),
            Self::Token(e) => f.write_str(e),
        }
    }
}

/// The redirect the issuer is given for a listener on `port`.
pub(crate) fn redirect_uri(port: u16, path: &str) -> String {
    format!("http://{}:{port}{path}", Ipv4Addr::LOCALHOST)
}

/// Sign in at `issuer`: listen on a loopback port, `open` the issuer's
/// page (the system's browser), wait up to `timeout` for it to come back
/// to `path` on that port, and trade the code. The tokens are the
/// caller's to keep.
///
/// # Errors
///
/// [`SignInError`]: no port, no answer in time, an answer without a code
/// (declined, or the issuer's error), or a refused trade.
pub(crate) async fn sign_in(
    issuer: &Issuer,
    attempt: Attempt,
    path: &str,
    timeout: Duration,
    open: impl FnOnce(&str),
) -> Result<Tokens, SignInError> {
    let listener =
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(SignInError::Listen)?;
    let port = listener.local_addr().map_err(SignInError::Listen)?.port();
    let redirect = redirect_uri(port, path);
    open(&authorize_url(issuer, &redirect, &attempt));
    let wait = catch(listener, attempt.state.clone(), path.to_string(), timeout);
    let code = drive_client::on_runtime(wait)
        .await
        .ok_or_else(|| SignInError::Listen(std::io::Error::other("the listener stopped")))??;
    let body = code_body(&code, &redirect, &issuer.client_id, &attempt.verifier);
    token_request(&issuer.issuer, &issuer.client_id, &body, None)
        .await
        .map_err(|e| SignInError::Token(e.message))
}

/// Wait on `listener` for the issuer's answer at `path` with `state`, up
/// to `timeout`; each request is answered with a page.
async fn catch(
    listener: std::net::TcpListener,
    state: String,
    path: String,
    timeout: Duration,
) -> Result<String, SignInError> {
    listener
        .set_nonblocking(true)
        .map_err(SignInError::Listen)?;
    let listener = TcpListener::from_std(listener).map_err(SignInError::Listen)?;
    let answers = async {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            if let Some(outcome) = answer(&mut stream, &path, &state).await {
                return outcome;
            }
        }
    };
    tokio::time::timeout(timeout, answers)
        .await
        .map_err(|_| SignInError::TimedOut)?
        .map_err(SignInError::Callback)
}

/// Answer one connection. `Some`: the sign-in's outcome; `None`: not this
/// sign-in's answer (another path, another state, no request), keep
/// waiting.
async fn answer(
    stream: &mut TcpStream,
    path: &str,
    state: &str,
) -> Option<Result<String, CallbackError>> {
    let target = tokio::time::timeout(REQUEST_TIMEOUT, read_target(stream))
        .await
        .ok()
        .flatten();
    let Some(search) = target.as_deref().and_then(|t| query_at(t, path)) else {
        respond(stream, "404 Not Found", "Not found", "").await;
        return None;
    };
    let outcome = super::parse_callback(search, Some(state));
    let (title, text) = page_text(&outcome);
    respond(stream, "200 OK", title, &text).await;
    (outcome != Err(CallbackError::StateMismatch)).then_some(outcome)
}

/// The request's target (`/auth/callback?code=…`), when it is a GET.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !ends_head(&buf) && buf.len() < MAX_REQUEST {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    request_target(&String::from_utf8_lossy(&buf)).map(str::to_string)
}

fn ends_head(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n")
}

/// `GET <target> HTTP/1.x` → the target.
fn request_target(head: &str) -> Option<&str> {
    let mut parts = head.lines().next()?.split(' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("GET"), Some(target), Some(v)) if v.starts_with("HTTP/") => Some(target),
        _ => None,
    }
}

/// The query of `target` when its path is `path` (`""` with none).
fn query_at<'a>(target: &'a str, path: &str) -> Option<&'a str> {
    let (p, q) = target.split_once('?').unwrap_or((target, ""));
    (p == path).then_some(q)
}

/// What the browser tab says.
fn page_text(outcome: &Result<String, CallbackError>) -> (&'static str, String) {
    match outcome {
        Ok(_) => (
            "Signed in",
            "You can close this tab and go back to Drive.".to_string(),
        ),
        Err(CallbackError::StateMismatch) => (
            "Not this sign-in",
            "This is not the sign-in Drive is waiting for. Close this tab and sign in from the app again.".to_string(),
        ),
        Err(e) => (
            "Not signed in",
            format!("Drive could not sign in: {e}. You can close this tab and try again from the app."),
        ),
    }
}

/// Send a small page and close.
async fn respond(stream: &mut TcpStream, status: &str, title: &str, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{t}</title>\
         <body style=\"font:16px/1.5 system-ui,sans-serif;background:#000;color:#f4f4f5;display:grid;place-items:center;min-height:90vh;text-align:center\">\
         <div><h1 style=\"font-size:22px\">{t}</h1><p style=\"color:#a1a1aa\">{x}</p></div>",
        t = escape(title),
        x = escape(text)
    );
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\n\
         content-security-policy: default-src 'none'; style-src 'unsafe-inline'\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;

    #[test]
    fn only_a_get_at_the_callback_path_is_an_answer() {
        assert_eq!(
            request_target("GET /auth/callback?code=x HTTP/1.1\r\nHost: a\r\n\r\n"),
            Some("/auth/callback?code=x")
        );
        assert_eq!(request_target("POST /auth/callback HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_target("garbage"), None);
        assert_eq!(
            query_at("/auth/callback?code=x", "/auth/callback"),
            Some("code=x")
        );
        assert_eq!(query_at("/auth/callback", "/auth/callback"), Some(""));
        assert_eq!(query_at("/favicon.ico", "/auth/callback"), None);
        assert_eq!(query_at("/auth/callbackx?code=x", "/auth/callback"), None);
    }

    #[test]
    fn the_redirect_is_on_the_loopback_ip_not_localhost() {
        assert_eq!(
            redirect_uri(53124, "/studio/auth/callback"),
            "http://127.0.0.1:53124/studio/auth/callback"
        );
    }

    use std::io::{Read as _, Write as _};

    fn attempt() -> Attempt {
        Attempt::new(crate::env::random_hex32).expect("randomness")
    }

    fn param(url: &str, name: &str) -> String {
        let search = url.split_once('?').map_or("", |(_, q)| q);
        crate::env::query_param(search, name).unwrap_or_default()
    }

    /// What a browser does: GET `url` and read the page.
    fn browse(url: &str) -> String {
        let rest = url.strip_prefix("http://").expect("http");
        let (host, target) = rest.split_once('/').expect("a path");
        let mut s = std::net::TcpStream::connect(host).expect("connect");
        write!(s, "GET /{target} HTTP/1.1\r\nHost: {host}\r\n\r\n").expect("send");
        let mut page = String::new();
        let _ = s.read_to_string(&mut page);
        page
    }

    /// A fake issuer's token endpoint: answers one request with tokens and
    /// hands over the request's body.
    async fn token_endpoint() -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body = loop {
                let n = s.read(&mut chunk).await.expect("read");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= len || n == 0 {
                        assert!(head.starts_with("POST /oauth2/token "), "{head}");
                        break body.to_string();
                    }
                }
            };
            let json = r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":60,"token_type":"Bearer"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                json.len()
            );
            s.write_all(reply.as_bytes()).await.expect("reply");
            let _ = tx.send(body);
        });
        (base, rx)
    }

    #[tokio::test]
    async fn the_browser_coming_back_signs_in() {
        let (base, body) = token_endpoint().await;
        let issuer = Issuer {
            issuer: base,
            client_id: "drive".into(),
        };
        let a = attempt();
        let verifier = a.verifier.clone();
        let (tx, pages) = std::sync::mpsc::channel();
        let open = move |url: &str| {
            let (redirect, state, challenge) = (
                param(url, "redirect_uri"),
                param(url, "state"),
                param(url, "code_challenge"),
            );
            std::thread::spawn(move || {
                // A stray request, another sign-in's answer and a forged
                // refusal are answered and change nothing.
                let base = redirect.trim_end_matches("/auth/callback");
                let stray = browse(&format!("{base}/favicon.ico"));
                let other = browse(&format!("{redirect}?code=theirs&state=other"));
                let forged = browse(&format!("{redirect}?error=access_denied&state=other"));
                let ours = browse(&format!("{redirect}?code=the-code&state={state}"));
                let _ = tx.send((redirect, challenge, stray, other, forged, ours));
            });
        };
        let tokens = sign_in(&issuer, a, "/auth/callback", Duration::from_secs(20), open)
            .await
            .expect("signed in");
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(tokens.client_id, "drive");

        let (redirect, challenge, stray, other, forged, ours) = pages.recv().expect("pages");
        assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
        assert!(stray.starts_with("HTTP/1.1 404"), "{stray}");
        assert!(
            other.contains("not the sign-in Drive is waiting for"),
            "{other}"
        );
        assert!(
            forged.contains("not the sign-in Drive is waiting for"),
            "{forged}"
        );
        assert!(ours.starts_with("HTTP/1.1 200"), "{ours}");
        assert!(ours.contains("You can close this tab"), "{ours}");

        // The trade is the code with this sign-in's redirect and verifier,
        // and the verifier is the one the challenge was made from.
        let body = body.await.expect("the token request");
        let form = |k| crate::env::query_param(&body, k).unwrap_or_default();
        assert_eq!(form("grant_type"), "authorization_code");
        assert_eq!(form("code"), "the-code");
        assert_eq!(form("redirect_uri"), redirect);
        assert_eq!(form("client_id"), "drive");
        assert_eq!(form("code_verifier"), verifier);
        assert_eq!(
            super::super::base64_url(&sha2::Sha256::digest(verifier.as_bytes())),
            challenge
        );
    }

    #[tokio::test]
    async fn a_declined_sign_in_ends_without_a_trade() {
        let issuer = Issuer {
            issuer: "http://127.0.0.1:9".into(),
            client_id: "drive".into(),
        };
        let (tx, page) = std::sync::mpsc::channel();
        let open = move |url: &str| {
            let (redirect, state) = (param(url, "redirect_uri"), param(url, "state"));
            std::thread::spawn(move || {
                let _ = tx.send(browse(&format!(
                    "{redirect}?error=access_denied&state={state}"
                )));
            });
        };
        let err = sign_in(
            &issuer,
            attempt(),
            "/auth/callback",
            Duration::from_secs(20),
            open,
        )
        .await
        .expect_err("declined");
        assert!(
            matches!(err, SignInError::Callback(CallbackError::Denied)),
            "{err:?}"
        );
        let page = page.recv().expect("page");
        assert!(
            page.contains("declined") && page.contains("close this tab"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn no_answer_times_out() {
        let issuer = Issuer {
            issuer: "http://127.0.0.1:9".into(),
            client_id: "drive".into(),
        };
        let err = sign_in(
            &issuer,
            attempt(),
            "/auth/callback",
            Duration::from_millis(200),
            |_| {},
        )
        .await
        .expect_err("timed out");
        assert!(matches!(err, SignInError::TimedOut), "{err:?}");
    }

    #[test]
    fn the_page_escapes_what_the_issuer_sent() {
        let (_, text) = page_text(&Err(CallbackError::Failed("<script>".into())));
        assert!(!escape(&text).contains("<script>"));
    }
}
