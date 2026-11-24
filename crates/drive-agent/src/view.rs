//! What the Devices folder shows: every device's locations, who is online
//! and which disks each has. Kept by the daemon; the FUSE mount (feature
//! `mount`) draws it. The locations' entries and their copies are not kept
//! here: the mount asks the index database for them (drive#47).

use std::collections::HashSet;

use drive_files::space::{Location, Volume};

/// What the folder shows: refreshed by the agent's loop.
#[derive(Default)]
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
pub struct View {
    pub me: String,
    /// (device id, its name), in order.
    pub devices: Vec<(String, String)>,
    pub locations: Vec<Location>,
    pub online: HashSet<String>,
}

#[cfg_attr(not(feature = "mount"), allow(dead_code))]
impl View {
    /// Who is here and where.
    pub fn set_devices(
        &mut self,
        me: &str,
        devices: Vec<(String, String)>,
        locations: Vec<Location>,
        online: HashSet<String>,
        volumes: &[Volume],
    ) {
        // Only devices that have a location or a disk (not browsers).
        self.devices = devices
            .into_iter()
            .filter(|(id, _)| {
                locations.iter().any(|l| &l.device == id) || volumes.iter().any(|v| &v.device == id)
            })
            .collect();
        self.me = me.to_string();
        self.locations = locations;
        self.online = online;
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
