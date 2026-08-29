// Unit tests for translate_sql_macros() — &sql macro translation for iris_execute.
// These tests make no IRIS connections.

use iris_agentic_dev_core::tools::translate_sql_macros;

// ── T006: No &sql → no-op ────────────────────────────────────────────────────

#[test]
fn test_no_sql_macro_is_noop() {
    let code = "set x = 1\nwrite x,!";
    let r = translate_sql_macros(code);
    assert!(!r.found, "found should be false");
    assert_eq!(r.translated_code, code);
    assert!(r.warnings.is_empty());
}

#[test]
fn test_empty_code_is_noop() {
    let r = translate_sql_macros("");
    assert!(!r.found);
    assert_eq!(r.translated_code, "");
}

// ── T007: SELECT INTO single variable ────────────────────────────────────────

#[test]
fn test_select_into_single_var() {
    let code = "&sql(SELECT Name INTO :name FROM %Dictionary.ClassDefinition WHERE ID = :id)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        r.translated_code.contains("%SQL.Statement"),
        "should contain %SQL.Statement"
    );
    assert!(
        r.translated_code.contains("%Prepare("),
        "should contain %Prepare"
    );
    assert!(
        r.translated_code.contains("%Execute("),
        "should contain %Execute"
    );
    assert!(
        r.translated_code.contains("%Get(\"Name\")"),
        "should contain %Get(\"Name\")"
    );
    assert!(
        r.translated_code.contains("set name = "),
        "should set 'name' variable"
    );
    // No-rows branch: name set to ""
    assert!(
        r.translated_code.contains("set name = \"\""),
        "no-rows branch should set name to empty string"
    );
}

// ── T008: SELECT INTO multiple variables ─────────────────────────────────────

#[test]
fn test_select_into_multiple_vars() {
    let code = "&sql(SELECT Name, Description INTO :nm, :desc FROM MyApp.Table WHERE ID = :id)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(r.translated_code.contains("%Get(\"Name\")"));
    assert!(r.translated_code.contains("%Get(\"Description\")"));
    assert!(r.translated_code.contains("set nm = "));
    assert!(r.translated_code.contains("set desc = "));
}

// ── T009: INSERT DML ──────────────────────────────────────────────────────────

#[test]
fn test_insert_dml() {
    let code = "&sql(INSERT INTO MyApp.Log (Message) VALUES (:msg))";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        r.translated_code.contains("%ExecDirect"),
        "INSERT should use %ExecDirect"
    );
    assert!(
        r.translated_code.contains("INSERT INTO MyApp.Log"),
        "SQL should be preserved"
    );
    assert!(
        r.translated_code.contains("msg)"),
        "msg variable should appear as arg"
    );
}

// ── T010: UPDATE DML ─────────────────────────────────────────────────────────

#[test]
fn test_update_dml() {
    let code = "&sql(UPDATE MyApp.Foo SET Name = :name WHERE ID = :id)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(r.translated_code.contains("%ExecDirect"));
    // Both host vars should appear as positional args
    assert!(
        r.translated_code.contains("name,"),
        "name should be first param"
    );
    assert!(r.translated_code.contains("id)"), "id should be last param");
}

// ── T011: DELETE DML ─────────────────────────────────────────────────────────

#[test]
fn test_delete_dml() {
    let code = "&sql(DELETE FROM MyApp.Foo WHERE ID = :id)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(r.translated_code.contains("%ExecDirect"));
    assert!(r.translated_code.contains("DELETE FROM MyApp.Foo"));
}

// ── T012: SQLCODE on next line rewritten ──────────────────────────────────────

#[test]
fn test_sqlcode_next_line_rewritten() {
    let code =
        "&sql(SELECT Name INTO :name FROM foo WHERE ID = :id)\nif SQLCODE { write \"err\",! }";
    let r = translate_sql_macros(code);
    assert!(r.found);
    // The SQLCODE on the NEXT line should be rewritten
    assert!(
        !r.translated_code.contains("\nif SQLCODE"),
        "bare SQLCODE on next line should be rewritten"
    );
    assert!(
        r.translated_code.contains("sqlSQLCODE"),
        "should contain generated SQLCODE var"
    );
}

