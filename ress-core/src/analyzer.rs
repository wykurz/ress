//! The background-analysis driver: `StatusWorker`'s own `run` loop (`crate::status`),
//! generalized into a shape any bounded, resumable background computation can reuse instead of
//! hand-rolling its own copy of the same supersession/retry/yield plumbing. `spawn` owns the
//! whole lifecycle -- pulling the latest request, resetting on a new one (`Analysis::begin`),
//! stepping bounded units of work (`Analysis::step`) with a courtesy yield between them, racing
//! every await against a fresh request via `select!` (supersession is control flow a fresh
//! request short-circuits straight back to the top, never a shared flag two call sites could
//! each half-update), and retrying a failing unit with backoff before giving up on it
//! (`Analysis::give_up`). An `Analysis` implementation supplies only the WORK; it never
//! reimplements the loop that drives it.
//!
//! `StatusWorker`/`ScanScheduler` retrofitting onto this driver is explicitly OUT of scope for
//! this task -- both keep their own hand-written loops for now. One difference from
//! `StatusWorker::run`'s own loop, which this generalizes: `give_up` SKIPS the failing unit and
//! the analysis KEEPS GOING -- after it runs, the driver `continue`s its inner loop and calls
//! `step` again immediately, rather than parking until a new request the way `StatusWorker` does
//! on retry exhaustion. A bounded, best-effort summary degrades to a FLOOR instead of going
//! wholesale `Unavailable`. What "skip" means in practice (advance state past the unit, record
//! the damage) is entirely the `Analysis`'s own call, not this driver's -- it only decides WHEN
//! retries are exhausted, never how to recover.
//!
//! Retry accounting matches `StatusWorker::run`'s own `scanned() > before` rule exactly, not a
//! coarser approximation of it (batch 3 (2026-07-23), finding #5, closing a gap an earlier
//! version of this comment used to describe as a second, deliberate difference): `Analysis::
//! progress_marker` is the generic hook a driver with no `Analysis`-specific field like
//! `scanned()` needs to ask the identical question. Sampled immediately before and after every
//! `step` call, a value that moved resets `consecutive_failures` -- `Err` outcomes included,
//! since a step can leave real partial progress behind even on the call that ultimately fails
//! (see `spawn`'s own comment at the reset for why that failure is still only the FIRST strike
//! at the new position, not another one piled onto the old).
//!
//! `ANALYSIS_RETRIES` and the backoff schedule (`50 * consecutive_failures` ms) match
//! `StatusWorker`'s own `STATUS_RETRIES`/schedule in value, not by sharing a definition --
//! independent constants kept at the same number by convention.
/// One background analysis: reset on a new request, step bounded units, publish snapshots. The
/// driver owns supersession, retry/backoff, and yielding -- an analysis implementation contains
/// NO copy of that loop.
pub(crate) trait Analysis: Send + 'static {
    type Request: Clone + Send + Sync + 'static;
    type Snapshot: Clone + Send + Sync + 'static;
    /// Resets state for `req`; returns the initial snapshot to publish.
    fn begin(&mut self, req: &Self::Request) -> Self::Snapshot;
    /// One bounded unit of work, publishing through `tx` as it goes.
    /// `Ok(true)` = the whole analysis is finished; `Ok(false)` = call again;
    /// `Err` = this unit failed (the driver retries with backoff).
    fn step(
        &mut self,
        tx: &tokio::sync::watch::Sender<Self::Snapshot>,
    ) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send;
    /// A cheap, monotonically non-decreasing measure of how far the analysis has advanced --
    /// `SweepAnalysis` returns its own `pos`. The driver samples this immediately before and
    /// after every `step` call and resets `consecutive_failures` when it moved, `Err` included
    /// (batch 3 (2026-07-23), finding #5; mirrors `StatusWorker`'s own `scanned() > before`
    /// rule -- see this module's own doc comment). Must be cheap: called on every single step,
    /// not only on failures.
    ///
    /// By design this puts NO ceiling on the analysis's total failure count, only on
    /// consecutive failures at one stuck position -- `spread_out_failures_with_progress_
    /// between_never_give_up` (this module's own test) pins exactly that: `give_up` never
    /// fires as long as progress keeps resetting the counter first. An `Analysis` impl whose
    /// own `progress_marker` could be nudged forward forever without the underlying work ever
    /// finishing must therefore guarantee its own termination; this driver does not do it for
    /// them. `SweepAnalysis` does: `progress_marker` only ever advances at report/window
    /// granularity, so a free reset costs at most one window's worth of stalling, and
    /// `give_up` itself always advances `pos` by at least one window -- together bounding the
    /// total retry cost across a whole file.
    fn progress_marker(&self) -> u64;
    /// Retries exhausted on the current unit: record the damage in the snapshot and skip past
    /// the unit (the analysis keeps going).
    fn give_up(&mut self, tx: &tokio::sync::watch::Sender<Self::Snapshot>);
}

