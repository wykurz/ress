# Search

`/` and `?` search forward and backward for a regex pattern (`regex::bytes`,
matched over raw file bytes — no encoding assumptions); `n` and `N` repeat
or reverse the last search. This document covers the model underneath: the
windowed matcher and its two honest limits, smartcase, the wrap policy and
its notice, the background sweep that keeps the status line's running
match count and the scrollbar's match ticks fed without making `n`/`N` pay
for a whole-file scan, and a rendering caveat search highlighting inherits
from the viewport.

## The windowed matching model

**Code home (restructure R4, 2026-07-27):** this whole model — the accept range, the
look-behind/lookahead margins, the phantom-newline rule, the edge declarations — is centralized in
`search::hay::Hay` (`ress-core/src/search/hay.rs`); every windowed search in this crate constructs
one and asks it which candidates are reportable, rather than re-deriving the arithmetic below by
hand at each call site (a mechanical CI guard, `ress-core/tests/no_raw_hay_primitives.rs`, keeps
it that way). The prose below states the MODEL — what the numbers mean and why — which the type's
own doc comments now also carry at each method; this document stays the one place a reader looks
for the model as a whole, not a derivation repeated at nine sites.

A pattern is compiled once (`SearchPattern::compile`, in
`ress-core/src/search.rs`) and then matched against fixed-size windows
rather than the whole file at once — the same "never read the file in one
gulp" discipline every other engine scan follows (see
[budgeted scanning](budgeted_scanning.md)). Each window is a
`SEARCH_WINDOW`-byte (64 KiB) **core** plus `MAX_MATCH_LEN + CTX_AHEAD - 1`
(a little over 4 KiB) bytes of trailing **lookahead**, so a match that
starts inside one window's core but needs a few more bytes to complete is
still found whole, without re-reading anything — the lookahead of one
window is the next window's own leading bytes. The lookahead reaches a full
`MAX_MATCH_LEN` past a maximal-length candidate's own **start** — one byte
past its own last content byte — not one byte less: its own trailing
assertion (`$`/`\b`/`\B`) is evaluated exactly there, so the margin has to
reach that far too, not merely to the candidate's own end (finding #10) —
and `CTX_AHEAD - 1` further still, because `$` and ASCII `\b`/`\B` are
satisfied by that one byte, but a Unicode-aware `(?u:\b)`/`(?u:\B)` needs
the FULL following character to decode, up to `CTX_AHEAD` (4) real bytes,
not one (finding #3 — the exact mirror of `CTX_BEHIND`'s own reasoning,
below, on the trailing side). A match's *start* is attributed to exactly
one window, the one whose core contains it, via an accept range that keeps
consecutive windows from ever double-reporting or dropping a match that
straddles the seam between them.

