//! Background prefetch: watches the viewport anchor, infers the scroll
//! direction, and keeps the next blocks warm in the shared cache so scrolling
//! never waits on a cold read. Fills are best-effort tasks bounded by a
//! semaphore; a direction change or jump simply changes what gets requested
//! next (in-flight single-block reads complete and stay useful in the cache).
//!
//! Fills join the ownership half of the crate's concurrency idiom (see
//! `docs/concurrency.md`): every fill is tracked in one `TaskOwner` (pass 7,
//! P7-C -- a thin `JoinSet` wrapper shared with `ScanScheduler`/
//! `StatusWorker`, see `task_owner.rs`'s own doc comment) owned by the
//! `Prefetcher`, so dropping the `Prefetcher` — which happens exactly when
//! its `Document` does, since it is a plain field, never shared — drops
//! that owner, which aborts every handle it holds. What that abort actually
//! reaches (found in PR #44 round 10, verified against tokio's own source,
//! not assumed): a queued fill still waiting on the semaphore dies outright,
//! never having started a read at all; a fill suspended at any ordinary
//! `.await` (the semaphore acquire, or between yields) dies there, same as
//! the queued case. A fill that has already reached `cache.warm` →
//! `PreadSource::read_block`'s `spawn_blocking` closure is a THIRD case,
//! not a variant of the first two: `spawn_blocking` tasks are documented by
//! tokio itself as uncancellable once running, so the blocking `pread`
//! completes on its own (or, on a wedged mount, hangs on its own) regardless
//! of the outer task's abort — only that outer wrapper, and any value it
//! would have produced, is gone. At most `FILL_CONCURRENCY` fills can be in
//! that third state at once (the semaphore's own permit count), so a drop
//! leaves at most that many single-block reads outstanding, real-file
//! sources only (`docs/prefetch.md`'s own Cancellation section states this
//! same bound). The idiom's other half does not apply here in production: a
//! fill's only observable effect is warming the shared cache, not an answer
//! a consumer reads back, so there is nothing to publish there. In TESTS
//! specifically (pass 7, P7-C, a p6-review2 finding, "replace latency-and-
//! silence cancellation tests"), each fill DOES publish two positive events
//! of its own -- `fill_cancelled_events`/`fill_released_events`, a
//! scopeguard wrapping the whole fill body including its semaphore wait
//! (`FillEventGuard`, below) -- always present, cheap and unobserved when
//! nobody subscribes (the identical no-receivers-is-a-no-op contract
//! `MockSource`'s own events already document, source.rs), so production
//! code path and behavior are unchanged; only a test that actually
//! subscribes ever sees them. This is what lets a cancellation test prove
//! even a QUEUED fill (one that never reached `read_block`, so
//! `MockSource`'s own events say nothing about it) was genuinely torn down,
//! not merely inferred absent from a bounded silence.
use crate::cache::BlockCache;
use crate::document::Anchor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Concurrent background fills: enough to hide latency without starving
/// interactive reads through the shared source (revisit with real NFS data).
const FILL_CONCURRENCY: usize = 4;

