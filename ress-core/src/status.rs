//! The status worker: one background task that answers "what line number is
//! this anchor on" for the status line. One owned task, cancel-on-drop,
//! publishing immutable snapshots through a watch channel — the same shape
//! as [`crate::schedule::ScanScheduler`], because sequential worker code
//! makes the failure modes of a shared-state machine unrepresentable:
//! supersession is control flow (`select!` racing a fresh anchor against
//! whatever the worker is doing), retries are a loop variable instead of a
//! struct field two call sites can each half-update, and ownership of the
//! in-flight count is a straight line rather than a memo two tasks reach
//! into. The draw side only ever sends the anchor it cares about and reads
//! the latest snapshot; it never drives the count itself.
use crate::cache::BlockCache;
use crate::index::{Frontier, LineIndex};
use crate::scan::{CountScan, CountStep};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// What the worker knows about the anchor it was last asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub anchor: u64,
    pub line: LineNumber,
}

/// A line-number answer in one of three honesties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineNumber {
    /// The resolved line number.
    Known(u64),
    /// The worker itself is making progress toward an answer, or waiting on
    /// something that will: the anchor is not yet covered by the index but
    /// the scan that will cover it is still running, or it is covered and a
    /// budgeted count is mid-walk. Either way a caller needs no other
    /// trigger to get the next update — the worker's own `select!` loop is
    /// already waiting on exactly the event that moves this forward, and
    /// publishes the moment it does.
    Converging,
    /// Nothing is going to change this on its own: the anchor sits past a
    /// dead scan's frontier (an error ended the background index before it
    /// ever reached this far, and a dead scan's `processed_up_to` never
    /// advances again), or the block the count needed kept failing to read
    /// past the retry budget. Quiescent either way — the worker parks until
    /// a different anchor is requested, never busy-looping on an anchor
    /// that will never resolve.
    Unavailable,
}

/// How many consecutive failed block reads the walk retries before giving
/// up on the anchor for good and reporting `Unavailable` — a persistently
/// unreadable block then costs nothing further; a fresh anchor gets a fresh
/// budget.
const STATUS_RETRIES: u8 = 3;

