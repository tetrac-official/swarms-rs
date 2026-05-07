//! Read-only TTC tools (PRD D3).
//!
//! Each tool wraps one method on `skill_trading::api::Client`. Tool
//! names match `ttc.box/api/v1/mcp` exactly (snake_case, unprefixed)
//! so an agent already wired to TTC's remote MCP can swap to these
//! tools by changing only the transport line.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skill_trading::models::{
    Balance, BestBidAsk, CancelOrderParams, ClosePositionParams, FundingRate, GetBestBidAskParams,
    GetTickersParams, HybridTickersData, LimitOrderParams, MarketOrderParams, OpenInterestItem,
    Order, Position, ScannerResult, SetLeverageParams, SetMarginModeParams, StopOrderParams,
    Ticker, VolumeSnapshotExchange,
};
use swarms_macro::tool;

use crate::client::{credentials_for, dry_run, runtime};
use crate::error::TtcToolError;
use crate::parsers::{
    normalize_symbol, parse_margin_mode, parse_position_side, parse_side, parse_time_in_force,
    parse_trigger_type,
};
use crate::runtime::with_auth_refresh;

/// Synthetic JSON envelope returned by mutating tools when
/// `TTC_DRY_RUN` is true (the default). Lets the LLM see exactly
/// what would have been sent without us hitting TTC.
fn dry_run_response(action: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "dry_run": true,
        "action": action,
        "args": args,
        "note": "no live call made; set TTC_DRY_RUN=false to enable",
    })
}

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
    arg(symbol, description = "Filter by symbol, e.g. \"BTCUSDT\".", required = false),
    arg(min_volume, description = "Minimum 24h quote volume.", required = false),
    arg(min_price, description = "Minimum last price.", required = false),
    arg(max_price, description = "Maximum last price.", required = false),
    arg(up, description = "Minimum % price change up.", required = false),
    arg(down, description = "Minimum % price change down.", required = false)
)]
#[allow(clippy::too_many_arguments)]
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
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        Ok(runtime()?
            .client
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
    })
    .await
}

#[tool(
    description = "Funding rates across exchanges, optionally filtered by symbol.",
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTCUSDT\".",
        required = false
    )
)]
async fn get_funding_rates(symbol: Option<String>) -> Result<Vec<FundingRate>, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        Ok(runtime()?
            .client
            .get_funding_rates(symbol.as_deref())
            .await?)
    })
    .await
}

#[tool(
    description = "Open interest across exchanges, optionally filtered by symbol.",
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTCUSDT\".",
        required = false
    )
)]
async fn get_open_interest(symbol: Option<String>) -> Result<Vec<OpenInterestItem>, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        Ok(runtime()?
            .client
            .get_open_interest(symbol.as_deref())
            .await?)
    })
    .await
}

#[tool(description = "Volume snapshot across exchanges (24h volume, OI, TVL, per-market breakdown).")]
async fn get_volume_snapshot() -> Result<Vec<VolumeSnapshotExchange>, TtcToolError> {
    with_auth_refresh(|| async { Ok(runtime()?.client.get_volume_snapshot().await?) }).await
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
    let symbol = normalize_symbol(&symbol);
    with_auth_refresh(|| async {
        Ok(runtime()?
            .client
            .get_scanner(&symbol, timeframe.as_deref(), bars, swing_strength)
            .await?)
    })
    .await
}

// ============================================================================
// /exchanges — require per-exchange credentials, loaded from env
// ============================================================================

#[tool(
    description = "Tickers from a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTCUSDT\".",
        required = false
    )
)]
async fn get_tickers(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Ticker>, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = GetTickersParams {
            symbol: symbol.clone(),
        };
        Ok(runtime()?
            .client
            .get_tickers(&exchange, params, creds)
            .await?)
    })
    .await
}

#[tool(
    description = "Best bid and ask for a symbol on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair, e.g. \"BTCUSDT\".", required = true)
)]
async fn get_best_bid_ask(exchange: String, symbol: String) -> Result<BestBidAsk, TtcToolError> {
    let symbol = normalize_symbol(&symbol);
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = GetBestBidAskParams {
            symbol: symbol.clone(),
        };
        Ok(runtime()?
            .client
            .get_best_bid_ask(&exchange, params, creds)
            .await?)
    })
    .await
}

