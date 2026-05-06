//! Unattended agent — the daemon shape.
//!
//! Builds a single agent once, then runs it on a fixed interval inside the
//! LoopRunner. Each cycle is wrapped in `with_auth_refresh`, so a 24h
//! TTC token expiration triggers `skill-trading login` and a single retry
//! instead of crashing the loop.
//!
//! No external scheduler. No supervisor. Ctrl-C exits cleanly.
//!
//! Run with:
//!   cargo run --example ttc_unattended -p swarms-tetrac
//!
//! Tunable via env (all optional):
//!   TICK_INTERVAL_SECS   default 60
//!   MAX_TICKS            default 2 (set to "0" or unset for forever)
//!   SKILL_TRADING_BIN    default <repo path>; needed for refresh_auth

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::agent::Agent;
use swarms_tetrac::tools::{GetFundingRatesTool, GetScannerTool};
use swarms_tetrac::{LoopRunner, TtcConfig, refresh_if_stale, with_auth_refresh};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();
    swarms_tetrac::install(&TtcConfig::from_env()?)?;

    let base_url = env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL not set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let interval_secs: u64 = env::var("TICK_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let max_ticks: u64 = env::var("MAX_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let client = OpenAI::from_url(base_url, api_key).set_model(&model);
    let agent = client
        .agent_builder()
        .agent_name("TtcUnattended")
        .system_prompt(
            "You are a TTC daemon agent. On each invocation, call \
             get_scanner with symbol=\"BTCUSDT\" timeframe=\"1h\", then \
             get_funding_rates with symbol=\"BTCUSDT\". Output ONE LINE \
             of the form: \"cycle <n>: signal=<dir> conf=<conf> funding_extremes=<ex>\". \
             End with <DONE>.",
        )
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .max_loops(4)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .verbose(false)
        .build();

    eprintln!(
        "ttc_unattended: interval={}s max_ticks={} (Ctrl-C to stop)",
        interval_secs,
        if max_ticks == 0 { "∞".into() } else { max_ticks.to_string() }
    );

    let runner = LoopRunner::every(Duration::from_secs(interval_secs))
        .max_ticks(if max_ticks == 0 { u64::MAX } else { max_ticks });

    // Proactive auth refresh: any token older than 23h gets refreshed at the
    // start of the cycle, before we even talk to the LLM. Belt; the per-tool
    // `with_auth_refresh` is the suspenders for clock skew / unexpected 401s.
    let max_token_age = Duration::from_secs(23 * 3600);

    runner
        .run(|cycle| {
            let agent = &agent;
            async move {
                if let Err(e) = refresh_if_stale(max_token_age).await {
                    tracing::warn!(error = %e, "proactive refresh failed; continuing with current token");
                }
                let prompt = format!(
                    "Run a fresh scan for cycle {cycle}. Use the prescribed format."
                );
                let summary = with_auth_refresh(|| async {
                    agent.run(prompt.clone()).await.map_err(|e| {
                        swarms_tetrac::TtcToolError::InvalidArg(format!("agent error: {e:?}"))
                    })
                })
                .await?;
                let last = summary.lines().last().unwrap_or("(empty)");
                tracing::info!(cycle, signal = %last, "cycle done");
                Ok(())
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("loop runner failed: {e}"))?;

    eprintln!("ttc_unattended: done");
    Ok(())
}
