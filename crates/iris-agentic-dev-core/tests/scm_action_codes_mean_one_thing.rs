//! #418 §2/§3/§4: one classification of a `UserAction` action code, read by every site.
//!
//! ## The defect this file exists to keep closed
//!
//! Three sites read the same `action_code` and answered differently. Measured on the tree before
//! the change, the set of codes each one recognised was:
//!
//! | site | recognised | what it did with everything else |
//! |---|---|---|
//! | `scm.rs` `checkout` arm | `{0}` | presented it as a **yes/no question** |
//! | `scm.rs` `execute` arm | `{0, 1, 7}` | `SCM_ERROR "Unexpected action code N"` |
//! | `doc.rs` pre-write probe | `{0, 1, 6}` | **proceeded** — the document was written ungated |
//!
//! The intersection is `{0}`. So for every code but one, the three disagreed, and the disagreement
//! was invisible at each site because each one read reasonably on its own. The expensive case is
//! action 2: CCR rewrites its own password prompt into it, so a missing Perforce credential made
//! `iris_doc put` write a document that was never checked out, while `iris_source_control checkout`
//! asked the user yes/no about a URL.
//!
//! ## What is asserted, and why at the source
//!
//! That each site DERIVES its answer from `CodeMeaning` instead of restating the codes. A second
//! table is the live hazard: a code handled at one site and missing from another is exactly the
//! state above, and both halves read fine. Source text is where a duplicate would be, so that is
//! where this looks — the behaviour of the table itself is tested next to it, in `scm.rs`.
//!
//! The variant list is READ FROM THE ENUM, not restated here. A seventh `CodeMeaning` variant
//! therefore makes this file fail until all three sites have decided what it means, which is the
//! tripwire `ScmAction::is_write` gets from exhaustiveness and a `match` on a `u8` cannot have.

use std::path::PathBuf;

fn src(file: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools");
    p.push(file);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// A window from `start` up to `end`. Both ends are asserted present, so a moved anchor fails
/// loudly instead of silently returning a window that proves nothing.
fn window(text: &str, start: &str, end: &str, label: &str) -> String {
    let a = text
        .find(start)
        .unwrap_or_else(|| panic!("{label}: start anchor {start:?} is gone"));
    let b = text[a..]
        .find(end)
        .unwrap_or_else(|| panic!("{label}: end anchor {end:?} is gone after the start"))
        + a;
    let w = text[a..b].to_string();
    // CONTROL, per the repo's own lesson about window extraction: a window that silently ran to the
    // end of the file satisfies every `contains` below. Print the size and bound it.
    assert!(
        w.len() > 200 && w.len() < text.len() / 8,
        "{label}: window is {} of {} bytes — that is not one site",
        w.len(),
        text.len()
    );
    w
}

/// Strip `//` line comments. Every guard in this repo that forgot to has eventually passed on prose
/// that merely mentioned the construct it forbids, and these three sites are heavily commented
/// precisely because this is where the asymmetry was.
fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The variants of `CodeMeaning`, read from its declaration.
fn variants() -> Vec<String> {
    let scm = src("scm.rs");
    let body = window(
        &scm,
        "pub(crate) enum CodeMeaning {",
        "\nimpl CodeMeaning {",
        "the CodeMeaning declaration",
    );
    let v: Vec<String> = strip_comments(&body)
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(','))
        .map(|l| l.trim_end_matches(',').to_string())
        .filter(|l| {
            !l.is_empty()
                && l.chars().next().is_some_and(char::is_uppercase)
                && l.chars().all(char::is_alphanumeric)
        })
        .collect();
    // CONTROL: the parse found the real variants, not an empty list that makes every loop below
    // vacuous, and not the doc-comment prose.
    assert!(
        v.len() >= 5 && v.contains(&"NeedsUi".to_string()) && v.contains(&"NoDialog".to_string()),
        "parsed {v:?} as the variants of CodeMeaning — that is not the enum"
    );
    v
}

/// The three sites, each as a (label, source window) pair.
fn sites() -> Vec<(&'static str, String)> {
    let scm = src("scm.rs");
    let doc = src("doc.rs");
    vec![
        (
            "scm.rs `checkout` arm",
            window(
                &scm,
                "\"checkout\" => {",
                "\"execute\" => {",
                "checkout arm",
            ),
        ),
        (
            "scm.rs `execute` arm",
            window(
                &scm,
                "\"execute\" => {",
                "other => err_json(",
                "execute arm",
            ),
        ),
        (
            "doc.rs `precheck_verdict`",
            window(
                &doc,
                "fn precheck_verdict(",
                "\n/// Run the SCM pre-write check",
                "precheck_verdict",
            ),
        ),
    ]
}

