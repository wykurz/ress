// ress-perf: pty-driven perf harness for ress vs less, replacing scripts/perf.sh (github #29).
// this file owns the CLI, fixture generation (ress-filegen as a library, not a subprocess --
// see generate_fixture), and the call sequence perf.sh's own main() ran; scenarios.rs owns what
// each scenario/subject/ceiling IS, report.rs owns turning samples into table rows.
//
// this binary never invokes `cargo build` itself (building the release ress binary this harness
// drives is its caller's job, not this one's -- see check_preconditions), so there is no cargo
// invocation HERE for a stray CARGO_TARGET_DIR to redirect -- but that claim doesn't extend to
// where cargo places THIS binary: a relocated CARGO_TARGET_DIR moves ress-perf's own executable
// out from under `<repo>/target`, which broke the naive current_exe()-based repo-root walk this
// file used to rely on exclusively (found by Codex on PR #44's first bot round, reproduced
// directly: `CARGO_TARGET_DIR=/tmp/x cargo run -p ress-perf -- --fixtures-only` wrote fixtures to
// `/tmp/fixtures` instead of the repo). See `resolve_repo_root`'s own doc comment for the fix.
//
// two DIFFERENT things both used to be resolved from repo_root, and only one of them should
// have been: fixtures/ is a source-tree location (repo_root is exactly right for it, and still
// owns it), but the release ress binary is a BUILD ARTIFACT -- repo_root.join("target/release/
// ress") is only correct when nothing has redirected where cargo places build output, which is
// precisely what the justfile's own `cargo build --release -p ress` step (added the same round
// as this file's repo_root fix) does NOT guarantee once CARGO_TARGET_DIR is set: cargo faithfully
// builds ress wherever CARGO_TARGET_DIR points, so a repo-root-relative guess at ress's path goes
// wrong in exactly the cases the round-1 fix made ress buildable in. See `resolve_ress_bin`'s own
// doc comment for the fix (PR #44's second bot round) -- it no longer touches repo_root at all.
// ress-perf is Linux-only: pty.rs calls libc::pipe2/ppoll directly, syscalls with no Darwin
// equivalent. The flake already restricts its own outputs to Linux systems (flake.nix), so this
// guard only matters for a plain `cargo build`/`cargo test` run outside that flake (e.g. a
// contributor on a Mac who never goes through `nix develop`) -- without it, that build fails
// deep inside pty.rs with a cryptic "cannot find function `pipe2` in crate `libc`" instead of a
// clear, immediate reason.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "ress-perf is Linux-only (pty.rs uses pipe2/ppoll, which have no Darwin equivalent)"
);
use clap::Parser;
mod pty;
mod report;
mod runner;
mod scenarios;
mod screen;
#[cfg(test)]
mod test_support;

#[derive(Parser, Debug)]
#[command(about = "pty-driven perf harness for ress vs less")]
struct Cli {
    /// use smaller fixtures and fewer runs per scenario
    #[arg(long)]
    quick: bool,
    /// materialize the standard fixture set only, without running any scenario
    #[arg(long)]
    fixtures_only: bool,
}

