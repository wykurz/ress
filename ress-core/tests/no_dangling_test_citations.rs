//! Mechanical guard against a divergence class this project's own review history kept producing:
//! **a comment or doc that cites a test by name as evidence, when no test of that name exists.**
//!
//! It is a cheap mistake to make and an expensive one to leave. Comments here routinely carry the
//! reasoning for a decision and name the test that pins it; when the test is later renamed, the
//! citation becomes a pointer to nothing, and a reader (or a reviewer) has no way to tell "this
//! evidence was removed" from "this evidence never existed". Two rounds of review found instances,
//! and one was worse than a dead link: `sweep_analysis_treats_its_own_read_limit_as_a_real_boundary`
//! was cited as pinning a defect while the real test, `sweep_analysis_does_NOT_treat_...`, pins the
//! opposite -- a citation that actively misinforms. Batch 8 also shipped a comment citing
//! `search_backward_never_reads_below_its_own_floor_for_context`, a test renamed when its own claim
//! was INVERTED, so the citation pointed at a pin for the contrary property.
//!
//! **What counts as a citation:** any backtick-quoted `snake_case` identifier at least
//! `MIN_NAME_LEN` characters long containing an underscore. That heuristic deliberately over-scans
//! -- it also picks up field names, local variables and concept names -- so everything it finds
//! must be either a real `fn` in this crate or excused below. In practice the long-identifier floor
//! plus the exemptions leave a small, quiet guard.
//!
//! **Two exemptions, both load-bearing:**
//!
//! - **Anything defined anywhere in the crate.** The needle is compared against every `fn` name in
//!   `ress-core`, test and production alike, plus struct/enum/const/field-ish declarations, so
//!   citing a helper, a field, or a type is never a violation.
//! - **Deliberate historical mentions.** This codebase's comment style explains decisions by
//!   naming what was replaced ("supersedes `red_...`", "the deleted `old_...`", "the
//!   `advancing` parameter and `trailing_margin_only_rejection` all retire here"). Those citations
//!   SHOULD name something that no longer exists -- that is their whole point. A marker word from
//!   `HISTORICAL_MARKERS` excuses a citation when it sits **in the citation's own clause**, within
//!   `MARKER_WINDOW` lines of it. Both halves of that are load-bearing and both were learned from
//!   defects: an unbounded line window excuses every citation in any file that mentions a deletion
//!   anywhere, and a marker anywhere in the SENTENCE excuses the live name in "`new` ... which
//!   replaced the deleted `old`" as readily as the dead one. So the writing convention the guard
//!   enforces is: **say what happened to a name in the same clause as the name.** See
//!   `excused_as_historical` for the measurements and for the three residual cases where one marker
//!   genuinely governs two names.
//!
//! Scope: citations are SCANNED in `ress-core/src` and `docs/`, but the set of known names is
//! collected from the WHOLE workspace. The asymmetry is deliberate -- `docs/perf.md` cites bench
//! functions and `ress-perf` tests quite legitimately, so a guard blind to them would flag correct
//! citations, which is the fastest way to get a guard ignored or deleted.
//!
//! Implementation note: hand-rolled scanning, matching this workspace's established style for CI
//! guards (`no_raw_hay_primitives.rs`, `no_timing_oracles.rs` both state the same rationale -- no
//! `regex` dependency exists in this crate and one is not worth adding for a text check).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Below this length, `snake_case` backticked words are overwhelmingly ordinary prose, field names
/// or third-party API names (`block_size`, `at_eof`, `extend_from_slice`) rather than test
/// citations. Lowered from 25 to 22 in batch 19 (2026-07-31), because 25 excluded real cited tests
/// -- `compile_error_surfaces` (22) among them.
///
/// **The blind spot below 22 is accepted deliberately, and it is a coverage limit rather than a
/// claim that every test name is longer** (batch 20 (2026-07-31)
/// corrects exactly that claim, which this comment used to make, while also naming a value the
/// constant did not have). Short names exist and are cited: `no_timing_oracles` (17) is a CI guard
/// this codebase refers to by its file stem. They are simply not scanned, and the alternative is
/// worse -- measured across the whole crate, lowering the cutoff admits ordinary identifiers fast:
/// 20 flags `unicode_segmentation` and `consecutive_failures`; 18 adds `look_set_prefix_any` and
/// `resolve_line_start`; 17 adds `borrow_and_update` and `extend_from_slice`, which are tokio and
/// std methods. A guard that cries wolf is a guard that gets switched off, so the tradeoff is
/// stated here rather than taken silently. Citing a short name is SAFE regardless: test-file stems
/// and every declared identifier are in the known set, so nothing short is ever flagged wrongly --
/// it is only unchecked.
///
/// The two ways out were both measured and both declined (batch 21 (2026-07-31)). An ALLOWLIST of
/// ordinary short identifiers needs ~18 entries at a cutoff of 16 -- `extend_from_slice`,
/// `borrow_and_update`, `look_set_prefix_any`, and a dozen field and LOCAL VARIABLE names, which
/// appear and vanish with every refactor -- to buy one genuine find, and a list that must be
/// maintained against local variables is a list that rots. Scanning known test STEMS separately
/// buys nothing: they are already in the declared set, so a citation of one resolves either way;
/// what a short-name scan could catch is a citation of a DELETED short test, and no lexical signal
/// separates that from a deleted field. So the heuristic stands and its cost is written down.
const MIN_NAME_LEN: usize = 22;

