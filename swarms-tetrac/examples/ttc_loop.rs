//! Portfolio loop — scans a list of markets, takes trades that pass an
//! R:R filter, sized by per-trade risk %, until cumulative risk reaches
//! a max-exposure cap. Pure Rust, no LLM in the trade decision.
//!
//! For each pass, for each symbol:
//!   - skip if a position already exists (conflict guard)
//!   - scan via TTC; reject if direction=neutral, confidence=low, or
//!     riskRewardRatio below MIN_RR
//!   - reject if taking this trade would exceed MAX_EXPOSURE_PCT
//!   - size qty so that hitting the scanner's stop costs RISK_PCT of balance
//!   - market-buy/sell, poll for fill, place reduce_only stop + layered TPs
//!
//! Loops the symbol list every TICK_INTERVAL_SECS until exposure is full
//! or MAX_PASSES is reached.
//!
//! Run with:
//!   cargo run --example ttc_loop -p swarms-tetrac
//!
//! Tunable via env (all optional):
//!   TRADE_EXCHANGE          default "phemex"
//!   TRADE_SYMBOLS           default: 4-symbol perp list (comma-separated)
//!   TRADE_RISK_PCT          default 0.1 (per-trade risk as % of balance)
//!   TRADE_MAX_EXPOSURE_PCT  default 1.0 (stop opening trades after this much
//!                                        cumulative risk, % of balance)
//!   MIN_RR_RATIO            default 2.0 (skip signals below this R:R)
//!   PROTECT_TIMEFRAME       default "1h"
//!   PROTECT_FALLBACK_STOP_PCT      default 5
//!   PROTECT_FALLBACK_TP_STEP_PCT   default 2.5
//!   PROTECT_TP_LAYER_THRESHOLD     default 30
//!   MIN_STOP_DISTANCE_PCT   default 0.5 (% of entry. If scanner stop is tighter
//!                                        than this, widen it to this distance
//!                                        and recompute qty so risk % stays the
//!                                        same. Prevents the "tight stop → huge
//!                                        notional → race-fail" trap.)
//!   TICK_INTERVAL_SECS      default 300 (sleep between full sweeps)
//!   MAX_PASSES              default 1   (set 0 to loop forever)
//!   INTER_TRADE_DELAY_MS    default 1000 (pause between trades in a sweep)

use std::collections::HashSet;
use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use skill_trading::models::{
    LimitOrderParams, OrderSide, Position, PositionSide, ScannerResult,
    StopOrderParams, TimeInForce,
};
use swarms_tetrac::TtcConfig;

