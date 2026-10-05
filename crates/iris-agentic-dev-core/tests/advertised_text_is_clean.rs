//! #378: no advertised tool description reaches a client with a run of leaked indentation.
//!
//! A Rust string continued with a trailing backslash collapses to single spaces. When that
//! continuation is COLLAPSED INTO ONE SOURCE LINE, the indentation stays inside the runtime string —
//! and seven caller-facing messages had that, found by running a tool rather than reading it:
//!
//! ```text
//! namespace_a and namespace_b are both 'USER'. Comparing a namespace with                      itself
//! ```
//!
//! ## Why this file guards DESCRIPTIONS and not "messages"
//!
//! #378 records that there is no cheap sound gate over "every string that can reach a caller":
//!
//! * A source scan cannot distinguish a collapsed continuation from a correct one without exclusion
//!   rules for whitespace test fixtures (`["", "   "]`), embedded ObjectScript whose indentation is
//!   deliberate (`"    Do ..RunUser()"`), and aligned trailing comments. Measured: the naive version
//!   reported 592 hits across 68 files, all false positives.
//! * Message CONSTANTS are a sound population but a tiny one — three of them in this crate
//!   (`TIMEOUT_HINT`, `SUSPENDED_MISMATCH_HINT`, `TABLE_NOT_FOUND_HINT`). It would not have caught
//!   any of the seven, which were literals inside functions.
//! * Error messages built at call time are only reachable by triggering their paths.
//!
//! Descriptions are the one population that is **complete, wire-facing and free**: every one of them
//! goes to every client on every `tools/list`, and `advertised_tools()` returns all of them with no
//! IRIS connection. So this asserts the surface it can assert exhaustively, and says plainly that it
//! is not the whole problem.

use iris_agentic_dev_core::tools::{IrisTools, Toolset};

/// A run of two or more spaces inside prose. Not three: a description is a single flowing sentence
/// stream, so even two is a defect there — unlike source, where `  ` is ordinary indentation.
fn leaked_run(text: &str) -> Option<(usize, String)> {
    let at = text.find("  ")?;
    let run = text[at..].chars().take_while(|c| *c == ' ').count();
    let from = at.saturating_sub(50);
    let to = (at + run + 40).min(text.len());
    Some((run, text[from..to].replace('\n', " ")))
}

#[test]
fn no_advertised_description_carries_a_run_of_leaked_indentation() {
    // Every toolset, because a description can be advertised by one and not another.
    for ts in [
        Toolset::Interop,
        Toolset::Nostub,
        Toolset::Merged,
        Toolset::Baseline,
    ] {
        let t = IrisTools::new_with_toolset(None, ts).expect("IrisTools::new_with_toolset");
        let tools = t.advertised_tools();
        // CONTROL: a zero here must not come from an empty list.
        assert!(
            tools.len() >= 20,
            "{ts:?} advertised only {} tools, so a clean result would prove nothing",
            tools.len()
        );
        let mut described = 0;
        for tool in &tools {
            let d = tool.description.clone().unwrap_or_default().to_string();
            if d.is_empty() {
                continue;
            }
            described += 1;
            if let Some((run, context)) = leaked_run(&d) {
                panic!(
                    "{ts:?} tool '{}' advertises a description with a {run}-space run — a Rust \
                     string continuation collapsed onto one source line keeps its indentation, and \
                     this text goes to every client on every tools/list. Context: …{context}…",
                    tool.name
                );
            }
        }
        // CONTROL: the descriptions were actually READ, not skipped as empty.
        assert!(
            described >= 20,
            "{ts:?}: only {described} tools carry a description, so the scan covered almost nothing"
        );
    }
}

/// The detector must fire. Without this, "no description is dirty" is indistinguishable from a
/// predicate that can never be true — which is how the first version of this scan produced 592
/// false positives and, tuned the other way, would have produced zero real ones.
#[test]
fn the_detector_flags_a_collapsed_continuation_and_leaves_clean_prose_alone() {
    // The real text, as it came back from the tool before the fix.
    let leaked = "namespace_a and namespace_b are both 'USER'. Comparing a namespace with                      itself always reports in_sync.";
    let (run, _) = leaked_run(leaked).expect("the detector must flag the measured defect");
    assert!(run >= 3, "run was {run}");

    // And the repaired form, which is what a correct `\` continuation produces.
    assert!(
        leaked_run("Comparing a namespace with itself always reports in_sync, which says nothing.")
            .is_none(),
        "clean prose must not be flagged"
    );
    // A newline is not a leaked run: descriptions may legitimately contain one.
    assert!(leaked_run("line one\nline two").is_none());
}