/// How far back a historical marker may sit and still excuse a citation. Three lines covers the
/// sentence a citation belongs to under this project's comment wrapping without reaching into an
/// unrelated paragraph.
const MARKER_WINDOW: usize = 3;

/// Words that mark a citation as deliberately naming something gone. Lowercase; the haystack is
/// lowercased before matching, and each entry is matched at a WORD BOUNDARY (`has_marker`), not as
/// a bare substring. Kept narrow and concrete -- each one is a phrase this codebase actually uses
/// for this purpose, not a guess at what someone might write.
const HISTORICAL_MARKERS: &[&str] = &[
    "retire",   // "all retire here", "retired"
    "delete",   // "deleted here", "deleted once this fix landed"
    "supersed", // "supersedes", "superseded by"
    "former",   // "this file's own former ..."
    // "used to" is DELIBERATELY absent (batch 19 (2026-07-31)). It is the one phrase here with two
    // senses -- "this test used to assert X" excuses, "a helper used to witness X" must not -- and
    // two rounds of review found real dangling citations hiding behind the ambiguous one. A grammar
    // heuristic (does a subject precede it?) closed the first and not the second, because "This
    // helper is used to witness `x`" has a subject too. So the phrase simply is not a marker: the
    // ~30 comments here that mean it historically all sit beside an unambiguous one, and any that
    // does not gets reworded rather than guessed at. A marker's whole job is to be unmistakable.
    "no longer exists",
    "renamed",
    "replaced",
    "earlier version",
    "pre-restructure",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ress-core's own Cargo.toml is one level below the workspace root")
        .to_path_buf()
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

fn scanned_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root.join("ress-core").join("src"), "rs", &mut files);
    walk(&root.join("docs"), "md", &mut files);
    files.sort();
    files
}

