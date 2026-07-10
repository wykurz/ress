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
    scheduler: crate::schedule::ScanScheduler,
}
impl Document {
    /// Must be called within a tokio runtime: construction spawns the
    /// background index scan.
    pub fn new(source: Arc<dyn BlockSource>, config: Config) -> Self {
        let size = source.size();
        let cache = Arc::new(crate::cache::BlockCache::new(
            source,
            config.block_size,
            config.cache_bytes,
        ));
        let prefetcher = crate::prefetch::Prefetcher::new(cache.clone(), config.prefetch_depth);
        let scheduler = crate::schedule::ScanScheduler::spawn(cache.clone());
        Self {
            cache,
            size,
            config,
            prefetcher,
            scheduler,
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
    /// Finds the start of the `n`-th line after `start`, clamped at the last
    /// real line start when EOF arrives first; a budget outcome continues as
    /// a pending background scan of the same scan object.
    async fn scroll_down(&self, start: u64, n: usize) -> anyhow::Result<Resolution> {
        let mut scan = crate::scan::ForwardScan::new(start, n, self.config.nav_scan_budget);
        match scan.step(self.cache()).await? {
            crate::scan::FwdStep::Found(s) => Ok(Resolution::Ready(Anchor(s))),
            crate::scan::FwdStep::Eof(clamp) => Ok(Resolution::Ready(Anchor(clamp))),
            crate::scan::FwdStep::More => {
                let cache = self.cache.clone();
                let span = self.size - start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.scanned(),
                    move |tx| async move { scan.complete(&cache, tx, span).await.map(Anchor) },
                )))
            }
        }
    }
    /// Finds the start of the `n`-th line before `start`, clamped at 0; a
    /// budget outcome continues as a pending background scan of the same
    /// scan object.
    async fn scroll_up(&self, start: u64, n: usize) -> anyhow::Result<Resolution> {
        if start == 0 || n == 0 {
            return Ok(Resolution::Ready(Anchor(start)));
        }
        let mut scan = crate::scan::BackwardScan::new(start, n, self.config.nav_scan_budget);
        match scan.step(self.cache()).await? {
            crate::scan::BwdStep::Found(s) => Ok(Resolution::Ready(Anchor(s))),
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::BwdStep::More => {
                let cache = self.cache.clone();
                let span = start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.scanned(),
                    move |tx| async move { scan.complete(&cache, tx, span).await.map(Anchor) },
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
        let budget = self.config.nav_scan_budget;
        let up = rows.saturating_sub(1);
        let mut scan = crate::scan::BackwardScan::new(self.size, 1, budget);
        match scan.step(self.cache()).await? {
            crate::scan::BwdStep::Found(last) => match self.scroll_up(last, up).await? {
                Resolution::Ready(a) => Ok(Resolution::Ready(a)),
                Resolution::Pending(mut p) => {
                    // the user asked to jump to the end, not to scroll.
                    p.label = "jumping to end";
                    Ok(Resolution::Pending(p))
                }
            },
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::BwdStep::More => {
                let cache = self.cache.clone();
                let span = self.size;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to end",
                    span,
                    scan.scanned(),
                    move |tx| async move {
                        let last = scan.complete(&cache, tx.clone(), span).await?;
                        if up == 0 {
                            // `BackwardScan::new(last, 0, ..)` would resolve
                            // Top (anchor 0); the tail line itself is the
                            // answer here, so guard before constructing.
                            return Ok(Anchor(last));
                        }
                        let walk = crate::scan::BackwardScan::new(last, up, budget);
                        // the walk-up is its own phase with its own honest
                        // span; inheriting the first phase's span pinned the
                        // progress row at 99% for the whole walk.
                        let span2 = walk.remaining_bytes();
                        walk.complete(&cache, tx, span2).await.map(Anchor)
                    },
                )))
            }
        }
    }
    /// Jumps `pct`% through the file by byte, landing on the start of the
    /// line containing the target byte (less-style, so the destination never
    /// depends on the scan budget); a snap that cannot resolve within the
    /// interactive budget continues as a pending background scan.
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
        let budget = self.config.nav_scan_budget;
        let bs = self.cache.block_size() as u64;
        let prev = self.cache.block((target - 1) / bs).await?;
        let rel = ((target - 1) % bs) as usize;
        if prev.get(rel) == Some(&b'\n') {
            return Ok(Resolution::Ready(Anchor(target)));
        }
        // the answer is the line containing the target byte: search backward
        // for the newline that starts that line, pending past the budget.
        let mut snap = crate::scan::BackwardScan::new(target, 1, budget);
        match snap.step(self.cache()).await? {
            crate::scan::BwdStep::Found(start) => Ok(Resolution::Ready(Anchor(start))),
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(Anchor::TOP)),
            crate::scan::BwdStep::More => {
                let cache = self.cache.clone();
                let span = target;
                Ok(Resolution::Pending(spawn_pending(
                    "percent jump",
                    span,
                    snap.scanned(),
                    move |tx| async move { snap.complete(&cache, tx, span).await.map(Anchor) },
                )))
            }
        }
    }
    /// Jumps to 1-based line `n` (`0` is treated as `1`), landing on its
    /// start; a line past the end clamps to the last real line. Answers
    /// come from the background line index: a query beyond its frontier
    /// pends on index progress, then walks the at-most-1023-line tail from
    /// the nearest checkpoint with a budgeted forward scan.
    pub async fn goto_line(&self, n: u64) -> anyhow::Result<Resolution> {
        let line0 = n.saturating_sub(1);
        let budget = self.config.nav_scan_budget;
        let (covered, clamp0, cp) = {
            let ix = self.scheduler.index().lock().unwrap();
            let covered = ix.newlines() >= line0;
            let clamp0 = ix.total_lines().map(|t| t.saturating_sub(1));
            (
                covered,
                clamp0,
                ix.nearest_checkpoint(line0.min(ix.newlines())),
            )
        };
        if covered {
            let (line0, cp) = if cp.0 >= self.size {
                // the queried start sits at EOF — a trailing newline's
                // phantom line, not a line; the real last line is the one
                // before it (ERRATUM 4a#3).
                let prev = line0.saturating_sub(1);
                let cp = self
                    .scheduler
                    .index()
                    .lock()
                    .unwrap()
                    .nearest_checkpoint(prev);
                (prev, cp)
            } else {
                (line0, cp)
            };
            return self.walk_to_line(line0, cp, budget).await;
        }
        if let Some(clamp0) = clamp0 {
            // the scan is done and the line does not exist: vim clamps to
            // the last real line (an empty file has none — the top).
            if self.size == 0 {
                return Ok(Resolution::Ready(Anchor::TOP));
            }
            let cp = self
                .scheduler
                .index()
                .lock()
                .unwrap()
                .nearest_checkpoint(clamp0);
            return self.walk_to_line(clamp0, cp, budget).await;
        }
        let cache = self.cache.clone();
        let index = self.scheduler.index().clone();
        let mut frontier = self.scheduler.frontier();
        let size = self.size;
        let seed = frontier.borrow().processed_up_to;
        Ok(Resolution::Pending(spawn_pending(
            "jumping to line",
            size,
            seed,
            move |tx| async move {
                let (line0, cp) = loop {
                    let snapshot = {
                        let ix = index.lock().unwrap();
                        if ix.newlines() >= line0 {
                            Some((line0, ix.nearest_checkpoint(line0)))
                        } else if let Some(t) = ix.total_lines() {
                            let clamp0 = t.saturating_sub(1);
                            Some((clamp0, ix.nearest_checkpoint(clamp0)))
                        } else {
                            None
                        }
                    };
                    if let Some(found) = snapshot {
                        break found;
                    }
                    frontier
                        .changed()
                        .await
                        .map_err(|_| anyhow::anyhow!("index scan ended unexpectedly"))?;
                    let f = *frontier.borrow();
                    let _ = tx.send(crate::resolve::Progress {
                        scanned: f.processed_up_to,
                        span: size,
                    });
                };
                if size == 0 {
                    return Ok(Anchor::TOP);
                }
                let (line0, cp) = if cp.0 >= size {
                    // the queried start sits at EOF — a trailing newline's
                    // phantom line, not a line; the real last line is the
                    // one before it (ERRATUM 4a#3).
                    let prev = line0.saturating_sub(1);
                    (prev, index.lock().unwrap().nearest_checkpoint(prev))
                } else {
                    (line0, cp)
                };
                let (cp_off, cp_line0) = cp;
                let scan =
                    crate::scan::ForwardScan::new(cp_off, (line0 - cp_line0) as usize, budget);
                // the tail walk is its own phase, like goto_end's walk-up.
                let span2 = size - cp_off;
                scan.complete(&cache, tx, span2).await.map(Anchor)
            },
        )))
    }
    /// The covered-index tail walk: one interactive step, then the
    /// standard pending continuation when the budget runs out first.
    async fn walk_to_line(
        &self,
        line0: u64,
        cp: (u64, u64),
        budget: usize,
    ) -> anyhow::Result<Resolution> {
        if self.size == 0 {
            return Ok(Resolution::Ready(Anchor::TOP));
        }
        let (cp_off, cp_line0) = cp;
        let mut scan = crate::scan::ForwardScan::new(cp_off, (line0 - cp_line0) as usize, budget);
        match scan.step(self.cache()).await? {
            crate::scan::FwdStep::Found(s) => Ok(Resolution::Ready(Anchor(s))),
            crate::scan::FwdStep::Eof(clamp) => Ok(Resolution::Ready(Anchor(clamp))),
            crate::scan::FwdStep::More => {
                let cache = self.cache.clone();
                let span = self.size - cp_off;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to line",
                    span,
                    scan.scanned(),
                    move |tx| async move { scan.complete(&cache, tx, span).await.map(Anchor) },
                )))
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
    #[tokio::test]
    async fn cache_accessor_exposes_the_shared_block_cache() {
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
    async fn goto_percent_lands_on_the_containing_line() {
        // 37% of 8 bytes = offset 2, the '\n' terminating "aa": that byte
        // belongs to the "aa" line, so the jump lands on its start.
        let d = doc(b"aa\nb\ncc\n", 1 << 20);
        let a = d.goto_percent(37).await.unwrap().ready();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_zero_is_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        assert_eq!(d.goto_percent(0).await.unwrap().ready(), Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_with_no_newline_above_target_is_top() {
        // the target sits mid-line with nothing but content above it: the
        // containing line is the first line, deterministically pinned (the
        // property covers this class; this pins the exact branch).
        let d = doc(b"abcdefgh", 1 << 20);
        assert_eq!(d.goto_percent(50).await.unwrap().ready(), Anchor::TOP);
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
        // a\nb\n size 4; 75% -> byte 3, the trailing '\n' itself: that byte
        // belongs to the "b" line, so the jump lands on its start, byte 2.
        let d = doc(b"a\nb\n", 1 << 20);
        let a = d.goto_percent(75).await.unwrap().ready();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn budgeted_percent_jump_lands_in_the_target_line() {
        // the target byte sits inside the second (10000-byte) line; the jump
        // lands on that line's start, well within the 256-byte backward budget.
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
    async fn goto_percent_is_budget_independent() {
        // the oracle's minimal divergence case: round-up-within-budget used
        // to send an unlimited budget to 7 and budget=1 to 4 (ERRATUM 3c#4).
        for budget in [1usize, 1 << 20] {
            let src = Arc::new(MockSource::new(bytes::Bytes::from_static(b"\n\na\nbc\nd")));
            let d = Document::new(
                src,
                Config {
                    block_size: 1,
                    nav_scan_budget: budget,
                    prefetch_depth: 0,
                    ..Config::default()
                },
            );
            assert_eq!(d.goto_percent(63).await.unwrap().join().await, Anchor(4));
        }
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
    async fn goto_end_of_one_row_pends_to_the_tail_line() {
        // rows == 1 makes up == 0: the walk-up must be skipped entirely —
        // constructing a zero-count BackwardScan would resolve Top and
        // silently send "go to end" to the very top of the file.
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
        match d.goto_end(1).await.unwrap() {
            Resolution::Pending(p) => {
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor(200), "the giant tail's start, never TOP");
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
    fn lined_doc(lines: u32, block_size: usize) -> Document {
        let mut data = Vec::new();
        for i in 0..lines {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = Arc::new(MockSource::new(data));
        Document::new(
            src,
            Config {
                block_size,
                prefetch_depth: 0,
                ..Config::default()
            },
        )
    }
    #[tokio::test]
    async fn goto_line_one_is_top() {
        let d = lined_doc(10, 64);
        assert_eq!(d.goto_line(1).await.unwrap().join().await, Anchor::TOP);
        assert_eq!(d.goto_line(0).await.unwrap().join().await, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_line_lands_on_the_nth_line_start() {
        // ten-byte lines: line n (1-based) starts at (n-1) * 10.
        let d = lined_doc(3000, 256);
        assert_eq!(d.goto_line(5).await.unwrap().join().await, Anchor::at(40));
        assert_eq!(
            d.goto_line(2000).await.unwrap().join().await,
            Anchor::at(19990)
        );
    }
    #[tokio::test]
    async fn goto_line_past_the_end_clamps_to_the_last_line() {
        let d = lined_doc(100, 64);
        assert_eq!(
            d.goto_line(1_000_000).await.unwrap().join().await,
            Anchor::at(990)
        );
    }
    #[tokio::test]
    async fn goto_line_on_an_empty_file_is_top() {
        let d = doc(b"", 64);
        assert_eq!(d.goto_line(7).await.unwrap().join().await, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_line_beyond_the_frontier_pends_with_progress() {
        // 2 ms latency per block keeps the background scan comfortably
        // behind the query, so the goto must pend on the frontier.
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = Arc::new(MockSource::new(data).with_latency(std::time::Duration::from_millis(2)));
        let d = Document::new(
            src,
            Config {
                block_size: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        match d.goto_line(3500).await.unwrap() {
            Resolution::Pending(p) => {
                assert_eq!(p.label, "jumping to line");
                let a = p.handle.await.unwrap().unwrap();
                assert_eq!(a, Anchor::at(34990));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({})", a.offset()),
        }
    }
    #[tokio::test]
    async fn goto_line_with_an_unterminated_last_line_clamps_to_it() {
        let mut data = b"a\nb\n".to_vec();
        data.extend_from_slice(&[b'x'; 100]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        assert_eq!(d.goto_line(99).await.unwrap().join().await, Anchor::at(4));
    }
    fn lined(pat: &'static str, lines: u32, block_size: usize) -> Document {
        let mut data = Vec::new();
        for _ in 0..lines {
            data.extend_from_slice(pat.as_bytes());
        }
        let src = Arc::new(MockSource::new(data));
        Document::new(
            src,
            Config {
                block_size,
                prefetch_depth: 0,
                ..Config::default()
            },
        )
    }
    #[tokio::test]
    async fn goto_line_one_past_a_checkpoint_aligned_end_clamps() {
        // 1024 lines of "x\n": the newline count is exactly one checkpoint
        // interval and the file ends on that newline, so line0 1024's
        // "start" is the at-EOF phantom candidate (ERRATUM 4a#3).
        let d = lined("x\n", 1024, 64);
        assert_eq!(d.goto_line(2).await.unwrap().join().await, Anchor::at(2));
        assert_eq!(
            d.goto_line(1025).await.unwrap().join().await,
            Anchor::at(2046),
            "one past the end clamps to the last real line, never EOF"
        );
    }
    #[tokio::test]
    async fn goto_line_phantom_clamp_holds_through_the_pending_path() {
        // same shape, but the query races a slow scan so resolution goes
        // through the pending closure's own coverage check.
        let src = Arc::new(
            MockSource::new(b"x\n".repeat(1024)).with_latency(std::time::Duration::from_millis(2)),
        );
        let d = Document::new(
            src,
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        assert_eq!(
            d.goto_line(1025).await.unwrap().join().await,
            Anchor::at(2046)
        );
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
            rt().block_on(async {
                let doc = Document::new(
                    std::sync::Arc::new(MockSource::new(data.clone())),
                    Config { block_size, prefetch_depth: 0, ..Config::default() },
                );
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
            rt().block_on(async {
                let doc = Document::new(
                    src.clone(),
                    Config { block_size, prefetch_depth: 0, ..Config::default() },
                );
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
        #[test]
        fn pending_and_sync_navigation_agree(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..160,
            ),
            block_size in 1usize..32,
            budget in 1usize..24,
            down in 1i64..40,
            up in 1i64..40,
            pct in 0u8..=100,
            rows in 1usize..10,
        ) {
            // the oracle for the whole pending machinery: a navigation that
            // pends and completes in tiny chunks must land exactly where the
            // same navigation lands when the interactive budget is unlimited.
            rt().block_on(async {
                let sync_doc = Document::new(
                    std::sync::Arc::new(MockSource::new(data.clone())),
                    Config { block_size, nav_scan_budget: usize::MAX, prefetch_depth: 0, ..Config::default() },
                );
                let tiny_doc = Document::new(
                    std::sync::Arc::new(MockSource::new(data.clone())),
                    Config { block_size, nav_scan_budget: budget, prefetch_depth: 0, ..Config::default() },
                );
                let a_sync = sync_doc.scroll_lines(Anchor::TOP, down).await.unwrap().ready();
                let a_tiny = tiny_doc.scroll_lines(Anchor::TOP, down).await.unwrap().join().await;
                prop_assert_eq!(a_sync, a_tiny, "scroll down diverged");
                let u_sync = sync_doc.scroll_lines(a_sync, -up).await.unwrap().ready();
                let u_tiny = tiny_doc.scroll_lines(a_sync, -up).await.unwrap().join().await;
                prop_assert_eq!(u_sync, u_tiny, "scroll up diverged");
                let p_sync = sync_doc.goto_percent(pct).await.unwrap().ready();
                let p_tiny = tiny_doc.goto_percent(pct).await.unwrap().join().await;
                prop_assert_eq!(p_sync, p_tiny, "percent jump diverged");
                let e_sync = sync_doc.goto_end(rows).await.unwrap().ready();
                let e_tiny = tiny_doc.goto_end(rows).await.unwrap().join().await;
                prop_assert_eq!(e_sync, e_tiny, "goto end diverged");
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn goto_line_agrees_with_a_naive_reference(
            data in proptest::collection::vec(
                prop_oneof![2 => Just(b'\n'), 8 => any::<u8>()],
                0..192,
            ),
            block_size in 1usize..32,
            budget in 1usize..24,
            n in 0u64..64,
        ) {
            // the reference: 1-based line starts by direct scan, with the
            // vim clamp for n past the end and TOP for an empty file.
            let starts = {
                let mut v = vec![];
                if !data.is_empty() {
                    v.push(0u64);
                }
                for (i, &b) in data.iter().enumerate() {
                    if b == b'\n' && i + 1 < data.len() {
                        v.push(i as u64 + 1);
                    }
                }
                v
            };
            let expected = match starts.len() {
                0 => 0,
                len => {
                    let line0 = (n.saturating_sub(1) as usize).min(len - 1);
                    starts[line0]
                }
            };
            rt().block_on(async {
                // construction spawns the background index scan via
                // tokio::spawn, so it must happen inside the runtime.
                let doc = Document::new(
                    std::sync::Arc::new(MockSource::new(data.clone())),
                    Config { block_size, nav_scan_budget: budget, prefetch_depth: 0, ..Config::default() },
                );
                let a = doc.goto_line(n).await.unwrap().join().await;
                prop_assert_eq!(a.offset(), expected, "line {} in {} lines", n, starts.len());
                Ok::<(), TestCaseError>(())
            })?;
        }
        #[test]
        fn goto_line_agrees_near_checkpoint_boundaries(
            lines in 1016u64..1033,
            delta in -3i64..4,
            terminated in proptest::bool::ANY,
        ) {
            // the checkpoint interval is 1024 lines and the general oracle's
            // byte-capped data can never form a second checkpoint; this
            // property sweeps the boundary neighborhood — including the
            // exactly-aligned trailing-newline phantom shape — with cheap
            // constructed data (no per-element strategy cost). the exact
            // aligned point is also pinned by two deterministic tests; this
            // net covers its neighborhood against variants.
            let mut data = b"x\n".repeat(lines as usize);
            if !terminated {
                data.push(b'y');
            }
            let total = if terminated { lines } else { lines + 1 };
            let n = (lines as i64 + delta).max(0) as u64;
            let line0 = n.saturating_sub(1).min(total - 1);
            let expected = line0 * 2;
            rt().block_on(async {
                // construction spawns the background index scan via
                // tokio::spawn, so it must happen inside the runtime.
                let doc = Document::new(
                    std::sync::Arc::new(MockSource::new(data)),
                    Config { block_size: 64, nav_scan_budget: 64, prefetch_depth: 0, ..Config::default() },
                );
                let a = doc.goto_line(n).await.unwrap().join().await;
                prop_assert_eq!(a.offset(), expected, "line {} of {}", n, total);
                Ok::<(), TestCaseError>(())
            })?;
        }
    }
}
