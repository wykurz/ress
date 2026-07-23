# Architecture

`ress` is built as a thin terminal binary over a fat, headless engine. This
document describes the durable, architecture-level decisions; the per-module
documents ([block cache](block_cache.md), [budgeted scanning](budgeted_scanning.md),
[prefetch](prefetch.md), [concurrency](concurrency.md)) go deeper on each
subsystem.

## Crate layout

- **`ress`** — the binary: clap CLI, terminal setup/teardown, the event loop,
  the keymap, and blitting rendered rows into ratatui. It contains as little
  logic as possible; the one non-trivial piece (`handle_key`, the mode
  machine) is a deterministic, IO-free state machine — no terminal, no I/O
  — exhaustively unit-tested.
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
target byte for the start of the line that contains it. The index has two
consumers, resolving opposite directions of the same checkpoint lookup:
line-number navigation (`goto_line`), covered in Seams for what comes
next below, and the status line's current-line display, resolved by a
background `StatusWorker` and covered in The status line, below.

## The event loop

The binary runs a `tokio::select!` loop over six sources: crossterm's
async event stream, the completion of a pending navigation scan, the
background index's frontier advancing, the status worker's snapshots
advancing (`ress_core::status`, see The status line, below), the search
sweep's own `SearchSummary` snapshots advancing (see [search](search.md)),
and a 100ms repaint tick that is always armed rather than only while
something is pending. Key presses resolve through the pure `handle_key`
state machine into an `Action`; a navigation key carries a `Nav` intent
into `apply_nav` — one that resolves within its budget applies
immediately, and one that cannot becomes the pending operation, the
viewport staying put while a transient bottom-row indicator shows
progress. Committing a search (Enter on a non-empty `/`/`?` buffer)
resolves to `Action::CommitSearch` instead: the run loop, not
`handle_key`, compiles the typed pattern where `Document` lives
(`handle_key` itself stays IO- and regex-free) — a bad pattern surfaces as
a one-line notice and nothing else happens; a good one starts the
background sweep (`Document::start_search_sweep`) and feeds a
`Nav::SearchNext` through that same `apply_nav` path to land the first
match. A jump to the end that also has to walk up several rows runs that
walk as a second stage with its own progress span, so the indicator is not
one continuous climb across the whole operation.

The frontier arm, the status arm, and the search-summary arm keep the tick
informed rather than drawing themselves: each raises a `dirty` flag when
its background task has published something new. The frontier and status
arms gate on the status line specifically being this frame's bottom-row
occupant (`ChromeLayout::status_visible`) — not just a chrome row existing
at all, since a row that exists but is currently showing the `:` command
line instead has just as little to repaint for as a hidden one does; the
search-summary arm gates on the wider `ChromeLayout::search_summary_visible`
instead, since a sweep tick can also move the scrollbar gutter's
match-density ticks while the status segment itself stays covered by a
notice or the command line. The tick — firing every 100ms unconditionally,
dirty or not, pending or not — is the loop's sole point that turns a
raised flag into a repaint, also redrawing while a navigation is pending,
gated the same specific way (`ChromeLayout::progress_visible`): a pending
nav's progress row is exactly as invisible when Command's line covers it
as it is on a chromeless terminal. Every draw call site clears `dirty`
itself immediately afterward; the three `watch` subscriptions are what
re-arm it, not a signal threaded back through `draw`'s own return value —
see [concurrency](concurrency.md) for why that coalescing stays correct
with nothing more than a bool and a timer. A burst of index, status, or
search-summary progress between two ticks still costs at most one extra
repaint, and a document that is both idle (no pending navigation, no
frontier, status, or search-summary movement) and fully resolved leaves
the tick firing every 100ms but painting nothing.

The pending slot holds its scan behind a cancel-on-drop guard (see
[concurrency](concurrency.md) for the shape this and the other background
tasks share, including the one blocking-read exception all four carry):
`Esc`, a new motion superseding it, quitting, the event stream ending, and
any error unwinding through the loop's `?` all drop the slot rather than
detach the task, so every exit path aborts the scan at whichever ordinary
`.await` it is currently suspended at. A resize drops it the same way and
re-applies the originating intent against the new dimensions (a `G` in
flight must not finish positioned for the old screen height). Terminal
state (alternate screen,
raw mode, mouse capture) is owned by a guard that restores it on every
exit path, including panics.