fn main() {
    // before anything else -- see install_signal_cleanup_handler's own doc comment. Must be in
    // place before the first TempFileGuard::new call, which is the earliest point a temp path
    // could need signal-safe cleanup.
    install_signal_cleanup_handler();
    if let Err(err) = run() {
        eprintln!("ress-perf: error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let repo_root = resolve_repo_root()?;
    let fixtures_dir = repo_root.join("fixtures");
    let ress_perf_mount = resolve_mount_override(std::env::var_os("RESS_PERF_MOUNT"));
    if let Some(mount) = &ress_perf_mount {
        anyhow::ensure!(
            mount.is_dir(),
            "RESS_PERF_MOUNT is not a directory: {mount:?}"
        );
    }
    // a dedicated subdirectory, not the mount's root: a fixture-shaped name colliding with a
    // real file the mount's owner put there is exactly the partial-pair abort in
    // generate_fixture exists to never paper over by guessing.
    let mount_fixtures_dir = ress_perf_mount
        .as_ref()
        .map(|mount| mount.join("ress-perf-fixtures"));
    let runs_override = resolve_runs_override(std::env::var_os("RESS_PERF_RUNS"));
    let (runs, varied_log_name, varied_log_bytes) =
        determine_runs(cli.quick, runs_override.as_deref())?;
    // the pager preflight (Linux gate, less on PATH, the release ress binary) and the
    // mount-fixture check are only needed once a scenario is about to launch a pager --
    // --fixtures-only never does, so it must not refuse to run just because less happens to be
    // uninstalled. The mount-fixture check runs here, before ensure_fixtures, so an unprepped
    // mount aborts in seconds, not after minutes of local-scenario samples print_results would
    // then never get a chance to report.
    let (ress_bin, less_bin) = if cli.fixtures_only {
        (None, None)
    } else {
        let (ress_bin, less_bin) = check_preconditions()?;
        check_mount_fixtures_ready(mount_fixtures_dir.as_deref())?;
        (Some(ress_bin), Some(less_bin))
    };
    ensure_fixtures(
        &fixtures_dir,
        varied_log_name,
        varied_log_bytes,
        mount_fixtures_dir.as_deref(),
        cli.fixtures_only,
    )?;
    if cli.fixtures_only {
        eprintln!("fixtures ready; --fixtures-only set, exiting.");
        return Ok(());
    }
    crate::scenarios::init_binaries(
        ress_bin.expect("resolved above whenever !fixtures_only"),
        less_bin.expect("resolved above whenever !fixtures_only"),
    );
    let mut rows = Vec::new();
    let mut rss_rows = Vec::new();
    let varied_log = fixtures_dir.join(varied_log_name);
    let varied_log_stats = crate::scenarios::stats_sidecar_path(&varied_log);
    crate::scenarios::run_open(&varied_log, varied_log_name, runs, &mut rows)?;
    crate::scenarios::run_jump_end(
        &varied_log,
        varied_log_name,
        &varied_log_stats,
        runs,
        &mut rows,
        &mut rss_rows,
    )?;
    crate::scenarios::run_deep_goto(
        &varied_log,
        varied_log_name,
        &varied_log_stats,
        runs,
        &mut rows,
    )?;
    crate::scenarios::run_scroll_cadence(&varied_log, varied_log_name, runs, &mut rows)?;
    let megalines = fixtures_dir.join("megalines-512m.log");
    let megalines_stats = crate::scenarios::stats_sidecar_path(&megalines);
    crate::scenarios::run_open(&megalines, "megalines-512m.log", runs, &mut rows)?;
    crate::scenarios::run_jump_end(
        &megalines,
        "megalines-512m.log",
        &megalines_stats,
        runs,
        &mut rows,
        &mut rss_rows,
    )?;
    crate::scenarios::run_deep_goto(
        &megalines,
        "megalines-512m.log",
        &megalines_stats,
        runs,
        &mut rows,
    )?;
    crate::scenarios::run_scroll_cadence(&megalines, "megalines-512m.log", runs, &mut rows)?;
    // single-line is exactly one line: G, deep-goto and scroll-cadence have nothing to jump or
    // scroll to, so only open->first-paint applies.
    let single_line = fixtures_dir.join("single-line-256m.log");
    crate::scenarios::run_open(&single_line, "single-line-256m.log", runs, &mut rows)?;
    if let Some(mount_fixtures_dir) = &mount_fixtures_dir {
        let label = "varied-log-256m.log";
        let open_fixture = crate::scenarios::mount_fixture_path(mount_fixtures_dir, "open");
        crate::scenarios::run_mount_open(&open_fixture, label, runs, &mut rows)?;
        let jump_end_fixture = crate::scenarios::mount_fixture_path(mount_fixtures_dir, "jump-end");
        let jump_end_stats = crate::scenarios::stats_sidecar_path(&jump_end_fixture);
        crate::scenarios::run_mount_jump_end(
            &jump_end_fixture,
            label,
            &jump_end_stats,
            runs,
            &mut rows,
            &mut rss_rows,
        )?;
    }
    print_results(&fixtures_dir, &rows, &rss_rows)
}

// resolved in order of preference -- three tiers, each a fallback for the one before it.
// Assumption this whole function enforces: ress-perf is a dev tool that only ever runs either
// via `cargo run`/`cargo test` from within this workspace, or from somewhere under this repo's
// own tree -- it does not defend against being launched from inside a DIFFERENT, unrelated cargo
// workspace (tier 2 would misidentify that workspace's own root as this one). That's an accepted
// scope limit for a tool that only ever ships as a workspace dev-dependency, never installed or
// run standalone.
//
// 1. `CARGO_MANIFEST_DIR` (a genuine RUNTIME environment variable `cargo run`/`cargo test` set
//    for the spawned process -- verified directly, not assumed: printed it from a throwaway bin
//    under `cargo run` and confirmed it's `Ok("<repo>/ress-perf")`, distinct from the
//    compile-time `env!()` macro this file's previous version rejected for being "wrong the
//    moment the binary runs from anywhere other than where it happened to be compiled" -- that
//    concern applies to `env!()`, which bakes the value in at BUILD time, not to reading the
//    same-named variable at RUNTIME from the process's own environment). Its parent is the
//    workspace root, since this crate lives one level below it by construction (`<repo>/ress-perf`,
//    alongside `ress/`, `ress-core/`, `ress-filegen/`) -- and, being about SOURCE location, this
//    tier is entirely unaffected by where cargo happens to place build artifacts, which is
//    exactly the CARGO_TARGET_DIR-relocation bug this fixes.
// 2. Failing that (the binary was invoked directly, not via `cargo run`/`cargo test`, so cargo
//    never set the variable): walk UP from the process's own current directory looking for the
//    nearest ancestor whose `Cargo.toml` contains a `[workspace]` table. Depends only on where
//    the process was launched FROM, never on where cargo placed build artifacts -- equally
//    CARGO_TARGET_DIR-relocation-proof as tier 1, for a different reason.
// 3. Failing THAT (the binary was copied elsewhere and launched from a directory outside the
//    repo entirely -- a scratch-copy checkout), fall back to the
//    original current_exe()-based parent-walk, which assumes the un-relocated
//    `target/<profile>/ress-perf` layout a plain, unredirected `cargo build` produces.
fn resolve_repo_root() -> anyhow::Result<std::path::PathBuf> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(std::path::PathBuf::from);
    if let Some(root) = repo_root_from_manifest_dir(manifest_dir.as_deref()) {
        return Ok(root);
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(root) = repo_root_from_cwd_walk(&cwd)?
    {
        return Ok(root);
    }
    repo_root_from_exe_walk()
}

fn repo_root_from_manifest_dir(
    manifest_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    manifest_dir?.parent().map(std::path::Path::to_path_buf)
}

fn repo_root_from_cwd_walk(start: &std::path::Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut dir = start;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() && is_workspace_root(&candidate)? {
            return Ok(Some(dir.to_path_buf()));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(None),
        }
    }
}

// a plain line-match, not a TOML parser -- this crate has no `toml` dependency, and a table
// header always sits alone on its own line, so a trimmed exact match on `[workspace]` correctly
// distinguishes the table itself from `[workspace.dependencies]`/`[workspace.package]` (a naive
// substring check would not) without needing one.
fn is_workspace_root(cargo_toml: &std::path::Path) -> anyhow::Result<bool> {
    let contents = std::fs::read_to_string(cargo_toml)
        .map_err(|err| anyhow::Error::new(err).context(format!("reading {cargo_toml:?}")))?;
    Ok(contents.lines().any(|line| line.trim() == "[workspace]"))
}

fn repo_root_from_exe_walk() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe().map_err(|err| {
        anyhow::Error::new(err).context("resolving the running binary's own path")
    })?;
    repo_root_from_exe_path(&exe)
}

fn repo_root_from_exe_path(exe: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    exe.parent() // target/<profile>
        .and_then(std::path::Path::parent) // target
        .and_then(std::path::Path::parent) // repo root
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("expected {exe:?} to live under target/<profile>/ress-perf"))
}

fn which(name: &str) -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("PATH is not set"))?;
    which_in(name, &path)
}

// `path` threaded in explicitly (rather than this function reading the `PATH` env var itself) so
// it stays a pure function of its inputs -- `which` is the one and only place that reads the
// actual environment variable. Mirrors `determine_runs`'s own reasoning: `PATH` is genuine
// process-wide state, and mutating it from a test would race every OTHER test in the same
// `cargo test` process under thread-parallel mode (the exact class of hazard this crate's own
// pty.rs/screen.rs tests were already bitten by twice) -- threading it as a parameter instead
// means a test can hand in a scratch PATH directly, with no shared state touched at all.
fn which_in(name: &str, path: &std::ffi::OsStr) -> anyhow::Result<std::path::PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file() && is_executable(candidate))
        .ok_or_else(|| anyhow::anyhow!("{name} not found (or not executable) on PATH"))
}

// found in PR #44's round-5 review: `which`'s own preflight used to accept ANY regular file named
// `less`, executable bit or not -- a decoy sitting earlier on PATH than the real binary (or a
// `less` that lost its exec bit some other way) passed preflight cleanly, then failed every
// single sample at spawn time as a harness error, deep into what could be minutes of an otherwise
// successful run.
//
// `libc::access(path, X_OK)`, not a metadata mode-bit check
// (`permissions().mode() & 0o111 != 0`): the kernel is the authority on whether THIS process
// could actually `execve()` a given path, not a userspace reimplementation of its permission
// algorithm. A mode-bit check would report a false positive for a file executable only by a
// DIFFERENT uid/gid than the one ress-perf is running as (the mode bits alone don't say WHICH of
// owner/group/other applies to this process) and would miss a `noexec` mount entirely -- neither
// of which `access()` gets wrong, since it asks the kernel directly. A TOCTOU gap between this
// check and the eventual `execve()` is accepted, not newly introduced: `ress_bin.is_file()` below
// already carries the identical race for the identical reason -- a preflight's job is catching
// the common case (a decoy or misconfigured binary present from the start of the run) early, not
// defending against adversarial concurrent modification mid-run.
fn is_executable(path: &std::path::Path) -> bool {
    let Ok(c_path) =
        std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))
    else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

