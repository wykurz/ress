//! Metering and end-certification for the engine's read loops (restructure R3, 2026-07-27,
//! `structural-budget.md` §3). Three things this module makes true by construction rather than
//! by review, one type each:
//!
//! - **A read charges, or it does not compile.** `Reader` is the ONLY way any loop in this
//!   engine touches `BlockCache::block`/`warm`; it borrows the operation's `Meter` for its whole
//!   life, so a read this crate can even build IS a charged read. Six of the nine hand-rolled
//!   `take` clamps this replaces (three different spellings of the same clamp) collapse into
//!   `classify`'s own one -- narrowed from an earlier, unqualified claim (fix round, P3-2): the
//!   remaining three, all in the reuse paths that deliberately bypass the Reader entirely
//!   (`SearchForward`'s own `seed_block`, `SearchBackward`'s own `warmed` -- `scan.rs`, both
//!   there specifically to avoid a second, cache-promoting touch of an already-fetched block)
//!   still hand-clamp, unreplaced. They also do not replicate `classify`'s own `block_is_short`
//!   logic -- worth stating rather than leaving the module doc's own "collapse into one" read as
//!   covering all nine. What they no longer do is go WITHOUT a witness: since batch 13
//!   (2026-07-29) the block itself carries the end certificate (`cache::Block::ends_data`), so a
//!   reuse path reads the same fact the read path would have, off the value it reused. That is the
//!   whole point of the certificate living with the bytes -- a shortcut cannot drop what it is
//!   already holding.
//! - **An end claim is earned, never assumed.** `CertifiedEnd` is mintable ONLY inside this
//!   module's own `classify`, from a read that came back WHOLLY EMPTY -- no public constructor,
//!   no `From<u64>`, and deliberately NOT derivable from `BlockCache::size()`, which is a
//!   snapshot that can overstate a truncated source's real data forever
//!   (`docs/budgeted_scanning.md`'s own truncation policy). **Not "short or empty"** (batch 9
//!   (2026-07-29), finding #3 -- this line said so for months while the law it summarises, stated
//!   in full on `Fetched` below, has said the opposite since restructure R3's own P1-1 fix): a
//!   short-but-nonempty answer is legal under `BlockSource`'s "up to `len`" contract for reasons
//!   other than EOF, so it certifies nothing ON ITS OWN. The two readings are not academic --
//!   believing the stale one is exactly how all three forward scanners came to treat a legal
//!   mid-file short read as end-of-file (finding #1).
//!
//!   **Amended, batch 14 (2026-07-30): `Short` may carry a witness, and it changes nothing about
//!   the law.** `BlockCache` now asks the source what lies past a short block and records the
//!   answer on the block itself (`cache::Block::ends_data`, set only by an observed EMPTY read),
//!   so `classify` can mint an `end` for a `Short` whose `got` runs to a certified block's own
//!   last byte. The claim is still earned from an observed empty read at the position it names --
//!   what changed is that the read which earned it happened inside the cache rather than in the
//!   consumer's own next call, so the fact now arrives alongside bytes instead of only in place of
//!   them. Inferring an end from a short LENGTH remains forbidden and is still what P1-1 is about.
//! - **A resumable step's `More` is earned too.** `Charged` is mintable ONLY from
//!   `Meter::progress_witness`, which fails when nothing was charged since the step began --
//!   this is the meter half of "no zero-progress `More`"; `struct-scan`'s own motion witness
//!   (R6) is the other half, and the two are never fused (`§3.5`'s seam ruling: a backward
//!   descent charges bytes it never reads, and a deferred forward candidate reads and charges
//!   while the reported answer does not move -- both legitimate `More`s neither witness alone
//!   can tell apart from the other's failure mode).
//!
//! Two named meters, not one, because two different questions were being asked of the same
//! number before this module existed (batch-5 #2): `charged()` is the budget-arithmetic number,
//! which `resolve_found`'s own `matched` used to answer wrongly via the progress-only one.
//! `progressed()` is payload bytes actually consumed toward the operation's own answer -- the
//! progress-display question, ERRATUM 3c#2's rule, unchanged, just no longer the ONLY number
//! available when budget arithmetic needed the other one.
//!
//! **What `charged()` counts, exactly** (batch 8 (2026-07-29), P2 -- this paragraph used to say
//! "every byte of I/O an operation has done", which is false in three separate directions, one of
//! them contradicted by this module's own test a few hundred lines down). The definition lives in
//! `docs/budgeted_scanning.md` and is "delivered work plus skipped spans"; restated here only to
//! name the three places "I/O" gets it wrong:
//!
//! - **A cache HIT charges exactly what a miss would.** `classify` cannot tell the two apart from
//!   `BlockCache`'s own return value and deliberately does not try, so a fully prewarmed step
//!   charges its full width having performed no physical read at all.
//! - **A physical touch can charge ZERO.** An `Empty` result genuinely asks the cache (and, on a
//!   miss, the source) and charges nothing, because `charged()` counts BYTES, not touches --
//!   `empty_block_certifies_with_a_block_jump_target`'s own assertion message states this
//!   verbatim.
//! - **`skip` charges bytes nobody ever read.** A backward descent past unreadable territory
//!   charges the whole span it passes over precisely so the descent stays bounded; those bytes are
//!   synthetic, delivered by nothing.
//!
//! The through-line is that `charged()` measures *what the budget is allowed to buy*, not what the
//! hardware did. Physical I/O is bounded by it only loosely, and `scan.rs`'s own module doc says
//! so in the same words rather than a second, differently-worded promise.
use crate::cache::BlockCache;

