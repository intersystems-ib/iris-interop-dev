//! #273: a `%UnitTest` method that ABORTS carried no detail at all.
//!
//! THE REPORTED CAUSE WAS WRONG, AND THE REAL ONE MAKES THIS SMALL.
//!
//! The report said the log parser fails to match `LogStateStatus:…` lines. But `failed_tests` is not
//! built from log text on this path — it comes from SQL, and all three detail fields are subqueries
//! over `%UnitTest_Result.TestAssert` filtered `Status=0`. An abort never creates such a row, so they
//! return NULL. `LogStateStatus` is indeed 0 hits in this tree, but because nothing parses log lines
//! here.
//!
//! WHERE THE TEXT ACTUALLY IS — read out of the IRIS source, not inferred:
//!
//! `%UnitTest.Manager.LogStateStatus` persists BEFORE it prints —
//! `Do LogStateStatus^%SYS.UNITTEST(...,action,errortext)` — and `LogStateStatusPrivate` does
//!
//! ```objectscript
//! Set ^UnitTest.Result(id,testsuite,testcase,testmethod) = $lb(0, 0, action, errortext)
//! ```
//!
//! which is the TestMethod node itself: `Status`, `Duration`, **`ErrorAction`**, **`ErrorDescription`**.
//! A failed assert instead writes a SUBnode (`LogAssertPrivate`) → `TestAssert`.
//!
//! | event | written to | previously read |
//! |---|---|---|
//! | failed assert | `TestAssert` subnode | yes |
//! | abort | `TestMethod.ErrorDescription` / `.ErrorAction` | **no — not in the SELECT** |
//!
//! So the data was already in the row the query reads. It just was not selected.
//!
//! NOT VERIFIED HERE: that IRIS populates those columns in practice. `%UnitTest_Result.TestMethod` is
//! empty on the read-only instance available for verification (`total = 0`), which is a clean zero and
//! not evidence; the source above is what establishes it. Producing a real abort means running a suite,
//! a write this session does not make on a shared instance.

use serde::Serialize;

/// Which kind of failure a red test was.
pub const KIND_ASSERT: &str = "assert";
pub const KIND_RUNTIME_ERROR: &str = "runtime_error";

/// The detail for one red test, from whichever source has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct FailureDetail {
    pub message: Option<String>,
    pub location: Option<String>,
    pub assert: Option<String>,
    /// `assert` or `runtime_error`. `None` when the test is red but neither source carried anything —
    /// which is a real state and must not be reported as either kind.
    pub kind: Option<&'static str>,
}

impl FailureDetail {
    pub fn is_runtime_error(&self) -> bool {
        self.kind == Some(KIND_RUNTIME_ERROR)
    }
}

/// Pull the ObjectScript frame `Label+offset^Routine` out of an error text.
///
/// The real shape, from the report:
///
/// ```text
/// ERROR #5002: ObjectScript error: <PROPERTY DOES NOT EXIST>TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1 *Username,EnsLib.SQL.OutboundAdapter
/// ```
///
/// The frame is emitted RAW and nothing more is promised. The report measured it useful in 8 of 13
/// aborts; the other 5 point into library code (`RunOneTestCase+83^%UnitTest.Manager.1`) where there is
/// no `.cls` to open. Translating the `.INT` offset to a `.cls` line is NOT possible on this version —
/// `%Studio.Debugger.MapToINT` does not exist on IRIS 2026.1 — so pretending otherwise would be worse
/// than a raw frame.
pub fn extract_frame(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let caret = text.find('^')?;
    // Walk back over `Label+offset`. A label is alphanumeric/underscore/%; the offset is `+digits`.
    let mut start = caret;
    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '%' {
            start -= 1;
        } else {
            break;
        }
    }
    // Walk forward over the routine name, which may contain dots and a leading %.
    let mut end = caret + 1;
    while end < bytes.len() {
        let c = bytes[end] as char;
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '%' {
            end += 1;
        } else {
            break;
        }
    }
    // A bare `^` with nothing usable on one side is not a frame.
    if start == caret || end == caret + 1 {
        return None;
    }
    let frame = text[start..end].trim_end_matches('.').to_string();
    (!frame.is_empty()).then_some(frame)
}