#[tool(
    description = "Open positions on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTCUSDT\".",
        required = false
    )
)]
async fn get_positions(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Position>, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        Ok(runtime()?
            .client
            .get_positions(&exchange, symbol.as_deref(), creds)
            .await?)
    })
    .await
}

#[tool(
    description = "Account balances on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true)
)]
async fn get_balance(exchange: String) -> Result<Vec<Balance>, TtcToolError> {
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        Ok(runtime()?.client.get_balance(&exchange, creds).await?)
    })
    .await
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UsdBalance {
    /// Asset name as the exchange reports it: USDT, USDC, DUSD, FDUSD, USDe, ...
    pub asset: String,
    /// Total balance in this asset.
    pub balance: f64,
    /// Available (unencumbered) balance in this asset — what you can spend.
    pub available: f64,
}

#[tool(
    description = "Available USD-denominated stablecoin balance on an exchange. \
                   Wraps get_balance and picks the largest asset whose name contains \
                   \"USD\" — covers USDT, USDC, DUSD, FDUSD, USDe, BUSD, etc. Use this \
                   for trade sizing instead of get_balance when you need a single \
                   number, since exchanges differ on which stablecoin they use.",
    arg(exchange, description = "Exchange slug, e.g. \"phemex\".", required = true)
)]
async fn get_usd_balance(exchange: String) -> Result<UsdBalance, TtcToolError> {
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let balances = runtime()?.client.get_balance(&exchange, creds).await?;
        pick_usd_balance(&balances).ok_or_else(|| {
            TtcToolError::InvalidArg(format!(
                "no USD-stablecoin balance found on {exchange}; assets present: {:?}",
                balances.iter().map(|b| &b.asset).collect::<Vec<_>>()
            ))
        })
    })
    .await
}

/// Pick the USD-stablecoin balance with the largest available amount.
///
/// Matches any asset whose name contains "USD" (case-insensitive), so
/// USDT / USDC / BUSD / FDUSD / USDe / DUSD / TUSD / PYUSD / etc. all
/// qualify. Returns the entry with the highest `available` value, since
/// that's what's spendable for sizing a trade.
fn pick_usd_balance(balances: &[Balance]) -> Option<UsdBalance> {
    balances
        .iter()
        .filter(|b| b.asset.to_ascii_uppercase().contains("USD"))
        .max_by(|a, b| {
            a.available
                .partial_cmp(&b.available)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|b| UsdBalance {
            asset: b.asset.clone(),
            balance: b.balance,
            available: b.available,
        })
}

#[tool(
    description = "Open or recent orders on a specific exchange.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(
        symbol,
        description = "Optional symbol filter, e.g. \"BTCUSDT\".",
        required = false
    )
)]
async fn get_orders(
    exchange: String,
    symbol: Option<String>,
) -> Result<Vec<Order>, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        Ok(runtime()?
            .client
            .get_orders(&exchange, symbol.as_deref(), creds)
            .await?)
    })
    .await
}

// ============================================================================
// Mutating tools — gated by TTC_DRY_RUN (default true)
// ============================================================================

