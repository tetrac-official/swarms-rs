//! W2 — Concurrent multi-perspective scan.
//!
//! Three specialist agents run in parallel, each focused on one read tool:
//!   - price agent       → get_hybrid_tickers
//!   - funding agent     → get_funding_rates
//!   - signal agent      → get_scanner
//!
//! All three are credential-free — no per-exchange API keys required to run
//! this example. ConcurrentWorkflow fans out the prompt to each agent and
//! returns their independent responses.
//!
//! Run with:
//!   cargo run --example ttc_arb_scan -p swarms-tetrac

use std::env;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::concurrent_workflow::ConcurrentWorkflow;
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

    let price_agent = client
        .agent_builder()
        .agent_name("PriceAgent")
        .system_prompt(
            "You report cross-exchange prices. Call get_hybrid_tickers with \
             symbol=\"BTCUSDT\" (Binance style, no dash — that's how the \
             hybrid endpoint indexes BTC). Output a compact list of \
             {exchange, last_price} tuples from the response, then end with \
             <DONE>.",
        )
        .add_tool(GetHybridTickersTool)
        .max_loops(3)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .build();

    let funding_agent = client
        .agent_builder()
        .agent_name("FundingAgent")
        .system_prompt(
            "You report funding rates across exchanges. Call get_funding_rates \
             with symbol=\"BTCUSDT\" (no dash — that's the form binance/bybit \
             index under) EXACTLY ONCE. Then output a compact list of \
             {exchange, rate} tuples sorted by rate (highest first). If only \
             one exchange responded, just report that one. End with <DONE>.",
        )
        .add_tool(GetFundingRatesTool)
        .max_loops(3)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .build();

    let signal_agent = client
        .agent_builder()
        .agent_name("SignalAgent")
        .system_prompt(
            "You report the TTC scanner signal. Call get_scanner with \
             symbol=\"BTCUSDT\" (no dash) on the 1h timeframe. Output \
             direction, confidence, and entry/SL/TP levels in one short \
             paragraph. End with <DONE>.",
        )
        .add_tool(GetScannerTool)
        .max_loops(3)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .build();

    let workflow = ConcurrentWorkflow::builder()
        .name("TtcArbScan")
        .description("Concurrent cross-exchange snapshot for one symbol.")
        .metadata_output_dir("./temp/ttc_arb_scan/metadata")
        .agents(vec![
            Box::new(price_agent),
            Box::new(funding_agent),
            Box::new(signal_agent),
        ])
        .build();

    let result = workflow.run("BTC-USDT").await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
