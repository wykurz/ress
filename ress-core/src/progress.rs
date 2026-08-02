//! Motion witnesses for the engine's resumable scan loops (restructure R6, 2026-07-28,
//! `.superpowers/sdd/structural-scan.md` §3.1). This is the structural half of "no
//! zero-progress `More`"; `crate::meter::Charged` is the other half. `meter.rs`'s own module doc
//! comment already named the seam: `Charged` answers "was any I/O done this step" (a read
//! happened, whether or not it moved anything); `Progressed` (this module) answers "did the
//! scan's own RESUMPTION CURSOR move this step" -- the state-machine half, never fused with the
//! meter half into one type (`structural-budget.md` §3.5's own ruling). `restr-R3-review.md` §R2.4
//! is the concrete evidence the seam is load-bearing, not aspirational: a reviewer's mutation to
//! `ForwardScan::step`'s read loop (charge the read, then `break` before advancing `self.pos`)
//! made `progress_witness()` succeed -- genuine I/O happened -- while the cursor stayed frozen;
//! the suite HUNG, not failed, because nothing at the time asked "but did the cursor move".
//! `scan.rs`'s own discriminating test for this exact mutation cites this module.
//!
//! **A resumption cursor is not "the reported answer".** A backward descent past an unreadable
//! truncation gap charges bytes it never reads (`Reader::skip`, meter-side bookkeeping) and moves
//! (this module's own `Descending::lower_to`) -- `Progressed` there. A forward search's deferred
//! candidate (`Candidate::Pending`, `search/hay.rs`) reads real bytes (`Charged`) while the
//! eventual ANSWER does not move at all -- but the SCAN's own resumption cursor (`self.pos`, the
//! byte offset the next `step` call resumes from) does, by the fresh bytes just consumed. Both are
//! legitimate `More`s; both carry a `Progressed` minted from the scan's own cursor, never from
//! whether an answer was found.
//!
//! **The escape hatch, named rather than hidden** (§5.2's own vacuity warning): nothing stops an
//! author inside this crate from writing `crate::progress::Progressed` -- Rust's `pub(crate)`
//! reaches every module in `ress-core`. The mitigation is the same shape this project already
//! uses for `tests/no_raw_hay_primitives.rs`: `tests/no_progressed_outside_progress.rs` greps for
//! the token `Progressed` anywhere outside this file and fails the build. A wall for an
//! inattentive author, a speed bump for a determined one -- the honest strength, not an
//! overclaimed one.

use std::num::NonZeroU64;

/// Evidence that a resumption cursor advanced by at least one byte. The only way to mint one is
/// `Ascending::advance_to`/`Descending::lower_to` actually returning `Some` -- no `From<u64>`, no
/// public tuple-struct access, no way to construct one from a bare number or from `Charged`
/// (`crate::meter`'s own witness answers a different question; see this module's own top-level
/// doc comment for why the two are never merged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Progressed(NonZeroU64);

impl Progressed {
    /// How many bytes the cursor moved. No production caller reads this back out today (every
    /// `More` site threads the witness straight into a `Step`/`BwdStep` variant for the driver to
    /// match on, never to inspect); kept as the natural accessor a witness type should have, and
    /// exercised by this module's own unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn bytes(self) -> u64 {
        self.0.get()
    }
}

/// A cursor that only ever descends, bounded below by `floor` -- `SearchBackward`'s `hi`,
/// `BackwardScan`'s `hi`.
pub(crate) struct Descending {
    at: u64,
    floor: u64,
}

impl Descending {
    pub(crate) fn new(at: u64, floor: u64) -> Descending {
        Descending { at, floor }
    }
    pub(crate) fn at(&self) -> u64 {
        self.at
    }
    pub(crate) fn floor(&self) -> u64 {
        self.floor
    }
    /// The loop guard every descent uses in place of a hand-written `self.hi > floor`.
    pub(crate) fn above_floor(&self) -> bool {
        self.at > self.floor
    }
    /// Re-clamps `at` down to a freshly discovered ceiling (`step`'s own entry, where an
    /// out-of-range `hi` degrades to the real bytes -- ERRATUM 3c#3) -- never itself a move: a
    /// clamp is not a resumption advance, and mints no witness.
    pub(crate) fn clamp_to(&mut self, ceiling: u64) {
        self.at = self.at.min(ceiling);
    }
    /// Lowers the cursor to `to`. `None` -- and NO mutation -- when `to >= self.at`: standing
    /// still, or "moving" to a position at or above the current one, is not a move, and the
    /// caller earns no witness for it. This is the check that makes the historical `.max(floor)`
    /// fixed-point hang (`restr-R3-review.md`'s own P1-NEW, `structural-scan.md` §2, class 3)
    /// unrepresentable as a `More`: a caller that computes a clamped target equal to `at` gets
    /// `None` here, not a witness it could paper over.
    #[must_use]
    pub(crate) fn lower_to(&mut self, to: u64) -> Option<Progressed> {
        let moved = NonZeroU64::new(self.at.checked_sub(to)?)?;
        self.at = to;
        Some(Progressed(moved))
    }
}

