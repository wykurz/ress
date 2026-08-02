# Budgeted scanning

A pager for huge files must never let one keypress — or one repaint — read
an unbounded amount of data. `ress` enforces this structurally: the engine
has eight read loops, and the invariant they all share is **bounded per
step, never unbounded in one synchronous call** — with each family naming
what one step is. Six are the budgeted primitives, all in one module
(`ress-core/src/scan.rs`), and take a **mandatory byte budget** as an
explicit `chunk`/`budget` construction parameter. **`chunk` is the threshold at which a step stops
issuing NEW budgeted reads — not an absolute ceiling on what a step may charge** (batch 9
(2026-07-29), finding #2; this sentence has now been wrong in three successive ways, so it is
worth being exact). `Meter::out_of_budget` is `spent_this_step >= chunk`, evaluated BEFORE each
read, so the read that crosses the line is issued in full and charges in full: a step can end up
charged well past `chunk`, and the crate's own tests pin that it does — 63 charged against a
`chunk` of 16 (`backward_scan_charges_the_span_a_short_read_leaves_undelivered`), 4 against a
`chunk` of 1 (`search_forward_ctx_seed_reads_charge_the_budget_the_driver_reads`). Two mechanisms
put it there: a single read is block-granular and indivisible once begun, and the fixed-size
bookkeeping look-around gathers are deliberately UNREFUSABLE (`Reader::read_at_unbounded` — a
scan that cannot afford its own look-behind cannot decide anything at all, so starving it would
trade an overshoot for a wrong answer). What the threshold does guarantee is termination: every
step charges at least one read's worth and then refuses the next, so no step runs unbounded and
progress is always made. **What a byte of budget buys is defined once, below** ("delivered work plus skipped spans") and every other statement of it in this repo defers
to that definition rather than restating it. This sentence used to read "consumes at most that
many *fresh* bytes" (batch 8 (2026-07-29), P2): wrong, and wrong in the direction that matters,
since a prewarmed step charges exactly what a cold one would — the crate's own cache-hit probe
(`classify_itself_refuses_before_touching_the_cache`, `meter.rs`) disproves "fresh" directly.
The other two are background passes, bounded per
step by structure rather than by a byte parameter, each driven through a
step-then-yield regime (see [concurrency](concurrency.md)) rather than a
single unbounded pass: `SweepAnalysis::step` (`ress-core/src/search.rs`,
see [search](search.md)) reads one `SEARCH_WINDOW` (64 KiB) core plus its
fixed lookahead margin per call, and `ScanScheduler`'s background index
pass (`ress-core/src/schedule.rs`) reads one cache block per iteration,
yielding between blocks. A scan without a bound cannot be written, which
is the point — early revisions had five hand-rolled read loops, and "each
loop manages its own bound" reliably decayed into "some loops forgot."

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
- `CountScan::new(from, to, chunk)` — a resumable count of the newlines in
  `[from, to)`; the window is captured once, at construction, like
  `BackwardScan`'s. Each `step` returns `Done(n)` (the whole window
  examined; `n` newlines found) or `More` to continue, reading each block
  through `warm()` rather than `block()`. Unlike `ForwardScan`/
  `BackwardScan`, it never becomes a spawned pending completion; its
  production consumer is the status line's background worker instead (see
  "The status line's count runs in the background", below), stepping it
  directly inside its own loop — a background task can await a block a
  synchronous, per-draw call never could.
- `SearchForward::new(pattern, from, limit, chunk)` — a resumable search
  for the compiled `pattern`'s next match at or after `from`, bounded
  above by the exclusive `limit`. Each `step` reads at most `chunk` bytes
  and returns `Found { match_at, line_start }`, `End` (this leg is
  exhausted with no match), or `More` to continue — see
  [search](search.md) for the two-leg wrap policy `n`/`N` build on top of
  this and `SearchBackward`, and for why a step can legitimately defer
  returning `Found` past where a match first completes.
- `SearchBackward::new(pattern, hi, limit, chunk)` — the mirror image: a
  resumable search for `pattern`'s nearest match before `hi`, bounded
  below by the inclusive `limit` (the two search scans' `limit` bounds are
  asymmetric — one exclusive, one inclusive — by construction; a caller
  composing a wrap-around search across both must account for that one
  byte). Same `step` outcomes as `SearchForward`.

