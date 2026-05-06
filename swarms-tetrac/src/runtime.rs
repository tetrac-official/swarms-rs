//! Runtime helpers for unattended operation: a fixed-interval loop runner,
//! a subprocess-based auth-refresh helper, and a retry-on-401 wrapper.
//!
//! `swarms-rs` itself has no built-in scheduler (see the `// TODO: Loop
//! interval` in `swarms_agent.rs`). This module fills that gap.

use std::env;
use std::time::Duration;

use tokio::process::Command;
use tokio::signal;
use tokio::time::{MissedTickBehavior, interval};

use crate::TtcConfig;
use crate::error::TtcToolError;

const DEFAULT_SKILL_TRADING_BIN: &str =
    "/Users/mac/Documents/rust-cli-ttc-api/.claude/skills/skill-trading/scripts/skill-trading";

/// Runs an async closure on a fixed interval until cancelled.
///
/// On each tick the closure is invoked. If it errors, the runner logs the
/// error, sleeps `failure_backoff`, and continues. Catches Ctrl-C for
/// graceful shutdown.
pub struct LoopRunner {
    pub interval: Duration,
    pub max_ticks: Option<u64>,
    pub failure_backoff: Duration,
}

impl Default for LoopRunner {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            max_ticks: None,
            failure_backoff: Duration::from_secs(60),
        }
    }
}

impl LoopRunner {
    pub fn every(period: Duration) -> Self {
        Self {
            interval: period,
            ..Default::default()
        }
    }

    pub fn max_ticks(mut self, n: u64) -> Self {
        self.max_ticks = Some(n);
        self
    }

    pub fn failure_backoff(mut self, dur: Duration) -> Self {
        self.failure_backoff = dur;
        self
    }

    pub async fn run<F, Fut>(self, mut task: F) -> Result<(), TtcToolError>
    where
        F: FnMut(u64) -> Fut,
        Fut: std::future::Future<Output = Result<(), TtcToolError>>,
    {
        let mut ticker = interval(self.interval);
        // Skip rather than burst-catch-up if a tick takes longer than the period.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let limit = self.max_ticks.unwrap_or(u64::MAX);
        let mut tick: u64 = 0;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if tick >= limit {
                        tracing::info!(ticks_completed = tick, "loop runner reached max_ticks");
                        return Ok(());
                    }
                    let cycle = tick;
                    tick += 1;
                    tracing::debug!(cycle, "tick");
                    if let Err(e) = task(cycle).await {
                        tracing::error!(cycle, error = %e, "task failed; backing off");
                        tokio::time::sleep(self.failure_backoff).await;
                    }
                }
                _ = signal::ctrl_c() => {
                    tracing::info!(ticks_completed = tick, "ctrl-c received; shutting down");
                    return Ok(());
                }
            }
        }
    }
}

/// True if `e` is a 401 Unauthorized from skill-trading. The TTC backend
/// returns this when the 24h session token has expired.
pub fn is_auth_error(e: &TtcToolError) -> bool {
    matches!(
        e,
        TtcToolError::Api(skill_trading::TtcError::Api { code: 401, .. })
    )
}

/// Run `skill-trading login` from cwd, reload `.env`, and re-install the
/// runtime. Idempotent — safe to call repeatedly.
///
/// Looks up the binary via `SKILL_TRADING_BIN` env var, falling back to the
/// dev-machine default. CI / production deployments should set
/// `SKILL_TRADING_BIN` explicitly.
pub async fn refresh_auth() -> Result<(), TtcToolError> {
    let bin = env::var("SKILL_TRADING_BIN")
        .unwrap_or_else(|_| DEFAULT_SKILL_TRADING_BIN.to_string());
    tracing::info!(bin = %bin, "refreshing TTC auth via skill-trading login");

    let output = Command::new(&bin)
        .arg("login")
        .output()
        .await
        .map_err(|e| TtcToolError::InvalidArg(format!("subprocess {bin} failed to spawn: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TtcToolError::InvalidArg(format!(
            "skill-trading login exited {}: {stderr}",
            output.status
        )));
    }

    // Reload .env so the new TTC_AUTH_TOKEN overrides the in-process env.
    if std::path::Path::new(".env").exists() {
        dotenvy::from_filename_override(".env")
            .map_err(|e| TtcToolError::InvalidArg(format!(".env reload failed: {e}")))?;
    }

    let cfg = TtcConfig::from_env()
        .map_err(|e| TtcToolError::InvalidArg(format!("post-refresh config: {e}")))?;
    crate::install(&cfg)?;
    tracing::info!("TTC auth refreshed");
    Ok(())
}

