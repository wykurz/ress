//! The accept model, centralized: restructure R4 (2026-07-27), `.superpowers/sdd/
//! structural-accept.md` §3. Before this module, "when is a regex match over a windowed byte
//! view trustworthy?" was answered independently at nine call sites across `search.rs`,
//! `scan.rs`, and `document.rs`, each hand-deriving its own margin arithmetic and its own
//! absolute/local coordinate shifts. `Hay` is now the ONLY place a windowed byte slice is handed
//! to the regex engine (`tests/no_raw_hay_primitives.rs` is the CI guard: `SearchPattern`'s own
//! `find_starting_in`/`rfind_starting_in`/`find_all_starting_in` are `pub(crate)` for THIS
//! module's own use, and this module is the only caller anywhere in the crate).
//!
//! **R4 was behavior-preserving; R5 (2026-07-28) is the semantic increment.** R4 reproduced each
//! consumer's then-current accept semantics exactly -- the start-based gates, each consumer's own
//! existing end constraint (or lack of one), the sweep's seam exclusion -- landing the type
//! deliberately shaped so the ONE real behavior change (`.superpowers/sdd/restructure-plan.md`'s
//! own R5 scoping) could arrive in ONE place instead of nine. It has: `verified` is now the single
//! end rule every enumeration method routes through (`first_verified`/`last_verified`/
//! `starts_verified`/`leftmost_confirmed`/`point_verified`/`all_verified_row`/
//! `zero_width_at_high`'s own leading half), assertion-aware via `SearchPattern::leading_
//! assertions`/`trailing_assertions` (a one-time, conservative `regex_syntax` HIR walk, "on any
//! doubt, true" -- `search.rs`'s own doc comment on those fields has the full derivation). The
//! unit-J fabrication class (an over-cap match reading a window's own artificial edge as if it
//! were the true end) dies for any pattern that carries the assertion needed to exploit it; a
//! pattern that carries none keeps reporting an over-cap match exactly as before, since a hay cut
//! cannot manufacture a body -- only fail to complete one.
//!
//! **The two counting walks stay two** (`structural-accept.md` §3.5's own near-miss, argued
//! there in detail): `SweepAnalysis` counts every *start position* (`starts_verified`, advancing
//! past each found start by exactly one byte, so an overlapping later start is still offered);
//! the viewport counts *non-overlapping matches* (`all_verified_row`, advancing past each
//! found match's own end). Unifying them would silently change `n`'s own overlapping-start
//! contract (`document.rs`'s own `visible_window_matches` doc comment explains why) -- so this
//! module exposes both shapes rather than forcing one.
//!
//! **What stays outside this type, deliberately** (`structural-accept.md` §6): `line_start`/
//! `last_nl` bookkeeping is a LINE question, not an accept question -- `SearchForward`/
//! `SearchBackward` still derive it themselves, via `memchr` over the same bytes, after asking
//! this module which candidate (if any) was accepted. Budget/charge accounting (which byte a
//! read cost) is `meter.rs`'s domain, untouched here -- a `Hay` is a borrowed view over bytes
//! some earlier `Reader` call already paid for.

use crate::search::SearchPattern;
use std::ops::Range;

/// An absolute byte offset into the file. The only way to get one outside this module is a
/// legitimate mint (`BlockCache::size()`, a scan's own persisted cursor) or `Hay::to_abs` --
/// there is no `From<u64>`, so an absolute and a hay-local index can never be added by accident.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Abs(pub(crate) u64);
impl std::ops::Add<u64> for Abs {
    type Output = Abs;
    fn add(self, rhs: u64) -> Abs {
        Abs(self.0 + rhs)
    }
}
impl std::ops::Sub<Abs> for Abs {
    type Output = u64;
    fn sub(self, rhs: Abs) -> u64 {
        self.0 - rhs.0
    }
}

/// An index into some `Hay`'s own bytes. Meaningless without the hay it came from -- two
/// `Local`s minted from different hays are not comparable to anything but each other's own
/// numeric value, and this type does not pretend otherwise (no cross-hay arithmetic is exposed).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Local(pub(crate) usize);

/// What sits immediately outside the HIGH end of a hay. (The low end has no `Edge` value of its
/// own -- see `Hay::low_is_true`'s own doc comment for why restructure R5 (2026-07-28) made the
/// two ends asymmetric types rather than giving `Unreadable` and a witness-checked `True` to
/// both.)
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Edge {
    /// The file's own true end. Assertions decided exactly at this edge are correct by
    /// construction -- there is nothing beyond to read, so no margin is ever needed there.
    /// **Audited, not witness-checked** (R4 review, P3-6, carried forward rather than silently
    /// dropped): every production call site derives this from a real fact already in scope
    /// (`read_pos` or `report_to` reaching `size`, `at_eof`, `hay_reaches_eof`, `self.hi` plus the
    /// carry's own length reaching `size`) before passing it here, but the constructor itself
    /// accepts the bare variant from any caller in the crate -- it
    /// does not, and structurally cannot, verify the claim. Closing that gap fully would mean
    /// requiring `meter::CertifiedEnd` (an actual observed-empty-read witness) at every one of
    /// these sites, and `structural-accept.md` §4's own conclusion still holds: the ordinary,
    /// healthy-source case earns `True` by trusting `BlockCache::size()` directly, with no
    /// contradicting read having happened yet, so there is no witness to demand there without
    /// breaking that whole (correct, load-bearing) path: a `debug_assert` in the constructor
    /// cannot close this gap either, because the thing it would assert against (`size()`) is the
    /// very thing that can lie. What DOES change here: `Edge::Unreadable` (R4's third variant) is
    /// retired -- proven, not merely argued, to have carried zero information any accept
    /// predicate reads, before OR after this restructure (`restr-R4-review.md`'s own mutation
    /// battery, M2-M4: deleting its every effect stayed 512/512 green).
    True,
    /// Real bytes exist beyond this edge and are (or will become) readable; this hay simply does
    /// not hold them (a window cut, a carry shrink, a bounded leg's own `limit`, a viewport row's
    /// own sub-slice of its parent) -- OR bytes beyond were attempted and could not be read (a
    /// `SweepAnalysis::give_up` seam). Assertions within a character's width of this edge are
    /// UNVERIFIABLE against it -- and become verifiable once a later hay reaches further (a Cut)
    /// or never (an Unreadable seam) -- but no accept predicate in this crate distinguishes the
    /// two reasons (the same proof above), so R5 folds them into this one variant: the
    /// progress-vs-verifiability distinction stays available to a FUTURE caller that wants it
    /// (nothing here forecloses reintroducing a witness-bearing progress marker elsewhere), but
    /// costs nothing to carry today for a fact no code reads.
    Cut,
}

/// Whether a run offered to an `Assembly` was actually taken. `#[must_use]` on purpose: a
/// refused run is a real fact about the file (a short read left a hole), and the whole point of
/// this type is that a caller cannot ignore it by accident the way a bare concatenation let it.
/// Most callers legitimately answer `Gap` with "then I do without those bytes" -- that is a
/// `let _ =`, one visible character, not silence.
#[must_use]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Joined {
    /// The run began exactly where the assembly ended (or ended exactly where it began), and is
    /// now part of it.
    Contiguous,
    /// The run did not touch the assembly: a hole sits between them, so it was DROPPED. The
    /// assembly is byte-for-byte what it was before the call.
    Gap,
}

/// **A hay under construction, assembled from runs that each state WHERE THEY ARE**
/// (restructure R7, batch 8 (2026-07-29)). Every hay in this crate is now built through this
/// type, and `tests/no_raw_hay_primitives.rs` is the CI guard that keeps it that way.
///
/// THE DEFECT CLASS THIS RETIRES. Before this, every production `Hay::new` derived its own base
/// by subtracting a LENGTH from a POSITION -- `Abs(pos - buf.len())` in seven different
/// spellings. That arithmetic silently encodes a premise nothing checked: *every byte I hold
/// arrived contiguously, ending at `pos`*. A conforming short read (`BlockSource`'s own "up to
/// `len`" contract, `source.rs`) breaks the premise without breaking anything visible, and the
/// consequence is not a missing byte but a WRONG POSITION -- which then propagates into reported
/// anchors and into the look-behind that `^`/`\b` are decided against. It shipped twice from one
/// function: batch 7 (2026-07-28) finding #2 spliced a `carry` onto a short `slice` (a match
/// assembled from two non-adjacent regions, confirmed against an edge the hay never reached),
/// and batch 8 (2026-07-29) P1 rebased an incomplete look-behind gather as though it sat
/// immediately below the payload (a fabricated line-start anchor). Both fixes were the same
/// shape -- notice the gap, drop the piece -- and both were found by review rather than by the
/// compiler.
///
/// HOW THIS MAKES THE NEXT ONE IMPOSSIBLE. A run is never offered as bare bytes; it is offered
/// together with the absolute position it was READ FROM. Adjacency is then arithmetic this type
/// performs, not a premise the caller states, and a run that does not touch the assembly is
/// refused rather than spliced. The resulting `Hay`'s base is READ from the anchor run, never
/// recomputed -- so there is no subtraction left at any call site to get wrong, and "I have `n`
/// bytes so they must start at `pos - n`" is no longer expressible.
///
/// BUILD OUTWARD FROM THE PAYLOAD. Callers anchor on the run they are actually searching and
/// extend toward the margins (`extend_below` for look-behind, `extend_above` for look-ahead or a
/// carry). This ordering is what makes refusal mean the right thing: the piece dropped is always
/// the outer one, and what survives is always the part adjacent to the payload. Two extensions
/// on the same side compose correctly without extra care -- a refused inner run leaves the base
/// unmoved, so an outer run beyond the same hole is refused in turn.
pub(crate) struct Assembly {
    base: Abs,
    bytes: Vec<u8>,
}

impl Assembly {
    /// The payload run: `bytes`, whose first byte sits at `at`. Every other run in the finished
    /// hay is positioned relative to this one by the file's own coordinates, never by a length.
    pub(crate) fn anchored_at(at: Abs, bytes: &[u8]) -> Assembly {
        Assembly {
            base: at,
            bytes: bytes.to_vec(),
        }
    }
    /// The absolute position of this assembly's first byte -- the finished hay's own base.
    /// Production never reads it back out (it flows into `hay()` directly, which is the point of
    /// the type); this module's own `Assembly` tests use it to pin where a refused run leaves
    /// the base, so it is kept as the natural accessor rather than removed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn base(&self) -> Abs {
        self.base
    }
    /// The absolute position one past this assembly's last byte.
    pub(crate) fn end(&self) -> Abs {
        self.base + self.bytes.len() as u64
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// This assembly's own extent, as an index into the hay it produces -- for a `body_limit` or
    /// an accept end that genuinely means "everything assembled", with no length arithmetic at
    /// the call site to keep in step with it.
    pub(crate) fn len(&self) -> Local {
        Local(self.bytes.len())
    }
    /// Appends `bytes` (read from `at`) iff `at` is exactly where this assembly currently ends.
    /// A carry held over from a previous iteration, a look-ahead peek, a `ctx_after`: each knows
    /// the position it was read from, and that position -- not its length -- is what decides.
    pub(crate) fn extend_above(&mut self, at: Abs, bytes: &[u8]) -> Joined {
        if at != self.end() {
            return Joined::Gap;
        }
        self.bytes.extend_from_slice(bytes);
        Joined::Contiguous
    }
    /// Prepends `bytes` (read from `at`) iff they end exactly where this assembly begins. The
    /// look-behind case: a gather that stopped short leaves its hole at the TOP of what it
    /// collected, so `at + bytes.len()` falls below `base` and the whole run is correctly refused
    /// -- not one byte of it is adjacent to the payload.
    pub(crate) fn extend_below(&mut self, at: Abs, bytes: &[u8]) -> Joined {
        if at + bytes.len() as u64 != self.base {
            return Joined::Gap;
        }
        let mut joined = Vec::with_capacity(bytes.len() + self.bytes.len());
        joined.extend_from_slice(bytes);
        joined.extend_from_slice(&self.bytes);
        self.bytes = joined;
        self.base = at;
        Joined::Contiguous
    }
    /// Where an absolute position lands in the finished hay. This is what accept ranges and body
    /// limits are expressed with now: a caller names the FILE positions its span covers, and this
    /// converts -- so a dropped run shifts every downstream index automatically instead of
    /// requiring the caller to recompute a `lb` in the right order afterwards.
    ///
    /// Clamped rather than panicking (a position outside is pinned to the nearer end, which can
    /// only ever narrow an accept range, never widen one past this assembly's own bytes), with a
    /// `debug_assert` so a caller genuinely confused about its own coordinates still hears about
    /// it in test builds.
    pub(crate) fn local_of(&self, at: Abs) -> Local {
        debug_assert!(
            at >= self.base && at <= self.end(),
            "local_of({at:?}) outside this assembly's own extent {:?}..{:?}",
            self.base,
            self.end()
        );
        Local((at.0.saturating_sub(self.base.0) as usize).min(self.bytes.len()))
    }
    /// The hay over everything assembled, based where the anchor run said it was. Callers still
    /// declare `accept`/`body_limit`/`high` on the result -- this type owns WHERE the bytes are,
    /// not what may be reported from them.
    pub(crate) fn hay(&self) -> Hay<'_> {
        Hay::new(&self.bytes, self.base)
    }
}

