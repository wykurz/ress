# Budgeted scanning

A pager for huge files must never let one keypress read an unbounded amount
of data. `ress` enforces this structurally: the engine has exactly three
read loops, they live in one module (`ress-core/src/scan.rs`), and each
takes a **mandatory byte budget**. A scan without a bound cannot be written,
which is the point — early revisions had five hand-rolled read loops, and
"each loop manages its own bound" reliably decayed into "some loops forgot."

## The primitives

- `fill_lines(from, rows, budget)` — the viewport's single-call read:
  collect bytes for a screenful, stopping at the rows-th newline, EOF, or
  the budget.
- `ForwardScan::new(from, n, chunk)` — a resumable scan for the `n`-th line
  start after `from`. Each `step` consumes one chunk and returns `Found`,
  `Eof` (the last real line start the scan saw, or `from` itself if it saw
  none — always a definitive anchor, never a sentinel), or `More` to
  continue.
- `BackwardScan::new(pos, n, chunk)` — a resumable scan for the `n`-th
  newline above `pos`. The search window is captured once, at construction:
  the byte at `pos - 1` is excluded, because for a line-start `pos` it is
  the newline that made it a line start, and for `pos` at EOF it is a
  trailing newline that must not count as a line above. Each `step` returns
  `Found`, `Top` (fewer than `n` newlines exist in the window; the answer is
  anchor 0), or `More`.

`ForwardScan` and `BackwardScan` are the engine's only navigation read
loops, and the cursor lives inside the object rather than being handed back
to the caller as a value — an interactive attempt and the pending
continuation it falls back to are literally the same scan, stepped further,
never a fresh scan re-derived from a resume cursor. All navigation
(`scroll_lines`, `goto_end`, `goto_percent`, `goto_line`) and the viewport
itself compose `fill_lines` and these two scans; `goto_line` additionally
consults the background line index to pick its scan's starting checkpoint
(see "Line index lookups reuse `ForwardScan`" below). Outcomes are explicit
enums — a caller must decide what `More` means for its operation; there is
no silent fallthrough.

## The budget contract is block-granular

Budgets bound **read work**, not the byte position of answers. Reads happen
in cache blocks; once a block is in hand, scanning its in-memory bytes is
free, so a result found in an already-fetched block is returned even if it
lies past the nominal byte budget. Each scan's I/O stays within one block of
its budget, and an operation's *interactive attempt* composes at most a
small, fixed number of scans — a percent jump, for instance, probes one
block for a direct hit and, failing that, scans backward once — so what a
keypress does before returning is bounded by a small multiple of the
budget. A *pending background completion* is bounded only by the file
itself, by design: it runs in budget-sized, cancellable chunks with visible
progress until the true answer. An answer from paid-for bytes always beats
clamping to a worse one.

## What happens when a budget runs out

The guiding invariant: **the viewport top is always a line start.** A
budget-exhausted scan never lands the anchor somewhere that breaks this —
instead, the operation becomes **pending**: the interactive attempt returns
immediately, the anchor stays put, and a background task keeps stepping the
same scan in budget-sized chunks (each chunk an abort point), publishing
progress. The UI shows a transient bottom-row indicator; `Esc` cancels (the
anchor never moved), any new motion supersedes, and completion moves the
viewport to the true answer:

- **Scrolling down** resolves to the requested line start, clamped at EOF's
  last line start when the file ends first.
- **Scrolling up** resolves to the requested line start, clamped at the top.
- **Jumping to the end** finds the true tail — including the case where the
  final line's start lies beyond any single budget, and the case where the
  screenful walk above it does.
- **Percent jumps** resolve to the start of the line containing the target
  byte, via a backward search from that byte (never a forward one), so the
  destination does not depend on the scan budget.

A property test drives every one of these operations both to completion
with an unlimited budget and through the pending path with a tiny one, on
randomized data, and asserts the two land on the same anchor — the
permanent regression oracle for the pending machinery.

Pending resolution triggers whenever a single motion needs more scanning
than the interactive budget (default 8 MiB): pathological content (a single
line larger than the budget), but also very large requested motions over
ordinary files — a `100000j` whose target lies further away than the budget
pends just the same, briefly. One honesty note: the progress percentage is
computed against the operation's scan span and saturates at 99% until the
answer is ready. A multi-stage jump (end-of-file with a walk-up stage over
the rows above the tail) computes and publishes each stage's percentage
against that stage's own span, so the indicator is not one continuous climb
across the whole operation — each stage reads relative to itself.

## Line index lookups reuse `ForwardScan`

The background line index stores one checkpoint every 1024 lines, not
every line, so resolving a line number to a byte offset means walking
forward from the nearest checkpoint. That walk is the same `ForwardScan`
used by navigation — constructed with the checkpoint's byte offset as
`from` and the remaining line count (at most 1023) as `n` — so it gets the
same budget discipline and pending machinery for free: an interactive
query either finishes within budget or becomes the pending continuation of
that exact scan, stepped further in the background with published
progress.

Checkpoint offsets are values the index computed mid-scan, not positions
re-derived from the current file on every use. `ForwardScan::step` clamps
its cursor and origin to the file size at the start of every step
regardless of what the caller already checked, so a checkpoint offset that
lands out of contract degrades into an in-file answer instead of scanning
past EOF.

## Layout is budgeted too

The same philosophy applies one layer up: `layout_row` (tab expansion,
Unicode width, control-byte carets, horizontal windowing) stops building
cells at the window's right edge, so rendering a window into a long line
costs the window, not the line. The horizontal offset itself is capped at
the type level (`HScroll`), which keeps the viewport's derived scan budget
finite for any input.
