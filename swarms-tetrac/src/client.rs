use std::sync::{Arc, OnceLock, RwLock};

use skill_trading::api::Client;
use skill_trading::config::AppConfig;
use skill_trading::models::ExchangeCredentials;

use crate::TtcConfig;
use crate::error::TtcToolError;

pub(crate) struct TtcRuntime {
    pub client: Client,
    pub dry_run: bool,
}

/// `OnceLock` seeds the slot lazily; `RwLock` lets `install()` swap the
/// inner `Arc` after auth refresh without invalidating in-flight calls.
/// Each tool fn takes a cheap `Arc` snapshot at the start of its body.
static SLOT: OnceLock<RwLock<Arc<TtcRuntime>>> = OnceLock::new();

fn build_runtime(cfg: &TtcConfig) -> Result<TtcRuntime, TtcToolError> {
    let api = skill_trading::config::ApiConfig {
        base_url: cfg.base_url.clone(),
        ..Default::default()
    };
    let app = AppConfig {
        api_key: Some(cfg.auth_token.clone()),
        public_key: Some(cfg.public_key.clone()),
        api,
        ..Default::default()
    };
    let client = Client::new(&app)?;
    Ok(TtcRuntime {
        client,
        dry_run: cfg.dry_run,
    })
}

/// Install or replace the process-wide TTC runtime.
///
/// Idempotent: a second call swaps the inner `Arc` atomically. Pre-existing
/// tool calls that already took an `Arc` snapshot keep using the old client
/// until they finish; new calls see the new one. This is what makes auth
/// refresh possible without restarting the process.
pub fn install(cfg: &TtcConfig) -> Result<(), TtcToolError> {
    let new_runtime = Arc::new(build_runtime(cfg)?);
    if let Some(slot) = SLOT.get() {
        *slot.write().expect("runtime lock poisoned") = new_runtime;
        return Ok(());
    }
    // First install. If a parallel call beats us, fall through to swap.
    if SLOT.set(RwLock::new(new_runtime.clone())).is_err() {
        let slot = SLOT.get().expect("set failed but slot must exist");
        *slot.write().expect("runtime lock poisoned") = new_runtime;
    }
    Ok(())
}

pub(crate) fn runtime() -> Result<Arc<TtcRuntime>, TtcToolError> {
    let slot = SLOT.get().ok_or(TtcToolError::NotInstalled)?;
    Ok(slot.read().expect("runtime lock poisoned").clone())
}

pub(crate) fn dry_run() -> Result<bool, TtcToolError> {
    Ok(runtime()?.dry_run)
}

/// Resolve per-exchange credentials by delegating to skill-trading's
/// existing env-var / config priority chain. Reads
/// `{EXCHANGE}_API_KEY`, `{EXCHANGE}_API_SECRET`,
/// `{EXCHANGE}_API_PASSPHRASE` from process env.
pub(crate) fn credentials_for(exchange: &str) -> Result<ExchangeCredentials, TtcToolError> {
    let settings = AppConfig::default();
    Ok(skill_trading::commands::common::get_credentials(
        exchange, None, None, None, &settings,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `install()` is intentionally not exercised here — the OnceLock-backed
    /// SLOT is process-global. Integration coverage lives under D9 (mockito).
    #[test]
    fn runtime_errors_with_not_installed_before_install() {
        match runtime() {
            Ok(_) => panic!("runtime should be empty in unit tests; install() must not be called here"),
            Err(e) => assert!(matches!(e, TtcToolError::NotInstalled)),
        }
    }

    #[test]
    fn credentials_for_missing_env_returns_api_error() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var("ZZZTEST_API_KEY");
            std::env::remove_var("ZZZTEST_API_SECRET");
            std::env::remove_var("ZZZTEST_API_PASSPHRASE");
            std::env::remove_var("EXCHANGE_API_KEY");
            std::env::remove_var("EXCHANGE_API_SECRET");
        }
        let err = credentials_for("zzztest").unwrap_err();
        assert!(matches!(err, TtcToolError::Api(_)), "got: {err:?}");
    }

    #[test]
    fn credentials_for_reads_per_exchange_env_slot() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized via ENV_LOCK.
        unsafe {
            std::env::set_var("ZZZTEST_API_KEY", "k");
            std::env::set_var("ZZZTEST_API_SECRET", "s");
            std::env::remove_var("ZZZTEST_API_PASSPHRASE");
            std::env::remove_var("EXCHANGE_API_KEY");
            std::env::remove_var("EXCHANGE_API_SECRET");
        }
        let creds = credentials_for("zzztest").unwrap();
        assert_eq!(creds.api_key, "k");
        assert_eq!(creds.api_secret, "s");
        assert!(creds.passphrase.is_none());
        unsafe {
            std::env::remove_var("ZZZTEST_API_KEY");
            std::env::remove_var("ZZZTEST_API_SECRET");
        }
    }
}
