use thiserror::Error;

#[derive(Debug, Error)]
pub enum TtcToolError {
    #[error("ttc client not installed; call swarms_tetrac::install() first")]
    NotInstalled,
    #[error("ttc client already installed")]
    AlreadyInstalled,
    #[error(transparent)]
    Api(#[from] skill_trading::TtcError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_installed_message_points_to_install_fn() {
        let s = TtcToolError::NotInstalled.to_string();
        assert!(s.contains("install"), "got: {s}");
    }

    #[test]
    fn already_installed_message_is_descriptive() {
        let s = TtcToolError::AlreadyInstalled.to_string();
        assert!(s.contains("already"), "got: {s}");
    }

    #[test]
    fn api_variant_wraps_skill_trading_error() {
        let inner = skill_trading::TtcError::MissingConfig("TTC_AUTH_TOKEN".into());
        let wrapped: TtcToolError = inner.into();
        assert!(matches!(wrapped, TtcToolError::Api(_)));
    }
}
