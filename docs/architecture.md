# Architecture

`ress` is built as a thin terminal binary over a fat, headless engine. This
document describes the durable, architecture-level decisions; the per-module
documents ([block cache](block_cache.md), [budgeted scanning](budgeted_scanning.md),
[prefetch](prefetch.md)) go deeper on each subsystem.

## Crate layout

- **`ress`** — the binary: clap CLI, terminal setup/teardown, the event loop,
  the keymap, and blitting rendered rows into ratatui. It contains as little
  logic as possible; the one non-trivial piece (`handle_key`, the keymap state
  machine) is a pure function with unit tests.
- **`ress-core`** — the engine: file I/O, the block cache, scanning,
  navigation, and line layout. It has **no terminal dependencies**, so all of
  the hard logic is testable headless. The property-test suites live here and
  act as the regression net for engine rewrites.

The seam between them is the `Document` API: the binary asks for a viewport
render or a navigation result and never touches bytes, blocks, or scans
directly.

## Offset-driven rendering

The single decision everything else hangs off: **rendering is driven by byte
offsets, not line numbers.**

The scroll position is an `Anchor` — the byte offset of the first visible
line. To paint a screen, the engine reads forward from the anchor until it
has a screenful of lines. That costs a bounded number of block reads
regardless of file size, which is what makes first paint on a 100 GiB file
indistinguishable from a 100 KiB one.

Absolute line numbers are a separate, background concern (a planned
`LineIndex` analyzer); navigation never waits for them. Jumps that are
naturally byte-addressed (`G`, `<count>%`) snap to a line start with a
small local scan — forward to the next line start in the normal case,
backward to the line containing the target when no line start is within
reach ahead of it.

## The event loop

The binary runs a `tokio::select!` loop over three sources: crossterm's
async event stream, the completion of a pending navigation scan, and a
repaint tick (active only while something is pending) that keeps the
progress indicator fresh. Key presses resolve through the pure `handle_key`
state machine into `Nav` intents; an intent that resolves within its budget
applies immediately, and one that cannot becomes the pending operation —
the viewport stays put, a transient bottom-row indicator shows progress,
`Esc` cancels, any new motion supersedes, quitting aborts, and a resize
restarts the pending jump against the new dimensions (a `G` in flight must
not finish positioned for the old screen height). Terminal state (alternate
screen, raw mode, mouse capture) is owned by a guard that restores it on
every exit path, including panics.

Runtime shutdown is bounded: once the event loop returns, the runtime is
shut down with a timeout, so **background** reads wedged on a dead network
mount cannot delay exit. A read the foreground is actively awaiting (a cold
viewport or navigation block) still holds the loop — including the `q` that
would quit — until it returns; making foreground reads cancellable is the
planned Resolution-for-reads work.

## Type-level invariants

Several correctness properties are enforced by construction rather than by
convention, a lesson from early review rounds where scattered integer
arithmetic and implicit invariants kept producing edge-case bugs:

- **`Anchor`** has a private field and is minted only by `Document`
  navigation (or the `TOP` constant). Every anchor is a line start — offset
  0 or the byte after a newline, never at or past EOF — so the renderer
  never has to consider a mid-line viewport top.
- **`HScroll`** (horizontal scroll offset) clamps itself to `MAX_HSCROLL` at
  construction. No downstream arithmetic can see an unbounded column offset,
  which keeps the viewport's scan budget finite for any input.
- **Scan budgets are mandatory parameters** of the scanner primitives — a
  read loop without a bound cannot be written. See
  [budgeted scanning](budgeted_scanning.md).

The engine's property tests pin the behavioral versions of these invariants
(layout never panics on arbitrary bytes and never exceeds the window;
navigation always lands on a line start; viewport reads stay within budget),
so structural rewrites keep the same observable contract.

## Concurrency rules

- The engine uses `std::sync::Mutex` for shared state with one hard rule:
  **a guard is never held across an `.await`.** Lock, decide, unlock, then
  await.
- Cancellation is a first-class case, not an afterthought: any future
  performing a read may be dropped at an await point (prefetch turnover,
  future cancellable jumps), and shared state must self-heal when that
  happens. The cache's coalescing protocol is written to this rule.
- Background work (prefetch fills) is best-effort and bounded: concurrency
  caps and a backlog cap limit how much of it can contend with interactive
  reads (they share the source and blocking pool — contention is limited,
  not eliminated), and it never delays exit beyond the bounded shutdown.

## Keymap philosophy

Vim-first, with two pragmatic influences: helix/zellij ergonomics (`ge`,
bare `d`/`u` for half-page) and `less` compatibility wherever it costs
nothing — a pager has no delete/undo, so `less`'s bare `d`/`u` bindings are
free to adopt. Key slots that vim semantics reserve stay reserved: a leading
`0` is a motion slot (planned: reset horizontal scroll), not a count digit,
which is why `0%` is intentionally not a binding.

## Seams for what comes next

The architecture keeps planned features additive rather than invasive:

- **`BlockSource`** is the I/O trait (positioned reads, known size). The
  current implementation is a blocking `pread` pool; an io_uring backend is
  a drop-in replacement to be adopted by benchmark, not by default.
- **Background analysis** (line index, search, syntax) is designed as
  streaming analyzers fed by a single shared scan through the same block
  cache, so the file is read once no matter how many analyzers run.
- **Pending navigation** exists: `Resolution { Ready, Pending }` is how
  every navigation result is expressed, and budget-exhausted jumps continue
  as cancellable background scans with live progress (see
  [budgeted scanning](budgeted_scanning.md)). The `Exhausted` variant — for
  "no such answer", e.g. a go-to-line beyond the file — arrives with the
  line index. Making *reads themselves* pending (a cold viewport block on a
  wedged mount still holds the loop) is the remaining step.
