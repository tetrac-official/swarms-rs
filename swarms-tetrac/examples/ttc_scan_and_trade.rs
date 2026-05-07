//! Scan-and-trade daemon — one agent, five tools, on a LoopRunner tick.
//!
//! Each cycle the agent reads the scanner + funding + balance + positions,
//! then either places a sized market order or skips. The runner reads the
//! agent's last line into a `CycleOutcome` so it can:
//!   - cool off after a successful trade (no back-to-back blasting)
//!   - back off on `Empty` (LLM rate-limited or output garbage)
//!
//! Dry-run by default; flip `TTC_DRY_RUN=false` only after watching dry
//! envelopes look sane.
//!
//! Run with:
//!   cargo run --example ttc_scan_and_trade -p swarms-tetrac
//!
//! Tunable via env (all optional):
//!   TICK_INTERVAL_SECS   default 60
//!   MAX_TICKS            default 2 (set "0" or unset for forever)
//!   TRADE_EXCHANGE       default "orderly"
//!   TRADE_SYMBOL         default "BTCUSDT"
//!   TRADE_USD_PCT        default "5"   (percent of free USDC per trade)
//!   COOLDOWN_SECS        default 300   (skip ticks for N secs after a trade)
//!   RATE_LIMIT_BACKOFF_SECS default 60 (sleep after agent returns empty)
//!   SKILL_TRADING_BIN    default <repo path>; needed for refresh_auth

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::agent::Agent;
use swarms_tetrac::tools::{
    GetBalanceTool, GetFundingRatesTool, GetPositionsTool, GetScannerTool, PlaceMarketOrderTool,
};
use swarms_tetrac::{
    CycleOutcome, LoopRunner, TtcConfig, refresh_if_stale, with_auth_refresh,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    let dry = cfg.dry_run;
    swarms_tetrac::install(&cfg)?;

    let interval_secs: u64 = env_u64("TICK_INTERVAL_SECS", 60);
    let max_ticks: u64 = env_u64("MAX_TICKS", 2);
    let cooldown_secs: u64 = env_u64("COOLDOWN_SECS", 300);
    let rate_limit_backoff_secs: u64 = env_u64("RATE_LIMIT_BACKOFF_SECS", 60);
    let exchange = env::var("TRADE_EXCHANGE").unwrap_or_else(|_| "orderly".into());
    let symbol = env::var("TRADE_SYMBOL").unwrap_or_else(|_| "BTCUSDT".into());
    let usd_pct: f64 = env::var("TRADE_USD_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);

    let base_url = env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL not set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let client = OpenAI::from_url(base_url, api_key).set_model(&model);

    let system_prompt = format!(
        "You are a TTC scan-and-trade daemon. Each invocation, perform the steps below \
         EXACTLY ONCE EACH, in order:\n\
         \n\
         1. get_scanner with symbol=\"{symbol}\" timeframe=\"1h\". Read direction \
            (long/short/neutral) and confidence.\n\
         2. get_funding_rates with symbol=\"{symbol}\". Note funding bias.\n\
         3. get_balance with exchange=\"{exchange}\". Read available USDC.\n\
         4. get_positions with exchange=\"{exchange}\". Check existing position. If this \
            tool errors with a deserialization or 5xx error, treat positions as unknown \
            and continue — do NOT abort the cycle.\n\
         5. DECIDE:\n\
            - SKIP if scanner direction is neutral.\n\
            - SKIP if scanner confidence is LOW or missing.\n\
            - SKIP if get_balance errored.\n\
            - SKIP if an existing position on {symbol} is already in the signal direction.\n\
            - Otherwise PROCEED: side = signal direction (long or short); \
              quantity = ({usd_pct}% of available USDC) / current price. Use the scanner's \
              entry/last price; if absent, use mid of any returned bid/ask. Round qty \
              to a sensible precision for the symbol. If qty rounds to 0, SKIP.\n\
         6. If PROCEED, call place_market_order EXACTLY ONCE with exchange=\"{exchange}\" \
            symbol=\"{symbol}\" side=<long|short> quantity=<qty>. NEVER call it more than \
            once per cycle, regardless of the response shape.\n\
         7. Output ONE LINE in this EXACT format (no extra text), then end with <DONE>:\n\
            \"cycle <n>: action=<TRADE|SKIP> exchange={exchange} symbol={symbol} \
            side=<long|short|n/a> qty=<q|n/a> reason=<short> dry_run=<true|false>\"\n\
         \n\
         Notes:\n\
         - Never invent numbers. Only use what tools return.\n\
         - place_market_order may return a dry-run envelope of the form \
           {{\"dry_run\": true, ...}}. Treat that as a successful trade and report \
           dry_run=true.\n\
         - When live, the response has the real order shape with order_id, status."
    );

    let agent = client
        .agent_builder()
        .agent_name("TtcScanAndTrade")
        .system_prompt(system_prompt)
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .add_tool(GetBalanceTool)
        .add_tool(GetPositionsTool)
        .add_tool(PlaceMarketOrderTool)
        .max_loops(8)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .verbose(false)
        .build();

    eprintln!(
        "ttc_scan_and_trade: {}",
        if dry {
            "DRY-RUN (TTC_DRY_RUN=true). No real orders will fire."
        } else {
            "LIVE (TTC_DRY_RUN=false). Orders will hit the exchange."
        }
    );
    eprintln!(
        "ttc_scan_and_trade: interval={interval_secs}s max_ticks={} exchange={exchange} \
         symbol={symbol} usd_pct={usd_pct} cooldown={cooldown_secs}s \
         rate_limit_backoff={rate_limit_backoff_secs}s",
        if max_ticks == 0 {
            "∞".into()
        } else {
            max_ticks.to_string()
        }
    );

    let runner = LoopRunner::every(Duration::from_secs(interval_secs))
        .max_ticks(if max_ticks == 0 { u64::MAX } else { max_ticks })
        .cooldown_after_trade(Duration::from_secs(cooldown_secs))
        .rate_limit_backoff(Duration::from_secs(rate_limit_backoff_secs));

    let max_token_age = Duration::from_secs(23 * 3600);

    runner
        .run_with_outcome(|cycle| {
            let agent = &agent;
            let exchange = exchange.clone();
            let symbol = symbol.clone();
            async move {
                if let Err(e) = refresh_if_stale(max_token_age).await {
                    tracing::warn!(error = %e, "proactive refresh failed; continuing with current token");
                }
                let prompt = format!("Run cycle {cycle}. Follow the prescribed steps.");
                let summary = with_auth_refresh(|| async {
                    agent.run(prompt.clone()).await.map_err(|e| {
                        swarms_tetrac::TtcToolError::InvalidArg(format!("agent error: {e:?}"))
                    })
                })
                .await?;
                Ok(parse_outcome(&summary, &exchange, &symbol))
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("loop runner failed: {e}"))?;

    eprintln!("ttc_scan_and_trade: done");
    Ok(())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Turn the agent's last "cycle <n>: action=... ..." line into a `CycleOutcome`.
/// Treats missing / malformed output as `Empty` so the runner backs off.
fn parse_outcome(summary: &str, default_exchange: &str, default_symbol: &str) -> CycleOutcome {
    if !summary.contains("<DONE>") {
        return CycleOutcome::Empty;
    }
    let Some(line) = summary
        .lines()
        .rfind(|l| l.contains("action=") && l.contains("cycle"))
    else {
        return CycleOutcome::Empty;
    };

    let action = kv(line, "action").unwrap_or_default();
    let reason = kv(line, "reason").unwrap_or_else(|| "(unknown)".into());
    let dry_run = kv(line, "dry_run")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if action.eq_ignore_ascii_case("trade") {
        let side = kv(line, "side").unwrap_or_default();
        let qty: f64 = kv(line, "qty").and_then(|v| v.parse().ok()).unwrap_or(0.0);
        if qty <= 0.0 || side == "n/a" || side.is_empty() {
            return CycleOutcome::Skip {
                reason: format!("invalid trade line: {reason}"),
            };
        }
        CycleOutcome::Trade {
            exchange: kv(line, "exchange").unwrap_or_else(|| default_exchange.into()),
            symbol: kv(line, "symbol").unwrap_or_else(|| default_symbol.into()),
            side,
            qty,
            dry_run,
        }
    } else {
        CycleOutcome::Skip { reason }
    }
}

/// Extract a `key=value` token from a line. Stops the value at the next
/// whitespace, so multi-word reasons get truncated to their first word —
/// that's fine for log output, the structured fields keep precision.
fn kv(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outcome_trade_line() {
        let s = "cycle 0: action=TRADE exchange=phemex symbol=BTCUSDT side=short \
                 qty=0.001 reason=signal dry_run=true\n<DONE>";
        match parse_outcome(s, "orderly", "ETHUSDT") {
            CycleOutcome::Trade {
                exchange,
                symbol,
                side,
                qty,
                dry_run,
            } => {
                assert_eq!(exchange, "phemex");
                assert_eq!(symbol, "BTCUSDT");
                assert_eq!(side, "short");
                assert!((qty - 0.001).abs() < 1e-9);
                assert!(dry_run);
            }
            other => panic!("expected Trade, got {other:?}"),
        }
    }

    #[test]
    fn parse_outcome_skip_line() {
        let s = "cycle 1: action=SKIP exchange=phemex symbol=BTCUSDT side=n/a \
                 qty=n/a reason=neutral dry_run=true\n<DONE>";
        match parse_outcome(s, "orderly", "ETHUSDT") {
            CycleOutcome::Skip { reason } => assert_eq!(reason, "neutral"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn parse_outcome_no_done_is_empty() {
        let s = "cycle 0: action=TRADE side=long qty=1.0 dry_run=true";
        assert_eq!(
            parse_outcome(s, "orderly", "BTC"),
            CycleOutcome::Empty
        );
    }

    #[test]
    fn parse_outcome_blank_is_empty() {
        assert_eq!(parse_outcome("", "orderly", "BTC"), CycleOutcome::Empty);
    }

    #[test]
    fn parse_outcome_trade_with_zero_qty_falls_to_skip() {
        let s = "cycle 0: action=TRADE side=long qty=0 reason=rounded dry_run=true\n<DONE>";
        match parse_outcome(s, "orderly", "BTC") {
            CycleOutcome::Skip { reason } => assert!(reason.contains("rounded")),
            other => panic!("expected Skip, got {other:?}"),
        }
    }
}
