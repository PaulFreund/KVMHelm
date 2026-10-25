//! Native KVMHelm components. See docs/rust-port.md for the integration status.
//! The TypeScript daemon remains the operational entry point until parity is verified.

pub mod auth;
pub mod contracts;
pub mod core;
pub mod drivers;
pub mod error;
pub mod keyboard;
pub mod mcp;
pub mod media;
pub mod network;
mod private_fs;
mod queue;
pub mod secrets;
mod shared;
pub mod store;
pub mod transport;

pub use error::{GatewayError, Result};
pub use shared::monotonic_ms;
