// the report layer: `scripts/perf.sh`'s `summarize`/`print_aligned_table`/`print_results`
// ported to Rust -- formats one pager's samples into the exact regime token perf.sh's own
// `summarize` produces (see docs/perf.md's "Hangs and crashes are recorded, not fatal"
// section), and renders the timing/rss tables both as an aligned human-readable table and a
// tab-separated file, byte-compatible with perf.sh's own schema so a later parity run can diff
// the two harnesses' outputs directly.

/// perf.sh's `PAINT_TIMEOUT_S` -- the readiness-wait ceiling every scenario's first paint is
/// held to. First paint must be fast regardless of fixture size, so a short timeout doubles as
/// a harness-health check, not just a safety net.
pub const PAINT_TIMEOUT_S: u64 = 30;
/// perf.sh's `SCENARIO_TIMEOUT_S` -- the completion-wait ceiling. Generous on purpose so a
/// slow-but-real scan on a huge fixture is never mistaken for a hang.
pub const SCENARIO_TIMEOUT_S: u64 = 240;

/// How many of a scenario's samples hung (bucketed by which ceiling fired) or crashed --
/// perf.sh's own `hung_paint`/`hung_scenario`/`crashed` counts, bundled into one type so
/// `TimingRow::new`/`RssRow::new` do not each need three loose `u64` parameters.
#[derive(Debug, Clone, Copy, Default)]
pub struct FailureCounts {
    pub hung_paint: u64,
    pub hung_scenario: u64,
    pub crashed: u64,
}

/// One row of the timing table: a scenario/fixture/pager triple and its summarized value.
pub struct TimingRow {
    pub scenario: String,
    pub fixture: String,
    pub pager: &'static str,
    pub value: String,
    pub runs: u64,
}

impl TimingRow {
    /// `runs` is the scenario's own raw sample count (perf.sh's `$RUNS`) -- every sample must
    /// land in exactly one of `vals`/`failures`, an invariant `summarize` itself checks.
    pub fn new(
        scenario: impl Into<String>,
        fixture: impl Into<String>,
        pager: &'static str,
        failures: FailureCounts,
        runs: u64,
        vals: &[u64],
    ) -> TimingRow {
        TimingRow {
            scenario: scenario.into(),
            fixture: fixture.into(),
            pager,
            value: summarize(
                failures.hung_paint,
                failures.hung_scenario,
                failures.crashed,
                runs,
                vals,
            ),
            runs,
        }
    }

    fn fields(&self) -> [String; 5] {
        [
            self.scenario.clone(),
            self.fixture.clone(),
            self.pager.to_string(),
            self.value.clone(),
            self.runs.to_string(),
        ]
    }

    /// Like `new`, but for an opportunistic secondary timing metric (the tracing-precise
    /// number) that -- unlike the plain `ms` every `Completed` sample always carries -- can be
    /// missing even on a sample that otherwise completed cleanly.
    ///
    /// found in PR #44 pass 5 (sweep): the two TRUE remaining no-data paths, corrected here
    /// post-pass-3 -- an untraced subject (`subject.tracing` was false, so no capture was ever
    /// attempted) and a malformed clock relationship (`checked_sub` underflow between the exec
    /// stamp and the tracing log line, most plausibly a small clock adjustment landing between
    /// two same-machine reads). Two OTHER reasons a missing reading used to land here are gone
    /// as of pass 3, not merely renamed: a missing exec stamp or an unflushed log line on a
    /// STILL-LIVE traced subject are now hard failures (`tracing_precise_ms` propagates an
    /// `Err`, aborting the whole run -- the stamp pipe and the log line are both harness-owned
    /// plumbing, so their absence in front of a confirmed paint is broken instrumentation, not
    /// a per-sample race) rather than a degraded-but-still-`Completed` reading; either cause on
    /// a subject that had already DIED by the time it was checked routes the whole sample to
    /// `SampleOutcome::Crashed` instead, never reaching `Completed` with a missing precise
    /// number at all. `runs` is therefore not a caller-supplied
    /// parameter here, exactly like `RssRow::new`'s own reasoning: this row's own `runs` is
    /// however many samples actually contributed a value or a hang/crash, which can be less
    /// than the scenario's raw sample count for the identical underlying samples -- passing the
    /// raw count instead (as every call site here did until PR #44's round-5 review) leaves
    /// `summarize`'s every-sample-accounted assertion short whenever a completed sample's
    /// precise reading is missing, panicking the run one layer up from the abort that
    /// opportunism was supposed to prevent in the first place. A completed-but-unreadable
    /// precise reading is a timing success with a missing secondary number, never a failure --
    /// zero contributing readings shows `no-data`, the same honest floor `RssRow::new` already
    /// gives a fully-raced metric.
    pub fn new_opportunistic(
        scenario: impl Into<String>,
        fixture: impl Into<String>,
        pager: &'static str,
        failures: FailureCounts,
        vals: &[u64],
    ) -> TimingRow {
        let runs =
            failures.hung_paint + failures.hung_scenario + failures.crashed + vals.len() as u64;
        TimingRow::new(scenario, fixture, pager, failures, runs, vals)
    }
}

