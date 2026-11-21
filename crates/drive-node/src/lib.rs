//! `drive-node`: an always-on Drive peer. It keeps every document it is
//! given (in Turso), passes live edits and presence between the peers that
//! have a document open, keeps blobs, and serves apps' web builds.

pub mod catalog;
pub mod rooms;
pub mod share;
pub mod store;

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

/// The node's routes: `/sync` (the websocket), `/health`, and each app's
/// web build under its prefix (`""` for the root).
pub async fn router(data: &Path, apps: Vec<(String, PathBuf)>) -> Router {
    std::fs::create_dir_all(data).expect("the data folder can be made");
    let store = store::Store::open(&data.join("drive.db"))
        .await
        .expect("the document store opens");
    let rooms = Arc::new(Rooms::new(store));
    tokio::spawn(rooms.clone().flush_loop());
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
    let mut app = Router::new()
        .route("/sync", get(sync))
        .route("/health", get(|| async { "ok" }))
        .with_state(rooms)
        .merge(
            Router::new()
                .route(
                    "/index/{space}/{location}",
                    axum::routing::put(catalog::put_index),
                )
                .route("/search", get(catalog::search))
                .route("/list", get(catalog::list))
                .route("/copies", get(catalog::copies))
                .route("/protection", get(catalog::protection))
                .route("/at-risk", get(catalog::at_risk))
                .route("/stats", get(catalog::stats))
                .route("/fetch/{space}/{location}", get(catalog::fetch))
                .route("/s/{space}/{token}", get(share::page))
                .route("/s/{space}/{token}/file", get(share::file))
                .route("/s/{space}/{token}/zip", get(share::zip))
                .layer(axum::extract::DefaultBodyLimit::max(512 << 20))
                .with_state(catalog),
        )
        .merge(
            Router::new()
                .route("/blobs/{hash}", get(get_blob).head(head_blob).put(put_blob))
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

async fn sync(ws: WebSocketUpgrade, State(rooms): State<Arc<Rooms>>) -> impl IntoResponse {
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket: WebSocket| rooms::serve(rooms, socket))
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
