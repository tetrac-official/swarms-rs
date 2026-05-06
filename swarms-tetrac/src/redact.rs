//! Tracing field redaction.
//!
//! Replaces values for secret-named fields with `[REDACTED]` before they
//! reach the log output. Defends against accidental `tracing::info!(token = …)`
//! and against `reqwest`'s debug logging when callers enable
//! `RUST_LOG=reqwest=debug` (which dumps headers including `ttc-auth-token`).

use std::fmt;

use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

const REDACTED: &str = "[REDACTED]";

/// Field names treated as secrets. Matched case-insensitively after
/// normalizing hyphens to underscores.
const SECRET_FIELDS: &[&str] = &[
    "ttc_auth_token",
    "ttc_public_key",
    "ttc_passkey",
    "ttc_email",
    "authorization",
    "api_key",
    "api_secret",
    "passphrase",
    "x_wallet_private_key",
];

pub fn is_secret_field(name: &str) -> bool {
    let normalized = name.replace('-', "_").to_ascii_lowercase();
    SECRET_FIELDS.iter().any(|s| *s == normalized)
}

/// `FormatFields` impl that redacts values for fields whose names match
/// [`is_secret_field`].
#[derive(Default)]
pub struct RedactingFields {
    _private: (),
}

impl RedactingFields {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(
        &self,
        writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut visitor = RedactingVisitor::new(writer);
        fields.record(&mut visitor);
        visitor.result
    }
}

struct RedactingVisitor<'writer> {
    writer: Writer<'writer>,
    result: fmt::Result,
    first: bool,
}

impl<'writer> RedactingVisitor<'writer> {
    fn new(writer: Writer<'writer>) -> Self {
        Self {
            writer,
            result: Ok(()),
            first: true,
        }
    }
}

impl Visit for RedactingVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.result.is_err() {
            return;
        }
        let res = self.write_field(field, |w| {
            if is_secret_field(field.name()) {
                w.write_str(REDACTED)
            } else {
                write!(w, "{value:?}")
            }
        });
        self.result = res;
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if self.result.is_err() {
            return;
        }
        let res = self.write_field(field, |w| {
            if is_secret_field(field.name()) {
                w.write_str(REDACTED)
            } else {
                w.write_str(value)
            }
        });
        self.result = res;
    }
}

impl<'writer> RedactingVisitor<'writer> {
    fn write_field(
        &mut self,
        field: &Field,
        write_value: impl FnOnce(&mut Writer<'writer>) -> fmt::Result,
    ) -> fmt::Result {
        if !self.first {
            self.writer.write_char(' ')?;
        }
        self.first = false;
        if field.name() == "message" {
            write_value(&mut self.writer)
        } else {
            write!(self.writer, "{}=", field.name())?;
            write_value(&mut self.writer)
        }
    }
}

/// Install a redacting tracing subscriber as the global default.
/// Honors `RUST_LOG`. Idempotent: returns early if a subscriber is already set.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = fmt::layer().fmt_fields(RedactingFields::new());
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_field_names_match_case_insensitively() {
        assert!(is_secret_field("ttc-auth-token"));
        assert!(is_secret_field("TTC_AUTH_TOKEN"));
        assert!(is_secret_field("Ttc-Auth-Token"));
        assert!(is_secret_field("TTC_PASSKEY"));
        assert!(is_secret_field("authorization"));
        assert!(is_secret_field("Authorization"));
        assert!(is_secret_field("x-wallet-private-key"));
    }

    #[test]
    fn non_secret_field_names_pass_through() {
        assert!(!is_secret_field("symbol"));
        assert!(!is_secret_field("exchange"));
        assert!(!is_secret_field("quantity"));
        assert!(!is_secret_field("message"));
    }

    /// End-to-end: emit a span+event with secret field, capture the formatted
    /// output, assert the secret value is not present and the redaction marker is.
    #[test]
    fn redacts_secret_field_values_in_captured_output() {
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::fmt;
        use tracing_subscriber::prelude::*;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Buf::default();
        let buf_for_writer = buf.clone();
        let layer = fmt::layer()
            .fmt_fields(RedactingFields::new())
            .with_writer(move || buf_for_writer.clone())
            .without_time()
            .with_ansi(false);

        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                ttc_auth_token = "super-secret-token-abc",
                ttc_public_key = "pk-deadbeef",
                symbol = "BTC-USDT",
                "starting trade"
            );
        });

        let captured = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            !captured.contains("super-secret-token-abc"),
            "auth token leaked into log output: {captured:?}"
        );
        assert!(
            !captured.contains("pk-deadbeef"),
            "public key leaked into log output: {captured:?}"
        );
        assert!(
            captured.contains("[REDACTED]"),
            "redaction marker missing: {captured:?}"
        );
        assert!(
            captured.contains("BTC-USDT"),
            "non-secret field was wrongly redacted: {captured:?}"
        );
    }

    #[test]
    fn ttc_config_debug_does_not_leak_auth_token() {
        use crate::TtcConfig;
        let cfg = TtcConfig {
            auth_token: "super-secret-token-abc".into(),
            public_key: "pk-deadbeef".into(),
            base_url: "https://ttc.box/api/v1".into(),
            default_exchange: None,
            dry_run: true,
            max_loops_per_minute: 60,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("super-secret-token-abc"), "leaked: {s}");
        assert!(!s.contains("pk-deadbeef"), "leaked: {s}");
        assert!(s.contains("[REDACTED]"));
        assert!(s.contains("ttc.box"));
    }
}
