// the sample runner: the measurement protocol the retired perf.sh implemented in bash,
// as code — driving one subject (ress or less, or fake-pager in this module's own tests)
// through one scenario against one fixture, over the pty/screen substrate. scenarios.rs's
// sample loop is the live caller, driven by main.rs's cli.

// a representative modern terminal, fixed rather than inherited -- matches perf.sh's
// PANE_ROWS/PANE_COLS exactly (see run_sample's doc comment for why fixed, not ambient).
const SCREEN_ROWS: u16 = 50;
const SCREEN_COLS: u16 = 120;
// poll cadence -- matches perf.sh's POLL_S.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
// bounds the wait for the tracing writer's non-blocking flush once a paint has already
// landed -- matches perf.sh's LOG_TIMEOUT_S. a harness-internal bound, not a measurement
// ceiling, so it lives here as a constant rather than on `Ceilings`.
const LOG_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A condition evaluated against the subject's reconstructed screen during a poll loop.
///
/// Concrete cases rather than the brief's suggested `fn(&Screen) -> bool` alias: a real
/// scenario (jump-end, deep-goto) needs to close over a runtime-computed needle -- a fixture's
/// target line prefix, derived from its own line count -- and a bare function pointer cannot
/// capture that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// The top row (row 0) holds non-blank content -- the generic "something painted" gate
    /// (perf.sh's `wait_first_paint`/`pane_row1`: literally `pane_text | head -n1`, row 1 only,
    /// never any other row -- a pager can paint a status/prompt line elsewhere on screen well
    /// before its content row ever renders, and that must not count as painted; see the parity note:
    /// parity-gate regression test for the real case this caught).
    Painted,
    /// The top row's text starts with `needle` -- goto-line lands the target at the top of the
    /// viewport for both pagers (perf.sh's `wait_top_line`).
    TopLineStartsWith(String),
    /// `needle` appears anywhere on the screen -- jump-to-end lands the target at the bottom of
    /// the viewport, not the top (perf.sh's `wait_pane_contains`).
    Contains(String),
    /// Same match as `TopLineStartsWith`, but `poll_predicate` requires it to hold on two
    /// consecutive polls before treating the sample as satisfied -- perf.sh's
    /// `wait_top_line_stable`, scroll-cadence's defense against a transient/partial redraw
    /// landing between two polls (see `poll_predicate`'s own comment for the mechanism this
    /// maps the fixed-cadence bash original onto).
    TopLineStableStartsWith(String),
}

impl Predicate {
    fn eval(&self, screen: &crate::screen::Screen) -> bool {
        match self {
            Predicate::Painted => !screen.row_text(0).is_empty(),
            Predicate::TopLineStartsWith(needle) | Predicate::TopLineStableStartsWith(needle) => {
                screen.row_text(0).starts_with(needle.as_str())
            }
            Predicate::Contains(needle) => screen.contents().contains(needle.as_str()),
        }
    }
}

/// What instant a sample's elapsed time is measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingBasis {
    /// From process launch -- the honest number whenever a subject's own pre-paint cost is
    /// part of what is being measured (perf.sh's deep-goto).
    Launch,
    /// From just before the scenario's keys are sent (i.e. right after readiness succeeds) --
    /// the readiness wait is a correctness gate, not part of the reported number (perf.sh's
    /// jump-end, scroll-cadence).
    Keypress,
}

/// Whether a scenario wants its fixture prewarmed before its sample loop starts.
///
/// `run_sample` never reads this field -- prewarming, if any, is the scenario loop's own disclosed,
/// pre-loop step (perf.sh's `no-prewarm-on-mount` invariant: `run_sample` never prewarms
/// anything). It exists on `Scenario` purely for that outer loop to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prewarm {
    /// The scenario's own driver reads the whole fixture into the page cache first.
    Full,
    /// No prewarm -- the mount leg's scenarios, deliberately measuring cold behavior.
    Skip,
}

/// A subject process this harness can drive -- ress, less, or (in this module's own tests) the
/// fake-pager test double.
#[derive(Debug, Clone, Copy)]
pub struct Subject {
    pub name: &'static str,
    /// Builds the full argv (including the resolved, absolute program path as element 0) for
    /// viewing `fixture` -- `PtyChild::spawn`'s own contract requires an already-resolved
    /// path, since the whitelist environment leaves the child no `PATH` to search.
    pub argv_for: fn(&std::path::Path) -> Vec<std::ffi::OsString>,
    /// Builds this subject's environment whitelist given the per-sample fake home directory
    /// (for `HOME`/`XDG_CONFIG_HOME`/`XDG_DATA_HOME`-shaped entries). `run_sample` layers
    /// `RESS_LOG=info` on top when `tracing` is set -- `env_for` itself never needs to know
    /// about tracing.
    pub env_for: fn(&std::path::Path) -> Vec<(String, String)>,
    /// Whether to append `--log-file` to argv and capture the precise, tracing-based
    /// measurement (ress-only; perf.sh never combines this with a keyed scenario).
    pub tracing: bool,
}

/// One timed measurement's protocol, as data -- what perf.sh's per-scenario `run_sample`
/// call-site options describe.
#[derive(Debug, Clone)]
pub struct Scenario {
    // `name` and `prewarm` are carried as scenario data per the runner's
    // design, but the current loop labels rows and prewarms outside the
    // struct — an interface-vs-implementation drift kept visible here
    // rather than papered over by a module-wide allow.
    #[allow(dead_code)]
    pub name: &'static str,
    /// Sent via `write_keys` once `readiness` succeeds; empty means no keys at all (a
    /// paint-only sample -- perf.sh's `open`). Owned, not `&'static [u8]`: deep-goto's ress leg
    /// sends `:{target}\r` where `target` is a runtime-computed line number (90% of a fixture's
    /// own, only-known-after-reading-its-stats-sidecar line count) -- the same "cannot be a
    /// bare compile-time constant" problem `Predicate`'s own doc comment gives for why it is an
    /// enum of owned data rather than a `fn(&Screen) -> bool`, applied to this field too.
    pub keys: Vec<u8>,
    /// Gates keys being sent (and, if `tracing`, the precise-measurement capture).
    pub readiness: Predicate,
    /// Awaited after keys are sent. `None` means readiness's own success IS the result
    /// (perf.sh's `(( ${#keys[@]} == 0 ))` branch).
    pub completion: Option<Predicate>,
    pub timing_basis: TimingBasis,
    // see `name`'s note — same drift, same visibility choice.
    #[allow(dead_code)]
    pub prewarm: Prewarm,
    /// Whether a completed sample's RSS should be settled and read at all -- see
    /// `rss_kib_if_reported`'s own doc comment for why this gate exists (found in PR #44 round
    /// 16, a codex P2). Only `jump_end_scenario` sets this `true` (report.rs's `RssRow` is the
    /// jump-to-end table -- perf.sh's own `add_rss_row` only ever reported this one metric); the
    /// mount leg's `run_mount_open`/`run_mount_jump_end` inherit the right answer for free via
    /// struct-update syntax off `open_scenario()`/`jump_end_scenario()`, no separate opinion
    /// needed there.
    pub reports_rss: bool,
}

/// The two timeouts a sample can be measured against -- perf.sh's `PAINT_TIMEOUT_S` and
/// `SCENARIO_TIMEOUT_S`, threaded in per call rather than hardcoded, since the cli (not
/// this module) owns `--quick`-style overrides.
#[derive(Debug, Clone, Copy)]
pub struct Ceilings {
    pub readiness_s: u64,
    pub completion_s: u64,
}

/// The taxonomy a sample resolves to -- never an `Err` for a subject that merely hangs or
/// crashes; see `run_sample`'s doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleOutcome {
    Completed {
        ms: u64,
        /// `/proc/<pid>/status` `VmRSS`, settled before being trusted (see `settled_rss_kib`'s
        /// own doc comment) -- `None` when the scenario does not report RSS at all
        /// (`Scenario::reports_rss`, see `rss_kib_if_reported`), or when not even one reading
        /// landed before the process exited (perf.sh's round-9 rule: racing the subject's own
        /// exit is not this sample's failure, never an `Err`).
        rss_kib: Option<u64>,
        /// Ress's tracing-precise first-paint measurement, present only when
        /// `subject.tracing`. Not in the brief's literal `SampleOutcome` sketch -- added
        /// because the brief separately requires this module to produce it, and a `Completed`
        /// sample is the only outcome for which it is ever valid (see the doc comment below).
        precise_ms: Option<u64>,
    },
    Hung {
        /// Whichever ceiling actually fired -- perf.sh's `SAMPLE_HUNG_CEILING` attribution:
        /// `readiness_s` if the readiness wait timed out, `completion_s` if the completion
        /// wait did.
        ceiling_s: u64,
    },
    Crashed,
}

/// Runs one timed sample of `scenario` against `subject`, spawned through a fresh pty against
/// `fixture`. Returns the outcome -- a subject that hangs or crashes is a recorded result, not
/// this function's own failure, exactly as `scripts/perf.sh`'s `run_sample` treated them.
/// `Err` is reserved for a genuine harness fault: a spawn failure, a missing tracing log line,
/// or a `write_keys` failure against a subject that is somehow still alive.
///
/// This function ports `scripts/perf.sh`'s `run_sample` and its header comment's invariant
/// list (read at HEAD before this port -- see the task report for the full reading). Each
/// invariant below is marked STRUCTURAL (the pty/`Command`/`Drop` substrate upholds it; nothing
/// in this function's own control flow has to), BEHAVIORAL (this function's control flow is
/// what upholds it, same as perf.sh's bash logic), or OUT OF SCOPE (perf.sh disclaimed it as
/// "not this function's concern" too -- listed for completeness, not implemented here):
///
/// - clock-before-spawn (STRUCTURAL, `t0` only): `t0` is captured in this function, immediately
///   before `PtyChild::spawn`, with nothing else able to intervene -- perf.sh needed a
///   documented ordering discipline across many call sites; here it is two adjacent statements.
///   The OTHER clock this function cares about, the tracing-precise exec stamp, deliberately
///   does NOT follow this invariant at all -- it isn't captured in this function in the first
///   place, but inside the child itself, by `PtyChild::spawn_capturing_exec_stamp` (see its own
///   doc comment for why a parent-side stamp, however early, can never be exec-adjacent enough).
/// - whitelist (STRUCTURAL, at the `PtyChild::spawn` layer): `Command::env_clear()` plus
///   exactly `env`'s entries is `pty.rs`'s own contract. This function's job is only to hand it
///   the right list (`subject.env_for`, plus `RESS_LOG` under tracing), not to re-enforce the
///   clearing itself.
/// - exec (STRUCTURAL): `std::process::Command` execve's `argv[0]` directly; no shell is ever
///   constructed, so there is no string a fixture's own characters could break out of, and
///   nothing depends on a shell's tail-call optimization the way the old tmux launch string did.
/// - comm (STRUCTURAL, subsumed): perf.sh's `assert_pane_is`/`pane_crashed` compared
///   `/proc/<pid>/comm` against the expected pager because a broken exec chain could leave a
///   wrapping shell at that pid instead. With no shell in the chain at all, the pid
///   `PtyChild::spawn` returns IS the subject's pid by construction -- there is no chain left
///   to verify, so no runtime check reproduces this one.
/// - atomic fixtures (OUT OF SCOPE): a sample only ever reads a fixture the fixture
///   generation already produced atomically -- this function never writes one, exactly as
///   perf.sh's `run_sample` never did either.
/// - partial-pair abort (OUT OF SCOPE): likewise the fixture/mount-leg machinery's concern.
/// - no-prewarm-on-mount (OUT OF SCOPE): see `Prewarm`'s doc comment -- `scenario.prewarm` is
///   never read here.
/// - hang-as-result (BEHAVIORAL, made strictly more precise): a wait timing out is `Hung`, not
///   an error -- ported as-is. But perf.sh could only tell a hang from a crash by revalidating
///   AFTER a wait's full ceiling elapsed (a separate `pane_crashed` poll). Here, `try_wait` and
///   EOF are interleaved into the SAME poll loop, so a subject that dies is observed and
///   recorded as `Crashed` the instant it happens, never after burning the ceiling -- see the
///   taxonomy tests' sub-500ms assertions, which pin exactly this property.
/// - no accidental abort on the way there (MOOT): this invariant existed to defend against
///   bash's `set -e` silently taking the whole harness down when a bare statement (like
///   send-keys) failed against an already-dead target. Every fallible call in this function is
///   a `Result` that must be explicitly handled (via `?` or a match) -- there is no silent,
///   whole-process abort mode for an unchecked failure to trigger.
/// - Linux gate (OUT OF SCOPE): not this function's concern -- main.rs's precondition check.
/// - forced target dir (OUT OF SCOPE): not this function's concern, and irrelevant to this
///   substrate anyway -- cargo resolves binary paths itself, no `CARGO_TARGET_DIR` coupling.
/// - teardown (STRUCTURAL): `PtyChild`'s `Drop` (killpg + reap) and this function's own
///   per-sample fake-home directory's `Drop` both run on every exit path -- including an early
///   `?` propagation -- for free, unlike perf.sh's manual `kill_session_quiet` before every
///   `return`.
///
/// Not ported as a named invariant: even-count alternation (which subject goes first). That is
/// `for_each_sample`'s concern in perf.sh, a loop this function has no equivalent of -- it runs
/// exactly one sample of one subject against one scenario. scenarios.rs's sample loop owns that
/// decision now.
///
/// A rule for this MODULE's own tests, stated once here rather than re-derived at each site:
/// tests prove EVENTS -- a kernel-verified fact like `EAGAIN`, an injected liveness/sleeper
/// answer, a real handshake between a subject and the poll loop watching it -- never SCHEDULER
/// TIMING. No test in this file may assert on how fast the OS happened to run it (an elapsed
/// `Duration` compared against a tuned threshold, a `write(2)` call's own duration used as a
/// stand-in for "did that block"). Found the hard way, twice, in the same file: a fake-pager
/// mode, `buffer_stall_then_marker` (since retired in pass 7, P7-D -- its own consuming test
/// converged onto a `ScriptedTransport` once v3/zero left the real-pty construction unable to
/// exercise anything, see `deadline_drain_caps_each_read_to_the_shrinking_remainder`'s own doc
/// comment), once inferred "the buffer is full" from how long a write took, which scheduler
/// preemption could fake; a `poll_predicate` test once asserted an elapsed-time budget directly,
/// which needed re-measuring and widening once under real contention, and was retired as a class
/// rather than tuned a second time (see
/// `would_block_sleep_request_never_exceeds_the_clamp_or_poll_interval`, this module's own
/// tests). Where a real clock genuinely cannot be avoided (this file has
/// exactly one such case: `tracing_precise_ms_reports_subject_died_quickly_even_with_the_real_
/// flush_timeout`, which exists specifically to bound worst-case LATENCY, not to prove a
/// kernel-verified fact), the bound is a wide fraction of a real ceiling, never a tight number
/// tuned against a single machine's own observed timing.
///
/// Tracing-precise measurement (only when `subject.tracing`): mirrors perf.sh's
/// `_tracing_precise_ms`, without perf.sh's wrapper/stamp-file indirection -- `exec_stamp` (a
/// `SystemTime` captured by the CHILD itself, inside `pre_exec`, immediately before its own
/// exec -- see `PtyChild::spawn_capturing_exec_stamp`'s own doc comment for the full mechanism
/// and why a parent-side stamp can never be exec-adjacent enough) stands in for the wrapper's
/// `$EPOCHREALTIME` stamp, and ress's own `perf: first paint` log line (`ress/src/app.rs`) is
/// parsed the same way, via `timegm` rather than a `date -d` subprocess. Computed once readiness
/// succeeds, regardless of what a completion phase later decides -- but only ever surfaced on
/// `Completed`, which tightens perf.sh's own documented (if not, for one untested combination,
/// actually enforced) contract: "valid if not hung and not crashed." Opportunistic like
/// `read_rss_kib` for its own `Reading` outcome -- see `tracing_precise_ms`'s own doc comment
/// for why that inner value is `Option<u64>`, not a fallible `Result` any caller has to unwrap
/// or propagate, and (found in PR #44 round 11) for the one OTHER outcome the wrapping
/// `TracingOutcome` enum exists for: a subject that died before it could ever write its own log
/// line, mapped straight to `SampleOutcome::Crashed` rather than either the opportunistic
/// no-data case or the genuinely-broken-instrumentation hard failure.
///
/// RSS (`/proc/<pid>/status` `VmRSS`): settled, not merely read opportunistically, and gated to
/// `Scenario::reports_rss` (`jump_end_scenario` alone, not every `Completed` path) -- see
/// `rss_kib_if_reported`/`settled_rss_kib`'s own doc comments for the full contract and why it
/// changed from perf.sh's own single immediate read.
pub fn run_sample(
    subject: &Subject,
    scenario: &Scenario,
    fixture: &std::path::Path,
    ceilings: &Ceilings,
) -> anyhow::Result<SampleOutcome> {
    let fake_home = FakeHome::create()?;
    let mut env = (subject.env_for)(fake_home.path());
    let mut argv = (subject.argv_for)(fixture);
    let log_path = fake_home.path().join("subject.log");
    if subject.tracing {
        argv.push(std::ffi::OsString::from("--log-file"));
        argv.push(log_path.clone().into_os_string());
        env.push(("RESS_LOG".to_string(), "info".to_string()));
    }
    let t0 = std::time::Instant::now();
    // round 3 shipped a PARENT-side stamp taken right after spawn() returned, reasoning it was
    // the closest a parent-side stamp could get to the child's own exec. Round 4's review found
    // that reasoning itself wrong: `Command::spawn`'s Unix path internally waits on its own
    // sync pipe for the child's exec to succeed before returning, so the parent's "as soon as
    // possible" is already SOMETIME AFTER the true exec instant -- by an amount that grows with
    // host scheduling contention, large enough on a loaded host for `ress`'s own first-paint
    // log line to already exist by the time this stamp was taken, which flipped the subtraction
    // negative and aborted the entire run. There is exactly one place a timestamp can be
    // genuinely exec-adjacent: taken by the CHILD itself, inside `pre_exec`, immediately before
    // `exec` -- literally where perf.sh's own deleted wrapper stamped. `t0` (above) deliberately
    // stays where it is regardless -- it backs `TimingBasis::Launch` and every ceiling deadline,
    // both of which are SUPPOSED to include spawn cost (that inclusiveness is the whole reason a
    // separate precise number exists at all); only the exec-adjacent stamp moves.
    let (mut child, exec_stamp) = if subject.tracing {
        crate::pty::PtyChild::spawn_capturing_exec_stamp(&argv, &env, (SCREEN_ROWS, SCREEN_COLS))?
    } else {
        (
            crate::pty::PtyChild::spawn(&argv, &env, (SCREEN_ROWS, SCREEN_COLS))?,
            None,
        )
    };
    let mut screen = crate::screen::Screen::new(SCREEN_ROWS, SCREEN_COLS);
    let mut buf = Vec::new();
    let readiness_deadline = t0 + std::time::Duration::from_secs(ceilings.readiness_s);
    let readiness_ms = match poll_predicate(
        &mut child,
        &mut screen,
        &mut buf,
        &scenario.readiness,
        t0,
        readiness_deadline,
        None,
    )? {
        PollOutcome::Satisfied(ms) => ms,
        PollOutcome::Crashed => return Ok(SampleOutcome::Crashed),
        PollOutcome::TimedOut => {
            return Ok(SampleOutcome::Hung {
                ceiling_s: ceilings.readiness_s,
            });
        }
    };
    let precise_ms = match tracing_precise_ms(
        subject.tracing,
        exec_stamp,
        &log_path,
        LOG_FLUSH_TIMEOUT,
        &mut child,
    )? {
        TracingOutcome::Reading(ms) => ms,
        // found in PR #44 round 11: see tracing_precise_ms's own doc comment for the full
        // derivation -- a dead subject explains its own missing log line, the round-1 crash
        // path reached from this new entry point instead of the no-completion branch below.
        TracingOutcome::SubjectDied => return Ok(SampleOutcome::Crashed),
    };
    let t_basis = match scenario.timing_basis {
        TimingBasis::Launch => t0,
        TimingBasis::Keypress => std::time::Instant::now(),
    };
    if !scenario.keys.is_empty()
        && let Err(err) = child.write_keys(&scenario.keys)
    {
        return match child.try_wait() {
            Ok(Some(_)) => Ok(SampleOutcome::Crashed),
            Ok(None) => Err(err.context(format!(
                "write_keys failed but subject {:?} is still alive",
                subject.name
            ))),
            Err(wait_err) => Err(wait_err),
        };
    }
    let Some(completion) = &scenario.completion else {
        // readiness's own success IS the sample (no completion phase to catch a post-readiness
        // death via its own poll_predicate call) -- so it gets the one revalidation perf.sh's
        // run_sample always did right after a successful wait. See
        // `no_completion_phase_still_alive`'s own doc comment for the full derivation of why
        // BOTH outcomes below are legitimate, depending on the interleaving -- this is a
        // referee, not a bug fix pinning one side.
        return if no_completion_phase_still_alive(|| Ok(child.try_wait()?.is_none()))? {
            Ok(SampleOutcome::Completed {
                ms: readiness_ms,
                rss_kib: rss_kib_if_reported(scenario.reports_rss, || {
                    settled_rss_kib(RSS_SETTLE_CEILING, || read_rss_kib(child.pid()))
                }),
                precise_ms,
            })
        } else {
            Ok(SampleOutcome::Crashed)
        };
    };
    let completion_deadline = t_basis + std::time::Duration::from_secs(ceilings.completion_s);
    match poll_predicate(
        &mut child,
        &mut screen,
        &mut buf,
        completion,
        t_basis,
        completion_deadline,
        None,
    )? {
        PollOutcome::Satisfied(ms) => Ok(SampleOutcome::Completed {
            ms,
            rss_kib: rss_kib_if_reported(scenario.reports_rss, || {
                settled_rss_kib(RSS_SETTLE_CEILING, || read_rss_kib(child.pid()))
            }),
            precise_ms,
        }),
        PollOutcome::Crashed => Ok(SampleOutcome::Crashed),
        PollOutcome::TimedOut => Ok(SampleOutcome::Hung {
            ceiling_s: ceilings.completion_s,
        }),
    }
}

#[derive(Debug)]
enum PollOutcome {
    Satisfied(u64),
    Crashed,
    TimedOut,
}

// found in PR #44 round 13 (a codex P2, accepted): the loop's own WouldBlock-arm sleep used to
// be an unclamped `POLL_INTERVAL` regardless of how close `deadline` already was -- so a sleep
// beginning just before the deadline could wake up to a full `POLL_INTERVAL` PAST it. That
// straddle mattered beyond mere lateness: pass 3's own deadline-branch fix (`queued_read_bytes`,
// snapshotted as the very first thing that branch does) bounds WHAT the recovery drain may
// accept to what was genuinely queued at snapshot time, but does nothing about WHEN that
// snapshot is taken -- an unclamped, late wake meant the snapshot itself could be taken up to a
// full `POLL_INTERVAL` after the true deadline, silently including bytes the subject wrote
// during the straddle. Clamping the sleep closes this at the source: capped to whatever time
// genuinely remains before `deadline` (saturating -- never negative, zero is a valid, immediate
// sleep), so a wake can never land meaningfully past the wall, only by irreducible scheduler
// jitter -- the same residual imprecision any timer has, not a designed-in gap. Extracted as its
// own pure function (rather than inlined at the one call site) specifically so the clamp
// arithmetic itself is directly testable against synthetic `Instant`s, without needing to spawn
// a child or race real wall-clock timing to exercise it.
//
// found in PR #44 pass 6 S1 (a user-review finding, #3): `next_checkpoint` closes the OTHER half
// of the reported ~18ms `TopLineStableStartsWith` cadence (documented as ~10ms, docs/perf.md).
// Before this, every wait request was bounded only by `deadline`/`POLL_INTERVAL`, never by how
// soon the NEXT regular checkpoint was due -- so a wait that woke slightly early (e.g. `pty.rs`'s
// own since-fixed millisecond-floor undershoot, but even a perfectly precise wait can wake early
// relative to a checkpoint that was already imminent) fell through to the checkpoint check,
// found it not-yet-due by a sliver, and requested ANOTHER full `POLL_INTERVAL` wait rather than
// just the sliver actually remaining -- two ~10ms waits land a checkpoint at ~20ms, not one.
// `Some(next_checkpoint)` only from the `TopLineStableStartsWith` call site (the only predicate
// variant that ever reads `next_checkpoint` at all); every other predicate passes `None`,
// preserving today's two-way (`deadline`/`POLL_INTERVAL`) clamp exactly.
fn clamped_poll_sleep(
    now: std::time::Instant,
    deadline: std::time::Instant,
    next_checkpoint: Option<std::time::Instant>,
) -> std::time::Duration {
    let bound = match next_checkpoint {
        Some(checkpoint) => deadline.min(checkpoint),
        None => deadline,
    };
    bound.saturating_duration_since(now).min(POLL_INTERVAL)
}

// found in PR #44 round 16 (a codex P2), reworked in pass 7 (P7-A/B, the reviewer's own explicit
// deadline-admission state machine): the deadline branch's own drain budget, extracted as its
// own pure function so the arithmetic is directly testable against synthetic inputs -- see
// `poll_predicate_with_waiter`'s own doc comment for the full derivation.
//
// `last_pre_ceiling_fionread: Option<usize>` is FINALIZING's only pre-deadline evidence:
// `Some(n)` from an ordinary Admitting-state Data iteration's own pre-read snapshot, still valid;
// `None` in BOTH cases nothing trustworthy exists -- no Admitting iteration has ever completed at
// all, reachable whenever no iteration BEFORE the one that finally reaches FINALIZING ever wrote
// `Some` (confirmed by derivation at this function's own call sites, not assumed; a p6-review2
// finding, pass 7 delta review, corrected an over-narrow "only the first iteration" claim this
// comment used to make here -- see this paragraph's own closing note). `Some` is written in
// exactly one place (the Data arm below, once a recheck has passed) -- so `None` survives as long
// as every iteration up to that point either never attempted a snapshot at all (`basis ==
// deadline`, or close enough that scheduling delay before the very first deadline check already
// consumed the whole remaining budget), took the zero-snapshot path (which never writes `Some`
// either -- see its own comment below), or had its own recheck fail (discarded via the recheck's
// own `continue` without ever writing `last_pre_ceiling_fionread` -- see the recheck's own
// comment). Any MIX of those across any number of iterations still leaves `None`; a recheck
// failure is not special to the first iteration, only to iterations that have not yet reached a
// passed-recheck Data arm (once one has, `Some` survives every LATER recheck failure untouched,
// since a recheck failure discards only the CURRENT snapshot). OR a waiter timed out on a wait
// that was itself bounded by `deadline`, which by that wait's own contract proves nothing became
// readable during it. `NeverTracked` and a proven-by-timeout zero
// used to be distinguished (an earlier `PreCeilingSnapshot` enum, with `NeverTracked` alone still
// taking a fresh FIONREAD) -- retired: both cases have equally NO pre-deadline evidence to spend,
// so both correctly get zero, uniformly, with no fresh FIONREAD call anywhere in this function at
// all. That makes invariant 4 ("no FIONREAD after expiry") hold with NO exception, not one
// documented residual -- the purest version of the design, not a compromise. No longer fallible
// either (no injected closure left to propagate an `Err` from).
//
// `saturating_sub`, never a bare `-`: `consumed_since_snapshot` can legitimately exceed the
// snapshot (an ordinary read landing right on the boundary can sweep up both the last
// provably-pre-ceiling remainder AND some genuinely post-ceiling bytes in the SAME call -- those
// extra bytes are already fed to `screen` via the ordinary per-iteration read+feed step
// regardless, so the recovery drain simply has nothing further it can prove and correctly gets a
// zero budget, not a wrapped-negative one).
fn deadline_drain_budget(
    last_pre_ceiling_fionread: Option<usize>,
    consumed_since_snapshot: usize,
) -> usize {
    last_pre_ceiling_fionread.map_or(0, |n| n.saturating_sub(consumed_since_snapshot))
}

// found in PR #44 pass 7 (P7-A/B, the reviewer's own structural recommendation): the read
// surface `poll_predicate_with_waiter`/`drain_available` drive -- FIONREAD, the bounded read,
// liveness -- until now was only ever a real `PtyChild`, even though the wait/clock seams
// alongside it were already fully injectable (`waiter`/`now`, below). A test that wants to
// script a full admission scenario (e.g. "a snapshot proves 5 bytes queued, the read returns
// exactly those 5, then the clock crosses the deadline before the next check") still needed a
// real spawned child to supply the read side, coupling every such test to real process/pty
// timing regardless of how deterministic its clock and wait already were. `PtyTransport` closes
// that: the read-surface operations this loop needs, as a trait `PtyChild` implements for
// production and a test can implement with a fully scripted fake -- no real process, no real
// pty, anywhere in an admission-POLICY test (see `ScriptedTransport`, this module's own tests,
// and the crate's own transport-primitive tests in `pty.rs` for what stays real).
//
// deliberately no unbounded read method here at all: `drain_available`'s own deadline-branch
// drain and the normal path's own bounded read both already only ever call
// `read_available_bounded` -- the zero-snapshot case (invariant 2) needs no read at all, decided
// via `try_wait`/`hung_up` instead (see `poll_predicate_with_waiter`'s own doc comment). Leaving
// an unbounded read out of the trait entirely means the type does not offer the footgun, rather
// than merely relying on every caller choosing not to reach for it.
trait PtyTransport {
    fn queued_read_bytes(&self) -> anyhow::Result<usize>;
    fn read_available_bounded(
        &mut self,
        buf: &mut Vec<u8>,
        max_bytes: usize,
    ) -> anyhow::Result<crate::pty::ReadStatus>;
    fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>>;
    fn hung_up(&self) -> anyhow::Result<bool>;
    fn raw_master_fd(&self) -> libc::c_int;
}

