// shared test-only helpers for pty.rs's and screen.rs's unit tests. unlike smoke.rs's
// production-vs-test relationship with pty.rs (an accepted ~80 lines of duplication, since
// production code can't depend on test infra), these two are both unit-test modules in the
// same crate — sharing is trivial and cheap, so the logic lives here once instead of twice.
#![cfg(test)]

// a which-style search over the TEST process's own PATH — PtyChild::spawn takes an
// already-resolved argv[0] (env_clear leaves the spawned child with no PATH to search).
pub(crate) fn on_path(name: &str) -> std::path::PathBuf {
    let path = std::env::var_os("PATH").expect("PATH must be set for this test");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} not found on PATH"))
}

// CARGO_BIN_EXE_<name> is only ever populated (compile-time env!, or the runtime process
// environment) for integration-test targets under tests/ — confirmed empirically: neither form
// resolves for a unit test compiled as part of the bin crate's own module tree, since that
// isn't a separate crate depending on the package's bins. `cargo test`/`cargo nextest run`
// likewise only build each target's own unit-test harness, not plain sibling binaries, so one
// may not exist on disk yet either. work around both gaps: derive the binary's expected path
// from this test binary's own location (a sibling of its grandparent —
// target/<profile>/deps/<harness> -> target/<profile>/<name>) and always run `cargo build`
// against it before returning, rather than gating on the path merely existing.
//
// three gaps confirmed empirically, not assumed, while writing runner.rs's timing-basis test
// (1-2) and fixing PR #44's round-7 review (3):
//
// 1. gating the build on `bin_path.is_file()` (this function's original shape) is wrong: a
//    binary that exists on disk but predates a later source edit is not "built", it is stale.
//    An already-built `fake-pager` from an earlier task silently kept serving its pre-edit
//    behavior (missing a mode a newer test needed) after the mode was added, with no
//    error, just a wrong answer downstream. `cargo build`'s own incremental check is fast (a
//    metadata/mtime comparison, a no-op when nothing changed), so always invoking it costs
//    nothing measurable while closing this gap for every future edit to any sibling binary.
// 2. `cargo build --bin <name>` run with no explicit manifest is resolved relative to the
//    invoking process's OWN current directory — and nextest runs each test binary with its
//    cwd set to ITS OWN package's root (`ress-perf/`), not the workspace root, confirmed by
//    printing `std::env::current_dir()` from inside a running test. For a bin whose package
//    sets `default-run` (`ress`'s `Cargo.toml` does), cargo invoked from a DIFFERENT package's
//    directory fails outright ("no bin target named `ress` in default-run packages") instead
//    of resolving it workspace-wide the way the same command does from the workspace root.
//    This was masked by gap 1 above for as long as `target/debug/ress` already existed from
//    unrelated prior builds (which, in an actively-developed workspace, is effectively always)
//    — removing that gate exposed it. Fixed the same way for every caller: an explicit
//    `--manifest-path` pinned to the workspace root (this crate's own `CARGO_MANIFEST_DIR` is
//    always `<workspace root>/ress-perf`, a compile-time constant, so `.parent()` of it is the
//    workspace root regardless of the process's own runtime cwd).
// 3. the nested `cargo build` didn't thread through the SAME target directory `profile_dir`
//    (below) already derived from `current_exe()`'s real, observed location -- it relied on
//    the OUTER `cargo test`/`cargo nextest run` invocation's own target-dir choice being
//    inherited into this process's environment and then forwarded into the nested build's
//    (`Command` inherits the parent's environment by default, and nothing here ever cleared
//    it). That inheritance chain has a real gap: a `--target-dir <dir>` CLI flag on the OUTER
//    invocation relocates where cargo builds/places THIS VERY test binary (confirmed --
//    `current_exe()` correctly reflects it), but does NOT export `CARGO_TARGET_DIR` as an
//    environment variable to the spawned test process at all -- confirmed directly with a
//    throwaway probe test (`eprintln!("{:?}", std::env::var("CARGO_TARGET_DIR"))` under
//    `cargo test -p ress-perf --target-dir <scratch> ... -- --nocapture`): `Err(NotPresent)`,
//    every time, even though `current_exe()` in the same process correctly showed `<scratch>`.
//    So under that specific invocation shape the nested build fell back to cargo's OWN default
//    target dir (the workspace's ordinary `target/`), landing the freshly built sibling
//    somewhere this function's own `bin_path` (derived from the REAL, relocated
//    `profile_dir`) never looks. Reproduced directly, not just reasoned about: `cargo test -p
//    ress-perf --target-dir <fresh, never-before-used dir> ... pty::tests::
//    child_exit_is_observable_and_drop_reaps` failed with `No such file or directory (os error
//    2)` spawning `fake-pager` -- the nested build had silently placed it at the workspace's
//    ordinary `target/debug/fake-pager` instead, confirmed absent from the fresh target-dir
//    entirely. Fixed by passing `--target-dir` explicitly on the nested build, derived from the
//    SAME `profile_dir` this function's own lookup already computed (`profile_dir.parent()` --
//    `profile_dir` is `<target-dir>/<profile>`, so its own parent IS the target-dir cargo
//    build wants) -- one source of truth, not a second, independently-derived guess that could
//    drift from the first. A CLI flag, not `cmd.env("CARGO_TARGET_DIR", ...)`: this function
//    already derives its own expectation from OBSERVED REALITY (`current_exe()`'s real path)
//    specifically because environment-based signaling proved unreliable one hop up (the exact
//    gap just described) -- leaning on `.env()` here would repeat that same "trust an env var
//    to carry the right value across a process boundary" assumption for the INNER hop instead
//    of closing it. A flag also always wins over cargo's own config-file/env-based target-dir
//    resolution regardless of how the OUTER invocation happened to set it (a CLI flag,
//    `CARGO_TARGET_DIR`, or a `.cargo/config.toml` `[build] target-dir` -- a third mechanism
//    neither this function nor a forwarded env var would otherwise account for), where
//    forwarding a possibly-absent inherited env var would not.
//
// found in PR #44 pass-2 RE-review: the round-7 fix above assumed a fixed depth
// (`<target-dir>/<profile>/deps/<harness>`), true on a plain dev machine but not under a
// `--target <triple>` build, which cargo lays out one level deeper:
// `<target-dir>/<triple>/<profile>/deps/<harness>`. The nix sandbox's own checkPhase builds
// this way (confirmed, not guessed -- see below), so `sibling_paths`' old `target_dir =
// profile_dir.parent()` pointed at the TRIPLE segment, not the true root -- the nested `cargo
// build` in `sibling_bin_path` then passed THAT as `--target-dir`, and cargo (still under the
// SAME ambient triple) re-qualified it a SECOND time, landing the freshly built sibling at
// `<target-dir>/<triple>/<triple>/<profile>/...` -- one level away from where `bin_path` (still
// computed from the OLD, un-triple-aware math) expected it. `sibling_bin_path("fake-pager")`
// exists precisely to make that binary buildable and locatable by every OTHER test that spawns
// it -- with the mismatch, those spawns failed "No such file or directory" instead.
//
// The phase-marker reason this was only DISCOVERED at pass-2 RE-review, not pass-2 itself: this
// package's own `nix build` used to build the WHOLE workspace (no `-p` selector) in its
// `buildPhase`, which ALSO builds under the same ambient triple and so ALSO landed `fake-pager`
// at the correctly triple-qualified path -- before `checkPhase`'s own nested build (this
// function) ever got a chance to expose its own bug. Pass-2's `cargoBuildFlags = [ "-p" "ress"
// ]` (this file's own sibling fix, `flake.nix`) removed that accidental safety net: `buildPhase`
// now builds ONLY `ress`, so `fake-pager` is never pre-seeded, and THIS function's nested build
// becomes the sole, and broken, source. The underlying bug in this function predates pass-2 --
// but pass-2's own `cargoBuildFlags` change is what turned it from latent into an observed
// `checkPhase` failure, confirmed directly: a real `nix build` with `cargoBuildFlags` scoped-
// reverted passes `checkPhase` cleanly (0 failures); with it restored, 20 of 102 tests fail
// (every one that spawns a sibling binary) -- so this is corrected as part of the SAME review
// pass that caused it to surface, not filed as a separate, unrelated pre-existing gap.
//
// Two signals detect a triple segment, tried in order, BOTH derived from observed reality rather
// than trusting a value to have crossed a process boundary correctly unchecked (the same
// reasoning `target_dir` vs `CARGO_TARGET_DIR` above already established):
//
// 1. `CARGO_BUILD_TARGET` -- confirmed the ACTUAL mechanism by reading nixpkgs' own
//    `cargoCheckHook`/`cargoBuildHook` setup-hooks directly (found via this build's own
//    `nix derivation show .#default`, not guessed): both hooks invoke `env
//    CARGO_BUILD_TARGET=<triple> ... cargo {test,build} --target <triple> --offline ...` --
//    the env var AND the flag, redundantly, on the SAME outer invocation. `Command`'s default
//    (inherit the parent's environment) means this env var is present in every test binary's own
//    process, this one included. Checked as an EXACT match against `profile_dir`'s own parent's
//    `file_name()` -- confirming the OBSERVED path actually carries this value, not assuming a
//    var that happens to be set necessarily describes THIS harness's own real layout. A MATCH is
//    trusted outright (signal 1 alone is enough). A MISMATCH is NOT itself proof of "no triple" --
//    found in PR #44 round 15 (a codex P2): an earlier version returned `None` straight from a
//    mismatch, on exactly the reasoning this doc comment's own previous paragraph already argued
//    against elsewhere -- a var that happens to be SET does not necessarily describe this
//    harness's own real layout, and a var that happens to be set WRONG is no different. Real
//    scenario: a CLI `--target` on the OUTER build overrides a stale/global `CARGO_BUILD_TARGET`
//    env var (cargo's own CLI-over-env precedence) -- the harness genuinely lives under a
//    DIFFERENT triple than the env var claims, and the old code, having concluded "no triple" from
//    the mismatch, never checked signal 2 to find the real one. A mismatch now falls through to
//    the filesystem probe below instead of returning early -- observed reality settles it either
//    way, exactly like signal 2 already does on its own.
// 2. A filesystem probe, tried whenever no ambient env var is set OR it did not match (a plain dev
//    machine using a committed `.cargo/config.toml`'s own `[build] target = ...`, with no
//    accompanying env var, lands here directly; a mismatched env var lands here as the fallback
//    just described): cargo writes `.rustc_info.json` exactly once, at the TRUE target-dir root
//    only -- confirmed empirically, not assumed (`cargo build --target
//    x86_64-unknown-linux-gnu --target-dir <scratch>`: `.rustc_info.json` landed at
//    `<scratch>/.rustc_info.json` only, absent from `<scratch>/x86_64-unknown-linux-gnu/`).
//    `CACHEDIR.TAG` was checked and ruled out for this same purpose first -- cargo writes an
//    IDENTICAL copy at both the true root AND every per-target subdirectory, so its presence
//    alone does not discriminate.
//
// Both signals are injected as explicit parameters (an `Option<&str>` for the env var,
// a closure for the filesystem check) rather than read here directly, so this stays a pure
// function testable with synthetic inputs -- no real env var or filesystem access needed in a
// unit test, the same reason `harness` itself is a parameter and not a live `current_exe()`
// call.
//
// This function's own return value is not the whole story, though: even after it correctly
// concludes `None` (no triple) for a genuinely-unqualified layout, a STALE `CARGO_BUILD_TARGET`
// can still be sitting in this process's own environment (the mismatch that got us here in the
// first place, or an unrelated leftover) -- and `Command`'s default of inheriting the parent
// environment would carry it straight into the nested `cargo build` below, with no `--target`
// flag on that invocation to out-rank it (a `None` triple means none is passed). Cargo itself
// then reads and honors that inherited env var, building into a triple-qualified subdirectory
// this function's own `None` said not to expect -- the identical bug shape as the mismatch just
// fixed above, reached through the sibling leg. See `nested_build_command`'s own doc comment for
// the fix.
fn detect_triple(
    profile_dir: &std::path::Path,
    ambient_target_env: Option<&str>,
    marker_exists: impl Fn(&std::path::Path) -> bool,
) -> Option<String> {
    let candidate = profile_dir.parent()?;
    let candidate_name = candidate.file_name()?.to_str()?;
    if let Some(env_target) = ambient_target_env
        && candidate_name == env_target
    {
        return Some(candidate_name.to_string());
    }
    // no env var, or one that did not match this path -- either way, observed reality (not the
    // env var) settles it; see this function's own doc comment for why a mismatch must not
    // short-circuit to `None` on its own.
    if marker_exists(&candidate.join(".rustc_info.json")) {
        // candidate already looks like the true root (has its own marker) -- no triple to skip.
        return None;
    }
    let root = candidate.parent()?;
    marker_exists(&root.join(".rustc_info.json")).then(|| candidate_name.to_string())
}

