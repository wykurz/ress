//! Width-aware layout of one logical line into a horizontal window of display
//! columns: expands tabs, measures unicode width, renders control bytes as caret
//! notation, and takes the `[hscroll, hscroll + cols)` column slice.
use unicode_width::UnicodeWidthChar;

/// Lays one logical line (`raw`, newline already stripped) out into the string
/// visible in display columns `[hscroll, hscroll + cols)`. Tabs advance to the
/// next `tab_stop`; wide chars occupy two columns; C0/DEL control bytes render as
/// caret notation (`^[`); invalid UTF-8 becomes the replacement char. A wide char
/// straddling either window edge renders as a single blank for its visible half.
/// An empty window (`cols == 0`) yields an empty string.
pub fn layout_row(raw: &[u8], tab_stop: usize, hscroll: usize, cols: usize) -> String {
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
    }
}