impl PtyTransport for crate::pty::PtyChild {
    fn queued_read_bytes(&self) -> anyhow::Result<usize> {
        crate::pty::PtyChild::queued_read_bytes(self)
    }
    fn read_available_bounded(
        &mut self,
        buf: &mut Vec<u8>,
        max_bytes: usize,
    ) -> anyhow::Result<crate::pty::ReadStatus> {
        crate::pty::PtyChild::read_available_bounded(self, buf, max_bytes)
    }
    fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        crate::pty::PtyChild::try_wait(self)
    }
    fn hung_up(&self) -> anyhow::Result<bool> {
        crate::pty::PtyChild::hung_up(self)
    }
    fn raw_master_fd(&self) -> libc::c_int {
        crate::pty::PtyChild::raw_master_fd(self)
    }
}

// drives `child` through one poll loop until `predicate` is satisfied against the
// accumulating `screen` (Satisfied), `child` exits without ever satisfying it (Crashed), or
// `deadline` passes (TimedOut). `deadline` is an absolute instant checked every iteration --
// never a fresh timeout re-derived per iteration, matching perf.sh's own wait_* invariant
// (chained waits must not each get a full fresh budget). `stable_confirmed_at_start` seeds the
// `TopLineStableStartsWith` checkpoint state this call begins with -- always `None` from
// `run_sample`'s own two call sites (a fresh readiness or completion poll never has an earlier
// checkpoint to inherit); a round-5 test seeds `Some(generation)` instead, to exercise the
// deadline branch's own stability-pair-completion path deterministically, without racing real
// `POLL_INTERVAL` timing to land a checkpoint immediately before the deadline (see that branch's
// own comment, and the tests below, for why). `Option<u64>`, not `bool`, since pass 5 (C2): a
// confirmed tick now carries the generation `Screen::top_row_generation` reported AT that tick,
// not just a bare "yes, matched" flag -- see this function's own checkpoint logic below for why.
// Since the post-merge batch (fix #5) the seed carries the confirmation INSTANT too, because the
// deadline branch now enforces the documented pair spacing against the wall.
fn poll_predicate(
    child: &mut crate::pty::PtyChild,
    screen: &mut crate::screen::Screen,
    buf: &mut Vec<u8>,
    predicate: &Predicate,
    basis: std::time::Instant,
    deadline: std::time::Instant,
    stable_confirmed_at_start: Option<(u64, std::time::Instant)>,
) -> anyhow::Result<PollOutcome> {
    poll_predicate_with_waiter(
        child,
        screen,
        buf,
        predicate,
        basis,
        deadline,
        stable_confirmed_at_start,
        &mut crate::pty::wait_for_readable,
        &mut std::time::Instant::now,
    )
}

// found in PR #44 pass 5 (B3, a codex P2), generalized in pass 5 commit 3: round 13's own test
// proved the clamp fix by asserting on real wall-clock elapsed time -- a SCHEDULER-TIMING oracle
// by class, not merely by one badly-chosen threshold. `waiter` is the seam that makes the loop's
// own single wait site injectable: production calls `poll_predicate` above, which supplies the
// real `crate::pty::wait_for_readable` (a `poll(2)` wait on the master fd, replacing this arm's
// former blind `std::thread::sleep` -- see `wait_for_readable`'s own doc comment for why: the
// scroll-cadence investigation's own dose-response evidence found the port's checkpoint-paced
// sleep, not the generation gate, explains the perf.sh gap, and `tmux`'s own event-driven wake
// is the faithful terminal model that closes it). A test can instead supply a RECORDING closure
// that logs the requested `(fd, Duration)` without ever actually polling, then assert directly
// on what the loop REQUESTED -- deterministic, no wall clock involved in the assertion at all
// (see `would_block_wait_request_never_exceeds_the_clamp_or_poll_interval`, this module's own
// tests, the direct port of B3's own recording-sleeper test to this shape). The fd is a plain
// argument, not something the closure captures -- see `PtyChild::raw_master_fd`'s own doc
// comment for why capturing `child` itself would alias against this same loop's own concurrent
// `&mut` use of it. Kept minimal on purpose: a plain `&mut dyn FnMut(c_int, Duration) ->
// Result<PollReadiness>`, not a named trait or any async machinery. The eighth parameter pushes
// this past clippy's default `too_many_arguments` threshold (7); bundling these into a params
// struct purely to satisfy the lint would be more machinery than the seam itself, for a function
// whose real complexity is the poll loop's own body, not its parameter count -- allowed
// explicitly rather than restructured.
//
// found in PR #44 pass 6 S1 (a user-review finding, #2), superseded in pass 7 (P7-A/B): a
// separate injected `fionread` closure used to generalize the SAME seam to the loop's own
// FIONREAD snapshots. `PtyTransport`, above, now covers that seam directly -- `child` itself
// (real `PtyChild` in production, `ScriptedTransport` in a policy test) is the injection point
// for every read-surface call this function makes (FIONREAD, the bounded read, liveness), not a
// parallel closure alongside it. See `normal_path_read_never_consumes_past_an_injected_pre_read_
// snapshot`, this module's own tests, for the test this now exercises via a scripted transport
// instead of a real child with an injected snapshot closure layered over it.
//
// found in PR #44 pass 6 S1 (closing the loop on the same finding): `now` generalizes the SAME
// seam a third time, to every wall-clock read this function takes (`next_checkpoint`'s own
// init, the top-of-loop deadline check, the NEW post-snapshot recheck pass 7 adds, the regular
// checkpoint's own timer check and re-arm, and `clamped_poll_sleep`'s own `now` argument) --
// production calls `poll_predicate` above, which supplies the real `std::time::Instant::now`,
// the same "pass the bare function item itself" shape already used for `waiter` (`&mut
// crate::pty::wait_for_readable`), since `Instant::now` is itself a zero-arg `fn() -> Instant`.
// Exists so a test can force the deadline to cross on a SPECIFIC, scripted iteration --
// deterministically, not by racing real elapsed time against a real writer -- so the deadline
// branch can be proven to fire strictly AFTER at least one real normal-path iteration has
// already tracked a pre-ceiling snapshot (the "mid-burst" case finding #2 exists for), rather
// than only ever the simpler "already expired on entry" case (`timeout_when_no_admitting_
// iteration_ever_ran_before_the_deadline`, this module's own tests, needs no scripting at all
// for -- a real `basis == deadline` already forces it). See
// `deadline_branch_uses_the_tracked_snapshot_not_a_fresh_one_mid_burst`, this module's own
// tests, for the test this seam exists for.
//
// found in PR #44 pass 7 (P7-A/B, the reviewer's own explicit deadline-admission state machine):
// this function now realizes exactly the two states the reviewer asked for. ADMITTING is
// everything from the top-of-loop deadline check down to (but not including) that check firing
// true: every snapshot taken here is immediately re-checked against the clock before anything it
// covers can reach `screen.feed` (invariant 1), a zero snapshot is resolved via `try_wait`/
// `hung_up` with no read of any kind (invariant 2), and a timed-out waiter marks
// `last_pre_ceiling_fionread = None` explicitly rather than leaving its own result to be silently
// re-derived (invariant 3) -- the SAME `None` a never-yet-tracked iteration already carries,
// since a wait bounded by the deadline that produced nothing readable and "nothing has ever been
// checked yet" are equally devoid of pre-deadline evidence to spend. FINALIZING is the
// top-of-loop check's own `true` branch: a one-shot judgment that spends only what
// `deadline_drain_budget` computes from `last_pre_ceiling_fionread` -- NO fresh, ad-hoc FIONREAD
// call anywhere in this function at all (invariant 4, with no exception, uniform across both
// states instead of split between a normal-path fix and a deadline-branch apparatus bolted on
// beside it).
#[allow(clippy::too_many_arguments)]
fn poll_predicate_with_waiter(
    child: &mut dyn PtyTransport,
    screen: &mut crate::screen::Screen,
    buf: &mut Vec<u8>,
    predicate: &Predicate,
    basis: std::time::Instant,
    deadline: std::time::Instant,
    stable_confirmed_at_start: Option<(u64, std::time::Instant)>,
    waiter: &mut dyn FnMut(
        libc::c_int,
        std::time::Duration,
    ) -> anyhow::Result<crate::pty::PollReadiness>,
    now: &mut dyn FnMut() -> std::time::Instant,
) -> anyhow::Result<PollOutcome> {
    // `TopLineStableStartsWith` only: `None` if the last checkpoint did not match (or there
    // has not been one yet); `Some(generation)` if it did, carrying the `Screen::
    // top_row_generation` reading AT that moment -- perf.sh's `wait_top_line_stable` own
    // `confirmed` flag, sharpened (pass 5, C2) to also remember WHICH generation it confirmed
    // at, so a later check can tell "still that same, unbroken span" apart from "matches again,
    // coincidentally, after an observed detour."
    let mut stable_confirmed = stable_confirmed_at_start;
    // `TopLineStableStartsWith` only: the wall-clock instant the NEXT checkpoint is due --
    // matches perf.sh's own `wait_top_line_stable` fixed-tick sampler (a check on entry, then
    // one every `POLL_S`), and see the main loop body's own comment below for why this must be
    // driven by elapsed TIME rather than by the pty going idle.
    let mut next_checkpoint = now();
    // found in PR #44 round 16 (a codex P2), reworked in pass 7 (P7-A/B): tracks the most recent
    // evidence FINALIZING can trust when it fires -- see `deadline_drain_budget`'s own doc
    // comment for how it's spent. Set to `Some(n)` by a successful (recheck-passed) ADMITTING
    // read below; explicitly reset to `None` by a timed-out waiter (rather than left stale); and
    // starts at `None` too, so "never tracked anything" and "a wait just proved zero" are the
    // identical value by construction, not two cases coincidentally spending the same -- see
    // `deadline_drain_budget`'s own doc comment for the full derivation of how `None` reaches
    // FINALIZING with no Admitting iteration ever having completed a passed-recheck Data arm; a
    // timed-out wait is a separate, unrelated route, reaching `None` by an explicit reset rather
    // than by never having been set at all.
    let mut last_pre_ceiling_fionread: Option<usize> = None;
    let mut consumed_since_snapshot: usize = 0;
    loop {
        if now() >= deadline {
            // a subject that died while this loop was asleep (the WouldBlock branch's own sleep
            // is clamped, round 13, to never extend PAST the deadline -- but it is not
            // eliminated, only bounded to whatever time remains before the wall, so the subject
            // can still die during it: still alive at the last try_wait check, dead by the time
            // this branch fires) would otherwise never get a chance to be observed as dead
            // before TimedOut wins the race -- one final revalidation here, mirroring perf.sh's
            // own post-timeout pane_crashed poll, so a near-ceiling crash is reported as Crashed
            // rather than folded into the Hung bucket.
            //
            // found in PR #44's round-4 review: an earlier version of this recheck only ever
            // looked for Eof, discarding a Data read outright -- a frame that arrived and was
            // sitting in the pty's own kernel buffer before the deadline (this loop simply
            // slept through it, per POLL_INTERVAL's own cadence) got thrown away, reporting
            // Hung for a sample that had actually succeeded. Data is checked FIRST here,
            // matching the main loop body's own screen-first ordering just below (predicate
            // satisfaction is always evaluated before anything status-based) -- and correctly
            // so even when the subject also happens to be dead by now: draining whatever is
            // CURRENTLY buffered regardless of the child's exit state means a matching frame
            // that arrived before death is the honest completed outcome (the same
            // "post-completion death is an RSS-only fact, not a Crashed downgrade" precedent
            // `run_sample`'s own doc comment already states for why this function does not
            // revalidate AFTER a completion predicate is satisfied). A sleep-straddling success
            // like this is recorded at OBSERVATION time (now), not the frame's true arrival
            // time -- the honest anchor actually available here, same as every other
            // `Satisfied` return in this function.
            //
            // found in PR #44's round-5 review: `TopLineStableStartsWith` used to be excluded
            // from this recovery entirely (a bare `!matches!(predicate,
            // Predicate::TopLineStableStartsWith(_))` guard), on the reasoning that "stable"
            // means two CONSECUTIVE checkpoints and this branch only ever gets one look. That
            // reasoning missed the case where the loop slept across the deadline right after its
            // FIRST checkpoint already matched (`stable_confirmed` from a real earlier
            // checkpoint, set before this branch ever ran): if nothing has changed since --
            // scroll-cadence's own fake-pager equivalent holds the exact same top line forever
            // once settled -- the buffered re-read right here IS the second consecutive
            // confirmation, just taken by this branch instead of the next checkpoint tick.
            // Denying that recovery reported Hung for a sample that had actually settled, the
            // same shape of bug the Eof-only recheck above was already fixed for in round 4.
            //
            // The other half matters just as much: this branch must never START a stability pair
            // on its own. `stable_confirmed` being `None` (pass 5: was a bare `false`) means
            // either no real checkpoint has happened yet, the last one did NOT match, or one DID
            // match but an observed change since reset it -- either way, a single deadline-branch
            // match here would be this loop's first (or only) look at that line, with nothing
            // earlier to pair it against, so it stays unconfirmed and the sample stays TimedOut
            // exactly as before this fix.
            //
            // found in PR #44 pass-2 review: this branch used to make its final judgment from a
            // SINGLE `read_available` call. A pty master's `read()` can return fewer bytes than
            // are actually queued (a real tty line-discipline quirk, not merely "everything
            // currently available" -- reproduced directly: a satisfying marker placed at the end
            // of a payload sized past one such short read landed in the remainder still queued
            // BEHIND the one chunk this branch used to look at, misclassifying a sample that had
            // actually succeeded as Hung). This is the branch's own FINAL, one-shot judgment --
            // unlike the main loop body below, it gets no further iterations to catch a
            // remainder on a later pass -- so it now drains fully via `drain_available` before
            // deciding anything, thorough specifically because it is the last look this function
            // ever takes.
            //
            // found in PR #44 pass 3: the byte count is snapshotted here, as the very first thing
            // this branch does -- before `drain_available`'s own first read -- so it reflects
            // exactly what had arrived by the time the deadline was detected, not some later
            // instant after this branch's own (comment-only, non-executing) prose above. See
            // `drain_available`'s own doc comment for why the drain is bounded to exactly this
            // count.
            //
            // found in PR #44 round 16, reworked in pass 7 (P7-A/B): FINALIZING's own one-shot
            // judgment spends exactly what `deadline_drain_budget` computes from
            // `last_pre_ceiling_fionread` -- no FIONREAD call of any kind happens in this branch;
            // the value was already established (or wasn't) entirely by ADMITTING, above.
            let queued_at_deadline =
                deadline_drain_budget(last_pre_ceiling_fionread, consumed_since_snapshot);
            let status = drain_available(child, buf, screen, queued_at_deadline)?;
            // found in PR #44 pass 5 (C2): a prior confirmation only still counts if row 0's
            // own generation (including whatever `drain_available` just fed in) has not moved
            // since -- an observed intermediate change, even one that reverted to the identical
            // matching content, resets the pair, since the two checks this predicate exists to
            // confirm are no longer bracketing a genuinely unchanged span. See `Screen::feed`'s
            // own doc comment for exactly what "observed" can and cannot see.
            let satisfied = match predicate {
                Predicate::TopLineStableStartsWith(_) => match stable_confirmed {
                    // post-merge batch (2026-07-22), fix #5: the pair must ALSO span the
                    // documented poll interval, measured against the wall itself (the branch
                    // fires at the wall; measuring against a possibly-later `now()` would make
                    // acceptance depend on scheduling). `saturating_duration_since` covers a
                    // confirmation stamped at-or-after the wall by a descheduled checkpoint:
                    // zero span, correctly refused.
                    Some((generation, confirmed_at)) => {
                        generation == screen.top_row_generation()
                            && deadline.saturating_duration_since(confirmed_at) >= POLL_INTERVAL
                            && predicate.eval(screen)
                    }
                    None => false,
                },
                _ => predicate.eval(screen),
            };
            if satisfied {
                // found in PR #44 round 9: for TopLineStableStartsWith specifically, `satisfied`
                // above can be true purely because an EARLIER checkpoint already set
                // `stable_confirmed` and the CURRENT re-read simply still matches -- which it
                // always will against a screen the child died without ever getting to update
                // again (dead pagers don't redraw). The stability pair this predicate exists to
                // confirm is "the top line held steady across two LIVE observations"; a frozen
                // aftermath looking stable is not the same event. Boundary, derived against
                // round 4's own declined finding so the two stay consistent rather than
                // contradicting each other: death AFTER a completed confirmation is an
                // RSS-only fact, `Completed` stands (the event already happened -- this
                // function's own doc comment states why it never revalidates after THAT point);
                // death BEFORE the confirming checkpoint means the event never completed at all
                // -- `Crashed`, not `Completed`. This branch is exactly the "before" case: the
                // confirmation has not happened yet, only the frozen APPEARANCE of one has. See
                // the regular checkpoint's own identical gate below for the same reasoning,
                // needed here too since this deadline branch is its own equally-final "second
                // look" (its own comment above already establishes it gets no further
                // iterations).
                if matches!(predicate, Predicate::TopLineStableStartsWith(_))
                    && (matches!(status, crate::pty::ReadStatus::Eof)
                        || child.try_wait()?.is_some())
                {
                    return Ok(PollOutcome::Crashed);
                }
                return Ok(PollOutcome::Satisfied(duration_ms(basis.elapsed())));
            }
            // read_available first (no cost, and the most direct signal); try_wait as the
            // fallback (covers the gap between the child fully exiting and its pty-side fds
            // actually tearing down, which read_available's Eof depends on).
            if matches!(status, crate::pty::ReadStatus::Eof) {
                return Ok(PollOutcome::Crashed);
            }
            if child.try_wait()?.is_some() {
                return Ok(PollOutcome::Crashed);
            }
            return Ok(PollOutcome::TimedOut);
        }
        // found in PR #44 round 16, reworked in pass 7 (P7-A/B, the reviewer's own invariant 1):
        // taken BEFORE this iteration's own read, not after. This point is reached only once the
        // deadline check just above has already passed, so `now()` was still pre-deadline at
        // that check -- but that check and THIS snapshot are two separate syscalls, and nothing
        // between them stops a descheduling gap from letting real wall-clock time pass. The
        // recheck immediately below closes that: a snapshot is only trusted if the clock STILL
        // shows pre-deadline the instant after it was taken, not merely at some earlier point
        // this same iteration.
        let pre_read_fionread = child.queued_read_bytes()?;
        if now() >= deadline {
            // the wall was crossed in the gap between the top-of-loop check and this snapshot --
            // the snapshot is not trustworthy pre-deadline evidence. Discard it (never assign it
            // anywhere) and loop back to the top: whatever was already tracked from an earlier,
            // genuinely-completed iteration survives untouched (this transition never writes
            // `last_pre_ceiling_fionread`/`consumed_since_snapshot`), so FINALIZING still spends
            // real evidence if any exists, or correctly finds `None` (no evidence at all) if this
            // is the very first thing this loop has ever attempted.
            continue;
        }
        buf.clear();
        // found in PR #44 pass 6 S1 (a user-review finding, #2): this read used to be
        // unbounded (`child.read_available(buf)`), throwing the snapshot above away one line
        // later -- if the subject wrote between the snapshot and the read, the extra
        // (potentially post-deadline) bytes got fed and evaluated below in this SAME iteration,
        // before the loop ever returned to the top to re-check the deadline. Bounding the read
        // to `pre_read_fionread` closes that: it can never return more than the snapshot
        // proved queued, so a byte that arrives after the snapshot is simply left queued,
        // picked up (or not) by a LATER iteration that re-checks the deadline first.
        //
        // found in PR #44 pass 7 (P7-A/B, the reviewer's own invariant 2): a snapshot of `0` used
        // to fall through to a real probe read (bounded or not) to disambiguate "nothing queued
        // yet" from "the child already exited" -- but ANY read here, however small, is still a
        // read whose resulting bytes were covered by a snapshot that proved nothing was queued,
        // which invariant 1 alone cannot make legitimate (there is nothing for such a read to be
        // bounded BY). Decoupled entirely: liveness is decided via `try_wait` (has the process
        // exited) and `hung_up` (has the pty slave side closed) -- two separate kernel signals,
        // checked because either can be observed first depending on timing (`try_wait`'s own doc
        // comment on this call site's sibling, and round 9's own empirical finding, both already
        // establish that this file cannot assume a fixed ordering between them) -- with NO read
        // of any kind. A `0` snapshot now admits exactly zero bytes, always.
        let status = if pre_read_fionread == 0 {
            if child.try_wait()?.is_some() || child.hung_up()? {
                if now() >= deadline {
                    // post-merge batch 2 (2026-07-22), finding #2: death observation can lag the
                    // wall -- the last clock check above predates the liveness syscalls, and the
                    // post-mortem drain must never admit bytes the deadline branch's own rules
                    // would exclude. Same discard idiom as the post-snapshot recheck above: loop
                    // back and let FINALIZING spend only tracked pre-deadline evidence.
                    continue;
                }
                // found in PR #44 pass 7 (P7-B): NOT an immediate `return Ok(Crashed)` -- the
                // original shape of this fix did exactly that, and broke `completed_sample_
                // measures_from_the_declared_basis` (a real regression, caught by running the
                // suite, not spotted in review): a paint-then-exit subject's completion poll
                // starts with the child already dead and nothing newly queued, but the screen
                // ALREADY shows the matching content from the readiness poll's own leftover
                // state (`screen` persists across separate `poll_predicate` calls within one
                // `run_sample`). Returning Crashed here would never let the predicate check
                // below see that already-true match at all -- exactly the ordering `run_sample`'s
                // own doc comment establishes elsewhere ("post-completion death is an RSS-only
                // fact, not a Crashed downgrade") and the pre-pass-7 code upheld by construction
                // (predicate checked before any status-based Crashed return, in EVERY path, not
                // just the deadline branch's own explicit comment on it). `Eof` here is the
                // decoupled-from-reading equivalent of that same signal -- falls through to the
                // identical rest-of-loop-body ordering a real `Eof` from a read always used:
                // predicate first, THEN (below) the Crashed return if it didn't already match.
                //
                // post-merge batch (2026-07-22), fix #4: the zero snapshot above and this death
                // observation are two separate syscalls, and a subject can write its final
                // frame and exit entirely inside that gap -- synthesizing Eof from the stale
                // zero would judge a stale screen and report Crashed with matching bytes still
                // queued. One fresh FIONREAD here is a THIRD legitimate snapshot class
                // (post-mortem): the subject is dead OR has no remaining slave-side reference
                // (a hung_up-only observation does not prove the subject exited), and subjects
                // do not fork (docs/perf.md, "Scope and standing assumptions"), so no writer
                // remains either way -- the value is final, and the bounded drain below can
                // never admit a byte it did not prove queued before death. `buf` is cleared
                // afterward: `drain_available` already fed everything it read into `screen`,
                // and the shared feed site below this branch must not feed the last chunk a
                // second time.
                //
                // post-merge batch 2 (2026-07-22), finding #1: re-snapshot until a zero round
                // (or Eof), capped -- the subject is dead, so the queue only ever shrinks, and
                // each round's read stays bounded by its own fresh snapshot. A 73,000-iteration
                // probe on this project's target kernel never once observed a post-poll FIONREAD
                // under-reporting recoverable bytes (0/73,000, incl. under 8-way CPU
                // contention), so the single-snapshot shape this replaces was already correct
                // here; the loop is timing-free defense-in-depth against theoretical
                // flip-buffer -> line-discipline delivery lag on other kernels, and the cap
                // keeps it provably terminating instead of trusting that theory either way.
                for _ in 0..POST_MORTEM_DRAIN_ROUNDS {
                    let post_exit_queued = child.queued_read_bytes()?;
                    if post_exit_queued == 0 {
                        break;
                    }
                    let status = drain_available(child, buf, screen, post_exit_queued)?;
                    buf.clear();
                    if matches!(status, crate::pty::ReadStatus::Eof) {
                        break;
                    }
                }
                crate::pty::ReadStatus::Eof
            } else {
                // still alive, still nothing queued: `WouldBlock`, the same outcome the old
                // zero-snapshot probe reported for this exact case, just reached without ever
                // attempting a read. Falls through to the SAME rest-of-loop-body handling every
                // other `WouldBlock` iteration uses (predicate check, the regular checkpoint,
                // THEN the waiter) -- this status must not short-circuit past the checkpoint: a
                // subject that has gone genuinely idle (the common case this branch exists for,
                // not a rare one) is exactly when `TopLineStableStartsWith`'s own settling needs
                // to keep ticking.
                crate::pty::ReadStatus::WouldBlock
            }
        } else {
            let status = child.read_available_bounded(buf, pre_read_fionread)?;
            if let crate::pty::ReadStatus::Data(n) = status {
                // found in PR #44 pass 6 S1: provable, not merely hoped-for, now that the read
                // above is bounded to `pre_read_fionread` (the only way this arm is reached with
                // `n > 0`, since the zero-snapshot case above never reaches this read at all) --
                // `n` can never exceed what the snapshot proved queued. This is what makes
                // codex-3615776144's own "short-read subcase" (a stale, over-counted
                // `consumed_since_snapshot` forcing `deadline_drain_budget`'s `saturating_sub` to
                // silently clamp a real pre-deadline completion away to a false `Hung`) provably
                // unreachable rather than merely guarded against after the fact -- see
                // `deadline_drain_budget`'s own doc comment for what that clamp is now permanent
                // defensive insurance against, not a load-bearing correction for a case that can
                // still occur.
                debug_assert!(
                    n <= pre_read_fionread,
                    "a bounded read must never consume more than its own snapshot proved \
                     queued: read {n}, snapshot {pre_read_fionread}"
                );
                last_pre_ceiling_fionread = Some(pre_read_fionread);
                consumed_since_snapshot = n;
            } else {
                // found in PR #44 pass 7 (P7-A/B): genuinely unreachable, not merely unexpected
                // -- `pre_read_fionread > 0` here (the zero case returned above), so this is a
                // bounded read requesting up to that many bytes against data the kernel just
                // confirmed, via FIONREAD, is currently queued. POSIX read(2) on a pty master
                // delivers already-buffered bytes before ever reporting EOF, even if the writer
                // has since closed, so `Eof`/`WouldBlock` cannot come back here. A hard panic,
                // not a silent fallback: silently leaving `last_pre_ceiling_fionread` as `None`
                // here would be WRONG (it would make `None` reachable from more than the one
                // degenerate path `deadline_drain_budget`'s own doc comment derives it from,
                // quietly hiding a kernel-contract violation behind a value that is supposed to
                // mean nothing more than "no evidence yet"), and inventing a new "reason unclear"
                // state just to avoid panicking would hide that same violation behind
                // plausible-looking bookkeeping instead of surfacing it. This is a dev-only
                // harness (never runs unattended, never ships) -- a loud, immediate panic here is
                // preferable to either.
                unreachable!(
                    "read_available_bounded({pre_read_fionread}) returned {status:?} despite a \
                     positive FIONREAD snapshot -- POSIX read(2) on a pty master should never \
                     report EOF/WouldBlock ahead of bytes it has already confirmed are queued"
                );
            }
            status
        };
        if !buf.is_empty() {
            screen.feed(buf);
        }
        // a non-stable predicate is checked on EVERY iteration, Data/WouldBlock/Eof alike, same
        // as before pass 7: `screen` persists across separate `poll_predicate` calls within one
        // `run_sample` (readiness and completion share it), so a predicate that is already true
        // from data fed during an EARLIER call must still be caught here even when THIS call
        // never sees a byte of its own -- gating this on `status == Data` would miss exactly that
        // case, and gating it on `status != Eof` would break it for the specific "already painted
        // the match, THEN exited" case `completed_sample_measures_from_the_declared_basis` (this
        // module's own tests) pins: the predicate must be checked BEFORE the Eof-triggered
        // Crashed return just below, on every path that can produce `Eof`, not only the ones a
        // real read used to.
        //
        // this single-read-per-iteration shape does NOT reopen the deadline branch's own
        // chunking gap for a marker hidden mid-run (audited, not assumed, per PR #44 pass-2
        // review): a remainder queued behind a short read is never lost, only delayed -- the
        // NEXT iteration's own read picks up exactly where the last one left off, and a `Data`
        // outcome loops back immediately (no wait) to take it, so a multi-chunk burst still gets
        // fully fed and re-evaluated within the same wall-clock instant a single unbounded drain
        // would have taken. The only way a remainder could be PERMANENTLY missed is the deadline
        // firing before this loop ever gets back to it -- and the deadline branch above now
        // drains fully specifically to close that exact gap.
        if !matches!(predicate, Predicate::TopLineStableStartsWith(_)) && predicate.eval(screen) {
            return Ok(PollOutcome::Satisfied(duration_ms(basis.elapsed())));
        }
        // found in PR #44 pass 7 (P7-B): `Eof` here means the zero-snapshot liveness check above
        // found the subject dead (via `try_wait`/`hung_up`, never a read) -- checked AFTER the
        // predicate, never before, so a subject that already painted the match before dying
        // still reports Satisfied, matching `run_sample`'s own "post-completion death is an
        // RSS-only fact, not a Crashed downgrade" doc comment.
        if matches!(status, crate::pty::ReadStatus::Eof) {
            return Ok(PollOutcome::Crashed);
        }
        // `TopLineStableStartsWith`'s own checkpoint -- fires on a fixed wall-clock cadence,
        // independent of `status`, checked on every iteration rather than only inside a
        // WouldBlock arm.
        //
        // found in PR #44 pass-2 review: an earlier version checkpointed only when the READ had
        // gone idle (`status == WouldBlock`), reasoning that "caught up on reading" was the
        // natural checkpoint moment. That conflated two unrelated things: whether the pty is
        // GLOBALLY idle, and whether the TOP LINE specifically has held steady. A subject
        // writing continuously to any OTHER row (a status line, a spinner, a byte-count
        // counter) kept this loop permanently in the `Data` arm, so `WouldBlock` -- and every
        // checkpoint -- never happened at all, no matter how long the top line itself had
        // actually been unchanged: a genuinely settled sample reported Hung. The top line's own
        // stability has nothing to do with whether some OTHER row is still being written, and
        // docs/perf.md's own claim ("writes to other rows do not delay the check") was true of
        // perf.sh's fixed-tick bash sampler but not of this port as shipped -- checking on a
        // timer instead of an idle signal restores it. This does NOT weaken the plan-6 honesty
        // property scroll-cadence's own OTHER regression test depends on (ress's OWN top row
        // still churning between redraws must still defeat premature settlement): that property
        // comes entirely from `predicate.eval` re-reading row 0's CURRENT content at each
        // checkpoint, never from what triggers the checkpoint to fire -- a still-churning top
        // line keeps failing `predicate.eval` regardless of cadence vs. idle gating, the same
        // as it always did.
        if matches!(predicate, Predicate::TopLineStableStartsWith(_)) && now() >= next_checkpoint {
            let matched = predicate.eval(screen);
            let current_generation = screen.top_row_generation();
            // found in PR #44 pass 5 (C2): a prior confirmation only still counts toward pairing
            // with THIS tick if row 0's own generation has not moved since -- see the deadline
            // branch's own identical check, above, for the full derivation (the same property,
            // reached from the other of this predicate's two "final look" sites).
            let prior_confirmation_still_holds =
                stable_confirmed.is_some_and(|(g, _)| g == current_generation);
            // found in PR #44 round 9: the SECOND confirming checkpoint (matched && the prior
            // one still holds) used to return Satisfied straight from the content match, with no
            // liveness check at all -- a subject that painted, was confirmed once while alive,
            // then died holds the exact same frozen, still-matching screen forever after (dead
            // pagers don't redraw), so a crash between the two checkpoints used to fold into
            // Completed instead of Crashed. Parity fix: perf.sh's own `wait_top_line_stable`
            // polled a live tmux pane on every check, so a dead pane always routed to crashed
            // there -- this restores that property for the port. Boundary derived against round
            // 4's own declined finding, so the two rules stay consistent rather than
            // contradicting each other: death AFTER a completed confirmation is an RSS-only fact
            // and `Completed` stands (the event already happened, see run_sample's own doc
            // comment for why this function never revalidates past that point); death BEFORE the
            // confirming checkpoint means the event never completed at all, so it is `Crashed`,
            // not `Completed` -- exactly what this branch is checking, since the confirmation
            // has not happened yet, only the frozen APPEARANCE of one has.
            //
            // `checkpoint_confirms_only_while_alive`'s own doc comment (this module's test
            // section) explains why the liveness check is an injectable closure here rather
            // than a direct `child.try_wait()` call inline: a real spawned-and-killed process
            // cannot reliably exercise this exact branch in a test -- this extraction keeps the
            // ORDERING itself unit-testable regardless.
            //
            // found in PR #44 pass 7 (P7-A/B): the `!matches!(status, Eof)` guard this closure
            // used to carry is gone -- `status` can no longer be `Eof` by the time this checkpoint
            // runs at all (see `screen.feed`'s own comment above), so `try_wait` alone is the
            // complete liveness answer now, not a defended-against-a-race approximation of one.
            match checkpoint_confirms_only_while_alive(
                prior_confirmation_still_holds,
                matched,
                || Ok(child.try_wait()?.is_none()),
            )? {
                Some(true) => return Ok(PollOutcome::Satisfied(duration_ms(basis.elapsed()))),
                Some(false) => return Ok(PollOutcome::Crashed),
                None => {}
            }
            // a match starts (or restarts, after a reset) a fresh potential pair anchored at
            // THIS tick's own generation; a non-match (or the pair having just been consumed,
            // above) leaves nothing to build on.
            //
            // regular checkpoints are inherently interval-spaced (`next_checkpoint = now +
            // POLL_INTERVAL` below), so only the deadline branch needs the explicit spacing
            // check.
            stable_confirmed = if matched {
                Some((current_generation, now()))
            } else {
                None
            };
            next_checkpoint = now() + POLL_INTERVAL;
        }
        // `Eof` is gone (see the zero-snapshot liveness check above) -- only `Data`/`WouldBlock`
        // reach here. `Data` falls straight through to the top of the loop with no wait, so more
        // output already buffered right behind what was just drained is picked up without a
        // poll-tick of latency (this loop body's own earlier comment on the chunking gap covers
        // why that still cannot permanently hide a marker behind a chunk boundary).
        if matches!(status, crate::pty::ReadStatus::WouldBlock) {
            // found in PR #44 round 13: clamped to whatever time genuinely remains before
            // `deadline` -- see `clamped_poll_sleep`'s own doc comment for why this must never
            // wait past the wall the deadline branch (above) is about to snapshot against.
            // found in PR #44 pass 5 (B3): routed through the injected `waiter` rather than
            // calling `crate::pty::wait_for_readable` directly -- see this function's own doc
            // comment for why (a test needs to observe what was REQUESTED, not race what
            // actually happened).
            //
            // found in PR #44 pass 7 (P7-A/B, the reviewer's own invariant 3): the waiter's own
            // `PollReadiness` used to be discarded here (`waiter(...)?;` as a bare statement) --
            // matched now instead. `TimedOut` means the ENTIRE wait, bounded by `deadline` by
            // construction, produced nothing readable: that is itself proof the budget is zero,
            // recorded by resetting `last_pre_ceiling_fionread` to `None` explicitly so FINALIZING
            // spends zero directly rather than re-deriving the same fact via a fresh FIONREAD that
            // would reopen the same non-atomicity gap invariant 1 closes for the normal path, one
            // level up (see `deadline_drain_budget`'s own doc comment).
            //
            // found in PR #44 pass 7 re-review (codex P2, against ce496ad): `Readable` used to
            // take NO action at all here, on the reasoning that "the next iteration's own
            // top-of-loop snapshot discovers whatever is now queued." That reasoning had a gap:
            // the next iteration's snapshot (line 818, below the loop's own top) is gated BEHIND
            // the top-of-loop deadline check (the very top of this loop) -- so if this thread is
            // descheduled for any nonzero time between the waiter returning `Readable` and the
            // loop getting back around, that check can fire true FIRST, routing straight to
            // FINALIZING with `last_pre_ceiling_fionread` still whatever it was BEFORE this arm
            // (`None`, on the common first-look path) -- a byte proven pre-deadline-readable by
            // the waiter itself, silently lost to FINALIZING's own zero budget. The deschedule
            // need not be long, and the wait itself never overran its own clamp -- `Readable`
            // already proves the fd became readable no later than `deadline`.
            //
            // Fixed by taking exactly one more ADMITTING-side snapshot here, recheck-guarded the
            // identical way invariant 1 already guards the normal path's own (lines 818-819): if
            // the clock has NOT crossed by the time this snapshot lands, it is recorded --
            // `Some(pre_read_fionread)`, `consumed_since_snapshot` reset to 0 (nothing has been
            // READ against it yet, unlike the Data arm's identical bookkeeping just above in this
            // same function) -- superseded harmlessly by the next iteration's own fresher
            // snapshot if the deadline does not fire immediately after, spent by FINALIZING's
            // existing, UNMODIFIED `deadline_drain_budget`/`drain_available` machinery if it
            // does. This is NOT a fresh FIONREAD in FINALIZING -- FINALIZING itself still never
            // calls `queued_read_bytes` (see `drain_available`'s own body) -- it is the same
            // recheck-guarded snapshot the per-iteration top-of-loop read already takes (line
            // 818), now also taken here, at a SECOND call site rather than a third: the Data arm
            // just above records that SAME line-818 value into `last_pre_ceiling_fionread` after
            // a successful read, it never calls `queued_read_bytes` again on its own. v3/zero and
            // invariant 4 both still hold exactly as documented.
            //
            // The residual left behind is real and irreducible, not closed: `waiter` returning
            // and this snapshot are still two separate syscalls, so a deschedule landing in that
            // couple-of-instructions gap can still lose the same byte -- the identical CLASS of
            // residual invariant 1's own recheck already carries between the top-of-loop check
            // and its own snapshot (lines 818-819), not eliminable short of an atomic
            // ppoll+FIONREAD syscall that does not exist. Accepted for the same reason
            // `DRAIN_BUDGET`'s own residual is (see its own doc comment): a measurement tool's
            // median is unaffected by a residual this narrow, and the alternative (a fresh
            // FIONREAD called from inside FINALIZING itself) would reopen the exact
            // non-atomicity gap invariant 1 was written to close.
            match waiter(
                child.raw_master_fd(),
                clamped_poll_sleep(
                    now(),
                    deadline,
                    matches!(predicate, Predicate::TopLineStableStartsWith(_))
                        .then_some(next_checkpoint),
                ),
            )? {
                crate::pty::PollReadiness::TimedOut => {
                    last_pre_ceiling_fionread = None;
                }
                crate::pty::PollReadiness::Readable => {
                    // see this match's own comment above for the full derivation.
                    let pre_read_fionread = child.queued_read_bytes()?;
                    if now() < deadline && pre_read_fionread > 0 {
                        last_pre_ceiling_fionread = Some(pre_read_fionread);
                        consumed_since_snapshot = 0;
                    }
                }
            }
        }
    }
}