/// One row of the RSS-after-jump-to-end table -- no scenario column (perf.sh's `add_rss_row`
/// only ever reports this one metric).
pub struct RssRow {
    pub fixture: String,
    pub pager: &'static str,
    pub value: String,
    pub runs: u64,
}

impl RssRow {
    /// Unlike `TimingRow::new`, `runs` is not a caller-supplied parameter: an RSS reading is
    /// opportunistic and can race the subject's own exit (perf.sh's `report_rss_row`), so this
    /// row's own `runs` is however many samples actually contributed a value or a hang/crash --
    /// which can be less than the timing row's own `runs` for the identical underlying samples,
    /// never the raw sample count standing in for a median it was not actually computed over.
    pub fn new(
        fixture: impl Into<String>,
        pager: &'static str,
        failures: FailureCounts,
        vals: &[u64],
    ) -> RssRow {
        let total =
            failures.hung_paint + failures.hung_scenario + failures.crashed + vals.len() as u64;
        RssRow {
            fixture: fixture.into(),
            pager,
            value: summarize(
                failures.hung_paint,
                failures.hung_scenario,
                failures.crashed,
                total,
                vals,
            ),
            runs: total,
        }
    }

    fn fields(&self) -> [String; 4] {
        [
            self.fixture.clone(),
            self.pager.to_string(),
            self.value.clone(),
            self.runs.to_string(),
        ]
    }
}

// perf.sh's `median` -- floor-averaged for an even sample count (the usual convention). Every
// summarize() call site below only ever reaches this with a non-empty `vals` (see summarize's
// own branches), so an empty slice here is a caller bug, not a reachable regime.
fn median(vals: &[u64]) -> u64 {
    assert!(!vals.is_empty(), "median called with no samples");
    let mut sorted = vals.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    }
}

