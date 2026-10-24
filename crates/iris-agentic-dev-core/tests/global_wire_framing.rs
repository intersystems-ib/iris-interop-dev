//! #532: a global's value is arbitrary bytes, so the wire format cannot be line- or pipe-delimited.
//!
//! Upstream framed `get` as `1|value` and the subtree walk as lines of `path|value`. Routine-era
//! globals hold `^`, `|` and embedded line feeds as ordinary data, so both split a single value
//! into phantom fields — the same defect #395 fixed one tool over, where a lookup key containing a
//! line feed became two entries and inflated the count. The repair is the same: length-prefix the
//! fields, which is what `parse_length_prefixed_entries` is for.

use iris_agentic_dev_core::tools::global::{
    build_get_code, build_list_code, build_walk_code, parse_get_output, parse_list_output,
    parse_walk_output, NodeData, Subscript, WalkEnd, WALK_SECONDS,
};

/// Frame fields the way the generated ObjectScript does: `<characters>:<text>`, concatenated.
/// Lengths are in CHARACTERS, matching `$LENGTH`.
fn frame(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|f| format!("{}:{}", f.chars().count(), f))
        .collect::<Vec<_>>()
        .join("")
}

fn text(s: &str) -> Subscript {
    Subscript::Text(s.to_string())
}

// ─── the framing itself ──────────────────────────────────────────────────────────────────────

/// Each of these characters broke upstream's format. They are data here.
#[test]
fn a_value_holding_the_old_delimiters_survives_intact() {
    for hostile in [
        "a|b",          // the field separator upstream used
        "line1\nline2", // the record separator upstream used
        "^OTHER(1)",    // a caret, which routine code stores as data
        "a|b\nc|d",     // both at once
        "DONE|3|0",     // upstream's own terminator, as a value
        "",             // the empty value, which must stay a value
        "   ",          // whitespace-only
    ] {
        let wire = frame(&["1", "^IIDTest(\"k\")", hostile]);
        let r = parse_get_output(&wire)
            .unwrap_or_else(|e| panic!("{hostile:?} should decode, got error: {e}"));
        assert_eq!(
            r.value.as_deref(),
            Some(hostile),
            "the value must come back byte-for-byte"
        );
        assert_eq!(r.data, NodeData::ValueOnly);
    }
}

/// The #395 defect at this layer: a SUBSCRIPT containing a newline is one subscript.
#[test]
fn a_subscript_holding_a_newline_is_one_subscript_not_two() {
    let wire = frame(&["a\nb", "plain", "done"]);
    let page = parse_list_output(&wire).expect("decodes");
    assert_eq!(
        page.subscripts,
        vec!["a\nb".to_string(), "plain".to_string()],
        "a length prefix delimits the entry; the newline inside it is data"
    );
    assert_eq!(page.end, WalkEnd::Complete);
}

// ─── $DATA drives whether there IS a value ───────────────────────────────────────────────────

/// THE distinction. Both of these carry `""` on the wire, because `$GET` returns `""` for a node
/// with children and no value. Only `$DATA` can tell them apart, which is why it is transmitted.
#[test]
fn an_empty_value_and_no_value_at_all_decode_differently() {
    let empty_value = parse_get_output(&frame(&["1", "^IIDTest(1)", ""])).expect("decodes");
    assert_eq!(empty_value.data, NodeData::ValueOnly);
    assert_eq!(
        empty_value.value.as_deref(),
        Some(""),
        "$DATA=1 means there IS a value, and it happens to be empty"
    );

    let children_only = parse_get_output(&frame(&["10", "^IIDTest(1)", ""])).expect("decodes");
    assert_eq!(children_only.data, NodeData::ChildrenOnly);
    assert_eq!(
        children_only.value, None,
        "$DATA=10 means no value of its own; reporting $GET's \"\" is the bug"
    );

    // And the two reports are not equal, which is the property a caller depends on.
    assert_ne!(empty_value, children_only);
}

/// `$DATA=11` carries a value AND children, and the value must not be dropped because children
/// exist — upstream's `$Query` walk skipped the base node's own value for exactly this case.
#[test]
fn a_node_with_a_value_and_children_reports_both() {
    let r = parse_get_output(&frame(&["11", "^IIDTest(1)", "mine"])).expect("decodes");
    assert_eq!(r.data, NodeData::ValueAndChildren);
    assert_eq!(r.value.as_deref(), Some("mine"));
    assert!(r.data.has_children());
}