#[tool(
    description = "Place a market order on an exchange. Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair, e.g. \"BTCUSDT\".", required = true),
    arg(side, description = "\"buy\" or \"sell\".", required = true),
    arg(quantity, description = "Order quantity in base units.", required = true),
    arg(
        position_side,
        description = "\"long\" / \"short\" / \"both\". Required for hedge-mode accounts.",
        required = false
    ),
    arg(reduce_only, description = "If true, only reduces an existing position.", required = false),
    arg(client_order_id, description = "Caller-supplied order id.", required = false)
)]
async fn place_market_order(
    exchange: String,
    symbol: String,
    side: String,
    quantity: f64,
    position_side: Option<String>,
    reduce_only: Option<bool>,
    client_order_id: Option<String>,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = normalize_symbol(&symbol);
    let echo = serde_json::json!({
        "exchange": exchange,
        "symbol": symbol,
        "side": side,
        "quantity": quantity,
        "position_side": position_side,
        "reduce_only": reduce_only,
        "client_order_id": client_order_id,
    });
    if dry_run()? {
        return Ok(dry_run_response("place_market_order", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = MarketOrderParams {
            symbol: symbol.clone(),
            side: parse_side(&side)?,
            quantity,
            position_side: position_side
                .as_deref()
                .map(parse_position_side)
                .transpose()?,
            reduce_only,
            client_order_id: client_order_id.clone(),
        };
        let order = runtime()?
            .client
            .place_market_order(&exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(order)?)
    })
    .await
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlaceLimitOrderInput {
    /// Exchange slug, e.g. "orderly".
    pub exchange: String,
    /// Trading pair, e.g. "BTCUSDT".
    pub symbol: String,
    /// "buy" or "sell".
    pub side: String,
    /// Order quantity in base units.
    pub quantity: f64,
    /// Limit price.
    pub price: f64,
    /// "long" / "short" / "both". Required for hedge-mode accounts.
    pub position_side: Option<String>,
    /// "GoodTillCancel" / "ImmediateOrCancel" / "FillOrKill" / "PostOnly" (or GTC/IOC/FOK).
    pub time_in_force: Option<String>,
    /// If true, only reduces an existing position.
    pub reduce_only: Option<bool>,
    /// Optional take-profit trigger price.
    pub take_profit_price: Option<f64>,
    /// Optional stop-loss trigger price.
    pub stop_loss_price: Option<f64>,
    /// Caller-supplied order id.
    pub client_order_id: Option<String>,
}

#[tool(
    description = "Place a limit order on an exchange. Returns a dry-run envelope unless TTC_DRY_RUN=false."
)]
async fn place_limit_order(args: PlaceLimitOrderInput) -> Result<serde_json::Value, TtcToolError> {
    let mut args = args;
    args.symbol = normalize_symbol(&args.symbol);
    let echo = serde_json::to_value(&args)?;
    if dry_run()? {
        return Ok(dry_run_response("place_limit_order", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&args.exchange)?;
        let params = LimitOrderParams {
            symbol: args.symbol.clone(),
            side: parse_side(&args.side)?,
            quantity: args.quantity,
            price: args.price,
            position_side: args
                .position_side
                .as_deref()
                .map(parse_position_side)
                .transpose()?,
            time_in_force: args
                .time_in_force
                .as_deref()
                .map(parse_time_in_force)
                .transpose()?,
            reduce_only: args.reduce_only,
            take_profit_price: args.take_profit_price,
            stop_loss_price: args.stop_loss_price,
            client_order_id: args.client_order_id.clone(),
        };
        let order = runtime()?
            .client
            .place_limit_order(&args.exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(order)?)
    })
    .await
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PlaceStopOrderInput {
    /// Exchange slug, e.g. "orderly".
    pub exchange: String,
    /// Trading pair, e.g. "BTCUSDT".
    pub symbol: String,
    /// "buy" or "sell".
    pub side: String,
    /// Order quantity in base units.
    pub quantity: f64,
    /// Stop trigger price.
    pub stop_price: f64,
    /// "long" / "short" / "both". Required for hedge-mode accounts.
    pub position_side: Option<String>,
    /// "ByLastPrice" / "ByMarkPrice" / "ByIndexPrice" (or last/mark/index).
    pub trigger_type: Option<String>,
    /// Limit price (turns this into a stop-limit). Omit for stop-market.
    pub price: Option<f64>,
    /// Close-position flag (some exchanges).
    pub close_position: Option<bool>,
    pub reduce_only: Option<bool>,
    pub client_order_id: Option<String>,
}

#[tool(
    description = "Place a stop order on an exchange. Returns a dry-run envelope unless TTC_DRY_RUN=false."
)]
async fn place_stop_order(args: PlaceStopOrderInput) -> Result<serde_json::Value, TtcToolError> {
    let mut args = args;
    args.symbol = normalize_symbol(&args.symbol);
    let echo = serde_json::to_value(&args)?;
    if dry_run()? {
        return Ok(dry_run_response("place_stop_order", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&args.exchange)?;
        let params = StopOrderParams {
            symbol: args.symbol.clone(),
            side: parse_side(&args.side)?,
            quantity: args.quantity,
            stop_price: args.stop_price,
            position_side: args
                .position_side
                .as_deref()
                .map(parse_position_side)
                .transpose()?,
            trigger_type: args
                .trigger_type
                .as_deref()
                .map(parse_trigger_type)
                .transpose()?,
            price: args.price,
            close_position: args.close_position,
            reduce_only: args.reduce_only,
            client_order_id: args.client_order_id.clone(),
        };
        let order = runtime()?
            .client
            .place_stop_order(&args.exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(order)?)
    })
    .await
}

