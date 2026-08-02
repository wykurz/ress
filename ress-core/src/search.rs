//! Search: pattern compilation (smartcase), windowed byte matching, and (task 5)
//! the background match-summary analysis. Matching runs over fixed windows with
//! `MAX_MATCH_LEN` overlap -- matches longer than the cap are not guaranteed to
//! be found. **Restructure R5 (2026-07-28) closes the unit-J class**: an over-cap
//! match can no longer be FALSELY reported against a window's own artificial edge
//! (pre-existing at least since `07fc59d`; `search::hay::Hay::verified`'s own end
//! rule now rejects any candidate whose trailing assertion is decided at an
//! unearned cut) -- an over-cap match with a trailing assertion the cut cannot
//! verify is now a documented MISS, same as any other unresolvable candidate;
//! an over-cap match with NO trailing assertion at all (`a+`, `.*`, `[a-z]+`) still
//! keeps reporting, since a hay cut cannot manufacture a body (see `search::hay`'s
//! own module doc comment and `Hay::verified`'s doc comment for the full rule).
//! `.` does not match `\n` (regex::bytes default), so ordinary
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
//! tail of some larger real content). Every windowed caller therefore carries real look-behind/
//! lookahead context of its own alongside its sliding window (see each one's own doc comment for
//! the exact mechanism) so a slice's edge is never mistaken for a real line boundary.
//!
//! **The boundary-context model** (batch 4 (2026-07-24), findings #4/#6/#9/#10/#12's own
//! re-derive, replacing an earlier one-byte-sentinel/one-byte-ctx design that produced three
//! independent false-match defect classes plus a lookahead off-by-one): every consumer's own hay
//! is `[look-behind ctx][payload][lookahead]`, where
//! - **Look-behind** is up to `CTX_BEHIND` real bytes immediately below the payload -- never
//!   fabricated. A UTF-8 character is at most 4 bytes, so `CTX_BEHIND = 4` guarantees the FULL
//!   preceding character is always present, sufficient for `^` (needs 1 byte), ASCII `\b`/`\B`
//!   (need 1 byte), and Unicode-aware `(?u:\b)`/`(?u:\B)` (need the whole preceding codepoint --
//!   finding #9: one byte alone can be an invalid, lone UTF-8 continuation byte, decoded as
//!   non-word regardless of what the real character actually was). At true BOF there is no ctx at
//!   all, and the hay start IS the real start, so `\A` is correct by construction.
//! - **Lookahead** is enough real bytes past the last acceptable start that any match up to
//!   `MAX_MATCH_LEN` long starting in the accept range is confirmed or refuted INCLUDING its own
//!   trailing assertion: a cap-length match's `$`/`\b` is evaluated at `start + MAX_MATCH_LEN`,
//!   so the requirement is `MAX_MATCH_LEN` real bytes past the last acceptable start, not
//!   `MAX_MATCH_LEN - 1` (finding #10: the old bound left the trailing assertion with nothing
//!   past the match's own end to check, so a slice's own edge -- coinciding exactly with that
//!   end -- was mistaken for a real boundary again) -- batch 5 (2026-07-26), finding #3 extends
//!   this the SAME way finding #9 extended look-behind: that one byte is enough for `$` and ASCII
//!   `\b`/`\B`, but a Unicode-aware `(?u:\b)`/`(?u:\B)` needs the FULL following character, up to
//!   `CTX_AHEAD` bytes, not one (the identical "an incomplete byte decodes as non-word regardless
//!   of the real character" hazard, now on the trailing side), so the true requirement is
//!   `MAX_MATCH_LEN + CTX_AHEAD - 1` real bytes past the last acceptable start, not `MAX_MATCH_LEN`
//!   alone. Stated once, for both sides at once: **assertions at a position are verifiable iff the
//!   full adjacent character on each side is readable, or that side is a true file boundary.**
//! - **Accept range** is payload positions only; ctx bytes are never reportable match starts.
//! - **Where a boundary byte cannot be read, there is no sentinel.** No single fixed byte value
//!   is conservative for every assertion: a NUL byte makes `^` safely false (NUL is never `\n`)
//!   but manufactures a false `\b` wherever the unreadable real predecessor was actually a word
//!   character (finding #4) -- there is no substitute byte that is simultaneously "definitely not
//!   `\n`" and "definitely not a word character," because whether a given fixed value is
//!   conservative for `\b` depends on which way the real (unknown) byte would have pushed the
//!   verdict. Instead, the affected boundary POSITION is excluded from the accept range for
//!   exactly the one report that would otherwise have to guess, and the exclusion is a
//!   documented possible miss, never a fabricated match -- this project's own standing rule
//!   (docs/search.md) that windowed gaps are misses, never false positives.

pub(crate) mod hay;

/// bytes of payload each matching window advances by; reads underneath stay
/// block-granular, this only shapes how the regex sees the stream. `SweepAnalysis::step`
/// (task 5) reads exactly one window per bounded unit of background work.
pub(crate) const SEARCH_WINDOW: usize = 64 << 10;
/// window overlap: a match is found across a window seam iff it is shorter
/// than this; the honest documented cap on match length.
pub(crate) const MAX_MATCH_LEN: usize = 4 << 10;
/// A look-BEHIND width in bytes. Restructure R6 (`structural-scan.md` §3.8): before this type,
/// `CTX_BEHIND` and `CTX_AHEAD` were both plain `usize`s that happened to share the same value
/// (4, the UTF-8 max character width) for entirely unrelated reasons -- interchangeable to the
/// compiler, so a look-AHEAD computation that reached for `CTX_BEHIND` by mistake (the confusion
/// batch 5's own finding #6 found live, `document.rs`'s `edge_context`) type-checked anyway. A
/// function that means "a look-behind width" now says so in its own signature.
///
/// Fix round (R6 review P2-1): the tuple field is deliberately **not** `pub(crate)` -- a
/// reviewer-verified probe swapped `CTX_AHEAD.0` for `CTX_BEHIND.0` inside `Hay::
/// consultable_reach()`/`lookahead_ok()` and it compiled clean (both constants are numerically 4,
/// and `.0` reaches both types identically), so the newtype bought nothing at the sites the
/// confusion actually happens. `get()` (below) is the only way OUTSIDE this module and its
/// descendants (`crate::search::hay`, which still sees a private field directly -- Rust's own
/// "visible to the defining module and its descendants" rule, not blocked by privacy at all) to
/// recover a `usize`; the load-bearing fix is `hay.rs`'s own signatures (`lookbehind_ok`/
/// `lookahead_ok`/`consultable_reach`/`consumable_reach`/`seam_floor`) now naming `Behind`/`Ahead`
/// explicitly at the exact spot the constant is selected, so swapping one for the other is a
/// type error there -- not merely renamed plumbing elsewhere that still bottoms out in `usize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Behind(usize);
impl Behind {
    /// The only way to a bare `usize` from outside this module and `crate::search::hay`. Exists
    /// because real call sites (`document.rs`'s `edge_context`, `scan.rs`'s carry-shrink) do their
    /// own arithmetic with this width and cannot reasonably route every one through `Hay` -- this
    /// accessor does not, by itself, prevent picking the wrong constant at ITS OWN call site (it
    /// is symmetric with `Ahead::get`, so `CTX_BEHIND.get()` and `CTX_AHEAD.get()` are equally
    /// well-typed); the guard against that confusion is `hay.rs`'s own typed signatures, above.
    pub(crate) fn get(self) -> usize {
        self.0
    }
}
/// The look-AHEAD twin of `Behind` -- see its own doc comment for why this exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ahead(usize);
impl Ahead {
    /// `Behind::get`'s own twin -- see its doc comment.
    pub(crate) fn get(self) -> usize {
        self.0
    }
}
/// The boundary-context model's own look-behind width (this module's own doc comment, above):
/// up to this many real bytes are kept/fetched immediately before a payload, enough to hold one
/// whole UTF-8 character (at most 4 bytes) for `(?u:\b)`/`(?u:\B)`, not just one raw byte.
pub(crate) const CTX_BEHIND: Behind = Behind(4);
/// The boundary-context model's own look-AHEAD width (batch 5 (2026-07-26), finding #3),
/// symmetric with `CTX_BEHIND` above: a position's TRAILING assertions need the full FOLLOWING
/// character -- up to 4 bytes -- not merely one raw byte, for the identical reason `CTX_BEHIND`
/// already established on the other side: a Unicode-aware `(?u:\b)`/`(?u:\B)` decodes an
/// incomplete lead/continuation byte as non-word regardless of what the real, not-yet-fully-read
/// character actually was. `$` and ASCII `\b`/`\B` need only 1 byte and get the rest for free.
pub(crate) const CTX_AHEAD: Ahead = Ahead(4);

