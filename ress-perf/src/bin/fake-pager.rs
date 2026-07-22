// fake-pager: a dependency-free (std + libc only) stand-in for ress/less, used by ress-perf's
// pty and sample-runner tests. it IS the process under test — no shell involved — so its argv
// and comm stay faithful the way a real subprocess launch would. mode is argv[1]; see main().
//
// Linux-only as a matter of policy, same as ress-perf's own main.rs (see that file's identical
// guard's own doc comment): this file's own libc calls (termios/sigprocmask) are ordinary POSIX
// and might well compile on Darwin, but it exists solely as a test double for pty.rs's
// Linux-only pipe2/ppoll-driven harness, so running it anywhere else was never a supported case.
#[cfg(not(target_os = "linux"))]
compile_error!("fake-pager is Linux-only (a test double for ress-perf's Linux-only pty harness)");
const MARKER: &str = "FAKE-PAGER-PAINTED";

// enters the alternate screen, clears it, homes the cursor, and prints one marker line — the
// same shape a real pager's first paint takes, so screen.rs's row-content tests have something
// realistic to feed a vt emulator.
fn paint() {
    print!("\x1b[?1049h\x1b[2J\x1b[H{MARKER}\r\n");
    std::io::Write::flush(&mut std::io::stdout()).ok();
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn check_tty() -> i32 {
    match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

// found in PR #44 pass 5 (A2's own test): reports which of the three spawn-window signals
// (SIGHUP, SIGINT, SIGTERM) this process sees blocked, as bit flags in its own exit code (bit 0
// = SIGHUP, bit 1 = SIGINT, bit 2 = SIGTERM) -- an exit code, not pty output, since PtyChild's
// own try_wait already gives a test a clean, parseable answer with no screen content to read or
// parse at all, the same "probe via exit code" shape check_tty already established above for a
// single-boolean answer.
fn report_sigmask() -> i32 {
    let mut code = 0;
    unsafe {
        let mut current: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut current);
        if libc::sigismember(&current, libc::SIGHUP) == 1 {
            code |= 1;
        }
        if libc::sigismember(&current, libc::SIGINT) == 1 {
            code |= 2;
        }
        if libc::sigismember(&current, libc::SIGTERM) == 1 {
            code |= 4;
        }
    }
    code
}

// a real pager (less, ress) puts its controlling terminal into raw/cbreak mode before reading
// keys, which is what lets a single un-terminated byte reach it immediately. without this, the
// pty's default canonical line discipline buffers input until a newline, and a lone keystroke
// (no newline) would never become readable — read_one_key would hang forever instead of the
// crash/timeout behavior the sample-runner tests actually want to exercise.
fn read_one_key() {
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut term) == -1 {
            return;
        }
        let original = term;
        libc::cfmakeraw(&mut term);
        libc::tcsetattr(0, libc::TCSANOW, &term);
        let mut one = [0u8; 1];
        std::io::Read::read_exact(&mut std::io::stdin(), &mut one).ok();
        libc::tcsetattr(0, libc::TCSANOW, &original);
    }
}

// a fixed, small delay ahead of the paint -- big enough to dwarf process-spawn/poll-cadence
// jitter (single-digit ms on this harness's own pty poll interval) so a test comparing a
// launch-basis measurement (which includes this delay) against a keypress-basis one (which
// does not, since the keypress lands after readiness) sees an unambiguous, non-flaky gap.
const SLOW_PAINT_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

// the gap between "flicker-top-line"'s one-shot flash and its permanent divergence --
// deliberately SHORTER than the harness's own 10ms poll interval (runner.rs's POLL_INTERVAL),
// so the flashed value cannot survive across two of that poll loop's checkpoints (guaranteed a
// full POLL_INTERVAL apart -- see poll_predicate's own comment) regardless of how the reader's
// polling happens to interleave with these writes.
const FLICKER_STEP_DELAY: std::time::Duration = std::time::Duration::from_millis(2);

