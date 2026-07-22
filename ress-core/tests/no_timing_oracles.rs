//! Mechanical guard against the timing/silence-oracle test-footgun class this pass (U1-U8,
//! pass 8) exists to close -- see `AGENTS.md`'s own "no scheduler oracles" rule; this test
//! mechanically enforces it, not replaces it. Three specific, NAMED shapes are checked, not
//! every conceivable reincarnation of the underlying idea -- heuristic but real, per the plan's
//! own framing (`.superpowers/sdd/pr44-pass8-audit.md`'s own U8), not a full static analyzer.
//! Checks 2 and 3 below are deliberately scoped to avoid flagging the 30+ sound loop-wrapped
//! diagnostic-ceiling sites and the latency-bound crash-promptness/RSS-ceiling checks this same
//! sweep left alone.
//!
//! The scoping in check 3 (`#[test]`/`#[tokio::test]` function bodies only) has a real,
//! confirmed blind spot, not a hypothetical one (p6-review2, pass 8 U-guard review): a sleep
//! hidden inside a SHARED TEST-SUPPORT HELPER -- a plain function called BY test bodies but not
//! itself `#[test]`-attributed -- is invisible to this guard, footgun-shaped or not, since it
//! never lexically sits inside an attributed function at all. This is a live, recurring idiom
//! in this workspace, not a one-off: `pty.rs`'s own `drain_to_eof` (cited again below, check
//! 3's own violation message, as this workspace's OWN exemplar of the sound loop-wrapped
//! idiom -- the same function is both) and `screen.rs`'s own `capture_for` are both exactly
//! this shape today, and p6-review2 confirmed the gap empirically, not by inspection alone --
//! temporarily de-looping each into the raw "sleep once, then check" footgun left this guard
//! silent both times. The equivalent gap already existed for a MOCK METHOD before
//! U-failingslow gated `cache.rs`'s own `FailingSlow` (an inline sleep inside a non-attributed
//! trait impl, not a free function, but the same underlying reason this guard cannot see it:
//! nothing here lexically sits inside an attributed test). A reintroduced footgun in either
//! shape needs a human reviewer -- this guard is not, and cannot structurally be, a substitute
//! for that review for shared-helper or mock-impl sleeps; it exists to make the COMMON,
//! already-seen-twice regressions living DIRECTLY inside test bodies (findings 5 and 6 of that
//! audit) fail loudly and immediately instead of waiting for a third recurrence.
//!
//! The same disclosure covers bounded YIELD-LOOP absence windows (`for _ in 0..N {
//! yield_now().await }` followed by a state assert): the same absence-inference family, and
//! deliberately NOT scanned -- the oracle-hardening campaign is closed (AGENTS.md, 2026-07-22
//! policy), and the live sites of this shape are policy-accepted residuals, each carrying an
//! `ACCEPTED-RESIDUAL:` marker naming why no positive terminal signal can exist for them.
//!
//! The three checks, each independent, each producing its own violation list:
//!
//! 1. `with_latency` must never exist again. `MockSource::with_latency` was deleted entirely
//!    (U-delete): a fixed per-read sleep tests kept reaching for as a COORDINATION shortcut --
//!    race a second concurrent thing against "N ms is definitely enough" -- not the armable gate
//!    this whole pass replaced it with. Checked by definition (`fn with_latency`), not call site:
//!    sidesteps comment collisions entirely (several comments in this tree, written recording
//!    this very deletion, literally contain the text `.with_latency(Duration::from_millis(5))`
//!    as history) and Rust's own compiler already makes a bare call site impossible without the
//!    definition existing somewhere too.
//!
//! 2. `.is_err()` must never chain DIRECTLY off a `timeout(...).await` -- the absence-inference
//!    shape: "nothing happened" concluded purely from a bounded wait timing out, no positive
//!    signal backing it up. Deliberately narrow, chained-off-the-SAME-expression only: an
//!    `.is_err()` on a variable extracted several lines after an EARLIER timeout was already
//!    `.expect()`'d (`source.rs`'s own
//!    `forgetting_to_release_the_gate_fails_loud_within_the_bound`, which checks whether the
//!    spawned task itself panicked -- a genuine positive fact, not the timeout's own outcome)
//!    must not be flagged, and is not: only a DIRECT chain off `timeout(...).await` counts.
//!
//! 3. A loop-less `sleep(` inside a `#[test]`/`#[tokio::test]` function body -- sleep a fixed
//!    duration, then check state once, hoping the duration was long enough. The established,
//!    reviewed-sound idiom used 30+ times across this workspace (pty.rs, runner.rs, smoke.rs) is
//!    always `loop { <real check>; if met { break }; assert!(deadline); sleep(short) }` -- a
//!    diagnostic ceiling wrapped around a genuine per-iteration positive check, the real-
//!    subprocess analogue of the gate's own `DIAGNOSTIC_CEILING` bound. The footgun this checks
//!    for is never loop-wrapped. Scoped strictly to `#[test]`/`#[tokio::test]` function bodies:
//!    this alone excludes production code (`status.rs`'s own backoff sleep,
//!    `settled_rss_kib`'s own pacing sleep -- neither lexically sits inside an attributed test
//!    fn) and test-double binaries (`fake-pager.rs` has no `#[test]` attributes anywhere, being
//!    its own separate binary crate root, not a test). A `// TIMING-EXEMPT: <reason>` comment on
//!    the line immediately before a flagged `sleep(` opts it out -- an escape hatch for a
//!    genuine one-shot, real-time-bound case that doesn't fit the loop-wrapped shape, checked
//!    here but not retrofitted onto any current site (none currently need it).
//!
//! Implementation note: hand-rolled character-level scanning, not a full Rust parser and no
//! `regex` dependency (not already a workspace dependency anywhere; not worth adding for this).
//! Comments are stripped line-by-line before structural matching (checks 2 and 3) specifically
//! so this file's OWN doc comments above, which necessarily describe the exact patterns being
//! matched, can never trip the guard on themselves.