// resolved once, here, on the client side, using this process's own PATH -- not left as the
// bare word "less" for Subject::argv_for to embed: PtyChild::spawn's env_clear() leaves a
// spawned child no PATH to search at all, so a bare name would fail outright rather than
// resolve against anything (perf.sh's own LESS_BIN resolves for a related but distinct reason:
// tmux's client-vs-server environment handling; this substrate has no such split, but "resolve
// once, absolute, here" is the same discipline either way).
fn check_preconditions() -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    // assert_pane_is/RSS reads (runner.rs) and this harness's own /proc-based checks are
    // Linux-only by spec, with no fallback for any other kernel -- failing loudly here is the
    // whole gate, not a stand-in for one.
    anyhow::ensure!(
        std::env::consts::OS == "linux",
        "this harness only runs on Linux (it reads /proc/<pid>/status directly; no fallback exists for {})",
        std::env::consts::OS
    );
    let less_bin = which("less")?;
    let ress_bin = resolve_ress_bin()?;
    // same executable-bit gap `which`/`is_executable`'s own doc comment names, applied to the
    // sibling `ress` binary this same preflight resolves two lines above -- a `ress` that exists
    // but somehow lost its exec bit (a botched build artifact, a filesystem that drops
    // permissions on copy) would otherwise pass this check and fail identically late, at the
    // first sample's own spawn.
    anyhow::ensure!(
        ress_bin.is_file() && is_executable(&ress_bin),
        "ress binary missing or not executable beside ress-perf: {ress_bin:?} (`just perf`/`just \
         fixtures` build it automatically via `cargo build --release -p ress`, immediately before \
         ress-perf's own `cargo run --release`, so the two land in the same profile directory \
         together -- a bare `cargo run -p ress-perf --release --` needs that same release build \
         run first by hand; a non-release ress-perf invocation needs a matching non-release ress \
         built alongside it instead, which no documented flow currently does)"
    );
    Ok((ress_bin, less_bin))
}

// ress is resolved as a SIBLING of the running ress-perf binary, not via repo_root -- ress-perf
// and ress are always built by the same cargo invocation shape into the same profile directory
// (see check_preconditions' own error text: `just perf`'s `cargo build --release -p ress`
// immediately precedes ress-perf's own `cargo run --release`), whatever CARGO_TARGET_DIR says,
// if anything -- so "beside me" is correct by construction for every invocation shape: the
// default target dir, a relocated one, or a config-file target-dir override. This is the exact
// insight `test_support::sibling_bin_path` already uses for test binaries (one extra `.parent()`
// there, to skip past a test harness's own `target/<profile>/deps/` nesting -- ress-perf's own
// `main()` binary sits directly in `target/<profile>/`, no `deps/` to skip). repo_root-relative
// resolution (`repo_root.join("target/release/ress")`) was exactly PR #44's second bot-review
// bug: the justfile fix that makes `ress` respect CARGO_TARGET_DIR (so it can be found there AT
// ALL) makes a repo-root-relative guess wrong in precisely that case, not right -- fixtures/
// still needs repo_root (a source-tree location, not a build artifact), but the binary doesn't,
// and no longer reads it.
fn resolve_ress_bin() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe().map_err(|err| {
        anyhow::Error::new(err).context("resolving the running binary's own path")
    })?;
    ress_bin_from_exe_path(&exe)
}

fn ress_bin_from_exe_path(exe: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    exe.parent()
        .map(|profile_dir| profile_dir.join("ress"))
        .ok_or_else(|| anyhow::anyhow!("expected {exe:?} to have a parent directory"))
}

// asserts every mount fixture the mount leg will need already exists -- called early (right
// after check_preconditions, before ensure_fixtures, before any scenario times anything), not
// from inside the mount leg itself: for a non-quick run that is minutes of real samples,
// discarded the instant this died, since print_results is the very last statement in run(). An
// unprepped mount must instead abort in seconds. Safe to call unconditionally: returns
// immediately when there is no mount to check, and is never reached at all under
// --fixtures-only (whose whole job, when a mount IS set, is to CREATE these exact fixtures, so
// asserting they already exist would be asserting against the one case they are expected to be
// absent).
fn check_mount_fixtures_ready(mount_fixtures_dir: Option<&std::path::Path>) -> anyhow::Result<()> {
    let Some(mount_fixtures_dir) = mount_fixtures_dir else {
        return Ok(());
    };
    let mut missing = Vec::new();
    for scenario in crate::scenarios::MOUNT_FIRST_SCENARIOS {
        let fixture = crate::scenarios::mount_fixture_path(mount_fixtures_dir, scenario);
        let stats = crate::scenarios::stats_sidecar_path(&fixture);
        if !(fixture.is_file() && stats.is_file()) {
            missing.push(fixture);
        }
    }
    anyhow::ensure!(
        missing.is_empty(),
        "mount fixture(s) missing: {missing:?} -- generate first: RESS_PERF_MOUNT=<dir> just fixtures \
         (a run that both generates and times a fixture would measure its own generation-warmth, \
         not a fresh first read)"
    );
    Ok(())
}

// found in PR #44 pass-2 review: perf.sh's own override gates were `[[ -n "${RESS_PERF_MOUNT:-}"
// ]]` and `[[ -n "${RESS_PERF_RUNS:-}" ]]` (see scripts/perf.sh, retired at 66820c6). Bash's `-n`
// is false for BOTH an unset variable and one set to the empty string, so `RESS_PERF_MOUNT=` /
// `RESS_PERF_RUNS=` in the calling environment (a stray blank CI variable, `VAR= just perf`,
// etc.) were silently "no override" -- identical to leaving the variable unset entirely. Neither
// `std::env::var` nor `var_os` make that distinction on their own: a present-but-empty variable
// reads back as `Some("")` / `Some(OsString::from(""))`, a value indistinguishable in kind from
// an actually-set one -- wrapping that raw `Option` straight into a `PathBuf` or straight into
// `determine_runs` treats an accidentally-empty override as a REQUEST instead of as nothing:
// `RESS_PERF_MOUNT=""` then fails validation ("is not a directory: \"\""), `RESS_PERF_RUNS=""`
// fails parsing ("must be a positive integer, got: "), where perf.sh used its own default in
// both cases. One seam closes it for both overrides: collapse the empty case to `None` right
// where each is resolved, before either reaches its own downstream validation. Both are pure
// functions over an explicit `Option<OsString>` parameter -- never reading the environment
// directly themselves, so each is testable against a synthetic input rather than only against a
// real, mutate-the-whole-process env var -- the same shape `determine_runs` just below already
// uses `runs_override` for, and for the same reason.
fn resolve_mount_override(raw: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    raw.filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

// RESS_PERF_RUNS must additionally be valid UTF-8 to ever parse as an integer. The prior
// `std::env::var(key).ok()` already collapsed a non-UTF-8 value to `None` silently (`Result`'s
// `VarError::NotUnicode` swallowed by `.ok()`); `OsString::into_string`'s own `Result::ok()`
// preserves that exact behavior unchanged here -- this fix touches only the empty case, not the
// pre-existing non-UTF-8 one.
fn resolve_runs_override(raw: Option<std::ffi::OsString>) -> Option<String> {
    raw.filter(|value| !value.is_empty())
        .and_then(|value| value.into_string().ok())
}

// `runs_override` is threaded in (rather than this function reading RESS_PERF_RUNS itself) so
// it stays a pure function of its inputs -- run() is the one and only place that reads the
// actual environment variable.
fn determine_runs(
    quick: bool,
    runs_override: Option<&str>,
) -> anyhow::Result<(u32, &'static str, u64)> {
    let (mut runs, varied_log_name, varied_log_bytes) = if quick {
        (2, "varied-log-256m.log", 256u64 << 20)
    } else {
        // even, not odd: the pager-order alternation in every scenario flips by sample parity,
        // so an odd RUNS gives one pager the first (residual-warmth-free) position one more
        // time than the other -- the exact bias alternation exists to cancel. An even count
        // keeps the two positions balanced N/2 and N/2.
        (6, "varied-log-2g.log", 2u64 << 30)
    };
    if let Some(s) = runs_override {
        runs = s.parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| {
            anyhow::anyhow!("RESS_PERF_RUNS must be a positive integer, got: {s}")
        })?;
    }
    // not a rejection: an odd override is the caller's informed call (there may be a real
    // reason, e.g. matching a previous run's sample count), but the imbalance above is real and
    // silent otherwise.
    if runs % 2 != 0 {
        eprintln!(
            "ress-perf: warning: RUNS={runs} is odd -- pager-order alternation cannot balance an \
             odd sample count, leaving a one-sample first-position imbalance (see docs/perf.md)"
        );
    }
    Ok((runs, varied_log_name, varied_log_bytes))
}

