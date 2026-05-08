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

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use skill_trading::models::{
    LimitOrderParams, OrderSide, Position, PositionSide, StopOrderParams, TimeInForce,
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
        let needs = match orphan_check(exchange, &pos.symbol).await {
            Ok(needs) => needs,
            Err(e) => {
                tracing::error!(
                    symbol = %pos.symbol,
                    error = format!("{e:#}"),
                    "orphan check failed; skipping"
                );
                continue;
            }
        };
        if !needs {
            tracing::info!(symbol = %pos.symbol, "already has open orders; assuming protected");
            continue;
        }
        tracing::info!(
            symbol = %pos.symbol,
            size = pos.size,
            side = %pos.side,
            "orphan detected; placing protections"
        );
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

/// Heuristic: a position is "orphaned" if it has zero open orders on its
/// symbol. Phemex (and most exchanges) list outstanding stops and TP limits
/// alongside regular orders, so this catches both. False negatives are
/// possible if the user has unrelated open orders — log and skip in that
/// case rather than risk duplicates.
async fn orphan_check(exchange: &str, symbol: &str) -> Result<bool> {
    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(exchange)?;
    let orders = rt
        .client
        .get_orders(exchange, Some(symbol), creds)
        .await
        .with_context(|| format!("get_orders failed for {exchange} {symbol}"))?;
    Ok(orders.is_empty())
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

    let rt = swarms_tetrac::client::runtime()?;
    let scan = rt
        .client
        .get_scanner(&pos.symbol, Some(timeframe), None, None)
        .await
        .with_context(|| format!("get_scanner failed for {}", pos.symbol))?;

    let stop_price = match scan.signal.stop_loss {
        Some(s) if stop_is_valid_for_direction(direction, s, pos.mark_price) => s,
        Some(s) => {
            // Scanner gave a stop, but it's already past the market — phemex
            // would reject it. Fall through to the mark-based fallback.
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

    let pos_side = phemex_position_side(&pos.position_side);
    place_stop(exchange, &pos.symbol, close_side, pos_side, pos.size, stop_price).await?;

    let tps: Vec<f64> = [
        scan.signal.take_profit1,
        scan.signal.take_profit2,
        scan.signal.take_profit3,
    ]
    .into_iter()
    .flatten()
    .collect();
    if tps.is_empty() {
        tracing::info!(symbol = %pos.symbol, "no TP levels in scan output; stop-only");
        return Ok(());
    }

    let chunks = split_qty(pos.size, tps.len(), scan.signal.entry);
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
}
