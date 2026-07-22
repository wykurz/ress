// openpty-based process driver: spawns a subject with a real controlling terminal, using the
// setsid + dup2 + TIOCSCTTY recipe proven in ress/tests/smoke.rs's
// first_paint_event_reaches_the_log_file, generalized with a real winsize, a non-blocking
// master, and a killpg-on-drop cleanup contract. no shell anywhere: argv is exec'd directly.
//
// runner.rs's run_sample is the live caller of everything here (spawn, read_available,
// write_keys, try_wait, pid), driven from main.rs through scenarios.rs.

/// Outcome of one non-blocking read attempt against the pty master.
#[derive(Debug)]
pub enum ReadStatus {
    // the byte count is the natural read api even though every current
    // caller matches `Data(_)` — the buffer itself carries the payload.
    #[allow(dead_code)]
    Data(usize),
    Eof,
    WouldBlock,
}

/// Outcome of one bounded `wait_for_readable` call -- found in PR #44 pass 5 commit 3: replaces
/// the poll loop's own former blind sleep with an actual wait FOR something, so this exists to
/// report which of the two ways that wait can end. `Readable` covers `POLLIN` and `POLLHUP`
/// alike, deliberately not distinguished here -- see `wait_for_readable`'s own doc comment for
/// why letting the subsequent `read()` sort out Data/Eof is the correct layering, not a missed
/// case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollReadiness {
    Readable,
    TimedOut,
}

// owns the master fd's lifetime; closing it is unconditional and does not depend on the
// child's own state, so it lives in its own small raii type rather than PtyChild's Drop body.
struct MasterFd(libc::c_int);
impl Drop for MasterFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// A subject process driven through an owned pty.
///
/// `Drop` sends `SIGKILL` to the whole process group and reaps it — idempotent, and safe to
/// call after the child has already been observed to exit via `try_wait`.
// the pid of the currently-active PtyChild, or 0 for none -- found necessary in PR #44's
// round-4 review: the SIGINT/SIGTERM handler main.rs installs (see its own doc comment) used to
// clean up temp files but never killed the actual subject, which setsid put in its OWN session
// -- an interrupted run left ress/less alive, possibly wedged on the exact hung/dead mount this
// harness exists to measure. `pid` doubles as the pgid: setsid() (this module's own pre_exec
// closure) makes them equal for every PtyChild this crate spawns, so no separate pgid needs
// tracking. Registered automatically by `spawn_impl` on success and cleared automatically by
// `Drop` -- tied to the SAME RAII lifecycle that already guarantees `Drop` runs on every
// `run_sample` exit path (an early `return`, a `?`-propagated error, or falling off the end),
// rather than separate manual bookkeeping at each of those sites that would be easy to miss on
// one of them.
//
// found in PR #44 round 9: `Drop` was not the ONLY moment this needed clearing at. `try_wait`
// (below) can OBSERVE the exit long before `Drop` actually runs -- `PtyChild` is a plain local
// in `run_sample`, live for the rest of that function's own polling loop -- and in that gap the
// pid is kernel-reusable the instant it is reaped, so a signal landing there would find the
// registration still non-zero and `kill(-pgid)` at a possibly-RECYCLED target. `try_wait` now
// clears it itself, at the moment the exit is observed, via the exact same compare-exchange
// helper `Drop` uses; `Drop`'s own clear stays as an idempotent backstop for the path where
// nothing ever called `try_wait` before it ran. See `try_wait`'s own doc comment for the fix
// itself.
//
// `Drop`'s own clear runs LAST, after its own `killpg` (and the bounded reap that follows it) --
// found necessary in PR #44's round-6 review: an earlier version cleared FIRST, before issuing
// the kill, on the reasoning that the compare-exchange's "clear only if it's still mine" guard
// was the only ordering property that mattered. It missed a sharper one: a SIGINT/SIGTERM
// landing in the (however brief) window between that early clear and the kill that followed it
// would find `ACTIVE_CHILD_PGID` already zero -- the handler's own `if active_pgid != 0`
// gate (main.rs) would then skip killing the subject entirely, unlink temp files, and `_exit`,
// orphaning the very setsid'd subject Drop was in the middle of trying to kill. Clearing last
// closes that window at the cost of opening a narrower, harmless one: a signal landing AFTER
// Drop's own `killpg` but before its clear could see the registration still active and issue
// ANOTHER `killpg` against the same pgid. This double-kill is accepted, not a new risk --
// `killpg`/`SIGKILL` are idempotent for this purpose: a process already sent `SIGKILL` either
// dies from the first one or (D-state, mid-blocking-syscall) has it already latched as pending,
// so a second delivery changes nothing observable; against a dead-but-unreaped zombie the second
// call still returns success (signaling a zombie is legal and inert), and only once the pgid is
// fully reaped does `killpg` return `ESRCH` -- both outcomes this Drop's own kill call already
// tolerates and discards. A missed kill orphans a subject; a redundant one is a no-op --
// clearing last trades the only shape of failure that matters here for one that cannot cause
// any.
//
// `Drop`'s own clear is a compare-and-swap ("clear only if it's still MY pid"), not a bare
// store, so an unexpected overlap between two `PtyChild`s could never let one's teardown erase a
// different, newer child's registration. In this crate's own production call path that overlap
// cannot actually happen -- `runner::run_sample` (the only call site that ever spawns a
// `PtyChild`, i.e. the only call site that ever registers a NEW pgid) owns its `PtyChild` as a
// plain local variable that is never moved out or leaked; the function cannot return, and so
// cannot be called again to spawn a second one, until the current one's `Drop` (this one) has
// already run to completion, single-threaded, on the same call stack. Nothing in this crate's own
// signal handler registers a NEW child either -- it only conditionally kills whatever is already
// registered and then `_exit`s. The compare-exchange guard is therefore genuine defensive
// insurance against that invariant ever drifting (a future concurrent caller, say), not a
// property this crate's own current call graph can actually exercise -- which is exactly why the
// pure-function tests below exercise it directly, against a throwaway atomic, rather than relying
// on it being reachable through the real one.
pub(crate) static ACTIVE_CHILD_PGID: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

// pure functions over an explicit `&AtomicI32` rather than reaching for ACTIVE_CHILD_PGID
// directly at each call site -- lets the registration/clear LOGIC be unit-tested against a
// throwaway local atomic (see this module's own tests), never the real global one, which is
// genuine process-wide shared state and would otherwise make such a test racy against every
// OTHER test that also spawns/drops a PtyChild under plain `cargo test`'s thread-parallel model
// (confirmed empirically: a first version of this test asserting directly against
// ACTIVE_CHILD_PGID failed 5/5 full-suite runs under that mode, 0/5 in isolation -- exactly the
// cross-test interference this refactor exists to sidestep, the same class round 3's item 6
// already found and fixed for a different shared path).
fn register_active_child(atomic: &std::sync::atomic::AtomicI32, pid: i32) {
    atomic.store(pid, std::sync::atomic::Ordering::SeqCst);
}

fn clear_active_child_if_mine(atomic: &std::sync::atomic::AtomicI32, pid: i32) {
    let _ = atomic.compare_exchange(
        pid,
        0,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );
}

// found in PR #44 pass 3: round 9's own fix cleared the registration at the moment `try_wait`
// OBSERVED an exit, closing the "clear only at Drop, much later" window -- but the observation
// and the REAL reap were still one call (`std::process::Child::try_wait` reaps and reports in a
// single step), so `self.reaped = true; clear_active_child_if_mine(...)` always ran strictly
// AFTER the kernel had already fully consumed the zombie. A signal landing in THAT (much
// narrower than round 9's, but still real and non-zero) window would still find
// `ACTIVE_CHILD_PGID` pointing at a pid the kernel had already made recyclable.
//
// `waitid(2)`'s own `WNOWAIT` flag closes it at the source: it reports a terminated child's
// status WITHOUT consuming it, so the zombie survives the peek, still unrecyclable, until
// something ELSE reaps it for real. Confirmed empirically, not assumed -- a standalone probe
// (both a raw C `waitid`/`waitpid` chain and an in-tree Rust one, driving a real
// `std::process::Command` child): peeking twice in a row both succeed identically (proving the
// zombie survives the first peek), and a later, ordinary reap (`std::process::Child::try_wait`'s
// own underlying `waitpid`) still consumes it correctly afterward. Also confirmed the hard way
// that `WNOWAIT` is a `waitid`-only flag: an earlier attempt passed `WNOHANG | WNOWAIT` straight
// to `waitpid(2)` and got `EINVAL` -- `waitpid`'s own options are `WNOHANG`/`WUNTRACED`/
// `WCONTINUED` only, `WNOWAIT` belongs to the separate `waitid` syscall, not a flag on the same
// one `std::process::Child` already uses internally. That separation is exactly what makes this
// safe to layer on top without disturbing `std::process::Child`'s own bookkeeping: the peek
// changes nothing about the zombie's kernel state, so `std::process::Child::try_wait` run
// afterward reaps for real exactly as if this function had never run at all.
//
// Returns whether a genuine exit was observed (and, if so, has already cleared the
// registration) -- the caller still owns doing the REAL reap afterward, via whatever mechanism
// it already uses (`try_wait`'s own `std::process::Child::try_wait`, or Drop's bounded-reap
// loop's identical call) -- this function only ever peeks, never reaps.
fn clear_registration_if_the_child_has_exited(
    atomic: &std::sync::atomic::AtomicI32,
    pid: i32,
) -> bool {
    // zeroed explicitly rather than trusting the kernel to have done it: POSIX leaves
    // `siginfo_t`'s contents unspecified when WNOHANG finds nothing ready yet, so a stale
    // `si_pid` left over from the caller's own stack would be indistinguishable from a genuine
    // report without this -- the same "own the answer instead of trusting an unspecified
    // default" reasoning `sibling_bin_path`'s own triple-detection already uses.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // `si_pid()` is only meaningful (and only safe to read at all -- siginfo_t is a union
    // underneath) once `rc == 0` genuinely reported something; `WEXITED`'s own contract means
    // that something is always a termination when it fires.
    if rc == 0 && unsafe { info.si_pid() } == pid as libc::pid_t {
        clear_active_child_if_mine(atomic, pid);
        true
    } else {
        false
    }
}

// the class-level shape `try_wait` and `Drop`'s own bounded-reap loop both now share: a
// consuming reap is authorized ONLY by a positive peek, and a negative one returns `None`
// WITHOUT ever invoking it -- see `try_wait`'s own doc comment for the full derivation of why.
// `peek` and `reap` are injected purely so a test can PROVE the negative-peek case never
// invokes `reap` at all (a panicking `reap` closure would fire if it did), the same "inject a
// probe, observe whether it runs" technique `checkpoint_confirms_only_while_alive`'s own tests
// already use for an analogous ordering claim -- necessary here for the identical reason it was
// there: a black-box test that only checks the two non-racing steady states cannot tell a
// correct implementation apart from the pre-fix, unconditionally-falls-through one, since both
// produce the IDENTICAL answer whenever the child genuinely is alive or genuinely already dead.
// The bug this closes is entirely about the race window BETWEEN those two states, invisible to
// any test that only ever samples one of them.
// call counter. Exists for exactly one reason: `try_wait` and Drop's own bounded-reap loop BOTH
// now route through this one function, but the race this function's own doc comment derives is,
// by its own nature, invisible to any black-box test of either call site (see
// `try_wait_negative_peek_never_consumes_positive_peek_clears_before_reaping`'s own doc comment
// for why a steady-state observation cannot tell the fix apart from the pre-fix,
// unconditionally-falls-through shape it replaced). A test cannot inject a probe into Drop's own
// hard-coded closure the way the pure tests below inject one here directly -- so this counter is
// the closest equivalent for THAT call site: not a race-safety proof (the pure tests already own
// that, and now cover Drop too, by construction, since Drop calls this exact function), but a
// deterministic, non-flaky proof that Drop's real path genuinely reaches it at all, which is
// exactly what the reviewer's own described revert (reverting Drop's call site back to a
// hand-inlined copy that never calls this function) would falsify.
//
// found in PR #44 pass 6 (codex pty.rs:1748, "the counter race"): this used to be a single
// `#[cfg(test)]`-only process-wide static that `peek_then_maybe_reap` reached for internally --
// race-free for proving a WORKING Drop (which always contributes at least one real increment of
// its own between the before/after reads, so no concurrently-running test's activity could ever
// make that assertion false), but NOT for catching a BROKEN one: a broken Drop contributes zero
// increments of its OWN, so `calls_after > calls_before` would need a DIFFERENT, concurrently
// running test's increment to sneak in during the same window to false-PASS -- exactly the
// process-wide-shared-state class `PtyChild::registry`'s own injection seam already exists to
// close, just not yet applied to this counter. Fixed the same way: `calls` is injected here,
// mirroring `registry`, so a test can supply its own private counter that no concurrently
// running test can ever touch, making the observation race-free by construction rather than by
// probabilistic argument. `PEEK_THEN_MAYBE_REAP_CALLS` remains the real, unconditional default
// every production call site still uses (via `PtyChild::spawn`'s own `reap_call_counter` field);
// only a test that explicitly opts in via a dedicated constructor ever sees anything else -- the
// same "production is unaffected" shape `registry`'s own injection already established.
static PEEK_THEN_MAYBE_REAP_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn peek_then_maybe_reap<T>(
    calls: &std::sync::atomic::AtomicU64,
    peek: impl FnOnce() -> bool,
    reap: impl FnOnce() -> T,
) -> Option<T> {
    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if !peek() {
        return None;
    }
    Some(reap())
}

// classifies one CONSUMING reap attempt's own outcome (the `reap` closure `peek_then_maybe_
// reap` just authorized and ran) -- was a genuine exit CONFIRMED (a real `ExitStatus` came
// back, or the attempt failed with `ECHILD`, meaning the kernel has no such child to wait for
// AT ALL: "no longer this process's own child," the identical terminal signal `retry_abandoned_
// reaps` already treats as done for the same errno), or did the attempt merely fail to
// COMPLETE -- `EINTR`, or any other transient error -- which proves NOTHING about whether the
// child has actually exited?
//
// found in PR #44 pass 6 S2 (#5, a user-review finding): `try_wait` and Drop's own bounded-reap
// loop both used to treat ANY `Err` here identically to a genuine `Ok(Some(_))` -- `try_wait`
// marked itself `reaped` regardless of which one it actually got, and Drop's own loop `return`ed
// immediately either way, never retrying and never falling back to `ABANDONED_PIDS`. A transient
// failure proves nothing about liveness: the child could easily still be alive. Marking `reaped`
// on that basis is the more serious of the two -- it makes a LATER `Drop` skip its own `kill()`
// entirely (`teardown_registered_child`'s own `if !reaped` gate), leaving a still-ALIVE subject
// completely unsignaled, worse than the zombie either bug actually risked. Extracted as its own
// pure function -- shared, not duplicated, by both call sites, the same "provably the identical
// implementation, not just the identical intent" reasoning `peek_then_maybe_reap` itself was
// extracted for -- so a test can pin the classification directly and deterministically, without
// a real child or a real interrupted syscall (see this module's own tests, and `PtyChild::
// try_wait_with_reap`'s own doc comment for the real-process, fault-injected half of this
// fix's coverage).
enum ReapAttempt {
    Confirmed,
    Retry,
}