// --- signal-safe cleanup for in-flight fixture temp files AND the active subject -------------
//
// SIGINT/SIGTERM/SIGHUP bypass Drop entirely: the default disposition for all three is to
// terminate the process immediately, with no unwinding and therefore no destructors run -- Drop
// is a language-level mechanism, invisible to a signal that never lets the language runtime take
// another step. TempFileGuard's own Drop (just below) only ever fires on a normal return or a
// panic-driven unwind; an interrupted `just fixtures`/`just perf` (Ctrl-C mid-write, a CI job's
// own SIGTERM on timeout, or a closed terminal/dropped SSH session's own SIGHUP) needs a cleanup
// path that does not go through Drop at all -- the retired bash harness had exactly this
// cleanup, via an EXIT trap; this is the equivalent.
//
// Found incomplete in PR #44's round-4 review: this handler originally only unlinked temp
// files, never killed the actual subject (ress/less) -- which `setsid` put in its OWN session,
// so the default SIGINT/SIGTERM delivered to THIS process's own process group never reaches it.
// An interrupted run used to leave the subject alive, possibly wedged on the exact hung/dead
// mount this harness exists to measure. See `crate::pty::ACTIVE_CHILD_PGID`'s own doc comment
// for how the handler now finds it, and `handle_termination_signal`'s for the ordering.
//
// Found STILL incomplete in PR #44 pass-2 review: SIGHUP was not in the handled set at all until
// this round -- a closed terminal or a dropped SSH session delivers SIGHUP, not SIGINT/SIGTERM,
// so that entire class of interruption used to bypass this cleanup path -- subject and temp
// files both -- no matter how carefully `install_signal_cleanup_handler` covered the other two.
// The same round also closed a narrower, earlier window on the SPAWN side: `pty.rs`'s own
// `spawn_window_blocked` now blocks all three of these signals around the gap between a subject
// actually coming into existence (`command.spawn()` returning) and `ACTIVE_CHILD_PGID` being
// registered -- see its own doc comment for the full derivation of why a registration-window
// signal is exactly as dangerous as one landing during `PtyChild::Drop`'s own teardown window
// (round 6's finding), just at the opposite end of a subject's lifecycle.
//
// This registry is deliberately not a `Vec`/`HashMap` and does not use a `Mutex`: a signal
// handler can interrupt this process at ANY instruction, including one already holding a lock
// the handler would then need itself (self-deadlock) or one mid-allocation (the allocator's
// own internal locks are not signal-safe either) -- POSIX's signal-safety(7) list is the hard
// constraint, and it does not include malloc, free, or pthread_mutex_lock. Two fixed slots (one
// `generate_fixture` call registers at most two paths -- its data file and its stats sidecar --
// and this binary never generates fixtures concurrently, so two is exactly enough, not a
// defensive round number), each an `AtomicPtr` the handler reads with one atomic load and no
// lock. Registration itself (ordinary, non-signal-handler code, free to allocate) converts the
// path to a `CString` and `Box::leak`s it for a `'static` pointer -- a small, bounded leak (at
// most a handful of short-lived allocations across one whole program run, for a CLI process
// that exits shortly after anyway) traded for never needing to free memory a signal handler
// might be mid-read of.
// shared by `register_temp_path` and `register_fake_home` below, which both need the identical
// "hand a signal handler a pointer it can read without ever needing to free it" shape: converts
// `path` to a `CString` (ordinary, non-signal-handler code, free to allocate) and `Box::leak`s
// it for a `'static` pointer -- a small, bounded leak (at most a handful of short-lived
// allocations across one whole program run, for a CLI process that exits shortly after anyway)
// traded for never needing to free memory a signal handler might be mid-read of.
fn leak_path_as_cstring(path: &std::path::Path) -> anyhow::Result<*mut libc::c_char> {
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let c_path = std::ffi::CString::new(bytes).map_err(|err| {
        anyhow::Error::new(err).context(format!("path {path:?} contains a NUL byte"))
    })?;
    Ok(Box::leak(Box::new(c_path)).as_ptr().cast_mut())
}

const MAX_REGISTERED_TEMP_PATHS: usize = 2;
static TEMP_PATH_SLOTS: [std::sync::atomic::AtomicPtr<libc::c_char>; MAX_REGISTERED_TEMP_PATHS] = [
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
];

fn register_temp_path(path: &std::path::Path) -> anyhow::Result<usize> {
    let ptr = leak_path_as_cstring(path)?;
    for (index, slot) in TEMP_PATH_SLOTS.iter().enumerate() {
        if slot
            .compare_exchange(
                std::ptr::null_mut(),
                ptr,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return Ok(index);
        }
    }
    anyhow::bail!(
        "no free temp-path registration slot ({} already in flight -- this binary never \
         registers more than {} paths at once; a caller changed that invariant without \
         widening this registry)",
        TEMP_PATH_SLOTS.len(),
        TEMP_PATH_SLOTS.len()
    );
}

fn unregister_temp_path(slot: usize) {
    TEMP_PATH_SLOTS[slot].store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
}

