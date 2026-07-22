//! The byte-reading seam: `BlockSource` abstracts the file behind the cache,
//! so a future io_uring backend is a drop-in replacement for `PreadSource`.
use bytes::Bytes;
/// Reads raw bytes from a fixed, seekable source.
#[async_trait::async_trait]
pub trait BlockSource: Send + Sync {
    /// Total size of the source in bytes.
    fn size(&self) -> u64;
    /// Reads up to `len` bytes starting at `offset`; returns fewer bytes near
    /// EOF and an empty buffer when `offset >= size`.
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes>;
}

use std::sync::atomic::{AtomicU64, Ordering};

// found in PR #44 pass 8 (a user/codex re-review, the timing-oracle class recurring a 3rd time):
// the shared "diagnostic ceiling, not a coordination oracle" bound this crate's event-wait
// idioms need -- the gate's own built-in safety backstop (see `arm_gate`'s doc comment for
// the full reasoning), `wait_for_count`'s own wrapping timeout (below), and, `pub(crate)`
// (U-guard sweep, pass 8), `cache.rs`'s own hand-rolled `FailingSlow` gate. Generous enough that
// no legitimate wait, even on a loaded CI machine, could ever brush against it; exists purely to
// convert "this should have arrived by now but didn't" from a silent, infinite hang into a loud,
// attributable panic. GENUINELY one constant for all of them (p6-review2, pass 8, U1 amendment:
// an earlier version of this comment claimed the first two already shared one value while
// `wait_for_count` still carried its own independent `Duration::from_secs(5)` literal -- true in
// effect, not in the code, and nothing kept them from drifting apart; this is what actually keeps
// every one of them the same value going forward -- `cache.rs:332`'s own, unrelated, pre-pass-8
// `Duration::from_secs(5)` literal is a deliberately separate case, not folded in here).
pub(crate) const DIAGNOSTIC_CEILING: std::time::Duration = std::time::Duration::from_secs(5);

