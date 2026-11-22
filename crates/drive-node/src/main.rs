//! `drive-node`: run a Drive node.
//!
//! ```text
//! drive-node --listen 127.0.0.1:8096 --data ./data \
//!     --app /=apps/drive/dist --app /frame=../frame-app/dist \
//!     --org codywright [--issuer https://auth.fasttrackstudio.app] [--backup-to r2.json] [--no-backfill]
//! drive-node backup [--data ./data]           # one backup now (node stopped)
//! drive-node restore FILE [--data ./data]     # merge a backup back in
//! drive-node rename-app FROM TO --org ORG     # an app's documents and spine entries follow a new name
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

struct Args {
    listen: SocketAddr,
    data: PathBuf,
    opts: drive_node::Options,
    /// (path prefix, directory with the built web app)
    apps: Vec<(String, PathBuf)>,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        listen: "127.0.0.1:8096".parse().expect("a socket address"),
        data: PathBuf::from("data"),
        opts: drive_node::Options::default(),
        apps: Vec::new(),
    };
    let mut it = std::env::args()
        .skip(1)
        .filter(|a| a != "backup" && a != "restore" && a != "rename-app")
        .skip_while(|a| !a.starts_with("--"));
    while let Some(flag) = it.next() {
        if flag == "--no-backfill" {
            a.opts.no_backfill = true;
            continue;
        }
        if flag == "--no-iroh" {
            a.opts.no_iroh = true;
            continue;
        }
        if flag == "--iroh-local" {
            a.opts.iroh.local_only = true;
            continue;
        }
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => a.listen = value.parse().map_err(|e| format!("--listen: {e}"))?,
            "--data" => a.data = value.into(),
            "--org" => a.opts.org = Some(value),
            "--issuer" => a.opts.issuer = Some(value),
            "--relay" => a.opts.iroh.relay = Some(value),
            "--space-org" => {
                let (space, org) = value
                    .split_once('=')
                    .ok_or("--space-org takes SPACE=ORG".to_string())?;
                a.opts.space_org.push((space.to_string(), org.to_string()));
            }
            "--backup-to" => {
                let json = std::fs::read_to_string(&value)
                    .map_err(|e| format!("--backup-to {value}: {e}"))?;
                a.opts.backup_to = Some(
                    serde_json::from_str(&json).map_err(|e| format!("--backup-to {value}: {e}"))?,
                );
            }
            "--app" => {
                let (prefix, dir) = value
                    .split_once('=')
                    .ok_or("--app takes PREFIX=DIR".to_string())?;
                a.apps
                    .push((prefix.trim_end_matches('/').to_string(), dir.into()));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    let _telemetry = drive_node::telemetry::init("drive-node", "drive_node=info,tower_http=warn");
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "bad arguments");
            std::process::exit(2);
        }
    };
    // `drive-node backup [--data D]`, `drive-node restore FILE [--data D]`:
    // one-off, with the node stopped.
    let words: Vec<String> = std::env::args()
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .collect();
    match words.first().map(String::as_str) {
        Some("backup") => {
            let store = drive_node::store::Store::open(&args.data.join("drive.db"))
                .await
                .expect("the document store opens");
            match drive_node::backup::write(
                &store,
                &args.data.join("backups"),
                14,
                args.opts.backup_to.as_ref(),
            )
            .await
            {
                Ok(p) => tracing::info!(backup.path = %p.display(), "backup written"),
                Err(e) => {
                    tracing::error!(error = %e, "the backup failed");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("restore") => {
            let Some(file) = words.get(1) else {
                tracing::error!("restore needs a backup file");
                std::process::exit(2);
            };
            let store = drive_node::store::Store::open(&args.data.join("drive.db"))
                .await
                .expect("the document store opens");
            let bytes = std::fs::read(file).expect("the backup file reads");
            match drive_node::backup::restore(&store, &bytes).await {
                Ok(n) => tracing::info!(restore.docs = n, "documents merged back from the backup"),
                Err(e) => {
                    tracing::error!(error = %e, "the restore failed");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("rename-app") => {
            let (Some(from), Some(to), Some(org)) =
                (words.get(1), words.get(2), args.opts.org.as_ref())
            else {
                tracing::error!("rename-app FROM TO --org ORG");
                std::process::exit(2);
            };
            let store = drive_node::store::Store::open(&args.data.join("drive.db"))
                .await
                .expect("the document store opens");
            drive_node::backup::write(&store, &args.data.join("backups"), 14, None)
                .await
                .expect("a backup before renaming");
            match drive_node::migrate::rename_app(&store, org, from, to).await {
                Ok(n) => {
                    tracing::info!(migrate.docs = n, "the app's documents follow its new name")
                }
                Err(e) => {
                    tracing::error!(error = %e, "renaming the app failed; the backup before it is in data/backups");
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }
    let app = drive_node::router(&args.data, args.apps, args.opts).await;
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .expect("the address is free");
    tracing::info!(node.listen = %args.listen, "drive-node listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("the server runs");
}