/// The ascending twin, for `ForwardScan`/`CountScan`/`SearchForward`'s resumption cursor (and,
/// while it is seeding, `SearchBackward`'s own straddle-read cursor).
pub(crate) struct Ascending {
    at: u64,
    ceiling: u64,
}

impl Ascending {
    pub(crate) fn new(at: u64, ceiling: u64) -> Ascending {
        Ascending { at, ceiling }
    }
    pub(crate) fn at(&self) -> u64 {
        self.at
    }
    pub(crate) fn ceiling(&self) -> u64 {
        self.ceiling
    }
    /// The loop guard every ascent uses in place of a hand-written `self.pos < size`.
    pub(crate) fn below_ceiling(&self) -> bool {
        self.at < self.ceiling
    }
    /// Re-narrows `ceiling` (and clamps `at` into the new bound) from freshly discovered
    /// information a constructor could not have had (`cache.size()`, unavailable until `step`'s
    /// first cache access) -- never itself a move.
    pub(crate) fn clamp_ceiling_to(&mut self, ceiling: u64) {
        self.ceiling = self.ceiling.min(ceiling);
        self.at = self.at.min(self.ceiling);
    }
    /// Advances the cursor to `to`. `None` -- and NO mutation -- when `to <= self.at`: see
    /// `Descending::lower_to`'s own doc comment for the symmetric reasoning.
    #[must_use]
    pub(crate) fn advance_to(&mut self, to: u64) -> Option<Progressed> {
        let moved = NonZeroU64::new(to.checked_sub(self.at)?)?;
        self.at = to;
        Some(Progressed(moved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descending_lower_to_mints_a_witness_on_a_real_move() {
        let mut d = Descending::new(100, 0);
        let p = d.lower_to(60).expect("60 < 100 is a real move");
        assert_eq!(p.bytes(), 40);
        assert_eq!(d.at(), 60);
    }

    #[test]
    fn descending_lower_to_refuses_a_non_decreasing_target_without_mutation() {
        let mut d = Descending::new(100, 0);
        assert!(d.lower_to(100).is_none(), "standing still is not a move");
        assert!(d.lower_to(150).is_none(), "moving backward is not a move");
        assert_eq!(d.at(), 100, "a refused move must not mutate the cursor");
    }

    #[test]
    fn ascending_advance_to_mints_a_witness_on_a_real_move() {
        let mut a = Ascending::new(10, 1000);
        let p = a.advance_to(35).expect("35 > 10 is a real move");
        assert_eq!(p.bytes(), 25);
        assert_eq!(a.at(), 35);
    }

    #[test]
    fn ascending_advance_to_refuses_a_non_increasing_target_without_mutation() {
        let mut a = Ascending::new(10, 1000);
        assert!(a.advance_to(10).is_none(), "standing still is not a move");
        assert!(a.advance_to(5).is_none(), "moving backward is not a move");
        assert_eq!(a.at(), 10, "a refused move must not mutate the cursor");
    }

    #[test]
    fn clamp_to_never_mints_a_witness() {
        // clamping is a discovery ("the file turned out to be shorter than assumed"), not a
        // resumption advance -- it must not require, or produce, a Progressed.
        let mut d = Descending::new(100, 0);
        d.clamp_to(50);
        assert_eq!(d.at(), 50);
        let mut a = Ascending::new(10, 1000);
        a.clamp_ceiling_to(20);
        assert_eq!(a.ceiling(), 20);
        assert_eq!(a.at(), 10);
        a.clamp_ceiling_to(5);
        assert_eq!(
            a.at(),
            5,
            "clamping the ceiling below `at` also pulls `at` down"
        );
    }
}
