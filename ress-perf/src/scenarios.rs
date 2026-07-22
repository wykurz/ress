// the scenario table: `scripts/perf.sh`'s `run_open_scenario`/`run_jump_end_scenario`/
// `run_deep_goto_scenario`/`run_scroll_cadence_scenario`/`run_mount_open_scenario`/
// `run_mount_jump_end_scenario`/`for_each_sample`/`pager_cmd` ported to Rust, driving
// `crate::runner::run_sample` (Tasks 1-2's protocol primitive) once per sample and folding the
// results into `crate::report`'s row types. `main.rs` owns the CLI, fixture generation, and the
// call sequence; this module owns what each scenario/subject/ceiling IS and how one scenario's
// worth of samples turns into table rows.

// resolved once, before any scenario in this module runs -- perf.sh's own RESS_BIN/LESS_BIN
// globals (resolved once in check_preconditions/main(), read by every pager_cmd call
// afterward). Subject::argv_for is a bare `fn` pointer with no closure support (see Subject's
// own doc comment for why), so the resolved paths live here instead, read by the two argv
// functions that need them.
static RESS_BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static LESS_BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Must be called once, before any scenario in this module runs.
pub fn init_binaries(ress_bin: std::path::PathBuf, less_bin: std::path::PathBuf) {
    let _ = RESS_BIN.set(ress_bin);
    let _ = LESS_BIN.set(less_bin);
}

fn ress_bin() -> &'static std::path::Path {
    RESS_BIN
        .get()
        .expect("init_binaries must run before any scenario executes")
        .as_path()
}

fn less_bin() -> &'static std::path::Path {
    LESS_BIN
        .get()
        .expect("init_binaries must run before any scenario executes")
        .as_path()
}

// perf.sh's ENV_WHITELIST_STATIC plus the three HOME-derived entries -- every launch's
// environment is built from exactly these entries (PtyChild::spawn's own env_clear() is the
// "env -i" half of this contract -- see runner.rs's own doc comment), never filtered down from
// whatever this process happened to inherit. Identical for both pagers -- run_sample layers
// RESS_LOG on top under tracing, so this function itself never needs to know about that.
// LESSCHARSET is deliberately absent (redundant with LC_ALL=C.utf8 -- see perf.sh's own
// comment), and HOME/XDG point at a directory that does not need to pre-create ".config"/
// ".local/share" itself: a lookup against a nonexistent path and a lookup against an existing
// directory with no such file both resolve to ENOENT, the identical "not found" outcome either
// pager already handles the same way.
fn whitelist_env(fake_home: &std::path::Path) -> Vec<(String, String)> {
    vec![
        ("TERM".to_string(), "tmux-256color".to_string()),
        ("LC_ALL".to_string(), "C.utf8".to_string()),
        ("LESSHISTFILE".to_string(), "-".to_string()),
        ("LESSKEYIN_SYSTEM".to_string(), "/dev/null".to_string()),
        ("HOME".to_string(), fake_home.display().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            fake_home.join(".config").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            fake_home.join(".local/share").display().to_string(),
        ),
    ]
}

// less always gets -S: ress has no wrap mode at all (only chop), so leaving less's default
// wrap on would give it different, not equivalent, work on the long-line fixtures.
fn less_argv(fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec![
        std::ffi::OsString::from(less_bin()),
        std::ffi::OsString::from("-S"),
        std::ffi::OsString::from(fixture),
    ]
}

fn ress_argv(fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec![
        std::ffi::OsString::from(ress_bin()),
        std::ffi::OsString::from(fixture),
    ]
}

// less's own cold-start jump flag: bare `+N` (equivalent to `+Ng`) placed straight into argv,
// so "painted" and "landed" are the same event for less's deep-goto leg. The target is derived
// from the fixture path alone (its own `.stats` sidecar), not a captured value -- argv_for is a
// bare `fn` pointer with no closure support (see whitelist_env's neighbor, Subject's own doc
// comment) -- and every fixture this is ever called against already has its sidecar written
// before any scenario runs (main.rs's fixture generation), so this is a cheap, already-
// guaranteed-to-succeed re-derivation rather than new I/O risk.
fn less_deep_goto_argv(fixture: &std::path::Path) -> Vec<std::ffi::OsString> {
    let stats = stats_sidecar_path(fixture);
    let lines = read_lines_from_stats(&stats).unwrap_or_else(|err| {
        panic!("reading fixture stats sidecar {stats:?} for deep-goto: {err:#}")
    });
    let target = deep_goto_target(lines);
    vec![
        std::ffi::OsString::from(less_bin()),
        std::ffi::OsString::from("-S"),
        std::ffi::OsString::from(format!("+{target}")),
        std::ffi::OsString::from(fixture),
    ]
}

