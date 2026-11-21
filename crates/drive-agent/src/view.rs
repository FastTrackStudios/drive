//! What the Computers view shows: every device's locations, their
//! indexes as trees, who is online and where each content's copies are.
//! Built by the daemon; the FUSE mount (feature `mount`) draws it.

use std::collections::{HashMap, HashSet};

use drive_files::index::{LocationIndex, Tree};
use drive_files::space::Location;

/// What the folder shows: refreshed by the agent's loop.
#[derive(Default)]
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
pub struct View {
    pub me: String,
    /// (device id, its name), in order.
    pub devices: Vec<(String, String)>,
    pub locations: Vec<Location>,
    pub trees: HashMap<String, Tree>,
    pub online: HashSet<String>,
    /// Content (sampled hash, size) → where it is: (location id, path).
    pub copies: HashMap<(String, u64), Vec<(String, String)>>,
}

#[cfg_attr(not(feature = "mount"), allow(dead_code))]
impl View {
    pub fn build(
        me: &str,
        devices: Vec<(String, String)>,
        locations: Vec<Location>,
        indexes: HashMap<String, LocationIndex>,
        online: HashSet<String>,
    ) -> View {
        let mut copies: HashMap<(String, u64), Vec<(String, String)>> = HashMap::new();
        for idx in indexes.values() {
            for e in idx
                .entries
                .iter()
                .filter(|e| !e.dir && !e.sampled.is_empty())
            {
                copies
                    .entry((e.sampled.clone(), e.size))
                    .or_default()
                    .push((idx.location.clone(), e.path.clone()));
            }
        }
        let trees = indexes
            .into_iter()
            .map(|(k, v)| (k, Tree::new(v)))
            .collect();
        // Only devices that have a location.
        let devices = devices
            .into_iter()
            .filter(|(id, _)| locations.iter().any(|l| &l.device == id))
            .collect();
        View {
            me: me.to_string(),
            devices,
            locations,
            trees,
            online,
            copies,
        }
    }

    pub(crate) fn device_by_name(&self, name: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id.as_str())
    }

    pub(crate) fn device_name(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|(d, _)| d == id)
            .map_or(id.to_string(), |(_, n)| n.clone())
    }
}