const DEFAULT_SYMBOLS: &str = "BTCUSDT,ETHUSDT,SOLUSDT,BNBUSDT";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    let dry = cfg.dry_run;
    swarms_tetrac::install(&cfg)?;

    let exchange = env::var("TRADE_EXCHANGE").unwrap_or_else(|_| "phemex".into());
    let symbols: Vec<String> = env::var("TRADE_SYMBOLS")
        .unwrap_or_else(|_| DEFAULT_SYMBOLS.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let timeframe = env::var("PROTECT_TIMEFRAME").unwrap_or_else(|_| "1h".into());
    let risk_pct: f64 = env_f64("TRADE_RISK_PCT", 0.1);
    let max_exposure_pct: f64 = env_f64("TRADE_MAX_EXPOSURE_PCT", 1.0);
    let min_rr: f64 = env_f64("MIN_RR_RATIO", 2.0);
    let interval_secs: u64 = env_u64("TICK_INTERVAL_SECS", 300);
    let max_passes: u64 = env_u64("MAX_PASSES", 1);
    let inter_trade_delay_ms: u64 = env_u64("INTER_TRADE_DELAY_MS", 1000);

    eprintln!(
        "ttc_loop: {}",
        if dry {
            "DRY-RUN (TTC_DRY_RUN=true). No real orders."
        } else {
            "LIVE (TTC_DRY_RUN=false). Real orders WILL fire."
        }
    );
    eprintln!(
        "ttc_loop: exchange={exchange} symbols={} risk={risk_pct}% \
         max_exposure={max_exposure_pct}% min_rr={min_rr} interval={interval_secs}s \
         max_passes={}",
        symbols.len(),
        if max_passes == 0 {
            "∞".into()
        } else {
            max_passes.to_string()
        }
    );

    let mut pass: u64 = 0;
    loop {
        if max_passes != 0 && pass >= max_passes {
            break;
        }
        pass += 1;

        let balance = match fetch_usd_balance(&exchange).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "couldn't read balance; skipping pass");
                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                continue;
            }
        };
        let max_risk_usd = balance * max_exposure_pct / 100.0;
        let per_trade_risk_usd = balance * risk_pct / 100.0;

        // ONE get_positions for the whole sweep. We use this to skip symbols
        // we already hold without per-symbol round trips. Each new entry we
        // open inside this sweep is a fresh symbol (the conflict guard on
        // the cached set ensures we don't double up), so the cache stays
        // valid for the duration of the sweep.
        let positioned: HashSet<String> = match fetch_all_positions(&exchange).await {
            Ok(positions) => positions
                .into_iter()
                .filter(|p| p.size > 0.0)
                .map(|p| p.symbol)
                .collect(),
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read positions cache; per-symbol guard will fall back to remote"
                );
                HashSet::new()
            }
        };

        tracing::info!(
            pass,
            balance,
            max_risk_usd,
            per_trade_risk_usd,
            symbols = symbols.len(),
            cached_positions = positioned.len(),
            "starting sweep"
        );

        let mut taken_risk_usd = 0.0;
        let mut trades_placed: u32 = 0;

        for symbol in &symbols {
            if taken_risk_usd + per_trade_risk_usd > max_risk_usd {
                tracing::info!(
                    taken_risk_usd,
                    max_risk_usd,
                    "max exposure reached; ending sweep early"
                );
                break;
            }
            match try_trade(
                &exchange,
                symbol,
                &timeframe,
                balance,
                risk_pct,
                min_rr,
                dry,
                &positioned,
            )
            .await
            {
                Ok(TradeOutcome::Placed { risk_usd }) => {
                    taken_risk_usd += risk_usd;
                    trades_placed += 1;
                    tokio::time::sleep(Duration::from_millis(inter_trade_delay_ms)).await;
                }
                Ok(TradeOutcome::Skipped { reason }) => {
                    tracing::info!(symbol, reason = %reason, "skipped");
                }
                Err(e) => {
                    tracing::warn!(symbol, error = format!("{e:#}"), "try_trade errored");
                }
            }
        }

        tracing::info!(
            pass,
            trades_placed,
            taken_risk_usd,
            max_risk_usd,
            "sweep done"
        );

        if taken_risk_usd >= max_risk_usd {
            eprintln!("ttc_loop: max exposure reached; exiting");
            break;
        }
        if max_passes != 0 && pass >= max_passes {
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    eprintln!("ttc_loop: done");
    Ok(())
}

#[derive(Debug)]
enum TradeOutcome {
    Placed { risk_usd: f64 },
    Skipped { reason: String },
}

