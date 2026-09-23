//! #327 item 2 and item 4: `iris_doc(put)` can run the test, and every write says whether it
//! compiled.
//!
//! Measured over 31,009 parsed `tool_use` blocks from 364 transcripts: `iris_doc(put)` is
//! immediately followed by `iris_test` **579 times** — 346 where the class written is the class
//! tested, 233 where the test lives beside it. And 89 `put` → `iris_compile` pairs are a put that
//! did not pass `compile` followed by a compile of the same class.
//!
//! What is covered here is the DECISION and the payload contract, both pure. Actually running a
//! `%UnitTest` suite needs an instance, so the e2e suite owns that half; the gate below is what
//! decides whether it is reached at all.

use iris_agentic_dev_core::tools::doc::{attach_test_result, test_gate, TestGate};
use serde_json::json;

/// The payload a put reports after a clean compile — the shape `handle_put` builds.
fn compiled() -> serde_json::Value {
    json!({"success": true, "name": "Hospital.BO.PatientDb.cls", "compiled": true,
           "compile_requested": true, "compile_errors": [], "compile_console": []})
}

#[test]
fn a_test_named_on_a_compiled_write_runs() {
    assert_eq!(
        test_gate(Some("Hospital.Tests.BO.PatientDbTest"), Some(&compiled())),
        TestGate::Run("Hospital.Tests.BO.PatientDbTest".into())
    );
}

/// The 233 of 579 a boolean could not have covered: the suite is not named after the class written.
#[test]
fn the_pattern_is_whatever_the_caller_named_not_the_class_written() {
    let TestGate::Run(p) = test_gate(Some("Some.Other.SuiteTest"), Some(&compiled())) else {
        panic!("expected Run");
    };
    assert_eq!(p, "Some.Other.SuiteTest");
}

/// Whitespace and empty string are not a request. Without this, `test: ""` would run `iris_test`
/// with an empty pattern, whose own failure has nothing to do with what the caller did.
#[test]
fn a_blank_test_is_not_a_request() {
    for t in [None, Some(""), Some("   ")] {
        assert_eq!(
            test_gate(t, Some(&compiled())),
            TestGate::NotRequested,
            "{t:?}"
        );
    }
    // CONTROL: the same payload DOES gate a real pattern through, so these four are not passing
    // because nothing ever runs.
    assert!(matches!(
        test_gate(Some("X.Test"), Some(&compiled())),
        TestGate::Run(_)
    ));
}

/// Nothing is added for a caller who never asked — a `test_skipped` on every ordinary put would be
/// noise on the 2,979 puts that want none of this.
#[test]
fn a_caller_who_did_not_ask_is_told_nothing() {
    assert_eq!(TestGate::NotRequested.skipped_reason(), None);
    assert_eq!(TestGate::Run("X".into()).skipped_reason(), None);
}

/// A write that did not succeed must not run the test: the red would be about a class that is not
/// there, and #310 is the standing rule against answering one failure with another's shape.
#[test]
fn a_failed_write_does_not_run_the_test() {
    let failed = json!({"name": "X.cls", "compiled": false, "compile_requested": true,
                        "error_code": "COMPILE_ERROR", "compile_errors": ["ERROR #1026: Invalid command"]});
    assert_eq!(
        test_gate(Some("X.Test"), Some(&failed)),
        TestGate::SkippedCallFailed
    );
    let reason = TestGate::SkippedCallFailed
        .skipped_reason()
        .expect("a reason");
    assert!(
        reason.contains("error_code"),
        "must point at the real answer: {reason}"
    );
}

/// A result carrying nothing parseable is treated as a failed call, never as a class that compiled.
/// Guessing the other way would run a suite against a state this code cannot see.
#[test]
fn an_unreadable_result_is_not_treated_as_a_compiled_class() {
    assert_eq!(test_gate(Some("X.Test"), None), TestGate::SkippedCallFailed);
}

