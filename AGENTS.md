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

## Architecture & documentation

`ress-core` is the headless engine (no terminal deps), `ress` is the thin TUI
binary. Durable design docs live in `docs/` (start with
`docs/architecture.md`); session-scoped specs and phase plans live under
`.claude/superpowers/` (`specs/`, `plans/`) — gitignored artifacts; save new
specs/plans there, never in `docs/`.

**Keep documentation current:** a change that alters user-facing behavior
(keymap, flags) must update `README.md`, and a change that alters an
architecture-level decision (cache policy, budget semantics, navigation
invariants, concurrency rules) must update the relevant `docs/*.md` — in the
same PR as the code.
