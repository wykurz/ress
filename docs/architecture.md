# Architecture

`ress` is built as a thin terminal binary over a fat, headless engine. This
document describes the durable, architecture-level decisions; the per-module
documents ([block cache](block_cache.md), [budgeted scanning](budgeted_scanning.md),
[prefetch](prefetch.md), [concurrency](concurrency.md)) go deeper on each
subsystem.

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
target byte for the start of the line that contains it. The index has two
consumers, resolving opposite directions of the same checkpoint lookup:
line-number navigation (`goto_line`), covered in Seams for what comes
next below, and the status line's current-line display, resolved by a
background `StatusWorker` and covered in The status line, below.

## The event loop

The binary runs a `tokio::select!` loop over five sources: crossterm's
async event stream, the completion of a pending navigation scan, the
background index's frontier advancing, the status worker's snapshots
advancing (`ress_core::status`, see The status line, below), and a 100ms
repaint tick that is always armed rather than only while something is
pending. Key presses resolve through the pure `handle_key` state machine
into `Nav` intents; an intent that resolves within its budget applies
immediately, and one that cannot becomes the pending operation — the
viewport stays put and a transient bottom-row indicator shows progress. A
jump to the end that also has to walk up several rows runs that walk as a
second stage with its own progress span, so the indicator is not one
continuous climb across the whole operation.

The frontier arm and the status arm keep the tick informed rather than
drawing themselves: each raises a `dirty` flag when its background task
has published something new, gated on a chrome row existing at all (a
hidden row has nothing to repaint for), and the tick — firing every 100ms
unconditionally, dirty or not, pending or not — is the loop's sole point
that turns a raised flag into a repaint, also redrawing (chrome-gated the
same way) while a navigation is pending, since a pending nav's progress
row is exactly as invisible on a chromeless terminal as the status line
is. Every draw call site clears `dirty` itself immediately afterward; the
two `watch` subscriptions are what re-arm it, not a signal threaded back
through `draw`'s own return value — see [concurrency](concurrency.md) for
why that coalescing stays correct with nothing more than a bool and a
timer. A burst of index or status progress between two ticks still costs
at most one extra repaint, and a document that is both idle (no pending
navigation, no frontier or status movement) and fully resolved leaves the
tick firing every 100ms but painting nothing.

The pending slot holds its scan behind a cancel-on-drop guard (see
[concurrency](concurrency.md) for the shape this and the other background
tasks share): `Esc`, a new motion superseding it, quitting, the event
stream ending, and any error unwinding through the loop's `?` all drop
the slot rather than detach the task, so every exit path aborts a live
scan. A resize drops it the same way and re-applies the originating
intent against the new dimensions (a `G` in flight must not finish
positioned for the old screen height). Terminal state (alternate screen,
raw mode, mouse capture) is owned by a guard that restores it on every
exit path, including panics.

Process exit is bounded rather than waiting on every background task (see
[concurrency](concurrency.md)): a read the foreground is actively
awaiting — a cold viewport or navigation block — still holds the loop,
including the `q` that would quit, until it returns; making foreground
reads cancellable too is the planned Resolution-for-reads work.

## The status line

The terminal's bottom row is reserved chrome, not content: `content_rows`
gives the viewport `rows - 1`, except that a 1- or 0-row terminal has no
row to spare, so content keeps the whole area rather than losing its only
row to chrome. That reserved row shows exactly one of two things: the
transient progress row while a navigation scan is pending (see
[budgeted scanning](budgeted_scanning.md)), or otherwise the status line —
`{name} · L{n}[/{total}] · {pct}%`, with an empty file reading
`{name} · empty · {pct}%` — recomputed on every draw. `pct` is
the anchor's byte offset as a percentage of the file size, unrelated to
indexing progress and always available; `n` and `total` come from the
background index (the indexing mechanics are in Seams for what comes
next, below) through queries that resolve on their own schedules, so
every combination of known and unknown is a real, displayed state. `draw`
skips the status line's queries entirely, requesting nothing and reading
neither `n` nor `total`, in the two cases where the row will not be
shown: while a scan owns it instead, and on a terminal too short to
have a chrome row at all (`has_chrome_row`, next to `content_rows`) —
either way, not only would the result go unused, but the request that
would otherwise wake the status worker would be spent on a number nobody
will ever see.

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
the index scan, the status worker, pending navigation — are shaped around
these rules.

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
- **`goto_line`** is bound to `<count>G` and `<count>gg`, and resolves
  through the index three ways.
  If the index already covers the target line, it walks
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
