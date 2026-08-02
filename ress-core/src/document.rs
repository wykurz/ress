//! The document model: turns a byte offset into a screenful of display rows.
//! rendering is driven by byte offsets, not line numbers, so first paint costs
//! one block read regardless of file size.
use crate::Config;
/// Re-exported so callers outside the crate (the terminal frontend's status
/// line) can name the type `Document::index_frontier` returns; the `index`
/// module itself stays `pub(crate)`.
pub use crate::index::Frontier;
use crate::resolve::{NavOutcome, Resolution};
use crate::scan::Resumable;
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
    /// against bytes this call already fetched via `fill_lines`/`edge_context`
    /// below -- never a new read, so an active search costs this call zero extra
    /// IO -- and returns the result as `ViewportRender::marks`. `current` is the
    /// ACTIVE match's absolute byte offset (`ActiveSearch::current` in the
    /// terminal frontend); it is resolved directly against the same bytes to
    /// pick out the one row's one span that also renders BOLD.
    ///
    /// Each row's own match list is derived independently, bounded by that row's own VISIBLE
    /// window (batch 4 (2026-07-24), finding #5: `render_row`'s own `visible_window_matches`,
    /// below) -- not by one `find_all_starting_in` pass over the whole fetched `buf` (batch 3
    /// (2026-07-23), finding #10's own design, which this supersedes for cost, not correctness: a
    /// row's own window is typically a small fraction of `buf`'s own size, and pre-restructure,
    /// EVERY match anywhere in `buf` was enumerated and re-filtered once per displayed row,
    /// regardless of the window). A match containing one of `buf`'s own newlines (e.g. an
    /// explicit `\n` in the pattern) is still found: a row's own accept range expands LEFTWARD by
    /// `MAX_MATCH_LEN`, crossing as many newlines as it needs to (a flat byte range in `buf`, not
    /// a row-scoped one) -- so row j's own enumeration reaches back and finds a match whose start
    /// sits on an earlier row i, even though row i's own (correctly excluded) enumeration never
    /// found it via its own start (`visible_window_matches`'s own doc comment has the full
    /// derivation; `docs/search.md`'s "Reaching the screen" section states the cost story).
    /// `render_row`'s own `clip_to_row` clips whatever a row's own enumeration returns down to
    /// that row's own bounds, so a match spanning rows i..j still contributes a clipped mark to
    /// each -- row i's own tail, row j's own head, every row strictly between the two entirely --
    /// exactly as before, only the WAY each row's own list is computed changed.
    ///
    /// batch 4 (2026-07-24), finding #6: `buf`'s own edges are NOT treated as real line/file
    /// boundaries the way the pre-fix code implicitly did by handing `find_all`/`find_starting_
    /// in` the bare buffer alone -- `search.rs`'s own top-level doc comment names this the same
    /// "a slice's own edge is unconditionally `^`/`$`-eligible" hazard every windowed/chunked
    /// matcher elsewhere in this engine already guards against; the viewport was the one
    /// remaining caller that didn't. `edge_context` (below) fetches up to `CTX_BEHIND` real
    /// bytes immediately before `top` (when `top > 0`) and, batch 5 (2026-07-26), finding #6, up
    /// to `MAX_MATCH_LEN + CTX_AHEAD - 1` real bytes immediately after `buf` (when more of the
    /// file exists past it -- widened from a single `CTX_BEHIND`-wide fetch, which was only ever
    /// enough for a candidate whose trailing assertion lands exactly at `buf`'s own edge, not one
    /// starting further back; `edge_context`'s own doc comment has the derivation) -- up to
    /// `CTX_BEHIND` (4) `warm()` reads on the BEHIND side (fix round (2026-07-28), P3-2: batch 6
    /// finding #4 widened this from a stale "at most one," corrected here, its own multi-block
    /// gather; a straddled block boundary is the only way more than one is ever needed), a few on
    /// the AHEAD side (bounded, `edge_context`'s own doc comment), done HERE (this call already
    /// does IO via `fill_lines`), not in `render` (the draw path's own no-IO rule is otherwise
    /// unbent). Every one of these `warm()` calls bypasses the `Reader` entirely and is charged to
    /// no `Meter` at all (`edge_context`'s own body, not `meter.rs`'s metered regime) --
    /// `budgeted_scanning.md`'s own "every *other* read, hit or miss alike, costs the budget the
    /// same" (fix round, P2-1) is scoped to reads THROUGH the `Reader`; the viewport's own context
    /// fetches sit outside it entirely, bounded by their own byte caps (`CTX_BEHIND`/
    /// `MAX_MATCH_LEN + CTX_AHEAD - 1`) rather than by any `chunk`/`allowance`. Every row's own search runs over a
    /// slice of `ctx_before + buf + ctx_after` (finding #5's own per-row accept/hay, above),
    /// restricted to `buf`'s own span for what a match may START at. Its own END is NOT restricted
    /// to `buf` (fix round (2026-07-25), F2, RULED): a match starting in `buf` and completing
    /// inside real `ctx_after` bytes is genuine, so its visible prefix is highlighted --
    /// `visible_window_matches`'s own doc comment states the resulting `end > buf.len()`
    /// invariant explicitly. A match needing MORE than `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes past
    /// `buf`'s own end remains a documented miss (the cap this whole engine shares, `search.rs`'s
    /// own doc comment) -- `visible_window_matches`'s own resolved-criterion belt (its doc
    /// comment) is what enforces this, and it fires on the HOT path, not only when the source is
    /// truncated (fix round (2026-07-27), P2-3, correcting an earlier, false "only when
    /// truncated" claim here): the belt's own margin check runs for every row, and its EOF
    /// exemption is consulted whenever a row's own hay is clamped to `full_hay`'s edge, which
    /// happens for the LAST row of an ordinary, perfectly healthy viewport too (`full_hay.len()`
    /// there simply IS that clamp). A genuinely truncated source is one further, narrower way a
    /// row's own hay can land short of the margin `accept_and_hay_for_row` wants -- never a
    /// fabrication either way, both resolved by the identical belt.
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
        let (buf, fill_outcome) =
            crate::scan::fill_lines(self.cache(), top.offset(), rows, scan_budget).await?;
        // restructure R3: `FillOutcome` replaces the old 4-cause bool. `Rows` is the one variant
        // where the loop stopped for a reason OTHER than "nothing more to give this call" (budget
        // or a certified end) -- the trailing-row render below cares only about that distinction,
        // matching the pre-restructure `pos >= size || buf.len() >= budget` formula exactly (a
        // row-satisfied stop, tracked identically by `out.len() == rows` a few lines down, never
        // needed `stop`'s own value either way).
        let stop = !matches!(fill_outcome, crate::scan::FillOutcome::Rows);
        // batch 4 (2026-07-24), finding #5: `hay`/`lb` replace the old whole-buf `all_matches` --
        // each row now derives its OWN visible-window-bounded match list from these
        // (`visible_window_matches`'s own doc comment has the full derivation), rather than one
        // shared list computed once over the whole payload and filtered per row regardless of
        // its own size. `current_match` resolution is UNCHANGED (still a single, already-targeted
        // `find_starting_in` point query over the same `hay` -- see its own comment below).
        // restructure R4 (2026-07-27): the parent `Hay` this whole call's search context lives
        // in, and the bytes it borrows -- declared here, in the OUTER scope, so both survive for
        // the whole function (the per-row loop below needs a live reference to it, and a `Hay`
        // cannot be bundled into the `Option` the pre-R4 code returned from this match without
        // borrowing its own about-to-move bytes). Populated below only when `search.is_some()`;
        // otherwise stays the empty default, unused (`search_ref`, further down, stays `None`).
        // restructure R7 (batch 8 (2026-07-29)): the ASSEMBLY, not a bare byte vector plus a
        // separately-computed base. It starts empty at `top` (the no-search case, where nothing
        // is ever read from it -- `search_ref` below stays `None`) and is rebuilt around `buf`
        // the moment a search is active. `lb`/`payload_end` become readings taken FROM it rather
        // than lengths maintained alongside it.
        let mut asm =
            crate::search::hay::Assembly::anchored_at(crate::search::hay::Abs(top.offset()), &[]);
        let mut lb = 0usize;
        let mut payload_end = 0usize;
        let mut buf_is_true_eof = false;
        let mut hay_high = crate::search::hay::Edge::Cut;
        if search.is_some() {
            {
                // this inner block exists only to give `?` (below) a scope that does not also
                // have to satisfy the borrow-checker for `hay_bytes`'s own later, immutable
                // borrow (`Hay::new`, just past this whole `if`) -- a plain `if search.is_some()
                // { .. }` with no nested block would work identically; kept nested purely so a
                // reader scanning for "where does the fetch happen" sees one clearly delimited
                // unit, matching this function's own established per-phase block style elsewhere
                // (`edge_context`'s own two arms, `resolve_found`'s own phases).
                // batch 4 (2026-07-24), finding #6: real boundary context, not slice edges --
                // this struct's own doc comment on `viewport` has the full derivation. Fetched
                // only when a search is actually active, so a plain (no-search) viewport pays
                // nothing extra.
                let (ctx_before_at, ctx_before, ctx_after, ahead_hit_real_end) =
                    self.edge_context(top.offset(), buf.len()).await?;
                // restructure R7 (batch 8 (2026-07-29)): assembled outward from `buf`, the run
                // this viewport is actually rendering, with each margin offered at the position
                // it was READ FROM -- `ctx_before_at` as the walk reported it, and `ctx_after`
                // at `buf`'s own end, where its gather starts. Neither can legitimately be
                // refused (both gathers are anchored at a `buf` edge and only ever truncate away
                // from it), which is the point of checking rather than a reason not to: `lb`
                // below is now a consequence of what the assembly ACCEPTED, so it cannot drift
                // from the bytes it is supposed to measure the way a separately-computed length
                // could.
                asm = crate::search::hay::Assembly::anchored_at(
                    crate::search::hay::Abs(top.offset()),
                    &buf,
                );
                let _ = asm.extend_below(ctx_before_at, &ctx_before);
                let _ = asm.extend_above(
                    crate::search::hay::Abs(top.offset() + buf.len() as u64),
                    &ctx_after,
                );
                lb = asm.local_of(crate::search::hay::Abs(top.offset())).0;
                payload_end = asm
                    .local_of(crate::search::hay::Abs(top.offset() + buf.len() as u64))
                    .0;
                // batch 5 (2026-07-26), findings #6/#7: is `buf`'s own physical edge the file's
                // TRUE end -- not merely where this call's own scan budget happened to stop?
                // `edge_context`'s own `ctx_after` is fetched only when more real file exists past
                // `buf` (its own `buf_end < self.size` gate), so whenever this holds, `ctx_after`
                // is empty and `hay.len() == payload_end` exactly.
                let buf_end = top.offset() + buf.len() as u64;
                // restructure R3, fix round (P1-1): TWO disjuncts, not one, after re-deriving
                // this from scratch (the original R3 pass replaced the size check outright,
                // which both fabricated a synthetic EOF match on a legitimate non-EOF short read
                // -- the adversarial review's own P1-1 finding -- AND, in overcorrecting, dropped
                // a case the size check always handled safely).
                //
                // `matches!(fill_outcome, FillOutcome::End(_))`: a witness certified by a
                // GENUINELY empty read, at its own position (`fill_lines`'s own doc comment has
                // the P1-1 law: only an observed EMPTY read may certify, never a merely-short
                // block's own LENGTH). Handles a real end that does not fall on a block boundary
                // AND whose containing block is short-but-nonempty. The empty read may reach
                // `fill_lines` either directly or as the certificate a short block carries
                // (`Block::ends_data`, minted by the cache's own completion read one layer down --
                // batch 22 (2026-08-01): this used to say `Budget`, not `End`, followed EVERY short
                // block, which stopped being true in batch 14). What still reports `Budget` is a
                // short block with no such observation behind it, since the block-indexed cache
                // cannot verify whatever might sit in the gap between that block's own short
                // answer and the next block's own start (P1-1's own point).
                //
                // `buf_end == self.size`: the ORIGINAL, pre-restructure check, restored as an
                // ADDITIONAL disjunct, not dropped -- it is safe on its own terms, independent of
                // whether `size()` itself is accurate: `buf` can only ever PHYSICALLY reach
                // `self.size` if nothing shorter stopped it first, so a truncated source (`size()`
                // overstating reality) never lets `buf_end` reach the inflated claim at all (the
                // real, honest data runs out first) -- this disjunct is simply unreachable in
                // that case, never wrongly true. It is what an ORDINARY, accurately-sized source
                // needs: a small file entirely within one block still reports `Short` (fewer
                // bytes than a full block's own capacity), which no longer certifies on its own
                // (P1-1) -- but `size()` being honest here is exactly the case this project's own
                // established model already trusted (`edge_context`'s own P2-1 predates this
                // restructure entirely), and `viewport_eof_zero_width_dollar_renders_ordinarily_
                // not_only_when_current` (batch 5, unit H) pins that trust as protected,
                // unmovable behavior.
                buf_is_true_eof = matches!(fill_outcome, crate::scan::FillOutcome::End(_))
                    || buf_end == self.size;
                // Fix round (2026-07-27), P3-6, answering M10 (the one round-1 item this unit's
                // own report left without a response): is `buf_is_true_eof`'s own presence in
                // `eof_zero_width_ok` load-bearing against fabrication?
                //
                // **Correction (fix round (2026-07-27), P2-FINAL):** the answer first shipped here
                // was NO, reasoned from a two-way enumeration -- whenever `ctx_after` is empty,
                // `buf_hi` is ALREADY verified as the genuine real end, either because `buf_is_
                // true_eof` itself holds or because `edge_context`'s own `ahead_hit_real_end` does.
                // **False, disproved by construction, not merely re-argued** (this project's own
                // established style: correct in place with a stated retraction, not a silent edit).
                // That enumeration has only two cases and misses a THIRD way `ctx_after` comes back
                // empty: the AHEAD-side loop's first touch returns `take == 0` from a block that is
                // SHORT but NONEMPTY -- `edge_context`'s own `block.is_empty()` check (P2-1, this
                // same unit's own earlier fix round) correctly refuses to certify there, so `ahead_
                // hit_real_end` is `false`; and `self.size` can be perfectly accurate while `buf`
                // still stops well short of it, so `buf_is_true_eof` is `false` too. The enumeration
                // was written against the PRE-P2-1 model, in the very commit that changed the model:
                // before P2-1, ANY short read certified, so two cases were the whole story; P2-1
                // made a short-but-nonempty touch refuse to certify, opening exactly this gap.
                //
                // So: YES, `buf_is_true_eof` IS load-bearing -- it is the ONLY thing standing
                // between this third case and a fabrication. Disproved by construction
                // (`viewport_buf_is_true_eof_gate_blocks_a_synthetic_eof_match_at_an_uncertified_
                // cut`, below, credited, adopted from the closing-round reviewer): a fully intact
                // 15-byte source (`size()` accurate, not truncated) whose block 0 legally answers
                // with 5 of its own 8 bytes -- a conforming "up to `len`" answer (`source.rs`), not
                // a bug. `buf` is `b"hello"`, position 5 is a real `'X'`, and the whole-file oracle
                // finds `$` only at 15. Dropping `buf_is_true_eof` here (mutant M10) fabricates a
                // `$` at 5 anyway, and the full 447-test suite stays green regardless -- the exact
                // "a comment argues safety, the guard is deleted, nothing objects" failure this
                // batch's own method exists to catch.
                //
                // The probe below is still valid, but only for the branch it actually reaches: when
                // `ctx_after` is non-empty, the synthetic check (below) resolves against REAL bytes
                // regardless of this gate, unaffected either way; and when `ctx_after` IS empty AND
                // `ahead_hit_real_end` holds (confirmed with `real: b"ax"`, `claimed_size: 100_000`,
                // `block_size: 2` -- block-aligned, so `ahead_hit_real_end` certifies), dropping
                // `buf_is_true_eof` turns a documented MISS (`spans: []`) into agreement with nav
                // (`spans: [2..3]`, matching `FoundMatch { match_at: 2 }`) -- strictly MORE
                // permissive there, never a fabrication. That probe is co-fitted to the two-case
                // model: block-aligned, so `ahead_hit_real_end` is the certifying disjunct, and the
                // missing third case (neither disjunct true) is never visited. The missing branch is
                // DEFINED by certification failing, so no block-aligned probe can ever reach it.
                //
                // The related, NOT-yet-taken opportunity (`buf_is_true_eof || ahead_hit_real_end`,
                // the SAME widening P2-NEW already applied to `hay_reaches_eof`) is MORE attractive
                // now that the enumeration above is corrected: it would close the P2-NEW-documented
                // miss WITHOUT touching the guard that stops this fabrication, since the two
                // conditions are independent (this comment's own disproof fixture has `ahead_hit_
                // real_end` false throughout, so the OR would not admit it there). Still flagged
                // only, deliberately left for a future round to keep this one minimal (team lead's
                // own framing: "small, as the reviewer scoped it"). Carried, not dropped, per the
                // reviewer's own allowance.
                // finding #6's own resolved-criterion belt: does `hay` (this call's own fetched
                // context, not any one row's narrower slice of it) reach a TRUE end -- one this
                // engine may act on with no further margin? Three ways: `buf_end + ctx_after.len()
                // == self.size` (the ordinary case -- `self.size` is accurate and the AHEAD-side
                // fetch read every real byte up to it), OR `edge_context`'s own `ahead_hit_real_
                // end` (a genuinely observed EMPTY read VERIFIES that position as the real end
                // regardless of what `self.size` claims -- `edge_context`'s own doc comment has
                // the derivation: NOT the identical trust `SearchForward` places in ANY short/
                // empty read, narrower, and only reachable in practice when the source's real end
                // happens to fall on a block boundary), OR `buf_is_true_eof` (restructure R3,
                // fix round P2-D: `buf_is_true_eof` is itself TWO disjuncts now, not one witness --
                // see that field's own doc comment above -- so this third disjunct's own soundness
                // splits the same way. When it holds via `FillOutcome::End(_)`, `fill_lines` has
                // genuinely, directly verified nothing more real exists past `buf_end` (only an
                // actually-empty read certifies, at its own position, restructure R3's own P1-1
                // law -- never inferred from a merely-short block's own boundary), so `edge_context`'s
                // own SEPARATE, later fetch for `ctx_after` finds nothing there either (the same
                // source, queried moments apart) -- `ctx_after` is empty, and `hay`'s own end
                // coincides with `buf_end`, the just-certified true end. When it holds via
                // `buf_end == self.size` instead, this third disjunct adds nothing NEW -- the
                // second disjunct just above (`buf_end + ctx_after.len() == self.size`) already
                // covers it: `edge_context` gathers `ctx_after` only when `buf_end < self.size`
                // (this field's own doc comment), so `buf_end == self.size` means that fetch never
                // ran at all, `ctx_after` is empty by construction the identical way, and the
                // second disjunct is already true whenever the third is, via this path -- sound,
                // simply redundant there, not wrong. Relevant only when NONE hold: an assertion
                // within `CTX_AHEAD` of `hay`'s own edge is then unresolved (the AHEAD-side fetch
                // stopped at its own byte budget, every read full, so more real data could still
                // exist past what was fetched) -- a documented miss, never fabricated by treating
                // the cut as if it were real EOF.
                let hay_reaches_eof = ahead_hit_real_end
                    || buf_end + ctx_after.len() as u64 == self.size
                    || buf_is_true_eof;
                hay_high = if hay_reaches_eof {
                    crate::search::hay::Edge::True
                } else {
                    crate::search::hay::Edge::Cut
                };
            }
        }
        // restructure R4: one `Hay` per `viewport` call, covering the whole fetched extent
        // (`ctx_before ++ buf ++ ctx_after`) -- `ctx_after` is real, reportable content here, not
        // consult-only (batch 4/5's own established rule: a match starting in `buf` and
        // completing inside `ctx_after` is genuine and highlighted), so `body_limit` is this
        // hay's own full length, unnarrowed. Built even when `search.is_none()` (over the empty
        // default `hay_bytes`) so `current_match`/`search_ref`, below, stay simple unconditional
        // expressions rather than duplicating the `Some`/`None` branch a second time -- both
        // degrade to `None` on an empty hay regardless (`point_verified` on an empty accept range
        // finds nothing), so this costs nothing observable.
        //
        // restructure R5 (2026-07-28): the low edge is no longer set here at all -- R4's own
        // `hay_low = if top.offset() == 0 { True } else { Cut }` is WRONG the moment `top.offset()`
        // sits strictly below `CTX_BEHIND` with real bytes reaching all the way down to file
        // position 0 (`edge_context`'s own behind-side fetch gets exactly `top.offset()` bytes
        // there, `lb == top.offset()`): this hay's own `base` -- `top.offset() - lb` -- is
        // genuinely `Abs(0)` (true BOF) even though `top.offset()` itself is nonzero, and the OLD
        // condition denied the BOF axiom to a position that legitimately owns it (inert under R4,
        // since nothing consulted `low` for any viewport accept decision; load-bearing the moment
        // `Hay::verified`'s own `lookbehind_ok` does). `Hay::low_is_true` derives the CORRECT
        // condition (`base == Abs(0)`) automatically -- there is no `with_low` to get wrong.
        // restructure R7 (batch 8 (2026-07-29)): the base is now READ from the assembly instead
        // of rebuilt here as `top.offset().saturating_sub(lb)`. Same number by construction --
        // `extend_below` moved the base to exactly `ctx_before`'s own start, and `lb` is the
        // reading taken back off it -- but the `saturating_sub` no longer has to be trusted to
        // agree with whatever `lb` ended up being, which is the whole subject of this
        // restructure. The empty no-search assembly is still based at `top`, unread.
        let hay = asm.hay().with_high(hay_high).with_body_limit(asm.len());
        // PHANTOM LINE (batch 4 (2026-07-24), finding #2's own rule; centralized restructure R4:
        // `Hay::phantom_at` is now the one implementation, cited here rather than re-derived --
        // `buf`'s own last byte is the real byte right before the true end whenever
        // `buf_is_true_eof` holds (`ctx_before` never reaches this far forward, and `ctx_after` is
        // empty then too -- both disjuncts of `buf_is_true_eof` independently guarantee it, see
        // this field's own doc comment above), so `hay.bytes().last()` -- what `phantom_at` reads
        // -- IS `buf`'s own last byte there, and `payload_end == hay_bytes.len()` makes the
        // position check hold too) -- if it's `\n`, `payload_end` names no real line, the
        // trailing newline's own phantom, and must never be a match, ordinarily or as current
        // (finding #7). The explicit `buf_is_true_eof &&` guard stays even though `phantom_at`
        // ALSO gates on `self.high == Edge::True` (`hay_reaches_eof`, a WIDER condition than
        // `buf_is_true_eof` -- three disjuncts, not one): whenever `buf_is_true_eof` is false,
        // this expression is false regardless of what `phantom_at` alone would say, so the two
        // conditions never actually need to agree beyond that -- kept explicit (not relied upon
        // as an unstated consequence) for the same reason `buf_is_true_eof`'s own presence in
        // `eof_zero_width_ok` is disclosed above (P3-6/P2-FINAL): a guard whose necessity is
        // merely implied by a wider one it happens to imply is exactly the shape a later edit to
        // either condition could silently break.
        let phantom_at_eof =
            buf_is_true_eof && hay.phantom_at(crate::search::hay::Local(payload_end));
        let eof_zero_width_ok = buf_is_true_eof && !phantom_at_eof;
        // batch 3 (2026-07-23), finding #11(a): resolved directly against the whole buffer,
        // independent of whatever any row's own enumeration separately finds -- `find_all`'s own
        // non-overlapping walk (and, per row, `Hay::all_verified_row`'s identical convention) can
        // skip straight over a start `search_next` itself legitimately lands on (e.g. `/aa` over
        // "aaa": only 0..2 is ever enumerated, but `n` can land the cursor at 1, an overlapping
        // start neither enumeration ever reports) -- a lookup keyed on "which enumerated match
        // starts here" would find nothing for those. `Hay::point_verified`'s own accept range is
        // a single point (`rel..rel+1`): the exactly-once contract it was built for (search.rs's
        // own doc comment) applied here to ask one narrow question, "what starts exactly at
        // `current`," independent of any row's own enumeration, resolved under today's viewport
        // belt (`Hay::viewport_belt`'s own doc comment -- restructure R4 (2026-07-27) centralizes
        // this; the belt itself is unchanged, batch 5 findings #6/#7). `checked_sub` guards a
        // `current` that has scrolled above `top` (this viewport no longer contains it) the same
        // way the `rel <= payload_end` check below guards one that hasn't been read this far yet
        // -- both sides of "not visible here" degrade to `None`, never a panic or a wrapped
        // offset.
        //
        // fix round (2026-07-25), F2: NO end-side filter here, deliberately, matching
        // `visible_window_matches`'s own reach -- a match starting in `buf` but completing inside
        // real `ctx_after` bytes is genuine (RULED to keep it: nav already finds it, and the
        // visible prefix earns its own highlight), so `current` must be resolvable there too, or
        // `spans` (REVERSED) and `current` (BOLD) would disagree on the identical match.
        let current_match = search.and_then(|(pattern, current)| {
            current.and_then(|target| {
                target.checked_sub(top.offset()).and_then(|rel| {
                    let rel = crate::search::hay::Local((rel as usize) + lb);
                    (rel.0 <= payload_end)
                        .then(|| {
                            hay.point_verified(
                                pattern,
                                rel,
                                crate::search::hay::Local(payload_end),
                                eof_zero_width_ok,
                            )
                        })
                        .flatten()
                        .map(|(s, e)| (s.0 - lb)..(e.0 - lb))
                })
            })
        });
        let search_ref = search.map(|(pattern, _)| RowSearch {
            pattern,
            parent: &hay,
            lb,
            buf_hi: crate::search::hay::Local(payload_end),
            eof_zero_width_ok,
        });
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
                    search_ref,
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
                search_ref,
                current_match.as_ref(),
            );
            out.push(row);
            marks.push(row_marks);
        }
        Ok(ViewportRender { rows: out, marks })
    }
    /// Up to `CTX_BEHIND` real bytes immediately before `top` -- unchanged by batch 5
    /// (2026-07-26), findings #6/#7 (the look-BEHIND side is out of this unit's own scope; batch
    /// 4's own finding #6 established it, `viewport`'s own doc comment) -- and, on the AHEAD
    /// side, up to `MAX_MATCH_LEN + CTX_AHEAD - 1` real bytes immediately after `top + buf_len`
    /// (batch 5 (2026-07-26), finding #6: widened from a single-block, `CTX_BEHIND`-wide fetch --
    /// that width was only ever enough for a candidate whose trailing assertion lands exactly at
    /// `buf`'s own edge; a maximal-length candidate starting further back needs the SAME margin
    /// `search.rs`'s own windowed scanners derive, `accept_and_hay_for_row`'s own doc comment has
    /// the derivation that consumes this). Up to `CTX_BEHIND` `warm()` reads on the BEHIND side
    /// -- batch 7 (2026-07-28), finding #6: this said "at most one ... (whichever single block
    /// sits adjacent to `top`)", which was true until batch 6 (2026-07-28), finding #4 made the
    /// behind-side gather a multi-block walk downward and left this sentence (and its twin in
    /// `docs/search.md`) unedited. The loop below asks for at most one byte per block in the
    /// worst case, so `block_size` 4 costs two reads on a non-aligned `top` and `block_size` 1
    /// costs four. **What decides the count is ALIGNMENT, not `block_size`** (batch 8
    /// (2026-07-29), P2): this used to add "at any realistic `block_size` it is still the one
    /// block adjacent to `top`", which is false whenever `top % block_size` is 1, 2, or 3 -- then
    /// `want_from = top - CTX_BEHIND` lands in the PRECEDING block and the walk touches two, at
    /// 1 MiB exactly as at 4. A large `block_size` makes that alignment rarer, never impossible,
    /// and rarity is not the property the sentence claimed. **For INTERIOR positions only** (batch
    /// 9 (2026-07-29), finding #5): `want_from` is a `saturating_sub`, so near BOF it clamps to 0
    /// rather than crossing anything -- at `top` 1 with `block_size` 4 the walk touches block 0
    /// alone, remainder 1 notwithstanding. The two-block case needs the full `CTX_BEHIND` of
    /// lookbehind to actually exist below `top`, i.e. `top >= CTX_BEHIND`. The bound itself is
    /// unaffected either way: at most `CTX_BEHIND` blocks. The AHEAD side may need a FEW block reads
    /// (mirrors `SearchForward`'s own bounded F4 peek, `scan.rs`, block by block, `warm()` not
    /// `block()` so this stays a cache-coherent read like every other fetch here) -- bounded, not
    /// unbounded: at most `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes, ONCE per `viewport` call (this
    /// function's own caller), never per row. **The ahead side is decided by alignment too, and
    /// the earlier measurement did not establish otherwise** (batch 8 (2026-07-29), P2). Fix
    /// round (2026-07-27), P3-2 measured `MockSource::read_count()` deltas flat at 1 across
    /// `rows` = 1, 40, 200 at the default `block_size` (1 MiB) and concluded those 4099 bytes
    /// "fit inside the SAME block `buf`'s own read already warmed, so this widening costs ZERO
    /// additional physical reads there". The measurement was real but its generalization was not:
    /// it varied `rows` while holding the viewport's own OFFSET fixed, and offset is the variable
    /// that decides this. Re-measured at the default 1 MiB with `buf` ending 100 bytes short of
    /// the boundary: 2 reads, against 1 for the same viewport block-aligned. **At most one more,
    /// not necessarily one more** (batch 9 (2026-07-29), finding #4 -- the batch-8 replacement
    /// overcorrected into a second unconditional claim): crossing the boundary is necessary but
    /// not sufficient, because `want` is `min(consumable_reach, self.size - buf_end)`, so the
    /// request only reaches past the boundary when that much real file actually remains. With
    /// only 50 real bytes past a `buf` ending 100 bytes short of the boundary the count stays at
    /// 1, measured. The honest statement is therefore doubly conditional: ZERO additional reads
    /// whenever `buf`'s own end has 4099 bytes of its own block left OR the file itself ends
    /// before the boundary; at most one more otherwise (and zero even then if that block is
    /// already warm -- these are PHYSICAL reads, so a cache hit costs nothing). "A few blocks"
    /// only materializes where `block_size` is itself on the order of 4 KiB or smaller. When
    /// the AHEAD side's own request straddles a block boundary partway through a small custom
    /// `block_size`, the FINAL block touched can still return fewer bytes than requested -- a
    /// narrower, still-honest context window, not a defect, same residual the BEHIND side already
    /// carries. When the real source has FEWER bytes past `buf_len` than `self.size` claims (a
    /// stale size, the same class `scan.rs`'s own `SearchBackward` wrap-leg terminal check guards
    /// against, batch 5 finding #1), a block read comes back short of what was asked -- but,
    /// fix round (2026-07-27), P2-NEW, this loop treats a SHORT-but-NONEMPTY answer differently
    /// from a GENUINELY EMPTY one, and only the latter certifies a real end (see `ahead_hit_real_
    /// end`'s own paragraph below for why, and for the consequence: this is narrower than `docs/
    /// budgeted_scanning.md`'s own general "truncation policy" as literally stated, not the
    /// identical instance of it an earlier version of this comment claimed).
    ///
    /// `ahead_hit_real_end` (the FOURTH return value -- batch 9 (2026-07-29), finding #6: it was
    /// the third until restructure R7 added `ctx_before`'s own absolute start ahead of it)
    /// reports whether THIS call's own AHEAD-side
    /// loop observed a block that came back GENUINELY EMPTY (`block.is_empty()`, not merely
    /// shorter than requested) before the requested margin was satisfied -- fix round (2026-07-27),
    /// P2-1: a short-but-nonempty answer is legal under `BlockSource`'s own "up to `len`" contract
    /// for a reason OTHER than "no more real data exists" (`source.rs`), so it is NEVER a
    /// certificate on its own; only an empty read verifies the position it lands at as the real
    /// end, regardless of what `self.size` claims.
    ///
    /// **Correction (fix round (2026-07-27), P2-NEW):** an earlier version of this paragraph
    /// claimed a SHORT-OR-empty read certifies here, "matching how `SearchForward`'s own `take ==
    /// 0` branch already treats an identical discovery" -- both halves are now false, and retracted
    /// (this project's own established style: correct in place with a stated retraction, not a
    /// silent edit). First, only EMPTY certifies here, not merely short (P2-1, above). Second, the
    /// two are no longer identical: `SearchForward` was deliberately left untouched by that same
    /// fix (out of this unit's own scope), so it still treats ANY short/empty read as a genuine
    /// end, exactly as `docs/budgeted_scanning.md`'s own general policy states. This asymmetry has
    /// a real, measured consequence, since `block_start <= p <= real_len` always holds on this
    /// loop: an empty touch requires `block_start == real_len` exactly, i.e. the source's
    /// real end must fall precisely on a block boundary -- at this project's own default
    /// `block_size` (1 MiB) an approximately 1-in-2^20 coincidence, so in practice `ahead_hit_real_
    /// end` almost never fires on a genuinely truncated, non-block-aligned source. On such a
    /// source, nav (`search_next`, via `SearchForward`) USED TO find a match the viewport refuses
    /// to paint; batch 11 (2026-07-29), P1 closed that divergence by making nav refuse it too
    /// (`viewport_and_nav_agree_at_a_non_block_aligned_truncation_now_by_resolving_it`, `document.rs`, pins the
    /// agreement now, and its own name changed with the verdict) -- an ACCEPTED cost (a
    /// conservative miss,
    /// never a fabrication, consistent with this same fix round's own P1-1 ruling: no fabrication
    /// beats false paint, and the viewport's own authority here is deliberately narrower than
    /// nav's), not a correctness win, and docketed for reconciliation by the restructure's planned
    /// `CertifiedEnd` type (a witness mintable from an observed-empty read at ANY position, not
    /// only a block-aligned one -- probing past a short block's own edge to find it -- which would
    /// restore agreement with nav without reopening `P2-1`'s own fabrication). `false` when the
    /// loop instead stopped because it reached its own `MAX_MATCH_LEN + CTX_AHEAD - 1` byte budget
    /// (or `self.size`'s own claim) with every read full -- unverified in the sense that MORE real
    /// data could still exist past what was fetched, though (per `accept_and_hay_for_row`'s own
    /// doc comment) that unverified case never actually needs the margin the belt exists for.
    ///
    /// `top`, when nonzero, is always a real line start (the `Anchor` invariant: "0, or the byte
    /// just past a newline before EOF") -- so its own immediate predecessor byte is virtually
    /// always `\n`. This deliberately does NOT assume that: it fetches the REAL byte regardless,
    /// the same "verify, don't assume" discipline `SweepAnalysis`'s own phantom-line check
    /// applies to an analogous invariant.
    async fn edge_context(
        &self,
        top: u64,
        buf_len: usize,
    ) -> anyhow::Result<(crate::search::hay::Abs, Vec<u8>, Vec<u8>, bool)> {
        let bs = self.cache().block_size() as u64;
        // batch 6 (2026-07-28), finding #4: a multi-block gather, not one block -- mirrors
        // `ctx_after`'s own shape (batch 5, finding #6), just walking block indices DOWNWARD
        // from `(top - 1) / bs` instead of up, `warm()`-ing each one, until CTX_BEHIND bytes are
        // gathered or BOF (`want_from`) is reached. `p` is the exclusive lower bound of what has
        // NOT yet been gathered, starting at `top` and descending; each iteration touches the
        // block containing `p - 1` and asks for everything from `max(want_from, block_start)` up
        // to `p`. Stops (without using this block's own contribution) the moment a block's own
        // returned length does not reach all the way up to `p` -- the identical "no refill, no
        // cross-block bridging" discipline `ctx_after`'s own loop already established: a short
        // read here just means less look-behind, never a byte spliced in from across an
        // unaccounted gap as though it were adjacent to `top`.
        // restructure R7 (batch 8 (2026-07-29)): this walk reports WHERE IT STOPPED, not just
        // what it gathered. `p` is the absolute position of `out`'s first byte at every point in
        // the loop, so returning it hands the caller a fact instead of leaving it to re-derive
        // `top - out.len()` -- the length subtraction that is exactly this restructure's subject.
        // The two agree here (this walk descends FROM `top`, so a short read only ever truncates
        // the far end and what survives stays flush against the anchor), which is why the
        // `Assembly::extend_below` in `viewport` can never legitimately refuse this run -- but
        // "can never" is now something the caller CHECKS rather than something it assumes.
        let (ctx_before_at, ctx_before) = if top > 0 {
            let want_from = top.saturating_sub(crate::search::CTX_BEHIND.get() as u64);
            let mut out = Vec::new();
            let mut p = top;
            while p > want_from {
                let idx = (p - 1) / bs;
                let block = self.cache().warm(idx).await?;
                let block_start = idx * bs;
                let hi_off = (p - block_start) as usize;
                if block.len() < hi_off {
                    // this block's own return doesn't reach up to `p` -- a gap sits between
                    // whatever real bytes it has and what has already been gathered above it.
                    // Nothing below the gap can be treated as contiguous with `top`, so stop
                    // without using this block at all (everything gathered so far, above the
                    // gap, remains valid and is returned as-is).
                    break;
                }
                let lo = want_from.max(block_start);
                let lo_off = (lo - block_start) as usize;
                let mut chunk = block[lo_off..hi_off].to_vec();
                chunk.extend_from_slice(&out);
                out = chunk;
                p = lo;
            }
            (crate::search::hay::Abs(p), out)
        } else {
            (crate::search::hay::Abs(top), Vec::new())
        };
        let buf_end = top + buf_len as u64;
        let mut ahead_hit_real_end = false;
        let ctx_after = if buf_end < self.size {
            // batch 5 (2026-07-26), finding #6: a multi-block gather, not one block -- mirrors
            // `SearchForward`'s own bounded F4 peek (scan.rs) in shape: `warm()` block by block,
            // stopping the moment a block comes back short of what it was asked (real EOF, or a
            // stale `self.size`, either way nothing more real to fetch) -- `docs/budgeted_
            // scanning.md`'s own truncation policy, applied here rather than reinvented.
            // restructure R4 (2026-07-27): `hay::Hay::consumable_reach()`.
            let want =
                (crate::search::hay::Hay::consumable_reach().get() as u64).min(self.size - buf_end);
            let want_end = buf_end + want;
            let mut out = Vec::new();
            let mut p = buf_end;
            while p < want_end {
                let idx = p / bs;
                let block = self.cache().warm(idx).await?;
                let block_start = idx * bs;
                let lo_off = (p - block_start) as usize;
                let take = (block.len().saturating_sub(lo_off)).min((want_end - p) as usize);
                if take == 0 {
                    // batch 5 (2026-07-26) fix round, P2-1: a short take is never a certificate on
                    // its own -- `BlockSource`'s own contract (`source.rs`) only promises "up to
                    // len" bytes, so a block shorter than requested can be a legal answer for a
                    // reason OTHER than "no more real data exists" (nothing stops a conforming
                    // source from answering short of `len` while more real content genuinely
                    // follows). Only a block that came back GENUINELY EMPTY verifies this position
                    // as the real end -- the identical `block.is_empty()` refinement unit F's own
                    // fix round needed for the analogous backward-descent case (`scan.rs`,
                    // `wholly_empty_first_block`), applied here rather than reinvented.
                    //
                    // **batch 12 (2026-07-29): "there is no safe way to ask for more from the SAME
                    // cached index" is no longer true**, and that sentence was the whole reason
                    // this branch had to leave a short block unresolved. `BlockCache` now completes
                    // a short block's own hole by re-asking the source, and records the answer when
                    // the source says there is nothing there (`Block::ends_data`). A block whose
                    // shortfall has been certified that way IS a verified boundary -- the source
                    // itself reported empty at that offset -- so it certifies here exactly as a
                    // wholly-empty block does.
                    //
                    // batch 13 (2026-07-29), findings #1 and #3: the fact is read off THE BLOCK,
                    // not re-queried by index (a certificate belongs to a length, and the entry
                    // behind an index can change length under a racing refill), and there is no
                    // longer a third, unresolved case to leave hanging -- the refill loop runs to
                    // an answer, so `ends_data` false here means "short, and the file's own claimed
                    // size ends inside this block", never "nobody could find out".
                    if block.is_empty() || block.ends_data() {
                        ahead_hit_real_end = true;
                    }
                    break;
                }
                out.extend_from_slice(&block[lo_off..lo_off + take]);
                p += take as u64;
            }
            out
        } else {
            Vec::new()
        };
        Ok((ctx_before_at, ctx_before, ctx_after, ahead_hit_real_end))
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
            crate::scan::Step::Done(crate::scan::FwdEnd::Found(s)) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(s))))
            }
            crate::scan::Step::Done(crate::scan::FwdEnd::Eof(clamp)) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(clamp))))
            }
            crate::scan::Step::More(..) => {
                let cache = self.cache.clone();
                let span = self.size - start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.progressed(),
                    move |tx| async move {
                        scan.complete(&cache, tx, span).await.map(|end| {
                            let s = match end {
                                crate::scan::FwdEnd::Found(s) | crate::scan::FwdEnd::Eof(s) => s,
                            };
                            NavOutcome::At(Anchor(s))
                        })
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
            // `Exhausted` is structurally unreachable here: `BackwardScan::new` always
            // constructs via `Meter::background`, never pre-spent (only `with_meter`'s own
            // composed caller, `resolve_line_start_step`, can start already out of budget).
            // Treated identically to `More` regardless -- both mean "pend, retry via `complete`".
            crate::scan::BwdStep::More(..) | crate::scan::BwdStep::Exhausted => {
                let cache = self.cache.clone();
                let span = start;
                Ok(Resolution::Pending(spawn_pending(
                    "scrolling",
                    span,
                    scan.progressed(),
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
            // `Exhausted` is structurally unreachable here (this leg's own `BackwardScan::new`,
            // not `with_meter` -- see `scroll_up`'s own identical comment); treated the same as
            // `More` regardless.
            crate::scan::BwdStep::More(..) | crate::scan::BwdStep::Exhausted => {
                let cache = self.cache.clone();
                let span = self.size;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to end",
                    span,
                    scan.progressed(),
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
            // `Exhausted` is structurally unreachable here (this leg's own `BackwardScan::new`,
            // not `with_meter` -- see `scroll_up`'s own identical comment); treated the same as
            // `More` regardless.
            crate::scan::BwdStep::More(..) | crate::scan::BwdStep::Exhausted => {
                let cache = self.cache.clone();
                let span = target;
                Ok(Resolution::Pending(spawn_pending(
                    "percent jump",
                    span,
                    snap.progressed(),
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
                scan.complete(&cache, tx, span2).await.map(|end| {
                    let s = match end {
                        crate::scan::FwdEnd::Found(s) | crate::scan::FwdEnd::Eof(s) => s,
                    };
                    NavOutcome::At(Anchor(s))
                })
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
            crate::scan::Step::Done(crate::scan::FwdEnd::Found(s)) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(s))))
            }
            crate::scan::Step::Done(crate::scan::FwdEnd::Eof(clamp)) => {
                Ok(Resolution::Ready(NavOutcome::At(Anchor(clamp))))
            }
            crate::scan::Step::More(..) => {
                let cache = self.cache.clone();
                let span = self.size - cp_off;
                Ok(Resolution::Pending(spawn_pending(
                    "jumping to line",
                    span,
                    scan.progressed(),
                    move |tx| async move {
                        scan.complete(&cache, tx, span).await.map(|end| {
                            let s = match end {
                                crate::scan::FwdEnd::Found(s) | crate::scan::FwdEnd::Eof(s) => s,
                            };
                            NavOutcome::At(Anchor(s))
                        })
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
                crate::scan::Bound::Exclusive(size),
                budget,
            ))
        } else {
            // leg 1: `hi` is the cursor's own EXCLUSIVE origin, not a genuine wrap leg's own
            // unconditional far end -- batch 4 (2026-07-24), finding #1, now the constructor
            // choice itself (restructure R6): `new_leg`, never `new_wrap_leg`.
            SearchLeg::Bwd(crate::scan::SearchBackward::new_leg(
                pattern.clone(),
                origin,
                crate::scan::Bound::Inclusive(0),
                budget,
            ))
        };
        match leg.step(self.cache()).await? {
            crate::scan::SearchStep::Done(crate::scan::SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                self.resolve_found(leg.into_meter(), match_at, line_start, false)
                    .await
            }
            crate::scan::SearchStep::Done(crate::scan::SearchEnd::End) => {
                self.search_wrapped_leg(origin, pattern, forward).await
            }
            crate::scan::SearchStep::More(..) => Ok(Resolution::Pending(
                self.spawn_search_pending(leg, origin, pattern.clone(), forward),
            )),
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
            crate::scan::SearchStep::Done(crate::scan::SearchEnd::Found {
                match_at,
                line_start,
            }) => {
                self.resolve_found(leg.into_meter(), match_at, line_start, true)
                    .await
            }
            crate::scan::SearchStep::Done(crate::scan::SearchEnd::End) => {
                Ok(Resolution::Ready(NavOutcome::Exhausted))
            }
            crate::scan::SearchStep::More(..) => {
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
    /// scratch).
    ///
    /// restructure R3 (batch-5 finding #2): `meter` is the match scan's own Meter (`SearchLeg::
    /// into_meter`), not a byte count -- the OLD code computed `remaining = budget -
    /// matched(scanned())` and handed the hunt a FRESH allowance of that many bytes, which
    /// OVER-granted whenever the match scan's own `scanned()` (payload only) understated its true
    /// spend (bookkeeping -- the straddle seed, ctx reads -- was invisible to it): block size 1,
    /// budget 64 could cost ~97 physical reads for one search, not ~64. Passing the SAME Meter
    /// through (`resolve_line_start_step` -> `BackwardScan::with_meter`) makes the bound additive
    /// by construction instead: the hunt's own `out_of_budget()` sees the search's own CUMULATIVE
    /// `charged`, whatever it was, with no subtraction to get wrong.
    async fn resolve_found(
        &self,
        mut meter: crate::meter::Meter,
        match_at: u64,
        hint: Option<u64>,
        wrapped: bool,
    ) -> anyhow::Result<Resolution> {
        // the search leg's own meter arrives `background` (no ceiling -- `SearchForward::new`'s
        // own doc comment explains why the leg itself must not carry one), with whatever it
        // already charged intact; imposed HERE, at the exact moment the search is done, so the
        // follow-up hunt below is bounded by the SAME interactive budget the search itself was,
        // additively (`Meter::impose_allowance`'s own doc comment).
        meter.impose_allowance(self.config.nav_scan_budget as u64);
        match resolve_line_start_step(self.cache(), meter, match_at, hint).await? {
            LineStartStep::Resolved(top) => Ok(Resolution::Ready(NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            })),
            LineStartStep::More { scan, span } => {
                let cache = self.cache.clone();
                let scanned = scan.progressed();
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
        let scanned = leg.progressed();
        spawn_pending("searching", size, scanned, move |tx| async move {
            match leg.complete(&cache, tx.clone(), size).await? {
                crate::scan::SearchEnd::Found {
                    match_at,
                    line_start,
                } => {
                    found_outcome_pending(&cache, budget, match_at, line_start, false, tx, size)
                        .await
                }
                crate::scan::SearchEnd::End => {
                    // leg 1 exhausted its whole origin-relative half with no match; continue
                    // straight into leg 2 (the wrap), inside this same background task -- never
                    // a second hop back to the caller. `tx.clone()` here (unlike the bare `tx`
                    // pre-batch-3): a Found below still needs `tx` again, for ITS OWN line-start
                    // hunt (`found_outcome_pending`, finding #1) -- the one remaining owner.
                    match wrapped_leg(pattern, origin, size, forward, budget)
                        .complete(&cache, tx.clone(), size)
                        .await?
                    {
                        crate::scan::SearchEnd::Found {
                            match_at,
                            line_start,
                        } => {
                            found_outcome_pending(
                                &cache, budget, match_at, line_start, true, tx, size,
                            )
                            .await
                        }
                        crate::scan::SearchEnd::End => Ok(NavOutcome::Exhausted),
                    }
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
        let scanned = leg.progressed();
        spawn_pending("searching", size, scanned, move |tx| async move {
            match leg.complete(&cache, tx.clone(), size).await? {
                crate::scan::SearchEnd::Found {
                    match_at,
                    line_start,
                } => {
                    found_outcome_pending(&cache, budget, match_at, line_start, true, tx, size)
                        .await
                }
                crate::scan::SearchEnd::End => Ok(NavOutcome::Exhausted),
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
    fn progressed(&self) -> u64 {
        match self {
            SearchLeg::Fwd(s) => s.progressed(),
            SearchLeg::Bwd(s) => s.progressed(),
        }
    }
    /// Consumes this leg for its own Meter -- `resolve_found`'s composition (batch-5 finding
    /// #2): shares this SAME Meter with the follow-up line-start hunt instead of re-deriving a
    /// budget by subtracting one accessor's number from another's (`SearchForward::into_meter`'s
    /// own doc comment has the full reasoning).
    fn into_meter(self) -> crate::meter::Meter {
        match self {
            SearchLeg::Fwd(s) => s.into_meter(),
            SearchLeg::Bwd(s) => s.into_meter(),
        }
    }
    async fn complete(
        self,
        cache: &crate::cache::BlockCache,
        tx: tokio::sync::watch::Sender<crate::resolve::Progress>,
        span: u64,
    ) -> anyhow::Result<crate::scan::SearchEnd> {
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
/// MAX_MATCH_LEN + CTX_AHEAD - 1` (batch 5 (2026-07-26), finding #3 widened this straddle seed
/// from a bare `hi + MAX_MATCH_LEN`; `scan.rs`'s own `SearchBackward` doc comment has the
/// derivation), capped at the true EOF -- purely as lookahead the moment `step` is first
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
/// owns. Proven, not merely argued: the straddler case (this file's own test) needed
/// this widening to pass; post-#3 it is found by LEG 1 instead (`wrapped: false`, not `true`) with
/// no widening at all, and the test was RENAMED to say so:
/// `search_next_backward_finds_a_straddler_via_leg_1_not_the_wrap`.
fn wrapped_leg(
    pattern: Arc<crate::search::SearchPattern>,
    origin: u64,
    size: u64,
    forward: bool,
    budget: usize,
) -> SearchLeg {
    if forward {
        let limit = origin.saturating_add(crate::search::MAX_MATCH_LEN as u64);
        // this is the ONE real, non-test caller anywhere in the engine whose `SearchForward`
        // peek gate (`at_final && !at_eof`, `scan.rs`'s own `step`) can actually FIRE -- not the
        // only bounded caller (fix round 2's own final verdict, P3-b: forward leg 1, just above
        // `search_leg`'s own construction in this file, ALSO passes a finite `limit` -- `size`,
        // not `u64::MAX`), but for leg 1 `limit == size` makes `at_final` (`read_end >= size`)
        // and `at_eof` (`read_end >= cache.size()`) the SAME condition whenever `size` and
        // `cache.size()` agree, as they do by construction for that call -- so `!at_eof` never
        // holds there and the gate never opens: leg 1 is bounded but structurally peek-free, not
        // unbounded. `wrapped_leg`'s own `limit` is different in kind, not just degree: `origin +
        // MAX_MATCH_LEN` is an artificial straddle-seed bound with real file content on both
        // sides of it, so `at_final` and `at_eof` genuinely diverge here -- making this the
        // real-world home of `SearchForward`'s own bounded-leg residual (F4) and its
        // fix-round-2 retry fix (R1, `scan.rs`'s own `step`, the `e > payload_end` branch).
        //
        // Coverage choice, stated per the fix-round-2 dispatch: no separate end-to-end fixture
        // through this function -- `step`'s
        // own unit tests (`search_forward_dollar_*`, `search_forward_finds_a_later_alternative_
        // when_the_leftmost_overruns_a_bounded_legs_own_limit`) already pin the exact mechanism
        // directly, and this function contributes no logic of its own to it beyond choosing
        // `limit`'s own value -- an e2e fixture would need to reconstruct the identical
        // overrun-vs-fits-in-payload shape through the full `Document`/wrap API for no
        // additional coverage over the direct `SearchForward` unit test.
        SearchLeg::Fwd(crate::scan::SearchForward::new(
            pattern,
            0,
            crate::scan::Bound::Exclusive(limit),
            budget,
        ))
    } else {
        // NOT widened -- deliberately the naive `origin`: `SearchBackward` itself now confirms
        // a straddler on leg 1's own pass (batch 3 (2026-07-23), finding #3), so leg 2 never
        // needs to re-examine that territory; see this function's own doc comment above.
        let floor = origin;
        // `hi = size` here is unconditionally the wrap's own far end (this function's own doc
        // comment), never merely coincidental with a cursor's own origin the way leg 1's `hi`
        // can be -- batch 4 (2026-07-24), finding #1, now `new_wrap_leg` itself (restructure R6).
        SearchLeg::Bwd(crate::scan::SearchBackward::new_wrap_leg(
            pattern,
            size,
            crate::scan::Bound::Inclusive(floor),
            budget,
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
/// Attempts the line-start hunt sharing `meter` -- the match scan's own Meter, whatever it has
/// already charged (batch 3 (2026-07-23), finding #1: without a cap here at all, a hot cache let
/// the OLD unconditional hunt run to completion inside the very same poll that found the match,
/// no yield point in reach regardless of the file's own size -- 157 reads for a 10,000-byte
/// single line at budget 64, probe-verified; restructure R3, batch-5 finding #2: the cap is now
/// the SAME Meter the match scan spent from, not a byte count re-derived by subtraction --
/// `Document::resolve_found`'s own doc comment has the composition reasoning). One shared helper
/// for both `search_next`'s own leg-1 Found arm and `search_wrapped_leg`'s leg-2 one
/// (`Document::resolve_found` is their common caller) -- budgeted the identical way either leg.
async fn resolve_line_start_step(
    cache: &crate::cache::BlockCache,
    meter: crate::meter::Meter,
    match_at: u64,
    hint: Option<u64>,
) -> anyhow::Result<LineStartStep> {
    if let Some(top) = line_start_shortcut(cache, match_at, hint).await? {
        return Ok(LineStartStep::Resolved(top));
    }
    let mut scan = crate::scan::BackwardScan::with_meter(match_at, 1, meter);
    let span = scan.remaining_bytes();
    match scan.step(cache).await? {
        crate::scan::BwdStep::Found(s) => Ok(LineStartStep::Resolved(Anchor(s))),
        crate::scan::BwdStep::Top => Ok(LineStartStep::Resolved(Anchor::TOP)),
        // fix round, P1-2: this is the ONE site `Exhausted` is genuinely reachable from -- the
        // shared, composed meter (`with_meter`, just above) can already be at its own ceiling
        // before this hunt's own first read, whenever the match search that shares it already
        // spent the whole allowance (`Document::resolve_found`'s own doc comment has the
        // composition reasoning; the adversarial review's own probe 2 is what caught this
        // returning a hard `Err` instead of `Pending`). Treated identically to `More`: the
        // caller (`Document::resolve_found`) converts either into `Resolution::Pending`, and
        // `BackwardScan::complete`'s own `lift_allowance` (its first line) removes the ceiling
        // once the background phase takes over, so the retry is never stuck the same way twice.
        crate::scan::BwdStep::More(..) | crate::scan::BwdStep::Exhausted => {
            Ok(LineStartStep::More { scan, span })
        }
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
/// Pure arithmetic, no IO: derives this row's own `accept` and `hay` ranges, both ABSOLUTE into
/// `full_hay` (`ctx_before + buf + ctx_after`, unit B's own hay -- `viewport`'s own doc comment)
/// from its visible byte window `win` (row-local, `line::visible_byte_range`'s own output).
/// Factored out from `visible_window_matches` (below) so the boundedness claim itself (batch 4
/// (2026-07-24), finding #5) can be unit tested directly, with exact expected ranges, independent
/// of a `Document`/tokio runtime.
///
/// `accept` -- the span a match's own START must fall within (its END is a SEPARATE concern,
/// handled by `hay`'s own wider reach and `visible_window_matches`'s own post-hoc filtering
/// below, fix round (2026-07-25), F2/F3 -- NOT by handing `accept` itself to `find_all_starting_
/// in` as its own accept parameter, which would additionally bound the END to `accept.end` too) --
/// is `win` shifted absolute, expanded LEFTWARD by `MAX_MATCH_LEN` and clamped to BUF's own span
/// (`lb..lb+buf_len`), deliberately NOT to the row: a match's own length is capped at
/// `MAX_MATCH_LEN`, so nothing starting further left could
/// ever reach the window regardless, but a match starting up to that far back CAN still reach it
/// -- crossing as many newlines as it needs to on the way, since this is a flat byte range, not a
/// row-scoped one (the multiline coverage argument, verified against the adversarial fixture in
/// the test suite below). The right side is `win.end + 1`, not `win.end`: a zero-width match
/// sitting exactly AT the row's own visible content end (an EOF `$` still within a wider window,
/// finding #7(a)) starts at that exact position, and admitting it here is always safe -- anything
/// this over-admits that should NOT actually render is dropped downstream regardless, by
/// `layout_row_with_marks`'s own `token_at(..) -> None` fallback (finding #7(b)'s own audit
/// argument, applied here to why over-admission can never manufacture a rendered mark).
///
/// `hay` -- the actual bytes handed to `find_all_starting_in`, wide enough that any candidate
/// starting in `accept` is confirmed or refuted including its own trailing assertion (the
/// boundary-context model, `search.rs`'s own doc comment) -- expands `accept` by `CTX_BEHIND`
/// bytes behind and `MAX_MATCH_LEN + CTX_AHEAD - 1` bytes ahead, clamped to `full_hay`'s own span
/// rather than buf's: within buf's own interior this never reaches past what `accept`'s own clamp
/// already allows, but AT buf's own physical edges it reaches into the real `ctx_before`/
/// `ctx_after` bytes unit B's own `edge_context` fetched -- the identical bytes the whole-buf
/// computation this replaces already relied on, now consulted per row instead of once.
///
/// **Correction (batch 5 (2026-07-26), findings #6/#7):** the paragraph above states the honest
/// claim; an earlier version (before this unit) widened the AHEAD side by only `MAX_MATCH_LEN`,
/// not `MAX_MATCH_LEN + CTX_AHEAD - 1` -- a cap-length candidate starting at `accept.end - 1`
/// could have its own trailing assertion evaluated with as little as ONE byte of real lookahead
/// past it, sometimes a split codepoint, fabricating `(?u:\b)`/`$` at a row-scoped hay cut that
/// was never a real boundary (unit F's own review, P2-2, constructed two such fixtures against
/// the pre-fix code). With the `+ CTX_AHEAD - 1` widening here AND `edge_context`'s own matching
/// AHEAD-side widening (its own doc comment), any candidate starting in `accept` now resolves its
/// own trailing assertion against a full character's worth of real lookahead, UNLESS the source
/// itself is truncated short of what `self.size` claims -- in which case `visible_window_matches`'s
/// own resolved-criterion belt (its doc comment, below) drops the candidate rather than treating
/// the truncation's own artificial edge as if it were real end-of-file: a documented miss, never a
/// fabrication.
/// Pure predicate, no IO: is a candidate `(s, e)` -- found via `Document::viewport`'s own
/// `current_match` resolution, a DIRECT, single-point `find_starting_in` query against the WHOLE
/// `hay` (never a per-row narrowed slice) -- safe to report? Factored out from `viewport` itself
/// (batch 5 (2026-07-26), findings #6/#7) so both gates can be unit tested directly, without a
/// `Document`/tokio runtime, mirroring `accept_and_hay_for_row`'s own precedent for testability.
///
/// Two independent gates, ANDed:
/// - finding #6's own resolved-criterion belt: `hay_end - e >= CTX_AHEAD` (a full character's
///   worth of real lookahead already exists, WITHIN THIS HAY, past this candidate's own end) OR
///   (`hay_end == full_hay_len && hay_reaches_eof`) (this hay's own edge IS `full_hay`'s own edge,
///   AND that edge genuinely reaches the file's true end -- no margin owed at a real boundary).
/// - finding #7's own phantom guard: `s != payload_end` (this candidate does not start exactly at
///   `buf`'s own edge, the one position the phantom rule can ever apply to) OR
///   `eof_zero_width_ok` (that edge IS the file's true end AND is not the trailing newline's own
///   phantom, batch 4, finding #2).
///
/// Fix round (2026-07-27), P3-5: `hay_end`/`full_hay_len` are SEPARATE parameters, not one -- an
/// earlier version of this function took a single `hay_len`, and `visible_window_matches`'s own
/// ordinary-path filter (below) called a hand-duplicated copy of this same expression instead of
/// this function, ANDing in an extra `hay.end == search.full_hay.len()` conjunct this predicate
/// did not have. The two agreed only because the current-match path's own caller (`Document::
/// viewport`) always passes the WHOLE `hay` with no further slicing, making that conjunct
/// trivially true there -- nothing STRUCTURAL prevented the two from drifting apart. Both call
/// sites now call this SAME function (the ordinary path passes its own row-scoped `hay.end` and
/// `search.full_hay.len()` separately, only trivially equal for the current-match path), so "the
/// two paths can never disagree" is now an actual property of the code, not merely an argument
/// about today's two callers.
///
/// The phantom guard's own effect is, today, unobservable through the full `viewport` ->
/// `render_row` -> `clip_to_row` pipeline for the one fixture that can ever trigger it (a file
/// whose real content ends in `\n`): `clip_to_row`'s own row-bounds check independently rejects a
/// candidate at `buf`'s own edge there too, since no row's own bounds are EVER built to reach
/// that exact position (`viewport`'s own row-building loop always ends a row's bounds AT a real
/// `\n`, never past it, and the final-unterminated-row branch requires `line_start < buf.len()`,
/// which fails exactly when `buf` ends in `\n`) -- verified by construction, not assumed, and kept
/// as a deliberate defense-in-depth measure (this engine's own established style: an invariant
/// stated and enforced at its own natural site, not solely relied upon downstream) rather than cut
/// for being currently redundant with `clip_to_row`.
///
/// **Restructure R4 (2026-07-27): frozen, `#[cfg(test)]`-only.** Both production call sites
/// this doc comment describes now go through `search::hay::Hay::point_verified`/
/// `all_verified_row`, which apply the identical two gates via `Hay::viewport_belt` (today's
/// landed belt, unconditional -- NOT R5's assertion-aware end-rule; that method's own doc
/// comment states the distinction). This function is kept, unmodified, as a plain-scalar
/// REFERENCE this predicate's own direct unit test
/// (`point_candidate_is_resolved_rejects_the_phantom_even_when_the_belt_would_allow_it`, below)
/// still exercises -- deleting it would be deleting that test's own subject, the kind of "test
/// that must move" R4's own brief calls a STOP. `hay_viewport_belt_agrees_with_the_frozen_point_
/// candidate_is_resolved_for_assertion_bearing_patterns` (`search/hay.rs`'s own test module) cross-checks the two stay in
/// agreement, so this is a referee, not dead prose. (Its full name carries an
/// assertion-bearing-patterns suffix that this citation dropped until the guard learned to read
/// wrapped citations, batch 19 (2026-07-31).)
#[cfg(test)]
pub(crate) fn point_candidate_is_resolved(
    s: usize,
    e: usize,
    hay_end: usize,
    full_hay_len: usize,
    hay_reaches_eof: bool,
    payload_end: usize,
    eof_zero_width_ok: bool,
) -> bool {
    (hay_end.saturating_sub(e) >= crate::search::CTX_AHEAD.get()
        || (hay_end == full_hay_len && hay_reaches_eof))
        && (s != payload_end || eof_zero_width_ok)
}
fn accept_and_hay_for_row(
    win: std::ops::Range<usize>,
    line_start: usize,
    buf_len: usize,
    lb: usize,
    full_hay_len: usize,
) -> (
    std::ops::Range<crate::search::hay::Local>,
    std::ops::Range<crate::search::hay::Local>,
) {
    use crate::search::hay::Local;
    let abs_start = lb + line_start + win.start;
    let abs_end = lb + line_start + win.end;
    let buf_lo = lb;
    let buf_hi = lb + buf_len;
    let accept_lo = abs_start
        .saturating_sub(crate::search::MAX_MATCH_LEN)
        .max(buf_lo);
    let accept_hi = abs_end.saturating_add(1).min(buf_hi).max(accept_lo);
    let accept = accept_lo..accept_hi;
    let hay_lo = accept.start.saturating_sub(crate::search::CTX_BEHIND.get());
    // batch 5 (2026-07-26), finding #6: `Hay::consumable_reach()` (`MAX_MATCH_LEN + CTX_AHEAD -
    // 1`), not `MAX_MATCH_LEN` alone -- one byte is enough for `$`/ASCII `\b`, but a
    // Unicode-aware `(?u:\b)`/`(?u:\B)` needs the FULL following character, up to `CTX_AHEAD`
    // real bytes, not one (restructure R4 centralizes this margin; `search.rs`'s own top-level
    // doc comment has the derivation; `SearchForward`/`SearchBackward`'s own identical widening,
    // batch 5, finding #3). Without this, a cap-length candidate starting at `accept.end - 1` had
    // its own trailing assertion evaluated with as little as ONE byte of real lookahead past it --
    // less than a full character, sometimes a split codepoint -- fabricating `(?u:\b)`/`$` at a
    // row-scoped hay cut that was never a real boundary (the interior-hay-cut defect, unit F's own
    // review, P2-2).
    let hay_hi = accept
        .end
        .saturating_add(crate::search::hay::Hay::consumable_reach().get())
        .min(full_hay_len)
        .max(hay_lo);
    (
        Local(accept.start)..Local(accept.end),
        Local(hay_lo)..Local(hay_hi),
    )
}
/// Every match (into `buf`'s own coordinate space, but see the END-side note below) that could
/// paint any of row `line`'s own visible cells -- `render_row`'s own per-row replacement for a
/// whole-buf `find_all_starting_in` call (batch 4 (2026-07-24), finding #5). Derives the row's
/// own visible byte window (`line::visible_byte_range`), then its own `accept`/`hay` ranges
/// (`accept_and_hay_for_row`, above), and runs `find_all_starting_in` over a bounded slice of
/// `full_hay` -- the RETURNED list's own length is `O(win_bytes)`, the row's own visible BYTE
/// window's own width (`win.end - win.start`, `line::visible_byte_range`'s own return value; see
/// the F3 filter below for the exact derivation), independent of `buf`'s own size or the line's
/// own length, the whole point of this restructure. `win_bytes` itself is `O(cols)` for ordinary
/// (non-zero-width) text -- up to `4 * cols` worst case, a maximal 4-byte UTF-8 character
/// rendering at width 1 -- but is NOT bounded by `cols` at all in the presence of a long
/// zero-width (combining-mark) run, the identical exception `line::visible_byte_range`'s own doc
/// comment states (fix round (2026-07-25), F6); this is a stated, accepted characterization, not
/// a perf defect -- ordinary text (including CJK, `4 * cols` there too, in bytes not columns)
/// stays comfortably small, and the pathological case needs a multi-KiB combining-mark run to
/// matter at all.
///
/// A returned range's own END can exceed `buf.len()` (fix round (2026-07-25), F2, RULED): a
/// match starting in `buf` and completing inside real `ctx_after` bytes (`full_hay`'s own reach
/// past `buf`, `Document::edge_context`) is genuine and highlighted, not rejected at `buf`'s own
/// physical edge -- `render_row`'s own `clip_to_row` (below) is relied on to clamp this back to
/// `line.end` regardless, so nothing downstream needs to know about it separately.
///
/// **The resolved-criterion belt (batch 5 (2026-07-26), finding #6):** `accept_and_hay_for_row`'s
/// own `+ CTX_AHEAD - 1` widening (above) guarantees a full character's worth of real lookahead
/// past any within-cap candidate's own end, UNLESS `hay`'s own end here is itself clamped to
/// `full_hay`'s own edge (`full_hay.len()`).
///
/// **Correction (fix round (2026-07-27), P2-3):** an earlier version of this paragraph claimed
/// that clamp is "reachable only when the source is truncated short of what `self.size` claims" --
/// **false, twice over**, and retracted here rather than silently edited (this project's own
/// established style: `docs/search.md`'s own P2-2 retraction, above, is the precedent). First,
/// `hay.end == full_hay.len()` is the ORDINARY case for the LAST row of any viewport at all, on a
/// perfectly healthy, untruncated source: `edge_context`'s own AHEAD-side fetch reaches exactly
/// `full_hay.len()` there by construction, no truncation involved -- it is on the hot path, not a
/// rare corner. Second, the belt's own DROP (not just its clamp) is reachable on a perfectly
/// healthy source too, for any OVER-CAP candidate (one whose own body extends `>= MAX_MATCH_LEN`
/// bytes past the row's own accept end): no margin, widened or not, can ever be enough for such a
/// candidate, so the belt drops it regardless of truncation -- a DELIBERATE, ACCEPTED, position-
/// dependent trade (batch 5 (2026-07-27) fix round, P1-1, CONTROLLER RULING: no fabrication beats
/// false paint, and nav already fabricates on this same class, F-review P2-4, docketed) pinned
/// directly, in both directions, by the test below that R5 (2026-07-28) RENAMED from
/// `..._over_cap_matches_paint_nothing_a_position_dependent_documented_miss` to
/// `visible_window_matches_over_cap_assertion_free_matches_paint_again_the_r5_recovery` -- the
/// trade narrowed to ASSERTION-BEARING patterns there, and this citation kept the retired name
/// (caught by the guard's wrapped-citation handling, batch 19 (2026-07-31)).
///
/// A candidate ending within `CTX_AHEAD` of `hay`'s own (clamped) edge is UNRESOLVED, not
/// fabricated: the filter below drops it unless that same edge is ALSO the file's true end
/// (`RowSearch::hay_reaches_eof`, `Document::viewport`'s own doc comment) -- a real boundary
/// needing no margin, the identical rule every other windowed scanner in this engine already
/// applies. A documented miss, consistent with nav's own authority, never a fabrication reusing an
/// unverified cut as if it were real EOF.
///
/// **EOF zero-width parity (batch 5 (2026-07-26), finding #7):** a zero-width match sitting
/// exactly at `buf`'s own TRUE end (`top + buf_len == self.size`, and not the trailing newline's
/// own phantom -- `RowSearch::eof_zero_width_ok`, `Document::viewport`'s own doc comment) is
/// structurally unreachable through `find_all_starting_in`'s own loop no matter how `accept`/
/// `hay` are widened: that loop can never examine a start AT its own accept upper bound
/// (exclusive), and this position IS that bound whenever `buf` truly ends there (`full_hay_len ==
/// buf_hi` then, `ctx_after` necessarily empty). Handled separately, below, by mirroring
/// `Document::viewport`'s own `current_match` resolution (the identical `find_starting_in` call,
/// already inclusive of this exact position) rather than coercing the enumerating loop.
///
/// The SAME absolute match can legitimately surface from more than one row's own enumeration (a
/// multiline match's own leftward reach can cross into an earlier row's own accept range too) --
/// harmless, not double-counted: `clip_to_row` (below) keeps only the portion intersecting THIS
/// row's own bounds regardless of which row's enumeration found it, so a row whose own bounds a
/// given match does not intersect drops it silently either way.
///
/// `find_all_starting_in`'s own contract bounds a candidate's END to its OWN accept parameter,
/// not just its START (unit B's own doc comment: "wholly contained," never partially reported) --
/// correct for its ORIGINAL caller, where accept already spanned the entire region a match could
/// ever render into (the whole buf), so nothing legitimate could ever need to extend past it.
/// Handing it THIS row's own narrow `accept` directly would silently lose exactly the
/// `foo\nbar`-style coverage batch-3 #10 fixed (a multiline match's own tail extending into a
/// LATER row). So the call below searches over `accept.start..hay.end` (the FULL hay reach,
/// already widened enough for any admissible match's own tail to complete within it --
/// `accept_and_hay_for_row`'s own doc comment) as `find_all_starting_in`'s OWN accept parameter,
/// then filters the results in two directions: `s < accept.end` (a start past the row's own
/// window paints nothing of it, regardless of how far its own end reaches -- the brief's own
/// two-direction argument) and, fix round (2026-07-25) F3, `e >= win_start_abs` (a candidate
/// ENDING before the window's own left edge paints nothing of it either, and non-overlapping
/// matches mean at most ONE result can straddle in from the leftward `MAX_MATCH_LEN` zone and
/// still satisfy that -- every other survivor starts within the `win_bytes`-wide window-adjacent
/// range (`win.end - win.start`, NOT `cols` -- fix round 2 (2026-07-25), R2: bytes and columns
/// only coincide for single-byte content), making the returned count genuinely `O(win_bytes) =
/// win_bytes + 2` (the one possible straddler, plus the `+1` accept admission), never
/// `O(MAX_MATCH_LEN)`.
fn visible_window_matches(
    search: RowSearch<'_>,
    buf_len: usize,
    raw_row: &[u8],
    line_start: usize,
    tab_stop: usize,
    hscroll: usize,
    cols: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut win = crate::line::visible_byte_range(raw_row, tab_stop, hscroll, cols);
    if win.start == win.end {
        // fix round (2026-07-25), F1: an EMPTY row's own `visible_byte_range` is ALSO `0..0` --
        // the identical value "hscroll past all content" and "cols == 0" return -- but it is not
        // the same situation: a blank line still has exactly one honest cursor position (byte 0)
        // whenever the window's own left edge sits there too (`hscroll == 0`) and the window has
        // any width at all (`cols > 0`) -- the same position `layout_row_with_marks`'s own `col`
        // fallback already renders a zero-width match onto
        // (`zero_width_mark_on_empty_line_renders_at_the_first_cell`, line.rs). The `hscroll == 0`
        // gate below is a SHORT-CIRCUIT, not a correctness requirement (N1, fix round 2
        // (2026-07-25)): a scrolled-away blank row would still render nothing even without it --
        // `layout_row_with_marks`'s own `raw < skip_cols` drop already discards the mark one layer
        // down (its zero-width branch's `raw` fallback is `0` for an untouched blank row, `<
        // skip_cols` for any `hscroll > 0`) -- verified by removing the gate: output byte-for-byte
        // identical, full suite green. Kept anyway, purely to skip a pointless accept/hay/regex
        // pass over a blank row nothing will ever admit. Anything else landing on `win.start ==
        // win.end` (real content scrolled fully off, or no columns at all) genuinely has nothing
        // visible.
        if crate::line::stripped_len(raw_row) == 0 && hscroll == 0 && cols > 0 {
            // fix round 2 (2026-07-25), R1: SAME widening as the branch below, not a no-op fall-
            // through -- a genuinely empty row (`raw_row.len() == 0`) still widens to 0 (no
            // change), but a blank CRLF row (`raw_row == b"\r"`, `stripped_len == 0` but
            // `raw_row.len() == 1`) widens to 1, admitting a bare `$` sitting one byte past the
            // stripped '\r' -- exactly the gap F4 closed for every OTHER row shape but this one.
            // Collapsed rule, stated once: whenever the walk consumed the whole body -- empty to
            // begin with, or fully walked without the window cutting it short -- `win.end` is the
            // row's own true length, `\r` included.
            win.end = raw_row.len();
        } else {
            return Vec::new();
        }
    } else if win.end == crate::line::stripped_len(raw_row) {
        // fix round (2026-07-25), F4 (see R1, above, for the collapsed rule stated once): the
        // walk consumed the WHOLE (stripped) body without the window cutting it short -- `win.end`
        // tracks how far the walk got, which stops at the stripped body's own length here, not
        // `raw_row`'s own true length, whenever a trailing '\r' was stripped before walking
        // (CRLF). A bare `$` sits exactly on that stripped byte (the position right after it,
        // before the row's own real '\n' terminator) -- widening to `raw_row.len()` (the row's own
        // honest end, `\r` included) restores the reach the "+1" admission was always meant to
        // provide. Safe even when there is no stripped byte (`raw_row.len() == stripped_len` then,
        // a no-op) and even at the ambiguous "exactly fills the window" boundary (#7(a)'s own
        // `raw >= end` drop still applies downstream regardless of what accept admits here --
        // over-admission can never manufacture a rendered mark, the same argument #7(b)'s own
        // audit made).
        win.end = raw_row.len();
    }
    // fix round (2026-07-25), F3: this row's own true visible-window LEFT edge, in `search.
    // parent`'s own `Local` coordinates -- captured before `win` is consumed below, used only to
    // filter results, never to change what gets searched.
    let win_start = crate::search::hay::Local(search.lb + line_start + win.start);
    // batch 5 (2026-07-26), finding #7: does THIS row's own window reach `buf`'s own physical
    // end? (`win.end` was widened, above, to `raw_row.len()` -- `\r` included -- whenever the
    // walk consumed the whole body; only the true FINAL row can ever satisfy this, since every
    // earlier row's own `raw_row` stops at a real `\n`, strictly before `buf_len`.)
    let reaches_buf_end = line_start + win.end == buf_len;
    // fix round (2026-07-27), P2-2: the STRUCTURAL reason `RowSearch::eof_zero_width_ok`'s own
    // phantom half can never matter here, stated as a checked invariant rather than left an
    // untested assumption -- `reaches_buf_end` can only ever become true for `viewport`'s own
    // FINAL, UNTERMINATED row (`document.rs`'s own row-building loop: every terminated row's own
    // `\n` is a valid, in-bounds index strictly below `buf_len`, so its own `raw_row` can never
    // reach `buf_len`; and that final-row branch itself is only ever entered when `line_start <
    // buf.len()`, which is FALSE whenever the newline loop has already consumed a trailing `\n` as
    // part of the row before it) -- meaning whenever `reaches_buf_end` holds, `raw_row` IS `buf`'s
    // own tail and its own last byte is provably never `\n`. `phantom_at_eof` (`Document::
    // viewport`'s own doc comment) can therefore never coexist with `reaches_buf_end`, which is
    // exactly why `eof_zero_width_ok`'s own phantom half is structurally unreachable through this
    // path (verified by mutation, not merely derived: dropping it from `eof_zero_width_ok` passes
    // the whole suite). Debug-only: this reasons about `raw_row`'s own construction, not this
    // function's own runtime behavior, so it costs nothing in release builds.
    debug_assert!(
        !reaches_buf_end || raw_row.last() != Some(&b'\n'),
        "reaches_buf_end implies raw_row cannot end in \\n (see the comment above); if this ever \
         fires, the row-building invariant changed and eof_zero_width_ok's own phantom half is no \
         longer dead code here"
    );
    let (accept, window) =
        accept_and_hay_for_row(win, line_start, buf_len, search.lb, search.parent.len().0);
    let buf_hi = search.buf_hi;
    let mut out = if accept.start.0 >= accept.end.0 || window.start.0 >= window.end.0 {
        Vec::new()
    } else {
        // restructure R4 (2026-07-27): a NARROWED sub-view (`Hay::subhay`'s own doc comment on
        // why -- a per-row search must not re-scan the whole parent hay, or this regresses the
        // cost batch 4 #5 fixed), not the parent's own full bytes. `Hay::all_verified_row`
        // internally searches `accept.start..body_limit` (this row's own FULL reach -- the
        // multiline argument the doc comment above still carries), filters `s < accept.end` and
        // `e >= win_start`, and applies today's viewport belt (`Hay::viewport_belt`, batch 5
        // findings #6/#7, centralized restructure R4) -- one shared implementation, not a
        // hand-duplicated copy at each of this module's two call sites.
        let row_hay = search.parent.subhay(window.clone(), accept.clone());
        let win_start_row = crate::search::hay::Local(win_start.0.saturating_sub(window.start.0));
        let buf_hi_row = crate::search::hay::Local(buf_hi.0 - window.start.0);
        row_hay
            .all_verified_row(
                search.pattern,
                win_start_row,
                buf_hi_row,
                search.eof_zero_width_ok,
            )
            .into_iter()
            .map(|(s, e)| (s.0 + window.start.0 - search.lb)..(e.0 + window.start.0 - search.lb))
            .collect()
    };
    // batch 5 (2026-07-26), finding #7: EOF zero-width parity. `Hay::all_verified_row`'s own
    // underlying `find_all_starting_in` walk (search.rs) can never examine a start AT its own
    // accept upper bound (exclusive), so a zero-width match sitting exactly at `buf`'s own true
    // end is structurally unreachable through the call above no matter how far `accept`/`window`
    // are widened. Mirror `Document::viewport`'s own `current_match` resolution instead (the
    // identical `Hay::point_verified` call, on the PARENT hay -- already inclusive of this exact
    // position, `buf_hi..buf_hi + 1`) rather than coercing that loop into admitting a start at
    // its own accept upper bound.
    if reaches_buf_end
        && search.eof_zero_width_ok
        && let Some((s, e)) =
            search
                .parent
                .point_verified(search.pattern, buf_hi, buf_hi, search.eof_zero_width_ok)
    {
        out.push((s.0 - search.lb)..(e.0 - search.lb));
    }
    out
}
/// The search context every row's own `visible_window_matches` call shares: `pattern` to match,
/// `parent` -- restructure R4 (2026-07-27): the WHOLE-call `Hay` (`ctx_before ++ buf ++
/// ctx_after`, `Document::viewport`'s own doc comment), a reference rather than the pre-R4 bare
/// `full_hay: &[u8]` -- `RowSearch` no longer carries `hay_reaches_eof` separately: that fact IS
/// `parent`'s own `high` edge now (`Hay::viewport_belt`'s own doc comment), one less field that
/// could drift from what it is supposed to mean. `lb` (`ctx_before`'s own length -- `buf`'s own
/// absolute start within `parent`) stays a plain field rather than a derived `parent.to_local`
/// call: every consumer of it (`visible_window_matches`'s own `win_start`/output-coordinate
/// arithmetic) works in `buf`-relative `usize`, not `parent`-relative `Local`, and re-deriving it
/// from `parent` on every use would cost a `top.offset()` this struct does not otherwise need to
/// carry. A named, `Copy` struct (a `Hay` reference is `Copy` even though `Hay` itself is not)
/// rather than a bare tuple purely to keep `visible_window_matches`'s own argument count under
/// clippy's limit; carries no behavior of its own.
#[derive(Clone, Copy)]
struct RowSearch<'a> {
    pattern: &'a crate::search::SearchPattern,
    parent: &'a crate::search::hay::Hay<'a>,
    lb: usize,
    /// `buf`'s own edge, in `parent`'s own `Local` coordinates (`payload_end` in `Document::
    /// viewport`'s own doc comment) -- the one position the phantom/zero-width-EOF rule (`Hay::
    /// viewport_belt`) can ever admit a zero-width match at.
    buf_hi: crate::search::hay::Local,
    /// batch 5 (2026-07-26), finding #7: is `buf`'s own physical end BOTH the file's true end AND
    /// not the trailing newline's own phantom -- i.e. may a zero-width match legitimately be
    /// admitted there? `Document::viewport`'s own doc comment has the derivation.
    eof_zero_width_ok: bool,
}
/// FROZEN copy of `viewport`'s own PRE-restructure CALLER-SIDE arithmetic (batch 4 (2026-07-24),
/// finding #5): one `find_all_starting_in` call over the WHOLE payload span, regardless of any
/// row's own window, never touched again after being pasted here. Fix round (2026-07-25), F5 --
/// downgraded from an earlier, overstated claim: this is a frozen CALLER, not a frozen
/// COMPUTATION -- it still calls the LIVE `find_all_starting_in`, so a change inside THAT method
/// moves both sides of the equivalence tests below together, unlike `line.rs`'s own
/// `reference_layout_row` (a genuinely independent, self-contained body, the precedent this was
/// modeled on). The right scope for THIS restructure regardless: what changed is the CALLER-side
/// accept/hay arithmetic (`visible_window_matches`, `accept_and_hay_for_row`), which this
/// independently reproduces the pre-restructure shape of; the equivalence tests below compare
/// `visible_window_matches`'s own per-row output (via `render_row`, the real, live `viewport`
/// path) against `clip_to_row`-ing THIS list, never against `visible_window_matches` itself.
#[cfg(test)]
fn reference_all_matches(
    pattern: &crate::search::SearchPattern,
    hay: &[u8],
    lb: usize,
    buf_len: usize,
) -> Vec<std::ops::Range<usize>> {
    let payload = lb..(lb + buf_len);
    pattern
        .find_all_starting_in(hay, payload)
        .into_iter()
        .map(|(s, e)| (s - lb)..(e - lb))
        .collect()
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
/// newline-terminated and the final-unterminated-line sites. `search`, when `Some`, is a
/// `RowSearch` (`pattern`, `full_hay` -- `ctx_before + buf + ctx_after`, `viewport`'s own doc
/// comment -- and `lb`, `ctx_before`'s own length, i.e. buf's own absolute start within it); this
/// row's own match list is derived HERE, per row, via `visible_window_matches` (batch 4
/// (2026-07-24), finding #5) rather than filtered from a list `viewport` computed once over the
/// whole payload.
/// `current_match` is unchanged -- still ABSOLUTE (into `buf`), still resolved once by `viewport`
/// itself, independent of any row's own enumeration (its own doc comment there). Both are clipped
/// to `line`'s own bounds here, per row, via `clip_to_row` above.
///
/// `current`'s own visible span is resolved with a SECOND, single-match `layout_row_with_marks`
/// call, not a lookup into the first call's own (already merged) `spans`: the merged list no
/// longer remembers which original match contributed which cells, so re-laying out just the one
/// matched byte range -- cheap, at most once per screen, only for the row that actually contains
/// it -- is simpler than threading that bookkeeping through `layout_row_with_marks`'s own return
/// value. Once resolved, `current`'s own span is folded into `spans` too, if not covered by an
/// entry already there (finding #11(a)): `current`, now resolved independently of any row's own
/// enumeration (see `viewport`'s own doc comment), can land on a span neither `find_all` nor
/// `find_all_starting_in` ever reported -- an `n` on an overlapping start -- and `RowMarks`'s own
/// contract (see its doc comment) requires `current` to always be contained in `spans`, since
/// BOLD is only ever painted on top of an already-REVERSED cell (`render_viewport`'s own doc
/// comment, `ress/src/render.rs`).
fn render_row(
    buf: &[u8],
    line: std::ops::Range<usize>,
    tab_stop: usize,
    hscroll: usize,
    cols: usize,
    search: Option<RowSearch<'_>>,
    current_match: Option<&std::ops::Range<usize>>,
) -> (String, RowMarks) {
    let slice = &buf[line.clone()];
    let byte_marks: Vec<std::ops::Range<usize>> = match search {
        Some(search) => visible_window_matches(
            search,
            buf.len(),
            slice,
            line.start,
            tab_stop,
            hscroll,
            cols,
        )
        .iter()
        .filter_map(|m| clip_to_row(m, &line))
        .collect(),
        None => Vec::new(),
    };
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
    use crate::search::hay::{Abs, Edge, Hay, Local};
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
    // fix round (2026-07-25), F5: an OWNED-data sibling of `doc`, above -- used only where a
    // fixture is built at runtime (e.g. `Vec<u8>` long enough to exercise `line_start`/`hscroll`
    // past `MAX_MATCH_LEN`) and leaking it to get `&'static` would be pure test-only noise.
    fn doc_owned(data: Vec<u8>, block_size: usize) -> Document {
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
    async fn search_next_backward_composed_budget_is_additive_not_over_granted() {
        // unit-G (batch-5 #2), GREEN: `resolve_found` used to derive the follow-up line-start
        // hunt's own budget as `budget - matched`, where `matched` is `scanned()` -- deliberately
        // excluding the straddle seed's own spend (ERRATUM 3c#2, correct for PROGRESS but wrong
        // for BUDGET). The seed's real cost was invisible to that subtraction, so the follow-up
        // hunt was over-granted by roughly the seed's own size -- ~98 physical reads for this
        // exact fixture (block size 1, budget 64, needle 1000 bytes above a 33-byte gap to EOF --
        // sized so the straddle seed is bounded by hitting EOF, not by chunk): 33 (seed) + 6
        // (payload) + 1 (the line-start shortcut's own predecessor probe) + 58 (the follow-up
        // hunt, over-granted `64 - 6` instead of the true remaining allowance).
        //
        // Now (restructure R3): the search leg's own Meter is handed whole into the follow-up
        // hunt (`SearchLeg::into_meter` -> `Meter::impose_allowance` -> `BackwardScan::
        // with_meter`), so `charged()` is additive across BOTH phases by construction --
        // instrumented directly (temporarily, during this fix) to confirm: the leg's own step
        // charges 58 (33 seed + 6 payload + ~19 per-iteration look-behind ctx, all bookkeeping,
        // `progressed()` alone would have massively understated it, exactly finding #2's own
        // point), leaving 6 of the 64-byte budget for the hunt, which charges EXACTLY that much
        // (58 -> 64) before `out_of_budget()` correctly refuses a 7th byte -- ZERO overshoot here,
        // since `block_size == 1` makes every read exactly one byte, so there is no partial-block
        // remainder to round up. `src.read_count()` (physical reads) sits BELOW `charged()`
        // (46 < 64): the hunt's own first touch, position 999, was already cached by the line-
        // start shortcut's own predecessor probe at that identical position, moments earlier.
        //
        // The bound below is therefore `budget + 1`, not a bigger guess: `charged()` itself never
        // exceeds `budget` by more than one block's worth on the very last read of either phase
        // (`block_size` here, i.e. at most 1 extra byte per phase -- 2 total, though this fixture
        // happens to land on exactly 0), PLUS the one deliberately uncharged read this whole
        // restructure leaves outside the Meter: `line_start_shortcut`'s own predecessor probe
        // (a single O(1) lookup via `cache.block()` directly, not a scanning loop -- argued, not
        // an oversight, in the R3 report). `budget + 3` gives that slack a little room without
        // losing the "vastly less than the OLD ~98" claim; the lower bound rules out a vacuous
        // pass (e.g. a regression that made the search return `End`/`Exhausted` instead of
        // actually composing two budgeted phases).
        let mut data = vec![b'x'; 1000];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'x'; 33]);
        let src = Arc::new(MockSource::new(data));
        let d = Document::new_unindexed(
            src.clone(),
            Config {
                block_size: 1,
                prefetch_depth: 0,
                nav_scan_budget: 64,
                ..Config::default()
            },
        );
        let pattern = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let outcome = d.search_next(1006, &pattern, false).await.unwrap();
        assert!(
            matches!(outcome, Resolution::Pending(_)),
            "this fixture has no newline anywhere below the match, so the line-start hunt \
             genuinely cannot resolve within any bounded budget -- still Pending, now for the \
             correct reason (budget genuinely exhausted, not artificially inflated)"
        );
        let reads = src.read_count();
        assert!(
            reads <= 64 + 3,
            "bounded (today): budget (64) + a block's worth of overshoot on each phase's own \
             last read (block_size 1, so at most 1 each) + the one deliberately uncharged \
             shortcut-probe read -- {reads} physical reads"
        );
        assert!(
            reads > 40,
            "not vacuous: the seed (33) alone already exceeds this -- {reads} physical reads"
        );
    }
    #[tokio::test]
    async fn search_next_backward_composed_budget_is_additive_charged_directly() {
        // fix round, P2-2: unit G's own headline claim ("there is currently NO test asserting a
        // read was charged") needs a DIRECT `charged()` assertion, not only the physical-
        // read-count proxy the sibling test above uses -- that test's own report citation
        // ("the charge tops out at exactly 64") came from TEMPORARY debug instrumentation
        // removed before finalizing, so nothing PERMANENT actually asserted it (the adversarial
        // review's own P2-2 finding). Constructs the identical composition `Document::
        // resolve_found` builds (`SearchLeg::into_meter` -> `Meter::impose_allowance` ->
        // `BackwardScan::with_meter`) directly, at the scan level, so `charged()` is a real,
        // permanent assertion instead of a print statement that got deleted.
        let mut data = vec![b'x'; 1000];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'x'; 33]);
        let src = Arc::new(MockSource::new(data));
        let c = crate::cache::BlockCache::new(src.clone(), 1, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut leg = crate::scan::SearchBackward::new_leg(
            pattern,
            1006,
            crate::scan::Bound::Inclusive(0),
            64,
        );
        let match_at = match leg.step(&c).await.unwrap() {
            crate::scan::SearchStep::Done(crate::scan::SearchEnd::Found { match_at, .. }) => {
                match_at
            }
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(match_at, 1000, "ground truth: \"needle\" starts at 1000");
        let leg_charged = leg.charged();
        assert!(
            leg_charged > 0,
            "the search leg's own straddle seed and payload read must have charged something"
        );
        let mut meter = leg.into_meter();
        meter.impose_allowance(64);
        assert_eq!(
            meter.charged(),
            leg_charged,
            "impose_allowance must not itself charge anything -- only set a ceiling"
        );
        let mut hunt = crate::scan::BackwardScan::with_meter(match_at, 1, meter);
        match hunt.step(&c).await.unwrap() {
            crate::scan::BwdStep::More(..) | crate::scan::BwdStep::Exhausted => {}
            other => panic!(
                "no newline exists anywhere below the match in this fixture; expected an \
                 unresolved, resumable outcome, got {other:?}"
            ),
        }
        // additive by construction, capped by the shared allowance: the composed total is the
        // leg's own spend PLUS whatever the hunt charged before running out, never more than the
        // 64-byte budget by more than one block's own width (here, at most 1 byte, since
        // `block_size == 1` leaves no partial-block remainder to round up) -- no subtraction
        // anywhere in this path to have gotten wrong (batch-5 finding #2's own root fix).
        assert!(
            hunt.charged() <= 65,
            "the composed total must never exceed the shared budget by more than one block's \
             own width -- {} charged (budget 64, block_size 1)",
            hunt.charged()
        );
        assert!(
            hunt.charged() > leg_charged,
            "the hunt must have charged something beyond the leg's own {leg_charged} -- {} \
             charged total",
            hunt.charged()
        );
    }
    #[tokio::test]
    async fn search_next_never_errors_across_a_composed_budget_sweep() {
        // fix round (2026-07-27), P1-2 -- credited, adopted verbatim from the adversarial
        // review's own probe (`.superpowers/sdd/restr-R3-review-probes.rs`,
        // `REVPROBE_composed_budget_sweep_never_errors`). `resolve_found` imposes the FULL
        // `nav_scan_budget` as a TOTAL ceiling on a meter the search leg has ALREADY spent from
        // -- whenever the leg's own `charged` has already reached that ceiling, the follow-up
        // line-start hunt's very first read used to return `OutOfBudget` with nothing charged,
        // and `BwdStep::More(self.meter.progress_witness()?)` propagated `NoProgress` as a hard
        // error out through `search_next` instead of `Resolution::Pending`. `BwdStep::Exhausted`
        // (this fix round) is what closes it: sweeps every budget in the window the report's own
        // instrumentation predicted (the leg charges 58 on this fixture, so 55-58 leave nothing
        // for the hunt) and asserts none of them error. Verified against both shas during the
        // review: all 200 budgets `Ok` at e9c7e9a; budgets 55-58 `Err` at the frozen 0d28bdc.
        let mut data = vec![b'x'; 1000];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'x'; 33]);
        let pattern = Arc::new(SearchPattern::compile("needle", false).unwrap());
        let mut errs = Vec::new();
        for budget in 1usize..=200 {
            let src = Arc::new(MockSource::new(data.clone()));
            let d = Document::new_unindexed(
                src.clone(),
                Config {
                    block_size: 1,
                    prefetch_depth: 0,
                    nav_scan_budget: budget,
                    ..Config::default()
                },
            );
            match d.search_next(1006, &pattern, false).await {
                Ok(_) => {}
                Err(e) => errs.push((budget, format!("{e}"))),
            }
        }
        assert!(
            errs.is_empty(),
            "search_next must never fail with an error on an ordinary budgeted search; \
             failing budgets: {errs:?}"
        );
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
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|offset, _len| {
                    Box::pin(async move {
                        if offset == 0 {
                            Ok(bytes::Bytes::from_static(b"a\nb\n"))
                        } else {
                            Err(anyhow::anyhow!("boom"))
                        }
                    })
                })
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
    async fn search_forward_across_a_gap_resolves_the_true_line_anchor_via_the_refill() {
        // batch 10 P2 -> batch 12 (2026-07-29): FIXED AT THE ROOT, not documented around.
        //
        // The defect: a match at 64 whose line actually starts at 63 (a `\n` at 62) reported
        // `top: Anchor(0)`. Batch 9 taught `SearchForward` to continue past an unreadable gap
        // instead of mistaking it for EOF; the newline that would place what it found was inside
        // that gap, unreadable by the scan and by the backward line-start hunt alike. Batch 10
        // called the result "imprecise, not invalid" on the grounds that 0 is a real line start --
        // half true (it is NOT batch 8's mid-line P1) but the wrong conclusion, because measuring
        // it showed the viewport rendered `["aaaaa"]` with ZERO marks at 1, 5 and 40 rows: the
        // match was ABSENT, and nav was reporting success for something nothing could display.
        //
        // What made it unfixable was never the scan logic -- it was that a memoized short block's
        // own tail is unreachable through the cache forever. The cache's own completion pass
        // removes that premise by re-asking the source once, so the `\n` at 62 is simply readable
        // and the anchor is the real one. `docs/architecture.md`'s `NavOutcome` contract ("`top`
        // the matched line's own anchor") is satisfied literally now, rather than approximately.
        struct GapHidesANewline;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for GapHidesANewline {
            fn size(&self) -> u64 {
                128
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        // logical file: 'a'*62, '\n' at 62, 'a' at 63, 'z' at 64, then filler.
                        let mut real = vec![b'a'; 62];
                        real.push(b'\n');
                        real.push(b'a');
                        real.push(b'z');
                        real.extend(vec![b'a'; 63]);
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        // block 0's FIRST answer delivers 5 of its own 64 bytes -- legal under
                        // `BlockSource`'s "up to `len`" contract, and it hides the `\n` at 62. The
                        // refill asks again from 5 and this source, being conforming, answers.
                        let want = if offset == 0 { 5 } else { len };
                        let end = (start + want).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let d = Document::new(
            Arc::new(GapHidesANewline),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let pattern = Arc::new(SearchPattern::compile("z", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::FoundMatch {
                top: Anchor::at(63),
                match_at: 64,
                wrapped: false,
            },
            "the `\\n` at 62 makes 63 the matched line's own start, and the refill is what makes \
             it readable; `Anchor(0)` here means the hole reopened. got {outcome:?}"
        );
        // and the anchor is one the viewport can actually render the match from -- the half batch
        // 10 asserted without measuring.
        let v = d
            .viewport(Anchor::at(63), 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            v.marks.iter().any(|m| !m.spans.is_empty()),
            "the match must be visible from the anchor nav reports; rows {:?}, marks {:?}",
            v.rows,
            v.marks
        );
    }
    /// batch 13 (2026-07-29), finding #3. **A dribbling source's block completes too, so a
    /// `FoundMatch` is always one the viewport can actually render.**
    ///
    /// The same fixture shape as
    /// `search_forward_across_a_gap_resolves_the_true_line_anchor_via_the_refill` above, with the one
    /// difference that broke it: this source answers ONE byte per read, forever, instead of
    /// answering short once and then complying. Under batch 12's `REFILL_ATTEMPT_CAP` block 0 went
    /// permanently blind after five bytes -- the `\n` at 62 stayed unreadable, `last_nl` stayed
    /// `None`, and nav returned `FoundMatch { top: Anchor(0), match_at: 64 }`: a match at 64
    /// reported against an anchor whose viewport contains neither the line nor the match, which is
    /// `resolve.rs`'s own `NavOutcome` contract broken by a hole nothing downstream could see.
    ///
    /// One byte per read is legal (`source.rs` promises only "up to `len`"), so the answer cannot be
    /// to call the source misbehaved. Retiring the cap means the refill loop just keeps asking:
    /// 64 reads for this block, bounded by its own width, every one of them charged and cancellable.
    #[tokio::test]
    async fn a_dribbling_source_still_yields_a_renderable_found_match() {
        struct OneBytePerRead {
            reads: Arc<std::sync::atomic::AtomicU64>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OneBytePerRead {
            fn size(&self) -> u64 {
                128
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let reads = self.reads.clone();
                crate::source::ReadTicket::from_fn(move |offset, _len| {
                    let reads = reads.clone();
                    Box::pin(async move {
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // logical file: 'a'*62, '\n' at 62, 'a' at 63, 'z' at 64, then filler.
                        let mut real = vec![b'a'; 62];
                        real.push(b'\n');
                        real.push(b'a');
                        real.push(b'z');
                        real.extend(vec![b'a'; 63]);
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        Ok(bytes::Bytes::copy_from_slice(
                            &real[offset as usize..offset as usize + 1],
                        ))
                    })
                })
            }
        }
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let d = Document::new(
            Arc::new(OneBytePerRead {
                reads: reads.clone(),
            }),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let pattern = Arc::new(SearchPattern::compile("z", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::FoundMatch {
                top: Anchor::at(63),
                match_at: 64,
                wrapped: false,
            },
            "the `\\n` at 62 is inside what used to be block 0's permanent blind spot; a cap of any \
             size leaves it unread and reports `Anchor(0)` for a match at 64. got {outcome:?}"
        );
        let v = d
            .viewport(Anchor::at(63), 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            v.marks.iter().any(|m| !m.spans.is_empty()),
            "and the anchor nav reports must be one the match is visible from -- the contract at \
             `resolve.rs`'s own `NavOutcome::FoundMatch`. rows {:?}, marks {:?}",
            v.rows,
            v.marks
        );
        // the cost is bounded by the hole's own width, not unbounded: two 64-byte blocks' worth of
        // one-byte reads plus the certifying read past the real end, and nothing repeated.
        let n = reads.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            n <= 4 * 64,
            "an uncapped refill is still bounded -- by `block_size` per block, once; got {n} reads"
        );
    }
    /// batch 13 (2026-07-29), finding #1's public consequence. **An EOF certificate belongs to a
    /// LENGTH, so `$` lands where the data really ends -- not where some other snapshot of the same
    /// block ended.**
    ///
    /// Batch 12 recorded finality in a `HashSet<u64>` of block indices, so any short entry that
    /// index later held inherited the certificate. Over `\nabczef` -- 7 real bytes behind a claimed
    /// 16 -- a 6-byte snapshot under a certificate earned at 7 made byte 6 look like EOF, and `e$`
    /// matched at 5 although the complete file has no such match (an `f` follows). The cache-level
    /// race that produced it is no longer constructible -- batch 14 (2026-07-30) moved completion
    /// inside `in_flight`, so concurrent callers share one read path
    /// (`concurrent_callers_share_one_completion_of_a_short_block`, `cache.rs`, which replaced the
    /// deleted `racing_refills_converge_instead_of_overwriting_each_other`). What this pins is the
    /// public answer the certificate's own length decides, which outlives either arrangement.
    #[tokio::test]
    async fn dollar_anchor_needs_the_certified_length_not_merely_a_certified_index() {
        // one short first answer, then compliance: block 0 comes back as `\n` alone, the refill
        // gathers the rest, and the read past 7 certifies the real end.
        struct ShortOnceThenCompliant;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortOnceThenCompliant {
            fn size(&self) -> u64 {
                16
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        const REAL: &[u8] = b"\nabczef";
                        if offset >= REAL.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let want = if offset == 0 { 1 } else { len };
                        Ok(bytes::Bytes::from_static(REAL)
                            .slice(start..(start + want).min(REAL.len())))
                    })
                })
            }
        }
        let d = Document::new(
            Arc::new(ShortOnceThenCompliant),
            Config {
                block_size: 16,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let e_dollar = Arc::new(SearchPattern::compile("e$", true).unwrap());
        let outcome = d
            .search_next(0, &e_dollar, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::Exhausted,
            "`e` sits at 5 with an `f` after it, so `e$` has no match anywhere in the complete \
             file. A certificate applied to the wrong length turns byte 6 into an end and \
             fabricates one. got {outcome:?}"
        );
        // and the real end still anchors what genuinely sits there, so this is not merely a
        // refusal to certify anything.
        let f_dollar = Arc::new(SearchPattern::compile("f$", true).unwrap());
        let found = d
            .search_next(0, &f_dollar, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            found,
            NavOutcome::FoundMatch {
                top: Anchor::at(1),
                match_at: 6,
                wrapped: false,
            },
            "`f$` is real: the data ends at 7, the source certified it, and the line starts at 1 \
             (after the `\\n` at 0). got {found:?}"
        );
    }
    /// A source whose `size()` overstates its real data: 5 bytes of `abcde` behind a claim of 8, so
    /// block 0 is legally short and the completion's own read at 5 gets EMPTY back -- the source
    /// itself certifying that its data ends exactly there (`cache::Block::ends_data`). The fixture
    /// batch 14 (2026-07-30)'s findings #1 and #2 both turn on: every consumer that decides "is
    /// this the end" must reach the same answer here, and three of them did not.
    struct TruncatedBelowItsClaim;
    #[async_trait::async_trait]
    impl crate::source::BlockSource for TruncatedBelowItsClaim {
        fn size(&self) -> u64 {
            8
        }
        async fn admit(&self) -> crate::source::ReadTicket {
            crate::source::ReadTicket::from_fn(move |offset, len| {
                Box::pin(async move {
                    const REAL: &[u8] = b"abcde";
                    if offset >= REAL.len() as u64 {
                        return Ok(bytes::Bytes::new());
                    }
                    let start = offset as usize;
                    Ok(bytes::Bytes::from_static(REAL).slice(start..(start + len).min(REAL.len())))
                })
            })
        }
    }
    /// batch 14 (2026-07-30), finding #1. **The viewport paints what nav found at a certified
    /// end.**
    ///
    /// The defect: over an actual `abcde` behind a claimed size of 8, forward search returned
    /// `FoundMatch` for `$` at 5 (batch 13 taught it to see the certificate) and the viewport then
    /// rendered no mark at all -- `fill_lines` dropped `Fetched::Short`'s block on the floor, so it
    /// reported `Budget` at a real, certified end and `buf_is_true_eof` came out false. Nav and the
    /// viewport disagreeing about where the file ends is the exact divergence the refill exists to
    /// end; it had simply moved one layer up.
    #[tokio::test]
    async fn viewport_paints_the_dollar_nav_found_at_a_certified_end() {
        let d = Document::new(
            Arc::new(TruncatedBelowItsClaim),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let dollar = Arc::new(SearchPattern::compile("$", true).unwrap());
        let outcome = d
            .search_next(0, &dollar, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::FoundMatch {
                top: Anchor::at(0),
                match_at: 5,
                wrapped: false,
            },
            "precondition: nav finds `$` at the real end. got {outcome:?}"
        );
        let v = d
            .viewport(
                Anchor::at(0),
                1,
                80,
                HScroll::ZERO,
                Some((&dollar, Some(5))),
            )
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec!["abcde".to_string()],
            "every real byte is rendered"
        );
        assert!(
            v.marks
                .iter()
                .any(|m| m.spans.contains(&(5..5)) || m.spans.iter().any(|s| s.start == 5)),
            "and the zero-width `$` nav just reported at 5 must be ON the screen -- rendering \
             nothing there is nav promising a match the viewport denies. rows {:?}, marks {:?}",
            v.rows,
            v.marks
        );
    }
    /// batch 14 (2026-07-30), finding #2. **A plain backward leg sees an observed real end too, so
    /// the two directions agree about where the file stops.**
    ///
    /// The defect: a plain leg carries no wrap certificate (`certified: None`), so its high-edge
    /// decision fell back to comparing the assembly's own top against `cache.size()` -- a claim a
    /// truncated source never reaches. Over the same `abcde` behind a claimed 8, every backward
    /// search whose match ends at the real end came back `Exhausted` while forward search found it.
    /// Three patterns, because the edge feeds three different assertions: `$` (end anchor), `\b`
    /// (word boundary against the end), and a literal run ending there.
    #[tokio::test]
    async fn backward_search_finds_matches_that_end_at_an_observed_real_end() {
        backward_finds_everything_ending_at_the_real_end(8).await;
    }
    /// batch 16 (2026-07-31), finding #1. **A zero-width match at the real end is found from
    /// backward, on an ACCURATELY sized file too.**
    ///
    /// Pre-existing, and adjacent to what batch 14 and 15 fixed rather than caused by them (the
    /// reviewer confirmed it reproduces on the base sha). Batch 14's endpoint probe required an
    /// OBSERVED certificate -- the thing a TRUNCATED source mints -- so `$` at the end of an
    /// ordinary `abcd`, searched backward from an origin past it, found nothing, while the very
    /// same position was being granted a true edge one line below in the same function. The edge
    /// and the probe were two spellings of one question; they are one function now
    /// (`SearchBackward::at_real_end`).
    ///
    /// The origin is 5, past the file's own end: this is the `$` a cursor sitting past EOF searches
    /// backward for, not a self-hit (which `search_backward_leg_one_does_not_admit_an_eof_self_hit`
    /// still refuses, from an origin of exactly 4).
    #[tokio::test]
    async fn backward_finds_a_zero_width_end_anchor_on_an_accurately_sized_file() {
        let d = Document::new(
            Arc::new(crate::source::MockSource::new(bytes::Bytes::from_static(
                b"abcd",
            ))),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let dollar = Arc::new(SearchPattern::compile("$", true).unwrap());
        let outcome = d
            .search_next(5, &dollar, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::FoundMatch {
                top: Anchor::at(0),
                match_at: 4,
                wrapped: false,
            },
            "`$` matches at 4, the file's own end, and 4 is strictly below the origin 5. \
             `Exhausted` here is the probe still demanding a certificate that an accurately sized \
             source never needs to mint. got {outcome:?}"
        );
    }
    /// batch 17 (2026-07-31), finding #1. **An ACCURATELY sized empty file too, where the loop
    /// never runs at all.**
    ///
    /// Batch 16 fixed the empty file behind an OVERSTATED claim -- there `hi` survives the clamp,
    /// so the payload loop runs and its own empty-slice probe fires. With an honest `size()` of 0,
    /// `hi` clamps to 0, `hi == floor`, and the loop body never executes: neither in-loop probe can
    /// be reached, and the leg fell straight through to `End`. The terminal probe is the third and
    /// last position: the one the loop never visits.
    ///
    /// The origin is 1, which is what makes offset 0 an ordinary match below it rather than the
    /// cursor's own. From origin 0 -- `origin == size == 0` -- it stays a self-hit that leg 1
    /// refuses and the wrap leg finds, pinned unchanged by
    /// `search_next_backward_finds_caret_dollar_on_an_empty_file_via_a_deliberate_wrap`.
    #[tokio::test]
    async fn backward_finds_zero_width_matches_on_an_accurately_sized_empty_file() {
        for pattern in ["$", "^", "^$", r"\B"] {
            let d = Document::new(
                Arc::new(crate::source::MockSource::new(bytes::Bytes::new())),
                Config {
                    block_size: 2,
                    prefetch_depth: 0,
                    ..Config::default()
                },
            );
            let p = Arc::new(SearchPattern::compile(pattern, true).unwrap());
            let outcome = d
                .search_next(1, &p, false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            assert_eq!(
                outcome,
                NavOutcome::FoundMatch {
                    top: Anchor::at(0),
                    match_at: 0,
                    wrapped: false,
                },
                "`{pattern}` matches at 0 on an empty file, and 0 is below the origin 1 -- so leg \
                 1 finds it and no wrap is needed. `Exhausted` here is the leg concluding nothing \
                 without ever probing the one position it could not reach by looping. got \
                 {outcome:?}"
            );
        }
    }
    /// batch 16 (2026-07-31), finding #1's other half. **The empty-slice path probes its endpoint
    /// before descending past it.**
    ///
    /// Also pre-existing. With no data at all behind a claimed size of 8, every read comes back
    /// empty, so the payload loop took its descent branch on the first iteration and eventually
    /// returned `End` -- never looking at position 0, where all four of these zero-width patterns
    /// match. `\B` is in the list on purpose: it is the one that does not depend on the high edge
    /// at all, so a fix that only widened the end-anchor handling would leave it behind.
    #[tokio::test]
    async fn backward_finds_zero_width_matches_on_an_empty_file_behind_a_claimed_size() {
        struct EmptyBehindAClaim;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for EmptyBehindAClaim {
            fn size(&self) -> u64 {
                8
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(move |_offset, _len| {
                    Box::pin(async move { Ok(bytes::Bytes::new()) })
                })
            }
        }
        for pattern in ["$", "^", "^$", r"\B"] {
            let d = Document::new(
                Arc::new(EmptyBehindAClaim),
                Config {
                    block_size: 2,
                    prefetch_depth: 0,
                    ..Config::default()
                },
            );
            let p = Arc::new(SearchPattern::compile(pattern, true).unwrap());
            let outcome = d
                .search_next(1, &p, false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            assert_eq!(
                outcome,
                NavOutcome::FoundMatch {
                    top: Anchor::at(0),
                    match_at: 0,
                    wrapped: false,
                },
                "`{pattern}` matches at 0 on an empty file, and 0 is below the origin 1. \
                 `Exhausted` here is the descent branch giving up without ever probing the \
                 position it was about to descend past. got {outcome:?}"
            );
        }
    }
    /// batch 15 (2026-07-30), finding #1. **The same searches, with the certificate and the match
    /// in DIFFERENT blocks.**
    ///
    /// Batch 14 read the end witness off whichever block the current iteration was working from,
    /// which is only ever enough when the match and the end share a block -- as they do at a block
    /// size of 8 above, where one block holds all of `abcde`. At a block size of 2 the `e` and its
    /// certificate come from block 2 while `cde$` needs a window assembled two iterations later,
    /// and `abcde$` two after that: `carry` preserved the bytes across those iterations and nothing
    /// preserved the fact about them, so the edge fell back to the claimed size and every one of
    /// these came back `Exhausted` while forward search found them.
    #[tokio::test]
    async fn backward_search_carries_the_end_witness_across_iterations() {
        backward_finds_everything_ending_at_the_real_end(2).await;
    }
    async fn backward_finds_everything_ending_at_the_real_end(block_size: usize) {
        let d = Document::new(
            Arc::new(TruncatedBelowItsClaim),
            Config {
                block_size,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        for (pattern, match_at) in [("$", 5u64), (r"e\b", 4), ("cde$", 2), ("abcde$", 0)] {
            let p = Arc::new(SearchPattern::compile(pattern, true).unwrap());
            // backward from the claimed end: the match sits below the origin, so no wrap is needed
            // and the leg is a plain one.
            let outcome = d
                .search_next(8, &p, false)
                .await
                .unwrap()
                .join_outcome()
                .await;
            match outcome {
                NavOutcome::FoundMatch {
                    match_at: got,
                    wrapped,
                    ..
                } => {
                    assert_eq!(
                        got, match_at,
                        "`{pattern}` matches at {match_at} in the real data `abcde`"
                    );
                    assert!(
                        !wrapped,
                        "`{pattern}` is below the origin; no wrap is needed"
                    );
                }
                other => panic!(
                    "`{pattern}` ends at the source's own certified end (5) and backward search \
                     must find it -- `Exhausted` here is the backward leg still measuring against \
                     the claimed size 8. got {other:?}"
                ),
            }
        }
    }
    /// batch 13 (2026-07-29), finding #4. **The seed-block reuse shortcut must reach the same
    /// end-of-data verdict the read path would have.**
    ///
    /// `SearchForward`'s payload loop reuses the block its own ctx seed already fetched when both
    /// land on one index, to avoid a redundant promoting touch. That shortcut hardcoded
    /// `certifies_end = false`, which was true when only a WHOLLY EMPTY block could certify and
    /// false the moment a short block whose end the source certified began to certify too: over an
    /// actual `abcde` behind a claimed size of 8, a `$` search from 5 could not see the real end
    /// sitting right there, ran off the top of the file, and reported what it eventually found as
    /// `wrapped: true` -- a wrap this file never needed.
    ///
    /// The certificate now travels with the block (`cache::Block`), so the two arms agree by
    /// construction instead of by remembering to keep them in step.
    #[tokio::test]
    async fn seed_block_reuse_keeps_the_certificate_so_a_dollar_at_the_real_end_does_not_wrap() {
        let d = Document::new(
            Arc::new(TruncatedBelowItsClaim),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let dollar = Arc::new(SearchPattern::compile("$", true).unwrap());
        // from 5 -- the real end itself, mid-block, so the ctx seed and the first payload fetch land
        // on the same index and the reuse arm is the one that decides.
        let outcome = d
            .search_next(5, &dollar, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::FoundMatch {
                top: Anchor::at(0),
                match_at: 5,
                wrapped: false,
            },
            "the source certified that its data ends at 5, and the reused block carries that fact: \
             `$` belongs there, reached going forward. `wrapped: true` here means the reuse arm threw \
             the certificate away again. got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn ahead_context_costs_a_block_read_only_when_real_data_sits_past_the_boundary() {
        // batch 9 (2026-07-29), finding #4. The physical-read cost of `ctx_after`'s widening has
        // been documented wrongly twice now -- first as unconditionally ZERO at a 1 MiB
        // `block_size` (a real measurement that varied `rows` while holding the viewport's OFFSET
        // fixed, and offset is the variable that decides it), then, over-correcting, as
        // unconditionally ONE whenever `buf` ends within 4099 bytes of a block boundary. Both are
        // prose about a number, so both drifted; this measures it instead.
        //
        // The deciding quantity is `want = min(consumable_reach, size - buf_end)`: crossing the
        // boundary is NECESSARY but not SUFFICIENT, because the request only reaches past it when
        // that much real file actually remains. Same viewport, same alignment (`buf` ending 100
        // bytes short of the 1 MiB boundary), two files differing only in how much follows.
        const MIB: usize = 1 << 20;
        let mut counts = Vec::new();
        for trailing in [50usize, 8192] {
            let mut data = Vec::new();
            while data.len() < MIB - 100 {
                data.extend_from_slice(&[b'a'; 63]);
                data.push(b'\n');
            }
            data.truncate(MIB - 100);
            data.extend_from_slice(&vec![b'b'; trailing]);
            let src = Arc::new(MockSource::new(bytes::Bytes::from(data)));
            let d = Document::new(
                src.clone(),
                Config {
                    block_size: MIB,
                    prefetch_depth: 0,
                    ..Config::default()
                },
            );
            let top = (MIB as u64) - 100 - 64; // one 64-byte row, ending at MIB - 100
            let pat = crate::search::SearchPattern::compile("zzzz", false).unwrap();
            // the no-search viewport first: it warms whatever `buf` itself needs, so what the
            // searched call adds on top is exactly the boundary-context cost and nothing else.
            let _ = d
                .viewport(Anchor::at(top), 1, 80, HScroll::default(), None)
                .await
                .unwrap();
            let before = src.read_count();
            let _ = d
                .viewport(
                    Anchor::at(top),
                    1,
                    80,
                    HScroll::default(),
                    Some((&pat, None)),
                )
                .await
                .unwrap();
            counts.push(src.read_count() - before);
        }
        assert_eq!(
            counts[0], 0,
            "only 50 real bytes follow `buf`, so the request never reaches the boundary at all \
             and the ahead-side gather costs nothing -- the batch-8 text claimed one read here"
        );
        assert_eq!(
            counts[1], 1,
            "with 8 KiB following, the request does cross into the next block -- exactly one \
             additional physical read, which the original text claimed was always zero"
        );
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
            block0_reads: Arc<std::sync::atomic::AtomicU64>,
            reads: Arc<std::sync::atomic::AtomicU64>,
        }
        #[async_trait::async_trait]
        impl BlockSource for FailsOnRereadOfBlockZero {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let data = self.data.clone();
                let block0_reads = self.block0_reads.clone();
                let reads = self.reads.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if offset == 0
                            && block0_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0
                        {
                            return Err(anyhow::anyhow!("block 0 unreachable"));
                        }
                        let size = data.len() as u64;
                        let start = offset.min(size) as usize;
                        let end = offset.saturating_add(len as u64).min(size) as usize;
                        Ok(data.slice(start..end))
                    })
                })
            }
        }
        let src = Arc::new(FailsOnRereadOfBlockZero {
            data: bytes::Bytes::from("aaaaaaa\n".repeat(8)),
            block0_reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        // Ruling revised (pass 7): this test then waited for a POSITIVE `Cancelled` event on the
        // one gated read, the same mechanism `status.rs`'s own test used at the time -- see the
        // REVISED note further below (batch 4 (2026-07-24), finding #11) for the mechanism this
        // test actually uses today, once that positive event stopped being reachable through the
        // cache at all. The DISTINCTION between the two tests still holds exactly as before --
        // this one proves the DROP CASCADE reaches the worker at all; `status.rs`'s proves
        // `StatusWorker`'s OWN `Drop` in isolation, anchor-tx held alive throughout -- only both
        // now prove their own claim through a positive event rather than one of the two doing it
        // through silence, then and now alike. Against the fix, the same simulated leak correctly
        // fails (`wait_for_count`'s own 5s timeout, loud, not silent). Both reverted immediately
        // after confirming.
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        // a gate, disarmed until armed below -- not a fixed latency (codex P2, PR #44 pass 7's
        // structural pass, a 3rd re-review, the shared root of 3 findings at once, this one an
        // audit find rather than one of the 3 codex flagged directly: identical shape to
        // `status.rs`'s own `dropping_the_worker_aborts_its_in_flight_step`, including the SAME
        // risk -- a fixed-latency read can complete on its own before `drop(d)` below,
        // independent of this test's own scheduling). Unlike that test, nothing here reads
        // through `src` again after the drop (the check below -- see the REVISED note further
        // down -- only watches the worker's own snapshot channel, never a fresh read), so there
        // is no later, legitimate read to protect from the gate staying armed -- no `open_gate`
        // call needed anywhere in this test.
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
        // REVISED, batch 4 (2026-07-24), finding #11: `cache.rs`'s own fetch path became a
        // detached publisher (`BlockCache::get`'s own doc comment), so the gated read no longer
        // dies with the worker `d` owns -- `drop(d)` below no longer fires a `cancelled_events`
        // on `src` at all (the detached fetch survives, unjoined, unopened, for the rest of this
        // test). What this test always actually claimed (see its own comment above: "does NOT
        // discriminate whether `.abort()` specifically... Document's drop reaching the worker at
        // all... is exactly what it was written to pin") is narrower than read-death and survives
        // intact: the worker's own `snapshot_tx` (status.rs's own `run`) lives inside that task's
        // body, so it closes if and only if the worker task itself ends, whether via the OLD
        // Arc-cycle leak this test guards against (never) or the drop cascade reaching it
        // (correctly). `borrow_and_update` immediately after subscribing, for the identical
        // reason `status.rs`'s own sibling fix needs it (RED-verified there): `status_snapshots`
        // clones a stale template, so an unconsumed backlog would otherwise resolve `changed()`
        // immediately, not on the sender closing.
        let mut snapshots = d.status_snapshots();
        snapshots.borrow_and_update();
        drop(d);
        // POSITIVE proof, pass 7's structural pass (superseding the P7-C silence ruling above):
        // the drop cascade reached all the way through to the worker task itself being torn
        // down, not left running forever by a leaked worker. See this test's own comment above
        // for why silence stopped being able to tell the two apart once the gate landed. Bounded
        // by the same diagnostic ceiling `wait_for_count` itself uses (source.rs).
        let closed = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, snapshots.changed())
            .await
            .expect("the worker's own snapshot sender never closed -- was it genuinely leaked?");
        assert!(
            closed.is_err(),
            "the drop cascade must reach the worker task itself for its snapshot sender to close"
        );
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
        // `pos` was ever clamped into the file -- `search_next` must degrade gracefully instead,
        // the same "out-of-contract positions degrade to the real bytes" contract every other
        // scan object already honors. Batch 4 (2026-07-24), finding #1 changed the mechanism
        // (an empty-domain `End`, not a clamp -- `scan.rs`'s own `search_forward_out_of_range_
        // origin_returns_end_instead_of_panicking`, renamed in this unit's own fix round for the
        // same reason) without changing this test's own contract: still no panic, still a clean
        // terminal outcome for a stale/out-of-range origin.
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
        // lies at byte 3, before origin) -- `SearchEnd::Found` reports
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
        // budget forces Pending; the gate then catches the background task's own next read
        // genuinely in-flight before cancelling.
        //
        // REVISED, batch 4 (2026-07-24), finding #11: `cache.rs`'s own fetch path became a
        // detached publisher (`BlockCache::get`'s own doc comment), so `p.cancel()` below no
        // longer tears down the physical read with it -- the detached fetch survives, unopened,
        // for the rest of this test, and `cancelled_events` never fires. What this test always
        // needed to prove -- the PENDING NAV'S OWN TASK genuinely gets torn down, not left
        // running -- was already directly, positively provable through `p.handle` (a raw
        // `tokio::task::JoinHandle`, `resolve.rs`'s own `PendingNav`, `cancel`'s own doc
        // comment: "Aborts the background scan"), no MockSource proxy needed at all:
        // `JoinError::is_cancelled()` is the direct, tokio-level fact.
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
        p.cancel();
        // bounded, fix round 1, F1: `get()`'s waiter self-heals on a closed channel and respawns
        // a fresh fetch whenever one dies without publishing -- against this test's own
        // still-armed, never-opened gate, a leaked (uncancelled) nav task would repeat that
        // spawn/gate-timeout-panic cycle every `DIAGNOSTIC_CEILING`, forever, so a bare
        // `p.handle.await` never resolves under a broken cancel (measured: hangs past 120s under
        // the identical mutation `app.rs`'s bounded twin fails loud on in 5s). Matches the other
        // bounded waits this same diff already uses (`schedule.rs`, `status.rs`, `document.rs`'s
        // own sibling snapshot/summary checks, `app.rs`).
        let joined = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, p.handle)
            .await
            .expect("the pending nav's own task never resolved -- was it genuinely aborted?");
        assert!(
            joined.is_err_and(|e| e.is_cancelled()),
            "cancelling the pending nav must abort its own task, not let it run to completion"
        );
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
        //
        // REVISED, batch 4 (2026-07-24), finding #11: see `search_next_pending_cancels_cleanly`'s
        // own comment, above -- identical reasoning, identical fix (a direct `p.handle` /
        // `JoinError::is_cancelled` proof of the NAV TASK's own death, not a `cancelled_events`
        // proxy for the read's, which the detached-publisher restructure makes untrue).
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
        p.cancel();
        // bounded -- fix round 1, F1, same reasoning as `search_next_pending_cancels_cleanly`'s
        // own comment, above: a bare `.await` here can hang forever under a broken cancel (the
        // respawn-on-closed-channel loop repeats every `DIAGNOSTIC_CEILING` against this test's
        // own never-opened gate).
        let joined = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, p.handle)
            .await
            .expect("the pending nav's own task never resolved -- was it genuinely aborted?");
        assert!(
            joined.is_err_and(|e| e.is_cancelled()),
            "cancelling the pending nav must abort its own task, not let it run to completion"
        );
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
    async fn search_next_backward_word_boundary_wrap_does_not_manufacture_a_match() {
        // batch 4 (2026-07-24), unit B, finding #4's own nav half: `\ba` over b"ba" from origin
        // 1, backward -- ground truth is `Exhausted` (`\ba` has no match in "ba": both bytes are
        // word characters, so no `\b` holds anywhere). RED-reproduced via this exact public path
        // before any fix: leg 1 (`hi = origin = 1`, floor 0) reads no context at all (`lo == 0`
        // takes priority over `lo == floor`, since floor is 0 here) and correctly finds nothing.
        // Leg 2 (the wrap) is where the false match actually lived: `wrapped_leg`'s own backward
        // floor is the NAIVE `origin` (1, not widened -- `wrapped_leg`'s own doc comment), so leg
        // 2's `hi = size = 2`, `floor = 1` -- its own main loop reaches `lo == floor == 1 > 0`,
        // exactly `SearchBackward`'s own floor-context site (scan.rs). The brief's own CAUTION
        // (uncertain whether floor > 0 was reachable here, reasoning from FORWARD's own widened
        // leg-2 floor formula) does not apply to backward, which is never widened -- confirmed by
        // running this exact fixture: pre-fix it returned `FoundMatch { match_at: 1, wrapped:
        // true }`, the sentinel (a fixed non-`\n`, non-word-implying byte) manufacturing a false
        // `\b` at the floor even though the real predecessor ('b', at position 0) is a word
        // character. Post-fix, `SearchBackward` reads the REAL byte below its own floor for
        // context (this struct's own decision point in scan.rs), so `\b` correctly evaluates
        // false there.
        let d = doc(b"ba", 1 << 20);
        let p = Arc::new(SearchPattern::compile(r"\ba", false).unwrap());
        let outcome = d
            .search_next(1, &p, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::Exhausted => {}
            other => panic!("\\ba has no match in \"ba\"; expected Exhausted, got {other:?}"),
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
    async fn search_next_backward_finds_caret_dollar_on_an_empty_file_via_a_deliberate_wrap() {
        // batch 4 (2026-07-24), finding #1's own fix round, F7 (reviewer-flagged as unremarked,
        // controller-ruled deliberate, not a bug): on an EMPTY file, `origin == size == 0`, so
        // leg 1's own EXCLUSIVE `hi` (`wrap_leg: false`) excludes the position-0 self-hit exactly
        // the same way it excludes any other self-hit, finding nothing and forcing a genuine wrap
        // to leg 2 (`wrap_leg: true`, `hi = size` unconditionally) to find the SAME `^$` match.
        // Result: a backward search on an empty file reports `wrapped: true` -- a wrap notice for
        // a file with nowhere to actually wrap around. This is the established self-hit-wraps
        // model (`search_next_backward_self_hit_wraps_to_find_it_again`, this file's own test)
        // applied consistently at size 0, not a special case carved out for it. The forward twin
        // just above reports `wrapped: false` instead -- not an inconsistency: forward's own
        // origin is INCLUSIVE (leg 1's own domain, starts >= 0, legitimately already contains the
        // match), the asymmetry is forward/backward's own pre-existing inclusive/exclusive
        // contract (`search_next_forward_origin_is_inclusive`, this file's own test), unrelated to
        // this finding.
        let d = doc(b"", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("^$", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, false)
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
                assert!(
                    wrapped,
                    "leg 1's own exclusive hi excludes the empty file's only position too, \
                     forcing the same genuine wrap every other self-hit gets"
                );
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
    async fn search_next_backward_self_hit_at_eof_now_requires_the_wrap() {
        // batch 4 (2026-07-24), finding #1: the REVERSED twin of this file's own former
        // `search_next_backward_self_hit_at_eof_is_found_directly_not_via_the_wrap` -- same
        // fixture, opposite (corrected) expectation. The cursor is ALREADY parked exactly on the
        // zero-width "$" at EOF (as if `n`/`N` just landed there) and repeats the SAME backward
        // search. For every other match shape, leg 1's own exclusive `hi` excludes this self-hit,
        // forcing a genuine wrap (`search_next_backward_self_hit_wraps_to_find_it_again`, this
        // file's own test, pins that for a NORMAL match) -- the old code treated a zero-width
        // match at EOF as immune to that, on the theory that `hi == cache.size()` can't tell leg 1
        // from leg 2, so admitting the self-hit either way was "byte-for-byte identical" and
        // therefore harmless. That theory holds only in a file with a SINGLE match. This fixture
        // (b"abc", pattern `$`) happens to have only one, so the OLD test's own assertion
        // (`wrapped: false`) could not by itself distinguish "found directly, correctly" from
        // "found directly because leg 1 wrongly self-hit" -- fix round F4 (reviewer-caught
        // mis-citation): `search_next_backward_exclusive_repeat_on_a_file_with_two_dollar_matches`
        // (this file's own sibling test, backward, using b"a\nb" where a SECOND, earlier match
        // exists) is what actually exposes the difference: there, admitting the self-hit would
        // have made that earlier match unreachable, not merely relabeled. `SearchBackward::
        // new_leg`'s own EXCLUSIVE `hi` (its own doc comment has the full reasoning; restructure
        // R6: formerly a `wrap_leg` field, now the choice of constructor) now makes leg 1
        // correctly exclude the self-hit here too, landing on the SAME position via a genuine
        // wrap instead: `wrapped: true`, not `false` -- no longer a cosmetic difference, the
        // actually-correct navigation contract.
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
                    wrapped,
                    "leg 1's own exclusive hi must now exclude the self-hit, forcing a genuine \
                     wrap to re-find it -- see SearchBackward::new_leg's own doc comment"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_wrap_does_not_fabricate_a_match_at_a_stale_size() {
        // batch 5 (2026-07-26), finding #1, end to end: the SAME stale-`cache.size()`
        // fabrication `scan.rs`'s own `search_backward_wrap_leg_terminal_check_does_not_
        // fabricate_a_match_at_a_stale_size` pins directly on `SearchBackward`, reached here
        // through the full `search_next` wrap path. A source claiming size 100 over 2 real bytes
        // (`b"a\n"`) used to resolve leg 2's terminal check to `Found { match_at: 100 }` -- and,
        // upstream of that, `resolve_found`'s own line-start hunt (independently walking backward
        // from 100) discovered the SAME truncation and settled on `top: Anchor(2)` (the real `\n`
        // at 1, plus one) -- an internally INCONSISTENT `FoundMatch { top: Anchor(2), match_at:
        // 100, .. }` (a line start numerically past its own claimed match). Correct: the wrap
        // finds the real match at 1, whose own line start is the file's own top.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"a\n".to_vec(),
            claimed_size: 100,
        };
        let d = Document::new(
            Arc::new(source),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, false)
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
                assert_eq!(
                    match_at, 1,
                    "must find the real match at 1, not a fabricated one at the stale size (100)"
                );
                assert_eq!(top, Anchor::TOP);
                assert!(wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_forward_leg_one_defers_to_the_wrap_rather_than_reporting_below_origin() {
        // batch 5 (2026-07-27), finding #10-internal, end to end: reached through LEG 1 DIRECTLY
        // -- no wrap needed to REACH the defect, which is the WIDER reachability path this
        // finding's own brief asked to be constructed rather than assumed narrow. The brief's own
        // analyst derived reachability as needing "a prior match at claimed_size - 1" (`app.rs`'s
        // own `origin = search.current.map(|m| m + 1)` arm) -- but `app.rs`'s SAME `origin`
        // computation has a SECOND arm, `.unwrap_or(app.top.offset())`, taken whenever there is no
        // previous match yet (the very FIRST search in a session). If the viewport's own top
        // happens to sit at the file's claimed end (e.g. the user has scrolled to the bottom) and
        // the file is truncated before that first search runs, `origin` lands on the stale
        // claimed size with NO prior match required at all -- strictly easier to trigger than the
        // analyst's own derivation, constructed here directly (`search_next`'s own `origin`
        // parameter, bypassing `app.rs` for a minimal fixture): a source claiming size 100 over 2
        // real bytes (`b"ab"`), searched forward from `origin = 100` exactly, with no earlier
        // match anywhere in this Document's own history.
        //
        // batch 5 fix round 2 (2026-07-27), review response, P1: renamed from `..._does_not_
        // fabricate...`, and `wrapped` flipped from `false` to `true`. The original fix made leg
        // 1 report `Found { match_at: 2, wrapped: false }` directly -- correctly avoiding the OLD
        // fabrication at 100, but introducing a NEW defect: 2 sits below `origin` (100), a match
        // START outside leg 1's own inclusive range, which the exclusive-repeat contract
        // (`document.rs:1060-1064`, "the caller alone is responsible for skipping a just-found
        // match by passing `current_match + 1`") depends on never happening -- confirmed
        // end-to-end by the reviewer's own repeat-navigation trace: pressing `n` again with
        // `origin = match_at + 1 = 3` re-certified the SAME position (2) and reported it AGAIN,
        // forever, with `wrapped: false` never signaling anything had gone around. Correct: leg 1
        // now recognizes its own certified range is empty (nothing at-or-after 100 exists) and
        // returns `End`, deferring to leg 2's own wrap -- which finds the SAME real match (2) from
        // the file's true beginning, honestly reporting `wrapped: true`. See
        // `search_next_forward_repeat_navigation_does_not_stick_on_a_stale_size`, below, for the
        // actual repeat-navigation sequence this single search only sets up.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let d = Document::new(
            Arc::new(source),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        // batch 11 (2026-07-29), P1: was `$`. This test is about leg 1 DEFERRING to the wrap
        // rather than reporting a match below its own origin -- a structural property of the
        // two-leg policy that has nothing to do with which pattern is used. `$` made it depend
        // incidentally on nav trusting an uncertified short boundary as EOF, which is exactly what
        // that finding stopped doing; a plain literal inside the real bytes exercises the same
        // deferral without resting on a fabrication.
        let pattern = Arc::new(SearchPattern::compile("a", false).unwrap());
        let outcome = d
            .search_next(100, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(
                    match_at, 0,
                    "must find the real 'a' at 0, not a fabricated match at the stale size (100)"
                );
                assert!(
                    wrapped,
                    "leg 1 has nothing at-or-after its own origin (100) in this 2-byte file, so \
                     it must return End and let leg 2's wrap find the match -- wrapped: false \
                     here would mean leg 1 reported a match START below its own origin, the P1 \
                     defect this test now pins"
                );
            }
            other => panic!("expected FoundMatch (the real 'a' at 0, wrapped), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_forward_repeat_navigation_does_not_stick_on_a_stale_size() {
        // batch 5 fix round 2 (2026-07-27), review response, P1: the reviewer's own end-to-end
        // reproduction of the livelock, mirroring `app.rs`'s own repeat-navigation arithmetic
        // directly (`origin = search.current.map(|m| m + 1)`) across several presses of `n`,
        // rather than a single search in isolation. At `825bed6` this fixture's own sequence was
        // `(100,2,false) (3,2,false) (3,2,false) (3,2,false) (3,2,false)` -- STUCK from the second
        // press on: `origin` recomputes to the SAME value (`match_at + 1 = 3`) every time because
        // `match_at` (2) never changes, and leg 1 kept re-certifying and re-reporting the SAME
        // position with `wrapped: false`, never once honestly signaling that a wrap had happened
        // (or logically needed to). Fixed (P1, above): every press past the first now resolves
        // through leg 2's own wrap, `wrapped: true` each time -- repeating the SAME single match
        // forever is the CORRECT, already-established behavior for a one-match file (matching
        // ordinary non-truncated `n` cycling), not a livelock; the defect was specifically the
        // dishonest `wrapped: false`, not the repetition itself. Presses three deep (one more than
        // the reviewer's own minimum "twice") to make the STEADY STATE, not just the transition
        // into it, unambiguous.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"ab".to_vec(),
            claimed_size: 100,
        };
        let d = Document::new(
            Arc::new(source),
            Config {
                block_size: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        // batch 11 (2026-07-29), P1: was `$`, for the same reason as the sibling test above. What
        // this test pins is that REPEAT navigation cannot stick -- three presses must keep giving
        // a real, stable answer with `wrapped` honestly reported -- which is a property of the
        // two-leg policy and the origin arithmetic, not of the pattern. `$` tied it to nav
        // trusting an uncertified short boundary as EOF; a literal inside the real bytes exercises
        // the identical sequence without resting on that.
        let pattern = Arc::new(SearchPattern::compile("a", false).unwrap());
        let mut origin = 100u64; // the viewport's own top, per the wider reachability path above
        for press in 1..=3 {
            let outcome = d
                .search_next(origin, &pattern, true)
                .await
                .unwrap()
                .join_outcome()
                .await;
            match outcome {
                NavOutcome::FoundMatch {
                    match_at, wrapped, ..
                } => {
                    assert_eq!(
                        match_at, 0,
                        "press {press}: the only real 'a' never moves in a 2-byte file"
                    );
                    assert!(
                        wrapped,
                        "press {press}: origin ({origin}) is past the only match (0) every time \
                         (it only ever advances to match_at + 1 = 1, still above 0) -- wrapped: \
                         false on ANY press here is the exact stuck-livelock symptom P1 fixes"
                    );
                    origin = match_at + 1; // app.rs's own repeat-navigation arithmetic
                }
                other => panic!("press {press}: expected FoundMatch, got {other:?}"),
            }
        }
    }

    // ---- exclusive repeat navigation at EOF (batch 4 (2026-07-24), finding #1) ----

    #[tokio::test]
    async fn search_next_forward_exclusive_repeat_on_a_file_with_two_dollar_matches() {
        // the derived truth table finding #1's own brief pins: b"a\nb" (bytes 'a','\n','b'),
        // pattern `$`, matches at 1 (before the \n) and 3 (the true end -- real, no trailing \n
        // follows it, so this is not #2's own phantom). Repeated `n` must visit BOTH and then
        // wrap, never getting stuck re-finding the same one -- the bug: `n` from 3 used to land
        // back on 3 instead of wrapping to 1, because the EOF widening silently re-admitted
        // position `size` even though leg 1's own domain (`starts >= origin`) was EMPTY once
        // `origin > size` (origin = current_match + 1 = 4).
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(b"a\nb"),
            vec![(1, 1), (3, 3)],
            "ground truth: two zero-width matches"
        );
        let d = doc(b"a\nb", 1 << 20);
        // n from the match at 1 (origin = 2): leg 1's own domain [2, 3] is non-empty (3 is
        // legitimately in range) -- finds the SECOND match directly, no wrap.
        let outcome = d
            .search_next(2, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(match_at, 3);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
        // n from the match at 3 (origin = 4): leg 1's own domain [4, 3] is EMPTY (4 > size) --
        // must wrap, landing back on the earlier match at 1, not get stuck at 3.
        let outcome = d
            .search_next(4, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(
                    match_at, 1,
                    "must wrap to the earlier match, not get stuck at 3"
                );
                assert!(wrapped, "leg 1's own domain is empty once origin > size");
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_exclusive_repeat_on_a_file_with_two_dollar_matches() {
        // the backward mirror of the forward test above, same fixture and derived truth table.
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let d = doc(b"a\nb", 1 << 20);
        // N from the match at 3 (origin = 3, no adjustment -- backward's own hi is already
        // exclusive): leg 1 searches strictly below 3 and finds the earlier match at 1 directly,
        // no wrap needed.
        let outcome = d
            .search_next(3, &pattern, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match outcome {
            NavOutcome::FoundMatch {
                match_at, wrapped, ..
            } => {
                assert_eq!(match_at, 1);
                assert!(!wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
        // N from the match at 1 (origin = 1): leg 1 searches strictly below 1 and finds nothing
        // (byte 0 is 'a', not a match) -- must wrap, and leg 2's own INCLUSIVE far end (hi =
        // size, unconditionally) is what finds the zero-width match at 3 this time, not leg 1
        // (whose own `hi` would otherwise have excluded it as a self-hit, pre-#1).
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
                assert_eq!(
                    match_at, 3,
                    "leg 2's own inclusive far end finds the EOF zero-width"
                );
                assert!(wrapped);
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_forward_wraps_and_terminates_on_a_lone_eof_match() {
        // batch 4 (2026-07-24), finding #1: a lone EOF-only match must still terminate via a
        // genuine wrap, not bounce between legs forever -- on b"abc" with `$` (single match at
        // 3, no trailing \n), `n` from 3 (origin = 4, past size) finds leg 1's own domain empty
        // and must wrap all the way around, landing back on the SAME match with `wrapped: true`.
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let d = doc(b"abc", 1 << 20);
        let outcome = d
            .search_next(4, &pattern, true)
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
                    "the lone match must be re-found via a genuine wrap"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }

    // ---- the phantom line is never a match, anywhere (batch 4 (2026-07-24), finding #2) ----

    #[tokio::test]
    async fn search_next_forward_wraps_to_self_when_the_only_other_candidate_is_the_phantom() {
        // finding #2's own interlock with #1: b"a\n" (size 2), pattern `$`, matches at 1 (real)
        // AND 2 (regex ground truth) -- but 2 is the trailing newline's own phantom (predecessor
        // is \n), never a match, anywhere. `n` from the match at 1 (origin = 1 + 1 = 2) must
        // therefore find NOTHING in leg 1's own domain (starts >= 2 has only the now-rejected
        // phantom) and wrap all the way around to the SAME match at 1 again -- not land on
        // `Anchor(2)` (past EOF, an invalid anchor -- the blank-viewport bug this finding names)
        // and not silently stay stuck.
        let d = doc(b"a\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let outcome = d
            .search_next(2, &pattern, true)
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
                assert_eq!(match_at, 1);
                assert_eq!(top, Anchor::TOP);
                assert!(
                    wrapped,
                    "leg 1's own domain has nothing left but the rejected phantom"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }

    // ---- the backward straddle seed respects the interactive budget (batch 4 (2026-07-24),
    // finding #3) ----

    #[tokio::test]
    async fn search_next_backward_seed_budget_exhaustion_pends_and_resolves_correctly() {
        // batch 4 (2026-07-24), finding #3's own fix round, F5a (reviewer correction): this
        // test's own `Pending` outcome is NOT, by itself, evidence the seed's own budget gate
        // exists -- the main loop's identical per-call budget would force `Pending` here even
        // without it (mutation-verified: removing only the seed's own `spent < self.chunk` check
        // still leaves this test green; it fails a DIFFERENT, scan-level test instead --
        // `search_backward_straddle_seed_is_budgeted_and_resumable`'s own exact physical-read
        // count -- which is the one that actually discriminates the seed's own budgeting).
        // What this test genuinely pins is RESUMABILITY: a seed needing many `step()` calls to
        // finish (4096 bytes at a 64-byte budget) must still hand off cleanly into the main loop
        // and land on the correct match once both complete -- "needle" sits well below `origin`,
        // findable only once the whole walk (seed, then scan) reaches it.
        let mut data = vec![b'x'; 20_000];
        data[100..106].copy_from_slice(b"needle");
        let d = Document::new_unindexed(
            Arc::new(MockSource::new(data)),
            Config {
                block_size: 64,
                nav_scan_budget: 64,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let Resolution::Pending(mut p) =
            d.search_next(5000, &needle_pattern(), false).await.unwrap()
        else {
            panic!(
                "expected Pending -- neither the seed nor the main loop can finish this fixture \
                 within one 64-byte budget step"
            );
        };
        assert_eq!(p.label, "searching");
        match (&mut p.handle).await.unwrap().unwrap() {
            NavOutcome::FoundMatch {
                top,
                match_at,
                wrapped,
            } => {
                assert_eq!(match_at, 100);
                assert_eq!(top, Anchor::TOP, "no newline anywhere in this fixture");
                assert!(
                    !wrapped,
                    "leg 1 alone finds it, once the seed and the scan both finish"
                );
            }
            other => panic!("expected FoundMatch, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn search_next_backward_seed_pending_cancels_cleanly_mid_seed() {
        // the straddle seed is now resumable across `step()` calls, just like the main
        // per-block loop -- dropping the pending nav while genuinely parked INSIDE a seed read
        // must cancel cleanly (no locks held, no further reads), the identical guarantee
        // `search_next_pending_cancels_cleanly` already pins for the main loop.
        //
        // batch 4 (2026-07-24), finding #3's own fix round, F5b (reviewer correction): the claim
        // "genuinely parked inside a SEED read" (as opposed to the main loop's own read) used to
        // rest on timing alone -- true in practice (a fresh `#[tokio::test]` gives the background
        // task no chance to run before `arm_gate`, and the seed needs 4096 bytes at 64
        // bytes/step), but not asserted. This unit's own `scanned` ruling (seed bytes never count
        // toward it) gives a free, POSITIVE discriminator instead: `p.progress`'s own `scanned`
        // must still read 0 the instant a read is confirmed in-flight -- a nonzero value there
        // would mean the seed had already finished and the main loop (which DOES charge
        // `scanned`) had already started.
        let mut data = vec![b'x'; 20_000];
        data[100..106].copy_from_slice(b"needle");
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
        let Resolution::Pending(p) = d.search_next(5000, &needle_pattern(), false).await.unwrap()
        else {
            panic!(
                "expected Pending -- neither the seed nor the main loop can finish this fixture \
                 within one 64-byte budget step"
            );
        };
        let mut started = src.started_events();
        let baseline = *started.borrow();
        src.arm_gate();
        wait_for_count(&mut started, |n| n > baseline).await;
        assert_eq!(
            p.progress.borrow().scanned,
            0,
            "still mid-seed: seed bytes never count toward scanned (this unit's own ruling) -- a \
             nonzero value here would mean the main loop had already taken over"
        );
        // REVISED, batch 4 (2026-07-24), finding #11: see `search_next_pending_cancels_cleanly`'s
        // own comment for the full reasoning -- identical fix here (a direct `p.handle` /
        // `JoinError::is_cancelled` proof of the NAV TASK's own death, in place of the
        // `cancelled_events` proxy the detached-publisher restructure makes untrue).
        p.cancel();
        // bounded -- fix round 1, F1, same reasoning as `search_next_pending_cancels_cleanly`'s
        // own comment: a bare `.await` here can hang forever under a broken cancel.
        let joined = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, p.handle)
            .await
            .expect("the pending nav's own task never resolved -- was it genuinely aborted?");
        assert!(
            joined.is_err_and(|e| e.is_cancelled()),
            "cancelling the pending nav must abort its own task, not let it run to completion"
        );
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
            async fn admit(&self) -> crate::source::ReadTicket {
                crate::source::ReadTicket::from_fn(|offset, _len| {
                    Box::pin(async move {
                        if offset == 0 {
                            let mut block = vec![b'x'; 8192];
                            block[0..6].copy_from_slice(b"needle");
                            Ok(bytes::Bytes::from(block))
                        } else {
                            Err(anyhow::anyhow!("boom"))
                        }
                    })
                })
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
        // above (`request_line_number`'s twin).
        //
        // REVISED, batch 4 (2026-07-24), finding #11: identical reasoning to that sibling's own
        // revision -- the gated read no longer dies with `d`, so `cancelled_events` never fires;
        // `search_summary`'s own sender lives partly inside the driver task (`crate::analyzer::
        // spawn`'s own `snapshot_tx`) and partly as `Document`'s own retained field clone (kept
        // for `start_search_sweep`'s own lazy-respawn contract) -- but `drop(d)` below drops
        // BOTH: `Document`'s own clone synchronously as part of the struct's own teardown, the
        // driver task's clone once the drop cascade's abort reaches it. The channel closing is
        // therefore still a valid, positive, task-scoped proof HERE specifically (unlike
        // `abort_background_and_join_reaches_the_search_sweep_too`, below, where `d` itself
        // stays alive and that field clone survives -- see that test's own comment for why it
        // needs a different fix).
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
        // `borrow_and_update` immediately after subscribing: `search_summary`'s own backlog
        // (`SweepAnalysis::begin`'s own initial publish, sent before the gated read ever starts)
        // would otherwise resolve `changed()` immediately, not on the sender(s) closing -- the
        // identical fix `status.rs`'s own sibling needs, RED-verified there.
        let mut summary = d.search_summary();
        summary.borrow_and_update();
        drop(d);
        let closed = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, summary.changed())
            .await
            .expect("the sweep driver's own summary sender never closed -- was it leaked?");
        assert!(
            closed.is_err(),
            "the drop cascade must reach the sweep driver's own task for its sender to close"
        );
    }

    #[tokio::test]
    async fn abort_background_and_join_reaches_the_search_sweep_too() {
        // the fourth owner (search final review): `abort_background_and_join` claims it awaits
        // EVERY background owner's own teardown, not just requests it.
        //
        // REVISED, batch 4 (2026-07-24), finding #11: the ORIGINAL immediate, no-polling
        // `cancelled_events` proof no longer works for two independent reasons -- the gated read
        // no longer dies with the sweep driver at all (`cache.rs`'s own detached-publisher
        // restructure), and even the WEAKER "search_summary's channel eventually closes" proof
        // `dropping_the_document_cancels_the_sweep` (above) now uses does not apply here: `d`
        // itself is NOT dropped in this test, so `Document`'s own retained clone of
        // `search_summary_tx` (`start_search_sweep`'s own lazy-respawn contract) keeps that
        // channel open regardless of the driver task's fate.
        //
        // ACCEPTED-RESIDUAL: a bounded, non-silence check in place of a positive terminal signal
        // (AGENTS.md's own 2026-07-22 policy: accepted over a structural fix when no positive
        // terminal signal exists, marked as such rather than left silently vacuous) -- the
        // GENERIC fact this test used to help prove -- that `TaskOwner::abort_all_and_join`
        // genuinely blocks until an aborted task's own teardown has run, not fire-and-forget -- is
        // already pinned once, positively, with no MockSource or cache involved at all, by
        // `task_owner.rs`'s own `abort_all_and_join_does_not_return_until_teardown_has_actually_
        // run`. What this test alone still needs to prove is narrower: that `abort_background_
        // and_join`'s own body actually reaches the search driver as its fourth call, not skips
        // it. Opening the gate only AFTER `abort_background_and_join` has already returned (safe:
        // that call no longer waits on this read at all) lets the orphaned detached fetch
        // complete -- a positive, non-silence checkpoint -- and this test's own single-threaded
        // `#[tokio::test]` runtime makes a bounded run of cooperative `yield_now`s a real
        // guarantee, not a timing gamble (`cache.rs`'s own `wait_for_in_flight_len_at_least` doc
        // comment gives the identical reasoning): a leaked driver task, once its own gated read
        // resolves, needs only a handful of reschedules to reach its own next `cache.warm` call
        // for block 1 and be caught red-handed by the assertion below.
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
        // the ONE expected read: the sweep's own first block, caught genuinely in flight on the
        // gate. Captured here, not assumed to be `baseline + 1` below without checking: `n >
        // baseline` only guarantees "at least one more," and pinning the exact count the moment
        // it is first observed is what makes the final assertion discriminate a SECOND read from
        // merely re-observing this same one.
        let after_first_read = *started.borrow();
        d.abort_background_and_join().await;
        src.open_gate();
        d.cache()
            .block(0)
            .await
            .expect("the orphaned detached fetch must still complete and publish");
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *started.borrow(),
            after_first_read,
            "a leaked search driver would have continued past block 0 by now"
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
        // find_all_starting_in/layout_row_with_marks run over the SAME buffer fill_lines already
        // read for `.rows` -- an active search must read exactly as many blocks as a plain,
        // search-less viewport call over an identical fresh document, never one more, PROVIDED
        // there is no real boundary context left to fetch: `top == Anchor::TOP` (no `ctx_before`)
        // and this fixture's own two lines fit entirely within `buf` (`buf` reaches the true
        // file end, so no `ctx_after` either) -- batch 4 (2026-07-24), finding #6 added up to two
        // extra `warm()` reads specifically for the cases THIS fixture doesn't exercise (`top >
        // 0`, or a viewport truncated before the real EOF); see `Document::edge_context`'s own
        // doc comment for the exact bound.
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

    // ---- multi-row highlighting (batch 3 (2026-07-23), finding #10; restructured for cost by
    // batch 4 (2026-07-24), finding #5): a per-row `find_all` over just that row's own byte slice
    // cannot see a match containing its own row's newline -- no single row's own slice ever holds
    // both halves. Each row's own accept range instead expands LEFTWARD by `MAX_MATCH_LEN` (a
    // flat byte range in `buf`, crossing newlines freely, `visible_window_matches`'s own doc
    // comment), so row j's own enumeration still reaches back and finds a match starting on an
    // earlier row i, and each row clips whatever ITS OWN enumeration returns down to its own
    // bounds -- the same observable result finding #10's original whole-buf pass produced, at
    // lower cost. ----

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
    async fn viewport_dollar_does_not_falsely_match_at_a_budget_truncated_edge() {
        // batch 4 (2026-07-24), finding #6: "𝄞" (U+1D11E, 4 UTF-8 bytes) is the WHOLE first row;
        // a real 'a' immediately follows in the file but past this viewport's own scan budget,
        // truncating `buf` right after 𝄞. Ground truth: "𝄞$" has NO match here -- a real byte
        // follows, not `\n`/EOF. rows=1, cols=1, hscroll=ZERO make `span=1`, so `scan_budget =
        // block_size.max(1*1*4) = 4` with `block_size <= 4` -- exactly 𝄞's own byte length, the
        // precise alignment needed to truncate `buf` right at its edge.
        let mut data = "𝄞".as_bytes().to_vec();
        data.push(b'a');
        data.extend(vec![b'z'; 50]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let d = doc(data, 4);
        let pattern = Arc::new(SearchPattern::compile("𝄞$", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 1, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            v.marks[0].spans.is_empty(),
            "a real 'a' follows 𝄞 past the budget; $ must not falsely match at buf's own edge, \
             got {:?}",
            v.marks[0].spans
        );
        // agreement check: nav must reach the identical verdict -- no "𝄞$" anywhere in the file.
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert!(
            matches!(outcome, NavOutcome::Exhausted),
            "nav and the highlighter must agree; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn viewport_absolute_start_anchor_does_not_match_past_the_true_file_start() {
        // batch 4 (2026-07-24), finding #6: `\A` means the TRUE start of the file, never merely
        // "the start of whatever buffer this viewport call happened to read" -- `top > 0` here
        // (scrolled past the first line), so "foo" (this row's own first word) sits at absolute
        // position 4, not 0; `\Afoo` must never match it.
        let d = doc(b"xxx\nfoo bar\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile(r"\Afoo", false).unwrap());
        let v = d
            .viewport(Anchor::at(4), 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            v.marks[0].spans.is_empty(),
            "\\A only holds at the true file start (position 0), not at top=4; got {:?}",
            v.marks[0].spans
        );
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

    // ---- batch 5 (2026-07-26), findings #6/#7: the viewport's end edge resolves like every
    // other edge (the highlighter's own AHEAD-side margin was too narrow, both in
    // `accept_and_hay_for_row`'s own formula and in `edge_context`'s own `ctx_after` fetch), plus
    // EOF zero-width parity (a zero-width match sitting exactly at buf's own TRUE end was missing
    // from the ordinary pass even though nav/current already found it). ----

    #[test]
    fn point_candidate_is_resolved_rejects_the_phantom_even_when_the_belt_would_allow_it() {
        // finding #7's own phantom guard, isolated at the unit level from `clip_to_row`'s own
        // (structurally always ALSO true, for this exact position -- `point_candidate_is_
        // resolved`'s own doc comment has the derivation) row-bounds rejection: a candidate
        // exactly at `payload_end`, with the belt's own margin fully satisfied (true EOF, so the
        // belt ALONE would allow it), must still be rejected when `eof_zero_width_ok` is false
        // (the phantom case) -- and must still be ACCEPTED when it's true (the ordinary EOF case,
        // `viewport_eof_zero_width_dollar_renders_ordinarily_not_only_when_current`'s own scenario,
        // above the current-match half of it).
        assert!(
            !point_candidate_is_resolved(2, 2, 2, 2, true, 2, false),
            "a candidate at payload_end, phantom (eof_zero_width_ok: false), must be rejected \
             even though the belt's own margin check (hay_reaches_eof: true) would allow it"
        );
        assert!(
            point_candidate_is_resolved(2, 2, 2, 2, true, 2, true),
            "the identical candidate, NOT a phantom (eof_zero_width_ok: true), must be accepted"
        );
        // the belt's own half, checked independently of the phantom guard (`s != payload_end`
        // trivially true here, so only the belt decides): insufficient margin, not at true EOF.
        assert!(
            !point_candidate_is_resolved(0, 3, 3, 3, false, 999, false),
            "margin 0 < CTX_AHEAD, and hay does not reach true EOF -- unresolved, reject"
        );
        assert!(
            point_candidate_is_resolved(0, 3, 3, 3, true, 999, false),
            "identical margin shortfall, but hay DOES reach true EOF -- a real boundary needs no \
             margin"
        );
    }
    #[test]
    fn visible_window_matches_interior_hay_cut_no_longer_fabricates_a_split_word_boundary() {
        // finding #6, the interior hay cut. Credited: unit F's own reviewer CONSTRUCTED this exact
        // fixture against the pre-fix code (`.superpowers/sdd/batch5-unit-F-review.md`, P2-2) --
        // reproduced here verbatim rather than invented independently. A 10_000-byte buf, a 5-byte
        // row ("HELLO", giving `accept = 0..6` exactly as the review's own derivation) followed by
        // a cap-length run of 4096 'a's (positions 5..4101) then a real "é" (a Unicode WORD
        // character, positions 4101..4103, straddling the OLD `hay = 0..4102` cut -- only é's own
        // LEAD byte visible there, a lone incomplete byte the OLD code's `(?u:\b)` decodes as
        // non-word, fabricating a boundary that isn't real) then filler out to 10_000 bytes.
        //
        // Verified, not assumed, which mechanism actually closes THIS fixture: mutating `hay_hi`'s
        // own `+ CTX_AHEAD - 1` term back out (mechanism 1b alone reverted, the belt left intact)
        // leaves this test GREEN -- the belt's own unconditional margin check independently
        // rejects the same candidate here (a maximal-length candidate's margin under the OLD,
        // narrower `hay_hi` is provably at most 1 byte, always `< CTX_AHEAD`, so the belt fires
        // regardless of whether `hay_hi` was ever widened). Since this fixture's own ground truth
        // is "no match" (é IS a word character), the two mechanisms are indistinguishable BY THIS
        // OBSERVATION ALONE -- both "correctly refuted with full margin" and "unresolved, dropped"
        // report the identical empty result. This test therefore pins the OUTCOME (no fabrication)
        // credited to the reviewer, not a specific mechanism; `accept_and_hay_ordinary_interior_
        // row_does_not_touch_bufs_own_edges` (and its two siblings, above) pin `hay_hi`'s own
        // formula directly, byte-exact; `visible_window_matches_widened_interior_margin_turns_a_
        // would_be_miss_into_a_correct_find` (below) isolates mechanism 1b's own positive value
        // with a ground truth of "yes, a match" instead.
        // fix round (2026-07-27), P3-1: this fixture used to be constructed twice, verbatim, the
        // first shadowed and discarded by the second (a stray, dead 10 KB allocation) -- one copy.
        let mut data = b"HELLO".to_vec();
        data.extend(vec![b'a'; crate::search::MAX_MATCH_LEN]);
        data.extend("é".as_bytes());
        let filler = 10_000 - data.len();
        data.extend(vec![b'z'; filler]);
        assert_eq!(data.len(), 10_000);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert_eq!(
            pattern.find_all(data),
            Vec::<(usize, usize)>::new(),
            "é is a word character; the whole-file oracle finds no real boundary after the a-run"
        );
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: false,
        };
        let raw_row = &data[0..5]; // "HELLO"
        let raw_matches = visible_window_matches(search, data.len(), raw_row, 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            Vec::<std::ops::Range<usize>>::new(),
            "must not fabricate (?u:\\b) from a split lead byte at the OLD hay cut; got {:?}",
            raw_matches
        );
    }
    #[test]
    fn visible_window_matches_widened_interior_margin_turns_a_would_be_miss_into_a_correct_find() {
        // finding #6, mechanism 1b's OWN positive value, isolated from the belt (the sibling test
        // above cannot distinguish the two: its own ground truth is "no match," which both
        // mechanisms independently produce). Identical shape to the sibling above -- "HELLO" + a
        // cap-length run of 4096 'a's -- but a real SPACE (a genuine non-word byte, so `(?u:\b)`
        // is TRUE) in place of é. The belt's own margin check is uniform/flat, never content-
        // refined (matching finding #8's own established choice, `search.rs`) -- it demands the
        // FULL `CTX_AHEAD` (4) bytes of margin regardless of how many of them a specific character
        // actually needs, so even this single-byte space needs `hay_hi`'s own widening to survive
        // the belt: verified by mutation (reverting `hay_hi`'s `+ CTX_AHEAD - 1` term alone, belt
        // intact) that this exact fixture turns from a correct FIND into a documented MISS (empty
        // `raw_matches`, not a fabrication) -- the belt's margin check rejects a 1-byte-short
        // margin regardless of the OLD code's own regex being able to correctly interpret the one
        // visible byte. With `hay_hi` widened (the shipped code), the margin is exactly `CTX_AHEAD`
        // and the match is correctly found.
        let mut data = b"HELLO".to_vec();
        data.extend(vec![b'a'; crate::search::MAX_MATCH_LEN]);
        data.push(b' ');
        let filler = 10_000 - data.len();
        data.extend(vec![b'z'; filler]);
        assert_eq!(data.len(), 10_000);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert_eq!(
            pattern.find_all(data),
            vec![(5, 4101)],
            "a real space is a genuine non-word byte; the whole-file oracle finds the boundary"
        );
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: false,
        };
        let raw_row = &data[0..5]; // "HELLO"
        let raw_matches = visible_window_matches(search, data.len(), raw_row, 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            vec![5..4101],
            "the widened margin lets this genuine match survive the belt; got {:?}",
            raw_matches
        );
    }
    #[test]
    fn visible_window_matches_resolved_criterion_belt_drops_a_dollar_satisfied_only_by_a_truncated_hays_own_edge()
     {
        // finding #6, mechanism 2 (the resolved-criterion belt). Credited: unit F's own reviewer
        // CONSTRUCTED this exact fixture too (same review, P2-2's second fixture, "even starker"),
        // reproduced verbatim. Real file `b"hello Qzzzzwwww"` (15 bytes); `buf = b"hello Q"` (7
        // bytes); `full_hay = b"hello Qzzzz"` (11 bytes) -- simulating a fetch that came back
        // SHORT of the real file's own remainder (`hay_reaches_eof: false`, exactly the truncated-
        // source case `edge_context`'s own doc comment names). Pattern `Qzzzz$`: the whole-file
        // oracle finds no match ("wwww" genuinely follows), but the truncated hay's OWN edge
        // (position 11) satisfies `$` on its own -- the resolved-criterion belt must recognize
        // this edge is NOT the true file end and drop the candidate rather than trust it.
        let real_file: &[u8] = b"hello Qzzzzwwww";
        let pattern = Arc::new(SearchPattern::compile("Qzzzz$", false).unwrap());
        assert_eq!(
            pattern.find_all(real_file),
            Vec::<(usize, usize)>::new(),
            "\"wwww\" genuinely follows; the whole-file oracle finds no match"
        );
        let buf: &[u8] = b"hello Q";
        let full_hay: &[u8] = b"hello Qzzzz";
        let hay = Hay::new(full_hay, Abs(0))
            .with_high(Edge::Cut)
            .with_body_limit(Local(full_hay.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(buf.len()),
            eof_zero_width_ok: false,
        };
        let raw_matches = visible_window_matches(search, buf.len(), buf, 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            Vec::<std::ops::Range<usize>>::new(),
            "the belt must drop this -- $ satisfied only by the truncated hay's own artificial \
             edge, not real EOF, got {:?}",
            raw_matches
        );
    }
    #[test]
    fn visible_window_matches_belt_margin_needs_all_four_ctx_ahead_bytes_not_one() {
        // fix round (2026-07-27), P2-4 -- credited, adopted verbatim from the fix-round reviewer.
        // The belt's own margin THRESHOLD is `CTX_AHEAD` (4), not merely "some positive margin" --
        // `CTX_AHEAD`'s own VALUE has now been wrong-by-default twice before in this project
        // (unit F's own review, P3-1: `CTX_AHEAD = 3` survived the whole suite; only a 4-byte
        // character discriminates) -- this is a fourth site needing the identical pin. Real file
        // `"hello Qzzzz"` (11 bytes) + a real `"é"` (2 bytes, a Unicode WORD character) + `"www"`
        // (3 bytes) = 16 bytes; `full_hay` cut to the FIRST 12 bytes -- one byte INTO é, leaving
        // exactly 1 byte of margin past the candidate's own end (position 11). Pattern
        // `Qzzzz(?u:\b)`.
        let mut real_file = b"hello Qzzzz".to_vec();
        real_file.extend("é".as_bytes());
        real_file.extend(b"www");
        let real_file: &'static [u8] = Box::leak(real_file.into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile(r"Qzzzz(?u:\b)", false).unwrap());
        assert_eq!(
            pattern.find_all(real_file),
            Vec::<(usize, usize)>::new(),
            "é is a word character; the whole-file oracle finds no real boundary after \"Qzzzz\""
        );
        let full_hay = &real_file[0..12]; // cuts one byte into é -- margin exactly 1
        let buf = &real_file[0..11]; // "hello Qzzzz"
        let hay = Hay::new(full_hay, Abs(0))
            .with_high(Edge::Cut)
            .with_body_limit(Local(full_hay.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(buf.len()),
            eof_zero_width_ok: false,
        };
        let raw_matches = visible_window_matches(search, buf.len(), buf, 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            Vec::<std::ops::Range<usize>>::new(),
            "margin exactly 1, well short of CTX_AHEAD (4) -- (?u:\\b) must not fabricate off a \
             split lead byte at margin 1; got {:?}",
            raw_matches
        );
    }
    #[tokio::test]
    async fn edge_context_crosses_a_block_boundary_to_gather_a_split_character() {
        // finding #6, mechanism 1a: the OLD `ctx_after` fetch touched exactly ONE block and asked
        // for only `CTX_BEHIND` (4) bytes -- near a block boundary this silently delivered FEWER
        // than 4 bytes (the brief's own required RED, distinct from the two fixtures above: those
        // exercise the formula/belt with plenty of real bytes already available; this one
        // exercises the FETCH itself coming up short at a block edge). `block_size: 8`, `buf_len:
        // 7` (buf = 7 'a's, positions 0..7): a real "é" (2 bytes, a Unicode WORD character) sits
        // at [7, 9) -- byte 7 is block 0's own LAST byte, byte 8 is block 1's own FIRST byte.
        //
        // fix round (2026-07-27), P3-4: this fixture pins the multi-block CROSSING (a mutation to
        // the old single-block fetch kills it) but, being only 59 bytes total, CANNOT discriminate
        // the ahead side's own WIDTH (`MAX_MATCH_LEN + CTX_AHEAD - 1` vs a bare `MAX_MATCH_LEN`):
        // `self.size - buf_end` (52) is smaller than either cap, so `min(4099, 52)` and
        // `min(4096, 52)` are identical. See `edge_context_ahead_width_needs_the_full_ctx_ahead_
        // margin_not_just_max_match_len`, below, for the dedicated width discriminator.
        let mut data = vec![b'a'; 7];
        data.extend("é".as_bytes());
        data.extend(vec![b'z'; 50]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let d = doc(data, 8);
        let pattern = Arc::new(SearchPattern::compile(r"a+(?u:\b)", false).unwrap());
        assert_eq!(
            pattern.find_all(data),
            Vec::<(usize, usize)>::new(),
            "é is a word character; the whole-file oracle finds no real boundary after the a-run"
        );
        let (_, ctx_before, ctx_after, ahead_hit_real_end) = d.edge_context(0, 7).await.unwrap();
        assert!(
            ctx_before.is_empty(),
            "top == 0, no look-behind context exists"
        );
        assert!(
            ctx_after.len() >= 2,
            "must gather the FULL é (2 bytes), crossing into block 1, not stop at block 0's own \
             last byte; got {} bytes: {:?}",
            ctx_after.len(),
            ctx_after
        );
        assert!(
            !ahead_hit_real_end,
            "self.size is accurate here (data.len() == 59) and the fetch is well within its own \
             budget (52 <= 4099) -- this completes normally, no short read involved"
        );
        let mut full_hay = data[0..7].to_vec();
        full_hay.extend_from_slice(&ctx_after);
        let hay_reaches_eof = ahead_hit_real_end || 7 + ctx_after.len() as u64 == data.len() as u64;
        let hay = Hay::new(&full_hay, Abs(0))
            .with_high(if hay_reaches_eof {
                Edge::True
            } else {
                Edge::Cut
            })
            .with_body_limit(Local(full_hay.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(7),
            eof_zero_width_ok: false,
        };
        let raw_row = &data[0..7];
        let raw_matches = visible_window_matches(search, 7, raw_row, 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            Vec::<std::ops::Range<usize>>::new(),
            "must not fabricate (?u:\\b) from a lone lead byte split across the block boundary; \
             got {:?}",
            raw_matches
        );
    }
    #[tokio::test]
    async fn edge_context_ahead_width_needs_the_full_ctx_ahead_margin_not_just_max_match_len() {
        // fix round (2026-07-27), P2-4/P3-4 -- credited, adopted verbatim from the fix-round
        // reviewer. `edge_context`'s own `want` must be `MAX_MATCH_LEN + CTX_AHEAD - 1`, not a
        // bare `MAX_MATCH_LEN`: 99 'x's, 4096 'a's, a real space (a genuine non-word byte), then
        // 100 bytes of padding so `self.size - buf_end` (4196) genuinely EXCEEDS both candidate
        // caps -- the byte BUDGET, not `self.size`, is what binds either way, so the two formulas
        // actually request different amounts (4099 vs 4096) rather than both collapsing to the
        // same `self.size`-capped value the way the smaller fixture above does.
        let mut data = vec![b'x'; 99];
        data.extend(vec![b'a'; crate::search::MAX_MATCH_LEN]);
        data.push(b' ');
        data.extend(vec![b'z'; 100]);
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile(r"a{4096}(?u:\b)", false).unwrap());
        assert_eq!(
            pattern.find_all(data),
            vec![(99, 4195)],
            "a real space is a genuine non-word byte; the whole-file oracle finds the boundary"
        );
        let d = doc(data, 100);
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 100).await.unwrap();
        assert!(
            !ahead_hit_real_end,
            "self.size is accurate and comfortably exceeds either candidate cap here"
        );
        assert_eq!(
            ctx_after.len(),
            crate::search::MAX_MATCH_LEN + crate::search::CTX_AHEAD.get() - 1,
            "must request the FULL ahead-side margin (4099), not a bare MAX_MATCH_LEN (4096); got \
             {} bytes",
            ctx_after.len()
        );
        let mut full_hay = data[0..100].to_vec();
        full_hay.extend_from_slice(&ctx_after);
        let hay_reaches_eof =
            ahead_hit_real_end || 100 + ctx_after.len() as u64 == data.len() as u64;
        let hay = Hay::new(&full_hay, Abs(0))
            .with_high(if hay_reaches_eof {
                Edge::True
            } else {
                Edge::Cut
            })
            .with_body_limit(Local(full_hay.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(100),
            eof_zero_width_ok: false,
        };
        let raw_row = &data[0..100];
        // cols must cover the WHOLE 100-byte row (buf_len), or `visible_byte_range`'s own window
        // truncates `win`/`accept` before the candidate's own start (position 99) is even in
        // reach -- unrelated to what this test exists to discriminate.
        let raw_matches = visible_window_matches(search, 100, raw_row, 0, 8, 0, 100);
        assert_eq!(
            raw_matches,
            vec![99..4195],
            "the full ahead-side margin is what lets this genuine, real cap-length match survive \
             the belt; got {:?}",
            raw_matches
        );
    }
    #[tokio::test]
    async fn viewport_trusts_a_verified_short_read_the_way_nav_does_even_against_a_stale_size() {
        // finding #6, mechanism 2's own OTHER half, fully integrated through a real `Document`
        // -- and a correction discovered mid-implementation (RED first surfaced the WRONG
        // expectation here, corrected against `docs/budgeted_scanning.md`'s own "truncation
        // policy" and `scan.rs`'s `SearchForward`'s own `take == 0` branch, not invented in
        // isolation): a source whose own `size()` overstates its real content causes `edge_
        // context`'s own multi-block `ctx_after` fetch to hit a genuinely EMPTY block -- `ahead_
        // hit_real_end` becomes `true`, and this engine's OWN established, documented policy
        // (stated once, applied to every forward-direction block-reading loop, not reinvented
        // here) is to TRUST that discovery as the real end and resolve using whatever was already
        // read, exactly as `SearchForward`'s own identical `take == 0` branch already does for
        // `search_next` -- confirmed directly: `d.search_next` on this exact fixture finds
        // `match_at: 7`, not `Exhausted`. The viewport must AGREE, not needlessly drop a candidate
        // nav itself resolves.
        //
        // fix round (2026-07-27), P2-1: the real content here is EXACTLY one whole block (8
        // bytes, `block_size: 8`) -- deliberately, so the AHEAD-side fetch's very first touch past
        // it (block index 1, offset 8) comes back GENUINELY EMPTY, not merely short of a bigger
        // block. `edge_context`'s own certification now requires exactly that (a short-but-
        // nonempty block is no longer a certificate, P2-1's own fix, see the dedicated fixture
        // below for the case this excludes) -- this fixture's own block-alignment is what keeps it
        // discriminating the CORRECTED rule rather than the superseded one.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"xxxxxxxa".to_vec(), // 8 bytes -- exactly one block, ending in a real 'a'
            claimed_size: 100_000,
        };
        let d = Document::new(
            Arc::new(source),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 8).await.unwrap();
        assert!(
            ahead_hit_real_end,
            "the AHEAD-side fetch must observe the genuinely EMPTY block past position 8"
        );
        assert!(
            ctx_after.is_empty(),
            "no real bytes exist past \"xxxxxxxa\"'s own 8 bytes"
        );
        let pattern = Arc::new(SearchPattern::compile(r"a(?u:\b)", false).unwrap());
        // nav-oracle agreement, checked FIRST (the corrected premise this test now pins): nav
        // itself resolves this via the identical `take == 0` mechanism.
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert!(
            matches!(outcome, NavOutcome::FoundMatch { match_at: 7, .. }),
            "nav trusts the verified short read as the real end (docs/budgeted_scanning.md's own \
             truncation policy); got {outcome:?}"
        );
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["xxxxxxxa".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![7..8],
            "the viewport must agree with nav -- a genuinely discovered EMPTY read is a verified \
             true end, not an unresolved truncation to distrust; got {:?}",
            v.marks[0].spans
        );
    }
    #[tokio::test]
    async fn viewport_and_nav_agree_at_a_non_block_aligned_truncation_now_by_resolving_it() {
        // restructure R3 -- FLIPPED, then UN-FLIPPED after an adversarial review, re-derived
        // (not merely re-worded) from scratch. Full history:
        //
        // batch 5, unit H, P2-NEW (pre-restructure): a genuinely truncated source (real content
        // 7 bytes, `size()` claims 100_000) whose real end does NOT fall on a block boundary.
        // `edge_context`'s own P2-1 fix required a GENUINELY EMPTY first touch to certify
        // `ahead_hit_real_end` -- correct (a short-but-nonempty answer is legal under
        // `BlockSource`'s own "up to len" contract for a reason other than real EOF, so it must
        // never certify on its own) but NARROWER than `SearchForward`'s own untouched `take == 0`
        // branch, which still trusted ANY short-or-empty read -- so the two consumers disagreed:
        // nav found the match, the viewport painted nothing. Docketed for this restructure's own
        // `CertifiedEnd` type to reconcile.
        //
        // R3's own first attempt (now REVERTED): made `fill_lines`'s own `FillOutcome::End`
        // certify via a discarded probe at the next block's own start when the boundary was a
        // merely-short block, then threaded `buf_is_true_eof` into `hay_reaches_eof` as a third
        // disjunct. This flipped the outcome to agreement -- but the certification itself was
        // unsound: an adversarial review (`.superpowers/sdd/restr-R3-review.md`, P1-1) constructed
        // `b"helloXmo"` (block 0 answers 5 of its own 8 bytes, block 1 is genuinely empty, but
        // real bytes 'X','m','o' sit at 5..8 regardless) and showed the SAME probe mechanism
        // fabricates a synthetic `$` at position 5. The two fixtures are structurally identical
        // from `fill_lines`'s own perspective -- a short-but-nonempty first block, then a
        // genuinely empty next one -- and differ ONLY in whether the file's real data happens to
        // continue into the gap between them, something the block-indexed cache cannot verify
        // once the short block is cached (it cannot be re-asked for a different slice of the
        // same block index). Trusting the next-block-empty signal to certify the SHORT block's
        // own boundary is therefore unsound in general, even though it happens to be correct for
        // THIS fixture specifically.
        //
        // Re-derived (P1-1a, this fix round): `fill_lines` now certifies `FillOutcome::End` ONLY
        // from a read that is directly, genuinely `Fetched::Empty` -- never inferred through an
        // intermediate short block. A short read that does not reach `rows` always reports
        // `Budget` (`fill_lines`'s own doc comment has the law). For THIS fixture, the very first
        // read (at position 0) is `Short` (7 of up to 1 MiB bytes) and never reaches `rows` --
        // `fill_lines` reports `Budget`, `buf_is_true_eof` stays false (its OTHER disjunct,
        // `buf_end == self.size`, also stays false here: `self.size` is the inflated 100_000
        // claim, which `buf_end` -- correctly stopped at 7 by the short read -- never reaches).
        // The flip does not survive being re-derived honestly: this test's own original name and
        // assertion (a documented, conservative miss) are restored, matching e9c7e9a's own
        // behavior exactly, not merely by coincidence -- both refuse to trust a short-but-
        // nonempty answer as the real end, for the identical reason.
        //
        // Verified at three block sizes, matching the original review's own probe exactly
        // (`bs=8`/`64`/`1_048_576` all give identical results) -- this test pins the project's
        // own DEFAULT `block_size` (1 MiB) specifically, since that is the realistic case the
        // original disclosure was about.
        struct OverstatesItsOwnSize {
            real: Vec<u8>,
            claimed_size: u64,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for OverstatesItsOwnSize {
            fn size(&self) -> u64 {
                self.claimed_size
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let source = OverstatesItsOwnSize {
            real: b"hello a".to_vec(), // 7 bytes -- NOT block-aligned at the 1 MiB default
            claimed_size: 100_000,
        };
        let d = Document::new(
            Arc::new(source),
            Config {
                block_size: 1 << 20, // the project's own default
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        // `edge_context`'s own certification is UNCHANGED and stays narrow here -- this is the
        // exact case its own P2-1 fix refuses to certify (a short-but-nonempty first touch, not
        // block-aligned): still true, still correct, and still not by itself enough to resolve
        // this input. What DOES resolve it now is `Document::viewport`'s own `buf_is_true_eof`
        // witness (via `fill_lines`, a separate mechanism from `edge_context` entirely) -- proven
        // below.
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 7).await.unwrap();
        assert!(
            ahead_hit_real_end,
            "batch 12 (2026-07-29), the gap refill: the real end (7) still does not fall on a 1 MiB \
             block boundary, so the first touch past it is short-but-nonempty exactly as before -- \
             but the cache now RE-ASKS the source for block 0's own withheld tail, gets an empty \
             answer, and records it. The boundary is verified rather than merely unresolved, which \
             is what lets the viewport agree with nav instead of conservatively refusing"
        );
        assert!(
            ctx_after.is_empty(),
            "no real bytes exist past \"hello a\"'s own 7 bytes -- and now that is a FACT the \
             source confirmed, not an inference from a short answer"
        );
        let pattern = Arc::new(SearchPattern::compile(r"a(?u:\b)", false).unwrap());
        let outcome = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        // **THE DOCKET, CLOSED -- and closed by RESOLVING the input, not by choosing a side**
        // (batch 12 (2026-07-29)). This test's own history above records the nav/viewport
        // divergence as "Docketed for this restructure's own `CertifiedEnd` type to reconcile",
        // and two attempts at reconciling it by picking a winner both failed: R3 moved the
        // VIEWPORT onto nav's side (reverted as unsound -- the `helloXmo` probe showed the same
        // mechanism fabricates), and batch 11 moved NAV onto the viewport's side (sound, but it
        // paid for the agreement by giving up `$` at genuinely truncated ends).
        //
        // Neither trade is necessary. The disagreement only ever existed because a memoized short
        // block's tail was unreachable, so "is this cut the real end?" had no answer and each
        // consumer guessed. The cache's own completion pass asks the source: here it comes
        // back EMPTY, which certifies the end at 7 outright, and both consumers now agree BECAUSE
        // THEY KNOW -- not because one of them was made to yield.
        assert!(
            matches!(outcome, NavOutcome::FoundMatch { match_at: 6, .. }),
            "`a(?u:\\b)` at 6 is real: the refill certifies that the data ends at 7, so the word \
             boundary after 'a' is a genuine file end rather than an unverified cut; got {outcome:?}"
        );
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["hello a".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![6..7],
            "and the viewport reaches the SAME verdict, which is what closing the docket means: it \
             paints the match it used to refuse, because the end is now certified rather than \
             guessed at. got {:?}",
            v.marks[0].spans
        );
    }
    #[tokio::test]
    async fn edge_context_a_short_but_nonempty_block_does_not_certify_reopening_the_adopted_fixture()
     {
        // fix round (2026-07-27), P2-1 -- credited, adopted verbatim from the fix-round reviewer.
        // A source whose real content is FULLY INTACT (all 15 bytes, `size()` accurate) but whose
        // block 1 (offset 8, `block_size: 8`) legally answers with only 3 of its 8 bytes --
        // `BlockSource`'s own contract (`source.rs`) promises only "up to `len`" bytes, so this is
        // a CONFORMING answer, not a bug in the source -- while real data ("wwww") genuinely
        // continues right where the short answer stopped, within the SAME nominal block. Before
        // this fix round, `take == 0` alone certified `ahead_hit_real_end`, reopening the EXACT
        // fabrication this unit adopted from unit F's own reviewer as its own original RED
        // (`Qzzzz$` satisfied by an artificial edge a short read falsely certified as EOF).
        struct ShortNonEofBlock {
            real: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortNonEofBlock {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        let start = offset as usize;
                        if start >= real.len() {
                            return Ok(bytes::Bytes::new());
                        }
                        if offset == 8 {
                            // legally short under BlockSource's own "up to len" contract -- NOT
                            // the real end: "wwww" genuinely follows, within this SAME nominal
                            // 8-byte block.
                            let short_end = (start + 3).min(real.len());
                            return Ok(bytes::Bytes::copy_from_slice(&real[start..short_end]));
                        }
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"hello Qzzzzwwww"; // 15 bytes, fully intact
        let pattern = Arc::new(SearchPattern::compile("Qzzzz$", false).unwrap());
        assert_eq!(
            pattern.find_all(real),
            Vec::<(usize, usize)>::new(),
            "\"wwww\" genuinely follows; the whole-file oracle finds no match"
        );
        let d = Document::new(
            Arc::new(ShortNonEofBlock {
                real: real.to_vec(),
            }),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 7).await.unwrap();
        assert!(
            !ahead_hit_real_end,
            "a short-but-nonempty block must NOT certify -- got ctx_after {:?}",
            ctx_after
        );
        assert_eq!(
            ctx_after, b"zzzzwwww",
            "batch 12 (2026-07-29), the gap refill: the short answer's own withheld tail is \
             re-asked and delivered, so the whole real block is here. Before the refill this \
             stopped at `zzzz` -- correct but narrow, since `wwww` was real all along and merely \
             unreachable through the memoized short entry"
        );
        let mut full_hay = b"hello Q".to_vec();
        full_hay.extend_from_slice(&ctx_after);
        let hay_reaches_eof = ahead_hit_real_end || 7 + ctx_after.len() as u64 == real.len() as u64;
        let hay = Hay::new(&full_hay, Abs(0))
            .with_high(if hay_reaches_eof {
                Edge::True
            } else {
                Edge::Cut
            })
            .with_body_limit(Local(full_hay.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(7),
            eof_zero_width_ok: false,
        };
        let raw_matches = visible_window_matches(search, 7, b"hello Q", 0, 8, 0, 80);
        assert_eq!(
            raw_matches,
            Vec::<std::ops::Range<usize>>::new(),
            "must not fabricate -- $ satisfied only by a short-but-nonempty block's own \
             uncertified edge, got {:?}",
            raw_matches
        );
    }
    #[tokio::test]
    async fn edge_context_ctx_before_gathers_across_a_block_boundary() {
        // batch 6 (2026-07-28), finding #4: `ctx_before` read exactly ONE block (`idx =
        // (top-1)/bs`, `lo` clamped to `block_start`) -- H's own finding #6 fix (batch 5) made
        // `ctx_after` a multi-block gather but left `ctx_before` untouched. At a block boundary
        // this silently delivers FEWER than CTX_BEHIND (4) bytes. "xxxx\nfoo\n" (9 bytes),
        // block_size 4: block 0 = "xxxx" [0,4), block 1 = "\nfoo" [4,8). top = 5 (one past the
        // real newline at byte 4). OLD: idx=(5-1)/4=1 (block 1 only), lo clamped to
        // block_start=4, delivering just the ONE byte at offset 0 within block 1 ("\n").
        let d = doc(b"xxxx\nfoo\n", 4);
        let (_, ctx_before, _, _) = d.edge_context(5, 3).await.unwrap();
        assert_eq!(
            ctx_before,
            b"xxx\n",
            "must gather the FULL CTX_BEHIND (4) bytes, crossing into block 0, not stop at block \
             1's own start; got {} bytes: {:?}",
            ctx_before.len(),
            ctx_before
        );
    }
    #[tokio::test]
    async fn edge_context_ctx_before_drops_bytes_below_a_conforming_short_blocks_own_gap() {
        // fix round (2026-07-28), P3-1 -- credited, adopted from the K3 review. The gather's own
        // `if block.len() < hi_off { break; }` guard (finding #4's own contiguity check) is
        // REACHABLE: a CONFORMING source (`BlockSource`'s own "up to len" contract, source.rs --
        // the identical premise `ShortNonEofBlock`, above, already relies on) can legally answer
        // FEWER bytes than a block's own nominal width while real data genuinely continues within
        // that SAME nominal block, and a look-behind gather can land astride the resulting gap.
        //
        // 20 real bytes, block_size 8: block 0 = "AAAAAAAA" [0,8) (untouched by this fixture);
        // block 1's own REAL content is "BBBBBBXX" [8,16) (8 bytes) but its FETCH legally
        // returns only the first 6 ("BBBBBB", positions 8-13) -- positions 14-15 ("XX") are
        // real, genuinely continuing data this ONE short answer simply does not reach; block 2 =
        // "DDDD" [16,20) (short only because the file truly ends at 20). `edge_context(17, 3)`:
        // `want_from = 17 - CTX_BEHIND(4) = 13`, landing exactly at the gap's own low edge.
        struct ShortMidFileBlock {
            real: Vec<u8>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortMidFileBlock {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        let start = offset as usize;
                        if start >= real.len() {
                            return Ok(bytes::Bytes::new());
                        }
                        if offset == 8 {
                            // legally short under BlockSource's own "up to len" contract --
                            // positions 14-15 are real and genuinely follow, within this SAME
                            // nominal 8-byte block.
                            let short_end = (start + 6).min(real.len());
                            return Ok(bytes::Bytes::copy_from_slice(&real[start..short_end]));
                        }
                        let end = (start + len).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"AAAAAAAABBBBBBXXDDDD"; // 20 bytes
        let d = Document::new(
            Arc::new(ShortMidFileBlock {
                real: real.to_vec(),
            }),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let (_, ctx_before, _, _) = d.edge_context(17, 3).await.unwrap();
        assert_eq!(
            ctx_before, b"BXXD",
            "batch 12 (2026-07-29), the gap refill: block 1's withheld tail (positions 14-15) is \
             re-asked from the source, so the walk is CONTIGUOUS all the way down and gathers the \
             full `CTX_BEHIND` window. The contiguity guard this test was written for is still \
             live -- it just has no gap left to refuse here. batch 13 (2026-07-29) retired the \
             refill cap, so a source answering short cannot produce an unresolved gap through this \
             cache at all; `hay.rs`'s own `Assembly` unit tests pin the refusal directly at the \
             type level, which is where it is now exercised. got {:?}",
            ctx_before
        );
        // batch 6 (2026-07-28), fix round, P3-1 argued this stayed harmless because a dropped gap
        // always shortened `ctx_before` below `CTX_BEHIND`, so `lookbehind_ok` refused anyway.
        // batch 12 (2026-07-29): the premise is gone in the better direction -- the window is now
        // FULL, because the bytes it wanted are actually readable.
        assert_eq!(
            ctx_before.len(),
            crate::search::CTX_BEHIND.get(),
            "the refill delivers the whole look-behind window, so `lookbehind_ok` is satisfied by \
             real, contiguous bytes rather than side-stepped by an inadequate one"
        );
    }
    #[tokio::test]
    async fn viewport_paints_a_low_edge_verified_by_a_multi_block_ctx_before_gather() {
        // batch 6 (2026-07-28), finding #4, end to end -- the reviewer's own fixture. "xxxx\nfoo\n"
        // (9 bytes), block_size 4, pattern /^foo/: block 0 = "xxxx" [0,4), block 1 = "\nfoo"
        // [4,8), block 2 = "\n" [8,9). The match starts at byte 5 ("foo"'s own line), immediately
        // after the real \n at byte 4. OLD ctx_before (one block, idx=(5-1)/4=1) delivers only
        // that ONE byte ("\n") -- lb=1 < CTX_BEHIND(4) -- so the uniform adequacy rule refuses to
        // trust the low edge even though the ONE delivered byte IS the newline that decides ^foo.
        // nav finds it; the OLD viewport (both paths) painted nothing.
        let d = doc(b"xxxx\nfoo\n", 4);
        let pattern = Arc::new(SearchPattern::compile("^foo", false).unwrap());
        assert_eq!(
            pattern.find_all(b"xxxx\nfoo\n"),
            vec![(5, 8)],
            "ground truth: ^foo matches exactly once, at byte 5"
        );
        let nav = d
            .search_next(0, &pattern, true)
            .await
            .unwrap()
            .join_outcome()
            .await;
        match nav {
            NavOutcome::FoundMatch { match_at, .. } => assert_eq!(match_at, 5),
            other => panic!("expected FoundMatch, got {other:?}"),
        }
        // ordinary path
        let v = d
            .viewport(Anchor(5), 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["foo".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![0..3],
            "the low edge is genuinely verified -- byte 4 (the real \\n) is among the CTX_BEHIND \
             bytes the gather now reaches -- must paint, not drop, the match; nav already agrees"
        );
        // current-match path
        let with_current = d
            .viewport(Anchor(5), 1, 80, HScroll::ZERO, Some((&pattern, Some(5))))
            .await
            .unwrap();
        assert_eq!(
            with_current.marks[0].current,
            Some(0..3),
            "the current-match resolution must agree with the ordinary path"
        );
    }
    #[tokio::test]
    async fn viewport_eof_zero_width_dollar_renders_ordinarily_not_only_when_current() {
        // finding #7: `accept_and_hay_for_row`'s own accept upper bound could never admit buf's
        // own TRUE end as a start (`find_all_starting_in`'s own loop can never examine a start AT
        // its own accept upper bound, exclusive -- no widening of `accept`/`hay` can fix that), so
        // the ordinary per-row pass silently dropped a zero-width `$` sitting exactly there even
        // though nav and the current-match resolution both already found it. `b"a\nb"`: two real,
        // non-phantom zero-width `$` matches -- position 1 (before the real `\n`) and position 3
        // (the true file end, following a real byte 'b', not a phantom).
        let d = doc(b"a\nb", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(b"a\nb"),
            vec![(1, 1), (3, 3)],
            "ground truth: two zero-width matches"
        );
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![1..2],
            "row 0's own real line-end $ is unaffected (rendered one cell wide, `line.rs`'s own \
             zero-width convention -- `viewport_dollar_current_renders_after_the_last_character`'s \
             own `Some(3..4)` is the established precedent)"
        );
        assert_eq!(
            v.marks[1].spans,
            vec![1..2],
            "the true-EOF zero-width $ must render ORDINARILY (relative to row 1's own single \
             byte 'b'), not only when targeted as current"
        );
        // parity: the SAME match, now as the active target, must resolve identically.
        let with_current = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, Some(3))))
            .await
            .unwrap();
        assert_eq!(
            with_current.marks[1].spans,
            vec![1..2],
            "spans must agree regardless of which viewport call resolved current"
        );
        assert_eq!(
            with_current.marks[1].current,
            Some(1..2),
            "current must resolve too -- the same match is REVERSED (spans) and must also be BOLD"
        );
        // behaves under hscroll like every other mark: scrolled one column right, row 1's own
        // single-byte content (and this mark, which sits right after it) is offscreen.
        let scrolled = d
            .viewport(Anchor::TOP, 2, 80, HScroll::new(1), Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            scrolled.marks[1].spans.is_empty(),
            "scrolled past the mark's own column, it must be dropped like any other offscreen \
             mark, got {:?}",
            scrolled.marks[1].spans
        );
    }
    #[tokio::test]
    async fn viewport_stays_unpainted_at_the_trailing_newline_phantom_end_to_end() {
        // finding #7's own phantom fixture, end to end (batch 4, finding #2's rule, applied here):
        // `b"a\n"` -- the file's own true end (position 2) is the trailing newline's own phantom
        // (its real predecessor, position 1, IS `\n`), so `$` must NOT render there, ordinarily or
        // as current, exactly as nav/sweep already refuse it (`search_next_forward_wraps_to_self_
        // when_the_only_other_candidate_is_the_phantom`, this module's own existing test).
        //
        // Fix round (2026-07-27), P2-2/P3-4: renamed from `..._respects_the_trailing_newline_
        // phantom` -- that name claimed to PIN the phantom rule; it does not. Verified by
        // mutation: dropping `!phantom_at_eof` from `eof_zero_width_ok` entirely leaves EVERY
        // assertion in this test passing unchanged. The output stays correct regardless, but for a
        // DIFFERENT reason on each path: the ordinary path never even reaches this position (no
        // row's own bounds cover it, `visible_window_matches`'s own `debug_assert!` states why),
        // and the current-match path's own resolution IS phantom-aware but is separately masked by
        // `render_row`'s own `clip_to_row` (which rejects position 2 for this one-row viewport on
        // ITS OWN row-bounds grounds, independent of the phantom guard). This test therefore pins
        // the END-TO-END OUTCOME (correct either way), not the MECHANISM -- the phantom rule's own
        // discriminating pin is `point_candidate_is_resolved_rejects_the_phantom_even_when_the_
        // belt_would_allow_it`, above, which calls the same production predicate directly.
        let d = doc(b"a\n", 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(b"a\n"),
            vec![(1, 1), (2, 2)],
            "ground truth (the pure oracle IS phantom-blind -- search.rs's own doc comment): the \
             regex itself sees TWO zero-width candidates, one at 1 (before the real \\n) and one \
             at 2 (the raw hay's own end) -- \\n's own phantom status is a DOCTRINE this engine \
             applies ON TOP of find_all, not something find_all itself decides"
        );
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["a".to_string()]);
        assert_eq!(
            v.marks[0].spans,
            vec![1..2],
            "the real, non-phantom $ right after \"a\" (before the real \\n) still renders"
        );
        // current deliberately TARGETS the phantom position (2) -- must not resolve to a match.
        let targeting_phantom = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, Some(2))))
            .await
            .unwrap();
        assert_eq!(
            targeting_phantom.marks[0].current, None,
            "the phantom must not resolve as current either"
        );
        assert_eq!(
            targeting_phantom.marks[0].spans,
            vec![1..2],
            "spans must stay exactly the real match -- no phantom leaking in"
        );
    }
    #[tokio::test]
    async fn search_backward_does_not_splice_a_match_across_an_undelivered_gap() {
        // batch 7 (2026-07-28), finding #2. `SearchBackward` reads DOWNWARD but assembles its hay
        // UPWARD (`ctx ++ slice ++ carry`), so it is the one scan whose two halves can end up
        // non-adjacent: a short read leaves `slice` stopping below `self.hi`, while `carry` still
        // holds bytes from `self.hi` up. Concatenating them spliced a match together across bytes
        // that were never read, and the high edge -- derived from `self.hi + carry.len()`, a
        // position above the last byte actually in hand -- then granted `Edge::True` to it, so a
        // trailing `$` was decided against an edge this hay does not reach.
        //
        // The source is CONFORMING, not broken: all 16 bytes are genuinely present and `size()`
        // is accurate; block 0 simply answers with 5 of its own 8, which `BlockSource`'s own "up
        // to `len`" contract explicitly permits (`source.rs`, and the sibling test below shares
        // this exact fixture shape for the viewport's own version of the hazard).
        struct ShortFirstBlock {
            real: Vec<u8>,
            first_len: usize,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortFirstBlock {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                let first_len = self.first_len;
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    let real = real.clone();
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let want = if offset == 0 { first_len } else { len };
                        let end = (start + want).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"helloXXXWORLD!!!"; // 16 bytes, intact, accurate size()
        // the concatenation the bug performed: "hello" ++ "WORLD!!!" -- a string that exists
        // nowhere in the file, whose own `$` sits at the spliced end.
        let pattern = Arc::new(SearchPattern::compile(r"helloWORLD!!!$", false).unwrap());
        assert_eq!(
            pattern.find_all(real),
            Vec::<(usize, usize)>::new(),
            "ground truth: 'XXX' sits between 'hello' and 'WORLD!!!' -- no match anywhere"
        );
        let d = Document::new(
            Arc::new(ShortFirstBlock {
                real: real.to_vec(),
                first_len: 5,
            }),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let outcome = d
            .search_next(0, &pattern, false)
            .await
            .unwrap()
            .join_outcome()
            .await;
        assert_eq!(
            outcome,
            NavOutcome::Exhausted,
            "reported FoundMatch {{ match_at: 0, wrapped: true }} before this fix -- a match \
             assembled from two non-adjacent regions, confirmed against an edge the hay never \
             reached; got {outcome:?}"
        );
    }
    #[tokio::test]
    async fn viewport_buf_is_true_eof_gate_blocks_a_synthetic_eof_match_at_an_uncertified_cut() {
        // fix round (2026-07-27), P2-FINAL -- credited, adopted verbatim from the closing-round
        // reviewer. `eof_zero_width_ok`'s own `buf_is_true_eof` half IS load-bearing against
        // fabrication, in a case the P3-6 answer's own two-way enumeration ("whenever `ctx_after`
        // IS empty, `buf_hi` is ALREADY verified -- either `buf_is_true_eof` holds, or `ahead_hit_
        // real_end` does") does not list. There is a THIRD way `ctx_after` comes back empty, and
        // it is the one this unit's OWN P2-1 fix created: the AHEAD-side loop's first touch returns
        // `take == 0` from a block that is SHORT but NONEMPTY, so `block.is_empty()` is false and
        // nothing certifies -- while `self.size` is perfectly accurate, so `buf_is_true_eof` is
        // false too.
        //
        // Source: all 15 bytes of `b"helloXmore data"` genuinely present, `size()` accurate (NOT a
        // truncation), but block 0 legally answers with only 5 of its own 8 bytes -- a conforming
        // answer under `BlockSource`'s own "up to `len`" contract (`source.rs`), not a buggy one.
        // `buf` is therefore `b"hello"`, `buf_hi` is 5, and position 5 is a real `'X'`: neither a
        // newline nor the file's end. The whole-file oracle finds `$` ONLY at 15.
        //
        // Without the `buf_is_true_eof &&` gate, `eof_zero_width_ok` widens to `true` here (the
        // phantom half cannot fire -- `buf` does not end in `\n`), `visible_window_matches`'s own
        // synthetic EOF check runs against a `full_hay` whose end is that uncertified cut, and `$`
        // matches it -- a fabricated mark at position 5, exactly the "a slice's own edge is
        // unconditionally `$`-eligible" hazard the whole boundary-context model exists to forbid.
        struct ShortFirstBlock {
            real: Vec<u8>,
            first_len: usize,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortFirstBlock {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                let first_len = self.first_len;
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        // block 0 answers short-but-NONEMPTY while real data genuinely continues
                        // past it.
                        let want = if offset == 0 { first_len } else { len };
                        let end = (start + want).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"helloXmore data"; // 15 bytes, fully intact, accurate size()
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(real),
            vec![(15, 15)],
            "ground truth: the ONLY zero-width $ is at the real file end (15); position 5 is 'X'"
        );
        let d = Document::new(
            Arc::new(ShortFirstBlock {
                real: real.to_vec(),
                first_len: 5,
            }),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        // batch 12 (2026-07-29), the gap refill: the three conditions this test was built around
        // (empty `ctx_after`, no certification, and `buf_hi` at a position that is neither) no
        // longer CO-OCCUR here, because the first one is gone -- block 0's withheld tail is
        // re-asked, so the bytes past `buf` are simply available.
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 5).await.unwrap();
        assert_eq!(
            &ctx_after[..],
            b"Xmore data",
            "the refill closes block 0's own hole, so the look-ahead reaches the rest of the block \
             instead of finding nothing at `lo_off == block.len()`"
        );
        assert!(
            !ahead_hit_real_end,
            "and this is still the right answer for a different reason: block 1 exists and holds \
             real data, so nothing here certifies a file end -- what changed is that the boundary \
             at 5 is no longer a mystery, not that 5 became an end"
        );
        assert_ne!(
            5,
            real.len(),
            "and buf_is_true_eof is false too -- self.size is accurate and buf stops well short of \
             it"
        );
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec!["helloXmore data".to_string()],
            "batch 12 (2026-07-29), the gap refill: block 0's withheld tail is re-asked, so the \
             viewport renders the whole real content instead of stopping at the memoized cut"
        );
        assert_eq!(
            v.marks[0].spans,
            vec![15..16],
            "and `$` is painted at the file's own REAL end (15), which is exactly the point of \
             this fixture: the same mechanism used to fabricate `$` at position 5 -- a real `'X'`, \
             neither a newline nor an end. batch 13 (2026-07-29) retired the refill cap, so there \
             is no longer ANY cut a refill leaves unclosed: the gate this pins is what keeps `$` off \
             a boundary the source has not certified, and every short block reaching a consumer now \
             either ends the data or ends inside the claimed size. got {:?}",
            v.marks[0].spans
        );
    }
    #[tokio::test]
    async fn viewport_short_block_then_empty_next_block_must_not_fabricate() {
        // fix round (2026-07-27), P1-1 -- credited, adopted verbatim (reindented only) from the
        // adversarial review's own probe (`.superpowers/sdd/restr-R3-review-probes.rs`,
        // `REVPROBE_short_block_then_empty_next_block_must_not_fabricate`). This is `ShortFirstBlock`
        // (above)'s own cousin, isolating exactly the shape R3's own first `fill_lines` fix got
        // wrong: block 0 legally answers short -- 5 of its own 8 bytes -- while real data
        // genuinely continues past it, but here the file's real end falls EXACTLY on the next
        // block boundary, so block 1 comes back genuinely empty. R3's own first attempt inferred
        // "the short answer at 5 was a true-EOF signal" from "the next block is empty" -- unsound:
        // that inference cannot distinguish this fixture from `ShortFirstBlock`'s own (where real
        // data DOES sit in the gap), since the block-indexed cache cannot re-ask block 0 for
        // whatever the gap actually holds once it is cached short. Verified against both shas
        // during the review: passes at e9c7e9a (`buf_is_true_eof` was `buf_end == self.size`, i.e.
        // `5 == 8`, false), failed at the frozen 0d28bdc (`got [5..6]`, a synthetic `$` on a real
        // `'X'`) -- this fix round's own re-derivation (`fill_lines`'s own doc comment, P1-1's
        // law: only a genuinely empty read may certify, never a short block's own boundary)
        // restores the e9c7e9a outcome, not by reverting, but by construction.
        struct ShortFirstBlockEndsOnBoundary {
            real: Vec<u8>,
            first_len: usize,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for ShortFirstBlockEndsOnBoundary {
            fn size(&self) -> u64 {
                self.real.len() as u64
            }
            async fn admit(&self) -> crate::source::ReadTicket {
                let real = self.real.clone();
                let first_len = self.first_len;
                crate::source::ReadTicket::from_fn(move |offset, len| {
                    Box::pin(async move {
                        if offset >= real.len() as u64 {
                            return Ok(bytes::Bytes::new());
                        }
                        let start = offset as usize;
                        let want = if offset == 0 { first_len } else { len };
                        let end = (start + want).min(real.len());
                        Ok(bytes::Bytes::copy_from_slice(&real[start..end]))
                    })
                })
            }
        }
        let real: &'static [u8] = b"helloXmo"; // 8 bytes, fully intact, accurate size()
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        assert_eq!(
            pattern.find_all(real),
            vec![(8, 8)],
            "ground truth: the ONLY zero-width $ is at the real file end (8); position 5 is 'X'"
        );
        let d = Document::new(
            Arc::new(ShortFirstBlockEndsOnBoundary {
                real: real.to_vec(),
                first_len: 5,
            }),
            Config {
                block_size: 8,
                prefetch_depth: 0,
                ..Config::default()
            },
        );
        let (_, _, ctx_after, ahead_hit_real_end) = d.edge_context(0, 5).await.unwrap();
        assert_eq!(
            &ctx_after[..],
            b"Xmo",
            "batch 12 (2026-07-29), the gap refill: block 0's withheld tail is re-asked, so the \
             bytes past `buf` are available rather than a hole at `take == 0`"
        );
        assert!(
            !ahead_hit_real_end,
            "short-but-nonempty must not certify (P2-1)"
        );
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.rows,
            vec!["helloXmo".to_string()],
            "batch 12 (2026-07-29), the gap refill: block 0's withheld tail is re-asked, so the \
             viewport renders the whole real content instead of stopping at the memoized cut"
        );
        assert_eq!(
            v.marks[0].spans,
            vec![8..9],
            "and `$` is painted at the file's own REAL end (8), which is exactly the point of \
             this fixture: the same mechanism used to fabricate `$` at position 5 -- a real `'X'`, \
             neither a newline nor an end. batch 13 (2026-07-29) retired the refill cap, so there \
             is no longer ANY cut a refill leaves unclosed: the gate this pins is what keeps `$` off \
             a boundary the source has not certified, and every short block reaching a consumer now \
             either ends the data or ends inside the claimed size. got {:?}",
            v.marks[0].spans
        );
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

    // ---- highlighting cost (batch 4 (2026-07-24), finding #5): before the per-row restructure
    // below, `viewport`'s own match enumeration ran ONCE over the WHOLE fetched buf's own payload
    // span, regardless of how few cells are ever actually visible -- `render_row` then filtered
    // that whole list per row. This measures the shape directly (not a timing oracle: a match
    // COUNT is a structural fact, not an elapsed duration), motivating the restructure below. ----

    #[test]
    fn measure_a_whole_buf_enumeration_returns_a_match_per_byte_on_a_giant_dense_line() {
        // the exact shape `viewport` used to hand off to `render_row`, pre-restructure: a
        // default-sized (1 MiB) single-line buf of dense 'a's, searched with pattern "a" --
        // `find_all_starting_in` over the WHOLE payload span returns roughly one match per byte
        // (~1M `Range<usize>` entries, ~16 MiB), even though a real screen only ever shows on the
        // order of `rows * cols` cells of it. `render_row` used to then filter/clip this WHOLE
        // list once per displayed row (`rows * total_matches` iterations, every repaint).
        let hay = vec![b'a'; 1 << 20];
        let pattern = SearchPattern::compile("a", false).unwrap();
        let matches = pattern.find_all_starting_in(&hay, 0..hay.len());
        assert_eq!(matches.len(), 1 << 20);
    }

    // ---- the accept/hay arithmetic (batch 4 (2026-07-24), finding #5): pure, IO-free, no
    // `Document`/tokio runtime needed -- the boundedness claim itself lives here, checked with
    // exact expected ranges, independent of the equivalence suite's own correctness claim. ----

    #[test]
    fn accept_and_hay_ordinary_interior_row_does_not_touch_bufs_own_edges() {
        // a row comfortably inside a large buf, far from either edge: `accept`'s own leftward
        // MAX_MATCH_LEN expansion and `hay`'s further CTX_BEHIND/(MAX_MATCH_LEN+CTX_AHEAD-1)
        // margins land on their own arithmetic, untouched by either clamp.
        //
        // batch 5 (2026-07-26), finding #6: `hay`'s own expected value MOVED here (14_102 ->
        // 14_105) -- the AHEAD-side margin widened from a bare `MAX_MATCH_LEN` to `MAX_MATCH_LEN
        // + CTX_AHEAD - 1` (this function's own doc comment has the derivation); `accept` is
        // untouched (its own formula did not change).
        let (accept, hay) = accept_and_hay_for_row(0..5, 10_000, 100_000, 0, 100_000);
        assert_eq!(
            accept,
            Local(5_904)..Local(10_006),
            "10_000 - MAX_MATCH_LEN .. 10_005 + 1"
        );
        assert_eq!(
            hay,
            Local(5_900)..Local(14_105),
            "accept -CTX_BEHIND .. accept +MAX_MATCH_LEN+CTX_AHEAD-1"
        );
    }
    #[test]
    fn accept_clamps_to_bufs_own_top_edge_while_hay_reaches_into_ctx_before() {
        // a row 2 bytes into buf, with 4 bytes of real ctx_before already fetched (`lb == 4`):
        // accept's own leftward expansion clamps at buf's own start (position 4 in `full_hay`'s
        // own coordinates) -- deliberately NOT reaching into ctx_before as an acceptable match
        // START -- but hay's own further CTX_BEHIND margin does reach all the way to `full_hay`'s
        // own start (position 0), the edge-context bytes unit B's own `edge_context` fetched.
        //
        // batch 5 (2026-07-26), finding #6: `hay`'s own upper bound MOVED here (4_108 -> 4_111)
        // for the identical AHEAD-side reason as the interior-row test above; the leftward side
        // (this test's own point) is untouched.
        let (accept, hay) = accept_and_hay_for_row(0..5, 2, 100_000, 4, 100_004);
        assert_eq!(
            accept,
            Local(4)..Local(12),
            "clamped to buf's own start (lb=4), not full_hay's (0)"
        );
        assert_eq!(
            hay,
            Local(0)..Local(4_111),
            "reaches full_hay's own start -- the real ctx_before bytes"
        );
    }
    #[test]
    fn accept_clamps_to_bufs_own_bottom_edge_while_hay_reaches_into_ctx_after() {
        // the symmetric case: a row ending exactly at buf's own end (`buf_len == 100_000`), with
        // 4 bytes of real ctx_after already fetched (`full_hay_len == 100_004`) -- accept clamps
        // at buf's own end, hay reaches past it into the real ctx_after bytes.
        //
        // batch 5 (2026-07-26), finding #6: `hay`'s own expected value is UNAFFECTED here (stays
        // 95_895..100_004) -- unlike the two tests above, this fixture's own `full_hay_len`
        // (100_004) is already narrower than either the OLD (`+MAX_MATCH_LEN`) or NEW
        // (`+MAX_MATCH_LEN+CTX_AHEAD-1`) unclamped reach, so both formulas clamp to the identical
        // value; this fixture cannot discriminate the widening (a realistic `full_hay_len` this
        // narrow only arises from a genuinely short/truncated real file, exercised instead by the
        // dedicated resolved-criterion-belt test below).
        let (accept, hay) = accept_and_hay_for_row(5..10, 99_990, 100_000, 0, 100_004);
        assert_eq!(
            accept,
            Local(95_899)..Local(100_000),
            "clamped to buf's own end, not full_hay's (100_004)"
        );
        assert_eq!(
            hay,
            Local(95_895)..Local(100_004),
            "reaches full_hay's own end -- the real ctx_after bytes"
        );
    }
    #[test]
    fn accept_admits_one_past_a_short_rows_own_visible_content_end() {
        // "abc" (win 0..3) fully visible in a wide window: accept's own upper bound is
        // `win_end + 1` (54, not 53) -- admitting a zero-width match (an EOF `$`) sitting exactly
        // AT the row's own content end, which is still a legitimate, within-window position (see
        // `line.rs`'s own finding #7(a)). Over-admitting by exactly one position here is always
        // safe: anything genuinely unrenderable that this lets through is dropped downstream by
        // `layout_row_with_marks`'s own `token_at(..) -> None` fallback regardless (the identical
        // argument finding #7(b)'s own audit made for why a start with nothing laid out for it
        // can never render).
        let (accept, _) = accept_and_hay_for_row(0..3, 50, 1_000, 0, 1_000);
        assert_eq!(accept, Local(0)..Local(54), "50 + 3 + 1, not 50 + 3");
    }
    #[test]
    fn accept_and_hay_stay_tiny_against_a_giant_buf() {
        // the giant-line shape directly (batch 4 (2026-07-24), finding #5's own headline claim):
        // an 80-column window on a 1 MiB buf -- `accept`/`hay` stay on the order of `cols` and
        // `MAX_MATCH_LEN`, nowhere near buf's own size.
        //
        // batch 5 (2026-07-26), finding #6: `hay`'s own expected value MOVED here (4_177 ->
        // 4_180) for the identical AHEAD-side reason as the interior-row test above; the `< 4_200`
        // bound still holds comfortably.
        let (accept, hay) = accept_and_hay_for_row(0..80, 0, 1 << 20, 0, 1 << 20);
        assert_eq!(accept, Local(0)..Local(81));
        assert_eq!(
            hay,
            Local(0)..Local(4_180),
            "81 + MAX_MATCH_LEN(4096) + CTX_AHEAD(4) - 1"
        );
        assert!(accept.end.0 - accept.start.0 < 100);
        assert!(hay.end.0 - hay.start.0 < 4_200);
    }

    // ---- the referee tests (batch 4 (2026-07-24), finding #5): the per-row restructure must be
    // a pure cost win, never a correctness change -- every fixture below runs `top == Anchor::TOP`
    // with `buf` reaching the true EOF, so `lb == 0` and `hay == buf` exactly, letting
    // `reference_all_matches` be called directly against the fixture's own raw data without
    // reproducing `edge_context`'s own fetch (already covered independently by unit B's own
    // finding #6 tests, which stay green above, unaffected by this restructure). ----

    #[tokio::test]
    async fn per_row_matches_agree_with_the_frozen_whole_buf_reference_on_ordinary_fixtures() {
        struct Case {
            name: &'static str,
            data: Vec<u8>,
            pattern: &'static str,
            rows: usize,
            cols: usize,
            hscroll: usize,
        }
        // fix round (2026-07-25), F5: two fixtures long enough to exercise `line_start`/`hscroll`
        // PAST `MAX_MATCH_LEN` (4096) -- the review's own audit found every original fixture was
        // small enough that `accept.start` always clamped to 0, so the leftward expansion (and
        // the hay SLICE it creates) was never exercised as an arithmetic quantity, only as "did
        // the multiline match survive". A non-self-overlapping pattern ("needle") is used
        // deliberately: a SELF-overlapping pattern's own non-overlapping tiling genuinely does
        // shift phase with where `accept.start` begins past this same threshold -- an accepted,
        // documented residual (see the dedicated pinned test below), not asserted here.
        let mut line_start_past_cap = vec![b'x'; 5000];
        line_start_past_cap.push(b'\n');
        line_start_past_cap.extend_from_slice(b"needle\n");
        let mut hscroll_past_cap = vec![b'x'; 4200];
        hscroll_past_cap.extend_from_slice(b"needle");
        hscroll_past_cap.extend(std::iter::repeat_n(b'x', 100));
        hscroll_past_cap.push(b'\n');
        let cases = [
            Case {
                name: "short_rows_no_match",
                data: b"foo\nbar\nbaz\n".to_vec(),
                pattern: "q",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "tabs",
                data: b"a\tfoo\tb\n".to_vec(),
                pattern: "foo",
                rows: 1,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "control_chars",
                data: b"a\x1bfoo\x07b\n".to_vec(),
                pattern: "foo",
                rows: 1,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "hscroll_mid_line",
                data: b"0123456789foo9876543210\n".to_vec(),
                pattern: "foo",
                rows: 1,
                cols: 5,
                hscroll: 8,
            },
            Case {
                name: "zero_width_caret_two_rows",
                data: b"foo\nbar\n".to_vec(),
                pattern: "^",
                rows: 2,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "zero_width_dollar_narrow_window",
                data: b"foo\n".to_vec(),
                pattern: "$",
                rows: 1,
                cols: 3,
                hscroll: 0,
            },
            Case {
                name: "zero_width_dollar_wide_window",
                data: b"foo\n".to_vec(),
                pattern: "$",
                rows: 1,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "match_straddling_the_right_window_edge",
                data: b"xxxfoo\n".to_vec(),
                pattern: "foo",
                rows: 1,
                cols: 5,
                hscroll: 0,
            },
            Case {
                name: "match_straddling_the_left_window_edge",
                data: b"xxxfoo\n".to_vec(),
                pattern: "xfoo",
                rows: 1,
                cols: 3,
                hscroll: 3,
            },
            Case {
                name: "multiline_match_two_rows",
                data: b"xxfoo\nbarzz\n".to_vec(),
                pattern: "foo\nbar",
                rows: 2,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "multiline_match_three_rows",
                data: b"xxfoo\nbar\nbazyy\n".to_vec(),
                pattern: "foo\nbar\nbaz",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "wide_chars",
                data: "wide世界chars\n".as_bytes().to_vec(),
                pattern: "世",
                rows: 1,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "stripped_carriage_return",
                data: b"foo\r\n".to_vec(),
                pattern: r"foo\r$",
                rows: 1,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "dense_matches_within_a_narrow_window",
                data: b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".to_vec(),
                pattern: "a",
                rows: 1,
                cols: 5,
                hscroll: 0,
            },
            // F1: blank lines -- LF and CRLF, all three assertion shapes.
            Case {
                name: "blank_line_caret",
                data: b"foo\n\nbar\n".to_vec(),
                pattern: "^",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "blank_line_dollar",
                data: b"foo\n\nbar\n".to_vec(),
                pattern: "$",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "blank_line_caret_dollar",
                data: b"foo\n\nbar\n".to_vec(),
                pattern: "^$",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "blank_line_crlf_caret",
                data: b"foo\r\n\r\nbar\r\n".to_vec(),
                pattern: "^",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            // fix round 2 (2026-07-25), R1: the missing cell of the blank x CRLF x `$` cross
            // product -- a bare `$` on a blank CRLF row (its own raw content is just the stripped
            // '\r') needs the SAME widening as any other CRLF row, which the blank branch didn't
            // apply before R1.
            Case {
                name: "blank_line_crlf_dollar",
                data: b"foo\r\n\r\nbar\r\n".to_vec(),
                pattern: "$",
                rows: 3,
                cols: 80,
                hscroll: 0,
            },
            // F4: a bare `$` on a CRLF line (the existing "stripped_carriage_return" case above
            // uses pattern `foo\r$`, whose start is byte 0 -- it never exercises the "+1" slack
            // this fixture is specifically about).
            Case {
                name: "bare_dollar_crlf",
                data: b"foo\r\nbar\r\n".to_vec(),
                pattern: "$",
                rows: 2,
                cols: 80,
                hscroll: 0,
            },
            // F5: line_start / hscroll past MAX_MATCH_LEN, ordinary (non-self-overlapping)
            // pattern -- must still agree with the reference.
            Case {
                name: "line_start_past_max_match_len",
                data: line_start_past_cap,
                pattern: "needle",
                rows: 2,
                cols: 80,
                hscroll: 0,
            },
            Case {
                name: "hscroll_past_max_match_len",
                data: hscroll_past_cap,
                pattern: "needle",
                rows: 1,
                cols: 20,
                hscroll: 4200,
            },
        ];
        // F5 (strengthened, fix round 2 (2026-07-25), per the review's own nit): a standing guard
        // against the per-case loop below silently asserting nothing (no `\n` in a fixture, or a
        // `viewport` call returning fewer rows than expected would both pass vacuously without
        // this) -- matches this project's own general policy of never trusting a loop-shaped test
        // to have actually compared anything. PER-CASE, not a single total across every fixture:
        // a global counter would let one silently-empty fixture hide behind any other fixture's
        // own real comparisons; per-case is strictly stronger, catching that one fixture directly.
        for c in &cases {
            let d = doc_owned(c.data.clone(), 1 << 20);
            let pattern = SearchPattern::compile(c.pattern, false).unwrap();
            let v = d
                .viewport(
                    Anchor::TOP,
                    c.rows,
                    c.cols,
                    HScroll::new(c.hscroll),
                    Some((&pattern, None)),
                )
                .await
                .unwrap();
            let reference = reference_all_matches(&pattern, &c.data, 0, c.data.len());
            let mut line_start = 0usize;
            let mut row = 0usize;
            let mut rows_compared = 0usize;
            for i in 0..c.data.len() {
                if c.data[i] != b'\n' {
                    continue;
                }
                if row >= v.marks.len() {
                    break;
                }
                let line = line_start..i;
                let slice = &c.data[line.clone()];
                let byte_marks: Vec<std::ops::Range<usize>> = reference
                    .iter()
                    .filter_map(|m| clip_to_row(m, &line))
                    .collect();
                let expected =
                    crate::line::layout_row_with_marks(slice, 8, c.hscroll, c.cols, &byte_marks).1;
                assert_eq!(
                    v.marks[row].spans, expected,
                    "case {:?} row {row}: per-row spans diverge from the frozen whole-buf \
                     reference",
                    c.name
                );
                line_start = i + 1;
                row += 1;
                rows_compared += 1;
            }
            assert!(
                rows_compared > 0,
                "case {:?}: vacuity guard -- this fixture's own loop never ran a single \
                 row-comparison",
                c.name
            );
        }
    }
    #[tokio::test]
    async fn multiline_match_starting_offscreen_right_on_one_row_is_still_found_via_the_next_rows_own_leftward_reach()
     {
        // the REQUIRED adversarial fixture (batch 4 (2026-07-24), finding #5's own multiline
        // coverage argument): row 0 is deliberately long enough that "foo" (the match's own head)
        // sits PAST row 0's own narrow window (cols=5) -- row 0's own enumeration must exclude it
        // (its start is offscreen-right, per the brief's own "starts beyond win_end paint nothing
        // of THIS window" rule) -- while row 1's own LEFTWARD MAX_MATCH_LEN expansion, crossing
        // the `\n` freely (a flat byte range, not row-scoped), still reaches back far enough to
        // find the SAME match and correctly paint its own head ("bar").
        let mut data = vec![b'x'; 20];
        data.extend_from_slice(b"foo\nbarzz\n");
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foo\nbar", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 2, 5, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert!(
            v.marks[0].spans.is_empty(),
            "row 0's own window (cols=5) never reaches byte 20 (\"foo\"'s own start) -- its \
             enumeration must correctly exclude a match starting offscreen-right, got {:?}",
            v.marks[0].spans
        );
        assert_eq!(
            v.marks[1].spans,
            vec![0..3],
            "row 1's own leftward MAX_MATCH_LEN reach must still find the match crossing the \\n \
             from row 0, painting \"bar\" at its own head"
        );
    }
    #[tokio::test]
    async fn self_overlapping_pattern_tiling_phase_can_shift_with_hscroll_past_max_match_len_accepted_residual()
     {
        // fix round (2026-07-25), F5 -- RULED (accepted, documented, not closed):
        // `find_all_starting_in` walks from `accept.start`, so the PHASE of its non-overlapping
        // tiling for a self-overlapping pattern (e.g. "aa" over a run of 'a's) depends on where a
        // row's own `accept` begins -- a quantity now shaped by `hscroll`/`line_start` once past
        // `MAX_MATCH_LEN`, where the OLD whole-buf computation always tiled from a FIXED buf
        // start (`accept.start == 0`, always). Pinning the walk's own start to a canonical phase
        // (e.g. always even, or always buf-start-relative) would need reintroducing the unbounded
        // whole-line walk finding #5 removed to compute it -- so this sits inside the ALREADY
        // documented non-overlapping-enumeration ambiguity (docs/search.md's own "how many times
        // does this pattern occur" section) rather than being closed. Pinned here as the live,
        // accepted behavior -- deliberately NOT asserted equal to the frozen reference, which
        // this specific fixture is EXPECTED to diverge from once `hscroll`/`MAX_MATCH_LEN`'s own
        // parity differs from the reference's fixed `0`-anchored tiling.
        let mut data = vec![b'a'; 5000];
        data.push(b'b');
        data.push(b'\n');
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("aa", false).unwrap());
        let reference = reference_all_matches(&pattern, data, 0, data.len());
        let line = 0..5000;
        let ref_spans_at = |hscroll: usize, cols: usize| {
            let byte_marks: Vec<std::ops::Range<usize>> = reference
                .iter()
                .filter_map(|m| clip_to_row(m, &line))
                .collect();
            crate::line::layout_row_with_marks(&data[line.clone()], 8, hscroll, cols, &byte_marks).1
        };
        // hscroll = 4995: `accept.start = (4995 - MAX_MATCH_LEN).max(0) = 899` (odd) -- the
        // live, row-anchored tiling disagrees with the reference's own buf-start-anchored one.
        let v_4995 = d
            .viewport(
                Anchor::TOP,
                1,
                10,
                HScroll::new(4995),
                Some((&pattern, None)),
            )
            .await
            .unwrap();
        let ref_4995 = ref_spans_at(4995, 10);
        assert_eq!(
            v_4995.marks[0].spans,
            vec![0..4],
            "pinning the live, hscroll-shifted tiling phase at 4995"
        );
        assert_ne!(
            v_4995.marks[0].spans, ref_4995,
            "this fixture is EXPECTED to diverge from the frozen reference at hscroll=4995 -- \
             the accepted residual itself, not a bug; got reference {ref_4995:?}"
        );
        // hscroll = 4996: `accept.start = 900` (even) -- phases agree again, by parity, not by
        // design; pinned as a sibling data point, not a general guarantee.
        let v_4996 = d
            .viewport(
                Anchor::TOP,
                1,
                10,
                HScroll::new(4996),
                Some((&pattern, None)),
            )
            .await
            .unwrap();
        assert_eq!(
            v_4996.marks[0].spans,
            ref_spans_at(4996, 10),
            "at hscroll=4996 the two phases happen to agree -- a coincidence of parity, not \
             evidence the residual is closed"
        );
    }
    #[test]
    fn visible_window_matches_over_cap_assertion_free_matches_paint_again_the_r5_recovery() {
        // **Restructure R5 (2026-07-28) -- expectation MOVED, direction RECOVERY.** Renamed from
        // `..._over_cap_matches_paint_nothing_a_position_dependent_documented_miss` (batch 4's own
        // renaming convention: a name states current behavior, not a stale claim). Fix round
        // (2026-07-27), P1-1's own CONTROLLER RULING (accept + disclose) landed R4's belt as a
        // UNIFORM, non-content-aware margin check: an over-cap match (one whose own body extends
        // `>= MAX_MATCH_LEN` bytes past the row's accept end) could never resolve without a real
        // EOF exemption, REGARDLESS of whether its own pattern carried a trailing assertion that
        // needed the margin at all -- `d22c106` painted the whole visible row for an ordinary,
        // assertion-free `a+`; R4 painted nothing. R5 is the planned refinement P1-1's own report
        // named explicitly ("assertion-aware acceptance... docketed for the accept-model
        // restructure"): `Hay::verified`'s own `!pat.trailing_assertions() || lookahead_ok(e)`
        // guard waives the margin entirely for a pattern with no trailing assertion, so `a+` paints
        // again -- direction 1, below, moves from an empty result to a genuine (oracle-agreeing,
        // row-window-bounded) span; direction 2 was ALREADY earning the belt's own EOF exemption
        // under R4 and is unaffected.
        //
        // Position-dependence itself does not go away -- it moves from "painted or not" to "how
        // far the painted span reaches": `accept_and_hay_for_row`'s own windowing widens a row's
        // hay by `consumable_reach()` (`MAX_MATCH_LEN + CTX_AHEAD - 1`) past `accept.end`, clamped
        // to `full_hay.len()` -- for a row NOT close enough to the fetched hay's own true end, that
        // widened window stops SHORT of it (`hay.end = 9_980 < full_hay.len() = 10_000` here), so
        // the row-hay's OWN `body_limit` -- not the file's real end -- is what caps the reported
        // span now, exactly the way `body_limit` already capped every OTHER (non-recovered)
        // candidate before this restructure. A 10_000-byte dense-'a' line, pattern `a+` (an
        // ordinary, assertion-free greedy pattern):
        let data: &'static [u8] = Box::leak(vec![b'a'; 10_000].into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile("a+", false).unwrap());
        assert_eq!(
            pattern.find_all(data),
            vec![(0, 10_000)],
            "ground truth: one match, the whole line"
        );
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: false,
        };
        // direction 1: far enough from the fetched hay's own end that this row's own hay never
        // reaches it (unclamped `hay.end = 9_980 < full_hay.len() = 10_000`) -- pre-R5, the
        // over-cap candidate's own margin was always 0 with no EOF exemption, so it was dropped.
        // R5: `a+` carries no trailing assertion, so the margin is waived outright -- the greedy
        // match is found and reported up to THIS row-hay's own `body_limit` (9_980), not the
        // file's real end (which this row's own window never reached).
        let recovers = visible_window_matches(search, data.len(), data, 5_800, 8, 0, 80);
        assert_eq!(
            recovers,
            vec![1_704..9_980],
            "assertion-free over-cap match -- the margin is waived, painting resumes up to this \
             row-hay's own body_limit; got {:?}",
            recovers
        );
        // direction 2: close enough that this row's own hay IS clamped exactly to `full_hay.len()`
        // (`hay.end = 10_000`) -- the identical over-cap candidate now earns the belt's own EOF
        // exemption and paints its own genuine (real, oracle-agreeing) tail.
        let painted = visible_window_matches(search, data.len(), data, 5_900, 8, 0, 80);
        assert_eq!(
            painted,
            vec![1_804..10_000],
            "close enough to the fetched hay's own edge that the belt's EOF exemption applies -- \
             the genuine tail must paint; got {:?}",
            painted
        );
    }
    #[tokio::test]
    async fn giant_line_viewport_finds_correct_bounded_marks() {
        // the REQUIRED giant-line end-to-end test (batch 4 (2026-07-24), finding #5): a 1 MiB
        // single-line dense-'a' buf (the exact shape `measure_a_whole_buf_enumeration_returns_a_
        // match_per_byte_on_a_giant_dense_line`, above, characterizes as ~1M matches pre-
        // restructure), viewed through an 80-column window. Correct visible marks (the exact
        // span), AND the per-row mark list stays small -- what the OLD code structurally could
        // not do (it would return ~1M byte_marks pre-clip, regardless of the window).
        let data: &'static [u8] = Box::leak(vec![b'a'; 1 << 20].into_boxed_slice());
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("a", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 1, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.marks[0].spans,
            vec![0..80],
            "every visible cell is part of one dense, merged match"
        );
        // the boundedness claim directly: `visible_window_matches` itself (not just the merged
        // `spans` output, which would collapse a huge list into one span regardless and so
        // cannot distinguish a bounded enumeration from an unbounded one -- see this test
        // module's own `measure_a_whole_buf_enumeration_...` sibling for why `spans.len()` alone
        // is the wrong place to look).
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: true,
        };
        let raw_matches = visible_window_matches(search, data.len(), data, 0, 8, 0, 80);
        assert!(
            raw_matches.len() < 200,
            "expected a small, window-bounded mark list (cols=80 plus a small margin), got {}",
            raw_matches.len()
        );
    }
    #[test]
    fn giant_line_returned_matches_stay_window_bounded_away_from_offset_zero() {
        // fix round F3 [P2]: the test above only ever calls `visible_window_matches` with
        // `line_start = 0, hscroll = 0`, where the leftward `MAX_MATCH_LEN` expansion clamps
        // away against buf's own start -- it passed by ACCIDENT of that clamp, not because the
        // general bound held. Re-derived directly from the arithmetic: non-overlapping matches
        // can't all reach the SAME point, so at most ONE result can straddle in from the
        // leftward `MAX_MATCH_LEN` zone; every other returned match starts within `[win_start_
        // abs, accept.end)`, a `win_bytes`-wide range (fix round 2 (2026-07-25), R2: BYTES, not
        // `cols` -- the two only coincide for single-byte content, this fixture's own dense
        // ASCII `'a'`s) -- `win_bytes + 2` is the derived bound (the `+1` accept admission, plus
        // the one possible straddler), regardless of where in a giant buf the row sits.
        let data: &'static [u8] = Box::leak(vec![b'a'; 1 << 20].into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile("a", false).unwrap());
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: true,
        };
        for (line_start, hscroll, label) in [
            (100_000usize, 0usize, "line_start past MAX_MATCH_LEN"),
            (0usize, 50_000usize, "hscroll past MAX_MATCH_LEN"),
        ] {
            let raw_matches =
                visible_window_matches(search, data.len(), data, line_start, 8, hscroll, 80);
            assert!(
                raw_matches.len() <= 82,
                "{label}: expected <= win_bytes + 2 = cols + 2 (82) for single-byte content, \
                 got {}",
                raw_matches.len()
            );
        }
    }
    #[test]
    fn giant_line_multibyte_bound_is_win_bytes_plus_2_not_cols_plus_2() {
        // fix round 2 (2026-07-25), R2 [P3]: the `cols + 2` bound above is specific to
        // single-byte content -- for multi-byte text the honest bound is `win_bytes + 2`, where
        // `win_bytes` (`win.end - win.start`, `line::visible_byte_range`'s own return value) can
        // run up to `4 * cols` for ordinary (non-zero-width) text. Pinned directly: 4000 `世`
        // (3 bytes each, width 2) at `cols = 80, hscroll = 3000` -- the window covers exactly 40
        // characters (`cols / 2`), `win_bytes = 40 * 3 = 120`, so the derived bound is `122`, not
        // `82`. Pattern `.` (matches any raw byte except `\n` -- `unicode(false)`, search.rs's
        // own doc comment) makes every byte of every visible character its own 1-byte match, the
        // densest case, so the returned count hits the bound exactly rather than merely staying
        // under it.
        let data: &'static [u8] = Box::leak("世".repeat(4000).into_bytes().into_boxed_slice());
        let pattern = Arc::new(SearchPattern::compile(".", false).unwrap());
        let hay = Hay::new(data, Abs(0))
            .with_high(Edge::True)
            .with_body_limit(Local(data.len()));
        let search = RowSearch {
            pattern: &pattern,
            parent: &hay,
            lb: 0,
            buf_hi: Local(data.len()),
            eof_zero_width_ok: true,
        };
        let raw_matches = visible_window_matches(search, data.len(), data, 0, 8, 3000, 80);
        assert_eq!(
            raw_matches.len(),
            122,
            "win_bytes (120, 40 characters x 3 bytes) + 2, not cols + 2 (82)"
        );
    }
    #[tokio::test]
    async fn same_start_alternation_residual_does_not_reach_ordinary_within_cap_matches() {
        // batch 4 (2026-07-24): `find_all_starting_in`'s own documented residual (search.rs's own
        // doc comment) -- a same-START, SHORTER alternative can be lost when a longer, preferred
        // alternative at that position overruns `accept.end` -- is INHERITED by this restructure
        // (both old and new call the identical method), not introduced by it. Verifying that
        // directly: `visible_window_matches`'s own search uses `hay.end` (widened a full
        // `MAX_MATCH_LEN` past the row's own narrow `accept.end`), not the narrow bound itself,
        // as `find_all_starting_in`'s own accept -- so an alternative WITHIN the documented cap
        // never overruns it, matching the old whole-buf computation's own verdict exactly. A
        // "foobar|foo" pattern, with "foobar" entirely within a row's own reach, must resolve
        // identically both ways: the FIRST (preferred, leftmost-first) alternative, found whole.
        let data =
            b"xxxxxfoobarxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("foobar|foo", false).unwrap());
        // cols=8 makes row 0's own window end exactly at "foo"'s own end (byte 8), narrower than
        // "foobar"'s own full extent (byte 11) -- exactly the shape that would overrun a NARROW
        // per-row accept, if the search itself used it instead of `hay.end`.
        let v = d
            .viewport(Anchor::TOP, 1, 8, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        let reference = reference_all_matches(&pattern, data, 0, data.len());
        assert_eq!(
            reference,
            vec![5..11],
            "sanity: the frozen whole-buf reference finds \"foobar\" whole, no overrun (buf is \
             comfortably larger than 11 bytes)"
        );
        assert_eq!(
            v.marks[0].spans,
            vec![5..8],
            "\"foobar\" (5..11) clipped to the 8-column window's own visible portion (5..8) -- \
             identical to what clipping the frozen reference's own match would produce, proving \
             the residual did not reach this within-cap fixture"
        );
    }

    // ---- fix round 1 (2026-07-25), review at .superpowers/sdd/batch4-unit-C-review.md ----

    #[tokio::test]
    async fn blank_line_zero_width_matches_still_render_not_only_when_current() {
        // F1 [P1]: a blank row's own `visible_byte_range` is `0..0` -- the SAME value "hscroll
        // past all content" and "cols == 0" return -- but a blank line still has exactly one
        // honest cursor position (byte 0), which `layout_row_with_marks`'s own `col` fallback
        // already renders a zero-width match onto
        // (`zero_width_mark_on_empty_line_renders_at_the_first_cell`, line.rs). The early return
        // in `visible_window_matches` used to drop this row's own match entirely, for `^`, `$`,
        // AND `^$` alike, on both LF and CRLF blank lines.
        for (data, pattern) in [
            (&b"foo\n\nbar\n"[..], "^"),
            (&b"foo\n\nbar\n"[..], "$"),
            (&b"foo\n\nbar\n"[..], "^$"),
            (&b"foo\r\n\r\nbar\r\n"[..], "^"),
            // fix round 2 (2026-07-25), R1: the one cell of the blank x CRLF x `$` cross product
            // the original fix round's fixtures missed -- a bare `$` on a blank CRLF row (its own
            // raw content is just the stripped '\r') needs the SAME widening F4 gave non-blank
            // CRLF rows, which the blank branch didn't yet apply.
            (&b"foo\r\n\r\nbar\r\n"[..], "$"),
        ] {
            let d = doc(data, 1 << 20);
            let p = Arc::new(SearchPattern::compile(pattern, false).unwrap());
            let v = d
                .viewport(Anchor::TOP, 3, 80, HScroll::ZERO, Some((&p, None)))
                .await
                .unwrap();
            assert_eq!(
                v.marks[1].spans,
                vec![0..1],
                "pattern {pattern:?} over {data:?}: blank row (row 1) must still show its own \
                 zero-width mark"
            );
        }
    }
    #[tokio::test]
    async fn blank_line_zero_width_match_is_consistent_between_plain_and_current() {
        // F1: before the fix, the SAME match reappeared only once it became `current` (resolved
        // independently of `visible_window_matches`) -- `n` landing on a blank line's own `^`
        // would summon a mark that was never in `spans` a moment earlier, violating `RowMarks`'s
        // own contract that `current` is always contained in `spans`.
        let data = b"foo\n\nbar\n";
        let pattern = Arc::new(SearchPattern::compile("^", false).unwrap());
        let d = doc(data, 1 << 20);
        let plain = d
            .viewport(Anchor::TOP, 3, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        let with_current = d
            .viewport(Anchor::TOP, 3, 80, HScroll::ZERO, Some((&pattern, Some(4))))
            .await
            .unwrap();
        assert_eq!(
            plain.marks[1].spans, with_current.marks[1].spans,
            "the blank row's own mark must not depend on whether it happens to be current"
        );
        assert_eq!(with_current.marks[1].current, Some(0..1));
    }
    #[tokio::test]
    async fn bare_dollar_on_a_crlf_line_still_highlights() {
        // F4 [P2]: `visible_byte_range` strips the trailing '\r' (correctly -- it mirrors
        // `layout_row_with_marks`'s own `body`), so a CRLF row's own accept upper bound
        // (`win_end + 1`) used to fall exactly one byte short of the `$` position (right after
        // the \r, before the real \n) -- the "+1" slack meant for an end-of-line zero-width
        // match was consumed by the stripped byte itself.
        let data = b"foo\r\nbar\r\n";
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile("$", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(
            v.marks[0].spans,
            vec![3..4],
            "row 0's own $ (after \"foo\\r\") must still render"
        );
        assert_eq!(
            v.marks[1].spans,
            vec![3..4],
            "row 1's own $ (after \"bar\\r\") must still render"
        );
    }
    #[tokio::test]
    async fn a_match_completing_in_ctx_after_is_both_reversed_and_bold_when_current() {
        // F2 [P1] -- RULED: keep the new behavior (a match starting in `buf` and completing in
        // real, already-fetched `ctx_after` bytes is genuine, and highlighting it improves
        // nav/highlighter agreement). "foo\nbar\nbazqux\n", rows=2 (`buf` stops right after
        // "bar\n"; "bazqux\n" is real file content past it, so `edge_context` fetches its own
        // leading bytes as `ctx_after`), pattern `r\nb` -- the 'b' of "bazqux" is byte 8,
        // one past `buf`'s own end (byte 8), living only in `ctx_after`.
        let data = b"foo\nbar\nbazqux\n";
        let d = doc(data, 1 << 20);
        let pattern = Arc::new(SearchPattern::compile(r"r\nb", false).unwrap());
        let v = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, None)))
            .await
            .unwrap();
        assert_eq!(v.rows, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(
            v.marks[1].spans,
            vec![2..3],
            "the match's own visible prefix (the 'r' of \"bar\") highlights even though its own \
             tail extends past buf's own end"
        );
        // the SAME match, now as the active target (absolute start = byte 6, the 'r').
        let with_current = d
            .viewport(Anchor::TOP, 2, 80, HScroll::ZERO, Some((&pattern, Some(6))))
            .await
            .unwrap();
        assert_eq!(
            with_current.marks[1].spans,
            vec![2..3],
            "spans must agree regardless of which viewport call resolved current"
        );
        assert_eq!(
            with_current.marks[1].current,
            Some(2..3),
            "current must resolve too -- the same match is REVERSED (spans) and must also be \
             BOLD (current), not just one or the other"
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
            // restructure R3 (the H's-pin-flip fix, `fill_lines`'s own doc comment): when the
            // scan reaches a real end short of `budget` (a small enough `len`), the final block
            // is short -- `fill_lines` now verifies that with ONE separate, discarded probe at
            // the next block's own start before trusting it as a certified real end, rather than
            // trusting a short answer alone (`BlockSource`'s own "up to len" contract permits a
            // short answer for a reason other than real EOF). `+ 2 * block_size`, not `+
            // block_size`: one block for the existing block-granular overshoot tolerance, one
            // more for this verification probe -- bounded (exactly one extra read, never
            // recursive), not unbounded slack.
            prop_assert!(
                src.read_count() as usize * block_size <= budget + 2 * block_size,
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