/// Every identifier this crate declares: `fn` names first (what a citation almost always means),
/// plus the other declaration forms, so citing a field or a type is never flagged.
fn declared_identifiers() -> HashSet<String> {
    let mut out = HashSet::new();
    let mut files = Vec::new();
    // Every CRATE's own Cargo source roots, not the whole workspace tree (batch 21 (2026-07-31)).
    // The wide walk read gitignored scratch directories too -- `.superpowers/` holds review probe
    // files that duplicate live test names -- so a definition deleted from tracked code could still
    // be "declared" by an untracked file sitting in the working tree. That passes locally and fails
    // in clean CI, which is the worst failure mode a guard has: it is trusted exactly when it is
    // wrong. Restricting discovery to `src`/`tests`/`benches` under each crate keeps what the wide
    // walk was FOR -- `docs/perf.md` legitimately cites bench functions and `ress-perf`'s own
    // tests, and a guard blind to them would flag correct citations.
    let root = workspace_root();
    for crate_dir in ["ress-core", "ress", "ress-filegen", "ress-perf", "."] {
        for sub in ["src", "tests", "benches"] {
            walk(&root.join(crate_dir).join(sub), "rs", &mut files);
        }
    }
    // integration-test FILE STEMS are citable names too (batch 20 (2026-07-31)): this codebase
    // refers to its CI guards that way -- "`no_timing_oracles` is law" -- and they are tests as
    // much as any `#[test]` fn is. Without them, a citation of one reads as dangling the moment the
    // length cutoff is low enough to see it.
    for f in &files {
        if f.parent().is_some_and(|p| p.ends_with("tests"))
            && let Some(stem) = f.file_stem().and_then(|s| s.to_str())
        {
            out.insert(stem.to_string());
        }
    }
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in text.lines() {
            let t = line.trim();
            for kw in [
                "fn ", "struct ", "enum ", "const ", "static ", "type ", "trait ", "mod ", "let ",
            ] {
                if let Some(rest) = t.strip_prefix(kw) {
                    out.insert(ident_prefix(rest));
                }
                // `pub fn`, `pub(crate) async fn`, `async fn`, ... -- find the keyword anywhere
                // and take the identifier after it, which covers every visibility/async spelling
                // without enumerating them.
                if let Some(i) = t.find(&format!(" {kw}")) {
                    out.insert(ident_prefix(&t[i + 1 + kw.len()..]));
                }
            }
            // struct fields and enum variants: `name: Type,` / `Name {`
            if let Some((head, _)) = t.split_once(':') {
                out.insert(ident_prefix(head));
            }
        }
    }
    out.remove("");
    out
}

