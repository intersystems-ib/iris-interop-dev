//! #398: `iris_symbols`, `iris_doc_search` and `iris_symbols_local` returned a capped list with
//! `count` equal to the number returned and nothing distinguishing a complete answer from a
//! truncated one.
//!
//! Measured 2026-09-25, namespace USER, against SQL ground truth:
//!
//! ```text
//! iris_symbols(query="Ens*")     -> 20, count 20   while %Dictionary.ClassDefinition has 1518
//! iris_symbols(query="%*")       -> 20, count 20   while it has 6846
//! iris_doc_search(term="string") -> 20, count 20   while 133 CompiledClass rows match
//! iris_symbols(query="Ens*", limit=5000) -> 1518   (the cap is on the result set, not the query)
//! ```
//!
//! The cap itself was documented — `iris_doc_search`'s `limit` schema even gives a rationale — so
//! this is about the RESPONSE, which could not say which case it was. Two sibling list tools
//! already carry the signal:
//!
//! ```text
//! iris_lookup_manage(list_tables) -> [count, success, tables, total_count, truncated]
//! iris_credential_list            -> [count, credentials, success, total_count, truncated]
//! ```

use iris_agentic_dev_core::tools::{probe_limit, take_with_truncation};

const MOD_SRC: &str = include_str!("../src/tools/mod.rs");

fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── the probe ───────────────────────────────────────────────────────────────────────────────

#[test]
fn the_probe_asks_for_exactly_one_more_row() {
    assert_eq!(probe_limit(20), 21);
    assert_eq!(probe_limit(1), 2);
    assert_eq!(probe_limit(0), 1);
}

#[test]
fn the_probe_cannot_overflow() {
    assert_eq!(
        probe_limit(usize::MAX),
        usize::MAX,
        "saturating, not wrapping"
    );
}

// ─── the boundary that carries the meaning ───────────────────────────────────────────────────

#[test]
fn exactly_the_limit_is_not_truncated() {
    // THE case the whole change exists for. A `limit + 1` query that returns exactly `limit` rows
    // proves there is no further match — reporting truncated here would be a false alarm, and it
    // is the difference between "20 matched" and "20 of 1518 matched".
    let rows: Vec<u32> = (0..20).collect();
    let (out, truncated) = take_with_truncation(rows, 20);
    assert_eq!(out.len(), 20);
    assert!(
        !truncated,
        "20 rows from a 21-row query means 20 is the whole answer"
    );
}

#[test]
fn one_row_past_the_limit_is_truncated_and_is_not_returned() {
    let rows: Vec<u32> = (0..21).collect();
    let (out, truncated) = take_with_truncation(rows, 20);
    assert!(truncated, "the probe row came back, so more matched");
    assert_eq!(
        out.len(),
        20,
        "the probe row must not be handed to the caller"
    );
    assert_eq!(
        out.last(),
        Some(&19),
        "and the returned rows are the first 20"
    );
}

#[test]
fn fewer_than_the_limit_is_never_truncated() {
    for n in [0usize, 1, 5, 19] {
        let rows: Vec<u32> = (0..n as u32).collect();
        let (out, truncated) = take_with_truncation(rows, 20);
        assert_eq!(out.len(), n);
        assert!(
            !truncated,
            "{n} rows is short of the cap, so nothing was dropped"
        );
    }
}

#[test]
fn a_zero_limit_returns_nothing_and_reports_truncation_when_matches_exist() {
    let (out, truncated) = take_with_truncation(vec![1, 2, 3], 0);
    assert!(out.is_empty());
    assert!(
        truncated,
        "matches existed and none were returned — that is truncation"
    );
}

// ─── the wiring: all three, or the suite fails ───────────────────────────────────────────────

#[test]
fn every_capped_search_asks_for_the_probe_row() {
    // Asserting all three together on purpose. Fixing one and leaving its siblings is the shape
    // behind #362, #384, #386, #392, #394 and #396 — a half-applied fix makes the untouched tools
    // look MORE trustworthy, not less.
    let src = without_comments(MOD_SRC);
    for (tool, call) in [
        (
            "iris_symbols",
            "translate_symbols_query(probe_limit(p.limit)",
        ),
        (
            "iris_doc_search classes",
            "class_sql(&p.term, p.within.as_deref(), probe_limit(limit))",
        ),
        (
            "iris_doc_search methods",
            "method_sql(&p.term, within, probe_limit(limit))",
        ),
        (
            "iris_symbols_local",
            "scan_workspace(&workspace, &p.query, probe_limit(limit))",
        ),
    ] {
        assert!(
            src.contains(call),
            "{tool} must ask for the probe row, or its truncation flag can never be true: \
             expected to find {call:?}"
        );
    }
}

/// The body of one handler, so a payload assertion cannot be satisfied by a sibling's payload.
fn handler_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} is not in this file any more"));
    let rest = &src[start + signature.len()..];
    let end = rest.find("\n    async fn ").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn every_capped_search_reports_the_flag_in_its_own_payload() {
    // Scoped per handler rather than counted across the file: a global count would be satisfied by
    // the two sibling list tools, which live in interop.rs and already had the field.
    let src = without_comments(MOD_SRC);
    for sig in [
        "    async fn iris_symbols(",
        "    async fn iris_doc_search(",
        "    async fn iris_symbols_local(",
    ] {
        let body = handler_body(&src, sig);
        assert!(
            body.contains("\"truncated\":"),
            "{sig} must put a truncated field in its OWN payload"
        );
        assert!(
            body.contains("take_with_truncation("),
            "{sig} must truncate what it returns, or the probe row reaches the caller"
        );
    }
}
