//! Width-aware layout of one logical line into a horizontal window of display
//! columns: expands tabs, measures unicode width, renders control bytes as caret
//! notation, and takes the `[hscroll, hscroll + cols)` column slice.
//! `layout_row_with_marks` additionally maps byte ranges (search matches) through
//! the SAME walk into visible-cell ranges, so highlighting never re-derives the
//! layout; `layout_row` is a thin `&[]` wrapper over it.
use unicode_width::UnicodeWidthChar;

/// Lays one logical line (`raw`, newline already stripped) out into the string
/// visible in display columns `[hscroll, hscroll + cols)`. Tabs advance to the
/// next `tab_stop`; wide chars occupy two columns; C0/DEL control bytes render as
/// caret notation (`^[`); invalid UTF-8 becomes the replacement char. A wide char
/// straddling either window edge renders as a single blank for its visible half.
/// An empty window (`cols == 0`) yields an empty string. A thin, one-line wrapper
/// over `layout_row_with_marks` (`byte_marks: &[]`) -- correct by inspection, so
/// the tests that matter compare `layout_row_with_marks`'s own `&[]` path against
/// `reference_layout_row`, a FROZEN copy of this function's own pre-refactor body
/// (`empty_byte_marks_change_nothing`, `empty_marks_agree_with_the_frozen_pre_
/// marks_reference`) -- never against this wrapper itself, which is now nothing
/// but a call to the function under test and so could never disagree with it,
/// regardless of what bug a future change here might introduce.
// dead on the lib target since batch 3 (2026-07-23), finding #10: `Document::render_row`
// (document.rs) used to call this directly for the no-search case; it now always goes through
// `layout_row_with_marks` itself (byte-for-byte identical result either way -- this function's
// own body IS exactly that call with `byte_marks: &[]`), so match-clipping and mark computation
// live in exactly one place instead of two. Kept alive here for the test suite -- most of this
// module's own non-marks tests exercise the simpler 4-argument shape directly -- same idiom as
// `scan.rs`'s own `SearchStep`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn layout_row(raw: &[u8], tab_stop: usize, hscroll: usize, cols: usize) -> String {
    layout_row_with_marks(raw, tab_stop, hscroll, cols, &[]).0
}

/// Processes one decoded input token -- a tab byte, a control byte, or one char
/// (valid UTF-8, or a lossy `'\u{FFFD}'` standing in for a maximal invalid
/// subsequence) -- into `cells`, exactly as `layout_row`'s own walk always has.
/// Returns whether the token was actually emitted: `false` means the window's
/// right edge (`end`) was already reached, so NOTHING further is ever walked --
/// the caller stops immediately, matching `Document::viewport`'s own "read
/// forward only until the screen is filled" contract, applied here to layout.
fn layout_token(
    ch: char,
    tab_stop: usize,
    end: usize,
    col: &mut usize,
    cells: &mut Vec<(char, usize)>,
) -> bool {
    if ch == '\t' {
        if *col >= end {
            return false;
        }
        let next = (*col / tab_stop + 1) * tab_stop;
        for _ in *col..next {
            cells.push((' ', 1));
        }
        *col = next;
    } else if ch.is_ascii_control() {
        if *col >= end {
            return false;
        }
        let caret = if ch == '\u{7f}' {
            '?'
        } else {
            (ch as u8 ^ 0x40) as char
        };
        cells.push(('^', 1));
        cells.push((caret, 1));
        *col += 2;
    } else {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        // width-0 marks combine with the previous cell, so let them through
        // at the window edge instead of breaking before them.
        if *col >= end && w > 0 {
            return false;
        }
        cells.push((ch, w));
        *col += w;
    }
    true
}

