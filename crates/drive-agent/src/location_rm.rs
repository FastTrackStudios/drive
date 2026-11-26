//! `drive location rm`: Drive stops tracking a folder.
//!
//! Removing a location takes it out of the space's registry, drops its
//! index on the node (its rows and per-extension totals) and this
//! device's copy of that index. The running agent then stops watching it
//! and the views (the mount's Devices folder, the app's sidebar) lose it
//! as the registry change reaches them. The folder and every file in it
//! stay exactly as they are: nothing here opens, moves or deletes them.

use drive_files::adopt::Adoption;
use drive_files::space::{self, Location};

use crate::agent::Agent;

/// What removing a location did.
pub struct Removed {
    pub location: Location,
    /// Whether the node dropped its index. When it was unreachable the
    /// rows stay, named by no location; `drive location rm ID --force`
    /// drops them later.
    pub node_dropped: Result<(), String>,
    /// Other locations mirroring this one (a storage server's copies):
    /// left as they are.
    pub mirrors: Vec<Location>,
}

/// Why `loc` may not be removed, if it may not: an adopted folder maps
/// into it (always refused: unadopt first), or it is another device's and
/// `force` was not given (the registry is shared, so removing it there
/// removes it for that device too).
pub fn refusal(
    loc: &Location,
    me: &str,
    holder: &str,
    adoptions: &[Adoption],
    force: bool,
) -> Option<String> {
    let into: Vec<&str> = adoptions
        .iter()
        .filter(|a| a.location == loc.id)
        .map(|a| a.at.as_str())
        .collect();
    if !into.is_empty() {
        let first = into[0];
        return Some(format!(
            "{} holds adopted folders ({}): `drive unadopt {first}` first",
            loc.name,
            into.join(", ")
        ));
    }
    if loc.device != me && !force {
        return Some(format!(
            "{} is a location of {holder}, not of this device; the registry is shared, so removing it here removes it there too: add --force to do it anyway",
            loc.name
        ));
    }
    None
}

/// The location a name or id means: an id first, then a name, this
/// device's own preferred when several devices use the name.
pub fn find(locations: &[Location], me: &str, who: &str) -> Result<Location, String> {
    if let Some(l) = locations.iter().find(|l| l.id == who) {
        return Ok(l.clone());
    }
    let named: Vec<&Location> = locations.iter().filter(|l| l.name == who).collect();
    let mine: Vec<&&Location> = named.iter().filter(|l| l.device == me).collect();
    match (named.as_slice(), mine.as_slice()) {
        ([], _) => Err(format!("no location {who}")),
        ([one], _) => Ok((*one).clone()),
        (_, [one]) => Ok((**one).clone()),
        _ => Err(format!(
            "several locations are called {who}; name one by its id: {}",
            named
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

impl Agent {
    /// Stop tracking a location (see the module). Never touches its
    /// folder or files.
    #[tracing::instrument(name = "drive.location.rm", skip_all, fields(
        location.id = tracing::field::Empty,
        location.elsewhere = tracing::field::Empty,
        location.force = force,
        location.outcome = tracing::field::Empty,
        node.dropped = tracing::field::Empty,
    ))]
    pub async fn remove_location(&self, who: &str, force: bool) -> Result<Removed, String> {
        let span = tracing::Span::current();
        let me = self.device_id();
        let locations = space::locations(&self.space);
        let loc = find(&locations, &me, who).inspect_err(|_| {
            span.record("location.outcome", "missing");
        })?;
        span.record("location.id", loc.id.as_str());
        span.record("location.elsewhere", loc.device != me);
        let holder = space::devices(&self.space)
            .into_iter()
            .find(|d| d.id == loc.device)
            .map_or(loc.device.clone(), |d| d.name);
        if let Some(why) = refusal(&loc, &me, &holder, &self.adoptions(), force) {
            span.record("location.outcome", "refused");
            tracing::warn!(location.id = %loc.id, location.elsewhere = loc.device != me, "a location was not removed");
            return Err(why);
        }
        space::remove_location(&self.space, &loc.id).map_err(|e| e.to_string())?;
        self.commit_space();
        self.forget_index(&loc.id);
        let node_dropped = self.drop_node_index(&loc.id).await;
        span.record("node.dropped", node_dropped.is_ok());
        span.record("location.outcome", "removed");
        let mirrors = locations
            .into_iter()
            .filter(|l| l.mirror_of == loc.id)
            .collect();
        Ok(Removed {
            location: loc,
            node_dropped,
            mirrors,
        })
    }

    /// Ask the node to drop a location's index (its rows and totals).
    pub(crate) async fn drop_node_index(&self, location: &str) -> Result<(), String> {
        let (node, space_name) = {
            let c = self.cfg.lock().expect("cfg");
            (c.node_http(), c.space.clone())
        };
        let resp = self
            .http
            .delete(format!("{node}/index/{space_name}/{location}"))
            .send()
            .await
            .map_err(|e| format!("the node: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("the node refused ({})", resp.status()))
        }
    }

    /// Forget this device's copy of a location's index: the decoded one in
    /// memory and its file in Drive's own folder (`indexes/<id>.idx`).
    pub(crate) fn forget_index(&self, location: &str) {
        self.unpublished
            .lock()
            .expect("unpublished")
            .remove(location);
        let (db, id) = (self.index_db.clone(), location.to_string());
        tokio::spawn(async move {
            if let Err(e) = db.remove(&id).await {
                tracing::warn!(location.id = %id, error = %e, "an index stayed in the index database");
            }
        });
        self.fetched.lock().expect("fetched").remove(location);
        if let Ok(p) = self.index_cache(location) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Whether the registry still has a location (it may have been removed
    /// while it was being indexed).
    pub(crate) fn has_location(&self, location: &str) -> bool {
        space::locations(&self.space)
            .iter()
            .any(|l| l.id == location)
    }
}

#[cfg(test)]
mod tests;
