//! Mechanical guard for restructure R6 (`.superpowers/sdd/structural-scan.md` §3.1,
//! `restructure-plan.md`'s own "New invariants get their CI guard in the same increment that
//! creates the invariant" rule -- the same standing rule `no_raw_hay_primitives.rs` cites for its
//! own guard, and this file mirrors that one's shape deliberately): `crate::progress::Progressed`
//! has no public constructor and no public way to reach one -- the only way to mint a witness is
//! `Ascending::advance_to`/`Descending::lower_to` actually observing a real move. `pub(crate)`
//! reaches every module in this crate, though, so nothing stops a determined author from adding a
//! NEW associated function directly on `Progressed` (`Progressed::new(...)`, or any future
//! `Progressed::` path) and calling it from outside `progress.rs`, bypassing the cursor entirely --
//! `progress.rs`'s own module doc comment names this escape hatch explicitly. This guard is the
//! mitigation: a `Progressed::` path (double colon -- an associated function or constant call,
//! never a bare type name) appearing anywhere outside `src/progress.rs` fails the build.
//!
//! **What this does NOT flag, deliberately:** a bare `Progressed` type name (`Option<Progressed>`,
//! a function signature, a doc comment) -- those are legitimate everywhere the type is used as a
//! witness to hold or pass, and are not the escape hatch. Only the `::` -- a path INTO the type,
//! reaching for something callable on it -- is checked, matching the escape hatch's own exact
//! shape (`Progressed`'s own tuple field is private besides, so `Progressed(x)` construction
//! already fails to compile from outside `progress.rs` on its own; this guard is for the
//! associated-function shape the compiler cannot catch, since nothing stops a future `impl
//! Progressed { pub(crate) fn new(...) }` from being added).
//!
//! Implementation note: hand-rolled character-level scanning (this workspace's own established
//! style for this exact kind of guard -- see `no_raw_hay_primitives.rs`'s own doc comment, which
//! states the same rationale: no `regex` dependency already in this crate, not worth adding for
//! one CI check). Comments are stripped line-by-line before any structural matching, so this
//! file's own doc comment (which necessarily contains the literal needle) can never trip itself.

use std::path::{Path, PathBuf};

const NEEDLE: &str = "Progressed::";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ress-core's own Cargo.toml is one level below the workspace root")
        .to_path_buf()
}

/// Only `ress-core/src` -- `Progressed` is `pub(crate)` (never exported), so no other crate in
/// the workspace can even name it, the same reasoning `no_raw_hay_primitives.rs`'s own
/// `collect_rs_files` states for its own, structurally identical, primitive.
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

/// Every occurrence of `NEEDLE` in `content` (already known not to be the exempt file), reported
/// as `"path:line"` strings -- comment-stripped first, so this file's own doc comment (and any
/// other file's prose mentioning `Progressed::` by name) never trips it.
fn find_violations(path: &Path, content: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let stripped = strip_line_comment(line);
        if stripped.contains(NEEDLE) {
            violations.push(format!(
                "{}:{}: `{NEEDLE}` outside the progress module -- mint a `Progressed` only \
                 through `Ascending::advance_to`/`Descending::lower_to`, never a new associated \
                 function on the type itself (restructure R6, `progress.rs`'s own module doc \
                 comment)",
                path.display(),
                i + 1
            ));
        }
    }
    violations
}

#[test]
fn progressed_construction_stays_inside_the_progress_module() {
    let progress_module = workspace_root()
        .join("ress-core")
        .join("src")
        .join("progress.rs");
    let mut violations = Vec::new();
    for path in collect_rs_files() {
        if path == progress_module {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        violations.extend(find_violations(&path, &content));
    }
    assert!(
        violations.is_empty(),
        "`Progressed::` found outside src/progress.rs -- the module boundary that keeps a \
         motion witness earned, never fabricated, has been breached:\n{}",
        violations.join("\n")
    );
}

/// The guard's own discriminating test (the R4 lesson: a guard that has never been shown to
/// reject anything is not yet evidence it rejects anything -- `no_raw_hay_primitives.rs`'s own
/// `exemption_logic_distinguishes_test_and_production_call_sites` is the precedent this mirrors).
/// Neuter the needle -- comment it out, or delete the check -- and this test dies, same as
/// deleting the field it stands in for.
#[test]
fn a_hypothetical_new_associated_function_call_is_caught() {
    let fixture = r#"
fn production_site(cursor: &mut crate::progress::Ascending) -> crate::progress::Progressed {
    // a determined author reaching for the escape hatch progress.rs's own doc comment names:
    // a NEW associated function, never routed through advance_to/lower_to.
    crate::progress::Progressed::new(std::num::NonZeroU64::new(1).unwrap())
}
"#;
    let violations = find_violations(Path::new("fixture.rs"), fixture);
    assert_eq!(
        violations.len(),
        1,
        "expected exactly one violation (the bare `Progressed` return type must NOT itself \
         trip this guard): {violations:?}"
    );
    assert!(violations[0].contains("fixture.rs:5"));
}

/// The converse: a bare type name (the legitimate, common case -- holding or passing a witness)
/// must NOT trip this guard, or every ordinary use of the type outside `progress.rs` would fail
/// the build alongside the one shape that should.
#[test]
fn a_bare_type_name_is_not_flagged() {
    let fixture = r#"
fn legitimate_site(moved: Option<crate::progress::Progressed>) -> bool {
    moved.is_some()
}
"#;
    let violations = find_violations(Path::new("fixture.rs"), fixture);
    assert!(
        violations.is_empty(),
        "a bare `Progressed` type name, with no trailing `::`, must not be flagged: {violations:?}"
    );
}