fn classify_reap_attempt(
    result: &std::io::Result<Option<std::process::ExitStatus>>,
) -> ReapAttempt {
    match result {
        Ok(Some(_)) => ReapAttempt::Confirmed,
        Ok(None) => ReapAttempt::Retry,
        Err(err) if err.raw_os_error() == Some(libc::ECHILD) => ReapAttempt::Confirmed,
        Err(_) => ReapAttempt::Retry,
    }
}

// SIGINT, SIGTERM, SIGHUP -- exactly the three signals main.rs's own cleanup handler installs
// for (see `install_signal_cleanup_handler`'s own doc comment for SIGHUP's own addition, the
// PR #44 pass-2 companion half of this same finding). One function builds the set so the
// parent-side block/unblock in `spawn_window_blocked` and the child-side unblock inside
// `spawn_impl`'s own `pre_exec` closure can never drift apart from each other. `pub(crate)`:
// `runner.rs`'s own `FakeHome::create` reuses BOTH this and `spawn_window_blocked` directly for
// the structurally identical "create a resource, then register it for signal-safe cleanup"
// window its own PR #44 pass-2 finding needed closed -- one mechanism, not two that could drift.
pub(crate) fn spawn_window_signal_set() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
    }
    set
}

// pulled apart from `spawn_impl`'s own body specifically to make the signal-masking WINDOW
// unit-testable, the same reasoning round 6's `teardown_registered_child` extraction already
// established for the structurally mirror-image problem there (does the registration CLEAR
// happen before or after the kill) applied here to its opposite (does the registration SET
// happen before or after the signals are unblocked). Found necessary in PR #44 pass-2 review:
// `spawn_impl` used to register the pid only AFTER `command.spawn()` had already returned a
// real, live child -- `command.spawn()`'s own fork already ran `setsid()` (via `pre_exec`) by
// the time it returns, so a genuine subject process exists for however long that gap lasts. A
// SIGINT/SIGTERM/SIGHUP landing in it would find `ACTIVE_CHILD_PGID` still zero -- main.rs's own
// handler gates its kill on `if active_pgid != 0` -- and skip killing the subject entirely,
// orphaning it exactly like round 6's own Drop-side finding, just on the other end of a
// `PtyChild`'s lifecycle. SIGHUP specifically was ALSO entirely unhandled before this round (see
// `install_signal_cleanup_handler`'s own doc comment) -- a closed terminal or a dropped SSH
// session delivers SIGHUP, not SIGINT/SIGTERM, and used to bypass this harness's own cleanup
// entirely, subject and temp files both.
//
// Mechanism: block the given `signals` in THIS thread before `spawn_and_register` runs (a
// signal arriving while blocked becomes PENDING, not delivered -- POSIX guarantees delivery is
// only deferred, never dropped), restore the previous mask once it returns. `command.spawn()`'s
// own internal fork() inherits the blocked mask into the child -- confirmed empirically, not
// assumed, with a standalone probe (`cat /proc/self/status` in a freshly spawned child showed
// the parent's own blocked SIGUSR1 still set in `SigBlk`, with no `pre_exec` of any kind
// touching it): `std::process::Command` does NOT reset the signal mask on its own, unlike its
// well-documented SIGPIPE disposition handling, which is a DIFFERENT concern (disposition, not
// mask) this same gap does not extend to. `spawn_impl`'s own `pre_exec` closure explicitly
// unblocks the identical set before its own `execve` for exactly this reason -- without it, the
// SUBJECT itself (ress/less) would start with these three signals permanently blocked, silently
// unable to receive them at all for as long as it ran.
//
// `sigprocmask`, not `pthread_sigmask`, in THIS (parent-side) call specifically -- derived, not
// assumed: POSIX documents `sigprocmask`'s behavior as unspecified in a multi-threaded process
// (`pthread_sigmask` is the portable, thread-safe equivalent), and `ress-perf`'s own TEST binary
// genuinely is multi-threaded under `cargo test`'s thread-parallel model even though the
// PRODUCTION binary is not. On Linux specifically -- this harness's only supported platform by
// its own explicit gate (`check_preconditions`) -- `sigprocmask` and `pthread_sigmask` share the
// identical underlying per-thread syscall, so this is a deliberate, disclosed reliance on
// Linux's own behavior rather than a portability gap silently introduced: consistent with this
// crate's existing Linux-only scope everywhere else (raw `/proc` reads, `killpg`/pty semantics),
// not a new one.
//
// A black-box test asserting only the two end states (blocked beforehand, unblocked afterward)
// cannot tell this ordering apart from a buggy one that, say, unblocks before
// `register_active_child` runs inside the closure -- both produce the identical final mask
// state regardless. Injecting `spawn_and_register` as a closure lets a test OBSERVE whether a
// signal is still blocked FROM INSIDE it, at the exact moment a real spawn-then-register would
// be running (see this module's own tests).
pub(crate) fn spawn_window_blocked<T>(
    signals: &libc::sigset_t,
    spawn_and_register: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut previous_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, signals, &mut previous_mask) } != 0 {
        return Err(last_os_error_context("sigprocmask(SIG_BLOCK) failed"));
    }
    let result = spawn_and_register();
    // restored unconditionally, on every path out of `spawn_and_register` (success or error) --
    // a failed spawn must not leave this thread's own signal mask permanently altered.
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut()) };
    result
}

pub struct PtyChild {
    child: std::process::Child,
    master: MasterFd,
    pid: i32,
    // set once try_wait observes an exit — Drop must not signal a reaped pid: the kernel is
    // free to recycle it, and a killpg at that point could hit an unrelated process.
    reaped: bool,
    // found in PR #44 pass 6 S2 (#6, a user-review finding): WHICH atomic this instance's own
    // registration/clear/peek calls all go through -- `spawn` (the real production entry point)
    // always sets this to `&ACTIVE_CHILD_PGID`, but a test can instead spawn against its own
    // per-test `'static` local via `spawn_with_registry`, below, so that instance can never
    // write to (or be corrupted by) the real global at all. See `ACTIVE_CHILD_PGID_TEST_LOCK`'s
    // own doc comment, this module's own tests, for why a lock alone could not fix this: it only
    // ever serialized tests that VOLUNTARILY took it, and codex confirmed real spawns elsewhere
    // in this same file that did not.
    registry: &'static std::sync::atomic::AtomicI32,
    // found in PR #44 pass 6 (codex pty.rs:1748): the same injection shape as `registry` above,
    // for the same reason -- `spawn` (production) always sets this to
    // `&PEEK_THEN_MAYBE_REAP_CALLS`, but a test that needs to observe ONLY its own instance's
    // contribution to `peek_then_maybe_reap`'s call count (not a process-wide total racing every
    // concurrently-running test's identical default) can inject a private counter instead. See
    // `peek_then_maybe_reap`'s own doc comment for the full derivation.
    reap_call_counter: &'static std::sync::atomic::AtomicU64,
}

impl PtyChild {
    /// Spawns `argv[0]` (already resolved — `env_clear` semantics leave the child with no PATH
    /// to search) against a fresh pty sized `(rows, cols)`, with exactly the environment in
    /// `env` (no inheritance from this process).
    pub fn spawn(
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
        winsize: (u16, u16),
    ) -> anyhow::Result<PtyChild> {
        Self::spawn_impl(
            argv,
            env,
            winsize,
            None,
            &ACTIVE_CHILD_PGID,
            &PEEK_THEN_MAYBE_REAP_CALLS,
        )
    }

    // found in PR #44 pass 6 S2 (#6): spawns exactly like `spawn`, but against an INJECTED
    // registry instead of the real `ACTIVE_CHILD_PGID` -- a test-only entry point so a test can
    // spawn a genuine, fully-real `PtyChild` (real fork/exec, real pty, real signal delivery)
    // whose OWN registration/clear/peek traffic never touches process-wide shared state at all,
    // the same "exercise real production code, just not through the shared static" reasoning
    // `active_child_registration_sets_and_clears_by_compare_exchange`'s own doc comment already
    // established for the pure functions underneath this -- extended here to cover the REAL
    // call sites (`spawn_impl`, `try_wait_with_reap`, `Drop::drop`) too, not just the logic they
    // call. `registry` must be `'static` (not a borrow tied to the test function's own stack
    // frame) so `PtyChild` itself needs no lifetime parameter -- a `static` declared INSIDE a
    // test function body is its own distinct process-wide symbol, isolated from every OTHER
    // test's own local `static` of the same shape, which is what makes this safe under `cargo
    // test`'s thread-parallel model without a lock: two different statics can never alias, so
    // two tests using two different ones can never race each other, regardless of scheduling.
    // The reap-call counter stays the real, unconditional default here (`&PEEK_THEN_MAYBE_REAP_
    // CALLS`) -- only `spawn_with_registry_and_reap_counter`, below, injects a different one; the
    // ~20 existing callers of THIS constructor don't care about that counter at all, so leaving
    // its signature unchanged avoids touching every one of them for an unrelated concern.
    #[cfg(test)]
    fn spawn_with_registry(
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
        winsize: (u16, u16),
        registry: &'static std::sync::atomic::AtomicI32,
    ) -> anyhow::Result<PtyChild> {
        Self::spawn_impl(
            argv,
            env,
            winsize,
            None,
            registry,
            &PEEK_THEN_MAYBE_REAP_CALLS,
        )
    }

    // found in PR #44 pass 6 (codex pty.rs:1748): sibling of `spawn_with_registry` above,
    // additionally injecting a private reap-call counter -- see `peek_then_maybe_reap`'s and
    // `PtyChild::reap_call_counter`'s own doc comments for why the process-wide default alone is
    // not race-free for the ONE test that needs to observe it directly
    // (`drop_reap_loop_calls_the_shared_peek_then_maybe_reap_helper`). A new sibling rather than
    // adding this parameter to `spawn_with_registry` itself, so the ~20 existing callers of that
    // constructor, none of which care about this counter, stay untouched.
    #[cfg(test)]
    fn spawn_with_registry_and_reap_counter(
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
        winsize: (u16, u16),
        registry: &'static std::sync::atomic::AtomicI32,
        reap_call_counter: &'static std::sync::atomic::AtomicU64,
    ) -> anyhow::Result<PtyChild> {
        Self::spawn_impl(argv, env, winsize, None, registry, reap_call_counter)
    }

