//! Browsing a device's disks, live and read-only, without indexing them
//! (docs/browse.md). The node asks the device's agent over its connection
//! ([`drive_proto::Browse`]) and answers the app:
//!
//! | Route (`/browse/{space}/{device}/…`) | What |
//! |---|---|
//! | `GET roots` | the volumes, `/` and the home folder (offline: the registry's volumes) |
//! | `GET list?root=&path=&offset=&limit=&hidden=` | a page of a folder (offline: the last page seen, marked `offline`) |
//! | `GET file?root=&path=&name=` | a file's bytes, streamed in chunks, `Range` honoured |
//! | `GET sidecar/{kind}?root=&path=&size=&mtime=` | a thumbnail or peaks, made by the device, keyed by content |
//! | `POST index` `{root, path, name}` | make the folder a location (the agent's `location add`) |
//!
//! Only members of the space's org (`/orgs`' issuer) may browse its
//! devices: not people of orgs it shared a project with. Refusals log one
//! `warn`. The agent confines every path to its root and never writes;
//! this side only passes requests on.
//!
//! The node keeps a little of what it saw (`data/browse.db`): each folder
//! page's last answer and each browsed file's content key (by size and
//! time), so that while the device is off its folders still list (marked
//! offline) and their thumbnails already made still show.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use drive_proto::{Browse, Browsed, READ_MAX};
use drive_sidecars::{Kind, kinds_for};
use serde::Deserialize;
use tokio::sync::Mutex;
use turso::Value;

use crate::rooms::{FetchError, Rooms};
use crate::sidecars::{self, Sidecars};

/// How long the node waits for a device: to list or stat, to read a
/// chunk, to make sidecars.
const QUICK: Duration = Duration::from_secs(20);
const CHUNK: Duration = Duration::from_secs(60);
const MAKE: Duration = Duration::from_secs(180);
/// Folder pages kept for offline browsing, at most.
const KEEP_PAGES: i64 = 20_000;

/// What the node remembers of browsing (`data/browse.db`).
pub struct Seen {
    conn: Mutex<turso::Connection>,
}

fn text(v: Value) -> String {
    match v {
        Value::Text(t) => t,
        _ => String::new(),
    }
}

impl Seen {
    pub async fn open(data: &Path) -> Result<Seen, String> {
        let db = turso::Builder::new_local(&data.join("browse.db").to_string_lossy())
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let conn = db.connect().map_err(|e| e.to_string())?;
        for sql in [
            "CREATE TABLE IF NOT EXISTS pages (space TEXT NOT NULL, device TEXT NOT NULL, root TEXT NOT NULL, \
             path TEXT NOT NULL, page TEXT NOT NULL, json TEXT NOT NULL, at INTEGER NOT NULL, \
             PRIMARY KEY (space, device, root, path, page))",
            "CREATE INDEX IF NOT EXISTS pages_at ON pages (at)",
            "CREATE TABLE IF NOT EXISTS keys (space TEXT NOT NULL, device TEXT NOT NULL, root TEXT NOT NULL, \
             path TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL, key TEXT NOT NULL, \
             PRIMARY KEY (space, device, root, path))",
        ] {
            conn.execute(sql, ()).await.map_err(|e| e.to_string())?;
        }
        Ok(Seen {
            conn: Mutex::new(conn),
        })
    }

    async fn put_page(&self, at: [&str; 5], json: &str) {
        let conn = self.conn.lock().await;
        let mut args: Vec<Value> = at.iter().map(|s| Value::Text((*s).into())).collect();
        args.push(Value::Text(json.into()));
        args.push(Value::Integer(drive_files::now_ms()));
        let _ = conn
            .execute(
                "INSERT OR REPLACE INTO pages (space, device, root, path, page, json, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                args,
            )
            .await;
        let _ = conn
            .execute(
                "DELETE FROM pages WHERE at < (SELECT at FROM pages ORDER BY at DESC LIMIT 1 OFFSET ?1)",
                vec![Value::Integer(KEEP_PAGES)],
            )
            .await;
    }