/// perf.sh's `summarize` -- see docs/perf.md's "Hangs and crashes are recorded, not fatal" for
/// the regime table this implements: no samples at all (`total == 0`) -> the literal token
/// `no-data`; zero failures -> the plain median, unchanged from before hangs were tracked; a
/// majority of samples failed (hung, crashed, or both) -> a token naming every cause that fired
/// -- bare when the whole majority shares one cause, bucketed with per-cause counts otherwise,
/// since a per-cause count would be redundant with `runs` when only one cause fired; a minority
/// failed -> the median of the samples that did complete, annotated with how many hung and/or
/// crashed, so no real number is ever silently absorbed into another.
pub fn summarize(
    hung_paint: u64,
    hung_scenario: u64,
    crashed: u64,
    total: u64,
    vals: &[u64],
) -> String {
    assert_eq!(
        total,
        hung_paint + hung_scenario + crashed + vals.len() as u64,
        "every sample must be accounted for in exactly one bucket"
    );
    let hung = hung_paint + hung_scenario;
    let failed = hung + crashed;
    if total == 0 {
        return "no-data".to_string();
    }
    if failed == 0 {
        return median(vals).to_string();
    }
    if failed * 2 > total {
        if crashed == 0 && hung_paint > 0 && hung_scenario == 0 {
            return format!("hung(>{PAINT_TIMEOUT_S}s)");
        }
        if crashed == 0 && hung_scenario > 0 && hung_paint == 0 {
            return format!("hung(>{SCENARIO_TIMEOUT_S}s)");
        }
        if hung == 0 {
            return "crashed".to_string();
        }
        let mut parts = Vec::new();
        if hung_paint > 0 {
            parts.push(format!(">{PAINT_TIMEOUT_S}s:{hung_paint}"));
        }
        if hung_scenario > 0 {
            parts.push(format!(">{SCENARIO_TIMEOUT_S}s:{hung_scenario}"));
        }
        if crashed > 0 {
            parts.push(format!("crashed:{crashed}"));
        }
        return format!("hung({})", parts.join(","));
    }
    if crashed == 0 {
        format!("{}({hung}h)", median(vals))
    } else if hung == 0 {
        format!("{}({crashed}c)", median(vals))
    } else {
        format!("{}({hung}h,{crashed}c)", median(vals))
    }
}

// one row's fields, left-justified to each column's own width (computed across the whole
// table, header included) with a fixed two-space gutter after every field, including the
// last -- perf.sh's print_aligned_table has no special case trimming trailing whitespace.
fn aligned_row(out: &mut String, fields: &[String], widths: &[usize]) {
    for (field, width) in fields.iter().zip(widths) {
        out.push_str(field);
        for _ in field.len()..*width {
            out.push(' ');
        }
        out.push_str("  ");
    }
    out.push('\n');
}

fn aligned_table<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> String {
    let mut widths: [usize; N] = header.map(str::len);
    for row in rows {
        for i in 0..N {
            widths[i] = widths[i].max(row[i].len());
        }
    }
    let mut out = String::new();
    aligned_row(&mut out, &header.map(str::to_string), &widths);
    for row in rows {
        aligned_row(&mut out, row, &widths);
    }
    out
}

/// perf.sh's `print_results`' stdout half -- the timing table followed by the RSS table, both
/// aligned, human-readable.
pub fn render_table(rows: &[TimingRow], rss_rows: &[RssRow]) -> String {
    let timing_fields: Vec<[String; 5]> = rows.iter().map(TimingRow::fields).collect();
    let rss_fields: Vec<[String; 4]> = rss_rows.iter().map(RssRow::fields).collect();
    let mut out = aligned_table(
        ["scenario", "fixture", "pager", "median_ms", "runs"],
        &timing_fields,
    );
    out.push('\n');
    out.push_str("RSS after jump-to-end (KiB):\n");
    out.push_str(&aligned_table(
        ["fixture", "pager", "median_kib", "runs"],
        &rss_fields,
    ));
    out
}

