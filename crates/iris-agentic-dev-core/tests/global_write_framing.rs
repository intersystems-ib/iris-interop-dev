//! #532, write half: what the generated write says, and what its reply is allowed to mean.
//!
//! The sibling of `global_wire_framing.rs`, which does this for the reads. The property under
//! test throughout: a write's success is READ BACK from IRIS, never inferred from the fact that
//! IRIS accepted the statement. Upstream decides a kill worked with
//! `out.trim().contains("KILLED")` against a literal its own generated code writes
//! unconditionally — true for any path that reaches the `Write`.

use iris_agentic_dev_core::tools::global::{
    build_set_code, parse_set_output, set_json, set_outcome, write_did_not_land, NodeData,
    SetReport, WRITE_DID_NOT_LAND,
};

/// Frame fields the way the generator does: `<characters>:<text>`, then one newline.
fn frame(fields: &[&str]) -> String {
    let mut out = String::new();
    for f in fields {
        out.push_str(&format!("{}:{}", f.chars().count(), f));
    }
    out.push('\n');
    out
}

// ─── the generated statement ─────────────────────────────────────────────────────────────────

/// A value is an ObjectScript expression, not text pasted between quotes. The three characters
/// that break naive quoting are a quote, a caret and a line feed, and all three are ordinary in
/// routine-era data.
#[test]
fn a_value_is_escaped_into_an_expression_not_embedded() {
    let code = build_set_code("IIDTest", &[], "he said \"hi\"\nand ^THAT was that");
    // The quote is doubled and the line feed is spliced, so the whole Set stays on one line.
    let set_line = code
        .lines()
        .find(|l| l.contains(" Set ^IIDTest = "))
        .expect("the Set statement is present");
    assert!(
        !set_line.contains('\n'),
        "the Set must be one line: a raw line feed in the value would split the statement"
    );
    assert!(
        set_line.contains("\"\""),
        "an embedded quote must be doubled, not left to close the literal: {set_line}"
    );
    assert!(
        set_line.contains("$CHAR(") || set_line.contains("$Char("),
        "a line feed must be spliced as a $CHAR, not embedded: {set_line}"
    );
    assert!(
        set_line.contains('^'),
        "a caret in the VALUE is data and survives into the expression: {set_line}"
    );
    // The caret must appear inside a quoted run, not as a bare reference.
    assert!(
        !set_line.contains("= ^"),
        "the value must not be emitted as a bare global reference: {set_line}"
    );
}

/// The success oracle. `$DATA` is read AFTER the write, in the same program, so "it landed" is a
/// measurement rather than an assumption — and the prior `$DATA` is read BEFORE it, because
/// "created" and "overwrote" are different events the caller cannot recover afterwards.
#[test]
fn the_write_reads_data_back_after_setting_and_before() {
    let code = build_set_code("IIDTest", &[], "v");
    let lines: Vec<&str> = code.lines().collect();
    let pos = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle} in:\n{code}"))
    };
    let prior = pos(" Set pr = $Data(");
    let write = pos(" Set ^IIDTest = ");
    let after = pos(" Set st = $Data(");
    assert!(
        prior < write && write < after,
        "order must be: read $DATA, write, read $DATA again — got prior={prior} write={write} \
         after={after} in:\n{code}"
    );
    assert!(
        !code.contains(r#"Write "ok""#) && !code.contains(r#"Write "KILLED""#),
        "an echoed literal is not evidence the write landed; the read-back is: {code}"
    );
}

