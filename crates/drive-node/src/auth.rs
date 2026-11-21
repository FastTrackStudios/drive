//! Who is asking, and which orgs they are in: the auth server (the issuer,
//! `--issuer https://auth.fasttrackstudio.app`) is the one place people,
//! orgs and memberships live; the node only asks it.
//!
//! A browser signs in with the issuer (OIDC, PKCE, client `drive`) and
//! hands the node its access token: in the sync handshake, and as a
//! `Bearer` on HTTP. The node asks the issuer which orgs that token's
//! person is in (`POST /auth/organization/list`, allowed to first-party
//! clients) and who they are (`/oauth2/userinfo`), and keeps the answer a
//! minute. A person may read and write the documents of their orgs, and of
//! projects other orgs shared with one of them.
//!
//! Without an issuer the node is open (development). A connection from
//! this computer itself (loopback, not forwarded by a proxy) is trusted
//! either way: it is this machine's own agent, or someone at its keyboard.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use tokio::sync::Mutex;

/// How long an answer from the issuer is kept.
const KEEP: Duration = Duration::from_secs(60);

/// Who a request or a connection is.
#[derive(Clone, Debug, PartialEq)]
pub struct Principal {
    /// Their email (or name); `this computer` for a trusted local one.
    pub who: String,
    /// The orgs they are in (slugs).
    pub orgs: Vec<String>,
    /// Everything (no issuer, or this computer itself).
    pub all: bool,
}

impl Principal {
    pub fn everyone(who: &str) -> Principal {
        Principal {
            who: who.into(),
            orgs: Vec::new(),
            all: true,
        }
    }

    pub fn member_of(&self, org: &str) -> bool {
        self.all || self.orgs.iter().any(|o| o == org)
    }
}

pub struct Auth {
    pub issuer: Option<String>,
    http: reqwest::Client,
    cache: Mutex<HashMap<[u8; 32], (Instant, Principal)>>,
}

impl Auth {
    pub fn new(issuer: Option<String>) -> Auth {
        Auth {
            issuer: issuer.map(|i| i.trim_end_matches('/').to_string()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("an HTTP client"),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Who this is: `None` when the node needs a sign-in and has none (or
    /// a token the issuer refuses).
    pub async fn principal(&self, token: Option<&str>, local: bool) -> Option<Principal> {
        let Some(issuer) = &self.issuer else {
            return Some(Principal::everyone("anyone (no issuer)"));
        };
        if local && token.is_none() {
            return Some(Principal::everyone("this computer"));
        }
        let token = token?.trim();
        if token.is_empty() {
            return None;
        }
        let key: [u8; 32] = *blake3::hash(token.as_bytes()).as_bytes();
        if let Some((at, p)) = self.cache.lock().await.get(&key)
            && at.elapsed() < KEEP
        {
            return Some(p.clone());
        }
        let p = match self.ask(issuer, token).await {
            Ok(p) => p,
            Err(why) => {
                tracing::warn!(auth.outcome = "refused", auth.why = %why, "the issuer did not accept a token");
                return None;
            }
        };
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < KEEP);
        cache.insert(key, (Instant::now(), p.clone()));
        Some(p)
    }

    async fn ask(&self, issuer: &str, token: &str) -> Result<Principal, String> {
        let orgs = self
            .http
            .post(format!("{issuer}/auth/organization/list"))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| format!("the issuer is unreachable: {e}"))?;
        if !orgs.status().is_success() {
            return Err(format!("the issuer answered {}", orgs.status()));
        }
        let list: serde_json::Value = orgs.json().await.map_err(|e| e.to_string())?;
        let mut slugs: Vec<String> = list
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|b| b["organization"]["slug"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        slugs.sort();
        slugs.dedup();
        // Who: best effort (a token without the email scope still works).
        let who = match self
            .http
            .get(format!("{issuer}/oauth2/userinfo"))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| {
                    v["email"]
                        .as_str()
                        .or(v["name"].as_str())
                        .or(v["sub"].as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Principal {
            who,
            orgs: slugs,
            all: false,
        })
    }
}

/// The `Bearer` token of a request, if any.
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Whether a request comes from this computer itself: from loopback, not
/// through a proxy (tailscale serve and the like forward from loopback
/// too, but say so in their headers), addressed to a local name.
pub fn is_local(addr: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    let from_loopback = addr.is_some_and(|a| a.ip().is_loopback());
    let forwarded = [
        "x-forwarded-for",
        "forwarded",
        "x-real-ip",
        "tailscale-user-login",
        "x-forwarded-host",
    ]
    .iter()
    .any(|h| headers.contains_key(*h));
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let local_name =
        host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]");
    from_loopback && !forwarded && local_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_means_this_computer_not_a_proxy() {
        let mut h = HeaderMap::new();
        h.insert("host", "127.0.0.1:8096".parse().unwrap());
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert!(is_local(Some(lo), &h));
        let mut proxied = h.clone();
        proxied.insert("x-forwarded-for", "100.64.0.7".parse().unwrap());
        assert!(!is_local(Some(lo), &proxied));
        let mut tailnet = HeaderMap::new();
        tailnet.insert(
            "host",
            "thebattleship.tail666c4b.ts.net:9444".parse().unwrap(),
        );
        assert!(!is_local(Some(lo), &tailnet));
        assert!(!is_local(Some("192.168.1.9:5000".parse().unwrap()), &h));
        assert!(!is_local(None, &h));
    }
}