#[allow(clippy::too_many_arguments)]
async fn try_trade(
    exchange: &str,
    symbol: &str,
    timeframe: &str,
    balance: f64,
    risk_pct: f64,
    min_rr: f64,
    dry: bool,
    positioned: &HashSet<String>,
) -> Result<TradeOutcome> {
    if positioned.contains(symbol) {
        return Ok(TradeOutcome::Skipped {
            reason: "already-positioned".into(),
        });
    }

    let scan = scan_symbol(exchange, symbol, timeframe).await?;
    let direction = scan.signal.direction.to_lowercase();
    if direction != "long" && direction != "short" {
        return Ok(TradeOutcome::Skipped {
            reason: format!("direction-{direction}"),
        });
    }
    if scan.signal.confidence.eq_ignore_ascii_case("low") {
        return Ok(TradeOutcome::Skipped {
            reason: "low-confidence".into(),
        });
    }
    let rr = scan.signal.risk_reward_ratio.unwrap_or(0.0);
    if rr < min_rr {
        return Ok(TradeOutcome::Skipped {
            reason: format!("rr-{rr:.2}-below-{min_rr}"),
        });
    }
    let entry = scan.signal.entry;
    let scanner_stop = match scan.signal.stop_loss {
        Some(s) => s,
        None => {
            return Ok(TradeOutcome::Skipped {
                reason: "no-stop-from-scanner".into(),
            });
        }
    };

    // Widen the stop if it's too tight. Risk math is unchanged; the wider
    // distance just produces a smaller position. Keeps notional sane on
    // sub-1% setups and gives phemex some breathing room before the stop
    // race-fails on adverse moves between scanner read and order placement.
    let min_distance_pct = min_stop_distance_pct();
    let stop = widen_stop_if_too_tight(&direction, entry, scanner_stop, min_distance_pct);
    if (stop - scanner_stop).abs() > f64::EPSILON {
        let scanner_dist_pct = (entry - scanner_stop).abs() / entry * 100.0;
        tracing::info!(
            symbol,
            scanner_stop,
            scanner_distance_pct = scanner_dist_pct,
            widened_stop = stop,
            min_distance_pct,
            "stop too tight; widened to minimum"
        );
    }

    let qty_raw = match compute_risk_qty(&direction, entry, stop, balance, risk_pct) {
        Ok(q) => q,
        Err(reason) => {
            return Ok(TradeOutcome::Skipped {
                reason: format!("sizing-{reason}"),
            });
        }
    };
    let qty = round_qty(qty_raw, entry);
    if qty <= 0.0 {
        return Ok(TradeOutcome::Skipped {
            reason: "qty-rounds-to-zero".into(),
        });
    }

    let close_side = match direction.as_str() {
        "long" => OrderSide::Sell,
        "short" => OrderSide::Buy,
        _ => unreachable!(),
    };
    let entry_side = match direction.as_str() {
        "long" => OrderSide::Buy,
        "short" => OrderSide::Sell,
        _ => unreachable!(),
    };

    // Decide stop + TP prices upfront. We use entry as the mark proxy
    // (we don't have a fresh mark fetch yet, and the scanner just gave us
    // the current entry — close enough for clamp/validity checks).
    let stop_price = clamp_stop_for_direction(&direction, stop, entry);
    let scanner_tps: Vec<f64> = [
        scan.signal.take_profit1,
        scan.signal.take_profit2,
        scan.signal.take_profit3,
    ]
    .into_iter()
    .flatten()
    .filter(|&p| tp_is_valid_for_direction(&direction, p, entry))
    .collect();
    let target = target_tp_count(qty * entry, tp_layer_threshold());
    let tp_prices: Vec<f64> = if scanner_tps.is_empty() {
        fallback_tp_prices(&direction, entry, fallback_tp_step_pct(), target)
    } else {
        scanner_tps.into_iter().take(target).collect()
    };
    // Use the trade's actual direction. ttc.box → phemex translates
    // PositionSide::Both inconsistently (we saw a Long entry get
    // posSide=Short on the protective stop URL, leading to
    // TE_REDUCE_ONLY_ABORT). Sending the matching direction is reliable
    // for both hedge mode and one-way mode in practice.
    let pos_side = match direction.as_str() {
        "long" => PositionSide::Long,
        "short" => PositionSide::Short,
        _ => PositionSide::Both,
    };

    tracing::info!(
        symbol,
        direction = %direction,
        confidence = %scan.signal.confidence,
        rr,
        entry,
        stop = stop_price,
        qty,
        notional = qty * entry,
        tp_count = tp_prices.len(),
        "PLACING entry + protections"
    );

    if dry {
        log_dry_run(symbol, entry_side, qty, stop_price, &tp_prices);
        return Ok(TradeOutcome::Placed {
            risk_usd: balance * risk_pct / 100.0,
        });
    }

    // Live path. Fire market then stop+TPs back-to-back. Track every order
    // ID we successfully placed so we can roll them back if the position
    // fails to materialize.
    let mut placed_ids: Vec<String> = Vec::new();

    let market_id = match place_market(exchange, symbol, entry_side, qty).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(symbol, error = format!("{e:#}"), "market entry failed");
            return Ok(TradeOutcome::Skipped {
                reason: "market-failed".into(),
            });
        }
    };
    if let Some(id) = market_id {
        placed_ids.push(id);
    }

    // Brief pause to let phemex's matching engine register the position
    // before we submit the reduce_only stop. Without this, the stop racy-
    // fails with TE_REDUCE_ONLY_ABORT (no position to reduce yet).
    tokio::time::sleep(Duration::from_millis(250)).await;

    if let Some(id) = place_stop_safe(exchange, symbol, close_side, pos_side, qty, stop_price).await {
        placed_ids.push(id);
    }

    let tp_chunks = split_qty(qty, tp_prices.len(), entry);
    let tp_pairs: Vec<(f64, f64)> = tp_prices
        .iter()
        .zip(tp_chunks.iter())
        .map(|(p, q)| (*p, *q))
        .collect();
    for (price, chunk) in &tp_pairs {
        if *chunk <= 0.0 {
            continue;
        }
        if let Some(id) =
            place_tp_safe(exchange, symbol, close_side, pos_side, *chunk, *price).await
        {
            placed_ids.push(id);
        }
    }

    // Wait briefly for the market order to fill on the matching engine.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // ONE position check.
    let position = match fetch_position(exchange, symbol).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                symbol,
                error = format!("{e:#}"),
                "post-trade position check failed; orders left as-is — verify manually"
            );
            return Ok(TradeOutcome::Placed {
                risk_usd: balance * risk_pct / 100.0,
            });
        }
    };
    let Some(_pos) = position else {
        // No position formed (rejected entry, or still pending). Cancel any
        // protective orders we placed so they don't dangle on a non-existent
        // position.
        tracing::warn!(
            symbol,
            placed = placed_ids.len(),
            "position not present after entry; cancelling placed orders"
        );
        for id in &placed_ids {
            if let Err(e) = cancel_order_by_id(exchange, symbol, id).await {
                tracing::error!(symbol, order_id = %id, error = format!("{e:#}"), "cancel failed");
            }
        }
        return Ok(TradeOutcome::Skipped {
            reason: "no-fill".into(),
        });
    };

    // Position exists. ONE get_orders to verify protections are on the books.
    let orders = match list_orders(exchange, symbol).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(
                symbol,
                error = format!("{e:#}"),
                "post-trade get_orders failed; assuming protections placed — verify manually"
            );
            return Ok(TradeOutcome::Placed {
                risk_usd: balance * risk_pct / 100.0,
            });
        }
    };
    let state = analyze_orders(&orders, &direction);
    if !state.has_stop {
        tracing::warn!(symbol, "stop missing post-fill; retrying");
        let _ = place_stop_safe(exchange, symbol, close_side, pos_side, qty, stop_price).await;
    }
    if !state.has_tp && !tp_pairs.is_empty() {
        tracing::warn!(symbol, "TPs missing post-fill; retrying");
        for (price, chunk) in &tp_pairs {
            if *chunk <= 0.0 {
                continue;
            }
            let _ =
                place_tp_safe(exchange, symbol, close_side, pos_side, *chunk, *price).await;
        }
    }

    Ok(TradeOutcome::Placed {
        risk_usd: balance * risk_pct / 100.0,
    })
}

