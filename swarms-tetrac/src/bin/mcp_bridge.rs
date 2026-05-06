//! swarms-tetrac-mcp-bridge — MCP STDIO server wrapping our 19 TTC tools.
//!
//! Lets any MCP host (Claude Code, Cursor, Windsurf, Claude Desktop) attach
//! the same tool surface our swarms-rs agents use, with the same dry-run
//! default and the same env-driven credential loading.
//!
//! Tool names match `https://ttc.box/api/v1/mcp` exactly — drop-in transport
//! swap for any agent already wired to the remote MCP.
//!
//! Stdout is reserved for line-delimited JSON-RPC responses. Anything we'd
//! normally `println!` goes to stderr or the tracing subscriber instead.

use std::collections::HashMap;
use std::io::{self, Write as _};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swarms_rs::structs::tool::ToolDyn;
use tokio::io::{AsyncBufReadExt, BufReader};

use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{
    CancelAllOrdersTool, CancelOrderTool, ClosePositionTool, GetBalanceTool, GetBestBidAskTool,
    GetFundingRatesTool, GetHybridTickersTool, GetOpenInterestTool, GetOrdersTool,
    GetPositionsTool, GetScannerTool, GetTickersTool, GetVolumeSnapshotTool, PlaceLimitOrderTool,
    PlaceMarketOrderTool, PlaceStopOrderTool, SetHedgeModeTool, SetLeverageTool,
    SetMarginModeTool,
};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "swarms-tetrac";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    // dotenv is best-effort; MCP hosts typically pass env explicitly via
    // their server config and never `cd` into our repo.
    dotenvy::dotenv().ok();

    // Tracing goes to stderr only — stdout is reserved for JSON-RPC.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = TtcConfig::from_env().map_err(|e| io::Error::other(e.to_string()))?;
    swarms_tetrac::install(&cfg).map_err(|e| io::Error::other(e.to_string()))?;

    let tools = build_tool_registry();
    tracing::info!(count = tools.len(), "swarms-tetrac mcp bridge ready");

    run_loop(tools).await
}

fn build_tool_registry() -> HashMap<String, Box<dyn ToolDyn>> {
    let mut m: HashMap<String, Box<dyn ToolDyn>> = HashMap::new();
    macro_rules! reg {
        ($t:expr) => {{
            let boxed: Box<dyn ToolDyn> = Box::new($t);
            m.insert(boxed.name(), boxed);
        }};
    }
    // Read tools (10).
    reg!(GetHybridTickersTool);
    reg!(GetFundingRatesTool);
    reg!(GetOpenInterestTool);
    reg!(GetVolumeSnapshotTool);
    reg!(GetScannerTool);
    reg!(GetTickersTool);
    reg!(GetBestBidAskTool);
    reg!(GetPositionsTool);
    reg!(GetBalanceTool);
    reg!(GetOrdersTool);
    // Mutating tools (9) — gated behind TtcConfig.dry_run by the tool fns.
    reg!(PlaceMarketOrderTool);
    reg!(PlaceLimitOrderTool);
    reg!(PlaceStopOrderTool);
    reg!(CancelOrderTool);
    reg!(CancelAllOrdersTool);
    reg!(ClosePositionTool);
    reg!(SetLeverageTool);
    reg!(SetMarginModeTool);
    reg!(SetHedgeModeTool);
    m
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

async fn run_loop(tools: HashMap<String, Box<dyn ToolDyn>>) -> io::Result<()> {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();
    let stdout = io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(line) {
            Ok(req) if req.jsonrpc == "2.0" => handle(req, &tools).await,
            Ok(req) => Some(error_response(
                req.id.unwrap_or(Value::Null),
                -32600,
                "invalid jsonrpc version",
            )),
            Err(e) => Some(error_response(Value::Null, -32700, &format!("parse error: {e}"))),
        };

        if let Some(resp) = response {
            let mut out = stdout.lock();
            let serialized = serde_json::to_string(&resp).unwrap_or_else(|e| {
                format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize failed: {e}"}}}}"#)
            });
            writeln!(out, "{serialized}")?;
            out.flush()?;
        }
    }

    Ok(())
}

async fn handle(
    req: JsonRpcRequest,
    tools: &HashMap<String, Box<dyn ToolDyn>>,
) -> Option<JsonRpcResponse> {
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        // Notifications (no id) get no response per JSON-RPC spec.
        "notifications/initialized" | "initialized" if req.id.is_none() => None,

        "initialize" => Some(ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                "capabilities": { "tools": {} }
            }),
        )),

        "tools/list" => {
            let mut entries: Vec<Value> = tools
                .values()
                .map(|t| {
                    let def = t.definition();
                    json!({
                        "name": def.name,
                        "description": def.description,
                        "inputSchema": def.parameters,
                    })
                })
                .collect();
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Some(ok(id, json!({ "tools": entries })))
        }

        "tools/call" => {
            let name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let Some(tool) = tools.get(name) else {
                return Some(content_error(id, &format!("unknown tool: {name}")));
            };

            let args_str = match serde_json::to_string(&args) {
                Ok(s) => s,
                Err(e) => return Some(content_error(id, &format!("arg serialize failed: {e}"))),
            };

            match tool.call(args_str).await {
                Ok(result_json) => Some(content_text(id, &result_json)),
                Err(e) => Some(content_error(id, &e.to_string())),
            }
        }

        "ping" => Some(ok(id, json!({}))),

        _ if req.id.is_none() => None, // unknown notification; ignore
        _ => Some(error_response(id, -32601, &format!("method not found: {}", req.method))),
    }
}

fn ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Value, code: i64, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    }
}

fn content_text(id: Value, text: &str) -> JsonRpcResponse {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
    )
}

fn content_error(id: Value, text: &str) -> JsonRpcResponse {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": true,
        }),
    )
}
