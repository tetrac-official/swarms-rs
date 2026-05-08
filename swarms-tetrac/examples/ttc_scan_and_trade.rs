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
//!   TRADE_RISK_PCT       default "1"   (percent of balance to lose if stop hits)
//!   COOLDOWN_SECS        default 300   (skip ticks for N secs after a trade)
//!   RATE_LIMIT_BACKOFF_SECS default 60 (sleep after agent returns empty)
//!   SKILL_TRADING_BIN    default <repo path>; needed for refresh_auth
//!
//! Sizing model (perp-aware, risk-based):
//!   loss_per_token = abs(entry - stop_loss)
//!   risk_amount    = balance × TRADE_RISK_PCT / 100
//!   qty            = risk_amount / loss_per_token
//!   notional       = qty × entry      (capped at 50× balance as a sanity bound)
//!
//! Leverage is whatever the exchange-side account is configured at — the
//! daemon does not call set_leverage. Risk math is leverage-independent;
//! leverage only changes margin requirements, not the size of the trade.

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
    let risk_pct: f64 = env::var("TRADE_RISK_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

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
                entry=<price|n/a> stop_loss=<price|n/a> \
                tp1=<price|n/a> tp2=<price|n/a> tp3=<price|n/a> \
                funding_bias=<long|short|mixed|neutral>\"\n\
             4. End with <DONE>.\n\
             \n\
             Field rules:\n\
             - direction = scanner signal.direction (lowercase)\n\
             - confidence = scanner signal.confidence (lowercase)\n\
             - entry = scanner signal.entry, or n/a if missing\n\
             - stop_loss = scanner signal.stopLoss, or n/a if missing\n\
             - tp1 = scanner signal.takeProfit1, or n/a if null/missing\n\
             - tp2 = scanner signal.takeProfit2, or n/a if null/missing\n\
             - tp3 = scanner signal.takeProfit3, or n/a if null/missing\n\
             - funding_bias: long if rates mostly positive, short if mostly negative, \
               mixed if split, neutral if no data\n\
             - If get_scanner errors, output direction=neutral confidence=low entry=n/a \
               stop_loss=n/a tp1=n/a tp2=n/a tp3=n/a funding_bias=neutral and stop."
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
             You receive a [trade] section with side and quantity precomputed by the \
             daemon. Place that exact order. Do NOT recompute, scale, round, or alter \
             the quantity — the daemon has already done risk-based sizing.\n\
             \n\
             Steps:\n\
             1. place_market_order exchange=\"{exchange}\" symbol=\"{symbol}\" \
                side=<side from [trade]> quantity=<quantity from [trade]> EXACTLY ONCE.\n\
             2. Output the cycle line with action=TRADE.\n\
             \n\
             Cycle line format (always exactly):\n\
             \"cycle <n>: action=TRADE exchange={exchange} symbol={symbol} \
             side=<long|short> qty=<quantity from [trade]> reason=signal-confirmed \
             dry_run=<true|false>\"\n\
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
         exchange={exchange} symbol={symbol} risk_pct={risk_pct}% cooldown={cooldown_secs}s \
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

                // Risk-based qty sizing in Rust, not the LLM. Pull entry,
                // stop_loss, direction from the signal line; usd_available
                // from the risk line. If anything is missing or the math
                // fails sanity bounds, skip with a specific reason instead
                // of feeding garbage to the executor.
                let direction = kv(&signal, "direction").unwrap_or_default();
                let entry = kv(&signal, "entry").and_then(|v| v.parse::<f64>().ok());
                let stop = kv(&signal, "stop_loss").and_then(|v| v.parse::<f64>().ok());
                let tp1 = kv(&signal, "tp1").and_then(|v| v.parse::<f64>().ok());
                let tp2 = kv(&signal, "tp2").and_then(|v| v.parse::<f64>().ok());
                let tp3 = kv(&signal, "tp3").and_then(|v| v.parse::<f64>().ok());
                let balance = kv(&risk, "usd_available").and_then(|v| v.parse::<f64>().ok());
                let (entry, stop, balance) = match (entry, stop, balance) {
                    (Some(e), Some(s), Some(b)) => (e, s, b),
                    _ => {
                        return Ok(CycleOutcome::Skip {
                            reason: "missing-sizing-input".into(),
                        });
                    }
                };
                let qty = match compute_risk_qty(&direction, entry, stop, balance, risk_pct) {
                    Ok(raw) => round_qty_for_price(raw, entry),
                    Err(reason) => {
                        return Ok(CycleOutcome::Skip {
                            reason: format!("sizing-{reason}"),
                        });
                    }
                };
                if qty <= 0.0 {
                    return Ok(CycleOutcome::Skip {
                        reason: "qty-rounds-to-zero".into(),
                    });
                }

                let exec_prompt = format!(
                    "cycle {cycle}\n\n\
                     [signal]:\n{signal}\n\n\
                     [risk]:\n{risk}\n\n\
                     [trade]:\nside={direction} quantity={qty}"
                );
                let exec_output = with_auth_refresh(|| async {
                    executor.run(exec_prompt.clone()).await.map_err(|e| {
                        TtcToolError::InvalidArg(format!("executor: {e:?}"))
                    })
                })
                .await?;
                let outcome = parse_outcome(&exec_output, &exchange, &symbol, dry);

                // Post-trade orders: stop-loss + layered take-profits, all
                // reduce_only. Best-effort — if any of these fail, we log and
                // return the Trade outcome anyway (the position is already
                // open; failing the cycle wouldn't roll it back). The user
                // sees the failure in the logs and can act manually.
                if matches!(outcome, CycleOutcome::Trade { .. }) {
                    let tps: Vec<f64> = [tp1, tp2, tp3].into_iter().flatten().collect();
                    place_post_trade_orders(
                        &exchange, &symbol, &direction, qty, stop, &tps, dry,
                    )
                    .await;
                }

                Ok(outcome)
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

/// Risk-based position size: how many tokens to buy/sell so that hitting
/// the stop-loss costs exactly `risk_pct` percent of `balance`.
///
///   loss_per_token = abs(entry - stop_loss)
///   risk_amount    = balance × risk_pct / 100
///   qty            = risk_amount / loss_per_token
///
/// Validates the stop is on the correct side of entry for the direction
/// (longs need stop below entry; shorts need stop above) and caps notional
/// at 50× balance — pros set their own risk, but if the math somehow asks
/// for a $15k position on a $300 account, something's wrong upstream and
/// we'd rather skip than place it.
fn compute_risk_qty(
    direction: &str,
    entry: f64,
    stop_loss: f64,
    balance: f64,
    risk_pct: f64,
) -> Result<f64, String> {
    if !entry.is_finite() || entry <= 0.0 {
        return Err("invalid-entry".into());
    }
    if !stop_loss.is_finite() || stop_loss <= 0.0 {
        return Err("invalid-stop".into());
    }
    if !balance.is_finite() || balance <= 0.0 {
        return Err("invalid-balance".into());
    }
    if !risk_pct.is_finite() || risk_pct <= 0.0 {
        return Err("invalid-risk-pct".into());
    }
    match direction {
        "long" if stop_loss >= entry => return Err("stop-not-below-entry".into()),
        "short" if stop_loss <= entry => return Err("stop-not-above-entry".into()),
        "long" | "short" => {}
        _ => return Err("invalid-direction".into()),
    }
    let distance = (entry - stop_loss).abs();
    if distance <= 0.0 {
        return Err("zero-distance".into());
    }
    let risk_amount = balance * risk_pct / 100.0;
    let qty = risk_amount / distance;
    let notional = qty * entry;
    let max_notional = balance * 50.0;
    if notional > max_notional {
        return Err("notional-exceeds-50x-balance".into());
    }
    Ok(qty)
}

/// Round a raw qty to a sensible precision based on the symbol's price.
/// High-priced (BTC/ETH-shaped) get 4 decimals; mid-priced 2; sub-$1 zero
/// (whole tokens). Uses a multiplier-then-round to avoid float-format hell.
fn round_qty_for_price(qty: f64, price: f64) -> f64 {
    let scale: f64 = if price > 100.0 {
        10_000.0
    } else if price >= 1.0 {
        100.0
    } else {
        1.0
    };
    (qty * scale).round() / scale
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
///
/// `daemon_dry_run` overrides any `dry_run=...` claim the agent makes:
/// the agent's output is unreliable (we've seen it print `dry_run=true`
/// while the daemon was live), but the daemon knows its own mode for sure.
fn parse_outcome(
    summary: &str,
    default_exchange: &str,
    default_symbol: &str,
    daemon_dry_run: bool,
) -> CycleOutcome {
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
            dry_run: daemon_dry_run,
        }
    } else {
        CycleOutcome::Skip { reason }
    }
}