// pure function over an explicit harness path -- lets this derivation be unit-tested with
// synthetic paths (see this module's own tests below), mirroring main.rs's own
// `ress_bin_from_exe_path`/`repo_root_from_exe_path` split, the exact pattern PR #44's round-2
// review established for this same class of `current_exe()`-based derivation, rather than only
// exercisable through a real, live `current_exe()` value (which a unit test cannot control).
// Returns the profile directory (`<target-dir>/<profile>`, e.g. `.../target/release`), the
// expected sibling binary path inside it, the target-dir the nested `cargo build` needs to land
// it there, and the detected target triple (if any) that same nested build must also pass
// through explicitly -- all four derived from the same walk, one source of truth rather than
// independent guesses that could drift apart.
fn sibling_paths(
    harness: &std::path::Path,
    name: &str,
    ambient_target_env: Option<&str>,
    marker_exists: impl Fn(&std::path::Path) -> bool,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    Option<String>,
) {
    let profile_dir = harness
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| panic!("expected {harness:?} under target/<profile>/deps/"))
        .to_path_buf();
    let bin_path = profile_dir.join(name);
    let triple = detect_triple(&profile_dir, ambient_target_env, marker_exists);
    // triple-qualified layouts (`<root>/<triple>/<profile>/deps/...`) need one extra `.parent()`
    // hop past the triple segment to reach the true root; the untriple-qualified layout
    // (`<root>/<profile>/deps/...`) is exactly the round-7 math, unchanged.
    let target_dir_base = if triple.is_some() {
        profile_dir.parent().and_then(std::path::Path::parent)
    } else {
        profile_dir.parent()
    };
    let target_dir = target_dir_base
        .unwrap_or_else(|| {
            panic!(
                "expected {profile_dir:?} to live under a target-dir[/<triple>]/<profile> layout"
            )
        })
        .to_path_buf();
    (profile_dir, bin_path, target_dir, triple)
}

