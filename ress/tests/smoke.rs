struct TempFile(std::path::PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}
#[test]
fn binary_starts_without_panicking_on_a_regular_file() {
    let path = std::env::temp_dir().join(format!("ress-smoke-{}.txt", std::process::id()));
    std::fs::write(&path, b"hello\nworld\n").unwrap();
    // drop removes the file even when an assertion or spawn failure unwinds.
    let _guard = TempFile(path.clone());
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ress"));
    command
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // setsid makes the child a session leader with no controlling terminal,
    // so crossterm's direct /dev/tty open fails and the binary takes the
    // graceful error path deterministically — the same path CI already
    // exercises on its tty-less runners — rather than depending on whether
    // this process happens to have a real controlling terminal to inherit.
    // setsid is async-signal-safe and allocates nothing, so calling it here
    // upholds pre_exec's safety contract, and so is reading errno back out
    // via last_os_error() below. the return value is checked because a
    // failed setsid would otherwise leave the child holding the real
    // controlling terminal — the exact hazard this mechanism exists to
    // remove — so a failure is returned as Err, which pre_exec forwards to
    // the parent as the error from Command::spawn() instead of letting the
    // child exec silently into the hazard. the 10s wait below is now just a
    // failsafe against an unrelated hang; the old hazard of a SIGKILL
    // leaving a real terminal stuck in raw mode / the alternate screen is
    // moot once the child never acquires a controlling terminal to corrupt.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let exited =
        wait_timeout::ChildExt::wait_timeout(&mut child, std::time::Duration::from_secs(10))
            .unwrap();
    if exited.is_none() {
        child.kill().ok();
        child.wait().ok();
        panic!("binary did not exit within 10s — probably entered the event loop on a real tty");
    }
    let mut stderr_buf = Vec::new();
    std::io::Read::read_to_end(&mut child.stderr.take().unwrap(), &mut stderr_buf).unwrap();
    let stderr = String::from_utf8_lossy(&stderr_buf);
    // headless raw-mode setup fails gracefully (nonzero exit is fine);
    // a panic means the program was dead before touching the terminal.
    assert!(
        !stderr.contains("panicked"),
        "binary panicked on launch: {stderr}"
    );
}
/// Closes a raw fd on drop. The pty master below (and, transiently, the
/// parent's own copy of the slave) needs the same RAII discipline `TempFile`
/// above gives the fixture/log paths — a leaked fd here would hold the pty's
/// session open past the child's exit.
struct FdGuard(std::os::unix::io::RawFd);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Kills and reaps the wrapped child on every exit path, panics included -- the same RAII
/// discipline `TempFile`/`FdGuard` above already give the fixture/log paths and the pty master
/// fd. `std::process::Child` does not kill on drop, so an explicit guard is needed to end a
/// still-running `ress` when an assertion panics. In `colon_command_typed_before_first_paint_
/// still_jumps`, `KillGuard` is declared after `_master_guard`, so on unwind it drops FIRST
/// (LIFO) -- it is the operative kill in that test today, not merely a fallback: `_master_guard`
/// closing the pty master would ALSO end `ress` there, via `SIGHUP` as the session's
/// controlling-terminal hangup (confirmed: `ress` installs no signal handler anywhere, so the
/// kernel's default disposition applies) -- but that is a side effect of a guard whose stated
/// purpose is fd cleanup, not process lifecycle, and it does not exist at all for a non-pty
/// spawn or a future test built without an `FdGuard`. `Option`, not a bare `Child`: the success
/// path calls `kill_and_wait` explicitly (it must run before `drain.join()` below -- the drain
/// thread's blocked read only unblocks once the child's pty-slave references close, which
/// killing+reaping it is what causes), and `Drop` routes through the same `take`-based logic, so
/// whichever runs first consumes the child and the other becomes a no-op rather than a second
/// kill/wait racing the first.
struct KillGuard(Option<std::process::Child>);
impl KillGuard {
    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}
