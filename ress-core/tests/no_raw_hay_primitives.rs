//! Mechanical guard for restructure R4 (`.superpowers/sdd/structural-accept.md` §3,
//! `restructure-plan.md`'s own "New invariants get their CI guard in the same increment that
//! creates the invariant" rule): `SearchPattern::find_starting_in` / `rfind_starting_in` /
//! `find_all_starting_in` are the raw, unguarded windowed-search primitives -- every accept
//! decision in production now goes through `search::hay::Hay`, the ONE place that declares a
//! hay's edges before ever handing bytes to the regex engine. A raw call anywhere else in
//! production is exactly the "someone searched a bare slice again" class `structural-accept.md`
//! §5 names as this guard's own reason to exist -- a CI failure now, not a batch-6 finding.
//!
//! **Two named exemptions, both load-bearing, both structural, not this guard's blind spot:**
//! - `ress-core/src/search/hay.rs` itself -- the primitives are `pub(crate)` FOR this module's
//!   own use (`structural-accept.md` §5); its own three call sites (the `raw_find_from`/
//!   `raw_rfind`/`raw_find_all` wrappers) are what every other production call now goes through.
//! - Any `#[cfg(test)]`-attributed item (a `mod tests { .. }` block, or a single test-only
//!   function/oracle) -- `restructure-plan.md`'s own binding constraint: the two independent
//!   equivalence oracles, `search.rs`'s own `windowed_scan` and `document.rs`'s own
//!   `reference_all_matches`, must NEVER import `Hay` (the tautology rule -- batch-4 unit-C F5,
//!   F-review P3-3: an oracle re-expressed through the type under test stops being independent
//!   of it). Ordinary tests calling the raw primitives directly (`search.rs`'s own unit tests
//!   for `find_starting_in` itself, `document.rs`'s own match-count measurement) are the same
//!   shape and exempted the same way -- this guard polices PRODUCTION reachability, not test
//!   code, exactly like `no_timing_oracles.rs`'s own check 3 scopes to test bodies for the
//!   opposite reason.
//!
//! Implementation note: hand-rolled character-level scanning (this workspace's own established
//! style for this exact kind of guard -- `no_timing_oracles.rs`'s own doc comment states the
//! same rationale: no `regex` dependency already in this crate, not worth adding for one CI
//! check). Comments are stripped line-by-line before any structural matching, including before
//! locating `#[cfg(test)]` brace spans, so this file's own doc comment (which necessarily
//! contains the literal method names it checks for) can never trip itself.

use std::path::{Path, PathBuf};

/// Leading `.` or `::` on each: a METHOD CALL (`pattern.find_starting_in(..)`) or a UFCS-style
/// CALL (`SearchPattern::find_starting_in(p, ..)`) -- never the `fn find_starting_in(` definition
/// itself (`search.rs`'s own `SearchPattern` impl, which must obviously keep defining these) --
/// `fn find_starting_in(` has neither `.` nor `::` immediately before `find_starting_in`, so the
/// definition site never matches these needles. Fix round (R4 review, P3-5): the `::`-prefixed
/// forms were the ordinary-Rust-idiom gap the review's own probe found
/// (`SearchPattern::find_starting_in(p, hay, 0..1)` scored zero violations before this) -- not
/// the excused string-construction evasion, an everyday call shape this guard must see. A BARE
/// function-pointer capture (`let f = SearchPattern::rfind_starting_in;`, no trailing `(`) stays
/// uncaught, deliberately -- `ufcs_call_form_is_caught_the_same_as_the_method_call_form`'s own
/// doc comment (below) states why a wider, paren-free needle is not the right fix for it.
/// `find_ending_by` (batch 7 (2026-07-28), findings #1/#3 -- the END-bounded primitive, the one
/// `Hay::leftmost_confirmed` replaced its whole retry walk with) is listed as BELT-AND-BRACES,
/// and the distinction is worth stating rather than blurring: unlike the six above it, it is
/// PRIVATE to `crate::search`, so Rust itself already refuses a call from `scan.rs` or
/// `document.rs` -- `search/hay.rs` reaches it only because it is a child module. That is
/// strictly stronger than this guard, which is a text scan. It is listed anyway because the
/// protection is a property of one `pub` keyword's absence: the day anyone widens it to
/// `pub(crate)` for a test's convenience -- exactly what happened to `find_starting_in` -- the
/// compiler stops objecting and this guard becomes the only thing left.
///
/// Its sibling `find_from` is deliberately NOT listed, and adding it was tried and reverted: it
/// is the shared body of the three `*_starting_in` primitives already on this list, so its only
/// callers are those methods, in `search.rs`, three lines from their own definitions. Listing it
/// flags exactly those three -- a call that IS the guarded model's own plumbing, not production
/// code escaping it. The needles here name entry points into the raw engine; a private helper
/// that only the entry points call is covered by covering them.
const RAW_PRIMITIVES: &[&str] = &[
    ".find_starting_in(",
    "::find_starting_in(",
    ".rfind_starting_in(",
    "::rfind_starting_in(",
    ".find_all_starting_in(",
    "::find_all_starting_in(",
    ".find_ending_by(",
    "::find_ending_by(",
];