**Matching runs over raw file bytes, never decoded Unicode text.** Patterns compile with
`regex::bytes`'s own `unicode(false)` mode (`SearchPattern::compile`), the honest reading of
"raw file bytes" above: `.` matches exactly one byte — any byte except `\n` — rather than
silently treating a run of bytes as one decoded codepoint the way `regex::bytes`'s own *default*
(Unicode) mode does (probe-verified: its `.` skips an invalid UTF-8 byte as if it were a single
unseen character, which would otherwise be an encoding assumption this document claims nowhere
else exists). The one place this is visible without typing anything special is
[smartcase](#smartcase): case-insensitive folding covers ASCII letters only (`a`-`z`/`A`-`Z`) — a
non-ASCII case pair such as `é`/`É` is never folded, so it stays case-sensitive even when
smartcase would otherwise have made the whole pattern insensitive. Smartcase's *sensitivity
detection* (does the typed pattern contain an uppercase character at all?) stays fully
Unicode-aware regardless — it is plain Rust `char` classification, run before the pattern ever
reaches the regex builder — so a pattern containing `É` sets it case-sensitive exactly like an
ASCII uppercase letter would; only the *folding* that would otherwise happen is ASCII-only.
Detection sees every uppercase letter there is; folding acts on the ASCII ones alone. A pattern
can opt back into
full Unicode-aware matching — codepoint-wise `.`, `\p{..}` character classes, Unicode-aware case
folding — per use, with the inline `(?u)` flag; without it, a `\p{..}` class fails to *compile*
at all (surfacing through the same bad-pattern notice any other syntax error takes, never a
silent behavior change), while a bare `\w`/`\d`/`\s` Perl class still compiles fine, just
narrowed to its ASCII definition.

Two callers reuse this model with a different accept-range rule each: a
caller that stops at its **first** match and never re-searches the same
bytes (`n`/`N`'s on-demand scan, below) may accept a match starting
anywhere in the window it is holding, but only *reports* one once every
start at or before it has had a full cap's worth of lookahead (or the scan
has reached its own read boundary) — so the match it returns is genuinely
`find_all`'s own **leftmost** one, for any match no longer than
`MAX_MATCH_LEN`, not merely the first one to *complete* (a later, shorter
alternative can otherwise complete on far less hay than an earlier, longer
one needs — e.g. `foobar|oo` — and race ahead of it); a caller that must
**enumerate every match** (the background sweep, below) has to accept only
*fresh* starts — bytes it has not already searched — or it recounts a
seam-straddling match once per window it still sits in. The two rules are
independent: leftmost-ness is about *which* candidate a stop-at-first
caller returns, fresh-starts-only is about never re-offering the same bytes
across the *many* reports an enumerating caller makes — narrowing the first
did not relax the second.

Two limits fall directly out of this design, worth stating honestly rather
than glossed over:

- **A match longer than `MAX_MATCH_LEN` (4 KiB) is not guaranteed to be
  found.** The lookahead is exactly enough to complete a match starting at a
  window's very last core byte and running the full cap; a genuinely longer
  one can straddle a seam with too little lookahead on either side to ever
  complete. At or below the cap, **no match is ever missed because of a
  window seam** — the lookahead margin stated in the previous sentence
  guarantees that much, and that is the guarantee this bullet is about
  (batch 6 (2026-07-28), finding #6 reversed an earlier claim that seams
  could lose a sub-cap match; batch 7 (2026-07-28), finding #5 narrows the
  replacement, which overshot in the other direction by promising a sub-cap
  match is *always found*, full stop). Seams are not the only reason a
  sub-cap match can go unreported, so the wider promise is not one this
  engine keeps: a source that answers a read short leaves the viewport
  refusing to paint a match navigation still finds
  (`viewport_and_nav_agree_at_a_non_block_aligned_truncation_now_by_resolving_it`,
  `ress-core/src/document.rs`, pins exactly that, deliberately), and batch
  7's own finding #1 was a live sub-cap miss with no seam involved at all
  (a walk that stopped early discarded a needle sitting mid-block — fixed,
  but it is the reason this sentence now scopes its claim instead of making
  it unconditionally). Above the cap, the outcome now
  depends on whether the pattern carries an assertion the cut
  could fake (restructure R5, 2026-07-28, closing the unit-J class —
  `search::hay::Hay::verified`'s own end rule): a pattern with **no
  trailing assertion at all** (`a+`, `.*`, `[a-z]+`) keeps reporting the
  over-cap match exactly as before — a hay cut cannot *manufacture* a body,
  only fail to complete one, so the match itself is genuine even when
  truncated context around it isn't — while a pattern that **does** carry
  one (`a+\b` over a run far longer than `MAX_MATCH_LEN`) is now a
  documented miss instead of a fabrication: the old behavior read a
  window's own artificial edge as the true haystack end once no accept site
  rejected the over-length span outright, reporting a match no whole-file
  `find_all` pass agreed with — the identical "slice's own edge is
  unconditionally eligible" hazard this whole document otherwise guards
  against, now closed here the same way it is everywhere else: the
  assertion is rejected unless decided against real content or a genuine
  file boundary, never an artificial cut. This bounds the *match itself*,
  not line length or file size, and applies identically to `n`/`N`, the
  background sweep, and the highlighter (see each one's own account below).
- **A pattern containing an explicit `\n` can match — but only within one
  window's own seam overlap.** `regex::bytes`'s `.` never matches `\n` by
  default, so an ordinary pattern is naturally confined to one line without
  any extra work here; typing a literal two-character `\n` escape into the
  pattern (e.g. `foo\nbar`) is not rejected, but it inherits the identical
  `MAX_MATCH_LEN` bound as everything else — the combined span has to fit
  inside one window's lookahead to be found, not an arbitrary number of
  lines away.

A third limit sits beside those two, unrelated to the windowed matching
design itself but still worth stating plainly: highlight cells — and the
`/`/`?` prompt's own on-screen width — are positioned per Unicode
**scalar value**, not per grapheme cluster. `layout_row_with_marks`
(`ress-core/src/line.rs`) walks a line one `char` at a time, measuring
each with `unicode_width::UnicodeWidthChar` and mapping a match's byte
range through that same per-scalar walk into the visible-cell range it
highlights; `render_command_line` (`ress/src/render.rs`) measures the
search prompt's own text the identical way — the same width source
`line.rs` already uses for the viewport, not a second model search
introduces. Ratatui's own `Buffer::set_stringn`, which actually paints the
string a moment later, disagrees: it groups text into grapheme clusters
(`unicode_segmentation`) before assigning cells. The two models agree for
ordinary text, but a cluster built from several scalars — a ZWJ emoji
sequence such as `👩‍💻` foremost — can make them disagree: `render_viewport`
paints a row's glyphs via `set_stringn` and then patches REVERSED/BOLD
styling onto the *scalar*-computed column range separately
(`restyle_span`), so such a cluster can leave the highlight landing on the
wrong cell; the command line's own scalar-summed width can likewise
over-count what `set_stringn` actually renders and truncate more of the
typed pattern than necessary. This is not a search defect: search
highlighting is simply the first caller to hand `layout_row_with_marks`
byte ranges that must land on exact cells, surfacing a mismatch the
viewport's plain text already carried latently. The fix belongs to the
roadmap's already-deferred grapheme-cluster workstream (a dependency of
the planned soft-wrap work), not to search, which inherits the viewport's
own width model rather than inventing its own.

## Anchored patterns: `^`/`$`/`\b`/`\B` and the boundary-context model

`^` and `$` are compiled as **line** anchors (`multi_line(true)`), matching right after or
before any `\n` as well as the file's own true start/end — not merely the start/end of whatever
window or slice a particular consumer happens to be holding at the time. That distinction
matters because every consumer below hands the regex engine a *slice*, and a slice's own edge is
unconditionally `^`/`$`-eligible on its own regardless of `multi_line` (a haystack that happens
to start or end mid-line is still treated as *the* start or end by the regex engine) — so each
consumer carries real look-behind/lookahead context of its own around a chunk boundary, rather
than trusting the slice's own edge, to keep that from being mistaken for a real line boundary.
`\b`/`\B` (word boundaries) and `\A` (the true, absolute file start — distinct from `^`'s *line*
start) share the identical hazard: a slice's own edge is just as eligible for those as it is for
`^`/`$`.

Every consumer's own hay is therefore `[look-behind][payload][lookahead]`: up to `CTX_BEHIND` (4)
real bytes immediately before the payload, and the window's own `MAX_MATCH_LEN + CTX_AHEAD - 1`
lookahead margin past it — the on-demand scans, the sweep, AND, batch 5 (2026-07-26), findings
#6/#7, the highlighter too (below); all three consumers now share the identical AHEAD-side margin,
not two different widths. **Correction (batch 5 (2026-07-26) fix round, review response;
superseded by findings #6/#7 below):** an earlier version of this paragraph claimed the
highlighter's own peek "was simply already wide enough" for the identical reason the windowed
margin was widened — false, and retracted at the time. `CTX_BEHIND` bytes of trailing context
(the highlighter's own OLD width) were adequate only for a candidate whose trailing assertion
lands exactly at the highlighter's own row-buffer edge; the highlighter's own row-scoped hay cut
(`accept_and_hay_for_row`, `document.rs`) could end a candidate's *own body* well short of that
edge instead, at a position with less than a full character's worth of real lookahead past it (or,
at a clamped cut, none at all) — a fabrication, not a documented miss, confirmed by two constructed
fixtures (an interior cut splitting a straddling `é`; a clamped cut where `$` is satisfied by the
slice's own artificial end). Pre-existing at the time this correction first landed; the fix is now
landed too (findings #6/#7, below) — the highlighter's own AHEAD-side margin widened to the same
`MAX_MATCH_LEN + CTX_AHEAD - 1` every other consumer already used, AND the resolved-criterion belt
(finding #6's own second mechanism). Fix round (2026-07-27), P3-3, crediting the mechanism that
actually closes each fixture precisely (an earlier version of this sentence credited the margin
widening alone for "closing both"): the clamped cut is closed by the BELT specifically (reverting
the belt alone, margin widening left intact, reopens it); the margin widening closes the interior
cut on its own terms too, but the belt independently closes that SAME fixture regardless (a
maximal-length candidate's own margin under the pre-widening formula is provably always short of
`CTX_AHEAD`, so the belt's own unconditional check rejects it either way) — both fixtures are
therefore covered by the belt alone, with the widening additionally turning what would otherwise be
a documented miss into a correct find for genuine, within-margin matches
(`visible_window_matches_widened_interior_margin_turns_a_would_be_miss_into_a_correct_find`,
`document.rs`).
`CTX_BEHIND`/`CTX_AHEAD` are both 4 because a UTF-8
character is at most 4 bytes — one byte on either side is enough for `^`/`$` and ASCII `\b`/`\B`,
but Unicode-aware `(?u:\b)`/`(?u:\B)` need the *whole* adjacent character, on WHICHEVER side it
sits: a single byte can be a lone, invalid UTF-8 continuation byte (the tail of a real multi-byte
character whose own head sits one byte further back, already dropped or not yet read) or an
incomplete lead byte (the head of a real multi-byte character whose own tail has not been read
yet), either decoded as non-word regardless of what the real character actually was — a genuine
false-match class this engine closed by widening every look-behind fetch (finding #9) and every
look-ahead margin (finding #3) alike, not merely `SweepAnalysis`'s or one side of the boundary.
Stated once, for both sides: **assertions at a position are verifiable iff the full adjacent
character on each side is readable, or that side is a true file boundary.** Where a boundary byte
genuinely cannot be read at all (see `SweepAnalysis`'s own `give_up` gap, below), there is **no
sentinel** standing in for it: no single fixed byte value is conservative for every assertion (a
byte that safely fails `^` — anything that isn't `\n` — can simultaneously manufacture a false
`\b`, if the real, unread predecessor happened to be a word character) — the affected position,
and up to `CTX_BEHIND - 1` more after it (a straddling multi-byte character can leave those
unverifiable too, not just the first one — finding #8), are excluded from the accept range
instead, a documented miss rather than a guess.

- **The highlighter supplies real edge context too.** `find_all_starting_in` (the highlighter's
  own sibling to the pure, whole-buffer `find_all` oracle) runs over `ctx_before + buf +
  ctx_after`, done in `Document::viewport` itself (which already performs IO fetching `buf`),
  never in the no-IO `render` path. Before this, the highlighter handed the regex `buf` alone,
  inheriting the identical "slice's own edge is unconditionally eligible" hazard every other
  consumer already guarded against: `𝄞$` could falsely highlight where a budget-truncated `buf`
  happened to end right after `𝄞`, even though a real byte (not `\n`/EOF) followed just past what
  was fetched; a viewport scrolled below the true file start (`top > 0`) could falsely highlight
  `\Afoo` even though `\A` only ever means the file's own true position 0.
  `ctx_before` fetches up to `CTX_BEHIND` real bytes immediately before `buf`; batch 6
  (2026-07-28), finding #4 made that a multi-block walk downward, so it is **up to `CTX_BEHIND`
  `warm()` reads, not one** (batch 7 (2026-07-28), finding #6: this sentence and its twin on
  `Document::edge_context` both still promised a single read after that change — at `block_size`
  4 the reported fixture needs two, and at `block_size` 1 it needs four). Bounded by the same
  constant either way. **What decides the count is ALIGNMENT, not `block_size`** (batch 8
  (2026-07-29), P2): this used to close "and at any realistic `block_size` still the single block
  adjacent to `top`", which is false whenever `top % block_size` is 1, 2, or 3 — then
  `want_from = top - CTX_BEHIND` lands in the preceding block and the walk touches two, at 1 MiB
  exactly as at 4. A large `block_size` makes that alignment rarer, never impossible. **For
  interior positions only** (batch 9 (2026-07-29), finding #5): `want_from` is a `saturating_sub`,
  so near BOF it clamps to 0 instead of crossing anything — at `top` 1 with `block_size` 4 only
  block 0 is touched, remainder 1 notwithstanding. The two-block case needs `top >= CTX_BEHIND`,
  i.e. the full lookbehind to actually exist below `top`.
  `ctx_after`, batch 5 (2026-07-26), finding #6, fetches up to `MAX_MATCH_LEN + CTX_AHEAD -
  1` real bytes immediately after `buf` — widened from a single-block, `CTX_BEHIND`-wide fetch,
  which was adequate only for a candidate whose trailing assertion lands exactly at `buf`'s own
  edge, not one starting further back (the fabrication the correction above names). The AHEAD side
  may need a few `warm()` reads, not one — mirroring `SearchForward`'s own bounded F4 peek in
  shape, block by block, stopping the moment a block comes back short — but stays bounded (at most
  `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes, `O(cap)`) and runs ONCE per `viewport` call, never once
  per row. **The ahead side is decided by alignment too** (batch 8 (2026-07-29), P2). Fix round
  (2026-07-27), P3-2 corrected this against a MEASUREMENT rather than the cap's own arithmetic —
  `MockSource::read_count()` deltas across one `viewport` call flat at 1 regardless of `rows`, at
  the default `block_size` (1 MiB) — and concluded that 4099 bytes "fits inside the SAME block
  `buf`'s own read already warmed, so the widening costs ZERO additional physical reads". The
  measurement was real; the generalization drawn from it was not. It varied `rows` while holding
  the viewport's own OFFSET fixed, and offset is the variable that decides this: whenever `buf`
  ends within 4099 bytes of a block boundary the request can cross it and read the next block.
  Re-measured at the default 1 MiB with `buf` ending 100 bytes short of the boundary: **2 reads**,
  against 1 for the same viewport block-aligned. **At most one more, not necessarily one more**
  (batch 9 (2026-07-29), finding #4 — the batch-8 replacement overcorrected into a second
  unconditional claim): crossing is necessary but not sufficient, since `want` is
  `min(consumable_reach, size - buf_end)` and so only reaches past the boundary when that much
  real file remains. With only 50 real bytes past that same `buf`, the count stays at 1, measured.
  The honest claim is therefore doubly conditional — ZERO additional reads when `buf`'s own end
  still has 4099 bytes of its block left OR the file ends before the boundary, at most one more
  otherwise (and zero even then against an already-warm block, since these are PHYSICAL reads) —
  and "a few blocks" only actually materializes near a `block_size` on the order of 4 KiB or
  smaller, where read count grows with `buf`'s own size, still flat in `rows`.
  A match *starting* in `buf` is highlighted — its visible prefix, up to whatever `buf` and
  `ctx_after` together cover — even when it *completes* inside those real `ctx_after` bytes
  (batch 4 (2026-07-24), finding #5's per-row restructure, fix round F2): the tail has cells to
  render only up to `buf`'s own end regardless, but the match itself is genuine (`ctx_after` is
  real, already-fetched file content), and `n`/`N` already finds the exact same match by walking
  there directly — highlighting it here keeps nav and the highlighter agreeing rather than one
  finding what the other hides. A match needing MORE than `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes
  past `buf`'s own end to complete remains a documented miss: `ctx_after` never reaches further
  than that, the identical cap every other windowed consumer shares.
  A row's own hay is clamped to `full_hay`'s own edge (`full_hay.len()`) at the LAST row of every
  viewport call, healthy or not — this is the ordinary, hot-path case, not a rare corner (fix round
  (2026-07-27), P2-3, correcting an earlier claim here that this clamp was "reachable only when the
  source is truncated"; false, and retracted). Two residuals the AHEAD-side widening alone does not
  remove, both closed by finding #6's own **resolved-criterion belt**, applied at every such clamp:
  - **A genuinely truncated source.** `ctx_after`'s own fetch can come back shorter than requested
    (its own byte budget bites before reaching more real data) — a candidate ending within
    `CTX_AHEAD` of the shortened edge is UNRESOLVED, not confirmed, UNLESS that edge is trustworthy
    as a real boundary (a real boundary needs no margin, the rule every windowed consumer already
    applies). Two ways it can be: the edge equals `self.size` (the ordinary case), OR the
    AHEAD-side fetch's own first touch past `buf` came back GENUINELY EMPTY (`edge_context`'s own
    `ahead_hit_real_end`) — verifying that position as the real end regardless of what `self.size`
    claims. **Correction (fix round (2026-07-27), P2-NEW):** an earlier version of this sentence
    called that second way "the identical trust `SearchForward`'s own `take == 0` branch already
    places in that exact discovery … so the highlighter agrees with nav even when a source's real
    content ends short of its own claimed `size()`" — **false, retracted**. At the time,
    `SearchForward` trusted ANY short-or-empty read (deliberately left untouched, out of that fix
    round's own scope); the highlighter, since finding #6's own P2-1 fix, trusts only a GENUINELY
    EMPTY one (a short-but-nonempty answer is legal under `BlockSource`'s own "up to `len`"
    contract for a reason other than real EOF, `source.rs`, so it is never a certificate on its
    own). **Further correction (batch 5 (2026-07-27), finding #10-internal):** `SearchForward` no
    longer trusts an uncertified read at all — its own early zero-width-at-EOF terminal check used
    to use the ctx seed's own output directly, fabricating a match at a fictional `self.pos`
    whenever the seed's own read came back short (`size()` a stale, never-re-stat'd snapshot, the
    identical hazard finding #1 closed on `SearchBackward`'s own wrap-leg terminal check). A seed
    gather that fails to REACH `self.pos` now DESCENDS it to the point the gather actually got to
    and retries there, budgeted, until one does reach it — the descent is gated on `p < self.pos`
    in `scan.rs`, not on the read being full, so a `Fetched::Short` whose bytes still cover the
    requested context certifies exactly as a full read does (batch 22 (2026-08-01) corrects "until
    a full read certifies it", the same overstatement this document carried for the backward
    terminal check). The comparison above is no longer live: `SearchForward`'s own
    "any short read" side of it has been fixed, leaving only the highlighter's own residual open.
    **Load-bearing reason, stated precisely (fix round 2, review response, P3-3):** the main
    loop's own `take == 0` branch is UNCHANGED and still fires on a short-but-nonempty block (the
    same condition, `block.len() <= lo`, a 36-byte block at `lo = 36` satisfies) — against unit
    H's own doctrine two paragraphs up ("a short-but-nonempty answer … is never a certificate on
    its own"), that looks like a contradiction, but is not: every position the main loop reaches
    is backed by bytes it has actually READ (the seed certifies the starting `pos`; the loop then
    advances only by `take` bytes genuinely consumed each iteration), so a `take == 0` there really
    does mean the real end, observed, not merely inferred. The highlighter needs the stricter
    empty-only rule specifically because it probes a position it has NOT read up to — the two
    rules differ because what backs the position being checked differs, not because one of them
    is wrong.

    **The same premise governs the highlighter's own empty reads, and batch 22 (2026-08-01),
    finding #1 found it missing there.** An empty read at `p` proves `real_end <= p`; the equality
    comes from a real byte immediately below `p`, which is exactly the "backed by bytes it has
    actually READ" property this paragraph names for the main loop. `fill_lines` applied the
    empty-only rule but not the premise, so its FIRST read — the one position it has read nothing
    below — certified whatever position the viewport happened to be anchored at: over 5 real bytes
    behind a claimed 100, filling from 50 reported the file's real end as 50. It now reports
    `FillOutcome::Budget` unless it holds the premise, which is a contiguous walk's own bytes or
    position 0 — and *not* a `BlockJump::ContentBelow` answer, which this sentence briefly listed
    as a third way (batch 23 (2026-08-01) corrects it, against `scan.rs`, where that arm is
    explicitly excluded): that certificate is exact about a position BELOW where the walk stopped,
    so accepting it would assert "buf ends at the real end" about a buf that runs past it. The
    premise leaves one narrow, documented miss: a viewport anchored exactly at the block-aligned
    real end of a truncated source. An accurately sized source is unaffected — `buf_end == self.size` answers it
    without needing a witness at all. **Correction (fix round (2026-07-27), P3-FINAL-b):** an earlier version of this
    sentence derived the consequence from
    "a read loop's own position only ever advances to a block's own start" — true of every touch
    AFTER the first, but the first touch involves no advancing at all (`p` starts at `buf_end`,
    not generally a block start), so that reasoning did not actually cover the case it was about.
    The rigorous version lives in `edge_context`'s own doc comment (`document.rs`): `block_start <=
    p <= real_len` always holds on this loop, on every touch including the first, so an empty touch
    requires `block_start == real_len` exactly — the source's real end must fall EXACTLY on a
    block boundary — at this project's own default `block_size`
    (1 MiB) an approximately 1-in-2^20 coincidence, so `edge_context`'s own `ahead_hit_real_end`
    almost never certifies a genuinely truncated, non-block-aligned source ON ITS OWN.
    **Reconciled, then re-adjudicated (restructure R3, 2026-07-27, fix round; `edge_context`
    itself is UNCHANGED throughout — see below):** R3's own first attempt made `Document::
    viewport`'s own `buf_is_true_eof` a pure witness check, replacing `buf_end == size()` outright
    — `fill_lines` (`scan.rs`) verified a merely-short block via a separate, discarded probe at
    the next block's own start, trusting it as the real end if that probe came back empty. An
    adversarial review found this unsound: the probe answers "is the NEXT block empty", not "is
    THIS block's own gap empty", and those are different questions a block-indexed cache cannot
    bridge. Once block `idx` is cached holding `N < block_size` bytes, positions
    `[idx*bs + N, (idx+1)*bs)` — the gap between the short answer's own length and the next
    block's own start — are structurally unreachable through that cache entry, permanently: the
    cache memoizes whatever the source returned, including a short answer, and never re-asks. A
    fixture where the file's real end happens to fall exactly on the next block boundary (`block
    0` answers 5 of its own 8 bytes, `block 1` is genuinely empty, but real bytes sit at `5..8`
    regardless) makes the probe certify a cut that is not the end — the identical hazard
    `ShortFirstBlock` (below) already guards against, just with the gap's own ground truth
    flipped. **Batch 12 (2026-07-29) closed the docket by RESOLVING the input rather than picking a
    side, and the ruling below is superseded for any CONFORMING source.** The divergence only ever
    existed because a memoized short block's own tail was unreachable, so "is this cut the real
    end?" had no answer and each consumer guessed — nav guessed "end" and fabricated `$` at cuts
    that were not ends, the viewport guessed "not end" and refused matches that were real. Two
    attempts to reconcile by choosing a winner both failed on their own terms: R3 moved the VIEWPORT
    onto nav's side (reverted as unsound — the `helloXmo` probe shows that mechanism fabricates),
    and batch 11 moved NAV onto the viewport's side (sound, but it paid for agreement by giving up
    `$` at genuinely truncated ends).

    `BlockCache` removes the premise instead: a short block's withheld tail is
    re-asked from the source, and the answer is durable — more bytes close the hole permanently, an
    empty answer certifies the real end at that exact offset (`source.rs`: an empty buffer means
    `offset >= size`) and rides with the block's own bytes (`cache::Block::ends_data`) so it is paid
    for once and can never be read for a length it was not earned for. The two cases that were
    "structurally indistinguishable" are only indistinguishable to a cache that refuses to ask.
    Consequences, all measured: nav and the viewport now AGREE by knowing rather than by one
    yielding; `$` at a truncated end is found again; and a match above a former gap resolves its
    TRUE line anchor, so `docs/architecture.md`'s `NavOutcome` contract ("`top` the matched line's
    own anchor") is satisfied literally.

    **Batch 13 (2026-07-29) retired the attempt cap, so there is no residual case left.** The
    completion loop runs until the block is full or the source certifies its end — bounded by the
    hole's own width, abandonable at every pass boundary, and resumable because the accumulator
    belongs to the cache and every ending hands it back. (Batch 13 achieved that durability by
    republishing the accumulated prefix on every pass, which batch 15 (2026-07-30) removed as
    quadratic; `docs/block_cache.md` carries the current lifecycle.) A source dribbling one byte per read is legal under "up to `len`", and it used to
    go permanently blind five bytes into every block: the search then crossed the invisible
    remainder and answered `FoundMatch` with a `top` that could render neither the line nor the
    match. What replaced the cap is not a more careful consumer but a smaller state space — an
    unresolved block is no longer something this cache can hand out, so nothing downstream needs a
    representation for one. Batch 9's own gap-jump and batch 11's `Edge::Cut` rule both remain, as
    the honest treatment of the short block that is genuinely real: a source whose `size()`
    overstates its data, ending inside a block, where the descent and the cut are exactly right.

    What R3 *did* buy, real and narrower: **empty-only certification**. `fill_lines`'s own
    `FillOutcome::End` is never inferred from a short block's own reported
    length — it comes from an observed EMPTY read, either directly (`Fetched::Empty`) or carried on
    a `Fetched::Short` whose block the source itself certified (`Block::ends_data`, batch 14
    (2026-07-30); `ReadOutcome::Short` drops it, having no consumer that needs it). What no variant
    may do is certify from a length (`meter.rs`'s own doc comment has the type-level
    law). `Document::viewport`'s own `buf_is_true_eof` is `matches!(fill_outcome, FillOutcome::
    End(_)) || buf_end == self.size` — TWO disjuncts, not a witness replacing the size check.
    The witness half certifies a real end that happens to land exactly on a block boundary without
    ever consulting `size()` to get there (so a truncated source, where `size()` overstates
    reality, is still handled correctly there); the restored `buf_end == self.size` half is safe
    on its own terms, independent of whether `size()` is accurate elsewhere, because `buf` can only
    ever physically reach `self.size` if nothing shorter stopped it first — exactly the ordinary,
    accurately-sized case (a small file entirely within one block) the witness alone would have
    missed. `hay_reaches_eof`'s own third disjunct, `buf_is_true_eof`, is sound under either half:
    when the witness half holds, the reasoning below (the "further" paragraph) applies directly;
    when the size half holds instead, `edge_context`'s own separate `ctx_after` fetch is gated on
    `buf_end < self.size` (`Document::viewport`'s own doc comment), so `buf_end == self.size` means
    that fetch never ran at all and `ctx_after` is empty by construction the same way. `edge_context`
    itself was never widened to match `fill_lines`'s own (reverted) attempt, and stays that way on
    purpose: `ctx_after`'s own bytes feed pattern matching directly, so the identical splicing risk
    `ShortFirstBlock` (below) exposed in `fill_lines`'s own first attempt would fabricate lookaround
    context there instead of merely widening a certification that turned out unsound anyway.
  - **An over-cap, assertion-bearing match** stays a documented miss, on a perfectly healthy
    source or not (restructure R5, 2026-07-28, superseding batch 5 (2026-07-27) fix round P1-1's
    own CONTROLLER RULING — accept and disclose — with the assertion-aware acceptance that ruling
    already named as the planned refinement): a candidate whose own body extends
    `>= MAX_MATCH_LEN` bytes past the row's own accept end can never have enough margin for an
    assertion that needs one, widened or not, so the belt still drops it regardless of truncation.
    **An assertion-free over-cap match (`/a+`, `/.*`, `/[a-z]+`) paints again** — the belt's own
    margin requirement is waived outright for a pattern that carries no trailing assertion at all
    (a hay cut cannot manufacture a body), recovering the whole-row painting `d22c106` had before
    P1-1's own fix round temporarily cost it. The painted SPAN can still be position-dependent, but
    for a narrower reason than before: `accept_and_hay_for_row`'s own windowing widens a row's hay
    by `MAX_MATCH_LEN + CTX_AHEAD - 1` past `accept.end`, clamped to `full_hay`'s own edge — a row
    far from that edge gets a row-hay whose own `body_limit` caps the reported span short of the
    file's real end (the match is still genuine and still paints, just not all the way to EOF in
    one row's own enumeration; a later row's own enumeration, closer to the edge, reports the
    remainder). Nav (`n`/`N`) now agrees with the highlighter on the assertion-bearing miss (both
    reject an unverifiable over-cap candidate) and on the assertion-free find (both report it) —
    the F-review P2-4 fabrication this section used to cite as "nav already fabricates on this
    exact class of input" is itself fixed by this same restructure (see the windowed matching
    model's own two-limits section, above).
  Finally, finding #7: a zero-width match sitting exactly at `buf`'s own TRUE end (not merely a
  budget-truncated stop) is a legitimate, in-window position — `line.rs`'s own `col` fallback
  already renders it once admitted — but was missing from the highlighter's ORDINARY pass even
  though `n`/`N` and the current-match resolution both already found it (an ordinary-vs-current
  parity break); the trailing newline's own phantom line (batch 4, finding #2) is excluded there
  exactly as it is everywhere else — an `\n` immediately before that position means no real line
  sits there to highlight.
- **The background sweep is correct below the `MAX_MATCH_LEN` cap, with no further limit of its
  own.** `$` and ASCII `\b`/`\B` fall out of the windowed model's own existing lookahead margin for
  free (a full `MAX_MATCH_LEN` past a maximal-length candidate's own start, above — the old
  `MAX_MATCH_LEN - 1` margin left it with nothing real past its own end to check, closed since);
  Unicode-aware `(?u:\b)`/`(?u:\B)` need `CTX_AHEAD - 1` bytes more still (finding #3), for the
  identical reason `CTX_BEHIND` is 4 rather than 1 on the other side. `^`/`\b`/`\B` also need one
  more mechanism on the near side: `SweepAnalysis` keeps up to `CTX_BEHIND` real look-behind bytes
  permanently at the front of its own carry, updated after every report (not just at a window's
  own boundary), so every zero-width assertion is evaluated against real content at every seam,
  not just the file's own true start. One narrow, honest residual, **now scoped to assertion-
  BEARING patterns only** (restructure R5, 2026-07-28, the F9 seam-cost retirement): after a block
  failure forces `give_up` to skip past the whole failing block (floor semantics, above), the real
  bytes immediately preceding wherever the sweep resumes are unknowable without a read `give_up`
  deliberately avoids. A pattern whose match needs a verified look-behind there (a leading
  assertion — `^`, `\b`, `\B`) still excludes that position, and up to `CTX_BEHIND - 1` more after
  it, from the next several reports' own accept ranges instead of guessing (finding #8: a
  straddling multi-byte character can leave more than just the landing position itself
  unverifiable, so the exclusion persists across as many separate reports as it takes to clear —
  derived per candidate from its own distance past the seam, not a persisted counter). A BARE
  LITERAL, or any pattern with no leading assertion at all, is unaffected: its own start is
  trustworthy regardless of what the unread predecessor was, since a hay cut cannot manufacture a
  body, so it is found immediately, right at the seam. Either way, the excluded case is only ever
  a possible miss, never a false match, and only immediately after a `failed` sweep's own skipped
  region.
- **The on-demand `n`/`N` scans are also correct**, via the identical kind of real look-behind
  bytes: `SearchForward` chains up to `CTX_BEHIND` of them across every carry-shrink seam for the
  whole scan's life, not just its own origin; `SearchBackward` re-fetches them fresh every
  iteration instead, since its own low edge is a new position each time, not one already carried
  forward — including, deliberately, up to `CTX_BEHIND` bytes *below* a bounded wraparound leg's
  own floor: those bytes are perfectly real and already cached, `floor` is a boundary on what may
  be *accepted* as a match start, not on what data exists, so reading them for context (never for
  a reportable start) violates nothing (see `SearchBackward`'s own doc comment in `scan.rs` for
  the full derivation, including the load-bearing self-hit-wraparound case that ruled out
  excluding the floor position instead). `SearchBackward` additionally reads past its own `hi`
  (up to `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes — finding #3 widened this the same way it widened
  every other lookahead margin in this document — as lookahead only, never an acceptable match
  start) before its very first iteration, so a match starting below `hi` but extending past it —
  a *straddler* — can be confirmed by this leg alone (see the wrap policy, above); the same read
  closes `$`'s own chunk-boundary gap for this scan too, since it hands the first iteration real
  bytes to check `$` against instead of an artificial hay edge. **Forward** shares the identical
  shape now too, via `safe_to` (the leftmost-match margin above, a genuine end-margin the same way
  the sweep's own `safe_to` always was): a match up to and including `MAX_MATCH_LEN` long is only
  ever accepted once `CTX_AHEAD` real bytes *past its own end* have been read (one byte sufficed
  for `$`/ASCII `\b`/`\B`, closed by finding #10; a Unicode-aware `(?u:\b)`/`(?u:\B)` needs the
  FULL following character, `CTX_AHEAD` bytes, closed by finding #3), so `$`/`\b`/`\B` are always
  checked against genuine content there, never hay's own current (possibly artificial) edge — the
  mid-file chunk-boundary false positive that used to be possible for any match length, the
  narrower one that used to remain at exactly `MAX_MATCH_LEN`, the narrower one still that used to
  remain for a Unicode-aware assertion at any length up to the cap, and — fix round (2026-07-25),
  F4 — a bounded wraparound leg's own reach at its own artificial `limit` (never the true file
  end), are all now closed, symmetrically with the floor-context reach two paragraphs up: `limit`
  bounds what may be *accepted* as a match start, not what data exists past it, so `SearchForward`
  peeks up to `CTX_AHEAD` real bytes past a bounded leg's own `limit` too — consultable for a
  trailing assertion, never reportable as part of a match's own span: a candidate whose own BODY,
  not just an assertion, would need those bytes to complete is skipped and retried from its own
  start plus one, within the same read, rather than accepted or given up on outright — the
  identical "step past it and keep looking" rule `find_all_starting_in`'s own span restriction
  already applies
  (fix round 2 (2026-07-25), R1: an earlier version of this paragraph said only "stays correctly
  unreported," true of the *outcome* whenever no other candidate exists, but silent on the retry —
  a leg already at its own bound has no next read coming, so skipping the retry could lose a
  later, wholly-in-bounds alternative the way it briefly did before R1). **The residual this used to carry is CLOSED** (batch 7
  (2026-07-28), findings #1/#3): the retry recovered a *later* start but never a *shorter*
  alternative at the *same* start, so `abcQ|abc` over data starting `abcQ` at `limit = 3` was a
  documented miss even though `abc` alone is wholly in-bounds. This document already named what
  closing it would take — "a different search API (`regex-automata`'s `Input::span`, not exposed
  by `find_at`), a dependency-shaped change" — and that is what landed. There is no retry at this
  boundary any more, and no same-start blind spot: the engine is asked directly for the leftmost
  match whose own end fits, and returns the completion that fits when one exists.
  Batch 6 (2026-07-28), finding #2: this peek's own byte-gathering is not atomic either — an
  `OutOfBudget` cutoff partway through used to be silently treated as "nothing more to see,"
  making the *verdict itself* budget-dependent (`Exhausted` at a low budget, `Found` at a high
  one, on the identical file) — it now resumes across as many `step()` calls as it takes
  (`FwdPhase::Peek`, `scan.rs`), so the answer no longer depends on how much budget any one call
  happened to have left when it reached the boundary.
- **The end rule is a BOUND on the search, not a verdict on candidates the engine already chose**
  (batch 7 (2026-07-28), findings #1 and #3 — replacing batch 6's own deferral rule, which is
  retired along with the walk it steered). Every windowed search here asks
  `regex-automata` for the leftmost match that starts inside the accept range **and ends at or
  below what this window may report on** — `min(body_limit, len − CTX_AHEAD)` behind a cut, the
  two end conditions of the end rule restated as a position. Look-around assertions are still
  decided against the *whole* hay, margin bytes included: `Input::span` bounds where the match may
  fall, not what the engine may look at, which is the distinction this whole document is about and
  is now enforced by the primitive rather than reconstructed after it. A leading assertion's own
  margin is the same idea on the other side — the search floors at the lowest start whose
  look-behind is decidable, applied only when the pattern can carry one.

  The design this replaced proposed candidates and rejected them one at a time, which cost
  correctness at both ends of the same walk. Rejecting and retrying at `s + 1` re-scanned the
  remaining hay per start position: for a long homogeneous run under an assertion-anchored
  pattern (`a+\b`), every one of N starts re-found the SAME rejected ending position, O(N²)
  synchronously inside one `step()` call, blocking cancellation for the duration — measured at
  0.77s/2.62s/9.71s for 20k/40k/80k bytes at a *bounded leg's* final boundary, where batch 6 left
  the retry in place deliberately. Stopping the walk early at the first undecidable candidate —
  batch 6's own fix for the ordinary mid-file case — cost the opposite thing: every later
  candidate in the hay went unexamined, and since the carry keeps only its last
  `MAX_MATCH_LEN + CTX_AHEAD − 1` bytes, an ordinary sub-cap match sitting further along was
  discarded outright rather than deferred. `A[A-Za-y]*(?u:\b)|needle` with a needle 5,000 bytes
  into a 10,000-byte block reported *nothing* against a whole-file oracle that found it. Both are
  gone, and neither can recur in the same shape: there is no retry loop to be quadratic and no
  early return to skip past, because there is no per-candidate rejection step at all.

  A candidate that is genuinely undecidable in this hay — a trailing assertion with too little
  margin — is simply not among the matches the bounded search returns, so the walk reports what it
  honestly is: nothing reportable *here*, read on. A later call rebuilds the hay with more bytes
  and asks again. What is NOT promised, and never was, is that the undecidable candidate itself
  survives to be re-offered: if it is longer than the carry retains, it is an over-cap match under
  a trailing assertion, which this document already documents as a miss.
- **A zero-width match exactly at the file's own true end is a legitimate match everywhere the
  search can reach that position — unless the real byte immediately before it is `\n`, in which
  case that position is the trailing newline's own *phantom* and is never a match, regardless of
  pattern.** Accept ranges
  are otherwise half-open and exclude a start equal to the hay's own end — the same "a slice's own
  edge is unconditionally eligible" hazard this whole section exists to guard against — but at the
  *genuine* file end (never merely a bounded leg's own `limit`), `$` unconditionally holds, so
  `SearchForward`, `SweepAnalysis`'s own final window, and `SearchBackward` — the *far-end* leg
  (`hi == size`) and, since batches 14-19, ordinary legs too, at the three probe positions the
  bullet below enumerates — each widen their own accept range by exactly one position there,
  checking the real predecessor byte first. ("In all three places" is what this bullet used to
  say, from when the backward side had one such position; batch 22 (2026-08-01) reconciles it with
  the bullet below, which had already contradicted it.) Nothing else could start at that position and still complete — there is
  no more hay past it for a non-zero-width match to consume — so this can never admit anything but
  the intended zero-width one, or reject anything but the phantom. `pattern$` on an unterminated
  file (no trailing `\n`) is a legitimate zero-width match there; so is `^$` on an empty file,
  which has no predecessor at all and so is never the phantom either. A file that *does* end in
  `\n`, though, has a "line" after that final newline which does not exist — the identical doctrine
  `docs/architecture.md` already states for `goto_line` ("a line start that lands exactly at EOF is
  a trailing newline's phantom, not a real line") applies here too: on `b"a\n"`, `$` alone
  genuinely matches at both 1 (before the `\n`, real) and 2 (the true end, by raw regex ground
  truth) but only the first counts, and `^$` on the same file counts zero matches, not one, since
  its only candidate position is the rejected phantom.
- **`SearchBackward`'s own near-`hi` leg distinguishes "the wrap's own far end" from "leg 1's own
  `hi` happening to coincide with EOF because the cursor is already parked there"** via the choice
  of constructor (restructure R6: formerly a `wrap_leg` flag, now `new_wrap_leg` vs `new_leg`):
  `new_wrap_leg` only for the wraparound leg (`hi = size`, unconditionally), `new_leg` for the
  origin-relative leg (`hi = origin`, exclusive by construction the same way every other match
  shape already is). Only a genuine wrap leg may accept a
  zero-width match exactly at `hi`, so repeating a backward search for a zero-width EOF match now
  finds it via a self-hit-driven wrap (`wrapped: true`) — the same shape every *other* match
  already gets — not directly (`wrapped: false`) the way an earlier version of this document
  described. That description called the difference cosmetic: "the landing position is identical
  either way, only the wrap notice would differ." True only in a file with a single match — the
  moment a second, earlier match exists (`b"a\nb"`, pattern `$`, matching at both 1 and 3),
  admitting the self-hit instead of excluding it makes that earlier match unreachable (leg 1
  returns the self-hit immediately and the search never even tries the wrap that would have found
  it), a real navigation defect, not a cosmetic one. The same model, applied consistently, reaches
  an empty file too: `hi == origin == size == 0` there, so leg 1's own exclusive `hi` still
  excludes the position-0 self-hit, and a backward `^$` search on an empty file also reports
  `wrapped: true` — a wrap notice for a file with nowhere to actually wrap, a deliberate
  consequence of one uniform rule rather than a special case carved out for size zero.
- **An ordinary backward leg reaches a real endpoint below its own origin too** (batches 14-19,
  2026-07-30/31). This section used to describe end-anchor handling as the far-end wrap leg's
  business alone, which was true when `hi` never moved: a plain leg's `hi` IS the caller's
  exclusive origin, and a zero-width match there is the self-hit `new_leg` exists to refuse. It
  stops being true the moment `hi` DESCENDS — to a source-certified end, or to a claimed size a
  truncated file never reaches — because that position was never the cursor's. A plain leg now
  probes it, at three places: the payload loop's own window, the empty-slice path it would
  otherwise descend past, and the terminal position the loop never visits at all (an accurately
  sized empty file arrives there with `hi == floor == 0`). The gate is uniform and is what
  preserves the self-hit contract: the position must be the real end AND strictly below the leg's
  ORIGIN, and matching exactly at the leg's own inclusive `floor` is legitimate — `floor` is a
  match-start bound, not a look-behind bound.
- **A short look-behind read does not always force a descent.** The rule below is about a read that
  fails to PROVE `hi`; a read that comes back short but CERTIFIED (the source itself answered empty
  past it) has proved the endpoint rather than failed to, and is accepted whenever the bytes it did
  deliver cover the requested context. The endpoint probe reads its own `CTX_BEHIND` look-behind
  for exactly this reason: `zero_width_at_high` gates look-behind adequacy unconditionally, so a
  probe without it can only ever answer at BOF.
- **The wrap leg's own `hi == cache.size()` must be CERTIFIED by a real read before its zero-width
  check may run at all, not trusted from `cache.size()` alone** (finding #1): `cache.size()` is a
  fixed snapshot for a real source's whole life (`PreadSource::size`, captured once at open, never
  re-stat), so a file truncated out from under the pager leaves it stale — `hi` claims a position
  past all real data forever after, with no later call ever correcting the comparison on its own.
  The terminal check's own small look-behind read is what actually proves `hi`: an UNCERTIFIED
  short or empty result means `hi` is fiction, and the leg **descends** to the point the read
  actually reached
  (the same forward-into-the-truncation-gap descent [budgeted scanning](budgeted_scanning.md#the-truncation-policy-when-a-sources-real-data-ends-below-its-own-claimed-size)
  already documents for backward-direction loops generally, applied here to a single
  zero-width-candidate position rather than the main payload search) and retries there, budgeted
  like any other read, until the gather **reaches `hi`** or the leg runs out
  of its own legitimate range (`hi` below `floor`). Reaching `hi` is the test, not "a read comes
  back FULL" as this paragraph used to say (batch 22 (2026-08-01), reconciling it with the bullet
  above): the descent is gated on `p < self.hi.at()` in `scan.rs`, so a `Fetched::Short` whose
  bytes still cover the requested context proves the position exactly as a full read does, and a
  certified short answer that stops below `hi` descends precisely once — to the position the
  source certified, where the next gather then completes. Descending once is not enough on its own: the
  check must **re-arm** so it runs again at the corrected position once reached, or a genuine
  zero-width match sitting exactly at the real end is silently missed — a false `Exhausted`,
  reachable no other way, since the ordinary payload search never treats `hi` itself as an
  acceptable start. On `b"a\n"` claiming size 100, the wrap used to return `Found` at the
  fabricated position 100; correctly, it now descends to the real end (2, preceded by `\n` — the
  trailing newline's own phantom, above, so still not a match there) and finds the real `$` at 1
  via the ordinary main loop instead.

## Smartcase

`SearchPattern::compile` is case-insensitive unless the typed pattern
contains an uppercase character — vim's own rule, applied to the *raw*
text the user typed. An uppercase character sitting inside a regex escape
(one that means nothing case-sensitive on its own) still flips the whole
pattern to case-sensitive; that is a deliberate, documented simplification,
not a hand-rolled escape-aware scanner. The folding this triggers is itself
ASCII-only — see [the raw-byte matching ruling](#the-windowed-matching-model)
above for what that means for a non-ASCII case pair.

## On-demand navigation: `n` / `N` and the wrap policy

`n` and `N` walk to the next (or previous) match via
`Document::search_next`, built on the same budgeted-scan machinery as
every other jump (`SearchForward`/`SearchBackward` in `scan.rs`, siblings
of `ForwardScan`/`BackwardScan` — see
[budgeted scanning](budgeted_scanning.md)): an attempt that resolves
within the interactive budget lands immediately, one that cannot becomes
the same cancellable, progress-reporting pending scan every jump falls
back to once it outruns that budget. A forward match can legitimately
defer its own return by up to `MAX_MATCH_LEN` (~4 KiB) of extra reading
past where it first completes — the cost of the leftmost guarantee above
(`safe_to`), not a bug: a shorter, later-starting candidate must wait
until every earlier position has had its own fair chance to resolve
before this scan can be sure nothing further left still wins.

The search is two legs, tried in order:

1. **The origin-relative half** — forward `[origin, EOF)`, backward
   `[0, origin)` — is tried first. Forward's `origin` is *inclusive* (the
   caller advances past a just-found match by passing
   `current_match + 1`); backward's is *exclusive* by construction, which
   is what excludes a self-hit — the cursor re-finding the match it is
   already parked on — with no extra arithmetic needed. Backward's own leg
   reads past its own `origin` boundary too (as pure lookahead, never an
   acceptable match start of its own) so it can *confirm* a match that
   starts below origin but straddles it — see `SearchBackward`'s own doc
   comment in `scan.rs`.
2. **The far half wraps around**, tried only once the first leg exhausts
   its own side without a match. **Forward's** far leg widens its own
   bound by `MAX_MATCH_LEN` past the origin — the identical cap the
   windowed model above uses — so it can still catch a match that
   *straddles* the origin seam (starting on the side leg 1 already ruled
   out, but needing bytes past the seam to confirm), something forward's
   own leg 1 cannot do (it never reads below its own inclusive `origin`).
   **Backward's** far leg needs no such widening: leg 1's own straddle
   read (above) already rules out every straddler below origin, so the
   far leg's floor is the plain `origin`. Either way, nothing is ever
   double-reported: a straddler is found by exactly the leg that owns its
   start.

A match found by the second leg sets `wrapped: true` on the outcome, which
the UI surfaces as a one-line "search wrapped" notice; a match found by
either leg lands the matched line at the top of the viewport and becomes
the origin for the next `n`/`N`. `N` reverses whichever direction `/` or
`?` established; `n` repeats it — plain vim algebra, independent of which
prompt was used. Only once *neither* leg finds a match anywhere does the
search resolve to `Exhausted` — no answer exists — surfaced as a "pattern
not found" notice, with the viewport left exactly where it was.

## The background sweep and its bounded summary

Finding "the next match" and reporting "how many matches, and roughly
where" are different problems with different costs, so they are two
independent mechanisms rather than one stretched to cover both: `n`/`N`
above only ever has to look as far as the nearest match, cheaply, on the
interactive path; a running total and a density map need to look at the
*whole* file, which is exactly the unbounded background work this engine
already has a shape for.

Every committed search starts (or restarts, from byte 0, even for an
unchanged pattern) a `SweepAnalysis` — the first analyzer built on
`crate::analyzer`'s driver, a generic background-analysis loop generalized
out of `StatusWorker`'s own (see [architecture](architecture.md)'s "Seams
for what comes next", and [concurrency](concurrency.md) for the
owned-task shape it still follows). It reads one `SEARCH_WINDOW` at a time
through the block cache and publishes a `SearchSummary`:

- `matches` — the exact running total found so far (`u64`, saturating).
- `buckets` — a fixed **2 KiB** histogram (`Arc<[u16; 1024]>`: 1024
  saturating counters, 2 bytes each) of how many matches fall in each of
  1024 equal byte ranges spanning the file. Together with a handful of
  scalar fields, this is the entire per-search memory cost of the
  summary — it does not grow with file size or match count. The
  scrollbar reads it to paint match-density ticks on the gutter.
- `generation` — a stamp minted fresh by every `start_search_sweep` call
  (never 0, which is reserved for "no sweep has ever run"), so a snapshot
  left over from a search the user has since replaced is never mistaken
  for the current one.
- `scanned_up_to` / `done` — how far the sweep has gotten, and whether it
  has finished; the status line shows a `searching {pct}%` tail while it
  has not.

`matches` enumerates every match **start**, including overlapping ones, since that is what
"how many times does this pattern occur" means for a self-overlapping pattern — the highlighter
(see "Reaching the screen", below) shows **non-overlapping** regions instead, the same
convention `regex::bytes::Regex::find_iter` itself uses. The two are not the same number by
design: `/aa` against `aaaa` counts 3 (starts at 0, 1, and 2) but highlights only 2 regions
(`[0,2)` and `[2,4)`). Which non-overlapping tiling the highlighter shows is itself not fully
canonical past `MAX_MATCH_LEN` (fix round (2026-07-25), F5, accepted residual): the per-row
search walks from each row's own `accept.start`, so a self-overlapping pattern's own tiling phase
can shift with `hscroll`/`line_start` once that exceeds `MAX_MATCH_LEN` — a line longer than the
cap, scrolled to different columns, can show a different (still valid, still non-overlapping)
tiling of the same self-overlapping match. Pinning one canonical phase would need reintroducing
the unbounded whole-line walk finding #5 removed to compute it.

## Sweep resilience: floor semantics

A block that fails to read is retried, with backoff, the same bounded
number of times as any other background reader in this engine
(`ANALYSIS_RETRIES`, 3) before the sweep gives up — but *only* on the one
failing **block**, not the search as a whole, and *only that block*: it
skips exactly past the failing block's own end, no further — never merely
the `SEARCH_WINDOW` unit it was being read through when the failure
happened (skipping only a window, inside a block bigger than a window,
would land back inside the same dead block, costing another full retry
round before the sweep ever escaped it), and never a whole extra window
*past* the block either, even when the block itself is much smaller than a
window — landing past a swath of perfectly healthy, readable blocks would
discard every match in territory that was never actually unreadable, an
undercount with no corresponding real failure to justify it. Marks the
summary `failed` and keeps scanning the rest of the file from there. A
`failed` summary's `matches` count is honestly presented as a **floor**,
not an exact total — the status line prefixes it with `≥` — because the
one block skipped this way could have held matches nothing will ever find.

Matches are recorded incrementally, block by block within a window,
specifically so this floor stays honest: an earlier block's already-
confirmed matches are locked into `matches`/`buckets` immediately, not
held back until the whole window finishes, so a *later* block's failure in
the same window cannot erase an earlier block's real progress.

This is a distinct regime from a block that reads back **short or empty with no error at all** —
a file that shrank below the size `size()` captured at open time, `BlockCache::block`'s own
documented "short at EOF, empty past EOF" contract. That case is not a failure to retry or give up
on; it is the sweep's own genuine, honest end, discovered mid-window rather than known in advance
— the [truncation policy](budgeted_scanning.md#the-truncation-policy-when-a-sources-real-data-ends-below-its-own-claimed-size)
every read loop in this engine follows applies here identically: the sweep resolves one final
report over whatever it already has in hand (treating the discovered position as the true end,
the same way a genuine `size()` match would be treated) and marks itself `done`, rather than
spinning forever on a `size()` claim the source will never actually reach.

## Reads stay bounded

The sweep reads through the block cache's non-promoting `warm()` path (see
[block cache](block_cache.md)), like the line index and prefetch — a
full-file sweep cannot evict the interactive working set. It is also
sequential by construction: `SweepAnalysis::step` awaits one block before
requesting the next, so the sweep itself ever adds at most **one**
outstanding OS read at a time, contending for the same shared
`--read-concurrency` permit (default 16 — see
[concurrency](concurrency.md)) every other reader in the engine already
waits on, not a second, uncapped concurrency budget of its own.

## Reaching the screen

Two render-side consumers turn the pattern and the sweep's summary into
what the user sees: viewport rows run the compiled pattern and restyle the
matches found (reversed; the current match additionally bold); the
scrollbar gutter turns `buckets` into a row of dim tick marks alongside the
position thumb, from the sweep's own summary, no extra read of its own at
all.

**Highlighting cost is bounded by the visible window, not by line length or
buf size** (batch 4 (2026-07-24), finding #5). Each row derives its own
visible BYTE window from its own `[hscroll, hscroll + cols)` column range
(`line::visible_byte_range`, a bounded token walk reusing the same
tab/control/width arithmetic `layout_row_with_marks` already does), then
searches only a small range around it: `accept` is that window expanded
leftward by `MAX_MATCH_LEN` (clamped to `buf`'s own span, never to the row)
— the leftward reach is what still finds a multiline match whose own start
sits on an *earlier* row, crossing as many newlines as it needs to, since
this is a flat byte range in `buf`, not a row-scoped one. The actual search
itself runs over a wider slice (`accept` expanded by `CTX_BEHIND` behind and
a further `MAX_MATCH_LEN + CTX_AHEAD - 1` ahead — batch 5 (2026-07-26),
finding #6, widened from a bare `MAX_MATCH_LEN`, "Reaching the screen"'s own
sibling correction above has the fabrication this closes — clamped to the
combined `ctx_before + buf + ctx_after` hay described above, not to `buf`
alone — reaching into the real `ctx_before`/`ctx_after` bytes at `buf`'s own
physical edges), so a match starting anywhere in `accept` resolves its own
trailing assertion against a full character's worth of real lookahead,
UNLESS the AHEAD-side fetch stopped short of that margin with no verified
true end to show for it (the resolved-criterion belt's own "must hold"
above has the two ways an edge earns that trust), in which case the belt
drops the candidate instead of trusting an unverified cut as real EOF
(finding #6's own second mechanism, a documented miss, never a fabrication)
— never rejected merely for extending past the row's own narrow window
otherwise; the *returned*
list is then filtered in two directions (fix round (2026-07-25), F3) — a
start past the window paints nothing of it regardless of its own end, and a
candidate *ending* before the window's own left edge paints nothing of it
either. Matches are
non-overlapping, so at most ONE result can straddle in from the leftward
`MAX_MATCH_LEN` zone and still satisfy the second filter; every other
survivor starts within the window-adjacent range — the returned count is
genuinely `win_bytes + 2` (fix round 2 (2026-07-25), R2 — corrected from an
earlier, imprecise `O(cols)`), where `win_bytes` is the row's own visible
BYTE window's own width (`line::visible_byte_range`'s own return value):
equal to `cols` for plain ASCII, up to `4 × cols` for ordinary multi-byte
text (a maximal 4-byte UTF-8 character rendering at width 1 is the worst
case), and — the identical exception `visible_byte_range`'s own doc comment
states — unbounded by `cols` in the presence of a long zero-width
(combining-mark) run. Not `O(MAX_MATCH_LEN)` either way. A row's own read
cost is unaffected — this bounds *work*, not *bytes fetched*: `buf` and its
edge context are still read exactly once per viewport, as before, and the
accept/hay ranges above only ever slice into bytes already in memory. Batch
5 (2026-07-26), finding #6's own `ctx_after` widening adds a SEPARATE,
one-time cost: up to `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes (`O(cap)`),
fetched ONCE per `Document::viewport` call regardless of how many rows it
renders, never once per row — the per-row `win_bytes + 2` bound above is
unaffected, since every row's own `accept`/`hay` slice into that same
already-fetched `ctx_after` rather than fetching their own. The physical-read cost is
ALIGNMENT-dependent, and this paragraph is the same claim as the one under
"boundary context" above — see there for the full statement (batch 9
(2026-07-29), finding #4: this copy still said the widening "costs ZERO
additional physical reads" unconditionally at 1 MiB, having been missed when
batch 8 corrected its twin; with enough trailing data the measured count is 2,
not 1). In brief: zero additional reads when `buf`'s own end still has 4099
bytes of its own block left or the file ends before the boundary, at most one
more otherwise, and "a few blocks" only near a `block_size` of 4 KiB or
smaller — where the count grows with `buf`'s own size, still flat in `rows`. The net
effect: highlighting a single default-sized (1
MiB) block containing one giant dense-match line now costs work proportional
to the visible window's own byte width — independent of the line's own
length, how many matches it contains, or where in the buffer the row sits —
where the pre-restructure code enumerated and re-filtered every match in the
whole fetched buffer, once per row, on every repaint.

The same absolute match can legitimately surface from more than one row's
own enumeration this way (a multiline match's own leftward reach can cross
into an earlier row's own `accept` range too) — this is convergent, not
duplicated: each row clips whatever it finds down to its own line bounds
(`clip_to_row`), so a row the match does not actually intersect drops it
silently regardless of whether that row's own enumeration happened to see
it. `find_all_starting_in`'s own documented same-start alternation residual
(above) is inherited by this per-row search — both call the identical
method — but does not reach ordinary, within-`MAX_MATCH_LEN` matches any
differently than the old whole-buf computation did: the search itself always
uses the *wide* reach (`accept` plus the further `MAX_MATCH_LEN + CTX_AHEAD -
1`/`CTX_BEHIND` margins), never the row's own narrow `accept.end`, as the
bound `find_all_starting_in` checks a candidate against, so an alternative
within the documented cap never overruns it on account of the row's own
window being narrow.