impl Drop for KillGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}
#[test]
fn first_paint_event_reaches_the_log_file() {
    let fixture = std::env::temp_dir().join(format!("ress-smoke-fp-{}.txt", std::process::id()));
    std::fs::write(&fixture, b"hello\nworld\n").unwrap();
    let _fixture_guard = TempFile(fixture.clone());
    let log = std::env::temp_dir().join(format!("ress-smoke-fp-{}.log", std::process::id()));
    let _log_guard = TempFile(log.clone());
    // a real pty, unlike binary_starts_without_panicking_on_a_regular_file's
    // setsid+pipes above: that mechanism exists specifically to DENY a
    // controlling terminal (so it can prove headless ress fails gracefully),
    // which is the opposite of what first paint needs — enable_raw_mode's
    // /dev/tty open, and so run()'s first draw, only succeed against a real
    // terminal device.
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(
        rc,
        0,
        "openpty failed: {:?}",
        std::io::Error::last_os_error()
    );
    let _master_guard = FdGuard(master);
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ress"));
    command.arg(&fixture).arg("--log-file").arg(&log).arg("-v");
    // this test's own point is exercising -v's contribution to the tracing
    // level (cli.rs's init_logging), not RESS_LOG's — but RESS_LOG, if set
    // in the environment this test happens to run under, wins over -v
    // (init_logging tries it first) and can filter the info-level "first
    // paint" event out entirely, leaving the log empty and this test timing
    // out for a reason that has nothing to do with the binary (reproduced:
    // RESS_LOG=warn set in the calling shell -> 10s timeout, empty log).
    // removing it here isolates the test from whatever the caller happens
    // to have exported, without changing which mechanism (-v) is under test.
    command.env_remove("RESS_LOG");
    // pre_exec does all of the child's stdio wiring itself (setsid, then its
    // own dup2s, then TIOCSCTTY) rather than going through Command's own
    // stdin/stdout/stderr builder methods — that sidesteps any question of
    // which would run first, since there is no second, Rust-driven dup2 left
    // to possibly race against or be overwritten by. setsid/dup2/close/ioctl
    // are all thin, non-allocating syscall wrappers — the same async-signal-
    // safety class as the setsid call in the test above, just a longer list
    // of them.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(slave, 0) == -1
                || libc::dup2(slave, 1) == -1
                || libc::dup2(slave, 2) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            // explicit acquire rather than relying on the open-after-setsid
            // auto-acquire rule, whose timing versus the dup2s above is
            // exactly the kind of ordering this closure exists to not
            // depend on.
            if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // the parent's own copy of the slave must not outlive spawn: held open,
    // it would keep the pty's slave-side refcount above zero independent of
    // the child, masking the child's real exit from the master-side reader
    // below.
    unsafe { libc::close(slave) };
    // drains the pty so the small-but-nonzero stream of repaint bytes (the
    // background indexer's frontier/status convergence redraws — see run()'s
    // tick arm) can never backpressure the child while this test's own
    // thread is busy polling the log file instead of the pty.
    let pty_output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let drain_output = pty_output.clone();
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            drain_output
                .lock()
                .unwrap()
                .extend_from_slice(&buf[..n as usize]);
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut first_seen = false;
    while std::time::Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(&log)
            && contents.contains("perf")
            && contents.contains("first paint")
        {
            first_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // The redraw loop's own tick interval (ress/src/app.rs:978's `tokio::time::interval`),
    // named here so the window below is an explicit multiple of it, not an unexplained
    // literal -- no compile-time link is possible across the process boundary (this test only
    // ever observes the real binary through a log file and pty output), but naming the
    // assumption couples the two for whoever next edits either.
    const ASSUMED_REDRAW_TICK: std::time::Duration = std::time::Duration::from_millis(100);
    // found in PR #44 pass 8 (U-perf): settled-poll across several tick-periods, not a single
    // sleep-then-check. The realistic regression this guards against is the first-paint log
    // line misplaced INSIDE the redraw loop (instead of fired once, before it) -- which would
    // then re-log on every subsequent tick, so a window spanning several ticks lets a climbing
    // occurrence count actually be observed, rather than betting that one point-in-time
    // snapshot after a single fixed wait happens to be representative.
    //
    // This window's own genuine incremental value over the OLD shape (a single fixed
    // `sleep(300ms)` then one check -- p6-review2, pass 8 U-guard review) is narrower than "an
    // every-tick repeat," stated precisely rather than over-credited: 300ms already spans
    // three of this loop's own 100ms ticks, so an event re-logging on EVERY tick was already
    // visible to that one post-sleep read too, old shape or new. What the old shape could
    // genuinely miss is a SLOWER, single, non-repeating re-occurrence landing somewhere after
    // its own fixed 300ms check point but before this window's own close
    // (`ASSUMED_REDRAW_TICK * 5` = 500ms) -- a one-shot read exactly at 300ms cannot see a
    // line that is not written yet; this window, still watching for the rest of its own span,
    // does.
    //
    // The aggregation itself is `max`, not settled_rss_kib's own N-consecutive-agreeing-
    // readings rule (U-rss) -- deliberately simpler, and safe here for a reason RSS does not
    // share: occurrence count is monotonically non-decreasing within one run (nothing ever
    // un-logs a line already written), so the max observed across the window IS the true
    // final count once the window closes. RSS can genuinely fluctuate (a process's own memory
    // usage rises and falls), which is exactly why it needs the stronger plateau rule instead
    // of a bare max -- a transient RSS spike would otherwise be indistinguishable from a
    // genuinely settled reading.
    //
    // Stated honestly, not overclaimed: this is black-box (a real spawned binary, a log file,
    // no test-only hook into the event itself), so it cannot structurally PROVE "fires exactly
    // once, ever" the way an in-process test can -- it is an improvement over a single guessed
    // sleep, not a full elimination of that class. The structural "fires exactly once" proof
    // lives in ress-core's own first-paint tests.
    let settle_deadline = std::time::Instant::now() + ASSUMED_REDRAW_TICK * 5;
    let mut occurrences = 0;
    while std::time::Instant::now() < settle_deadline {
        let contents = std::fs::read_to_string(&log).unwrap_or_default();
        occurrences = occurrences.max(
            contents
                .lines()
                .filter(|line| line.contains("perf") && line.contains("first paint"))
                .count(),
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let log_contents = std::fs::read_to_string(&log).unwrap_or_default();
    child.kill().ok();
    child.wait().ok();
    // with the child reaped, the kernel has already closed its fds (fd
    // cleanup is part of process exit, which completes before a process
    // becomes reapable), so the slave side now has zero references — the
    // drain thread's blocked read returns (EIO is the conventional signal a
    // pty master gets once its last slave reference closes) on its own.
    drain.join().ok();
    let pty_snapshot = String::from_utf8_lossy(&pty_output.lock().unwrap()).into_owned();
    assert!(
        first_seen,
        "log never contained the first-paint event within 10s. log_contents={log_contents:?} pty_output={pty_snapshot:?}"
    );
    assert_eq!(
        occurrences, 1,
        "expected exactly one first-paint line, got {occurrences}. log_contents={log_contents:?}"
    );
}
/// Regression pin for issue #30: a `:N` command typed AND submitted (Enter) entirely before
/// the first paint used to leave the command line stuck composing, uncommitted. The mechanism
/// (see `docs/architecture.md`'s event loop section and the Ctrl+J alias comment in
/// `app.rs::handle_key_command`): Enter's CR byte arrives while the tty is still cooked —
/// before `Term::new`'s `enable_raw_mode` call — where POSIX ICRNL translates it to LF as it's
/// received, before ress ever reads it. That LF survives the later raw-mode transition intact
/// (confirmed empirically, not assumed — see the issue #30 investigation report), but
/// crossterm only decodes a bare LF as Enter when raw mode is NOT active at parse time, which
/// by construction it always is by the time ress's loop parses anything — so it decodes as
/// Ctrl+J instead, which had no binding before this fix. Every byte here (`:`, `3`, `0`, `0`,
/// `0`, Enter) is unaffected by ICRNL except the trailing Enter, which is exactly the case
/// that used to break: the digits alone already worked pre-paint, so this test's whole point is
/// the final byte, not the burst.
#[test]
fn colon_command_typed_before_first_paint_still_jumps() {
    let fixture = std::env::temp_dir().join(format!("ress-smoke-colon-{}.txt", std::process::id()));
    let mut contents = String::new();
    for n in 1..=3500u32 {
        contents.push_str(&format!(
            "{n:010} the quick brown fox jumps over the lazy dog\n"
        ));
    }
    std::fs::write(&fixture, &contents).unwrap();
    let _fixture_guard = TempFile(fixture.clone());
    let log = std::env::temp_dir().join(format!("ress-smoke-colon-{}.log", std::process::id()));
    let _log_guard = TempFile(log.clone());

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(
        rc,
        0,
        "openpty failed: {:?}",
        std::io::Error::last_os_error()
    );
    let _master_guard = FdGuard(master);

    // found in PR #44 pass-2 review: writing the keys AFTER `command.spawn()` returns (this
    // test's own earlier shape) raced the child's own raw-mode flip instead of deterministically
    // avoiding it -- proven, not assumed: with the Ctrl+J fix reverted, adding a mere 10ms delay
    // before the post-spawn write made this test pass FALSELY, because the delay let the child
    // win the race and flip to raw mode FIRST, so the trailing CR arrived natively (raw mode
    // disables ICRNL, so crossterm decodes a bare CR as Enter directly -- the ALREADY-working
    // path every OTHER key in this burst, and every post-paint keystroke in general, already
    // exercises) rather than through the cooked-mode ICRNL-to-LF path this test exists to pin.
    // "written immediately at spawn" was never actually deterministic -- it was a race this
    // process's own few intervening statements USUALLY won, on THIS host, not a guarantee.
    //
    // Written HERE instead, to the master, BEFORE the child process even exists: `openpty`
    // leaves the pty in its own default COOKED-mode termios (ICRNL enabled) until SOME process
    // flips it, and nothing has run yet that COULD flip it -- there is no race left to win or
    // lose. POSIX's own cooked-mode line discipline translates the trailing CR to LF AS IT IS
    // RECEIVED into the discipline's edit buffer, not lazily re-interpreted later against
    // whatever mode happens to be active when a reader eventually reads it (confirmed by this
    // test's own ORIGINAL investigation, cited below: the translated LF "survives the later
    // raw-mode transition intact") -- so the translated byte sits waiting in the pty's own
    // buffer regardless of when the child later starts or how long it takes to reach its own
    // `enable_raw_mode` call.
    let keys = b":3000\r";
    let written = unsafe { libc::write(master, keys.as_ptr() as *const libc::c_void, keys.len()) };
    assert_eq!(written as usize, keys.len(), "short write injecting keys");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ress"));
    command.arg(&fixture).arg("--log-file").arg(&log).arg("-v");
    // see first_paint_event_reaches_the_log_file's identical comment: RESS_LOG, if inherited
    // from the calling shell, can filter "first paint" out and hang this test for a reason
    // that has nothing to do with the binary.
    command.env_remove("RESS_LOG");
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(slave, 0) == -1
                || libc::dup2(slave, 1) == -1
                || libc::dup2(slave, 2) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            Ok(())
        });
    }
    // found in PR #44 pass 6 S5 (codex 353): the first-paint assertion below can panic before
    // any explicit cleanup has run -- `KillGuard`'s comment above has the full mechanism,
    // including why it (not `_master_guard`'s incidental SIGHUP) is the operative kill in this
    // test. Wrapping the child at the point of spawn means every path out of this function,
    // panics included, runs through the same kill+wait.
    let mut child = KillGuard(Some(command.spawn().unwrap()));
    unsafe { libc::close(slave) };

    let pty_output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let drain_output = pty_output.clone();
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            drain_output
                .lock()
                .unwrap()
                .extend_from_slice(&buf[..n as usize]);
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut first_seen = false;
    while std::time::Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(&log)
            && contents.contains("perf")
            && contents.contains("first paint")
        {
            first_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(first_seen, "ress never reached first paint");

    // found in PR #44 pass 6 S5 (codex 356, a scheduler oracle this crate's own AGENTS.md rule
    // forbids): a fixed 500ms sleep used to sit here, gambling that was long enough for the
    // pre-paint `:3000<Enter>` to have navigated and painted before capture stopped -- a loaded
    // host or a slow background index/redraw can make that kill a CORRECT child before the
    // target frame ever paints, then assert against a stale screen. Replaced with a handshake:
    // poll the same `pty_output` buffer the drain thread is already filling, replayed through a
    // vt100 parser (the identical technique the final assertion below already uses), until the
    // target line's own marker actually appears on screen -- an observed fact, not a guessed
    // duration. The deadline below is a diagnostic backstop, not the oracle: a genuinely broken
    // navigation still fails, via the final assertion once the backstop gives up and the child
    // is killed regardless, just with "the loop never saw it" rather than succeeding or failing
    // on unrelated host speed.
    let paint_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut vt = vt100::Parser::new(24, 80, 0);
        vt.process(&pty_output.lock().unwrap());
        if vt.screen().contents().contains("0000003000") {
            break;
        }
        if std::time::Instant::now() >= paint_deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.kill_and_wait();
    drain.join().ok();

    // a raw substring search on the captured bytes is unreliable here: ratatui only writes the
    // cells that actually changed between two similar-looking frames (both are 22 rows of
    // "{line:010} the quick brown fox..." differing in a handful of digits), so a target line
    // number can arrive as several short, cursor-positioned fragments rather than one
    // contiguous run. Replaying the whole byte stream through a vt100 emulator reconstructs
    // the actual final screen contents regardless of how many diffed frames it took to get
    // there -- the same reason ress-perf's own Screen wrapper exists (see ress-perf/src/screen.rs).
    let mut vt = vt100::Parser::new(24, 80, 0);
    vt.process(&pty_output.lock().unwrap());
    let screen_text = vt.screen().contents();
    assert!(
        screen_text.contains("0000003000"),
        "expected the pre-paint `:3000<Enter>` to have navigated to line 3000 -- it should not \
         still be stuck composing the command line. final screen:\n{screen_text}"
    );
}