pub fn less_subject() -> crate::runner::Subject {
    crate::runner::Subject {
        name: "less",
        argv_for: less_argv,
        env_for: whitelist_env,
        tracing: false,
    }
}

fn less_subject_deep_goto() -> crate::runner::Subject {
    crate::runner::Subject {
        name: "less",
        argv_for: less_deep_goto_argv,
        env_for: whitelist_env,
        tracing: false,
    }
}

/// `tracing` is ress-only, and only ever `true` for `open` -- perf.sh never combines `--tracing`
/// with a keyed scenario (tracing is open-only, and open never sends keys).
pub fn ress_subject(tracing: bool) -> crate::runner::Subject {
    crate::runner::Subject {
        name: "ress",
        argv_for: ress_argv,
        env_for: whitelist_env,
        tracing,
    }
}

/// `"${out}.stats"` -- literal suffix concatenation, not extension replacement (a fixture named
/// `varied-log-2g.log` gets `varied-log-2g.log.stats`, not `varied-log-2g.stats`).
pub fn stats_sidecar_path(fixture: &std::path::Path) -> std::path::PathBuf {
    let mut name = fixture.as_os_str().to_owned();
    name.push(".stats");
    std::path::PathBuf::from(name)
}

/// `bytes=<n> lines=<n>` is ress-filegen's whole stderr (see main.rs's fixture generation) --
/// parsed field-by-field, so a malformed sidecar fails loudly instead of silently returning 0.
pub fn read_lines_from_stats(stats: &std::path::Path) -> anyhow::Result<u64> {
    let content = std::fs::read_to_string(stats).map_err(|err| {
        anyhow::Error::new(err).context(format!("reading fixture stats sidecar {stats:?}"))
    })?;
    let lines_field = content
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed fixture stats sidecar {stats:?}: {content:?}"))?;
    let lines_str = lines_field
        .strip_prefix("lines=")
        .ok_or_else(|| anyhow::anyhow!("malformed fixture stats sidecar {stats:?}: {content:?}"))?;
    lines_str.trim().parse::<u64>().map_err(|err| {
        anyhow::Error::new(err).context(format!(
            "parsing line count from fixture stats sidecar {stats:?}"
        ))
    })
}

// zero-padded to 10 digits -- matches ress-filegen's own `{line_no:010}` line-number prefix
// exactly, so a needle built from this always matches real fixture content.
fn line_prefix(line: u64) -> String {
    format!("{line:010}")
}

// 90% of the line count, floored, never below line 1 -- perf.sh's `target=$(( lines * 9 / 10 ))`.
fn deep_goto_target(lines: u64) -> u64 {
    (lines * 9 / 10).max(1)
}

pub fn standard_ceilings() -> crate::runner::Ceilings {
    crate::runner::Ceilings {
        readiness_s: crate::report::PAINT_TIMEOUT_S,
        completion_s: crate::report::SCENARIO_TIMEOUT_S,
    }
}

// less's deep-goto leg has no separate paint-then-land phases: `+N` makes the ONE wait (a
// TopLineStartsWith at process launch) the whole sample, so it is held to the more generous
// scenario ceiling, not the paint one (perf.sh's own `--wait1-ceiling "$SCENARIO_TIMEOUT_S"`
// override).
fn deep_goto_less_ceilings() -> crate::runner::Ceilings {
    crate::runner::Ceilings {
        readiness_s: crate::report::SCENARIO_TIMEOUT_S,
        completion_s: crate::report::SCENARIO_TIMEOUT_S,
    }
}

// open -> first paint. No keys: readiness's own success IS the sample (perf.sh's
// `(( ${#keys[@]} == 0 ))` branch). Shared by both pagers (only the Subject differs) and reused
// as-is by the mount leg (with `prewarm` overridden to `Skip` there -- see run_mount_open).
fn open_scenario() -> crate::runner::Scenario {
    crate::runner::Scenario {
        name: "open",
        keys: Vec::new(),
        readiness: crate::runner::Predicate::Painted,
        completion: None,
        timing_basis: crate::runner::TimingBasis::Launch,
        prewarm: crate::runner::Prewarm::Full,
        // report.rs has no RSS row for open (or its mount-leg reuse, run_mount_open) -- see
        // `Scenario::reports_rss`'s own doc comment (found in PR #44 round 16, a codex P2).
        reports_rss: false,
    }
}