    /// Spawns exactly like `spawn`, but additionally captures an exec-adjacent timestamp --
    /// found necessary in PR #44's round-4 review: a PARENT-side stamp taken right after
    /// `spawn()` returns (round 3's own fix) can still be taken AFTER the child has already
    /// exec'd and even logged its own first paint, since `Command::spawn`'s Unix path
    /// internally waits on its own sync pipe for the child's exec to succeed before returning
    /// -- meaning the parent's "as soon as possible" is already sometime after the true exec
    /// instant, by an amount that grows with host scheduling contention. There is exactly one
    /// place a timestamp can be genuinely exec-adjacent: taken by the CHILD itself, inside
    /// `pre_exec`, as the very last thing before `Command`'s own internal `execve` -- literally
    /// where perf.sh's own deleted wrapper stamped (`$EPOCHREALTIME` immediately before its own
    /// `exec "$@"`), so this is exact parity with the retired script's mechanism, not merely an
    /// approximation of it.
    ///
    /// Mechanism: a `pipe2(O_CLOEXEC)` pair created before fork; the write end is captured into
    /// the SAME `pre_exec` closure `spawn_impl` already builds, and written to (16 bytes: a
    /// `CLOCK_REALTIME` `timespec`'s `tv_sec`/`tv_nsec`, each an 8-byte native-endian `i64` on
    /// this Linux-only, 64-bit-assumed harness) as the very last step before the closure
    /// returns `Ok(())` -- both `clock_gettime` and `write` are on POSIX's `signal-safety(7)`
    /// list, which is the hard constraint here (`pre_exec` runs post-fork, in a child that is a
    /// fork of a possibly-multithreaded parent -- only signal-safe calls are permitted; no
    /// allocation, since a `Vec`/`String` could deadlock on a lock some other, now-frozen
    /// "ghost" thread held at the moment of fork). `O_CLOEXEC` on both pipe ends means the read
    /// end never leaks into the CHILD's own exec (it isn't even referenced inside `pre_exec`)
    /// and the write end is additionally, explicitly closed inside `pre_exec` itself --
    /// belt-and-suspenders, matching this same closure's existing explicit-close discipline for
    /// `slave_fd`/`master_fd` rather than relying on `O_CLOEXEC` alone. The parent reads the
    /// stamp back (bounded, non-blocking, `read_exec_stamp`'s own doc comment) after `spawn_impl`
    /// returns.
    ///
    /// Returns `None` for the stamp -- never an `Err` -- if anything about capturing it goes
    /// wrong (the pipe closed early, a partial write, a read that never completed): this
    /// function's own job is only to REPORT what it managed to capture, honestly, never to
    /// decide what a missing capture means for the sample as a whole.
    ///
    /// found in PR #44 pass 5 (sweep): that "report, don't decide" contract is still exactly
    /// right at THIS layer -- corrected here only to stop implying it settles the question for
    /// every caller. `runner.rs`'s own `tracing_precise_ms` is the one that actually decides:
    /// since pass 3, a `None` reaching it for a TRACED, STILL-LIVE subject is escalated into a
    /// hard failure (the stamp pipe is harness-owned plumbing, so its absence there is broken
    /// instrumentation, not a per-sample race) -- an untraced subject or a dead one still stays
    /// opportunistic, matching this function's own original framing. This function has no way
    /// to know which case it is in (it doesn't know whether `subject.tracing` was even true, or
    /// whether the child is still alive by the time its caller looks), so it correctly leaves
    /// that call to the one place that does.
    pub fn spawn_capturing_exec_stamp(
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
        winsize: (u16, u16),
    ) -> anyhow::Result<(PtyChild, Option<std::time::SystemTime>)> {
        let mut pipe_fds: [libc::c_int; 2] = [-1, -1];
        if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(last_os_error_context("pipe2 failed"));
        }
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);
        // pipe2's own fd allocation follows ordinary lowest-available-fd semantics -- if
        // ress-perf itself were launched with fd 0/1/2 already closed (the identical daemonized/
        // CI-wrapper context spawn_impl's own master_fd guard below already documents), either
        // end could land there. The write end landing on 0/1/2 is the sharp danger: pre_exec's
        // dup2(slave_fd, 0..2) calls run AFTER this pipe is created and BEFORE write_exec_stamp
        // fires, so a write end sitting at fd 0/1/2 would already have been silently overwritten
        // by the pty slave by the time write_exec_stamp runs -- the stamp write would land IN THE
        // PTY instead of the pipe, visible to the subject (and to Screen's own vt100 parser) as a
        // handful of raw, mostly non-printable bytes prepended to its own output, which --
        // Predicate::Painted only checks whether row 0 is non-empty -- could register as a false
        // "already painted" before the subject itself ever produces a byte. The read end has no
        // equivalent CHILD-side danger (pre_exec's closure never references read_fd at all, so a
        // dup2 clobbering the child's own copy of that fd number is inert -- nothing there ever
        // reads from it), but it shares the identical PARENT-side one: this process's own
        // `eprintln!`/`println!` calls go to fd 2/1 by number, not through a handle that would
        // notice a pipe end quietly occupying that slot in the same closed-stdio launch context.
        // No such call currently falls inside this pipe's own open window (checked: spawn_impl
        // and its callees emit nothing to stdout/stderr), but "nothing happens to try it today"
        // is not a fix -- both ends get the identical guard, moved off 0/1/2 before anything else
        // touches them. Same disclosure standard as the master-fd guard: this exact launch
        // context (ress-perf started with closed stdio) is not exercised by an automated test
        // here, for the same fd-0/1/2-safety reasons that guard's own untestability was already
        // accepted in round 3 -- verified instead by code review plus the safe no-op branch this
        // module's own tests do cover (see `ensure_fd_above_stdio`'s own tests).
        let read_fd = match ensure_fd_above_stdio(read_fd) {
            Ok(fd) => fd,
            Err(err) => {
                unsafe { libc::close(write_fd) };
                return Err(err);
            }
        };
        let write_fd = match ensure_fd_above_stdio(write_fd) {
            Ok(fd) => fd,
            Err(err) => {
                unsafe { libc::close(read_fd) };
                return Err(err);
            }
        };
        let spawn_result = Self::spawn_impl(
            argv,
            env,
            winsize,
            Some(write_fd),
            &ACTIVE_CHILD_PGID,
            &PEEK_THEN_MAYBE_REAP_CALLS,
        );
        // the parent's own copy of the write end, closed unconditionally right here regardless
        // of spawn_impl's outcome -- see spawn_impl's own comment at its slave_fd close for why
        // this fd's cleanup stays entirely in this one place rather than split across both
        // functions' several exit paths.
        unsafe { libc::close(write_fd) };
        let child = match spawn_result {
            Ok(child) => child,
            Err(err) => {
                unsafe { libc::close(read_fd) };
                return Err(err);
            }
        };
        let stamp = read_exec_stamp(read_fd);
        unsafe { libc::close(read_fd) };
        Ok((child, stamp))
    }

    // shared by `spawn` (exec_stamp_write_fd: None) and `spawn_capturing_exec_stamp` (Some) --
    // the ENTIRE openpty/Command/pre_exec sequence is identical either way except for the one
    // extra write `exec_stamp_write_fd` requests, so it lives here once rather than duplicated.
    fn spawn_impl(
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
        winsize: (u16, u16),
        exec_stamp_write_fd: Option<libc::c_int>,
        registry: &'static std::sync::atomic::AtomicI32,
        reap_call_counter: &'static std::sync::atomic::AtomicU64,
    ) -> anyhow::Result<PtyChild> {
        let program = argv
            .first()
            .ok_or_else(|| anyhow::anyhow!("argv must contain at least a program path"))?;
        let (rows, cols) = winsize;
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        if rc != 0 {
            return Err(last_os_error_context("openpty failed"));
        }
        // found in PR #44 pass 8 fold re-review (codex P2): the identical guard the exec-stamp
        // pipe's own read_fd/write_fd already get (see spawn_capturing_exec_stamp's own doc
        // comment for the shared danger), missing here despite that comment's own claim that
        // "the master-fd guard" already existed -- it did not, for the PARENT's own sake:
        // pre_exec's own `if master_fd > 2 { close }` below only protects the CHILD's just-
        // established stdio from an accidental close, it does nothing for the PARENT's own
        // continued use of master_fd for the rest of this PtyChild's lifetime (every read/write
        // against the pty master goes through it). Without this, a closed-stdio launch
        // (ress-perf itself started with fd 1 or 2 unavailable) could let openpty hand back
        // master_fd AT that slot, making this process's own stdout/stderr alias the pty master --
        // every subsequent pty read/write would then double as this process's own stdout/stderr
        // traffic, and vice versa. Relocated before `set_nonblocking` runs, so that call (and
        // everything after it) operates on the fd this `PtyChild` will actually keep, not one
        // about to be closed. Same disclosure standard as the exec-stamp pipe's own guard: this
        // exact launch context is not exercised by an automated test here (round 3's own
        // untestability call, unchanged) -- verified instead by code review plus
        // `ensure_fd_above_stdio`'s own existing tests covering the underlying mechanism.
        let master_fd = match ensure_fd_above_stdio(master_fd) {
            Ok(fd) => fd,
            Err(err) => {
                unsafe { libc::close(slave_fd) };
                return Err(err);
            }
        };
        if let Err(err) = set_nonblocking(master_fd) {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(err.context("failed to set pty master non-blocking"));
        }
        let mut command = std::process::Command::new(program);
        command.args(&argv[1..]);
        command.env_clear();
        for (key, value) in env {
            command.env(key, value);
        }
        // see `spawn_window_blocked`'s own doc comment for the full window this closes --
        // captured here (before pre_exec, before spawn) so the SAME set backs both the parent's
        // own block below and the child's own mask restoration inside pre_exec.
        let spawn_window_signals = spawn_window_signal_set();
        // found in PR #44 pass 5 (A2, a codex P2): the child's own pre_exec used to SIG_UNBLOCK
        // `spawn_window_signals` unconditionally -- correct for undoing THIS function's own
        // spawn-window block, but wrong whenever the CALLER had independently, already blocked
        // one of these same three signals (SIGINT/SIGTERM/SIGHUP) for some unrelated reason
        // before ever calling into this spawn machinery at all: an unconditional unblock cannot
        // distinguish "blocked only by this window" from "was already blocked before it," so it
        // clobbered the caller's own pre-existing intent for any signal that happened to overlap
        // this function's own set. `spawn_window_blocked`'s own parent-side restoration already
        // gets this right (it captures the true pre-window mask via `sigprocmask(SIG_BLOCK, ...,
        // &old)` and restores exactly that, not a blanket unblock) -- the child needs the
        // identical mask, not a separately-reasoned approximation of it. Queried here, BEFORE
        // `pre_exec` is even built (its own closure captures by `move` and is fully constructed
        // well before `spawn_window_blocked` is called below, so there is no later point to
        // reach into `spawn_window_blocked`'s own internal capture from) -- a query-only
        // `sigprocmask` call (`set` null leaves the mask unchanged, the standard idiom this
        // file's own tests already use), capturing the SAME value `spawn_window_blocked`'s own
        // capture is about to take a few lines below (nothing signal-related runs in between,
        // and signal masks are per-thread state, so a concurrently running test thread's own
        // mask cannot perturb this one even under `cargo test`'s thread-parallel model).
        let pre_window_mask: libc::sigset_t = unsafe {
            let mut mask = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
            mask
        };
        // pre_exec does all of the child's stdio wiring itself (setsid, dup2, TIOCSCTTY) rather
        // than going through Command's stdin/stdout/stderr builder methods — ported verbatim
        // from smoke.rs's proven ordering. every libc call here is async-signal-safe, which
        // pre_exec's contract requires (this closure runs after fork, before exec).
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(slave_fd, 0) == -1
                    || libc::dup2(slave_fd, 1) == -1
                    || libc::dup2(slave_fd, 2) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                // guarded the identical way as slave_fd just above, for the identical reason:
                // if ress-perf itself were launched with fd 0/1/2 already closed (a daemonized
                // or CI-wrapper context), openpty's own open()-family allocation could hand
                // back one of those low numbers for master_fd -- and by this point the dup2
                // calls above have already made 0/1/2 the child's real stdio, so an
                // unconditional close here could sever the wiring this closure just built
                // instead of closing the parent-side copy it's meant to close.
                if master_fd > 2 {
                    libc::close(master_fd);
                }
                // undoes the parent's own spawn-window block just below -- fork() inherits the
                // parent's signal mask verbatim, and exec() does NOT reset it (confirmed
                // empirically, not assumed -- see `spawn_window_blocked`'s own doc comment), so
                // without this the SUBJECT itself (ress/less) would start with
                // SIGINT/SIGTERM/SIGHUP permanently blocked -- silently unable to receive them
                // at all, for as long as it runs.
                //
                // found in PR #44 pass 5 (A2): RESTORES `pre_window_mask` (`SIG_SETMASK`), does
                // NOT unblock `spawn_window_signals` (the old `SIG_UNBLOCK` shape). The two look
                // equivalent but are not: an unconditional unblock of these three signals cannot
                // tell "blocked only by this function's own spawn window" apart from "was
                // independently already blocked by the CALLER, for some unrelated reason, before
                // this function was ever called" -- it clobbers the latter, silently overriding
                // the caller's own intent for any of these three signals it happened to have
                // already blocked. `pre_window_mask` (queried just above, before this closure
                // was even built) is exactly the mask this thread had BEFORE this function's own
                // spawn-window block touched anything -- restoring it undoes precisely what this
                // window itself added, for each of the three signals independently, while
                // leaving every OTHER ambient block (these three or otherwise) exactly as the
                // caller had it. `sigprocmask`, not `pthread_sigmask`: a forked child has exactly
                // one thread by construction, so the multi-threaded-process ambiguity POSIX
                // documents for `sigprocmask` (see the parent-side call's own comment) does not
                // apply here at all -- and it is on POSIX's signal-safety(7) list either way, the
                // hard constraint `pre_exec`'s own contract imposes.
                if libc::sigprocmask(libc::SIG_SETMASK, &pre_window_mask, std::ptr::null_mut())
                    == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                // the very last step, right before this closure returns and Command's own
                // execve follows -- see spawn_capturing_exec_stamp's own doc comment for the
                // full mechanism and its async-signal-safety derivation.
                if let Some(write_fd) = exec_stamp_write_fd {
                    write_exec_stamp(write_fd);
                    libc::close(write_fd);
                }
                Ok(())
            });
        }
        // the ENTIRE spawn-then-register sequence runs inside the blocked window -- see
        // `spawn_window_blocked`'s own doc comment for the full derivation of why registration
        // specifically must stay inside it, not just the spawn call.
        spawn_window_blocked(&spawn_window_signals, || {
            let spawn_result = command.spawn();
            // the parent's own copy of the slave must not outlive spawn (see smoke.rs) — held
            // open, it would keep the slave-side refcount above zero independent of the child,
            // masking the child's real exit from the master-side reader. exec_stamp_write_fd's
            // own parent-side copy is NOT closed here (unlike slave_fd) -- ownership of that one
            // stays entirely with spawn_capturing_exec_stamp, since this function has earlier
            // return paths (a malformed argv, a failed openpty) that never reach this point at
            // all, and splitting cleanup across two functions across those paths risks exactly
            // the leak (or a double-close on the paths that DO reach here) that keeping it in
            // one place avoids.
            unsafe { libc::close(slave_fd) };
            let child = match spawn_result {
                Ok(child) => child,
                Err(err) => {
                    unsafe { libc::close(master_fd) };
                    return Err(anyhow::Error::new(err).context("failed to spawn pty child"));
                }
            };
            let pid = child.id() as i32;
            // see ACTIVE_CHILD_PGID's own doc comment -- registered here, on success, right
            // before this constructor's only return, and STILL inside the blocked window: a
            // SIGINT/SIGTERM/SIGHUP arriving between `command.spawn()` returning (a real, live,
            // already-setsid'd child now exists) and this registration completing would
            // otherwise find ACTIVE_CHILD_PGID still zero and orphan exactly the subject this
            // whole mechanism exists to protect -- see `spawn_window_blocked`'s own doc comment
            // for the full derivation. `registry`, not the hardcoded global directly (found in
            // PR #44 pass 6 S2, #6) -- production `spawn`/`spawn_capturing_exec_stamp` both
            // still pass `&ACTIVE_CHILD_PGID` here, unchanged; only a test that explicitly opts
            // in via `spawn_with_registry` ever sees anything else.
            register_active_child(registry, pid);
            Ok(PtyChild {
                child,
                master: MasterFd(master_fd),
                pid,
                reaped: false,
                registry,
                reap_call_counter,
            })
        })
    }

    /// Performs one non-blocking read against the pty master, appending any data to `buf`.
    ///
    /// Linux pty masters report the child's stdio going away as `EIO`, not a `0`-byte read —
    /// both are treated as `Eof` here (see smoke.rs's drain-thread comment for the `EIO`
    /// behavior, confirmed against a real pty in that test).
    ///
    /// found in PR #44 pass 7 (P7-A/B): `#[cfg(test)]` now -- `poll_predicate_with_waiter`'s own
    /// admission logic was this method's last production caller, and it no longer uses it (see
    /// `PtyTransport`'s own doc comment, runner.rs: the trait deliberately never exposes an
    /// unbounded read at all). Still genuinely needed by test setup code that manually drains an
    /// initial paint before a policy test begins -- unrelated to admission, just convenient here.
    #[cfg(test)]
    pub fn read_available(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<ReadStatus> {
        self.read_available_impl(buf, usize::MAX)
    }

    // found in PR #44 pass 3: `poll_predicate`'s own deadline-branch drain used to call
    // `read_available` (below) directly, unbounded by anything except a chunk-COUNT budget
    // (`runner.rs`'s own `DRAIN_BUDGET`) -- which bounds how many TIMES it reads, not how much it
    // reads. Each read frees pty kernel-buffer space, which can unblock a writer stalled
    // mid-write on a full buffer, letting it push MORE bytes into this fd -- bytes that arrived
    // only because the drain's own earlier reads made room, strictly AFTER the deadline this
    // drain exists to serve. `max_bytes` closes that at the request itself, not just after the
    // fact: capping what THIS call asks the kernel for is the only way to guarantee it can never
    // return bytes beyond a caller-chosen boundary, regardless of how much the kernel buffer
    // happens to hold or how the writer's own unblocking races this call -- a post-hoc "stop once
    // N total bytes have been observed" check would still let a single `read()` call that lands
    // after an unblock return MORE than N in one shot (a `read()` returns up to the length it was
    // asked for, not the length some outside accounting expected).
    //
    // `max_bytes == 0` returns `WouldBlock` without a syscall at all, deliberately never a `read`
    // request of length `0`: POSIX defines a zero-length `read(2)` to report `0`, the exact same
    // shape `read_available_impl` treats as `Eof` -- issuing one here would misreport "caller's
    // budget is exhausted" as "the far end hung up", which it is not.
    pub fn read_available_bounded(
        &mut self,
        buf: &mut Vec<u8>,
        max_bytes: usize,
    ) -> anyhow::Result<ReadStatus> {
        if max_bytes == 0 {
            return Ok(ReadStatus::WouldBlock);
        }
        self.read_available_impl(buf, max_bytes)
    }

    fn read_available_impl(
        &mut self,
        buf: &mut Vec<u8>,
        max_len: usize,
    ) -> anyhow::Result<ReadStatus> {
        let mut chunk = [0u8; 65536];
        let want = max_len.min(chunk.len());
        let n = unsafe { libc::read(self.master.0, chunk.as_mut_ptr() as *mut libc::c_void, want) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            return Ok(ReadStatus::Data(n as usize));
        }
        if n == 0 {
            return Ok(ReadStatus::Eof);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // EWOULDBLOCK == EAGAIN on Linux; matching one covers both.
            Some(libc::EAGAIN) => Ok(ReadStatus::WouldBlock),
            Some(libc::EIO) => Ok(ReadStatus::Eof),
            _ => Err(anyhow::Error::new(err).context("pty master read failed")),
        }
    }

    // reports how many bytes are CURRENTLY queued, unread, on the master -- via `FIONREAD`,
    // which (like `waitid`'s own `WNOWAIT` elsewhere in this file) answers without consuming
    // anything: a peek, not a read. Exists for `runner.rs`'s deadline-branch drain to snapshot
    // "how much genuinely arrived before the deadline fired" and hand that count to
    // `read_available_bounded` above, so the drain it drives can never accept a byte that only
    // became readable because the drain's own reads unblocked a stalled writer.
    pub fn queued_read_bytes(&self) -> anyhow::Result<usize> {
        let mut n: libc::c_int = 0;
        let rc = unsafe { libc::ioctl(self.master.0, libc::FIONREAD, &mut n as *mut libc::c_int) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(anyhow::Error::new(err).context("FIONREAD on pty master failed"));
        }
        // the kernel does not report a negative count for FIONREAD in practice, but the field is
        // a plain `c_int` with no type-level guarantee of that -- clamped rather than trusted,
        // the same reasoning `clear_registration_if_the_child_has_exited` already applies to a
        // kernel-populated struct elsewhere in this file.
        Ok(n.max(0) as usize)
    }

    // found in PR #44 pass 7 (P7-A/B, the reviewer's own deadline-admission invariant 2): reports
    // whether the pty master's read side has hung up -- decoupled from `queued_read_bytes` above
    // ("is there data queued") and from a `read()`'s own `Eof` outcome (which requires attempting
    // a read at all). A non-blocking, zero-timeout `ppoll` peek, the same primitive
    // `wait_for_readable` below already uses, just inspecting `revents` for `POLLHUP` directly
    // instead of collapsing `POLLIN`/`POLLHUP` into one undifferentiated `Readable`. A pty master
    // reports `POLLHUP` once every reference to the slave side has closed, independent of whether
    // any unread bytes remain buffered -- this is what lets a caller detect a dead subject on a
    // snapshot of zero queued bytes WITHOUT ever reading: `runner.rs`'s own zero-snapshot
    // admission path checks this (and `try_wait`) instead of issuing a probe read, so a snapshot
    // that proves nothing is queued can never be the vehicle for smuggling in bytes that arrived
    // after it was taken -- see `poll_predicate_with_waiter`'s own doc comment for the full
    // derivation of why that mattered.
    pub fn hung_up(&self) -> anyhow::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.master.0,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let rc = unsafe { libc::ppoll(&mut pollfd, 1, &timeout_ts, std::ptr::null()) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // a spurious EINTR on a ZERO-duration poll has nothing to retry against (there is no
            // remaining timeout to recompute, unlike `wait_for_readable`'s own loop) -- reporting
            // "not observed hung up yet" is safe here: the caller's own next iteration re-peeks,
            // and a genuine hangup that outlives one interrupted syscall will still be there to
            // observe then.
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(
                anyhow::Error::new(err).context("non-blocking POLLHUP peek on pty master failed")
            );
        }
        Ok(pollfd.revents & libc::POLLHUP != 0)
    }

    // found in PR #44 pass 5 commit 3: exposes the bare fd number (a `Copy` `c_int`, not a
    // reference) so `runner.rs`'s own poll loop can pass it to `wait_for_readable` below without
    // holding any borrow of `self` across that call -- the injected waiter closure this loop uses
    // (see `poll_predicate_with_sleeper`'s own doc comment) takes the fd as a plain argument for
    // exactly this reason: closing over `&PtyChild` instead would alias against the SAME loop's
    // own direct, concurrent `&mut` use of `child` for `read_available`/`try_wait`.
    pub(crate) fn raw_master_fd(&self) -> libc::c_int {
        self.master.0
    }

    /// Writes `bytes` to the pty master (delivered to the child as keyboard input), retrying
    /// past transient `EAGAIN` on the non-blocking fd until every byte is accepted.
    pub fn write_keys(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut written = 0usize;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.master.0,
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            if n > 0 {
                written += n as usize;
                continue;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            return Err(anyhow::Error::new(err).context("pty master write failed"));
        }
        Ok(())
    }

    /// Non-blocking poll for the child's exit status; `Ok(None)` means still running.
    ///
    /// found in PR #44 pass 5 (round-13-review's own A1, a P1): round 9 cleared
    /// `ACTIVE_CHILD_PGID` the moment an exit was OBSERVED rather than only at `Drop`'s own much
    /// later teardown; pass 3 added the `WNOWAIT` peek below so the clear itself happens before
    /// any reap, while the zombie is still provably unrecyclable. Both rounds still left the
    /// SAME shape of gap, just progressively narrower: after a NEGATIVE peek (child still
    /// running as far as that one syscall could tell), the code fell through UNCONDITIONALLY
    /// into the real, consuming `std::process::Child::try_wait` below -- and if the child died
    /// in the tiny gap between the peek returning and that next call's own internal `waitpid`
    /// actually running, THAT call would observe and reap the exit itself, with the
    /// registration still armed at the exact moment of the reap. Narrower than the original
    /// bug (bounded by two back-to-back syscalls rather than arbitrary surrounding code), but
    /// the identical class of window: a signal landing in it still sees a stale, already-
    /// recyclable pid.
    ///
    /// Closed at the source by making the peek the ONLY liveness oracle, in a real three-state
    /// protocol rather than an ordering convention: **Armed** (registration set, nothing
    /// observed yet) -> **Observed** (a POSITIVE `WNOWAIT` peek found the exit and cleared the
    /// registration, in the same instant) -> **Reaped** (the consuming wait, legal ONLY here,
    /// since the registration is already gone by construction -- a signal racing the ACTUAL
    /// kernel-level reap that follows finds no registration to act on, not a stale one). A
    /// NEGATIVE peek returns `Ok(None)` immediately, in the SAME call, without ever reaching
    /// the consuming wait -- there is no state that positive-observation-then-consumption can
    /// race against, because consumption never happens without a positive observation
    /// immediately preceding it in the same call.
    ///
    /// The cost, stated plainly rather than left implicit: death-observation latency can grow
    /// by up to one poll tick. A child that dies in the instant right after a negative peek is
    /// NOT retried within this same call -- the next genuine exit observation waits for the
    /// next call to `try_wait` (in production, `poll_predicate`'s own loop, at most
    /// `POLL_INTERVAL` later). That is the correctness/latency trade this protocol makes,
    /// deliberately: a bounded, honestly-documented delay in noticing a death, in exchange for
    /// zero window at all for the signal-handler race, rather than an ever-narrower but
    /// never-actually-zero one.
    ///
    /// Once `reaped`, every later call skips the peek entirely (nothing new to observe -- the
    /// real reap already happened) and goes straight to `std::process::Child::try_wait`, whose
    /// own cached-status behavior (confirmed empirically in pass 3) makes repeat calls after
    /// the real reap correct on their own, with no further `ACTIVE_CHILD_PGID` involvement.
    ///
    /// found in PR #44 pass 6 S2 (#5, a user-review finding): `reaped` used to be set on ANY
    /// `Some(result)` here, including a transient reap FAILURE (`EINTR`, say) that proves
    /// nothing about whether the child actually exited. See `classify_reap_attempt`'s own doc
    /// comment for the fix and why it matters (a wrongly-`reaped` child makes a later `Drop`
    /// skip its own `kill()` entirely). Delegates to `try_wait_with_reap`, below, which takes
    /// the consuming reap as an injectable seam so a test can prove the retry deterministically.
    pub fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        self.try_wait_with_reap(&mut std::process::Child::try_wait)
    }

    // found in PR #44 pass 6 S2 (#5): generalizes `try_wait`'s own consuming reap into an
    // injectable seam, the same "pass the bare function item" shape `poll_predicate`/
    // `poll_predicate_with_waiter` (runner.rs) already established for `waiter`/`fionread`/
    // `now` -- `std::process::Child::try_wait` is already a plain `fn(&mut Child) ->
    // io::Result<Option<ExitStatus>>`, so the real `try_wait` above passes it directly, no
    // closure needed. Exists so a test can inject a SCRIPTED reap outcome (an `Err` standing in
    // for a real transient failure like `EINTR`, which cannot be forced from a real syscall
    // deterministically -- signal-delivery timing inside a specific blocking syscall is exactly
    // the scheduler-timing race this crate's own rule forbids leaning on) and observe how
    // `self.reaped` responds, against a genuine spawned-and-killed child -- see
    // `try_wait_stays_armed_and_retries_past_an_injected_transient_reap_error`, this module's
    // own tests.
    fn try_wait_with_reap(
        &mut self,
        reap: &mut dyn FnMut(
            &mut std::process::Child,
        ) -> std::io::Result<Option<std::process::ExitStatus>>,
    ) -> anyhow::Result<Option<std::process::ExitStatus>> {
        if self.reaped {
            // already reaped on an earlier call: nothing new to observe, no peek, no
            // registration involvement -- std's own cached-status behavior (confirmed
            // empirically in pass 3) makes repeat calls after the real reap correct on their
            // own.
            return reap(&mut self.child)
                .map_err(|err| anyhow::Error::new(err).context("try_wait on pty child failed"));
        }
        let pid = self.pid;
        let registry = self.registry;
        let reap_call_counter = self.reap_call_counter;
        let child = &mut self.child;
        let outcome = peek_then_maybe_reap(
            reap_call_counter,
            || clear_registration_if_the_child_has_exited(registry, pid),
            || {
                // Observed -> Reaped: the peek that authorized this closure just cleared the
                // registration in the same instant it observed the exit -- checked, not merely
                // trusted, the same "own the answer" discipline this file already applies to
                // `siginfo_t`'s own unspecified-on-miss contents.
                debug_assert_ne!(
                    registry.load(std::sync::atomic::Ordering::SeqCst),
                    pid,
                    "the registration must already be cleared by the time the consuming wait \
                     runs -- the positive peek that authorized this closure clears it in the \
                     same instant it observes the exit"
                );
                reap(child)
            },
        );
        match outcome {
            // Armed, negative peek: still running as far as this call can tell. No consuming
            // wait was invoked at all -- see this function's own doc comment for why.
            None => Ok(None),
            Some(result) => {
                // found in PR #44 pass 6 S2 (#5): only a CONFIRMED outcome marks this child
                // reaped now -- see `classify_reap_attempt`'s own doc comment. A `Retry`
                // classification leaves `reaped` false, so the NEXT call to `try_wait` (in
                // production, the poll loop's own next iteration) takes the SAME peek-then-reap
                // path again rather than skipping straight to a bare, registration-blind
                // `std::process::Child::try_wait` -- correct because a genuinely-still-armed
                // registration is exactly what a transient failure here leaves behind.
                if matches!(classify_reap_attempt(&result), ReapAttempt::Confirmed) {
                    self.reaped = true;
                }
                result
                    .map_err(|err| anyhow::Error::new(err).context("try_wait on pty child failed"))
            }
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
}

// found in PR #44 pass 5 commit 3: `poll_predicate_with_sleeper`'s own WouldBlock arm used to
// call a blind `std::thread::sleep` for up to one `POLL_INTERVAL` before trying to read again --
// correct, but not the faithful terminal model the scroll-cadence investigation's own
// dose-response evidence called for: `tmux` (perf.sh's own reader) is event-driven, waking the
// instant the subject writes, not on a fixed poll cadence. A free function, not a `PtyChild`
// method, and taking `fd` as a bare argument rather than `&self` -- deliberately, so the closure
// this is passed through as (`runner.rs`'s own injectable waiter seam, generalizing B3's
// recording-sleeper) never needs to hold any borrow of the `PtyChild` it is polling; see
// `raw_master_fd`'s own doc comment for why that aliasing avoidance is the actual reason, not
// just a style preference.
//
// `POLLIN` is the only event requested. `POLLHUP` (and `POLLERR`) are NOT treated as a separate,
// special case, and are NOT errors -- Linux always reports them in `revents` regardless of the
// requested `events` mask when they apply, so a hung-up master fd still makes `ppoll` return
// `Readable` here (rc > 0), exactly like real data arriving does. The very next `read_available`
// call already knows how to turn that into the correct outcome (`ReadStatus::Eof` on a `0`-byte
// read or `EIO`, `read_available_impl`'s own established handling) -- re-deriving that
// distinction here, one layer up, would just be a second, less complete copy of logic this crate
// already has exactly once.
//
// `EINTR` is retried, recomputing the REMAINING time against this call's own deadline each time
// (not restarting the full `timeout` on every retry, which would let repeated signal delivery
// stretch one wait arbitrarily past what its caller asked for) -- the same "bounded by the wall,
// not by how many times something interrupts it" discipline `clamped_poll_sleep` already
// establishes for the sleep this replaces.
//
// Zero-timeout semantics are preserved by construction, not special-cased: `Duration::ZERO`
// converts to a zeroed `timespec` (a non-blocking, check-right-now poll), matching
// `std::thread::sleep(Duration::ZERO)`'s own immediate-return behavior for the call this
// replaces -- no branch needed, the arithmetic already does the right thing at that boundary.
//
// found in PR #44 pass 6 S1 (a user-review finding, #3): `libc::poll` takes its own timeout as a
// bare millisecond `c_int`, which FLOORS -- `Duration::as_millis()` truncates any sub-millisecond
// remainder, so a genuinely-computed `Duration::from_millis(10)` request (after subtracting the
// microseconds of real time already spent computing `deadline` and re-checking `remaining`) was
// measured landing at 9.10-9.27ms actual, every single call, not merely occasionally -- a
// systematic ~1ms undershoot on every wait this function ever makes. Composed with `runner.rs`'s
// own missing `next_checkpoint` clamp (see `clamped_poll_sleep`'s own doc comment), this produced
// the reported ~18ms stability-checkpoint cadence instead of the documented 10ms (docs/perf.md's
// own "fixed-cadence 10ms checkpoint" claim). `libc::ppoll` takes a `libc::timespec` instead --
// nanosecond precision, no truncation -- closing this specific source of drift at the syscall
// itself. `sigmask: std::ptr::null()` preserves `poll`'s own exact semantics (no signal-mask
// modification during the wait); `ppoll` only replaces the OS-level timeout mechanism, nothing
// about signal handling changes.
pub(crate) fn wait_for_readable(
    fd: libc::c_int,
    timeout: std::time::Duration,
) -> anyhow::Result<PollReadiness> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let timeout_ts = libc::timespec {
            tv_sec: remaining.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(remaining.subsec_nanos()),
        };
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::ppoll(&mut pollfd, 1, &timeout_ts, std::ptr::null()) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(anyhow::Error::new(err).context("ppoll on pty master failed"));
        }
        return Ok(if rc == 0 {
            PollReadiness::TimedOut
        } else {
            PollReadiness::Readable
        });
    }
}