/// An in-memory `BlockSource` for tests: counts reads and can hold one deterministically
/// gated. Latency injection (a fixed per-read sleep) was deleted (U-delete, pass 8): tests kept
/// reaching for it as a coordination shortcut -- race a second concurrent thing against a fixed
/// sleep -- an undocumented-by-CONTRACT reliance on "N milliseconds is definitely enough,"
/// exactly the class of footgun the armable gate below exists to close structurally instead. A
/// bench that genuinely needs to MEASURE latency (not coordinate against it) keeps its own,
/// bench-local latency wrapper -- see `ress-core/benches/engine.rs`'s own `LatencySource`.
///
/// found in PR #44 pass 7 (P7-C, a p6-review2 finding, "replace latency-and-silence
/// cancellation tests"): publishes three positive, `watch`-based events per `read_block` call --
/// `started_events` (this call has entered `read_block`, before the gate's own park check, if
/// armed -- the original, round-16 mechanism), and now `cancelled_events`/`released_events`
/// (this call's own future has since been dropped -- `Cancelled` if that happened before it
/// ever reached its own return, `Released` if it ran to completion, Ok or Err either way). A
/// future torn down by an abort mid-gate-park signals `Cancelled` the moment its own `Drop`
/// runs; a future left to finish signals `Released` instead -- exactly one of the two, always,
/// for every read that ever started, via the scopeguard `BlockEventGuard` below. This is what
/// lets a cancellation test assert a POSITIVE "this specific in-flight read's own future was
/// torn down," rather than inferring it from a bounded wait for silence (nothing further
/// happening within some window).
pub struct MockSource {
    data: Bytes,
    reads: AtomicU64,
    cancelled: AtomicU64,
    released: AtomicU64,
    // found in PR #44 round 16 (a codex P2): a `watch` of how many `read_block` calls have
    // STARTED (before the gate's own park check, if armed) -- lets a test AWAIT "N reads have entered
    // read_block" as an explicit event via `started_events()`'s own receiver, rather than
    // sleeping a guessed duration and hoping enough real time passed for N reads to have
    // started (a correct implementation can fail that race under a loaded/parallel executor --
    // exactly this crate's own "tests prove events, not scheduler timing" rule, AGENTS.md).
    // `watch`, not `Notify`: the existing count needs to be OBSERVABLE by a receiver that
    // subscribes AFTER some reads have already started (a `Notify` only wakes tasks already
    // waiting at the moment of the call, missing anything that happened earlier) -- the same
    // "publish state, let observers await or poll it" idiom this workspace's own concurrency
    // model already uses elsewhere (docs/concurrency.md). `cancelled`/`released` (pass 7, P7-C)
    // share the identical shape, one channel per event kind, for the same reason.
    started_tx: tokio::sync::watch::Sender<u64>,
    cancelled_tx: tokio::sync::watch::Sender<u64>,
    released_tx: tokio::sync::watch::Sender<u64>,
    // post-merge batch (2026-07-22), fix #6: the gate's own safety-bound panic used to unwind
    // through `BlockEventGuard::drop`'s two-way completed/cancelled split and emit `Cancelled`
    // -- the exact positive event ownership tests accept as proof of a torn-down future. Both
    // that bound and `wait_for_count`'s ceiling default to the same `DIAGNOSTIC_CEILING`, and
    // the gate's timer starts first (at read entry), so a LEAKED task panicking at the bound
    // could satisfy a cancellation wait that should have failed. A third, distinct event keeps
    // the diagnostic backstop from counterfeiting the proof.
    gate_timeouts: AtomicU64,
    gate_timeout_tx: tokio::sync::watch::Sender<u64>,
    // found in PR #44 pass 7 re-review (codex P2, "dropping_the_prefetcher_cancels_in_flight_
    // and_queued_fills" flake), extended in pass 7's structural pass (a 3rd re-review, the same
    // class recurring): `None` (the default) means `read_block` never parks -- every other
    // `MockSource` user is unaffected. `with_gate` installs the mechanism, but DISARMED --
    // `gate_armed` (below) starts `false`, so every read still proceeds exactly as if no gate
    // existed at all until `arm_gate` is called. Split into install-then-arm, rather than armed
    // from construction (this field's own pass-7-P7-C original shape), because a test can need
    // EARLIER, unrelated reads (a background index scan sharing the same cache/source) to
    // complete normally before the ONE read it actually wants to gate -- arming is a separate,
    // explicit, `&self` step a test takes whenever it chooses, not tied to construction.
    // Once armed, every call parks (past its own `started` event, before returning data) until
    // `open_gate` releases it -- a deterministic barrier for a read that must sit genuinely,
    // provably in-flight, not merely likely still-running after a fixed latency sleep a
    // slow/loaded test runner could race past (a latency-based read completes and frees whatever
    // it holds on its own timeline, independent of when the test gets around to observing it).
    // `watch`, not `Notify`, for the identical reason `started_tx`/`cancelled_tx`/`released_tx`
    // above are `watch`: a read that calls `read_block` AFTER `open_gate` already ran must see
    // the gate already open, not miss a wakeup that already happened (`Notify::notify_waiters`
    // only reaches waiters already parked at that instant).
    gate_tx: Option<tokio::sync::watch::Sender<bool>>,
    gate_armed: std::sync::atomic::AtomicBool,
    // found in PR #44 pass 8: the bound `read_block`'s own gate-wait is timed out against (see
    // `arm_gate`'s doc comment). Defaults to `DIAGNOSTIC_CEILING`; overridable, test-only, via
    // `with_gate_safety_bound` -- exists so the ONE test that specifically exercises "the bound
    // itself fires" (proving the mechanism works, not merely asserting it should) does not have
    // to wait out the real, generous production value to do it.
    gate_bound: std::time::Duration,
}
impl MockSource {
    pub fn new(data: impl Into<Bytes>) -> Self {
        let (started_tx, _) = tokio::sync::watch::channel(0);
        let (cancelled_tx, _) = tokio::sync::watch::channel(0);
        let (released_tx, _) = tokio::sync::watch::channel(0);
        let (gate_timeout_tx, _) = tokio::sync::watch::channel(0);
        Self {
            data: data.into(),
            reads: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            released: AtomicU64::new(0),
            started_tx,
            cancelled_tx,
            released_tx,
            gate_timeouts: AtomicU64::new(0),
            gate_timeout_tx,
            gate_tx: None,
            gate_armed: std::sync::atomic::AtomicBool::new(false),
            gate_bound: DIAGNOSTIC_CEILING,
        }
    }
    /// Installs a gate, disarmed: every `read_block` call proceeds exactly as if no gate existed
    /// at all until `arm_gate` is called. See this struct's own doc comment on `gate_tx` for why
    /// arming is a separate step, and (this struct's own doc comment, above) why a test reaches
    /// for this over a fixed sleep.
    pub fn with_gate(mut self) -> Self {
        let (tx, _) = tokio::sync::watch::channel(false);
        self.gate_tx = Some(tx);
        self
    }
    /// Test-only: overrides the gate's own built-in safety bound (see `arm_gate`'s doc comment)
    /// away from the generous, production-sized `DIAGNOSTIC_CEILING` default. Exists for exactly
    /// one caller: the test that specifically proves the bound fires and panics, which needs a
    /// SHORT override to do that quickly rather than actually waiting out the real value. Every
    /// other test leaves this at its default -- a bound short enough to matter for THAT one test
    /// would also be short enough to spuriously fire against a legitimately slow CI machine
    /// everywhere else.
    #[cfg(test)]
    pub(crate) fn with_gate_safety_bound(mut self, bound: std::time::Duration) -> Self {
        self.gate_bound = bound;
        self
    }
    /// Arms the gate installed by `with_gate`: every `read_block` call from this point onward
    /// parks until `open_gate` releases it -- or until `gate_bound` (`DIAGNOSTIC_CEILING` by
    /// default) elapses, whichever comes first. A silent no-op if no gate was installed. Every
    /// test that uses this arms strictly BEFORE triggering the one read it wants gated, never
    /// concurrently with it, so there is no call this needs to race.
    ///
    /// UN-HANGABLE by construction (pass 8: the class this closes recurred a 3rd time because the
    /// EARLIER, unbounded version of this gate made `with_latency` the path of least resistance --
    /// fix the trap, not just each site that fell into it). The original footgun: arming is
    /// source-WIDE and sticky -- once armed, `read_block` parks EVERY later call on this source,
    /// not just the one call a test had in mind, and a forgotten `open_gate` before a SECOND read
    /// used to hang forever, silently, with no timeout to catch it (a real incident, not a
    /// hypothetical one: a first draft of `status.rs`'s own fix did exactly this). Now, a
    /// forgotten release does not hang -- the parked `read_block` call itself panics once
    /// `gate_bound` elapses, a loud, attributable failure naming the mechanism ("forgotten
    /// open_gate?") instead of a CI job that simply never finishes.
    ///
    /// This does NOT break the OTHER legitimate shape -- a read parked forever, by design (e.g.
    /// proving a block is never read at all), that nothing in the test ever awaits: the bound
    /// lives on the READ's own gate-wait, which only ever gets a chance to fire if something is
    /// still driving that read's own task forward when the bound elapses. A test whose own body
    /// finishes well within `gate_bound` (the overwhelmingly common case -- checking some
    /// transient state takes milliseconds, not seconds) ends its own `#[tokio::test]` runtime
    /// first, which aborts every task it was still holding -- the parked read included -- long
    /// before `gate_bound` could ever matter. The bound only becomes reachable at all when
    /// something keeps the test's own async context alive AT LEAST that long waiting on
    /// something depending on the parked read -- exactly the "forgot to release, and something
    /// really is stuck" case this exists to catch, not the "nobody's watching" one.
    ///
    /// Prefer `park` over calling this directly when the test knows up front it WILL eventually
    /// release: the RAII guard that method returns cannot be forgotten the way a bare, later
    /// `open_gate()` call can.
    pub fn arm_gate(&self) {
        // deliberately NOT a silent no-op the way `open_gate` is (see that method's own doc
        // comment for why IT stays lenient): calling this without `with_gate` first is never a
        // legitimate "nothing to do here" the way releasing an unarmed gate is -- it means a test
        // constructed a plain `MockSource` and then asked to gate a read anyway, almost certainly
        // a missing `.with_gate()` in the construction chain. Caught here, loudly and immediately,
        // is far better than the failure mode this line replaced while writing this unit's own
        // tests: a `park()`/`arm_gate()` that silently did nothing, leaving a test that reads as
        // gate-protected but is not, discovered only by noticing the read completed instantly
        // when it should have parked -- exactly the kind of quiet, easy-to-miss vacuity this
        // whole pass exists to eliminate, not reintroduce in the fix for it.
        assert!(
            self.gate_tx.is_some(),
            "arm_gate (or park) called without with_gate first -- this MockSource was never \
             installed with a gate at all, so there is nothing here to arm"
        );
        self.gate_armed.store(true, Ordering::Relaxed);
    }
    /// Arms the gate (see `arm_gate`'s own doc comment for the full contract) and returns a
    /// guard that calls `open_gate` when dropped -- the common "park this read, do something,
    /// definitely release it again" shape, ergonomic against forgetting the release the way
    /// holding a bare `arm_gate()` + a later bare `open_gate()` call can be. A test that wants
    /// the OTHER shape -- armed and parked forever, by design, nothing ever releases it -- calls
    /// `arm_gate` directly instead and never asks for this guard at all; requesting one and then
    /// deliberately never dropping it (`std::mem::forget`, say) would work but fights the type
    /// rather than using it, and reads as exactly that to anyone reviewing the test.
    pub fn park(&self) -> GateReleaseGuard<'_> {
        self.arm_gate();
        GateReleaseGuard { source: self }
    }
    /// Opens the gate installed by `with_gate`, releasing every call currently parked on it and
    /// any future one. A silent no-op if no gate was installed (mirrors `send_if_modified`'s own
    /// no-receivers-is-a-no-op contract used throughout this struct -- see
    /// `publish_high_water_mark`'s own doc comment).
    ///
    /// Call this before any read a test makes AFTER the one it deliberately gated, once whatever
    /// that gated read was proving (a cancellation event, usually) has already been confirmed --
    /// or, more robustly, reach for `park`'s own RAII guard instead so there is no bare call to
    /// remember at all. A forgotten release before a later read no longer hangs (see `arm_gate`'s
    /// own doc comment on `gate_bound`), but it still fails that later read's own await with a
    /// generic-sounding panic rather than proceeding -- naming the actual mechanism explicitly,
    /// via `park` or a deliberate `open_gate()` call, reads far better in a failure than "why did
    /// this time out."
    pub fn open_gate(&self) {
        if let Some(tx) = &self.gate_tx {
            tx.send_if_modified(|open| {
                if *open {
                    false
                } else {
                    *open = true;
                    true
                }
            });
        }
    }
    /// Number of `read_block` calls so far.
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many `read_block` calls
    /// have STARTED so far -- see this struct's own doc comment for why this exists at all (an
    /// event-based alternative to sleeping and guessing).
    pub fn started_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.started_tx.subscribe()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many `read_block` calls
    /// have had their own future dropped WITHOUT reaching their own return -- see this struct's
    /// own doc comment.
    pub fn cancelled_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.cancelled_tx.subscribe()
    }
    /// A receiver a test can `wait_for`/`changed()` on to observe how many `read_block` calls
    /// have run to their own completion (Ok or Err) -- see this struct's own doc comment.
    pub fn released_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.released_tx.subscribe()
    }
    /// A receiver for how many reads' own gate-wait hit `gate_bound` and panicked -- distinct
    /// from `cancelled_events` so the gate's own diagnostic backstop can never counterfeit the
    /// positive cancellation proof ownership tests wait on. See the field's own doc comment.
    pub fn gate_timeout_events(&self) -> tokio::sync::watch::Receiver<u64> {
        self.gate_timeout_tx.subscribe()
    }
}

