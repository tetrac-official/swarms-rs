//! Direct positions check — no agent, no LLM, no daemon.
//!
//! Calls skill_trading::api::Client::get_positions for the configured
//! exchange and prints the raw `Vec<Position>` as pretty JSON. Useful
//! for confirming live positions are visible to our tool layer
//! independent of any prompt-following decisions the LLM makes.
//!
//! Run with:
//!   cargo run --example ttc_show_positions -p swarms-tetrac
//!
//! Tunable via env (all optional):
//!   TRADE_EXCHANGE   default "phemex"
//!   TRADE_SYMBOL     optional symbol filter (e.g. "BTCUSDT"); omit for all

use std::env;

use anyhow::{Context, Result};
use swarms_tetrac::TtcConfig;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    swarms_tetrac::install(&cfg)?;

    let exchange = env::var("TRADE_EXCHANGE").unwrap_or_else(|_| "phemex".into());
    let symbol = env::var("TRADE_SYMBOL").ok();

    eprintln!(
        "ttc_show_positions: exchange={exchange} symbol={}",
        symbol.as_deref().unwrap_or("(all)")
    );

    let rt = swarms_tetrac::client::runtime()?;
    let creds = swarms_tetrac::client::credentials_for(&exchange)?;

    let positions = rt
        .client
        .get_positions(&exchange, symbol.as_deref(), creds)
        .await
        .with_context(|| format!("get_positions failed for {exchange}"))?;

    eprintln!("ttc_show_positions: {} position(s) returned", positions.len());
    println!("{}", serde_json::to_string_pretty(&positions)?);
    Ok(())
}
