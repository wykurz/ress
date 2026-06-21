# AGENTS.md

Guidance for AI coding agents. See `README.md` for overview and
`CONVENTIONS.md` for coding conventions.

## Build & Test

Prefer `just` over direct `cargo`:
- `just build` / `just check` / `just fmt`
- `just lint` — fmt check + clippy (warnings denied)
- `just test` — run tests
- `just ci` — lint + test; run before committing

## Critical Conventions

- **Error logging:** always log errors with `{:#}` or `{:?}`, never plain `{}`
  (plain `{}` hides the chain, e.g. "Permission denied").
- **Error chains:** never use `anyhow::Error::msg()` — it destroys the chain.
- Follow `CONVENTIONS.md` for comment/import/version style.

## Architecture

`ress-core` is the headless engine (no terminal deps), `ress` is the thin TUI
binary. See `docs/` for the design and current plan.
