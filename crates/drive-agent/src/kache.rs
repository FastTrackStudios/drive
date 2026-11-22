//! kache build caches shared with this device's build group (docs/kache.md):
//! the daemon keeps a `drive_kache::Kache` in `<agent>/kache`, answers the
//! group's lookups on its iroh endpoint, and, while this device has a
//! build group in the registry, serves kache's S3 remote on
//! `127.0.0.1:<port>`.

use std::sync::Arc;
use std::time::Duration;

use drive_files::space;
use drive_kache::{ForgetFn, Kache, Link, Members, Peer, ProvideFn, Store, Timeouts};
use drive_transfer::Fetcher;
use futures::FutureExt;
use serde::{Deserialize, Serialize};

use crate::agent::Agent;

/// This device's kache settings (in `config.json`; never synced).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// The loopback port kache's remote is served on (default 9778).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// How much of the group's cache is kept here, in GiB (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_gib: Option<u64>,
    /// How long a miss waits for the group, in ms (default 600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup_ms: Option<u64>,
}

impl Settings {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(drive_kache::DEFAULT_PORT)
    }

    pub fn budget(&self) -> u64 {
        self.budget_gib
            .map_or(drive_kache::DEFAULT_BUDGET, |g| g << 30)
    }
}

/// The group this computer's builds belong to by default: its OS and CPU
/// (`linux-x86_64`, `darwin-arm64`, …).
pub fn default_group() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    };
    format!("{os}-{arch}")
}

/// The other devices of `me`'s build group that serve over iroh.
pub fn group_peers(doc: &loro::LoroDoc, me: &str) -> Vec<Peer> {
    let groups = space::build_groups(doc);
    let Some(mine) = groups.get(me) else {
        return Vec::new();
    };
    let names: std::collections::HashMap<String, String> = space::devices(doc)
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect();
    let mut out: Vec<Peer> = space::endpoints(doc)
        .into_iter()
        .filter(|(dev, _)| dev != me && groups.get(dev) == Some(mine))
        .filter_map(|(dev, (id, addr))| {
            let id: drive_transfer::EndpointId = id.parse().ok()?;
            let addr = drive_transfer::addr_from_str(&addr)
                .unwrap_or_else(|_| drive_transfer::EndpointAddr::new(id));
            (addr.id == id).then(|| Peer {
                id,
                addr,
                name: names.get(&dev).cloned().unwrap_or(dev),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// This device's build group now ("" when none).
pub fn my_group(a: &Agent) -> String {
    space::build_groups(&a.space)
        .remove(&a.device_id())
        .unwrap_or_default()
}

/// Open the cache (before the endpoint starts: its lookups are one of the
/// endpoint's protocols).
pub fn open(a: &Arc<Agent>) -> Result<Arc<Kache>, String> {
    let settings = a.cfg.lock().expect("cfg").kache.clone();
    let store = Store::open(&a.dir.join("kache"), settings.budget())
        .map_err(|e| format!("the kache store: {e}"))?;
    let weak = Arc::downgrade(a);
    let members: Members = Arc::new(move || {
        weak.upgrade()
            .map(|a| group_peers(&a.space, &a.device_id()))
            .unwrap_or_default()
    });
    let mut timeouts = Timeouts::default();
    if let Some(ms) = settings.lookup_ms {
        timeouts.lookup = Duration::from_millis(ms);
    }
    Ok(Kache::new(store, members, timeouts))
}

/// Once the endpoint is up: reach the group through it, keep connections
/// warm, and serve kache's remote while this device has a build group.
pub fn run(a: Arc<Agent>, k: Arc<Kache>) {
    let Some(p) = a.provider.get() else {
        return;
    };
    let weak = Arc::downgrade(&a);
    let provide: ProvideFn = Arc::new(move |path| {
        let a = weak.upgrade();
        async move {
            let p = a
                .as_ref()
                .and_then(|a| a.provider.get())
                .ok_or("not serving")?;
            p.provide(&path).await.map(|got| got.hash)
        }
        .boxed()
    });
    let weak = Arc::downgrade(&a);
    let forget: ForgetFn = Arc::new(move |path| {
        let a = weak.upgrade();
        async move {
            if let Some(p) = a.as_ref().and_then(|a| a.provider.get()) {
                p.forget(&path).await;
            }
        }
        .boxed()
    });
    k.attach(Link {
        endpoint: p.endpoint().clone(),
        fetcher: Arc::new(Fetcher::on(p.endpoint().clone())),
        provide,
        forget,
    });
    tokio::spawn(k.clone().keep_warm());
    tokio::spawn(async move {
        let mut serving: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let grouped = !my_group(&a).is_empty();
            match (&serving, grouped) {
                (None, true) => {
                    let port = a.cfg.lock().expect("cfg").kache.port();
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                    match drive_kache::s3::serve(k.clone(), addr).await {
                        Ok((bound, task)) => {
                            tracing::info!(
                                kache.endpoint = %bound,
                                kache.group = %my_group(&a),
                                kache.peers = k.members().len(),
                                "serving kache's remote cache"
                            );
                            serving = Some(task);
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            kache.port = port,
                            "kache's remote cache cannot be served; tried again in a minute"
                        ),
                    }
                }
                (Some(task), false) => {
                    task.abort();
                    serving = None;
                    tracing::info!("no build group: kache's remote cache is not served");
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

/// The `[cache.remote]` block that points kache at this device's agent.
pub fn remote_block(port: u16) -> String {
    format!(
        "[cache.remote]\ntype = \"s3\"\nbucket = \"{}\"\nendpoint = \"http://127.0.0.1:{port}\"\nregion = \"us-east-1\"",
        drive_kache::BUCKET
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_are_the_group_less_this_device() {
        let d = loro::LoroDoc::new();
        let ids: Vec<String> = (0..4)
            .map(|_| iroh::SecretKey::generate().public().to_string())
            .collect();
        for (dev, id, group) in [
            ("pc", &ids[0], "linux-x86_64"),
            ("vps", &ids[1], "linux-x86_64"),
            ("mac", &ids[2], "darwin-arm64"),
            ("nas", &ids[3], ""),
        ] {
            space::set_endpoint(&d, dev, id, "").unwrap();
            space::set_build_group(&d, dev, group).unwrap();
        }
        let peers = group_peers(&d, "pc");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id.to_string(), ids[1]);
        assert!(group_peers(&d, "nas").is_empty());
        assert!(group_peers(&d, "mac").is_empty());
    }

    #[test]
    fn the_default_group_names_os_and_cpu() {
        let g = default_group();
        assert!(g.contains('-') && !g.contains("macos") && !g.contains("aarch64"));
    }
}