#[tool(
    description = "Cancel a specific order. Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair, e.g. \"BTCUSDT\".", required = true),
    arg(order_id, description = "Order id to cancel.", required = false),
    arg(client_order_id, description = "Client-set order id to cancel.", required = false)
)]
async fn cancel_order(
    exchange: String,
    symbol: String,
    order_id: Option<String>,
    client_order_id: Option<String>,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = normalize_symbol(&symbol);
    let echo = serde_json::json!({
        "exchange": exchange,
        "symbol": symbol,
        "order_id": order_id,
        "client_order_id": client_order_id,
    });
    if dry_run()? {
        return Ok(dry_run_response("cancel_order", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = CancelOrderParams {
            symbol: symbol.clone(),
            order_id: order_id.clone(),
            client_order_id: client_order_id.clone(),
        };
        let cancelled = runtime()?
            .client
            .cancel_order(&exchange, params, creds)
            .await?;
        Ok(serde_json::json!({ "cancelled": cancelled }))
    })
    .await
}

#[tool(
    description = "Cancel all open orders on an exchange, optionally scoped to a symbol. Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Optional symbol scope, e.g. \"BTCUSDT\".", required = false)
)]
async fn cancel_all_orders(
    exchange: String,
    symbol: Option<String>,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    let echo = serde_json::json!({ "exchange": exchange, "symbol": symbol });
    if dry_run()? {
        return Ok(dry_run_response("cancel_all_orders", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let result = runtime()?
            .client
            .cancel_all_orders(&exchange, symbol.as_deref(), creds)
            .await?;
        Ok(serde_json::to_value(result)?)
    })
    .await
}

#[tool(
    description = "Close an open position on an exchange (full or partial). Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair to close, e.g. \"BTCUSDT\".", required = true),
    arg(
        position_side,
        description = "\"long\" / \"short\" / \"both\". Required for hedge-mode accounts.",
        required = false
    ),
    arg(quantity, description = "Partial-close quantity. Omit to close fully.", required = false)
)]
async fn close_position(
    exchange: String,
    symbol: String,
    position_side: Option<String>,
    quantity: Option<f64>,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = normalize_symbol(&symbol);
    let echo = serde_json::json!({
        "exchange": exchange,
        "symbol": symbol,
        "position_side": position_side,
        "quantity": quantity,
    });
    if dry_run()? {
        return Ok(dry_run_response("close_position", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = ClosePositionParams {
            symbol: symbol.clone(),
            position_side: position_side
                .as_deref()
                .map(parse_position_side)
                .transpose()?,
            quantity,
        };
        let order = runtime()?
            .client
            .close_position(&exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(order)?)
    })
    .await
}

#[tool(
    description = "Set leverage for a symbol on an exchange. Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(symbol, description = "Trading pair, e.g. \"BTCUSDT\".", required = true),
    arg(leverage, description = "Leverage multiplier, e.g. 10. Some exchanges (Phemex) encode direction in the sign: -10 means 10× on the short side.", required = true)
)]
async fn set_leverage(
    exchange: String,
    symbol: String,
    leverage: i32,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = normalize_symbol(&symbol);
    let echo = serde_json::json!({
        "exchange": exchange,
        "symbol": symbol,
        "leverage": leverage,
    });
    if dry_run()? {
        return Ok(dry_run_response("set_leverage", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = SetLeverageParams {
            symbol: symbol.clone(),
            leverage,
        };
        let result = runtime()?
            .client
            .set_leverage(&exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(result)?)
    })
    .await
}

#[tool(
    description = "Set margin mode (isolated/cross). Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(margin_mode, description = "\"isolated\" or \"cross\".", required = true),
    arg(
        symbol,
        description = "Optional symbol scope (some exchanges set margin mode per-symbol).",
        required = false
    )
)]
async fn set_margin_mode(
    exchange: String,
    margin_mode: String,
    symbol: Option<String>,
) -> Result<serde_json::Value, TtcToolError> {
    let symbol = symbol.as_deref().map(normalize_symbol);
    let echo = serde_json::json!({
        "exchange": exchange,
        "margin_mode": margin_mode,
        "symbol": symbol,
    });
    if dry_run()? {
        return Ok(dry_run_response("set_margin_mode", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let params = SetMarginModeParams {
            symbol: symbol.clone(),
            margin_mode: parse_margin_mode(&margin_mode)?,
        };
        let result = runtime()?
            .client
            .set_margin_mode(&exchange, params, creds)
            .await?;
        Ok(serde_json::to_value(result)?)
    })
    .await
}

#[tool(
    description = "Toggle hedge mode (one-way vs two-way positions). Returns a dry-run envelope unless TTC_DRY_RUN=false.",
    arg(exchange, description = "Exchange slug, e.g. \"orderly\".", required = true),
    arg(enabled, description = "true = hedge mode, false = one-way.", required = true)
)]
async fn set_hedge_mode(
    exchange: String,
    enabled: bool,
) -> Result<serde_json::Value, TtcToolError> {
    let echo = serde_json::json!({ "exchange": exchange, "enabled": enabled });
    if dry_run()? {
        return Ok(dry_run_response("set_hedge_mode", echo));
    }
    with_auth_refresh(|| async {
        let creds = credentials_for(&exchange)?;
        let result = runtime()?
            .client
            .set_hedge_mode(&exchange, enabled, creds)
            .await?;
        Ok(serde_json::to_value(result)?)
    })
    .await
}

