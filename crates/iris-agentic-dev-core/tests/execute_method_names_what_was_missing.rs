//! #396: `iris_execute_method` reported three conditions as `METHOD_THREW`, a code asserting the
//! method RAN and raised. For two of them the method never ran.
//!
//! Measured 2026-09-25, namespace USER, with a working call as the control:
//!
//! ```text
//! Zz.No.Such::Foo              -> METHOD_THREW
//!    'Zz.No.Such::Foo' raised: <CLASS DOES NOT EXIST> 150 RunUser+7^IrisDevTmp.Run4b7a…1 Zz.No.Such
//! %Library.String::ZzNope      -> METHOD_THREW
//!    '…::ZzNope' raised: <METHOD DOES NOT EXIST> 148 RunUser+7^IrisDevTmp.Runc9d1…1 ZzNope,%Library.String
//! %Library.String::IsValid("abc") -> success, value "1"                            <- the control
//! %Library.String::IsValid()      -> METHOD_THREW <FUNCTION> 6 RunUser+7^…         <- really threw
//! ```
//!
//! The fix reads IRIS's verdict rather than pre-checking the dictionary: per the `MethodMeta`
//! contract and #242, a class written AND compiled in one process still reads as absent from
//! `%Dictionary.*` until a later one, so a pre-call refusal would reject calls that work.

use iris_agentic_dev_core::tools::execute_method::{
    classify_threw, threw_message, threw_report, without_wrapper_frame, ThrewKind,
};

const CONNECTION_SRC: &str = include_str!("../src/iris/connection.rs");

/// The measured frames, verbatim.
const CLASS_GONE: &str =
    "<CLASS DOES NOT EXIST> 150 RunUser+7^IrisDevTmp.Run4b7a21befdcf.1 Zz.No.Such";
const METHOD_GONE: &str =
    "<METHOD DOES NOT EXIST> 148 RunUser+7^IrisDevTmp.Runc9d101083836.1 ZzNope,%Library.String";
const REALLY_THREW: &str = "<FUNCTION> 6 RunUser+7^IrisDevTmp.Run59c64ca777f0.1";

#[test]
fn an_absent_class_is_not_a_method_that_threw() {
    assert_eq!(
        classify_threw("Zz.No.Such", "Foo", CLASS_GONE),
        ThrewKind::ClassMissing
    );
    let (code, msg) = threw_report("Zz.No.Such", "Foo", "USER", ThrewKind::ClassMissing);
    assert_eq!(code, "CLASS_NOT_FOUND");
    assert!(msg.contains("never ran"), "{msg}");
    assert!(msg.contains("not a method failure"), "{msg}");
    assert!(msg.contains("'USER'"), "the namespace must be named: {msg}");
    assert!(msg.contains("iris_symbols"), "remedy one: {msg}");
    assert!(
        msg.contains("iris_doc(mode=put, compile=true)"),
        "remedy two: {msg}"
    );
}

#[test]
fn an_absent_method_is_not_a_method_that_threw() {
    assert_eq!(
        classify_threw("%Library.String", "ZzNope", METHOD_GONE),
        ThrewKind::MethodMissing
    );
    let (code, msg) = threw_report(
        "%Library.String",
        "ZzNope",
        "USER",
        ThrewKind::MethodMissing,
    );
    assert_eq!(code, "METHOD_NOT_FOUND");
    assert!(msg.contains("has no method 'ZzNope'"), "{msg}");
    assert!(msg.contains("nothing ran"), "{msg}");
    assert!(
        msg.contains("docs_introspect(class_name=%Library.String)"),
        "the remedy must name the class to introspect: {msg}"
    );
}

#[test]
fn a_method_that_really_threw_keeps_method_threw() {
    assert_eq!(
        classify_threw("%Library.String", "IsValid", REALLY_THREW),
        ThrewKind::Raised
    );
    let (code, msg) = threw_report("%Library.String", "IsValid", "USER", ThrewKind::Raised);
    assert_eq!(code, "METHOD_THREW");
    assert!(
        msg.is_empty(),
        "Raised keeps the original wording, built by the caller: {msg:?}"
    );
}

#[test]
fn a_missing_class_hit_inside_the_method_is_still_the_method_throwing() {
    // The ambiguity a bare `contains` would get wrong: the target ran and reached a DIFFERENT
    // absent class. Misattributing this would tell the caller their target is absent when it is
    // present and failing. The frame is inside the target method, not the wrapper.
    let inner = "<CLASS DOES NOT EXIST> 150 zDoWork+3^My.Pkg.Worker.1 Some.Other.Class";
    assert_eq!(
        classify_threw("My.Pkg.Worker", "DoWork", inner),
        ThrewKind::Raised,
        "no wrapper frame, so the method ran"
    );
}

#[test]
fn the_offending_name_must_match_what_was_asked_for() {
    // A wrapper-frame signal naming something OTHER than what we asked for is not evidence about
    // our target. Both branches are asserted: checking only the class branch let a mutant that
    // dropped the METHOD name check survive, which is the same one-branch-not-both gap this suite
    // exists to catch in the product code.
    let other_class = "<CLASS DOES NOT EXIST> 150 RunUser+7^IrisDevTmp.Run1.1 Completely.Other";
    assert_eq!(
        classify_threw("My.Pkg.Worker", "DoWork", other_class),
        ThrewKind::Raised,
        "a different class being absent says nothing about My.Pkg.Worker"
    );
    let other_method =
        "<METHOD DOES NOT EXIST> 148 RunUser+7^IrisDevTmp.Run1.1 SomethingElse,My.Pkg.Worker";
    assert_eq!(
        classify_threw("My.Pkg.Worker", "DoWork", other_method),
        ThrewKind::Raised,
        "a different method being absent says nothing about DoWork"
    );
}