// G to an already-painted pager, timed from the keypress -- isolated from open cost. Shared by
// both pagers and reused by the mount leg (with `timing_basis` and `prewarm` overridden there --
// see run_mount_jump_end for why the mount leg cannot keep the keypress anchor).
fn jump_end_scenario(lines: u64) -> crate::runner::Scenario {
    crate::runner::Scenario {
        name: "jump-end",
        keys: b"G".to_vec(),
        readiness: crate::runner::Predicate::Painted,
        completion: Some(crate::runner::Predicate::Contains(line_prefix(lines))),
        timing_basis: crate::runner::TimingBasis::Keypress,
        prewarm: crate::runner::Prewarm::Full,
        // the ONLY scenario report.rs's RssRow (the jump-to-end table) is ever built from --
        // see `Scenario::reports_rss`'s own doc comment (found in PR #44 round 16, a codex P2).
        // The mount leg's run_mount_jump_end inherits this via `..jump_end_scenario(lines)`.
        reports_rss: true,
    }
}

// less's deep-goto leg: bare +N is baked into argv (less_deep_goto_argv), so this scenario
// sends no keys at all -- readiness's own success (the target line landing at the top) IS the
// sample, timed from process launch by construction (no completion phase to time from a
// keypress instead).
fn deep_goto_scenario_less(target: u64) -> crate::runner::Scenario {
    crate::runner::Scenario {
        name: "deep-goto",
        keys: Vec::new(),
        readiness: crate::runner::Predicate::TopLineStartsWith(line_prefix(target)),
        completion: None,
        timing_basis: crate::runner::TimingBasis::Launch,
        prewarm: crate::runner::Prewarm::Full,
        // report.rs has no RSS row for deep-goto -- see `Scenario::reports_rss`'s own doc
        // comment (found in PR #44 round 16, a codex P2).
        reports_rss: false,
    }
}

// ress has no cold-start jump flag (see this scenario's module-level doc in docs/perf.md's
// "deep-goto's two pagers do not reach line N the same way"): open, wait for first paint, send
// ":$target" + Enter, wait for the same top-line landing. `\r`, not `\n`: a raw-mode terminal's
// physical Enter key transmits CR, which is what tmux's own symbolic "Enter" key name resolves
// to as well (perf.sh's `--keys ":$target" "Enter"`). `--timing-basis launch` keeps the
// reported number "since process launch" for both pagers, so ress's number honestly includes
// its line-1 flash.
fn deep_goto_scenario_ress(target: u64) -> crate::runner::Scenario {
    crate::runner::Scenario {
        name: "deep-goto",
        keys: format!(":{target}\r").into_bytes(),
        readiness: crate::runner::Predicate::Painted,
        completion: Some(crate::runner::Predicate::TopLineStartsWith(line_prefix(
            target,
        ))),
        timing_basis: crate::runner::TimingBasis::Launch,
        prewarm: crate::runner::Prewarm::Full,
        // report.rs has no RSS row for deep-goto -- see `Scenario::reports_rss`'s own doc
        // comment (found in PR #44 round 16, a codex P2).
        reports_rss: false,
    }
}

// 100 j's at full speed from the top, sent as one literal burst (both pagers get the identical
// burst rather than 100 separate writes), landing on line 101. TopLineStableStartsWith, not
// TopLineStartsWith: perf.sh's own defense against a transient/partial redraw landing between
// two polls (see runner.rs's Predicate doc comment).
fn scroll_cadence_scenario() -> crate::runner::Scenario {
    crate::runner::Scenario {
        name: "scroll-cadence",
        keys: vec![b'j'; 100],
        readiness: crate::runner::Predicate::Painted,
        completion: Some(crate::runner::Predicate::TopLineStableStartsWith(
            line_prefix(101),
        )),
        timing_basis: crate::runner::TimingBasis::Keypress,
        prewarm: crate::runner::Prewarm::Full,
        // report.rs has no RSS row for scroll-cadence -- see `Scenario::reports_rss`'s own doc
        // comment (found in PR #44 round 16, a codex P2).
        reports_rss: false,
    }
}

// perf.sh's `prewarm_fixture` -- reads the whole fixture into the page cache before any timed
// sample, so the first sample never pays a cache-fill cost the later ones don't.
fn prewarm(fixture: &std::path::Path) -> anyhow::Result<()> {
    eprintln!("prewarming: {}", fixture.display());
    let mut file = std::fs::File::open(fixture)
        .map_err(|err| anyhow::Error::new(err).context(format!("prewarming {fixture:?}")))?;
    std::io::copy(&mut file, &mut std::io::sink())
        .map_err(|err| anyhow::Error::new(err).context(format!("prewarming {fixture:?}")))?;
    Ok(())
}