// ============================================================================
// Tests for pure-logic helpers (dry_run_response shape).
// Live + dry_run install paths are covered by D9 (mockito).
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_response_carries_action_and_args() {
        let v = dry_run_response("place_market_order", serde_json::json!({"x": 1}));
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["action"], "place_market_order");
        assert_eq!(v["args"]["x"], 1);
        assert!(
            v["note"].as_str().unwrap().contains("TTC_DRY_RUN"),
            "note must point at the env var the user flips"
        );
    }

    #[test]
    fn dry_run_response_args_passthrough_is_lossless() {
        let original = serde_json::json!({
            "exchange": "orderly",
            "symbol": "BTC-USDT",
            "side": "buy",
            "quantity": 0.001,
        });
        let v = dry_run_response("place_market_order", original.clone());
        assert_eq!(v["args"], original);
    }

    fn b(asset: &str, available: f64) -> Balance {
        Balance {
            asset: asset.into(),
            balance: available,
            available,
            locked: None,
        }
    }

    #[test]
    fn pick_usd_balance_picks_largest_usd_asset() {
        let balances = vec![b("BTC", 0.01), b("USDT", 100.0), b("USDC", 250.0), b("ETH", 1.0)];
        let picked = pick_usd_balance(&balances).unwrap();
        assert_eq!(picked.asset, "USDC");
        assert_eq!(picked.available, 250.0);
    }

    #[test]
    fn pick_usd_balance_matches_exotic_stablecoins() {
        // Standx uses DUSD; Frax uses FDUSD; Ethena uses USDe.
        let balances = vec![b("BTC", 5.0), b("DUSD", 800.0), b("FDUSD", 200.0), b("USDe", 50.0)];
        let picked = pick_usd_balance(&balances).unwrap();
        assert_eq!(picked.asset, "DUSD");
    }

    #[test]
    fn pick_usd_balance_case_insensitive() {
        let balances = vec![b("usdt", 42.0)];
        let picked = pick_usd_balance(&balances).unwrap();
        assert_eq!(picked.asset, "usdt");
        assert_eq!(picked.available, 42.0);
    }

    #[test]
    fn pick_usd_balance_returns_none_when_no_usd() {
        let balances = vec![b("BTC", 1.0), b("ETH", 5.0)];
        assert!(pick_usd_balance(&balances).is_none());
    }

    #[test]
    fn pick_usd_balance_handles_empty() {
        assert!(pick_usd_balance(&[]).is_none());
    }

    #[test]
    fn phemex_short_position_deserializes_after_leverage_fix() {
        // Regression: Phemex encodes short direction in the sign of `leverage`
        // (-10 = 10x short, +10 = 10x long). skill-trading originally typed
        // Position::leverage as u32, so any short position blew up the
        // deserializer. This fixture is the shape that bit us live; if anyone
        // downgrades the skill-trading rev or reverts the i32 fix, this goes red.
        use skill_trading::models::Position;
        let json = r#"[{
            "symbol":"BTCUSDT",
            "side":"short",
            "positionSide":"short",
            "size":0.0002,
            "entryPrice":80000.0,
            "markPrice":79900.0,
            "pnl":0.02,
            "leverage":-10,
            "liquidationPrice":null,
            "marginType":"cross",
            "unrealizedPnl":0.02,
            "notional":16.0
        }]"#;
        let positions: Vec<Position> = serde_json::from_str(json)
            .expect("phemex short with negative leverage must deserialize");
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].leverage, -10);
        assert_eq!(positions[0].symbol, "BTCUSDT");
        assert_eq!(positions[0].side, "short");
    }
}

