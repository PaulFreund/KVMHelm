//! Native KVMHelm components. See docs/rust-port.md for the integration status.
//! The TypeScript daemon remains the complete reference until native parity is verified.

pub mod auth;
pub mod cli;
pub mod contracts;
pub mod core;
pub mod drivers;
pub mod error;
pub mod keyboard;
pub mod mcp;
mod mcp_bridge;
mod mcp_http;
pub mod media;
pub mod network;
mod private_fs;
mod queue;
pub mod runtime;
pub mod secrets;
pub mod server;
mod shared;
pub mod store;
pub mod transport;

pub use error::{GatewayError, Result};
pub use shared::monotonic_ms;

pub mod web_assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}