/// A windowed byte view with a declared HIGH edge: bytes a caller already holds, where a
/// reported match's START may lie (`accept`), how far its BODY may reach (`body_limit`), and
/// what is known about what lies just past the high end (`high`). No margin is ever a field --
/// every margin below is a method, computed from the four things above plus the two boundary-
/// context constants (`consumable_reach`/`consultable_reach`), so a correction lands once.
///
/// **The low edge is not a field** (restructure R5, 2026-07-28) -- see `low_is_true`'s own doc
/// comment: unlike the high edge, "is this hay's own byte 0 the file's true start" is a fact the
/// hay already holds in full (`base`), so deriving it beats declaring it. An R4-era caller could
/// construct `base != Abs(0)` paired with a hand-set `low: Edge::True` -- wrong, and nothing
/// stopped it (this is precisely the class of bug `restr-R4-review.md` found live in
/// `Document::viewport`'s own low-edge derivation, see that call site's own fix comment). That
/// whole class is unconstructible now: there is no `with_low`, and no field to set it wrong in.
pub(crate) struct Hay<'a> {
    bytes: &'a [u8],
    base: Abs,
    accept: Range<Local>,
    body_limit: Local,
    high: Edge,
}

impl<'a> Hay<'a> {
    /// A fresh hay over the whole of `bytes`: `accept`/`body_limit` default to the full extent
    /// (every consumer either narrows `accept` explicitly or, like `SearchBackward`'s per-
    /// iteration slice, genuinely means the whole thing), `high` defaults to `Cut` (the safe,
    /// unverified default -- a caller must say `True` explicitly, never get it by omission; the
    /// low edge needs no such default, see `low_is_true`).
    pub(crate) fn new(bytes: &'a [u8], base: Abs) -> Hay<'a> {
        Hay {
            bytes,
            base,
            accept: Local(0)..Local(bytes.len()),
            body_limit: Local(bytes.len()),
            high: Edge::Cut,
        }
    }
    pub(crate) fn with_accept(mut self, accept: Range<Local>) -> Hay<'a> {
        self.accept = accept;
        self
    }
    pub(crate) fn with_body_limit(mut self, limit: Local) -> Hay<'a> {
        self.body_limit = limit;
        self
    }
    pub(crate) fn with_high(mut self, e: Edge) -> Hay<'a> {
        self.high = e;
        self
    }
    /// **BOF's axiom, derived, not declared** (restructure R5, 2026-07-28): this hay's own byte 0
    /// is the file's true start iff its `base` -- the absolute file position that byte already
    /// names -- is itself absolute zero. No witness is needed (position zero cannot be truncated
    /// away: there is nothing before it to have gone missing), and no caller can get this wrong,
    /// because no caller SETS it -- every hay derives its own low edge from data it was already
    /// constructed with. This is what closes the exact bug class R4 shipped unfixed: `Document::
    /// viewport`'s own hay used to compute `low = True iff top.offset() == 0`, but `top.offset()`
    /// is the VIEWPORT's own scroll position, not the hay's own `base` -- whenever `top.offset()`
    /// sits strictly below `CTX_BEHIND` with real bytes all the way down to 0 (`edge_context`'s
    /// own behind-side fetch reaches exactly that far there, `ctx_before.len() == top.offset()`),
    /// `base == top.offset() - ctx_before.len() == 0` genuinely -- true BOF -- while the OLD
    /// condition (`top.offset() == 0`) said `Cut`, wrongly denying the BOF axiom to a position
    /// that legitimately owns it. Inert under R4 (nothing consulted `low` for any consumer's own
    /// accept decision except `SweepAnalysis`'s already-correct `self.pos == lb` derivation, which
    /// this method subsumes without changing its answer -- `self.pos == lb` IS `hay.base() == 0`
    /// restated), load-bearing the moment `lookbehind_ok`, below, exists to consult it.
    pub(crate) fn low_is_true(&self) -> bool {
        self.base == Abs(0)
    }

    // ---- coordinates: the only conversions in the crate (structural-accept.md §3.2) ----

    pub(crate) fn to_abs(&self, p: Local) -> Abs {
        self.base + p.0 as u64
    }
    /// `None` when `p` names a position outside this hay's own bytes -- a caller asking about a
    /// position this hay never held, not a bug on its own (a cursor that scrolled above `top`,
    /// e.g. `Document::viewport`'s own `current_match` resolution). No production consumer needs
    /// this yet (every R4 migration site mints its own `Local`s directly from values it already
    /// has, per-consumer) -- kept as the sketch's own declared conversion pair with `to_abs`
    /// (structural-accept.md §3.2: "`Hay` owns the ONLY conversions"), exercised by this module's
    /// own unit tests (`to_abs_and_to_local_round_trip`, below).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn to_local(&self, p: Abs) -> Option<Local> {
        if p.0 < self.base.0 {
            return None;
        }
        let off = p.0 - self.base.0;
        (off as usize <= self.bytes.len()).then_some(Local(off as usize))
    }
    /// No production consumer reads this back out today (each already holds its own bytes
    /// separately, for its own carry/buffer bookkeeping) -- kept as a natural accessor,
    /// exercised by this module's own unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub(crate) fn len(&self) -> Local {
        Local(self.bytes.len())
    }
    pub(crate) fn base(&self) -> Abs {
        self.base
    }

    // ---- shared doctrine: one implementation each (structural-accept.md §3.4) ----

    /// The phantom trailing-newline doctrine, the ONE implementation (six call sites before this
    /// module collapse here): a position sitting exactly at a declared TRUE high edge names no
    /// real line -- and can never be a match, regardless of pattern -- when the byte just before
    /// it is `\n` (`docs/architecture.md`'s own doctrine for `goto_line`, unified across every
    /// search consumer by batch 4 (2026-07-24), finding #2).
    pub(crate) fn phantom_at(&self, p: Local) -> bool {
        self.high == Edge::True && p.0 == self.bytes.len() && self.bytes.last() == Some(&b'\n')
    }
    /// How far a *lookahead* must reach past an accept span for every sub-cap candidate starting
    /// in it to be decidable, INCLUDING its own trailing assertion (`search.rs`'s own top-level
    /// doc comment: `MAX_MATCH_LEN` for the match body, `+ CTX_AHEAD - 1` more so a maximal-
    /// length candidate's own trailing assertion sees a full character, not one byte). The one
    /// place this arithmetic exists now (batch 4 #10, batch 5 #3, and the `±1` fix rounds those
    /// two findings needed -- structural-accept.md's own "merely localized" ledger entry).
    ///
    /// Fix round (R6 review P2-1): returns `Ahead`, not a bare `usize` -- the constant this
    /// formula selects (`CTX_AHEAD`, never `CTX_BEHIND`) is now the one place in the expression
    /// with an explicit type (`margin: crate::search::Ahead`), so swapping it for the wrong
    /// constant is `expected Ahead, found Behind`, not a silent same-value coincidence (both are
    /// numerically 4 today; `.get()`'s own downstream arithmetic cannot tell them apart once
    /// unwrapped, which is why the constant-selection moment, not the arithmetic after it, is
    /// what needs the guard).
    pub(crate) fn consumable_reach() -> crate::search::Ahead {
        let margin: crate::search::Ahead = crate::search::CTX_AHEAD;
        crate::search::Ahead(crate::search::MAX_MATCH_LEN + margin.get() - 1)
    }
    /// How far a *consult-only* peek must reach for a trailing assertion evaluated exactly at
    /// its own edge to see a full character -- narrower than `consumable_reach`: no match BODY
    /// is ever accepted into this reach, only used to decide `^`/`$`/`\b`/`\B` at a boundary
    /// (`SearchForward`'s own F4 peek; `Document::edge_context`'s own AHEAD-side gather).
    ///
    /// Fix round (R6 review P2-1): returns `Ahead`, not a bare `usize` -- this is the reviewer's
    /// own demonstrated hole (`hay.rs:231`, pre-fix: swapping this body's `CTX_AHEAD` for
    /// `CTX_BEHIND` compiled and passed, since both unwrapped to the identical `usize`). The bare
    /// constant is now the function's own return value with no arithmetic to hide behind: the
    /// wrong constant is `expected Ahead, found Behind` at the return position, unconditionally.
    pub(crate) fn consultable_reach() -> crate::search::Ahead {
        crate::search::CTX_AHEAD
    }
    /// **The end rule's look-behind half** (restructure R5, 2026-07-28, `structural-accept.md`
    /// §3.4's own sketch): the look-behind at `s` is adequate -- a leading assertion evaluated
    /// there is decided against real content, never an artificial cut -- iff the whole preceding
    /// character is present (`CTX_BEHIND` real bytes already held before `s`) or `s`'s own hay
    /// sits at the true file start (`low_is_true`, BOF's axiom: nothing precedes position zero,
    /// so no margin is ever owed there regardless of `s`'s own numeric value).
    ///
    /// Fix round (R6 review P2-1): takes `margin: Behind` -- previously this hardcoded
    /// `crate::search::CTX_BEHIND` internally, the reviewer's own sibling hole to `hay.rs:231`'s
    /// (the same file, the same shape, not itself demonstrated but structurally identical). There
    /// is nothing left inside this function to swap; the constant-selection decision moved to
    /// every call site (`verified`, below, and this file's own tests), where passing `CTX_AHEAD`
    /// where `Behind` is expected is `expected Behind, found Ahead`.
    pub(crate) fn lookbehind_ok(&self, s: Local, margin: crate::search::Behind) -> bool {
        s.0 >= margin.get() || self.low_is_true()
    }
    /// **The end rule's look-ahead half.** The look-ahead at `e` is adequate -- a trailing
    /// assertion evaluated there is decided against real content -- iff the whole following
    /// character is present (`CTX_AHEAD` real bytes already held past `e`) or `e` sits at this
    /// hay's own declared true high edge (a real boundary needs no margin).
    ///
    /// Fix round (R6 review P2-1): takes `margin: Ahead` -- this is the reviewer's own
    /// demonstrated hole (`hay.rs:247`, pre-fix: swapping this body's `CTX_AHEAD` for
    /// `CTX_BEHIND` compiled and passed). `lookbehind_ok`'s own doc comment has the shared
    /// rationale.
    pub(crate) fn lookahead_ok(&self, e: Local, margin: crate::search::Ahead) -> bool {
        self.bytes.len() - e.0 >= margin.get() || self.high == Edge::True
    }
    /// **The end rule itself** (restructure R5, 2026-07-28, the unit-J class's own close --
    /// `structural-accept.md` §3.4's `verified()`): a candidate `(s, e)` is REPORTABLE iff its
    /// start is acceptable, its body fits what this hay may ever report on, and every assertion
    /// it can actually carry was decided against real bytes rather than an artificial cut.
    ///
    /// The two `!pat...assertions() ||` guards are the whole of the fix: a pattern that carries
    /// NO leading (respectively trailing) assertion at all needs no look-behind (look-ahead)
    /// margin whatsoever -- its truncated body still has a trustworthy start (a hay cut cannot
    /// MANUFACTURE a body, only fail to complete one), so an assertion-free over-cap candidate
    /// (`a+`, `.*`, `[a-z]+` run past `MAX_MATCH_LEN`) keeps reporting exactly as it always did.
    /// A pattern that DOES carry one pays the identical margin every sub-cap candidate already
    /// paid for free (`structural-accept.md` §3.6's own arithmetic: for a sub-cap match, `s + M +
    /// A <= R` already implies `e + A <= R`, so `lookahead_ok` was ALWAYS true there -- only an
    /// over-cap candidate, where that implication does not hold, is where this predicate newly
    /// disagrees with the old start-only gate). `pat.leading_assertions()`/`trailing_assertions()`
    /// are `SearchPattern::compile`'s own conservative, one-time HIR walk (search.rs) -- "on any
    /// doubt, true," so this predicate can only MISS more than the old one, never fabricate.
    pub(crate) fn verified(&self, pat: &SearchPattern, s: Local, e: Local) -> bool {
        self.accept.contains(&s)
            && e.0 <= self.body_limit.0
            && (!pat.leading_assertions() || self.lookbehind_ok(s, crate::search::CTX_BEHIND))
            && (!pat.trailing_assertions() || self.lookahead_ok(e, crate::search::CTX_AHEAD))
    }
    /// **`verified`'s two END conditions, restated as a single position** (batch 7 (2026-07-28),
    /// findings #1/#3): the highest local offset a candidate's own end may reach and still be
    /// reportable out of this hay. Exactly `e.0 <= body_limit.0 && (!trailing_assertions ||
    /// lookahead_ok(e))`, with nothing added and nothing dropped -- `body_limit` bounds what this
    /// hay may report a BODY over, and the margin term bounds what it may decide a trailing
    /// ASSERTION against, so a candidate satisfies both exactly when its end is at or below the
    /// smaller of the two.
    ///
    /// Restating them as a bound rather than a test is the whole of the fix: a predicate can only
    /// ever answer "not this one" about a candidate the engine already chose, which is why the
    /// rejection path needed a retry loop at all -- and why K2's attempt to make that loop cheap
    /// (stopping at the first undecidable candidate) lost every later candidate behind it,
    /// finding #1. As a BOUND, the same two conditions become something the engine can be asked
    /// to satisfy up front (`raw_find_ending_by`), so the leftmost reportable candidate arrives
    /// in one call with nothing skipped and nothing retried.
    ///
    /// `low_is_true`'s own half deliberately does NOT appear here: the look-BEHIND condition is a
    /// bound on where a match may START, not end, and `leftmost_confirmed` applies it as exactly
    /// that (`seam_floor`), which is why that walk needs no per-candidate retry either.
    /// `None` -- not `Local(0)` -- when the margin is unsatisfiable at any end whatsoever, i.e.
    /// when this hay is itself shorter than `CTX_AHEAD` and its high edge is a cut. **`checked_
    /// sub`, deliberately, and caught by this method's own caller-side `assert!` during batch 7's
    /// own differential** (the first version wrote `saturating_sub`, which floors to `Local(0)`
    /// and thereby ADMITS a zero-width candidate at 0 -- exactly the position `lookahead_ok`
    /// refuses there, since `bytes.len() - 0 >= CTX_AHEAD` is false for a hay that short). A
    /// floor of zero reads as "the tightest possible bound" and is in fact the opposite: `e <= 0`
    /// still admits `e == 0`. "No end can satisfy this" is a different answer from "only the
    /// lowest end can," and the type now says which one it means.
    fn reportable_end(&self, pat: &SearchPattern) -> Option<Local> {
        let by_margin = if pat.trailing_assertions() && self.high != Edge::True {
            // `lookahead_ok(e, CTX_AHEAD)` is `bytes.len() - e >= CTX_AHEAD` once the true-edge
            // disjunct is ruled out above -- i.e. `e <= bytes.len() - CTX_AHEAD`, which has no
            // solution at all when the hay holds fewer than `CTX_AHEAD` bytes.
            self.bytes
                .len()
                .checked_sub(crate::search::CTX_AHEAD.get())?
        } else {
            self.bytes.len()
        };
        Some(Local(self.body_limit.0.min(by_margin)))
    }
    /// The accept span, widened by the one zero-width position at a TRUE high edge -- unless
    /// that position is the trailing newline's own phantom (never a match). The general form of
    /// the "+1 unless phantom" widening every consumer's own terminal report performs (sweep's
    /// per-report EOF widening, forward's truncation-final-pass and in-loop EOF widening).
    pub(crate) fn accept_with_eof_widening(&self) -> Range<Local> {
        let widen = self.high == Edge::True
            && self.accept.end.0 == self.bytes.len()
            && !self.phantom_at(self.accept.end);
        self.accept.start..Local(self.accept.end.0 + usize::from(widen))
    }
    /// **Kept as an observable, no longer as a search bound** (restructure R5, 2026-07-28): the
    /// smallest local start position whose own look-behind would be fully verifiable, exactly
    /// `lookbehind_ok`'s own numeric condition restated as a position rather than a per-candidate
    /// predicate -- the two agree pointwise by construction (`s.0 >= CTX_BEHIND || low_is_true()`
    /// iff `s.0 >= seam_floor().0`, whether or not `low_is_true()` holds: `True` makes both sides
    /// unconditional, `false` makes both sides the identical `CTX_BEHIND` comparison). R4's own
    /// `SweepAnalysis` used this as a hard search FLOOR (never even offering a below-floor start
    /// to the regex engine); R5 retired that -- flooring the search unconditionally would ALSO
    /// exclude an assertion-free candidate that needs no look-behind margin at all (the exact
    /// class the F9 seam-cost retirement recovers), so `verified()`'s own `lookbehind_ok` gate,
    /// applied PER CANDIDATE with the pattern's own leading-assertion fact in hand, replaced it
    /// there. What remains: the R4-reviewer-prescribed re-pointing target for the protected
    /// white-box test that used to inspect `SweepAnalysis::seam_gap_remaining` directly (deleted
    /// this restructure, proven inert by R4's own mutation battery, M1-M4) -- a hay built at a
    /// give_up landing's own position still names its own excluded floor here, observably,
    /// without needing a persisted counter to ask.
    /// Fix round (R6 review P2-1): the `CTX_BEHIND` selection is an explicitly `Behind`-typed
    /// local (`margin`), the same guard `lookbehind_ok`/`consultable_reach` now use -- not named
    /// among the reviewer's own demonstrated holes, but the identical shape (a bare, unarithmetic
    /// reference to one of the two numerically-equal constants) and in the same file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn seam_floor(&self) -> Local {
        if self.low_is_true() {
            self.accept.start
        } else {
            let margin: crate::search::Behind = crate::search::CTX_BEHIND;
            Local(self.accept.start.0.max(margin.get()))
        }
    }
    /// A sub-view over a NARROWER byte window (never the whole parent -- `structural-accept.md`
    /// §6's own performance caveat: a per-row search must not re-scan the whole parent hay, or
    /// the viewport regresses the cost batch 4 #5 fixed). Both `window` and `accept` are given in
    /// THIS hay's own `Local` coordinates; `accept` must be `window`'s own sub-range. The low edge
    /// needs no inheritance logic of its own (restructure R5) -- `low_is_true` re-derives itself
    /// from the sub-hay's own `base`, which is already correct by construction (`to_abs(window.
    /// start)`: zero exactly when `window.start` reaches all the way back to THIS hay's own true
    /// BOF, whatever `window` turns out to be). `high` is INHERITED only when `window`'s own high
    /// bound coincides with this hay's own bound (a real file boundary carries through the
    /// narrowing); otherwise `Edge::Cut` -- an INTERIOR cut is never a true boundary, regardless
    /// of what the wider hay's own edge was. `body_limit` inherits (rebased, clamped to this
    /// sub-hay's own extent): a match may still extend past `window`'s own end into the parent's
    /// own remaining `body_limit` reach ONLY if that reach is itself still inside `window` --
    /// otherwise it is simply not visible to this sub-view at all, which is exactly the narrowing
    /// this method exists to enforce.
    pub(crate) fn subhay(&self, window: Range<Local>, accept: Range<Local>) -> Hay<'a> {
        debug_assert!(
            window.start.0 <= accept.start.0
                && accept.start.0 <= accept.end.0
                && accept.end.0 <= window.end.0
                && window.end.0 <= self.bytes.len(),
            "subhay: accept must nest inside window, which must nest inside this hay"
        );
        let high = if window.end.0 == self.bytes.len() {
            self.high
        } else {
            Edge::Cut
        };
        let body_limit = Local(
            self.body_limit
                .0
                .min(window.end.0)
                .saturating_sub(window.start.0),
        );
        Hay {
            bytes: &self.bytes[window.start.0..window.end.0],
            base: self.to_abs(window.start),
            accept: Local(accept.start.0 - window.start.0)..Local(accept.end.0 - window.start.0),
            body_limit,
            high,
        }
    }

    // ---- raw primitives: the ONLY callers of SearchPattern's own windowed methods ----

    fn raw_find_from(
        &self,
        pattern: &SearchPattern,
        at: Local,
        end: Local,
    ) -> Option<(Local, Local)> {
        #[cfg(test)]
        RAW_FIND_FROM_CALLS.with(|c| c.set(c.get() + 1));
        pattern
            .find_starting_in(self.bytes, at.0..end.0)
            .map(|(s, e)| (Local(s), Local(e)))
    }
    /// The end-bounded twin of `raw_find_from` (batch 7 (2026-07-28), findings #1/#3): the
    /// leftmost match starting at or after `span.start` whose own END does not exceed `span.end`,
    /// with every look-around assertion still decided against the WHOLE hay -- the bytes above
    /// `span.end` stay real context, they are simply not eligible to be part of the match.
    /// `SearchPattern::find_ending_by`'s own doc comment has the "this is not slicing" argument
    /// and the probe that pins it.
    fn raw_find_ending_by(
        &self,
        pattern: &SearchPattern,
        span: Range<Local>,
    ) -> Option<(Local, Local)> {
        #[cfg(test)]
        RAW_FIND_FROM_CALLS.with(|c| c.set(c.get() + 1));
        pattern
            .find_ending_by(self.bytes, span.start.0..span.end.0)
            .map(|(s, e)| (Local(s), Local(e)))
    }
    fn raw_find_all(&self, pattern: &SearchPattern, range: Range<Local>) -> Vec<(Local, Local)> {
        pattern
            .find_all_starting_in(self.bytes, range.start.0..range.end.0)
            .into_iter()
            .map(|(s, e)| (Local(s), Local(e)))
            .collect()
    }

    // ---- per-consumer enumeration: today's semantics, one entry point per shape ----
    //
    // Argued, not forced into one predicate (the brief's own instruction, `structural-accept.md`
    // §3's own "a consumer-behavior enum or per-consumer entry points" allowance): the nine sites
    // this module replaces apply FOUR genuinely different end conditions today (table 1a of
    // `structural-accept.md` §1a) -- none of it is a lie about today, all of it is the honest
    // shape R5 will edit in one place instead of nine.

    /// A6/A7's shape (`SearchBackward`): the rightmost VERIFIED match whose start lies in
    /// `accept` -- no end-containment condition of its own beyond what `verified` itself checks
    /// (table 1a: "end gate -- none, `_e` discarded" was R4's today; R5 gives backward its first
    /// real end condition here, per `structural-accept.md` §3.5: "this is where it stops being a
    /// unit-J site"). `first_verified` is this method's leftmost twin (A5's shape: `SearchForward`'s
    /// forced truncation-final pass, and A2/A4/A7's shared zero-width point probe below).
    ///
    /// A plain `raw_rfind` + filter (R4's own shape) is WRONG once a per-candidate condition can
    /// reject the rightmost raw match while an EARLIER one would still verify: filtering an
    /// `Option` cannot fall back to a second-best candidate, it can only discard the one it was
    /// given. This walks every raw candidate left to right (`starts_verified`'s own cadence),
    /// keeping the LAST one that verifies rather than the last one found, full stop.
    pub(crate) fn last_verified(&self, pattern: &SearchPattern) -> Option<(Local, Local)> {
        let mut best = None;
        let mut at = self.accept.start;
        while let Some((s, e)) = self.raw_find_from(pattern, at, self.accept.end) {
            if self.verified(pattern, s, e) {
                best = Some((s, e));
            }
            at = Local(s.0 + 1);
        }
        best
    }
    /// The leftmost VERIFIED match whose start lies in `accept` -- retries past a raw candidate
    /// that fails `verified` (an over-cap body, or an assertion decided against an artificial
    /// cut) the same way `leftmost_confirmed` retries past a `body_limit` overrun: a rejected
    /// candidate does not end the search, it just is not THIS one.
    pub(crate) fn first_verified(&self, pattern: &SearchPattern) -> Option<(Local, Local)> {
        let mut at = self.accept.start;
        while let Some((s, e)) = self.raw_find_from(pattern, at, self.accept.end) {
            if self.verified(pattern, s, e) {
                return Some((s, e));
            }
            at = Local(s.0 + 1);
        }
        None
    }
    /// A2/A4/A7's shared shape: a single-position probe for a zero-width match sitting exactly
    /// at this hay's own TRUE high edge (`self.high == Edge::True` is a precondition the caller
    /// establishes, not inferred here -- a hay built with a `Cut` high edge simply never admits
    /// anything through this method, the safe default). Every call site probes a pure
    /// look-behind buffer (`lb == bytes.len()` there, always, by construction: nothing has been
    /// read past the probed position yet) for a zero-width start at that buffer's own end --
    /// structurally identical regardless of which consumer is asking. The trailing side needs no
    /// separate gate here: `high == Edge::True` already IS `lookahead_ok`'s own unconditional
    /// case.
    ///
    /// **The look-behind check is load-bearing for A2 and for every backward endpoint probe**
    /// (batch 22 (2026-08-01) -- this paragraph used to say "exactly ONE of the three callers",
    /// counted when A7 was a single wrap-leg check; batches 14-19 turned the backward side into
    /// three probe positions on ORDINARY legs as well, and `scan.rs`'s own
    /// `endpoint_match_reading_lookbehind` exists precisely because this gate refused them):
    /// - A2 (`SweepAnalysis`'s own `pos == size` probe, reachable right after `give_up`) can be
    ///   handed an EMPTY carry, which is the fixture this method's own doc comment below has.
    /// - The backward endpoint probes reach positions no read has landed below -- a leg that
    ///   DESCENDED to a certified end, or an accurately sized file's terminal position that the
    ///   payload loop never visits. Bare assemblies answered only at BOF there, a demonstrated
    ///   miss (`$` at the real end of `b"a"` with the leg floored at 1), so those call sites now
    ///   GATHER their look-behind before asking rather than being exempt from the gate.
    /// - A4 (`SearchForward`'s entry check) is the one that still never reaches an inadequate
    ///   margin without ALSO being at genuine BOF (no `give_up`-style seam; it either holds the
    ///   full `CTX_BEHIND` real bytes or the file itself is that short), so for A4 alone
    ///   `lookbehind_ok` is a defensive backstop -- kept for the same reason `viewport_belt`'s own
    ///   analogous gate is kept where IT is currently a no-op (that method's own doc comment
    ///   cross-references this one).
    ///
    /// **The look-behind check is UNCONDITIONAL here -- fix round (2026-07-28), P2-1, corrected
    /// from restructure R5's own first attempt, which gated this on `pattern.leading_assertions()`
    /// the same way `verified`'s ordinary candidates are gated.** That reasoning does not transfer:
    /// `verified`'s own "a hay cut cannot manufacture a body" argument is about a candidate's own
    /// BODY being real, already-observed bytes regardless of what the pattern demands of them --
    /// it says nothing about whether the START POSITION ITSELF is a legitimate place to report a
    /// match at all. A zero-width probe has no body (its start and end coincide), so the question
    /// this method answers is purely positional: is `p` verified NOT to be the trailing newline's
    /// own phantom (`phantom_at`, immediately above) -- and `phantom_at` can only answer that when
    /// it can actually SEE the byte immediately before `p`. Right after `SweepAnalysis::give_up`
    /// lands its skip exactly at the true end, `self.bytes` (the hay's own carry) is EMPTY --
    /// `phantom_at` has no byte to inspect and returns `false` by default, which reads as "verified
    /// NOT a phantom" but actually means "unable to verify" (the identical conflation the "no
    /// sentinel" doctrine, `search.rs`'s own top-level doc comment, already names for assertion
    /// verification -- this is the SAME hazard, one layer up, for position/phantom verification).
    /// Gating on `leading_assertions()` let an assertion-FREE pattern (`x*`, `q*`, a bare `(?:)`,
    /// `x?` -- all zero-width-capable with no `Look` node at all) skip this check entirely,
    /// reporting a zero-width match at a position whose own phantom-status is genuinely unknown --
    /// review-measured: 0 → 1 on the committed `sweep_analysis_seam_gap_landing_exactly_at_size_
    /// excludes_zero_width_matches` fixture (`x*` at a `give_up` seam landing exactly at `size`,
    /// real unread predecessor `\n`), a phantom overcount 05a2a2e did not have, reopening the exact
    /// class this whole restructure exists to close. `lookbehind_ok(p)` -- true at genuine BOF
    /// (nothing precedes position zero, so nothing can be an unverified phantom either) or once
    /// `CTX_BEHIND` real bytes are held (enough to know for certain the byte at `p - 1` is not
    /// `\n`) -- is the general-purpose "is there enough verified context here" fact this position
    /// question needs, applied regardless of the pattern, exactly as it was before assertion-
    /// awareness existed to (wrongly) suggest gating it.
    pub(crate) fn zero_width_at_high(&self, pattern: &SearchPattern) -> bool {
        let p = Local(self.bytes.len());
        self.high == Edge::True
            && self.lookbehind_ok(p, crate::search::CTX_BEHIND)
            && !self.phantom_at(p)
            && self
                .raw_find_from(pattern, p, Local(p.0 + 1))
                .is_some_and(|(_, e)| e.0 <= self.body_limit.0)
    }
    /// A1's shape (`SweepAnalysis`): every VERIFIED start position in `accept` (not just
    /// non-overlapping matches -- `structural-accept.md` §3.5's own "the two counting walks stay
    /// two"). Returns starts only; `SweepAnalysis::record_match` never used any match's own end.
    ///
    /// **The F9 seam cost retires here** (restructure R5, 2026-07-28): R4 floored this walk's own
    /// starting point at `seam_floor()`, excluding every below-floor position from the search
    /// OUTRIGHT, regardless of whether the pattern being searched carries any assertion that
    /// look-behind adequacy could even matter to. That was too wide a brush -- a bare literal
    /// (`"needle"`, no assertion at all) starting one byte past a `give_up` seam has a
    /// trustworthy start with or without a verified predecessor (a hay cut cannot MANUFACTURE a
    /// body), so excluding it was a needless miss, not a required one. The walk now starts at
    /// `accept.start` unconditionally and lets `verified`'s own `lookbehind_ok` gate -- itself
    /// gated on `pattern.leading_assertions()` -- decide per candidate: an assertion-bearing
    /// pattern (`(?u:\b)needle`) is excluded exactly as before (the overcount test this seam
    /// exists to prevent is unmoved); an assertion-free one is not.
    pub(crate) fn starts_verified(&self, pattern: &SearchPattern) -> Vec<Local> {
        let mut found = Vec::new();
        let mut at = self.accept.start;
        while let Some((s, e)) = self.raw_find_from(pattern, at, self.accept.end) {
            if self.verified(pattern, s, e) {
                found.push(s);
            }
            at = Local(s.0 + 1);
        }
        found
    }
    /// A3's shape (`SearchForward`'s main loop): the leftmost REPORTABLE candidate -- the
    /// leftmost `(s, e)` with `s` in `accept` and `verified(s, e)` true -- delivered by asking the
    /// engine for exactly that, in ONE call, instead of proposing candidates and rejecting them
    /// one at a time. Reported as `Found` when `s` already precedes `explored_to` (every sub-cap
    /// alternative starting there has had its fair chance to complete) or `Pending` otherwise (a
    /// real candidate, just not yet safe -- more bytes may still surface an earlier alternative).
    /// `Exhausted` when no reportable candidate exists in `accept` at all.
    ///
    /// **The two bounds, and why they are bounds** (batch 7 (2026-07-28), findings #1/#3, and the
    /// retirement of the whole `CutAdvances`/`Deferred` apparatus K2 built here):
    ///
    /// - the END bound is `reportable_end` (that method's own doc comment derives it): `verified`'s
    ///   `body_limit` and trailing-margin conditions, restated as the highest end this hay may
    ///   report, handed to `raw_find_ending_by` so the engine returns a candidate that ALREADY
    ///   satisfies both. Look-around is still decided against the whole hay, so the bytes above the
    ///   bound remain real context for a trailing assertion -- they are merely not eligible to be
    ///   part of the match.
    /// - the START bound is `seam_floor`, applied only when the pattern can carry a leading
    ///   assertion (`verified`'s own gate, unchanged): the lowest start whose look-behind is
    ///   decidable. R5 retired this as an UNCONDITIONAL floor for good reason -- it excluded
    ///   assertion-free candidates that need no look-behind at all -- and that retirement stands;
    ///   gating it on `leading_assertions()` reproduces `verified`'s per-candidate answer exactly
    ///   (`s.0 >= CTX_BEHIND || low_is_true()` iff `s.0 >= seam_floor().0`, `seam_floor`'s own
    ///   doc comment) while needing no retry to discover it.
    ///
    /// With both conditions expressed as bounds, this method has no loop at all -- which is the
    /// point, and is what makes the two defects it closes unrepresentable rather than fixed:
    ///
    /// **Finding #1 (a deferral discarded later valid matches).** K2 made a trailing-margin
    /// rejection at an advancing cut return `Deferred(s)` IMMEDIATELY, on the argument that the
    /// caller "re-runs this WHOLE walk from `accept.start` again once more bytes arrive, so
    /// nothing is skipped, only deferred." That argument was FALSE, and the falseness was not in
    /// this file: `SearchForward::step` shrinks its carry to the last `consumable_reach()` bytes
    /// before the next call, so the next walk's `accept.start` is thousands of bytes HIGHER, and
    /// every candidate between the old start and the new one was discarded unexamined. A pattern
    /// whose first alternative is a long greedy run (`A[A-Za-y]*(?u:\b)|needle`) therefore hid an
    /// ordinary sub-cap literal sitting inside that run: reviewer-measured, `Exhausted` against a
    /// whole-file oracle of `(5000, 5006)`. Deferral is gone, so there is nothing to stop early
    /// at: a later candidate cannot be skipped by a walk that never proposes a rejected one.
    ///
    /// **Finding #3 (an uninterruptible quadratic at the final boundary).** The `Final` arm --
    /// which K2 deliberately left retrying at `s + 1`, because a `Deferred` nothing would ever
    /// resolve is worse than a retry -- re-scanned the whole remaining hay per start position:
    /// 0.77 s / 2.62 s / 9.71 s at 20k/40k/80k bytes (reviewer-measured; independently reproduced
    /// at 843/2890/10829 ms), synchronously, with no await, charge or cancellation inside it, and
    /// bounded only by `take <= block_size` -- 1 MiB by default. There is no retry loop left on
    /// either path, so there is no arm to leave un-fixed and no caller-supplied fact
    /// (`CutAdvances`) that could be passed wrong at one of two call sites -- the exact hazard K2
    /// needed two scan-level pins to guard, now unrepresentable instead of guarded.
    ///
    /// The `advancing` parameter, `Candidate::Deferred`, and `trailing_margin_only_rejection` all
    /// retire here. What survives unchanged is `verified` itself -- still the end rule, still the
    /// thing this method's own result satisfies, now asserted rather than tested-then-rejected.
    pub(crate) fn leftmost_confirmed(
        &self,
        pattern: &SearchPattern,
        explored_to: Local,
    ) -> Candidate {
        let start = if pattern.leading_assertions() {
            self.seam_floor()
        } else {
            self.accept.start
        };
        let Some(end) = self.reportable_end(pattern) else {
            return Candidate::Exhausted;
        };
        let Some((s, e)) = self.raw_find_ending_by(pattern, start..end) else {
            return Candidate::Exhausted;
        };
        // the engine bounds where a match may START only from below; a hay whose `accept` ends
        // before its own bytes do (a bounded leg's `limit`, a viewport row) can still be handed a
        // candidate starting past it. Nothing further left exists -- the answer was leftmost --
        // so this is `Exhausted`, exactly as `find_starting_in`'s own identical guard concludes.
        if !self.accept.contains(&s) {
            return Candidate::Exhausted;
        }
        // `assert!`, never `debug_assert!` (R6's own keeper: this workspace's release test profile
        // makes `debug_assert!` a no-op, so every pin here is a plain assert). This is the whole
        // equivalence claim of batch 7's rewrite in one line -- the bounds handed to the engine
        // admit exactly what `verified` admits -- and it runs on every production search, on both
        // call sites, for every candidate either one returns.
        assert!(
            self.verified(pattern, s, e),
            "the start/end bounds handed to the engine must admit exactly what `verified` does"
        );
        if s.0 < explored_to.0 {
            Candidate::Found(s, e)
        } else {
            Candidate::Pending(s)
        }
    }

    // ---- the viewport's own landed belt (batch 5 (2026-07-26), findings #6/#7) ----
    //
    // Restructure R5 (2026-07-28): now assertion-aware (`structural-accept.md` §3.4's
    // `lookahead_ok`/`lookbehind_ok`, each gated by the pattern's own `trailing_assertions`/
    // `leading_assertions`), replacing the UNCONDITIONAL margin `document.rs`'s pre-R4
    // `point_candidate_is_resolved` applied to every candidate regardless of whether it carried
    // an assertion at all. `point_candidate_is_resolved` itself stays FROZEN (`document.rs`'s own
    // doc comment: it is R4's own migration referee, not a living spec) -- the two are no longer
    // expected to agree, and are not cross-checked against each other past this point (see the
    // retired `hay_viewport_belt_agrees_with_the_frozen_point_candidate_is_resolved` test, this
    // module's own movement table entry).

    /// A candidate is resolved iff every assertion it can actually carry was decided against real
    /// bytes -- `lookahead_ok`/`lookbehind_ok`, each waived when the pattern carries no such
    /// assertion at all (an assertion-free over-cap match paints again: H's own P1-1 paint pins,
    /// direction RECOVERY) -- AND its own start is not `buf_hi` (the payload/ctx_after boundary --
    /// a position `current_match`'s own point-probe can legitimately land on) unless a genuine,
    /// non-phantom true-EOF zero-width match is admissible there (`eof_zero_width_ok`, computed
    /// once per `viewport` call and threaded through). `buf_hi` is a viewport-specific reference
    /// point (`buf`'s own edge, not necessarily this hay's own `bytes.len()` -- `ctx_after` can
    /// extend past it), so it stays an explicit parameter rather than folding into `body_limit`/
    /// `phantom_at`, which reason about THIS hay's own physical edge instead.
    ///
    /// **`lookbehind_ok`'s own gate is defensive here, not load-bearing in production** (fix
    /// round (2026-07-28), P3-5, reviewed and confirmed): `Document::edge_context`'s own
    /// behind-side fetch gets `ctx_before.len() == min(CTX_BEHIND, top.offset())`, and
    /// `accept_and_hay_for_row`'s own windowing backs a row's hay off by `MAX_MATCH_LEN` and then
    /// `CTX_BEHIND` besides -- so every candidate this method is ever asked about already has
    /// either `s.0 >= CTX_BEHIND` (row-local) or sits in a hay whose own `base` is genuinely
    /// `Abs(0)`, making `lookbehind_ok` trivially true either way. Kept for uniformity with
    /// `verified`'s own shape (every OTHER end-rule consumer checks both sides) and as a real
    /// backstop should the viewport's own geometry ever change to narrow that margin -- NOT the
    /// same situation `zero_width_at_high`'s own look-behind check is in: that one is genuinely
    /// load-bearing (for `SweepAnalysis`'s own give_up seam, A2, fixed this same round, P2-1),
    /// just not for every one of its three callers (A4/A7 are the no-ops there, per that method's
    /// own doc comment).
    pub(crate) fn viewport_belt(
        &self,
        pattern: &SearchPattern,
        s: Local,
        e: Local,
        buf_hi: Local,
        eof_zero_width_ok: bool,
    ) -> bool {
        (!pattern.trailing_assertions() || self.lookahead_ok(e, crate::search::CTX_AHEAD))
            && (!pattern.leading_assertions() || self.lookbehind_ok(s, crate::search::CTX_BEHIND))
            && (s != buf_hi || eof_zero_width_ok)
    }
    /// A8's shape (`Document::viewport`'s own `current_match` point query): is there a match
    /// starting exactly at `at`, resolved under today's viewport belt?
    pub(crate) fn point_verified(
        &self,
        pattern: &SearchPattern,
        at: Local,
        buf_hi: Local,
        eof_zero_width_ok: bool,
    ) -> Option<(Local, Local)> {
        self.raw_find_from(pattern, at, Local(at.0 + 1))
            .filter(|&(s, e)| self.viewport_belt(pattern, s, e, buf_hi, eof_zero_width_ok))
    }
    /// A9's shape (`visible_window_matches`): every non-overlapping match reachable through this
    /// row's own (already narrowed, via `subhay`) hay. Searches `accept.start..body_limit` -- the
    /// row's own FULL reach, not narrowed to `accept.end` (batch 4 finding #5's own multiline-
    /// reach rule: a match starting in this row's own accept range but completing past it, inside
    /// real trailing content, is genuine and must still be found) -- then filters down to starts
    /// within `accept`, ends reaching at least `win_start` (this row's own visible left edge, in
    /// this hay's own `Local` coordinates), and today's viewport belt.
    pub(crate) fn all_verified_row(
        &self,
        pattern: &SearchPattern,
        win_start: Local,
        buf_hi: Local,
        eof_zero_width_ok: bool,
    ) -> Vec<(Local, Local)> {
        if self.accept.start.0 >= self.body_limit.0 {
            return Vec::new();
        }
        self.raw_find_all(pattern, self.accept.start..self.body_limit)
            .into_iter()
            .filter(|&(s, e)| {
                s.0 < self.accept.end.0
                    && e.0 >= win_start.0
                    && self.viewport_belt(pattern, s, e, buf_hi, eof_zero_width_ok)
            })
            .collect()
    }
}

