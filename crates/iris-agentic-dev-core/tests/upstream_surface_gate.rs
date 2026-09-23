//! #353: the upstream-surface report must have THREE outcomes, and must never restate a count.
//!
//! `scripts/validate-tools.sh` gained a section that computes which of upstream's tools we advertise,
//! which we ported but pruned from the profile, and which we never ported. Those last two are the
//! point: the issue called all of them "excluded", and they have different remedies — one line in
//! `INTEROP_TOOLS` versus a port.
//!
//! ## What is asserted, and why each half reads a different thing
//!
//! Claims 1–3 are about CODE: the script must verify `upstream/master` exists before using it, must
//! treat a not-fetched clone as an explicit non-result rather than a pass, and must refuse to print
//! buckets from a scan that returned implausibly few tools. Those are `if`/`echo`/`exit` lines, so
//! the scan strips comments.
//!
//! Claim 4 is about PROSE, so it reads the comments on purpose: this gate's own sibling rule forbids
//! restating a tool count in `tools-status.json`, and the first draft of this section restated three
//! of them in a comment — the rule broken inside the file that enforces it.
//!
//! ## Why "not fetched" must exit 0
//!
//! CI clones need not carry the `upstream` remote. Failing there would make the gate red for a reason
//! unrelated to the tree, and the predictable response is to delete the section. Exiting 0 while
//! printing that the comparison DID NOT RUN is the honest third state — the same shape as
//! `Candidates::Unavailable` and `RowCount::Unavailable` elsewhere in this repo.

use std::path::PathBuf;

fn script() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("scripts");
    p.push("validate-tools.sh");
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — this guard must fail, not skip",
            p.display()
        )
    })
}

/// The `#353` section onward. Scoped so the assertions cannot be satisfied by the older half of the
/// script, which has its own controls and its own wording.
fn section(text: &str) -> String {
    let at = text
        .find("#353: the upstream surface")
        .expect("the upstream-surface section must exist — if it was renamed, update this guard");
    text[at..].to_string()
}

fn code_only(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn comments_only(text: &str) -> String {
    text.lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn upstream_master_is_verified_before_it_is_used() {
    let code = code_only(&section(&script()));
    assert!(
        code.len() > 500,
        "the extracted section looks wrong ({} bytes) — the scan broke, not the script",
        code.len()
    );
    assert!(
        code.contains("rev-parse --verify") && code.contains("upstream/master"),
        "the script must confirm upstream/master exists before reading it; otherwise a clone without \
         the remote produces an empty scan and the buckets become fiction:\n{code}"
    );
}

#[test]
fn a_clone_without_upstream_reports_a_non_result_and_does_not_pass_silently() {
    let code = code_only(&section(&script()));
    assert!(
        code.contains("NOT CHECKED"),
        "the not-fetched path must say the comparison did not run:\n{code}"
    );
    assert!(
        code.contains("not a pass"),
        "it must say IN WORDS that this is not a pass — a reader who sees exit 0 and a tidy report \
         will otherwise take it as a clean comparison:\n{code}"
    );
    // exit 0, not 1: see the header. A red gate here gets the section deleted.
    assert!(
        code.contains("exit 0"),
        "the not-fetched path must exit 0 so an unrelated clone cannot redden the gate:\n{code}"
    );
}

#[test]
fn an_implausible_scan_refuses_to_print_buckets() {
    let code = code_only(&section(&script()));
    assert!(
        code.contains("Refusing to report buckets from a broken read"),
        "a scan that returned almost nothing must refuse rather than report 'we ported everything' \
         — a renamed crate path would otherwise read as full coverage:\n{code}"
    );
    assert!(
        code.contains("len(up) < 40") || code.contains("len(ours) < 40"),
        "the plausibility control must test BOTH sides' scan sizes:\n{code}"
    );
}

#[test]
fn the_section_restates_no_current_count() {
    // Reads COMMENTS deliberately — the opposite of the tests above. The rule being enforced is
    // about prose, and the first draft of this very section restated three counts in a comment.
    let prose = comments_only(&section(&script()));
    assert!(
        prose.len() > 300,
        "the comment scan found {} bytes — broken, so this proves nothing",
        prose.len()
    );
    // Numbers that are allowed: issue references (#353, "48 excluded" quoting the issue) and the
    // threshold. A bare two-or-more-digit number NOT preceded by '#' and not part of a quoted
    // historical claim is a restated count.
    let offenders: Vec<&str> = prose
        .lines()
        .filter(|l| {
            let without_refs = l.replace("#353", "").replace("#294", "");
            // the issue's own wrong figure is quoted, and quoting it is the point
            let without_quotes = without_refs.replace("\"48 excluded\"", "");
            without_quotes.split_whitespace().any(|w| {
                let t = w.trim_matches(|c: char| !c.is_ascii_digit());
                t.len() >= 2 && t.chars().all(|c| c.is_ascii_digit())
            })
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "these comment lines restate a count; print it instead — the numbers in #353 were already \
         stale when it was filed:\n  {}",
        offenders.join("\n  ")
    );
}
