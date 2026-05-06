use std::env;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct TtcConfig {
    pub auth_token: String,
    pub public_key: String,
    pub base_url: String,
    pub default_exchange: Option<String>,
    pub dry_run: bool,
    pub max_loops_per_minute: u32,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required env var: {0}")]
    MissingEnv(&'static str),
}

impl TtcConfig {
    /// Build a config from process env vars.
    ///
    /// Required: `TTC_AUTH_TOKEN`, `TTC_PUBLIC_KEY`.
    /// Optional: `TTC_BASE_URL`, `TTC_DEFAULT_EXCHANGE`,
    /// `TTC_DRY_RUN`, `TTC_MAX_LOOPS_PER_MINUTE`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let auth_token = env::var("TTC_AUTH_TOKEN")
            .map_err(|_| ConfigError::MissingEnv("TTC_AUTH_TOKEN"))?;
        let public_key = env::var("TTC_PUBLIC_KEY")
            .map_err(|_| ConfigError::MissingEnv("TTC_PUBLIC_KEY"))?;
        let base_url =
            env::var("TTC_BASE_URL").unwrap_or_else(|_| "https://ttc.box/api/v1".into());
        let default_exchange = env::var("TTC_DEFAULT_EXCHANGE").ok();
        // dry_run defaults true so accidental runs of mutating tools
        // never hit live TTC; flip TTC_DRY_RUN=false to enable real calls.
        let dry_run = env::var("TTC_DRY_RUN")
            .map(|v| !matches!(v.as_str(), "0" | "false" | "FALSE" | ""))
            .unwrap_or(true);
        let max_loops_per_minute = env::var("TTC_MAX_LOOPS_PER_MINUTE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);

        Ok(Self {
            auth_token,
            public_key,
            base_url,
            default_exchange,
            dry_run,
            max_loops_per_minute,
        })
    }
}