// found in PR #44 round 15 (a codex P2): `Command`'s default of inheriting the parent process's
// own environment used to leave `CARGO_BUILD_TARGET` (if this process has one, stale/mismatched
// or not) sitting in the nested build's own environment unconditionally. Harmless whenever an
// explicit `--target` is ALSO passed below -- cargo's own CLI-over-env precedence means that flag
// wins regardless of what the inherited var says -- but NOT when `triple` is `None`: nothing on
// this invocation then out-ranks the inherited var, so cargo itself reads and honors it, building
// into a triple-qualified subdirectory this crate's own, `None`-triple `target_dir`/`bin_path`
// never accounts for (see `detect_triple`'s own doc comment for the full derivation of this
// sibling bug, and why a stale env var can coexist with a genuinely-unqualified layout).
// `env_remove` is called UNCONDITIONALLY, not only when `triple.is_none()`: harmless in the
// `Some` case (the explicit `--target` already wins), closes the gap in the `None` case, no
// branch needed either way.
//
// Extracted as its own pure function -- `Command` itself is introspectable without ever spawning
// it (`get_envs()` reports every explicit `.env`/`.env_remove` call, though never the ambiently
// inherited environment itself), so this stays unit-testable with synthetic inputs, the same
// "pure core, thin real-I/O wrapper" split `sibling_paths`/`sibling_bin_path` already established
// for triple detection itself.
fn nested_build_command(
    cargo: &std::path::Path,
    workspace_root: &std::path::Path,
    name: &str,
    target_dir: &std::path::Path,
    triple: Option<&str>,
    release: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(cargo);
    cmd.args(["build", "--quiet", "--manifest-path"])
        .arg(workspace_root)
        .args(["--bin", name])
        .arg("--target-dir")
        .arg(target_dir)
        .env_remove("CARGO_BUILD_TARGET");
    // explicit, not left to ambient re-inheritance: this harness already derived the exact
    // triple from OBSERVED reality above, so passing it through directly makes the nested
    // build's own target selection independent of whether env var inheritance happens to carry
    // it correctly into this specific `Command` -- the same "don't trust a value to cross a
    // process boundary unchecked" reasoning this function already applies to `target_dir` itself
    // versus a re-read `CARGO_TARGET_DIR`.
    if let Some(triple) = triple {
        cmd.arg("--target").arg(triple);
    }
    if release {
        cmd.arg("--release");
    }
    cmd
}

