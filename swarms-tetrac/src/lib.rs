//! TTC (ttc.box) integration for swarms-rs agents.
//!
//! See `tetrac-integration.md` at the workspace root for the PRD.

mod client;
pub mod config;
pub mod error;
pub mod tools;

pub use client::install;
pub use config::{ConfigError, TtcConfig};
pub use error::TtcToolError;