    async fn page(&self, at: [&str; 5]) -> Option<(String, i64)> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT json, at FROM pages WHERE space = ?1 AND device = ?2 AND root = ?3 AND path = ?4 AND page = ?5",
                at.iter()
                    .map(|s| Value::Text((*s).into()))
                    .collect::<Vec<_>>(),
            )
            .await
            .ok()?;
        let r = rows.next().await.ok()??;
        let at = match r.get_value(1).ok()? {
            Value::Integer(n) => n,
            _ => 0,
        };
        Some((text(r.get_value(0).ok()?), at))
    }

    async fn put_key(&self, at: [&str; 4], size: i64, mtime: i64, key: &str) {
        let conn = self.conn.lock().await;
        let mut args: Vec<Value> = at.iter().map(|s| Value::Text((*s).into())).collect();
        args.extend([
            Value::Integer(size),
            Value::Integer(mtime),
            Value::Text(key.into()),
        ]);
        let _ = conn
            .execute(
                "INSERT OR REPLACE INTO keys (space, device, root, path, size, mtime, key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                args,
            )
            .await;
    }

    /// The content key last seen for this file, if it has the same size
    /// and time still.
    async fn key(&self, at: [&str; 4], size: i64, mtime: i64) -> Option<String> {
        let conn = self.conn.lock().await;
        let mut args: Vec<Value> = at.iter().map(|s| Value::Text((*s).into())).collect();
        args.extend([Value::Integer(size), Value::Integer(mtime)]);
        let mut rows = conn
            .query(
                "SELECT key FROM keys WHERE space = ?1 AND device = ?2 AND root = ?3 AND path = ?4 AND size = ?5 AND mtime = ?6",
                args,
            )
            .await
            .ok()?;
        let r = rows.next().await.ok()??;
        Some(text(r.get_value(0).ok()?))
    }
}

#[derive(Clone)]
pub struct Routes {
    pub rooms: Arc<Rooms>,
    pub sc: Arc<Sidecars>,
    pub seen: Arc<Seen>,
}

type Q = Query<HashMap<String, String>>;

fn q<'a>(q: &'a HashMap<String, String>, k: &str) -> &'a str {
    q.get(k).map_or("", String::as_str)
}

fn answer(status: StatusCode, state: &str, why: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "state": state, "why": why })),
    )
        .into_response()
}

/// The device's name, or `None` when the space has no such device.
async fn device_name(rooms: &Rooms, space: &str, device: &str) -> Option<String> {
    rooms
        .read(&drive_files::space_doc(space), |d| {
            drive_files::space::devices(d)
                .into_iter()
                .find(|x| x.id == device)
                .map(|x| x.name)
        })
        .await
}

/// A failed ask as the app sees it.
fn failed(e: FetchError, name: &str, space: &str, device: &str) -> Response {
    match e {
        FetchError::Offline(_) => answer(
            StatusCode::SERVICE_UNAVAILABLE,
            "offline",
            &format!("{name} is offline"),
        ),
        FetchError::Timeout => answer(
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            &format!("{name} did not answer (its agent may be older than browsing)"),
        ),
        FetchError::Refused(why) => {
            let outside =
                why.contains("outside") || why == "not shown" || why.contains("not a volume");
            if outside {
                tracing::warn!(browse.outcome = "refused", browse.space = %space, browse.device = %device, browse.why = %why, "a device refused a path outside what it lets be browsed");
                answer(StatusCode::FORBIDDEN, "refused", &why)
            } else {
                answer(StatusCode::NOT_FOUND, "missing", &why)
            }
        }
        FetchError::NoLocation => answer(StatusCode::NOT_FOUND, "missing", "no such device"),
    }
}

