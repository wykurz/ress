//! Search: pattern compilation (smartcase), windowed byte matching, and (task 5)
//! the background match-summary analysis. Matching runs over fixed windows with
//! `MAX_MATCH_LEN` overlap -- matches longer than the cap are not guaranteed to
//! be found, and `.` does not match `\n` (regex::bytes default), so ordinary
//! patterns are line-scoped naturally. Matching is over raw bytes, not decoded text --
//! `unicode(false)` on the builder, `SearchPattern::compile`'s own doc comment (batch 3
//! (2026-07-23), finding #8's ruling) -- see docs/search.md for the full model.
//!
//! `^`/`$` are LINE anchors (`multi_line(true)`, below), matching after/before any `\n` as well
//! as the true start/end of the file -- not haystack anchors: every windowed/chunked caller in
//! this module (`SweepAnalysis` here, `SearchForward`/`SearchBackward` in `scan.rs`) hands the
//! regex engine a SLICE, and a slice's own edge is unconditionally `^`/`$`-eligible regardless of
//! `multi_line` (confirmed empirically: `Regex::new("^foo").find_at(b"xxxfoo", 3)` is `None` --
//! `^` needs position 0 of the TEXT itself, not the search's own `start` offset -- but
//! `Regex::new("^foo").find_at(b"foo", 0)` matches, even though `b"foo"` is itself a truncated
//! tail of some larger real content). Every windowed caller therefore carries a real look-behind
//! byte alongside its sliding window (see each one's own doc comment for the exact mechanism) so
//! a slice's edge is never mistaken for a real line boundary.

/// bytes of payload each matching window advances by; reads underneath stay
/// block-granular, this only shapes how the regex sees the stream. `SweepAnalysis::step`
/// (task 5) reads exactly one window per bounded unit of background work.
pub(crate) const SEARCH_WINDOW: usize = 64 << 10;
/// window overlap: a match is found across a window seam iff it is shorter
/// than this; the honest documented cap on match length.
pub(crate) const MAX_MATCH_LEN: usize = 4 << 10;

pub struct SearchPattern {
    pub raw: String,
    pub backward: bool,
    regex: regex::bytes::Regex,
}
impl SearchPattern {
    /// smartcase: case-insensitive unless the typed pattern contains an
    /// uppercase character (vim's rule, applied to the raw text -- an
    /// uppercase inside an escape still counts; documented simplification).
    pub fn compile(raw: &str, backward: bool) -> Result<SearchPattern, regex::Error> {
        let sensitive = raw.chars().any(|c| c.is_uppercase());
        // multi_line: ^/$ are LINE anchors (after/before any \n, or the true file start/end),
        // not merely "start/end of whatever slice this call happens to be handed" -- see this
        // module's own doc comment for why every caller must still carry real look-behind/
        // lookahead context of its own around a chunk boundary regardless.
        //
        // unicode(false) (batch 3 (2026-07-23), finding #8, controller ruling): probe-verified,
        // `regex::bytes`'s own DEFAULT (unicode) mode treats the haystack as decoded text and
        // silently skips an invalid UTF-8 byte as if it were one unseen codepoint -- `.` over
        // b"a\xffb" finds only 2 matches, not 3 -- contradicting docs/search.md's own "raw file
        // bytes -- no encoding assumptions" claim this engine otherwise holds everywhere else
        // (block reads, line layout). `unicode(false)` is the honest fix: `.` matches exactly
        // one byte, always, except `\n`; `case_insensitive` folds ASCII letters only, so a
        // non-ASCII case pair (e.g. é/É) is now case-SENSITIVE even though smartcase would
        // otherwise have folded it (see docs/search.md's own matching-model paragraph). A
        // pattern may opt back into full Unicode-aware matching -- codepoint `.`, `\p{..}`
        // classes, Unicode-aware folding -- per use, via the inline `(?u)` flag; without it, a
        // `\p{..}` class fails to COMPILE outright (a bare `\w`/`\d`/`\s` Perl class stays fine,
        // just narrowed to its ASCII definition) -- surfacing through the same "bad pattern"
        // notice path any other syntax error already takes, not a silent behavior change.
        let regex = regex::bytes::RegexBuilder::new(raw)
            .case_insensitive(!sensitive)
            .multi_line(true)
            .unicode(false)
            .build()?;
        Ok(SearchPattern {
            raw: raw.to_string(),
            backward,
            regex,
        })
    }
    /// first match whose START lies in `accept` (indices into `hay`);
    /// returns (start, end). The accept range is the exactly-once rule:
    /// windows overlap by MAX_MATCH_LEN, and only the window whose
    /// non-overlap span contains the start reports it. A caller that stops
    /// at its first match (never searches this hay again) may instead pass
    /// the whole hay as `accept`; a caller that enumerates every match must
    /// still pass fresh-starts-only or it double-counts a seam match.
    pub(crate) fn find_starting_in(
        &self,
        hay: &[u8],
        accept: std::ops::Range<usize>,
    ) -> Option<(usize, usize)> {
        let mut at = accept.start;
        while at < accept.end {
            let m = self.regex.find_at(hay, at)?;
            if m.start() >= accept.end {
                return None;
            }
            if m.start() >= accept.start {
                return Some((m.start(), m.end()));
            }
            // find_at can return a match starting before `at` only when
            // anchored oddly; step past defensively.
            at = m.start().max(at) + 1;
        }
        None
    }
    /// last match whose start lies in `accept` -- iterate forward, keep the last.
    pub(crate) fn rfind_starting_in(
        &self,
        hay: &[u8],
        accept: std::ops::Range<usize>,
    ) -> Option<(usize, usize)> {
        let mut best = None;
        let mut at = accept.start;
        while let Some(m) = self.regex.find_at(hay, at) {
            if m.start() >= accept.end {
                break;
            }
            if m.start() >= accept.start {
                best = Some((m.start(), m.end()));
            }
            at = m.start() + 1;
        }
        best
    }
    /// all matches intersecting `hay` (start anywhere) -- the viewport
    /// highlighter's helper; bounded by the row slice it is handed.
    pub fn find_all(&self, hay: &[u8]) -> Vec<(usize, usize)> {
        self.regex
            .find_iter(hay)
            .map(|m| (m.start(), m.end()))
            .collect()
    }
}

