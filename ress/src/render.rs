//! Blits a `ViewportRender` into a ratatui buffer, one display row per line,
//! clamped to the render area's width — and paints the rest of the chrome
//! around it: the scrollbar gutter, the hint bar, the bottom row's
//! command/progress/status occupants, and the help overlay.
use crate::app::{CommandKind, Mode};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use ress_core::document::ViewportRender;
use ress_core::search::SearchSummary;
/// Draws each display row at successive lines of `area`, clamped to its width;
/// rows beyond `area.height` are dropped. Restyles each row's own search-match
/// spans (`view.marks`) REVERSED, and the ACTIVE match's own span additionally
/// BOLD -- on top of, never instead of, REVERSED -- after the row's own text
/// is written: `set_style` PATCHES the modifier bits already on a cell rather
/// than replacing them (`Cell::set_style` inserts; `render_scrollbar`'s own
/// doc comment names the identical lesson), so the BOLD pass only needs to
/// add its own bit, relying on the REVERSED pass just before it to have
/// already landed on every one of `current`'s own cells (`current` is always
/// contained in one of `spans`, never a disjoint range of its own -- see
/// `RowMarks`, `ress-core`).
pub fn render_viewport(view: &ViewportRender, area: Rect, buf: &mut Buffer) {
    for (i, line) in view.rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let y = area.y + i as u16;
        buf.set_stringn(area.x, y, line, area.width as usize, Style::default());
        let Some(row_marks) = view.marks.get(i) else {
            continue;
        };
        for span in &row_marks.spans {
            restyle_span(buf, area, y, span, Modifier::REVERSED);
        }
        if let Some(current) = &row_marks.current {
            restyle_span(buf, area, y, current, Modifier::BOLD);
        }
    }
}
/// Patches `modifier` over `span`'s own cells on row `y`, clamped to `area`'s
/// own width. `Buffer::set_style` only clamps to the WHOLE buffer's bounds,
/// not to this narrower `area`, so a span computed against a wider `cols`
/// than this frame's live area -- the same resize race `draw`'s own comment
/// in `app.rs` describes for `fetch_cols` vs. the paint-time `text_area` --
/// could otherwise paint past `area`'s own right edge, into the scrollbar
/// gutter or the chrome beyond it. An empty clamped span (entirely past
/// `area.width`) is a no-op rather than an inverted, panicking range.
fn restyle_span(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    span: &std::ops::Range<u16>,
    modifier: Modifier,
) {
    let start = span.start.min(area.width);
    let stop = span.end.min(area.width);
    if start >= stop {
        return;
    }
    buf.set_style(
        Rect {
            x: area.x + start,
            y,
            width: stop - start,
            height: 1,
        },
        Style::default().add_modifier(modifier),
    );
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
/// Maps `buckets`' own 1024-bin histogram onto `rows` on-screen tick rows: row `r` is marked iff
/// any NONZERO bucket's own byte span intersects row `r`'s. Bucket `b` spans `[b*size/1024,
/// (b+1)*size/1024)` — the same proportion `SweepAnalysis::record_match` (`ress-core`'s
/// `search.rs`) computes a bucket INDEX from, read here as a bucket's own SPAN instead — and row
/// `r` spans `[r*size/rows, (r+1)*size/rows)` by the identical proportion. Both spans are
/// RATIONAL, not integer: the overlap test below compares them by cross-multiplication rather
/// than by computing `b*size/1024` etc. as `u128` values first and comparing THOSE — an earlier
/// version did exactly that, and integer division floors, so any bucket under 1 byte wide (i.e.
/// EVERY bucket whenever `size < 1024`) floored to an empty, self-touching span that could never
/// overlap anything; see the inline comment below for the exact identity and its own overflow
/// bound. `size == 0` (nothing to spread a histogram over) or `rows == 0` (no cells to mark)
/// returns an empty `Vec` rather than dividing by zero or allocating a buffer nothing will ever
/// index into — the same "nothing to draw" shape `scrollbar_thumb`'s own `None` already gives
/// `render_scrollbar` for the thumb.
pub fn tick_rows(buckets: &[u16; 1024], size: u64, rows: u16) -> Vec<bool> {
    if size == 0 || rows == 0 {
        return Vec::new();
    }
    let size = size as u128;
    let rows = rows as u128;
    (0..rows)
        .map(|r| {
            buckets.iter().enumerate().any(|(b, &count)| {
                if count == 0 {
                    return false;
                }
                // the exact RATIONAL overlap test, via cross-multiplication -- never flooring
                // either boundary to an integer first. Flooring `b*size/1024`/`(b+1)*size/1024`
                // to integer byte offsets (the previous implementation) collapses a bucket to an
                // empty `[k,k)` span whenever its true width is under 1 byte -- i.e. EVERY bucket
                // once `size < 1024` -- so `row_start < bucket_end` could then never hold and no
                // tick could ever show for a small file. `[b/1024,(b+1)/1024)` and
                // `[r/rows,(r+1)/rows)` (both real-valued, as fractions of `size`) intersect iff
                // `b/1024 < (r+1)/rows && r/rows < (b+1)/1024`; multiplying both sides of each by
                // the positive `size*1024*rows` clears every denominator without ever rounding a
                // boundary down. `b*size*rows` and its siblings stay far under `u128::MAX` even
                // at the largest `b`/`size`/`rows` this function accepts (~1.2e27 vs. ~3.4e38).
                let b = b as u128;
                b * size * rows < (r + 1) * size * 1024 && r * size * 1024 < (b + 1) * size * rows
            })
        })
        .collect()
}
/// Draws the scrollbar gutter over the rightmost column of `area` — the
/// CONTENT area, never the chrome rows below it, which callers must exclude
/// from `area` themselves (see `content_cols` in `app.rs`). Every row gets
/// exactly one of three cells, the thumb branch checked first and
/// unconditionally: a plain-style `█` on `scrollbar_thumb`'s own row(s), a
/// DIM `·` on any OTHER row `ticks` marks (a search's match density —
/// `tick_rows`, above), or a plain DIM `│` track cell otherwise — so the
/// thumb always wins a row it shares with a tick, exactly as it already
/// wins the plain track. Each cell gets exactly one `set_style` call rather
/// than a track write later overwritten, since `Cell::set_style` patches
/// onto the existing style instead of replacing it, so a later
/// `Style::default()` would not actually clear an earlier DIM. `ticks` is
/// read defensively by index (`ticks.get(dy)`, never `ticks[dy]`) rather
/// than assumed to be exactly `area.height` long, mirroring
/// `render_viewport`'s own `view.marks.get(i)` — a caller-side row-count
/// mismatch (there is none today: `paint_frame` in `app.rs` always calls
/// `tick_rows` against this very `area`'s own height) then just draws a
/// plain track for the unmatched rows rather than panicking. `ticks: None`
/// reproduces exactly what this function drew before ticks existed at all —
/// every row is thumb-or-plain-track, never `·`. A `None` from
/// `scrollbar_thumb` (nothing to mark) or a zero-width `area` (no column to
/// draw into, which `scrollbar_thumb` cannot itself detect since it never
/// sees `area.width`) is a no-op, `ticks` notwithstanding.
pub fn render_scrollbar(
    anchor: u64,
    size: u64,
    area: Rect,
    buf: &mut Buffer,
    ticks: Option<&[bool]>,
) {
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
        let ticked = ticks
            .and_then(|t| t.get(dy as usize))
            .copied()
            .unwrap_or(false);
        if dy >= top && dy < thumb_end {
            cell.set_symbol("█").set_style(Style::default());
        } else if ticked {
            cell.set_symbol("·").set_style(track_style);
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
/// normal` — not what a chrome gate would refuse); `:N line` and `/find`
/// both need `Mode::Command(_)` to be enterable (`has_bottom_row(rows) &&
/// cols >= 3` — `can_enter`'s single, kind-agnostic `Command(_)` arm in
/// `app.rs`, shared by `CommandKind::Colon` and `CommandKind::Search`
/// alike), whose rows half the bar's own `rows >= 4` already satisfies —
/// but NOT its cols half: at `cols` 1 or 2 the bar shows while `:`/`/`/`?`
/// are all refused. No `help_fits`-style flag is needed for that gap
/// because geometry closes it: this row renders through `set_stringn`,
/// clamped to `area.width`, and `:N line` starts 39 characters into both
/// Normal variants with `/find` right behind it at 49 (recomputed after
/// `/find` was added — both positions are unchanged between the two
/// variants; only what follows `/find` moves), so everywhere the gate
/// refuses (`cols < 3`) BOTH segments are truncated away long before
/// either advertisement could reach the screen. That argument leans on
/// the segments' own position — moving either one near the front of the
/// string would break it and require a real check like F1's. Command's
/// own hint text — `Colon`'s and `Search`'s alike — and Help's mention
/// neither `F1` nor `:`/`/`/`?`, so none of the three depend on
/// `help_fits` either.
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
            "j/k scroll · d/u half · gg/G top/end · :N line · /find · N% pct · F1 help · q quit"
        }
        Mode::Normal => "j/k scroll · d/u half · gg/G top/end · :N line · /find · N% pct · q quit",
        Mode::Command(CommandKind::Colon) => "Enter go · Esc cancel · Backspace edit",
        Mode::Command(CommandKind::Search { .. }) => "Enter search · Esc cancel · Backspace edit",
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
/// The status line's search segment, appended to `status_text`'s own result while a search is
/// active (`draw`, `app.rs`). `None` when `summary`'s own generation doesn't match
/// `active_generation` (`ActiveSearch::generation`) — a snapshot left over from a SUPERSEDED
/// sweep must never render as if it described the CURRENT pattern (the same generation-stamping
/// discipline `SearchSummary`'s own doc comment establishes); self-correcting the moment a fresh
/// snapshot for the right generation publishes. `raw` is the pattern's own typed text
/// (`SearchPattern::raw`); `pct` is the sweep's own scan progress, PRE-computed by the caller
/// from `scanned_up_to`/the file size via the u128 house rule — the identical division of labor
/// `status_text`'s own `pct` parameter already has (see `draw`), so the u128 arithmetic lives in
/// exactly one place (`draw`) rather than being duplicated into every formatter that needs a
/// percentage. The `≥` prefix marks the count as a FLOOR, not an exact total, once the sweep has
/// given up on any part of the file (`summary.failed`) — honest about a degraded, partial count
/// rather than presenting it as complete. The `searching {pct}%` tail is shown only while
/// `!summary.done`, mirroring `status_text`'s own `indexing…` treatment of the background index.
pub fn search_status_suffix(
    raw: &str,
    summary: &SearchSummary,
    active_generation: u64,
    pct: u64,
) -> Option<String> {
    if summary.generation != active_generation {
        return None;
    }
    let floor = if summary.failed { "≥" } else { "" };
    let mut suffix = format!(" · /{raw} · {floor}{} matches", summary.matches);
    if !summary.done {
        suffix.push_str(&format!(" · searching {pct}%"));
    }
    Some(suffix)
}
/// Draws the in-progress `:`/`/`/`?` command line over the bottom row of
/// `area`: the reserved chrome slot's top precedence, ahead of both the
/// progress row and the status line, while `Mode::Command(_)` is live.
/// Reversed, full-row clear first — same pattern as
/// `render_progress_row`/`render_status_line`. The buffer is shown as
/// `{prompt}{buffer}` — `prompt` is `':'`, `'/'`, or `'?'`, threaded by the
/// caller from the live `CommandKind` (`app.rs`'s `command_prompt`) — followed
/// by a trailing `_` cursor cell in the same reversed style as the rest of
/// the row, standing in for a real terminal cursor.
///
/// LEFT-truncated, not right, when `{prompt}{buffer}_` is wider than
/// `area.width`: the cursor cell is always the LAST cell shown — it marks
/// the one point that matters, where the next keystroke lands, so it is the
/// one thing that must never scroll off — with the buffer's own tail
/// filling backward from it, and the leading prompt sacrificed first if
/// there still isn't room for everything.
///
/// The windowing itself is WIDTH-aware, not character-count-aware: once the
/// buffer holds arbitrary search text rather than only digits, one char is
/// no longer one terminal cell (the digits-only assumption an earlier
/// version of this function made, back when `:` was the only grammar —
/// `unicode_width::UnicodeWidthChar` is the same width source `line.rs`
/// already uses for the viewport itself). Concretely: walk the full string's
/// chars from the front, dropping each one's own display width from a
/// running total until what remains fits `area.width`, then show everything
/// from there on — the display-width analogue of "show exactly the last
/// `min(area.width, len(...))` characters" the char-count version used.
/// Right-truncating instead — showing the buffer's HEAD, as an even earlier
/// version of this function did — let the visible text diverge from what
/// `Enter` actually executes: at `area.width == 3` with buffer `"123"`,
/// right-truncation showed `":12"` while `Enter` still jumped to line `123`,
/// not `12`. Left-truncation shows `"23_"` there instead — always in sync
/// with what committing right now would do, at the cost of the leading
/// prompt (dropped first) and then the buffer's own earlier characters
/// (dropped next, by WIDTH now, not by count) as the row narrows further.
pub fn render_command_line(prompt: char, buffer: &str, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let y = area.y + area.height - 1;
    let style = Style::default().add_modifier(Modifier::REVERSED);
    for x in area.x..area.x.saturating_add(area.width) {
        buf[(x, y)].set_symbol(" ").set_style(style);
    }
    let text = format!("{prompt}{buffer}_");
    let width = area.width as usize;
    // width-aware left truncation: drop leading chars until the DISPLAY
    // width fits -- one char is no longer one cell once the buffer holds
    // arbitrary text (the digits-only assumption this replaces).
    let mut chars: Vec<char> = text.chars().collect();
    let mut total: usize = chars
        .iter()
        .map(|&c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let mut skip = 0;
    while total > width && skip < chars.len() {
        total -= unicode_width::UnicodeWidthChar::width(chars[skip]).unwrap_or(0);
        skip += 1;
    }
    let visible: String = chars.drain(skip..).collect();
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
/// Draws a transient one-line notice ("search wrapped", "pattern not found", a bad-pattern
/// compile error) over the bottom row of `area` — the status-line idiom exactly: a notice IS a
/// status-line-shaped message, just a different source of text, so this is a thin wrapper rather
/// than a second copy of the clear-then-write body.
pub fn render_notice(text: &str, area: Rect, buf: &mut Buffer) {
    render_status_line(text, area, buf);
}
/// The full keymap, one hint per line — mirrors the README's key-bindings
/// table, plus `:N` and `F1` (the two bindings Task 5 also adds to the
/// README itself). The overlay does not read the README file at runtime,
/// so this list and that table are kept in sync by hand, not by
/// construction.
const HELP_LINES: [&str; 17] = [
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
    "/pat, ?pat — search fwd / back",
    "n / N — next / previous match",
    "smartcase; wraps with notice",
];
/// Draws the help overlay: a centered, bordered panel over `area`, sized
/// `min(area.width − 2, 60)` by `min(area.height − 2, 19)` so it never
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
/// margin/cap/border arithmetic here means updating that bound too. The 19
/// itself (grown from 16 when `HELP_LINES` grew from 14 to 17 rows for the
/// search keys) is chosen the same way the original 16 was: the border eats
/// 2 more rows off the panel to reach the INTERIOR (`19 - 2 = 17`), so the
/// cap alone — on any terminal tall enough not to bind the OTHER clamp,
/// `area.height - 2` — is exactly enough interior for every `HELP_LINES`
/// row, no more, no fewer; the smaller minimum bound `help_overlay_fits`
/// checks (`content_rows(rows) >= 5`, i.e. an interior of at least 1 row) is
/// unaffected either way, since it is the OTHER branch of the `min` (small
/// terminals, where `area.height - 2` binds instead of the cap) that
/// determines it — see that function's own doc comment for the full
/// derivation, confirmed against `Block::inner()`'s real behavior by the
/// worked panel/interior numbers in this function's own tests, not merely
/// asserted.
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
    let height = area.height.saturating_sub(2).min(19);
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
    use ress_core::document::RowMarks;
    #[test]
    fn writes_rows_clamped_to_width() {
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["hi".to_string(), "world!!".to_string()],
            marks: vec![RowMarks::default(); 2],
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
            marks: vec![RowMarks::default()],
        };
        render_viewport(&view, area, &mut buf);
        let expected = Buffer::with_lines(["ab ", "   ", "   "]);
        assert_eq!(buf, expected);
    }
    #[test]
    fn no_search_renders_identically() {
        // ViewportRender::marks is all-default (empty spans, no current) when
        // no search is active -- the restyle loop must then be a complete
        // no-op, pixel- AND style-for-style the same as before this task.
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["hi".to_string(), "world!!".to_string()],
            marks: vec![RowMarks::default(); 2],
        };
        render_viewport(&view, area, &mut buf);
        let expected = Buffer::with_lines(["hi   ", "world"]);
        assert_eq!(buf, expected);
        for y in 0..2 {
            for x in 0..5 {
                assert_eq!(
                    buf[(x, y)].style().add_modifier,
                    Modifier::empty(),
                    "no search must leave every cell's style untouched at ({x},{y})"
                );
            }
        }
    }
    #[test]
    fn render_viewport_restyles_match_spans_reversed() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["abcdefghij".to_string()],
            marks: vec![RowMarks {
                spans: vec![2..4, 7..9],
                current: None,
            }],
        };
        render_viewport(&view, area, &mut buf);
        for x in 0..10u16 {
            let modifier = buf[(x, 0)].style().add_modifier;
            let expected_reversed = (2..4).contains(&x) || (7..9).contains(&x);
            assert_eq!(
                modifier.contains(Modifier::REVERSED),
                expected_reversed,
                "cell {x}"
            );
            assert!(
                !modifier.contains(Modifier::BOLD),
                "cell {x} must not be bold with no current match"
            );
        }
    }
    #[test]
    fn render_viewport_marks_the_current_match_bold_on_top_of_reversed() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let view = ViewportRender {
            rows: vec!["abcdefghij".to_string()],
            marks: vec![RowMarks {
                spans: vec![2..4, 7..9],
                current: Some(7..9),
            }],
        };
        render_viewport(&view, area, &mut buf);
        for x in 0..10u16 {
            let modifier = buf[(x, 0)].style().add_modifier;
            let in_current = (7..9).contains(&x);
            let expected_reversed = (2..4).contains(&x) || in_current;
            assert_eq!(
                modifier.contains(Modifier::REVERSED),
                expected_reversed,
                "reversed at cell {x}"
            );
            assert_eq!(
                modifier.contains(Modifier::BOLD),
                in_current,
                "bold only over the current match, cell {x}"
            );
        }
    }
    #[test]
    fn render_viewport_clamps_spans_to_the_area_width() {
        // a span computed against a wider `cols` than this frame's own live
        // area (the resize race `restyle_span`'s own doc comment describes)
        // must never paint past `area`'s own right edge -- a wider buffer
        // than `area` catches any such overflow directly, the same shape
        // `scrollbar_draws_a_dim_track_and_one_thumb_in_the_rightmost_column`
        // uses for its own out-of-area bleed check.
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        // a single-element `Vec<Range<_>>` reads as a mistake to clippy
        // (`single_range_in_vec_init`) if spelled as a literal -- built via
        // `from_iter` instead, since this test genuinely wants exactly one.
        let spans = Vec::from_iter(std::iter::once(3u16..20));
        let view = ViewportRender {
            rows: vec!["abcde".to_string()],
            marks: vec![RowMarks {
                spans,
                current: None,
            }],
        };
        render_viewport(&view, area, &mut buf);
        for x in 5..8u16 {
            assert!(
                !buf[(x, 0)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED),
                "cell {x} is outside area and must be untouched"
            );
        }
        for x in 3..5u16 {
            assert!(
                buf[(x, 0)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
        }
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
    fn summary(
        generation: u64,
        matches: u64,
        scanned_up_to: u64,
        done: bool,
        failed: bool,
    ) -> SearchSummary {
        SearchSummary {
            generation,
            matches,
            buckets: std::sync::Arc::new([0u16; 1024]),
            scanned_up_to,
            done,
            failed,
        }
    }
    #[test]
    fn search_status_suffix_is_none_on_generation_mismatch() {
        // a snapshot left over from a SUPERSEDED sweep must never render as if it described the
        // CURRENT pattern -- self-correcting the moment a matching-generation snapshot publishes.
        let s = summary(1, 5, 100, true, false);
        assert_eq!(search_status_suffix("foo", &s, 2, 0), None);
    }
    #[test]
    fn search_status_suffix_shows_matches_when_done() {
        let s = summary(1, 5, 100, true, false);
        assert_eq!(
            search_status_suffix("foo", &s, 1, 0),
            Some(" · /foo · 5 matches".to_string())
        );
    }
    #[test]
    fn search_status_suffix_shows_searching_pct_while_not_done() {
        let s = summary(1, 2, 50, false, false);
        assert_eq!(
            search_status_suffix("foo", &s, 1, 42),
            Some(" · /foo · 2 matches · searching 42%".to_string())
        );
    }
    #[test]
    fn search_status_suffix_marks_a_floor_when_failed() {
        // counts are a floor, not a total, once the sweep has given up on any part of the file.
        let s = summary(1, 3, 100, true, true);
        assert_eq!(
            search_status_suffix("foo", &s, 1, 0),
            Some(" · /foo · ≥3 matches".to_string())
        );
    }
    #[test]
    fn search_status_suffix_combines_the_floor_marker_and_the_searching_tail() {
        let s = summary(1, 7, 40, false, true);
        assert_eq!(
            search_status_suffix("f.*o", &s, 1, 20),
            Some(" · /f.*o · ≥7 matches · searching 20%".to_string())
        );
    }
    #[test]
    fn status_line_renders_reversed_and_clears_the_row() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
        render_viewport(
            &ViewportRender {
                rows: vec!["xxxxxxxxxxxxxxxxxxxx".into(); 3],
                marks: vec![RowMarks::default(); 3],
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
    fn notice_renders_reversed_and_clears_the_row() {
        // the status-line idiom, exactly -- render_notice is a thin wrapper over
        // render_status_line, so this pins that it actually behaves like one, not just that it
        // compiles as one.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
        render_viewport(
            &ViewportRender {
                rows: vec!["xxxxxxxxxxxxxxxxxxxx".into(); 3],
                marks: vec![RowMarks::default(); 3],
            },
            Rect::new(0, 0, 20, 3),
            &mut buf,
        );
        render_notice("search wrapped", Rect::new(0, 0, 20, 3), &mut buf);
        let bottom: String = (0..20).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert!(bottom.starts_with("search wrapped"));
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
        render_command_line(':', "150", Rect::new(0, 0, 20, 2), &mut buf);
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
        render_command_line(':', "42", Rect::new(0, 0, 20, 2), &mut buf);
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
        render_command_line(':', buffer, area3, &mut buf3);
        let row3: String = (0..3).map(|x| buf3[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row3, "23_");
        // width 4: last 4 of ":123_" = "123_" (just the colon dropped).
        let area4 = Rect::new(0, 0, 4, 1);
        let mut buf4 = Buffer::empty(area4);
        render_command_line(':', buffer, area4, &mut buf4);
        let row4: String = (0..4).map(|x| buf4[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row4, "123_");
        // width 5: ":123_" is exactly 5 chars — everything fits, nothing
        // is sacrificed.
        let area5 = Rect::new(0, 0, 5, 1);
        let mut buf5 = Buffer::empty(area5);
        render_command_line(':', buffer, area5, &mut buf5);
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
        render_command_line(':', "1234567890", area, &mut buf);
        let row: String = (0..5).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "7890_");
    }
    #[test]
    fn search_prompt_renders() {
        // `/` and `?` thread through as the prompt char exactly like `:` does — same clear/
        // reverse/cursor shape, just a different leading character.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 2));
        render_command_line('/', "foo", Rect::new(0, 0, 20, 2), &mut buf);
        let bottom: String = (0..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert!(bottom.starts_with("/foo_"), "unexpected row: {bottom:?}");
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 20, 2));
        render_command_line('?', "bar", Rect::new(0, 0, 20, 2), &mut buf2);
        let bottom2: String = (0..20).map(|x| buf2[(x, 1)].symbol().to_string()).collect();
        assert!(bottom2.starts_with("?bar_"), "unexpected row: {bottom2:?}");
    }
    #[test]
    fn wide_chars_never_push_the_cursor_off() {
        // 'あ' is 2 cells wide (East Asian Wide) -- a char-COUNT-based window (the digits-only
        // assumption this function used to make) would under-truncate here and let
        // `set_stringn`'s own right-side width clamp cut the cursor off instead: buffer "ああ"
        // (2 chars, 4 display cells) + prompt (1 cell) + cursor (1 cell) = 6 display cells,
        // into an area only 4 cells wide -- the OLD char-count skip (chars=4, width=4, skip=0)
        // would try to show all of ":ああ_" and lose the cursor to `set_stringn`'s clamp.
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        render_command_line(':', "ああ", area, &mut buf);
        let row_symbols: Vec<String> = (0..4).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            row_symbols.contains(&"_".to_string()),
            "the cursor must survive width-aware truncation: {row_symbols:?}"
        );
    }
    #[test]
    fn hint_bar_shows_normal_mode_hints_including_f1_when_help_fits() {
        // width 90, not 80: the `/find` segment pushed the with-F1 variant to 82 characters --
        // wider than the row would have clipped it to at the old 80, which would have silently
        // truncated the tail this test's own `starts_with` needs intact to verify end-to-end.
        let area = Rect::new(0, 0, 90, 5);
        let mut buf = Buffer::empty(area);
        render_hint_bar(Mode::Normal, true, area, &mut buf);
        let text: String = (0..90).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(text.starts_with(
            "j/k scroll · d/u half · gg/G top/end · :N line · /find · N% pct · F1 help · q quit"
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
        let bottom: String = (0..90).map(|x| buf[(x, 4)].symbol().to_string()).collect();
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
            text.starts_with(
                "j/k scroll · d/u half · gg/G top/end · :N line · /find · N% pct · q quit"
            ),
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
        render_hint_bar(Mode::Command(CommandKind::Colon), true, area, &mut buf);
        let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(text.starts_with("Enter go · Esc cancel · Backspace edit"));
    }
    #[test]
    fn hint_bar_shows_search_command_mode_hints() {
        let area = Rect::new(0, 0, 80, 5);
        // both directions share the same hint text -- the `Search { .. }` arm doesn't
        // discriminate on `backward`.
        for backward in [false, true] {
            let mut buf = Buffer::empty(area);
            render_hint_bar(
                Mode::Command(CommandKind::Search { backward }),
                true,
                area,
                &mut buf,
            );
            let text: String = (0..80).map(|x| buf[(x, 3)].symbol().to_string()).collect();
            assert!(
                text.starts_with("Enter search · Esc cancel · Backspace edit"),
                "backward={backward}: unexpected hint text: {text:?}"
            );
        }
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
            render_hint_bar(Mode::Command(CommandKind::Colon), help_fits, area, &mut buf);
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
        // area 80x24 -> panel min(78,60)x min(22,19) = 60x19, centered at
        // x=(80-60)/2=10, y=(24-19)/2=2; inner (after 1-cell borders on all
        // sides) is x=11,y=3,58x17.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_help_overlay(area, &mut buf);
        assert_ne!(
            buf[(10, 2)].symbol(),
            " ",
            "top-left corner should carry a border glyph"
        );
        let top_row: String = (10..70).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert!(
            top_row.contains("help"),
            "title missing from the top border: {top_row:?}"
        );
        let mut found_quit_hint = false;
        for y in 3..20 {
            let row: String = (11..69).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if row.contains("q / Ctrl-c") {
                found_quit_hint = true;
            }
        }
        assert!(found_quit_hint, "expected the quit hint inside the panel");
    }
    #[test]
    fn help_overlay_shows_the_new_search_keymap_lines() {
        // direct verification that the 19 cap (not the brief's initially-suggested 18, which
        // arithmetic shows would still leave the INTERIOR one row short -- see
        // render_help_overlay's own doc comment for the derivation) actually fits all three new
        // rows, not just the pre-existing 14.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_help_overlay(area, &mut buf);
        let mut found = [false; 3];
        for y in 3..20 {
            let row: String = (11..69).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if row.contains("/pat, ?pat") {
                found[0] = true;
            }
            if row.contains("n / N") {
                found[1] = true;
            }
            if row.contains("smartcase") {
                found[2] = true;
            }
        }
        assert!(
            found.iter().all(|&f| f),
            "expected all three search keymap lines inside the panel: {found:?}"
        );
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
        let top_row: String = (10..70).map(|x| buf[(x, 2)].symbol().to_string()).collect();
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
        render_scrollbar(50, 100, content_area, &mut buf, None);
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
        render_scrollbar(0, 0, area, &mut buf, None); // size 0 -> None
        assert_eq!(buf, Buffer::empty(area));
    }
    #[test]
    fn tick_rows_maps_buckets_onto_rows() {
        // rows < 1024: size=1024 makes each of the 1024 buckets exactly 1 byte wide (bucket b
        // spans [b, b+1)), so a match's own byte offset IS its bucket index -- rows=4 then splits
        // that into four 256-byte rows: [0,256), [256,512), [512,768), [768,1024).
        let mut small = [0u16; 1024];
        small[100] = 1; // byte 100 -> row 0
        small[500] = 2; // byte 500 -> row 1
        assert_eq!(
            tick_rows(&small, 1024, 4),
            vec![true, true, false, false],
            "rows 2 and 3 have no nonzero bucket anywhere in their span"
        );
        // rows > 1024: size=2_048_000 makes each of the 1024 buckets exactly 2000 bytes wide
        // (bucket b spans [2000b, 2000b+2000)); rows=2048 (> 1024) then makes each row exactly
        // 1000 bytes wide -- half a bucket -- so ONE nonzero bucket spans exactly TWO rows, the
        // case a naive 1:1 bucket-to-row assumption would get wrong.
        let mut large = [0u16; 1024];
        large[5] = 3; // bucket 5 spans [10_000, 12_000) -> rows 10 ([10_000,11_000)) and 11
        let ticks = tick_rows(&large, 2_048_000, 2048);
        assert_eq!(ticks.len(), 2048);
        for (r, &marked) in ticks.iter().enumerate() {
            assert_eq!(marked, r == 10 || r == 11, "row {r}");
        }
    }
    #[test]
    fn tick_rows_empty_buckets_mark_nothing() {
        let buckets = [0u16; 1024];
        assert_eq!(tick_rows(&buckets, 1000, 5), vec![false; 5]);
    }
    #[test]
    fn tick_rows_empty_file_or_zero_rows_returns_empty_vec() {
        // no bytes to spread a histogram over, or no on-screen cells to mark, either way there is
        // nothing to draw -- the same shape scrollbar_thumb's own None already gives
        // render_scrollbar for the thumb, not a divide-by-zero panic. Every bucket is nonzero
        // here so an empty Vec can only come from the guard, not from there being nothing to mark.
        let buckets = [1u16; 1024];
        assert_eq!(tick_rows(&buckets, 0, 10), Vec::<bool>::new());
        assert_eq!(tick_rows(&buckets, 1000, 0), Vec::<bool>::new());
    }
    #[test]
    fn tick_rows_handles_sub_byte_wide_buckets_when_size_is_less_than_1024() {
        // the CRITICAL fix: flooring bucket_start/bucket_end to integer byte offsets (the
        // previous implementation) degenerates every bucket to an empty [k,k) span whenever its
        // TRUE (rational) width is under 1 byte -- i.e. EVERY bucket once size < 1024 -- so no
        // tick could ever show for a small file. The fix compares the exact rational bucket/row
        // spans via cross-multiplication, never flooring either boundary to an integer first.
        // size=100 (< 1024): bucket 0's true span is [0, 100/1024) ~= [0, 0.098) -- entirely
        // inside row 0's own [0, 33.33...) -- so only row 0 should tick.
        let mut b100 = [0u16; 1024];
        b100[0] = 1;
        assert_eq!(tick_rows(&b100, 100, 3), vec![true, false, false]);
        // size=1 (a one-byte file): EVERY bucket's true span ([b/1024, (b+1)/1024) for any b in
        // 0..1024) lies inside [0,1), row 0's own whole span -- so a 1-byte file must show its
        // tick regardless of which bucket the single possible match position happened to floor
        // into (bucket 777 here, deliberately not bucket 0, to prove it isn't special-cased).
        let mut b1 = [0u16; 1024];
        b1[777] = 1;
        assert_eq!(tick_rows(&b1, 1, 1), vec![true]);
        // size=5, rows=20: four rows per byte (20/5), so byte k occupies rows [4k, 4k+4). Bucket
        // 0's span [0, 5/1024) sits inside byte 0's rows, specifically row 0. Bucket 1023's span
        // [1023*5/1024, 5) sits inside byte 4's rows, specifically row 19. Bucket 204's span
        // straddles the byte-0/byte-1 boundary at real position 1 ([1020/1024, 1025/1024) =
        // [0.996.., 1.001..)), so it overlaps BOTH row 3 (byte 0's last row, [0.75,1.0)) and row
        // 4 (byte 1's first row, [1.0,1.25)) -- the fractional-boundary-crossing case the old
        // flooring-based code could never get right for size < 1024.
        let mut b5 = [0u16; 1024];
        b5[0] = 1;
        b5[204] = 1;
        b5[1023] = 1;
        let ticks = tick_rows(&b5, 5, 20);
        for (r, &marked) in ticks.iter().enumerate() {
            assert_eq!(marked, matches!(r, 0 | 3 | 4 | 19), "row {r}");
        }
    }
    #[test]
    fn tick_rows_touching_boundary_does_not_over_mark_the_neighboring_row() {
        // a bucket whose true span ends EXACTLY at a row boundary must mark only the row it's
        // actually inside, never the neighbor it merely touches -- pinning that the cross-
        // multiplication fix (above) didn't trade the size<1024 under-marking bug for a new
        // over-marking one at exact boundaries. size=1024 makes bucket 255 span exactly
        // [255,256), the shared edge between row 0's [0,256) and row 1's [256,512) at rows=4.
        let mut buckets = [0u16; 1024];
        buckets[255] = 1;
        assert_eq!(
            tick_rows(&buckets, 1024, 4),
            vec![true, false, false, false]
        );
    }
    #[test]
    fn scrollbar_renders_ticks_dim_and_thumb_wins_overlap() {
        // same anchor/size/area as
        // scrollbar_draws_a_dim_track_and_one_thumb_in_the_rightmost_column above -- the thumb
        // lands on row 2 of 4 -- with ticks additionally marking rows 2 (the thumb's own row) and
        // 3, to pin that the thumb wins the row they share rather than showing `·` there.
        let content_area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(content_area);
        let ticks = [false, false, true, true];
        render_scrollbar(50, 100, content_area, &mut buf, Some(&ticks));
        let expect = [("│", true), ("│", true), ("█", false), ("·", true)];
        for (y, (symbol, dim)) in expect.iter().enumerate() {
            let cell = &buf[(9, y as u16)];
            assert_eq!(cell.symbol(), *symbol, "row {y}");
            assert_eq!(
                cell.style().add_modifier.contains(Modifier::DIM),
                *dim,
                "row {y} dim"
            );
        }
    }
    #[test]
    fn scrollbar_without_ticks_matches_the_old_rendering() {
        // regression: ticks: None must reproduce exactly what render_scrollbar drew before ticks
        // existed -- every non-thumb row a plain track `│` DIM, never `·`, regardless of where
        // the thumb itself lands.
        let area = Rect::new(0, 0, 8, 5);
        let mut buf = Buffer::empty(area);
        render_scrollbar(30, 90, area, &mut buf, None);
        for y in 0..5u16 {
            let cell = &buf[(7, y)];
            if y == 1 {
                assert_eq!(cell.symbol(), "█", "thumb row");
                assert!(!cell.style().add_modifier.contains(Modifier::DIM));
            } else {
                assert_eq!(cell.symbol(), "│", "track row {y}");
                assert!(cell.style().add_modifier.contains(Modifier::DIM));
            }
        }
    }
}