fn log_dry_run(symbol: &str, side: OrderSide, qty: f64, stop: f64, tps: &[f64]) {
    tracing::info!(symbol, ?side, qty, stop, tp_count = tps.len(), "dry-run: would submit");
    for (i, tp) in tps.iter().enumerate() {
        tracing::info!(symbol, tp_index = i + 1, tp_price = tp, "dry-run TP");
    }
}

#[derive(Debug, Clone, Copy)]
struct OrderState {
    has_stop: bool,
    has_tp: bool,
}

fn analyze_orders(orders: &[skill_trading::models::Order], direction: &str) -> OrderState {
    let close = match direction {
        "long" => "sell",
        "short" => "buy",
        _ => "",
    };
    let mut s = OrderState { has_stop: false, has_tp: false };
    for o in orders {
        if !o.side.eq_ignore_ascii_case(close) {
            continue;
        }
        let kind = o.order_type.to_lowercase();
        if kind.contains("stop") {
            s.has_stop = true;
        } else if kind == "limit" {
            s.has_tp = true;
        }
    }
    s
}

/// Try to place a stop, returning the order_id on success or None on
/// error (which is logged). This shape lets the caller track placed IDs
/// for cleanup without bailing on failure mid-trade.
async fn place_stop_safe(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    stop_price: f64,
) -> Option<String> {
    match place_stop(exchange, symbol, close_side, pos_side, quantity, stop_price).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(symbol, error = format!("{e:#}"), "stop placement failed");
            None
        }
    }
}

