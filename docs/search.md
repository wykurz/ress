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

A pattern is compiled once (`SearchPattern::compile`, in
`ress-core/src/search.rs`) and then matched against fixed-size windows
rather than the whole file at once — the same "never read the file in one
gulp" discipline every other engine scan follows (see
[budgeted scanning](budgeted_scanning.md)). Each window is a
`SEARCH_WINDOW`-byte (64 KiB) **core** plus `MAX_MATCH_LEN - 1` (4 KiB,
minus one) bytes of trailing **lookahead**, so a match that starts inside
one window's core but needs a few more bytes to complete is still found
whole, without re-reading anything — the lookahead of one window is the
next window's own leading bytes. A match's *start* is attributed to
exactly one window, the one whose core contains it, via an accept range
that keeps consecutive windows from ever double-reporting or dropping a
match that straddles the seam between them.

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
  found.** The lookahead is exactly enough to complete a match starting at
  a window's very last core byte and running the full cap; a genuinely
  longer one can straddle a seam with too little lookahead on either side
  to ever complete. This bounds the *match itself*, not line length or
  file size.
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

## Anchored patterns: `^` and `$` are line anchors

`^` and `$` are compiled as **line** anchors (`multi_line(true)`), matching right after or
before any `\n` as well as the file's own true start/end — not merely the start/end of whatever
window or slice a particular consumer happens to be holding at the time. That distinction
matters because every consumer below hands the regex engine a *slice*, and a slice's own edge is
unconditionally `^`/`$`-eligible on its own regardless of `multi_line` (a haystack that happens
to start or end mid-line is still treated as *the* start or end by the regex engine) — so each
consumer carries real look-behind/lookahead context of its own around a chunk boundary, rather
than trusting the slice's own edge, to keep that from being mistaken for a real line boundary.

- **The highlighter is exactly line-correct.** Each displayed row's own slice, handed to
  `SearchPattern::find_all`, genuinely starts and ends at that line's own boundaries — no seam,
  no approximation, nothing to get wrong.