`ForwardScan` and `BackwardScan` are the engine's line-position navigation
read loops — search navigation (`n`/`N`) is built the same way but on its
own pair, `SearchForward`/`SearchBackward` (see [search](search.md)),
resuming toward a match rather than a line count. Either pair's cursor
lives inside the object rather than being handed back to the caller as a
value — an interactive attempt and the pending continuation it falls back
to are literally the same scan, stepped further, never a fresh scan
re-derived from a resume cursor. All line-position navigation
(`scroll_lines`, `goto_end`, `goto_percent`, `goto_line`) and the viewport
itself compose `fill_lines` and the `ForwardScan`/`BackwardScan` pair;
`goto_line` additionally consults the background line index to pick its
scan's starting checkpoint (see "Line index lookups reuse `ForwardScan`"
below). Outcomes are explicit enums for every scan here — a caller must
decide what `More` means for its operation; there is no silent
fallthrough.

## The budget contract is block-granular

Budgets bound **read work**, not the byte position of answers. Reads happen
in cache blocks; once a block is in hand, scanning its in-memory bytes is
free, so a result found in an already-fetched block is returned even if it
lies past the nominal byte budget.

**Derived, post-charging bound** (restructure R3, `ress-core/src/meter.rs`): every read a step
performs THROUGH THE READER is now charged without exception — payload and bookkeeping (context
reads for `^`/`\b`/`\B` lookaround, the backward straddle seed) alike, closing the gap where a
step's own true cost used to be invisible to its own budget.

**The per-step `chunk` changed what it counts, disclosed here rather than left implicit — and
this paragraph is the CANONICAL definition the rest of the repo defers to** (batch 8
(2026-07-29), P2: the contract had four separate statements of itself, no two of which agreed —
this one, the "fresh bytes" line at the top of this file, `meter.rs`'s "every byte of I/O an
operation has done", and `scan.rs`'s "never read past the point it was told to stop". The other
three now point here or state their own narrower property instead of paraphrasing this one.)
(fix round, P2-4; corrected, batch 6 (2026-07-28), finding #5, fix round — the paragraph below has now
been wrong in both directions in succession: it used to name this "bytes physically read," which
overclaimed, and an intermediate correction named it "bytes requested," which undershot the fix by
overclaiming the opposite way): before this restructure, `chunk` bounded bytes *consumed* — a loop
that reused an already-in-hand block (`SearchForward`'s own `seed_block`, `SearchBackward`'s own
`warmed`, both there specifically to avoid a second, cache-promoting touch of a block already
fetched) still charged those reused bytes against its own budget, since charging was tied to how
much of the answer the bytes contributed, not to whether a physical read happened. Now `chunk`
bounds **delivered work plus skipped spans**: bytes `classify` (`ress-core/src/meter.rs`, the
shared core both `Reader::read_at` and `Reader::read_at_unbounded` fetch through) actually hands
back through the `Reader` — `take = min(want, block.len() - lo)`, capped by whatever the ONE block
touched has to give, never by `want` itself (a read asking for 1000 bytes against a 64-byte block
is charged 64, not 1000) — charged identically whether the underlying `BlockCache` touch is a
genuine physical read (a cache miss) or a free in-memory hit, since `classify` has no way to tell
the two apart from its own return value and does not try to; **plus** whatever `Reader::skip`
charges for a span deliberately passed over WITHOUT reading it (a backward descent past territory
an `Empty`/`Short` observation already certified as unreadable). That second term is synthetic —
no bytes are delivered for it at all — and it is charged for the reason the descent exists: a
huge stale-size descent must pend across `step` calls rather than run unbounded inside one, and
only a budget that counts it can make it do so. It is disclosed further down in this document's
own backward-direction bullet; batch 7 (2026-07-28), finding #4: naming it here too, because a
definition of the counter that mentions only `classify` contradicts a mechanism that charges the
same counter elsewhere, and a reader has no way to know which of the two paragraphs to believe. This is one *narrower* exemption than "physically read"
claimed: the two hand-optimized reuse paths named above (`seed_block`/`warmed`) bypass the Reader
entirely for a block they already hold locally, crediting `Meter::record_progress` with no matching
`charge()` — genuinely free, because no `BlockCache` touch happens at all, not merely a cache hit
through one. Every *other* read, hit or miss alike, costs the budget the same:

