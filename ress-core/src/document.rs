//! The document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
/// Re-exported so callers outside the crate (the terminal frontend's status
/// line) can name the type `Document::index_frontier` returns; the `index`
/// module itself stays `pub(crate)`.
pub use crate::index::Frontier;
use crate::resolve::{NavOutcome, Resolution};
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
/// Highlighted match spans for ONE displayed row, in the same visible-cell
/// coordinates (relative to the row's own `[0, cols)` window) `ViewportRender::
/// rows`' own strings occupy -- `line::layout_row_with_marks`'s own output,
/// verbatim; `Document::viewport` never re-derives a column from a byte offset
/// itself. `spans` are every match intersecting this row (restyled REVERSED by
/// `render_viewport`, in the terminal frontend); `current` is, additionally,
/// the one match -- if any, and if still visible after clipping -- whose
/// absolute byte start is the active search's own `current` position
/// (restyled BOLD, on top of `spans`' REVERSED). Empty/`None` when no search
/// is active, the default `viewport` produces whenever `search` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowMarks {
    pub spans: Vec<std::ops::Range<u16>>,
    pub current: Option<std::ops::Range<u16>>,
}
/// The lines to draw for one screen, already chopped to the terminal width,
/// with `marks[i]` describing `rows[i]`'s own search highlights -- always the
/// same length as `rows`.
#[derive(Debug, PartialEq, Eq)]
pub struct ViewportRender {
    pub rows: Vec<String>,
    pub marks: Vec<RowMarks>,
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
    /// Requests to the background match-summary sweep (search v1, task 5): `Some((generation,
    /// pattern))` (re)starts it from byte 0, `None` idles it. Supersession on a fresh request
    /// is the driver's own job (`crate::analyzer`), not this sender's -- see
    /// `start_search_sweep`'s own doc comment for why a plain `send` (not `send_if_modified`)
    /// is correct here.
    search_requests: tokio::sync::watch::Sender<Option<(u64, Arc<crate::search::SearchPattern>)>>,
    /// The latest published `SearchSummary`; every `search_summary()` call clones this.
    search_summary_rx: tokio::sync::watch::Receiver<crate::search::SearchSummary>,
    /// Kept only so a LAZY driver spawn (`new_unindexed`'s own contract -- see
    /// `start_search_sweep`'s own doc comment) still has a sender to hand
    /// `crate::analyzer::spawn`; `new` consumes a clone of this immediately instead of waiting.
    search_summary_tx: tokio::sync::watch::Sender<crate::search::SearchSummary>,
    /// The next sweep's generation stamp; see `start_search_sweep`.
    search_generation: std::sync::atomic::AtomicU64,
    /// `None` until the sweep driver is actually running: always `Some` immediately after
    /// `new` (spawned eagerly, like `scheduler`/`status`), spawned lazily on the first
    /// `start_search_sweep` call for a `new_unindexed` document -- see that method's own doc
    /// comment for why, and for the narrow case where it needs (and, if absent, panics for
    /// lacking) a tokio runtime.
    search_driver: std::sync::Mutex<Option<crate::task_owner::TaskOwner<()>>>,
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
        let (search_requests, search_request_rx) = tokio::sync::watch::channel(None);
        let (search_summary_tx, search_summary_rx) =
            tokio::sync::watch::channel(crate::search::SearchSummary::empty());
        let search_driver = crate::analyzer::spawn(
            crate::search::SweepAnalysis::new(cache.clone()),
            search_request_rx,
            search_summary_tx.clone(),
        );
        Self {
            cache,
            size,
            config,
            prefetcher,
            scheduler: Some(scheduler),
            status: Some(status),
            search_requests,
            search_summary_rx,
            search_summary_tx,
            search_generation: std::sync::atomic::AtomicU64::new(0),
            search_driver: std::sync::Mutex::new(Some(search_driver)),
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
        // no runtime is required to build one (this constructor's own contract, above), so the
        // sweep driver is not spawned here -- only the channels, which need none -- see
        // `start_search_sweep`'s own doc comment for the lazy spawn this defers to.
        let (search_requests, _) = tokio::sync::watch::channel(None);
        let (search_summary_tx, search_summary_rx) =
            tokio::sync::watch::channel(crate::search::SearchSummary::empty());
        Self {
            cache,
            size,
            config,
            prefetcher,
            scheduler: None,
            status: None,
            search_requests,
            search_summary_rx,
            search_summary_tx,
            search_generation: std::sync::atomic::AtomicU64::new(0),
            search_driver: std::sync::Mutex::new(None),
        }
    }
    /// Aborts every background owner this document holds (the prefetcher, the index scan, the
    /// status worker, and the search sweep driver) and awaits each one's own teardown to
    /// finish, rather than the fire-and-forget abort-REQUEST a bare `drop` alone provides (each
    /// owner's `TaskOwner`/`JoinSet` aborts on drop, but does not wait for that abort to
    /// actually take effect). Measured, not assumed (U-bench, finding 4): in this crate's own 64
    /// MiB / 1 MiB-block fixture, `abort_and_join` on the index scan alone
    /// (which starts scanning immediately and unconditionally at
    /// construction, so it can be many blocks into an unthrottled pass by
    /// the time a single `.viewport()` call returns) measured in the
    /// hundreds of microseconds, and the status worker's in the tens to
    /// low hundreds — both dwarfing `first_paint`'s own tens-of-microseconds
    /// timed span at `latency_0ms`; the prefetcher's own teardown measured
    /// far smaller (tens of microseconds) but is included for the same
    /// reason. The search sweep driver is included for the identical reason as the other
    /// three (an eagerly-spawned background reader whose own still-unwinding abort must not
    /// bleed into the next timed iteration) though not separately re-measured here — it shares
    /// the same `crate::analyzer::spawn`-driven `TaskOwner` shape the others already cover. A
    /// criterion bench that builds a fresh `Document` every iteration needs all four awaited
    /// explicitly, OUTSIDE the timed region, before the next iteration starts — otherwise the
    /// previous iteration's own still-unwinding teardown keeps contending for the same runtime
    /// worker threads the next iteration's timed work needs. Bench-visible (test +
    /// bench-internals, matching `new_unindexed`'s own precedent — a `[[bench]]` target is a
    /// separate crate that cannot see crate-private items regardless of cfg, so this one
    /// specifically needs `pub`, unlike the owner-level methods it calls).
    #[cfg(any(test, feature = "bench-internals"))]
    pub async fn abort_background_and_join(&mut self) {
        self.prefetcher.abort_and_join().await;
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.abort_and_join().await;
        }
        if let Some(status) = self.status.as_mut() {
            status.abort_and_join().await;
        }
        if let Some(mut driver) = self.search_driver.get_mut().unwrap().take() {
            driver.abort_all_and_join().await;
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
    ///
    /// `search`, when `Some((pattern, current))`, additionally matches `pattern`
    /// against the WHOLE buffer this call already fetched via `fill_lines` above
    /// -- never a new read, so an active search costs this call zero extra IO --
    /// and returns the result as `ViewportRender::marks`. `current` is the
    /// ACTIVE match's absolute byte offset (`ActiveSearch::current` in the
    /// terminal frontend); it is resolved directly against the same buffer to
    /// pick out the one row's one span that also renders BOLD.
    ///
    /// `find_all` runs ONCE, here, over the whole contiguous `buf` -- not once
    /// per row over each row's own slice, the pre-batch-3 design -- so a match
    /// containing one of `buf`'s own newlines (e.g. an explicit `\n` in the
    /// pattern) is found at all: no single row's own slice ever holds both
    /// halves of it (batch 3 (2026-07-23), finding #10). Same order of work as
    /// the per-row calls this replaces: the expensive part, the one regex pass,
    /// still happens exactly once either way -- only the WIDTH of that one pass
    /// changed, not its count. Each row below clips the resulting absolute
    /// match list (`render_row`'s own `clip_to_row`) to its own bounds instead
    /// of re-deriving it, so a match spanning rows i..j contributes a clipped
    /// mark to each -- row i's own tail, row j's own head, every row strictly
    /// between the two entirely.
    pub async fn viewport(
        &self,
        top: Anchor,
        rows: usize,
        cols: usize,
        hscroll: HScroll,
        search: Option<(&crate::search::SearchPattern, Option<u64>)>,
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
        let all_matches: Vec<std::ops::Range<usize>> = match search {
            Some((pattern, _)) => pattern
                .find_all(&buf)
                .into_iter()
                .map(|(s, e)| s..e)
                .collect(),
            None => Vec::new(),
        };
        // batch 3 (2026-07-23), finding #11(a): resolved directly against the whole buffer,
        // independent of `all_matches` above -- `find_all`'s own non-overlapping walk can skip
        // straight over a start `search_next` itself legitimately lands on (e.g. `/aa` over
        // "aaa": `find_all` reports only 0..2, but `n` can land the cursor at 1, an overlapping
        // start `find_all` never reports) -- a lookup keyed on "which of `all_matches` starts
        // here" would find nothing for those. `find_starting_in`'s own accept range is a single
        // point (`rel..rel+1`): the exactly-once contract it was built for (search.rs's own doc
        // comment) applied here to ask one narrow question, "what starts exactly at `current`,"
        // never mind what `all_matches` separately found. `checked_sub` guards a `current` that
        // has scrolled above `top` (this viewport no longer contains it) the same way the
        // `rel <= buf.len()` check below guards one that hasn't been read this far yet -- both
        // sides of "not visible here" degrade to `None`, never a panic or a wrapped offset.
        let current_match: Option<std::ops::Range<usize>> = match search {
            Some((pattern, Some(target))) => target.checked_sub(top.offset()).and_then(|rel| {
                let rel = rel as usize;
                (rel <= buf.len())
                    .then(|| pattern.find_starting_in(&buf, rel..rel + 1))
                    .flatten()
                    .map(|(s, e)| s..e)
            }),
            _ => None,
        };
        let mut out = Vec::with_capacity(rows);
        let mut marks = Vec::with_capacity(rows);
        let mut line_start = 0usize;
        for i in 0..buf.len() {
            if buf[i] == b'\n' {
                let (row, row_marks) = render_row(
                    &buf,
                    line_start..i,
                    self.config.tab_stop,
                    hscroll.columns(),
                    cols,
                    &all_matches,
                    current_match.as_ref(),
                );
                out.push(row);
                marks.push(row_marks);
                line_start = i + 1;
                if out.len() == rows {
                    break;
                }
            }
        }
        if out.len() < rows && line_start < buf.len() && stop {
            let (row, row_marks) = render_row(
                &buf,
                line_start..buf.len(),
                self.config.tab_stop,
                hscroll.columns(),
                cols,
                &all_matches,
                current_match.as_ref(),
            );
            out.push(row);
            marks.push(row_marks);
        }
        Ok(ViewportRender { rows: out, marks })
    }
    /// Moves the top anchor by `delta` lines. Ready within the interactive
    /// budget; a longer scan continues in the background as a pending
    /// operation (any await point cancels cleanly).
    pub async fn scroll_lines(&self, from: Anchor, delta: i64) -> anyhow::Result<Resolution> {
        match delta.cmp(&0) {
            std::cmp::Ordering::Equal => Ok(Resolution::Ready(NavOutcome::At(from))),
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
            crate::scan::FwdStep::Found(s) => Ok(Resolution::Ready(NavOutcome::At(Anchor(s)))),
            crate::scan::FwdStep::Eof(clamp) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(clamp))))
            }
            crate::scan::FwdStep::More => {
                let cache = self.cache.clone();
                let span = self.size - start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.scanned(),
                    move |tx| async move {
                        scan.complete(&cache, tx, span)
                            .await
                            .map(|s| NavOutcome::At(Anchor(s)))
                    },
                )))
            }
        }
    }
    /// Finds the start of the `n`-th line before `start`, clamped at 0; a
    /// budget outcome continues as a pending background scan of the same
    /// scan object.
    async fn scroll_up(&self, start: u64, n: usize) -> anyhow::Result<Resolution> {
        if start == 0 || n == 0 {
            return Ok(Resolution::Ready(NavOutcome::At(Anchor(start))));
        }
        let mut scan = crate::scan::BackwardScan::new(start, n, self.config.nav_scan_budget);
        match scan.step(self.cache()).await? {
            crate::scan::BwdStep::Found(s) => Ok(Resolution::Ready(NavOutcome::At(Anchor(s)))),
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP))),
            crate::scan::BwdStep::More => {
                let cache = self.cache.clone();
                let span = start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.scanned(),
                    move |tx| async move {
                        scan.complete(&cache, tx, span)
                            .await
                            .map(|s| NavOutcome::At(Anchor(s)))
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
            return Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP)));
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
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP))),
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
                            return Ok(NavOutcome::At(Anchor(last)));
                        }
                        let walk = crate::scan::BackwardScan::new(last, up, budget);
                        // the walk-up is its own phase with its own honest
                        // span; inheriting the first phase's span pinned the
                        // progress row at 99% for the whole walk.
                        let span2 = walk.remaining_bytes();
                        walk.complete(&cache, tx, span2)
                            .await
                            .map(|s| NavOutcome::At(Anchor(s)))
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
            return Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP)));
        }
        if pct >= 100 {
            return self.goto_end(1).await;
        }
        let target = percent_offset(self.size, pct);
        if target == 0 {
            return Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP)));
        }
        let budget = self.config.nav_scan_budget;
        let bs = self.cache.block_size() as u64;
        let prev = self.cache.block((target - 1) / bs).await?;
        let rel = ((target - 1) % bs) as usize;
        if prev.get(rel) == Some(&b'\n') {
            return Ok(Resolution::Ready(NavOutcome::At(Anchor(target))));
        }
        // the answer is the line containing the target byte: search backward
        // for the newline that starts that line, pending past the budget.
        let mut snap = crate::scan::BackwardScan::new(target, 1, budget);
        match snap.step(self.cache()).await? {
            crate::scan::BwdStep::Found(start) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(start))))
            }
            crate::scan::BwdStep::Top => Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP))),
            crate::scan::BwdStep::More => {
                let cache = self.cache.clone();
                let span = target;
                Ok(Resolution::Pending(spawn_pending(
                    "percent jump",
                    span,
                    snap.scanned(),
                    move |tx| async move {
                        snap.complete(&cache, tx, span)
                            .await
                            .map(|s| NavOutcome::At(Anchor(s)))
                    },
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
                return Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP)));
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
                    return Ok(NavOutcome::At(Anchor::TOP));
                }
                let (line0, cp) = dephantom(&index, size, line0, cp);
                let (cp_off, cp_line0) = cp;
                let scan =
                    crate::scan::ForwardScan::new(cp_off, (line0 - cp_line0) as usize, budget);
                // the tail walk is its own phase, like goto_end's walk-up.
                let span2 = size - cp_off;
                scan.complete(&cache, tx, span2)
                    .await
                    .map(|s| NavOutcome::At(Anchor(s)))
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
            return Ok(Resolution::Ready(NavOutcome::At(Anchor::TOP)));
        }
        let (cp_off, cp_line0) = cp;
        let mut scan = crate::scan::ForwardScan::new(cp_off, (line0 - cp_line0) as usize, budget);
        match scan.step(self.cache()).await? {
            crate::scan::FwdStep::Found(s) => Ok(Resolution::Ready(NavOutcome::At(Anchor(s)))),
            crate::scan::FwdStep::Eof(clamp) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(clamp))))
            }
            crate::scan::FwdStep::More => {
                let cache = self.cache.clone();
                let span = self.size - cp_off;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to line",
                    span,
                    scan.scanned(),
                    move |tx| async move {
                        scan.complete(&cache, tx, span)
                            .await
                            .map(|s| NavOutcome::At(Anchor(s)))
                    },
                )))
            }
        }
    }
    /// Finds the next pattern match in `forward`'s direction from `origin`,
    /// wrapping around the file's other end (`wrapped: true`) when the
    /// origin-relative half finds nothing, and reporting `Exhausted` only
    /// once NEITHER half finds anything anywhere. `origin` is a raw byte
    /// offset, not yet an `Anchor` -- the caller passes `current_match + 1`
    /// to advance past a known match (forward), or `top.offset()`/any
    /// current position (backward; see the leg-1 construction below for why
    /// no `-1` adjustment is needed there). Anchor minting happens only
    /// here, on the way out.
    ///
    /// Only leg 1 -- the origin-relative half -- is tried within the
    /// interactive budget; a leg-1 budget exhaustion pends the WHOLE
    /// two-leg operation as one background scan (`spawn_search_pending`),
    /// never just the first half.
    pub async fn search_next(
        &self,
        origin: u64,
        pattern: &Arc<crate::search::SearchPattern>,
        forward: bool,
    ) -> anyhow::Result<Resolution> {
        let budget = self.config.nav_scan_budget;
        let size = self.size;
        // leg 1: origin -> the near end. Forward's `origin` is INCLUSIVE
        // (SearchForward's own `from`): "the first byte at-or-after which a
        // match START counts", exactly the contract `search_next_forward_
        // origin_is_inclusive` pins -- the caller alone is responsible for
        // skipping a just-found match by passing `current_match + 1`.
        // Backward's `hi = origin` is EXCLUSIVE (SearchBackward's own "not
        // including hi" contract, documented on its constructor): this
        // alone achieves "matches strictly below origin" with no extra
        // arithmetic here -- a match sitting exactly at `origin` (the
        // self-hit case, e.g. the cursor parked on the current match) can
        // never be read, let alone found, by this leg; pinned by
        // `search_next_backward_self_hit_finds_the_earlier_match_instead`
        // and `search_next_backward_self_hit_wraps_to_find_it_again`.
        let mut leg = if forward {
            SearchLeg::Fwd(crate::scan::SearchForward::new(
                pattern.clone(),
                origin,
                size,
                budget,
            ))
        } else {
            SearchLeg::Bwd(crate::scan::SearchBackward::new(
                pattern.clone(),
                origin,
                0,
                budget,
            ))
        };
        match leg.step(self.cache()).await? {
            crate::scan::SearchStep::Found {
                match_at,
                line_start,
            } => {
                self.resolve_found(leg.scanned(), match_at, line_start, false)
                    .await
            }
            crate::scan::SearchStep::End => self.search_wrapped_leg(origin, pattern, forward).await,
            crate::scan::SearchStep::More => Ok(Resolution::Pending(self.spawn_search_pending(
                leg,
                origin,
                pattern.clone(),
                forward,
            ))),
        }
    }
    /// Leg 2: the wraparound attempt from the file's FAR end, tried only
    /// after leg 1 (the origin-relative half, above) has exhaustively found
    /// nothing. Bounded so it can find, exactly once, a match that
    /// STRADDLES origin -- starts on leg 1's own excluded side but needs
    /// bytes past the origin seam to confirm, something leg 1 is
    /// structurally unable to see (see `wrapped_leg`'s own doc comment for
    /// the full bound derivation, both directions).
    async fn search_wrapped_leg(
        &self,
        origin: u64,
        pattern: &Arc<crate::search::SearchPattern>,
        forward: bool,
    ) -> anyhow::Result<Resolution> {
        let budget = self.config.nav_scan_budget;
        let mut leg = wrapped_leg(pattern.clone(), origin, self.size, forward, budget);
        match leg.step(self.cache()).await? {
            crate::scan::SearchStep::Found {
                match_at,
                line_start,
            } => {
                self.resolve_found(leg.scanned(), match_at, line_start, true)
                    .await
            }
            crate::scan::SearchStep::End => Ok(Resolution::Ready(NavOutcome::Exhausted)),
            crate::scan::SearchStep::More => {
                Ok(Resolution::Pending(self.spawn_wrapped_pending(leg)))
            }
        }
    }
    /// The interactive half of resolving a `Found` match into its terminal `NavOutcome`
    /// (`search_next`'s own leg-1 arm, `search_wrapped_leg`'s leg-2 one, above): attempts the
    /// line-start hunt within its own remaining budget (`resolve_line_start_step`, batch 3
    /// (2026-07-23), finding #1) and, if it doesn't fit, converts the WHOLE outcome into
    /// `Resolution::Pending` -- `match_at` is already known, only `top` is not, and the pending
    /// task finishes exactly the hunt this call already started (never re-derives it from
    /// scratch). `matched` is the match scan's own `scanned()`, already spent from the SAME
    /// interactive budget before this call -- the hunt below gets only what is left of it.
    async fn resolve_found(
        &self,
        matched: u64,
        match_at: u64,
        hint: Option<u64>,
        wrapped: bool,
    ) -> anyhow::Result<Resolution> {
        let budget = self.config.nav_scan_budget;
        let remaining = budget.saturating_sub(matched as usize);
        match resolve_line_start_step(self.cache(), remaining, match_at, hint).await? {
            LineStartStep::Resolved(top) => Ok(Resolution::Ready(NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            })),
            LineStartStep::More { scan, span } => {
                let cache = self.cache.clone();
                let scanned = scan.scanned();
                Ok(Resolution::Pending(spawn_pending(
                    "searching",
                    span,
                    scanned,
                    move |tx| async move {
                        let top = Anchor(scan.complete(&cache, tx, span).await?);
                        Ok(NavOutcome::FoundMatch {
                            top,
                            match_at,
                            wrapped,
                        })
                    },
                )))
            }
        }
    }
    /// Spawns leg 1's pending continuation: completes IT in the background,
    /// and, if it too ends without a match, continues straight into a
    /// freshly constructed leg 2 (`wrapped_leg` -- the identical bound
    /// derivation `search_wrapped_leg`'s own synchronous path uses) run to
    /// ITS OWN completion, all inside this one task. One pending operation
    /// covers both legs, never two separate hops back to the caller: the
    /// label stays "searching" and the SAME progress channel (`tx`) is used
    /// throughout, seeded with leg 1's own bytes already spent before
    /// pending. `span = size` (the whole two-leg walk's own honest upper
    /// bound) for both legs' own sends -- deliberately NOT swapped to a
    /// leg-specific `span2` the way `goto_end`'s own two-phase pending does,
    /// so a leg transition can make the raw `scanned` number dip (leg 2
    /// starts its own count from 0) rather than stay monotonic; accepted
    /// for v1 (`Progress` has never promised monotonicity, only a bound and
    /// an eventual completion signal) in exchange for one constant
    /// span/label the whole way through instead of a phase relabel.
    ///
    /// Every await here -- both legs' own `complete()`, which already
    /// yields once per chunk, and the line-start resolution's own backward
    /// hunt -- is a cancel point.
    fn spawn_search_pending(
        &self,
        leg: SearchLeg,
        origin: u64,
        pattern: Arc<crate::search::SearchPattern>,
        forward: bool,
    ) -> crate::resolve::PendingNav {
        let cache = self.cache.clone();
        let size = self.size;
        let budget = self.config.nav_scan_budget;
        let scanned = leg.scanned();
        spawn_pending("searching", size, scanned, move |tx| async move {
            match leg.complete(&cache, tx.clone(), size).await? {
                crate::scan::SearchStep::Found {
                    match_at,
                    line_start,
                } => {
                    found_outcome_pending(&cache, budget, match_at, line_start, false, tx, size)
                        .await
                }
                crate::scan::SearchStep::End => {
                    // leg 1 exhausted its whole origin-relative half with no match; continue
                    // straight into leg 2 (the wrap), inside this same background task -- never
                    // a second hop back to the caller. `tx.clone()` here (unlike the bare `tx`
                    // pre-batch-3): a Found below still needs `tx` again, for ITS OWN line-start
                    // hunt (`found_outcome_pending`, finding #1) -- the one remaining owner.
                    match wrapped_leg(pattern, origin, size, forward, budget)
                        .complete(&cache, tx.clone(), size)
                        .await?
                    {
                        crate::scan::SearchStep::Found {
                            match_at,
                            line_start,
                        } => {
                            found_outcome_pending(
                                &cache, budget, match_at, line_start, true, tx, size,
                            )
                            .await
                        }
                        crate::scan::SearchStep::End => Ok(NavOutcome::Exhausted),
                        crate::scan::SearchStep::More => {
                            unreachable!("complete() always returns a terminal step")
                        }
                    }
                }
                crate::scan::SearchStep::More => {
                    unreachable!("complete() always returns a terminal step")
                }
            }
        })
    }
    /// Spawns leg 2's own pending continuation when the WRAP attempt itself
    /// (not leg 1) exhausts the interactive budget -- reached only via
    /// `search_wrapped_leg`'s own `More` arm, i.e. leg 1 already
    /// synchronously returned `End` before this leg was even constructed.
    /// Simpler than `spawn_search_pending`: there is no leg 3, so completing
    /// this one scan is the whole job.
    fn spawn_wrapped_pending(&self, leg: SearchLeg) -> crate::resolve::PendingNav {
        let cache = self.cache.clone();
        let size = self.size;
        let budget = self.config.nav_scan_budget;
        let scanned = leg.scanned();
        spawn_pending("searching", size, scanned, move |tx| async move {
            match leg.complete(&cache, tx.clone(), size).await? {
                crate::scan::SearchStep::Found {
                    match_at,
                    line_start,
                } => {
                    found_outcome_pending(&cache, budget, match_at, line_start, true, tx, size)
                        .await
                }
                crate::scan::SearchStep::End => Ok(NavOutcome::Exhausted),
                crate::scan::SearchStep::More => {
                    unreachable!("complete() always returns a terminal step")
                }
            }
        })
    }
    /// Starts (or restarts) the background match-summary sweep over `pattern` from byte 0,
    /// returning the generation stamp the resulting snapshots will carry -- `search_summary()`'s
    /// own receiver distinguishes a fresh sweep's snapshots from a stale, in-flight previous
    /// one by this alone (see `crate::search::SearchSummary`'s own doc comment). A request
    /// always restarts from scratch, even for an unchanged pattern: unlike
    /// `request_line_number`'s per-frame idempotent redraw suppression (`send_if_modified`),
    /// this is never called every frame, only on a genuine new-search action, so a plain `send`
    /// is correct -- supersession itself is entirely `crate::analyzer`'s own driver's job, not
    /// this sender's.
    ///
    /// On a document built via `new_unindexed`, the FIRST call spawns the sweep driver lazily
    /// (that constructor's own contract: no runtime is needed to build one, unlike `new` --
    /// see its own doc comment) and therefore needs an active tokio runtime to do so, exactly
    /// like `new` itself already needs one at construction -- `tokio::spawn`'s own
    /// runtime-context check panics if none is active, the same mechanism `new` already relies
    /// on, not a new failure mode this method introduces. Every later call reuses the
    /// already-spawned driver and needs no runtime at all. Every test that never calls this on
    /// a `new_unindexed` document (the overwhelming majority -- see `NO_INDEX`'s own precedent
    /// for the other index-only queries) never touches this requirement at all.
    pub fn start_search_sweep(&self, pattern: Arc<crate::search::SearchPattern>) -> u64 {
        let generation = self
            .search_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        {
            let mut driver = self.search_driver.lock().unwrap();
            if driver.is_none() {
                *driver = Some(crate::analyzer::spawn(
                    crate::search::SweepAnalysis::new(self.cache.clone()),
                    self.search_requests.subscribe(),
                    self.search_summary_tx.clone(),
                ));
            }
        }
        let _ = self.search_requests.send(Some((generation, pattern)));
        generation
    }
    /// Idles the sweep: the driver parks until the next `start_search_sweep` call, leaving the
    /// last published `SearchSummary` in place (stale, not cleared -- a caller that wants
    /// "nothing" rather than "whatever was last found" compares `search_summary().borrow().
    /// generation` against the generation it started, the same pattern
    /// `sweep_summary_is_generation_stamped_and_supersedes` pins for a fresh request
    /// superseding a stale one).
    pub fn cancel_search_sweep(&self) {
        let _ = self.search_requests.send(None);
    }
    /// A fresh subscription to match-summary updates; see `crate::search::SearchSummary`.
    pub fn search_summary(&self) -> tokio::sync::watch::Receiver<crate::search::SearchSummary> {
        self.search_summary_rx.clone()
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
    Fut: std::future::Future<Output = anyhow::Result<NavOutcome>> + Send + 'static,
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
/// One leg of a two-leg search: whichever direction's scan object, unified
/// behind one small wrapper so `search_next`'s own composition never
/// matches on `Fwd`/`Bwd` at every call site. A pending continuation holds
/// one of these across the budget boundary, exactly like every other
/// resumable scan in this file lives inside its own struct.
enum SearchLeg {
    Fwd(crate::scan::SearchForward),
    Bwd(crate::scan::SearchBackward),
}
impl SearchLeg {
    async fn step(
        &mut self,
        cache: &crate::cache::BlockCache,
    ) -> anyhow::Result<crate::scan::SearchStep> {
        match self {
            SearchLeg::Fwd(s) => s.step(cache).await,
            SearchLeg::Bwd(s) => s.step(cache).await,
        }
    }
    fn scanned(&self) -> u64 {
        match self {
            SearchLeg::Fwd(s) => s.scanned(),
            SearchLeg::Bwd(s) => s.scanned(),
        }
    }
    async fn complete(
        self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<crate::scan::SearchStep> {
        match self {
            SearchLeg::Fwd(s) => s.complete(cache, tx, span).await,
            SearchLeg::Bwd(s) => s.complete(cache, tx, span).await,
        }
    }
}
/// Constructs leg 2 -- the wraparound attempt from the file's far end,
/// bounded past the origin seam so it can find a match that STRADDLES
/// origin (starts on leg 1's own excluded side but needs bytes at-or-past
/// the seam to confirm) exactly once, never zero times and never twice.
/// Shared by `search_wrapped_leg` (the synchronous, one-step attempt) and
/// `spawn_search_pending`'s own background continuation, so the bound
/// formula lives in exactly one place.
///
/// **Forward** (leg 1 covered `[origin, size)`, exactly; no truncation risk
/// there, since its own far bound is the true EOF): leg 2 reads `[0, size)`
/// but is bounded above by `origin + MAX_MATCH_LEN`, not the naive `origin`.
/// `SearchForward`'s own `limit` doubles as both the accepted-start bound
/// AND the read bound at once (`step` clamps `size = cache.size().min
/// (limit)` and never reads a byte past that clamp) -- so a naive `limit =
/// origin` would silently truncate the hay a straddling match (start <
/// origin <= end) needs to confirm itself, and leg 1 can't see it either
/// (its own bound is the true EOF, not the seam). Widening past `origin` by
/// up to `MAX_MATCH_LEN` -- the documented cap past which a match is not
/// guaranteed findable regardless -- lets this leg finally confirm it.
/// Safe: leg 2 only ever runs after leg 1's own exhaustive `End`, which
/// already proves no match starts anywhere in `[origin, size)` -- so this
/// leg's widened read can newly ACCEPT only a match starting below origin
/// (leg 1's own `End` already ruled out every other kind), never
/// double-report one leg 1 already found (leg 1 would have returned
/// `Found`, and leg 2 would never even run).
///
/// **Backward is NOT mirrored the same way** (batch 3 (2026-07-23), finding #3's own
/// re-derivation -- this was still true before that fix, and the paragraph above described it
/// accurately then): `SearchBackward::step` itself now reads PAST its own `hi` -- up to `hi +
/// MAX_MATCH_LEN`, capped at the true EOF -- purely as lookahead the moment `step` is first
/// called, while still only ACCEPTING starts strictly below `hi` (that struct's own doc
/// comment). Leg 1 here (`hi = origin`) therefore already confirms a straddler on its own --
/// the identical job forward's leg 2 above still has to do by widening ITS OWN bound past the
/// seam, since forward has no such mechanism (a match starting below forward's own origin is
/// invisible to leg 1 there, full stop, `SearchForward::new`'s own inclusive-`from` contract).
/// Leg 2's own floor is therefore the NAIVE `origin`, not `origin - MAX_MATCH_LEN`: widening it
/// would only re-examine territory leg 1's own exhaustive `End` already ruled out, straddlers
/// included, so leg 2 would find nothing new there, ever. `hi = size` stays the true EOF (no
/// truncation risk, matching forward leg 2 above): leg 2's real job is exactly `[origin, size)`
/// -- the half leg 1's own `hi = origin` excludes entirely, never any part of what leg 1 already
/// owns. Proven, not merely argued: `search_next_backward_wrap_finds_a_match_straddling_the_
/// origin` (this file's own test) used to need this widening to pass; post-#3 it is found by
/// LEG 1 instead (`wrapped: false`, not `true`) with no widening at all.
fn wrapped_leg(
    pattern: Arc<crate::search::SearchPattern>,
    origin: u64,
    size: u64,
    forward: bool,
    budget: usize,
) -> SearchLeg {
    if forward {
        let limit = origin.saturating_add(crate::search::MAX_MATCH_LEN as u64);
        SearchLeg::Fwd(crate::scan::SearchForward::new(pattern, 0, limit, budget))
    } else {
        // NOT widened -- deliberately the naive `origin`: `SearchBackward` itself now confirms
        // a straddler on leg 1's own pass (batch 3 (2026-07-23), finding #3), so leg 2 never
        // needs to re-examine that territory; see this function's own doc comment above.
        let floor = origin;
        SearchLeg::Bwd(crate::scan::SearchBackward::new(
            pattern, size, floor, budget,
        ))
    }
}
/// The free lookups shared by both budget regimes below (batch 3 (2026-07-23), finding #1): the
/// scan's own `hint`, when the match confirmation already saw the preceding newline; `match_at
/// == 0`; or a cheap single-block check for "already a line start" -- unchanged from the pre-#1
/// code, and never the bug that finding was about. `match_at` may itself ALREADY be a line start
/// (a match can begin right after a newline): `BackwardScan::new(pos, 1, ..)` assumes `pos`
/// already IS one and deliberately excludes `pos - 1` from its own search window -- "the newline
/// that made it a line start" (its own doc comment) -- so calling it directly here on a
/// `match_at` that IS one would skip straight past match_at's own newline and return the
/// PREVIOUS line's start instead (RED: caught by `search_next_forward_origin_is_inclusive`,
/// expected `Anchor(4)`, got `Anchor(0)`). Checked first and short-circuited, mirroring
/// `goto_percent`'s identical guard for the identical reason (same file, above). `Ok(None)`
/// means a real backward hunt is unavoidable; the caller picks it up from there under its own
/// budget regime.
async fn line_start_shortcut(
    cache: &crate::cache::BlockCache,
    match_at: u64,
    hint: Option<u64>,
) -> anyhow::Result<Option<Anchor>> {
    if let Some(nl) = hint {
        return Ok(Some(Anchor(nl)));
    }
    if match_at == 0 {
        return Ok(Some(Anchor::TOP));
    }
    let bs = cache.block_size() as u64;
    let prev = cache.block((match_at - 1) / bs).await?;
    let rel = ((match_at - 1) % bs) as usize;
    if prev.get(rel) == Some(&b'\n') {
        return Ok(Some(Anchor(match_at)));
    }
    Ok(None)
}
/// Whether the interactive line-start hunt (`Document::resolve_found`, above) resolved outright,
/// or still has work left -- `More` means the caller must convert the WHOLE `search_next` Found
/// outcome into `Resolution::Pending`: `match_at` is already known, only `top` is not.
enum LineStartStep {
    Resolved(Anchor),
    /// `scan` still needs `BackwardScan::complete`; `span` is its own full window, captured at
    /// construction (`BackwardScan::remaining_bytes`'s own doc comment) -- before the one
    /// interactive step below shrank it -- so the pending task's own progress reports against
    /// the hunt's true total, not what is left after that first bite.
    More {
        scan: crate::scan::BackwardScan,
        span: u64,
    },
}
/// Attempts the line-start hunt within `remaining` bytes -- the interactive nav budget LESS
/// whatever the match scan that found `match_at` already spent (batch 3 (2026-07-23), finding
/// #1: without this cap, a hot cache let the OLD unconditional hunt run to completion inside the
/// very same poll that found the match, no yield point in reach regardless of the file's own
/// size -- 157 reads for a 10,000-byte single line at budget 64, probe-verified). One shared
/// helper for both `search_next`'s own leg-1 Found arm and `search_wrapped_leg`'s leg-2 one
/// (`Document::resolve_found` is their common caller) -- budgeted the identical way either leg.
async fn resolve_line_start_step(
    cache: &crate::cache::BlockCache,
    remaining: usize,
    match_at: u64,
    hint: Option<u64>,
) -> anyhow::Result<LineStartStep> {
    if let Some(top) = line_start_shortcut(cache, match_at, hint).await? {
        return Ok(LineStartStep::Resolved(top));
    }
    let mut scan = crate::scan::BackwardScan::new(match_at, 1, remaining);
    let span = scan.remaining_bytes();
    match scan.step(cache).await? {
        crate::scan::BwdStep::Found(s) => Ok(LineStartStep::Resolved(Anchor(s))),
        crate::scan::BwdStep::Top => Ok(LineStartStep::Resolved(Anchor::TOP)),
        crate::scan::BwdStep::More => Ok(LineStartStep::More { scan, span }),
    }
}
/// The background half of resolving a `Found` match's line start: unlike `resolve_line_start_
/// step` above, a task already running as a `Resolution::Pending` has no interactive budget left
/// to protect -- the hunt is driven straight through `BackwardScan::complete` instead, which
/// yields and publishes progress once per chunk like every other resumable scan in this module
/// (never the tight, unyielding step loop this whole finding replaces -- `BackwardScan`'s reads
/// promote the cache, and the byte just before a match the caller JUST confirmed is
/// overwhelmingly likely to still be in the very block that confirmed it, so this is a cache hit
/// far more often than a fresh read). `tx`/`span` are the SAME progress channel and span the
/// whole pending operation already publishes on -- label "searching" throughout, `scanned`
/// dipping at the phase transition exactly like the leg-1 -> leg-2 transition this mirrors
/// (`spawn_search_pending`'s own doc comment). A free function, not a `Document` method, so the
/// pending closures above (which own a cloned cache, not `&Document`) can call it for a match
/// found in the background.
async fn found_outcome_pending(
    cache: &crate::cache::BlockCache,
    budget: usize,
    match_at: u64,
    hint: Option<u64>,
    wrapped: bool,
    tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
    span: u64,
) -> anyhow::Result<NavOutcome> {
    let top = match line_start_shortcut(cache, match_at, hint).await? {
        Some(top) => top,
        None => {
            let scan = crate::scan::BackwardScan::new(match_at, 1, budget);
            Anchor(scan.complete(cache, tx, span).await?)
        }
    };
    Ok(NavOutcome::FoundMatch {
        top,
        match_at,
        wrapped,
    })
}
/// Clips an absolute (into `buf`) byte range to row `line`'s own bounds, shifted into that
/// row's own relative coordinates -- `None` when the two don't intersect at all. Both
/// `render_row` call sites below (a row's own full match list, and the current match's own
/// single span) key off this shared arithmetic (batch 3 (2026-07-23), finding #10), so a
/// match/current spanning a `\n` -- or landing exactly ON one -- is clipped identically either
/// way.
///
/// `line.end` is the row's own newline position (or `buf.len()` for an unterminated final row),
/// EXCLUSIVE for a real-extent match (the newline itself is never part of any row's own visible
/// content) but treated as still belonging to THIS row for a zero-width one landing exactly
/// there (`$` at end-of-line; finding #11(b), line.rs) -- the row's own newline byte has no cell
/// of its own to hand the match to instead, and the NEXT row's own `line.start` sits one byte
/// past it, so the two never double-claim the same absolute position.
fn clip_to_row(
    m: &std::ops::Range<usize>,
    line: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    if m.start >= m.end {
        return (line.start <= m.start && m.start <= line.end)
            .then(|| m.start - line.start..m.start - line.start);
    }
    let start = m.start.max(line.start);
    let end = m.end.min(line.end);
    (start < end).then(|| start - line.start..end - line.start)
}
/// Lays out one row AND, when a search is active, its match highlights --
/// `Document::viewport`'s own per-line body, factored out since it is called from both the
/// newline-terminated and the final-unterminated-line sites. `all_matches`/`current_match` are
/// ABSOLUTE (into `buf`, computed once by `viewport` itself -- see its own doc comment on why,
/// batch 3 (2026-07-23), finding #10); clipped to `line`'s own bounds here, per row, via
/// `clip_to_row` above.
///
/// `current`'s own visible span is resolved with a SECOND, single-match `layout_row_with_marks`
/// call, not a lookup into the first call's own (already merged) `spans`: the merged list no
/// longer remembers which original match contributed which cells, so re-laying out just the one
/// matched byte range -- cheap, at most once per screen, only for the row that actually contains
/// it -- is simpler than threading that bookkeeping through `layout_row_with_marks`'s own return
/// value. Once resolved, `current`'s own span is folded into `spans` too, if not covered by an
/// entry already there (finding #11(a)): `current`, now resolved independently of `all_matches`
/// (see `viewport`'s own doc comment), can land on a span `find_all` never reported at all -- an
/// `n` on an overlapping start -- and `RowMarks`'s own contract (see its doc comment) requires
/// `current` to always be contained in `spans`, since BOLD is only ever painted on top of an
/// already-REVERSED cell (`render_viewport`'s own doc comment, `ress/src/render.rs`).
fn render_row(
    buf: &[u8],
    line: std::ops::Range<usize>,
    tab_stop: usize,
    hscroll: usize,
    cols: usize,
    all_matches: &[std::ops::Range<usize>],
    current_match: Option<&std::ops::Range<usize>>,
) -> (String, RowMarks) {
    let slice = &buf[line.clone()];
    let byte_marks: Vec<std::ops::Range<usize>> = all_matches
        .iter()
        .filter_map(|m| clip_to_row(m, &line))
        .collect();
    let (row, mut spans) =
        crate::line::layout_row_with_marks(slice, tab_stop, hscroll, cols, &byte_marks);
    let current = current_match
        .and_then(|m| clip_to_row(m, &line))
        .and_then(|m| {
            crate::line::layout_row_with_marks(
                slice,
                tab_stop,
                hscroll,
                cols,
                std::slice::from_ref(&m),
            )
            .1
            .into_iter()
            .next()
        });
    if let Some(cur) = &current
        && !spans
            .iter()
            .any(|s| s.start <= cur.start && cur.end <= s.end)
    {
        spans.push(cur.clone());
    }
    (row, RowMarks { spans, current })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchPattern;
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
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn returns_all_lines_when_file_is_short() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 10, 80, HScroll::ZERO, None)
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
        let v = d
            .viewport(Anchor::TOP, 5, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["abc".to_string()]);
    }
    #[tokio::test]
    async fn chops_long_lines_to_width() {
        let d = doc(b"xxxxxxxxxx\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 1, 4, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["xxxx".to_string()]);
    }
    #[tokio::test]
    async fn handles_lines_spanning_block_boundaries() {
        let d = doc(b"aaaaaa\nbb\n", 4);
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["aaaaaa".to_string(), "bb".to_string()]);
    }
    #[tokio::test]
    async fn strips_trailing_carriage_return() {
        let d = doc(b"a\r\nb\r\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
    }
    #[tokio::test]
    async fn empty_file_has_no_rows() {
        let d = doc(b"", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 5, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert!(v.rows.is_empty());
    }
    #[tokio::test]
    async fn starts_from_nonzero_line_offset() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let v = d
            .viewport(Anchor(2), 2, 80, HScroll::ZERO, None)
            .await
            .unwrap();
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
        let v = d
            .viewport(Anchor::TOP, 2, 4, HScroll::ZERO, None)
            .await
            .unwrap();
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
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["a       b".to_string()]);
    }
    #[tokio::test]
    async fn viewport_applies_horizontal_scroll() {
        let d = doc(b"0123456789\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 1, 4, HScroll::new(3), None)
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
            .viewport(Anchor::TOP, 1, 10, HScroll::new(200), None)
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
            .viewport(Anchor::TOP, 1, 1, HScroll::new(usize::MAX), None)
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
        let a = d.scroll_lines(Anchor::TOP, 1).await.unwrap().ready().at();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_down_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor::TOP, 2).await.unwrap().ready().at();
        assert_eq!(a, Anchor(10));
    }
    #[tokio::test]
    async fn scroll_down_clamps_to_last_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap().ready().at();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scroll_down_clamps_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.scroll_lines(Anchor::TOP, 99).await.unwrap().ready().at();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn scrolls_up_one_line() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(4), -1).await.unwrap().ready().at();
        assert_eq!(a, Anchor(2));
    }
    #[tokio::test]
    async fn scrolls_up_across_block_boundary() {
        let d = doc(b"aaaa\nbbbb\ncccc\n", 4);
        let a = d.scroll_lines(Anchor(10), -2).await.unwrap().ready().at();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn scroll_up_clamps_at_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), -99).await.unwrap().ready().at();
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
        let top = d.scroll_lines(Anchor(198), -40).await.unwrap().ready().at();
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
            d.scroll_lines(Anchor::TOP, 5).await.unwrap().ready().at(),
            Anchor::TOP
        );
        assert_eq!(
            d.scroll_lines(Anchor::TOP, -5).await.unwrap().ready().at(),
            Anchor::TOP
        );
        assert_eq!(d.goto_end(3).await.unwrap().ready().at(), Anchor::TOP);
        assert_eq!(d.goto_percent(50).await.unwrap().ready().at(), Anchor::TOP);
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
            top = d.scroll_lines(top, 50).await.unwrap().ready().at();
            let _ = d.viewport(top, 40, 80, HScroll::ZERO, None).await.unwrap();
        }
        let cold = src.read_count();
        for _ in 0..10 {
            top = d.scroll_lines(top, -50).await.unwrap().ready().at();
            let _ = d.viewport(top, 40, 80, HScroll::ZERO, None).await.unwrap();
        }
        assert_eq!(src.read_count(), cold, "return trip issued cold reads");
    }
    #[tokio::test]
    async fn scroll_zero_is_identity() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        let a = d.scroll_lines(Anchor(2), 0).await.unwrap().ready().at();
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
        let a = d.goto_end(2).await.unwrap().ready().at();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_without_trailing_newline() {
        let d = doc(b"a\nb\nc", 1 << 20);
        let a = d.goto_end(1).await.unwrap().ready().at();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_end_clamps_when_file_shorter_than_screen() {
        let d = doc(b"a\nb\n", 1 << 20);
        let a = d.goto_end(10).await.unwrap().ready().at();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_end_of_empty_file_is_top() {
        let d = doc(b"", 1 << 20);
        let a = d.goto_end(10).await.unwrap().ready().at();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_snaps_to_line_start() {
        // size 8; 50% -> offset 4, already a line start ('c').
        let d = doc(b"a\nb\nc\nd\n", 1 << 20);
        let a = d.goto_percent(50).await.unwrap().ready().at();
        assert_eq!(a, Anchor(4));
    }
    #[tokio::test]
    async fn goto_percent_lands_on_the_containing_line() {
        // 37% of 8 bytes = offset 2, the '\n' terminating "aa": that byte
        // belongs to the "aa" line, so the jump lands on its start.
        let d = doc(b"aa\nb\ncc\n", 1 << 20);
        let a = d.goto_percent(37).await.unwrap().ready().at();
        assert_eq!(a, Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_zero_is_top() {
        let d = doc(b"a\nb\nc\n", 1 << 20);
        assert_eq!(d.goto_percent(0).await.unwrap().ready().at(), Anchor::TOP);
    }
    #[tokio::test]
    async fn goto_percent_with_no_newline_above_target_is_top() {
        // the target sits mid-line with nothing but content above it: the
        // containing line is the first line, deterministically pinned (the
        // property covers this class; this pins the exact branch).
        let d = doc(b"abcdefgh", 1 << 20);
        assert_eq!(d.goto_percent(50).await.unwrap().ready().at(), Anchor::TOP);
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
        let a = d.goto_percent(75).await.unwrap().ready().at();
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
        assert_eq!(
            d.goto_percent(51).await.unwrap().ready().at(),
            Anchor(10001)
        );
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
        assert_eq!(d.goto_percent(50).await.unwrap().ready().at(), Anchor(4));
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(200));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(7680), "must land on the boundary-byte line start");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                    .unwrap()
                    .at();
                assert_eq!(a, Anchor(2));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(2), "must clamp to the giant line's start");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(200), "the line containing the target byte");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor::TOP, "no second newline above the tail");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(198));
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                let a = p.handle.await.unwrap().unwrap().at();
                assert_eq!(a, Anchor(200), "the giant tail's start, never TOP");
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
        let a = (&mut p.handle).await.unwrap().unwrap().at();
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
                let a = (&mut p.handle).await.unwrap().unwrap().at();
                assert_eq!(a, Anchor::at(34990));
                let last = *p.progress.borrow();
                assert!(
                    last.scanned > first.scanned,
                    "a pending line jump must publish frontier progress"
                );
            }
            Resolution::Ready(a) => panic!("expected Pending, got Ready({a:?})"),
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
                assert_eq!(p.handle.await.unwrap().unwrap().at(), Anchor::at(2046));
            }
            Resolution::Ready(a) => panic!(
                "expected Pending -- the whole point of this test is the pending path's own \
                 coverage check -- got Ready({a:?})"
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
            Resolution::Ready(a) => assert_eq!(a.at(), Anchor::at(2046)),
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
            let _ = d.viewport(a, 5, 80, HScroll::ZERO, None).await.unwrap();
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

    // ---- search_next (search v1, task 4) ----

    fn needle_pattern() -> Arc<SearchPattern> {
        Arc::new(SearchPattern::compile("needle", false).unwrap())
    }
    #[tokio::test]
    async fn search_next_lands_the_match_line_at_the_top() {
        // "needle" sits mid-line (byte 8); its own LINE starts earlier, at
        // byte 4 -- `top` must be the line start, not `match_at` itself.
        let d = doc(b"aaa\nxxx needle yyy\n", 1 << 20);
        let outcome = d
            .search_next(0, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(top, Anchor::at(4));
                assert_eq!(match_at, 8);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_wraps_and_says_so() {
        // the only "needle" sits BEFORE origin; leg 1 (origin -> EOF) finds
        // nothing, so leg 2 must wrap from the top and say so.
        let d = doc(b"needle\nxxx\nyyy\n", 1 << 20);
        let outcome = d
            .search_next(7, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(top, Anchor::TOP);
                assert_eq!(match_at, 0);
                assert!(wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_exhausted_when_no_match_anywhere() {
        let d = doc(b"aaa\nbbb\nccc\n", 1 << 20);
        let outcome = d
            .search_next(0, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(outcome, NavOutcome::Exhausted);
    }
    #[tokio::test]
    async fn search_next_forward_out_of_range_origin_does_not_panic() {
        // batch 3 (2026-07-23), finding #6, end to end: `SearchForward::new`'s own `from` (here
        // `origin`, forwarded unclamped) used to panic indexing its ctx seed's own block before
        // `pos` was ever clamped into the file -- `search_next` must clamp gracefully instead,
        // the same "out-of-contract positions degrade to the real bytes" contract every other
        // scan object already honors.
        let d = doc(b"aaa\nbbb\nccc\n", 1 << 20);
        let outcome = d
            .search_next(u64::MAX, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(outcome, NavOutcome::Exhausted);
    }
    #[tokio::test]
    async fn search_next_backward_mirrors() {
        // Found, not wrapped: the only match sits below origin.
        {
            let d = doc(b"xxx needle yyy\nzzz\n", 1 << 20);
            let outcome = d
                .search_next(15, &needle_pattern(), false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            match outcome {
                NavOutcome::FoundMatch {
                    top,
                    match_at,
                    wrapped,
                } => {
                    assert_eq!(top, Anchor::TOP, "no newline precedes the only line");
                    assert_eq!(match_at, 4);
                    assert!(!wrapped);
                }
                other => panic!("expected FoundMatch, got {other:?}"),
            }
        }
        // Wraps: the only match sits AT/AFTER origin; leg 1 (strictly below
        // origin) must find nothing and hand off to the wrap.
        {
            let d = doc(b"xxx\nyyy needle zzz\n", 1 << 20);
            let outcome = d
                .search_next(2, &needle_pattern(), false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            match outcome {
                NavOutcome::FoundMatch {
                    top,
                    match_at,
                    wrapped,
                } => {
                    assert_eq!(top, Anchor::at(4));
                    assert_eq!(match_at, 8);
                    assert!(wrapped);
                }
                other => panic!("expected FoundMatch, got {other:?}"),
            }
        }
        // Exhausted: no match anywhere, either direction.
        {
            let d = doc(b"aaa\nbbb\nccc\n", 1 << 20);
            let outcome = d
                .search_next(7, &needle_pattern(), false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            assert_eq!(outcome, NavOutcome::Exhausted);
        }
    }
    #[tokio::test]
    async fn search_next_resolves_a_line_start_the_scan_could_not() {
        // origin (4) sits exactly at the containing line's own start, so the
        // forward scan itself never reads the newline that made it one (it
        // lies at byte 3, before origin) -- `SearchStep::Found` reports
        // `line_start: None`, and `search_next` must still resolve the true
        // top (4) via its own backward hunt, not leave it wrong or panic.
        let d = doc(b"zzz\nabc needle xyz", 1 << 20);
        let outcome = d
            .search_next(4, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(top, Anchor::at(4));
                assert_eq!(match_at, 8);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_resolves_the_true_top_when_a_persisted_newline_falls_inside_the_match() {
        // batch 3 (2026-07-23), finding #4, end to end: "foo\nbar" (an explicit-`\n` pattern)
        // matches [2, 9) in "xxfoo\nbarzz"; chunked block_size-1 reads make `SearchForward`
        // record the `\n` at 5 as a candidate line start on an EARLIER iteration, before it can
        // know that position ends up INSIDE this match rather than before it. Pre-fix the scan
        // reported `line_start: Some(6)` -- PAST `match_at` (2) -- topping the viewport past the
        // match's own start; post-fix it reports `None`, and `search_next`'s own bounded
        // backward hunt (`resolve_line_start_step`/`resolve_found`) must resolve the TRUE top: no
        // `\n` precedes position 2 at all, so that hunt lands on `Anchor::TOP`, not the false
        // `Anchor(6)`.
        let d = doc(b"xxfoo\nbarzz", 1);
        let pattern = Arc::new(SearchPattern::compile("foo\nbar", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 2);
                assert_eq!(
                    top,
                    Anchor::TOP,
                    "no newline precedes the match's own start"
                );
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_pending_covers_both_legs() {
        // budget (64) can't cross the 2000-byte filler between origin and
        // EOF in one step, forcing leg 1 to pend; the only "needle" sits
        // BEFORE origin, so leg 1's own completion must End and the SAME
        // background task must continue straight into leg 2 (the wrap) --
        // driven via the gate and a positive `started_events` wait, never a
        // sleep (this crate's own event-driven testing policy).
        let mut data = b"needle\n".to_vec();
        data.extend_from_slice(&[b'x'; 2000]);
        data.push(b'\n');
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new_unindexed(
            src.clone(),
            Config {
                block_size: 32,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(mut p) = d.search_next(7, &needle_pattern(), true).await.unwrap()
        else {
            panic!("expected Pending -- 2000 filler bytes must exceed the 64-byte budget");
        };
        assert_eq!(p.label, "searching");
        let mut started = src.started_events();
        let baseline = *started.borrow();
        src.arm_gate();
        wait_for_count(&mut started, |n| n > baseline).await;
        src.open_gate();
        let outcome = (&mut p.handle).await.unwrap().unwrap();
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert!(wrapped, "the only match sits before origin -- must wrap");
                assert_eq!(match_at, 0);
                assert_eq!(top, Anchor::TOP);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_pending_cancels_cleanly() {
        // budget forces Pending; the gate then catches the background
        // task's own next read genuinely in-flight before cancelling -- a
        // positive `cancelled_events` proof, not a bounded-silence wait.
        let mut data = vec![b'x'; 8192];
        data.extend_from_slice(b"needle\n");
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new_unindexed(
            src.clone(),
            Config {
                block_size: 64,
                nav_scan_budget: 256,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.search_next(0, &needle_pattern(), true).await.unwrap()
        else {
            panic!("expected Pending");
        };
        let mut started = src.started_events();
        let baseline = *started.borrow();
        src.arm_gate();
        wait_for_count(&mut started, |n| n > baseline).await;
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        p.cancel();
        let _ = p.handle.await; // aborted join error is expected
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
    }

    // ---- resolve_line_start must respect the interactive budget (batch 3 (2026-07-23), finding
    // #1): the probe fixture -- a single 10,000-byte line (no `\n` anywhere), "needle" planted in
    // its own last 6 bytes, origin 10 bytes before EOF -- makes leg 1's own match scan resolve
    // `Found` synchronously, within its very first (small) read, while the line-start hunt this
    // triggers has the WHOLE file behind it to search backward through before it can prove
    // `Anchor::TOP`. Pre-fix, `resolve_line_start`'s own tight `loop { scan.step(..).await? }`
    // had no yield point in it at all (unlike every other resumable scan's own `complete`, which
    // yields once per chunk) and so ran that ENTIRE backward hunt to completion inside the same
    // poll that found the match -- 157 block reads (10,000 / 64, block_size == nav_scan_budget
    // here), no progress published, no cancellation possible, all before `search_next` had even
    // returned once. ----

    #[tokio::test]
    async fn search_next_resolve_line_start_respects_the_budget() {
        let mut data = vec![b'x'; 10000];
        data[9994..10000].copy_from_slice(b"needle");
        let src = Arc::new(MockSource::new(data));
        let d = Document::new_unindexed(
            src.clone(),
            Config {
                block_size: 64,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let resolution = d.search_next(9990, &needle_pattern(), true).await.unwrap();
        let Resolution::Pending(mut p) = resolution else {
            panic!(
                "expected Pending -- the line-start hunt (up to 9993 bytes) cannot fit in the \
                 64-byte budget leg 1's own match scan (10 bytes) left behind; read_count so far: \
                 {}",
                src.read_count()
            );
        };
        assert_eq!(p.label, "searching");
        // budget-derived bound, not a magic number: leg 1's own match confirmation touches
        // exactly one block (the file's own last, short block, read once via the ctx seed and
        // reused for the payload fetch -- `SearchForward::step`'s own `seed_block` reuse, batch 3
        // (2026-07-23) finding #7), and the ONE interactive line-start step this fix adds touches
        // at most two more (the same last block, already cached, plus one fresh block it spills
        // into under "block granularity" -- this module's own doc comment). Nowhere near the
        // pre-fix ~157 (10,000 bytes / 64-byte chunks) this replaces.
        assert!(
            src.read_count() <= 4,
            "synchronous reads must stay budget-bounded, not proportional to the file's own \
             size: {} reads",
            src.read_count()
        );
        match (&mut p.handle).await.unwrap().unwrap() {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 9994);
                assert_eq!(top, Anchor::TOP, "no newline anywhere in this fixture");
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_resolve_line_start_pending_cancels_cleanly() {
        // same fixture as the budget test above (leg 1 resolves Found synchronously; the
        // line-start hunt itself is what pends) -- the gate stays disarmed through the whole
        // synchronous portion (so leg 1's own reads proceed freely) and is armed only afterward,
        // catching the BACKGROUND hunt's own next read genuinely in-flight before cancelling.
        let mut data = vec![b'x'; 10000];
        data[9994..10000].copy_from_slice(b"needle");
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new_unindexed(
            src.clone(),
            Config {
                block_size: 64,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.search_next(9990, &needle_pattern(), true).await.unwrap()
        else {
            panic!("expected Pending -- see search_next_resolve_line_start_respects_the_budget");
        };
        let mut started = src.started_events();
        let baseline = *started.borrow();
        src.arm_gate();
        wait_for_count(&mut started, |n| n > baseline).await;
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        p.cancel();
        let _ = p.handle.await; // aborted join error is expected
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
    }
    #[tokio::test]
    async fn search_next_wrap_leg_pending_hunt_preserves_the_wrapped_flag() {
        // review correction (batch 3 (2026-07-23) unit B): this report's first draft claimed the
        // `wrapped: true` pending-conversion path through `resolve_found` was structurally
        // unreachable, reasoning that leg 2's own origin is pinned at 0, so a match it can find
        // "quickly" must sit close to 0, making its own line-start hunt cheap too. WRONG --
        // "quickly" means "within one budget-sized step", not "few bytes scanned": on a file whose
        // remaining span is itself comparable to the budget, one step can scan the WHOLE thing,
        // finding a match arbitrarily far from 0, while the corresponding backward hunt runs
        // against whatever budget leg 2's OWN scan left over -- hunt cost grows with `match_at`
        // while remaining budget shrinks, crossing near `match_at ~= budget / 2`.
        //
        // Here: leg 1 ([55, 60)) finds nothing and ends almost for free. Leg 2 (the wrap,
        // scanning from 0) finds "needle" at 40 within its own first `step()` -- the file's
        // remaining 60 bytes fit the 64-byte budget in one call -- but, since a match this close
        // to a small file's own EOF cannot be accepted without reading all the way to that EOF
        // (`SearchForward::step`'s own `safe_to`/`MAX_MATCH_LEN` margin, batch 3 (2026-07-23)
        // finding #2: this file has no room to spare that margin before EOF), leg 2 has already
        // spent nearly the WHOLE budget just confirming it, leaving too little for the 39-byte
        // backward hunt (`match_at` 40 down to 0, no `\n` anywhere) at block_size-8 granularity.
        let mut data = vec![b'x'; 60];
        data[40..46].copy_from_slice(b"needle");
        let d = Document::new_unindexed(
            Arc::new(MockSource::new(data)),
            Config {
                block_size: 8,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.search_next(55, &needle_pattern(), true).await.unwrap()
        else {
            panic!(
                "expected Pending -- leg 2's own line-start hunt must exceed the budget its own \
                 match confirmation left behind"
            );
        };
        assert_eq!(p.label, "searching");
        match p.handle.await.unwrap().unwrap() {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 40);
                assert_eq!(top, Anchor::TOP, "no newline anywhere in this fixture");
                assert!(
                    wrapped,
                    "found only by leg 2, the wrap -- the flag must survive the Pending \
                     conversion `resolve_found` performs"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }

    // ---- spawn_wrapped_pending (search final review, TRIAGE 5): distinct from `search_next_
    // pending_covers_both_legs` above, which pends on LEG 1 itself (`spawn_search_pending`
    // handles both legs in one task from there) -- these target the NARROWER path reached only
    // when leg 1 resolves synchronously (`End`, within budget) and THEN leg 2, the wrap, is the
    // one that exceeds budget on its own first `step` (`search_wrapped_leg`'s own `More` arm). ----

    #[tokio::test]
    async fn spawn_wrapped_pending_finds_a_far_match_after_leg_1_ends_synchronously() {
        // origin (990) sits 10 bytes from EOF (1000) -- leg 1's own short span ends
        // synchronously within the 16-byte budget, no match. "needle" sits far from leg 2's own
        // start (byte 0), well past what a 16-byte-budget first step can reach, forcing leg 2
        // itself to pend -- `spawn_wrapped_pending`, not `spawn_search_pending`.
        let mut data = vec![b'x'; 1000];
        data[500..506].copy_from_slice(b"needle");
        let d = Document::new_unindexed(
            Arc::new(MockSource::new(data)),
            Config {
                block_size: 64,
                nav_scan_budget: 16,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.search_next(990, &needle_pattern(), true).await.unwrap()
        else {
            panic!("expected Pending -- leg 2's own first step must exceed the 16-byte budget");
        };
        match p.handle.await.unwrap().unwrap() {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 500);
                assert_eq!(top, Anchor::TOP, "no real newline anywhere in this fixture");
                assert!(wrapped, "found only by leg 2, the wrap");
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn spawn_wrapped_pending_exhausts_when_leg_2_also_finds_nothing() {
        // identical shape to the test above, minus the needle: leg 1 still ends synchronously,
        // leg 2 still pends on its own first step, but now finds nothing anywhere either --
        // `spawn_wrapped_pending`'s own `End => Exhausted` arm, not leg 1's own (different) End
        // -> search_wrapped_leg path.
        let data = vec![b'x'; 1000];
        let d = Document::new_unindexed(
            Arc::new(MockSource::new(data)),
            Config {
                block_size: 64,
                nav_scan_budget: 16,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(p) = d.search_next(990, &needle_pattern(), true).await.unwrap()
        else {
            panic!("expected Pending -- leg 2's own first step must exceed the 16-byte budget");
        };
        match p.handle.await.unwrap().unwrap() {
            NavOutcome::Exhausted => {}
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_self_hit_finds_the_earlier_match_instead() {
        // origin sits exactly at the SECOND "needle"'s own start; backward
        // search must not re-report that one (SearchBackward's own
        // exclusive `hi` -- "not including hi" -- achieves strictly-below
        // with no extra arithmetic here), landing on the earlier one at 0.
        let d = doc(b"needle\nxxx\nneedle\n", 1 << 20);
        let outcome = d
            .search_next(11, &needle_pattern(), false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 0, "must skip the match AT the origin");
                assert_eq!(top, Anchor::TOP);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_self_hit_wraps_to_find_it_again() {
        // origin sits exactly at the ONLY "needle" in the file: leg 1
        // (strictly below origin) correctly excludes it and finds nothing,
        // so the wrap must go all the way around and land back on the same
        // match -- vim's own single-match wraparound behavior, not a
        // spurious Exhausted.
        let d = doc(b"xxx\nneedle\n", 1 << 20);
        let outcome = d
            .search_next(4, &needle_pattern(), false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 4);
                assert_eq!(top, Anchor::at(4));
                assert!(wrapped, "the only match must be found via the wrap");
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_forward_origin_is_inclusive() {
        // "at-or-after": unlike backward's exclusive `hi`, forward's origin
        // is the caller's own advance point -- a match starting EXACTLY at
        // origin must be found, not skipped (the caller is responsible for
        // passing `current_match + 1` when advancing past a known match).
        let d = doc(b"xxx\nneedle\n", 1 << 20);
        let outcome = d
            .search_next(4, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 4);
                assert_eq!(top, Anchor::at(4));
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_forward_wrap_finds_a_match_straddling_the_origin() {
        // "needle" spans [10, 16); origin (13) falls INSIDE it. Leg 1
        // (forward from 13) can only ever see "dle" -- not a match. Leg 2
        // must widen its own read past origin (to origin + MAX_MATCH_LEN)
        // to CONFIRM this match at all -- a naive `limit = origin` would
        // truncate the read exactly like leg 1's own bound did, and this
        // match would be missed entirely, not just misattributed.
        let mut data = vec![b'z'; 10];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'z'; 5]);
        let d = Document::new(
            Arc::new(MockSource::new(data)),
            Config {
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let outcome = d
            .search_next(13, &needle_pattern(), true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 10);
                assert_eq!(top, Anchor::TOP);
                assert!(wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_finds_a_straddler_via_leg_1_not_the_wrap() {
        // "needle" spans [5, 11); origin (8) falls INSIDE it. Before batch 3 (2026-07-23),
        // finding #3, leg 1 (backward, hi = origin) could only ever see "zzzzznee" -- not a
        // match, and structurally could not see more (its own `hi` bounded every byte it would
        // ever read) -- so only leg 2's OWN widened floor could confirm it, and `wrapped` was
        // `true`. Post-#3, `SearchBackward` reads PAST its own `hi` as lookahead while still
        // only ACCEPTING starts below it (that struct's own doc comment), so leg 1 now confirms
        // this straddler entirely on its own: the match is unchanged, but `wrapped` flips to
        // `false` (this IS the legitimate semantics `wrapped_leg`'s own doc comment now
        // describes, not a regression -- `search_next_backward_mirrors`, this file's own test,
        // still separately pins the genuine-wrap case for a match starting AT-OR-PAST origin).
        let mut data = vec![b'z'; 5];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'z'; 10]);
        let d = Document::new(
            Arc::new(MockSource::new(data)),
            Config {
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let outcome = d
            .search_next(8, &needle_pattern(), false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 5);
                assert_eq!(top, Anchor::TOP);
                assert!(!wrapped, "leg 1 itself now confirms the straddler");
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_zero_width_dollar_agrees_with_the_highlighter_and_the_sweep() {
        // batch 3 (2026-07-23), finding #9, end to end: "$" over unterminated "abc" has one
        // match, the zero-width position 3 (the file's own true end) -- `find_all` (the
        // highlighter's own underlying match source) already saw this correctly (probe-
        // verified: it reports `(3, 3)`); nav and the sweep must now agree, closing the
        // highlight-vs-count/nav contradiction this finding names. Deliberately NOT asserted via
        // `viewport`'s own rendered `marks[i].spans`: `layout_row_with_marks` (line.rs) drops
        // zero-width byte ranges on its own, unrelated purpose (`m.start >= m.end` is skipped
        // there) -- there is no CELL for a zero-width match to visually highlight, a rendering
        // choice orthogonal to whether the match is COUNTED at all, which is what this finding
        // is about; `find_all` itself, called directly here exactly as the highlighter's own
        // `render_row` does, is the right layer to pin the agreement against.
        let d = doc(b"abc", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(b"abc"),
            vec![(3, 3)],
            "the highlighter's own underlying match source"
        );
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(match_at, 3, "nav must agree with the highlighter");
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
        let generation = d.start_search_sweep(pattern);
        let mut rx = d.search_summary();
        let summary = wait_for_summary(&mut rx, |s| s.generation == generation && s.done).await;
        assert_eq!(
            summary.matches, 1,
            "the sweep must agree too, not silently count 0"
        );
    }
    #[tokio::test]
    async fn search_next_finds_caret_dollar_on_an_empty_file() {
        // the degenerate empty-file case, end to end: position 0 is simultaneously the file's
        // own true start and true end -- `^$` must match there, zero-width, not report Exhausted.
        let d = doc(b"", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 0);
                assert_eq!(top, Anchor::TOP);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_wraps_to_find_a_zero_width_dollar_at_eof() {
        // batch 3 (2026-07-23), finding #9's own wrap interaction, derived: origin (1) sits
        // well before the file's own true end, and leg 1 (backward, `[0, origin)`) has nothing
        // to find at all in this unterminated, newline-free file -- the only "$" anywhere is the
        // zero-width position 3, findable only by the far leg's own terminal check (`hi =
        // size`). Leg semantics stay sane: a genuine wrap, not a spurious Exhausted or a panic.
        let d = doc(b"abc", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let outcome = d
            .search_next(1, &pattern, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(match_at, 3);
                assert!(
                    wrapped,
                    "leg 1 has nothing in [0, origin); only the wrap finds it"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_self_hit_at_eof_is_found_directly_not_via_the_wrap() {
        // the OTHER wrap-interaction shape, deliberately distinct from the genuine-wrap test
        // above: the cursor is ALREADY parked exactly on the zero-width "$" at EOF (as if `n`/`N`
        // just landed there) and repeats the SAME backward search. For every other match shape,
        // leg 1's own exclusive `hi` excludes this self-hit, forcing a genuine wrap
        // (`search_next_backward_self_hit_wraps_to_find_it_again`, this file's own test, pins
        // that for a NORMAL match) -- a zero-width match at EOF is immune to that (`SearchBackward
        // ::step`'s own doc comment: `hi == cache.size()` can't tell leg 1 from leg 2, and the
        // landing position is identical either way, so this is accepted rather than threading a
        // new "which leg" parameter through every call site). Pinned here as the ACCEPTED shape,
        // not a bug: `wrapped: false`, not `true` -- a cosmetic difference (no wrap notice),
        // never a wrong landing position.
        let d = doc(b"abc", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let outcome = d
            .search_next(3, &pattern, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 3, "still the same, correct position");
                assert_eq!(top, Anchor::TOP);
                assert!(
                    !wrapped,
                    "accepted residual -- see SearchBackward::step's own comment"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }

    // ---- background match-summary sweep (search v1, task 5) ----

    /// Waits for a `SearchSummary` satisfying `pred`, mirroring `StatusWorker`'s own internal
    /// `wait_for` test helper (`status.rs`) -- `SearchSummary` is `Clone`, not `Copy` (it holds
    /// an `Arc` bucket histogram), hence `.clone()` rather than a bare deref.
    async fn wait_for_summary(
        rx: &mut tokio::sync::watch::Receiver<crate::search::SearchSummary>,
        pred: impl Fn(&crate::search::SearchSummary) -> bool,
    ) -> crate::search::SearchSummary {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snap = rx.borrow().clone();
                if pred(&snap) {
                    return snap;
                }
                rx.changed()
                    .await
                    .expect("the sweep driver dropped its sender before resolving");
            }
        })
        .await
        .expect("the sweep never reached the expected state")
    }

    #[tokio::test]
    async fn sweep_counts_matches_and_fills_buckets() {
        // three "needle" matches at known offsets (0, 11, 22); the sweep must find exactly
        // three and place each start in the bucket its own position maps to.
        let d = doc(b"needle aaa needle bbb needle ccc\n", 1 << 20);
        let generation = d.start_search_sweep(needle_pattern());
        let mut rx = d.search_summary();
        let summary = wait_for_summary(&mut rx, |s| s.generation == generation && s.done).await;
        assert_eq!(summary.matches, 3);
        assert!(!summary.failed);
        assert_eq!(summary.scanned_up_to, d.size());
        assert_eq!(
            summary.buckets.iter().map(|&n| n as u64).sum::<u64>(),
            3,
            "every recorded match must land in exactly one bucket"
        );
    }

    #[tokio::test]
    async fn sweep_summary_is_generation_stamped_and_supersedes() {
        // pattern A ("needle", once) is gated genuinely mid-read when pattern B ("zzz", twice)
        // supersedes it; B's own match count (2, not A's 1) is what proves the driver actually
        // switched analyses rather than merely relabeling A's own in-flight result.
        //
        // batch 3 (2026-07-23), finding #15b: the OLD shape opened the gate immediately after
        // submitting B, then waited only for B's own DONE snapshot -- proving nothing about WHEN
        // the switch happened. A driver that checked for a new request only AFTER A's own step()
        // returned (never racing the two) would, once the gate opened, let A's read complete,
        // finish A, THEN notice B and run it to completion -- reaching the identical terminal
        // state (generation B, done, 2 matches) this test asserted, indistinguishable from real
        // preemption. Fixed by inserting a POSITIVE wait, with the gate still CLOSED, for the
        // driver's own begin(B) snapshot to land on the summary watch before ever opening it: an
        // (unwatched) driver that only reacts to a new request after A's own step() returns could
        // never publish this while A's sole read stays parked forever on the still-closed gate --
        // the wait would time out loudly instead of passing. Happens-before chain this relies on
        // (verified by reading, not assumed): `crate::analyzer::spawn`'s own loop calls
        // `analysis.begin(&req)` and sends its snapshot BEFORE the inner `select!` that races a
        // fresh request against `analysis.step()` even runs once -- so B's begin-snapshot needs
        // only the request-changed signal to arrive, never A's own gated read to resolve.
        let mut data = vec![b'x'; 4096];
        data.extend_from_slice(b"needle\n");
        data.extend_from_slice(b"zzz zzz\n");
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 64,
                cache_bytes: 4 * 64,
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
        let gen_a = d.start_search_sweep(needle_pattern());
        let mut rx = d.search_summary();
        let mut started = src.started_events();
        let baseline = *started.borrow();
        wait_for_count(&mut started, |n| n > baseline).await;
        let zzz_pattern = Arc::new(SearchPattern::compile("zzz", false).unwrap());
        let gen_b = d.start_search_sweep(zzz_pattern);
        assert!(gen_b > gen_a);
        // the gate is STILL CLOSED here, and A's sole read is still parked on it -- only genuine
        // preemption (the driver's own `select!` picking the request-changed branch over the
        // in-flight, gated step) can publish generation B's own begin-snapshot before this
        // resolves. See this test's own comment above for the happens-before chain and what a
        // non-preempting driver could never do here.
        wait_for_summary(&mut rx, |s| s.generation == gen_b).await;
        src.open_gate();
        let summary = wait_for_summary(&mut rx, |s| s.generation == gen_b && s.done).await;
        assert_eq!(
            summary.matches, 2,
            "B's own match count, not A's -- proves the driver switched analyses, not just labels"
        );
        // POSITIVE proof, not silence: the channel's CURRENT value, read right after observing
        // B's own terminal snapshot, is still generation B -- if A's own abandoned step() had
        // somehow kept running and publishing (a supersession bug), a LATER A-generation
        // snapshot landing after B's own done snapshot would show up here, since a `watch`
        // channel always holds the most recently sent value regardless of arrival order.
        assert_eq!(rx.borrow().generation, gen_b);
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_gives_up_and_floors_on_a_failing_block() {
        // mirrors status.rs's own error-shortened-scan fixture: block 0 reads fine (one
        // "needle"), everything after it fails forever -- the sweep must retry, then give up
        // and float a partial-but-honest floor rather than lose block 0's own already-found
        // match or hang retrying forever.
        struct FailsAfterFirstBlock;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsAfterFirstBlock {
            fn size(&self) -> u64 {
                16384
            }
            async fn read_block(&self, offset: u64, _len: usize) -> anyhow::Result<bytes::Bytes> {
                if offset == 0 {
                    let mut block = vec![b'x'; 8192];
                    block[0..6].copy_from_slice(b"needle");
                    Ok(bytes::Bytes::from(block))
                } else {
                    Err(anyhow::anyhow!("boom"))
                }
            }
        }
        let d = Document::new(
            Arc::new(FailsAfterFirstBlock),
            Config {
                block_size: 8192,
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
        let generation = d.start_search_sweep(needle_pattern());
        let mut rx = d.search_summary();
        // `start_paused = true` auto-advances straight through the sweep's own retry backoff --
        // see `analyzer.rs`'s own `retries_back_off_then_give_up_moves_on` for why this, not a
        // manual `advance()` loop, is the reliable mechanism for a chain of
        // dependently-scheduled timers.
        let summary = wait_for_summary(&mut rx, |s| s.generation == generation && s.done).await;
        assert!(
            summary.failed,
            "a permanently failing block must be reflected as failed"
        );
        assert!(
            summary.matches >= 1,
            "block 0's own match must survive a later block's failure, not be discarded"
        );
    }

    #[tokio::test]
    async fn dropping_the_document_cancels_the_sweep() {
        // same shape as this module's own `dropping_the_document_stops_the_status_worker`
        // above (`request_line_number`'s twin): a positive `cancelled_events` proof that the
        // drop cascade reaches the sweep driver's own in-flight read, not a bounded silence
        // window.
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        let src = Arc::new(MockSource::new(data).with_gate());
        let d = Document::new(
            src.clone(),
            Config {
                block_size: 64,
                cache_bytes: 4 * 64,
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
        let mut started = src.started_events();
        let baseline = *started.borrow();
        d.start_search_sweep(needle_pattern());
        wait_for_count(&mut started, |n| n > baseline).await;
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        drop(d);
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
    }

    #[tokio::test]
    async fn abort_background_and_join_reaches_the_search_sweep_too() {
        // the fourth owner (search final review): `abort_background_and_join` claims it awaits
        // EVERY background owner's own teardown, not just requests it -- unlike `drop`'s own
        // fire-and-forget cascade (`dropping_the_document_cancels_the_sweep`, above, needs its
        // own `wait_for_count` poll for exactly this reason), a genuine await-until-stopped
        // means `cancelled_events` must already be past baseline the INSTANT this call returns,
        // no polling needed.
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        let src = Arc::new(MockSource::new(data).with_gate());
        let mut d = Document::new(
            src.clone(),
            Config {
                block_size: 64,
                cache_bytes: 4 * 64,
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
        let mut started = src.started_events();
        let baseline = *started.borrow();
        d.start_search_sweep(needle_pattern());
        wait_for_count(&mut started, |n| n > baseline).await;
        let cancelled_baseline = *src.cancelled_events().borrow();
        d.abort_background_and_join().await;
        assert!(
            *src.cancelled_events().borrow() > cancelled_baseline,
            "abort_background_and_join must not return before the sweep's own in-flight read \
             has actually been cancelled"
        );
    }

    // ---- match highlighting (search v1, task 7) ----

    #[tokio::test]
    async fn viewport_marks_default_to_empty_with_no_active_search() {
        let d = doc(b"xxfooxx\n", 1 << 20);
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        assert_eq!(v.marks, vec![RowMarks::default()]);
    }
    #[tokio::test]
    async fn viewport_finds_match_spans_at_the_right_columns() {
        let d = doc(b"xxfooxx\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["xxfooxx".to_string()]);
        assert_eq!(v.marks[0].spans.len(), 1);
        assert!(v.marks[0].spans.contains(&(2..5)), "{:?}", v.marks[0].spans);
        assert_eq!(
            v.marks[0].current, None,
            "no active target means no row is 'current'"
        );
    }
    #[tokio::test]
    async fn viewport_marks_the_current_match_by_absolute_byte_offset() {
        // two identical lines, so only the ABSOLUTE offset (top.offset() + line_start + s), not
        // the pattern or the in-line column, can distinguish which one is "current".
        let d = doc(b"xxfooxx\nxxfooxx\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo", false).unwrap());
        let v = d
            .viewport(
                Anchor::TOP,
                2,
                80,
                HScroll::ZERO,
                Some((&pattern, Some(10))),
            )
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["xxfooxx".to_string(), "xxfooxx".to_string()]);
        assert!(v.marks[0].spans.contains(&(2..5)));
        assert_eq!(
            v.marks[0].current, None,
            "row 0's match (absolute start 2) is not the active one"
        );
        assert!(v.marks[1].spans.contains(&(2..5)));
        assert_eq!(
            v.marks[1].current,
            Some(2..5),
            "row 1's match at absolute offset 10 is the active one"
        );
    }
    #[tokio::test]
    async fn viewport_current_stays_none_when_the_target_is_not_a_visible_match() {
        let d = doc(b"xxfooxx\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo", false).unwrap());
        let v = d
            .viewport(
                Anchor::TOP,
                1,
                80,
                HScroll::ZERO,
                Some((&pattern, Some(999))),
            )
            .await
            .unwrap();
        assert!(v.marks[0].spans.contains(&(2..5)));
        assert_eq!(v.marks[0].current, None);
    }
    #[tokio::test]
    async fn search_marks_read_no_extra_blocks() {
        // find_all/layout_row_with_marks run over the SAME buffer fill_lines already read for
        // `.rows` -- an active search must read exactly as many blocks as a plain, search-less
        // viewport call over an identical fresh document, never one more.
        let data = b"xxfooxx\nxxbarxx\n".to_vec();
        let plain_src = Arc::new(MockSource::new(data.clone()));
        let plain = Document::new_unindexed(
            plain_src.clone(),
            Config {
                block_size: 4,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let _ = plain
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, None)
            .await
            .unwrap();
        let search_src = Arc::new(MockSource::new(data));
        let searched = Document::new_unindexed(
            search_src.clone(),
            Config {
                block_size: 4,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let pattern = Arc::new(SearchPattern::compile("foo", false).unwrap());
        let _ = searched
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(plain_src.read_count(), search_src.read_count());
    }

    // ---- multi-row highlighting (batch 3 (2026-07-23), finding #10): a per-row `find_all`
    // cannot see a match containing its own row's newline -- no single row's own byte slice ever
    // holds both halves. `find_all` now runs ONCE over the whole viewport buffer (which, unlike
    // any row's own slice, still contains the newlines), and each row clips the same absolute
    // match list to its own bounds. ----

    #[tokio::test]
    async fn viewport_highlights_a_match_spanning_two_rows() {
        let d = doc(b"xxfoo\nbarzz\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo\nbar", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["xxfoo".to_string(), "barzz".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![2..5],
            "row 0's own tail (\"foo\") carries the match's own first half"
        );
        assert_eq!(
            v.marks[1].spans,
            vec![0..3],
            "row 1's own head (\"bar\") carries the match's own second half"
        );
    }
    #[tokio::test]
    async fn viewport_highlights_a_match_spanning_three_rows() {
        let d = doc(b"xxfoo\nbar\nbazyy\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo\nbar\nbaz", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 3, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec!["xxfoo".to_string(), "bar".to_string(), "bazyy".to_string()]
        );
        assert_eq!(v.marks[0].spans, vec![2..5], "row 0's own tail");
        assert_eq!(
            v.marks[1].spans,
            vec![0..3],
            "row 1 sits entirely inside the match"
        );
        assert_eq!(v.marks[2].spans, vec![0..3], "row 2's own head");
    }

    // ---- the current match must always be findable and visible (batch 3 (2026-07-23), finding
    // #11): (a) `find_all`'s own non-overlapping walk can skip straight over a start `n`/`N`
    // legitimately lands on; (b) a zero-width current match used to be dropped by
    // `layout_row_with_marks` outright (line.rs's own fix, above) -- covered end to end here. ----

    #[tokio::test]
    async fn viewport_finds_the_current_match_even_when_find_all_would_skip_its_start() {
        // "aa" over "aaa" has only ONE non-overlapping `find_all` match, (0,2) -- but `n`
        // navigation can legitimately land the cursor on the second, OVERLAPPING occurrence
        // starting at 1 (`search_next`'s own scan sees overlapping starts `find_all` structurally
        // cannot -- `search.rs`'s own doc comment on `find_starting_in`). The old lookup
        // (`byte_marks.iter().find(|m| .. == target)`) found nothing for target 1 and silently
        // left `current` at `None` -- no BOLD, ever, for this landing.
        let d = doc(b"aaa\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("aa", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, Some(1))))
            .await
            .unwrap();
        assert_eq!(
            v.marks[0].current,
            Some(1..3),
            "the overlapping match starting at 1 must resolve, not silently stay None"
        );
        assert!(
            v.marks[0].spans.contains(&(1..3)),
            "current must stay contained in spans (BOLD is only ever painted on top of an \
             already-REVERSED cell, render.rs's own doc comment) even though find_all itself \
             never found this span: {:?}",
            v.marks[0].spans
        );
    }
    #[tokio::test]
    async fn viewport_caret_current_is_a_visible_cell() {
        // before line.rs's own fix, a zero-width current match's second, single-mark
        // `layout_row_with_marks` call always came back empty -- `current` stayed `None` even
        // though `find_starting_in` above it resolved a real span.
        let d = doc(b"abc\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("^", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, Some(0))))
            .await
            .unwrap();
        assert_eq!(
            v.marks[0].current,
            Some(0..1),
            "a zero-width current match must still render on a real, visible cell"
        );
    }
    #[tokio::test]
    async fn viewport_dollar_current_renders_after_the_last_character() {
        let d = doc(b"abc\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, Some(3))))
            .await
            .unwrap();
        assert_eq!(v.marks[0].current, Some(3..4));
    }
    #[tokio::test]
    async fn viewport_zero_width_matches_render_reversed_even_when_not_current() {
        // every line start is its own zero-width "^" match; with no active target, every row's
        // own match must still show up in `spans`, not silently vanish the way EVERY zero-width
        // match used to before line.rs's own fix.
        let d = doc(b"aaa\nbbb\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("^", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.marks[0].spans, vec![0..1]);
        assert_eq!(v.marks[1].spans, vec![0..1]);
    }

    // ---- anchored-pattern agreement (search final review): `^`/`$` are LINE anchors
    // (`crate::search::SearchPattern::compile`'s own doc comment), and every one of the three
    // independent consumers -- the nav jump (`search_next`), the background sweep (task 5), and
    // the row-slice highlighter (task 7) -- must agree on exactly which occurrences of "needle"
    // are real, line-anchored matches. ----

    #[tokio::test]
    async fn anchored_pattern_agrees_across_nav_sweep_and_highlight() {
        // three lines, three literal "needle" occurrences, but only ONE is preceded by a real
        // \n (line 1's own, right at its own start) -- the other two are mid-line ("aneedle",
        // "xneedle"), planted specifically so a haystack-anchor bug (this review's own CRITICAL
        // finding) would over-count/mis-jump/over-highlight instead of agreeing with this one.
        let data: &'static [u8] = b"aneedle bbb\nneedle ccc\nxneedle ddd\n";
        let line1_start = 12u64; // "aneedle bbb\n" is 12 bytes
        let pattern = Arc::new(SearchPattern::compile("^needle", false).unwrap());

        // (i) the nav jump lands exactly on line 1, at its own start (no earlier resolution
        // needed: the match itself IS the line start).
        let d = doc(data, 1 << 20);
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(top, Anchor::at(line1_start));
                assert_eq!(match_at, line1_start);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }

        // (ii) the background sweep counts exactly one match -- not three.
        let generation = d.start_search_sweep(pattern.clone());
        let mut rx = d.search_summary();
        let summary = wait_for_summary(&mut rx, |s| s.generation == generation && s.done).await;
        assert_eq!(
            summary.matches, 1,
            "only line 1's own \"needle\" is line-anchored"
        );
        assert!(!summary.failed);

        // (iii) the row-slice highlighter marks line 1's row and ONLY line 1's row: each row's
        // own slice starts exactly at that line's own start, so `^` is exactly line-correct
        // there (docs/search.md's own claim) -- lines 0 and 2 must show no marks at all.
        let v = d
            .viewport(Anchor::TOP, 3, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec![
                "aneedle bbb".to_string(),
                "needle ccc".to_string(),
                "xneedle ddd".to_string(),
            ]
        );
        assert!(
            v.marks[0].spans.is_empty(),
            "line 0's \"needle\" is mid-line, not anchored"
        );
        assert_eq!(
            v.marks[1].spans,
            vec![0..6],
            "line 1's own \"needle\" starts its own line"
        );
        assert!(
            v.marks[2].spans.is_empty(),
            "line 2's \"needle\" is mid-line, not anchored"
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
                let a = doc.scroll_lines(Anchor::TOP, down).await.unwrap().ready().at();
                prop_assert!(is_line_start(&data, a.offset()), "down -> {}", a.offset());
                prop_assert!(a.offset() == 0 || a.offset() < data.len() as u64);
                let b = doc.scroll_lines(a, -up).await.unwrap().ready().at();
                prop_assert!(is_line_start(&data, b.offset()), "up -> {}", b.offset());
                prop_assert!(b.offset() <= a.offset());
                let p = doc.goto_percent(pct).await.unwrap().ready().at();
                prop_assert!(is_line_start(&data, p.offset()), "pct -> {}", p.offset());
                prop_assert!(p.offset() == 0 || p.offset() < data.len() as u64);
                let e = doc.goto_end(rows).await.unwrap().ready().at();
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
                let _ = doc.viewport(Anchor::TOP, rows, cols, HScroll::new(hscroll), None).await.unwrap();
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
                let a_sync = sync_doc.scroll_lines(Anchor::TOP, down).await.unwrap().ready().at();
                let a_tiny = tiny_doc.scroll_lines(Anchor::TOP, down).await.unwrap().join().await;
                prop_assert_eq!(a_sync, a_tiny, "scroll down diverged");
                let u_sync = sync_doc.scroll_lines(a_sync, -up).await.unwrap().ready().at();
                let u_tiny = tiny_doc.scroll_lines(a_sync, -up).await.unwrap().join().await;
                prop_assert_eq!(u_sync, u_tiny, "scroll up diverged");
                let p_sync = sync_doc.goto_percent(pct).await.unwrap().ready().at();
                let p_tiny = tiny_doc.goto_percent(pct).await.unwrap().join().await;
                prop_assert_eq!(p_sync, p_tiny, "percent jump diverged");
                let e_sync = sync_doc.goto_end(rows).await.unwrap().ready().at();
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
