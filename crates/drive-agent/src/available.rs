//! Keeping files available: what a project marks available (its client
//! mixes) and the kinds kept available (charts, documents) go onto the node
//! from the device that has them, so they can be had, and shared, while
//! that device is off. Big media stays put.

use drive_files::projects::{self, Available};
use drive_files::space;
use drive_spine::kinds::Kinds;

use crate::agent::Agent;

impl Agent {
    /// Put what should be available on the node; returns how many files went up.
    #[tracing::instrument(name = "drive.available.make", skip_all, fields(process.rss_mb = tracing::field::Empty))]
    pub async fn make_available(&self) -> usize {
        let _rss = crate::telemetry::RssOnDrop;
        let kinds = Kinds::load(&self.spine);
        let all = projects::projects(&self.space);
        // Files a rule asked to keep highly available (docs/rules.md).
        let wished = self.rule_wishes().await;
        let mut put = 0;
        for loc in self.my_locations() {
            let Some(index) = self.cached_index(&loc.id) else {
                continue;
            };
            let have = projects::available(&self.space, &loc.id);
            let mut changed = false;
            for e in index
                .entries
                .iter()
                .filter(|e| !e.dir && !e.sampled.is_empty())
            {
                let name = e.path.rsplit('/').next().unwrap_or_default();
                let wanted = projects::in_available_part(&all, &loc.id, &e.path)
                    || kinds.of_file(name, false).keeps_available(e.size)
                    || wished.contains(&(loc.id.clone(), e.path.clone()));
                let rec = have.get(&e.path);
                if !wanted {
                    if rec.is_some() {
                        let _ = projects::set_available(&self.space, &loc.id, &e.path, None);
                        changed = true;
                    }
                    continue;
                }
                if rec.is_some_and(|r| r.size == e.size as i64 && r.mtime == e.mtime) {
                    continue;
                }
                match self.answer_fetch(&loc.id, &e.path).await {
                    Ok(hash) => {
                        let a = Available {
                            hash: hash.clone(),
                            size: e.size as i64,
                            mtime: e.mtime,
                        };
                        let _ = projects::set_available(&self.space, &loc.id, &e.path, Some(&a));
                        let _ = space::set_held(&self.space, "node", &hash, Some("stored"));
                        changed = true;
                        put += 1;
                    }
                    Err(err) => {
                        tracing::warn!(location.name = %loc.name, error = %err, "a file could not be made available")
                    }
                }
            }
            // Files gone from the location are no longer available from it.
            for path in have
                .keys()
                .filter(|p| !index.entries.iter().any(|e| &e.path == *p))
            {
                let _ = projects::set_available(&self.space, &loc.id, path, None);
                changed = true;
            }
            if changed {
                self.commit_space();
            }
        }
        put
    }

    /// Copy every available file into the public storages this device runs
    /// (the R2 bucket), so share links can send people straight there.
    /// Returns how many went up.
    #[tracing::instrument(name = "drive.available.publish", skip_all, fields(process.rss_mb = tracing::field::Empty))]
    pub async fn publish_available(&self) -> usize {
        let _rss = crate::telemetry::RssOnDrop;
        let public = space::public(&self.space);
        let mut hashes: Vec<String> = projects::all_available(&self.space)
            .into_iter()
            .map(|(_, _, a)| a.hash)
            .filter(|h| !h.is_empty())
            .collect();
        hashes.sort();
        hashes.dedup();
        let mut put = 0;
        for (sid, store) in self
            .stores
            .iter()
            .filter(|(sid, _)| public.contains_key(sid))
        {
            for h in &hashes {
                if space::holders(&self.space, h)
                    .iter()
                    .any(|(who, _)| who == sid)
                {
                    continue;
                }
                let bytes = match self.fetch_bytes(h).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(storage.id = %sid, error = %e, "an available file could not be read to publish");
                        continue;
                    }
                };
                match store.put_checked(h, bytes).await {
                    Ok(()) => {
                        let _ = space::set_held(&self.space, sid, h, Some("stored"));
                        put += 1;
                    }
                    Err(e) => {
                        tracing::warn!(storage.id = %sid, error = %e, "a public storage refused a file")
                    }
                }
            }
        }
        if put > 0 {
            self.commit_space();
        }
        put
    }
}

impl Agent {
    /// (location, path) of every file a rule wants highly available.
    async fn rule_wishes(&self) -> std::collections::HashSet<(String, String)> {
        let space = self.cfg.lock().expect("cfg").space.clone();
        let doc = self
            .drive
            .open(&drive_rules::journal::journal_doc(&space))
            .await;
        drive_rules::journal::wished_available(&doc)
            .into_iter()
            .filter_map(|i| match i {
                drive_rules::Item::Location { location, path, .. } => Some((location, path)),
                drive_rules::Item::Drive { .. } => None,
            })
            .collect()
    }
}
