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
/// task reads through the same `cache.warm` → `PreadSource::read_block` path):
/// a scan step already inside `read_block`'s `spawn_blocking` closure is not
/// reachable by the abort at all, since tokio documents `spawn_blocking`
/// tasks as uncancellable once running — that one blocking `pread` (at most
/// one, this task never has two reads in flight at once) completes on its
/// own, real-file sources only. A closed document never leaves a stray
/// reader RUNNING AS A TRACKED TASK behind; it can leave at most that one
/// syscall finishing alone.
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
            while idx * bs < size {
                let block = match cache.warm(idx).await {
                    Ok(b) => b,
                    Err(e) => {
                        // a partial index still answers everything below
                        // its frontier; goto_line treats done as "no more
                        // coverage is coming" and clamps to best known.
                        tracing::warn!("background index scan failed: {e:#}");
                        reached_eof = false;
                        break;
                    }
                };
                if block.is_empty() {
                    // warm() returning empty means the offset is past EOF,
                    // not a read failure — the scan still reached the end.
                    break;
                }
                let f = {
                    let mut ix = ix.lock().unwrap();
                    ix.ingest(&block);
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
            async fn read_block(&self, offset: u64, _len: usize) -> anyhow::Result<bytes::Bytes> {
                if offset == 0 {
                    Ok(bytes::Bytes::from_static(b"a\nb\n"))
                } else {
                    Err(anyhow::anyhow!("boom"))
                }
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
        let src = Arc::new(MockSource::new(vec![b'x'; 1 << 20]).with_gate());
        src.arm_gate();
        let c = Arc::new(crate::cache::BlockCache::new(src.clone(), 4096, 1 << 20));
        let s = ScanScheduler::spawn(c);
        let fr = s.frontier();
        let mut started = src.started_events();
        wait_for_count(&mut started, |n| n > 0).await;
        // the task is now genuinely, and PERMANENTLY (until released, which never happens in
        // this test), suspended on the gate armed above.
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        drop(s);
        // POSITIVE proof: the in-flight read's own future was torn down, not left running or
        // let finish on its own.
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
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
