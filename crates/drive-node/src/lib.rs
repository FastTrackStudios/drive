//! `drive-node`: an always-on Drive peer. It keeps every document it is
//! given (in Turso), passes live edits and presence between the peers that
//! have a document open, keeps blobs, and serves apps' web builds.

pub mod rooms;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use rooms::Rooms;

/// The node's routes: `/sync` (the websocket), `/health`, and each app's
/// web build under its prefix (`""` for the root).
pub async fn router(data: &Path, apps: Vec<(String, PathBuf)>) -> Router {
    std::fs::create_dir_all(data).expect("the data folder can be made");
    let store = store::Store::open(&data.join("drive.db")).await.expect("the document store opens");
    let rooms = Arc::new(Rooms::new(store));
    tokio::spawn(rooms.clone().flush_loop());

    let mut app = Router::new()
        .route("/sync", get(sync))
        .route("/health", get(|| async { "ok" }))
        .with_state(rooms);
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
