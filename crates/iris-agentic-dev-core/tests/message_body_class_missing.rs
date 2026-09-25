//! #392: a message whose body class is not compiled in the namespace answered with a raw
//! `<CLASS DOES NOT EXIST>` under the generic `IRIS_EXECUTE_ERROR`.
//!
//! Measured 2026-09-25, namespace USER, `dataPolicy=redact`:
//!
//! ```text
//! message_id=19  ->  success true, body "{}\n"          (Ens.Response IS compiled here)
//! message_id=18  ->  IRIS_EXECUTE_ERROR
//!                    "ERROR: <CLASS DOES NOT EXIST> 150 RunUser+6^IrisDevTmp.Runeaeb54569992.1
//!                     IOP.MSG.Req"
//! ```
//!
//! 19 is the control. The header for 18 exists and names `IOP.MSG.Req`; that class is absent from
//! `%Dictionary.CompiledClass`, while `Ens.Response` is present. **9 of the 19 messages** on that
//! instance named an absent body class, which is the normal end state once a production's classes
//! are removed while its `Ens` data persists.
//!
//! The existence test was measured before being relied on:
//! `%Dictionary.CompiledClass.%ExistsId` = 1 for `Ens.Response`, 0 for `IOP.MSG.Req`, 0 for a
//! nonsense name.

use iris_agentic_dev_core::tools::interop::{
    build_message_body_code, message_body_class_missing_message,
};

const INTEROP_SRC: &str = include_str!("../src/tools/interop.rs");

fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── the generated program ───────────────────────────────────────────────────────────────────

#[test]
fn the_body_class_is_checked_before_the_body_is_opened() {
    let code = build_message_body_code(18, 1024);
    let check = code
        .find("%ExistsId(bodyClass)")
        .expect("the body class must be proved present before it is used");
    let open = code
        .find("$ClassMethod(bodyClass,\"%OpenId\",bodyId)")
        .expect("the body is still opened through $ClassMethod");
    // Position, not mere presence: a check AFTER the open cannot prevent <CLASS DOES NOT EXIST>,
    // and `contains` alone would pass for a relocated check.
    assert!(
        check < open,
        "checking after opening cannot stop the signal that is being avoided:\n{code}"
    );
}

#[test]
fn the_check_returns_instead_of_falling_through() {
    let code = build_message_body_code(18, 1024);
    let line = code
        .lines()
        .find(|l| l.contains("MESSAGE_BODY_CLASS_MISSING"))
        .expect("the program must carry the marker");
    assert!(
        line.contains("Quit"),
        "without Quit the program falls through to $ClassMethod anyway: {line}"
    );
    assert!(
        line.contains("%ExistsId(bodyClass)"),
        "the marker must be guarded by the existence test: {line}"
    );
}

#[test]
fn the_marker_carries_the_class_name_not_a_literal() {
    let code = build_message_body_code(18, 1024);
    let line = code
        .lines()
        .find(|l| l.contains("MESSAGE_BODY_CLASS_MISSING"))
        .expect("marker line");
    assert!(
        line.contains("\"ERROR:MESSAGE_BODY_CLASS_MISSING:\"_bodyClass"),
        "the caller needs the class that is missing, read from the header: {line}"
    );
}

// ─── the refusal text ────────────────────────────────────────────────────────────────────────

#[test]
fn a_missing_body_class_is_not_reported_as_a_missing_message() {
    // The distinction that matters: the header IS there. A caller told "no such message" would
    // go looking for a wrong id.
    let msg = message_body_class_missing_message(18, "IOP.MSG.Req", "USER");
    assert!(
        msg.contains("NOT a missing message"),
        "the reply must say the header exists: {msg}"
    );
    assert!(
        msg.contains("message 18 exists"),
        "the reply must name the message it is about: {msg}"
    );
}

#[test]
fn the_refusal_names_the_class_the_namespace_and_both_remedies() {
    let msg = message_body_class_missing_message(18, "IOP.MSG.Req", "APP");
    assert!(
        msg.contains("'IOP.MSG.Req'"),
        "the missing class must be named: {msg}"
    );
    assert!(
        msg.contains("'APP'"),
        "the namespace it is missing FROM must be named: {msg}"
    );
    assert!(
        msg.contains("compile 'IOP.MSG.Req' into this namespace"),
        "remedy one: compile the class here: {msg}"
    );
    assert!(
        msg.contains("iris_interop_query(what=messages)"),
        "remedy two: read the header without the body: {msg}"
    );
}

#[test]
fn the_refusal_leaks_no_scratch_class_and_no_objectscript_frame() {
    // What the raw error exposed: `RunUser+6^IrisDevTmp.Runeaeb54569992.1`.
    let msg = message_body_class_missing_message(18, "IOP.MSG.Req", "USER");
    for leak in ["IrisDevTmp", "RunUser", "^", "<CLASS DOES NOT EXIST>"] {
        assert!(
            !msg.contains(leak),
            "{leak:?} is an internal detail the caller cannot act on: {msg}"
        );
    }
}

#[test]
fn the_refusal_does_not_leak_its_source_indentation() {
    // #378: a `\`-continued literal collapsed onto one source line keeps the indentation.
    let msg = message_body_class_missing_message(18, "IOP.MSG.Req", "USER");
    assert!(
        !msg.contains("  "),
        "a run of spaces reached the caller: {msg:?}"
    );
}

// ─── the wiring ──────────────────────────────────────────────────────────────────────────────

#[test]
fn the_new_marker_is_read_before_the_generic_error_path() {
    // A marker the handler never strips would fall through to the raw-execute-error arm, which is
    // exactly the behaviour being fixed.
    let src = without_comments(INTEROP_SRC);
    let strip = src
        .find("out.strip_prefix(\"ERROR:MESSAGE_BODY_CLASS_MISSING:\")")
        .expect("the handler must read the new marker");
    let emit = src
        .find("err_json(\"MESSAGE_BODY_CLASS_MISSING\"")
        .expect("the handler must emit the typed code");
    assert!(
        strip < emit,
        "the marker is stripped and then reported, in that order"
    );
}
