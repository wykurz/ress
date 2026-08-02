//! The background scan: one sequential pass over the file through the
//! shared cache, feeding the line index and publishing a frontier. The
//! file is read once at block granularity; interactive reads stay
//! prioritized because the pass warms only the probationary segment and
//! yields between blocks.
/// Owns the background indexing task; dropping it aborts the scan at
/// whichever ordinary `.await` it is currently suspended at (including the
/// explicit yield between ingested blocks, below) — via `tasks`'s own
/// `Drop` (`TaskOwner`, pass 7 P7-C), not a hand-written one: see
/// `task_owner.rs`'s own doc comment for why `JoinSet`'s abort-on-drop
/// (what `TaskOwner` wraps) replaces the hand-written `self.task.abort()`
/// this struct used to need — the byte-for-byte same shape `StatusWorker`
/// used to hand-roll too, both now sharing the identical mechanism instead
/// of two copies that could drift apart. The one exception (found in PR
/// #44 round 10, auditing `Prefetcher`'s own identical exposure — this
/// task reads through the same `cache.warm` path): the abort does not reach
/// the block cache's own fetch, which is detached from every requester by
/// design and for every source, not only real files (batch 4 (2026-07-24),
/// finding #11 — batch 22 (2026-08-01) corrects this comment, which still
/// described the older, `spawn_blocking`-only boundary). Dropping the step
/// drops its `WaiterTicket`; the fetch behind it (at most one, this task
/// never has two reads in flight at once) dies unstarted if one of its own
/// abandon checks OBSERVES that it was the last waiter — each check is a
/// linearization point, not a promise about the window around it — and
/// otherwise completes and publishes into the cache. A closed document never leaves a stray reader
/// RUNNING AS A TRACKED TASK behind; it can leave at most that one detached
/// fetch finishing alone, and what it finishes is an ordinary cache fill.
pub struct ScanScheduler {
    index: std::sync::Arc<std::sync::Mutex<crate::index::LineIndex>>,
    frontier: tokio::sync::watch::Receiver<crate::index::Frontier>,
    // never read explicitly -- that IS the point: its existence alone (and, transitively, its
    // own `JoinSet`'s) is what provides the cancel-on-drop guarantee this struct's own doc
    // comment describes, needing no method call anywhere to do it. See `task_owner.rs`'s own
    // doc comment.
    #[allow(dead_code)]
    tasks: crate::task_owner::TaskOwner<()>,
}
impl ScanScheduler {
    /// Starts indexing immediately; progress arrives on `frontier`.
    pub fn spawn(cache: std::sync::Arc<crate::cache::BlockCache>) -> ScanScheduler {
        let index = std::sync::Arc::new(std::sync::Mutex::new(crate::index::LineIndex::new()));
        let (tx, rx) = tokio::sync::watch::channel(crate::index::Frontier::default());
        let ix = index.clone();
        let mut tasks = crate::task_owner::TaskOwner::new();
        tasks.spawn(async move {
            let size = cache.size();
            let bs = cache.block_size() as u64;
            let mut idx = 0u64;
            // stays true unless the loop breaks out on a read error below.
            let mut reached_eof = true;
            // restructure R3 (design point 5, the driver regime): this pass keeps its own
            // existing per-block structural bound (one `idx` per iteration, `size` decides when
            // to stop), never a byte budget -- `read_at_unbounded` is charged but never refused,
            // routed through the Reader purely so it is the ONLY read path in the codebase, not
            // to gate this loop's own progress on it. No `begin_step()`/per-call boundary: unlike
            // an interactive scan's own `step()`, this loop has no external caller resuming it
            // across calls, so there is no "step" for `spent_this_step` to be scoped to, and
            // nothing here ever reads it (`out_of_budget`/`progress_witness` are both budget/
            // interactive-step concepts this driver-regime loop does not use).
            let mut meter = crate::meter::Meter::background(cache.block_size());
            let mut reader = crate::meter::Reader::new(&cache, &mut meter);
            while idx * bs < size {
                let fetched = match reader
                    .read_at_unbounded(
                        idx * bs,
                        bs as usize,
                        crate::meter::Access::Peek,
                        crate::meter::Charge::Payload,
                    )
                    .await
                {
                    Ok(f) => f,
                    Err(e) => {
                        // a partial index still answers everything below
                        // its frontier; goto_line treats done as "no more
                        // coverage is coming" and clamps to best known.
                        tracing::warn!("background index scan failed: {e:#}");
                        reached_eof = false;
                        break;
                    }
                };
                // an improvement over the old `warm()`-then-check-`is_empty()` dance, not a
                // behavior change: `classify` (inside `read_at_unbounded`) can certify a short,
                // truncated final block on its own FIRST touch (`Fetched::Short` with a non-empty
                // `got`), where the old code needed a SECOND, now-redundant touch at the same
                // `idx` (returning an empty block past EOF) to discover the identical fact --
                // same bytes ingested, same `reached_eof`, one fewer physical read.
                let got = match fetched {
                    crate::meter::Fetched::Bytes { got, .. } => got,
                    crate::meter::Fetched::Short { got, .. } => got,
                    crate::meter::Fetched::Empty { .. } => bytes::Bytes::new(),
                };
                if got.is_empty() {
                    // an empty result here means the offset is past EOF,
                    // not a read failure — the scan still reached the end.
                    break;
                }
                let f = {
                    let mut ix = ix.lock().unwrap();
                    ix.ingest(&got);
                    ix.frontier()
                };
                let _ = tx.send(f);
                idx += 1;
                // give aborts a guaranteed point to take effect when the
                // cache serves every block without awaiting.
                tokio::task::yield_now().await;
            }
            let f = {
                let mut ix = ix.lock().unwrap();
                ix.finish(reached_eof);
                ix.frontier()
            };
            let _ = tx.send(f);
        });
        ScanScheduler {
            index,
            frontier: rx,
            tasks,
        }
    }
    /// The shared index, for query-time checkpoint lookups.
    pub fn index(&self) -> &std::sync::Arc<std::sync::Mutex<crate::index::LineIndex>> {
        &self.index
    }
    /// A fresh frontier subscription.
    pub fn frontier(&self) -> tokio::sync::watch::Receiver<crate::index::Frontier> {
        self.frontier.clone()
    }
    /// Aborts the scan task and awaits its own teardown to finish, rather
    /// than the fire-and-forget abort-request `Drop` alone provides -- see
    /// `TaskOwner::abort_all_and_join`'s own doc comment for the mechanism
    /// and `Prefetcher::abort_and_join`'s own doc comment for why a
    /// criterion bench needs this distinction at all. Bench-visible (test +
    /// bench-internals, matching `Document::new_unindexed`'s own
    /// precedent).
    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) async fn abort_and_join(&mut self) {
        self.tasks.abort_all_and_join().await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{MockSource, wait_for_count};
    use std::sync::Arc;
    fn cache(data: Vec<u8>, block_size: usize) -> Arc<crate::cache::BlockCache> {
        Arc::new(crate::cache::BlockCache::new(
            Arc::new(MockSource::new(data)),
            block_size,
            1 << 20,
        ))
    }
    async fn wait_done(rx: &mut tokio::sync::watch::Receiver<crate::index::Frontier>) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !rx.borrow().done {
                rx.changed()
                    .await
                    .expect("scheduler dropped its sender before done");
            }
        })
        .await
        .expect("index scan never finished");
    }
    #[tokio::test]
    async fn indexes_the_whole_file_and_reports_done() {
        let mut data = Vec::new();
        for i in 0..3000u32 {
            data.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let len = data.len() as u64;
        let c = cache(data, 256);
        let s = ScanScheduler::spawn(c);
        let mut rx = s.frontier();
        wait_done(&mut rx).await;
        let f = *rx.borrow();
        assert_eq!(f.processed_up_to, len);
        assert_eq!(f.lines_so_far, 3000);
        assert_eq!(s.index().lock().unwrap().total_lines(), Some(3000));
    }
    #[tokio::test]
    async fn empty_file_is_done_immediately() {
        let s = ScanScheduler::spawn(cache(Vec::new(), 64));
        let mut rx = s.frontier();
        wait_done(&mut rx).await;
        assert_eq!(s.index().lock().unwrap().total_lines(), Some(0));
    }
    #[tokio::test]
    async fn background_scan_warms_but_never_promotes() {
        // the whole point of feeding the scheduler through warm(): promote
        // only matters on a probationary HIT, so block 0 is touched once
        // interactively BEFORE the scan — the scheduler's visit is then the
        // second touch, and warm() must leave it probationary where block()
        // would promote it (review finding: without the pre-touch, the two
        // are indistinguishable here and the test has no power).
        let data = vec![b'x'; 4096];
        let c = cache(data, 64);
        let _ = c.block(0).await.unwrap();
        let s = ScanScheduler::spawn(c.clone());
        let mut rx = s.frontier();
        wait_done(&mut rx).await;
        assert_eq!(c.protected_len(), 0, "background fill must not promote");
    }
    #[tokio::test]
    async fn background_scan_leaves_protected_recency_alone() {
        // promote block 3 then block 0 via two touches each, giving protected
        // MRU order [0, 3]; the scan then visits every block through warm(),
        // including both already-protected blocks in file order (0 before
        // 3) — if warm() refreshed protected recency the same way block()
        // does, that later touch to 3 would flip the order to [3, 0].
        let data = vec![b'x'; 40];
        let c = cache(data, 4);
        let _ = c.block(3).await.unwrap();
        let _ = c.block(3).await.unwrap();
        let _ = c.block(0).await.unwrap();
        let _ = c.block(0).await.unwrap();
        assert_eq!(c.protected_keys(), vec![0, 3]);
        let s = ScanScheduler::spawn(c.clone());
        let mut rx = s.frontier();
        wait_done(&mut rx).await;
        assert_eq!(
            c.protected_keys(),
            vec![0, 3],
            "background scan must not reorder protected recency"
        );
    }
    #[tokio::test]
    async fn error_shortened_scan_leaves_a_done_frontier_counting_the_frontier_line() {
        // block 0 reads fine; every later block fails, so the scan stops
        // after ingesting "a\nb\n" (2 newlines) with 4 more real bytes
        // sitting unread past the frontier — total_lines must count the
        // line starting there rather than undercounting to the newlines.
        struct FailsAfterFirstBlock;
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsAfterFirstBlock {
            fn size(&self) -> u64 {
                8
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
        let c = Arc::new(crate::cache::BlockCache::new(
            Arc::new(FailsAfterFirstBlock),
            4,
            1 << 20,
        ));
        let s = ScanScheduler::spawn(c);
        let mut rx = s.frontier();
        wait_done(&mut rx).await;
        let f = *rx.borrow();
        assert!(f.done, "a failed scan must still report done");
        assert_eq!(f.processed_up_to, 4);
        assert_eq!(s.index().lock().unwrap().total_lines(), Some(3));
    }
    #[tokio::test]
    async fn dropping_the_scheduler_aborts_the_scan() {
        // an armed gate holds the scan's very first read open forever (until released, which
        // never happens in this test) -- unlike a fixed-latency read (this test's own pre-
        // U-delete shape), which can complete and free whatever it holds on its own timeline,
        // independent of when this test gets around to observing it, this is a structural
        // guarantee: the read genuinely cannot complete on its own before the drop below.
        // Armed immediately, unlike `status.rs`'s own use of the same mechanism: nothing here
        // needs an earlier read to go through normally first, since the scan's own FIRST read
        // is the one this test wants gated.
        //
        // REVISED, batch 4 (2026-07-24), finding #11: `cache.rs`'s own fetch path became a
        // detached publisher (`BlockCache::get`'s own doc comment), so the block's physical read
        // no longer dies with the scan that triggered it -- dropping `s` below no longer fires a
        // `cancelled_events` on `src` at all (the detached fetch survives and completes once
        // opened). What still, correctly, dies is the SCAN TASK itself: `frontier`'s own sender
        // (`tx`, this file's own `spawn`) lives inside the scan's `async move` block, moved
        // there, never cloned elsewhere -- so it closes if and only if that task's own future is
        // torn down or returns, entirely independent of whatever the cache's own detached fetch
        // does. `fr.changed()` erroring is therefore a POSITIVE, task-scoped proof the scan
        // itself was aborted, the direct replacement for the read-scoped `cancelled_events` wait
        // this test used to make. Not a silence/absence wait: a leaked, never-aborted scan (the
        // regression this test exists to catch) would leave `tx` alive forever, so `fr.changed()`
        // would simply never resolve, failing loud at `wait_for`'s own bound instead of passing
        // vacuously -- the identical qualitative gap (resolves promptly vs. hangs to the bound)
        // AGENTS.md's own timing-oracle carve-out already accepts.
        let src = Arc::new(MockSource::new(vec![b'x'; 1 << 20]).with_gate());
        src.arm_gate();
        let c = Arc::new(crate::cache::BlockCache::new(src.clone(), 4096, 1 << 20));
        let s = ScanScheduler::spawn(c);
        let mut fr = s.frontier();
        // `borrow_and_update` immediately, defensively: `ScanScheduler`'s own `frontier` field is
        // a stale template `frontier()` only ever `.clone()`s, never itself advanced, so a fresh
        // clone's very first `changed()` would resolve against any accumulated backlog rather
        // than a genuinely new event (see `status.rs`'s own identical fix, with the RED
        // verification, for the sibling case where this matters in practice). Here it is a no-op
        // in practice -- the gate holds the scan's very first read, before its first `tx.send` --
        // but making the baseline explicit costs nothing and removes the coincidence.
        fr.borrow_and_update();
        let mut started = src.started_events();
        wait_for_count(&mut started, |n| n > 0).await;
        // the task is now genuinely, and PERMANENTLY (until released, which never happens in
        // this test), suspended on the gate armed above -- not yet having sent a single
        // frontier update (the gate holds the scan's very first read, before its first `tx.send`).
        drop(s);
        // POSITIVE proof: the scan task's own future was torn down, not left running or let
        // finish on its own -- `frontier`'s sender lives only inside that task (see this test's
        // own comment above), so its channel can only close via the scan task itself ending.
        // Bounded by the same diagnostic ceiling `wait_for_count` itself uses (source.rs): a
        // leaked, never-aborted scan would leave this parked forever rather than pass silently.
        let closed = tokio::time::timeout(crate::source::DIAGNOSTIC_CEILING, fr.changed())
            .await
            .expect("the scan's own frontier sender never closed -- was it genuinely aborted?");
        assert!(
            closed.is_err(),
            "the scan task itself must have been torn down for its own frontier sender to close"
        );
        // a watch receiver keeps the last value after its sender drops: an
        // abort mid-scan leaves the initial default frontier (done == false)
        // behind, while a natural finish would have sent one with done ==
        // true — this is the direct discriminator between the two.
        assert!(
            !fr.borrow().done,
            "the scan must have been aborted, not completed"
        );
    }
}