// the CURRENTLY-ACTIVE sample's own fake HOME directory (and the one file this harness itself
// ever writes inside it -- `subject.log`, only under `--tracing`), or null for neither -- found
// necessary in PR #44 pass-2 review: `runner::FakeHome`'s own Drop is the ONLY thing that used
// to clean it up, and SIGINT/SIGTERM/SIGHUP bypass Drop entirely (see this file's own file-level
// doc comment above), the identical gap this same round already closed for fixture generation's
// temp files and the active subject. Exactly one slot each, mirroring `pty::ACTIVE_CHILD_PGID`'s
// own single-slot design: `FakeHome::create`'s own doc comment establishes that `run_sample`
// never has more than one alive at a time. Two separate pointers, not one: the directory needs
// `rmdir`, the log file needs `unlink` -- different syscalls for a directory vs. a plain file --
// and both are pre-registered as `CString`s at ordinary, non-signal-handler registration time
// (`register_fake_home`, called from inside `runner::FakeHome::create`'s own
// `pty::spawn_window_blocked` window, the same "create a resource, then register it, with
// SIGINT/SIGTERM/SIGHUP blocked across the whole gap" pattern `pty.rs`'s own spawn-side finding
// already established) rather than built byte-by-byte inside the handler itself, which
// async-signal-safety would otherwise force (no allocation is permitted there at all).
//
// A bare store to clear, not `ACTIVE_CHILD_PGID`'s own compare-exchange: that guard exists
// specifically because a PID can be RECYCLED, creating a genuine "is this still MY subject"
// ambiguity a bare store could get wrong. A directory path has no equivalent ambiguity -- this
// registry is closer in spirit to `TEMP_PATH_SLOTS` just above (a scratch-path registry, not a
// process identity), which already uses a bare store for the identical "clear my own
// registration" operation.
static ACTIVE_FAKE_HOME_DIR: std::sync::atomic::AtomicPtr<libc::c_char> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static ACTIVE_FAKE_HOME_LOG: std::sync::atomic::AtomicPtr<libc::c_char> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

// pure functions over explicit `&AtomicPtr` slots rather than reaching for
// ACTIVE_FAKE_HOME_DIR/LOG directly -- lets the registration/clear LOGIC be unit-tested against
// throwaway local atomics (see this module's own tests), never the real globals, which are
// genuine process-wide shared state and would otherwise make such a test racy against every
// OTHER test that also creates/drops a `FakeHome` under plain `cargo test`'s thread-parallel
// model -- the identical reasoning `pty.rs`'s own `register_active_child`/
// `clear_active_child_if_mine` split already established for `ACTIVE_CHILD_PGID`.
fn register_fake_home_paths(
    dir_slot: &std::sync::atomic::AtomicPtr<libc::c_char>,
    log_slot: &std::sync::atomic::AtomicPtr<libc::c_char>,
    dir: &std::path::Path,
    log_file: &std::path::Path,
) -> anyhow::Result<()> {
    let dir_ptr = leak_path_as_cstring(dir)?;
    let log_ptr = leak_path_as_cstring(log_file)?;
    dir_slot.store(dir_ptr, std::sync::atomic::Ordering::SeqCst);
    log_slot.store(log_ptr, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

fn unregister_fake_home_paths(
    dir_slot: &std::sync::atomic::AtomicPtr<libc::c_char>,
    log_slot: &std::sync::atomic::AtomicPtr<libc::c_char>,
) {
    dir_slot.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
    log_slot.store(std::ptr::null_mut(), std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn register_fake_home(
    dir: &std::path::Path,
    log_file: &std::path::Path,
) -> anyhow::Result<()> {
    register_fake_home_paths(&ACTIVE_FAKE_HOME_DIR, &ACTIVE_FAKE_HOME_LOG, dir, log_file)
}

pub(crate) fn unregister_fake_home() {
    unregister_fake_home_paths(&ACTIVE_FAKE_HOME_DIR, &ACTIVE_FAKE_HOME_LOG)
}

// the ENTIRE handler body must stay async-signal-safe (POSIX's signal-safety(7) list): it can
// run at any point this process's normal control flow was already at, including mid-allocation
// or holding a lock the handler would then need itself. Every call here is on that list: atomic
// loads, `kill(2)`, `unlink(2)`, `rmdir(2)`, `_exit(2)`. Deliberately no `eprintln!`/`println!`
// (Rust's formatting/stdio machinery allocates and can lock a buffer), no panics, nothing
// beyond those primitives.
//
// the subject is killed FIRST, before any of the path cleanup -- independent concerns with no
// ordering dependency between them, but killing the subject is the more time-sensitive of the
// three (every extra moment it stays alive on a hung/dead mount is exactly the failure mode this
// fix exists to close), so it goes first. The fake HOME's log file is unlinked before its own
// directory is rmdir'd, for the obvious structural reason (a non-empty directory cannot be
// rmdir'd at all) -- both are best-effort: `unlink` on a file that was never created (tracing was
// off, or nothing was written yet) fails with ENOENT, silently discarded, the same "already gone
// is fine" contract `TEMP_PATH_SLOTS`' own cleanup below already keeps; `rmdir` on a directory
// that (unexpectedly, contrary to this harness's own whitelist_env design -- see FakeHome's own
// doc comment) still contains something else fails with ENOTEMPTY, ALSO silently discarded --
// graceful degradation (a leftover scratch directory outlives this run) rather than a reason to
// skip or delay the cleanup around it.
extern "C" fn handle_termination_signal(_signum: libc::c_int) {
    let active_pgid = crate::pty::ACTIVE_CHILD_PGID.load(std::sync::atomic::Ordering::SeqCst);
    if active_pgid != 0 {
        unsafe { libc::kill(-active_pgid, libc::SIGKILL) };
    }
    let fake_home_log = ACTIVE_FAKE_HOME_LOG.load(std::sync::atomic::Ordering::SeqCst);
    if !fake_home_log.is_null() {
        unsafe { libc::unlink(fake_home_log) };
    }
    let fake_home_dir = ACTIVE_FAKE_HOME_DIR.load(std::sync::atomic::Ordering::SeqCst);
    if !fake_home_dir.is_null() {
        unsafe { libc::rmdir(fake_home_dir) };
    }
    for slot in &TEMP_PATH_SLOTS {
        let ptr = slot.load(std::sync::atomic::Ordering::SeqCst);
        if !ptr.is_null() {
            unsafe { libc::unlink(ptr) };
        }
    }
    unsafe { libc::_exit(130) };
}

// installed once, in main(), before anything that could register a temp path runs. SIGINT,
// SIGTERM, and (since PR #44 pass-2) SIGHUP share the identical handler: all three mean "stop
// now," and all three need the identical cleanup -- 130 (128 + SIGINT's own number) is used for
// all of them, the conventional shell exit code for "killed by a signal," rather than a
// signal-specific 128+n for each (SIGHUP included: still 130, not 129, the same uniform choice
// this handler already made for SIGTERM rather than SIGTERM's own 128+15).
fn install_signal_cleanup_handler() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_termination_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut());
    }
}

// registered before the write starts, not after: the write itself is the long, interruptible
// part, potentially multi-GiB for the large presets. Drop always attempts removal
// unconditionally -- a path that has already been renamed away (the success path) makes this a
// harmless no-op (`remove_file` on an already-moved path just returns a `NotFound` error,
// silently discarded below), the identical reasoning perf.sh's own `cleanup()`/
// `GENERATED_TEMP_FILES` gives for never bothering to de-register a path post-rename. Drop
// alone is not the whole story: SIGINT/SIGTERM never reach it at all, so this guard also
// registers/deregisters the SAME path in the signal-safe registry above -- the handler's own
// unlink is what actually saves an interrupted run, Drop is only the normal-exit path.
struct TempFileGuard {
    path: std::path::PathBuf,
    slot: usize,
}

impl TempFileGuard {
    fn new(path: std::path::PathBuf) -> anyhow::Result<TempFileGuard> {
        let slot = register_temp_path(&path)?;
        Ok(TempFileGuard { path, slot })
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
        unregister_temp_path(self.slot);
    }
}

// perf.sh's `generate_fixture`: `out` via ress-filegen's `preset_name` at `target_bytes`, seed
// pinned to 42 (perf.sh's own `--seed 42`, overriding the preset's own default seed), unless
// both `out` and its `.stats` sidecar already exist. Both files are written through a
// same-directory temp file and renamed into place, data first and stats last, so a completeness
// check of "both files exist" is actually true: a run interrupted mid-generation leaves either
// neither file (killed before the data rename) or the data file with no `.stats` (killed after)
// -- never a half-written file sitting at the final name. An existing data file with no
// `.stats` is never assumed to be this function's own regenerable leftover -- it could be a
// real file that happens to collide with the name, especially on a shared mount -- so that case
// is a hard abort naming both paths, left exactly as found for a human to inspect.
fn generate_fixture(
    out: &std::path::Path,
    preset_name: &str,
    target_bytes: u64,
) -> anyhow::Result<()> {
    let stats = crate::scenarios::stats_sidecar_path(out);
    if out.is_file() && stats.is_file() {
        eprintln!("fixture present, reusing: {}", out.display());
        return Ok(());
    }
    if out.is_file() && !stats.is_file() {
        anyhow::bail!(
            "partial fixture pair, never auto-overwritten: {out:?} exists but {stats:?} does not \
             (inspect/remove {out:?} by hand, then re-run)"
        );
    }
    if !out.is_file() && stats.is_file() {
        anyhow::bail!(
            "partial fixture pair, never auto-overwritten: {stats:?} exists but {out:?} does not \
             (inspect/remove {stats:?} by hand, then re-run)"
        );
    }
    eprintln!(
        "generating fixture: {} (preset={preset_name} target_bytes={target_bytes} seed=42)",
        out.display()
    );
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            anyhow::Error::new(err).context(format!("creating fixture directory {parent:?}"))
        })?;
    }
    let pid = std::process::id();
    let mut tmp_out = out.as_os_str().to_owned();
    tmp_out.push(format!(".tmp.{pid}"));
    let tmp_out = std::path::PathBuf::from(tmp_out);
    let mut tmp_stats = stats.as_os_str().to_owned();
    tmp_stats.push(format!(".tmp.{pid}"));
    let tmp_stats = std::path::PathBuf::from(tmp_stats);
    let _tmp_out_guard = TempFileGuard::new(tmp_out.clone())?;
    let _tmp_stats_guard = TempFileGuard::new(tmp_stats.clone())?;
    let mut spec = ress_filegen::preset(preset_name)
        .ok_or_else(|| anyhow::anyhow!("unknown ress-filegen preset: {preset_name}"))?;
    spec.seed = 42;
    spec.target_bytes = target_bytes;
    let file = std::fs::File::create(&tmp_out).map_err(|err| {
        anyhow::Error::new(err).context(format!("creating fixture temp file {tmp_out:?}"))
    })?;
    let mut writer = std::io::BufWriter::new(file);
    let stats_data = ress_filegen::generate(&spec, &mut writer)
        .map_err(|err| anyhow::Error::new(err).context(format!("generating fixture {out:?}")))?;
    std::io::Write::flush(&mut writer)
        .map_err(|err| anyhow::Error::new(err).context(format!("flushing fixture {out:?}")))?;
    std::fs::write(
        &tmp_stats,
        format!("bytes={} lines={}\n", stats_data.bytes, stats_data.lines),
    )
    .map_err(|err| {
        anyhow::Error::new(err).context(format!("writing fixture stats sidecar {tmp_stats:?}"))
    })?;
    std::fs::rename(&tmp_out, out).map_err(|err| {
        anyhow::Error::new(err).context(format!("renaming {tmp_out:?} to {out:?}"))
    })?;
    std::fs::rename(&tmp_stats, &stats).map_err(|err| {
        anyhow::Error::new(err).context(format!("renaming {tmp_stats:?} to {stats:?}"))
    })?;
    Ok(())
}

