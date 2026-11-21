//! `drive-node`: an always-on Drive peer. It keeps every document it is
//! given (in Turso), passes live edits and presence between the peers that
//! have a document open, and serves apps' web builds from one address.
//!
//! ```text
//! drive-node --listen 127.0.0.1:8096 --data ./data \
//!     --app /=apps/drive/dist --app /canvas=../canvas/dist
//! ```

mod rooms;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::services::{ServeDir, ServeFile};

use rooms::Rooms;

struct Args {
    listen: SocketAddr,
    data: PathBuf,
    /// (path prefix, directory with the built web app)
    apps: Vec<(String, PathBuf)>,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        listen: "127.0.0.1:8096".parse().expect("a socket address"),
        data: PathBuf::from("data"),
        apps: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => a.listen = value.parse().map_err(|e| format!("--listen: {e}"))?,
            "--data" => a.data = value.into(),
            "--app" => {
                let (prefix, dir) = value
                    .split_once('=')
                    .ok_or("--app takes PREFIX=DIR".to_string())?;
                a.apps.push((prefix.trim_end_matches('/').to_string(), dir.into()));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "drive_node=info,tower_http=warn".into()),
        )
        .init();
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "bad arguments");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&args.data).expect("the data folder can be made");
    let store = store::Store::open(&args.data.join("drive.db"))
        .await
        .expect("the document store opens");
    let rooms = Arc::new(Rooms::new(store));
    tokio::spawn(rooms.clone().flush_loop());

    let mut app = Router::new()
        .route("/sync", get(sync))
        .route("/health", get(|| async { "ok" }))
        .with_state(rooms);
    // Longest prefixes first, so "/" does not swallow "/canvas".
    let mut apps = args.apps;
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

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .expect("the address is free");
    tracing::info!(node.listen = %args.listen, "drive-node listening");
    axum::serve(listener, app).await.expect("the server runs");
}

async fn sync(ws: WebSocketUpgrade, State(rooms): State<Arc<Rooms>>) -> impl IntoResponse {
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket: WebSocket| rooms::serve(rooms, socket))
}
