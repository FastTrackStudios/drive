//! `drive-node`: an always-on Drive peer. It keeps every document it is
//! given (in Turso), passes live edits and presence between the peers that
//! have a document open, keeps blobs, serves apps' web builds, and serves
//! the sites agents build (`/sites/<space>/<id>/`).

pub mod auth;
pub mod backup;
pub mod browse;
pub mod catalog;
pub mod migrate;
pub mod rooms;
pub mod share;
pub mod sidecars;
pub mod sites;
pub mod store;
pub mod transfer;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use drive_files::blob::BlobStore;
use drive_files::storage::StorageConfig;
use tower_http::services::{ServeDir, ServeFile};

use rooms::Rooms;

/// How a node runs beyond its data and apps.
#[derive(Clone, Default)]
pub struct Options {
    /// The org apps use when they have not picked one; also the org that
    /// documents from before org-scoped ids are moved into.
    pub org: Option<String>,
    /// Spaces from before orgs that move into an org of another name
    /// (`cody` → `codywright`).
    pub space_org: Vec<(String, String)>,
    /// The auth server (`https://auth.fasttrackstudio.app`): who people
    /// are and which orgs they are in. None: the node is open.
    pub issuer: Option<String>,
    /// Where daily backups also go (the R2 bucket, Starcommand).
    pub backup_to: Option<StorageConfig>,
    /// Make missing sidecars of the files the node keeps in the background
    /// (on by default; `--no-backfill` turns it off).
    pub no_backfill: bool,
    /// How the node's iroh endpoint reaches devices (docs/transfer.md):
    /// our own relay, or none at all (`--iroh-local`); `--no-iroh` runs
    /// none and fetches every file the old way.
    pub iroh: drive_transfer::Settings,
    pub no_iroh: bool,
}

type Remote = Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>;

fn peer_addr(p: &Remote) -> Option<std::net::SocketAddr> {
    p.as_ref().map(|e| e.0.0)
}

/// `/orgs`: the orgs this person is in (the issuer says; without one, every
/// org the node keeps), the one to open by default, and how to sign in.
async fn orgs(
    State((rooms, default)): State<(Arc<Rooms>, String)>,
    headers: axum::http::HeaderMap,
    peer: Remote,
) -> axum::Json<serde_json::Value> {
    let local = auth::is_local(peer_addr(&peer), &headers);
    let who = rooms
        .auth
        .principal(auth::bearer(&headers).as_deref(), local)
        .await;
    let sign_in = rooms
        .auth
        .issuer
        .as_ref()
        .map(|i| serde_json::json!({ "issuer": i, "client_id": "drive" }));
    let Some(who) = who else {
        return axum::Json(
            serde_json::json!({ "signed_in": false, "auth": sign_in, "orgs": [], "default": "" }),
        );
    };
    let mut orgs = if who.all {
        every_org(&rooms).await
    } else {
        who.orgs.clone()
    };
    if who.all && !default.is_empty() {
        orgs.push(default.clone());
    }
    orgs.sort();
    orgs.dedup();
    let default = if orgs.contains(&default) {
        default
    } else {
        orgs.first().cloned().unwrap_or_default()
    };
    axum::Json(
        serde_json::json!({ "signed_in": true, "who": who.who, "auth": sign_in, "default": default, "orgs": orgs }),
    )
}

/// Every org the node keeps documents for.
async fn every_org(rooms: &Rooms) -> Vec<String> {
    rooms
        .store
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, _)| drive_proto::org_of(&id).map(str::to_string))
        .filter(|o| !o.is_empty())
        .collect()
}

