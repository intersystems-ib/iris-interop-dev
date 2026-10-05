//! #408: `iris_production_item` mutated the live configuration and left the production CLASS behind.
//!
//! `add`, `remove`, `set_settings`, `enable` and `disable` all ended in `tProd.%Save()`, which writes
//! the **extent** (`Ens_Config.Item`) — the live configuration — and then optionally
//! `Ens.Director.UpdateProduction` to apply it. None of them called `SaveToClass()`, the step the
//! Management Portal performs, so the class's `XData ProductionDefinition` kept the old item list.
//! `SaveToClass` appeared **0 times** in `crates/` (control: `iris_production_item` appeared 66).
//!
//! ## Staleness is the mild half
//!
//! The XData block is what a **compile** of the production class replays into the extent. So a stale
//! class plus any later `iris_compile` silently reverts the mutation. Worse, the obvious next step —
//! the skills plugin's drift guidance, "iris_doc get the production class and write it to src/" —
//! writes the stale class to disk *first*, making it the source of truth. The reporter hit exactly
//! that: a hand-rebuilt XML, lost `&lt;-&gt;` escaping, `ErrInvalidProduction`, and a first message
//! delivery that took 243 s instead of about 40 s.
//!
//! ## Why a failed class save must not fail the action
//!
//! By the time `SaveToClass` runs, `%Save()` has already succeeded and the change may already be
//! live. Reporting failure would tell the caller to retry a mutation that has happened. Swallowing it
//! is the same defect in new clothes. So the action still reports OK and carries `class_saved: false`
//! with the reason — a third state, which is the shape this repo keeps arriving at.
//!
//! ## Why the marker comes AFTER the OK line
//!
//! A `%Status` chain's text is multi-line. A marker written *before* the OK line would push the OK
//! line past wherever a reader looks, which is the truncation #347 fixed elsewhere; with the marker
//! last, an arbitrarily long reason cannot corrupt the production name.

use iris_agentic_dev_core::tools::interop::{
    attach_class_save, build_add_item_code, build_remove_item_code, build_set_enabled_code,
    build_set_settings_code, class_not_saved_warning, read_class_save_marker,
};
use std::collections::HashMap;

fn settings() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("Adapter.Port".to_string(), "47213".to_string());
    m
}

/// Every builder that mutates a production, by the name the action carries.
fn all_mutators() -> Vec<(&'static str, String)> {
    vec![
        (
            "add",
            build_add_item_code(
                "HospitalCare.Production",
                "Census.BS.HL7",
                "EnsLib.HL7.Service.TCPService",
                false,
                None,
                Some("Census"),
                &settings(),
            ),
        ),
        (
            "remove",
            build_remove_item_code("HospitalCare.Production", "Census.BS.HL7"),
        ),
        (
            "set_settings",
            build_set_settings_code(
                "HospitalCare.Production",
                "Census.BS.HL7",
                &settings(),
                true,
            ),
        ),
        (
            "enable",
            build_set_enabled_code("HospitalCare.Production", "Census.BS.HL7", true),
        ),
        (
            "disable",
            build_set_enabled_code("HospitalCare.Production", "Census.BS.HL7", false),
        ),
    ]
}

#[test]
fn every_mutating_action_saves_the_production_class() {
    // The property of the SET, not of the one builder a fix happened to start from.
    // #409 was a sibling asymmetry in this same file; five arms is five chances to repeat it.
    for (action, code) in all_mutators() {
        assert!(
            code.contains("Set tSCC=tProd.SaveToClass()"),
            "{action} does not save the production class:\n{code}"
        );
        // Calling it and discarding the %Status would satisfy the line above while losing
        // every failure — the class would silently stay stale and the envelope would still
        // say class_saved: true. So the verdict must actually be examined. (#409's parity
        // assertion in this same crate passed with its subject deleted, because it matched a
        // string that appeared elsewhere in the output; an assertion has to name the thing it
        // means.)
        assert!(
            code.contains("$$$ISERR(tSCC)"),
            "{action} calls SaveToClass but never checks its %Status:\n{code}"
        );
    }
}

#[test]
fn the_class_is_saved_after_the_extent_and_before_the_production_is_updated() {
    for (action, code) in all_mutators() {
        let save = code.find("tProd.%Save()").expect("the extent save");
        let cls = code.find("tProd.SaveToClass()").expect("the class save");
        assert!(
            save < cls,
            "{action}: the class must be written from a production that was already saved \
             (%Save@{save}, SaveToClass@{cls})"
        );
        // set_settings with apply=false emits no UpdateProduction at all, so this is
        // conditional on the call being there — but when it is, the order is the Portal's.
        if let Some(upd) = code.find("UpdateProduction") {
            assert!(
                cls < upd,
                "{action}: the class save must precede UpdateProduction (SaveToClass@{cls}, update@{upd})"
            );
        }
    }
}

#[test]
fn a_class_that_could_not_be_saved_does_not_abort_the_action() {
    for (action, code) in all_mutators() {
        // Scoped to the lines that HANDLE the class save, not to everything between it and
        // the OK write: that wider window also contains the UpdateProduction guard, which
        // legitimately does `Quit` and does write `ERROR:`, so the assertion would be
        // asserting something it does not mean.
        let handling: Vec<&str> = code
            .lines()
            .filter(|l| l.contains("tSCC") || l.contains("tClsErr"))
            .collect();
        assert!(
            handling.len() >= 2,
            "{action}: expected the class save and its report; a filter that matches nothing \
             passes vacuously. Got {handling:?}\n{code}"
        );
        for line in &handling {
            // `Quit` inside an `If {}` returns from the method (measured), so a Quit here
            // would turn an already-applied mutation into a reported failure.
            assert!(
                !line.contains("Quit"),
                "{action}: a failed class save must not abort — the mutation already landed: {line}"
            );
            assert!(
                !line.contains("ERROR:"),
                "{action}: a failed class save must not be reported as an action error: {line}"
            );
        }
    }
}