/// `Hay::leftmost_confirmed`'s own outcome -- see that method's doc comment.
///
/// **`Deferred` retired here (batch 7 (2026-07-28), finding #1).** K2 added it as a deliberate
/// third state -- a candidate that FAILED `verified` on a trailing margin at an advancing cut,
/// distinguished from `Pending` because the two need different caller-side `last_nl` treatment --
/// and that distinction was correct for the walk that produced it. The walk is gone:
/// `leftmost_confirmed` now asks the engine for a candidate that already satisfies `verified`,
/// so a rejected candidate is never returned at all and there is no third state to describe one.
/// `Pending(s)` keeps its full original meaning (a candidate that PASSED `verified` and sits at
/// or past `explored_to`, so a caller may safely stop its own newline bookkeeping at `s`), and
/// the whole-hay treatment `Deferred` shared with `Exhausted` goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Candidate {
    Found(Local, Local),
    Pending(Local),
    Exhausted,
}

// batch 6 (2026-07-28), finding #3's own structural RED instrument (`AGENTS.md`'s own rule: a
// test proves a quadratic by counting invocations, never by timing one) -- counts calls to
// `raw_find_from` on the CURRENT thread. `thread_local!`, not a crate-wide `AtomicUsize`: cargo
// runs tests on multiple threads by default, and a shared global would let one test's count leak
// into another's running concurrently; each test resets it (`reset_raw_find_from_calls`) before
// driving the walk it means to measure. `cfg(test)`-only: production code never reads this, so
// it cannot become a second implicit contract the way a timing assertion would.
//
// `pub(crate)`, not module-private (K2 fix round, P1-2): `scan.rs`'s own tests need this same
// counter for the M-A scan-level pin -- a fixture driven through the REAL `SearchForward::step`
// (not through `Hay` directly, the gap the fix round found: a `Hay`-level RED can never observe
// what `step` itself derives and passes for `advancing`).
#[cfg(test)]
thread_local! {
    static RAW_FIND_FROM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn reset_raw_find_from_calls() {
    RAW_FIND_FROM_CALLS.with(|c| c.set(0));
}
#[cfg(test)]
pub(crate) fn raw_find_from_calls() -> usize {
    RAW_FIND_FROM_CALLS.with(|c| c.get())
}
/// Restructure R4 (2026-07-27): a unit module ON TOP of the migrated consumers' own protected
/// suites (`restructure-plan.md`'s own "Refereeing" rule -- every existing behavior-named test
/// stays, `Hay` gets a new module here, not a replacement for any of them). Exercises the type
/// in isolation, without a `Document`/`BlockCache`/tokio runtime -- the same testability argument
/// `document.rs`'s own `accept_and_hay_for_row` unit tests already established for the pre-R4
/// arithmetic this module now centralizes.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchPattern;

    fn pat(raw: &str) -> SearchPattern {
        SearchPattern::compile(raw, false).unwrap()
    }

    // ---- coordinates ----

    #[test]
    fn to_abs_and_to_local_round_trip() {
        let hay = Hay::new(b"hello", Abs(100));
        assert_eq!(hay.to_abs(Local(0)), Abs(100));
        assert_eq!(hay.to_abs(Local(5)), Abs(105));
        assert_eq!(hay.to_local(Abs(100)), Some(Local(0)));
        assert_eq!(
            hay.to_local(Abs(105)),
            Some(Local(5)),
            "one past the last byte is in range"
        );
        assert_eq!(
            hay.to_local(Abs(106)),
            None,
            "two past the last byte is genuinely outside this hay"
        );
        assert_eq!(
            hay.to_local(Abs(99)),
            None,
            "a position below this hay's own base is outside it"
        );
    }

    #[test]
    fn bytes_and_len_reflect_construction() {
        let hay = Hay::new(b"hello", Abs(0));
        assert_eq!(hay.bytes(), b"hello");
        assert_eq!(hay.len(), Local(5));
        assert_eq!(hay.base(), Abs(0));
    }

    // ---- phantom_at ----

    #[test]
    fn phantom_at_requires_a_true_high_edge_the_exact_position_and_a_real_newline() {
        let hay = Hay::new(b"foo\n", Abs(0)).with_high(Edge::True);
        assert!(
            hay.phantom_at(Local(4)),
            "true high edge, at bytes.len(), last byte is \\n"
        );
        assert!(
            !hay.phantom_at(Local(3)),
            "not at bytes.len() -- the byte at 3 is \\n but this position is not the edge"
        );
        let not_newline = Hay::new(b"food", Abs(0)).with_high(Edge::True);
        assert!(!not_newline.phantom_at(Local(4)), "last byte is not \\n");
        let cut = Hay::new(b"foo\n", Abs(0)).with_high(Edge::Cut);
        assert!(
            !cut.phantom_at(Local(4)),
            "high is not True -- an artificial cut, not real EOF"
        );
    }

    // ---- Assembly (restructure R7, batch 8 (2026-07-29)) ----

    #[test]
    fn assembly_takes_runs_that_touch_and_refuses_runs_that_do_not() {
        // the whole type in one test: a run offered at the position it was read from either
        // abuts what is already held or it does not, and "does not" means DROPPED, with the
        // assembly byte-for-byte unchanged.
        let mut asm = Assembly::anchored_at(Abs(10), b"payload"); // [10, 17)
        assert_eq!(asm.base(), Abs(10));
        assert_eq!(asm.end(), Abs(17));

        // below: ends exactly at 10 -- taken, and the base moves down to where it began.
        assert_eq!(asm.extend_below(Abs(6), b"abcd"), Joined::Contiguous);
        assert_eq!(asm.base(), Abs(6));
        assert_eq!(asm.bytes(), b"abcdpayload");

        // above: starts exactly at 17 -- taken.
        assert_eq!(asm.extend_above(Abs(17), b"XY"), Joined::Contiguous);
        assert_eq!(asm.end(), Abs(19));
        assert_eq!(asm.bytes(), b"abcdpayloadXY");

        // a hole on either side -- refused, and nothing moves.
        let before = asm.bytes().to_vec();
        assert_eq!(
            asm.extend_below(Abs(0), b"zz"),
            Joined::Gap,
            "ends at 2, base is 6"
        );
        assert_eq!(
            asm.extend_above(Abs(20), b"zz"),
            Joined::Gap,
            "starts at 20, end is 19"
        );
        assert_eq!(
            asm.bytes(),
            &before[..],
            "a refused run leaves the assembly untouched"
        );
        assert_eq!((asm.base(), asm.end()), (Abs(6), Abs(19)));
    }

    #[test]
    fn assembly_refuses_a_look_behind_gather_that_stopped_short() {
        // batch 8 (2026-07-29), P1, as arithmetic. `SearchBackward`'s look-behind walk ASCENDS
        // toward the payload, so a conforming short read leaves its hole at the TOP of the run:
        // the bytes are real and contiguous among themselves, they simply do not reach. The old
        // `Abs(lo - ctx.len())` base could not tell that apart from a complete gather -- it
        // asserted adjacency by construction -- and reported a fabricated line-start anchor.
        //
        // Payload at 8; the walk wanted [4, 8) but a short block answered only [4, 7).
        let mut asm = Assembly::anchored_at(Abs(8), b"defWORLD");
        assert_eq!(
            asm.extend_below(Abs(4), b"\nab"),
            Joined::Gap,
            "4 + 3 = 7, not 8 -- position 7 was never delivered, so not one of these bytes is \
             adjacent to the payload"
        );
        assert_eq!(
            asm.base(),
            Abs(8),
            "refusal leaves the payload's own base intact"
        );
        assert_eq!(asm.bytes(), b"defWORLD");
        // and the same run, complete, is taken -- the check is about the HOLE, not about the
        // run being short of the full CTX_BEHIND width.
        let mut complete = Assembly::anchored_at(Abs(8), b"defWORLD");
        assert_eq!(complete.extend_below(Abs(5), b"abc"), Joined::Contiguous);
        assert_eq!(complete.base(), Abs(5));
    }

    #[test]
    fn assembly_refuses_a_carry_left_stranded_by_a_short_read() {
        // batch 7 (2026-07-28), finding #2, as arithmetic -- the high-side twin of the test
        // above. A carry holds bytes from `self.hi` upward; when a short read leaves the payload
        // stopping below `self.hi`, the two are not adjacent and concatenating them assembles a
        // match out of bytes that were never read as neighbours.
        //
        // Payload read at 8 wanted [8, 16) but delivered only [8, 13); the carry sits at 16.
        let mut asm = Assembly::anchored_at(Abs(8), b"hello");
        assert_eq!(
            asm.extend_above(Abs(16), b"WORLD!!!"),
            Joined::Gap,
            "the assembly ends at 13 and the carry begins at 16 -- [13, 16) was never delivered"
        );
        assert_eq!(
            asm.bytes(),
            b"hello",
            "no splice: 'helloWORLD!!!' exists nowhere in the file"
        );
    }

    #[test]
    fn assembly_local_of_absorbs_a_refused_run() {
        // why accept ranges are named in FILE coordinates now: the local index of a given file
        // position depends on whether the run below it joined, and `local_of` is what makes that
        // automatic instead of a `lb` the caller has to recompute in the right order afterwards.
        let mut joined = Assembly::anchored_at(Abs(8), b"defWORLD");
        assert_eq!(joined.extend_below(Abs(4), b"abcd"), Joined::Contiguous);
        assert_eq!(
            joined.local_of(Abs(8)),
            Local(4),
            "4 ctx bytes precede the payload"
        );

        let mut refused = Assembly::anchored_at(Abs(8), b"defWORLD");
        assert_eq!(refused.extend_below(Abs(4), b"\nab"), Joined::Gap);
        assert_eq!(
            refused.local_of(Abs(8)),
            Local(0),
            "the same file position, with the ctx dropped, is now the hay's own first byte -- \
             the accept range shifts with it and the caller writes the same expression either way"
        );
    }

    #[test]
    fn assembly_hay_is_based_where_the_anchor_run_said_it_was() {
        // the base is READ, never recomputed: `low_is_true` (BOF's axiom) follows from it, so an
        // assembly that genuinely reaches position 0 earns it and one that does not cannot.
        let mut at_bof = Assembly::anchored_at(Abs(4), b"payload");
        assert_eq!(at_bof.extend_below(Abs(0), b"abcd"), Joined::Contiguous);
        assert!(
            at_bof.hay().low_is_true(),
            "the assembly genuinely reaches position 0"
        );

        let mut refused = Assembly::anchored_at(Abs(4), b"payload");
        assert_eq!(refused.extend_below(Abs(0), b"ab"), Joined::Gap);
        assert!(
            !refused.hay().low_is_true(),
            "a gap cannot fabricate BOF -- the base stays at the payload, which is not 0"
        );
    }

    // ---- low_is_true ----

    #[test]
    fn low_is_true_iff_base_is_absolute_zero() {
        assert!(
            Hay::new(b"xxxxxxxx", Abs(0)).low_is_true(),
            "base is absolute zero -- genuine BOF, no witness needed"
        );
        assert!(
            !Hay::new(b"xxxxxxxx", Abs(1)).low_is_true(),
            "base is absolute one -- not BOF, regardless of how few bytes precede it in reality"
        );
        assert!(
            !Hay::new(b"xxxxxxxx", Abs(100)).low_is_true(),
            "an ordinary interior base, far from BOF"
        );
    }

    // ---- seam_floor ----

    #[test]
    fn seam_floor_is_zero_at_a_true_low_edge() {
        let hay = Hay::new(b"xxxxxxxx", Abs(0)).with_accept(Local(0)..Local(8));
        assert_eq!(
            hay.seam_floor(),
            Local(0),
            "BOF needs no look-behind margin at all"
        );
    }

    #[test]
    fn seam_floor_is_ctx_behind_when_low_is_not_true() {
        // restructure R5 (2026-07-28): `Edge::Unreadable` is retired (folded into `Cut`, proven
        // to carry zero information any accept predicate reads, `restr-R4-review.md`'s own
        // mutation battery) -- a non-zero `base` is now the only way to be "not true".
        let hay = Hay::new(b"xxxxxxxx", Abs(100)).with_accept(Local(0)..Local(8));
        assert_eq!(
            hay.seam_floor(),
            Local(crate::search::CTX_BEHIND.get()),
            "base != Abs(0) -- not BOF, needs the full look-behind margin"
        );
    }

    #[test]
    fn seam_floor_never_drops_below_accepts_own_start() {
        let hay = Hay::new(b"xxxxxxxx", Abs(100)).with_accept(Local(6)..Local(8));
        assert_eq!(
            hay.seam_floor(),
            Local(6),
            "accept.start (6) already exceeds CTX_BEHIND (4) -- the floor is accept.start, not a \
             smaller CTX_BEHIND"
        );
    }

    // ---- accept_with_eof_widening ----

    #[test]
    fn eof_widening_admits_one_zero_width_position_when_not_phantom() {
        let hay = Hay::new(b"abc", Abs(0))
            .with_high(Edge::True)
            .with_accept(Local(0)..Local(3));
        assert_eq!(hay.accept_with_eof_widening(), Local(0)..Local(4));
    }

    #[test]
    fn eof_widening_does_not_admit_the_trailing_newlines_own_phantom() {
        let hay = Hay::new(b"ab\n", Abs(0))
            .with_high(Edge::True)
            .with_accept(Local(0)..Local(3));
        assert_eq!(
            hay.accept_with_eof_widening(),
            Local(0)..Local(3),
            "the last real byte is \\n -- position 3 is the phantom, never admitted"
        );
    }

    #[test]
    fn eof_widening_is_a_no_op_away_from_a_true_high_edge_or_the_hays_own_end() {
        let not_true = Hay::new(b"abc", Abs(0))
            .with_high(Edge::Cut)
            .with_accept(Local(0)..Local(3));
        assert_eq!(not_true.accept_with_eof_widening(), Local(0)..Local(3));
        let not_at_end = Hay::new(b"abcde", Abs(0))
            .with_high(Edge::True)
            .with_accept(Local(0)..Local(3));
        assert_eq!(
            not_at_end.accept_with_eof_widening(),
            Local(0)..Local(3),
            "accept.end (3) is not this hay's own bytes.len() (5) -- the true edge sits further \
             out, so this accept span earns no widening of its own"
        );
    }

    // ---- subhay ----

    #[test]
    fn subhay_inherits_the_high_edge_only_when_the_window_touches_the_parent_bound() {
        let parent = Hay::new(b"0123456789", Abs(1000))
            .with_high(Edge::True)
            .with_body_limit(Local(10));
        // window == the whole parent (0..10, touches the high bound) -- high inherits True.
        // Accept reaching the window's OWN end (10) is what lets widening fire at all
        // (`accept_with_eof_widening`'s own two-part gate: high == True AND accept.end ==
        // bytes.len()) -- proves `high` specifically came through, not merely that some edge
        // was preserved.
        let touches_high = parent.subhay(Local(0)..Local(10), Local(2)..Local(10));
        assert_eq!(touches_high.bytes(), b"0123456789");
        assert_eq!(touches_high.base(), Abs(1000));
        assert_eq!(
            touches_high.accept_with_eof_widening(),
            Local(2)..Local(11),
            "high inherited True, and accept reaches this sub-hay's own end -- widening applies"
        );
        // window 2..8 ("234567", an interior cut on both sides) with accept reaching the
        // WINDOW's own end (parent-relative 3..8, rebasing to local 1..6, and bytes.len() == 6
        // here too) -- even though accept reaches this sub-hay's own physical edge exactly the
        // way the widening gate wants, `high` did NOT inherit True (the window's own end, 8,
        // does not coincide with the parent's own bytes.len(), 10), so widening still must not
        // fire.
        let interior = parent.subhay(Local(2)..Local(8), Local(3)..Local(8));
        assert_eq!(interior.bytes(), b"234567");
        assert_eq!(interior.base(), Abs(1002));
        assert_eq!(
            interior.accept_with_eof_widening(),
            Local(1)..Local(6),
            "interior cut on the high end -- True did not carry through, no widening even though \
             accept's own (rebased) end reaches this sub-hay's own bytes.len()"
        );
    }

    #[test]
    fn subhay_low_is_true_re_derives_from_its_own_base_never_inherited() {
        // restructure R5 (2026-07-28): the low edge has no field to inherit at all -- `low_is_
        // true` re-derives itself from whatever `base` `subhay` computed (`to_abs(window.start)`),
        // so it is correct by construction regardless of what the PARENT's own low edge was.
        let parent = Hay::new(b"0123456789", Abs(0)).with_body_limit(Local(10));
        assert!(parent.low_is_true(), "parent base is absolute zero");
        let touches_start = parent.subhay(Local(0)..Local(10), Local(0)..Local(10));
        assert!(
            touches_start.low_is_true(),
            "window.start == 0 reaches all the way back to the parent's own true BOF -- base \
             stays Abs(0)"
        );
        let interior = parent.subhay(Local(2)..Local(8), Local(2)..Local(8));
        assert!(
            !interior.low_is_true(),
            "window.start == 2 -- an interior cut, base is Abs(2), not true BOF, regardless of \
             what the wider parent's own low edge was"
        );
    }

    #[test]
    fn subhay_rebases_body_limit_and_clamps_it_to_the_narrower_window() {
        let parent = Hay::new(b"0123456789", Abs(0)).with_body_limit(Local(9));
        // window 2..8, parent body_limit 9 -- rebased (9-2=7) but clamped to the window's own
        // width (6), so nothing past this sub-hay's own bytes is ever claimed reachable.
        let sub = parent.subhay(Local(2)..Local(8), Local(2)..Local(4));
        assert_eq!(sub.body_limit, Local(6));
    }

    // ---- last_verified / first_verified ----

    #[test]
    fn last_verified_returns_the_rightmost_accepted_start_no_end_condition() {
        let hay = Hay::new(b"..x..x..", Abs(0)).with_accept(Local(0)..Local(8));
        assert_eq!(hay.last_verified(&pat("x")), Some((Local(5), Local(6))));
    }

    #[test]
    fn first_verified_returns_the_leftmost_accepted_start() {
        let hay = Hay::new(b"..x..x..", Abs(0)).with_accept(Local(0)..Local(8));
        assert_eq!(hay.first_verified(&pat("x")), Some((Local(2), Local(3))));
    }

    #[test]
    fn first_and_last_verified_respect_body_limit() {
        let hay = Hay::new(b"..xxx..", Abs(0))
            .with_accept(Local(0)..Local(7))
            .with_body_limit(Local(4));
        assert_eq!(
            hay.first_verified(&pat("xxx")),
            None,
            "the match's own end (5) exceeds body_limit (4) -- never reportable"
        );
    }

    #[test]
    fn last_verified_falls_back_to_an_earlier_candidate_when_the_rightmost_fails_verification() {
        // restructure R5 (2026-07-28): the exact bug a plain `raw_rfind` + filter has (this
        // method's own doc comment) -- the RIGHTMOST raw "x" is at 6, but `body_limit` cuts it
        // off (7 > 6); a naive filter would discard the whole `Option` and report nothing, even
        // though an EARLIER "x" at 3 is fully within body_limit and must win instead.
        let hay = Hay::new(b"..x..x.", Abs(0))
            .with_accept(Local(0)..Local(7))
            .with_body_limit(Local(6));
        assert_eq!(
            hay.last_verified(&pat("x")),
            Some((Local(5), Local(6))),
            "the rightmost \"x\" (at 5, end 6) fits body_limit exactly -- must not be skipped for \
             a filter-shaped bug"
        );
        let tighter = hay.with_body_limit(Local(4));
        assert_eq!(
            tighter.last_verified(&pat("x")),
            Some((Local(2), Local(3))),
            "with body_limit tightened to 4, the \"x\" at 5 no longer fits -- the EARLIER one at \
             2 must be returned instead of None"
        );
    }

    // ---- zero_width_at_high ----

    /// **A P1 the restructure closed incidentally, pinned so it stays closed** (PR #46 review,
    /// 2026-07-23; verified against the restructured code 2026-08-01). The report: an accept range
    /// that includes the EOF position -- "`lb..lb + 1` over a `probe` whose length is `lb`" -- lets
    /// a zero-width `$`/`^$` match at `hay.len()` push the retry cursor to `hay.len() + 1`, and the
    /// next engine call panics, `find_at` being documented to panic at `start > haystack.len()`.
    ///
    /// It cannot happen through this type. `find_starting_in` (`search.rs`) advances only `while at
    /// < accept.end`, so a cursor stepped past the range never reaches the engine at all; and the
    /// only widening that can put `accept.end` above `bytes.len()` is `accept_with_eof_widening`,
    /// which adds exactly one position and only when `accept.end` already IS `bytes.len()`. Both
    /// halves are load-bearing, so the fixture exercises the reported shape verbatim as well as the
    /// widened one, across every zero-width-capable pattern this crate has had trouble with and
    /// both edge kinds. The assertion is that it returns at all.
    #[test]
    fn an_eof_accept_position_never_steps_the_engine_past_the_haystack() {
        for p in ["$", "^$", "x*", "(?:)", r"\b", r"\B", "a*$"] {
            let pattern = pat(p);
            for bytes in [&b""[..], &b"a"[..], &b"a\n"[..], &b"abc"[..], &b"aaaa"[..]] {
                for edge in [Edge::True, Edge::Cut] {
                    let base = || Hay::new(bytes, Abs(0)).with_high(edge);
                    let _ = base().zero_width_at_high(&pattern);
                    for hay in [
                        base().with_accept(base().accept_with_eof_widening()),
                        // the report's own shape: an accept range that starts at the hay's own end
                        base().with_accept(Local(bytes.len())..Local(bytes.len() + 1)),
                    ] {
                        let _ = hay.first_verified(&pattern);
                        let _ = hay.last_verified(&pattern);
                        let _ = hay.starts_verified(&pattern);
                    }
                }
            }
        }
    }

    #[test]
    fn zero_width_at_high_requires_a_true_edge_and_rejects_the_phantom() {
        let dollar = pat("$");
        let true_edge = Hay::new(b"abc", Abs(0)).with_high(Edge::True);
        assert!(true_edge.zero_width_at_high(&dollar));
        let cut_edge = Hay::new(b"abc", Abs(0)).with_high(Edge::Cut);
        assert!(
            !cut_edge.zero_width_at_high(&dollar),
            "not a declared true edge -- never admitted, regardless of what the regex would say"
        );
        let phantom = Hay::new(b"ab\n", Abs(0)).with_high(Edge::True);
        assert!(
            !phantom.zero_width_at_high(&dollar),
            "the phantom trailing newline"
        );
    }

    #[test]
    fn zero_width_at_high_gates_look_behind_adequacy_unconditionally() {
        // restructure R5 (2026-07-28) / fix round P2-1: a zero-width candidate has no body (its
        // start and end coincide), so this is a purely POSITIONAL question -- is `p` verified NOT
        // to be the trailing newline's own phantom -- not an assertion-soundness one; the gate
        // applies REGARDLESS of what the pattern is (`zero_width_at_high`'s own doc comment has
        // the full argument for why gating this on `leading_assertions()`, R5's first attempt,
        // was wrong: it let an assertion-free pattern report a match whose own phantom-status was
        // genuinely unverifiable). `base == Abs(3)` (not zero) with only 1 byte of held context
        // ("x", a word char) -- fewer than `CTX_BEHIND` (4) -- is exactly the fresh-seam shape:
        // unverifiable, for ANY pattern.
        let inadequate = Hay::new(b"x", Abs(3)).with_high(Edge::True);
        assert!(
            !inadequate.low_is_true(),
            "base (3) is not absolute zero -- not BOF"
        );
        let word_boundary_dollar = pat(r"(?u:\b)$");
        assert!(
            !inadequate.zero_width_at_high(&word_boundary_dollar),
            "an ASSERTION-BEARING pattern -- only 1 byte of look-behind, base not BOF -- (?u:\\b) \
             at this position is unverifiable"
        );
        let star = pat("x*");
        assert!(
            !inadequate.zero_width_at_high(&star),
            "an ASSERTION-FREE pattern -- still rejected: the gate is about this POSITION's own \
             unverifiable phantom-status, not about anything the pattern demands"
        );
        // the identical hay, but at true BOF (base == Abs(0)) -- BOF's axiom makes the SAME 1
        // byte fully adequate regardless of its own count, since nothing precedes it to verify --
        // for either pattern.
        let at_bof = Hay::new(b"x", Abs(0)).with_high(Edge::True);
        assert!(
            at_bof.zero_width_at_high(&word_boundary_dollar),
            "true BOF needs no look-behind margin at all (assertion-bearing)"
        );
        assert!(
            at_bof.zero_width_at_high(&star),
            "true BOF needs no look-behind margin at all (assertion-free)"
        );
    }

    // ---- starts_verified ----

    #[test]
    fn starts_verified_counts_every_start_including_overlapping_ones() {
        // "aa" over "aaa": non-overlapping enumeration finds one match (0..2); starts_verified
        // must find every START position a match exists at, including the overlapping one at 1.
        let hay = Hay::new(b"aaa", Abs(0)).with_accept(Local(0)..Local(3));
        assert_eq!(hay.starts_verified(&pat("aa")), vec![Local(0), Local(1)]);
    }

    #[test]
    fn starts_verified_the_f9_seam_cost_retirement_an_assertion_free_pattern_is_never_excluded() {
        // restructure R5 (2026-07-28): the F9 seam-cost retirement, at the `Hay` level. 8 'x's,
        // NOT at BOF (`base = Abs(100)`, mimicking a fresh `give_up` landing far from file start)
        // -- a bare literal ("x", no assertion at all) has a trustworthy start regardless of
        // look-behind adequacy, so EVERY position is now found, including the four that used to
        // be excluded by R4's own seam_floor-as-search-bound.
        let hay = Hay::new(b"xxxxxxxx", Abs(100)).with_accept(Local(0)..Local(8));
        assert_eq!(
            hay.starts_verified(&pat("x")),
            vec![
                Local(0),
                Local(1),
                Local(2),
                Local(3),
                Local(4),
                Local(5),
                Local(6),
                Local(7)
            ],
            "a bare literal needs no look-behind margin -- nothing is excluded"
        );
    }

    #[test]
    fn starts_verified_an_assertion_bearing_pattern_still_excludes_a_seam_adjacent_boundary() {
        // the overcount test's own twin, at the `Hay` level (mirrors `sweep_give_up_seam_does_
        // not_overcount_a_word_boundary_one_byte_past_the_landing`'s own fixture shape, search.rs):
        // an ASSERTION-BEARING pattern (`(?u:\b)needle`) still pays the identical excluded-window
        // cost R4 already charged -- the F9 retirement narrows the miss to assertion-bearing
        // patterns, it does not remove it. `Abs(100)`, not BOF: a single real space (a genuine
        // non-word byte) sits at local position 0, right before "needle" -- a real (?u:\b) exists
        // at local position 1, but with only ONE byte of held look-behind (short of CTX_BEHIND),
        // it is unverifiable and must be excluded.
        let excluded = Hay::new(b" needle", Abs(100)).with_accept(Local(0)..Local(7));
        assert_eq!(
            excluded.starts_verified(&pat(r"(?u:\b)needle")),
            Vec::<Local>::new(),
            "only 1 byte of look-behind, not BOF -- the real boundary at 1 is unverifiable"
        );
        // the identical boundary, now CTX_BEHIND (4) bytes past the hay's own start -- fully
        // verifiable, found normally.
        let included = Hay::new(b"xxx needle", Abs(96)).with_accept(Local(0)..Local(10));
        assert_eq!(
            included.starts_verified(&pat(r"(?u:\b)needle")),
            vec![Local(4)],
            "the real space at local position 3 now has a full CTX_BEHIND(4) bytes of look-behind \
             (\"xxx \") before it reaches this hay's own start -- verified, found"
        );
    }

    // ---- leftmost_confirmed ----
    //
    // batch 7 (2026-07-28), findings #1/#3: `leftmost_confirmed` LOST its third parameter
    // (`advancing: CutAdvances`, retired with `Candidate::Deferred` and the whole retry loop the
    // two existed to steer). The tests below are the batch-6 set carried forward, each re-derived
    // against the new shape rather than merely re-signatured -- three keep their outcome exactly,
    // one reverts to the outcome it had before K2 (and says why that is the honest one), one is
    // deleted as superseded, and two are new.

    #[test]
    fn leftmost_confirmed_reports_found_when_within_the_explored_frontier() {
        let hay = Hay::new(b"..needle..", Abs(0)).with_accept(Local(0)..Local(10));
        match hay.leftmost_confirmed(&pat("needle"), Local(10)) {
            Candidate::Found(s, e) => assert_eq!((s, e), (Local(2), Local(8))),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn leftmost_confirmed_reports_pending_past_the_explored_frontier() {
        let hay = Hay::new(b"..needle..", Abs(0)).with_accept(Local(0)..Local(10));
        match hay.leftmost_confirmed(&pat("needle"), Local(2)) {
            Candidate::Pending(s) => assert_eq!(s, Local(2)),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn leftmost_confirmed_finds_the_alternative_that_fits_inside_body_limit() {
        // "abcQ|bc" at body_limit=3: the leftmost-first branch "abcQ" (s=0) needs 4 bytes, one
        // more than this hay may report a body over, and "bc" (s=1, e=3) fits -- the exact fixture
        // batch-4 R1 introduced (fix round F4's own rationale, scan.rs) to prove that a candidate
        // the end rule rejects must not end the search. It never could be lost to `advancing`: a
        // body_limit overrun was never a trailing-margin rejection, so this retried under either
        // variant. Batch 7 reaches the same answer without a retry -- the end bound IS
        // `body_limit` here, so the engine is asked for a candidate that fits and returns the only
        // one there is.
        let hay = Hay::new(b"abcQ", Abs(0))
            .with_accept(Local(0)..Local(4))
            .with_body_limit(Local(3));
        match hay.leftmost_confirmed(&pat("abcQ|bc"), Local(4)) {
            Candidate::Found(s, e) => assert_eq!((s, e), (Local(1), Local(3))),
            other => panic!("expected Found(1, 3), got {other:?}"),
        }
    }

    #[test]
    fn leftmost_confirmed_reports_exhausted_when_nothing_matches() {
        let hay = Hay::new(b"xxxxx", Abs(0)).with_accept(Local(0)..Local(5));
        assert_eq!(
            hay.leftmost_confirmed(&pat("needle"), Local(5)),
            Candidate::Exhausted
        );
    }

    #[test]
    fn leftmost_confirmed_the_unit_j_class_over_cap_assertion_bearing_misses_assertion_free_survives()
     {
        // restructure R5 (2026-07-28), the unit-J class at the `Hay` level (mirrors
        // `search_forward_does_not_fabricate_an_over_cap_word_boundary`, scan.rs, and
        // `structural-accept.md` §3.6's own arithmetic proof). A run of 'a's exceeding
        // `MAX_MATCH_LEN`, cut by an ARTIFICIAL hay edge (`high` stays `Cut`, the default -- not
        // real EOF).
        //
        // RENAMED, batch 7 (2026-07-28) -- "defers" back to "misses" (R5's own word), and the
        // assertion-bearing outcome reverts from K2's `Deferred(Local(0))` to `Exhausted`, the
        // value it had at R5. That is not a regression of K2's fix, it is the removal of a
        // distinction that never reached anybody: `scan.rs` gave `Deferred` and `Exhausted` the
        // IDENTICAL arm, so the two differed only inside this file. `Exhausted` is also the
        // honest name for what this hay holds -- an over-cap match under a trailing assertion is
        // R5's documented MISS, not a candidate awaiting resolution, and calling it "deferred"
        // implied a later call would resolve it, which for a run longer than the carry is exactly
        // the false promise finding #1 was.
        let over_cap_len = crate::search::MAX_MATCH_LEN + 500;
        let data: Vec<u8> = vec![b'a'; over_cap_len];
        let hay = Hay::new(&data, Abs(0)).with_accept(Local(0)..Local(over_cap_len));
        match hay.leftmost_confirmed(&pat("a+"), Local(over_cap_len)) {
            Candidate::Found(s, e) => assert_eq!(
                (s, e),
                (Local(0), Local(over_cap_len)),
                "assertion-free -- the over-cap match keeps reporting, unaffected by the end rule \
                 (the recovery pin this predicate exists to guarantee)"
            ),
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(
            hay.leftmost_confirmed(&pat(r"a+(?u:\b)"), Local(over_cap_len)),
            Candidate::Exhausted,
            "assertion-bearing and over-cap -- no end at or below the reportable bound carries a \
             real word boundary (the only boundary in a homogeneous run sits at the hay's own \
             artificial edge, above the margin), so this hay genuinely holds nothing reportable"
        );
    }

    #[test]
    fn leftmost_confirmed_excludes_an_undecidable_leading_margin_by_flooring_the_start() {
        // batch 6 (2026-07-28), K2 fix round, P2-2's own fixture (a reviewer probe, adopted
        // verbatim then), re-derived for batch 7's shape. Advancing a FORWARD window adds bytes at
        // the HIGH end only, so a `lookbehind_ok` failure is never curable by a later call -- K2
        // expressed that by scoping its deferral to TRAILING rejections; batch 7 expresses it as a
        // bound instead, flooring the search at `seam_floor()` whenever the pattern can carry a
        // leading assertion, which admits exactly the positions `verified` admits and needs no
        // retry to discover it. `base = Abs(100)` (not BOF, so `low_is_true()` is false) and every
        // candidate start below `CTX_BEHIND` make `lookbehind_ok` genuinely fail.
        let hay = Hay::new(b"aq", Abs(100)).with_accept(Local(0)..Local(2));
        let pattern = pat(r"(?u:\b)a|q");
        assert!(
            !hay.lookbehind_ok(Local(0), crate::search::CTX_BEHIND),
            "fixture precondition: the leading margin genuinely fails at s=0"
        );
        assert!(
            pattern.leading_assertions(),
            "fixture precondition: the pattern carries a leading assertion, so the floor applies"
        );
        assert_eq!(
            hay.leftmost_confirmed(&pattern, Local(2)),
            Candidate::Exhausted,
            "no start in this hay has a decidable look-behind, and the floor says so directly \
             rather than discovering it one rejected candidate at a time"
        );
        // the floor and the per-candidate gate agree pointwise -- the equivalence the rewrite
        // relies on, asserted rather than argued (`seam_floor`'s own doc comment derives it).
        for s in 0..=8usize {
            assert_eq!(
                s >= hay.seam_floor().0,
                hay.lookbehind_ok(Local(s), crate::search::CTX_BEHIND),
                "seam_floor and lookbehind_ok must agree at local start {s}"
            );
        }
    }

    #[test]
    fn leftmost_confirmed_resolves_a_margin_rejected_run_without_rescanning_it() {
        // batch 6 (2026-07-28), finding #3's own structural RED, kept and re-pointed. Before K2, a
        // margin-only rejection retried at `s + 1` unconditionally: for a homogeneous run under a
        // trailing-assertion pattern the engine re-finds the SAME ending position from EVERY
        // starting position (greedy `a+` always consumes the whole remaining run), so all N starts
        // fail for the IDENTICAL reason -- O(N) retries, each paying an O(remaining) re-scan,
        // O(N^2) synchronously inside one step. This counts invocations rather than time
        // (`no_timing_oracles` is law, `AGENTS.md`'s own rule).
        //
        // What changed in batch 7: the count is 1 at BOTH boundaries now, not just at an advancing
        // one. K2 bought this count with an early return that only fired when the caller said the
        // window would grow, which left the `Final` boundary quadratic (finding #3, still 9.71s at
        // 80k) and lost later candidates at the advancing one (finding #1). There is no early
        // return and no `advancing` any more: one call, one answer, at every boundary.
        let n = 6000;
        let data = vec![b'a'; n];
        let hay = Hay::new(&data, Abs(0)).with_accept(Local(0)..Local(n));
        reset_raw_find_from_calls();
        let outcome = hay.leftmost_confirmed(&pat(r"a+\b"), Local(n));
        assert_eq!(
            outcome,
            Candidate::Exhausted,
            "no reportable candidate: every `\\b` this run could offer sits at the artificial edge"
        );
        let calls = raw_find_from_calls();
        assert_eq!(
            calls, 1,
            "O(1) expected: one end-bounded call answers the whole hay -- got {calls} calls for an \
             {n}-byte run; a count that scales with n would be the quadratic this test forbids"
        );
    }

    #[test]
    fn leftmost_confirmed_finds_a_later_candidate_behind_an_undecidable_greedy_run() {
        // batch 7 (2026-07-28), finding #1, at the `Hay` level -- the defect K2's deferral
        // introduced, in its smallest form. The leftmost RAW candidate (`A[A-Za-y]*(?u:\b)` from
        // 0) runs to this hay's own artificial edge and is undecidable there; an ordinary sub-cap
        // literal sits INSIDE it. K2 returned `Deferred(Local(0))` at the first such rejection,
        // which discarded the needle unexamined -- and `SearchForward::step`'s own carry shrink
        // then dropped the bytes it sat in, so no later call re-offered it either.
        let mut data = vec![b'b'; 200];
        data[0] = b'A';
        data[100..106].copy_from_slice(b"needle");
        let hay = Hay::new(&data, Abs(0)).with_accept(Local(0)..Local(200));
        match hay.leftmost_confirmed(&pat(r"A[A-Za-y]*(?u:\b)|needle"), Local(200)) {
            Candidate::Found(s, e) => assert_eq!(
                (s, e),
                (Local(100), Local(106)),
                "the sub-cap literal behind the undecidable run must still be reported"
            ),
            other => panic!("expected Found(100, 106), got {other:?}"),
        }
    }

    #[test]
    fn leftmost_confirmed_reports_nothing_when_the_hay_is_shorter_than_the_trailing_margin() {
        // batch 7 (2026-07-28): `reportable_end`'s own `None`, and why it is not `Local(0)`. This
        // hay holds fewer than CTX_AHEAD bytes behind a cut, so NO end -- not even 0 -- has a
        // decidable trailing assertion. The first version of `reportable_end` used
        // `saturating_sub` and floored to `Local(0)`, which still ADMITS a zero-width candidate at
        // 0; the caller-side `assert!` caught it on the very first differential run.
        let hay = Hay::new(b"ab", Abs(0)).with_high(Edge::Cut);
        let p = pat(r"(?:)(?u:\b)");
        assert!(p.trailing_assertions(), "fixture precondition");
        assert!(
            !hay.lookahead_ok(Local(0), crate::search::CTX_AHEAD),
            "fixture precondition: even end 0 lacks the margin in a 2-byte hay behind a cut"
        );
        assert_eq!(hay.leftmost_confirmed(&p, Local(2)), Candidate::Exhausted);
    }

    // ---- verified / lookbehind_ok / lookahead_ok ----

    #[test]
    fn lookbehind_ok_is_the_margin_or_bof() {
        let cut = Hay::new(b"xxxxxxxx", Abs(100));
        assert!(
            !cut.lookbehind_ok(Local(3), crate::search::CTX_BEHIND),
            "3 bytes -- short of CTX_BEHIND(4), not BOF"
        );
        assert!(
            cut.lookbehind_ok(Local(4), crate::search::CTX_BEHIND),
            "exactly CTX_BEHIND(4) bytes -- adequate"
        );
        let bof = Hay::new(b"xxxxxxxx", Abs(0));
        assert!(
            bof.lookbehind_ok(Local(0), crate::search::CTX_BEHIND),
            "BOF's axiom -- adequate regardless of the numeric margin"
        );
    }

    #[test]
    fn lookahead_ok_is_the_margin_or_true_high_edge() {
        let cut = Hay::new(b"xxxxxxxx", Abs(0)).with_high(Edge::Cut);
        assert!(
            !cut.lookahead_ok(Local(5), crate::search::CTX_AHEAD),
            "3 bytes past e (8-5) -- short of CTX_AHEAD(4), and high is not True"
        );
        assert!(
            cut.lookahead_ok(Local(4), crate::search::CTX_AHEAD),
            "exactly CTX_AHEAD(4) bytes past e (8-4) -- adequate"
        );
        let true_edge = Hay::new(b"xxxxxxxx", Abs(0)).with_high(Edge::True);
        assert!(
            true_edge.lookahead_ok(Local(8), crate::search::CTX_AHEAD),
            "e sits exactly at the true high edge -- adequate regardless of margin"
        );
    }

    #[test]
    fn verified_waives_each_margin_precisely_when_the_pattern_carries_no_matching_assertion() {
        // a hay NOT at BOF, with both margins inadequate at the candidate positions probed below
        // (nothing trivially waived by position alone).
        let hay = Hay::new(b"xxxxxxxx", Abs(100)).with_accept(Local(0)..Local(8));
        assert!(
            hay.verified(&pat("xxx"), Local(0), Local(3)),
            "no assertions at all -- both margin gates are waived, only accept/body_limit matter"
        );
        assert!(
            !hay.verified(&pat(r"(?u:\b)xxx"), Local(0), Local(3)),
            "a leading assertion -- lookbehind_ok(0) is false here (not BOF, 0 < CTX_BEHIND) -- \
             rejected"
        );
        assert!(
            hay.verified(&pat(r"xxx(?u:\b)"), Local(0), Local(3)),
            "a trailing assertion, but the margin past e=3 (8-3=5) already exceeds CTX_AHEAD(4) \
             -- adequate, so this one is NOT rejected (contrast with the next case)"
        );
        assert!(
            !hay.verified(&pat(r"xxxxxxx(?u:\b)"), Local(0), Local(7)),
            "a trailing assertion, end at 7 -- lookahead_ok(7) is false (8-7=1 < CTX_AHEAD) -- \
             rejected"
        );
    }

    // ---- viewport belt: point_verified / all_verified_row, and the frozen-reference cross-check ----

    #[test]
    fn point_verified_applies_the_viewport_belt() {
        let dollar = pat("Q$");
        // margin 0, hay does not reach true EOF -- unresolved.
        let unresolved = Hay::new(b"Q", Abs(0)).with_high(Edge::Cut);
        assert_eq!(
            unresolved.point_verified(&dollar, Local(0), Local(1), false),
            None
        );
        // identical hay, but declared as reaching true EOF -- a real boundary needs no margin.
        let resolved = Hay::new(b"Q", Abs(0)).with_high(Edge::True);
        assert_eq!(
            resolved.point_verified(&dollar, Local(0), Local(1), false),
            Some((Local(0), Local(1)))
        );
    }

    #[test]
    fn point_verified_rejects_buf_hi_unless_eof_zero_width_ok() {
        let star = pat("x*"); // zero-width-capable at position 1 too
        let hay = Hay::new(b"x", Abs(0)).with_high(Edge::True);
        assert_eq!(
            hay.point_verified(&star, Local(1), Local(1), false),
            None,
            "at buf_hi, not eof_zero_width_ok -- rejected (the phantom-admission gate)"
        );
        assert_eq!(
            hay.point_verified(&star, Local(1), Local(1), true),
            Some((Local(1), Local(1)))
        );
    }

    /// Cross-checks `Hay::viewport_belt` against `document::point_candidate_is_resolved`, the
    /// pre-R4 frozen reference the direct unit test
    /// (`point_candidate_is_resolved_rejects_the_phantom_even_when_the_belt_would_allow_it`,
    /// document.rs) still exercises.
    ///
    /// **Restructure R5 (2026-07-28): NARROWED, not retired, deliberately.** R4's version of this
    /// test asserted the two agree on EVERY input, full stop -- correct then, because `viewport_
    /// belt` had no assertion-awareness of its own to diverge over. R5 gives it one, and the
    /// frozen reference (unconditional margin check, by design -- it never gained a `pattern`
    /// parameter and never waives anything) is exactly what R4's OWN belt always was too. So the
    /// two are STILL expected to agree, but only for a pattern that carries BOTH a leading and a
    /// trailing assertion: `verified`'s guards (`!pat.leading_assertions() || ...` / `!pat.
    /// trailing_assertions() || ...`) collapse to the bare margin checks precisely when both flags
    /// are `true`, reproducing the frozen reference's own unconditional shape exactly. An
    /// ASSERTION-FREE pattern is where the two are now SUPPOSED to diverge (`viewport_belt` waives
    /// the margin, the frozen reference never can) -- that divergence IS H's own P1-1 paint pins
    /// moving, direction RECOVERY, the whole point of this restructure's own Part 2, pinned
    /// separately by `visible_window_matches_over_cap_assertion_free_matches_paint_again_the_r5_
    /// recovery` (document.rs), not here.
    #[test]
    fn hay_viewport_belt_agrees_with_the_frozen_point_candidate_is_resolved_for_assertion_bearing_patterns()
     {
        // both a leading (`^`) and a trailing (`$`) assertion -- neither of `verified`'s own two
        // guards is ever waived, reproducing the frozen reference's own unconditional margin check
        // exactly.
        let assertion_bearing = pat("^x$");
        for hay_end in [0usize, 1, 4, 10] {
            for e in 0..=hay_end {
                for full_hay_len in [hay_end, hay_end + 5] {
                    for hay_reaches_eof in [false, true] {
                        for buf_hi in [0usize, e, hay_end] {
                            for eof_zero_width_ok in [false, true] {
                                let s = e; // zero-width candidate; s's own value is what the
                                // "s != buf_hi" half discriminates on, independent of e.
                                let bytes = vec![b'x'; hay_end];
                                let high = if hay_end == full_hay_len && hay_reaches_eof {
                                    Edge::True
                                } else {
                                    Edge::Cut
                                };
                                let hay = Hay::new(&bytes, Abs(0)).with_high(high);
                                let via_hay = hay.viewport_belt(
                                    &assertion_bearing,
                                    Local(s),
                                    Local(e),
                                    Local(buf_hi),
                                    eof_zero_width_ok,
                                );
                                let via_frozen = crate::document::point_candidate_is_resolved(
                                    s,
                                    e,
                                    hay_end,
                                    full_hay_len,
                                    hay_reaches_eof,
                                    buf_hi,
                                    eof_zero_width_ok,
                                );
                                assert_eq!(
                                    via_hay, via_frozen,
                                    "hay_end={hay_end} e={e} full_hay_len={full_hay_len} \
                                     hay_reaches_eof={hay_reaches_eof} buf_hi={buf_hi} \
                                     eof_zero_width_ok={eof_zero_width_ok}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
