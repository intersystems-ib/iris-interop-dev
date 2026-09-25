//! #400: `iris_doc(mode=get)` with a `names` array reported `success: true` on a partial batch and
//! carried no top-level indication that anything had failed.
//!
//! Measured 2026-09-25, namespace USER (three documents put first, the rest absent):
//!
//! ```text
//! 2 present            -> success true, 2 entries with content
//! 1 present, 1 absent  -> success true, keys [documents, namespace, success], NO signal
//! 3 present, 7 absent  -> success true, 10 entries of which 7 carry an error, NO signal
//! 2 absent (all)       -> success false, NOT_FOUND, "None of the 2 requested documents ..."
//! ```
//!
//! `mode=delete` reports `success: false` for the same partial condition, so the tool distinguished
//! 0-of-N but not K-of-N.
//!
//! `success: true` is kept: the return site has always documented why ("some documents WERE read"),
//! and changing it is a contract decision, not a bug fix. This is the additive half — the summary
//! now says what happened.

use iris_agentic_dev_core::tools::doc::batch_counts;

const DOC_SRC: &str = include_str!("../src/tools/doc.rs");

fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_measured_partial_case_is_no_longer_silent() {
    // 10 requested, 3 read — the case measured on the instance.
    assert_eq!(batch_counts(10, 3), (10, 3, 7));
}

#[test]
fn a_complete_batch_reports_zero_failed() {
    assert_eq!(batch_counts(2, 2), (2, 2, 0));
    assert_eq!(batch_counts(1, 1), (1, 1, 0));
}

#[test]
fn an_entirely_failed_batch_reports_all_of_them_failed() {
    assert_eq!(batch_counts(4, 0), (4, 0, 4));
}

#[test]
fn an_empty_request_is_not_a_failure() {
    assert_eq!(batch_counts(0, 0), (0, 0, 0));
}

#[test]
fn a_read_count_cannot_exceed_what_was_requested() {
    // Defensive: `failed` is computed by subtraction, so an over-count would underflow a usize and
    // panic in debug or wrap to a vast number in release. Clamping keeps the arithmetic honest.
    assert_eq!(batch_counts(3, 9), (3, 3, 0));
}

#[test]
fn both_batch_return_paths_carry_the_counts() {
    // The partial path AND the all-failed path. Reporting on only one would leave a caller unable
    // to trust the keys' presence — the same half-applied shape as #362, #384, #386, #392 and #398.
    let src = without_comments(DOC_SRC);
    let occurrences = src.matches("\"failed\":").count();
    assert!(
        occurrences >= 2,
        "both the partial and the all-failed payloads must state the counts; found {occurrences}"
    );
    assert!(
        src.contains("batch_counts(p.names.len(), ok_count)"),
        "the partial path must derive its counts from the ok_count already tracked"
    );
}

#[test]
fn the_partial_path_still_reports_success_true() {
    // Deliberate and documented: some documents WERE read. Flipping it is a contract change, left
    // to the repo owner on the issue. This test exists so a later edit cannot flip it silently.
    let src = without_comments(DOC_SRC);
    let anchor = src
        .find("let (requested, read, failed) = batch_counts(p.names.len(), ok_count);")
        .expect("the partial path must compute the counts");
    let after = &src[anchor..anchor + 400];
    assert!(
        after.contains("\"success\": true"),
        "the partial path keeps success: true — see #400 for why, and change it deliberately"
    );
}