use std::path::{Path, PathBuf};

/// Crates this guard scans; `ress-filegen` (a fixture byte-generator, no async/timing surface
/// at all) is deliberately excluded rather than scanned-and-trivially-clean every run.
const SCANNED_CRATES: &[&str] = &["ress-core", "ress-perf", "ress"];
const SCANNED_SUBDIRS: &[&str] = &["src", "tests", "benches"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ress-core's own Cargo.toml is one level below the workspace root")
        .to_path_buf()
}

fn collect_rs_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for crate_name in SCANNED_CRATES {
        for subdir in SCANNED_SUBDIRS {
            let dir = root.join(crate_name).join(subdir);
            if dir.is_dir() {
                walk(&dir, &mut files);
            }
        }
    }
    // this file itself, excluded: its own diagnostic message strings necessarily contain the
    // literal patterns being checked for (as DATA describing what was found, e.g. the text "fn
    // with_latency" inside a format! string), which is not the same thing as actually declaring
    // that function or writing that shape -- a false positive on the checker's own
    // implementation, not a real instance of any footgun. Self-exclusion, not a blind spot: this
    // file contains no test code of its own beyond `no_timing_oracles` itself, which has no
    // `sleep`/`timeout`/`with_latency` of any kind to hide. Built from `CARGO_MANIFEST_DIR`
    // directly (always absolute) rather than `file!()` (relative, and ambiguous relative to
    // what) to match `files`' own absolute paths unambiguously.
    let self_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("no_timing_oracles.rs");
    files.retain(|f| f != &self_path);
    files.sort();
    files
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strips a `//` line comment. Naive (does not account for `//` occurring inside a string
/// literal), an accepted heuristic gap: this codebase's own style never writes a `//` inside a
/// string literal on a line that also contains the structural patterns checks 2/3 look for.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_word_boundary_before(chars: &[char], pos: usize) -> bool {
    pos == 0 || !is_ident_char(chars[pos - 1])
}

fn is_word_boundary_after(chars: &[char], pos: usize) -> bool {
    pos >= chars.len() || !is_ident_char(chars[pos])
}