#[test]
fn the_class_verdict_is_reported_after_the_ok_line() {
    for (action, code) in all_mutators() {
        let ok = code.find("Write \"OK").expect("the OK write");
        let marker = code
            .find("CLASS_NOT_SAVED:")
            .unwrap_or_else(|| panic!("{action} never reports the class verdict:\n{code}"));
        assert!(
            ok < marker,
            "{action}: a multi-line %Status reason must not displace the OK line \
             (OK@{ok}, marker@{marker})"
        );
    }
}

#[test]
fn no_marker_means_the_class_was_saved() {
    let (head, err) = read_class_save_marker("OK:HospitalCare.Production\n");
    assert_eq!(head, "OK:HospitalCare.Production");
    assert!(
        err.is_none(),
        "absence of the marker is a positive statement that the class was saved, got {err:?}"
    );
}

#[test]
fn the_production_name_survives_a_marker() {
    let (head, err) =
        read_class_save_marker("OK:HospitalCare.Production\nCLASS_NOT_SAVED:ERROR #5002\n");
    assert_eq!(
        head.strip_prefix("OK:"),
        Some("HospitalCare.Production"),
        "the existing parser must still find the production name"
    );
    assert_eq!(err, Some("ERROR #5002"));
}

#[test]
fn a_multiline_reason_survives_whole() {
    let out =
        "OK:HospitalCare.Production\nCLASS_NOT_SAVED:ERROR #5002: first\nsecond line\nthird line";
    let (head, err) = read_class_save_marker(out);
    assert_eq!(head, "OK:HospitalCare.Production");
    let err = err.expect("a reason");
    // #347: a %Status chain joined by newlines lost everything after the first line.
    for line in ["ERROR #5002: first", "second line", "third line"] {
        assert!(err.contains(line), "lost {line:?} from the reason: {err:?}");
    }
}

#[test]
fn an_empty_reason_is_still_a_failure() {
    let (_, err) = read_class_save_marker("OK\nCLASS_NOT_SAVED:");
    // None means "saved". An empty reason must not decay into that.
    assert!(
        err.is_some(),
        "an empty reason must stay a failure, or a class that was NOT saved reads as saved"
    );
}

#[test]
fn a_bare_ok_still_compares_equal() {
    // set_settings and enable/disable test `head == "OK"`, so the reader must not
    // hand them trailing whitespace or a newline.
    let (head, err) = read_class_save_marker("OK\n");
    assert_eq!(head, "OK");
    assert!(err.is_none());
    let (head, _) = read_class_save_marker("OK\nCLASS_NOT_SAVED:nope");
    assert_eq!(head, "OK");
}

#[test]
fn the_envelope_says_true_when_the_class_was_saved() {
    let mut env = serde_json::json!({"success": true, "item": "Census.BS.HL7"});
    attach_class_save(&mut env, None);
    assert_eq!(env["class_saved"], serde_json::json!(true));
    assert!(
        env.get("class_error").is_none(),
        "no error field when there was no error: {env}"
    );
    assert!(env.get("warning").is_none(), "{env}");
}

#[test]
fn the_envelope_says_false_and_why_when_it_was_not() {
    let mut env = serde_json::json!({"success": true, "item": "Census.BS.HL7"});
    attach_class_save(&mut env, Some("ERROR #5002: class is deployed"));
    assert_eq!(env["class_saved"], serde_json::json!(false));
    assert_eq!(
        env["class_error"],
        serde_json::json!("ERROR #5002: class is deployed")
    );
    assert!(
        env["warning"]
            .as_str()
            .is_some_and(|w| w.contains("ERROR #5002")),
        "the warning must carry the reason, not just a category: {env}"
    );
    // The action itself succeeded — the item is in the live configuration.
    assert_eq!(env["success"], serde_json::json!(true));
}

#[test]
fn the_warning_names_the_consequence_and_the_remedy() {
    let w = class_not_saved_warning("ERROR #5002: class is deployed");
    assert!(w.contains("ERROR #5002: class is deployed"), "{w}");
    // The consequence: a recompile replays the stale XData.
    assert!(
        w.contains("revert"),
        "the caller cannot judge the risk without being told a recompile reverts this: {w}"
    );
    // The remedy, and the trap: the obvious next step is the wrong one.
    assert!(
        w.contains("iris_doc"),
        "it must name the step that would make the stale class authoritative: {w}"
    );
    assert!(
        w.contains("re-run"),
        "a refusal or warning names a way forward (#329): {w}"
    );
}

#[test]
fn the_warning_does_not_say_the_mutation_failed() {
    let w = class_not_saved_warning("ERROR #5002");
    let lower = w.to_lowercase();
    for wrong in [
        "was not applied",
        "not saved to the configuration",
        "retry the add",
    ] {
        assert!(
            !lower.contains(wrong),
            "the live change DID land; the warning must not read as a failed mutation ({wrong}): {w}"
        );
    }
    assert!(
        w.contains("live configuration was changed"),
        "it must state plainly that the live change landed: {w}"
    );
}