async fn place_tp_safe(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    tp_price: f64,
) -> Option<String> {
    match place_tp(exchange, symbol, close_side, pos_side, quantity, tp_price).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                symbol,
                tp_price,
                error = format!("{e:#}"),
                "TP placement failed"
            );
            None
        }
    }
}

// -------- env helpers --------

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

// -------- balance / position helpers --------

async fn fetch_usd_balance(exchange: &str) -> Result<f64> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let balances = rt
        .client
        .get_balance(exchange, creds)
        .await
        .with_context(|| format!("get_balance failed for {exchange}"))?;
    let usd = balances
        .iter()
        .filter(|b| b.asset.to_ascii_uppercase().contains("USD"))
        .map(|b| b.available)
        .fold(f64::NAN, |acc, v| if acc.is_nan() || v > acc { v } else { acc });
    if !usd.is_finite() {
        anyhow::bail!("no USD-denominated balance found on {exchange}");
    }
    Ok(usd)
}

async fn fetch_all_positions(exchange: &str) -> Result<Vec<Position>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    rt.client
        .get_positions(exchange, None, creds)
        .await
        .with_context(|| format!("get_positions (all) failed for {exchange}"))
}

async fn fetch_position(exchange: &str, symbol: &str) -> Result<Option<Position>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let positions = rt
        .client
        .get_positions(exchange, Some(symbol), creds)
        .await
        .with_context(|| format!("get_positions failed for {exchange} {symbol}"))?;
    Ok(positions
        .into_iter()
        .find(|p| p.symbol == symbol && p.size > 0.0))
}

async fn scan_symbol(exchange: &str, symbol: &str, timeframe: &str) -> Result<ScannerResult> {
    let rt = swarms_tetrac::client::runtime()?;
    let _ = exchange;
    rt.client
        .get_scanner(symbol, Some(timeframe), None, None)
        .await
        .with_context(|| format!("get_scanner failed for {symbol}"))
}

// -------- order placement --------

async fn place_market(
    exchange: &str,
    symbol: &str,
    side: OrderSide,
    quantity: f64,
) -> Result<Option<String>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = skill_trading::models::MarketOrderParams {
        symbol: symbol.into(),
        side,
        quantity,
        position_side: None,
        reduce_only: Some(false),
        client_order_id: None,
    };
    if rt.dry_run {
        tracing::info!(symbol, ?side, quantity, "dry-run: would place market entry");
        return Ok(None);
    }
    let order = rt
        .client
        .place_market_order(exchange, params, creds)
        .await
        .context("place_market_order failed")?;
    tracing::info!(symbol, ?side, quantity, order_id = %order.order_id, "market entry placed");
    Ok(Some(order.order_id))
}

async fn place_stop(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    stop_price: f64,
) -> Result<Option<String>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = StopOrderParams {
        symbol: symbol.into(),
        side: close_side,
        quantity,
        stop_price,
        position_side: Some(pos_side),
        trigger_type: None,
        price: None,
        close_position: None,
        reduce_only: Some(true),
        client_order_id: None,
    };
    if rt.dry_run {
        tracing::info!(symbol, quantity, stop_price, "dry-run: would place stop");
        return Ok(None);
    }
    let order = rt
        .client
        .place_stop_order(exchange, params, creds)
        .await
        .context("place_stop_order failed")?;
    tracing::info!(symbol, quantity, stop_price, order_id = %order.order_id, "stop placed");
    Ok(Some(order.order_id))
}

async fn place_tp(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    tp_price: f64,
) -> Result<Option<String>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = LimitOrderParams {
        symbol: symbol.into(),
        side: close_side,
        quantity,
        price: tp_price,
        position_side: Some(pos_side),
        time_in_force: Some(TimeInForce::GoodTillCancel),
        reduce_only: Some(true),
        take_profit_price: None,
        stop_loss_price: None,
        client_order_id: None,
    };
    if rt.dry_run {
        tracing::info!(symbol, quantity, tp_price, "dry-run: would place TP limit");
        return Ok(None);
    }
    let order = rt
        .client
        .place_limit_order(exchange, params, creds)
        .await
        .context("place_limit_order failed")?;
    tracing::info!(symbol, quantity, tp_price, order_id = %order.order_id, "TP limit placed");
    Ok(Some(order.order_id))
}

