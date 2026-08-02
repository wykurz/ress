# CLAUDE.md

Project conventions live in [AGENTS.md](AGENTS.md) — read that first. This file carries
standing practices for how changes are made.

## After fix rounds, look for the structural fix

When a change or a review batch produces two or more rounds of corrective fixes in the same
area, stop patching and run a structural review before taking the next batch: read the
accumulated fixes as a defect *history*, name the recurring classes (not the instances), and
ask what restructuring would make each class unrepresentable. Rust's type system and ownership
patterns are the first tools to reach for:

- newtypes for unit-bearing quantities (absolute offsets vs buffer-local indices, bytes vs
  columns) so mixed-unit arithmetic doesn't compile;
- enums/typestate in place of accumulating boolean flags, so illegal state combinations are
  unrepresentable and every transition is a `match` the compiler audits;
- RAII guards for operations that must happen in pairs (register/publish, charge/return),
  so the forgotten second half is impossible rather than reviewed-for;
- witness types for facts that must be earned, not assumed (an EOF claim requires holding the
  value only a real short-read can mint);
- routing effectful calls through a type that owns the invariant (reads that cannot happen
  without charging a budget), so the invariant-violating call simply doesn't exist.

Prefer making the next bug impossible over making the current bug fixed; the accumulated test
suite is the durable asset that survives the restructure and referees it. Run the analysis
with parallel read-only agents over separate areas or aspects, each pinned to a frozen sha
(never a live working tree), and synthesize before changing anything. This applies to all
substantive changes, not only to escalations: a feature that lands clean still gets the
"which classes could recur here" pass before its PR closes.
