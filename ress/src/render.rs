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
}
