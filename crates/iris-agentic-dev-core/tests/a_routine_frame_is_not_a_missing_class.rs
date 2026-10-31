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

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::info::{handle_iris_debug, parse_int_frame, DebugParams};
use iris_agentic_dev_core::tools::int_frame::{
    class_of_int_routine, classify_routine, int_labels, map_int_answer, map_label_offset,
    FrameKind, IntLine, IntLineOutcome,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    assert_eq!(
        f.label, "F2",
        "the label is unaffected by the trailing detail"
    );
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

// ─── defect 1: the frame is CLASSIFIED, and the classification is its own function ───────────
//
// Independent of any lookup, so what it decides can be read off the frame string. The handler
// then asks IRIS about a class only when the frame names one.

#[test]
fn a_dotted_name_ending_in_a_number_is_a_class_frame() {
    for routine in [
        "Radio.BO.RisWorklistOut.1",
        "Ejercicio3.Tests.BO.WriteToCocina.1",
        "My.Pkg.Helper.7",
        // No package: a class called `Foo` compiles to the INT routine `Foo.1`.
        "Foo.1",
    ] {
        assert_eq!(
            classify_routine(routine),
            FrameKind::Class,
            "{routine} is the INT routine a class compiles to"
        );
    }
}

#[test]
fn a_name_with_no_int_suffix_is_a_routine_frame() {
    for routine in [
        // The frame from the issue, and the commonest one in routine-based code.
        "OMORDL",
        "OMCUSTL",
        "%ZSTART",
        // A class's INT routine ALWAYS carries the `.N` suffix, so a dotted name without one
        // is a routine whose name happens to contain dots — not a class.
        "My.Pkg.Helper",
        "Rtn.v2",
    ] {
        assert_eq!(
            classify_routine(routine),
            FrameKind::Routine,
            "{routine} is not the INT routine of any class"
        );
    }
}

/// The class NAME and the class/routine DECISION come from one function, so they cannot
/// disagree: `class_of_int_routine` is `Some` exactly when the frame is a class frame.
#[test]
fn the_class_name_and_the_kind_are_decided_together() {
    for routine in [
        "Radio.BO.RisWorklistOut.1",
        "Foo.1",
        "OMORDL",
        "My.Pkg.Helper",
        "",
    ] {
        assert_eq!(
            class_of_int_routine(routine).is_some(),
            classify_routine(routine) == FrameKind::Class,
            "{routine:?}: one site says class-shaped and the other does not"
        );
    }
}

#[test]
fn the_parsed_frame_carries_its_kind() {
    assert_eq!(
        parse_int_frame("<UNDEFINED>LIST+2^OMORDL").unwrap().kind,
        FrameKind::Routine
    );
    assert_eq!(
        parse_int_frame("<ILLEGAL VALUE>zSlotKey+1^Radio.BO.RisWorklistOut.1")
            .unwrap()
            .kind,
        FrameKind::Class
    );
}

// ─── LABEL+offset against .INT TEXT, as arithmetic over text passed in ──────────────────────
//
// The fixture is a routine .int listing. Blank line 5 is deliberate: the `.int` IRIS stores
// drops the blank lines of its `.mac` (#534's thread measured a 28-line .mac against a 26-line
// .int), so an implementation that "helpfully" skipped blanks would answer line 7 for LIST+3
// and nothing about that answer would look wrong. A fixture with no blank line above the
// target passes against both the right implementation and that one.
//
// 1  OMORDL ; order list
// 2   ;
// 3  LIST(pat) ; list one patient's orders
// 4   new i
// 5
// 6   set i=""
// 7   for  set i=$order(^OMORD(pat,i)) quit:i=""  do write i,!
// 8   quit
// 9  zINIT ; a hand-written label that begins with z
// 10  set ^OMORD("init")=1
// 11  quit
const INT: &str = concat!(
    "OMORDL ; order list\n",
    " ;\n",
    "LIST(pat) ; list one patient's orders\n",
    " new i\n",
    "\n",
    " set i=\"\"\n",
    " for  set i=$order(^OMORD(pat,i)) quit:i=\"\"  do write i,!\n",
    " quit\n",
    "zINIT ; a hand-written label that begins with z\n",
    " set ^OMORD(\"init\")=1\n",
    " quit\n",
);

#[test]
fn the_fixture_is_the_listing_the_tests_below_assume() {
    // A control on the fixture itself: every assertion below is a line number into it, so a
    // typo in the string would move them all and still look like a measurement.
    assert_eq!(INT.lines().count(), 11, "fixture line count");
    assert_eq!(
        int_labels(INT),
        vec![
            (1, "OMORDL".to_string()),
            (3, "LIST".to_string()),
            (9, "zINIT".to_string()),
        ],
        "labels and their 1-based lines"
    );
    assert_eq!(INT.lines().nth(4), Some(""), "line 5 is blank");
}

#[test]
fn offset_zero_is_the_label_line_itself() {
    assert_eq!(
        map_label_offset(INT, "LIST", 0),
        IntLineOutcome::Resolved {
            label_line: 3,
            block_lines: 6,
            line: IntLine {
                line_number: 3,
                text: "LIST(pat) ; list one patient's orders".to_string(),
            },
        },
        "LABEL^RTN with no +n trapped on the label line"
    );
}

#[test]
fn an_offset_counts_the_lines_of_the_listing_literally() {
    // +2 lands ON the blank line, and +3 therefore lands on the line after it. An
    // implementation that skipped blanks would answer line 7 here.
    let two = map_label_offset(INT, "LIST", 2);
    let three = map_label_offset(INT, "LIST", 3);
    assert!(
        matches!(&two, IntLineOutcome::Resolved { line, .. } if line.line_number == 5 && line.text.is_empty()),
        "LIST+2 is the blank line 5, not the next non-blank one: {two:?}"
    );
    assert!(
        matches!(&three, IntLineOutcome::Resolved { line, .. } if line.line_number == 6 && line.text == " set i=\"\""),
        "LIST+3 is line 6, which is only true if the blank line was counted: {three:?}"
    );
}

#[test]
fn a_label_block_ends_at_the_next_label() {
    // OMORDL owns lines 1-2 and stops because LIST begins at 3.
    assert!(
        matches!(
            map_label_offset(INT, "OMORDL", 1),
            IntLineOutcome::Resolved { block_lines: 2, .. }
        ),
        "OMORDL's block is the two lines before LIST"
    );
    assert!(
        matches!(
            map_label_offset(INT, "OMORDL", 2),
            IntLineOutcome::OffsetPastLabelBlock { block_lines: 2, .. }
        ),
        "OMORDL+2 is LIST's line, and belongs to LIST"
    );
}

#[test]
fn the_last_label_block_runs_to_the_end_of_the_routine() {
    assert!(
        matches!(
            map_label_offset(INT, "zINIT", 2),
            IntLineOutcome::Resolved {
                block_lines: 3,
                line: IntLine {
                    line_number: 11,
                    ..
                },
                ..
            }
        ),
        "zINIT has no following label, so its block ends with the routine"
    );
}

// ─── the refusals the issue demands: never invent a line ─────────────────────────────────────

#[test]
fn an_offset_past_the_end_of_the_block_is_refused() {
    // LIST owns lines 3-8, so +5 is its last line and +6 is somebody else's.
    assert!(
        matches!(
            map_label_offset(INT, "LIST", 5),
            IntLineOutcome::Resolved {
                line: IntLine { line_number: 8, .. },
                ..
            }
        ),
        "the control: +5 is still inside the block"
    );
    assert_eq!(
        map_label_offset(INT, "LIST", 6),
        IntLineOutcome::OffsetPastLabelBlock {
            label: "LIST".to_string(),
            offset: 6,
            label_line: 3,
            block_lines: 6,
        },
        "+6 runs past the block and no neighbouring line is offered instead"
    );
}

#[test]
fn a_label_that_is_not_in_the_listing_is_refused_with_the_labels_that_are() {
    assert_eq!(
        map_label_offset(INT, "LIST2", 1),
        IntLineOutcome::LabelNotFound {
            label: "LIST2".to_string(),
            labels_present: vec![
                "OMORDL".to_string(),
                "LIST".to_string(),
                "zINIT".to_string()
            ],
        },
        "the refusal carries what the listing DOES contain, so the caller can see it fetched \
         the wrong document"
    );
}

/// The house rule, at the layer it bites: text that could not be read is not a listing with no
/// labels, and a listing with no labels is not a missing label.
#[test]
fn text_that_is_not_a_listing_is_its_own_refusal() {
    for (name, text) in [
        ("empty", ""),
        ("whitespace", "   \n\n  \n"),
        // A .cls body, or a .mac whose every line is indented: no label starts a line.
        ("all indented", " set x=1\n quit\n"),
        // The closing brace of a generated method body sits at column 0 and is not a label.
        ("braces only", "}\n}\n"),
    ] {
        let out = map_label_offset(text, "LIST", 1);
        assert!(
            matches!(out, IntLineOutcome::SourceUnusable { .. }),
            "{name}: nothing was searched, so the answer must not be LabelNotFound: {out:?}"
        );
    }
    // The control: the same label against the real listing is found, so the sweep above is
    // about the text and not about a function that refuses everything.
    assert!(matches!(
        map_label_offset(INT, "LIST", 1),
        IntLineOutcome::Resolved { .. }
    ));
}

#[test]
fn a_negative_offset_is_refused_before_anything_is_searched() {
    assert_eq!(
        map_label_offset(INT, "LIST", -1),
        IntLineOutcome::OffsetInvalid { offset: -1 }
    );
    // And it is refused for text that is also unusable, which pins the precedence the doc
    // comment states rather than leaving it to whichever check happens to run first.
    assert_eq!(
        map_label_offset("", "LIST", -1),
        IntLineOutcome::OffsetInvalid { offset: -1 }
    );
}

#[test]
fn every_refusal_carries_a_code_and_resolved_carries_none() {
    assert_eq!(map_label_offset(INT, "LIST", 0).code(), None);
    let codes: Vec<&str> = [
        map_label_offset("", "LIST", 0),
        map_label_offset(INT, "NOPE", 0),
        map_label_offset(INT, "LIST", 99),
        map_label_offset(INT, "LIST", -1),
    ]
    .iter()
    .map(|o| o.code().expect("a refusal carries a code"))
    .collect();
    assert_eq!(
        codes,
        vec![
            "INT_SOURCE_UNUSABLE",
            "INT_LABEL_NOT_FOUND",
            "INT_OFFSET_PAST_LABEL_BLOCK",
            "INT_OFFSET_INVALID",
        ],
        "4 refusals requested / 4 distinct codes — one code for two causes would put the \
         caller back to parsing prose"
    );
}

// ─── the answer the tool returns ─────────────────────────────────────────────────────────────

fn payload(r: Result<rmcp::model::CallToolResult, rmcp::ErrorData>) -> (bool, serde_json::Value) {
    let r = r.expect("map_int_answer never returns a protocol error");
    let text = r
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("one text block")
        .text
        .clone();
    (
        r.is_error.unwrap_or(false),
        serde_json::from_str(&text).expect("the payload is JSON"),
    )
}

/// The defect, at the layer the caller sees it.
#[test]
fn a_routine_frame_is_never_answered_as_a_missing_class() {
    let frame = parse_int_frame("<UNDEFINED>LIST+2^OMORDL").unwrap();
    let (is_error, v) = payload(map_int_answer("<UNDEFINED>LIST+2^OMORDL", &frame, None));
    let whole = v.to_string();
    assert!(
        !whole.contains("ROUTINE_NOT_FOUND"),
        "a routine frame must not be answered with the class refusal: {whole}"
    );
    assert!(
        !whole.contains("is not a compiled class"),
        "nor with its wording: {whole}"
    );
    assert!(
        !is_error,
        "the frame WAS decomposed, so this is not a failure"
    );
    assert_eq!(v["frame_kind"], "routine");
    assert_eq!(v["routine"], "OMORDL");
    assert_eq!(v["label"], "LIST");
    assert_eq!(v["offset"], 2);
    assert_eq!(
        v["outcome"], "INT_SOURCE_REQUIRED",
        "the outcome is NAMED: no line was produced, and the reason is that no .INT text was \
         supplied — not that anything is absent"
    );
    assert_eq!(v["mapped_to_source_line"], false);
    let note = v["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("OMORDL.int"),
        "the remedy names the document to read: {note}"
    );
}

#[test]
fn a_resolved_frame_reports_an_int_line_and_claims_nothing_about_the_source() {
    let err = "<UNDEFINED>LIST+3^OMORDL";
    let frame = parse_int_frame(err).unwrap();
    let (is_error, v) = payload(map_int_answer(err, &frame, Some(INT)));
    assert!(!is_error);
    assert_eq!(v["success"], true);
    assert_eq!(v["int_line_number"], 6);
    assert_eq!(v["int_line"], " set i=\"\"");
    assert_eq!(v["label_line_number"], 3);
    assert_eq!(v["label_block_lines"], 6);
    assert_eq!(
        v["mapped_to_source_line"], false,
        "an INT line is exact; the .MAC line it came from is not computed here"
    );
    assert!(
        v.get("source_line").is_none() && v.get("source_line_number").is_none(),
        "and no source line is invented under any key: {v}"
    );
}

#[test]
fn each_refusal_reaches_the_caller_as_an_error_envelope_with_its_own_code() {
    let err = "<UNDEFINED>LIST+99^OMORDL";
    let frame = parse_int_frame(err).unwrap();
    let (is_error, v) = payload(map_int_answer(err, &frame, Some(INT)));
    assert!(is_error, "isError must be set: {v}");
    assert_eq!(v["success"], false);
    assert_eq!(v["error_code"], "INT_OFFSET_PAST_LABEL_BLOCK");
    assert_eq!(v["label_block_lines"], 6);
    assert_eq!(
        v["highest_offset_in_block"], 5,
        "the refusal says which offsets WOULD have resolved"
    );

    let err = "<UNDEFINED>NOPE+1^OMORDL";
    let frame = parse_int_frame(err).unwrap();
    let (is_error, v) = payload(map_int_answer(err, &frame, Some(INT)));
    assert!(is_error);
    assert_eq!(v["error_code"], "INT_LABEL_NOT_FOUND");
    assert_eq!(
        v["labels_total"], 3,
        "the denominator, so a truncated list is visible as truncated"
    );
    assert_eq!(v["labels_listed"], 3);
    assert_eq!(
        v["labels_present"],
        serde_json::json!(["OMORDL", "LIST", "zINIT"])
    );
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("says nothing about whether the routine"),
        "a missing label must not read as a missing routine: {v}"
    );
}