fn matches_at(chars: &[char], pos: usize, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    pos + pat.len() <= chars.len() && chars[pos..pos + pat.len()] == pat[..]
}

fn skip_ws(chars: &[char], mut pos: usize) -> usize {
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    pos
}

/// `chars[open]` must be `(`; returns the index of its matching `)`, if the parens balance
/// before the slice ends.
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    debug_assert_eq!(chars[open], '(');
    let mut depth = 0i32;
    for (offset, &c) in chars[open..].iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// A comment-stripped, whitespace-flattened (all newlines become a single space) view of a
/// file, alongside a parallel `line_of` vector mapping each char in `flat` back to its original
/// 1-based line number -- lets checks 2/3 match a pattern that rustfmt may have wrapped across
/// several lines while still reporting an accurate line number.
struct Flattened {
    chars: Vec<char>,
    line_of: Vec<usize>,
}

fn flatten(content: &str) -> Flattened {
    let mut chars = Vec::new();
    let mut line_of = Vec::new();
    for (i, line) in content.lines().enumerate() {
        for ch in strip_line_comment(line).chars() {
            chars.push(ch);
            line_of.push(i + 1);
        }
        chars.push(' ');
        line_of.push(i + 1);
    }
    Flattened { chars, line_of }
}

// ---- check 1: `with_latency` must never exist again (by definition, not call site) ----

fn check_with_latency_definition(path: &Path, content: &str, violations: &mut Vec<String>) {
    for (i, line) in content.lines().enumerate() {
        let stripped = strip_line_comment(line);
        if let Some(at) = stripped.find("fn with_latency") {
            let chars: Vec<char> = stripped.chars().collect();
            let name_end = at + "fn with_latency".len();
            let boundary_ok = name_end >= chars.len() || !is_ident_char(chars[name_end]);
            if boundary_ok {
                violations.push(format!(
                    "{}:{}: `fn with_latency` reintroduced -- MockSource::with_latency was \
                     deleted (U-delete) specifically because tests kept reaching for it as a \
                     coordination shortcut ('N ms is definitely enough'); use the armable gate \
                     (`with_gate`/`arm_gate`/`park`/`open_gate`) instead",
                    path.display(),
                    i + 1
                ));
            }
        }
    }
}

// ---- check 2: no `.is_err()` chained directly off `timeout(...).await` ----

fn check_direct_timeout_is_err(path: &Path, content: &str, violations: &mut Vec<String>) {
    let Flattened { chars, line_of } = flatten(content);
    let mut i = 0;
    while i < chars.len() {
        if is_word_boundary_before(&chars, i) && matches_at(&chars, i, "timeout(") {
            let open = i + "timeout".len();
            if let Some(close) = matching_paren(&chars, open) {
                let mut j = skip_ws(&chars, close + 1);
                if matches_at(&chars, j, ".await") {
                    j = skip_ws(&chars, j + ".await".len());
                    if matches_at(&chars, j, ".is_err()") {
                        violations.push(format!(
                            "{}:{}: `.is_err()` chained directly off `timeout(...).await` -- \
                             an absence-inference shape (\"nothing happened\", concluded purely \
                             from a bounded wait timing out, with no positive signal behind \
                             it). Wait for a positive event/gate/ScriptedTransport transition \
                             instead; if this genuinely checks an already-extracted value (not \
                             the timeout's own outcome), bind it to a variable first so it is \
                             not a direct chain",
                            path.display(),
                            line_of[i]
                        ));
                    }
                }
            }
        }
        i += 1;
    }
}

// ---- check 3: no loop-less `sleep(` inside a `#[test]`/`#[tokio::test]` fn body ----

fn has_timing_exempt_marker(original_lines: &[&str], line_number: usize) -> bool {
    let check = |n: usize| -> bool {
        n >= 1
            && original_lines
                .get(n - 1)
                .is_some_and(|l| l.contains("TIMING-EXEMPT:"))
    };
    check(line_number) || check(line_number.saturating_sub(1))
}

