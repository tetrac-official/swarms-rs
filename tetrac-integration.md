# Tetrac (TTC) Integration — PRD

Status: Draft v0.3
Owner: tetrac-official
Repo: `tetrac-official/swarms-rs` (fork of `The-Swarm-Corporation/swarms-rs`)
Date: 2026-05-06

## 0. What changed since v0.2

Read `next-ttc/src/app/api/v1/mcp/route.ts` and confirmed:

- **Transport is streamable-http only.** The handler is built with
  `mcp-handler`'s `createMcpHandler`, configured with
  `streamableHttpEndpoint: "/api/v1/mcp"` and `disableSse: true`.
  swarms-rs's `rmcp 0.1.5` only speaks SSE + STDIO, so
  `add_sse_mcp_server("ttc", "https://ttc.box/api/v1/mcp")` will not
  work without a local bridge.
- **The MCP route is a thin proxy over `/api/v1/exchanges`.**
  `callExchangeAPI` POSTs `{exchangeName, method, params, credentials}`
  to the same backend `skill-trading` already calls. Going via remote
  MCP from a Rust agent that already has `skill-trading` is strictly
  more hops, more JSON ser/deser, and more snake↔camel translation
  for the same result.
- **The MCP tool surface is a strict subset of skill-trading's.**
  16 tools registered: `get_tickers`, `get_best_bid_ask`,
  `get_positions`, `place_market_order`, `place_limit_order`,
  `place_stop_order`, `set_leverage`, `cancel_order`, `get_balance`,
  `get_orders`, `cancel_all_orders`, `close_all_positions`,
  `set_hedge_mode`, `get_user_trade_history`, `get_deposit_address`,
  `create_withdrawal` (+ `echo`). Missing vs the CLI: `get_scanner`,
  `get_funding_rates`, `get_open_interest`, `get_volume_snapshot`,
  `get_hybrid_tickers`, `set_margin_mode`, `close_position` (single).
- **Credential hygiene is worse on MCP.** Exchange credentials
  (`apiKey`, `apiSecret`, `passphrase`, `walletAddress`) are passed
  **inline as tool arguments** — meaning they enter the agent's
  prompt/tool-call context. The CLI reads them from `.env` per-exchange
  slots and the model never sees them.

Net effect: the **native Rust tool path is strictly better than the
remote MCP path** for any swarms-rs agent. The remote MCP endpoint
becomes "not used" rather than "an alternative". The STDIO MCP bridge
deliverable (D7) gains value, because wrapping `skill-trading` ships
more tools and better credential hygiene than `ttc.box/api/v1/mcp`
itself.

## 0a. What changed since v0.1

