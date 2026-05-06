# Swarms Python vs swarms-rs — Research Notes

Status: Research / not a PRD
Date: 2026-05-06
Scope: capture what the [Python quickstart](https://docs.swarms.world/quickstart)
shows, compare to what `swarms-rs` actually offers, and note which
patterns are worth borrowing for the TTC integration. Kept separate
from `tetrac-integration.md` so the PRD stays focused.

## Headline finding

The official quickstart is **Python-only**. Every code block in
`docs.swarms.world/quickstart` imports `from swarms import …`. The
Rust port (`swarms-rs`, the fork we're working in) is a separate
project on a separate doc site (`docs.rs/swarms-rs`) and **lags the
Python feature set**. So treat the Python quickstart as a north star
for surface design, not a literal API to clone.

## What the Python quickstart shows

Quoted directly from the page:

> An Agent is the fundamental building block of a swarm — an
> autonomous entity powered by an LLM + Tools + Memory.

Smallest agent (Python):

```python
from swarms import Agent

agent = Agent(
    model_name="gpt-5.4",
    max_loops="auto",
    interactive=True,
)

response = agent.run(
    "What are the key benefits of using a multi-agent system?"
)
```

Smallest sequential swarm (Python):

```python
from swarms import Agent, SequentialWorkflow

researcher = Agent(
    agent_name="Researcher",
    system_prompt="Your job is to research the provided topic and provide a detailed summary.",
    model_name="gpt-5.4",
)

writer = Agent(
    agent_name="Writer",
    system_prompt="Your job is to take the research summary and write a beautiful, engaging blog post about it.",
    model_name="gpt-5.4",
)

workflow = SequentialWorkflow(agents=[researcher, writer])

final_post = workflow.run(
    "The history and future of artificial intelligence"
)
```

Notable Python features mentioned in the quickstart:

- `model_name` accepts `"gpt-5.4"`, `"claude-sonnet-4-5"` (string-keyed
  provider abstraction).
- `max_loops="auto"` — runtime decides when to stop.
- `interactive=True` — real-time conversational mode.
- `SequentialWorkflow`, `ConcurrentWorkflow` referenced.
- `HierarchicalSwarm` and `MixtureOfAgents` mentioned in passing
  (no detail on the page).
- Tools, memory, async, and structured output are *not* covered in
  the quickstart itself — they're linked off-page.

## What `swarms-rs` actually has today

From reading the source under `swarms-rs/swarms-rs/src/`:

| Capability                      | swarms-rs status |
|---------------------------------|------------------|
| `Agent` type                    | yes (`structs::agent::Agent`, builder via `client.agent_builder()`) |
| OpenAI/DeepSeek provider        | yes (`llm::provider::openai::OpenAI`) |
| Anthropic provider              | yes (per `Cargo.toml` example wiring) |
| Tool calling via free-fn macro  | yes (`swarms-macro::tool`, see `examples/single_agent/tool.rs`) |
| MCP STDIO transport             | yes (`add_stdio_mcp_server`) |
| MCP SSE transport               | yes (`add_sse_mcp_server`) |
| MCP streamable-http transport   | **no** (rmcp 0.1.5) |
| Sequential workflow             | yes (`structs::sequential_workflow`) |
| Concurrent workflow             | yes (`structs::concurrent_workflow`) |
| Graph workflow                  | yes (`structs::graph_workflow`) |
| Swarm router                    | yes (`structs::swarms_router`) |
| Agent rearrange                 | yes (`structs::rearrange`) |
| Persistence / autosave          | yes (`enable_autosave`, `save_state_dir`) |
| Pretty-print agent state        | yes (per recent commits in fork) |
| `max_loops = "auto"`            | **no** — Rust API takes `u32` only |
| `interactive=True` chat mode    | **no equivalent** found |
| Memory subsystem                | partial — there's `conversation.rs` for chat history, but no long-term/embedding memory module |
| Structured output (pydantic-ish)| partial — tool args use `JsonSchema`, but agent output is `String` |
| HierarchicalSwarm               | **no** — only `swarms_router.rs`, not a hierarchical primitive |
| MixtureOfAgents                 | **no** |

`swarms-macro::tool` shape (confirmed):

```rust
#[tool(description = "Subtract y from x", arg(x, description = "..."), arg(y, description = "..."))]
fn sub(x: f64, y: f64) -> Result<f64, CalcError> { Ok(x - y) }

// Generated:
//   pub struct SubTool;
//   pub static SUB: SubTool = SubTool;
// Register:
//   .agent_builder().add_tool(SubTool).add_tool(Add).build()
```

Or with a struct argument:

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
struct ExecShell { commands: Vec<String>, … }

#[tool(description = "Execute shell commands")]
fn exec(x: ExecShell) -> Result<String, CalcError> { … }
```

Constraint from `structs/tool.rs`:

```rust
pub trait Tool: Sized + Send + Sync {
    type Error: core::error::Error + Send + Sync + 'static;
    type Args: for<'a> Deserialize<'a> + Send + Sync;
    type Output: Serialize;
    const NAME: &'static str;
    fn definition(&self) -> ToolDefinition;
    fn call(&self, args: Self::Args) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send + Sync;
}
```

Important consequence: **the `#[tool]` macro produces stateless
structs**. They can't close over a `TtcClient` instance. This is why
the PRD's §5.3 uses a `OnceLock<TtcClient>` — it's the idiomatic
workaround in this codebase, not a hack. (If we needed per-agent
state we'd hand-roll the `Tool` trait instead of using the macro.)

## Side-by-side: same workflow, both languages

The TTC PRD's W1 (market snapshot) in both worlds, conceptually:

**Python (hypothetical, given quickstart conventions):**

```python
from swarms import Agent
# Suppose we expose ttc tools as a python tool list
agent = Agent(
    agent_name="TtcMarketWatcher",
    model_name="claude-sonnet-4-5",
    tools=[get_tickers, get_scanner],   # callables
    max_loops="auto",
)
agent.run("What's the orderly funding rate on BTC-USDT and the top 3 movers?")
```

**Rust (concrete, what we'll write):**

```rust
let agent = OpenAI::from_url(base_url, api_key)
    .set_model("deepseek-chat")
    .agent_builder()
    .system_prompt("You are a TTC market watcher.")
    .agent_name("TtcMarketWatcher")
    .with_tetrac(TtcConfig::from_env()?)   // installs CLIENT, registers read tools
    .max_loops(5)                           // Rust API takes u32, no "auto"
    .build();

let response = agent.run(
    "What's the orderly funding rate on BTC-USDT and the top 3 movers?"
        .to_string(),
).await?;
```

Same shape, two notable Rust-side concessions: `max_loops: u32` (no
`"auto"`), and the agent run is async (no `interactive` REPL).

## Things worth borrowing from the Python world

1. **String-keyed model selection.** Python's `model_name="gpt-5.4"`
   is friendlier than swarms-rs's
   `OpenAI::from_url(...).set_model(...)`. Out of scope for our PRD,
   but a useful upstream contribution someday.
2. **`max_loops="auto"`.** Worth filing as a swarms-rs issue. For TTC
   trading agents, "auto" is dangerous (could keep retrying), so
   we'd probably *not* adopt it even if it shipped — a fixed cap is
   safer for money-moving code.
3. **System prompt patterns.** The Python quickstart's role-based
   prompts ("Your job is to research…") translate verbatim — they're
   model-side, language-agnostic.
4. **Workflow names match.** `SequentialWorkflow`/`ConcurrentWorkflow`
   exist in both. Our W2 example should mirror Python idioms so a
   Python user reading the Rust example feels at home.

## Things deliberately *not* worth borrowing

1. **`interactive=True`.** A live REPL in a Rust trading agent is
   a UX antipattern — interactivity belongs in `skill-trading` (the
   CLI), not in an autonomous agent. Skip.
2. **Tool callables-as-Python-functions.** Python's "just pass a
   function" style is incompatible with Rust's typed Tool trait.
   We're using the proc macro instead; that's the right Rust idiom.
3. **`HierarchicalSwarm` / `MixtureOfAgents`.** The Python docs
   mention these but the Rust port doesn't have them. We don't need
   them for W1/W2/W3. If a future TTC workflow needs hierarchy, the
   existing `SwarmRouter` may be enough; if not, file upstream.

## Useful for the PRD specifically

- The Rust `#[tool]` macro is **stateless**, so the PRD's
  `OnceLock<TtcClient>` choice (§5.3, option 1) is already the
  documented pattern in upstream's own `tool.rs` example. Confirms
  that's not a workaround — it's the idiom.
- The `agent_builder` pattern composes `.add_tool(X)` calls
  arbitrarily, so `with_tetrac(cfg)` can chain ~10 `add_tool` calls
  internally and the user code stays one line. Matches the PRD's
  ergonomics goal.
- Sequential and concurrent workflows already work today — W2
  (concurrent arb scan) and W3 (sequential signal-execute) don't
  require any new framework code, only TTC tools.

## Open questions for upstream (file as issues, not PRD scope)

- Will `rmcp` gain streamable-http transport? Affects whether anyone
  can ever consume `ttc.box/api/v1/mcp` from Rust without a bridge.
- Does swarms-rs plan an embedding-memory module? Long-running TTC
  agents (signal watcher, trail-watch) would benefit from one.
- Is there interest in a shared "trading tools" pattern that other
  exchanges could adopt (Hyperliquid, GMX, etc.) so TTC isn't the
  only hosted service plugged in? The fork's Tool naming
  (`get_tickers`, `place_market_order`) is generic enough to slot
  into this.

## Things to confirm next time we touch swarms-rs

- Whether `concurrent_workflow_run_batch.rs` is the right primitive
  for W2 (5 concurrent venue lookups) vs hand-rolling
  `tokio::join!`. Quick read on day 1 of M2.
- Whether `graph_workflow.rs` adds anything over `SequentialWorkflow`
  for W3. Probably not for v1, but worth a peek.

## Sources

- https://docs.swarms.world/quickstart (Python quickstart)
- `swarms-rs/swarms-rs/src/structs/tool.rs` (Tool trait)
- `swarms-rs/swarms-rs/examples/single_agent/tool.rs` (macro usage)
- `swarms-rs/swarms-rs/examples/single_agent/mcp_tool.rs` (MCP wiring)
- `swarms-rs/swarms-rs/Cargo.toml` (rmcp version, providers)
