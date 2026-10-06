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

/// Production code only: every `#[cfg(test)]` module removed by brace matching.
///
/// Both of this file's remaining problems had ONE cause — it was searching its own test module.
/// `neither_finalizer_treats_silence_as_a_committed_checkout` forbids
/// `write $system.Status.GetErrorText(sc)"`, and `scm.rs`'s in-file test asserts the ABSENCE of
/// exactly that string, so the guard flagged its own control line. The windows below pulled in test
/// code for the same reason. A guard that reads the tests of the thing it guards is measuring the
/// wrong population.
fn production(rel: &str) -> String {
    let raw = src(rel);
    let mut body = raw.clone();
    while let Some(at) = body.find("#[cfg(test)]") {
        let Some(open_rel) = body[at..].find('{') else {
            body.truncate(at);
            break;
        };
        let open = at + open_rel;
        let mut depth = 0usize;
        let mut end = body.len();
        for (i, c) in body[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        body.replace_range(at..end, "");
    }
    // Comments go too. Each of these call sites now carries a comment that QUOTES the construct it
    // replaced — `aout != "SCM_UNAVAILABLE"`, `write $system.Status.GetErrorText(sc)` — because
    // that is what makes the code readable. A guard that searches prose flags the explanation of
    // its own fix, which is the commonest false witness in this repo's guards and the reason the
    // remedy is always "strip comments, then mutate one to prove they were stripped".
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.len() > raw.len() / 5 && !code.contains("#[cfg(test)]"),
        "{rel}: stripped to {} of {} bytes, test module gone = {}",
        code.len(),
        raw.len(),
        !code.contains("#[cfg(test)]")
    );
    // CONTROL: comments really were removed. This string appears ONLY in a comment at both call
    // sites, so finding it would mean the filter above did nothing.
    assert!(
        !code.contains("fall through to a committed checkout"),
        "{rel}: comments were not stripped, so every search below can match prose"
    );
    code
}

/// Every block that finalizes a checkout: from the call that PRODUCES the output to just past the
/// call that reads it.
///
/// **Re-anchored for #418.** This used to anchor on `SCM_CHECKOUT_FAILED`, the code each site
/// reported — and that code moved INTO `after_user_action_outcome` when the decision was shared, so
/// the anchor vanished and all three tests failed at their own control. The control is why that was
/// a short diagnosis rather than a puzzle: it said "{file} must contain a finalizer" instead of
/// passing on an empty window.
///
/// Anchoring on the consumer is still the rule — the first version of this helper anchored on
/// `after_user_action_code`, and in `scm.rs` the first occurrence of that name is the function
/// DEFINITION, so the window landed nowhere near a consumer. The definition is skipped explicitly.
///
/// **The window is a SPAN, not a fixed lookaround**, and that mattered: a symmetric ±260 bytes
/// reached backwards out of the `scm.rs` resume finalizer into an earlier block that legitimately
/// maps a transport failure to `SCM_UNAVAILABLE` — correct code, flagged by a guard whose window was
/// wider than its claim. The span starts at the `xecute` / `execute_via_generator` call whose output
/// is being read, which is what "the finalizer block" means.
fn finalization_blocks(text: &str) -> Vec<String> {
    const CALL: &str = "after_user_action_outcome(";
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(CALL) {
        let at = from + rel;
        from = at + CALL.len();
        if text[..at].ends_with("fn ") {
            continue; // the definition itself
        }
        // Back to whichever producing call is nearest.
        let start = ["xecute(", "execute_via_generator("]
            .iter()
            .filter_map(|p| text[..at].rfind(p))
            .max()
            .unwrap_or_else(|| at.saturating_sub(200));
        let end = text[at..]
            .find("\n        }")
            .map(|i| at + i)
            .unwrap_or(text.len())
            .min(at + 400);
        let w = text[start..end].to_string();
        // CONTROL: the span is a block, not a stray byte range and not half the file.
        assert!(
            w.len() > 40 && w.len() < text.len() / 10,
            "a finalizer window is {} of {} bytes, which is not one block",
            w.len(),
            text.len()
        );
        out.push(w);
    }
    out
}

/// Both finalizers read the output through the ONE shared function, and neither takes a first line.
///
/// The property has not changed since #342 — the whole `%Status` chain must survive, because on a
/// checkout failure the specific cause is frequently the later element while the first is a generic
/// wrapper. What changed is where it is enforced: `after_user_action_outcome` reads a labelled
/// record, so there is no first line at either site to be wrong about.
#[test]
fn neither_checkout_finalizer_reads_only_the_first_line() {
    for file in ["doc.rs", "scm.rs"] {
        let text = production(&format!("tools/{file}"));
        let blocks = finalization_blocks(&text);
        assert!(
            !blocks.is_empty(),
            "control: {file} must contain a finalizer that calls after_user_action_outcome"
        );
        for block in &blocks {
            assert!(
                !block.contains("lines().next()"),
                "{file} takes the FIRST LINE of the finalizer's output:\n{block}"
            );
        }
    }
    // Follow the delegation one hop: the shared reader must not take a first line either, or the
    // assertion above is satisfied by a call to something that does.
    let scm = production("tools/scm.rs");
    let at = scm
        .find("pub(crate) fn after_user_action_outcome")
        .expect("the shared reader must exist");
    let body = &scm[at..];
    let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
    let body = &body[..end];
    assert!(
        !body.contains("lines().next()"),
        "after_user_action_outcome reads a first line, so delegating to it does not preserve the \
         whole %Status chain:\n{body}"
    );
    // CONTROL: the window is the function and was really read.
    assert!(
        body.contains("ActionMsg::Record"),
        "the window does not contain the reader's own body:\n{body}"
    );
}

