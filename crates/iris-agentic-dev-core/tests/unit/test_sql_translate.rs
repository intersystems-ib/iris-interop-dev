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

// ── T012: SQLCODE on the next line survives verbatim (#177) ──────────────────

/// This test used to assert the DEFECT: that `if SQLCODE` was rewritten to a generated
/// `sqlSQLCODE{n}`. Once #145 moved the producer to the real `SQLCODE`, that rewrite
/// renamed the caller's read to a variable nothing sets, and every `&sql(...)` followed
/// by `If SQLCODE` threw `<UNDEFINED>`. The assertions passed through #145 untouched
/// because #145 only ADDED assertions about the producer and never asked whether the
/// existing ones still described a contract we wanted.
#[test]
fn test_sqlcode_next_line_is_not_renamed() {
    let code =
        "&sql(SELECT Name INTO :name FROM foo WHERE ID = :id)\nif SQLCODE { write \"err\",! }";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        r.translated_code.contains("\nif SQLCODE"),
        "the caller's SQLCODE read must survive verbatim: {}",
        r.translated_code
    );
    assert!(
        !r.translated_code.contains("sqlSQLCODE"),
        "no generated SQLCODE variable may appear — nothing sets one: {}",
        r.translated_code
    );
    assert!(
        r.translated_code.contains("set SQLCODE="),
        "the epilogue must still set SQLCODE under its real name: {}",
        r.translated_code
    );
}

/// The regression that would have caught #177: a SELECT INTO that FINDS a row and then
/// reads SQLCODE. The old code set the generated variable in the `else` branch ONLY, so
/// the failure was inverted — the statement threw precisely when the query SUCCEEDED.
#[test]
fn test_select_into_found_branch_leaves_sqlcode_readable() {
    let r = translate_sql_macros(
        "&sql(SELECT Name INTO :name FROM foo WHERE ID = 1)\nwrite SQLCODE, !",
    );
    let found_branch = r
        .translated_code
        .split(" } else {")
        .next()
        .expect("if/else emitted");
    assert!(
        !found_branch.contains("sqlSQLCODE"),
        "the found branch must not depend on a generated name: {found_branch}"
    );
    assert!(
        r.translated_code.contains("write SQLCODE, !"),
        "the read must be left alone: {}",
        r.translated_code
    );
}

/// #177: `%msg` is set by the epilogue under its real name too, so it needs no rewrite.
#[test]
fn test_msg_is_set_under_its_real_name() {
    let r = translate_sql_macros("&sql(SELECT Name INTO :n FROM foo)\nwrite %msg,!");
    assert!(
        r.translated_code.contains("%msg="),
        "epilogue must set %msg: {}",
        r.translated_code
    );
    assert!(
        r.translated_code.contains("write %msg,!"),
        "the caller's %msg read must survive verbatim: {}",
        r.translated_code
    );
}

/// #179: a SELECT with no INTO binds nothing, so every column read afterwards is
/// <UNDEFINED>. Emitting that silently is worse than refusing.
#[test]
fn test_select_without_into_warns_that_nothing_is_bound() {
    let r = translate_sql_macros("&sql(SELECT ID, Name FROM Ens_Config.Production)");
    assert!(r.found);
    assert!(
        r.warnings.iter().any(|w| w.contains("no INTO clause")),
        "expected an unbound-host-variable warning, got {:?}",
        r.warnings
    );
    let with_into = translate_sql_macros("&sql(SELECT Name INTO :n FROM foo)");
    assert!(
        !with_into.warnings.iter().any(|w| w.contains("no INTO")),
        "a SELECT INTO must not warn: {:?}",
        with_into.warnings
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

// ── T013: %msg is set, not rewritten (#177) ──────────────────────────────────

/// The sibling of T012, and it had the same defect for the same reason: the rewrite
/// pointed the caller's `%msg` at `{rs}.%Message`, which works only on the ONE line
/// immediately after the macro. The epilogue now sets `%msg` itself, so a read works
/// wherever it appears.
#[test]
fn test_msg_next_line_is_not_rewritten() {
    let code = "&sql(SELECT Name INTO :name FROM foo WHERE ID = :id)\nwrite %msg,!";
    let r = translate_sql_macros(code);
    assert!(r.found);
    assert!(
        r.translated_code.contains("\nwrite %msg,!"),
        "the caller's %msg read must survive verbatim: {}",
        r.translated_code
    );
    assert!(
        r.translated_code.contains("%msg=$Select("),
        "the epilogue must set %msg: {}",
        r.translated_code
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
        // SQLCODE next line (#177: set under its real name, never renamed)
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
                // #177: the read is left alone and the epilogue defines the name.
                assert!(
                    !r.translated_code.contains("sqlSQLCODE")
                        && r.translated_code.contains("set SQLCODE="),
                    "SQLCODE must be set, not renamed, for: {code}"
                );
            }
            "rewrite_msg" => {
                assert!(r.found);
                assert!(
                    r.translated_code.contains("%msg="),
                    "%msg must be set by the epilogue for: {code}"
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
