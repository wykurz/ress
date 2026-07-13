//! Blits a `ViewportRender` into a ratatui buffer, one display row per line,
//! clamped to the render area's width — and paints the rest of the chrome
//! around it: the scrollbar gutter, the hint bar, the bottom row's
//! command/progress/status occupants, and the help overlay.
use crate::app::Mode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
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
/// The scrollbar thumb's position: `None` when there is nothing to draw a
/// gutter over at all — an empty file (`size == 0`) or no content rows to
/// draw it in (`rows == 0`) — else `(top, height)` in cells relative to the
/// content area's top. `height` is fixed at `1` for v1: the viewport's byte
/// span isn't cheaply known at render time (it depends on where lines wrap
/// and how far the scan actually read), so a proportional thumb height
/// would need either a second scan or a rough estimate presented as exact —
/// a one-cell position marker is honest about what's actually known, and
/// growing it into a proportional height is a later nicety, not a v1
/// requirement. `top` maps the anchor's proportion of the file onto the
/// `rows` cells the thumb can occupy: `anchor * rows / size`, clamped to
/// `rows - 1`. The multiplier is `rows`, not `rows - 1`: a real anchor is
/// always strictly less than `size` (it is the byte offset of some line
/// start, never past-EOF), so multiplying by `rows - 1` — which reaches the
/// last cell only when `anchor == size` exactly — left the last cell
/// permanently unreachable for every anchor an actual navigation produces,
/// including `G`. Multiplying by `rows` instead lets the top `~1/rows` of
/// the file's anchors floor to `rows - 1` on their own; the `.min(rows - 1)`
/// clamp then only has to catch the one remaining case, `anchor == size`
/// itself (already funneled here through the `anchor.min(size)` clamp
/// below), where the unclamped quotient would equal `rows` — one cell past
/// the thumb's actual range. Computed in `u128` — the house rule for
/// offset/size proportions (see `draw`'s status-percent calc in `app.rs`) —
/// because `anchor * rows` can overflow a native `u64` for a huge anchor
/// even at a two-digit `rows`.
pub fn scrollbar_thumb(anchor: u64, size: u64, rows: u16) -> Option<(u16, u16)> {
    if size == 0 || rows == 0 {
        return None;
    }
    // out-of-contract positions clamp like every scan object's.
    let anchor = anchor.min(size);
    let top = (anchor as u128 * rows as u128 / size.max(1) as u128).min(rows as u128 - 1) as u16;
    Some((top, 1))
}
/// Draws the scrollbar gutter over the rightmost column of `area` — the
/// CONTENT area, never the chrome rows below it, which callers must exclude
/// from `area` themselves (see `content_cols` in `app.rs`). Every row gets
/// either a DIM `│` track cell or, on `scrollbar_thumb`'s row(s) if any, a
/// plain-style `█` — each cell gets exactly one `set_style` call rather than
/// a track write later overwritten, since `Cell::set_style` patches onto the
/// existing style instead of replacing it, so a later `Style::default()`
/// would not actually clear an earlier DIM. A `None` from `scrollbar_thumb`
/// (nothing to mark) or a zero-width `area` (no column to draw into, which
/// `scrollbar_thumb` cannot itself detect since it never sees `area.width`)
/// is a no-op.
pub fn render_scrollbar(anchor: u64, size: u64, area: Rect, buf: &mut Buffer) {
    if area.width == 0 {
        return;
    }
    let Some((top, height)) = scrollbar_thumb(anchor, size, area.height) else {
        return;
    };
    let x = area.x + area.width - 1;
    let track_style = Style::default().add_modifier(Modifier::DIM);
    let thumb_end = top.saturating_add(height);
    for dy in 0..area.height {
        let cell = &mut buf[(x, area.y + dy)];
        if dy >= top && dy < thumb_end {
            cell.set_symbol("█").set_style(Style::default());
        } else {
            cell.set_symbol("│").set_style(track_style);
        }
    }
}
/// Draws the persistent zellij-style hint bar on the second-from-bottom row
/// of `area` — one row above the bottom row's command/progress/status
/// content, shown whenever the terminal has room for both (`chrome_rows`
/// reaching 2 in `app.rs`). DIM rather than REVERSED, matching the other
/// chrome rows' clear-then-write shape: it needs to read as distinct
/// chrome, not as a second status line. `area.height < 2` (no
/// second-from-bottom row to occupy) is a no-op rather than underflowing.
///
/// Hints advertise only what actually works: `help_fits` (`ChromeLayout::
/// help_fits` in `app.rs`, the same bound `can_enter`'s `Help` arm asks)
/// drops the `F1 help` segment from the Normal-mode hint when Help would
/// be refused — the hint bar's own bound (`chrome_rows` reaching 2, i.e.
/// `rows >= 4`) is far weaker than Help's real one (`rows >= 7 && cols >=
/// 5`), so a range of sizes shows the bar while F1 does nothing, and
/// advertising a refused key there would be actively misleading. Every
/// OTHER segment in every mode's hint is derived to need no equivalent
/// check: `j/k`, `d/u`, `gg/G`, `N% pct`, and `q` are unconditional
/// Normal-mode motions with no chrome gate of their own (`gg`/`G` and
/// `N%` are spelled to match what actually acts standalone — bare `g`
/// only arms the chord, bare `%` is `Action::None`, see `handle_key_
/// normal` — not what a chrome gate would refuse); `:N line` needs
/// Command to be enterable (`has_bottom_row(rows) && cols >= 3` —
/// `can_enter` in `app.rs`), whose rows half the bar's own `rows >= 4`
/// already satisfies — but NOT its cols half: at `cols` 1 or 2 the bar
/// shows while `:` is refused. No `help_fits`-style flag is needed for
/// that gap because geometry closes it: this row renders through
/// `set_stringn`, clamped to `area.width`, and the `:N line` segment
/// starts 39 characters into both Normal variants, so everywhere the
/// gate refuses (`cols < 3`) the segment is truncated away long before
/// the advertisement could reach the screen. That argument leans on the
/// segment's position — moving `:N line` near the front of the string
/// would break it and require a real check like F1's. Command's and
/// Help's own hint text mentions neither `F1` nor `:`, so neither
/// depends on `help_fits` either.
pub fn render_hint_bar(mode: Mode, help_fits: bool, area: Rect, buf: &mut Buffer) {
    if area.height < 2 {
        return;
    }
    let y = area.y + area.height - 2;
    let style = Style::default().add_modifier(Modifier::DIM);
    for x in area.x..area.x.saturating_add(area.width) {
        buf[(x, y)].set_symbol(" ").set_style(style);
    }
    let text = match mode {
        Mode::Normal if help_fits => {
            "j/k scroll · d/u half · gg/G top/end · :N line · N% pct · F1 help · q quit"
        }
        Mode::Normal => "j/k scroll · d/u half · gg/G top/end · :N line · N% pct · q quit",
        Mode::Command => "Enter go · Esc cancel · Backspace edit",
        Mode::Help => "any key closes",
    };
    buf.set_stringn(area.x, y, text, area.width as usize, style);
}
/// Draws the transient pending-operation indicator over the bottom row of
/// `area`; a precursor of the real status line, shown only while a scan
/// runs. `esc_cancels` is the caller's truth about what `Esc` actually does
/// RIGHT NOW, not a fact this function can determine itself from `label`/
/// `percent` alone: the row can be visible while `Mode::Help`'s overlay
/// covers only the content area (`ChromeLayout`'s bottom-row precedence
/// puts `Progress` ahead of `Status` regardless of mode, `Command`
/// excepted), but `Mode::Help`'s any-key-closes contract means `Esc` there
/// closes Help rather than cancelling the scan — advertising "Esc cancels"
/// in that state would be a lie the row itself has no way to detect. The
/// `— Esc cancels` segment is included only when `esc_cancels` is true.
pub fn render_progress_row(
    label: &str,
    percent: u64,
    esc_cancels: bool,
    area: Rect,
    buf: &mut Buffer,
) {
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
    let text = if esc_cancels {
        format!("{label}… {percent}% — Esc cancels")
    } else {
        format!("{label}… {percent}%")
    };
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
/// Draws the in-progress `:` command line over the bottom row of `area`: the
/// reserved chrome slot's top precedence, ahead of both the progress row and
/// the status line, while `Mode::Command` is live. Reversed, full-row clear
/// first — same pattern as `render_progress_row`/`render_status_line`. The
/// buffer is shown as `:{buffer}` followed by a trailing `_` cursor cell in
/// the same reversed style as the rest of the row, standing in for a real
/// terminal cursor.
///
/// LEFT-truncated, not right, when `:{buffer}_` is longer than `area.width`:
/// the cursor cell is always the LAST cell shown — it marks the one point
/// that matters, where the next keystroke lands, so it is the one thing
/// that must never scroll off — with the buffer's own tail filling
/// backward from it, and the leading `:` sacrificed first if there still
/// isn't room for everything. Concretely: show exactly the last
/// `min(area.width, len(":{buffer}_"))` characters of the full string, by
/// character count, not by byte length. Char slicing is panic-safe
/// whatever the buffer holds, but the never-scrolls-off guarantee itself
/// needs one char to equal one terminal cell — true only because the
/// buffer is digits-only (`handle_key_command`'s grammar); a wide
/// character would undercount its cells, and `set_stringn`'s forward
/// clamp could then drop the cursor again. Right-truncating instead —
/// showing the buffer's HEAD, as
/// an earlier version of this function did — let the visible text diverge
/// from what `Enter` actually executes: at `area.width == 3` with buffer
/// `"123"`, right-truncation showed `":12"` while `Enter` still jumped to
/// line `123`, not `12`. Left-truncation shows `"23_"` there instead —
/// always in sync with what committing right now would do, at the cost of
/// the leading `:` (dropped first) and then the buffer's own earlier
/// digits (dropped next) as the row narrows further.
pub fn render_command_line(buffer: &str, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height - 1;
    let style = Style::default().add_modifier(Modifier::REVERSED);
    for x in area.x..area.x.saturating_add(area.width) {
        buf[(x, y)].set_symbol(" ").set_style(style);
    }
    let text = format!(":{buffer}_");
    let width = area.width as usize;
    let skip = text.chars().count().saturating_sub(width);
    let visible: String = text.chars().skip(skip).collect();
    buf.set_stringn(area.x, y, &visible, width, style);
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
/// The full keymap, one hint per line — mirrors the README's key-bindings
/// table, plus `:N` and `F1` (the two bindings Task 5 also adds to the
/// README itself). The overlay does not read the README file at runtime,
/// so this list and that table are kept in sync by hand, not by
/// construction.
const HELP_LINES: [&str; 14] = [
    "j / k / ↓ / ↑ — line down / up",
    "d / u, Ctrl-d / Ctrl-u — half page down / up",
    "Ctrl-f / Space / PgDn — full page down",
    "Ctrl-b / PgUp — full page up",
    "gg / ge / G — top / end of file",
    "<count>G / <count>gg — jump to line N",
    "<count>% — jump to that byte percentage",
    ":N — jump to line N",
    "h / l / ← / → — horizontal scroll",
    "mouse wheel — scroll three lines",
    "Ctrl-l — force redraw",
    "Esc — cancel a pending operation, count, or chord",
    "F1 — toggle help",
    "q / Ctrl-c — quit",
];
/// Draws the help overlay: a centered, bordered panel over `area`, sized
/// `min(area.width − 2, 60)` by `min(area.height − 2, 16)` so it never
/// touches the screen edge and never grows past a comfortable size on a
/// huge terminal. `Clear`d first so no content already painted in `area`
/// bleeds through — `Block` only paints its own border and title cells, so
/// the interior stays exactly as `Clear` left it apart from what the loop
/// below writes over it. Keymap lines beyond the panel's interior are
/// dropped, the same clamp-by-breaking pattern `render_viewport` uses; an
/// `area` too small to fit any panel at all (width or height clamps to
/// zero) draws nothing rather than a degenerate sliver. Note that this
/// function's own guard only rules out a zero-sized PANEL, not a
/// zero-sized INTERIOR — `Block::bordered()`'s border eats two more cells
/// off each dimension, so a small-but-nonzero panel still draws a
/// bordered, titled box with no keymap line inside it if called with one.
/// This function relies on its caller to rule that out before ever
/// invoking it: `app.rs`'s `help_overlay_fits` is the single source for
/// the true, interior-based bound in both dimensions — changing the
/// margin/cap/border arithmetic here means updating that bound too.
///
/// The title states the one fact nothing else on screen states: the
/// listed keymap describes `Mode::Normal`, not this overlay. It does NOT
/// also say Help's own `any-key-closes` arm means none of those keys —
/// Esc, q, included — perform their listed action from in here (an
/// earlier version of this title tried to say both; a review found that
/// version truncating closes-first in an ordinary size band, leaving the
/// scope-free half on screen next to a fully-readable, wrong-looking
/// keymap — a title's tail is the first casualty of a narrow width, so a
/// two-fact title is only as reliable as its shorter half). The
/// closes-truth doesn't need the title, because it is already
/// guaranteed to be on screen by a chain independent of it: Help only
/// opens where `can_enter(Mode::Help, rows, cols)` — `help_overlay_fits`
/// — holds, which needs raw `rows >= 7`; `chrome_rows(rows) == 2` for
/// every `rows >= 4`, so any `rows` Help can open at also satisfies
/// that; `ChromeLayout::of`'s `hint_bar` field is exactly `chrome_rows
/// (rows) == 2`, so the hint bar is showing whenever Help is; and
/// `render_hint_bar`'s `Mode::Help` arm is unconditionally `"any key
/// closes"` (no `help_fits`-style gate, unlike Normal's `F1` segment).
/// That string is 14 characters, and `render_hint_bar` is handed the
/// full, un-reduced frame `area` (see `paint_frame`), so it paints
/// untruncated wherever `cols >= 14` — Help's own floor is only `cols >=
/// 5`, so a `5..14` sliver could still clip the hint bar too, but that
/// same width already clips every `HELP_LINES` entry (the shortest is
/// far longer than 14 columns) into illegible fragments, not the
/// fully-readable-but-wrong keymap the trap needs; the reviewer's own
/// repro band sits entirely above 14 regardless. The title costs no
/// interior space either way: `Block::inner()` only reserves a row for
/// the top edge once, whether that reservation comes from `Borders::TOP`
/// or a title or both, and this block already sets every border — so
/// `help_overlay_fits`'s bound needs no change for a title-only edit
/// like this one. A header LINE instead would have; that's why this
/// stays a title.
pub fn render_help_overlay(area: Rect, buf: &mut Buffer) {
    let width = area.width.saturating_sub(2).min(60);
    let height = area.height.saturating_sub(2).min(16);
    if width == 0 || height == 0 {
        return;
    }
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    ratatui::widgets::Clear.render(panel, buf);
    let block = ratatui::widgets::Block::bordered().title(" help — normal-view keys ");
    let inner = block.inner(panel);
    block.render(panel, buf);
    for (i, line) in HELP_LINES.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let y = inner.y + i as u16;
        buf.set_stringn(inner.x, y, line, inner.width as usize, Style::default());
    }
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
    fn progress_row_shows_esc_cancels_when_esc_actually_cancels() {
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        render_progress_row("jumping to end", 42, true, area, &mut buf);
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
    fn progress_row_drops_esc_cancels_when_esc_does_not_cancel() {
        // the P2 finding: with Help open over a pending nav, Esc closes
        // Help (Mode::Help's any-key-closes contract) rather than
        // cancelling the scan — the row must not advertise a key that
        // doesn't do what it says.
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        render_progress_row("jumping to end", 42, false, area, &mut buf);
        let text: String = (0..30).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert!(
            text.starts_with("jumping to end… 42%"),
            "unexpected text: {text:?}"
        );
        assert!(
            !text.contains("Esc"),
            "must not advertise Esc when it doesn't cancel: {text:?}"
        );
    }
    #[test]
    fn progress_row_ignores_zero_height() {
        let area = Rect::new(0, 0, 10, 0);
        let mut buf = Buffer::empty(area);
        render_progress_row("x", 1, true, area, &mut buf);
    }
    #[test]
    fn progress_row_clears_leftover_viewport_content() {
        // the viewport paints the bottom row before the indicator does; file
        // content to the right of the text must not bleed through.
        let area = Rect::new(0, 0, 40, 2);
        let mut buf = Buffer::empty(area);
        let full = "x".repeat(40);
        buf.set_stringn(0, 1, &full, 40, Style::default());
        render_progress_row("scrolling", 5, true, area, &mut buf);
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
    #[test]
    fn command_line_renders_reversed_with_the_buffer() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 2));
        render_command_line("150", Rect::new(0, 0, 20, 2), &mut buf);
        let bottom: String = (0..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert!(bottom.starts_with(":150"));
        assert!(
            buf[(0, 1)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }
    #[test]
    fn command_line_cursor_cell_follows_the_buffer_reversed() {
        // pins the exact cursor shape: a trailing `_` cell, same REVERSED
        // style as the rest of the row, right after `:{buffer}`.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 2));
        render_command_line("42", Rect::new(0, 0, 20, 2), &mut buf);
        assert_eq!(buf[(3, 1)].symbol(), "_");
        assert!(
            buf[(3, 1)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }
    #[test]
    fn command_line_left_truncates_so_the_cursor_and_tail_survive() {
        // the P2 finding's exact repro: right-truncation used to show
        // ":12" for buffer "123" at width 3 while Enter still executed
        // 123 — the visible text and the executed command diverged.
        // Left-truncation keeps the cursor cell last always, filling
        // backward with the buffer's own tail and sacrificing the
        // leading `:` first.
        let buffer = "123";
        // width 3: `:123_` is 5 chars: last 3 = "23_" (colon AND the
        // leading digit both dropped, tail + cursor survive).
        let area3 = Rect::new(0, 0, 3, 1);
        let mut buf3 = Buffer::empty(area3);
        render_command_line(buffer, area3, &mut buf3);
        let row3: String = (0..3).map(|x| buf3[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row3, "23_");
        // width 4: last 4 of ":123_" = "123_" (just the colon dropped).
        let area4 = Rect::new(0, 0, 4, 1);
        let mut buf4 = Buffer::empty(area4);
        render_command_line(buffer, area4, &mut buf4);
        let row4: String = (0..4).map(|x| buf4[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row4, "123_");
        // width 5: ":123_" is exactly 5 chars — everything fits, nothing
        // is sacrificed.
        let area5 = Rect::new(0, 0, 5, 1);
        let mut buf5 = Buffer::empty(area5);
        render_command_line(buffer, area5, &mut buf5);
        let row5: String = (0..5).map(|x| buf5[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row5, ":123_");
    }
    #[test]
    fn command_line_left_truncation_always_shows_the_rightmost_digits() {
        // a longer buffer than the row is wide, to show the windowing
        // rule tracks the buffer's OWN tail, not just its last few chars
        // relative to a short buffer.
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        render_command_line("1234567890", area, &mut buf);
        let row: String = (0..5).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "7890_");
    }
    #[test]
    fn hint_bar_shows_normal_mode_hints_including_f1_when_help_fits() {
        let area = Rect::new(0, 0, 80, 5);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Normal, true, area, &mut buf);
        let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(text.starts_with(
            "j/k scroll · d/u half · gg/G top/end · :N line · N% pct · F1 help · q quit"
        ));
        assert!(buf[(0, 3)].style().add_modifier.contains(Modifier::DIM));
        assert!(
            !buf[(0, 3)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "the hint bar must read as distinct chrome, not a second status line"
        );
        // the bottom row itself is untouched by the hint bar.
        let bottom: String = (0..80).map(|x| buf[(x, 4)].symbol().to_string()).collect();
        assert!(bottom.chars().all(|c| c == ' '));
    }
    #[test]
    fn hint_bar_drops_the_f1_hint_when_help_does_not_fit() {
        // the P3 finding: F1 must not be advertised at a size where
        // pressing it is refused — the rest of the Normal-mode hint is
        // otherwise unaffected.
        let area = Rect::new(0, 0, 80, 5);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Normal, false, area, &mut buf);
        let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(
            text.starts_with("j/k scroll · d/u half · gg/G top/end · :N line · N% pct · q quit"),
            "unexpected hint text: {text:?}"
        );
        assert!(
            !text.contains("F1"),
            "F1 must not be advertised when Help does not fit: {text:?}"
        );
    }
    #[test]
    fn hint_bar_shows_command_mode_hints() {
        let area = Rect::new(0, 0, 80, 5);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Command, true, area, &mut buf);
        let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(text.starts_with("Enter go · Esc cancel · Backspace edit"));
    }
    #[test]
    fn hint_bar_shows_help_mode_hints() {
        let area = Rect::new(0, 0, 80, 5);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Help, true, area, &mut buf);
        let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(text.starts_with("any key closes"));
    }
    #[test]
    fn hint_bar_command_and_help_hints_are_unaffected_by_whether_help_fits() {
        // derived claim, pinned directly: F1 was the only hint segment
        // gated on Help's own fit — Command's and Help's hint text
        // mention neither F1 nor `:`, so neither should ever change based
        // on `help_fits`.
        let area = Rect::new(0, 0, 80, 5);
        for help_fits in [false, true] {
            let mut buf = Buffer::empty(area);
            render_hint_bar(Mode::Command, help_fits, area, &mut buf);
            let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
            assert!(
                text.starts_with("Enter go · Esc cancel · Backspace edit"),
                "help_fits={help_fits}"
            );
            let mut buf2 = Buffer::empty(area);
            render_hint_bar(Mode::Help, help_fits, area, &mut buf2);
            let text2: String = (0..80).map(|x| buf2[(x, 3)].symbol().to_string()).collect();
            assert!(text2.starts_with("any key closes"), "help_fits={help_fits}");
        }
    }
    #[test]
    fn hint_bar_clears_leftover_viewport_content() {
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        let full = "x".repeat(40);
        buf.set_stringn(0, 3, &full, 40, Style::default());
        render_hint_bar(Mode::Normal, true, area, &mut buf);
        let text: String = (0..40).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(
            !text.contains('x'),
            "leftover viewport content visible: {text:?}"
        );
    }
    #[test]
    fn hint_bar_ignores_areas_too_short_for_a_second_row() {
        // must not underflow/panic when there is no second-from-bottom row.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Normal, true, area, &mut buf);
    }
    #[test]
    fn help_overlay_draws_a_bordered_titled_panel_with_a_sampled_hint() {
        // area 80x24 -> panel min(78,60)x min(22,16) = 60x16, centered at
        // x=(80-60)/2=10, y=(24-16)/2=4; inner (after 1-cell borders on all
        // sides) is x=11,y=5,58x14.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_help_overlay(area, &mut buf);
        assert_ne!(
            buf[(10, 4)].symbol(),
            " ",
            "top-left corner should carry a border glyph"
        );
        let top_row: String = (10..70).map(|x| buf[(x, 4)].symbol().to_string()).collect();
        assert!(
            top_row.contains("help"),
            "title missing from the top border: {top_row:?}"
        );
        let mut found_quit_hint = false;
        for y in 5..19 {
            let row: String = (11..69).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if row.contains("q / Ctrl-c") {
                found_quit_hint = true;
            }
        }
        assert!(found_quit_hint, "expected the quit hint inside the panel");
    }
    #[test]
    fn help_overlay_title_states_scope_only_the_hint_bar_carries_closure() {
        // pass 6, then independently pass 11's Codex finding, flagged the
        // same reading trap: the panel lists Esc/q's NORMAL-mode effects,
        // but any-key-closes means neither actually fires from inside the
        // overlay itself. Pass 11 first tried fitting both truths into the
        // title; a later review found that title truncating closes-first
        // in an ordinary size band, leaving a fully-readable, wrong-
        // looking keymap on screen with no disclaimer next to it. The
        // title now carries only the fact nothing else on screen
        // states — scope — and leans on a derived guarantee for the
        // rest: wherever Help can open at all, the chrome ladder already
        // forces the hint bar to show `Mode::Help`'s own unconditional
        // "any key closes" text (see render_help_overlay's doc comment
        // for the full rows/chrome_rows/hint_bar chain, and
        // hint_bar_shows_help_mode_hints, above, for that text's own
        // pin) — so the title doesn't need to repeat it, and a shorter
        // title is also a title less likely to be the one that
        // truncates.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_help_overlay(area, &mut buf);
        let top_row: String = (10..70).map(|x| buf[(x, 4)].symbol().to_string()).collect();
        assert!(
            top_row.contains("normal"),
            "title must name the keys as the normal view's own: {top_row:?}"
        );
        assert!(
            !top_row.contains("any key closes"),
            "closure belongs to the hint bar now, not the title: {top_row:?}"
        );
    }
    #[test]
    fn help_overlay_clears_leftover_content_inside_the_panel() {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let full = "x".repeat(80);
        for y in 0..24 {
            buf.set_stringn(0, y, &full, 80, Style::default());
        }
        render_help_overlay(area, &mut buf);
        // deep inside the panel, well clear of the border/title row.
        let row: String = (15..60)
            .map(|x| buf[(x, 10)].symbol().to_string())
            .collect();
        assert!(
            !row.contains('x'),
            "leftover content inside the panel must be cleared: {row:?}"
        );
    }
    #[test]
    fn help_overlay_ignores_an_area_too_small_to_fit_a_panel() {
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_help_overlay(area, &mut buf); // must not panic
    }
    #[test]
    fn scrollbar_thumb_table() {
        // anchor 0 always lands on cell 0, regardless of size/rows.
        assert_eq!(scrollbar_thumb(0, 100, 11), Some((0, 1)));
        // exact-ish midpoint: 50 * 11 / 100 = 5.5, floors to 5 — the same
        // value the pre-fix formula also gave (50 * 10 / 100 = 5.0): the
        // fix only changes anchors near the ends of the range, not the
        // middle.
        assert_eq!(scrollbar_thumb(50, 100, 11), Some((5, 1)));
        // truncating division: 3 * 4 / 7 = 12 / 7 = 1.71..., floors to 1 —
        // pins the floor-not-round behavior of integer division (also
        // unchanged from the pre-fix 3 * 3 / 7 = 1.28... -> 1).
        assert_eq!(scrollbar_thumb(3, 7, 4), Some((1, 1)));
        // an anchor at (or past) EOF lands on the last cell, rows - 1: the
        // unclamped quotient is exactly rows (100 * 11 / 100 = 11), caught
        // by the .min(rows - 1) clamp — this used to be the ONLY anchor
        // that reached the last cell at all; see the 91/90 pair below for
        // what the fix changes about that.
        assert_eq!(scrollbar_thumb(100, 100, 11), Some((10, 1)));
        // an out-of-contract anchor well past size clamps to size first
        // (the existing anchor.min(size) guard), landing here exactly the
        // way anchor == size does above.
        assert_eq!(scrollbar_thumb(100 + 999, 100, 11), Some((10, 1)));
        // the fix: the last cell is now reachable by an in-contract anchor
        // strictly less than size, not just anchor == size. At this
        // table's rows (11), 91 is the smallest such anchor:
        // 91 * 11 / 100 = 10.01 -> 10 (the last cell), while
        // 90 * 11 / 100 = 9.9 -> 9 stays one cell short. Pre-fix, EVERY
        // anchor from 1 through 99 floored to at most 9 — a real anchor is
        // always < size (it is a byte offset, never past-EOF), so the last
        // cell was unreachable for any anchor an actual navigation
        // produces, including `G`.
        assert_eq!(scrollbar_thumb(91, 100, 11), Some((10, 1)));
        assert_eq!(scrollbar_thumb(90, 100, 11), Some((9, 1)));
        // the finding's exact repro: at 2 content rows (1 cell of headroom
        // under rows - 1), anchor = size - 1 (one byte short of EOF, about
        // as close to the end as an in-contract anchor gets) used to floor
        // to row 0 for every such anchor (99 * 1 / 100 = 0.99 -> 0) — not
        // just this one value, the entire non-EOF range. Under the fix:
        // 99 * 2 / 100 = 1.98 -> 1, the last row.
        assert_eq!(scrollbar_thumb(99, 100, 2), Some((1, 1)));
        // no gutter at all: an empty file, or no content rows to draw it in.
        assert_eq!(scrollbar_thumb(0, 0, 11), None);
        assert_eq!(scrollbar_thumb(0, 100, 0), None);
        // u128 overflow shape: anchor * rows overflows u64 (by ~50x here)
        // for a huge anchor and a merely two-digit rows — exactly why the
        // math runs in u128. A plain u64 multiplication would panic
        // (debug) or wrap (release) before the division ever ran. The
        // ~0.5 fraction here lands well clear of the clamp boundary, so
        // the result (49) is the same value the pre-fix formula also
        // produced for this input — the fix only changes results near the
        // ends of the range.
        let anchor = u64::MAX / 2;
        assert!(
            anchor.checked_mul(100).is_none(),
            "the case must actually overflow u64 to test what it claims to"
        );
        assert_eq!(scrollbar_thumb(anchor, u64::MAX, 100), Some((49, 1)));
    }
    #[test]
    fn scrollbar_draws_a_dim_track_and_one_thumb_in_the_rightmost_column() {
        // content_area is the top 4 rows of a 6-row buffer; rows 4-5 stand
        // in for the chrome rows below it, which render_scrollbar must
        // never touch — callers are responsible for excluding them from
        // `area` in the first place.
        let content_area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        // anchor at the midpoint of a 100-byte file over 4 rows: top = 50
        // * 4 / 100 = 2.0, exactly the middle of the 4 available cells (0
        // through 3). Pre-fix this was top = 50 * 3 / 100 = 1 (rows - 1 =
        // 3 thumb cells) — the same fix that makes the last cell reachable
        // also nudges this exact-midpoint case from 1 to 2.
        render_scrollbar(50, 100, content_area, &mut buf);
        for y in 0..4 {
            let cell = &buf[(9, y)];
            if y == 2 {
                assert_eq!(cell.symbol(), "█", "thumb cell at row {y}");
                assert!(
                    !cell.style().add_modifier.contains(Modifier::DIM),
                    "the thumb is plain style, not DIM like the track"
                );
            } else {
                assert_eq!(cell.symbol(), "│", "track cell at row {y}");
                assert!(cell.style().add_modifier.contains(Modifier::DIM));
            }
        }
        let thumb_count = (0..4).filter(|&y| buf[(9, y)].symbol() == "█").count();
        assert_eq!(thumb_count, 1, "v1's thumb is a single fixed-height cell");
        // the chrome rows below the content area are untouched.
        for y in 4..6 {
            assert_eq!(buf[(9, y)].symbol(), " ");
        }
        // no bleed into the text columns.
        for x in 0..9 {
            for y in 0..4 {
                assert_eq!(buf[(x, y)].symbol(), " ", "unexpected write at ({x},{y})");
            }
        }
    }
    #[test]
    fn scrollbar_is_a_no_op_when_scrollbar_thumb_returns_none() {
        let area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(area);
        render_scrollbar(0, 0, area, &mut buf); // size 0 -> None
        assert_eq!(buf, Buffer::empty(area));
    }
}