// bounds `drain_available`'s own inner loop -- found necessary DURING implementation, not
// anticipated up front: an unbounded drain here is safe against a finite burst (this module's
// own `large-payload-paint-at-end` fake-pager scenario, since retired in pass 7 -- P7-D, its own
// real-pty test having gone silently vacuous post-v3/zero, see
// `deadline_drain_caps_each_read_to_the_shrinking_remainder`'s own doc comment for the full
// account -- once needed only ~3 reads to drain ~9600 bytes past the pty's own ~4096-byte
// single-read cap) but hangs indefinitely against a genuinely relentless writer
// -- caught directly, not reasoned about, while RED-verifying finding 3's own fix: reverting
// JUST the checkpoint-cadence change (leaving this drain unbounded) against
// `continuous-bottom-row-writer`'s relentless loop left `run_sample`'s own 2-second ceiling
// exceeded by over a minute before a manual kill, because the deadline branch's own "final,
// thorough drain" doesn't know the deadline it's serving has already passed -- it just keeps
// draining as long as there's more, and a subject that never lets up never gives it a WouldBlock
// to return on. 64 reads is generous against any realistic finite burst while still bounding the
// pathological case in a handful of milliseconds.
//
// found in PR #44 pass 3, RE-JUSTIFIED in pass 7 (P7-D, a p6-review2 finding): originally framed
// as a secondary backstop alongside `drain_available`'s own new `byte_budget` parameter below --
// `byte_budget` was called the PRINCIPLED bound (what makes a relentless writer's own excess
// unreadable at all) with this chunk-count bound merely "cheap insurance" alongside it. Post-pass
// -7's v3/zero correction, that framing is stale: `byte_budget` is now ALWAYS finite (0, or
// `snapshot - consumed`, never open-ended) for EVERY call this function ever receives, so it
// already makes any excess unreadable on its own, with no help needed from this cap. What
// `DRAIN_BUDGET` now bounds, and the ONLY thing it still bounds, is READ COUNT independent of
// BYTE count -- the two diverge whenever the same real, already-cited tty short-read property
// returns data in enough small pieces that draining a legitimately large (but still finite)
// `byte_budget` would otherwise cost up to `byte_budget` syscalls. Not a hypothetical: proven
// directly, deterministically, by `drain_available_stops_at_drain_budget_even_with_real_pre_
// deadline_bytes_still_owed` (this module's own tests) -- an always-1-byte scripted transport
// with a 99-byte remaining budget, where this cap binds at read 64, leaving 35 genuinely
// pre-deadline bytes undrained. That is an honest trade-off, not a free one: this cap can cost a
// real ceiling-beat to a subject whose output happens to arrive in enough tiny fragments in one
// burst, in exchange for bounding this function's own worst-case syscall count regardless of how
// fragmented a real kernel's own short reads turn out to be.
//
// Acceptable for a measurement tool specifically because of how ONE stray outcome is handled,
// not despite it -- verified against this crate's own aggregation, not assumed: `SampleOutcome::
// Hung` is a recorded RESULT, never a harness failure (see its own doc comment, above), and
// `report::summarize`'s own minority-failure branch computes the median from the OTHER, still-
// completed samples in that same run, annotating a stray hang as a visible count (e.g.
// `12(1h)`) rather than silently dropping it or letting it corrupt the reported number -- as
// long as the run's own failures stay BELOW `summarize`'s own majority threshold (`failed * 2 >
// total`, the OPPOSITE branch, which reports a bare `hung(...)`/`crashed` token with no median
// at all), a threshold one pathologically-fragmented sample among the several a normal run takes
// (`default_run_uses_six_runs_over_the_large_fixture`, `quick_defaults_to_two_runs_over_the_
// small_fixture`, this crate's own tests) stays comfortably under.
const DRAIN_BUDGET: usize = 64;

// post-merge batch 2 (2026-07-22), finding #1: how many post-mortem snapshot->drain rounds the
// death arm attempts before concluding the queue is genuinely empty. One round sufficed in
// 73,000 probed iterations on the target kernel; the extra rounds cover straggler delivery on
// kernels where tty buffer flushing is more asynchronous, without ever waiting on time.
const POST_MORTEM_DRAIN_ROUNDS: usize = 4;

// drains `child`'s pty master to `WouldBlock`, `Eof`, `DRAIN_BUDGET` reads, or `byte_budget`
// bytes (whichever comes first), feeding every chunk into `screen` as it arrives, rather than
// stopping after one `read_available` call -- found necessary in PR #44 pass-2 review: a pty
// master's `read()` can return fewer bytes than are actually queued (a real tty line-discipline
// quirk, not merely "everything currently available" -- see this module's own test). Used
// specifically by `poll_predicate`'s own deadline branch, which is about to make a FINAL,
// one-shot judgment and can afford to be thorough about it -- the main loop body deliberately
// keeps its original single-read-per-iteration shape instead (see its own comment for why that,
// combined with this drain at the deadline, already closes the same gap without changing the
// main loop's own read granularity or reopening the chunking risk mid-run). A budget-exhausted
// drain reports `WouldBlock`, not a distinct status: from this function's one caller's point of
// view, "still producing data faster than we can usefully keep draining for one final look" and
// "genuinely caught up" both mean the same thing -- alive, not EOF, nothing further worth
// waiting on right now -- so no new `ReadStatus` variant is needed to tell them apart.
//
// found in PR #44 pass 3: `byte_budget` closes a gap the chunk-count bound above never addressed
// -- every read here frees pty kernel-buffer space, which can unblock a writer stalled mid-write
// on a full buffer, letting it push MORE bytes into the very fd this function is still draining.
// Unbounded by bytes, a satisfying marker written only after that unblock would be read and fed
// to `screen` here as if it had arrived before the deadline, when it did not. The caller passes
// `child.queued_read_bytes()` (`FIONREAD`), snapshotted before this function's first read --
// bytes already queued then genuinely did arrive before the deadline. Genuinely BEFORE, not just
// before this function's OWN read, since round 13: the caller's own wake (the WouldBlock arm's
// sleep, clamped to never extend past `deadline`) can no longer land meaningfully past the wall
// either, so the snapshot itself is taken close to the true deadline instant, not up to a full
// `POLL_INTERVAL` after it -- irreducible scheduler jitter is the only residual imprecision left
// (see `clamped_poll_sleep`'s own doc comment for the derivation). Anything the drain's own
// reads unblock afterward is newer than the deadline and is deliberately left unread, still
// sitting in the kernel buffer, exactly as if this function had never run. The drain reads
// history, never collaborates with the present. Bounding is done by
// capping each individual `read_available_bounded` REQUEST to the remaining budget, not by
// counting bytes after the fact: a post-hoc count could still let a single `read()` landing
// right after an unblock return more than the budget in one shot (a `read()` returns up to the
// length it was asked for), so the request itself must never ask for more than remains.
fn drain_available(
    child: &mut dyn PtyTransport,
    buf: &mut Vec<u8>,
    screen: &mut crate::screen::Screen,
    mut byte_budget: usize,
) -> anyhow::Result<crate::pty::ReadStatus> {
    for _ in 0..DRAIN_BUDGET {
        if byte_budget == 0 {
            return Ok(crate::pty::ReadStatus::WouldBlock);
        }
        buf.clear();
        let status = child.read_available_bounded(buf, byte_budget)?;
        if !buf.is_empty() {
            byte_budget = byte_budget.saturating_sub(buf.len());
            screen.feed(buf);
        }
        if !matches!(status, crate::pty::ReadStatus::Data(_)) {
            return Ok(status);
        }
    }
    Ok(crate::pty::ReadStatus::WouldBlock)
}