/// Every browse request: signed in, and a member of the space's own org
/// (not of an org it shared a project with: a project is not a disk).
pub async fn guard(
    State(rooms): State<Arc<Rooms>>,
    peer: crate::Remote,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let local = crate::auth::is_local(crate::peer_addr(&peer), req.headers());
    let space = req
        .uri()
        .path()
        .trim_start_matches('/')
        .split('/')
        .nth(1)
        .map(crate::percent_decode)
        .unwrap_or_default();
    let Some(who) = rooms
        .auth
        .principal(crate::auth::bearer(req.headers()).as_deref(), local)
        .await
    else {
        tracing::warn!(auth.outcome = "unauthenticated", browse.space = %space, "browsing a device without signing in");
        return (StatusCode::UNAUTHORIZED, "sign in").into_response();
    };
    if space.is_empty() || !who.member_of(&space) {
        tracing::warn!(auth.outcome = "denied", auth.who = %who.who, browse.space = %space, "browsing a device of an org the person is not in");
        return (StatusCode::FORBIDDEN, "not one of your orgs").into_response();
    }
    next.run(req).await
}

/// `GET /browse/{space}/{device}/roots`.
pub async fn roots(
    UrlPath((space, device)): UrlPath<(String, String)>,
    State(s): State<Routes>,
) -> Response {
    let Some(name) = device_name(&s.rooms, &space, &device).await else {
        return answer(StatusCode::NOT_FOUND, "missing", "no such device");
    };
    let at = [space.as_str(), device.as_str(), "", "", "roots"];
    match s.rooms.browse(&space, &device, Browse::Roots, QUICK).await {
        Ok(Browsed::Roots(r)) => {
            let roots = serde_json::to_value(&r).unwrap_or_default();
            s.seen.put_page(at, &roots.to_string()).await;
            Json(serde_json::json!({ "online": true, "device_name": name, "roots": roots }))
                .into_response()
        }
        Ok(_) => answer(
            StatusCode::BAD_GATEWAY,
            "failed",
            "an answer of the wrong kind",
        ),
        Err(FetchError::Offline(_)) | Err(FetchError::Timeout) => {
            // What was seen last, else the volumes the registry has.
            let (roots, as_of) = match s.seen.page(at).await {
                Some((j, t)) => (serde_json::from_str(&j).unwrap_or_default(), t),
                None => {
                    let vols = s
                        .rooms
                        .read(&drive_files::space_doc(&space), drive_files::space::volumes)
                        .await;
                    let r: Vec<serde_json::Value> = vols
                        .into_iter()
                        .filter(|v| v.device == device)
                        .map(|v| {
                            let label = Path::new(&v.mount).file_name().map_or_else(
                                || "System".to_string(),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            serde_json::json!({ "mount": v.mount, "name": label, "fs": v.fs,
                                "total": v.total.max(0), "free": v.free.max(0),
                                "removable": v.removable, "home": false })
                        })
                        .collect();
                    (serde_json::Value::Array(r), 0)
                }
            };
            Json(
                serde_json::json!({ "online": false, "device_name": name, "roots": roots,
                "as_of": as_of, "why": format!("{name} is offline") }),
            )
            .into_response()
        }
        Err(e) => failed(e, &name, &space, &device),
    }
}

/// `GET /browse/{space}/{device}/list?root=&path=&offset=&limit=&hidden=1`.
pub async fn list(
    UrlPath((space, device)): UrlPath<(String, String)>,
    State(s): State<Routes>,
    Query(p): Q,
) -> Response {
    let Some(name) = device_name(&s.rooms, &space, &device).await else {
        return answer(StatusCode::NOT_FOUND, "missing", "no such device");
    };
    let (root, path) = (q(&p, "root"), q(&p, "path").trim_matches('/'));
    let offset: u32 = q(&p, "offset").parse().unwrap_or(0);
    let limit: u32 = q(&p, "limit").parse().unwrap_or(500);
    let hidden = q(&p, "hidden") == "1";
    let page = format!("{offset}:{limit}:{}", u8::from(hidden));
    let at = [space.as_str(), device.as_str(), root, path, page.as_str()];
    let ask = Browse::List {
        root: root.into(),
        path: path.into(),
        offset,
        limit,
        hidden,
    };
    match s.rooms.browse(&space, &device, ask, QUICK).await {
        Ok(Browsed::Listing(l)) => {
            let mut v = serde_json::to_value(&l).unwrap_or_default();
            s.seen.put_page(at, &v.to_string()).await;
            v["online"] = true.into();
            v["path"] = path.into();
            Json(v).into_response()
        }
        Ok(_) => answer(
            StatusCode::BAD_GATEWAY,
            "failed",
            "an answer of the wrong kind",
        ),
        Err(e @ (FetchError::Offline(_) | FetchError::Timeout)) => match s.seen.page(at).await {
            Some((j, t)) => {
                let mut v: serde_json::Value = serde_json::from_str(&j).unwrap_or_default();
                v["online"] = false.into();
                v["path"] = path.into();
                v["as_of"] = t.into();
                v["why"] = format!("{name} is offline").into();
                Json(v).into_response()
            }
            None => failed(e, &name, &space, &device),
        },
        Err(e) => failed(e, &name, &space, &device),
    }
}

/// `GET /browse/{space}/{device}/file?root=&path=&name=`: the file's
/// bytes, streamed a chunk at a time from the device; a `Range` header
/// gets just those bytes (`206`), so audio and video seek.
pub async fn file(
    UrlPath((space, device)): UrlPath<(String, String)>,
    State(s): State<Routes>,
    Query(p): Q,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(dname) = device_name(&s.rooms, &space, &device).await else {
        return answer(StatusCode::NOT_FOUND, "missing", "no such device");
    };
    let (root, path) = (q(&p, "root").to_string(), q(&p, "path").to_string());
    let stat = Browse::Read {
        root: root.clone(),
        path: path.clone(),
        start: 0,
        len: 0,
    };
    let size = match s.rooms.browse(&space, &device, stat, QUICK).await {
        Ok(Browsed::Chunk(c)) => c.size,
        Ok(_) => {
            return answer(
                StatusCode::BAD_GATEWAY,
                "failed",
                "an answer of the wrong kind",
            );
        }
        Err(e) => return failed(e, &dname, &space, &device),
    };
    let range = headers.get("range").and_then(|v| v.to_str().ok());
    let (start, end, partial) = match range {
        _ if size == 0 => (0, 0, false),
        Some(h) => match crate::byte_range(h, size) {
            Some((a, b)) => (a, b, true),
            None => {
                let mut r = Response::new(axum::body::Body::empty());
                *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                    r.headers_mut().insert("content-range", v);
                }
                return r;
            }
        },
        None => (0, size - 1, false),
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    let (rooms, sp, dev) = (s.rooms.clone(), space.clone(), device.clone());
    let stream = futures::stream::unfold(start, move |pos| {
        let (rooms, sp, dev, root, path) = (
            rooms.clone(),
            sp.clone(),
            dev.clone(),
            root.clone(),
            path.clone(),
        );
        async move {
            if len == 0 || pos > end {
                return None;
            }
            let want = (end - pos + 1).min(u64::from(READ_MAX)) as u32;
            let ask = Browse::Read {
                root,
                path,
                start: pos,
                len: want,
            };
            match rooms.browse(&sp, &dev, ask, CHUNK).await {
                Ok(Browsed::Chunk(c)) if !c.bytes.is_empty() => {
                    let next = pos + c.bytes.len() as u64;
                    Some((Ok(axum::body::Bytes::from(c.bytes)), next))
                }
                Ok(_) => Some((Err(std::io::Error::other("the file got shorter")), end + 1)),
                Err(e) => Some((Err(std::io::Error::other(format!("{e:?}"))), end + 1)),
            }
        }
    });
    let mut r = Response::new(axum::body::Body::from_stream(stream));
    if partial {
        *r.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
            r.headers_mut().insert("content-range", v);
        }
    }
    let name = match q(&p, "name") {
        "" => path_name(q(&p, "path")).to_string(),
        n => n.to_string(),
    };
    let safe: String = name
        .chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(drive_files::tree::mime_of(&safe)) {
        h.insert("content-type", v);
    }
    // Never shown as a page of the node's own (an HTML file would run).
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert("content-disposition", v);
    }
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(
        "cache-control",
        HeaderValue::from_static("private, max-age=60"),
    );
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert("content-length", v);
    }
    r
}

