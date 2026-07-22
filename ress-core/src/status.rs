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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// aborts the task at whichever ordinary `.await` it is currently suspended
/// at (the coverage-gate `select!`, or between count steps) — via `tasks`'s
/// own `Drop` (`TaskOwner`, pass 7 P7-C), not a hand-written one: see
/// `task_owner.rs`'s own doc comment for why `JoinSet`'s abort-on-drop
/// (what `TaskOwner` wraps) replaces the hand-written `self.task.abort()`
/// this struct used to need. The one exception (same finding as
/// `ScanScheduler`'s own doc comment — this worker's count walk reads
/// through the identical `cache.warm` → `PreadSource::read_block` path via
/// `CountScan::step`): a step already inside `read_block`'s
/// `spawn_blocking` closure keeps running regardless, tokio's own
/// `spawn_blocking` tasks being uncancellable once started — at most one
/// such syscall per worker, real-file sources only. A closed document
/// never leaves a background counter running AS A TRACKED TASK; it can
/// leave at most that one syscall finishing alone.
pub struct StatusWorker {
    anchor_tx: watch::Sender<u64>,
    snapshot_rx: watch::Receiver<StatusSnapshot>,
    // never read explicitly -- that IS the point: its existence alone (and, transitively, its
    // own `JoinSet`'s) is what provides the cancel-on-drop guarantee this struct's own doc
    // comment describes, needing no method call anywhere to do it. See `task_owner.rs`'s own
    // doc comment.
    #[allow(dead_code)]
    tasks: crate::task_owner::TaskOwner<()>,
    // found in PR #44 pass 8 (U-worker, 1a): a positive signal for `request_line_number`'s own
    // SKIP decision (see that method's own doc comment on `send_if_modified`) -- an identical,
    // already-current anchor is a genuine channel-level no-op that must never wake the worker's
    // task at all. A test proving a stalled anchor stays stalled under repeated identical
    // requests used to infer that from a bounded silence window on a downstream read counter (a
    // timing oracle: the regression it guards against -- `send_if_modified` regressing to a bare
    // `send` -- only becomes observable there after the worker's own retry backoff sleeps
    // elapse). This exposes the decision itself, positively, at the instant it is made, with no
    // dependency on the worker's task ever being scheduled.
    request_skipped: AtomicU64,
    request_skipped_tx: watch::Sender<u64>,
    // found in PR #44 pass 8 (U-worker, 1a's document.rs sibling): a positive signal for how
    // many times `run`'s own `'outer` loop has begun a fresh resolution attempt (every
    // iteration, including the first). Unlike `request_skipped` above, this covers the case
    // where NOTHING is ever re-requested at all -- proving the worker's retry-exhaustion state
    // permanently sticks needs a signal on the WORKER's own side, since there is no request-side
    // decision to observe when no request arrives. `run` owns the actual counter (it is a single
    // sequential task; no atomic is needed), and only publishes its high-water mark here.
    //
    // `#[cfg(test)]` (p6-review2, pass 8 U-worker review, point 4): unlike `request_skipped` and
    // `cache.rs`'s own `coalesced` (both fire only on a comparatively rare event), this increments
    // and publishes on EVERY `'outer` iteration -- including ordinary supersession restarts from
    // normal interactive scrolling, which can be frequent in real use. Gating this field (and, in
    // `run`, the two loop-top statements that write it) removes that recurring per-iteration cost
    // from the production status-worker loop entirely, leaving only a one-time, effectively-free
    // unused-channel construction in `spawn` -- `run`'s own parameter and the channel that feeds
    // it stay unconditional regardless, since conditionally changing `run`'s signature itself
    // would be the "messier signature-level cfg" this amendment specifically avoids.
    #[cfg(test)]
    resolution_attempts_tx: watch::Sender<u64>,
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
        let (request_skipped_tx, _) = watch::channel(0u64);
        let (resolution_attempts_tx, _) = watch::channel(0u64);
        let mut tasks = crate::task_owner::TaskOwner::new();
        tasks.spawn(run(
            cache,
            index,
            frontier,
            anchor_rx,
            budget,
            snapshot_tx,
            resolution_attempts_tx.clone(),
        ));
        StatusWorker {
            anchor_tx,
            snapshot_rx,
            tasks,
            request_skipped: AtomicU64::new(0),
            request_skipped_tx,
            #[cfg(test)]
            resolution_attempts_tx,
        }
    }
    /// A clone of the inbound anchor sender -- test-only. Holding this alive independently of
    /// the `StatusWorker` itself lets a test drop the worker while keeping the anchor channel
    /// open, isolating abort-via-Drop from channel-close (`run`'s own `anchor_rx.changed()`
    /// returning `Err` the instant the last sender goes away, entirely apart from whether
    /// `.abort()` ever ran -- see `dropping_the_worker_aborts_its_in_flight_step`'s own doc
    /// comment, this module's own tests, for the full isolation this exists for).
    #[cfg(test)]
    pub(crate) fn anchor_sender_for_test(&self) -> watch::Sender<u64> {
        self.anchor_tx.clone()
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
        let changed = self.anchor_tx.send_if_modified(|current| {
            if *current == anchor {
                return false;
            }
            *current = anchor;
            true
        });
        // `send_if_modified` already returns exactly this decision -- see `request_skipped`'s
        // own doc comment on this struct for why it is worth publishing, not just returning.
        if !changed {
            let n = self.request_skipped.fetch_add(1, Ordering::Relaxed) + 1;
            publish_high_water_mark(&self.request_skipped_tx, n);
        }
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
    /// A receiver a test can `wait_for`/`changed()` on to observe how many
    /// `request_line_number` calls found an already-current anchor and
    /// correctly skipped -- never touching the anchor channel at all, so
    /// the worker's own task never wakes for them -- rather than starting
    /// or restarting a walk. See `request_skipped`'s own doc comment on
    /// this struct for why this exists.
    #[cfg(test)]
    pub(crate) fn request_skipped_events(&self) -> watch::Receiver<u64> {
        self.request_skipped_tx.subscribe()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many times `run`'s own
    /// `'outer` loop has begun a fresh resolution attempt -- see `resolution_attempts_tx`'s own
    /// doc comment on this struct for why this exists (proving a stalled worker's own retry
    /// exhaustion permanently sticks when nothing is ever re-requested at all).
    #[cfg(test)]
    pub(crate) fn resolution_attempts_events(&self) -> watch::Receiver<u64> {
        self.resolution_attempts_tx.subscribe()
    }
    /// Aborts the worker task and awaits its own teardown to finish, rather
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
// `send_if_modified`, monotonic high-water-mark publish, no-op with no receivers -- the
// identical contract `source.rs`'s own `publish_high_water_mark` documents at length (also
// reimplemented locally in `prefetch.rs` and `cache.rs`); kept local here too rather than
// imported, per the same P7-C layering principle: this is the status worker's own request-level
// signal, not the block-source's.
fn publish_high_water_mark(sender: &watch::Sender<u64>, value: u64) {
    sender.send_if_modified(|current| {
        if value > *current {
            *current = value;
            true
        } else {
            false
        }
    });
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
    resolution_attempts_tx: watch::Sender<u64>,
) {
    // acknowledges the parameter unconditionally: `run`'s own signature stays the same in every
    // build (see `resolution_attempts_tx`'s own doc comment, `StatusWorker`, for why gating the
    // signature itself is exactly the "messier" cfg this amendment avoids), but the ONLY use of
    // this value below is inside the `#[cfg(test)]` block, so a non-test build would otherwise
    // see it as unused.
    let _ = &resolution_attempts_tx;
    // a single sequential task drives this whole loop -- no atomic needed, unlike
    // `StatusWorker::request_skipped` (genuinely concurrent callers of `request_line_number`).
    // `#[cfg(test)]`: test-only instrumentation, see `resolution_attempts_tx`'s own doc comment.
    #[cfg(test)]
    let mut resolution_attempts: u64 = 0;
    'outer: loop {
        #[cfg(test)]
        {
            resolution_attempts += 1;
            publish_high_water_mark(&resolution_attempts_tx, resolution_attempts);
        }
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
    use crate::source::{MockSource, wait_for_count};
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
        // audit finding 1b: `run` publishes Converging from TWO sites -- the coverage-gated
        // one (this test's own target, line ~204) and an UNCONDITIONAL one right after the
        // coverage gate is satisfied (line ~240). A test that merely waits for "any Converging"
        // cannot discriminate a genuinely gated wait from an already-(mis-)covered anchor whose
        // count-scan just ALSO happens to publish Converging on its own way in -- a broken
        // `covered` (e.g. always `true`) would be invisible regardless of how long the test
        // waits, since BOTH paths publish the identical `{anchor, Converging}` payload.
        //
        // Gating block 0 forever (never opened) makes coverage PERMANENTLY, STRUCTURALLY
        // unsatisfied -- not just "probably not yet, given enough real latency" -- so the scan
        // is genuinely alive (its own task is still running, parked, not done) yet can never
        // advance `processed_up_to` past 0. The discriminator: `BlockCache::coalesced_events`
        // (pass 8, U-cache) proves the worker's own count-scan never even attempted a read. If
        // `covered` incorrectly returned `true`, the resulting count-scan would need to walk
        // from checkpoint `(0, 0)` -- the only checkpoint that can exist with zero progress,
        // `LineIndex::new`'s own built-in first entry -- straight into the SAME still-in-flight,
        // gated block 0, registering as a coalescing waiter on it. `in_flight_len` cannot serve
        // this role (same reasoning as U-cache's own): it would show exactly 1 either way, since
        // a waiter joining an existing fetch does not change the count of distinct in-flight
        // BLOCKS.
        let data: &'static [u8] = Box::leak(vec![b'x'; 4096].into_boxed_slice());
        let src = Arc::new(MockSource::new(data).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src, 256, 1 << 20));
        // captured BEFORE anything is spawned: the real risk is ANY yield before this baseline
        // capture, not specifically "the worker's own default anchor-0 pass" (p6-review2, pass 8
        // U-worker review, point 1, refining the earlier framing here) -- `request_line_number
        // (2000)` below runs synchronously, with no yield, immediately after both spawns, so the
        // worker's very first `run()` iteration likely never actually processes anchor 0 at all:
        // by the time `run()`'s task gets its first chance to execute, `anchor_rx.borrow_and_
        // update()` already reads 2000, not the seeded 0. Regardless of which anchor triggers the
        // FIRST coverage check, checkpoint `(0, 0)` is the only one reachable with zero index
        // progress, so the practical conclusion is identical either way: a baseline captured
        // after any yield at all would silently absorb the coalescing event this test exists to
        // catch.
        let coalesced = c.coalesced_events();
        let coalesced_baseline = *coalesced.borrow();
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let w = StatusWorker::spawn(c.clone(), s.index().clone(), s.frontier(), 1 << 20);
        w.request_line_number(2000); // deep into the file; unreachable while block 0 is parked.
        let mut rx = w.status_snapshots();
        let snap = wait_for(&mut rx, |s| s.anchor == 2000).await;
        assert_eq!(snap.line, LineNumber::Converging);
        // a few cooperative yields give a hypothetically-bypassed coverage gate a genuine
        // chance to kick off a count-scan and attempt (and coalesce onto) the gated block --
        // not real or virtual time, just scheduler turns, matching the same reasoning as the
        // 1a fixes above.
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
            w.line_number().line,
            LineNumber::Converging,
            "a genuinely uncovered anchor must never progress to Known while parked"
        );
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
    // found in PR #44 pass 6 S4 (#8, a user-review finding) and pass 7 (P7-C, a p6-review2
    // delta-review rewrite, "go through real StatusWorker::drop"): proves `StatusWorker`'s own
    // `Drop` genuinely aborts its task -- through the REAL `StatusWorker::spawn`/`Drop`, not by
    // calling `run` directly and aborting a raw `JoinHandle` by hand the way this test used to.
    // That older shape (`abort_ends_an_in_flight_step_with_anchor_tx_still_alive`, deleted here,
    // itself the pass-6-S4 fix for an EARLIER test that only proved "drop stops reading, however
    // that happens" without isolating `.abort()`'s own contribution) isolated `.abort()` cleanly
    // but never touched `StatusWorker` at all -- it proved `abort()` works in isolation, not that
    // `Drop` calls it. `w.anchor_sender_for_test()` clones `anchor_tx` and holds it alive for
    // this whole test, so when `w` drops, its own internal `anchor_tx` dropping cannot close the
    // channel (this clone keeps it open) -- the identical isolation idea the deleted test used,
    // now applied to the type real callers actually use.
    //
    // found in PR #44 pass 6 S4 (validated before implementing, not assumed; carried forward
    // unchanged -- the finding is about `run`'s own `select!`, not about which test drives it): a
    // "hanging read" design -- a read that never resolves, specifically to put the task somewhere
    // channel-close supposedly cannot reach -- was considered and directly, empirically disproven
    // before being built: `tokio::select!` (this loop's own `select!` around `scan.step`, see
    // `run`'s own body above) polls EVERY one of its arms independently on each wake,
    // `anchor_rx.changed()` included, regardless of whether a SIBLING arm's own future is designed
    // to never resolve. A throwaway experiment confirmed this directly: `run` driven with an
    // hour-long read, then `anchor_tx` DROPPED (not aborted) -- the task still ended within 500ms,
    // via `run`'s own ordinary `return` (a plain `Ok(())` join result, not a cancellation), exactly
    // as fast as a normal-latency read would have. A hanging read does not, in fact, make a task
    // unreachable by channel-close on this runtime; it would have been an equally confounded test,
    // not a fix. The genuinely unconfounded approach is the one already used here: hold a clone of
    // `anchor_tx` ALIVE so channel-close never happens at all, regardless of how long the in-flight
    // read takes.
    //
    // The gated `MockSource` puts the task inside a REAL, suspended `.await` when this fires --
    // parked there deterministically (see `source.rs`'s own doc comment on `gate_tx`), the same
    // ordinary, cooperative await point a latency sleep would suspend at, fully abort-able at its
    // own poll point (unlike `PreadSource`'s real `spawn_blocking` reads, the one documented
    // exception `StatusWorker`'s own doc comment already names -- not what this test is about,
    // and not reachable with a `MockSource` regardless).
    //
    // Cancelled is now a POSITIVE event (pass 7, P7-C: `MockSource`'s own `cancelled_events`, a
    // scopeguard on `read_block`'s own future -- see `source.rs`'s own doc comment), not inferred
    // from a bounded silence. The deleted test also checked `JoinError::is_cancelled` directly;
    // this one can't -- `StatusWorker` does not expose a raw handle to check, an encapsulation
    // improvement, not a loss -- `task_owner.rs`'s own dedicated probe test pins that tokio-level
    // fact once, generically, so it does not need re-deriving here.
    //
    // Finally, confirms `BlockCache` itself survives an abort mid-fetch cleanly: a REAL, separate
    // `.block(0)` call against the SAME cache, after the abort, must still succeed normally --
    // proving the cache's `Mutex` isn't wedged by a fetch that will never finish, not that
    // `InFlightGuard` specifically cleaned up its own registration (block 0 is never the block the
    // aborted anchor-4000 walk registered, ~62). That guard-specific property is `cache.rs`'s
    // `aborted_fetcher_does_not_leak_its_registration` to prove, via a direct `in_flight_len`
    // assertion on the interrupted block itself.
    //
    // RED-verified: temporarily deleted `TaskOwner`'s field-drop-abort by swapping `StatusWorker`'s
    // `spawn` to leak the task via `std::mem::forget` on a throwaway `TaskOwner` instead of storing
    // the real one (simulating "the field never gets dropped, so nothing aborts") -- this test
    // failed at its 5s timeout waiting for a `Cancelled` event that never arrived. Reverted
    // immediately after confirming.
    //
    // The gate below (pass 7's structural pass, codex P2, a 3rd re-review) is NOT a RED-verified
    // fix in that same sense, stated honestly rather than overclaimed: reverting just this test's
    // own source construction to its pre-fix `.with_latency(Duration::from_millis(5))` shape (a
    // method since deleted entirely, U-delete, pass 8 -- source.rs's own struct doc comment has
    // the full history) and running it 30 times in a row never once failed on this system. The
    // gate closes an undocumented-by-CONTRACT reliance on scheduling, not a reproduced flake --
    // nothing in `tokio::time::sleep`'s own contract guarantees a fixed-duration sleep cannot
    // elapse in the gap between this test's own `wait_for_count` resolving and `drop(w)` actually
    // running, only that it usually does not on this system today. The gate makes the read's own
    // inability to complete unaided into a structural fact instead, provable regardless of what
    // any particular scheduler happens to do.
    #[tokio::test]
    async fn dropping_the_worker_aborts_its_in_flight_step() {
        let mut data = vec![b'x'; 4096];
        data.push(b'\n');
        // a gate, disarmed until armed below -- not a fixed latency (codex P2, PR #44 pass 7's
        // structural pass, a 3rd re-review, the shared root of 3 findings at once: a fixed-latency
        // read can complete and free whatever it holds on its own timeline, independent of when
        // this test gets around to observing it -- descheduled past that window, `drop(w)` below
        // lands after the read already finished, leaving nothing in flight to cancel). Disarmed at
        // construction rather than armed immediately, unlike `prefetch.rs`'s own use of the same
        // mechanism: the SCAN just below shares this exact source and must read through it
        // normally first -- arming happens only once that is done, right before the one read this
        // test actually wants to gate.
        let src = Arc::new(MockSource::new(data).with_gate());
        let c = Arc::new(BlockCache::new(src.clone(), 64, 4 * 64));
        let s = crate::schedule::ScanScheduler::spawn(c.clone());
        let mut fr = s.frontier();
        while !fr.borrow().done {
            fr.changed().await.unwrap();
        }
        src.arm_gate();
        // kept alongside the worker below, specifically to check the cache is still fully usable
        // after the abort.
        let cache_after = c.clone();
        let w = StatusWorker::spawn(c, s.index().clone(), s.frontier(), 64);
        let anchor_keepalive = w.anchor_sender_for_test();
        let mut started = src.started_events();
        let baseline = *started.borrow();
        w.request_line_number(4000);
        wait_for_count(&mut started, |n| n > baseline).await;
        // the task is now genuinely, and PERMANENTLY (until released, which never happens in this
        // test), suspended mid-walk, parked on the gate armed above -- unlike a fixed-latency
        // read, there is no window in which it could complete on its own before the drop below.
        let mut cancelled = src.cancelled_events();
        let cancelled_baseline = *cancelled.borrow();
        drop(w);
        // POSITIVE proof: the in-flight read's own future was torn down, not left running or let
        // finish on its own. `anchor_keepalive` is STILL alive here, proven by construction (it
        // was never dropped, moved out of, or closed anywhere above) -- this cannot have been
        // produced by channel closure.
        wait_for_count(&mut cancelled, |n| n > cancelled_baseline).await;
        drop(anchor_keepalive);
        // the gate stays armed for every future read on this source, not just the one this test
        // meant to gate -- opened here, now that the abort is already positively proven above, so
        // the cache-still-usable check just below (a fresh, ungated read against a DIFFERENT
        // block) does not itself park forever against a gate with no more purpose left to serve.
        src.open_gate();
        cache_after
            .block(0)
            .await
            .expect("the cache must still be usable after an abort mid-fetch");
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
        // no injected latency: the discriminator is the structural read-count assertion below,
        // not timing -- empirically confirmed (U-delete), not assumed: built the actual
        // regression this test guards against (send_if_modified -> a plain send) and ran it at
        // both the original 50ms and at 0ms latency -- it failed reliably at both (17 stray
        // reads at 50ms, 80 at 0ms; more loudly at 0ms, since an unthrottled worker races
        // through more restarts before the walk's own deadline), and the correct code passed
        // cleanly 10/10 at 0ms. Both numbers confirmed against `with_latency`'s own real
        // mechanism specifically (count-before-sleep -- `BlockEventGuard::new` fires, and so
        // increments `read_count`, BEFORE the injected sleep, inside `read_block` itself), not
        // a latency-injecting wrapper reproduction of it: a wrapper that sleeps BEFORE
        // delegating to the wrapped source undercounts here, since an attempt aborted mid-sleep
        // by this worker's own `select!` restart never reaches the wrapped source's own count
        // at all -- reconciling a p6-review2 finding that used exactly such a wrapper and,
        // for that reason alone, could not reproduce the 50ms number (see engine.rs's own
        // `LatencySource` doc comment for the general gotcha this uncovered). Latency was
        // genuinely incidental at either duration, not masked: send_if_modified already makes 9
        // of these 10 requests unconditional no-ops at the channel level (see
        // `request_skipped_events`), so the walk-restart signature this test catches shows up
        // in the read count regardless of how fast any one read resolves.
        let src = Arc::new(MockSource::new(data));
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
        let mut skipped = w.request_skipped_events();
        let skip_baseline = *skipped.borrow();
        for _ in 0..10 {
            w.request_line_number(63);
            tokio::task::yield_now().await;
        }
        // POSITIVE proof, not a bounded silence window: `request_line_number`'s own
        // `send_if_modified` decision is published the instant each call is made (see
        // `request_skipped`'s own doc comment, this file) -- reaching skip_baseline + 10 here
        // proves all ten identical re-requests were genuinely recognized as no-ops, without
        // ever touching the anchor channel, rather than merely "nothing arrived within 60ms."
        // The `yield_now()` calls above remain load-bearing regardless: a re-kick regression
        // would still need the worker's own (woken) task to actually be polled before the
        // read-count assertion below could observe it.
        wait_for_count(&mut skipped, |n| n >= skip_baseline + 10).await;
        assert_eq!(
            *skipped.borrow(),
            skip_baseline + 10,
            "exactly ten identical re-requests must be recognized as skips, no more, no fewer"
        );
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