/// How many consecutive failed units a step retries before giving up on the current unit and
/// moving on -- see `Analysis::give_up`. Matches `crate::status::STATUS_RETRIES`'s own value (an
/// independent constant, not a shared definition; see this module's own doc comment).
pub(crate) const ANALYSIS_RETRIES: u8 = 3;

/// Spawns the driver for `analysis`, owned cancel-on-drop (see `crate::task_owner`'s own doc
/// comment): dropping the returned `TaskOwner` aborts the loop below at whichever `.await` it is
/// then suspended at, the same guarantee `StatusWorker`/`ScanScheduler` already get from the
/// identical mechanism.
pub(crate) fn spawn<A: Analysis>(
    mut analysis: A,
    mut request_rx: tokio::sync::watch::Receiver<Option<A::Request>>,
    snapshot_tx: tokio::sync::watch::Sender<A::Snapshot>,
) -> crate::task_owner::TaskOwner<()> {
    let mut tasks = crate::task_owner::TaskOwner::new();
    tasks.spawn(async move {
        'outer: loop {
            // bound to its own statement, not matched on directly: `borrow_and_update()`
            // returns a `watch::Ref` guard, and a `match` scrutinee temporary's own scope
            // would otherwise extend across every arm -- including the `None` arm's own
            // `.await` below -- making the whole async block non-`Send` (the guard itself
            // isn't `Send`) even though `.clone()` immediately produces an owned value.
            let current = request_rx.borrow_and_update().clone();
            let req = match current {
                Some(r) => r,
                None => {
                    if request_rx.changed().await.is_err() {
                        return;
                    }
                    continue 'outer;
                }
            };
            let snap = analysis.begin(&req);
            let _ = snapshot_tx.send(snap);
            let mut consecutive_failures: u8 = 0;
            loop {
                // one step can leave real partial progress behind even on the call that
                // ultimately fails (`SweepAnalysis` reports incrementally, block by block,
                // before ever reaching the byte that errors) -- sampled here, before `step`
                // runs, so the `Err` arm below can tell a failure that follows genuine progress
                // from one that doesn't (batch 3 (2026-07-23), finding #5; mirrors status.rs's
                // own `before = scan.scanned()`).
                let before = analysis.progress_marker();
                tokio::select! {
                    changed = request_rx.changed() => {
                        if changed.is_err() { return; }
                        continue 'outer;
                    }
                    step = analysis.step(&snapshot_tx) => match step {
                        Ok(true) => {
                            if request_rx.changed().await.is_err() { return; }
                            continue 'outer;
                        }
                        Ok(false) => {
                            consecutive_failures = 0;
                            tokio::task::yield_now().await;
                        }
                        Err(e) => {
                            // a failure that follows real progress is the FIRST strike at the
                            // new position, not another one piled onto the old -- four
                            // unrelated one-shot failures on four different units must never
                            // accumulate toward give_up the way four consecutive failures on
                            // the SAME unit must (batch 3 (2026-07-23), finding #5).
                            consecutive_failures = if analysis.progress_marker() > before {
                                1
                            } else {
                                consecutive_failures + 1
                            };
                            tracing::debug!("analysis step failed: {e:#}");
                            if consecutive_failures > ANALYSIS_RETRIES {
                                analysis.give_up(&snapshot_tx);
                                consecutive_failures = 0;
                                continue;
                            }
                            let backoff = std::time::Duration::from_millis(
                                50 * consecutive_failures as u64,
                            );
                            tokio::select! {
                                changed = request_rx.changed() => {
                                    if changed.is_err() { return; }
                                    continue 'outer;
                                }
                                _ = tokio::time::sleep(backoff) => {}
                            }
                        }
                    }
                }
            }
        }
    });
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::wait_for_count;
    use std::collections::VecDeque;
    use tokio::sync::watch;

    /// Exercises the driver in isolation -- no cache, no regex, no IO anywhere: `step` pops one
    /// scripted `(progress, outcome)` pair per call from a fixed queue, hanging forever (a
    /// genuine, permanently suspended await point via `std::future::pending`, never a race the
    /// driver could "beat" no matter how the scheduler happens to run it) once the queue runs
    /// dry. A test parks the driver mid-analysis on demand simply by scripting fewer outcomes
    /// than it plans to trigger. `progress` is added to a running total (`progress_marker`'s own
    /// return value) BEFORE `outcome` is evaluated, whether `outcome` is `Ok` or `Err` --
    /// modeling `SweepAnalysis`'s own real shape, where a step can leave genuine partial
    /// progress behind even on the call that ultimately fails (batch 3 (2026-07-23), finding
    /// #5); a script entry with `progress: 0` models a step that makes none, e.g. a retry that
    /// fails again immediately with nothing new read first. `begin`/`step`/`give_up` each
    /// publish their own call count on a dedicated `watch` counter -- a positive signal a test
    /// can `wait_for_count` on (`crate::source::wait_for_count`, this crate's own event-driven
    /// testing idiom), never a sleep-and-hope.
    struct ScriptedAnalysis {
        script: VecDeque<(u64, anyhow::Result<bool>)>,
        progress: u64,
        begin_count: u64,
        begin_tx: watch::Sender<u64>,
        step_count: u64,
        step_tx: watch::Sender<u64>,
        give_up_count: u64,
        give_up_tx: watch::Sender<u64>,
    }
    /// The test-facing half of `ScriptedAnalysis`'s counters -- split out so the analysis itself
    /// (moved into the spawned task) and the counters (kept in the test) can each be owned
    /// separately, the same shape `MockSource`'s own `started_events`/`cancelled_events` give a
    /// test over state that otherwise lives inside a moved-away value.
    struct ScriptedCounters {
        begin: watch::Receiver<u64>,
        step: watch::Receiver<u64>,
        give_up: watch::Receiver<u64>,
    }
    impl ScriptedAnalysis {
        fn new(script: VecDeque<(u64, anyhow::Result<bool>)>) -> (Self, ScriptedCounters) {
            let (begin_tx, begin) = watch::channel(0);
            let (step_tx, step) = watch::channel(0);
            let (give_up_tx, give_up) = watch::channel(0);
            (
                Self {
                    script,
                    progress: 0,
                    begin_count: 0,
                    begin_tx,
                    step_count: 0,
                    step_tx,
                    give_up_count: 0,
                    give_up_tx,
                },
                ScriptedCounters {
                    begin,
                    step,
                    give_up,
                },
            )
        }
    }
    impl Analysis for ScriptedAnalysis {
        // plain counters/IDs are all these tests need; the driver itself is generic over both,
        // so nothing about its own behavior depends on richer types.
        type Request = u64;
        type Snapshot = ();
        fn begin(&mut self, _req: &u64) {
            self.progress = 0;
            self.begin_count += 1;
            let _ = self.begin_tx.send(self.begin_count);
        }
        async fn step(&mut self, _tx: &watch::Sender<()>) -> anyhow::Result<bool> {
            self.step_count += 1;
            let _ = self.step_tx.send(self.step_count);
            let Some((progress, outcome)) = self.script.pop_front() else {
                // the script ran dry: hang forever rather than panic or invent an outcome --
                // see this struct's own doc comment for why a test wants exactly this.
                std::future::pending::<()>().await;
                unreachable!("a pending future does not resolve on its own");
            };
            self.progress += progress;
            outcome
        }
        fn progress_marker(&self) -> u64 {
            self.progress
        }
        fn give_up(&mut self, _tx: &watch::Sender<()>) {
            self.give_up_count += 1;
            let _ = self.give_up_tx.send(self.give_up_count);
        }
    }

    #[tokio::test]
    async fn a_new_request_supersedes_mid_analysis() {
        // an empty script: the very first step() call (right after begin) has nothing to pop
        // and hangs forever -- exactly the "parked mid-analysis" state this test needs, with no
        // scripting required to arrange it.
        let (analysis, mut counters) = ScriptedAnalysis::new(VecDeque::new());
        let (request_tx, request_rx) = watch::channel(None);
        let (snapshot_tx, _snapshot_rx) = watch::channel(());
        let _owner = spawn(analysis, request_rx, snapshot_tx);
        request_tx.send(Some(1)).unwrap();
        // a positive proof the driver is genuinely parked INSIDE the hanging step() call, not
        // merely past begin -- otherwise a fast test could send request 2 before the driver
        // ever entered step() at all, which would prove only ordinary startup behavior, not
        // mid-analysis abandonment.
        wait_for_count(&mut counters.begin, |n| n >= 1).await;
        wait_for_count(&mut counters.step, |n| n >= 1).await;
        request_tx.send(Some(2)).unwrap();
        // the discriminating assertion: begin() called a SECOND time can only happen by the
        // driver's `select!` abandoning the still-pending step() future in favor of the fresh
        // request -- the script is empty, so that step() call structurally cannot have returned
        // Ok(true) on its own to explain a second begin() any other way.
        wait_for_count(&mut counters.begin, |n| n >= 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn retries_back_off_then_give_up_moves_on() {
        // 1 + ANALYSIS_RETRIES consecutive failures exhausts the retry budget on the very first
        // unit (see the driver's own `consecutive_failures > ANALYSIS_RETRIES` check); the
        // Ok(true) right after proves give_up did not stop the analysis from continuing. Every
        // entry scripts zero progress -- these failures are all on the SAME stuck unit, the
        // mirror image of `spread_out_failures_with_progress_between_never_give_up`, below --
        // so `consecutive_failures` must accumulate across every one of them, unlike there.
        let mut script = VecDeque::new();
        for _ in 0..=ANALYSIS_RETRIES {
            script.push_back((0, Err(anyhow::anyhow!("boom"))));
        }
        script.push_back((0, Ok(true)));
        let total_steps = script.len() as u64;
        let (analysis, mut counters) = ScriptedAnalysis::new(script);
        let (request_tx, request_rx) = watch::channel(None);
        let (snapshot_tx, _snapshot_rx) = watch::channel(());
        let _owner = spawn(analysis, request_rx, snapshot_tx);
        request_tx.send(Some(1)).unwrap();
        // `start_paused = true` gives this test tokio's own virtual clock: with nothing else
        // runnable, the runtime auto-advances straight through each of the driver's own
        // successive backoff sleeps the moment this test parks on a `wait_for_count` -- tokio's
        // own documented, RELIABLE mechanism for a CHAIN of dependently-scheduled timers (each
        // retry's own sleep is only created once the PREVIOUS one fires and the retry runs).
        // Deliberately not a manual `tokio::time::advance()` loop instead: that function's own
        // doc comment explicitly warns it does not guarantee processing a timer scheduled AS A
        // REACTION to an earlier one within the same jump -- exactly this shape. No real time
        // elapses either way; `wait_for_count`'s own bound (`DIAGNOSTIC_CEILING`) is itself
        // virtual-time here, so a genuine hang still fails fast, not after 5 real seconds.
        wait_for_count(&mut counters.give_up, |n| n >= 1).await;
        wait_for_count(&mut counters.step, |n| n >= total_steps).await;
        assert_eq!(
            *counters.give_up.borrow(),
            1,
            "give_up must fire exactly once, not once per retry"
        );
        assert_eq!(
            *counters.step.borrow(),
            total_steps,
            "every scripted outcome must be consumed exactly once: {} failures then Ok(true)",
            1 + ANALYSIS_RETRIES
        );
    }

    #[tokio::test(start_paused = true)]
    async fn spread_out_failures_with_progress_between_never_give_up() {
        // batch 3 (2026-07-23), finding #5a: four UNRELATED one-shot failures, each on a fresh
        // position reached only by real progress since the last one -- e.g. four different
        // blocks each failing once, never retried -- must never accumulate toward give_up the
        // way `retries_back_off_then_give_up_moves_on`'s four CONSECUTIVE failures on the SAME
        // unit do. Each entry here scripts nonzero progress before its own `Err`, modeling
        // `SweepAnalysis` reading (and reporting) real bytes before the read that fails --
        // "unrelated" *because* every failure follows fresh progress, not despite it.
        let mut script = VecDeque::new();
        for _ in 0..8 {
            script.push_back((10, Err(anyhow::anyhow!("boom"))));
        }
        script.push_back((10, Ok(true)));
        let total_steps = script.len() as u64;
        let (analysis, mut counters) = ScriptedAnalysis::new(script);
        let (request_tx, request_rx) = watch::channel(None);
        let (snapshot_tx, _snapshot_rx) = watch::channel(());
        let _owner = spawn(analysis, request_rx, snapshot_tx);
        request_tx.send(Some(1)).unwrap();
        // more failures than ANALYSIS_RETRIES + 1 (8 vs. 4) -- if the driver were still
        // accumulating them regardless of progress, give_up would already have fired well
        // before every scripted outcome is consumed. `start_paused = true` auto-advances
        // through every backoff sleep in between, exactly as the sibling test above does.
        wait_for_count(&mut counters.step, |n| n >= total_steps).await;
        assert_eq!(
            *counters.give_up.borrow(),
            0,
            "progress ahead of each failure must keep resetting the retry budget, not \
             accumulate it toward give_up"
        );
    }

    #[tokio::test]
    async fn none_request_idles_without_stepping() {
        let (analysis, mut counters) = ScriptedAnalysis::new(VecDeque::from([(0, Ok(true))]));
        let (request_tx, request_rx) = watch::channel(None);
        let (snapshot_tx, _snapshot_rx) = watch::channel(());
        let _owner = spawn(analysis, request_rx, snapshot_tx);
        // ACCEPTED-RESIDUAL: a bounded absence window (AGENTS.md closed-campaign policy,
        // 2026-07-22 -- see e.g. `status.rs`'s own
        // `stays_converging_while_the_scan_is_alive_and_uncovered`) -- there is no positive
        // "will never step" signal to wait for while the request stays `None`, so this can only
        // give a hypothetically-broken driver (one that steps regardless of the request) a few
        // cooperative yields' worth of genuine chance to do so before checking. The request
        // round trip below is the primary proof, not this window -- see its own comment.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *counters.begin.borrow(),
            0,
            "a None request must never call begin"
        );
        assert_eq!(
            *counters.step.borrow(),
            0,
            "a None request must never call step"
        );
        // the positive half: a request round trip through the SAME channel proves the driver's
        // own task was alive, scheduled, and reachable the whole time above -- not stuck or
        // panicked for some unrelated reason -- and the ORDERING (step only ever observed AFTER
        // this send) is what proves it was genuinely parked on the request channel the whole
        // time, not merely lucky so far.
        request_tx.send(Some(1)).unwrap();
        wait_for_count(&mut counters.begin, |n| n >= 1).await;
        wait_for_count(&mut counters.step, |n| n >= 1).await;
    }
}
