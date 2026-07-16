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
    // settles past a couple of the loop's 100ms ticks: a future edit that
    // misplaced the event inside the loop (rather than once, before it)
    // would show up as a second line in this window.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let log_contents = std::fs::read_to_string(&log).unwrap_or_default();
    let occurrences = log_contents
        .lines()
        .filter(|line| line.contains("perf") && line.contains("first paint"))
        .count();
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
