# Budgeted scanning

A pager for huge files must never let one keypress read an unbounded amount
of data. `ress` enforces this structurally: the engine has exactly three
read loops, they live in one module (`ress-core/src/scan.rs`), and each
takes a **mandatory byte budget**. A scan without a bound cannot be written,
which is the point — early revisions had five hand-rolled read loops, and
"each loop manages its own bound" reliably decayed into "some loops forgot."

## The primitives

- `fill_lines(from, rows, budget)` — collect bytes for a screenful,
  stopping at the rows-th newline, EOF, or the budget.
- `nth_line_start_after(from, n, budget)` — forward scan for a line start;
  returns `Found`, `Eof` (with the furthest line start seen), or `Budget`
  (likewise).
- `nth_newline_before(pos, n, budget)` — backward scan; returns `Found`,
  `Top`, or `Budget`.

All navigation (`scroll_lines`, `goto_end`, `goto_percent`) and the viewport
itself compose these. Outcomes are explicit enums — a caller must decide
what `Budget` means for its operation; there is no silent fallthrough.

## The budget contract is block-granular

Budgets bound **read work**, not the byte position of answers. Reads happen
in cache blocks; once a block is in hand, scanning its in-memory bytes is
free, so a result found in an already-fetched block is returned even if it
lies past the nominal byte budget. Each scan's I/O stays within one block of
its budget, and an operation's *interactive attempt* composes at most a
small, fixed number of scans (a percent jump, the largest, probes one block,
scans forward once, and may scan backward once) — so what a keypress does
before returning is bounded by a small multiple of the budget. A *pending
background completion* is bounded only by the file itself, by design: it
runs in budget-sized, cancellable chunks with visible progress until the
true answer. An answer from paid-for bytes always beats
clamping to a worse one.

## What happens when a budget runs out

The guiding invariant: **the viewport top is always a line start.** A
budget-exhausted scan never lands the anchor somewhere that breaks this —
instead, the operation becomes **pending**: the interactive attempt returns
immediately, the anchor stays put, and a background task continues the scan
in budget-sized chunks (each chunk an abort point), publishing progress.
The UI shows a transient bottom-row indicator; `Esc` cancels (the anchor
never moved), any new motion supersedes, and completion moves the viewport
to the true answer:

- **Scrolling down** resolves to the requested line start, clamped at EOF's
  last line start when the file ends first.
- **Scrolling up** resolves to the requested line start, clamped at the top.
- **Jumping to the end** finds the true tail — including the case where the
  final line's start lies beyond any single budget, and the case where the
  screenful walk above it does.
- **Percent jumps** resolve *backward* to the line containing the target
  byte whenever no line start exists at-or-after the target within reach —
  the ordinary resolution when the target lands inside the file's final
  line, never a teleport to EOF.

Pending resolution triggers whenever a single motion needs more scanning
than the interactive budget (default 8 MiB): pathological content (a single
line larger than the budget), but also very large requested motions over
ordinary files — a `100000j` whose target lies further away than the budget
pends just the same, briefly. One honesty note: the progress percentage is
computed against the operation's maximum scan span and saturates at 99% —
in multi-stage jumps (end-of-file with a walk-up stage) the bar can sit at
99% through the final stage.

## Layout is budgeted too

The same philosophy applies one layer up: `layout_row` (tab expansion,
Unicode width, control-byte carets, horizontal windowing) stops building
cells at the window's right edge, so rendering a window into a long line
costs the window, not the line. The horizontal offset itself is capped at
the type level (`HScroll`), which keeps the viewport's derived scan budget
finite for any input.