// bounds Drop's own reap wait -- see Drop::drop's comment for why an unbounded one is wrong
// for this specific tool.
const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const REAP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

// pids the bounded reap below gave up on (still not reaped after REAP_TIMEOUT), persisted
// across every `PtyChild`'s own Drop -- found necessary in PR #44 pass-2 review: a subject stuck
// in a D-state read is exactly the case the mount leg exists to reach (see the bounded reap's
// own comment), and a LONG mount-testing run can hit it more than once. "Give up and leave it
// for init" (this Drop's own pre-existing behavior) meant every SUCH sample accumulated its own
// zombie, alive for the REST of the run rather than for however long its own D-state condition
// actually persisted -- init only ever reaps them once this WHOLE process finally exits. Every
// SUBSEQUENT sample's own Drop now retries WNOHANG against everything abandoned so far, right
// before doing its own teardown (see `retry_abandoned_reaps`'s own call site below) -- a
// persistent reaper WITHOUT a background thread: this crate never spawns one, so "persistent"
// here means "retried opportunistically, piggybacked on work a later Drop was already doing,"
// not "polled on its own independent schedule." Bounded memory by construction, not by an
// explicit cap: this harness spawns at most one `PtyChild` per sample, sequentially, so the
// number of EVER-abandoned pids across one whole run is bounded by the run's own sample count
// (a handful of scenarios times a handful of runs each), never unbounded growth. A `Mutex`, not
// a raw `Vec` behind `unsafe`: this crate's own production call path is single-threaded, but
// `Mutex` is the ordinary, safe way to get interior mutability in a `static` regardless, and
// costs nothing measurable for a registry this small, touched once per sample teardown.
static ABANDONED_PIDS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

