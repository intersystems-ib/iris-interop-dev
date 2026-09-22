//! #303 residual: the row count must not be obtained by writing.
//!
//! `iris_table_info(include_row_count=true)` used to get its `COUNT(*)` through
//! `execute_via_generator`, which PUTs a scratch class, compiles it, runs it and deletes it. That is
//! a real write — on an optional branch of a tool the caller experiences as a read, and on any
//! instance the caller points it at, including a Live one.
//!
//! This is the "remove the write instead of relabelling it" case: nothing about a `SELECT COUNT(*)`
//! needed a generator. `IrisConnection::query` is the Atelier `/action/query` path, needs no scratch
//! class, and already carries the transparent retry that idempotent SELECTs are safe to have (#7).
//!
//! It is deliberately narrow. The three OTHER `execute_via_generator` calls in `info.rs` are not
//! touched — whether the write gate should refuse the tools that genuinely need a generator is the
//! open policy question in #303, and this guard must not pre-empt it. Hence the scan is scoped to
//! one function, and the positive control below asserts the other three are still visible to it.
//!
//! ## The aliased column
//!
//! The SQL aliases the count (`AS row_count`). Measured on IRIS 2026.1: an un-aliased `COUNT(*)`
//! comes back as `[{"Aggregate_1": 0}]`, a generated name that would parse correctly until IRIS
//! chose to generate a different one and then silently stop matching. The guard pins the alias
//! because the failure mode it prevents is invisible.

use std::path::{Path, PathBuf};

const FORBIDDEN: &str = "execute_via_generator";

fn info_rs() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
    p.push("info.rs");
    p
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — this guard must fail, not skip",
            p.display()
        )
    })
}

/// `//` comments removed.
///
/// Not optional here: the comment INSIDE `get_row_count` explains why it no longer calls
/// `execute_via_generator`, and naming the construct is what a good comment does. A guard that
/// greps raw source would fire on its own rationale — which has happened twice in this repo, once
/// to a comment reading "not `lines().next()`" in the fix for #342.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("//") {
            Some(at) => &l[..at],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `async fn get_row_count(` up to its matching brace.
///
/// Scoped to the function, not a byte window: a fixed lookahead would either cut the body short or
/// run past it into the next item, and `info.rs` has a generator call about 200 lines earlier that a
/// generous window would swallow — which would make this guard fail for the wrong reason and teach
/// the next reader to widen it.
fn get_row_count_body(text: &str) -> String {
    let at = text.find("async fn get_row_count(").expect(
        "get_row_count must exist — if it was renamed, this guard needs updating, not deleting",
    );
    let open = text[at..]
        .find('{')
        .map(|o| at + o)
        .expect("get_row_count must have a body");
    let mut depth = 0i32;
    for (off, ch) in text[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return text[open..open + off + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after get_row_count");
}

#[test]
fn the_row_count_does_not_write() {
    let text = read(&info_rs());
    let body = strip_line_comments(&get_row_count_body(&text));
    // CONTROL: the extracted body is a real body, not an empty string from a failed parse.
    assert!(
        body.len() > 200 && body.contains("COUNT(*)"),
        "the extracted get_row_count body looks wrong ({} bytes) — the scan broke rather than the \
         function being clean:\n{body}",
        body.len()
    );
    assert!(
        !body.contains(FORBIDDEN),
        "get_row_count calls {FORBIDDEN}, which PUTs and compiles a scratch class for what is a \
         pure SELECT. Use IrisConnection::query.\n{body}"
    );
    assert!(
        body.contains("iris.query("),
        "get_row_count should read through IrisConnection::query:\n{body}"
    );
    eprintln!("get_row_count body scanned: {} bytes, no write", body.len());
}

#[test]
fn the_scan_can_still_see_a_generator_call_elsewhere_in_the_file() {
    // POSITIVE CONTROL. If `execute_via_generator` stopped being findable — renamed, reformatted
    // across lines, or the comment stripper eating too much — the assertion above would pass
    // vacuously and read exactly like a correctly-fixed function. `info.rs` has other, deliberately
    // untouched generator calls; this asserts the needle is still detectable in the same file, by
    // the same stripper.
    let text = read(&info_rs());
    let whole = strip_line_comments(&text);
    let hits = whole.matches(FORBIDDEN).count();
    assert!(
        hits >= 2,
        "found only {hits} occurrence(s) of {FORBIDDEN} in info.rs outside comments. Either the \
         other generator paths were removed (then this control needs rewriting) or the needle is no \
         longer detectable, in which case `the_row_count_does_not_write` proves nothing."
    );
    eprintln!("{FORBIDDEN} still visible elsewhere in info.rs: {hits} occurrences");
}

#[test]
fn the_count_column_is_aliased() {
    let text = read(&info_rs());
    let body = strip_line_comments(&get_row_count_body(&text));
    assert!(
        body.contains("AS row_count"),
        "the COUNT(*) must be aliased. Un-aliased, IRIS names the column `Aggregate_1` — a \
         generated name that parses until IRIS generates a different one:\n{body}"
    );
    assert!(
        !body.contains("Aggregate_1"),
        "reading the generated column name is what the alias exists to avoid:\n{body}"
    );
}

#[test]
fn the_comment_stripper_does_not_eat_code() {
    // The stripper is crude on purpose (no string-literal awareness). This pins that it keeps the
    // two things this file asserts about, and that a `//` inside them would be the only hazard —
    // there is none today, and this test fails if one is introduced.
    let sample = "let a = 1; // execute_via_generator mentioned in prose\nlet b = iris.query(x);";
    let out = strip_line_comments(sample);
    assert!(
        !out.contains(FORBIDDEN),
        "prose survived stripping: {out:?}"
    );
    assert!(out.contains("iris.query(x)"), "code was eaten: {out:?}");
    assert!(
        out.contains("let a = 1;"),
        "code before a comment was eaten: {out:?}"
    );
}
