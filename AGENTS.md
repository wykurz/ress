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
- **Tests prove events, not scheduler timing:** never assert on how fast the
  OS happened to run a test (an elapsed `Duration` compared against a tuned
  threshold, a call's own duration used as a stand-in for "did that block").
  Assert on a kernel-verified fact, an injected answer, or a real handshake
  instead — see `ress-perf/src/runner.rs`'s own `run_sample` doc comment for
  two real bugs this caught. Not every `Duration` in a test is this rule,
  and treating all three alike both over- and under-flags: a sleep that
  GENERATES the subject's own behavior (a mock's injected latency, a fake
  subject's paced writes) is not an oracle at all, nothing is being
  discriminated; an `elapsed()` that IS the reported measurement (the actual
  product a test exists to check) has nothing else to discriminate against;
  and a bound wide enough that no plausible correct-but-slow run could cross
  it — checking a qualitative "responded promptly" vs. "sat out the whole
  timeout" gap, not a threshold tuned near the real mechanism's own latency
  — cannot false-pass a genuine regression. The banned shape is narrower and
  specific: a sleep or comparison whose OUTCOME decides whether the test can
  tell buggy code from correct code, so that a slow host can make both pass
  (or a fast one make both fail) for reasons unrelated to the property under
  test. PR #44 rounds 16-17 found and fixed two of these hiding among
  otherwise-correct-shaped, pre-existing tests — a stale sleep-then-assert
  predating this rule (`prefetch.rs`), and a sleep-then-kill from a
  SEPARATE THREAD racing a single-threaded poll loop's own internal
  ordering (`runner.rs`) — neither one an obviously wrong-shaped test until
  traced through to how a loaded host could make it pass for the wrong
  reason.
  The oracle-hardening campaign is **closed** (2026-07-22 policy): the
  `no_timing_oracles` guard is the floor, not a ratchet. A bounded absence
  window layered over a structural fix is an accepted residual when no
  positive terminal signal exists — mark it `ACCEPTED-RESIDUAL:` with the
  reason, and do not sweep for new variants of the class.
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

**Docs claim only enforced invariants** (2026-07-22 policy): a numeric
bound, ordering, or resource guarantee stated in `docs/*.md` must be
enforced by code (ideally pinned by a test); anything aspirational is
labeled design intent or a known gap. Adopted after a doc claimed a read
bound the code did not enforce.