#[test]
fn an_empty_or_unrecognised_error_stays_method_threw() {
    for e in ["", "<STORE>", "something IRIS has never said"] {
        assert_eq!(
            classify_threw("A.B", "C", e),
            ThrewKind::Raised,
            "{e:?} names no condition, so the honest label is the generic one"
        );
    }
}

#[test]
fn the_wrapper_frame_anchor_still_matches_the_generator() {
    // classify_threw depends on the generated wrapper being called RunUser. If the generator is
    // ever renamed, this fails loudly instead of silently reclassifying every absent target as
    // a method that threw.
    assert!(
        CONNECTION_SRC.contains("RunUser()"),
        "the generated wrapper method is no longer named RunUser — classify_threw's frame anchor \
         must be updated with it"
    );
}

#[test]
fn the_two_never_ran_messages_drop_the_scratch_frame() {
    // RunUser+N^IrisDevTmp.Run<uuid>.1 is this server's own class. It is not actionable, and the
    // uuid differs per call, so two identical requests would otherwise produce two messages.
    for (kind, class, method, err) in [
        (ThrewKind::ClassMissing, "Zz.No.Such", "Foo", CLASS_GONE),
        (
            ThrewKind::MethodMissing,
            "%Library.String",
            "ZzNope",
            METHOD_GONE,
        ),
    ] {
        let (_, named) = threw_report(class, method, "USER", kind);
        let msg = threw_message(class, method, kind, &named, err, "");
        assert!(!msg.contains("IrisDevTmp"), "scratch class leaked: {msg}");
        assert!(!msg.contains("RunUser+"), "wrapper frame leaked: {msg}");
        // The signal itself is still quoted — it is IRIS's verdict and worth showing.
        assert!(
            msg.contains("DOES NOT EXIST"),
            "the signal must survive: {msg}"
        );
    }
}

#[test]
fn a_raised_message_loses_only_the_wrapper_frame() {
    // A genuine throw AT the call site carries the wrapper frame (measured: a wrong-arity call
    // returns `<FUNCTION> 6 RunUser+7^IrisDevTmp.Run…`). That is noise like any other wrapper
    // frame, so it goes — while the signal stays.
    let (_, named) = threw_report("%Library.String", "IsValid", "USER", ThrewKind::Raised);
    let msg = threw_message(
        "%Library.String",
        "IsValid",
        ThrewKind::Raised,
        &named,
        REALLY_THREW,
        "",
    );
    assert!(!msg.contains("IrisDevTmp"), "wrapper frame kept: {msg}");
    assert!(msg.contains("<FUNCTION>"), "the signal must survive: {msg}");
}

#[test]
fn the_raised_message_keeps_a_frame_inside_the_callers_method() {
    // There the location can be inside the caller's OWN method, which is the diagnosis.
    let inner = "<DIVIDE> 8 zDoWork+3^My.Pkg.Worker.1";
    let (_, named) = threw_report("My.Pkg.Worker", "DoWork", "USER", ThrewKind::Raised);
    let msg = threw_message(
        "My.Pkg.Worker",
        "DoWork",
        ThrewKind::Raised,
        &named,
        inner,
        "",
    );
    assert!(
        msg.contains("zDoWork+3^My.Pkg.Worker.1"),
        "a frame inside the caller's method is the answer, not noise: {msg}"
    );
}

#[test]
fn stripping_the_frame_removes_only_the_frame() {
    assert_eq!(
        without_wrapper_frame("<CLASS DOES NOT EXIST> 150 RunUser+7^IrisDevTmp.Run1.1 Zz.No.Such"),
        "<CLASS DOES NOT EXIST> 150 Zz.No.Such",
        "the signal, its number and the offending name all survive"
    );
    assert_eq!(
        without_wrapper_frame("<DIVIDE> 8 zDoWork+3^My.Pkg.Worker.1"),
        "<DIVIDE> 8 zDoWork+3^My.Pkg.Worker.1",
        "a non-wrapper frame is left alone"
    );
    assert_eq!(without_wrapper_frame(""), "");
}

#[test]
fn no_message_ends_in_whitespace() {
    // IRIS's own error text ends with a trailing space (measured on the wrong-arity call:
    // `<FUNCTION> 6 RunUser+7^IrisDevTmp.Run0b45682d73e6.1 `). Before the frame was stripped that
    // space sat mid-string; afterwards it would be the last character a caller sees.
    for (kind, class, method, err) in [
        (
            ThrewKind::Raised,
            "%Library.String",
            "IsValid",
            "<FUNCTION> 6 RunUser+7^IrisDevTmp.Run1.1 ",
        ),
        (
            ThrewKind::ClassMissing,
            "Zz.No.Such",
            "Foo",
            "<CLASS DOES NOT EXIST> 150 RunUser+7^IrisDevTmp.Run1.1 Zz.No.Such ",
        ),
        (
            ThrewKind::MethodMissing,
            "%Library.String",
            "ZzNope",
            "<METHOD DOES NOT EXIST> 148 RunUser+7^IrisDevTmp.Run1.1 ZzNope,%Library.String ",
        ),
    ] {
        let (_, named) = threw_report(class, method, "USER", kind);
        let msg = threw_message(class, method, kind, &named, err, "");
        assert_eq!(
            msg,
            msg.trim_end(),
            "a caller-facing message must not end in whitespace: {msg:?}"
        );
        assert!(!msg.is_empty(), "and it must still say something");
    }
}