/// The org an HTTP request is about: `?space=` / `?org=`, or the first
/// path segment after `/index/` or `/fetch/`.
fn org_of_request(uri: &axum::http::Uri) -> Option<String> {
    let q = uri.query().unwrap_or("");
    for kv in q.split('&') {
        if let Some(v) = kv
            .strip_prefix("space=")
            .or_else(|| kv.strip_prefix("org="))
        {
            return Some(percent_decode(v));
        }
    }
    let mut parts = uri.path().trim_start_matches('/').split('/');
    match parts.next() {
        Some("index") | Some("fetch") => parts.next().map(percent_decode),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every catalog request: signed in, and about an org (or a project
/// shared with one) the person is in.
async fn guard(
    State(rooms): State<Arc<Rooms>>,
    peer: Remote,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let local = auth::is_local(peer_addr(&peer), req.headers());
    let Some(who) = rooms
        .auth
        .principal(auth::bearer(req.headers()).as_deref(), local)
        .await
    else {
        return (StatusCode::UNAUTHORIZED, "sign in").into_response();
    };
    // Writing a blob needs a sign-in only (its name is its hash); reading
    // one needs nothing (share links hand out its URL).
    let blob = req.uri().path().starts_with("/blobs/");
    if blob {
        return next.run(req).await;
    }
    if let Some(org) = org_of_request(req.uri())
        && !rooms.may(&who, &drive_files::space_doc(&org)).await
    {
        tracing::warn!(auth.outcome = "denied", auth.who = %who.who, req.org = %org, "a request about an org the person is not in");
        return (StatusCode::FORBIDDEN, "not one of your orgs").into_response();
    }
    next.run(req).await
}

async fn guard_writes(
    State(rooms): State<Arc<Rooms>>,
    peer: Remote,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD {
        return next.run(req).await;
    }
    guard(State(rooms), peer, req, next).await
}

/// Sidecar requests: a plain read by content key is open (like a blob);
/// one that names a space (asking its device to make it), and every write
/// or purge, goes through [`guard`].
async fn guard_sidecars(
    State(rooms): State<Arc<Rooms>>,
    peer: Remote,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let read = req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD;
    let names_space = org_of_request(req.uri()).is_some();
    let stats = req.uri().path() == "/sidecars/stats";
    if read && !names_space && !stats {
        return next.run(req).await;
    }
    guard(State(rooms), peer, req, next).await
}

/// The node's routes: `/sync` (the websocket), `/health`, and each app's
/// web build under its prefix (`""` for the root).
pub async fn router(data: &Path, apps: Vec<(String, PathBuf)>, opts: Options) -> Router {
    std::fs::create_dir_all(data).expect("the data folder can be made");
    let store = store::Store::open(&data.join("drive.db"))
        .await
        .expect("the document store opens");
    // Documents from before org-scoped ids: back up, then rename them.
    if let Some(org) = &opts.org {
        let old = store
            .list()
            .await
            .map(|l| l.iter().any(|(id, _)| !id.contains('/')))
            .unwrap_or(false);
        if old {
            backup::write(&store, &data.join("backups"), 14, None)
                .await
                .expect("a backup before renaming documents");
            match migrate::to_org_ids(&store, org, &opts.space_org).await {
                Ok(n) => {
                    tracing::info!(migrate.docs = n, migrate.org = %org, "documents now name their org")
                }
                Err(e) => {
                    tracing::error!(error = %e, "renaming documents failed; the backup before it is in data/backups")
                }
            }
        }
    }
    let rooms = Arc::new(Rooms::new(store, auth::Auth::new(opts.issuer.clone())));
    if !opts.no_iroh {
        match transfer::Transfer::start(data, &opts.iroh).await {
            Ok(t) => {
                let _ = rooms.transfer.set(Arc::new(t));
                let r = rooms.clone();
                tokio::spawn(async move {
                    for (id, _) in r.store.list().await.unwrap_or_default() {
                        if let Some(space) = id.strip_suffix("/drive:space") {
                            transfer::publish(&r, space).await;
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "no iroh endpoint: files come from devices the old way")
            }
        }
    }
    tokio::spawn(rooms.clone().flush_loop());
    tokio::spawn(backup::daily(
        rooms.clone(),
        data.join("backups"),
        opts.backup_to.clone(),
    ));
    let default_org = opts.org.clone().unwrap_or_default();
    // The node is a storage too: blobs on its disk, over HTTP.
    let blobs = StorageConfig::Fs {
        root: data.join("blobs").to_string_lossy().into_owned(),
    }
    .blobs()
    .expect("the node's blob folder opens");

    let catalog = catalog::Catalog {
        rooms: rooms.clone(),
        blobs: blobs.clone(),
    };
    let sidecar_store = Arc::new(
        sidecars::Sidecars::open(data, blobs.clone(), data.join("blobs"))
            .await
            .expect("the sidecar store opens"),
    );
    tokio::spawn(sidecars::flush_loop(sidecar_store.clone()));
    if !opts.no_backfill {
        tokio::spawn(sidecars::backfill(
            sidecar_store.clone(),
            rooms.clone(),
            std::time::Duration::from_secs(120),
            50,
        ));
    }
    let browse_routes = browse::Routes {
        rooms: rooms.clone(),
        sc: sidecar_store.clone(),
        seen: Arc::new(
            browse::Seen::open(data)
                .await
                .expect("the browse cache opens"),
        ),
    };
    let sidecar_routes = sidecars::Routes {
        sc: sidecar_store,
        rooms: rooms.clone(),
    };
    let mut app = Router::new()
        .route("/sync", get(sync))
        .route("/health", get(|| async { "ok" }))
        .with_state(rooms.clone())
        .merge(
            Router::new()
                .route("/orgs", get(orgs))
                .with_state((rooms.clone(), default_org)),
        )
        .merge(
            Router::new()
                .route(
                    "/index/{space}/{location}",
                    axum::routing::put(catalog::put_index).delete(catalog::drop_index),
                )
                .route("/search", get(catalog::search))
                .route("/list", get(catalog::list))
                .route("/copies", get(catalog::copies))
                .route("/protection", get(catalog::protection))
                .route("/at-risk", get(catalog::at_risk))
                .route("/stats", get(catalog::stats))
                .route("/shared", get(catalog::shared))
                .route("/fetch/{space}/{location}", get(catalog::fetch))
                .layer(axum::middleware::from_fn_with_state(rooms.clone(), guard))
                .layer(axum::extract::DefaultBodyLimit::max(512 << 20))
                .with_state(catalog.clone()),
        )
        // Every computer's disks, live and read-only (docs/browse.md):
        // members of the device's own org only.
        .merge(
            Router::new()
                .route("/browse/{space}/{device}/roots", get(browse::roots))
                .route("/browse/{space}/{device}/list", get(browse::list))
                .route("/browse/{space}/{device}/file", get(browse::file))
                .route(
                    "/browse/{space}/{device}/sidecar/{kind}",
                    get(browse::sidecar),
                )
                .route(
                    "/browse/{space}/{device}/index",
                    axum::routing::post(browse::index),
                )
                .layer(axum::middleware::from_fn_with_state(
                    rooms.clone(),
                    browse::guard,
                ))
                .with_state(browse_routes),
        )
        // Share links carry their own permission (the token).
        .merge(
            Router::new()
                .route("/s/{space}/{token}", get(share::page))
                .route("/s/{space}/{token}/file", get(share::file))
                .route("/s/{space}/{token}/zip", get(share::zip))
                .route(
                    "/s/{space}/{token}/comments",
                    get(share::comments).post(share::comment),
                )
                // Built sites: public, like share links.
                .route("/sites/{space}/{id}", get(sites::root))
                .route("/sites/{space}/{id}/", get(sites::index))
                .route("/sites/{space}/{id}/{*path}", get(sites::file))
                .with_state(catalog),
        )
        // Sidecars: reading one by its content key needs nothing (as a
        // blob); asking a device to make one, and every write, is a
        // member's.
        .merge(
            Router::new()
                .route(
                    "/sidecar/{key}/{kind}",
                    get(sidecars::get).put(sidecars::put),
                )
                .route("/sidecars/missing", axum::routing::post(sidecars::missing))
                .route("/sidecars/miss", axum::routing::post(sidecars::miss))
                .route("/sidecars/stats", get(sidecars::stats))
                .route("/sidecars/purge", axum::routing::post(sidecars::purge))
                .layer(axum::middleware::from_fn_with_state(
                    rooms.clone(),
                    guard_sidecars,
                ))
                .layer(axum::extract::DefaultBodyLimit::max(64 << 20))
                .with_state(sidecar_routes),
        )
        .merge(
            Router::new()
                .route(
                    "/blobs/{hash}",
                    get(get_blob_ranged).head(head_blob).put(put_blob),
                )
                .route("/pdf/edit/{hash}", axum::routing::post(edit_pdf))
                .layer(axum::middleware::from_fn_with_state(
                    rooms.clone(),
                    guard_writes,
                ))
                .layer(axum::extract::DefaultBodyLimit::max(2 << 30))
                .with_state(blobs),
        );
    // Longest prefixes first, so "/" does not swallow "/canvas".
    let mut apps = apps;
    apps.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    for (prefix, dir) in apps {
        let index = dir.join("index.html");
        let serve = ServeDir::new(&dir).fallback(ServeFile::new(index));
        if prefix.is_empty() {
            app = app.fallback_service(serve);
        } else {
            app = app.nest_service(&prefix, serve);
        }
        tracing::info!(app.prefix = %prefix, app.dir = %dir.display(), "serving app");
    }
    app
}

async fn sync(
    ws: WebSocketUpgrade,
    State(rooms): State<Arc<Rooms>>,
    headers: axum::http::HeaderMap,
    peer: Remote,
) -> impl IntoResponse {
    let local = auth::is_local(peer_addr(&peer), &headers);
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket: WebSocket| rooms::serve(rooms, socket, local))
}

/// [`get_blob`] answering a `Range: bytes=…` request with just those bytes
/// (`206`), so a player can seek in a video or a long recording.
pub(crate) async fn get_blob_ranged(
    path: UrlPath<String>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    blobs: State<BlobStore>,
) -> Result<axum::response::Response, StatusCode> {
    let resp = get_blob(path, query, blobs).await?;
    let Some(range) = headers.get("range").and_then(|v| v.to_str().ok()) else {
        return Ok(with_accept_ranges(resp));
    };
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let len = bytes.len() as u64;
    let Some((start, end)) = byte_range(range, len) else {
        let mut r = axum::response::Response::new(axum::body::Body::empty());
        *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!("bytes */{len}")) {
            r.headers_mut().insert("content-range", v);
        }
        return Ok(r);
    };
    let mut r = axum::response::Response::from_parts(
        parts,
        axum::body::Body::from(bytes.slice(start as usize..=end as usize)),
    );
    *r.status_mut() = StatusCode::PARTIAL_CONTENT;
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
        r.headers_mut().insert("content-range", v);
    }
    Ok(with_accept_ranges(r))
}

fn with_accept_ranges(mut r: axum::response::Response) -> axum::response::Response {
    r.headers_mut().insert(
        "accept-ranges",
        axum::http::HeaderValue::from_static("bytes"),
    );
    r
}

/// The first range of a `Range` header (`bytes=0-99`, `bytes=100-`,
/// `bytes=-500`) over `len` bytes, as inclusive offsets; `None` when it
/// cannot be served.
pub(crate) fn byte_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header
        .trim()
        .strip_prefix("bytes=")?
        .split(',')
        .next()?
        .trim();
    let (a, b) = spec.split_once('-')?;
    if len == 0 {
        return None;
    }
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

/// Page edits to a PDF blob (rotate, delete, move: `drive_view::PageEdit`s
/// as JSON), made here with pdfcraft; the edited file is kept as a new blob
/// and its hash and size are the answer. The file's entry is the app's to
/// point at it, as after an upload.
async fn edit_pdf(
    UrlPath(hash): UrlPath<String>,
    State(blobs): State<BlobStore>,
    axum::Json(edits): axum::Json<Vec<drive_view::PageEdit>>,
) -> axum::response::Response {
    let bytes = match blobs.get(&hash).await {
        Ok(b) => b,
        Err(drive_files::blob::Error::BadHash(_)) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let n = edits.len();
    let edited = tokio::task::spawn_blocking(move || drive_view::edit::apply(bytes, &edits)).await;
    let out = match edited {
        Ok(Ok(out)) => out,
        Ok(Err(why)) => {
            tracing::warn!(pdf.hash = %hash, pdf.edits = n, pdf.why = %why, "a PDF edit was refused");
            return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let size = out.len();
    match blobs.put(out).await {
        Ok(new) => axum::Json(serde_json::json!({ "hash": new, "size": size })).into_response(),
        Err(e) => {
            tracing::warn!(pdf.hash = %hash, error = %e, "an edited PDF could not be stored");
            StatusCode::INSUFFICIENT_STORAGE.into_response()
        }
    }
}

/// A blob's bytes (checked against its hash on the way out); `?name=`
/// makes it a download of that name.
pub(crate) async fn get_blob(
    UrlPath(hash): UrlPath<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    State(blobs): State<BlobStore>,
) -> Result<axum::response::Response, StatusCode> {
    let bytes = match blobs.get(&hash).await {
        Ok(b) => b,
        Err(drive_files::blob::Error::BadHash(_)) => return Err(StatusCode::BAD_REQUEST),
        Err(drive_files::blob::Error::Corrupt(_)) => {
            tracing::warn!(blob.hash = %hash, "a stored blob no longer matches its hash");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Err(_) => return Err(StatusCode::NOT_FOUND),
    };
    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    let h = resp.headers_mut();
    let _ = h.insert(
        "cache-control",
        axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Some(name) = q.get("name") {
        let safe: String = name
            .chars()
            .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
            .collect();
        if let Ok(v) = axum::http::HeaderValue::from_str(drive_files::tree::mime_of(&safe)) {
            h.insert("content-type", v);
        }
        if let Ok(v) =
            axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{safe}\""))
        {
            h.insert("content-disposition", v);
        }
    }
    Ok(resp)
}

async fn head_blob(UrlPath(hash): UrlPath<String>, State(blobs): State<BlobStore>) -> StatusCode {
    match blobs.has(&hash).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Keep a blob; refused unless the bytes match the hash in the path.
async fn put_blob(
    UrlPath(hash): UrlPath<String>,
    State(blobs): State<BlobStore>,
    body: Bytes,
) -> StatusCode {
    match blobs.put_checked(&hash, body.to_vec()).await {
        Ok(()) => StatusCode::CREATED,
        Err(drive_files::blob::Error::Corrupt(_)) | Err(drive_files::blob::Error::BadHash(_)) => {
            tracing::warn!(blob.hash = %hash, "a blob upload did not match its hash");
            StatusCode::BAD_REQUEST
        }
        Err(e) => {
            tracing::warn!(blob.hash = %hash, error = %e, "a blob could not be stored");
            StatusCode::INSUFFICIENT_STORAGE
        }
    }
}

#[cfg(test)]
mod range_tests {
    use super::byte_range;

    #[test]
    fn byte_ranges() {
        assert_eq!(byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(byte_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(byte_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(byte_range("bytes=0-1,5-9", 1000), Some((0, 1)));
        assert_eq!(byte_range("bytes=1000-", 1000), None);
        assert_eq!(byte_range("bytes=5-1", 1000), None);
        assert_eq!(byte_range("items=0-1", 1000), None);
        assert_eq!(byte_range("bytes=0-0", 0), None);
    }
}
