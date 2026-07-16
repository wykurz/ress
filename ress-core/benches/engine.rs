//! Engine-level performance: deterministic in-memory fixtures + injected
//! latency stand in for a network filesystem. Run via `just bench`; CI only
//! compiles this (`--no-run`) so numbers never gate merges.
fn fixture(mib: u64) -> bytes::Bytes {
    let spec = ress_filegen::Spec {
        seed: 42,
        target_bytes: mib << 20,
        line_len: ress_filegen::LineLen::Uniform { min: 10, max: 200 },
        trailing_newline: true,
        mega: None,
        utf8_fraction: 0.1,
    };
    let mut out = Vec::with_capacity((mib << 20) as usize);
    ress_filegen::generate(&spec, &mut out).expect("in-memory generate");
    bytes::Bytes::from(out)
}
// builds a latency-injecting source over the shared fixture bytes; `Bytes::clone`
// is a cheap refcount bump, so cloning it once per iteration is not the 64 MiB
// memcpy `Vec<u8>::clone` would be.
fn source(
    bytes: &bytes::Bytes,
    latency_ms: u64,
) -> std::sync::Arc<dyn ress_core::source::BlockSource> {
    std::sync::Arc::new(
        ress_core::source::MockSource::new(bytes.clone())
            .with_latency(std::time::Duration::from_millis(latency_ms)),
    )
}
// resolves a navigation outcome to its final anchor, joining a pending
// background scan when the interactive budget was exceeded. the engine's own
// equivalent, `Resolution::join`, is `#[cfg(test)]` and `pub(crate)`, so it is
// invisible to this bench (a separate crate) regardless of feature gates;
// this is built entirely from `Resolution`/`PendingNav`'s already-public
// fields instead of widening that production-only helper.
async fn resolve(r: ress_core::resolve::Resolution) -> ress_core::document::Anchor {
    match r {
        ress_core::resolve::Resolution::Ready(a) => a,
        ress_core::resolve::Resolution::Pending(p) => p
            .handle
            .await
            .expect("scan task panicked")
            .expect("scan failed"),
    }
}
// latencies chosen so a full `just bench` stays in minutes: 0 = pure engine,
// 1ms ≈ fast NFS round trip, 5ms ≈ slow mount; sample_size trimmed for the
// latency groups because every injected sleep is real wall time.
const LATENCIES_MS: &[u64] = &[0, 1, 5];
// a representative terminal size. `ress/src/app.rs`'s own gutter-width
// bookkeeping (`content_cols`) is a private concern of that binary crate and
// is not reproduced here; this bench mirrors the engine call `draw` makes
// (`Document::viewport`) at fixed dimensions instead.
const ROWS: usize = 50;
const COLS: usize = 120;
fn first_paint(c: &mut criterion::Criterion) {
    let bytes = fixture(64);
    let mut group = c.benchmark_group("first_paint");
    group.sample_size(10);
    for ms in LATENCIES_MS {
        group.bench_function(format!("latency_{ms}ms"), |b| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            b.to_async(&rt).iter(|| async {
                // a fresh document every iteration: first paint is the cost of
                // opening a file that was never open before, not the warm,
                // already-open path `scroll_warm` measures separately.
                // prefetch_depth: 0 disables Prefetcher::note_viewport's spawn
                // outright (its own first check) — Config::default()'s depth
                // is 8, and Prefetcher has no cancel-on-drop: under injected
                // latency, note_viewport's detached fill tasks (spawned by the
                // viewport() call below) hold only Arc<BlockCache>/Arc<Semaphore>,
                // not this Document, so they outlive it and keep sleeping into
                // the NEXT iteration's fresh Document, contending for the
                // runtime's worker threads while that one is being timed. This
                // isolates the un-contended open path: in the real binary,
                // prefetch runs asynchronously and never blocks the paint
                // itself, so disabling it here does not change what "first
                // paint" means — it removes an artifact of this tight
                // per-iteration loop that production's spaced-out opens never hit.
                let doc = ress_core::document::Document::new(
                    source(&bytes, *ms),
                    ress_core::Config {
                        prefetch_depth: 0,
                        ..ress_core::Config::default()
                    },
                );
                doc.viewport(
                    ress_core::document::Anchor::TOP,
                    ROWS,
                    COLS,
                    ress_core::document::HScroll::ZERO,
                )
                .await
                .expect("first viewport fetch");
            });
        });
    }
    group.finish();
}
fn scroll_warm(c: &mut criterion::Criterion) {
    let bytes = fixture(64);
    let half_page = (ROWS / 2).max(1) as i64;
    let mut group = c.benchmark_group("scroll_warm");
    for (nav_name, delta) in [("scroll_by_one", 1i64), ("half_page", half_page)] {
        for warm_ms in [5u64, 0] {
            let label = if warm_ms == 0 {
                "latency_0"
            } else {
                "warm_5ms"
            };
            group.bench_function(format!("{nav_name}/{label}"), |b| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                // builds ONE document for the whole bench_function (cache
                // pre-warmed by a first viewport fetch) outside the timed
                // loop: this group's whole point is the cache-hit path, so
                // latency may only ever show up here, in setup.
                // new_unindexed, not new: scroll_lines and viewport touch
                // only self.cache/self.prefetcher (confirmed by reading both
                // bodies — neither references self.scheduler/self.status,
                // the same "faithful, non-panicking" test goto_end_cold's
                // identical substitution already relies on), so nothing this
                // bench's routine calls needs the indexer at all. Document::new
                // spawns one regardless, and unlike goto_end_cold/cold_scroll
                // (whose Documents live for exactly one iteration or one
                // scroll_lines call), THIS Document — and its background
                // indexer, had one existed — lives for the group's entire
                // sample collection: with warm_ms latency shaping the
                // indexer's own reads exactly as it shapes everything else
                // this file reads through `source()`, a real indexer is
                // still plausibly mid-scan deep into every measured
                // iteration, competing with the timed scroll/viewport calls
                // for the SAME runtime's worker threads for the group's
                // entire duration, not just its setup — corrupting the
                // "warm, cache-hit-only" measurement this group exists to
                // report. new_unindexed removes that background reader
                // entirely instead of hoping it finishes before sampling
                // starts.
                //
                // Config::default()'s prefetch stays on (unlike
                // first_paint/goto_end_cold's fresh-per-iteration Documents)
                // — re-derived here, not just carried over, now that the
                // indexer is gone: Prefetcher (prefetch.rs) holds only a
                // cache handle, depth, and its own last-offset/direction
                // state — no reference to the scheduler or status worker
                // anywhere in it, confirmed by reading the struct and
                // note_viewport, so indexed-vs-unindexed was never a factor
                // in what it fetches or when, and removing the indexer
                // changes nothing about this reasoning. what the reasoning
                // itself still requires: this one Document, and its cache,
                // live across every iteration, and every iteration scrolls
                // the same short distance from the same fixed TOP anchor, so
                // the blocks note_viewport spawns fills for are the setup
                // fetch's own already-warm region from the second iteration
                // on — BlockCache::warm resolves those fills as a fast cache
                // hit, not a latency-bound source read, so there is nothing
                // latency-shaped left to carry over. (a background indexer's
                // own sequential warm() pass sharing this same cache raised
                // a second, separate question worth ruling out explicitly:
                // could it evict this setup fetch's own warm region before
                // the timed loop reads it back? no — Config::default()'s
                // cache_bytes is 256 MiB against this group's 64 MiB
                // fixture, so the whole file fits without eviction pressure
                // regardless; moot now besides, since the indexer this
                // would have come from no longer exists.)
                let doc = rt.block_on(async {
                    let doc = ress_core::document::Document::new_unindexed(
                        source(&bytes, warm_ms),
                        ress_core::Config::default(),
                    );
                    doc.viewport(
                        ress_core::document::Anchor::TOP,
                        ROWS,
                        COLS,
                        ress_core::document::HScroll::ZERO,
                    )
                    .await
                    .expect("cache warm-up fetch");
                    doc
                });
                b.to_async(&rt).iter(|| async {
                    // scrolls from the same fixed anchor on every iteration:
                    // a running anchor would eventually walk past the warmed
                    // region and start paying cold-read cost mid-measurement.
                    let anchor = resolve(
                        doc.scroll_lines(ress_core::document::Anchor::TOP, delta)
                            .await
                            .expect("scroll"),
                    )
                    .await;
                    doc.viewport(anchor, ROWS, COLS, ress_core::document::HScroll::ZERO)
                        .await
                        .expect("warm viewport fetch");
                });
            });
        }
    }
    group.finish();
}
fn goto_end_cold(c: &mut criterion::Criterion) {
    let bytes = fixture(16);
    let mut group = c.benchmark_group("goto_end_cold");
    group.sample_size(10);
    for ms in LATENCIES_MS {
        group.bench_function(format!("latency_{ms}ms"), |b| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            b.to_async(&rt).iter_batched(
                || {
                    // goto_end never touches self.scheduler/self.status (it
                    // only reads self.size/self.cache — confirmed by reading
                    // its body), so new_unindexed is a faithful cold setup
                    // here. PerIteration (not SmallInput) matters too:
                    // SmallInput can batch several Document::new instances'
                    // setup before any routine runs, so their background
                    // indexer/status tasks (spawned by the indexed
                    // constructor this used to call) would contend for the
                    // runtime's worker threads during the timed routines —
                    // new_unindexed removes that source entirely rather than
                    // just de-batching it. new_unindexed still builds a
                    // Prefetcher, though, so prefetch_depth: 0 is needed here
                    // too — see first_paint's comment for the carryover
                    // mechanism (Prefetcher has no cancel-on-drop); this
                    // group's own viewport() call below, after the tail jump,
                    // would otherwise spawn the same detached fills.
                    ress_core::document::Document::new_unindexed(
                        source(&bytes, *ms),
                        ress_core::Config {
                            prefetch_depth: 0,
                            ..ress_core::Config::default()
                        },
                    )
                },
                |doc| async move {
                    let anchor = resolve(doc.goto_end(ROWS).await.expect("goto_end")).await;
                    doc.viewport(anchor, ROWS, COLS, ress_core::document::HScroll::ZERO)
                        .await
                        .expect("tail viewport fetch");
                },
                criterion::BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}
// line 500_000 of a ~640_000-line, 64 MiB fixture (10-200 byte lines):
// comfortably in-bounds, and far enough in to make the cold linear walk
// expensive and the checkpoint jump cheap by comparison.
const TARGET_LINE: u64 = 500_000;
fn goto_line_cold_vs_indexed(c: &mut criterion::Criterion) {
    let bytes = fixture(64);
    let mut group = c.benchmark_group("goto_line_cold_vs_indexed");
    // three ways to reach the same target line, each isolating a different
    // cost. cold_scroll: a pure index-free forward scroll — a legal stand-in
    // for goto_line's own tail-walk, not goto_line itself (goto_line cannot
    // be called at all on an unindexed document — see its own comment
    // below). pending: the actual goto_line cold path a user hits on a real
    // open — construction AND goto_line both inside the measured span (see
    // the leg's own comment for why: a Document built in iter_batched's own
    // setup, as this leg used to, gives its background indexer an
    // unmeasured head start before the timed call even begins), asked for
    // the target line before the background scan has caught up, resolving
    // through Resolution::Pending. indexed: goto_line once the background
    // scan has already finished, the synchronous best case.
    // pending sits between the other two: it pays for a real background
    // scan racing toward the target (cold_scroll never spawns one) but can
    // resolve as soon as the target line itself is covered, not necessarily
    // the whole file (unlike index_throughput's full-file scan).
    // `goto_line` itself panics on a `new_unindexed` document — it needs the
    // scheduler unconditionally (`document.rs`'s `NO_INDEX`) — so the cold
    // leg cannot call `goto_line` at all; it reaches the target line the
    // only index-free way the engine offers, a plain forward scroll from the
    // top, which is the "pure tail-walk" `goto_line`'s own uncovered path
    // otherwise approximates once the background index catches up to it.
    // the label says "cold_scroll", not "cold": bare criterion output is
    // what a regression tracker reads, and a leg that never calls goto_line
    // must not report under goto_line's name alone.
    group.bench_function("cold_scroll", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        // the document must be built INSIDE the batched setup, not once
        // outside iter(): a document built outside the timed closure is
        // still shared across criterion's warm-up iterations, which fully
        // populate its block cache before the first *measured* iteration
        // ever runs — every measured sample would then read a warm cache,
        // not a cold one. PerIteration forces a fresh, empty cache for
        // every sample instead of letting criterion batch several setups
        // ahead of their routines. Config::default()'s prefetch stays on
        // here (unlike first_paint/goto_end_cold): the routine below never
        // calls viewport(), the only site that triggers
        // Prefetcher::note_viewport, so there is no spawn to carry over.
        b.to_async(&rt).iter_batched(
            || {
                ress_core::document::Document::new_unindexed(
                    source(&bytes, 0),
                    ress_core::Config::default(),
                )
            },
            |doc| async move {
                // scroll_lines(TOP, n) resolves the start of the n-th line
                // strictly after line-start 0 (ForwardScan::new's doc
                // comment), landing on 0-based line n. goto_line(n) below
                // lands on 0-based line n-1 (n is 1-based there). so
                // TARGET_LINE - 1 here lands on the exact same 0-based line
                // `indexed`'s goto_line(TARGET_LINE) resolves to.
                resolve(
                    doc.scroll_lines(ress_core::document::Anchor::TOP, (TARGET_LINE - 1) as i64)
                        .await
                        .expect("cold scroll to target line"),
                )
                .await;
            },
            criterion::BatchSize::PerIteration,
        );
    });
    // the actual cold goto_line path a user hits: open a normally-indexed
    // Document and immediately ask it to goto_line(TARGET_LINE), before its
    // background scan has caught up — resolving through Resolution::Pending
    // (wait for the indexer's frontier, then a bounded checkpoint-tail
    // walk). cold_scroll above is not this path either: goto_line panics on
    // an unindexed document (NO_INDEX in document.rs), so cold_scroll's
    // index-free scroll_lines walk is the closest legal stand-in for "no
    // index at all," not "index still catching up," which is what a real
    // cold open actually does.
    //
    // Document::new lives INSIDE the measured async block below, not in a
    // separate iter_batched setup closure the way this leg used to build
    // it (matching first_paint's shape): a Document built in criterion's
    // setup phase is excluded from the timed span, but its background
    // indexer keeps running regardless — on a multithread runtime, setup
    // finishing and the routine actually starting are not the same
    // instant, and the indexer spends whatever gap exists between them
    // making unmeasured progress toward TARGET_LINE, a real, if small,
    // head start this leg's own point (measuring how much catch-up cost a
    // user actually pays) argues against hiding. Measured directly (a
    // targeted, isolated repro of iter_batched's own setup-to-routine
    // handoff, not Document::new's specific weight): tens of nanoseconds,
    // not milliseconds — and a real before/after of this exact leg at a
    // larger sample size (300, not criterion's own default 100 — the
    // effect is small enough that 100 samples alone were not resolving
    // it cleanly run to run) shows the median LANDING WITHIN NOISE of the
    // pre-fix number (10.452ms pre-fix vs 10.481ms post-fix, +0.28%,
    // criterion's own significance test: p=0.61, "No change in
    // performance detected") — not the rise a hidden-head-start theory
    // alone would predict. The fix is made anyway, and is correct
    // regardless of whether its effect clears this leg's own noise floor:
    // the leg's own name and meaning changes for the better either way —
    // this is no longer "goto_line on an already-existing, already-
    // scanning Document" but "cold open + goto_line + resolve," the
    // honest, complete user story a real `ress somefile` followed
    // immediately by a line jump actually pays for, and one fewer thing
    // for a future reader to have to reason about ("is some of this
    // leg's cost happening off-camera?").
    group.bench_function("pending", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        b.to_async(&rt).iter(|| async {
            // a real Document::new (not new_unindexed): the background
            // index scan it spawns is the thing under measurement, so it
            // must run for real, on a real tokio runtime. latency 0: like
            // index_throughput, the scan's own progress is the quantity
            // under test here, not an injected read cost. prefetch_depth:
            // 0 is a no-op for this leg specifically (goto_line's Pending
            // path never calls viewport(), the only call site that
            // reaches Prefetcher::note_viewport — confirmed by reading
            // document.rs) but kept for consistency with this file's
            // other fresh-per-iteration cold legs.
            let doc = ress_core::document::Document::new(
                source(&bytes, 0),
                ress_core::Config {
                    prefetch_depth: 0,
                    ..ress_core::Config::default()
                },
            );
            // TARGET_LINE is asked for immediately after construction (no
            // other await point precedes this), so the background scan
            // just spawned has had essentially no wall-clock time to reach
            // a line 78% of the way through a 64 MiB fixture — this is
            // what forces Resolution::Pending here instead of racing to
            // Ready, and it lands on the identical 0-based line indexed's
            // goto_line(TARGET_LINE) resolves to (same call, same
            // argument, no off-by-one to derive). ScanScheduler/
            // StatusWorker both abort their background task on Drop, so
            // `doc` going out of scope at the end of this async block
            // cancels the scan cleanly before the next iteration spawns a
            // fresh one — no cross-iteration contention, the same
            // guarantee the old setup-closure shape had, without needing
            // iter_batched/PerIteration to get it: plain `iter()` already
            // runs this whole block, construction included, exactly once
            // per sample, with nothing else batched ahead of it.
            resolve(doc.goto_line(TARGET_LINE).await.expect("pending goto_line")).await;
        });
    });
    group.bench_function("indexed", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Config::default()'s prefetch stays on: one Document built once,
        // outside iter() (so no per-iteration fresh cache to warm anyway),
        // and its goto_line routine below never calls viewport() — nothing
        // for note_viewport to spawn against, let alone carry over.
        let doc = rt.block_on(async {
            let doc =
                ress_core::document::Document::new(source(&bytes, 0), ress_core::Config::default());
            let mut frontier = doc.index_frontier();
            while !frontier.borrow().done {
                frontier
                    .changed()
                    .await
                    .expect("index scan ended unexpectedly");
            }
            doc
        });
        b.to_async(&rt).iter(|| async {
            resolve(doc.goto_line(TARGET_LINE).await.expect("indexed goto_line")).await;
        });
    });
    group.finish();
}
fn index_throughput(c: &mut criterion::Criterion) {
    let bytes = fixture(64);
    let len = bytes.len() as u64;
    let mut group = c.benchmark_group("index_throughput");
    group.throughput(criterion::Throughput::Bytes(len));
    group.bench_function("scan_64mib", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Config::default()'s prefetch stays on: a fresh Document every
        // iteration, like first_paint/goto_end_cold, but the routine below
        // only awaits index_frontier() and never calls viewport() — the
        // background index scan goes through ScanScheduler, not Prefetcher,
        // so there is no note_viewport call here to spawn a fill in the
        // first place.
        b.to_async(&rt).iter(|| async {
            let doc =
                ress_core::document::Document::new(source(&bytes, 0), ress_core::Config::default());
            let mut frontier = doc.index_frontier();
            while !frontier.borrow().done {
                frontier
                    .changed()
                    .await
                    .expect("index scan ended unexpectedly");
            }
        });
    });
    group.finish();
}
criterion::criterion_group!(
    benches,
    first_paint,
    scroll_warm,
    goto_end_cold,
    goto_line_cold_vs_indexed,
    index_throughput
);
criterion::criterion_main!(benches);