/// Read work, metered, for one OPERATION -- a whole interactive attempt (composed of one or more
/// phases: a search plus its own line-start hunt, `Document::resolve_found`'s own case) or one
/// background continuation. Every scan struct in this crate holds one instead of a bare `chunk:
/// usize`, so `spent` (this struct's own `spent_this_step`) is state, not a local that dies at
/// the end of a `step` call -- "declared after the loop that should charge it" stops being
/// writable (`structural-budget.md` §2.1's site 4).
///
/// Composition is why a `Meter`, not a `usize`, is the thing to share: `resolve_found` used to
/// hand the follow-up line-start hunt a freshly derived `budget.saturating_sub(matched)` --
/// arithmetic performed once, in one place, that had to independently get both operands right.
/// Passing the SAME `Meter` into both phases instead means the follow-up hunt's own `Reader`
/// sees `charged()` already reflecting phase one's spend, and `OutOfBudget` fires once the
/// SHARED `allowance` is exhausted -- correct by construction, with no subtraction anywhere to
/// get wrong.
pub(crate) struct Meter {
    chunk: usize,
    allowance: Option<u64>,
    spent_this_step: usize,
    charged: u64,
    progressed: u64,
}
impl Meter {
    /// An interactive attempt, allowance included from the start: `chunk` bounds any one `step`;
    /// `allowance` additionally bounds the WHOLE operation -- once `charged()` reaches it, every
    /// further read anywhere in the operation is `OutOfBudget`, even on a fresh `begin_step()`.
    /// Deliberately NOT what a search leg's own constructor uses (`SearchForward::new`'s own doc
    /// comment): a total ceiling from construction would cap EVERY caller that steps a leg
    /// directly, repeatedly, outside `complete()` -- most of this crate's own scan tests -- not
    /// just the one composed caller that actually wants it. `impose_allowance` retrofits the
    /// identical ceiling onto an already-in-progress `background` meter instead, at the one call
    /// site (`Document::resolve_found`) that needs it; this constructor remains for a caller that
    /// legitimately wants a total-bounded meter from its very first `begin_step()` -- none exists
    /// yet (this crate's one composed caller uses `impose_allowance` instead, retrofitting rather
    /// than constructing this way), so this is presently exercised only by this module's own unit
    /// tests below; structural-budget.md §3's own design point 1 names it explicitly ("interactive/
    /// background ctors"), so it is kept as a real, tested capability rather than removed for
    /// being momentarily unused in production.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn interactive(chunk: usize, allowance: u64) -> Meter {
        Meter {
            chunk: chunk.max(1),
            allowance: Some(allowance),
            spent_this_step: 0,
            charged: 0,
            progressed: 0,
        }
    }
    /// A background continuation, or a driver-regime pass (the sweep, the index scan) that is
    /// bounded per step by structure rather than by a byte parameter (`docs/budgeted_scanning.md`
    /// :10-19) -- `chunk` bounds each step; there is no total ceiling, so `OutOfBudget` can never
    /// fire (`Reader::read_at_unbounded` is the accessor that relies on this).
    pub fn background(chunk: usize) -> Meter {
        Meter {
            chunk: chunk.max(1),
            allowance: None,
            spent_this_step: 0,
            charged: 0,
            progressed: 0,
        }
    }
    /// Drops the total ceiling, converting an interactive meter into a background one in place.
    /// Every scan's own `complete()` calls this FIRST, before its own step loop begins: an
    /// interactive attempt that did not finish and is now a `Resolution::Pending` background
    /// continuation is, from this point on, "bounded only by the file itself, by design"
    /// (`docs/budgeted_scanning.md`'s own words) -- an `allowance` a PRIOR interactive step
    /// already spent against must not permanently cap every later step to `OutOfBudget` before
    /// it ever charges a single read (the exact shape `Meter::progress_witness` would then
    /// rightly refuse as `NoProgress`, since nothing WAS charged: this is the fix, not a
    /// workaround for that check). See `impose_allowance`'s own doc comment (this struct's own
    /// dual) for `resolve_found`'s shared, composed meter -- the one caller that goes the OTHER
    /// direction, on a DIFFERENT meter than this one ever un-caps.
    pub fn lift_allowance(&mut self) {
        self.allowance = None;
    }
    /// Imposes a total ceiling on an ALREADY-IN-PROGRESS meter -- `lift_allowance`'s own dual,
    /// going background-to-interactive instead of the other way. `Document::resolve_found`'s
    /// composition (batch-5 finding #2) is the one caller: a match search's own leg meter
    /// (`background`, no ceiling -- see `SearchForward::new`'s own doc comment for why the leg
    /// itself must NOT carry an allowance from construction) is handed here, at the exact moment
    /// the search is done and its `charged()` is final, so the follow-up line-start hunt that
    /// reuses it (`BackwardScan::with_meter`) is bounded by the SAME operation's own total, not a
    /// budget re-derived by subtracting one accessor's number from another's. Whatever `charged`/
    /// `progressed` already accumulated is kept, exactly like `lift_allowance` keeps them when
    /// going the other way -- only the ceiling changes, from none to `allowance`.
    pub fn impose_allowance(&mut self, allowance: u64) {
        self.allowance = Some(allowance);
    }
    /// Resets the per-step allowance. Every `step` calls this first, so a resumed step always
    /// gets a fresh `chunk` regardless of what a PRIOR step charged -- `progress_witness`, below,
    /// is what this resets against.
    pub fn begin_step(&mut self) {
        self.spent_this_step = 0;
    }
    /// Everything this operation has spent AGAINST ITS BUDGET -- delivered bytes plus skipped
    /// spans, bookkeeping included. NOT a measure of physical I/O (batch 9 (2026-07-29), finding
    /// #6): a cache hit charges exactly what a miss would, an `Empty` result charges zero despite
    /// genuinely touching the cache, and `skip`/`skip_ahead` charge bytes nobody ever read. This
    /// module's own top-level doc comment enumerates all three; `docs/budgeted_scanning.md` holds
    /// the canonical definition. `resolve_found`'s own budget arithmetic reads this, additive by
    /// construction.
    pub fn charged(&self) -> u64 {
        self.charged
    }
    /// Payload bytes actually consumed toward this operation's own answer -- the progress
    /// channel's own number (ERRATUM 3c#2's rule): the eight `spawn_pending` seeds and the two
    /// retry-accounting callers (`analyzer.rs`, `status.rs`) read this, never `charged()`.
    pub fn progressed(&self) -> u64 {
        self.progressed
    }
    /// Corrects `progressed` after a terminal step (`Found`/`Eof`/`Top`/`Done`) determines it
    /// needed fewer of the just-charged `Payload` bytes than the whole read supplied --
    /// ERRATUM 3c#2's rule ("`scanned` is literally bytes consumed", not the whole block just
    /// because the block happened to be in hand once a match resolved partway through it).
    /// `charged()` is UNCHANGED: the full read genuinely happened, and stays block-granular
    /// exactly as `docs/budgeted_scanning.md`'s own budget philosophy states -- only `progressed`,
    /// which answers the narrower "how much did the ANSWER need" question, gives back the part
    /// it did not.
    pub fn give_back_progress(&mut self, excess: u64) {
        self.progressed = self.progressed.saturating_sub(excess);
    }
    /// Credits `progressed` directly, with no matching `charged()` increment -- for content
    /// already in hand from an earlier `Bookkeeping` charge (`SearchForward`'s own `seed_block`,
    /// `SearchBackward`'s own `warmed`: the ctx-seed and the payload phase often land on the
    /// identical block, and the payload's own use of the ALREADY-FETCHED bytes is free in I/O
    /// terms -- `docs/budgeted_scanning.md`'s own "once a block is in hand, scanning it is free"
    /// -- but still counts as progress, exactly as it would have had a fresh `Payload` read
    /// supplied the same bytes). Charging `charged()` a second time for the same physical read
    /// would double it; not crediting `progressed` at all would under-report the answer's own
    /// consumption. Callers still call `give_back_progress` afterward if a terminal step resolves
    /// before consuming everything credited here (ERRATUM 3c#2 applies identically either way).
    pub fn record_progress(&mut self, amount: u64) {
        self.progressed += amount;
    }
    /// Mints the witness a resumable step needs to report `More` honestly: fails when nothing
    /// was charged since `begin_step`, which is exactly the shape every zero-progress livelock in
    /// this engine's own history had (batch-4 F1/F2, batch-5 fix-round P1-NEW) -- a stuck cursor
    /// and an unconditional `More` that no future call could ever escape. `struct-scan`'s own
    /// motion witness (R6) is the other half of this question ("did the cursor move"); this half
    /// answers "was any I/O actually done" and the two are never fused into one type (`§3.5`).
    pub fn progress_witness(&self) -> Result<Charged, NoProgress> {
        if self.spent_this_step > 0 {
            Ok(Charged(()))
        } else {
            Err(NoProgress)
        }
    }
    /// A direct, no-read query: has this step's allowance already run out? Exists for callers
    /// that need to decide "keep descending or hand back `More`" AFTER a `skip()`, without
    /// attempting (and having refused) a read just to discover the same fact (`Reader::skip`'s
    /// own doc comment references this for exactly that use).
    pub fn out_of_budget(&self) -> bool {
        self.spent_this_step >= self.chunk || self.allowance.is_some_and(|a| self.charged >= a)
    }
    fn charge(&mut self, amount: usize, kind: Charge) {
        self.spent_this_step += amount;
        self.charged += amount as u64;
        if kind == Charge::Payload {
            self.progressed += amount as u64;
        }
    }
}
/// Nothing was charged since the last `begin_step` -- the `More`/`Ok(false)` a caller was about
/// to report would have been a lie: the cursor would come back next call to find itself in the
/// identical state, forever. Every livelock this engine's own fix history produced was exactly
/// this, reached a different way each time; `Meter::progress_witness` is the one place that now
/// checks for it.
#[derive(Debug)]
pub(crate) struct NoProgress;
impl std::fmt::Display for NoProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "step reported More/Ok(false) without charging any read since begin_step"
        )
    }
}
impl std::error::Error for NoProgress {}
/// A witness that this step charged at least one read since it began. Minted only by
/// `Meter::progress_witness` -- there is no other constructor, so `More(Charged)` cannot be
/// written without actually asking the meter first. `PartialEq`/`Eq`/`Debug` derive trivially
/// (both sides of the wrapped `()` are always equal) -- any two witnesses of "something was
/// charged" are interchangeable, which is all the enclosing step enums' own `#[derive(PartialEq,
/// Eq)]` needs from this field; no test compares `More` by equality (all match on the shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Charged(());
/// What a charge is FOR. `Payload` is a loop's own reportable content, counted toward both
/// `charged()` and `progressed()`. `Bookkeeping` is look-behind/lookahead/seed content read to
/// make a correct decision but never itself reported as an answer -- the straddle seed, the
/// forward/backward `^`/`\b` context fetches, the F4 trailing-assertion peek: charged (so the
/// driver's own budget sees it, batch-5 #5), never progress (so ERRATUM 3c#2's rule holds,
/// unconditionally, in the one place that now enforces it instead of three separate comments
/// each arguing it by hand).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charge {
    Payload,
    Bookkeeping,
}
/// Which `BlockCache` accessor a read uses. `Payload` (`cache.block`, promotes on a probationary
/// hit) is for a loop's own user-visible content; `Peek` (`cache.warm`, does not promote) is for
/// everything else -- bookkeeping reads, and the two driver-regime passes' own payload, which
/// must not promote the working set any more than prefetch warming does (`CountScan`'s own
/// established precedent, generalized here). Independent of `Charge`: `CountScan`'s own content
/// is progress-worthy (`Charge::Payload`) even though it reads through `warm` (`Access::Peek`)
/// for this unrelated cache-policy reason.
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Payload,
    Peek,
}
/// Evidence that nothing real exists at or above position `p` -- mintable ONLY inside this
/// module, from a read that actually came back short or empty. No public constructor, no
/// `From<u64>`, and deliberately NOT derivable from `BlockCache::size()` (a snapshot that can
/// overstate a truncated source's real data forever, unchanged for the source's whole life) --
/// holding one of these is the only way anywhere in this crate to assert a position is genuinely
/// final.
///
/// **`p` is an UPPER BOUND on the real end; it is the real end EXACTLY only with a second premise
/// in hand** (batch 22 (2026-08-01), finding #1 -- this doc used to say "position `p` IS the
/// source's real end" flatly, and the one arm that cannot earn that was quietly relying on it).
/// An empty read at `p` proves `real_end <= p` and nothing more. Equality needs a real byte
/// immediately below `p`, which each mint site either has or does not:
/// - `classify`'s short-block arm (`BlockJump::ContentBelow`) and its `Fetched::Short` mint both
///   HAVE it -- real bytes sit below the position inside the very block being answered from, and
///   the source certified their end. Those are exact at the mint.
/// - `classify`'s wholly-empty arm does NOT. It answers "nothing at or above here", and the
///   premise belongs to whoever called: `p == 0` (nothing can be below zero) or a contiguous walk
///   that has already read the byte below. `fill_lines` (`scan.rs`) is the only consumer, and it
///   discharges exactly that -- see its own `Fetched::Empty` arm for the three-way case analysis
///   and for what it conservatively reports when it holds none of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CertifiedEnd(u64);
impl CertifiedEnd {
    /// **Read in production by `SearchBackward::note_empty_end` only** (`scan.rs`), which records
    /// it into `observed_end` -- and only after checking `BlockJump`, since this position is an
    /// upper bound in the wholly-empty case (see this type's own doc comment above). Batch 23
    /// (2026-08-01) corrects the claim that no production caller reads it at all, which had been
    /// true when written and stopped being true in batch 15. `fill_lines`'s own `FillOutcome::End`
    /// still consumes the variant and never the position, matching `document.rs`'s own doc comment
    /// on `buf_is_true_eof`; this module's unit tests need the exact position to verify `classify`
    /// certifies at the right one.
    pub fn at(self) -> u64 {
        self.0
    }
    /// The one crack in "no constructor outside this module", and it is `#[cfg(test)]`-only so it
    /// cannot widen: `scan.rs`'s own `note_empty_end` unit test needs to hand the recorder each
    /// `(position, BlockJump)` pair directly, since the pair it must REFUSE is the one no
    /// search-level fixture could reach without also tripping a contiguity gate that hides it.
    #[cfg(test)]
    pub(crate) fn for_test(at: u64) -> CertifiedEnd {
        CertifiedEnd(at)
    }
}
/// **Whether an end-of-data observation also licenses jumping a whole block** (batch 12
/// (2026-07-29)). `Fetched::Empty` used to mean one thing -- "this block is wholly empty" -- from
/// which two separate facts followed: the position is a certified end, AND nothing real exists
/// anywhere in the block, so a backward descent may drop a full `block_size` at once instead of
/// creeping. Now that a CONFIRMED-FINAL short block also certifies its end, those two facts come
/// apart: real bytes sit below that position inside the same block, and a descent that jumped past
/// them would silently narrow what it may report over genuine content. This type keeps the pair
/// from being conflated again -- the certification is in `end`, the jump licence is here, and a
/// consumer has to name which one it is using.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockJump {
    /// The whole block is empty: nothing real anywhere in `[block_start, at]`, so a descent may
    /// jump to `block_start` in one step.
    Safe,
    /// The block holds real content BELOW this position; only the position itself is certified.
    /// A descent must keep creeping rather than jump.
    ContentBelow,
}

