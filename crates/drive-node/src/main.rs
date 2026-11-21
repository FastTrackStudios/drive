//! `drive-node`: run a Drive node.
//!
//! ```text
//! drive-node --listen 127.0.0.1:8096 --data ./data \
//!     --app /=apps/drive/dist --app /frame=../frame-app/dist
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

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
                let (prefix, dir) = value.split_once('=').ok_or("--app takes PREFIX=DIR".to_string())?;
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
    let app = drive_node::router(&args.data, args.apps).await;
    let listener = tokio::net::TcpListener::bind(args.listen).await.expect("the address is free");
    tracing::info!(node.listen = %args.listen, "drive-node listening");
    axum::serve(listener, app).await.expect("the server runs");
}
