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
    /// An org's members, by its slug, as the issuer last said.
    members: Mutex<HashMap<String, (Instant, Vec<drive_proto::OrgMember>)>>,
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
            members: Mutex::new(HashMap::new()),
        }
    }

    /// Take `token` as `p`'s for a minute without asking the issuer (tests).
    #[doc(hidden)]
    pub async fn remember(&self, token: &str, p: Principal) {
        let key: [u8; 32] = *blake3::hash(token.as_bytes()).as_bytes();
        self.cache.lock().await.insert(key, (Instant::now(), p));
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

    /// Who is in `org`, as the issuer says (asked with the token of one of
    /// its members, which the caller has checked), kept a minute. Without
    /// an issuer nobody is listed: the node knows no people.
    pub async fn members(
        &self,
        token: Option<&str>,
        org: &str,
    ) -> Result<Vec<drive_proto::OrgMember>, String> {
        let Some(issuer) = &self.issuer else {
            return Ok(Vec::new());
        };
        if let Some((at, m)) = self.members.lock().await.get(org)
            && at.elapsed() < KEEP
        {
            return Ok(m.clone());
        }
        let token = token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("asking the issuer needs a sign-in")?;
        let post = |path: &str, body: String| {
            self.http
                .post(format!("{issuer}/auth/organization/{path}"))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .body(body)
                .send()
        };
        let list: serde_json::Value = answer(post("list", "{}".into()).await).await?;
        let id = list
            .as_array()
            .into_iter()
            .flatten()
            .find(|b| b["organization"]["slug"] == org)
            .and_then(|b| b["organization"]["id"].as_str())
            .ok_or_else(|| format!("the issuer has no org {org} for this person"))?
            .to_string();
        let body = serde_json::json!({ "organization_id": id }).to_string();
        let found: serde_json::Value = answer(post("members", body).await).await?;
        let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        let mut members: Vec<drive_proto::OrgMember> = found
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| drive_proto::OrgMember {
                account: text(&m["user"]["id"]),
                email: text(&m["user"]["email"]),
                name: text(&m["user"]["name"]),
                image: text(&m["user"]["image"]),
                role: text(&m["member"]["role"]),
            })
            .filter(|m| !m.account.is_empty())
            .collect();
        members.sort_by(|a, b| {
            let key = |m: &drive_proto::OrgMember| {
                if m.name.is_empty() {
                    m.email.to_lowercase()
                } else {
                    m.name.to_lowercase()
                }
            };
            key(a).cmp(&key(b))
        });
        let mut cache = self.members.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < KEEP);
        cache.insert(org.to_string(), (Instant::now(), members.clone()));
        Ok(members)
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

/// An issuer's JSON answer, or why not.
async fn answer(r: Result<reqwest::Response, reqwest::Error>) -> Result<serde_json::Value, String> {
    let r = r.map_err(|e| format!("the issuer is unreachable: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("the issuer answered {}", r.status()));
    }
    r.json().await.map_err(|e| e.to_string())
}

/// The token of a request: its `Bearer`, or the `drive_token` cookie the
/// browser apps set (plain links, a download, carry no header).
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let from_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string());
    let from_cookie = || {
        headers
            .get_all(axum::http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| {
                c.split(';').find_map(|kv| {
                    let (k, v) = kv.trim().split_once('=')?;
                    (k == "drive_token").then(|| percent(v))
                })
            })
    };
    from_header.or_else(from_cookie).filter(|t| !t.is_empty())
}

fn percent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_local_host(host: &str) -> bool {
    let bare = |name: &str| {
        host == name
            || host
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
    };
    bare("127.0.0.1") || bare("localhost") || bare("[::1]")
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
    from_loopback && !forwarded && is_local_host(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_host_rejects_evil_suffixes() {
        let lo = Some("127.0.0.1:43210".parse().unwrap());
        for good in [
            "localhost",
            "localhost:8096",
            "127.0.0.1",
            "127.0.0.1:8096",
            "[::1]",
            "[::1]:8096",
        ] {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::HOST, good.parse().unwrap());
            assert!(is_local(lo, &h), "{good} should be local");
        }
        for bad in [
            "",
            "localhost.evil.example",
            "localhost.evil.example:8096",
            "127.0.0.1.evil.example",
            "[::1].evil.example",
            "localhost:notaport",
            "localhost:",
        ] {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::HOST, bad.parse().unwrap());
            assert!(!is_local(lo, &h), "{bad} must not be local");
        }
    }

    #[test]
    fn a_token_from_the_header_or_the_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            "theme=dark; drive_token=abc%2Edef".parse().unwrap(),
        );
        assert_eq!(bearer(&h).as_deref(), Some("abc.def"));
        h.insert("authorization", "Bearer xyz".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("xyz"));
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

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
        tailnet.insert("host", "workstation.example.ts.net:9444".parse().unwrap());
        assert!(!is_local(Some(lo), &tailnet));
        assert!(!is_local(Some("192.168.1.9:5000".parse().unwrap()), &h));
        assert!(!is_local(None, &h));
    }
}