/// RAII release for the gate `MockSource::park` installs: opens it (see `MockSource::open_gate`)
/// when dropped, so the common "park a read, do something, definitely release it again" shape
/// cannot forget the release the way a bare, later `open_gate()` call could. See `park`'s own
/// doc comment for the one shape this is deliberately NOT for (armed and parked forever, by
/// design) -- that caller reaches for `arm_gate` directly instead and never asks for this guard.
///
/// `#[must_use]` (p6-review2, pass 8, U1 amendment): a bare `src.park();` -- the value never
/// bound to anything -- drops this guard IMMEDIATELY, re-opening the gate before any read has a
/// chance to reach it at all. Reproduced directly, not assumed: an unbound `park()` call let a
/// read complete in 27.6µs with zero compiler warning, silently testing nothing -- exactly the
/// vacuity this whole pass exists to close, undercutting this type's own "cannot be forgotten"
/// doc claim above. The lint catches it at compile time instead.
#[must_use = "dropping this guard re-opens the gate immediately; bind it (`let _g = ...`) to keep the read parked"]
pub struct GateReleaseGuard<'a> {
    source: &'a MockSource,
}
impl Drop for GateReleaseGuard<'_> {
    fn drop(&mut self) {
        self.source.open_gate();
    }
}

// found in PR #44 pass 7 (P7-C): the scopeguard that turns a dropped `read_block` future into a
// positive `Cancelled`/`Released` event instead of leaving cancellation something only ever
// inferred from silence. Constructed the instant a call enters `read_block` (folding the
// existing `started` signal into construction, so a guard can never exist without one), it fires
// `Released` from its own `Drop` if `complete()` was called first (the call reached its own
// return, regardless of Ok/Err); a third, distinct gate-timeout event if `mark_gate_timeout` was
// called first instead (the gate's own safety-bound panicking path in `read_block`'s gate-wait
// arm -- post-merge batch (2026-07-22), fix #6); or `Cancelled` otherwise (the future was torn
// down early with neither mark set -- an abort mid-gate-park being the only remaining way that
// happens in this crate's own tests).
// Needs no new Send/Sync reasoning: it holds only `&'a MockSource`, and `BlockSource: Send + Sync`
// already requires `MockSource: Sync`, so `&'a MockSource` is `Send` on its own.
struct BlockEventGuard<'a> {
    source: &'a MockSource,
    completed: bool,
    gate_timed_out: bool,
}
impl<'a> BlockEventGuard<'a> {
    fn new(source: &'a MockSource) -> Self {
        let started = source.reads.fetch_add(1, Ordering::Relaxed) + 1;
        publish_high_water_mark(&source.started_tx, started);
        Self {
            source,
            completed: false,
            gate_timed_out: false,
        }
    }
    fn complete(&mut self) {
        self.completed = true;
    }
    fn mark_gate_timeout(&mut self) {
        self.gate_timed_out = true;
    }
}
impl<'a> Drop for BlockEventGuard<'a> {
    fn drop(&mut self) {
        if self.completed {
            let n = self.source.released.fetch_add(1, Ordering::Relaxed) + 1;
            publish_high_water_mark(&self.source.released_tx, n);
        } else if self.gate_timed_out {
            let n = self.source.gate_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
            publish_high_water_mark(&self.source.gate_timeout_tx, n);
        } else {
            let n = self.source.cancelled.fetch_add(1, Ordering::Relaxed) + 1;
            publish_high_water_mark(&self.source.cancelled_tx, n);
        }
    }
}