fn log_outcome(
    scenario: &str,
    pager: &str,
    i: u32,
    runs: u32,
    outcome: &crate::runner::SampleOutcome,
) {
    match outcome {
        crate::runner::SampleOutcome::Hung { ceiling_s } => {
            eprintln!("   {pager} {scenario} {i}/{runs}: hung (no completion within {ceiling_s}s)");
        }
        crate::runner::SampleOutcome::Crashed => {
            eprintln!("   {pager} {scenario} {i}/{runs}: crashed");
        }
        crate::runner::SampleOutcome::Completed { .. } => {}
    }
}

// perf.sh's `for_each_sample`: runs `runs` samples, alternating which pager goes first per
// sample index, so whichever pager would otherwise systematically benefit from the other's
// residual warmth does not -- the advantage rotates and cancels out across the reported median.
fn for_each_sample(
    runs: u32,
    mut callback: impl FnMut(&'static str, u32) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for i in 1..=runs {
        let order: [&'static str; 2] = if i % 2 == 0 {
            ["ress", "less"]
        } else {
            ["less", "ress"]
        };
        for pager in order {
            callback(pager, i)?;
        }
    }
    Ok(())
}

// one pager's samples for one scenario, accumulated the way report::summarize expects them --
// perf.sh's per-scenario local arrays/counters (less_ms/less_hung_paint/...), gathered into one
// small type instead of four loose variables per pager.
#[derive(Default)]
struct Samples {
    vals: Vec<u64>,
    failures: crate::report::FailureCounts,
}

impl Samples {
    fn record(&mut self, outcome: &crate::runner::SampleOutcome) {
        match *outcome {
            crate::runner::SampleOutcome::Completed { ms, .. } => self.vals.push(ms),
            crate::runner::SampleOutcome::Hung { ceiling_s }
                if ceiling_s == crate::report::PAINT_TIMEOUT_S =>
            {
                self.failures.hung_paint += 1;
            }
            crate::runner::SampleOutcome::Hung { .. } => self.failures.hung_scenario += 1,
            crate::runner::SampleOutcome::Crashed => self.failures.crashed += 1,
        }
    }

    fn row(
        &self,
        scenario: impl Into<String>,
        fixture: impl Into<String>,
        pager: &'static str,
        runs: u64,
    ) -> crate::report::TimingRow {
        crate::report::TimingRow::new(scenario, fixture, pager, self.failures, runs, &self.vals)
    }
}

/// perf.sh's `run_open_scenario`: both pagers, local fixture, prewarmed. ress additionally gets
/// a precise tracing-based number from the same launch (`open-precise`).
pub fn run_open(
    fixture: &std::path::Path,
    label: &str,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
) -> anyhow::Result<()> {
    eprintln!("== open: {label} ==");
    prewarm(fixture)?;
    let scenario = open_scenario();
    let ceilings = standard_ceilings();
    let mut less = Samples::default();
    let mut ress = Samples::default();
    let mut ress_precise_vals = Vec::new();
    for_each_sample(runs, |pager, i| {
        eprintln!("   {pager} open {i}/{runs}");
        let outcome = if pager == "less" {
            crate::runner::run_sample(&less_subject(), &scenario, fixture, &ceilings)?
        } else {
            crate::runner::run_sample(&ress_subject(true), &scenario, fixture, &ceilings)?
        };
        log_outcome("open", pager, i, runs, &outcome);
        if pager == "less" {
            less.record(&outcome);
        } else {
            if let crate::runner::SampleOutcome::Completed {
                precise_ms: Some(p),
                ..
            } = &outcome
            {
                ress_precise_vals.push(*p);
            }
            ress.record(&outcome);
        }
        Ok(())
    })?;
    rows.push(less.row("open", label, "less", u64::from(runs)));
    rows.push(ress.row("open", label, "ress", u64::from(runs)));
    rows.push(crate::report::TimingRow::new_opportunistic(
        "open-precise",
        label,
        "ress",
        ress.failures,
        &ress_precise_vals,
    ));
    Ok(())
}

