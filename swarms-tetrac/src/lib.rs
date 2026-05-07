//! TTC (ttc.box) integration for swarms-rs agents.
//!
//! See `tetrac-integration.md` at the workspace root for the PRD.

mod client;
pub mod config;
pub mod error;
mod parsers;
pub mod redact;
pub mod runtime;
pub mod tools;

pub use client::install;
pub use config::{ConfigError, TtcConfig};
pub use error::TtcToolError;
pub use redact::{RedactingFields, init_tracing};
pub use runtime::{
    CycleOutcome, LoopRunner, refresh_auth, refresh_if_stale, token_age_if_stale,
    with_auth_refresh, with_retry_on_auth,
};
