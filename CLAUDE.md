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

## DevOps cycle (every change to `swarms-tetrac`)

Run these in order before merging a feature branch into `main`:

1. **Write tests first** for any new module or behavior. Pure-logic
   tests live next to the code in `#[cfg(test)] mod tests`. Tests
   that touch process env vars must serialize via a `Mutex<()>` and
   wrap `set_var` / `remove_var` in `unsafe { ... }` (Rust 2024).
2. `cargo test -p swarms-tetrac` — must be green.
3. `cargo build -p swarms-tetrac` — must succeed without errors.
4. `cargo clippy -p swarms-tetrac --all-targets` — zero warnings
   on our code. Upstream warnings in `swarms-rs/` and `swarms-macro/`
   are pre-existing and ignored; do **not** add `-D warnings` (it
   would fail on those).
5. **If the change adds or modifies a runnable example, run it**:
   `cargo run --example <name> -p swarms-tetrac`. Compilation green
   is not enough — the example must actually produce sensible output
   against live services. Skipping this step is how a "one-line fix"
   ships broken behavior.
6. **Clean up** before committing: remove dead code, unused imports,
   any debug `dbg!` / `println!`, and unrelated whitespace churn.
7. Commit with a focused message; squash sibling fixups into the
   feature commit before merge if it keeps history readable.
8. `git checkout main && git merge <branch>` (fast-forward when
   possible). Then create the next feature branch off `main`.

If any step fails, fix the underlying cause — don't bypass it with
`--no-verify`, `#[allow]` blanket suppressions, or skipping tests.
The one allowed `#[allow(clippy::too_many_arguments)]` is on
`get_hybrid_tickers` because skill-trading's source method already
uses the same allow.

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

## Commit messages

- Subject line ≤72 chars. One line.
- Skip the body. Only add one if there's a non-obvious *why* a
  reader can't see in the diff (e.g. workaround for a specific bug,
  a deliberate trade-off). One sentence max.
- No bullet-point feature lists, no "what changed" recaps — that's
  what the diff is for.

## Env file

- One template only: `.env.example` (gitignored sibling: `.env`).
- Don't ship a `.env.sample` alongside it.
