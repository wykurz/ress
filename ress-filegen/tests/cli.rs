#[test]
fn list_presets_names_every_preset() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .arg("--list-presets")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for name in ress_filegen::PRESETS {
        assert!(
            text.lines().any(|l| l == *name),
            "preset {name} missing from --list-presets output"
        );
    }
}
#[test]
fn same_invocation_writes_identical_files_and_reports_stats() {
    let dir = std::env::temp_dir().join(format!("filegen-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.log");
    let b = dir.join("b.log");
    for path in [&a, &b] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
            .args(["--preset", "varied-log", "--kib", "64", "--seed", "42"])
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let err = String::from_utf8(out.stderr).unwrap();
        let last = err.lines().last().unwrap();
        assert!(
            last.starts_with("bytes=") && last.contains(" lines="),
            "stats line: {last}"
        );
    }
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}
#[test]
fn unknown_preset_fails_with_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--preset", "no-such", "/dev/null"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
#[test]
fn zero_size_is_rejected() {
    // the error now originates in ress_filegen::validate() (the library
    // boundary), not a CLI-only check — same invariant, one source of truth.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--preset", "uniform-log", "--kib", "0", "/dev/null"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("target_bytes must be nonzero"),
        "stderr: {err}"
    );
}
#[test]
fn min_greater_than_max_is_rejected_and_leaves_destination_untouched() {
    let dir = std::env::temp_dir().join(format!("filegen-cli-minmax-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sentinel.log");
    std::fs::write(&path, b"sentinel content, must survive").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--min-len", "20", "--max-len", "10", "--kib", "1"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("min < max"), "stderr: {err}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel content, must survive",
        "destination must be untouched on a rejected spec"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
#[test]
fn one_sided_min_len_is_rejected_at_the_argument_level() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--min-len", "10", "--kib", "1", "/dev/null"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
#[test]
fn fixed_len_conflicts_with_min_max_len_at_the_argument_level() {
    // previously silent: fixed_len was checked first in main.rs, so a
    // mixed invocation quietly took the fixed branch instead of erroring.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args([
            "--fixed-len",
            "50",
            "--min-len",
            "10",
            "--max-len",
            "20",
            "--kib",
            "1",
            "/dev/null",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
#[test]
fn mega_every_zero_is_rejected_and_leaves_destination_untouched() {
    let dir = std::env::temp_dir().join(format!("filegen-cli-megazero-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sentinel.log");
    std::fs::write(&path, b"sentinel content, must survive").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--mega-every", "0", "--mega-len", "5", "--kib", "1"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("mega.every must be nonzero"), "stderr: {err}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel content, must survive",
        "destination must be untouched on a rejected spec"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
#[test]
fn size_suffix_overflow_is_rejected_and_leaves_destination_untouched() {
    // the exact reported repro: 2^54 + 1, shifted left by 10 bits (--kib),
    // wraps a u64 down to 1024 with a bare `<<`, silently producing a bogus
    // ~1KiB file instead of erroring. swept across all three suffixes since
    // they share the identical checked_mul pattern.
    for flag in ["--kib", "--mib", "--gib"] {
        let dir = std::env::temp_dir().join(format!(
            "filegen-cli-sizeoverflow-{}-{}",
            flag.trim_start_matches('-'),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sentinel.log");
        std::fs::write(&path, b"sentinel content, must survive").unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
            .args([flag, "18014398509481985"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(!out.status.success(), "flag {flag}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(
            err.contains("overflows a u64 byte count"),
            "flag {flag} stderr: {err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"sentinel content, must survive",
            "flag {flag}: destination must be untouched on an overflowing size"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
#[test]
fn utf8_fraction_out_of_range_is_rejected_and_leaves_destination_untouched() {
    let dir = std::env::temp_dir().join(format!("filegen-cli-utf8frac-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sentinel.log");
    std::fs::write(&path, b"sentinel content, must survive").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ress-filegen"))
        .args(["--utf8-fraction", "2", "--kib", "1"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("utf8_fraction must be within 0.0..=1.0"),
        "stderr: {err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel content, must survive",
        "destination must be untouched on a rejected spec"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
