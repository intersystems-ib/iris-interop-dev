//! #534, the remaining half: an `.INT` line is mapped back to the `.MAC` line it was generated
//! from, and a frame for a routine that does not exist says so in a sentence about ROUTINES.
//!
//! #564 landed the frame classification and the `LABEL+offset` arithmetic, but the tool still
//! never read anything: the caller had to fetch the listing and pass it as `int_source`, and a
//! frame naming a routine that does not exist came back as `INT_SOURCE_REQUIRED` — honest, but
//! never an answer about the routine.
//!
//! ## The correspondence, measured rather than assumed
//!
//! Two fixtures compiled on IRIS for Health 2026.1 (`IIDFrame`, `IIDMacro`), and the `.int` IRIS
//! generated read back through Atelier. Both are reproduced verbatim below.
//!
//! | fixture | .mac | .int | what went missing |
//! |---|---|---|---|
//! | no preprocessor content | 16 | 15 | the one blank line |
//! | two `#define`s + a blank | 14 | 11 | both directive lines, and the blank |
//!
//! Comment-only lines are carried VERBATIM. Macro INVOCATIONS expand in place and never change
//! the line count — ` set a=$$$Twice(7)` became ` set a=($select(7>7:7,1:7))` on one line.
//!
//! So the `.int` is the `.mac` with blank lines and `#`-directive lines removed, order preserved.
//! That is what makes the mapping countable without running the preprocessor, and it is why the
//! walk skips exactly those two kinds of line and nothing else.
//!
//! The ground truth every assertion below is checked against came from a REAL error, raised by
//! calling the fixture and reading `$ZERROR`: `<UNDEFINED>BOOM+2^IIDFrame *zzUndefinedVar`,
//! where `BOOM` is INT line 10, so `+2` is INT line 12 — which is MAC line 13.

use iris_agentic_dev_core::tools::int_frame::{map_int_line_to_mac, MacMapping};

/// `IIDFrame.mac` as written. 16 lines, one blank (line 8), two comment-only lines (2 and 4).
const FRAME_MAC: &str = "ROUTINE IIDFrame [Type=MAC]
 ; comment on MAC line 2
 quit
 ;
MAIN ; label MAIN is on MAC line 5
 new a
 set a=1

 set a=a+1
 quit a
BOOM ; label BOOM is on MAC line 11
 new b
 set b=zzUndefinedVar
 quit b
zINIT ; lowercase-z label must NOT be stripped for a routine
 quit \"zinit\"";

/// `IIDFrame.int` as IRIS generated it. 15 lines: the blank is gone, the comments are not.
const FRAME_INT: &str = "ROUTINE IIDFrame [Type=INT,Generated]
 ; comment on MAC line 2
 quit
 ;
MAIN ; label MAIN is on MAC line 5
 new a
 set a=1
 set a=a+1
 quit a
BOOM ; label BOOM is on MAC line 11
 new b
 set b=zzUndefinedVar
 quit b
zINIT ; lowercase-z label must NOT be stripped for a routine
 quit \"zinit\"";

/// `IIDMacro.mac`: two `#define` directives and a blank line.
const MACRO_MAC: &str = "ROUTINE IIDMacro [Type=MAC]
#define Twice(%x) ($$$MAX(%x,%x))
#define MAX(%a,%b) $select(%a>%b:%a,1:%b)
 ; a comment
MAIN ; label MAIN on MAC line 5
 new a
 set a=$$$Twice(7)

 set a=a+1
 quit a
BOOM ; label BOOM on MAC line 11
 set x=$$$MAX(1,2)
 set y=zzAlsoUndefined
 quit";

/// `IIDMacro.int` as IRIS generated it. 11 lines: both directives and the blank are gone, and
/// the two `$$$` invocations have expanded in place.
const MACRO_INT: &str = "ROUTINE IIDMacro [Type=INT,Generated]
 ; a comment
MAIN ; label MAIN on MAC line 5
 new a
 set a=($select(7>7:7,1:7))
 set a=a+1
 quit a
BOOM ; label BOOM on MAC line 11
 set x=$select(1>2:1,1:2)
 set y=zzAlsoUndefined
 quit";

/// `IIDInBlock.mac`: a `#define` and a `#;` INSIDE a label block, below the label and above the
/// target line. Added because a mutation that stopped dropping directive lines SURVIVED the
/// fixtures above — the walk anchors on the label, so a directive ABOVE the label can never
/// affect the result, and neither `IIDFrame` nor `IIDMacro` has one BELOW it. That is the only
/// position where the rule is load-bearing, and nothing was asserting it.
const INBLOCK_MAC: &str = "ROUTINE IIDInBlock [Type=MAC]
MAIN ; label MAIN on MAC line 2
 new a
