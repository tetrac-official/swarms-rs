//! Single-purpose agent: spend a fixed USD amount on one symbol.
//!
//! The agent fetches current price via get_best_bid_ask, computes the
//! base-unit quantity for the USD budget, then places ONE market order.
//!
//! Defaults to a $20 buy of SWARMSUSDT on phemex. Override via env:
//!   TRADE_EXCHANGE   default "phemex"
//!   TRADE_SYMBOL     default "SWARMSUSDT"
//!   TRADE_SIDE       default "buy"
//!   TRADE_USD        default "20"
//!
//! Safety:
//!   - TTC_DRY_RUN=true (the default) → the order is intercepted at the tool
//!     layer; you see the synthetic envelope and no real trade fires.
//!   - TTC_DRY_RUN=false → real money. Set this only after a dry run looks
//!     correct.
//!   - Per-exchange API key/secret/passphrase must be in .env
//!     (e.g. PHEMEX_API_KEY, PHEMEX_API_SECRET, PHEMEX_API_PASSPHRASE).
//!
//! Run:
//!   cargo run --release --example ttc_buy_usd -p swarms-tetrac

use std::env;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::agent::Agent;
use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{GetBestBidAskTool, PlaceMarketOrderTool};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    let dry_run_banner = if cfg.dry_run {
        "DRY-RUN (TTC_DRY_RUN=true). No real orders will fire."
    } else {
        "LIVE (TTC_DRY_RUN=false). Orders will hit the exchange."
    };
    swarms_tetrac::install(&cfg)?;

    let exchange = env::var("TRADE_EXCHANGE").unwrap_or_else(|_| "phemex".into());
    let symbol = env::var("TRADE_SYMBOL").unwrap_or_else(|_| "SWARMSUSDT".into());
    let side = env::var("TRADE_SIDE").unwrap_or_else(|_| "buy".into());
    let usd: f64 = env::var("TRADE_USD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20.0);

    eprintln!("ttc_buy_usd: {dry_run_banner}");
    eprintln!("ttc_buy_usd: target = {side} ${usd} of {symbol} on {exchange}");

    let base_url = env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL not set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let client = OpenAI::from_url(base_url, api_key).set_model(&model);

    let system_prompt = format!(
        "You are a TTC trade executor. Execute exactly ONE specific trade then stop.\n\
         \n\
         Trade target:\n\
         - exchange: {exchange}\n\
         - symbol:   {symbol}\n\
         - side:     {side}\n\
         - budget:   ${usd:.2} USD\n\
         \n\
         Steps:\n\
         1. Call get_best_bid_ask with exchange=\"{exchange}\" symbol=\"{symbol}\".\n\
            If it errors or returns empty, output \"ABORT: no market data for {symbol}\" \
            and stop. Do not retry with a different symbol.\n\
         2. Read the ask price (best_ask.price for a buy, best_bid.price for a sell).\n\
            Compute quantity = budget / price. Round it like this:\n\
              - price > 100   → 4 decimals\n\
              - price 1..=100 → 2 decimals\n\
              - price < 1     → 0 decimals\n\
            Reject the trade and output \"ABORT: computed quantity rounds to 0\" if the \
            rounded quantity is 0.\n\
         3. Call place_market_order EXACTLY ONCE with exchange=\"{exchange}\" \
            symbol=\"{symbol}\" side=\"{side}\" quantity=<computed_quantity>. \
            Do not call place_market_order more than once under any circumstance, \
            even if the response looks unexpected.\n\
         4. Report ONE LINE in this exact format:\n\
            \"RESULT exchange={exchange} symbol={symbol} side={side} qty=<q> price=<p> \
            usd=<estimated> dry_run=<true|false> order_id=<id|n/a>\"\n\
         5. End with <DONE>.\n\
         \n\
         Notes:\n\
         - The place_market_order response may be a dry-run envelope when \
           TTC_DRY_RUN=true: it has the form {{\"dry_run\": true, \"action\": ..., \
           \"args\": ..., \"note\": ...}}. Treat that as success and report it as \
           dry_run=true with order_id=n/a.\n\
         - When live, the response has the real order shape with order_id, status, etc."
    );

    let agent = client
        .agent_builder()
        .agent_name("TtcBuyUsd")
        .system_prompt(system_prompt)
        .add_tool(GetBestBidAskTool)
        .add_tool(PlaceMarketOrderTool)
        .max_loops(5)
        .temperature(0.0)
        .add_stop_word("<DONE>")
        .build();

    let prompt = format!(
        "Execute the {side} of ${usd:.2} {symbol} on {exchange}. \
         Follow the prescribed steps. One market order only."
    );

    let response = agent
        .run(prompt)
        .await
        .map_err(|e| anyhow::anyhow!("agent error: {e:?}"))?;

    let last = response
        .lines()
        .rfind(|l| !l.trim().is_empty() && l.trim() != "<DONE>")
        .unwrap_or("(no output)");
    println!("{last}");
    Ok(())
}