// found in PR #44 round 9: probed try_wait vs read_available's own Eof, racing both against a
// real process (both a SIGKILL and, separately, a natural exit) -- Eof was observed first in
// 30/30 trials both ways, by a wide, consistent margin (tens of microseconds). This means a
// real, killed-or-exited process cannot reliably reach the regular checkpoint block (below,
// inside `poll_predicate`) WHILE its read still reads as `WouldBlock` -- the loop's own earlier,
// unrelated `if matches!(status, Eof) { return Crashed }` check already wins that race
// deterministically on this system, before this checkpoint's own liveness gate ever gets a
// chance to matter. Not a reason to skip the fix (perf.sh's own tmux-pane-liveness check never
// depended on winning this race either -- it's a genuine parity/correctness fix, not a
// race-window mitigation, and a narrower window on a differently-loaded system or kernel is not
// something to assume away), but it IS why this ordering is pulled apart into its own function
// and tested with an injected, synthetic liveness answer rather than a real spawned-and-killed
// process -- the same reasoning `teardown_registered_child` was extracted for in round 6: a
// black-box test using only the two steady states (confirmed pair -> Satisfied, or not) cannot
// tell "checks liveness first" apart from "never checks at all," since both produce the SAME
// answer whenever the child actually is alive. A test can instead inject a synthetic liveness
// answer and observe that it was actually consulted before Satisfied is returned. The real,
// end-to-end `paint_stable_then_exit_before_second_checkpoint_records_crashed_not_completed`
// test (this module's own tests) still exists and still passes post-fix -- it verifies the
// OBSERVABLE outcome (Crashed, not Completed) via whichever check actually catches it in
// practice (empirically, the pre-existing Eof check), complementing rather than replacing this
// function's own direct ordering test.
//
// `stable_confirmed`/`matched` fold together the SAME two cases the caller's own `if
// predicate.eval(screen) { if stable_confirmed { ... } ... }` shape has: `None` covers both "no
// match this checkpoint" and "matched, but this is only the FIRST confirmation" (the caller sets
// `stable_confirmed = matched` for both); `Some(alive)` is the second-confirmation moment, the
// only one that ever needs a liveness answer at all -- `child_is_alive` is therefore never even
// invoked for the other two cases, not just harmlessly ignored.
fn checkpoint_confirms_only_while_alive(
    stable_confirmed: bool,
    matched: bool,
    child_is_alive: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<Option<bool>> {
    if !matched || !stable_confirmed {
        return Ok(None);
    }
    Ok(Some(child_is_alive()?))
}

// found in PR #44 pass 5 (B2, a codex P2): whether a no-completion-phase sample should record
// Completed or Crashed, given a liveness answer at the ONE revalidation perf.sh's own
// run_sample always performed right after a successful wait. `child_is_alive` is injectable for
// the identical reason `checkpoint_confirms_only_while_alive`'s own closure just above is: a
// test can pin BOTH sides of the race this revalidation exists to referee -- dead at this exact
// check -> Crashed; alive -> Completed -- deterministically, without racing a real
// spawned-and-killed process against real revalidation timing.
//
// Exit landing before vs after this check is a legitimate race the taxonomy itself already
// treats as valid either way, not a bug to pin one side of: a subject observed alive here
// genuinely survived measurement (Completed stands, even if it exits a moment later -- the same
// "post-completion death is an RSS-only fact, not a downgrade" precedent this file states
// elsewhere for the completion-phase and stability-checkpoint cases); a subject already dead
// here means poll_predicate's own readiness success never actually observed the death (a
// Data-satisfied predicate short-circuits before any Eof/try_wait check runs), so the death
// predates the recorded readiness, not the other way around, and Crashed is equally correct.
// The pre-fix test asserted Crashed unconditionally against a real paint-then-immediately-exit
// subject -- WRONG, not merely flaky: Completed is a legitimate outcome of that identical
// interleaving too, whenever this revalidation happens to run before the exit becomes
// observable. `paint_hang_with_no_completion_phase_records_completed` (this module's own
// tests) is the real, end-to-end, but still fully deterministic complement -- `paint-hang`
// never exits on its own, so the alive side of this same branch is provable through
// `run_sample`'s real entry point too, not only injected.
fn no_completion_phase_still_alive(
    child_is_alive: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    child_is_alive()
}

fn duration_ms(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

// opportunistic: a missing or malformed reading is None, never an Err -- the subject may have
// already torn its own /proc entry down in the gap since completion succeeded (perf.sh's own
// round-9 rule: this races the subject's exit, and a lost race is not this sample's failure).
fn read_rss_kib(pid: i32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
}

// found in PR #44 pass 5 commit 3: derived from the RSS investigation's own measured data (see
// the report) -- VmRSS after jump-to-end climbs roughly 2-4x within ~50-100ms of the target line
// becoming visible (the initial paint can render from a small, already-resident slice near EOF;
// the background indexer keeps building the full line index afterward), then plateaus hard, flat
// for at least 2.9s in every trajectory recorded. `RSS_SETTLE_CADENCE` (10ms, matching
// `POLL_INTERVAL`, an already-established unit in this crate) paced sampling, requiring
// `RSS_SETTLE_CONSECUTIVE` (3) consecutive agreeing readings -- at least 20ms of genuinely
// unchanged VmRSS -- comfortably filters a single coincidental tick without meaningfully
// delaying a sample that has genuinely settled. `RSS_SETTLE_CEILING` (500ms, parameterized below
// rather than a bare constant, so a test can shrink it) is 5x the observed climb time: ample
// margin for run-to-run variance without leaving a genuinely pathological subject's RSS read
// hanging the sample indefinitely.
const RSS_SETTLE_CADENCE: std::time::Duration = std::time::Duration::from_millis(10);
const RSS_SETTLE_CONSECUTIVE: usize = 3;
const RSS_SETTLE_CEILING: std::time::Duration = std::time::Duration::from_millis(500);

// found in PR #44 round 16 (a codex P2): `run_sample`'s two `Completed` call sites used to
// invoke `settled_rss_kib` unconditionally, on every live completed sample -- but report.rs's
// `RssRow` is the jump-to-end table alone (perf.sh's own `add_rss_row` only ever reported this
// one metric; `open`/`deep-goto`/`scroll-cadence`/`mount-open` never read `rss_kib` off the
// outcome at all, see scenarios.rs's own `run_open`/`run_deep_goto`/`run_scroll_cadence`/
// `run_mount_open`). Settling is not free: up to `RSS_SETTLE_CEILING` (500ms) of extra
// `RSS_SETTLE_CADENCE`-paced reads keeping the subject alive after it would otherwise be torn
// down, which on the mount leg specifically hands a still-running background reader that much
// more mount-touching time it would not otherwise get -- biasing subsequent warm samples
// relative to the immediate kill every non-reporting scenario used to get. This gate makes the
// READER itself conditional (`settle` is never even invoked, not merely its answer discarded)
// on `Scenario::reports_rss`, which only `jump_end_scenario` sets `true`. Extracted as its own
// pure function, `settle` injectable, for the same "prove the ordering, not just the steady
// state" reason every other liveness-shaped seam in this crate is: a test can inject a
// panicking closure to prove the reader is never invoked at all for a non-reporting scenario,
// not merely that its answer goes unused.
fn rss_kib_if_reported(reports_rss: bool, settle: impl FnOnce() -> Option<u64>) -> Option<u64> {
    if reports_rss { settle() } else { None }
}

// found in PR #44 pass 8 (U-rss, finding 3, GOVERNED -- see docs/perf.md's own RSS band note for
// the full account and the sign-off this supersedes): samples VmRSS across the WHOLE ceiling (or
// until the subject exits), rather than stopping the instant it first sees
// `RSS_SETTLE_CONSECUTIVE` agreeing readings in a row. The early-return version understated a
// subject that plateaus briefly -- e.g. blocked on a slow mount read -- then climbs further: it
// would report the FIRST plateau, not the true, final settled value, silently treating a
// mid-ramp pause as "done." Now tracks the MAXIMUM value ever confirmed stable for
// `RSS_SETTLE_CONSECUTIVE`-in-a-row at any point during the whole window, so a later, higher
// plateau always wins over an earlier, lower one -- robust to however many distinct plateaus a
// subject passes through on its way to its true resting value. Slower than the old version for a
// subject that settles quickly (this now always samples out to the ceiling, or until exit,
// rather than returning the moment three readings agree) -- the accepted cost of not mistaking a
// pause for done, still bounded by the same ceiling either way.
//
// `read_one` is injectable for the same "prove the ordering, not just the steady state" reason
// every other liveness-shaped seam in this crate is (see `checkpoint_confirms_only_while_alive`'s
// own doc comment) -- a scripted sequence of readings pins the plateau-tracking, the give-up-
// after-the-ceiling fallback, and the mid-ramp-pause case all deterministically. `ceiling` is a
// parameter, not the bare constant above, purely so a test can shrink the worst-case run time
// without needing a second, separate injected clock seam -- `RSS_SETTLE_CADENCE`/
// `RSS_SETTLE_CONSECUTIVE` stay fixed constants since neither affects a test's own worst-case
// wall-clock cost the way the ceiling does.
//
// On reaching the ceiling (or the subject exiting) without ever confirming a stable plateau:
// reports the LAST reading taken, silently, not a distinct marker -- unchanged from before, a
// deliberate choice, not an oversight. `RssRow` (report.rs) has no per-sample metadata channel
// today, only a bag of `u64` values folded into a median; threading a new "unsettled" flag all
// the way from here to that report would be considerably more machinery than this edge case's
// own likelihood justifies. The identical reasoning covers a subject that exits mid-settle
// (`read_one` starts returning `None` after already producing at least one real reading): the
// best plateau found so far (or, absent one, the last good reading) is returned rather than
// discarded, since there is nothing further to settle TOWARD once the process itself is gone. A
// read that races the subject's exit before EVEN ONE reading ever lands still reports `None`,
// exactly as `read_rss_kib` already did alone -- this function only changes what happens once at
// least one reading has been taken, never widens the pre-existing opportunistic-miss case.
fn settled_rss_kib(
    ceiling: std::time::Duration,
    mut read_one: impl FnMut() -> Option<u64>,
) -> Option<u64> {
    let deadline = std::time::Instant::now() + ceiling;
    let mut last: Option<u64> = None;
    let mut run_length = 0usize;
    // the highest value ever confirmed stable for RSS_SETTLE_CONSECUTIVE-in-a-row, across the
    // whole sampling window -- a later, higher plateau always wins over an earlier, lower one.
    let mut plateau: Option<u64> = None;
    loop {
        match read_one() {
            None => return plateau.or(last),
            Some(v) => {
                if last == Some(v) {
                    run_length += 1;
                    if run_length >= RSS_SETTLE_CONSECUTIVE {
                        plateau = Some(plateau.map_or(v, |p| p.max(v)));
                    }
                } else {
                    last = Some(v);
                    run_length = 1;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return plateau.or(last);
        }
        std::thread::sleep(RSS_SETTLE_CADENCE);
    }
}

// mirrors perf.sh's _tracing_precise_ms without the wrapper/stamp-file indirection: exec_stamp
// (captured by pty.rs's spawn_capturing_exec_stamp, from INSIDE the child, immediately before
// its own exec) stands in for the wrapper's $EPOCHREALTIME stamp, which the deleted script's
// own wrapper captured at exactly that same point.
//
// NOT uniformly opportunistic -- found necessary in PR #44 pass-2 review, correcting round 5's
// own fix; extended in pass 3 from a two-way split (by TREATMENT: opportunistic vs. hard-fail)
// to a three-way one (by CASE: each of the three things that can actually go wrong gets its own
// entry below, even though two of them end up sharing the same treatment). Case split derived
// against the retired script's ACTUAL behavior (`git show
// 66820c6:scripts/perf.sh`'s `wait_for_log_line`/`_tracing_precise_ms`/`run_sample`), not
// invented fresh:
//
// - Missing STAMP while tracing WAS requested (`exec_stamp` is `None` and `tracing_requested` is
//   true -- the pipe capture in pty.rs genuinely failed): CORRECTED in PR #44 pass 3 -- no longer
//   opportunistic, now a hard `Err` liveness-gated exactly like the missing-log-line case below.
//   Pass-2's own grouping of this together with clock-skew (both simply "`Ok(None)`") missed a
//   real distinction between them: the stamp pipe is harness-owned plumbing, written
//   unconditionally by pty.rs's `spawn_capturing_exec_stamp` pre-exec whenever tracing was
//   requested at all (see its own doc comment) -- there is no external clock, no NTP, nothing
//   outside this harness's own control involved in writing it, so its absence in front of a
//   CONFIRMED screen-poll paint (this function's own precondition, same as the log-line case)
//   is not the subject racing anything -- it is broken instrumentation, the identical framing
//   the missing-log-line hard-fail below already uses. `tracing_requested` (threaded in as its
//   own parameter, set from `subject.tracing` at the one production call site) is what makes
//   this case distinguishable from the untraced one at all -- `exec_stamp` alone cannot tell
//   them apart, both present as a bare `None`. (Untraced subjects -- `tracing_requested` false,
//   so `exec_stamp` is unconditionally `None` by construction, `run_sample` never even calling
//   `spawn_capturing_exec_stamp` for one -- are NOT this case: `Ok(None)` unconditionally, same
//   as always, since there was never an instrument here to fail in the first place. No
//   equivalent to either half exists in the retired script AT ALL -- perf.sh's own wrapper wrote
//   its stamp file unconditionally as part of the SAME shell invocation that then `exec`'d the
//   pager, so "the stamp mechanism itself failed" was not a distinct failure mode the script
//   ever had to handle; it is new surface this port's own pipe-based mechanism introduced.)
// - Clock-skew (`checked_sub` underflow -- the log line's own timestamp precedes the stamp):
//   stays opportunistic, `Ok(None)`. Round 4's own addition, with no script equivalent either --
//   perf.sh's bash arithmetic (`tracing_us - start_us`) had no checked-subtraction concept at
//   all, and would have silently reported a nonsensical NEGATIVE number rather than catching
//   this case in any way. There is no "hard die" behavior in the original to preserve parity
//   with here, and reporting `no-data` is strictly more honest than the script's own silent
//   garbage -- an improvement round 4 was right to keep as non-fatal, not a gap to close. Unlike
//   the missing-stamp case above, both readings are genuinely PRESENT here -- the harness's own
//   instrument worked, on both ends -- so there is no broken capture to blame; the disagreement
//   is a real property of the two underlying clock reads (most plausibly a small NTP slew
//   landing between them, even on one machine), an environmental hazard outside the harness's
//   own control, not evidence of a misconfigured one.
// - Missing OR unparseable LOG LINE (the flush-timeout wait ran out with no matching line, or a
//   line was found but its timestamp does not parse): a hard `Err`, matching
//   `wait_for_log_line "$logf" "perf: first paint" "$LOG_TIMEOUT_S" || die "ress first-paint log
//   line missing"` in the script exactly. This function is only ever called AFTER readiness
//   (the screen-poll paint predicate) has already succeeded -- perf.sh's own equivalent ordering
//   (`wait_for_log_line` runs only after `wait1` succeeded and `assert_pane_is` confirmed the
//   pane is still alive and genuinely the pager). Given a real paint was independently observed
//   on the SCREEN, a missing or garbled corresponding TRACING line is not this one sample racing
//   anything -- it is evidence the harness's own instrumentation is broken (a renamed event, a
//   misconfigured `--log-file`/`RESS_LOG`), the retired script's own framing for treating this
//   as a die()-worthy harness fault rather than a per-sample outcome to shrug off and paper over
//   with `no-data` (round 5's own mistake: making EVERY failure mode here opportunistic treated
//   this cross-check between two independent evidence sources as if it were a primary,
//   racy-by-nature signal like RSS or the stamp itself, when perf.sh never did).
//
// `log_flush_timeout` threaded in (rather than reading `LOG_FLUSH_TIMEOUT` directly) so a test
// can drive the missing-line path in milliseconds instead of the real 5-second wait -- `run_sample`
// is the one production call site, and passes the real constant.
//
// found in PR #44 round 11, extended in pass 3 to the missing-stamp case above: the
// missing-log-line hard-fail (and, since pass 3, the missing-stamp hard-fail before it) is
// reached, and this function returns `Err` via `?`, BEFORE `run_sample`'s own no-completion
// branch ever gets a chance to run its own liveness revalidation (round 1's own fix, `run_sample`
// ~324) -- so a tracing subject that paints (readiness succeeds) and then crashes/exits before it
// ever gets to hand either instrument something to report burned the full timeout/window and
// ABORTED THE WHOLE RUN as broken instrumentation, when the truth is far more mundane: the
// subject died, and its death is exactly what explains the silence, no mystery instrument
// required. Boundary, derived directly from each hard-fail's own purpose (the case-split doc
// comment above): both hard-fails exist to catch a MISCONFIGURED INSTRUMENT on a subject that is
// demonstrably fine -- a healthy process that simply never got asked to write the line (or,
// since pass 3, whose stamp pipe write never landed), or wrote/sent it somewhere this harness
// isn't looking. A subject that is no longer alive to write or send anything is not that case at
// all; a dead subject's silence is the crash taxonomy's jurisdiction, the same one round 1's
// revalidation already owns for the "painted then died before the completion phase ever got a
// look" shape -- both hard-fails are that identical shape, just reached through two different
// deadlines. So liveness is consulted HERE, at the exact moment each deadline expires and BEFORE
// the fatal verdict, not after: `try_wait()` (a direct kernel query, unaffected by the pty
// read-side's own Eof-detection lag round 9 found and documented) is the same authoritative
// signal `run_sample`'s own no-completion branch already uses for the identical purpose. A dead
// child at either point returns `TracingOutcome::SubjectDied`, which `run_sample` maps straight
// to `Crashed` -- the round-1 path, reached from two different entry points now instead of one.
// An alive child with a genuinely absent stamp or line is unchanged (for the line, exactly as
// pass 2 ruled; for the stamp, exactly as pass 3 now rules identically): the hard `Err` stands,
// since a live, healthy subject not producing either really is the instrument's own fault.
// Checked for composition, not assumed: neither of the two tracing-enabled call sites (`open`,
// `open` (mount) -- `scenarios.rs`'s own `open_scenario`) ever uses `TopLineStableStartsWith` for
// readiness (both use `Predicate::Painted`), so round 9's own second-checkpoint liveness gate is
// never even reachable for a tracing sample's readiness poll -- the fixes have disjoint
// jurisdictions by construction, not merely by luck; no double-handling is possible, let alone
// occurring.
#[derive(Debug)]
enum TracingOutcome {
    /// The existing opportunistic reading -- a real millisecond delta, or no-data (an untraced
    /// subject, or a genuine clock-skew underflow; see this function's own case-split doc
    /// comment above for exactly which failure modes land here).
    Reading(Option<u64>),
    /// The child was already dead at the exact moment the missing-stamp or missing-log-line
    /// deadline expired -- see this function's own doc comment for the full derivation.
    /// `run_sample` maps this straight to `SampleOutcome::Crashed`.
    SubjectDied,
}
fn tracing_precise_ms(
    tracing_requested: bool,
    exec_stamp: Option<std::time::SystemTime>,
    log_path: &std::path::Path,
    log_flush_timeout: std::time::Duration,
    child: &mut crate::pty::PtyChild,
) -> anyhow::Result<TracingOutcome> {
    let Some(exec_stamp) = exec_stamp else {
        // found in PR #44 pass 3: see this function's own case-split doc comment above for the
        // full derivation -- an untraced subject (never attempted an instrument) stays
        // opportunistic; a traced subject whose stamp genuinely never arrived gets the same
        // liveness gate the missing-log-line deadline below already uses.
        if !tracing_requested {
            return Ok(TracingOutcome::Reading(None));
        }
        if child.try_wait()?.is_some() {
            return Ok(TracingOutcome::SubjectDied);
        }
        anyhow::bail!(
            "ress exec stamp missing for a tracing subject with a confirmed screen-poll paint \
             and a live child -- broken instrumentation (pty.rs's own stamp-pipe capture \
             failed), not a per-sample race, the same standard applied to the first-paint \
             tracing log line just below"
        );
    };
    let Ok(duration_since_epoch) = exec_stamp.duration_since(std::time::UNIX_EPOCH) else {
        return Ok(TracingOutcome::Reading(None));
    };
    let exec_stamp_micros = duration_since_epoch.as_micros();
    let deadline = std::time::Instant::now() + log_flush_timeout;
    let line = loop {
        if let Ok(contents) = std::fs::read_to_string(log_path)
            && let Some(line) = contents
                .lines()
                .rev()
                .find(|line| line.contains("perf") && line.contains("first paint"))
        {
            break line.to_string();
        }
        // found in PR #44 pass 3: liveness used to be checked only once, at the deadline itself
        // (the `if std::time::Instant::now() >= deadline` branch below) -- a subject that died
        // right after painting (readiness succeeded, so SOMETHING painted) but never got to
        // flush its own log line stalled every affected sample for the FULL `log_flush_timeout`
        // (5s in production) before that one, final liveness check ever ran, even though the
        // death was detectable within a single `POLL_INTERVAL` of it happening. Polled here
        // instead, after every unsuccessful read: a dead child is caught almost immediately
        // (`SubjectDied`, the same round-11 mapping to `Crashed`), the identical standard the
        // deadline branch already applied, just checked continuously rather than only once at
        // the very end. By the time the deadline check below can fire, this iteration's own
        // `try_wait` has already run and returned `None` (alive) -- the deadline branch no
        // longer needs (or performs) a liveness check of its own.
        if child.try_wait()?.is_some() {
            return Ok(TracingOutcome::SubjectDied);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "ress first-paint tracing log line missing from {log_path:?} within {log_flush_timeout:?} \
                 of a confirmed screen-poll paint -- broken instrumentation (a renamed tracing \
                 event, a misconfigured --log-file/RESS_LOG), not a per-sample race, matching the \
                 retired script's own wait_for_log_line || die(...)"
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    let Ok(line_micros) = parse_rfc3339_line_timestamp(&line) else {
        anyhow::bail!(
            "ress first-paint tracing log line found but unparseable in {log_path:?}: {line:?}"
        );
    };
    // clock-skew: stays opportunistic -- see this function's own doc comment for why.
    let Some(delta_micros) = line_micros.checked_sub(exec_stamp_micros) else {
        return Ok(TracingOutcome::Reading(None));
    };
    Ok(TracingOutcome::Reading(
        u64::try_from(delta_micros / 1000).ok(),
    ))
}

// parses the first whitespace-delimited field of a tracing_subscriber::fmt() log line, e.g.
// "2026-07-14T07:14:04.905722Z  INFO perf: first paint", into microseconds since the unix
// epoch (UTC). uses timegm, not mktime: the timestamp is already UTC ("Z"), so this needs no
// timezone database and cannot be perturbed by the host's local timezone or DST rules -- see
// ress/src/cli.rs's init_logging (tracing_subscriber::fmt().with_ansi(false), the default
// RFC3339 writer) for where the format comes from.
fn parse_rfc3339_line_timestamp(line: &str) -> anyhow::Result<u128> {
    let field = line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty tracing log line"))?;
    let stamp = field
        .strip_suffix('Z')
        .ok_or_else(|| anyhow::anyhow!("tracing timestamp {field:?} missing the Z suffix"))?;
    let (date, time) = stamp
        .split_once('T')
        .ok_or_else(|| anyhow::anyhow!("tracing timestamp {field:?} missing the T separator"))?;
    let mut date_fields = date.splitn(3, '-');
    let year: i32 = date_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad year in tracing timestamp {field:?}"))?;
    let month: i32 = date_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad month in tracing timestamp {field:?}"))?;
    let day: i32 = date_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad day in tracing timestamp {field:?}"))?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut hms_fields = hms.splitn(3, ':');
    let hour: i32 = hms_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad hour in tracing timestamp {field:?}"))?;
    let minute: i32 = hms_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad minute in tracing timestamp {field:?}"))?;
    let second: i32 = hms_fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad second in tracing timestamp {field:?}"))?;
    let mut frac_digits = frac.to_string();
    frac_digits.truncate(6);
    while frac_digits.len() < 6 {
        frac_digits.push('0');
    }
    let micros: u32 = frac_digits.parse().map_err(|err| {
        anyhow::Error::from(err).context(format!("bad fractional seconds in {field:?}"))
    })?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = second;
    let epoch_s = unsafe { libc::timegm(&mut tm) };
    anyhow::ensure!(epoch_s >= 0, "timegm rejected tracing timestamp {field:?}");
    Ok(u128::from(epoch_s as u64) * 1_000_000 + u128::from(micros))
}

// a fresh, empty, per-sample directory pointed at by HOME/XDG_*_HOME (see Subject::env_for) --
// perf.sh's FAKE_HOME exists once per whole run; this narrows that to once per sample, a
// stronger isolation guarantee that costs nothing to provide. removed on drop, covering every
// exit path out of run_sample, including an early `?` propagation -- `run_sample` never has more
// than one `FakeHome` alive at a time (it is a plain local variable, exactly the same "owns
// its own resource, dropped before a new one is ever created" reasoning `pty::ACTIVE_CHILD_PGID`'s
// own doc comment already establishes for `PtyChild`).
//
// found in PR #44 pass-2 review: the path used to be DERIVED (`ress-perf-home-<pid>-<counter>`),
// not kernel-GUARANTEED fresh -- two real gaps, not hypothetical: (1) an interrupted run skips
// this struct's own `Drop` entirely (SIGINT/SIGTERM/SIGHUP bypass it, the same class this whole
// file's signal-safe cleanup exists for elsewhere), leaving a directory behind at a name a LATER
// invocation could derive again; (2) `NEXT_FAKE_HOME_ID` always restarts at 0 every process, so
// the very FIRST `FakeHome` of any invocation that happens to reuse an earlier, killed
// invocation's own pid would derive the IDENTICAL path a leftover directory from that earlier
// run might still occupy -- a subject reading STALE content from a REUSED HOME (a config file
// this harness's own whitelist never intended to survive between samples, or -- more subtly --
// `tracing_precise_ms` finding a stale `subject.log` left over from a DIFFERENT sample's own
// tracing run) rather than the honestly-empty directory this whole mechanism exists to
// guarantee. `libc::mkdtemp` closes both: the kernel picks the actual suffix (atomically,
// retrying past any collision internally), so the resulting path is fresh by construction, not
// by this process's own bookkeeping -- no new dependency, `mkdtemp` is already `libc`'s.
//
// the SAME class of gap round 6 (the Drop-side ordering) and this same round's own spawn-side
// finding (pty.rs's `spawn_window_blocked`) already closed for `PtyChild` applies here too, just
// for a directory instead of a subject process: `mkdtemp` succeeding creates a REAL,
// externally-visible resource, and a signal landing between that and this resource being
// registered for signal-safe cleanup would orphan it exactly like an unregistered subject.
// `create` therefore reuses `pty::spawn_window_blocked` directly (not a second, parallel
// mechanism) to close the identical window here.
struct FakeHome(std::path::PathBuf);

impl FakeHome {
    fn create() -> anyhow::Result<FakeHome> {
        let signals = crate::pty::spawn_window_signal_set();
        let path = crate::pty::spawn_window_blocked(
            &signals,
            || -> anyhow::Result<std::path::PathBuf> {
                let path = mkdtemp_under_temp_dir("ress-perf-home-XXXXXX")?;
                let log_path = path.join("subject.log");
                crate::register_fake_home(&path, &log_path)?;
                Ok(path)
            },
        )?;
        Ok(FakeHome(path))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        // clear the signal-safe registration LAST, after the actual removal -- the same
        // round-6 ordering lesson applied here: a signal landing WHILE `remove_dir_all` is
        // still in flight must still see this directory registered, so its own (redundant,
        // idempotent -- ENOENT/ENOTEMPTY are both silently discarded either side of this race,
        // see `handle_termination_signal`'s own doc comment) unlink+rmdir attempt has something
        // to work with rather than silently no-op'ing against an already-cleared slot.
        std::fs::remove_dir_all(&self.0).ok();
        crate::unregister_fake_home();
    }
}

// `libc::mkdtemp`: `template` must end in EXACTLY six literal `X` characters, which the kernel
// overwrites in place with a suffix that makes the resulting path unique -- atomically (no
// separate exists-check-then-create race for a caller to get wrong) and by construction (the
// kernel's own choice, not this process's derived-from-pid-and-a-counter guess). `dir` is joined
// under `std::env::temp_dir()` by the one caller here; kept as a plain `&str` template SUFFIX
// parameter rather than a full path so a future second caller (there is currently only one)
// cannot accidentally omit the six `X`s mkdtemp's own contract requires.
fn mkdtemp_under_temp_dir(template_name: &str) -> anyhow::Result<std::path::PathBuf> {
    let template_path = std::env::temp_dir().join(template_name);
    let mut template_bytes: Vec<u8> =
        std::os::unix::ffi::OsStrExt::as_bytes(template_path.as_os_str()).to_vec();
    template_bytes.push(0);
    let result_ptr = unsafe { libc::mkdtemp(template_bytes.as_mut_ptr() as *mut libc::c_char) };
    if result_ptr.is_null() {
        return Err(anyhow::Error::new(std::io::Error::last_os_error())
            .context(format!("mkdtemp failed for template {template_path:?}")));
    }
    // mkdtemp overwrote the XXXXXX suffix IN PLACE and returned a pointer into the SAME buffer
    // -- find the actual (shorter-or-equal, since the suffix is a fixed-width replacement)
    // length up to the NUL terminator it left behind, rather than assuming the original
    // template's own length.
    let len = template_bytes
        .iter()
        .position(|&b| b == 0)
        .expect("mkdtemp always NUL-terminates its result in the same buffer");
    let os_str =
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&template_bytes[..len]);
    Ok(std::path::PathBuf::from(os_str))
}

#[cfg(test)]
mod tests {
    fn write_fixture() -> (crate::test_support::TempFile, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("ress-perf-runner-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello\n").unwrap();
        (crate::test_support::TempFile(path.clone()), path)
    }

    // found in PR #44 pass 7 (P7-A/B, the reviewer's own scripted PTY transport): a fully
    // scripted `super::PtyTransport` -- no real process, no real pty, anywhere. Lets an
    // admission-POLICY test drive `poll_predicate_with_waiter`/`drain_available` against exactly
    // the sequence of read-surface answers its own claim needs, deterministically -- real-pty
    // coverage of the TRANSPORT PRIMITIVES themselves (a real read returns what was written,
    // FIONREAD counts what's queued, `hung_up` observes a real POLLHUP) stays in `pty.rs`'s own
    // tests instead.
    //
    // `queued` is consumed in order by `queued_read_bytes`, the last entry repeating once
    // exhausted (most tests want a steady answer after their own scripted transient, not a panic
    // on one extra call the loop's own control flow happens to make) -- and deliberately
    // INDEPENDENT of `readable`'s own actual remaining length, not derived from it: a test can
    // script an under-reporting snapshot (fewer bytes claimed than are genuinely available to
    // read) and prove a bound is still honored against it, the identical property `normal_path_
    // read_never_consumes_past_an_injected_pre_read_snapshot` (below) used to prove against a
    // real, over-writing child. `exit` is the same shape for `try_wait`. `read_calls`/`queued_
    // calls` exist so a test can assert a call NEVER happened, not merely that its answer didn't
    // matter -- the same "prove the decision, not just the outcome" standard this crate already
    // holds every other injected seam to.
    //
    // `read_caps`, same consumed-in-order shape, ADDITIONALLY bounds each `read_available_
    // bounded` call beyond `max_bytes`/`readable.len()` -- empty means uncapped. Exists to
    // simulate a genuine short read (a real pty master's own `read()` CAN return fewer bytes than
    // are truly queued, a real tty line-discipline quirk `drain_available`'s own doc comment
    // already cites), the only way ADMITTING's own snapshot can leave a NONZERO remainder for
    // FINALIZING to legitimately drain -- see `deadline_recovery_drains_a_real_short_reads_own_
    // remainder` (below).
    //
    // `requested_sizes`, found in PR #44 pass 7 (P7-D, a p6-review2 precision finding): records
    // the RAW `max_bytes` argument every `read_available_bounded` call receives, BEFORE
    // `read_caps` applies -- i.e., exactly what the CALLER (ADMITTING's own single bounded read,
    // or one of FINALIZING's `drain_available` internal reads) computed and asked for, not what
    // the script chose to hand back. A test can assert on this sequence directly (e.g. `[8, 5,
    // 3]`) to pin each call's OWN requested size individually -- which of them was ADMITTING's
    // (always the raw snapshot, unconditionally) and which were `drain_available`'s own
    // internal, monotonically-shrinking requests -- rather than inferring the boundary only from
    // the aggregate outcome or a bare call count.
    #[derive(Default)]
    struct ScriptedTransport {
        queued: Vec<usize>,
        queued_cursor: std::cell::Cell<usize>,
        queued_calls: std::cell::Cell<u32>,
        readable: std::collections::VecDeque<u8>,
        read_caps: Vec<usize>,
        read_cap_cursor: usize,
        read_calls: u32,
        requested_sizes: Vec<usize>,
        exit: Vec<Option<std::process::ExitStatus>>,
        exit_cursor: usize,
        hung_up: bool,
    }

    impl super::PtyTransport for ScriptedTransport {
        fn queued_read_bytes(&self) -> anyhow::Result<usize> {
            self.queued_calls.set(self.queued_calls.get() + 1);
            if self.queued.is_empty() {
                return Ok(0);
            }
            let i = self.queued_cursor.get();
            let value = self.queued[i.min(self.queued.len() - 1)];
            self.queued_cursor.set(i + 1);
            Ok(value)
        }
        fn read_available_bounded(
            &mut self,
            buf: &mut Vec<u8>,
            max_bytes: usize,
        ) -> anyhow::Result<crate::pty::ReadStatus> {
            self.read_calls += 1;
            self.requested_sizes.push(max_bytes);
            let cap = if self.read_caps.is_empty() {
                usize::MAX
            } else {
                let i = self.read_cap_cursor.min(self.read_caps.len() - 1);
                self.read_cap_cursor += 1;
                self.read_caps[i]
            };
            let max_bytes = max_bytes.min(cap);
            if max_bytes == 0 || self.readable.is_empty() {
                return Ok(crate::pty::ReadStatus::WouldBlock);
            }
            let n = max_bytes.min(self.readable.len());
            for _ in 0..n {
                buf.push(self.readable.pop_front().unwrap());
            }
            Ok(crate::pty::ReadStatus::Data(n))
        }
        fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
            if self.exit.is_empty() {
                return Ok(None);
            }
            let i = self.exit_cursor.min(self.exit.len() - 1);
            self.exit_cursor += 1;
            Ok(self.exit[i])
        }
        fn hung_up(&self) -> anyhow::Result<bool> {
            Ok(self.hung_up)
        }
        fn raw_master_fd(&self) -> libc::c_int {
            // never dereferenced -- `ScriptedTransport` tests also script `waiter`, which never
            // touches the real fd it's handed, only logs/inspects it or returns a scripted
            // `PollReadiness` directly. A recognizable sentinel, not 0/-1 (which could plausibly
            // collide with a real fd number and mask a bug), if a future caller ever DID
            // dereference it.
            -12345
        }
    }

    // `std::process::ExitStatus` has no public constructor -- the only way to obtain one is a
    // real process actually exiting. This is that, kept to the absolute minimum a scripted-death
    // test needs: `true` exits immediately, no pty, no `fake-pager`, no paint to wait for --
    // microseconds, not the real spawn + first-paint drain + hundreds of milliseconds of settling
    // the tests that use this used to need for the SAME purpose.
    fn exited_status() -> std::process::ExitStatus {
        std::process::Command::new(crate::test_support::on_path("true"))
            .status()
            .unwrap()
    }

    // pins `checkpoint_confirms_only_while_alive`'s own contract directly: the liveness closure
    // is consulted ONLY on the second-confirmation moment (matched && already stable_confirmed),
    // never for a plain non-match or a first-time match -- both of THOSE cases `panic!` inside
    // the closure if it's ever called, proving it genuinely isn't, not merely that its answer
    // happened not to matter.
    #[test]
    fn checkpoint_liveness_closure_is_never_consulted_on_a_plain_non_match() {
        let verdict = super::checkpoint_confirms_only_while_alive(true, false, || {
            panic!("a non-match must never consult liveness")
        })
        .unwrap();
        assert_eq!(verdict, None);
    }

    #[test]
    fn checkpoint_liveness_closure_is_never_consulted_on_the_first_confirmation() {
        let verdict = super::checkpoint_confirms_only_while_alive(false, true, || {
            panic!("a first-time match must never consult liveness -- nothing to confirm yet")
        })
        .unwrap();
        assert_eq!(verdict, None);
    }

    #[test]
    fn checkpoint_second_confirmation_reports_alive_when_the_child_still_is() {
        let verdict = super::checkpoint_confirms_only_while_alive(true, true, || Ok(true)).unwrap();
        assert_eq!(
            verdict,
            Some(true),
            "a genuinely live second confirmation must report alive, not fall open"
        );
    }

    // the exact PR #44 round 9 finding: a second confirmation against a frozen, dead-child
    // screen must report NOT alive (the caller then returns Crashed), not silently agree with
    // whatever the content match already said.
    #[test]
    fn checkpoint_second_confirmation_reports_not_alive_when_the_child_has_died() {
        let verdict =
            super::checkpoint_confirms_only_while_alive(true, true, || Ok(false)).unwrap();
        assert_eq!(
            verdict,
            Some(false),
            "a dead child at the second confirmation must not be trusted just because the \
             screen still matches"
        );
    }

    // found in PR #44 round 13: pins `clamped_poll_sleep`'s own contract directly, against
    // synthetic `Instant`s derived from one shared `now` -- no real sleeping, no racing.
    #[test]
    fn clamped_poll_sleep_caps_at_poll_interval_when_plenty_of_time_remains() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);
        assert_eq!(
            super::clamped_poll_sleep(now, deadline, None),
            super::POLL_INTERVAL
        );
    }

    #[test]
    fn clamped_poll_sleep_shrinks_to_whatever_time_genuinely_remains() {
        let now = std::time::Instant::now();
        // 3ms remaining -- comfortably less than POLL_INTERVAL (10ms), so the clamp must bind.
        let deadline = now + std::time::Duration::from_millis(3);
        assert_eq!(
            super::clamped_poll_sleep(now, deadline, None),
            std::time::Duration::from_millis(3)
        );
    }

    // the exact round 13 boundary: a deadline that has ALREADY passed by the time this is
    // called (the loop's own deadline check at the top of `poll_predicate` races this in
    // production, but here it's simply constructed already-past) must never sleep at all --
    // a negative "time remaining" saturates to zero, not wrapping or panicking.
    #[test]
    fn clamped_poll_sleep_is_zero_once_the_deadline_has_already_passed() {
        let now = std::time::Instant::now();
        let deadline = now - std::time::Duration::from_millis(5);
        assert_eq!(
            super::clamped_poll_sleep(now, deadline, None),
            std::time::Duration::ZERO
        );
    }

    // found in PR #44 pass 6 S1 (#3): the OTHER half of the reported ~18ms cadence -- a
    // `next_checkpoint` due sooner than both `deadline` and `POLL_INTERVAL` must be the binding
    // clamp, not silently ignored in favor of a full fresh `POLL_INTERVAL` wait.
    #[test]
    fn clamped_poll_sleep_binds_to_next_checkpoint_when_it_is_the_tightest_bound() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);
        // 1ms remaining until the next checkpoint -- comfortably less than both `deadline` and
        // `POLL_INTERVAL` (10ms), so the checkpoint clamp must bind.
        let next_checkpoint = now + std::time::Duration::from_millis(1);
        assert_eq!(
            super::clamped_poll_sleep(now, deadline, Some(next_checkpoint)),
            std::time::Duration::from_millis(1)
        );
    }

    // the sibling half: when `next_checkpoint` is farther away than `POLL_INTERVAL` (or than
    // `deadline`), it must not WIDEN the wait past what the existing two-way clamp already
    // allows -- `Some` still only NARROWS, never loosens.
    #[test]
    fn clamped_poll_sleep_next_checkpoint_never_widens_past_the_existing_clamp() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);
        let next_checkpoint = now + std::time::Duration::from_secs(5);
        assert_eq!(
            super::clamped_poll_sleep(now, deadline, Some(next_checkpoint)),
            super::POLL_INTERVAL
        );
    }

    // found in PR #44 round 16: the exact bug -- a tracked, provably-pre-ceiling snapshot must
    // be preferred over anything else.
    #[test]
    fn deadline_drain_budget_prefers_the_tracked_snapshot_over_zero() {
        let budget = super::deadline_drain_budget(Some(100), 30);
        assert_eq!(
            budget, 70,
            "100 provably queued as of the last snapshot, minus 30 already drained by an \
             ordinary read since"
        );
    }

    // the saturating half: an ordinary read can legitimately consume MORE than the last tracked
    // snapshot recorded (it swept up both the provable remainder and some genuinely
    // post-ceiling bytes in the same call -- see this function's own doc comment) -- the budget
    // must floor at zero, not wrap around a `usize` subtraction underflow.
    #[test]
    fn deadline_drain_budget_saturates_at_zero_rather_than_going_negative() {
        let budget = super::deadline_drain_budget(Some(50), 80);
        assert_eq!(budget, 0);
    }

    // found in PR #44 pass 7 (P7-A/B, the reviewer's own invariants 3 and 4): `None` covers BOTH
    // "no Admitting iteration has ever completed a passed-recheck Data arm" (see
    // `deadline_drain_budget`'s own doc comment for the full derivation, corrected in pass 7's
    // delta review to drop an over-narrow "only the first iteration" claim this comment used to
    // make here too) and "a waiter timed out on a wait bounded by the deadline" (which by that
    // wait's own contract proves nothing became readable) -- the SAME value, deliberately, since
    // both have equally no pre-deadline evidence to spend. Budget is zero either way, with no
    // fresh FIONREAD call anywhere in this function -- invariant 4 holds with no exception, not
    // one documented residual.
    #[test]
    fn deadline_drain_budget_is_zero_when_nothing_was_ever_tracked() {
        let budget = super::deadline_drain_budget(None, 0);
        assert_eq!(budget, 0);
    }

    // found in PR #44 pass 7 (P7-B, RED for invariant 1): a scripted clock that crosses the
    // deadline strictly BETWEEN the snapshot and the recheck must never let the bytes that
    // snapshot claimed reach the screen. `queued` scripts exactly one call: `100` (the snapshot
    // taken during the doomed iteration -- large enough that, if wrongly bound to, it would
    // consume the scripted marker below). The recheck discards it by looping back to the top
    // without ever assigning `last_pre_ceiling_fionread`, so FINALIZING reaches
    // `deadline_drain_budget` with `None` -- no re-check, no second `queued_read_bytes` call, a
    // budget of zero by construction (pass 7 correction, same PR: FINALIZING's own fresh-FIONREAD
    // fallback is gone entirely, not merely skipped here, so scripting a second `queued` value for
    // it to consume would now be dead configuration). `readable` carries a marker that must never
    // reach the screen; asserted directly on `read_calls` (never called at all) and
    // `queued_calls` (called exactly once), not inferred from the outcome -- a bug that fed the
    // bytes without the predicate happening to match would still report TimedOut.
    #[test]
    fn recheck_discards_a_snapshot_taken_after_the_clock_already_crossed_the_deadline() {
        let mut child = ScriptedTransport {
            queued: vec![100],
            readable: b"SHOULD-NEVER-BE-ADMITTED".iter().copied().collect(),
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("SHOULD-NEVER-BE-ADMITTED".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut || {
                now_calls += 1;
                // calls 1-2: next_checkpoint's own init, then the loop's first top-of-loop
                // deadline check -- both pre-deadline, so the loop reaches the snapshot. Call 3,
                // the recheck immediately after that snapshot, reports PAST the wall -- the
                // exact "clock checked once, syscall happens later, non-atomic" gap invariant 1
                // closes.
                if now_calls <= 2 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut -- the marker must never have been admitted, so the predicate \
             can never match. Got {outcome:?}"
        );
        assert_eq!(
            child.read_calls, 0,
            "expected read_available_bounded to never be called at all -- the recheck must \
             discard the snapshot before any read is attempted against it, and FINALIZING \
             correctly finds nothing left to drain"
        );
        assert_eq!(
            child.queued_calls.get(),
            1,
            "expected exactly one queued_read_bytes call -- the doomed snapshot itself. \
             FINALIZING never calls queued_read_bytes at all, so a second call here would mean \
             a fresh-FIONREAD fallback exists somewhere it should not"
        );
    }

    // found in PR #44 pass 7 (P7-B, RED for invariant 2): a zero snapshot combined with a
    // hung-up pty must report Crashed via try_wait/hung_up alone, never by attempting a read of
    // any kind -- asserted directly (read_calls == 0), not inferred from the outcome.
    #[test]
    fn zero_snapshot_with_hung_up_reports_crashed_without_ever_reading() {
        let mut child = ScriptedTransport {
            queued: vec![0],
            hung_up: true,
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-APPEARS".to_string()),
            basis,
            basis + std::time::Duration::from_secs(5),
            None,
            &mut crate::pty::wait_for_readable,
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed, got {outcome:?}"
        );
        assert_eq!(
            child.read_calls, 0,
            "expected read_available_bounded to never be called -- a zero snapshot must decide \
             liveness via try_wait/hung_up alone, never by attempting a read"
        );
    }

    // post-merge batch (2026-07-22), fix #4: a subject that writes its satisfying frame and
    // exits entirely inside the gap between a zero FIONREAD snapshot and the liveness check
    // must be judged on those final bytes, not on the stale screen the zero snapshot saw.
    // `queued` scripts three calls: the pre-read snapshot (0), the post-exit snapshot (28 --
    // the frame below), then a third, post-merge batch 2 (2026-07-22) finding #1's own
    // re-snapshot loop's terminating zero round -- without it, the ScriptedTransport cursor
    // would clamp to the LAST element and keep re-offering 28 with nothing left readable
    // (harmless: drain returns WouldBlock and the loop simply spends its remaining rounds), but
    // the explicit zero exercises the loop's clean, intentional termination instead.
    #[test]
    fn final_frame_written_between_zero_snapshot_and_exit_is_still_judged() {
        let frame: &[u8] = b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED";
        let mut child = ScriptedTransport {
            queued: vec![0, frame.len(), 0],
            readable: frame.iter().copied().collect(),
            exit: vec![Some(exited_status())],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis + std::time::Duration::from_secs(1),
            None,
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "the final frame was queued at death -- it must be drained and judged, not \
             discarded into a false Crashed: {outcome:?}"
        );
        assert!(
            child.read_calls >= 1,
            "the post-exit drain must have actually read the queued frame"
        );
    }

    // the boundary preserved: death with genuinely nothing queued stays Crashed, still
    // without a single read (invariant 2's own spirit, now with the post-mortem snapshot
    // confirming emptiness instead of assuming it).
    #[test]
    fn zero_snapshot_death_with_nothing_queued_stays_crashed_without_reading() {
        let mut child = ScriptedTransport {
            queued: vec![0, 0],
            exit: vec![Some(exited_status())],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-PAINTED".to_string()),
            basis,
            basis + std::time::Duration::from_secs(1),
            None,
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "{outcome:?}"
        );
        assert_eq!(
            child.read_calls, 0,
            "an empty post-mortem queue must never be read"
        );
    }

    // found in post-merge batch 2 (2026-07-22), finding #2: the death arm used to admit
    // whatever the post-mortem snapshot proved queued with no wall recheck of its own -- the
    // last clock check before this arm is the top-of-loop's pre-read recheck, which predates
    // the liveness syscalls (`try_wait`/`hung_up`) entirely, so a deadline expiring in that gap
    // let late bytes through. RED pre-fix: this exact scenario used to observe `Satisfied(0)`,
    // the marker wrongly admitted.
    //
    // now() calls: 1-3 are `next_checkpoint`'s own init, the loop's first top-of-loop deadline
    // check, and the pre-read snapshot's own recheck -- all pre-deadline (mirrors `recheck_
    // discards_a_snapshot_taken_after_the_clock_already_crossed_the_deadline`'s own calls 1-2,
    // plus one more here for the pre-read recheck this scenario must actually pass through, to
    // reach the zero-snapshot death arm at all). Call 4, the death arm's OWN new recheck, fires
    // immediately after `try_wait` reports the scripted exit and reports PAST the wall --
    // discarding the death observation before the post-mortem snapshot is ever taken. Every
    // call after (the second top-of-loop check, which then drives FINALIZING) reports past the
    // wall too.
    //
    // `exit` scripts a single call, `Some(exited_status())`: the ScriptedTransport cursor
    // clamps to this LAST element forever (its own doc comment), so a child observed dead via
    // `try_wait` stays dead for every later call too -- the honest shape, since a real
    // transport cannot un-observe a death. That includes FINALIZING's own SEPARATE,
    // unconditional `try_wait` revalidation (this file, the deadline branch's own tail), which
    // this loop reaches via the new recheck's `continue` once the wall has passed: it sees the
    // same latched death and reports `Crashed`, not `TimedOut` -- the outcome this test asserts.
    // The refusal this test actually exists to pin is not which `PollOutcome` variant that
    // (unrelated, pre-existing) tail decides on, but that NEITHER the post-mortem snapshot NOR
    // any read ever happens once the wall has passed -- exactly what `read_calls == 0` and
    // `queued_calls.get() == 1`, below, pin directly.
    #[test]
    fn post_mortem_drain_is_refused_once_the_wall_has_passed() {
        let mut child = ScriptedTransport {
            queued: vec![0, 99],
            readable: b"SHOULD-NEVER-BE-ADMITTED".iter().copied().collect(),
            exit: vec![Some(exited_status())],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("SHOULD-NEVER-BE-ADMITTED".to_string()),
            basis,
            deadline,
            None,
            &mut |_fd, _requested| {
                panic!(
                    "waiter must never be reached -- FINALIZING returns before this loop ever \
                     reaches its own WouldBlock arm"
                )
            },
            &mut || {
                now_calls += 1;
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed -- the child is genuinely dead (latched via the scripted exit), \
             and once the wall has passed FINALIZING's own tail try_wait reports exactly that; \
             the marker must never have been admitted either way, so the predicate can never \
             match. Got {outcome:?}"
        );
        assert_eq!(
            child.read_calls, 0,
            "expected read_available_bounded to never be called -- the death arm's new recheck \
             must discard the death observation before any post-mortem snapshot (let alone a \
             read) is attempted"
        );
        assert_eq!(
            child.queued_calls.get(),
            1,
            "expected exactly one queued_read_bytes call -- the pre-read snapshot itself. The \
             post-mortem snapshot must never even be taken once the wall has passed"
        );
    }

    // found in post-merge batch 2 (2026-07-22), finding #1: the post-mortem snapshot is now a
    // capped re-snapshot loop rather than a single snapshot+drain, since a snapshot can
    // theoretically under-report on kernels where tty buffer flushing is more asynchronous than
    // this project's own target kernel (a 73,000-iteration probe there never reproduced it --
    // see `POST_MORTEM_DRAIN_ROUNDS`'s own doc comment). `queued` scripts four post-zero-snapshot
    // calls: 12 (round 1's own snapshot), 16 (round 2's), then 0 (round 3's, the terminating
    // zero round that proves the loop stops on its own rather than spinning to the cap).
    // `readable` is a 28-byte frame -- exactly 12 + 16 -- split at that same boundary: round 1
    // reads a VT cursor-home + clear-line prefix plus two plain characters (bytes 0-11), round 2
    // reads the rest (bytes 12-27). The needle "MARKER!!" sits at byte offset 20, entirely
    // inside round 2's own chunk -- a regression that stopped after round 1 (or otherwise
    // dropped round 2's bytes) would leave the needle absent from the screen, so this test would
    // correctly fail via a non-`Satisfied` outcome, not merely via a `read_calls` count.
    #[test]
    fn post_mortem_drain_takes_multiple_rounds_when_bytes_land_between_snapshots() {
        let frame: &[u8] = b"\x1b[1;1H\x1b[2KABPADPADPAMARKER!!";
        let mut child = ScriptedTransport {
            queued: vec![0, 12, 16, 0],
            readable: frame.iter().copied().collect(),
            exit: vec![Some(exited_status())],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("MARKER!!".to_string()),
            basis,
            basis + std::time::Duration::from_secs(1),
            None,
            &mut |_fd, _requested| {
                panic!(
                    "waiter must never be reached -- the death arm resolves this sample without \
                     ever reaching the WouldBlock arm"
                )
            },
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "expected Satisfied -- both rounds' bytes must be drained and fed before the \
             post-mortem judgment is made. Got {outcome:?}"
        );
        assert!(
            child.read_calls >= 2,
            "expected at least two real drain rounds (12 bytes, then 16) -- got {}",
            child.read_calls
        );
    }

    // found in PR #44 pass 7 (P7-B, RED for invariant 3): a waiter that times out on a wait
    // bounded by the deadline must leave FINALIZING with zero budget and no fresh FIONREAD of
    // its own -- asserted directly (queued_calls == 1, the ONE call from the zero-snapshot
    // check itself), not inferred from the outcome. (Pass 7 correction, same PR:
    // deadline_drain_budget later lost fresh_fionread entirely rather than merely skipping it
    // here -- this test's queued_calls == 1 now pins the ONLY queued_read_bytes call that
    // exists anywhere on this path, not one of two the timed-out branch happened to avoid.)
    #[test]
    fn timed_out_waiter_reaches_finalizing_with_zero_budget_and_no_fresh_fionread() {
        let mut child = ScriptedTransport::default();
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-APPEARS".to_string()),
            basis,
            deadline,
            None,
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut || {
                now_calls += 1;
                // calls 1-3: next_checkpoint's own init, the first top-of-loop check, and the
                // snapshot's own recheck -- all pre-deadline, so the zero-snapshot liveness
                // check runs once (queued_calls == 1) and the waiter is reached and times out.
                // Everything after (the clamp's own now(), the second top-of-loop check)
                // reports past the wall.
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        assert_eq!(
            child.queued_calls.get(),
            1,
            "expected exactly one queued_read_bytes call -- the zero-snapshot liveness check's \
             own initial snapshot. deadline_drain_budget never calls queued_read_bytes at all, \
             so a second call here would not be a fresh-FIONREAD fallback (that path is gone) \
             but a plain bug"
        );
    }

    // found in PR #44 pass 7 re-review (codex P2, against ce496ad): the waiter's own `Readable`
    // arm used to take no action at all, on the mistaken assumption that the next iteration's
    // own top-of-loop snapshot would always get a chance to run -- but that snapshot is gated
    // behind the top-of-loop deadline check, so a thread descheduled for any nonzero time right
    // after the waiter returns `Readable` can let that check fire FIRST, reaching FINALIZING
    // with nothing tracked. `queued` scripts two calls: `0` (the zero-snapshot that puts this
    // loop into the WouldBlock/Readable arm in the first place) then `6` (the Readable arm's own
    // new recheck-guarded snapshot -- see that arm's own comment for the full derivation) --
    // "MARKER", six bytes, sits staged and genuinely readable throughout. The scripted clock
    // reports pre-deadline for every `now()` call through the Readable arm's own recheck (calls
    // 1-5: `next_checkpoint`'s init, the first top-of-loop check, the zero-snapshot's own
    // recheck, the clamp's own `now()` just before the waiter, and the Readable arm's own new
    // recheck), then crosses the wall starting at call 6 -- the SECOND top-of-loop check,
    // modeling a scheduling gap strictly AFTER this arm's own synchronous work is already done,
    // never inside it. A pre-fix run of this exact scenario (RED-verified before this fix
    // landed, by reverting just the `Readable` arm back to `{}`) reports `TimedOut`; asserting
    // `Satisfied` here now pins the fix as the permanent regression test for it.
    #[test]
    fn a_pre_deadline_readable_result_survives_a_scheduling_gap_before_the_next_top_of_loop_check()
    {
        let mut child = ScriptedTransport {
            queued: vec![0, 6],
            readable: b"MARKER".iter().copied().collect(),
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("MARKER".to_string()),
            basis,
            deadline,
            None,
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::Readable),
            &mut || {
                now_calls += 1;
                if now_calls <= 5 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "expected Satisfied -- the waiter itself proved \"MARKER\" was queued before the \
             deadline; a scheduling gap between that result and this loop's own next \
             top-of-loop check must not be able to lose it. Got {outcome:?}"
        );
    }

    // found in PR #44 pass-2 review: pins the actual property the fix is FOR -- kernel-
    // guaranteed uniqueness, not just "produces a path that happens not to collide in this one
    // test run." Two calls with the IDENTICAL template must still yield two DIFFERENT,
    // independently-existing directories -- the pid-and-counter-derived scheme this replaces
    // would already have satisfied that within one process (the counter alone prevents
    // same-process collisions), so the real distinguishing property is that a `cargo test`
    // thread-parallel run -- many independent calls racing concurrently -- never collides
    // either, which mkdtemp's own atomicity guarantees and a naive scheme does not.
    #[test]
    fn mkdtemp_under_temp_dir_produces_fresh_distinct_directories() {
        let first = super::mkdtemp_under_temp_dir("ress-perf-mkdtemp-test-XXXXXX").unwrap();
        let second = super::mkdtemp_under_temp_dir("ress-perf-mkdtemp-test-XXXXXX").unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir(), "{first:?}");
        assert!(second.is_dir(), "{second:?}");
        std::fs::remove_dir(&first).unwrap();
        std::fs::remove_dir(&second).unwrap();
    }

    fn fake_pager_env(_fake_home: &std::path::Path) -> Vec<(String, String)> {
        vec![("TERM".to_string(), "tmux-256color".to_string())]
    }

    fn argv_paint_hang(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "paint-hang".into(),
        ]
    }

    fn argv_paint_exit(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "paint-exit".into(),
        ]
    }

    fn argv_never_paint(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "never-paint".into(),
        ]
    }

    fn argv_paint_wrong_row_then_hang(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "paint-wrong-row-hang".into(),
        ]
    }

    fn argv_exit_after_key(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "exit-after-key".into(),
        ]
    }

    fn argv_slow_paint_exit(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "slow-paint-exit".into(),
        ]
    }

    fn argv_flicker_top_line(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "flicker-top-line".into(),
        ]
    }

    fn argv_continuous_bottom_row_writer(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "continuous-bottom-row-writer".into(),
        ]
    }

    fn argv_churning_top_line(_fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
        vec![
            crate::test_support::sibling_bin_path("fake-pager").into(),
            "churning-top-line".into(),
        ]
    }

    fn subject(
        name: &'static str,
        argv_for: fn(&std::path::Path) -> Vec<std::ffi::OsString>,
    ) -> super::Subject {
        super::Subject {
            name,
            argv_for,
            env_for: fake_pager_env,
            tracing: false,
        }
    }

    // fake-pager paint-hang paints once (readiness succeeds fast) then sleeps forever -- a
    // keyed scenario whose completion predicate never appears must time out at the
    // COMPLETION ceiling, not the readiness one (perf.sh's own attribution rule: whichever
    // wait actually timed out is the ceiling recorded).
    #[test]
    fn paint_then_hang_records_hung_at_the_completion_ceiling() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-hang", argv_paint_hang);
        let scenario = super::Scenario {
            name: "paint-then-hang",
            keys: b"g".to_vec(),
            readiness: super::Predicate::Painted,
            completion: Some(super::Predicate::Contains("NEVER-APPEARS".to_string())),
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 1,
            completion_s: 1,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Hung { ceiling_s: 1 });
    }

    // fake-pager paint-exit paints then exits immediately, before any key is sent -- a keyed
    // scenario must not mistake the ensuing write-against-a-dying-subject for a hang: this
    // pins the substrate's structural improvement over perf.sh (EOF/try_wait detect the exit
    // immediately; no revalidation-after-a-timeout dance is needed).
    #[test]
    fn paint_then_exit_before_keys_records_crashed_not_hung() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-exit", argv_paint_exit);
        let scenario = super::Scenario {
            name: "paint-then-exit",
            keys: b"g".to_vec(),
            readiness: super::Predicate::Painted,
            completion: Some(super::Predicate::Contains("NEVER-APPEARS".to_string())),
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        // a deliberately generous ceiling (perf.sh's own illustrative figure) -- the assertion
        // below is only meaningful if crash detection could not possibly be mistaken for
        // "waited out a tiny ceiling". the threshold is a fraction of the ceiling rather than a
        // tight absolute bound: under a fully-loaded `just ci` (the whole workspace's release
        // suite, hundreds of tests and real subprocesses contending for scheduling at once,
        // confirmed directly -- a wholly unrelated pre-existing pty.rs test showed the same
        // multi-second inflation in that same run), wall-clock scheduling latency alone can
        // reach low single-digit seconds even though the DETECTION itself is still immediate
        // (the very next poll iteration, no waiting-out-a-ceiling). a fixed 500ms bound conflated
        // "detected immediately" with "the host was not busy"; half the ceiling keeps the
        // property meaningful (crash detection, not ceiling exhaustion) without being a flaky
        // proxy for host load.
        //
        // found in PR #44 round 17 (a codex P2, sweep): audited and kept, not a race-discriminator
        // -- `outcome == Crashed` above already establishes detection correctness; this elapsed
        // check only bounds LATENCY, a QUALITATIVE gap (event-driven, microseconds-to-low-ms) vs
        // (falls back to waiting out the full ceiling, ~10s) with no plausible correct-but-slow
        // outcome anywhere near the halfway mark to blur that line, the same "qualitative, not
        // razor-thin" reasoning `wait_for_readable_reports_data_already_queued_promptly_not_
        // after_the_full_timeout`'s own doc comment already states for an identical shape.
        let ceilings = super::Ceilings {
            readiness_s: 10,
            completion_s: 10,
        };
        let started = std::time::Instant::now();
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Crashed);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(ceilings.completion_s) / 2,
            "crash detection burned the ceiling instead of catching the exit immediately: {:?}",
            started.elapsed()
        );
    }

    // fake-pager exit-after-key paints, reads exactly one key, then exits without ever
    // satisfying a completion predicate -- the same no-ceiling-burn property as the crash
    // test above, this time for a subject that dies mid-completion-wait rather than
    // pre-keypress.
    #[test]
    fn exit_after_key_records_crashed_when_completion_never_paints() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-exit-after-key", argv_exit_after_key);
        let scenario = super::Scenario {
            name: "exit-after-key",
            keys: b"x".to_vec(),
            readiness: super::Predicate::Painted,
            completion: Some(super::Predicate::Contains("NEVER-APPEARS".to_string())),
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        // see paint_then_exit_before_keys_records_crashed_not_hung's comment for why this is a
        // ceiling fraction, not a tight absolute bound.
        let ceilings = super::Ceilings {
            readiness_s: 10,
            completion_s: 10,
        };
        let started = std::time::Instant::now();
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Crashed);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(ceilings.completion_s) / 2,
            "crash detection burned the ceiling instead of catching the exit immediately: {:?}",
            started.elapsed()
        );
    }

    // fake-pager never-paint never produces a byte -- readiness itself times out, so the
    // recorded ceiling must be the READINESS one (there is no completion phase to blame it
    // on: keys is empty, completion is None).
    #[test]
    fn never_paint_records_hung_at_the_readiness_ceiling() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-never-paint", argv_never_paint);
        let scenario = super::Scenario {
            name: "never-paint",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 1,
            completion_s: 1,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Hung { ceiling_s: 1 });
    }

    // found in PR #44's round-3 bot review: a subject that dies while poll_predicate's own
    // WouldBlock branch is asleep can have the deadline expire before the loop ever gets
    // another chance to observe the death via read_available/try_wait -- pre-fix, that raced
    // straight to TimedOut (Hung) instead of Crashed. Deliberately NOT reproduced by racing a
    // fake-pager subject's own exit against a tiny ceiling (the brief's own first idea, and
    // correctly flagged as racy): the fix's code path is the SAME branch regardless of which
    // loop iteration finally trips the deadline, so killing the child directly, confirming it's
    // observably dead, and THEN calling poll_predicate with an already-expired deadline
    // exercises the exact lines under test deterministically, with no dependency on hitting a
    // narrow timing window.
    #[test]
    fn timeout_revalidates_a_child_that_already_died_and_reports_crashed() {
        let argv = argv_never_paint(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        unsafe { libc::kill(child.pid(), libc::SIGKILL) };
        // wait for the kernel to make the death actually observable (Eof on the pty master)
        // before proceeding -- a fixed sleep would be a guess; polling for the real signal
        // read_available itself depends on is not. Non-destructive: draining for Eof here
        // doesn't consume anything poll_predicate's own later read still needs.
        let mut throwaway = Vec::new();
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            throwaway.clear();
            if matches!(
                child.read_available(&mut throwaway).unwrap(),
                crate::pty::ReadStatus::Eof
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "child never became observably dead"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        // an already-expired deadline: the very first loop iteration inside poll_predicate
        // hits the deadline check immediately, before any read of its own -- the same branch a
        // later iteration would eventually reach, just reached sooner.
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Painted,
            basis,
            basis,
            None,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed, got {outcome:?}"
        );
    }

    // found in PR #44's round-4 bot review: a frame that arrives and sits unread in the pty's
    // own kernel buffer when the deadline expires used to be discarded by the timeout branch's
    // final check, which only ever looked for Eof -- reporting Hung for a sample that had
    // actually succeeded. Split in pass 7 (P7-B) into the two honest cases the original single
    // test conflated -- see this pair's own doc comments for why.
    //
    // this half: FINALIZING's own drain genuinely does the recovering -- Admitting tracks a
    // real, provably-pre-deadline snapshot, but a SHORT read (a real pty master's own `read()`
    // CAN return fewer bytes than are truly queued -- `drain_available`'s own doc comment cites
    // this, and pass 3's own investigation reproduced it directly against a real large payload)
    // leaves part of the marker un-consumed when the deadline fires one iteration later.
    // `ScriptedTransport`'s `read_caps` simulates that short read deterministically -- a real pty
    // was tried and rejected for this specific case: this system's own pty buffer capacity
    // (~4095 bytes, found empirically in pass 3) means a single real `read()` already retrieves
    // everything that could ever be queued, so a genuine short read against a small marker isn't
    // reliably reproducible against a real child at all -- only a real payload sized well past
    // that cap could ever force one (pass 2/3's own `large-payload-paint-at-end` scenario did,
    // for a different property; retired in pass 7, P7-D, once its own consuming test went
    // vacuous post-v3/zero -- see `deadline_drain_caps_each_read_to_the_shrinking_remainder`'s
    // own doc comment for the full account).
    #[test]
    fn deadline_recovery_drains_a_real_short_reads_own_remainder() {
        let mut child = ScriptedTransport {
            queued: vec![6],
            readable: b"NEEDLE".iter().copied().collect(),
            read_caps: vec![3],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEEDLE".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut || {
                now_calls += 1;
                // calls 1-3: next_checkpoint's own init, the first top-of-loop check, and the
                // snapshot's own recheck -- all pre-deadline, so Admitting completes exactly one
                // short read (3 of the 6 bytes FIONREAD proved queued), tracks Some(6) with
                // consumed=3, and the predicate does not yet match ("NEE" alone). Everything
                // after reports past the wall, so the second top-of-loop check routes straight
                // to FINALIZING with that still-valid Tracked snapshot.
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "expected Satisfied -- FINALIZING's own drain must spend the remaining 3 bytes \
             (6 - 3) the tracked snapshot proved were queued pre-deadline, completing the \
             marker. Got {outcome:?}"
        );
    }

    // the other half: NO Admitting iteration has ever completed at all when the deadline fires --
    // this test's own `basis == deadline` construction forces the SIMPLEST of the (now several,
    // see `deadline_drain_budget`'s own doc comment for the full derivation) routes to `None`: the
    // very first deadline check already finds the wall passed, so the loop never even attempts a
    // snapshot. There is no pre-deadline evidence whatsoever, and a subject that genuinely painted
    // before this call even started cannot be credited via a fresh, post-deadline FIONREAD (pass
    // 7, P7-B, retired that fallback entirely -- see `deadline_drain_budget`'s own doc comment).
    // This is a deliberate semantic sharpening, not a regression: a completion this harness's own
    // thread was never scheduled in time to look at, even once, before the wall cannot honestly be
    // claimed as a ceiling-beat -- we never witnessed it pre-ceiling, so TimedOut is the honest
    // verdict.
    //
    // found in PR #44 pass 7 (P7-D, a p6-review2 finding): this test's own `basis == deadline` is
    // genuinely degenerate/test-only -- it needs the harness's own very first look to already be
    // late, which real scheduling delay essentially never produces on its own. But NOT every route
    // to `None` shares that: a subject that produces no output for its whole deadline window takes
    // the zero-snapshot path on EVERY iteration (which never writes `Some` either -- see
    // `deadline_drain_budget`'s own doc comment), so `None` persisting all the way to FINALIZING
    // for a genuinely slow or hung real subject is the ORDINARY case this branch exists for, not a
    // degenerate one. Only the tighter races -- this test's own `basis == deadline`, or a recheck
    // landing in the microsecond gap between a snapshot and its own immediate recheck -- are
    // test-only. This test's own construction is specifically `basis == deadline`, deliberately,
    // not a stand-in for the ordinary slow-subject route: that one needs no dedicated test of its
    // own, since it is just this same zero-budget FINALIZING outcome, reached the unremarkable
    // way.
    #[test]
    fn timeout_when_no_admitting_iteration_ever_ran_before_the_deadline() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // codex P2, PR #44 pass 7 re-review: a fixed sleep here does not GUARANTEE paint-hang
        // has queued its frame -- the assertion below holds either way (TimedOut is correct
        // regardless, since basis == deadline zeroes the budget unconditionally), but under load
        // a too-short sleep would make it hold for the WRONG reason: nothing queued at all,
        // rather than the scenario this test's own name and comment above claim to prove (a
        // genuinely queued frame, declined only because Admitting never ran). Poll
        // `queued_read_bytes` (FIONREAD) instead, bounded so this fails loud -- not silently
        // proves nothing -- if paint-hang never paints.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        // pins the premise right at the point of use: a frame is provably queued going into the
        // call under test, not merely likely so.
        assert!(
            child.queued_read_bytes().unwrap() > 0,
            "the frame must still be queued going into poll_predicate"
        );
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Painted,
            basis,
            basis,
            None,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut -- the frame IS genuinely sitting there, unread (confirmed via \
             queued_read_bytes just above), but basis == deadline means Admitting never ran even \
             once, so there is no pre-deadline snapshot to spend and no fresh FIONREAD to take \
             one from. Got {outcome:?}"
        );
    }

    // found in PR #44 pass 6 S1 (a codex P2, raised during review of the #2 fix): bounding the
    // normal path's read to its own FIONREAD snapshot cannot use `read_available_bounded`
    // blindly on a `0` snapshot -- FIONREAD reports `0` both when nothing is queued YET (a real
    // read would block) and when the child has already exited (a real read would return `Eof`
    // immediately), and these need different handling. This test pins the dead-child half:
    // `paint-exit` paints once and exits immediately, so draining it manually first (own
    // "already observed a real Eof" loop, not what's under test) leaves a child that is fully,
    // observably gone with nothing queued -- the exact ambiguous-zero state. If a `0` snapshot
    // silently trusted `WouldBlock` (the way a naive `read_available_bounded(buf, 0)` would,
    // skipping the read entirely), this call would depend on `try_wait` alone, one iteration at
    // a time, to ever notice -- correct eventually, but a busy spin against the waiter's own
    // `POLLHUP`-driven instant wake (never actually blocking) rather than the immediate,
    // single-read detection this function relies on elsewhere. A generous ceiling (10s) with a
    // "detected well under half of it" bound distinguishes "detected promptly, by some prompt
    // mechanism" from "waited out a stall" without asserting on exactly how many microseconds
    // anything took -- the same fraction-of-ceiling shape `paint_then_exit_before_keys_records_
    // crashed_not_hung` (this module's own tests) already establishes for the identical reason.
    #[test]
    fn zero_snapshot_after_a_real_exit_still_detects_crashed_promptly() {
        let argv = argv_paint_exit(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        // drains to a REAL, observed Eof first (discarding the paint -- irrelevant to this
        // test) so the FIONREAD snapshot `poll_predicate` itself takes is genuinely `0`, not
        // hopefully so.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            buf.clear();
            match child.read_available(&mut buf).unwrap() {
                crate::pty::ReadStatus::Eof => break,
                crate::pty::ReadStatus::Data(_) => {}
                crate::pty::ReadStatus::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < ready_deadline,
                        "paint-exit never reached eof"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        buf.clear();
        let basis = std::time::Instant::now();
        let ceilings_deadline = basis + std::time::Duration::from_secs(10);
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-APPEARS".to_string()),
            basis,
            ceilings_deadline,
            None,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed, got {outcome:?}"
        );
        assert!(
            basis.elapsed() < std::time::Duration::from_secs(5),
            "detection took {:?} against a 10s ceiling -- looks like a stall/spin, not the \
             prompt detection a zero snapshot must still provide",
            basis.elapsed()
        );
    }

    // the sibling half: a `0` snapshot against a child that is genuinely STILL ALIVE (not just
    // not-yet-written-to) must still correctly report `TimedOut`, not misread its own probe read
    // as anything else -- `paint-hang` never exits, so draining its one paint first leaves a
    // real `0`-queued, genuinely-not-blocked-forever state to probe against.
    #[test]
    fn zero_snapshot_while_still_alive_reports_timed_out_not_crashed() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // found in PR #44 pass 8 (U-perf): a fixed sleep here does not GUARANTEE paint-hang has
        // queued its frame before the drain below -- under load a too-short sleep would leave
        // nothing queued at all rather than the genuinely-alive-but-quiet state this test's own
        // name claims to exercise. Poll `queued_read_bytes` (FIONREAD) instead, bounded so this
        // fails loud -- not silently proves nothing -- if paint-hang never paints (same idiom as
        // `timeout_when_no_admitting_iteration_ever_ran_before_the_deadline`, below).
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match child.read_available(&mut buf).unwrap() {
                crate::pty::ReadStatus::Data(_) => {}
                crate::pty::ReadStatus::WouldBlock => break,
                crate::pty::ReadStatus::Eof => panic!("paint-hang should not have exited"),
            }
        }
        buf.clear();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_millis(100);
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-APPEARS".to_string()),
            basis,
            deadline,
            None,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut (still alive, nothing queued), got {outcome:?}"
        );
    }

    // found in PR #44's round-5 bot review: the deadline branch used to exclude
    // `TopLineStableStartsWith` from its own recovery read entirely, so a stability pair begun
    // by a real checkpoint just before the ceiling -- and never disturbed since, because
    // paint-hang holds its line forever once painted -- reported Hung instead of completing the
    // pair the buffered re-read plainly confirms. Deterministic, not raced: this test drives its
    // OWN "first checkpoint" (a manual drain-to-WouldBlock, standing in for the real WouldBlock
    // branch's own read) only after confirming real data is actually queued -- no fixed delay
    // here, just the bounded `queued_read_bytes()` poll below (U-perf, pass 8; see this
    // function's own next comment for the fuller rationale) -- confirms the line it saw actually
    // matches, and only THEN calls `poll_predicate` with `stable_confirmed_at_start:
    // Some(generation)` (pass 5, C2: the generation `Screen::top_row_generation` reports
    // immediately after that same manual drain -- the real first checkpoint would have captured
    // the identical value) and an already-expired deadline -- no dependency on landing a real
    // checkpoint within a narrow window of the real ceiling.
    #[test]
    fn deadline_completes_a_stability_pair_begun_before_expiry() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // found in PR #44 pass 8 (U-perf): see `zero_snapshot_while_still_alive_reports_timed_
        // out_not_crashed`'s own identical comment for why a fixed sleep here is replaced with an
        // active, bounded, fail-loud poll instead.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        // stands in for the real first WouldBlock checkpoint: drain whatever paint-hang has
        // produced (a single paint, per its own name) until the pty goes quiet.
        loop {
            buf.clear();
            match child.read_available(&mut buf).unwrap() {
                crate::pty::ReadStatus::Data(_) => screen.feed(&buf),
                crate::pty::ReadStatus::WouldBlock => break,
                crate::pty::ReadStatus::Eof => panic!("paint-hang should not have exited"),
            }
        }
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "row 0: {:?}",
            screen.row_text(0)
        );
        let confirmed_at_generation = screen.top_row_generation();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            // this test's own property is the generation/content pair completing at the
            // deadline, not the fix #5 spacing rule -- backdated a full poll interval so real
            // setup time above (a bounded poll plus a manual drain) can never accidentally
            // leave the pair under-spaced.
            Some((confirmed_at_generation, basis - super::POLL_INTERVAL)),
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "expected Satisfied, got {outcome:?}"
        );
    }

    // found in PR #44 round 9: the deadline branch's own recovery -- a stability pair begun by a
    // real checkpoint, then re-confirmed here by a frozen re-read -- used to trust that re-read
    // unconditionally, even against a screen the child died without ever getting the chance to
    // update again. Deterministic, not raced, and deliberately NOT built on a live Eof-vs-
    // try_wait timing race (see `checkpoint_confirms_only_while_alive`'s own doc comment for why
    // that race cannot be relied on to land in either direction): unlike the regular checkpoint
    // path, THIS branch's own bug fires unconditionally whenever `satisfied` computes true,
    // regardless of `status` -- so confirming the child is FULLY, observably dead first (Eof,
    // not just a SIGKILL sent) still exercises the exact code under test, with zero ambiguity
    // about which check catches it. Same "first checkpoint" simulation as the test above (a
    // manual drain-to-WouldBlock after a generous head start), but the child is killed and
    // confirmed dead before `poll_predicate` is ever called.
    #[test]
    fn deadline_stability_pair_requires_the_child_to_still_be_alive_not_a_frozen_screen() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // found in PR #44 pass 8 (U-perf): see `zero_snapshot_while_still_alive_reports_timed_
        // out_not_crashed`'s own identical comment for why a fixed sleep here is replaced with an
        // active, bounded, fail-loud poll instead.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match child.read_available(&mut buf).unwrap() {
                crate::pty::ReadStatus::Data(_) => screen.feed(&buf),
                crate::pty::ReadStatus::WouldBlock => break,
                crate::pty::ReadStatus::Eof => panic!("paint-hang should not have exited yet"),
            }
        }
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "row 0: {:?}",
            screen.row_text(0)
        );
        let confirmed_at_generation = screen.top_row_generation();
        // NOW kill it -- `screen` already holds the matching content, permanently frozen from
        // here on (a dead pager cannot redraw), so `confirmed_at_generation` above is also this
        // screen's own final generation.
        unsafe { libc::kill(child.pid(), libc::SIGKILL) };
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            buf.clear();
            if matches!(
                child.read_available(&mut buf).unwrap(),
                crate::pty::ReadStatus::Eof
            ) {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "child never became observably dead"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            // this test's own property is the liveness check, not the fix #5 spacing rule --
            // backdated a full poll interval so real setup time above (a bounded poll for the
            // child to become observably dead) can never accidentally leave the pair
            // under-spaced.
            Some((confirmed_at_generation, basis - super::POLL_INTERVAL)),
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed (the child died before the second, LIVE confirmation), got \
             {outcome:?}"
        );
    }

    // the other half of the same fix: a single deadline-branch match must never START a
    // stability pair on its own. Same scenario as the test above (a queued frame CONFIRMED via
    // `queued_read_bytes`, not merely hoped for after a fixed sleep -- codex P2, PR #44 pass 7
    // re-review: the old fixed-sleep version could pass vacuously if the sleep undershot, since
    // nothing-queued-at-all ALSO reports TimedOut, just for the wrong reason -- this test's own
    // point, "despite the content matching," needs the content to genuinely be there) but
    // `stable_confirmed_at_start: None` -- no real checkpoint has happened from poll_predicate's
    // own point of view -- so this must stay TimedOut despite the content matching, exactly as
    // it did before this round's fix.
    #[test]
    fn deadline_never_starts_a_stability_pair_on_its_own() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        // pins the premise right at the point of use, same as the sibling test above.
        assert!(
            child.queued_read_bytes().unwrap() > 0,
            "the frame must still be queued going into poll_predicate"
        );
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            None,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
    }

    // post-merge batch (2026-07-22), fix #5: the deadline branch is this predicate's second
    // "final look" site, and it used to accept a stability pair whose first confirmation
    // landed ONE MILLISECOND before the wall -- bypassing the documented requirement that
    // confirmations be spaced by the poll interval. The pair's spacing is measured against
    // the WALL itself (deterministic), not against however late the branch actually runs.
    #[test]
    fn deadline_finalization_refuses_a_confirmation_younger_than_the_poll_interval() {
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        let generation = screen.top_row_generation();
        let mut child = ScriptedTransport::default();
        let basis = std::time::Instant::now();
        // basis == deadline: the deadline branch is the ONLY look this call ever takes (the
        // same trick the existing seeded deadline test uses).
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            // confirmed 1ms before the wall: generation matches, content matches, but the
            // pair spans less than POLL_INTERVAL -- must NOT satisfy.
            Some((generation, basis - std::time::Duration::from_millis(1))),
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "an under-spaced pair at the wall must time out, not satisfy: {outcome:?}"
        );
    }

    // the sibling boundary: a confirmation aged exactly one full poll interval at the wall IS
    // a properly spaced pair -- the fix must not over-reject.
    #[test]
    fn deadline_finalization_accepts_a_confirmation_aged_a_full_poll_interval() {
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        let generation = screen.top_row_generation();
        let mut child = ScriptedTransport::default();
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            Some((generation, basis - super::POLL_INTERVAL)),
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "a properly spaced pair must still satisfy at the wall: {outcome:?}"
        );
    }

    // found in PR #44 pass 5 (C2): the exact finding, deterministic, using the same
    // seeded-checkpoint precedent as `deadline_completes_a_stability_pair_begun_before_expiry`
    // above -- this test's own manual "first checkpoint" simulation is that test's, this one
    // then injects CHURN between it and the "second checkpoint" (the deadline-branch call): a
    // synthetic feed that changes row 0 away from the needle, then a second synthetic feed that
    // changes it right back. The pre-C2 logic (a bare `bool`, "did the content match last
    // time") cannot see this at all -- content matches now, content matched then, so it settles,
    // exactly as if nothing had happened in between. C2's own generation check catches it: the
    // churn bumps `top_row_generation` twice, so the generation recorded at the first checkpoint
    // no longer equals the generation at the second, even though the CONTENT happens to agree
    // again -- the pair is correctly refused.
    #[test]
    fn deadline_declines_a_stability_pair_when_row_zero_churned_and_reverted_in_between() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let mut child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // found in PR #44 pass 8 (U-perf): see `zero_snapshot_while_still_alive_reports_timed_
        // out_not_crashed`'s own identical comment for why a fixed sleep here is replaced with an
        // active, bounded, fail-loud poll instead.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        // stands in for the real first checkpoint, exactly as in the seeded-checkpoint tests
        // above.
        loop {
            buf.clear();
            match child.read_available(&mut buf).unwrap() {
                crate::pty::ReadStatus::Data(_) => screen.feed(&buf),
                crate::pty::ReadStatus::WouldBlock => break,
                crate::pty::ReadStatus::Eof => panic!("paint-hang should not have exited"),
            }
        }
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "row 0: {:?}",
            screen.row_text(0)
        );
        let confirmed_at_generation = screen.top_row_generation();
        // the churn: row 0 changes away from the needle, then changes right back -- an
        // intermediate state this test observes directly (it fed the bytes itself), the exact
        // shape a differently-chunked real read could also have surfaced, per `Screen::feed`'s
        // own honesty note. Fed directly into `screen`, not through the child, so this test does
        // not depend on racing any real subject's own write timing.
        screen.feed(b"\x1b[1;1H\x1b[2Ksomething-else-entirely");
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        assert_ne!(
            screen.top_row_generation(),
            confirmed_at_generation,
            "the churn must have bumped the generation at least once"
        );
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "the content must have reverted to match the needle again -- that is the whole \
             point: the pre-C2 logic cannot see anything wrong here"
        );
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            basis,
            // backdated a full poll interval so the new fix #5 spacing rule cannot shadow the
            // generation check this test exists to pin.
            Some((confirmed_at_generation, basis - super::POLL_INTERVAL)),
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut (row 0 churned and reverted between the two checkpoints -- \
             content-only agreement is not enough, C2's own generation check must refuse this \
             pair), got {outcome:?}"
        );
    }

    // found in PR #44 pass-5-review-followup (finding 2): the test above pins C2's own generation
    // check on the DEADLINE branch specifically (runner.rs's own deadline-recovery site); this one
    // pins the identical check on the REGULAR (cadence-driven, non-deadline) checkpoint -- the
    // primary settlement path every live sample actually takes, which had no dedicated coverage
    // of its own (reverting just that one site left all 139 ress-perf tests green).
    //
    // Cannot use the deadline test's own `basis == deadline` trick here (that trick makes the
    // deadline branch the ONLY look this call ever gets, which is exactly what tests the deadline
    // branch and exactly what must NOT happen here) -- and a bare "settles later instead of
    // TimedOut forever" framing does not work either: once row 0 stops churning, ANY later look
    // (a further regular checkpoint, or an eventual deadline) sees the SAME held-steady generation
    // and legitimately completes a fresh pair, by design (a stale confirmation resets, it does not
    // poison every pair after it) -- so the discriminating signal cannot be "TimedOut forever," it
    // has to be an EVENT, not an elapsed-time comparison (see run_sample's own doc comment on
    // exactly this point).
    //
    // The event used here: `stable_confirmed_at_start` seeds a stale confirmation (the same
    // churn-then-revert as the test above, fed directly into `screen`, bypassing the child) so
    // the regular checkpoint's OWN first evaluation -- effectively instant, since `next_checkpoint`
    // starts at `Instant::now()` -- is the one under test, with a deadline far enough away that
    // only the child's own death, not a real timeout, can end this call. Old, bool-shaped logic
    // ("was there any prior match") accepts the seeded, stale confirmation immediately: `Satisfied`.
    // C2's own generation-gated logic refuses it (the seeded generation does not match the
    // post-churn one), so the pair is not complete when the child dies -- the same "death before
    // the confirming checkpoint means Crashed, not Completed" boundary this file already
    // establishes elsewhere catches it: `Crashed`. Two different enum variants, not a timing
    // threshold.
    //
    // found in PR #44 round 17 (a codex P2): the ORIGINAL version of this test killed the child
    // from a SEPARATE thread, sleeping 3ms first, reasoning that 3ms was comfortably longer than
    // the first checkpoint's own near-instant evaluation but comfortably shorter than the next
    // `POLL_INTERVAL` (10ms) tick. That is exactly a scheduler-timing oracle, and a silent one:
    // `poll_predicate_with_waiter`'s own loop has an EARLIER, unconditional `if
    // matches!(status, Eof) { return Crashed }` check, well before the regular checkpoint block
    // (see that function's own body, above). If a sufficiently loaded host starves this loop's own
    // first iteration past the killer thread's 3ms, the child is already dead before the loop ever
    // reads for the first time -- that earlier Eof check fires immediately, and BOTH the pre-C2
    // (buggy) and C2 (fixed) checkpoint logic are bypassed identically, neither one ever reached.
    // Both then report the SAME `Crashed` this test asserts: a silent false-pass for a fix that
    // was never actually exercised. (The 66-run stress test elsewhere in this crate only proves no
    // false-FAIL under load; a false-PASS needs running against the PRE-fix code under load, which
    // nothing here did.)
    //
    // found in PR #44 pass 7 (P7-B): converted to a scripted transport -- no real process, no
    // real pty, no real SIGKILL. The death this test needs sequenced strictly AFTER the first
    // checkpoint (round 9's own empirical Eof-vs-death-detection finding, cited above, motivated
    // timing this off a real kill through the waiter) is now scripted directly: `try_wait`'s own
    // answer sequence is `[alive, exited]` -- call #1, inside this iteration's zero-snapshot
    // liveness check (which the loop reaches BEFORE the checkpoint), reports alive; call #2, on
    // the NEXT iteration (necessarily after the first checkpoint has already run, by the exact
    // same "two ordered statements in one loop body, not two racing threads" construction the
    // pre-P7-B version relied on a real kill's own timing to approximate) reports exited. No
    // waiter trick needed to sequence it -- the sequence IS the script.
    #[test]
    fn stable_checkpoint_declines_a_pair_when_row_zero_churned_and_reverted_since_the_last_confirmation()
     {
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "row 0: {:?}",
            screen.row_text(0)
        );
        let confirmed_at_generation = screen.top_row_generation();
        screen.feed(b"\x1b[1;1H\x1b[2Ksomething-else-entirely");
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        assert_ne!(
            screen.top_row_generation(),
            confirmed_at_generation,
            "the churn must have bumped the generation at least once"
        );
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "the content must have reverted to match the needle again"
        );
        let mut child = ScriptedTransport {
            exit: vec![None, Some(exited_status())],
            ..Default::default()
        };
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            // far enough away that only the scripted death (via `try_wait`'s own sequence)
            // can end this call -- not a real timeout racing anything.
            basis + std::time::Duration::from_secs(2),
            // this call's outcome is decided by the REGULAR checkpoint (the deadline is 2s
            // away) plus the scripted death, neither of which consults the seeded instant --
            // inert, so `basis` is as good as any other value here.
            Some((confirmed_at_generation, basis)),
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed -- the seeded pair should be refused by the regular checkpoint's \
             own generation check (its content-only agreement is not enough, exactly like the \
             deadline-branch test above), so the scripted death (sequenced strictly after that \
             checkpoint) is observed BEFORE any pair ever completes; old, bool-shaped logic would \
             have accepted the stale confirmation immediately instead (Satisfied), before the \
             second try_wait call ever ran. Got {outcome:?}"
        );
    }

    // found by the parity gate that retired perf.sh: less -S on single-line-256m.log writes its bottom status
    // line (perf.sh's PANE_ROWS puts that at row 49) almost immediately, then can spend 30s+
    // scanning backward for a line start that does not exist -- so row 0, the content row, never
    // gets anything within the ceiling. perf.sh's own readiness check (`pane_row1`, literally
    // `pane_text | head -n1`) correctly keeps waiting and reports the hang; an "any cell on
    // screen" reading of Painted would mistake the status-line write for a paint and report an
    // instant, wrong success. fake-pager's paint-wrong-row-hang reproduces the same shape
    // (content on row 5, row 0 never touched) so this is a deterministic regression test rather
    // than a dependency on a real pager's real bug.
    #[test]
    fn painted_predicate_ignores_content_outside_row_zero() {
        let (_guard, fixture) = write_fixture();
        let subject = subject(
            "fake-pager-paint-wrong-row-hang",
            argv_paint_wrong_row_then_hang,
        );
        let scenario = super::Scenario {
            name: "paint-wrong-row-hang",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 1,
            completion_s: 1,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Hung { ceiling_s: 1 });
    }

    // found by PR #44's bot review, corrected in pass 5 (B2, a codex P2): a pager that paints a
    // matching frame and then exits immediately can be reported Completed OR Crashed here,
    // legitimately, depending on whether `run_sample`'s own post-readiness revalidation happens
    // to run before or after the exit becomes observable -- poll_predicate's readiness check can
    // be satisfied on the very Data read that carries the paint bytes, returning Satisfied
    // before ever observing the child's own exit, so this revalidation (perf.sh's own run_sample
    // did the identical thing right after a successful readiness wait) is what decides which of
    // the two answers a given run gets. Only scenarios with NO completion phase are exposed
    // (open; less's deep-goto leg) -- a scenario WITH a completion phase still catches a
    // post-readiness death via that phase's own poll_predicate call, which is why the existing
    // paint-then-exit tests above (both keyed, both with a completion predicate) never hit this
    // branch at all.
    //
    // The ORIGINAL version of this test asserted Crashed unconditionally and failed once in
    // review -- correctly: that assertion was WRONG, not merely flaky, since Completed is a
    // legitimate outcome of the identical interleaving too (see `no_completion_phase_still_
    // alive`'s own doc comment for the full derivation). Split into three tests below: the pure
    // decision function pinned deterministically on both sides via injection (no process
    // involved at all), a real, end-to-end, but still fully deterministic confirmation of the
    // ALIVE side (paint-hang never exits on its own, so there is no race to land wrong), and a
    // race-tolerant smoke test retaining the ORIGINAL real subject shape (paint-exit) with an
    // honestly weaker assertion -- it cannot pin the DEAD side deterministically without
    // controlling `run_sample`'s own internal spawn timing, so it only guards against the whole
    // path hanging or erroring, accepting either legitimate outcome.
    #[test]
    fn no_completion_phase_still_alive_reports_completed_side_when_injected_alive() {
        assert!(super::no_completion_phase_still_alive(|| Ok(true)).unwrap());
    }

    #[test]
    fn no_completion_phase_still_alive_reports_crashed_side_when_injected_dead() {
        assert!(!super::no_completion_phase_still_alive(|| Ok(false)).unwrap());
    }

    #[test]
    fn paint_hang_with_no_completion_phase_records_completed() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-hang", argv_paint_hang);
        let scenario = super::Scenario {
            name: "paint-hang-no-completion",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert!(
            matches!(outcome, super::SampleOutcome::Completed { .. }),
            "expected Completed (paint-hang never exits on its own, so the revalidation must \
             always find it alive), got {outcome:?}"
        );
    }

    // found in PR #44 round 16 (a codex P2): pins `run_sample`'s own WIRING of
    // `rss_kib_if_reported` into its no-completion-phase call site -- not just that the
    // extracted gate function behaves correctly in isolation (see the `rss_kib_if_reported_*`
    // tests near `settled_rss_kib`'s own, below), but that a real `Completed` sample genuinely
    // comes back with `rss_kib: None` for a non-reporting scenario, through the real call site.
    // paint-hang never exits on its own, so this is the same deterministic "ALIVE side" shape
    // `paint_hang_with_no_completion_phase_records_completed` above already establishes -- no
    // race to land wrong.
    #[test]
    fn completed_sample_skips_rss_for_a_non_reporting_scenario() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-hang", argv_paint_hang);
        let scenario = super::Scenario {
            name: "non-reporting-no-completion",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        let super::SampleOutcome::Completed { rss_kib, .. } = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(
            rss_kib, None,
            "a non-reporting scenario must not settle or read RSS at all"
        );
    }

    // the sibling half, through `run_sample`'s OTHER Completed call site (a scenario WITH a
    // completion phase, jump-end's own shape) -- both call sites route through the same
    // `rss_kib_if_reported` gate, but only an integration test through each literal call site
    // proves both are actually wired, not just one of the two copies.
    #[test]
    fn completed_sample_reports_rss_for_a_reporting_scenario() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-hang", argv_paint_hang);
        let scenario = super::Scenario {
            name: "reporting-with-completion",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: Some(super::Predicate::Painted),
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            reports_rss: true,
        };
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        let super::SampleOutcome::Completed { rss_kib, .. } = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert!(
            rss_kib.is_some(),
            "a reporting scenario must still settle and read a real RSS value -- fake-pager \
             paint-hang is a genuinely live process throughout, so this reading cannot race an \
             exit"
        );
    }

    #[test]
    fn paint_exit_with_no_completion_phase_lands_in_the_legitimate_outcome_space() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-exit", argv_paint_exit);
        let scenario = super::Scenario {
            name: "paint-exit-no-completion",
            keys: Vec::new(),
            readiness: super::Predicate::Painted,
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert!(
            matches!(
                outcome,
                super::SampleOutcome::Completed { .. } | super::SampleOutcome::Crashed
            ),
            "expected Completed or Crashed (both are legitimate depending on interleaving -- \
             see no_completion_phase_still_alive's own doc comment), got {outcome:?}"
        );
    }

    // fake-pager slow-paint-exit sleeps a known ~300ms before painting, then exits -- Launch
    // basis measures from spawn (includes the pre-paint sleep); Keypress basis measures from
    // just before the key is sent (right after readiness, so it excludes that sleep almost
    // entirely). same subject, same scenario shape, only the basis differs.
    #[test]
    fn completed_sample_measures_from_the_declared_basis() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-slow-paint-exit", argv_slow_paint_exit);
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let launch_scenario = super::Scenario {
            name: "timing-basis-launch",
            keys: b"q".to_vec(),
            readiness: super::Predicate::Painted,
            completion: Some(super::Predicate::Painted),
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let keypress_scenario = super::Scenario {
            timing_basis: super::TimingBasis::Keypress,
            ..launch_scenario.clone()
        };
        let launch_outcome =
            super::run_sample(&subject, &launch_scenario, &fixture, &ceilings).unwrap();
        let keypress_outcome =
            super::run_sample(&subject, &keypress_scenario, &fixture, &ceilings).unwrap();
        let super::SampleOutcome::Completed { ms: launch_ms, .. } = launch_outcome else {
            panic!("expected Completed, got {launch_outcome:?}");
        };
        let super::SampleOutcome::Completed {
            ms: keypress_ms, ..
        } = keypress_outcome
        else {
            panic!("expected Completed, got {keypress_outcome:?}");
        };
        assert!(
            launch_ms > keypress_ms,
            "launch basis ({launch_ms}ms) should exceed keypress basis ({keypress_ms}ms) by \
             roughly the subject's pre-paint delay"
        );
    }

    // fake-pager paint-hang paints once and holds that exact screen forever -- the top line
    // never changes after the initial paint, so two consecutive polls always agree.
    // `TopLineStableStartsWith` must still succeed against a genuinely steady value (it is not
    // a predicate that can never be satisfied) -- paired with the never-settles test just
    // below, which pins the other half: a value that never HOLDS never satisfies it either.
    #[test]
    fn stable_top_line_is_satisfied_once_the_target_line_holds_steady() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-paint-hang", argv_paint_hang);
        let scenario = super::Scenario {
            name: "stable-readiness-on-a-steady-line",
            keys: Vec::new(),
            readiness: super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert!(
            matches!(outcome, super::SampleOutcome::Completed { .. }),
            "expected Completed, got {outcome:?}"
        );
    }

    // found in PR #44 round 9: the original version of this test used a dedicated fake-pager
    // mode (paint, hold 3ms, exit on its own) reasoning that 3ms was comfortably shorter than
    // the ~20ms two LIVE checkpoints need to confirm readiness on their own. Verified for
    // reliability at the time (20/20 consecutive runs green) -- but only ever run unloaded.
    //
    // found in PR #44 pass 6 S3 (#7, "remaining oracles" sweep, the user's own item (b) --
    // "stable-then-exit relies on a 3ms sleep beating a checkpoint"): p6-review2 REPRODUCED a
    // real flake under adversarial load (3.5x CPU oversubscription: 2/80 runs failed "expected
    // Crashed, got Completed") that the original 20/20-unloaded verification never had a chance
    // to catch. `sleep()` only guarantees a LOWER bound on duration; unbounded POST-sleep
    // scheduling delay under contention can let the child's own exit slip past the reader's
    // independently-clocked checkpoint regardless of how the 3ms itself was chosen -- exactly
    // the "no fixed duration survives enough scheduling delay" class this whole pass exists to
    // eliminate, not merely narrow (widening the constant would only move the goalposts, not
    // remove them). Each observed outcome was individually correct for what actually happened
    // (not a production defect) -- this was a genuine test-side race, in a real path this crate
    // cares about.
    //
    // The dies-before-confirmation DECISION is already covered deterministically by
    // `checkpoint_confirms_only_while_alive`'s own injected-liveness tests (this module's own
    // tests, above) -- this test's own job was always narrower: prove the REAL poll loop, with
    // a REAL child, actually REACHES that decision, which does not need to hinge on winning a
    // race. Converted to the exact same shape
    // `stable_checkpoint_declines_a_pair_when_row_zero_churned_and_reverted_since_the_last_
    // confirmation` (this module's own tests, above) already established: a real, genuinely
    // alive `paint-hang` child (never exits on its own), killed on the injected `waiter`'s own
    // FIRST invocation -- which `poll_predicate_with_waiter`'s own loop body only ever reaches
    // AFTER the regular checkpoint has already run for that same iteration, by CONSTRUCTION (the
    // checkpoint and the wait are two ordered statements in the same loop body, not a sleep
    // racing an independently-scheduled timer). `stable_confirmed_at_start` is `None` here,
    // unlike the sibling test's seeded-stale generation -- this call's own FIRST checkpoint is
    // the one that genuinely confirms (the child really is alive, really does hold the line
    // steady), so the death sequenced via the waiter lands strictly AFTER that first
    // confirmation and strictly BEFORE any second one ever could -- the exact "died before the
    // confirming checkpoint" scenario this test exists to pin. Whichever check actually reports
    // it (empirically, per round 9's own finding cited in `checkpoint_confirms_only_while_
    // alive`'s own doc comment, the loop's own earlier, unrelated `Eof` short-circuit reliably
    // wins that race on this system, the same as the sibling test above) is not asserted on
    // directly -- only the OBSERVABLE outcome is: `Crashed`, never folded into `Completed`.
    //
    // found in PR #44 pass 6 S3 (a p6-review2 finding, closing the residual): the paragraph
    // above's own first version claimed "zero wall-clock dependence anywhere in the sequencing"
    // -- an OVERCLAIM at the time. `kill(SIGKILL)` RETURNING only means the signal was
    // DELIVERED, not that the kernel has finished tearing the process down; that teardown is
    // asynchronous and was not itself bounded to complete within one `POLL_INTERVAL`. Under
    // p6-review2's own 3.5x-CPU-oversubscription stress, this reopened a (much narrower, 1-in-
    // 120 vs. the original 2-in-80) residual: if the loop's SECOND checkpoint ran its own
    // `try_wait`-based liveness gate before the kernel had actually confirmed the death, that
    // gate would (correctly, given what it could observe) report "still alive," and the frozen,
    // still-matching screen would complete a pair that should never have completed --
    // `Satisfied`, not `Crashed`. Closed at the injected `waiter` itself (see its own doc
    // comment, below): it now spins on a real, non-consuming `WNOWAIT` kernel confirmation of
    // death BEFORE ever returning, so by the time the loop's own second checkpoint runs,
    // `try_wait` is guaranteed to find a genuine zombie. The claim is now literally true: the
    // ENTIRE sequence -- kill, kernel-confirmed death, second checkpoint -- is ordered by real
    // events this closure itself waits on, not by how much wall-clock time happens to separate
    // them.
    //
    // found in PR #44 pass 7 (P7-B): converted to a scripted transport, the same shape as the
    // sibling test above (its own doc comment has the full derivation of why scripting `try_
    // wait`'s own answer sequence directly reproduces the "died strictly after the first
    // checkpoint, strictly before any second one" scenario this test exists to pin, without
    // needing a real child, a real SIGKILL, or the real kernel-confirmed-death WNOWAIT spin the
    // p6-review2 flake fix (immediately above) needed for a REAL kill's own asynchronous
    // teardown). `stable_confirmed_at_start` is `None` here, unlike the sibling's seeded-stale
    // generation: this call's own first checkpoint is the one that genuinely confirms, so `try_
    // wait`'s `[alive, exited]` sequence lands the exit strictly after that first confirmation
    // and strictly before any second one could ever happen. The real-SIGKILL-to-kernel-confirmed
    // -death mechanics this test used to also incidentally exercise are independently covered in
    // pty.rs (`try_wait_negative_peek_never_consumes_positive_peek_clears_before_reaping`,
    // `try_wait_stays_armed_and_retries_past_an_injected_transient_reap_error`), confirmed before
    // this conversion rather than assumed.
    //
    // fake-pager's own dedicated `paint-stable-then-exit` mode and `STABLE_HOLD_DELAY` constant
    // were already removed (not kept as a dead fixture) when the sibling test above converted.
    #[test]
    fn paint_stable_then_exit_before_second_checkpoint_records_crashed_not_completed() {
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        screen.feed(b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED");
        assert!(
            screen.row_text(0).starts_with("FAKE-PAGER-PAINTED"),
            "row 0: {:?}",
            screen.row_text(0)
        );
        let mut child = ScriptedTransport {
            exit: vec![None, Some(exited_status())],
            ..Default::default()
        };
        let basis = std::time::Instant::now();
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            basis,
            // far enough away that only the scripted death (via `try_wait`'s own sequence) can
            // end this call -- not a real timeout racing anything.
            basis + std::time::Duration::from_secs(2),
            // genuine first-time confirmation, unlike the sibling test's seeded-stale one --
            // THIS call's own first checkpoint is the one that confirms.
            None,
            &mut |_fd, _requested| Ok(crate::pty::PollReadiness::TimedOut),
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Crashed),
            "expected Crashed -- the child died strictly after its own first checkpoint already \
             confirmed the marker while genuinely alive (scripted, not a race against a sleep), \
             so the required second confirmation never happens; whichever check actually catches \
             it (this loop's own Eof-equivalent short-circuit, or the second checkpoint's own \
             liveness gate), the OBSERVABLE outcome must be the same: Crashed, never folded into \
             Completed. Got {outcome:?}"
        );
    }

    // fake-pager flicker-top-line paints, flashes the target line, then permanently diverges
    // to a different one 2ms later -- deliberately shorter than runner.rs's own 10ms poll
    // interval, so the flash cannot survive across two of poll_predicate's checkpoints
    // (guaranteed a full POLL_INTERVAL apart -- see its own comment) regardless of how the
    // reader's polling happens to interleave with these writes. `TopLineStableStartsWith` must
    // never trust that single transient match -- the rejection half completing the pair with
    // the steady-line test just above (perf.sh's own "transient/partial redraw landing between
    // two polls" concern).
    #[test]
    fn stable_top_line_never_satisfies_a_transient_match_that_never_recurs() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-flicker", argv_flicker_top_line);
        let scenario = super::Scenario {
            name: "stable-readiness-on-a-flickering-line",
            keys: Vec::new(),
            readiness: super::Predicate::TopLineStableStartsWith("0000000101".to_string()),
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 1,
            completion_s: 1,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Hung { ceiling_s: 1 });
    }

    // found in PR #44 pass-2 review, corrected pass 3, converged in pass 7 (P7-D, a p6-review2
    // finding): this property used to be end-to-end-proven against two separate REAL pty
    // children -- `large-payload-paint-at-end` (pass 2/3's own repro: ~9600 bytes of filler
    // before row 0, relying on this system's own ~4095-byte pty buffer CAPACITY, empirically
    // confirmed in pass 3, to genuinely block the writer) and `buffer-stall-then-marker` (pass 6
    // S3's own EAGAIN-synchronized version, replacing a timing-inferred "did that write block"
    // with the kernel's own EAGAIN as proof, itself the RENAMED successor of the original
    // large-payload test once that one's own name stopped matching what it proved). Both used
    // `basis == deadline` to force the deadline branch on the very first loop iteration --
    // which, post-pass-7's v3/zero correction (`deadline_drain_budget` no longer takes a fresh
    // FIONREAD for a never-tracked snapshot), computes a budget of exactly 0 for that
    // construction: `drain_available`'s own `if byte_budget == 0 { return WouldBlock }` (its own
    // first line) returns before ever calling `read_available_bounded`. Both real-pty tests had
    // gone silently VACUOUS -- passing (TimedOut) because nothing was ever read, not because a
    // late-arriving marker was correctly declined -- an unwitnessed consequence of v3/zero
    // nobody had traced until this pass. Both retired here, their history kept on this comment
    // rather than silently dropped (`large_payload_paint_at_end`/`buffer_stall_then_marker` and
    // the EAGAIN-signal-file apparatus removed from fake-pager.rs alongside them, confirmed via
    // exhaustive grep to have no other callers), in favor of the construction below: the property
    // they were proving is pure arithmetic internal to `drain_available`'s own shrinking-budget
    // loop, not a real kernel EAGAIN/unblock fact (independently established elsewhere in this
    // file's own comments, not something this test needs to re-verify empirically) -- provable
    // directly and deterministically against a `ScriptedTransport`, the same "policy tests use
    // scripted transport, real-pty tests verify transport primitives" line this pass's own
    // admission-state-machine work already drew.
    //
    // Renamed from `deadline_drain_does_not_accept_a_marker_written_only_after_its_own_reads_
    // unblock_the_writer`: the OLD name promised an end-to-end kernel property (a marker written
    // only after the drain's own reads unblock a REAL stalled writer is declined) this
    // construction no longer proves or needs to -- there is no writer, no unblock, no kernel
    // race here, only `drain_available`'s own request arithmetic. The prefetcher cancellation
    // test (pass 7, P7-C) kept its name through an analogous mechanism swap because the PROPERTY
    // stayed the same; this one's property genuinely narrowed, so the name moves with it, the
    // same "stop asserting something no longer true" standard this file already applied once to
    // this exact scenario's own predecessor (see the retired history just above).
    //
    // The construction, with the ADMITTING-read-vs-FINALIZING-drain-read boundary made explicit
    // (a p6-review2 precision finding: the ADMITTING Data arm sets `last_pre_ceiling_fionread =
    // Some(snapshot)` AND `consumed_since_snapshot = n`, so FINALIZING spends `snapshot -
    // consumed`, never the raw snapshot -- whatever ADMITTING's own read already consumed
    // shrinks what FINALIZING's drain can spend): `queued: [8]` is the snapshot ADMITTING
    // tracks. `read_caps: [3, 2, usize::MAX]` scripts three attributable reads in the exact
    // order they happen, never the reverse. [0] ADMITTING's own SINGLE bounded read always
    // requests the raw snapshot (8) unconditionally -- capped here to 3, a short read, leaving
    // `consumed_since_snapshot == 3`. The scripted clock then crosses the deadline (same
    // `now_calls <= 3` shape as `deadline_recovery_drains_a_real_short_reads_own_remainder`,
    // below -- a `Data` result loops back to the top with no wait in between), so FINALIZING
    // fires with `deadline_drain_budget(Some(8), 3) == 5`. [1] `drain_available`'s own FIRST
    // internal call requests that 5 -- capped here to 2, ANOTHER short read, shrinking its own
    // budget to `5 - 2 == 3`. [2] its SECOND internal call requests that 3 -- left UNCAPPED by
    // the script (`usize::MAX`), so what actually bounds it is `drain_available`'s own request,
    // not the script: returns exactly 3, its budget reaching `3 - 3 == 0`. A fourth call is
    // never attempted (`byte_budget == 0` short-circuits `drain_available` before it). Three
    // reads total, `3 + 2 + 3 == 8`, exactly the original snapshot -- "MARKER", appended in
    // `readable` immediately after those 8 "old" bytes (never itself containing the substring
    // "MARKER", so no short prefix read above can accidentally satisfy the predicate early), is
    // never reached by any of them.
    //
    // RED-verified, both halves the `requested_sizes` assertion exists to distinguish:
    // temporarily stopped `drain_available`'s own budget from shrinking after each read (removed
    // its `byte_budget = byte_budget.saturating_sub(buf.len())` line) -- "MARKER" leaked all the
    // way through, `Satisfied` instead of `TimedOut`. Separately (reverted first), temporarily
    // made `deadline_drain_budget` ignore `consumed_since_snapshot` (returning the raw snapshot
    // unconditionally) -- and the OUTCOME assertion alone did NOT catch it: only 3 of "MARKER"'s
    // own 6 bytes leaked through ("MAR", not the full substring, coincidentally short of
    // satisfying `Contains`), so `TimedOut` still held, for the wrong reason. Only
    // `requested_sizes` caught it -- `[8, 8, 6]`, not `[8, 5, 3]` -- a concrete demonstration,
    // not just a claim, of why this test asserts on individual request sizes rather than the
    // aggregate outcome alone. Both reverted immediately after confirming.
    #[test]
    fn deadline_drain_caps_each_read_to_the_shrinking_remainder() {
        let mut child = ScriptedTransport {
            queued: vec![8],
            readable: b"OLDOLDOLMARKER".iter().copied().collect(),
            read_caps: vec![3, 2, usize::MAX],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("MARKER".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut || {
                now_calls += 1;
                // calls 1-3: next_checkpoint's own init, the first top-of-loop check, and
                // ADMITTING's own snapshot recheck -- all pre-deadline, so ADMITTING completes
                // its one short read and loops back with no wait (a Data result never waits).
                // Everything after (call 4, the second top-of-loop check) reports past the wall,
                // routing straight to FINALIZING.
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut -- \"MARKER\" sits past the 8-byte snapshot's own total and must \
             never be reached by any read this call makes. Got {outcome:?}"
        );
        assert_eq!(
            child.requested_sizes,
            vec![8, 5, 3],
            "expected exactly these three requests, each individually attributable: [0] \
             ADMITTING's own single bounded read (always the raw snapshot, 8, unconditionally); \
             [1] and [2] FINALIZING's own drain_available, its own internal loop requesting the \
             shrinking remainder each time (8 - 3 = 5, then 5 - 2 = 3) -- never the raw 8 again, \
             never more than what remains. Got {:?}",
            child.requested_sizes
        );
    }

    // found in PR #44 round 17 (a codex P2, sweep), reconstructed in pass 7 (P7-D, a p6-review2
    // finding): a fix originally discovered DURING implementation, not anticipated up front --
    // `drain_available`'s own inner loop, if left unbounded, hangs indefinitely against a
    // genuinely relentless writer, caught directly while RED-verifying the checkpoint-cadence fix
    // elsewhere in this file: reverting JUST that fix (leaving this drain unbounded) against a
    // real relentless writer (`continuous-bottom-row-writer`, fake-pager.rs) left a 2-second-
    // ceiling sample running for over a minute before a manual kill. This test used to reproduce
    // that against the real subject with `basis == deadline` -- which, post-pass-7's v3/zero
    // correction, means `deadline_drain_budget` computes a budget of exactly 0 for that
    // construction, so `drain_available` returns before ever attempting a read at all. The test
    // had gone silently vacuous (its own "a handful of DRAIN_BUDGET-capped reads" claim, below,
    // was already false -- zero reads happen) -- it could no longer distinguish a bounded
    // `DRAIN_BUDGET` from an unbounded one, since removing the cap changes nothing when the loop
    // never starts.
    //
    // `DRAIN_BUDGET` itself is NOT redundant post-v3/zero, despite `byte_budget` now always being
    // finite (0, or `snapshot - consumed`) and every `Data` result consuming at least one byte
    // (structurally, for both `ScriptedTransport` -- see `read_available_bounded`'s own early
    // `WouldBlock` return above `Data`'s -- and a real pty's own `read(2)`, which cannot return a
    // successful zero-length result ahead of `EAGAIN`/`Eof`): `byte_budget` bounds `drain_
    // available`'s own loop by BYTES, `DRAIN_BUDGET` bounds it by READ COUNT, and those two
    // diverge under the SAME real, already-documented tty short-read property that motivated
    // `DRAIN_BUDGET` in the first place (see its own doc comment, above) -- a budget drained one
    // byte at a time would otherwise cost up to `byte_budget` syscalls, however large that
    // legitimately is. This is an honest trade-off, not a free one: when `DRAIN_BUDGET` binds
    // first, real, provably-pre-deadline bytes the snapshot already proved queued are left
    // undrained purely because the READ COUNT ran out, not the byte budget -- demonstrated
    // directly below, not merely asserted.
    //
    // Reconstructed on `ScriptedTransport` rather than kept real-pty: forcing a real short read,
    // let alone 64 of them in one drain, is not reliably reproducible against a real child in
    // this environment (this system's own pty buffer capacity finding, cited throughout this
    // file), where a scripted always-short transport makes it deterministic and exact. `read_
    // caps: [1]` needs no new `ScriptedTransport` machinery: a single-element vec already repeats
    // for every call past the first (see `ScriptedTransport`'s own doc comment), so it caps EVERY
    // read -- admitting's own included -- to exactly 1 byte without a 65-element vec spelled out
    // by hand. `queued: [100]`: admitting's own single bounded read consumes 1 (capped), leaving
    // FINALIZING a budget of `100 - 1 == 99`. `drain_available`'s own loop then reads 1 byte at a
    // time, `DRAIN_BUDGET` (64) times -- its own `for _ in 0..DRAIN_BUDGET` cap exits the loop
    // there, with `99 - 64 == 35` bytes STILL remaining in the byte budget, never mind: the
    // iteration count ran out first. "MARKER", placed at byte 65 of `readable` (genuinely inside
    // the original 100-byte snapshot -- proven pre-deadline, unlike every other retired test in
    // this file's own history), is never reached.
    //
    // RED-verified: temporarily raised `DRAIN_BUDGET` to 1000 (comfortably past this test's own
    // 99-byte FINALIZING budget) -- the drain then ran all the way to byte-budget-0 instead of
    // stopping at 64 reads, reaching and feeding "MARKER" to `screen`: `Satisfied(0)` instead of
    // `TimedOut`. Confirms this test genuinely exercises `DRAIN_BUDGET`'s own cap, not merely a
    // construction that happens to time out regardless. Reverted immediately after confirming.
    #[test]
    fn drain_available_stops_at_drain_budget_even_with_real_pre_deadline_bytes_still_owed() {
        let mut child = ScriptedTransport {
            queued: vec![100],
            readable: {
                let mut bytes = vec![b'x'; 65];
                bytes.extend_from_slice(b"MARKER");
                bytes.into_iter().collect()
            },
            read_caps: vec![1],
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("MARKER".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut || {
                now_calls += 1;
                // calls 1-3: next_checkpoint's own init, the first top-of-loop check, and
                // ADMITTING's own snapshot recheck -- all pre-deadline, so ADMITTING completes
                // its one 1-byte read and loops back with no wait (a Data result never waits).
                // Everything after (call 4, the second top-of-loop check) reports past the wall,
                // routing straight to FINALIZING.
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut -- \"MARKER\" sits at byte 65, genuinely within the 100-byte \
             pre-deadline snapshot, but DRAIN_BUDGET's own 64-read cap must stop the drain before \
             ever reaching it. Got {outcome:?}"
        );
        assert_eq!(
            child.read_calls, 65,
            "expected exactly 65 real reads (1 admitting + DRAIN_BUDGET's own 64) -- stopping \
             here, not at byte-budget-0 (which would need 99 reads, one per remaining byte), is \
             what proves DRAIN_BUDGET's own read-count cap fired, not the byte budget. Got {}",
            child.read_calls
        );
    }

    // found in PR #44 pass 6 S1 (the path-3 end-to-end test, closing the loop on finding #2):
    // the relentless-writer test just above only ever exercises the deadline branch's SIMPLEST
    // entry -- `basis == deadline`, already expired before the loop's very first check, so
    // `last_pre_ceiling_fionread` is still its initial `None` and `deadline_drain_budget` returns
    // zero directly, with nothing tracked to spend. The "mid-burst" case finding #2 actually
    // exists for -- the deadline crossing AFTER at least one real normal-path iteration has
    // already tracked a snapshot -- needs the loop to survive past its first check, which real
    // wall-clock timing cannot force deterministically against a genuinely relentless writer (too
    // fast, too close to call reliably the same way on every machine). `now` (this module's
    // newest seam -- `poll_predicate_with_waiter`'s own doc comment, above, has the full
    // derivation) is scripted instead: its first two calls (`next_checkpoint`'s own init, then
    // the loop's FIRST top-of-loop deadline check) report a time comfortably before `deadline`,
    // so the loop runs exactly one real normal-path iteration -- a real, individually-bounded
    // read against the relentless writer that genuinely sets
    // `last_pre_ceiling_fionread`/`consumed_since_snapshot` for real, the same tracking the
    // deadline branch is about to consume. Every call after that reports a time strictly past
    // `deadline`, so the loop's SECOND top-of-loop check fires the deadline branch immediately,
    // with no further normal-path iteration in between to blur which snapshot it inherits.
    //
    // `queued_read_bytes` is scripted to `1` on its own single call (the one scripted
    // normal-path iteration takes), so the bounded read against the scripted `readable` content
    // consumes exactly that one byte, leaving `last_pre_ceiling_fionread == Some(1)`,
    // `consumed_since_snapshot == 1`. `deadline_drain_budget` then computes `1 - 1 == 0` from
    // that tracked snapshot, never calling `queued_read_bytes` again (pass 7 correction, same PR:
    // there is no separate `fresh_fionread` path left to call instead) -- so if the tracked
    // snapshot genuinely survives to FINALIZING, `queued_read_bytes` is called exactly ONCE
    // across the whole run. Reverting the round-16 tracking (falling back to a fresh, unbounded
    // snapshot every time the deadline branch fires -- the pre-pass-6-S1 shape finding #2 closed)
    // would instead call it a SECOND time here. Counting calls (via `ScriptedTransport`'s own
    // built-in `queued_calls`) is therefore a direct, deterministic proxy for "the deadline
    // branch drained against a provably pre-ceiling budget, not a fresh post-ceiling one."
    //
    // found in PR #44 pass 7 (P7-B): converted to a scripted transport -- no real
    // `continuous-bottom-row-writer` child, no real pty, no 50ms settle sleep. `readable` is
    // pre-loaded generously (far more than the one iteration this test's own scripted `now`
    // allows will ever consume) to stand in for a writer that "never lets up"; the scripted
    // clock, not a real relentless writer, is what proves the loop only takes the one iteration
    // this test's own claim depends on.
    #[test]
    fn deadline_branch_uses_the_tracked_snapshot_not_a_fresh_one_mid_burst() {
        let mut child = ScriptedTransport {
            queued: vec![1],
            readable: std::iter::repeat_n(b'x', 4096).collect(),
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_secs(1);
        let mut now_calls: u32 = 0;
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEEDLE-NEVER-APPEARS".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut || {
                now_calls += 1;
                // found in PR #44 pass 7 (P7-B): calls 1-3, not 1-2 -- `next_checkpoint`'s own
                // init, the loop's FIRST top-of-loop deadline check, AND the new post-snapshot
                // recheck invariant 1 adds (see `poll_predicate_with_waiter`'s own doc comment)
                // all need to land comfortably before `deadline` for the one scripted
                // normal-path iteration to complete at all -- a recheck that reported past-wall
                // here would (correctly, per invariant 1) discard the snapshot and route straight
                // to FINALIZING with nothing ever tracked, which is a different scenario than the
                // one this test exists to pin. Every call past that (the loop's SECOND
                // top-of-loop check, and anything beyond) reports strictly past `deadline`.
                if now_calls <= 3 {
                    basis
                } else {
                    deadline + std::time::Duration::from_millis(1)
                }
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        assert_eq!(
            child.queued_calls.get(),
            1,
            "expected exactly one queued_read_bytes call (the single scripted normal-path \
             iteration's own pre-read snapshot) -- a second call would mean the deadline branch \
             fell back to a FRESH snapshot instead of the tracked pre-ceiling one, against a \
             writer that has kept producing the whole time: exactly the busy-burst regression \
             this structural pass exists to keep closed"
        );
    }

    // found in PR #44 pass 6 S1 (a user-review finding, #2), converted to a scripted transport in
    // pass 7 (P7-B): pins the actual fix, deterministically -- the normal path's read must never
    // consume more than an injected (not merely plausible) FIONREAD snapshot claims is queued,
    // even when more is genuinely available to read. `queued` is scripted to always report `1`,
    // independent of `readable`'s own much larger actual length (see `ScriptedTransport`'s own
    // doc comment for why that independence is deliberate) -- so the ONLY way this call can ever
    // find the full "FAKE-PAGER-PAINTED" marker is by accumulating it across MANY small,
    // individually-bounded reads; an unbounded read (the pre-fix shape) would instead find the
    // whole, already-queued marker on its own very first call, needing exactly one
    // `queued_read_bytes` invocation, not many. Counting the transport's own built-in call count
    // is therefore a direct, deterministic proxy for "was the read actually bounded" -- no real
    // process, no real pty, no real timing, the same "count calls to prove a seam was genuinely
    // exercised, not just plausible" technique `drop_reap_loop_calls_the_shared_peek_then_maybe_
    // reap_helper` (pty.rs) already uses for an unrelated seam.
    #[test]
    fn normal_path_read_never_consumes_past_an_injected_pre_read_snapshot() {
        let mut child = ScriptedTransport {
            queued: vec![1],
            readable: b"\x1b[1;1H\x1b[2KFAKE-PAGER-PAINTED"
                .iter()
                .copied()
                .collect(),
            ..Default::default()
        };
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        // far enough away that only genuinely draining the marker one forced-small-read at a
        // time can end this call -- not a real timeout racing anything.
        let deadline = basis + std::time::Duration::from_secs(5);
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("FAKE-PAGER-PAINTED".to_string()),
            basis,
            deadline,
            None,
            &mut crate::pty::wait_for_readable,
            &mut std::time::Instant::now,
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::Satisfied(_)),
            "expected Satisfied (nothing is ever lost, only rationed one byte at a time), got \
             {outcome:?}"
        );
        assert!(
            child.queued_calls.get() > 5,
            "expected the marker to require many small, individually-bounded reads to \
             assemble -- only {} queued_read_bytes calls happened, which an unbounded (pre-fix) \
             read consuming the whole already-queued marker in one call would also produce",
            child.queued_calls.get()
        );
    }

    // found in PR #44 pass 5 (B3, a codex P2), ported in pass 5 commit 3 to the waiter shape:
    // the round-13 test that used to sit here was a SCHEDULER-TIMING oracle by class, not merely
    // by one badly-chosen threshold -- round-13-review's own fix (widening 8ms to 9.5ms) treated
    // the symptom, not the class, and the reviewer said so explicitly. Retired outright, replaced
    // with the test below: no real sleeping or polling, no wall-clock assertion, no elapsed-time
    // budget to tune under contention ever again -- `poll_predicate_with_waiter` (this module's
    // own production code, above) is driven with a RECORDING waiter that logs every requested
    // `(fd, Duration)` (and when it was requested) without ever actually polling, and the
    // assertions are on those recorded Durations directly: what the loop asked for, not how long
    // the OS took to honor it. Commit 3 replaced the underlying mechanism (sleep -> poll(2) wait)
    // but not this contract -- the CLAMP arithmetic feeding the waiter is unchanged, only its
    // consumer is, so this test's own assertions port over unchanged in shape, just against a
    // waiter instead of a sleeper. The `Contains` predicate on a needle `paint-hang` never
    // produces means `poll_predicate_with_waiter` never finds a match and keeps requesting waits
    // every iteration until `deadline` -- exactly the repeated-WouldBlock shape needed to get
    // more than one sample. The recording closure itself sleeps a token 100us AFTER logging each
    // request (standing in for a real wait that never actually becomes readable, matching what a
    // genuinely-not-readable fd would report), purely to bound how many iterations a genuinely
    // non-waiting busy-loop would otherwise spin through before real wall-clock time crosses
    // `deadline` -- irrelevant to what's being asserted (the recorded Durations), just keeping
    // the test's own runtime reasonable.
    //
    // found in PR #44 pass 6 S3 (#7, the user's own item (a) -- "clamp test, real 5ms deadline +
    // ~1ms margin"): CHECKED against S1, not assumed -- this test SURVIVED S1, it was not
    // subsumed or removed by it. S1 added the `now` seam (this function's own 10th parameter)
    // and used it to script a DIFFERENT new test
    // (`deadline_branch_uses_the_tracked_snapshot_not_a_fresh_one_mid_burst`), but did not touch
    // THIS pre-existing test, which kept passing the REAL `std::time::Instant::now` here -- so
    // it still had a real, if narrow, wall-clock dependency: a 1ms "measurement-ordering margin"
    // was needed because `clamped_poll_sleep`'s own internal `Instant::now()` call and this
    // test's OWN separate `Instant::now()` call (inside the recording waiter, a few instructions
    // later) were two INDEPENDENT real-clock reads, close but never quite simultaneous, so
    // comparing `requested` against a remaining-time computed from the LATER read could come out
    // looking like a (microsecond-scale) violation even when the clamp itself was exactly
    // correct. Converted here to actually USE the seam S1 built: the injected `now` closure logs
    // every value it returns, and the waiter looks up the LOG's own last entry (not a fresh,
    // separately-measured `Instant::now()`) -- the exact same value `clamped_poll_sleep`
    // computed `requested` against, not a close-but-different one. The assertion is now exact,
    // no margin, and this can never be a scheduler-timing oracle even in the narrow, principled
    // way the margin already was: `requested` and `remaining` are derived from the identical
    // logged `Instant`, not two real reads racing each other.
    #[test]
    fn would_block_wait_request_never_exceeds_the_clamp_or_poll_interval() {
        // found in PR #44 pass 7 (P7-B): converted to a scripted transport -- no real process, no
        // real pty, no real clock start. `ScriptedTransport::default()` reports nothing ever
        // queued and the subject always alive, so `poll_predicate_with_waiter` routes through the
        // zero-snapshot liveness check every iteration, which calls the waiter below on every
        // pass -- exactly the repeated-wait shape this test needs, without spawning a real
        // `paint-hang` or waiting on it to paint.
        let mut child = ScriptedTransport::default();
        let mut screen = crate::screen::Screen::new(super::SCREEN_ROWS, super::SCREEN_COLS);
        let mut buf = Vec::new();
        let basis = std::time::Instant::now();
        let deadline = basis + std::time::Duration::from_millis(5);
        let mut samples: Vec<(std::time::Duration, std::time::Instant)> = Vec::new();
        // `RefCell`, not a second `&mut` capture: `waiter` and `now` are two SEPARATE injected
        // closures, each independently borrowed by `poll_predicate_with_waiter`, so the log they
        // share needs interior mutability -- they are never actually invoked reentrantly (this
        // loop calls at most one of them at a time, synchronously), so the runtime borrow check
        // `RefCell` performs can never panic here, only enforce at compile time what is already
        // true at runtime.
        let now_log: std::cell::RefCell<Vec<std::time::Instant>> =
            std::cell::RefCell::new(Vec::new());
        let outcome = super::poll_predicate_with_waiter(
            &mut child,
            &mut screen,
            &mut buf,
            &super::Predicate::Contains("NEVER-APPEARS".to_string()),
            basis,
            deadline,
            None,
            &mut |_fd, requested| {
                let exact_now = *now_log
                    .borrow()
                    .last()
                    .expect("clamped_poll_sleep's own now() call always precedes the waiter");
                samples.push((requested, exact_now));
                Ok(crate::pty::PollReadiness::TimedOut)
            },
            &mut || {
                let n = std::time::Instant::now();
                now_log.borrow_mut().push(n);
                n
            },
        )
        .unwrap();
        assert!(
            matches!(outcome, super::PollOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        assert!(
            !samples.is_empty(),
            "expected at least one WouldBlock-triggered wait request"
        );
        for (requested, exact_now) in &samples {
            assert!(
                *requested <= super::POLL_INTERVAL,
                "requested {requested:?}, which exceeds POLL_INTERVAL {:?}",
                super::POLL_INTERVAL
            );
            let remaining = deadline.saturating_duration_since(*exact_now);
            assert!(
                *requested <= remaining,
                "requested {requested:?}, which exceeds the {remaining:?} remaining at the \
                 EXACT instant clamped_poll_sleep computed it (same logged Instant, not a \
                 separate measurement -- no margin should ever be needed here)"
            );
        }
    }

    // found in PR #44 pass 5 commit 3: the event-based counterpart to the recording-waiter test
    // above -- that one proves the requested TIMEOUT never exceeds its bound; this one proves the
    // wait ITSELF is a genuine event wait, not a disguised sleep, by giving it something to wake
    // up FOR. The master's own READ side is what `wait_for_readable` watches, and what makes it
    // readable is the CHILD writing -- not this test writing (`write_keys` sends the opposite
    // direction, master-to-slave, landing on the child's own stdin, not on the fd this function
    // polls). `paint-hang` writes its own marker immediately on starting, so a short, generous
    // margin (well past process-spawn/exec/first-write jitter, the same order of magnitude
    // `write_fixture`-adjacent tests elsewhere in this file already rely on) is enough to
    // guarantee real, already-queued bytes are sitting on the master before this call ever
    // starts -- a real, literal "data already queued" precondition, not a simulated one. Asserts
    // `wait_for_readable` reports `Readable`, not `TimedOut`, well under the requested timeout --
    // if this call were secretly a sleep, it would report `TimedOut` after sitting out the full
    // duration regardless of the data sitting right there. A generous 2-second requested timeout
    // makes the assertion about PROMPTNESS, not about a razor-thin real-time budget: a genuinely
    // broken (sleep-based) implementation would take the full 2s; a genuinely working (poll-based)
    // one returns in microseconds. `started.elapsed()` is checked against a loose 500ms ceiling --
    // not the class of scheduler-timing oracle B3 retired (that class asserted a THRESHOLD close
    // to the real mechanism's own expected latency; 500ms against an expected microsecond-scale
    // return, on a 2-second budget, is instead checking for the qualitative difference between
    // "returned promptly" and "sat out the whole timeout," the same event-vs-non-event
    // distinction `PollReadiness` itself exists to report).
    #[test]
    fn wait_for_readable_reports_data_already_queued_promptly_not_after_the_full_timeout() {
        let argv = argv_paint_hang(std::path::Path::new("unused"));
        let env = fake_pager_env(std::path::Path::new("unused"));
        let child =
            crate::pty::PtyChild::spawn(&argv, &env, (super::SCREEN_ROWS, super::SCREEN_COLS))
                .unwrap();
        // found in PR #44 pass 8 (U-perf): this test's own precondition -- "data already queued"
        // -- used to rest on a fixed sleep guessing paint-hang has written its marker by then; a
        // too-short guess under load would make the assertion below pass for the wrong reason
        // (nothing queued yet, so any return looks "prompt"). Poll `queued_read_bytes` (FIONREAD)
        // instead, bounded so this fails loud if paint-hang never paints at all -- see
        // `zero_snapshot_while_still_alive_reports_timed_out_not_crashed`'s own identical comment.
        // The promptness assertion below (the actual point of this test) is unchanged: it is
        // about `wait_for_readable` itself, not about how the precondition was established.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.queued_read_bytes().unwrap() > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "paint-hang never queued a frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let started = std::time::Instant::now();
        let readiness =
            crate::pty::wait_for_readable(child.raw_master_fd(), std::time::Duration::from_secs(2))
                .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(
            readiness,
            crate::pty::PollReadiness::Readable,
            "expected Readable -- paint-hang's own initial paint must already be queued on the \
             master by now"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "expected a prompt return (data was already queued) -- took {elapsed:?} against a \
             2s requested timeout, which is what a disguised sleep (not a real poll(2) wait) \
             would produce instead"
        );
    }

    // found in PR #44 pass-2 review: `TopLineStableStartsWith`'s own checkpoint used to fire
    // only when the pty had gone globally idle (`status == WouldBlock`) -- a subject writing
    // continuously to any OTHER row (here, row 10, never row 0) kept this loop permanently in
    // the Data-handling path, so the checkpoint -- and therefore stability confirmation -- never
    // happened at all, no matter how long row 0 itself had actually held steady. This also
    // exercises a real, full `run_sample` (not a direct `poll_predicate` call with a pre-baked
    // deadline) specifically because the bug is about NORMAL polling never reaching a
    // checkpoint, not about the deadline-branch recovery findings 2's own test covers.
    #[test]
    fn stable_top_line_settles_despite_continuous_unrelated_row_activity() {
        let (_guard, fixture) = write_fixture();
        let subject = subject(
            "fake-pager-continuous-bottom-row-writer",
            argv_continuous_bottom_row_writer,
        );
        let scenario = super::Scenario {
            name: "stable-readiness-despite-other-row-churn",
            keys: Vec::new(),
            readiness: super::Predicate::TopLineStableStartsWith("FAKE-PAGER-PAINTED".to_string()),
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        // generous relative to POLL_INTERVAL (10ms) and the writer's own 2ms cadence, but still
        // small in absolute terms -- pre-fix this hangs the full ceiling every time (no
        // checkpoint ever fires), post-fix it settles within a couple of checkpoint ticks.
        let ceilings = super::Ceilings {
            readiness_s: 2,
            completion_s: 2,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert!(
            matches!(outcome, super::SampleOutcome::Completed { .. }),
            "expected Completed, got {outcome:?}"
        );
    }

    // the direct negative-case counterpart to the test above: a top line that is genuinely still
    // churning (never holding the same value across two checkpoints) must keep failing to
    // settle, proving that decoupling the checkpoint's own TIMING from the pty going idle does
    // not also weaken the stability CONTENT check itself -- the same plan-6 honesty property
    // `stable_top_line_never_satisfies_a_transient_match_that_never_recurs` already pins for a
    // one-shot divergence, extended here to an unending one. A short ceiling is deliberate and
    // safe: `churning-top-line` never settles on ANY value, so there is no flakiness risk from
    // "maybe it would have matched a moment later."
    #[test]
    fn churning_top_line_never_settles_no_matter_how_often_it_is_checked() {
        let (_guard, fixture) = write_fixture();
        let subject = subject("fake-pager-churning-top-line", argv_churning_top_line);
        let scenario = super::Scenario {
            name: "churning-top-line-never-settles",
            keys: Vec::new(),
            readiness: super::Predicate::TopLineStableStartsWith("CHURN-0000000005".to_string()),
            completion: None,
            timing_basis: super::TimingBasis::Launch,
            prewarm: super::Prewarm::Skip,
            // none of these tests inspect rss_kib -- see the dedicated
            // completed_sample_skips_rss_for_a_non_reporting_scenario/
            // completed_sample_reports_rss_for_a_reporting_scenario tests for that gate (found
            // in PR #44 round 16, a codex P2).
            reports_rss: false,
        };
        let ceilings = super::Ceilings {
            readiness_s: 1,
            completion_s: 1,
        };
        let outcome = super::run_sample(&subject, &scenario, &fixture, &ceilings).unwrap();
        assert_eq!(outcome, super::SampleOutcome::Hung { ceiling_s: 1 });
    }

    fn write_log(contents: &str) -> (crate::test_support::TempFile, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "ress-perf-tracing-precise-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        (crate::test_support::TempFile(path.clone()), path)
    }

    // a live child for the tests below whose own deadline is never reached at all (exec_stamp
    // missing, clock skew, or a line found immediately) -- try_wait is never called on these
    // paths, so any real, spawned PtyChild satisfies the signature; paint-hang is simplest.
    fn any_child() -> (
        crate::pty::PtyChild,
        crate::test_support::TempFile,
        std::path::PathBuf,
    ) {
        let (guard, fixture) = write_fixture();
        let child = crate::pty::PtyChild::spawn(
            &argv_paint_hang(&fixture),
            &fake_pager_env(&fixture),
            (super::SCREEN_ROWS, super::SCREEN_COLS),
        )
        .unwrap();
        (child, guard, fixture)
    }

    // untraced-subject opportunism: no equivalent in the retired script at all (see
    // tracing_precise_ms's own doc comment) -- a subject that never asked for tracing never had
    // an instrument to fail in the first place, so `Ok(None)` unconditionally, never fails the
    // run. Renamed in PR #44 pass 3 from `..._is_no_data_when_the_exec_stamp_is_missing`: that
    // name no longer distinguishes this (untraced, `tracing_requested = false`) case from the
    // NEW `tracing_precise_ms_errs_when_a_traced_subjects_stamp_never_arrives_and_the_child_is_still_alive`
    // case just below, which also has a `None` exec_stamp but a very different outcome.
    #[test]
    fn tracing_precise_ms_is_no_data_for_an_untraced_subject() {
        let (_guard, log_path) = write_log("irrelevant -- never read\n");
        let (mut child, _fixture_guard, _fixture) = any_child();
        let result = super::tracing_precise_ms(
            false,
            None,
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap();
        assert!(matches!(result, super::TracingOutcome::Reading(None)));
    }

    // CORRECTED in PR #44 pass 3: a traced subject (`tracing_requested = true`) whose stamp
    // never arrived used to fold into the same opportunistic `Ok(None)` as the untraced case
    // above -- see tracing_precise_ms's own case-split doc comment for why that grouping missed
    // a real distinction. A confirmed paint (this function's own precondition) with a genuinely
    // missing stamp AND a still-alive child is broken instrumentation, matching the
    // missing-log-line hard-fail's own standard exactly. `any_child`'s own subject never exits on
    // its own (paint-hang-shaped), so this is deterministically the ALIVE case.
    #[test]
    fn tracing_precise_ms_errs_when_a_traced_subjects_stamp_never_arrives_and_the_child_is_still_alive()
     {
        let (_guard, log_path) = write_log("irrelevant -- never read\n");
        let (mut child, _fixture_guard, _fixture) = any_child();
        let err = super::tracing_precise_ms(
            true,
            None,
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("exec stamp missing"), "{err:#}");
    }

    // the DEAD-and-silent counterpart to the ALIVE-and-silent test just above -- same shape as
    // `tracing_precise_ms_reports_subject_died_when_the_child_is_already_dead_at_the_deadline`
    // below, but for the missing-STAMP deadline instead of the missing-LOG-LINE one. A
    // paint-exit-shaped subject (paints, exits immediately) confirmed observably dead BEFORE
    // calling tracing_precise_ms, so there is no ambiguity for the liveness check to resolve --
    // deterministic, not raced.
    #[test]
    fn tracing_precise_ms_reports_subject_died_when_a_traced_subjects_stamp_never_arrives_and_the_child_is_already_dead()
     {
        let (_guard, log_path) = write_log("irrelevant -- never read\n");
        let (_fixture_guard, fixture) = write_fixture();
        let mut child = crate::pty::PtyChild::spawn(
            &argv_paint_exit(&fixture),
            &fake_pager_env(&fixture),
            (super::SCREEN_ROWS, super::SCREEN_COLS),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = super::tracing_precise_ms(
            true,
            None,
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap();
        assert!(
            matches!(result, super::TracingOutcome::SubjectDied),
            "expected SubjectDied, got {result:?}"
        );
    }

    // a confirmed paint (this function's own precondition) with the tracing log line missing
    // within its flush window is a hard harness error, matching the retired script's
    // `wait_for_log_line || die(...)` exactly -- see tracing_precise_ms's own doc comment for
    // the full case-split derivation. found in PR #44 round 11: this is specifically the
    // ALIVE-but-silent case -- paint-hang never exits on its own, so the deadline's own liveness
    // check finds it still running and the hard fail stands unchanged, exactly as pass 2 ruled.
    // The DEAD-and-silent case is the new
    // `tracing_precise_ms_reports_subject_died_when_the_child_is_already_dead_at_the_deadline`
    // test below, not a variant of this one.
    #[test]
    fn tracing_precise_ms_errs_when_the_log_line_never_appears() {
        let (_guard, log_path) = write_log("2026-07-14T07:14:04.905722Z  INFO some other line\n");
        let exec_stamp = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1);
        let (mut child, _fixture_guard, _fixture) = any_child();
        let err = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("tracing log line missing"),
            "{err:#}"
        );
    }

    // same hard-failure treatment for a line that exists but does not parse -- "missing OR
    // unparseable" per the finding's own wording and this function's doc comment. The child's
    // own liveness is irrelevant here (a line WAS found, so death cannot explain its absence --
    // see tracing_precise_ms's own doc comment for why only the missing-line case gets the
    // round-11 liveness gate, not this one).
    #[test]
    fn tracing_precise_ms_errs_when_the_log_line_is_unparseable() {
        let (_guard, log_path) = write_log("not-a-timestamp  INFO perf: first paint\n");
        let exec_stamp = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1);
        let (mut child, _fixture_guard, _fixture) = any_child();
        let err = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unparseable"), "{err:#}");
    }

    // clock-skew opportunism: round 4's own addition, with no script equivalent either (the
    // retired script's unchecked bash arithmetic would have silently reported a nonsensical
    // negative number) -- stays no-data, never a hard failure, see tracing_precise_ms's own doc
    // comment.
    #[test]
    fn tracing_precise_ms_is_no_data_on_a_clock_skew_underflow() {
        let (_guard, log_path) = write_log("2026-07-14T07:14:04.905722Z  INFO perf: first paint\n");
        // the log line's own micros: 1_784_013_244_000_000 + 905_722 -- an exec_stamp AFTER
        // that (by 500ms) makes the subtraction underflow.
        let line_micros = 1_784_013_244_u128 * 1_000_000 + 905_722;
        let exec_stamp = std::time::UNIX_EPOCH
            + std::time::Duration::from_micros((line_micros + 500_000) as u64);
        let (mut child, _fixture_guard, _fixture) = any_child();
        let result = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap();
        assert!(matches!(result, super::TracingOutcome::Reading(None)));
    }

    // the success path, for completeness alongside the three failure/opportunism modes above.
    #[test]
    fn tracing_precise_ms_computes_the_delta_when_everything_lines_up() {
        let (_guard, log_path) = write_log("2026-07-14T07:14:04.905722Z  INFO perf: first paint\n");
        let line_micros = 1_784_013_244_u128 * 1_000_000 + 905_722;
        // exec_stamp 250ms before the log line -- a plausible, ordinary first-paint delta.
        let exec_stamp = std::time::UNIX_EPOCH
            + std::time::Duration::from_micros((line_micros - 250_000) as u64);
        let (mut child, _fixture_guard, _fixture) = any_child();
        let result = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap();
        assert!(matches!(result, super::TracingOutcome::Reading(Some(250))));
    }

    // found in PR #44 round 11: the exact finding. A paint-exit-shaped subject (paints, exits
    // immediately -- fake-pager writes no tracing log line at all, so the line is genuinely,
    // permanently missing regardless of timing) confirmed observably dead BEFORE calling
    // tracing_precise_ms, so the missing-log-line deadline's own liveness check has no
    // ambiguity to resolve -- deterministic, not raced, the same "confirm death first" technique
    // round 9's own deadline-branch test uses. A real spawned process (paint-exit is a genuine,
    // comm-distinguishable subprocess, not a synthetic stand-in), tested directly against
    // tracing_precise_ms with the injectable short timeout rather than through a real
    // `run_sample` call specifically to avoid LOG_FLUSH_TIMEOUT's real 5-second wait (run_sample
    // hardcodes the real constant, with no test-facing override) -- run_sample's own mapping
    // from this outcome to `SampleOutcome::Crashed` is a direct, visibly-correct three-line
    // match at its call site, not additionally exercised here.
    #[test]
    fn tracing_precise_ms_reports_subject_died_when_the_child_is_already_dead_at_the_deadline() {
        let (_guard, log_path) = write_log("2026-07-14T07:14:04.905722Z  INFO some other line\n");
        let exec_stamp = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1);
        let (_fixture_guard, fixture) = write_fixture();
        let mut child = crate::pty::PtyChild::spawn(
            &argv_paint_exit(&fixture),
            &fake_pager_env(&fixture),
            (super::SCREEN_ROWS, super::SCREEN_COLS),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            std::time::Duration::from_millis(50),
            &mut child,
        )
        .unwrap();
        assert!(
            matches!(result, super::TracingOutcome::SubjectDied),
            "expected SubjectDied, got {result:?}"
        );
    }

    // found in PR #44 pass 3: the exact finding. Extends the round-11 test just above (which
    // only pins the OUTCOME, SubjectDied) to also pin the SPEED of reaching it -- the finding
    // being fixed is specifically that liveness used to be checked only once, at the deadline,
    // stalling every affected sample for the FULL `log_flush_timeout`. `super::LOG_FLUSH_TIMEOUT`
    // (the real, production 5s constant) is used here deliberately, in place of the short
    // test-friendly override every other test in this file passes -- a pre-fix, only-check-at-
    // the-deadline implementation would make this test visibly slow (sleeping out the whole 5s
    // before ever discovering the child was dead); post-fix, the per-iteration poll this round
    // adds catches it within about one POLL_INTERVAL of the call starting, regardless of how far
    // away the deadline is. Bounded to half the real timeout, not a tight absolute figure --
    // matching `paint_then_exit_before_keys_records_crashed_not_hung`'s own established
    // reasoning elsewhere in this file: under a fully-loaded `just ci`, wall-clock scheduling
    // latency alone can reach low single-digit seconds even though the detection itself is still
    // immediate, so a fraction of the ceiling keeps this meaningful (fast detection, not timeout
    // exhaustion) without being a flaky proxy for host load.
    #[test]
    fn tracing_precise_ms_reports_subject_died_quickly_even_with_the_real_flush_timeout() {
        let (_guard, log_path) = write_log("2026-07-14T07:14:04.905722Z  INFO some other line\n");
        let exec_stamp = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1);
        let (_fixture_guard, fixture) = write_fixture();
        let mut child = crate::pty::PtyChild::spawn(
            &argv_paint_exit(&fixture),
            &fake_pager_env(&fixture),
            (super::SCREEN_ROWS, super::SCREEN_COLS),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let started = std::time::Instant::now();
        let result = super::tracing_precise_ms(
            true,
            Some(exec_stamp),
            &log_path,
            super::LOG_FLUSH_TIMEOUT,
            &mut child,
        )
        .unwrap();
        assert!(
            matches!(result, super::TracingOutcome::SubjectDied),
            "expected SubjectDied, got {result:?}"
        );
        assert!(
            started.elapsed() < super::LOG_FLUSH_TIMEOUT / 2,
            "tracing_precise_ms took {:?} to report a subject already dead before the call even \
             started -- the per-iteration liveness poll did not catch it promptly",
            started.elapsed()
        );
    }

    // independent oracle values from `date -u -d <ts> +%s`, not derived from the parser under
    // test -- a self-referential check would pass even if both sides shared the same bug.
    #[test]
    fn parses_the_tracing_subscriber_timestamp_format() {
        let micros = super::parse_rfc3339_line_timestamp(
            "2026-07-14T07:14:04.905722Z  INFO perf: first paint",
        )
        .unwrap();
        assert_eq!(micros, 1_784_013_244_u128 * 1_000_000 + 905_722);
    }

    #[test]
    fn parses_a_year_boundary_timestamp() {
        let micros = super::parse_rfc3339_line_timestamp(
            "2026-01-01T00:00:00.000000Z  INFO perf: first paint",
        )
        .unwrap();
        assert_eq!(micros, 1_767_225_600_u128 * 1_000_000);
    }

    #[test]
    fn parses_a_pre_2000_timestamp() {
        let micros = super::parse_rfc3339_line_timestamp(
            "1999-12-31T23:59:59.500000Z  INFO perf: first paint",
        )
        .unwrap();
        assert_eq!(micros, 946_684_799_u128 * 1_000_000 + 500_000);
    }

    // found in PR #44 round 16 (a codex P2): pins the gate ITSELF, in isolation -- a panicking
    // `settle` closure proves it is never invoked at all for a non-reporting scenario, not
    // merely that a real reading happened to go unused, the same "inject a probe, observe
    // whether it runs" technique this file already uses elsewhere (e.g. `deadline_drain_budget`,
    // `checkpoint_confirms_only_while_alive`).
    #[test]
    fn rss_kib_if_reported_never_invokes_settle_for_a_non_reporting_scenario() {
        let result = super::rss_kib_if_reported(false, || {
            panic!("a non-reporting scenario must never invoke the settle closure at all")
        });
        assert_eq!(result, None);
    }

    // the sibling half: a reporting scenario must genuinely invoke `settle` -- exactly once --
    // and return whatever it produces, not some other fixed answer.
    #[test]
    fn rss_kib_if_reported_invokes_settle_exactly_once_for_a_reporting_scenario() {
        let mut calls = 0;
        let result = super::rss_kib_if_reported(true, || {
            calls += 1;
            Some(12_345)
        });
        assert_eq!(result, Some(12_345));
        assert_eq!(
            calls, 1,
            "the settle closure must run exactly once for a reporting scenario"
        );
    }

    // found in PR #44 pass 5 commit 3, RENAMED pass 8 (U-rss, finding 3): the injected `read_one`
    // pins part of `settled_rss_kib`'s own contract deterministically -- a scripted sequence, no
    // real /proc reads or real sleeping needed to prove it. RENAMED from `..._settles_at_the_
    // first_run_of_consecutive_agreeing_readings`: that name and its own former comment described
    // the RETIRED early-return mechanism (stop the instant three readings agree, never read
    // again) -- no longer true post pass-8's sample-full-window redesign, which always samples
    // out to the ceiling (or subject exit) regardless. This script's own trailing 99_999 no
    // longer proves "the function never reads past its own settlement point" (it now does read
    // past it, on purpose) -- what it still proves, unchanged, is that a single LONE reading
    // after an already-confirmed plateau, one that never itself reconfirms three-in-a-row, does
    // not override that plateau. See `settled_rss_kib_reports_the_later_plateau_not_a_mid_ramp_
    // pause`, below, for the test that actually exercises a SECOND, GENUINELY reconfirmed (and
    // higher) plateau overriding the first -- the property this one's old name wrongly implied it
    // already covered.
    #[test]
    fn settled_rss_kib_keeps_its_confirmed_plateau_despite_a_lone_trailing_divergent_reading() {
        let script = [
            Some(16_000u64),
            Some(35_000),
            Some(74_000),
            Some(74_000),
            Some(74_000),
            Some(99_999),
        ];
        let mut iter = script.into_iter();
        let result =
            super::settled_rss_kib(std::time::Duration::from_secs(5), || iter.next().flatten());
        assert_eq!(
            result,
            Some(74_000),
            "a lone trailing reading that never itself reconfirms three-in-a-row must not \
             override an already-confirmed plateau"
        );
    }

    // found in PR #44 pass 8 (U-rss, finding 3): the actual discriminator for the sample-full-
    // window redesign -- unlike the sibling test above (whose script the retired early-return
    // algorithm ALSO happened to answer correctly, just for the wrong reason, since it never
    // read far enough to find out otherwise), THIS script contains a second, genuinely
    // reconfirmed, HIGHER plateau after the first: 35_000 stable for three readings (a subject
    // paused mid-ramp -- e.g. blocked on a slow mount read), then climbing further to 74_000,
    // also stable for three readings. The retired algorithm would have returned 35_000 (it
    // never reads past the first three-in-a-row run at all); this one must return 74_000, the
    // later, higher, genuinely-settled value. RED-verified directly: reverting `settled_rss_kib`
    // to its own pre-redesign early-return shape reproduces exactly the wrong answer, 35_000, on
    // this exact script -- confirming this test discriminates the fix, not just happens to pass
    // under both versions the way the sibling above does.
    #[test]
    fn settled_rss_kib_reports_the_later_plateau_not_a_mid_ramp_pause() {
        let script = [
            Some(16_000u64),
            Some(35_000),
            Some(35_000),
            Some(35_000),
            Some(74_000),
            Some(74_000),
            Some(74_000),
        ];
        let mut iter = script.into_iter();
        let result =
            super::settled_rss_kib(std::time::Duration::from_secs(5), || iter.next().flatten());
        assert_eq!(
            result,
            Some(74_000),
            "a later, higher, genuinely-reconfirmed plateau must win over an earlier mid-ramp \
             pause that also happened to hold steady for three readings"
        );
    }

    // the other half: a reading that never agrees with itself three times in a row must still
    // report SOMETHING (the last reading taken), not None and not hang -- the ceiling parameter
    // (shrunk here to keep this test's own real wall-clock cost small) is what bounds a
    // pathological, never-settling subject's RSS read, exactly as a genuinely relentless writer
    // is bounded elsewhere in this file (`drain_available`'s own `DRAIN_BUDGET`).
    #[test]
    fn settled_rss_kib_reports_the_last_reading_when_it_never_settles_before_the_ceiling() {
        let mut counter = 0u64;
        let result = super::settled_rss_kib(std::time::Duration::from_millis(30), || {
            counter += 1;
            Some(counter) // always increments -- never three-in-a-row, by construction.
        });
        assert!(
            result.is_some(),
            "expected the last reading taken, not None, even though it never settled"
        );
    }

    // opportunistic-miss floor, unchanged from `read_rss_kib` alone: a reading that fails before
    // even one real value ever lands still reports None, exactly as before this function existed.
    #[test]
    fn settled_rss_kib_reports_none_when_the_first_read_already_fails() {
        let result = super::settled_rss_kib(std::time::Duration::from_millis(100), || None);
        assert_eq!(result, None);
    }

    // the deliberate choice named in `settled_rss_kib`'s own doc comment, pinned directly: a
    // subject whose /proc entry vanishes mid-settle (after already producing at least one real
    // reading) reports that LAST GOOD reading, not None -- there is nothing further to settle
    // toward once the process itself is gone, so discarding an already-obtained real number
    // would be strictly less honest than reporting it.
    #[test]
    fn settled_rss_kib_reports_the_last_good_reading_when_the_proc_entry_vanishes_mid_settle() {
        let script: [Option<u64>; 3] = [Some(16_000), Some(35_000), None];
        let mut iter = script.into_iter();
        let result =
            super::settled_rss_kib(std::time::Duration::from_secs(5), || iter.next().flatten());
        assert_eq!(
            result,
            Some(35_000),
            "a subject that exits mid-settle still leaves its last real reading usable"
        );
    }
}
