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
//!   TRADE_SYMBOLS           default: 15-symbol perp list (comma-separated)
//!   TRADE_RISK_PCT          default 0.1 (per-trade risk as % of balance)
//!   TRADE_MAX_EXPOSURE_PCT  default 1.0 (stop opening trades after this much
//!                                        cumulative risk, % of balance)
//!   MIN_RR_RATIO            default 2.0 (skip signals below this R:R)
//!   PROTECT_TIMEFRAME       default "1h"
//!   PROTECT_FALLBACK_STOP_PCT      default 5
//!   PROTECT_FALLBACK_TP_STEP_PCT   default 2.5
//!   PROTECT_TP_LAYER_THRESHOLD     default 30
//!   TICK_INTERVAL_SECS      default 300 (sleep between full sweeps)
//!   MAX_PASSES              default 1   (set 0 to loop forever)
//!   INTER_TRADE_DELAY_MS    default 1000 (pause between trades in a sweep)

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use skill_trading::models::{
    LimitOrderParams, OrderSide, Position, PositionSide, ScannerResult,
    StopOrderParams, TimeInForce,
};
use swarms_tetrac::TtcConfig;

const DEFAULT_SYMBOLS: &str = "BTCUSDT,ETHUSDT,SOLUSDT,BNBUSDT,XRPUSDT,\
    ADAUSDT,AVAXUSDT,DOGEUSDT,LINKUSDT,DOTUSDT,ATOMUSDT,NEARUSDT,ARBUSDT,\
    OPUSDT,SUIUSDT";

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

        tracing::info!(
            pass,
            balance,
            max_risk_usd,
            per_trade_risk_usd,
            symbols = symbols.len(),
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

async fn try_trade(
    exchange: &str,
    symbol: &str,
    timeframe: &str,
    balance: f64,
    risk_pct: f64,
    min_rr: f64,
    dry: bool,
) -> Result<TradeOutcome> {
    if has_position(exchange, symbol).await? {
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
    let stop = match scan.signal.stop_loss {
        Some(s) => s,
        None => {
            return Ok(TradeOutcome::Skipped {
                reason: "no-stop-from-scanner".into(),
            });
        }
    };

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

    tracing::info!(
        symbol,
        direction = %direction,
        confidence = %scan.signal.confidence,
        rr,
        entry,
        stop,
        qty,
        notional = qty * entry,
        "PLACING entry"
    );

    place_market(exchange, symbol, entry_side, qty).await?;

    // Poll briefly to find the actual position size after fill, then protect.
    let pos_size = if dry {
        qty
    } else {
        match poll_for_position(exchange, symbol, 20, 500).await? {
            Some(p) => p.size,
            None => {
                tracing::error!(
                    symbol,
                    "position never appeared after market order; SKIPPING stop/TP"
                );
                return Ok(TradeOutcome::Placed {
                    risk_usd: balance * risk_pct / 100.0,
                });
            }
        }
    };
    let pos_side_for_orders = if dry {
        PositionSide::Both
    } else {
        // Re-fetch to get the real position_side.
        match fetch_position(exchange, symbol).await? {
            Some(p) => phemex_position_side(&p.position_side),
            None => PositionSide::Both,
        }
    };
    let mark = if dry {
        entry
    } else {
        match fetch_position(exchange, symbol).await? {
            Some(p) => p.mark_price,
            None => entry,
        }
    };

    if let Err(e) = place_stop(
        exchange,
        symbol,
        close_side,
        pos_side_for_orders,
        pos_size,
        clamp_stop_for_direction(&direction, stop, mark),
    )
    .await
    {
        tracing::error!(symbol, error = format!("{e:#}"), "stop placement failed");
    }

    let scanner_tps: Vec<f64> = [
        scan.signal.take_profit1,
        scan.signal.take_profit2,
        scan.signal.take_profit3,
    ]
    .into_iter()
    .flatten()
    .filter(|&p| tp_is_valid_for_direction(&direction, p, mark))
    .collect();
    let target = target_tp_count(pos_size * mark, tp_layer_threshold());
    let tps: Vec<f64> = if scanner_tps.is_empty() {
        fallback_tp_prices(&direction, mark, fallback_tp_step_pct(), target)
    } else {
        scanner_tps.into_iter().take(target).collect()
    };
    let n = tps.len();
    let chunks = split_qty(pos_size, n, entry);
    for (price, q) in tps.iter().zip(chunks.iter()) {
        if *q <= 0.0 {
            continue;
        }
        if let Err(e) = place_tp(exchange, symbol, close_side, pos_side_for_orders, *q, *price)
            .await
        {
            tracing::error!(symbol, tp_price = price, error = format!("{e:#}"), "TP placement failed");
        }
    }

    Ok(TradeOutcome::Placed {
        risk_usd: balance * risk_pct / 100.0,
    })
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

async fn has_position(exchange: &str, symbol: &str) -> Result<bool> {
    Ok(fetch_position(exchange, symbol).await?.is_some())
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

async fn poll_for_position(
    exchange: &str,
    symbol: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<Option<Position>> {
    for attempt in 0..max_attempts {
        if let Some(p) = fetch_position(exchange, symbol).await? {
            tracing::debug!(attempt = attempt + 1, "position found");
            return Ok(Some(p));
        }
        if attempt + 1 < max_attempts {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Ok(None)
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
) -> Result<()> {
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
        return Ok(());
    }
    let order = rt
        .client
        .place_market_order(exchange, params, creds)
        .await
        .context("place_market_order failed")?;
    tracing::info!(symbol, ?side, quantity, ?order, "market entry placed");
    Ok(())
}

async fn place_stop(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    stop_price: f64,
) -> Result<()> {
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
        return Ok(());
    }
    let order = rt
        .client
        .place_stop_order(exchange, params, creds)
        .await
        .context("place_stop_order failed")?;
    tracing::info!(symbol, quantity, stop_price, ?order, "stop placed");
    Ok(())
}

async fn place_tp(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
    pos_side: PositionSide,
    quantity: f64,
    tp_price: f64,
) -> Result<()> {
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
        return Ok(());
    }
    let order = rt
        .client
        .place_limit_order(exchange, params, creds)
        .await
        .context("place_limit_order failed")?;
    tracing::info!(symbol, quantity, tp_price, ?order, "TP limit placed");
    Ok(())
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

fn phemex_position_side(reported: &str) -> PositionSide {
    match reported.trim().to_lowercase().as_str() {
        "long" => PositionSide::Long,
        "short" => PositionSide::Short,
        _ => PositionSide::Both,
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
}
