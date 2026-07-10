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

Absolute line numbers are a separate, background concern, tracked by the
`LineIndex` analyzer (Seams for what comes next, below, has the
mechanics). Jumps that are naturally byte-addressed (`G`, `<count>%`)
never wait for it: they snap to a line start with a small local backward
scan — `G` searches back from EOF for the last line (and further back to
walk up any additional rows the screen needs); `%` searches back from the
target byte for the start of the line that contains it. Line-number
navigation (`goto_line`) is the index's one consumer, and its resolution
flow is covered in that section.

## The event loop

The binary runs a `tokio::select!` loop over three sources: crossterm's
async event stream, the completion of a pending navigation scan, and a
repaint tick (active only while something is pending) that keeps the
progress indicator fresh. Key presses resolve through the pure `handle_key`
state machine into `Nav` intents; an intent that resolves within its budget
applies immediately, and one that cannot becomes the pending operation — the
viewport stays put and a transient bottom-row indicator shows progress. A
jump to the end that also has to walk up several rows runs that walk as a
second stage with its own progress span, so the indicator is not one
continuous climb across the whole operation.

The pending slot holds its scan behind a cancel-on-drop guard: `Esc`, a new
motion superseding it, quitting, the event stream ending, and any error
unwinding through the loop's `?` all drop the slot rather than detach the
task, so every exit path aborts a live scan. A resize drops it the same way
and re-applies the originating intent against the new dimensions (a `G` in
flight must not finish positioned for the old screen height). The cache's
own in-flight guard (see [block cache](block_cache.md)) means a scan
aborted mid-fetch cleans up its own in-flight registration instead of
leaking it. Terminal state (alternate screen, raw mode, mouse capture) is
owned by a guard that restores it on every exit path, including panics.

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

The architecture keeps future work additive rather than invasive.
Background analysis, and `goto_line` as its first consumer, is the one
seam already filled below; the rest are still ahead:

- **`BlockSource`** is the I/O trait (positioned reads, known size). The
  current implementation is a blocking `pread` pool; an io_uring backend is
  a drop-in replacement to be adopted by benchmark, not by default.
- **Background analysis** has its first real analyzer. `Document::new`
  spawns a `ScanScheduler` — construction must happen inside a tokio
  runtime, since spawning is how the scan starts — which runs one
  sequential pass over the file through the shared block cache, calling
  only the cache's `warm()` path (see [block cache](block_cache.md)): it
  fills the probationary segment but never promotes, so scanning the whole
  file cannot evict the interactive working set. `Document` owns the
  scheduler, so dropping a document aborts its scan — the same
  cancel-on-drop discipline as pending navigation (see The event loop,
  above) — and no closed document leaves a background reader running. The
  scan feeds `LineIndex`: a sparse table of the byte offset of every
  1024th line start (~8 MB of checkpoints indexes a billion lines),
  publishing progress after every ingested block through a watch channel
  (the "frontier"). One subtlety worth stating precisely: a line "start"
  that lands exactly at EOF is a trailing newline's phantom, not a real
  line; `goto_line` guards against ever resolving there, redirecting to
  the line before it. Search and syntax highlighting remain future
  analyzers meant to share this same single-shared-scan design, so the
  file is read once no matter how many analyzers eventually run.
- **`goto_line`** is the line index's only consumer today (not yet bound
  to a key — `<count>G` is future keymap work), and resolves through the
  index three ways. If the index already covers the target line, it walks
  forward from the nearest checkpoint with a budgeted `ForwardScan` (at
  most 1023 newlines — see [budgeted scanning](budgeted_scanning.md)):
  Ready if the walk finishes in budget, Pending on that same walk if not.
  If the scan has finished and the line does not exist, it clamps to the
  last real line, vim-style. Otherwise — the target is past what the scan
  has covered so far, and the scan is still running — it pends on the
  frontier, reporting the scan's own progress as the bar until coverage
  catches up, then walks as above.
- **Pending navigation** exists: `Resolution { Ready, Pending }` is how
  every navigation result is expressed today, and budget-exhausted jumps
  continue as cancellable background scans with live progress (see
  [budgeted scanning](budgeted_scanning.md)). A planned `Exhausted` variant
  — for "no such answer" — has no consumer yet: `goto_line` clamps rather
  than failing, so search's "pattern not found" is expected to be the
  variant's first real use once search lands. Making *reads themselves*
  pending (a cold viewport block on a wedged mount still holds the loop) is
  the remaining step.
