//! #430 item 3: the neutral trims — omit what carries no information, and DECLARE every omission.
//!
//! The whole item is one judgement applied three times: an empty value and an absent value are the
//! same fact for a *description*, and different facts for a *count*. So the rule these tests pin is
//! not "emit fewer bytes", it is:
//!
//!   - drop a member whose empty value says nothing (`description: ""`, `repeating: false`);
//!   - NEVER drop one whose value is a claim about completeness (`values_truncated: true`,
//!     `errors_incomplete: true`, or a console that holds the errors a short list omitted);
//!   - when something IS dropped, say so in the payload, because a consumer that finds a field
//!     missing will otherwise read the absence as "there was none".

use iris_agentic_dev_core::tools::hl7_schema::{field_json, Hl7Field};
use iris_agentic_dev_core::tools::{
    console_holds_a_warning, flatten_symbol_names, omit_clean_console,
};

// ─── hl7_schema_inspect field rows ───────────────────────────────────────────────────────────

fn full_field() -> Hl7Field {
    Hl7Field {
        number: "3".into(),
        name: "PatientIdentifierList".into(),
        repeating: true,
        description: "Patient identifiers".into(),
        type_raw: "DS:2.5:CX()".into(),
        data_type: "CX".into(),
        optionality: "R".into(),
        values: vec!["A".into(), "F".into()],
        values_truncated: true,
    }
}

#[test]
fn a_fully_populated_field_keeps_every_member() {
    let v = field_json(&full_field());
    let o = v.as_object().expect("an object");
    for k in [
        "number",
        "name",
        "repeating",
        "description",
        "type_raw",
        "data_type",
        "optionality",
        "values",
        "values_truncated",
    ] {
        assert!(o.contains_key(k), "{k} must survive when populated: {v}");
    }
}

#[test]
fn the_empty_members_are_dropped() {
    let v = field_json(&Hl7Field {
        number: "1".into(),
        name: "SetId".into(),
        ..Default::default()
    });
    let o = v.as_object().expect("an object");
    // Identity survives even when blank — these are how a caller refers to the row.
    assert_eq!(o.get("number").and_then(|x| x.as_str()), Some("1"));
    assert_eq!(o.get("name").and_then(|x| x.as_str()), Some("SetId"));
    for k in [
        "repeating",
        "description",
        "type_raw",
        "data_type",
        "optionality",
        "values",
        "values_truncated",
    ] {
        assert!(!o.contains_key(k), "{k} carries nothing and must go: {v}");
    }
}

/// `type_raw` is the schema's unparsed form. It earns its bytes only when it DIFFERS from the
/// parsed `data_type`: `DS:2.5:CX()` against `CX` is worth seeing, `SI` against `SI` is the same
/// string twice.
#[test]
fn type_raw_survives_only_when_it_differs_from_data_type() {
    let differs = field_json(&Hl7Field {
        type_raw: "DS:2.5:CX()".into(),
        data_type: "CX".into(),
        ..Default::default()
    });
    assert_eq!(
        differs["type_raw"].as_str(),
        Some("DS:2.5:CX()"),
        "{differs}"
    );

    let same = field_json(&Hl7Field {
        type_raw: "SI".into(),
        data_type: "SI".into(),
        ..Default::default()
    });
    assert!(
        same.get("type_raw").is_none(),
        "the same string twice is not information: {same}"
    );
    assert_eq!(
        same["data_type"].as_str(),
        Some("SI"),
        "but the parsed type stays: {same}"
    );
}

/// `values_truncated: true` says `values` is a PARTIAL list. Dropping it would turn "IRIS did not
/// list every permitted value" into "these are all of them" — on exactly the field a caller would
/// validate an inbound message against.
#[test]
fn a_truncated_value_list_always_says_so() {
    let truncated = field_json(&Hl7Field {
        values: vec!["A".into()],
        values_truncated: true,
        ..Default::default()
    });
    assert_eq!(
        truncated["values_truncated"], true,
        "a partial list must announce itself: {truncated}"
    );

    let complete = field_json(&Hl7Field {
        values: vec!["A".into(), "B".into()],
        values_truncated: false,
        ..Default::default()
    });
    assert!(
        complete.get("values_truncated").is_none(),
        "a complete list needs no flag: {complete}"
    );
    assert!(complete.get("values").is_some(), "{complete}");
}

// ─── the compiler console ────────────────────────────────────────────────────────────────────

fn clean_payload(field: &str) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        field: ["Compilation finished successfully in 0.003s."],
    })
}

#[test]
fn a_clean_console_is_dropped_and_the_omission_is_declared() {
    for field in ["console", "compile_console"] {
        let mut p = clean_payload(field);
        assert!(omit_clean_console(&mut p, field, true), "{field}");
        assert!(p.get(field).is_none(), "{field} must be gone: {p}");
        assert_eq!(
            p[format!("{field}_omitted")],
            true,
            "the absence must be DECLARED, not inferred: {p}"
        );
    }
}