/// Invoke `f`. On a 401-shaped error, run `refresh()` once, then retry `f`.
/// The refresh is parameterized so tests can inject a stub.
pub async fn with_retry_on_auth<F, Fut, R, RFut, T>(
    mut f: F,
    refresh: R,
) -> Result<T, TtcToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, TtcToolError>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<(), TtcToolError>>,
{
    match f().await {
        Ok(t) => Ok(t),
        Err(e) if is_auth_error(&e) => {
            tracing::warn!(error = %e, "auth error; refreshing once and retrying");
            refresh().await?;
            f().await
        }
        Err(e) => Err(e),
    }
}

/// Convenience: [`with_retry_on_auth`] using [`refresh_auth`] as the refresh.
pub async fn with_auth_refresh<F, Fut, T>(f: F) -> Result<T, TtcToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, TtcToolError>>,
{
    with_retry_on_auth(f, refresh_auth).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn is_auth_error_matches_401_only() {
        let e_401 = TtcToolError::Api(skill_trading::TtcError::Api {
            code: 401,
            message: "Unauthorized".into(),
        });
        let e_500 = TtcToolError::Api(skill_trading::TtcError::Api {
            code: 500,
            message: "Internal".into(),
        });
        let e_other = TtcToolError::NotInstalled;
        assert!(is_auth_error(&e_401));
        assert!(!is_auth_error(&e_500));
        assert!(!is_auth_error(&e_other));
    }

    #[tokio::test]
    async fn loop_runner_calls_task_max_ticks_times() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(20))
            .max_ticks(3)
            .failure_backoff(Duration::from_millis(0))
            .run(move |_cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn loop_runner_continues_after_failures() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(10))
            .max_ticks(3)
            .failure_backoff(Duration::from_millis(0))
            .run(move |cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    if cycle == 1 {
                        Err(TtcToolError::NotInstalled)
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn with_retry_on_auth_refreshes_and_retries_on_401() {
        let attempt = Arc::new(AtomicU64::new(0));
        let refresh_called = Arc::new(AtomicU64::new(0));

        let attempt_for_f = attempt.clone();
        let f = move || {
            let c = attempt_for_f.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err::<&'static str, _>(TtcToolError::Api(skill_trading::TtcError::Api {
                        code: 401,
                        message: "expired".into(),
                    }))
                } else {
                    Ok("ok")
                }
            }
        };

        let refresh_for_r = refresh_called.clone();
        let refresh = || {
            let c = refresh_for_r.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };

        let result = with_retry_on_auth(f, refresh).await.unwrap();
        assert_eq!(result, "ok");
        assert_eq!(attempt.load(Ordering::SeqCst), 2, "f called twice");
        assert_eq!(refresh_called.load(Ordering::SeqCst), 1, "refresh called once");
    }

    #[tokio::test]
    async fn with_retry_on_auth_does_not_refresh_for_non_auth_errors() {
        let attempt = Arc::new(AtomicU64::new(0));
        let refresh_called = Arc::new(AtomicU64::new(0));

        let attempt_for_f = attempt.clone();
        let f = move || {
            let c = attempt_for_f.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TtcToolError::Api(skill_trading::TtcError::Api {
                    code: 500,
                    message: "boom".into(),
                }))
            }
        };

        let refresh_for_r = refresh_called.clone();
        let refresh = || {
            let c = refresh_for_r.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };

        let result = with_retry_on_auth(f, refresh).await;
        assert!(result.is_err());
        assert_eq!(attempt.load(Ordering::SeqCst), 1, "f called once");
        assert_eq!(refresh_called.load(Ordering::SeqCst), 0, "refresh never called");
    }
}