/// The reference is built from the parts, so a subscript holding a comma stays one subscript on
/// the write path exactly as it does on the read path.
#[test]
fn a_subscripted_write_addresses_one_node() {
    let code = build_set_code(
        "CUST",
        &[
            iris_agentic_dev_core::tools::global::Subscript::Text("ACME, INC.".to_string()),
            iris_agentic_dev_core::tools::global::Subscript::from_i64(1),
        ],
        "v",
    );
    assert!(
        code.contains(r#" Set ^CUST("ACME, INC.",1) = "v""#),
        "a comma inside a subscript is data, and a JSON number is emitted bare:\n{code}"
    );
}

// ─── what the reply is allowed to mean ───────────────────────────────────────────────────────

#[test]
fn a_write_reports_what_data_said_before_and_after() {
    let r =
        parse_set_output(&frame(&["0", "1", "^IIDTest(\"new\")"])).expect("three fields decode");
    assert_eq!(r.prior, NodeData::Absent);
    assert_eq!(r.current, NodeData::ValueOnly);
    assert!(r.created(), "$DATA 0 before means the value did not exist");
    assert_eq!(r.name, "^IIDTest(\"new\")");
}

/// A node that already had children but no value of its own: the VALUE was created, and the
/// children are reported separately rather than making this look like an overwrite.
#[test]
fn a_node_with_children_and_no_value_reports_its_value_as_created() {
    let r = parse_set_output(&frame(&["10", "11", "^IIDTest(\"parent\")"]))
        .expect("three fields decode");
    assert_eq!(r.prior, NodeData::ChildrenOnly);
    assert_eq!(r.current, NodeData::ValueAndChildren);
    assert!(
        r.created(),
        "$DATA 10 before means there was no value here, so this write created one"
    );
    assert!(
        r.current.has_children(),
        "the children were already there and must still be reported"
    );
}

#[test]
fn overwriting_an_existing_value_is_not_reported_as_a_creation() {
    let r = parse_set_output(&frame(&["1", "1", "^IIDTest"])).expect("three fields decode");
    assert!(
        !r.created(),
        "$DATA 1 before means a value was already there; reporting that as created hides an \
         overwrite of somebody's data"
    );
}

/// A value that is the empty string is still a value. This is the read half's distinction holding
/// on the write path: after setting `""` the node has `$DATA` 1, not 0.
#[test]
fn writing_an_empty_string_is_a_value_not_an_absence() {
    let code = build_set_code("IIDTest", &[], "");
    assert!(
        code.contains(" Set ^IIDTest = \"\""),
        "an empty value is written as an empty string literal:\n{code}"
    );
    let r = parse_set_output(&frame(&["0", "1", "^IIDTest"])).expect("decodes");
    assert!(
        r.current.has_value(),
        "a node set to the empty string HAS a value; $DATA says 1, and treating that as absent is \
         the defect the read half was written to fix"
    );
}

#[test]
fn a_short_write_reply_is_an_error_not_a_landed_write() {
    let err = parse_set_output(&frame(&["0", "1"])).expect_err("two fields is a truncated reply");
    assert!(
        err.contains("UNKNOWN"),
        "a truncated write reply means the outcome is unknown, not that it worked: {err}"
    );
    assert!(
        err.contains("action=get"),
        "and it must say how to find out what actually happened: {err}"
    );
}

#[test]
fn an_unknown_data_code_on_the_write_path_is_an_error() {
    for bad in [&["7", "1", "^X"], &["0", "7", "^X"]] {
        let err = parse_set_output(&frame(bad)).expect_err("7 is not a $DATA value");
        assert!(
            err.contains('7'),
            "the refusal must name the value it could not read: {err}"
        );
    }
}

/// The last line of defence. IRIS accepted the Set, answered all three fields, and still reports
/// no value: the only honest answer is that the write did not land.
#[test]
fn a_write_that_reads_back_as_absent_is_not_a_success() {
    let r = SetReport {
        name: "^IIDTest".to_string(),
        prior: NodeData::Absent,
        current: NodeData::Absent,
    };
    assert!(
        !r.current.has_value(),
        "this is the condition the dispatch checks"
    );
    let (code, msg) = write_did_not_land(&r);
    assert_eq!(code, WRITE_DID_NOT_LAND);
    assert!(
        msg.contains("NOT there") && msg.contains("not a successful write"),
        "the message must say plainly that the value is absent: {msg}"
    );
    assert!(
        msg.contains("read-only"),
        "and name the likeliest cause, a mapping to a read-only database: {msg}"
    );
}

/// The payload distinguishes the two outcomes a caller cares about, and carries both `$DATA`
/// states so nothing has to be inferred from the new one alone.
#[test]
fn the_payload_says_whether_it_created_or_overwrote() {
    let created = set_json(
        &SetReport {
            name: "^IIDTest(1)".to_string(),
            prior: NodeData::Absent,
            current: NodeData::ValueOnly,
        },
        "APP",
    );
    assert_eq!(created["created"], serde_json::json!(true));
    assert_eq!(created["previous_data"], serde_json::json!(0));
    assert_eq!(created["data"], serde_json::json!(1));
    assert_eq!(created["namespace"], serde_json::json!("APP"));
    assert_eq!(created["action"], serde_json::json!("set"));

    let overwrote = set_json(
        &SetReport {
            name: "^IIDTest(1)".to_string(),
            prior: NodeData::ValueAndChildren,
            current: NodeData::ValueAndChildren,
        },
        "APP",
    );
    assert_eq!(overwrote["created"], serde_json::json!(false));
    assert_eq!(overwrote["has_children"], serde_json::json!(true));
    assert_ne!(
        created["created"], overwrote["created"],
        "creating and overwriting must not render identically"
    );
}

/// The write oracle, as the dispatch now reaches it: a node that reads back without a value is a
/// REFUSAL, not a success envelope describing an empty node.
///
/// Both directions in one test. Only the pair pins the branch — asserting the refusal alone passes
/// against a `set_outcome` that refuses everything, and asserting the success alone passes against
/// one that never refuses.
#[test]
fn a_write_whose_read_back_is_absent_is_refused_not_reported() {
    let landed = SetReport {
        name: "^IIDTest".to_string(),
        prior: NodeData::Absent,
        current: NodeData::ValueOnly,
    };
    let body = set_outcome(&landed, "APP").expect("a node with a value is a completed write");
    assert_eq!(body["success"], serde_json::json!(true));
    assert_eq!(body["created"], serde_json::json!(true));

    let vanished = SetReport {
        name: "^IIDTest".to_string(),
        prior: NodeData::Absent,
        current: NodeData::Absent,
    };
    let (code, msg) = set_outcome(&vanished, "APP")
        .expect_err("a node that reads back absent did NOT take the write");
    assert_eq!(code, WRITE_DID_NOT_LAND);
    assert!(msg.contains("NOT there"), "{msg}");

    // The children-only case is the subtle one: $DATA 10 means the node has children and no value
    // of its own, so a `set` that lands there and reports 10 has NOT stored the value either.
    let children_only = SetReport {
        name: "^IIDTest(\"parent\")".to_string(),
        prior: NodeData::ChildrenOnly,
        current: NodeData::ChildrenOnly,
    };
    assert!(
        set_outcome(&children_only, "APP").is_err(),
        "$DATA 10 after a set means the value is not there; $GET would return the empty string \
         for it, which is exactly the collapse the read half was written to prevent"
    );
}
