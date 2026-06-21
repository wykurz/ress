//! blits a `ViewportRender` into a ratatui buffer, one display row per line,
//! clamped to the render area's width.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ress_core::document::ViewportRender;
/// draws each display row at successive lines of `area`, clamped to its width;
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
}
