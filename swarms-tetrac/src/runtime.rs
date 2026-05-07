//! Runtime helpers for unattended operation: a fixed-interval loop runner,
//! a subprocess-based auth-refresh helper, and a retry-on-401 wrapper.
//!
//! `swarms-rs` itself has no built-in scheduler (see the `// TODO: Loop
//! interval` in `swarms_agent.rs`). This module fills that gap.

use std::env;
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::signal;
use tokio::time::{MissedTickBehavior, interval};

use crate::TtcConfig;
use crate::error::TtcToolError;

const DEFAULT_SKILL_TRADING_BIN: &str =
    "/Users/mac/Documents/rust-cli-ttc-api/.claude/skills/skill-trading/scripts/skill-trading";

/// What a single cycle decided to do. Returned by the closure passed to
/// [`LoopRunner::run_with_outcome`] so the runner can drive cooldowns,
/// rate-limit backoff, and structured logs.
///
/// `Empty` covers any "agent gave up" case — exhausted LLM retries, missing
/// stop word, or a model timeout. From outside we can't tell those apart,
/// so they share one bucket and one backoff.
#[derive(Debug, Clone, PartialEq)]
pub enum CycleOutcome {
    Trade {
        exchange: String,
        symbol: String,
        side: String,
        qty: f64,
        dry_run: bool,
    },
    Skip {
        reason: String,
    },
    Empty,
}

impl fmt::Display for CycleOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CycleOutcome::Trade {
                exchange,
                symbol,
                side,
                qty,
                dry_run,
            } => write!(
                f,
                "trade exchange={exchange} symbol={symbol} side={side} qty={qty} dry_run={dry_run}"
            ),
            CycleOutcome::Skip { reason } => write!(f, "skip reason={reason}"),
            CycleOutcome::Empty => write!(f, "empty"),
        }
    }
}

#[derive(Debug, Default)]
struct CycleState {
    last_trade_at: Option<Instant>,
}

impl CycleState {
    fn record(&mut self, outcome: &CycleOutcome) {
        if matches!(outcome, CycleOutcome::Trade { .. }) {
            self.last_trade_at = Some(Instant::now());
        }
    }

    fn cooldown_remaining(&self, window: Option<Duration>) -> Option<Duration> {
        let window = window?;
        let last = self.last_trade_at?;
        let elapsed = last.elapsed();
        (elapsed < window).then(|| window - elapsed)
    }
}

/// Runs an async closure on a fixed interval until cancelled.
///
/// On each tick the closure is invoked. If it errors, the runner logs the
/// error, sleeps `failure_backoff`, and continues. Catches Ctrl-C for
/// graceful shutdown.
///
/// Use [`Self::run`] for fire-and-forget closures returning `Result<(), _>`.
/// Use [`Self::run_with_outcome`] when the closure returns a [`CycleOutcome`]
/// so the runner can apply post-trade cooldown and rate-limit-aware backoff
/// on `Empty`.
pub struct LoopRunner {
    pub interval: Duration,
    pub max_ticks: Option<u64>,
    pub failure_backoff: Duration,
    /// After a `CycleOutcome::Trade`, ticks landing inside this window
    /// are skipped (closure is not invoked). `None` disables cooldown.
    pub cooldown_after_trade: Option<Duration>,
    /// Sleep this long after a `CycleOutcome::Empty`, which we treat as
    /// "agent gave up" / rate-limited. Independent of `failure_backoff`,
    /// which only fires on hard task errors.
    pub rate_limit_backoff: Duration,
}

impl Default for LoopRunner {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            max_ticks: None,
            failure_backoff: Duration::from_secs(60),
            cooldown_after_trade: None,
            rate_limit_backoff: Duration::from_secs(60),
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

    pub fn cooldown_after_trade(mut self, dur: Duration) -> Self {
        self.cooldown_after_trade = Some(dur);
        self
    }

