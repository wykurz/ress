#[test]
fn binary_starts_without_panicking_on_a_regular_file() {
    let path = std::env::temp_dir().join(format!("ress-smoke-{}.txt", std::process::id()));
    std::fs::write(&path, b"hello\nworld\n").unwrap();
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
    // upholds pre_exec's safety contract. The 10s wait below is now just a
    // failsafe against an unrelated hang; the old hazard of a SIGKILL
    // leaving a real terminal stuck in raw mode / the alternate screen is
    // moot once the child never acquires a controlling terminal to corrupt.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            libc::setsid();
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
        std::fs::remove_file(&path).ok();
        panic!("binary did not exit within 10s — probably entered the event loop on a real tty");
    }
    let mut stderr_buf = Vec::new();
    std::io::Read::read_to_end(&mut child.stderr.take().unwrap(), &mut stderr_buf).unwrap();
    std::fs::remove_file(&path).ok();
    let stderr = String::from_utf8_lossy(&stderr_buf);
    // headless raw-mode setup fails gracefully (nonzero exit is fine);
    // a panic means the program was dead before touching the terminal.
    assert!(
        !stderr.contains("panicked"),
        "binary panicked on launch: {stderr}"
    );
}
