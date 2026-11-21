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
    pub async fn make_available(&self) -> usize {
        let kinds = Kinds::load(&self.spine);
        let all = projects::projects(&self.space);
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
                    || kinds.of_file(name, false).keeps_available(e.size);
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
}