/// Every refusal carries a remedy IN THE ENVELOPE, not only in the review table.
/// `every_refusal_names_a_remedy`'s own header says it "does not assert the server puts that
/// sentence in the envelope at runtime" — this does, over all four at once.
#[test]
fn every_refusal_envelope_names_what_the_caller_should_do() {
    let cases: Vec<(&str, &str, Option<&str>)> = vec![
        ("INT_SOURCE_UNUSABLE", "<UNDEFINED>LIST+1^OMORDL", Some("")),
        ("INT_LABEL_NOT_FOUND", "<UNDEFINED>NOPE+1^OMORDL", Some(INT)),
        (
            "INT_OFFSET_PAST_LABEL_BLOCK",
            "<UNDEFINED>LIST+99^OMORDL",
            Some(INT),
        ),
        ("INT_OFFSET_INVALID", "<UNDEFINED>LIST+-1^OMORDL", Some(INT)),
    ];
    // CONTROL: the population is the four refusals it claims to be, driven through the real
    // entry point — otherwise the sweep below passes over whatever happened to come back.
    assert_eq!(cases.len(), 4, "4 refusals requested");
    let mut seen = Vec::new();
    for (code, err, src) in &cases {
        let frame = parse_int_frame(err).unwrap_or_else(|| panic!("{err} must parse"));
        let (is_error, v) = payload(map_int_answer(err, &frame, *src));
        assert!(is_error, "{code} must set isError: {v}");
        assert_eq!(v["error_code"], *code, "drove the wrong refusal: {v}");
        let hint = v["hint"].as_str().unwrap_or_default();
        assert!(
            !hint.trim().is_empty(),
            "{code} reaches a caller with nothing to do next: {v}"
        );
        seen.push(v["error_code"].as_str().unwrap_or_default().to_string());
    }
    assert_eq!(seen.len(), 4, "4 refusals requested / 4 envelopes read");
}