// found in PR #44 pass 6 S3 (codex, a review finding, "also status/document"): three separate
// call sites (prefetch.rs, status.rs, document.rs) each drove `reads_started()`'s own receiver
// through a RAW `watch::Receiver::wait_for` -- unbounded, with no timeout of its own. #4's fix
// (above) removes the specific regression that could make such a wait hang forever, but an
// unbounded wait on an external signal is fragile defense-in-depth regardless of whether THIS
// particular root cause is the one that ever trips it: a hang here silently eats the whole CI
// job's own timeout instead of failing this one test fast, with no diagnostic pointing at what
// never arrived. One shared, timeout-wrapped helper, not three copies that could drift apart
// (the same "provably the identical implementation" reasoning this crate's own `ress-perf`
// sibling already applies to `peek_then_maybe_reap`) -- mirrors `status.rs`'s own internal
// `wait_for` test helper (a `StatusSnapshot`-typed predicate wrapped in a 5s
// `tokio::time::timeout`). Renamed from `wait_for_reads_started` (pass 7, P7-C) now that
// `started_events`/`cancelled_events`/`released_events` all share this identical `u64`-receiver
// shape -- one generic helper for all three, not one named after (and copy-pasted for) each.
#[cfg(test)]
pub(crate) async fn wait_for_count(
    rx: &mut tokio::sync::watch::Receiver<u64>,
    pred: impl Fn(u64) -> bool,
) -> u64 {
    let value = tokio::time::timeout(DIAGNOSTIC_CEILING, rx.wait_for(|&n| pred(n)))
        .await
        .unwrap_or_else(|_| {
            // named, not a re-typed literal: this message and `DIAGNOSTIC_CEILING`'s own value
            // can never say two different things to each other the way the doc comment and
            // `wait_for_count`'s own old independent literal used to (p6-review2, pass 8, U1
            // amendment).
            panic!("the watched event count never satisfied the expected condition within {DIAGNOSTIC_CEILING:?}")
        })
        .expect("the sender must outlive this receiver for the whole test");
    *value
}
// found in PR #44 pass 6 S3 (#4, a user-review finding, CONFIRMED): `fetch_add` and the watch
// publish used to be two separate, non-atomic steps (`let started = self.reads.fetch_add(1,
// ..) + 1; self.started.send_replace(started);`) -- correctly ordered WITHIN one call (no
// `.await` sits between them), but that guarantees nothing across two DIFFERENT concurrent
// calls under tokio's multi-threaded runtime: real OS-thread preemption can let a task that
// incremented to a LOWER count reach its own publish step AFTER a task that incremented to a
// HIGHER one already published theirs, silently REGRESSING the published value back down (task
// A fetch_adds to 1, task B fetch_adds to 2, B's publish(2) lands first, then A's stale
// publish(1) overwrites it). A receiver `wait_for`-ing "count >= 2" that happens to observe the
// channel only after the regression never sees `2` again -- nothing guarantees a LATER call
// ever republishes it -- so the wait hangs. `send_if_modified` publishes a monotonic
// HIGH-WATER MARK instead: the whole read-current-then-maybe-write happens under `watch`'s own
// internal lock, so no other publish can interleave inside it, and a stale, already-superseded
// value can never overwrite a fresher one that already landed. Extracted as its own function so
// the invariant is directly, deterministically testable by calling it with hand-picked,
// intentionally out-of-order values -- the real race (two OS threads preempted at exactly the
// wrong instant) cannot be forced to reproduce on demand, the same class of race this crate's
// own testing philosophy (AGENTS.md) already refuses to rely on scheduler timing to prove.
fn publish_high_water_mark(sender: &tokio::sync::watch::Sender<u64>, value: u64) {
    sender.send_if_modified(|current| {
        if value > *current {
            *current = value;
            true
        } else {
            false
        }
    });
}