fn path_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// `GET /browse/{space}/{device}/sidecar/{kind}?root=&path=&size=&mtime=`:
/// a browsed file's thumbnail or peaks. Found by the content key the node
/// saw for this file (same size and time) without asking the device, so it
/// shows while the device is off; else the device says the key and, when
/// the node lacks it, makes it. `make=0`: only look.
pub async fn sidecar(
    UrlPath((space, device, kind)): UrlPath<(String, String, String)>,
    State(s): State<Routes>,
    Query(p): Q,
) -> Response {
    let Some(kind) = Kind::parse(&kind) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (root, path) = (q(&p, "root"), q(&p, "path"));
    let all = kinds_for(path_name(path));
    if !all.contains(&kind) {
        return sidecars::state(
            StatusCode::NOT_FOUND,
            "no-generator",
            "nothing derives this from that kind of file",
        );
    }
    let size: i64 = q(&p, "size").parse().unwrap_or(-1);
    let mtime: i64 = q(&p, "mtime").parse().unwrap_or(-1);
    let at = [space.as_str(), device.as_str(), root, path];
    let known = s.seen.key(at, size, mtime).await;
    let fresh = |r: &sidecars::Row| r.version >= kind.version();
    if let Some(k) = &known
        && let Some(r) = s.sc.row(k, kind).await
        && fresh(&r)
    {
        return sidecars::serve(&s.sc, &r).await;
    }
    let online = s.rooms.agent_online(&space, &device).await;
    if !online {
        if let Some(k) = &known
            && let Some(r) = s.sc.row(k, kind).await
        {
            return sidecars::serve(&s.sc, &r).await;
        }
        return sidecars::state(StatusCode::NOT_FOUND, "offline", "its device is offline");
    }
    let key = match known {
        Some(k) => k,
        None => {
            let ask = Browse::Key {
                root: root.into(),
                path: path.into(),
            };
            match s.rooms.browse(&space, &device, ask, QUICK).await {
                Ok(Browsed::Key(k)) if drive_sidecars::valid_key(&k) => {
                    if size >= 0 && mtime >= 0 {
                        s.seen.put_key(at, size, mtime, &k).await;
                    }
                    k
                }
                Ok(_) => return sidecars::state(StatusCode::NOT_FOUND, "failed", "no content key"),
                Err(e) => {
                    let name = device_name(&s.rooms, &space, &device)
                        .await
                        .unwrap_or_default();
                    return failed(e, &name, &space, &device);
                }
            }
        }
    };
    if let Some(r) = s.sc.row(&key, kind).await
        && fresh(&r)
    {
        return sidecars::serve(&s.sc, &r).await;
    }
    if q(&p, "make") == "0" {
        return sidecars::state(StatusCode::NOT_FOUND, "missing", "");
    }
    let lock = s.sc.lock_for(&key);
    let _one = lock.lock().await;
    if let Some(r) = s.sc.row(&key, kind).await
        && fresh(&r)
    {
        return sidecars::serve(&s.sc, &r).await;
    }
    if let Some(why) = s.sc.miss(&key, kind).await {
        return sidecars::state(StatusCode::NOT_FOUND, "no-generator", &why);
    }
    let ask = Browse::Sidecars {
        root: root.into(),
        path: path.into(),
        kinds: all.iter().map(|k| k.as_str().to_string()).collect(),
        force: false,
    };
    let outcome = s.rooms.browse(&space, &device, ask, MAKE).await;
    if let Some(r) = s.sc.row(&key, kind).await {
        return sidecars::serve(&s.sc, &r).await;
    }
    match outcome {
        Err(FetchError::Refused(w)) if w.starts_with("no generator") => {
            s.sc.record_miss(std::slice::from_ref(&key), &all, &w).await;
            sidecars::state(StatusCode::NOT_FOUND, "no-generator", &w)
        }
        Err(FetchError::Refused(w)) => sidecars::state(StatusCode::NOT_FOUND, "failed", &w),
        Err(FetchError::Offline(_) | FetchError::Timeout) => sidecars::state(
            StatusCode::NOT_FOUND,
            "offline",
            "its device did not answer",
        ),
        _ => sidecars::state(StatusCode::NOT_FOUND, "missing", "made, but not this kind"),
    }
}