// pure function over an explicit `&Mutex<Vec<i32>>` rather than reaching for ABANDONED_PIDS
// directly -- lets the retry LOGIC be unit-tested against a throwaway local registry (see this
// module's own tests), the same reasoning `register_active_child`/`clear_active_child_if_mine`
// already established for `ACTIVE_CHILD_PGID`. `waitpid(pid, _, WNOHANG)`'s own contract is what
// makes the retry safe to repeat indefinitely: `0` means still running (kept for the next
// retry), a match on `pid` means it just reaped successfully, and `-1`/`ECHILD` means it is no
// longer this process's own child at all (already reaped some other way, or never was one) --
// the LATTER two cases are indistinguishable in terms of "is there anything more to wait for
// here" (no), so both simply drop the pid from the registry. No risk of ever reaping the WRONG
// process even if a pid number gets recycled by the kernel during a long run: `waitpid` only
// ever succeeds against a GENUINE child of the calling process, tracked by the kernel's own
// parent-child relationship, never by number alone -- a recycled number belonging to some
// unrelated process elsewhere on the system would correctly fail with ECHILD here, the same
// "kill(2)/killpg(2) can never hit an unrelated process just because a number was reused" safety
// this codebase already relies on elsewhere for signal delivery.
//
// audited in PR #44 pass 5 (A1's own "audit every consuming-wait call site" instruction): this
// IS a raw, unconditionally consuming `waitpid` with no `WNOWAIT` peek in front of it -- and yet
// it does NOT need the three-state protocol `try_wait`/`Drop`'s own loop now use. A pid only
// ever lands in `abandoned_pids` via `Drop`'s own timeout/give-up path -- WITHIN that one
// `teardown_registered_child` call, the push actually happens BEFORE its own outer,
// unconditional `clear_active_child_if_mine` (the push lives inside the `kill` closure, which
// `teardown_registered_child` runs FIRST; the outer clear only runs once `kill` returns --
// corrected in PR #44 pass-5-review-followup, finding 4, traced against the real call graph
// rather than assumed). The safety property this function depends on holds anyway, from a
// different boundary: `retry_abandoned_reaps` is never called from within that SAME `drop()` --
// only from a SEPARATE, LATER `PtyChild`'s own teardown (see this function's own call site,
// above) -- and `teardown_registered_child` (kill, the abandon-push on timeout, AND the outer
// clear) fully completes before ANY `drop()` call returns. So by the time this function ever
// actually retries a given pid, the `drop()` call that abandoned it is long over, its own outer
// clear has unconditionally already fired, and `ACTIVE_CHILD_PGID` cannot still be pointing at
// it. There is no live registration left to race a signal against here, regardless of when this
// reap actually completes.
fn retry_abandoned_reaps(abandoned_pids: &std::sync::Mutex<Vec<i32>>) {
    let mut abandoned = abandoned_pids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    abandoned.retain(|&pid| {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        rc == 0
    });
}

// pulled apart from Drop's own body specifically to make the ORDERING it enforces
// unit-testable, the same reason `register_active_child`/`clear_active_child_if_mine` were
// extracted in round 4: `kill` runs (when `reaped` is false) BEFORE the registration is
// cleared, never after -- see `ACTIVE_CHILD_PGID`'s own doc comment for why that direction is
// the only safe one. A black-box test that only checks the two end states (registered before,
// cleared after) cannot tell this ordering apart from the reversed, buggy one -- both produce
// the identical final state when nothing interrupts Drop, since the bug is entirely about the
// WINDOW between the two steps, not either steady state. A test can instead inject a probe
// closure for `kill` and observe the atomic's value AT THE MOMENT it runs, pinning the ordering
// itself (see this module's own tests).
fn teardown_registered_child(
    atomic: &std::sync::atomic::AtomicI32,
    pid: i32,
    reaped: bool,
    kill: impl FnOnce(),
) {
    if !reaped {
        kill();
    }
    clear_active_child_if_mine(atomic, pid);
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        // opportunistic cleanup of anything a PREVIOUS sample's own bounded reap gave up on --
        // see ABANDONED_PIDS' own doc comment for why this piggybacks here, at the start of
        // EVERY subsequent teardown, rather than running on a schedule of its own. Unrelated to
        // (and independent of) THIS PtyChild's own teardown below -- ordering between the two
        // does not matter, since neither can affect the other's own outcome.
        retry_abandoned_reaps(&ABANDONED_PIDS);
        let pid = self.pid;
        let reaped = self.reaped;
        let registry = self.registry;
        let reap_call_counter = self.reap_call_counter;
        let child = &mut self.child;
        teardown_registered_child(registry, pid, reaped, || {
            // pgid == pid here because the pre_exec closure called setsid(); ESRCH (already
            // gone) and ECHILD (already reaped elsewhere) are both fine outcomes for a
            // best-effort, idempotent cleanup, so the return value is intentionally discarded.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            // an unbounded, synchronous wait() here would be wrong for THIS tool specifically:
            // the mount leg exists to drive subjects against slow, hung, or dead filesystems, so
            // a child blocked in an uninterruptible (D-state) read is exactly the case this
            // harness is built to reach, not a rare edge case to assume away. SIGKILL only stays
            // PENDING against a D-state process -- the kernel delivers it once the blocking
            // syscall eventually returns, which for a genuinely dead mount could be a very long
            // time -- so an unbounded wait() here could hang the entire harness (every remaining
            // scenario, the whole report) on a single dead-mount sample. Poll WNOHANG instead
            // (`try_wait`'s own contract), bounded by a short deadline. If the child still
            // hasn't been reaped by then, give up and leave the zombie behind: process exit
            // reparents it to init, whose job includes reaping orphaned zombies, and this
            // process's own exit (which follows shortly after every Drop in this binary's
            // actual call sites) removes ress-perf's interest in it regardless -- a zombie only
            // occupies a process-table slot, not a mount/filesystem resource, so leaving one
            // behind briefly costs nothing this harness cares about.
            let deadline = std::time::Instant::now() + REAP_TIMEOUT;
            loop {
                // found in PR #44 pass 5 (A1); deduped in PR #44 pass-5-review-followup (finding
                // 1): this loop used to hand-inline the SAME three-state protocol `try_wait`'s
                // own doc comment derives in full, rather than calling the shared
                // `peek_then_maybe_reap` that codifies it -- two copies of one invariant, with
                // nothing to stop them drifting apart (confirmed a real gap by the reviewer:
                // reverting just THIS site's own peek-gating back to the old
                // unconditional-fallthrough shape left every existing test green, since none of
                // them observed the Drop path's own ordering directly). Routing through the same
                // helper `try_wait` uses means both call sites are now provably the identical
                // implementation, not just the identical intent -- see `peek_then_maybe_reap`'s
                // own doc comment for the full derivation, and
                // `drop_reap_loop_calls_the_shared_peek_then_maybe_reap_helper` (this module's
                // own tests) for the deterministic, Drop-path-specific proof this call site
                // genuinely reaches the shared function.
                let outcome = peek_then_maybe_reap(
                    reap_call_counter,
                    || clear_registration_if_the_child_has_exited(registry, pid),
                    || {
                        // Observed -> Reaped: see `try_wait`'s own identical assertion for why
                        // this must already hold by construction, not merely by convention.
                        debug_assert_ne!(
                            registry.load(std::sync::atomic::Ordering::SeqCst),
                            pid,
                            "the registration must already be cleared by the time the consuming \
                             wait runs -- the positive peek that authorized this closure clears \
                             it in the same instant it observes the exit"
                        );
                        child.try_wait()
                    },
                );
                // found in PR #44 pass 6 S2 (#5, a user-review finding): this used to stop the
                // loop (`return`) on ANY `Err` here, exactly like `try_wait`'s own identical bug
                // -- a transient reap failure (`EINTR`, say) would abandon this child WITHOUT
                // retrying and WITHOUT falling back to `ABANDONED_PIDS` either, since `return`
                // skips the deadline/give-up handling below entirely. Routed through the SAME
                // `classify_reap_attempt` `try_wait_with_reap` now uses -- see its own doc
                // comment -- so both call sites are provably the identical decision, not just
                // the identical intent (the same bar `peek_then_maybe_reap` itself already set
                // for this loop). Only a CONFIRMED outcome ends the loop; a negative peek
                // (`None`) or a `Retry` classification (an unconfirmed `Ok(None)`, or a
                // transient error) both fall through to the same deadline/sleep handling below,
                // exactly as a negative peek always already did.
                //
                // Confirmed empirically, not assumed: reverting just this call site's own
                // conditional back to the pre-fix `match outcome { ... Some(Err(_)) => return,
                // ... }` shape and running the full suite left every existing test green --
                // exactly the same gap pass-5-review-followup finding 1 already documented for
                // this identical call site's own peek-gating fix, for the identical reason (a
                // steady-state, black-box observation of a real Drop cannot distinguish this
                // fix from the bug it replaces; the two only differ on a real, injected-error
                // path this loop cannot take real injected closures to reach). Coverage instead
                // comes from composition, the same shape S1's own path-3 analysis already
                // accepted for an analogous gap: `classify_reap_attempt`'s pure tests prove the
                // classification itself; `drop_reap_loop_calls_the_shared_peek_then_maybe_reap_
                // helper` proves this real call site reaches it; and this call site now
                // provably CALLS that exact, already-proven function rather than a parallel
                // check that could silently drift from it.
                if let Some(result) = &outcome
                    && matches!(classify_reap_attempt(result), ReapAttempt::Confirmed)
                {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!(
                        "ress-perf: warning: subject pid {} did not reap within {:?} of SIGKILL \
                         (likely blocked in an uninterruptible filesystem read) -- registered for \
                         a retry at the next sample's own teardown; falls back to init reaping it \
                         only if this run exits before that retry ever succeeds",
                        pid, REAP_TIMEOUT
                    );
                    ABANDONED_PIDS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(pid);
                    return;
                }
                std::thread::sleep(REAP_POLL_INTERVAL);
            }
        });
    }
}

// wraps the current errno as a chained `anyhow::Error` -- never a `bail!("...: {}", err)`
// formatted-away string. AGENTS.md's own rule ("Error chains: never use `anyhow::Error::msg()`
// -- it destroys the chain") names `Error::msg()` directly, but a `bail!` built from a formatted
// `std::io::Error::last_os_error()` is the identical anti-pattern by construction: it stringifies
// the OS error into the message text and discards the error VALUE itself, so `.source()`/`{:?}`'s
// full chain and downcasting to the concrete `std::io::Error` (e.g. a caller inspecting
// `.raw_os_error()` programmatically) are all gone past that point -- the same PR #28 class this
// crate's every OTHER libc-call error path (`spawn_impl`'s own `command.spawn()` failure,
// `set_nonblocking`'s own caller in `spawn_impl`) already avoided by chaining through
// `anyhow::Error::new(err).context(...)`. Found in PR #44's round-5 review: this file's own new
// preflight sites (`pipe2`, `openpty`, both `fcntl` calls) had drifted from that existing
// convention. One helper, used at every one of those sites, so the convention cannot drift
// site-by-site again.
fn last_os_error_context(context: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::last_os_error()).context(context.to_string())
}

fn set_nonblocking(fd: libc::c_int) -> anyhow::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(last_os_error_context("fcntl(F_GETFL) failed"));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(last_os_error_context("fcntl(F_SETFL, O_NONBLOCK) failed"));
    }
    Ok(())
}

// see spawn_capturing_exec_stamp's own doc comment at its call site for the full danger this
// guards against (a stamp-pipe end landing on fd 0/1/2 under a closed-stdio launch context, then
// getting silently overwritten by pre_exec's own dup2 calls). `F_DUPFD_CLOEXEC`, not a bare
// `F_DUPFD`: the latter would NOT set close-on-exec on the new descriptor, quietly losing the
// exact property `pipe2(..., O_CLOEXEC)` established for this fd in the first place. `3` as the
// minimum target: the lowest fd guaranteed to sit above the three stdio slots this whole guard
// exists to avoid.
fn ensure_fd_above_stdio(fd: libc::c_int) -> anyhow::Result<libc::c_int> {
    if fd > 2 {
        return Ok(fd);
    }
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if moved == -1 {
        let err = last_os_error_context("fcntl(F_DUPFD_CLOEXEC) failed");
        unsafe { libc::close(fd) };
        return Err(err);
    }
    unsafe { libc::close(fd) };
    Ok(moved)
}

