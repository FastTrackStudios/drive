//! A device's iroh endpoint: its key (kept in a file), how it reaches
//! others (our relay, n0's, or direct addresses only), and finding the
//! devices nearby (mDNS). Shared by `drive-transfer` (files) and
//! `drive-sync` (documents), so a device has one identity for both.

use std::path::Path;

pub use iroh::{EndpointAddr, EndpointId, SecretKey};

/// How a device's endpoint reaches others.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Our own relay (`https://relay.example.com`), instead of n0's public
    /// ones. Empty: n0's.
    pub relay: Option<String>,
    /// No relay and no address lookup at all: direct addresses only (tests,
    /// and machines that only ever talk on one network).
    pub local_only: bool,
}

/// This device's endpoint key, kept in `key_file` (made the first time;
/// mode 600). Its public half is the device's endpoint id.
pub fn secret_key(key_file: &Path) -> Result<iroh::SecretKey, String> {
    if let Ok(bytes) = std::fs::read(key_file)
        && let Ok(b) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return Ok(iroh::SecretKey::from_bytes(&b));
    }
    let key = iroh::SecretKey::generate();
    if let Some(dir) = key_file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    drive_local::write_private(key_file, &key.to_bytes())
        .map_err(|e| format!("{}: {e}", key_file.display()))?;
    Ok(key)
}

/// An endpoint with this key, speaking `alpns` (empty: dial only).
pub async fn endpoint(
    key: iroh::SecretKey,
    settings: &Settings,
    alpns: Vec<Vec<u8>>,
) -> Result<iroh::Endpoint, String> {
    use iroh::endpoint::presets;
    let builder = if settings.local_only {
        iroh::Endpoint::builder(presets::Minimal).relay_mode(iroh::RelayMode::Disabled)
    } else {
        let b = iroh::Endpoint::builder(presets::N0);
        match settings.relay.as_deref().filter(|r| !r.is_empty()) {
            Some(url) => {
                let url: iroh::RelayUrl = url.parse().map_err(|e| format!("relay {url}: {e}"))?;
                b.relay_mode(iroh::RelayMode::custom([url]))
            }
            None => b,
        }
    };
    builder
        .secret_key(key)
        .alpns(alpns)
        .bind()
        .await
        .map_err(|e| format!("iroh endpoint: {e}"))
}

/// An endpoint address as text (what the agent tells the node), and back.
pub fn addr_to_string(addr: &EndpointAddr) -> String {
    serde_json::to_string(addr).expect("an address serializes")
}

pub fn addr_from_str(s: &str) -> Result<EndpointAddr, String> {
    if let Ok(a) = serde_json::from_str::<EndpointAddr>(s) {
        return Ok(a);
    }
    // A bare endpoint id: found through address lookup.
    s.parse::<EndpointId>()
        .map(EndpointAddr::new)
        .map_err(|e| format!("not an endpoint address: {e}"))
}

/// The mDNS service Drive's devices advertise under, so a scan finds only
/// them (not every iroh app on the network).
pub const MDNS_SERVICE: &str = "drive";

/// Advertise this endpoint on the local network and find the others
/// there: the returned lookup is added to the endpoint, and its
/// [`iroh_mdns_address_lookup::MdnsAddressLookup::subscribe`] says who
/// appears and goes. Needs a Tokio runtime.
pub fn mdns(
    endpoint: &iroh::Endpoint,
) -> Result<iroh_mdns_address_lookup::MdnsAddressLookup, String> {
    let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder()
        .service_name(MDNS_SERVICE)
        .build(endpoint.id())
        .map_err(|e| format!("mdns: {e}"))?;
    endpoint
        .address_lookup()
        .map_err(|e| format!("mdns: {e}"))?
        .add(mdns.clone());
    Ok(mdns)
}

pub use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