/// The two ways a successful write produces no compiled class need DIFFERENT fixes from the
/// caller, so they are different gate states and different sentences.
#[test]
fn not_asking_for_a_compile_and_a_compile_that_produced_nothing_are_different_answers() {
    let never =
        json!({"success": true, "name": "X.cls", "compiled": false, "compile_requested": false});
    assert_eq!(
        test_gate(Some("X.Test"), Some(&never)),
        TestGate::SkippedNotCompiled {
            compile_requested: false
        }
    );
    let asked =
        json!({"success": true, "name": "X.cls", "compiled": false, "compile_requested": true});
    assert_eq!(
        test_gate(Some("X.Test"), Some(&asked)),
        TestGate::SkippedNotCompiled {
            compile_requested: true
        }
    );

    let a = TestGate::SkippedNotCompiled {
        compile_requested: false,
    }
    .skipped_reason()
    .unwrap();
    let b = TestGate::SkippedNotCompiled {
        compile_requested: true,
    }
    .skipped_reason()
    .unwrap();
    assert_ne!(
        a, b,
        "one sentence for two states tells half the callers the wrong fix"
    );
    assert!(
        a.contains("compile=true"),
        "must name the missing flag: {a}"
    );
    assert!(
        b.contains("compile_errors") || b.contains("compile_console"),
        "must send the caller to the compiler output: {b}"
    );
}

/// A mode that never compiles (get, head, delete) carries no `compiled` key at all, and an absent
/// key must not read as a compiled class.
#[test]
fn a_mode_that_never_compiles_does_not_run_the_test() {
    let got = json!({"success": true, "name": "X.cls", "content": "Class X"});
    assert_eq!(
        test_gate(Some("X.Test"), Some(&got)),
        TestGate::SkippedNotCompiled {
            compile_requested: false
        }
    );
}

/// The write's verdict and the test's verdict stay separate. A red test must not read as a failed
/// write — the document DID land — and a failed write must not read as a red test.
#[test]
fn a_red_test_does_not_rewrite_the_writes_own_verdict() {
    let mut payload = compiled();
    attach_test_result(
        &mut payload,
        "Hospital.Tests.BO.PatientDbTest",
        json!({"success": false, "error_code": "TEST_FAILED", "failed": 1, "passed": 3}),
    );
    assert_eq!(
        payload["success"], true,
        "the write landed and still says so"
    );
    assert_eq!(payload["compiled"], true);
    assert_eq!(payload["test_ok"], false);
    assert_eq!(payload["test"]["error_code"], "TEST_FAILED");
    assert_eq!(payload["test"]["failed"], 1);
    assert_eq!(payload["test_pattern"], "Hospital.Tests.BO.PatientDbTest");
}

/// And a green one is reported as green — without this, "always false" would satisfy the test above.
#[test]
fn a_green_test_is_reported_as_green() {
    let mut payload = compiled();
    attach_test_result(
        &mut payload,
        "X.Test",
        json!({"success": true, "passed": 4, "failed": 0}),
    );
    assert_eq!(payload["test_ok"], true);
    assert_eq!(payload["test"]["passed"], 4);
}

// ── item 4: every write says whether it compiled ──────────────────────────────────────────

/// The three write outcomes must be distinguishable from `compiled` + `compile_requested` alone.
/// The source is read because reaching all three needs a live instance; the assertion is that each
/// branch SETS both keys, which is what makes the pair readable without a fourth field.
#[test]
fn all_three_write_outcomes_report_both_compile_keys() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/doc.rs"),
    )
    .expect("doc.rs");
    // `compiled` appeared on two of the three branches before #327; the third had no key at all.
    let compiled_sites = src.matches("\"compiled\":").count();
    let requested_sites = src.matches("\"compile_requested\":").count();
    assert!(
        compiled_sites >= 3,
        "expected all three write outcomes to set `compiled`, found {compiled_sites}"
    );
    assert_eq!(
        requested_sites, compiled_sites,
        "`compile_requested` is set on {requested_sites} payloads and `compiled` on \
         {compiled_sites} — a branch reporting one without the other leaves the caller unable to \
         tell a compile nobody asked for from a compile that produced nothing"
    );
}

/// CONTROL for the counter above: a key that does not exist must count 0, or the two counts could
/// be agreeing on nothing.
#[test]
fn the_payload_key_counter_finds_nothing_for_a_key_that_does_not_exist() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/doc.rs"),
    )
    .expect("doc.rs");
    assert_eq!(src.matches("\"compile_teleported\":").count(), 0);
    assert!(src.matches("\"compiled\":").count() > 0);
}
