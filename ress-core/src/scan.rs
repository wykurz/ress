//! Budgeted newline scanning over the block cache. `ForwardScan`,
//! `BackwardScan`, `SearchForward`, and `SearchBackward` are the engine's
//! navigation read loops: every scan states a byte budget per step, its
//! window and origin are captured once at construction, and the cursor
//! lives inside the object — a pending continuation is the same value as
//! the interactive attempt. `fill_lines` is the viewport's single-call
//! read. Budgets bound **delivered work plus skipped spans**, not physical
//! I/O: what a step charges is what `classify` hands back through the
//! `Reader` (hit or miss alike — a prewarmed step charges exactly what a
//! cold one would, and can legitimately report `More` having performed no
//! new reads at all) plus whatever a backward descent charges via
//! `Reader::skip` for territory it passes over without reading. A result
//! found in a block already in hand is returned even past the nominal byte
//! budget: an answer from paid-for bytes always beats a clamp.
//!
//! **Physical I/O is bounded by the budget only loosely, and this module
//! deliberately does not promise otherwise** (batch 7 (2026-07-28), finding
//! #4 — this paragraph used to claim "total I/O stays within one block of
//! the budget", which the crate's own tests disprove): a bookkeeping
//! look-around gather costs one `BlockCache` touch per block it crosses,
//! each unconditional, so at a pathological `block_size` a budget of 1 can
//! still perform `CTX_BEHIND` physical reads before the first payload read
//! is even attempted (`search_forward_ctx_seed_reads_charge_the_budget_the_
//! driver_reads`, this module's own test, pins exactly that). The honest
//! bound, and the one `docs/budgeted_scanning.md` derives in full, is a
//! small fixed number of blocks — dominated by the block term at any
//! realistic `block_size`, by the bookkeeping term at a tiny one. The two
//! search scans are the
//! exception to "block granularity": their `limit` bound (an upper bound
//! forward, a lower bound backward) IS respected byte-exactly, the same way
//! `CountScan`'s window is.
//!
//! **`limit` bounds what may be REPORTED, not what may be READ** (batch 8
//! (2026-07-29), P2). This used to end "a wraparound leg must never read, let
//! alone report, past the point it was told to stop" — contradicted, by
//! design, a few thousand lines down in this same file: `SearchBackward`'s own
//! per-iteration look-behind gather deliberately reads up to `CTX_BEHIND` real
//! bytes BELOW `floor`, and its own comment there argues at length why that is
//! correct (the bytes below a declared boundary are perfectly real and
//! readable; `floor` declares what may be ACCEPTED, never what exists, and a
//! `^`/`\b` decided against fabricated context is the worse failure). The two
//! statements cannot both stand, and it is the reading half that has to go:
//! nothing below `floor` is ever reportable or promotes the working set past
//! it, which is the property the byte-exact bound actually protects.
//! `search_backward_reads_a_real_byte_below_its_own_floor_for_context` pins
//! the reading half directly — its own name is the correction, having replaced
//! an earlier test that asserted the opposite.
use crate::cache::BlockCache;
/// A resumable unit's outcome: either it finished (`Done`), or it consumed its budget and must be
/// called again -- carrying BOTH witnesses a caller needs to trust "call me again" honestly
/// (restructure R6, `structural-scan.md` §3.1-3.2): `Progressed` (`crate::progress`) proves the
/// scan's own resumption cursor moved; `Charged` (`crate::meter`) proves at least one read
/// happened since the step began. The two are never fused into one type or connected by a `From`
/// (`structural-budget.md` §3.5's own seam ruling): a backward descent past unreadable territory
/// charges without reading and moves (`Charged` via `skip`, `Progressed` via the cursor); a
/// forward search's deferred candidate reads and charges while the reported ANSWER does not move,
/// but the scan's own resumption cursor does (`crate::progress`'s own module doc comment has the
/// full derivation of why `Progressed` tracks the cursor, never the answer).
///
/// `BwdStep` (below) does NOT alias this: `SearchBackward`'s composed-meter `Exhausted` case has
/// NEITHER witness (the step began already out of budget, attempted nothing) -- a state `Step<T>`
/// deliberately cannot express, so `BwdStep` stays its own enum rather than force a variant onto
/// every other consumer of this vocabulary that only `BackwardScan`'s one composed caller needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step<T> {
    Done(T),
    More(crate::progress::Progressed, crate::meter::Charged),
}
/// A resumable, budgeted unit of background work: `step` advances by one chunk; the blanket
/// `complete()` drives it to a terminal outcome, publishing progress and yielding between chunks
/// so an abort has a guaranteed point to take effect on a hot cache (restructure R6,
/// `structural-scan.md` §3.7) -- one implementation instead of the 4 near-identical driver loops
/// this crate used to hand-write. `ForwardScan`, `SearchForward`, and `SearchBackward` implement
/// it, each delegating `step`/`progressed` straight to their own identically-named inherent
/// methods (Rust's own method resolution always prefers an inherent method over a trait one at
/// the call site, so this is not recursion) and `lift_allowance` to `Meter::lift_allowance`.
/// `BackwardScan` does NOT: `BwdStep`'s own doc comment states why (`Exhausted` has neither
/// witness `Step::More` requires -- a state `Step<T>` cannot express, so forcing `BackwardScan`
/// into this trait's shared vocabulary would mean adding a variant only it needs, or fabricating
/// a witness for a state that has none). It keeps its own hand-written `complete()`, unchanged.
///
/// `progressed`, not the design sketch's own `scanned` -- every implementor already had this
/// exact accessor, under this exact name, before this trait existed (reconciling with what R3
/// already landed, not introducing a second name for the same thing).
pub(crate) trait Resumable {
    type Terminal;
    fn step(
        &mut self,
        cache: &crate::cache::BlockCache,
    ) -> impl std::future::Future<Output = anyhow::Result<Step<Self::Terminal>>> + Send;
    /// Bytes consumed so far; `complete`'s own progress channel.
    fn progressed(&self) -> u64;
    /// Drops any interactive allowance -- a background continuation is "bounded only by the file
    /// itself, by design" (docs/budgeted_scanning.md); see `Meter::lift_allowance`'s own doc
    /// comment for why this must happen before this trait's own `complete()` loop begins.
    fn lift_allowance(&mut self);
    async fn complete(
        mut self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<Self::Terminal>
    where
        Self: Sized,
    {
        self.lift_allowance();
        loop {
            match self.step(cache).await? {
                Step::Done(t) => return Ok(t),
                Step::More(..) => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.progressed(),
                        span,
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}
/// `ForwardScan`'s terminal outcome.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FwdEnd {
    /// The requested line start.
    Found(u64),
    /// EOF arrived first; the payload is the anchor to clamp to — the last
    /// real line start the scan saw, or the origin when it saw none. It is
    /// always a definitive answer, never a sentinel.
    Eof(u64),
}
/// One chunk's outcome of a resumable forward scan.
pub(crate) type FwdStep = Step<FwdEnd>;
/// A resumable scan for the `n`-th line start after a fixed origin. The
/// origin and target are captured at construction and the cursor never
/// leaves the object, so a pending continuation is the same value as the
/// interactive attempt — there is no cursor handoff to get wrong. Behavior
/// after a terminal outcome (`Found`/`Eof`) is unspecified; callers stop.
/// For an out-of-contract origin past EOF, terminal anchors clamp to the
/// file size — in-file, though not a line start.
pub struct ForwardScan {
    origin: u64,
    pos: crate::progress::Ascending,
    needed: usize,
    meter: crate::meter::Meter,
    last_found: Option<u64>,
}
impl ForwardScan {
    /// Scans for the `n`-th line start strictly after line-start `from`
    /// (`n == 0` resolves to `from` itself). Reads at most `chunk` bytes per
    /// step, clamped to one so every step makes progress. A `Meter::background` (restructure
    /// R3): this object's own contract is already "bounded per step, resumable indefinitely" --
    /// an ordinary caller that only ever takes ONE interactive step gets identical behavior
    /// either way (nothing has been charged yet, so an `allowance` equal to `chunk` and no
    /// allowance at all agree on that first step); a caller that steps this SAME object again,
    /// interactively or via `complete()`, does not, and only `background` is correct for it. The
    /// one construction that genuinely needs a cross-step ceiling (`Document::resolve_found`'s
    /// composed search-then-line-start-hunt) builds its own `Meter::interactive` directly and
    /// threads it in, rather than through this convenience constructor.
    pub fn new(from: u64, n: usize, chunk: usize) -> ForwardScan {
        ForwardScan {
            origin: from,
            // `new` has no cache access, so the real ceiling (`cache.size()`) is unknown until
            // the first `step` call narrows it (`Ascending::clamp_ceiling_to`); `u64::MAX` is the
            // honest "no bound discovered yet" starting point, never itself observed as a real
            // file size.
            pos: crate::progress::Ascending::new(from, u64::MAX),
            needed: n,
            meter: crate::meter::Meter::background(chunk),
            last_found: None,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn progressed(&self) -> u64 {
        self.meter.progressed()
    }
    /// Consumes up to one chunk of bytes looking for the target line start,
    /// never reporting a start at or past EOF (an EOF-adjacent trailing
    /// newline terminates the scan instead).
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<FwdStep> {
        let size = cache.size();
        // a computed position past EOF could otherwise echo back through
        // a terminal anchor (the n == 0 Found or the Eof clamp); degrade to
        // the real bytes, mirroring BackwardScan's out-of-range philosophy.
        self.pos.clamp_ceiling_to(size);
        self.origin = self.origin.min(size);
        if self.needed == 0 {
            return Ok(Step::Done(FwdEnd::Found(self.pos.at())));
        }
        self.meter.begin_step();
        let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
        // the motion witness for THIS step -- overwritten (never accumulated) on every real
        // advance below; `restr-R3-review.md` §R2.4's own mutation (charge a read, then break
        // before the position update) is exactly what this variable existing separately from the
        // charge witness is for: with the update removed, this stays `None` while `progress_
        // witness()` still succeeds, and the `.expect()` below -- proven unreachable in correct
        // code by `chunk.max(1)` (a fresh step's budget is never already exhausted, so its first
        // read is never refused) -- turns that mutation into a loud panic instead of the silent
        // livelock this engine's own history recorded (`crate::progress`'s own module doc
        // comment cites this exact evidence).
        let mut moved: Option<crate::progress::Progressed> = None;
        while self.pos.below_ceiling() {
            let outcome = reader
                .read_at(
                    self.pos.at(),
                    usize::MAX,
                    crate::meter::Access::Payload,
                    crate::meter::Charge::Payload,
                )
                .await?;
            let slice = match outcome {
                crate::meter::ReadOutcome::Bytes { got: b, .. } => b,
                crate::meter::ReadOutcome::Short { got, .. } => got,
                crate::meter::ReadOutcome::Empty { .. } => {
                    // the truncation policy (docs/budgeted_scanning.md, fix round F1): an empty
                    // block here means the source's real data ends at `self.pos`, below `size`
                    // (`BlockCache::block`'s own doc comment: "short at EOF, empty past EOF") --
                    // a FORWARD-direction loop treats that as the genuine end and resolves its
                    // own terminal anchor there, exactly as it would at the claimed `size`
                    // (`self.pos` IS the correct `Eof` clamp target either way).
                    return Ok(Step::Done(FwdEnd::Eof(self.clamp())));
                }
                crate::meter::ReadOutcome::OutOfBudget => break,
            };
            if slice.is_empty() {
                // batch 9 (2026-07-29), finding #1: this used to read "a `Short` read with
                // nothing new ... is the same terminal signal `Empty` is -- the source's real
                // data ends here", which is false under `BlockSource`'s own "up to `len`"
                // contract (`source.rs`). A conforming source may answer short mid-file with
                // ordinary content still to come in later blocks, and the block-indexed cache
                // memoises the shortfall, so `[pos, next_block)` is unreadable forever while
                // everything above it is fine. One bookkeeping peek separates that from a
                // genuinely truncated source -- `SearchForward::step`'s own version of this
                // branch has the full two-case derivation; an EMPTY next block means nothing
                // real exists at or past its start, so the file really did end in the short
                // block and the terminal `Eof` below is correct.
                let bs = cache.block_size() as u64;
                // saturating: see `SearchForward::step`'s own twin of this line (batch 10
                // (2026-07-29), P2) -- the block start cannot overflow, the `+ bs` can.
                let next_block = (self.pos.at() / bs * bs).saturating_add(bs);
                let next_has_real_data = next_block < size
                    && !matches!(
                        reader
                            .read_at_unbounded(
                                next_block,
                                1,
                                crate::meter::Access::Peek,
                                crate::meter::Charge::Bookkeeping,
                            )
                            .await?,
                        crate::meter::Fetched::Empty { .. }
                    );
                if !next_has_real_data {
                    return Ok(Step::Done(FwdEnd::Eof(self.clamp())));
                }
                // charged, so a source short-answering every block cannot walk a whole file
                // inside one synchronous step (the read above delivered zero bytes, so it
                // charged zero -- this is the only thing bounding the traversal).
                reader.skip_ahead(self.pos.at(), next_block)?;
                if let Some(p) = self.pos.advance_to(next_block) {
                    moved = Some(p);
                }
                continue;
            }
            for (i, &b) in slice.iter().enumerate() {
                if b == b'\n' {
                    // saturating, like `new_pos` below and for the same reason (batch 13
                    // (2026-07-29)): a source claiming `u64::MAX` has real data at the byte
                    // `u64::MAX` itself, and neither "the position after it" nor "one past the
                    // block" is representable. `u64::MAX` is the right answer for both -- it fails
                    // the `< size` / `below_ceiling` tests just below, which is exactly the verdict
                    // a position past the end should get.
                    let ls = self.pos.at().saturating_add(i as u64 + 1);
                    // terminal returns count their partial block, so `progressed` stays
                    // literally "bytes consumed" (ERRATUM 3c#2) -- the read above already
                    // charged the whole slice; give back what the answer did not need.
                    if ls >= size {
                        self.meter
                            .give_back_progress((slice.len() - (i + 1)) as u64);
                        return Ok(Step::Done(FwdEnd::Eof(self.clamp())));
                    }
                    self.last_found = Some(ls);
                    self.needed -= 1;
                    if self.needed == 0 {
                        self.meter
                            .give_back_progress((slice.len() - (i + 1)) as u64);
                        return Ok(Step::Done(FwdEnd::Found(ls)));
                    }
                }
            }
            // saturating (batch 13 (2026-07-29)): this loop asks for `usize::MAX` and consumes
            // whatever the block holds, so in the LAST representable block `pos + slice.len()` is
            // the unrepresentable exclusive end `2^64`. Batch 10's own
            // `next_block_arithmetic_does_not_overflow_in_the_final_block` could not reach it while
            // a short final block stayed short forever; retiring the refill cap completes that
            // block, and 64 delivered bytes from `u64::MAX - 63` land exactly on the overflow.
            let new_pos = self.pos.at().saturating_add(slice.len() as u64);
            if let Some(p) = self.pos.advance_to(new_pos) {
                moved = Some(p);
            }
        }
        Ok(if self.pos.below_ceiling() {
            Step::More(
                moved.expect(
                    "chunk.max(1) guarantees a fresh step's first read is never refused, so \
                     exiting this loop below `size` always followed at least one charged, \
                     moved iteration",
                ),
                self.meter.progress_witness()?,
            )
        } else {
            Step::Done(FwdEnd::Eof(self.clamp()))
        })
    }
    fn clamp(&self) -> u64 {
        self.last_found.unwrap_or(self.origin)
    }
}
impl Resumable for ForwardScan {
    type Terminal = FwdEnd;
    async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<FwdStep> {
        ForwardScan::step(self, cache).await
    }
    fn progressed(&self) -> u64 {
        ForwardScan::progressed(self)
    }
    fn lift_allowance(&mut self) {
        self.meter.lift_allowance();
    }
}
/// One chunk's outcome of a resumable backward scan. NOT a `Step<T>` alias (`Step`'s own doc
/// comment states why): `Exhausted`, below, has neither witness `Step::More` requires.
#[derive(Debug, PartialEq, Eq)]
pub enum BwdStep {
    /// The byte just after the target newline — a line start.
    Found(u64),
    /// Fewer than `n` newlines exist in the window; the line start is 0.
    Top,
    /// The chunk is consumed; call `step` again to continue. Carries BOTH witnesses (restructure
    /// R6, `Step`'s own doc comment): `Progressed` proves `self.hi` actually descended this step;
    /// `Charged` proves at least one read happened. `Ok(BwdStep::More(..))` (one field) no longer
    /// compiles; the caller must ask both the cursor and the meter.
    More(crate::progress::Progressed, crate::meter::Charged),
    /// The shared meter's own allowance was ALREADY at or past its ceiling before -- or without
    /// -- this step charging anything (restructure R3, fix round P1-2). Only reachable via
    /// `with_meter`'s own composed meter (`Document::resolve_found`): the follow-up line-start
    /// hunt can start already out of budget if the search leg that shares its Meter already
    /// spent the whole allowance. This is NOT what `More`'s own witnesses guard against -- a step
    /// that HAD budget to work with but made no progress anyway, a livelock -- it is an
    /// ordinary, resumable pend that never got the chance to attempt a single read, so it earns
    /// NEITHER a `Progressed` nor a `Charged` (nothing was attempted at all) -- exactly why this
    /// is its own variant rather than a degenerate `More`. Treated identically to `More` by every
    /// caller: retry later (`complete()`'s own `lift_allowance` removes the ceiling once a
    /// background phase takes over, so the retry is never stuck the same way twice).
    Exhausted,
}
/// A resumable scan for the `n`-th newline above a fixed position. The search
/// window is captured once, at construction: the byte at `pos - 1` is
/// excluded, because for a line-start `pos` it is the newline that made it a
/// line start, and for `pos == size` it is a trailing newline that must not
/// count as a line above. Resumption is just another `step` — the exclusion
/// is never re-derived, so the off-by-one class from bare resume cursors is
/// unrepresentable. Behavior after a terminal outcome is unspecified.
pub struct BackwardScan {
    hi: crate::progress::Descending,
    needed: usize,
    meter: crate::meter::Meter,
}
impl BackwardScan {
    /// Scans `[0, pos - 1)` downward for the `n`-th newline, reading at most
    /// `chunk` bytes per step (clamped to one so every step makes progress).
    /// Callers guard `n >= 1`; `n == 0` resolves as `Top` without reading. A `Meter::background`
    /// (restructure R3, `ForwardScan::new`'s own doc comment has the full reasoning); a caller
    /// that needs this scan to share an EXISTING meter (`Document::resolve_found`'s own
    /// composition) uses `with_meter` instead.
    pub fn new(pos: u64, n: usize, chunk: usize) -> BackwardScan {
        Self::with_meter(pos, n, crate::meter::Meter::background(chunk))
    }
    /// Like `new`, but continues an EXISTING meter instead of starting a fresh one. The follow-up
    /// line-start hunt in `Document::resolve_found` shares the match search's own meter this
    /// way, so its available budget is whatever the shared `allowance` has left -- additive by
    /// construction, never re-derived by subtracting one accessor's number from another's
    /// (`crate::meter::Meter`'s own top-level doc comment; batch-5 #2).
    pub(crate) fn with_meter(pos: u64, n: usize, meter: crate::meter::Meter) -> BackwardScan {
        BackwardScan {
            hi: crate::progress::Descending::new(pos.saturating_sub(1), 0),
            needed: n,
            meter,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn progressed(&self) -> u64 {
        self.meter.progressed()
    }
    /// ALL read work charged against this scan's own budget, payload only (this scan has no
    /// bookkeeping reads of its own -- a raw newline count needs no look-around context) so this
    /// coincides with `progressed()` for a fresh scan, but diverges once `with_meter` composes
    /// it onto a Meter another phase already charged from. See `SearchForward::charged`'s own
    /// doc comment for the fuller reasoning this struct's own composition seam shares. Exercised
    /// by `search_next_backward_composed_budget_is_additive_charged_directly` (document.rs) --
    /// no PRODUCTION caller, so the lib target alone (no test cfg) sees it as unused.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn charged(&self) -> u64 {
        self.meter.charged()
    }
    /// Bytes not yet searched. At construction this is the full window, which
    /// makes an honest progress span for a scan started as its own phase.
    pub fn remaining_bytes(&self) -> u64 {
        self.hi.at()
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
        self.hi.clamp_to(cache.size());
        let bs = cache.block_size() as u64;
        self.meter.begin_step();
        let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
        // see `ForwardScan::step`'s own identical variable (and its own discriminating test,
        // `forward_scan_resumes_across_chunks_without_handoff`) for the mutation class this
        // guards against, on this struct's own descending twin.
        let mut moved: Option<crate::progress::Progressed> = None;
        while self.hi.above_floor() {
            let idx = (self.hi.at() - 1) / bs;
            let lo = idx * bs;
            let want = (self.hi.at() - lo) as usize;
            let outcome = reader
                .read_at(
                    lo,
                    want,
                    crate::meter::Access::Payload,
                    crate::meter::Charge::Payload,
                )
                .await?;
            let slice = match outcome {
                crate::meter::ReadOutcome::Bytes { got: b, .. } => b,
                // reachable here only with NONEMPTY `got`: this loop always reads AT a block's
                // own start (`lo`), so a `got: empty` Short (a position past a short block's own
                // end) cannot occur on a first touch -- only a genuinely short, but nonempty,
                // final block can.
                crate::meter::ReadOutcome::Short { got, .. } => got,
                crate::meter::ReadOutcome::Empty { .. } => {
                    // the truncation policy (docs/budgeted_scanning.md, fix round F1): no real
                    // data exists at this position -- `self.hi` still sits above the source's
                    // real end (`size()` overstating its own data, distinct from the `hi >
                    // cache.size()` regime the clamp above already handles). A BACKWARD-direction
                    // loop DESCENDS past the fictional territory rather than freezing: `self.hi`
                    // drops to this block's own start, charged like any other read (bookkeeping,
                    // via `skip` -- nothing here was ever a candidate answer) so a huge
                    // stale-size descent pends across `step` calls instead of running unbounded
                    // in one. Strictly monotone (`lo < self.hi` always, since `idx = (self.hi -
                    // 1) / bs`), so this terminates -- either resuming real reading once the
                    // descent reaches genuine content, or reaching `hi == 0` (`Top`) if it never
                    // does.
                    reader.skip(self.hi.at(), lo)?;
                    if let Some(p) = self.hi.lower_to(lo) {
                        moved = Some(p);
                    }
                    continue;
                }
                crate::meter::ReadOutcome::OutOfBudget => break,
            };
            // **A short answer still descends a whole block, so it pays for a whole block**
            // (batch 8 (2026-07-29), P2). `self.hi` drops to `lo` below regardless of how few
            // bytes came back, so `[delivered_top, self.hi)` is territory this step passes over
            // without reading -- structurally the same act the `Empty` arm above already charges
            // via `skip`, and charged here for the identical reason its own comment gives: a
            // descent that costs less budget than the ground it covers is a descent the per-step
            // chunk cannot bound. A conforming one-byte-per-block source (`BlockSource`'s own
            // "up to `len`" contract, `source.rs`) is the extreme: uncharged, a `chunk` of 16
            // bought all 16 blocks of a 1024-byte file in ONE synchronous step
            // (`backward_scan_charges_the_span_a_short_read_leaves_undelivered`).
            //
            // Guarded rather than unconditional because `skip` rejects a non-decreasing target
            // outright (`NotMonotone`) -- a FULL read has `delivered_top == self.hi`, no gap, and
            // nothing to charge.
            let delivered_top = lo + slice.len() as u64;
            if delivered_top < self.hi.at() {
                reader.skip(self.hi.at(), delivered_top)?;
            }
            for (i, &b) in slice.iter().enumerate().rev() {
                if b == b'\n' {
                    self.needed -= 1;
                    if self.needed == 0 {
                        // count the partially examined block, so `progressed` stays literally
                        // "bytes consumed" (ERRATUM 3c#2) -- the read above already charged the
                        // whole slice; give back what the answer did not need.
                        self.meter.give_back_progress(i as u64);
                        return Ok(BwdStep::Found(lo + i as u64 + 1));
                    }
                }
            }
            if let Some(p) = self.hi.lower_to(lo) {
                moved = Some(p);
            }
        }
        if self.hi.at() == 0 {
            Ok(BwdStep::Top)
        } else {
            // fix round, P1-2: reaching here with `self.hi > 0` (the loop's own condition still
            // true) means the loop exited via a `break`, not natural completion -- the only
            // `break` in this loop is `ReadOutcome::OutOfBudget`, which `Reader::read_at` only
            // ever returns when `out_of_budget()` was ALREADY true before the read was even
            // attempted (checked first, unconditionally). So `reader.out_of_budget()` is always
            // true here -- the guard states that invariant explicitly rather than leaving it
            // implicit, and the fallback arm still propagates a genuine `NoProgress` loudly
            // (`?`'s own old behavior) should some future change to this loop's own shape ever
            // make that assumption false.
            match reader.progress_witness() {
                Ok(charged) => Ok(BwdStep::More(
                    // every charging path in this loop (a payload read, always non-empty at a
                    // block-aligned `lo` as the comment above establishes; a `skip` over
                    // provably-nonzero span) ALSO calls `lower_to` with a strictly smaller `lo`
                    // -- so `progress_witness` succeeding (charged) guarantees `moved` is `Some`,
                    // REGARDLESS of whether this step's own meter started fresh or, via
                    // `with_meter`, already composed and partway spent (restructure R6: `moved`
                    // is never fabricated here, only ever a witness this step's own loop earned).
                    moved.expect(
                        "every charge in this loop pairs with a strictly decreasing `lower_to`",
                    ),
                    charged,
                )),
                Err(e) if reader.out_of_budget() => {
                    let _ = e; // NoProgress, correctly not the failure here -- see match arm above
                    Ok(BwdStep::Exhausted)
                }
                Err(e) => Err(e.into()),
            }
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
        // see `ForwardScan::complete`'s own comment: a background continuation must not stay
        // capped by whatever ceiling an earlier interactive (possibly composed) attempt built.
        self.meter.lift_allowance();
        loop {
            match self.step(cache).await? {
                BwdStep::Found(s) => return Ok(s),
                BwdStep::Top => return Ok(0),
                // `Exhausted` is structurally unreachable here: `lift_allowance()`, above, is
                // this function's own first line, so `out_of_budget()` can only ever come from
                // this SAME step's own `chunk` (never the now-absent allowance), which always
                // permits at least one read per step (`chunk.max(1)`). Kept as a real, resumable
                // retry rather than `unreachable!()` regardless -- identical to `More` in every
                // way that matters here (this loop's own progress send uses `self.meter.
                // progressed()` directly, not the witness payload either arm carries).
                BwdStep::More(..) | BwdStep::Exhausted => {
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: self.meter.progressed(),
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
/// One chunk's outcome of a resumable newline count: `Done` carries every byte of the window
/// examined; `More`'s witness pair is `Step`'s own (this struct's own doc comment).
pub(crate) type CountStep = Step<u64>;
/// A resumable count of the newlines in `[from, to)`. The window is captured
/// at construction (an inverted window is empty); `step` consumes one budget
/// chunk at a time, and out-of-contract positions clamp into the file like
/// every scan object. Behavior after `Done` is unspecified; callers stop.
pub struct CountScan {
    pos: crate::progress::Ascending,
    meter: crate::meter::Meter,
    found: u64,
}
impl CountScan {
    /// Counts newlines in `[from, to)`, reading at most `chunk` bytes per
    /// step (clamped to one so every step makes progress). Always a
    /// `Meter::background` (restructure R3): unlike `ForwardScan`/`BackwardScan`/the search
    /// pair, `CountScan` has no interactive/`complete()` split of its own -- its only production
    /// caller (`StatusWorker`) steps it directly inside its own background loop, so it is
    /// "bounded only by the file itself" from its very first step, never a synchronous attempt
    /// with a total ceiling to lift later.
    pub fn new(from: u64, to: u64, chunk: usize) -> CountScan {
        CountScan {
            pos: crate::progress::Ascending::new(from, to.max(from)),
            meter: crate::meter::Meter::background(chunk),
            found: 0,
        }
    }
    /// Bytes examined so far.
    // dead on the lib target: the status worker, `step`'s production
    // caller, tracks convergence through the published `StatusSnapshot`
    // instead of bytes progressed, so this has no production caller — kept for
    // the test suite, same as ForwardScan/BackwardScan's `progressed` would be
    // if anything ever needed to build a progress bar over a count.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn progressed(&self) -> u64 {
        self.meter.progressed()
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
        self.pos.clamp_ceiling_to(size);
        self.meter.begin_step();
        let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
        // see `ForwardScan::step`'s own identical variable for the mutation this guards against.
        let mut moved: Option<crate::progress::Progressed> = None;
        while self.pos.below_ceiling() {
            let want = (self.pos.ceiling() - self.pos.at()) as usize;
            match reader
                .read_at(
                    self.pos.at(),
                    want,
                    crate::meter::Access::Peek,
                    crate::meter::Charge::Payload,
                )
                .await?
            {
                crate::meter::ReadOutcome::Bytes { got: b, .. } => {
                    self.found += memchr::memchr_iter(b'\n', &b).count() as u64;
                    let new_pos = self.pos.at() + b.len() as u64;
                    if let Some(p) = self.pos.advance_to(new_pos) {
                        moved = Some(p);
                    }
                }
                crate::meter::ReadOutcome::Short { got, .. } => {
                    // batch 9 (2026-07-29), finding #1: this used to say "the source's real data
                    // ends here" and return outright -- a `Short` read certifies no such thing
                    // (`meter.rs`'s own P1-1 law; `source.rs`'s "up to `len`" contract lets a
                    // conforming source answer short mid-file). Count what arrived, then let one
                    // bookkeeping peek decide whether anything real follows -- `SearchForward::
                    // step`'s own version of this branch carries the full two-case derivation.
                    self.found += memchr::memchr_iter(b'\n', &got).count() as u64;
                    let bs = cache.block_size() as u64;
                    // saturating: see `SearchForward::step`'s own twin of this line (batch 10
                    // (2026-07-29), P2) -- the block start cannot overflow, the `+ bs` can.
                    let next_block = (self.pos.at() / bs * bs).saturating_add(bs);
                    let next_has_real_data = next_block < self.pos.ceiling()
                        && !matches!(
                            reader
                                .read_at_unbounded(
                                    next_block,
                                    1,
                                    crate::meter::Access::Peek,
                                    crate::meter::Charge::Bookkeeping,
                                )
                                .await?,
                            crate::meter::Fetched::Empty { .. }
                        );
                    if !next_has_real_data {
                        return Ok(Step::Done(self.found));
                    }
                    // the newlines inside the unreadable gap are lost, unavoidably -- no read can
                    // ever reach them through this cache. Continuing past it still counts every
                    // one this scan CAN see, where returning here abandoned the whole rest of the
                    // window as well. Charged, so a source short-answering every block cannot
                    // traverse a file inside one synchronous step.
                    reader.skip_ahead(self.pos.at() + got.len() as u64, next_block)?;
                    if let Some(p) = self.pos.advance_to(next_block) {
                        moved = Some(p);
                    }
                }
                crate::meter::ReadOutcome::Empty { .. } => {
                    return Ok(Step::Done(self.found));
                }
                crate::meter::ReadOutcome::OutOfBudget => break,
            }
        }
        Ok(if self.pos.below_ceiling() {
            Step::More(
                moved.expect(
                    "chunk.max(1) guarantees a fresh step's first read is never refused, so \
                     exiting this loop below `end` always followed at least one charged, moved \
                     iteration",
                ),
                self.meter.progress_witness()?,
            )
        } else {
            Step::Done(self.found)
        })
    }
}
/// `SearchForward`/`SearchBackward`'s terminal outcome.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SearchEnd {
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
}
/// One chunk's outcome of a resumable pattern search.
// dead on the lib target: the navigation-resolution spawn sites that drive
// `SearchForward`/`SearchBackward` from a real search request arrive in a
// later task (search v1, task 4+); kept alive here for the test suite,
// same idiom as `CountScan::scanned`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type SearchStep = Step<SearchEnd>;
/// A resumable forward search for the next match at or after `from`, bounded
/// above by `limit` (typically the file size, or an earlier position when a
/// wraparound leg must stop before re-finding its own starting match). The
/// carry keeps up to `MAX_MATCH_LEN` already-read bytes BEHIND `pos` --
/// `[pos - keep, pos)`, not ahead of it -- so a match straddling a block
/// read is still found whole (batch 4 (2026-07-24), finding #10: `MAX_MATCH_LEN`,
/// not `MAX_MATCH_LEN - 1` -- see this struct's own `$` paragraph, below, for why).
/// The guarantee is `find_all`'s own LEFTMOST
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
/// ANCHOR CORRECTNESS (`^`/`$`/`\b`/`\B`, `multi_line` -- `SearchPattern::compile`'s own doc
/// comment), using the boundary-context model (`search.rs`'s own top-level doc comment, batch 4
/// (2026-07-24)): `^`/`\b`/`\B` need `ctx`, below: WITHOUT it, `pos`'s own hay-index (0, always,
/// since `carry` there would hold only bytes AT OR AFTER `pos`) is unconditionally `^`-eligible
/// regardless of the REAL byte before `pos` -- true not just at `from` (the scan's own origin, if
/// it sits mid-line) but at EVERY carry-shrink seam thereafter, since the shrunk carry's own new
/// front is exactly as edge-shaped as `from` was. Confirmed by two regressions found during
/// batch 3's own review, before `ctx` existed: `search_forward_does_not_fake_a_line_start_at_a_
/// carry_shrink_seam` (a false match, far from `from`, at a shrink seam) and a genuine arithmetic
/// panic in this method's own `scanned` bookkeeping (`e < carry_len`, i.e. a "match" resolving
/// entirely inside bytes ALREADY fully searched on a prior iteration -- only possible because
/// that earlier search used the correct, now-stale look-behind while this one used none). `ctx`
/// fixes both: up to `CTX_BEHIND` real look-behind bytes, threaded through every iteration (not
/// just the first, and, since batch 4 (2026-07-24) finding #9, wide enough to hold a whole UTF-8
/// character rather than a lone possibly-invalid continuation byte -- `(?u:\b)`/`(?u:\B)` need
/// the WHOLE preceding character, not one raw byte), makes every zero-width assertion's verdict
/// at any given position stable across the whole scan, restoring the invariant this method's own
/// comments already relied on.
///
/// `$` now gets a genuine end margin here too, the SAME KIND `SweepAnalysis::step`'s own
/// `safe_to` always reserved (review response, batch 3 (2026-07-23) -- `step`'s own `safe_to`,
/// below, was finding #2's fix for LEFTMOST-ness, not `$`, but it closes this as a side effect):
/// a match is only ever accepted once `match_at < safe_to`, i.e. once a full `MAX_MATCH_LEN`
/// bytes PAST ITS OWN END have been read (or this scan hit its own read boundary) -- batch 4
/// (2026-07-24), finding #10: the margin is `MAX_MATCH_LEN` bytes past the match's own start,
/// not `MAX_MATCH_LEN - 1` -- a maximal-length candidate's own trailing assertion is evaluated
/// at `match_at + MAX_MATCH_LEN`, one byte past its own last content byte, and the old margin
/// gave it nothing there to check. With the corrected margin, EVERY match up to and including
/// `MAX_MATCH_LEN` long always has at least one real byte past its own end already in hand
/// before being accepted, so `$`/`\b`/`\B` are checked against genuine content there, never hay's
/// own CURRENT (possibly artificial) end -- closing the residual an earlier version of this
/// comment accepted at exactly this one length
/// (`search_forward_dollar_is_confirmed_by_a_real_byte_even_at_exactly_max_match_len`, this
/// module's own test, now pins the CLOSED behavior; `docs/search.md` updated to match).
///
/// fix round (2026-07-25), F4: the OTHER residual an earlier version of this comment left open
/// -- a BOUNDED leg reaching its own artificial `limit` (never the true file end) getting no
/// margin at all (`safe_to == read_end`, no `MAX_MATCH_LEN` subtracted) -- is ALSO closed now,
/// and that earlier version's own reasoning for calling it "structurally uncloseable" (reading
/// past `limit` at all, even as pure lookahead, would violate this module's own "never read...
/// past the point it was told to stop" rule) did not survive being checked against `SearchBackward`'s
/// own IDENTICAL rule for its floor, decided the OPPOSITE way in the SAME commit: `limit` bounds
/// what may be ACCEPTED as a match start, not what data exists past it, exactly like `floor` does
/// -- so a BOUNDED, CONSULTABLE peek (up to `CTX_BEHIND` real bytes past `limit`, informing
/// `$`/`\b`/`\B` but never itself an acceptable start, and never letting a match's own BODY
/// silently consume it -- `payload_end`/`accept_end` and the `e <= payload_end` guard below,
/// symmetric with `find_all_starting_in`'s identical "wholly contained" rule) violates nothing.
/// `search_forward_dollar_is_confirmed_by_a_real_byte_even_at_a_bounded_legs_own_limit` (reversed
/// from `..._can_false_positive_at_a_bounded_legs_own_limit`) now pins the CLOSED behavior;
/// `search_forward_dollar_still_matches_at_a_bounded_legs_own_limit_when_genuine` and
/// `search_forward_dollar_body_cannot_consume_bytes_past_a_bounded_legs_own_limit` pin the two
/// ways this fix must NOT overreach (a genuine confirming byte still matches; a match needing the
/// peeked bytes for its own BODY, not just an assertion, still does not). No residual of this
/// shape remains -- this struct's own `$` correctness now matches `SearchBackward`'s exactly.
///
/// batch 5 (2026-07-26), finding #3: finding #10's own margin (one byte past a cap-length match's
/// own end) is enough for `$` and ASCII `\b`/`\B`, which need only that one byte -- but a
/// Unicode-aware `(?u:\b)`/`(?u:\B)` needs the FULL following character to decode, up to
/// `CTX_AHEAD` bytes, not one (`search.rs`'s own top-level doc comment: the identical "an
/// incomplete byte decodes as non-word regardless of the real character" hazard finding #9 already
/// closed on the LOOK-BEHIND side, now closed on this, the trailing side). The true margin is
/// `MAX_MATCH_LEN + CTX_AHEAD - 1` real bytes past the last acceptable start, not `MAX_MATCH_LEN`
/// alone -- `safe_to`'s own mid-file branch, the carry-shrink `keep`, and the F4 peek's width
/// (renamed from `CTX_BEHIND` to `CTX_AHEAD` -- the two were only ever numerically equal by
/// coincidence, both 4, the same UTF-8 max character width) all widen together.
/// `search_forward_unicode_word_boundary_needs_the_full_trailing_character_not_one_byte` (this
/// module's own test) pins the fixed defect: `a{4096}(?u:\b)` immediately followed by `é` (a Unicode word
/// character, so no boundary exists) used to fabricate `(?u:\b)` from `é`'s own lone, invalid
/// lead byte alone.
/// Whether a search leg's own far bound admits a match starting exactly there (restructure R6,
/// `structural-scan.md` §3.8). The prose this used to live in only -- `SearchForward::new`'s own
/// EXCLUSIVE `limit`, `SearchBackward`'s own INCLUSIVE one -- moves into the signature: `document.
/// rs`'s own `wrapped_leg`, the one caller that composes both directions against the SAME seam
/// side by side, now states which rule it means at each call instead of relying on a reader to
/// recall which struct's own doc comment applies. Each constructor still only accepts its own
/// direction's rule (enforced, not merely documented, below) -- `Bound` does not relax which rule
/// is correct where; it makes a caller who gets it backward fail loudly instead of silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bound {
    /// A match starting exactly at this position is NOT found.
    Exclusive(u64),
    /// A match starting exactly at this position IS found.
    Inclusive(u64),
}
impl Bound {
    fn exclusive(self) -> u64 {
        match self {
            Bound::Exclusive(v) => v,
            Bound::Inclusive(v) => panic!(
                "Bound::Inclusive({v}) passed where this constructor's own contract requires \
                 Exclusive -- SearchForward's far bound is always exclusive, never inclusive"
            ),
        }
    }
    fn inclusive(self) -> u64 {
        match self {
            Bound::Inclusive(v) => v,
            Bound::Exclusive(v) => panic!(
                "Bound::Exclusive({v}) passed where this constructor's own contract requires \
                 Inclusive -- SearchBackward's far bound is always inclusive, never exclusive"
            ),
        }
    }
}
/// `SearchForward`'s phase (restructure R6, `structural-scan.md` §3.2): `ctx_ready: bool` dies
/// into the discriminant -- "have I seeded look-behind context" is not a fact stored alongside
/// the seed's own leftovers, it is where this leg IS. `seed_block` moves INTO `Scan` specifically
/// so `(SeedCtx, seed_block: Some(_))` -- reachable in the pre-R6 code whenever a seed retry left
/// a block cached from an earlier, now-superseded attempt before returning `More`, always
/// harmless there only because the payload loop's own index filter (`step`'s own `.filter(|
/// (sidx,_)| *sidx == idx)`) happened to discard a stale entry -- stops being expressible instead
/// of merely being filtered.
enum FwdPhase {
    /// Seeding `ctx` (`step`'s own retry loop, below): `self.pos` may still DESCEND here if the
    /// source turns out truncated (batch 5 (2026-07-27), finding #10-internal).
    SeedCtx,
    /// `ctx` is certified; `self.pos` is the scan's own forward resumption cursor from here on.
    /// `seed_block` is the block the seed's own last touch fetched at `pos`'s block index, if
    /// any -- reused by the FIRST payload iteration instead of a second, promoting cache touch
    /// (`step`'s own reuse comment, below); consumed (`.take()`n) on that first use regardless of
    /// whether the index matched.
    Scan {
        seed_block: Option<(u64, crate::cache::Block)>,
    },
    /// batch 6 (2026-07-28), finding #2: the F4 peek (a bounded leg's own final boundary,
    /// `at_final && !at_eof`, `step`'s own comment at the transition INTO this phase has the
    /// full derivation) cut short by `OutOfBudget` before gathering the full `CTX_AHEAD`
    /// consult-only margin a trailing assertion at the boundary needs to be judged at all. Before
    /// this phase existed, an incomplete peek was silently treated as a COMPLETE one -- the
    /// candidate's own verdict came out budget-dependent (`Exhausted` at a low budget, `Found` at
    /// a high one on the IDENTICAL file), violating "budgets bound read work, not answers"
    /// (`docs/budgeted_scanning.md`). Entered once, resolved once: this leg's own payload cursor
    /// (`self.pos`) has already reached `size` by the time this phase exists (`step`'s own
    /// comment at the transition), so `Scan`'s own `while self.pos < size` loop can never run
    /// again to re-derive any of this -- `self.ctx`/`self.carry` do not change again either
    /// (nothing left to seed or scan), so only the peek's OWN progress needs to survive a resumed
    /// call.
    Peek {
        /// The peek's own gather cursor -- `size..peek_end`, advanced in place across as many
        /// resumed calls as it takes. Never reset: restarting from `size` on every resumed call
        /// would silently discard real progress a prior call already paid for (and, at a small
        /// enough `chunk`, would never converge at all -- a resumed call's own fresh budget can
        /// cross a block boundary this phase's own gather span straddles, but only if the bytes
        /// already on THIS side of it are kept, not re-fetched).
        p: u64,
        /// Bytes gathered so far, oldest first -- the tail `Hay::leftmost_confirmed` will see
        /// appended past `payload_end` once `gather_peek` reports completion.
        peeked: Vec<u8>,
        /// `self.ctx.len() + self.carry.len()` at the moment this phase was entered -- both
        /// fields are frozen from here on (this doc comment's own argument), so this one number
        /// plus `self.ctx`/`self.carry` themselves is enough to rebuild the exact hay
        /// `resolve_bounded_terminal` needs, with no second, driftable copy of either field.
        payload_end: usize,
        /// How many of `payload_end`'s own bytes were already in `self.carry` BEFORE this
        /// iteration's fresh payload slice was merged into it -- `resolve_bounded_terminal`'s own
        /// give-back arithmetic needs this to report `progressed()` exactly as precisely as the
        /// ordinary (non-peek) path already does (ERRATUM 3c#2's rule).
        carry_len_before: usize,
    },
}
// dead on the lib target: see `SearchStep`'s own doc comment.
#[cfg_attr(not(test), allow(dead_code))]
pub struct SearchForward {
    pattern: std::sync::Arc<crate::search::SearchPattern>,
    pos: u64,
    /// The `from` this leg was constructed with, kept immutable for its whole life -- unlike
    /// `pos`, which the certifying ctx-seed retry (batch 5 (2026-07-27), finding #10-internal)
    /// may DESCEND below it when a truncated source certifies its own real end below where this
    /// leg was told to start. Batch 5 fix round 2, P1: `pos` may descend past `origin` to CERTIFY
    /// the real end (nothing else can determine whether a zero-width match legitimately exists
    /// there), but `origin` is what gates whether the certified result may ever be REPORTED --
    /// see `step`'s own use of it, and its doc comment's citation of this struct's own decision-1
    /// precedent (`floor` bounds what may be accepted, never what exists).
    origin: u64,
    limit: u64,
    carry: Vec<u8>,
    last_nl: Option<u64>,
    meter: crate::meter::Meter,
    /// Up to `CTX_BEHIND` real look-behind bytes immediately below `pos`, oldest first (so
    /// `ctx.last()` is always the real byte at `pos - 1`) -- empty only before the `Scan` phase,
    /// and (even after) whenever `pos == 0` at the point it was resolved (file position 0 needs
    /// no real predecessor of its own: it is unconditionally a valid line start). Read lazily on
    /// the first `step` call (needs a cache access `new` cannot make -- the task brief's own
    /// "OPTIONAL" seed, folded into the general mechanism below since a one-time seed alone
    /// does not survive a later carry-shrink), then kept current across every shrink after
    /// that. See this struct's own ANCHOR CORRECTNESS doc comment.
    ctx: Vec<u8>,
    /// **Where `ctx ++ carry` actually BEGINS** (restructure R7, batch 8 (2026-07-29)) -- state
    /// maintained at every mutation of that window, never re-derived from its length at a use
    /// site. This is the SearchForward half of the class `hay::Assembly` retires: four hays here
    /// used to compute their own base as `Abs(self.pos - (ctx.len() + carry.len()))`, an
    /// arithmetic that silently re-asserts "everything I hold arrived contiguously, ending at
    /// `pos`" every time it is written. The premise is TRUE on this scan -- `take` is the
    /// DELIVERED length (`slice.len()`, never the requested `want`), so a short read advances
    /// `pos` by exactly what arrived and the window stays flush -- which is precisely why it was
    /// worth making state: a premise that holds only because of a subtle fact three hundred lines
    /// away, restated in four places, is one edit away from being false in one of them. Tracked
    /// here, "what happens to the base" becomes a question each MUTATION has to answer, and
    /// `window()` asserts the two agree.
    ///
    /// Maintained at exactly three places: construction (`from`, both buffers empty), the seed's
    /// own certification (`pos - ctx.len()`, carry still empty there), and the carry-shrink
    /// (advanced by the bytes the shrink drops off the front). Appending a freshly read `slice`
    /// does not move it, by definition -- that grows the window upward.
    window_base: crate::search::hay::Abs,
    phase: FwdPhase,
    /// Fix round (R6 review P2-2): a pin for `seed_progress`'s own termination lemma (`step`'s
    /// own doc comment) -- set the FIRST (and, in correct code, only) time `seed_progress` mints
    /// a witness; `assert!`ing it is not already `true` at that moment turns "the `SeedCtx ->
    /// Scan` transition re-armed" from a silent livelock (the reviewer's own re-entrant-`Certify`
    /// construction, adapted here, hung the suite for the `SearchBackward` twin) into an
    /// immediate, loud panic instead. A plain `assert!`, not `debug_assert!`: this project's own
    /// `--release` test profile (`just test-release`) disables `debug_assert!` (no
    /// `debug-assertions` override in `[profile.release]`), and this check must hold in both --
    /// caught in the fix round when a `#[should_panic]` test pinning this exact invariant passed
    /// under `just test` but failed under `just test-release`, a cheap always-on bool compare
    /// against a genuine, if rare, correctness hazard.
    seed_fallback_minted: bool,
}
#[cfg_attr(not(test), allow(dead_code))]
impl SearchForward {
    /// Searches forward from `from`, bounded above by `limit` (clamped to
    /// the real file size at `step` time), reading at most `chunk` bytes per
    /// step (clamped to one so every step makes progress). `limit` is an
    /// EXCLUSIVE upper bound on match starts: a match starting exactly at
    /// `limit` is not found -- asymmetric with `SearchBackward`'s own
    /// INCLUSIVE far bound (`Bound`'s own doc comment); a wrap-seam composer
    /// must account for that one byte. Always `Bound::Exclusive` -- passing
    /// `Inclusive` here panics (`Bound::exclusive`'s own doc comment).
    pub fn new(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        from: u64,
        limit: Bound,
        chunk: usize,
    ) -> SearchForward {
        SearchForward {
            pattern,
            pos: from,
            origin: from,
            limit: limit.exclusive(),
            carry: Vec::new(),
            last_nl: None,
            meter: crate::meter::Meter::background(chunk),
            ctx: Vec::new(),
            window_base: crate::search::hay::Abs(from),
            phase: FwdPhase::SeedCtx,
            seed_fallback_minted: false,
        }
    }
    /// **The look-behind + carry window, positioned where this scan has TRACKED it** (restructure
    /// R7, batch 8 (2026-07-29)) -- `window_base`'s own doc comment has the full derivation of
    /// why that is state rather than a subtraction repeated at each of this struct's four hay
    /// sites. `ctx` is the look-behind-only prefix and `carry` the acceptable remainder; the two
    /// are contiguous by construction, so `extend_above` here can never legitimately refuse, and
    /// the `debug_assert` is what says so out loud rather than leaving it to a reader.
    ///
    /// Callers narrow `accept`/`body_limit` on the result themselves, in FILE coordinates via
    /// `Assembly::local_of` -- which is the other half of the win: the old `Local(lb)` spellings
    /// silently depended on `lb` being recomputed after every buffer change, in the right order.
    ///
    /// Takes its fields explicitly rather than `&self`: every call site sits inside `step`, where
    /// `self.phase` and `self.meter` are already mutably borrowed (the `seed_block` binding and
    /// the `Reader`), and Rust has no way to express "borrows only these other three fields" on a
    /// method. The four arguments ARE the window, so passing them is not losing anything.
    fn window_of(
        base: crate::search::hay::Abs,
        ctx: &[u8],
        carry: &[u8],
        pos: u64,
    ) -> crate::search::hay::Assembly {
        let mut asm = crate::search::hay::Assembly::anchored_at(base, ctx);
        let joined = asm.extend_above(base + ctx.len() as u64, carry);
        debug_assert_eq!(
            joined,
            crate::search::hay::Joined::Contiguous,
            "ctx and carry are two halves of ONE window by construction -- a gap here means \
             `window_base` drifted from the buffers it describes"
        );
        debug_assert_eq!(
            asm.end(),
            crate::search::hay::Abs(pos),
            "the window always ends exactly at the read frontier: `take` is the DELIVERED length, \
             so `pos` advances by precisely what was appended"
        );
        asm
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn progressed(&self) -> u64 {
        self.meter.progressed()
    }
    /// ALL read work charged against this leg's own budget -- payload AND bookkeeping (ctx seed,
    /// skips), unlike `progressed()` (payload only). Unit G's own accessor (batch-5 finding #2):
    /// `progressed()` answers "how much of the file has the user been shown", a different
    /// question from "how much of the budget is left", and finding #2 was exactly that
    /// conflation. The composition fix (`Document::resolve_found`) ended up reading the shared
    /// `Meter` directly (`SearchLeg::into_meter`) rather than this accessor, but this is what
    /// `search_forward_ctx_seed_reads_charge_the_budget_the_driver_reads` (this module's own
    /// test, below) uses to assert the driver's own budget sees ALL of a step's charges, not
    /// just its payload -- finding #2's own claim, pinned directly rather than only through the
    /// composition path that motivated it.
    pub fn charged(&self) -> u64 {
        self.meter.charged()
    }
    /// Consumes this leg for its own Meter, still a plain `background` one (no allowance) -- a
    /// bare one-shot `step()` and a `complete()`-driven background continuation both only ever
    /// needed the per-step `chunk` cap, never a running total, and MANY tests step a leg directly,
    /// repeatedly, without ever calling `complete()` (an allowance imposed here from construction
    /// would wrongly cap THEIR total too, not just the one composed caller's). `Document::
    /// resolve_found`'s composition (batch-5 finding #2) imposes an allowance onto the returned
    /// Meter itself (`Meter::impose_allowance`), at the one call site that actually needs it,
    /// leaving every other consumer of this leg untouched.
    pub(crate) fn into_meter(self) -> crate::meter::Meter {
        self.meter
    }
    /// batch 6 (2026-07-28), finding #2: the F4 peek's own upper bound -- up to `CTX_AHEAD`
    /// consult-only bytes past this leg's own bounded limit, never past the true file end. A
    /// tiny, pure function rather than an inline expression because BOTH the fresh-entry peek
    /// (`step`'s own main loop) and a resumed one (`FwdPhase::Peek`, above) need the IDENTICAL
    /// value, and `size`/`cache.size()` are both fixed for this leg's whole life (`FwdPhase::
    /// Peek`'s own doc comment already relies on this), so re-deriving it here is exact, never a
    /// second, driftable copy of the formula.
    fn peek_end(cache: &crate::cache::BlockCache, size: u64) -> u64 {
        size.saturating_add(crate::search::hay::Hay::consultable_reach().get() as u64)
            .min(cache.size())
    }
    /// batch 6 (2026-07-28), finding #2: gathers the F4 peek's own up-to-`CTX_AHEAD` consult-only
    /// bytes, resumable -- `p`/`peeked` are the caller's own cursor and accumulator, advanced in
    /// place, so a call that runs out of budget mid-gather can be resumed verbatim later with a
    /// fresh chunk rather than restarting (and re-charging) bytes already in hand. Restarting
    /// from `size` on every resumed call was the design this rejected: at a small enough `chunk`,
    /// a peek span that straddles a block boundary would never converge -- a resumed call's own
    /// fresh budget can cross that boundary, but only if the bytes already gathered on the near
    /// side of it are kept, not re-fetched every time (`FwdPhase::Peek`'s own doc comment).
    ///
    /// Returns `true` once nothing more will ever be gathered (`p` reached `peek_end`, or a
    /// `Short`/`Empty` read genuinely certified no more real bytes exist -- the SAME two "stop
    /// peeking" reasons the original, non-resumable peek loop used), `false` when `OutOfBudget`
    /// cut this call off before either.
    async fn gather_peek(
        reader: &mut crate::meter::Reader<'_>,
        p: &mut u64,
        peek_end: u64,
        peeked: &mut Vec<u8>,
    ) -> anyhow::Result<bool> {
        while *p < peek_end {
            let want = (peek_end - *p) as usize;
            match reader
                .read_at(
                    *p,
                    want,
                    crate::meter::Access::Peek,
                    crate::meter::Charge::Bookkeeping,
                )
                .await?
            {
                crate::meter::ReadOutcome::Bytes { got, .. } => {
                    *p += got.len() as u64;
                    peeked.extend_from_slice(&got);
                }
                crate::meter::ReadOutcome::Short { got, .. } => {
                    // batch 6 (2026-07-28), K2 fix round, P3-2: a `Short` answer bounds the peek
                    // CONSERVATIVELY, it does not CERTIFY an end -- `meter.rs`'s own P1-1 law
                    // (only a genuinely `Empty` read may certify, at that read's own position;
                    // this very commit's own rider, `SearchBackward`'s wrap-leg corollary,
                    // rewrites a different site to stop making the identical "short implies
                    // real EOF" inference). Stops peeking with whatever it has: a second,
                    // now-empty touch at the same position would gain nothing (this position's
                    // own answer cannot change), but that is a statement about redundant reads,
                    // not about what the short answer proves.
                    peeked.extend_from_slice(&got);
                    return Ok(true);
                }
                crate::meter::ReadOutcome::Empty { .. } => return Ok(true),
                crate::meter::ReadOutcome::OutOfBudget => return Ok(false),
            }
        }
        Ok(true)
    }
    /// batch 6 (2026-07-28), finding #2's own shared resolution, reached only once `gather_peek`
    /// reports completion (`step`'s own two call sites -- fresh-entry and resumed -- both only
    /// reach here then): builds the hay from `self.ctx` + `self.carry` + whatever the peek
    /// gathered and decides this leg's own terminal Found/End. Always returns `Done`: `self.pos`
    /// already equals `size` by the time either call site reaches here (`FwdPhase::Peek`'s own
    /// doc comment), so the payload loop can never run again regardless of how this resolves.
    ///
    /// `payload_end`/`carry_len_before` are passed in rather than re-derived because a RESUMED
    /// call's own `self.pos` no longer carries "where this iteration's fresh slice began" -- both
    /// were captured once, at the moment `at_final && !at_eof` was first detected, from `self.
    /// ctx`/`self.carry`, which do not change again before this phase resolves.
    fn resolve_bounded_terminal(
        &mut self,
        peeked: &[u8],
        payload_end: usize,
        carry_len_before: usize,
    ) -> SearchStep {
        // restructure R7 (batch 8 (2026-07-29)): the tracked window, then the peek appended AT
        // the position it was gathered from. `self.pos` is `size` on every path into this phase
        // (`FwdPhase::Peek`'s own doc comment: it commits to `read_end` before the peek, and
        // `at_final` with `read_end <= size` forces the two equal), and `gather_peek` starts at
        // `size` -- so the peek begins exactly where the window ends. That was the premise the
        // old `Abs(self.pos - payload_end)` base assumed silently; here it is the argument, and a
        // peek that ever started somewhere else would be refused rather than mis-based.
        let asm = {
            let mut asm = Self::window_of(self.window_base, &self.ctx, &self.carry, self.pos);
            debug_assert_eq!(
                asm.bytes().len(),
                payload_end,
                "self.ctx/self.carry are frozen for the whole life of FwdPhase::Peek (its own \
                 doc comment) -- payload_end, captured once at entry, must still match their \
                 combined length exactly"
            );
            let _ = asm.extend_above(crate::search::hay::Abs(self.pos), peeked);
            asm
        };
        let accept_from = self.window_base + self.ctx.len() as u64;
        // the payload/peek boundary, in FILE coordinates: `self.pos` is where the window ended
        // and the lookaround-only bytes began. `Hay::body_limit` centralizes the "the peek's own
        // lookaround-only bytes can never become an acceptable match START or BODY" rule
        // (restructure R4, 2026-07-27); this is that boundary, no longer a captured length.
        let payload_boundary = asm.local_of(crate::search::hay::Abs(self.pos));
        let hay_bytes = asm.bytes().to_vec();
        let hay = asm
            .hay()
            .with_body_limit(payload_boundary)
            // `at_final && !at_eof` is this whole phase's own precondition (`FwdPhase::Peek`'s doc
            // comment) -- `high` is unconditionally `Cut`, never the true file end.
            .with_high(crate::search::hay::Edge::Cut)
            .with_accept(asm.local_of(accept_from)..payload_boundary);
        let accept = hay.accept_with_eof_widening();
        let hay = hay.with_accept(accept);
        // `safe_to` on this path is always `read_end` (the `at_final`, non-`at_eof` branch of
        // `step`'s own ordinary formula) -- and `read_end == self.pos` here (`FwdPhase::Peek`'s
        // own doc comment), so `explored_to` is exactly the payload boundary above.
        let explored_to = payload_boundary;
        // batch 7 (2026-07-28), finding #3: no `advancing` argument any more. This boundary is
        // this leg's own final one -- no later call on this scan ever presents a bigger hay here
        // (`FwdPhase::Peek`'s own doc comment) -- which USED to matter, because a candidate
        // rejected on its trailing margin had to be retried at `s + 1` here (batch-4 R1's
        // alternative-recovery) rather than deferred to a call that would never come, and that
        // retry was finding #3's own quadratic. `leftmost_confirmed` no longer proposes rejected
        // candidates at all, so both the retry and the caller-supplied fact that used to select
        // it are gone: whether this boundary will grow no longer changes what this call returns.
        match hay.leftmost_confirmed(&self.pattern, explored_to) {
            crate::search::hay::Candidate::Found(s, e) => {
                if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes[..s.0]) {
                    self.last_nl = Some(self.pos - payload_end as u64 + nl as u64);
                }
                let match_at = hay.to_abs(s).0;
                // ERRATUM 3c#2's rule, the identical give-back the ordinary (non-peek) path
                // performs -- `take` is this iteration's own fresh payload span, `payload_end -
                // carry_len_before` restated (`FwdPhase::Peek`'s own field doc comments derive
                // the equality).
                let take = (payload_end - carry_len_before) as u64;
                let needed = (e.0 as u64).saturating_sub(carry_len_before as u64);
                self.meter.give_back_progress(take.saturating_sub(needed));
                let line_start = self.last_nl.filter(|&nl| nl < match_at).map(|nl| nl + 1);
                SearchStep::Done(SearchEnd::Found {
                    match_at,
                    line_start,
                })
            }
            crate::search::hay::Candidate::Pending(s) => {
                if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes[..s.0]) {
                    self.last_nl = Some(self.pos - payload_end as u64 + nl as u64);
                }
                SearchStep::Done(SearchEnd::End)
            }
            crate::search::hay::Candidate::Exhausted => {
                if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes) {
                    self.last_nl = Some(self.pos - payload_end as u64 + nl as u64);
                }
                SearchStep::Done(SearchEnd::End)
            }
        }
    }
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        // batch 4 (2026-07-24), finding #1: `from` past the true EOF makes this leg's own
        // domain -- starts >= `from` (this struct's own `new` doc comment: "INCLUSIVE... the
        // first byte at-or-after which a match START counts") -- EMPTY: even the zero-width
        // position at the true end (`size`) is < `from` once `from > size`, so nothing may ever
        // be accepted. The ORIGINAL fix for batch 3's finding #6 (the panic this still guards
        // against: an out-of-contract `pos`, e.g. a stale saved position on a since-truncated
        // file, or -- the shape this finding is about -- a wraparound leg's naive caller) instead
        // CLAMPED `pos` down to `size` here, avoiding the panic but also silently re-opening
        // position `size` as if it still satisfied `pos >= from`, which it no longer does once
        // `from > size`: `document.rs`'s own `search_next` reaches exactly this the moment a
        // cursor already parked on a zero-width match at the true end repeats the SAME forward
        // search (`from = current_match + 1 = size + 1`) -- the clamp re-admitted the very
        // self-hit `search_next`'s own leg-1/leg-2 split exists to exclude, defeating `n`'s own
        // repeat-navigation contract (`search_next_forward_wraps_and_terminates_on_a_lone_eof_
        // match`/the exclusive-repeat truth-table tests, document.rs). An empty domain needs no
        // read at all -- not even the ctx seed below -- so this returns before touching the cache.
        if self.pos > cache.size() {
            return Ok(SearchStep::Done(SearchEnd::End));
        }
        let bs = cache.block_size() as u64;
        self.meter.begin_step();
        let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
        // batch 6 (2026-07-28), finding #2: resume a previously-incomplete F4 peek before
        // anything else this call might do. Checked here, first, rather than falling through to
        // `Scan`'s own payload loop below: `self.pos` already equals `size` by construction
        // whenever this phase is active (`FwdPhase::Peek`'s own doc comment), so that loop's own
        // `while self.pos < size` guard would run zero iterations regardless -- checking here
        // instead just skips the wasted evaluation and keeps the peek's own resumption cursor
        // (`p`/`peeked`) the ONLY state this call touches.
        if let FwdPhase::Peek {
            p,
            peeked,
            payload_end,
            carry_len_before,
        } = &mut self.phase
        {
            let p_entry = *p;
            let size = cache.size().min(self.limit);
            let peek_end = Self::peek_end(cache, size);
            if Self::gather_peek(&mut reader, p, peek_end, peeked).await? {
                let peeked = std::mem::take(peeked);
                let payload_end = *payload_end;
                let carry_len_before = *carry_len_before;
                return Ok(self.resolve_bounded_terminal(&peeked, payload_end, carry_len_before));
            }
            // Still incomplete. This call's own `begin_step()`, just above, means
            // `out_of_budget()` was false when `gather_peek`'s own first read attempted -- some
            // real progress always happens here (`p` itself advances by at least one byte, or
            // the loop completes via a certifying `Short`/`Empty`, which `gather_peek` treats as
            // done, never as incomplete): never a zero-witness call the way a SHARED, already-
            // exhausted `Meter::interactive` allowance could produce (`BwdStep::Exhausted`'s own
            // scenario, R3). `SearchForward`'s own meter is ALWAYS `Meter::background` -- see
            // `new`'s own doc comment and `wrapped_leg`'s forward branch, `document.rs`, the only
            // two constructors -- so that shared-allowance state cannot arise for this leg at
            // all; `Step<SearchEnd>`'s existing `More` needs no analogous no-witness variant.
            let moved = crate::progress::Ascending::new(p_entry, u64::MAX)
                .advance_to(*p)
                .expect(
                    "gather_peek's own OutOfBudget arm is reachable only mid-loop (p < peek_end \
                     going in, this phase's own invariant for ever being entered), and a fresh \
                     begin_step() means out_of_budget() was false for its own first read attempt \
                     this call -- that attempt always advances p by at least one byte before any \
                     later attempt in the same call can be refused",
                );
            return Ok(SearchStep::More(moved, reader.progress_witness()?));
        }
        // restructure R6, discovered by `search_forward_ctx_seed_reads_charge_the_budget_the_
        // driver_reads` (an R3-era test, not a new one): the ctx-seed gather below is UNBOUNDED
        // (`read_at_unbounded`, ignoring budget by design, batch-5 finding #5) and charges as
        // BOOKKEEPING -- at a small enough `chunk`, that charge alone can exhaust the WHOLE
        // shared per-step budget before the `Scan` phase's own first read ever runs, even within
        // the SAME call the seed itself just certified in. `self.pos` never moves during an
        // ordinary (non-descending) seed gather, so the `Scan` phase's own `scan_entry`-based
        // witness (below) has nothing to report even though real, charged work genuinely
        // happened THIS call. `seed_progress` is the fallback witness for exactly that.
        //
        // **Fix round (R6 review P2-2) -- the real termination argument, stated, not implied.**
        // The ctx-gather's own local cursor (`p`, below) advancing from `ctx_start` to `self.pos`
        // IS real, non-fabricated motion (`Ascending::advance_to`, never constructed from
        // nothing) -- but that fact alone does not close the livelock class this fallback exists
        // to prevent: `p` is created fresh and discarded every call, so a hypothetical bug that
        // kept re-entering `FwdPhase::SeedCtx` without ever reaching `Scan` could mint this SAME
        // honest witness forever, each call individually indistinguishable from the last, driving
        // an unbounded `More` loop with BOTH witnesses genuinely present -- the reviewer
        // demonstrated exactly this shape on `SearchBackward`'s own `Certify` twin (a re-entrant
        // phase minting the same honest fallback witness every call hangs the suite; adopted
        // below as `search_backward_re_entrant_certify_would_livelock_the_guard_catches_it`).
        // What ACTUALLY makes this fallback safe is `self.phase` transitioning `SeedCtx -> Scan`
        // exactly once and never back (`step`'s own phase assignment, below, is the ONLY place
        // that ever leaves `SeedCtx`, and nothing ever re-enters it) -- so this arm runs at most
        // once per object lifetime, and `seed_progress` can be minted at most once, ever.
        // `seed_fallback_minted` (this struct's own field) turns that "at most once" claim from
        // an argument the reader must trust into an `assert!` that panics the instant it
        // stops being true.
        //
        // `None` whenever the seed phase does not run THIS call, or runs but never advances `p`
        // at all (only possible when `self.pos == ctx_start` already, e.g. `pos == 0` -- and that
        // shape gathers zero bytes, charges nothing, so `Scan`'s own budget is untouched and its
        // own witness always has something to report instead; see the final `match` below).
        let mut seed_progress: Option<crate::progress::Progressed> = None;
        if matches!(self.phase, FwdPhase::SeedCtx) {
            // restructure R6: `self.pos` DESCENDS during this phase (a truncation retry, below) --
            // the opposite direction from the `Scan` phase's own forward search -- so the motion
            // witness for THIS phase's own `More` return needs a `Descending` view of the SAME
            // field, snapshotted fresh at THIS call's own entry into the phase (never once for the
            // whole `step` call: a call that seeds AND scans, budget permitting, has the scan phase's
            // own ascent starting from wherever the seed phase left `self.pos`, not from this call's
            // very first byte -- see `scan_entry`, below, for that phase's own analogous snapshot).
            let seed_entry = self.pos;
            loop {
                // lazy: `new` has no cache access. Up to CTX_BEHIND extra bytes -- through
                // `Access::Peek`/`Charge::Bookkeeping` (`cache.warm()`'s own restructure-R3
                // equivalent): this is bookkeeping (look-behind), not user-visible payload content,
                // `CountScan`'s own precedent (batch 3 (2026-07-23), finding #7) -- charged to the
                // budget (batch-5 finding #5), never to progress. batch 4 (2026-07-24), finding #9:
                // widened from a single byte to up to CTX_BEHIND, enough to hold a whole UTF-8
                // character. May span more than one block when CTX_BEHIND straddles a block boundary
                // (unlike the old single-byte fetch, always confined to one).
                //
                // batch 5 (2026-07-27), finding #10-internal: a RETRY loop, not a one-shot fetch --
                // finding #1's own hazard, one function over. A real source's `size()` is a fixed
                // snapshot, never re-stat (`PreadSource::size`), so a source truncated below the
                // claimed size reads back short/empty at `self.pos` forever after. A short read here
                // CERTIFIES that `self.pos` is fiction; the loop below DESCENDS to the discovered
                // real point and retries there, budgeted like any other read, mirroring the
                // established truncation-descent shape (`SearchBackward`'s own wrap-leg terminal
                // check, this module).
                let ctx_start = self
                    .pos
                    .saturating_sub(crate::search::CTX_BEHIND.get() as u64);
                let payload_idx = self.pos / bs; // the block the FIRST payload iteration will fetch
                let mut ctx = Vec::with_capacity((self.pos - ctx_start) as usize);
                let mut p = ctx_start;
                // restructure R6: local, not `self.seed_block` -- that field now belongs to
                // `FwdPhase::Scan` alone (this struct's own `FwdPhase` doc comment), so a value only
                // ever crosses into it at the moment of certification, below; an INCOMPLETE retry
                // (this call runs out of budget before certifying) discards whatever this local held,
                // exactly as harmlessly as the pre-R6 code's own stale `self.seed_block` always was
                // (never consulted until `ctx_ready`, and index-filtered even then).
                let mut seed_block: Option<(u64, crate::cache::Block)> = None;
                // batch 5 fix round 2 (2026-07-27), review response, P3-1: latched the moment the
                // read breaks on the FIRST block it ever touches (`p == ctx_start`, still) AND that
                // block is truly, wholly empty (`Empty`, not merely a short `Short`) -- the
                // one case where the descent below may jump the whole block width at once instead of
                // creeping down `CTX_BEHIND` bytes per retry (the backward twin's own P3-4/P2-NEW
                // history, `SearchBackward::step`, this module: a block that has SOME real bytes,
                // just not reaching to `lo`, is NOT this case, and jumping past its own real content
                // would fabricate exactly the class of bug finding #10-internal fixes).
                let mut wholly_empty_first_block = false;
                // this bounded, at-most-`CTX_BEHIND`-bytes gather always runs to completion --
                // `read_at_unbounded` (charged, never refused) -- exactly as it did before this
                // read was charged at all: a payload read earlier THIS SAME step may already have
                // spent the step's own chunk, and this small, fixed-size look-behind must not be
                // starved by that (the old, unbudgeted code never was). Only the DESCENT below, which
                // can span many blocks for a badly truncated source, stays interruptible -- checked
                // explicitly via `Reader::out_of_budget` after charging its own `skip`, never by
                // refusing an ordinary gather read.
                while p < self.pos {
                    let want = (self.pos - p) as usize;
                    match reader
                        .read_at_unbounded(
                            p,
                            want,
                            crate::meter::Access::Peek,
                            crate::meter::Charge::Bookkeeping,
                        )
                        .await?
                    {
                        crate::meter::Fetched::Bytes {
                            got,
                            whole_block,
                            block_start,
                        } => {
                            if block_start / bs == payload_idx {
                                seed_block = Some((payload_idx, whole_block));
                            }
                            ctx.extend_from_slice(&got);
                            p += got.len() as u64;
                        }
                        crate::meter::Fetched::Short {
                            got,
                            whole_block,
                            block_start,
                            ..
                        } => {
                            if block_start / bs == payload_idx {
                                seed_block = Some((payload_idx, whole_block));
                            }
                            if got.is_empty() {
                                // a real, nonempty block whose own content simply doesn't reach `lo`
                                // -- NOT the wholly-empty case (that's the `Empty` arm below); nothing
                                // more real exists to fetch as context, but the block-jump shortcut
                                // does not apply.
                                break;
                            }
                            ctx.extend_from_slice(&got);
                            p += got.len() as u64;
                        }
                        crate::meter::Fetched::Empty { jump, .. } => {
                            // batch 12 (2026-07-29): the jump needs `BlockJump::Safe`, not merely
                            // "the read came back empty". A CONFIRMED-FINAL short block now
                            // certifies its end too, but real bytes sit below that position inside
                            // the same block -- dropping a whole `block_size` past them would
                            // narrow what this leg may report over genuine content.
                            wholly_empty_first_block =
                                p == ctx_start && jump == crate::meter::BlockJump::Safe;
                            break; // short/empty block; nothing more real exists to fetch as context.
                        }
                    }
                }
                if p < self.pos {
                    // a short read -- distinguishable from the legitimate `pos == 0` case (where
                    // this loop's own `while p < self.pos` never runs at all, so `p` starts already
                    // equal to `pos`, certifying trivially: file position 0 needs no real
                    // predecessor of its own) because THAT case never reaches this branch at all.
                    //
                    // batch 5 fix round 2 (2026-07-27), review response, P3-1: a WHOLLY empty first
                    // block jumps to that block's own start instead of `p` (== `ctx_start` here) --
                    // creeping down by only `CTX_BEHIND` bytes per retry through a long run of empty
                    // blocks (a source whose claimed `size()` overstates its real length by a wide
                    // margin) means one retry per few bytes of stale span instead of one retry per
                    // block. Unlike the backward twin's own terminal check, this descent needs no
                    // floor-respecting gate at all: `block_start = ctx_start / bs * bs <= ctx_start <
                    // self.pos` always holds (`ctx_start = self.pos.saturating_sub(CTX_BEHIND)` with
                    // `CTX_BEHIND > 0`, and floor division only shrinks further), so the jump is
                    // unconditionally a strict decrease -- there is no `origin`-style lower bound this
                    // descent must respect (P1, below, gates what may be REPORTED, never how far the
                    // descent itself may travel to CERTIFY). `skip` charges the skipped span as
                    // bookkeeping and rejects a non-decreasing target outright (the P1-NEW fixed-point
                    // hang this restructure makes unrepresentable, `NotMonotone`'s own doc comment).
                    let new_pos = if wholly_empty_first_block {
                        ctx_start / bs * bs
                    } else {
                        p
                    };
                    reader.skip(self.pos, new_pos)?;
                    self.pos = new_pos;
                    if reader.out_of_budget() {
                        // re-arm: NOT resolved yet -- staying in `FwdPhase::SeedCtx` (unchanged) is
                        // what lets the very next call re-enter this same retry from the
                        // already-descended `self.pos` (a fresh `begin_step` next call means this
                        // same check passes again until it doesn't).
                        let moved = crate::progress::Descending::new(seed_entry, 0)
                            .lower_to(self.pos)
                            .expect(
                                "`new_pos < self.pos` was just established (`skip` above would \
                                 have rejected it otherwise) -- this descent always strictly \
                                 lowers `self.pos` below `seed_entry`",
                            );
                        return Ok(SearchStep::More(moved, reader.progress_witness()?));
                    }
                    continue;
                }
                // certified: `self.pos` is now backed by a real, complete ctx read. `p` (== `self.
                // pos` exactly, by this loop's own invariant: `want` always clamps `p` to advance no
                // further) reaching here from `ctx_start` is this call's own fallback witness --
                // `seed_progress`'s own doc comment, above -- for the case the `Scan` phase below
                // gets no chance to make its own. This is the ONE place `self.phase` ever leaves
                // `SeedCtx`; the assert is `seed_progress`'s own one-shot lemma made loud (its
                // doc comment has the full argument; NOT `debug_assert!` -- this project's own
                // `--release` test profile disables those, and this check must hold in both).
                assert!(
                    !self.seed_fallback_minted,
                    "SeedCtx -> Scan ran more than once on the same SearchForward -- the one-way \
                     phase transition seed_progress's own soundness depends on has been broken"
                );
                self.seed_fallback_minted = true;
                // restructure R7: the window's base is established HERE, at the one moment the
                // gather is known complete (`p == self.pos`, this loop's own fall-through
                // condition), and never re-derived afterwards. `carry` is still empty at this
                // transition -- the seed phase runs strictly before any payload read -- so the
                // whole window is `ctx`, and `ctx_start` is where it genuinely begins.
                self.window_base = crate::search::hay::Abs(ctx_start);
                debug_assert!(
                    self.carry.is_empty(),
                    "SeedCtx -> Scan happens before this leg's first payload read, so the carry \
                     cannot yet hold anything for `window_base` to be wrong about"
                );
                self.ctx = ctx;
                self.phase = FwdPhase::Scan { seed_block };
                seed_progress = crate::progress::Ascending::new(ctx_start, u64::MAX).advance_to(p);
                break;
            }
        }
        // batch 5 fix round 2 (2026-07-27), review response, P1: the descent above may CERTIFY
        // `self.pos` at a point strictly below `self.origin` -- necessarily so, since descending
        // is the only way to discover where the source's real data actually ends, and that
        // discovery cannot itself be bounded by where the caller happened to ask this leg to
        // start. But `self.origin` still bounds what this leg may REPORT: `SearchForward::new`'s
        // own contract (`from` INCLUSIVE, "the first byte at-or-after which a match START
        // counts") means a match starting below `origin` is not this leg's to find, truncation
        // or not -- the identical principle `SearchBackward`'s own decision-1 already states for
        // `floor` ("a DECLARED boundary on what may be ACCEPTED as a match start, not a boundary
        // on what data exists"), applied here on the low side of a FORWARD leg instead. Once the
        // certified real end sits below `origin`, this leg's own range `[origin, real_end)` is
        // certifiably empty -- every real byte the source has is now known, and none of it is
        // at-or-past `origin` -- so `End` is the honest answer; the wrap leg (`origin` unchanged,
        // starting from the file's own true beginning) is what correctly finds a match down there
        // instead, honestly reporting `wrapped: true`. Without this, leg 1 reports a match START
        // below its own origin, which `document.rs`'s own exclusive-repeat contract depends on
        // never happening: repeat navigation (`origin = match_at + 1` each press) then recomputes
        // the SAME certified position below the SAME new origin forever, live-locking on a
        // `wrapped: false` result that never advances
        // (`search_next_forward_repeat_navigation_does_not_stick_on_a_stale_size`, document.rs,
        // pins the fixed behavior; `search_forward_terminal_check_does_not_report_below_its_own_
        // origin`, this module's own test, pins the scan-level shape directly).
        if self.pos < self.origin {
            return Ok(SearchStep::Done(SearchEnd::End));
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
        //
        // PHANTOM LINE (batch 4 (2026-07-24), finding #2): position `size` whose real
        // predecessor IS `\n` is not a line at all -- the trailing newline's own phantom
        // (docs/architecture.md's own doctrine for `goto_line`, unified here) -- so it can never
        // be a match, regardless of pattern.
        //
        // batch 5 (2026-07-27), finding #10-internal: this comment used to claim `self.ctx` "is
        // always real bytes at this site (never a sentinel)... this rejection is exact, not
        // approximate" -- FALSE as written: it did not cover `self.ctx` being SHORT or EMPTY (a
        // short-of-expectation seed used to leave `self.pos` uncertified and reach this exact
        // point anyway -- `phantom` false on an empty `ctx` admits a fabricated match). The retry
        // loop above is what makes the claim true NOW: this point is only ever reached once the
        // seed phase has certified (`self.phase` is `FwdPhase::Scan`), meaning the seed loop
        // reached `p == self.pos` without a short read --
        // `self.ctx` is a CERTIFIED, complete look-behind window ending exactly at `self.pos`,
        // never a partial one. That certification -- not merely "self.ctx happens to hold real
        // bytes" -- is what makes this rejection exact
        // (`search_forward_terminal_check_does_not_fabricate_a_match_at_a_stale_size` and its
        // sibling below pin both halves: the fabrication this fixes, and the legitimate
        // zero-width match at the real end that must still be found via the payload loop).
        if self.pos >= cache.size() {
            // restructure R4 (2026-07-27): A4's shape, `Hay::zero_width_at_high` -- the SAME
            // point probe A2 (`SweepAnalysis`) and A7 (`SearchBackward`'s wrap-leg terminal
            // check) use, structurally identical (a pure look-behind buffer, `lb == bytes.len()`
            // here always, by construction). `self.ctx` is a CERTIFIED, complete look-behind
            // window at this point (the retry loop above's own comment), never a partial one.
            // restructure R7 (batch 8 (2026-07-29)): the WHOLE tracked window, not `self.ctx`
            // with a base of `self.pos - ctx.len()`. Behaviour is unchanged today -- `carry` is
            // provably empty on every path that reaches this line, so the window IS `ctx` and the
            // old base was the same number. The derivation, since it is not obvious: the payload
            // loop's own guard is `while self.pos < size`, so a `More` return from it always
            // leaves `self.pos < size <= cache.size()`; the `take == 0` and `at_final && !at_eof`
            // paths both return terminal; and the seed phase only ever LOWERS `self.pos`. So
            // `self.pos >= cache.size()` here means this is the first entry into `Scan` on a leg
            // constructed at or past EOF, where nothing has been read into `carry` yet.
            //
            // Worth routing anyway, and not only for the guard: the old spelling was wrong in TWO
            // ways at once if that reasoning ever stopped holding -- the base would be off by
            // `carry.len()`, and `zero_width_at_high` probes the hay's own HIGH end, which for a
            // ctx-only hay is not `self.pos` at all. Both disappear when the hay is the window.
            let asm = Self::window_of(self.window_base, &self.ctx, &self.carry, self.pos);
            let hay = asm.hay().with_high(crate::search::hay::Edge::True);
            if hay.zero_width_at_high(&self.pattern) {
                return Ok(SearchStep::Done(SearchEnd::Found {
                    match_at: self.pos,
                    line_start: self.last_nl.filter(|&nl| nl < self.pos).map(|nl| nl + 1),
                }));
            }
        }
        // restructure R6: this phase's own resumption cursor only ever ASCENDS (the opposite
        // direction from the seed phase above) -- snapshotted fresh here, at THIS call's own
        // entry into the scan's own work, whether that is because we started the call already in
        // `FwdPhase::Scan` or because the seed phase above just certified and fell through
        // (`seed_entry`'s own doc comment explains why one snapshot per phase-entry is needed,
        // not one per whole `step` call).
        let scan_entry = self.pos;
        let FwdPhase::Scan { seed_block } = &mut self.phase else {
            unreachable!(
                "the block above always transitions to FwdPhase::Scan before falling through \
                 (or this call started already in it)"
            )
        };
        while self.pos < size {
            let idx = self.pos / bs;
            let lo = (self.pos % bs) as usize;
            let want = (size - self.pos) as usize;
            // reuse the seed's own block when `from` sat mid-block (so the seed and this,
            // this scan's very first payload fetch, land on the identical index) -- avoids the
            // redundant, PROMOTING second touch `seed_block`'s own doc comment describes. Free
            // in I/O terms (no cache touch, no `charged()` increment -- the ctx-seed already
            // paid for this block's own physical fetch, as bookkeeping); still counts as
            // progress (`Meter::record_progress`'s own doc comment).
            let reused = seed_block.take().filter(|(sidx, _)| *sidx == idx);
            // batch 9 (2026-07-29), finding #1: `certifies_end` carries out the ONE distinction
            // this loop used to throw away -- whether a zero-byte answer came from a block that
            // is WHOLLY EMPTY (the only thing that certifies a real end, `meter.rs`'s own P1-1
            // law) or merely from a legally SHORT block whose own content this scan has now
            // consumed. Collapsing the two made every short block look like EOF.
            let (slice, certifies_end) = if let Some((_, whole_block)) = reused {
                let available = whole_block.len().saturating_sub(lo);
                let take = available.min(want);
                let s = whole_block.slice(lo..lo + take);
                reader.record_progress(s.len() as u64);
                // batch 13 (2026-07-29), finding #4: **this arm must reach the SAME verdict the
                // read arm below would have.** It used to hardcode `false`, on the grounds that a
                // reused block is always nonempty (true -- `seed_block` is only ever set from the
                // ctx gather's `Bytes`/`Short` arms) and that only a wholly-empty block certifies
                // (no longer true: a short block whose end the SOURCE certified certifies too, and
                // `classify` mints exactly that `Empty` for it). So reuse silently threw away a
                // real EOF certificate: over an actual `abcde` behind a claimed size of 8, a `$`
                // search from 5 could not see the real end here, ran off the top, and reported the
                // match it eventually found as `wrapped: true` -- a wrap the file never needed.
                //
                // The certificate now rides WITH the block (`cache::Block`), which is what makes
                // the two arms agree by construction rather than by remembering to keep them in
                // step. Exhausting the block still only certifies when the source said so.
                (s, take == 0 && whole_block.ends_data())
            } else {
                match reader
                    .read_at(
                        self.pos,
                        want,
                        crate::meter::Access::Payload,
                        crate::meter::Charge::Payload,
                    )
                    .await?
                {
                    crate::meter::ReadOutcome::Bytes { got, .. } => (got, false),
                    crate::meter::ReadOutcome::Short { got, .. } => (got, false),
                    crate::meter::ReadOutcome::Empty { .. } => (bytes::Bytes::new(), true),
                    crate::meter::ReadOutcome::OutOfBudget => break,
                }
            };
            let take = slice.len();
            // **A LEGALLY SHORT BLOCK IS NOT THE END OF THE FILE** (batch 9 (2026-07-29),
            // finding #1). `BlockSource`'s own "up to `len`" contract (`source.rs`) lets a
            // conforming source answer with fewer bytes than asked for a reason that has nothing
            // to do with EOF, and the block-indexed cache then memoises that short answer -- so
            // `[self.pos, next_block)` is unreadable through this cache forever, while every
            // LATER block may still hold perfectly ordinary content. This loop used to treat the
            // resulting zero-byte answer as the genuine end and return `Exhausted`, so a
            // conforming source delivering one byte per 64-byte block reported "not found" for a
            // pattern sitting in plain sight at offset 64.
            //
            // The backward scans have always handled their own version of this by DESCENDING
            // past the unreadable span (charged via `Reader::skip`, so a huge traversal pends
            // across `step` calls instead of running unbounded inside one). This is that policy's
            // ascending twin, and it must clear the same state `SearchBackward::give_up` clears
            // for the same reason: the window on the far side of a gap is not contiguous with
            // the near side, so keeping `ctx`/`carry` across the jump would splice a match out of
            // bytes that were never read as neighbours -- the exact class restructure R7 makes
            // `Assembly` refuse, and `window_of`'s own assertion would fire on the next use.
            if take == 0 && !certifies_end {
                // `idx * bs` is this block's own start, so it is at most `self.pos` and cannot
                // overflow; only the `+ bs` can, in the last representable block (batch 10
                // (2026-07-29), P2). Saturating there is exactly right rather than merely safe:
                // `u64::MAX` fails the `< size` test below, which is the truthful answer -- there
                // IS no next block above the final one.
                let next_block = (idx * bs).saturating_add(bs);
                // **WHICH KIND OF SHORT READ WAS IT?** This is the question the old code never
                // asked, and neither answer is derivable from the short block alone:
                //
                //   (a) a CONFORMING source that simply answered with fewer bytes than asked
                //       (`source.rs`'s "up to `len`" contract) and whose later blocks still hold
                //       ordinary content -- finding #1's case, where terminating here reports
                //       "not found" for a match sitting in plain sight one block up;
                //   (b) a TRUNCATED source whose `size()` overstates its real data, which really
                //       does end inside this block -- the truncation policy's case, where the
                //       last real bytes sit unsearched in `ctx`/`carry` and the window must be
                //       accepted against a TRUE high edge.
                //
                // One bookkeeping peek at the next block tells them apart, which is the whole
                // reason this is decidable at all: an EMPTY block means nothing real exists at or
                // past its start (`source.rs`: "an empty buffer when `offset >= size`"), so the
                // file genuinely ended in the short block and (b) holds. Anything else is (a).
                // Charged, never refused -- the same treatment every other fixed-size bookkeeping
                // probe in this module gets, and for the same reason: a payload read earlier this
                // step may already have spent the chunk, and this one probe must not be starved
                // by that or the loop cannot decide at all.
                let next_has_real_data = next_block < size
                    && !matches!(
                        reader
                            .read_at_unbounded(
                                next_block,
                                1,
                                crate::meter::Access::Peek,
                                crate::meter::Charge::Bookkeeping,
                            )
                            .await?,
                        crate::meter::Fetched::Empty { .. }
                    );
                if next_has_real_data {
                    // **FLUSH BEFORE JUMPING.** Everything in hand is now final: a gap sits
                    // immediately above it, so the lookahead `safe_to` has been waiting for can
                    // never arrive contiguously, and a candidate left `Pending` here would be
                    // silently destroyed by the buffer clear below. This is the same one-final-
                    // accept-pass the certified-end branch further down performs, with one
                    // deliberate difference: `high` stays `Cut`, never `True`, and there is no
                    // EOF widening -- a gap is not the end of the file, and granting it a true
                    // edge would let `$`/`\b` confirm against a boundary that is an artefact of
                    // one short read (exactly the fabrication class the whole boundary-context
                    // model exists to forbid).
                    let asm = Self::window_of(self.window_base, &self.ctx, &self.carry, self.pos);
                    let accept_from = self.window_base + self.ctx.len() as u64;
                    let flush_bytes = asm.bytes().to_vec();
                    let hay = asm
                        .hay()
                        .with_high(crate::search::hay::Edge::Cut)
                        .with_accept(
                            asm.local_of(accept_from)
                                ..asm.local_of(crate::search::hay::Abs(self.pos)),
                        );
                    if let Some((s, _e)) = hay.first_verified(&self.pattern) {
                        let match_at = hay.to_abs(s).0;
                        if let Some(nl) = memchr::memrchr(b'\n', &flush_bytes[..s.0]) {
                            self.last_nl = Some(hay.to_abs(crate::search::hay::Local(nl)).0);
                        }
                        let line_start = self.last_nl.filter(|&nl| nl < match_at).map(|nl| nl + 1);
                        return Ok(SearchStep::Done(SearchEnd::Found {
                            match_at,
                            line_start,
                        }));
                    }
                    reader.skip_ahead(self.pos, next_block)?;
                    self.pos = next_block;
                    self.window_base = crate::search::hay::Abs(next_block);
                    self.ctx.clear();
                    self.carry.clear();
                    // a newline seen BELOW the gap is no longer a trustworthy line start for a
                    // match found above it -- the gap may hold newlines this scan will never see.
                    // `None` routes the caller to its own bounded backward hunt, which is the
                    // honest answer rather than a stale guess (`last_nl`'s own filter downstream
                    // already handles `None` this way).
                    self.last_nl = None;
                    if reader.out_of_budget() {
                        let moved = crate::progress::Ascending::new(scan_entry, u64::MAX)
                            .advance_to(self.pos)
                            .expect("skip_ahead rejects a non-advancing jump, so self.pos grew");
                        return Ok(SearchStep::More(moved, reader.progress_witness()?));
                    }
                    continue;
                }
            }
            if take == 0 {
                // the truncation policy (docs/budgeted_scanning.md, fix round F1 -- corrected
                // from the original batch-4 finding #14-internal fix, which returned `End`
                // directly here): `self.pos < size` (the loop's own guard) but the block came
                // back short/empty -- a source whose `size()` overstates its own real data
                // (`BlockCache::block`'s own doc comment: "short at EOF, empty past EOF").
                // Nothing more will EVER be READ here -- but this leg does NOT "have nothing
                // left it could ever find below here" (the ORIGINAL fix's own false claim,
                // corrected): the last up to `MAX_MATCH_LEN`-ish real bytes already sit in
                // `ctx` + `carry`, entirely unsearched, since `safe_to` never accepted anything
                // while waiting for lookahead that (unknown at the time) was never coming. A
                // FORWARD-direction loop runs ONE final accept pass over that already-in-hand
                // hay, forcing `at_final` true (the normal formulas can't derive this on their
                // own, since they compare against the CLAIMED `size`, which a truncated source
                // never reaches) -- mirroring the in-loop accept logic just below, specialized
                // for "no new bytes here" rather than duplicating the whole loop body.
                //
                // **`at_eof` is NOT forced with it** (batch 11 (2026-07-29), P1). This pass used
                // to grant `Edge::True` unconditionally, fabricating a file end out of a boundary
                // nothing certified. `meter.rs`'s own P1-1 law states the case exactly, with the
                // fixture that now pins it (`search_forward_reports_a_miss_not_a_fabrication_at_an_
                // uncertified_short_boundary`): for `b"helloXmo"` with block 0 answering 5 of its
                // own 8 bytes, position 5 is a real `'X'` and the block-indexed cache can never
                // re-ask block 0 for the gap it did not fetch -- so `hello$` matched an edge that
                // does not exist.
                //
                // Only `certifies_end` (a WHOLLY EMPTY read AT this position) earns `Edge::True`;
                // a short read earns `Cut`, and with it no EOF widening, since that widening
                // exists to admit a zero-width match at a REAL end. This is `docs/search.md`'s own
                // standing rule, not a new judgement: an unverified edge means "the belt drops the
                // candidate instead of trusting an unverified cut as real EOF -- a documented miss,
                // never a fabrication", and nav is required to AGREE with the highlighter on it
                // (the same document records that the contrary argument, "nav already fabricates on
                // this exact class of input", was itself retired as wrong by restructure R3).
                //
                // THE COST, stated because it is real and chosen: a genuinely truncated source
                // whose data ends inside a short block loses the `$` it legitimately has there --
                // the block-indexed cache cannot tell that from `helloXmo`'s gap holding real
                // bytes, so the rule took the miss in both. That trade was RETIRED by batch 12
                // (2026-07-29): the cache asks the source, so the two cases stopped being
                // indistinguishable and the `$` at a truncated end is found again --
                // `forward_scans_resolve_a_truncated_sources_real_end_exactly` (this module) is
                // the test that used to pin the loss and now pins the resolution, under its own
                // new name. The two `search_next_forward_*` stale-size tests still bound what may
                // be traded here. Recovering both at once
                // needs the capability `docs/search.md` names and this layer does not own: a way to
                // re-ask the source for a memoized short block's own gap.
                // restructure R4 (2026-07-27): A5's shape, `Hay::first_verified` over the accept
                // span widened by `Hay::accept_with_eof_widening` -- the SAME "+1 unless phantom"
                // widening every other terminal report in this crate now shares one
                // implementation of. `body_limit` defaults to the whole hay (`Hay::new`'s own
                // default): this pass forces `at_final`/`at_eof` true, so nothing past `hay`
                // itself was ever fetched to need a narrower one.
                // restructure R7 (batch 8 (2026-07-29)): the tracked window, and an accept span
                // named in FILE coordinates -- everything from where the carry begins up to the
                // read frontier.
                let asm = Self::window_of(self.window_base, &self.ctx, &self.carry, self.pos);
                let accept_from = self.window_base + self.ctx.len() as u64;
                let hay_bytes = asm.bytes().to_vec();
                let carry_len = hay_bytes.len();
                let hay = asm
                    .hay()
                    .with_high(if certifies_end {
                        crate::search::hay::Edge::True
                    } else {
                        crate::search::hay::Edge::Cut
                    })
                    .with_accept(
                        asm.local_of(accept_from)..asm.local_of(crate::search::hay::Abs(self.pos)),
                    );
                // the widening admits a zero-width match sitting exactly AT a real end; an
                // uncertified cut has no such position to offer, so it applies only where the edge
                // above is genuinely `True`.
                let hay = if certifies_end {
                    let accept = hay.accept_with_eof_widening();
                    hay.with_accept(accept)
                } else {
                    hay
                };
                if let Some((s, _e)) = hay.first_verified(&self.pattern) {
                    // restructure R5 (2026-07-28): `Hay::to_abs`, not a hand-derived `self.pos -
                    // carry_len + s.0` -- the exact shift-cancels-out shape `docs/architecture.md`
                    // named as the still-open half of the Abs/Local doctrine; this is one of the
                    // sites now routed.
                    let match_at = hay.to_abs(s).0;
                    if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes[..s.0]) {
                        self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
                    }
                    let line_start = self.last_nl.filter(|&nl| nl < match_at).map(|nl| nl + 1);
                    return Ok(SearchStep::Done(SearchEnd::Found {
                        match_at,
                        line_start,
                    }));
                }
                return Ok(SearchStep::Done(SearchEnd::End));
            }
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
            let lb = self.ctx.len();
            let carry_len = lb + self.carry.len();
            self.carry.extend_from_slice(&slice);
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
            // the boundary a reported match's own span (start AND end) must never cross, shared
            // by both branches below -- `self.ctx.len() + self.carry.len()`, now that `self.
            // carry` was just extended with `slice`. Restructure R4 (2026-07-27): this becomes
            // `Hay::body_limit`, below (and, on the peek branch, `resolve_bounded_terminal`'s
            // own).
            let payload_end = lb + self.carry.len();
            // fix round (2026-07-25), F4 -- closes the bounded-leg residual symmetrically with
            // decision 1 (`SearchBackward`'s own floor fix, `scan.rs`'s doc comment there): a
            // bounded leg reaching its own artificial `limit` (`at_final` but NOT `at_eof` --
            // the true file end) used to get NO trailing margin at all (`safe_to == read_end`),
            // so `$`/`\b`/`\B` were checked against hay's own artificial edge -- exactly the
            // "slice's own edge is unconditionally eligible" hazard this whole engine otherwise
            // guards against. `limit` bounds what may be ACCEPTED, not what data exists past it
            // -- the byte right after is ordinary file content, usually already sitting in the
            // cache (leg 1 read it) -- so peek up to `CTX_AHEAD` real bytes past `limit`,
            // purely for the regex's own lookaround, exactly mirroring decision 1's
            // "consultable, never reportable" rule: `payload_end` still caps what may be
            // accepted, so a match's own start can never land in the peeked bytes, and the
            // `e <= payload_end` guard downstream additionally keeps a match's own BODY (not
            // just an assertion) from silently consuming them. batch 5 (2026-07-26), finding #3:
            // `CTX_AHEAD`, not `CTX_BEHIND` -- this peek supplies LOOKAHEAD, and the two
            // constants were only ever numerically equal by coincidence (both 4, the same UTF-8
            // max character width, but justified on opposite sides of the boundary-context
            // model); a candidate whose own trailing assertion lands exactly at `payload_end`
            // (the only position the peek is ever consulted for) needs the FULL following
            // character there, the same requirement `safe_to` below now enforces mid-file.
            //
            // batch 6 (2026-07-28), finding #2: the peek itself moved to its own resolution,
            // below (`gather_peek`/`resolve_bounded_terminal`, resumable via `FwdPhase::Peek`) --
            // gathering it INLINE here and silently treating an `OutOfBudget` cutoff as "nothing
            // more to see" made this leg's own ANSWER budget-dependent (`Exhausted` at a low
            // budget, `Found` at a high one, on the IDENTICAL file), violating "budgets bound
            // read work, not answers" (`docs/budgeted_scanning.md`). This iteration's own payload
            // read already happened and is charged regardless of how the peek resolves, so
            // `self.pos` commits to `read_end` right here, before the peek -- `FwdPhase::Peek`'s
            // own doc comment states why nothing on this leg ever consults a stale `self.pos`
            // again once that is true, on either the fresh or a resumed call.
            if at_final && !at_eof {
                self.pos = read_end;
                let peek_end = Self::peek_end(cache, size);
                let mut p = size;
                let mut peeked = Vec::new();
                if Self::gather_peek(&mut reader, &mut p, peek_end, &mut peeked).await? {
                    return Ok(self.resolve_bounded_terminal(&peeked, payload_end, carry_len));
                }
                self.phase = FwdPhase::Peek {
                    p,
                    peeked,
                    payload_end,
                    carry_len_before: carry_len,
                };
                let moved = crate::progress::Ascending::new(scan_entry, u64::MAX)
                    .advance_to(self.pos)
                    .expect(
                        "take > 0 was already established (the take == 0 branch above always \
                         returns instead of falling through), so self.pos strictly advanced this \
                         iteration -- and scan_entry is this call's own entry position into the \
                         Scan phase, never past self.pos's own value going in",
                    );
                return Ok(SearchStep::More(moved, reader.progress_witness()?));
            }
            // restructure R4 (2026-07-27): A3's shape. `Hay::accept_with_eof_widening`
            // centralizes the PHANTOM LINE / "+1 unless phantom" widening (batch 4 finding #2,
            // extended to this in-loop site by the same finding's own fix round -- `hay_bytes`'s
            // own last byte, the byte just read, is always real content here, never `ctx`, since
            // `self.carry` was just extended with `slice` above); `Hay::body_limit` (set to
            // `payload_end`) centralizes the "the peek's own lookaround-only bytes can never
            // become an acceptable match START or BODY" rule -- moot on THIS path (batch 6
            // (2026-07-28), finding #2: the `at_final && !at_eof` branch that ever needed a peek
            // always returns above now), kept only for uniformity with `resolve_bounded_
            // terminal`'s own hay, which needs it live.
            // restructure R7 (batch 8 (2026-07-29)): the tracked window. `self.pos` has NOT yet
            // advanced past this iteration's `take` here (that happens below), while `self.carry`
            // already holds `slice` -- so the window's own end is `read_end`, and both the accept
            // span and the body limit are named against that rather than against a `payload_end`
            // length that has to be kept in step with two buffers by hand.
            let asm = Self::window_of(self.window_base, &self.ctx, &self.carry, read_end);
            let accept_from = self.window_base + self.ctx.len() as u64;
            let hay_bytes = asm.bytes().to_vec();
            let hay = asm
                .hay()
                .with_body_limit(asm.local_of(crate::search::hay::Abs(read_end)))
                .with_high(if at_eof {
                    crate::search::hay::Edge::True
                } else {
                    crate::search::hay::Edge::Cut
                })
                .with_accept(
                    asm.local_of(accept_from)..asm.local_of(crate::search::hay::Abs(read_end)),
                );
            let accept = hay.accept_with_eof_widening();
            let hay = hay.with_accept(accept);
            // batch 4 (2026-07-24), finding #10 / batch 5 (2026-07-26), finding #3:
            // `Hay::consumable_reach()` (`MAX_MATCH_LEN + CTX_AHEAD - 1`) -- this struct's own
            // ANCHOR CORRECTNESS doc comment has the full derivation of why a cap-length
            // candidate's own trailing assertion needs this much real lookahead past `read_end`
            // before it is truly confirmed leftmost, not merely eager-first-complete. batch 6
            // (2026-07-28), finding #2: the formula's own `at_final` (but not `at_eof`) branch
            // -- `read_end`, no margin -- is unreachable here now: that case always returns
            // above, before this line, so only the true-EOF and ordinary-advancing cases remain.
            let safe_to = if at_eof {
                // batch 13 (2026-07-29): `saturating_add`, not `+`. A latent overflow batch 10's
                // own `next_block_arithmetic_does_not_overflow_in_the_final_block` could not reach
                // while a short final block stayed short forever: `at_eof` is `read_end >=
                // cache.size()`, so a source claiming `u64::MAX` and delivering a FULL final block
                // arrives here with `read_end == u64::MAX` and panics the debug build on the `+ 1`.
                // Retiring the refill cap is what made that block completable, and with it this
                // line reachable. Saturating is the honest clamp: the frontier means "everything in
                // hand is safe at a true EOF", and it still says that for every file except one
                // whose data ends at `u64::MAX` exactly -- where the single position `u64::MAX`
                // stops being confirmable, in a file no source can physically produce.
                read_end.saturating_add(1)
            } else {
                read_end.saturating_sub(crate::search::hay::Hay::consumable_reach().get() as u64)
            };
            // `safe_to` is absolute; `Hay::leftmost_confirmed` wants its own frontier in `Local`
            // terms. `saturating_sub` (never negative, `usize`): a `safe_to` below this hay's own
            // `base` correctly floors to `Local(0)`, making every candidate `Pending` (`s.0 < 0`
            // can never hold) -- the same "nothing here is safe yet" verdict the absolute
            // comparison `match_at < safe_to` gave; a `safe_to` past this hay's own extent needs
            // no clamp on the high side (`s.0` is always `< hay_bytes.len()`, so `s.0 <
            // explored_to.0` already holds for every candidate once `explored_to` exceeds it).
            let explored_to =
                crate::search::hay::Local(safe_to.saturating_sub(hay.base().0) as usize);
            // fix round 2 (2026-07-25), R1: `Hay::leftmost_confirmed` retries past an overrunning
            // candidate internally (batch 3 (2026-07-23), finding #2's own LEFTMOST guarantee,
            // fix round F4's own R1 rationale for why "reject, don't stop" is required here: the
            // reviewer's exact fixture, `abcQ|bc` at `limit=3` -- "abcQ" (s=0) only completes
            // using the peeked byte, "bc" (s=1) does not, and a caller that stopped at the first
            // REJECTED candidate would lose "bc" whenever this leg's own outer `while self.pos <
            // size` has nothing left to retry with, `search_forward_finds_a_later_alternative_
            // when_the_leftmost_overruns_a_bounded_legs_own_limit`, this module's own test).
            // `last_nl` bookkeeping stays HERE, outside the type (`structural-accept.md` §6: a
            // line question, not an accept question) -- one `memchr` call, using whichever
            // candidate (or the whole hay, if none) the walk actually reached, reproduces the
            // SAME final value the old per-candidate incremental scan converged to: every
            // incremental call scanned a strictly GROWING prefix (`s` only ever increases across
            // retries), so only the LAST call's own outcome could ever change `self.last_nl`
            // (`restr-R4-report.md` has the general argument this collapses).
            //
            // batch 7 (2026-07-28), findings #1/#3: no `advancing` argument any more. K2 passed
            // `CutAdvances::from_final(at_final)` here so that a trailing-margin rejection could
            // DEFER rather than pay for a retry-and-rescan -- and finding #1 showed the deferral
            // discarded every later candidate in the hay, because the carry shrink below drops
            // exactly the bytes a deferred position was promised to be re-offered from. Both the
            // deferral and the retry it was avoiding are gone (`hay.rs`'s own doc comment on
            // `leftmost_confirmed` has the full account), and with them the whole hazard the K2
            // fix round needed two scan-level pins to guard: there is no longer a value here to
            // pass wrongly at one of two call sites.
            match hay.leftmost_confirmed(&self.pattern, explored_to) {
                crate::search::hay::Candidate::Found(s, e) => {
                    if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes[..s.0]) {
                        self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
                    }
                    // restructure R5 (2026-07-28): `Hay::to_abs` -- see A5's own identical note,
                    // above.
                    let match_at = hay.to_abs(s).0;
                    // only the fresh bytes actually needed to confirm the match count as
                    // consumed THIS step (ERRATUM 3c#2's rule). `read_at`/`record_progress`
                    // already credited the WHOLE `take` to `progressed` before this search ran
                    // (block-granular: the read already happened); give back whatever this
                    // match's own confirmation did not need. `e.0 > carry_len` does not always
                    // hold here: a candidate DEFERRED on an earlier iteration (found, but not yet
                    // safe) can be re-found on a later one using only bytes already inside the
                    // old carry -- already tallied into `progressed` back when it was first
                    // deferred (the fallthrough below ran then, crediting the FULL `take` that
                    // iteration) -- so `saturating_sub` correctly gives back the ENTIRE current
                    // `take` for those, rather than underflow or double-count them.
                    let needed = (e.0 as u64).saturating_sub(carry_len as u64);
                    self.meter
                        .give_back_progress((take as u64).saturating_sub(needed));
                    // a stale persisted `last_nl` sitting AT OR PAST `match_at` is not this
                    // match's own line start at all -- report `None` instead, letting the
                    // caller's own bounded backward hunt (`resolve_line_start_step`/
                    // `found_outcome_pending`, document.rs) resolve the true one, exactly as it
                    // already does when no newline has been seen at all.
                    let line_start = self.last_nl.filter(|&nl| nl < match_at).map(|nl| nl + 1);
                    return Ok(SearchStep::Done(SearchEnd::Found {
                        match_at,
                        line_start,
                    }));
                }
                // not yet safe: fall through and keep reading. The candidate is never lost --
                // `match_at < read_end` and `read_end - match_at <= MAX_MATCH_LEN + CTX_AHEAD -
                // 1` (else it would already be safe), so it sits within the trailing bytes the
                // carry-shrink below keeps; the whole-hay accept re-offers it (or a still
                // earlier alternative that only now has enough hay to complete) next iteration.
                crate::search::hay::Candidate::Pending(s) => {
                    if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes[..s.0]) {
                        self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
                    }
                }
                // batch 7 (2026-07-28): the `Deferred` arm this used to share retired with the
                // variant (`Candidate`'s own doc comment). `Exhausted` keeps the whole-hay
                // newline scan it always had -- no candidate was accepted anywhere in this hay,
                // so the last newline anywhere in it is the one a later match's own `line_start`
                // needs.
                crate::search::hay::Candidate::Exhausted => {
                    if let Some(nl) = memchr::memrchr(b'\n', &hay_bytes) {
                        self.last_nl = Some(self.pos - carry_len as u64 + nl as u64);
                    }
                }
            }
            self.pos += take as u64;
            // `progressed` already reflects the whole `take` (the read/reuse above credited it
            // in full); this iteration found no acceptable match, so nothing to give back.
            // shrink the carry to the last MAX_MATCH_LEN + CTX_AHEAD - 1 bytes for the seam
            // (batch 4 (2026-07-24), finding #10: MAX_MATCH_LEN, not MAX_MATCH_LEN - 1 -- this
            // struct's own ANCHOR CORRECTNESS doc comment; batch 5 (2026-07-26), finding #3: `+
            // CTX_AHEAD - 1` on top -- a match deferred near the carry's own low end needs its
            // OWN trailing assertion's FULL following character still retained here, not merely
            // one byte, the same margin `safe_to` above now requires before ever accepting it),
            // and update the look-behind context to match: the new `ctx` is up to CTX_BEHIND real
            // bytes immediately below the new carry front -- the OLD ctx's own tail (if the newly
            // dropped region alone is shorter than CTX_BEHIND) chained with whatever is actually
            // being dropped now, never fabricated (batch 4 (2026-07-24), finding #9: up to
            // CTX_BEHIND bytes, not one). `cut` can be 0 (carry hasn't grown past `keep` yet) --
            // the formula below degenerates correctly: an empty `dropped` leaves `combined` (and
            // so `ctx`) exactly as it already was.
            // restructure R4 (2026-07-27): `hay::Hay::consumable_reach()`.
            let keep = crate::search::hay::Hay::consumable_reach()
                .get()
                .min(self.carry.len());
            let cut = self.carry.len() - keep;
            let mut combined = std::mem::take(&mut self.ctx);
            combined.extend_from_slice(&self.carry[..cut]);
            let new_start = combined
                .len()
                .saturating_sub(crate::search::CTX_BEHIND.get());
            self.ctx = combined[new_start..].to_vec();
            self.carry.drain(..cut);
            // restructure R7: the ONE mutation that moves the window's low end. `combined` (the
            // old `ctx` plus the `cut` bytes leaving the carry's front) begins at `window_base`,
            // and the new `ctx` keeps only its last `CTX_BEHIND` bytes -- so the window now
            // starts `new_start` bytes further along. Everything above stays flush: the new `ctx`
            // ends where the drained `carry` now begins, and the window still ends at `self.pos`
            // (`window`'s own assertions check both on the next use).
            self.window_base = self.window_base + new_start as u64;
        }
        if self.pos >= size {
            Ok(SearchStep::Done(SearchEnd::End))
        } else {
            // `self.pos` only ever advances in this loop (`take == 0` always returns rather than
            // falling through, so `self.pos += take` here always adds a positive amount) --
            // comparing against `scan_entry` proves the SAME fact per-iteration tracking would,
            // without a second cursor threaded through every iteration (`scan_entry`'s own doc
            // comment, above). `None` whenever this loop ran zero iterations THIS call (the seed
            // phase's own unbounded ctx charge already exhausted the shared budget before `Scan`
            // ever got a read of its own -- `seed_progress`'s own doc comment) -- `seed_progress`
            // is the honest fallback for exactly that call.
            let moved = crate::progress::Ascending::new(scan_entry, u64::MAX)
                .advance_to(self.pos)
                .or(seed_progress)
                .expect(
                    "every call that reaches here charged something (progress_witness below \
                     would otherwise fail first) via either a Scan-phase read that also moved \
                     self.pos, or a seed-phase ctx gather that certified but left self.pos in \
                     place -- seed_progress covers the latter",
                );
            Ok(SearchStep::More(moved, self.meter.progress_witness()?))
        }
    }
}
impl Resumable for SearchForward {
    type Terminal = SearchEnd;
    async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        SearchForward::step(self, cache).await
    }
    fn progressed(&self) -> u64 {
        SearchForward::progressed(self)
    }
    fn lift_allowance(&mut self) {
        self.meter.lift_allowance();
    }
}
/// A resumable backward search for the previous match below `hi`, bounded
/// below by `limit`. Windows are read high to low; the carry keeps up to
/// `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes from the LOW end of the already-processed
/// (higher) region, prepended after the fresh slice: `hay = slice + carry`
/// (batch 4 (2026-07-24), finding #10: `MAX_MATCH_LEN`, not `MAX_MATCH_LEN - 1`
/// -- see this struct's own `$` paragraph, below; batch 5 (2026-07-26), finding
/// #3 widened it again, to `MAX_MATCH_LEN + CTX_AHEAD - 1` -- see this struct's
/// own trailing-Unicode-assertion paragraph, further below).
/// Unlike `SearchForward`, a match starting in the fresh slice never needs
/// bytes this scan hasn't read yet -- anything it could extend into lies
/// ABOVE it, in territory an earlier (higher) iteration already read into
/// carry -- so restricting starts to the fresh slice (`0..slice.len()`) is
/// safe here: nothing is ever deferred past the point it's resolvable, the
/// way excluding forward's carry was.
///
/// ANCHOR CORRECTNESS (`^`/`$`/`\b`/`\B`, `multi_line` -- `SearchPattern::compile`'s own doc
/// comment), using the boundary-context model (`search.rs`'s own top-level doc comment, batch 4
/// (2026-07-24)): `^`/`\b`/`\B` need real look-behind bytes ending at `lo - 1` (this iteration's
/// own low edge), the SAME reasoning as `SearchForward`'s own ANCHOR CORRECTNESS doc comment,
/// but a DIFFERENT mechanism: `lo` is a fresh position every iteration (a new block boundary
/// each time), never re-tested at a later hay-index the way forward's carry-shrink ages a
/// position toward 0, so there is nothing to chain -- `step` fetches up to `CTX_BEHIND` real
/// bytes ending at `lo` itself, EVERY iteration, rather than carrying them forward. Cheap in
/// practice: `cache.block` is a cache, and `[lo - CTX_BEHIND, lo)` is almost always inside the
/// SAME block just fetched above (a genuine second read only near a block boundary).
///
/// `$` is fully correct here, with no residual of its own (batch 3 (2026-07-23), finding #3's
/// own side effect, not something #3 set out to fix): `carry` no longer starts EMPTY -- `step`'s
/// own STRADDLE READ, below, seeds it with the real bytes in `[hi, hi + MAX_MATCH_LEN + CTX_AHEAD
/// - 1)` (capped at the true EOF; batch 5 (2026-07-26), finding #3 widened this range from a bare
/// `[hi, hi + MAX_MATCH_LEN)` -- see this struct's own trailing paragraph, below -- `$` itself
/// only ever needs the narrower `MAX_MATCH_LEN` portion of it, which is what the rest of THIS
/// paragraph reasons about) before the very first iteration ever runs, specifically so a match
/// starting below `hi` but extending past it can be CONFIRMED at all (this struct's own name for
/// the class, "a straddler"). That same seed happens to close the OLD gap here too: a sub-cap
/// match (length `L <= MAX_MATCH_LEN`) starting anywhere below `hi` has its own `$`-confirmation
/// byte at `s + L`, and `s + L < hi + MAX_MATCH_LEN` always holds (`s < hi`, `L <= MAX_MATCH_LEN`)
/// -- so that byte is ALWAYS real content the seed already read, never hay's own artificial edge.
/// Before this seed existed, `carry` only grew toward its own cap over several iterations, so a
/// match near the top of an EARLY slice could have its own `$` confirmation land exactly on
/// hay's own THEN-thin edge; this struct's own seed reads a FULL `MAX_MATCH_LEN + CTX_AHEAD - 1`
/// bytes past `hi` (`$` only needs the first `MAX_MATCH_LEN` of those), so even the worst-case
/// straddler (`s = hi - 1`, `L = MAX_MATCH_LEN`) has its own confirmation byte strictly inside
/// the seed's own read range -- and, since finding #10 widened the ONGOING per-iteration
/// carry-shrink cap to match (`MAX_MATCH_LEN`, not `MAX_MATCH_LEN - 1` -- the seed's own margin
/// was ALREADY right at the time; only the shrink lagged behind it), that same full margin held
/// at every LATER seam too, not just the seed's own first one -- finding #3, below, widens both
/// again, together, the same way. Unlike `SearchForward`'s own `$`, this struct has NO analogous
/// "bounded leg's own artificial limit" residual: the seed always reads real bytes up to the
/// TRUE file end (`cache.size()`), capped there regardless of what `hi` itself represents to the
/// caller, so `$` is never confirmed by anything but genuine content.
///
/// batch 5 (2026-07-26), finding #3: the margin above (one real byte past a worst-case
/// straddler's own end) is enough for `$` and ASCII `\b`/`\B`, which need only that one byte --
/// but a Unicode-aware `(?u:\b)`/`(?u:\B)` needs the FULL following character to decode, up to
/// `CTX_AHEAD` bytes, not one (`search.rs`'s own top-level doc comment: the identical "an
/// incomplete byte decodes as non-word regardless of the real character" hazard finding #9 already
/// closed on the LOOK-BEHIND side, below, now closed on this, the trailing side). `seed_end` and
/// the ONGOING per-iteration carry-shrink cap both widen to `MAX_MATCH_LEN + CTX_AHEAD - 1`, kept
/// in lockstep exactly as finding #10's own margin was.
/// `search_backward_unicode_word_boundary_needs_the_full_trailing_character_not_one_byte` (this
/// module's own test) pins the fixed defect: `a{4096}(?u:\b)` immediately followed by `é` (a
/// Unicode word character, so no boundary exists) used to fabricate `(?u:\b)` from `é`'s own
/// lone, invalid lead byte alone.
///
/// Look-behind (`^`/`\b`/`\B`) is a DIFFERENT story at the low end: `step` fetches up to
/// `CTX_BEHIND` real bytes ending at `lo`, every iteration -- and batch 4 (2026-07-24), finding
/// #4's own DECISION POINT is about what happens when that window would dip AT OR BELOW `floor`.
/// An EARLIER version of this fix excluded local start 0 (absolute `floor`) from that one
/// iteration's own accept range instead of reading below `floor` at all -- mirroring
/// `SweepAnalysis::give_up`'s own seam exclusion -- but a reviewer-caught regression disproved
/// it: `search_next_backward_self_hit_wraps_to_find_it_again` (document.rs) broke, because the
/// WRAP leg's own floor is `origin` -- exactly where the cursor's own self-hit match starts, an
/// ordinary and load-bearing case, not an obscure edge. The two seams are NOT the same shape:
/// `give_up` has NO bytes to offer at all (the failing region was never read, full stop); here
/// the bytes immediately below `floor` are perfectly real and already sitting in the cache --
/// `floor` is a DECLARED boundary on what may be ACCEPTED as a match start, not a boundary on
/// what data exists. DECISION: read the real bytes below `floor` too, exactly like anywhere
/// else -- `ctx`'s own fetch below has no floor-clamp at all. Only the ACCEPT range (`lo..`, the
/// `slice` bound, unchanged) stays floor-bounded, so nothing below `floor` can ever be
/// REPORTED, only CONSULTED for context -- the same "bookkeeping, not payload" distinction
/// `CountScan`'s own `warm()`-not-`block()` choice already draws elsewhere in this module, and
/// bounded at `CTX_BEHIND` bytes, never unbounded, so the module's own "never read... past the
/// point it was told to stop" rule stays true in the sense that actually matters (nothing below
/// `floor` is ever reportable, or promotes the working set past it). `search_backward_reads_a_real_byte_
/// below_its_own_floor_for_context`'s own name and fixture are updated to state this
/// precisely (now proving the POSITIVE case: the real byte below `floor` IS read and used), not
/// merely re-asserted under the old, disproven claim. Net residual: only when the real
/// predecessor sits MORE than `CTX_BEHIND` bytes below `floor` can a `^`/`\b`/`\B` match starting
/// near the floor still be missed -- the same general cap this model applies everywhere, not a
/// floor-specific one anymore.
/// `SearchBackward`'s phase (restructure R6, `structural-scan.md` §3.2). `straddle_seeded` /
/// `terminal_checked` die into the discriminant -- "which phase am I in" is not a pair of facts
/// stored alongside each phase's own leftovers, it is where this leg IS. `wrap_leg: bool` dies
/// into two named constructors (`new_leg`/`new_wrap_leg`, class 5's fix) that choose `Seed`'s own
/// `then` -- a plain leg's seed always resolves to `Scan` directly; a wrap leg's resolves to
/// `Certify` first. `terminal_pending` dies into NOTHING: being in `BwdPhase::Certify` at all,
/// across as many resumed calls as it takes, already IS the pending state -- there is no entry
/// condition to re-derive on resume (`self.hi == cache.size()` used to be that condition, and it
/// can never be true again once a descent has moved `self.hi` away from a `cache.size()` that
/// never catches up, `terminal_pending`'s own pre-R6 field doc comment), because the discriminant
/// itself is what a resumed `step` consults, never a snapshot of `self.hi` taken when the check
/// first committed. `seed_pos` moves into `Seed`'s own cursor.
///
/// **One deviation from the design's own literal sketch (`Scan` as a bare unit variant), argued
/// here rather than forced:** `Scan` carries `certified: Option<wrap_certified_top::
/// WrapCertifiedTop>`. The pre-R6 code's own hay construction (`step`'s own main loop) consulted
/// `self.wrap_leg && self.terminal_checked`
/// *after* the certify phase had already ended, to decide whether a wrap leg's own (possibly
/// DESCENDED) `hi` still deserves `Edge::True` even though it may no longer equal `cache.size()`
/// -- a fact about HISTORY (did this leg pass through `Certify` and resolve without a match),
/// which the phase discriminant alone cannot answer once execution has moved past `Certify` into
/// `Scan`. Losing it would be a real behavior change (silently downgrading a certified wrap leg's
/// own high edge to `Edge::Cut`, weakening `$`/`\b`/`\B` at exactly the position `Certify` existed
/// to certify) -- so it carries forward explicitly instead, set once at each of the two arrows
/// that ever produce a `Scan` (`Seed`'s own direct resolve for a plain leg: `None`; `Certify`'s
/// own resolve, either exit: `Some`), and never written again after that.
///
/// R6 shipped this as a bare `bool` -- batch 6 (2026-07-28), finding #1: a bool remembers only
/// THAT this leg passed through `Certify`, not WHERE `self.hi` stood when it did. `Scan`'s own
/// payload descent keeps lowering `self.hi` for entirely unrelated reasons (ordinary progress
/// through the file), and once `self.carry`'s own retention cap truncates, `self.hi.at() +
/// self.carry.len()` -- THIS iteration's own hay top -- falls below the position `Certify`
/// actually verified. A position-less bool cannot tell "still at the certified spot" from "long
/// since descended past it"; it just says yes forever, fabricating `Edge::True` (and the trailing
/// `$`/`\b`/`\B` assertions it licenses) onto whatever arbitrary cut the descent has since
/// reached (`search_backward_wrap_leg_certified_end_does_not_survive_carry_truncation`, this
/// module's own test, RED-reproduced verbatim before this fix: a fabricated match at 8191 in a
/// file with no real `a+\b` anywhere). Fixed: `Option<WrapCertifiedTop>` carries the EXACT
/// position (`wrap_certified_top::WrapCertifiedTop`, just below -- this module's own witness
/// type, a separate authority from `crate::meter::CertifiedEnd` on purpose: see that type's own
/// doc comment for why), and `step`'s own hay construction compares the CURRENT hay top against
/// THAT position specifically, never a stale "yes at some point" memory. A second pin closes the
/// arithmetic proof's own other half: `Edge::True` must still be re-earned on a LATER iteration,
/// not just the one `Certify` itself resolved on, for as long as `self.carry` has not yet
/// truncated (`search_backward_wrap_leg_certified_end_still_earns_true_on_a_later_iteration`,
/// added in this finding's own fix round after review found the claim unpinned -- a strictly
/// weaker implementation, comparing `self.hi.at()` alone against the witness and dropping the
/// carry term, passed the whole suite without it).
enum BwdPhase {
    /// The straddle-read seed (`step`'s own doc comment): pre-fills `carry` with lookahead above
    /// `hi`, budgeted and resumable. `cursor` starts at `hi` and only ever grows.
    Seed {
        cursor: crate::progress::Ascending,
        then: AfterSeed,
    },
    /// The wrap-leg terminal check only (`step`'s own doc comment): verifies `hi` against a
    /// possibly-stale `cache.size()` before admitting a zero-width match exactly at `hi`. Never
    /// entered by a plain leg (`new_leg`'s own `Seed { then: AfterSeed::Scan, .. }` skips straight
    /// to `Scan` instead).
    Certify,
    /// The payload descent. `certified`: this struct's own doc comment, above.
    Scan {
        certified: Option<wrap_certified_top::WrapCertifiedTop>,
    },
}
/// Which phase `Seed` resolves to once its own gather completes -- the constructor's choice
/// (`new_leg` -> `Scan`, `new_wrap_leg` -> `Certify`), not a fact re-examined at each transition.
#[derive(Clone, Copy)]
enum AfterSeed {
    Certify,
    Scan,
}
/// `BwdPhase::Certify`'s own witness type, nested in its own tiny module on purpose (batch 6
/// (2026-07-28), K1 fix round, P2-2) -- see `WrapCertifiedTop`'s own doc comment for the position
/// this records and why it is a SEPARATE authority from `crate::meter::CertifiedEnd`, not that
/// type reused. Kept out of `scan`'s own top-level scope specifically so the field can be private
/// to THIS module instead of to all ~6,900 lines of `scan` (a review probe found the top-level
/// version constructible, via a bare struct literal, from `fill_lines` -- an entirely unrelated
/// function ~1,100 lines away that happens to share the file): only `certify_resolved`, below, may
/// mint one; nothing outside this module can reach the private field a struct literal would need.
mod wrap_certified_top {
    /// The exact position `BwdPhase::Certify` verified before resolving into `Scan`. Captured
    /// once, at the moment of resolution, from `self.hi.at()` -- the position Certify's own retry
    /// loop had JUST finished verifying (a real, complete ctx read reaching `hi` with no
    /// shortfall), or the position the leg's own legitimate range ran out at (`hi < floor`, where
    /// the value is moot -- `Scan`'s own loop never runs there either, see `Certify`'s own comment
    /// at that exit).
    ///
    /// **A separate authority from `crate::meter::CertifiedEnd` on purpose, not merely a
    /// borrowed name.** That type is mintable only inside `meter.rs`, only from a single metered
    /// read that came back genuinely empty (`classify`'s own law, `meter.rs`'s module doc) --
    /// and this loop does not hold one at either resolution exit: the retry-loop-succeeds exit
    /// advances its own cursor only via `Fetched::Bytes` or a non-empty `Fetched::Short`, and
    /// neither arm binds an `end` -- both discard it, which is a CHOICE about what this phase
    /// needs, not a claim that no witness exists there.
    ///
    /// **(Batch 22 (2026-08-01) restates the reasoning this paragraph used to give, which was
    /// "`Fetched::Short` has carried no witness at all since `meter.rs`'s own P1-1 law" plus a
    /// structural-unreachability argument built on it. Batch 14 (2026-07-30) gave `Short` an
    /// `end` minted from `Block::ends_data`, so a certified short block whose end coincides with
    /// `hi` would in fact make a `meter::CertifiedEnd` available here. The conclusion is
    /// unchanged; the route to it is not an unreachability claim any more.)**
    ///
    /// Requiring that mint would still be requiring the WRONG fact, which is the durable half of
    /// the argument. What `Certify` needs to know before it may search is that it holds enough
    /// verified context -- *a complete `CTX_BEHIND`-byte look-behind read reached this exact
    /// position* -- and that is honestly weaker and simply different from "the source's data ends
    /// here". A position can satisfy either without the other: mid-file, `hi` has full look-behind
    /// and no end; over a source that dribbles, an end can be certified at a position this gather
    /// never reached. `WrapCertifiedTop` carries the fact `Certify` actually earns, under its own
    /// name, so it is never confused for the meter's own.
    ///
    /// **Minting discipline:** the ONLY intended constructor is `certify_resolved`, called
    /// exactly where `BwdPhase::Certify` resolves into `Scan` (`step`'s own two transition
    /// sites -- the `hi < floor` exit and the retry-loop-succeeds exit), each guarded by its own
    /// `assert!(self.carry.is_empty())` (batch 6 K1 fix round, P3-3) so a value only ever gets
    /// minted at a position `Scan`'s own first check can trust unconditionally. This module
    /// boundary makes a bare struct-literal forgery a compile error from anywhere else in
    /// `scan`; it does not (Rust privacy has no finer grain) restrict *this* module's own two
    /// call sites from each other -- that half is convention, stated here, not compiled.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct WrapCertifiedTop(u64);
    impl WrapCertifiedTop {
        pub(super) fn certify_resolved(hi: u64) -> Self {
            Self(hi)
        }
        pub(super) fn at(self) -> u64 {
            self.0
        }
    }
}
// dead on the lib target: see `SearchStep`'s own doc comment.
#[cfg_attr(not(test), allow(dead_code))]
pub struct SearchBackward {
    pattern: std::sync::Arc<crate::search::SearchPattern>,
    /// **The source's own real end, once any read on this leg has observed one** (batch 15
    /// (2026-07-30), finding #1). Batch 14 derived this from the block the CURRENT iteration
    /// happened to be working from, which is only ever right when the match and the end sit in one
    /// block: at a 2-byte block size over an actual `abcde` behind a claimed 8, the `e` and its
    /// certificate come from block 2 while `cde$` needs a window assembled two iterations later,
    /// by which time the witness was gone and the edge fell back to the claimed size. `carry`
    /// preserved the BYTES across those iterations and nothing preserved the fact about them.
    ///
    /// Safe to keep once learned, unlike `WrapCertifiedTop` (which asserts something about where
    /// `hi` currently is, and so must be re-earned): this is a fact about the FILE. What keeps it
    /// honest is not its freshness but the position check at every use -- it grants a true edge
    /// only where the assembly actually REACHES this offset.
    observed_end: Option<u64>,
    /// The `hi` this leg was CONSTRUCTED with -- the caller's own exclusive origin, which never
    /// moves, unlike `self.hi` (batch 14 (2026-07-30), finding #2). The two are the same only
    /// until the first descent, and the difference is exactly what distinguishes "this position is
    /// the cursor's own, and a zero-width match on it would be a self-hit" from "this position is
    /// where the SOURCE's data turned out to end, which this leg descended to and never chose".
    origin: u64,
    /// The main descent's own resumption cursor, shared by `Certify`'s own descent-retry and
    /// `Scan`'s own payload descent (both lower it directly) -- `floor` (formerly a separate
    /// `limit` field) lives inside it now (`structural-scan.md` §3.2's own sketch: "floor is
    /// inside the cursor").
    hi: crate::progress::Descending,
    carry: Vec<u8>,
    meter: crate::meter::Meter,
    /// A block fetched purely for `^` look-behind context (`step`'s own `ctx`, below), kept in
    /// case the VERY NEXT iteration's own payload turns out to want the identical block --
    /// which it structurally always does, whenever `lo` lands block-aligned (see `step`'s own
    /// derivation): reusing it there avoids a SECOND, promoting `cache.block()` touch of a block
    /// this scan's own bookkeeping peek already warmed a moment earlier, which is what actually
    /// caused batch 3 (2026-07-23), finding #7's "no-match backward scan leaves nearly every
    /// block protected" -- switching the context fetch alone to `cache.warm()` does NOT fix
    /// this by itself (a probationary HIT still promotes under `cache.block()`, regardless of
    /// which call warmed it first): the redundant second cache call must never happen at all.
    warmed: Option<(u64, crate::cache::Block)>,
    phase: BwdPhase,
    /// Fix round (R6 review P2-2): `seed_progress`'s own one-shot pin -- `SearchForward`'s own
    /// identically-named field has the full rationale. Independent of `certify_fallback_minted`
    /// (below): `Seed`'s own fallback and `Certify`'s own fallback are two SEPARATE one-shot
    /// events that can both legitimately fire once each, on different calls, over one object's
    /// life (a wrap leg visits `Seed` then `Certify` then `Scan`, never revisiting either).
    seed_fallback_minted: bool,
    /// `certify_progress`'s own one-shot pin -- `step`'s own doc comment on `certify_progress`
    /// has the full rationale, including the reviewer's own re-entrant-`Certify` construction
    /// this specifically guards against.
    certify_fallback_minted: bool,
}
/// The wrap-leg terminal check's own retry-loop descent target for a single iteration, given
/// whether that iteration's ctx read broke on a wholly empty first block (`p == ctx_start` AND the
/// block itself was empty, not merely short of `lo`) versus a partially-short one. Pulled out of
/// `step`'s own inline `if` so its termination property is testable in isolation, synchronously,
/// with no async/runtime involved at all -- batch 5 fix round 2 (2026-07-27), review response,
/// P1-NEW, a hang the round-1 fix round introduced and the second review caught, cannot safely be
/// reproduced by actually running the buggy version to its fixed point: the review's own evidence
/// records that doing so hangs the test binary outright (killed manually, not kept as a test).
///
/// Bug history: an earlier version of this function computed `(ctx_start / bs * bs).max(floor)`
/// UNCONDITIONALLY whenever `wholly_empty_first_block`, mirroring the payload-path descent's own
/// `block_start.max(floor)` (`step`'s own main loop, below). Safe THERE only because that loop's
/// own guard is `while self.hi > floor` (strict): the clamped value (`floor`) immediately fails the
/// guard, so the loop simply does not run again. THIS loop's own guard is `if self.hi < floor {
/// break }` (also strict) -- checked only at the TOP, inside an unconditional `loop {}` that
/// `continue`s back into the SAME body -- so landing EXACTLY on `floor` does not stop it: the next
/// iteration recomputes the identical clamped value again, `spent` stops growing (`self.hi -
/// new_hi == 0` at a fixed point), and the `spent >= chunk` escape can never fire either -- an
/// unbounded spin inside one `step()` call. Reachable in production, not just a unit test:
/// `document.rs`'s `wrapped_leg` builds backward leg 2 with `floor = origin`, nonzero for any
/// cursor off the file start; a file truncated under the pager while the cursor sits at least one
/// whole empty block above the new real end, then `N`, reaches it.
///
/// Fixed by gating the block jump on it landing STRICTLY past `floor`; otherwise falling back to
/// the unclamped, byte-exact `p` -- exactly what this same site did, unconditionally, before this
/// whole optimization existed (`85bc5f0`, findings #1/#3/#8). Both arms now strictly decrease
/// `self.hi`, so this always terminates (batch 5 fix round 3 (2026-07-27), review response,
/// P3-NEW: an earlier version of this derivation stated the fallback arm's own inequality
/// backwards -- `p <= ctx_start`, when `p` is initialized TO `ctx_start` and only ever GROWS
/// during the ctx-read loop above (`p += take`, `take >= 0`), so the correct direction is
/// `ctx_start <= p`; corrected below, along with the one thing both arms lean on that neither
/// previously named -- `self.hi > 0` -- so the chain is fully checkable, not merely asserted):
/// - block-jump arm: `block_start = ctx_start / bs * bs <= ctx_start < self.hi` always holds
///   (`ctx_start = self.hi.saturating_sub(CTX_BEHIND)` with `CTX_BEHIND > 0` gives `ctx_start <
///   self.hi` whenever `self.hi > 0`; floor division only shrinks further) -- the gate additionally
///   requires `block_start > floor`, so this arm can never merely repeat `self.hi` OR land
///   at-or-below `floor`.
/// - fallback arm: `ctx_start <= p < self.hi` -- `ctx_start <= p` because `p` starts at `ctx_start`
///   and only grows; `p < self.hi` is NOT derived through `ctx_start` at all, it is the caller's
///   own precondition for reaching this function (the enclosing `if p < self.hi` branch, `step`'s
///   own retry loop, below) -- entirely unconditional on `floor`, exactly the property `85bc5f0`'s
///   own unclamped descent always had, letting it pass below `floor` into the existing `hi < floor`
///   resolution the very next iteration, same as before this whole optimization existed.
///
/// Both arms' own `self.hi > 0` premise holds only because `step`'s own ctx-read loop (`while p <
/// self.hi`) never runs at all when `self.hi == 0` (the legitimate empty-file/floor-reached case),
/// so this function is never even called with `self.hi == 0` in the first place -- the branch
/// that calls it (`if p < self.hi`) is not taken there.
fn terminal_check_descent_target(
    wholly_empty_first_block: bool,
    ctx_start: u64,
    bs: u64,
    floor: u64,
    p: u64,
) -> u64 {
    if wholly_empty_first_block {
        let block_start = ctx_start / bs * bs;
        if block_start > floor {
            return block_start;
        }
    }
    p
}
/// What an endpoint probe needs to know about the LEG asking it. Grouped so the call sites pass one
/// value and the compiler checks they pass the same things -- three sites ask this question and the
/// history of this file is sites that asked it slightly differently (batch 16's own finding #1).
struct EndpointCtx<'a> {
    pattern: &'a crate::search::SearchPattern,
    observed_end: Option<u64>,
    certified: Option<&'a wrap_certified_top::WrapCertifiedTop>,
    size: u64,
    origin: u64,
}
#[cfg_attr(not(test), allow(dead_code))]
impl SearchBackward {
    /// **Does this assembly top out at the source's REAL end of data?** (batch 16 (2026-07-31),
    /// finding #1.) The one predicate the high edge and the zero-width endpoint probe both ask,
    /// extracted because they were asking it differently: the edge already accepted an accurately
    /// sized source (`top == size`), the probe did not, so `$` at the true end of an ordinary
    /// `abcd` -- searched backward from an origin past it -- found nothing while the very same
    /// position was granted a true edge one line below. Two spellings of one question is how they
    /// came apart; this is the question.
    ///
    /// Three ways to know, all position-checked: a wrap leg's own verified top, an END the SOURCE
    /// certified (`observed_end`, which a truncated source mints and an accurate one never needs),
    /// or the claimed size for a source that reaches it -- the ordinary, accurate case, where the
    /// claim IS the truth. Associated function rather than a method because its callers hold
    /// `&mut self.meter` (through `Reader`) across the same statement.
    fn at_real_end(
        top: crate::search::hay::Abs,
        observed_end: Option<u64>,
        certified: Option<&wrap_certified_top::WrapCertifiedTop>,
        size: u64,
    ) -> bool {
        let named = match certified {
            Some(end) => top == crate::search::hay::Abs(end.at()),
            None => top == crate::search::hay::Abs(size),
        };
        named || observed_end.is_some_and(|e| top == crate::search::hay::Abs(e))
    }
    /// **A zero-width match sitting exactly AT the top of what this leg can see** (batch 14
    /// (2026-07-30) finding #2, generalized by batch 16 (2026-07-31) finding #1). The payload
    /// loop's accept span is `[lo, lo + slice.len())`, which excludes its own top, so a bare `$` --
    /// or `^`, `^$`, `\B` on an empty file -- can only be found by probing that one position.
    ///
    /// Gated on the top being the real end AND strictly below the leg's own ORIGIN. The second gate
    /// is what preserves `new_leg`'s exclusive-`hi` contract: a zero-width match at the position the
    /// caller started from is the self-hit that contract exists to refuse
    /// (`search_backward_leg_one_does_not_admit_an_eof_self_hit`), while a position this leg
    /// DESCENDED to is one it never chose and an ordinary match below the origin.
    fn endpoint_match(
        pattern: &crate::search::SearchPattern,
        asm: &crate::search::hay::Assembly,
        observed_end: Option<u64>,
        certified: Option<&wrap_certified_top::WrapCertifiedTop>,
        size: u64,
        origin: u64,
    ) -> Option<u64> {
        let top = asm.end();
        (Self::at_real_end(top, observed_end, certified, size)
            && top.0 < origin
            && asm
                .hay()
                .with_high(crate::search::hay::Edge::True)
                .zero_width_at_high(pattern))
        .then_some(top.0)
    }
    /// **The endpoint probe, with the look-behind it needs to be allowed to answer** (batch 19
    /// (2026-07-31)). `Hay::zero_width_at_high` gates look-behind adequacy UNCONDITIONALLY
    /// (`lookbehind_ok`: `CTX_BEHIND` real bytes below the position, or a genuine BOF), so a probe
    /// handed a bare assembly can only ever match at offset 0. Batch 17's two out-of-loop probes
    /// were exactly that, which is why they answered for an empty file and nowhere else: over an
    /// accurate `b"a"` with a leg floored at 1, `$` at the real end (1) was refused for want of the
    /// one byte sitting right below it.
    ///
    /// So this reads that byte. Bounded (at most `CTX_BEHIND`), charged as bookkeeping, and through
    /// `read_at_unbounded` -- never refused, like every other fixed-size context gather in this
    /// module, so no resumability question arises on a path that is about to return a terminal.
    /// The cheap gates are checked FIRST: a position that could not answer anyway costs no read.
    ///
    /// Look-behind may reach below the leg's own `floor`. That is deliberate and long-established
    /// here (`search_backward_reads_a_real_byte_below_its_own_floor_for_context`): context is not a
    /// reportable answer, and refusing to look at it would make a bounded leg answer differently
    /// than an unbounded one over the same bytes.
    async fn endpoint_match_reading_lookbehind(
        ctx: EndpointCtx<'_>,
        reader: &mut crate::meter::Reader<'_>,
        at: u64,
        bs: u64,
    ) -> anyhow::Result<Option<u64>> {
        if !Self::at_real_end(
            crate::search::hay::Abs(at),
            ctx.observed_end,
            ctx.certified,
            ctx.size,
        ) || at >= ctx.origin
        {
            return Ok(None);
        }
        // walk down block by block until `CTX_BEHIND` bytes are in hand or BOF is reached, exactly
        // as `Document::edge_context`'s own look-behind walk does -- and stop the moment a block
        // fails to reach what has already been gathered, so nothing is ever spliced across a gap.
        let want_from = at.saturating_sub(crate::search::CTX_BEHIND.get() as u64);
        let mut lookbehind: Vec<u8> = Vec::new();
        let mut p = at;
        while p > want_from {
            let block_start = (p - 1) / bs * bs;
            let lo = block_start.max(want_from);
            let got = match reader
                .read_at_unbounded(
                    lo,
                    (p - lo) as usize,
                    crate::meter::Access::Peek,
                    crate::meter::Charge::Bookkeeping,
                )
                .await?
            {
                crate::meter::Fetched::Bytes { got, .. } => got,
                crate::meter::Fetched::Short { got, .. } => got,
                crate::meter::Fetched::Empty { .. } => break,
            };
            if got.len() as u64 != p - lo {
                break;
            }
            let mut joined = got.to_vec();
            joined.extend_from_slice(&lookbehind);
            lookbehind = joined;
            p = lo;
        }
        let asm =
            crate::search::hay::Assembly::anchored_at(crate::search::hay::Abs(p), &lookbehind);
        debug_assert_eq!(
            asm.end(),
            crate::search::hay::Abs(at),
            "the gather above only ever prepends bytes that reach `at`, so the assembly tops out \
             there by construction"
        );
        Ok(asm
            .hay()
            .with_high(crate::search::hay::Edge::True)
            .zero_width_at_high(ctx.pattern)
            .then_some(at))
    }
    /// Records what one read taught this leg about where the source's data ends (batch 15
    /// (2026-07-30), finding #1). An associated function over the field rather than a method, so a
    /// caller can hold `&mut self.phase` or `&mut self.meter` across the same statement.
    ///
    /// `Block::ends_data` is set only by an observed EMPTY read at exactly `block_start + len`
    /// (`cache::BlockCache::fetch`), so this is a fact about the source, never the claimed size.
    fn note_end(observed: &mut Option<u64>, block: &crate::cache::Block, block_start: u64) {
        if block.ends_data() {
            *observed = Some(block_start + block.len() as u64);
        }
    }
    /// The same record, from an `Empty` answer -- **and only from the shape of `Empty` that is
    /// exact** (batch 23 (2026-08-01), finding #2). `observed_end` feeds `at_real_end`, which
    /// grants `Edge::True` and with it a zero-width match at the position, so the field's
    /// invariant is equality, not a bound.
    ///
    /// `BlockJump` is what separates the two shapes (`meter.rs`'s own `Fetched::Empty`):
    /// `ContentBelow` comes from a certified SHORT block, so real bytes sit immediately below the
    /// position inside that same block and the source answered empty at exactly it -- exact.
    /// `Safe` comes from a WHOLLY empty block, which proves only that nothing exists at or above
    /// that block's own start; the position it names is an upper bound, and storing it here read
    /// as "the data ends exactly here" for a `$` to match against. No false match was constructed
    /// through it -- the surrounding contiguity gates were holding -- but the field would have
    /// been lying, and `at_real_end` has an accurate-source disjunct (`top == size`) that covers
    /// the ordinary block-aligned end without this. What is given up is the same narrow residual
    /// `fill_lines` documents for the same reason: a TRUNCATED source whose real end lands exactly
    /// on a block boundary.
    ///
    /// **Position 0 is exact even from a `Safe` answer**, and is kept for that reason rather than
    /// as an exception: nothing can sit below zero, so `real_end <= 0` IS `real_end == 0`. That is
    /// the same third premise `fill_lines` holds for its own first read, and it is load-bearing
    /// here -- an EMPTY file behind a claimed size reaches this arm at 0 and nowhere else, which
    /// is the whole of batch 16 (2026-07-31) finding #1's second half
    /// (`backward_finds_zero_width_matches_on_an_empty_file_behind_a_claimed_size`). Dropping it
    /// with the rest of the `Safe` case turned that test red, which is how the premise got written
    /// down instead of assumed.
    fn note_empty_end(
        observed: &mut Option<u64>,
        end: crate::meter::CertifiedEnd,
        jump: crate::meter::BlockJump,
    ) {
        if jump == crate::meter::BlockJump::ContentBelow || end.at() == 0 {
            *observed = Some(end.at());
        }
    }
    /// Leg 1 and every ordinary caller: `hi` is EXCLUSIVE (not including `hi` itself), and `hi`
    /// coinciding with the true file end is never treated as a genuine wraparound's own
    /// unconditional far end -- only ever a coincidence (the cursor already parked on a
    /// zero-width match there). `wrap_leg: bool` (batch 4 (2026-07-24), finding #1) used to be a
    /// parameter here that meant nothing at a call site; restructure R6 makes it the choice of
    /// constructor instead (`structural-scan.md` §3.2) -- see `new_wrap_leg`'s own doc comment
    /// for the other leg. `limit` is an INCLUSIVE lower bound on match starts: a match starting
    /// exactly at `limit` IS found -- asymmetric with `SearchForward::new`'s EXCLUSIVE `limit`
    /// (`Bound`'s own doc comment); a wrap-seam composer must account for that one byte. Always
    /// `Bound::Inclusive` -- passing `Exclusive` here panics (`Bound::inclusive`'s own doc
    /// comment). Reads at most `chunk` bytes per step (clamped to one so every step makes
    /// progress).
    pub fn new_leg(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        hi: u64,
        limit: Bound,
        chunk: usize,
    ) -> SearchBackward {
        Self::new_inner(pattern, hi, limit, chunk, false)
    }
    /// Leg 2 of a wraparound only: `hi` (conventionally the file's own true end, `size`) may
    /// itself hold a zero-width match, unlike `new_leg`'s own `hi` -- see the terminal-check
    /// phase this unlocks (`step`'s own doc comment). Otherwise identical to `new_leg`: `limit`
    /// is the same INCLUSIVE lower bound, `chunk` the same per-step cap.
    pub fn new_wrap_leg(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        hi: u64,
        limit: Bound,
        chunk: usize,
    ) -> SearchBackward {
        Self::new_inner(pattern, hi, limit, chunk, true)
    }
    fn new_inner(
        pattern: std::sync::Arc<crate::search::SearchPattern>,
        hi: u64,
        limit: Bound,
        chunk: usize,
        wrap_leg: bool,
    ) -> SearchBackward {
        SearchBackward {
            pattern,
            observed_end: None,
            origin: hi,
            hi: crate::progress::Descending::new(hi, limit.inclusive()),
            carry: Vec::new(),
            meter: crate::meter::Meter::background(chunk),
            warmed: None,
            phase: BwdPhase::Seed {
                cursor: crate::progress::Ascending::new(hi, u64::MAX),
                then: if wrap_leg {
                    AfterSeed::Certify
                } else {
                    AfterSeed::Scan
                },
            },
            seed_fallback_minted: false,
            certify_fallback_minted: false,
        }
    }
    /// Bytes consumed so far; spawn sites seed the progress channel with it.
    pub fn progressed(&self) -> u64 {
        self.meter.progressed()
    }
    /// ALL read work charged against this leg's own budget -- payload AND bookkeeping (straddle
    /// seed, terminal ctx, per-iteration ctx, skips), unlike `progressed()` (payload only). See
    /// `SearchForward::charged`'s own doc comment for the full reasoning; this struct's own twin.
    /// `Document::resolve_found` itself reads the shared `Meter` directly (`SearchLeg::
    /// into_meter`), never this accessor, but `search_next_backward_composed_budget_is_additive_
    /// charged_directly` (document.rs, fix round P2-2) does, to pin the composition's own
    /// additive claim with a permanent assertion rather than instrumentation that gets deleted.
    pub fn charged(&self) -> u64 {
        self.meter.charged()
    }
    /// Consumes this leg for its own Meter -- see `SearchForward::into_meter`'s own doc comment;
    /// identical composition seam, this struct's own twin.
    pub(crate) fn into_meter(self) -> crate::meter::Meter {
        self.meter
    }
    pub async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        let floor = self.hi.floor();
        // an out-of-range hi must degrade to the real bytes, mirroring
        // BackwardScan's philosophy (ERRATUM 3c#3).
        self.hi.clamp_to(cache.size());
        let bs = cache.block_size() as u64;
        // batch 4 (2026-07-24), finding #3: shared with the seed loop below -- a single `step`
        // call spends AT MOST the meter's own per-step chunk of TOTAL work, seed and main loop
        // together, not a chunk PER PHASE (a looser bound the seed used to get for free by not
        // charging this at all). `begin_step` resets that allowance here, before either phase, so
        // the main loop -- unchanged below -- simply continues spending whatever the seed left of
        // this SAME call's allowance.
        self.meter.begin_step();
        let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
        // restructure R6, the same discovery `SearchForward::step`'s own `seed_progress` records
        // (see its doc comment for the full reasoning): `Certify`'s own ctx-gather below is
        // UNBOUNDED (`read_at_unbounded`) and can, at a small enough `chunk`, exhaust the whole
        // step before `Scan` ever gets a read of its own, even though `self.hi` never moves
        // during a successful (non-descending) certify gather. `certify_progress` is the fallback
        // witness for exactly that.
        //
        // (Fix round, P3-2: an earlier version of this comment also cited "a composed,
        // already-partly-spent meter (`Document::resolve_found`'s own shared budget)" as a second
        // way to reach this arm -- false. `SearchLeg::into_meter` takes `self` by value, so a
        // search leg is fully consumed before `Document::resolve_found` ever calls `Meter::
        // impose_allowance`; the composed allowance goes to a freshly constructed `BackwardScan`
        // for the line-start hunt, never back into THIS object. `Step<T>`'s own doc comment,
        // top of this file, already states the correct version: composition is `BackwardScan`'s
        // own thing, one specific caller of it, never `SearchBackward`'s.)
        //
        // **The real termination argument (fix round, P2-2), stated, not implied.** The gather's
        // own local cursor (`p`, below) moving from `ctx_start` to `self.hi.at()` is real,
        // non-fabricated motion in its own right, but -- exactly as `SearchForward::step`'s own
        // `seed_progress` doc comment argues -- that fact alone does not close the livelock class
        // this fallback exists to prevent: `p` is recreated every call, so a hypothetical bug
        // that kept `Certify` re-entrant (never transitioning to `Scan`) could mint this SAME
        // honest witness forever. Reviewer-demonstrated, live: keep `certify_progress` minting
        // but skip the `self.phase = BwdPhase::Scan { .. }` assignment below, and `cargo test`
        // hangs -- `Progressed` real, `Charged` real, no bound on how many times either mints.
        // Adopted as `search_backward_re_entrant_certify_would_livelock_the_guard_catches_it`,
        // credited. What actually prevents it in this code is `self.phase` leaving `Certify`
        // (for `Scan`) exactly once and never re-entering -- so this arm runs at most once per
        // object lifetime. `certify_fallback_minted` (this struct's own field) is that "at most
        // once" claim as an `assert!` instead of an argument the reader must trust -- a plain
        // `assert!`, not `debug_assert!` (this project's own `--release` test profile disables
        // those; fix round, caught by a release-mode test failure).
        let mut certify_progress: Option<crate::progress::Progressed> = None;
        // restructure R6, the SAME discovery, one phase earlier: `Seed`'s own straddle-read is
        // BOUNDED (`read_at`, unlike `Certify`'s unbounded ctx-gather) but can still consume
        // EXACTLY up to `chunk` on its way to completing NORMALLY (reaching `seed_end` the same
        // step its own last byte happens to exhaust the budget) -- `Scan`'s own first check then
        // finds `out_of_budget()` already true, with `self.hi` never having moved at all THIS
        // call (a plain leg's `Seed` resolves straight to `Scan`, skipping `Certify` -- no
        // `certify_progress` either). `seed_progress` is the fallback witness for exactly that --
        // `certify_progress`'s own doc comment, just above, has the shared one-shot argument and
        // the reviewer-credited discriminator; `seed_fallback_minted` is this fallback's own
        // `assert!`-backed pin, the same mechanism.
        let mut seed_progress: Option<crate::progress::Progressed> = None;
        if matches!(self.phase, BwdPhase::Seed { .. }) {
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
            //
            // BUDGETED AND RESUMABLE (batch 4 (2026-07-24), finding #3): this used to read the
            // WHOLE `[hi, seed_end)` window -- up to `MAX_MATCH_LEN` bytes -- in one unbudgeted
            // burst before the budgeted main loop even started, uncharged to `spent` and with no
            // way to hand back `More`; a single interactive `search_next` call with a small
            // `chunk` could perform ~`MAX_MATCH_LEN / block_size` synchronous physical reads
            // instead of pending once the budget ran out (probe-verified: block_size 1, budget 1
            // -> ~4,097 reads from one call). `cursor` (`BwdPhase::Seed`'s own field, restructure
            // R6 -- formerly `self.seed_pos`, a struct field directly) persists across calls
            // exactly like `self.hi` does for the main loop, so a resumed seed picks up exactly
            // where the last one left off -- no cursor hand-off to get wrong.
            // batch 5 (2026-07-26), finding #3: `+ CTX_AHEAD - 1` on top of the worst-case
            // straddler's own `MAX_MATCH_LEN` margin -- one real byte past its own end is enough
            // for `$`/ASCII `\b`, but a Unicode-aware `(?u:\b)`/`(?u:\B)` needs the FULL following
            // character there, up to `CTX_AHEAD` real bytes, not one (this struct's own top-level
            // doc comment has the extended derivation).
            // restructure R4 (2026-07-27): `hay::Hay::consumable_reach()`.
            let seed_end = self
                .hi
                .at()
                .saturating_add(crate::search::hay::Hay::consumable_reach().get() as u64)
                .min(cache.size());
            // restructure R6: local, not `self.hi` -- `self.hi` never moves during this phase (the
            // seed's own cursor is `Seed`'s own `cursor`, entirely separate), so it is always a
            // valid, unchanging baseline for the "did the SEED cursor move THIS call" witness a
            // budget-exhausted return needs (`crate::progress`'s own module doc comment: the
            // motion witness tracks the scan's own resumption cursor, and during `Seed` that
            // cursor is `cursor`, not `self.hi`).
            let BwdPhase::Seed { cursor, then } = &mut self.phase else {
                unreachable!("this whole block is gated on matches!(self.phase, BwdPhase::Seed)")
            };
            let then = *then;
            let seed_call_entry = cursor.at();
            let mut out_of_budget = false;
            while cursor.at() < seed_end {
                let want = (seed_end - cursor.at()) as usize;
                let (got, warmed) = match reader
                    .read_at(
                        cursor.at(),
                        want,
                        crate::meter::Access::Peek,
                        crate::meter::Charge::Bookkeeping,
                    )
                    .await?
                {
                    // batch 15 (2026-07-30), finding #1: every arm records what it learned about
                    // where the source's data ends, into `self.observed_end`, instead of leaving it
                    // in a value that dies with this iteration. A block carrying `ends_data`
                    // certifies at its own last byte; an `Empty` certifies at its own read position
                    // ONLY when real bytes sit immediately below it. Both are then checked against
                    // the assembly's own top before they grant anything.
                    crate::meter::ReadOutcome::Bytes {
                        got,
                        whole_block,
                        block_start,
                    } => {
                        Self::note_end(&mut self.observed_end, &whole_block, block_start);
                        (got, Some((block_start / bs, whole_block)))
                    }
                    crate::meter::ReadOutcome::Short {
                        got,
                        whole_block,
                        block_start,
                    } => {
                        Self::note_end(&mut self.observed_end, &whole_block, block_start);
                        (got, Some((block_start / bs, whole_block)))
                    }
                    crate::meter::ReadOutcome::Empty { end, jump, .. } => {
                        Self::note_empty_end(&mut self.observed_end, end, jump);
                        (bytes::Bytes::new(), None)
                    }
                    crate::meter::ReadOutcome::OutOfBudget => {
                        out_of_budget = true;
                        break;
                    }
                };
                if let Some((widx, wb)) = warmed
                    && cursor.at() == self.hi.at()
                    && self.hi.at() > 0
                    && widx == (self.hi.at() - 1) / bs
                {
                    self.warmed = Some((widx, wb));
                }
                if got.is_empty() {
                    break;
                }
                self.carry.extend_from_slice(&got);
                let new_pos = cursor.at() + got.len() as u64;
                // discarded: this loop uses a snapshot-compare witness at the budget-exhausted
                // return below (`seed_call_entry`), not a per-iteration one -- see that site's
                // own comment.
                let _ = cursor.advance_to(new_pos);
                // charged as `Charge::Bookkeeping` (above) but deliberately does not count as
                // progress: the seed is pure lookahead, never an acceptable match start of its
                // own (this struct's own doc comment on `carry`, above) -- the same role
                // `SearchForward`'s own `ctx` look-behind byte plays there, which likewise never
                // counts toward ITS `progressed()`. A caller uses `progressed()` to seed the
                // remaining interactive budget for a FOLLOW-UP hunt (`Document::resolve_found`)
                // and to report progress; counting bytes the eventual match never needed to be
                // confirmed would misreport both the same way ERRATUM 3c#2 already forbids for
                // ordinary over-read payload (`search_backward_straddle_seed_bytes_do_not_count_
                // toward_scanned`, this module's own test, pins the rule).
            }
            // batch 4 (2026-07-24), finding #3's own fix round, F1 [P1] (reviewer-caught
            // regression -- fixed here, not merely disclosed): the loop above has TWO distinct
            // exits, and `cursor.at() < seed_end` alone cannot tell them apart. Budget
            // exhaustion (`OutOfBudget`) is genuinely resumable: the next `step` picks up at
            // `cursor`'s own position and makes real further progress -- hand back exactly the
            // shape every other resumable unit in this file uses for "more work remains", with
            // the seed's own progress already persisted in `self.phase`'s own cursor for the next
            // `step` (an interactive retry or the pending background task, `Document::
            // spawn_search_pending`/`spawn_wrapped_pending`) to resume from. Cancellation between
            // calls (dropping the pending nav) is clean for free: no lock is held across an
            // `.await`, and the only state that would need unwinding (`self.carry`, the cursor)
            // simply stays exactly as far as it got, discarded along with the rest of `self` when
            // the caller drops it (`search_next_backward_seed_pending_cancels_cleanly_mid_seed`,
            // document.rs).
            //
            // A short/empty read is NOT resumable the same way: it means fewer real bytes exist
            // than `cache.size()` claims (a source whose `size()` overstates its own real data --
            // `PreadSource` after the file is truncated out from under the pager, `BlockCache::
            // block`'s own doc comment: "short at EOF, empty past EOF") -- nothing more will EVER
            // arrive at this position, no matter how many more times `step` is called. An earlier
            // version of this comment called the two exits "structurally equivalent" -- false:
            // returning `More` for BOTH left `self.seed_pos` unmoved and `straddle_seeded` still
            // false on the short/empty path, so the very next call re-entered the identical state
            // and broke at the identical place -- an infinite `More` loop (`SearchBackward::
            // complete` loops on `More` forever; the background nav task spins, publishing
            // progress, never resolving), on a path the pre-batch-4 code completed correctly.
            // `search_backward_seed_reaches_a_terminal_step_when_the_source_over_reports_its_size`
            // (this module's own test) pins the fix with a bounded-iteration-count proof (not a
            // sleep): reaching the bound without a terminal step IS the livelock, deterministically.
            if out_of_budget {
                // this call's own read attempts either advanced `cursor` (charged, via the
                // `Bytes`/`Short` arms) or the very first one hit `OutOfBudget` immediately --
                // impossible on a fresh `begin_step()` with no allowance (`Meter::background`,
                // `chunk.max(1)`) unless a PRIOR resumption already exhausted it before this
                // call's own first read, which cannot happen here since `Seed` is always the
                // FIRST thing a call touches. So reaching this arm always followed at least one
                // charged, moved iteration.
                let moved = crate::progress::Ascending::new(seed_call_entry, u64::MAX)
                    .advance_to(cursor.at())
                    .expect(
                        "the first read of a fresh step is never refused (chunk.max(1), no \
                         allowance on a background meter), so OutOfBudget here always follows \
                         at least one prior charged, moved iteration this call",
                    );
                return Ok(SearchStep::More(moved, reader.progress_witness()?));
            }
            // this call's own fallback witness if neither `Certify` (a wrap leg only) nor `Scan`
            // below gets a chance to make one of its own (`seed_progress`'s own doc comment,
            // above) -- `None` when the seed ran zero iterations THIS call (already covered:
            // that means zero charge too, so `Scan`'s own budget is untouched and always has
            // something to report instead, the same argument `seed_progress`'s Forward-side twin
            // makes).
            // this is the ONE place `self.phase` ever leaves `Seed` (the `matches!` gate above
            // admits this block at most once per object -- there is no other exit that reaches
            // here). The assert is `seed_progress`'s own one-shot lemma made loud (its doc
            // comment, above, has the full argument), placed FIRST and unconditionally -- not
            // gated on the witness below turning out `Some` -- so no later mutation to this
            // block (an early return added after the witness is computed, say) can route around
            // it: reaching this line at all is already the one-shot event, whether or not this
            // particular call's own witness happens to be `Some` or `None`. NOT `debug_assert!`
            // -- this project's own `--release` test profile disables those, and this check must
            // hold in both (fix round: caught when a `#[should_panic]` test pinning this exact
            // invariant passed under `just test` but failed under `just test-release`).
            assert!(
                !self.seed_fallback_minted,
                "Seed -> {{Certify,Scan}} ran more than once on the same SearchBackward -- the \
                 one-way phase transition seed_progress's own soundness depends on has been \
                 broken"
            );
            self.seed_fallback_minted = true;
            seed_progress =
                crate::progress::Ascending::new(seed_call_entry, u64::MAX).advance_to(cursor.at());
            self.phase = match then {
                AfterSeed::Certify => BwdPhase::Certify,
                // a plain leg's own `hi` was never run through `Certify` -- `certified: None`
                // matches the pre-R6 `self.wrap_leg && self.terminal_checked` always being false
                // here (`wrap_leg` was false for exactly this constructor): there is no witness
                // to hold because there is nothing this leg ever certified.
                AfterSeed::Scan => BwdPhase::Scan { certified: None },
            };
        }
        if matches!(self.phase, BwdPhase::Certify) {
            // ZERO-WIDTH AT THE TRUE EOF (batch 3 (2026-07-23), finding #9): accept ranges are
            // half-open and exclude `hi` itself everywhere else in this struct (`0..slice.len()`,
            // below), but when `hi` IS the file's own true end (`cache.size()`, not merely a
            // bounded leg's own `limit`), position `hi` is a legitimate zero-width match start of
            // its own (`$` unconditionally holds there, being the true haystack end; other
            // zero-width assertions like `^` depend on the real byte just below it, fetched
            // fresh here exactly like the per-iteration `ctx` below). Checked ONCE resolved, per
            // generation, before the main loop even runs, since a zero-width match needs no
            // content at all to confirm -- only `^`/`$` context -- and must fire even when the
            // loop itself never would (an empty file, `hi == floor == 0`, is exactly `^$`'s own
            // fixture). A REAL, non-zero-width match can never start here regardless (nothing
            // exists to read at or past `hi`), so widening the accept range this one extra
            // position is never able to admit anything but a genuine zero-width match -- no
            // separate length check needed.
            //
            // this read shares `warmed` with the per-iteration `ctx` fetch below: `(hi - 1) /
            // bs` is the SAME block index the main loop's own FIRST iteration will fetch as
            // payload whenever this check does not itself return (`idx` there is computed
            // identically, since `hi` has not moved yet) -- without reusing it here too, this
            // read would independently re-trigger finding #7's own redundant-second-touch
            // promotion hazard (caught by `search_backward_context_peeks_do_not_promote_
            // untouched_blocks`, this module's own test, when this fix first landed without it).
            //
            // Only a genuine wrap leg's own `hi` (`new_wrap_leg`'s own `hi = size`, unconditionally
            // the wrap's own far end) ever reaches this phase at all -- an ordinary leg
            // (`new_leg`) resolves its own `Seed` straight to `Scan`, skipping `Certify` entirely
            // (`BwdPhase`'s own doc comment), so there is no gate left to state here: reaching
            // this arm at all already answers "is this a genuine wrap" (batch 4 (2026-07-24),
            // finding #1's own history: before the phase enum, `hi == cache.size()` alone could
            // not tell `search_next`'s own leg 1, coincidentally at the true end, from a genuine
            // wrap -- admitting the self-hit on leg 1 made an earlier, second match unreachable,
            // pinned by `search_backward_leg_one_does_not_admit_an_eof_self_hit`, this module's
            // own test, and `search_next_backward_self_hit_at_eof_now_requires_the_wrap`,
            // document.rs).
            //
            // batch 5 (2026-07-26), finding #1: `cache.size()` can be STALE (a real source's own
            // `size()` is a fixed snapshot for its whole life, `PreadSource::size`, never re-stat)
            // -- the ctx read just below is what actually CERTIFIES `hi`: if it comes back short
            // (real data ends before `hi`), `hi` is fiction, and admitting a zero-width match
            // there -- the pre-fix `ctx` stayed EMPTY on an all-empty read, `phantom` false on
            // empty, `rfind_starting_in(&[], 0..1)` matching `$` trivially -- fabricates a match
            // at a position that may not even exist
            // (`search_backward_wrap_leg_terminal_check_does_not_fabricate_a_match_at_a_stale_size`,
            // this module's own test: claimed size 100 over 2 real bytes used to return
            // `Found { match_at: 100 }`). The leg instead DESCENDS to the discovered real point and
            // retries there (the truncation policy's own shape, mirroring the payload-path
            // descent below, `lo_off >= hi_off`) until a ctx read comes back FULL (certifying the
            // new `hi`) or this leg runs out of its own legitimate range (`hi < floor`). Staying
            // in `BwdPhase::Certify` across as many resumed calls as that takes IS the pending
            // state now (`BwdPhase`'s own doc comment) -- a budget-exhausted retry finds its way
            // back into this SAME check on a later call simply because the discriminant still
            // says so, with no `self.hi == cache.size()` re-derivation to go stale
            // (`search_backward_wrap_leg_terminal_check_re_arms_to_find_the_real_end`, this
            // module's own test: claimed size 100 over 2 real, non-`\n`-terminated bytes -- the
            // genuine `$` at the real end, 2, must still be found, not silently dropped).
            loop {
                if self.hi.at() < floor {
                    // descended past this leg's own legitimate range entirely (the source's
                    // real data ends even before `floor`) -- nothing left to certify here.
                    // Resolved, not a re-armable state: the main loop below (`self.hi >
                    // floor`) will not run either, and `step`'s own terminal `self.hi <=
                    // floor` check honestly reports `End`. The witness's own position is moot
                    // here (`Scan`'s own loop guard, `self.hi > floor`, is already false too, so
                    // it never runs, and the high-edge check that would consult it is therefore
                    // unreachable) -- `Some` at the current (sub-floor) `self.hi.at()` for
                    // internal consistency with the sibling exit below, same as the pre-fix
                    // bool's own `true` here.
                    assert!(
                        self.carry.is_empty(),
                        "a wrap leg's own certified position is only sound if self.carry is \
                         still empty here (batch 6 K1 fix round, P3-3): new_wrap_leg's own \
                         hi == cache.size() collapses seed_end (min(hi + consumable_reach, \
                         cache.size())) to hi itself, so the seed phase's own gather loop never \
                         runs and never populates carry -- Certify itself never touches \
                         self.carry either, only its own local ctx, so it is still exactly what \
                         Seed left it as here. A wrap leg built with hi < cache.size() \
                         (new_wrap_leg's own doc says hi is only \"conventionally\" the file's \
                         own true end, never enforced) would violate this and let a later Scan \
                         iteration re-earn Edge::True at a coincidental, uncertified position \
                         once self.hi.at() + self.carry.len() happened to cross back down \
                         through the witness -- the exact fabrication class this fix closes, \
                         reopened one call site up. NOT debug_assert! -- this project's own \
                         --release test profile disables those, and this precondition must hold \
                         in both."
                    );
                    self.phase = BwdPhase::Scan {
                        certified: Some(wrap_certified_top::WrapCertifiedTop::certify_resolved(
                            self.hi.at(),
                        )),
                    };
                    break;
                }
                // batch 4 (2026-07-24), finding #9: up to CTX_BEHIND real bytes ending at
                // `self.hi`, not one -- the wrap's own far end is always the true file end
                // here (never bounded by `floor`, since a genuine wrap leg's own `hi` IS the
                // true EOF), so no floor-clamping is needed the way the per-iteration fetch
                // below needs it.
                let ctx_start = self
                    .hi
                    .at()
                    .saturating_sub(crate::search::CTX_BEHIND.get() as u64);
                let payload_idx = if self.hi.at() > 0 {
                    (self.hi.at() - 1) / bs
                } else {
                    0
                };
                let mut ctx = Vec::with_capacity((self.hi.at() - ctx_start) as usize);
                let mut p = ctx_start;
                // batch 5 fix round (2026-07-26), review response, P3-4: latched the moment
                // the ctx read breaks on the FIRST block it ever touches (`p == ctx_start`,
                // still) AND that block is truly, wholly empty (`Empty`, not merely a short
                // `Short`) -- the one case where the descent below may jump the whole block
                // width at once instead of creeping down byte by byte.
                let mut wholly_empty_first_block = false;
                // this bounded, at-most-`CTX_BEHIND`-bytes gather always runs to completion
                // -- `read_at_unbounded` (charged, never refused), for the identical reason
                // `SearchForward`'s own ctx-seed retry loop uses it: a payload read on an
                // EARLIER iteration of the main loop below may already have spent this same
                // step's own chunk, and this small, fixed-size look-behind must not be
                // starved by that. Only the DESCENT below stays interruptible, checked
                // explicitly via `Reader::out_of_budget` after charging its own `skip`.
                while p < self.hi.at() {
                    let want = (self.hi.at() - p) as usize;
                    match reader
                        .read_at_unbounded(
                            p,
                            want,
                            crate::meter::Access::Peek,
                            crate::meter::Charge::Bookkeeping,
                        )
                        .await?
                    {
                        crate::meter::Fetched::Bytes {
                            got,
                            whole_block,
                            block_start,
                        } => {
                            if block_start / bs == payload_idx {
                                self.warmed = Some((payload_idx, whole_block));
                            }
                            ctx.extend_from_slice(&got);
                            p += got.len() as u64;
                        }
                        crate::meter::Fetched::Short {
                            got,
                            whole_block,
                            block_start,
                            ..
                        } => {
                            if block_start / bs == payload_idx {
                                self.warmed = Some((payload_idx, whole_block));
                            }
                            if got.is_empty() {
                                break;
                            }
                            ctx.extend_from_slice(&got);
                            p += got.len() as u64;
                        }
                        crate::meter::Fetched::Empty { jump, .. } => {
                            // batch 12 (2026-07-29): the jump needs `BlockJump::Safe`, not merely
                            // "the read came back empty". A CONFIRMED-FINAL short block now
                            // certifies its end too, but real bytes sit below that position inside
                            // the same block -- dropping a whole `block_size` past them would
                            // narrow what this leg may report over genuine content.
                            wholly_empty_first_block =
                                p == ctx_start && jump == crate::meter::BlockJump::Safe;
                            break;
                        }
                    }
                }
                if p < self.hi.at() {
                    // batch 5 (2026-07-26), finding #1: a short read -- distinguishable from
                    // the legitimate `hi == 0` empty-file case, where `p` starts already equal
                    // to `hi` and this loop never runs at all -- means `self.hi` is fiction.
                    // Descend and charge the skipped span as bookkeeping (`skip`), exactly
                    // like the payload-path descent below.
                    //
                    // batch 5 fix round (2026-07-26), review response, P3-4: a WHOLLY empty
                    // first block jumps to that block's own start instead of `p` (==
                    // `ctx_start` here) -- creeping down by only CTX_BEHIND bytes per retry
                    // through a long run of empty blocks (a source whose claimed `size()`
                    // overstates its real length by a wide margin) means one retry per few
                    // bytes of stale span instead of one retry per block, the gap widening
                    // with block size. A block that has SOME real bytes, just not reaching to
                    // `lo`, is NOT this case (`wholly_empty_first_block` is false there):
                    // jumping past its own real content would fabricate exactly the class of
                    // bug finding #1 fixes, so that case keeps the byte-exact `p` descent.
                    //
                    // batch 5 fix round 2 (2026-07-27), review response, P1-NEW: the jump
                    // target is no longer unconditionally clamped to `floor` here -- see
                    // `terminal_check_descent_target`'s own doc comment (above this impl
                    // block) for why an unconditional clamp is a hang, not merely a missed
                    // optimization, and for the termination proof of what replaced it (and
                    // `skip`'s own rejection of a non-decreasing target, restructure R3's own
                    // structural guarantee of the identical property).
                    let new_hi = terminal_check_descent_target(
                        wholly_empty_first_block,
                        ctx_start,
                        bs,
                        floor,
                        p,
                    );
                    reader.skip(self.hi.at(), new_hi)?;
                    let moved = self.hi.lower_to(new_hi).expect(
                        "terminal_check_descent_target's own doc comment proves both its \
                             arms strictly decrease self.hi",
                    );
                    if reader.out_of_budget() {
                        // re-arm: NOT resolved yet -- staying in `BwdPhase::Certify`
                        // (unchanged) is what lets the very next call re-enter this same
                        // retry from the already-descended `self.hi`.
                        return Ok(SearchStep::More(moved, reader.progress_witness()?));
                    }
                    continue;
                }
                // certified: `self.hi` is now backed by a real, complete ctx read -- the phase
                // transition below (`BwdPhase::Scan { certified: Some(WrapCertifiedTop::
                // certify_resolved(self.hi.at())) }`) is what records this now, at the exact
                // position, not a separate position-less flag set here and consulted later
                // (batch 6 (2026-07-28), finding #1 -- `BwdPhase`'s own doc comment has the full
                // history of why a bare `bool` here was wrong).
                // restructure R4 (2026-07-27): A7's shape, `Hay::zero_width_at_high` -- the
                // SAME point probe A2 (`SweepAnalysis`) and A4 (`SearchForward`'s own entry
                // check) use (this method's own doc comment has the structural-identity
                // argument). `ctx` here is always real bytes (never a sentinel) once
                // certified, so the phantom rejection this centralizes is exact.
                // restructure R7 (batch 8 (2026-07-29)): anchored where the gather actually
                // STARTED (`ctx_start`), not at `self.hi` minus a length. Reaching this line
                // already means the gather completed (`p < self.hi.at()` descends and retries
                // above, and this is the only fall-through), so the two agree here today -- the
                // point is that they cannot silently stop agreeing: the day this retry loop grows
                // a path that reaches here on a partial, the assembly is based on where its bytes
                // came from rather than on where the loop hoped they ended.
                let asm = crate::search::hay::Assembly::anchored_at(
                    crate::search::hay::Abs(ctx_start),
                    &ctx,
                );
                let hay = asm.hay().with_high(crate::search::hay::Edge::True);
                if hay.zero_width_at_high(&self.pattern) {
                    return Ok(SearchStep::Done(SearchEnd::Found {
                        match_at: self.hi.at(),
                        line_start: None,
                    }));
                }
                // certified, no match here -- `Scan`'s own hay construction still needs to know
                // not just THAT this leg passed through `Certify` but the exact position it
                // resolved at (this enum's own doc comment): `self.hi.at()`, read fresh below,
                // unchanged since entering this retry attempt (`p` (== `self.hi.at()` exactly,
                // by this loop's own invariant: `want` always clamps `p` to advance no further)
                // reaching here from `ctx_start` is this call's own fallback PROGRESS witness if
                // `Scan` below gets no chance to make one of its own -- `certify_progress`'s own
                // doc comment, above; a different witness for a different question than the
                // `WrapCertifiedTop` position captured just below).
                //
                // This is the ONE place `self.phase` ever leaves `Certify` (every OTHER exit
                // from the retry loop above either `return`s a terminal `Found` or `return`s a
                // resumable `More` while STAYING in `Certify` for a future retry -- neither
                // reaches here). The assert is `certify_progress`'s own one-shot lemma made loud
                // (its doc comment, above, has the full argument, including the
                // reviewer-credited discriminator), placed FIRST and unconditionally -- not
                // gated on the witness below turning out `Some` -- so no later mutation to this
                // block (an early return added after the witness is computed, exactly the
                // reviewer's own construction) can route around it: reaching this point at all
                // is already the one-shot event. NOT `debug_assert!` -- this project's own
                // `--release` test profile disables those, and this check must hold in both.
                assert!(
                    !self.certify_fallback_minted,
                    "Certify -> Scan ran more than once on the same SearchBackward -- the \
                     one-way phase transition certify_progress's own soundness depends on has \
                     been broken"
                );
                self.certify_fallback_minted = true;
                certify_progress =
                    crate::progress::Ascending::new(ctx_start, u64::MAX).advance_to(p);
                // batch 6 K1 fix round, P3-3: the sibling exit's own identical assert, above in
                // this method -- see that call site's own message for the full derivation.
                assert!(
                    self.carry.is_empty(),
                    "a wrap leg's own certified position is only sound if self.carry is still \
                     empty here -- new_wrap_leg's own hi == cache.size() convention is what \
                     makes it so; see the sibling assert's own message for the full argument"
                );
                self.phase = BwdPhase::Scan {
                    certified: Some(wrap_certified_top::WrapCertifiedTop::certify_resolved(
                        self.hi.at(),
                    )),
                };
                break;
            }
        }
        // restructure R6: this phase's own resumption cursor (`self.hi`) only ever DESCENDS here
        // -- snapshotted fresh at entry into `Scan`'s own work THIS call (whether that is because
        // we started already in it, or `Seed`/`Certify` above just transitioned and fell
        // through), mirroring `SearchForward::step`'s own `scan_entry` (see its doc comment for
        // why one snapshot per phase-entry is needed, not one per whole `step` call).
        let scan_entry = self.hi.at();
        let BwdPhase::Scan { certified } = &self.phase else {
            unreachable!(
                "the two blocks above always transition to BwdPhase::Scan before falling \
                 through (or this call started already in it)"
            )
        };
        let certified = *certified;
        while self.hi.at() > floor {
            // checked BEFORE the reuse-or-fresh-read decision below, not left to `read_at`'s own
            // check alone: the `reused` path (a few lines down) never calls `read_at` at all --
            // it slices an already-in-hand block the PREVIOUS iteration's own look-behind ctx
            // fetch warmed via `read_at_unbounded` (charged, but never refused). Without this
            // explicit check, a run of block-aligned iterations can chain through `self.warmed`
            // indefinitely, each one "free" because the prior iteration's own ctx fetch happened
            // to pre-warm it -- silently defeating the whole per-step budget (caught by
            // `search_backward_complete_resolves_like_stepping`: a 40-byte file, chunk 8, resolved
            // in a single `step()` call with zero intermediate `More`s).
            if reader.out_of_budget() {
                break;
            }
            let idx = (self.hi.at() - 1) / bs;
            let block_start = idx * bs;
            // never read below `floor`, even mid-block -- the byte-exact
            // bound forward gets for free by clamping `size` before it ever
            // slices (without this, a match starting below `floor` could
            // still be read and reported, since block reads are otherwise
            // block-granular, not bounded to the caller's exact window).
            let lo = block_start.max(floor);
            let want = (self.hi.at() - lo) as usize;
            // reuse a block the PREVIOUS iteration's own `^` look-behind context already
            // fetched, when it turns out to be THIS iteration's own payload block too --
            // structurally always the case whenever `lo` landed block-aligned last time (see
            // `ctx`'s own derivation, below): the redundant second, PROMOTING `cache.block()`
            // touch this replaces is what actually caused batch 3 (2026-07-23), finding #7 --
            // switching the context fetch alone to `cache.warm()` cannot fix it by itself,
            // since a probationary HIT still promotes under `cache.block()` regardless of
            // which call warmed the entry first; the redundant second call must never happen.
            let reused = self.warmed.take().filter(|(widx, _)| *widx == idx);
            // `whole_payload_block` -- the UNSLICED block, kept alongside `slice` so the
            // per-iteration ctx fetch below (which may want a DIFFERENT offset within this same
            // block) can reuse it too, exactly as the old code's own `block.clone()` did.
            let (slice, whole_payload_block) = if let Some((_, whole_block)) = reused {
                let lo_off = (lo - block_start) as usize;
                let hi_off = ((self.hi.at() - block_start) as usize).min(whole_block.len());
                let s = if lo_off >= hi_off {
                    bytes::Bytes::new()
                } else {
                    let s = whole_block.slice(lo_off..hi_off);
                    reader.record_progress(s.len() as u64);
                    s
                };
                (s, whole_block)
            } else {
                match reader
                    .read_at(
                        lo,
                        want,
                        crate::meter::Access::Payload,
                        crate::meter::Charge::Payload,
                    )
                    .await?
                {
                    // (the `whole_block` each arm keeps is what carries `ends_data` to the edge
                    // decision below -- batch 14 (2026-07-30), finding #2.)
                    crate::meter::ReadOutcome::Bytes {
                        got,
                        whole_block,
                        block_start,
                    } => {
                        Self::note_end(&mut self.observed_end, &whole_block, block_start);
                        (got, whole_block)
                    }
                    crate::meter::ReadOutcome::Short {
                        got,
                        whole_block,
                        block_start,
                    } => {
                        Self::note_end(&mut self.observed_end, &whole_block, block_start);
                        (got, whole_block)
                    }
                    crate::meter::ReadOutcome::Empty { end, jump, .. } => {
                        Self::note_empty_end(&mut self.observed_end, end, jump);
                        (
                            bytes::Bytes::new(),
                            crate::cache::Block::uncertified(bytes::Bytes::new()),
                        )
                    }
                    crate::meter::ReadOutcome::OutOfBudget => break,
                }
            };
            // the REUSED path has no `ReadOutcome` to learn from, so it reads the same fact off
            // the block it reused (batch 15 (2026-07-30), finding #1 -- `observed_end` is the
            // leg's own persistent field now, not a per-iteration local, so a witness earned here
            // is still available to a window assembled several iterations later).
            Self::note_end(&mut self.observed_end, &whole_payload_block, block_start);
            if slice.is_empty() {
                // **batch 16 (2026-07-31), finding #1: probe the endpoint before descending past
                // it.** Nothing readable sits in `[lo, hi)`, and this branch used to go straight to
                // the descent (and eventually `End`) -- so a zero-width match AT `lo`, where `lo`
                // is the source's own real end, was never looked for. Over an empty file behind a
                // claimed size of 8, searching backward from 1, that is every one of `$`, `^`,
                // `^$` and `\B`: all four match at 0, and all four came back `Exhausted`.
                //
                // The assembly is deliberately bare -- this position has no look-behind, because
                // nothing was read here and `self.carry` lives ABOVE `hi` (splicing it in across
                // whatever gap this branch is about to descend is exactly what the carry-clearing
                // discipline elsewhere in this loop exists to prevent). An empty assembly anchored
                // at `lo` is the honest hay: `^` is true only when `lo` is genuinely 0, which for
                // an empty file it is.
                let ctx = EndpointCtx {
                    pattern: &self.pattern,
                    observed_end: self.observed_end,
                    certified: certified.as_ref(),
                    size: cache.size(),
                    origin: self.origin,
                };
                if let Some(at) =
                    Self::endpoint_match_reading_lookbehind(ctx, &mut reader, lo, bs).await?
                {
                    return Ok(SearchStep::Done(SearchEnd::Found {
                        match_at: at,
                        line_start: None,
                    }));
                }
                // the truncation policy (docs/budgeted_scanning.md, fix round F1 -- corrected
                // from the original batch-4 finding #14-internal fix, which returned `End`
                // directly here): an empty result here can ONLY be reached via a SHORT/EMPTY
                // block (with a full-sized block, `want` bytes from `lo` always exist whenever
                // `self.hi > floor`, this loop's own guard -- by construction, `floor < self.hi`
                // makes `want = self.hi - lo > 0`, and a full block has all of it). A short block
                // means a source whose `size()` overstates its own real data (a truncated file
                // under the pager, `BlockCache::block`'s own doc comment: "short at EOF, empty
                // past EOF") -- nothing more will EVER be READ at THIS position. But this leg
                // does NOT "have nothing left it could ever find below here" (the ORIGINAL fix's
                // own false claim, corrected): everything strictly below `self.hi` down to
                // `floor` is still real, readable, unsearched territory -- `self.hi` merely
                // started (or, via a wrap leg's own stale `cache.size()`, was CONSTRUCTED) above
                // the truncation point. A BACKWARD-direction loop DESCENDS past the fictional
                // territory instead of terminating: `self.hi` drops to `lo` (this block's own
                // start, clamped at `floor`, never below it), charged as bookkeeping (`skip`) so
                // a huge stale-size descent pends across `step` calls rather than running
                // unbounded in one. Strictly monotone (`self.hi > block_start` and `self.hi >
                // floor`, both already established, so `self.hi` strictly exceeds their max, i.e.
                // `lo`), so this terminates: the loop's own `while self.hi > floor` guard,
                // re-checked next iteration, naturally resumes real reading once the descent
                // reaches genuine content, or ends at `floor`.
                reader.skip(self.hi.at(), lo)?;
                // discarded: this loop uses a snapshot-compare witness at the post-loop return
                // below (`scan_entry`), not a per-iteration one -- see that site's own comment.
                let _ = self.hi.lower_to(lo);
                continue;
            }
            // up to CTX_BEHIND real look-behind bytes ending at `lo`, for `^`/`\b`/`\B` --
            // fetched fresh every iteration, not chained (this struct's own ANCHOR CORRECTNESS
            // doc comment). `lo == 0` needs no context at all (file position 0 is
            // unconditionally a valid line start -- an empty `ctx` here relies on the regex's
            // own "start of hay" default, correctly, since it IS the true start); the formula
            // below degenerates to that correctly on its own (`saturating_sub` floors at 0, same
            // as `lo` itself there), no separate branch needed.
            //
            // batch 4 (2026-07-24), finding #4's own DECISION POINT, reversed from an earlier
            // version of this fix (reviewer-caught regression: EXCLUDING local start 0 whenever
            // `lo == floor > 0`, mirroring the give_up seam's own exclusion, broke a real,
            // load-bearing case -- `search_next_backward_self_hit_wraps_to_find_it_again`
            // (document.rs): the wrap leg's own floor is `origin`, exactly where the cursor's
            // own self-hit match starts, so excluding that ONE position turned ordinary
            // single-match wraparound into a spurious `Exhausted`). This is NOT the give_up seam
            // shape: give_up has NO bytes to offer at all (the failing region was never read);
            // here the bytes immediately below `floor` are perfectly real and readable, `floor`
            // is merely a DECLARED boundary on what may be ACCEPTED, not a boundary on what
            // exists. Decision: read the real bytes below `floor` too, same as anywhere else,
            // no floor-clamp on the FETCH at all -- only the ACCEPT range (`lo..` for `slice`,
            // unchanged) stays floor-bounded, so nothing below `floor` can ever be REPORTED,
            // only CONSULTED for context, the identical "bookkeeping, not payload" distinction
            // `CountScan`'s own `warm()`-not-`block()` choice already draws elsewhere in this
            // module. Bounded (at most CTX_BEHIND bytes, never unbounded), so this does not
            // violate what the module's own byte-exact `limit` rule actually protects (nothing
            // below `floor` is ever reportable or promotes the working set past it) --
            // `search_backward_reads_a_real_byte_below_its_own_floor_for_context` pins this
            // deliberate read directly. batch 8 (2026-07-29), P2: this used to cite the renamed
            // `search_backward_never_reads_below_its_own_floor_for_context`, a name that no
            // longer exists anywhere in this tree -- it became the above
            // when its claim was inverted, so the citation pointed at a pin for the opposite
            // property. The module doc's own "never read, let alone report" wording, which this
            // sentence was arguing around, is likewise gone: it now says `limit` bounds what may
            // be REPORTED, so there is no longer a rule here to reconcile with.
            // **The DELIVERED top, batch 7 (2026-07-28), finding #2.** Everything below --
            // this hay's own extent, its high edge, and what may legally be concatenated into it
            // -- keys off the bytes this iteration ACTUALLY has, never off `self.hi`, the top it
            // set out to read to. The two coincide on every full read and diverge on a short one,
            // and `self.hi` is not lowered to compensate (neither read path does: the `read_at`
            // arm passes `Short`'s own `got` straight through, and the `warmed`-reuse arm clamps
            // its own `hi_off` to the block's real length). Before this, the difference was
            // invisible to the two places it mattered:
            //
            //   1. `search_hay` is assembled as `ctx ++ slice ++ carry`, and `carry` holds bytes
            //      from `self.hi` upward. When `slice` stops short of `self.hi`, those two pieces
            //      are NOT adjacent in the file, and concatenating them splices a match together
            //      across bytes that were never read. A conforming source holding
            //      "helloXXXWORLD!!!" whose first block answers only "hello" was searched as
            //      "helloWORLD!!!".
            //   2. the high edge below was granted `Edge::True` from `self.hi + carry.len()`,
            //      which on a short read names a position ABOVE the last byte in hand -- so `$`
            //      was decided against an edge this hay does not reach. Reviewer-measured on that
            //      same fixture: `FoundMatch { match_at: 0, wrapped: true }` for
            //      `helloWORLD!!!$` against a whole-file oracle of nothing at all.
            //
            // The gap cannot be closed by re-reading (the same block index answers the same short
            // slice from cache -- `meter.rs`'s own memo law), so the carry above it is simply no
            // longer usable: drop it. Nothing legitimate is lost, because every byte it could
            // have contributed to a match here would have had to reach across the gap to do so.
            // This descends, so the gap can never be re-crossed from below either.
            let delivered_top = lo + slice.len() as u64;
            if delivered_top < self.hi.at() {
                self.carry.clear();
                // **and the gap is charged, not merely noticed** (batch 8 (2026-07-29), P2).
                // `self.hi` drops to `lo` at the foot of this iteration however few bytes came
                // back, so `[delivered_top, self.hi)` is territory passed over without reading --
                // the same act the `slice.is_empty()` arm above already charges via `skip`, for
                // the same stated reason (a descent must pend across `step` calls rather than run
                // unbounded inside one). Charging it HERE, at the one site both read paths have
                // already converged on, is what covers the `warmed` reuse path -- the worse of
                // the two, since that path charges NOTHING for its own delivered bytes either
                // (`record_progress` alone, which never touches `charged()`), so a run of reused
                // short blocks descended entirely for free.
                //
                // `<`, not the `!=` this test used to spell: `delivered_top <= self.hi` always
                // (both read paths clamp -- `read_at` cannot return more than `want`, and the
                // reuse path clamps `hi_off` to the block's real length), so the two conditions
                // are equivalent, but `<` is the one that makes `skip`'s own strict-decrease
                // precondition evident at the call site instead of inferred from that clamp.
                reader.skip(self.hi.at(), delivered_top)?;
            }
            assert!(
                self.carry.is_empty() || delivered_top == self.hi.at(),
                "a non-empty carry must sit immediately above the bytes just delivered -- this is \
                 the contiguity premise `search_hay`'s own three-part assembly rests on, checked \
                 rather than assumed (batch 7 (2026-07-28), finding #2)"
            );
            let ctx_start = lo.saturating_sub(crate::search::CTX_BEHIND.get() as u64);
            // the block adjacent to the NEXT iteration's own payload (`(lo - 1) / bs`, since
            // `lo` becomes the next `hi`) -- stashed in `self.warmed` for that iteration to
            // reuse, exactly mirroring what the old single-byte fetch always cached (its own
            // ctx byte's block WAS that block, always, by construction).
            let next_payload_idx = if lo > 0 { (lo - 1) / bs } else { 0 };
            let mut ctx = Vec::with_capacity((lo - ctx_start) as usize);
            let mut p = ctx_start;
            while p < lo {
                let cidx = p / bs;
                let got = if cidx == idx {
                    // reuse THIS iteration's own payload block -- `whole_payload_block` is always
                    // nonempty here (an empty/short-empty payload read already `continue`d above).
                    if cidx == next_payload_idx {
                        self.warmed = Some((cidx, whole_payload_block.clone()));
                    }
                    let clo = (p - block_start) as usize;
                    let take = whole_payload_block
                        .len()
                        .saturating_sub(clo)
                        .min((lo - p) as usize);
                    whole_payload_block.slice(clo..clo + take)
                } else {
                    // charged, but never refused -- see the terminal check's own identical
                    // comment: a payload read earlier THIS iteration (or an earlier one) may
                    // already have spent this step's own chunk, and this small, fixed-size
                    // look-behind must not be starved by that.
                    let cwant = (lo - p) as usize;
                    match reader
                        .read_at_unbounded(
                            p,
                            cwant,
                            crate::meter::Access::Peek,
                            crate::meter::Charge::Bookkeeping,
                        )
                        .await?
                    {
                        crate::meter::Fetched::Bytes {
                            got,
                            whole_block,
                            block_start: cbs,
                        } => {
                            if cbs / bs == next_payload_idx {
                                self.warmed = Some((next_payload_idx, whole_block));
                            }
                            got
                        }
                        crate::meter::Fetched::Short {
                            got,
                            whole_block,
                            block_start: cbs,
                            ..
                        } => {
                            if cbs / bs == next_payload_idx {
                                self.warmed = Some((next_payload_idx, whole_block));
                            }
                            got
                        }
                        // real, in-bounds (>= floor) data; a short/empty read here shouldn't
                        // happen, but degrade gracefully (fewer ctx bytes) rather than loop --
                        // this is bookkeeping look-behind, never the leg's own reportable answer.
                        crate::meter::Fetched::Empty { .. } => bytes::Bytes::new(),
                    }
                };
                if got.is_empty() {
                    break;
                }
                ctx.extend_from_slice(&got);
                p += got.len() as u64;
            }
            // **THE ASSEMBLY** (restructure R7, batch 8 (2026-07-29)). This is the function that
            // shipped the contiguity class twice -- batch 7 finding #2 spliced a `carry` onto a
            // short `slice`, batch 8 P1 rebased a look-behind gather that never reached `lo` --
            // so it is the one that most needs the premise checked by a type rather than by a
            // reader. Each run is offered together with the position it was READ FROM, and
            // `Assembly` refuses any that does not touch what it already holds:
            //
            //   - `slice` anchors the assembly at `lo`: it is this iteration's payload, the run
            //     everything else is positioned relative to, and the only one that can never be
            //     misplaced (it was just read AT `lo`).
            //   - `ctx` was gathered ASCENDING from `ctx_start` toward `lo`, so a short answer
            //     leaves its hole at the TOP of the run -- `ctx_start + ctx.len()` falls below
            //     `lo` and the whole run is correctly refused. Batch 8's P1, now arithmetic.
            //   - `carry` holds bytes from `self.hi` upward, so offering it AT `self.hi` is
            //     exactly batch 7's `delivered_top == self.hi` check, expressed as the position
            //     the bytes actually occupy instead of a length comparison beside them.
            //
            // The carry is also STATE (the next iteration prepends `slice` to it), so a refusal
            // has to reach further than this hay: `Joined` drives both, which is the point --
            // one decision, made once by the type, rather than two hand-written checks that have
            // to agree with each other forever.
            //
            // Nothing legitimate is lost by either refusal. Re-reading cannot close a gap (the
            // same block index answers the same short slice from cache -- `meter.rs`'s own memo
            // law) and the descent never revisits territory from below. Dropping look-behind is
            // conservative in every direction that matters: an empty `ctx` leaves the base at
            // `lo`, so `low_is_true` stays false unless `lo` is genuinely 0 and `^` at the seam
            // is refused rather than fabricated, while `line_start` reports `None` -- which
            // `Document::resolve_found` answers with a real `BackwardScan` hunt over honestly
            // read bytes, not a guess.
            let mut asm =
                crate::search::hay::Assembly::anchored_at(crate::search::hay::Abs(lo), &slice);
            let _ = asm.extend_below(crate::search::hay::Abs(ctx_start), &ctx);
            if asm.extend_above(crate::search::hay::Abs(self.hi.at()), &self.carry)
                == crate::search::hay::Joined::Gap
            {
                // the same gap the charge above already paid for; clearing the FIELD keeps the
                // next iteration's own `next_carry` from splicing across it.
                self.carry.clear();
            }
            let search_hay = asm.bytes().to_vec();
            // restructure R4 (2026-07-27): A6's shape, `Hay::last_verified`. `body_limit` stays
            // this hay's own default (`search_hay.len()`), unconstraining -- `e <= search_hay.
            // len()` always holds by construction.
            //
            // restructure R5 (2026-07-28): BOTH edges now declared, per `structural-accept.md`
            // §3.5's own prescription (`restr-R4-review.md`'s own "Observation": this hay used to
            // default BOTH to `Cut` even at genuine BOF/EOF -- inert under R4, where `last_verified`
            // had no end condition to consult them; load-bearing now that it does). Low needs no
            // declaration of its own (`Hay::low_is_true` derives it from `base` automatically --
            // `lo - lb == 0` exactly when `ctx` reaches all the way back to the file's true start,
            // the same condition this site used to have no way to express). High: for a PLAIN leg
            // (`certified: None` -- never ran `Certify`), `delivered_top` (the top of what this
            // iteration actually holds -- batch 7 (2026-07-28), finding #2 replaced `self.hi`
            // here, which named the top it set out to reach instead) plus `self.carry`'s own
            // length (already-read content above it) reaching `cache.size()` is the ordinary
            // case, exactly the same test every other consumer's own ordinary accept path uses
            // (`SearchForward`'s `at_eof`, `SweepAnalysis`'s `report_to >= size`).
            //
            // A WRAP LEG (`certified: Some(end)`) does NOT use that test at all, on purpose: its
            // own `self.hi` starts life AS `cache.size()` itself (`docs/search.md`'s own
            // wrap-policy account), which is exactly the value batch 5 finding #1 proved can lie
            // for a truncated source -- `cache.size()` never re-stats, so once `Certify` has
            // corrected a stale claim downward, `cache.size()` itself keeps repeating the ORIGINAL
            // lie forever; comparing against it here would silently resurrect that exact
            // fabrication one level up (caught by `search_backward_wrap_leg_terminal_check_does_
            // not_fabricate_a_match_at_a_stale_size`, this module's own pre-existing test). `end`
            // -- the exact position `Certify`'s own retry loop verified, captured once at
            // resolution (`WrapCertifiedTop`'s own doc comment) -- is what actually answers "is
            // THIS hay's own top the genuine certified end" honestly, independent of whatever
            // `cache.size()` still claims.
            //
            // batch 6 (2026-07-28), finding #1: a bare `bool` here (`certified`'s own pre-fix
            // type) answered a DIFFERENT, weaker question -- "did this leg EVER pass through
            // `Certify`" -- true forever after, regardless of where `self.hi` has since wandered.
            //
            // THE SAFETY LEMMA (no re-earning after a drop), RE-DERIVED over the DELIVERED top
            // (batch 7 (2026-07-28), finding #2 -- the previous statement of this lemma was
            // correct arithmetic about the wrong quantity, which is exactly why it did not catch
            // the defect it was written to rule out). It wrote `T_N = hi_N + carry_N`: the top
            // this iteration set out to reach. What the hay actually holds is `D_N = delivered_
            // top_N + carry_N`, and the two differ by precisely the undelivered gap a short read
            // leaves behind -- so a lemma about `T` said nothing at all about the case that
            // mattered. Restated over `D`, with `lo`/`slice` as this iteration computes them:
            //
            // the next iteration has `hi_{N+1} = lo_N` (`lower_to(lo)`, below) and `carry_{N+1} =
            // min(cap, slice_N.len() + carry_N)` where `cap` is `Hay::consumable_reach()`. Its
            // own delivered top obeys `delivered_top_{N+1} = lo_{N+1} + slice_{N+1}.len() <=
            // hi_{N+1} = lo_N` UNCONDITIONALLY -- a read starts at `lo_{N+1}` and asks for at
            // most `hi_{N+1} - lo_{N+1}`, and can only come back at or under what it asked for,
            // whether full, short, or empty. Therefore
            //
            //     D_{N+1} = delivered_top_{N+1} + carry_{N+1}
            //             <= lo_N + min(cap, slice_N.len() + carry_N)
            //             <= lo_N + slice_N.len() + carry_N
            //              = delivered_top_N + carry_N = D_N.
            //
            // `D` is monotone non-increasing on every path this loop can take, with no premises
            // -- which is the whole of what "no re-earning" needs: once `D` drops strictly below
            // `end.at()` it can never rise back to meet it.
            //
            // THE WRAP-LEG COROLLARY (pinned equality, not just non-increase): for a wrap leg,
            // `D` stays EXACTLY `end.at()` across as many iterations as the carry's own cap
            // allows, correctly re-earning `Edge::True` there every time, not just on the first
            // check (`search_backward_wrap_leg_certified_end_still_earns_true_on_a_later_
            // iteration`, this module's own test, added in batch 6's own fix round after review
            // found the claim unpinned -- dropping the carry term passes the whole suite without
            // it). The equality needs both inequalities above to be tight: `min(cap, X) == X` (no
            // truncation yet) AND a FULL read (`delivered_top == hi`). Batch 6 argued the second
            // held because of `search_hay`'s own contiguity premise -- every byte from `lo` to
            // the certified top having been read, in order, exactly once -- and noted that a
            // short answer would break it. That premise is no longer left standing on argument:
            // a short read now clears the carry outright (`delivered_top`'s own derivation
            // above), so the assembly this loop performs cannot span a gap in the first place,
            // and the loss of tightness shows up where it belongs -- `D` drops, every later
            // iteration correctly sees `Edge::Cut`, and the outcome is a conservative miss rather
            // than a fabrication. Once the cap does truncate, the first inequality stops being
            // tight and `D` strictly decreases every subsequent iteration, never to return
            // (`search_backward_wrap_leg_certified_end_does_not_survive_carry_truncation`, this
            // module's own test, RED-reproduced a fabrication in that regime before batch 6's
            // fix; `search_backward_does_not_splice_a_match_across_an_undelivered_gap`
            // (document.rs) is the short-read half's own pin, RED-reproduced likewise).
            // **The zero-width match ON the real end** (batch 14 (2026-07-30) finding #2, batch
            // 16 (2026-07-31) finding #1). See `Self::endpoint_match` for the whole derivation --
            // including why the self-hit contract survives it, and why this used to require an
            // OBSERVED certificate and so missed the same position on an accurately sized file.
            if let Some(at) = Self::endpoint_match(
                &self.pattern,
                &asm,
                self.observed_end,
                certified.as_ref(),
                cache.size(),
                self.origin,
            ) {
                return Ok(SearchStep::Done(SearchEnd::Found {
                    match_at: at,
                    // resolved by the caller's own bounded backward hunt, exactly as the wrap
                    // leg's own terminal check leaves it.
                    line_start: None,
                }));
            }
            let hay = asm
                .hay()
                .with_high(
                    // restructure R7: `asm.end()` -- the top of what the assembly ACTUALLY
                    // holds, which is what this edge has always been trying to name. It
                    // supersedes batch 7 finding #2's `delivered_top + self.carry.len()`: that
                    // expression was correct only while the carry's own adjacency was separately
                    // guaranteed (finding #2's whole subject), whereas `end()` is that top by
                    // construction whether the carry joined or was refused.
                    // batch 14 (2026-07-30), finding #2: `observed_end` is the third way to earn
                    // this edge, and the only one that works for a PLAIN leg over a source whose
                    // `size()` overstates its data. A plain leg has `certified: None` (nothing ran
                    // it through `Certify`), so the fallback compared the assembly's own top against
                    // `cache.size()` -- a snapshot that a truncated source never reaches. Over an
                    // actual `abcde` behind a claimed 8, every backward search whose match ends at
                    // the real end (`$`, `e\b`, `cde$`) got `Edge::Cut` and answered `Exhausted`,
                    // while forward search found them: the two directions disagreeing about where
                    // the file ends.
                    //
                    // It is position-checked exactly like the wrap-leg witness beside it -- true
                    // only when the assembly tops out AT the certified position -- so it cannot
                    // grant a true edge anywhere else, and it is a real observation rather than a
                    // claim: `Block::ends_data` is minted only where the source itself answered
                    // empty. OR rather than a third `match` arm because both witnesses are genuine
                    // end observations and either one suffices; `cache.size()` stays the fallback
                    // for the ordinary, accurately-sized source that never earns one.
                    if Self::at_real_end(
                        asm.end(),
                        self.observed_end,
                        certified.as_ref(),
                        cache.size(),
                    ) {
                        crate::search::hay::Edge::True
                    } else {
                        crate::search::hay::Edge::Cut
                    },
                )
                // the accept span named in FILE coordinates -- this iteration's own payload,
                // `[lo, lo + slice.len())`. Whether `ctx` joined or was refused shifts every
                // local index under it, and `local_of` absorbs that automatically; the old
                // `Local(lb)..Local(lb + slice.len())` only stayed right because `lb` was
                // recomputed after the drop, in the correct order, by hand.
                .with_accept(
                    asm.local_of(crate::search::hay::Abs(lo))
                        ..asm.local_of(crate::search::hay::Abs(lo + slice.len() as u64)),
                );
            if let Some((s, _e)) = hay.last_verified(&self.pattern) {
                // restructure R5 (2026-07-28): `Hay::to_abs`, not the hand-derived `lo - lb + X`
                // shift this comment used to describe by hand (`docs/architecture.md`'s own
                // "still-open half" of the Abs/Local doctrine -- this is one of the sites routed).
                let match_at = hay.to_abs(s).0;
                // bounded: only this slice's own bytes before the match (plus, now, up to
                // CTX_BEHIND real look-behind bytes) are consulted; a `\n` further down (not yet
                // read) reports None rather than reading more just to resolve it.
                let line_start = memchr::memrchr(b'\n', &search_hay[..s.0])
                    .map(|nl| hay.to_abs(crate::search::hay::Local(nl)).0 + 1);
                // only the fresh bytes actually needed to reach the match's start count as
                // consumed this step (ERRATUM 3c#2's rule) -- `progressed` already reflects the
                // whole `slice.len()` (the read/reuse above credited it in full); give back what
                // this match's own confirmation did not need.
                // restructure R7: `match_at - lo`, the same quantity the old `s.0 - lb` computed
                // (this iteration's own payload bytes below the match) but named in file
                // coordinates, so it no longer depends on whether `ctx` joined the assembly.
                reader.give_back_progress(match_at - lo);
                return Ok(SearchStep::Done(SearchEnd::Found {
                    match_at,
                    line_start,
                }));
            }
            // `progressed` already reflects the whole `slice.len()` (the read/reuse above
            // credited it in full); this iteration found no acceptable match, so nothing to give
            // back.
            //
            // discarded: this loop uses a snapshot-compare witness at the post-loop return below
            // (`scan_entry`), not a per-iteration one -- see that site's own comment.
            let _ = self.hi.lower_to(lo);
            // shrink to the last MAX_MATCH_LEN + CTX_AHEAD - 1 bytes for the seam (batch 4
            // (2026-07-24), finding #10: MAX_MATCH_LEN, not MAX_MATCH_LEN - 1 -- this struct's
            // own ANCHOR CORRECTNESS doc comment; batch 5 (2026-07-26), finding #3: `+ CTX_AHEAD
            // - 1` on top -- a match near this slice's own low end needs its OWN trailing
            // assertion's FULL following character still retained in carry, not merely one byte),
            // keeping the LOW end -- closest to where the next, lower slice picks up. `ctx` (this
            // iteration's own look-behind bytes, if any) is deliberately EXCLUDED: it belongs to
            // territory BELOW the next iteration's own `hi` (`= lo`), not the already-read HIGH
            // carry -- the next iteration fetches its own look-behind fresh, per this struct's
            // own ANCHOR CORRECTNESS doc comment, never inherited.
            let mut next_carry = Vec::with_capacity(slice.len() + self.carry.len());
            next_carry.extend_from_slice(&slice);
            next_carry.extend_from_slice(&self.carry);
            // restructure R4 (2026-07-27): `hay::Hay::consumable_reach()`.
            let keep = crate::search::hay::Hay::consumable_reach()
                .get()
                .min(next_carry.len());
            next_carry.truncate(keep);
            self.carry = next_carry;
        }
        if self.hi.at() <= floor {
            // **Only AT the floor, never below it** (batch 18 (2026-07-31)). `clamp_to` degrades an
            // out-of-range `hi` to the source's real size (`ERRATUM 3c#3`), and that clamp does not
            // know about `floor` -- so an empty source under a leg bounded to `[50, 100)` arrives
            // here with `hi == 0`, and probing THERE reports a match at 0 for a caller that asked
            // about 50 upward. `floor` itself is legal (`new_leg`'s `limit` is an INCLUSIVE lower
            // bound: a match starting exactly there IS found); anything below it is outside the
            // window this leg was asked about, and the honest answer is `End`.

            if self.hi.at() < floor {
                return Ok(SearchStep::Done(SearchEnd::End));
            }
            // **batch 17 (2026-07-31), finding #1: the leg concludes NOTHING only after probing
            // its own endpoint.** The two probes inside the loop cover the positions the loop
            // reaches; this covers the one it never does. An ACCURATE empty file clamps `hi` to 0
            // (`ERRATUM 3c#3`'s degrade-to-real-bytes rule, at the top of this function), so
            // `hi == floor == 0` and the loop body does not run even once -- and `$`, `^`, `^$`,
            // `\B` all match at 0 on an empty file. Batch 16 fixed the empty file behind an
            // OVERSTATED claim, where `hi` survives the clamp and the loop does run; this is the
            // same file with an honest `size()`.
            //
            // Same gate as the other two (`Self::endpoint_match`), so the self-hit contract is
            // untouched: an empty file searched from origin 0 -- where `origin == size == 0` makes
            // this position the cursor's own -- is still refused here, and still found by the wrap
            // leg, exactly as `search_backward_finds_caret_dollar_on_an_empty_file` and
            // `search_next_backward_finds_caret_dollar_on_an_empty_file_via_a_deliberate_wrap`
            // pin it. Only an origin ABOVE this position reaches the probe.
            //
            // The assembly is bare: nothing was read at `hi`, and `self.carry` holds bytes from
            // ABOVE it, which are look-AHEAD for this position and never look-behind. An empty
            // assembly anchored here is the honest hay -- `^` is true only where the anchor is
            // genuinely 0.
            let ctx = EndpointCtx {
                pattern: &self.pattern,
                observed_end: self.observed_end,
                certified: certified.as_ref(),
                size: cache.size(),
                origin: self.origin,
            };
            let at_hi = self.hi.at();
            let mut reader = crate::meter::Reader::new(cache, &mut self.meter);
            let found =
                Self::endpoint_match_reading_lookbehind(ctx, &mut reader, at_hi, bs).await?;
            if let Some(at) = found {
                return Ok(SearchStep::Done(SearchEnd::Found {
                    match_at: at,
                    line_start: None,
                }));
            }
            Ok(SearchStep::Done(SearchEnd::End))
        } else {
            // `self.hi` only ever descends in this loop (`lower_to(lo)` above, always `lo <
            // self.hi.at()` by construction -- `lo = block_start.max(floor) < self.hi.at()`,
            // this loop's own guard) -- comparing against `scan_entry` proves the SAME fact
            // per-iteration tracking would (`scan_entry`'s own doc comment, above). `None`
            // whenever this loop ran zero iterations THIS call (`Seed`'s own straddle-read, or
            // `Certify`'s own ctx-gather, already exhausted the shared budget before `Scan` ever
            // got a read of its own -- `seed_progress`/`certify_progress`'s own doc comments) --
            // whichever of those actually ran THIS call is the honest fallback.
            let moved = crate::progress::Descending::new(scan_entry, floor)
                .lower_to(self.hi.at())
                .or(certify_progress)
                .or(seed_progress)
                .expect(
                    "every call that reaches here charged something (progress_witness below \
                     would otherwise fail first) via a Scan-phase read that also moved self.hi, \
                     a certify-phase ctx gather, or a seed-phase straddle-read that certified/\
                     completed but left self.hi in place -- certify_progress/seed_progress cover \
                     those",
                );
            Ok(SearchStep::More(moved, self.meter.progress_witness()?))
        }
    }
}
impl Resumable for SearchBackward {
    type Terminal = SearchEnd;
    async fn step(&mut self, cache: &crate::cache::BlockCache) -> anyhow::Result<SearchStep> {
        SearchBackward::step(self, cache).await
    }
    fn progressed(&self) -> u64 {
        SearchBackward::progressed(self)
    }
    fn lift_allowance(&mut self) {
        self.meter.lift_allowance();
    }
}
/// Why `fill_lines` stopped collecting bytes (restructure R3, replacing a 4-cause bool that had
/// thrown the distinction away -- `structural-budget.md` §2.2c). The `End` witness THREADS to
/// the highlighter (`Document::viewport`): a buf whose own end carries one is a VERIFIED true
/// end, never merely "this call's own scan budget happened to stop here".
#[derive(Debug)]
pub(crate) enum FillOutcome {
    /// Filled the requested row count; more of the file may exist past `buf`.
    Rows,
    /// **Stopped without a certificate.** Three ways in, and the name only describes the first
    /// (batch 22 (2026-08-01) -- this used to read "hit the byte budget before finishing `rows` or
    /// reaching the source's real end," which is wrong about the third and the most ordinary case
    /// there is):
    /// 1. the byte `budget` filled before `rows` newlines were seen;
    /// 2. a short read stopped the walk and nothing certified its position -- the conservative miss
    ///    the `End` variant's own P1-1 law requires;
    /// 3. the block resolved because the source's own CLAIMED SIZE ends inside it
    ///    (`BlockCache::is_resolved`'s fourth condition). Nothing was read past it and nobody asked
    ///    the source about it, so there is no certificate to carry -- yet for an accurately sized
    ///    file this is exactly where the data ends, and it is how EVERY file whose end is not
    ///    block-aligned finishes.
    ///
    /// So `Budget` means "this walk cannot prove where it stopped", not "the budget ran out".
    /// Case 3 is answered one layer up, by `Document::viewport`'s own `buf_end == self.size`
    /// disjunct, and deliberately not by a fourth variant here: that check is needed anyway for
    /// `Rows` (a viewport that fills its rows exactly at the claimed end is at a true EOF too), so
    /// a variant would make the claimed-size fact derivable two ways -- the shape batch 13's
    /// finding #4 came from, and the reason `ReadOutcome::Short` drops the witness it could carry
    /// (`docs/budgeted_scanning.md`'s four-case taxonomy, case 3).
    Budget,
    /// The source's real data ends exactly at `buf`'s own end -- certified by an OBSERVED EMPTY
    /// read at that exact position. That observation reaches this function two ways (batch 21
    /// (2026-07-31) -- this comment used to name only the first, and denied the second outright):
    /// directly, as `Fetched::Empty`; or carried on a `Fetched::Short` whose block the cache had
    /// already asked about, `Block::ends_data` set by the same kind of empty read one layer down.
    /// `meter.rs`'s own P1-1 law is unmoved by that -- what may never certify is a merely-short
    /// LENGTH, since the block-indexed cache cannot verify whatever might sit in the gap between a
    /// short answer's own length and the next block's own start unless it goes and ASKS. This
    /// function never consults `BlockCache::size()` to decide when to stop (unlike its
    /// pre-restructure self).
    ///
    /// **Block alignment does not decide this** (batch 23 (2026-08-01) -- the sentence here used
    /// to say a real end off a block boundary "reports `Budget` instead", which stopped being true
    /// in batch 12 (2026-07-29) and is contradicted by
    /// `fill_lines_reports_the_certified_end_at_a_non_block_aligned_truncation` two screens down).
    /// Whether the SOURCE was asked decides it: the cache completes a short block by re-reading
    /// until the source answers empty, so a real end inside a block earns `Block::ends_data` and
    /// arrives here on a `Fetched::Short`. What still reports `Budget` is an end nobody asked
    /// about -- an uncertified short answer (P1-1a, R2.1: the fix round's own H's-pin-flip attempt
    /// does not survive re-derivation -- see
    /// `viewport_and_nav_agree_at_a_non_block_aligned_truncation_now_by_resolving_it`'s own
    /// comment, document.rs), or a block the claimed size resolved.
    ///
    /// The one place alignment still bites is the OTHER direction, and it is a property of this
    /// walk rather than of the witness: a fill whose FIRST read comes back wholly empty holds no
    /// byte below it, so it cannot claim the position even when the position is right. See the
    /// `Fetched::Empty` arm below.
    ///
    /// The wrapped `CertifiedEnd` is presently consumed only for ITS VARIANT (`Document::viewport`
    /// checks `matches!(fill_outcome, FillOutcome::End(_))` for `buf_is_true_eof`, never the
    /// position it carries) -- kept, not simplified to a unit variant, because `CertifiedEnd` has
    /// no public constructor: the mere ability to build this variant here is itself proof the
    /// underlying observation was real, a guarantee a bare `bool` could not make.
    End(#[allow(dead_code)] crate::meter::CertifiedEnd),
}
/// Collects bytes from `from` up to and including the `rows`-th newline, the source's real end,
/// or `budget`.
pub async fn fill_lines(
    cache: &BlockCache,
    from: u64,
    rows: usize,
    budget: usize,
) -> anyhow::Result<(Vec<u8>, FillOutcome)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut pos = from;
    let mut newlines = 0usize;
    let mut meter = crate::meter::Meter::background(cache.block_size());
    let mut reader = crate::meter::Reader::new(cache, &mut meter);
    while newlines < rows && buf.len() < budget {
        let outcome = reader
            .read_at_unbounded(
                pos,
                usize::MAX,
                crate::meter::Access::Payload,
                crate::meter::Charge::Payload,
            )
            .await?;
        let (slice, is_short, certified_end) = match outcome {
            crate::meter::Fetched::Bytes { got: b, .. } => (b, false, None),
            crate::meter::Fetched::Short { got, end, .. } => (got, true, end),
            crate::meter::Fetched::Empty { end, .. } => {
                // the truncation policy (docs/budgeted_scanning.md, fix round F1): a genuinely
                // EMPTY block (`block.is_empty()`, not merely short) is the one unambiguous
                // signal -- a single-call FORWARD reader resolves its own terminal outcome here
                // directly. The OLD code silently returned `stopped: false` at the genuine real
                // end whenever `pos < size` (a stale/overstated claim), which `Document::viewport`
                // read as "more to read, just budget/row-limited" -- dropping the final
                // unterminated line from the screen instead of rendering it.
                //
                // **An empty read proves `real_end <= pos`, not `real_end == pos`** (batch 22
                // (2026-08-01), finding #1), and what this variant tells `Document::viewport` is
                // the stronger claim: the data ends at BUF's OWN END. Two premises turn the bound
                // into that claim, and this walk is the only place that can hold either:
                //
                // - `pos == 0`: nothing can sit below zero, so `real_end <= 0` IS `real_end == 0`,
                //   and `buf` (necessarily empty) ends there too. The empty file.
                // - a non-empty `buf`: this walk read `[from, pos)` contiguously and appended
                //   every byte of it, so byte `pos - 1` was real -- with `real_end <= pos` from
                //   the read in hand, that is equality, and `buf` ends exactly at `pos`.
                //
                // Neither is inferable inside `meter.rs`, which sees one read and no history --
                // see `CertifiedEnd`'s own doc comment for why the wholly-empty arm mints a bound
                // rather than a location. Without a premise, `fill_lines` was handing the bound to
                // the viewport as `buf_is_true_eof`: over 5 real bytes behind a claimed 100,
                // filling from 50 returned `End(CertifiedEnd(50))` -- a real end 45 bytes past
                // where the data stops. `Budget` is the honest answer, exactly as it is for an
                // uncertified short block and for the same reason: this walk cannot prove where it
                // stopped.
                //
                // **`BlockJump::ContentBelow` is deliberately NOT a third premise**, though the
                // certificate it carries is exact. Exact about a position BELOW this one: that arm
                // fires when `pos` has run past a certified short block's own content, so `end`
                // names where the data stops and `pos` names somewhere after it. Accepting it
                // would re-assert "buf ends at the real end" for a `buf` that does not -- the same
                // defect one arm over. A walk that consumed the block's bytes reaches it with a
                // non-empty `buf` and qualifies on that premise instead, which is the case worth
                // keeping.
                //
                // The conservative miss this leaves is narrow and known: a viewport anchored
                // exactly at the BLOCK-ALIGNED real end of a TRUNCATED source, whose first read is
                // therefore empty with nothing below it in hand. An accurately sized source is
                // unaffected -- `Document::viewport`'s own `buf_end == self.size` disjunct answers
                // it without a witness. Closing the gap would take one bounded look-behind read at
                // `pos - 1`, the same move `SearchBackward::endpoint_match_reading_lookbehind`
                // makes for the identical reason; it is not worth a read on the viewport's hot
                // path until that miss is shown to matter.
                let earned = pos == 0 || !buf.is_empty();
                return Ok((
                    buf,
                    if earned {
                        FillOutcome::End(end)
                    } else {
                        FillOutcome::Budget
                    },
                ));
            }
        };
        let mut take = slice.len();
        let mut found_row = false;
        for (i, &b) in slice.iter().enumerate() {
            if b == b'\n' {
                newlines += 1;
                if newlines == rows {
                    take = i + 1;
                    found_row = true;
                    break;
                }
            }
        }
        buf.extend_from_slice(&slice[..take]);
        pos += take as u64;
        // restructure R3 fix round, P1-1: only a GENUINELY empty read may certify `FillOutcome::
        // End`, at its own position -- never inferred from a merely-short block's own boundary.
        // `BlockSource`'s own "up to len" contract (source.rs) permits a short answer for a
        // reason OTHER than real EOF; the block-indexed cache structurally cannot re-ask this
        // same block for whatever real data might sit in the gap between the short answer's own
        // length and the next block's own start, once cached -- so there is no honest way to
        // verify that gap at all, and a Short's own boundary can never be trusted as the real
        // end. (An earlier version of this fix tried to verify via one discarded probe at the
        // next block's own start, inferring "empty there" as "certified here" -- unsound: the
        // adversarial review's own P1-1 finding, `b"helloXmo"`, block 0 answers 5 of its own 8
        // bytes while block 1 is genuinely empty, real bytes 'X','m','o' sit at 5..8 regardless
        // -- caught by the PROTECTED `ShortFirstBlock` sibling fixture's own cousin, not by
        // construction.) A short read that does not reach `rows` and carries NO CERTIFICATE
        // therefore reports `Budget` -- the honest, pre-restructure-equivalent conservative miss
        // for this case;
        // `viewport_and_nav_agree_at_a_non_block_aligned_truncation_now_by_resolving_it`'s
        // own comment (document.rs) has the full R2.1 adjudication: this is inherent to the
        // block-indexed cache, not merely a reverted attempt. What the law rules out is inferring
        // an end from a short LENGTH; a certificate the SOURCE minted (`Block::ends_data`, an
        // observed empty read at exactly that position) is a different fact and does certify, which
        // is what the next paragraph is about -- batch 22 (2026-08-01) restates the "always reports
        // `Budget`" this used to say, contradicting the branch immediately below it.
        if is_short && !found_row {
            // batch 14 (2026-07-30), finding #1: `Budget` UNLESS the source itself certified this
            // block's end, in which case `buf` stops exactly at the file's real end and this is a
            // genuine `End`. `!found_row` is what makes the position exact -- the whole slice went
            // into `buf`, so `buf`'s own end IS the certified position, not somewhere inside it.
            //
            // Without this, `fill_lines` reported `Budget` at a real, certified end; `Document::
            // viewport` read that as "more to read, just row/budget-limited" (`buf_is_true_eof`
            // false) and painted no `$` at a position forward search had just returned a
            // `FoundMatch` for -- nav and the viewport disagreeing about where the data ends, which
            // is the exact divergence the refill exists to end, reopened one layer up.
            //
            // The P1-1 law the paragraph above states is unweakened (`meter.rs`'s own module doc
            // carries batch 14's amendment): the certificate is earned by an observed EMPTY read at
            // the position it names -- `cache::Block::ends_data` is mintable nowhere else -- and a
            // short LENGTH still certifies nothing. Every short answer without such an observation
            // behind it still reports `Budget`, unchanged, including the one this paragraph's own
            // `b"helloXmo"` fixture produces.
            return Ok((
                buf,
                match certified_end {
                    Some(end) => FillOutcome::End(end),
                    None => FillOutcome::Budget,
                },
            ));
        }
    }
    if newlines >= rows {
        Ok((buf, FillOutcome::Rows))
    } else {
        Ok((buf, FillOutcome::Budget))
    }
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
    /// batch 22 (2026-08-01), finding #1. **An empty read bounds the real end from above; it does
    /// not locate it.** Starting a fill at a position with nothing real below it in hand, over a
    /// source whose data stopped long before, used to return `End(CertifiedEnd(from))` -- an exact
    /// end 45 bytes past where the file actually stops, handed to `Document::viewport` as
    /// `buf_is_true_eof`. Both halves are asserted, since a fix that simply stopped certifying
    /// would also break the ordinary walk.
    #[tokio::test]
    async fn a_wholly_empty_first_read_bounds_the_end_but_a_contiguous_walk_locates_it() {
        struct Overstates;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for Overstates {
            fn size(&self) -> u64 {
                100
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        const REAL: &[u8] = b"abcde"; // 5 real bytes behind a claimed 100
                        if offset >= REAL.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let s = offset as usize;
                        Ok(bytes::Bytes::from_static(REAL).slice(s..(s + len).min(REAL.len())))
                    })
                })
            }
        }
        let c = BlockCache::new(Arc::new(Overstates), 8, 1 << 20);
        let (buf, outcome) = fill_lines(&c, 50, 3, 1 << 10).await.unwrap();
        assert!(buf.is_empty(), "nothing real lives at 50");
        assert!(
            matches!(outcome, FillOutcome::Budget),
            "position 50 is only an UPPER bound on a file that stops at 5 -- nothing was read \
             below it to make the bound an equality; got {outcome:?}"
        );
        // the same source, walked contiguously from 0: the empty read at 5 now lands with byte 4
        // already in `buf`, which is the premise that turns `real_end <= 5` into `real_end == 5`.
        let c = BlockCache::new(Arc::new(Overstates), 8, 1 << 20);
        let (buf, outcome) = fill_lines(&c, 0, 3, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"abcde");
        assert!(
            matches!(outcome, FillOutcome::End(_)),
            "a contiguous walk earns the exact end it stops at; got {outcome:?}"
        );
        // The `BlockJump::ContentBelow` arm, from inside the same block's dead zone: starting at 6
        // hands back a certificate that is EXACT -- and names 5, not 6. `buf` ends at 6, so
        // reporting `End` here would re-assert "the data ends at buf's own end" about a `buf` that
        // runs one byte past it. Exactness at the mint is not the same premise as exactness HERE.
        let c = BlockCache::new(Arc::new(Overstates), 8, 1 << 20);
        let (buf, outcome) = fill_lines(&c, 6, 3, 1 << 10).await.unwrap();
        assert!(buf.is_empty());
        assert!(
            matches!(outcome, FillOutcome::Budget),
            "a certificate for a position BELOW where `buf` ends cannot say `buf` ends at the \
             real end; got {outcome:?}"
        );
    }
    /// batch 23 (2026-08-01), finding #2: the same upper-bound-stored-as-an-exact-end defect, one
    /// layer over in `SearchBackward`'s own `observed_end`. Asserted directly on the recorder,
    /// because the field's invariant is what is being pinned and the surrounding contiguity gates
    /// were (accidentally) holding the search-level behaviour up in every case anyone constructed
    /// -- so a search-level assertion would have passed before the fix and proved nothing.
    #[test]
    fn only_an_exact_empty_observation_is_recorded_as_the_observed_end() {
        use crate::meter::{BlockJump, CertifiedEnd};
        let exact = CertifiedEnd::for_test(64);
        let mut observed = None;
        SearchBackward::note_empty_end(&mut observed, exact, BlockJump::Safe);
        assert_eq!(
            observed, None,
            "a WHOLLY empty block proves only `real_end <= 64`; recording it as the end grants \
             `Edge::True` there and lets `$` match a position the data may stop well below"
        );
        SearchBackward::note_empty_end(&mut observed, exact, BlockJump::ContentBelow);
        assert_eq!(
            observed,
            Some(64),
            "a certified SHORT block has real bytes immediately below the position -- exact"
        );
        let mut observed = None;
        SearchBackward::note_empty_end(&mut observed, CertifiedEnd::for_test(0), BlockJump::Safe);
        assert_eq!(
            observed,
            Some(0),
            "nothing can sit below zero, so the bound IS the end -- the empty-file case batch 16 \
             (2026-07-31) exists to serve"
        );
    }
    /// The other two ways the premise is held, kept apart from the walk above because each is the
    /// sole reason its own case works: an empty file needs no byte below position 0, and a
    /// block-aligned accurate source reaches its empty block only after reading the block below.
    #[tokio::test]
    async fn an_empty_file_and_a_block_aligned_end_both_still_certify() {
        let c = cache(b"", 4);
        let (buf, outcome) = fill_lines(&c, 0, 3, 1 << 10).await.unwrap();
        assert!(buf.is_empty());
        assert!(
            matches!(outcome, FillOutcome::End(_)),
            "nothing can sit below position 0, so `real_end <= 0` IS `real_end == 0`; got \
             {outcome:?}"
        );
        let c = cache(b"abcd", 4); // ends exactly on a block boundary
        let (buf, outcome) = fill_lines(&c, 0, 3, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"abcd");
        assert!(
            matches!(outcome, FillOutcome::End(_)),
            "block 1 is wholly empty, but block 0's bytes are in `buf` below it; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn fill_collects_rows_worth_of_lines() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let (buf, outcome) = fill_lines(&c, 2, 2, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"b\nc\n");
        assert!(
            matches!(outcome, FillOutcome::Rows),
            "expected Rows, got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn fill_stops_at_eof_and_reports_it() {
        let c = cache(b"a\nbc", 4);
        let (buf, outcome) = fill_lines(&c, 2, 5, 1 << 10).await.unwrap();
        assert_eq!(&buf[..], b"bc");
        assert!(
            matches!(outcome, FillOutcome::End(_)),
            "expected a certified End, got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn fill_respects_the_budget() {
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let c = cache(data, 16);
        let (buf, outcome) = fill_lines(&c, 0, 2, 64).await.unwrap();
        assert!(
            buf.len() >= 64 && buf.len() <= 64 + 16,
            "budget overshoot: {}",
            buf.len()
        );
        assert!(
            matches!(outcome, FillOutcome::Budget),
            "expected Budget, got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn forward_scan_finds_nth_line_start() {
        let c = cache(b"a\nb\nc\nd\n", 4);
        let mut s = ForwardScan::new(0, 2, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Done(FwdEnd::Found(start)) => assert_eq!(start, 4),
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
            FwdStep::Done(FwdEnd::Eof(clamp)) => assert_eq!(clamp, 4),
            other => panic!("expected Eof, got {other:?}"),
        }
        // pins the fast-Eof branch's accounting, the twin of the Found
        // branch asserted in forward_scan_scanned_counts_the_terminal_block.
        assert_eq!(s.progressed(), 6, "all six bytes were examined");
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
            FwdStep::Done(FwdEnd::Found(start)) => assert_eq!(start, 65),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(s.progressed(), 65, "bytes 0..=64 were all examined");
    }
    #[tokio::test]
    async fn forward_scan_eof_clamps_to_origin_when_no_newline_seen() {
        let c = cache(b"abcdef", 4);
        let mut s = ForwardScan::new(0, 1, 1 << 10);
        match s.step(&c).await.unwrap() {
            FwdStep::Done(FwdEnd::Eof(clamp)) => assert_eq!(clamp, 0),
            other => panic!("expected Eof, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn forward_scan_resumes_across_chunks_without_handoff() {
        // 4096 x's then a newline then y: one chunk cannot reach it; stepping
        // the same object to the end must, with no cursor re-derivation.
        //
        // Restructure R6's own discriminating test for `restr-R3-review.md` §R2.4's
        // charged-but-frozen mutation (a real one, verified by hand, not merely asserted): edit
        // `ForwardScan::step` to `break` immediately after a successful read, before `self.pos`'s
        // own advance -- charging the read while never moving the cursor. Pre-R6, `progress_
        // witness()` alone had no opinion on cursor motion, so the mutated code returned an
        // honest-LOOKING `More` every call; THIS test's own unbounded `loop { match step ... }`
        // (no step-count ceiling, unlike some sibling tests) is exactly the shape that hung the
        // suite when the reviewer tried the identical mutation on the pre-restructure code. Under
        // R6, the same mutation instead PANICS immediately (0.00s), at the `.expect()` inside
        // `step`'s own post-loop match: `moved` stays `None` (the cursor never advanced) while
        // `progress_witness()` still succeeds (a charge genuinely happened), so `Step::More`
        // cannot be constructed -- a runtime residue, not a compile refusal (Rust cannot see
        // statically that `moved` is always `Some` here; only the loop's own `chunk.max(1)`
        // argument proves it), but a diagnostic panic naming the exact invariant beats an
        // unbounded hang. The converse (a position advance with no charge) stays impossible by
        // construction: every advance here derives from a `slice` that only ever exists because
        // `reader.read_at` already charged it.
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
                FwdStep::Done(FwdEnd::Found(start)) => {
                    assert_eq!(start, 4097);
                    assert!(steps >= 2, "must have taken multiple chunks");
                    break;
                }
                FwdStep::Done(FwdEnd::Eof(clamp)) => panic!("unexpected Eof({clamp})"),
                FwdStep::More(..) => {
                    steps += 1;
                    assert!(
                        s.progressed() >= 64 * steps as u64 - 16,
                        "progressed() tracks chunks"
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
            FwdStep::Done(FwdEnd::Found(start)) => assert_eq!(start, 2),
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
                FwdStep::Done(FwdEnd::Found(start)) => {
                    assert_eq!(start, 2);
                    break;
                }
                FwdStep::More(..) => {}
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
        assert_eq!(got, FwdEnd::Found(1025));
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
            FwdStep::Done(FwdEnd::Eof(clamp)) => {
                assert_eq!(clamp, 4, "clamped to size, not the garbage origin")
            }
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
            FwdStep::Done(FwdEnd::Found(start)) => assert_eq!(start, 4, "clamped into the file"),
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
            BwdStep::More(..) => {}
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
        assert_eq!(s.progressed(), 62, "bytes 1..=62 were all examined");
    }
    // `OneBytePerBlock` -- a source answering exactly one byte per read, `size()` accurate -- was
    // DELETED here in batch 13 (2026-07-29). It existed for the two short-read charging tests
    // below, and it can no longer reach their subject: retiring `REFILL_ATTEMPT_CAP` means the
    // cache COMPLETES a dribbling source's block, so nothing short arrives at this layer and there
    // is no undelivered span left to charge for. `TruncatedInsideTheLastBlock` below is the shape
    // that still produces one. The dribbling shape itself is still exercised, one layer down where
    // it now belongs: `document.rs`'s own
    // `a_dribbling_source_still_yields_a_renderable_found_match`.
    /// A source whose `size()` overstates its real data, which ends INSIDE the last block: 961 real
    /// bytes behind a claim of 1024, so block 15 is legally short with one real byte at 960 and the
    /// refill's own read at 961 certifies that end (batch 13 (2026-07-29)).
    ///
    /// This is the fixture `backward_scan_charges_the_span_a_short_read_leaves_undelivered` and
    /// `search_backward_charges_the_span_a_short_read_leaves_undelivered` need now. Both used
    /// `OneBytePerBlock`, and that shape no longer reaches their subject at all:
    /// retiring `REFILL_ATTEMPT_CAP` means a dribbling source's block gets COMPLETED (64 one-byte
    /// reads, all 64 bytes delivered), so no short read arrives at the consumer and there is no
    /// undelivered span left to charge for. A truncated source is the path that still produces
    /// one -- the block really is short, permanently and legitimately -- which is exactly the
    /// descent-past-fictional-territory case those tests were written about.
    struct TruncatedInsideTheLastBlock {
        reads: Arc<std::sync::atomic::AtomicU64>,
    }
    impl TruncatedInsideTheLastBlock {
        const REAL: u64 = 961;
        const CLAIMED: u64 = 1024;
    }
    #[async_trait::async_trait]
    impl crate::source::BlockSource for TruncatedInsideTheLastBlock {
        fn size(&self) -> u64 {
            Self::CLAIMED
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let reads = self.reads.clone();
            crate::source::ReadTicket::from_fn(move |offset, len| {
                let reads = reads.clone();
                Box::pin(async move {
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let real = TruncatedInsideTheLastBlock::REAL;
                    if offset >= real {
                        return Ok(bytes::Bytes::new());
                    }
                    let n = (len as u64).min(real - offset) as usize;
                    Ok(bytes::Bytes::from(vec![b'x'; n]))
                })
            })
        }
    }
    /// A CONFORMING source with an ACCURATE `size()` that answers every read with a single byte
    /// -- `BlockSource`'s own "up to `len`" contract (`source.rs`) permits exactly this, and it is
    /// the regime where "a short read means EOF" and "a short read means nothing" diverge
    /// maximally: every block is short, and every block after the first still holds real data.
    /// `needle_at`, when set, places a `z` at that offset instead of the filler byte.
    struct ShortEveryBlock {
        size: u64,
        needle_at: Option<u64>,
    }
    #[async_trait::async_trait]
    impl crate::source::BlockSource for ShortEveryBlock {
        fn size(&self) -> u64 {
            self.size
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let size = self.size;
            let needle_at = self.needle_at;
            crate::source::ReadTicket::from_fn(move |offset, _len| {
                Box::pin(async move {
                    if offset >= size {
                        return Ok(bytes::Bytes::new());
                    }
                    if needle_at == Some(offset) {
                        return Ok(bytes::Bytes::from_static(b"z"));
                    }
                    Ok(bytes::Bytes::from_static(b"a"))
                })
            })
        }
    }
    #[tokio::test]
    async fn search_forward_continues_past_a_legally_short_block_mid_file() {
        // batch 9 (2026-07-29), finding #1. A legal short read is NOT end-of-file: `Short`
        // carries no witness at all (`meter.rs`'s own P1-1 law), yet all three forward scanners
        // treated a zero-byte follow-up answer as the genuine end. With 64-byte blocks, an
        // accurate `size()` of 1024, one delivered byte per block and `z` at offset 64, this
        // reported `Exhausted` for a match sitting one block up in plain sight.
        //
        // Two things had to change together. The scan must JUMP the unreadable gap (the cache
        // memoises the short answer, so `[1, 64)` can never be read again) -- and it must FLUSH
        // the window first, because a candidate waiting on lookahead that a gap guarantees will
        // never arrive would otherwise be destroyed by the buffer clear the jump requires.
        let c = BlockCache::new(
            Arc::new(ShortEveryBlock {
                size: 1024,
                needle_at: Some(64),
            }),
            64,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("z", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(1024), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                assert_eq!(
                    match_at, 64,
                    "the match is real and reachable -- block 1 is ordinary"
                );
            }
            other => panic!(
                "reported Exhausted before this fix -- the first short block was mistaken for \
                 EOF and the scan never looked at block 1; got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn forward_scan_continues_past_a_legally_short_block_mid_file() {
        // finding #1's newline-counting twin (`ForwardScan`, whose own comment used to assert
        // outright that a short read "is the same terminal signal `Empty` is"). The gap's own
        // newlines are unreachable through this cache and stay lost either way -- but stopping
        // abandoned the entire rest of the file with them.
        let c = BlockCache::new(
            Arc::new(ShortEveryBlock {
                size: 1024,
                needle_at: None,
            }),
            64,
            1 << 20,
        );
        let mut s = ForwardScan::new(0, 1, 1 << 20);
        let step = s.step(&c).await.unwrap();
        assert!(
            matches!(step, Step::Done(FwdEnd::Eof(_))),
            "no newline exists anywhere in this fixture, so EOF is the right terminal; got \
             {step:?}"
        );
        assert!(
            s.progressed() > 1,
            "before this fix the scan stopped after block 0's single byte -- it must have walked \
             the whole file's worth of blocks to conclude there is no newline; progressed: {}",
            s.progressed()
        );
    }
    #[tokio::test]
    async fn count_scan_continues_past_a_legally_short_block_mid_file() {
        // finding #1's third site. `CountScan`'s `Short` arm counted its own bytes and returned
        // `Done` immediately, so a count over a conforming short-answering source stopped at the
        // first block and reported whatever it had seen so far as the total.
        let c = BlockCache::new(
            Arc::new(ShortEveryBlock {
                size: 1024,
                needle_at: None,
            }),
            64,
            1 << 20,
        );
        let mut s = CountScan::new(0, 1024, 1 << 20);
        let step = s.step(&c).await.unwrap();
        assert!(
            matches!(step, Step::Done(0)),
            "no newlines in this fixture; got {step:?}"
        );
        assert!(
            s.progressed() > 1,
            "the count must reach past block 0 rather than stopping at its single short byte; \
             progressed: {}",
            s.progressed()
        );
    }
    #[tokio::test]
    async fn search_forward_reports_a_miss_not_a_fabrication_at_an_uncertified_short_boundary() {
        // batch 11 (2026-07-29), P1. `b"helloXmo"` is `meter.rs`'s own P1-1 law's fixture: block 0
        // answers 5 of its own 8 bytes, position 5 is a real `'X'`, and the block-indexed cache can
        // never re-ask block 0 for the gap it did not fetch. Granting that boundary a TRUE high
        // edge let `hello$` match an edge the file does not have.
        //
        // This pins `docs/search.md`'s own standing rule, which nav was the last consumer to
        // violate: an unverified edge means the candidate is DROPPED -- "a documented miss, never a
        // fabrication" -- and nav is required to AGREE with the highlighter rather than trust a cut
        // the highlighter refuses. Batch 10 briefly kept the fabrication as "adjudicated policy",
        // reading R3's own note that the viewport "continue[s] to disagree with nav on this exact
        // input" as an endorsement of nav's side. It is not: the sibling test in `document.rs`
        // records that same divergence as "docketed ... to reconcile", and R3's own attempt to
        // reconcile it by moving the VIEWPORT was reverted as unsound. Moving NAV is the sound
        // direction, and this is it.
        let c = BlockCache::new(
            Arc::new(ShortFirstBlockOnly {
                real: b"helloXmo".to_vec(),
                first_len: 5,
            }),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("hello$", false).unwrap());
        assert_eq!(
            p.find_all(b"helloXmo"),
            Vec::<(usize, usize)>::new(),
            "ground truth: 'hello' is followed by a real 'X', so `hello$` matches nowhere"
        );
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(8), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "fabricated a match against an uncertified cut -- position 5 is a real byte this \
                 cache can never deliver, which is not the same as the file ending there; got \
                 {other:?}"
            ),
        }
    }
    /// A conforming source whose FIRST block alone answers short (`first_len` of its own nominal
    /// width) while every byte it claims is genuinely present -- `BlockSource`'s "up to `len`"
    /// contract (`source.rs`). Distinct from `ShortEveryBlock` above: exactly one short answer, so
    /// the gap it leaves is the LAST thing in the file rather than one of many.
    struct ShortFirstBlockOnly {
        real: Vec<u8>,
        first_len: usize,
    }
    #[async_trait::async_trait]
    impl crate::source::BlockSource for ShortFirstBlockOnly {
        fn size(&self) -> u64 {
            self.real.len() as u64
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let real = self.real.clone();
            let first_len = self.first_len;
            crate::source::ReadTicket::from_fn(move |offset, len| {
                let real = real.clone();
                Box::pin(async move {
                    if offset >= real.len() as u64 {
                        return Ok(bytes::Bytes::new());
                    }
                    let start = offset as usize;
                    let want = if offset == 0 { first_len } else { len };
                    let end = (start + want).min(real.len());
                    Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                })
            })
        }
    }
    /// A source claiming the largest size a `u64` can express, answering a fixed small number of
    /// bytes for every read. Exists for the next-block arithmetic overflow (batch 10
    /// (2026-07-29), P2): the only way to reach the final block is to START there.
    struct HugeClaimedSize {
        short_at: u64,
    }
    #[async_trait::async_trait]
    impl crate::source::BlockSource for HugeClaimedSize {
        fn size(&self) -> u64 {
            u64::MAX
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let short_at = self.short_at;
            crate::source::ReadTicket::from_fn(move |offset, len| {
                Box::pin(async move {
                    // the final block answers 2 bytes (legally short); every other read is full,
                    // so the ctx seed below it completes and the scan reaches the payload path.
                    let n = if offset >= short_at { 2 } else { len };
                    Ok(bytes::Bytes::from(vec![b'a'; n]))
                })
            })
        }
    }
    #[tokio::test]
    async fn next_block_arithmetic_does_not_overflow_in_the_final_block() {
        // batch 10 (2026-07-29), P2. `(idx + 1) * bs` overflows `u64` for any position inside the
        // last representable block -- with `bs` 64 the last block starts at `u64::MAX / 64 * 64`
        // and `(idx + 1) * 64` is 2^64 exactly, a debug-build panic. Batch 9 introduced this
        // expression at three sites (`SearchForward`, `ForwardScan`, `CountScan`), all reached
        // only on a short read, which is precisely the path a final short block takes.
        const BS: u64 = 64;
        let last_block_start = u64::MAX / BS * BS;
        let c = BlockCache::new(
            Arc::new(HugeClaimedSize {
                short_at: last_block_start,
            }),
            BS as usize,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("zzzz", false).unwrap());
        let mut s = SearchForward::new(p, last_block_start, Bound::Exclusive(u64::MAX), 1 << 20);
        // the assertion is that this does not panic; any terminal outcome is acceptable (there is
        // no `zzzz` anywhere in the fixture).
        let step = s.step(&c).await.unwrap();
        assert!(
            matches!(step, SearchStep::Done(_) | SearchStep::More(..)),
            "got {step:?}"
        );
        let mut f = ForwardScan::new(last_block_start, 1, 1 << 20);
        let _ = f.step(&c).await.unwrap();
        let mut cs = CountScan::new(last_block_start, u64::MAX, 1 << 20);
        let _ = cs.step(&c).await.unwrap();
    }
    #[tokio::test]
    async fn forward_scans_resolve_a_truncated_sources_real_end_exactly() {
        // the other half of batch 9's finding #1 distinction -- the gap-jump must NOT widen into
        // "always keep going" -- and the retirement of batch 11's own cost.
        //
        // `size()` claims 1024 over 2 real bytes. Batch 11 made nav refuse to treat block 0's
        // short answer as EOF (rightly: that boundary was indistinguishable from `helloXmo`'s gap
        // holding real bytes), and the price was this file's own legitimate `$` at 2 becoming a
        // documented miss. Batch 12's refill removes the trade entirely: asking the source once
        // for `[2, 64)` gets EMPTY back, which certifies -- per `source.rs`, an empty answer means
        // `offset >= size` -- so the end at 2 is now a FACT, and `$` belongs there again. The two
        // cases stopped being indistinguishable the moment the cache was allowed to ask.
        struct TruncatedBelowItsClaim {
            reads: Arc<std::sync::atomic::AtomicU64>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for TruncatedBelowItsClaim {
            fn size(&self) -> u64 {
                1024
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let reads = self.reads.clone();
                crate::source::ReadTicket::from_fn(move |offset, _len| {
                    let reads = reads.clone();
                    Box::pin(async move {
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if offset >= 2 {
                            return Ok(bytes::Bytes::new());
                        }
                        Ok(bytes::Bytes::from_static(b"ab").slice(offset as usize..))
                    })
                })
            }
        }
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = BlockCache::new(
            Arc::new(TruncatedBelowItsClaim {
                reads: reads.clone(),
            }),
            64,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(1024), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 2,
                "the real end is 2 and the refill proves it, so `$` is reported there against a \
                 genuinely earned TRUE edge -- not guessed, and no longer given up on"
            ),
            other => panic!("expected Found at the certified real end (2), got {other:?}"),
        }
        assert!(
            reads.load(std::sync::atomic::Ordering::Relaxed) <= 4,
            "and it must still not walk 15 blocks of fictional territory: one payload read, one \
             refill that certifies, and the scan is done; got {} reads",
            reads.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    #[tokio::test]
    async fn backward_scan_charges_the_span_a_short_read_leaves_undelivered() {
        // batch 8 (2026-07-29), P2. A non-empty SHORT read charged only its delivered bytes and
        // then dropped `hi` to the block start anyway -- so the span `[delivered_top, old_hi)`
        // was passed over for free. The `Empty` arm a few lines up has always charged its own
        // descent via `Reader::skip` for exactly this reason ("a huge stale-size descent must
        // pend across `step` calls rather than run unbounded inside one"); a short-but-nonempty
        // answer descends just as far and was the one path that did not pay for it.
        //
        // 16 blocks of 64, one byte delivered each: before the fix a step charged 1 per block, so
        // a `chunk` of 16 bought all 16 blocks -- the whole 1024-byte file descended synchronously
        // for 16 bytes of budget. (The reported example named a budget of 2; that yields 2 reads,
        // not 16, since `out_of_budget` is `spent_this_step >= chunk`. The mechanism is the same
        // and the ratio is what matters: the budget under-counts the descent by up to a full
        // `block_size` per read, so the number that makes it buy the entire file is 16.)
        //
        // batch 13 (2026-07-29): the fixture is `TruncatedInsideTheLastBlock`, not the dribbling
        // `OneBytePerBlock` this was written against -- see that fixture's own doc comment for why
        // a dribbling source no longer produces a short read at this layer at all. The defect under
        // test is unchanged and so are its numbers: one real byte delivered out of a 63-byte span.
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = BlockCache::new(
            Arc::new(TruncatedInsideTheLastBlock {
                reads: reads.clone(),
            }),
            64,
            1 << 20,
        );
        let mut s = BackwardScan::new(1024, 1, 16);
        let step = s.step(&c).await.unwrap();
        assert!(
            matches!(step, BwdStep::More(..)),
            "one step must not resolve the whole descent; got {step:?}"
        );
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "one payload read for block 15 plus the ONE refill read that certifies its end -- the \
             block is genuinely short, so the refill loop ends on the source's own empty answer \
             rather than filling anything, and the descent below still happens. What matters is \
             that a budget of 16 buys ONE block's worth of it, not all 16 blocks"
        );
        assert_eq!(
            s.remaining_bytes(),
            960,
            "exactly one block of territory descended"
        );
        assert_eq!(
            s.charged(),
            63,
            "1 byte delivered + 62 skipped -- the budget must see the territory passed over, not \
             merely what the short answer happened to hand back. 63 rather than a round 64 \
             because this scan starts at `pos - 1` (`with_meter`: a line-start hunt never \
             examines its own position), so the first block's span is [960, 1023)"
        );
    }
    #[tokio::test]
    async fn search_backward_charges_the_span_a_short_read_leaves_undelivered() {
        // batch 8 (2026-07-29), P2's second site: the same defect in the search leg, where
        // `delivered_top` (batch 7's own finding #2) already NAMES the gap in order to drop the
        // carry across it -- but charged nothing for it. Placing the charge at that same site
        // covers the `warmed` reuse path too, which is the worse of the two: it charges NOTHING
        // at all (only `record_progress`, which never touches `charged()`), so a run of reused
        // blocks descended entirely for free.
        // batch 13 (2026-07-29): `TruncatedInsideTheLastBlock` for the same reason as its sibling
        // above -- a dribbling source's block is completed now, so it leaves no undelivered span.
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = BlockCache::new(
            Arc::new(TruncatedInsideTheLastBlock {
                reads: reads.clone(),
            }),
            64,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 1024, Bound::Inclusive(0), 16);
        let step = s.step(&c).await.unwrap();
        assert!(
            matches!(step, SearchStep::More(..)),
            "one step must not descend the whole file; got {step:?}"
        );
        assert!(
            s.charged() >= 64,
            "the step descended a 64-byte block on a 1-byte answer, so at least that span must \
             be charged; got {}",
            s.charged()
        );
        assert!(
            reads.load(std::sync::atomic::Ordering::Relaxed) <= 4,
            "a budget of 16 must not buy a walk down the whole file: block 15's own payload read \
             and the one refill that certifies its end, plus at most the ctx peek's own pair; got \
             {} reads",
            reads.load(std::sync::atomic::Ordering::Relaxed)
        );
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
                CountStep::More(..) => steps += 1,
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
            s.progressed(),
            65,
            "bytes 0..65 were examined, not byte 65 itself"
        );
    }
    #[tokio::test]
    async fn search_forward_ctx_seed_reads_charge_the_budget_the_driver_reads() {
        // unit-G (batch-5 #5), GREEN: before this restructure, the forward ctx seed read up to
        // CTX_BEHIND look-behind bytes through `warm()` BEFORE the budgeted payload loop ever
        // ran, entirely UNCHARGED (`spent` was declared after this loop in the pre-restructure
        // code, structurally unable to charge it) -- at block size 1, budget 1, one `step()` call
        // did 5 physical reads (4 ctx + 1 payload) while the driver's own budget only ever saw 1
        // charged. Now every read routes through the same metered `Reader`, so the 4 ctx reads
        // charge `spent_this_step` (as bookkeeping, per `Charge::Bookkeeping`) exactly like the
        // payload read does. At this same budget of 1, that charge alone exhausts the step before
        // the payload read ever starts: `out_of_budget()` is already true by the time the payload
        // loop's own `read_at` runs, so it returns `OutOfBudget` without touching the source --
        // only 4 physical reads happen, not 5, and all 4 are now visible to the driver's own
        // budget (`charged()`), the exact discrepancy this restructure closes.
        let data: &'static [u8] = Box::leak(vec![b'x'; 100].into_boxed_slice());
        let src = Arc::new(MockSource::new(bytes::Bytes::from_static(data)));
        let c = crate::cache::BlockCache::new(src.clone(), 1, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 10, Bound::Exclusive(u64::MAX), 1);
        match s.step(&c).await.unwrap() {
            SearchStep::More(..) => {}
            other => panic!("expected More at chunk 1, got {other:?}"),
        }
        assert_eq!(
            src.read_count(),
            4,
            "bounded (today): the 4 ctx bytes alone exhaust a budget of 1, so the payload read \
             never starts -- {} physical reads",
            src.read_count()
        );
        assert_eq!(
            s.charged(),
            4,
            "the driver's own budget must see all 4 ctx bytes charged, not just the (never \
             reached) payload byte -- {} charged",
            s.charged()
        );
    }
    #[tokio::test]
    async fn search_forward_ctx_seed_charges_a_cache_hit_exactly_like_a_miss() {
        // batch 6 (2026-07-28), finding #5: `classify` charges the REQUESTED slice, never
        // distinguishing a physical fetch from a cache hit -- deliberate (a pure "physical
        // reads only" charge would mint no `Charged` witness on an all-hit step, unable to
        // report `More` honestly, the R6 witness law), but the sibling test above only ever
        // demonstrates it against a COLD source. This is the same fixture at block_size 32 (so
        // CTX_BEHIND's 4-byte look-behind, at `pos: 34`, straddles block 0 [0,32) and block 1
        // [32,64) -- two block-granular touches, not one), run twice: once cold, once with both
        // blocks prewarmed.
        let data: &'static [u8] = Box::leak(vec![b'x'; 100].into_boxed_slice());
        let cold_src = Arc::new(MockSource::new(bytes::Bytes::from_static(data)));
        let cold_cache = crate::cache::BlockCache::new(cold_src.clone(), 32, 1 << 20);
        let mut cold = SearchForward::new(
            Arc::new(SearchPattern::compile("needle", false).unwrap()),
            34,
            Bound::Exclusive(u64::MAX),
            1,
        );
        match cold.step(&cold_cache).await.unwrap() {
            SearchStep::More(..) => {}
            other => panic!("expected More at chunk 1, got {other:?}"),
        }
        assert_eq!(
            cold_src.read_count(),
            2,
            "cold: the straddling ctx gather touches two never-before-cached blocks -- {} \
             physical reads",
            cold_src.read_count()
        );
        assert_eq!(cold.charged(), 4, "the full CTX_BEHIND width, cold");

        let warm_src = Arc::new(MockSource::new(bytes::Bytes::from_static(data)));
        let warm_cache = crate::cache::BlockCache::new(warm_src.clone(), 32, 1 << 20);
        let _ = warm_cache.warm(0).await.unwrap();
        let _ = warm_cache.warm(1).await.unwrap();
        assert_eq!(
            warm_src.read_count(),
            2,
            "the two prewarming touches themselves"
        );
        let mut warm = SearchForward::new(
            Arc::new(SearchPattern::compile("needle", false).unwrap()),
            34,
            Bound::Exclusive(u64::MAX),
            1,
        );
        match warm.step(&warm_cache).await.unwrap() {
            SearchStep::More(..) => {}
            other => panic!("expected More at chunk 1, got {other:?}"),
        }
        assert_eq!(
            warm_src.read_count(),
            2,
            "prewarmed: the SAME step must cost zero NEW physical reads -- both blocks are \
             already in hand -- {} physical reads (started at 2)",
            warm_src.read_count()
        );
        assert_eq!(
            warm.charged(),
            4,
            "yet the charge is IDENTICAL to the cold run -- a cache hit costs the budget exactly \
             as much as a miss, which is why this step still reports More on zero new I/O \
             (docs/budgeted_scanning.md's own delivered-work disclosure)"
        );
    }
    #[tokio::test]
    async fn search_forward_finds_the_first_match_after_the_origin() {
        // "needle" at [4, 10) and [19, 25); a `\n` at 14 sits between the
        // origin and the second match, so line_start must reflect it.
        let c = cache(b"aaa needle bbb\nccc needle ddd\n", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 10, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("expected End, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_does_not_fabricate_an_over_cap_word_boundary() {
        // restructure R5 (2026-07-28), the unit-J class -- RED, written FIRST against 05a2a2e:
        // F-review's own P2-4 fixture (`.superpowers/sdd/batch5-unit-F-review.md`), the nav
        // (`n`/`N`) side of the same fabrication `sweep_analysis_does_not_fabricate_an_over_cap_
        // word_boundary_unicode` (search.rs) pins for the background sweep. `a+(?u:\b)` over 100
        // 'x's, a run of 20,000 'a's, a real "é" (a Unicode WORD character), then 200 'z's: no
        // real `(?u:\b)` exists anywhere (word 'a' to word 'é' is never a boundary, at any prefix
        // length), so the oracle finds no match at all. Pre-R5, `SearchForward`'s accept rule is
        // also START-based only: once `s = 100` clears the lookahead margin, `leftmost_confirmed`
        // accepts whatever `a+(?u:\b)` finds against the hay AS IT EXISTS AT THAT MOMENT -- which,
        // read incrementally in `chunk`-sized pieces, ends well short of the real "é" -- `\b`
        // fires against that artificial edge exactly as it would a real boundary. Reported as a
        // fabricated `FoundMatch { match_at: 100, .. }` (the composed `Document::search_next`
        // path) at the sha this brief cites; this test drives `SearchForward` directly (the
        // engine underneath) and pins the oracle's honest `End`.
        let mut data = vec![b'x'; 100];
        data.extend(vec![b'a'; 20_000]);
        data.extend("é".as_bytes());
        data.extend(vec![b'z'; 200]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let pattern = SearchPattern::compile(r"a+(?u:\b)", false).unwrap();
        assert_eq!(
            pattern.find_all(data),
            Vec::<(usize, usize)>::new(),
            "oracle: é is a word character, so no real (?u:\\b) exists after the a-run at any \
             prefix length"
        );
        let c = cache(data, 4096);
        let p = Arc::new(SearchPattern::compile(r"a+(?u:\b)", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1024);
        let outcome = loop {
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                other => break other,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "expected End (no real boundary exists anywhere in this file) -- a Found here is \
                 the over-cap fabrication reading an artificial read-chunk edge as a real \
                 boundary; got {other:?}"
            ),
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 3),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_caret_dollar_on_an_empty_file() {
        // the degenerate empty-file case: position 0 is simultaneously the file's own true
        // start (`^`) and true end (`$`) -- `^$` must match there, zero-width. Not a phantom
        // (batch 4 (2026-07-24), finding #2): position 0 has no predecessor at all, let alone a
        // `\n` one.
        let c = cache(b"", 8);
        let p = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 0),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_rejects_a_zero_width_match_whose_predecessor_is_a_phantom_newline() {
        // batch 4 (2026-07-24), finding #2: position `size` whose real predecessor is `\n` is
        // the trailing newline's own phantom -- never a match, regardless of pattern (the
        // doctrine architecture.md:418-421 already states for `goto_line`, unified here). `$`
        // finds the REAL match at 1 first when scanning from further back
        // (`search_forward_finds_a_zero_width_dollar_at_unterminated_eof`'s own sibling covers a
        // similar shape); this pins the position-2 candidate directly, in isolation, by starting
        // the scan AT it (`from == size`, this leg's own domain is exactly `{2}`).
        let c = cache(b"a\n", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 2, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("falsely matched the phantom at the trailing newline: {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_does_not_admit_a_phantom_caret_dollar_reached_mid_scan() {
        // batch 4 (2026-07-24), finding #2, extended to a site the brief's own enumeration did
        // not name: the "already at/past EOF at entry" widening above is one thing, but a scan
        // that reaches the true end by reading NORMALLY through it (the in-loop `at_eof`
        // widening, `step`'s own main loop) admits the identical phantom unless it is ALSO
        // rejected there. `^$` is what exposes it: on b"a\n", the ONLY position satisfying both
        // `^` and `$` at once is 2, the phantom itself (`^` holds right after the \n at 1; `$`
        // holds at the true end) -- plain `$` alone instead finds the REAL match at 1 first and
        // never even reaches this widening within the same call (`find_starting_in`'s own
        // leftmost-match contract), which is why the other test above (built around plain `$`)
        // does not surface this site on its own.
        let c = cache(b"a\n", 8);
        let p = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("phantom line falsely matched via the in-loop widening: {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_out_of_range_origin_returns_end_instead_of_panicking() {
        // batch 3 (2026-07-23), finding #6 (probe-verified): `from` far past EOF used to panic
        // indexing the ctx seed's own block (fetched for a block index nowhere near this file)
        // before `pos` was ever clamped -- the file-wide "out-of-contract positions degrade to
        // the real bytes" philosophy every other scan object already follows. batch 4
        // (2026-07-24), finding #1 replaced the unconditional clamp with an empty-domain check
        // (`from > size` returns `End` directly, before any read) -- this fixture still exercises
        // the ORIGINAL finding #6 shape (nothing indexed off an out-of-range `pos`), just via the
        // new mechanism. Renamed in this unit's own fix round (F5c, reviewer correction): the
        // mechanism is an early `End`, not a clamp, and arguing a stale name is "accurate in
        // spirit" costs more than just renaming it.
        let c = cache(b"aaa bbb ccc", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, u64::MAX, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("expected End (degraded gracefully, no panic), got {other:?}"),
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 4096);
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, plant as u64);
                    assert!(steps >= 2, "must have taken multiple chunks");
                    break;
                }
                SearchStep::Done(SearchEnd::End) => panic!("expected to find the planted needle"),
                SearchStep::More(..) => steps += 1,
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
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
        // the documented residual `safe_to` does NOT paper over: a leftmost alternative well
        // past MAX_MATCH_LEN can never complete within any hay this scan ever holds (the carry
        // itself caps at MAX_MATCH_LEN bytes -- batch 4 (2026-07-24), finding #10: MAX_MATCH_LEN,
        // not MAX_MATCH_LEN - 1, see `SearchForward`'s own ANCHOR CORRECTNESS doc comment), so it
        // silently drops out of consideration once the carry shrinks past its own start -- a
        // later, shorter, genuinely-findable match must still be returned, not a hang and not a
        // false miss. The over-cap length here is deliberately far past the cap (not merely one
        // byte over, which the corrected `keep = MAX_MATCH_LEN` -- one more byte of slack than
        // before -- can occasionally still resolve within the SAME iteration the match completes
        // in, an accident of alignment the "not GUARANTEED" cap documentation already allows for,
        // not a claim this scan tries to make false), so the unreachability holds regardless of
        // that one-byte shift.
        let max = crate::search::MAX_MATCH_LEN;
        let over = max + 4096; // unambiguously past the cap, whichever exact keep width applies
        let mut data = vec![b'y'; over + 200];
        data[0] = b'a';
        data[over - 1] = b'z'; // 1 (a) + (over - 2) any + 1 (z) == `over` bytes total
        data[over + 50] = b'b'; // the legitimately findable later match
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64);
        let pattern = format!("a.{{{}}}z|b", over - 2);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at,
                (over + 50) as u64,
                "the over-cap leftmost match is unreachable; the later match is the honest answer"
            ),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_is_confirmed_by_a_real_byte_even_at_exactly_max_match_len() {
        // batch 4 (2026-07-24), finding #10 CLOSES the residual this test used to be named
        // `..._can_still_false_positive_at_exactly_max_match_len` and pin as accepted: `safe_to`
        // closes the mid-file `pat$` chunk-boundary false positive for any match SHORTER than
        // `MAX_MATCH_LEN` (accepting requires `read_end - match_at > MAX_MATCH_LEN`, which forces
        // at least one byte of REAL slack past the match's own end); a match of EXACTLY
        // `MAX_MATCH_LEN` bytes used to be the one length where that slack could be zero (the OLD
        // margin, `MAX_MATCH_LEN - 1`, was calibrated with nothing left over for it) -- widening
        // the margin to a full `MAX_MATCH_LEN` (this struct's own ANCHOR CORRECTNESS doc comment)
        // gives even THIS length one real byte of slack too, so `$` is now confirmed by the real
        // 'Q' that follows, never hay's own artificial edge. Same fixture as the old residual
        // test (block_size 64: `max` is an exact multiple, landing the read boundary exactly on
        // the match's own end -- the worst-case alignment this needs to exercise at all), reversed:
        // `Found` here now means the REAL 'Q' correctly disproved `$`, i.e. `End`.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'y'; max + 200];
        data[0] = b'a';
        data[max - 1] = b'z'; // 1 (a) + (max - 2) any + 1 (z) == max bytes exactly
        data[max] = b'Q'; // real byte right after the match -- NOT `\n`; disproves `$`
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64);
        let pattern = format!("a.{{{}}}z$", max - 2);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "the real 'Q' must disprove $ now that the margin is a full MAX_MATCH_LEN, got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_still_matches_at_exactly_max_match_len_when_genuine() {
        // the positive twin: same exactly-cap alignment, but the file genuinely ENDS right after
        // the match (true EOF, not a real disproving byte) -- `$` must still be found, proving
        // the fix above didn't just start rejecting everything at this exact length.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'y'; max];
        data[0] = b'a';
        data[max - 1] = b'z';
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 64);
        let pattern = format!("a.{{{}}}z$", max - 2);
        let p = Arc::new(SearchPattern::compile(&pattern, false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 0),
            other => panic!("the file truly ends here; $ must hold, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_is_confirmed_by_a_real_byte_even_at_a_bounded_legs_own_limit() {
        // fix round (2026-07-25), F4 CLOSES this residual (reversed from `..._can_false_
        // positive_at_a_bounded_legs_own_limit`, which used to pin the false positive as
        // accepted): a bounded leg reaching its own artificial `limit` (never the true file
        // end -- a wrap leg's own bound) now peeks up to `CTX_BEHIND` real bytes past it,
        // consultable for `$`/`\b`/`\B` but never reportable (`SearchForward`'s own doc comment
        // has the full derivation, symmetric with decision 1's identical floor-context rule).
        // The real 'Q' right after "abc" now correctly disproves `$`.
        let mut data = vec![b'y'; 200];
        data[0..3].copy_from_slice(b"abc");
        data[3] = b'Q'; // real byte right after the match -- NOT `\n`; disproves `$`
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("abc$", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(3), 1 << 20); // limit=3: bounded exactly at "abc"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => {
                panic!("the real 'Q' past the leg's own limit must disprove $ now, got {other:?}")
            }
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_still_matches_at_a_bounded_legs_own_limit_when_genuine() {
        // the positive twin: same bounded-leg alignment, but the byte right after `limit` IS a
        // real `\n` -- `$` must still be found, proving the F4 peek doesn't just start
        // rejecting everything at the leg's own limit.
        let mut data = vec![b'y'; 200];
        data[0..3].copy_from_slice(b"abc");
        data[3] = b'\n'; // real byte past the limit -- confirms $ genuinely
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("abc$", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(3), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 0),
            other => panic!("a real \\n past the limit must confirm $, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_dollar_body_cannot_consume_bytes_past_a_bounded_legs_own_limit() {
        // fix round (2026-07-25), F4's own scope guard: the peek exists purely for lookaround
        // context, never as consumable match BODY -- "abcQ" would complete using the real byte
        // just past `limit`, but a leg bounded at `limit = 3` must never report a match whose
        // own span extends past it, symmetric with `find_all_starting_in`'s identical "wholly
        // contained" rule (search.rs). Ground truth here is `End`, not a match reaching into
        // territory this leg was told to stop at.
        let mut data = vec![b'y'; 200];
        data[0..4].copy_from_slice(b"abcQ");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("abcQ", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(3), 1 << 20); // limit=3: "abcQ" needs byte 3, past it
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "a match whose own body needs bytes past `limit` must stay unreported, got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_bounded_legs_final_peek_finds_the_same_match_at_every_budget() {
        // batch 6 (2026-07-28), finding #2. Before this fix, the F4 peek (`step`'s own
        // `at_final && !at_eof` branch) gathered its up-to-`CTX_AHEAD` consult-only bytes
        // INLINE, in the SAME call, and silently treated an `OutOfBudget` cutoff mid-gather as
        // "nothing more to see" -- rejecting a candidate the peek simply never got the CHANCE to
        // verify. `block_size = 2` makes the 4-byte peek span `[50, 54)` straddle a block
        // boundary (`[50,52)` / `[52,54)`), so a small `chunk` genuinely cannot gather it in one
        // read the way a single, oversized block would let it -- reproducing K1's own review
        // fixture shape (`a{4096}(?u:\b)`, scaled down here for speed; the mechanism is
        // block-boundary-driven, not length-driven, so the smaller run exercises it identically)
        // and its own observed symptom: `Exhausted`-shaped (`End`) at a low budget, `Found` at a
        // high one, on the IDENTICAL file. `budgets 1..=200` mirrors the established sweep idiom
        // (`document.rs`'s own `search_next_never_errors_across_a_composed_budget_sweep`, R3's
        // P1-2 fix) -- every one of them must now resolve to the SAME answer.
        let mut data = vec![b'a'; 50];
        // 4 real bytes past the 50-'a' run: a space (a genuine non-word byte -- confirms `\b`)
        // plus three filler bytes. The engine's own margin is a BLANKET `CTX_AHEAD` (4) for ANY
        // trailing assertion regardless of which byte actually decides it (`Hay::verified`'s own
        // doc comment) -- all 4 must be gathered before the peek can ever certify either way.
        data.extend_from_slice(b" bcd");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        assert_eq!(data.len(), 54);
        let p = Arc::new(SearchPattern::compile(r"a{50}(?u:\b)", false).unwrap());
        assert_eq!(
            p.find_all(data),
            vec![(0, 50)],
            "whole-file oracle: a real match, exactly once, disproving nothing about \\b"
        );
        for chunk in 1..=200usize {
            let c = cache(data, 2);
            let mut s = SearchForward::new(p.clone(), 0, Bound::Exclusive(50), chunk);
            let mut steps = 0;
            loop {
                steps += 1;
                assert!(
                    steps <= 1000,
                    "livelock at chunk={chunk}: {steps} consecutive More with no terminal outcome"
                );
                match s.step(&c).await.unwrap() {
                    SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                        assert_eq!(match_at, 0, "chunk={chunk}");
                        break;
                    }
                    SearchStep::Done(SearchEnd::End) => panic!(
                        "chunk={chunk}: budget-dependent miss -- a real match exists here \
                         regardless of how little budget any one call had to peek for it \
                         (\"budgets bound read work, not answers\", docs/budgeted_scanning.md)"
                    ),
                    SearchStep::More(..) => continue,
                }
            }
        }
    }
    #[tokio::test]
    async fn search_forward_reports_the_true_most_recent_newline_many_iterations_later() {
        // RE-DERIVED, batch 7 (2026-07-28), finding #1 -- renamed from `search_forward_margin_
        // deferral_keeps_whole_hay_newline_accounting_not_prefix_only`, whose own fixture and
        // expected value are both retired. Full history, because the retirement is a correction:
        //
        // K2's fix round added that test against a REAL bug (its first attempt routed a
        // margin-rejected deferral through `Pending`'s prefix-only `memrchr`, losing every
        // newline between `accept.start` and `s`; reviewer-measured 10/10 fixtures wrong). The
        // fix was right and survives -- what does not survive is the fixture, which used
        // `(?s).+(?u:\b)` over 20,000 bytes and asserted `line_start == Some(w + 1)`. That
        // expectation silently encoded a SECOND defect: at the base commit the same fixture
        // reported `match_at = 15869` on every one of its ten sweeps, while the whole-file oracle
        // for that pattern and data is `(0, 20000)` -- the leftmost match starts at 0, and
        // `Some(w + 1)` was simply the last newline before a start 15,869 bytes to the right of
        // it. Batch 7 reports `match_at = 0` (oracle-agreeing) and therefore `line_start = None`,
        // which is correct for a match at position 0 and is what made the old assertion fail.
        // Greedy `.+` overruns every window it is given; before the end bound, that overrun was
        // rejected outright at every start, so the leftmost match was reachable only once the
        // window had crawled far enough right for a short completion to survive by accident.
        //
        // What this test pins now is the invariant the original was written to protect, on a
        // fixture that isolates it: a match found MANY iterations into a scan must report the
        // true, most-recent newline before it -- one seen and recorded long before the iteration
        // that finds the match, and long since dropped from the carry.
        for w in [600u64, 700, 800, 900, 1000, 1100, 1200, 2000, 3000, 4000] {
            let mut data = vec![b'x'; 20_000];
            data[506] = b'\n';
            data[w as usize] = b'\n';
            data[15_000..15_006].copy_from_slice(b"needle");
            let data: &'static [u8] = Box::leak(data.into_boxed_slice());
            let c = cache(data, 512);
            let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
            // ground truth, from the pure whole-file oracle rather than restated by hand.
            assert_eq!(p.find_all(data), vec![(15_000, 15_006)], "w={w}");
            let mut s = SearchForward::new(p, 0, Bound::Exclusive(20_000), 512);
            let mut steps = 0;
            loop {
                steps += 1;
                assert!(steps <= 10_000, "livelock at w={w}");
                match s.step(&c).await.unwrap() {
                    SearchStep::Done(SearchEnd::Found {
                        match_at,
                        line_start,
                    }) => {
                        assert_eq!(match_at, 15_000, "w={w}");
                        assert_eq!(
                            line_start,
                            Some(w + 1),
                            "w={w}: the newline at {w} was read roughly {} iterations before the \
                             one that found this match, and dropped from the carry long before \
                             it -- `last_nl` must still carry it",
                            (15_000 - w) / 512
                        );
                        break;
                    }
                    SearchStep::Done(SearchEnd::End) => panic!("w={w}: expected Found"),
                    SearchStep::More(..) => continue,
                }
            }
        }
    }
    #[tokio::test]
    async fn search_forward_keeps_raw_find_calls_bounded_on_an_advancing_window() {
        // batch 6 (2026-07-28), K2 fix round, P1-2 (M-A direction), RE-POINTED by batch 7. The
        // guarantee is unchanged and the fixture is unchanged; what changed is what could break
        // it. K2 bought this bound with an early return steered by a caller-supplied
        // `CutAdvances`, and the pin existed because that value could be flipped at either call
        // site with the whole suite green. There is no such value any more (`hay.rs`'s own
        // `leftmost_confirmed` doc comment), so what this now guards is the end BOUND itself: if
        // `reportable_end` ever stopped bounding the search -- or a retry loop returned to this
        // walk -- the count would scale with the run length again.
        let n = 4000;
        let mut data = vec![b'a'; n];
        data.extend_from_slice(b" trailing content, never reached this call");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        // one block per step: the whole run arrives as a SINGLE fresh payload slice, landing
        // squarely on an ordinary (non-final) iteration -- `cache.size()` is well past `n` alone.
        let c = cache(data, 500);
        let p = Arc::new(SearchPattern::compile(r"a+\b", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 500);
        crate::search::hay::reset_raw_find_from_calls();
        match s.step(&c).await.unwrap() {
            SearchStep::More(..) => {} // nothing reportable in this hay yet; read on
            other => panic!("expected More, got {other:?}"),
        }
        let calls = crate::search::hay::raw_find_from_calls();
        assert!(
            calls <= 2,
            "O(1) expected on an advancing window -- got {calls} raw_find calls for an {n}-byte \
             run; a count scaling with n is finding #3's own quadratic, back"
        );
    }
    #[tokio::test]
    async fn search_forward_keeps_raw_find_calls_bounded_at_a_bounded_legs_final_boundary() {
        // batch 7 (2026-07-28), finding #3's own SCAN-LEVEL pin -- the boundary K2 deliberately
        // left quadratic and docketed (`hay.rs`'s own pre-batch-7 doc comment stated the residual
        // outright: "reviewer-measured, IDENTICAL at both this fix's base and head ... 5001/10001/
        // 20001 calls, 535 ms/2.09 s/8.01 s at 5k/10k/20k bytes"). The reviewer's own public
        // repro shape: a homogeneous run below a bounded leg's `limit`, with the pattern's
        // completion sitting just past it, so every start in the run is proposed and rejected.
        // Counted, never timed (`AGENTS.md`'s own rule, and `no_timing_oracles` enforces it) --
        // the call count IS the quadratic, and it is what the wall clock was only ever a proxy
        // for. Measured directly at the base commit while writing this: 20001 calls, 8.87s.
        let n = 20_000;
        let mut data = vec![b'x'; n];
        data.extend(vec![b'a'; 4100]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let p = Arc::new(SearchPattern::compile(r"x+a+", false).unwrap());
        let c = cache(data, 1 << 20);
        // `Bound::Exclusive(n)` puts the leg's own final boundary exactly at the run's end -- the
        // `at_final && !at_eof` precondition for `resolve_bounded_terminal`, the one call site
        // that used to pass `CutAdvances::Final`.
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(n as u64), 1 << 20);
        crate::search::hay::reset_raw_find_from_calls();
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps <= 1000, "livelock");
            match s.step(&c).await.unwrap() {
                SearchStep::Done(_) => break,
                SearchStep::More(..) => continue,
            }
        }
        let calls = crate::search::hay::raw_find_from_calls();
        assert!(
            calls <= 8,
            "O(1) expected at the final boundary -- got {calls} raw_find calls for an {n}-byte \
             run; one call per byte here is the 9.71s freeze finding #3 reported"
        );
    }
    #[tokio::test]
    async fn search_forward_finds_a_sub_cap_match_hidden_inside_an_undecidable_greedy_run() {
        // batch 7 (2026-07-28), finding #1's own SCAN-LEVEL pin -- the defect in the shape the
        // reviewer reported it in, driven through the real `step()` loop rather than through
        // `Hay` alone, because the loss needed BOTH halves: `leftmost_confirmed` stopping at the
        // first undecidable candidate AND this loop's own carry shrink then dropping the bytes
        // that candidate was promised to be re-offered from. A `Hay`-level RED can only ever show
        // the first half (this module's own
        // `leftmost_confirmed_finds_a_later_candidate_behind_an_undecidable_greedy_run`).
        //
        // The first alternative matches from 0 and runs to the window's own artificial edge,
        // where its `(?u:\b)` is undecidable; "needle" sits at 5,000, INSIDE that run, sub-cap
        // and entirely ordinary. The 'z' tail is what makes the whole-file answer unambiguous:
        // 'z' is outside `[A-Za-y]` but is still a word character, so the first alternative can
        // never complete anywhere in the real file -- the oracle's ONLY match is the needle.
        let mut data = vec![b'b'; 10_000];
        data[0] = b'A';
        data[5000..5006].copy_from_slice(b"needle");
        data.extend(vec![b'z'; 100]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let p = Arc::new(SearchPattern::compile(r"A[A-Za-y]*(?u:\b)|needle", false).unwrap());
        assert_eq!(
            p.find_all(data),
            vec![(5000, 5006)],
            "ground truth from the pure whole-file oracle"
        );
        // block size 10,000: the run and the needle arrive in ONE payload slice, and the carry
        // shrink that follows keeps only the last `consumable_reach()` (4,099) bytes -- so a
        // candidate deferred at 0 takes byte 5,000 down with it.
        let c = cache(data, 10_000);
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 10_000);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps <= 1000, "livelock");
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, 5000);
                    break;
                }
                SearchStep::Done(SearchEnd::End) => panic!(
                    "the sub-cap needle was discarded unexamined -- finding #1: the walk stopped \
                     at the undecidable candidate at 0, and the carry shrink then dropped byte \
                     5,000 before any later call could look at it"
                ),
                SearchStep::More(..) => continue,
            }
        }
    }
    #[tokio::test]
    async fn search_forward_bounded_legs_final_boundary_still_recovers_a_shorter_alternative() {
        // batch 6 (2026-07-28), K2 fix round, P1-2 (M-B direction) -- a SCAN-LEVEL pin, RE-POINTED
        // by batch 7. It was written to catch a flipped `CutAdvances` constant at
        // `resolve_bounded_terminal`'s own call site (which would have made an undecidable
        // candidate DEFER at a boundary that never runs again, silently losing the shorter
        // alternative). There is no constant there any more, and no deferral anywhere, so what
        // this pins now is the GUARANTEE rather than the mechanism that used to deliver it: a
        // candidate the end rule cannot accept at this leg's own final boundary must not take a
        // shorter, wholly-decidable alternative down with it. Batch 7 satisfies that more
        // directly than the retry did -- the shorter alternative is what the end-bounded search
        // returns in the first place, rather than what a retry recovers afterwards.
        //
        // Fixture: `abcd(?u:\b)|bc`, `limit=4`, file = "abcd" + 3 of 𐐀's own 4 UTF-8 bytes (an
        // INCOMPLETE lead sequence past `limit`, `search_forward_needs_all_four_ctx_ahead_bytes_
        // not_three`'s own established mechanism -- decodes as non-word regardless of the real,
        // not-yet-fully-read character). The FIRST candidate the walk reaches, "abcd(?u:\b)" at
        // (0,4), needs `\b` at position 4 = `limit` = the artificial edge: with `hay_bytes.len()
        // == 7` (4 + 3 peeked bytes), `lookahead_ok(4, 4)` is `7-4=3 < 4` -- margin-rejected,
        // genuinely undecided (the real 4th byte of 𐐀 could still complete a WORD character,
        // which would disprove \b). Batch-4 R1's alternative-recovery must still find "bc" at
        // (1,3): `lookahead_ok(3, 4)` is `7-3=4 >= 4` -- satisfied using bytes ALREADY held, no
        // peek needed for this shorter candidate at all.
        let mut data = b"abcd".to_vec();
        data.extend_from_slice(&"\u{10400}".as_bytes()[..3]); // 3 of 4 bytes: incomplete lead
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        assert_eq!(data.len(), 7);
        let p = Arc::new(SearchPattern::compile(r"abcd(?u:\b)|bc", false).unwrap());
        let c = cache(data, 1 << 20);
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(4), 1 << 20);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps <= 1000, "livelock");
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(
                        match_at, 1,
                        "expected the shorter alternative \"bc\" (start=1) to be reported instead \
                         of the longer, undecidable \"abcd(?u:\\b)\" candidate, per batch-4 R1's \
                         alternative-recovery -- a candidate this leg's own final boundary cannot \
                         decide must never take a decidable one down with it"
                    );
                    break;
                }
                SearchStep::Done(SearchEnd::End) => {
                    panic!("expected Found(\"bc\") via alternative-recovery, got End")
                }
                SearchStep::More(..) => continue,
            }
        }
    }
    #[tokio::test]
    async fn search_forward_unicode_word_boundary_needs_the_full_trailing_character_not_one_byte() {
        // batch 5 fix round (2026-07-26), review response, P3-6: renamed from
        // `..._dollar_needs_...` -- the pattern below is `(?u:\b)`, never `$`, and `$` is
        // precisely the ONE trailing assertion that does NOT need `CTX_AHEAD` (this struct's own
        // top-level doc comment, the `$` paragraph: it needs only the one byte finding #10
        // already secured). batch 5 (2026-07-26), finding #3: the trailing assertion at a cap-length match's own
        // end needs the FULL following character (up to CTX_AHEAD real bytes), not merely the one
        // byte finding #10 (batch 4) secured -- a Unicode-aware `(?u:\b)` decodes an incomplete
        // lead/continuation byte as non-word regardless of what the real, not-yet-fully-read
        // character actually was (the same "no sentinel" hazard `CTX_BEHIND` already closed on
        // the OTHER side, batch 4 finding #9). `é` (2 bytes, 0xC3 0xA9) is a Unicode word
        // character, so a run of `a`s immediately followed by it must NOT satisfy `(?u:\b)` there
        // -- but reading just 1 byte (0xC3 alone, invalid UTF-8 on its own) used to be enough for
        // the OLD margin to accept the match anyway. block_size 1 exposes the exact moment only
        // one byte of `é` is visible; a huge chunk still resolves this in one `step` call.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'a'; max];
        data.extend_from_slice("é".as_bytes()); // 0xC3, 0xA9 -- a Unicode word character
        data.extend_from_slice(b"zzzz"); // real bytes past é so full information is available
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 1);
        let p = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert!(
            p.find_all(data).is_empty(),
            "whole-file oracle: é is a word character, (?u:\\b) must not hold there"
        );
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => panic!(
                "fabricated (?u:\\b) at {match_at} from a lone lead byte of é -- the whole-file \
                 oracle (find_all) correctly finds no match here"
            ),
            other => panic!("expected a decisive terminal result, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_carry_shrink_retains_a_deferred_cap_length_candidate() {
        // batch 5 fix round (2026-07-26), review response, P3-1: `keep` (the carry-shrink cap)
        // needs to be `MAX_MATCH_LEN + CTX_AHEAD - 1`, not `MAX_MATCH_LEN` alone, or a DEFERRED
        // candidate near carry's own low end can be discarded before it ever gets a chance to
        // become safe. No assertion is needed to exercise this -- `safe_to`'s own gate
        // (`match_at < safe_to`) applies uniformly to whatever `find_starting_in` returns,
        // regardless of whether the pattern has a trailing assertion at all, so a bare `a{4096}`
        // (cheap: no Unicode-mode sub-expression, no enumeration) is enough. `block_size` is
        // chosen so the FIRST read lands `read_end` at EXACTLY `MAX_MATCH_LEN + CTX_AHEAD - 1`
        // past `match_at` (0) -- the worst-case deferred distance (`safe_to` is exactly `0` there,
        // so `match_at(0) < safe_to(0)` is false: JUST barely not yet safe) -- so the very next
        // carry-shrink is what decides whether this candidate survives at all.
        let max = crate::search::MAX_MATCH_LEN;
        let margin = max + crate::search::CTX_AHEAD.get() - 1;
        let mut data = vec![b'a'; max];
        data.extend(vec![b'z'; margin - max + 10]); // pads read 1 to exactly `margin` bytes, plus a little more to read on step 2
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, margin);
        let p = Arc::new(SearchPattern::compile(&format!("a{{{max}}}"), false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 0,
                "the deferred a{{max}} candidate at 0 must survive the carry-shrink and be found \
                 once safe, not be silently discarded by a too-narrow retention window"
            ),
            other => panic!(
                "expected Found once the candidate becomes safe -- a too-narrow carry-shrink \
                 discards its own start before that, producing a false {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_f4_peek_needs_the_full_trailing_character_at_a_bounded_limit() {
        // batch 5 fix round (2026-07-26), review response, P3-1: the F4 peek's own width (past a
        // bounded leg's own `limit`, never the true file end) needs to be `CTX_AHEAD`, not the 1
        // byte finding #10's own margin secured -- the identical (?u:\b) hazard as the mid-file
        // case, just at the OTHER place a slice's own edge can masquerade as a real boundary.
        // `limit = 1` bounds this leg to just "y"; the real file continues past it with `é` (a
        // Unicode word character) -- `y(?u:\b)` must NOT match (word 'y' -> word é, no boundary),
        // but reading only é's own lead byte past `limit` used to be enough to fabricate it.
        let data = b"y\xc3\xa9zz"; // 'y', then real e-acute (0xC3 0xA9), then filler
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile(r"y(?u:\b)", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(1), 1 << 20); // limit=1: bounded exactly at "y"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => panic!(
                "fabricated (?u:\\b) at {match_at} from é's own lone lead byte past the leg's own \
                 limit -- é is a real word character, no boundary exists"
            ),
            other => panic!("expected a decisive terminal result, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_needs_all_four_ctx_ahead_bytes_not_three() {
        // batch 5 fix round (2026-07-26), review response, P3-1: `é` (2 bytes) only ever needs
        // `CTX_AHEAD >= 2` -- it cannot discriminate `CTX_AHEAD == 4` from a mutant `CTX_AHEAD ==
        // 3` (or `2`), which would still pass every other test in this unit. `𐐀` (U+10400, a
        // genuine 4-byte UTF-8 word character) is the first fixture that actually needs all 4
        // bytes: reading only 3 of its own 4 bytes leaves an incomplete lead sequence, still
        // invalid on its own, still decoded as non-word.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'a'; max];
        data.extend_from_slice("\u{10400}".as_bytes()); // 4-byte UTF-8, a real word character
        data.extend_from_slice(b"zzzz");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 1);
        let p = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert!(
            p.find_all(data).is_empty(),
            "whole-file oracle: 𐐀 is a word character, (?u:\\b) must not hold there"
        );
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => panic!(
                "fabricated (?u:\\b) at {match_at} from an incomplete (3-of-4-byte) lead sequence \
                 of 𐐀 -- the whole-file oracle (find_all) correctly finds no match here"
            ),
            other => panic!("expected a decisive terminal result, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_finds_a_later_alternative_when_the_leftmost_overruns_a_bounded_legs_own_limit()
     {
        // fix round 2 (2026-07-25), R1: F4's own peek reintroduced F3's exact bug shape (search.
        // rs's `find_all_starting_in`, batch 4 fix round finding #3) here in `SearchForward`.
        // `find_starting_in` is LEFTMOST (batch 3 (2026-07-23), finding #2): with the peek
        // extending `hay` past `limit`, "abcQ" (starting at 0) now COMPLETES using the peeked
        // 'Q' and is returned ahead of "bc" (starting at 1, wholly within payload) -- correctly
        // REJECTED by the `e <= payload_end` guard (its own body consumed a peek-only byte), but
        // the surrounding code then fell straight through to "not yet safe: keep reading", the
        // wrong reasoning for this rejection reason: there is no next iteration that will ever
        // produce more bytes for a leg already at its own `limit`, so `self.pos` advances to
        // `size` and the very next loop check exits with `SearchStep::End` -- "bc" is never even
        // retried, even though it needs nothing this leg hasn't already read. Reviewer's exact
        // fixture: `abcQ|bc` over data starting "abcQ", `limit=3` -- pre-F4 (a39c78e) correctly
        // finds `Found{match_at:1}` (no peek existed to manufacture the "abcQ" distraction);
        // 229bdb5 (F4, pre-R1) regresses to `End`.
        let mut data = vec![b'y'; 200];
        data[0..4].copy_from_slice(b"abcQ");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("abcQ|bc", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(3), 1 << 20); // limit=3: "bc" (1..3) fits, "abcQ" (0..4) doesn't
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 1,
                "the later, wholly-contained alternative must still be found"
            ),
            other => panic!(
                "an overrun leftmost candidate must not blind the leg to a later, in-bounds one, got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_line_start_is_none_when_no_newline_precedes_the_match_in_scan_range() {
        // the only `\n` (at byte 0) is FIVE bytes before the origin (5, at origin - 5); `ctx`
        // (this struct's own ANCHOR CORRECTNESS mechanism) looks back up to `CTX_BEHIND` (4)
        // real bytes (batch 4 (2026-07-24), finding #9 -- was a single byte, origin - 1, before
        // this), so this one -- one byte further back than the reach -- genuinely stays out of
        // the scan's view; the caller must still resolve it itself. The immediately-adjacent
        // case (`\n` AT origin - CTX_BEHIND, the new edge of visibility) is pinned by
        // `search_forward_line_start_is_resolved_from_a_real_look_behind_byte`, just below:
        // unlike this one, it IS now visible.
        let c = cache(b"\nwwwwneedle xyz", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 5, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                assert_eq!(match_at, 5);
                assert_eq!(line_start, None);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_line_start_is_resolved_from_a_real_look_behind_byte() {
        // the only `\n` is exactly ONE byte before the origin (4, at origin - 1) -- well within
        // `ctx`'s own `CTX_BEHIND`-byte reach (read lazily for anchor correctness, this struct's
        // own ANCHOR CORRECTNESS doc comment), so `line_start` is now resolved directly instead
        // of falling back to `None` -- a side benefit of the same mechanism, not a separate
        // feature: `line_start_shortcut` (document.rs) already treats a `Some` hint and a
        // `None`-then-self-resolve as equally correct, just cheaper here.
        let c = cache(b"zzz\nneedle xyz", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 4, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found {
                    match_at,
                    line_start,
                }) => {
                    assert_eq!(match_at, 2);
                    assert_eq!(
                        line_start, None,
                        "a persisted newline INSIDE the match is not a valid line start"
                    );
                    return;
                }
                SearchStep::Done(SearchEnd::End) => panic!("expected to find foo\\nbar"),
                SearchStep::More(..) => {}
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 32);
        match s.step(&c).await.unwrap() {
            SearchStep::More(..) => {}
            other => panic!("expected More, got {other:?}"),
        }
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, 200);
                    break;
                }
                SearchStep::More(..) => {}
                SearchStep::Done(SearchEnd::End) => panic!("expected to find the needle"),
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 0),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            s.progressed(),
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(4), 1 << 10); // limit=4, needle starts at 4
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
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
        let s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 4096);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: data.len() as u64,
        });
        match s.complete(&c, tx, data.len() as u64).await.unwrap() {
            SearchEnd::Found { match_at, .. } => assert_eq!(match_at, plant as u64),
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(rx.borrow().scanned > 0, "progress published per chunk");
    }
    #[tokio::test]
    async fn search_forward_terminal_check_does_not_fabricate_a_match_at_a_stale_size() {
        // batch 5 (2026-07-27), finding #10-internal: the forward twin of finding #1
        // (`search_backward_wrap_leg_terminal_check_does_not_fabricate_a_match_at_a_stale_size`,
        // this module's own sibling test) -- `SearchForward`'s early zero-width-at-EOF terminal
        // check used the ctx seed's own output directly, with no certification: a source
        // claiming size 100 over 2 real bytes (`b"ab"`) left the seed's `ctx` EMPTY (the seed
        // loop breaks on `take == 0` immediately), `phantom` false on empty, and
        // `rfind_starting_in`'s forward twin, `find_starting_in(&[], 0..1)`, matches `$`
        // trivially -- the OLD code returned `Found { match_at: 100 }`, a wholly fabricated
        // position.
        //
        // batch 5 fix round 2 (2026-07-27), review response, P1: this fixture's own expected
        // outcome changed from the original fix round's own `Found { match_at: 2 }`. The seed's
        // own retry loop still DESCENDS and CERTIFIES the real end (2) correctly -- but `from`
        // (100 here) is this leg's own INCLUSIVE lower bound on what may be REPORTED, and 2 sits
        // below it: reporting it anyway was itself a defect (P1, below), a match START below the
        // leg's own origin, not merely a smaller-magnitude fabrication than 100. Correct: `End`
        // -- leg 1 has nothing at-or-after 100 (the file does not even reach there), and the wrap
        // leg (not exercised by this scan-level fixture; see the document-level test below) is
        // what correctly finds 2 with `wrapped: true`. Still proves the ORIGINAL defect is gone:
        // neither 100 (the old fabrication) nor a report of 2 (the P1 regression) comes back.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 100, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "expected End -- nothing at-or-after origin (100) exists in this 2-byte file; a \
                 fabricated Found at 100 OR a report of the real end (2, below origin) would both \
                 be this defect class, got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_terminal_check_does_not_report_below_its_own_origin() {
        // batch 5 fix round 2 (2026-07-27), review response, P1: renamed and re-fixtured from a
        // now-impossible combination. This test used to pin phantom rejection reached through the
        // stale-size EARLY CHECK specifically (`origin` set to the claimed size itself) -- but
        // that combination cannot arise anymore under the P1 fix below: ANY fixture where the
        // seed's retry actually descends necessarily has `origin` above the real end (descending
        // is only ever triggered by a short read, which only happens once `self.pos` -- starting
        // at `origin` -- has already passed the real end), and the P1 fix below makes EVERY such
        // case resolve to `End` before the early check's own gate could ever fire again (the gate
        // needs `self.pos >= cache.size()`, but a certified, descended `self.pos` is now BELOW
        // `origin`, hence below the stale `cache.size()` too, whenever `origin` sits below it).
        // Phantom rejection at genuine, non-truncated EOF stays covered by this module's own
        // pre-existing, unrelated tests (batch 4, finding #2); this fixture instead pins the
        // reviewer's own P1 shape directly: `from` set to an ARBITRARY value (3) that matches
        // neither the claimed size (100) nor the real end (2), proving the origin-floor rule
        // generalizes beyond the "origin happens to equal the stale claim" case this unit's own
        // sibling test above exercises. `SearchForward::new`'s own contract makes `from`
        // INCLUSIVE -- "the first byte at-or-after which a match START counts"
        // (`document.rs:1060-1064` depends on this for its own exclusive-repeat navigation) -- so
        // a certified match at 2, strictly below `from` (3), is not this leg's to report: nothing
        // at-or-after 3 exists in this 2-byte file, and `End` is the only honest answer. Before
        // this fix, `825bed6` reported `Found { match_at: 2 }` here -- a match START below the
        // leg's own origin, the exact defect the review's own P1 finding names.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 3, Bound::Exclusive(u64::MAX), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "expected End -- nothing at-or-after origin (3) exists; a reported match START \
                 below origin ({other:?}) is exactly the P1 defect this test pins"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_terminal_check_pends_across_calls_before_re_arming() {
        // the SAME fixture as the fabrication test above, but with a small `chunk` so the
        // descent from the stale size (100) down to the real end (2) cannot finish inside one
        // `step` call -- it must pend (`More`) at least once, resume on a LATER call (via
        // `self.phase` staying `FwdPhase::SeedCtx`, restructure R6 -- simpler than
        // `SearchBackward`'s own `BwdPhase::Certify` re-entry, since there is no `self.hi ==
        // cache.size()`-style condition here that stops being re-derivable once `self.pos`
        // moves: the phase discriminant alone is the gate, and it only ever transitions
        // `SeedCtx -> Scan`, once, for good). Mutation-
        // verified (stated for the record, not by a second checked-in test): reverting the seed
        // to its own OLD one-shot form (no retry, no descent) makes this fixture return a
        // fabricated `Found { match_at: 100 }` on the very first call, never reaching a second one
        // at all -- this test's own `saw_more` assertion would never even get the chance to fail,
        // since the fabrication happens instead; the fabrication test above is what actually
        // catches that mutation.
        //
        // batch 5 fix round 2 (2026-07-27), review response, P1: the terminal outcome changed
        // from `Found { match_at: 2 }` to `End`, same as the fabrication test above -- `from`
        // (100) sits above the certified real end (2), so P1's own origin-floor now applies here
        // too. The re-arm itself is UNCHANGED and still needs pinning across a resumed call: the
        // descent that CERTIFIES `self.pos` still has to pend and resume regardless of what the
        // eventual resolution turns out to be, and `saw_more` proves that happened.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(p, 100, Bound::Exclusive(u64::MAX), 8);
        let mut saw_more = false;
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 100,
                "livelock: {steps} consecutive steps, no terminal result"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => {
                    saw_more = true;
                    continue;
                }
                terminal => break terminal,
            }
        };
        assert!(
            saw_more,
            "the small chunk must force the descent to pend at least once -- otherwise this \
             fixture does not actually exercise a resumed call, and the re-arm stays untested"
        );
        match outcome {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "expected End across a resumed call -- nothing at-or-after origin (100) exists \
                 in this 2-byte file; a {other:?} here would mean either the old fabrication or \
                 the P1 regression survived the resume"
            ),
        }
    }
    #[tokio::test]
    async fn search_forward_terminal_check_does_not_skip_a_partially_short_blocks_real_content() {
        // batch 5 fix round 2 (2026-07-27), review response, P3-1: pins the `is_empty` half of
        // `wholly_empty_first_block` specifically, distinct from the sibling test above (which
        // pins the block-jump-vs-byte-exact CHOICE, given the flag, but uses a fixture where
        // every touched block genuinely IS wholly empty throughout -- neutering `&&
        // block.is_empty()` there changes nothing, since `p == ctx_start` alone already agrees
        // with it on every retry). This fixture is built specifically so the two conditions
        // DISAGREE: block1 (`[64, 128)`) has a few real bytes (`[64, 68)`) at its own START, not
        // wholly empty, so a read landing past them (`lo > 4`) is short-but-nonempty, not empty --
        // finding #10-internal's own class of bug if skipped over (the backward twin's own
        // P2-NEW history, `SearchBackward::step`, this module, found the identical shape).
        //
        // The margin is engineered to be LARGE and reliable, unlike a naive attempt: real content
        // sits at the partially-short block's own START (bytes `[64, 68)`, not e.g. `[96, 100)`),
        // so the CORRECT byte-exact creep has to travel almost the block's own full width (124
        // down to 68, 14 retries) before certifying, while the WRONG block-jump reaches a
        // certified (but incorrect) answer in essentially one retry (128 -> 64, then certifies
        // immediately using block0's own real content) -- an 8x-or-more gap, comfortably clear of
        // off-by-one noise, unlike a same-scale attempt with the real bytes near the block's own
        // END (which very nearly converges: `chunk: CTX_BEHIND` makes total step count the number
        // of retries either way, and a real portion near the block's own end needs almost no
        // extra byte-exact creep to certify, so the two mechanisms' own step counts nearly agree
        // by coincidence there -- probe-verified empirically before choosing this shape, not
        // assumed: an earlier attempt using bytes `[64, 124)` real gave 300 steps EITHER way).
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let mut real = vec![b'a'; 64]; // block0 ([0, 64)): fully real
        real.extend(vec![b'a'; 4]); // block1 ([64, 128)): only [64, 68) real
        let source = OverstatesItsOwnSize {
            real,
            claimed_size: 128,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchForward::new(
            p,
            128,
            Bound::Exclusive(u64::MAX),
            crate::search::CTX_BEHIND.get(),
        );
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 50,
                "livelock: {steps} consecutive steps, no terminal result"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        // the is_empty gate keeps this at ~16 (byte-exact creep through block1's own real
        // content, [64, 68), is required to certify correctly at 68); a naive take==0 trigger
        // (ignoring whether the block is truly empty) collapses this to ~2 (a wrong block-jump
        // straight to block1's own start, 64, skipping its real content entirely, then
        // certifying immediately using block0's own real content instead) -- a floor comfortably
        // between the two catches the wrong-and-faster mechanism without pinning an exact count.
        assert!(
            steps >= 10,
            "only {steps} step calls -- too few for the is_empty gate's own byte-exact creep \
             through block1's real content; this is the wrong-block-jump shortcut instead, \
             skipping real content the way finding #10-internal's own class of bug does"
        );
        assert!(
            matches!(outcome, SearchStep::Done(SearchEnd::End)),
            "expected End -- origin (128) sits above the certified real end regardless; this \
             test's own concern is the STEP COUNT to get there, not the terminal outcome; got \
             {outcome:?}"
        );
    }
    #[tokio::test]
    async fn search_forward_terminal_check_skips_whole_empty_blocks_not_one_byte_at_a_time() {
        // batch 5 fix round 2 (2026-07-27), review response, P3-1: the forward twin of
        // `search_backward_wrap_leg_terminal_check_skips_whole_empty_blocks_not_one_byte_at_a_
        // time` (this module's own sibling, unit F's own P3-4) -- a source whose claimed `size()`
        // overstates its real length by many WHOLE blocks, not just a handful of bytes. `chunk`
        // is pinned to `CTX_BEHIND` itself, so EVERY retry -- byte-exact or block-sized --
        // immediately meets it and ends its own `step` call: the total `step`-call count becomes
        // a direct, non-timing proxy for retry granularity. One call per BLOCK through the empty
        // span (this fix) versus one call per `CTX_BEHIND`-byte creep through it (the reverted
        // code) differ by more than an order of magnitude for a 300-block stale span, so a loose
        // bound cleanly separates them without pinning an exact count. Reviewer measured the
        // UNFIXED shape directly: ~55 ms/MiB (linear), 1024 `step` calls for a 4096-byte span at
        // `chunk: 4` -- cited here as the mechanism this test replaces with a bounded count, not
        // as a timing assertion of its own.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let bs: u64 = 64;
        let stale_blocks: u64 = 300;
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: bs * stale_blocks,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        // `from` matches the claimed size exactly, the same wider reachability shape this unit's
        // own sibling tests use -- P1 (above) means the terminal outcome is `End` regardless (the
        // certified real end, 2, sits far below it), so this test's own concern is purely the
        // STEP-COUNT it takes to get there, not what it resolves to.
        let mut s = SearchForward::new(
            p,
            bs * stale_blocks,
            Bound::Exclusive(u64::MAX),
            crate::search::CTX_BEHIND.get(),
        );
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 500,
                "{steps} step calls to cross a {stale_blocks}-block stale span -- a block-sized \
                 jump keeps this in the low hundreds (roughly one call per block), not the \
                 thousands a CTX_BEHIND-byte creep through the same span would need"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        assert!(
            matches!(outcome, SearchStep::Done(SearchEnd::End)),
            "expected End -- the real data sits entirely below origin, so nothing in this leg's \
             own [origin, real_end) range exists to find; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn search_backward_finds_the_last_match_below_the_origin() {
        // "needle" at [4,10) and [39,45); origin above both must land on the closer one (39),
        // matching forward's "skip the earlier one" twin. The `\n` at 14 sits far more than
        // CTX_BEHIND (4) bytes below the closer match's own block start (batch 4 (2026-07-24),
        // finding #9 widened look-behind from one byte to up to CTX_BEHIND -- a 20-byte 'x'
        // filler gap keeps the \n genuinely out of reach regardless of block alignment, unlike
        // the OLD, narrower gap this fixture used before the widening made it reachable), so
        // this still pins the "bounded, not carried" line_start.
        let mut data = b"aaa needle bbb\n".to_vec();
        data.extend(vec![b'x'; 20]);
        data.extend_from_slice(b"needle ddd\n");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, data.len() as u64, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                assert_eq!(match_at, 35, "must land on the closer match, not 4");
                assert_eq!(
                    line_start, None,
                    "the \\n at 14 is far outside this match's own look-behind reach"
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
        let mut s = SearchBackward::new_leg(p, 10, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                assert_eq!(match_at, 4);
                assert_eq!(line_start, Some(4), "the \\n at 3 makes 4 a line start");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_does_not_rebase_an_incomplete_look_behind_as_adjacent() {
        // batch 8 (2026-07-29), P1 -- the LOOK-BEHIND half of the class batch 7's own finding #2
        // closed on the CARRY half (`search_backward_does_not_splice_a_match_across_an_undelivered_
        // gap`, document.rs). This struct assembles `ctx ++ slice ++ carry` and bases the whole
        // hay at `Abs(lo - ctx.len())` -- arithmetic that ASSERTS `ctx`'s own top byte sits
        // immediately below `lo`. The per-iteration gather walks UPWARD from `ctx_start` toward
        // `lo`, so a short read leaves its gap at the TOP of what was gathered, orphaning every
        // byte below it from `lo` and making that assertion false by exactly the gap's own width.
        //
        // DIRECTION is why the identical "break on a gap" idiom is correct in the one sibling
        // that shares it and wrong here. `Document::edge_context`'s own behind-side gather
        // (`edge_context_ctx_before_drops_bytes_below_a_conforming_short_blocks_own_gap`)
        // descends FROM its anchor, so whatever it keeps stays adjacent to that anchor and a gap
        // only truncates the far end. The other two gathers in this module (`SearchForward`'s own
        // ctx seed, this struct's own wrap-leg `Certify`) never rebase a partial at all -- they
        // descend and RETRY until the gather is complete. This site was the one outlier.
        //
        // WHAT A GAP CAN AND CANNOT CORRUPT, precisely. A gap forces `lb < CTX_BEHIND` (a FULL
        // 4-byte ctx means the walk reached `lo`, so there was no gap), which bounds the damage:
        //   - `match_at` is always right regardless: `to_abs(s) == lo + (s - lb)` for every
        //     accepted match, so the SLICE half of the same base arithmetic is independently
        //     correct and the reported match position never moves;
        //   - `^`/`$`/`\b` are still decided against real, correctly-positioned bytes. `Hay`'s own
        //     `lookbehind_ok` refuses any assertion-carrying match with less than `CTX_BEHIND` of
        //     look-behind, and a gap cannot fabricate `low_is_true` either (`base == 0` needs
        //     `lb == lo`, which forces a COMPLETE gather), so an accepted one has `s.0 >= 4 > lb`
        //     and the byte below it comes from `slice`;
        //   - `line_start`'s own `memrchr` is the leak: it scans the WHOLE hay below the match
        //     with no margin bounding it at all, so a `\n` among the mis-based ctx bytes resolves
        //     to a fabricated ABSOLUTE position. That is the anchor-invariant violation.
        //
        // 32 bytes, block_size 8, block 0 answering 7 of its own 8 -- CONFORMING under
        // `BlockSource`'s own "up to `len`" contract (`source.rs`), with all 32 bytes genuinely
        // present and `size()` accurate. Iteration 1 reads block 2 [16,24); iteration 2's payload
        // is block 1 [8,16), so `ctx_start = 8 - CTX_BEHIND(4) = 4`: the walk gathers positions
        // 4-6 and then finds position 7 unreachable through that same short block. Basing at
        // `Abs(8 - 3) = Abs(5)` claims the `\n` really at 4 is the byte at 5, so the `\n` that
        // makes 5 a line start reports 6 instead.
        struct ShortFirstBlock {
            real: Vec<u8>,
            first_len: usize,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortFirstBlock {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                let first_len = self.first_len;
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    let real = real.clone();
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let want = if offset == 0 { first_len } else { len };
                        let end = (start + want).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"xxxx\nabcdefWORLD................";
        assert_eq!(real.len(), 32);
        let c = BlockCache::new(
            Arc::new(ShortFirstBlock {
                real: real.to_vec(),
                first_len: 7,
            }),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("WORLD", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 24, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                assert_eq!(match_at, 11, "the match position itself was never at risk");
                // the only `\n` is at 4, so 5 is the true line start -- but positions 5-7 were
                // never delivered, so no honest answer is available from bytes in hand. `None`
                // is that honest answer: `Document::resolve_found` routes it to a real
                // `BackwardScan` hunt, which reads position 4 and lands on 5.
                assert_eq!(
                    line_start,
                    Some(5),
                    "batch 12 (2026-07-29), the gap refill: the `\\n` at 4 is READABLE now, so the \
                     true line start is reported outright. Before the refill this was `None` (the \
                     honest conservative answer once R7 refused to rebase the gapped look-behind) \
                     and before R7 it was a fabricated `Some(6)`. The R7 refusal itself is still \
                     guarded, at the type level, by `hay.rs`'s own `Assembly` unit tests -- this \
                     fixture simply no longer has a gap for it to refuse"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_reports_end_at_top() {
        let c = cache(b"aaa bbb ccc", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 11, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("expected End, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_zero_width_dollar_from_the_true_eof() {
        // batch 3 (2026-07-23), finding #9's backward twin: a genuine wrap leg (`wrap_leg: true`
        // -- `wrapped_leg`'s own unconditional `hi = size`, document.rs) must find the same
        // zero-width `$` this file's own unterminated end offers. batch 4 (2026-07-24), finding
        // #1 narrowed WHO this applies to: `hi` still excludes itself everywhere else (self-hit
        // avoidance), and -- now -- at the true file end too, UNLESS this leg is the wrap's own
        // unconditional far end, which this fixture deliberately is (see this test's own
        // `wrap_leg: false` twin, `search_backward_leg_one_does_not_admit_an_eof_self_hit`,
        // just below, for the excluded case).
        let c = cache(b"abc", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 3, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 3),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_leg_one_does_not_admit_an_eof_self_hit() {
        // batch 4 (2026-07-24), finding #1: the `wrap_leg: false` twin of the test just above --
        // same fixture, only the flag differs. `hi == cache.size()` alone can't tell a genuine
        // wrap leg's own unconditional far end from leg 1's own EXCLUSIVE origin merely
        // happening to coincide with EOF (a cursor already parked on a zero-width match at the
        // true end, repeating the SAME search) -- the plain constructor's own contract (`new`'s
        // own doc comment: "not including hi") must hold even at this one position, or a caller
        // stuck on the very last match could never advance past it without an explicit wrap.
        let c = cache(b"abc", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 3, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!("expected End (leg 1 must exclude the EOF self-hit), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_rejects_a_zero_width_match_whose_predecessor_is_a_phantom_newline() {
        // batch 4 (2026-07-24), finding #2: position `hi`, when its real predecessor IS `\n`,
        // is the trailing newline's own phantom -- never a match, regardless of pattern (the
        // doctrine architecture.md:418-421 already states for `goto_line`, unified here).
        // `wrap_leg: true` so the terminal check runs at all (the case this rule targets); the
        // main loop then finds the REAL match at 1 instead, proving this is a rejection, not a
        // hang or a silent `End`.
        let c = cache(b"a\n", 8);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 2, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                assert_eq!(
                    match_at, 1,
                    "must find the REAL match, not the phantom at 2"
                )
            }
            other => panic!("expected Found (the real match at 1), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_does_not_fabricate_a_match_at_a_stale_size() {
        // batch 5 (2026-07-26), finding #1: the wrap-leg terminal check's own ctx loop breaks on
        // `take == 0` (a short/empty block) leaving `ctx` EMPTY whenever the source's `size()` is
        // stale (`PreadSource::size` never re-stats -- a truncated file reads back short/empty
        // forever after, `size()` unchanged) -- `phantom` is false on an empty `ctx`, and
        // `rfind_starting_in(&[], 0..1)` matches `$` trivially (an empty hay's own end), so the
        // OLD code returns `Found { match_at: 100 }`, a wholly fabricated position past the real
        // 2 bytes of data. Correct: the leg DESCENDS to the real end (2), where the real
        // predecessor IS `\n` -- a phantom, never a match -- so the only real match, found via the
        // normal main loop once `hi` is corrected, is at 1.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"a\n".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 100, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                assert_eq!(
                    match_at, 1,
                    "must find the real match at 1, not a fabricated one at the stale size (100) \
                     or the phantom at the real end (2)"
                )
            }
            other => panic!("expected Found (the real match at 1), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_re_arms_to_find_the_real_end() {
        // batch 5 (2026-07-26), finding #1's own other half: descending past a stale `hi` is not
        // enough on its own -- staying in `BwdPhase::Certify` (restructure R6; formerly a
        // `terminal_checked` re-arm) must let the zero-width check re-run at the REAL end once
        // reached, or a legitimate zero-width match there is silently missed (a
        // false `Exhausted`/`End`, never reachable any other way: the main loop's own accept range
        // excludes `hi` itself unconditionally, so only the terminal check can ever confirm a
        // match sitting exactly at the file's own true end). Real data `b"ab"` (no trailing `\n`,
        // so no phantom): the true end (2) is itself a genuine, unconditional `$` match.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 100, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 2,
                "the legitimate zero-width $ at the real end (2) must still be found"
            ),
            other => panic!(
                "expected Found (the genuine zero-width match at the real end, 2) -- without \
                 re-arming the terminal check after descending past the stale size, this is a \
                 false {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_pends_across_calls_before_re_arming() {
        // batch 5 fix round (2026-07-26), review response, P2-3: the SAME fixture as the test
        // just above, but with a small `chunk` so the descent from the stale size (100) down to
        // the real end (2) cannot finish inside one `step` call -- it must pend (`More`) at least
        // once, resume on a LATER call, and still land on the genuine zero-width match. Neither
        // sibling test above discriminates the re-arming at all: both use a `chunk` far bigger
        // than the whole descent's own cost (~98), so the entire retry loop runs inside a single
        // call, and the loop's own `continue` masks whether re-entry actually survives a real
        // `More`/resume boundary.
        //
        // restructure R6: the mechanism this test discriminates changed shape, not meaning.
        // Pre-R6, this was mutation-verified by deleting a `self.terminal_pending ||` disjunct
        // from a boolean gate; that gate (and the field) are gone -- `BwdPhase::Certify` being
        // the discriminant itself, across as many resumed calls as it takes, is what re-enters
        // this check now (`BwdPhase`'s own doc comment). The equivalent mutation today would be
        // making the terminal-check block conditional on something OTHER than `matches!(self.
        // phase, BwdPhase::Certify)` -- there is no such condition left to delete by accident,
        // which is the whole point of the redesign, not a gap in this test's own coverage.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 100, Bound::Inclusive(0), 8);
        let mut saw_more = false;
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 100,
                "livelock: {steps} consecutive steps, no terminal result"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => {
                    saw_more = true;
                    continue;
                }
                terminal => break terminal,
            }
        };
        assert!(
            saw_more,
            "the small chunk must force the descent to pend at least once -- otherwise this \
             fixture does not actually exercise a resumed call, and the Certify phase's own \
             re-entry stays untested"
        );
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 2,
                "the genuine zero-width $ at the real end must still be found across the resume"
            ),
            other => panic!(
                "expected Found across a resumed call -- a false {other:?} here is exactly the \
                 defect a missing BwdPhase::Certify re-entry produces"
            ),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_skips_whole_empty_blocks_not_one_byte_at_a_time()
     {
        // batch 5 fix round (2026-07-26), review response, P3-4: a source whose claimed `size()`
        // overstates its real length by many WHOLE blocks (not just a handful of bytes, unlike the
        // siblings above) -- the terminal check's ctx read discovers each of those blocks is wholly
        // empty, and the fix jumps `self.hi` to that block's own start in one retry instead of
        // creeping down by CTX_BEHIND bytes at a time. `chunk` is pinned to CTX_BEHIND itself, so
        // EVERY retry -- byte-exact or block-sized -- immediately meets it and ends its own `step`
        // call: the total `step`-call count becomes a direct, non-timing proxy for retry
        // granularity. One call per BLOCK through the empty span (this fix) versus one call per
        // CTX_BEHIND-byte creep through it (the reverted code) differ by more than an order of
        // magnitude for a 300-block stale span, so a loose bound cleanly separates them without
        // pinning an exact count. Mutation-verified: reverting to the unconditional `new_hi = p`
        // descent pushes this fixture into the low thousands of calls, well past the bound below.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let bs: u64 = 64;
        let stale_blocks: u64 = 300;
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: bs * stale_blocks,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(
            p,
            bs * stale_blocks,
            Bound::Inclusive(0),
            crate::search::CTX_BEHIND.get(),
        );
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 500,
                "{steps} step calls to cross a {stale_blocks}-block stale span -- a block-sized \
                 jump keeps this in the low hundreds (roughly one call per block), not the \
                 thousands a CTX_BEHIND-byte creep through the same span would need"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 2,
                "the genuine zero-width $ at the real end must still be found past the whole \
                 stale span"
            ),
            other => panic!("expected Found at the real end (2), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_certified_end_does_not_survive_carry_truncation() {
        // batch 6 (2026-07-28), finding #1: `certified` used to be `BwdPhase::Scan`'s own
        // position-LESS bool -- "this scan's memory of having passed through Certify" -- and it
        // survived descent and carry truncation. Once the carry's own retention cap
        // (MAX_MATCH_LEN + CTX_AHEAD - 1 = 4099) truncates, `hi.at() + carry.len()` falls below
        // the position Certify actually verified, but the bare bool kept forcing `Edge::True`
        // onto whatever ARTIFICIAL hay top the descent had reached by then.
        //
        // `block_size` 8192, real data `"a" * 16384 ++ "bz"` (16386 bytes, `cache.size()`
        // accurate -- no staleness needed, this defect does not require one, unlike the sibling
        // fixtures above): Certify resolves immediately at 16386 (its own ctx read is a real,
        // complete one). Scan iteration 1 (block 2, the topmost, 2-byte-short `"bz"` block):
        // enters with `hi = 16386`, `carry.len() = 0` -- sum 16386 == `cache.size()`, correctly
        // `Edge::True`, no match (`bz` has no `a+`). Iteration 2 (block 1, all 8192 `a` bytes):
        // enters with `hi = 16384`, `carry.len() = 2` (the carried `"bz"`) -- sum 16386, STILL
        // correctly `Edge::True` (nothing truncated yet: this iteration's own `next_carry`,
        // 8192 + 2 = 8194 bytes, is what triggers truncation, not what it entered with). No
        // match (a pure `a`-run has no boundary within itself). Iteration 3 (block 0): enters
        // with `hi = 8192`, `carry.len() = 4099` (iteration 2's `next_carry` truncated to the
        // low 4099 bytes) -- sum 12291, NOT `cache.size()` (16386): an ordinary, uncertified
        // position nowhere near the real end. The old bare bool still said `certified`, forcing
        // `Edge::True` there anyway. `a+\b` is never zero-width (`BwdPhase::Certify`'s own
        // zero-width-at-high check cannot short-circuit it -- it must reach `Scan`), so it
        // greedily matches a single trailing `a` right at the top of iteration 3's own accept
        // range against that FABRICATED boundary, reporting `Found { match_at: 8191 }` -- RED,
        // reproduced verbatim below at 466e851.
        //
        // Ground truth: 'a' is a word character and is immediately followed by 'b' (also a word
        // character -- no boundary) at the only place the real data's `a`-run actually ends; the
        // sole genuine trailing boundary (before EOF, 'z' then nothing) is not preceded by an
        // `a`. A whole-file linear oracle finds NO `a+\b` match anywhere in this file.
        //
        // Fixed: `certified` threads the EXACT position Certify verified
        // (`Option<wrap_certified_top::WrapCertifiedTop>`, this module's own witness -- a
        // separate authority from `crate::meter::CertifiedEnd`, see that type's own doc comment
        // for why), and the edge check compares the CURRENT hay top against THAT position, never
        // a stale, position-less "yes at some point" memory -- iteration 3's own sum (12291)
        // correctly fails the comparison against the witness (16386), giving `Edge::Cut` there
        // instead, and the search correctly runs out of real territory with no match at all.
        //
        // This is the NEGATIVE half of the fix's own arithmetic proof (`step`'s own hay
        // construction has the full derivation): once the sum drops below the certified end it
        // can never return. The sibling test just below pins the POSITIVE half -- the sum
        // staying pinned at the certified end across more than one iteration, correctly
        // re-earning `Edge::True` each time, not just on the first check.
        let mut real = vec![b'a'; 16384];
        real.extend_from_slice(b"bz");
        let size = real.len() as u64;
        let c = crate::cache::BlockCache::new(
            std::sync::Arc::new(MockSource::new(real)),
            8192,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile(r"a+\b", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, size, Bound::Inclusive(0), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 50,
                "livelock: {steps} consecutive steps, no terminal result"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "a+\\b has no match anywhere in this file (every real a-run edge is followed by \
                 another word character, 'b'; the only genuine trailing boundary, before 'z', is \
                 not preceded by an 'a') -- expected End, got {other:?} (a `Found` here, \
                 especially at 8191, is exactly finding #1's fabrication: `certified` outliving \
                 the position it was earned at)"
            ),
        }
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_certified_end_still_earns_true_on_a_later_iteration() {
        // batch 6 (2026-07-28), K1 fix round, review response, P2-1: the sibling test just above
        // pins the NEGATIVE half of the arithmetic proof (the certified sum can never be re-earned
        // once truncation drops it below `end.at()`) but nothing pinned the POSITIVE half --
        // `Edge::True` staying earned on a SECOND (or later) `Scan` iteration, not just the one
        // `Certify` itself resolved on, for as long as `self.carry` has not yet truncated.
        // Mutation-verified during review: dropping the carry term from the comparison entirely
        // (`Some(end) => self.hi.at() == end.at()`, checking only `self.hi` against the witness)
        // passes the ENTIRE pre-existing suite, including the sibling test above -- a strictly
        // weaker implementation that grants `Edge::True` only on iteration 1 and never re-earns
        // it was, until this test, indistinguishable from the landed one.
        //
        // Reviewer's own fixture, credited and adopted verbatim: file `b"zzzzabcd"` (8 bytes),
        // block_size 4, backward wrap leg from `hi = 8` (== `cache.size()`, accurate -- no
        // staleness), pattern `zabcd$`. Block 0 = `"zzzz"` (positions 0-3), block 1 = `"abcd"`
        // (positions 4-7). `Certify` resolves immediately at 8 (block 1 reads back whole, no
        // descent) -- `end.at() == 8`.
        //
        // Scan iteration 1: `hi = 8`, `carry.len() = 0` -- sum 8 == `end.at()`, `Edge::True`.
        // `idx = 1`, `lo = 4`, `slice = "abcd"` (block 1). The one place `zabcd$` can start is
        // position 3 (`z` at 3, `abcd` at 4-7, `$` at 8) -- but position 3 sits in `ctx` (the
        // look-behind portion below `lo`), never an acceptable match start (`search_hay`'s own
        // three-part doc comment, above: "ctx ... never an acceptable start"), so iteration 1
        // finds nothing despite `Edge::True` already holding. After: `self.hi` lowers to 4,
        // `next_carry = "abcd" ++ [] = "abcd"` (4 bytes, nowhere near the cap -- no truncation).
        //
        // Scan iteration 2: `hi = 4`, `carry.len() = 4` (the carried `"abcd"`) -- sum 8, STILL ==
        // `end.at()` (the positive half this test exists to pin: nothing truncated between
        // iterations 1 and 2, so the sum stayed exactly pinned, not merely non-increasing).
        // `idx = 0`, `lo = 0`, `slice = "zzzz"` (block 0), accept range now `[0, 4)` -- position 3
        // (the match's own start) is finally IN the accept range. Under the landed fix,
        // `Edge::True` still holds here (sum 8 == `end.at()` 8), so `zabcd$` matches "zabcd"
        // starting at 3 and ending at the certified top -- `Found { match_at: 3 }`. Under the M8
        // mutant (`self.hi.at() == end.at()`, dropping `+ carry.len()`), iteration 2's check
        // becomes `4 == 8` -- false, `Edge::Cut` -- and the match is silently lost: `Done(End)`.
        let c = crate::cache::BlockCache::new(
            std::sync::Arc::new(MockSource::new(bytes::Bytes::from_static(b"zzzzabcd"))),
            4,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("zabcd$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 8, Bound::Inclusive(0), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 50,
                "livelock: {steps} consecutive steps, no terminal result"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 3,
                "zabcd$ must match \"zabcd\" starting at 3 -- the certified top (8) is still \
                 correctly trusted on Scan iteration 2, not just iteration 1"
            ),
            other => panic!(
                "expected Found at 3 -- a `Done(End)` here is exactly the M8 mutant's own \
                 failure mode: dropping self.carry.len() from the comparison loses Edge::True on \
                 every iteration after the first, silently losing this match (got {other:?})"
            ),
        }
    }
    #[test]
    fn terminal_check_descent_target_does_not_repeat_hi_when_the_block_jump_would_land_at_floor() {
        // batch 5 fix round 2 (2026-07-27), review response, P1-NEW: pins the exact fixed-point
        // trigger the re-verdict's own evidence #1 used ("pure arithmetic, no runtime, loop not
        // executed") -- deliberately synchronous, no `step`/cache/async involved at all, since
        // actually running the buggy formula to this fixed point inside `step`'s own retry loop
        // hangs (the review's own evidence #3: the test binary never returned and had to be
        // killed, and was deliberately not kept as a committed test). `bs = 64`, `floor = 100`,
        // `self_hi = 100` is exactly the reviewer's own arithmetic repro: `ctx_start = 96`,
        // `block_start = 96 / 64 * 64 = 64`. The OLD, unconditional `.max(floor)` formula gives
        // `64.max(100) == 100 == self_hi` -- a fixed point (confirmed directly below against the
        // literal old expression, not merely against this test's own expectation, so a revert of
        // the real fix is what this test actually exercises). Mutation-verified: temporarily
        // changing `terminal_check_descent_target`'s own gate from `block_start > floor` to
        // unconditional (matching the old formula) makes this test's own `assert_ne!` fail with
        // `new_hi == self_hi == 100` -- confirmed, then restored.
        let bs: u64 = 64;
        let floor: u64 = 100;
        let self_hi: u64 = 100;
        let ctx_start = self_hi - crate::search::CTX_BEHIND.get() as u64;
        assert_eq!(
            ctx_start, 96,
            "sanity: CTX_BEHIND must still be 4 for this fixture's own numbers"
        );
        let old_unconditional_formula = (ctx_start / bs * bs).max(floor);
        assert_eq!(
            old_unconditional_formula, self_hi,
            "sanity: confirms these inputs really do trigger the OLD formula's own fixed point -- \
             if this assertion ever fails, the fixture no longer reproduces the bug and needs \
             updating"
        );
        let p = ctx_start; // wholly empty: p never advances past ctx_start
        let new_hi = terminal_check_descent_target(true, ctx_start, bs, floor, p);
        assert_ne!(
            new_hi, self_hi,
            "must not repeat self.hi -- the exact fixed point that hangs step's own retry loop \
             forever (spent stops growing, so the spent >= chunk escape can never fire again)"
        );
        assert!(
            new_hi < self_hi,
            "must still make real progress, not merely differ from self_hi"
        );
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_descends_past_a_nonzero_floor_without_hanging()
    {
        // batch 5 fix round 2 (2026-07-27), review response, P1-NEW: the functional twin of the
        // pure-arithmetic test above -- exercises the REAL `step` machinery end to end through the
        // reviewer's own repro shape (claimed size 200, real `b"ab"`, `bs: 64`, `floor: 100`,
        // i.e. `SearchBackward::new`'s own `limit` nonzero), which every terminal-check test
        // before this fix round used `floor: 0` and so never exercised (the coverage gap the
        // review named as "precisely why my round-1 probe did not catch it"). Safe to run to
        // actual completion, unlike a literal revert of the fix against this same fixture (which
        // hangs, per the review's own evidence and the sibling test above): this fix's own
        // `terminal_check_descent_target` strictly decreases `self.hi` on EVERY iteration
        // (its own doc comment has the proof), so this fixture cannot spin regardless of how many
        // stale blocks it crosses -- a small, generous step ceiling (not a timing bound) is enough
        // to prove that, not merely hope it. `chunk: 8` forces several `More`s, matching this
        // module's own established small-chunk idiom for exercising a multi-call resume.
        //
        // Expected outcome: the real 2 bytes sit ENTIRELY below `floor` (100), so once the
        // descent (correctly) passes below `floor`, `SearchBackward`'s own `[floor, hi)` window
        // contains no real data at all -- `End`, not a match at the real end, is the honest
        // answer (mirrors the "control" case the review's own evidence #4 already established:
        // real data strictly below `floor` is out of this leg's own range by construction, the
        // exact reason a plain leg 1/leg 2 composition, not this one leg alone, is what finds it).
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 200,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 200, Bound::Inclusive(100), 8);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 50,
                "{steps} step calls -- the fix's own strictly-decreasing descent resolves this \
                 fixture in well under a dozen calls; a bound this generous only exists to catch \
                 an actual regression to non-termination, not to pin an exact count"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        assert!(
            matches!(outcome, SearchStep::Done(SearchEnd::End)),
            "expected End -- the real data sits entirely below floor (100), so nothing in this \
             leg's own [floor, hi) range exists to find; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_control_when_real_data_shares_floors_own_block()
     {
        // batch 5 fix round 2 (2026-07-27), review response, P1-NEW's own control (reviewer-
        // supplied shape, credited): the SAME stale-size fixture as the sibling test above, but
        // `floor` (50) now lands INSIDE block 0 -- the SAME block the 2 real bytes live in --
        // rather than a whole empty block below it. This is precisely why the whole suite missed
        // the regression in the first place: `wholly_empty_first_block` is only ever true while
        // the descent is still above block 0 (the one wholly-empty block it crosses here, at
        // `hi = 100 -> 64`, lands on `64 > floor (50)` either way -- a no-op for the OLD formula's
        // own `.max(floor)`, since 64 already exceeds 50). Once the descent reaches block 0 itself
        // (nonempty: `block.is_empty()` is false there), `wholly_empty_first_block` is false for
        // every remaining iteration, so the byte-exact `p` fallback applies regardless of which
        // formula -- buggy or fixed -- is active; the two are IDENTICAL on this entire fixture.
        // The regression needs real data ending a WHOLE BLOCK below `floor`, never merely below
        // it: this fixture pins that the fix changes nothing when that precondition doesn't hold,
        // and resolves correctly (it always did).
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 100, Bound::Inclusive(50), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(steps <= 50, "livelock: {steps} steps, no terminal result");
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        assert!(
            matches!(outcome, SearchStep::Done(SearchEnd::End)),
            "expected End -- the real data (positions 0-1) sits below floor (50), nothing in \
             this leg's own [floor, hi) range exists to find; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn search_backward_wrap_leg_terminal_check_does_not_jump_past_a_partially_short_blocks_real_content()
     {
        // batch 5 fix round 3 (2026-07-27), review response, P2-NEW: round 2's `block_start >
        // floor` gate is a DIFFERENT safeguard from `wholly_empty_first_block`'s own `&&
        // block.is_empty()` half, and neither subsumes the other -- this fixture is what actually
        // pins the `is_empty` half, since every OTHER terminal-check fixture in this suite keeps
        // its real data inside block 0 with `floor: 0`, where `block_start == 0` makes the `>
        // floor` gate reject the jump regardless of `is_empty` (masking the guard's own necessity
        // entirely -- confirmed: neutering `&& block.is_empty()` alone passes the WHOLE suite at
        // this unit's own round-2 shas, `c5787cd`'s and `3a1eccc`'s own siblings included, even
        // though it re-opens finding #1's fabrication in general).
        //
        // Real data 100 bytes: entirely real inside block 0 (0..64), and PARTIALLY real inside
        // block 1 (64..100 real, 100..128 past the real end) -- block 1 is NOT wholly empty
        // (`block.is_empty()` is false there, `block.len() == 36`), so the correct trigger must
        // recognize it is merely SHORT of the needed offset, not wholly empty, and use the
        // byte-exact `p` fallback -- landing, eventually, on the genuine real end at 100. The
        // NEUTERED trigger (`p == ctx_start` alone, ignoring whether the block is truly empty)
        // instead treats block 1's own short read the same as a wholly-empty one: `block_start =
        // 64`, which DOES exceed `floor` (0) here (unlike the floor-0-block-0 fixtures above),
        // so the gate does NOT save us this time -- the jump fires, skipping block 1's own 36 real
        // bytes (positions 64..100) entirely, landing on 64, and fabricating a `$` there: `Found {
        // match_at: 64 }`, inside real content, when the genuine end is 100.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: vec![b'a'; 100],
            claimed_size: 6400,
        };
        let c = crate::cache::BlockCache::new(std::sync::Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 6400, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 100,
                "the genuine zero-width $ at the real end (100) must be found -- a fabricated \
                 match anywhere inside [0, 100) (64, in particular) would mean the is_empty half \
                 of the trigger stopped mattering"
            ),
            other => panic!("expected Found at the real end (100), got {other:?}"),
        }
    }
    /// batch 19 (2026-07-31). **An endpoint away from BOF gets the look-behind it needs.**
    ///
    /// `Hay::zero_width_at_high` gates look-behind adequacy unconditionally, so the bare assemblies
    /// batch 17's out-of-loop probes handed it could only ever answer at offset 0 -- which is why
    /// they fixed the empty file and nothing else. Over an accurate `b"a"` under a leg floored at 1,
    /// `$` at the real end sits one byte above real content and was refused for want of it.
    #[tokio::test]
    async fn search_backward_terminal_probe_reads_its_own_lookbehind() {
        let c = cache(b"a", 8);
        let p = Arc::new(SearchPattern::compile("$", true).unwrap());
        let mut s = SearchBackward::new_leg(p, 2, Bound::Inclusive(1), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(
                match_at, 1,
                "`$` matches at the real end (1), which is inside `[1, 2)` and below the origin"
            ),
            other => panic!(
                "the byte below the endpoint is readable and is what makes the probe admissible; \
                 got {other:?}"
            ),
        }
    }
    /// batch 18 (2026-07-31). **The terminal endpoint probe stays inside the leg's own window.**
    ///
    /// `clamp_to` degrades an out-of-range `hi` to the source's real size and knows nothing about
    /// `floor`, so an empty source under a bounded leg lands with `hi` STRICTLY BELOW the floor --
    /// and batch 17's probe fired there, reporting offset 0 to a caller that asked about `[50, 100)`.
    /// A bounded leg must answer only about its own window; below the floor there is nothing to say.
    #[tokio::test]
    async fn search_backward_terminal_probe_does_not_escape_the_floor() {
        let c = cache(b"", 8);
        for pattern in ["$", "^", "^$", r"\B"] {
            let p = Arc::new(SearchPattern::compile(pattern, true).unwrap());
            let mut s = SearchBackward::new_leg(p, 100, Bound::Inclusive(50), 1 << 10);
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::End) => {}
                other => panic!(
                    "`{pattern}` has no match in [50, 100) over an empty source -- offset 0 is \
                     outside the window this leg was asked about. got {other:?}"
                ),
            }
        }
    }
    #[tokio::test]
    async fn search_backward_finds_caret_dollar_on_an_empty_file() {
        // `wrap_leg: true`, exercising the terminal check's own mechanism directly, in isolation
        // -- this raw unit test does NOT represent leg 1's own exclusive view of an empty file
        // (batch 4 (2026-07-24), finding #1's own fix round, F7, reviewer-caught): `origin ==
        // size == 0` makes leg 1 and leg 2's own `hi` numerically identical here, but `wrap_leg`
        // still distinguishes them structurally -- a plain `wrap_leg: false` leg 1 on this SAME
        // fixture returns `End` (the terminal check never runs, and `floor == hi == 0` means the
        // main loop never runs either), so `Document::search_next`'s own two-leg composition
        // genuinely needs the wrap to find this match on an empty file, reporting `wrapped: true`
        // even though there is nowhere to "wrap" on a zero-byte file -- a deliberate consequence
        // of applying the self-hit-exclusion model consistently, not a special case for size 0.
        // See `search_next_backward_finds_caret_dollar_on_an_empty_file_via_a_deliberate_wrap`
        // (document.rs) for that end-to-end shape.
        let c = cache(b"", 8);
        let p = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 0, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 0),
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
        let mut s = SearchBackward::new_leg(p, 3, Bound::Inclusive(0), 1 << 10); // hi=3, but the real file is longer
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
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
        let mut s = SearchBackward::new_leg(p, 40, Bound::Inclusive(0), 8);
        let mut found = 0usize;
        let mut steps = 0usize;
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    found += 1;
                    assert_eq!(match_at, 13);
                    break;
                }
                SearchStep::Done(SearchEnd::End) => panic!("expected to find the planted needle"),
                SearchStep::More(..) => steps += 1,
            }
        }
        assert_eq!(found, 1, "reported exactly once");
        assert!(steps >= 1, "must have taken multiple chunks");
    }
    #[tokio::test]
    async fn search_backward_does_not_fabricate_an_over_cap_word_boundary() {
        // restructure R5 (2026-07-28): the per-mechanism neuter battery's own "an unearned True
        // forced in -- which fabrication test catches it, per consumer" check (A6, `SearchBackward`)
        // found NO existing test discriminates this -- forcing this site's own `high` edge to
        // `Edge::True` unconditionally left the WHOLE suite green. Closes that gap; the backward
        // sibling of `search_forward_does_not_fabricate_an_over_cap_word_boundary` and `sweep_
        // analysis_does_not_fabricate_an_over_cap_word_boundary_unicode` (search.rs).
        //
        // A FIRST construction attempt (`hi` placed exactly at a small "needle"'s own end, nothing
        // above it yet read) turned out NOT to discriminate: `SearchBackward`'s own straddle seed
        // (`step`'s own doc comment) unconditionally reads up to `MAX_MATCH_LEN + CTX_AHEAD - 1`
        // real bytes past `hi` BEFORE the main loop's first iteration, so for a small file the seed
        // itself already reaches the true end and hands the regex the REAL disqualifying byte --
        // correct regardless of what `high` claims, hence no discrimination. The fixture needs to
        // OUTLAST the seed's own reach for the artificial-edge hazard to be reachable at all.
        //
        // 100 'x's, a run of 20,000 'a's, a real "é" (a Unicode WORD character), 200 'z's -- the
        // same P2-4 fixture as its forward/sweep siblings. `hi = 10_000` (inside the a-run): the
        // seed reads up to 4_099 real bytes past it (to 14_099), still well inside the a-run, NOT
        // reaching the real é boundary at 20_100. `search_hay`'s own top (14_099) is therefore an
        // ARTIFICIAL cut -- `a+(?u:\b)`, searched leftmost-first within it, extends through `slice`
        // and INTO `carry` (both all 'a's, one contiguous run) all the way to that cut, and `\b`
        // evaluated there fires against "nothing past the slice" exactly as it would a real
        // boundary. `self.hi(10_000) + self.carry.len()(4_099) = 14_099 != cache.size()(20_302)`:
        // `high` must be `Cut`, not `True`.
        let mut data = vec![b'x'; 100];
        data.extend(vec![b'a'; 20_000]);
        data.extend("é".as_bytes());
        data.extend(vec![b'z'; 200]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let pattern = SearchPattern::compile(r"a+(?u:\b)", false).unwrap();
        assert_eq!(
            pattern.find_all(data),
            Vec::<(usize, usize)>::new(),
            "oracle: é is a word character, so no real (?u:\\b) exists after the a-run at any \
             prefix length"
        );
        let c = cache(data, 4096);
        let p = Arc::new(SearchPattern::compile(r"a+(?u:\b)", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 10_000, Bound::Inclusive(0), 1 << 20);
        let outcome = loop {
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                other => break other,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::End) => {}
            other => panic!(
                "expected End (no real boundary exists anywhere in this file) -- a Found here is \
                 the over-cap fabrication reading this leg's own artificial carry-top edge as a \
                 real boundary; got {other:?}"
            ),
        }
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
        let mut s = SearchBackward::new_leg(p, data.len() as u64, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 100),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            s.progressed(),
            6,
            "only the matched bytes were needed, not the rest of the block"
        );
    }
    #[tokio::test]
    async fn search_backward_straddle_seed_is_budgeted_and_resumable() {
        // batch 4 (2026-07-24), finding #3: RED evidence for the claim -- the seed used to read
        // up to MAX_MATCH_LEN bytes through `warm()` in one synchronous burst, BEFORE the
        // budgeted main loop even started and uncharged to `spent`; with a 64-byte block/budget
        // here that shape would perform MAX_MATCH_LEN / 64 == 64 physical reads in this ONE
        // `step()` call (block_size 1 -- the brief's own fixture -- makes the same point at
        // ~4,097 reads, but is impractical to assert against directly; 64 is the brief's own
        // suggested scale-up). Post-fix, a single `step()` call spends at most `chunk` (64)
        // bytes of work TOTAL -- exactly one 64-byte block here, since `hi` is block-aligned --
        // and hands back `More` with the seed's own progress persisted in `BwdPhase::Seed`'s own
        // cursor (restructure R6; formerly `self.seed_pos`), not a burst proportional to
        // `MAX_MATCH_LEN`.
        let data = vec![b'x'; 10_000];
        let src = Arc::new(MockSource::new(data));
        let c = crate::cache::BlockCache::new(src.clone(), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 4096, Bound::Inclusive(0), 64);
        match s.step(&c).await.unwrap() {
            SearchStep::More(..) => {}
            other => panic!(
                "expected More -- the seed alone (4096 bytes) exceeds the 64-byte budget, got \
                 {other:?}"
            ),
        }
        assert_eq!(
            src.read_count(),
            1,
            "one step() call must read at most one block's worth (block-aligned hi, chunk == \
             block_size), not the whole MAX_MATCH_LEN seed at once: {} physical reads",
            src.read_count()
        );
    }
    #[tokio::test]
    async fn search_backward_straddle_seed_bytes_do_not_count_toward_scanned() {
        // batch 4 (2026-07-24), finding #3: the straddle seed is lookahead (this struct's own
        // `carry` doc comment -- "lookahead only, never an acceptable start of their own"), the
        // same role `SearchForward`'s own `ctx` look-behind byte plays there, and like `ctx` it
        // must not inflate `scanned()` -- a caller uses `scanned()` to seed the interactive
        // budget's own remaining allowance (`Document::resolve_found`) and to report progress;
        // counting the seed's own MAX_MATCH_LEN bytes of pure lookahead the match itself never
        // needed would misreport both. Same fixture as `search_backward_scanned_counts_only_the_
        // bytes_needed_for_the_match` just above, just with room AFTER `hi` for a full-length
        // seed to actually run (there, `hi == size`, so the seed reads nothing at all -- this
        // module's own "the seed reads nothing when hi == size" property, unexercised by that
        // test).
        let mut data = vec![b'x'; 100];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'x'; 5000]);
        let c = crate::cache::BlockCache::new(Arc::new(MockSource::new(data)), 1 << 10, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 106, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 100),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            s.progressed(),
            6,
            "only the matched bytes count, not the 4096-byte straddle seed read to get there"
        );
    }
    #[tokio::test]
    async fn search_backward_seed_reaches_a_terminal_step_when_the_source_over_reports_its_size() {
        // batch 4 (2026-07-24), finding #3's own fix round, F1 [P1] (reviewer-caught regression):
        // the seed's read loop has TWO exits -- budget exhaustion (`spent >= self.chunk`, which
        // resumes with real progress next call) and `take == 0` (a short/empty block; NOTHING more
        // will ever arrive, this module's own header doc comment on why block reads can be short).
        // The post-loop code could not tell them apart: both leave the seed cursor `< seed_end`,
        // so both returned `More` -- but retrying after a `take == 0` exit re-enters the identical
        // state and breaks at the identical place, forever. `BlockCache::block`'s own doc comment
        // already advertises "short at EOF, empty past EOF", and `PreadSource::admit`'s own
        // ticket (source.rs) truncates its own buffer to what it actually read while `size()`
        // keeps the value captured at open -- a file truncated out from under the pager produces
        // exactly this shape, the same "since-truncated file" case batch 3's own finding #6 and this
        // struct's own finding #1 comment both already name as a must-degrade-gracefully scenario.
        //
        // This source claims size 20,000 but only has 200 real bytes; `hi = 128` sits WELL inside
        // the real data (so only the seed's own `[hi, hi + MAX_MATCH_LEN)` lookahead runs off the
        // end, not the main loop's own scan). Bounded, not timing-based: `step` is driven a
        // generous but FINITE number of times -- the correct scan needs only a handful (one seed
        // read, a skipped terminal check, a couple of main-loop iterations down to "needle" at 10)
        // -- reaching the bound without ever seeing a terminal step IS the livelock, deterministically,
        // on any host, never a flaky wall-clock race.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let mut real = vec![b'x'; 200];
        real[10..16].copy_from_slice(b"needle");
        let source = OverstatesItsOwnSize {
            real,
            claimed_size: 20_000,
        };
        let c = crate::cache::BlockCache::new(Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 128, Bound::Inclusive(0), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 50,
                "livelock: {steps} consecutive More steps, no progress, no terminal step"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 10),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_main_loop_finds_a_match_below_a_truncation_gap_instead_of_spinning() {
        // batch 4 (2026-07-24), finding #14-internal, RE-FIXTURED in the 2026-07-25 fix round
        // (F1 [P1] -- the review's own M4 lesson: the original fixture planted no match in the
        // real 200 bytes, so "reaches *a* terminal" was pinned, never "reaches the *right*
        // one" -- mutating `return Ok(SearchStep::End)` to a budgeted descent survived the
        // whole suite unnoticed). `hi = 5000` sits PAST the real 200 bytes -- the truncation
        // policy (docs/budgeted_scanning.md): a BACKWARD-direction loop descends past the
        // fictional territory `size()` claims exists rather than terminating the moment it
        // meets an empty block, since everything below it down to `floor` is still real,
        // readable, unsearched territory (this leg does NOT "have nothing left it could ever
        // find below here" -- the ORIGINAL fix's own false claim). Ground truth: `Found{100}`.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let mut real = vec![b'x'; 200];
        real[100..106].copy_from_slice(b"needle");
        let source = OverstatesItsOwnSize {
            real,
            claimed_size: 20_000,
        };
        let c = crate::cache::BlockCache::new(Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 5000, Bound::Inclusive(0), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 100,
                "livelock: {steps} consecutive More steps, no progress, no terminal step"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 100),
            other => panic!("the real, readable needle at 100 must be found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_forward_main_loop_finds_a_match_below_a_truncation_gap_instead_of_terminating()
    {
        // batch 4 (2026-07-24), finding #14-internal, RE-FIXTURED in the 2026-07-25 fix round
        // (F1 [P1] -- same M4 lesson as backward's own sibling test, just above): the original
        // fixture planted no match in the real 200 bytes, so asserting `End` could not tell a
        // correct implementation from one that silently drops every match in the final
        // `MAX_MATCH_LEN`-ish window before a truncation point. `SearchForward`'s own `safe_to`
        // never accepts a candidate while waiting for lookahead (`at_final`/`at_eof`, both
        // compared against the CLAIMED `size`) -- on a source that never reaches that claim,
        // nothing is ever accepted, and the OLD `take == 0` site returned `End` immediately,
        // before ever running the final accept pass this fix round adds. Ground truth: `Found{100}`.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let mut real = vec![b'x'; 200];
        real[100..106].copy_from_slice(b"needle");
        let source = OverstatesItsOwnSize {
            real,
            claimed_size: 20_000,
        };
        let c = crate::cache::BlockCache::new(Arc::new(source), 64, 1 << 20);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 1 << 20);
        let mut steps = 0usize;
        let outcome = loop {
            steps += 1;
            assert!(
                steps <= 100,
                "livelock: {steps} consecutive More steps, no progress, no terminal step"
            );
            match s.step(&c).await.unwrap() {
                SearchStep::More(..) => continue,
                terminal => break terminal,
            }
        };
        match outcome {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 100),
            other => panic!("the real, readable needle at 100 must be found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_limit_bounds_the_scan_below_an_earlier_match() {
        // needle starts at 4; a floor of 5 excludes it byte-exactly, even
        // though it shares an 8-byte block with in-bounds bytes.
        let c = cache(b"aaa needle bbb", 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 14, Bound::Inclusive(5), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
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
        let mut s = SearchBackward::new_leg(p, 2, Bound::Inclusive(0), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                assert_eq!(
                    match_at, 1,
                    "must find the closer straddling match, not fall back to 0"
                )
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_straddler_planted_one_byte_above_the_floor() {
        // the floor-edge twin: the straddling match's own start sits just ONE byte above `limit`
        // (the inclusive lower bound), stressing the seed against the OTHER boundary in the same
        // scan at once -- "needle" spans [8, 14), hi=10 (so leg 1 can only ever see "ne" without
        // the seed), limit=7 (byte 8 -- "needle"'s own start -- is in-bounds, and this iteration
        // never even reaches `lo == floor`, since `lo` lands block-aligned at 8 first). batch 4
        // (2026-07-24), finding #4's own decision point moved this off EXACTLY the floor (was:
        // `limit: 8`, `..._planted_at_the_floor_edge`) -- a match starting AT the floor itself is
        // now a documented miss regardless of pattern shape, see the sibling test just below;
        // this one keeps pinning that the SEED mechanism itself is unaffected by sitting close
        // to (not exactly on) the floor.
        let mut data = vec![b'z'; 8];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'z'; 4]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 10, Bound::Inclusive(7), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                assert_eq!(match_at, 8, "the near-floor straddler must still be found")
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_finds_a_match_starting_exactly_at_the_floor() {
        // batch 4 (2026-07-24), finding #4's own decision point, stated directly (this struct's
        // own DECISION paragraph): a match starting EXACTLY at `floor` is found, not excluded --
        // an earlier version of this fix uniformly excluded local start 0 whenever `lo == floor`
        // (mirroring `SweepAnalysis::give_up`'s own seam exclusion), but that broke a real,
        // load-bearing case (`search_next_backward_self_hit_wraps_to_find_it_again`, document.rs
        // -- the wrap leg's own floor is `origin`, exactly the cursor's own self-hit position).
        // The corrected model reads the real bytes below `floor` (bounded, up to CTX_BEHIND) for
        // context instead of excluding the position, so an ordinary literal match right at the
        // floor -- needing no look-behind at all -- is unaffected either way.
        let mut data = vec![b'z'; 8];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'z'; 4]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 10, Bound::Inclusive(8), 1 << 10);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 8),
            other => {
                panic!("a match starting exactly at the floor must still be found: got {other:?}")
            }
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
        let mut s = SearchForward::new(p, origin, Bound::Exclusive(u64::MAX), 32);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                panic!("falsely found ^needle at {match_at}: origin ({origin}) is mid-line")
            }
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::More(..) => panic!("fixture is small enough to resolve in one step"),
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
        let mut s = SearchForward::new(p, origin, Bound::Exclusive(u64::MAX), 32);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, origin),
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    panic!("falsely found ^needle at {match_at}: no real newline precedes it")
                }
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::More(..) => {}
            }
        }
    }
    #[tokio::test]
    async fn search_forward_unicode_word_boundary_needs_the_whole_character_across_a_carry_shrink()
    {
        // batch 4 (2026-07-24), finding #9: "é" (2 bytes, U+00E9, a Unicode WORD character)
        // plants its LAST byte right where a carry-shrink seam lands (a small block size forces
        // several shrink cycles before reaching it, the same shape as the sibling `^needle` test
        // above) -- with the OLD one-byte `ctx`, the regex would see only é's lone, INVALID
        // continuation byte there and treat it as non-word, falsely satisfying `(?u:\b)`. Ground
        // truth: é is a word char, same as the 'n' that follows -- no boundary, no match.
        let tail = crate::search::MAX_MATCH_LEN + 200;
        let plant = 5000usize;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + tail];
            v[plant - 2..plant].copy_from_slice("é".as_bytes());
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("(?u:\\b)needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    panic!("falsely found (?u:\\b)needle at {match_at}: é is a word char")
                }
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::More(..) => {}
            }
        }
    }
    #[tokio::test]
    async fn search_forward_unicode_word_boundary_still_matches_after_a_real_non_word_char() {
        // the positive twin, identical shape and seam alignment: a genuine non-word byte
        // (space) immediately before "needle" instead of é -- `(?u:\b)` must still hold, proving
        // the fix doesn't overcorrect into never matching across a shrink seam.
        let tail = crate::search::MAX_MATCH_LEN + 200;
        let plant = 5000usize;
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; plant + 6 + tail];
            v[plant - 1] = b' ';
            v[plant..plant + 6].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("(?u:\\b)needle", false).unwrap());
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, plant as u64);
                    return;
                }
                SearchStep::Done(SearchEnd::End) => {
                    panic!("expected to find (?u:\\b)needle after a real space")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchForward::new(p, 0, Bound::Exclusive(u64::MAX), 32);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, plant as u64);
                    return;
                }
                SearchStep::Done(SearchEnd::End) => {
                    panic!("expected to find the line-anchored needle")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchBackward::new_leg(p, 400, Bound::Inclusive(0), 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    panic!("falsely found ^needle at {match_at}: no real newline precedes it")
                }
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::More(..) => {}
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
        let mut s = SearchBackward::new_leg(p, 400, Bound::Inclusive(0), 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, 97);
                    return;
                }
                SearchStep::Done(SearchEnd::End) => {
                    panic!("expected to find the line-anchored needle")
                }
                SearchStep::More(..) => {}
            }
        }
    }
    #[tokio::test]
    async fn search_backward_unicode_word_boundary_needs_the_whole_character() {
        // batch 4 (2026-07-24), finding #9: "é" (2 bytes, U+00E9, a Unicode WORD character)
        // immediately precedes "needle" -- with the OLD one-byte per-iteration `ctx` fetch, the
        // regex would see only é's lone, INVALID continuation byte and treat it as non-word,
        // falsely satisfying `(?u:\b)`. Ground truth: é is a word char, same as 'n' -- no
        // boundary, no match. A tiny chunk/block size forces several distinct `lo` edges.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 400];
            v[95..97].copy_from_slice("é".as_bytes());
            v[97..103].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("(?u:\\b)needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 400, Bound::Inclusive(0), 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    panic!("falsely found (?u:\\b)needle at {match_at}: é is a word char")
                }
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::More(..) => {}
            }
        }
    }
    #[tokio::test]
    async fn search_backward_unicode_word_boundary_still_matches_after_a_real_non_word_char() {
        // the positive twin: a genuine non-word byte (space) immediately before "needle",
        // instead of é -- `(?u:\b)` must still hold.
        let data: &'static [u8] = Box::leak({
            let mut v = vec![b'x'; 400];
            v[96] = b' ';
            v[97..103].copy_from_slice(b"needle");
            v.into_boxed_slice()
        });
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("(?u:\\b)needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 400, Bound::Inclusive(0), 8);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::Found { match_at, .. }) => {
                    assert_eq!(match_at, 97);
                    return;
                }
                SearchStep::Done(SearchEnd::End) => {
                    panic!("expected to find (?u:\\b)needle after a real space")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchBackward::new_leg(p, 7, Bound::Inclusive(0), 8); // hi = 7, right at "needle"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
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
        let mut s = SearchBackward::new_leg(p, 7, Bound::Inclusive(0), 8); // hi = 7, right at "needle"'s own end
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 1),
            other => panic!("expected Found, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_unicode_word_boundary_needs_the_full_trailing_character_not_one_byte()
    {
        // batch 5 fix round (2026-07-26), review response, P3-6: renamed from
        // `..._dollar_needs_...`, the same correction as this test's forward twin -- the pattern
        // below is `(?u:\b)`, never `$`, and `$` is precisely the ONE trailing assertion that does
        // NOT need `CTX_AHEAD`. batch 5 (2026-07-26), finding #3's backward twin: the straddle seed's own margin
        // (`seed_end`) needs the FULL trailing character past a worst-case straddler's own end,
        // not merely 1 byte -- the same defect as `SearchForward`'s own twin, just on the seed
        // that makes a below-`hi` match's own trailing assertion findable at all. `hi = 1` forces
        // the WORST-CASE straddler (`s = hi - 1 = 0`, `L = MAX_MATCH_LEN`): its own trailing
        // assertion sits at `0 + MAX_MATCH_LEN`, exactly where `é` (2 bytes, a Unicode word
        // character) begins.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'a'; max];
        data.extend_from_slice("é".as_bytes()); // 0xC3, 0xA9 -- a Unicode word character
        data.extend_from_slice(b"zzzz"); // real bytes past é so full information is available
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 1);
        let p = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert!(
            p.find_all(data).is_empty(),
            "whole-file oracle: é is a word character, (?u:\\b) must not hold there"
        );
        let mut s = SearchBackward::new_leg(p, 1, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => panic!(
                "fabricated (?u:\\b) at {match_at} from a lone lead byte of é -- the whole-file \
                 oracle (find_all) correctly finds no match here"
            ),
            other => panic!("expected a decisive terminal result, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_ongoing_carry_retains_a_cap_length_candidates_own_assertion() {
        // batch 5 fix round (2026-07-26), review response, P3-1: the sibling test above pins the
        // STRADDLE SEED's own margin, seeded once before the main loop ever runs; this pins the
        // ONGOING per-iteration carry-shrink (scan.rs, the loop's own `keep`), exercised only once
        // the seed's own contribution has already aged through at least one shrink cycle (`hi`
        // sits at the true end here, so the straddle seed itself reads nothing at all: the whole
        // margin is built up by the main loop alone). `block_size: 1` is load-bearing, not
        // incidental: at any BIGGER block size, `keep`'s own "spare" capacity past a full a-run
        // (`keep - (a_run_len - slice.len()) == slice.len()` when `keep == a_run_len` exactly, the
        // OLD margin) already comfortably fits `é`'s 2 bytes whenever `slice.len() >= 2` -- a
        // first attempt at this fixture used `block_size: 8` and never discriminated anything for
        // exactly that reason, confirmed by tracing the actual carry contents at the final
        // iteration rather than assuming. Only `block_size: 1` (spare capacity exactly 1 byte
        // under the OLD margin, 4 bytes under the fixed one) forces the OLD margin's own spare
        // room to fall short of `é`'s own 2 bytes while the fixed one still covers it.
        let max = crate::search::MAX_MATCH_LEN;
        let mut data = vec![b'a'; max];
        data.extend_from_slice("é".as_bytes()); // 0xC3, 0xA9 -- right after the a-run's own end
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let c = cache(data, 1);
        let p = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert!(
            p.find_all(data).is_empty(),
            "whole-file oracle: é is a word character, (?u:\\b) must not hold there"
        );
        let mut s = SearchBackward::new_leg(p, data.len() as u64, Bound::Inclusive(0), 1 << 20);
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::End) => {}
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => panic!(
                "fabricated (?u:\\b) at {match_at} -- é survived only partially (or not at all) \
                 in the ongoing carry, from a too-narrow keep discarding its own oldest bytes"
            ),
            other => panic!("expected a decisive terminal result, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_reads_a_real_byte_below_its_own_floor_for_context() {
        // batch 4 (2026-07-24), finding #4's own decision point (this struct's own ANCHOR
        // CORRECTNESS doc comment has the full derivation): a real `\n` sits ONE byte below
        // `limit` -- reading it for `^` context is now DELIBERATE, not forbidden (an earlier
        // version of this test, `..._never_reads_below_its_own_floor_for_context`, pinned the
        // OPPOSITE claim under the old sentinel/exclusion model; disproven by a reviewer-caught
        // regression in the wrap-leg self-hit case, see this struct's own doc comment). `^needle`
        // now correctly matches, since the real predecessor genuinely IS `\n`.
        let data = b"z\nneedle";
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let mut s = SearchBackward::new_leg(p, 8, Bound::Inclusive(2), 8); // limit=2: "needle" (2..8) is in-bounds, the \n (1) is not
        match s.step(&c).await.unwrap() {
            SearchStep::Done(SearchEnd::Found { match_at, .. }) => assert_eq!(match_at, 2),
            other => panic!("expected Found (the real \\n below the floor), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_backward_still_misses_a_real_byte_beyond_ctx_behinds_own_reach_below_the_floor()
    {
        // the residual that DOES remain (this struct's own ANCHOR CORRECTNESS doc comment's own
        // "net residual" paragraph): the look-behind fetch is bounded at CTX_BEHIND (4) bytes,
        // even below `floor` -- a real `\n` sitting FIVE bytes below `limit` (one byte beyond
        // that reach) is still an honest miss, not a false positive.
        //
        // restructure R3: driven via `complete()`, not a single `step()`. This fixture used to
        // resolve in exactly one `step()` call at e9c7e9a; it no longer does, because finding
        // #5's OWN intended change now charges the per-iteration look-behind context read (it
        // was structurally uncharged before this restructure at all) -- this fixture's own tiny
        // chunk (8) is exhausted by that bookkeeping alone well before the payload work e9c7e9a
        // also did in one step. (An earlier version of this comment attributed the change to a
        // budget-accounting BUG this restructure's own first migration attempt introduced and
        // then fixed -- the `warmed`-block reuse path, `search_backward_still_misses...`'s own
        // sibling section, just below the payload loop's own `out_of_budget` guard -- but that
        // bug did NOT exist at e9c7e9a and is unrelated to why THIS test needs `complete()`; the
        // adversarial review's own P2-3 finding caught the conflation. Fix round, corrected: not
        // re-worded, re-derived -- the fix-round reviewer's own re-run of e9c7e9a's payload loop,
        // `spent += slice.len()` unconditionally including the reuse path, single-`step()`-
        // resolves this fixture too, at only 3 physical reads; the difference this restructure
        // actually makes is charging bookkeeping, not fixing a bypass that was never there.) This
        // test's claim was always the miss, not the step count, so it now asserts the same
        // outcome at whatever step count correct charging takes.
        let data = b"z\nwwwwneedle"; // \n at 1, "needle" at 6..12, limit will be 6
        let c = cache(data, 8);
        let p = Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let s = SearchBackward::new_leg(p, 12, Bound::Inclusive(6), 8); // limit=6: \n(1) is 5 bytes below it
        let (tx, _rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 12,
        });
        match s.complete(&c, tx, 12).await.unwrap() {
            SearchEnd::End => {}
            other => panic!(
                "the \\n sits beyond CTX_BEHIND's own reach below the floor; expected End, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    #[should_panic(expected = "Certify -> Scan ran more than once")]
    async fn search_backward_re_entrant_certify_would_livelock_the_guard_catches_it() {
        // R6 review P2-2, credited: the reviewer's own construction (keep minting
        // `certify_progress` every call while skipping the `self.phase = BwdPhase::Scan { .. }`
        // assignment) hung `cargo test -p ress-core --lib` for 240s on the pre-fix code --
        // `Progressed` real, `Charged` real, no bound on how many times either mints, because
        // nothing checked whether `Certify` had already resolved once before. Reproducing the
        // exact re-entrant CODE PATH here (rather than editing production `step` itself, which
        // this project never leaves broken in a committed test) would need a second, parallel
        // implementation of the function -- so this test exercises the GUARD directly instead:
        // pre-poison `certify_fallback_minted` (`SearchBackward`'s own one-shot pin, `step`'s own
        // doc comment on `certify_progress` has the full argument) to `true`, as if some earlier
        // call had already minted the fallback once, then drive a wrap leg's very first `step`
        // through `Certify`'s own completion path on a real, no-match fixture. The `assert!` --
        // NOT `debug_assert!`; this test itself is what caught that distinction mattering, by
        // failing under `just test-release` before the fix -- `step` runs immediately before
        // every `Certify -> Scan` transition fires here, deterministically, with no timing
        // dependency, proving the identical check that (verified separately, by hand, then
        // reverted: not left in this tree) makes the reviewer's own live re-entrant construction
        // fail seven tests in ~35s via this exact panic message, not hang for 240s.
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(b"xxxxxxxx"))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("zzz_never_matches_zzz", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 8, Bound::Inclusive(0), 1 << 20);
        s.certify_fallback_minted = true;
        let _ = s.step(&c).await;
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
        //
        // `wrap_leg: true` is LOAD-BEARING here, not incidental (batch 4 (2026-07-24), finding
        // #3's own fix round, F2 [P2], reviewer-caught): `hi == cache.size()` (64 over a 64-byte
        // file) only reaches the `self.warmed = Some(..)` sharing this test guards -- the exact
        // touch finding #7 needed fixed -- through the terminal check, which `wrap_leg: false`
        // skips entirely. "needle" can never match the terminal check's own one-byte probe
        // regardless (the test's own subject and assertion are unaffected either way), but with
        // `false` the terminal check's own block-reuse code never RUNS at all, silently retiring
        // this test's own coverage of it without changing a single assertion.
        let data: &'static [u8] = Box::leak(vec![b'x'; 64].into_boxed_slice());
        let c = crate::cache::BlockCache::new(
            Arc::new(MockSource::new(bytes::Bytes::from_static(data))),
            8,
            1 << 20,
        );
        let p = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut s = SearchBackward::new_wrap_leg(p, 64, Bound::Inclusive(0), 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::Done(SearchEnd::Found { .. }) => {
                    panic!("no match planted in this fixture")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchBackward::new_leg(p, 5, Bound::Inclusive(0), 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::Done(SearchEnd::Found { .. }) => {
                    panic!("no match planted in this fixture")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchBackward::new_leg(p, 13, Bound::Inclusive(0), 1 << 10);
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::Done(SearchEnd::Found { .. }) => {
                    panic!("no match planted in this fixture")
                }
                SearchStep::More(..) => {}
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
        let mut s = SearchForward::new(p, 4, Bound::Exclusive(u64::MAX), 1 << 10); // mid-block origin (block_size 8)
        loop {
            match s.step(&c).await.unwrap() {
                SearchStep::Done(SearchEnd::End) => break,
                SearchStep::Done(SearchEnd::Found { .. }) => {
                    panic!("no match planted in this fixture")
                }
                SearchStep::More(..) => {}
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
        let s = SearchBackward::new_leg(p, 40, Bound::Inclusive(0), 8);
        let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress {
            scanned: 0,
            span: 40,
        });
        match s.complete(&c, tx, 40).await.unwrap() {
            SearchEnd::Found { match_at, .. } => assert_eq!(match_at, 13),
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
                        FwdStep::More(..) => {}
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
                        BwdStep::More(..) => {}
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
                        CountStep::More(..) => {}
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
                let mut big = SearchForward::new(pattern.clone(), from, Bound::Exclusive(u64::MAX), usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = SearchForward::new(pattern, from, Bound::Exclusive(u64::MAX), chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        SearchStep::More(..) => {}
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
                let mut big = SearchBackward::new_leg(pattern.clone(), hi, Bound::Inclusive(0), usize::MAX);
                let oracle = big.step(&c).await.unwrap();
                let mut small = SearchBackward::new_leg(pattern, hi, Bound::Inclusive(0), chunk);
                let got = loop {
                    match small.step(&c).await.unwrap() {
                        SearchStep::More(..) => {}
                        terminal => break terminal,
                    }
                };
                prop_assert_eq!(&got, &oracle, "chunked search diverged from unbudgeted search");
                Ok::<(), TestCaseError>(())
            })?;
        }
    }
}
/// The truncation policy's own audit (fix round, 2026-07-25, F1's own inventory ask):
/// `ForwardScan`, `CountScan`, `BackwardScan`'s line-hunt, and `fill_lines` are all
/// "structurally identical by inspection" to the SAME `if slice.is_empty() { break }` shape
/// `SearchForward`/`SearchBackward`/`SweepAnalysis` had -- probed directly here, not assumed,
/// and all four turned out to be genuinely reachable (not merely theoretical): three livelock
/// (bounded-step-count proofs below), one (`fill_lines`, a single-call function, never
/// "stepped" externally) a silent wrong-answer instead (`stopped: false` at the real end,
/// which `Document::viewport` reads as "budget/row-limited", silently dropping the final
/// unterminated line -- fixed the same policy way regardless of the different failure shape).
#[cfg(test)]
mod truncation_policy_audit {
    use super::*;
    use std::sync::Arc;
    struct OverstatesItsOwnSize {
        real: Vec<u8>,
        claimed: u64,
    }
    #[async_trait::async_trait]
    impl crate::source::BlockSource for OverstatesItsOwnSize {
        fn size(&self) -> u64 {
            self.claimed
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            let real = self.real.clone();
            crate::source::ReadTicket::from_fn(move |offset, len| {
                Box::pin(async move {
                    if offset >= real.len() as u64 {
                        return Ok(bytes::Bytes::new());
                    }
                    let end = ((offset + len as u64) as usize).min(real.len());
                    Ok(bytes::Bytes::copy_from_slice(&real[offset as usize..end]))
                })
            })
        }
    }
    fn trunc_cache(real: Vec<u8>, claimed: u64, bs: usize) -> crate::cache::BlockCache {
        crate::cache::BlockCache::new(
            Arc::new(OverstatesItsOwnSize { real, claimed }),
            bs,
            1 << 20,
        )
    }
    #[tokio::test]
    async fn forward_scan_clamps_to_the_last_real_line_start_past_a_truncation_gap() {
        // `size()` claims 20_000 over 200 real bytes; only ONE real line start exists (101),
        // fewer than the 5 requested, so the scan must walk all the way to the real end before
        // resolving -- exactly where the OLD `if slice.is_empty() { break }` froze `self.pos`
        // and returned `More` forever (`self.pos < size` stayed true against the claim).
        let mut real = vec![b'x'; 200];
        real[100] = b'\n';
        let c = trunc_cache(real, 20_000, 64);
        let mut s = ForwardScan::new(0, 5, 1 << 20);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(
                steps <= 200,
                "livelock: {steps} consecutive More, no progress"
            );
            match s.step(&c).await.unwrap() {
                FwdStep::More(..) => continue,
                FwdStep::Done(FwdEnd::Eof(clamp)) => {
                    assert_eq!(clamp, 101, "must clamp to the one real line start it found");
                    return;
                }
                other => panic!("expected Eof, got {other:?}"),
            }
        }
    }
    #[tokio::test]
    async fn count_scan_counts_every_real_newline_past_a_truncation_gap() {
        let mut real = vec![b'x'; 200];
        real[50] = b'\n';
        real[150] = b'\n';
        let c = trunc_cache(real, 20_000, 64);
        let mut s = CountScan::new(0, 20_000, 1 << 20);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(
                steps <= 200,
                "livelock: {steps} consecutive More, no progress"
            );
            match s.step(&c).await.unwrap() {
                CountStep::More(..) => continue,
                CountStep::Done(n) => {
                    assert_eq!(
                        n, 2,
                        "must count both real newlines despite the claimed size"
                    );
                    return;
                }
            }
        }
    }
    #[tokio::test]
    async fn backward_scan_line_hunt_descends_a_truncation_gap_to_top() {
        // no `\n` anywhere in the real bytes, so the hunt must descend all the way from the
        // fictional `hi = 5000` through the empty gap down to 0 -- the OLD code froze `self.hi`
        // at the first empty block and returned `More` forever.
        let real = vec![b'x'; 200];
        let c = trunc_cache(real, 20_000, 64);
        let mut s = BackwardScan::new(5000, 1, 1 << 20);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(
                steps <= 200,
                "livelock: {steps} consecutive More, no progress"
            );
            match s.step(&c).await.unwrap() {
                BwdStep::More(..) => continue,
                BwdStep::Top => return,
                other => panic!("no \\n anywhere in the real bytes; expected Top, got {other:?}"),
            }
        }
    }
    #[tokio::test]
    async fn fill_lines_reports_the_certified_end_at_a_non_block_aligned_truncation() {
        // **This assertion was FLIPPED in batch 14 (2026-07-30), and the test renamed with it** --
        // the earlier version was named
        // `fill_lines_reports_budget_not_a_false_end_at_a_non_block_aligned_truncation_gap`
        // and required `Budget` here. Read the history in order, because the flip is not a return
        // to what R3's fix round rejected:
        //
        // - The PRE-fix-round design certified from a discarded probe at the NEXT BLOCK's own
        //   start, inferring "empty there" as "the data ends here". R3's own P1-1 finding killed
        //   that, and rightly: `b"helloXmo"` has block 0 answering 5 of its 8 bytes with block 1
        //   genuinely empty, and real bytes `X`,`m`,`o` sit at 5..8 regardless. The probe cannot
        //   see them, so the inference is a guess that happens to be right sometimes.
        // - This test then pinned the honest conservative miss, on the ground that `fill_lines`
        //   "cannot honestly distinguish" this fixture from that adversarial cousin. TRUE THEN,
        //   FALSE NOW, and the sentence names its own expiry condition: the two are
        //   indistinguishable only to a cache that refuses to ASK. Batch 12 made it ask -- at 200,
        //   the exact boundary in question, not at the next block's start -- and this source
        //   answers EMPTY there while `ShortFirstBlock`'s cousin answers with its real bytes. The
        //   observable shapes stopped being the same observable.
        //
        // So the certificate here is earned by an observed empty read at 200, which is the only
        // thing P1-1 ever asked for. The adversarial cousin still reports no certified end,
        // pinned separately by `viewport_short_block_then_empty_next_block_must_not_fabricate`
        // (document.rs) -- both directions live, which is what makes this a distinction rather
        // than a relaxation.
        let mut real = vec![b'x'; 200];
        real[199] = b'\n';
        let c = trunc_cache(real, 20_000, 64);
        let (buf, outcome) = fill_lines(&c, 0, 999, 1 << 20).await.unwrap();
        assert_eq!(
            buf.len(),
            200,
            "must still read every real byte despite the claimed size -- the fix changes the \
             witness, not what gets collected"
        );
        assert!(
            matches!(outcome, FillOutcome::End(_)),
            "the source was asked at 200 and answered empty, so 200 IS the real end and the \
             viewport must be told -- `Budget` here is what left nav painting a `$` the viewport \
             refused to render. got {outcome:?}"
        );
    }
}
