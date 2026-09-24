//! #386: `iris_lookup_manage(get)` reported a key whose stored value is `""` as KEY_NOT_FOUND,
//! and `delete` of a missing key leaked a raw `#5810` carrying the internal `Table||key` ID.
//!
//! Measured 2026-09-24 against a live instance, with the store itself as the control:
//!
//! ```text
//! set   key=kempty value=""   -> success true, value ""
//! list_keys                   -> ["kempty","kfull"]        <- the store says it is there
//! get   key=kempty            -> KEY_NOT_FOUND             <- get says it is not
//! get   key=knever            -> KEY_NOT_FOUND             <- identical to the above
//! delete key=kempty           -> success true              <- you cannot delete what is absent
//! delete key=knever           -> ERROR #5810 ... ID 'ZzProbeLookup2||knever' under INTEROP_ERROR
//! ```
//!
//! And the reason `$DATA` is the right test, measured with its own control:
//!
//! ```text
//! $DATA(^Ens.LookupTable(T,"kE")) = 1   key whose value is ""
//! $DATA(^Ens.LookupTable(T,"kZ")) = 0   key never set        <- the control
//! $GET(^Ens.LookupTable(T,"kE"))  = ""  same as kZ by value
//! ```

use iris_agentic_dev_core::tools::interop::{
    build_lookup_delete_code, build_lookup_get_code, read_lookup_get_output, LookupValue,
    LOOKUP_VALUE_MARKER,
};

const T: &str = "\"MyTable\"";
const K: &str = "\"MyKey\"";

// ─── the decode: four outcomes, not two ──────────────────────────────────────────────────────

#[test]
fn an_empty_value_is_a_value_not_an_absence() {
    // The defect, at the layer it lived on.
    assert_eq!(
        read_lookup_get_output("LK_VALUE:"),
        LookupValue::Value(""),
        "a key holding the empty string exists and holds the empty string"
    );
}

#[test]
fn an_empty_reply_is_a_failed_program_not_an_empty_value() {
    // execute_via_generator returns Ok("") for a SqlProc that failed at runtime (#362), so this
    // is reachable. Before the marker it was indistinguishable from the case above.
    assert_eq!(
        read_lookup_get_output(""),
        LookupValue::ProgramFailed(""),
        "nothing came back, so nothing is known about the key"
    );
    assert_eq!(
        read_lookup_get_output("<SYNTAX>zLookup+4^Foo.1"),
        LookupValue::ProgramFailed("<SYNTAX>zLookup+4^Foo.1"),
        "an ObjectScript error is a failure, not a value"
    );
}

#[test]
fn a_stored_value_comes_back_whole() {
    assert_eq!(
        read_lookup_get_output("LK_VALUE:v"),
        LookupValue::Value("v")
    );
    // A value that itself looks like a marker or an error must survive.
    assert_eq!(
        read_lookup_get_output("LK_VALUE:LK_VALUE:x"),
        LookupValue::Value("LK_VALUE:x"),
        "only the first marker is the envelope"
    );
    assert_eq!(
        read_lookup_get_output("LK_VALUE:ERROR:KEY_NOT_FOUND:x"),
        LookupValue::Value("ERROR:KEY_NOT_FOUND:x"),
        "a value that reads like a refusal is still a value"
    );
}

#[test]
fn the_two_absences_stay_distinct() {
    assert_eq!(
        read_lookup_get_output("ERROR:TABLE_NOT_FOUND:Table not found: T"),
        LookupValue::TableNotFound("Table not found: T")
    );
    assert_eq!(
        read_lookup_get_output("ERROR:KEY_NOT_FOUND:Key not found: K in table T."),
        LookupValue::KeyNotFound("Key not found: K in table T.")
    );
}

// ─── the generated programs ──────────────────────────────────────────────────────────────────

#[test]
fn get_tests_existence_with_data_not_with_the_value() {
    let code = build_lookup_get_code(T, K);
    assert!(
        code.contains("$DATA(^Ens.LookupTable(\"MyTable\",\"MyKey\"))"),
        "the key's existence must be read from $DATA on the key node: {code}"
    );
    assert!(
        !code.contains("If tVal=\"\""),
        "a value comparison cannot answer existence — that was the defect: {code}"
    );
}

#[test]
fn get_writes_its_value_behind_the_marker() {
    let code = build_lookup_get_code(T, K);
    let write = code
        .lines()
        .find(|l| l.starts_with("Write "))
        .expect("get must still write the value");
    let marker = write
        .find(LOOKUP_VALUE_MARKER)
        .unwrap_or_else(|| panic!("without the marker an empty value is an empty reply: {write}"));
    let value = write
        .find("$GET(^Ens.LookupTable(\"MyTable\",\"MyKey\"))")
        .unwrap_or_else(|| panic!("the value itself still comes from $GET: {write}"));
    // BEHIND, not merely present. A marker appended as a SUFFIX would satisfy `contains` while
    // `strip_prefix` failed for every non-empty value — and would then read as ProgramFailed.
    assert!(
        marker < value,
        "the value must come after the marker, or strip_prefix cannot find it: {write}"
    );
}

#[test]
fn delete_refuses_a_missing_key_before_removing_anything() {
    let code = build_lookup_delete_code(T, K);
    let key_check = code
        .find("$DATA(^Ens.LookupTable(\"MyTable\",\"MyKey\"))")
        .expect("delete must check the key exists — it only checked the table");
    let remove = code
        .find("%RemoveValue")
        .expect("delete must still remove the value");
    assert!(
        key_check < remove,
        "checking after removing is how the raw #5810 got out:\n{code}"
    );
    let precheck = code
        .lines()
        .find(|l| l.contains("KEY_NOT_FOUND"))
        .expect("the key refusal must be in the program");
    assert!(
        precheck.contains("Quit"),
        "the precheck must return, or %RemoveValue runs anyway: {precheck}"
    );
}

#[test]
fn the_key_refusal_names_the_table_and_the_way_to_list_keys() {
    for code in [build_lookup_get_code(T, K), build_lookup_delete_code(T, K)] {
        let line = code
            .lines()
            .find(|l| l.contains("KEY_NOT_FOUND"))
            .expect("key refusal");
        assert!(
            line.contains("in table \"_\"MyTable\""),
            "the refusal must name the table it looked in: {line}"
        );
        assert!(
            line.contains("action=list_keys"),
            "the refusal must name the remedy: {line}"
        );
    }
}

#[test]
fn both_arms_share_one_presence_check() {
    // Two copies of an existence test is how `get` and `delete` came to disagree about it.
    let get = build_lookup_get_code(T, K);
    let del = build_lookup_delete_code(T, K);
    let head = |c: &str| {
        c.lines()
            .take_while(|l| !l.starts_with("Write ") && !l.starts_with("Set tSC="))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        head(&get),
        head(&del),
        "get and delete must ask the same question of the store"
    );
    assert!(
        head(&get).contains("TABLE_NOT_FOUND") && head(&get).contains("KEY_NOT_FOUND"),
        "the shared head is the two presence checks:\n{}",
        head(&get)
    );
}

#[test]
fn neither_program_leaks_the_internal_composite_id() {
    // The raw #5810 exposed `Table||key`, which a caller cannot act on.
    for code in [build_lookup_get_code(T, K), build_lookup_delete_code(T, K)] {
        assert!(
            !code.contains("||"),
            "the internal composite ID format must not be built into a message: {code}"
        );
    }
}