// found in PR #44 pass 8 fold re-review (codex P2): the profile check just below used to be
// entirely implicit in `release`'s own `== Some("release")` comparison -- accurate for
// "debug"/"release" (the only two profiles anything actually builds this harness under today:
// CI only ever runs plain `cargo nextest run --workspace[, --release]`, no `--profile` anywhere,
// checked against .github/workflows/ci.yml), but silently WRONG for any other named profile: a
// custom `--profile ci` build of ress-perf itself would make `release` false, so
// `nested_build_command` below would build the nested binary WITHOUT `--release`, landing it in
// cargo's own "dev"-profile output directory (`target/debug/`, since "dev" is the one profile
// whose directory name diverges from its own name) while `bin_path` above still expects
// `target/ci/<name>` -- a real, silent path mismatch, not a build failure: whatever a test then
// execs at `bin_path` would be stale or simply wrong code.
//
// found in PR #44 pass 8 fold re-review (scope call): the FULL fix -- teaching this harness to
// build under an arbitrary named profile -- means threading the real profile name through
// `nested_build_command`'s own signature (today just a `release: bool`), special-casing cargo's
// own "dev"-directory-is-named-"debug" quirk against every other, genuinely-named custom
// profile, and its own test coverage: real scope, for a profile nothing currently uses. YAGNI.
// Refuse the unsupported case LOUDLY instead -- an unsupported profile becomes a fail-loud panic
// here rather than a silent wrong-or-stale binary a test could go on to exec.
//
// Factored out as its own pure function -- like `sibling_paths`/`nested_build_command` above,
// this keeps the check unit-testable with a synthetic name instead of requiring a real, live
// `current_exe()` this module's own tests otherwise cannot control.
fn assert_supported_profile_dir_name(name: Option<&str>) {
    assert!(
        matches!(name, Some("debug") | Some("release")),
        "sibling_bin_path only handles the debug/release profiles; got {name:?} -- a custom \
         cargo profile would build into target/debug/ while this path expects \
         target/<profile>/, silently resolving to a wrong or stale binary"
    );
}