fn scan_function_body_for_looped_sleep(
    chars: &[char],
    line_of: &[usize],
    original_lines: &[&str],
    path: &Path,
    start: usize,
    end: usize,
    violations: &mut Vec<String>,
) {
    // forward walk: the NEXT `{` seen after a `loop`/`while` keyword is that construct's own
    // body brace (true for every actual loop/while shape in this workspace today -- none put a
    // brace-containing closure inside a while-condition; an accepted heuristic gap, not a
    // full parser).
    let mut loop_open_depths: Vec<usize> = Vec::new();
    let mut pending_loop_keyword = false;
    let mut depth = 0usize;
    let mut pos = start;
    while pos <= end {
        if is_word_boundary_before(chars, pos)
            && ((matches_at(chars, pos, "loop") && is_word_boundary_after(chars, pos + 4))
                || (matches_at(chars, pos, "while") && is_word_boundary_after(chars, pos + 5)))
        {
            pending_loop_keyword = true;
        }
        match chars[pos] {
            '{' => {
                if pending_loop_keyword {
                    loop_open_depths.push(depth);
                    pending_loop_keyword = false;
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if loop_open_depths.last() == Some(&depth) {
                    loop_open_depths.pop();
                }
            }
            _ => {
                if is_word_boundary_before(chars, pos) && matches_at(chars, pos, "sleep(") {
                    let ln = line_of[pos];
                    if loop_open_depths.is_empty() && !has_timing_exempt_marker(original_lines, ln)
                    {
                        violations.push(format!(
                            "{}:{}: loop-less `sleep(` inside a #[test]/#[tokio::test] \
                             function body -- sleeping a fixed duration then checking state \
                             once, hoping the duration was long enough. Wrap it in a loop that \
                             re-checks a real condition every iteration and backstops on a \
                             deadline (the established idiom -- see pty.rs's own \
                             `drain_to_eof`), or mark it `// TIMING-EXEMPT: <reason>` if it is \
                             genuinely a one-shot, justified exception",
                            path.display(),
                            ln
                        ));
                    }
                }
            }
        }
        pos += 1;
    }
}

fn check_looped_sleep_in_tests(path: &Path, content: &str, violations: &mut Vec<String>) {
    let Flattened { chars, line_of } = flatten(content);
    let original_lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < chars.len() {
        let is_test_attr = is_word_boundary_before(&chars, i)
            && (matches_at(&chars, i, "#[test]") || matches_at(&chars, i, "#[tokio::test"));
        if is_test_attr && let Some(fn_pos) = find_next(&chars, i, "fn ") {
            if let Some(body_open) = find_next_char(&chars, fn_pos, '{')
                && let Some(body_close) = matching_brace(&chars, body_open)
            {
                scan_function_body_for_looped_sleep(
                    &chars,
                    &line_of,
                    &original_lines,
                    path,
                    body_open,
                    body_close,
                    violations,
                );
            }
            i = fn_pos;
            continue;
        }
        i += 1;
    }
}

fn find_next(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let pat_chars: Vec<char> = pat.chars().collect();
    let mut i = from;
    while i + pat_chars.len() <= chars.len() {
        if chars[i..i + pat_chars.len()] == pat_chars[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_next_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

/// `chars[open]` must be `{`; returns the index of its matching `}`.
fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    debug_assert_eq!(chars[open], '{');
    let mut depth = 0i32;
    for (offset, &c) in chars[open..].iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn no_timing_oracles() {
    let mut violations = Vec::new();
    let files = collect_rs_files();
    assert!(
        files.len() > 20,
        "found only {} .rs files under {:?} -- the workspace-root/crate-list logic is almost \
         certainly broken, not that the workspace shrank this much",
        files.len(),
        SCANNED_CRATES
    );
    for path in &files {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        check_with_latency_definition(path, &content, &mut violations);
        check_direct_timeout_is_err(path, &content, &mut violations);
        check_looped_sleep_in_tests(path, &content, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "timing/silence-oracle footgun(s) detected -- AGENTS.md's own \"no scheduler oracles\" \
         rule, mechanically enforced (see this test's own module doc comment for what each \
         check catches and why):\n{}",
        violations.join("\n")
    );
}
