//! What differs between a browser and a native app: the socket, timers,
//! spawning, randomness and where this device's copies are kept. The
//! client (`lib.rs`) is the same on both.

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub(crate) use web::*;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::*;