pub struct SearchPattern {
    pub raw: String,
    pub backward: bool,
    /// **`regex_automata::meta::Regex`, not `regex::bytes::Regex`** (batch 7 (2026-07-28),
    /// findings #1/#3). The two are the SAME engine under identical settings -- `regex::bytes::
    /// RegexBuilder::build` is a thin wrapper that parses with `regex_syntax` and hands the
    /// result to exactly this type -- so this is an API swap, not an engine swap. What the wider
    /// API buys is the one capability the narrower one structurally cannot express, and which
    /// this file's own `find_all_starting_in` doc comment already named as the reason a
    /// documented miss had to stay open ("closing it needs a span-aware search API this crate
    /// does not expose"): `Input::span` bounds where a match may END, while still deciding every
    /// look-around assertion against the bytes OUTSIDE the span.
    ///
    /// That distinction is this whole module's own boundary-context doctrine, finally available
    /// as a primitive instead of reconstructed after the fact. Before it, a windowed search had
    /// to hand the engine a hay whose PHYSICAL edge was the haystack edge, let the engine decide
    /// `$`/`\b` against that artificial boundary, and then have `Hay::verified` retroactively
    /// reject whatever the engine had already gotten wrong -- a predicate that can only ever
    /// answer "not this one," never "here is the one that fits," which is precisely why the
    /// rejection path needed a retry loop (finding #3's quadratic) and why stopping that loop
    /// early lost later candidates (finding #1). With a span, the margin bytes stay IN the
    /// haystack as real look-around context and only the match itself is bounded, so the engine
    /// never sees a fake boundary at all and the leftmost fitting candidate comes back in one
    /// call. `Hay::verified` survives as the end rule it always was; what dies is the walk that
    /// used to approximate it one rejected candidate at a time.
    regex: regex_automata::meta::Regex,
    /// **Restructure R5 (2026-07-28), the end rule's own conservatism** (`structural-accept.md`
    /// §3.4's own note: "the single most load-bearing detail of the proposal ... also the part
    /// most worth a second opinion"). Computed ONCE, here, from a `regex_syntax` parse of the
    /// SAME pattern text under the SAME flags the regex above compiles with -- a second parse,
    /// not a second source of truth: the `regex` crate does not expose the `Hir` it built
    /// internally, so this is the only way to ask "does this pattern carry an assertion in
    /// leading (respectively trailing) position" at all.
    ///
    /// **Fix round (2026-07-28), P1-1: a real recursive `HirKind::Look` walk with nullability**
    /// (`leading_look`/`trailing_look`, below), NOT `Properties::look_set_prefix_any`/
    /// `look_set_suffix_any` (round 1's own choice, reverted here). Those two are documented as
    /// "the set of assertions that MAY be passed" but their own implementation
    /// (`regex_syntax`'s `Properties::concat`) stops scanning a concatenation's children at the
    /// first one that CAN consume a byte (`maximum_len() > 0`), not the first one that MUST
    /// (`minimum_len() > 0`) -- an emptiable-but-consuming neighbour (`X*`, `X?`, `X{0,n}`,
    /// `(?:X|)`) stops the walk while still being able to match empty, hiding every assertion
    /// behind it. Measured, not assumed: `a+(?u:\b)\s*` on this module's own P2-4 fixture
    /// reported 12,185 against an oracle of 0 under the reverted classifier -- the exact
    /// fabrication this whole restructure exists to close, byte-for-byte, reopened by three
    /// trailing characters an ordinary user could type. The fix walks the HIR directly, using
    /// `minimum_len() == Some(0)` (not `maximum_len`) as the "can this neighbour be skipped
    /// entirely" test, which is regex-syntax's own precise nullability fact, not a re-derivation
    /// of one.
    ///
    /// Conservative in the direction that can only MISS, never fabricate: `true` means "may carry
    /// one" (a `Look` node reachable through a prefix/suffix of entirely nullable siblings --
    /// doubt resolves toward continuing the walk, never toward stopping early), `false` means
    /// "provably cannot" (a bare literal/class pattern like `a+`/`.*`/`[a-z]+` has no `Look` node
    /// anywhere, so both are `false` regardless of position; a STRICTLY INTERIOR assertion like
    /// `a\bb`, with a non-nullable neighbour on both sides, is correctly assertion-free on both
    /// axes too -- `structural-accept.md`'s own "for s < p < e both of the assertion's neighbour
    /// bytes lie in `[s, e)` by construction" argument, preserved exactly by this walk and lost by
    /// the simpler `!look_set().is_empty()` alternative, which would treat every pattern
    /// containing ANY `Look` node anywhere as bearing BOTH ends). See `Hay::verified`, the sole
    /// consumer, for how these gate the end rule.
    leading_assertions: bool,
    trailing_assertions: bool,
}
/// Does `hir` match the empty string? `minimum_len() == Some(0)` is regex-syntax's own exact
/// nullability fact (`None` means "matches nothing at all" -- the unsatisfiable-pattern case,
/// treated conservatively here as "could still be empty" so the walk below never stops early on
/// account of it; `Some(n > 0)` is the only case that legitimately halts a leading/trailing walk,
/// since it PROVES at least one real byte must be consumed there).
fn hir_can_be_empty(hir: &regex_syntax::hir::Hir) -> bool {
    !matches!(hir.properties().minimum_len(), Some(n) if n > 0)
}
/// Is a `Look` node reachable as a LEADING assertion of `hir` -- i.e. via a path from `hir`'s own
/// root where every sibling passed along the way could itself match empty, so control can reach
/// this `Look` having consumed zero bytes? Fix round (2026-07-28), P1-1's own replacement for
/// `Properties::look_set_prefix_any` (see `SearchPattern`'s own `leading_assertions` field doc
/// comment for why that library facility does not deliver what its own documentation promises).
fn hir_leading_look(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Look(_) => true,
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) => false,
        HirKind::Repetition(rep) => hir_leading_look(&rep.sub),
        HirKind::Capture(cap) => hir_leading_look(&cap.sub),
        HirKind::Concat(subs) => {
            for sub in subs {
                if hir_leading_look(sub) {
                    return true;
                }
                if !hir_can_be_empty(sub) {
                    return false;
                }
            }
            false
        }
        HirKind::Alternation(subs) => subs.iter().any(hir_leading_look),
    }
}
/// The trailing mirror of `hir_leading_look`: reachable via a path from `hir`'s own root where
/// every sibling AFTER this `Look`, up to the pattern's own end, could itself match empty.
fn hir_trailing_look(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Look(_) => true,
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) => false,
        HirKind::Repetition(rep) => hir_trailing_look(&rep.sub),
        HirKind::Capture(cap) => hir_trailing_look(&cap.sub),
        HirKind::Concat(subs) => {
            for sub in subs.iter().rev() {
                if hir_trailing_look(sub) {
                    return true;
                }
                if !hir_can_be_empty(sub) {
                    return false;
                }
            }
            false
        }
        HirKind::Alternation(subs) => subs.iter().any(hir_trailing_look),
    }
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
        // batch 7 (2026-07-28): ONE parse, feeding BOTH the engine and the assertion walk.
        // Before this, the pattern text was parsed twice -- once inside `regex::bytes::
        // RegexBuilder::build`, once here by `regex_syntax` for the HIR walk below -- with a long
        // comment (deleted with the code it defended) arguing at length that the two parsers
        // could not disagree, and a `.unwrap_or((true, true))` fallback in case they somehow did.
        // `meta::Builder::build_from_hir` takes the HIR this function already has, so "the engine
        // and the classifier agree about what this pattern is" stops being a verified claim and
        // becomes a fact with no second parse to be wrong about. The four flags below are the
        // same four `regex::bytes::RegexBuilder` sets internally (`regex-1.12.4/src/builders.rs`,
        // `build_many_bytes`), `.utf8(false)` included -- see this function's own flag comment
        // above, which is unchanged in substance and now applies to the only parse there is.
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(!sensitive)
            .multi_line(true)
            .unicode(false)
            .utf8(false)
            .build()
            .parse(raw)
            // `regex_syntax::Error` and `regex::Error::Syntax`'s own wrapped string are the
            // IDENTICAL rendering (`regex::bytes::RegexBuilder` formats the former into the
            // latter and adds nothing) -- verified byte-for-byte across ten malformed patterns
            // covering every arm `ress` itself surfaces, so the user-facing "bad pattern: ..."
            // notice (`ress/src/app.rs`, three pinned messages) is unchanged by this swap. The
            // error TYPE stays `regex::Error` for the same reason: no caller has to change, and
            // this crate keeps exactly one public vocabulary for a bad pattern.
            .map_err(|e| regex::Error::Syntax(e.to_string()))?;
        // `utf8_empty(false)`: the bytes-API semantics `regex::bytes::Regex` has and `regex::
        // Regex` does not -- an empty match may land between the bytes of a multi-byte codepoint.
        // Required here for the same reason `.utf8(false)` is required on the parser above: this
        // engine searches raw file bytes, and a zero-width match at a split codepoint is a real
        // position this pager can be asked about, not an invariant violation to be suppressed.
        let regex = regex_automata::meta::Builder::new()
            .configure(regex_automata::meta::Config::new().utf8_empty(false))
            .build_from_hir(&hir)
            .map_err(|e| match e.size_limit() {
                Some(limit) => regex::Error::CompiledTooBig(limit),
                None => regex::Error::Syntax(e.to_string()),
            })?;
        // restructure R5 (2026-07-28) / fix round P1-2: the HIR walk this struct's own
        // `leading_assertions`/`trailing_assertions` fields document -- now over the SAME `hir`
        // the engine above was built from, which retires the entire "can the second parse
        // disagree with the first" question this site used to answer at length. The four flags
        // that walk depended on (`.utf8(false)` in particular -- `ParserBuilder` defaults to
        // `utf8(true)`, which under `unicode(false)` rejected `.`, `.*`, `[^x]`, `\S`, `\W`, `\D`
        // outright and silently took a conservative fallback, so `.*`, named as the flagship
        // assertion-free recovery in five places, never actually recovered) are set once, on the
        // parse above, and there is no longer a second configuration to keep aligned with them.
        // The `.unwrap_or((true, true))` fallback is gone with the second parse that needed it: a
        // pattern whose parse fails now fails compilation outright, surfacing through the same
        // "bad pattern" notice path (`compile_error_surfaces`, this module's own test) rather
        // than compiling into an engine whose classifier had to guess.
        let (leading_assertions, trailing_assertions) =
            (hir_leading_look(&hir), hir_trailing_look(&hir));
        Ok(SearchPattern {
            raw: raw.to_string(),
            backward,
            regex,
            leading_assertions,
            trailing_assertions,
        })
    }
    /// The leftmost match starting at or after `at`, with every look-around assertion decided
    /// against the WHOLE of `hay` (`\A` only at a true 0, `\b` at `at` against the real preceding
    /// byte) -- exactly `regex::bytes::Regex::find_at`'s own contract, which is itself exactly
    /// this call: that method's whole body is `self.meta.find(Input::new(h).span(at..h.len()))`.
    /// Named and kept as its own method so every pre-existing caller below reads unchanged, and
    /// so the ONE thing batch 7 adds -- an upper bound on the span, `find_ending_by` -- is
    /// visibly the same primitive with one more bound, not a different search.
    fn find_from(&self, hay: &[u8], at: usize) -> Option<(usize, usize)> {
        self.regex
            .find(regex_automata::Input::new(hay).span(at..hay.len()))
            .map(|m| (m.start(), m.end()))
    }
    /// **The end-bounded primitive** (batch 7 (2026-07-28), findings #1/#3): the leftmost match
    /// that starts at or after `span.start` AND ends at or before `span.end`, with every
    /// look-around assertion still decided against the whole of `hay` -- including the bytes
    /// between `span.end` and `hay.len()`, which is the entire point.
    ///
    /// **This is not slicing, and the difference is the whole boundary-context doctrine**
    /// (this module's own top-level doc comment). `find_from(hay[..span.end], span.start)` would
    /// answer a DIFFERENT question: it hands the engine an artificial haystack whose end is
    /// `span.end`, so `$`/`\b`/`(?u:\b)` evaluated there fire against a cut this file has spent
    /// four review batches teaching every consumer not to trust. `Input::span` bounds only where
    /// the MATCH may fall; the haystack the assertions see is unchanged. Probe-verified rather
    /// than taken from the documentation (this project's own standing rule -- a library's
    /// documented contract is a claim to TEST): `a(?u:\b)` over `b"ab"` with `span 0..1` returns
    /// `None`, not a fabricated match, and `(?u:\b)y` over `b"xy"` with `span 1..2` likewise --
    /// both would match if the span were a slice. This module's own
    /// `span_bounds_the_end_without_faking_either_boundary` test pins both directions.
    ///
    /// A greedy pattern returns a SHORTER match here than it would unbounded (`a+` over `aaaa`
    /// with `span 0..3` is `(0, 3)`, not `(0, 4)`) -- which is the point at the one call site
    /// that uses this: a candidate whose greedy body overruns what a window may report on is not
    /// thereby a dead position, it may still have a completion that fits, and asking the engine
    /// for that completion directly is what replaces retrying at every subsequent byte.
    fn find_ending_by(&self, hay: &[u8], span: std::ops::Range<usize>) -> Option<(usize, usize)> {
        if span.start > span.end || span.end > hay.len() {
            return None;
        }
        self.regex
            .find(regex_automata::Input::new(hay).span(span))
            .map(|m| (m.start(), m.end()))
    }
    /// See this struct's own `leading_assertions` field doc comment.
    pub(crate) fn leading_assertions(&self) -> bool {
        self.leading_assertions
    }
    /// See this struct's own `trailing_assertions` field doc comment.
    pub(crate) fn trailing_assertions(&self) -> bool {
        self.trailing_assertions
    }
    /// first match whose START lies in `accept` (indices into `hay`);
    /// returns (start, end). The accept range is the exactly-once rule:
    /// windows overlap by `MAX_MATCH_LEN + CTX_AHEAD - 1` (batch 5 (2026-07-26), finding #3:
    /// widened from a bare `MAX_MATCH_LEN` so a cap-length match's own trailing assertion always
    /// has the FULL following character, not one byte, already read -- this module's own
    /// top-level doc comment), and only the window whose non-overlap span contains the start
    /// reports it. A caller that stops at its first match (never searches this hay again) may
    /// instead pass the whole hay as `accept`; a caller that enumerates every match must still
    /// pass fresh-starts-only or it double-counts a seam match.
    pub(crate) fn find_starting_in(
        &self,
        hay: &[u8],
        accept: std::ops::Range<usize>,
    ) -> Option<(usize, usize)> {
        let mut at = accept.start;
        while at < accept.end {
            let (s, e) = self.find_from(hay, at)?;
            if s >= accept.end {
                return None;
            }
            if s >= accept.start {
                return Some((s, e));
            }
            // a search from `at` can return a match starting before `at` only when
            // anchored oddly; step past defensively.
            at = s.max(at) + 1;
        }
        None
    }
    /// last match whose start lies in `accept` -- iterate forward, keep the last.
    ///
    /// Restructure R5 (2026-07-28): zero production callers -- `search::hay::Hay::last_verified`
    /// used to be this primitive's only caller (via the deleted `raw_rfind`), but a plain
    /// rightmost-raw-match-then-filter cannot express `verified`'s own per-candidate retry (this
    /// method's own doc comment on the exact bug that shape has), so `last_verified` now walks
    /// forward via `find_starting_in` instead, same as every other enumeration method here. Kept
    /// -- not deleted -- as part of the CI-guarded raw-primitive set (`tests/no_raw_hay_
    /// primitives.rs`, which references it by name in its own fixtures) and exercised directly by
    /// this module's own test, below.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rfind_starting_in(
        &self,
        hay: &[u8],
        accept: std::ops::Range<usize>,
    ) -> Option<(usize, usize)> {
        let mut best = None;
        let mut at = accept.start;
        while let Some((s, e)) = self.find_from(hay, at) {
            if s >= accept.end {
                break;
            }
            if s >= accept.start {
                best = Some((s, e));
            }
            at = s + 1;
        }
        best
    }
    /// All matches intersecting `hay` (start anywhere) -- the pure whole-buffer oracle this
    /// module's own proptests compare windowed scanning against (`SearchPattern`'s own doc
    /// comment on why `find_all` stays unfiltered/phantom-unaware). Batch 4 (2026-07-24),
    /// finding #6 moved the viewport highlighter off this method onto `find_all_starting_in`,
    /// below, which supplies real boundary-context bytes around a row slice's own edges instead
    /// of treating them as real boundaries -- `find_all` itself is unchanged, still bounded by
    /// whatever hay it is handed.
    pub fn find_all(&self, hay: &[u8]) -> Vec<(usize, usize)> {
        self.regex
            .find_iter(regex_automata::Input::new(hay))
            .map(|m| (m.start(), m.end()))
            .collect()
    }
    /// All NON-OVERLAPPING matches (the same convention `find_all`'s own `find_iter` already
    /// uses) whose START lies in `accept` AND whose END does not exceed `accept.end` either --
    /// the viewport highlighter's own span-restricted sibling to `find_all` (batch 4
    /// (2026-07-24), finding #6). `hay` may extend past `accept` on either side (real boundary
    /// CONTEXT -- up to `CTX_BEHIND` bytes before/after a viewport's own buffer, `Document::
    /// viewport`'s own doc comment) so `^`/`$`/`\b`/`\B` see genuine content there instead of a
    /// slice's own artificial edge, but a candidate match is only ever REPORTED once it is
    /// wholly contained in `accept` -- a match starting inside `accept` but needing bytes past
    /// `accept.end` to complete remains a documented miss (this project's own standing rule:
    /// gaps are misses, never false positives), never partially reported and never confirmed by
    /// content the caller cannot actually render. `find_all` itself is untouched by this
    /// (`SearchPattern`'s own doc comment): a separate, additive method, not a behavior change
    /// to the pure oracle.
    ///
    /// **One caller deliberately opts OUT of the END-containment half** (fix round (2026-07-27),
    /// P3-7, stated here so a reader checking this guarantee does not assume it holds universally):
    /// `document.rs`'s `visible_window_matches` passes `hay.end` (its own row-scoped slice's own
    /// length), not `accept.end` (the row's own narrower window bound), as THIS method's own
    /// `accept` parameter -- so the end-containment check above degenerates to `m.end() >
    /// slice.len()`, which can never fire, and a match's own END is constrained by that caller's
    /// OWN resolved-criterion belt instead (a margin/EOF check applied to the RESULTS, not a
    /// containment check on the SEARCH). This is deliberate, not an oversight: unit C's own review
    /// (batch 4) demonstrated that passing the row's own true `accept.end` here breaks multiline
    /// matches whose own tail extends into a LATER row (six tests, including both spanning-row
    /// fixtures) -- the wider `hay.end` is what lets a match starting in one row's own accept
    /// range still be found when its own body crosses into the next.
    ///
    /// Symmetric residual (fix round 2 (2026-07-25), final verdict, P3-a): the retry below
    /// recovers a *later* start, never a *shorter* alternative at the *same* start --
    /// `find_all_starting_in(b"foobar", 0..3)` with pattern `foobar|foo` returns `[]`, even
    /// though `(0, 3)` (the `foo` alternative alone) is wholly within `accept`, because
    /// leftmost-first alternation commits to the longer branch at that position. A documented
    /// miss, never a false positive.
    ///
    /// **Still open HERE, but no longer for the reason this comment used to give** (batch 7
    /// (2026-07-28)): it said "closing it needs a span-aware search API this crate does not
    /// expose," and that API now exists -- `find_ending_by`, above, which `Hay::leftmost_
    /// confirmed` uses to close this exact residual at `SearchForward`'s own bounded-`limit`
    /// boundary (`docs/search.md`'s F4 bullet, where it was the same limitation). What keeps it
    /// open on THIS method is a different fact: an END-bounded search returns the leftmost match
    /// that fits, which is what a stop-at-first caller wants, while this method ENUMERATES and
    /// must advance past each reported match by its own end. Bounding every one of those probes
    /// would change which matches an enumerating caller reports -- `n`'s own overlapping-start
    /// contract and the viewport's own non-overlapping one both key off it (this module's own
    /// "the two counting walks stay two"). That is a semantics decision for the counting walks,
    /// not a primitive that was missing; deliberately out of batch 7's scope, which is the accept
    /// walk alone.
    pub fn find_all_starting_in(
        &self,
        hay: &[u8],
        accept: std::ops::Range<usize>,
    ) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut at = accept.start;
        while at < accept.end {
            let Some((s, e)) = self.find_from(hay, at) else {
                break;
            };
            if s >= accept.end {
                break;
            }
            if s < accept.start {
                // fix round (2026-07-25), F10: unreachable in practice -- `at` starts at
                // `accept.start` and only ever grows, and `find_from(hay, at)`'s own contract is
                // to return a match starting at or after `at`, so `s < accept.start`
                // (equivalently `< at`) never fires. Kept anyway, mirroring `find_starting_in`'s
                // own identical guard, as a release-mode safety net rather than trusting that
                // contract unconditionally with no fallback if it were ever violated -- a
                // defensive branch, explicitly stated as such so a reader does not mistake it
                // for a live path (the `continue` below never actually runs).
                at = s.max(at) + 1;
                continue;
            }
            if e > accept.end {
                // fix round (2026-07-25), F3 -- corrected from an earlier `break` here, whose
                // own comment claimed "every match further right needs at least as much room,
                // so nothing more remains to find": false for alternations and greedy patterns,
                // where a LATER start can complete in FEWER bytes than an earlier, leftmost-
                // first candidate that overran the accept range (probe: `xyz|y` over `b"xyz"`,
                // `accept = 0..2` -- the `xyz` branch overruns and used to `break` immediately,
                // silently dropping the wholly-contained `y` match at `1..2` that `find_all`
                // itself, before this method existed, correctly reported). This start alone is
                // the documented miss (this method's own doc comment) -- not a proof that the
                // walk is over. Step past just it and keep looking, mirroring `find_starting_
                // in`'s own identical "step past, don't stop" shape for its own guard above.
                at = s + 1;
                continue;
            }
            found.push((s, e));
            at = if e > s { e } else { s + 1 };
        }
        found
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
/// ANCHOR CORRECTNESS (`^`/`$`, `multi_line` -- `SearchPattern::compile`'s own doc comment),
/// using the boundary-context model (this module's own top-level doc comment, batch 4
/// (2026-07-24)): `$` is correct below the cap with no further mechanism -- a candidate match's
/// own start is only ever accepted below `safe_to`, and a SUB-cap match (length `<
/// MAX_MATCH_LEN`) starting there ends strictly before `read_pos` (`step`'s own local, the
/// absolute position already read), so the byte immediately after it is always already in
/// `carry`, letting `$` see real content, never an artificial end. `^`/`\b`/`\B` need an explicit
/// mechanism, `ctx_len`/`carry`'s own leading bytes below: WITHOUT it, a position only ever
/// tested as `carry`'s own hay-index 0 (freshly re-based after every report's `drain`, per this
/// struct's own history) would unconditionally satisfy `^` regardless of the REAL byte before
/// it, since a slice's own edge is always `^`-eligible (this module's own doc comment) --
/// reachable at EVERY report boundary, not just a window seam, and proven by a regression test
/// in this module (`sweep_analysis_does_not_fake_a_line_start_at_a_report_boundary`) before this
/// existed. The fix keeps up to `CTX_BEHIND` REAL look-behind bytes permanently at the front of
/// `carry` (`carry`'s own doc comment) so hay-indices `[0, ctx_len)` are only ever that context
/// -- never itself an acceptable match start (`step`'s own accept range excludes it) -- and the
/// real earliest candidate sits at hay-index `ctx_len`, correctly evaluated against real content
/// on every single report, not just the window's own first one. Where NO real look-behind byte
/// exists to offer (right after `give_up` skips a failing block, `seam_gap` below), there is no
/// sentinel standing in for it either: the one affected position is excluded from that report's
/// own accept range instead (this module's own top-level doc comment's own "no sentinel" rule).
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
    /// `[0, ctx_len)` holds up to `CTX_BEHIND` REAL look-behind bytes (this struct's own ANCHOR
    /// CORRECTNESS doc comment) -- either the real file bytes immediately below `pos`, or (right
    /// after `give_up`) nothing at all (`ctx_len == 0`, paired with `seam_gap`, below: no
    /// sentinel stands in for an unknowable predecessor -- this module's own top-level doc
    /// comment). `[ctx_len, ..)` holds bytes read at or past `pos` but not yet safely
    /// reportable -- either genuinely fresh (the current window's own not-yet-confirmed tail) or,
    /// right after `pos` itself, physically the PREVIOUS window's own lookahead read (windows are
    /// contiguous, so nothing is ever re-read from the source for it). Empty only at the very
    /// start of a sweep (before `pos` has ever advanced past 0) or right after `give_up`.
    carry: Vec<u8>,
    /// How many of `carry`'s own leading bytes are look-behind context (`carry`'s own doc
    /// comment) rather than the first byte of not-yet-reported content -- `0` only at true BOF
    /// (before `pos` has ever advanced past 0) or right after `give_up` (`seam_gap`, below);
    /// otherwise `1..=CTX_BEHIND`, however many real bytes were actually available (fewer than
    /// `CTX_BEHIND` only ever at true BOF plus a short read, never past `give_up`, which starts
    /// this back at exactly 0). Unlike the single permanent context byte this field replaces
    /// (pre-batch-4), a WIDTH generalizes cleanly to "however many real bytes are on hand,"
    /// including zero, without a separate sentinel case.
    ctx_len: usize,
    /// **Where `carry` actually BEGINS** (restructure R7, batch 8 (2026-07-29)) -- state
    /// maintained at every mutation of the buffer, never re-derived at a use site. Both hays in
    /// this struct used to compute their own base as `Abs(self.pos - self.ctx_len)`, which is the
    /// position-minus-length shape `hay::Assembly` exists to retire: it silently re-asserts that
    /// `carry`'s leading `ctx_len` bytes sit immediately below `pos`, a premise `pos` and
    /// `ctx_len` only satisfy while every mutation happens to move them in lockstep. That is true
    /// today -- `give_up` clears both when its skip breaks contiguity (its own comment says so at
    /// length), and the per-report drain advances them together -- and tracking it makes the
    /// lockstep a thing each mutation states rather than a thing four sites assume.
    carry_base: crate::search::hay::Abs,
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
    /// restructure R3 (design point 5, the driver regime): this sweep keeps its own existing
    /// per-step-by-structure bound (`SEARCH_WINDOW`), never a byte budget -- reads route through
    /// `read_at_unbounded` (charged, never refused) purely so the same one Reader is the ONLY
    /// read path in the codebase, not to gate this loop's own progress on it.
    meter: crate::meter::Meter,
}
impl SweepAnalysis {
    pub(crate) fn new(cache: std::sync::Arc<crate::cache::BlockCache>) -> SweepAnalysis {
        let bs = cache.block_size();
        SweepAnalysis {
            cache,
            pattern: None,
            generation: 0,
            pos: 0,
            carry: Vec::new(),
            ctx_len: 0,
            carry_base: crate::search::hay::Abs(0),
            matches: 0,
            buckets: [0u16; 1024],
            failed: false,
            last_failed_block: None,
            meter: crate::meter::Meter::background(bs),
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
    /// byte offset times 1024 must not overflow a `u64` multiply. `match_at <= size` always
    /// holds here (a match can only start inside hay this sweep actually read, itself bounded
    /// by `size` -- INCLUSIVE, not exclusive: a legitimate zero-width match exactly at the true
    /// end reports `match_at == size`, both from `step`'s own in-loop EOF widening and from this
    /// unit's own empty-file/`give_up` probe, batch 4 (2026-07-24), findings #2/#8). `.min(len -
    /// 1)` is therefore a REACHABLE clamp, not merely defensive: `match_at == size` computes
    /// `bucket == self.buckets.len()`, one past the end, and the clamp folds it into the LAST
    /// bucket -- the correct answer (the file's own final position belongs in its own final
    /// bin), just not one the raw division alone lands on.
    fn record_match(&mut self, match_at: u64, size: u64) {
        self.matches = self.matches.saturating_add(1);
        let bucket =
            ((match_at as u128 * self.buckets.len() as u128) / (size.max(1) as u128)) as usize;
        let bucket = bucket.min(self.buckets.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}
impl SweepAnalysis {
    /// **The look-behind + payload window, positioned where this sweep has TRACKED it**
    /// (restructure R7, batch 8 (2026-07-29)) -- `carry_base`'s own doc comment has the
    /// derivation of why that is state rather than the `pos - ctx_len` subtraction this replaces
    /// at both of this struct's hay sites.
    ///
    /// Unlike `SearchForward`'s twin, this struct keeps its look-behind INSIDE `carry` (indices
    /// `[0, ctx_len)`, this struct's own doc comment), so there is only ever one run to anchor
    /// and nothing to `extend_*` -- the assembly exists here for the base, and for the guard that
    /// keeps every hay in the crate going through one constructor.
    ///
    /// Takes its fields explicitly rather than `&self` for the same reason `SearchForward::
    /// window_of` does: the call sites sit where other fields are already mutably borrowed.
    fn window_of(
        base: crate::search::hay::Abs,
        carry: &[u8],
        ctx_len: usize,
        pos: u64,
    ) -> crate::search::hay::Assembly {
        debug_assert_eq!(
            base + ctx_len as u64,
            crate::search::hay::Abs(pos),
            "`carry`'s own look-behind prefix always sits immediately below `pos` -- a mismatch \
             means `carry_base`, `ctx_len` and `pos` stopped moving in lockstep"
        );
        crate::search::hay::Assembly::anchored_at(base, carry)
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
        self.ctx_len = 0;
        self.carry_base = crate::search::hay::Abs(0);
        self.matches = 0;
        self.buckets = [0u16; 1024];
        self.failed = false;
        self.last_failed_block = None;
        self.snapshot(false)
    }
    async fn step(
        &mut self,
        tx: &tokio::sync::watch::Sender<SearchSummary>,
    ) -> anyhow::Result<crate::scan::Step<()>> {
        let pattern = self
            .pattern
            .clone()
            .expect("step called before begin -- the driver never does this");
        // batch 4 (2026-07-24 fix round), F2: `mut` -- the truncation policy (docs/budgeted_
        // scanning.md) shrinks this down to the source's own REAL end the moment a short/empty
        // block proves `self.cache.size()` overstated it (below, at the read loop's own
        // `take == 0` site), so every downstream formula in this call (`at_final`, the
        // zero-width-at-true-end widening, and `done` at the very end) uniformly treats the
        // real end as final from that point on -- one reassignment, not a parallel flag
        // threaded through every formula that currently reads `size`.
        let mut size = self.cache.size();
        self.pos = self.pos.min(size);
        // restructure R6: this call's own resumption-cursor snapshot, taken right after the
        // clamp above (so it reflects the position `step` genuinely started this call from) --
        // compared against `self.pos`'s own final value to mint `Step::More`'s motion witness at
        // the bottom of this function, the same "did the cursor move THIS call" fact an
        // `Ascending` cursor threaded through the whole loop would prove, without retrofitting
        // one onto every one of `self.pos`'s existing read/write sites in this large loop (a
        // scoped reconciliation choice: `Progressed` is still minted ONLY through `advance_to`,
        // never fabricated -- see the `moved` binding, below, for where that happens).
        let pos_at_entry = self.pos;
        self.meter.begin_step();
        // batch 4 (2026-07-24), finding #8: nothing left to read -- either an empty file (`begin`
        // leaves `pos` at 0, `size == 0`) or `give_up` having skipped straight to the true EOF.
        // `core_end`/`read_to` below both collapse to `pos` in this state (`core_end =
        // min(pos + WINDOW, size) = size = pos`, and `read_to` likewise), so the read loop's own
        // `while read_pos < read_to` never runs at all -- not just zero matches found, but no
        // REPORT of any kind, ever, meaning the EOF zero-width widening the loop's own per-block
        // report performs (ZERO-WIDTH AT THE TRUE EOF, below) never gets a chance to fire either.
        // Probe directly instead, the same accept shape that widening uses for the identical
        // position, before publishing `done`. Reached AT MOST ONCE per generation: `self.pos` can
        // only equal `size` at a step's own ENTRY via `begin` (`size == 0`) or `give_up` (which
        // sets it directly) -- in both cases this branch itself returns `Ok(true)` below, and the
        // driver (`crate::analyzer::spawn`) never calls `step` again until a fresh request resets
        // `pos` to 0 via a new `begin` -- so there is no way to double-probe, let alone
        // double-count, within one generation.
        if self.pos == size {
            // batch 4 (2026-07-24), finding #4's own UNIT-A INTERACTION note: this probe is
            // reached via TWO different routes that must NOT be treated alike -- true BOF
            // (`begin` leaves `pos`/`ctx_len` at 0/0 for an empty file) has no predecessor at
            // all; `give_up` landing its skip exactly at the true EOF also leaves `ctx_len == 0`
            // (no sentinel, this struct's own top-level doc comment) but its real predecessor --
            // the failing region's own last byte -- is UNKNOWABLE, not "no predecessor."
            //
            // restructure R4 (2026-07-27) / R5 (2026-07-28): A2's shape, `Hay::zero_width_at_high`
            // -- the SAME point probe A4/A7 use. R5 retired the explicit `low`/`seam_floor` gate
            // this comment used to derive by hand: `Hay::low_is_true` now derives the identical
            // distinction from the hay's own `base` alone (`self.pos - self.ctx_len as u64`,
            // zero exactly at true BOF, nonzero at a fresh give_up landing -- the two routes this
            // paragraph's own first sentence names), and `zero_width_at_high` itself now consults
            // it -- UNCONDITIONALLY (fix round, 2026-07-28, P2-1; R5's own first attempt gated
            // this on the pattern's own leading-assertion fact instead, which is wrong: applying
            // the phantom rule ONLY when the pattern happens to carry a leading assertion would
            // let an assertion-free zero-width candidate (`x*`, `q*`, a bare `(?:)`) through
            // regardless of whether the seam-gap path's real predecessor might have been `\n` --
            // a phantom overcount, exactly the sentinel defect class this whole re-derive exists
            // to close, review-measured back in on the committed `sweep_analysis_seam_gap_
            // landing_exactly_at_size_excludes_zero_width_matches` fixture before being fixed
            // here) -- before admitting a zero-width candidate here. One predicate, not a second
            // gate re-derived at this call site to match it; see `zero_width_at_high`'s own doc
            // comment (`hay.rs`) for the full position-vs-assertion argument.
            // restructure R7 (batch 8 (2026-07-29)): based where `carry` is TRACKED to begin,
            // not at `pos` minus a length.
            let asm = Self::window_of(self.carry_base, &self.carry, self.ctx_len, self.pos);
            let hay = asm.hay().with_high(crate::search::hay::Edge::True);
            if hay.zero_width_at_high(&pattern) {
                self.record_match(self.pos, size);
            }
            let _ = tx.send(self.snapshot(true));
            return Ok(crate::scan::Step::Done(()));
        }
        let bs = self.cache.block_size() as u64;
        let core_end = self.pos.saturating_add(SEARCH_WINDOW as u64).min(size);
        // batch 4 (2026-07-24), finding #10: this window's own search needs a full
        // MAX_MATCH_LEN bytes of LOOKAHEAD past `core_end`, not MAX_MATCH_LEN - 1, before its
        // own tail can be definitively resolved (this module's own top-level doc comment): a
        // maximal-length candidate's own TRAILING assertion is evaluated at `start +
        // MAX_MATCH_LEN`, one byte past its own last content byte, and the old bound gave it
        // nothing there to check -- a slice ending exactly at the candidate's own end was
        // mistaken for a real boundary again (the `a{MAX_MATCH_LEN}$` false positive
        // `sweep_analysis_does_not_treat_its_own_read_limit_as_a_real_boundary`, this module's own
        // test, pins the old defect). `a_maximal_length_match_at_the_last_core_byte_is_found_
        // whole` (this module's own windowed-model test) still pins the FINDING half of the
        // old comment's claim -- true both before and after this fix -- but never proved the
        // REFUTING half the old comment claimed too; this bound is what actually proves that.
        //
        // batch 5 (2026-07-26), finding #3: `+ CTX_AHEAD - 1` on top -- one byte past a
        // maximal-length candidate's own end is enough for `$`/ASCII `\b`, but a Unicode-aware
        // `(?u:\b)`/`(?u:\B)` needs the FULL following character, up to `CTX_AHEAD` bytes, not
        // one (this module's own top-level doc comment; `sweep_analysis_needs_the_full_trailing_
        // character_not_one_byte_at_the_read_limit`, this module's own test, pins the old defect:
        // a lone lead byte of `é` past the a-run decoded as non-word, fabricating `(?u:\b)`).
        //
        // batch 5 fix round (2026-07-26), review response, P3-2 -- stated accurately, not
        // overclaimed: THIS widening is a coherence property, not the correctness guard --
        // `safe_to`, below, is the sole gate an accepted candidate must pass, and reverting only
        // `read_to` (leaving `safe_to` correct) survives the whole suite, because a narrower
        // `read_to` merely shifts where a window's own seam falls (`report_to = read_to -
        // (MAX_MATCH_LEN + CTX_AHEAD - 1)` still exceeds `self.pos` before `core_end`, so progress
        // is preserved either way). Widening it anyway keeps "one window's own read always covers
        // its whole core in a single pass" true, and keeps the F8 comment's own `report_to ==
        // core_end` reasoning (just below) exactly true rather than merely close -- a belt, not
        // the buckle, alongside `safe_to`'s own guard.
        // restructure R4 (2026-07-27): `hay::Hay::consumable_reach()` centralizes this margin
        // (`MAX_MATCH_LEN + CTX_AHEAD - 1`) -- see that method's own doc comment.
        let read_to = core_end
            .saturating_add(crate::search::hay::Hay::consumable_reach().get() as u64)
            .min(size);
        // `self.carry` already holds this window's own leading bytes -- either the PREVIOUS
        // window's own lookahead read, physically contiguous with `self.pos` (windows never
        // skip a byte, except right after `give_up`, which clears `carry` for exactly this
        // reason), or, on a RETRY after THIS SAME window failed partway through, whatever it
        // already read (and already reported -- see below) before the failing block -- PLUS,
        // the first `ctx_len` bytes of look-behind context (this struct's own ANCHOR
        // CORRECTNESS doc comment), which are not themselves unread/unreported content and so
        // must not be counted as such here.
        let already_read = self.carry.len() - self.ctx_len;
        let mut read_pos = self.pos.saturating_add(already_read as u64);
        // batch 4 (2026-07-24), finding #10's own fix round (extension the brief's own
        // enumeration did not name -- discovered implementing the cap-1 -> cap change above,
        // not a corrected claim about it): the report-and-advance logic below used to live only
        // INSIDE `while read_pos < read_to`, so it ran exactly once per fresh block read and
        // never otherwise. That silently relies on a fresh read ALWAYS happening at least once
        // per window -- and CAN fail whenever `already_read` alone already satisfies `read_pos
        // >= read_to`: the loop body, report included, never runs at all, `self.pos` never
        // advances, `done` never becomes true, and the driver spins calling `step` forever.
        //
        // fix round (2026-07-25), F6 -- corrected from an earlier version of this comment, which
        // claimed the OLD (cap-1) `read_to` "could never exactly equal a `read_pos` already
        // fully supplied by a PREVIOUS window's own over-read," i.e. that this whole livelock
        // was NEW, introduced by widening `read_to` in this very fix round. Measured, not
        // argued: probed directly against a detached `ac9b5da` worktree (identical `read_to`/
        // `at_final`/`while` code to `7cd4df8`, i.e. shipped `main`, before this fix round ever
        // touched this file) -- the livelock is PRE-EXISTING, reached whenever `size mod
        // SEARCH_WINDOW` falls in `[1, MAX_MATCH_LEN - 1]` (roughly 6.25% of all file sizes
        // above 64 KiB, on ANY pattern), including the exact fixture the pre-existing `sweep_
        // analysis_finds_a_maximal_length_match_at_the_last_core_byte` test below already used
        // (`size = 2 * SEARCH_WINDOW + MAX_MATCH_LEN`, remainder exactly `MAX_MATCH_LEN`) --
        // which is WHY that one test alone caught this restructure's own absence as a hang, with
        // no separate regression test of its own before this fix round added one. Widening
        // `read_to` to a full `MAX_MATCH_LEN` did not CREATE this shape; it moved the band's own
        // top edge by exactly one byte (old `[1, MAX_MATCH_LEN - 1]`, new `[1, MAX_MATCH_LEN]`).
        // This restructure is therefore a materially BIGGER fix than "closes a gap this same
        // commit's own cap change opened" -- it closes a live hang reachable on shipped `main`,
        // on ordinary patterns, with no failing block and no truncation involved at all.
        //
        // Restated as a `loop` that checks-and-reports FIRST, using whatever `read_pos` already
        // is (freshly read or entirely carried over), THEN decides whether more reading is
        // needed: strictly more general than the old `while`, and a no-op on every call where
        // the pre-loop `read_pos` doesn't yet justify a new report (`report_to > self.pos` stays
        // false), so every previously-passing case is unaffected -- one extra, cheap check, not
        // a behavior change for them.
        loop {
            // report whatever is now SAFELY resolved -- incrementally, right after every
            // block, not deferred to the end of the whole window: a LATER block in the SAME
            // window failing (`give_up`) must not cost the matches an EARLIER, already-read
            // block already found (`sweep_gives_up_and_floors_on_a_failing_block`, document.
            // rs's own test, pins exactly this). "Safe" means either this window's own read
            // has reached the file's own TRUE end (`size` -- batch 4 (2026-07-24), finding #10:
            // reaching this window's own artificial `read_to` limit is NOT finality, only the
            // real end is; more bytes exist and will be read in a LATER window/call otherwise)
            // or there is already a full MAX_MATCH_LEN bytes of lookahead past it (this
            // module's own top-level doc comment); either way, never past this window's own
            // core (`core_end`) -- bytes beyond it belong to the NEXT window's own report.
            //
            // fix round (2026-07-25), F8: `at_final` reads as `read_pos >= size` alone here (the
            // OLD formula was `read_pos >= read_to || read_pos >= size`) -- a CLARIFICATION
            // riding the `read_to` widening just above, not a second, independently load-bearing
            // fix of its own: with `read_to = core_end + MAX_MATCH_LEN + CTX_AHEAD - 1` (batch 5
            // (2026-07-26), finding #3, widened again from a bare `MAX_MATCH_LEN`), mid-file
            // `read_pos == read_to` gives `safe_to = read_to = core_end + MAX_MATCH_LEN +
            // CTX_AHEAD - 1` (this branch) or `read_to - (MAX_MATCH_LEN + CTX_AHEAD - 1) =
            // core_end` (the other), and `report_to = min(safe_to, core_end) = core_end` EITHER
            // WAY; `read_pos > read_to` is unreachable (`take` is clamped by `read_to -
            // read_pos`); and `read_to < core_end + MAX_MATCH_LEN + CTX_AHEAD - 1` only when
            // clamped to `size`, which this simpler formula's own `read_pos >= size` clause
            // already covers on its own. Restoring the OLD, wider formula here changes nothing
            // observable (mutation-verified: the RED test for finding #10,
            // `sweep_analysis_does_not_treat_its_own_read_limit_as_a_real_boundary`, is pinned
            // entirely by the `read_to` change above, not by this line) -- kept in this
            // simplified form because it reads more clearly, not because the wider form was
            // wrong.
            let at_final = read_pos >= size;
            let safe_to = if at_final {
                read_pos
            } else {
                // batch 5 (2026-07-26), finding #3 / restructure R4: `hay::Hay::
                // consumable_reach()` -- a maximal-length candidate's own trailing assertion,
                // evaluated at `start + MAX_MATCH_LEN`, needs the FULL following character (up to
                // `CTX_AHEAD` real bytes), not merely the one byte finding #10 (batch 4) secured;
                // see this module's own top-level doc comment for the full derivation.
                read_pos.saturating_sub(crate::search::hay::Hay::consumable_reach().get() as u64)
            };
            let report_to = safe_to.min(core_end);
            if report_to > self.pos {
                // `lb` ("look-behind"): however many of `carry`'s own leading bytes are context
                // right now -- recomputed every report, not hoisted above the loop, since the
                // very first report of a sweep (`pos` still 0) is the one point `ctx_len` flips
                // 0 -> nonzero mid-`step`, and a later report in this SAME call must see that.
                let lb = self.ctx_len;
                let accept_end_before_widen =
                    lb + ((report_to - self.pos) as usize).min(self.carry.len() - lb);
                // restructure R4 (2026-07-27): A1's shape. ZERO-WIDTH AT THE TRUE EOF (batch 3
                // (2026-07-23), finding #9) and the PHANTOM LINE rule (batch 4 (2026-07-24),
                // finding #2) both now go through `Hay::accept_with_eof_widening` (this method's
                // own doc comment has the general form; `restr-R4-report.md` has the proof that
                // its own trigger, "`accept.end` sits at this hay's own `bytes.len()`," is
                // exactly `report_to >= size` here, not merely similar to it -- an ACCEPT-side
                // fact, distinct from the hay's own PHYSICAL reach the paragraph below needs).
                //
                // restructure R5 (2026-07-28): `high: True` iff THIS HAY'S OWN BYTES (`self.
                // carry`, not the accept-clamped `report_to`) reach the file's own true end --
                // `read_pos`, not `report_to`. The two coincide whenever `core_end >= size` (the
                // window containing true EOF), but NOT in general: `core_end` caps what THIS
                // report may ACCEPT (never past the window's own core, the sweep's own fresh-
                // starts-only invariant), while the underlying READ routinely reaches past it for
                // lookahead (`read_to = core_end + consumable_reach()`) -- enough, on a file whose
                // real end falls within `consumable_reach()` of `core_end`, to reach true EOF
                // while `core_end` itself still sits below `size`. `report_to >= size` would then
                // be permanently false (capped at `core_end < size`) even though `self.carry`
                // genuinely holds the last real byte -- exactly the gap `sweep_analysis_finds_a_
                // dollar_match_exactly_at_a_real_window_seam`'s own fixture reaches (found DURING
                // this restructure's own verification, not carried over from R4: `lookahead_ok`,
                // Part 2's own new consumer of `high`, is what first made this distinction
                // observable -- R4 never consulted `high` for an ordinary, non-widening accept).
                // `read_pos` and `self.pos - lb + self.carry.len()` are kept in lockstep by this
                // loop's own construction (both advance together, every read), so either
                // expression works; `read_pos` is already in scope.
                // restructure R7 (batch 8 (2026-07-29)): based where `carry` is TRACKED to
                // begin. `lb` stays the accept floor's own local index -- it IS `ctx_len` here,
                // and the sweep's fresh-starts-only invariant is expressed in local terms
                // throughout this report (`accept_end_before_widen` likewise), so this site keeps
                // its local spellings rather than round-tripping them through file coordinates
                // for no gain.
                let asm = Self::window_of(self.carry_base, &self.carry, lb, self.pos);
                let hay = asm
                    .hay()
                    .with_high(if read_pos >= size {
                        crate::search::hay::Edge::True
                    } else {
                        crate::search::hay::Edge::Cut
                    })
                    .with_accept(
                        crate::search::hay::Local(lb)
                            ..crate::search::hay::Local(accept_end_before_widen),
                    );
                let accept = hay.accept_with_eof_widening();
                let hay = hay.with_accept(accept.clone());
                // matches are collected first and recorded after: the scan below borrows
                // `self.carry` immutably for its whole duration (inside `hay`), and `record_
                // match` needs `&mut self` -- the two cannot overlap. `Hay::starts_verified`
                // (restructure R5) no longer floors the search by position at all -- it walks
                // from `accept.start` and applies `verified`'s own `lookbehind_ok` gate PER
                // CANDIDATE, waived entirely for a pattern that carries no leading assertion (the
                // F9 seam-cost retirement: a bare literal starting right after a `give_up` seam
                // has a trustworthy start regardless of what its own unknowable predecessor was,
                // since a hay cut cannot MANUFACTURE a body). An assertion-bearing pattern
                // (`(?u:\b)needle`) still pays the identical excluded-window cost R4 already
                // charged -- `sweep_give_up_seam_does_not_overcount_a_word_boundary_one_byte_
                // past_the_landing` and `sweep_give_up_seam_still_finds_a_word_boundary_outside_
                // the_excluded_window` (this module's own tests) pin that boundary unmoved.
                // `sweep_give_up_seam_the_f9_retirement_finds_a_bare_literal_across_multiple_tiny_reports`, by
                // contrast, uses a BARE LITERAL ("needle", no assertion at all) and is exactly
                // where the retirement bites -- its own expectation MOVES, direction RECOVERY
                // (this module's own movement table has the reasoning).
                let found: Vec<u64> = hay
                    .starts_verified(&pattern)
                    .into_iter()
                    .map(|s| self.pos + (s.0 - lb) as u64)
                    .collect();
                for match_at in found {
                    self.record_match(match_at, size);
                }
                // drop the reported prefix, EXCEPT its own last up to CTX_BEHIND bytes: those
                // become the NEW look-behind context (the real bytes immediately below the new
                // `pos`), replacing whatever `carry`'s own old leading `lb` bytes held before
                // (batch 4 (2026-07-24), finding #9: up to CTX_BEHIND real bytes, not just one --
                // a UTF-8 character is at most 4 bytes, so `(?u:\b)`/`(?u:\B)` need the WHOLE
                // preceding character, not a lone, possibly-invalid continuation byte). Clamped
                // by `self.carry.len()` (not just `accept.end`) so the true-EOF `+1` widening
                // above -- one past `carry`'s own real content -- can never make `ctx_len` claim
                // a byte that was never actually read; on that terminal report `ctx_len`/`carry`
                // end up irrelevant anyway (`done` publishes and a fresh `begin` resets both
                // before either is read again).
                let carry_len = self.carry.len();
                let new_ctx_len = accept.end.0.min(carry_len).min(CTX_BEHIND.0);
                let drop_to = accept.end.0.min(carry_len) - new_ctx_len;
                self.carry.drain(..drop_to);
                // restructure R7: the drain is the one mutation that moves this window's low end
                // -- `drop_to` bytes leave the front, so it begins that much further along. The
                // result stays flush against the new `pos` (`window`'s own assertion checks it):
                // `carry_base + accept.end` IS `report_to` by this report's own construction, and
                // `drop_to` is `accept.end - new_ctx_len`, so the new base lands exactly at
                // `report_to - new_ctx_len`.
                self.carry_base = self.carry_base + drop_to as u64;
                self.ctx_len = new_ctx_len;
                self.pos = report_to;
            }
            // batch 4 (2026-07-24 fix round), F2: `read_pos >= size` too -- once the `got.is_
            // empty()` branch below shrinks `size` down to the real end, this is what lets the
            // loop stop cleanly (one more report-check already ran, with `at_final` now correctly
            // true) instead of attempting another doomed read at the identical position.
            if read_pos >= read_to || read_pos >= size {
                break;
            }
            // restructure R3: routed through the same metered `Reader` every other loop uses
            // (design point 5, the driver regime) -- `Access::Peek` preserves the non-promoting
            // `warm()` this sweep always used (a background pass touching a block must not push
            // it into the protected segment, `BlockCache::warm`'s own doc comment); `Charge::
            // Payload` because this IS the sweep's own reportable content, unlike a ctx/lookahead
            // read elsewhere. `read_at_unbounded` folds the OLD manual `block_idx`/`lo`/`take`
            // slicing into the Reader's own `classify`, and -- an improvement, not a behavior
            // change the sweep depends on -- can certify a short block on its OWN first touch
            // (`Fetched::Short` with a non-empty `got`) where the old two-call dance needed a
            // second, now-redundant touch at the same `block_idx` to discover the same fact.
            let want = (read_to - read_pos) as usize;
            // built fresh right here, not hoisted above the loop: `record_match` (above, this
            // same loop body) needs `&mut self` as a whole, which the borrow checker cannot prove
            // disjoint from a `Reader` held across the loop -- a short-lived borrow, taken only
            // for this one call, sidesteps that entirely (the SAME shape `SearchForward`'s own
            // per-iteration ctx fetch, scan.rs, already uses for an analogous reason).
            let mut reader = crate::meter::Reader::new(&self.cache, &mut self.meter);
            let fetched = match reader
                .read_at_unbounded(
                    read_pos,
                    want,
                    crate::meter::Access::Peek,
                    crate::meter::Charge::Payload,
                )
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    // batch 3 (2026-07-23), finding #5b: record which block this read actually
                    // targeted, not just that SOME read in the current window failed -- `give_
                    // up`, if retries exhaust, needs the failing block's own index to skip past
                    // the whole thing rather than one window at a time (see `give_up`'s own
                    // comment). Every failing attempt (including retries) overwrites this with
                    // its own block index, so it always reflects the most recent failure.
                    self.last_failed_block = Some(read_pos / bs);
                    return Err(e);
                }
            };
            let got = match fetched {
                crate::meter::Fetched::Bytes { got, .. } => got,
                crate::meter::Fetched::Short { got, .. } => got,
                crate::meter::Fetched::Empty { .. } => bytes::Bytes::new(),
            };
            if got.is_empty() {
                // the truncation policy (docs/budgeted_scanning.md, fix round F2 -- the same
                // regime livelocks this sweep too, outside the original batch-4 finding
                // #14-internal's own literal `scan.rs` scope, but the brief asked to check the
                // sibling loop "while you're there"): a short/empty block means the source's
                // real data ends at `read_pos`, below the claimed `size`. Shrinking `size`
                // itself to `read_pos` makes every downstream formula in this SAME call --
                // `at_final` (in the report-check above, re-run once more via `continue`), the
                // zero-width-at-true-end widening (`report_to >= size`, now correctly
                // reachable), and `done` below -- uniformly treat `read_pos` as final, so the
                // FORWARD-direction policy's "run the final report pass, then stop" falls out
                // for free from the loop's own existing report-check-first structure (this
                // struct's own `step` doc comment on the `loop` restructure) rather than
                // needing a second, duplicated accept-check the way `SearchForward`'s single-
                // shot `take == 0` site does. The OLD code `break`-ed here unconditionally,
                // leaving `self.pos` wherever the last (possibly zero) report left it and
                // `done = self.pos >= size` false against the inflated claim -- `Ok(false)`
                // forever, the driver re-entering the identical state (probed: size claims
                // 20_000 over 200 real bytes, "needle" at 100 -- 500+ consecutive `Ok(false)`
                // with `pos == 0`, `matches == 0`, never finding the real, readable match).
                size = size.min(read_pos);
                continue;
            }
            self.carry.extend_from_slice(&got);
            read_pos += got.len() as u64;
        }
        let done = self.pos >= size;
        let _ = tx.send(self.snapshot(done));
        if done {
            Ok(crate::scan::Step::Done(()))
        } else {
            // the motion witness: every report inside the loop above only ever moves `self.pos`
            // FORWARD (`self.pos = report_to`, guarded by `report_to > self.pos`), so comparing
            // against `pos_at_entry` is the identical fact a persistent `Ascending` cursor would
            // prove (`pos_at_entry`'s own doc comment, above).
            let moved = crate::progress::Ascending::new(pos_at_entry, u64::MAX)
                .advance_to(self.pos)
                .expect(
                    "`done` is false, so this call did not reach `size`; every path that leaves \
                     this loop without reaching `size` already reported at least once (the \
                     loop's own report-check-first restructure, fix round F6/F8's own doc \
                     comment) or shrank `size` down to meet `self.pos` exactly (making `done` \
                     true instead) -- `self.pos` moving is the same fact `progress_marker` \
                     already relies on for the driver's own retry-reset logic",
                );
            Ok(crate::scan::Step::More(
                moved,
                self.meter.progress_witness()?,
            ))
        }
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
        // to exactly one.
        //
        // batch 4 (2026-07-24), finding #12: the OLD code additionally floored this at
        // `past_one_window` (`self.pos + SEARCH_WINDOW`), on the theory that a block SMALLER
        // than a window must still advance a full window -- but that theory was backwards: many
        // small blocks between the failing one and `past_one_window` can be perfectly healthy,
        // and flooring past all of them discarded every match in territory that was never
        // actually unreadable, undercounting far more than the one truly dead block justifies
        // (`sweep_give_up_only_skips_the_failing_block_not_a_whole_window_of_healthy_ones`, this
        // module's own test). Dropped entirely: `self.pos` advances past the failing block
        // ONLY. PROGRESS ARGUMENT (why this alone still can't livelock): the failing read that
        // set `last_failed_block` was attempted at some `read_pos >= self.pos` (`read_pos` only
        // ever grows from `self.pos`'s own starting point), and `read_pos` necessarily falls
        // WITHIN the failing block (`block_idx = read_pos / bs`), so the block's own END,
        // `past_failing_block = (last_failed_block + 1) * bs`, is strictly greater than
        // `read_pos`, which is itself `>= self.pos` -- `past_failing_block > self.pos` always,
        // so every `give_up` still advances `pos` by at least one byte; consecutive dead blocks
        // each cost their own bounded retry round and then their own single, honest skip, never
        // a second one for the same block. `.max(self.pos)` below is a defensive floor for the
        // "shouldn't happen" path only (see the next comment), not part of this argument.
        //
        // `last_failed_block` defaults to "no further than where we already are"
        // (`unwrap_or(self.pos)`, paired with the trailing `.max(self.pos)`) on the defensive,
        // shouldn't-happen path where `give_up` runs without a prior recorded failure -- the
        // driver never does this (it only calls `give_up` once `step` has already returned
        // `Err` `ANALYSIS_RETRIES + 1` times in a row, which always sets `last_failed_block`
        // first), so this is a no-regression floor, never a value this sweep expects to
        // actually use: without it, a hypothetical premature `give_up` would compute
        // `past_failing_block == 0` and silently move `pos` BACKWARD, which is worse than the
        // livelock a plain floor would have merely failed to prevent -- re-reporting territory
        // already counted.
        let bs = self.cache.block_size() as u64;
        let past_failing_block = self
            .last_failed_block
            .map(|idx| idx.saturating_add(1).saturating_mul(bs))
            .unwrap_or(self.pos);
        self.pos = past_failing_block.max(self.pos).min(self.cache.size());
        self.last_failed_block = None;
        // whatever `carry` held (this window's own partial core-plus-lookahead read, plus any
        // context bytes) is no longer contiguous with the NEW `pos` once the skip above lands
        // past it -- keeping it would let the NEXT window's own search see stale bytes
        // separated from `pos` by the skipped gap as if they were its own leading bytes, a
        // false match spanning territory this sweep never actually read as contiguous.
        //
        // batch 4 (2026-07-24), finding #4: the new `pos`'s own real predecessor is unknowable
        // without a read this method deliberately avoids -- but unlike the OLD sentinel
        // (`carry.push(0u8)`, a fixed non-`\n` byte meant to keep `^` conservative), no single
        // byte value is conservative for EVERY assertion: it kept `^` safely false (NUL is never
        // `\n`) but silently manufactured a false `\b` wherever the real, unread predecessor
        // happened to be a word character -- this module's own top-level doc comment's "no
        // sentinel" rule. `carry` starts genuinely EMPTY (`ctx_len == 0`, structurally identical
        // in WIDTH to true BOF, but not in POSITION -- restructure R5, 2026-07-28: `self.pos`
        // itself stays wherever `past_failing_block` landed, almost never 0, so `Hay::
        // low_is_true`'s own `base == Abs(0)` derivation correctly reads this as `Cut`, not
        // `True`, the next time a hay is built here -- see `step`'s own A1/A2 sites). That is
        // what marks the difference from true BOF now, replacing the deleted `seam_gap_remaining`
        // countdown (R4-era; proven inert by its own reviewer's mutation battery, M1-M4, both
        // before AND after this fact was expressed positionally instead): `lookbehind_ok`, the
        // predicate that actually consults it, needs no persisted state of its own to tell the
        // two apart -- `low_is_true` is `false` for as many following reports as `self.pos`
        // stays within `CTX_BEHIND` of this landing, the identical width the old counter counted
        // down, without a separate field to keep in sync.
        self.carry.clear();
        self.ctx_len = 0;
        // restructure R7: cleared alongside the buffer it describes -- the skip above landed
        // `self.pos` past whatever `carry` held, which is precisely why that content is dropped.
        self.carry_base = crate::search::hay::Abs(self.pos);
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

    // ---- leading_assertions / trailing_assertions: the recursive HIR walk (fix round, P1-1) ----

    #[test]
    fn leading_and_trailing_assertions_the_reviews_own_probe_table() {
        // fix round (2026-07-28), P1-1: every row of the review's own adversarial probe table
        // (`restr-R5-review.md`), pinned directly against the FIXED (recursive, nullability-aware)
        // walk. `**` marks the rows that discriminate the bug this fix closes -- the reverted
        // `look_set_*_any` classifier got these wrong (see this module's own `SearchPattern::
        // leading_assertions` field doc comment for the mechanism).
        let cases: &[(&str, bool, bool)] = &[
            ("^a", true, false),
            ("a$", false, true),
            (r"a|^b", true, false),
            (r"foo|bar$", false, true),
            (r"a\bb", false, false), // strictly interior -- neither end
            (r"(?:^a)?b", true, false),
            (r"a{0}^b", true, false), // ** min==max==0: continues past
            (r"x(?:$|y)", false, true),
            (r"a*^b", true, false),     // ** emptiable neighbour, was false
            (r"a?^b", true, false),     // ** emptiable neighbour, was false
            (r"a{0,3}^b", true, false), // ** emptiable neighbour, was false
            (r"(?:a|)^b", true, false), // ** emptiable neighbour, was false
            (r"(a?)^b", true, false),   // ** emptiable neighbour, was false
            (r"b$a*", false, true),     // ** emptiable neighbour, was false
            (r"b$a?", false, true),     // ** emptiable neighbour, was false
            (r"a\bb*", false, true),    // ** emptiable neighbour, was false
            (r"b\ba*", false, true),    // ** emptiable neighbour, was false
            (r"[a-z]*^x", true, false), // ** emptiable neighbour, was false
            // restructure R6 rider (2026-07-28), credited to `restr-R5-review.md`'s own P3-6:
            // the trailing-repetition arm's missing pin. `hir_trailing_look`'s `Repetition` arm
            // (`HirKind::Repetition(rep) => hir_trailing_look(&rep.sub)`) descends into a `min >=
            // 1` repetition whose own look is a SIBLING of consuming content, not a harmless
            // `min == 0` case (those can always match zero times, so the reported start is the
            // same either way) -- before this row existed, deleting the arm's own descent
            // (`=> false`) left the whole 529-test suite green; the review's own mutation battery
            // (M-TRAILING-REP) found the gap by attacking every arm in turn, not by inspection.
            (r"b(?:a$){1,2}", false, true),
        ];
        for &(pat, want_lead, want_trail) in cases {
            let p = SearchPattern::compile(pat, false).unwrap();
            assert_eq!(
                p.leading_assertions(),
                want_lead,
                "{pat:?}: leading_assertions"
            );
            assert_eq!(
                p.trailing_assertions(),
                want_trail,
                "{pat:?}: trailing_assertions"
            );
        }
    }

    #[test]
    fn leading_and_trailing_assertions_are_false_for_every_assertion_free_recovery_pattern() {
        // the movement table's own three named recovery patterns, plus the bare literal --
        // confirms the fixed walk keeps every recovery it is supposed to (P1-1's own fix must not
        // regress the unit's own primary deliverable while closing the emptiable-neighbour gap).
        for pat in ["a+", ".*", "[a-z]+", "needle"] {
            let p = SearchPattern::compile(pat, false).unwrap();
            assert!(
                !p.leading_assertions(),
                "{pat:?}: must have no leading assertion"
            );
            assert!(
                !p.trailing_assertions(),
                "{pat:?}: must have no trailing assertion"
            );
        }
    }

    #[test]
    fn dot_star_recovers_under_the_fixed_utf8_flag() {
        // fix round (2026-07-28), P1-2 -- RED, written FIRST against the un-mirrored `utf8(true)`
        // default: `.`, `.*`, `[^x]`, `\S`, `\W`, `\D` all fail the second (syntax-only) parse
        // under `unicode(false)` when `regex_syntax::ParserBuilder`'s own default `utf8(true)`
        // rejects a HIR that could match invalid UTF-8 -- `regex::bytes::RegexBuilder` places no
        // such restriction on the FIRST (real) parse, so every one of these compiled patterns
        // silently took the `(true, true)` conservative fallback, meaning `.*` -- named as the
        // flagship assertion-free recovery pattern in five doc/code sites -- never actually
        // recovered. `.utf8(false)` on the `ParserBuilder` (confirmed against `regex::bytes`'s
        // own internal `build_many_bytes`, which does exactly this) closes it.
        for pat in [".", ".*", "[^x]", r"\S+", r"\W", r"\D", "a.*b", "(?s:.)"] {
            let p = SearchPattern::compile(pat, false).unwrap();
            assert!(
                !p.leading_assertions(),
                "{pat:?}: must parse and classify as assertion-free (leading), not silently fall \
                 back to the conservative default"
            );
            assert!(
                !p.trailing_assertions(),
                "{pat:?}: must parse and classify as assertion-free (trailing), not silently fall \
                 back to the conservative default"
            );
        }
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

    /// batch 7 (2026-07-28), findings #1/#3: the load-bearing claim the whole fix rests on. A
    /// library's documented contract is a claim to TEST (restructure R5's own keeper, learned
    /// when `regex_syntax`'s `look_set_prefix_any` turned out not to deliver what it promised) --
    /// so this pins BOTH halves of `Input::span`'s contract, in the direction that would silently
    /// reintroduce the fabrication class if either ever changed.
    #[test]
    fn span_bounds_the_end_without_faking_either_boundary() {
        // half 1: the span really does cap the END -- a greedy body comes back SHORTER rather
        // than overrunning. This is what replaces "reject the overrun, then retry at s + 1".
        let p = SearchPattern::compile("a+", false).unwrap();
        let hay = b"aaaaXbbbb";
        assert_eq!(p.find_from(hay, 0), Some((0, 4)), "unbounded: greedy to 4");
        assert_eq!(
            p.find_ending_by(hay, 0..3),
            Some((0, 3)),
            "the span caps the end, yielding the completion that fits"
        );
        // half 2: assertions at BOTH span edges are decided against the real bytes outside it.
        // Each of these would MATCH if `span` were equivalent to slicing the haystack -- which is
        // exactly the "a slice's own edge is unconditionally eligible" fabrication this module
        // exists to forbid, so a `span` that behaved like a slice would be strictly worse than
        // the retry loop it replaces, not better.
        let trailing = SearchPattern::compile(r"a(?u:\b)", false).unwrap();
        assert_eq!(trailing.find_from(b"ab", 0), None, "ground truth: no \\b");
        assert_eq!(
            trailing.find_ending_by(b"ab", 0..1),
            None,
            "\\b at the span's own end must see the real successor 'b', not an artificial end"
        );
        let leading = SearchPattern::compile(r"(?u:\b)y", false).unwrap();
        assert_eq!(leading.find_from(b"xy", 0), None, "ground truth: no \\b");
        assert_eq!(
            leading.find_ending_by(b"xy", 1..2),
            None,
            "\\b at the span's own start must see the real predecessor 'x', not an artificial start"
        );
        let dollar = SearchPattern::compile("a$", false).unwrap();
        assert_eq!(
            dollar.find_ending_by(b"abc", 0..1),
            None,
            "$ must not fire at an artificial span end"
        );
        // and the span's own START still bounds where a match may begin, unchanged.
        let lit = SearchPattern::compile("a", false).unwrap();
        assert_eq!(lit.find_ending_by(b"aXa", 1..3), Some((2, 3)));
    }

    /// batch 7 (2026-07-28): the engine swap (`regex::bytes::Regex` -> `regex_automata::meta::
    /// Regex`) is an API change, not a semantics change -- pinned DIFFERENTIALLY against the old
    /// frontend rather than argued from the fact that the two share an implementation. The old
    /// builder is constructed here with the exact flags `compile` used to pass it, so this test
    /// is the retired code path kept alive as an independent oracle: if the two ever diverge on
    /// any pattern/haystack pair below, this fails rather than some distant scan test.
    #[test]
    fn the_meta_frontend_answers_what_the_regex_crate_frontend_answered() {
        let pats = [
            "a+",
            "x+a+",
            ".*",
            ".",
            "^$",
            "^",
            "$",
            r"(?u:\b)needle",
            r"a{4096}(?u:\b)",
            "needle",
            "foobar|foo",
            r"\bword\b",
            "(?i)ABC",
            r"[A-Za-y]*",
            r"a+(?u:\b)\s*",
            r"\n",
            r"[^x]+",
            r"\S+",
            "é",
            r"(?u).",
            "Xy",
        ];
        let hays: [&[u8]; 11] = [
            b"",
            b"aaaa",
            b"a needle here",
            b"xxxaaa",
            b"\n\n\n",
            b"foobar",
            b"word boundary word",
            b"ABC abc AbC",
            "héllo wörld".as_bytes(),
            b"a\xffb",
            b"Xy Xy Xy",
        ];
        for raw in pats {
            let new = SearchPattern::compile(raw, false).unwrap();
            let sensitive = raw.chars().any(|c| c.is_uppercase());
            let old = regex::bytes::RegexBuilder::new(raw)
                .case_insensitive(!sensitive)
                .multi_line(true)
                .unicode(false)
                .build()
                .unwrap();
            for hay in hays {
                assert_eq!(
                    new.find_all(hay),
                    old.find_iter(hay)
                        .map(|m| (m.start(), m.end()))
                        .collect::<Vec<_>>(),
                    "find_all diverged for pattern {raw:?} over {hay:?}"
                );
                for at in 0..=hay.len() {
                    assert_eq!(
                        new.find_from(hay, at),
                        old.find_at(hay, at).map(|m| (m.start(), m.end())),
                        "find_from diverged for pattern {raw:?} over {hay:?} at {at}"
                    );
                }
            }
        }
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

    // ---- find_all_starting_in (batch 4 (2026-07-24), finding #6) ----

    #[test]
    fn find_all_starting_in_returns_every_non_overlapping_match_within_the_span() {
        let p = SearchPattern::compile("xx", false).unwrap();
        let hay = b"..xx....xxYxx..";
        //          0123456789012345
        // "xx" at 2,8,11 -- accept 2..13 should find 2 and 8 and 11 (11's own end 13 <= 13).
        assert_eq!(
            p.find_all_starting_in(hay, 2..13),
            vec![(2, 4), (8, 10), (11, 13)]
        );
    }

    #[test]
    fn find_all_starting_in_excludes_a_start_before_the_accept_range() {
        let p = SearchPattern::compile("xx", false).unwrap();
        let hay = b"xx..xx";
        // the leading "xx" (0,2) starts before accept.start(3) -- excluded; only (4,6) counts.
        assert_eq!(p.find_all_starting_in(hay, 3..6), vec![(4, 6)]);
    }

    #[test]
    fn find_all_starting_in_excludes_a_match_extending_past_the_accept_range() {
        let p = SearchPattern::compile("xxx", false).unwrap();
        let hay = b"..xxx..";
        // "xxx" spans [2,5); accept ends at 4 -- the match needs byte 4, past accept.end, so it
        // is a documented miss (this method's own "wholly contained" rule), not reported.
        assert_eq!(
            p.find_all_starting_in(hay, 0..4),
            Vec::<(usize, usize)>::new()
        );
    }

    #[test]
    fn find_all_starting_in_keeps_looking_past_an_overrun_leftmost_alternative() {
        // fix round (2026-07-25), F3: `xyz|y` over `b"xyz"`, `accept = 0..2` ('z' is
        // ctx-after) -- the leftmost-first `xyz` branch overruns `accept.end` and used to
        // `break` immediately, silently dropping the wholly-contained `y` match at `1..2`
        // (ground truth over the payload `b"xy"`: `find_all` itself reports exactly this).
        let p = SearchPattern::compile("xyz|y", false).unwrap();
        let hay = b"xyz";
        assert_eq!(p.find_all_starting_in(hay, 0..2), vec![(1, 2)]);
    }

    #[test]
    fn find_all_starting_in_finds_a_match_ending_exactly_at_the_accept_boundary() {
        let p = SearchPattern::compile("xxx", false).unwrap();
        let hay = b"..xxx..";
        // "xxx" spans [2,5); accept ends at 5 (its own END coincides exactly with accept.end)
        // -- this is the "wholly contained, inclusive of a match ending exactly at the edge"
        // case that must NOT be treated as extending past it (the off-by-one this method's own
        // implementation must get right for a real end-of-line match to highlight at all).
        assert_eq!(p.find_all_starting_in(hay, 0..5), vec![(2, 5)]);
    }

    #[test]
    fn find_all_starting_in_context_informs_assertions_without_being_reported() {
        // the exact defect class finding #6 fixes: `$` must see the REAL byte past the accept
        // range (here, a real 'a', not \n/EOF) to correctly refuse a false end-of-line match --
        // ctx bytes are visible to the regex for look-around, but never themselves a start.
        let p = SearchPattern::compile("foo$", false).unwrap();
        let hay = b"xxfooa"; // "foo" is the payload; the real 'a' (ctx-after) disproves $
        assert_eq!(
            p.find_all_starting_in(hay, 2..5),
            Vec::<(usize, usize)>::new(),
            "a real byte follows; $ must not falsely hold at the accept range's own edge"
        );
        let p2 = SearchPattern::compile("^foo", false).unwrap();
        let hay2 = b"x\nfooo"; // real \n at index 1 (ctx-before); "foo" payload starts at 2
        assert_eq!(
            p2.find_all_starting_in(hay2, 2..6),
            vec![(2, 5)],
            "the real \\n before the accept range must let ^ hold"
        );
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
    /// Also carries real look-behind bytes across the window seam (`ctx`, up to `CTX_BEHIND`
    /// of them), mirroring `SweepAnalysis::step`'s own permanent context (its ANCHOR
    /// CORRECTNESS doc comment): without it, every window after the first would search a
    /// slice whose own index 0 is an artificial edge, unconditionally `^`-eligible regardless
    /// of the real preceding byte (`SearchPattern::compile`'s own module-level doc comment) --
    /// a no-op for the non-anchored patterns this model was first written for, but
    /// load-bearing for `windowed_scan_equals_whole_buffer_scan_anchored`, below (batch 4
    /// (2026-07-24), finding #9's own width -- widened from a single byte here too, so this
    /// abstract model stays an honest stand-in for what production actually does).
    ///
    /// batch 5 fix round (2026-07-26), review response, P3-3: the trailing `ext` reach is widened
    /// by `CTX_AHEAD - 1` past `overlap` too, for the identical reason -- a maximal-length
    /// candidate's own trailing assertion needs the FULL following character, not one byte, and
    /// this model's own `overlap` stood in for `MAX_MATCH_LEN` alone before finding #3, same gap.
    /// Derived from the production CONSTANT (`CTX_AHEAD`), not the production FORMULA, so this
    /// stays an independent check rather than one that could cancel a systematic bug the same way
    /// twice: a bug in production's OWN `+ CTX_AHEAD - 1` arithmetic would not also be baked into
    /// this literal `+ CTX_AHEAD - 1` here, since the two are computed by entirely separate code.
    fn windowed_scan(
        p: &SearchPattern,
        hay: &[u8],
        window: usize,
        overlap: usize,
    ) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut start = 0usize;
        let mut ctx: Vec<u8> = Vec::new();
        while start < hay.len().max(1) {
            let end = (start + window).min(hay.len());
            let ext = (end + overlap + CTX_AHEAD.0 - 1).min(hay.len());
            let core = &hay[start..ext];
            let lb = ctx.len();
            let mut slice = Vec::with_capacity(lb + core.len());
            slice.extend_from_slice(&ctx);
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
            // the next window's own look-behind is up to CTX_BEHIND real bytes ending at
            // `end` -- always fully in range since `end > start` and this whole model has
            // unrestricted access to `hay` (there is no give_up-style unreadable region in
            // this abstract simulation, so no "no sentinel" exclusion is needed here either).
            let ctx_start = end.saturating_sub(CTX_BEHIND.0);
            ctx = hay[ctx_start..end].to_vec();
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

    // The anchored counterpart of the oracle above: `^`/`$`/`\b`/`\B` need real boundary
    // context (`SearchPattern::compile`'s own doc comment, `search.rs`'s own top-level doc
    // comment), so the oracle needs real line structure AND real word/non-word transitions --
    // a haystack built from LINES of small tokens (mostly "tok", the search target, so hits are
    // dense; occasionally other lowercase filler; occasionally "é" -- a 2-byte UTF-8 word
    // character, batch 4 (2026-07-24), finding #9's own shape -- so `(?u:\b)` sometimes sits
    // immediately adjacent to a multi-byte character, not just ASCII), joined by spaces,
    // `\n`-terminated. Compares the windowed model (now anchor-aware -- `windowed_scan`'s own
    // doc comment) against a WHOLE-BUFFER `find_all` for `^tok`/`tok$`/`\btok`/`tok\b`/
    // `(?u:\b)tok`/`tok(?u:\b)`, restricted to sub-cap matches as above.
    proptest::proptest! {
        #[test]
        fn windowed_scan_equals_whole_buffer_scan_anchored(
            lines in proptest::collection::vec(
                proptest::collection::vec(
                    proptest::prelude::prop_oneof![
                        3 => proptest::strategy::Just("tok".to_string()),
                        2 => "[a-z]{1,6}",
                        1 => proptest::strategy::Just("é".to_string()),
                    ],
                    1..6,
                ),
                0..60,
            ),
            window in 8usize..512,
            pattern_choice in 0usize..6,
        ) {
            let mut hay = Vec::new();
            for line in &lines {
                hay.extend_from_slice(line.join(" ").as_bytes());
                hay.push(b'\n');
            }
            let pattern = match pattern_choice {
                0 => "^tok",
                1 => "tok$",
                2 => r"\btok",
                3 => r"tok\b",
                4 => r"(?u:\b)tok",
                _ => r"tok(?u:\b)",
            };
            let p = SearchPattern::compile(pattern, false).unwrap();
            let overlap = 64usize; // stand-in MAX_MATCH_LEN, as above; "tok"/"^tok"/"tok$" never exceed it
            // batch 4 (2026-07-24), finding #2: `naive`, this oracle's own EXPECTED side, is
            // `find_all` -- pure regex, deliberately never phantom-aware (this struct's own
            // GUARDRAIL doc comment: "find_all stays pure regex, the whole-buffer oracle").
            // Production (`SweepAnalysis`/`SearchForward`/`SearchBackward`) now REJECTS a
            // zero-width match at `hay.len()` whose real predecessor is `\n` (the trailing
            // newline's own phantom, never a match anywhere) -- an oracle comparing production's
            // own windowed output against raw `find_all` would need to filter that phantom out of
            // `naive` too, or a haystack ending in `\n` with a genuinely zero-width pattern (bare
            // `$`, `^$`) would diverge. NONE of the six patterns here can ever trigger it: every
            // one requires the literal 3 real bytes of "tok" to match at all (the `\b`/`(?u:\b)`
            // variants -- batch 4 (2026-07-24), finding #9's own addition to this oracle -- are
            // zero-width ASSERTIONS glued to that same literal, not zero-width matches of their
            // own), so none is ever zero-width, and `hay.len()` (a real match's own end) can
            // therefore never equal `hay.len()` AS A ZERO-WIDTH START the way the phantom needs --
            // no filter is added here for exactly that reason (a `.filter()` that can never fire
            // would be dead code, not a correctness statement); `windowed_scan_equals_whole_buffer_
            // scan`, above (a non-anchored, always-nonzero-width pattern), needs no filter for the
            // same reason and none is added there either. A future draw of a genuinely zero-width
            // anchored pattern here (bare `$`/`^$`) WOULD need this filter -- see
            // `sweep_analysis_does_not_count_a_zero_width_match_on_the_phantom_line` and
            // `search_forward_rejects_a_zero_width_match_whose_predecessor_is_a_phantom_newline`
            // for the deterministic fixtures pinning that exact rule instead.
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
            if matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::Done(())) {
                break;
            }
        }
        rx.borrow().clone()
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_livelock_on_a_size_landing_in_the_pre_existing_livelock_band()
    {
        // fix round (2026-07-25), F6's own direct regression test -- the restructure above
        // (`loop` that checks-and-reports before deciding whether to read more) fixes a
        // PRE-EXISTING hang, reachable on shipped `main` before this whole unit ever touched
        // this file (this struct's own `step` doc comment on the restructure has the full
        // measured derivation): any `size` whose own remainder mod `SEARCH_WINDOW` falls in
        // `[1, MAX_MATCH_LEN]` triggers it, on an ordinary pattern, no failing block and no
        // truncation involved. `size = SEARCH_WINDOW + 100` is the review's own minimal direct
        // fixture (remainder 100, squarely inside the band) -- before this test, the ONLY thing
        // pinning the restructure at all was a much larger, pre-existing fixture (`sweep_
        // analysis_finds_a_maximal_length_match_at_the_last_core_byte`, remainder exactly
        // `MAX_MATCH_LEN`) that happened to also land in the band, not a fixture chosen to
        // exercise it directly. Bounded step count, not a real hang -- reaching the bound
        // without ever completing IS the livelock, deterministically, on any host.
        use crate::analyzer::Analysis;
        let data = vec![b'x'; SEARCH_WINDOW + 100];
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(crate::source::MockSource::new(data)),
            37,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let (tx, rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        let mut steps = 0usize;
        loop {
            steps += 1;
            assert!(
                steps <= 10_000,
                "livelock: {steps} consecutive Ok(false), no progress (pos stuck at {})",
                sweep.pos
            );
            if matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::Done(())) {
                break;
            }
        }
        let summary = rx.borrow().clone();
        assert_eq!(summary.matches, 0, "no \"needle\" planted in this fixture");
        assert!(summary.done);
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
    async fn sweep_analysis_does_not_fabricate_an_over_cap_word_boundary_unicode() {
        // restructure R5 (2026-07-28), the unit-J class -- RED, written FIRST against 05a2a2e:
        // F-review's own P2-4 fixture (`.superpowers/sdd/batch5-unit-F-review.md`, independently
        // re-verified in the F2 re-review). `a+(?u:\b)` over a run of 20,000 'a's followed by a
        // real "é" (a Unicode WORD character, 2 bytes): no real `(?u:\b)` exists anywhere in this
        // file -- transitioning from 'a' (word) to 'é' (also word) is never a boundary, at any
        // prefix length of the `a+` run. Pre-R5, the accept rule is START-based only: a candidate
        // whose start clears the lookahead margin (`s + MAX_MATCH_LEN + CTX_AHEAD - 1 <=
        // safe_to`) is accepted regardless of where its own END falls -- and an over-cap `a+`
        // greedily consumes up to whatever the CURRENT read happens to hold, landing its own end
        // at that read's own artificial edge long before the real 'é' is ever fetched; `\b`
        // evaluated there sees "nothing past the cut" and fires, exactly as it would at a real
        // boundary. Reported 12,185-vs-0 at the sha this brief cites; this test pins the ORACLE
        // answer (0) as what the sweep must also report once fixed.
        let mut data = vec![b'x'; 100];
        data.extend(vec![b'a'; 20_000]);
        data.extend("é".as_bytes());
        data.extend(vec![b'z'; 200]);
        let pattern = SearchPattern::compile(r"a+(?u:\b)", false).unwrap();
        assert_eq!(
            pattern.find_all(&data),
            Vec::<(usize, usize)>::new(),
            "oracle: é is a word character, so no real (?u:\\b) exists after the a-run at any \
             prefix length"
        );
        let summary = run_sweep_to_completion(data, 4096, r"a+(?u:\b)").await;
        assert_eq!(
            summary.matches, 0,
            "must agree with the oracle -- no real (?u:\\b) exists anywhere in this file; a \
             nonzero count here is the over-cap fabrication reading an artificial window edge as \
             a real boundary"
        );
        assert!(summary.done);
        assert!(!summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_fabricate_an_over_cap_word_boundary_ascii() {
        // restructure R5 (2026-07-28): the pure-ASCII mirror of the fixture above -- F2's own
        // re-review independently reproduced this variant and found it STRONGER than the Unicode
        // one (no `(?u:...)` opt-in needed at all; plain `\b` -- ASCII by default, `SearchPattern::
        // compile`'s own doc comment -- triggers the identical fabrication). `a+\b` over a run of
        // 20,000 'a's followed by a real 'b' (an ASCII WORD byte, distinct from 'a' so the `a+`
        // run cannot simply extend into it): no real boundary exists, word ('a') to word ('b').
        let mut data = vec![b'x'; 100];
        data.extend(vec![b'a'; 20_000]);
        data.push(b'b');
        data.extend(vec![b'z'; 200]);
        let pattern = SearchPattern::compile(r"a+\b", false).unwrap();
        assert_eq!(
            pattern.find_all(&data),
            Vec::<(usize, usize)>::new(),
            "oracle: 'b' is an ASCII word byte, so no real \\b exists after the a-run at any \
             prefix length"
        );
        let summary = run_sweep_to_completion(data, 4096, r"a+\b").await;
        assert_eq!(
            summary.matches, 0,
            "must agree with the oracle -- no real \\b exists anywhere in this file; a nonzero \
             count here is the over-cap fabrication reading an artificial window edge as a real \
             boundary"
        );
        assert!(summary.done);
        assert!(!summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_fabricate_an_over_cap_word_boundary_through_an_emptiable_tail()
    {
        // fix round (2026-07-28), P1-1 -- RED, written FIRST against the reverted (unsound)
        // classifier: identical to `sweep_analysis_does_not_fabricate_an_over_cap_word_boundary_
        // unicode`'s own fixture (100 'x' + 20,000 'a' + real "é" + 200 'z'), pattern `a+(?u:\b)\s*`
        // -- an ordinary, unremarkable thing for a user to type (a word-boundary search with
        // trailing whitespace tolerance). `\s*` can match empty, so it changes NOTHING about
        // where real matches exist -- the oracle is still 0, identically. Under `Properties::
        // look_set_suffix_any` (this unit's own original, reverted classifier), the emptiable
        // `\s*` tail stopped the walk BEFORE reaching `(?u:\b)`, hiding it and reporting
        // `trailing_assertions() == false` -- the exact fabrication `sweep_analysis_does_not_
        // fabricate_an_over_cap_word_boundary_unicode` (this module's own sibling test) already
        // kills for the bare pattern reopens here, verbatim (12,185 against oracle 0, review-
        // measured against `2fe1a9a` before this fix). The recursive walk (`hir_trailing_look`)
        // correctly continues past `\s*` (nullable, `minimum_len() == Some(0)`) to find the real
        // `(?u:\b)` behind it.
        let mut data = vec![b'x'; 100];
        data.extend(vec![b'a'; 20_000]);
        data.extend("é".as_bytes());
        data.extend(vec![b'z'; 200]);
        let pattern = SearchPattern::compile(r"a+(?u:\b)\s*", false).unwrap();
        assert_eq!(
            pattern.find_all(&data),
            Vec::<(usize, usize)>::new(),
            "oracle: é is a word character, so no real (?u:\\b) exists after the a-run at any \
             prefix length -- the trailing \\s* changes nothing about where real matches exist"
        );
        let summary = run_sweep_to_completion(data, 4096, r"a+(?u:\b)\s*").await;
        assert_eq!(
            summary.matches, 0,
            "must agree with the oracle -- an emptiable \\s* tail must not hide the real (?u:\\b) \
             behind it from the end rule; a nonzero count here is the P1-1 fabrication"
        );
        assert!(summary.done);
        assert!(!summary.failed);
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
    async fn sweep_analysis_does_not_count_a_zero_width_match_on_the_phantom_line() {
        // batch 4 (2026-07-24), finding #2: b"a\n" (size 2), pattern "$", matches at 1 (real,
        // before the \n) AND 2 (regex ground truth -- the true end) -- but 2 is the trailing
        // newline's own phantom (its real predecessor, byte 1, IS \n), never a match, anywhere
        // (the doctrine architecture.md:418-421 already states for `goto_line`, unified here).
        // The sweep must count exactly the one REAL match, agreeing with nav
        // (`search_next_forward_wraps_to_self_when_the_only_other_candidate_is_the_phantom`,
        // document.rs) -- not the two a naive `find_all` count would give.
        let summary = run_sweep_to_completion(b"a\n".to_vec(), 8, "$").await;
        assert_eq!(
            summary.matches, 1,
            "only the real match at 1, not the phantom at 2"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_finds_the_zero_width_match_on_an_empty_file() {
        // batch 4 (2026-07-24), finding #8: `step`'s own read loop (`while read_pos < read_to`)
        // never iterates when `size == 0` (`read_to == 0` too), so no report -- and no EOF
        // zero-width probe -- ever ran; nav (`search_next`) and the highlighter (`find_all`)
        // already agree an empty file's "^$" matches once, at 0 (no preceding byte -- #2's own
        // rule needs a real \n to call a position phantom, and position 0 has no predecessor at
        // all). Pre-fix this published `done: true, matches: 0`.
        let summary = run_sweep_to_completion(Vec::new(), 8, "^$").await;
        assert_eq!(summary.matches, 1);
        assert!(summary.done);
        assert_eq!(summary.generation, 1);
    }

    #[tokio::test]
    async fn sweep_analysis_finds_no_empty_line_on_a_single_terminated_line() {
        // finding #8's own companion truth test: b"a\n" has exactly one real line, "a", and no
        // EMPTY line -- "^$" must count 0. Position 2 (size) IS a zero-width "^$" candidate in
        // raw regex ground truth (`^` holds right after the \n at 1; `$` holds at the true end),
        // but its predecessor (byte 1) is \n -- finding #2's own phantom rule rejects it, the
        // same way nav's `line_start_shortcut` refuses to mint `Anchor(size)` for it. This is
        // the SAME fixture the primary test above uses, reached via a DIFFERENT path (`give_up`
        // is not involved here; the file is fully readable) -- both must agree the phantom line
        // does not exist.
        let summary = run_sweep_to_completion(b"a\n".to_vec(), 8, "^$").await;
        assert_eq!(
            summary.matches, 0,
            "no empty line exists; position 2 is the phantom"
        );
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
    async fn give_up_does_not_fake_a_line_start_right_after_the_skipped_block() {
        // regression (found during batch 3, while fixing two others): `give_up`'s FIRST fix
        // attempt reset `has_ctx` to false rather than seeding a conservative sentinel -- which
        // does not merely leave the new `pos`'s own predecessor unknown, it re-opens the same
        // "hay's own edge is unconditionally ^-eligible" hole for the position right after the
        // skip, producing a false MATCH there (never the honest miss `give_up`'s own doc comment
        // claims). batch 4 (2026-07-24), finding #4 replaced the sentinel with `seam_gap`'s own
        // positional exclusion (this struct's own top-level doc comment); finding #12 dropped
        // the `past_one_window` floor, landing give_up right after the failing BLOCK instead of
        // a whole SEARCH_WINDOW later -- this test's own fixture is renamed and its "needle"
        // replanted at the NEW landing spot accordingly (was: `..._right_after_the_skipped_
        // window`, needle at `SEARCH_WINDOW`; the old landing no longer exists). A source that
        // fails every block below the block boundary forces `give_up` directly (mirrors
        // document.rs's own `sweep_gives_up_and_floors_on_a_failing_block` fixture); "needle"
        // sits right at the start of the (never-read) failing block's own successor, with NO \n
        // anywhere -- `^needle` must not match it.
        use crate::analyzer::Analysis;
        // block 0's own region ([0, 4096)) always fails (forcing `give_up` on it); everything
        // from block 1 onward reads fine, with "needle" planted right at ITS OWN start (4096,
        // finding #12's own corrected landing -- past the failing BLOCK only, not a window) and
        // NO \n anywhere.
        struct FailsOnlyBlock0;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock0 {
            fn size(&self) -> u64 {
                (SEARCH_WINDOW * 3) as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|offset, len| {
                    Box::pin(async move {
                        if offset < 4096 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let mut block = vec![b'x'; len];
                            if offset == 4096 {
                                block[0..6].copy_from_slice(b"needle");
                            }
                            Ok(bytes::Bytes::from(block))
                        }
                    })
                })
            }
        }
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyBlock0),
            4096,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("^needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        // block 0's very first read fails outright, so `pos` is still 0 (nothing was ever
        // successfully reported) when `give_up` runs directly -- exercising its own effect
        // without depending on the driver's retry/backoff timing.
        assert!(sweep.step(&tx).await.is_err(), "block 0 must fail to read");
        assert_eq!(sweep.pos, 0, "nothing was reported before the failing read");
        sweep.give_up(&tx);
        // finding #12: give_up now lands exactly past the ONE failing 4096-byte block, not a
        // whole 64 KiB SEARCH_WINDOW later.
        assert_eq!(sweep.pos, 4096, "give_up must skip only the failing block");
        // "needle" is planted at the new `pos` itself -- but give_up never READS block 0, so
        // its own real predecessor (byte 4095) cannot be confirmed; `seam_gap`'s own one-shot
        // exclusion must keep ^needle from matching regardless, even though the very next
        // read (now normal) succeeds and finds "needle" itself just fine.
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 0,
            "give_up must not fake a line start it never verified"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_give_up_does_not_confirm_a_match_needing_lookahead_into_the_failing_block() {
        // batch 5 (2026-07-26), finding #8's own "compose with #3" bullet, CONFIRMED rather than
        // assumed: a match ENDING near a failing block -- needing lookahead INTO it to verify its
        // own trailing assertion -- must be excluded too, symmetric with the look-behind seam
        // this whole finding widens. Already true, via a DIFFERENT, pre-existing mechanism: `safe_
        // to` (finding #10/#3) never marks ANY candidate safe-to-report until enough real
        // lookahead has actually been read past it, and `give_up` unconditionally clears `carry`
        // (this method's own doc comment) -- so a match still sitting in the not-yet-safe carry
        // when a read fails is discarded along with it, never reported, the SAME "documented miss,
        // never a fabrication" floor this whole struct promises, not a gap #8's own fix needs to
        // touch. "needle" ends exactly at block 1's own start (block 0 healthy, block 1 -- and
        // ONLY block 1 -- always fails; block 2 onward is healthy again), so confirming `needle$`
        // would need to read INTO the failing region; ground truth is `matches == 0`, not a
        // fabricated confirmation of `$` nor a confirmed non-match -- genuinely never resolved
        // either way (the carry holding it is discarded, along with the failing block itself,
        // the moment `give_up` runs).
        use crate::analyzer::Analysis;
        struct FailsOnlyBlock1 {
            data: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock1 {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let data = self.data.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if (64..128).contains(&offset) {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let start = offset as usize;
                            let end = (start + len).min(data.len());
                            Ok(bytes::Bytes::copy_from_slice(&data[start..end]))
                        }
                    })
                })
            }
        }
        let mut data = vec![b'x'; 58];
        data.extend_from_slice(b"needle"); // ends exactly at 64, block 1's own start
        data.extend_from_slice(&[b'x'; 128]); // block 1 ([64,128)) plus healthy content after it
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyBlock1 { data }),
            64,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle$", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        assert_eq!(
            sweep.pos, 0,
            "nothing is safe to report yet -- MAX_MATCH_LEN dominates"
        );
        let mut failures = 0u8;
        loop {
            match sweep.step(&tx).await {
                Ok(crate::scan::Step::Done(())) => {
                    panic!("must not finish while block 1 is still permanently failing")
                }
                Ok(crate::scan::Step::More(..)) => continue,
                Err(_) => {
                    failures += 1;
                    if failures > crate::analyzer::ANALYSIS_RETRIES {
                        break;
                    }
                }
            }
        }
        assert_eq!(
            sweep.pos, 0,
            "\"needle\" was never confirmed safe before the failure; nothing was reported"
        );
        sweep.give_up(&tx);
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 0,
            "needle$ needed lookahead into the permanently-failing block -- never confirmed, \
             never fabricated, discarded along with the carry give_up clears"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_give_up_seam_the_f9_retirement_finds_a_bare_literal_across_multiple_tiny_reports()
     {
        // **Restructure R5 (2026-07-28), the F9 seam-cost retirement -- expectation MOVED,
        // direction RECOVERY.** Renamed from `..._seam_exclusion_persists_across_multiple_tiny_
        // reports` (the project's own established convention: a name states current behavior, not
        // a stale claim -- batch 4's own precedent renamed `sweep_analysis_treats_its_own_read_
        // limit_as_a_real_boundary` to `..._does_not_treat_...`). R4's own version of this test proved the
        // seam-look-behind EXCLUSION persists across as many tiny reports as it takes to heal, for
        // ANY pattern, assertion or not. R5 narrows that to assertion-BEARING patterns only (`Hay::
        // starts_verified`'s own doc comment: "a hay cut cannot MANUFACTURE a body") -- "needle"
        // here is a BARE LITERAL, so it now has a trustworthy start regardless of its own
        // unverifiable predecessor, and is found -- unaffected by however many tiny reports (one
        // byte at a time, below) it took to get there. `sweep_give_up_seam_does_not_overcount_a_
        // word_boundary_one_byte_past_the_landing` (this module's own sibling test, its own
        // pattern `(?u:\b)needle` genuinely assertion-bearing) is the unmoved control: THAT one
        // still excludes its own seam-adjacent boundary exactly as before.
        //
        // With `block_size: 1`, once the huge `MAX_MATCH_LEN + CTX_AHEAD - 1` lookahead margin is
        // first crossed, EVERY subsequent report covers exactly ONE new byte -- the same "every
        // single byte read triggers its own report+drain cycle" shape `sweep_analysis_needs_the_
        // whole_utf8_character_for_unicode_word_boundary`'s own comment already documents -- so
        // the report covering "needle" (planted at seam + 3) is one of many tiny, one-byte-at-a-
        // time reports, not a single lumped one; this test still pins that the multi-report
        // accumulation itself introduces no bug of its own (a match found via many tiny reports is
        // identical to one found via a single large one).
        use crate::analyzer::Analysis;
        struct FailsOnlyByteZero {
            data: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyByteZero {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let data = self.data.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset == 0 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let start = offset as usize;
                            let end = (start + len).min(data.len());
                            Ok(bytes::Bytes::copy_from_slice(&data[start..end]))
                        }
                    })
                })
            }
        }
        // enough filler between the seam (position 1, right after the one failing byte) and
        // "needle" (planted at seam + 3 = position 4) to cross the huge `MAX_MATCH_LEN +
        // CTX_AHEAD - 1` margin well past it, so every one of the four seam-adjacent positions is
        // reported on its own before the ramp itself is ever reported.
        let ramp = MAX_MATCH_LEN + CTX_AHEAD.0 + 4096;
        let mut data = vec![b'x']; // position 0: the one byte that always fails to read
        data.extend(vec![b'y'; 3]); // positions 1, 2, 3: the seam gap's own first three positions
        data.extend_from_slice(b"needle"); // position 4: the fourth, still-excluded position
        data.extend(vec![b'z'; ramp]); // ramp well past the margin so reports go 1 byte at a time
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyByteZero { data }),
            1,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        assert!(sweep.step(&tx).await.is_err(), "byte 0 must fail to read");
        sweep.give_up(&tx);
        assert_eq!(
            sweep.pos, 1,
            "give_up must land exactly past the one failing byte"
        );
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 1,
            "\"needle\" is a bare literal -- the F9 retirement means it is found regardless of \
             sitting at the fourth position past the seam, reported via its own tiny, one-byte-\
             at-a-time report"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_seam_gap_landing_exactly_at_size_excludes_zero_width_matches() {
        // fix round (2026-07-25), F5 -- the review's own P8 fixture and mutation M6: the
        // unit-A-interaction branch (`step`'s own `if self.pos == size` early return, gated on
        // `seam_gap_remaining == 0`, batch 5 (2026-07-26), finding #8's own countdown -- a plain
        // bool at the time this test was written) was completely untested -- flipping the guard
        // to `if true` (always apply the phantom rule, ignoring the seam gap) survived the whole
        // suite. `size = 128`,
        // `block_size = 64`: block 0 ([0,64)) always succeeds, block 1 ([64,128)) always fails
        // -- the whole file fits in ONE window (`SEARCH_WINDOW` far exceeds 128), so no report
        // ever fires before the failure (not enough lookahead accumulated: `self.pos` stays 0
        // through every retry), and `give_up` lands `pos = past_failing_block = (1+1)*64 = 128
        // == size` -- exactly the seam-gap-at-true-end case the guard exists for. `$`/`^$` must
        // count 0 (the real predecessor at 127 -- inside the never-read block 1 -- is UNKNOWN,
        // not confirmed non-`\n`, so the position is excluded, not guessed); `x$` (needing real
        // content the sweep never read at all) must ALSO count 0, unaffected by the guard.
        use crate::analyzer::Analysis;
        struct FailsFromOffset64;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsFromOffset64 {
            fn size(&self) -> u64 {
                128
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|offset, len| {
                    Box::pin(async move {
                        if offset >= 64 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            Ok(bytes::Bytes::from(vec![b'x'; len]))
                        }
                    })
                })
            }
        }
        async fn run_to_give_up_then_completion(pattern: &str) -> SearchSummary {
            let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
                std::sync::Arc::new(FailsFromOffset64),
                64,
                1 << 20,
            ));
            let mut sweep = SweepAnalysis::new(cache);
            let p = std::sync::Arc::new(SearchPattern::compile(pattern, false).unwrap());
            let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
            sweep.begin(&(1, p));
            let mut failures = 0u8;
            loop {
                match sweep.step(&tx).await {
                    Ok(crate::scan::Step::Done(())) => {
                        panic!("must not finish while block 1 is still permanently failing")
                    }
                    Ok(crate::scan::Step::More(..)) => continue,
                    Err(_) => {
                        failures += 1;
                        if failures > crate::analyzer::ANALYSIS_RETRIES {
                            break;
                        }
                    }
                }
            }
            assert_eq!(
                sweep.pos, 0,
                "no report fired before the failure; pos stays at 0"
            );
            sweep.give_up(&tx);
            assert_eq!(
                sweep.pos, 128,
                "give_up must land exactly at size on this fixture"
            );
            // restructure R5 (2026-07-28): re-pointed from the deleted `seam_gap_remaining`
            // counter (R4-reviewer-prescribed shape, `restr-R4-review.md`) at `Hay::seam_floor()`
            // on a hay mirroring the EXACT state `step`'s own A1/A2 sites would build right here
            // -- `sweep.carry` cleared and `sweep.ctx_len == 0` (both true post-`give_up`), so
            // this hay's own `base` is `sweep.pos` itself (128), not absolute zero: `low_is_true`
            // is `false`, and `seam_floor` floors at `CTX_BEHIND`, not `accept.start` (`0`) --
            // "the landing must be marked as a fresh seam gap, not a genuine BOF" restated as an
            // observable fact about the hay `step` will actually construct, not a private
            // counter's own numeric value.
            let landing_hay = crate::search::hay::Hay::new(
                &sweep.carry,
                crate::search::hay::Abs(sweep.pos - sweep.ctx_len as u64),
            )
            .with_accept(
                crate::search::hay::Local(sweep.ctx_len)..crate::search::hay::Local(sweep.ctx_len),
            );
            assert_eq!(
                landing_hay.seam_floor(),
                crate::search::hay::Local(CTX_BEHIND.0),
                "the landing must be marked as a fresh seam gap, not a genuine BOF"
            );
            // `step` returns `Ok(Step::Done(()))` immediately from the `pos == size` early
            // return -- no further stepping needed or possible within this generation.
            assert!(matches!(
                sweep.step(&tx).await.unwrap(),
                crate::scan::Step::Done(())
            ));
            rx_last(&tx)
        }
        assert_eq!(
            run_to_give_up_then_completion("$").await.matches,
            0,
            "the seam's own real predecessor is unknown; $ must be excluded, not guessed"
        );
        assert_eq!(
            run_to_give_up_then_completion("^$").await.matches,
            0,
            "same exclusion; ^$ needs the identical unknowable predecessor"
        );
        assert_eq!(
            run_to_give_up_then_completion("x$").await.matches,
            0,
            "x$ needs real content the sweep never read at all -- unaffected by the guard"
        );
        // fix round (2026-07-28), P2-1 -- RED, written FIRST against R5's own first attempt
        // (`Hay::zero_width_at_high` gating this exclusion on `pattern.leading_assertions()`):
        // ASSERTION-FREE zero-width-capable patterns reopened the exact phantom-overcount class
        // this test exists to pin, since the gate was waived for them entirely regardless of
        // look-behind adequacy (`zero_width_at_high`'s own doc comment has the full argument for
        // why this exclusion is positional, not assertion-based, and must apply unconditionally).
        // Review-measured against `2fe1a9a`: 0 → 1 for every one of these.
        assert_eq!(
            run_to_give_up_then_completion("x*").await.matches,
            0,
            "x* is assertion-free but still zero-width-capable at this position -- the position's \
             own phantom-status is unverifiable regardless of what the pattern demands"
        );
        assert_eq!(
            run_to_give_up_then_completion("q*").await.matches,
            0,
            "same exclusion, a different assertion-free zero-width-capable pattern"
        );
        assert_eq!(
            run_to_give_up_then_completion("(?:)").await.matches,
            0,
            "a bare empty group -- unconditionally zero-width, no Look node at all"
        );
        assert_eq!(
            run_to_give_up_then_completion("x?").await.matches,
            0,
            "an optional literal -- zero-width-capable, assertion-free"
        );
    }

    #[tokio::test]
    async fn sweep_give_up_does_not_manufacture_a_word_boundary() {
        // batch 4 (2026-07-24), finding #4's own sweep half: the OLD sentinel (a fixed non-`\n`
        // byte) kept `^` safely false but manufactured a false `\b` wherever the real, unread
        // predecessor happened to be a WORD character -- there is no substitute byte
        // simultaneously "definitely not `\n`" and "definitely not a word character" (this
        // module's own top-level doc comment). Ground truth: `\bneedle` never matches here --
        // every byte in this fixture is 'x' (a WORD character), including the never-read failing
        // block, so the real predecessor of "needle" (wherever it lands) is always a word
        // character too, and no `\b` can ever hold there.
        use crate::analyzer::Analysis;
        struct FailsOnlyBlock0;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock0 {
            fn size(&self) -> u64 {
                (SEARCH_WINDOW * 3) as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|offset, len| {
                    Box::pin(async move {
                        if offset < 4096 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let mut block = vec![b'x'; len]; // 'x' is a WORD character throughout
                            if offset == 4096 {
                                block[0..6].copy_from_slice(b"needle");
                            }
                            Ok(bytes::Bytes::from(block))
                        }
                    })
                })
            }
        }
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyBlock0),
            4096,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile(r"\bneedle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        assert!(sweep.step(&tx).await.is_err(), "block 0 must fail to read");
        sweep.give_up(&tx);
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 0,
            "the unreadable real predecessor is a word char ('x'); \\bneedle must not match"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_give_up_seam_does_not_overcount_a_word_boundary_one_byte_past_the_landing() {
        // batch 5 fix round (2026-07-26), review response, P3-6: renamed from
        // `..._overcounts_a_word_boundary_...`, which named the BUG rather than the fixed
        // behavior this test actually pins (batch 4's own precedent: `sweep_analysis_treats_
        // its_own_read_limit_as_a_real_boundary` renamed to `..._does_not_treat_...`).
        //
        // batch 5 (2026-07-26), finding #8: the one-shot exclusion (`at = lb + 1`) excludes only
        // the ONE position right at the `give_up` landing spot itself -- but a codepoint can
        // straddle the seam too, leaving `carry[0]` a bare, invalid continuation byte whose real
        // lead byte sat one byte further back, inside the never-read failing block. A start at
        // seam + 1 consults JUST that lone byte -- decoded as non-word regardless of what the
        // real (unread) character actually was -- fabricating `(?u:\b)` where none exists: block 0
        // ([0, 64)) always fails; `give_up` lands at 64; byte 64 (`carry[0]`, this leg's own
        // ANCHOR CORRECTNESS doc comment) is `é`'s OWN CONTINUATION byte (0xA9) -- é's lead byte
        // (0xC3) is conceptually the never-read byte 63, inside the failing block -- and "needle"
        // starts right after, at 65 (seam + 1, the position the OLD one-shot exclusion admits).
        // Ground truth: unknown whether the real predecessor was é (a word char, no boundary) or
        // something else entirely -- unverifiable either way, so this must be a documented MISS
        // (matches == 0), never a fabricated match -- disproving batch 4's own "failed-summary
        // floor" framing, which sold this residual as miss-only (`search.rs`'s own top-level doc
        // comment already states floor semantics; an OVERCOUNT here breaks that doctrine, not
        // merely undercounts it).
        use crate::analyzer::Analysis;
        struct FailsOnlyBlock0 {
            after: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock0 {
            fn size(&self) -> u64 {
                64 + self.after.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let after = self.after.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset < 64 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let start = (offset - 64) as usize;
                            let end = (start + len).min(after.len());
                            Ok(bytes::Bytes::copy_from_slice(&after[start..end]))
                        }
                    })
                })
            }
        }
        let mut after = vec![0xA9u8]; // é's own continuation byte -- its lead (0xC3) is byte 63, unreadable
        after.extend_from_slice(b"needle");
        after.extend_from_slice(&[b'x'; 100]); // plenty past "needle" so this window resolves fully
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyBlock0 { after }),
            64,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile(r"(?u:\b)needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        assert!(sweep.step(&tx).await.is_err(), "block 0 must fail to read");
        sweep.give_up(&tx);
        assert_eq!(
            sweep.pos, 64,
            "give_up must land exactly past the one failing block"
        );
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 0,
            "the byte right after the seam is é's own unverifiable continuation byte -- \
             (?u:\\b)needle must be a documented miss, not a fabricated match"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_give_up_seam_still_finds_a_word_boundary_outside_the_excluded_window() {
        // the positive twin, at the SAME seam: once far enough past `give_up`'s own landing spot
        // that the look-behind bytes needed no longer touch the never-read region at all (`CTX_
        // BEHIND` real bytes, all confirmed since the seam), a genuine `(?u:\b)` must still be
        // found -- proving the widened exclusion is a BOUNDED miss (at most CTX_BEHIND positions),
        // not an unbounded "never trust anything near a seam again" overcorrection. Bytes [64, 68)
        // are all real, confirmed 'x' (word characters) except the last, a space (non-word) right
        // before "needle" at 68 -- seam + CTX_BEHIND exactly, the first position whose own full
        // look-behind character needs nothing from below the seam.
        use crate::analyzer::Analysis;
        struct FailsOnlyBlock0 {
            after: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock0 {
            fn size(&self) -> u64 {
                64 + self.after.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let after = self.after.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset < 64 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let start = (offset - 64) as usize;
                            let end = (start + len).min(after.len());
                            Ok(bytes::Bytes::copy_from_slice(&after[start..end]))
                        }
                    })
                })
            }
        }
        let mut after = b"xxx ".to_vec(); // 3 word chars, then a real, unambiguous non-word byte
        after.extend_from_slice(b"needle");
        after.extend_from_slice(&[b'x'; 100]);
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(FailsOnlyBlock0 { after }),
            64,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile(r"(?u:\b)needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        assert!(sweep.step(&tx).await.is_err(), "block 0 must fail to read");
        sweep.give_up(&tx);
        assert_eq!(
            sweep.pos, 64,
            "give_up must land exactly past the one failing block"
        );
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 1,
            "the real, confirmed space right before \"needle\" -- 4 bytes past the seam, fully \
             verified without touching the never-read region -- must still be found"
        );
        assert!(summary.failed);
    }

    #[tokio::test]
    async fn sweep_analysis_needs_the_whole_utf8_character_for_unicode_word_boundary() {
        // batch 4 (2026-07-24), finding #9: one look-behind byte can be a lone, INVALID UTF-8
        // continuation byte -- the tail of a real multi-byte character whose own head sits one
        // byte further back, already dropped. "é" (2 bytes, U+00E9) is a Unicode WORD character;
        // planting its last byte immediately before "needle" at a report/drain seam (forced by
        // `block_size: 1`, so every single byte read triggers its own report+drain, landing this
        // exactly at the seam the OLD one-byte ctx would mishandle) must NOT satisfy `(?u:\b)`,
        // since é (the REAL preceding character) is a word char, same as 'n' -- no boundary.
        let plant = 5000usize; // past the initial MAX_MATCH_LEN ramp, so reports are already firing
        let mut data = vec![b'x'; SEARCH_WINDOW];
        data[plant - 2..plant].copy_from_slice("é".as_bytes()); // [0xC3, 0xA9]
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 1, "(?u:\\b)needle").await;
        assert_eq!(
            summary.matches, 0,
            "é is a unicode word char; (?u:\\b) must not hold between é and 'n'"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_unicode_word_boundary_still_holds_after_a_real_non_word_char() {
        // the positive twin: a genuine non-word byte (space) immediately before "needle", at the
        // identical report/drain seam shape -- (?u:\b) must still hold. Proves the finding #9 fix
        // (up to CTX_BEHIND real bytes of context) doesn't overcorrect into never matching.
        let plant = 5000usize;
        let mut data = vec![b'x'; SEARCH_WINDOW];
        data[plant - 1] = b' ';
        data[plant..plant + 6].copy_from_slice(b"needle");
        let summary = run_sweep_to_completion(data, 1, "(?u:\\b)needle").await;
        assert_eq!(summary.matches, 1, "space is non-word; (?u:\\b) must hold");
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_does_not_treat_its_own_read_limit_as_a_real_boundary() {
        // batch 4 (2026-07-24), finding #10: an a-run of exactly MAX_MATCH_LEN bytes, starting
        // at the very last byte of the first window's own core (so its own end coincides exactly
        // with the OLD, one-byte-short `read_to`), followed by a real, non-newline byte. Ground
        // truth: `a{4096}$` never matches -- a real byte follows the run, so `$` must fail.
        // Pre-fix this counted 1: the old `read_to`/`safe_to` gave the trailing `$` nothing past
        // the a-run's own end to check, so the window's own artificial read limit was mistaken
        // for a real boundary (the same "slice's own edge is unconditionally eligible" hazard
        // this module's own top-level doc comment names, here hitting `$` instead of `^`/`\b`).
        let start = SEARCH_WINDOW - 1;
        let mut data = vec![b'x'; start];
        data.extend(std::iter::repeat_n(b'a', MAX_MATCH_LEN));
        data.push(b'x'); // the real byte that disproves $
        data.extend(vec![b'x'; 200]);
        let pattern = format!("a{{{MAX_MATCH_LEN}}}$");
        let summary = run_sweep_to_completion(data, 37, &pattern).await;
        assert_eq!(
            summary.matches, 0,
            "a real byte follows the a-run; $ must not falsely confirm at the read limit"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_finds_a_dollar_match_exactly_at_a_real_window_seam() {
        // the positive twin, at the SAME (cap-length, seam-aligned) alignment as the test above:
        // this time the file genuinely ENDS right after the a-run (true EOF, not a real byte) --
        // `a{MAX_MATCH_LEN}$` must still be found, proving the fix didn't just start rejecting
        // everything at that boundary.
        let start = SEARCH_WINDOW - 1;
        let mut data = vec![b'x'; start];
        data.extend(std::iter::repeat_n(b'a', MAX_MATCH_LEN));
        let pattern = format!("a{{{MAX_MATCH_LEN}}}$");
        let summary = run_sweep_to_completion(data, 37, &pattern).await;
        assert_eq!(summary.matches, 1, "the file truly ends here; $ must hold");
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_needs_the_full_trailing_character_not_one_byte_at_the_read_limit() {
        // batch 5 (2026-07-26), finding #3: the SAME `read_to`/`safe_to` margin finding #10
        // (batch 4) widened to MAX_MATCH_LEN (not MAX_MATCH_LEN - 1) still left only ONE byte of
        // real lookahead past a cap-length match's own end -- enough for `$`/ASCII `\b`, but not
        // for a Unicode-aware `(?u:\b)`, which needs the FULL following character (up to
        // CTX_AHEAD bytes). Same alignment as `sweep_analysis_does_not_treat_its_own_read_limit_
        // as_a_real_boundary` (an a-run ending exactly at the first window's own OLD read limit),
        // `é` (2 bytes, a Unicode word character) right after it: `(?u:\b)` must NOT hold (é is a
        // word char, same as 'a' -- no boundary).
        let start = SEARCH_WINDOW - 1;
        let mut data = vec![b'x'; start];
        data.extend(std::iter::repeat_n(b'a', MAX_MATCH_LEN));
        data.extend_from_slice("é".as_bytes());
        data.extend(vec![b'z'; 200]);
        let pattern = format!("a{{{MAX_MATCH_LEN}}}(?u:\\b)");
        let summary = run_sweep_to_completion(data, 37, &pattern).await;
        assert_eq!(
            summary.matches, 0,
            "é is a unicode word char immediately after the a-run; (?u:\\b) must not hold"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_safe_to_alone_needs_the_full_trailing_character() {
        // batch 5 fix round (2026-07-26), review response, P3-1: the sibling test above conflates
        // TWO widened quantities -- `read_to` and `safe_to` -- because its own alignment (an
        // a-run ending exactly at the first window's own core boundary) only resolves once the
        // window's own read reaches `read_to`, so reverting `safe_to` ALONE there (leaving `read_
        // to` still widened) survives: the report that would fabricate never gets a chance to
        // fire before the wider `read_to` supplies the REST of `é` anyway. This fixture isolates
        // `safe_to`'s own mid-file branch: match_at is 0 (the a-run starts at the true file
        // start), so the VERY FIRST report a sweep ever makes is the one that resolves it --
        // `core_end` (`SEARCH_WINDOW`, 64 KiB) is far beyond this tiny file either way, so no
        // report here ever depends on reaching a window boundary, and (unlike a later match_at)
        // no earlier report has to search a hay that does not yet contain the full a-run at all
        // -- avoiding the pathologically expensive "does `a{4096}` match anywhere in this
        // still-incomplete hay" scan a naive later-match_at fixture forces on every one of its
        // own many intermediate reports. `block_size: 1` still exposes the exact byte where the
        // margin differs: with the correct margin (`MAX_MATCH_LEN + CTX_AHEAD - 1` = 4099) the
        // first report fires once `é` (2 bytes) is fully read plus 1 more; with the OLD, reverted
        // `safe_to` alone (margin 4096) it fires 3 bytes earlier, with only `é`'s own lead byte
        // visible -- reproducing the exact fabrication, driven by `safe_to` and nothing else.
        // Trailing filler kept to the bare minimum past `é` (just enough that the OLD margin's own
        // critical byte still lands mid-file, not at the true end) -- every report after the
        // first is itself a "confirm there is no SECOND match" search over the same a-run-sized
        // hay, and each one of those is genuinely expensive for this specific pattern (a bounded
        // `{4096}` repetition combined with a Unicode-mode sub-assertion); minimizing how many of
        // them this fixture needs is a deliberate performance choice, not a correctness one.
        let mut data = vec![];
        data.extend(std::iter::repeat_n(b'a', MAX_MATCH_LEN));
        data.extend_from_slice("é".as_bytes());
        data.extend(vec![b'z'; 5]);
        let pattern = format!("a{{{MAX_MATCH_LEN}}}(?u:\\b)");
        let summary = run_sweep_to_completion(data, 1, &pattern).await;
        assert_eq!(
            summary.matches, 0,
            "é is a unicode word char immediately after the a-run; (?u:\\b) must not hold, and \
             this alignment isolates safe_to's own margin from read_to's"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_give_up_only_skips_the_failing_block_not_a_whole_window_of_healthy_ones() {
        // batch 4 (2026-07-24), finding #12: the OLD `give_up` additionally floored its skip at
        // a full `past_one_window` (`pos + SEARCH_WINDOW`) even when the failing block itself was
        // far smaller -- discarding every match in the (perfectly healthy, readable) territory
        // between the failing block's own end and that artificial floor. Block 0 (64 bytes) is
        // the ONLY thing that ever fails; "needle" is planted three times well within the
        // (undercounted, pre-fix) gap between the failing block's own end and a full SEARCH_
        // WINDOW past it. Ground truth (whole-buffer minus the one dead block): 3.
        use crate::analyzer::Analysis;
        struct FailsOnlyBlock0 {
            data: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsOnlyBlock0 {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let data = self.data.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset < 64 {
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            let end = (offset as usize + len).min(data.len());
                            Ok(bytes::Bytes::copy_from_slice(&data[offset as usize..end]))
                        }
                    })
                })
            }
        }
        let mut data = vec![b'x'; 3 * SEARCH_WINDOW];
        for &pos in &[1000usize, 30000, 60000] {
            data[pos..pos + 6].copy_from_slice(b"needle");
        }
        let src = FailsOnlyBlock0 { data };
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(src),
            64,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        let mut failures = 0u8;
        loop {
            match sweep.step(&tx).await {
                Ok(crate::scan::Step::Done(())) => {
                    panic!("must not finish while block 0 is still permanently failing")
                }
                Ok(crate::scan::Step::More(..)) => continue,
                Err(_) => {
                    failures += 1;
                    if failures > crate::analyzer::ANALYSIS_RETRIES {
                        break;
                    }
                }
            }
        }
        sweep.give_up(&tx);
        assert_eq!(
            sweep.pos, 64,
            "give_up must land exactly past the one failing block"
        );
        while matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::More(..)) {}
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 3,
            "all three needles live in healthy blocks the old past_one_window floor discarded"
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
            block1_reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsSecondBlock {
            fn size(&self) -> u64 {
                self.block_size * 3
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let block_size = self.block_size;
                let block1_reads = self.block1_reads.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset < block_size {
                            Ok(bytes::Bytes::from(vec![b'x'; len]))
                        } else if offset < 2 * block_size {
                            block1_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            Err(anyhow::anyhow!("boom"))
                        } else {
                            Ok(bytes::Bytes::from(vec![b'x'; len]))
                        }
                    })
                })
            }
        }
        let block_size = 4 * SEARCH_WINDOW as u64;
        let src = std::sync::Arc::new(FailsSecondBlock {
            block_size,
            block1_reads: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
                Ok(crate::scan::Step::Done(())) => {
                    panic!("must not finish while block 1 is still permanently failing")
                }
                Ok(crate::scan::Step::More(..)) => continue,
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

    #[tokio::test]
    async fn sweep_analysis_finds_a_match_below_a_truncation_gap_instead_of_spinning() {
        // fix round (2026-07-25), F2 -- the review's own P7 fixture: a source whose `size()`
        // claims 20_000 but has only 200 real bytes, "needle" planted at 100. Pre-fix this spun
        // (500+ consecutive `Ok(false)` with `pos == 0`, `matches == 0`, since `safe_to` never
        // accumulated enough lookahead to report anything before the read loop's own `take == 0`
        // `break` froze `pos` at 0 forever) -- ground truth is one real, readable match.
        use crate::analyzer::Analysis;
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
        let mut real = vec![b'x'; 200];
        real[100..106].copy_from_slice(b"needle");
        let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
            std::sync::Arc::new(OverstatesItsOwnSize {
                real,
                claimed: 20_000,
            }),
            64,
            1 << 20,
        ));
        let mut sweep = SweepAnalysis::new(cache);
        let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
        let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
        sweep.begin(&(1, p));
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(
                steps <= 1000,
                "livelock: {steps} consecutive Ok(false), no progress (pos stuck at {})",
                sweep.pos
            );
            if matches!(sweep.step(&tx).await.unwrap(), crate::scan::Step::Done(())) {
                break;
            }
        }
        let summary = rx_last(&tx);
        assert_eq!(
            summary.matches, 1,
            "the one real, readable needle must be found"
        );
        assert!(summary.done);
    }

    #[tokio::test]
    async fn sweep_analysis_step_never_errors_across_a_truncated_window_boundary_sweep() {
        // restructure R6: `Step::More`'s own `Charged` witness comes from `self.meter.
        // progress_witness()`, which fails if NOTHING was charged since `begin_step` -- a real
        // risk specifically at the empty-read-driven truncation discovery (`got.is_empty()`,
        // `step`'s own doc comment): could a report fire (self.pos moves) using ONLY carry bytes
        // charged in an EARLIER call, while the terminal `Empty` read that unlocked it (via the
        // `at_final` flip) itself charges zero, leaving THIS call's own `spent_this_step` at 0?
        //
        // Worked out by hand, not merely hoped: whenever the source overstates (`claimed >=
        // real`, the only interesting truncation shape), `core_end = min(pos + WINDOW, claimed)`
        // is ALWAYS `>= real_end` the moment `real_end` falls within this window's own reach
        // (`pos + WINDOW >= real_end`) -- both `pos + WINDOW >= real_end` (the reach premise) and
        // `claimed >= real_end` (overstating) hold, and `min` of two values each `>= real_end` is
        // itself `>= real_end`. So `report_to = min(safe_to, core_end)`, with `safe_to == real_end`
        // exactly at the moment of empty-discovery (`at_final`'s own `safe_to = read_pos`, and
        // `read_pos` cannot advance past `real_end` without itself hitting the SAME empty wall),
        // is NEVER clamped below `real_end` by `core_end` -- `report_to` always reaches the FULL,
        // newly-corrected `size` in one shot, making `done` true the same call. A call that
        // reports without charging and STAYS not-done is therefore unconstructible for this
        // fixture shape, not merely untested -- this sweep is the permanent check on that
        // argument, mirroring `search_next_never_errors_across_a_composed_budget_sweep`'s own
        // established idiom (document.rs, R3's own P1-2 fix) for the identical kind of claim.
        use crate::analyzer::Analysis;
        struct Overstated {
            real: Vec<u8>,
            claimed: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for Overstated {
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
        // `real_extra`: how far past one whole SEARCH_WINDOW the real data reaches -- brackets
        // the boundary itself (0), a span shorter than `consumable_reach()` (100, the shape the
        // hand derivation above is about), and one comfortably past it (5000).
        for real_extra in [0usize, 1, 100, 5000] {
            // `claimed_extra`: how much FURTHER the source overstates beyond the real end.
            for claimed_extra in [0u64, 1, 50, 5000] {
                let real_size = SEARCH_WINDOW + real_extra;
                let claimed = (real_size as u64) + claimed_extra;
                let real = vec![b'x'; real_size];
                let cache = std::sync::Arc::new(crate::cache::BlockCache::new(
                    std::sync::Arc::new(Overstated { real, claimed }),
                    4096,
                    1 << 20,
                ));
                let mut sweep = SweepAnalysis::new(cache);
                let p = std::sync::Arc::new(SearchPattern::compile("needle", false).unwrap());
                let (tx, _rx) = tokio::sync::watch::channel(SearchSummary::empty());
                sweep.begin(&(1, p));
                let mut steps = 0;
                loop {
                    steps += 1;
                    assert!(
                        steps <= 1000,
                        "livelock: {steps} consecutive More, no Done (real_extra={real_extra}, \
                         claimed_extra={claimed_extra}, pos stuck at {})",
                        sweep.pos
                    );
                    match sweep.step(&tx).await {
                        Ok(crate::scan::Step::Done(())) => break,
                        Ok(crate::scan::Step::More(..)) => continue,
                        Err(e) => panic!(
                            "step must never error on an overstating source \
                             (real_extra={real_extra}, claimed_extra={claimed_extra}, \
                             pos={}): {e:#}",
                            sweep.pos
                        ),
                    }
                }
            }
        }
    }
}
