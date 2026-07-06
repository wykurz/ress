//! The document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
use crate::source::BlockSource;
use std::sync::Arc;
/// The scroll position: the byte offset of the first visible line. Navigation is
/// expressed in anchors, never line numbers, so it never waits on indexing.
/// Anchors come only from `Anchor::TOP` or `Document` navigation, which keeps
/// the offset a line start: 0, or the byte just past a newline before EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor(u64);
impl Anchor {
    /// The top of the file.
    pub const TOP: Anchor = Anchor(0);
    /// The byte offset this anchor points at.
    pub fn offset(self) -> u64 {
        self.0
    }
}
/// Upper bound on the horizontal scroll offset in display columns. `HScroll`
/// clamps to it on construction, so larger offsets are unrepresentable and the
/// viewport's scan budget stays bounded on very long lines.
pub const MAX_HSCROLL: usize = 1 << 16;
/// A horizontal scroll offset in display columns. Values are capped at
/// `MAX_HSCROLL` on construction, so no arithmetic downstream of the type can
/// ever see an unbounded offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HScroll(usize);
impl HScroll {
    /// No horizontal scroll.
    pub const ZERO: HScroll = HScroll(0);
    /// Creates an offset, clamped to `MAX_HSCROLL`.
    pub fn new(columns: usize) -> HScroll {
        HScroll(columns.min(MAX_HSCROLL))
    }
    /// The offset in display columns.
    pub fn columns(self) -> usize {
        self.0
    }
    /// Shifts by `n` columns, saturating at zero and capping at `MAX_HSCROLL`,
    /// so a huge count prefix can neither overflow nor unbound the offset.
    pub fn shift(self, n: i64) -> HScroll {
        let shifted = if n >= 0 {
            self.0.saturating_add(n as usize)
        } else {
            self.0.saturating_sub(n.unsigned_abs() as usize)
        };
        HScroll::new(shifted)
    }
}
/// The lines to draw for one screen, already chopped to the terminal width.
#[derive(Debug, PartialEq, Eq)]
pub struct ViewportRender {
    pub rows: Vec<String>,
}
/// A read-only view over a file's bytes.
pub struct Document {
    source: Arc<dyn BlockSource>,
    size: u64,
    config: Config,
}
impl Document {
    pub fn new(source: Arc<dyn BlockSource>, config: Config) -> Self {
        let size = source.size();
        Self {
            source,
            size,
            config,
        }
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    /// Builds one screen of `rows` display rows starting at line-start byte
    /// offset `top`, each laid out to the display-column window
    /// `[hscroll, hscroll + cols)` (wide chars occupy two columns). Reads forward
    /// only until the screen is filled, EOF, or a bounded scan budget is hit — so a file
    /// with sparse newlines (or one giant unterminated line) can never pull the
    /// whole file into memory before the first paint.
    pub async fn viewport(
        &self,
        top: Anchor,
        rows: usize,
        cols: usize,
        hscroll: HScroll,
    ) -> anyhow::Result<ViewportRender> {
        // include the horizontal offset so scrolling right into a long line reads
        // far enough to find the window's bytes instead of rendering a blank row;
        // `HScroll` is capped on construction, so the budget stays bounded.
        let span = hscroll.columns().saturating_add(cols.max(1));
        let scan_budget = self
            .config
            .block_size
            .max(rows.saturating_mul(span).saturating_mul(4));
        let mut buf: Vec<u8> = Vec::new();
        let mut pos = top.offset();
        let mut newlines = 0usize;
        while newlines < rows && pos < self.size && buf.len() < scan_budget {
            let block = self.source.read_block(pos, self.config.block_size).await?;
            if block.is_empty() {
                break;
            }
            pos += block.len() as u64;
            newlines += bytecount_newlines(&block);
            buf.extend_from_slice(&block);
        }
        // nothing more to show once we hit EOF or exhaust the scan budget; a line
        // longer than the budget is rendered chopped and truncates the screen
        // (full long-line handling lands with wrap/navigation in a later plan).
        let stop = pos >= self.size || buf.len() >= scan_budget;
        let mut out = Vec::with_capacity(rows);
        let mut line_start = 0usize;
        for i in 0..buf.len() {
            if buf[i] == b'\n' {
                out.push(crate::line::layout_row(
                    &buf[line_start..i],
                    self.config.tab_stop,
                    hscroll.columns(),
                    cols,
                ));
                line_start = i + 1;
                if out.len() == rows {
                    break;
                }
            }
        }
        if out.len() < rows && line_start < buf.len() && stop {
            out.push(crate::line::layout_row(
                &buf[line_start..],
                self.config.tab_stop,
                hscroll.columns(),
                cols,
            ));
        }
        Ok(ViewportRender { rows: out })
    }
    /// Moves the top anchor by `delta` lines: positive scrolls down, negative up.
    /// Clamps at offset 0 and at the last content line's start.
    pub async fn scroll_lines(&self, from: Anchor, delta: i64) -> anyhow::Result<Anchor> {
        match delta.cmp(&0) {
            std::cmp::Ordering::Equal => Ok(from),
            std::cmp::Ordering::Greater => self.scroll_down(from.offset(), delta as usize).await,
            std::cmp::Ordering::Less => {
                self.scroll_up(from.offset(), delta.unsigned_abs() as usize)
                    .await
            }
        }
    }
    /// Finds the start of the `n`-th line after `start` (a line start), stopping at
    /// the last content line rather than moving past EOF.
    async fn scroll_down(&self, start: u64, n: usize) -> anyhow::Result<Anchor> {
        let mut pos = start;
        let mut last_line_start = start;
        let mut found = 0usize;
        while found < n && pos < self.size {
            let block = self.source.read_block(pos, self.config.block_size).await?;
            if block.is_empty() {
                break;
            }
            for (i, &b) in block.iter().enumerate() {
                if b == b'\n' {
                    let ls = pos + i as u64 + 1;
                    if ls >= self.size {
                        return Ok(Anchor(last_line_start));
                    }
                    last_line_start = ls;
                    found += 1;
                    if found == n {
                        return Ok(Anchor(ls));
                    }
                }
            }
            pos += block.len() as u64;
        }
        Ok(Anchor(last_line_start))
    }
    /// Finds the start of the `n`-th line before `start` (a line start), clamped
    /// at 0. Scans backward once, collecting newline offsets, so moving up a
    /// screenful of lines costs a single block read rather than one read per line
    /// — re-reading the same block per line would defeat the high-latency-FS goal.
    async fn scroll_up(&self, start: u64, n: usize) -> anyhow::Result<Anchor> {
        if start == 0 || n == 0 {
            return Ok(Anchor(start));
        }
        let bs = self.config.block_size as u64;
        // the '\n' at start-1 terminates the line above `start`; count line starts
        // by the newlines strictly before it, taking the n-th one from the right.
        let mut hi = start - 1;
        let mut found = 0usize;
        loop {
            if hi == 0 {
                return Ok(Anchor::TOP);
            }
            let lo = hi.saturating_sub(bs);
            let len = (hi - lo) as usize;
            let block = self.source.read_block(lo, len).await?;
            let block = &block[..len.min(block.len())];
            for (idx, &b) in block.iter().enumerate().rev() {
                if b == b'\n' {
                    found += 1;
                    if found == n {
                        return Ok(Anchor(lo + idx as u64 + 1));
                    }
                }
            }
            if lo == 0 {
                return Ok(Anchor::TOP);
            }
            hi = lo;
        }
    }
    /// Given an offset `pos` (> 0), returns the start of the line ending just
    /// before `pos` — the byte after the last '\n' strictly before `pos`, or 0
    /// if there is none. `pos` need not itself be a line start.
    async fn prev_line_start(&self, pos: u64) -> anyhow::Result<u64> {
        let search_end = pos - 1;
        if search_end == 0 {
            return Ok(0);
        }
        let bs = self.config.block_size as u64;
        let mut hi = search_end;
        loop {
            let lo = hi.saturating_sub(bs);
            let len = (hi - lo) as usize;
            let block = self.source.read_block(lo, len).await?;
            let block = &block[..len.min(block.len())];
            if let Some(rel) = block.iter().rposition(|&b| b == b'\n') {
                return Ok(lo + rel as u64 + 1);
            }
            if lo == 0 {
                return Ok(0);
            }
            hi = lo;
        }
    }
    /// The top of the file — always answerable.
    pub fn goto_top(&self) -> Anchor {
        Anchor::TOP
    }
    /// The anchor that puts the file's final content line on the bottom row.
    pub async fn goto_end(&self, rows: usize) -> anyhow::Result<Anchor> {
        if self.size == 0 {
            return Ok(Anchor::TOP);
        }
        let last = self.last_line_start().await?;
        if rows <= 1 {
            return Ok(Anchor(last));
        }
        self.scroll_up(last, rows - 1).await
    }
    /// Jumps `pct`% through the file by byte, snapped forward to the next line
    /// start so the top row is never a partial line.
    pub async fn goto_percent(&self, pct: u8) -> anyhow::Result<Anchor> {
        let pct = pct.min(100) as u64;
        if self.size == 0 || pct == 0 {
            return Ok(Anchor::TOP);
        }
        if pct >= 100 {
            return Ok(Anchor(self.last_line_start().await?));
        }
        let target = percent_offset(self.size, pct);
        let start = self.line_start_at_or_after(target).await?;
        Ok(Anchor(start))
    }
    /// The start offset of the file's final content line (ignoring one trailing '\n').
    async fn last_line_start(&self) -> anyhow::Result<u64> {
        if self.size == 0 {
            return Ok(0);
        }
        self.prev_line_start(self.size).await
    }
    /// The next line start at or after `offset` (offset 0, or just past a '\n').
    async fn line_start_at_or_after(&self, offset: u64) -> anyhow::Result<u64> {
        if offset == 0 {
            return Ok(0);
        }
        let prev = self.source.read_block(offset - 1, 1).await?;
        if prev.first() == Some(&b'\n') {
            return Ok(offset);
        }
        let mut pos = offset;
        while pos < self.size {
            let block = self.source.read_block(pos, self.config.block_size).await?;
            if block.is_empty() {
                break;
            }
            if let Some(rel) = block.iter().position(|&b| b == b'\n') {
                let start = pos + rel as u64 + 1;
                // a '\n' as the file's final byte snaps to an EOF offset (a phantom
                // empty line that renders blank); clamp to the last content line.
                if start >= self.size {
                    return self.last_line_start().await;
                }
                return Ok(start);
            }
            pos += block.len() as u64;
        }
        self.last_line_start().await
    }
}
/// Byte offset `pct`% into `size` bytes, computed in u128 so huge (sparse)
/// file sizes near `u64::MAX` can't overflow the multiplication.
fn percent_offset(size: u64, pct: u64) -> u64 {
    (size as u128 * pct as u128 / 100) as u64
}
fn bytecount_newlines(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    fn doc(data: &'static [u8], block_size: usize) -> Document {
        let src = Arc::new(MockSource::new(bytes::Bytes::from_static(data)));
        Document::new(
            src,
            Config {
                block_size,
                ..Config::default()
            },
        )
    }
    #[tokio::test]
    async fn returns_first_screen() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d.viewport(Anchor::TOP, 2, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn returns_all_lines_when_file_is_short() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 10, 80, HScroll::ZERO)
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
    #[tokio::test]
    async fn includes_final_line_without_trailing_newline() {
        let d = doc(b"abc", 1 << 20);
        let v = d.viewport(Anchor::TOP, 5, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["abc".to_string()]);
    }
    #[tokio::test]
    async fn chops_long_lines_to_width() {
        let d = doc(b"xxxxxxxxxx\n", 1 << 20);
        let v = d.viewport(Anchor::TOP, 1, 4, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["xxxx".to_string()]);
    }
    #[tokio::test]
    async fn handles_lines_spanning_block_boundaries() {
        let d = doc(b"aaaaaa\nbb\n", 4);
        let v = d.viewport(Anchor::TOP, 2, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["aaaaaa".to_string(), "bb".to_string()]);
    }
    #[tokio::test]
    async fn strips_trailing_carriage_return() {
        let d = doc(b"a\r\nb\r\n", 1 << 20);
        let v = d.viewport(Anchor::TOP, 2, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn empty_file_has_no_rows() {
        let d = doc(b"", 1 << 20);
        let v = d.viewport(Anchor::TOP, 5, 80, HScroll::ZERO).await.unwrap();
        assert!(v.rows.is_empty());
    }
    #[tokio::test]
    async fn starts_from_nonzero_line_offset() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d.viewport(Anchor(2), 2, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["b".to_string(), "c".to_string()]);
    }
    #[tokio::test]
    async fn long_unterminated_line_does_not_read_whole_file() {
        // a single 4096-byte line with no newline, read through tiny blocks: the
        // viewport must stop at its scan budget rather than pull the whole "file"
        // into memory before the first paint.
        let src = Arc::new(MockSource::new(bytes::Bytes::from(vec![b'x'; 4096])));
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 16,
                ..Config::default()
            },
        );
        let v = d.viewport(Anchor::TOP, 2, 4, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["xxxx".to_string()]);
        assert!(
            src.read_count() < 16,
            "read {} blocks of a 4096-byte line; expected a bounded few",
            src.read_count()
        );
    }
    #[tokio::test]
    async fn expands_tabs_in_viewport() {
        let d = doc(b"a\tb\n", 1 << 20);
        let v = d.viewport(Anchor::TOP, 1, 80, HScroll::ZERO).await.unwrap();
        assert_eq!(v.rows, vec!["a       b".to_string()]);
    }
    #[tokio::test]
    async fn viewport_applies_horizontal_scroll() {
        let d = doc(b"0123456789\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 1, 4, HScroll::new(3))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["3456".to_string()]);
    }
    #[tokio::test]
    async fn viewport_reads_far_enough_for_horizontal_scroll() {
        // one long line (300 digits, block_size 64); scrolling right to column 200
        // must still read the bytes for that window rather than blanking the row.
        let mut data: Vec<u8> = (0..300u32).map(|i| b'0' + (i % 10) as u8).collect();
        data.push(b'\n');
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                ..Config::default()
            },
        );
        let v = d
            .viewport(Anchor::TOP, 1, 10, HScroll::new(200))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["0123456789".to_string()]);
    }
    #[tokio::test]
    async fn huge_hscroll_keeps_the_scan_budget_bounded() {
        // a saturated horizontal offset clamps to MAX_HSCROLL at construction, so
        // the window sits at the cap (and shows content); the guarantee under
        // test is that the read stays budget-bounded instead of scanning to EOF.
        let src = Arc::new(MockSource::new(bytes::Bytes::from(vec![b'x'; 1 << 20])));
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 4096,
                ..Config::default()
            },
        );
        let v = d
            .viewport(Anchor::TOP, 1, 1, HScroll::new(usize::MAX))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["x".to_string()]);
        assert!(
            src.read_count() < 100,
            "read {} blocks of a 256-block file",
            src.read_count()
        );
    }
    #[tokio::test]
    async fn scrolls_down_one_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 1).await.unwrap();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_down_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor::TOP, 2).await.unwrap();
        assert_eq!(a, Anchor(10));
    }
    #[tokio::test]
    async fn scroll_down_clamps_to_last_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scroll_down_clamps_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scrolls_up_one_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(4), -1).await.unwrap();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_up_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor(10), -2).await.unwrap();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn scroll_up_clamps_at_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), -99).await.unwrap();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn scroll_up_scans_backward_once_per_block() {
        // 100 short lines inside one default (1 MiB) block; scrolling up 40 lines
        // must not re-read the block once per line (the high-latency-FS anti-pattern).
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"x\n");
        }
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(src.clone(), Config::default());
        let top = d.scroll_lines(Anchor(198), -40).await.unwrap();
        assert_eq!(top, Anchor(118));
        assert!(
            src.read_count() <= 2,
            "scroll_up re-read blocks {} times for 40 lines",
            src.read_count()
        );
    }
    #[tokio::test]
    async fn zero_newline_file_navigates_to_top_only() {
        // a file with no newlines is one line; every motion lands on offset 0.
        let d = doc(b"abcdef", 2);
        assert_eq!(d.scroll_lines(Anchor::TOP, 5).await.unwrap(), Anchor::TOP);
        assert_eq!(d.scroll_lines(Anchor::TOP, -5).await.unwrap(), Anchor::TOP);
        assert_eq!(d.goto_end(3).await.unwrap(), Anchor::TOP);
        assert_eq!(d.goto_percent(50).await.unwrap(), Anchor::TOP);
    }
    #[tokio::test]
    async fn scroll_zero_is_identity() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), 0).await.unwrap();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn goto_top_is_offset_zero() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        assert_eq!(d.goto_top(), Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_end_shows_last_screenful() {
        let d = doc(b"a\nb\nc\nd\n", 1 << 20);
        // 2 rows: last line 'd' at the bottom, so 'c' is the top.
        let a = d.goto_end(2).await.unwrap();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.goto_end(1).await.unwrap();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_clamps_when_file_shorter_than_screen() {
        let d = doc(b"a\nb\n", 1 << 20);
        let a = d.goto_end(10).await.unwrap();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_end_of_empty_file_is_top() {
        let d = doc(b"", 1 << 20);
        let a = d.goto_end(10).await.unwrap();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_snaps_to_line_start() {
        // size 8; 50% -> offset 4, already a line start ('c').
        let d = doc(b"a\nb\nc\nd\n", 1 << 20);
        let a = d.goto_percent(50).await.unwrap();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_percent_rounds_up_to_next_line() {
        // 37% of 8 bytes = offset 2 (the '\n' after "aa"); snap forward to 'b' at 3.
        let d = doc(b"aa\nb\ncc\n", 1 << 20);
        let a = d.goto_percent(37).await.unwrap();
        assert_eq!(a, Anchor(3));
    }
    #[tokio::test]
    async fn goto_percent_zero_is_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        assert_eq!(d.goto_percent(0).await.unwrap(), Anchor::TOP);
    }
    #[test]
    fn percent_offset_handles_huge_sparse_sizes() {
        // sparse files can be hundreds of PB; the multiplication must not overflow.
        assert_eq!(percent_offset(u64::MAX, 50), u64::MAX / 2);
        assert_eq!(percent_offset(u64::MAX, 100), u64::MAX);
        assert_eq!(percent_offset(8, 37), 2);
        assert_eq!(percent_offset(0, 50), 0);
    }
    #[test]
    fn hscroll_caps_on_construction_and_shift() {
        assert_eq!(HScroll::new(usize::MAX).columns(), MAX_HSCROLL);
        assert_eq!(HScroll::ZERO.shift(i64::MAX).columns(), MAX_HSCROLL);
        assert_eq!(HScroll::new(5).shift(-99).columns(), 0);
        assert_eq!(HScroll::new(5).shift(3).columns(), 8);
    }
    #[test]
    fn hscroll_shift_handles_i64_min() {
        assert_eq!(HScroll::new(MAX_HSCROLL).shift(i64::MIN).columns(), 0);
    }
    #[tokio::test]
    async fn goto_percent_on_trailing_newline_clamps_to_last_line() {
        // a\nb\n size 4; 75% -> byte 3 (the trailing '\n'); snapping forward would
        // reach EOF (a blank screen), so it must clamp to the last content line.
        let d = doc(b"a\nb\n", 1 << 20);
        let a = d.goto_percent(75).await.unwrap();
        assert_eq!(a, Anchor(2));
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use crate::source::MockSource;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }
    fn is_line_start(data: &[u8], off: u64) -> bool {
        off == 0 || data.get(off as usize - 1) == Some(&b'\n')
    }
    proptest! {
        #[test]
        fn navigation_always_lands_on_a_line_start(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..256,
            ),
            block_size in 1usize..48,
            down in 0i64..64,
            up in 0i64..64,
            pct in 0u8..=100,
            rows in 1usize..24,
        ) {
            let doc = Document::new(
                std::sync::Arc::new(MockSource::new(data.clone())),
                Config { block_size, ..Config::default() },
            );
            rt().block_on(async {
                let a = doc.scroll_lines(Anchor::TOP, down).await.unwrap();
                prop_assert!(is_line_start(&data, a.offset()), "down -> {}", a.offset());
                prop_assert!(a.offset() == 0 || a.offset() < data.len() as u64);
                let b = doc.scroll_lines(a, -up).await.unwrap();
                prop_assert!(is_line_start(&data, b.offset()), "up -> {}", b.offset());
                prop_assert!(b.offset() <= a.offset());
                let p = doc.goto_percent(pct).await.unwrap();
                prop_assert!(is_line_start(&data, p.offset()), "pct -> {}", p.offset());
                prop_assert!(p.offset() == 0 || p.offset() < data.len() as u64);
                let e = doc.goto_end(rows).await.unwrap();
                prop_assert!(is_line_start(&data, e.offset()), "end -> {}", e.offset());
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn viewport_reads_stay_within_the_scan_budget(
            len in 0usize..(1 << 16),
            block_size in 1usize..1024,
            rows in 1usize..12,
            cols in 0usize..80,
            hscroll in prop_oneof![0usize..200, Just(usize::MAX)],
        ) {
            // all-x content has no newlines, so the budget is the only stop.
            let src = std::sync::Arc::new(MockSource::new(vec![b'x'; len]));
            let doc = Document::new(
                src.clone(),
                Config { block_size, ..Config::default() },
            );
            rt().block_on(async {
                let _ = doc.viewport(Anchor::TOP, rows, cols, HScroll::new(hscroll)).await.unwrap();
                Ok::<(), TestCaseError>(())
            })?;
            let span = HScroll::new(hscroll).columns().saturating_add(cols.max(1));
            let budget = block_size.max(rows.saturating_mul(span).saturating_mul(4));
            prop_assert!(
                src.read_count() as usize * block_size <= budget + block_size,
                "{} reads of {} bytes vs budget {}",
                src.read_count(),
                block_size,
                budget
            );
        }
    }
}
