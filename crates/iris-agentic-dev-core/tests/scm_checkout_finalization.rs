//! #342: a failed `AfterUserAction` must not read as a committed checkout, and neither reader may
//! drop the errors after the first.
//!
//! Two defects at two call sites doing the same job — `doc.rs`'s elicitation-resume path and
//! `scm.rs`'s checkout path — where the two DISAGREED about error handling. `scm.rs` had a real
//! `Err(e) => return err_json(...)` arm; `doc.rs` had `if let Ok(out) = ...`, which skipped the whole
//! block on an error and fell through to `checkout_cache.mark(...)`, recording a checkout that never
//! happened and then writing on that basis.
//!
//! The comment above that call explains why it exists: without AfterUserAction the write hits
//! `ERROR #5865 "not checked out of source control"`. When the call FAILED we got exactly that, plus
//! a poisoned cache entry making the next write skip its pre-write probe.
//!
//! ## The truncation, measured
//!
//! `after_user_action_code` ends with `write $system.Status.GetErrorText(sc)`. On IRIS 2026.1, with a
//! 2-error chain built via `AppendStatus`:
//!
//! ```text
//! GetErrorText(sc) = "ERROR #5001: first cause\r\nERROR #5001: second cause"
//! ```
//!
//! The whole chain, CRLF-joined — so `out.lines().next()` reported error 1 and discarded the rest.
//! On a checkout failure the specific cause is frequently the later element while the first is a
//! generic wrapper.
//!
//! ## Why this is a source-level guard
//!
//! Driving either path end to end needs a live SCM provider, which no CI instance has. What CAN be
//! pinned without one is that neither reader takes a first line and that neither swallows an `Err` —
//! both are properties of the source, and both are what actually regressed.

use std::path::{Path, PathBuf};

fn src(rel: &str) -> String {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — this guard must fail, not skip",
            p.display()
        )
    })
}

/// Every block that finalizes a checkout: the code leading up to each `SCM_CHECKOUT_FAILED` report.
///
/// ANCHORED ON THE CONSUMER, not on `after_user_action_code`. The first version anchored on the call
/// and took a forward window — and in `scm.rs` the first occurrence of that name is the function
/// DEFINITION, so the window landed nowhere near the consumer. The control assertion caught it,
/// which is the only reason the guard was not quietly measuring the wrong text.
///
/// Returns one window per report site, so a file with two finalizers has both checked rather than
/// only the first.
fn finalization_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("SCM_CHECKOUT_FAILED") {
        let at = from + rel;
        // The span from the CALL to its report — not a fixed-size window. A 1400-byte lookback
        // reached past this consumer into the UserAction read at scm.rs:376, which takes a first
        // line LEGITIMATELY (its generator writes a single-line action code, an asymmetry pinned by
        // `the_two_generators_disagree_about_empty`). Scoping to the statement is what makes the
        // assertion about the code it names. Third time today a guard here checked a haystack
        // larger than its claim.
        let start = text[..at]
            .rfind("after_user_action_code")
            .expect("every SCM_CHECKOUT_FAILED must follow an after_user_action_code call");
        // COMMENTS STRIPPED. A construct named in prose is not a construct in code, and this bit
        // twice in one day: the explanatory comment for this very fix says "not `lines().next()`",
        // which tripped the assertion, and a comment in execute_redirect_hint broke that file's
        // pattern-count guard the same way. Any guard that greps source must say so explicitly.
        let code_only: String = text[start..at]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.push(code_only);
        from = at + "SCM_CHECKOUT_FAILED".len();
    }
    out
}

#[test]
fn neither_checkout_finalizer_reads_only_the_first_line() {
    for file in ["doc.rs", "scm.rs"] {
        let blocks = finalization_blocks(&src(&format!("tools/{file}")));
        // CONTROL: there is at least one finalizer in each file, or the loop is vacuous.
        assert!(
            !blocks.is_empty(),
            "{file} reports no SCM_CHECKOUT_FAILED at all — either the finalizer moved or this \
             guard is measuring the wrong file"
        );
        for block in &blocks {
            assert!(
                !block.contains("lines().next()"),
                "{file} takes the FIRST LINE of after_user_action_code's output. That output is \
             $system.Status.GetErrorText(sc), which returns the whole %Status chain CRLF-joined \
             (measured on 2026.1), so every error after the first is discarded — and the specific \
             cause of a checkout failure is frequently the later one (#342)."
            );
        }
    }
}

#[test]
fn neither_checkout_finalizer_swallows_the_error_arm() {
    for file in ["doc.rs", "scm.rs"] {
        let blocks = finalization_blocks(&src(&format!("tools/{file}")));
        assert!(
            !blocks.is_empty(),
            "control: {file} must contain a finalizer"
        );
        for block in &blocks {
            assert!(
            !block.contains("if let Ok("),
            "{file} finalizes the checkout inside `if let Ok(`, so an Err — transport failure, 401, \
             timeout — skips the block entirely and execution falls through to \
             checkout_cache.mark(), recording a checkout that never happened and writing on that \
             basis (#342). Handle the Err arm."
        );
        }
    }
}

/// The positive half: both paths must still treat EMPTY output as success. `GetErrorText` returns ""
/// for an OK status, so an empty reply is the normal successful case — the asymmetry that
/// `the_two_generators_disagree_about_empty` pins in scm.rs. Without this, "report everything as a
/// failure" would satisfy the two guards above.
#[test]
fn both_finalizers_still_treat_empty_output_as_success() {
    for file in ["doc.rs", "scm.rs"] {
        let blocks = finalization_blocks(&src(&format!("tools/{file}")));
        assert!(
            !blocks.is_empty(),
            "control: {file} must contain a finalizer"
        );
        for block in &blocks {
            assert!(
            block.contains("is_empty()"),
            "{file} no longer distinguishes empty output from an error string. GetErrorText is \"\" \
             for a success status, so dropping that check turns every successful checkout into \
             SCM_CHECKOUT_FAILED"
        );
        }
    }
}