/// Side of the closing/protecting order (opposite of the entry direction).
/// Long entries close with sells; shorts close with buys.
fn close_side_for(entry_direction: &str) -> Option<skill_trading::models::OrderSide> {
    match entry_direction {
        "long" => Some(skill_trading::models::OrderSide::Sell),
        "short" => Some(skill_trading::models::OrderSide::Buy),
        _ => None,
    }
}

/// Top-level coordinator for "after the entry order, set protections."
/// In live mode: poll until the position appears, then place stop + TPs
/// using the actual filled size. In dry mode: skip polling, use the
/// computed qty so we still exercise the order-placement wiring against
/// dry-run envelopes.
///
/// All errors are logged, never propagated — the entry order is already
/// open and we'd rather report a partial setup than crash the daemon.
async fn place_post_trade_orders(
    exchange: &str,
    symbol: &str,
    entry_direction: &str,
    intended_qty: f64,
    stop_price: f64,
    tp_prices: &[f64],
    dry: bool,
) {
    let Some(close_side) = close_side_for(entry_direction) else {
        tracing::error!(direction = entry_direction, "unknown direction; skipping post-trade orders");
        return;
    };

    let position_size = if dry {
        tracing::info!(
            symbol,
            qty = intended_qty,
            "dry-run: using computed qty for stop/TP sizing (skipping position poll)"
        );
        intended_qty
    } else {
        match poll_for_position(exchange, symbol, 20, 500).await {
            Ok(Some(pos)) => {
                tracing::info!(symbol, size = pos.size, "position confirmed; placing protections");
                pos.size
            }
            Ok(None) => {
                tracing::error!(
                    symbol,
                    "position never appeared after market order; SKIPPING stop/TP placement — set them manually"
                );
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "polling for position failed; skipping stop/TP placement");
                return;
            }
        }
    };

    if let Err(e) = place_stop_loss_order(exchange, symbol, close_side, position_size, stop_price).await {
        tracing::error!(
            error = format!("{e:#}"),
            stop_price,
            "stop-loss placement FAILED — set one manually"
        );
    }

    if tp_prices.is_empty() {
        tracing::info!("no take-profit levels in scanner output; skipping TP layer");
        return;
    }

    let chunks = split_qty_for_tps(position_size, tp_prices.len(), stop_price);
    for (tp_price, chunk_qty) in tp_prices.iter().zip(chunks.iter()) {
        if *chunk_qty <= 0.0 {
            tracing::warn!(tp_price, "tp chunk rounds to zero; skipping this level");
            continue;
        }
        if let Err(e) = place_take_profit_limit(
            exchange, symbol, close_side, *chunk_qty, *tp_price,
        )
        .await
        {
            tracing::error!(
                error = format!("{e:#}"),
                tp_price,
                "TP limit placement failed"
            );
        }
    }
}

