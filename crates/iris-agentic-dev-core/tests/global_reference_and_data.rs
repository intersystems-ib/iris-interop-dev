//! #532: addressing a global node, and saying what is actually there.
//!
//! Upstream (`v1.6.2`, `tools/global.rs`) restricted every subscript to
//! `^[a-zA-Z0-9 _.:\-]+$` and then built the reference with
//! `format!("\"{}\"", s)` — no escaping. The allowlist is the only reason that is safe, so
//! widening it (which is the point of #532) and switching to `os_str_expr` are ONE change, not
//! two: either alone is worse than neither.
//!
//! The other half is what a read reports back. Upstream's `get` emits `1|value` where the `1` is
//! `$Data(ref) > 0`, so a node holding `""` and a node holding nothing but children both arrive
//! as `1|` — "the value is empty" and "there is no value here" collapsed into one answer.

use iris_agentic_dev_core::tools::global::{
    build_global_ref, data_status_from_code, global_name_error, subscript_expr, NodeData,
    Subscript, WalkEnd,
};

fn text(s: &str) -> Subscript {
    Subscript::Text(s.to_string())
}

// ─── subscripts: what legacy data actually contains ──────────────────────────────────────────

/// The reference from the issue, which upstream's allowlist rejects outright: a comma and a period
/// inside a string subscript, beside a numeric one.
#[test]
fn a_company_name_with_a_comma_is_one_subscript() {
    let r = build_global_ref(
        "CUST",
        &[text("X"), text("ACME, INC."), Subscript::from_i64(1)],
    );
    assert_eq!(r, r#"^CUST("X","ACME, INC.",1)"#);
}

/// A quote inside a subscript is the case that turns upstream's builder into code injection the
/// moment the allowlist is widened. It must be DOUBLED, which is ObjectScript's own escape.
#[test]
fn a_quote_inside_a_subscript_is_doubled_not_closed() {
    assert_eq!(subscript_expr(&text(r#"SAY "HI""#)), r#""SAY ""HI""""#);
    // An apostrophe is not special in ObjectScript string literals — it must NOT be escaped,
    // because escaping it would change the subscript and address a different node.
    assert_eq!(subscript_expr(&text("O'BRIEN")), r#""O'BRIEN""#);
}

/// THE injection control. A subscript crafted to close the reference and append a command must
/// come out as one inert literal. Asserting the shape is not enough — the payload's own
/// punctuation must not survive as syntax.
#[test]
fn a_subscript_cannot_close_the_reference_and_add_a_command() {
    let hostile = r#"") Kill ^IIDTest Set x=("#;
    let expr = subscript_expr(&text(hostile));
    // One literal: starts and ends with a quote, and every interior quote is doubled.
    assert!(expr.starts_with('"') && expr.ends_with('"'), "got {expr}");
    let interior = &expr[1..expr.len() - 1];
    for run in interior.split("\"\"") {
        assert!(
            !run.contains('"'),
            "an unpaired quote escapes the literal in {expr}"
        );
    }
    // And the whole reference is exactly one argument holding the payload as text. Stated as the
    // literal expected string, because "contains" assertions pass against a reference that also
    // contains something else.
    let r = build_global_ref("IIDTest", &[text(hostile)]);
    assert_eq!(
        r, r#"^IIDTest(""") Kill ^IIDTest Set x=(")"#,
        "the payload must appear only inside a quoted literal"
    );
}

/// A caret in a value or subscript is ordinary data in routine-era globals.
#[test]
fn a_caret_in_a_subscript_is_data() {
    assert_eq!(subscript_expr(&text("A^B")), r#""A^B""#);
}

/// Control characters and non-ASCII go through `os_str_expr`, so the emitted expression stays
/// pure ASCII and a newline cannot break the generated line.
#[test]
fn a_newline_or_an_accent_is_spliced_not_embedded() {
    let nl = subscript_expr(&text("a\nb"));
    assert!(nl.contains("$CHAR(10)"), "got {nl}");
    assert!(
        !nl.contains('\n'),
        "a raw newline would split the line: {nl}"
    );
    let accent = subscript_expr(&text("café"));
    assert!(accent.contains("$CHAR(233)"), "got {accent}");
    assert!(accent.is_ascii(), "the expression must be ASCII: {accent}");
}

// ─── the canonic-number rule ─────────────────────────────────────────────────────────────────

/// Corrected premise (see #532's comments): `^X(1)` and `^X("1")` are the SAME node, because IRIS
/// canonicalises a canonic numeric string. A quoted canonic number is therefore harmless.
///
/// What is NOT harmless is a NON-canonic numeric-looking string: `"01"` and `1` are different
/// nodes, so these must stay quoted or they become the wrong node.
#[test]
fn a_non_canonic_numeric_string_stays_a_quoted_string() {
    for s in ["01", "1.0", "-0", "+1", "5.", " 1", "1e3"] {
        let expr = subscript_expr(&text(s));
        assert_eq!(
            expr,
            format!("\"{s}\""),
            "{s} is not a canonic number, so unquoting it would address a different node"
        );
    }
}

/// A JSON number is emitted as a bare numeric literal, which IRIS canonicalises itself — so the
/// Rust side never has to implement canonicalisation. `1.0` as a NUMBER means node 1.
#[test]
fn a_json_number_is_emitted_bare_for_iris_to_canonicalise() {
    assert_eq!(subscript_expr(&Subscript::from_i64(1)), "1");
    assert_eq!(subscript_expr(&Subscript::from_i64(-42)), "-42");
    assert_eq!(subscript_expr(&Subscript::from_f64(0.5).unwrap()), "0.5");
    // The control: a bare emission is only safe for something IRIS reads as a number. Anything
    // that is not a plain numeric literal must fall back to a quoted string rather than be
    // spliced raw.
    assert_eq!(subscript_expr(&text("12abc")), r#""12abc""#);
}

// ─── $DATA: four states, not a boolean ───────────────────────────────────────────────────────

/// The defect this replaces: `$Data > 0` made 1, 10 and 11 the same answer.
#[test]
fn the_four_data_states_are_four_different_answers() {
    let states = [
        (0u8, NodeData::Absent),
        (1, NodeData::ValueOnly),
        (10, NodeData::ChildrenOnly),
        (11, NodeData::ValueAndChildren),
    ];
    for (code, expected) in states {
        assert_eq!(
            data_status_from_code(code),
            Some(expected),
            "$DATA={code} must map to its own state"
        );
    }
    // Every pair is distinct — a mapping that collapsed two of them would still pass the loop
    // above if it returned the same variant twice.
    let all: Vec<NodeData> = states.iter().map(|(_, s)| *s).collect();
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(all[i], all[j], "two $DATA states collapsed into one");
        }
    }
    // A code IRIS does not define is not silently a state.
    for bogus in [2u8, 9, 12, 100] {
        assert_eq!(data_status_from_code(bogus), None, "{bogus} is not a $DATA");
    }
}

/// The distinction that was lost, stated as the question a caller actually asks.
#[test]
fn an_empty_value_is_not_the_absence_of_a_value() {
    assert!(
        NodeData::ValueOnly.has_value(),
        "a node whose value is the empty string still HAS a value"
    );
    assert!(
        !NodeData::ChildrenOnly.has_value(),
        "a node with children and no value must not report a value"
    );
    assert!(NodeData::ChildrenOnly.has_children());
    assert!(!NodeData::ValueOnly.has_children());
    assert!(NodeData::ValueAndChildren.has_value() && NodeData::ValueAndChildren.has_children());
    assert!(!NodeData::Absent.has_value() && !NodeData::Absent.has_children());
}

// ─── truncation: three outcomes ──────────────────────────────────────────────────────────────

/// Upstream set `truncated = (count>=maxNodes)` AFTER a 5-second `$ZH` guard could also stop the
/// loop, and its own comment calls that "conservative and safe". A subtree cut short by the clock
/// reported as complete is the opposite of safe.
#[test]
fn a_walk_stopped_by_the_clock_is_not_a_complete_walk() {
    assert!(WalkEnd::Complete.is_complete());
    assert!(!WalkEnd::NodeCap.is_complete());
    assert!(!WalkEnd::TimeLimit.is_complete());
    // And the two incomplete cases are distinguishable, because the caller's next move differs:
    // raise the cap, or narrow the subtree.
    assert_ne!(WalkEnd::NodeCap, WalkEnd::TimeLimit);
    assert_eq!(WalkEnd::NodeCap.wire_code(), "cap");
    assert_eq!(WalkEnd::TimeLimit.wire_code(), "time");
    assert_eq!(WalkEnd::Complete.wire_code(), "done");
}

// ─── names and extended references ───────────────────────────────────────────────────────────

#[test]
fn a_global_name_is_validated_before_it_is_spliced() {
    for ok in ["IIDTest", "MyApp.Data", "%SYS", "A1"] {
        assert_eq!(global_name_error(ok), None, "{ok} is a valid global name");
    }
    for bad in [
        "",
        "1Leading",
        "has space",
        "has(paren",
        "a..b",
        "ends.",
        "x\"y",
    ] {
        assert!(
            global_name_error(bad).is_some(),
            "{bad} must be refused, not spliced"
        );
    }
}

/// An extended reference names a DIFFERENT namespace than the one the caller asked for, so
/// accepting it silently would route a read past the `namespace` parameter.
#[test]
fn an_extended_reference_is_refused_rather_than_redirected() {
    for ext in [r#"|"OTHER"|G"#, r#"["OTHER"]G"#, r#"|"^a"|G"#] {
        let e = global_name_error(ext);
        assert!(
            e.is_some(),
            "{ext} is an extended reference and must be refused"
        );
        let msg = e.unwrap();
        // "contains namespace" is NOT enough: the GENERIC refusal also mentions the namespace
        // parameter, so deleting the extended-reference branch entirely would still satisfy it
        // (these inputs fail the name regex anyway). The refusal has to say which problem it is,
        // or this test passes against an implementation that never recognised the case.
        assert!(
            msg.contains("extended reference"),
            "the refusal must name the case it diagnosed, got: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("namespace"),
            "and point at the namespace parameter, got: {msg}"
        );
    }
}

// ─── plan_read: everything that can be wrong about a request ─────────────────────────────────

use iris_agentic_dev_core::tools::global::{plan_read, GlobalAction, IrisGlobalParams};

fn params(action: &str) -> IrisGlobalParams {
    IrisGlobalParams {
        action: action.to_string(),
        global_name: "IIDTest".to_string(),
        subscripts: None,
        namespace: None,
        max_subscripts: None,
        max_nodes: None,
        after_subscript: None,
        after_node: None,
        value: None,
    }
}

/// An action this build does not have is refused BY NAME, with the alternatives. #541 is what the
/// other shape costs: an unrecognised value became a silent default.
#[test]
fn an_unknown_action_is_refused_and_names_the_valid_ones() {
    for bad in ["", "   ", "fetch", "read", "nonsense", "global"] {
        let mut p = params(bad);
        p.action = bad.to_string();
        let (code, msg) = plan_read(&p).expect_err(&format!("{bad:?} must be refused"));
        assert_eq!(code, "INVALID_ACTION", "{bad:?} -> {msg}");
        for valid in GlobalAction::NAMES {
            assert!(msg.contains(valid), "{msg} should offer {valid}");
        }
    }
    // The control: every READ action plans. Without it the loop above would pass against a
    // plan_read that refused everything.
    //
    // It is filtered by `is_write` rather than written as a literal list, because that is the
    // defect this control just had: it looped over `GlobalAction::NAMES` assuming every advertised
    // action was a read, and when #532's write half added `set` to NAMES the control began
    // asserting something false. A site that measures a population has to ask the population
    // which members it is about.
    let reads: Vec<&&str> = GlobalAction::NAMES
        .iter()
        .filter(|n| {
            !GlobalAction::parse(n)
                .expect("an advertised name parses")
                .is_write()
        })
        .collect();
    assert!(
        reads.len() >= 3,
        "the read actions are the subject of this file; found {reads:?}"
    );
    for good in reads {
        assert!(
            plan_read(&params(good)).is_ok(),
            "{good} must plan as a read"
        );
    }
    // Case and surrounding whitespace are tolerated, deliberately and for the same reason
    // `Toolset::parse` tolerates them — an action is a name the caller typed, not a token. My
    // first draft of this test listed "GET " as invalid and the code was right, not the test.
    for lenient in ["GET ", " get", "Walk", "  LIST  "] {
        assert!(
            plan_read(&params(lenient)).is_ok(),
            "{lenient:?} names a real action; case and padding are not errors"
        );
    }
}

/// `kill` is not an action of this tool, and the refusal has to say where removal DOES live —
/// otherwise a caller reads INVALID_ACTION as a typo and retries the same call.
///
/// This replaces `set_and_kill_are_refused_as_not_implemented_here`, which pinned the read-only
/// build: `set` is now implemented, so that test was asserting the absence of a feature that had
/// arrived. It is recorded here rather than silently deleted because the two halves of its claim
/// had different fates — `set` landed, `kill` was declined on purpose — and a reader finding only
/// the deletion would not be able to tell which.
#[test]
fn kill_is_not_an_action_here_and_the_refusal_names_the_confirmed_path() {
    for removal in ["kill", "delete", "remove"] {
        let (code, msg) = plan_read(&params(removal)).expect_err("must be refused");
        assert_eq!(code, "INVALID_ACTION", "{removal} -> {msg}");
        assert!(
            msg.contains("global_preview") && msg.contains("global_kill"),
            "a caller asking to remove a global must be told which tools do it, or the refusal \
             reads as 'you typed it wrong': {msg}"
        );
    }
    // `set` is an action now, and it must NOT reach the read planner. Different refusal, because
    // "that is not an action" and "that action is a write" are different facts.
    let (code, _) = plan_read(&params("set")).expect_err("a write must not plan as a read");
    assert_eq!(
        code, "INVALID_PARAMS",
        "set is a real action in the wrong place, not an unknown one"
    );
}

#[test]
fn a_bad_global_name_is_refused_before_anything_is_spliced() {
    let mut p = params("get");
    p.global_name = "has space".to_string();
    let (code, _) = plan_read(&p).expect_err("must be refused");
    assert_eq!(code, "INVALID_GLOBAL_NAME");

    // An extended reference keeps its own diagnosis through the plan.
    let mut p = params("get");
    p.global_name = r#"^|"OTHER"|G"#.to_string();
    let (code, msg) = plan_read(&p).expect_err("must be refused");
    assert_eq!(code, "INVALID_GLOBAL_NAME");
    assert!(msg.contains("extended reference"), "{msg}");

    // The leading caret is optional, not an error.
    let mut p = params("get");
    p.global_name = "^IIDTest".to_string();
    assert_eq!(plan_read(&p).expect("plans").name, "IIDTest");
}

/// The two resume parameters belong to different actions. Taking the wrong one silently would
/// restart from the beginning — a complete-looking answer that repeats a page it already returned.
#[test]
fn a_resume_parameter_meant_for_the_other_action_is_refused() {
    let mut p = params("list");
    p.after_node = Some("^IIDTest(1)".to_string());
    let (code, msg) = plan_read(&p).expect_err("after_node does not page a list");
    assert_eq!(code, "INVALID_PARAMS");
    assert!(msg.contains("after_subscript"), "name the right one: {msg}");

    let mut p = params("walk");
    p.after_subscript = Some(serde_json::json!("a"));
    let (code, msg) = plan_read(&p).expect_err("after_subscript does not page a walk");
    assert_eq!(code, "INVALID_PARAMS");
    assert!(msg.contains("after_node"), "name the right one: {msg}");

    // And each action accepts its OWN resume parameter, or the guard is just a ban.
    let mut p = params("list");
    p.after_subscript = Some(serde_json::json!("a"));
    assert!(plan_read(&p).is_ok(), "a list pages with after_subscript");
    let mut p = params("walk");
    p.after_node = Some("^IIDTest(1)".to_string());
    assert!(plan_read(&p).is_ok(), "a walk pages with after_node");
    // get takes neither.
    let mut p = params("get");
    p.after_node = Some("^IIDTest(1)".to_string());
    assert!(plan_read(&p).is_err());
}

/// The plan carries the action it validated, and the code it generated is that action's code — so
/// a caller cannot dispatch the output of one action to another's parser.
#[test]
fn the_plan_generates_the_code_for_the_action_it_reports() {
    let get = plan_read(&params("get")).expect("plans");
    assert_eq!(get.action, GlobalAction::Get);
    assert!(get.code.contains("$Data(^IIDTest)"), "{}", get.code);

    let list = plan_read(&params("list")).expect("plans");
    assert_eq!(list.action, GlobalAction::List);
    assert!(list.code.contains("$Order("), "{}", list.code);

    let walk = plan_read(&params("walk")).expect("plans");
    assert_eq!(walk.action, GlobalAction::Walk);
    assert!(walk.code.contains("$Query("), "{}", walk.code);
}

/// Subscripts arrive as JSON, so the number/string distinction has to survive deserialisation —
/// this is the path the tool actually takes, as opposed to the hand-built `Subscript` above.
#[test]
fn json_subscripts_keep_their_number_or_string_nature() {
    let mut p = params("get");
    p.subscripts = Some(vec![
        serde_json::json!("X"),
        serde_json::json!("ACME, INC."),
        serde_json::json!(1),
    ]);
    let plan = plan_read(&p).expect("plans");
    assert!(
        plan.code.contains(r#"^IIDTest("X","ACME, INC.",1)"#),
        "the number stays bare and the strings stay quoted: {}",
        plan.code
    );

    // A non-canonic numeric STRING must stay quoted, or it addresses a different node.
    let mut p = params("get");
    p.subscripts = Some(vec![serde_json::json!("01")]);
    assert!(
        plan_read(&p)
            .expect("plans")
            .code
            .contains(r#"^IIDTest("01")"#),
        "\"01\" is a string subscript"
    );
}
