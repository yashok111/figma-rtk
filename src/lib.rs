//! figma-rtk — token-killer reverse proxy for the Figma MCP server.
//!
//! Library surface so integration tests can drive the proxy and reuse the
//! transform logic. The `frtk` binary (see `main.rs`) is a thin CLI on top.

pub mod cache;
pub mod capture;
pub mod compress;
pub mod config;
pub mod filter;
pub mod fsutil;
pub mod init;
pub mod mcp;
pub mod proxy;
pub mod stats;
pub mod status;
pub mod tee;
pub mod tokens;
pub mod trust;
