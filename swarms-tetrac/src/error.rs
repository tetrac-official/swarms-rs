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
