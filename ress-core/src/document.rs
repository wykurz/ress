//! The document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
/// Re-exported so callers outside the crate (the terminal frontend's status
/// line) can name the type `Document::index_frontier` returns; the `index`
/// module itself stays `pub(crate)`.
pub use crate::index::Frontier;
use crate::resolve::Resolution;
use crate::source::BlockSource;
use std::sync::Arc;
/// The scroll position: the byte offset of the first visible line. Scrolling
/// and rendering are expressed in anchors, never line numbers, so they never
/// wait on indexing; `goto_line` is the deliberate exception, since turning a
/// line number into an anchor needs the background line index and pends on
/// its progress when the query outruns the indexed frontier. Anchors come
/// only from `Anchor::TOP` or `Document` navigation, which keeps the offset a
/// line start: 0, or the byte just past a newline before EOF.
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
/// Panic message for the index-only queries (`goto_line`, `request_line_number`/
/// `line_number`/`status_snapshots`, `index_frontier`) on a document built
/// via `new_unindexed`; those are test-only constructions and never reach
/// these call paths in production.
const NO_INDEX: &str = "goto_line needs the background index (unindexed documents are test-only)";
/// Re-exported so callers outside the crate (the terminal frontend's status
/// line) can name the types `Document::line_number` and `status_snapshots`
/// return without reaching into `crate::status` directly; the `status`
/// module itself stays `pub(crate)`, same treatment as `Frontier` above.
pub use crate::status::{LineNumber, StatusSnapshot};
/// A read-only view over a file's bytes.
pub struct Document {
    cache: Arc<crate::cache::BlockCache>,
    size: u64,
    config: Config,
    prefetcher: crate::prefetch::Prefetcher,
    scheduler: Option<crate::schedule::ScanScheduler>,
    /// The status line's line-number worker; spawned after `scheduler`
    /// since it needs that scan's index and frontier handles. See
    /// `crate::status` for why this is one owned, cancel-on-drop task
    /// rather than a shared memo two call sites could each half-update.
    status: Option<crate::status::StatusWorker>,
}
impl Document {
    /// Must be called within a tokio runtime: construction spawns the
    /// background index scan and the status worker.
    pub fn new(source: Arc<dyn BlockSource>, config: Config) -> Self {
        let size = source.size();
        let cache = Arc::new(crate::cache::BlockCache::new(
            source,
            config.block_size,
            config.cache_bytes,
        ));
        let prefetcher = crate::prefetch::Prefetcher::new(cache.clone(), config.prefetch_depth);
        let scheduler = crate::schedule::ScanScheduler::spawn(cache.clone());
        let status = crate::status::StatusWorker::spawn(
            cache.clone(),
            scheduler.index().clone(),
            scheduler.frontier(),
            config.nav_scan_budget,
        );
        Self {
            cache,
            size,
            config,
            prefetcher,
            scheduler: Some(scheduler),
            status: Some(status),
        }
    }
    /// Test-only: a document with no background index scan and no status
    /// worker, for tests that meter source reads and must not race a
    /// scanner. `goto_line` and the status-line queries panic on such a
    /// document; every other navigation is index-free by design. No tokio
    /// runtime is required to construct one — unlike `new`, this spawns
    /// nothing. Also compiled under the `bench-internals` feature (and made
    /// `pub` rather than `pub(crate)`, since a `[[bench]]` target is a
    /// separate crate that cannot see crate-private items regardless of
    /// cfg) so criterion benches can measure the cold, index-free walk
    /// deterministically.
    #[cfg(any(test, feature = "bench-internals"))]
    pub fn new_unindexed(source: Arc<dyn BlockSource>, config: Config) -> Self {
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
            scheduler: None,
            status: None,
        }
    }
    /// Aborts every background owner this document holds (the prefetcher,
    /// the index scan, and the status worker) and awaits each one's own
    /// teardown to finish, rather than the fire-and-forget abort-REQUEST a
    /// bare `drop` alone provides (each owner's `TaskOwner`/`JoinSet` aborts
    /// on drop, but does not wait for that abort to actually take effect).
    /// Measured, not assumed (U-bench, finding 4): in this crate's own 64
    /// MiB / 1 MiB-block fixture, `abort_and_join` on the index scan alone
    /// (which starts scanning immediately and unconditionally at
    /// construction, so it can be many blocks into an unthrottled pass by
    /// the time a single `.viewport()` call returns) measured in the
    /// hundreds of microseconds, and the status worker's in the tens to
    /// low hundreds — both dwarfing `first_paint`'s own tens-of-microseconds
    /// timed span at `latency_0ms`; the prefetcher's own teardown measured
    /// far smaller (tens of microseconds) but is included for the same
    /// reason. A criterion bench that builds a fresh `Document` every
    /// iteration needs all three awaited explicitly, OUTSIDE the timed
    /// region, before the next iteration starts — otherwise the previous
    /// iteration's own still-unwinding teardown keeps contending for the
    /// same runtime worker threads the next iteration's timed work needs.
    /// Bench-visible (test + bench-internals, matching `new_unindexed`'s
    /// own precedent — a `[[bench]]` target is a separate crate that cannot
    /// see crate-private items regardless of cfg, so this one specifically
    /// needs `pub`, unlike the three owner-level methods it calls).
    #[cfg(any(test, feature = "bench-internals"))]
    pub async fn abort_background_and_join(&mut self) {
        self.prefetcher.abort_and_join().await;
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.abort_and_join().await;
        }
        if let Some(status) = self.status.as_mut() {
            status.abort_and_join().await;
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
        let scheduler = self.scheduler.as_ref().expect(NO_INDEX);
        let line0 = n.saturating_sub(1);
        let budget = self.config.nav_scan_budget;
        let (covered, clamp0, cp) = {
            let ix = scheduler.index().lock().unwrap();
            let covered = ix.newlines() >= line0;
            let clamp0 = ix.total_lines().map(|t| t.saturating_sub(1));
            (
                covered,
                clamp0,
                ix.nearest_checkpoint(line0.min(ix.newlines())),
            )
        };
        if covered {
            let (line0, cp) = dephantom(scheduler.index(), self.size, line0, cp);
            return self.walk_to_line(line0, cp, budget).await;
        }
        if let Some(clamp0) = clamp0 {
            // the scan is done and the line does not exist: vim clamps to
            // the last real line (an empty file has none — the top).
            if self.size == 0 {
                return Ok(Resolution::Ready(Anchor::TOP));
            }
            let cp = scheduler.index().lock().unwrap().nearest_checkpoint(clamp0);
            return self.walk_to_line(clamp0, cp, budget).await;
        }
        let cache = self.cache.clone();
        let index = scheduler.index().clone();
        let mut frontier = scheduler.frontier();
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
                let (line0, cp) = dephantom(&index, size, line0, cp);
                let (cp_off, cp_line0) = cp;
                let scan =
                    crate::scan::ForwardScan::new(cp_off, (line0 - cp_line0) as usize, budget);
                // the tail walk is its own phase, like goto_end's walk-up.
                let span2 = size - cp_off;
                scan.complete(&cache, tx, span2).await.map(Anchor)
            },
        )))
    }
    /// Asks the status worker for `a`'s line number; the answer arrives
    /// later, through `line_number`/`status_snapshots` — this never blocks
    /// and never fails, not even without a tokio runtime bound to the
    /// calling thread, since it is nothing more than a `watch` send. A
    /// request for an anchor the worker is already resolving supersedes it
    /// outright; see `crate::status` for the worker's own life.
    pub fn request_line_number(&self, a: Anchor) {
        self.status
            .as_ref()
            .expect(NO_INDEX)
            .request_line_number(a.offset());
    }
    /// The status worker's latest snapshot. Stale the moment the anchor
    /// moves until a fresh `request_line_number` lands and the worker
    /// answers it — callers compare `StatusSnapshot::anchor` against what
    /// they actually want before trusting `line`.
    pub fn line_number(&self) -> StatusSnapshot {
        self.status.as_ref().expect(NO_INDEX).line_number()
    }
    /// A fresh subscription to the status worker's snapshots, for the run
    /// loop's repaint arm — the status-line twin of `index_frontier`.
    pub fn status_snapshots(&self) -> tokio::sync::watch::Receiver<StatusSnapshot> {
        self.status.as_ref().expect(NO_INDEX).status_snapshots()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many times the status
    /// worker's own loop has begun a fresh resolution attempt -- see
    /// `crate::status::StatusWorker::resolution_attempts_events`'s own doc comment for why this
    /// exists.
    #[cfg(test)]
    pub(crate) fn resolution_attempts_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.status
            .as_ref()
            .expect(NO_INDEX)
            .resolution_attempts_events()
    }
    /// A subscription to the background index's progress, for repaint-driven
    /// consumers like the status line.
    pub fn index_frontier(&self) -> tokio::sync::watch::Receiver<crate::index::Frontier> {
        self.scheduler.as_ref().expect(NO_INDEX).frontier()
    }
    /// The file's total line count once the background index has finished
    /// AND reached EOF — the status line's `L{n}/{total}` display. `None`
    /// while indexing continues, and permanently `None` after an
    /// error-shortened scan (a read error stopped it before EOF): a
    /// partial prefix count is a real, useful number — `LineIndex::
    /// total_lines` still returns it, and `goto_line`'s clamp still reads
    /// that directly — but it is not the file's total, and displaying it
    /// as one would claim an exactness the scan never earned.
    pub fn index_total_lines(&self) -> Option<u64> {
        let scheduler = self.scheduler.as_ref().expect(NO_INDEX);
        let ix = scheduler.index().lock().unwrap();
        if !ix.reached_eof() {
            return None;
        }
        ix.total_lines()
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
/// Steps a resolved (line0, checkpoint) pair off the at-EOF phantom: a
/// trailing newline's "line start" at the file size is not a line, so the
/// real last line is the one before it. A no-op for every in-file
/// checkpoint. Steps back with `saturating_sub`, not plain subtraction:
/// `line0 == 0` reaches this guard too (an empty file trivially satisfies
/// the caller's "covered" check at line 0), and must step back to 0 rather
/// than underflow.
fn dephantom(
    index: &std::sync::Mutex<crate::index::LineIndex>,
    size: u64,
    line0: u64,
    cp: (u64, u64),
) -> (u64, (u64, u64)) {
    if cp.0 < size {
        return (line0, cp);
    }
    let prev = line0.saturating_sub(1);
    (prev, index.lock().unwrap().nearest_checkpoint(prev))
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
    use crate::source::{MockSource, wait_for_count};
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
        let d = Document::new_unindexed(
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
        let d = Document::new_unindexed(
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
        let d = Document::new_unindexed(
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
        let d = Document::new_unindexed(
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
        // no injected latency: nav_scan_budget (256 bytes) forces Pending regardless, and
        // `p.cancel()` below accepts EITHER race outcome (`let _ = p.handle.await` -- an aborted
        // join error is fine) -- nothing here depends on the read still being in flight at
        // cancel time (U-delete).
        let src = Arc::new(MockSource::new({
            let mut data = vec![b'a'; 8192];
            data.push(b'\n');
            data
        }));
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
        // n=7 takes the uncovered clamp shortcut; n<=1 takes the covered
        // branch straight into dephantom — the shape whose underflow
        // saturating_sub prevents.
        let d = doc(b"", 64);
        assert_eq!(d.goto_line(7).await.unwrap().join().await, Anchor::TOP);
        assert_eq!(d.goto_line(1).await.unwrap().join().await, Anchor::TOP);
        assert_eq!(d.goto_line(0).await.unwrap().join().await, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_line_beyond_the_frontier_pends_with_progress() {
        // an armed gate holds the background scan's very first read open, so it provably
        // cannot have advanced the index AT ALL by the time goto_line is asked below --
        // deterministic, not "2ms per block is probably enough to stay behind" (U-delete).
        // Stated honestly, not overclaimed: `Document::new` is fully synchronous and
        // `goto_line`'s own Pending decision (document.rs, above) never awaits either, so
        // TODAY there is in fact no yield point at all between construction and the query --
        // the scan's task cannot get a single poll in either way, gate or not (confirmed: 20/20
        // clean with the gate deliberately left unarmed). The gate is not closing an observed
        // flake here, it is removing reliance on that being true FOREVER -- nothing in either
        // function's own contract promises no yield point will ever appear between them, and a
        // future fairness yield (`StatusWorker::run`'s own count-step loop already has one, for
        // exactly this reason) would silently make this test racy again. Reproduced directly:
        // temporarily inserted 2000 `yield_now`s here with the gate left unarmed, simulating
        // exactly that -- the scan raced ahead and finished, flipping this to
        // `Ready(34990)` and hitting the panic below. Reverted after confirming.
        // Safe for the QUERY's own path too: `goto_line`'s Pending decision reads only the
        // in-memory index Mutex, never the source, so gating the source cannot block the query
        // itself from returning -- only the background scan it races against.
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        src.arm_gate();
        match d.goto_line(3500).await.unwrap() {
            Resolution::Pending(mut p) => {
                assert_eq!(p.label, "jumping to line");
                let first = *p.progress.borrow();
                // the discriminator: reaching this arm at all proves the scan was still
                // uncovered when asked -- a scan that could race ahead (the risk the fixed
                // latency only probabilistically avoided) would resolve Ready instead, hitting
                // the panic in that arm below. Opened only now, having already proven Pending:
                // the walk below needs real reads to actually resolve.
                src.open_gate();
                let a = (&mut p.handle).await.unwrap().unwrap();
                assert_eq!(a, Anchor::at(34990));
                let last = *p.progress.borrow();
                assert!(
                    last.scanned > first.scanned,
                    "a pending line jump must publish frontier progress"
                );
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
        // same shape, but the query races the background scan so resolution goes
        // through the pending closure's own coverage check -- unlike the
        // neighbor above, this used to resolve via `Resolution::join`, which
        // accepts EITHER variant transparently (see its own doc comment,
        // resolve.rs): a future change that made this resolve synchronously
        // as `Ready` instead -- skipping the pending path's own coverage
        // check entirely, the exact mechanism this test claims to exercise
        // -- would have kept passing, silently testing nothing about it.
        //
        // an armed gate, not a fixed latency (U-delete): same mechanism and same
        // "stated honestly" caveat as `goto_line_beyond_the_frontier_pends_with_progress`'s own
        // comment, above -- today there is no yield point between `Document::new` and this
        // query either, so the gate isn't closing an observed flake, it's removing reliance on
        // that staying true forever. Arm immediately, before any await, for the identical
        // reason: `Document::new` is synchronous and a freshly spawned task cannot be polled
        // until this task yields, so the scan cannot have read anything yet regardless.
        let src = Arc::new(MockSource::new(b"x\n".repeat(1024)).with_gate());
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        src.arm_gate();
        match d.goto_line(1025).await.unwrap() {
            Resolution::Pending(p) => {
                src.open_gate();
                assert_eq!(p.handle.await.unwrap().unwrap(), Anchor::at(2046));
            }
            Resolution::Ready(a) => panic!(
                "expected Pending -- the whole point of this test is the pending path's own \
                 coverage check -- got Ready({})",
                a.offset()
            ),
        }
    }
    #[tokio::test]
    async fn goto_line_phantom_clamp_resolves_synchronously_once_indexed() {
        // joining a past-the-end jump waits for the scan to finish (its
        // clamp needs total_lines), so the second ask is answered from the
        // covered branch — pinning the sync-side dephantom deterministically
        // (the pending twin is separately pinned; a cold index would always
        // route this shape through it).
        let d = lined("x\n", 1024, 64);
        assert_eq!(
            d.goto_line(4000).await.unwrap().join().await,
            Anchor::at(2046)
        );
        match d.goto_line(1025).await.unwrap() {
            Resolution::Ready(a) => assert_eq!(a, Anchor::at(2046)),
            Resolution::Pending(_) => panic!("index is done; the ask must resolve synchronously"),
        }
    }
    /// Waits until the status worker publishes a snapshot for `anchor`
    /// satisfying `pred`, without sending a request itself — for tests that
    /// need control over exactly when (or whether) `request_line_number` is
    /// called relative to the wait, mirroring schedule.rs's `wait_done`.
    async fn wait_for_line_number(
        d: &Document,
        anchor: u64,
        pred: impl Fn(LineNumber) -> bool,
    ) -> LineNumber {
        let mut rx = d.status_snapshots();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snap = *rx.borrow();
                if snap.anchor == anchor && pred(snap.line) {
                    return snap.line;
                }
                rx.changed()
                    .await
                    .expect("status worker ended before resolving");
            }
        })
        .await
        .expect("status worker never reached the expected state")
    }
    /// Requests `a`'s line number and waits for it to settle — `Known` or
    /// `Unavailable`, never left mid-`Converging` — the common case for
    /// tests that only care about the final answer.
    async fn resolved_line_number(d: &Document, a: Anchor) -> LineNumber {
        d.request_line_number(a);
        wait_for_line_number(d, a.offset(), |l| !matches!(l, LineNumber::Converging)).await
    }
    #[tokio::test]
    async fn line_number_resolves_after_the_frontier_passes() {
        let d = lined_doc(3000, 256);
        // structural done-wait: a past-the-end jump joins only once the
        // scan is finished.
        let _ = d.goto_line(9999).await.unwrap().join().await;
        assert_eq!(d.index_total_lines(), Some(3000));
        assert_eq!(
            resolved_line_number(&d, Anchor::TOP).await,
            LineNumber::Known(1)
        );
        let a = d.goto_line(2000).await.unwrap().join().await;
        assert_eq!(resolved_line_number(&d, a).await, LineNumber::Known(2000));
    }
    #[tokio::test]
    async fn index_frontier_observes_done() {
        // the run loop's repaint arm subscribes once via `index_frontier()`
        // and polls `changed()` until `done`, disarming there — this pins
        // that the eventually-done contract holds through the public
        // accessor itself, not just the `ScanScheduler` it wraps (already
        // covered in schedule.rs).
        let d = lined_doc(3000, 256);
        let mut frontier = d.index_frontier();
        while !frontier.borrow().done {
            frontier
                .changed()
                .await
                .expect("index scan ended without ever sending a done frontier");
        }
        assert_eq!(frontier.borrow().lines_so_far, 3000);
    }
    #[tokio::test]
    async fn line_number_converges_while_uncovered_and_the_scan_is_alive() {
        // audit finding 1b (status.rs sibling: `stays_converging_while_the_scan_is_alive_and_
        // uncovered`, whose own doc comment explains the discrimination in full): the status
        // worker publishes Converging from TWO sites, one coverage-gated and one unconditional,
        // so a test that only checks the published VALUE cannot tell a genuinely gated wait from
        // an already-(mis-)covered anchor whose count-scan just also happens to publish
        // Converging on its own way in. Gating block 0 forever (rather than betting a fixed
        // latency keeps a live scan behind) makes coverage permanently, structurally
        // unsatisfied; `BlockCache::coalesced_events` (pass 8, U-cache), read through
        // `Document::cache` (already `pub(crate)`, used by the prefetcher), proves the status
        // worker's own count-scan never even attempted a read -- see the status.rs sibling for
        // why `in_flight_len` cannot serve this role, and why checkpoint `(0, 0)` is exactly
        // where a wrongly-triggered count-scan would collide with the gated block.
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let src = Arc::new(MockSource::new(data).with_gate());
        src.arm_gate();
        let d = Document::new(
            src,
            Config {
                block_size: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        // captured BEFORE any request at all -- see the status.rs sibling's identical comment for
        // the full reasoning (p6-review2, pass 8 U-worker review, point 1): the real risk is any
        // yield before this capture, not specifically the worker's own default anchor-0 pass.
        let coalesced = d.cache().coalesced_events();
        let coalesced_baseline = *coalesced.borrow();
        d.request_line_number(Anchor::at(2000)); // deep into the file; unreachable while block 0 is parked.
        let line = wait_for_line_number(&d, 2000, |_| true).await;
        assert_eq!(line, LineNumber::Converging);
        // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
        // 2026-07-22) -- direct sibling of status.rs's own marked site: this test gates block 0
        // forever (see its own doc comment above), so the worker is parked on a coverage wait
        // that can never resolve and no positive "will never read" signal can exist here either;
        // the discriminator remains the coalesced_events baseline below.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *coalesced.borrow(),
            coalesced_baseline,
            "the status worker's own count-scan must never even attempt a read while the \
             anchor is genuinely uncovered"
        );
        assert_eq!(
            d.line_number().line,
            LineNumber::Converging,
            "a genuinely uncovered anchor must never progress to Known while parked"
        );
    }
    #[tokio::test]
    async fn line_number_converges_across_many_budget_chunks() {
        // one giant 8192-byte line then short lines; budget 256: the count
        // from checkpoint 0 to an anchor past the giant line cannot finish
        // in one internal step. Unlike the old design, convergence here is
        // the worker's OWN loop resuming its CountScan across many chunks
        // in the background — not something the caller drives call by
        // call — so there is no caller-visible "number of tries" left to
        // pin the way the pre-redesign version of this test did; every
        // block in the window is already resident (the goto_line(9999)
        // done-wait below reads the whole file), so convergence here is
        // pure CPU across the worker's `yield_now` points, not I/O.
        let mut data = vec![b'x'; 8192];
        data.push(b'\n');
        for _ in 0..10 {
            data.extend_from_slice(b"y\n");
        }
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
        let _ = d.goto_line(9999).await.unwrap().join().await;
        // offset 8193 is the first "y" line's start: the giant line spans
        // [0, 8193) (8192 x's plus its newline at 8192).
        let a = Anchor::at(8193);
        assert_eq!(resolved_line_number(&d, a).await, LineNumber::Known(2));
        // the resolved number stays published: an immediate re-read
        // answers from the same snapshot without re-driving the scan.
        assert_eq!(d.line_number().line, LineNumber::Known(2));
    }
    #[tokio::test]
    async fn line_number_on_an_empty_file_is_line_one() {
        let d = doc(b"", 64);
        let _ = d.goto_line(9999).await.unwrap().join().await;
        assert_eq!(
            resolved_line_number(&d, Anchor::TOP).await,
            LineNumber::Known(1)
        );
    }
    #[tokio::test]
    async fn line_number_stays_unavailable_past_a_dead_scan_frontier() {
        // block 0 reads fine; every block from the second on fails on
        // every attempt, not just the first — mirrors schedule.rs's
        // error_shortened_scan_leaves_a_done_frontier_counting_the_frontier_line,
        // but a persistent failure so a retry cannot mask the bug by
        // succeeding on a second attempt at the same block.
        struct FailsAfterFirstBlock;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsAfterFirstBlock {
            fn size(&self) -> u64 {
                20
            }
            async fn read_block(&self, offset: u64, _len: usize) -> anyhow::Result<bytes::Bytes> {
                if offset == 0 {
                    Ok(bytes::Bytes::from_static(b"a\nb\n"))
                } else {
                    Err(anyhow::anyhow!("boom"))
                }
            }
        }
        let d = Document::new(
            Arc::new(FailsAfterFirstBlock),
            Config {
                block_size: 4,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let mut frontier = d.index_frontier();
        while !frontier.borrow().done {
            frontier
                .changed()
                .await
                .expect("index scan ended without ever sending a done frontier");
        }
        assert_eq!(
            d.index_total_lines(),
            None,
            "an error-shortened scan's partial count must not display as the file's total"
        );
        // below the dead frontier (processed_up_to == 4): the count stays
        // inside the successfully-scanned prefix and must resolve.
        assert_eq!(
            resolved_line_number(&d, Anchor::at(2)).await,
            LineNumber::Known(2)
        );
        // past the dead frontier: coverage must never be granted by `done`
        // alone, so this anchor stays Unavailable forever — never a wrong
        // number, never an error surfaced from the dead scan's old read
        // failure, since processed_up_to never moves again once the scan
        // has died. Structurally guaranteed, not just observed: the
        // coverage gate never touches the cache at all (only the index
        // lock), so an anchor that never passes it can never reach a read.
        assert_eq!(
            resolved_line_number(&d, Anchor::at(8)).await,
            LineNumber::Unavailable
        );
    }
    /// An 8-line, 8-byte-per-line fixture over a 4-block cache (block_size
    /// 8, cache_bytes 32: probation capacity 1) whose background scan —
    /// warming sequentially through `warm()`, never promoting — evicts
    /// every block behind itself, so by the time the index is done only
    /// the last-scanned block remains resident. Anchor 8 (the second
    /// line's start) then needs block 0, which the scan read successfully
    /// once but which is now cold: the shared setup the worker tests below
    /// build on (retry exhaustion, runtime-independence, supersession
    /// under a slow re-fetch). Waits on the frontier directly rather than
    /// done-waiting through a `goto_line` join: the latter's own clamped
    /// walk would read block 0 a second time as part of resolving its
    /// answer, double-counting reads a source that fails from the second
    /// read on (see the retry-exhaustion test below) would not survive.
    async fn cold_block_zero_doc(src: Arc<dyn BlockSource>) -> (Document, Anchor) {
        let d = Document::new(
            src,
            Config {
                block_size: 8,
                cache_bytes: 32,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let mut frontier = d.index_frontier();
        while !frontier.borrow().done {
            frontier
                .changed()
                .await
                .expect("index scan ended without ever sending a done frontier");
        }
        (d, Anchor::at(8))
    }
    #[tokio::test]
    async fn line_number_converges_despite_viewport_eviction_pressure() {
        // finding 2's shape (round 3): `draw` reads the viewport first
        // (filling and evicting the shared cache) and the status query
        // second, every frame; on a cache too small to hold both, the
        // worker's own walk must keep making progress even though the
        // viewport keeps evicting blocks behind it. Unlike the old
        // design's managed-fetch pinning, there is nothing to pin here:
        // the worker retains its own `CountScan` cursor across internal
        // steps and consumes each block's bytes the instant it is warmed,
        // within that same step — so it only ever needs the one block
        // currently in hand, and a viewport read racing in between can
        // never evict a block the walk still needs. The two reads are on
        // disjoint blocks, matching `draw`'s real shape (the count's
        // window sits strictly before the anchor; the viewport reads
        // strictly at and after it).
        let data = "aaaaaaa\n".repeat(4).into_bytes(); // four 8-byte lines
        let src = Arc::new(MockSource::new(data));
        let d = Document::new(
            src,
            Config {
                block_size: 8,
                cache_bytes: 16, // 2 blocks: 1 probationary, 1 protected
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let _ = d.goto_line(9999).await.unwrap().join().await;
        // the third line's start: the count's window [0, 16) needs blocks
        // 0 and 1; the viewport reading forward from here needs blocks 2
        // and 3 — disjoint from what the count needs.
        let a = Anchor::at(16);
        d.request_line_number(a);
        let rx = d.status_snapshots();
        let mut tries = 0usize;
        let n = loop {
            tries += 1;
            assert!(tries < 500, "eviction livelock: query failed to converge");
            let _ = d.viewport(a, 5, 80, HScroll::ZERO).await.unwrap();
            let snap = *rx.borrow();
            if snap.anchor != a.offset() {
                tokio::task::yield_now().await;
                continue;
            }
            match snap.line {
                LineNumber::Known(n) => break n,
                LineNumber::Converging => tokio::task::yield_now().await,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(n, 3);
    }
    #[tokio::test]
    async fn line_number_settles_on_the_latest_anchor_despite_rapid_requests() {
        // finding 1's shape (round 3), inverted for the new design:
        // spamming `request_line_number` for ten different anchors with no
        // yields in between must not spawn ten walks — the worker can
        // only ever act on the LAST anchor it was told about once it is
        // next scheduled, so nothing before that point can have read
        // anything. This is a structural guarantee, not a probabilistic
        // one: no `.await` happens between the first send and the
        // read-count check below, and a spawned task cannot run at all
        // until this task yields — pileup is impossible to observe here,
        // not just unlikely.
        let data = "aaaaaaa\n".repeat(8).into_bytes();
        // no injected latency: this test's own comment already states the pileup-avoidance
        // property is structural (no await between send and check), and `cold_block_zero_doc`
        // below gets its "block 0 is cold" guarantee from cache_bytes being too small to hold
        // the whole 8-block scan, not from timing (U-delete).
        let src = Arc::new(MockSource::new(data));
        let (d, _) = cold_block_zero_doc(src.clone()).await;
        let before = src.read_count();
        for i in 0..10u64 {
            d.request_line_number(Anchor::at(i));
        }
        let settled = Anchor::at(8); // second line's start; needs the cold block 0.
        d.request_line_number(settled);
        assert_eq!(
            src.read_count(),
            before,
            "the worker ran before it could possibly have been scheduled"
        );
        let line = wait_for_line_number(&d, settled.offset(), |l| {
            !matches!(l, LineNumber::Converging)
        })
        .await;
        assert_eq!(line, LineNumber::Known(2));
    }
    #[tokio::test]
    async fn line_number_stalls_at_unavailable_after_exhausting_retries() {
        // findings 1 and 2's shared failure mode (round 3), pushed to its
        // limit: a block that reads fine once (so the background scan
        // covers past it) but fails every time it is re-fetched (a
        // network mount that dropped after indexing) must retry a bounded
        // number of times, then give up — permanently. The worker parks
        // on the anchor channel after publishing `Unavailable`, so unlike
        // the old per-call design there is no "next call" to keep
        // retrying: read activity on the source must freeze too.
        struct FailsOnRereadOfBlockZero {
            data: bytes::Bytes,
            block0_reads: std::sync::atomic::AtomicU64,
            reads: std::sync::atomic::AtomicU64,
        }
        #[async_trait::async_trait]
        impl BlockSource for FailsOnRereadOfBlockZero {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<bytes::Bytes> {
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if offset == 0
                    && self
                        .block0_reads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        > 0
                {
                    return Err(anyhow::anyhow!("block 0 unreachable"));
                }
                let size = self.data.len() as u64;
                let start = offset.min(size) as usize;
                let end = offset.saturating_add(len as u64).min(size) as usize;
                Ok(self.data.slice(start..end))
            }
        }
        let src = Arc::new(FailsOnRereadOfBlockZero {
            data: bytes::Bytes::from("aaaaaaa\n".repeat(8)),
            block0_reads: std::sync::atomic::AtomicU64::new(0),
            reads: std::sync::atomic::AtomicU64::new(0),
        });
        let (d, a) = cold_block_zero_doc(src.clone()).await;
        let attempts = d.resolution_attempts_events();
        let line = resolved_line_number(&d, a).await;
        assert_eq!(line, LineNumber::Unavailable, "retries must exhaust");
        let frozen_at = src.reads.load(std::sync::atomic::Ordering::Relaxed);
        let attempts_at_stall = *attempts.borrow();
        // POSITIVE proof, not a bounded silence window -- but stated precisely about what
        // discriminates what here (p6-review2, pass 8 U-worker review, point 2): in THIS test's
        // own construction (`FailsOnRereadOfBlockZero` over a deliberately undersized cache,
        // making every walk attempt a genuine cold read), the read-count assertion below is what
        // actually does the discriminating work -- any re-kick, however caused, needs a real
        // source read to advance a walk, so `src.reads` alone already catches it. `resolution_
        // attempts` (see its own doc comment, status.rs) is kept as a secondary, defense-in-depth
        // check: it counts loop *iterations* regardless of whether a given one hits the cache or
        // the real source, so it would ALSO catch a subtler class this specific test does not
        // happen to exercise -- a re-kick landing on an already-warm/cached block, needing no
        // fresh read at all, which `src.reads` structurally cannot see. Unlike a downstream read
        // count, this signal fires the instant the loop re-enters, before any retry backoff sleep
        // would even begin, so a few cooperative yields (not real or virtual time) suffice to
        // give either class of bug a genuine chance to run before the assertions below.
        // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
        // 2026-07-22) -- once retries are exhausted the worker parks silently, exposing no
        // "retrying is now permanently over" signal to await, so these yields only give a
        // hypothetical re-kick room to run before the src.reads/resolution_attempts baselines
        // below do the actual discriminating work this test's own doc comment above describes.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *attempts.borrow(),
            attempts_at_stall,
            "worker kept retrying after exhausting its budget"
        );
        assert_eq!(
            src.reads.load(std::sync::atomic::Ordering::Relaxed),
            frozen_at,
            "worker kept retrying after exhausting its budget"
        );
    }
    #[tokio::test]
    async fn line_number_request_and_read_need_no_runtime() {
        // finding 3's shape (round 3), restated for the new API: the old
        // design's bare `tokio::spawn` inside a sync `line_number_at`
        // needed a runtime bound to the calling thread and panicked
        // without one. `request_line_number`/`line_number` need nothing of
        // the sort — they are a `watch` send and a `watch` borrow,
        // thread-safe and allocation-free by construction, so a plain OS
        // thread with no tokio runtime at all can drive both without
        // panicking. The worker itself keeps running on the runtime that
        // spawned it, wholly independent of whatever thread calls these
        // two methods, and still resolves a request sent from the
        // runtime-free thread once given a chance to run.
        let data = "aaaaaaa\n".repeat(8).into_bytes();
        let src = Arc::new(MockSource::new(data));
        let (d, a) = cold_block_zero_doc(src).await;
        let from_plain_thread = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    d.request_line_number(a);
                    d.line_number()
                })
                .join()
        });
        assert!(
            from_plain_thread.is_ok(),
            "request_line_number/line_number must not panic without a runtime"
        );
        let line =
            wait_for_line_number(&d, a.offset(), |l| !matches!(l, LineNumber::Converging)).await;
        assert_eq!(
            line,
            LineNumber::Known(2),
            "the worker still resolves a request sent from a runtime-free thread"
        );
    }
    #[tokio::test]
    async fn line_number_is_converging_for_a_covered_but_unresolved_count() {
        // one of the four corner semantics `status_converging` used to
        // pin at the render layer, now re-pinned engine-side: a budget too
        // small to finish counting from the checkpoint to the anchor in
        // one internal step is covered, but not yet resolved. A trailing
        // line past the anchor is required for coverage itself: coverage
        // needs `processed_up_to` strictly past the anchor, which a
        // done-waited scan can never be true of when the anchor sits
        // exactly at EOF. Every block in the window is warmed into cache
        // first (the done-waited background scan reads the whole file),
        // so the walk's own 33 chunked steps are back-to-back cache hits —
        // each still followed by an explicit yield (see `crate::status`),
        // giving this test's own task ample opportunity to observe the
        // `Converging` snapshot before the walk finishes.
        let mut data = vec![b'x'; 8192];
        data.push(b'\n');
        data.extend_from_slice(b"y\n");
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
        let _ = d.goto_line(9999).await.unwrap().join().await;
        let a = Anchor::at(8193);
        let mut rx = d.status_snapshots();
        rx.borrow_and_update(); // a clean baseline before requesting.
        d.request_line_number(a);
        let mut saw_converging = false;
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snap = *rx.borrow();
                if snap.anchor == a.offset() {
                    match snap.line {
                        LineNumber::Converging => saw_converging = true,
                        other @ (LineNumber::Known(_) | LineNumber::Unavailable) => return other,
                    }
                }
                rx.changed()
                    .await
                    .expect("status worker ended before resolving");
            }
        })
        .await
        .expect("status worker never resolved");
        assert!(
            saw_converging,
            "a multi-step walk must publish an intermediate Converging snapshot"
        );
        assert_eq!(line, LineNumber::Known(2));
    }
    #[tokio::test]
    async fn line_number_is_known_once_resolved() {
        // the second of the four corner semantics: a count that finishes
        // within its budget resolves to the real line number.
        let d = lined_doc(10, 64);
        let a = d.goto_line(5).await.unwrap().join().await;
        assert_eq!(resolved_line_number(&d, a).await, LineNumber::Known(5));
    }
    #[tokio::test]
    async fn dropping_the_document_stops_the_status_worker() {
        // the round-4 Arc-ownership-cycle bug this redesign replaces,
        // inverted as a regression test: the old design's fetch task
        // strongly owned the very Arc holding its own abort handle, so a
        // hung fetch survived dropping the Document entirely — a leaked
        // background reader with nothing left to cancel it. The new
        // worker holds only cache/index/channel handles, never a handle
        // to itself, so Document's own drop (which drops the worker,
        // whose `Drop` impl aborts its task) is sufficient on its own.
        //
        // found in PR #44 pass 6 S4 (#8, a user-review finding): this test proves Document's own
        // drop genuinely reaches all the way through to the worker being torn down (the
        // Arc-cycle regression above) -- it does NOT, on its own, discriminate whether `.abort()`
        // specifically is what ends the worker's task, since dropping the worker ALSO drops its
        // `anchor_tx`, and `status.rs`'s own `run` loop already exits on its own once that
        // channel closes, whichever happens. Confirmed directly, not assumed: the identical
        // confound is documented on `status.rs`'s own `dropping_the_worker_aborts_its_in_flight_
        // step` (pass 7, P7-C rewrote that test to prove `.abort()`'s own contribution through
        // `StatusWorker`'s REAL `Drop`, holding a cloned anchor sender alive throughout to isolate
        // it from channel-close -- see its own doc comment for the full account) -- that is the
        // test that isolates `.abort()`'s own contribution unconfounded. This test's own value
        // stands regardless: Document's drop reaching the worker at all (not leaking it via the
        // old Arc cycle) is exactly what it was written to pin, whichever mechanism the worker's
        // own teardown then uses.
        //
        // found in PR #44 pass 7 (P7-C, a p6-review2 ruling, SUPERSEDED below): this test used to
        // stay silence-based rather than converting to a positive `Cancelled`-event wait the way
        // `status.rs`'s own test was, on the reasoning that this test's whole point is the
        // Arc-ownership-cycle regression above -- an INTEGRATION-level claim that `Document`'s own
        // drop cascade reaches all the way through to the worker being torn down at all, not
        // `StatusWorker`'s own `Drop` mechanism in isolation (already covered directly, and more
        // precisely, by `dropping_the_worker_aborts_its_in_flight_step`, status.rs) -- and a
        // LEAKED worker (this test's own regression case) would keep reading, so silence alone
        // would still catch it without needing to re-prove the SAME mechanism that other test
        // already isolates.
        //
        // found in PR #44 pass 7's structural pass, re-review (codex P2): that premise no longer
        // holds, once the gate below (added for the scheduling-race fix) entered the picture. A
        // gated read PARKS instead of completing or retrying -- so a LEAKED worker (never torn
        // down at all) also reads nothing further: it just sits there, parked, forever. Both
        // silence checks this test used to make -- a `started.changed()` timeout, `read_count()
        // == mid` holding -- passed IDENTICALLY whether the worker was genuinely torn down OR
        // leaked-and-parked, unable to tell the two apart anymore (RED-verified directly: took
        // `d`'s own `status` field out via `.take()` and `std::mem::forget`-ed it before the drop,
        // simulating the original round-4 leak -- the OLD silence assertions passed anyway).
        // Ruling revised: this test now waits for a POSITIVE `Cancelled` event on the one gated
        // read, the same mechanism `status.rs`'s own test uses. The DISTINCTION between the two
        // tests still holds exactly as before -- this one proves the DROP CASCADE reaches the
        // worker at all; `status.rs`'s proves `StatusWorker`'s OWN `Drop` in isolation, anchor-tx
        // held alive throughout -- only both now prove their own claim through a positive event
        // rather than one of the two doing it through silence. Against the fix, the same
        // simulated leak correctly fails (`wait_for_count`'s own 5s timeout, loud, not silent).
        // Both reverted immediately after confirming.
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        // a gate, disarmed until armed below -- not a fixed latency (codex P2, PR #44 pass 7's
        // structural pass, a 3rd re-review, the shared root of 3 findings at once, this one an
        // audit find rather than one of the 3 codex flagged directly: identical shape to
        // `status.rs`'s own `dropping_the_worker_aborts_its_in_flight_step`, including the SAME
        // risk -- a fixed-latency read can complete on its own before `drop(d)` below,
        // independent of this test's own scheduling). Unlike that test, nothing here reads
        // through `src` again after the drop (the check below is a positive `cancelled_events`
        // wait, which only watches a channel, never a fresh read), so there is no later,
        // legitimate read to protect from the gate staying armed -- no `open_gate` call needed
        // anywhere in this test.
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 64,
                cache_bytes: 4 * 64,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let mut frontier = d.index_frontier();
        while !frontier.borrow().done {
            frontier
                .changed()
                .await
                .expect("index scan ended without ever sending a done frontier");
        }
        src.arm_gate();
        // found in PR #44 round 17 (a codex P2, sweep): this test used to sleep 30ms and expect
        // that to have been enough real time for the walk to have started re-reading -- see
        // status.rs's own `dropping_the_worker_aborts_its_in_flight_step` (this crate's own
        // tests, identical shape, including the baseline-before-the-request derivation) for why.
        // The baseline is captured before the request: the index scan above already performed
        // its own reads, so waiting for merely "n >= 1" resolves trivially off THOSE, never
        // actually confirming the WALK toward 4000 had started at all.
        //
        // found in PR #44 pass 6 S3 (codex, "bound the read-start handshake waits with a
        // timeout"): that `wait_for` used to be raw and unbounded -- see
        // `wait_for_count`'s own doc comment (source.rs) for why.
        let mut started = src.started_events();
        let baseline = *started.borrow();
        d.request_line_number(Anchor::at(4000));
        wait_for_count(&mut started, |n| n > baseline).await;
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        drop(d);
        // POSITIVE proof, pass 7's structural pass (superseding the P7-C silence ruling above):
        // the drop cascade reached all the way through to the gated read's own future being torn
        // down, not left parked forever by a leaked worker. See this test's own comment above for
        // why silence stopped being able to tell the two apart once the gate landed.
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
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
                let doc = Document::new_unindexed(
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