    pub fn rate_limit_backoff(mut self, dur: Duration) -> Self {
        self.rate_limit_backoff = dur;
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

    /// Like [`Self::run`] but the closure returns a [`CycleOutcome`].
    /// The runner uses the outcome to gate later ticks (cooldown after
    /// trade) and to back off on rate-limit-shaped failures.
    pub async fn run_with_outcome<F, Fut>(self, mut task: F) -> Result<(), TtcToolError>
    where
        F: FnMut(u64) -> Fut,
        Fut: std::future::Future<Output = Result<CycleOutcome, TtcToolError>>,
    {
        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let limit = self.max_ticks.unwrap_or(u64::MAX);
        let mut tick: u64 = 0;
        let mut state = CycleState::default();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if tick >= limit {
                        tracing::info!(ticks_completed = tick, "loop runner reached max_ticks");
                        return Ok(());
                    }
                    let cycle = tick;
                    tick += 1;

                    if let Some(rem) = state.cooldown_remaining(self.cooldown_after_trade) {
                        tracing::info!(cycle, remaining_ms = rem.as_millis() as u64, "cycle: cooldown, skipping");
                        continue;
                    }

                    match task(cycle).await {
                        Ok(outcome) => {
                            tracing::info!(cycle, outcome = %outcome, "cycle outcome");
                            if matches!(outcome, CycleOutcome::Empty) {
                                tracing::warn!(cycle, "agent returned empty; rate-limit backoff");
                                tokio::time::sleep(self.rate_limit_backoff).await;
                            }
                            state.record(&outcome);
                        }
                        Err(e) => {
                            tracing::error!(cycle, error = %e, "task failed; backing off");
                            tokio::time::sleep(self.failure_backoff).await;
                        }
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

/// Read `TTC_TOKEN_ISSUED_AT` (unix seconds, written by `skill-trading
/// register/login`); if the token is older than `max_age`, run
/// [`refresh_auth`]. Returns `Ok(true)` if a refresh fired.
///
/// No-op (returns `Ok(false)`) when the env var is missing or unparseable —
/// we don't want a missing var to subprocess `skill-trading login` on every
/// tick. Pair this with [`with_auth_refresh`] for belt-and-suspenders: the
/// timer keeps the token fresh proactively, the wrapper handles the rare
/// case where the timer was wrong (clock skew, server-side revocation).
/// Pure helper: returns `Some(age_secs)` when the token's age exceeds
/// `max_age`. `None` if the env var is missing/unparseable, the token is
/// fresh, or the system clock is broken. Doesn't subprocess anything.
pub fn token_age_if_stale(max_age: Duration) -> Option<u64> {
    let issued_at: u64 = env::var("TTC_TOKEN_ISSUED_AT")
        .ok()
        .and_then(|s| s.parse().ok())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let age = now.saturating_sub(issued_at);
    (age >= max_age.as_secs()).then_some(age)
}

pub async fn refresh_if_stale(max_age: Duration) -> Result<bool, TtcToolError> {
    if let Some(age) = token_age_if_stale(max_age) {
        tracing::info!(age_secs = age, "token stale; refreshing");
        refresh_auth().await?;
        Ok(true)
    } else {
        Ok(false)
    }
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

    #[test]
    fn token_age_if_stale_returns_age_when_old() {
        let _g = ENV_LOCK.lock().unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        // SAFETY: serialized via ENV_LOCK.
        unsafe {
            std::env::set_var("TTC_TOKEN_ISSUED_AT", (now - 100_000).to_string());
        }
        let result = token_age_if_stale(Duration::from_secs(60_000));
        assert!(matches!(result, Some(age) if age >= 100_000));
        unsafe { std::env::remove_var("TTC_TOKEN_ISSUED_AT") };
    }

    #[test]
    fn token_age_if_stale_returns_none_when_fresh() {
        let _g = ENV_LOCK.lock().unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        unsafe {
            std::env::set_var("TTC_TOKEN_ISSUED_AT", (now - 60).to_string());
        }
        let result = token_age_if_stale(Duration::from_secs(3600));
        assert!(result.is_none());
        unsafe { std::env::remove_var("TTC_TOKEN_ISSUED_AT") };
    }

    #[test]
    fn token_age_if_stale_handles_missing_env() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("TTC_TOKEN_ISSUED_AT") };
        assert!(token_age_if_stale(Duration::from_secs(1)).is_none());
    }

    #[test]
    fn token_age_if_stale_handles_garbage() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("TTC_TOKEN_ISSUED_AT", "not-a-number") };
        assert!(token_age_if_stale(Duration::from_secs(1)).is_none());
        unsafe { std::env::remove_var("TTC_TOKEN_ISSUED_AT") };
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn dummy_trade() -> CycleOutcome {
        CycleOutcome::Trade {
            exchange: "phemex".into(),
            symbol: "BTCUSDT".into(),
            side: "short".into(),
            qty: 0.001,
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn run_with_outcome_calls_task_max_ticks_times_when_no_cooldown() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(10))
            .max_ticks(3)
            .rate_limit_backoff(Duration::from_millis(0))
            .run_with_outcome(move |_cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, TtcToolError>(CycleOutcome::Skip {
                        reason: "noop".into(),
                    })
                }
            })
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_with_outcome_skips_ticks_during_cooldown() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(10))
            .max_ticks(5)
            .cooldown_after_trade(Duration::from_secs(10))
            .rate_limit_backoff(Duration::from_millis(0))
            .failure_backoff(Duration::from_millis(0))
            .run_with_outcome(move |_cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, TtcToolError>(dummy_trade())
                }
            })
            .await
            .unwrap();

        // First tick: closure runs, returns Trade, locks cooldown for 10s.
        // Remaining 4 ticks land inside the cooldown window → skipped.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_with_outcome_skip_does_not_arm_cooldown() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(10))
            .max_ticks(3)
            .cooldown_after_trade(Duration::from_secs(10))
            .rate_limit_backoff(Duration::from_millis(0))
            .run_with_outcome(move |_cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, TtcToolError>(CycleOutcome::Skip {
                        reason: "neutral".into(),
                    })
                }
            })
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_with_outcome_handles_empty_without_panicking() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_for_task = counter.clone();

        LoopRunner::every(Duration::from_millis(5))
            .max_ticks(3)
            .rate_limit_backoff(Duration::from_millis(1))
            .run_with_outcome(move |_cycle| {
                let c = counter_for_task.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, TtcToolError>(CycleOutcome::Empty)
                }
            })
            .await
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cycle_outcome_display_is_logfmt() {
        let t = dummy_trade();
        assert_eq!(
            t.to_string(),
            "trade exchange=phemex symbol=BTCUSDT side=short qty=0.001 dry_run=true"
        );
        let s = CycleOutcome::Skip {
            reason: "neutral".into(),
        };
        assert_eq!(s.to_string(), "skip reason=neutral");
        assert_eq!(CycleOutcome::Empty.to_string(), "empty");
    }

    #[test]
    fn cycle_state_cooldown_remaining_window_disabled() {
        let mut state = CycleState::default();
        state.record(&dummy_trade());
        assert!(state.cooldown_remaining(None).is_none());
    }

    #[test]
    fn cycle_state_cooldown_remaining_no_trade_yet() {
        let state = CycleState::default();
        assert!(
            state
                .cooldown_remaining(Some(Duration::from_secs(60)))
                .is_none()
        );
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
