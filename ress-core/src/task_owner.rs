//! Cancel-on-drop ownership of one or more spawned tasks, shared by every
//! background-computation owner in this crate (`ScanScheduler`,
//! `StatusWorker`, `Prefetcher`) instead of each hand-rolling its own
//! `Drop`. A thin wrapper around `tokio::task::JoinSet`, deliberately with
//! NO `Drop` impl of its own: `JoinSet`'s own `Drop` already aborts every
//! task it holds -- unlike `tokio::task::JoinHandle`'s own `Drop`, which
//! only DETACHES a task (leaves it running standalone), never aborts it.
//! That distinction is exactly why `ScanScheduler` and `StatusWorker` each
//! used to need their own hand-written `Drop { self.task.abort() }` at
//! all, and exactly why converting both onto this wrapper removes that
//! hand-written call rather than relocating it: the OWNERSHIP TYPE now
//! provides the guarantee structurally, the same way `Prefetcher` already
//! got it for free from a bare `JoinSet`. "Does dropping this really abort
//! what it owns" stops being a property re-derived per owner and becomes
//! one this module's own test proves once (see `tests`, below).
use tokio::task::JoinSet;

pub(crate) struct TaskOwner<T = ()> {
    tasks: JoinSet<T>,
}
impl<T: 'static> Default for TaskOwner<T> {
    fn default() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }
}
impl<T: Send + 'static> TaskOwner<T> {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    /// Spawns `fut` as a tracked task; dropping this owner aborts it (and
    /// everything else it still holds) at whichever `.await` it is then
    /// suspended at, via `JoinSet`'s own `Drop` -- see this module's own
    /// doc comment for why that is a structural guarantee, not a call this
    /// type makes itself.
    pub(crate) fn spawn(
        &mut self,
        fut: impl std::future::Future<Output = T> + Send + 'static,
    ) -> tokio::task::AbortHandle {
        self.tasks.spawn(fut)
    }
    /// Drops every already-finished handle this owner is still holding, so
    /// a caller polling `len()` afterward sees only tasks still running or
    /// still queued -- `JoinSet` only reaps a completed handle when polled,
    /// so without this the count would grow forever instead of reflecting
    /// genuine backlog (`Prefetcher`'s own pre-`TaskOwner` reap loop, moved
    /// here so every owner shares it rather than each writing its own).
    pub(crate) fn reap_finished(&mut self) {
        while self.tasks.try_join_next().is_some() {}
    }
    /// How many tasks this owner is currently tracking (running, queued, or
    /// finished but not yet reaped -- call `reap_finished` first for a
    /// backlog-only count).
    pub(crate) fn len(&self) -> usize {
        self.tasks.len()
    }
    /// Awaits every task this owner currently holds to completion;
    /// test-only (production never wants to block on background work
    /// finishing -- `Prefetcher::settle`'s own pre-`TaskOwner` shape).
    #[cfg(test)]
    pub(crate) async fn join_all(&mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
    /// Aborts every task this owner currently holds, THEN awaits their own teardown to
    /// finish -- unlike `join_all` above (natural completion, no abort at all), this
    /// forces every tracked task to stop first, and waits only for that stop to actually
    /// take effect, however long that takes (an aborted task's own `JoinError::is_
    /// cancelled` still has to travel through `join_next` like any other outcome; a
    /// dropped `JoinSet` does not wait for that, `abort_all` alone does not either).
    /// Bench-visible (test + bench-internals, matching `Document::new_unindexed`'s own
    /// precedent -- a `[[bench]]` target is a separate crate that cannot see
    /// crate-private items regardless of cfg): a criterion bench that builds a fresh
    /// owner-holding value every iteration needs this to guarantee the PREVIOUS
    /// iteration's own still-unwinding abort never shares runtime worker threads with
    /// the NEXT iteration's own timed work -- see `engine.rs`'s own `first_paint` group.
    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) async fn abort_all_and_join(&mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // signals `false` (cancelled) via its own `Drop` unless `complete()` was called first --
    // deliberately local to this module's own tests, not shared with `source.rs`'s structurally
    // similar `BlockEventGuard`: task ownership must not depend on the block-source file (a
    // p6-review2 layering finding, pass 7 delta review), and this is this type's only user, so a
    // shared abstraction would cost a dependency for no reuse actually realized.
    //
    // found in PR #44 pass 7's structural pass (codex P2, a 3rd re-review, the shared root of 3
    // findings at once): `new` also fires `entered_tx` immediately, before returning -- a
    // positive signal that this spawned task has actually been polled at least once (entered its
    // own async body far enough to construct this guard), fired BEFORE the caller's own
    // `pending()`/whatever it awaits next, so a test can wait for it explicitly rather than
    // trusting a single `yield_now` to have given the scheduler a turn (DETERMINISTICALLY
    // disproven: with zero yields between spawn and drop, `drop` reliably races ahead of the
    // first poll -- tokio's own `spawn` never polls synchronously -- so `yield_now`'s own
    // "probably enough of a chance" was never a guarantee, only usually enough of one).
    struct SignalOnDrop {
        tx: Option<tokio::sync::oneshot::Sender<bool>>,
    }
    impl SignalOnDrop {
        fn new(
            entered_tx: tokio::sync::oneshot::Sender<()>,
            tx: tokio::sync::oneshot::Sender<bool>,
        ) -> Self {
            let _ = entered_tx.send(());
            Self { tx: Some(tx) }
        }
        fn complete(&mut self) {
            // consumed, not merely flagged: a second `Drop` (there is only ever one, but the
            // shape makes "already resolved" structurally inexpressible as a race) can never
            // send twice on an already-consumed oneshot.
            self.tx.take();
        }
    }
    impl Drop for SignalOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(true); // reached Drop with no `complete()` call -- cancelled.
            }
        }
    }

    // the cancellation probe (p6-review2's own bullet 8): proves, once and generically -- no
    // BlockSource, no cache, no real IO anywhere -- that dropping a `TaskOwner` genuinely aborts
    // a task it still holds, not merely detaches it. Deliberately does NOT use
    // `JoinError::is_cancelled` (the original design's own proposal): once `owner` is dropped,
    // its `JoinSet` is gone with it, and nothing is left to `join_next()` a `JoinError` FROM --
    // there is no handle left capable of reporting one. `SignalOnDrop` proves the same underlying
    // fact a different way: the spawned task's own `Drop` glue genuinely ran while it was still
    // suspended (never having reached its own `complete()` call), which is only possible if it
    // was torn down mid-flight, not left running or allowed to finish.
    //
    // RED-verified (both halves): (1) the original detach-not-own bug -- temporarily changed
    // `spawn` to bypass `self.tasks` entirely (a raw `tokio::spawn(fut)`, returning that
    // independent task's own `abort_handle()` just to satisfy the return type) -- failed at its
    // timeout exactly as predicted, confirming this genuinely exercises the wrapper's own wiring.
    // (2) the entered-handshake's own necessity, pass 7's structural pass (codex P2, a 3rd
    // re-review): with the `entered_rx.await` below removed and zero yields put in its place,
    // `drop(owner)` deterministically (not scheduler-luck -- tokio's own `spawn` never polls
    // synchronously) races ahead of the spawned task's first poll, so `SignalOnDrop::new` never
    // runs and `cancelled` times out -- proving a single `yield_now` (this test's own pre-fix
    // shape) was never a real guarantee, only usually enough of one. Both reverted immediately
    // after confirming.
    #[tokio::test]
    async fn dropping_the_owner_aborts_a_task_still_suspended_inside_it() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let mut owner = TaskOwner::<()>::new();
        owner.spawn(async move {
            let mut guard = SignalOnDrop::new(entered_tx, tx);
            std::future::pending::<()>().await; // never resolves on its own.
            guard.complete(); // unreached except by a bug that lets this future run to term.
        });
        // waits for a POSITIVE signal that the spawned task has actually been polled at least
        // once and constructed its own guard -- not a `yield_now` and a hope. See this test's own
        // RED-verification note above.
        entered_rx
            .await
            .expect("the spawned task must construct its guard before dropping the owner");
        drop(owner);
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        assert!(
            cancelled.is_ok_and(|r| r == Ok(true)),
            "dropping the owner must abort its task, not merely detach it"
        );
    }

    // the sibling positive case: a task that runs to its own natural completion before the owner
    // ever drops must NOT report cancelled -- otherwise the probe above would just be pinning
    // "SignalOnDrop always fires," not "fires specifically because of an abort."
    #[tokio::test]
    async fn a_task_that_finishes_on_its_own_is_not_reported_cancelled() {
        // this test never races entry at all (`join_all` below waits for the task to run to
        // completion before the owner ever drops), so the entered signal is unused here --
        // discarded rather than named `_entered_rx`, since an unused `Receiver` still needs a
        // live `Sender` bound to something, not dropped immediately (which would make `send`
        // inside `SignalOnDrop::new` itself observably fail, harmlessly here, but needlessly
        // divergent from every other constructor call in this module's own tests).
        let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let mut owner = TaskOwner::<()>::new();
        owner.spawn(async move {
            let mut guard = SignalOnDrop::new(entered_tx, tx);
            guard.complete();
        });
        owner.join_all().await;
        drop(owner);
        assert!(
            rx.await.is_err(),
            "a task that completed on its own must never signal cancelled"
        );
    }

    // `abort_all_and_join`'s own discriminating property (U-bench, finding 4), distinct from
    // both siblings above: `join_all` waits for NATURAL completion (no abort at all), and a bare
    // `drop` aborts but is fire-and-forget (needs its own 5s timeout above specifically because
    // Drop never waits for the abort to take effect). This checks, with NO wait or timeout at
    // all, that the aborted task's own Drop glue has ALREADY run by the instant
    // `abort_all_and_join().await` itself returns -- exactly the property a criterion bench
    // needs (see `Document::abort_background_and_join`'s own doc comment) and exactly what would
    // fail if this were implemented as `abort_all()` alone, with no join loop after it.
    //
    // RED-verified: temporarily stripped the join loop from `abort_all_and_join` down to just
    // `self.tasks.abort_all()` -- this test's immediate, non-blocking `try_recv()` failed as
    // predicted (`Empty`, not `Ok(true)`): the signal had not fired yet at the instant the call
    // returned, confirming the join loop is what this test actually exercises, not the abort
    // alone. Reverted after confirming.
    #[tokio::test]
    async fn abort_all_and_join_does_not_return_until_teardown_has_actually_run() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<bool>();
        let mut owner = TaskOwner::<()>::new();
        owner.spawn(async move {
            let mut guard = SignalOnDrop::new(entered_tx, tx);
            std::future::pending::<()>().await; // never resolves on its own.
            guard.complete(); // unreached except by a bug that lets this future run to term.
        });
        // see this module's other tests' identical wait, above: a positive signal that the task
        // was actually polled at least once, not a `yield_now` and a hope.
        entered_rx
            .await
            .expect("the spawned task must construct its guard before aborting it");
        owner.abort_all_and_join().await;
        assert_eq!(
            rx.try_recv(),
            Ok(true),
            "abort_all_and_join must not return until the aborted task's own Drop has already run"
        );
    }
}
