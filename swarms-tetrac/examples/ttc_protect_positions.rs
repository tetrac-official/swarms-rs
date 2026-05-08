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
//!   TRADE_EXCHANGE       default "phemex"
//!   TICK_INTERVAL_SECS   default 60
//!   MAX_TICKS            default 1 (set 0 to loop forever)
//!   PROTECT_TIMEFRAME    default "1h"  (timeframe passed to get_scanner)

use std::env;
use std::time::Duration;

use anyhow::{Context, Result};
use skill_trading::models::{
    LimitOrderParams, OrderSide, Position, StopOrderParams, TimeInForce,
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
                tracing::error!(symbol = %pos.symbol, error = %e, "orphan check failed; skipping");
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
            tracing::error!(symbol = %pos.symbol, error = %e, "protection failed");
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

    let stop_price = scan
        .signal
        .stop_loss
        .ok_or_else(|| anyhow::anyhow!("scanner returned no stop_loss for {}", pos.symbol))?;

    place_stop(exchange, &pos.symbol, close_side, pos.size, stop_price).await?;

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
        if let Err(e) = place_tp(exchange, &pos.symbol, close_side, *qty, *price).await {
            tracing::error!(symbol = %pos.symbol, tp_price = price, error = %e, "TP placement failed");
        }
    }
    Ok(())
}

async fn place_stop(
    exchange: &str,
    symbol: &str,
    close_side: OrderSide,
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
        position_side: None,
        trigger_type: None,
        price: None,
        close_position: None,
        reduce_only: Some(true),
        client_order_id: None,
    };
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
        position_side: None,
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
}
