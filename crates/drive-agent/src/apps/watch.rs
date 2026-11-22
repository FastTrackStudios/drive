//! Watching a file for new versions, so an engine can reload: this
//! device's own file by its size and time (checked every second), a Drive
//! tree file by its content hash in the tree (every second), another
//! computer's file by asking that computer (every few seconds, which
//! hashes it there only when its size or time changed).

use std::sync::Arc;
use std::time::Duration;

use drive_local::proto::Event;

use super::resolve::Source;
use super::{Apps, ConnCtx};

const LOCAL_EVERY: Duration = Duration::from_secs(1);
const REMOTE_EVERY: Duration = Duration::from_secs(5);

/// What a version of the file is, as far as can be told cheaply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Version {
    Stat {
        size: u64,
        mtime: i64,
    },
    Hash {
        size: u64,
        hash: String,
    },
    Gone,
    /// Not reachable now (its computer is off): no news.
    Unknown,
}

fn stat(path: &std::path::Path) -> Version {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => Version::Stat {
            size: m.len(),
            mtime: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64),
        },
        _ => Version::Gone,
    }
}

/// The version of `target` now.
pub async fn version(apps: &Apps, target: &str) -> Version {
    let src = match apps.resolve(target).await {
        Ok(s) => s,
        Err(_) => return Version::Gone,
    };
    match src {
        Source::Local { path, .. } => stat(&path),
        Source::Tree { hash, size } => Version::Hash { size, hash },
        Source::Content { hash } => Version::Hash { size: 0, hash },
        Source::Remote { device, ask, .. } => match apps.ask(&device, &ask).await {
            Ok((_, a)) => match a.hash {
                Some(hash) => Version::Hash { size: a.size, hash },
                None => Version::Gone,
            },
            Err(_) => Version::Unknown,
        },
    }
}

pub(crate) async fn start(c: Arc<ConnCtx>, id: u64, target: String) {
    let apps = c.apps.clone();
    let src = match apps.resolve(&target).await {
        Ok(s) => s,
        Err(e) => return c.answer(id, Err(e)),
    };
    let every = match src {
        Source::Remote { .. } => REMOTE_EVERY,
        _ => LOCAL_EVERY,
    };
    let mut last = version(&apps, &target).await;
    c.answer(id, Ok(serde_json::json!({ "watch": id })));
    let c2 = c.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let now = version(&c2.apps, &target).await;
            if now == Version::Unknown || now == last {
                continue;
            }
            match &now {
                Version::Gone => c2.event(Event::Gone {
                    watch: id,
                    target: target.clone(),
                }),
                Version::Stat { size, .. } => c2.event(Event::Changed {
                    watch: id,
                    target: target.clone(),
                    size: *size,
                    hash: None,
                }),
                Version::Hash { size, hash } => c2.event(Event::Changed {
                    watch: id,
                    target: target.clone(),
                    size: *size,
                    hash: Some(hash.clone()),
                }),
                Version::Unknown => {}
            }
            last = now;
        }
    });
    c.tasks
        .lock()
        .expect("tasks")
        .insert(id, task.abort_handle());
}
