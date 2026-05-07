//! Scan-and-trade daemon — three-agent pipeline on a LoopRunner tick.
//!
//! Each cycle runs Signal → Risk → Executor sequentially, with Rust-side
//! short-circuits between stages (neutral / low-confidence / risk-SKIP)
//! to skip later stages and save LLM calls on cheap rejections.
//!
//! Per-call context stays bounded (each agent only sees its own role's
//! input), unlike a single-agent-with-many-tools shape where the
//! conversation grows with each tool result.
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
    GetFundingRatesTool, GetPositionsTool, GetScannerTool, GetUsdBalanceTool,
    PlaceMarketOrderTool,
};
use swarms_tetrac::{
    CycleOutcome, LoopRunner, TtcConfig, TtcToolError, refresh_if_stale, with_auth_refresh,
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

    if model.contains(":free") {
        eprintln!(
            "ttc_scan_and_trade: WARNING — LLM_MODEL={model} ends with ':free'. \
             Free OpenRouter routes are rate-limited and often have flaky tool calling. \
             If cycles return Empty, switch to a paid route or DeepSeek (deepseek-chat)."
        );
    }

    let client = OpenAI::from_url(base_url, api_key).set_model(&model);

    let signal_watch = client
        .agent_builder()
        .agent_name("SignalWatch")
        .system_prompt(format!(
            "You are a TTC market signal watcher.\n\
             Tools: get_scanner, get_funding_rates.\n\
             \n\
             Steps (each tool EXACTLY ONCE):\n\
             1. get_scanner symbol=\"{symbol}\" timeframe=\"1h\".\n\
             2. get_funding_rates symbol=\"{symbol}\".\n\
             3. Output ONE LINE:\n\
                \"signal direction=<long|short|neutral> confidence=<low|medium|high> \
                entry=<price|n/a> funding_bias=<long|short|mixed|neutral>\"\n\
             4. End with <DONE>.\n\
             \n\
             Field rules:\n\
             - direction = scanner signal.direction (lowercase)\n\
             - confidence = scanner signal.confidence (lowercase)\n\
             - entry = scanner signal.entry, or n/a if missing\n\
             - funding_bias: long if rates mostly positive, short if mostly negative, \
               mixed if split, neutral if no data\n\
             - If get_scanner errors, output direction=neutral confidence=low entry=n/a \
               funding_bias=neutral and stop."
        ))
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .max_loops(4)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .verbose(false)
        .build();

    let risk_check = client
        .agent_builder()
        .agent_name("RiskCheck")
        .system_prompt(format!(
            "You are a TTC trade risk checker for exchange=\"{exchange}\".\n\
             Tools: get_usd_balance, get_positions.\n\
             \n\
             You receive the upstream signal line as input. Then:\n\
             1. get_usd_balance exchange=\"{exchange}\" EXACTLY ONCE. \
                Returns the largest USD-stablecoin (USDT/USDC/DUSD/etc.) and its \
                available amount — read \"asset\" and \"available\" from the result.\n\
             2. get_positions exchange=\"{exchange}\" EXACTLY ONCE. \
                If errors (deserialization or 5xx), treat positions as unknown — \
                do NOT abort.\n\
             3. Output ONE LINE:\n\
                \"risk verdict=<PROCEED|SKIP> usd_asset=<symbol> usd_available=<amount|unknown> \
                existing_position=<long|short|none|unknown> reason=<single-word-or-hyphenated>\"\n\
             4. End with <DONE>.\n\
             \n\
             Decision rules (in order):\n\
             - SKIP if signal direction is neutral. reason=neutral\n\
             - SKIP if signal confidence is low. reason=low-confidence\n\
             - SKIP if get_usd_balance errors or available is 0. reason=no-stablecoin\n\
             - Otherwise PROCEED. reason=signal-confirmed\n\
             \n\
             NOTE: existing_position is reported for observability only. A separate \
             deterministic Rust guard between this stage and the executor handles \
             position-conflict skipping — do NOT skip on existing_position yourself.\n\
             \n\
             Reason MUST be a single word or hyphenated tokens — no spaces."
        ))
        .add_tool(GetUsdBalanceTool)
        .add_tool(GetPositionsTool)
        .max_loops(4)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .verbose(false)
        .build();

    let executor = client
        .agent_builder()
        .agent_name("Executor")
        .system_prompt(format!(
            "You are a TTC trade executor for exchange=\"{exchange}\" symbol=\"{symbol}\".\n\
             Tools: place_market_order.\n\
             \n\
             You receive upstream signal and risk lines as input.\n\
             \n\
             - If risk verdict is SKIP: do NOT call any tool. Output the cycle line \
               with action=SKIP and reason copied from the upstream risk reason. \
               End with <DONE>.\n\
             - If risk verdict is PROCEED:\n\
               1. Compute qty = ({usd_pct}% of usd_available) / entry. Round sensibly: \
                  BTC/ETH-priced symbols use 4 decimals; mid-priced use 2; sub-$1 use 0. \
                  If qty rounds to 0, output action=SKIP reason=qty-too-small (no tool call).\n\
               2. side = signal direction (long or short).\n\
               3. place_market_order exchange=\"{exchange}\" symbol=\"{symbol}\" \
                  side=<side> quantity=<qty> EXACTLY ONCE.\n\
               4. Output the cycle line with action=TRADE.\n\
             \n\
             Cycle line format (always exactly):\n\
             \"cycle <n>: action=<TRADE|SKIP> exchange={exchange} symbol={symbol} \
             side=<long|short|n/a> qty=<q|n/a> reason=<single-word-or-hyphenated> \
             dry_run=<true|false>\"\n\
             \n\
             reason MUST be a single word or hyphenated tokens — no spaces, no quotes. \
             Examples: \"neutral\", \"low-confidence\", \"no-stablecoin\", \"signal-confirmed\", \"qty-too-small\".\n\
             \n\
             End with <DONE>.\n\
             \n\
             Notes:\n\
             - <n> = cycle number from the user message (\"cycle 0\", \"cycle 1\", ...).\n\
             - place_market_order may return {{\"dry_run\": true, ...}} — report dry_run=true.\n\
             - Never call place_market_order more than once."
        ))
        .add_tool(PlaceMarketOrderTool)
        .max_loops(3)
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
        "ttc_scan_and_trade: 3-agent pipeline | interval={interval_secs}s max_ticks={} \
         exchange={exchange} symbol={symbol} usd_pct={usd_pct} cooldown={cooldown_secs}s \
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
            let signal_watch = &signal_watch;
            let risk_check = &risk_check;
            let executor = &executor;
            let exchange = exchange.clone();
            let symbol = symbol.clone();
            async move {
                if let Err(e) = refresh_if_stale(max_token_age).await {
                    tracing::warn!(error = %e, "proactive refresh failed; continuing");
                }

                let signal_prompt = format!("cycle {cycle}: scan {symbol}");
                let signal = with_auth_refresh(|| async {
                    signal_watch.run(signal_prompt.clone()).await.map_err(|e| {
                        TtcToolError::InvalidArg(format!("signal_watch: {e:?}"))
                    })
                })
                .await?;
                if !signal.contains("<DONE>") {
                    tracing::warn!(stage = "signal", "agent returned without <DONE>");
                    return Ok(CycleOutcome::Empty);
                }
                if signal.contains("direction=neutral") {
                    return Ok(CycleOutcome::Skip {
                        reason: "neutral".into(),
                    });
                }
                if signal.contains("confidence=low") {
                    return Ok(CycleOutcome::Skip {
                        reason: "low-confidence".into(),
                    });
                }

                let risk_prompt = format!("cycle {cycle}\n\n[signal]:\n{signal}");
                let risk = with_auth_refresh(|| async {
                    risk_check.run(risk_prompt.clone()).await.map_err(|e| {
                        TtcToolError::InvalidArg(format!("risk_check: {e:?}"))
                    })
                })
                .await?;
                if !risk.contains("<DONE>") {
                    tracing::warn!(stage = "risk", "agent returned without <DONE>");
                    return Ok(CycleOutcome::Empty);
                }
                if risk.contains("verdict=SKIP") {
                    let reason = extract_reason(&risk).unwrap_or_else(|| "risk-skip".into());
                    return Ok(CycleOutcome::Skip { reason });
                }

                // Deterministic position-conflict guard. The agent's
                // existing_position field is informational only; this is the
                // load-bearing check. Conservative default: any open position
                // on the target symbol blocks a new trade. In Phemex's merged
                // position mode a sell-while-long would reduce/flip the long
                // (paying fees, losing exposure), and a buy-while-long would
                // average up (potentially over-leveraging). Both are surprises
                // we don't want unattended.
                match check_position_conflict(&exchange, &symbol).await {
                    Ok(Some(dir)) => {
                        return Ok(CycleOutcome::Skip {
                            reason: format!("already-positioned-{dir}"),
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "position conflict check failed; skipping cycle");
                        return Ok(CycleOutcome::Skip {
                            reason: "position-check-failed".into(),
                        });
                    }
                }

                let exec_prompt = format!(
                    "cycle {cycle}\n\n[signal]:\n{signal}\n\n[risk]:\n{risk}"
                );
                let exec_output = with_auth_refresh(|| async {
                    executor.run(exec_prompt.clone()).await.map_err(|e| {
                        TtcToolError::InvalidArg(format!("executor: {e:?}"))
                    })
                })
                .await?;
                Ok(parse_outcome(&exec_output, &exchange, &symbol))
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("loop runner failed: {e}"))?;

    eprintln!("ttc_scan_and_trade: done");
    Ok(())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Pure logic for conflict detection — picks the first non-empty position
/// on `symbol` and returns its direction ("long"/"short"). Returns `None`
/// if no conflicting position exists.
fn detect_position_conflict(
    positions: &[skill_trading::models::Position],
    symbol: &str,
) -> Option<String> {
    for pos in positions {
        if pos.symbol == symbol && pos.size > 0.0 {
            let dir = match pos.side.as_str() {
                "buy" => "long",
                "sell" => "short",
                other => other,
            };
            return Some(dir.to_string());
        }
    }
    None
}

/// Async wrapper: fetches positions via the runtime client and runs
/// `detect_position_conflict`. Errors propagate to the caller, which
/// treats them as Skip-with-reason rather than Proceed.
async fn check_position_conflict(
    exchange: &str,
    symbol: &str,
) -> Result<Option<String>, TtcToolError> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let positions = rt
        .client
        .get_positions(exchange, Some(symbol), creds)
        .await
        .map_err(TtcToolError::Api)?;
    Ok(detect_position_conflict(&positions, symbol))
}

/// Pull `reason=...` out of the upstream risk line so we can report it on
/// the cycle's Skip outcome instead of a generic "risk-skip".
fn extract_reason(text: &str) -> Option<String> {
    let line = text.lines().rfind(|l| l.contains("verdict="))?;
    kv(line, "reason")
}

/// Turn the executor's last "cycle <n>: action=... ..." line into a
/// `CycleOutcome`. Treats missing / malformed output as `Empty` so the
/// runner backs off.
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

/// Extract `key=value` from a logfmt-ish line. Stops the value at whitespace,
/// so multi-word reasons get truncated to their first word — fine for the
/// outcome-summary log line; full text stays in the agent transcript.
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
        assert_eq!(parse_outcome(s, "orderly", "BTC"), CycleOutcome::Empty);
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

    #[test]
    fn extract_reason_pulls_from_risk_line() {
        let s = "cycle 0\n\nrisk verdict=SKIP usdc_available=200 \
                 existing_position=none reason=neutral\n<DONE>";
        assert_eq!(extract_reason(s).as_deref(), Some("neutral"));
    }

    #[test]
    fn extract_reason_returns_none_when_no_verdict_line() {
        assert!(extract_reason("nothing here").is_none());
    }

    fn pos(symbol: &str, side: &str, size: f64) -> skill_trading::models::Position {
        skill_trading::models::Position {
            symbol: symbol.into(),
            side: side.into(),
            position_side: "merged".into(),
            size,
            entry_price: 100.0,
            mark_price: 100.0,
            pnl: None,
            leverage: -10,
            liquidation_price: None,
            margin_type: Some("cross".into()),
            unrealized_pnl: None,
            notional: None,
        }
    }

    #[test]
    fn detect_conflict_returns_none_for_empty_positions() {
        assert!(detect_position_conflict(&[], "BTCUSDT").is_none());
    }

    #[test]
    fn detect_conflict_returns_none_when_no_match_on_target_symbol() {
        let positions = vec![pos("ETHUSDT", "buy", 1.0), pos("SOLUSDT", "sell", 5.0)];
        assert!(detect_position_conflict(&positions, "BTCUSDT").is_none());
    }

    #[test]
    fn detect_conflict_returns_long_for_buy_side() {
        let positions = vec![pos("ZROUSDT", "buy", 133.0)];
        assert_eq!(
            detect_position_conflict(&positions, "ZROUSDT").as_deref(),
            Some("long")
        );
    }

    #[test]
    fn detect_conflict_returns_short_for_sell_side() {
        let positions = vec![pos("BTCUSDT", "sell", 0.001)];
        assert_eq!(
            detect_position_conflict(&positions, "BTCUSDT").as_deref(),
            Some("short")
        );
    }

    #[test]
    fn detect_conflict_skips_zero_size_positions() {
        // A zero-size entry might appear briefly after a close; treat as no conflict.
        let positions = vec![pos("BTCUSDT", "buy", 0.0)];
        assert!(detect_position_conflict(&positions, "BTCUSDT").is_none());
    }
}
