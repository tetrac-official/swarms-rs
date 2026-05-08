//! Position protector — finds open positions without stop-loss orders
//! and adds them.
//!
//! For each cycle: list positions on the configured exchange. For every
//! position with size > 0 that has zero open orders on its symbol (i.e.
//! no stop, no TP — clearly orphaned), re-scan the symbol via TTC and
//! place a stop-market plus layered TP limits, all reduce_only.
//!
//! This is a separate role from the scan-and-trade daemon: that one
//! places its own protections at entry; this one cleans up orphans
//! that were placed manually or before stop-automation existed.
//!
//! Dry-run by default; flip `TTC_DRY_RUN=false` to actually place orders.
//!
//! Run with:
//!   cargo run --example ttc_protect_positions -p swarms-tetrac
//!
//! Tunable via env (all optional):
//!   TRADE_EXCHANGE              default "phemex"
//!   TICK_INTERVAL_SECS          default 60
//!   MAX_TICKS                   default 1 (set 0 to loop forever)
//!   PROTECT_TIMEFRAME           default "1h"  (timeframe passed to get_scanner)
//!   PROTECT_FALLBACK_STOP_PCT   default 5     (fallback stop distance as % of
//!                                              CURRENT mark price when scanner
//!                                              has no stop_loss; e.g. 5 means a
//!                                              short's stop sits 5% above mark.
//!                                              Mark-based not entry-based so the
//!                                              stop stays valid even when the
//!                                              position has drifted underwater.)
//!   PROTECT_TP_LAYER_THRESHOLD  default 30    (notional $ above which we layer
//!                                              up to 3 TPs; at or below, single
//!                                              TP — exchange-min-order driven)
//!   PROTECT_FALLBACK_TP_STEP_PCT default 2.5  (% step between fallback TPs when
//!                                              scanner gives none. TP_n sits at
//!                                              n × step from mark, on the
//!                                              profitable side of the position.)

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use skill_trading::models::{
    LimitOrderParams, Order, OrderSide, Position, PositionSide, StopOrderParams, TimeInForce,
};
use swarms_tetrac::TtcConfig;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    let dry = cfg.dry_run;
    swarms_tetrac::install(&cfg)?;

    let exchange = env::var("TRADE_EXCHANGE").unwrap_or_else(|_| "phemex".into());
    let interval_secs: u64 = env_u64("TICK_INTERVAL_SECS", 60);
    let max_ticks: u64 = env_u64("MAX_TICKS", 1);
    let timeframe = env::var("PROTECT_TIMEFRAME").unwrap_or_else(|_| "1h".into());

    eprintln!(
        "ttc_protect_positions: {}",
        if dry { "DRY-RUN (TTC_DRY_RUN=true). No real orders." }
        else { "LIVE (TTC_DRY_RUN=false). Orders will hit the exchange." }
    );
    eprintln!(
        "ttc_protect_positions: exchange={exchange} timeframe={timeframe} interval={interval_secs}s max_ticks={}",
        if max_ticks == 0 { "∞".into() } else { max_ticks.to_string() }
    );

    let mut tick: u64 = 0;
    loop {
        if max_ticks != 0 && tick >= max_ticks {
            break;
        }
        tick += 1;
        tracing::info!(cycle = tick - 1, "protector: scanning positions");
        if let Err(e) = protect_pass(&exchange, &timeframe).await {
            tracing::error!(error = %e, "protector: pass failed");
        }
        if max_ticks != 0 && tick >= max_ticks {
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    eprintln!("ttc_protect_positions: done");
    Ok(())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

async fn protect_pass(exchange: &str, timeframe: &str) -> Result<()> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let positions = rt
        .client
        .get_positions(exchange, None, creds)
        .await
        .with_context(|| format!("get_positions failed for {exchange}"))?;

    if positions.is_empty() {
        tracing::info!("protector: no positions on {exchange}");
        return Ok(());
    }

    for pos in positions {
        if pos.size <= 0.0 {
            continue;
        }
        if let Err(e) = protect_position(exchange, &pos, timeframe).await {
            tracing::error!(
                symbol = %pos.symbol,
                error = format!("{e:#}"),
                "protection failed"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderState {
    has_stop: bool,
    has_tp: bool,
}

/// Detect whether the existing open orders on a symbol already cover the
/// stop and TP roles. We only care about orders on the *closing* side
/// (sell for longs, buy for shorts) because that's the only direction
/// that protects a position; user-placed entry orders on the same side
/// as the position aren't protections.
fn analyze_orders(orders: &[Order], direction: &str) -> OrderState {
    let close = match direction {
        "long" => "sell",
        "short" => "buy",
        _ => "",
    };
    let mut has_stop = false;
    let mut has_tp = false;
    for o in orders {
        if !o.side.eq_ignore_ascii_case(close) {
            continue;
        }
        let kind = o.order_type.to_lowercase();
        if kind.contains("stop") {
            has_stop = true;
        } else if kind == "limit" {
            has_tp = true;
        }
    }
    OrderState { has_stop, has_tp }
}

async fn list_orders(exchange: &str, symbol: &str) -> Result<Vec<Order>> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let orders = rt
        .client
        .get_orders(exchange, Some(symbol), creds)
        .await
        .with_context(|| format!("get_orders failed for {exchange} {symbol}"))?;
    Ok(orders)
}

async fn protect_position(exchange: &str, pos: &Position, timeframe: &str) -> Result<()> {
    let direction = match pos.side.as_str() {
        "buy" => "long",
        "sell" => "short",
        other => anyhow::bail!("unknown position side: {other}"),
    };
    let close_side = match direction {
        "long" => OrderSide::Sell,
        "short" => OrderSide::Buy,
        _ => unreachable!(),
    };
    let pos_side = phemex_position_side(&pos.position_side);

    let orders = list_orders(exchange, &pos.symbol).await?;
    let state = analyze_orders(&orders, direction);
    if state.has_stop && state.has_tp {
        tracing::info!(
            symbol = %pos.symbol,
            "stop + TP already in place; nothing to do"
        );
        return Ok(());
    }

    let notional = position_notional(pos);
    tracing::info!(
        symbol = %pos.symbol,
        size = pos.size,
        side = %pos.side,
        notional,
        has_stop = state.has_stop,
        has_tp = state.has_tp,
        "protecting position"
    );

    let rt = swarms_tetrac::client::runtime()?;
    let scan = rt
        .client
        .get_scanner(&pos.symbol, Some(timeframe), None, None)
        .await
        .with_context(|| format!("get_scanner failed for {}", pos.symbol))?;

    if !state.has_stop {
        let stop_price = match scan.signal.stop_loss {
            Some(s) if stop_is_valid_for_direction(direction, s, pos.mark_price) => s,
            Some(s) => {
                let pct = fallback_stop_pct();
                let fallback = fallback_stop_price(direction, pos.mark_price, pct);
                tracing::warn!(
                    symbol = %pos.symbol,
                    scanner_stop = s,
                    mark_price = pos.mark_price,
                    fallback_stop = fallback,
                    fallback_pct = pct,
                    "scanner stop already past mark; using % fallback from mark"
                );
                fallback
            }
            None => {
                let pct = fallback_stop_pct();
                let fallback = fallback_stop_price(direction, pos.mark_price, pct);
                tracing::warn!(
                    symbol = %pos.symbol,
                    mark_price = pos.mark_price,
                    fallback_stop = fallback,
                    fallback_pct = pct,
                    "scanner has no stop_loss; using % fallback from mark"
                );
                fallback
            }
        };
        place_stop(exchange, &pos.symbol, close_side, pos_side, pos.size, stop_price).await?;
    } else {
        tracing::info!(symbol = %pos.symbol, "stop already present; skipping");
    }

    if !state.has_tp {
        let target = target_tp_count(notional, tp_layer_threshold());
        let scanner_tps: Vec<f64> = [
            scan.signal.take_profit1,
            scan.signal.take_profit2,
            scan.signal.take_profit3,
        ]
        .into_iter()
        .flatten()
        .filter(|&p| tp_is_valid_for_direction(direction, p, pos.mark_price))
        .collect();

        let tps: Vec<f64> = if scanner_tps.is_empty() {
            let step = fallback_tp_step_pct();
            let synth = fallback_tp_prices(direction, pos.mark_price, step, target);
            tracing::warn!(
                symbol = %pos.symbol,
                mark_price = pos.mark_price,
                step_pct = step,
                count = synth.len(),
                "scanner has no usable TP levels; using mark-based fallback"
            );
            synth
        } else {
            scanner_tps.into_iter().take(target).collect()
        };

        let n = tps.len();
        let chunks = split_qty(pos.size, n, scan.signal.entry);
        tracing::info!(
            symbol = %pos.symbol,
            notional,
            target_tps = target,
            placing_tps = n,
            "layering take-profit limits"
        );
        for (price, qty) in tps.iter().zip(chunks.iter()) {
            if *qty <= 0.0 {
                continue;
            }
            if let Err(e) = place_tp(exchange, &pos.symbol, close_side, pos_side, *qty, *price).await {
                tracing::error!(
                    symbol = %pos.symbol,
                    tp_price = price,
                    error = format!("{e:#}"),
                    "TP placement failed"
                );
            }
        }
    } else {
        tracing::info!(symbol = %pos.symbol, "TP already present; skipping");
    }
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
    tracing::info!(
        symbol,
        side = ?close_side,
        quantity,
        stop_price,
        reduce_only = true,
        "submitting stop-market order"
    );
    if rt.dry_run {
        tracing::info!(symbol, quantity, stop_price, "dry-run: would place stop-loss");
        return Ok(());
    }
    let order = rt
        .client
        .place_stop_order(exchange, params, creds)
        .await
        .context("place_stop_order failed")?;
    tracing::info!(symbol, quantity, stop_price, ?order, "stop-loss placed");
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
    tracing::info!(
        symbol,
        side = ?close_side,
        quantity,
        tp_price,
        reduce_only = true,
        "submitting TP limit order"
    );
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

/// Map the position's reported `position_side` string into the enum value
/// the API expects on follow-on orders. Phemex (and the ttc.box bridge)
/// requires posSide on every order or it returns 500. "merged"/"both" /
/// empty all map to PositionSide::Both — the one-way mode default. Hedge-
/// mode accounts return "long"/"short" explicitly and we pass that through.
fn phemex_position_side(reported: &str) -> PositionSide {
    match reported.trim().to_lowercase().as_str() {
        "long" => PositionSide::Long,
        "short" => PositionSide::Short,
        _ => PositionSide::Both,
    }
}

/// Stop price relative to the supplied base (typically `position.mark_price`,
/// i.e. current market). For a short the stop sits above the mark by `pct`;
/// for a long, below. Mark-based so the stop is always valid even when the
/// position has drifted past wherever the original entry was.
fn fallback_stop_price(direction: &str, base_price: f64, pct: f64) -> f64 {
    let factor = pct / 100.0;
    match direction {
        "short" => base_price * (1.0 + factor),
        "long" => base_price * (1.0 - factor),
        _ => base_price,
    }
}

/// Phemex rejects a stop that's already past the mark (a buy-stop below
/// market for a short, or a sell-stop above market for a long — those would
/// trigger instantly). Use this to decide whether to trust the scanner's
/// stop or fall back to a mark-based one.
fn stop_is_valid_for_direction(direction: &str, stop: f64, mark: f64) -> bool {
    match direction {
        "short" => stop > mark,
        "long" => stop < mark,
        _ => false,
    }
}

fn fallback_stop_pct() -> f64 {
    env::var("PROTECT_FALLBACK_STOP_PCT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(5.0)
}

fn tp_layer_threshold() -> f64 {
    env::var("PROTECT_TP_LAYER_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(30.0)
}

/// Number of TP orders to place. Above the threshold (default $30 notional)
/// we layer up to 3; at or below, a single TP (exchange-min-order driven —
/// splitting a $30 position into 3 makes the chunks too small to fill).
fn target_tp_count(notional: f64, threshold: f64) -> usize {
    if notional > threshold { 3 } else { 1 }
}

/// Position notional in USD. Prefer the position's reported notional when
/// available; fall back to size × mark for exchanges that don't populate it.
fn position_notional(pos: &Position) -> f64 {
    pos.notional.unwrap_or(pos.size * pos.mark_price)
}

fn fallback_tp_step_pct() -> f64 {
    env::var("PROTECT_FALLBACK_TP_STEP_PCT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(2.5)
}

/// Synthesize TP prices when the scanner gives none. TP_n sits at
/// n × step_pct from mark on the profitable side: below mark for shorts
/// (price needs to drop), above for longs.
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

/// Phemex rejects a TP-limit on the wrong side of mark just like a stop.
/// For a short, the TP must be BELOW mark (we profit when price drops).
/// For a long, ABOVE.
fn tp_is_valid_for_direction(direction: &str, tp: f64, mark: f64) -> bool {
    match direction {
        "short" => tp < mark,
        "long" => tp > mark,
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_qty_basic() {
        let chunks = split_qty(9.0, 3, 50.0);
        assert_eq!(chunks.len(), 3);
        assert!((chunks.iter().sum::<f64>() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn split_qty_zero_n() {
        assert!(split_qty(10.0, 0, 50.0).is_empty());
    }

    #[test]
    fn round_qty_high_priced() {
        assert!((round_qty(0.000186, 80000.0) - 0.0002).abs() < 1e-9);
    }

    #[test]
    fn round_qty_sub_dollar() {
        assert!((round_qty(526.7, 0.0277) - 527.0).abs() < 1e-9);
    }

    #[test]
    fn fallback_short_puts_stop_above_mark() {
        let s = fallback_stop_price("short", 0.02797, 5.0);
        assert!(s > 0.02797);
    }

    #[test]
    fn fallback_long_puts_stop_below_mark() {
        let s = fallback_stop_price("long", 100.0, 5.0);
        assert!((s - 95.0).abs() < 1e-9);
        assert!(s < 100.0);
    }

    #[test]
    fn fallback_unknown_direction_returns_base() {
        let s = fallback_stop_price("merged", 100.0, 5.0);
        assert_eq!(s, 100.0);
    }

    #[test]
    fn stop_validity_short() {
        // For shorts: stop must be ABOVE mark.
        assert!(stop_is_valid_for_direction("short", 0.028, 0.027));
        assert!(!stop_is_valid_for_direction("short", 0.026, 0.027));
        assert!(!stop_is_valid_for_direction("short", 0.027, 0.027));
    }

    #[test]
    fn stop_validity_long() {
        // For longs: stop must be BELOW mark.
        assert!(stop_is_valid_for_direction("long", 95.0, 100.0));
        assert!(!stop_is_valid_for_direction("long", 105.0, 100.0));
    }

    #[test]
    fn phemex_pos_side_merged_is_both() {
        assert!(matches!(phemex_position_side("merged"), PositionSide::Both));
        assert!(matches!(phemex_position_side("MERGED"), PositionSide::Both));
        assert!(matches!(phemex_position_side(""), PositionSide::Both));
    }

    #[test]
    fn phemex_pos_side_hedge_modes_pass_through() {
        assert!(matches!(phemex_position_side("long"), PositionSide::Long));
        assert!(matches!(phemex_position_side("Short"), PositionSide::Short));
    }

    #[test]
    fn target_tp_count_above_threshold() {
        assert_eq!(target_tp_count(31.0, 30.0), 3);
        assert_eq!(target_tp_count(100.0, 30.0), 3);
    }

    #[test]
    fn target_tp_count_at_or_below_threshold() {
        assert_eq!(target_tp_count(30.0, 30.0), 1);
        assert_eq!(target_tp_count(15.0, 30.0), 1);
        assert_eq!(target_tp_count(0.01, 30.0), 1);
    }

    fn order(side: &str, order_type: &str) -> Order {
        Order {
            order_id: "x".into(),
            symbol: "BTCUSDT".into(),
            side: side.into(),
            position_side: "merged".into(),
            order_type: order_type.into(),
            price: 0.0,
            quantity: 1.0,
            status: "new".into(),
            timestamp: 0,
            filled_quantity: None,
            average_price: None,
        }
    }

    #[test]
    fn analyze_orders_detects_stop_for_short() {
        // For a short, the closing side is buy. A buy-stop is the protection.
        let orders = vec![order("buy", "stop")];
        let s = analyze_orders(&orders, "short");
        assert!(s.has_stop);
        assert!(!s.has_tp);
    }

    #[test]
    fn analyze_orders_detects_tp_for_long() {
        // For a long, the closing side is sell. A sell-limit is the TP.
        let orders = vec![order("sell", "limit")];
        let s = analyze_orders(&orders, "long");
        assert!(s.has_tp);
        assert!(!s.has_stop);
    }

    #[test]
    fn analyze_orders_detects_both() {
        let orders = vec![order("buy", "stop"), order("buy", "limit")];
        let s = analyze_orders(&orders, "short");
        assert!(s.has_stop);
        assert!(s.has_tp);
    }

    #[test]
    fn analyze_orders_ignores_wrong_side_orders() {
        // A user-placed entry order on the same side as the position is not
        // a protection. For a short (entry side=sell), a sell-limit would
        // be e.g. a planned add — don't count it as a TP.
        let orders = vec![order("sell", "limit")];
        let s = analyze_orders(&orders, "short");
        assert!(!s.has_tp);
        assert!(!s.has_stop);
    }

    #[test]
    fn fallback_tp_short_below_mark_layered() {
        // Mark $100, step 2.5%. Three TPs at 97.5, 95.0, 92.5.
        let tps = fallback_tp_prices("short", 100.0, 2.5, 3);
        assert_eq!(tps.len(), 3);
        assert!((tps[0] - 97.5).abs() < 1e-9);
        assert!((tps[1] - 95.0).abs() < 1e-9);
        assert!((tps[2] - 92.5).abs() < 1e-9);
        // All below mark.
        assert!(tps.iter().all(|&p| p < 100.0));
    }

    #[test]
    fn fallback_tp_long_above_mark_layered() {
        let tps = fallback_tp_prices("long", 100.0, 2.5, 3);
        assert_eq!(tps.len(), 3);
        assert!(tps.iter().all(|&p| p > 100.0));
    }

    #[test]
    fn fallback_tp_single() {
        let tps = fallback_tp_prices("short", 100.0, 5.0, 1);
        assert_eq!(tps.len(), 1);
        assert!((tps[0] - 95.0).abs() < 1e-9);
    }

    #[test]
    fn tp_validity_short_must_be_below_mark() {
        assert!(tp_is_valid_for_direction("short", 95.0, 100.0));
        assert!(!tp_is_valid_for_direction("short", 105.0, 100.0));
    }

    #[test]
    fn tp_validity_long_must_be_above_mark() {
        assert!(tp_is_valid_for_direction("long", 105.0, 100.0));
        assert!(!tp_is_valid_for_direction("long", 95.0, 100.0));
    }
}