// the standard set lives here, in one place -- a `just fixtures` recipe (the justfile's
// concern) calls this binary with --fixtures-only rather than repeating the list, so there is
// exactly one place that knows what a full run needs. The mount fixtures are generated ONLY on
// the --fixtures-only path, never on a timed run: writing 256 MiB onto the mount warms the
// client's (and possibly the server's) cache through the write itself (see
// scenarios::run_mount_open's own doc comment), so a run that generates a fixture and then
// immediately times access to it would be measuring its own generation-warmth, not a fresh,
// unprewarmed first read.
fn ensure_fixtures(
    fixtures_dir: &std::path::Path,
    varied_log_name: &str,
    varied_log_bytes: u64,
    mount_fixtures_dir: Option<&std::path::Path>,
    fixtures_only: bool,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(fixtures_dir).map_err(|err| {
        anyhow::Error::new(err).context(format!("creating fixtures directory {fixtures_dir:?}"))
    })?;
    generate_fixture(
        &fixtures_dir.join(varied_log_name),
        "varied-log",
        varied_log_bytes,
    )?;
    generate_fixture(
        &fixtures_dir.join("megalines-512m.log"),
        "megalines",
        512u64 << 20,
    )?;
    generate_fixture(
        &fixtures_dir.join("single-line-256m.log"),
        "single-line",
        256u64 << 20,
    )?;
    if fixtures_only && let Some(mount_fixtures_dir) = mount_fixtures_dir {
        for scenario in crate::scenarios::MOUNT_FIRST_SCENARIOS {
            generate_fixture(
                &crate::scenarios::mount_fixture_path(mount_fixtures_dir, scenario),
                "varied-log",
                256u64 << 20,
            )?;
        }
    }
    Ok(())
}