- **Hit-heavy steps still charge, and can still report `More` on zero new I/O, deliberately.** A
  prewarmed scan whose every touch this step is a cache hit charges exactly what a cold run of the
  same touches would — provable directly (`search_forward_ctx_seed_charges_a_cache_hit_exactly_
  like_a_miss`, `ress-core/src/scan.rs`: an identical ctx-seed gather charges 4 either way, while
  `MockSource::read_count()` shows 2 fresh physical reads cold vs. zero new ones warm). This is
  the delivered-work reading applied consistently, not an oversight: a step whose reads are all
  hits still needs to mint `Meter::progress_witness`'s `Charged` to report `More` honestly (the
  R6 witness law — a step that read nothing genuinely new but claims `More` anyway is exactly the
  zero-progress livelock shape this crate's own history keeps producing); charging delivered work
  regardless of hit/miss is what lets a hit-heavy step still earn that witness. A charging scheme
  that counted physical reads only would mint no `Charged` at all on an all-hit step, unable to
  report `More` honestly, and would need a second, different witness for "read nothing new but
  genuinely made progress" — deliberately not built, since delivered-work charging already covers
  it.
- **The last payload read may overshoot by up to one block.** `Reader::read_at` checks the
  budget BEFORE each read, never mid-read, so the read that pushes a step's own charge over the
  edge still completes in full — at most `block_size` extra bytes, once, per step. batch 6
  (2026-07-28), finding #5: this check-then-fetch ordering is now `classify`'s own first action
  (it takes the refusal decision as an explicit `refuse: bool`, computed by the caller before the
  call, and returns without touching the cache at all when `true`) rather than a convention each
  of `classify`'s two callers had to individually remember — `read_at` passes its own live
  `Meter::out_of_budget()`; `read_at_unbounded` (next bullet) always passes `false`, spelling out
  its exemption at the call site instead of by omission.