/// Both spellings go through the same function. This is the sibling rule made structural: there is
/// no second implementation to fall behind.
#[test]
fn the_two_console_field_names_behave_identically() {
    let mut a = clean_payload("console");
    let mut b = clean_payload("compile_console");
    assert_eq!(
        omit_clean_console(&mut a, "console", true),
        omit_clean_console(&mut b, "compile_console", true),
    );
    assert_eq!(a["console_omitted"], b["compile_console_omitted"]);
}

#[test]
fn a_console_with_diagnostics_is_kept() {
    let mut p = clean_payload("console");
    assert!(
        !omit_clean_console(&mut p, "console", false),
        "not clean: nothing may be dropped"
    );
    assert!(p["console"].is_array(), "the console stays: {p}");
    assert!(
        p.get("console_omitted").is_none(),
        "and no omission is claimed: {p}"
    );
}

/// `errors_incomplete` means the parsed error list is a SUBSET and the console holds the rest. The
/// hint written beside that flag tells the reader to look in the console, so removing it would make
/// the hint point at a field that is not there.
#[test]
fn an_incomplete_error_list_pins_the_console_in_place() {
    let mut p = serde_json::json!({
        "success": true,
        "errors_incomplete": true,
        "console": ["Detected 24 errors during compilation"],
    });
    assert!(
        !omit_clean_console(&mut p, "console", true),
        "errors_incomplete outranks a clean-looking success"
    );
    assert!(p["console"].is_array(), "{p}");
}

/// An omission is never claimed for a field that was not there. `console_omitted: true` beside a
/// response that never carried a console would be an invented fact.
#[test]
fn an_absent_console_is_not_reported_as_omitted() {
    let mut p = serde_json::json!({"success": true});
    assert!(!omit_clean_console(&mut p, "console", true));
    assert!(
        p.get("console_omitted").is_none(),
        "nothing was dropped, so nothing is declared: {p}"
    );
}

/// The scan that decides whether `iris_doc`'s console may be dropped.
///
/// Matched through `parse_console_diag`, not by searching for the word: a console line that merely
/// MENTIONS a warning is not a warning, and treating it as one would pin every console in place.
#[test]
fn a_warning_line_is_recognised_by_its_diagnostic_shape() {
    assert!(console_holds_a_warning([
        "Compiling class Demo.T",
        "WARNING: Demo.T.cls(M1+2) #6001: Something questionable",
    ]));
    assert!(
        console_holds_a_warning(["WARNING #6001:4: Something questionable"]),
        "both prefixes IRIS uses"
    );
}

#[test]
fn an_ordinary_console_holds_no_warning() {
    assert!(!console_holds_a_warning([
        "Compilation finished successfully in 0.003s.",
        "Compiling class Demo.T",
    ]));
    assert!(
        !console_holds_a_warning(["ERROR: Demo.T.cls(M1+2) #1002: real error"]),
        "an error is not a warning — the error path keeps the console for its own reason"
    );
    assert!(
        !console_holds_a_warning(["the previous run produced a WARNING about this class"]),
        "a line mentioning a warning is not a diagnostic"
    );
    assert!(!console_holds_a_warning(Vec::<&str>::new()));
}

// ─── iris_symbols name projection ────────────────────────────────────────────────────────────

#[test]
fn symbol_rows_flatten_to_bare_names() {
    let rows = vec![
        serde_json::json!({"Name": "Ens.Director"}),
        serde_json::json!({"Name": "EnsLib.HL7.Message"}),
    ];
    let (names, unreadable) = flatten_symbol_names(&rows);
    assert_eq!(names, vec!["Ens.Director", "EnsLib.HL7.Message"]);
    assert_eq!(unreadable, 0);
}

/// A row with no readable `Name` is COUNTED, not dropped. Silently shortening the list would make a
/// projection failure look like a smaller namespace — the house bug, on a tool whose answer a model
/// treats as the set of classes that exist.
#[test]
fn a_row_without_a_readable_name_is_counted_not_discarded() {
    let rows = vec![
        serde_json::json!({"Name": "Ens.Director"}),
        serde_json::json!({"Nmae": "typo in the projection"}),
        serde_json::json!({"Name": 42}),
        serde_json::json!({}),
    ];
    let (names, unreadable) = flatten_symbol_names(&rows);
    assert_eq!(names, vec!["Ens.Director"], "only the readable one");
    assert_eq!(
        unreadable, 3,
        "and the other three are accounted for, not vanished"
    );
    // The invariant that makes the count trustworthy: nothing is lost between the two.
    assert_eq!(names.len() + unreadable, rows.len());
}

#[test]
fn no_rows_yields_no_names_and_no_unreadable() {
    let (names, unreadable) = flatten_symbol_names(&[]);
    assert!(names.is_empty());
    assert_eq!(unreadable, 0, "an empty input is not an unreadable one");
}