/// Neither finalizer may skip its block on an `Err`. #342: `doc.rs` had `if let Ok(out) = …`, which
/// fell through to `checkout_cache.mark(...)`, recording a checkout that never happened and then
/// writing on that basis.
#[test]
fn neither_checkout_finalizer_swallows_the_error_arm() {
    for file in ["doc.rs", "scm.rs"] {
        let text = production(&format!("tools/{file}"));
        let blocks = finalization_blocks(&text);
        assert!(
            !blocks.is_empty(),
            "control: {file} must contain a finalizer"
        );
        for block in &blocks {
            assert!(
                !block.contains("if let Ok("),
                "{file} finalizes the checkout inside `if let Ok(`, so an Err — transport failure, \
                 401, timeout — skips the block entirely and execution falls through, recording a \
                 checkout that never happened (#342):\n{block}"
            );
        }
    }
}

/// **This test's claim is INVERTED by #418 §1, and that is the point of the rename.**
///
/// It used to be `both_finalizers_still_treat_empty_output_as_success`, and it was right:
/// `after_user_action_code` ended with `write $system.Status.GetErrorText(sc)`, GetErrorText returns
/// `""` for an OK status, so empty WAS success on that path — the exact opposite of the `UserAction`
/// generator, which always wrote at least `0|`. Two call sites, two conventions, and a fix applied
/// to either one was wrong at the other.
///
/// The generator now writes an explicit `ok` field, so empty means "the snippet never ran" at both,
/// and treating it as success would be the defect rather than the correctness condition. Keeping the
/// old assertion would have pinned the convention the change removed.
///
/// **What this does NOT assert, deliberately.** Two earlier drafts forbade `is_empty()` and
/// `SCM_UNAVAILABLE` anywhere in a finalizer window. Both have legitimate uses right there — the
/// hook-output check is `is_empty()`, and the transport-failure arm maps a dead connection to
/// `SCM_UNAVAILABLE` correctly — so the assertions flagged working code, and widening or narrowing
/// the window only moved which correct line got flagged. A guard that cannot express its claim
/// without false positives does not belong; the claim it was reaching for is "empty is refused",
/// which is a property of ONE function and is stated here against that function, and behaviourally
/// in `scm.rs`'s own tests where it can be executed rather than grepped.
#[test]
fn neither_finalizer_treats_silence_as_a_committed_checkout() {
    let scm = production("tools/scm.rs");
    // The generator carries the explicit field...
    assert!(
        scm.contains("set r.ok=$select(sc=1:1,1:0)"),
        "after_user_action_code no longer writes an explicit ok field, so empty is ambiguous again"
    );
    // ...and no longer ends by writing the error text alone, which is what made empty mean success.
    assert!(
        !scm.contains(concat!("write $system.Status.GetErrorText(sc)", "\"")),
        "after_user_action_code ends by writing the error text alone, so empty means success there \
         and 'the snippet never ran' everywhere else"
    );
    // Both finalizers delegate rather than deciding for themselves.
    for file in ["doc.rs", "scm.rs"] {
        let text = production(&format!("tools/{file}"));
        assert!(
            !finalization_blocks(&text).is_empty(),
            "{file} has no finalizer that reads its outcome through after_user_action_outcome"
        );
    }
    // And the one function they delegate to refuses silence. Narrow on purpose: this is a claim
    // about the `Empty` arm of one `match`, not about what any call site happens to mention.
    let at = scm
        .find("pub(crate) fn after_user_action_outcome")
        .expect("the shared reader must exist");
    let body = &scm[at..];
    let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
    let body = &body[..end];
    let empty_arm = body
        .lines()
        .find(|l| l.trim_start().starts_with("ActionMsg::Empty =>"))
        .unwrap_or_else(|| {
            panic!("the reader has no Empty arm, so silence falls through:\n{body}")
        });
    assert!(
        empty_arm.contains("Err("),
        "the reader answers silence with something other than an error (`{}`), which is how a \
         snippet that never ran becomes a committed checkout",
        empty_arm.trim()
    );
    // CONTROL: the window is the function body and the arm search can see a non-Empty arm too.
    assert!(
        body.contains("ActionMsg::Record"),
        "the window does not contain the reader's own body:\n{body}"
    );
}