/// Split a position size across N take-profit levels, rounding each chunk
/// to a sensible precision for the symbol's price tier. Returns a Vec the
/// same length as `n_tps`. The last chunk gets any remainder so the sum
/// matches the original size as closely as rounding allows.
fn split_qty_for_tps(total: f64, n_tps: usize, reference_price: f64) -> Vec<f64> {
    if n_tps == 0 || total <= 0.0 {
        return Vec::new();
    }
    let raw = total / n_tps as f64;
    let chunk = round_qty_for_price(raw, reference_price);
    let mut out = vec![chunk; n_tps];
    // Adjust the last chunk so chunks sum to total within rounding.
    let assigned = chunk * (n_tps - 1) as f64;
    let last = round_qty_for_price(total - assigned, reference_price);
    out[n_tps - 1] = last.max(0.0);
    out
}

async fn poll_for_position(
    exchange: &str,
    symbol: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<Option<skill_trading::models::Position>, TtcToolError> {
    let rt = swarms_tetrac::client::runtime()?;
    for attempt in 0..max_attempts {
        let creds = swarms_tetrac::client::credentials_for(exchange)?;
        let positions = rt
            .client
            .get_positions(exchange, Some(symbol), creds)
            .await
            .map_err(TtcToolError::Api)?;
        if let Some(pos) = positions.iter().find(|p| p.symbol == symbol && p.size > 0.0) {
            tracing::debug!(attempt = attempt + 1, "position found");
            return Ok(Some(pos.clone()));
        }
        if attempt + 1 < max_attempts {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Ok(None)
}

async fn place_stop_loss_order(
    exchange: &str,
    symbol: &str,
    close_side: skill_trading::models::OrderSide,
    quantity: f64,
    stop_price: f64,
) -> Result<(), TtcToolError> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = skill_trading::models::StopOrderParams {
        symbol: symbol.into(),
        side: close_side,
        quantity,
        stop_price,
        position_side: None,
        trigger_type: None,
        price: None,
        close_position: None,
        reduce_only: Some(true),
        client_order_id: None,
    };
    if rt.dry_run {
        tracing::info!(
            symbol, quantity, stop_price,
            "dry-run: would place stop-loss (reduce_only)"
        );
        return Ok(());
    }
    let order = rt
        .client
        .place_stop_order(exchange, params, creds)
        .await
        .map_err(TtcToolError::Api)?;
    tracing::info!(symbol, quantity, stop_price, ?order, "stop-loss placed");
    Ok(())
}

async fn place_take_profit_limit(
    exchange: &str,
    symbol: &str,
    close_side: skill_trading::models::OrderSide,
    quantity: f64,
    tp_price: f64,
) -> Result<(), TtcToolError> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = skill_trading::models::LimitOrderParams {
        symbol: symbol.into(),
        side: close_side,
        quantity,
        price: tp_price,
        position_side: None,
        time_in_force: Some(skill_trading::models::TimeInForce::GoodTillCancel),
        reduce_only: Some(true),
        take_profit_price: None,
        stop_loss_price: None,
        client_order_id: None,
    };
    if rt.dry_run {
        tracing::info!(
            symbol, quantity, tp_price,
            "dry-run: would place TP limit (reduce_only)"
        );
        return Ok(());
    }
    let order = rt
        .client
        .place_limit_order(exchange, params, creds)
        .await
        .map_err(TtcToolError::Api)?;
    tracing::info!(symbol, quantity, tp_price, ?order, "TP limit placed");
    Ok(())
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
    fn parse_outcome_trade_line_uses_daemon_dry_run() {
        let s = "cycle 0: action=TRADE exchange=phemex symbol=BTCUSDT side=short \
                 qty=0.001 reason=signal dry_run=true\n<DONE>";
        // Agent claims dry_run=true; daemon says it's live. Daemon wins.
        match parse_outcome(s, "orderly", "ETHUSDT", false) {
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
                assert!(!dry_run, "daemon's live state must override agent's claim");
            }
            other => panic!("expected Trade, got {other:?}"),
        }
    }

    #[test]
    fn parse_outcome_skip_line() {
        let s = "cycle 1: action=SKIP exchange=phemex symbol=BTCUSDT side=n/a \
                 qty=n/a reason=neutral dry_run=true\n<DONE>";
        match parse_outcome(s, "orderly", "ETHUSDT", true) {
            CycleOutcome::Skip { reason } => assert_eq!(reason, "neutral"),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn parse_outcome_no_done_is_empty() {
        let s = "cycle 0: action=TRADE side=long qty=1.0 dry_run=true";
        assert_eq!(parse_outcome(s, "orderly", "BTC", true), CycleOutcome::Empty);
    }

    #[test]
    fn parse_outcome_blank_is_empty() {
        assert_eq!(parse_outcome("", "orderly", "BTC", true), CycleOutcome::Empty);
    }

    #[test]
    fn parse_outcome_trade_with_zero_qty_falls_to_skip() {
        let s = "cycle 0: action=TRADE side=long qty=0 reason=rounded dry_run=true\n<DONE>";
        match parse_outcome(s, "orderly", "BTC", true) {
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

    // Risk-based sizing math.

    #[test]
    fn risk_qty_short_basic() {
        // $300 balance, 1% risk = $3.
        // Short BTC, entry=80000, stop=80800 → distance=800.
        // qty = 3 / 800 = 0.00375 → notional ~ $300 (very small position).
        let qty = compute_risk_qty("short", 80000.0, 80800.0, 300.0, 1.0).unwrap();
        assert!((qty - 0.00375).abs() < 1e-9);
    }

    #[test]
    fn risk_qty_long_basic() {
        // Long ZRO, entry=$1.50, stop=$1.45 → distance=$0.05.
        // $300 × 1% = $3 risk → qty = 3 / 0.05 = 60.
        let qty = compute_risk_qty("long", 1.50, 1.45, 300.0, 1.0).unwrap();
        assert!((qty - 60.0).abs() < 1e-9);
    }

    #[test]
    fn risk_qty_long_with_stop_above_entry_is_invalid() {
        // For a long, stop must be BELOW entry. This catches scanner output
        // where the side and stop are inconsistent.
        let err = compute_risk_qty("long", 100.0, 105.0, 300.0, 1.0).unwrap_err();
        assert_eq!(err, "stop-not-below-entry");
    }

    #[test]
    fn risk_qty_short_with_stop_below_entry_is_invalid() {
        let err = compute_risk_qty("short", 100.0, 95.0, 300.0, 1.0).unwrap_err();
        assert_eq!(err, "stop-not-above-entry");
    }

    #[test]
    fn risk_qty_caps_at_50x_balance() {
        // Tiny stop distance would explode the position size. Cap kicks in.
        let err = compute_risk_qty("long", 1000.0, 999.99, 300.0, 1.0).unwrap_err();
        assert_eq!(err, "notional-exceeds-50x-balance");
    }

    #[test]
    fn risk_qty_rejects_zero_distance() {
        let err = compute_risk_qty("long", 100.0, 100.0, 300.0, 1.0).unwrap_err();
        // Stop equals entry: caught by stop-not-below-entry first.
        assert_eq!(err, "stop-not-below-entry");
    }

    #[test]
    fn risk_qty_rejects_garbage_inputs() {
        assert!(compute_risk_qty("long", f64::NAN, 95.0, 300.0, 1.0).is_err());
        assert!(compute_risk_qty("long", 100.0, f64::INFINITY, 300.0, 1.0).is_err());
        assert!(compute_risk_qty("long", 100.0, 95.0, 0.0, 1.0).is_err());
        assert!(compute_risk_qty("long", 100.0, 95.0, 300.0, 0.0).is_err());
        assert!(compute_risk_qty("sideways", 100.0, 95.0, 300.0, 1.0).is_err());
    }

    #[test]
    fn round_qty_high_priced_uses_4_decimals() {
        // BTC at $80k: 0.000186 → 0.0002 (round to 4)
        assert!((round_qty_for_price(0.000186, 80000.0) - 0.0002).abs() < 1e-9);
    }

    #[test]
    fn round_qty_mid_priced_uses_2_decimals() {
        // ZRO at $1.50: 9.572 → 9.57 (round to 2)
        assert!((round_qty_for_price(9.572, 1.50) - 9.57).abs() < 1e-9);
    }

    #[test]
    fn round_qty_sub_dollar_uses_whole_tokens() {
        // SWARMS at $0.0277: 526.7 → 527 (round to 0)
        assert!((round_qty_for_price(526.7, 0.0277) - 527.0).abs() < 1e-9);
    }

    #[test]
    fn split_qty_for_one_tp_uses_full_size() {
        let chunks = split_qty_for_tps(10.0, 1, 50.0);
        assert_eq!(chunks, vec![10.0]);
    }

    #[test]
    fn split_qty_for_three_tps_splits_evenly() {
        let chunks = split_qty_for_tps(9.0, 3, 50.0);
        assert_eq!(chunks.len(), 3);
        // 9/3 = 3 each, mid-priced rounds to 2 decimals → exact.
        let total: f64 = chunks.iter().sum();
        assert!((total - 9.0).abs() < 1e-9);
    }

    #[test]
    fn split_qty_handles_uneven_chunks() {
        // 10 / 3 = 3.333... per chunk; reference price $50 (mid-priced) rounds to 3.33.
        // Last chunk gets the remainder so the sum stays at 10.
        let chunks = split_qty_for_tps(10.0, 3, 50.0);
        assert_eq!(chunks.len(), 3);
        let total: f64 = chunks.iter().sum();
        assert!(
            (total - 10.0).abs() < 0.01,
            "chunks should sum close to total; got {chunks:?}"
        );
    }

    #[test]
    fn split_qty_for_zero_tps_returns_empty() {
        assert!(split_qty_for_tps(10.0, 0, 50.0).is_empty());
    }

    #[test]
    fn close_side_for_long_returns_sell() {
        assert!(matches!(
            close_side_for("long"),
            Some(skill_trading::models::OrderSide::Sell)
        ));
    }

    #[test]
    fn close_side_for_short_returns_buy() {
        assert!(matches!(
            close_side_for("short"),
            Some(skill_trading::models::OrderSide::Buy)
        ));
    }

    #[test]
    fn close_side_for_garbage_returns_none() {
        assert!(close_side_for("sideways").is_none());
    }
}