// cargo's own build lock makes concurrent invocations safe even though nextest runs every test
// in its own process. `name` may be any workspace binary target (fake-pager, ress, ...) since
// `--manifest-path` plus `--bin <name>` resolves workspace-wide, unambiguously, from any cwd.
pub(crate) fn sibling_bin_path(name: &str) -> std::path::PathBuf {
    let harness = std::env::current_exe().expect("current_exe");
    let ambient_target_env = std::env::var("CARGO_BUILD_TARGET").ok();
    let (profile_dir, bin_path, target_dir, triple) =
        sibling_paths(&harness, name, ambient_target_env.as_deref(), |path| {
            path.exists()
        });
    let profile_name = profile_dir.file_name().and_then(std::ffi::OsStr::to_str);
    assert_supported_profile_dir_name(profile_name);
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| panic!("expected {} to have a parent", env!("CARGO_MANIFEST_DIR")))
        .join("Cargo.toml");
    // guarded just above: `profile_name` is exhaustively "debug" or "release" by this point.
    let release = profile_name == Some("release");
    let mut cmd = nested_build_command(
        std::path::Path::new(env!("CARGO")),
        &workspace_root,
        name,
        &target_dir,
        triple.as_deref(),
        release,
    );
    let status = cmd.status().expect("failed to invoke cargo build");
    assert!(
        status.success(),
        "cargo build --manifest-path {workspace_root:?} --bin {name} --target-dir {target_dir:?} \
         --target {triple:?} failed: {status:?}"
    );
    bin_path
}