/// perf.sh's `run_jump_end_scenario`: both pagers, local fixture, prewarmed, RSS sampled right
/// after the jump completes.
pub fn run_jump_end(
    fixture: &std::path::Path,
    label: &str,
    stats: &std::path::Path,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
    rss_rows: &mut Vec<crate::report::RssRow>,
) -> anyhow::Result<()> {
    let lines = read_lines_from_stats(stats)?;
    eprintln!("== jump-end: {label} (last line {lines}) ==");
    prewarm(fixture)?;
    let scenario = jump_end_scenario(lines);
    let ceilings = standard_ceilings();
    let mut less = Samples::default();
    let mut ress = Samples::default();
    let mut less_rss = Vec::new();
    let mut ress_rss = Vec::new();
    for_each_sample(runs, |pager, i| {
        eprintln!("   {pager} jump-end {i}/{runs}");
        let outcome = if pager == "less" {
            crate::runner::run_sample(&less_subject(), &scenario, fixture, &ceilings)?
        } else {
            crate::runner::run_sample(&ress_subject(false), &scenario, fixture, &ceilings)?
        };
        log_outcome("jump-end", pager, i, runs, &outcome);
        let (samples, rss) = if pager == "less" {
            (&mut less, &mut less_rss)
        } else {
            (&mut ress, &mut ress_rss)
        };
        if let crate::runner::SampleOutcome::Completed {
            rss_kib: Some(kib), ..
        } = &outcome
        {
            rss.push(*kib);
        }
        samples.record(&outcome);
        Ok(())
    })?;
    rows.push(less.row("jump-end", label, "less", u64::from(runs)));
    rows.push(ress.row("jump-end", label, "ress", u64::from(runs)));
    rss_rows.push(crate::report::RssRow::new(
        label,
        "less",
        less.failures,
        &less_rss,
    ));
    rss_rows.push(crate::report::RssRow::new(
        label,
        "ress",
        ress.failures,
        &ress_rss,
    ));
    Ok(())
}

/// perf.sh's `run_deep_goto_scenario`: cold-launch jump to 90% of the line count. The two
/// pagers do not reach line N the same way (see docs/perf.md's caveat of the same name) -- less
/// gets its own cold-start jump flag baked into argv; ress pays a paint-then-`:N` handicap.
pub fn run_deep_goto(
    fixture: &std::path::Path,
    label: &str,
    stats: &std::path::Path,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
) -> anyhow::Result<()> {
    let lines = read_lines_from_stats(stats)?;
    let target = deep_goto_target(lines);
    eprintln!("== deep-goto: {label} (target line {target} of {lines}) ==");
    prewarm(fixture)?;
    let less_scenario = deep_goto_scenario_less(target);
    let less_ceilings = deep_goto_less_ceilings();
    let ress_scenario = deep_goto_scenario_ress(target);
    let ress_ceilings = standard_ceilings();
    let mut less = Samples::default();
    let mut ress = Samples::default();
    for_each_sample(runs, |pager, i| {
        eprintln!("   {pager} deep-goto {i}/{runs}");
        let outcome = if pager == "less" {
            crate::runner::run_sample(
                &less_subject_deep_goto(),
                &less_scenario,
                fixture,
                &less_ceilings,
            )?
        } else {
            crate::runner::run_sample(
                &ress_subject(false),
                &ress_scenario,
                fixture,
                &ress_ceilings,
            )?
        };
        log_outcome("deep-goto", pager, i, runs, &outcome);
        if pager == "less" {
            less.record(&outcome)
        } else {
            ress.record(&outcome)
        };
        Ok(())
    })?;
    rows.push(less.row("deep-goto", label, "less", u64::from(runs)));
    rows.push(ress.row("deep-goto", label, "ress", u64::from(runs)));
    Ok(())
}

/// perf.sh's `run_scroll_cadence_scenario`: 100 j's from the top, timed from the keypress.
pub fn run_scroll_cadence(
    fixture: &std::path::Path,
    label: &str,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
) -> anyhow::Result<()> {
    eprintln!("== scroll-cadence: {label} ==");
    prewarm(fixture)?;
    let scenario = scroll_cadence_scenario();
    let ceilings = standard_ceilings();
    let mut less = Samples::default();
    let mut ress = Samples::default();
    for_each_sample(runs, |pager, i| {
        eprintln!("   {pager} scroll-cadence {i}/{runs}");
        let outcome = if pager == "less" {
            crate::runner::run_sample(&less_subject(), &scenario, fixture, &ceilings)?
        } else {
            crate::runner::run_sample(&ress_subject(false), &scenario, fixture, &ceilings)?
        };
        log_outcome("scroll-cadence", pager, i, runs, &outcome);
        if pager == "less" {
            less.record(&outcome)
        } else {
            ress.record(&outcome)
        };
        Ok(())
    })?;
    rows.push(less.row("scroll-cadence", label, "less", u64::from(runs)));
    rows.push(ress.row("scroll-cadence", label, "ress", u64::from(runs)));
    Ok(())
}

