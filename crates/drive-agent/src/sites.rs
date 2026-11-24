//! Building and publishing the space's static sites.
//!
//! The agent builds, not the node: it has the files (its cache, its
//! storages), it holds the keys of the public storage a site goes to (the
//! R2 bucket's credentials never leave the device that runs it), and `ddc`
//! is installed for its user. The node only serves what an agent built
//! (`/sites/<space>/<id>/`, from blobs).
//!
//! A site is built by its `builder` device (the first agent to see it
//! claims it). `drive site build ID` builds it now; the agent's
//! service rebuilds it a few seconds after its pages or theme change, and
//! when someone asks for a build in the Drive app.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use drive_files::sites::{self, Build, Site};
use drive_files::{space, tree};
use drive_sites::rewrite::{self, Placement};
use drive_sites::source::{Blobs, Source, TreeFolder};
use drive_sites::{Inputs, ddc};
use loro::{LoroDoc, TreeID};

use crate::agent::Agent;

/// Where the starter theme goes in a space.
pub const DEFAULT_THEME: &str = "Sites/Themes/Base";

/// `ddc --version`, asked once.
static DDC_VERSION: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

impl Blobs for Agent {
    async fn get(&self, hash: &str) -> Result<Vec<u8>, String> {
        if self.is_cached(hash) {
            return tokio::fs::read(self.cache_path(hash)?)
                .await
                .map_err(|e| e.to_string());
        }
        self.fetch_bytes(hash).await
    }
}

/// What a build did.
pub enum Outcome {
    /// Nothing changed since the last build.
    Unchanged,
    Built(Build),
}

impl Agent {
    pub fn space_name(&self) -> String {
        self.cfg.lock().expect("cfg").space.clone()
    }