/// Best-effort background warming of the blocks ahead of the viewport.
/// Every fill spawned by `note_viewport` is tracked in `state`'s
/// `TaskOwner`, never detached: dropping a `Prefetcher` drops that owner,
/// which aborts every handle it holds — a queued or cooperatively-awaiting
/// fill dies outright, but a fill already inside its own blocking read does
/// not (see this module's own doc comment for the full, three-layer
/// account and why that residual is bounded by `FILL_CONCURRENCY`,
/// real-file sources only). The task-abort guarantee itself is still
/// reached through `state`'s own field-drop, not a hand-written one FOR
/// THAT PURPOSE — see `task_owner.rs`'s own doc comment for why that is
/// structural (`TaskOwner` wraps a `JoinSet`, which already aborts
/// everything it holds on drop) rather than a call this module makes
/// itself, the same mechanism `ScanScheduler`/`StatusWorker` each use for
/// their own single task. `Prefetcher` does have one hand-written `Drop`
/// (below), for a narrower, different reason — see its own doc comment.
pub struct Prefetcher {
    cache: Arc<BlockCache>,
    depth: usize,
    sem: Arc<tokio::sync::Semaphore>,
    state: Mutex<PrefetchState>,
    // found in PR #44 pass 7 (P7-C): fill-level Cancelled/Released events, ALWAYS present (not
    // `#[cfg(test)]`-gated -- production runs the identical code path a test does, just with
    // nobody ever subscribing), so a cancellation test can prove even a QUEUED fill (one that
    // never reached `read_block`, so `MockSource`'s own events say nothing about it) was
    // genuinely torn down -- see this module's own doc comment and `FillEventGuard`, below.
    // `Arc`-wrapped (unlike `MockSource`'s own equivalent fields, which are plain): a fill's own
    // spawned task is `'static` and cannot borrow `&Prefetcher`, so `note_viewport` clones this
    // ONE `Arc` into each fill instead, the same way it already clones `cache`/`sem`.
    fill_events: Arc<FillEvents>,
}
struct FillEvents {
    // found in PR #44 pass 7's structural pass (a 3rd codex re-review, the shared root of 3
    // findings at once): a fill still queued on the semaphore has been SPAWNED but may not yet
    // have been POLLED even once -- and `FillEventGuard::new` (below), like any constructor, only
    // runs on that first poll. A test that drops the `Prefetcher` without first confirming all N
    // queued fills were actually polled can abort a future that never built this guard at all, so
    // no `Cancelled` fires for it -- not a race in the abort itself (`TaskOwner`'s own guarantee
    // is unconditional), only in whether the thing being aborted had anything to report yet.
    // `entered` generalizes `started`/`cancelled`/`released` to this one earlier point: a positive
    // event fired at construction, before any `.await` this future takes (including the semaphore
    // acquire), so a test can wait for ALL N fills to have entered before ever dropping the owner
    // -- synchronization, not scheduler luck, the same upgrade `task_owner.rs`'s own
    // `SignalOnDrop` gets for the identical reason.
    //
    // Stated honestly, not overclaimed: a 500-iteration probe checking the entered-count at the
    // exact point the OLD (pre-this-fix) test used to drop never once observed it land below 8 on
    // this system -- tokio's own current-thread scheduler appears to reliably drain its ready
    // queue here. This field closes an undocumented-by-CONTRACT reliance on that scheduling
    // behavior, not a reproduced flake -- nothing in tokio's own public API guarantees every
    // freshly spawned task gets polled before a sibling future resumes, so the old test's
    // correctness depended on an implementation detail, whether or not this system's own
    // scheduler happens to make it hold in practice today.
    entered: AtomicU64,
    cancelled: AtomicU64,
    released: AtomicU64,
    entered_tx: tokio::sync::watch::Sender<u64>,
    cancelled_tx: tokio::sync::watch::Sender<u64>,
    released_tx: tokio::sync::watch::Sender<u64>,
}
struct PrefetchState {
    last_offset: Option<u64>,
    direction: i64,
    tasks: crate::task_owner::TaskOwner<()>,
}

// found in PR #44 pass 7 (P7-C, a p6-review2 finding, "close the queued-four residual"):
// the same scopeguard shape as `source.rs`'s `BlockEventGuard`, deliberately NOT shared with it
// (a p6-review2 layering finding, same pass: this is `Prefetcher`'s own event surface, not
// `MockSource`'s, and the two owners differ -- an `Arc<FillEvents>` here, `&'a MockSource` there,
// forced apart by the `'static` spawn bound below -- so genericizing over "who to notify" would
// cost an abstraction for two call sites with no reuse actually realized). Constructed at the TOP
// of a fill's own async block, BEFORE the semaphore acquire -- so a fill still queued on
// `sem.acquire()` has already entered its own async block and constructed this guard, meaning an
// abort reaches it (and fires `Cancelled`) exactly the same as one already inside `cache.warm`.
// `complete()` marks a fill that ran to its own natural end (successful or not -- `cache.warm`'s
// own result is already best-effort and discarded) as `Released` instead.
struct FillEventGuard {
    events: Arc<FillEvents>,
    completed: bool,
}
impl FillEventGuard {
    fn new(events: Arc<FillEvents>) -> Self {
        // fires the instant this fill's own async body starts running -- before the semaphore
        // acquire just below it in `note_viewport`'s own spawned future, so a fill still queued
        // on the semaphore has already published this the moment it entered at all. See
        // `FillEvents::entered`'s own doc comment for why this exists as a positive event rather
        // than being inferred from scheduler timing.
        let n = events.entered.fetch_add(1, Ordering::Relaxed) + 1;
        FillEventGuard::publish(&events.entered_tx, n);
        Self {
            events,
            completed: false,
        }
    }
    fn complete(&mut self) {
        self.completed = true;
    }
    // `send_if_modified`, monotonic high-water-mark publish, no-op with no receivers -- the
    // identical contract `source.rs`'s own `publish_high_water_mark` documents at length;
    // reimplemented locally rather than imported (pass 7, P7-C layering ruling: task/fill
    // ownership code must not depend on the block-source file).
    fn publish(sender: &tokio::sync::watch::Sender<u64>, value: u64) {
        sender.send_if_modified(|current| {
            if value > *current {
                *current = value;
                true
            } else {
                false
            }
        });
    }
}
impl Drop for FillEventGuard {
    fn drop(&mut self) {
        if self.completed {
            let n = self.events.released.fetch_add(1, Ordering::Relaxed) + 1;
            Self::publish(&self.events.released_tx, n);
        } else {
            let n = self.events.cancelled.fetch_add(1, Ordering::Relaxed) + 1;
            Self::publish(&self.events.cancelled_tx, n);
        }
    }
}