/// A bounded, generation-stamped summary of one background match-summary sweep
/// (`SweepAnalysis`) over a whole file: how many matches were found, roughly where (a
/// fixed-size histogram over the file's byte range -- scrollbar tick annotations, a later
/// task), how far the sweep has gotten, and whether it is done or gave up on part of the file.
///
/// `generation` is the discriminator a caller needs: `Document::start_search_sweep` mints a
/// fresh one per request (never 0 -- see `empty`'s own doc comment), so a snapshot left over
/// from a SUPERSEDED sweep is never mistaken for the current one even though every request
/// shares the same long-lived `watch` channel (`crate::analyzer`'s driver publishes through one
/// channel for the whole life of the `Document`, not a fresh one per request).
///
/// `buckets[i]` counts matches whose start falls in the i-th of 1024 equal-width byte ranges
/// spanning the file -- `u16`, saturating: a bucket that would overflow just reports the cap
/// rather than wrapping or panicking, since this is a coarse density hint, not an exact count.
/// `matches` IS the exact total (also saturating, at `u64`, for the same reason: an honest
/// floor rather than a wrapped-around lie if a file somehow had more matches than fit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSummary {
    pub generation: u64,
    pub matches: u64,
    pub buckets: std::sync::Arc<[u16; 1024]>,
    pub scanned_up_to: u64,
    pub done: bool,
    pub failed: bool,
}
impl SearchSummary {
    /// The zero-value summary: generation 0 (never a real sweep's own -- `Document::
    /// start_search_sweep`'s `fetch_add(1) + 1` starts real generations at 1, so a caller can
    /// always tell "no sweep has ever run yet" apart from "sweep 1 hasn't published anything
    /// new"), no matches, an all-zero histogram, nothing scanned, not done, not failed. Seeds
    /// the `watch` channel `crate::analyzer::spawn` publishes through before any sweep has ever
    /// started, and is exactly what `SweepAnalysis::begin` overlays a fresh generation onto.
    pub fn empty() -> SearchSummary {
        SearchSummary {
            generation: 0,
            matches: 0,
            buckets: std::sync::Arc::new([0u16; 1024]),
            scanned_up_to: 0,
            done: false,
            failed: false,
        }
    }
}

