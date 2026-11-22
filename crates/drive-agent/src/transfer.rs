//! This device's files over iroh (docs/transfer.md): the endpoint the
//! daemon runs, published in the space registry next to the device's
//! entry, letting in only the space's own node and devices, and serving
//! read-only, in place, the files the node asks for: of this device's
//! locations, and of its browsed volumes, under the same confinement as
//! browsing (`browse::Rules`: inside the root, no escapes, never the
//! agent's own folder).

use std::path::Path;
use std::sync::Arc;

use drive_files::space;
use drive_proto::Root;
use drive_transfer::{Allow, Provider, Settings};

use crate::agent::Agent;
use crate::browse::Rules;

/// How this device's endpoint reaches others: `DRIVE_RELAY` (or the
/// config's `relay`) for our own relay, `DRIVE_IROH_LOCAL=1` for direct
/// addresses only.
pub fn settings(relay: Option<String>) -> Settings {
    Settings {
        relay: std::env::var("DRIVE_RELAY").ok().or(relay),
        local_only: std::env::var("DRIVE_IROH_LOCAL").is_ok_and(|v| v == "1"),
    }
}

impl Agent {
    /// Start serving files over iroh (and `extra` protocols beside them:
    /// the kache lookups) and say so in the registry.
    pub async fn start_transfer(&self, extra: Vec<drive_transfer::Extra>) -> Result<(), String> {
        let relay = self.cfg.lock().expect("cfg").relay.clone();
        self.start_transfer_with(settings(relay), extra).await
    }

    /// [`Agent::start_transfer`] with these endpoint settings.
    pub async fn start_transfer_with(
        &self,
        settings: Settings,
        extra: Vec<drive_transfer::Extra>,
    ) -> Result<(), String> {
        let doc = self.space.clone();
        let allow: Allow =
            Arc::new(move |id| space::allowed_endpoints(&doc).contains(&id.to_string()));
        let p = Provider::start_with(&self.dir.join("transfer"), &settings, allow, extra).await?;
        p.online(std::time::Duration::from_secs(5)).await;
        tracing::info!(transfer.endpoint = %p.id().fmt_short(), "serving files over iroh");
        let _ = self.provider.set(p);
        self.publish_endpoint();
        Ok(())
    }

    /// Put this device's endpoint (and its address now) in the registry;
    /// written only when it changed.
    pub fn publish_endpoint(&self) {
        let Some(p) = self.provider.get() else {
            return;
        };
        let me = self.device_id();
        let id = p.id().to_string();
        let addr = drive_transfer::addr_to_string(&p.addr());
        let known = space::endpoints(&self.space).get(&me).cloned();
        if known.as_ref() != Some(&(id.clone(), addr.clone())) {
            let _ = space::set_endpoint(&self.space, &me, &id, &addr);
            self.commit_space();
        }
    }

    fn provider(&self) -> Result<&Provider, String> {
        self.provider
            .get()
            .ok_or_else(|| "this device does not serve files over iroh yet".to_string())
    }

    /// Serve a confined, resolved file.
    async fn provide_file(&self, file: &Path) -> Result<drive_proto::Provided, String> {
        let p = self.provider()?;
        let got = p.provide(file).await?;
        Ok(drive_proto::Provided {
            hash: got.hash,
            size: got.size,
            mtime: got.mtime,
            endpoint: drive_transfer::addr_to_string(&p.addr()),
        })
    }

    /// A file of a browsed volume (`Browse::Provide`).
    pub async fn provide_browsed(
        &self,
        rules: &Rules,
        root: &str,
        path: &str,
    ) -> Result<drive_proto::Provided, String> {
        let file = rules.resolve(root, path)?;
        self.provide_file(&file).await
    }

    /// A file of one of this device's locations (`Browse::ProvideLocation`):
    /// confined to the location's folder as a browsed file is to its root.
    pub async fn provide_location(
        &self,
        location: &str,
        path: &str,
    ) -> Result<drive_proto::Provided, String> {
        let loc = self
            .my_locations()
            .into_iter()
            .find(|l| l.id == location)
            .ok_or("not one of this device's locations")?;
        if !loc.storage.is_empty() {
            return Err("in a storage, not on a disk here".into());
        }
        let rules = location_rules(&loc.path, &self.dir);
        let file = rules.resolve(&loc.path, path)?;
        self.provide_file(&file).await
    }
}

/// A location's folder as the one root requests may reach.
pub fn location_rules(folder: &str, agent_dir: &Path) -> Rules {
    Rules::new(
        &[Root {
            mount: folder.to_string(),
            name: String::new(),
            fs: String::new(),
            total: 0,
            free: 0,
            removable: false,
            home: false,
        }],
        agent_dir,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_files_stay_inside_their_folder() {
        let t = tempfile::tempdir().unwrap();
        let loc = t.path().join("Samples");
        std::fs::create_dir_all(loc.join("Kits")).unwrap();
        std::fs::write(loc.join("Kits/kick.wav"), b"k").unwrap();
        std::fs::write(t.path().join("secret.txt"), b"s").unwrap();
        let agent = loc.join(".drive-agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("config.json"), b"{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("secret.txt"), loc.join("out.txt")).unwrap();
        let folder = loc.to_string_lossy().into_owned();
        let r = location_rules(&folder, &agent);
        assert!(r.resolve(&folder, "Kits/kick.wav").is_ok());
        assert!(r.resolve(&folder, "../secret.txt").is_err());
        // An absolute path is read inside the folder, never from `/`.
        assert!(r.resolve(&folder, "/etc/passwd").is_err());
        #[cfg(unix)]
        assert!(r.resolve(&folder, "out.txt").is_err());
        assert_eq!(
            r.resolve(&folder, ".drive-agent/config.json").unwrap_err(),
            "not shown"
        );
        // Another folder is not this location.
        assert!(r.resolve("/", "etc/passwd").is_err());
    }
}