#[test]
fn an_absent_node_has_no_value() {
    let r = parse_get_output(&frame(&["0", "^IIDTest(1)", ""])).expect("decodes");
    assert_eq!(r.data, NodeData::Absent);
    assert_eq!(r.value, None);
}

/// IRIS's canonicalisation is visible in the report, so a caller can see which node was read.
#[test]
fn the_report_carries_the_canonical_name_iris_resolved() {
    let r = parse_get_output(&frame(&["1", "^IIDTest(1)", "v"])).expect("decodes");
    assert_eq!(
        r.name, "^IIDTest(1)",
        "asking for ^IIDTest(\"1\") reads ^IIDTest(1), and the report should say so"
    );
}

// ─── a truncated or malformed read is an error, never a negative fact ────────────────────────

/// The house rule. Two fields plus a default would read as a node whose value is empty.
#[test]
fn a_short_read_is_an_error_not_a_node_with_no_value() {
    for short in [frame(&[]), frame(&["1"]), frame(&["1", "^IIDTest"])] {
        let e = parse_get_output(&short);
        assert!(
            e.is_err(),
            "{short:?} is an unfinished read and must not decode to a node"
        );
    }
    // The control: the full three fields DO decode, so the test above is not passing because
    // everything fails.
    assert!(parse_get_output(&frame(&["1", "^IIDTest", "v"])).is_ok());
}

/// A listing with no end marker is a read that did not finish. Upstream's `global_preview` minted
/// a kill-authorising token off output with no terminator; the lesson generalises.
#[test]
fn a_listing_with_no_end_marker_is_an_error_not_an_empty_level() {
    let e = parse_list_output("");
    assert!(e.is_err(), "no output at all is not an empty level");

    // Subscripts but no marker: the read was cut off mid-stream.
    let e = parse_list_output(&frame(&["a", "b"]));
    assert!(
        e.is_err(),
        "'b' is not an end marker, so this listing did not finish"
    );

    // A genuinely empty level says so, with its marker.
    let page = parse_list_output(&frame(&["done"])).expect("an empty level still has a marker");
    assert!(page.subscripts.is_empty());
    assert_eq!(page.end, WalkEnd::Complete);
}

#[test]
fn the_end_marker_distinguishes_complete_from_capped() {
    let done = parse_list_output(&frame(&["a", "done"])).expect("decodes");
    assert_eq!(done.end, WalkEnd::Complete);
    assert!(done.end.is_complete());

    let capped = parse_list_output(&frame(&["a", "cap"])).expect("decodes");
    assert_eq!(capped.end, WalkEnd::NodeCap);
    assert!(!capped.end.is_complete());
    assert_eq!(
        capped.subscripts,
        vec!["a".to_string()],
        "the marker is the marker, not a subscript"
    );
}

/// A `$DATA` IRIS does not define must not become a plausible state.
#[test]
fn an_unknown_data_code_is_an_error() {
    for bogus in ["2", "7", "99", "", "x"] {
        assert!(
            parse_get_output(&frame(&[bogus, "^IIDTest", "v"])).is_err(),
            "$DATA={bogus:?} is not a state this code may invent"
        );
    }
}

// ─── the generated code ──────────────────────────────────────────────────────────────────────

