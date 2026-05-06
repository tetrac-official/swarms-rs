//! Tool definitions land here in PRD D3 (read tools) and D4 (mutating).
//!
//! Each tool wraps one method on `skill_trading::api::Client` via the
//! `#[tool]` proc macro from `swarms-macro`. Per-agent state (the
//! shared TTC client) is held in a `OnceLock` initialized by
//! `with_tetrac()`. See PRD §5.3.
