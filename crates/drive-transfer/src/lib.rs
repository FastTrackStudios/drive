//! Drive's bulk bytes between devices (docs/transfer.md).
//!
//! Every device runs one iroh endpoint. An agent serves files of its own
//! disks as BLAKE3-verified blobs (iroh-blobs), **read-only and in place**:
//! the blob store references a file where it is (an external entry) and
//! keeps only the file's outboard (the BLAKE3 tree that lets any range be
//! verified) in the agent's own folder. The full hash is computed the first
//! time a file is asked for and remembered by (path, size, mtime), so a
//! changed file is hashed again.
//!
//! The node (or any device) fetches exactly the byte ranges it needs from
//! the device that has the file, verified chunk by chunk, and picks up
//! where it left off when a connection drops ([`Fetcher`]).
//!
//! Who may connect is the caller's rule ([`Allow`]): Drive's agents let in
//! only their org's node and devices (endpoint ids in the space registry).
//! Everyone else is refused at the handshake, with one `warn` each.

mod fetch;
mod provide;
mod served;

use std::path::Path;

pub use fetch::{Fetcher, Source};
pub use iroh::{EndpointAddr, EndpointId};
pub use iroh_blobs::Hash;
pub use provide::{Allow, Provided, Provider};

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
    std::fs::write(key_file, key.to_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_file, std::fs::Permissions::from_mode(0o600));
    }
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

#[cfg(test)]
mod tests;