// ─── the dispatch, measured at the wire ──────────────────────────────────────────────────────
//
// "A routine frame is not looked up as a class" is a claim about a request that must NOT be
// sent, so it is asserted over the requests the server received rather than inferred from the
// answer. The class frame below is the control: the same handler, the same mock, and one
// `/action/query` — without it, a handler that had lost its connection entirely would satisfy
// the zero above and read exactly like the fix.

const NS: &str = "USER";

/// An Atelier that answers every `/action/query` with "no such class". No IRIS instance and no
/// credentials: `wiremock` binds a loopback port, and the strings below are placeholders it
/// never checks.
async fn atelier() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/USER/action/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": []},
            "result": {"content": [{"N": 0}]},
        })))
        .mount(&server)
        .await;
    server
}

fn params(error_string: &str, int_source: Option<&str>) -> DebugParams {
    serde_json::from_value(serde_json::json!({
        "action": "map_int",
        "error_string": error_string,
        "namespace": NS,
        "int_source": int_source,
    }))
    .expect("DebugParams accepts int_source")
}

async fn queries_seen(server: &MockServer) -> usize {
    // `.expect`, not `.unwrap_or_default()`: with recording off this returns None, and a
    // defaulted empty Vec would make "no query was sent" true for a reason that has nothing
    // to do with the handler — the exact shape this whole issue is about.
    server
        .received_requests()
        .await
        .expect("wiremock records requests")
        .iter()
        .filter(|r| r.url.path().ends_with("/action/query"))
        .count()
}