/// The mount leg's scenarios that need a fixture never touched by an earlier scenario in the
/// same run -- perf.sh's `MOUNT_FIRST_SCENARIOS`. Each gets its own suffixed file (see
/// `mount_fixture_path`) so one scenario's repeated reads can never warm another's own first
/// sample.
pub const MOUNT_FIRST_SCENARIOS: [&str; 2] = ["open", "jump-end"];

/// The suffixed on-mount path for `scenario`'s own dedicated first-touch fixture -- byte-
/// identical content to every other scenario's copy (same preset/size/seed), just a distinct
/// inode.
pub fn mount_fixture_path(
    mount_fixtures_dir: &std::path::Path,
    scenario: &str,
) -> std::path::PathBuf {
    mount_fixtures_dir.join(format!("varied-log-256m-{scenario}.log"))
}

// mount-first's own value formatting -- unlike summarize()'s multi-sample regime table, a
// single first-touch sample needs no bucketing: hung shows the CEILING THAT ACTUALLY FIRED
// (which can be either ceiling, not just the paint one), crashed shows the bare token; a
// completed sample's own reported ms is supplied by the caller. The precise half of open's
// double report (perf.sh's report_mount_row, called twice per open sample) goes through the
// separate `mount_first_precise` below instead of a second closure parameter here -- found
// necessary in PR #44's round-5 review: `ms` is unconditionally present on every `Completed`
// sample, but `precise_ms` is opportunistic and can legitimately be missing, which needs its
// own "no-data" floor (see `mount_first_precise`'s own doc comment), not a bare `String` a
// caller could paper over with an `unreachable!()` that turned out reachable.
fn mount_first_value(
    outcome: &crate::runner::SampleOutcome,
    completed_value: impl Fn(u64) -> String,
) -> String {
    match *outcome {
        crate::runner::SampleOutcome::Hung { ceiling_s } => format!("hung(>{ceiling_s}s)"),
        crate::runner::SampleOutcome::Crashed => "crashed".to_string(),
        crate::runner::SampleOutcome::Completed { ms, .. } => completed_value(ms),
    }
}

// report_mount_rss_row's own honesty layer for sample 1 specifically: a completed sample whose
// RSS reading raced away contributes nothing (runs=0, value "no-data"), never silently claiming
// a run it has no value for.
fn mount_first_rss(outcome: &crate::runner::SampleOutcome) -> (String, u64) {
    match *outcome {
        crate::runner::SampleOutcome::Hung { ceiling_s } => (format!("hung(>{ceiling_s}s)"), 1),
        crate::runner::SampleOutcome::Crashed => ("crashed".to_string(), 1),
        crate::runner::SampleOutcome::Completed {
            rss_kib: Some(kib), ..
        } => (kib.to_string(), 1),
        crate::runner::SampleOutcome::Completed { rss_kib: None, .. } => ("no-data".to_string(), 0),
    }
}

// mount_first_rss's own twin for the precise reading -- found necessary in PR #44's round-5
// review: the previous version of this leg's "open-precise" row went through `mount_first_value`
// with a closure that `unwrap_or_else(|| unreachable!(...))`'d a missing `precise_ms`, reasoning
// that "ress open runs under tracing=true; a Completed sample always carries precise_ms" --
// exactly backwards, the same opportunism gap `TimingRow::new_opportunistic`'s own doc comment
// names: a completed sample's precise reading can legitimately be missing, and `unreachable!()`
// is a panic waiting to fire the first time one is, not a real invariant. CORRECTED in pass 5
// (sweep): the TRUE remaining reasons, post-pass-3, are an untraced subject or a malformed
// clock relationship -- not "a flush-timeout miss, a lost exec stamp" as this comment used to
// say, since pass 3 made both of those hard failures (an `Err` aborting the run) for a
// still-live traced subject, and a routing to `Crashed` (never `Completed` at all) for a dead
// one -- see `TimingRow::new_opportunistic`'s own doc comment for the full derivation. Mirrors
// `mount_first_rss` exactly: a missing reading on an otherwise-completed sample contributes
// nothing (runs=0, value "no-data"), never a crash and never a panic.
fn mount_first_precise(outcome: &crate::runner::SampleOutcome) -> (String, u64) {
    match *outcome {
        crate::runner::SampleOutcome::Hung { ceiling_s } => (format!("hung(>{ceiling_s}s)"), 1),
        crate::runner::SampleOutcome::Crashed => ("crashed".to_string(), 1),
        crate::runner::SampleOutcome::Completed {
            precise_ms: Some(p),
            ..
        } => (p.to_string(), 1),
        crate::runner::SampleOutcome::Completed {
            precise_ms: None, ..
        } => ("no-data".to_string(), 0),
    }
}