// overwrites row 0 in place (home cursor, erase the line, write text) -- a stand-in for a
// pager redrawing its top line, e.g. mid-scroll.
fn write_top_line(text: &str) {
    print!("\x1b[1;1H\x1b[2K{text}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
}

// enters the alternate screen, clears it, then writes a marker to row 5 (never row 0) and hangs
// -- a stand-in for a real pager that paints a status/prompt line before its content row ever
// renders (e.g. less -S on single-line-256m.log: it writes its bottom status line almost
// immediately, then can spend the readiness ceiling and beyond scanning backward for a line
// start that does not exist, so row 0 -- the content row -- never gets anything). pins
// Predicate::Painted's actual contract (perf.sh's `pane_row1`: row 0 specifically) against the
// bug this scenario caught: an "any cell on screen" reading treats this row-5 write as painted.
fn paint_wrong_row_then_hang() -> ! {
    print!("\x1b[?1049h\x1b[2J\x1b[5;1H{MARKER}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    sleep_forever();
}

// found in PR #44 pass-2 review (a stand-in for that pass's own repro: a real 64 KiB
// `read_available` request returning only 4095 bytes, the satisfying marker sitting in a
// remainder still queued behind that chunk -- ~9600+ bytes of filler across rows 2-49, well past
// this pty's own ~4096-byte single-read cap, before row 0 is ever touched), corrected pass 3 (the
// ~4095-byte figure is this pty's own real buffer CAPACITY, empirically confirmed, not merely a
// per-call return cap). Retired in pass 7 (P7-D, a p6-review2 finding), for the identical reason
// `buffer_stall_then_marker` was, just above: its one consuming test had gone silently vacuous
// post-v3/zero, converged onto a `ScriptedTransport` instead -- see
// `deadline_drain_caps_each_read_to_the_shrinking_remainder`'s own doc comment (runner.rs).

// paints row 0 once (the same MARKER every other mode uses, held steady forever after) and then
// writes to row 10 -- never row 0 -- in a genuinely relentless loop (no sleep between writes,
// each one padded to ~140 bytes), standing in for a real pager's own continuously-updating
// status/spinner/byte-count line. Exists to reproduce PR #44 pass-2's finding: a checkpoint
// mechanism gated on the pty going globally IDLE never fires at all here, no matter how long row
// 0 itself has actually been settled, because row 10's own churn keeps the pty producing data
// continuously. A sleep-throttled writer was tried first and did NOT reproduce the bug -- the
// gap between writes gave the reader's own single `read_available` call plenty of chances to
// observe a real `WouldBlock` anyway, letting the OLD idle-gated checkpoint fire in the pauses.
// No sleep at all, with each write sized well past what a single non-blocking read drains
// instantly, keeps the kernel pty buffer genuinely non-empty between the reader's own read
// calls -- confirmed empirically against the reverted (pre-fix) checkpoint logic, not assumed.
fn continuous_bottom_row_writer() -> ! {
    paint();
    let mut counter: u64 = 0;
    loop {
        print!(
            "\x1b[10;1H\x1b[2Kbottom-row-activity:{counter:010}{}",
            "y".repeat(100)
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();
        counter = counter.wrapping_add(1);
    }
}

// row 0 itself never stops changing -- the direct negative-case counterpart to
// `continuous_bottom_row_writer`: proves that decoupling the checkpoint's own TIMING from the
// pty going idle (this round's fix) does not also weaken the stability CONTENT check -- a top
// line that is genuinely still churning must keep failing to settle regardless of how often it
// gets checked, the same plan-6 honesty property `flicker-top-line` already pins for a one-shot
// divergence, extended here to an unending one.
fn churning_top_line() -> ! {
    print!("\x1b[?1049h\x1b[2J\x1b[H");
    let mut counter: u64 = 0;
    loop {
        write_top_line(&format!("CHURN-{counter:010}"));
        counter += 1;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

// found in PR #44 pass 8 (U-perf): `churn_and_revert_top_line` (pass-5-review-followup, finding
// 2) was removed here -- grepped runner.rs and the rest of ress-perf, zero callers of its own
// "churn-and-revert-top-line" mode string remained. Its own consuming test moved on without it
// at some earlier point in this file's history; kept as a fixture nothing exercises would just
// be dead code, the same reasoning already applied to the two retired modes documented below.

// found in PR #44 pass 6 S3 (#7, "remaining oracles" sweep, the user's own item (b) --
// "stable-then-exit relies on a 3ms sleep beating a checkpoint"): this mode -- paint, hold
// STABLE_HOLD_DELAY (3ms), exit -- USED to be audited and kept, reasoned to be safe because the
// consuming test's own detection mechanism is `Eof` (an event), with the 3ms only needing to
// land the death somewhere within a wide margin, not hit a razor-thin target. p6-review2
// REPRODUCED a real flake under adversarial load (3.5x CPU oversubscription: 2/80 runs failed
// "expected Crashed, got Completed"), refuting that audit: `sleep()` only guarantees a LOWER
// bound, and unbounded POST-sleep scheduling delay under contention can let this process's own
// exit slip past the reader's independently-clocked checkpoint regardless of how the 3ms itself
// was chosen -- exactly the "no fixed duration survives enough scheduling delay" class this
// whole pass exists to eliminate, not merely narrow. The consuming test
// (`paint_stable_then_exit_before_second_checkpoint_records_crashed_not_completed`, runner.rs)
// is converted to sequence a REAL `paint-hang` child's death deterministically instead (via the
// injected `waiter` seam, the same shape `stable_checkpoint_declines_a_pair_when_row_zero_
// churned_and_reverted_since_the_last_confirmation` already uses) -- this mode became provably
// unused once that landed (grepped: no other caller), so it is REMOVED here rather than kept as
// a dead fixture nothing exercises.
//
// found in PR #44 pass 3 (finding 2's own repro), corrected pass 5 (B1), hardened pass 6 S3
// (#7): this mode used to fill the pty buffer, block on one guaranteed-blocking write (an EVENT
// oracle -- O_NONBLOCK + EAGAIN, never a timing inference, after an earlier timing-based version
// was found to flake under scheduler preemption -- see git history for the full account), and
// paint a marker only once that write finally unblocked, with an EAGAIN_SIGNAL_PATH file
// signaling the caller's own side of the handshake. Retired in pass 7 (P7-D, a p6-review2
// finding): its one consuming test (runner.rs) had gone silently vacuous post-v3/zero (the
// deadline-branch drain it relied on no longer ever reads anything for that test's own
// construction), and the property it existed to prove turned out to be pure `drain_available`
// arithmetic, not a real kernel EAGAIN fact -- provable directly and deterministically against a
// `ScriptedTransport` instead. See `deadline_drain_caps_each_read_to_the_shrinking_remainder`'s
// own doc comment (runner.rs) for the full derivation.

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let code = match mode.as_str() {
        "check-tty" => check_tty(),
        "report-sigmask" => report_sigmask(),
        "paint-hang" => {
            paint();
            sleep_forever();
        }
        "paint-exit" => {
            paint();
            0
        }
        "never-paint" => sleep_forever(),
        "exit-after-key" => {
            paint();
            read_one_key();
            0
        }
        "slow-paint-exit" => {
            std::thread::sleep(SLOW_PAINT_DELAY);
            paint();
            0
        }
        "flicker-top-line" => {
            paint();
            write_top_line("0000000101");
            std::thread::sleep(FLICKER_STEP_DELAY);
            write_top_line("0000000102");
            sleep_forever();
        }
        "paint-wrong-row-hang" => paint_wrong_row_then_hang(),
        "continuous-bottom-row-writer" => continuous_bottom_row_writer(),
        "churning-top-line" => churning_top_line(),
        other => {
            eprintln!("fake-pager: unknown mode {other:?}");
            2
        }
    };
    std::process::exit(code);
}