fn ident_prefix(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// One logical line: `lines[i]`, plus the comment lines continuing it, with their comment markers
/// removed so a citation wrapped across them reads as one word. Bounded by `MARKER_WINDOW` for the
/// same reason the marker search is -- an unbounded join would let a citation on one line be
/// completed by text an arbitrary distance away.
fn joined_from(lines: &[&str], i: usize) -> (String, usize) {
    let mut out = lines[i].to_string();
    let mut consumed = 0;
    // **Continue only while a backtick is still OPEN.** A citation wrapped across lines always
    // leaves one unclosed, and nothing else does -- so this joins exactly the lines that finish a
    // name and never glues unrelated prose into a false one. It is also the rule that makes the
    // join format-agnostic (batch 20 (2026-07-31)): the previous version stripped `///` or `//` and
    // stopped at anything else, which silently missed `//!` module docs (where the `!` was glued
    // INTO the name) and Markdown entirely, since prose has no marker to strip. Both carry live
    // citations.
    for line in lines.iter().skip(i + 1).take(MARKER_WINDOW) {
        if out.chars().filter(|c| *c == '`').count() % 2 == 0 {
            break;
        }
        // seamless: a name broken across lines has no space at the break, and `citations` stops at
        // the first non-name character anyway.
        out.push_str(body(line));
        consumed += 1;
    }
    (out, consumed)
}
/// A line with its comment marker removed -- `//!`, `///`, `//`, or nothing at all for Markdown.
fn body(line: &str) -> &str {
    let t = line.trim_start();
    t.strip_prefix("//!")
        .or_else(|| t.strip_prefix("///"))
        .or_else(|| t.strip_prefix("//"))
        .unwrap_or(t)
        .trim_start()
}
/// **Clauses**, not sentences (batch 22 (2026-08-01)): terminal punctuation followed by whitespace
/// (or the end), and also `,` `;` `:` `(` `)` `--` `--`'s em-dash spelling. A sentence was too
/// coarse a unit for the question this guard actually asks -- see `excused_as_historical` for the
/// defect that showed it. Terminal punctuation leaves `self.pos` and `1.5` intact; "e.g. " does
/// split one early, which errs toward NOT excusing a citation -- the safe direction for a guard.
///
/// **Splitting stops inside backticks**, so every clause is backtick-BALANCED. Two reasons, both
/// found by measurement rather than foreseen: a delimiter genuinely occurs inside code spans
/// (`min(4099, 52)`, `search_summary().borrow()`), and a clause that opens mid-span inverts the
/// parity `has_marker` relies on to tell prose from code -- which silently turned a real marker
/// into "code" at one measured site. A balanced clause cannot do that.
///
/// Each clause is returned with **the char offset it starts at in `text`**, because the caller has
/// to find the clause holding one particular OCCURRENCE (batch 23 (2026-08-01), finding #3), not
/// any clause that happens to mention the same name.
fn clauses(text: &str) -> Vec<(usize, String)> {
    let ch: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut i, mut start, mut in_span) = (0, 0, false);
    while i < ch.len() {
        let c = ch[i];
        if c == '`' {
            in_span = !in_span;
        }
        if !in_span {
            if matches!(c, ',' | ';' | ':' | '(' | ')' | '\u{2014}')
                || (matches!(c, '.' | '!' | '?') && ch.get(i + 1).is_none_or(|n| n.is_whitespace()))
            {
                cur.push(c);
                out.push((start, std::mem::take(&mut cur)));
                i += 1;
                start = i;
                continue;
            }
            if c == '-' && ch.get(i + 1) == Some(&'-') {
                out.push((start, std::mem::take(&mut cur)));
                i += 2;
                start = i;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    if !cur.is_empty() {
        out.push((start, cur));
    }
    out
}
/// Does this clause's PROSE carry a historical marker? Prose is the odd-numbered backtick split:
/// markers say something ABOUT a name, so they are never looked for inside one (batch 21
/// (2026-07-31) -- a name containing "supersed" excused itself).
///
/// The match requires a leading WORD BOUNDARY (batch 22 (2026-08-01)). Bare `contains` made every
/// marker a substring rule, so "performer"/"transformer" carried "former" and "unreplaced" carried
/// "replaced" -- the last of those is not hypothetical, `meter.rs` writes it. A TRAILING boundary
/// is deliberately not required: the markers are stems on purpose ("supersed" for
/// supersedes/superseded, "retire" for retires/retired/retirement).
fn has_marker(clause: &str) -> bool {
    let prose = clause
        .split('`')
        .step_by(2)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let b: Vec<char> = prose.chars().collect();
    HISTORICAL_MARKERS.iter().any(|m| {
        let m: Vec<char> = m.chars().collect();
        b.len() >= m.len()
            && (0..=b.len() - m.len()).any(|i| {
                b[i..].starts_with(&m)
                    && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_'))
            })
    })
}
/// A citation looks like a test name: long, `snake_case`, inside backticks.
fn citations(line: &str) -> Vec<String> {
    citations_at(line).into_iter().map(|(_, n)| n).collect()
}
/// `citations`, with each name's own opening-backtick position. The position is what lets the
/// scanner attribute a citation to the line it STARTS on: a logical line is joined with the ones
/// continuing it (`joined_from`), so a name sitting wholly on a later line would otherwise be
/// reported once per line the join spans, each time with a marker window computed from the wrong
/// place (batch 19 (2026-07-31)).
fn citations_at(line: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '`' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != '`' {
                j += 1;
            }
            if j < bytes.len() {
                let word: String = bytes[start..j].iter().collect();
                let looks_like_a_name = word.len() >= MIN_NAME_LEN
                    && word.contains('_')
                    && word
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if looks_like_a_name {
                    out.push((i, word));
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A marker may sit either side of the citation: this codebase writes both "supersedes `x`" and
/// "`x` and `y` all retire here". Backward reach is `MARKER_WINDOW` (a wrapped sentence); forward
/// reach is deliberately tighter at one line, since a marker BELOW a citation is only plausibly
/// about it when it is in the same sentence.
fn excused_as_historical(
    lines: &[&str],
    idx: usize,
    wrapped_over: usize,
    at_in_line: usize,
) -> bool {
    let lo = idx.saturating_sub(MARKER_WINDOW);
    // forward reach is measured from the citation's CLOSING line, not its opening one (batch 21
    // (2026-07-31)): a wrapped name ends `wrapped_over` lines below where it starts, and its marker
    // naturally follows the end of the sentence it sits in.
    let hi = (idx + wrapped_over + 1).min(lines.len() - 1);
    // marker-stripped, so a citation wrapped across lines reads as one word here exactly as it does
    // in `joined_from` -- the window and the citation must agree about what the name IS, or a
    // wrapped name is never found in its own sentence and nothing can ever excuse it. An EMPTY
    // comment body becomes a full stop: a blank `///` line is a paragraph break in prose but
    // carries no punctuation, so without this the sentences either side of it merge and a marker
    // reaches across a paragraph it has nothing to do with.
    //
    // Built by hand rather than with `join`, because the citation's own offset has to be carried
    // into the window's coordinates: `body` only ever strips from the FRONT of a line, so the
    // opening backtick moves left by exactly what was stripped and right by wherever this line
    // landed in the window. NOT lowercased here -- `has_marker` lowercases each clause instead,
    // since a case fold can change a string's LENGTH and every offset below would shift with it.
    let mut window = String::new();
    let mut cite_at = None;
    for (k, l) in lines[lo..=hi].iter().enumerate() {
        if k > 0 {
            window.push(' ');
        }
        let b = body(l);
        if lo + k == idx {
            let stripped = l.chars().count() - b.chars().count();
            cite_at = Some(window.chars().count() + at_in_line.saturating_sub(stripped));
        }
        window.push_str(if b.is_empty() { "." } else { b });
    }
    let Some(cite_at) = cite_at else {
        return false;
    };
    // **The marker must be in the citation's OWN CLAUSE, and must be PROSE** (batch 20/21
    // (2026-07-31), narrowed from "sentence" to "clause" in batch 22 (2026-08-01)). Collapsing the
    // whole window to one boolean let an unrelated "superseded ..." paragraph two lines up excuse a
    // dangling citation; searching the sentence including its backticked names let a name
    // CONTAINING a marker excuse itself, and let a live name lend its marker to a dead one beside
    // it. Markers say something ABOUT a name, so they are looked for only outside the names.
    //
    // **Why a clause and not a sentence.** This codebase's standard way of recording a rename puts
    // BOTH names in one sentence -- "`new_name` ..., which replaced the deleted `old_name`" -- so a
    // sentence-wide marker excused the LIVE name too, silently removing it from the guard's reach
    // for the rest of its life. Demonstrated on the pattern at `document.rs`'s own
    // `dollar_anchor_needs_the_certified_length_not_merely_a_certified_index`: renaming the live
    // test it cites produced zero violations. A clause is the smallest unit that still holds the
    // established phrasings ("supersedes `x`", "`x` and `y` all retire here", "the deleted `x`"),
    // and it is a RULE a writer can follow rather than another proximity guess: **say what happened
    // to a name in the same clause as the name.** Measured over the whole tree: 21 live citations
    // were unprotected, now 3, and 5 comments were reworded to put their marker beside their name.
    //
    // The 3 that remain are the honest floor of a lexical rule, not an oversight -- each is one
    // marker legitimately governing a clause that also holds a live name ("RENAMED from `old` to
    // `new`", "an earlier version of this function ... whenever `live_name`"). Splitting finer than
    // a clause would not separate them; only a parser that knows which name a verb takes as its
    // object would, and that is not a thing a CI guard should contain.
    // **The clause is the one holding THIS OCCURRENCE, found by offset** (batch 23 (2026-08-01),
    // finding #3). Two weaker rules preceded it and each let a marker cover a citation it was not
    // about. Searching every clause for the NAME let a longer identifier stand in for a shorter
    // one -- "`current_long_name`, which replaced the deleted `current_long_name_old`" has the
    // live name inside the dead one (batch 22, finding #2). Narrowing that to exact
    // backtick-span equality fixed the containment but not the aliasing: the same name can be
    // cited TWICE in one window, once historically and once live, and "does some clause cite this
    // name and carry a marker" answers yes for both --
    //
    //     /// the deleted `a_name` was historical.
    //     /// See `a_name` for the current rule.
    //
    // reported nothing. An occurrence is a position, so the lookup is by position: the clause
    // containing `cite_at` is the last one that starts at or before it, since `clauses` partitions
    // the window in order. Nothing else in this function needs the name at all any more.
    let cl = clauses(&window);
    cl.iter()
        .rev()
        .find(|(start, _)| *start <= cite_at)
        .is_some_and(|(_, clause)| has_marker(clause))
}

fn check(path: &Path, text: &str, declared: &HashSet<String>, out: &mut Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // **Wrapped citations count too** (batch 19 (2026-07-31)). A name too long for one line is
        // written `` `search_backward_never_ `` / `` reads_below_its_own_floor` ``, and line-local
        // parsing sees two short fragments instead of one stale name -- a demonstrated miss. Each
        // line is therefore scanned joined to the comment lines that CONTINUE it, with their `///`
        // or `//` markers stripped so the halves meet exactly as they read.
        let (joined, wrapped_over) = joined_from(&lines, i);
        for (at, name) in citations_at(&joined) {
            // only names that OPEN on this line: the join reaches forward, so anything opening
            // later belongs to that later line and is judged there, against its own markers.
            if at >= line.chars().count() {
                continue;
            }
            if declared.contains(&name) || excused_as_historical(&lines, i, wrapped_over, at) {
                continue;
            }
            out.push(format!(
                "{file}:{line}: cites `{name}`, which is not defined anywhere in ress-core. \
                 Either fix the name -- a rename is the usual cause, and check the CLAIM too, \
                 since a renamed test has sometimes had its meaning inverted, leaving the old \
                 citation asserting the opposite of what the new test pins -- or, if it names \
                 something deliberately gone, say so on the line with one of {markers:?}",
                file = path.display(),
                line = i + 1,
                name = name,
                markers = HISTORICAL_MARKERS,
            ));
        }
    }
}

#[test]
fn every_cited_test_name_exists() {
    let declared = declared_identifiers();
    let mut violations = Vec::new();
    for path in scanned_files() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        check(&path, &text, &declared, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "comments or docs cite test names that do not exist:\n{}",
        violations.join("\n")
    );
}

/// The guard has to tell a stale citation from a deliberate historical one, or it is unusable in
/// this codebase -- explaining a decision by naming what it replaced is the established comment
/// style here. Both shapes in one fixture so neither can pass for the wrong reason.
#[test]
fn historical_mentions_are_excused_and_live_citations_are_not() {
    let mut declared = HashSet::new();
    declared.insert("a_real_test_that_definitely_exists".to_string());
    // NOTE the blank lines: the marker window reaches one line FORWARD as well as back, so the
    // stale citation has to be genuinely isolated from the historical one or it is excused --
    // which is the behaviour `a_distant_historical_marker_does_not_excuse_a_citation` pins from
    // the other side.
    let fixture = "\
// pinned by `a_real_test_that_definitely_exists`, which exists.
//
// pinned by `a_stale_citation_that_names_nothing`, which does not.
//
//
// supersedes `an_older_fixture_that_was_deleted`, deliberately gone.
";
    let mut v = Vec::new();
    check(Path::new("fixture.rs"), fixture, &declared, &mut v);
    assert_eq!(v.len(), 1, "expected only the stale citation: {v:?}");
    assert!(v[0].contains("fixture.rs:3") && v[0].contains("a_stale_citation_that_names_nothing"));
}

/// The marker window must be BOUNDED. An unbounded backward search for a historical marker would
/// excuse every citation in any file that mentions a deletion anywhere above it -- which is most
/// files in this crate, making the guard silently vacuous.
#[test]
fn a_distant_historical_marker_does_not_excuse_a_citation() {
    let mut v = Vec::new();
    let far = format!(
        "// this test was deleted long ago.\n{}// pinned by `a_stale_citation_that_names_nothing`.\n",
        "//\n".repeat(MARKER_WINDOW + 1)
    );
    check(Path::new("fixture.rs"), &far, &HashSet::new(), &mut v);
    assert_eq!(
        v.len(),
        1,
        "a marker beyond MARKER_WINDOW lines must not excuse: {v:?}"
    );
}

/// Short backticked identifiers are ordinary prose in this codebase (`block_size`, `at_eof`,
/// `last_nl`) and must not be treated as citations, or the guard drowns in false positives.
#[test]
fn short_snake_case_words_are_not_treated_as_citations() {
    let mut v = Vec::new();
    check(
        Path::new("fixture.rs"),
        "// bounded by `block_size` and `at_eof`, neither of which is a test.\n",
        &HashSet::new(),
        &mut v,
    );
    assert!(v.is_empty(), "short identifiers must be ignored: {v:?}");
}

/// `check` over one in-memory file, for the unit tests below.
fn violations(text: &str, declared: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    check(Path::new("f.rs"), text, declared, &mut out);
    out
}
#[test]
fn a_citation_wrapped_across_comment_lines_is_read_as_one_name() {
    let text = "/// see `search_backward_never_\n/// reads_below_its_own_floor_for_context` here\n";
    let v = violations(text, &HashSet::new());
    assert_eq!(v.len(), 1, "one name, not two fragments: {v:?}");
    assert!(
        v[0].contains("search_backward_never_reads_below_its_own_floor_for_context"),
        "the halves must be joined seamlessly: {v:?}"
    );
}
#[test]
fn a_wrapped_citation_is_reported_once_at_the_line_it_opens_on() {
    // the join reaches forward, so a name sitting wholly on a later line must NOT also be
    // reported from each earlier line the join spans -- it would be judged against the wrong
    // line's markers.
    let text = "/// filler\n/// filler\n/// `a_name_that_is_long_enough_to_count`\n";
    let v = violations(text, &HashSet::new());
    assert_eq!(v.len(), 1, "exactly one report: {v:?}");
    assert!(v[0].contains("f.rs:3"), "at its own line: {v:?}");
}
#[test]
fn a_name_containing_a_marker_word_does_not_excuse_itself() {
    // the reviewer's own case: `..._and_supersedes` contains "supersed", and searching the sentence
    // INCLUDING its backticked names let the name vouch for itself.
    let text = "/// See `sweep_summary_is_generation_stamped_and_supersedes` for the rule.\n";
    assert_eq!(
        violations(text, &HashSet::new()).len(),
        1,
        "a marker substring inside the cited name is not a marker ABOUT it"
    );
}
#[test]
fn a_live_name_does_not_lend_its_marker_to_a_dead_one_beside_it() {
    let declared: HashSet<String> = ["a_live_name_that_is_replaced_and_long".to_string()]
        .into_iter()
        .collect();
    let text =
        "/// `a_live_name_that_is_replaced_and_long` and `a_dead_name_that_is_long_enough`\n";
    assert_eq!(
        violations(text, &declared).len(),
        1,
        "the dead name must still be flagged: the only 'marker' here is inside another name"
    );
}
#[test]
fn a_blank_comment_line_breaks_the_sentence() {
    let text = "/// The old rule was superseded.\n///\n\
                /// See `a_name_that_is_long_enough_to_count` for the current one.\n";
    assert_eq!(
        violations(text, &HashSet::new()).len(),
        1,
        "a blank `///` is a paragraph break even though it carries no punctuation"
    );
}
#[test]
fn a_marker_after_a_wrapped_citations_closing_line_still_excuses() {
    // the forward window is measured from where the name ENDS, not where it starts.
    let text =
        "/// see `a_name_that_is_long_enough_\n/// to_count_here_ok` \n/// which was deleted.\n";
    assert!(
        violations(text, &HashSet::new()).is_empty(),
        "a marker following the closing line is still in the citation's own sentence"
    );
}
#[test]
fn an_unrelated_marker_sentence_does_not_excuse_a_citation() {
    // the marker must be about the name it sits beside -- a "superseded" sentence two lines up is
    // about something else entirely.
    let text = "/// The old belt was superseded here.\n/// Filler.\n\
                /// See `a_name_that_is_long_enough_to_count` for the current rule.\n";
    assert_eq!(
        violations(text, &HashSet::new()).len(),
        1,
        "an unrelated marker sentence must not suppress a dangling citation"
    );
    let same_sentence = "/// `a_name_that_is_long_enough_to_count` was superseded here.\n";
    assert!(
        violations(same_sentence, &HashSet::new()).is_empty(),
        "a marker in the citation's own sentence still excuses"
    );
}
#[test]
fn a_marker_in_a_neighbouring_clause_of_the_same_sentence_does_not_excuse() {
    // batch 22 (2026-08-01). The shape that motivated narrowing sentence -> clause: this codebase
    // records a rename by naming BOTH tests in one sentence, and a sentence-wide marker took the
    // live one out of the guard's reach along with the dead one. Both halves asserted here, so
    // neither can pass for the wrong reason.
    let text = "/// so callers share one path (`a_live_name_that_is_long_enough_ok`, `x.rs`, \
                which replaced the deleted `a_dead_name_that_is_long_enough`).\n";
    let v = violations(text, &HashSet::new());
    assert_eq!(
        v.len(),
        1,
        "the live name's own clause carries no marker, so it must still be checked: {v:?}"
    );
    assert!(
        v[0].contains("a_live_name_that_is_long_enough_ok"),
        "the flagged one must be the live name, not the deliberately-dead one: {v:?}"
    );
}
#[test]
fn a_longer_historical_name_does_not_excuse_the_live_name_inside_it() {
    // batch 22 (2026-08-01). Renames in this codebase routinely EXTEND a name, so the dead one
    // very often contains the live one -- and a substring test over the clause let the dead one's
    // marker cover both. The clause is matched against its own backticked spans, for equality.
    let text = "/// `a_live_name_that_is_long_enough`, which replaced the deleted \
                `a_live_name_that_is_long_enough_old`.\n";
    let v = violations(text, &HashSet::new());
    assert_eq!(
        v.len(),
        1,
        "only the live name is checkable here; the longer dead one is excused: {v:?}"
    );
    assert!(
        v[0].contains("`a_live_name_that_is_long_enough`"),
        "the flagged name must be the live one, not the longer historical one: {v:?}"
    );
}
#[test]
fn a_historical_mention_does_not_excuse_a_second_live_citation_of_the_same_name() {
    // batch 23 (2026-08-01), the reviewer's own case. Matching by NAME -- however exactly --
    // cannot tell two occurrences apart, so a name mentioned once historically went unchecked
    // everywhere else in the window. The lookup is by OFFSET for exactly this reason.
    let text = "/// the deleted `a_name_that_is_long_enough_ok` was historical.\n\
                /// See `a_name_that_is_long_enough_ok` for the current rule.\n";
    let v = violations(text, &HashSet::new());
    assert_eq!(
        v.len(),
        1,
        "the second occurrence sits in a clause with no marker of its own: {v:?}"
    );
    assert!(v[0].contains("f.rs:2"), "flagged at the live line: {v:?}");
}
#[test]
fn a_marker_word_matches_only_at_a_word_boundary() {
    // "performer" ends in "former"; `meter.rs` really does write "unreplaced". A substring rule
    // made both of them excuse whatever citation they shared a clause with.
    for prose in ["the performer", "still unreplaced", "predeletion"] {
        let text = format!("/// {prose} `a_name_that_is_long_enough_to_count`\n");
        assert_eq!(
            violations(&text, &HashSet::new()).len(),
            1,
            "{prose:?} embeds a marker mid-word and must not excuse"
        );
    }
    let text = "/// formerly `a_name_that_is_long_enough_to_count`\n";
    assert!(
        violations(text, &HashSet::new()).is_empty(),
        "a marker used as a real word still excuses, prefix-matched (formerly/retires/supersedes)"
    );
}
#[test]
fn a_delimiter_inside_a_code_span_does_not_split_a_clause() {
    // clause splitting is backtick-aware, so a marker cannot be separated from its name by
    // punctuation that is part of an expression -- nor can a clause open mid-span and invert the
    // prose/code parity `has_marker` reads (a real, measured misfire on `document.rs`).
    let text = "/// compares `search_summary().borrow()` against the deleted \
                `a_dead_name_that_is_long_enough`\n";
    assert!(
        violations(text, &HashSet::new()).is_empty(),
        "an unbalanced clause would read `deleted` as code and flag a deliberate historical mention"
    );
}
#[test]
fn used_to_is_not_a_historical_marker() {
    // it has two senses and only one excuses; see `HISTORICAL_MARKERS`' own comment.
    let text = "/// This helper is used to witness `a_name_that_is_long_enough_to_count`\n";
    assert_eq!(
        violations(text, &HashSet::new()).len(),
        1,
        "instrumental \"used to\" must not excuse a dangling citation"
    );
    let excused = "/// `a_name_that_is_long_enough_to_count` was deleted here\n";
    assert!(
        violations(excused, &HashSet::new()).is_empty(),
        "an unambiguous marker still excuses"
    );
}
#[test]
fn the_length_cutoff_admits_a_real_test_name_and_rejects_an_ordinary_identifier() {
    // `compile_error_surfaces` (22) is a real cited test; `consecutive_failures` (20) is a
    // struct field that merely looks like one in backticks.
    assert_eq!(citations("`compile_error_surfaces`").len(), 1);
    assert!(citations("`consecutive_failures`").is_empty());
}