/// perf.sh's `run_mount_open_scenario`: ress-only open, first/warm split -- no prewarm (the
/// whole point of this leg), no pager alternation (nothing to alternate with).
pub fn run_mount_open(
    fixture: &std::path::Path,
    label: &str,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
) -> anyhow::Result<()> {
    eprintln!("== open (mount): {label} ==");
    let scenario = crate::runner::Scenario {
        prewarm: crate::runner::Prewarm::Skip,
        ..open_scenario()
    };
    let ceilings = standard_ceilings();
    let mut warm = Samples::default();
    let mut warm_precise_vals = Vec::new();
    let mut first: Option<crate::runner::SampleOutcome> = None;
    for i in 1..=runs {
        eprintln!("   ress open {i}/{runs}");
        let outcome =
            crate::runner::run_sample(&ress_subject(true), &scenario, fixture, &ceilings)?;
        log_outcome("open (mount)", "ress", i, runs, &outcome);
        if i == 1 {
            first = Some(outcome);
        } else {
            if let crate::runner::SampleOutcome::Completed {
                precise_ms: Some(p),
                ..
            } = &outcome
            {
                warm_precise_vals.push(*p);
            }
            warm.record(&outcome);
        }
    }
    let first = first.expect("runs is always >= 1 -- checked at CLI parse time");
    rows.push(crate::report::TimingRow {
        scenario: "open".to_string(),
        fixture: format!("mount-first:{label}"),
        pager: "ress",
        value: mount_first_value(&first, |ms| ms.to_string()),
        runs: 1,
    });
    let warm_total = u64::from(runs) - 1;
    if warm_total > 0 {
        rows.push(warm.row("open", format!("mount-warm:{label}"), "ress", warm_total));
    }
    let (first_precise_value, first_precise_runs) = mount_first_precise(&first);
    rows.push(crate::report::TimingRow {
        scenario: "open-precise".to_string(),
        fixture: format!("mount-first:{label}"),
        pager: "ress",
        value: first_precise_value,
        runs: first_precise_runs,
    });
    if warm_total > 0 {
        rows.push(crate::report::TimingRow::new_opportunistic(
            "open-precise",
            format!("mount-warm:{label}"),
            "ress",
            warm.failures,
            &warm_precise_vals,
        ));
    }
    Ok(())
}

