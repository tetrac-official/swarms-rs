# CLAUDE.md

This is a hard fork of `The-Swarm-Corporation/swarms-rs`, used as a
**library wireframe** for the Tetrac (ttc.box) integration. We do not
contribute upstream. Treat upstream as frozen reference code.

## Workflow

- Work on feature branches off `main` (`feat/<thing>`, `fix/<thing>`,
  `chore/<thing>`).
- Merge into `main` locally after the change builds and tests pass.
- **Never open a PR to `The-Swarm-Corporation/swarms-rs`.** No
  upstream relationship.
- The `upstream` remote exists for read-only reference; do not push
  to it, do not fetch+merge from it without an explicit ask.

## Layout

- `swarms-rs/` — upstream library crate. **Don't edit.** Read it to
  understand the framework; copy patterns into `swarms-tetrac` if
  needed.
- `swarms-macro/` — upstream `#[tool]` proc macro. Don't edit.
- `swarms-tetrac/` — our integration crate. **Edit here.**
- `tetrac-integration.md` — the PRD. Source of truth for design
  decisions; update when scope shifts.
- `py_vs_rust.md` — research notes on swarms.ai's Python SDK vs
  swarms-rs. Keep separate from the PRD.

## Dependencies

- `skill-trading` is pinned by git rev to
  `github.com/tetrac-official/rust-cli-ttc-api`. Bump the `rev =`
  in `swarms-tetrac/Cargo.toml` to pull updates. Switch to a
  `tag =` if/when that repo cuts releases.

## Build

```sh
cargo check -p swarms-tetrac      # quick check
cargo build --workspace            # full workspace build
cargo test -p swarms-tetrac        # unit tests (D9+)
```

The 17 warnings under `swarms-rs/src/structs/` are pre-existing
upstream lifetime style nits. Ignore.

## Defaults

- No comments in code unless the *why* is non-obvious. Names
  document the *what*.
- No new `*.md` files unless explicitly asked.
- No emoji unless explicitly asked.
- `dry_run: true` is the default for any tool that places, cancels,
  or modifies orders. Flip via `TTC_DRY_RUN=false`.