fn print_results(
    fixtures_dir: &std::path::Path,
    rows: &[crate::report::TimingRow],
    rss_rows: &[crate::report::RssRow],
) -> anyhow::Result<()> {
    let tsv_path = fixtures_dir.join("last-run.tsv");
    std::fs::write(&tsv_path, crate::report::render_tsv(rows, rss_rows))
        .map_err(|err| anyhow::Error::new(err).context(format!("writing {tsv_path:?}")))?;
    println!();
    print!("{}", crate::report::render_table(rows, rss_rows));
    println!();
    println!("machine-readable copy: {}", tsv_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    // removes a scratch directory (and everything under it) on drop -- mirrors
    // test_support::TempFile, one level up (a whole fixture directory, not a single file).
    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn scratch_dir(label: &str) -> (TempDir, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("ress-perf-main-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        (TempDir(path.clone()), path)
    }

    #[test]
    fn parses_quick_and_fixtures_only_flags() {
        let cli = <super::Cli as clap::Parser>::try_parse_from([
            "ress-perf",
            "--quick",
            "--fixtures-only",
        ])
        .unwrap();
        assert!(cli.quick);
        assert!(cli.fixtures_only);
    }

    #[test]
    fn defaults_to_false_for_both_flags() {
        let cli = <super::Cli as clap::Parser>::try_parse_from(["ress-perf"]).unwrap();
        assert!(!cli.quick);
        assert!(!cli.fixtures_only);
    }

    #[test]
    fn quick_defaults_to_two_runs_over_the_small_fixture() {
        let (runs, name, bytes) = super::determine_runs(true, None).unwrap();
        assert_eq!(runs, 2);
        assert_eq!(name, "varied-log-256m.log");
        assert_eq!(bytes, 256 << 20);
    }

    #[test]
    fn default_run_uses_six_runs_over_the_large_fixture() {
        let (runs, name, bytes) = super::determine_runs(false, None).unwrap();
        assert_eq!(runs, 6);
        assert_eq!(name, "varied-log-2g.log");
        assert_eq!(bytes, 2 << 30);
    }

    #[test]
    fn override_replaces_the_derived_default() {
        let (runs, ..) = super::determine_runs(false, Some("10")).unwrap();
        assert_eq!(runs, 10);
    }

    #[test]
    fn override_zero_is_rejected() {
        assert!(super::determine_runs(false, Some("0")).is_err());
    }

    #[test]
    fn override_non_numeric_is_rejected() {
        assert!(super::determine_runs(false, Some("abc")).is_err());
    }

    #[test]
    fn mount_override_empty_string_is_treated_as_unset() {
        // the exact PR #44 pass-2 regression: `RESS_PERF_MOUNT=""` used to be wrapped straight
        // into `Some(PathBuf::from(""))` and then fail `is_dir()` validation, where perf.sh's
        // own `[[ -n ]]` gate silently used no mount at all.
        assert_eq!(
            super::resolve_mount_override(Some(std::ffi::OsString::new())),
            None
        );
    }

    #[test]
    fn mount_override_absent_stays_none() {
        assert_eq!(super::resolve_mount_override(None), None);
    }

    #[test]
    fn mount_override_non_empty_passes_through_as_a_path() {
        assert_eq!(
            super::resolve_mount_override(Some(std::ffi::OsString::from("/mnt/perf"))),
            Some(std::path::PathBuf::from("/mnt/perf"))
        );
    }

    #[test]
    fn runs_override_empty_string_is_treated_as_unset() {
        // the exact PR #44 pass-2 regression: `RESS_PERF_RUNS=""` used to reach
        // `determine_runs` as `Some("")` and fail integer parsing, where perf.sh's own `[[ -n ]]`
        // gate silently kept the derived default.
        assert_eq!(
            super::resolve_runs_override(Some(std::ffi::OsString::new())),
            None
        );
    }

    #[test]
    fn runs_override_absent_stays_none() {
        assert_eq!(super::resolve_runs_override(None), None);
    }

    #[test]
    fn runs_override_non_empty_passes_through_as_a_string() {
        assert_eq!(
            super::resolve_runs_override(Some(std::ffi::OsString::from("10"))),
            Some("10".to_string())
        );
    }

    #[test]
    fn generates_a_fresh_fixture_and_its_stats_sidecar() {
        let (_guard, dir) = scratch_dir("fresh");
        let out = dir.join("f.log");
        super::generate_fixture(&out, "varied-log", 4096).unwrap();
        assert!(out.is_file());
        let stats = crate::scenarios::stats_sidecar_path(&out);
        let lines = crate::scenarios::read_lines_from_stats(&stats).unwrap();
        assert!(lines > 0);
        assert!(std::fs::metadata(&out).unwrap().len() <= 4096);
        // no leftover temp files -- both renames landed.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn reuses_an_existing_complete_pair_without_regenerating() {
        let (_guard, dir) = scratch_dir("reuse");
        let out = dir.join("f.log");
        std::fs::write(&out, b"pre-existing content\n").unwrap();
        let stats = crate::scenarios::stats_sidecar_path(&out);
        std::fs::write(&stats, "bytes=21 lines=1\n").unwrap();
        super::generate_fixture(&out, "varied-log", 4096).unwrap();
        // untouched: a fresh generation would have overwritten this with filegen's own output.
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "pre-existing content\n"
        );
    }

    #[test]
    fn aborts_on_a_partial_pair_missing_stats() {
        let (_guard, dir) = scratch_dir("partial-missing-stats");
        let out = dir.join("f.log");
        std::fs::write(&out, b"data with no sidecar\n").unwrap();
        let err = super::generate_fixture(&out, "varied-log", 4096).unwrap_err();
        assert!(format!("{err:#}").contains("partial fixture pair"));
        // never silently overwritten.
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "data with no sidecar\n"
        );
    }

    #[test]
    fn aborts_on_a_partial_pair_missing_data() {
        let (_guard, dir) = scratch_dir("partial-missing-data");
        let out = dir.join("f.log");
        let stats = crate::scenarios::stats_sidecar_path(&out);
        std::fs::write(&stats, "bytes=1 lines=1\n").unwrap();
        let err = super::generate_fixture(&out, "varied-log", 4096).unwrap_err();
        assert!(format!("{err:#}").contains("partial fixture pair"));
        assert!(!out.exists());
    }

    #[test]
    fn unknown_preset_is_rejected_before_any_write() {
        let (_guard, dir) = scratch_dir("unknown-preset");
        let out = dir.join("f.log");
        assert!(super::generate_fixture(&out, "no-such-preset", 4096).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn which_finds_a_binary_already_known_to_be_on_path() {
        // cross-checked against test_support's own PATH search (used throughout pty.rs's/
        // screen.rs's tests) rather than hardcoding a binary name this dev shell might lack.
        let expected = crate::test_support::on_path("env");
        assert_eq!(super::which("env").unwrap(), expected);
    }

    #[test]
    fn which_reports_a_missing_binary_by_name() {
        let err = super::which("no-such-binary-ress-perf-test").unwrap_err();
        assert!(format!("{err:#}").contains("no-such-binary-ress-perf-test"));
    }

    #[test]
    fn is_executable_distinguishes_a_plain_file_from_an_exec_bit_file() {
        let (_guard, dir) = scratch_dir("is-executable");
        let plain = dir.join("plain");
        std::fs::write(&plain, b"data").unwrap();
        assert!(!super::is_executable(&plain));
        let mut perms = std::fs::metadata(&plain).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&plain, perms).unwrap();
        assert!(super::is_executable(&plain));
    }

    // found in PR #44 pass-2 review: mirrors `pty::tests`' own
    // `active_child_registration_sets_and_clears_by_compare_exchange` for the structurally
    // similar FakeHome registration, against throwaway local atomics rather than the real
    // process-wide `ACTIVE_FAKE_HOME_DIR`/`ACTIVE_FAKE_HOME_LOG` -- which every OTHER
    // runner.rs test that spawns a real subject also touches indirectly via `FakeHome::create`,
    // making the real globals unsafe to assert against directly under `cargo test`'s
    // thread-parallel model (the exact class of hazard this crate has already been bitten by
    // twice).
    #[test]
    fn register_fake_home_paths_sets_and_clears_both_slots() {
        let dir_slot = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
        let log_slot = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
        super::register_fake_home_paths(
            &dir_slot,
            &log_slot,
            std::path::Path::new("/tmp/fake-home-test"),
            std::path::Path::new("/tmp/fake-home-test/subject.log"),
        )
        .unwrap();
        assert!(!dir_slot.load(std::sync::atomic::Ordering::SeqCst).is_null());
        assert!(!log_slot.load(std::sync::atomic::Ordering::SeqCst).is_null());
        super::unregister_fake_home_paths(&dir_slot, &log_slot);
        assert!(dir_slot.load(std::sync::atomic::Ordering::SeqCst).is_null());
        assert!(log_slot.load(std::sync::atomic::Ordering::SeqCst).is_null());
    }

    // found in PR #44's round-5 review: a `less` decoy earlier on PATH than the real binary,
    // lacking the exec bit (a freshly-written file, exactly as constructed here, carries no
    // executable bit by default -- the identical mode a `chmod -x` would leave behind), used to
    // satisfy `which`'s own `is_file()`-only check and hide the real binary sitting right behind
    // it. `which_in` takes the PATH string directly (see its own doc comment) so this never
    // touches the real, process-wide `PATH` env var -- safe under `cargo test`'s thread-parallel
    // model, unlike a `std::env::set_var("PATH", ...)` would be.
    #[test]
    fn which_in_skips_a_non_executable_decoy_and_finds_the_real_binary_later_on_path() {
        let (_guard, dir) = scratch_dir("which-decoy");
        let decoy_dir = dir.join("decoy");
        let real_dir = dir.join("real");
        std::fs::create_dir_all(&decoy_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        let decoy = decoy_dir.join("probe-bin");
        std::fs::write(&decoy, b"not a real binary\n").unwrap();
        let real = real_dir.join("probe-bin");
        std::fs::write(&real, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&real).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&real, perms).unwrap();
        let scratch_path = std::env::join_paths([&decoy_dir, &real_dir]).unwrap();
        assert_eq!(super::which_in("probe-bin", &scratch_path).unwrap(), real);
    }

    #[test]
    fn which_in_reports_not_executable_when_the_only_candidate_lacks_the_exec_bit() {
        let (_guard, dir) = scratch_dir("which-only-decoy");
        let decoy = dir.join("probe-bin-solo");
        std::fs::write(&decoy, b"not a real binary\n").unwrap();
        let scratch_path = std::env::join_paths([&dir]).unwrap();
        let err = super::which_in("probe-bin-solo", &scratch_path).unwrap_err();
        assert!(
            format!("{err:#}").contains("not found (or not executable)"),
            "{err:#}"
        );
    }

    #[test]
    fn manifest_dir_tier_takes_the_crate_dirs_parent() {
        let manifest_dir = std::path::Path::new("/a/b/repo/ress-perf");
        assert_eq!(
            super::repo_root_from_manifest_dir(Some(manifest_dir)),
            Some(std::path::PathBuf::from("/a/b/repo"))
        );
    }

    #[test]
    fn manifest_dir_tier_is_none_when_the_variable_is_absent() {
        assert_eq!(super::repo_root_from_manifest_dir(None), None);
    }

    #[test]
    fn cwd_walk_tier_finds_an_ancestor_workspace_root_from_a_nested_start() {
        let (_guard, dir) = scratch_dir("cwd-walk-found");
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
        let nested = dir.join("ress-perf").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            super::repo_root_from_cwd_walk(&nested).unwrap(),
            Some(dir.clone())
        );
    }

    #[test]
    fn cwd_walk_tier_returns_none_when_no_ancestor_is_a_workspace_root() {
        // a scratch tree with a Cargo.toml that is NOT a workspace root (a plain member
        // package) -- the walk must not mistake it for one, and must walk all the way to the
        // real filesystem root and return Ok(None) rather than erroring once it runs out of
        // ancestors (relies on no real ancestor of a tempdir being a `[workspace]` -- true on
        // every dev/CI machine this crate expects to run on).
        let (_guard, dir) = scratch_dir("cwd-walk-not-found");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"leaf\"\n").unwrap();
        assert_eq!(super::repo_root_from_cwd_walk(&dir).unwrap(), None);
    }

    #[test]
    fn is_workspace_root_matches_the_bare_table_header_only() {
        let (_guard, dir) = scratch_dir("is-workspace-root");
        let workspace_toml = dir.join("workspace.toml");
        std::fs::write(&workspace_toml, "[workspace]\nresolver = \"2\"\n").unwrap();
        assert!(super::is_workspace_root(&workspace_toml).unwrap());
        // [workspace.dependencies] alone (no bare [workspace] line) is a member package that
        // merely inherits from a workspace elsewhere -- not itself the workspace root.
        let member_toml = dir.join("member.toml");
        std::fs::write(
            &member_toml,
            "[package]\nname = \"leaf\"\n\n[dependencies]\nfoo = { workspace = true }\n",
        )
        .unwrap();
        assert!(!super::is_workspace_root(&member_toml).unwrap());
    }

    #[test]
    fn exe_walk_tier_climbs_target_profile_binary_to_the_repo_root() {
        let exe = std::path::Path::new("/x/y/repo/target/release/ress-perf");
        assert_eq!(
            super::repo_root_from_exe_path(exe).unwrap(),
            std::path::PathBuf::from("/x/y/repo")
        );
    }

    #[test]
    fn exe_walk_tier_errors_when_the_exe_path_is_too_shallow() {
        let exe = std::path::Path::new("/ress-perf");
        assert!(super::repo_root_from_exe_path(exe).is_err());
    }

    #[test]
    fn ress_bin_sits_beside_ress_perf_in_the_default_target_layout() {
        let exe = std::path::Path::new("/repo/target/release/ress-perf");
        assert_eq!(
            super::ress_bin_from_exe_path(exe).unwrap(),
            std::path::PathBuf::from("/repo/target/release/ress")
        );
    }

    #[test]
    fn ress_bin_follows_ress_perf_under_a_relocated_target_dir() {
        // the exact PR #44 round-2 scenario: CARGO_TARGET_DIR moves ress-perf (and, since the
        // justfile builds both the same way, ress too) out from under the repo entirely --
        // sibling resolution must follow it there rather than guessing a repo-relative path.
        let exe = std::path::Path::new("/tmp/relocated-target/release/ress-perf");
        assert_eq!(
            super::ress_bin_from_exe_path(exe).unwrap(),
            std::path::PathBuf::from("/tmp/relocated-target/release/ress")
        );
    }

    #[test]
    fn ress_bin_follows_ress_perf_into_a_debug_profile_directory_too() {
        // no special-casing "release" -- sibling resolution is profile-agnostic by construction,
        // since it only ever looks beside whatever directory ress-perf itself happens to be
        // running from. (Nothing builds a debug ress to pair with a debug ress-perf today --
        // that's a documented assumption, not something this function needs to enforce.)
        let exe = std::path::Path::new("/repo/target/debug/ress-perf");
        assert_eq!(
            super::ress_bin_from_exe_path(exe).unwrap(),
            std::path::PathBuf::from("/repo/target/debug/ress")
        );
    }
}
