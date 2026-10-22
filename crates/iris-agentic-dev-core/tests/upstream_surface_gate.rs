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

/// Remove `#<digits>` runs — issue references, which are not counts.
///
/// Written out rather than regexed so this test keeps no dependency beyond std, matching the rest
/// of the file.
fn strip_issue_refs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '#' {
            out.push(c);
            continue;
        }
        // A '#' followed by at least one digit is a reference: drop the '#' and the digits.
        if chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn the_issue_reference_allowance_is_a_rule_not_a_list() {
    // The regression this replaces: a hardcoded allowlist passed #353 and failed #531.
    assert_eq!(strip_issue_refs("see #353 and #531"), "see  and ");
    // A real restated count must still survive stripping, or the guard above goes blind.
    assert_eq!(strip_issue_refs("31 tools"), "31 tools");
    // A bare '#' is not a reference and must be kept.
    assert_eq!(strip_issue_refs("a # b"), "a # b");
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
    // Numbers that are allowed: issue references (#353, #531, …), the quoted historical figure,
    // and the threshold. A bare two-or-more-digit number that is NOT an issue reference is a
    // restated count.
    //
    // The allowance used to be a literal list — `.replace("#353", "").replace("#294", "")` — which
    // pinned a SPELLING rather than the rule it stood for. #531 then cited its own issue number in
    // a comment in the section and this test failed, flagging `#531` as a restated count. A guard
    // that forbids every issue number nobody has hardcoded is a guard the next person deletes, so
    // the allowance is now the rule: strip `#` followed by digits, whatever the digits are.
    let offenders: Vec<&str> = prose
        .lines()
        .filter(|l| {
            // the issue's own wrong figure is quoted, and quoting it is the point
            let line = l.replace("\"48 excluded\"", "");
            let without_refs = strip_issue_refs(&line);
            without_refs.split_whitespace().any(|w| {
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

/// #352: both halves of the comparison must read the SAME ref.
///
/// `ours` comes from `git show HEAD:…`, and INTEROP_TOOLS was read from the WORKTREE. While a change
/// was uncommitted the two disagreed: a tool implemented and advertised in the working tree appeared
/// as advertised AND never-ported at once, and the #353 join then reported it UNCLASSIFIED — a gate
/// failure about a tool that is in the profile. Measured while adding `stream_inspect`: exit 1 before
/// the commit, exit 0 after, with no source change in between.
///
/// One source, and the caveat printed, rather than two that agree only after a commit.
#[test]
fn both_halves_of_the_surface_comparison_read_the_same_ref() {
    let s = script();
    // The UPSTREAM section only. This file has two python blocks and each has its own `src =` and its
    // own INTEROP_TOOLS parse — the first version of this test matched the drift check's worktree read
    // and failed on correct code, which is how a guard gets loosened until it catches nothing.
    let section_at = s
        .find("# ── #353: the upstream surface")
        .expect("the upstream section must be findable");
    let section = &s[section_at..];
    let profile_at = section
        .find("INTEROP_TOOLS[^=]")
        .unwrap_or_else(|| panic!("the upstream section must parse INTEROP_TOOLS"));
    // The assignment that feeds it: the nearest `src =` BEFORE the parse, inside this section.
    let before = &section[..profile_at];
    let src_at = before
        .rfind("src = ")
        .unwrap_or_else(|| panic!("no `src =` assignment precedes the INTEROP_TOOLS parse"));
    let src_assign = section[src_at..]
        .lines()
        .next()
        .expect("the assignment line");
    // CONTROL: the window really is the upstream section, not the whole file.
    assert!(
        !section.contains("Per-tool validation gate"),
        "the section window ran back into the earlier block"
    );
    assert!(
        src_assign.contains("git(") && src_assign.contains("HEAD:"),
        "INTEROP_TOOLS is read from the worktree while `ours` is read from HEAD, so the buckets \
         disagree until the change is committed and the #353 join reports a false UNCLASSIFIED: \
         {src_assign}"
    );
    assert!(
        !src_assign.contains("open("),
        "reading the file directly is the worktree read this guards against: {src_assign}"
    );
    // The caveat must be stated, because "at HEAD" is a real limitation for anyone running this on
    // a dirty tree.
    assert!(
        s.contains("uncommitted changes are not included"),
        "the section compares committed refs and must say so"
    );
}