/// `Some(trimmed)` only when there is actually text. A free fn rather than a closure: a closure's
/// lifetime binds to its first call site, which does not survive being used on two different borrows.
fn nonempty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// Choose the detail for one red test.
///
/// PRECEDENCE: a failed assert wins. When a test both asserts and then aborts, the assert is the
/// specific, actionable fact and the abort is often its consequence — and reporting the abort would
/// replace a named comparison with a stack frame.
///
/// `error_action` is deliberately NOT put in `assert`. For an abort it is the METHOD name (the log
/// renders `LogStateStatus:0:<action>:<text>`), not an assertion like `AssertStatusOK`, and putting a
/// method name in a field callers read as "which assertion failed" would be a plausible-looking lie.
pub fn failure_detail(
    fail_msg: Option<&str>,
    fail_loc: Option<&str>,
    fail_act: Option<&str>,
    error_description: Option<&str>,
    _error_action: Option<&str>,
) -> FailureDetail {
    if let Some(msg) = nonempty(fail_msg) {
        return FailureDetail {
            message: Some(msg.to_string()),
            location: nonempty(fail_loc).map(str::to_string),
            assert: nonempty(fail_act).map(str::to_string),
            kind: Some(KIND_ASSERT),
        };
    }
    if let Some(err) = nonempty(error_description) {
        return FailureDetail {
            message: Some(err.to_string()),
            // The frame is inside the error text itself; there is no separate column for it.
            location: extract_frame(err),
            // No assertion was involved. None, not the action.
            assert: None,
            kind: Some(KIND_RUNTIME_ERROR),
        };
    }
    // Red with nothing recorded anywhere. A real state — keep every field null and claim no kind,
    // rather than inventing one.
    FailureDetail::default()
}

/// The row keys [`shape_method_row`] reads. The query MUST alias every one of them.
///
/// A mis-aliased column is invisible: `r["ErrDesc"]` on a row that does not have it is `Null`, which is
/// exactly what an absent abort looks like — so the feature would silently do nothing. This list is the
/// contract between the two halves, asserted by `the_query_aliases_every_key_the_shaper_reads`.
pub const ROW_KEYS: &[&str] = &[
    "Class", "Method", "St", "FailMsg", "FailLoc", "FailAct", "ErrDesc", "ErrAct",
];

/// The query that reads a finished run out of the `%UnitTest_Result` tables.
///
/// EXTRACTED from the handler so its aliases can be asserted. A mutation replacing
/// `tm.ErrorDescription ErrDesc` with `NULL ErrDesc` is invisible to a test that only exercises the
/// shaper — the same gap that let a mutation survive on #271, where the helper was covered and the
/// wiring was not.
///
/// `ErrorDescription` / `ErrorAction` come from the TestMethod row itself and are what an ABORT writes;
/// the three `Fail*` subqueries only ever see a failed ASSERT.
pub fn result_query_sql(before_id: i64) -> String {
    format!(
        "SELECT tc.Name Class, tm.Name Method, tm.Status St, \
         (SELECT TOP 1 ta.Description FROM %UnitTest_Result.TestAssert ta WHERE ta.TestMethod=tm.ID AND ta.Status=0 ORDER BY ta.Counter) FailMsg, \
         (SELECT TOP 1 ta.Location FROM %UnitTest_Result.TestAssert ta WHERE ta.TestMethod=tm.ID AND ta.Status=0 ORDER BY ta.Counter) FailLoc, \
         (SELECT TOP 1 ta.Action FROM %UnitTest_Result.TestAssert ta WHERE ta.TestMethod=tm.ID AND ta.Status=0 ORDER BY ta.Counter) FailAct, \
         tm.ErrorDescription ErrDesc, tm.ErrorAction ErrAct \
         FROM %UnitTest_Result.TestMethod tm, %UnitTest_Result.TestCase tc, %UnitTest_Result.TestSuite ts \
         WHERE tm.TestCase=tc.ID AND tc.TestSuite=ts.ID AND ts.TestInstance > {before_id} ORDER BY tc.Name, tm.Name"
    )
}

