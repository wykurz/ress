//! Budgeted newline scanning over the block cache. `ForwardScan`,
//! `BackwardScan`, `SearchForward`, and `SearchBackward` are the engine's
//! navigation read loops: every scan states a byte budget per step, its
//! window and origin are captured once at construction, and the cursor
//! lives inside the object — a pending continuation is the same value as
//! the interactive attempt. `fill_lines` is the viewport's single-call
//! read. Budgets bound read work at block granularity: a result found in a
//! block already in hand is returned even past the nominal byte budget —
//! total I/O stays within one block of the budget, and an answer from
//! paid-for bytes always beats a clamp. The two search scans are the
//! exception to "block granularity": their `limit` bound (an upper bound
//! forward, a lower bound backward) IS respected byte-exactly, the same way
//! `CountScan`'s window is -- a wraparound leg must never read, let alone
//! report, past the point it was told to stop.
use crate::cache::BlockCache;
/// One chunk's outcome of a resumable forward scan.
#[derive(Debug, PartialEq, Eq)]
pub enum FwdStep {
    /// The requested line start.
    Found(u64),
    /// EOF arrived first; the payload is the anchor to clamp to — the last
    /// real line start the scan saw, or the origin when it saw none. It is
    /// always a definitive answer, never a sentinel.
    Eof(u64),
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable scan for the `n`-th line start after a fixed origin. The
/// origin and target are captured at construction and the cursor never
/// leaves the object, so a pending continuation is the same value as the
/// interactive attempt — there is no cursor handoff to get wrong. Behavior
/// after a terminal outcome (`Found`/`Eof`) is unspecified; callers stop.
/// For an out-of-contract origin past EOF, terminal anchors clamp to the
/// file size — in-file, though not a line start.
pub struct ForwardScan {
    origin: u64,
    pos: u64,
    needed: usize,
    chunk: usize,
    last_found: Option<u64>,
    scanned: u64,
}
impl ForwardScan {
    /// Scans for the `n`-th line start strictly after line-start `from`
    /// (`n == 0` resolves to `from` itself). Reads at most `chunk` bytes per
    /// step, clamped to one so every step makes progress.
    pub fn new(from: u64, n: usize, chunk: usize) -> ForwardScan {
        ForwardScan {
            origin: from,
            pos: from,
            needed: n,
            chunk: chunk.max(1),
            last_found: None,
            scanned: 0,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    /// Consumes up to one chunk of bytes looking for the target line start,
    /// never reporting a start at or past EOF (an EOF-adjacent trailing
    /// newline terminates the scan instead).
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<FwdStep> {
        let size = cache.size();
        // a computed position past EOF could otherwise echo back through
        // a terminal anchor (the n == 0 Found or the Eof clamp); degrade to
        // the real bytes, mirroring BackwardScan's out-of-range philosophy.
        self.pos = self.pos.min(size);
        self.origin = self.origin.min(size);
        if self.needed == 0 {
            return Ok(FwdStep::Found(self.pos));
        }
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.pos < size && spent < self.chunk {
            let block = cache.block(self.pos / bs).await?;
            let lo = (self.pos % bs) as usize;
            let slice = &block[lo.min(block.len())..];
            if slice.is_empty() {
                break;
            }
            for (i, &b) in slice.iter().enumerate() {
                if b == b'\n' {
                    let ls = self.pos + i as u64 + 1;
                    // terminal returns count their partial block, so
                    // `scanned` stays literally "bytes consumed" (ERRATUM 3c#2).
                    if ls >= size {
                        self.scanned += i as u64 + 1;
                        return Ok(FwdStep::Eof(self.clamp()));
                    }
                    self.last_found = Some(ls);
                    self.needed -= 1;
                    if self.needed == 0 {
                        self.scanned += i as u64 + 1;
                        return Ok(FwdStep::Found(ls));
                    }
                }
            }
            self.pos += slice.len() as u64;
            self.scanned += slice.len() as u64;
            spent += slice.len();
        }
        if self.pos >= size {
            Ok(FwdStep::Eof(self.clamp()))
        } else {
            Ok(FwdStep::More)
        }
    }
    /// Drives the scan to its terminal anchor in chunk-sized steps, publishing
    /// progress after each and yielding so aborts have a guaranteed point to
    /// take effect on a hot cache.
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<u64> {
        loop {
            match self.step(cache).await? {
                FwdStep::Found(s) | FwdStep::Eof(s) => return Ok(s),
                FwdStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
    fn clamp(&self) -> u64 {
        self.last_found.unwrap_or(self.origin)
    }
}
/// One chunk's outcome of a resumable backward scan.
#[derive(Debug, PartialEq, Eq)]
pub enum BwdStep {
    /// The byte just after the target newline — a line start.
    Found(u64),
    /// Fewer than `n` newlines exist in the window; the line start is 0.
    Top,
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable scan for the `n`-th newline above a fixed position. The search
/// window is captured once, at construction: the byte at `pos - 1` is
/// excluded, because for a line-start `pos` it is the newline that made it a
/// line start, and for `pos == size` it is a trailing newline that must not
/// count as a line above. Resumption is just another `step` — the exclusion
/// is never re-derived, so the off-by-one class from bare resume cursors is
/// unrepresentable. Behavior after a terminal outcome is unspecified.
pub struct BackwardScan {
    hi: u64,
    needed: usize,
    chunk: usize,
    scanned: u64,
}
impl BackwardScan {
    /// Scans `[0, pos - 1)` downward for the `n`-th newline, reading at most
    /// `chunk` bytes per step (clamped to one so every step makes progress).
    /// Callers guard `n >= 1`; `n == 0` resolves as `Top` without reading.
    pub fn new(pos: u64, n: usize, chunk: usize) -> BackwardScan {
        BackwardScan {
            hi: pos.saturating_sub(1),
            needed: n,
            chunk: chunk.max(1),
            scanned: 0,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    /// Bytes not yet searched. At construction this is the full window, which
    /// makes an honest progress span for a scan started as its own phase.
    pub fn remaining_bytes(&self) -> u64 {
        self.hi
    }
    /// Consumes up to one chunk of bytes searching downward.
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<BwdStep> {
        if self.needed == 0 {
            return Ok(BwdStep::Top);
        }
        // a window past EOF can never make progress (past-EOF blocks are
        // empty, and the empty-slice break skips the `hi = lo` advance), so
        // an out-of-range position must degrade to scanning the real bytes
        // rather than stepping forever (ERRATUM 3c#3).
        self.hi = self.hi.min(cache.size());
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.hi > 0 && spent < self.chunk {
            let idx = (self.hi - 1) / bs;
            let lo = idx * bs;
            let block = cache.block(idx).await?;
            let take = ((self.hi - lo) as usize).min(block.len());
            let slice = &block[..take];
            if slice.is_empty() {
                break;
            }
            for (i, &b) in slice.iter().enumerate().rev() {
                if b == b'\n' {
                    self.needed -= 1;
                    if self.needed == 0 {
                        // count the partially examined block, so `scanned`
                        // stays literally "bytes consumed" (ERRATUM 3c#2).
                        self.scanned += (slice.len() - i) as u64;
                        return Ok(BwdStep::Found(lo + i as u64 + 1));
                    }
                }
            }
            spent += slice.len();
            self.scanned += slice.len() as u64;
            self.hi = lo;
        }
        if self.hi == 0 {
            Ok(BwdStep::Top)
        } else {
            Ok(BwdStep::More)
        }
    }
    /// Drives the scan to its terminal anchor (`Top` is the definitive anchor
    /// 0) in chunk-sized steps, publishing progress after each and yielding so
    /// aborts have a guaranteed point to take effect on a hot cache.
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<u64> {
        loop {
            match self.step(cache).await? {
                BwdStep::Found(s) => return Ok(s),
                BwdStep::Top => return Ok(0),
                BwdStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}
/// One chunk's outcome of a resumable newline count. The status worker (see
/// `crate::status`) is the production consumer, stepping this directly
/// inside its own `select!` loop rather than through a synchronous,
/// cache-only sibling — a background task can afford to await a block a
/// synchronous draw-time query never could.
#[derive(Debug, PartialEq, Eq)]
pub enum CountStep {
    /// Every byte of the window has been examined.
    Done(u64),
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable count of the newlines in `[from, to)`. The window is captured
/// at construction (an inverted window is empty); `step` consumes one budget
/// chunk at a time, and out-of-contract positions clamp into the file like
/// every scan object. Behavior after `Done` is unspecified; callers stop.
pub struct CountScan {
    pos: u64,
    end: u64,
    chunk: usize,
    found: u64,
    scanned: u64,
}
impl CountScan {
    /// Counts newlines in `[from, to)`, reading at most `chunk` bytes per
    /// step (clamped to one so every step makes progress).
    pub fn new(from: u64, to: u64, chunk: usize) -> CountScan {
        CountScan {
            pos: from,
            end: to.max(from),
            chunk: chunk.max(1),
            found: 0,
            scanned: 0,
        }
    }
    /// Bytes examined so far.
    // dead on the lib target: the status worker, `step`'s production
    // caller, tracks convergence through the published `StatusSnapshot`
    // instead of bytes scanned, so this has no production caller — kept for
    // the test suite, same as ForwardScan/BackwardScan's `scanned` would be
    // if anything ever needed to build a progress bar over a count.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    /// Consumes up to one chunk of bytes counting newlines, reading through
    /// `warm()` rather than `block()`: the status worker is a background
    /// consumer racing the interactive viewport for the same cache, and
    /// must not promote or reorder the working set any more than the index
    /// scan does (see `crate::schedule::ScanScheduler`).
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<CountStep> {
        let size = cache.size();
        // computed windows past EOF degrade to the real bytes, mirroring
        // the other scan objects' out-of-contract philosophy.
        self.pos = self.pos.min(size);
        self.end = self.end.min(size);
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.pos < self.end && spent < self.chunk {
            let block = cache.warm(self.pos / bs).await?;
            let lo = (self.pos % bs) as usize;
            // the outer clamp makes this unreachable; kept for sibling
            // consistency (a bare range start would panic where take merely
            // saturates).
            let lo = lo.min(block.len());
            let take = ((self.end - self.pos) as usize).min(block.len() - lo);
            let slice = &block[lo..lo + take];
            if slice.is_empty() {
                break;
            }
            self.found += memchr::memchr_iter(b'\n', slice).count() as u64;
            self.pos += slice.len() as u64;
            self.scanned += slice.len() as u64;
            spent += slice.len();
        }
        if self.pos >= self.end {
            Ok(CountStep::Done(self.found))
        } else {
            Ok(CountStep::More)
        }
    }
}
/// One chunk's outcome of a resumable pattern search.
// dead on the lib target: the navigation-resolution spawn sites that drive
// `SearchForward`/`SearchBackward` from a real search request arrive in a
// later task (search v1, task 4+); kept alive here for the test suite,
// same idiom as `CountScan::scanned`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub enum SearchStep {
    /// a match: its own byte offset, plus the byte just after the last `\n`
    /// the scan saw before it (None = no newline between the scan's start
    /// and the match -- the caller resolves the line start itself).
    Found {
        match_at: u64,
        line_start: Option<u64>,
    },
    /// this LEG is over (EOF forward / offset 0 backward) with no match --
    /// deliberately not named Exhausted: wrap is the caller's policy.
    End,
    /// The chunk is consumed; call `step` again to continue.
    More,
}
/// A resumable forward search for the next match at or after `from`, bounded
/// above by `limit` (typically the file size, or an earlier position when a
/// wraparound leg must stop before re-finding its own starting match). The
/// carry keeps up to `MAX_MATCH_LEN - 1` already-read bytes BEHIND `pos` --
/// `[pos - keep, pos)`, not ahead of it -- so a match straddling a block
/// read is still found whole. The guarantee is `find_all`'s own LEFTMOST
/// match, for any match no longer than `MAX_MATCH_LEN` (batch 3
/// (2026-07-23), finding #2 -- `step`'s own `safe_to` gate, below, is what
/// makes this true; before it existed this was only eager-first-COMPLETE:
/// for an alternation where a later, shorter branch could complete before
/// an earlier, longer one had enough hay to confirm, e.g. `foobar|oo`, the
/// shorter match could be reported instead of the truly leftmost one --
/// `search_forward_returns_the_leftmost_match_not_the_first_complete_one`,
/// this module's own test, pins the fixed fixture). Only the pattern's own
/// documented cap remains as a residual: a match longer than
/// `MAX_MATCH_LEN` can still go unreported if the leftmost such candidate
/// never fits inside any hay this scan holds at once -- `safe_to`'s own doc
/// comment states the exact boundary, and
/// `search_forward_returns_a_later_match_when_the_leftmost_exceeds_the_cap`
/// pins it deliberately, not accidentally, unreported.
///
/// ANCHOR CORRECTNESS (`^`/`$`, `multi_line` -- `SearchPattern::compile`'s own doc comment):
/// `^` needs `ctx`, below: WITHOUT it, `pos`'s own hay-index (0, always, since `carry` there would
/// hold only bytes AT OR AFTER `pos`) is unconditionally `^`-eligible regardless of the REAL
/// byte before `pos` -- true not just at `from` (the scan's own origin, if it sits mid-line)
/// but at EVERY carry-shrink seam thereafter, since the shrunk carry's own new front is exactly
/// as edge-shaped as `from` was. Confirmed by two regressions found during this review, before
/// `ctx` existed: `search_forward_does_not_fake_a_line_start_at_a_carry_shrink_seam` (a false
/// match, far from `from`, at a shrink seam) and a genuine arithmetic panic in this method's own
/// `scanned` bookkeeping (`e < carry_len`, i.e. a "match" resolving entirely inside bytes ALREADY
/// fully searched on a prior iteration -- only possible because that earlier search used the
/// correct, now-stale look-behind while this one used none). `ctx` fixes both: a permanent real
/// look-behind byte, threaded through every iteration (not just the first), makes `^`'s verdict
/// at any given position stable across the whole scan, restoring the invariant this method's
/// own comments already relied on.
///
/// `$` now gets a genuine end margin here too, the SAME KIND `SweepAnalysis::step`'s own
/// `safe_to` always reserved (review response, batch 3 (2026-07-23) -- `step`'s own `safe_to`,
/// below, was finding #2's fix for LEFTMOST-ness, not `$`, but it closes this as a side effect):
/// a match is only ever accepted once `match_at < safe_to`, i.e. once at least `MAX_MATCH_LEN`
/// bytes past its own start have been read (or this scan hit its own read boundary) -- for any
/// match SHORTER than `MAX_MATCH_LEN`, that margin always leaves at least one real byte past the
/// match's own end already in hand, so `$` is checked against genuine content there, never hay's
/// own CURRENT (possibly artificial) end. Two narrow residuals remain, both confined to this
/// scan's own read boundary rather than an arbitrary mid-file accident: a match of *exactly*
/// `MAX_MATCH_LEN` bytes can still have its own `$` confirmation land precisely on hay's own
/// current edge, mid-file, no wrap seam involved -- `safe_to`'s own margin is calibrated to
/// exactly that length, with nothing left over
/// (`search_forward_dollar_can_still_false_positive_at_exactly_max_match_len`, this module's own
/// test, pins it); and, independent of length, a BOUNDED leg reaching its own artificial `limit`
/// (never the true file end) gets no margin at all there (`safe_to == read_end`, no
/// `MAX_MATCH_LEN` subtracted), so a match of any length ending exactly at that seam can also
/// false-positive
/// (`search_forward_dollar_can_false_positive_at_a_bounded_legs_own_limit`, pinned the same
/// way). Neither is a new class of approximation -- both are the identical
/// `MAX_MATCH_LEN`-cap-and-boundary approximation docs/search.md's own "Anchored patterns"
/// section already documents for `n`/`N`, now narrowed to these two specific edges instead of
/// any mid-file chunk boundary at all, and left as that same honest approximation rather than
/// given a third lookahead mechanism of its own.
// dead on the lib target: see `SearchStep`'s own doc comment.
#[cfg_attr(not(test), allow(dead_code))]
pub struct SearchForward {
    pattern: std::sync::Arc<crate::search::SearchPattern>,
    pos: u64,
    limit: u64,
    carry: Vec<u8>,
    last_nl: Option<u64>,
    chunk: usize,
    scanned: u64,
    /// The real byte at `pos - 1`, once known -- `None` only before `ctx_ready`, and (even
    /// after) whenever `pos == 0` at the point it was resolved (file position 0 needs no real
    /// predecessor of its own: it is unconditionally a valid line start). Read lazily on the
    /// first `step` call (needs a cache access `new` cannot make -- the task brief's own
    /// "OPTIONAL" seed, folded into the general mechanism below since a one-time seed alone
    /// does not survive a later carry-shrink), then kept current across every shrink after
    /// that. See this struct's own ANCHOR CORRECTNESS doc comment.
    ctx: Option<u8>,
    ctx_ready: bool,
    /// The block fetched to seed `ctx` (above), kept in case the FIRST payload iteration in
    /// this same call turns out to want the identical block -- true whenever `from` sits
    /// mid-block (not block-aligned): reusing it avoids a second, PROMOTING `cache.block()`
    /// touch of a block this scan's own bookkeeping seed already warmed a moment earlier, the
    /// same hazard `SearchBackward::step`'s own per-iteration `warmed` field fixes (batch 3
    /// (2026-07-23), finding #7). Consumed (`.take()`n) on the very next block fetch regardless
    /// of whether the index matched, so a stale entry is never consulted twice.
    seed_block: Option<(u64, bytes::Bytes)>,
}
#[cfg_attr(not(test), allow(dead_code))]
impl SearchForward {
    /// Searches forward from `from`, bounded above by `limit` (clamped to
    /// the real file size at `step` time), reading at most `chunk` bytes per
    /// step (clamped to one so every step makes progress). `limit` is an
    /// EXCLUSIVE upper bound on match starts: a match starting exactly at
    /// `limit` is not found -- asymmetric with `SearchBackward::new`'s
    /// INCLUSIVE `limit`; a wrap-seam composer must account for that one
    /// byte.
    pub fn new(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        from: u64,
        limit: u64,
        chunk: usize,
    ) -> SearchForward {
        SearchForward {
            pattern,
            pos: from,
            limit,
            carry: Vec::new(),
            last_nl: None,
            chunk: chunk.max(1),
            scanned: 0,
            ctx: None,
            ctx_ready: false,
            seed_block: None,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        // an out-of-contract `pos` (e.g. `SearchForward::new`'s own `from` past EOF -- a
        // wraparound leg's naive caller, or a stale saved position on a since-truncated file)
        // must degrade to the real bytes BEFORE anything indexes off it, mirroring every other
        // scan object's philosophy (`ForwardScan::step`'s own comment, `CountScan::step`'s own
        // clamp) -- clamp FIRST, seed second: batch 3 (2026-07-23), finding #6 (probe-verified:
        // `SearchForward::new(pattern, u64::MAX, ..)` panicked indexing the ctx seed's own
        // block, fetched for a block index nowhere near this file, before this clamp existed).
        self.pos = self.pos.min(cache.size());
        if !self.ctx_ready {
            // lazy: `new` has no cache access. One extra read, at most once per scan -- through
            // `warm()`, not `block()`: this is bookkeeping (a look-behind byte), not user-visible
            // payload content, `CountScan`'s own precedent (batch 3 (2026-07-23), finding #7).
            self.ctx = if self.pos == 0 {
                None
            } else {
                let bs = cache.block_size() as u64;
                let p = self.pos - 1;
                let idx = p / bs;
                let block = cache.warm(idx).await?;
                self.seed_block = Some((idx, block.clone()));
                Some(block[(p % bs) as usize])
            };
            self.ctx_ready = true;
        }
        let size = cache.size().min(self.limit);
        // ZERO-WIDTH AT THE TRUE EOF, the case the main loop's own in-body widening (below)
        // structurally cannot reach: when `pos` already sits at (or past) the file's own true
        // end, the loop's own `while pos < size` never runs at all -- an empty file (`^$`'s own
        // fixture) or an `origin` that already lands exactly at EOF, not merely a bounded leg's
        // own `limit` (`pos >= size` alone is not enough: a wrap leg's own truncated `limit` can
        // make that true well before the real EOF, and must NOT gain this widening -- batch 3
        // (2026-07-23), finding #9). No new read needed: a zero-width match needs only `^`/`$`
        // context, which `self.ctx` (seeded above) already is.
        if self.pos >= cache.size() {
            let lb = self.ctx.is_some() as usize;
            let probe: Vec<u8> = self.ctx.into_iter().collect();
            if self
                .pattern
                .find_starting_in(&probe, lb..(lb + 1))
                .is_some()
            {
                return Ok(SearchStep::Found {
                    match_at: self.pos,
                    line_start: self.last_nl.filter(|&nl| nl < self.pos).map(|nl| nl + 1),
                });
            }
        }
        let bs = cache.block_size() as u64;
        let mut spent = 0usize;
        while self.pos < size && spent < self.chunk {
            let idx = self.pos / bs;
            // reuse the seed's own block when `from` sat mid-block (so the seed and this,
            // this scan's very first payload fetch, land on the identical index) -- avoids the
            // redundant, PROMOTING second touch `seed_block`'s own doc comment describes.
            let block = match self.seed_block.take() {
                Some((sidx, sb)) if sidx == idx => sb,
                _ => cache.block(idx).await?,
            };
            let lo = (self.pos % bs) as usize;
            let take = (block.len() - lo.min(block.len())).min((size - self.pos) as usize);
            if take == 0 {
                break;
            }
            let slice = &block[lo..lo + take];
            // the carry holds up to MAX_MATCH_LEN-1 already-read bytes ahead
            // of the fresh slice: hay = carry + slice. The accept range is
            // the WHOLE hay, 0..hay.len(), not just the fresh tail -- a
            // start near the end of a PAST slice could not be resolved with
            // that slice alone (the match may have needed bytes this slice
            // now supplies), so it must be re-offered here. Re-scanning the
            // carry every time costs work (bounded: carry is always <
            // MAX_MATCH_LEN) but never re-reports a match, and nothing is
            // ever silently skipped the way excluding the carry from the
            // accept range would.
            //
            // GUARDRAIL (narrowed by batch 3 (2026-07-23), finding #2, not removed): this
            // whole-hay accept, together with `safe_to` below, is what makes `step`'s own Found
            // the LEFTMOST sub-cap match (this struct's own doc comment) -- it stays safe as an
            // ENUMERATING caller's own model ONLY because `step` never re-offers an
            // ALREADY-RETURNED match's own carry again (once `Found` comes back, this scan is
            // done). An ENUMERATING caller (e.g. the background match-counting sweep,
            // `SweepAnalysis`) must still NOT copy this range as-is: it would recount every seam
            // match once per iteration it stays in the carry -- fresh-starts-only
            // (`carry_len..`) is what that caller needs instead, unrelated to and unaffected by
            // `safe_to`'s own leftmost guarantee here (see `SweepAnalysis`'s own GUARDRAIL doc
            // comment). NOTE the one thing THIS finding changed about the invariant below: before
            // `safe_to` existed, a start could never resolve to Found using only carry bytes (`e
            // <= carry_len`) -- that would have meant a complete match sat wholly inside an
            // already-fully-searched hay, which would have returned already. `safe_to` makes
            // this a real, HANDLED case now: a candidate DEFERRED (found, but not yet the
            // confirmed leftmost) on an earlier iteration can be re-found and finally accepted,
            // using only carry bytes, on a later one -- see the `saturating_sub` on `scanned`
            // just below the accept check, and its own comment.
            //
            // `carry_len` includes `ctx` (`lb`) when present -- so is `hay`, prepended with it
            // here for the search only (never stored: `self.carry` itself never holds `ctx`,
            // see its own field's -- unaffected -- shape). This keeps every downstream formula
            // (`match_at`, `last_nl`, `scanned`) byte-for-byte identical to before `ctx` existed
            // (the `+lb`/`-lb` shift cancels out arithmetically), and `^`'s own accept-range
            // floor at `lb` (not 0) means `ctx` is visible to the regex for look-behind but can
            // never itself be reported as a match's start (this struct's own ANCHOR CORRECTNESS
            // doc comment).
            let lb = self.ctx.is_some() as usize;
            let carry_len = lb + self.carry.len();
            self.carry.extend_from_slice(slice);
            let mut hay = Vec::with_capacity(lb + self.carry.len());
            hay.extend(self.ctx);
            hay.extend_from_slice(&self.carry);
            let hay = &hay[..];
            // LEFTMOST (batch 3 (2026-07-23), finding #2): `find_starting_in` returns the first
            // position where SOME alternative completes within the CURRENT (possibly truncated)
            // hay -- for an alternation like `a.{10}z|b`, a short branch can complete on far less
            // hay than a longer, earlier-starting one needs, so the naive "accept the first
            // Found" is only eager-first-COMPLETE, not truly leftmost. `safe_to` is the absolute
            // position up to which EVERY start has either had a full `MAX_MATCH_LEN` bytes of
            // lookahead already read (enough to give any sub-cap alternative starting there a
            // fair chance to complete) or this scan has reached its own read boundary (`size`,
            // real EOF or a wrap leg's own `limit`) -- past which nothing more will EVER arrive
            // to change the verdict. Only a match below `safe_to` is truly confirmed leftmost:
            // anything to ITS left is, by construction, equally covered by the same lookahead
            // margin, so `find_starting_in` -- which always returns the smallest completing
            // position -- would already have returned it instead, had one existed.
            let read_end = self.pos + take as u64;
            let at_final = read_end >= size;
            // ZERO-WIDTH AT THE TRUE EOF (batch 3 (2026-07-23), finding #9): accept ranges are
            // half-open and exclude a start == hay's own end everywhere else (`lb..hay.len()`),
            // but at the file's own TRUE end (`read_end >= cache.size()`, not merely a bounded
            // leg's own `limit` -- `read_end <= size <= cache.size()` always, so this can only
            // hold when all three are equal) position `size` is a legitimate zero-width start of
            // its own (`$` unconditionally holds there). Widening the accept end by one position
            // only ever admits a zero-width match: nothing else could start at hay's own last
            // index and still complete, since there is no more hay past it for a real match to
            // consume. `safe_to` widens identically -- a candidate exactly at hay's own end is,
            // by definition, as final as this scan will ever get.
            let at_eof = read_end >= cache.size();
            let accept_end = if at_eof { hay.len() + 1 } else { hay.len() };
            let safe_to = if at_eof {
                read_end + 1
            } else if at_final {
                read_end
            } else {
                read_end.saturating_sub(crate::search::MAX_MATCH_LEN as u64 - 1)
            };
            if let Some((s, e)) = self.pattern.find_starting_in(hay, lb..accept_end) {
                let match_at = self.pos - carry_len as u64 + s as u64;
                // last_nl only from bytes strictly before the match's own start -- `hay[..s]`
                // is, by construction, exactly those bytes, so a newline found there is always
                // < match_at, no guard needed. Degenerate for a pattern containing an explicit
                // `\n` (non-ordinary -- `.` never matches `\n`, so an ordinary pattern can't
                // itself straddle a line): if hay[..s] has none of its own, this falls back to
                // the PERSISTED `self.last_nl`, set on an earlier iteration that could not yet
                // know whether its own newline would end up sitting INSIDE this match's own
                // span (batch 3 (2026-07-23), finding #4, probe-verified: `foo\nbar` over
                // `xxfoo\nbarzz` at block_size 1 records the `\n` at 5 while no match is yet in
                // view, then finds the match at 2 spanning [2, 9) -- 5 is inside it, not before
                // it). With `multi_line` support this is load-bearing correctness now, not a
                // documented caveat: the guard below is applied ONLY to the persisted fallback
                // (the fresh `hay[..s]` scan just above is already always safe on its own).
                // Backward is unaffected -- it derives line_start fresh from the hay below `s`
                // every time, never from a persisted field.
                if let Some(nl) = memchr::memrchr(b'\n', &hay[..s]) {
                    self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
                }
                if match_at < safe_to {
                    // only the fresh bytes actually needed to confirm the match count as
                    // consumed THIS step (ERRATUM 3c#2's rule). Unlike before `safe_to`
                    // existed, `e > carry_len` no longer always holds here: a candidate
                    // DEFERRED on an earlier iteration (found, but not yet safe) can be
                    // re-found on a later one using only bytes already inside the old carry --
                    // already tallied into `scanned` back when it was first deferred (the
                    // fallthrough below ran then) -- so `saturating_sub` correctly adds zero
                    // for those, rather than underflow or double-count them.
                    self.scanned += (e as u64).saturating_sub(carry_len as u64);
                    // a stale persisted `last_nl` sitting AT OR PAST `match_at` is not this
                    // match's own line start at all -- report `None` instead, letting the
                    // caller's own bounded backward hunt (`resolve_line_start_step`/
                    // `found_outcome_pending`, document.rs) resolve the true one, exactly as it
                    // already does when no newline has been seen at all (this match's own doc
                    // comment, above).
                    let line_start = self.last_nl.filter(|&nl| nl < match_at).map(|nl| nl + 1);
                    return Ok(SearchStep::Found {
                        match_at,
                        line_start,
                    });
                }
                // not yet safe: fall through and keep reading. The candidate is never lost --
                // `match_at < read_end` and `read_end - match_at <= MAX_MATCH_LEN - 1` (else it
                // would already be safe), so it sits within the trailing MAX_MATCH_LEN-1 bytes
                // the carry-shrink below keeps; the whole-hay accept re-offers it (or a still
                // earlier alternative that only now has enough hay to complete) next iteration.
            } else if let Some(nl) = memchr::memrchr(b'\n', hay) {
                self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
            }
            self.pos += take as u64;
            self.scanned += take as u64;
            spent += take;
            // shrink the carry to the last MAX_MATCH_LEN-1 bytes for the seam, and update the
            // real look-behind byte to match: the new `ctx` is the real last byte just dropped
            // (the real predecessor of the new front), never fabricated -- see this struct's
            // own ANCHOR CORRECTNESS doc comment. `cut` can be 0 (carry hasn't grown past
            // `keep` yet); only touch `ctx` when something was actually dropped.
            let keep = (crate::search::MAX_MATCH_LEN - 1).min(self.carry.len());
            let cut = self.carry.len() - keep;
            if cut > 0 {
                self.ctx = Some(self.carry[cut - 1]);
                self.carry.drain(..cut);
            }
        }
        if self.pos >= size {
            Ok(SearchStep::End)
        } else {
            Ok(SearchStep::More)
        }
    }
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<SearchStep> {
        loop {
            match self.step(cache).await? {
                SearchStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
                terminal => return Ok(terminal),
            }
        }
    }
}
/// A resumable backward search for the previous match below `hi`, bounded
/// below by `limit`. Windows are read high to low; the carry keeps up to
/// `MAX_MATCH_LEN - 1` bytes from the LOW end of the already-processed
/// (higher) region, prepended after the fresh slice: `hay = slice + carry`.
/// Unlike `SearchForward`, a match starting in the fresh slice never needs
/// bytes this scan hasn't read yet -- anything it could extend into lies
/// ABOVE it, in territory an earlier (higher) iteration already read into
/// carry -- so restricting starts to the fresh slice (`0..slice.len()`) is
/// safe here: nothing is ever deferred past the point it's resolvable, the
/// way excluding forward's carry was.
///
/// ANCHOR CORRECTNESS (`^`/`$`, `multi_line` -- `SearchPattern::compile`'s own doc comment):
/// `^` needs a real look-behind byte at `lo - 1` (this iteration's own low edge), the SAME
/// reasoning as `SearchForward`'s own ANCHOR CORRECTNESS doc comment, but a DIFFERENT mechanism:
/// `lo` is a fresh position every iteration (a new block boundary each time), never re-tested at
/// a later hay-index the way forward's carry-shrink ages a position toward 0, so there is
/// nothing to chain -- `step` fetches the one real byte at `lo - 1` itself, EVERY iteration,
/// rather than carrying it forward. Cheap in practice: `cache.block` is a cache, and `lo - 1` is
/// almost always inside the SAME block just fetched above (a genuine second read only at a
/// block boundary, which happens once per block, not once per byte).
///
/// `$` is fully correct here, with no residual of its own (batch 3 (2026-07-23), finding #3's
/// own side effect, not something #3 set out to fix): `carry` no longer starts EMPTY -- `step`'s
/// own STRADDLE READ, below, seeds it with the real bytes in `[hi, hi + MAX_MATCH_LEN)` (capped
/// at the true EOF) before the very first iteration ever runs, specifically so a match starting
/// below `hi` but extending past it can be CONFIRMED at all (this struct's own name for the
/// class, "a straddler"). That same seed happens to close the OLD gap here too: a sub-cap match
/// (length `L <= MAX_MATCH_LEN`) starting anywhere below `hi` has its own `$`-confirmation byte
/// at `s + L`, and `s + L < hi + MAX_MATCH_LEN` always holds (`s < hi`, `L <= MAX_MATCH_LEN`) --
/// so that byte is ALWAYS real content the seed already read, never hay's own artificial edge.
/// Before this seed existed, `carry` only grew toward its `MAX_MATCH_LEN - 1` cap over several
/// iterations, so a match near the top of an EARLY slice could have its own `$` confirmation
/// land exactly on hay's own THEN-thin edge -- the identical chunk-boundary residual
/// `SearchForward`'s own ANCHOR CORRECTNESS doc comment still names for `$` (docs/search.md's
/// own "Anchored patterns" section): review response (batch 3 (2026-07-23)) -- that one is now
/// NARROWER, not gone (`SearchForward`'s own `safe_to`, finding #2, gives it a genuine
/// end-margin too, closing the general mid-file case there as well), but not zero the way this
/// struct's own is: this struct's seed reads a FULL `MAX_MATCH_LEN` bytes past `hi` (one more
/// than `MAX_MATCH_LEN - 1`), so even the worst-case straddler (`s = hi - 1`, `L =
/// MAX_MATCH_LEN`) has its own confirmation byte strictly inside the seed's own read range;
/// `SearchForward`'s own carry caps at `MAX_MATCH_LEN - 1`, exactly `L` for a maximal-length
/// match, leaving zero slack at that one exact length. See `SearchForward`'s own doc comment for
/// the precise, now-narrower shape of what remains there.
///
/// THREE cases, not one, for what context `step` hands the regex (see its own inline comment):
/// `lo == 0` needs none -- file position 0 is unconditionally a valid line start, and relying
/// on the regex's own "start of hay" default is correct there. `lo == floor > 0` -- `lo - 1`
/// sits AT OR BELOW `limit`; reading it would violate this module's own "a wraparound leg must
/// never read, let alone report, past the point it was told to stop" rule (this file's own
/// header doc comment) -- gets an EXPLICIT non-`\n` sentinel, NOT the bare absence a first
/// attempt at this used (a real regression, caught before landing: an absent context byte
/// leaves `lb == 0`, re-opening the exact "hay's own edge is unconditionally `^`-eligible" hole
/// this whole mechanism exists to close, at exactly the one spot meant to be conservative
/// instead). Anything else reads the real byte. Net residual: a `^` match starting exactly at
/// a bounded wraparound leg's own floor can be MISSED -- never a false positive, only a
/// possible miss, symmetric with `SweepAnalysis::give_up`'s own gap after a skipped window.
// dead on the lib target: see `SearchStep`'s own doc comment.
#[cfg_attr(not(test), allow(dead_code))]
pub struct SearchBackward {
    pattern: std::sync::Arc<crate::search::SearchPattern>,
    hi: u64,
    limit: u64,
    carry: Vec<u8>,
    chunk: usize,
    scanned: u64,
    /// A block fetched purely for `^` look-behind context (`step`'s own `ctx`, below), kept in
    /// case the VERY NEXT iteration's own payload turns out to want the identical block --
    /// which it structurally always does, whenever `lo` lands block-aligned (see `step`'s own
    /// derivation): reusing it there avoids a SECOND, promoting `cache.block()` touch of a block
    /// this scan's own bookkeeping peek already warmed a moment earlier, which is what actually
    /// caused batch 3 (2026-07-23), finding #7's "no-match backward scan leaves nearly every
    /// block protected" -- switching the context fetch alone to `cache.warm()` does NOT fix
    /// this by itself (a probationary HIT still promotes under `cache.block()`, regardless of
    /// which call warmed it first): the redundant second cache call must never happen at all.
    warmed: Option<(u64, bytes::Bytes)>,
    /// Whether the straddle-read seed (`step`'s own doc comment) has run yet -- `new` has no
    /// cache access, so this happens lazily on the first `step` call, mirroring `SearchForward`'s
    /// own `ctx_ready` (batch 3 (2026-07-23), finding #3).
    straddle_seeded: bool,
    /// Whether the zero-width-at-EOF terminal check (`step`'s own doc comment) has run yet --
    /// exactly once, like `straddle_seeded` (batch 3 (2026-07-23), finding #9).
    terminal_checked: bool,
}
#[cfg_attr(not(test), allow(dead_code))]
impl SearchBackward {
    /// Searches backward from (but not including) `hi`, bounded below by
    /// `limit`, reading at most `chunk` bytes per step (clamped to one so
    /// every step makes progress). `limit` is an INCLUSIVE lower bound on
    /// match starts: a match starting exactly at `limit` IS found --
    /// asymmetric with `SearchForward::new`'s EXCLUSIVE `limit`; a wrap-seam
    /// composer must account for that one byte.
    pub fn new(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        hi: u64,
        limit: u64,
        chunk: usize,
    ) -> SearchBackward {
        SearchBackward {
            pattern,
            hi,
            limit,
            carry: Vec::new(),
            chunk: chunk.max(1),
            scanned: 0,
            warmed: None,
            straddle_seeded: false,
            terminal_checked: false,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn scanned(&self) -> u64 {
        self.scanned
    }
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        let floor = self.limit;
        // an out-of-range hi must degrade to the real bytes, mirroring
        // BackwardScan's philosophy (ERRATUM 3c#3).
        self.hi = self.hi.min(cache.size());
        let bs = cache.block_size() as u64;
        if !self.straddle_seeded {
            // STRADDLER READ (batch 3 (2026-07-23), finding #3): `hi` used to bound reads and
            // accepts TOGETHER, so a match starting below `hi` but extending past it (a
            // straddler) was structurally unreadable, let alone findable, by this leg -- the
            // mirror image of forward's own read-past-the-naive-bound widening (`wrapped_leg`'s
            // own forward-leg-2 derivation, document.rs). Pre-seeding `carry` with the bytes
            // AT-OR-ABOVE `hi` (lookahead only, never an acceptable start of their own -- the
            // per-iteration accept range stays `lb..(lb+slice.len())`, strictly the fresh,
            // below-`hi` slice, below) gives the FIRST iteration's own search the same
            // "confirm a match near the seam" power forward's carry already has at every one of
            // ITS OWN internal seams, with no other change to the per-iteration loop needed: the
            // existing "keep the low MAX_MATCH_LEN-1 bytes" shrink, below, naturally ages this
            // seed out exactly once a candidate is too far from `hi` to ever need it, the same
            // way it already ages out any other carry content. Read through `warm()`, not
            // `block()`: lookahead bookkeeping, not user-visible payload (batch 3, finding #7's
            // own precedent).
            //
            // BLOCK-granular reuse, not just byte-granular (a review-caught correction: an
            // earlier version of this comment reasoned "payload only ever reads BELOW `hi`" and
            // called the territory disjoint -- true of the BYTES, false of the BLOCKS they live
            // in, and promotion is block-granular). Whenever `hi` itself is mid-block, the
            // seed's own FIRST block (`hi / bs`) IS the main loop's own first payload block
            // (`(hi - 1) / bs` -- the same block, since both bytes share it): reused via
            // `warmed` below, exactly like the per-iteration `ctx` fetch does, or the identical
            // redundant-second-touch promotion hazard reopens (probe-verified: `hi = 5` and
            // `hi = 13` over 64 bytes @ block_size 8 both left `protected_len() == 1` before
            // this reuse existed). Mutually exclusive with the terminal check's own `warmed`
            // use, below: this loop reads NOTHING at all when `hi == cache.size()` (`seed_end ==
            // hi` there, so the `while` never runs once) -- exactly the one case the terminal
            // check's own reuse applies to instead.
            let seed_end = self
                .hi
                .saturating_add(crate::search::MAX_MATCH_LEN as u64)
                .min(cache.size());
            let mut pos = self.hi;
            while pos < seed_end {
                let idx = pos / bs;
                let block = cache.warm(idx).await?;
                if pos == self.hi && self.hi > 0 && idx == (self.hi - 1) / bs {
                    self.warmed = Some((idx, block.clone()));
                }
                let lo = (pos % bs) as usize;
                let take = (block.len() - lo.min(block.len())).min((seed_end - pos) as usize);
                if take == 0 {
                    break;
                }
                self.carry.extend_from_slice(&block[lo..lo + take]);
                pos += take as u64;
            }
            self.straddle_seeded = true;
        }
        if !self.terminal_checked {
            self.terminal_checked = true;
            // ZERO-WIDTH AT THE TRUE EOF (batch 3 (2026-07-23), finding #9): accept ranges are
            // half-open and exclude `hi` itself everywhere else in this struct (`0..slice.len()`,
            // below), but when `hi` IS the file's own true end (`cache.size()`, not merely a
            // bounded leg's own `limit`), position `hi` is a legitimate zero-width match start of
            // its own (`$` unconditionally holds there, being the true haystack end; other
            // zero-width assertions like `^` depend on the real byte just below it, fetched
            // fresh here exactly like the per-iteration `ctx` below). Checked ONCE, before the
            // main loop even runs, since a zero-width match needs no content at all to confirm --
            // only `^`/`$` context -- and must fire even when the loop itself never would (an
            // empty file, `hi == floor == 0`, is exactly `^$`'s own fixture). A REAL, non-zero-
            // width match can never start here regardless (nothing exists to read at or past
            // `hi`), so widening the accept range this one extra position is never able to admit
            // anything but a genuine zero-width match -- no separate length check needed.
            //
            // this read shares `warmed` with the per-iteration `ctx` fetch below: `(hi - 1) /
            // bs` is the SAME block index the main loop's own FIRST iteration will fetch as
            // payload whenever this check does not itself return (`idx` there is computed
            // identically, since `hi` has not moved yet) -- without reusing it here too, this
            // read would independently re-trigger finding #7's own redundant-second-touch
            // promotion hazard (caught by `search_backward_context_peeks_do_not_promote_
            // untouched_blocks`, this module's own test, when this fix first landed without it).
            //
            // net residual, accepted rather than re-derived further: `hi == cache.size()` holds
            // for BOTH callers -- `search_next`'s own leg 1 (`hi = origin`, only coincidentally
            // the true EOF when the cursor already sits there) and `wrapped_leg`'s own leg 2
            // (`hi = size`, always, unconditionally the wrap's own far end) -- and this struct
            // has no field to tell them apart. For every other match shape, leg 1's own
            // exclusive `hi` is what excludes a self-hit (the cursor re-finding the match it is
            // already parked on), forcing a genuine wrap (`wrapped: true`) to re-find a lone
            // match again (`search_next_backward_self_hit_wraps_to_find_it_again`, document.rs).
            // a zero-width match exactly at EOF is the one shape immune to that: leg 1 finds it
            // directly here instead (`wrapped: false`), never via the wrap. Deliberately left
            // as-is rather than threading a new "which leg" parameter through this struct's own
            // constructor (every existing call site, this module's own test suite included):
            // the landing position is byte-for-byte identical either way (`match_at == hi` in
            // both cases) -- only the wrap notice would differ, a cosmetic gap, not a functional
            // one (`search_next_backward_self_hit_at_eof_is_found_directly_not_via_the_wrap`,
            // document.rs, pins the accepted shape).
            if self.hi == cache.size() {
                let ctx = if self.hi == 0 {
                    None
                } else {
                    let idx = (self.hi - 1) / bs;
                    let block = cache.warm(idx).await?;
                    self.warmed = Some((idx, block.clone()));
                    Some(block[((self.hi - 1) % bs) as usize])
                };
                let lb = ctx.is_some() as usize;
                let probe: Vec<u8> = ctx.into_iter().collect();
                if let Some((_s, _e)) = self.pattern.rfind_starting_in(&probe, lb..(lb + 1)) {
                    return Ok(SearchStep::Found {
                        match_at: self.hi,
                        line_start: None,
                    });
                }
            }
        }
        let mut spent = 0usize;
        while self.hi > floor && spent < self.chunk {
            let idx = (self.hi - 1) / bs;
            let block_start = idx * bs;
            // reuse a block the PREVIOUS iteration's own `^` look-behind context already
            // fetched, when it turns out to be THIS iteration's own payload block too --
            // structurally always the case whenever `lo` landed block-aligned last time (see
            // `ctx`'s own derivation, below): the redundant second, PROMOTING `cache.block()`
            // touch this replaces is what actually caused batch 3 (2026-07-23), finding #7 --
            // switching the context fetch alone to `cache.warm()` cannot fix it by itself,
            // since a probationary HIT still promotes under `cache.block()` regardless of
            // which call warmed the entry first; the redundant second call must never happen.
            let block = match self.warmed.take() {
                Some((widx, wb)) if widx == idx => wb,
                _ => cache.block(idx).await?,
            };
            let hi_off = ((self.hi - block_start) as usize).min(block.len());
            // never read below `floor`, even mid-block -- the byte-exact
            // bound forward gets for free by clamping `size` before it ever
            // slices (without this, a match starting below `floor` could
            // still be read and reported, since block reads are otherwise
            // block-granular, not bounded to the caller's exact window).
            let lo = block_start.max(floor);
            let lo_off = (lo - block_start) as usize;
            if lo_off >= hi_off {
                break;
            }
            let slice = &block[lo_off..hi_off];
            // the real byte at `lo - 1`, for `^` look-behind -- fetched fresh every iteration,
            // not chained (this struct's own ANCHOR CORRECTNESS doc comment). Three cases, not
            // two: `lo == 0` needs no context at all (file position 0 is unconditionally a
            // valid line start -- `None` here relies on the regex's own "start of hay" default,
            // correctly, since it IS the true start); `lo == floor > 0` must NOT rely on that
            // same default (it would silently ASSUME a line start this scan is forbidden to
            // verify -- a real regression caught by `search_backward_never_reads_below_its_own_
            // floor_for_context`), so it gets an EXPLICIT non-`\n` sentinel instead, forcing `^`
            // to correctly fail rather than default to true; anything else reads the real byte.
            let ctx = if lo == 0 {
                None
            } else if lo == floor {
                Some(0u8) // never `\n` (0x0A); see this block's own comment just above
            } else {
                let cidx = (lo - 1) / bs;
                let cblock = if cidx == idx {
                    block.clone()
                } else {
                    let warmed = cache.warm(cidx).await?;
                    self.warmed = Some((cidx, warmed.clone()));
                    warmed
                };
                Some(cblock[((lo - 1) - cidx * bs) as usize])
            };
            let lb = ctx.is_some() as usize;
            // search_hay = ctx? (look-behind only, never an acceptable start) + slice (fresh,
            // low) + carry (already-read, high). Vec has no cheap prepend, and the fresh bytes
            // change every iteration while carry is the accumulated state, so it's cheaper to
            // build this fresh here than to keep shuffling a persistent buffer's front open the
            // way SearchForward's append-then-drain does for its own (opposite) direction.
            let mut search_hay = Vec::with_capacity(lb + slice.len() + self.carry.len());
            search_hay.extend(ctx);
            search_hay.extend_from_slice(slice);
            search_hay.extend_from_slice(&self.carry);
            if let Some((s, _e)) = self
                .pattern
                .rfind_starting_in(&search_hay, lb..(lb + slice.len()))
            {
                // `lo - lb + X` maps ANY search_hay index X (ctx at X < lb, real content at
                // X >= lb) to its real absolute file position -- the same shift-cancels-out
                // shape as SearchForward's own `match_at`/`last_nl`, see this struct's own
                // ANCHOR CORRECTNESS doc comment.
                let match_at = lo - lb as u64 + s as u64;
                // bounded: only this slice's own bytes before the match (plus, now, the one
                // real look-behind byte) are consulted; a `\n` further down (not yet read)
                // reports None rather than reading more just to resolve it.
                let line_start = memchr::memrchr(b'\n', &search_hay[..s])
                    .map(|nl| lo - lb as u64 + nl as u64 + 1);
                // only the fresh bytes actually needed to reach the match's
                // start count as consumed this step (ERRATUM 3c#2's rule).
                self.scanned += (slice.len() - (s - lb)) as u64;
                return Ok(SearchStep::Found {
                    match_at,
                    line_start,
                });
            }
            spent += slice.len();
            self.scanned += slice.len() as u64;
            self.hi = lo;
            // shrink to the last MAX_MATCH_LEN-1 bytes for the seam, keeping the LOW end --
            // closest to where the next, lower slice picks up. `ctx` (this iteration's own
            // look-behind byte, if any) is deliberately EXCLUDED: it belongs to territory
            // BELOW the next iteration's own `hi` (`= lo`), not the already-read HIGH carry --
            // the next iteration fetches its own `lo - 1` fresh, per this struct's own ANCHOR
            // CORRECTNESS doc comment, never inherited.
            let mut next_carry = Vec::with_capacity(slice.len() + self.carry.len());
            next_carry.extend_from_slice(slice);
            next_carry.extend_from_slice(&self.carry);
            let keep = (crate::search::MAX_MATCH_LEN - 1).min(next_carry.len());
            next_carry.truncate(keep);
            self.carry = next_carry;
        }
        if self.hi <= floor {
            Ok(SearchStep::End)
        } else {
            Ok(SearchStep::More)
        }
    }
    pub async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<SearchStep> {
        loop {
            match self.step(cache).await? {
                SearchStep::More => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.scanned,
                        span,
                    });
                    tokio::task::yield_now().await;
                }
                terminal => return Ok(terminal),
            }
        }
    }
}
/// Collects bytes from `from` up to and including the `rows`-th newline, EOF,
/// or `budget`; the bool reports whether the scan stopped at EOF or budget.
pub async fn fill_lines(
    cache: &BlockCache,
    from: u64,
    rows: usize,
    budget: usize,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let size = cache.size();
    let bs = cache.block_size() as u64;
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = from;
    let mut newlines = 0usize;
    while newlines < rows && pos < size && buf.len() < budget {
        let block = cache.block(pos / bs).await?;
        let lo = (pos % bs) as usize;
        let slice = &block[lo.min(block.len())..];
        if slice.is_empty() {
            break;
        }
        let mut take = slice.len();
        for (i, &b) in slice.iter().enumerate() {
            if b == b'\n' {
                newlines += 1;
                if newlines == rows {
                    take = i + 1;
                    break;
                }
            }
        }
        buf.extend_from_slice(&slice[..take]);
        pos += take as u64;
    }
    let stopped = pos >= size || buf.len() >= budget;
    Ok((buf, stopped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{SEARCH_WINDOW, SearchPattern};
    use crate::source::MockSource;
    use std::sync::Arc;
    fn cache(data: &'static [u8], block_size: usize) -> BlockCache {
        BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            block_size,
            1 << 20,
        )
    }
    #[tokio::test]
    async fn fill_collects_rows_worth_of_lines() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let (buf, stopped) = fill_lines(&c, 2, 2, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"b\nc\n");
        assert!(!stopped);
    }
    #[tokio::test]
    async fn fill_stops_at_eof_and_reports_it() {
        let c = cache(b"a\nbc", 4);
        let (buf, stopped) = fill_lines(&c, 2, 5, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"bc");
        assert!(stopped);
    }
    #[tokio::test]
    async fn fill_respects_the_budget() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let c = cache(data, 16);
        let (buf, stopped) = fill_lines(&c, 0, 2, 64).await.unwrap();
        assert!(
            buf.len() >= 64 && buf.len() <= 64 + 16,
            "budget overshoot: {}",
            buf.len()
        );
        assert!(stopped);
    }
    #[tokio::test]
    async fn forward_scan_finds_nth_line_start() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let mut s = ForwardScan::new(0, 2, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 4),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_eof_clamps_to_last_real_line_start() {
        // the trailing newline before EOF is not a line start; the clamp is
        // the last real one, delivered as a definitive anchor (no sentinel).
        let c = cache(b"a\nb\nc\n", 4);
        let mut s = ForwardScan::new(0, 99, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Eof(clamp) => assert_eq!(clamp, 4),
            other => panic!("expected Eof, got {other:?}"),
        }
        // pins the fast-Eof branch's accounting, the twin of the Found
        // branch asserted in forward_scan_scanned_counts_the_terminal_block.
        assert_eq!(s.scanned(), 6, "all six bytes were examined");
    }
    #[tokio::test]
    async fn forward_scan_scanned_counts_the_terminal_block() {
        // a terminal step examines part of a block before returning; those
        // bytes are consumed and must be counted (ERRATUM 3c#2).
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 64];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = ForwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 65),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(s.scanned(), 65, "bytes 0..=64 were all examined");
    }
    #[tokio::test]
    async fn forward_scan_eof_clamps_to_origin_when_no_newline_seen() {
        let c = cache(b"abcdef", 4);
        let mut s = ForwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Eof(clamp) => assert_eq!(clamp, 0),
            other => panic!("expected Eof, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_resumes_across_chunks_without_handoff() {
        // 4096 x's then a newline then y: one chunk cannot reach it; stepping
        // the same object to the end must, with no cursor re-derivation.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 4096];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = ForwardScan::new(0, 1, 64);
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                FwdStep::Found(start) => {
                    assert_eq!(start, 4097);
                    assert!(steps >= 2, "must have taken multiple chunks");
                    break;
                }
                FwdStep::Eof(clamp) => panic!("unexpected Eof({clamp})"),
                FwdStep::More => {
                    steps += 1;
                    assert!(
                        s.scanned() >= 64 * steps as u64 - 16,
                        "scanned() tracks chunks"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn forward_scan_zero_n_is_the_origin() {
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(2, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_zero_chunk_still_makes_progress() {
        // the clamp to one byte lives in the constructor now, not at call sites.
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(0, 1, 0);
        loop {
            match s.step(&c).await.unwrap() {
                FwdStep::Found(start) => {
                    assert_eq!(start, 2);
                    break;
                }
                FwdStep::More => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    #[tokio::test]
    async fn forward_scan_complete_resolves_like_stepping() {
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 1024];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let s = ForwardScan::new(0, 1, 64);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 1026,
        });
        let got = s.complete(&c, tx, 1026).await.unwrap();
        assert_eq!(got, 1025);
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[tokio::test]
    async fn forward_scan_out_of_range_origin_stays_in_file() {
        // a computed origin past EOF must terminate AND clamp inside the
        // file, mirroring BackwardScan's philosophy: contract-violating
        // input degrades to the real bytes, never to an out-of-range anchor.
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(64, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Eof(clamp) => assert_eq!(clamp, 4, "clamped to size, not the garbage origin"),
            other => panic!("expected Eof, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_zero_n_with_an_out_of_range_origin_is_clamped() {
        // the n == 0 fast path returns the origin without touching the
        // cache; it must still be bounded — an exact checkpoint hit is a
        // plausible way for computed offsets to construct exactly this.
        let c = cache(b"a\nb\n", 4);
        let mut s = ForwardScan::new(64, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Found(start) => assert_eq!(start, 4, "clamped into the file"),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_finds_nth_newline() {
        let c = cache(b"aaaa\nbbbb\ncccc\n", 4);
        let mut s = BackwardScan::new(10, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
        let mut s = BackwardScan::new(14, 2, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 5),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_top_when_fewer_newlines_exist() {
        let c = cache(b"a\nb\n", 4);
        let mut s = BackwardScan::new(2, 99, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_resume_covers_the_boundary_byte() {
        // the 3b P1 bug class: 4097 bytes through 16-byte blocks with a
        // 64-byte chunk makes the first step end exactly at hi = 4032; the
        // newline at byte 4031 is the boundary byte an entry-semantics
        // re-derivation (`nth_newline_before(4032)` searching [0, 4031))
        // would skip. the exclusion happens once, at construction, so the
        // resumed step must find it.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 4097];
            v[4031] = b'\n';
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = BackwardScan::new(4097, 1, 64);
        match s.step(&c).await.unwrap() {
            BwdStep::More => {}
            other => panic!("expected More on the first chunk, got {other:?}"),
        }
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 4032, "the boundary-byte line start"),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_excludes_the_entry_byte() {
        // from line-start 5, the newline at byte 4 (which made 5 a line
        // start) must not count; the next one up is at byte 1.
        let c = cache(b"a\nbb\ncc\n", 4);
        let mut s = BackwardScan::new(5, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_from_zero_is_top() {
        let c = cache(b"a\nb\n", 4);
        let mut s = BackwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn backward_scan_scanned_counts_the_terminal_block() {
        // same contract as the forward twin: the partially examined terminal
        // block counts toward bytes consumed (ERRATUM 3c#2).
        let data: &'static [u8] = Box::leak({
            let mut v = b"a\n".to_vec();
            v.extend_from_slice(&[b'x'; 62]);
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = BackwardScan::new(64, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Found(start) => assert_eq!(start, 2),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(s.scanned(), 62, "bytes 1..=62 were all examined");
    }
    #[tokio::test]
    async fn backward_scan_complete_resolves_top_as_zero() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 1024].into_boxed_slice());
        let c = cache(data, 16);
        let s = BackwardScan::new(1024, 1, 64);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 1024,
        });
        let got = s.complete(&c, tx, 1024).await.unwrap();
        assert_eq!(got, 0);
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[test]
    fn backward_scan_reports_its_window() {
        // construction needs no cache: the window is pure arithmetic.
        let s = BackwardScan::new(5, 1, 1 << 10);
        assert_eq!(s.remaining_bytes(), 4);
        assert_eq!(BackwardScan::new(0, 1, 1).remaining_bytes(), 0);
    }
    #[tokio::test]
    async fn backward_scan_out_of_range_position_terminates() {
        // hi far past EOF lands on an empty block; without the clamp the
        // empty-slice break never advances hi and step() returns More
        // forever (ERRATUM 3c#3). a single More here IS the hang.
        let c = cache(b"xxxxxxxxxx", 16);
        let mut s = BackwardScan::new(64, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            BwdStep::Top => {}
            other => panic!("expected Top, got {other:?} (no progress on an out-of-range window)"),
        }
    }
    #[tokio::test]
    async fn count_scan_counts_newlines_in_the_window() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let mut s = CountScan::new(0, 8, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(4));
        let mut s = CountScan::new(2, 6, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(2));
    }
    #[tokio::test]
    async fn count_scan_excludes_the_end_byte() {
        // [2, 3): byte 3 is a newline and must not be counted.
        let c = cache(b"a\nb\nc\n", 4);
        let mut s = CountScan::new(2, 3, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(0));
        let mut s = CountScan::new(2, 4, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(1));
    }
    #[tokio::test]
    async fn count_scan_resumes_across_chunks() {
        let data: &'static [u8] = Box::leak(b"x\n".repeat(512).into_boxed_slice());
        let c = cache(data, 16);
        let mut s = CountScan::new(0, 1024, 64);
        let mut steps = 0usize;
        let total = loop {
            match s.step(&c).await.unwrap() {
                CountStep::Done(n) => break n,
                CountStep::More => steps += 1,
            }
        };
        assert_eq!(total, 512);
        assert!(steps >= 2, "must have taken multiple chunks");
    }
    #[tokio::test]
    async fn count_scan_empty_and_inverted_windows_are_zero() {
        let c = cache(b"a\nb\n", 4);
        let mut s = CountScan::new(2, 2, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(0));
        let mut s = CountScan::new(3, 1, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(0));
    }
    #[tokio::test]
    async fn count_scan_out_of_range_window_clamps_into_the_file() {
        let c = cache(b"a\nb\n", 4);
        let mut s = CountScan::new(0, 999, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(2));
        let mut s = CountScan::new(700, 999, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(0));
    }
    #[tokio::test]
    async fn count_scan_scanned_counts_the_terminal_block() {
        // same contract as the forward/backward twins: a terminal slice that
        // ends mid-block counts only the window's bytes toward `scanned`,
        // not the rest of the block sitting in hand (ERRATUM 3c#2's rule,
        // applied to a window boundary instead of a found newline).
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 64];
            v.extend_from_slice(b"\ny");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let mut s = CountScan::new(0, 65, 1 << 10);
        assert_eq!(s.step(&c).await.unwrap(), CountStep::Done(1));
        assert_eq!(
            s.scanned(),
            65,
            "bytes 0..65 were examined, not byte 65 itself"
        );
    }
    #[tokio::test]
    async fn search_forward_finds_the_first_match_after_the_origin() {
        // "needle" at [4, 10) and [19, 25); a `\n` at 14 sits between the
        // origin and the second match, so line_start must reflect it.
        let c = cache(b"aaa needle bbb\nccc needle ddd\n", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 10, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found {
                match_at,
                line_start,
            } => {
                assert_eq!(match_at, 19, "the first match ends at 10; must skip it");
                assert_eq!(line_start, Some(15), "the \\n at 14 makes 15 a line start");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_reports_end_at_eof_without_a_match() {
        let c = cache(b"aaa bbb ccc", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_zero_width_dollar_at_unterminated_eof() {
        // batch 3 (2026-07-23), finding #9 (probe-verified): "$" over unterminated "abc" has no
        // `\n` anywhere, so its only match is the zero-width position 3 (the file's own true
        // end) -- pre-fix the accept range excluded a start == hay's own end everywhere, so this
        // scan reported `End` even though `find_all` (the highlighter) sees `(3, 3)`.
        let c = cache(b"abc", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 3),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_caret_dollar_on_an_empty_file() {
        // the degenerate empty-file case: position 0 is simultaneously the file's own true
        // start (`^`) and true end (`$`) -- `^$` must match there, zero-width.
        let c = cache(b"", 8);
        let p = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 0),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_out_of_range_origin_clamps_instead_of_panicking() {
        // batch 3 (2026-07-23), finding #6 (probe-verified): `from` far past EOF used to panic
        // indexing the ctx seed's own block (fetched for a block index nowhere near this file)
        // before `pos` was ever clamped -- the file-wide "out-of-contract positions degrade to
        // the real bytes" philosophy every other scan object already follows.
        let c = cache(b"aaa bbb ccc", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, u64::MAX, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End (clamped into the file), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_match_straddling_a_block_boundary() {
        // the plant sits past a full SEARCH_WINDOW of filler (stresses the
        // scan at production scale) and is not block-aligned, so "needle"
        // straddles an 8-byte block boundary; a small chunk forces several
        // step() calls to reach it.
        let plant = SEARCH_WINDOW + 5;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + 32];
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 4096);
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    assert_eq!(match_at, plant as u64);
                    assert!(steps >= 2, "must have taken multiple chunks");
                    break;
                }
                SearchStep::End => panic!("expected to find the planted needle"),
                SearchStep::More => steps += 1,
            }
        }
    }
    #[tokio::test]
    async fn search_forward_returns_the_leftmost_match_not_the_first_complete_one() {
        // batch 3 (2026-07-23), finding #2 (probe-verified): a whole-hay accept used to return
        // the FIRST COMPLETE match in the growing hay, not the regex's own leftmost -- "b" (1
        // byte) completes the moment the first read reaches it, long before "a.{10}z" (needing
        // the FULL 12 bytes) ever can, even though the pattern's true leftmost match starts at
        // 0. Pre-fix this returned 6; post-fix (the `safe_to` gate) it must return 0.
        let c = cache(b"a12345b1234z", 8);
        let p = Arc::new(SearchPattern::compile("a.{10}z|b", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => {
                assert_eq!(match_at, 0, "the true leftmost match starts at 0, not 6")
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_leftmost_match_exactly_at_the_cap() {
        // the boundary `safe_to` itself must get right: the leftmost alternative's own span is
        // EXACTLY MAX_MATCH_LEN bytes -- the last position at which it is still guaranteed
        // findable, not yet the ">cap" residual `search_forward_returns_a_later_match_when_the_
        // leftmost_exceeds_the_cap` (below) pins. A shorter "b" alternative, planted well inside
        // the longer one's own span (mirroring the probe fixture's own shape), must not win.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'y'; max + 200];
        data[0] = b'a';
        data[max - 1] = b'z'; // 1 (a) + (max - 2) any + 1 (z) == max bytes, spanning [0, max)
        data[10] = b'b';
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64);
        let pattern = format!("a.{{{}}}z|b", max - 2);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => {
                assert_eq!(
                    match_at, 0,
                    "the maximal-length leftmost match must still be found"
                )
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_returns_a_later_match_when_the_leftmost_exceeds_the_cap() {
        // the documented residual `safe_to` does NOT paper over: a leftmost alternative ONE
        // byte past MAX_MATCH_LEN can never complete within any hay this scan ever holds (the
        // carry itself caps at MAX_MATCH_LEN - 1 bytes), so it silently drops out of
        // consideration once the carry shrinks past its own start -- a later, shorter,
        // genuinely-findable match must still be returned, not a hang and not a false miss.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'y'; max + 200];
        data[0] = b'a';
        data[max] = b'z'; // 1 (a) + (max - 1) any + 1 (z) == max + 1 bytes -- one over the cap
        data[max + 50] = b'b'; // the legitimately findable later match
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64);
        let pattern = format!("a.{{{}}}z|b", max - 1);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(
                match_at,
                (max + 50) as u64,
                "the over-cap leftmost match is unreachable; the later match is the honest answer"
            ),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_can_still_false_positive_at_exactly_max_match_len() {
        // review response (batch 3 (2026-07-23)): `safe_to` closes the mid-file `pat$` chunk-
        // boundary false positive for any match SHORTER than `MAX_MATCH_LEN` (proven, not just
        // asserted: accepting requires `read_end - match_at >= MAX_MATCH_LEN`, and for L <
        // MAX_MATCH_LEN that forces at least one byte of REAL slack past the match's own end,
        // so `$` can only be confirmed by genuine content there, never hay's own artificial
        // edge). A match of EXACTLY `MAX_MATCH_LEN` bytes is the one length where that slack can
        // be zero: `safe_to`'s own margin is calibrated to precisely this length, with nothing
        // left over, so this scan can still accept a `$` confirmed only by hay's own CURRENT
        // end -- mid-file, on an entirely unbounded leg, no wrap seam involved. Pinned as a real,
        // narrow, accepted residual (docs/search.md's own "Anchored patterns" section), not a
        // hang and not silently swept under the ">cap" residual above (this one is AT the cap,
        // not over it).
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'y'; max + 200];
        data[0] = b'a';
        data[max - 1] = b'z'; // 1 (a) + (max - 2) any + 1 (z) == max bytes exactly
        data[max] = b'Q'; // real byte right after the match -- NOT `\n`; disproves `$` if read
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64); // block_size 64: max (4096) is an exact multiple, landing the
        // read boundary exactly on the match's own end -- the worst-case alignment this
        // residual needs to manifest at all.
        let pattern = format!("a.{{{}}}z$", max - 2);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(
                match_at, 0,
                "the accepted residual: $ confirmed only by hay's own current (artificial) edge"
            ),
            other => panic!("expected Found (the residual), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_can_false_positive_at_a_bounded_legs_own_limit() {
        // the OTHER accepted residual, wider but confined to a bounded leg's own seam: at its
        // own `limit` (never the true file end -- a wrap leg's own artificial bound), `safe_to`
        // gives NO margin at all (`safe_to == read_end`, not `read_end - MAX_MATCH_LEN + 1`), so
        // a match of ANY length ending exactly there can falsely confirm `$` against hay's own
        // edge -- unlike the exactly-MAX_MATCH_LEN case above, this one needs no special length
        // at all, only the leg's own limit landing exactly at the match's own end.
        let mut data = vec![b'y'; 200];
        data[0..3].copy_from_slice(b"abc");
        data[3] = b'Q'; // real byte right after the match -- NOT `\n`; disproves `$` if read
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("abc$", false).unwrap());
        let mut s = SearchForward::new(p, 0, 3, 1 << 20); // limit=3: bounded exactly at "abc"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(
                match_at, 0,
                "the accepted residual: $ confirmed only by a bounded leg's own artificial limit"
            ),
            other => panic!("expected Found (the residual), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_line_start_is_none_when_no_newline_precedes_the_match_in_scan_range() {
        // the only `\n` (at byte 2) is TWO bytes before the origin (4, at origin - 2); `ctx`
        // (this struct's own ANCHOR CORRECTNESS mechanism) only ever looks back exactly ONE
        // byte (origin - 1, here 'x', not '\n'), so this one genuinely stays out of the scan's
        // view -- the caller must still resolve it itself. The immediately-adjacent case (`\n`
        // AT origin - 1) is pinned by `search_forward_line_start_is_resolved_from_the_one_real_
        // look_behind_byte`, just below: unlike this one, it is now visible.
        let c = cache(b"zz\nxneedle xyz", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 4, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found {
                match_at,
                line_start,
            } => {
                assert_eq!(match_at, 4);
                assert_eq!(line_start, None);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_line_start_is_resolved_from_the_one_real_look_behind_byte() {
        // the only `\n` is exactly ONE byte before the origin (4, at origin - 1) -- `ctx`
        // (read lazily for `^` anchor correctness, this struct's own ANCHOR CORRECTNESS doc
        // comment) reads exactly this byte regardless of the pattern, so `line_start` is now
        // resolved directly instead of falling back to `None` -- a side benefit of the same
        // mechanism, not a separate feature: `line_start_shortcut` (document.rs) already treats
        // a `Some` hint and a `None`-then-self-resolve as equally correct, just cheaper here.
        let c = cache(b"zzz\nneedle xyz", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 4, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found {
                match_at,
                line_start,
            } => {
                assert_eq!(match_at, 4);
                assert_eq!(line_start, Some(4));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_line_start_is_none_when_a_persisted_newline_falls_inside_the_match() {
        // batch 3 (2026-07-23), finding #4 (probe-verified): a pattern containing an explicit
        // `\n` ("foo\nbar", non-ordinary -- see this struct's own doc comment) can straddle a
        // line; chunked reads (block_size 1, chunk 1) force the scan to record the `\n` at
        // absolute position 5 as `last_nl` on an EARLIER iteration, before it can know that same
        // newline will end up INSIDE the eventual match at [2, 9) rather than before it. Pre-fix
        // this returned `Some(6)` -- past `match_at` (2), which would top the viewport PAST the
        // match's own start; post-fix it must be `None`, deferring to the caller's own hunt.
        let c = cache(b"xxfoo\nbarzz", 1);
        let p = Arc::new(SearchPattern::compile("foo\nbar", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found {
                    match_at,
                    line_start,
                } => {
                    assert_eq!(match_at, 2);
                    assert_eq!(
                        line_start, None,
                        "a persisted newline INSIDE the match is not a valid line start"
                    );
                    return;
                }
                SearchStep::End => panic!("expected to find foo\\nbar"),
                SearchStep::More => {}
            }
        }
    }
    #[tokio::test]
    async fn search_scans_return_more_when_the_chunk_budget_runs_out_midway() {
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 200];
            v.extend_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 16);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 32);
        match s.step(&c).await.unwrap() {
            SearchStep::More => {}
            other => panic!("expected More, got {other:?}"),
        }
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    assert_eq!(match_at, 200);
                    break;
                }
                SearchStep::More => {}
                SearchStep::End => panic!("expected to find the needle"),
            }
        }
    }
    #[tokio::test]
    async fn search_forward_scanned_counts_only_the_bytes_needed_for_the_match() {
        // the match resolves 6 bytes into a much larger take; scanned must
        // reflect only the bytes needed to confirm it (ERRATUM 3c#2's
        // rule), not the whole block that happened to be read.
        let data: &'static [u8] = Box::leak({
            let mut v = b"needle".to_vec();
            v.extend_from_slice(&[b'x'; 100]);
            v.into_boxed_slice()
        });
        let c = cache(data, 1 << 10);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 0),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            s.scanned(),
            6,
            "only the matched bytes were needed, not the rest of the block"
        );
    }
    #[tokio::test]
    async fn search_forward_limit_bounds_the_scan_before_a_later_match() {
        // limit stops the scan short of a match further in the file -- the
        // bound a wraparound leg needs to avoid re-finding its own start.
        let c = cache(b"aaa needle bbb", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, 4, 1 << 10); // limit=4, needle starts at 4
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End (bounded before the match), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_complete_resolves_like_stepping() {
        let plant = SEARCH_WINDOW + 5;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + 32];
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let s = SearchForward::new(p, 0, u64::MAX, 4096);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: data.len() as u64,
        });
        match s.complete(&c, tx, data.len() as u64).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, plant as u64),
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[tokio::test]
    async fn search_backward_finds_the_last_match_below_the_origin() {
        // "needle" at [4,10) and [19,25); origin above both must land on
        // the closer one (19), matching forward's "skip the earlier one"
        // twin. The `\n` at 14 sits in a different 8-byte block than the
        // match, so this also pins the "bounded, not carried" line_start.
        let c = cache(b"aaa needle bbb\nccc needle ddd\n", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 30, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found {
                match_at,
                line_start,
            } => {
                assert_eq!(match_at, 19, "must land on the closer match, not 4");
                assert_eq!(
                    line_start, None,
                    "the \\n at 14 is outside this match's own block"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_line_start_found_within_the_same_slice() {
        // the `\n` and the match now share one block: the happy-path twin
        // of the bounded/None case above.
        let c = cache(b"xxx\nneedle", 1 << 10);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 10, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found {
                match_at,
                line_start,
            } => {
                assert_eq!(match_at, 4);
                assert_eq!(line_start, Some(4), "the \\n at 3 makes 4 a line start");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_reports_end_at_top() {
        let c = cache(b"aaa bbb ccc", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 11, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_zero_width_dollar_from_the_true_eof() {
        // batch 3 (2026-07-23), finding #9's backward twin: the far-end leg (`hi = size`) must
        // find the same zero-width `$` this file's own unterminated end offers -- `hi` excludes
        // itself everywhere else (self-hit avoidance), but at the TRUE file end there is no
        // "later" leg for a self-hit to matter against.
        let c = cache(b"abc", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new(p, 3, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 3),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_caret_dollar_on_an_empty_file() {
        let c = cache(b"", 8);
        let p = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let mut s = SearchBackward::new(p, 0, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 0),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_does_not_widen_a_bounded_legs_own_limit() {
        // NOT the true EOF: `hi` here is a wrap leg's own bounded ceiling (mirrors
        // `search_backward_limit_bounds_the_scan_below_an_earlier_match`'s own shape) -- the
        // zero-width widening must apply ONLY when `hi == cache.size()`, never to an arbitrary
        // bounded `hi`, or it would silently admit starts the caller explicitly bounded away.
        let c = cache(b"abc more tail bytes here", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new(p, 3, 0, 1 << 10); // hi=3, but the real file is longer
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End (hi=3 is not the true EOF here), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_seam_match_found_once() {
        // "needle" straddles the boundary between blocks 1 and 2 (8-byte
        // blocks); a tiny chunk forces the walk through several steps, and
        // it must be found exactly once, whole, not skipped and not
        // reported twice from two different windows.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 40];
            v[13..19].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 40, 0, 8);
        let mut found = 0usize;
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    found += 1;
                    assert_eq!(match_at, 13);
                    break;
                }
                SearchStep::End => panic!("expected to find the planted needle"),
                SearchStep::More => steps += 1,
            }
        }
        assert_eq!(found, 1, "reported exactly once");
        assert!(steps >= 1, "must have taken multiple chunks");
    }
    #[tokio::test]
    async fn search_backward_scanned_counts_only_the_bytes_needed_for_the_match() {
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 100];
            v.extend_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 1 << 10);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, data.len() as u64, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 100),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            s.scanned(),
            6,
            "only the matched bytes were needed, not the rest of the block"
        );
    }
    #[tokio::test]
    async fn search_backward_limit_bounds_the_scan_below_an_earlier_match() {
        // needle starts at 4; a floor of 5 excludes it byte-exactly, even
        // though it shares an 8-byte block with in-bounds bytes.
        let c = cache(b"aaa needle bbb", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 14, 5, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("expected End (bounded above the match), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_straddler_that_extends_past_hi() {
        // batch 3 (2026-07-23), finding #3 (probe-verified): "aa" matches at starts 0,1,2,3 in
        // "aaaaa"; hi=2 excludes starts >= 2, leaving 0 and 1 as candidates -- the CLOSEST is 1
        // (needing byte 2, which sits AT hi, to complete). `hi` used to bound reads and accepts
        // TOGETHER, so this scan could never even READ byte 2, let alone confirm the closer
        // match -- it fell back to 0. Post-fix (the straddle-read seed) it must return 1.
        let c = cache(b"aaaaa", 8);
        let p = Arc::new(SearchPattern::compile("aa", false).unwrap());
        let mut s = SearchBackward::new(p, 2, 0, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => {
                assert_eq!(
                    match_at, 1,
                    "must find the closer straddling match, not fall back to 0"
                )
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_straddler_planted_at_the_floor_edge() {
        // the floor-edge twin: the straddling match's own start sits EXACTLY at `limit` (the
        // inclusive lower bound), stressing the seed against the OTHER boundary in the same
        // scan at once -- "needle" spans [8, 14), hi=10 (so leg 1 can only ever see "ne" without
        // the seed), limit=8 (so byte 8 -- "needle"'s own start -- is in-bounds by the
        // INCLUSIVE-limit contract, `SearchBackward::new`'s own doc comment).
        let mut data = vec![b'z'; 8];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'z'; 4]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 10, 8, 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => {
                assert_eq!(match_at, 8, "the floor-edge straddler must still be found")
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // ---- ANCHOR CORRECTNESS regressions (found during this review, before `ctx` existed in
    // either scan): a slice's own edge is unconditionally `^`-eligible (`search::SearchPattern
    // ::compile`'s own module doc comment), regardless of the real preceding byte -- reachable
    // at EVERY carry-shrink seam for forward, not just its own origin, and at every fresh `lo`
    // for backward. ----

    #[tokio::test]
    async fn search_forward_does_not_fake_a_line_start_at_a_mid_line_origin() {
        // the ORIGINAL, narrowest form of this class (this struct's own ANCHOR CORRECTNESS doc
        // comment): the scan's own origin sits mid-line (not preceded by a real \n), with
        // "needle" planted RIGHT AT the origin itself -- the very first position `step` ever
        // tests. `^needle` must not match here just because it happens to be hay-index 0 of the
        // scan's own very first read.
        let data: &'static [u8] = b"xxxxneedle";
        let origin = 4u64; // "needle" starts here; byte 3 ('x') is NOT a newline
        let c = cache(data, 3);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchForward::new(p, origin, u64::MAX, 32);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => {
                panic!("falsely found ^needle at {match_at}: origin ({origin}) is mid-line")
            }
            SearchStep::End => {}
            SearchStep::More => panic!("fixture is small enough to resolve in one step"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_real_line_anchored_match_at_the_origin() {
        // the positive twin: origin sits RIGHT AFTER a real \n -- `^needle` must find it,
        // proving the fix does not just conservatively reject everything at the origin.
        let data: &'static [u8] = b"xxx\nneedle";
        let origin = 4u64; // right after the \n at byte 3
        let c = cache(data, 3);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchForward::new(p, origin, u64::MAX, 32);
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, origin),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_does_not_fake_a_line_start_at_a_carry_shrink_seam() {
        // needle sits far past the origin, long enough after it that a small block size forces
        // several carry-shrink seams before reaching it -- no \n anywhere in the buffer, so
        // `^needle` must not match.
        let tail = crate::search::MAX_MATCH_LEN + 200;
        let plant = 5000usize;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + tail];
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    panic!("falsely found ^needle at {match_at}: no real newline precedes it")
                }
                SearchStep::End => break,
                SearchStep::More => {}
            }
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_real_line_anchored_match_past_a_carry_shrink_seam() {
        // the positive twin of the regression above: a REAL \n precedes "needle", still past
        // several carry-shrink seams -- `^needle` must still find it.
        let tail = crate::search::MAX_MATCH_LEN + 200;
        let plant = 5000usize;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + tail];
            v[plant - 1] = b'\n';
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, u64::MAX, 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    assert_eq!(match_at, plant as u64);
                    return;
                }
                SearchStep::End => panic!("expected to find the line-anchored needle"),
                SearchStep::More => {}
            }
        }
    }
    #[tokio::test]
    async fn search_backward_does_not_fake_a_line_start_at_a_fresh_low_edge() {
        // needle sits well below `hi`, with no \n anywhere -- a tiny chunk/block size forces
        // several distinct `lo` edges before reaching it; none of them is a real line start.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 400];
            v[97..103].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchBackward::new(p, 400, 0, 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    panic!("falsely found ^needle at {match_at}: no real newline precedes it")
                }
                SearchStep::End => break,
                SearchStep::More => {}
            }
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_real_line_anchored_match_at_a_fresh_low_edge() {
        // the positive twin: a REAL \n immediately precedes "needle" -- `^needle` must find it.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 400];
            v[96] = b'\n';
            v[97..103].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchBackward::new(p, 400, 0, 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Found { match_at, .. } => {
                    assert_eq!(match_at, 97);
                    return;
                }
                SearchStep::End => panic!("expected to find the line-anchored needle"),
                SearchStep::More => {}
            }
        }
    }
    #[tokio::test]
    async fn search_backward_dollar_anchor_confirms_against_a_real_byte_straddling_hi() {
        // batch 3 (2026-07-23), finding #3's own side effect: the straddle-read seed hands the
        // FIRST iteration real lookahead bytes AT-OR-ABOVE `hi`, not just an artificial hay-edge
        // there -- so "needle$" (needing to see the REAL byte right after "needle" to confirm)
        // must correctly NOT match when that byte is neither `\n` nor true EOF, even though
        // "needle" itself sits entirely below `hi` and the scan's own hay would otherwise end
        // exactly at the match's own tail with nothing (yet) proving the `$` false.
        let data = b"xneedleZ"; // 'Z' (not \n) immediately follows "needle"; more real data past it
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle$", false).unwrap());
        let mut s = SearchBackward::new(p, 7, 0, 8); // hi = 7, right at "needle"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => panic!("falsely matched needle$ at an artificial hay edge, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_dollar_anchor_finds_a_real_newline_straddling_hi() {
        // the positive twin: a REAL `\n` sits right where "needle" ends -- `needle$` must still
        // be found, proving the fix does not just conservatively reject everything at `hi`.
        let data = b"xneedle\nzzz";
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle$", false).unwrap());
        let mut s = SearchBackward::new(p, 7, 0, 8); // hi = 7, right at "needle"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 1),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_never_reads_below_its_own_floor_for_context() {
        // a real \n sits ONE byte below `limit` -- reading it for ^ context would violate this
        // module's own "never read past the point it was told to stop" rule (this file's own
        // header doc comment), so `^needle` must NOT match here even though, byte-for-byte,
        // "needle" genuinely is preceded by a \n -- the honest, narrow miss this struct's own
        // ANCHOR CORRECTNESS doc comment documents. Not a crash, not a false positive: a
        // deliberately conservative miss at the scan's own bound.
        let data = b"z\nneedle";
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchBackward::new(p, 8, 2, 8); // limit=2: "needle" (2..8) is in-bounds, the \n (1) is not
        match s.step(&c).await.unwrap() {
            SearchStep::End => {}
            other => {
                panic!("expected End (the \\n sits below this scan's own floor), got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn search_backward_context_peeks_do_not_promote_untouched_blocks() {
        // batch 3 (2026-07-23), finding #7: a no-match walk across 8 blocks used to leave 7 of
        // them PROTECTED -- the per-iteration `^` look-behind peek warmed the NEXT iteration's
        // own payload block one step early, and `cache.block()`'s own "promotes on a
        // probationary hit" rule then promoted it on that later touch regardless of which call
        // warmed it first (switching the peek alone to `cache.warm()` does not change this: the
        // redundant second cache call itself is the hazard, not which function it names -- see
        // `SearchBackward`'s own `warmed` field doc comment). A one-pass scan like this one must
        // behave like the one-pass line-index scan it is (docs/block_cache.md's own "lives and
        // dies in probation"), not manufacture protected entries out of its own bookkeeping.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64].into_boxed_slice());
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 64, 0, 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::End => break,
                SearchStep::Found { .. } => panic!("no match planted in this fixture"),
                SearchStep::More => {}
            }
        }
        assert_eq!(
            c.protected_len(),
            0,
            "a one-pass no-match scan must not protect anything: {:?}",
            c.protected_keys()
        );
    }
    #[tokio::test]
    async fn search_backward_straddle_seed_does_not_promote_a_mid_block_his_own_block() {
        // review-caught re-opening of finding #7 (batch 3 (2026-07-23)) by finding #3's own
        // straddle-read seed: when `hi` is MID-BLOCK, the seed's own FIRST block (`hi / bs`) is
        // the identical block as the main loop's own first payload block (`(hi - 1) / bs`) --
        // without reusing it (this struct's own `warmed` field, shared with the per-iteration
        // `ctx` fetch), the seed's `warm()` fill left a probation entry for the main loop's own
        // `cache.block()` payload touch to promote a moment later, the exact hazard finding #7
        // already closed for the per-iteration case. `hi = 5` here: seed block 0 (5/8), payload
        // block 0 ((5-1)/8) -- same block.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64].into_boxed_slice());
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 5, 0, 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::End => break,
                SearchStep::Found { .. } => panic!("no match planted in this fixture"),
                SearchStep::More => {}
            }
        }
        assert_eq!(
            c.protected_len(),
            0,
            "a one-pass no-match scan must not protect anything: {:?}",
            c.protected_keys()
        );
    }
    #[tokio::test]
    async fn search_backward_straddle_seed_does_not_promote_a_mid_block_his_own_block_hi13() {
        // the same regression as the `hi = 5` twin above, at a DIFFERENT mid-block `hi` (13,
        // landing on block 1 rather than block 0) -- pins that the fix generalizes, not just an
        // off-by-one that happens to work for one specific block.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64].into_boxed_slice());
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new(p, 13, 0, 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::End => break,
                SearchStep::Found { .. } => panic!("no match planted in this fixture"),
                SearchStep::More => {}
            }
        }
        assert_eq!(
            c.protected_len(),
            0,
            "a one-pass no-match scan must not protect anything: {:?}",
            c.protected_keys()
        );
    }
    #[tokio::test]
    async fn search_forward_context_seed_does_not_promote_its_own_first_payload_block() {
        // the forward twin of the backward regression above (batch 3 (2026-07-23), finding #7,
        // the audit's own "the forward origin-seed too"): a MID-BLOCK origin makes the one-time
        // `^` context seed land on the identical block as this scan's own very first payload
        // fetch, the same redundant-second-touch shape `SearchBackward`'s own `warmed` field
        // fixes -- must not promote it either.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64].into_boxed_slice());
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 4, u64::MAX, 1 << 10); // mid-block origin (block_size 8)
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::End => break,
                SearchStep::Found { .. } => panic!("no match planted in this fixture"),
                SearchStep::More => {}
            }
        }
        assert_eq!(
            c.protected_len(),
            0,
            "the context seed must not promote its own first payload block: {:?}",
            c.protected_keys()
        );
    }
    #[tokio::test]
    async fn search_backward_complete_resolves_like_stepping() {
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 40];
            v[13..19].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let s = SearchBackward::new(p, 40, 0, 8);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 40,
        });
        match s.complete(&c, tx, 40).await.unwrap() {
            SearchStep::Found { match_at, .. } => assert_eq!(match_at, 13),
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use crate::search::SearchPattern;
    use crate::source::MockSource;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }
    proptest! {
        #[test]
        fn forward_stepped_chunks_agree_with_one_giant_step(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..128,
            ),
            block_size in 1usize..32,
            chunk in 1usize..24,
            from in 0u64..128,
            n in 0usize..6,
        ) {
            let from = from.min(data.len() as u64);
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = ForwardScan::new(from, n, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = ForwardScan::new(from, n, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        FwdStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked scan diverged from unbudgeted scan");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn backward_stepped_chunks_agree_with_one_giant_step(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..128,
            ),
            block_size in 1usize..32,
            chunk in 1usize..24,
            pos in 0u64..128,
            n in 1usize..6,
        ) {
            let pos = pos.min(data.len() as u64);
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = BackwardScan::new(pos, n, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = BackwardScan::new(pos, n, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        BwdStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked scan diverged from unbudgeted scan");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn count_stepped_chunks_agree_with_one_giant_step(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..128,
            ),
            block_size in 1usize..32,
            chunk in 1usize..24,
            from in 0u64..128,
            to in 0u64..128,
        ) {
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = CountScan::new(from, to, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = CountScan::new(from, to, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        CountStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked count diverged from unbudgeted count");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn search_forward_stepped_chunks_agree_with_one_giant_step(
            hay in proptest::collection::vec(
                prop_oneof![
                    30 => any::<u8>().prop_map(|b| vec![b]),
                    1 => (1usize..80).prop_map(|n| {
                        let mut v = vec![b'a'];
                        v.resize(v.len() + n, b'b');
                        v.push(b'c');
                        v
                    }),
                ],
                0..300,
            ).prop_map(|chunks| chunks.into_iter().flatten().collect::<Vec<u8>>()),
            block_size in 1usize..32,
            chunk in 1usize..24,
            from in 0u64..300,
        ) {
            let from = from.min(hay.len() as u64);
            // "ab+c" mirrors search.rs's own oracle strategy: b-runs stay
            // well under MAX_MATCH_LEN (<=80), so every real match is
            // within the documented cap and the two scans must agree.
            let pattern = std::sync::Arc::new(SearchPattern::compile("ab+c", false).unwrap());
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(hay.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = SearchForward::new(pattern.clone(), from, u64::MAX, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = SearchForward::new(pattern, from, u64::MAX, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        SearchStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked search diverged from unbudgeted search");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn search_backward_stepped_chunks_agree_with_one_giant_step(
            hay in proptest::collection::vec(
                prop_oneof![
                    30 => any::<u8>().prop_map(|b| vec![b]),
                    1 => (1usize..80).prop_map(|n| {
                        let mut v = vec![b'a'];
                        v.resize(v.len() + n, b'b');
                        v.push(b'c');
                        v
                    }),
                ],
                0..300,
            ).prop_map(|chunks| chunks.into_iter().flatten().collect::<Vec<u8>>()),
            block_size in 1usize..32,
            chunk in 1usize..24,
            hi in 0u64..300,
        ) {
            let hi = hi.min(hay.len() as u64);
            let pattern = std::sync::Arc::new(SearchPattern::compile("ab+c", false).unwrap());
            let c = crate::cache::BlockCache::new(
                std::sync::Arc::new(MockSource::new(hay.clone())),
                block_size,
                1 << 20,
            );
            rt().block_on(async {
                let mut big = SearchBackward::new(pattern.clone(), hi, 0, usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = SearchBackward::new(pattern, hi, 0, chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        SearchStep::More => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked search diverged from unbudgeted search");
                Ok::<(), TestCaseError>(())
            })?;
        }
    }
}
