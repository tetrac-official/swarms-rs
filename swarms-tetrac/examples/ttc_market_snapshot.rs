//! W1 — Single-agent market snapshot.
//!
//! One agent, three credential-free tools, a question:
//!   "Run the scanner on BTC-USDT and report the funding direction."
//!
//! Run with:
//!   cargo run --example ttc_market_snapshot -p swarms-tetrac

use std::env;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::agent::Agent;
use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{GetFundingRatesTool, GetHybridTickersTool, GetScannerTool};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();
    swarms_tetrac::install(&TtcConfig::from_env()?)?;

    let base_url = env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL not set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let client = OpenAI::from_url(base_url, api_key).set_model(&model);
    let agent = client
        .agent_builder()
        .agent_name("TtcMarketWatcher")
        .system_prompt(
            "You are a TTC market watcher. Use the provided tools to fetch live \
             data from ttc.box. \
             SYMBOL FORMAT: get_scanner expects Binance-style symbols (e.g. \
             \"BTCUSDT\" — no dash). get_funding_rates and get_hybrid_tickers \
             accept dashed forms (\"BTC-USDT\") and most exchange-native forms. \
             If a tool errors with \"Binance klines fetch failed\" or 400/500, \
             RETRY ONCE with the unhyphenated symbol — don't loop on the same \
             failed args. Never invent numbers — only report what tools return. \
             When you have enough data, summarize in 2-3 sentences.",
        )
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .add_tool(GetHybridTickersTool)
        .max_loops(6)
        .temperature(0.2)
        .build();

    let response = agent
        .run(
            "Run the TTC scanner on BTCUSDT at the 1h timeframe, then check \
             current funding rates for BTC-USDT. Report the scanner's signal \
             direction and whether funding is currently long-biased or \
             short-biased across the exchanges that returned data."
                .to_string(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("agent run failed: {e:?}"))?;

    println!("{response}");
    Ok(())
}