#[async_trait::async_trait]
impl BlockSource for MockSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes> {
        // `send_if_modified`, not `send`/`send_replace`, inside `BlockEventGuard` and here alike:
        // a `watch` send errors when there are no receivers, which none of these three events
        // must ever care about (most callers of a `MockSource` never subscribe to any of them,
        // and that must stay a silent no-op, not a spurious failure inside a trait method with no
        // way to report it upward) -- `send_if_modified`'s own return value (whether it actually
        // updated) is equally fine to discard for the identical reason. See
        // `publish_high_water_mark`'s own doc comment for why this must be a monotonic publish,
        // not a bare replace.
        let mut guard = BlockEventGuard::new(self);
        if let Some(tx) = &self.gate_tx
            && self.gate_armed.load(Ordering::Relaxed)
        {
            let mut rx = tx.subscribe();
            // found in PR #44 pass 8: bounded, not the earlier unbounded wait -- see `arm_gate`'s
            // own doc comment for why this exists (a forgotten `open_gate` must fail loud, never
            // hang silently) and for why a legitimately-forever-parked read with no awaiter never
            // reaches this bound at all (the test's own runtime tears down first).
            match tokio::time::timeout(self.gate_bound, rx.wait_for(|open| *open)).await {
                Ok(recv) => {
                    recv.expect("the sender lives on this same MockSource, alongside the receiver");
                }
                Err(_elapsed) => {
                    guard.mark_gate_timeout();
                    panic!(
                        "MockSource's gate parked longer than {:?} without being opened -- \
                         forgotten open_gate (or the `park` guard it returns)? This is a diagnostic \
                         backstop, not a coordination oracle: a legitimate park-then-release \
                         sequence should never come anywhere close to it.",
                        self.gate_bound
                    )
                }
            }
        }
        let size = self.data.len() as u64;
        let start = offset.min(size) as usize;
        let end = offset.saturating_add(len as u64).min(size) as usize;
        // marked complete BEFORE the tail expression, not after: `guard` drops as this function
        // returns, and only a completion reached HERE (never by a future torn down mid-`.await`
        // above) may claim `Released` -- an early return added later, above this line, would
        // correctly fall through to `Cancelled` instead by simply never reaching this call.
        guard.complete();
        Ok(self.data.slice(start..end))
    }
}

use std::sync::Arc;

/// Default cap on concurrent OS-level reads per `PreadSource` -- see `BoundedBlockingReads`.
/// Far above legitimate steady-state concurrency (prefetch's FILL_CONCURRENCY of 4, plus three
/// single-task background workers, plus the interactive attempt), far below the blocking pool's
/// own ~512: a wedged mount piles up at most this many stuck threads, not the whole pool.
pub const DEFAULT_READ_CONCURRENCY: usize = 16;

/// post-merge batch (2026-07-22), fix #1 -- the P1: bounds how many blocking OS reads one
/// source can have in flight AT THE OS, regardless of task choreography. The permit moves INTO
/// the `spawn_blocking` closure, so it is released when the OS read actually returns -- never
/// by a dropped future. Supersession (a `select!` arm dropping a scan mid-read, a pending nav
/// slot replaced) detaches the *await* but never the *permit*: each detached read keeps its
/// slot until the OS answers, and a new read past the cap waits at the acquire -- which is
/// itself an ordinary cancellable await, so a superseded waiter consumes nothing at all.
/// Deadlock-free with the block cache by construction: same-block requests coalesce on the
/// cache's in-flight watch channel without ever reaching this semaphore, and the acquire never
/// runs under the cache mutex. FIFO fairness comes from tokio's own semaphore.
#[derive(Clone)]
pub struct BoundedBlockingReads {
    sem: Arc<tokio::sync::Semaphore>,
}
impl BoundedBlockingReads {
    pub fn new(limit: usize) -> Self {
        // clamped, not asserted, at both ends: a knob value of 0 means "no reads ever," which
        // no caller can want, so 1 (fully serialized) is the honest minimum; and a bare
        // `--read-concurrency` value above `Semaphore::MAX_PERMITS` would make `Semaphore::new`
        // itself panic at startup, so the upper end is clamped too rather than trusted.
        Self {
            sem: Arc::new(tokio::sync::Semaphore::new(
                limit.clamp(1, tokio::sync::Semaphore::MAX_PERMITS),
            )),
        }
    }
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("this semaphore is never closed for the life of its source");
        let value = tokio::task::spawn_blocking(move || {
            // the permit lives exactly as long as the OS-level work: released on return,
            // held across a caller's own cancellation -- the entire point of this type.
            let _permit = permit;
            work()
        })
        .await?;
        Ok(value)
    }
    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.sem.available_permits()
    }
}