/// The listing reference is assembled from the name and parent subscripts, so punctuation inside a
/// quoted subscript cannot move the insertion point. Upstream found it with `rsplit_once(')')`.
#[test]
fn the_order_reference_is_built_from_the_parts_not_by_string_surgery() {
    // A parent subscript containing a closing parenthesis — the character upstream split on.
    let code = build_list_code("IIDTest", &[text("a)b")], 50, None);
    assert!(
        code.contains(r#"$Order(^IIDTest("a)b",sub))"#),
        "the loop variable must be appended as a subscript, got:\n{code}"
    );
}

/// Paging resumes from a given subscript rather than restarting, and the seed is quoted like any
/// other subscript.
#[test]
fn a_page_resumes_after_the_last_subscript_it_returned() {
    let fresh = build_list_code("IIDTest", &[], 50, None);
    assert!(
        fresh.contains(r#" Set sub = """#),
        "a first page starts at the beginning of the level, got:\n{fresh}"
    );
    let resumed = build_list_code("IIDTest", &[], 50, Some(&text("ACME, INC.")));
    assert!(
        resumed.contains(r#" Set sub = "ACME, INC.""#),
        "a later page seeds $ORDER with the last subscript returned, got:\n{resumed}"
    );
}

/// The cap is enforced in the generated code, not only in Rust, and it is clamped.
#[test]
fn the_subscript_cap_is_clamped_in_the_generated_code() {
    assert!(build_list_code("G", &[], 9999, None).contains(" Set maxSubs = 500"));
    assert!(build_list_code("G", &[], 0, None).contains(" Set maxSubs = 1"));
}

/// `get` asks for all three facts, and asks `$DATA` for the real number rather than a comparison.
#[test]
fn the_get_code_reads_data_as_a_number_not_as_a_boolean() {
    let code = build_get_code("IIDTest", &[Subscript::from_i64(1)]);
    assert!(code.contains("$Data(^IIDTest(1))"), "got:\n{code}");
    assert!(code.contains("$Name(^IIDTest(1))"), "got:\n{code}");
    assert!(code.contains("$Get(^IIDTest(1))"), "got:\n{code}");
    // The defect being replaced: a comparison collapses 1, 10 and 11 before Rust ever sees it.
    assert!(
        !code.contains("> 0") && !code.contains(">0"),
        "$DATA must be transmitted, not compared away:\n{code}"
    );
}

// ─── the subtree walk ────────────────────────────────────────────────────────────────────────

/// Each node carries its own `$DATA`, so a `$D=10` node inside a subtree does not acquire a value
/// it does not have. Upstream's walk emitted `path|value` with no status at all.
#[test]
fn every_node_in_a_walk_carries_its_own_data_state() {
    let wire = frame(&[
        "11",
        "^IIDTest(1)",
        "base-value", // a value AND children
        "10",
        "^IIDTest(1,\"kids\")",
        "", // children only — no value of its own
        "1",
        "^IIDTest(1,\"kids\",1)",
        "leaf", // an ordinary leaf
    ]) + &frame(&["done"]);
    let page = parse_walk_output(&wire).expect("decodes");
    assert_eq!(page.end, WalkEnd::Complete);
    assert_eq!(page.nodes.len(), 3);

    assert_eq!(page.nodes[0].data, NodeData::ValueAndChildren);
    assert_eq!(page.nodes[0].value.as_deref(), Some("base-value"));

    assert_eq!(page.nodes[1].data, NodeData::ChildrenOnly);
    assert_eq!(
        page.nodes[1].value, None,
        "a $D=10 node inside a walk has no value; the wire's \"\" is $GET's artefact"
    );

    assert_eq!(page.nodes[2].data, NodeData::ValueOnly);
    assert_eq!(page.nodes[2].value.as_deref(), Some("leaf"));
}

/// A value holding the characters upstream's `path|value` format used as structure.
#[test]
fn a_walked_value_may_contain_pipes_newlines_and_carets() {
    let hostile = "a|b\n^C(1)|d";
    let wire = frame(&["1", "^IIDTest(1)", hostile]) + &frame(&["done"]);
    let page = parse_walk_output(&wire).expect("decodes");
    assert_eq!(page.nodes.len(), 1, "one node, not four");
    assert_eq!(page.nodes[0].value.as_deref(), Some(hostile));
}

/// The three end reasons, and the fact a caller acts on each differently.
#[test]
fn the_walk_reports_which_bound_it_hit() {
    let one = frame(&["1", "^IIDTest(1)", "v"]);
    for (marker, expected) in [
        ("done", WalkEnd::Complete),
        ("cap", WalkEnd::NodeCap),
        ("time", WalkEnd::TimeLimit),
    ] {
        let page = parse_walk_output(&(one.clone() + &frame(&[marker]))).expect("decodes");
        assert_eq!(page.end, expected, "marker {marker}");
    }
    // The distinction upstream could not make: a clock-stopped walk is not a finished one.
    let timed = parse_walk_output(&(one.clone() + &frame(&["time"]))).expect("decodes");
    assert!(
        !timed.end.is_complete(),
        "a walk stopped by the clock must never read as the end of the data"
    );
}

/// Paging hands back a reference, not a reconstruction. A caller joining subscripts itself would
/// have to re-quote them, which is where the original injection lived.
#[test]
fn a_walk_hands_back_the_reference_to_resume_from() {
    let wire = frame(&[
        "1",
        "^IIDTest(1)",
        "a",
        "1",
        r#"^IIDTest("ACME, INC.")"#,
        "b",
    ]) + &frame(&["cap"]);
    let page = parse_walk_output(&wire).expect("decodes");
    assert_eq!(
        page.resume_after.as_deref(),
        Some(r#"^IIDTest("ACME, INC.")"#),
        "resume_after is the last node's canonical reference, verbatim"
    );
    // A complete walk still reports it — the caller should not have to special-case the end.
    let done = parse_walk_output(&(frame(&["1", "^IIDTest(1)", "a"]) + &frame(&["done"])))
        .expect("decodes");
    assert_eq!(done.resume_after.as_deref(), Some("^IIDTest(1)"));
    // And an empty walk has nothing to resume from.
    let empty = parse_walk_output(&frame(&["done"])).expect("decodes");
    assert_eq!(empty.resume_after, None);
    assert!(empty.nodes.is_empty());
}

/// A cut-off read is an error, not a shorter subtree. This is the shape the whole repo is about.
#[test]
fn a_partial_triple_is_a_truncated_read_not_a_smaller_subtree() {
    // Two full triples plus a dangling pair, then a marker: 8 fields, which is not 3n+1.
    let wire = frame(&[
        "1",
        "^IIDTest(1)",
        "a",
        "1",
        "^IIDTest(2)",
        "b",
        "1",
        "^IIDTest(3)",
    ]) + &frame(&["done"]);
    let e = parse_walk_output(&wire);
    assert!(
        e.is_err(),
        "8 node-fields is a truncated read; dropping the dangling pair would report 2 nodes as the \
         whole subtree"
    );
    // The control: 6 node-fields plus the marker DOES decode, so this is about the remainder and
    // not about everything failing.
    let ok = frame(&["1", "^IIDTest(1)", "a", "1", "^IIDTest(2)", "b"]) + &frame(&["done"]);
    assert_eq!(parse_walk_output(&ok).expect("decodes").nodes.len(), 2);
}

#[test]
fn a_walk_with_no_end_marker_is_an_error() {
    assert!(
        parse_walk_output("").is_err(),
        "no output is not an empty subtree"
    );
    // Nodes but no marker: the read was cut off after a complete triple, which is the case most
    // likely to look fine.
    let no_marker = frame(&["1", "^IIDTest(1)", "a"]);
    assert!(
        parse_walk_output(&no_marker).is_err(),
        "'a' is not an end marker, so this walk did not finish"
    );
}

// ─── the generated walk code ─────────────────────────────────────────────────────────────────

/// A resume point comes from a previous page's output, so it is spliced as DATA and dereferenced,
/// never pasted in as code. This is the same injection surface as a subscript, one layer along.
#[test]
fn a_resume_point_is_quoted_data_not_spliced_code() {
    let hostile = r#"^IIDTest(1) Kill ^IIDTest Set x="#;
    let code = build_walk_code("IIDTest", &[], 100, Some(hostile));
    assert!(
        code.contains(r#" Set node = "^IIDTest(1) Kill ^IIDTest Set x=""#),
        "the resume point must appear as a quoted literal, got:\n{code}"
    );
    assert!(
        !code.contains("\n Set node = ^IIDTest(1) Kill"),
        "the resume point was spliced as code:\n{code}"
    );
}

/// The base node is emitted on a first page and not on a resumed one, because `$QUERY` never
/// returns the node it starts from — so without this a `$D=11` base loses its own value, and with
/// it unconditionally a resumed page would repeat it.
#[test]
fn the_base_node_is_emitted_only_on_a_first_page() {
    let first = build_walk_code("IIDTest", &[], 100, None);
    assert!(
        first.contains(" Set node = base"),
        "a first page starts at the base, got:\n{first}"
    );
    assert!(
        first.contains("emitBase = ((bst>0)&(node=base))"),
        "the base is emitted when it exists AND this is the first page, got:\n{first}"
    );
    // $D=10 counts as existing: "has children, no value of its own" is a fact, not an empty row.
    assert!(
        !first.contains("bst#2"),
        "testing bst#2 would drop a $D=10 base, which upstream did:\n{first}"
    );
}

/// Both bounds are in the generated code, and the end reason is decided from STATE rather than
/// from the node count alone — which is what let upstream report a clock-stop as complete.
#[test]
fn the_walk_is_bounded_by_nodes_and_by_time_and_says_which() {
    let code = build_walk_code("IIDTest", &[], 250, None);
    assert!(code.contains(" Set maxN = 250"), "got:\n{code}");
    assert!(
        code.contains(&format!("($ZH-t0)>{WALK_SECONDS}")),
        "the clock bound must be in the loop, got:\n{code}"
    );
    // The end reason distinguishes all three, and "left the subtree" is tested FIRST so an
    // exhausted subtree is never reported as a bound.
    assert!(
        code.contains(r#"$Select('inSub:"done",cnt'<maxN:"cap",1:"time")"#),
        "the end reason must be decided from state, in that order, got:\n{code}"
    );
    // The cap is clamped like the listing cap.
    assert!(build_walk_code("G", &[], 99999, None).contains(" Set maxN = 1000"));
}

// ─── the response shape ──────────────────────────────────────────────────────────────────────

/// The distinction, as a caller sees it on the wire. Both nodes carry `"value": null`-vs-`""`, and
/// `has_value` is what makes them different rather than a judgement the caller has to make.
#[test]
fn the_response_distinguishes_an_empty_value_from_no_value() {
    use iris_agentic_dev_core::tools::global::node_json;

    let empty = parse_get_output(&frame(&["1", "^IIDTest(1)", ""])).expect("decodes");
    let none = parse_get_output(&frame(&["10", "^IIDTest(1)", ""])).expect("decodes");

    let e = node_json(&empty);
    let n = node_json(&none);

    assert_eq!(
        e["value"],
        serde_json::json!(""),
        "an empty value is a value"
    );
    assert_eq!(e["has_value"], serde_json::json!(true));
    assert_eq!(e["data"], serde_json::json!(1));

    assert_eq!(n["value"], serde_json::Value::Null, "no value is null");
    assert_eq!(n["has_value"], serde_json::json!(false));
    assert_eq!(n["data"], serde_json::json!(10));
    assert_eq!(n["has_children"], serde_json::json!(true));

    // The raw $DATA number is carried because routine code branches on it directly, and the words
    // are carried because a reader who does not know the convention would otherwise guess.
    assert_eq!(
        n["data_meaning"],
        serde_json::json!("children only, no value of its own")
    );
    assert_ne!(e["data_meaning"], n["data_meaning"]);
}

/// A truncated answer says so in the payload, not only in an enum nobody serialises.
#[test]
fn an_incomplete_read_says_so_in_the_payload() {
    use iris_agentic_dev_core::tools::global::{list_json, walk_json};

    let capped = parse_list_output(&frame(&["a", "b", "cap"])).expect("decodes");
    let j = list_json(&capped, "APP");
    assert_eq!(j["complete"], serde_json::json!(false));
    assert_eq!(j["stopped_because"], serde_json::json!("cap"));
    assert_eq!(j["count"], serde_json::json!(2));
    assert_eq!(j["resume_after_subscript"], serde_json::json!("b"));

    let done = parse_list_output(&frame(&["a", "done"])).expect("decodes");
    assert_eq!(list_json(&done, "APP")["complete"], serde_json::json!(true));

    // The walk's two incomplete reasons stay distinguishable in the payload.
    let timed = parse_walk_output(&(frame(&["1", "^IIDTest(1)", "v"]) + &frame(&["time"])))
        .expect("decodes");
    let w = walk_json(&timed, "APP");
    assert_eq!(w["complete"], serde_json::json!(false));
    assert_eq!(
        w["stopped_because"],
        serde_json::json!("time"),
        "a clock-stop must be reported as its own reason, not merged with the node cap"
    );
    assert_eq!(w["resume_after"], serde_json::json!("^IIDTest(1)"));
}
