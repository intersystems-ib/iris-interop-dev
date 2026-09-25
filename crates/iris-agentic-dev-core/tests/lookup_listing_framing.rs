//! #394: `list_keys` and `list_tables` framed entries with `$CHAR(10)` and split on newlines, so a
//! key or table name containing a line feed became two phantom entries and inflated the count.
//!
//! Measured 2026-09-25, namespace USER, with the global itself as the authority:
//!
//! ```text
//! one key "a\nb"       ($ORDER: key len=3, hasLF=1)
//!   list_keys  -> {"count":2,"keys":["a","b"]}
//!   get "a\nb" -> success, value "v"      <- the real key
//!   get "a"    -> KEY_NOT_FOUND           <- neither reported name exists
//!   get "b"    -> KEY_NOT_FOUND
//!
//! one table "ZzA\nZzB" ($ORDER: table len=7, hasLF=1)
//!   list_tables -> {"count":3,"total_count":3,"tables":["%IRIS_X12ReplyType","ZzA","ZzB"]}
//! ```
//!
//! `set`, `get`, `delete` and the value path were all correct; a value containing a newline
//! round-tripped intact. Only the two listings were wrong.

use iris_agentic_dev_core::tools::interop::{
    build_lookup_list_keys_code, build_lookup_list_tables_code, parse_length_prefixed_entries,
};

// ─── the decode ──────────────────────────────────────────────────────────────────────────────

#[test]
fn an_entry_containing_a_newline_stays_one_entry() {
    // The defect, at the layer it lived on.
    assert_eq!(
        parse_length_prefixed_entries("3:a\nb").unwrap(),
        vec!["a\nb".to_string()],
        "a length prefix delimits the entry; the newline inside it is data"
    );
}

#[test]
fn several_entries_decode_in_order_including_awkward_ones() {
    // Lengths are in CHARACTERS: "a\0"=2, "x\ny\r"=4, "  "=2, "ZzA\nZzB"=7.
    // (My first draft mislabelled the first entry as length 1; the parser refused it rather than
    // truncating, which is the whole point of returning Err.)
    let stream = "2:a\u{0}4:x\ny\r2:  7:ZzA\nZzB";
    assert_eq!(
        parse_length_prefixed_entries(stream).unwrap(),
        vec![
            "a\u{0}".to_string(), // a NUL inside an entry
            "x\ny\r".to_string(), // both line endings
            "  ".to_string(),     // whitespace-only name survives; the old code trimmed it away
            "ZzA\nZzB".to_string(),
        ]
    );
}

#[test]
fn no_entries_is_an_empty_list_not_an_error() {
    assert_eq!(
        parse_length_prefixed_entries("").unwrap(),
        Vec::<String>::new()
    );
    // The transport can leave a trailing newline; that is not a malformed stream.
    assert_eq!(
        parse_length_prefixed_entries("\n").unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        parse_length_prefixed_entries("1:a\n").unwrap(),
        vec!["a".to_string()]
    );
}

#[test]
fn a_malformed_stream_is_an_error_not_a_shorter_list() {
    // The point of the whole change: the old framing degraded SILENTLY into a plausible list.
    // A parser that skipped what it could not read would do the same, reporting fewer keys than
    // the table holds as though that were the answer.
    for (bad, why) in [
        ("a:b", "a length that is not digits"),
        ("3", "a length with no colon"),
        ("3-abc", "the wrong separator"),
        ("9:ab", "a length longer than what remains"),
        ("1:a2", "a trailing fragment with no colon"),
    ] {
        assert!(
            parse_length_prefixed_entries(bad).is_err(),
            "{bad:?} ({why}) must be refused, not silently truncated"
        );
    }
}

#[test]
fn a_length_that_lies_long_is_refused_rather_than_clamped() {
    let e = parse_length_prefixed_entries("9:ab").expect_err("must fail");
    assert!(
        e.contains("claims 9") && e.contains("only 2 remain"),
        "the error must say what did not add up: {e}"
    );
}

// ─── the generated programs ──────────────────────────────────────────────────────────────────

#[test]
fn both_programs_write_a_length_prefix_and_no_newline_delimiter() {
    let keys = build_lookup_list_keys_code("\"MyTable\"");
    let tables = build_lookup_list_tables_code();
    for (what, code) in [("list_keys", &keys), ("list_tables", &tables)] {
        let write = code
            .lines()
            .find(|l| l.contains("$ORDER"))
            .unwrap_or_else(|| panic!("{what} must still walk the global"));
        assert!(
            write.contains("$LENGTH("),
            "{what} must length-prefix each entry: {write}"
        );
        assert!(
            !write.contains("$CHAR(10)"),
            "{what} must not use a newline as the delimiter — that is the defect: {write}"
        );
    }
}

#[test]
fn the_length_precedes_the_entry_in_the_written_expression() {
    // Position, not presence: writing the entry and THEN its length would keep both substrings
    // while making the stream undecodable.
    for code in [
        build_lookup_list_keys_code("\"MyTable\""),
        build_lookup_list_tables_code(),
    ] {
        let write = code
            .lines()
            .find(|l| l.contains("$ORDER"))
            .expect("walk line");
        let len_at = write.find("$LENGTH(").expect("length");
        let colon_at = write
            .find("_\":\"_")
            .expect("the separator between length and entry");
        assert!(
            len_at < colon_at,
            "the length must be written before the entry: {write}"
        );
    }
}

#[test]
fn list_keys_still_refuses_a_missing_table_first() {
    let code = build_lookup_list_keys_code("\"MyTable\"");
    let guard = code
        .find("ERROR:TABLE_NOT_FOUND:")
        .expect("the table precheck must survive the reframing");
    let walk = code.find("$ORDER").expect("the walk");
    assert!(
        guard < walk,
        "the table check must still come before the walk:\n{code}"
    );
}