/// perf.sh's `print_results`' file half -- byte-compatible with its own `last-run.tsv` schema:
/// the column set, ordering, and the blank-line-then-comment separator between the two
/// sections all match `scripts/perf.sh`'s own `print_results` exactly.
pub fn render_tsv(rows: &[TimingRow], rss_rows: &[RssRow]) -> String {
    let mut out = String::new();
    out.push_str("scenario\tfixture\tpager\tmedian_ms\truns\n");
    for row in rows {
        out.push_str(&row.fields().join("\t"));
        out.push('\n');
    }
    out.push_str("\n# RSS after jump-to-end (KiB) — median_kib, not median_ms\n");
    out.push_str("fixture\tpager\tmedian_kib\truns\n");
    for row in rss_rows {
        out.push_str(&row.fields().join("\t"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_samples_reports_no_data() {
        assert_eq!(summarize(0, 0, 0, 0, &[]), "no-data");
    }

    #[test]
    fn all_completed_reports_the_plain_median_odd_count() {
        assert_eq!(summarize(0, 0, 0, 3, &[10, 30, 20]), "20");
    }

    #[test]
    fn all_completed_reports_the_plain_median_even_count() {
        assert_eq!(summarize(0, 0, 0, 2, &[10, 20]), "15");
    }

    #[test]
    fn majority_hung_at_paint_ceiling_reports_the_bare_token() {
        assert_eq!(summarize(2, 0, 0, 3, &[100]), "hung(>30s)");
    }

    #[test]
    fn majority_hung_at_scenario_ceiling_reports_the_bare_token() {
        assert_eq!(summarize(0, 2, 0, 3, &[100]), "hung(>240s)");
    }

    #[test]
    fn majority_crashed_reports_the_bare_token() {
        assert_eq!(summarize(0, 0, 2, 3, &[100]), "crashed");
    }

    #[test]
    fn majority_hung_at_both_ceilings_reports_bucketed_counts() {
        assert_eq!(summarize(1, 1, 0, 2, &[]), "hung(>30s:1,>240s:1)");
    }

    #[test]
    fn majority_hung_and_crashed_reports_bucketed_counts() {
        assert_eq!(summarize(1, 0, 1, 2, &[]), "hung(>30s:1,crashed:1)");
    }

    #[test]
    fn majority_every_cause_reports_bucketed_counts_in_order() {
        assert_eq!(summarize(1, 1, 1, 3, &[]), "hung(>30s:1,>240s:1,crashed:1)");
    }

    #[test]
    fn minority_hung_annotates_the_median_of_completed_samples() {
        assert_eq!(summarize(1, 0, 0, 3, &[10, 20]), "15(1h)");
    }

    #[test]
    fn minority_crashed_annotates_the_median_of_completed_samples() {
        assert_eq!(summarize(0, 0, 1, 3, &[10, 20]), "15(1c)");
    }

    #[test]
    fn minority_hung_and_crashed_annotates_both_counts() {
        assert_eq!(summarize(1, 0, 1, 5, &[10, 20, 30]), "20(1h,1c)");
    }

    #[test]
    fn exactly_half_failed_is_the_minority_regime_not_majority() {
        // failed * 2 > total is a strict inequality -- an exact half stays in the minority
        // (annotated-median) branch, not the majority-token one.
        assert_eq!(summarize(2, 0, 0, 4, &[10, 20]), "15(2h)");
    }

    #[test]
    #[should_panic(expected = "every sample must be accounted for")]
    fn asserts_every_sample_is_accounted_for_exactly_once() {
        summarize(1, 0, 0, 5, &[10, 20]); // 1 + 0 + 0 + 2 != 5
    }

    #[test]
    fn rss_row_runs_counts_only_contributing_samples_not_the_raw_run_count() {
        // e.g. 3 samples completed the jump, but only 2 RSS readings actually landed -- the
        // third raced the pager's own exit (report_rss_row's own honesty layer: a raced-away
        // reading is not silently absorbed into a run count it has no value for).
        let row = RssRow::new("f.log", "ress", FailureCounts::default(), &[1000, 2000]);
        assert_eq!(row.runs, 2);
        assert_eq!(row.value, "1500");
    }

    #[test]
    fn rss_row_reports_no_data_when_every_reading_raced_away() {
        let row = RssRow::new("f.log", "ress", FailureCounts::default(), &[]);
        assert_eq!(row.value, "no-data");
        assert_eq!(row.runs, 0);
    }

    // pins the actual PR #44 round-5 bug: passing the scenario's raw sample count as `runs`
    // (rather than deriving it from failures+vals, as `new_opportunistic` now does) panics the
    // moment a completed sample's precise reading is missing -- exactly the shape every
    // `TimingRow::new("open-precise", ...)` call site used before this round.
    #[test]
    #[should_panic(expected = "every sample must be accounted for")]
    fn calling_new_directly_with_the_raw_run_count_panics_on_a_missing_opportunistic_reading() {
        // 3 samples completed, but only 2 precise readings landed -- the third's tracing log
        // line never appeared within the flush timeout (a timing success, not a failure), so
        // neither `failures` nor `vals` accounts for it.
        TimingRow::new(
            "open-precise",
            "f.log",
            "ress",
            FailureCounts::default(),
            3,
            &[10, 20],
        );
    }

    #[test]
    fn opportunistic_timing_row_counts_only_contributing_samples_not_a_caller_supplied_total() {
        let row = TimingRow::new_opportunistic(
            "open-precise",
            "f.log",
            "ress",
            FailureCounts::default(),
            &[10, 20],
        );
        assert_eq!(row.runs, 2);
        assert_eq!(row.value, "15");
    }

    #[test]
    fn opportunistic_timing_row_reports_no_data_when_every_reading_is_missing() {
        let row = TimingRow::new_opportunistic(
            "open-precise",
            "f.log",
            "ress",
            FailureCounts::default(),
            &[],
        );
        assert_eq!(row.value, "no-data");
        assert_eq!(row.runs, 0);
    }

    #[test]
    fn opportunistic_timing_row_still_annotates_real_hangs_and_crashes() {
        // failures are never opportunistic -- a hung/crashed sample contributed no precise
        // reading for a REAL reason (there was no completed sample to read one from), unlike a
        // completed sample's merely-missing reading. `new_opportunistic` must fold these into
        // its own derived `runs` exactly as `new` would, not silently drop them.
        let failures = FailureCounts {
            hung_paint: 1,
            crashed: 1,
            ..Default::default()
        };
        let row =
            TimingRow::new_opportunistic("open-precise", "f.log", "ress", failures, &[10, 20]);
        assert_eq!(row.runs, 4);
        assert_eq!(row.value, "15(1h,1c)");
    }

    #[test]
    fn tsv_matches_perf_sh_schema_byte_for_byte() {
        let rows = vec![
            TimingRow::new(
                "open",
                "varied-log-2g.log",
                "less",
                FailureCounts::default(),
                2,
                &[10, 20],
            ),
            TimingRow::new(
                "open",
                "varied-log-2g.log",
                "ress",
                FailureCounts::default(),
                2,
                &[8, 12],
            ),
        ];
        let rss_rows = vec![RssRow::new(
            "varied-log-2g.log",
            "ress",
            FailureCounts::default(),
            &[102_400, 98_304],
        )];
        let tsv = render_tsv(&rows, &rss_rows);
        assert_eq!(
            tsv,
            "scenario\tfixture\tpager\tmedian_ms\truns\n\
             open\tvaried-log-2g.log\tless\t15\t2\n\
             open\tvaried-log-2g.log\tress\t10\t2\n\
             \n\
             # RSS after jump-to-end (KiB) — median_kib, not median_ms\n\
             fixture\tpager\tmedian_kib\truns\n\
             varied-log-2g.log\tress\t100352\t2\n"
        );
    }

    #[test]
    fn table_pads_columns_to_the_widest_value_with_a_two_space_gutter() {
        let rows = vec![TimingRow::new(
            "open",
            "f.log",
            "less",
            FailureCounts::default(),
            1,
            &[5],
        )];
        let table = render_table(&rows, &[]);
        let first_line = table.lines().next().unwrap();
        assert_eq!(first_line, "scenario  fixture  pager  median_ms  runs  ");
    }

    #[test]
    fn table_widens_a_column_to_fit_a_value_longer_than_its_header() {
        let fixture = "verylongfixture.log"; // 20 chars, wider than the "fixture" header (7)
        let rows = vec![TimingRow::new(
            "open",
            fixture,
            "less",
            FailureCounts::default(),
            1,
            &[5],
        )];
        let table = render_table(&rows, &[]);
        let expected_header = format!(
            "scenario  fixture{}  pager  median_ms  runs  ",
            " ".repeat(fixture.len() - "fixture".len())
        );
        assert_eq!(table.lines().next().unwrap(), expected_header);
    }
}