- **The background sweep is correct below the `MAX_MATCH_LEN` cap, with no further limit of its
  own.** `$` falls out of the windowed model's own existing lookahead margin for free. `^` needs
  one more mechanism: `SweepAnalysis` keeps exactly one real look-behind byte permanently at the
  front of its own carry, updated after every report (not just at a window's own boundary), so
  `^` is evaluated against real content at every seam, not just the file's own true start. One
  narrow, honest residual: after a block failure forces `give_up` to skip past the whole failing
  block (floor semantics, above), the real byte immediately preceding wherever the sweep resumes
  is unknowable without a read `give_up` deliberately avoids — a `^` match starting exactly there
  can be missed (never a false match, only a possible miss, and only immediately after a
  `failed` sweep's own skipped region).
- **The on-demand `n`/`N` scans are also correct**, via the identical kind of real look-behind
  byte: `SearchForward` chains it across every carry-shrink seam for the whole scan's life, not
  just its own origin; `SearchBackward` re-fetches it fresh every iteration instead, since its
  own low edge is a new position each time, not one already carried forward. `SearchBackward`
  additionally reads past its own `hi` (up to `MAX_MATCH_LEN` bytes, as lookahead only — never an
  acceptable match start) before its very first iteration, so a match starting below `hi` but
  extending past it — a *straddler* — can be confirmed by this leg alone (see the wrap policy,
  above); the same read closes `$`'s own chunk-boundary gap for this scan too, since it hands the
  first iteration real bytes to check `$` against instead of an artificial hay edge. **Forward**
  shares the identical shape now too, via `safe_to` (the leftmost-match margin above, a genuine
  end-margin the same way the sweep's own `safe_to` always was): a match shorter than
  `MAX_MATCH_LEN` is only ever accepted once at least one real byte past its own end has been
  read, so `$` is always checked against genuine content there, never hay's own current
  (possibly artificial) edge — the mid-file chunk-boundary false positive that used to be
  possible for *any* match length no longer is, for anything shorter than the cap. Two narrow
  residuals remain, both confined to a scan's own read BOUNDARY rather than an arbitrary
  mid-file accident: a match of *exactly* `MAX_MATCH_LEN` bytes can still land its own `$`
  confirmation precisely at whatever this scan has read so far, even mid-file — `safe_to`'s own
  margin is calibrated to exactly that length, with nothing left over — and, independent of
  length, a bounded wraparound leg reaching its own artificial `limit` (never the true file end)
  gets no margin at all there, so a match of any length ending exactly at that seam can also
  false-positive. Neither is a new class of approximation — both are the identical
  `MAX_MATCH_LEN`-cap-and-boundary approximation this document already accepts elsewhere, now
  narrowed to these two specific edges instead of any chunk boundary at all. `SearchBackward`
  also cannot read below a bounded wraparound leg's own floor for `^` context — the same "never
  read past the point it was told to stop" rule that already makes its `limit` byte-exact — the
  mirror image of the sweep's own `give_up` gap: a possible miss right at that one boundary,
  never a false match.
- **A zero-width match exactly at the file's own true end is a legitimate match, in all three
  places.** Accept ranges are otherwise half-open and exclude a start equal to the hay's own
  end — the same "a slice's own edge is unconditionally eligible" hazard this whole section
  exists to guard against — but at the *genuine* file end (never merely a bounded leg's own
  `limit`), `$` unconditionally holds, so `SearchForward`, the *far-end* `SearchBackward` leg
  (`hi == size`), and `SweepAnalysis`'s own final window each widen their own accept range by
  exactly one position there. Nothing else could start at that position and still complete —
  there is no more hay past it for a non-zero-width match to consume — so this can never admit
  anything but the intended zero-width one. `pattern$` on an unterminated file (no trailing
  `\n`) and `^$` on an empty file are both this case. `SearchBackward`'s own near-`hi` leg does
  not, by construction, distinguish "the wrap's own far end" from "leg 1's own `hi` happening to
  coincide with EOF because the cursor is already parked there" — the accepted, deliberately
  unfixed consequence is that repeating a backward search for a zero-width EOF match finds it
  directly (`wrapped: false`) rather than via a self-hit-driven wrap (`wrapped: true`, the normal
  shape for every other match): the landing position is identical either way, only the wrap
  notice would differ.

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
(`[0,2)` and `[2,4)`).

## Sweep resilience: floor semantics

A block that fails to read is retried, with backoff, the same bounded
number of times as any other background reader in this engine
(`ANALYSIS_RETRIES`, 3) before the sweep gives up — but *only* on the one
failing **block**, not the search as a whole: it skips past that whole
block — not merely the `SEARCH_WINDOW` unit it was being read through when
the failure happened — marks the summary `failed`, and keeps scanning the
rest of the file. Skipping the whole block matters because a block can be
bigger than a window (the 1 MiB default block over the 64 KiB window is
16×): landing just one window past the failure, inside the same dead
block, would cost another full retry round and give-up before the sweep
ever escaped it. A `failed` summary's `matches` count is honestly
presented as a **floor**, not an exact total — the status line prefixes it
with `≥` — because a block skipped this way could have held matches
nothing will ever find.

Matches are recorded incrementally, block by block within a window,
specifically so this floor stays honest: an earlier block's already-
confirmed matches are locked into `matches`/`buckets` immediately, not
held back until the whole window finishes, so a *later* block's failure in
the same window cannot erase an earlier block's real progress.

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
what the user sees, both computed only over bytes the viewport was already
going to read — never an extra scan of their own: viewport rows run the
compiled pattern over their own already-fetched bytes and restyle the
matches found (reversed; the current match additionally bold), and the
scrollbar gutter turns `buckets` into a row of dim tick marks alongside the
position thumb.