/// **Restructure R7, batch 8 (2026-07-29): the hay's own BASE is a second raw primitive.**
/// `Hay::new(bytes, base)` takes the absolute position of `bytes[0]` as a parameter, so every
/// caller had to derive it -- and every production caller derived it the same wrong-in-principle
/// way, by subtracting a LENGTH from a POSITION (`Abs(pos - buf.len())`), which silently asserts
/// that everything the caller holds arrived contiguously ending at `pos`. A conforming short read
/// breaks that premise and turns it into a WRONG POSITION, which propagates into reported anchors
/// and into the look-behind `^`/`\b` are decided against. It shipped twice from one function
/// (batch 7 finding #2's spliced carry; batch 8 P1's rebased look-behind gather) before
/// `hay::Assembly` made the premise checkable.
///
/// Production code now builds every hay through `Assembly`, which derives the base from the run
/// each caller says it READ FROM and refuses a run that does not touch what it already holds.
/// `Hay::new` stays `pub(crate)` because `Assembly::hay` is its one legitimate caller; this needle
/// is what keeps the count at one. Same two exemptions as the primitives above: `hay.rs` itself,
/// and `#[cfg(test)]` code (test hays are hand-built over literal fixtures at a known base, with
/// no read to be short in the first place).
///
/// `.new(` is deliberately NOT listed as a bare needle -- it would match every `Foo::new(` in the
/// crate. The `Hay::` qualifier is what makes this precise, and it is also how the type is
/// actually spelled at every existing call site (`crate::search::hay::Hay::new(`), since `Hay` is
/// never imported unqualified in production.
const RAW_HAY_BASE: &[&str] = &["Hay::new("];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ress-core's own Cargo.toml is one level below the workspace root")
        .to_path_buf()
}