- **Bookkeeping (context) reads are charged but never refused**, deliberately (`Reader::
  read_at_unbounded`) — a starved look-behind/look-ahead fetch would either compute a wrong
  answer (an incomplete `^`/`\b` context) or livelock (a context gather that can never complete),
  worse outcomes than a bounded overshoot. Each such read is itself capped by a fixed, small
  constant, never proportional to file size: `CTX_BEHIND`/`CTX_AHEAD` (4 bytes each, one UTF-8
  character, `search.rs`) for ordinary look-around, and — the largest single contributor —
  `SearchBackward`'s own wrap-leg straddle seed, bounded by `MAX_MATCH_LEN` (4 KiB) or the real
  EOF, whichever comes first. **That cap is on requested content, not on physical block touches**
  (batch 6 (2026-07-28), finding #5): a look-behind/look-ahead gather that straddles a block
  boundary costs one `BlockCache` touch per block it crosses, each unconditional regardless of
  `chunk`/`allowance` — at the project's own default `block_size` (1 MiB) this is at most two
  touches for a 4-byte gather; at a small, pathological `block_size` (down to 1 byte) it can be
  as many touches as bytes (`search_forward_ctx_seed_reads_charge_the_budget_the_driver_reads`,
  `ress-core/src/scan.rs`, pins exactly this: 4 physical reads at `chunk: 1`, `block_size: 1`, the
  ctx gather alone exhausting the step before the payload read ever starts). A budget of 1 bounds
  none of this — by design, since refusing partway through an already-started bookkeeping gather
  would produce the wrong-answer/livelock outcome this bullet's own first sentence rules out, not
  a smaller one.

  **One deliberate exception (batch 6 (2026-07-28), finding #2): `SearchForward`'s own F4 peek**
  (a bounded leg's own final boundary, up to `CTX_AHEAD` bytes past `limit`) uses the BUDGETED
  path (`Reader::read_at`, refusable) instead. Unlike the reads above, an incomplete F4 peek does
  not need to compute a wrong answer or livelock to stay safe — the candidate it is verifying is
  already fully read; only the VERDICT (does a trailing assertion hold) is pending — so refusal
  is survivable by resuming (`FwdPhase::Peek`, `scan.rs`) rather than by never refusing at all.
  Before this fix, an incomplete peek WAS silently treated as a complete one, producing exactly
  the budget-dependent wrong answer this bullet's own reasoning names as the reason the OTHER
  bookkeeping reads must never refuse; the fix chose resumability over unboundedness here because
  this one read can be safely deferred to a later step, where the others cannot.

At the project's own default `block_size` (1 MiB), the block term dominates by three orders of
magnitude, and the pre-restructure framing ("a small, fixed number of blocks of its budget")
holds almost exactly. At a small `block_size`, the bookkeeping term dominates instead — proven,
not merely argued, by a direct `charged()` assertion in both directions (fix round, P2-2: the
citation below used to point at a test that only asserts physical read counts, a weaker proxy —
corrected to name the test that actually asserts the charge):
`search_forward_ctx_seed_reads_charge_the_budget_the_driver_reads` (`ress-core/src/scan.rs`)
shows a `chunk` of 1 byte entirely consumed by 4 bytes of look-behind context alone (`s.charged()
== 4`), before the payload read is ever attempted. `Document::resolve_found`'s composition
(below) shares one `Meter` across a search leg and its own follow-up line-start hunt, so the same
bound applies to the composed total, not per phase —
`search_next_backward_composed_budget_is_additive_charged_directly` (`ress-core/src/document.rs`)
pins this end to end with a direct `charged()` assertion on the composed total, capped at
`budget + 1` (one block's own width past the 64-byte budget, block size 1 leaving no
partial-block remainder to round up). Its own sibling,
`search_next_backward_composed_budget_is_additive_not_over_granted`, pins the SAME composition
through the full `Document::search_next` path instead, asserting physical read counts (`budget +
3`, the extra `+2` covering both phases' own block-granular slack plus the one deliberately
uncharged `line_start_shortcut` probe — that field's own doc comment has the full accounting) —
a weaker, but still load-bearing, end-to-end proxy for the same claim.

An operation's *interactive attempt* composes at most a small, fixed number of scans — a percent
jump, for instance, probes one block for a direct hit and, failing that, scans backward once —
so what a keypress does before returning is bounded by a small multiple of the budget, plus that
same small, fixed bookkeeping margin, never by the file's own size. A *pending background
completion* is bounded only by the file itself, by design: it runs in budget-sized, cancellable
chunks with visible progress until the true answer. An answer from paid-for bytes always beats
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

## The truncation policy: when a source's real data ends below its own claimed size

A file can shrink out from under the pager while a `Document` is already open — `size()` reflects
whatever was true when the source opened, but `BlockCache::block`/`warm()` are honest about what
they can actually deliver: a read past the real, current end of the file comes back **short or
empty**, never an error and never padded (see [block cache](block_cache.md)). Every read loop in
this module — and the two background passes outside it, `SweepAnalysis`'s sweep in
`ress-core/src/search.rs` and `ScanScheduler`'s index pass in `ress-core/src/schedule.rs`, which
share the identical exposure — must treat that short/empty
read as meaningful information, not an anomaly to shrug off. One policy, stated once, applied
everywhere a loop reads blocks in either direction:

**A WHOLLY EMPTY block read means the source's real data ends at or below the position that read
was attempted, below whatever `size()` claimed at open time.** From that point on, nothing more
will ever arrive at or past it, no matter how many more times the loop is stepped — but that is a
**narrower** fact than "nothing more can ever be found": every real, readable byte strictly
*below* that position is still exactly as findable as it always was.

**At or below, not at** (batch 23 (2026-08-01) — this paragraph said "ends at the position that
read was attempted", which is an upper bound written as a location). The read proves nothing
exists from that position upward; where the data actually stops could be anywhere below it, since
the block underneath may itself be short. That is enough for every loop here, all of which use the
fact to STOP or to descend — both correct under a bound. It is not enough to answer "is this exact
position the real end", which is what a zero-width `$` and the viewport's own `buf_is_true_eof`
ask, so the two consumers that ask it require a second premise: a real byte immediately below the
position, or the position being 0. See `CertifiedEnd`'s own doc comment (`meter.rs`) for the
premise, and `fill_lines` and `SearchBackward::note_empty_end` (`scan.rs`) for the two places that
discharge it.

**A SHORT-but-nonempty read means none of that** (batch 9 (2026-07-29), finding #3 — this
paragraph opened with "An empty or short-of-expectation block read" and so contradicted, in its
own first sentence, the only-`Empty`-certifies law stated two paragraphs down and enforced by
`meter.rs`). `BlockSource`'s "up to `len`" contract permits a short answer for reasons that have
nothing to do with EOF, so a short block certifies *nothing* — not the end of the file, and not
even the end of that block's own real content as far as the source is concerned. What it does
establish is narrower and purely local — and, since batch 12 (2026-07-29), no longer permanent:
the cache's own fetch task re-asks the source for the withheld tail, so the hole either
closes (more bytes arrive) or the real end is CERTIFIED at that exact offset (the source itself
answers empty there). Before that, the memoised short answer made the remainder of that block
unreadable for the rest of the document's life while every later block might hold perfectly
ordinary content — the missing fact that every consumer had to guess at, and guessed differently.

Batch 13 (2026-07-29) retired that refill's own attempt cap, and with it the residual hole:
the loop now runs until the block is full or the source certifies its end, bounded by the
`block_size` bytes the hole is wide — the same policy `PreadSource` already applies one layer
down. **Every block a consumer receives is therefore resolved**, so "a hole a refill could not
close" is no longer a state anything downstream has to represent, and the cap's own two costs are
gone with it: past it, legal source bytes stayed permanently invisible and a search could answer
`FoundMatch` with an anchor whose viewport contained neither the line nor the match; and because
the attempt was spent before admission, cancelling a search consumed refill budget without ever
performing a read. The descent machinery below remains the answer for the short block that is
still real — a source whose `size()` overstates its data, which really does end inside a
block. A loop that reads the two as the
same thing gets a wrong answer, not a conservative one: all three forward scanners once reported
"not found" for matches sitting in plain sight one block above a legally short read (finding #1).
The way to tell them apart is to look — one bookkeeping peek at the next block, whose emptiness
is the thing that actually certifies. A loop that ignores
this and just returns its "more work remains" outcome unconditionally hangs — the position never
advances, and a caller re-stepping it walks into the identical state and gets the identical
non-answer forever (a real, once-shipped defect this document now says exactly why to avoid).

**Restructure R3 makes this claim type-enforced, not merely a convention every call site must
remember** (`ress-core/src/meter.rs`): `Reader` is the only way a loop touches `BlockCache::
block`/`warm`, and its `classify` is the only place a `CertifiedEnd` can be minted — no public
constructor, no `From<u64>`, and never derivable from `BlockCache::size()`. A loop that receives
`ReadOutcome::Short`/`Empty` (or, for the two unbounded-driver-regime passes, `Fetched::Short`/
`Empty`) knows the observation itself was genuine rather than trusting its own bookkeeping. **A `CertifiedEnd` is never
minted from a short LENGTH, though** (batch 9 (2026-07-29), finding #3: this sentence used to say a
loop receiving `Short` *or* `Empty` "is holding proof", which reads as
though both certify an END — they do not, and the next paragraph has said so since restructure R3's
own P1-1 fix). Four cases worth keeping apart, since the shorthand
"short means nothing" collapsed them (batch 21 (2026-07-31)):
1. a **raw short length** certifies nothing, ever — that is the law, and it has not moved;
2. a **certified short block** does: `BlockCache` asked the source what lay past it and got EMPTY
   back, so `Block::ends_data` is set and `classify` mints `Fetched::Short { end: Some(..) }` at the
   position the source itself answered for;
3. **`ReadOutcome::Short` drops that witness** — not because it is unsound, but because its one
   consumer derives the same fact from the block it is already holding, and two derivations of one
   fact is the shape that produced batch 13's finding #4;
4. a block short only because the file's **claimed size** ends inside it is resolved without any
   certificate at all — nothing beyond it to fetch, and nobody asked the source about it.

`fill_lines` (the viewport's own single-call read) is the one exception to "short means the end,"
and deliberately so, after an adversarial review corrected a first attempt at widening it further
(fix round, restructure R3, 2026-07-27): only an **observed empty read** may certify `fill_lines`'s
own `FillOutcome::End`, at that read's own position, never inferred from a short block's own
reported length. That observation reaches `fill_lines` two ways — directly as `Fetched::Empty`,
or carried on a `Fetched::Short` whose block the cache had already certified, which is case 2
above (batch 22 (2026-08-01): this paragraph used to say `Fetched::Short` carries no
witness *at all* and that only a direct `Empty` can produce `End`, contradicting the taxonomy
immediately above it and the code in `scan.rs` since batch 14). A merely-**short** (not empty,
not certified) block is not, on its own, proof nothing more
exists elsewhere — `BlockSource`'s own "up to `len`" contract (`source.rs`) permits a short
answer for a reason other than real EOF — and unlike the six budgeted loops above, which may
safely treat a short block as terminal because their own truncation-descent machinery keeps
reading past it on the very next `step` regardless, `fill_lines` returns straight to the
highlighter: a wrong trust there paints a fabricated match on screen, not merely a delayed
correction. A first attempt tried to recover the block-aligned real end past a short block by
issuing one further, discarded probe at the next block's own start — unsound: the block-indexed
cache memoizes whatever a source returns for a block, including a short answer, so the gap
between that short answer's own length and the next block's own start becomes permanently
unaskable once cached, and "the next block is empty" cannot distinguish "nothing real is in that
gap" from "real data is in that gap and the file's own end simply also happens to fall on the next
boundary." `fill_lines` therefore reports `FillOutcome::Budget` — the same conservative miss the
pre-restructure code gave — whenever an **uncertified** short block stops it before `rows` is
reached. Read that variant's own doc comment (`scan.rs`) for what `Budget` actually covers: it is
the "cannot prove where this stopped" answer, and its third and most ordinary member is a block
resolved on the file's own claimed size (case 4 above), which is how every file whose real end is
not block-aligned finishes. `Document::viewport` recovers that one from `buf_end == self.size`
rather than from a fourth outcome variant, deliberately — see the same doc comment. (`docs/
search.md`'s own account of the highlighter has the full derivation and the adversarial fixture
that motivated the correction.) Direction decides the correct response for
the six loops that DO trust a short block, and it is never "stop dead" alone:

- **Forward-direction loops** (position increases toward the discovery point) treat the position
  where the short read landed as the genuine, honest end and resolve their own terminal outcome
  there — exactly the outcome they would have produced had `size()` reported that value from the
  start. A scan hunting for a line start clamps to the last one it found; a count reports the
  count so far as final; a pattern search runs the one remaining accept check its own model
  already has for "this is genuinely the last byte that will ever exist" (a zero-width match at
  the true end, or a real match already sitting in an unconsumed lookahead margin), using
  whatever it already read, rather than waiting on lookahead that is now known to never arrive.
  The background index pass is this rule in its simplest form: an empty block read ends the pass
  at the real end (a short-but-nonempty block is ingested as ordinary content, and the following
  iteration's read comes back empty — the pass resolves one iteration later, never hangs).
- **Backward-direction loops** (position decreases from a starting point that may itself sit
  above the real data, e.g. a wrap leg constructed from a now-stale `cache.size()`) **descend**
  past the empty territory instead of terminating: the position drops to the empty block's own
  start and the loop continues from there, budgeted exactly like any other read (the skipped span
  is charged against the same chunk budget the loop already carries), so a huge stale-size
  descent pends across many `step` calls the same way any other long scan does, rather than
  either hanging or running unbounded in one synchronous call. This is safe precisely because the
  loop's own lower bound (0, or an explicit floor) is unaffected — the descent can only ever
  shrink the search window, never widen what may be reported. `SearchBackward`'s own wrap-leg
  terminal check (a narrower, zero-width-only verification of a single candidate position, not
  the main payload search — see [search](search.md)'s own account of the wrap policy) applies the
  identical principle at byte, not block, granularity: a short look-behind read there certifies
  whether `hi` itself is real before its own zero-width accept check may run at all, descending to
  the exact point the read reached (not merely the containing block's start) when it is not, and
  **re-arming** so the check retries at each successively corrected position — the one addition
  this narrower verification needs that the general payload descent above does not, since a
  payload search is retried anyway on its own very next iteration, while this check only ever runs
  once per resolved `hi` and must be told explicitly to run again. `SearchForward`'s own early
  zero-width-at-EOF terminal check (batch 5 (2026-07-27), finding #10-internal) is the forward
  twin of the identical shape, but it is a COMPOSITE of both bullets above, not a bare instance of
  either one alone (fix round 2, review response, P3-2 -- correcting an earlier claim here that
  this policy "already describes" the forward case, which reads as no-change-needed when a new
  descent is exactly what changed): the lazy ctx seed itself now DESCENDS on a short read, this
  bullet's own backward behavior, applied on the forward side to CERTIFY where the source's real
  data ends; the payload loop that runs once certification succeeds still RESOLVES IN PLACE, the
  bullet just above's own forward behavior, unchanged. Needing only the descend half of the
  backward bullet's own two-part shape, not a separate re-arm field, is true and is this
  composite's own simplification — staying in `FwdPhase::SeedCtx` (restructure R6; formerly a
  bare `ctx_ready` flag, false until a full, certified read succeeds) already serves that role,
  since unlike `hi == cache.size()` it never stops being re-derivable once `pos` moves — but the
  descent itself is new, not something this section already covered before that finding landed.
- **No loop may return "more work remains" with its own cursor left exactly where it was.** That
  outcome must mean real progress happened and more remains; if a read comes back short, the loop
  either resolves a terminal outcome (forward) or moves its own cursor before yielding control
  back (backward) — never both "I have not finished" and "I made no progress," together, from the
  same `step`.

This is a distinct regime from a caller-supplied position simply landing past the *current*
`cache.size()` (the ordinary `pos.min(size)`-style clamp several scans already apply at entry) —
that regime is about a stale caller-side cursor and is fully handled by clamping before the first
read ever happens. The truncation policy is about what a loop *discovers mid-scan*, only knowable
once a read actually comes back short.

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

## The status line's count runs in the background

The status line's current-line query used to run on `draw`'s own call
stack every frame, which meant it could never await source I/O the way
the other read loops do — a draw that stalled on a cold read would
defeat the pager's whole first-paint guarantee. `StatusWorker` (see
[architecture](architecture.md)) sidesteps the constraint instead of
working around it on the same call stack: it is a separate background
task per document, fed the anchor `draw` cares about through a `watch`
channel and answering through another, so `draw` itself only ever sends
and reads — never awaits a scan step at all.

The worker's own loop steps a `CountScan` directly, exactly like any other
budgeted scan: chunks stay bounded by the same `nav_scan_budget` config
value, and a window wider than one chunk resumes the same scan object
across internal steps rather than restarting from the checkpoint. Because
the stepping happens in the background rather than on a per-draw call
stack, a block the step needs but the cache does not have is simply
awaited through `warm()` — there is no separate "block missing" outcome to
manage, no detached fetch to track, and no holding area outside the
shared cache to protect a delivered block from eviction before it is
consumed: the worker consumes each block's bytes the instant `warm()`
returns them, within that same step, so there is never a window in which
a block it still needs could be evicted out from under it the way one
could be evicted from underneath a synchronous, cache-only caller working
one delivered block at a time. A block that fails to read is retried,
after a short backoff, up to a bounded number of times before the anchor
is given up on for good — a persistently unreadable block stops costing
anything; requesting a new anchor starts the count, and its retry budget,
fresh. A fresh anchor supersedes either a mid-walk step or a backing-off
retry the same way: `select!` races the anchor channel against whichever
the worker is doing, so the worker jumps straight to resolving the new
anchor rather than racing the old walk to completion and throwing the
result away.

## Layout is budgeted too

The same philosophy applies one layer up: `layout_row` (tab expansion,
Unicode width, control-byte carets, horizontal windowing) stops building
cells at the window's right edge, so rendering a window into a long line
costs the window, not the line. The horizontal offset itself is capped at
the type level (`HScroll`), which keeps the viewport's derived scan budget
finite for any input.
