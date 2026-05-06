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
    let mut app = AppConfig::default();
    app.api_key = Some(cfg.auth_token.clone());
    app.public_key = Some(cfg.public_key.clone());
    app.api.base_url = cfg.base_url.clone();
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