/// Only `ress-core/src` -- these primitives are `pub(crate)` (never exported), so no other
/// crate in the workspace can call them at all; scanning `ress-core/tests` is unnecessary for
/// the same reason and would only ever scan integration tests, already test code either way.
fn collect_rs_files() -> Vec<PathBuf> {
    let root = workspace_root().join("ress-core").join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
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

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// A comment-stripped, line-number-tracked flattening of a file -- mirrors
/// `no_timing_oracles.rs`'s own `Flattened`, independently, since this guard is a separate,
/// self-contained integration test binary (no shared support module between the two exists,
/// and adding one for two ~20-line helpers is not worth the indirection).
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

/// `chars[open]` must be `{`; returns the index of its matching `}`, if the braces balance
/// before the slice ends. Comment-stripped input only (a `{`/`}` inside a string literal is
/// not accounted for -- the same accepted heuristic gap `no_timing_oracles.rs`'s own
/// `strip_line_comment` doc comment states; this codebase's own style does not write braces
/// inside string literals on lines this scan needs to be exact about).
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

/// Every `[start, end)` char-index span (into `f.chars`) that is exempt from scanning: each
/// `#[cfg(test)]` attribute's own following brace-delimited body (a `mod tests { .. }`, or a
/// single test-only `fn`/oracle).
///
/// **Fix round (R4 review, P2-1) -- corrected from a real under-scan, not merely re-derived.**
/// The original version of this function searched for the FIRST `{` at or after the attribute
/// with no bound at all -- on a NON-brace attributed item (a struct field, a `let`, a `use`, a
/// `const`, all real shapes this attribute is written on elsewhere in this crate: `status.rs`'s
/// own `#[cfg(test)] resolution_attempts_tx: ...` field and `#[cfg(test)] let mut
/// resolution_attempts = ...`), that search walks straight past the item's own terminator and
/// keeps going until it finds SOME `{` -- which belongs to a later, unrelated, often-PRODUCTION
/// item, and the entire span up to that item's own close was wrongly exempted (measured on the
/// real tree, pre-fix: 93.7% of `status.rs`, most of it ordinary production code). The guard's
/// own doc comment used to claim this "fails safe (over-scans, never under-scans)" -- false on
/// both halves: it is an UNDER-scan (a real production call can be hidden behind it, and the
/// review's own probe did exactly that), and the shape is not hypothetical, it is live in this
/// crate today. Fixed by bounding the forward search at the first `;` or `}` seen, whichever
/// comes first: a non-brace item always ends in one of those (a statement's own `;`, or a
/// struct's own closing `}` if the attributed field has no trailing item after it) strictly
/// before any unrelated LATER brace could be reached, so hitting either one first means "this
/// attribute has no brace-delimited body of its own" and nothing is exempted -- correctly
/// under-EXEMPTING (the safe direction) rather than under-SCANNING.
/// `cfg_test_spans_does_not_exempt_a_production_site_hidden_behind_a_non_brace_cfg_test_item`
/// (below) is the review's own probe, adopted as a permanent regression test.
fn cfg_test_spans(f: &Flattened) -> Vec<(usize, usize)> {
    let marker: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    while i + marker.len() <= f.chars.len() {
        if f.chars[i..i + marker.len()] == marker[..] {
            let mut j = i + marker.len();
            while j < f.chars.len() && f.chars[j] != '{' && f.chars[j] != ';' && f.chars[j] != '}' {
                j += 1;
            }
            if j < f.chars.len()
                && f.chars[j] == '{'
                && let Some(close) = matching_brace(&f.chars, j)
            {
                spans.push((i, close + 1));
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    spans
}

fn in_any_span(pos: usize, spans: &[(usize, usize)]) -> bool {
    spans.iter().any(|&(s, e)| pos >= s && pos < e)
}

fn check_file(path: &Path, content: &str, violations: &mut Vec<String>) {
    let f = flatten(content);
    let exempt = cfg_test_spans(&f);
    let hay: String = f.chars.iter().collect();
    for needle in RAW_PRIMITIVES {
        let mut start = 0;
        while let Some(rel) = hay[start..].find(needle) {
            let pos = start + rel;
            if !in_any_span(pos, &exempt) {
                violations.push(format!(
                    "{}:{}: raw `{}` call outside search::hay -- route it through a `Hay` \
                     method instead (restructure R4, structural-accept.md §5's own guard)",
                    path.display(),
                    f.line_of[pos],
                    needle.trim_start_matches([':', '.']).trim_end_matches('(')
                ));
            }
            start = pos + needle.len();
        }
    }
    for needle in RAW_HAY_BASE {
        let mut start = 0;
        while let Some(rel) = hay[start..].find(needle) {
            let pos = start + rel;
            if !in_any_span(pos, &exempt) {
                violations.push(format!(
                    "{}:{}: raw `Hay::new` outside search::hay -- its `base` parameter is an \
                     absolute position the caller has to derive, and deriving it from a LENGTH \
                     (`Abs(pos - buf.len())`) silently asserts contiguity a short read can break. \
                     Build the hay with `hay::Assembly` instead: anchor on the payload run at the \
                     position it was read from, then `extend_below`/`extend_above` the margins \
                     (restructure R7, batch 8's own P1 and batch 7's own finding #2)",
                    path.display(),
                    f.line_of[pos],
                ));
            }
            start = pos + needle.len();
        }
    }
}

#[test]
fn raw_windowed_search_primitives_stay_inside_search_hay() {
    let hay_module = workspace_root()
        .join("ress-core")
        .join("src")
        .join("search")
        .join("hay.rs");
    let mut violations = Vec::new();
    for path in collect_rs_files() {
        if path == hay_module {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        check_file(&path, &content, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "raw windowed-search primitive calls found outside search::hay (and outside \
         #[cfg(test)] oracle/test code):\n{}",
        violations.join("\n")
    );
}

/// Sanity probe for the guard's own exemption logic: a hand-built fixture with a violation
/// INSIDE a `#[cfg(test)] mod tests { .. }` block and one OUTSIDE it must be told apart --
/// otherwise this guard could pass today for the wrong reason (over-exempting, and so blind to
/// a real regression) rather than the right one.
#[test]
fn exemption_logic_distinguishes_test_and_production_call_sites() {
    let fixture = r#"
fn production_site(p: &SearchPattern, hay: &[u8]) {
    let _ = p.find_starting_in(hay, 0..1);
}
#[cfg(test)]
mod tests {
    fn oracle_site(p: &SearchPattern, hay: &[u8]) {
        let _ = p.find_starting_in(hay, 0..1);
    }
}
"#;
    let mut violations = Vec::new();
    check_file(Path::new("fixture.rs"), fixture, &mut violations);
    assert_eq!(
        violations.len(),
        1,
        "expected exactly the production-site violation: {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:3"));
}

/// The R4 review's own P2-1 probe, adopted verbatim as a permanent regression test: a
/// `#[cfg(test)]` attribute on a NON-brace item (`use ...;`) must not swallow a later,
/// unrelated, PRODUCTION `find_starting_in` call into its own (nonexistent) exemption span.
/// Before the fix, this fixture scored zero violations -- the forward scan for `cfg_test_spans`'s
/// own next `{` walked straight past the `use` statement's `;` into `production_site`'s own body.
#[test]
fn cfg_test_spans_does_not_exempt_a_production_site_hidden_behind_a_non_brace_cfg_test_item() {
    let fixture = r#"
#[cfg(test)]
use std::collections::HashMap;

fn production_site(p: &SearchPattern, hay: &[u8]) {
    let _ = p.find_starting_in(hay, 0..1);
}
"#;
    let mut violations = Vec::new();
    check_file(Path::new("fixture.rs"), fixture, &mut violations);
    assert_eq!(
        violations.len(),
        1,
        "the #[cfg(test)] use statement must not exempt the production call below it: {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:6"));
}

/// The sibling non-brace shape the review found live in `status.rs` (a `#[cfg(test)]` struct
/// field, not a `use`) -- same bug, same fix, a separate fixture so a regression in one shape
/// cannot hide behind the other's own test staying green.
#[test]
fn cfg_test_spans_does_not_exempt_a_production_site_hidden_behind_a_cfg_test_struct_field() {
    let fixture = r#"
struct Worker {
    #[cfg(test)]
    probe_tx: Option<u64>,
    real_field: u64,
}

fn production_site(p: &SearchPattern, hay: &[u8]) {
    let _ = p.find_starting_in(hay, 0..1);
}
"#;
    let mut violations = Vec::new();
    check_file(Path::new("fixture.rs"), fixture, &mut violations);
    assert_eq!(
        violations.len(),
        1,
        "the #[cfg(test)] field must not exempt the production call below it: {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:9"));
}

/// Restructure R7's own needle, proven to catch the shape it exists for rather than assumed to:
/// a production `Hay::new` with a hand-derived base is a violation, the same call inside
/// `#[cfg(test)]` is not. Without this, `raw_windowed_search_primitives_stay_inside_search_hay`
/// passing after the R7 migration would be indistinguishable from a needle that matches nothing.
#[test]
fn hand_built_hay_bases_are_caught_outside_search_hay() {
    let fixture = r#"
fn production_site(pos: u64, buf: &[u8]) {
    let _ = crate::search::hay::Hay::new(buf, crate::search::hay::Abs(pos - buf.len() as u64));
}
#[cfg(test)]
mod tests {
    fn fixture_site(buf: &[u8]) {
        let _ = Hay::new(buf, Abs(0));
    }
}
"#;
    let mut violations = Vec::new();
    check_file(Path::new("fixture.rs"), fixture, &mut violations);
    assert_eq!(
        violations.len(),
        1,
        "expected exactly the production-site violation: {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:3") && violations[0].contains("Assembly"));
}

/// R4 review, P3-5: the UFCS CALL form (no leading `.`) is an ordinary Rust idiom, not the
/// excused string-construction evasion -- `SearchPattern::find_starting_in(p, ..)` must be caught
/// the same as the method-call form.
///
/// **Scope, disclosed rather than silently narrower than the review's own evidence:** the
/// review's own P3-5 probe also demonstrated a BARE function-pointer capture (`let f =
/// SearchPattern::rfind_starting_in;`, no trailing `(` at all) scoring zero violations. That shape
/// stays uncaught here, deliberately, matching the review's own suggested remediation (three
/// PAREN-terminated needles, not a bare-path scan) rather than the broader fix: a needle without
/// the trailing `(` would also match as a PREFIX of any future, unrelated identifier merely
/// starting with the same text (e.g. a hypothetical `find_starting_index`), a real precision
/// regression this guard's own established heuristic style (word-boundary-free substring
/// matching, `no_timing_oracles.rs`'s own precedent) does not otherwise carry for a bare path. A
/// captured-but-never-called function pointer is a narrow, unrealistic residual: the crate has no
/// call site that only ever captures either primitive without eventually calling it through `.`
/// or `::(`, both of which this guard does catch.
#[test]
fn ufcs_call_form_is_caught_the_same_as_the_method_call_form() {
    let fixture = r#"
fn production_site(p: &SearchPattern, hay: &[u8]) {
    let _ = SearchPattern::find_starting_in(p, hay, 0..1);
    let _ = SearchPattern::rfind_starting_in(p, hay, 0..1);
}
"#;
    let mut violations = Vec::new();
    check_file(Path::new("fixture.rs"), fixture, &mut violations);
    assert_eq!(
        violations.len(),
        2,
        "expected both UFCS calls: {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:3") && violations[0].contains("find_starting_in"));
    assert!(violations[1].contains("fixture.rs:4") && violations[1].contains("rfind_starting_in"));
}
