use std::sync::OnceLock;

use skill_trading::api::Client;
use skill_trading::config::AppConfig;
use skill_trading::models::ExchangeCredentials;

use crate::TtcConfig;
use crate::error::TtcToolError;

static CLIENT: OnceLock<Client> = OnceLock::new();

/// Build a TTC API client from `cfg` and store it in the process-wide
/// `OnceLock`. Subsequent tool calls read from this slot.
///
/// Returns `AlreadyInstalled` if called twice. Re-installing would
/// silently change the client every active tool sees, which is the
/// kind of surprise that turns into a debugging nightmare; force the
/// caller to be explicit instead.
pub fn install(cfg: &TtcConfig) -> Result<(), TtcToolError> {
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
    CLIENT
        .set(client)
        .map_err(|_| TtcToolError::AlreadyInstalled)
}

pub(crate) fn client() -> Result<&'static Client, TtcToolError> {
    CLIENT.get().ok_or(TtcToolError::NotInstalled)
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

    /// `install()` is intentionally not exercised here — the OnceLock
    /// is process-global and a successful install would poison every
    /// other test in this module. Integration coverage of install
    /// belongs under D9 (mockito-backed).
    #[test]
    fn client_errors_with_not_installed_before_install() {
        match client() {
            Ok(_) => panic!("client should be empty in unit tests; install() must not be called here"),
            Err(e) => assert!(matches!(e, TtcToolError::NotInstalled)),
        }
    }

    #[test]
    fn credentials_for_missing_env_returns_api_error() {
        let _g = ENV_LOCK.lock().unwrap();
        // Make sure neither the per-exchange slot nor the global
        // override is set in this test's process env.
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
        // Cleanup so the next test sees a clean slate.
        unsafe {
            std::env::remove_var("ZZZTEST_API_KEY");
            std::env::remove_var("ZZZTEST_API_SECRET");
        }
    }
}
