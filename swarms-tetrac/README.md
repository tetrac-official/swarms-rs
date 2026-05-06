# swarms-tetrac

TTC (ttc.box) integration tools for swarms-rs agents.

A thin adapter crate that exposes [skill-trading](https://github.com/tetrac-official/rust-cli-ttc-api)'s
typed API as swarms-rs `Tool` objects so an LLM agent can read TTC market
data and execute trades across 30+ exchanges. Mutating tools are dry-run
by default — no real money moves until you flip `TTC_DRY_RUN=false`.

## What's in the box

- 19 tools wired via the `#[tool]` proc macro from `swarms-macro`. Names
  match `https://ttc.box/api/v1/mcp` exactly.
- An `install(&TtcConfig)` one-liner that builds a shared `Client` for
  every tool to use.
- A `RedactingFields` tracing formatter that redacts `ttc-auth-token`,
  `ttc-public-key`, `TTC_PASSKEY`, and similar secret-named fields.
- A `swarms-tetrac-mcp-bridge` binary — MCP STDIO server that wraps all
  19 tools so non-Rust hosts (Claude Code, Cursor, Claude Desktop) can
  attach the same surface.

## Tool surface

### Read tools (10) — credential-free for the `/markets/*` group

| Tool | Endpoint | Needs exchange creds |
|------|----------|----------------------|
| `get_hybrid_tickers` | `/markets/hybrid-tickers` | no |
| `get_funding_rates` | `/markets/funding-rates` | no |
| `get_open_interest` | `/markets/open-interest` | no |
| `get_volume_snapshot` | `/markets/volume-snapshot` | no |
| `get_scanner` | `/markets/ttc-scanner` | no |
| `get_tickers` | `/exchanges` | yes |
| `get_best_bid_ask` | `/exchanges` | yes |
| `get_positions` | `/exchanges` | yes |
| `get_balance` | `/exchanges` | yes |
| `get_orders` | `/exchanges` | yes |

### Mutating tools (9) — all dry-run by default

`place_market_order`, `place_limit_order`, `place_stop_order`,
`cancel_order`, `cancel_all_orders`, `close_position`, `set_leverage`,
`set_margin_mode`, `set_hedge_mode`.

Each one returns a synthetic JSON envelope when `TtcConfig.dry_run` is
true:

```json
{
  "dry_run": true,
  "action": "place_market_order",
  "args": { "exchange": "orderly", "symbol": "BTC-USDT", ... },
  "note": "no live call made; set TTC_DRY_RUN=false to enable"
}
```

The default is `dry_run: true`. To send real orders, set `TTC_DRY_RUN=false`
in `.env`.

## Setup

### 1. Install skill-trading and register a TTC account

The TTC session token comes from the [skill-trading CLI](https://github.com/tetrac-official/rust-cli-ttc-api).
Run from your `swarms-rs` working directory so the resulting `.env` lands
here, not in skill-trading's repo:

```sh
cd /path/to/swarms-rs
/path/to/rust-cli-ttc-api/.claude/skills/skill-trading/scripts/skill-trading register
```

This generates a Solana wallet, creates a prod account on `ttc.box`, and
writes `TTC_AUTH_TOKEN`, `TTC_PUBLIC_KEY`, `TTC_PASSKEY`, `TTC_EMAIL`,
`TTC_TOKEN_ISSUED_AT` into `.env`. Tokens expire every 24h — refresh with
`skill-trading login`.

For the `/exchanges`-group tools, also fill in any per-exchange slots
in `.env` (`ORDERLY_API_KEY`, `BYBIT_API_KEY`, etc.). See
`.env.example` at the workspace root for the full list of supported
exchanges.

### 2. LLM provider

Any OpenAI-protocol-compatible endpoint works:

```sh
# .env
OPENAI_API_KEY=sk-or-v1-...                          # or your provider's key
OPENAI_BASE_URL=https://openrouter.ai/api/v1         # or api.openai.com, etc.
LLM_MODEL=anthropic/claude-sonnet-4-5                # or any model the endpoint serves
```

## Quick start

```rust
use std::env;
use anyhow::Result;
use swarms_rs::llm::provider::openai::OpenAI;
use swarms_rs::structs::agent::Agent;
use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{GetFundingRatesTool, GetScannerTool};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    swarms_tetrac::init_tracing();
    swarms_tetrac::install(&TtcConfig::from_env()?)?;

    let base_url = env::var("OPENAI_BASE_URL")?;
    let api_key = env::var("OPENAI_API_KEY")?;
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4o".into());

    let agent = OpenAI::from_url(base_url, api_key)
        .set_model(&model)
        .agent_builder()
        .agent_name("TtcMarketWatcher")
        .system_prompt("Use the tools to fetch live data from ttc.box.")
        .add_tool(GetScannerTool)
        .add_tool(GetFundingRatesTool)
        .max_loops(5)
        .build();

    let response = agent
        .run("Run the scanner on BTCUSDT 1h and report the signal.".into())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    println!("{response}");
    Ok(())
}
```

## Examples

Three runnable examples cover the main workflow shapes. All run against
live ttc.box + your configured LLM.

```sh
cargo run --example ttc_market_snapshot -p swarms-tetrac     # single agent
cargo run --example ttc_arb_scan -p swarms-tetrac            # 3 concurrent agents
cargo run --example ttc_signal_executor -p swarms-tetrac     # 3 sequential agents
```

`ttc_signal_executor` exercises `place_market_order`. It's safe by default
because `TtcConfig.dry_run` is true — orders return synthetic envelopes
and never reach an exchange.

## MCP bridge — use the same tools from non-Rust hosts

The crate ships a `swarms-tetrac-mcp-bridge` binary that speaks MCP STDIO
and re-exports the 19 tools above. Tool names match `ttc.box/api/v1/mcp`
exactly so any agent already wired to TTC's remote MCP can swap to the
bridge by changing only the transport.

Build it once:

```sh
cargo build --release -p swarms-tetrac --bin swarms-tetrac-mcp-bridge
```

Then point Claude Desktop / Claude Code at it (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "tetrac": {
      "command": "/path/to/swarms-rs/target/release/swarms-tetrac-mcp-bridge",
      "env": {
        "TTC_AUTH_TOKEN": "...",
        "TTC_PUBLIC_KEY": "...",
        "TTC_DRY_RUN": "true"
      }
    }
  }
}
```

The bridge offers strictly more tools than `ttc.box/api/v1/mcp` (it adds
`get_scanner`, `get_funding_rates`, `get_open_interest`,
`get_volume_snapshot`, `get_hybrid_tickers`, `set_margin_mode`,
`close_position`) and reads exchange API keys from `.env` instead of
taking them as inline tool arguments.

## Configuration

`TtcConfig::from_env()` reads:

| Var | Required | Default |
|-----|----------|---------|
| `TTC_AUTH_TOKEN` | yes | — |
| `TTC_PUBLIC_KEY` | yes | — |
| `TTC_BASE_URL` | no | `https://ttc.box/api/v1` |
| `TTC_DEFAULT_EXCHANGE` | no | unset |
| `TTC_DRY_RUN` | no | `true` |
| `TTC_MAX_LOOPS_PER_MINUTE` | no | `60` |

`{EXCHANGE}_API_KEY` / `_API_SECRET` / `_API_PASSPHRASE` per exchange you
target via the `/exchanges` tools. `ORDERLY_MAIN_WALLET_ADDRESS` is
required when targeting Orderly with an email-registered account.

## Custody

`swarms-tetrac` itself never persists, logs, or transmits the session
token, public key, or `TTC_PASSKEY` beyond what skill-trading already
does. `RedactingFields` defends against accidental tracing leaks; the
custom `Debug` impl on `TtcConfig` keeps `{cfg:?}` output safe.

The crate writes nothing to disk. Rotate `TTC_AUTH_TOKEN` by re-running
`skill-trading login` from the `swarms-rs` directory.

## License

Apache-2.0.