Keys typed before the first paint are not discarded — investigated for
issue #30, where they looked discarded because the one key most likely to
be typed pre-paint, Enter, silently is. `Term::new`'s `enable_raw_mode`
call is the first thing `run` does, but a process still takes some nonzero
time (dynamic linking, CLI parsing, opening the file) to get there, and a
terminal typed into during that window is still in its default cooked
mode: POSIX `ICRNL` translates Enter's CR byte to LF the instant the byte
is *received*, not when it's later read. That translated LF sits in the
kernel's tty buffer and survives the later raw-mode transition completely
intact (confirmed empirically, not assumed — FIONREAD, a raw `poll`+`read`,
and crossterm's own `EventStream` all see it, unmutated, once raw mode is
on) — so nothing is actually lost at the kernel or crossterm-plumbing
level, and every other key (digits, letters, arrows) survives the same
transition unchanged, since `ICRNL` only touches CR. The break is
downstream, in how that surviving LF gets interpreted: crossterm decodes a
bare LF as `Enter` only when raw mode is *not currently* active at parse
time (its own source cites its issue #371 on this), which is never true
here, since parsing never starts before `Term::new` has already flipped
raw mode on — so the byte decodes as Ctrl+J instead. `handle_key_command`
binds Ctrl+J as an Enter alias for exactly this reason (Ctrl+J has no
other meaning in ress), which is enough: a `:N` typed and submitted before
first paint now commits exactly like one typed after. See
`App::handle_key_command`'s Ctrl+J comment for the full mechanism and the
regression pins (`ress::app::tests::command_ctrl_j_jumps_exactly_like_enter`,
`smoke::colon_command_typed_before_first_paint_still_jumps`).

Process exit is bounded rather than waiting on every background task (see
[concurrency](concurrency.md)): a read the foreground is actively
awaiting — a cold viewport or navigation block — still holds the loop,
including the `q` that would quit, until it returns; making foreground
reads cancellable too is the planned Resolution-for-reads work.

## The mode machine and the chrome stack

`handle_key` routes every key press by `Mode` — `Normal`, `Command`, or
`Help` — before deciding what it means: each mode consults its own key
table, so a binding in one mode has no effect in another. `Help` is the
simplest: any key at all closes it back to `Normal` without ever reaching
`Normal`'s own table, which is why `q` pressed inside help cannot quit the
pager — it just closes help, like any other key would. `Command` carries a
payload, `CommandKind` (`Colon` or `Search { backward }`), that selects
which of two grammars its one shared `command` buffer is read through —
`:` enters `Colon`, `/` and `?` enter `Search` with `backward` set to
which of the two was typed. `Colon` owns the `:N` line-jump prompt: digits
accumulate into a buffer capped at 20 characters (more than a `u64` line
number can ever need), Backspace edits it, and Enter commits it directly
as a `Nav::Line` — an all-digit buffer's only parse failure is overflow,
and `goto_line`'s vim-style clamp already treats an overflowed value the
same as a past-the-end line number, so there is no separate error path to
render. `Search` owns the `/`/`?` pattern prompt instead: the buffer holds
arbitrary text rather than digits only, with no length cap of its own (a
regex pattern isn't bounded the way a line number is), and Enter on a
non-empty buffer commits it as `Action::CommitSearch` rather than a `Nav`
directly, since compiling the typed text into a pattern needs `Document`
— that step happens in the run loop, not here (see The event loop, above,
and [search](search.md)). Esc, or Enter on an empty buffer, cancels back
to `Normal` for either grammar, clearing the buffer. Every buffer edit
repaints immediately, unlike `Normal`'s count prefix below: the buffer is
on screen, so leaving an edit unpainted would freeze the row while
keystrokes kept registering underneath it.

The mouse wheel never reaches this router — `run`'s own event loop
decides its effect directly, since a wheel tick is not a key event at
all — but the same invariant governs it: wheel navigation routes only
where navigation is owned, `Normal`, exactly where `j`/`k` do. In
`Command` or `Help` a tick is a true no-op: it does not close `Help`,
abort the command being composed, or touch a navigation already
pending — a scan started before the tick keeps running, untouched,
exactly as it would have before an unrelated key.

`Normal`'s own table carries one more piece of state across key presses:
an optional numeric count, built one digit at a time and spent as a
multiplier by the motion that consumes it. What survives a given key
press is a rule, not a per-key list: only a digit that extends the count,
or a motion that spends it, lets it live past that key — everything else
drops it, matching vim's own behavior. An unmapped key, `Ctrl-l`,
entering `Command` or `Help`, and any motion with no count semantics of
its own (half-page and full-page among them) all clear it on the way
through, and a `g`-chord that does not complete as `gg` or `ge` aborts
both the chord and whatever count was armed for it. "Entering `Command`
or `Help`" means a *successful* entry specifically: a `:`/F1 the
terminal refuses is as if the key had never been pressed at all, so it
changes nothing — count, chord, and all — leaving both exactly as they
were for the next key to build on or complete. `Command` needs a bottom
chrome row and `cols >= 3` (room for `:` plus one digit plus the
cursor); `Help` needs `rows >= 7` and `cols >= 5` (`Block::bordered`'s
own interior, once its border and the panel's one-cell margin are both
subtracted, needs at least one cell on each axis) — below either bound
the key is refused as just described.

The terminal's bottom rows are chrome, not content, reserved in steps
rather than a flat amount: `chrome_rows` gives a terminal of 4 rows or
more both the hint bar and the bottom row (2), one with 2 or 3 rows the
bottom row alone (1 — unchanged from before the hint bar existed), and
one with 0 or 1 rows no chrome at all, so its lone row still goes to
content instead of being lost to chrome nobody could read anyway.
`content_rows` is simply `rows - chrome_rows(rows)`, applied everywhere
terminal rows enter the loop — the pre-draw viewport fetch, row-dependent
jumps like `G`, and the live frame area inside `draw` alike — so none of
them can drift from what the reservation actually applies. The hint bar,
when shown, is a persistent DIM row one above the bottom row, naming the
current mode's own keys — a condensed cheat-sheet in `Normal`, the edit
keys in `Command`, "any key closes" in `Help` — DIM rather than reversed
so it reads as distinct chrome, not a second status line. The bottom row
itself shows exactly one of four things, in precedence order: the command
line (`:`, `/`, or `?`, depending on `CommandKind`) while `Command` is
composing one, else a one-line transient notice ("search wrapped",
"pattern not found", a bad-pattern error) while `App.notice` holds one,
else the transient progress row while a navigation scan is pending (see
[budgeted scanning](budgeted_scanning.md)), else the status line (below).
A notice outranks the progress row rather than the reverse specifically so
a fresh search-error notice can never sit invisible behind an unrelated,
already-running navigation's progress row — but still yields to
`Command`, since composing a command stays uninterrupted either way.
The help overlay paints last, over the content area only: `Clear` first,
so no content already painted there bleeds through, then a bordered
`Block` holding the keymap reference — both chrome rows stay visible
beneath it exactly as they were before `Help` opened.

The content area's own text width narrows by one column whenever a
scrollbar gutter is shown on its right edge: `content_cols(size, rows,
cols)` cedes a column only when there is a file to mark a position in
(`size > 0`), a content row to draw it on (`rows > 0`), and a second
column left over for text once the gutter's own is spoken for (`cols >=
2`) — an empty file or a one-column terminal keeps the full width
instead. The gutter itself draws exactly one of three cells per row, the thumb
checked first: a plain `█` cell marking where the viewport's top anchor
falls, proportional to its byte offset through the file — a position
marker, not a proportional-height thumb: the viewport's own byte span
isn't cheaply known at render time, so sizing a thumb to it would need a
second scan or a rough estimate presented as exact — else a DIM `·` on any
row a live search's match density marks (`tick_rows`, mapping
`SweepAnalysis`'s 1024-bucket histogram onto screen rows — see
[search](search.md)), else a plain DIM `│` track cell. Search is no
longer a future addition to this gutter (see Seams for what comes next,
below): its background sweep is the ticks' own source today.

## The status line

Once it wins the bottom row's precedence (see The mode machine and the
chrome stack, above), the status line reads
`{name} · L{n}[/{total}] · {pct}%`, with an empty file reading
`{name} · empty · {pct}%` instead — recomputed on every draw. `pct` is
the anchor's byte offset as a percentage of the file size, unrelated to
indexing progress and always available; `n` and `total` come from the
background index (the indexing mechanics are in Seams for what comes
next, below) through queries that resolve on their own schedules, so
every combination of known and unknown is a real, displayed state. `draw`
skips the status line's queries entirely, requesting nothing and reading
neither `n` nor `total`, in every case where the row will not show it:
while the command line or the progress row claims it instead, and on a
terminal too short to have a chrome row at all (`chrome_rows(rows) > 0`,
next to `content_rows`) — in every case, not only would the result go
unused, but the request that would otherwise wake the status worker would
be spent on a number nobody will ever see.

The current line comes from `StatusWorker` (`ress_core::status`), one
background task per document answering "what line is this anchor on" —
see [concurrency](concurrency.md) for the shape it shares with the index
scan and pending navigation, and why a fresh anchor arriving mid-walk
needs no reconciling against whatever the worker was doing. `draw` never
drives the count itself — every frame it calls `request_line_number` (an
anchor send) and `line_number` (a snapshot read), both synchronous,
infallible, and independent of whether the calling thread has a tokio
runtime bound to it at all, so this can never stall on a cold read or
panic outside a runtime the way a foreground read once could. The
worker's own loop is where the real work happens: pull the latest anchor,
wait for the background index to cover it, walk a budgeted `CountScan`
(see [budgeted scanning](budgeted_scanning.md)) from the nearest
checkpoint, and publish a snapshot after every state change along the
way.

The worker's coverage gate mirrors `goto_line`'s own reasoning, aimed the
other way: an anchor must be covered — the index has processed strictly
past its byte offset — before a checkpoint lookup for it can be trusted,
and coverage is judged by `processed_up_to` alone, never by `done`, for
the same reason `goto_line`'s clamp is — an error-shortened scan (see
Seams for what comes next, below) can finish with bytes past its
frontier permanently unindexed, and a dead scan's `processed_up_to` never
advances again. While the index has not yet caught up and the scan is
still running, the worker publishes `Converging` and waits on the scan's
own frontier — a real difference from treating an uncovered anchor as a
dead end: the worker resumes on its own the moment coverage lands,
needing no draw or other external trigger to ask again. Only once the
scan has finished *without* ever covering the anchor does the worker give
up and publish `Unavailable`, honestly and permanently, since nothing is
coming. An empty file needs none of this: covered trivially, the worker
resolves it to line one immediately, without touching the scan or the
cache at all.

Past the gate, the worker counts newlines from the nearest checkpoint to
the anchor with a `CountScan`, budgeted by the same `nav_scan_budget`
config value navigation scans use. A window wider than one budget chunk
does not restart from the checkpoint on every internal step: the scan
object's cursor lives inside the worker's own loop, resumed step after
step until it is `Done`, so convergence across a wide window is bounded
background work, not bytes re-examined from scratch. Each step reads
through `warm()`, not `block()` (see [block cache](block_cache.md)): the
worker is a background consumer racing the interactive viewport for the
same cache, and must not promote or reorder the working set any more than
the index scan does. A step that fails to read its block counts as one
failure and, below a bounded retry ceiling, is retried after a short
backoff — long enough that the retry itself, like the walk's own
progress, has to be raced against a fresh anchor inside the same
`select!`, so a worker stuck backing off is exactly as supersedable as
one mid-fetch. Past the retry ceiling the anchor gives up for good —
`Unavailable` — and the worker parks until a different anchor is
requested: no further reads, no respawned attempt, ever, for an anchor
nobody is asking about anymore.

This leaves the unresolved states — covered but still mid-walk,
retrying after a failed read, given up as `Unavailable`, or simply
stale because the worker has not yet answered the latest request (its
anchor does not match `app.top`, indistinguishable from
still-converging at the render layer). While the background index is
still running, every one of them reads `indexing… {k} lines`; once the
scan has finished, they read `L?` instead — the index can no longer be
the explanation, so the row stops implying it is. `status_text` reads `done` off the frontier directly to
tell an unresolved `L?` apart from `indexing…`, rather than inferring
"still indexing" from `total.is_none()`: post error-shortened scan,
`total` stays `None` forever, so that equivalence does not hold. The run
loop repaints an unresolved `L?` on its own, subscribed to
`status_snapshots` exactly like it already subscribes to the index's
frontier (see The event loop, above), so it keeps updating for as long as
it takes to settle. Before the anchor converges at all — briefly
possible right after a document opens, even one that ends up indexing
almost instantly, since the very first
draw can land before the scanner has ingested anything, and lasting for
as long as the background scan itself keeps running — the row reads
`indexing… {k} lines` instead, `k` being `Frontier::lines_so_far`, the
newlines seen so far. That count, not a derived total, is what's shown:
reusing it as a total mid-scan would repeat the trailing-newline class of
off-by-one that `total_lines`'s own end-of-scan hedge exists to correct.

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

See [concurrency](concurrency.md) for how the engine's background tasks —
the index scan, the status worker, pending navigation, the search sweep —
are shaped around these rules.

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
  above), with the same one blocking-read exception (see
  [concurrency](concurrency.md)) — and no closed document leaves a
  background reader running as a tracked task, though real files can leave
  at most one already-in-flight positioned read finishing on its own. The
  scan feeds `LineIndex`: a sparse table of the byte offset of every
  1024th line start (~8 MB of checkpoints indexes a billion lines),
  publishing progress after every ingested block through a watch channel
  (the "frontier"). One subtlety worth stating precisely: a line "start"
  that lands exactly at EOF is a trailing newline's phantom, not a real
  line; `goto_line` guards against ever resolving there, redirecting to
  the line before it. Search is no longer a future analyzer: its
  background match-summary sweep (`SweepAnalysis`, in `search.rs`) is the
  first consumer of a second, generic analysis driver
  (`crate::analyzer::spawn`, generalized out of `StatusWorker`'s own loop
  shape — see The status line, below) that owns reset-on-a-fresh-request
  supersession, bounded steps, retry-with-backoff, and
  give-up-and-degrade-to-a-floor as one reusable shape, so an analyzer
  supplies only the work, never the loop. `Document::start_search_sweep`
  spawns it — eagerly for `Document::new`, lazily on its own first call
  otherwise. The two scans share this cache — a byte range one of them
  has already read is a cache hit for the other, never a second disk
  read — but not one single pass: the sweep walks the file again, on its
  own schedule, rather than riding this one. Syntax highlighting remains
  the one analyzer still ahead. See [search](search.md) for the sweep's
  own bounds.
- **`goto_line`** is bound to `<count>G`, `<count>gg`, and the `:N`
  command line, and resolves through the index three ways.
  If the index already covers the target line, it walks
  forward from the nearest checkpoint with a budgeted `ForwardScan` (at
  most 1023 newlines — see [budgeted scanning](budgeted_scanning.md)):
  Ready if the walk finishes in budget, Pending on that same walk if not.
  If the scan has finished and the line does not exist, it clamps to the
  last real line, vim-style. Otherwise — the target is past what the scan
  has covered so far, and the scan is still running — it pends on the
  frontier, reporting the scan's own progress as the bar until coverage
  catches up, then walks as above.
- **Pending navigation** exists: `Resolution { Ready(NavOutcome),
  Pending(PendingNav) }` is how every navigation result is expressed
  today, and budget-exhausted jumps continue as cancellable background
  scans with live progress (see [budgeted scanning](budgeted_scanning.md)).
  `NavOutcome` is `At(Anchor)` for an ordinary jump — the only variant
  `goto_line` and its siblings ever resolve, since `goto_line` clamps a
  past-the-end target rather than failing — `FoundMatch { top, match_at,
  wrapped }` for a search jump (`top` the matched line's own anchor,
  `match_at` its byte offset — the next search's own origin and the
  highlight target — `wrapped` whether the jump crossed an end to get
  there), or `Exhausted` when no answer exists anywhere: search's own
  "pattern not found," reached once neither leg of its two-leg wrap
  policy finds a match (see [search](search.md)) — the variant's real,
  not merely reserved, first use. Making *reads themselves* pending (a
  cold viewport block on a wedged mount still holds the loop) is the
  remaining step.
