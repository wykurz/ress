//! Blits a `ViewportRender` into a ratatui buffer, one display row per line,
//! clamped to the render area's width.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ress_core::document::ViewportRender;
/// Draws each display row at successive lines of `area`, clamped to its width;
/// rows beyond `area.height` are dropped.
pub fn render_viewport(view: &ViewportRender, area: Rect, buf: &mut Buffer) {
    for (i, line) in view.rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let y = area.y + i as u16;
        buf.set_stringn(area.x, y, line, area.width as usize, Style::default());
    }
}
/// Draws the transient pending-operation indicator over the bottom row of
/// `area`; a precursor of the real status line, shown only while a scan runs.
pub fn render_progress_row(label: &str, percent: u64, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height - 1;
    let style = Style::default().add_modifier(Modifier::REVERSED);
    // the viewport has already painted this row: clear it in the indicator's
    // style first, or file content bleeds through to the right of the text.
    for x in area.x..area.x.saturating_add(area.width) {
        buf[(x, y)].set_symbol(" ").set_style(style);
    }
    let text = format!("{label}… {percent}% — Esc cancels");
    buf.set_stringn(area.x, y, &text, area.width as usize, style);
}
/// Formats the status line's display states across the indexing lifecycle.
/// `total` (from `Document::index_total_lines`) is `Some` only once the
/// background index has finished AND reached EOF; `line` (from the status
/// worker's snapshot, via `Document::line_number` — see `draw`, which maps
/// a resolved `LineNumber::Known` to `Some` and everything else to `None`)
/// is `Some` once this particular anchor's number has resolved — the two
/// resolve independently. `done` (the frontier's own flag) is threaded
/// through explicitly rather than inferred from `total.is_none()`: an
/// error-shortened scan is `done` with `total` still `None` (its partial
/// count is real but not the file's total — see `Document::index_total_lines`),
/// so `total.is_none()` no longer implies "still indexing" on its own.
/// `indexing_lines` is the newline count seen so far
/// (`Frontier::lines_so_far`), shown only while `done` is still `false`.
/// An empty file's index reaches EOF trivially, so `total` is `Some(0)`
/// almost immediately, with `line` already `Some(1)` (the status worker's
/// own empty-file special case — see `ress_core::status`) — that
/// combination is special-cased ahead of the lifecycle match below, since
/// falling into the `L{n}/{t}` arm would render the misleading `L1/0`: a
/// line one of zero lines.
pub fn status_text(
    name: &str,
    line: Option<u64>,
    total: Option<u64>,
    done: bool,
    indexing_lines: u64,
    pct: u64,
) -> String {
    if total == Some(0) {
        return format!("{name} · empty · {pct}%");
    }
    match (line, total, done) {
        (Some(n), Some(t), _) => format!("{name} · L{n}/{t} · {pct}%"),
        (Some(n), None, _) => format!("{name} · L{n} · {pct}%"),
        (None, _, false) => format!("{name} · indexing… {indexing_lines} lines · {pct}%"),
        (None, _, true) => format!("{name} · L? · {pct}%"),
    }
}
/// Draws the status line over the bottom row of `area`: the reserved chrome
/// slot when no scan is pending (`render_progress_row` takes it instead
/// while one runs). Reversed, full-row clear first — same pattern as
/// `render_progress_row`, since the viewport has already painted this row.
pub fn render_status_line(text: &str, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height - 1;
    let style = Style::default().add_modifier(Modifier::REVERSED);
    for x in area.x..area.x.saturating_add(area.width) {
        buf[(x, y)].set_symbol(" ").set_style(style);
    }
    buf.set_stringn(area.x, y, text, area.width as usize, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_rows_clamped_to_width() {
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["hi".to_string(), "world!!".to_string()],
        };
        render_viewport(&view, area, &mut buf);
        let expected = Buffer::with_lines(["hi   ", "world"]);
        assert_eq!(buf, expected);
    }
    #[test]
    fn leaves_extra_rows_blank() {
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["ab".to_string()],
        };
        render_viewport(&view, area, &mut buf);
        let expected = Buffer::with_lines(["ab ", "   ", "   "]);
        assert_eq!(buf, expected);
    }
    #[test]
    fn progress_row_overwrites_the_bottom_row() {
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        render_progress_row("jumping to end", 42, area, &mut buf);
        let text: String = (0..30)
            .map(|x| buf[(x, 2)].symbol().chars().next().unwrap())
            .collect();
        assert!(
            text.starts_with(
                "jumping to end… 42% — Esc cancels"
                    .chars()
                    .take(30)
                    .collect::<String>()
                    .as_str()
            )
        );
    }
    #[test]
    fn progress_row_ignores_zero_height() {
        let area = Rect::new(0, 0, 10, 0);
        let mut buf = Buffer::empty(area);
        render_progress_row("x", 1, area, &mut buf);
    }
    #[test]
    fn progress_row_clears_leftover_viewport_content() {
        // the viewport paints the bottom row before the indicator does; file
        // content to the right of the text must not bleed through.
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        let full = "x".repeat(40);
        buf.set_stringn(0, 1, &full, 40, Style::default());
        render_progress_row("scrolling", 5, area, &mut buf);
        let text: String = (0..40)
            .map(|x| buf[(x, 1)].symbol().chars().next().unwrap())
            .collect();
        assert!(
            !text.contains('x'),
            "leftover viewport content visible: {text:?}"
        );
    }
    #[test]
    fn status_text_variants_cover_the_indexing_lifecycle() {
        // total is Some only once the background index has finished AND
        // reached EOF (Document::index_total_lines); line is Some once
        // this anchor's number has resolved; done is the frontier's own
        // flag. total.is_none() no longer implies "still indexing" on its
        // own — an error-shortened scan is done with total still None —
        // so done is threaded through explicitly instead.
        assert_eq!(
            status_text("big.log", Some(42), Some(500), true, 128, 8),
            "big.log · L42/500 · 8%"
        );
        assert_eq!(
            status_text("big.log", Some(42), None, false, 128, 8),
            "big.log · L42 · 8%"
        );
        assert_eq!(
            status_text("big.log", Some(42), None, true, 128, 8),
            "big.log · L42 · 8%",
            "a dead (error-shortened) scan renders the resolved line the same way"
        );
        assert_eq!(
            status_text("big.log", None, None, false, 128, 8),
            "big.log · indexing… 128 lines · 8%"
        );
        assert_eq!(
            status_text("big.log", None, Some(500), true, 128, 8),
            "big.log · L? · 8%"
        );
        assert_eq!(
            status_text("big.log", None, None, true, 128, 8),
            "big.log · L? · 8%",
            "a dead scan (done, but total never reached EOF) still renders L?, not indexing…"
        );
    }
    #[test]
    fn status_text_renders_empty_for_a_zero_line_total() {
        // an empty file's index reaches EOF trivially, so total is
        // Some(0) almost immediately, with line already Some(1) (the
        // status worker's own empty-file special case); falling into
        // the L{n}/{t} arm would render the misleading "L1/0".
        assert_eq!(
            status_text("empty.log", Some(1), Some(0), true, 0, 0),
            "empty.log · empty · 0%"
        );
    }
    #[test]
    fn status_line_renders_reversed_and_clears_the_row() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
        render_viewport(
            &ViewportRender {
                rows: vec!["xxxxxxxxxxxxxxxxxxxx".into(); 3],
            },
            Rect::new(0, 0, 20, 3),
            &mut buf,
        );
        render_status_line("f · L1 · 0%", Rect::new(0, 0, 20, 3), &mut buf);
        let bottom: String = (0..20).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert!(bottom.starts_with("f · L1 · 0%"));
        assert!(
            !bottom.contains('x'),
            "leftover viewport content must be cleared"
        );
        assert!(
            buf[(0, 2)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }
}