/// A caller tried to skip to a position that did not strictly move in its own direction --
/// decreasing for `Reader::skip`, increasing for `Reader::skip_ahead`. Rejected rather than
/// silently accepted: the single highest-value check this module adds -- every hand-rolled
/// backward-descent fixed point in this engine's own history (`5-FR2` P1-NEW's `.max(floor)`
/// hang, a 900-second lost review run) becomes this error on the FIRST iteration that would have
/// repeated, instead of an unbounded synchronous spin no caller could ever regain control from.
#[derive(Debug)]
pub(crate) struct NotMonotone;
impl std::fmt::Display for NotMonotone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "skip target did not strictly move away from its origin")
    }
}
impl std::error::Error for NotMonotone {}
/// What one `BlockCache` touch actually returned, already classified -- the shared core both
/// `Reader::read_at` (adds `OutOfBudget` on top) and `Reader::read_at_unbounded` (has no such
/// arm to add) return through. `Bytes`/`Short` carry the WHOLE, unsliced block alongside `got`
/// (the `want`-clamped slice a caller normally wants) -- `bytes::Bytes` clones are cheap
/// (refcounted), and this is what lets a caller remember an already-fetched block for reuse at a
/// DIFFERENT offset within it later without a second, promoting cache touch (`SearchForward`'s
/// own `seed_block`, `SearchBackward`'s own `warmed`: the ctx-seed and the payload phase often
/// land on the identical block, and re-touching it via `cache.block()` after `cache.warm()`
/// already fetched it would promote it a second time -- batch-3 finding #7's own fix, preserved
/// here as the reader's own primitive rather than each loop re-deriving it).
///
/// **The law (restructure R3 fix round, P1-1): a SHORT LENGTH certifies nothing.** Batch 14
/// (2026-07-30) restated this from its original "only an EMPTY answer certifies" -- `Short` DOES
/// carry an `end` now, but never one inferred from its own length: the witness is minted from
/// `cache::Block::ends_data`, which only an observed empty read can set, so the claim is still
/// earned at the position it names. What the law forbids is unchanged. `Short` used to carry a
/// `CertifiedEnd` minted from "fewer bytes than `want` demanded" -- FALSE as a general claim: `BlockSource`'s own "up to `len`" contract (`source.
/// rs`) permits a short answer for a reason other than real EOF (unit H's own P2-1, batch 5,
/// established this for `edge_context`; this restructure's own first attempt at `fill_lines`
/// forgot it and fabricated a synthetic EOF match at an uncertified cut -- the adversarial
/// review's own P1-1 finding, `b"helloXmo"`: block 0 answers 5 of its own 8 bytes, block 1 is
/// genuinely empty, and the OLD code inferred position 5 was the true end from block 1 being
/// empty -- but position 5 is a real `'X'`, and the block-indexed cache structurally cannot
/// re-ask block 0 for the gap `[5, 8)` it never fetched, so that inference is never actually
/// verifiable, only ever a guess that happens to be right when the gap is empty too). `Short`
/// therefore carried no witness at all -- until batch 14 (2026-07-30) gave it one that is not an
/// inference from its own length: see the amendment above and `Fetched::Short`'s own `end`. What
/// stayed constant across both is the rule, not the shape: a short LENGTH certifies nothing.
///
/// **Correction (batch 6 (2026-07-28), finding #5's docket item, K1's review): the paragraph
/// used to say a caller wanting to verify a short answer's own boundary "must keep reading
/// FORWARD from `got`'s own boundary ... until a GENUINELY `Empty` answer certifies, at THAT
/// position -- `fill_lines`'s own certify-loop (`scan.rs`) is the one place that does this."**
/// No such loop exists anywhere in this tree, at this sha or any prior one. `fill_lines`
/// (`scan.rs`) does the opposite: on a short read that does not complete a row, it returns
/// immediately -- a conservative miss, never a forward probe past the short boundary -- with
/// `FillOutcome::Budget` unless the answer arrived already certified. R3's own fix round replaced
/// an earlier, unsound probe-and-infer attempt with exactly this conservative return (the
/// `b"helloXmo"` fabrication the P1-1 law above exists to close). **`fill_lines` is the ONLY
/// consumer of a minted end-witness anywhere in this crate** (the only `Empty { end` binding
/// outside this module, and the only reader of `Short`'s own `end`): its own single read AT its
/// current position (not a search past a short one) turns that witness into `FillOutcome::End`,
/// propagated to `Document::viewport` as `buf_is_true_eof` (the variant only, never the position). **Fix round (batch 6 (2026-07-28), finding #5, K3 review P2-2):
/// `SearchBackward`'s own terminal check (`scan.rs`) is NOT a second consumer** -- an earlier
/// version of this paragraph claimed it "collapses a single `Empty` read ... into 'nothing more
/// will ever be read'", which overstates what it does.
///
/// **Superseded in part, batch 15 (2026-07-30), and the correction recorded here by batch 17
/// (2026-07-31), finding #3: `SearchBackward` no longer discards the witness -- on the two arms
/// that decide anything.** Its SEED-gather and PAYLOAD `Empty` arms retain the position into
/// `SearchBackward::observed_end`, which is what lets a window assembled several iterations later
/// know where the source's data really ends -- so `fill_lines` is no longer the only consumer of an
/// observed-`Empty` mint. Two OTHER `Empty` arms still discard it, correctly: the terminal-descent
/// arm (whose own question is the block-jump licence, not a position) and the bookkeeping
/// look-behind arm (which gathers context and never reports an answer). Narrowed from an
/// over-broad "its `Empty` arms retain the position", batch 18 (2026-07-31). What the sentences below still describe
/// correctly is the SLICE-EMPTINESS decision beside it: that one matches on `got` being empty,
/// reachable three ways -- a genuine `Empty` (whose witness the
/// leg records separately, as above, and which this particular test does not consult), a `Short`
/// whose own `got` is empty (`take == 0`, a `Short` boundary, exactly what
/// this paragraph says nothing may certify from), or the `warmed` reuse path finding
/// `lo_off >= hi_off` with no read at all. It cannot distinguish any of the three, by its own
/// adjacent comment's own words ("an empty result here can ONLY be reached via a SHORT/EMPTY
/// block"). It is sound for its own purpose regardless -- an empty-*slice* test that deliberately
/// does not distinguish `Empty` from `Short`, safe to descend past on a backward leg precisely
/// because it certifies nothing about POSITION, only that nothing reportable sits at this one --
/// but it is not itself a second instance of the `Empty`-certifies law this
/// paragraph documents. The forward-probe-to-certify capability this paragraph described remains
/// an unimplemented docket item (the `CertifiedEnd` restructure batch 5's own unit H report
/// cites: a witness mintable from an observed-empty read at ANY position, probing past a short
/// block's own edge to find it) -- not a phantom, but not landed either; a caller that needs
/// it today gets `fill_lines`'s own honest `Budget` miss instead.
#[derive(Debug)]
pub(crate) enum Fetched {
    /// Bytes arrived, no shortfall observed -- more may exist past them.
    Bytes {
        got: bytes::Bytes,
        whole_block: crate::cache::Block,
        block_start: u64,
    },
    /// Fewer bytes than `want` demanded, OR a touch that landed past a short block's own real
    /// content (`got` empty in that case) -- NOT, on its own, a certificate that the source's
    /// real data ends here (see this enum's own doc comment, the law P1-1 established). Distinct
    /// from `Empty` below only in whether the containing BLOCK itself had any real content at
    /// all; carries `block_start` so a caller that must keep verifying can compute the NEXT
    /// block's own start (`block_start + block_size`) without re-deriving it.
    Short {
        got: bytes::Bytes,
        whole_block: crate::cache::Block,
        block_start: u64,
        /// **The one amendment to the law above** (batch 14 (2026-07-30)): present exactly when the
        /// SOURCE ITSELF certified that its real data ends where this block's bytes do
        /// (`cache::Block::ends_data`, mintable nowhere but inside `BlockCache::fetch`, from an
        /// observed EMPTY read) AND `got` reaches that end. It is not an inference from a short
        /// length -- that remains forbidden and is what P1-1 is about; it is the same observed-empty
        /// fact `Empty` carries, reported at the position it was earned for by a read that also
        /// returned bytes. `None` on every other short answer, including a block that is short only
        /// because the file's own CLAIMED size ends inside it: nobody asked the source about that
        /// one.
        ///
        /// Without it the certificate stopped one layer short of the consumers that decide "is this
        /// the end": `fill_lines` reported `Budget` at a certified end, so the viewport painted no
        /// `$` where forward search had just found one, and `SearchBackward` fell back to comparing
        /// against a stale `cache.size()` and answered `Exhausted` for matches that exist.
        end: Option<CertifiedEnd>,
    },
    /// Nothing at all at THIS position, and the block behind it offers nothing more. Two shapes,
    /// distinguished by `jump` (see `BlockJump`): a WHOLLY empty block (`Safe`), and a short block
    /// whose real content sits below this position and whose end the source certified
    /// (`ContentBelow`, batch 12 (2026-07-29)) -- the second is not empty, and describing every
    /// `Empty` as a wholly empty block is the wording batch 19 (2026-07-31) corrected. `end` names
    /// `at` (never an earlier short answer's own boundary), and **`jump` is also what says whether
    /// that position is EXACT or merely an upper bound** (batch 22 (2026-08-01), finding #1):
    /// `ContentBelow` earns exactness in the arm itself, since real bytes sit immediately below
    /// `at` inside the same block; `Safe` cannot, since a wholly empty block says only that
    /// nothing exists at or above its own start. See `CertifiedEnd`'s own doc comment for the
    /// premise a `Safe` consumer must supply. Also the block-jump-safe case: nothing real
    /// exists anywhere in `[block_start, at]`, so a backward descent may jump a whole block's
    /// width at once instead of creeping down byte by byte (`5-FR1`/`5-FR3`'s own `P3-4`/`P2-1`
    /// distinction, generalized here into the one gate every caller now shares instead of each
    /// hand-rolling its own `block.is_empty()` check).
    Empty {
        end: CertifiedEnd,
        block_start: u64,
        /// Whether a backward descent may jump this whole block at once -- see `BlockJump`.
        jump: BlockJump,
    },
}
/// The result of one `Reader::read_at` call -- always already charged. `#[must_use]` because
/// every variant demands a different response and none may be silently treated as any other:
/// an ignored `OutOfBudget` mistaken for `Bytes` is exactly the "still writable, but only in the
/// one place nobody checks" defect class this type exists to close.
/// **Only `Empty` carries a witness** (see `Fetched`'s own doc comment for the law and its own
/// P1-1 history) -- `Short` does not, even though structural-budget.md §3's own design point 3
/// originally sketched both carrying one. `CertifiedEnd` has no public constructor, so the mere
/// ABILITY to construct `Empty` here is itself proof the observation was real; no `Short`
/// construction anywhere in this module can make an equivalent claim, by design, now.
#[must_use]
#[derive(Debug)]
pub(crate) enum ReadOutcome {
    Bytes {
        got: bytes::Bytes,
        whole_block: crate::cache::Block,
        block_start: u64,
    },
    Short {
        got: bytes::Bytes,
        whole_block: crate::cache::Block,
        block_start: u64,
    },
    Empty {
        #[allow(dead_code)]
        end: CertifiedEnd,
        #[allow(dead_code)]
        block_start: u64,
        /// Carried for parity with `Fetched::Empty`, whose own `jump` the two backward ctx-gathers
        /// consult (`scan.rs`). No `ReadOutcome` consumer needs it yet: the loops that jump all
        /// read through `read_at_unbounded`, which returns `Fetched` directly.
        #[allow(dead_code)]
        jump: BlockJump,
    },
    /// The step's allowance ran out BEFORE this read: nothing touched, cursor unmoved, genuinely
    /// resumable (batch-4 F1's own discrimination between this and a terminal short/empty read,
    /// now the reader's structural guarantee instead of each loop's own exit analysis).
    OutOfBudget,
}
impl From<Fetched> for ReadOutcome {
    fn from(f: Fetched) -> Self {
        match f {
            Fetched::Bytes {
                got,
                whole_block,
                block_start,
            } => ReadOutcome::Bytes {
                got,
                whole_block,
                block_start,
            },
            Fetched::Short {
                got,
                whole_block,
                block_start,
                // deliberately NOT carried onto `ReadOutcome` (batch 14 (2026-07-30)): its one
                // consumer -- `SearchBackward`'s own high-edge decision -- derives the same fact
                // from `whole_block`, which it must do anyway because its `warmed` REUSE path has a
                // block and no `ReadOutcome` at all. Two derivations of one fact is the shape that
                // produced finding #4 (a shortcut that forgot the certificate); one is the fix.
                end: _,
            } => ReadOutcome::Short {
                got,
                whole_block,
                block_start,
            },
            Fetched::Empty {
                end,
                block_start,
                jump,
            } => ReadOutcome::Empty {
                end,
                block_start,
                jump,
            },
        }
    }
}
/// Classifies one `BlockCache` touch -- the shared core `Reader::read_at`/`read_at_unbounded`
/// both fetch through. `refuse` is the WHOLE budget decision, checked FIRST: `true` means the
/// caller has already determined (via `Meter::out_of_budget`, or a deliberate "never" for the
/// unbounded bookkeeping path) that this read must not happen, and `classify` returns `Ok(None)`
/// without touching the cache at all -- batch 6 (2026-07-28), finding #5. Before this, the check
/// lived entirely in `Reader::read_at`'s own caller-side `if`, one statement above its call to
/// this function: correct in that one place by construction of the code as written, but nothing
/// stopped a DIFFERENT caller (there was, and is, exactly one other -- `read_at_unbounded`) from
/// reaching this function's own fetch with no check at all, relying on a convention ("call the
/// right wrapper") rather than a guarantee this function itself enforces. Moving the decision
/// inside `classify` makes "a refused read never touches the cache" true of the PRIMITIVE, not
/// merely of its one careful caller today -- `read_at` now always passes its own live
/// `Meter::out_of_budget()` value; `read_at_unbounded` always passes `false`, spelling out its own
/// documented exemption at the call site instead of by omission.
async fn classify(
    cache: &BlockCache,
    at: u64,
    want: usize,
    access: Access,
    refuse: bool,
) -> anyhow::Result<Option<(Fetched, usize)>> {
    if refuse {
        return Ok(None);
    }
    // P3-1: `want == 0` would otherwise reach the `take == 0` branch below for a reason that has
    // nothing to do with a short block's own real content ending -- a caller bug, not a
    // legitimate certification input. Every current call site already clamps its own `want` to
    // at least 1 before calling, so this is not reachable today; cheap insurance against a
    // future one that does not. (Now that `Short` mints no witness FROM ITS OWN LENGTH -- see
    // `Fetched`'s
    // own doc comment, the P1-1 law -- a `want == 0` misfire could no longer mint a false
    // `CertifiedEnd` the way it could before this fix round; kept anyway, since a `want == 0`
    // read is a wasted round-trip regardless of what it returns.)
    debug_assert!(want > 0, "classify called with want == 0");
    let bs = cache.block_size() as u64;
    let idx = at / bs;
    let block = match access {
        Access::Payload => cache.block(idx).await?,
        Access::Peek => cache.warm(idx).await?,
    };
    let block_start = idx * bs;
    if block.is_empty() {
        return Ok(Some((
            Fetched::Empty {
                end: CertifiedEnd(at),
                block_start,
                jump: BlockJump::Safe,
            },
            0,
        )));
    }
    // A block shorter than the FULL nominal block size is the only honest signal that the
    // source's real data ends inside it (`BlockCache::block`/`warm`'s own contract: "short at
    // EOF, empty past EOF") -- a FULL block simply not having `want` bytes to give says nothing
    // about the file's overall end, only that this one block's own capacity was exhausted (the
    // caller's next read, at the next block, is ordinary). Certifying on `take < want` alone
    // (an earlier version of this function did) was wrong whenever `want` merely exceeded one
    // block's own capacity -- unreachable by every EXISTING call site (each already clamps its
    // own `want` to well under a block), but a landmine for whichever future one does not.
    //
    // Neither `Short` arm below mints a `CertifiedEnd` FROM ITS OWN LENGTH (P1-1 -- see `Fetched`'s
    // own doc comment for why a short-but-nonempty answer can never certify on its own). The
    // second arm does PASS ONE ON when the block in hand already carries it, which is a different
    // act: the witness was minted by an observed empty read inside `BlockCache::fetch`, and this
    // function only reads it off `Block::ends_data` (batch 14 (2026-07-30); batch 22 (2026-08-01)
    // corrects this comment, which claimed the blanket "neither arm mints one" directly above the
    // arm that sets `end: Some(..)`).
    // The hole a memoized SHORT block leaves is closed by `BlockCache` itself now, before any
    // block reaches this function (batch 12 (2026-07-29) -- `BlockCache::get`'s own doc comment has
    // the derivation, including why it lives there rather than here). What remains for this
    // function is to READ the answer that refill produced: a short block the source itself
    // certified the end of is a genuine end-of-data observation, and certifies.
    //
    // batch 13 (2026-07-29), finding #1: that answer is read off THE BLOCK IN HAND
    // (`Block::ends_data`), never re-queried by index. The old `cache.is_confirmed_final(idx)`
    // asked a question this function cannot ask correctly -- finality is a fact about a specific
    // LENGTH, and by the time the answer came back the entry behind that index could be a
    // different length than the `block` local here (two racing refills, one shorter). Certifying
    // `block_start + block.len()` from a certificate earned for some OTHER length is exactly how a
    // 7-byte file came to report EOF at 6.
    let block_is_short = block.len() < bs as usize;
    let lo = ((at - block_start) as usize).min(block.len());
    let available = block.len() - lo;
    let take = available.min(want);
    if take == 0 {
        // Nothing left in this block. If its length has been CONFIRMED final, that is a genuine
        // end-of-data observation at `block_start + block.len()` and it certifies -- the source
        // itself answered empty there. `BlockJump::ContentBelow` is what keeps it honest: real
        // bytes sit below this position inside the same block, so unlike a wholly-empty block this
        // is NOT safe to jump a whole block's width past (see `BlockJump`'s own doc comment).
        if block_is_short && block.ends_data() {
            return Ok(Some((
                Fetched::Empty {
                    end: CertifiedEnd(block_start + block.len() as u64),
                    block_start,
                    jump: BlockJump::ContentBelow,
                },
                0,
            )));
        }
        return Ok(Some((
            Fetched::Short {
                got: bytes::Bytes::new(),
                whole_block: block,
                block_start,
                // unreachable as `Some`: a certified short block with nothing left at this position
                // took the `Empty` arm above, which is where that case belongs (it has a
                // `BlockJump` to report as well).
                end: None,
            },
            0,
        )));
    }
    let got = block.slice(lo..lo + take);
    if take == available && block_is_short {
        // batch 14 (2026-07-30): `got` runs to this block's own last byte, so if the SOURCE
        // certified that its data ends there, this answer ends there too -- and says so. Minted
        // from `Block::ends_data`, never from `take < want`; see `Fetched::Short`'s own `end`.
        let end = block
            .ends_data()
            .then(|| CertifiedEnd(block_start + block.len() as u64));
        Ok(Some((
            Fetched::Short {
                got,
                whole_block: block,
                block_start,
                end,
            },
            take,
        )))
    } else {
        Ok(Some((
            Fetched::Bytes {
                got,
                whole_block: block,
                block_start,
            },
            take,
        )))
    }
}
/// The ONLY read a budgeted loop performs -- borrows the operation's `Meter` for its whole life,
/// so a read this crate can even compile is a charged read. Cheap to construct; expected fresh
/// once per `step` call (never held across an `.await` outside a read of its own).
pub(crate) struct Reader<'a> {
    cache: &'a BlockCache,
    meter: &'a mut Meter,
}
impl<'a> Reader<'a> {
    pub fn new(cache: &'a BlockCache, meter: &'a mut Meter) -> Reader<'a> {
        Reader { cache, meter }
    }
    /// Reads up to `want` bytes at `at` through `access`'s accessor, charged as `kind` against
    /// this step's allowance -- refuses even to touch the cache once that allowance is spent.
    /// batch 6 (2026-07-28), finding #5: the refusal decision is now `classify`'s own first
    /// action (`refuse: self.meter.out_of_budget()`), not a check this function performs before
    /// calling an unconditional primitive -- see `classify`'s own doc comment.
    pub async fn read_at(
        &mut self,
        at: u64,
        want: usize,
        access: Access,
        kind: Charge,
    ) -> anyhow::Result<ReadOutcome> {
        match classify(self.cache, at, want, access, self.meter.out_of_budget()).await? {
            None => Ok(ReadOutcome::OutOfBudget),
            Some((fetched, amount)) => {
                self.meter.charge(amount, kind);
                Ok(fetched.into())
            }
        }
    }
    /// The driver-regime accessor (`docs/budgeted_scanning.md`'s two background passes, bounded
    /// per step by structure rather than a byte parameter, `§3.6`): identical classification, no
    /// budget check at all, so the three-variant result never needs an `OutOfBudget` arm a caller
    /// would have to handle with an `unreachable!()`. Still charges `self.meter` (a
    /// `Meter::background` one, conventionally) for the same reason every other read here does --
    /// a uniform record of budget spent, even where nothing gates on it today (batch 9
    /// (2026-07-29), finding #6: "I/O done" overstated it, see `Meter::charged`). batch 6 (2026-07-28),
    /// finding #5: passes `refuse: false` to `classify` explicitly -- this function's own
    /// exemption from the budget gate is now a visible choice at its one call into the shared
    /// primitive, not an absence of a check nobody happened to add.
    pub async fn read_at_unbounded(
        &mut self,
        at: u64,
        want: usize,
        access: Access,
        kind: Charge,
    ) -> anyhow::Result<Fetched> {
        let (fetched, amount) = classify(self.cache, at, want, access, false)
            .await?
            .expect("classify(refuse: false) always returns Some");
        self.meter.charge(amount, kind);
        Ok(fetched)
    }
    /// Charges a span the caller chooses to SKIP without reading -- a backward descent past
    /// territory a prior `Empty`/`Short` observation already certified as unreadable. Always
    /// `Charge::Bookkeeping` (nothing skipped was ever a candidate answer). Rejects `to >= from`:
    /// see `NotMonotone`'s own doc comment for why this is the highest-value check here.
    pub fn skip(&mut self, from: u64, to: u64) -> Result<(), NotMonotone> {
        if to >= from {
            return Err(NotMonotone);
        }
        self.meter.charge((from - to) as usize, Charge::Bookkeeping);
        Ok(())
    }
    /// `skip`'s FORWARD twin (batch 9 (2026-07-29), finding #1): charges a span an ASCENDING loop
    /// passes over without reading -- the gap left behind when a legally short block is exhausted
    /// mid-file and the scan jumps to the next block rather than mistaking the shortfall for EOF.
    /// Rejects `to <= from` for the identical reason `skip` rejects `to >= from`: a jump that does
    /// not strictly advance is a fixed point, and a forward loop sitting on one spins exactly as
    /// unrecoverably as the backward hangs `NotMonotone`'s own doc comment describes.
    ///
    /// Charging matters more here than it might look: the read that triggers this jump delivered
    /// ZERO bytes, so it charged zero. Without this, a source answering one byte per block would
    /// let a single `step` walk an entire file's worth of blocks with no budget consumed at all.
    pub fn skip_ahead(&mut self, from: u64, to: u64) -> Result<(), NotMonotone> {
        if to <= from {
            return Err(NotMonotone);
        }
        self.meter.charge((to - from) as usize, Charge::Bookkeeping);
        Ok(())
    }
    /// Delegates to `Meter::out_of_budget` -- for a descent loop that charges its own skip via
    /// `skip()` (never refused on its own) and then must decide whether to keep descending or
    /// hand back `More`, without spending a wasted read attempt just to discover the answer.
    pub fn out_of_budget(&self) -> bool {
        self.meter.out_of_budget()
    }
    /// Delegates to `Meter::progress_witness` -- a `Reader` borrows the meter for a whole `step`
    /// call, so a caller holding one cannot also reach `self.meter` directly without a second,
    /// conflicting mutable borrow; this (and the two methods below) are that access, not a new
    /// capability.
    pub fn progress_witness(&self) -> Result<Charged, NoProgress> {
        self.meter.progress_witness()
    }
    /// Delegates to `Meter::give_back_progress` -- see `progress_witness`'s own doc comment for
    /// why this exists on `Reader` at all.
    pub fn give_back_progress(&mut self, excess: u64) {
        self.meter.give_back_progress(excess);
    }
    /// Delegates to `Meter::record_progress` -- see `progress_witness`'s own doc comment for why
    /// this exists on `Reader` at all.
    pub fn record_progress(&mut self, amount: u64) {
        self.meter.record_progress(amount);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::BlockCache;
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
    async fn read_at_charges_full_bytes_as_payload() {
        let c = cache(b"0123456789", 4);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(0, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Bytes { got: b, .. } => assert_eq!(&b[..], b"0123"),
            other => panic!("expected Bytes, got {other:?}"),
        }
        assert_eq!(m.charged(), 4);
        assert_eq!(m.progressed(), 4);
    }
    #[tokio::test]
    async fn bookkeeping_charges_but_never_progresses() {
        let c = cache(b"0123456789", 4);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        let _ = r
            .read_at(0, 4, Access::Peek, Charge::Bookkeeping)
            .await
            .unwrap();
        assert_eq!(m.charged(), 4, "bookkeeping still charges the budget");
        assert_eq!(m.progressed(), 0, "but never counts as progress");
    }
    #[tokio::test]
    async fn short_read_does_not_certify() {
        // fix round, P1-1: `Short` mints no witness from its own length -- a short-but-nonempty
        // answer is
        // legal under `BlockSource`'s own "up to len" contract for a reason other than real EOF,
        // so it must never claim to certify the true end on its own (only `Empty` may, at its
        // own position -- `empty_block_certifies_with_a_block_jump_target`, below).
        let c = cache(b"01234567", 100); // one 8-byte block, "block size" 100
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(0, 100, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Short { got, .. } => {
                assert_eq!(&got[..], b"01234567");
            }
            other => panic!("expected Short, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn a_full_block_short_of_a_large_want_does_not_falsely_certify() {
        // `want` exceeding a FULL block's own capacity must never be mistaken for a truncated
        // source: block 0 here is completely full (4 of 4 bytes), a second, equally full block
        // exists right after it -- asking for more than one block can ever give, in a single
        // `read_at`, must report ordinary `Bytes`, not `Short`.
        let c = cache(b"01234567", 4);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(0, 100, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Bytes { got: b, .. } => assert_eq!(&b[..], b"0123"),
            other => panic!(
                "a full block short of an oversized `want` must be Bytes, not a false Short/Empty \
                 certification; got {other:?}"
            ),
        }
    }
    #[tokio::test]
    async fn empty_block_certifies_with_a_block_jump_target() {
        let c = cache(b"01234567", 4); // block 0 real, block 1+ empty
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(8, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Empty {
                end, block_start, ..
            } => {
                assert_eq!(end.at(), 8);
                assert_eq!(block_start, 8);
            }
            other => panic!("expected Empty, got {other:?}"),
        }
        // P3-6: this used to read "nothing was actually read" -- false. `charge(0, kind)` is a
        // no-op on `charged()` because `Empty`'s own `amount` is 0 (nothing counted), but the
        // cache (and the source, on a miss) WAS genuinely touched -- a physical read happened,
        // it simply came back with zero bytes. Every descent site pairs an `Empty` with a
        // `skip()` that charges the span it jumps, so this never lets an unbounded uncharged
        // loop through; it is not a claim that the touch itself was free.
        assert_eq!(
            m.charged(),
            0,
            "charged() counts bytes, not touches -- an Empty result reads zero of them even though the cache was genuinely asked"
        );
    }
    #[tokio::test]
    async fn a_short_read_past_a_short_blocks_own_end_still_does_not_certify() {
        // block 0 has only 3 real bytes (block_size 8); revisiting position 3..8 within it is
        // the case `Fetched::Short` with an EMPTY `got` exists for (distinct from `Empty`: this
        // block is not wholly empty, it just has nothing left past position 3) -- fix round,
        // P1-1: still does NOT certify, same as any other UNCERTIFIED `Short`, empty `got` or not.
        // Nothing certified this one: block 0 resolved on the claimed size (3 of a nominal 8), so
        // the source was never asked what lies past it and `Block::ends_data` stays false.
        let c = cache(b"abc", 8);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(3, 5, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Short { got, .. } => {
                assert!(got.is_empty());
            }
            other => panic!("expected Short with empty got, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn out_of_budget_never_touches_the_cache() {
        // count physical reads through the MockSource behind the cache -- OutOfBudget must not
        // even warm a block.
        let src = Arc::new(MockSource::new(bytes::Bytes::from_static(b"0123456789")));
        let c = BlockCache::new(src.clone(), 4, 1 << 20);
        let mut m = Meter::interactive(4, 4);
        let mut r = Reader::new(&c, &mut m);
        let _ = r
            .read_at(0, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap();
        match r
            .read_at(4, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::OutOfBudget => {}
            other => panic!("expected OutOfBudget, got {other:?}"),
        }
        assert_eq!(
            src.read_count(),
            1,
            "the second, refused read must not touch the source"
        );
    }
    #[tokio::test]
    async fn classify_itself_refuses_before_touching_the_cache() {
        // batch 6 (2026-07-28), finding #5: `out_of_budget_never_touches_the_cache` (above)
        // already pinned this property for `Reader::read_at`'s own caller-side check -- but that
        // check used to live entirely in `read_at`, one statement above its call into `classify`,
        // which itself had no way to refuse at all. This test exercises `classify` directly (it
        // is a private fn, reachable from this same module's own test code) with `refuse: true`,
        // proving the primitive itself now enforces "never touch the cache when refused" --
        // reachable by ANY caller, not only the one that happened to check first. Both a cold
        // source and a prewarmed one: refusing must cost zero NEW physical reads either way.
        let src = Arc::new(MockSource::new(bytes::Bytes::from_static(b"0123456789")));
        let c = BlockCache::new(src.clone(), 4, 1 << 20);
        let refused = classify(&c, 0, 4, Access::Payload, true).await.unwrap();
        assert!(
            refused.is_none(),
            "refuse: true must yield None, not a fetch"
        );
        assert_eq!(
            src.read_count(),
            0,
            "a refused classify must not touch the source at all"
        );
        // prewarm, then refuse again -- still zero NEW physical reads (the warm touch below is
        // the only one, confirming refusal doesn't even consult the cache's own warm state).
        let _ = c.warm(0).await.unwrap();
        assert_eq!(
            src.read_count(),
            1,
            "the prewarm itself is the one real physical read"
        );
        let refused_warm = classify(&c, 0, 4, Access::Payload, true).await.unwrap();
        assert!(refused_warm.is_none());
        assert_eq!(
            src.read_count(),
            1,
            "refusing against an already-warm cache still must not re-touch the source"
        );
        // refuse: false behaves exactly as the old, unconditional classify always did.
        let allowed = classify(&c, 0, 4, Access::Payload, false).await.unwrap();
        assert!(
            matches!(allowed, Some((Fetched::Bytes { .. }, 4))),
            "refuse: false must fetch and classify normally, got {allowed:?}"
        );
    }
    #[tokio::test]
    async fn allowance_bounds_a_composed_operation_across_two_readers() {
        // the composition property (`resolve_found`'s own fix): a SHARED meter's allowance
        // bounds the total across two logically separate "phases" without any subtraction. A
        // fresh `begin_step` (as a follow-up phase's own `step` call would do) resets the
        // PER-STEP chunk check, but the shared, never-reset `allowance` still catches it.
        let c = cache(b"0123456789abcdef", 4);
        let mut m = Meter::interactive(100, 4);
        {
            let mut r = Reader::new(&c, &mut m);
            let _ = r
                .read_at(0, 4, Access::Payload, Charge::Payload)
                .await
                .unwrap();
        }
        assert_eq!(m.charged(), 4, "phase one spent the whole shared allowance");
        m.begin_step();
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(4, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::OutOfBudget => {}
            other => {
                panic!("expected OutOfBudget once the shared allowance is spent, got {other:?}")
            }
        }
    }
    #[tokio::test]
    async fn skip_rejects_a_non_decreasing_target() {
        let c = cache(b"0123456789", 4);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        assert!(r.skip(100, 100).is_err(), "skip(100, 100) must be Err");
        assert!(r.skip(100, 120).is_err(), "skip(100, 120) must be Err");
        assert!(r.skip(100, 99).is_ok(), "skip(100, 99) must be Ok");
        assert_eq!(m.charged(), 1);
    }
    #[tokio::test]
    async fn skip_ahead_rejects_a_non_increasing_target() {
        // `skip`'s forward twin (batch 9 (2026-07-29), finding #1): same fixed-point rejection,
        // mirrored -- a forward jump that does not strictly advance is exactly as unrecoverable a
        // spin as the backward hangs `NotMonotone` exists to catch.
        let c = cache(b"0123456789", 4);
        let mut m = Meter::interactive(100, 100);
        let mut r = Reader::new(&c, &mut m);
        assert!(
            r.skip_ahead(100, 100).is_err(),
            "skip_ahead(100, 100) must be Err"
        );
        assert!(
            r.skip_ahead(100, 80).is_err(),
            "skip_ahead(100, 80) must be Err"
        );
        assert!(
            r.skip_ahead(100, 101).is_ok(),
            "skip_ahead(100, 101) must be Ok"
        );
        assert_eq!(
            m.charged(),
            1,
            "the skipped span is charged, like its backward twin's"
        );
    }
    #[tokio::test]
    async fn progress_witness_fails_when_nothing_was_charged_this_step() {
        let mut m = Meter::interactive(100, 100);
        assert!(m.progress_witness().is_err());
        m.begin_step();
        m.charge(1, Charge::Bookkeeping);
        assert!(m.progress_witness().is_ok());
    }
    #[tokio::test]
    async fn give_back_progress_corrects_progressed_without_touching_charged() {
        // ERRATUM 3c#2: a terminal step that resolves partway through a physically-larger read
        // (the whole block was fetched, block-granular, but the answer only needed the first
        // few bytes of it) must count only those bytes toward `progressed`, while `charged`
        // keeps reflecting the REAL I/O work done.
        let mut m = Meter::interactive(100, 100);
        m.charge(10, Charge::Payload);
        assert_eq!(m.charged(), 10);
        assert_eq!(m.progressed(), 10);
        m.give_back_progress(6);
        assert_eq!(m.charged(), 10, "the physical read is unchanged");
        assert_eq!(
            m.progressed(),
            4,
            "only the bytes the answer needed count as progress"
        );
    }
    #[tokio::test]
    async fn lift_allowance_unblocks_a_background_continuation() {
        // the bug this exists to fix: an interactive meter's `allowance`, once spent, must not
        // permanently cap every LATER step (a background continuation via `complete()`) to
        // `OutOfBudget` before it ever charges a read again.
        let c = cache(b"0123456789", 4);
        let mut m = Meter::interactive(4, 4);
        {
            let mut r = Reader::new(&c, &mut m);
            let _ = r
                .read_at(0, 4, Access::Payload, Charge::Payload)
                .await
                .unwrap();
        }
        assert_eq!(m.charged(), 4, "the whole allowance is spent");
        m.lift_allowance();
        m.begin_step();
        let mut r = Reader::new(&c, &mut m);
        match r
            .read_at(4, 4, Access::Payload, Charge::Payload)
            .await
            .unwrap()
        {
            ReadOutcome::Bytes { got: b, .. } => assert_eq!(&b[..], b"4567"),
            other => panic!(
                "a lifted allowance must let a background continuation keep reading; got {other:?}"
            ),
        }
    }
}