#define Inner(%x) (%x+1)
 set a=$$$Inner(5)
#; a directive-style comment inside the block
 set b=zzBoomHere
 quit";

/// `IIDInBlock.int` as IRIS generated it: 8 MAC lines became 6, both in-block directives gone.
const INBLOCK_INT: &str = "ROUTINE IIDInBlock [Type=INT,Generated]
MAIN ; label MAIN on MAC line 2
 new a
 set a=(5+1)
 set b=zzBoomHere
 quit";

// ── the premise: these fixtures really are the shape the rule describes ──────────────────────

/// If this fails, every mapping assertion below is being checked against the wrong fixture.
#[test]
fn the_fixtures_have_the_line_counts_that_were_measured() {
    assert_eq!(FRAME_MAC.lines().count(), 16, "IIDFrame.mac");
    assert_eq!(FRAME_INT.lines().count(), 15, "IIDFrame.int");
    assert_eq!(MACRO_MAC.lines().count(), 14, "IIDMacro.mac");
    assert_eq!(MACRO_INT.lines().count(), 11, "IIDMacro.int");
    // Exactly the lines the rule says are dropped, and no others.
    let dropped_frame = FRAME_MAC
        .lines()
        .filter(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        .count();
    assert_eq!(dropped_frame, 1, "IIDFrame.mac drops only its blank line");
    let dropped_macro = MACRO_MAC
        .lines()
        .filter(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        .count();
    assert_eq!(
        dropped_macro, 3,
        "IIDMacro.mac drops two directives and a blank"
    );
    assert_eq!(
        FRAME_MAC.lines().count() - dropped_frame,
        FRAME_INT.lines().count()
    );
    assert_eq!(
        MACRO_MAC.lines().count() - dropped_macro,
        MACRO_INT.lines().count()
    );
    println!("fixture line counts: frame 16/15, macro 14/11");
}

// ── the mapping ──────────────────────────────────────────────────────────────────────────────

/// The real frame. `<UNDEFINED>BOOM+2^IIDFrame` resolves to INT 12, which is MAC 13.
#[test]
fn the_line_a_real_frame_named_maps_to_its_mac_line() {
    match map_int_line_to_mac(FRAME_MAC, FRAME_INT, "BOOM", 12) {
        MacMapping::Exact { line_number, text } => {
            assert_eq!(line_number, 13);
            assert_eq!(text, " set b=zzUndefinedVar");
        }
        other => panic!("the frame's own line mapped to {other:?}"),
    }
}

/// Crossing the dropped blank line is the whole arithmetic: MAIN is at the same number in both,
/// so a mapping that ignored the blank would be right here and wrong after it.
#[test]
fn a_line_above_the_blank_and_one_below_it_both_map() {
    assert_eq!(
        map_int_line_to_mac(FRAME_MAC, FRAME_INT, "MAIN", 7).line_number(),
        Some(7),
        "INT 7 is above the blank, so the numbers coincide"
    );
    assert_eq!(
        map_int_line_to_mac(FRAME_MAC, FRAME_INT, "MAIN", 8).line_number(),
        Some(9),
        "INT 8 is below the blank at MAC 8, so it is MAC 9 — this is the off-by-one the rule fixes"
    );
}

/// A lowercase-`z` label is a real routine label, and `z` must not be stripped from it.
#[test]
fn a_lowercase_z_label_anchors_the_walk() {
    assert_eq!(
        map_int_line_to_mac(FRAME_MAC, FRAME_INT, "zINIT", 15).line_number(),
        Some(16)
    );
}

/// Across two `#define`s and a blank: BOOM's block starts 3 lines earlier in the `.int`.
#[test]
fn a_routine_with_preprocessor_directives_still_maps_exactly() {
    match map_int_line_to_mac(MACRO_MAC, MACRO_INT, "BOOM", 10) {
        MacMapping::Exact { line_number, text } => {
            assert_eq!(line_number, 13);
            assert_eq!(text, " set y=zzAlsoUndefined");
        }
        other => panic!("expected an exact mapping across the directives, got {other:?}"),
    }
}

/// The macro-expanded line. The LINE is exact; only the text differs, and both are reported —
/// `mac_text` is what you edit, `int_text` is what ran.
#[test]
fn a_macro_expanded_line_is_the_right_line_with_both_texts() {
    match map_int_line_to_mac(MACRO_MAC, MACRO_INT, "MAIN", 5) {
        MacMapping::ExpandedOnThatLine {
            line_number,
            mac_text,
            int_text,
        } => {
            assert_eq!(line_number, 7);
            assert_eq!(mac_text, " set a=$$$Twice(7)");
            assert_eq!(int_text, " set a=($select(7>7:7,1:7))");
        }
        other => panic!("the expanded line must not be reported as {other:?}"),
    }
}

/// Offset 0 is the label line itself, in both documents.
#[test]
fn the_label_line_maps_to_the_label_line() {
    assert_eq!(
        map_int_line_to_mac(FRAME_MAC, FRAME_INT, "BOOM", 10).line_number(),
        Some(11)
    );
}

// ── the refusals: never a line salvaged from a partial walk ──────────────────────────────────

#[test]
fn a_label_absent_from_the_int_is_undetermined_not_a_guess() {
    match map_int_line_to_mac(FRAME_MAC, FRAME_INT, "NOSUCH", 5) {
        MacMapping::Undetermined(why) => assert!(why.contains("NOSUCH"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        map_int_line_to_mac(FRAME_MAC, FRAME_INT, "NOSUCH", 5).line_number(),
        None,
        "an undetermined mapping must offer no line number at all"
    );
}

/// Two texts from different generations of the routine. The walk runs off the end of the `.mac`
/// rather than returning whatever line it reached.
#[test]
fn a_mac_that_ends_mid_walk_is_undetermined() {
    let truncated = "ROUTINE IIDFrame [Type=MAC]\nBOOM ; label\n quit";
    match map_int_line_to_mac(truncated, FRAME_INT, "BOOM", 13) {
        MacMapping::Undetermined(why) => {
            assert!(why.contains("not the same generation"), "{why}")
        }
        other => panic!("a short .mac must not yield {other:?}"),
    }
}

#[test]
fn an_int_line_above_its_label_belongs_to_another_block() {
    match map_int_line_to_mac(FRAME_MAC, FRAME_INT, "BOOM", 3) {
        MacMapping::Undetermined(why) => assert!(why.contains("another label"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_int_line_outside_the_text_is_undetermined() {
    match map_int_line_to_mac(FRAME_MAC, FRAME_INT, "BOOM", 9999) {
        MacMapping::Undetermined(why) => assert!(why.contains("outside"), "{why}"),
        other => panic!("{other:?}"),
    }
    match map_int_line_to_mac(FRAME_MAC, FRAME_INT, "BOOM", 0) {
        MacMapping::Undetermined(_) => {}
        other => panic!("line 0 does not exist, got {other:?}"),
    }
}

/// A `.mac` that is not the same routine at all. The label is in the `.int` and not in the
/// `.mac`, and the reason must say THAT rather than blaming the label.
#[test]
fn a_mac_for_another_routine_is_undetermined_and_says_why() {
    let other = "ROUTINE Something [Type=MAC]\nOTHER ; nothing in common\n quit";
    match map_int_line_to_mac(other, FRAME_INT, "BOOM", 12) {
        MacMapping::Undetermined(why) => {
            assert!(why.contains("not the same routine"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

/// The position where dropping directive lines actually matters: BETWEEN the label and the
/// target. Measured on 2026.1 — an 8-line `.mac` with a `#define` and a `#;` inside the block
/// compiled to a 6-line `.int`, both directives gone.
///
/// Without the `#` rule the walk lands on MAC 5 instead of MAC 7, off by the two directives, so
/// this is the assertion that was missing.
#[test]
fn directives_inside_a_label_block_are_crossed_exactly() {
    assert_eq!(INBLOCK_MAC.lines().count(), 8);
    assert_eq!(INBLOCK_INT.lines().count(), 6);
    match map_int_line_to_mac(INBLOCK_MAC, INBLOCK_INT, "MAIN", 5) {
        MacMapping::Exact { line_number, text } => {
            assert_eq!(line_number, 7, "two in-block directives must be skipped");
            assert_eq!(text, " set b=zzBoomHere");
        }
        other => panic!("expected MAC 7 across two in-block directives, got {other:?}"),
    }
}

/// The same block, on the line where a macro expanded AND a directive was crossed above it.
#[test]
fn an_expansion_below_an_in_block_directive_still_names_its_mac_line() {
    match map_int_line_to_mac(INBLOCK_MAC, INBLOCK_INT, "MAIN", 4) {
        MacMapping::ExpandedOnThatLine {
            line_number,
            mac_text,
            int_text,
        } => {
            assert_eq!(line_number, 5, "one in-block #define must be skipped");
            assert_eq!(mac_text, " set a=$$$Inner(5)");
            assert_eq!(int_text, " set a=(5+1)");
        }
        other => panic!("{other:?}"),
    }
}