#[test]
fn test_sqlcode_elsewhere_not_rewritten() {
    // SQLCODE on a DIFFERENT line (not immediately after &sql) should NOT be touched
    let code = "set x = SQLCODE\n&sql(SELECT Name INTO :name FROM foo WHERE ID = :id)\nwrite name,!\nif SQLCODE { }";
    let r = translate_sql_macros(code);
    // The leading SQLCODE and the SQLCODE two lines after &sql should remain
    // (only the line immediately after the macro gets rewritten)
    assert!(r.found);
    // At minimum, the leading "set x = SQLCODE" line should be untouched
    assert!(
        r.translated_code.contains("set x = SQLCODE"),
        "SQLCODE not immediately after &sql should be untouched"
    );
}

// ── T013: %msg on next line rewritten ────────────────────────────────────────

#[test]
fn test_msg_next_line_rewritten() {
    let code = "&sql(SELECT Name INTO :name FROM foo WHERE ID = :id)\nwrite %msg,!";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        !r.translated_code.contains("\nwrite %msg"),
        "%msg on next line should be rewritten"
    );
    assert!(
        r.translated_code.contains(".%Message") || r.translated_code.contains("_sqlMsg"),
        "should reference result set message"
    );
}

// ── T014: CALL falls through with warning ────────────────────────────────────

#[test]
fn test_call_falls_through_with_warning() {
    let code = "&sql(CALL MyProc(1, 2))";
    let r = translate_sql_macros(code);
    assert!(r.found, "found should be true (macro detected)");
    assert!(!r.warnings.is_empty(), "should have a warning for CALL");
    assert!(
        r.translated_code.contains("&sql(CALL"),
        "CALL should be left in translated_code unchanged"
    );
    assert!(
        r.warnings[0].to_lowercase().contains("call"),
        "warning should mention CALL"
    );
}

// ── T015: Multiple &sql macros — collision avoidance ─────────────────────────

#[test]
fn test_multiple_sql_macros_unique_vars() {
    let code = "&sql(SELECT Name INTO :n1 FROM foo WHERE ID = 1)\n&sql(SELECT Name INTO :n2 FROM foo WHERE ID = 2)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    // Should have sqlrs1 and sqlrs2 (different result set vars)
    assert!(
        r.translated_code.contains("sqlrs1"),
        "first macro should use sqlrs1"
    );
    assert!(
        r.translated_code.contains("sqlrs2"),
        "second macro should use sqlrs2"
    );
}

// ── T016: SELECT INTO no-rows sets vars to "" ────────────────────────────────

#[test]
fn test_select_into_no_rows_sets_empty_string() {
    let code = "&sql(SELECT Name INTO :name FROM foo WHERE 1 = 0)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    // The else branch must set name = ""
    assert!(
        r.translated_code.contains("set name = \"\""),
        "no-rows else branch must set name to empty string"
    );
}

// ── T017: Paren depth — nested parens handled correctly ──────────────────────

#[test]
fn test_nested_parens_correct_boundary() {
    let code = "&sql(SELECT * FROM foo WHERE x IN (SELECT id FROM bar))\nwrite \"done\",!";
    let r = translate_sql_macros(code);
    assert!(r.found);
    // The translation should not include "write done" in the SQL
    let sql_part = r.translated_code.clone();
    // After translation, "write done" should still be on a separate line
    assert!(
        sql_part.contains("write \"done\""),
        "code after &sql should be preserved"
    );
}

// ── T018: Column alias — %Get uses alias ─────────────────────────────────────

#[test]
fn test_column_alias_uses_alias() {
    let code = "&sql(SELECT Name AS nm INTO :nm FROM foo WHERE ID = :id)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        r.translated_code.contains("%Get(\"nm\")"),
        "should use alias 'nm' not original column name"
    );
}

// ── T023/T024: translate_sql param behavior (structural) ─────────────────────