#[tokio::test]
async fn the_handler_asks_iris_about_a_class_only_when_the_frame_names_one() {
    let server = atelier().await;
    let iris = IrisConnection::new(
        server.uri(),
        NS,
        "placeholder",
        "placeholder",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();

    // The frame from the issue. It names a ROUTINE, so no class is looked up.
    let err = "<UNDEFINED>LIST+2^OMORDL";
    let (is_error, v) = payload(handle_iris_debug(&iris, &client, params(err, None)).await);
    let after_routine = queries_seen(&server).await;
    assert_eq!(
        after_routine, 0,
        "a routine frame sent {after_routine} SQL queries; it must send none, because \
         %Dictionary.CompiledClass cannot answer a question about a routine"
    );
    assert!(!is_error, "{v}");
    assert_eq!(v["frame_kind"], "routine");
    assert_eq!(v["outcome"], "INT_SOURCE_REQUIRED");
    assert!(
        !v.to_string().contains("ROUTINE_NOT_FOUND"),
        "the old answer must be gone: {v}"
    );

    // THE CONTROL: a class frame against the same mock does query, and the mock says the
    // class is absent, so the class refusal is unchanged and still reachable.
    let err = "<ILLEGAL VALUE>zSlotKey+1^Radio.BO.RisWorklistOut.1";
    let (is_error, v) = payload(handle_iris_debug(&iris, &client, params(err, None)).await);
    let after_class = queries_seen(&server).await;
    assert_eq!(
        after_class - after_routine,
        1,
        "a class frame must still be looked up: {v}"
    );
    assert!(is_error, "{v}");
    assert_eq!(v["error_code"], "ROUTINE_NOT_FOUND");
    assert_eq!(v["class"], "Radio.BO.RisWorklistOut");
}

/// The whole offline half, through the tool's own entry point: a routine frame plus the `.INT`
/// text the caller read for themselves resolves to an exact INT line, still without a query.
#[tokio::test]
async fn a_routine_frame_with_int_text_resolves_through_the_handler_without_any_query() {
    let server = atelier().await;
    let iris = IrisConnection::new(
        server.uri(),
        NS,
        "placeholder",
        "placeholder",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let err = "<UNDEFINED>zINIT+1^OMORDL";
    let (is_error, v) = payload(handle_iris_debug(&iris, &client, params(err, Some(INT))).await);
    assert_eq!(queries_seen(&server).await, 0, "still no class lookup: {v}");
    assert!(!is_error, "{v}");
    assert_eq!(
        v["label"], "zINIT",
        "the routine's own label, z intact — stripping it would have looked for INIT"
    );
    assert_eq!(v["int_line_number"], 10);
    assert_eq!(v["int_line"], " set ^OMORD(\"init\")=1");
}

/// And the refusals reach the caller through the handler, not only through the pure function.
#[tokio::test]
async fn a_refusal_survives_the_handler() {
    let server = atelier().await;
    let iris = IrisConnection::new(
        server.uri(),
        NS,
        "placeholder",
        "placeholder",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let err = "<UNDEFINED>LIST+42^OMORDL";
    let (is_error, v) = payload(handle_iris_debug(&iris, &client, params(err, Some(INT))).await);
    assert!(is_error, "{v}");
    assert_eq!(v["error_code"], "INT_OFFSET_PAST_LABEL_BLOCK");
    assert_eq!(queries_seen(&server).await, 0);
}