// found in PR #44 pass 8 (U-prefetch, finding 1): resource-ordering cleanup, NOT a second
// cancellation mechanism -- the task-abort guarantee is still `state`'s own field-drop
// (`TaskOwner`/`JoinSet`, see this struct's own doc comment above), unconditional and
// unchanged. This closes a narrower, different gap: `Drop::drop` always runs BEFORE a type's
// own fields drop, so closing `sem` here happens strictly before `state` drops and starts
// aborting tracked tasks. Without it, the two could interleave the wrong way: aborting an
// ACTIVE fill (one that already holds a permit, mid-`cache.warm`) drops that fill's own local
// `SemaphorePermit`, which releases the permit back to the semaphore -- and if a still-QUEUED
// fill's own pending `sem.acquire().await` has not yet itself been aborted at that exact
// moment, it could observe the just-freed permit, win it, and start a brand-new read (via the
// existing `let Ok(_permit) = sem.acquire().await else { return; }` path in `note_viewport`,
// which would simply succeed instead of returning) during what is supposed to be shutdown --
// one more `cache.warm` call, which, once it reaches a real read, is uncancellable-once-running
// the same way any other fill's read is (this module's own doc comment). `Semaphore::close()`
// closes this outright rather than hoping the abort wins a scheduling race: once closed, every
// `acquire()` call -- already pending, or made afterward -- fails immediately, regardless of
// how many permits get released later. See `note_viewport`'s own `sem.acquire()` call site for
// the `Err` path this makes reachable during shutdown, which was previously only reachable if
// the semaphore's sender side were dropped (never true in this module) rather than closed.
impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.sem.close();
    }
}

