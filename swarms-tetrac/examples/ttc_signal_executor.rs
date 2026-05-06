//! W3 — Sequential signal → risk → executor pipeline.
//!
//! Signal-watch agent reads the scanner + funding. Risk-check agent reads
//! account balance + open positions. Executor agent decides whether to place
//! a market order — but every place_*_order call returns a dry-run envelope
//! by default (TtcConfig.dry_run = true). Set TTC_DRY_RUN=false in .env to
//! send real orders.
//!
//! Run with:
//!   cargo run --example ttc_signal_executor -p swarms-tetrac
//!
//! Live trading prerequisites (when you flip TTC_DRY_RUN=false):
//!   - ORDERLY_API_KEY / ORDERLY_API_SECRET / ORDERLY_API_PASSPHRASE in .env
//!   - small USDC float on the agent's Orderly account

use std::env;

use anyhow::{Context, Result};
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::sequential_workflow::SequentialWorkflow;
use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{
    GetBalanceTool, GetFundingRatesTool, GetPositionsTool, GetScannerTool, PlaceMarketOrderTool,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();

    let cfg = TtcConfig::from_env()?;
    if !cfg.dry_run {
        eprintln!(
            "WARNING: TTC_DRY_RUN=false — this run will place a REAL order if the \
             executor agent decides to. Ctrl-C now if you didn't mean to."
        );
    }
    swarms_tetrac::install(&cfg)?;

    let base_url = env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL not set")?;
    let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());
    let client = OpenAI::from_url(base_url, api_key).set_model(&model);

    let signal_watch = client
        .agent_builder()
        .agent_name("SignalWatch")
        .system_prompt(
            "You read live signals from TTC. Call get_scanner on the requested \
             symbol (1h) and get_funding_rates for the same symbol. Output a \
             one-paragraph verdict: signal direction (long/short/neutral), \
             confidence, and current funding bias. End with <DONE>.",
        )
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .max_loops(3)
        .temperature(0.2)
        .add_stop_word("<DONE>")
        .build();

    let risk_check = client
        .agent_builder()
        .agent_name("RiskCheck")
        .system_prompt(
            "You evaluate whether a trade is safe given current account state. \
             Call get_balance and get_positions on \"orderly\" EXACTLY ONCE \
             EACH. Given the upstream signal verdict and the tool results, \
             output exactly one paragraph: a verdict word (PROCEED / SKIP) \
             followed by reasoning. \
             IMPORTANT: if either tool returns \"Missing credentials\" or \
             any other error, do NOT retry — just output \"SKIP — account \
             state unavailable: <error reason>\" and stop. End with <DONE>.",
        )
        .add_tool(GetBalanceTool)
        .add_tool(GetPositionsTool)
        .max_loops(3)
        .temperature(0.2)
        .add_stop_word("<DONE>")
        .build();

    let executor = client
        .agent_builder()
        .agent_name("Executor")
        .system_prompt(
            "You place orders on \"orderly\" only when the upstream verdict is \
             PROCEED. If PROCEED, call place_market_order with quantity sized to \
             5% of available USDC, side matching the signal. If SKIP, do nothing. \
             Always echo the tool result. End with <DONE>.",
        )
        .add_tool(PlaceMarketOrderTool)
        .max_loops(3)
        .temperature(0.1)
        .add_stop_word("<DONE>")
        .build();

    let agents = vec![signal_watch, risk_check, executor]
        .into_iter()
        .map(|a| Box::new(a) as _)
        .collect::<Vec<_>>();

    let workflow = SequentialWorkflow::builder()
        .name("TtcSignalExecutor")
        .description("Signal → risk → executor pipeline; dry-run by default.")
        .metadata_output_dir("./temp/ttc_signal_executor/metadata")
        .agents(agents)
        .build();

    let result = workflow.run("BTC-USDT").await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