// removes a scratch file on drop even if an assertion or spawn failure unwinds first —
// mirrors ress/tests/smoke.rs's own TempFile.
pub(crate) struct TempFile(pub(crate) std::path::PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

#[cfg(test)]
mod tests {
    // the exact PR #44 round-7 scenario: a `--target-dir` CLI flag on the OUTER `cargo test`
    // invocation relocates where THIS harness itself lives, but (unlike the CARGO_TARGET_DIR
    // env var) is never exported back to this process's own environment -- sibling resolution
    // must derive the nested build's own target-dir from the harness's REAL, observed path
    // instead of trusting an env var that may not be there at all. See `sibling_bin_path`'s own
    // doc comment (gap 3) for the live repro this synthetic-path test cannot itself cover (that
    // the nested `cargo build` subprocess actually receives and honors the flag).
    //
    // no ambient target env var and no triple segment in these two -- `marker_exists` always
    // answers `true` for the immediate parent, matching "this candidate IS the true root",
    // exactly `detect_triple`'s own no-op path. Pins that the pass-2 RE-review fix didn't
    // disturb the already-correct, already-tested untriple-qualified case.
    #[test]
    fn sibling_paths_follow_the_harness_into_a_relocated_target_dir() {
        let harness = std::path::Path::new("/tmp/relocated/debug/deps/ress_perf-abc123");
        let (profile_dir, bin_path, target_dir, triple) =
            super::sibling_paths(harness, "fake-pager", None, |_| true);
        assert_eq!(
            profile_dir,
            std::path::PathBuf::from("/tmp/relocated/debug")
        );
        assert_eq!(
            bin_path,
            std::path::PathBuf::from("/tmp/relocated/debug/fake-pager")
        );
        assert_eq!(target_dir, std::path::PathBuf::from("/tmp/relocated"));
        assert_eq!(triple, None);
    }

    #[test]
    fn sibling_paths_follow_the_harness_into_a_release_profile_under_the_default_target_dir() {
        // no relocation this time -- the ordinary `<repo>/target/release` layout, pinning that
        // the fix didn't disturb the common, unrelocated case.
        let harness = std::path::Path::new("/repo/target/release/deps/ress_perf-abc123");
        let (profile_dir, bin_path, target_dir, triple) =
            super::sibling_paths(harness, "ress", None, |_| true);
        assert_eq!(
            profile_dir,
            std::path::PathBuf::from("/repo/target/release")
        );
        assert_eq!(
            bin_path,
            std::path::PathBuf::from("/repo/target/release/ress")
        );
        assert_eq!(target_dir, std::path::PathBuf::from("/repo/target"));
        assert_eq!(triple, None);
    }

    // the exact PR #44 pass-2 RE-review scenario, signal 1: the nix sandbox's own checkPhase
    // sets CARGO_BUILD_TARGET, confirmed directly from nixpkgs' cargoCheckHook.sh (see
    // sibling_paths' own doc comment). A triple-qualified layout, ambient env var present and
    // matching the observed path exactly.
    #[test]
    fn sibling_paths_skips_a_triple_segment_confirmed_by_the_ambient_env_var() {
        let harness = std::path::Path::new(
            "/build/source/target/x86_64-unknown-linux-gnu/release/deps/ress_perf-abc123",
        );
        let (profile_dir, bin_path, target_dir, triple) = super::sibling_paths(
            harness,
            "fake-pager",
            Some("x86_64-unknown-linux-gnu"),
            |_| panic!("the env-var signal must short-circuit before any filesystem probe"),
        );
        assert_eq!(
            profile_dir,
            std::path::PathBuf::from("/build/source/target/x86_64-unknown-linux-gnu/release")
        );
        assert_eq!(
            bin_path,
            std::path::PathBuf::from(
                "/build/source/target/x86_64-unknown-linux-gnu/release/fake-pager"
            )
        );
        assert_eq!(target_dir, std::path::PathBuf::from("/build/source/target"));
        assert_eq!(triple, Some("x86_64-unknown-linux-gnu".to_string()));
    }

    // signal 1's own exact-match guard: an env var that does NOT match the observed path's own
    // component must not be TRUSTED as if it described this harness -- but per PR #44 round 15's
    // own fix, "not trusted" means falling through to the filesystem probe (signal 2), not
    // returning `None` outright. This harness's own layout genuinely has no triple (the immediate
    // parent, "target", already carries its own marker), so the probe's own honest answer here
    // still comes out `None` -- this test's own job, after the fix, is pinning that the probe is
    // now REACHED at all on a mismatch, not merely that this specific layout's own answer stays
    // `None` (that half is `sibling_paths_recovers_a_genuine_triple_via_the_marker_probe_despite_a_
    // mismatched_env_var`, just below, which pins the case where falling through actually changes
    // the answer).
    #[test]
    fn sibling_paths_falls_through_to_the_marker_probe_when_the_ambient_env_var_does_not_match() {
        let harness = std::path::Path::new("/repo/target/release/deps/ress_perf-abc123");
        let probed = std::cell::Cell::new(false);
        let (_, _, target_dir, triple) = super::sibling_paths(
            harness,
            "ress",
            Some("aarch64-unknown-linux-musl"),
            |path| {
                probed.set(true);
                path == std::path::Path::new("/repo/target/.rustc_info.json")
            },
        );
        assert!(
            probed.get(),
            "expected the marker probe to run on a mismatch, not be skipped"
        );
        assert_eq!(triple, None);
        assert_eq!(target_dir, std::path::PathBuf::from("/repo/target"));
    }

    // found in PR #44 round 15 (a codex P2): the exact bug this round fixed -- a CLI `--target`
    // on the outer build overrides a stale/global `CARGO_BUILD_TARGET` env var (cargo's own
    // CLI-over-env precedence), so the harness genuinely lives under a DIFFERENT triple than the
    // env var claims. The pre-fix code concluded "no triple" straight from the mismatch and never
    // looked further; the fix falls through to the marker probe, which finds the REAL triple the
    // env var alone could not have revealed.
    #[test]
    fn sibling_paths_recovers_a_genuine_triple_via_the_marker_probe_despite_a_mismatched_env_var() {
        let harness = std::path::Path::new(
            "/build/source/target/x86_64-unknown-linux-gnu/release/deps/ress_perf-abc123",
        );
        let (_, bin_path, target_dir, triple) = super::sibling_paths(
            harness,
            "fake-pager",
            // stale/global env var naming a DIFFERENT triple than the one this harness actually
            // built under.
            Some("aarch64-unknown-linux-musl"),
            |path| path == std::path::Path::new("/build/source/target/.rustc_info.json"),
        );
        assert_eq!(
            triple,
            Some("x86_64-unknown-linux-gnu".to_string()),
            "expected the marker probe to recover the REAL triple, not the mismatched env var's \
             own (wrong) claim of no triple at all"
        );
        assert_eq!(
            bin_path,
            std::path::PathBuf::from(
                "/build/source/target/x86_64-unknown-linux-gnu/release/fake-pager"
            )
        );
        assert_eq!(target_dir, std::path::PathBuf::from("/build/source/target"));
    }

    // signal 2: no ambient env var at all (a plain dev machine with a committed
    // `.cargo/config.toml`'s own `[build] target = ...`, the scenario the real sandbox repro
    // doesn't exercise but the brief asked for a fallback for) -- the `.rustc_info.json` marker
    // probe takes over, confirmed empirically to exist only at the true root (see
    // `detect_triple`'s own doc comment).
    #[test]
    fn sibling_paths_skips_a_triple_segment_confirmed_by_the_marker_file_fallback() {
        let harness = std::path::Path::new(
            "/repo/target/x86_64-unknown-linux-gnu/debug/deps/ress_perf-abc123",
        );
        let (_, bin_path, target_dir, triple) =
            super::sibling_paths(harness, "fake-pager", None, |path| {
                // marker absent at the candidate triple segment, present at the true root --
                // exactly the empirically-confirmed asymmetry `detect_triple` relies on.
                path == std::path::Path::new("/repo/target/.rustc_info.json")
            });
        assert_eq!(
            bin_path,
            std::path::PathBuf::from("/repo/target/x86_64-unknown-linux-gnu/debug/fake-pager")
        );
        assert_eq!(target_dir, std::path::PathBuf::from("/repo/target"));
        assert_eq!(triple, Some("x86_64-unknown-linux-gnu".to_string()));
    }

    // the marker-file fallback's own negative case: no env var, and the immediate parent
    // already has its own marker (the ordinary, untriple-qualified shape) -- must not
    // misdetect a triple that isn't there.
    #[test]
    fn sibling_paths_marker_fallback_finds_no_triple_when_the_immediate_parent_has_its_own_marker()
    {
        let harness = std::path::Path::new("/repo/target/debug/deps/ress_perf-abc123");
        let (_, _, target_dir, triple) =
            super::sibling_paths(harness, "fake-pager", None, |path| {
                path == std::path::Path::new("/repo/target/.rustc_info.json")
            });
        assert_eq!(triple, None);
        assert_eq!(target_dir, std::path::PathBuf::from("/repo/target"));
    }

    // found in PR #44 round 15 (a codex P2): pins that the nested build's own `Command` removes
    // `CARGO_BUILD_TARGET` from its own environment -- the fix for the sibling bug `detect_triple`'s
    // own doc comment describes (a stale env var, with no `--target` flag on this invocation to
    // out-rank it, would otherwise reach cargo itself via ordinary process-environment
    // inheritance). `Command::get_envs()` reports only EXPLICIT `.env`/`.env_remove` calls, never
    // the ambient environment a real spawn would also inherit -- exactly what is needed here,
    // and why this is checkable without ever spawning the command for real. Checked in BOTH the
    // `None` and `Some` triple cases: unconditional, not only reached on the path where it is
    // strictly load-bearing.
    #[test]
    fn nested_build_command_always_removes_the_ambient_target_env_var() {
        for triple in [None, Some("x86_64-unknown-linux-gnu")] {
            let cmd = super::nested_build_command(
                std::path::Path::new("cargo"),
                std::path::Path::new("/repo/Cargo.toml"),
                "fake-pager",
                std::path::Path::new("/repo/target"),
                triple,
                false,
            );
            let removed = cmd.get_envs().any(|(key, value)| {
                key == std::ffi::OsStr::new("CARGO_BUILD_TARGET") && value.is_none()
            });
            assert!(
                removed,
                "expected CARGO_BUILD_TARGET to be explicitly removed regardless of triple \
                 ({triple:?}) -- get_envs() reported: {:?}",
                cmd.get_envs().collect::<Vec<_>>()
            );
        }
    }

    // the two profiles this harness actually supports -- neither should ever panic.
    #[test]
    fn profile_dir_name_guard_accepts_debug_and_release() {
        super::assert_supported_profile_dir_name(Some("debug"));
        super::assert_supported_profile_dir_name(Some("release"));
    }

    // found in PR #44 pass 8 fold re-review (finding 3, scope call): a custom-profile build
    // (e.g. `--profile ci`) is exactly the case the full fix was scoped OUT of -- this pins that
    // it now fails loud instead of silently resolving `sibling_bin_path` to the wrong directory.
    #[test]
    #[should_panic(expected = "only handles the debug/release profiles")]
    fn profile_dir_name_guard_rejects_a_custom_profile_name() {
        super::assert_supported_profile_dir_name(Some("ci"));
    }

    // `profile_dir.file_name()` can only be `None` for a root or empty path -- not a real
    // scenario `sibling_paths`' own `<target-dir>/<profile>` derivation would produce, but the
    // guard's contract is "debug or release, nothing else", so `None` must fail loud too, not
    // slip through some third, unwritten branch.
    #[test]
    #[should_panic(expected = "only handles the debug/release profiles")]
    fn profile_dir_name_guard_rejects_a_missing_name() {
        super::assert_supported_profile_dir_name(None);
    }
}
