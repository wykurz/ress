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
its budget, and an operation composes at most a small, fixed number of scans
(a percent jump, the largest, probes one block, scans forward once, and may
scan backward once) — so every operation's total read work is bounded by a
small multiple of the budget. An answer from paid-for bytes always beats
clamping to a worse one.

## What happens when a budget runs out

The guiding invariant: **the viewport top is always a line start.** A
budget-exhausted scan must never land the anchor somewhere that breaks this,
so each operation has a defined, honest degradation:

- **Scrolling down** clamps to the furthest line start the scan verified —
  you move as far as could be proven, never past it.
- **Scrolling up** stays put when the target cannot be found within budget.
  An arbitrary landing spot would corrupt the viewport; staying put is safe
  and predictable.
- **Jumping to the end** has two budget paths. If the final line's start
  cannot be found within budget, it falls back to the top of the file — the
  operation computes an anchor from scratch and has no "current position"
  to stay at. If the tail is found but walking up a screenful exhausts a
  second budget (an over-budget line just above the last one), it lands
  with the final line at the top of the screen instead of the bottom. Both
  are known rough edges, scheduled to become pending operations.
- **Percent jumps** resolve *backward* to the line containing the target
  byte whenever no line start exists at-or-after the target within reach.
  That backward snap is the ordinary resolution when the target lands
  inside the file's final line, and also the budget degradation — more
  faithful to the request than a forward snap, and never a teleport to EOF.
  Only a region with no newline within budget on either side of the target
  falls back to the top.

Budget-triggered degradations occur whenever a single motion needs more
scanning than the budget (default 8 MiB): pathological content (a single
line larger than the budget), but also very large requested motions over
ordinary files — a `100000j` whose target lies further away than the budget
clamps early just the same. The percent backward snap additionally serves
everyday final-line targets. The planned `Resolution` mechanism upgrades
the budget cases into live-progress, cancellable pending operations; the
enum-outcome shape of the scanners is the seam it plugs into.

## Layout is budgeted too

The same philosophy applies one layer up: `layout_row` (tab expansion,
Unicode width, control-byte carets, horizontal windowing) stops building
cells at the window's right edge, so rendering a window into a long line
costs the window, not the line. The horizontal offset itself is capped at
the type level (`HScroll`), which keeps the viewport's derived scan budget
finite for any input.
