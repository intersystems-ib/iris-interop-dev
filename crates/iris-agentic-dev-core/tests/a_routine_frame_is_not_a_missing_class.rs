//! #534: `iris_debug(action=map_int)` read every frame as a CLASS frame.
//!
//! `<UNDEFINED>LIST+2^OMORDL` is a routine frame. The handler looked `OMORDL` up in
//! `%Dictionary.CompiledClass`, found nothing — correctly, it is not a class — and answered
//! `ROUTINE_NOT_FOUND`: *"'OMORDL' is not a compiled class"*. That is a true sentence about a
//! question nobody asked, standing in for an answer about a routine that exists. It is the
//! repo's commonest bug shape: a failed lookup returned as a negative fact.
//!
//! Three defects live in `parse_int_frame`/`handle_iris_debug`, and each gets its own
//! assertions here because any one of them alone still breaks the answer:
//!
//! 1. the frame is never CLASSIFIED, so a routine is looked up as a class;
//! 2. `strip_prefix('z')` is applied to ROUTINE labels, where `zINIT` is a label the author
//!    wrote and `INIT` may not exist at all. For a class, `z` IS the compiler's generated
//!    method-label prefix, so the strip is right there and only there;
//! 3. the routine name is not terminated, so IRIS's trailing detail rides into the lookup.
//!    Measured on a live 2026.2 stack (issue thread): `<UNDEFINED>F2+1^OMCUSTL *X` was looked
//!    up as the class `OMCUSTL *X` — a name nothing can ever match.

use iris_agentic_dev_core::tools::info::parse_int_frame;

// ─── defect 2: the z-strip is a CLASS rule applied to every frame ────────────────────────────

#[test]
fn a_routine_label_keeps_its_z() {
    let f = parse_int_frame("<UNDEFINED>zINIT+2^OMORDL").expect("a routine frame must parse");
    assert_eq!(
        f.label, "zINIT",
        "`zINIT^OMORDL` is a label the routine's author wrote; stripping the z names a label \
         that need not exist, and then the label lookup fails for a reason the caller cannot see"
    );
}

/// The control for the assertion above: the strip must still happen where it is correct. Without
/// this, deleting the strip outright would satisfy the test above and look like a fix.
#[test]
fn a_class_label_still_loses_its_z() {
    let f = parse_int_frame("<ILLEGAL VALUE>zSlotKey+1^Radio.BO.RisWorklistOut.1")
        .expect("a class frame must parse");
    assert_eq!(
        f.label, "SlotKey",
        "`z` is the class compiler's generated method-label prefix, so a CLASS frame's label \
         is the method name without it"
    );
}

// ─── defect 3: IRIS's trailing detail is part of neither the routine nor the class ───────────

#[test]
fn the_offending_reference_is_not_part_of_the_routine_name() {
    let f = parse_int_frame("<UNDEFINED>F2+1^OMCUSTL *X").expect("the measured frame must parse");
    assert_eq!(
        f.routine, "OMCUSTL",
        "IRIS appends the offending reference to an <UNDEFINED> frame; the routine name ends \
         before it"
    );
    assert_eq!(
        f.class, "OMCUSTL",
        "and the name carried into any lookup must not contain a space either"
    );
    assert_eq!(f.label, "F2", "the label is unaffected by the trailing detail");
}

/// The same shape measured on a CLASS frame, from `unittest_result::extract_frame`'s own doc
/// comment: `<PROPERTY DOES NOT EXIST>TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1 *Username,EnsLib.SQL.OutboundAdapter`.
#[test]
fn a_class_frames_trailing_detail_is_dropped_too() {
    let f = parse_int_frame(
        "<PROPERTY DOES NOT EXIST>zTestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1 *Username,EnsLib.SQL.OutboundAdapter",
    )
    .expect("the measured frame must parse");
    assert_eq!(f.routine, "Ejercicio3.Tests.BO.WriteToCocina.1");
    assert_eq!(f.class, "Ejercicio3.Tests.BO.WriteToCocina");
}