v0.1 assumed we needed to write a TTC REST client and a Rust x402 payer
from scratch. We don't. The user already maintains
[`rust-cli-ttc-api`](file:///Users/mac/Documents/rust-cli-ttc-api)
(crate name `skill-trading`, v0.1.5) which:

- exposes `pub mod api; pub mod commands; pub mod models; pub mod
  crypto;` in `src/lib.rs` — it's already a library, not just a CLI
- ships a fully typed `api::Client` with every TTC Box endpoint
  implemented (`place_limit_order`, `place_market_order`,
  `place_stop_order`, `cancel_order`, `cancel_all_orders`,
  `get_positions`, `close_position`, `get_balance`, `set_leverage`,
  `set_margin_mode`, `set_hedge_mode`, `get_tickers`,
  `get_best_bid_ask`, `get_hybrid_tickers`, `get_funding_rates`,
  `get_open_interest`, `get_volume_snapshot`, `get_scanner`, …)
- handles auth via `ttc-auth-token` + `ttc-public-key` headers, retry
  with exponential backoff (3 attempts, network/rate-limit aware), and
  a `TtcError` enum with retryable classification
- ships local wallet crypto (Ed25519/secp256k1, PBKDF2-SHA1 +
  AES-256-CBC) so private keys never leave the box in plaintext
- already supports `--output-format json` for stable machine output,
  used by existing agent skills under
  `rust-cli-ttc-api/.claude/skills/`

The integration plan therefore collapses to **wrapping
`skill-trading` as a swarms-rs tool surface**, not reimplementing it.
That changes architecture, deliverables, and timeline.

## 1. Summary

Wire [Tetrac / ttc.box](https://ttc.box/docs) into `swarms-rs` by
reusing the existing `skill-trading` Rust library. Goal: a swarms-rs
single agent or workflow can read TTC market data and execute trades
across 15+ exchanges in under 50 lines of user code, with no new HTTP
client, auth flow, or crypto path written in this fork.

Two surfaces are exposed to the agent:

1. **Native Rust tools** — `swarms-tetrac` wraps each
   `skill_trading::api::Client` method as a swarms-rs `Tool`. Direct
   in-process calls, lowest latency, full type safety.
2. **MCP STDIO bridge (optional)** — a thin shim wraps the
   `skill-trading` binary as an MCP STDIO server, callable via the
   existing `agent_builder.add_stdio_mcp_server(...)`. This serves
   non-Rust swarms hosts and other MCP hosts (Claude Code, Cursor,
   Windsurf) for free.

The remote `ttc.box/api/v1/mcp` endpoint is **not consumed** by this
fork. It runs streamable-http only (see §0) and is a thin proxy over
`/api/v1/exchanges` — the same backend `skill-trading` already calls.
Routing a Rust agent through it would add hops without adding
capability, and would expose exchange credentials to the agent prompt.
Documented here so future contributors don't re-propose it.

## 2. Why this fork

The upstream `swarms-rs` is a generic multi-agent framework with MCP
support but no first-class TTC bindings. This fork's first meaningful
divergence is a thin `swarms-tetrac` companion crate that imports
`skill-trading` and re-exports each endpoint as a swarms-rs `Tool`. No
upstream files are modified beyond an example registration in
`Cargo.toml`. The fork stays trivially mergeable.

## 3. Non-goals

- **No new HTTP client.** Reuse `skill_trading::api::Client`.
- **No new TTC auth code.** Reuse `skill-trading`'s session token model
  (`ttc-auth-token` + `ttc-public-key`).
- **No new crypto code.** Reuse `skill_trading::crypto`.
- **No ATP integration.** (Same rationale as v0.1: redundant with
  x402 + session tokens, single-vendor facilitator, raw-key-in-header
  ergonomics, encryption latency hurts trading data.)
- **No x402 integration in v1.** Every realistic TTC user already has
  a session token via `skill-trading login` or `register`. x402 only
  matters for *anonymous external* agents and is a separate persona;
  ship it as a follow-up crate `swarms-tetrac-x402` only if demand
  materializes.
- **No changes to `skill-trading`'s CLI surface.** Backward-compatible
  additions to its lib API (e.g. a new helper) are fine; renaming or
  removing existing public items is forbidden.
- **No SSE/streamable-http bridge for `ttc.box/api/v1/mcp` in v1.**
  See §0 and §5. Skipping the bridge avoids reimplementing what
  `skill-trading` already does, with strictly worse credential
  hygiene.
- **No TTC backend changes.** Anything requiring a TTC-side change is
  filed against `tradingtoolcrypto/next-ttc`, not done here.

## 4. Users and motivating workflows

Three personas, narrowed to authenticated users now that x402 is
deferred:

1. **Quant-curious Rust dev** — already runs `skill-trading login`,
   wants `cargo add swarms-rs swarms-tetrac` and a working agent that
   pulls live order books across 30+ exchanges in 20 lines.
2. **Signal-following bot operator** — already runs the Signal Copier
   manually. Wants a swarms-rs sequential workflow: signal-watch →
   risk-check → executor calling
   `skill_trading::api::Client::place_market_order(...)`.
3. **Multi-host agent author** — wants the same TTC capabilities
   available from non-Rust agents (Claude Code, Cursor, etc.) via
   STDIO MCP. Wraps `skill-trading` once, gets every host for free.

Workflow targets:

- **W1 — Market snapshot.** Single agent → `get_tickers` /
  `get_scanner` → return JSON. Read-only, no order-placement
  permissions.
- **W2 — Multi-exchange arb scan.** Concurrent workflow → 5 agents,
  each calling `get_best_bid_ask` for a different venue → reducer
  agent computes the spread.
- **W3 — Signal-routed perp order.** Sequential workflow:
  signal-watch → risk-check → executor that calls
  `place_market_order` (or `place_limit_order`) with a `--dry-run`
  guard for first-run safety.

## 5. Architecture

### 5.1 Crate layout

Add a new sibling crate `swarms-tetrac` next to `swarms-rs` and
`swarms-macro`:

```
swarms-rs/                         # workspace root
  swarms-rs/                       # existing — untouched
  swarms-macro/                    # existing — untouched
  swarms-tetrac/                   # NEW
    src/
      lib.rs                       # re-exports + TtcAgentBuilderExt
      tools.rs                     # swarms-rs Tool impls per endpoint
      mcp_bridge.rs                # optional: STDIO MCP wrapper
      config.rs                    # TtcConfig (session token, base url)
    examples/
      ttc_market_snapshot.rs       # W1
      ttc_arb_scan.rs              # W2
      ttc_signal_executor.rs       # W3 (--dry-run by default)
    Cargo.toml
  tetrac-integration.md            # this file
```

Rationale for a separate crate:
- isolates the `skill-trading` dependency (proprietary license, see
  §9) from the MIT/Apache `swarms-rs` core
- lets users opt in via `cargo add swarms-tetrac`
- keeps upstream PR-friendly

### 5.2 Dependency graph

```
                +------------------+
                |  user agent code |
                +--------+---------+
                         |
                         v
              +------------------------+
              |     swarms-tetrac      |
              | (Tool impls, examples) |
              +-----+--------------+---+
                    |              |
        depends on  |              | depends on
                    v              v
        +-------------------+   +----------------+
        |   swarms-rs       |   |  skill-trading |
        |   (Agent / Tool   |   |  (api::Client, |
        |    traits)        |   |   crypto, …)   |
        +-------------------+   +-------+--------+
                                        |
                                        v
                                +-----------------+
                                |  ttc.box/api/v1 |
                                +-----------------+
```

`skill-trading` is referenced as a **path dependency** during
development:

```toml
# swarms-tetrac/Cargo.toml
[dependencies]
swarms-rs    = { path = "../swarms-rs" }
skill-trading = { path = "../../rust-cli-ttc-api" }
```

For external users we need either (a) `skill-trading` published to
crates.io, or (b) a public git repo for `rust-cli-ttc-api` so the dep
can be `skill-trading = { git = "..." }`. Pick one in M1 — see §9.

### 5.3 Tool wrapping pattern

Confirmed against `swarms-rs/src/structs/tool.rs` and
`swarms-rs/examples/single_agent/tool.rs`: tools are defined with the
`#[tool]` proc macro from `swarms_macro` on a free function. The macro
generates a `<FnName>Tool` struct plus a `<FN_NAME>` static singleton,
both registerable via `agent_builder.add_tool(...)`.

Each public method on `skill_trading::api::Client` becomes one such
tool. Constraints from the macro:

- Function must return `Result<T, E>` where `T: Serialize` and
  `E: core::error::Error`.
- Either positional primitive args (`x: f64, y: f64`) with per-arg
  `arg(name, description = "…")` in the macro, or a single struct
  argument that derives `Serialize + Deserialize + JsonSchema` (with
  `schemars >= 1.0`).
- Tool name defaults to the function name; override via
  `name = "..."` in the macro.

Open issue with the macro shape: tool functions are top-level free
fns, so they can't close over a `TtcClient`. Two options:

1. Hold a single global `TtcClient` in a `OnceLock` initialized from
   `TtcConfig::from_env()` and have each tool fn read it. Simplest,
   matches the existing `tool.rs` example style.
2. Drop the macro for TTC tools and hand-implement the `Tool` trait
   (`pub trait Tool { type Args; type Output; type Error; const NAME;
   fn definition(&self); fn call(&self, args) -> impl Future; }`) so
   the struct can carry its own `TtcClient` field.

Recommendation: **option 1 for v1**. The `OnceLock` is initialized
inside `with_tetrac(cfg)` before the agent runs, and every tool fn
reads from it. This keeps the swarms-rs idiom; if a per-agent client
later matters (e.g. multi-tenant), switch to option 2.

Sketch (option 1):

```rust
// swarms-tetrac/src/tools.rs
use std::sync::OnceLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skill_trading::api::Client as TtcClient;
use swarms_macro::tool;
use thiserror::Error;

static CLIENT: OnceLock<TtcClient> = OnceLock::new();

pub(crate) fn install(client: TtcClient) {
    let _ = CLIENT.set(client);
}

#[derive(Debug, Error)]
pub enum TtcToolError {
    #[error("ttc client not installed; call with_tetrac() first")]
    NotInstalled,
    #[error(transparent)]
    Api(#[from] skill_trading::TtcError),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetTickersArgs {
    /// Exchange slug, e.g. "orderly", "bybit", "binance".
    pub exchange: String,
    /// Optional symbol filter, e.g. "BTC-USDT".
    pub symbol: Option<String>,
}

#[tool(
    name = "get_tickers",
    description = "Get current ticker information for trading pairs"
)]
fn get_tickers(args: GetTickersArgs) -> Result<serde_json::Value, TtcToolError> {
    let client = CLIENT.get().ok_or(TtcToolError::NotInstalled)?;
    // Tools are async via futures-erased internally; the macro adapts.
    let fut = client.get_tickers(&args.exchange, args.symbol.as_deref());
    let v = futures::executor::block_on(fut)?;
    Ok(serde_json::to_value(v)?)
}
```

Tool names: match `ttc.box/api/v1/mcp` exactly (snake_case,
unprefixed: `get_tickers`, `place_market_order`, …). This gives the
STDIO bridge in §5.5 drop-in compatibility for any agent already wired
to the remote MCP. If the agent is also given non-TTC tools that
collide on name, the user can wrap with a renaming helper — name
collisions are a per-agent concern, not a per-crate concern.

### 5.4 Agent builder extension

```rust
use swarms_tetrac::TtcAgentBuilderExt;

let agent = OpenAI::from_url(base_url, api_key)
    .set_model("deepseek-chat")
    .agent_builder()
    .system_prompt("You are a TTC trading agent.")
    .with_tetrac(TtcConfig::from_env()?)   // adds all read tools
    .with_tetrac_trading(true)             // adds order-placing tools
    .max_loops(5)
    .build();
```

`with_tetrac` registers read-only tools (`get_tickers`, `get_scanner`,
`get_positions`, `get_balance`, `get_funding_rates`, …).
`with_tetrac_trading(true)` additionally registers mutating tools
(`place_*`, `cancel_*`, `close_position`, `set_leverage`, …). The split
exists so an agent author has to opt into trading explicitly — read
tools are safe defaults.

### 5.5 STDIO MCP bridge (M4)

A separate small binary `swarms-tetrac-mcp-bridge` (in
`swarms-tetrac/src/bin/`) speaks MCP STDIO and dispatches each tool
call to `skill_trading::api::Client`. Any swarms-rs agent — or any MCP
host (Claude Code, Cursor, Windsurf, Claude Desktop) — can then
attach it via:

```rust
.add_stdio_mcp_server("ttc", ["swarms-tetrac-mcp-bridge"])
```

The bridge has independent value beyond the swarms-rs use case:

- **Strictly more tools than `ttc.box/api/v1/mcp`.** Adds
  `get_scanner`, `get_funding_rates`, `get_open_interest`,
  `get_volume_snapshot`, `get_hybrid_tickers`, `set_margin_mode`,
  `close_position` (single).
- **Better credential hygiene.** Reads exchange API keys from the
  user's `.env` (per-exchange slots like `ORDERLY_API_KEY`,
  `BYBIT_API_KEY`) instead of taking them as tool arguments. The
  agent's prompt context never contains exchange secrets.
- **Tool-name compatible** with `ttc.box/api/v1/mcp`. We keep the
  exact `snake_case` names (`get_tickers`, `place_market_order`, …)
  so any agent already wired to TTC's remote MCP can swap to the
  bridge by changing only the transport line.

This is the same `add_stdio_mcp_server` pattern as the existing
`swarms-rs/examples/single_agent/mcp_tool.rs`. The bridge is a
deliverable for M4, not M1, because the in-process Tool path already
covers the swarms-rs personas (W1/W2/W3); the bridge primarily exists
to extend the win to non-Rust hosts.

### 5.6 Auth model

| Path                  | Credential                        | Where it lives          |
|-----------------------|-----------------------------------|-------------------------|
| All native Tool calls | `TTC_AUTH_TOKEN`, `TTC_PUBLIC_KEY` | env, never logged       |
| MCP STDIO bridge      | same                              | inherited from env      |
| Trade execution       | session token (24h expiry)        | refresh via `skill-trading login` |
| Wallet ops            | `TTC_PASSKEY` (64-char hex)       | env, used by skill-trading::crypto for local AES-256-CBC |

**Custody rule:** `swarms-tetrac` never persists, logs, or transmits
either token or `TTC_PASSKEY`. We rely on `skill-trading`'s existing
hygiene: tokens are only ever sent in headers, the passkey is only
used locally to AES-encrypt wallet keys before they leave the box.

`tracing` spans must redact both headers — add a span filter as part
of D5 acceptance.

## 6. Public API sketch

```rust
// swarms-tetrac/src/lib.rs

pub use config::TtcConfig;
pub use tools::{ReadTools, TradingTools};

pub trait TtcAgentBuilderExt {
    /// Adds read-only TTC tools (market data, account read, position read).
    fn with_tetrac(self, cfg: TtcConfig) -> Self;
    /// Adds mutating TTC tools (place/cancel orders, close positions, leverage).
    /// Caller must explicitly opt in.
    fn with_tetrac_trading(self, enabled: bool) -> Self;
}

pub struct TtcConfig {
    pub auth_token: String,        // ttc-auth-token header
    pub public_key: String,        // ttc-public-key header
    pub base_url: String,          // default: https://ttc.box/api/v1
    pub default_exchange: Option<String>,
    pub dry_run: bool,             // forces all mutating tools to no-op + log
    pub max_loops_per_minute: u32, // soft circuit breaker
}

impl TtcConfig {
    pub fn from_env() -> Result<Self, ConfigError> { /* … */ }
}
```

The `dry_run` flag at the config level is the single most important
guardrail: setting it true forces every mutating tool to log the
intended call and return a synthetic success without hitting TTC. This
is what every example will use by default.

## 7. Deliverables

- [ ] **D1.** Mirror `gitlab.com/tradingtoolcrypto/rust-cli-ttc-api`
  to GitHub under Apache-2.0; update `Cargo.toml`'s `license` field;
  push. Path dep works for local dev in the meantime — D1 is *not*
  a hard prerequisite for D2/D3, only for shipping the fork to
  external users.
- [ ] **D2.** `swarms-tetrac` crate skeleton: `Cargo.toml`, `lib.rs`
  with `TtcConfig`, empty `tools.rs`. Compiles, no logic.
- [ ] **D3.** Read-only tools — `get_tickers`, `get_best_bid_ask`,
  `get_hybrid_tickers`, `get_funding_rates`, `get_open_interest`,
  `get_volume_snapshot`, `get_scanner`, `get_positions`,
  `get_balance`, `get_orders`. Each with a `JsonSchema`-derived input
  type and a `swarms-rs` `Tool` impl.
- [ ] **D4.** Trading tools — `place_limit_order`,
  `place_market_order`, `place_stop_order`, `cancel_order`,
  `cancel_all_orders`, `close_position`, `set_leverage`,
  `set_margin_mode`, `set_hedge_mode`. All gated on
  `with_tetrac_trading(true)`.
- [ ] **D5.** `tracing` redaction layer for `ttc-auth-token`,
  `ttc-public-key`, `TTC_PASSKEY` — verified by a unit test that
  inspects captured spans.
- [ ] **D6.** Examples wired into root `Cargo.toml`:
  - `ttc_market_snapshot.rs` (W1, read-only)
  - `ttc_arb_scan.rs` (W2, concurrent workflow)
  - `ttc_signal_executor.rs` (W3, sequential workflow,
    `dry_run: true` by default, with a comment explaining how to flip
    it)
- [ ] **D7.** `swarms-tetrac-mcp-bridge` binary (STDIO MCP) and one
  example (`ttc_via_mcp_bridge.rs`) demonstrating
  `add_stdio_mcp_server`.
- [ ] **D8.** `swarms-tetrac/README.md` covering env setup, the
  read-vs-trading split, the `dry_run` default, and an explicit
  pointer to `rust-cli-ttc-api/CLAUDE.md` for the underlying CLI.
- [ ] **D9.** Integration test against a recorded mock of TTC's
  endpoints (mockito), exercising one read tool, one mutating tool
  with `dry_run`, and the auth-header redaction.

## 8. Milestones

- **M1 (week 1)** — D1, D2, D3. Decision on `skill-trading`
  distribution made and acted on. `ttc_market_snapshot` example
  running against live TTC with a real session token.
- **M2 (week 2)** — D4, D5, D6 (W1 + W2). Concurrent arb scan running.
  Trading tools implemented but only exercised with `dry_run: true`.
- **M3 (week 3)** — D6 W3, D9. Signal executor example landed,
  integration tests green. First **real** (small, isolated wallet)
  order placement via the executor — manual sign-off required.
- **M4 (week 4)** — D7, D8. STDIO MCP bridge + README polish. Open a
  PR upstream proposing `swarms-tetrac` as an optional companion
  crate (no expectation it merges; the fork ships independently).

This is two weeks faster than v0.1 because the REST + x402 deliverables
are gone.

## 9. Risks and open questions

- **Distribution of `skill-trading` — RESOLVED.** Source already
  lives at https://gitlab.com/tradingtoolcrypto/rust-cli-ttc-api. User
  will mirror to GitHub under Apache-2.0. M1 starts with a path
  dependency for local dev, switches to a `cargo` git dependency
  pointing at the GitHub mirror once the mirror is up. crates.io
  publication is optional and not required for the fork to work.

- **License compatibility — RESOLVED.** With `skill-trading`
  relicensed to Apache-2.0, `swarms-tetrac` ships under Apache-2.0
  too. No proprietary-derivative complications.

- **Session-token expiry.** Tokens expire every 24h. If a swarms-rs
  agent runs longer than that (e.g. an overnight workflow), all calls
  start failing with 401. Mitigation: `swarms-tetrac` should detect
  401, log a clear "run `skill-trading login` to refresh" error, and
  fail fast — not retry forever. Filed as part of D3.

- **`Tool` trait shape — RESOLVED.** Confirmed: `#[tool]` proc macro
  from `swarms-macro` on free fns, returning `Result<T, E>`. See §5.3
  for the full pattern, including the `OnceLock<TtcClient>` workaround
  for closing over per-agent state.

- **Remote MCP transport — RESOLVED.** `next-ttc/src/app/api/v1/mcp/route.ts`
  uses `mcp-handler` with `disableSse: true` and
  `streamableHttpEndpoint: "/api/v1/mcp"`. swarms-rs's `rmcp 0.1.5`
  cannot speak streamable-http. Resolution: do not consume the remote
  MCP at all from this fork; ship the STDIO bridge wrapping
  `skill-trading` instead. The remote MCP also has fewer tools and
  worse credential hygiene than the bridge, so this is a net
  improvement, not a workaround.

- **MCP tool surface drift.** TTC may add new tools to
  `ttc.box/api/v1/mcp` (in `route.ts`) that don't exist as `Client`
  methods in `skill-trading`. Mitigation: the bridge's tool list is
  the union of `skill-trading::api::Client` methods, not a copy of
  `route.ts`. New TTC MCP tools land in the bridge only after they
  land in `skill-trading`. Document this in `swarms-tetrac/README.md`.

- **`skill-trading` lib stability.** The crate is `0.1.5` and exists
  primarily to back a CLI. Adding `swarms-tetrac` as a second consumer
  creates pressure for a stable lib API. Mitigation: keep the surface
  we depend on small (just `api::Client` methods + `models::*` enums),
  avoid touching internals.

- **Concurrency safety.** `api::Client` derives `Clone` and looks safe
  to share across tasks (`reqwest::Client` is `Send + Sync`), but the
  W2 concurrent example will be the real proof. Add a 5-task stress
  test in D9.

- **Logging hygiene.** `reqwest`'s default `tracing` output can
  include header dumps. D5's redaction layer is non-negotiable;
  acceptance test must assert the headers don't appear in any
  captured span.

## 10. Out-of-scope follow-ups

- **`swarms-tetrac-x402`** — separate companion crate for the
  unauthenticated public-agent persona. Ship only if there's demand
  beyond TTC's existing user base.
- **TypeScript / Python ports** — would let JS/Py agents call the same
  surface. Out of scope for this Rust fork; could land in a separate
  repo later by reusing the MCP STDIO bridge.
- **TTC frontend → swarms-rs reverse direction** — exposing swarms-rs
  agents *as* TTC tools. TTC product decision, not ours.
- **MT5 integration** — separate transport entirely; not blocked on
  this work and not enabled by it.

## 11. Decision log

- 2026-05-06 — Drop ATP integration entirely. Rationale: redundant
  with x402 + session tokens, single-vendor facilitator, raw-key
  ergonomics, encryption pre-settlement adds latency.
- 2026-05-06 — Drop x402 from v1. Rationale: every realistic TTC user
  already has a session token via skill-trading; x402 is for an
  anonymous-agent persona that doesn't yet have demand. Move to
  follow-up crate.
- 2026-05-06 — Reuse `rust-cli-ttc-api` (`skill-trading`) as the TTC
  client instead of writing a new one. Rationale: the user already
  maintains a complete, retry-aware, crypto-correct Rust client with
  every endpoint typed; rewriting it would be pure duplication and
  would diverge over time.
- 2026-05-06 — Create new sibling crate `swarms-tetrac` rather than
  edit `swarms-rs/src`. Rationale: keeps the upstream merge-friendly,
  isolates the proprietary `skill-trading` dependency from the
  MIT/Apache `swarms-rs` core.
- 2026-05-06 — Default `dry_run: true` for all examples that touch
  mutating endpoints. Rationale: the cost of an accidental real order
  in an example file is much higher than the cost of one extra config
  line for users who actually want it live.
- 2026-05-06 — Do not consume `ttc.box/api/v1/mcp` from this fork.
  Rationale: per `next-ttc/src/app/api/v1/mcp/route.ts`, it's
  streamable-http only (rmcp 0.1.5 can't speak it), is a thin proxy
  over `/api/v1/exchanges` that `skill-trading` already calls, has a
  smaller tool surface, and forces exchange credentials into the
  agent prompt context. The STDIO MCP bridge wrapping `skill-trading`
  is strictly better.
- 2026-05-06 — Bridge tool names match `ttc.box/api/v1/mcp` exactly
  (`get_tickers`, `place_market_order`, …). Rationale: drop-in swap
  for any agent already wired to TTC's remote MCP — change the
  transport line, keep the prompts.