/// One background task answering line-number queries for the status line:
/// the anchor-in/snapshot-out twin of `ScanScheduler`. Dropping the worker
/// aborts the task, so a closed document never leaves a background counter
/// running.
pub struct StatusWorker {
    anchor_tx: watch::Sender<u64>,
    snapshot_rx: watch::Receiver<StatusSnapshot>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for StatusWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl StatusWorker {
    /// Starts the worker at anchor 0 (`Anchor::TOP`); its first pass through
    /// the loop resolves that immediately, without needing a request —
    /// `Known(1)` right away for an empty file, otherwise whatever the
    /// coverage gate and count below it produce, exactly like any other
    /// anchor. `budget` bounds bytes examined per count step, the same
    /// config value navigation scans use.
    pub fn spawn(
        cache: Arc<BlockCache>,
        index: Arc<Mutex<LineIndex>>,
        frontier: watch::Receiver<Frontier>,
        budget: usize,
    ) -> StatusWorker {
        let (anchor_tx, anchor_rx) = watch::channel(0u64);
        let (snapshot_tx, snapshot_rx) = watch::channel(StatusSnapshot {
            anchor: 0,
            line: LineNumber::Converging,
        });
        let task = tokio::spawn(run(cache, index, frontier, anchor_rx, budget, snapshot_tx));
        StatusWorker {
            anchor_tx,
            snapshot_rx,
            task,
        }
    }
    /// Asks the worker for `anchor`'s line number; the answer arrives later
    /// on `status_snapshots`/`line_number`. A request for an anchor the
    /// worker is already mid-walk toward supersedes it outright — the
    /// worker never finishes counting toward an anchor nobody asked about
    /// anymore, it just never resumes that walk. Silently a no-op if the
    /// worker task has already ended (a dropped `Document` racing this
    /// call): the snapshot simply goes stale visibly, like a repaint that
    /// arrives a moment too late.
    ///
    /// Sends through `send_if_modified` rather than `send`, on purpose:
    /// `draw` calls this every frame regardless of whether the anchor
    /// actually moved, and a plain `send` bumps the channel's version even
    /// for a value equal to the current one — which would make every
    /// redraw look like a fresh anchor to the worker's own supersession
    /// `select!` and restart the walk from underneath itself repeatedly,
    /// never letting a slow multi-block count finish. An unchanged anchor
    /// must be a true no-op at the channel itself, not just in spirit.
    pub fn request_line_number(&self, anchor: u64) {
        self.anchor_tx.send_if_modified(|current| {
            if *current == anchor {
                return false;
            }
            *current = anchor;
            true
        });
    }
    /// The latest snapshot the worker has published.
    pub fn line_number(&self) -> StatusSnapshot {
        *self.snapshot_rx.borrow()
    }
    /// A fresh subscription to snapshot updates, for the run loop's repaint
    /// arm.
    pub fn status_snapshots(&self) -> watch::Receiver<StatusSnapshot> {
        self.snapshot_rx.clone()
    }
}

/// The worker's whole life: pull the latest requested anchor, resolve it —
/// through the coverage gate, then a budgeted count from the nearest
/// checkpoint — publishing every state change as it happens, and park until
/// the next request. A newer anchor arriving mid-resolution is not a race
/// to reconcile: `anchor_rx.changed()` inside the two inner `select!`s below
/// is control flow, jumping straight back to the top where
/// `borrow_and_update` sees the freshest value, so a walk toward a
/// superseded anchor is simply never resumed rather than raced to
/// completion and thrown away.
async fn run(
    cache: Arc<BlockCache>,
    index: Arc<Mutex<LineIndex>>,
    mut frontier: watch::Receiver<Frontier>,
    mut anchor_rx: watch::Receiver<u64>,
    budget: usize,
    snapshot_tx: watch::Sender<StatusSnapshot>,
) {
    'outer: loop {
        let anchor = *anchor_rx.borrow_and_update();
        if cache.size() == 0 {
            let _ = snapshot_tx.send(StatusSnapshot {
                anchor,
                line: LineNumber::Known(1),
            });
            if anchor_rx.changed().await.is_err() {
                return; // the Document dropped; end the task.
            }
            continue 'outer;
        }
        // the coverage gate: `checkpoint_at_or_below` only names the true
        // nearest checkpoint once the index has processed strictly past
        // this anchor (see `LineIndex::checkpoint_at_or_below`'s own doc),
        // so nothing below may use its answer before `covered` holds.
        let cp = loop {
            let (covered, done, checkpoint) = {
                let ix = index.lock().unwrap();
                let f = ix.frontier();
                (
                    f.processed_up_to > anchor,
                    f.done,
                    ix.checkpoint_at_or_below(anchor),
                )
            };
            if covered {
                break checkpoint;
            }
            let line = if done {
                LineNumber::Unavailable
            } else {
                LineNumber::Converging
            };
            let _ = snapshot_tx.send(StatusSnapshot { anchor, line });
            if done {
                // the scan died before ever reaching this anchor:
                // processed_up_to will never move again, so nothing short
                // of a new anchor can change the answer.
                if anchor_rx.changed().await.is_err() {
                    return;
                }
                continue 'outer;
            }
            tokio::select! {
                changed = frontier.changed() => {
                    if changed.is_err() {
                        // the scheduler's sender is gone without ever
                        // publishing a done frontier — a teardown race, not
                        // a normal finish; the honest conclusion is the same
                        // one a done frontier would have reached.
                        let _ = snapshot_tx.send(StatusSnapshot {
                            anchor,
                            line: LineNumber::Unavailable,
                        });
                        if anchor_rx.changed().await.is_err() {
                            return;
                        }
                        continue 'outer;
                    }
                    // loop back and re-check coverage against the new frontier.
                }
                changed = anchor_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue 'outer;
                }
            }
        };
        let _ = snapshot_tx.send(StatusSnapshot {
            anchor,
            line: LineNumber::Converging,
        });
        let mut scan = CountScan::new(cp.0, anchor, budget);
        // resets on any step that makes progress, not just an eventual
        // success — a block that fails once and then reads fine is a
        // transient hiccup, not a strike against the retry budget.
        let mut consecutive_failures: u8 = 0;
        loop {
            // one step can consume several blocks before failing on a later
            // one; the cursor advancing proves the earlier blocks healed, so
            // a failure after progress is the first strike at the new
            // position, not another against the old one.
            let before = scan.scanned();
            tokio::select! {
                changed = anchor_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue 'outer;
                }
                step = scan.step(&cache) => {
                    match step {
                        Ok(CountStep::Done(newlines)) => {
                            let _ = snapshot_tx.send(StatusSnapshot {
                                anchor,
                                line: LineNumber::Known(cp.1 + newlines + 1),
                            });
                            if anchor_rx.changed().await.is_err() {
                                return;
                            }
                            continue 'outer;
                        }
                        Ok(CountStep::More) => {
                            consecutive_failures = 0;
                            // a fully-cached walk never actually suspends on
                            // its own; without this, it would monopolize the
                            // runtime instead of sharing it with interactive
                            // reads on the same cache — the same courtesy
                            // ScanScheduler pays after every ingested block.
                            tokio::task::yield_now().await;
                        }
                        Err(e) => {
                            consecutive_failures = if scan.scanned() > before {
                                1
                            } else {
                                consecutive_failures + 1
                            };
                            tracing::debug!("status count failed: {e:#}");
                            if consecutive_failures > STATUS_RETRIES {
                                let _ = snapshot_tx.send(StatusSnapshot {
                                    anchor,
                                    line: LineNumber::Unavailable,
                                });
                                if anchor_rx.changed().await.is_err() {
                                    return;
                                }
                                continue 'outer;
                            }
                            let backoff = std::time::Duration::from_millis(
                                50 * consecutive_failures as u64,
                            );
                            tokio::select! {
                                changed = anchor_rx.changed() => {
                                    if changed.is_err() {
                                        return;
                                    }
                                    continue 'outer;
                                }
                                _ = tokio::time::sleep(backoff) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSource;
    fn cache(data: Vec<u8>, block_size: usize) -> Arc<BlockCache> {
        Arc::new(BlockCache::new(
            Arc::new(MockSource::new(data)),
            block_size,
            1 << 20,
        ))
    }
    fn lined(lines: u32, block_size: usize) -> Arc<BlockCache> {
        // ten-byte lines ("line 0000\n"): 0-based line k starts at 10*k.
        let mut data = Vec::new();
        for i in 0..lines {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        cache(data, block_size)
    }
    async fn wait_for(
        rx: &mut watch::Receiver<StatusSnapshot>,
        pred: impl Fn(StatusSnapshot) -> bool,
    ) -> StatusSnapshot {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snap = *rx.borrow();
                if pred(snap) {
                    return snap;
                }
                rx.changed()
                    .await
                    .expect("worker dropped its sender before resolving");
            }
        })
        .await
        .expect("status worker never reached the expected state")
    }
    #[tokio::test]
    async fn resolves_a_known_line_number_once_covered() {
        let c = lined(3000, 256);
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(40); // 0-based line 4 (1-based line 5).
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| {
            s.anchor == 40 && !matches!(s.line, LineNumber::Converging)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Known(5));
    }
    #[tokio::test]
    async fn empty_file_resolves_immediately_to_known_one() {
        let c = cache(Vec::new(), 64);
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(0);
        let mut rx = w.status_snapshots();
        // anchor 0 is also `spawn`'s placeholder value (honestly
        // `Converging` until the worker's first pass runs), so the
        // predicate must wait past it rather than match on anchor alone.
        let snap = wait_for(&mut rx, |s| {
            s.anchor == 0 && !matches!(s.line, LineNumber::Converging)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Known(1));
    }
    #[tokio::test]
    async fn stays_converging_while_the_scan_is_alive_and_uncovered() {
        // latency keeps the background scan well behind, so the anchor
        // cannot possibly be covered yet — a live scan means Converging,
        // never Unavailable (the old design's blanket "uncovered means
        // Unavailable" is exactly the semantic this worker replaces).
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(format!("line {i:04}\n").as_bytes());
        }
        let src = Arc::new(MockSource::new(data).with_latency(std::time::Duration::from_millis(2)));
        let c = Arc::new(BlockCache::new(src, 256, 1 << 20));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(34990);
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| s.anchor == 34990).await;
        assert_eq!(snap.line, LineNumber::Converging);
    }
    #[tokio::test]
    async fn becomes_unavailable_past_a_dead_scans_frontier() {
        // mirrors schedule.rs's own error-shortened-scan fixture: block 0
        // reads fine, everything after fails, so the frontier finishes done
        // with only bytes [0, 4) ever indexed.
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
        let c = Arc::new(BlockCache::new(Arc::new(FailsAfterFirstBlock), 4, 1 << 20));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(8); // past processed_up_to == 4, forever.
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| s.anchor == 8).await;
        assert_eq!(snap.line, LineNumber::Unavailable);
    }
    #[tokio::test]
    async fn dropping_the_worker_aborts_its_task() {
        // a cache too small to hold the scan's own linear pass: by the time
        // coverage is established, the blocks a walk toward a far anchor
        // needs are cold again, so the walk must re-fetch through the same
        // slow source — giving the drop below something real to interrupt.
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        let src = Arc::new(MockSource::new(data).with_latency(std::time::Duration::from_millis(5)));
        let c = Arc::new(BlockCache::new(src.clone(), 64, 4 * 64));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 64);
        w.request_line_number(4000);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let mid = src.read_count();
        assert!(mid > 0, "walk should be re-reading evicted blocks by now");
        drop(w);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            src.read_count(),
            mid,
            "worker kept reading after being dropped"
        );
    }
    #[tokio::test]
    async fn repeated_identical_requests_do_not_restart_the_walk() {
        // `draw` calls `request_line_number(app.top)` every frame regardless
        // of whether the anchor actually moved. A `watch` send always bumps
        // the channel's version, even for an unchanged value, so without
        // guarding against that, every redraw would look like a fresh
        // anchor to the worker's own supersession `select!` and restart the
        // walk from scratch — never letting a slow multi-block count finish.
        let mut data = Vec::new();
        for _ in 0..8 {
            data.extend_from_slice(b"aaaaaaa\n"); // eight 8-byte lines.
        }
        let src =
            Arc::new(MockSource::new(data).with_latency(std::time::Duration::from_millis(50)));
        // a cache too small to hold the scan's own pass: by the time
        // coverage is established, the blocks the walk needs are cold
        // again, so every one of them is a genuine, freshly-counted read.
        let c = Arc::new(BlockCache::new(src.clone(), 8, 4 * 8));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let before_walk = src.read_count();
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        // one byte short of EOF (64): `processed_up_to > anchor` can never
        // hold at exactly the file size, so this is the furthest coverable
        // anchor — and its window still needs all 8 blocks, since byte 62
        // sits in the 8th (last) one.
        let anchor = 63u64;
        for _ in 0..10 {
            w.request_line_number(anchor);
            tokio::task::yield_now().await;
        }
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| {
            s.anchor == anchor && !matches!(s.line, LineNumber::Converging)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Known(8));
        let reads_for_walk = src.read_count() - before_walk;
        // the window [0, 63) needs exactly 8 cold blocks; ten identical
        // requests must not multiply that.
        assert!(
            reads_for_walk <= 8,
            "read {reads_for_walk} times for an 8-block window; repeated \
             identical requests must not restart the walk"
        );
    }
    #[tokio::test]
    async fn retry_budget_is_scoped_to_the_current_block_not_the_whole_walk() {
        // one `step()` call can span multiple blocks (the budget bounds
        // bytes, not blocks), so a step that consumes several blocks
        // successfully before failing on a later one must not have that
        // earlier progress held against the later block's own retry
        // budget. Each block here fails its first reread (the scan's own
        // first-ever read of every block succeeds; the walk's re-fetch
        // after eviction is what hits the failure) and succeeds on every
        // attempt after that — eight blocks, each needing exactly one
        // retry, must all converge. A retry counter that never resets on
        // intra-step progress exhausts its budget by the fourth block
        // (three prior blocks' failures already spent it), well short of
        // the eighth.
        struct FailsFirstRereadOfEachBlock {
            data: bytes::Bytes,
            attempts: std::sync::Mutex<std::collections::HashMap<u64, u32>>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsFirstRereadOfEachBlock {
            fn size(&self) -> u64 {
                self.data.len() as u64
            }
            async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<bytes::Bytes> {
                let attempt = {
                    let mut a = self.attempts.lock().unwrap();
                    let n = a.entry(offset).or_insert(0);
                    *n += 1;
                    *n
                };
                if attempt == 2 {
                    return Err(anyhow::anyhow!(
                        "transient failure at offset {offset}, attempt {attempt}"
                    ));
                }
                let size = self.data.len() as u64;
                let start = offset.min(size) as usize;
                let end = offset.saturating_add(len as u64).min(size) as usize;
                Ok(self.data.slice(start..end))
            }
        }
        let mut data = Vec::new();
        for _ in 0..8 {
            data.extend_from_slice(b"aaaaaaa\n");
        }
        let src = Arc::new(FailsFirstRereadOfEachBlock {
            data: bytes::Bytes::from(data),
            attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        // small cache: the scan's own linear pass evicts each block behind
        // itself, so the walk's fetches are genuine re-reads (attempt 2)
        // rather than cache hits on the scan's already-successful attempt 1.
        let c = Arc::new(BlockCache::new(src, 8, 4 * 8));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        let anchor = 63u64; // needs all 8 blocks; see the anchor-63 note above.
        w.request_line_number(anchor);
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| {
            s.anchor == anchor && !matches!(s.line, LineNumber::Converging)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Known(8));
    }
    #[tokio::test]
    async fn a_stalled_anchor_stays_frozen_under_repeated_identical_requests() {
        // once the retry budget is spent the worker parks on the anchor
        // channel; identical re-requests are a channel no-op, so nothing
        // can re-kick the doomed walk — the read counter must freeze.
        // every block reads fine on its first attempt (the index scan
        // completes and covers everything) and fails every reread; a cache
        // too small to retain the scan's pass makes the walk's fetches
        // genuine rereads that never heal.
        struct FailsEveryReread {
            reads: std::sync::atomic::AtomicU64,
            attempts: std::sync::Mutex<std::collections::HashMap<u64, u32>>,
        }
        #[async_trait::async_trait]
        impl crate::source::BlockSource for FailsEveryReread {
            fn size(&self) -> u64 {
                64
            }
            async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<bytes::Bytes> {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let attempt = {
                    let mut a = self.attempts.lock().unwrap();
                    let n = a.entry(offset).or_insert(0);
                    *n += 1;
                    *n
                };
                if attempt > 1 {
                    return Err(anyhow::anyhow!("reread of offset {offset} refused"));
                }
                let data = bytes::Bytes::from(b"aaaaaaa\n".repeat(8));
                let start = offset.min(64) as usize;
                let end = offset.saturating_add(len as u64).min(64) as usize;
                Ok(data.slice(start..end))
            }
        }
        let src = Arc::new(FailsEveryReread {
            reads: std::sync::atomic::AtomicU64::new(0),
            attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        let c = Arc::new(BlockCache::new(src.clone(), 8, 4 * 8));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(63);
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| {
            s.anchor == 63 && matches!(s.line, LineNumber::Unavailable)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Unavailable);
        let frozen_at = src.reads.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..10 {
            w.request_line_number(63);
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(
            src.reads.load(std::sync::atomic::Ordering::SeqCst),
            frozen_at,
            "identical requests after a stall must not re-kick the walk"
        );
    }
    #[tokio::test]
    async fn supersession_converges_to_the_latest_requested_anchor() {
        let c = lined(50, 64);
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 1 << 20);
        // ten anchors, back to back, no yields in between: the worker
        // cannot possibly have acted on any but the last by the time it is
        // next scheduled — pileup is structurally impossible, not just rare.
        for i in 0..10u64 {
            w.request_line_number(i * 10);
        }
        w.request_line_number(90); // 0-based line 9 (1-based line 10).
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| {
            s.anchor == 90 && !matches!(s.line, LineNumber::Converging)
        })
        .await;
        assert_eq!(snap.line, LineNumber::Known(10));
    }
}