/// Every site decides every meaning. This is the symmetry the three sites did not have.
#[test]
fn every_site_answers_every_meaning() {
    let variants = variants();
    eprintln!("CodeMeaning variants read from the enum: {variants:?}");
    for (label, w) in sites() {
        let code = strip_comments(&w);
        let missing: Vec<&String> = variants
            .iter()
            .filter(|v| !code.contains(&format!("CodeMeaning::{v}")))
            .collect();
        assert!(
            missing.is_empty(),
            "{label} does not say what these mean: {missing:?}. A site that is silent about a \
             meaning answers it by accident, which is the state #418 §2/§4 describe."
        );
    }
}

/// No site restates the codes. The duplicate is the hazard, not the integer itself.
#[test]
fn no_site_matches_the_raw_action_code() {
    for (label, w) in sites() {
        let code = strip_comments(&w);
        for line in code.lines() {
            let t = line.trim();
            assert!(
                !t.starts_with("action_code ==") && !t.contains("if action_code =="),
                "{label} compares the raw action code: `{t}`"
            );
            // A bare `7 => …` arm. `CodeMeaning::of` is the only place a number may appear.
            let is_int_arm = t
                .split("=>")
                .next()
                .map(|p| {
                    let p = p.trim().trim_start_matches('|').trim();
                    !p.is_empty()
                        && p.split('|')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .all(|s| s.chars().all(|c| c.is_ascii_digit()))
                })
                .unwrap_or(false);
            assert!(
                !(t.contains("=>") && is_int_arm),
                "{label} has a bare integer match arm: `{t}`"
            );
        }
        assert!(
            code.contains("CodeMeaning::of(") || code.contains("CodeMeaning::of"),
            "{label} never consults CodeMeaning, so it is classifying on its own"
        );
    }
}

/// Each site's `match` over the meaning has no catch-all, so a seventh variant does not compile
/// until all three have decided. `CodeMeaning::of` itself needs one — a `u8` has 256 values — which
/// is exactly why the tripwire has to live at the consumers.
#[test]
fn no_site_has_a_catch_all_over_the_meaning() {
    for (label, w) in sites() {
        let code = strip_comments(&w);
        for line in code.lines() {
            let t = line.trim();
            assert!(
                t != "_ => {" && !t.starts_with("_ =>"),
                "{label} has a catch-all arm (`{t}`), so a new CodeMeaning variant would inherit a \
                 verdict silently instead of failing to compile"
            );
        }
    }
    // CONTROL: `CodeMeaning::of` DOES have one, and this test would be meaningless if the search
    // could not see a catch-all at all.
    let of = window(
        &src("scm.rs"),
        "pub(crate) fn of(code: u8) -> Self {",
        "\n}",
        "CodeMeaning::of",
    );
    assert!(
        strip_comments(&of)
            .lines()
            .any(|l| l.trim().starts_with("_ =>")),
        "CodeMeaning::of has no catch-all, so the search above cannot distinguish one"
    );
}

/// The refusal is worded once. Three sites wording "this cannot be driven from here" three ways is
/// how they drifted apart the first time.
#[test]
fn the_undriveable_refusal_is_worded_in_one_place() {
    let scm = src("scm.rs");
    let prod = strip_comments(&scm);
    for c in ["SCM_NEEDS_UI", "SCM_NEEDS_INPUT"] {
        let decl = format!("const {c}: &str");
        let uses = prod.matches(c).count() - prod.matches(&decl).count();
        assert!(
            uses >= 1,
            "{c} is declared and never used — a code no site emits has no business existing"
        );
    }
    assert_eq!(
        prod.matches("fn undriveable_refusal").count(),
        1,
        "there is more than one undriveable_refusal"
    );
    // Each site reaches it rather than writing its own message.
    for (label, w) in sites() {
        let code = strip_comments(&w);
        if code.contains("CodeMeaning::Undriveable") {
            assert!(
                code.contains("undriveable_refusal"),
                "{label} names Undriveable but words its own refusal"
            );
        }
    }
}

/// `CodeMeaning::of` lists each documented code exactly once. A duplicated arm is unreachable and
/// a silently shadowed classification.
#[test]
fn each_code_is_listed_once_in_the_table() {
    let of = strip_comments(&window(
        &src("scm.rs"),
        "pub(crate) fn of(code: u8) -> Self {",
        "\n}",
        "CodeMeaning::of",
    ));
    let mut seen: Vec<u8> = Vec::new();
    for line in of.lines() {
        let t = line.trim();
        let Some(pat) = t.split("=>").next() else {
            continue;
        };
        for part in pat.split('|') {
            if let Ok(n) = part.trim().parse::<u8>() {
                assert!(
                    !seen.contains(&n),
                    "code {n} is matched twice in CodeMeaning::of"
                );
                seen.push(n);
            }
        }
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![0, 1, 2, 3, 6, 7],
        "CodeMeaning::of names {seen:?}; the hook API defines 0,1,2,3,6,7 and the rest are \
         Undriveable — a change here is a change to what the server believes IRIS said"
    );
}