    /// A site's document, with the node's copy (or this device's, offline).
    pub async fn site_doc(&self, id: &str) -> (String, LoroDoc) {
        let doc_id = sites::site_doc(&self.space_name(), id);
        let doc = self.drive.open(&doc_id).await;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            self.drive.synced(&doc_id),
        )
        .await;
        (doc_id, doc)
    }

    /// A folder of the tree, made if it is not there (and its parents).
    pub fn ensure_folder(&self, path: &str) -> Result<Option<TreeID>, String> {
        let mut cur: Option<TreeID> = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = Some(match tree::lookup(&self.files, cur, part) {
                Some(e) if e.dir => e.id,
                Some(_) => return Err(format!("{part} is a file, not a folder")),
                None => tree::mkdir(&self.files, cur, part).map_err(|e| e.to_string())?,
            });
        }
        Ok(cur)
    }

    /// Put the starter theme into the space (files already there are kept).
    /// Returns how many files it added.
    pub async fn theme_init(&self, folder: &str) -> Result<usize, String> {
        let tmp = self.dir.join("tmp-theme");
        let mut added = 0;
        for (path, text) in drive_sites::theme::STARTER {
            let full = format!("{}/{path}", folder.trim_matches('/'));
            if tree::resolve(&self.files, &full).flatten().is_some() {
                continue;
            }
            let (dir, name) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
            let parent = self.ensure_folder(dir)?;
            let local = tmp.join(path);
            if let Some(p) = local.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::write(&local, text).map_err(|e| e.to_string())?;
            self.put(&local, parent, name).await?;
            added += 1;
        }
        let _ = std::fs::remove_dir_all(&tmp);
        self.commit_files();
        Ok(added)
    }

    /// Where a site is published, and the storage this device runs for it.
    fn public_target(&self, site: &Site) -> Result<Option<(String, opendal::Operator)>, String> {
        if site.storage.is_empty() {
            return Ok(None);
        }
        let urls = space::public(&self.space);
        let url = urls.get(&site.storage).cloned().unwrap_or_default();
        let cfg = self.cfg.lock().expect("cfg").clone();
        let own = cfg
            .storages
            .iter()
            .find(|s| s.id == site.storage || s.name == site.storage)
            .ok_or_else(|| "its storage is run by another device; build it there".to_string())?;
        let base = if !site.base_url.is_empty() {
            site.base_url.clone()
        } else if !url.is_empty() {
            format!("{}/sites/{}/", url.trim_end_matches('/'), site.id)
        } else {
            return Err(format!(
                "{} is not public (drive storage public {} URL)",
                own.name, own.name
            ));
        };
        let op = own.config.operator().map_err(|e| e.to_string())?;
        Ok(Some((base, op)))
    }

    /// Upload a file to the node unless it is there.
    async fn node_put(&self, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let url = format!("{}/blobs/{hash}", self.cfg.lock().expect("cfg").node_http());
        if let Ok(r) = self.http.head(&url).send().await
            && r.status().is_success()
        {
            return Ok(());
        }
        let r = self
            .http
            .put(&url)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("the node refused a site file ({})", r.status()));
        }
        Ok(())
    }

    /// Build a site, and publish it to the node and its public storage.
    /// Unchanged inputs are not built again unless `force` (or a build
    /// was asked for since the last one).
    pub async fn site_build(&self, id: &str, force: bool) -> Result<Outcome, String> {
        let space_name = self.space_name();
        let me = self.device_id();
        let (doc_id, doc) = self.site_doc(id).await;
        let mut site = sites::read(&doc).ok_or(format!("no site {id}"))?;
        if site.builder.is_empty() {
            site.builder = me.clone();
            sites::write(&doc, &site).map_err(|e| e.to_string())?;
            self.drive.commit(&doc_id);
        }
        let last = sites::last_build(&doc);
        let started = std::time::Instant::now();

        let source = TreeFolder::new(&self.files, &site.source);
        if !source.exists() {
            return Err(format!("no folder {}", site.source));
        }
        let inputs = Inputs {
            source: source.files(),
            theme: TreeFolder::new(&self.files, &site.theme).files(),
            overrides: if site.overrides.is_empty() {
                Vec::new()
            } else {
                TreeFolder::new(&self.files, &site.overrides).files()
            },
        };
        let bin = ddc::locate().ok_or(
            "ddc (dodeca) is not installed for this user: see https://github.com/bearcove/dodeca/releases",
        )?;
        let version = DDC_VERSION.get_or_init(|| ddc::version(&bin)).await.clone();
        let public = self.public_target(&site);
        let base_url = match &public {
            Ok(Some((base, _))) => base.clone(),
            _ => sites::node_path(&space_name, id),
        };
        let input_hash = drive_sites::input_hash(&site, &base_url, &inputs, &version);
        let asked = last.as_ref().is_none_or(|l| site.requested > l.at);
        if !force
            && !asked
            && last
                .as_ref()
                .is_some_and(|l| l.ok && l.input_hash == input_hash)
        {
            return Ok(Outcome::Unchanged);
        }

        let work = work_dir(&self.dir, id)?;
        let result = drive_sites::build(&work, &site, "/", &inputs, self, &bin).await;
        let mut record = Build {
            at: drive_files::now_ms(),
            input_hash: input_hash.clone(),
            by: me.clone(),
            ..Build::default()
        };
        match result {
            Err(e) => {
                record.ok = false;
                record.message = e;
            }
            Ok(built) => {
                record.pages = built.pages;
                record.files = built.files.len() as i64;
                record.broken_links = built.broken_links.clone();
                record.unresolved = built.unresolved.clone();
                match self
                    .site_publish(&space_name, id, &built.files, public)
                    .await
                {
                    Ok((published, warning)) => {
                        record.ok = true;
                        record.published = published;
                        record.message = warning.unwrap_or_else(|| {
                            format!(
                                "{} pages, {} broken links, {} unresolved wikilinks",
                                built.pages,
                                built.broken_links.len(),
                                built.unresolved.len()
                            )
                        });
                    }
                    Err(e) => {
                        record.ok = false;
                        record.message = e;
                    }
                }
            }
        }
        record.duration_ms = started.elapsed().as_millis() as i64;
        sites::set_build(&doc, &record).map_err(|e| e.to_string())?;
        self.drive.commit(&doc_id);
        if record.ok {
            Ok(Outcome::Built(record))
        } else {
            Err(record.message)
        }
    }

    /// Put a built site on the node (and its public storage). Returns where
    /// it went, and a warning if the public copy could not be made.
    async fn site_publish(
        &self,
        space_name: &str,
        id: &str,
        files: &BTreeMap<String, Vec<u8>>,
        public: Result<Option<(String, opendal::Operator)>, String>,
    ) -> Result<(Vec<String>, Option<String>), String> {
        let out_id = sites::output_doc(space_name, id);
        let out = self.drive.open(&out_id).await;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            self.drive.synced(&out_id),
        )
        .await;

        // The node: under /sites/<space>/<id>/.
        let node_path = sites::node_path(space_name, id);
        let placed = rewrite::site(&Placement::from_base_url(&node_path, false), files);
        let before: HashSet<String> = sites::output(&out).into_values().collect();
        let mut manifest = BTreeMap::new();
        for (path, bytes) in &placed {
            let h = drive_sites::hash(bytes);
            if !before.contains(&h) {
                self.node_put(&h, bytes).await?;
            }
            manifest.insert(path.clone(), h);
        }
        sites::set_output(&out, &manifest).map_err(|e| e.to_string())?;
        self.drive.commit(&out_id);
        let mut published = vec![node_path];

        // Its public storage, if it has one.
        let warning = match public {
            Ok(None) => None,
            Err(e) => Some(format!("built for the node only: {e}")),
            Ok(Some((base, op))) => {
                let site = Placement::from_base_url(&base, true);
                let placed = rewrite::site(&site, files);
                let storage = sites::read(&self.site_doc(id).await.1)
                    .map(|s| s.storage)
                    .unwrap_or_default();
                let before = sites::published(&out, &storage);
                match drive_sites::publish_storage(&op, &format!("sites/{id}"), &placed, &before)
                    .await
                {
                    Ok(now) => {
                        sites::set_published(&out, &storage, &now).map_err(|e| e.to_string())?;
                        self.drive.commit(&out_id);
                        published.push(base);
                        None
                    }
                    Err(e) => Some(format!("the public copy failed: {e}")),
                }
            }
        };
        Ok((published, warning))
    }

    /// Build every site this device builds whose inputs changed, or that
    /// someone asked to be built (sites no device has claimed yet are
    /// claimed). Returns the ids built.
    pub async fn build_due_sites(&self) -> Vec<String> {
        let me = self.device_id();
        let mut built = Vec::new();
        for id in sites::listed(&self.space).into_keys() {
            let (_, doc) = self.site_doc(&id).await;
            let Some(site) = sites::read(&doc) else {
                continue;
            };
            if !site.builder.is_empty() && site.builder != me {
                continue;
            }
            let asked = sites::last_build(&doc).is_none_or(|l| site.requested > l.at);
            if !site.auto && !asked {
                continue;
            }
            match self.site_build(&id, false).await {
                Ok(Outcome::Built(b)) => {
                    tracing::info!(site.id = %id, site.pages = b.pages, site.broken = b.broken_links.len(), site.ms = b.duration_ms, "site built");
                    built.push(id);
                }
                Ok(Outcome::Unchanged) => {}
                Err(e) => tracing::warn!(site.id = %id, error = %e, "a site did not build"),
            }
        }
        built
    }
}

/// Where a site is: its node path, and the public one if it has one.
pub fn addresses(node_http: &str, b: &Build) -> Vec<String> {
    b.published
        .iter()
        .map(|p| {
            if p.starts_with('/') {
                format!("{node_http}{p}")
            } else {
                p.clone()
            }
        })
        .collect()
}

/// A site's work folder (its dodeca project).
pub fn work_dir(agent_dir: &std::path::Path, id: &str) -> Result<PathBuf, String> {
    Ok(agent_dir.join("sites").join(crate::synced::id(id)?))
}