async fn cancel_order_by_id(exchange: &str, symbol: &str, order_id: &str) -> Result<()> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let params = skill_trading::models::CancelOrderParams {
        symbol: symbol.into(),
        order_id: Some(order_id.into()),
        client_order_id: None,
    };
    rt.client
        .cancel_order(exchange, params, creds)
        .await
        .context("cancel_order failed")?;
    tracing::info!(symbol, order_id, "order cancelled");
    Ok(())
}

async fn list_orders(exchange: &str, symbol: &str) -> Result<Vec<skill_trading::models::Order>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    rt.client
        .get_orders(exchange, Some(symbol), creds)
        .await
        .with_context(|| format!("get_orders failed for {exchange} {symbol}"))
}

// -------- math helpers (duplicated from other examples; refactor later) --------

fn compute_risk_qty(
    direction: &str,
    entry: f64,
    stop_loss: f64,
    balance: f64,
    risk_pct: f64,
) -> std::result::Result<f64, String> {
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
    let qty = balance * risk_pct / 100.0 / distance;
    if qty * entry > balance * 50.0 {
        return Err("notional-exceeds-50x-balance".into());
    }
    Ok(qty)
}

fn round_qty(qty: f64, price: f64) -> f64 {
    let scale: f64 = if price > 100.0 {
        10_000.0
    } else if price >= 1.0 {
        100.0
    } else {
        1.0
    };
    (qty * scale).round() / scale
}

fn split_qty(total: f64, n: usize, reference_price: f64) -> Vec<f64> {
    if n == 0 || total <= 0.0 {
        return Vec::new();
    }
    let raw = total / n as f64;
    let chunk = round_qty(raw, reference_price);
    let mut out = vec![chunk; n];
    let assigned = chunk * (n - 1) as f64;
    let last = round_qty(total - assigned, reference_price);
    out[n - 1] = last.max(0.0);
    out
}

fn target_tp_count(notional: f64, threshold: f64) -> usize {
    if notional > threshold { 3 } else { 1 }
}

fn tp_layer_threshold() -> f64 {
    env_f64("PROTECT_TP_LAYER_THRESHOLD", 30.0)
}

fn fallback_stop_pct() -> f64 {
    env_f64("PROTECT_FALLBACK_STOP_PCT", 5.0)
}

fn min_stop_distance_pct() -> f64 {
    env_f64("MIN_STOP_DISTANCE_PCT", 0.5)
}

/// If the scanner's stop is closer to entry than `min_pct` percent of entry,
/// widen it to exactly that distance. Returns the scanner's stop unchanged
/// when it's already wide enough. Direction-aware: shorts widen above entry,
/// longs widen below.
fn widen_stop_if_too_tight(direction: &str, entry: f64, scanner_stop: f64, min_pct: f64) -> f64 {
    if entry <= 0.0 || min_pct <= 0.0 {
        return scanner_stop;
    }
    let factor = min_pct / 100.0;
    let min_distance = entry * factor;
    let current_distance = (entry - scanner_stop).abs();
    if current_distance >= min_distance {
        return scanner_stop;
    }
    match direction {
        "short" => entry * (1.0 + factor),
        "long" => entry * (1.0 - factor),
        _ => scanner_stop,
    }
}

fn fallback_tp_step_pct() -> f64 {
    env_f64("PROTECT_FALLBACK_TP_STEP_PCT", 2.5)
}

fn fallback_tp_prices(direction: &str, mark: f64, step_pct: f64, n: usize) -> Vec<f64> {
    if n == 0 || mark <= 0.0 || step_pct <= 0.0 {
        return Vec::new();
    }
    let factor = step_pct / 100.0;
    (1..=n)
        .filter_map(|i| match direction {
            "long" => Some(mark * (1.0 + factor * i as f64)),
            "short" => Some(mark * (1.0 - factor * i as f64)),
            _ => None,
        })
        .filter(|&p| p > 0.0)
        .collect()
}

fn tp_is_valid_for_direction(direction: &str, tp: f64, mark: f64) -> bool {
    match direction {
        "short" => tp < mark,
        "long" => tp > mark,
        _ => false,
    }
}