#[test]
fn test_translate_result_found_true_when_macro_present() {
    let r = translate_sql_macros("&sql(SELECT 1 INTO :x)");
    assert!(r.found);
    assert!(
        !r.translated_code.contains("&sql("),
        "translated_code should not contain &sql"
    );
}

#[test]
fn test_translate_result_found_false_when_no_macro() {
    let r = translate_sql_macros("set x = 42\nwrite x,!");
    assert!(!r.found);
    assert!(r.translated_code == "set x = 42\nwrite x,!");
}

// ── T031: translate_sql: false means no translation (structural) ─────────────

#[test]
fn test_code_with_sql_passes_through_when_not_called() {
    // When translate_sql=false, the handler should NOT call translate_sql_macros.
    // This test verifies that translate_sql_macros does NOT modify code passed directly.
    // (The handler logic test is in E2E; here we verify the function contract)
    let code = "&sql(SELECT 1 INTO :x)";
    let r = translate_sql_macros(code);
    // When called, it DOES translate. The handler's job is to not call it when translate_sql=false.
    assert!(r.found, "function always translates when called");
}

// ── T037/T038: US3 — multi-column and warnings ───────────────────────────────

#[test]
fn test_multi_column_translated_code_has_all_gets() {
    let code = "&sql(SELECT ColA, ColB INTO :a, :b FROM foo)";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(r.translated_code.contains("%Get(\"ColA\")"));
    assert!(r.translated_code.contains("%Get(\"ColB\")"));
}

#[test]
fn test_call_warning_message_is_descriptive() {
    let r = translate_sql_macros("&sql(CALL MyProc(1))");
    assert!(!r.warnings.is_empty());
    let warning = &r.warnings[0];
    assert!(warning.len() > 10, "warning should be descriptive");
}

// ── SC-001: ≥15 patterns validation (T048) ───────────────────────────────────

#[test]
fn test_sc001_representative_patterns() {
    let cases: &[(&str, &str)] = &[
        // SELECT INTO single var
        ("&sql(SELECT Name INTO :name FROM foo WHERE ID = 1)", "found"),
        // SELECT INTO multi var
        ("&sql(SELECT A, B INTO :a, :b FROM foo)", "found"),
        // INSERT
        ("&sql(INSERT INTO foo (Col) VALUES (:val))", "execDirect"),
        // UPDATE
        ("&sql(UPDATE foo SET Name = :n WHERE ID = :id)", "execDirect"),
        // DELETE
        ("&sql(DELETE FROM foo WHERE ID = :id)", "execDirect"),
        // SQLCODE next line
        ("&sql(SELECT Name INTO :n FROM foo WHERE 1=1)\nif SQLCODE { }", "rewrite_sqlcode"),
        // %msg next line
        ("&sql(SELECT Name INTO :n FROM foo WHERE 1=1)\nwrite %msg,!", "rewrite_msg"),
        // No rows semantics
        ("&sql(SELECT Name INTO :name FROM foo WHERE 1=0)", "no_rows"),
        // Nested parens
        ("&sql(SELECT * FROM foo WHERE x IN (SELECT id FROM bar))", "found"),
        // Column alias
        ("&sql(SELECT Name AS n INTO :n FROM foo)", "alias"),
        // Multiple macros
        ("&sql(SELECT A INTO :a FROM foo)\n&sql(SELECT B INTO :b FROM bar)", "multi"),
        // CALL warning
        ("&sql(CALL MyProc())", "call_warning"),
        // No &sql — noop
        ("set x = 1\nwrite x,!", "noop"),
        // MERGE (if classified)
        ("&sql(MERGE INTO foo USING src ON foo.ID = src.ID WHEN MATCHED THEN UPDATE SET Name = src.Name)", "execDirect_or_warn"),
        // DML with multiple params
        ("&sql(INSERT INTO foo (A, B, C) VALUES (:a, :b, :c))", "execDirect"),
    ];

    for (code, pattern_type) in cases {
        let r = translate_sql_macros(code);
        match *pattern_type {
            "found" => assert!(r.found, "Expected found=true for: {code}"),
            "execDirect" | "execDirect_or_warn" => {
                assert!(r.found, "Expected found=true for: {code}");
                // Either ExecDirect or a warning — both are valid
                let ok = r.translated_code.contains("%ExecDirect") || !r.warnings.is_empty();
                assert!(ok, "Expected ExecDirect or warning for: {code}");
            }
            "rewrite_sqlcode" => {
                assert!(r.found);
                assert!(
                    r.translated_code.contains("sqlSQLCODE")
                        || !r.translated_code.contains("\nif SQLCODE"),
                    "SQLCODE should be rewritten for: {code}"
                );
            }
            "rewrite_msg" => {
                assert!(r.found);
                assert!(
                    !r.translated_code.contains("\nwrite %msg"),
                    "%msg should be rewritten for: {code}"
                );
            }
            "no_rows" => {
                assert!(r.found);
                assert!(
                    r.translated_code.contains("\"\""),
                    "no-rows branch should set vars to empty for: {code}"
                );
            }
            "alias" => {
                assert!(r.found);
                assert!(
                    r.translated_code.contains("%Get(\"n\")"),
                    "alias should be used for: {code}"
                );
            }
            "multi" => {
                assert!(r.found);
                assert!(
                    r.translated_code.contains("sqlrs1") && r.translated_code.contains("sqlrs2"),
                    "multiple macros should use unique vars for: {code}"
                );
            }
            "call_warning" => {
                assert!(r.found);
                assert!(
                    !r.warnings.is_empty(),
                    "CALL should produce warning for: {code}"
                );
            }
            "noop" => {
                assert!(!r.found, "No &sql should be noop for: {code}");
            }
            _ => {}
        }
    }
}