/// Shape one row of the result-table query into a test-case object.
///
/// EXTRACTED so the wiring is testable, not just the helpers. On #271 a mutation removing the call
/// from `list_tools` survived precisely because every test called the helper directly — the function
/// was covered and the integration point was not. Here the integration point is "does the row shaping
/// actually consult ErrorDescription", which is the whole fix.
///
/// Returns the case object and whether it was a runtime error, so the caller counts without
/// re-deriving.
pub fn shape_method_row(r: &serde_json::Value) -> (serde_json::Value, bool) {
    let cls = r["Class"].as_str().unwrap_or("").to_string();
    let method = r["Method"].as_str().unwrap_or("").to_string();
    let is_passed = match &r["St"] {
        serde_json::Value::String(s) => s == "1",
        serde_json::Value::Number(n) => n.as_i64() == Some(1),
        _ => false,
    };
    let detail = failure_detail(
        r["FailMsg"].as_str(),
        r["FailLoc"].as_str(),
        r["FailAct"].as_str(),
        r["ErrDesc"].as_str(),
        r["ErrAct"].as_str(),
    );
    let to_json = |v: Option<String>| {
        v.map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null)
    };
    let runtime = !is_passed && detail.is_runtime_error();
    let tc = serde_json::json!({
        "name": method,
        "class_name": cls,
        // Stays "failed", NOT "error": inline_failed_tests selects on status=="failed", so
        // reclassifying would DROP aborts out of failed_tests entirely — the opposite of #273.
        "status": if is_passed { "passed" } else { "failed" },
        "duration_ms": null,
        "failure_message": to_json(detail.message),
        "failure_location": to_json(detail.location),
        "failure_assert": to_json(detail.assert),
        "failure_kind": detail.kind,
    });
    (tc, runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VERBATIM from #273 — the log line for `Ejercicio3.Tests.BO.WriteToCocina`, which is what
    /// `ErrorDescription` holds for that abort.
    const ABORT: &str = "ERROR #5002: ObjectScript error: <PROPERTY DOES NOT EXIST>\
         TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1 *Username,EnsLib.SQL.OutboundAdapter";

    // ── frame extraction ────────────────────────────────────────────────────────────────────

    #[test]
    fn the_frame_comes_out_of_the_real_abort_text() {
        assert_eq!(
            extract_frame(ABORT).as_deref(),
            Some("TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1")
        );
    }

    /// The `<SIGNAL>` immediately precedes the label with no space, so the scan must stop at `>`
    /// rather than swallowing it into the frame.
    #[test]
    fn the_signal_marker_is_not_swallowed_into_the_frame() {
        let f = extract_frame(ABORT).unwrap();
        assert!(!f.contains('>'), "{f}");
        assert!(!f.contains("EXIST"), "{f}");
        assert!(f.starts_with("TestWrite"), "{f}");
    }

    /// The trailing ` *Username,EnsLib…` must not be absorbed either — the frame ends at the space.
    #[test]
    fn the_trailing_symbol_list_is_not_part_of_the_frame() {
        let f = extract_frame(ABORT).unwrap();
        assert!(!f.contains('*'), "{f}");
        assert!(!f.contains(','), "{f}");
    }

    /// A library frame still extracts — the report measured 5 of 13 landing here
    /// (`RunOneTestCase+83^%UnitTest.Manager.1`). The `%` must survive in the routine name.
    #[test]
    fn a_percent_routine_frame_extracts_intact() {
        let t =
            "ERROR #5002: ObjectScript error: <UNDEFINED>RunOneTestCase+83^%UnitTest.Manager.1 *x";
        assert_eq!(
            extract_frame(t).as_deref(),
            Some("RunOneTestCase+83^%UnitTest.Manager.1")
        );
    }

    /// No frame at all → None, never an empty string or a fragment of the message.
    #[test]
    fn a_text_with_no_frame_yields_none() {
        assert_eq!(extract_frame("ERROR #5023: something went wrong"), None);
        assert_eq!(extract_frame(""), None);
        // a bare caret is not a frame
        assert_eq!(extract_frame("a ^ b"), None);
    }

    // ── precedence ──────────────────────────────────────────────────────────────────────────

    /// A failed assert WINS over an abort. When a test asserts and then aborts, the assertion is the
    /// specific actionable fact and the trap is often its consequence; reporting the trap would
    /// replace a named comparison with a stack frame.
    #[test]
    fn a_failed_assert_wins_over_an_abort() {
        let d = failure_detail(
            Some("ERROR #5023: expected 3, got 4"),
            Some("TestX+9^Pkg.Tests.1"),
            Some("AssertStatusOK"),
            Some(ABORT),
            Some("TestX"),
        );
        assert_eq!(d.kind, Some(KIND_ASSERT));
        assert_eq!(d.assert.as_deref(), Some("AssertStatusOK"));
        assert!(d.message.as_deref().unwrap().contains("#5023"), "{d:?}");
        assert!(!d.is_runtime_error());
    }

    /// The case the issue is about: no assert row, so the abort supplies everything.
    #[test]
    fn an_abort_with_no_assert_row_supplies_the_message_and_frame() {
        let d = failure_detail(
            None,
            None,
            None,
            Some(ABORT),
            Some("TestWriteToCocinaBasic"),
        );
        assert_eq!(d.kind, Some(KIND_RUNTIME_ERROR));
        assert!(d.is_runtime_error());
        assert!(
            d.message
                .as_deref()
                .unwrap()
                .contains("PROPERTY DOES NOT EXIST"),
            "the whole error text is the value: {d:?}"
        );
        assert_eq!(
            d.location.as_deref(),
            Some("TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1")
        );
    }

    /// `ErrorAction` is the METHOD name, not an assertion. Putting it in `failure_assert` — a field
    /// callers read as "which assertion failed" — would be a plausible-looking lie, so it stays out.
    #[test]
    fn the_error_action_is_never_reported_as_an_assert() {
        let d = failure_detail(
            None,
            None,
            None,
            Some(ABORT),
            Some("TestWriteToCocinaBasic"),
        );
        assert_eq!(d.assert, None, "an abort involved no assertion: {d:?}");
    }

    /// Empty strings are not values. IRIS returns "" rather than NULL on some driver paths, and an
    /// empty `FailMsg` must not shadow a populated `ErrorDescription`.
    #[test]
    fn an_empty_assert_message_does_not_shadow_the_abort_text() {
        let d = failure_detail(Some("   "), Some(""), Some(""), Some(ABORT), None);
        assert_eq!(d.kind, Some(KIND_RUNTIME_ERROR), "{d:?}");
        assert!(d.message.is_some(), "{d:?}");
    }

    /// Red with nothing recorded anywhere is a REAL state. It must claim no kind rather than be
    /// labelled an assert or a runtime error — the whole complaint in #273 is fields that assert
    /// something the data does not support.
    #[test]
    fn red_with_no_detail_at_all_claims_no_kind() {
        let d = failure_detail(None, None, None, None, None);
        assert_eq!(d.kind, None);
        assert_eq!(d.message, None);
        assert_eq!(d.location, None);
        assert_eq!(d.assert, None);
        assert!(!d.is_runtime_error(), "absent is not a runtime error");
    }

    // ── the wiring: the query must alias what the shaper reads ──────────────────────────────

    /// A MUTATION SURVIVED before this: replacing `tm.ErrorDescription ErrDesc` with `NULL ErrDesc`
    /// passed every test, because the SQL was a string literal in the handler that nothing inspected.
    /// A mis-aliased column is INVISIBLE — `r["ErrDesc"]` on a row lacking it is `Null`, identical to
    /// "this test did not abort" — so the whole feature would silently do nothing.
    #[test]
    fn the_query_aliases_every_key_the_shaper_reads() {
        let sql = result_query_sql(0);
        for key in ROW_KEYS {
            assert!(
                sql.contains(key),
                "the query does not alias '{key}', which shape_method_row reads — it would always \
                 be Null. SQL: {sql}"
            );
        }
    }

    /// Specifically the two the fix added, and from the TestMethod row rather than a subquery: the
    /// whole point is that an abort writes there and never creates a TestAssert row.
    #[test]
    fn the_abort_columns_come_from_the_test_method_row() {
        let sql = result_query_sql(0);
        assert!(
            sql.contains("tm.ErrorDescription ErrDesc"),
            "must read ErrorDescription off tm, not a TestAssert subquery: {sql}"
        );
        assert!(sql.contains("tm.ErrorAction ErrAct"), "{sql}");
    }

    /// The three assert subqueries must keep their `Status=0` filter — without it a PASSING assert
    /// would be reported as the failure, which is worse than reporting nothing.
    #[test]
    fn the_assert_subqueries_still_filter_on_a_failed_status() {
        let sql = result_query_sql(0);
        assert_eq!(
            sql.matches("ta.Status=0").count(),
            3,
            "all three assert subqueries must filter failed asserts: {sql}"
        );
    }

    /// The run boundary is interpolated — without it the query returns every historical run.
    #[test]
    fn the_run_boundary_is_interpolated() {
        assert!(result_query_sql(41).contains("TestInstance > 41"));
        assert!(
            !result_query_sql(41).contains("{before_id}"),
            "unsubstituted placeholder"
        );
    }

    // ── the wiring: shape_method_row on rows shaped like the real query ─────────────────────

    /// The row keys are exactly the SQL aliases (`Class`, `Method`, `St`, `FailMsg`, `FailLoc`,
    /// `FailAct`, `ErrDesc`, `ErrAct`). If the SELECT stops aliasing `ErrDesc`, this row stops
    /// carrying it and the assertion below fails — which is the wiring check.
    fn abort_row() -> serde_json::Value {
        serde_json::json!({
            "Class": "Ejercicio3.Tests.BO.WriteToCocina",
            "Method": "TestWriteToCocinaBasic",
            "St": 0,
            "FailMsg": null, "FailLoc": null, "FailAct": null,
            "ErrDesc": ABORT,
            "ErrAct": "TestWriteToCocinaBasic",
        })
    }

    /// The exact shape #273 reported as all-null now carries the message, the frame and the kind.
    #[test]
    fn an_abort_row_is_shaped_with_the_detail_the_issue_asked_for() {
        let (tc, is_runtime) = shape_method_row(&abort_row());
        assert!(is_runtime, "must be counted as a runtime error: {tc}");
        assert_eq!(tc["class_name"], "Ejercicio3.Tests.BO.WriteToCocina");
        assert_eq!(tc["name"], "TestWriteToCocinaBasic");
        assert_eq!(tc["failure_kind"], KIND_RUNTIME_ERROR);
        assert!(
            tc["failure_message"]
                .as_str()
                .unwrap()
                .contains("PROPERTY DOES NOT EXIST"),
            "{tc}"
        );
        assert_eq!(
            tc["failure_location"],
            "TestWriteToCocinaBasic+18^Ejercicio3.Tests.BO.WriteToCocina.1"
        );
        assert!(
            tc["failure_assert"].is_null(),
            "no assert was involved: {tc}"
        );
    }

    /// The status must stay "failed". `inline_failed_tests` selects on `status == "failed"`, so
    /// promoting an abort to "error" would DROP it out of `failed_tests` entirely — the opposite of
    /// what the issue asks for. This is the regression that a well-meaning "aborts are errors" change
    /// would introduce.
    #[test]
    fn an_abort_stays_status_failed_so_it_is_not_dropped_from_failed_tests() {
        let (tc, _) = shape_method_row(&abort_row());
        assert_eq!(
            tc["status"], "failed",
            "status must remain 'failed' or inline_failed_tests filters it out: {tc}"
        );
    }

    /// A passing row is untouched and is NOT counted as a runtime error, whatever ErrDesc holds.
    #[test]
    fn a_passing_row_is_never_a_runtime_error() {
        let mut row = abort_row();
        row["St"] = serde_json::json!(1);
        let (tc, is_runtime) = shape_method_row(&row);
        assert_eq!(tc["status"], "passed", "{tc}");
        assert!(!is_runtime, "a passing test is not a runtime error: {tc}");
    }

    /// `St` arrives as a number on one driver path and a numeric STRING on another — the same split
    /// `class_presence` documents for IsCompiled. Both must read as passed.
    #[test]
    fn the_status_column_is_read_on_both_driver_shapes() {
        for v in [serde_json::json!(1), serde_json::json!("1")] {
            let mut row = abort_row();
            row["St"] = v.clone();
            let (tc, _) = shape_method_row(&row);
            assert_eq!(tc["status"], "passed", "St={v} should read as passed");
        }
    }

    /// An assert row keeps behaving exactly as before — the regression guard. #273 notes 49 of the 74
    /// reds already had all three fields; none of them may change.
    #[test]
    fn an_assert_row_is_unchanged_by_the_abort_path() {
        let row = serde_json::json!({
            "Class": "Pkg.Tests.Foo", "Method": "TestBar", "St": 0,
            "FailMsg": "ERROR #5023: expected 3, got 4",
            "FailLoc": "TestBar+9^Pkg.Tests.Foo.1",
            "FailAct": "AssertStatusOK",
            "ErrDesc": null, "ErrAct": null,
        });
        let (tc, is_runtime) = shape_method_row(&row);
        assert!(
            !is_runtime,
            "an assert failure is not a runtime error: {tc}"
        );
        assert_eq!(tc["failure_kind"], KIND_ASSERT);
        assert_eq!(tc["failure_message"], "ERROR #5023: expected 3, got 4");
        assert_eq!(tc["failure_location"], "TestBar+9^Pkg.Tests.Foo.1");
        assert_eq!(tc["failure_assert"], "AssertStatusOK");
    }

    /// CONTROL: both kinds are reachable. If `failure_detail` ever became constant, the per-case
    /// tests would still pass individually; this says the two outcomes differ.
    #[test]
    fn both_kinds_are_reachable() {
        let a = failure_detail(Some("m"), None, Some("AssertTrue"), None, None);
        let r = failure_detail(None, None, None, Some(ABORT), None);
        assert_ne!(a.kind, r.kind);
        assert_eq!(a.kind, Some(KIND_ASSERT));
        assert_eq!(r.kind, Some(KIND_RUNTIME_ERROR));
    }
}
