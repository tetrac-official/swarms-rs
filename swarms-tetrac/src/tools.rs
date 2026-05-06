//! Read-only TTC tools (PRD D3).
//!
//! Each tool wraps one method on `skill_trading::api::Client`. Tool
//! names match `ttc.box/api/v1/mcp` exactly (snake_case, unprefixed)
//! so an agent already wired to TTC's remote MCP can swap to these
//! tools by changing only the transport line.

use skill_trading::models::{
    Balance, BestBidAsk, FundingRate, GetBestBidAskParams, GetTickersParams, HybridTickersData,
    OpenInterestItem, Order, Position, ScannerResult, Ticker, VolumeSnapshotExchange,
};
use swarms_macro::tool;

use crate::client::{client, credentials_for};
use crate::error::TtcToolError;

// ============================================================================
// /markets/* — credential-free
// ============================================================================

#[tool(
    description = "Hybrid spot+futures ticker snapshot across exchanges, with optional filters.",
    arg(
        market_type,
        description = "Filter by market type: \"spot\" or \"futures\".",
        required = false
    ),
    arg(
        exchange,
        description = "Filter by exchange slug, e.g. \"binance\", \"orderly\".",
        required = false
    ),
    arg(symbol, description = "Filter by symbol, e.g. \"BTC-USDT\".", required = false),
    arg(min_volume, description = "Minimum 24h quote volume.", required = false),
    arg(min_price, description = "Minimum last price.", required = false),
    arg(max_price, description = "Maximum last price.", required = false),
    arg(up, description = "Minimum % price change up.", required = false),
    arg(down, description = "Minimum % price change down.", required = false)
)]
async fn get_hybrid_tickers(
    market_type: Option<String>,
    exchange: Option<String>,
    symbol: Option<String>,
    min_volume: Option<f64>,
    min_price: Option<f64>,
    max_price: Option<f64>,
    up: Option<f64>,
    down: Option<f64>,
) -> Result<HybridTickersData, TtcToolError> {
    Ok(client()?
        .get_hybrid_tickers(
            market_type.as_deref(),
            exchange.as_deref(),
            symbol.as_deref(),
            min_volume,
            min_price,
            max_price,
            up,
            down,
        )
        .await?)
}

#[tool(
    description = "Funding rates across exchanges, optionally filtered by symbol.",
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTC-USDT\".",
        required = false
    )
)]
async fn get_funding_rates(symbol: Option<String>) -> Result<Vec<FundingRate>, TtcToolError> {
    Ok(client()?.get_funding_rates(symbol.as_deref()).await?)
}

#[tool(
    description = "Open interest across exchanges, optionally filtered by symbol.",
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTC-USDT\".",
        required = false
    )
)]
async fn get_open_interest(symbol: Option<String>) -> Result<Vec<OpenInterestItem>, TtcToolError> {
    Ok(client()?.get_open_interest(symbol.as_deref()).await?)
}

#[tool(description = "Volume snapshot across exchanges (24h volume, OI, TVL, per-market breakdown).")]
async fn get_volume_snapshot() -> Result<Vec<VolumeSnapshotExchange>, TtcToolError> {
    Ok(client()?.get_volume_snapshot().await?)
}

#[tool(
    description = "TTC scanner signal for a symbol (entry, stop loss, take profits, momentum).",
    arg(symbol, description = "Trading pair, e.g. \"BTCUSDT\".", required = true),
    arg(
        timeframe,
        description = "Timeframe slug, e.g. \"1h\", \"4h\", \"1d\".",
        required = false
    ),
    arg(bars, description = "Number of bars to scan.", required = false),
    arg(swing_strength, description = "Swing strength filter.", required = false)
)]
async fn get_scanner(
    symbol: String,
    timeframe: Option<String>,
    bars: Option<u32>,
    swing_strength: Option<u32>,
) -> Result<ScannerResult, TtcToolError> {
    Ok(client()?
        .get_scanner(&symbol, timeframe.as_deref(), bars, swing_strength)
        .await?)
}

// ============================================================================
// /exchanges — require per-exchange credentials, loaded from env
// ============================================================================

#[tool(
    description = "Tickers from a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTC-USDT\".",
        required = false
    )
)]
async fn get_tickers(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Ticker>, TtcToolError> {
    let creds = credentials_for(&exchange)?;
    let params = GetTickersParams { symbol };
    Ok(client()?.get_tickers(&exchange, params, creds).await?)
}

#[tool(
    description = "Best bid and ask for a symbol on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair, e.g. \"BTC-USDT\".", required = true)
)]
async fn get_best_bid_ask(exchange: String, symbol: String) -> Result<BestBidAsk, TtcToolError> {
    let creds = credentials_for(&exchange)?;
    let params = GetBestBidAskParams { symbol };
    Ok(client()?
        .get_best_bid_ask(&exchange, params, creds)
        .await?)
}

#[tool(
    description = "Open positions on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTC-USDT\".",
        required = false
    )
)]
async fn get_positions(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Position>, TtcToolError> {
    let creds = credentials_for(&exchange)?;
    Ok(client()?
        .get_positions(&exchange, symbol.as_deref(), creds)
        .await?)
}

#[tool(
    description = "Account balances on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true)
)]
async fn get_balance(exchange: String) -> Result<Vec<Balance>, TtcToolError> {
    let creds = credentials_for(&exchange)?;
    Ok(client()?.get_balance(&exchange, creds).await?)
}

#[tool(
    description = "Open or recent orders on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTC-USDT\".",
        required = false
    )
)]
async fn get_orders(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Order>, TtcToolError> {
    let creds = credentials_for(&exchange)?;
    Ok(client()?
        .get_orders(&exchange, symbol.as_deref(), creds)
        .await?)
}
