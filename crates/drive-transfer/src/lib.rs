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

pub use fetch::{Fetcher, Source};
pub use iroh::{EndpointAddr, EndpointId};
pub use iroh_blobs::Hash;
pub use provide::{Allow, Extra, Provided, Provider};

pub use drive_net::{Settings, addr_from_str, addr_to_string, endpoint, secret_key};

#[cfg(test)]
mod tests;
