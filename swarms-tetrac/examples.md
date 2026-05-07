# Examples

Files in `swarms-tetrac/examples/` are runnable demo binaries, not tests.
Cargo picks them up automatically — run any of them with:

```sh
cargo run --example <name> -p swarms-tetrac
```

Tests live in `swarms-tetrac/tests/` (mockito-based, D9). Examples are
not bundled into the library: when someone does `cargo add
swarms-tetrac`, they don't pull these in.

## Are they required?

No. The library (`src/`) is the product. The examples exist for three
reasons:

1. **Acceptance demos for the PRD's W1/W2/W3 workflows** — proof the
   wiring works end-to-end against live `ttc.box`.
2. **Reference templates** users copy into their own crate to bootstrap
   their own agent.
3. **Manual smoke tests** — the DevOps cycle in
   [CLAUDE.md](../CLAUDE.md) requires running the affected example
   after a change. Compilation green is not enough; the example must
   produce sensible output against live services.

You could delete the whole folder and the library would still compile
and ship. You'd lose the smoke-test surface and the copy-paste starting
points, but no production code depends on them.

## Inventory

| Example | Role |
|---|---|
| [examples/ttc_market_snapshot.rs](examples/ttc_market_snapshot.rs) | W1 — single agent, read-only tools (scanner + funding) |
| [examples/ttc_arb_scan.rs](examples/ttc_arb_scan.rs) | W2 — concurrent multi-exchange spread |
| [examples/ttc_signal_executor.rs](examples/ttc_signal_executor.rs) | W3 — sequential signal → risk → executor (dry-run default) |
| [examples/ttc_unattended.rs](examples/ttc_unattended.rs) | Daemon shape — `LoopRunner` + auth refresh, the unattended-operation end-goal from CLAUDE.md |
| [examples/ttc_buy_usd.rs](examples/ttc_buy_usd.rs) | Single-purpose: spend a fixed USD amount on one symbol via one market order |

## Convention

Every example:

- Loads `.env` via `dotenvy`.
- Calls `swarms_tetrac::init_tracing()` and
  `swarms_tetrac::install(&TtcConfig::from_env()?)` before building any
  agent.
- Reads `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `LLM_MODEL` from env.
- Defaults to `dry_run: true` for any path that would place an order
  (`TTC_DRY_RUN=false` to flip).

When adding a new example, follow the same shape so the smoke-test
step in the DevOps cycle stays uniform.