// runs inside pre_exec -- post-fork, pre-exec, in a child that is a fork of a possibly-
// multithreaded parent, so this function (and everything it calls) must stay strictly
// async-signal-safe (POSIX's signal-safety(7) list): no allocation (a `Vec`/`String` could
// deadlock on a lock some other, now-frozen "ghost" thread held at the moment of fork), no
// locking, no panicking. `clock_gettime` and `write` are both explicitly on that list; the
// `to_ne_bytes`/array-copy work is pure register/stack manipulation, no allocation involved.
// best-effort by design: any failure here (clock_gettime erroring, a short/failed write) is
// silently swallowed -- the parent's own read will see fewer than 16 bytes or EOF and treat
// the stamp as unavailable, which must never fail the sample this pre_exec belongs to.
fn write_exec_stamp(write_fd: libc::c_int) {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } != 0 {
        return;
    }
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&ts.tv_sec.to_ne_bytes());
    bytes[8..16].copy_from_slice(&ts.tv_nsec.to_ne_bytes());
    unsafe {
        libc::write(write_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

// bounds the parent-side read of the 16-byte stamp write_exec_stamp writes, so a child that
// somehow never reaches its own write (unprecedented -- everything between fork and the write
// is a handful of fast, ordinary syscalls) degrades this ONE sample's precise reading to
// unavailable rather than blocking the run before its own ceiling-based timeouts even have a
// chance to apply (the exact same reasoning PtyChild::Drop's own bounded reap already
// documents at length for a structurally similar risk).
const EXEC_STAMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn read_exec_stamp(read_fd: libc::c_int) -> Option<std::time::SystemTime> {
    if set_nonblocking(read_fd).is_err() {
        return None;
    }
    let mut buf = [0u8; 16];
    let mut total_read = 0usize;
    let deadline = std::time::Instant::now() + EXEC_STAMP_TIMEOUT;
    while total_read < buf.len() {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        let n = unsafe {
            libc::read(
                read_fd,
                buf[total_read..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - total_read,
            )
        };
        if n > 0 {
            total_read += n as usize;
            continue;
        }
        if n == 0 {
            return None; // eof before a full stamp arrived
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }
        return None;
    }
    let tv_sec = i64::from_ne_bytes(buf[0..8].try_into().unwrap());
    let tv_nsec = i64::from_ne_bytes(buf[8..16].try_into().unwrap());
    if tv_sec < 0 || tv_nsec < 0 {
        return None;
    }
    Some(
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(tv_sec as u64)
            + std::time::Duration::from_nanos(tv_nsec as u64),
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::{on_path, sibling_bin_path};

    fn osstrs(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    fn fake_pager_argv(mode: &str) -> Vec<std::ffi::OsString> {
        vec![
            sibling_bin_path("fake-pager").into(),
            std::ffi::OsString::from(mode),
        ]
    }

    fn drain_to_eof(child: &mut super::PtyChild, deadline: std::time::Instant) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match child.read_available(&mut out).unwrap() {
                super::ReadStatus::Data(_) => {}
                super::ReadStatus::Eof => return out,
                super::ReadStatus::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for eof"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    // found in PR #44 round 16 (a codex P2): `ACTIVE_CHILD_PGID` is genuine process-wide shared
    // state (see its own doc comment, above this module). A `Mutex<()>` used to live here,
    // serializing every test that spawns-and-inspects it against every other one.
    //
    // found in PR #44 pass 6 S2 (#6, a user-review finding): "every test that spawns-and-inspects
    // the real global" turned out to describe a CONVENTION, not a GUARANTEE -- a direct sweep
    // found most of this module's own spawning tests never took that lock at all (they spawn a
    // real `PtyChild` for some OTHER reason -- an env var, a signal mask, a read primitive -- and
    // that spawn still writes the real global as an unavoidable side effect, whether or not the
    // test's own author remembered, or even had reason to think, the lock existed). A lock only
    // SOME writers take cannot isolate state ANY writer can touch. Structural fix, not a wider
    // convention: `PtyChild::spawn_with_registry` (see its own doc comment) lets a test spawn a
    // fully real child whose registration/clear/peek traffic goes through an INJECTED, per-test
    // `'static` local instead -- every test in this module that does not specifically need to
    // pin the real global's own identity now uses that.
    //
    // found in PR #44 pass 6 S2 (#6, closing the residual): the ONE test that used to remain on
    // the real global (`try_wait_negative_peek_never_consumes_positive_peek_clears_before_
    // reaping`) is converted too -- see its own doc comment. With NOTHING left anywhere that
    // asserts `ACTIVE_CHILD_PGID`'s own value, `runner.rs`'s own ~19 real-`PtyChild`-spawning
    // tests (a direct sweep of that file confirms: every one of them only ever WRITES the real
    // global, as an unavoidable side effect of calling the real `spawn` for some unrelated
    // reason -- screen/predicate/poll-loop behavior -- never reads or asserts on it) are harmless
    // by construction regardless of scheduling: a write nobody ever reads-and-checks cannot make
    // any assertion, anywhere, come out wrong. The lock itself is removed along with the last
    // test that took it -- kept unused "as insurance" is exactly the dead-defensive-code shape
    // this crate's own `AGENTS.md` rule (and this exact PR's own S1 debug_assert precedent)
    // argues against: a lock nothing ever locks provides zero protection, unlike an active check
    // that still fires on every call. The one property that genuinely still needs the REAL
    // static specifically -- that production `spawn` defaults to it, not some other atomic --
    // is proven by its own tiny, race-free CONSTRUCTION check instead:
    // `spawn_registers_into_the_real_active_child_pgid_by_default`, this module's own tests.
    //
    // Confirmed empirically, not assumed, that the clobbering this fix eliminates is a real
    // mechanism, not merely a theoretical concern: a throwaway RED-check (two real `PtyChild::
    // spawn` calls back to back against the real global, standing in for two independently-
    // scheduled tests interleaving) failed exactly as predicted -- the second spawn's own pid
    // silently overwrote the first's registration, breaking an exact-equality assertion of the
    // FIRST spawn's own pid. Not kept as a permanent test (it deliberately reintroduces the
    // unlocked-real-global-spawn shape this whole fix removes), reverted immediately after
    // confirming.

    #[test]
    fn spawned_child_sees_only_the_given_environment() {
        // found in PR #44 pass 6 S2 (#6): this test does not care about `ACTIVE_CHILD_PGID` at
        // all -- spawns against its own per-test local registry instead of the real global, so
        // it can never write to (or be corrupted by) it. See `PtyChild::spawn_with_registry`'s
        // own doc comment for why a `static` declared inside a test function body is itself
        // enough isolation, with no lock needed.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = osstrs(&[on_path("env").to_str().unwrap()]);
        let env = [
            ("MARKER".to_string(), "42".to_string()),
            ("TERM".to_string(), "tmux-256color".to_string()),
        ];
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &env, (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let out = drain_to_eof(&mut child, deadline);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("MARKER=42"), "missing MARKER: {text}");
        assert!(text.contains("TERM=tmux-256color"), "missing TERM: {text}");
        assert!(
            !text.contains("PATH="),
            "PATH leaked into child env: {text}"
        );
        assert!(
            !text.contains("HOME="),
            "HOME leaked into child env: {text}"
        );
    }

    // isolates the registration/clear LOGIC from the real global entirely, against a throwaway
    // local atomic instead -- exercises the EXACT functions PtyChild::spawn_impl/Drop call in
    // production (`register_active_child`/`clear_active_child_if_mine`), so this still tests
    // real production code, just not through the shared static. Found necessary the hard way:
    // a first version of this test asserted directly against the real `ACTIVE_CHILD_PGID` and
    // failed 5/5 full-suite runs under plain `cargo test`'s thread-parallel model (0/5 filtered
    // in isolation) -- another concurrently-running test's own spawn/drop legitimately
    // interleaved with this one's reads, since that atomic is genuine process-wide shared
    // state. This version cannot race anything, by construction, regardless of test mode.
    #[test]
    fn active_child_registration_sets_and_clears_by_compare_exchange() {
        let atomic = std::sync::atomic::AtomicI32::new(0);
        super::register_active_child(&atomic, 12345);
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 12345);
        // clearing with the WRONG pid must not touch it -- proves the compare-exchange guard
        // (not a bare store) is what is actually wired in, the exact property that keeps one
        // child's teardown from ever erasing a different, newer child's registration.
        super::clear_active_child_if_mine(&atomic, 99999);
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 12345);
        super::clear_active_child_if_mine(&atomic, 12345);
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // found in PR #44 pass-2 review: an obviously-invalid pid (far past any realistic
    // /proc/sys/kernel/pid_max) is neither a real process nor this test process's own child --
    // `waitpid` must fail (ESRCH or ECHILD, either way `rc != 0`), and the retry logic must
    // treat that exactly like a successful reap for ITS OWN purposes: nothing more to wait for,
    // so the pid is dropped from the registry rather than retried forever.
    #[test]
    fn retry_abandoned_reaps_drops_a_pid_that_is_not_this_processes_child() {
        let abandoned = std::sync::Mutex::new(vec![999_999_999]);
        super::retry_abandoned_reaps(&abandoned);
        assert!(
            abandoned.lock().unwrap().is_empty(),
            "a pid that can never be reaped by this process must not be retried forever"
        );
    }

    // the positive case: a pid that genuinely IS this process's own child, has genuinely
    // exited, and is sitting as a real zombie (deliberately never reaped through
    // std::process::Child's own API, via `mem::forget` below, so this test constructs the exact
    // "abandoned, not yet reaped" state the whole registry exists to recover from) must actually
    // be reaped and removed, not just discarded on a technicality.
    #[test]
    fn retry_abandoned_reaps_reaps_a_genuinely_exited_child() {
        let argv = osstrs(&[on_path("true").to_str().unwrap()]);
        let child = std::process::Command::new(&argv[0]).spawn().unwrap();
        let pid = child.id() as i32;
        // poll for the real zombie state (/proc's own "Z" comm-state field) rather than a fixed
        // sleep -- `true` exits near-instantly, but "near" is a guess; the real condition is not.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // format: "<pid> (<comm>) <state> ..." -- state is the first field after the
            // closing paren, so this substring match is safe even for a comm containing spaces.
            if stat.contains(") Z ") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child pid {pid} never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let abandoned = std::sync::Mutex::new(vec![pid]);
        super::retry_abandoned_reaps(&abandoned);
        assert!(
            abandoned.lock().unwrap().is_empty(),
            "a genuinely-exited, genuinely-this-process's-own child must be reaped and removed"
        );
        // std::process::Child does not itself reap on Drop (documented std behavior) and has no
        // idea this test just reaped its pid out from under it via a raw waitpid -- forgetting
        // it avoids any later confusion from its own now-stale internal state, though it is not
        // load-bearing for correctness here (Child's own Drop is a no-op regarding the OS
        // process either way).
        std::mem::forget(child);
    }

    // returns whether `signal` is currently blocked in THIS thread -- a plain query, `set` NULL
    // (the standard idiom: leaves the mask unchanged, fills `oldset` with the current one) so it
    // never itself perturbs what it is inspecting.
    fn is_signal_blocked(signal: libc::c_int) -> bool {
        unsafe {
            let mut current: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut current);
            libc::sigismember(&current, signal) == 1
        }
    }

    // found in PR #44 pass-2 review: `spawn_impl` used to register the newly-spawned pid only
    // AFTER `command.spawn()` had already returned a real, live, already-setsid'd child --
    // `spawn_window_blocked` closes that window by blocking SIGINT/SIGTERM/SIGHUP around the
    // WHOLE spawn-then-register sequence, not just the spawn call. A black-box test asserting
    // only the two end states (blocked beforehand, unblocked afterward) cannot tell that correct
    // ordering apart from a buggy one that unblocks too early -- e.g. right after `spawn()`
    // returns but before registration runs -- since both produce the identical final mask state
    // when nothing actually interrupts the closure. This test observes the mask FROM INSIDE the
    // injected closure itself, at the exact moment a real spawn-then-register would be running,
    // the same seam round 6's own `teardown_keeps_the_registration_active_through_the_kill_step`
    // established for the mirror-image (clear-side, not set-side) ordering. SIGUSR2, not
    // SIGINT/SIGTERM/SIGHUP: this test exercises the GENERIC blocking mechanism, not anything
    // specific to those three signals' own semantics, and using a signal nothing else in this
    // suite (or `cargo test`'s own harness) ever touches rules out any risk of interfering with,
    // or being confused for, unrelated signal handling.
    #[test]
    fn spawn_window_keeps_the_signal_blocked_until_the_closure_returns() {
        let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGUSR2);
        }
        assert!(
            !is_signal_blocked(libc::SIGUSR2),
            "test precondition: SIGUSR2 must start unblocked on this thread"
        );
        let mut blocked_during: Option<bool> = None;
        let result: anyhow::Result<()> = super::spawn_window_blocked(&signals, || {
            blocked_during = Some(is_signal_blocked(libc::SIGUSR2));
            Ok(())
        });
        result.unwrap();
        assert_eq!(
            blocked_during,
            Some(true),
            "SIGUSR2 must still be blocked while the closure -- standing in for spawn-then- \
             register -- runs"
        );
        assert!(
            !is_signal_blocked(libc::SIGUSR2),
            "SIGUSR2 must be unblocked once the window closes"
        );
    }

    // the error path must restore the mask too -- a failed spawn (standing in via the closure
    // itself returning Err) must not leave this thread's own signal mask permanently altered.
    #[test]
    fn spawn_window_restores_the_mask_even_when_the_closure_errs() {
        let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGUSR2);
        }
        let result: anyhow::Result<()> =
            super::spawn_window_blocked(&signals, || anyhow::bail!("simulated spawn failure"));
        assert!(result.is_err());
        assert!(
            !is_signal_blocked(libc::SIGUSR2),
            "SIGUSR2 must still be unblocked after an error path"
        );
    }

    // restores this test THREAD's own signal mask on every exit path, panic included -- a plain
    // `unsafe { sigprocmask(...) }` call placed after the test's own body would never run if an
    // assertion in between panicked, leaking a permanently blocked SIGHUP into whatever runs on
    // this same OS thread next (only a real risk under plain `cargo test`'s thread-parallel
    // model, where threads are not necessarily one-per-test; harmless either way under
    // `cargo nextest run`'s own per-process isolation, but not worth relying on that).
    struct SigmaskGuard(libc::sigset_t);
    impl Drop for SigmaskGuard {
        fn drop(&mut self) {
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
        }
    }

    // found in PR #44 pass 5 (A2, a codex P2): pins the fix against a real spawned process.
    // Blocks SIGHUP in THIS test thread BEFORE spawning, for a reason entirely unrelated to
    // `spawn_window_blocked`'s own window -- simulating exactly the "caller had independently
    // already blocked one of these three signals" scenario the fix exists for -- then asserts
    // the subject sees SIGHUP still blocked (the caller's own pre-existing intent preserved,
    // not clobbered by an unconditional unblock) and SIGINT/SIGTERM unblocked (nothing else
    // ever blocked those, so restoring `pre_window_mask` correctly leaves them alone too).
    #[test]
    fn pre_exec_restores_the_callers_own_pre_window_mask_not_a_blanket_unblock() {
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut to_block: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut to_block);
            libc::sigaddset(&mut to_block, libc::SIGHUP);
            assert_eq!(
                libc::sigprocmask(libc::SIG_BLOCK, &to_block, &mut previous),
                0
            );
        }
        let _restore = SigmaskGuard(previous);

        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("report-sigmask");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "report-sigmask never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let code = status
            .code()
            .expect("report-sigmask must exit normally, not by signal");
        assert_eq!(
            code & 1,
            1,
            "SIGHUP: the caller (this test) had it blocked before spawning -- must stay \
             blocked in the child, not clobbered by an unconditional unblock"
        );
        assert_eq!(
            code & 2,
            0,
            "SIGINT: nothing blocked it before spawning -- must be unblocked in the child"
        );
        assert_eq!(
            code & 4,
            0,
            "SIGTERM: nothing blocked it before spawning -- must be unblocked in the child"
        );
    }

    // found in PR #44's round-6 review: an earlier version of `teardown_registered_child`
    // (inlined directly in `Drop::drop` at the time) cleared the registration BEFORE calling
    // `kill`, not after -- a black-box test asserting only the two end states (registered
    // beforehand, cleared afterward) could never have caught that, since both orderings produce
    // the identical final state when nothing interrupts Drop. This test observes the atomic FROM
    // INSIDE the `kill` closure itself, at the exact moment `Drop`'s own real closure would be
    // issuing `killpg` -- proving the registration is still in place then, which is the actual
    // property a SIGINT/SIGTERM handler racing this Drop depends on (see `ACTIVE_CHILD_PGID`'s
    // own doc comment for the full reasoning).
    #[test]
    fn teardown_keeps_the_registration_active_through_the_kill_step() {
        let atomic = std::sync::atomic::AtomicI32::new(777);
        let mut registration_seen_during_kill = None;
        super::teardown_registered_child(&atomic, 777, false, || {
            registration_seen_during_kill = Some(atomic.load(std::sync::atomic::Ordering::SeqCst));
        });
        assert_eq!(
            registration_seen_during_kill,
            Some(777),
            "a signal arriving during the kill step must still see the subject registered"
        );
        // and still cleared by the time teardown returns -- the fix moves WHEN the clear
        // happens, not WHETHER it happens.
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn teardown_skips_the_kill_step_and_still_clears_when_already_reaped() {
        let atomic = std::sync::atomic::AtomicI32::new(777);
        let mut kill_called = false;
        super::teardown_registered_child(&atomic, 777, true, || {
            kill_called = true;
        });
        assert!(
            !kill_called,
            "an already-reaped child must never be signaled again -- the pid could have been \
             recycled by the kernel"
        );
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn child_exit_is_observable_and_drop_reaps() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        // Both spawns below share this ONE local registry: safe (not a re-introduction of the
        // exact aliasing this fix exists to eliminate) because it mirrors the SAME "only one
        // child registered at a time" invariant `ACTIVE_CHILD_PGID`'s own doc comment already
        // establishes for the real global in production -- `child` is fully observed-and-cleared
        // by its own `try_wait` loop below before `sleeper` ever spawns, and `sleeper` is fully
        // dropped (killed, reaped, cleared) before `child`'s own later, implicit drop at the end
        // of this function.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = osstrs(&[on_path("true").to_str().unwrap()]);
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "expected `true` to exit successfully, got {status:?}"
        );

        let argv = fake_pager_argv("never-paint");
        let sleeper =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let pid = sleeper.pid();
        drop(sleeper);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let rc = unsafe { libc::kill(pid, 0) };
            if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} still alive after drop"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // found in PR #44 round 9: pins that `try_wait` clears its own registration, at the moment it
    // observes the exit -- not only later, when `Drop` eventually runs.
    //
    // found in PR #44 pass 6 S2 (#6): used to deliberately check the REAL `ACTIVE_CHILD_PGID`
    // global (reasoned, at the time, to be race-safe even without a lock -- pids are unique among
    // simultaneously-alive processes, so a concurrently-running test's own child registering
    // itself could not make a NOT-EQUAL-to-this-pid comparison come out wrong in either
    // direction) and took `ACTIVE_CHILD_PGID_TEST_LOCK` anyway, "uniformly." Codex's own sweep
    // found that reasoning did not generalize: the LOCK was voluntary, and most of this module's
    // own spawning tests never took it at all (a genuinely unlocked spawn, running concurrently,
    // could still corrupt THIS test's assumption that no other write touches the real global
    // between its own read and assert, a narrower but real version of the same gap). Converted
    // to a per-test local registry instead -- this property (clear-at-observation, not only at
    // drop) does not need the REAL specific static to hold; `try_wait_negative_peek_never_
    // consumes_positive_peek_clears_before_reaping`, below, is now the one test left deliberately
    // exercising the real global end-to-end (see its own doc comment for why keeping exactly one
    // such test, rather than converting every last one, still matters).
    #[test]
    fn try_wait_clears_the_registration_the_moment_it_observes_the_exit_not_only_at_drop() {
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = osstrs(&[on_path("true").to_str().unwrap()]);
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let pid = child.pid();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // checked BEFORE any drop -- `child` is still a live binding here, Drop has not run.
        assert_ne!(
            LOCAL_REGISTRY.load(std::sync::atomic::Ordering::SeqCst),
            pid,
            "the registration must already be cleared the moment try_wait observes the exit, \
             not only later when Drop runs -- a signal in that gap could kill(-pgid) at a \
             possibly-recycled target"
        );
    }

    // found in PR #44 pass 5 (A1): pins `peek_then_maybe_reap`'s own CLASS property directly and
    // unconditionally, via an injected probe -- see its own doc comment for why a black-box
    // before/after check cannot tell this apart from the pre-fix, unconditionally-falls-through
    // shape. A panicking `reap` closure proves it is never invoked at all on a negative peek,
    // not merely that its answer happened not to matter.
    #[test]
    fn peek_then_maybe_reap_never_invokes_reap_on_a_negative_peek() {
        // neither test in this pair makes any claim about the call counter's value (that's
        // `drop_reap_loop_calls_the_shared_peek_then_maybe_reap_helper`'s own job, with its own
        // injected counter) -- the real, process-wide default is fine to pass here since nothing
        // ever reads it back.
        let result: Option<()> = super::peek_then_maybe_reap(
            &super::PEEK_THEN_MAYBE_REAP_CALLS,
            || false,
            || panic!("a negative peek must never invoke the consuming reap"),
        );
        assert!(result.is_none());
    }

    #[test]
    fn peek_then_maybe_reap_invokes_reap_exactly_once_on_a_positive_peek() {
        let mut calls = 0;
        let result = super::peek_then_maybe_reap(
            &super::PEEK_THEN_MAYBE_REAP_CALLS,
            || true,
            || {
                calls += 1;
                "reaped"
            },
        );
        assert_eq!(result, Some("reaped"));
        assert_eq!(
            calls, 1,
            "the reap closure must run exactly once on a positive peek"
        );
    }

    // found in PR #44 pass 6 S2 (#5): pins `classify_reap_attempt`'s own contract directly,
    // deterministically -- no real child, no real syscall, no real timing. See its own doc
    // comment (above this module) for the full derivation of why `Ok(Some(_))`/a confirmed
    // `ECHILD` are the only two outcomes allowed to mark a child reaped.
    #[test]
    fn classify_reap_attempt_confirms_a_genuine_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        let result: std::io::Result<Option<std::process::ExitStatus>> =
            Ok(Some(std::process::ExitStatus::from_raw(0)));
        assert!(matches!(
            super::classify_reap_attempt(&result),
            super::ReapAttempt::Confirmed
        ));
    }

    #[test]
    fn classify_reap_attempt_retries_when_a_positive_peek_yields_no_status() {
        // "should not happen" per the loop's own long-standing comment (a positive WNOWAIT peek
        // means a genuine, still-unrecycled zombie exists for this consuming call to find) --
        // but if it ever did, there is no positive evidence of a confirmed exit to act on, so
        // this must retry rather than claim one.
        let result: std::io::Result<Option<std::process::ExitStatus>> = Ok(None);
        assert!(matches!(
            super::classify_reap_attempt(&result),
            super::ReapAttempt::Retry
        ));
    }

    #[test]
    fn classify_reap_attempt_retries_on_a_transient_error_like_eintr() {
        let result: std::io::Result<Option<std::process::ExitStatus>> =
            Err(std::io::Error::from_raw_os_error(libc::EINTR));
        assert!(matches!(
            super::classify_reap_attempt(&result),
            super::ReapAttempt::Retry
        ));
    }

    #[test]
    fn classify_reap_attempt_confirms_on_echild() {
        // ECHILD means the kernel has no such child to wait for AT ALL -- the same "nothing
        // more to do here" signal `retry_abandoned_reaps` already relies on for this errno.
        let result: std::io::Result<Option<std::process::ExitStatus>> =
            Err(std::io::Error::from_raw_os_error(libc::ECHILD));
        assert!(matches!(
            super::classify_reap_attempt(&result),
            super::ReapAttempt::Confirmed
        ));
    }

    // found in PR #44 pass 5 (A1's own three-state protocol): the real-process, integration-
    // level half of A1's coverage -- confirms `try_wait` actually WIRES `peek_then_maybe_reap`
    // in correctly (the pure tests above pin the gating function's own contract in isolation;
    // this one proves the real method observably behaves per that contract against a genuine
    // spawned-and-killed process). Cannot, on its own, distinguish the fix from the pre-fix
    // unconditional-fallthrough shape -- both produce the identical steady-state answers here,
    // since the bug they differ on is entirely about the race window between "genuinely alive"
    // and "genuinely dead," which neither steady state below ever visits (see the pure tests
    // above for the property that DOES tell them apart, unconditionally, regardless of timing).
    // A negative peek must never fall through to a consuming wait -- checked from both of that
    // claim's own directions: it must not consume the CHILD (nothing to consume -- the process
    // really is still alive when peeked), and it must not clear the REGISTRATION (nothing was
    // observed to clear it for). The positive peek that eventually does observe the exit is
    // then confirmed to be the SAME call that reaps it, clearing the registration as part of
    // that observation rather than after a separate, later reap.
    //
    // found in PR #44 round 16 (a codex P2): phase 1's own assertion below (checked EQUAL to
    // this test's own pid) used to not be race-tolerant the way the sibling test above already
    // was -- a concurrently-running test's own spawn could overwrite `ACTIVE_CHILD_PGID` with a
    // DIFFERENT pid in the gap between this test's own spawn and this assert, under plain
    // `cargo test`'s thread-parallel model.
    //
    // found in PR #44 pass 6 S2 (#6, closing the residual the team lead's own review flagged):
    // converted to a per-test local registry too -- the LAST test anywhere in the workspace that
    // still asserted on the real `ACTIVE_CHILD_PGID`'s own value. With nothing left asserting
    // it, `runner.rs`'s own ~19 real-`PtyChild`-spawning tests (confirmed by a direct sweep: none
    // of them ever assert on `ACTIVE_CHILD_PGID` either -- see this module's own sweep note,
    // `ACTIVE_CHILD_PGID_TEST_LOCK`'s doc comment, for the full result) are harmless BY
    // CONSTRUCTION regardless of what they write or when: a write nobody ever reads-and-checks
    // cannot make any assertion, anywhere, come out wrong. This is what makes the "uncontended"
    // property workspace-wide and TRUE now, not merely a `pty.rs`-scoped, disclosed residual --
    // it rests on "nothing asserts the real global" (a property just proven, not merely
    // convenient), not on "only this module spawns." The one property that genuinely still needs
    // the REAL static specifically -- that production `spawn` defaults to it, not some other
    // atom -- moved to its own tiny, race-free CONSTRUCTION check instead:
    // `spawn_registers_into_the_real_active_child_pgid_by_default`, this module's own tests,
    // below.
    #[test]
    fn try_wait_negative_peek_never_consumes_positive_peek_clears_before_reaping() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment for the general
        // reasoning. `ACTIVE_CHILD_PGID_TEST_LOCK` is no longer taken here either: nothing in
        // this test touches the real global anymore, so there is nothing left for the lock to
        // serialize against.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-hang");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let pid = child.pid();

        // phase 1: the child is genuinely, unambiguously alive (paint-hang never exits on its
        // own) -- try_wait must be a negative peek: `None`, registration left armed since
        // nothing was observed to clear it for.
        assert!(
            child.try_wait().unwrap().is_none(),
            "a genuinely running child must report still-running"
        );
        assert_eq!(
            LOCAL_REGISTRY.load(std::sync::atomic::Ordering::SeqCst),
            pid,
            "a negative peek must leave the registration armed -- nothing was observed to \
             clear it for"
        );

        // phase 2: kill it directly (bypassing PtyChild's own Drop/killpg machinery entirely --
        // this test is about try_wait's own peek-then-reap ordering, not about signal
        // delivery), then confirm it becomes a completely ordinary, still-unconsumed zombie --
        // proving phase 1's negative-peek call consumed nothing at all, via an INDEPENDENT
        // WNOWAIT peek of this test's own, not through the function under test.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 && unsafe { info.si_pid() } == pid {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "child never became observably dead after SIGKILL"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // phase 3: NOW try_wait must take the positive-peek path -- clearing the registration
        // AND reaping in this one call, not clearing now and reaping (or having reaped) some
        // other time.
        let status = child.try_wait().unwrap().expect(
            "a genuinely exited, still-unreaped child must be observed and reaped in this call",
        );
        assert!(
            !status.success(),
            "expected a SIGKILL exit, not a clean one"
        );
        assert_ne!(
            LOCAL_REGISTRY.load(std::sync::atomic::Ordering::SeqCst),
            pid,
            "the positive peek that found this exit must have cleared the registration in the \
             same call"
        );
    }

    // found in PR #44 pass 6 S2 (#6, closing the residual the team lead's own review flagged):
    // the one property that genuinely still needs the REAL `ACTIVE_CHILD_PGID` specifically --
    // that production `spawn` defaults to it, not some other atomic -- proven directly, and
    // race-free by a different argument than every other test in this module: no concurrently-
    // running test can make a POINTER comparison come out wrong the way it could a VALUE
    // comparison. WHICH atomic a `PtyChild`'s own `registry` field points to is decided once, at
    // construction, by this call alone -- nothing else can write THAT (a `&'static` reference,
    // not the atomic's own contents) after the fact, unlike the real global's runtime VALUE,
    // which any concurrently-spawning test legitimately can and does change. This is the wiring
    // guarantee `spawn_with_registry`'s own existence would otherwise leave unchecked: that the
    // production, no-`_with_registry` entry point still reaches for the real global, not
    // silently regressed to default to something else.
    //
    // RED-verified directly: temporarily pointed `spawn`'s own call to `spawn_impl` at a
    // throwaway `WRONG_ATOMIC` instead of `&ACTIVE_CHILD_PGID` -- this test failed immediately
    // and specifically ("must default to the real ACTIVE_CHILD_PGID"), confirming it genuinely
    // catches a mis-wiring rather than passing regardless. Reverted immediately after
    // confirming.
    #[test]
    fn spawn_registers_into_the_real_active_child_pgid_by_default() {
        let argv = osstrs(&[on_path("true").to_str().unwrap()]);
        let child = super::PtyChild::spawn(&argv, &[], (24, 80)).unwrap();
        assert!(
            std::ptr::eq(child.registry, &super::ACTIVE_CHILD_PGID),
            "production PtyChild::spawn must default to the real ACTIVE_CHILD_PGID, not some \
             other atomic"
        );
        // found in PR #44 pass 6 (codex pty.rs:1748): the same guarantee, for the same reason,
        // now that `reap_call_counter` is a sibling injectable field -- production `spawn` must
        // default it to the real `PEEK_THEN_MAYBE_REAP_CALLS`, not silently drift onto some other
        // atomic a future refactor might introduce.
        assert!(
            std::ptr::eq(child.reap_call_counter, &super::PEEK_THEN_MAYBE_REAP_CALLS),
            "production PtyChild::spawn must default to the real PEEK_THEN_MAYBE_REAP_CALLS, not \
             some other atomic"
        );
    }

    // found in PR #44 pass 6 S2 (#5): the real-process, fault-injected half of this fix's own
    // coverage -- `classify_reap_attempt`'s own pure tests, above, pin the classification in
    // isolation; this one proves `try_wait_with_reap` actually WIRES that classification into
    // `self.reaped` against a genuine spawned-and-killed child, across a scripted
    // Err-then-real-Ok sequence no actual syscall can be made to produce deterministically (a
    // real `EINTR` depends on a real signal landing inside a real blocking syscall -- exactly
    // the scheduler-timing race this crate's own rule forbids leaning on for a test; "No real
    // signal races," per the team lead's own instruction for this fix). Only the FIRST reap
    // attempt is faked (an injected `Err(EINTR)`) -- the SECOND is the real
    // `std::process::Child::try_wait`, so the eventual reap is genuine, not simulated, and a
    // final independent, raw `waitpid` proves no zombie survives it.
    //
    // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- this
    // test's own claim is about `self.reaped` (an internal flag `PtyChild` owns regardless of
    // which atomic it happens to be registered against), so it never needed the specific real
    // static to make its point; see `spawned_child_sees_only_the_given_environment`'s own doc
    // comment for the general reasoning, and `try_wait_negative_peek_never_consumes_positive_
    // peek_clears_before_reaping`'s own doc comment for why exactly one OTHER test keeps the
    // real-global coverage this one no longer needs to duplicate.
    #[test]
    fn try_wait_stays_armed_and_retries_past_an_injected_transient_reap_error() {
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-hang");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let pid = child.pid();

        // same real, independent kill-then-WNOWAIT-peek sequence the sibling test above already
        // establishes -- confirms the child is genuinely, observably dead (a real, still-unreaped
        // zombie) BEFORE this test's own calls, so the peek `try_wait_with_reap` drives below is
        // exercising a REAL positive observation, not racing the kill.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 && unsafe { info.si_pid() } == pid {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "child never became observably dead after SIGKILL"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // call 1: the peek is real and positive (the zombie above), but the CONSUMING reap is
        // faked to fail transiently -- the real kernel-level zombie is untouched by this call,
        // still there, still unreaped, for call 2 to find for real.
        let mut calls = 0u32;
        let result = child.try_wait_with_reap(&mut |_real_child| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::EINTR))
        });
        assert!(
            result.is_err(),
            "the injected-EINTR call must still propagate that error to its own caller"
        );
        assert_eq!(calls, 1, "expected the injected reap to run exactly once");
        assert!(
            !child.reaped,
            "an injected transient error must NOT mark this child reaped -- a transient \
             failure proves nothing about whether the child is actually still alive"
        );

        // call 2: the real reap, this time -- the zombie left untouched by call 1 is still
        // there for it to find and genuinely consume.
        let result =
            child.try_wait_with_reap(&mut |real_child| std::process::Child::try_wait(real_child));
        let status = result
            .unwrap()
            .expect("the second, real call must find and reap the genuine zombie left behind");
        assert!(
            !status.success(),
            "expected a SIGKILL exit, not a clean one"
        );
        assert!(
            child.reaped,
            "a confirmed real reap must mark this child reaped"
        );

        // no zombie survives: an independent, raw WNOHANG waitpid must now find nothing left to
        // reap for this pid (ECHILD/-1), not a repeat report of the same exit.
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(
            rc, -1,
            "expected no remaining zombie for this pid -- the real reap above must have fully \
             consumed it"
        );
    }

    // found in PR #44 pass-5-review-followup (finding 1): the Drop-path half of A1's own
    // integration coverage -- pins that Drop's real bounded-reap loop genuinely calls the SAME
    // `peek_then_maybe_reap` this module's pure tests already prove correct in isolation, rather
    // than a hand-inlined copy that could silently drift from it (which is exactly what the
    // reviewer found: reverting just Drop's own call site back to the pre-fix,
    // unconditionally-falls-through shape left every one of this module's other 23 tests green,
    // including `child_exit_is_observable_and_drop_reaps` -- because, like
    // `try_wait_negative_peek_never_consumes_positive_peek_clears_before_reaping` above already
    // discloses for the identical reason, a steady-state, black-box observation of either call
    // site cannot distinguish the fix from the bug it replaced; the race the bug is about is
    // invisible to external timing by construction). `PEEK_THEN_MAYBE_REAP_CALLS` is the
    // deterministic substitute this call site needs precisely because it is NOT
    // itself injectable the way the standalone function is: a real `PtyChild`, dropped for real,
    // must be observed to have reached the shared, already-proven function at all -- not proof of
    // the race-safety property itself (the pure tests own that, and now cover this call site too,
    // by construction), but proof this call site has not quietly stopped being the same
    // implementation.
    //
    // found in PR #44 pass 6 S2 (#6, a user-review finding, "the new counter race"): this test's
    // OWN spawn is converted to a per-test local registry below, the same treatment every other
    // unlocked spawner in this module gets. `PEEK_THEN_MAYBE_REAP_CALLS` itself was deliberately
    // left process-wide at the time, on the argument that a WORKING Drop always contributes at
    // least one real increment of its own, so no concurrently-running test's activity could ever
    // flip `calls_after > calls_before` false.
    //
    // found in PR #44 pass 6 (codex pty.rs:1748): that argument was unsound -- it only covers the
    // case where the test passes for the right reason. This test's actual job is to catch a
    // BROKEN Drop, and a broken Drop contributes ZERO increments of its own, so a false PASS
    // needed only a DIFFERENT, concurrently running test's own increment to land in the same
    // window -- realistic under `nix checkPhase`'s plain thread-parallel `cargo test` (unlike the
    // isolated-per-process `nextest`, the primary `just ci` path), where many other pty tests are
    // incrementing the same global around the same millisecond-scale spawn/kill/reap window.
    // Exactly the class the registry injection already existed to close, just not yet applied to
    // this counter. Fixed the same way: `peek_then_maybe_reap` now takes an injected counter (see
    // its own doc comment), and this test supplies a private one via
    // `spawn_with_registry_and_reap_counter` that no concurrently running test can ever touch --
    // race-free by construction, not by argument about guaranteed contributions.
    #[test]
    fn drop_reap_loop_calls_the_shared_peek_then_maybe_reap_helper() {
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        static LOCAL_REAP_CALLS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        {
            let argv = fake_pager_argv("paint-hang");
            let _child = super::PtyChild::spawn_with_registry_and_reap_counter(
                &argv,
                &[],
                (24, 80),
                &LOCAL_REGISTRY,
                &LOCAL_REAP_CALLS,
            )
            .unwrap();
            // dropped here -- paint-hang never exits on its own, so this exercises Drop's own
            // SIGKILL-then-reap path for real, not a child that happened to already be dead.
        }
        let calls = LOCAL_REAP_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            calls > 0,
            "Drop's own bounded-reap loop must call peek_then_maybe_reap at least once (its \
             first iteration always does, regardless of the peek's own outcome) -- {calls} calls \
             on this test's own private counter, which no other test can ever touch"
        );
    }

    // found in PR #44 pass 3: pins the seam round 9's own test above could not reach --
    // `clear_registration_if_the_child_has_exited`'s OWN peek-then-clear ordering, not just
    // that a clear eventually happens somewhere before `try_wait` returns. A synthetic pid (a
    // throwaway local atomic, matching every OTHER pure-function test in this module -- this
    // function takes `pid` as an explicit parameter, so no real spawn is needed to exercise the
    // ATOMIC's own clearing logic in isolation) confirms the clear itself; the real-process
    // half below confirms the PEEK genuinely doesn't consume the zombie, by peeking a SECOND
    // time, directly, after the function under test has already run once -- if the first peek
    // had consumed it, this second one would fail (ECHILD, or a stale/absent report), not
    // succeed identically.
    #[test]
    fn clear_registration_if_the_child_has_exited_clears_only_on_a_genuine_match() {
        let atomic = std::sync::atomic::AtomicI32::new(777);
        // a pid that plainly isn't a genuine, currently-reportable child of this test process
        // (0 is never a valid pid to wait on) -- `waitid` must fail, the registration must stay
        // untouched, and the function must report no exit observed.
        assert!(!super::clear_registration_if_the_child_has_exited(
            &atomic, 0
        ));
        assert_eq!(atomic.load(std::sync::atomic::Ordering::SeqCst), 777);
    }

    #[test]
    fn clear_registration_if_the_child_has_exited_leaves_the_zombie_unrecycled_for_a_later_real_reap()
     {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry for the SPAWN itself -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why. The
        // function-under-test's own direct call, below, already used its own separate throwaway
        // atomic; only the spawn's own implicit registration was still touching the real global.
        static SPAWN_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = osstrs(&[on_path("true").to_str().unwrap()]);
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &SPAWN_REGISTRY).unwrap();
        let pid = child.pid();
        // wait for the child to genuinely exit, via a raw, non-consuming peek of our own --
        // confirms readiness without touching PtyChild's own reap state at all.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 && unsafe { info.si_pid() } == pid {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "child never became observably dead"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let atomic = std::sync::atomic::AtomicI32::new(pid);
        assert!(
            super::clear_registration_if_the_child_has_exited(&atomic, pid),
            "a genuinely exited pid must report an observed exit"
        );
        assert_eq!(
            atomic.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the registration must be cleared once a genuine exit is observed"
        );
        // the pin: peek AGAIN, directly, completely independent of the function under test --
        // if its own internal peek had consumed the zombie, THIS would now fail (ECHILD). It
        // must still succeed, identically, proving the zombie is still there, still
        // unrecyclable, exactly as the function's own doc comment claims.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(
            rc, 0,
            "the zombie must still be there, unconsumed, after the clear"
        );
        assert_eq!(unsafe { info.si_pid() }, pid);
        // finally, the real reap -- via PtyChild's own public try_wait, the normal production
        // path -- must still succeed correctly, proving the peek never disturbed it.
        let status = child.try_wait().unwrap();
        assert!(
            status.is_some(),
            "the real reap must still succeed after the peek"
        );
    }

    #[test]
    fn controlling_terminal_is_established() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("check-tty");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "check-tty probe never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "fake-pager check-tty failed to open /dev/tty: {status:?}"
        );
    }

    #[test]
    fn eof_after_child_exit() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-exit");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let out = drain_to_eof(&mut child, deadline);
        assert!(!out.is_empty(), "expected the paint marker before eof");
    }

    #[test]
    fn queued_read_bytes_reports_the_count_without_consuming_it() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-exit");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let queued = loop {
            let queued = child.queued_read_bytes().unwrap();
            if queued > 0 {
                break queued;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "paint marker never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // a second peek reports the SAME count -- paint-exit writes once and exits, so nothing
        // new is arriving to confound this; the only way this could differ is if the first call
        // had itself consumed something, which FIONREAD (unlike a real `read`) never does.
        assert_eq!(child.queued_read_bytes().unwrap(), queued);
        let mut buf = Vec::new();
        let status = child.read_available(&mut buf).unwrap();
        assert!(matches!(status, super::ReadStatus::Data(_)));
        assert_eq!(
            buf.len(),
            queued,
            "read_available must consume exactly what queued_read_bytes reported queued"
        );
    }

    #[test]
    fn read_available_bounded_with_zero_budget_reads_nothing() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-exit");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let queued = loop {
            let queued = child.queued_read_bytes().unwrap();
            if queued > 0 {
                break queued;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "paint marker never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut buf = Vec::new();
        let status = child.read_available_bounded(&mut buf, 0).unwrap();
        assert!(matches!(status, super::ReadStatus::WouldBlock));
        assert!(buf.is_empty());
        assert_eq!(
            child.queued_read_bytes().unwrap(),
            queued,
            "a zero-budget call must not consume anything"
        );
    }

    #[test]
    fn read_available_bounded_never_reads_past_its_own_budget() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment, above, for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-exit");
        let mut child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() >= 8 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "paint marker never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut buf = Vec::new();
        let status = child.read_available_bounded(&mut buf, 4).unwrap();
        assert!(
            matches!(status, super::ReadStatus::Data(4)),
            "expected exactly the 4-byte budget, got {status:?}"
        );
        assert_eq!(buf.len(), 4);
    }

    // found in PR #44 pass 7 (a p6-review2 finding, delta review): `hung_up` is brand-new this
    // commit and, until now, only ever exercised INDIRECTLY -- real-subject runner scenarios
    // that never isolate it, and `ScriptedTransport` tests (`runner.rs`) that only prove the
    // wiring reads whatever bool was scripted, neither of which proves the real `ppoll`/
    // `POLLHUP` mechanics work at all. This is the dedicated transport-primitive test, the same
    // standard `read_available_bounded` and `queued_read_bytes` already get above: a real
    // child, killed for real, observed through `hung_up` alone -- no read, no `try_wait`.
    //
    // RED-verified directly: temporarily changed `hung_up`'s own `revents & POLLHUP != 0` to
    // `== 0` (inverted) -- phase 1's OWN assertion failed immediately ("must not report hung
    // up"), and separately (reverted, then removed the check entirely -- hardcoded `Ok(false)`)
    // phase 3 failed with "never observed POLLHUP after SIGKILL" at the 5s deadline, confirming
    // this test genuinely exercises the mechanism in both directions, not passing regardless.
    // Reverted immediately after confirming each.
    #[test]
    fn hung_up_becomes_true_once_the_child_exits_and_its_slave_side_closes() {
        // found in PR #44 pass 6 S2 (#6): a per-test local registry, not the real global -- see
        // `spawned_child_sees_only_the_given_environment`'s own doc comment for why.
        static LOCAL_REGISTRY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let argv = fake_pager_argv("paint-hang");
        let child =
            super::PtyChild::spawn_with_registry(&argv, &[], (24, 80), &LOCAL_REGISTRY).unwrap();
        let pid = child.pid();

        // phase 1: the child is genuinely, unambiguously alive (paint-hang never exits on its
        // own) -- hung_up must be a true negative here, not a stale or spuriously-true peek this
        // early.
        assert!(
            !child.hung_up().unwrap(),
            "a genuinely running child's pty slave must not report hung up"
        );

        // phase 2: kill it directly (bypassing PtyChild's own Drop/killpg machinery entirely --
        // this test is about hung_up's own POLLHUP detection, not about signal delivery),
        // mirroring the sibling try_wait primitive tests' own reasoning above. The parent's own
        // copy of the slave fd is already closed by spawn (see spawn_impl's own comment at its
        // slave_fd close, above) specifically so nothing but the child's own reference keeps the
        // slave side open -- SIGKILL tearing down the child's fds is what should let POLLHUP
        // fire, not a reap.
        unsafe { libc::kill(pid, libc::SIGKILL) };

        // phase 3: hung_up must observe POLLHUP once the kernel tears down the killed child's
        // fds -- polled as an event, not a single guessed sleep, the same "tests prove events,
        // not scheduler timing" discipline every other primitive test in this file already
        // holds to. Deliberately never calls try_wait or reads anything: proving hung_up alone
        // is sufficient is the whole point.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.hung_up().unwrap() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hung_up() never observed POLLHUP after SIGKILL"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // real pipe fds in a `cargo test` process are always > 2 (stdio stays open throughout the
    // whole suite), so this pins the common-case no-op path directly against a real fd pair --
    // the fd-lands-on-0/1/2 path itself cannot be safely exercised in-process against this test
    // binary's own real stdio (shared, process-wide state under cargo test's thread-parallel
    // model, the same class of hazard this file's other tests already document at length), and
    // is instead verified by code review plus a standalone, out-of-process repro (see the round-5
    // report) -- the identical disclosure standard already accepted for the master-fd guard.
    #[test]
    fn ensure_fd_above_stdio_leaves_an_already_high_fd_untouched() {
        let mut pipe_fds: [libc::c_int; 2] = [-1, -1];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);
        assert!(read_fd > 2, "test precondition: real stdio must be open");
        assert!(write_fd > 2, "test precondition: real stdio must be open");
        assert_eq!(super::ensure_fd_above_stdio(read_fd).unwrap(), read_fd);
        assert_eq!(super::ensure_fd_above_stdio(write_fd).unwrap(), write_fd);
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }

    // pins the AGENTS.md chain-preserving rule (PR #28's own class) for this file's own preflight
    // error sites: -1 is never a valid fd, so `fcntl(F_GETFL)` fails deterministically and safely
    // (no real fd is touched) with EBADF -- a bare `bail!("...: {}", err)` would still render the
    // identical TEXT here, but would silently drop `err` as a structured source, so downcasting
    // to the concrete `std::io::Error` (or walking `.source()`) would come back empty. This
    // asserts the chain survives, not just the rendered message.
    #[test]
    fn set_nonblocking_on_an_invalid_fd_preserves_the_os_error_in_the_chain() {
        let err = super::set_nonblocking(-1).unwrap_err();
        let io_err = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("expected the underlying std::io::Error to survive in the anyhow chain");
        assert_eq!(io_err.raw_os_error(), Some(libc::EBADF));
    }
}