/// `layout_row`'s own full contract, plus mark translation: `byte_marks` --
/// byte ranges into `raw`, e.g. search match spans -- come back as visible-cell
/// ranges (relative to the RETURNED string's own columns, i.e. already
/// `skip_cols`-adjusted), clipped to the `[skip_cols, skip_cols + cols)` window,
/// with adjacent/overlapping ranges merged.
///
/// Marks are translated in the SAME walk that builds the cells, not a second
/// layout: every decoded token records its own raw-byte end (offsets are
/// contiguous and strictly increasing, so they stay sorted for free) and the
/// display columns it occupies as it is produced; each `byte_marks` range is
/// then resolved against that record with a binary search, so the layout
/// itself is computed exactly once regardless of how many marks there are.
pub fn layout_row_with_marks(
    raw: &[u8],
    tab_stop: usize,
    skip_cols: usize,
    cols: usize,
    byte_marks: &[std::ops::Range<usize>],
) -> (String, Vec<std::ops::Range<u16>>) {
    if cols == 0 {
        // with skip_cols == end every straddle test degenerates: a wide char
        // crossing the window would emit its blank half into zero columns --
        // and there is nothing left for a mark to ever land on either.
        return (String::new(), Vec::new());
    }
    let tab_stop = tab_stop.max(1);
    let end = skip_cols.saturating_add(cols);
    // a trailing '\r' (CRLF line endings) is dropped before layout; ASCII '\r'
    // is always a standalone valid byte -- never a continuation byte, never
    // what a lossy replacement produces -- so trimming the raw byte here lands
    // on exactly the text the old `str::strip_suffix('\r')` (on the fully
    // lossy-decoded string) did, while keeping every remaining raw byte's own
    // index intact for the marks walk below.
    let body = raw.strip_suffix(b"\r").unwrap_or(raw);
    let mut cells: Vec<(char, usize)> = Vec::new();
    // one entry per decoded token: the raw byte offset just past it (exclusive,
    // so entries are contiguous and sorted) and the (start col, width) it
    // occupies. Bytes past the last entry (the window's right edge cut the
    // walk short) deliberately have none -- `token_at` below then reports
    // `end`, exactly where an unclipped mark there would land anyway.
    let mut token_end: Vec<usize> = Vec::new();
    let mut token_col: Vec<(usize, usize)> = Vec::new();
    let mut col = 0usize;
    let mut pos = 0usize;
    'walk: for chunk in body.utf8_chunks() {
        for ch in chunk.valid().chars() {
            let before = col;
            if !layout_token(ch, tab_stop, end, &mut col, &mut cells) {
                break 'walk;
            }
            pos += ch.len_utf8();
            token_end.push(pos);
            token_col.push((before, col - before));
        }
        if !chunk.invalid().is_empty() {
            let before = col;
            if !layout_token('\u{fffd}', tab_stop, end, &mut col, &mut cells) {
                break 'walk;
            }
            pos += chunk.invalid().len();
            token_end.push(pos);
            token_col.push((before, col - before));
        }
    }
    let mut out = String::new();
    let mut c = 0usize;
    for (ch, w) in cells {
        let start = c;
        let stop = c + w;
        c = stop;
        if stop <= skip_cols {
            continue;
        }
        if start >= end {
            // a width-0 mark at the edge belongs to the last visible cell; a
            // width-0 cell can also arrive past the edge when a tab or wide
            // char overshot `end` — that one falls through to the break.
            if w == 0 && start == end {
                out.push(ch);
                continue;
            }
            break;
        }
        if start < skip_cols || stop > end {
            out.push(' ');
            if stop > end {
                break;
            }
            continue;
        }
        out.push(ch);
    }
    // the (col start, width) of the token whose raw byte range contains `byte`
    // -- `None` past the last recorded token (the walk stopped at or before
    // `byte`, i.e. at or past `end`).
    let token_at = |byte: usize| -> Option<(usize, usize)> {
        let i = token_end.partition_point(|&e| e <= byte);
        token_col.get(i).copied()
    };
    let mut spans: Vec<std::ops::Range<u16>> = Vec::new();
    for m in byte_marks {
        if m.start >= m.end {
            // batch 3 (2026-07-23), finding #11(b): a zero-width match (`^`, `$`, or any other
            // empty pattern) has no bytes of its own to map through a token, and used to be
            // dropped outright here -- current or not, it could never render at all. `token_at`
            // answers "the token starting at-or-after this byte", i.e. the cell the match sits
            // BEFORE -- right when real content follows it (`^` before the first char) -- but has
            // no entry for a position at or past everything the walk actually laid out (an EOF
            // `$`, or `^$` on an empty line): `col`'s own final value there -- one past the last
            // emitted cell, whatever the line's own length -- is the honest fallback, not `end`
            // (the window's own right edge, which a short line in a wide window never reaches;
            // the non-zero-width branch below still falls back to `end`, unaffected, since ITS
            // fallback only fires once the walk already ran past the window edge, the one case
            // where `col` and `end` agree anyway). Clamped into the visible window like any other
            // mark; a point at-or-past `end` lands on the final in-window cell instead of
            // vanishing -- there is no column `end` (one PAST the window's own last column) to
            // occupy -- the cursor-cell precedent `render.rs`'s command line already uses for "a
            // position with nothing past it still needs a visible cell". A point left of
            // `skip_cols` (scrolled past on the left) has nowhere sane to clamp TO and is dropped
            // instead, same as a wide mark that is entirely off-screen there.
            let raw = token_at(m.start).map_or(col, |(s, _)| s);
            if raw < skip_cols {
                continue;
            }
            let lo = (raw.min(end - 1) - skip_cols) as u16;
            let hi = lo + 1;
            match spans.last_mut() {
                Some(last) if lo <= last.end => last.end = last.end.max(hi),
                _ => spans.push(lo..hi),
            }
            continue;
        }
        let raw_start = token_at(m.start).map_or(end, |(s, _)| s);
        let raw_end = token_at(m.end - 1).map_or(end, |(s, w)| s + w);
        let lo = raw_start.clamp(skip_cols, end);
        let hi = raw_end.clamp(skip_cols, end);
        if lo >= hi {
            continue;
        }
        let (lo, hi) = ((lo - skip_cols) as u16, (hi - skip_cols) as u16);
        match spans.last_mut() {
            Some(last) if lo <= last.end => last.end = last.end.max(hi),
            _ => spans.push(lo..hi),
        }
    }
    (out, spans)
}