impl Prefetcher {
    /// A prefetcher keeping `depth` blocks warm; `depth == 0` disables it.
    pub fn new(cache: Arc<BlockCache>, depth: usize) -> Self {
        let (entered_tx, _) = tokio::sync::watch::channel(0);
        let (cancelled_tx, _) = tokio::sync::watch::channel(0);
        let (released_tx, _) = tokio::sync::watch::channel(0);
        Self {
            cache,
            depth,
            sem: Arc::new(tokio::sync::Semaphore::new(FILL_CONCURRENCY)),
            state: Mutex::new(PrefetchState {
                last_offset: None,
                direction: 1,
                tasks: crate::task_owner::TaskOwner::new(),
            }),
            fill_events: Arc::new(FillEvents {
                entered: AtomicU64::new(0),
                cancelled: AtomicU64::new(0),
                released: AtomicU64::new(0),
                entered_tx,
                cancelled_tx,
                released_tx,
            }),
        }
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many fills have had their
    /// own `FillEventGuard` constructed -- i.e. been polled at least once, entering their own
    /// async body -- regardless of whether they went on to acquire a semaphore permit at all.
    /// See `FillEvents::entered`'s own doc comment for why a test would wait on this before
    /// dropping the `Prefetcher`, rather than assuming every spawned fill got a turn by now.
    #[cfg(test)]
    pub(crate) fn fill_entered_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.fill_events.entered_tx.subscribe()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many fills (in-flight OR
    /// still queued on the semaphore) have had their own future dropped WITHOUT reaching their
    /// own end -- see this struct's own doc comment and `FillEventGuard`.
    #[cfg(test)]
    pub(crate) fn fill_cancelled_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.fill_events.cancelled_tx.subscribe()
    }
    /// The `Released` sibling of `fill_cancelled_events` -- how many fills have run to their own
    /// end (successful or not).
    #[cfg(test)]
    pub(crate) fn fill_released_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.fill_events.released_tx.subscribe()
    }
    /// Notes the viewport anchor, infers direction, and spawns fills for the
    /// next `depth` blocks that way. Cheap and non-blocking; safe outside a
    /// runtime only if `depth == 0`.
    pub fn note_viewport(&self, top: Anchor) {
        if self.depth == 0 {
            return;
        }
        let bs = self.cache.block_size() as u64;
        let offset = top.offset();
        let block = offset / bs;
        let total_blocks = self.cache.size().div_ceil(bs);
        let mut st = self.state.lock().unwrap();
        // compare offsets, not block indices: most scrolls stay inside one
        // block, and a reversal must flip the direction before the anchor
        // crosses a block boundary or the first read after turning is cold.
        if let Some(last) = st.last_offset {
            if offset > last {
                st.direction = 1;
            } else if offset < last {
                st.direction = -1;
            }
        }
        st.last_offset = Some(offset);
        let direction = st.direction;
        // drop any fills that have already finished: a JoinSet only reaps a
        // completed handle when polled, so without this the backlog count
        // below would grow forever instead of reflecting fills still running
        // or still queued.
        st.tasks.reap_finished();
        // hard cap on queued fills: on a slow source, redraws can outpace
        // completions, and the semaphore bounds active reads but not queued
        // tasks. skipping new spawns at the cap keeps the backlog finite;
        // the cache's coalescing makes any skipped block cheap to fetch later.
        if st.tasks.len() > self.depth * 2 {
            return;
        }
        for step in 1..=self.depth as i64 {
            let idx = block as i64 + direction * step;
            if idx < 0 || idx as u64 >= total_blocks {
                break;
            }
            let cache = self.cache.clone();
            let sem = self.sem.clone();
            let fill_events = self.fill_events.clone();
            st.tasks.spawn(async move {
                let mut guard = FillEventGuard::new(fill_events);
                let Ok(_permit) = sem.acquire().await else {
                    return;
                };
                // best effort: a failed fill is retried by whoever needs the block.
                let _ = cache.warm(idx as u64).await;
                guard.complete();
            });
        }
    }
    /// Awaits all outstanding fills; used by tests for determinism.
    #[cfg(test)]
    pub(crate) async fn settle(&self) {
        let mut tasks = {
            let mut st = self.state.lock().unwrap();
            std::mem::take(&mut st.tasks)
        };
        tasks.join_all().await;
    }
    /// Aborts every outstanding fill and awaits its own teardown to finish --
    /// unlike `settle` above (waits for NATURAL completion, no abort at
    /// all), this is `TaskOwner::abort_all_and_join` under the same
    /// take-then-drain shape. See that method's own doc comment for the
    /// mechanism; see `engine.rs`'s own `first_paint` group for why a
    /// criterion bench needs it: `JoinSet::drop` only REQUESTS an abort (a
    /// queued or cooperatively-awaiting fill dies at its own next poll, not
    /// synchronously with the drop call), so a bare `drop(doc)` between
    /// iterations leaves the PREVIOUS iteration's own still-unwinding fills
    /// free to keep contending for the same runtime worker threads the NEXT
    /// iteration's own timed work needs -- a source of measurement noise a
    /// fixed-latency `MockSource` mostly hides (its simulated wait is a
    /// plain, fast-to-unwind `tokio::time::sleep`) but never structurally
    /// rules out. Bench-visible (test + bench-internals, matching
    /// `Document::new_unindexed`'s own precedent): only `pub(crate)` is
    /// needed here specifically, since the bench itself only ever reaches
    /// this indirectly through `Document`'s own wrapper, same crate either
    /// way -- unlike `new_unindexed`, which the bench calls directly.
    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) async fn abort_and_join(&self) {
        // found in PR #44 pass 8 fold re-review (codex P2, on 4e2400f): this used to skip the
        // `sem.close()` step `Drop` (above) does first, reopening the identical race that fix
        // closes, via a second code path -- see `Drop`'s own doc comment for the full mechanism.
        // Reproduced directly against exactly this method before this fix landed (30 parallel
        // runs of `no_queued_fill_starts_a_read_after_abort_and_join`, this module's own tests
        // below: 1 observed `started_events` at 5 against an expected 4 -- a queued fill won a
        // permit an aborting active fill's own drop had just freed, and started a brand-new
        // read, mid-shutdown); clean across 50 more runs of the same test with this fix in
        // place. Ordered identically to `Drop`: closed BEFORE the take-then-abort below, so a
        // still-queued fill's own pending `sem.acquire()` can never observe a permit an aborting
        // active fill's own drop frees -- it fails immediately instead, the same guarantee
        // `Drop` already gives.
        self.sem.close();
        let mut tasks = {
            let mut st = self.state.lock().unwrap();
            std::mem::take(&mut st.tasks)
        };
        tasks.abort_all_and_join().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{MockSource, wait_for_count};
    fn cache_of(len: usize, block_size: usize) -> (Arc<MockSource>, Arc<BlockCache>) {
        let src = Arc::new(MockSource::new(vec![b'x'; len]));
        let c = Arc::new(BlockCache::new(src.clone(), block_size, 1 << 20));
        (src, c)
    }
    #[tokio::test]
    async fn prefetches_ahead_in_scroll_direction() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 4);
        p.note_viewport(Anchor::TOP);
        p.note_viewport(Anchor::TOP);
        p.settle().await;
        let warmed = src.read_count();
        // blocks 1..=4 are now resident: touching them adds no source reads.
        for idx in 1..=4u64 {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(
            src.read_count(),
            warmed,
            "prefetched blocks were not resident"
        );
    }
    #[tokio::test]
    async fn direction_flips_when_scrolling_up() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 2);
        p.note_viewport(Anchor::TOP);
        // simulate being at block 32 then moving up to block 30.
        let at = |b: u64| Anchor::at(b * 64);
        p.note_viewport(at(32));
        p.note_viewport(at(30));
        p.settle().await;
        let warmed = src.read_count();
        for idx in [29u64, 28u64] {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(src.read_count(), warmed, "upward prefetch missed");
    }
    #[tokio::test]
    async fn direction_flips_within_a_single_block() {
        // reversals usually happen inside one block; direction must come from
        // anchor offsets, not block indices.
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 2);
        p.note_viewport(Anchor::at(40 * 64));
        p.note_viewport(Anchor::at(40 * 64 + 32));
        p.note_viewport(Anchor::at(40 * 64 + 16));
        p.settle().await;
        let warmed = src.read_count();
        for idx in [39u64, 38u64] {
            let _ = c.block(idx).await.unwrap();
        }
        assert_eq!(
            src.read_count(),
            warmed,
            "upward prefetch missed after in-block reversal"
        );
    }
    #[tokio::test]
    async fn depth_zero_disables_prefetch() {
        let (src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 0);
        p.note_viewport(Anchor::TOP);
        p.settle().await;
        assert_eq!(src.read_count(), 0);
    }
    // found in PR #44 pass 7 (P7-C): the sibling positive case for `FillEventGuard` (mirroring
    // `task_owner.rs`'s own `a_task_that_finishes_on_its_own_is_not_reported_cancelled`): a fill
    // that runs to its own natural end -- never touched by a drop mid-flight -- must signal
    // Released, not Cancelled, proving the guard genuinely discriminates the two outcomes rather
    // than firing Cancelled unconditionally on every Drop (which would make
    // `dropping_the_prefetcher_cancels_in_flight_and_queued_fills`'s own Cancelled-count assertion
    // meaningless -- it would pass even if abort were never reached at all).
    #[tokio::test]
    async fn completed_fills_signal_released_not_cancelled() {
        let (_src, c) = cache_of(1 << 12, 64);
        let p = Prefetcher::new(c.clone(), 4);
        p.note_viewport(Anchor::TOP);
        p.settle().await;
        assert_eq!(
            *p.fill_released_events().borrow(),
            4,
            "every fill that ran to completion must have signalled Released"
        );
        assert_eq!(
            *p.fill_cancelled_events().borrow(),
            0,
            "a fill that was never interrupted must never signal Cancelled"
        );
    }
    // found in PR #44 round 16 (a codex P2): this test used to sleep 10ms and EXPECT that to
    // have been enough real time for four fills to have acquired every semaphore permit and
    // started reading -- a correct implementation fails that race under a loaded/parallel
    // executor, exactly the "tests prove events, not scheduler timing" rule (AGENTS.md) this
    // workspace already states elsewhere. Replaced with an event handshake:
    // `MockSource::started_events()` is awaited via `wait_for` until it reports
    // `FILL_CONCURRENCY`, no sleep, no guessed margin.
    //
    // found in PR #44 pass 6 S3 (codex, "bound the read-start handshake waits with a timeout"):
    // that `wait_for` used to be raw and unbounded -- see `wait_for_count`'s own doc
    // comment (source.rs) for why an unbounded wait on an external signal is worth bounding on
    // its own, not only because #4's specific regression is now fixed.
    //
    // found in PR #44 pass 7 (P7-C, a p6-review2 finding, "close the queued-four residual"): the
    // tail of this test used to prove the queued four's own cancellation by SILENCE -- a 100ms
    // bounded wait for "no further read starts," unavoidably an inference from absence rather
    // than a positive observation, and the last "latency-and-silence" proof this module's own
    // cancellation coverage still had (the in-flight four's own cancellation was already provable
    // via `MockSource`, but the queued four never reach `read_block` at all, so `MockSource`'s
    // own events say nothing about them). Closed via `FillEventGuard` (this module's own, wraps
    // the WHOLE fill including its semaphore wait): all 8 fills -- 4 in-flight, 4 still queued on
    // the semaphore -- have equally already entered their own async block and constructed a
    // guard, so `TaskOwner`'s abort-on-drop reaches every one of them identically, and every one
    // fires a positive `Cancelled` event from its own guard's `Drop`. Zero silence anywhere in
    // this test now.
    //
    // RED-verified: temporarily moved `FillEventGuard::new` to AFTER the semaphore acquire
    // (simulating "a queued fill never constructs a guard, so a queued cancellation goes
    // unsignalled") -- failed, but not with the 5s timeout expected going in: only the 4
    // in-flight fills (already past the acquire) ever construct a guard, so once every fill task
    // is torn down the LAST `Arc<FillEvents>` clone drops with them, dropping `cancelled_tx`
    // itself -- the waiting receiver gets a `RecvError` ("Sender outlives this receiver") almost
    // immediately instead, an even faster and more specific failure than a timeout would have
    // been. Reverted immediately after confirming.
    #[tokio::test]
    async fn dropping_the_prefetcher_signals_cancelled_for_every_in_flight_and_queued_fill() {
        // softened, name and comment (p6-review2, pass 8 U-prefetch review, point 3): this test
        // runs on this crate's usual current_thread runtime, which the sibling below
        // (`no_queued_fill_starts_a_read_after_the_prefetcher_is_dropped`) proves is
        // structurally unable to expose the cross-task shutdown race U-prefetch's own fix
        // closes -- a `JoinSet`'s abort-all runs as one synchronous sequence with no yield point
        // for another task to observe a half-torn-down state in between, under cooperative
        // scheduling. This test's own former name claimed "cancels ... fills," which reads as
        // covering that ground; it never did, even before the fix existed. What it genuinely,
        // exclusively proves -- a single-task-observes-its-own-abort property, not a cross-task
        // race, so current_thread proves it just fine -- is that dropping the Prefetcher
        // signals Cancelled for every fill it was tracking, in-flight and queued alike, via
        // `fill_cancelled_events` below. For "no queued fill sneaks in an extra read during
        // shutdown" (the cross-thread property this test's own current_thread runtime cannot
        // touch), see the sibling instead.
        //
        // depth (8) exceeds FILL_CONCURRENCY (4): four fills acquire every
        // permit and start reading; the other four sit queued on the
        // semaphore, never having reached the source at all. A gate --
        // not a fixed latency (codex P2, PR #44 pass 7 re-review: a 50ms-latency read can
        // complete and free its permit on its own timeline, independent of when this test gets
        // around to reading `read_count()` below -- descheduled past that window, a fifth
        // queued fill starts and `mid` no longer reads exactly `FILL_CONCURRENCY`) -- parks the
        // four in-flight reads deterministically once armed: they cannot complete on their own,
        // so there is no window in which a fifth fill could start before the drop below lands.
        // Armed immediately, before `note_viewport` spawns anything -- unlike `status.rs`'s own
        // use of the same mechanism, there is no EARLIER, unrelated read here that needs to
        // proceed first.
        let src = Arc::new(MockSource::new(vec![b'x'; 4096]).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 64, 1 << 20));
        let p = Prefetcher::new(c.clone(), 8);
        // found in PR #44 pass 7's structural pass (codex P2, a 3rd re-review, the shared root of
        // 3 findings at once): the 4 fills queued on the semaphore are spawned but may not have
        // been POLLED even once by the time `drop(p)` below runs -- `FillEventGuard::new` (and so
        // this test's own `fill_cancelled` proof) only fires for a fill that has actually entered
        // its own async body, which a `JoinSet` aborting an unpolled future never reaches at all.
        // Waited for explicitly, all 8, before ever touching `started` or `mid` below --
        // synchronization, not an assumption that 8 freshly spawned tasks all got a turn by now.
        let mut entered = p.fill_entered_events();
        let mut started = src.started_events();
        p.note_viewport(Anchor::TOP);
        wait_for_count(&mut entered, |n| n >= 8).await;
        wait_for_count(&mut started, |n| n >= FILL_CONCURRENCY as u64).await;
        let mid = src.read_count();
        assert_eq!(
            mid, FILL_CONCURRENCY as u64,
            "expected exactly the semaphore's permits worth of fills to have started"
        );
        let mut fill_cancelled = p.fill_cancelled_events();
        drop(p);
        // POSITIVE proof, all 8: every fill this test spawned -- the 4 already reading AND the 4
        // still queued on the semaphore -- signals its own Cancelled event once TaskOwner's
        // abort-on-drop reaches it. No inference from silence anywhere in this assertion, and (as
        // of the `entered` wait above) no dependence on all 8 having been polled by luck either.
        wait_for_count(&mut fill_cancelled, |n| n >= 8).await;
        assert_eq!(
            src.read_count(),
            mid,
            "prefetch fills kept reading after the prefetcher was dropped"
        );
    }
    // found in PR #44 pass 8 (U-prefetch, finding 1): proves the shutdown fix (`Prefetcher`'s
    // own `Drop`, above) directly. Unlike the sibling test above, which runs on this crate's
    // usual current_thread runtime, this ONE MUST be multi-threaded -- discovered empirically,
    // not assumed. The race this fix closes (an active fill's aborted `SemaphorePermit`
    // releasing a permit a still-queued fill's own pending `acquire()` could then win) needs
    // REAL OS-level parallelism between "task A's permit releases" and "task B's acquire
    // observes it" to manifest at all: 50 current_thread runs of an equivalent shape, with the
    // fix temporarily disabled, never once failed -- cooperative single-thread scheduling
    // appears to never interleave the two operations the wrong way, so a current_thread version
    // of this test would be structurally unable to catch the bug it claims to guard against,
    // regardless of whether the fix is present. Only under `flavor = "multi_thread"` (8 worker
    // threads) AND genuine CPU contention (30 parallel process launches on a 20-core box) did
    // the unfixed code fail: 3 of those 30 runs observed extra reads (started_events landing at
    // 5, 7, and 8 against an expected 4, across the three failing runs) -- 1 to 4 of the four
    // queued fills sneaking an extra `cache.warm` call in during shutdown. With the fix
    // restored, 50 more runs of this exact test, same oversubscribed setup, were clean. Stated
    // as the actual numbers, not "a clean run proves it": the underlying race is scheduling-
    // dependent and probabilistic to reproduce, but the fix's own guarantee
    // (`Semaphore::close()` fails every pending-or-future `acquire()` unconditionally) does not
    // depend on winning any race, so a 0/50 failure rate after the fix is strong, not merely
    // lucky, evidence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn no_queued_fill_starts_a_read_after_the_prefetcher_is_dropped() {
        let src = Arc::new(MockSource::new(vec![b'x'; 4096]).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 64, 1 << 20));
        let p = Prefetcher::new(c.clone(), FILL_CONCURRENCY + 4);
        // waited for explicitly, all FILL_CONCURRENCY + 4 -- see the sibling test's own
        // identical comment for why (a queued fill's own `FillEventGuard` -- and so `entered` --
        // only fires once its future is actually polled, which a `JoinSet` aborting an unpolled
        // future never reaches).
        let mut entered = p.fill_entered_events();
        let mut started = src.started_events();
        p.note_viewport(Anchor::TOP);
        wait_for_count(&mut entered, |n| n >= (FILL_CONCURRENCY + 4) as u64).await;
        // positively confirms the split: exactly FILL_CONCURRENCY fills hold a permit and are
        // parked mid-read on the gate; the other four are genuinely queued on the semaphore,
        // never having reached the source at all.
        wait_for_count(&mut started, |n| n >= FILL_CONCURRENCY as u64).await;
        let mut fill_cancelled = p.fill_cancelled_events();
        drop(p);
        // post-merge batch 2 (2026-07-22), finding #4: POSITIVE proof, all FILL_CONCURRENCY + 4
        // -- every fill this test spawned signals its own Cancelled event once the abort
        // reaches it (the same wait the sibling test above already uses), and once all of them
        // have, no fill exists that could ever start a read, so the assertion below is final.
        // This retires the yield-margin this wait replaces (and its ACCEPTED-RESIDUAL marker,
        // whose "no positive signal exists" claim was itself a too-narrow inventory: the
        // signal was in use forty lines up).
        wait_for_count(&mut fill_cancelled, |n| n >= (FILL_CONCURRENCY + 4) as u64).await;
        assert_eq!(
            *started.borrow(),
            FILL_CONCURRENCY as u64,
            "a queued fill started a read after the prefetcher began shutting down"
        );
    }
    // found in PR #44 pass 8 fold re-review (codex P2, on 4e2400f): `abort_and_join` (above)
    // never closed the semaphore the way `Drop` does -- reopening, via a second code path, the
    // identical cross-task shutdown race the sibling test just above proves `Drop` closes. Same
    // mechanism, same doc comment, same multi_thread-with-real-contention requirement to have
    // any chance of observing it (current_thread's synchronous `abort_all()` sequence gives no
    // yield point for another task to observe a half-torn-down state in between) -- this test is
    // that sibling with `p.abort_and_join().await` in place of `drop(p)`, nothing else changed.
    //
    // Reachable in real bench usage, not a synthetic-only setup: `engine.rs`'s own `first_paint`
    // group runs every iteration against `Config::default()` (prefetch_depth 8, above
    // `FILL_CONCURRENCY`'s 4) on this crate's own `tokio::runtime::Runtime::new()` -- a
    // multi-threaded runtime by default -- and calls `abort_and_join` (via `Document::
    // abort_background_and_join`) on a `Document` it discards immediately after, every single
    // iteration. (`goto_line_cold_vs_indexed`'s own "pending" leg also calls `abort_and_join`
    // but never spawns a single fill in the first place -- that leg's own comment confirms
    // `viewport()` is the only call site reaching `note_viewport` -- so it cannot exercise this
    // race at all, unlike `first_paint`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn no_queued_fill_starts_a_read_after_abort_and_join() {
        let src = Arc::new(MockSource::new(vec![b'x'; 4096]).with_gate());
        src.arm_gate();
        let c = Arc::new(BlockCache::new(src.clone(), 64, 1 << 20));
        let p = Prefetcher::new(c.clone(), FILL_CONCURRENCY + 4);
        let mut entered = p.fill_entered_events();
        let mut started = src.started_events();
        p.note_viewport(Anchor::TOP);
        wait_for_count(&mut entered, |n| n >= (FILL_CONCURRENCY + 4) as u64).await;
        wait_for_count(&mut started, |n| n >= FILL_CONCURRENCY as u64).await;
        p.abort_and_join().await;
        // no manifestation margin here, deliberately (post-merge batch 2026-07-22, fix #7 --
        // the previously deferred P3): unlike the drop-based sibling above (fire-and-forget
        // `drop(p)`, which genuinely needs its window), `abort_and_join` is synchronous-complete
        // -- `TaskOwner::abort_all_and_join` has its own separately-tested guarantee that it
        // does not return until every task's teardown ran -- so the state asserted below is
        // final the moment it returns; a margin would only imply otherwise.
        assert_eq!(
            *started.borrow(),
            FILL_CONCURRENCY as u64,
            "a queued fill started a read after abort_and_join began shutting down"
        );
    }
}