/// The background `Analysis` (`crate::analyzer`) that enumerates every match of a pattern over
/// a whole file, producing a `SearchSummary`. One window of `SEARCH_WINDOW` fresh bytes per
/// `step` call -- a small, cheap bounded unit that keeps the driver's own supersession
/// `select!` responsive even on a huge file, exactly the contract `Analysis::step` documents.
///
/// GUARDRAIL (binding, carried over from the task 3 review, narrowed but NOT lifted by batch 3
/// (2026-07-23), finding #2 -- see `scan::SearchForward::step`'s own identical marker, and
/// `SearchPattern::find_starting_in`'s own doc comment: "windows overlap by MAX_MATCH_LEN, and
/// only the window whose non-overlap span contains the start reports it"): this sweep
/// ENUMERATES every match, so unlike a stop-at-first-match caller it must accept only FRESH
/// starts, never the whole hay -- a whole-hay accept (`SearchForward`'s own shape) would recount
/// a match once per window it still sits in. Finding #2 gave `SearchForward` a `safe_to` gate
/// that makes ITS OWN whole-hay accept report `find_all`'s own leftmost sub-cap match, not merely
/// the first one to complete -- a real improvement, but an ORTHOGONAL one: `safe_to` governs
/// WHICH of possibly several candidates a stop-at-first-match caller returns, not whether
/// re-offering the same bytes across MULTIPLE reports is safe, which is what this sweep's own
/// fresh-starts rule is about. `SearchForward` stays safe returning whole-hay because it still
/// returns at most once per scan, full stop -- this sweep calls its own equivalent, `step`,
/// repeatedly, over and over, for the SAME long-lived carry, so nothing about finding #2 relaxes
/// this GUARDRAIL: fresh-starts-only remains the enumerating requirement, unchanged.
///
/// `step` (one `SEARCH_WINDOW`-bounded unit) reads block by block and, after EVERY block,
/// reports (records into `matches`/`buckets`, then drops from `carry`) whatever prefix of the
/// window's own core is now SAFE: a position is safe once either this window's own read has
/// reached its final byte (`read_to`/EOF -- nothing more will ever arrive to change the answer)
/// or there are already `MAX_MATCH_LEN - 1` bytes of lookahead past it, the window overlap
/// `find_starting_in`'s own doc comment names -- never past the window's own `core_end` itself,
/// which belongs to the NEXT window's own report. `carry` thus always holds exactly the
/// not-yet-safe tail, which -- once a position's own window finally reports it -- becomes the
/// NEXT window's own leading bytes (no re-read): a match starting anywhere is searched, and
/// reportable, in EXACTLY the window whose core contains its start, never zero and never two.
///
/// Reporting INCREMENTALLY (after every block within a window), not deferred to the window's own
/// end, is what makes `give_up`'s floor honest: `sweep_gives_up_and_floors_on_a_failing_block`
/// (`document.rs`) pins that an earlier block's own already-found matches survive a LATER
/// block's failure in the same window -- a batch design that only ever recorded matches once a
/// whole window's worth of reading had succeeded would instead discard that earlier progress
/// the moment ANY later block in the same window failed, exactly backwards from floor semantics
/// (this struct's own `give_up`). A match longer than `MAX_MATCH_LEN` may still straddle a
/// window seam un-found -- the documented cap this sweep shares with
/// `SearchForward`/`SearchBackward`, not a new one.
///
/// ANCHOR CORRECTNESS (`^`/`$`, `multi_line` -- `SearchPattern::compile`'s own doc comment):
/// `$` is correct below the cap with no further mechanism -- a candidate match's own start is
/// only ever accepted below `safe_to`, and a SUB-cap match (length `< MAX_MATCH_LEN`) starting
/// there ends strictly before `read_pos` (`step`'s own local, the absolute position already
/// read), so the byte immediately after it is always already in `carry`, letting `$` see real
/// content, never an artificial end. `^` needs an explicit mechanism, `has_ctx`/`carry[0]`
/// below: WITHOUT it, a position only ever tested as `carry`'s own hay-index 0 (freshly re-based
/// after every report's `drain`, per this struct's own history) would unconditionally satisfy
/// `^` regardless of the REAL byte before it, since a slice's own edge is always `^`-eligible
/// (this module's own doc comment) -- reachable at EVERY report boundary, not just a window
/// seam, and proven by a regression test in this module (`sweep_analysis_does_not_fake_a_line_
/// start_at_a_report_boundary`) before this existed. The fix keeps exactly one REAL look-behind
/// byte permanently at `carry[0]` (`carry`'s own doc comment) so hay-index 0 is only ever that
/// context byte -- itself never an acceptable match start (`step`'s own accept range excludes
/// it) -- and the real earliest candidate sits at hay-index 1, correctly evaluated against real
/// content on every single report, not just the window's own first one.
pub(crate) struct SweepAnalysis {
    cache: std::sync::Arc<crate::cache::BlockCache>,
    pattern: Option<std::sync::Arc<SearchPattern>>,
    generation: u64,
    /// The absolute position up to which every match has been found and recorded, for good --
    /// never revisited. Advances INCREMENTALLY, not just at window boundaries: `step` moves it
    /// forward after every block it reads, as soon as enough lookahead makes a prefix of the
    /// current window's core safe to report (see `step`'s own doc comment on `SweepAnalysis`).
    /// Between calls it always sits at either a window boundary (a call ran to completion) or
    /// wherever the last successful report inside a call left it (a call that then failed --
    /// the next call resumes searching from here, never re-reporting what is already counted).
    pos: u64,
    /// Bytes read at or past `pos` but not yet safely reportable -- either genuinely fresh (the
    /// current window's own not-yet-confirmed tail) or, right after `pos` itself, physically the
    /// PREVIOUS window's own lookahead read (windows are contiguous, so nothing is ever re-read
    /// from the source for it) -- PLUS, once `has_ctx` is true, exactly one more byte at index 0
    /// that is NEITHER of those: either the real file byte at `pos - 1`, or (only right after
    /// `give_up`) a conservative non-`\n` sentinel standing in for an UNKNOWABLE one -- either
    /// way, kept permanently as `^`'s own look-behind context (this struct's own ANCHOR
    /// CORRECTNESS doc comment) and never itself an acceptable match start. Empty only at the
    /// very start of a sweep (before `pos` has ever advanced past 0).
    carry: Vec<u8>,
    /// Whether `carry[0]` currently holds a context byte (real or sentinel, `carry`'s own doc
    /// comment) rather than the first byte of not-yet-reported content. Becomes true the first
    /// time `step` ever reports past `pos` 0 (true file start needs no look-behind of its own --
    /// it is unconditionally a valid line start, `multi_line` or not) and STAYS true for the
    /// rest of the sweep's life, including across `give_up`: `give_up` cannot know the real
    /// byte at the new `pos - 1` (the window it skips is never read) but must NOT fall back to
    /// `has_ctx = false` either -- that would silently OPEN the exact "hay's own edge is
    /// unconditionally ^-eligible" hole this whole mechanism exists to close (a false MATCH, not
    /// the honest miss an earlier version of this comment claimed -- a real regression, caught
    /// before landing; `give_up`'s own comment). A sentinel keeps this true unconditionally
    /// after the very first advance past `pos` 0, for the sweep's entire remaining life.
    has_ctx: bool,
    matches: u64,
    buckets: [u16; 1024],
    failed: bool,
    /// The block index the most recent failing read (`step`, which records it right where the
    /// read happens) actually attempted -- `None` until the first failure of a sweep. `give_up`
    /// consumes it (batch 3 (2026-07-23), finding #5b) to skip past the whole FAILING BLOCK,
    /// not merely one `SEARCH_WINDOW` still inside it; see `give_up`'s own comment for why a
    /// block bigger than a window otherwise costs one give_up, and a full retry round, per
    /// window still inside it.
    last_failed_block: Option<u64>,
}
impl SweepAnalysis {
    pub(crate) fn new(cache: std::sync::Arc<crate::cache::BlockCache>) -> SweepAnalysis {
        SweepAnalysis {
            cache,
            pattern: None,
            generation: 0,
            pos: 0,
            carry: Vec::new(),
            has_ctx: false,
            matches: 0,
            buckets: [0u16; 1024],
            failed: false,
            last_failed_block: None,
        }
    }
    fn snapshot(&self, done: bool) -> SearchSummary {
        SearchSummary {
            generation: self.generation,
            matches: self.matches,
            buckets: std::sync::Arc::new(self.buckets),
            scanned_up_to: self.pos,
            done,
            failed: self.failed,
        }
    }
    /// Buckets `match_at`'s start position over `[0, size)` into one of 1024 equal-width bins.
    /// The multiply is done in `u128`, matching `percent_offset`'s own precedent in
    /// `document.rs` (`CONVENTIONS.md`'s own ratio-arithmetic rule): a huge sparse file's own
    /// byte offset times 1024 must not overflow a `u64` multiply. `match_at < size` always
    /// holds here (a match can only start inside hay this sweep actually read, itself bounded
    /// by `size`), so the computed index is always `< 1024` on its own; `.min(len - 1)` is a
    /// defensive backstop, not a reachable clamp.
    fn record_match(&mut self, match_at: u64, size: u64) {
        self.matches = self.matches.saturating_add(1);
        let bucket =
            ((match_at as u128 * self.buckets.len() as u128) / (size.max(1) as u128)) as usize;
        let bucket = bucket.min(self.buckets.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}
impl crate::analyzer::Analysis for SweepAnalysis {
    type Request = (u64, std::sync::Arc<SearchPattern>);
    type Snapshot = SearchSummary;
    fn begin(&mut self, req: &Self::Request) -> SearchSummary {
        let (generation, pattern) = req.clone();
        self.generation = generation;
        self.pattern = Some(pattern);
        self.pos = 0;
        self.carry.clear();
        self.has_ctx = false;
        self.matches = 0;
        self.buckets = [0u16; 1024];
        self.failed = false;
        self.last_failed_block = None;
        self.snapshot(false)
    }
    async fn step(
        &mut self,
        tx: &tokio::sync::watch::Sender<SearchSummary>,
    ) -> anyhow::Result<bool> {
        let pattern = self
            .pattern
            .clone()
            .expect("step called before begin -- the driver never does this");
        let size = self.cache.size();
        self.pos = self.pos.min(size);
        let bs = self.cache.block_size() as u64;
        let core_end = self.pos.saturating_add(SEARCH_WINDOW as u64).min(size);
        // this window's own search needs up to MAX_MATCH_LEN-1 bytes of LOOKAHEAD past
        // `core_end` before its own tail can be definitively resolved (this struct's own
        // GUARDRAIL); `read_to` is the precise minimum sufficient bound (not a generous
        // MAX_MATCH_LEN's worth): pinned by `a_maximal_length_match_at_the_last_core_byte_is_
        // found_whole` (this module's own tests) for the identical windowed model.
        let read_to = core_end.saturating_add(MAX_MATCH_LEN as u64 - 1).min(size);
        // `self.carry` already holds this window's own leading bytes -- either the PREVIOUS
        // window's own lookahead read, physically contiguous with `self.pos` (windows never
        // skip a byte, except right after `give_up`, which clears `carry` for exactly this
        // reason), or, on a RETRY after THIS SAME window failed partway through, whatever it
        // already read (and already reported -- see below) before the failing block -- PLUS,
        // when `has_ctx`, the one permanent look-behind byte at index 0 (this struct's own
        // ANCHOR CORRECTNESS doc comment), which is not itself unread/unreported content and so
        // must not be counted as such here.
        let already_read = self.carry.len() - self.has_ctx as usize;
        let mut read_pos = self.pos.saturating_add(already_read as u64);
        while read_pos < read_to {
            let block_idx = read_pos / bs;
            let block = match self.cache.warm(block_idx).await {
                Ok(block) => block,
                Err(e) => {
                    // batch 3 (2026-07-23), finding #5b: record which block this read actually
                    // targeted, not just that SOME read in the current window failed -- `give_
                    // up`, if retries exhaust, needs the failing block's own index to skip past
                    // the whole thing rather than one window at a time (see `give_up`'s own
                    // comment). Every failing attempt (including retries) overwrites this with
                    // its own block index, so it always reflects the most recent failure.
                    self.last_failed_block = Some(block_idx);
                    return Err(e);
                }
            };
            let lo = ((read_pos % bs) as usize).min(block.len());
            let take = (block.len() - lo).min((read_to - read_pos) as usize);
            if take == 0 {
                break; // a short block at EOF; nothing more to read.
            }
            self.carry.extend_from_slice(&block[lo..lo + take]);
            read_pos += take as u64;
            // report whatever is now SAFELY resolved -- incrementally, right after every
            // block, not deferred to the end of the whole window: a LATER block in the SAME
            // window failing (`give_up`) must not cost the matches an EARLIER, already-read
            // block already found (`sweep_gives_up_and_floors_on_a_failing_block`, document.
            // rs's own test, pins exactly this). "Safe" means either this window's own read
            // has reached its final byte (`read_to`/EOF -- nothing more will ever arrive to
            // change the answer) or there are already MAX_MATCH_LEN-1 bytes of lookahead past
            // it (this struct's own GUARDRAIL); either way, never past this window's own core
            // (`core_end`) -- bytes beyond it belong to the NEXT window's own report.
            let at_final = read_pos >= read_to || read_pos >= size;
            let safe_to = if at_final {
                read_pos
            } else {
                read_pos.saturating_sub(MAX_MATCH_LEN as u64 - 1)
            };
            let report_to = safe_to.min(core_end);
            if report_to > self.pos {
                // `lb` ("look-behind"): 1 while `carry[0]` is the permanent context byte, else
                // 0 -- recomputed every report, not hoisted above the loop, since the very
                // first report of a sweep (`pos` still 0) is the one point `has_ctx` flips
                // false -> true mid-`step`, and a later report in this SAME call must see that.
                let lb = self.has_ctx as usize;
                let accept_end = lb + ((report_to - self.pos) as usize).min(self.carry.len() - lb);
                // ZERO-WIDTH AT THE TRUE EOF (batch 3 (2026-07-23), finding #9): `accept_end`
                // above excludes a start == carry's own end everywhere, but when THIS report
                // reaches the file's own true end (`report_to == size`, only ever true for the
                // very last report of the very last window: `report_to <= core_end <= size`
                // always, so equality forces `core_end == size` too) position `size` is a
                // legitimate zero-width match start (`$` unconditionally holds at the true
                // haystack end). Widening by one position only ever admits a zero-width match --
                // nothing else could start at carry's own last index and still complete, since
                // there is no more hay past it for a real match to consume.
                let accept_end = if report_to >= size {
                    accept_end + 1
                } else {
                    accept_end
                };
                // matches are collected first and recorded after: the scan below borrows
                // `self.carry` immutably for its whole duration, and `record_match` needs
                // `&mut self` -- the two cannot overlap. `at` starts at `lb`, never 0, so the
                // context byte itself (index 0, when `lb == 1`) is visible to the regex for `^`
                // look-behind but can never itself be RETURNED as a match's own start (this
                // struct's own ANCHOR CORRECTNESS doc comment).
                let mut found = Vec::new();
                let mut at = lb;
                while let Some((s, _e)) = pattern.find_starting_in(&self.carry, at..accept_end) {
                    found.push(self.pos + (s - lb) as u64);
                    at = s + 1;
                }
                for match_at in found {
                    self.record_match(match_at, size);
                }
                // drop the reported prefix, EXCEPT its own last byte: that byte becomes the
                // NEW permanent context (the real predecessor of the new `pos`), replacing
                // whatever `carry[0]` held before -- `accept_end - 1` is always a valid index
                // (accept_end >= lb + 1 >= 1, since report_to > self.pos guarantees at least
                // one newly-safe byte). `has_ctx` is set every time, not just the first: cheap,
                // and correct regardless of which report first makes `pos` positive.
                self.carry.drain(..accept_end - 1);
                self.has_ctx = true;
                self.pos = report_to;
            }
        }
        let done = self.pos >= size;
        let _ = tx.send(self.snapshot(done));
        Ok(done)
    }
    fn progress_marker(&self) -> u64 {
        self.pos
    }
    fn give_up(&mut self, tx: &tokio::sync::watch::Sender<SearchSummary>) {
        self.failed = true;
        // advance past the failing BLOCK, not merely one SEARCH_WINDOW into it (batch 3
        // (2026-07-23), finding #5b) -- floor semantics (this struct's own doc comment): a
        // caller sees an honest partial count instead of the driver looping on a block that
        // will never heal. A block bigger than a window (the default 1 MiB block over the 64
        // KiB SEARCH_WINDOW is 16x) used to cost one give_up -- and its own full
        // ANALYSIS_RETRIES-deep retry round -- PER WINDOW still inside it, up to 16 for a
        // single permanently dead block; skipping straight to the block boundary AFTER the one
        // `last_failed_block` names (`step`'s own comment: recorded right where the failing
        // read happened, always the most recent one by the time retries exhaust) collapses that
        // to exactly one. No special-casing for WHERE in the window the failure lands: a block
        // that fails purely in the trailing lookahead margin (past this window's own core_end,
        // i.e. after the core's own matches are already permanently recorded -- `step`'s own
        // incremental reporting caps `report_to` at `core_end`, so `pos` is already there)
        // still costs the identical bounded retry sequence (`ANALYSIS_RETRIES`, `crate::
        // analyzer`) before this runs, resuming each retry from wherever `carry` already left
        // off rather than re-reading the core -- and `failed` ends up set the same way either
        // way. `.max(...)` keeps the ORIGINAL floor guarantee alongside the new one: a block
        // SMALLER than a window (many small blocks, one of them permanently dead) must still
        // advance by a full window, exactly as before this fix --
        // `give_up_does_not_fake_a_line_start_right_after_the_skipped_window` (this module's
        // own test) pins that unchanged floor with a 4 KiB block over the real SEARCH_WINDOW.
        // `last_failed_block` defaults to "no further than one window" (`unwrap_or(0)`) on the
        // defensive, shouldn't-happen path where `give_up` runs without a prior recorded
        // failure -- the driver never does this (it only calls `give_up` once `step` has
        // already returned `Err` `ANALYSIS_RETRIES + 1` times in a row), so this is a floor,
        // never a value this sweep expects to actually use.
        let bs = self.cache.block_size() as u64;
        let past_failing_block = self
            .last_failed_block
            .map(|idx| idx.saturating_add(1).saturating_mul(bs))
            .unwrap_or(0);
        let past_one_window = self.pos.saturating_add(SEARCH_WINDOW as u64);
        self.pos = past_failing_block
            .max(past_one_window)
            .min(self.cache.size());
        self.last_failed_block = None;
        // whatever `carry` held (this window's own partial core-plus-lookahead read, plus any
        // context byte) is no longer contiguous with the NEW `pos` once the skip above lands
        // past it -- keeping it would let the NEXT window's own search see stale bytes
        // separated from `pos` by the skipped gap as if they were its own leading bytes, a
        // false match spanning territory this sweep never actually read as contiguous.
        //
        // The new `pos`'s own real predecessor is unknowable without a read this method
        // deliberately avoids -- but `has_ctx = false` here would NOT leave that predecessor
        // merely undetermined: it would set `lb == 0` on the next `step`, re-opening the exact
        // "hay's own edge is unconditionally ^-eligible" hole this whole mechanism exists to
        // close, only now producing a false MATCH instead of the honest miss this comment
        // used to claim (a real regression, caught before landing -- the same class as
        // `SearchBackward::step`'s own floor case, its own doc comment). An explicit non-`\n`
        // sentinel keeps `has_ctx` true and `^` conservative instead.
        self.carry.clear();
        self.carry.push(0u8); // never `\n` (0x0A); see this block's own comment just above
        self.has_ctx = true;
        let done = self.pos >= self.cache.size();
        let _ = tx.send(self.snapshot(done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::Strategy;

    #[test]
    fn smartcase_lowercase_is_insensitive() {
        let p = SearchPattern::compile("abc", false).unwrap();
        assert_eq!(p.find_all(b"ABC"), vec![(0, 3)]);
    }

    #[test]
    fn smartcase_any_uppercase_is_sensitive() {
        let p = SearchPattern::compile("aBc", false).unwrap();
        assert!(p.find_all(b"abc").is_empty());
        assert_eq!(p.find_all(b"aBc"), vec![(0, 3)]);
    }

    #[test]
    fn smartcase_detects_uppercase_even_in_a_non_ascii_letter() {
        // reviewer-requested closer, batch 3 (2026-07-23), finding #8: the OTHER half of
        // smartcase's own interaction with the unicode(false) ruling. Sensitivity DETECTION
        // (`raw.chars().any(char::is_uppercase)`, `SearchPattern::compile`) is plain Rust `char`
        // classification, fully Unicode-aware and entirely independent of the regex builder's
        // own `unicode(false)` -- "É" (U+00C9, probe-confirmed `char::is_uppercase() == true`)
        // sets `sensitive = true` exactly like an ASCII uppercase letter would. Folding, if the
        // pattern HAD stayed insensitive, is the ASCII-only half (`smartcase_folding_is_ascii_
        // only_non_ascii_case_pairs_stay_sensitive`, this module's own sibling test) -- two
        // different mechanisms, and `unicode(false)` only ever touches one of them.
        let p = SearchPattern::compile("É", false).unwrap();
        assert!(
            p.find_all("é".as_bytes()).is_empty(),
            "sensitive: the lowercase pair must not match"
        );
        assert_eq!(p.find_all("É".as_bytes()), vec![(0, 2)]);
    }

    #[test]
    fn compile_error_surfaces() {
        assert!(SearchPattern::compile("(", false).is_err());
    }

    // ---- the raw-byte matching ruling: unicode(false) (batch 3 (2026-07-23), finding #8) ----

    #[test]
    fn dot_matches_any_raw_byte_except_newline_not_a_decoded_codepoint() {
        // probe-verified: `regex::bytes`'s own DEFAULT (unicode-mode) `.` treats the haystack as
        // decoded text and skips an invalid UTF-8 byte as if it were one unseen codepoint --
        // `Regex::new(".").find_iter(b"a\xffb")` finds only 2 matches, silently swallowing byte
        // 1 -- contradicting docs/search.md's own "raw file bytes -- no encoding assumptions"
        // claim. `.unicode(false)` (`SearchPattern::compile`) is the honest fix: `.` matches
        // exactly one byte, always, except `\n` -- three matches here, one per byte, invalid or
        // not.
        let p = SearchPattern::compile(".", false).unwrap();
        assert_eq!(p.find_all(b"a\xffb"), vec![(0, 1), (1, 2), (2, 3)]);
        assert!(
            p.find_all(b"\n").is_empty(),
            "unicode(false) does not change the `.`-never-matches-\\n default"
        );
    }

    #[test]
    fn unicode_opt_in_restores_codepoint_dot_semantics() {
        // the escape hatch: `(?u)` inline, on a per-pattern basis, re-enables Unicode-aware
        // matching for that pattern alone -- probe-confirmed to compile under `unicode(false)`
        // and behave exactly like the pre-ruling default, skipping the invalid byte as one
        // codepoint again (2 matches, not 3).
        let p = SearchPattern::compile("(?u).", false).unwrap();
        assert_eq!(p.find_all(b"a\xffb"), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn smartcase_folding_is_ascii_only_non_ascii_case_pairs_stay_sensitive() {
        // case-insensitive folding is now ASCII-only: smartcase still says "no uppercase in the
        // typed pattern, so fold" (the pattern here, "é", has no ASCII-uppercase character), but
        // folding itself only ever touches a-z/A-Z under `unicode(false)` -- a non-ASCII case
        // pair like é/É (U+00E9/U+00C9, each 2 UTF-8 bytes) is NOT folded, so it stays
        // case-sensitive even though smartcase would otherwise have made the pattern
        // insensitive. Documented in docs/search.md's own matching-model paragraph.
        let p = SearchPattern::compile("é", false).unwrap();
        assert!(p.find_all("É".as_bytes()).is_empty());
        assert_eq!(p.find_all("é".as_bytes()), vec![(0, 2)]);
    }

    #[test]
    fn unicode_class_without_opt_in_fails_to_compile_with_opt_in_it_works() {
        // CAUTION (finding #8's own): `unicode(false)` makes some patterns error outright --
        // `\p{..}` Unicode class syntax needs Unicode mode, so it errors without an explicit
        // `(?u)` opt-in (probe-verified: "Unicode not allowed here") -- surfacing through the
        // exact same "bad pattern" notice path any other syntax error already takes
        // (`ress/src/app.rs`'s own `CommitSearch` handling), never a silent misbehavior. A bare
        // `\w`/`\d`/`\s` (Perl class, not `\p{..}`) stays fine either way, just narrowed to its
        // ASCII definition -- this pins the `\p{..}` case specifically, the one that can fail.
        assert!(SearchPattern::compile(r"\p{L}", false).is_err());
        assert!(SearchPattern::compile(r"(?u)\p{L}", false).is_ok());
    }

    #[test]
    fn find_starting_in_respects_the_accept_range() {
        let p = SearchPattern::compile("x", false).unwrap();
        let hay = b"x123456789x"; // literal 'x' matches at start 0 and start 10
        assert_eq!(p.find_starting_in(hay, 5..15), Some((10, 11)));
    }

    #[test]
    fn rfind_returns_the_last_accepted_start() {
        let p = SearchPattern::compile("x", false).unwrap();
        let mut hay = vec![b'.'; 21];
        hay[2] = b'x';
        hay[8] = b'x';
        hay[20] = b'x';
        assert_eq!(p.rfind_starting_in(&hay, 0..10), Some((8, 9)));
    }

    /// The windowed model itself, parameterized over (window, overlap) so
    /// both the proptest oracle below and the deterministic seam sweep run
    /// the identical scan shape production will use (task 2+): slide a
    /// `window`-byte core forward, reading `overlap` extra trailing bytes
    /// so a match starting in the core but extending past it is still
    /// wholly visible as long as its length is <= overlap (the documented
    /// cap); each absolute start position falls in exactly one core, so
    /// `find_starting_in`'s accept range is the sole de-duplication device
    /// -- no match is ever attributed to two windows or to none.
    ///
    /// Also carries a real look-behind byte across the window seam (`ctx`), mirroring
    /// `SweepAnalysis::step`'s own permanent context byte (its ANCHOR CORRECTNESS doc
    /// comment): without it, every window after the first would search a slice whose own
    /// index 0 is an artificial edge, unconditionally `^`-eligible regardless of the real
    /// preceding byte (`SearchPattern::compile`'s own module-level doc comment) -- a no-op
    /// for the non-anchored patterns this model was first written for, but load-bearing for
    /// `windowed_scan_equals_whole_buffer_scan_anchored`, below.
    fn windowed_scan(
        p: &SearchPattern,
        hay: &[u8],
        window: usize,
        overlap: usize,
    ) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut start = 0usize;
        let mut ctx: Option<u8> = None;
        while start < hay.len().max(1) {
            let end = (start + window).min(hay.len());
            let ext = (end + overlap).min(hay.len());
            let core = &hay[start..ext];
            let lb = ctx.is_some() as usize;
            let mut slice = Vec::with_capacity(lb + core.len());
            slice.extend(ctx);
            slice.extend_from_slice(core);
            // the core, in slice-relative coordinates, is [lb, lb + end - start); that's
            // the whole (shifted) slice length whenever hay is non-empty (end > start
            // always holds there, since window >= 1), and [lb, lb) on the one-shot
            // empty-hay pass -- find_starting_in on an empty accept range correctly
            // reports no match either way, so no special-casing is needed here.
            let accept_end = lb + (end - start);
            let mut at = lb;
            while let Some((s, e)) = p.find_starting_in(&slice, at..accept_end) {
                if e - s <= overlap {
                    found.push((start + (s - lb), start + (e - lb)));
                }
                at = s + 1;
            }
            if end == hay.len() {
                break;
            }
            // the next window's own look-behind byte is the real last byte of THIS
            // window's own core (`end - 1`) -- always in range since `end > start`.
            ctx = Some(hay[end - 1]);
            start = end;
        }
        found
    }

    // the windowed model's oracle: sliding accept-windows over any chunking
    // must find exactly the matches a whole-buffer pass finds, for matches
    // shorter than the overlap. This mirrors the pending≡sync nav oracles.
    //
    // The haystack is built from chunks that are mostly one arbitrary byte,
    // occasionally a whole spliced-in "a" + `n` "b"s + "c" token (`n` up to
    // 80, so ~1 in 5 tokens is itself longer than `overlap` and must be
    // filtered out of both sides, exercising that path too). A flat
    // per-byte a/b/c bias was tried first and measured: raw bytes alone
    // spell "ab+c" in ~0.16% of cases, so a naive per-byte mix has to push
    // p(a)/p(b)/p(c) an order of magnitude above baseline to stop being
    // vacuous -- but that also raises the *raw* 'a' rate the regex engine
    // has to chase every window through, which measured ~30x slower in the
    // debug profile (23s/1024 cases vs. 0.75s) for no better coverage than
    // this token-splice version (still ~97% of cases containing a match, at
    // roughly the original per-byte 'a' rate since only the deliberately
    // spliced tokens contribute real 'a's).
    proptest::proptest! {
        #[test]
        fn windowed_scan_equals_whole_buffer_scan(
            hay in proptest::collection::vec(
                proptest::prelude::prop_oneof![
                    30 => proptest::prelude::any::<u8>().prop_map(|b| vec![b]),
                    1 => (1usize..80).prop_map(|n| {
                        let mut v = vec![b'a'];
                        v.resize(v.len() + n, b'b');
                        v.push(b'c');
                        v
                    }),
                ],
                0..1200,
            ).prop_map(|chunks| chunks.into_iter().flatten().collect::<Vec<u8>>()),
            window in 8usize..512,
        ) {
            let p = SearchPattern::compile("ab+c", false).unwrap();
            let overlap = 64usize; // stand-in MAX_MATCH_LEN for the oracle; "ab+c" can exceed it only via b-runs > 62, excluded below
            let naive: Vec<_> = p.find_all(&hay).into_iter()
                .filter(|(s, e)| e - s <= overlap).collect();
            let windowed = windowed_scan(&p, &hay, window, overlap);
            proptest::prop_assert_eq!(naive, windowed);
        }
    }

    // The anchored counterpart of the oracle above: `^`/`$` are LINE anchors
    // (`SearchPattern::compile`'s own doc comment), so the oracle needs real line structure --
    // a haystack built from LINES of small tokens (mostly "tok", the search target, so hits are
    // dense; occasionally other lowercase filler, joined by spaces), `\n`-terminated. Compares
    // the windowed model (now anchor-aware -- `windowed_scan`'s own doc comment) against a
    // WHOLE-BUFFER `find_all` for `^tok` and `tok$`, restricted to sub-cap matches as above.
    proptest::proptest! {
        #[test]
        fn windowed_scan_equals_whole_buffer_scan_anchored(
            lines in proptest::collection::vec(
                proptest::collection::vec(
                    proptest::prelude::prop_oneof![
                        3 => proptest::strategy::Just("tok".to_string()),
                        2 => "[a-z]{1,6}",
                    ],
                    1..6,
                ),
                0..60,
            ),
            window in 8usize..512,
            anchor_end in proptest::prelude::any::<bool>(),
        ) {
            let mut hay = Vec::new();
            for line in &lines {
                hay.extend_from_slice(line.join(" ").as_bytes());
                hay.push(b'\n');
            }
            let pattern = if anchor_end { "tok$" } else { "^tok" };
            let p = SearchPattern::compile(pattern, false).unwrap();
            let overlap = 64usize; // stand-in MAX_MATCH_LEN, as above; "tok"/"^tok"/"tok$" never exceed it
            let naive: Vec<_> = p.find_all(&hay).into_iter()
                .filter(|(s, e)| e - s <= overlap).collect();
            let windowed = windowed_scan(&p, &hay, window, overlap);
            proptest::prop_assert_eq!(naive, windowed);
        }
    }

    /// Plants a single "needle" at every offset straddling the window/core
    /// seam and checks the windowed scan reports it exactly once, at the
    /// right place, for every plant position -- the proptest above covers
    /// arbitrary content but leans on random `window` draws to eventually
    /// land on a seam; this sweeps every seam offset deterministically.
    fn assert_seam_sweep_finds_every_plant(window: usize, overlap: usize) {
        let p = SearchPattern::compile("needle", false).unwrap();
        let base = vec![b'x'; 3 * window];
        for plant in (window - 8)..=(window + 8) {
            let mut hay = base.clone();
            hay[plant..plant + 6].copy_from_slice(b"needle");
            let found = windowed_scan(&p, &hay, window, overlap);
            assert_eq!(found, vec![(plant, plant + 6)], "plant offset {plant}");
        }
    }

    #[test]
    fn a_match_straddling_every_seam_offset_is_reported_exactly_once() {
        assert_seam_sweep_finds_every_plant(64, 16);
    }

    /// Same sweep, at the real production (SEARCH_WINDOW, MAX_MATCH_LEN)
    /// scale -- exercises the actual shipped constants, not just small
    /// stand-ins, and is what keeps them from being unused in this task.
    #[test]
    fn a_match_straddling_every_seam_offset_is_reported_exactly_once_at_production_scale() {
        assert_seam_sweep_finds_every_plant(SEARCH_WINDOW, MAX_MATCH_LEN);
    }

    /// The seam sweep above pins down the *core* boundary (a short, fixed
    /// "needle" -- which window's accept range owns a given start). It
    /// can't pin down the *overlap* boundary, because a 6-byte needle
    /// never comes close to needing all `overlap` bytes of lookahead. This
    /// does: a match of exactly `overlap` bytes, starting at the very last
    /// byte of a core, needs the slice to reach `core_end + overlap - 1` --
    /// found this gap by mutating `ext`'s `+ overlap` down to `+ overlap -
    /// 1` (harmless: the true bound only needs `overlap - 1`, so the
    /// shipped formula has exactly one byte of unused slack) and then `-
    /// 2` (a genuine miss, not caught by either the seam sweep or the
    /// proptest -- their `window`/content draws essentially never land on
    /// this exact worst-case alignment).
    #[test]
    fn a_maximal_length_match_at_the_last_core_byte_is_found_whole() {
        let window = 64usize;
        let overlap = 16usize;
        let p = SearchPattern::compile("ab+c", false).unwrap();
        let mut hay = vec![b'x'; 3 * window];
        let start = window - 1; // the last byte of the first core
        let mut needle = vec![b'a'];
        needle.resize(overlap - 1, b'b'); // 1 (a) + (overlap - 2) b's ...
        needle.push(b'c'); // ... + 1 (c) == overlap bytes total
        hay[start..start + overlap].copy_from_slice(&needle);
        let found = windowed_scan(&p, &hay, window, overlap);
        assert_eq!(found, vec![(start, start + overlap)]);
    }

    // ---- SweepAnalysis (task 5): the abstract windowed model above, proven against the real,
    // block-cache-backed implementation at the exact seams that matter -- the full-stack
    // sweep tests live in `document.rs`; these target `step`'s own read-ahead-then-search
    // shape directly (this struct's own GUARDRAIL doc comment) against the exact byte
    // boundaries a `Document`-level test cannot easily aim at. ----

    /// Drives a `SweepAnalysis` to completion against `data`, returning the final published
    /// `SearchSummary`.
    async fn run_sweep_to_completion(
        data: impl Into<bytes::Bytes>,
        block_size: usize,
        pattern: &str,
    ) -> SearchSummary {
        use crate::analyzer::Analysis;
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(crate::source::MockSource::new(data)),
            block_size,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile(pattern, false).unwrap());
        let (tx, rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        loop {
            if sweep.step(&tx).await.unwrap() {
                break;
            }
        }
        rx.borrow().clone()
    }

    #[tokio::test]
    async fn sweep_analysis_finds_a_match_straddling_a_window_seam_exactly_once() {
        // "needle" (6 bytes) straddles the boundary between the first window's own core and
        // its successor's -- exactly the seam a read-then-search-PER-BLOCK design (rejected;
        // see `SweepAnalysis`'s own GUARDRAIL doc comment) would miss entirely. A small,
        // non-block-aligned block size forces several `cache.warm` reads within the one window
        // that must find it.
        let plant = SEARCH_WINDOW - 3;
        let mut data = vec![b'x'; SEARCH_WINDOW * 2];
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 37, "needle").await;
        assert_eq!(
            summary.matches, 1,
            "the seam-straddling match must be found exactly once"
        );
        assert!(summary.done);
        assert!(!summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_finds_a_maximal_length_match_at_the_last_core_byte() {
        // the seam test above pins the *core* boundary; this pins the *lookahead* boundary -- a
        // match of exactly MAX_MATCH_LEN bytes starting at the very last byte of the first
        // window's own core, needing the full lookahead `step`'s own `read_to` reads before
        // ever searching -- the identical worst-case alignment
        // `a_maximal_length_match_at_the_last_core_byte_is_found_whole` (this module's own
        // windowed-model test, just above) already pins for the abstract model.
        let start = SEARCH_WINDOW - 1;
        let mut needle = vec![b'a'];
        needle.resize(MAX_MATCH_LEN - 1, b'b'); // 1 (a) + (MAX_MATCH_LEN - 2) b's ...
        needle.push(b'c'); // ... + 1 (c) == MAX_MATCH_LEN bytes total.
        let mut data = vec![b'x'; SEARCH_WINDOW * 2 + MAX_MATCH_LEN];
        data[start..start + MAX_MATCH_LEN].copy_from_slice(&needle);
        let summary = run_sweep_to_completion(data, 37, "ab+c").await;
        assert_eq!(
            summary.matches, 1,
            "a maximal-length match at the last core byte must be found whole"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_counts_a_line_anchored_needle_across_a_window_seam() {
        // "^needle" plants "needle" right after a real \n that itself sits one byte before the
        // window seam (so the \n is in window 0's own core, the match straddles into window
        // 1's) -- must be counted exactly once, proving `^` is evaluated against the real
        // preceding byte at the window boundary (this module's own ANCHOR CORRECTNESS doc
        // comment), not an artificial "start of carry" edge.
        let plant = SEARCH_WINDOW - 3;
        let mut data = vec![b'x'; SEARCH_WINDOW * 2];
        data[plant - 1] = b'\n';
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 37, "^needle").await;
        assert_eq!(
            summary.matches, 1,
            "the line-anchored match must be counted once"
        );
        assert!(summary.done);
        assert!(!summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_counts_a_line_anchored_needle_dollar_across_a_window_seam() {
        // "needle$" plants "needle" straddling the window seam, immediately followed by a
        // real \n -- must be counted exactly once (the mirror image of the ^ case above).
        let plant = SEARCH_WINDOW - 3;
        let mut data = vec![b'x'; SEARCH_WINDOW * 2];
        data[plant + 6] = b'\n';
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 37, "needle$").await;
        assert_eq!(
            summary.matches, 1,
            "the line-anchored match must be counted once"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_counts_a_zero_width_dollar_at_unterminated_eof() {
        // batch 3 (2026-07-23), finding #9 (probe-verified): "$" over unterminated "abc" has no
        // `\n` anywhere, so its only match is the zero-width position 3 (the file's own true
        // end) -- pre-fix the final window's own accept range excluded a start == carry's own
        // end, so the sweep counted 0 even though the highlighter (`find_all`) sees `(3, 3)` and
        // (post-#9) the nav scan finds it too -- the highlight-vs-count contradiction this
        // finding closes, zero-width edition.
        let summary = run_sweep_to_completion(b"abc".to_vec(), 8, "$").await;
        assert_eq!(summary.matches, 1);
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_fake_a_line_start_at_a_window_boundary() {
        // regression (found during this review): without a real look-behind byte, EVERY
        // window boundary's own carry re-basing makes hay-index 0 unconditionally
        // `^`-eligible (a slice's own edge always is -- this module's own doc comment),
        // regardless of the REAL preceding byte. "needle" sits exactly at window 1's own
        // start, with NO \n anywhere in the buffer -- `^needle` must not match it.
        let plant = SEARCH_WINDOW; // exactly window 1's own start
        let mut data = vec![b'x'; SEARCH_WINDOW * 2];
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 8192, "^needle").await;
        assert_eq!(summary.matches, 0, "no real newline precedes \"needle\"");
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_fake_a_line_start_at_a_report_boundary() {
        // the same regression as above, but at a MID-window incremental report boundary (this
        // struct's own `step` reports after EVERY block, not just at the window's own end) --
        // the tighter, more common case: a small block size forces many report+drain cycles
        // before "needle" (planted well inside window 0, no \n anywhere) is ever reached.
        let plant = 5000usize;
        let mut data = vec![b'x'; SEARCH_WINDOW];
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 1, "^needle").await;
        assert_eq!(summary.matches, 0, "no real newline precedes \"needle\"");
    }

    #[tokio::test]
    async fn give_up_does_not_fake_a_line_start_right_after_the_skipped_window() {
        // regression (found during this review, while fixing the two above): `give_up`'s FIRST
        // fix attempt reset `has_ctx` to false rather than seeding a conservative sentinel --
        // which does not merely leave the new `pos`'s own predecessor unknown, it re-opens the
        // same "hay's own edge is unconditionally ^-eligible" hole for the position right after
        // the skipped window, producing a false MATCH there (never the honest miss `give_up`'s
        // own doc comment claims). A source that fails every block after the first forces
        // `give_up` directly (mirrors document.rs's own `sweep_gives_up_and_floors_on_a_failing_
        // block` fixture); "needle" sits right at the start of the (skipped) second window, with
        // NO \n anywhere -- `^needle` must not match it.
        use crate::analyzer::Analysis;
        // window 0's own region always fails (forcing `give_up` on it); window 1's own region
        // reads fine, with "needle" planted right at its own start and NO \n anywhere.
        struct FailsOnlyWindow0;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyWindow0 {
            fn size(&self) -> u64 {
                (SEARCH_WINDOW * 3) as u64
            }
            async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<bytes::Bytes> {
                if offset < SEARCH_WINDOW as u64 {
                    Err(anyhow::anyhow!("boom"))
                } else {
                    let mut block = vec![b'x'; len];
                    if offset == SEARCH_WINDOW as u64 {
                        block[0..6].copy_from_slice(b"needle");
                    }
                    Ok(bytes::Bytes::from(block))
                }
            }
        }
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyWindow0),
            4096,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        // window 0's very first read fails outright, so `pos` is still 0 (nothing was ever
        // successfully reported) when `give_up` runs directly -- exercising its own effect
        // without depending on the driver's retry/backoff timing.
        assert!(sweep.step(&tx).await.is_err(), "window 0 must fail to read");
        assert_eq!(sweep.pos, 0, "nothing was reported before the failing read");
        sweep.give_up(&tx);
        // "needle" is planted at window 1's own start (`SEARCH_WINDOW`), now `pos` after
        // give_up -- but give_up never READS it, so it cannot be confirmed as a real line
        // start; the sentinel must keep ^needle from matching regardless, even though window
        // 1's own (now normal) read succeeds and finds "needle" itself just fine.
        assert_eq!(sweep.pos, SEARCH_WINDOW as u64);
        while !sweep.step(&tx).await.unwrap() {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 0,
            "give_up must not fake a line start it never verified"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn give_up_skips_the_whole_failing_block_not_just_one_window() {
        // batch 3 (2026-07-23), finding #5b: a failing BLOCK bigger than SEARCH_WINDOW used to
        // cost one give_up -- and its own full ANALYSIS_RETRIES-deep retry round -- PER WINDOW
        // still inside it: the real 1 MiB default block over the real 64 KiB SEARCH_WINDOW is
        // 16 windows, so a single permanently dead block wasted up to 16x(ANALYSIS_RETRIES + 1)
        // retry attempts before the sweep ever got past it. `block_size` (not SEARCH_WINDOW --
        // a shipped constant, see this module's own doc comment) is what varies here, spanning
        // 4 windows -- a testable stand-in for the real ratio: give_up must skip straight past
        // the WHOLE failing block, not merely SEARCH_WINDOW further into it.
        use crate::analyzer::Analysis;
        struct FailsSecondBlock {
            block_size: u64,
            block1_reads: std::sync::atomic::AtomicU64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsSecondBlock {
            fn size(&self) -> u64 {
                self.block_size * 3
            }
            async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<bytes::Bytes> {
                if offset < self.block_size {
                    Ok(bytes::Bytes::from(vec![b'x'; len]))
                } else if offset < 2 * self.block_size {
                    self.block1_reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Err(anyhow::anyhow!("boom"))
                } else {
                    Ok(bytes::Bytes::from(vec![b'x'; len]))
                }
            }
        }
        let block_size = 4 * SEARCH_WINDOW as u64;
        let src = std::sync::Arc::new(FailsSecondBlock {
            block_size,
            block1_reads: std::sync::atomic::AtomicU64::new(0),
        });
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            src.clone(),
            block_size as usize,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let (tx, rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        // drives `step` exactly like the real driver's own retry loop (`crate::analyzer::spawn`)
        // would -- retrying on failure, nothing else -- until block 1's read has failed
        // ANALYSIS_RETRIES + 1 times, the exact threshold that triggers give_up there. Block 0
        // heals on its very first attempt (`FailsSecondBlock`, above), so every failure here
        // lands on block 1 specifically, never re-litigating block 0.
        let mut failures = 0u8;
        loop {
            match sweep.step(&tx).await {
                Ok(true) => panic!("must not finish while block 1 is still permanently failing"),
                Ok(false) => continue,
                Err(_) => {
                    failures += 1;
                    if failures > crate::analyzer::ANALYSIS_RETRIES {
                        break;
                    }
                }
            }
        }
        assert_eq!(
            src.block1_reads.load(std::sync::atomic::Ordering::Relaxed),
            (crate::analyzer::ANALYSIS_RETRIES + 1) as u64,
            "exactly one retry round on the failing block, not one per window inside it"
        );
        let pos_before_give_up = sweep.pos;
        assert!(
            pos_before_give_up < block_size,
            "nothing at or past block 1 was ever successfully reported"
        );
        sweep.give_up(&tx);
        assert!(
            sweep.pos >= 2 * block_size,
            "give_up must land past the WHOLE failing block ({}), not just one window past {} \
             (got {})",
            2 * block_size,
            pos_before_give_up,
            sweep.pos
        );
        // reviewer-requested closer: the block-skip distance above is only half of give_up's
        // own contract -- floor semantics (this struct's own doc comment) also means the
        // summary itself, not just `pos`, must honestly report the damage. Checked on both the
        // struct's own field (what every later `step` call sees) and the published `SearchSummary`
        // (what a caller actually reads) -- `give_up` sets one via `self.failed = true` and
        // sends the other via `self.snapshot(done)`, two separate writes that a partial fix
        // could desync.
        assert!(
            sweep.failed,
            "give_up must mark the sweep's own struct field failed"
        );
        assert!(
            rx.borrow().failed,
            "give_up must publish a summary with failed=true too, not just set its own field"
        );
    }

    /// `give_up`/`step` `tx.send` doesn't leave a receiver in this test (dropped after `begin`'s
    /// own construction) -- reconstructs one bound to the same sender to read the last publish.
    fn rx_last(tx: &tokio::sync::watch::Sender<SearchSummary>) -> SearchSummary {
        tx.subscribe().borrow().clone()
    }
}
