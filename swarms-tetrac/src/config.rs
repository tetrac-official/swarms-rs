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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VARS: &[&str] = &[
        "TTC_AUTH_TOKEN",
        "TTC_PUBLIC_KEY",
        "TTC_BASE_URL",
        "TTC_DEFAULT_EXCHANGE",
        "TTC_DRY_RUN",
        "TTC_MAX_LOOPS_PER_MINUTE",
    ];

    fn clear_env() {
        // SAFETY: every caller holds ENV_LOCK so no concurrent reader/writer.
        unsafe {
            for k in VARS {
                std::env::remove_var(k);
            }
        }
    }

    fn set(k: &str, v: &str) {
        // SAFETY: every caller holds ENV_LOCK so no concurrent reader/writer.
        unsafe {
            std::env::set_var(k, v);
        }
    }

    #[test]
    fn from_env_requires_auth_token() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let err = TtcConfig::from_env().unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnv("TTC_AUTH_TOKEN")));
    }

    #[test]
    fn from_env_requires_public_key() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        let err = TtcConfig::from_env().unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnv("TTC_PUBLIC_KEY")));
    }

    #[test]
    fn from_env_minimum_required_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "tok");
        set("TTC_PUBLIC_KEY", "pub");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.auth_token, "tok");
        assert_eq!(cfg.public_key, "pub");
    }

    #[test]
    fn from_env_dry_run_defaults_true() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        let cfg = TtcConfig::from_env().unwrap();
        assert!(cfg.dry_run);
    }

    #[test]
    fn from_env_dry_run_disabled_by_false() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        set("TTC_DRY_RUN", "false");
        let cfg = TtcConfig::from_env().unwrap();
        assert!(!cfg.dry_run);
    }

    #[test]
    fn from_env_dry_run_disabled_by_zero() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        set("TTC_DRY_RUN", "0");
        let cfg = TtcConfig::from_env().unwrap();
        assert!(!cfg.dry_run);
    }

    #[test]
    fn from_env_base_url_defaults_to_ttc_box() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.base_url, "https://ttc.box/api/v1");
    }

    #[test]
    fn from_env_base_url_overridable() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        set("TTC_BASE_URL", "http://localhost:3000/api/v1");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.base_url, "http://localhost:3000/api/v1");
    }

    #[test]
    fn from_env_default_exchange_optional() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        let cfg = TtcConfig::from_env().unwrap();
        assert!(cfg.default_exchange.is_none());

        set("TTC_DEFAULT_EXCHANGE", "orderly");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.default_exchange.as_deref(), Some("orderly"));
    }

    #[test]
    fn from_env_max_loops_defaults_60() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.max_loops_per_minute, 60);
    }

    #[test]
    fn from_env_max_loops_overridable() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        set("TTC_MAX_LOOPS_PER_MINUTE", "120");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.max_loops_per_minute, 120);
    }

    #[test]
    fn from_env_max_loops_invalid_falls_back() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("TTC_AUTH_TOKEN", "x");
        set("TTC_PUBLIC_KEY", "y");
        set("TTC_MAX_LOOPS_PER_MINUTE", "not-a-number");
        let cfg = TtcConfig::from_env().unwrap();
        assert_eq!(cfg.max_loops_per_minute, 60);
    }
}