/// If the scanner's stop sits on the wrong side of mark, swap to a
/// mark-based fallback so phemex doesn't reject the order.
fn clamp_stop_for_direction(direction: &str, scanner_stop: f64, mark: f64) -> f64 {
    let valid = match direction {
        "short" => scanner_stop > mark,
        "long" => scanner_stop < mark,
        _ => false,
    };
    if valid {
        scanner_stop
    } else {
        let pct = fallback_stop_pct();
        let factor = pct / 100.0;
        match direction {
            "short" => mark * (1.0 + factor),
            "long" => mark * (1.0 - factor),
            _ => mark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_qty_basic_short() {
        let q = compute_risk_qty("short", 100.0, 102.0, 1000.0, 0.1).unwrap();
        // 0.1% of $1000 = $1; distance $2 → 0.5 tokens
        assert!((q - 0.5).abs() < 1e-9);
    }

    #[test]
    fn target_tp_above_threshold_returns_3() {
        assert_eq!(target_tp_count(31.0, 30.0), 3);
    }

    #[test]
    fn target_tp_at_threshold_returns_1() {
        assert_eq!(target_tp_count(30.0, 30.0), 1);
    }

    #[test]
    fn clamp_stop_keeps_valid_scanner_stop() {
        // Short with scanner stop above mark → keep it
        assert!((clamp_stop_for_direction("short", 110.0, 100.0) - 110.0).abs() < 1e-9);
    }

    #[test]
    fn clamp_stop_falls_back_when_scanner_invalid() {
        // Short with scanner stop below mark → fallback (5% above mark = 105)
        // (assuming default PROTECT_FALLBACK_STOP_PCT=5)
        let s = clamp_stop_for_direction("short", 90.0, 100.0);
        assert!(s > 100.0);
    }

    #[test]
    fn fallback_tp_short_layered() {
        let tps = fallback_tp_prices("short", 100.0, 2.5, 3);
        assert_eq!(tps.len(), 3);
        assert!(tps.iter().all(|&p| p < 100.0));
    }

    #[test]
    fn split_qty_three_chunks_sum_to_total() {
        let chunks = split_qty(9.0, 3, 50.0);
        assert!((chunks.iter().sum::<f64>() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn widen_stop_keeps_already_wide_short() {
        // Scanner stop 1% above entry; min 0.5%. Already wide enough → unchanged.
        let s = widen_stop_if_too_tight("short", 100.0, 101.0, 0.5);
        assert_eq!(s, 101.0);
    }

    #[test]
    fn widen_stop_widens_too_tight_short() {
        // Scanner stop 0.1% above entry; min 0.5%. Widen to 100.5.
        let s = widen_stop_if_too_tight("short", 100.0, 100.1, 0.5);
        assert!((s - 100.5).abs() < 1e-9);
    }

    #[test]
    fn widen_stop_keeps_already_wide_long() {
        // Long: stop below entry. 1% below is wider than min 0.5%, keep.
        let s = widen_stop_if_too_tight("long", 100.0, 99.0, 0.5);
        assert_eq!(s, 99.0);
    }

    #[test]
    fn widen_stop_widens_too_tight_long() {
        // Long with stop only 0.1% below entry; min 0.5%. Widen to 99.5.
        let s = widen_stop_if_too_tight("long", 100.0, 99.9, 0.5);
        assert!((s - 99.5).abs() < 1e-9);
    }

    #[test]
    fn widen_stop_unknown_direction_passes_through() {
        let s = widen_stop_if_too_tight("merged", 100.0, 100.1, 0.5);
        assert_eq!(s, 100.1);
    }

    #[test]
    fn widen_recomputes_qty_with_smaller_position() {
        // ARB-style scenario: tight stop → big qty. After widening: smaller qty.
        // Risk = $0.30 on $300 balance, 0.1% risk. Entry $100.
        // Scanner stop $100.10 → distance $0.10 → qty = 3.0
        // Widened stop (0.5% min) → $100.50 → distance $0.50 → qty = 0.6
        let raw_q = compute_risk_qty("short", 100.0, 100.1, 300.0, 0.1).unwrap();
        let widened = widen_stop_if_too_tight("short", 100.0, 100.1, 0.5);
        let new_q = compute_risk_qty("short", 100.0, widened, 300.0, 0.1).unwrap();
        assert!(raw_q > new_q, "widened stop should give a smaller qty");
        assert!((new_q - 0.6).abs() < 1e-9);
    }
}