#[derive(Deserialize)]
pub struct IndexReq {
    pub root: String,
    pub path: String,
    pub name: String,
}

/// `POST /browse/{space}/{device}/index` `{root, path, name}`: the device
/// makes the folder a location and indexes it.
pub async fn index(
    UrlPath((space, device)): UrlPath<(String, String)>,
    State(s): State<Routes>,
    Json(r): Json<IndexReq>,
) -> Response {
    let Some(dname) = device_name(&s.rooms, &space, &device).await else {
        return answer(StatusCode::NOT_FOUND, "missing", "no such device");
    };
    let name = r.name.trim();
    if name.is_empty() {
        return answer(
            StatusCode::BAD_REQUEST,
            "refused",
            "a location needs a name",
        );
    }
    let ask = Browse::Index {
        root: r.root,
        path: r.path.trim_matches('/').to_string(),
        name: name.to_string(),
    };
    match s.rooms.browse(&space, &device, ask, QUICK).await {
        Ok(Browsed::Key(id)) => {
            tracing::info!(browse.space = %space, browse.device = %device, location.id = %id, "a browsed folder became a location");
            Json(serde_json::json!({ "location": id })).into_response()
        }
        Ok(_) => answer(
            StatusCode::BAD_GATEWAY,
            "failed",
            "an answer of the wrong kind",
        ),
        Err(FetchError::Refused(w)) if w.starts_with("already a location") => {
            answer(StatusCode::CONFLICT, "exists", &w)
        }
        Err(e) => failed(e, &dname, &space, &device),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Auth, Principal};
    use tower::ServiceExt;

    async fn app(dir: &Path) -> axum::Router {
        let store = crate::store::Store::open(&dir.join("drive.db"))
            .await
            .unwrap();
        // An issuer that is never reached: tokens are remembered below.
        let rooms = Arc::new(Rooms::new(
            store,
            Auth::new(Some("http://127.0.0.1:9".into())),
        ));
        let member = Principal {
            who: "member".into(),
            orgs: vec!["home".into()],
            all: false,
        };
        let other = Principal {
            who: "other".into(),
            orgs: vec!["elsewhere".into()],
            all: false,
        };
        rooms.auth.remember("tm", member).await;
        rooms.auth.remember("to", other).await;
        axum::Router::new()
            .route(
                "/browse/{space}/{device}/roots",
                axum::routing::get(|| async { "ok" }),
            )
            .layer(axum::middleware::from_fn_with_state(rooms.clone(), guard))
    }

    async fn status(app: &axum::Router, token: Option<&str>, url: &str) -> StatusCode {
        let mut req = axum::http::Request::get(url);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        app.clone()
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn only_members_of_the_devices_org_browse() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        let url = "/browse/home/dev1/roots";
        assert_eq!(status(&app, None, url).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            status(&app, Some("bad"), url).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(status(&app, Some("to"), url).await, StatusCode::FORBIDDEN);
        assert_eq!(status(&app, Some("tm"), url).await, StatusCode::OK);
        assert_eq!(
            status(&app, Some("tm"), "/browse/elsewhere/dev1/roots").await,
            StatusCode::FORBIDDEN
        );
    }
}
