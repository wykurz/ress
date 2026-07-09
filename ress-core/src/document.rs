//! The document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
use crate::resolve::Resolution;
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
    /// Crate-internal constructor for tests that compute offsets directly
    /// (the public invariant still holds: callers pass line starts). Only
    /// tests need this today, hence `cfg(test)` rather than a permanent
    /// `pub(crate)` surface with no production caller.
    #[cfg(test)]
    pub(crate) fn at(offset: u64) -> Anchor {
        Anchor(offset)
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
    cache: Arc<crate::cache::BlockCache>,
    size: u64,
    config: Config,
    prefetcher: crate::prefetch::Prefetcher,
}
impl Document {
    pub fn new(source: Arc<dyn BlockSource>, config: Config) -> Self {
        let size = source.size();
        let cache = Arc::new(crate::cache::BlockCache::new(
            source,
            config.block_size,
            config.cache_bytes,
        ));
        let prefetcher = crate::prefetch::Prefetcher::new(cache.clone(), config.prefetch_depth);
        Self {
            cache,
            size,
            config,
            prefetcher,
        }
    }
    /// The shared cache, for the prefetcher.
    pub(crate) fn cache(&self) -> &Arc<crate::cache::BlockCache> {
        &self.cache
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
        self.prefetcher.note_viewport(top);
        // include the horizontal offset so scrolling right into a long line reads
        // far enough to find the window's bytes instead of rendering a blank row;
        // `HScroll` is capped on construction, so the budget stays bounded.
        let span = hscroll.columns().saturating_add(cols.max(1));
        let scan_budget = self
            .config
            .block_size
            .max(rows.saturating_mul(span).saturating_mul(4));
        let (buf, stop) =
            crate::scan::fill_lines(self.cache(), top.offset(), rows, scan_budget).await?;
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
    /// Moves the top anchor by `delta` lines. Ready within the interactive
    /// budget; a longer scan continues in the background as a pending
    /// operation (any await point cancels cleanly).
    pub async fn scroll_lines(&self, from: Anchor, delta: i64) -> anyhow::Result<Resolution> {
        match delta.cmp(&0) {
            std::cmp::Ordering::Equal => Ok(Resolution::Ready(from)),
            std::cmp::Ordering::Greater => self.scroll_down(from.offset(), delta as usize).await,
            std::cmp::Ordering::Less => {
                self.scroll_up(from.offset(), delta.unsigned_abs() as usize)
                    .await
            }
        }
    }
    /// Finds the start of the `n`-th line after `start`, clamped at the furthest
    /// line start the budgeted scan saw (EOF or budget — never past EOF); a
    /// budget outcome continues as a pending background scan.
    async fn scroll_down(&self, start: u64, n: usize) -> anyhow::Result<Resolution> {
        // a zero budget would make pending chunks spin without forward
        // progress; one byte is the smallest honest chunk.
        let budget = self.config.nav_scan_budget.max(1);
        match crate::scan::nth_line_start_after(self.cache(), start, n, budget).await? {
            crate::scan::Forward::Found { start } => Ok(Resolution::Ready(Anchor(start))),
            crate::scan::Forward::Eof { last_start } => Ok(Resolution::Ready(Anchor(last_start))),
            crate::scan::Forward::Budget {
                last_start,
                resume,
                found,
            } => {
                let cache = self.cache.clone();
                let span = self.size - start;
                let scanned = resume - start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scanned,
                    move |tx| async move {
                        let s = forward_to_completion(
                            cache,
                            resume,
                            last_start,
                            n - found,
                            budget,
                            tx,
                            crate::resolve::Progress { scanned, span },
                        )
                        .await?;
                        Ok(Anchor(s))
                    },
                )))
            }
        }
    }
    /// Finds the start of the `n`-th line before `start`, clamped at 0; a
    /// budget outcome continues as a pending background scan to the true
    /// target instead of staying put.
    async fn scroll_up(&self, start: u64, n: usize) -> anyhow::Result<Resolution> {
        if start == 0 || n == 0 {
            return Ok(Resolution::Ready(Anchor(start)));
        }
        let budget = self.config.nav_scan_budget.max(1);
        match crate::scan::nth_newline_before(self.cache(), start, n, budget).await? {
            crate::scan::Backward::Found { start: s } => Ok(Resolution::Ready(Anchor(s))),
            crate::scan::Backward::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::Backward::Budget { resume, found } => {
                let cache = self.cache.clone();
                let span = start;
                let scanned = start - resume;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scanned,
                    move |tx| async move {
                        match backward_to_completion(
                            cache,
                            resume,
                            n - found,
                            budget,
                            tx,
                            span,
                            scanned,
                        )
                        .await?
                        {
                            Some(s) => Ok(Anchor(s)),
                            None => Ok(Anchor::TOP),
                        }
                    },
                )))
            }
        }
    }
    /// The top of the file — always answerable.
    pub fn goto_top(&self) -> Anchor {
        Anchor::TOP
    }
    /// The anchor that puts the file's final content line on the bottom row;
    /// a tail that cannot be found within the interactive budget resolves as
    /// a pending background scan.
    pub async fn goto_end(&self, rows: usize) -> anyhow::Result<Resolution> {
        if self.size == 0 {
            return Ok(Resolution::Ready(Anchor::TOP));
        }
        let budget = self.config.nav_scan_budget.max(1);
        let up = rows.saturating_sub(1);
        match crate::scan::nth_newline_before(self.cache(), self.size, 1, budget).await? {
            crate::scan::Backward::Found { start: last } => match self.scroll_up(last, up).await? {
                Resolution::Ready(a) => Ok(Resolution::Ready(a)),
                Resolution::Pending(mut p) => {
                    // the user asked to jump to the end, not to scroll.
                    p.label = "jumping to end";
                    Ok(Resolution::Pending(p))
                }
            },
            crate::scan::Backward::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::Backward::Budget { resume, found } => {
                let cache = self.cache.clone();
                let span = self.size;
                let scanned = self.size - resume;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to end",
                    span,
                    scanned,
                    move |tx| async move {
                        let last = match backward_to_completion(
                            cache.clone(),
                            resume,
                            1 - found,
                            budget,
                            tx.clone(),
                            span,
                            scanned,
                        )
                        .await?
                        {
                            Some(s) => s,
                            None => return Ok(Anchor::TOP),
                        };
                        if up == 0 {
                            return Ok(Anchor(last));
                        }
                        // walking up from line start `last` searches [0, last-1): the
                        // byte at last-1 is the newline that made `last` a line start.
                        match backward_to_completion(cache, last - 1, up, budget, tx, span, span)
                            .await?
                        {
                            Some(s) => Ok(Anchor(s)),
                            None => Ok(Anchor::TOP),
                        }
                    },
                )))
            }
        }
    }
    /// Jumps `pct`% through the file by byte, snapped to a line start;
    /// a snap that cannot resolve within the interactive budget continues as
    /// a pending background scan to the line containing the target.
    pub async fn goto_percent(&self, pct: u8) -> anyhow::Result<Resolution> {
        let pct = pct.min(100) as u64;
        if self.size == 0 || pct == 0 {
            return Ok(Resolution::Ready(Anchor::TOP));
        }
        if pct >= 100 {
            return self.goto_end(1).await;
        }
        let target = percent_offset(self.size, pct);
        if target == 0 {
            return Ok(Resolution::Ready(Anchor::TOP));
        }
        let budget = self.config.nav_scan_budget.max(1);
        let bs = self.cache.block_size() as u64;
        let prev = self.cache.block((target - 1) / bs).await?;
        let rel = ((target - 1) % bs) as usize;
        if prev.get(rel) == Some(&b'\n') {
            return Ok(Resolution::Ready(Anchor(target)));
        }
        // no line start at-or-after within reach means the answer is the line
        // containing the target: snap backward, pending past the budget.
        match crate::scan::nth_line_start_after(self.cache(), target, 1, budget).await? {
            crate::scan::Forward::Found { start } => return Ok(Resolution::Ready(Anchor(start))),
            crate::scan::Forward::Eof { .. } | crate::scan::Forward::Budget { .. } => {}
        }
        match crate::scan::nth_newline_before(self.cache(), target, 1, budget).await? {
            crate::scan::Backward::Found { start } => Ok(Resolution::Ready(Anchor(start))),
            crate::scan::Backward::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::Backward::Budget { resume, found } => {
                let cache = self.cache.clone();
                let span = target;
                let scanned = target - resume;
                Ok(Resolution::Pending(spawn_pending(
                    "percent jump",
                    span,
                    scanned,
                    move |tx| async move {
                        match backward_to_completion(
                            cache,
                            resume,
                            1 - found,
                            budget,
                            tx,
                            span,
                            scanned,
                        )
                        .await?
                        {
                            Some(s) => Ok(Anchor(s)),
                            None => Ok(Anchor::TOP),
                        }
                    },
                )))
            }
        }
    }
}
/// Runs a backward newline search to completion in budget-sized chunks,
/// publishing progress after each. `bound` is the exclusive upper end of the
/// not-yet-searched region: the scan covers all of `[0, bound)`, including
/// the boundary byte a fresh `nth_newline_before` entry call would skip.
async fn backward_to_completion(
    cache: Arc<crate::cache::BlockCache>,
    bound: u64,
    n: usize,
    chunk: usize,
    tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
    span: u64,
    already_scanned: u64,
) -> anyhow::Result<Option<u64>> {
    let mut bound = bound;
    let mut remaining = n;
    let mut scanned = already_scanned;
    loop {
        // `nth_newline_before(pos)` searches `[0, pos - 1)`, so `bound + 1`
        // covers `[0, bound)` exactly; `bound` is always < size, so the
        // increment cannot overflow.
        match crate::scan::nth_newline_before(&cache, bound + 1, remaining, chunk).await? {
            crate::scan::Backward::Found { start } => return Ok(Some(start)),
            crate::scan::Backward::Top => return Ok(None),
            crate::scan::Backward::Budget { resume, found } => {
                scanned += bound - resume;
                remaining -= found;
                bound = resume;
                let _ = tx.send(crate::resolve::Progress { scanned, span });
                // a hot cache can serve whole chunks without ever yielding;
                // give aborts a guaranteed point to take effect between chunks.
                tokio::task::yield_now().await;
            }
        }
    }
}
/// Forward twin of `backward_to_completion`; resolves to the n-th line start
/// after `from`, clamped at the last verified line start when EOF arrives
/// first. `last_known` seeds that clamp; a chunk's `last_start` is trusted
/// only when it advanced past the chunk's entry cursor, which is exactly
/// when the chunk saw a real newline (otherwise it is the mid-line cursor).
async fn forward_to_completion(
    cache: Arc<crate::cache::BlockCache>,
    from: u64,
    last_known: u64,
    n: usize,
    chunk: usize,
    tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
    initial: crate::resolve::Progress,
) -> anyhow::Result<u64> {
    let span = initial.span;
    let mut resume = from;
    let mut remaining = n;
    let mut last = last_known;
    let mut scanned = initial.scanned;
    // resuming mid-line is sound: the scanner only counts newlines at or
    // after `resume`, and every byte before it was scanned by prior chunks.
    loop {
        match crate::scan::nth_line_start_after(&cache, resume, remaining, chunk).await? {
            crate::scan::Forward::Found { start } => return Ok(start),
            crate::scan::Forward::Eof { last_start } => {
                return Ok(if last_start > resume {
                    last_start
                } else {
                    last
                });
            }
            crate::scan::Forward::Budget {
                last_start,
                resume: r,
                found,
            } => {
                if last_start > resume {
                    last = last_start;
                }
                scanned += r - resume;
                remaining -= found;
                resume = r;
                let _ = tx.send(crate::resolve::Progress { scanned, span });
                // a hot cache can serve whole chunks without ever yielding;
                // give aborts a guaranteed point to take effect between chunks.
                tokio::task::yield_now().await;
            }
        }
    }
}
/// Spawns a pending-navigation task with a fresh progress channel.
fn spawn_pending<F, Fut>(
    label: &'static str,
    span: u64,
    scanned: u64,
    task: F,
) -> crate::resolve::PendingNav
where
    F: FnOnce(tokio::sync::watch::Sender<crate::resolve::Progress>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Anchor>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::watch::channel(crate::resolve::Progress { scanned, span });
    crate::resolve::PendingNav {
        label,
        progress: rx,
        handle: tokio::spawn(task(tx)),
    }
}
/// Byte offset `pct`% into `size` bytes, computed in u128 so huge (sparse)
/// file sizes near `u64::MAX` can't overflow the multiplication.
fn percent_offset(size: u64, pct: u64) -> u64 {
    (size as u128 * pct as u128 / 100) as u64
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
                prefetch_depth: 0,
                ..Config::default()
            },
        )
    }
    #[test]
    fn cache_accessor_exposes_the_shared_block_cache() {
        // the prefetcher (a later plan) reads blocks through this accessor;
        // it must see the same source the document itself reads through.
        let d = doc(b"a\nb\nc\n", 4);
        assert_eq!(d.cache().block_size(), 4);
        assert_eq!(d.cache().size(), d.size());
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
                prefetch_depth: 0,
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
                prefetch_depth: 0,
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
                prefetch_depth: 0,
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
        let a = d.scroll_lines(Anchor::TOP, 1).await.unwrap().ready();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_down_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor::TOP, 2).await.unwrap().ready();
        assert_eq!(a, Anchor(10));
    }
    #[tokio::test]
    async fn scroll_down_clamps_to_last_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap().ready();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scroll_down_clamps_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap().ready();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scrolls_up_one_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(4), -1).await.unwrap().ready();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_up_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor(10), -2).await.unwrap().ready();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn scroll_up_clamps_at_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), -99).await.unwrap().ready();
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
        let d = Document::new(
            src.clone(),
            Config {
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let top = d.scroll_lines(Anchor(198), -40).await.unwrap().ready();
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
        assert_eq!(
            d.scroll_lines(Anchor::TOP, 5).await.unwrap().ready(),
            Anchor::TOP
        );
        assert_eq!(
            d.scroll_lines(Anchor::TOP, -5).await.unwrap().ready(),
            Anchor::TOP
        );
        assert_eq!(d.goto_end(3).await.unwrap().ready(), Anchor::TOP);
        assert_eq!(d.goto_percent(50).await.unwrap().ready(), Anchor::TOP);
    }
    #[tokio::test]
    async fn scrolling_back_through_cached_blocks_reads_nothing() {
        // scroll forward through several blocks, then back: the return trip must
        // be served entirely from cache (this is the pager's whole thesis).
        let mut data = Vec::new();
        for i in 0..2000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 1024,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let mut top = Anchor::TOP;
        for _ in 0..10 {
            top = d.scroll_lines(top, 50).await.unwrap().ready();
            let _ = d.viewport(top, 40, 80, HScroll::ZERO).await.unwrap();
        }
        let cold = src.read_count();
        for _ in 0..10 {
            top = d.scroll_lines(top, -50).await.unwrap().ready();
            let _ = d.viewport(top, 40, 80, HScroll::ZERO).await.unwrap();
        }
        assert_eq!(src.read_count(), cold, "return trip issued cold reads");
    }
    #[tokio::test]
    async fn scroll_zero_is_identity() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), 0).await.unwrap().ready();
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
        let a = d.goto_end(2).await.unwrap().ready();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.goto_end(1).await.unwrap().ready();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_clamps_when_file_shorter_than_screen() {
        let d = doc(b"a\nb\n", 1 << 20);
        let a = d.goto_end(10).await.unwrap().ready();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_end_of_empty_file_is_top() {
        let d = doc(b"", 1 << 20);
        let a = d.goto_end(10).await.unwrap().ready();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_snaps_to_line_start() {
        // size 8; 50% -> offset 4, already a line start ('c').
        let d = doc(b"a\nb\nc\nd\n", 1 << 20);
        let a = d.goto_percent(50).await.unwrap().ready();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_percent_rounds_up_to_next_line() {
        // 37% of 8 bytes = offset 2 (the '\n' after "aa"); snap forward to 'b' at 3.
        let d = doc(b"aa\nb\ncc\n", 1 << 20);
        let a = d.goto_percent(37).await.unwrap().ready();
        assert_eq!(a, Anchor(3));
    }
    #[tokio::test]
    async fn goto_percent_zero_is_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        assert_eq!(d.goto_percent(0).await.unwrap().ready(), Anchor::TOP);
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
        let a = d.goto_percent(75).await.unwrap().ready();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn budgeted_percent_jump_lands_in_the_target_line() {
        // the next newline after the percent target is beyond the budget; snap
        // backward to the line containing the target instead of seeking EOF.
        let mut data = vec![b'a'; 10000];
        data.push(b'\n');
        data.extend_from_slice(&[b'b'; 10000]);
        data.push(b'\n');
        data.extend_from_slice(b"tail");
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        assert_eq!(d.goto_percent(51).await.unwrap().ready(), Anchor(10001));
    }
    #[tokio::test]
    async fn percent_into_a_long_final_line_snaps_to_its_start() {
        // the final line is longer than the nav budget measured from EOF, but
        // its start is within budget of the percent target; the jump must land
        // there, not fall back to the top.
        let mut data = b"aaa\n".to_vec();
        data.extend_from_slice(&[b'b'; 400]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        assert_eq!(d.goto_percent(50).await.unwrap().ready(), Anchor(4));
    }
    #[tokio::test]
    async fn budget_exhausted_scroll_up_pends_then_resolves() {
        // 100 short lines then a giant tail; scrolling up from deep inside the
        // tail exceeds the interactive budget, pends, and resolves correctly.
        // the newline before 8000 is at 199, so the line start is 200.
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.scroll_lines(Anchor::at(8000), -1).await.unwrap() {
            Resolution::Pending(p) => {
                assert_eq!(p.label, "scrolling");
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(200));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn backward_pending_finds_a_newline_on_the_chunk_boundary() {
        // the needed newline sits exactly at `resume - 1` of the first
        // continuation chunk; skipping the boundary byte would sail past it
        // to the top instead of landing on the true line start.
        let mut data = vec![b'x'; 7679];
        data.push(b'\n');
        data.extend_from_slice(&[b'x'; 1321]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.scroll_lines(Anchor::at(8000), -1).await.unwrap() {
            Resolution::Pending(p) => {
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(7680), "must land on the boundary-byte line start");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn zero_nav_budget_still_resolves() {
        // budget 0 must not spin an unabortable chunk loop; it clamps to one
        // byte and completes.
        let mut data = b"a\n".to_vec();
        data.extend_from_slice(&[b'x'; 200]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 16,
                nav_scan_budget: 0,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.scroll_lines(Anchor::TOP, 2).await.unwrap() {
            Resolution::Pending(p) => {
                let a = tokio::time::timeout(std::time::Duration::from_secs(2), p.handle)
                    .await
                    .expect("zero-budget pending scan hung")
                    .unwrap()
                    .unwrap();
                assert_eq!(a, Anchor(2));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn budget_exhausted_scroll_down_pends_and_lands_on_a_line_start() {
        // a newline-less tail longer than the budget: the pending resolution
        // must clamp to the last real line start, never a mid-line offset.
        let mut data = b"a\n".to_vec();
        data.extend_from_slice(&[b'x'; 8192]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.scroll_lines(Anchor::TOP, 2).await.unwrap() {
            Resolution::Pending(p) => {
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(2), "must clamp to the giant line's start");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn budgeted_percent_jump_pends_and_resolves_to_the_containing_line() {
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.goto_percent(90).await.unwrap() {
            Resolution::Pending(p) => {
                assert_eq!(p.label, "percent jump");
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(200), "the line containing the target byte");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn goto_end_walk_up_pends_with_the_jump_label() {
        // stage 1 finds the tail within budget; the walk-up stage pends and
        // carries the user's intent in its label.
        let mut data = vec![b'x'; 8192];
        data.extend_from_slice(b"\ny\ny\n");
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.goto_end(3).await.unwrap() {
            Resolution::Pending(p) => {
                assert_eq!(p.label, "jumping to end");
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor::TOP, "no second newline above the tail");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn goto_end_pends_and_finds_the_true_tail() {
        // the 3a behavior fell back to TOP here; pending resolution finds the
        // real answer: the giant unterminated line is the last content line.
        // last line start = 200 (the byte after the final '\n' at 199), then
        // one line up = 198, the start of the last "y\n".
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"y\n");
        }
        data.extend_from_slice(&[b'x'; 8192]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.goto_end(2).await.unwrap() {
            Resolution::Pending(p) => {
                assert_eq!(p.label, "jumping to end");
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(198));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn cancelled_pending_nav_leaves_the_engine_healthy() {
        let src = Arc::new(
            MockSource::new({
                let mut data = vec![b'a'; 8192];
                data.push(b'\n');
                data
            })
            .with_latency(std::time::Duration::from_millis(5)),
        );
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.scroll_lines(Anchor::at(8000), -1).await.unwrap() else {
            panic!("expected Pending");
        };
        p.cancel();
        let _ = p.handle.await; // aborted join error is fine
        // the same operation immediately afterwards completes correctly —
        // no wedged blocks, no poisoned state (3a's coalescing self-heals).
        let a = d
            .scroll_lines(Anchor::at(8000), -1)
            .await
            .unwrap()
            .join()
            .await;
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn pending_progress_is_published() {
        let src = Arc::new(MockSource::new(vec![b'x'; 8192]));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(mut p) = d.scroll_lines(Anchor::at(8000), -1).await.unwrap() else {
            panic!("expected Pending");
        };
        let first = *p.progress.borrow();
        assert!(first.scanned > 0 && first.span == 8000);
        let a = (&mut p.handle).await.unwrap().unwrap();
        assert_eq!(a, Anchor::TOP);
        let last = *p.progress.borrow();
        assert!(last.scanned >= first.scanned);
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
                Config { block_size, prefetch_depth: 0, ..Config::default() },
            );
            rt().block_on(async {
                let a = doc.scroll_lines(Anchor::TOP, down).await.unwrap().ready();
                prop_assert!(is_line_start(&data, a.offset()), "down -> {}", a.offset());
                prop_assert!(a.offset() == 0 || a.offset() < data.len() as u64);
                let b = doc.scroll_lines(a, -up).await.unwrap().ready();
                prop_assert!(is_line_start(&data, b.offset()), "up -> {}", b.offset());
                prop_assert!(b.offset() <= a.offset());
                let p = doc.goto_percent(pct).await.unwrap().ready();
                prop_assert!(is_line_start(&data, p.offset()), "pct -> {}", p.offset());
                prop_assert!(p.offset() == 0 || p.offset() < data.len() as u64);
                let e = doc.goto_end(rows).await.unwrap().ready();
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
                Config { block_size, prefetch_depth: 0, ..Config::default() },
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