// ── #145: the &sql contract includes SQLCODE and %ROWCOUNT ──────────────────
//
// Reproduced live on IRIS 2026.1 before the fix: the INSERT below COMMITTED and
// the caller still saw `<UNDEFINED> ... SQLCODE`, because the rewrite ran the
// statement and set neither variable. A caller reading that concluded the write
// had failed when the row was already there.

#[test]
fn a_translated_dml_sets_sqlcode_and_rowcount() {
    let r = translate_sql_macros("&sql(INSERT INTO T (A) VALUES ('x'))");
    assert!(r.found);
    assert!(
        r.translated_code.contains("set SQLCODE="),
        "SQLCODE must be set under its REAL name, or a same-line read still throws: {}",
        r.translated_code
    );
    assert!(
        r.translated_code.contains("%ROWCOUNT="),
        "%ROWCOUNT is part of the same contract: {}",
        r.translated_code
    );
}

/// The reported shape exactly: the read is on the SAME line as the &sql, which
/// the next-line rewrite never covered.
#[test]
fn sqlcode_is_readable_on_the_same_line_as_the_statement() {
    let r = translate_sql_macros(
        "&sql(INSERT INTO Ens_Util.LookupTable (TableName) VALUES ('P')) Write \"sqlcode=\",SQLCODE,!",
    );
    let code = &r.translated_code;
    let set_at = code.find("set SQLCODE=").expect("SQLCODE assigned");
    let read_at = code.find("Write").expect("the original read survives");
    assert!(
        set_at < read_at,
        "the assignment has to precede the read or it is useless: {code}"
    );
}

/// execute_via_generator maps submitted line N to RunUser+N. A multi-line
/// expansion of a one-line DML would silently break every frame number the
/// #124 work made trustworthy.
#[test]
fn translating_a_one_line_dml_stays_one_line() {
    let r = translate_sql_macros("&sql(DELETE FROM T WHERE A=1)");
    assert_eq!(
        r.translated_code.lines().count(),
        1,
        "line count must survive translation: {}",
        r.translated_code
    );
}

#[test]
fn a_successful_select_into_also_sets_sqlcode() {
    let r = translate_sql_macros("&sql(SELECT A INTO :a FROM T WHERE B=1)");
    assert!(
        r.translated_code.contains("set SQLCODE="),
        "the found branch left SQLCODE undefined too: {}",
        r.translated_code
    );
}