/// FROZEN copy of `layout_row`'s own body exactly as it stood before this task
/// introduced `layout_row_with_marks` (byte-for-byte, from the commit right
/// before that refactor) -- an INDEPENDENT second implementation, never touched
/// again after being pasted here, so the tests that compare against it are a
/// genuine divergence oracle: `layout_row_with_marks(.., &[])` is checked
/// against a fixed implementation that shares no code with it, never against the
/// live `layout_row`, which is now nothing but a one-line call to the function
/// under test -- comparing THAT against itself could never fail no matter what
/// bug a refactor introduced, exactly the tautology this function exists to
/// close off (`empty_byte_marks_change_nothing`, `empty_marks_agree_with_the_
/// frozen_pre_marks_reference`, below).
#[cfg(test)]
fn reference_layout_row(raw: &[u8], tab_stop: usize, hscroll: usize, cols: usize) -> String {
    if cols == 0 {
        // with hscroll == end every straddle test degenerates: a wide char
        // crossing the window would emit its blank half into zero columns.
        return String::new();
    }
    let tab_stop = tab_stop.max(1);
    let end = hscroll.saturating_add(cols);
    let cow = String::from_utf8_lossy(raw);
    let text: &str = match cow.strip_suffix('\r') {
        Some(t) => t,
        None => cow.as_ref(),
    };
    let mut cells: Vec<(char, usize)> = Vec::new();
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            if col >= end {
                break;
            }
            let next = (col / tab_stop + 1) * tab_stop;
            for _ in col..next {
                cells.push((' ', 1));
            }
            col = next;
        } else if ch.is_ascii_control() {
            if col >= end {
                break;
            }
            let caret = if ch == '\u{7f}' {
                '?'
            } else {
                (ch as u8 ^ 0x40) as char
            };
            cells.push(('^', 1));
            cells.push((caret, 1));
            col += 2;
        } else {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            // width-0 marks combine with the previous cell, so let them through
            // at the window edge instead of breaking before them.
            if col >= end && w > 0 {
                break;
            }
            cells.push((ch, w));
            col += w;
        }
    }
    let mut out = String::new();
    let mut c = 0usize;
    for (ch, w) in cells {
        let start = c;
        let stop = c + w;
        c = stop;
        if stop <= hscroll {
            continue;
        }
        if start >= end {
            // a width-0 mark at the edge belongs to the last visible cell; a
            // width-0 cell can also arrive past the edge when a tab or wide
            // char overshot `end` — that one falls through to the break.
            if w == 0 && start == end {
                out.push(ch);
                continue;
            }
            break;
        }
        if start < hscroll || stop > end {
            out.push(' ');
            if stop > end {
                break;
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chops_to_width() {
        assert_eq!(layout_row(b"hello world", 8, 0, 5), "hello");
    }
    #[test]
    fn expands_tabs_to_next_stop() {
        assert_eq!(layout_row(b"a\tb", 8, 0, 20), "a       b");
    }
    #[test]
    fn expands_leading_tab() {
        assert_eq!(layout_row(b"\tx", 4, 0, 20), "    x");
    }
    #[test]
    fn wide_char_counts_as_two_columns() {
        // cols=3: 世 fills columns 0-1, 界 would straddle the right edge -> blank.
        assert_eq!(layout_row("世界".as_bytes(), 8, 0, 3), "世 ");
    }
    #[test]
    fn control_byte_renders_as_caret() {
        assert_eq!(layout_row(b"a\x1bb", 8, 0, 20), "a^[b");
    }
    #[test]
    fn del_renders_as_caret_question() {
        assert_eq!(layout_row(b"\x7f", 8, 0, 20), "^?");
    }
    #[test]
    fn invalid_utf8_becomes_replacement() {
        assert_eq!(layout_row(b"a\xffb", 8, 0, 20), "a\u{fffd}b");
    }
    #[test]
    fn strips_trailing_carriage_return() {
        assert_eq!(layout_row(b"abc\r", 8, 0, 20), "abc");
    }
    #[test]
    fn horizontal_scroll_windows_columns() {
        assert_eq!(layout_row(b"0123456789", 8, 3, 4), "3456");
    }
    #[test]
    fn wide_char_straddling_left_edge_is_blanked() {
        // "a世b": a@0, 世@1-2, b@3. window [2,4) -> 世's right half is a blank.
        assert_eq!(layout_row("a世b".as_bytes(), 8, 2, 2), " b");
    }
    #[test]
    fn combining_mark_takes_zero_columns() {
        // e + combining acute is one column, so the following 'b' isn't shifted right.
        assert_eq!(layout_row("e\u{301}b".as_bytes(), 8, 0, 2), "e\u{301}b");
    }
    #[test]
    fn combining_mark_at_right_edge_stays_attached() {
        // the mark is width-0 and combines with 'e', so a 1-column window must
        // keep it rather than truncate the grapheme to a bare 'e'.
        assert_eq!(layout_row("e\u{301}".as_bytes(), 8, 0, 1), "e\u{301}");
    }
    #[test]
    fn multiple_trailing_marks_stay_attached() {
        // every width-0 mark at the edge belongs to the last visible cell.
        assert_eq!(
            layout_row("e\u{301}\u{301}".as_bytes(), 8, 0, 1),
            "e\u{301}\u{301}"
        );
    }
    #[test]
    fn mark_after_tab_overshoot_is_dropped_harmlessly() {
        // a tab can advance col past the window end before a width-0 mark is
        // seen; the phantom cell must fall through to the break, not render.
        assert_eq!(layout_row("\t\u{301}".as_bytes(), 8, 0, 3), "   ");
    }
    #[test]
    fn empty_window_emits_nothing() {
        // a wide char straddling an empty window (hscroll == end) "straddles"
        // both edges at once; its blank half must not appear in zero columns.
        assert_eq!(layout_row("世".as_bytes(), 8, 1, 0), "");
        assert_eq!(layout_row(b"abc", 8, 0, 0), "");
    }
    #[test]
    fn windows_far_into_a_long_line() {
        // 300-digit line; window at column 200 is laid out correctly even though
        // cells past the window's right edge are never built.
        let line: String = (0..300u32)
            .map(|i| char::from(b'0' + (i % 10) as u8))
            .collect();
        assert_eq!(layout_row(line.as_bytes(), 8, 200, 10), "0123456789");
    }
    #[test]
    fn marks_map_through_tabs() {
        // a tab before the match expands to 4 columns (tab_stop=4); the match on
        // byte 1..2 ('X') must map to the column AFTER the tab's own width, not
        // its raw byte index.
        let (text, spans) = layout_row_with_marks(b"\tX", 4, 0, 20, std::slice::from_ref(&(1..2)));
        assert_eq!(text, "    X");
        assert_eq!(spans, vec![4..5]);
    }
    #[test]
    fn marks_clip_at_the_hscroll_window() {
        // window [3, 7) -> "3456": a mark straddling the left edge clips to its
        // visible remainder; a mark entirely left of it vanishes.
        let (text, spans) = layout_row_with_marks(b"0123456789", 8, 3, 4, &[0..2, 2..5]);
        assert_eq!(text, "3456");
        assert_eq!(
            spans,
            vec![0..2],
            "only \"34\" (cols 3-4) of the 2..5 mark survives"
        );
    }
    #[test]
    fn marks_clip_at_the_right_edge() {
        // window [0, 4) -> "0123": a mark straddling the right edge clips to its
        // visible remainder; a mark entirely past it vanishes.
        let (text, spans) = layout_row_with_marks(b"0123456789", 8, 0, 4, &[2..7, 5..9]);
        assert_eq!(text, "0123");
        assert_eq!(
            spans,
            vec![2..4],
            "only \"23\" (cols 2-3) of the 2..7 mark survives"
        );
    }
    #[test]
    fn marks_over_control_carets_cover_the_caret_cells() {
        // one control byte renders as a 2-cell caret pair (^[); a mark over just
        // that BYTE must cover BOTH cells, not one.
        let (text, spans) =
            layout_row_with_marks(b"a\x1bb", 8, 0, 20, std::slice::from_ref(&(1..2)));
        assert_eq!(text, "a^[b");
        assert_eq!(spans, vec![1..3]);
    }
    #[test]
    fn marks_merge_when_adjacent() {
        // two matches with no gap between them ("xy" split into two 1-byte
        // matches) must come back as ONE merged span, not two touching ones --
        // fewer restyle calls downstream, and a single continuous highlight.
        let (text, spans) = layout_row_with_marks(b"xyz", 8, 0, 20, &[0..1, 1..2]);
        assert_eq!(text, "xyz");
        assert_eq!(spans, vec![0..2]);
    }
    #[test]
    fn empty_byte_marks_change_nothing() {
        // the &[] case must match the pre-refactor implementation exactly --
        // checked against `reference_layout_row`, a FROZEN independent copy
        // (see its own doc comment), never against the live `layout_row`: that
        // wrapper is now defined as `layout_row_with_marks(.., &[]).0`, so
        // comparing it here would compare the function under test to itself, a
        // tautology that can never fail regardless of what the code does.
        // Pinned directly here, and as a property below over arbitrary input.
        let (text, spans) = layout_row_with_marks(b"a\tb\x1bc", 8, 0, 20, &[]);
        assert_eq!(text, reference_layout_row(b"a\tb\x1bc", 8, 0, 20));
        assert!(spans.is_empty());
    }
    // batch 3 (2026-07-23), finding #11(b): a zero-width mark (`^`/`$`) used to be dropped
    // outright (`m.start >= m.end` was an unconditional `continue`) -- invisible, current or
    // not, since the current-match lookup's own single-mark `layout_row_with_marks` call could
    // never come back with anything to paint BOLD on. Fixed to render a single visible cell.
    #[test]
    fn zero_width_mark_renders_the_cell_it_sits_before() {
        // `^` at byte 0 of "abc": the cell it sits before is 'a', column 0.
        let (text, spans) = layout_row_with_marks(b"abc", 8, 0, 20, std::slice::from_ref(&(0..0)));
        assert_eq!(text, "abc");
        assert_eq!(spans, vec![0..1]);
    }
    #[test]
    fn zero_width_mark_mid_line_renders_the_cell_it_sits_before() {
        // a zero-width match at byte 1 (between 'a' and 'b') sits before 'b', column 1.
        let (text, spans) = layout_row_with_marks(b"abc", 8, 0, 20, std::slice::from_ref(&(1..1)));
        assert_eq!(text, "abc");
        assert_eq!(spans, vec![1..2]);
    }
    #[test]
    fn zero_width_mark_at_content_end_renders_after_the_last_character() {
        // `$` at byte 3 (one past "abc"'s own last byte): no token starts there, so the fallback
        // is the layout's own final column (3), not the window's far-away right edge (20) -- a
        // short line in a wide window must not scatter its own end-of-line cursor 17 columns
        // away from the actual text.
        let (text, spans) = layout_row_with_marks(b"abc", 8, 0, 20, std::slice::from_ref(&(3..3)));
        assert_eq!(text, "abc");
        assert_eq!(spans, vec![3..4]);
    }
    #[test]
    fn zero_width_mark_on_empty_line_renders_at_the_first_cell() {
        // `^$` on an empty line: nothing was ever laid out, so both the token lookup and the
        // `col` fallback land on 0 -- the only sane "cursor" position on an empty line.
        let (text, spans) = layout_row_with_marks(b"", 8, 0, 20, std::slice::from_ref(&(0..0)));
        assert_eq!(text, "");
        assert_eq!(spans, vec![0..1]);
    }
    #[test]
    fn zero_width_mark_exactly_at_the_window_edge_renders_on_the_final_cell() {
        // "abc" exactly fills a 3-column window; `$` at byte 3 has nowhere of its own (column 3
        // is one PAST the window's own last column, 2) -- it renders on the final in-window
        // cell instead of vanishing, the cursor-cell precedent `render.rs`'s command line uses.
        let (text, spans) = layout_row_with_marks(b"abc", 8, 0, 3, std::slice::from_ref(&(3..3)));
        assert_eq!(text, "abc");
        assert_eq!(spans, vec![2..3]);
    }
    #[test]
    fn zero_width_mark_scrolled_off_the_left_is_dropped() {
        // "ab" (2 display columns) viewed through a window starting at column 5: there is no
        // sane cell within the visible window to clamp a scrolled-away cursor onto, unlike the
        // window's own right edge -- dropped, exactly like a wide mark entirely left of hscroll.
        let (text, spans) = layout_row_with_marks(b"ab", 8, 5, 10, std::slice::from_ref(&(0..0)));
        assert_eq!(text, "");
        assert!(spans.is_empty());
    }
    #[test]
    fn zero_width_marks_merge_with_an_adjacent_span() {
        // a zero-width mark ("sits before" the cell at column 1) touches the preceding 0..1
        // span's own end -- the same adjacency rule non-zero marks already follow
        // (`marks_merge_when_adjacent`), extending it rather than starting a second span.
        let (text, spans) = layout_row_with_marks(b"xyz", 8, 0, 20, &[0..1, 1..1]);
        assert_eq!(text, "xyz");
        assert_eq!(
            spans,
            vec![0..2],
            "1..1 (0 width) merges into the touching 0..1 span"
        );
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn never_panics_on_arbitrary_input(
            raw in proptest::collection::vec(any::<u8>(), 0..512),
            tab_stop in 0usize..17,
            hscroll in 0usize..300,
            cols in 0usize..160,
        ) {
            let _ = layout_row(&raw, tab_stop, hscroll, cols);
        }
        #[test]
        fn output_width_never_exceeds_cols(
            raw in proptest::collection::vec(any::<u8>(), 0..512),
            tab_stop in 0usize..17,
            hscroll in 0usize..300,
            cols in 0usize..160,
        ) {
            let out = layout_row(&raw, tab_stop, hscroll, cols);
            let width: usize = out
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
                .sum();
            prop_assert!(width <= cols, "width {} > cols {} in {:?}", width, cols, out);
        }
        #[test]
        fn output_never_contains_raw_controls(
            raw in proptest::collection::vec(any::<u8>(), 0..512),
            tab_stop in 0usize..17,
            hscroll in 0usize..300,
            cols in 0usize..160,
        ) {
            let out = layout_row(&raw, tab_stop, hscroll, cols);
            prop_assert!(
                out.chars().all(|c| !c.is_ascii_control()),
                "unexpanded control in {:?}",
                out
            );
        }
        // the regression oracle for the `layout_row_with_marks` refactor: compared against
        // `reference_layout_row`, a FROZEN, independent copy of the pre-refactor body (see its
        // own doc comment) -- deliberately NOT the live `layout_row`, which is now itself defined
        // as `layout_row_with_marks(.., &[]).0` and so would make a self-comparison here
        // tautological, incapable of catching any refactor bug regardless of arbitrary input.
        #[test]
        fn empty_marks_agree_with_the_frozen_pre_marks_reference(
            raw in proptest::collection::vec(any::<u8>(), 0..512),
            tab_stop in 0usize..17,
            hscroll in 0usize..300,
            cols in 0usize..160,
        ) {
            let (text, spans) = layout_row_with_marks(&raw, tab_stop, hscroll, cols, &[]);
            prop_assert_eq!(&text, &reference_layout_row(&raw, tab_stop, hscroll, cols));
            prop_assert!(spans.is_empty());
        }
    }
}