/// perf.sh's `run_mount_jump_end_scenario`: ress-only jump-end, first/warm split. Unlike the
/// local jump-end scenario, this is timed from process LAUNCH, not the keypress: ress's own
/// startup (Document::new spawning the background indexer, the first viewport read behind the
/// pane's own first paint) already reads this mount-resident file throughout the paint wait,
/// before any key is ever sent -- a keypress anchor would silently exclude that first touch,
/// measuring only a warm continuation of a read this same process already started. The local
/// scenario keeps its keypress anchor because its fixture is explicitly prewarmed first, making
/// startup reads free there; on the mount, where startup reads are exactly what is not free,
/// including them is the correct, equally deliberate choice for this leg.
pub fn run_mount_jump_end(
    fixture: &std::path::Path,
    label: &str,
    stats: &std::path::Path,
    runs: u32,
    rows: &mut Vec<crate::report::TimingRow>,
    rss_rows: &mut Vec<crate::report::RssRow>,
) -> anyhow::Result<()> {
    let lines = read_lines_from_stats(stats)?;
    eprintln!("== jump-end (mount): {label} (last line {lines}) ==");
    let scenario = crate::runner::Scenario {
        timing_basis: crate::runner::TimingBasis::Launch,
        prewarm: crate::runner::Prewarm::Skip,
        ..jump_end_scenario(lines)
    };
    let ceilings = standard_ceilings();
    let mut warm = Samples::default();
    let mut warm_rss = Vec::new();
    let mut first: Option<crate::runner::SampleOutcome> = None;
    for i in 1..=runs {
        eprintln!("   ress jump-end {i}/{runs}");
        let outcome =
            crate::runner::run_sample(&ress_subject(false), &scenario, fixture, &ceilings)?;
        log_outcome("jump-end (mount)", "ress", i, runs, &outcome);
        if i == 1 {
            first = Some(outcome);
        } else {
            if let crate::runner::SampleOutcome::Completed {
                rss_kib: Some(kib), ..
            } = &outcome
            {
                warm_rss.push(*kib);
            }
            warm.record(&outcome);
        }
    }
    let first = first.expect("runs is always >= 1 -- checked at CLI parse time");
    rows.push(crate::report::TimingRow {
        scenario: "jump-end".to_string(),
        fixture: format!("mount-first:{label}"),
        pager: "ress",
        value: mount_first_value(&first, |ms| ms.to_string()),
        runs: 1,
    });
    let warm_total = u64::from(runs) - 1;
    if warm_total > 0 {
        rows.push(warm.row(
            "jump-end",
            format!("mount-warm:{label}"),
            "ress",
            warm_total,
        ));
    }
    let (first_rss_value, first_rss_runs) = mount_first_rss(&first);
    rss_rows.push(crate::report::RssRow {
        fixture: format!("mount-first:{label}"),
        pager: "ress",
        value: first_rss_value,
        runs: first_rss_runs,
    });
    if warm_total > 0 {
        rss_rows.push(crate::report::RssRow::new(
            format!("mount-warm:{label}"),
            "ress",
            warm.failures,
            &warm_rss,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // real ress + real less, not fake-pager -- the full local scenario table exercised
    // end-to-end against a small, freshly generated fixture. #[ignore]: spawns real
    // subprocesses and needs both binaries built/on PATH; run explicitly (`cargo nextest run
    // -- --ignored -p ress-perf`), not part of the default `just ci` gate (which uses
    // fake-pager throughout precisely so it never needs a release ress build).
    fn write_small_fixture() -> (
        crate::test_support::TempFile,
        crate::test_support::TempFile,
        std::path::PathBuf,
    ) {
        let path = std::env::temp_dir().join(format!(
            "ress-perf-scenarios-smoke-{}.log",
            std::process::id()
        ));
        let spec = ress_filegen::Spec {
            seed: 42,
            target_bytes: 200_000,
            line_len: ress_filegen::LineLen::Uniform { min: 10, max: 200 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.1,
        };
        let mut file = std::fs::File::create(&path).unwrap();
        let stats = ress_filegen::generate(&spec, &mut file).unwrap();
        std::io::Write::flush(&mut file).unwrap();
        let stats_path = super::stats_sidecar_path(&path);
        std::fs::write(
            &stats_path,
            format!("bytes={} lines={}\n", stats.bytes, stats.lines),
        )
        .unwrap();
        (
            crate::test_support::TempFile(path.clone()),
            crate::test_support::TempFile(stats_path),
            path,
        )
    }

    #[test]
    #[ignore = "spawns real ress and less; run explicitly, e.g. as a full-scale smoke check before a release"]
    fn real_ress_and_less_complete_the_full_local_scenario_table() {
        let (_fixture_guard, _stats_guard, fixture) = write_small_fixture();
        super::init_binaries(
            crate::test_support::sibling_bin_path("ress"),
            crate::test_support::on_path("less"),
        );
        let stats = super::stats_sidecar_path(&fixture);
        let mut rows = Vec::new();
        let mut rss_rows = Vec::new();
        super::run_open(&fixture, "smoke.log", 2, &mut rows).unwrap();
        super::run_jump_end(&fixture, "smoke.log", &stats, 2, &mut rows, &mut rss_rows).unwrap();
        super::run_deep_goto(&fixture, "smoke.log", &stats, 2, &mut rows).unwrap();
        super::run_scroll_cadence(&fixture, "smoke.log", 2, &mut rows).unwrap();
        // open (less, ress, open-precise) + jump-end (less, ress) + deep-goto (less, ress) +
        // scroll-cadence (less, ress).
        let values: Vec<&str> = rows.iter().map(|row| row.value.as_str()).collect();
        assert_eq!(rows.len(), 9, "unexpected row count, values: {values:?}");
        for row in &rows {
            assert!(
                row.value.parse::<u64>().is_ok(),
                "expected a clean numeric value for {}/{}/{}, got: {}",
                row.scenario,
                row.fixture,
                row.pager,
                row.value
            );
            assert_eq!(
                row.runs, 2,
                "{}/{}/{}",
                row.scenario, row.fixture, row.pager
            );
        }
        for row in &rss_rows {
            assert_eq!(row.runs, 2, "{}/{}", row.fixture, row.pager);
        }
        // the report layer renders without panicking and produces the expected schema shape --
        // not an exact column-width match, since a wide value (e.g. "scroll-cadence") legitimately
        // widens its column beyond the header's own length.
        let table = crate::report::render_table(&rows, &rss_rows);
        assert!(table.starts_with("scenario"), "table:\n{table}");
        assert!(
            table.contains("RSS after jump-to-end (KiB):"),
            "table:\n{table}"
        );
        let tsv = crate::report::render_tsv(&rows, &rss_rows);
        assert!(
            tsv.starts_with("scenario\tfixture\tpager\tmedian_ms\truns\n"),
            "tsv:\n{tsv}"
        );
    }
}