/// A `BlockSource` backed by a real file using positioned reads (`pread`),
/// so concurrent reads never contend a shared file offset. Blocking reads run
/// on tokio's blocking pool; an io_uring backend can replace this later.
pub struct PreadSource {
    file: Arc<std::fs::File>,
    size: u64,
    reads: BoundedBlockingReads,
}
impl PreadSource {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        Self::open_with_read_concurrency(path, DEFAULT_READ_CONCURRENCY)
    }
    /// Opens with an explicit in-flight OS read cap (`--read-concurrency`; clamped into
    /// [1, Semaphore::MAX_PERMITS] by `BoundedBlockingReads::new`). Construction-only BY DESIGN
    /// (post-merge batch 2 (2026-07-22), finding #3): a post-use swap would mint a fresh
    /// allowance while detached mid-flight reads still hold permits of the old pool, silently
    /// breaking the "at most N OS reads per source, ever" invariant docs/concurrency.md
    /// claims -- the pool's identity is fixed for the source's whole life, so the invariant
    /// holds structurally, not by caller discipline.
    pub fn open_with_read_concurrency(
        path: &std::path::Path,
        limit: usize,
    ) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            size,
            reads: BoundedBlockingReads::new(limit),
        })
    }
}
#[async_trait::async_trait]
impl BlockSource for PreadSource {
    fn size(&self) -> u64 {
        self.size
    }
    async fn read_block(&self, offset: u64, len: usize) -> anyhow::Result<Bytes> {
        let file = self.file.clone();
        let size = self.size;
        let bytes = self
            .reads
            .run(move || -> anyhow::Result<Bytes> {
                use std::os::unix::fs::FileExt;
                let start = offset.min(size);
                let end = offset.saturating_add(len as u64).min(size);
                let n = (end - start) as usize;
                let mut buf = vec![0u8; n];
                let mut filled = 0;
                while filled < n {
                    let read = file.read_at(&mut buf[filled..], start + filled as u64)?;
                    if read == 0 {
                        break;
                    }
                    filled += read;
                }
                buf.truncate(filled);
                Ok(Bytes::from(buf))
            })
            .await??;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mock_returns_requested_bytes() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        assert_eq!(src.size(), 10);
        let b = src.read_block(2, 4).await.unwrap();
        assert_eq!(&b[..], b"2345");
    }
    #[tokio::test]
    async fn mock_short_read_at_eof() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let b = src.read_block(8, 100).await.unwrap();
        assert_eq!(&b[..], b"89");
    }
    #[tokio::test]
    async fn mock_empty_past_eof() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let b = src.read_block(50, 4).await.unwrap();
        assert!(b.is_empty());
    }
    #[tokio::test]
    async fn mock_counts_reads() {
        let src = MockSource::new(Bytes::from_static(b"0123456789"));
        let _ = src.read_block(0, 4).await.unwrap();
        let _ = src.read_block(4, 4).await.unwrap();
        assert_eq!(src.read_count(), 2);
    }
    // found in PR #44 pass 6 S3 (#4): pins `publish_high_water_mark`'s own contract directly,
    // deterministically -- no real concurrent tasks, no real thread-scheduling race. Calls it
    // with `2` THEN `1`, standing in for the exact regression the real bug allowed: a task that
    // incremented to the HIGHER count (2) publishing first, then a task that incremented to the
    // LOWER count (1) -- stale by the time it actually gets to publish -- landing second. The
    // published value must stay at the high-water mark (2), never regress back down to the
    // stale, out-of-order arrival.
    #[test]
    fn publish_high_water_mark_never_regresses_on_a_stale_out_of_order_publish() {
        let (tx, mut rx) = tokio::sync::watch::channel(0u64);
        super::publish_high_water_mark(&tx, 2);
        super::publish_high_water_mark(&tx, 1);
        assert_eq!(
            *rx.borrow_and_update(),
            2,
            "a stale, out-of-order publish must never regress the high-water mark"
        );
    }
    #[test]
    fn publish_high_water_mark_advances_on_a_genuine_increase() {
        let (tx, mut rx) = tokio::sync::watch::channel(0u64);
        super::publish_high_water_mark(&tx, 1);
        super::publish_high_water_mark(&tx, 2);
        assert_eq!(*rx.borrow_and_update(), 2);
    }
    // found in PR #44 pass 8, U1 (the gate itself, un-hangable): (a) the ordinary shape still
    // works exactly as before -- park, do something (here, just confirm the read genuinely
    // reached the gate via `started_events`, not merely spawned), release via `park`'s own RAII
    // guard, and the parked read completes normally afterward.
    #[tokio::test]
    async fn park_then_release_lets_the_read_complete() {
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        let guard = src.park();
        let read = tokio::spawn({
            let src = src.clone();
            async move { src.read_block(0, 4).await }
        });
        let mut started = src.started_events();
        wait_for_count(&mut started, |n| n >= 1).await;
        drop(guard);
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), read)
            .await
            .expect("the read never completed after the gate was released")
            .unwrap()
            .unwrap();
        assert_eq!(&bytes[..], b"0123");
    }
    // (b) the footgun this whole unit exists to close: a forgotten release must fail LOUD within
    // the bound, not hang forever. Uses a short override (see `with_gate_safety_bound`'s own doc
    // comment) so this test itself stays fast while still exercising the real timeout-then-panic
    // mechanism, not the multi-second production default. `arm_gate` directly, not `park` --
    // deliberately simulating a test that never asks for (or forgets) a release, the exact shape
    // `park`'s own RAII guard exists to make less likely, not a claim `park` itself is broken.
    // The outer 5s timeout is a SEPARATE safety net around this test's own assertion, not the
    // mechanism under test: if this fix ever regresses to an unbounded hang, this test still
    // fails within 5s instead of hanging the whole suite forever, with a diagnostic pointing at
    // exactly that regression rather than an inscrutable CI timeout.
    #[tokio::test]
    async fn forgetting_to_release_the_gate_fails_loud_within_the_bound() {
        let src = Arc::new(
            MockSource::new(Bytes::from_static(b"0123456789"))
                .with_gate()
                .with_gate_safety_bound(std::time::Duration::from_millis(50)),
        );
        src.arm_gate();
        let read = tokio::spawn({
            let src = src.clone();
            async move { src.read_block(0, 4).await }
        });
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .expect("regressed to an unbounded hang -- the gate's own safety bound never fired");
        assert!(
            joined.is_err(),
            "expected the spawned read to PANIC once the gate's own safety bound elapsed \
             (a forgotten release), but it returned normally instead: {joined:?}"
        );
        // post-merge batch (2026-07-22), fix #6: deterministic post-join reads -- the panicked
        // task is fully unwound by the time `joined` resolved, so every Drop (the event guard
        // included) has already run; no wait, no race.
        assert_eq!(
            *src.gate_timeout_events().borrow(),
            1,
            "the gate's own timeout must be its own positive, distinct event"
        );
        assert_eq!(
            *src.cancelled_events().borrow(),
            0,
            "the gate's own timeout panic must never counterfeit a Cancelled event -- \
             ownership tests accept Cancelled as proof a future was torn down, and a leaked \
             task panicking at the bound is the opposite of that proof"
        );
    }
    // (c) the OTHER legitimate shape (audit finding 1b: a block provably never read at all)
    // must keep working -- armed and parked forever, by design, with nothing in the test ever
    // awaiting that read's own completion. This test's own body finishes in milliseconds (well
    // under even the default multi-second `DIAGNOSTIC_CEILING`, never mind bothering to shrink
    // it), so the `#[tokio::test]` runtime tears down -- aborting the still-parked read along
    // with it -- long before the bound could ever matter.
    // stated honestly (post-merge batch (2026-07-22), fix #6): a panic inside the unawaited
    // spawned task is structurally INVISIBLE to this test (nothing joins `_read`), so this test
    // cannot detect "the bound fired when nothing awaited it." What it pins is narrower and
    // still real: the no-awaiter shape stays legal and non-hanging -- the test body completes in
    // milliseconds and the runtime teardown aborts the parked read long before the bound could
    // matter.
    #[tokio::test]
    async fn a_permanently_parked_read_with_no_awaiter_never_trips_the_bound() {
        let src = Arc::new(MockSource::new(Bytes::from_static(b"0123456789")).with_gate());
        src.arm_gate();
        let mut started = src.started_events();
        let _read = tokio::spawn({
            let src = src.clone();
            async move { src.read_block(0, 4).await }
        });
        wait_for_count(&mut started, |n| n >= 1).await;
        // deliberately no `open_gate`, no `.await` on `_read`'s own join handle: the test ends
        // here, and the runtime drop that follows aborts `_read` while it still sits parked.
    }
    #[tokio::test]
    async fn pread_reads_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let src = PreadSource::open(&path).unwrap();
        assert_eq!(src.size(), 11);
        let b = src.read_block(6, 5).await.unwrap();
        assert_eq!(&b[..], b"world");
    }
    #[tokio::test]
    async fn pread_short_read_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc").unwrap();
        let src = PreadSource::open(&path).unwrap();
        let b = src.read_block(1, 100).await.unwrap();
        assert_eq!(&b[..], b"bc");
    }
    #[tokio::test]
    async fn pread_empty_past_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc").unwrap();
        let src = PreadSource::open(&path).unwrap();
        let b = src.read_block(99, 4).await.unwrap();
        assert!(b.is_empty());
    }
    // post-merge batch (2026-07-22), fix #1 -- the P1. The bound's whole contract in one test,
    // via real thread barriers (no scheduler timing): with N=2 permits held by parked blocking
    // work, a third run() cannot start its work; releasing one holder is what lets it in; full
    // release drains back to N. Every wait below is a positive rendezvous (Barrier::wait
    // returns only when all parties arrived), so nothing here infers from absence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bound_admits_at_most_n_blocking_reads_and_releases_by_completion() {
        let pool = BoundedBlockingReads::new(2);
        let entry = Arc::new(std::sync::Barrier::new(3)); // two holders + the test
        let (hold_a_tx, hold_a_rx) = std::sync::mpsc::channel::<()>();
        let (hold_b_tx, hold_b_rx) = std::sync::mpsc::channel::<()>();
        let a = tokio::spawn({
            let pool = pool.clone();
            let entry = entry.clone();
            async move {
                pool.run(move || {
                    entry.wait();
                    hold_a_rx.recv().unwrap();
                })
                .await
            }
        });
        let b = tokio::spawn({
            let pool = pool.clone();
            let entry = entry.clone();
            async move {
                pool.run(move || {
                    entry.wait();
                    hold_b_rx.recv().unwrap();
                })
                .await
            }
        });
        // rendezvous: both closures are INSIDE the blocking pool, each holding a permit.
        let entry_wait = entry.clone();
        tokio::task::spawn_blocking(move || entry_wait.wait())
            .await
            .unwrap();
        assert_eq!(
            pool.available_permits(),
            0,
            "both permits held by parked reads"
        );

        // the third read: its own entry barrier proves when its closure actually runs.
        let third_entry = Arc::new(std::sync::Barrier::new(2));
        let c = tokio::spawn({
            let pool = pool.clone();
            let third_entry = third_entry.clone();
            async move { pool.run(move || third_entry.wait()).await }
        });
        // release ONE holder; the third read's closure running (this rendezvous returning) is
        // the positive, ordered proof it acquired the freed permit.
        hold_a_tx.send(()).unwrap();
        let third_wait = third_entry.clone();
        tokio::task::spawn_blocking(move || third_wait.wait())
            .await
            .unwrap();

        hold_b_tx.send(()).unwrap();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        c.await.unwrap().unwrap();
        assert_eq!(
            pool.available_permits(),
            2,
            "all permits returned once the reads finished"
        );
    }

    // supersession is the P1's trigger: a run() future dropped while parked AT THE ACQUIRE
    // must consume nothing (no OS read started, no permit leaked). The positive proof is the
    // aftermath: the released pool admits a later read and ends with its full permit count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropping_a_run_parked_at_the_acquire_leaks_no_permit() {
        let pool = BoundedBlockingReads::new(1);
        let entry = Arc::new(std::sync::Barrier::new(2));
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::spawn({
            let pool = pool.clone();
            let entry = entry.clone();
            async move {
                pool.run(move || {
                    entry.wait();
                    hold_rx.recv().unwrap();
                })
                .await
            }
        });
        let entry_wait = entry.clone();
        tokio::task::spawn_blocking(move || entry_wait.wait())
            .await
            .unwrap();
        assert_eq!(pool.available_permits(), 0);

        // post-merge batch 2 (2026-07-22), finding #7: the future is polled to Pending HERE,
        // proving it genuinely reached and parked at the acquire (the sole permit is held
        // above, so a first poll cannot return Ready) -- the earlier spawn-then-abort shape
        // could race the first poll and pass without ever exercising the acquire path at all.
        // Dropped while provably parked; the aftermath below proves nothing leaked.
        {
            use std::future::Future;
            let noop = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(noop);
            let mut superseded = std::pin::pin!(pool.run(|| ()));
            assert!(
                superseded.as_mut().poll(&mut cx).is_pending(),
                "with the sole permit held, a polled run() must park at the acquire"
            );
        } // <- superseded dropped here, parked at the acquire

        hold_tx.send(()).unwrap();
        holder.await.unwrap().unwrap();
        assert_eq!(
            pool.run(|| 42).await.unwrap(),
            42,
            "the pool still admits work"
        );
        assert_eq!(
            pool.available_permits(),
            1,
            "no permit leaked by the dropped acquire"
        );
    }

    // the flagship discrimination the sibling tests cannot make (post-merge batch (2026-07-22),
    // fix #1): dropping the owning future MID-READ -- after the acquire, while the blocking
    // closure is parked inside spawn_blocking -- must NOT free the permit. A broken impl that
    // kept the permit in the future's own frame (released on drop, while the detached OS read
    // runs on) shows available_permits() == 1 at the assert below; the real impl shows 0 until
    // the detached closure itself returns, which is the entire point of the type.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_mid_read_drop_keeps_the_permit_until_the_os_read_returns() {
        let pool = BoundedBlockingReads::new(1);
        let entry = Arc::new(std::sync::Barrier::new(2));
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let owner = tokio::spawn({
            let pool = pool.clone();
            let entry = entry.clone();
            async move {
                pool.run(move || {
                    entry.wait();
                    hold_rx.recv().unwrap();
                })
                .await
            }
        });
        // rendezvous: the closure is provably inside spawn_blocking, holding the permit.
        let entry_wait = entry.clone();
        tokio::task::spawn_blocking(move || entry_wait.wait())
            .await
            .unwrap();
        // supersession, mid-read: the owning future is torn down; the await ensures the
        // task's own teardown fully ran before the assert reads the permit count.
        owner.abort();
        let _ = owner.await;
        assert_eq!(
            pool.available_permits(),
            0,
            "a dropped future must never free the permit while the detached OS read still runs"
        );
        hold_tx.send(()).unwrap();
        // the detached closure returning is what frees the permit -- proven by a subsequent
        // run completing and the count returning to full.
        assert_eq!(pool.run(|| 7).await.unwrap(), 7);
        assert_eq!(pool.available_permits(), 1);
    }

    // new(0) clamps to 1 rather than deadlocking every caller forever -- proven by a read
    // actually completing, with a diagnostic ceiling so a broken clamp fails loud, not silent.
    #[tokio::test]
    async fn a_zero_limit_clamps_to_one_and_still_admits_reads() {
        let pool = BoundedBlockingReads::new(0);
        let value = tokio::time::timeout(std::time::Duration::from_secs(5), pool.run(|| 42))
            .await
            .expect("a clamped-to-one pool must admit a read, not park it forever")
            .unwrap();
        assert_eq!(value, 42);
    }

    // new(usize::MAX) clamps to Semaphore::MAX_PERMITS rather than panicking at startup --
    // `Semaphore::new` itself panics above that ceiling, and a bare `usize` from
    // `--read-concurrency` can carry any value including this one; same loud-ceiling idiom as
    // the zero-limit sibling above, proving admission rather than merely that construction
    // did not panic.
    #[tokio::test]
    async fn a_usize_max_limit_clamps_to_the_semaphore_ceiling_and_still_admits_reads() {
        let pool = BoundedBlockingReads::new(usize::MAX);
        let value = tokio::time::timeout(std::time::Duration::from_secs(5), pool.run(|| 42))
            .await
            .expect("a clamped-to-the-ceiling pool must admit a read, not park it forever")
            .unwrap();
        assert_eq!(value, 42);
    }

    // post-merge batch 2 (2026-07-22), finding #8 (PARTIALLY DECLINED -- the discriminating
    // half declined as a banned shape): renamed from `pread_source_bound_of_one_serializes_
    // without_deadlock`, which claimed to observe serialization but did not -- an
    // implementation that bypassed `BoundedBlockingReads` entirely would also pass this. What
    // this actually pins: routing through a bound-of-one pool compiles end to end, and both
    // concurrent reads against a real file complete with the correct bytes, no deadlock. The
    // discriminating coverage for the bound itself lives above, in `BoundedBlockingReads`'s own
    // unit tests (`bound_admits_at_most_n_blocking_reads_and_releases_by_completion` and the
    // mutation-verified `a_mid_read_drop_keeps_the_permit_until_the_os_read_returns`), plus the
    // fact that `read_block` routes through `self.reads.run(..)` in three directly inspectable
    // lines. The stronger shape here -- proving "the second read cannot enter until the first
    // releases" against a real file -- was considered and declined: it needs
    // timeout-as-proof-of-blocking, the exact absence shape
    // `ress-core/tests/no_timing_oracles.rs` check 2 bans.
    #[tokio::test]
    async fn pread_source_reads_complete_correctly_through_a_bound_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let src = PreadSource::open_with_read_concurrency(&path, 1).unwrap();
        let (a, b) = tokio::join!(src.read_block(0, 4), src.read_block(6, 4));
        assert_eq!(&a.unwrap()[..], b"0123");
        assert_eq!(&b.unwrap()[..], b"6789");
    }
}
