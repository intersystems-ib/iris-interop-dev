//! #533: `iris_debug(action=error_logs)` answered
//!
//! ```json
//! {"success": true, "logs": [], "note": "IRIS error log is not accessible via Atelier REST
//!  SQL. Set IRIS_CONTAINER to enable docker exec access to the full error log."}
//! ```
//!
//! **without issuing a single request.** A caller reading `success: true, logs: []` concludes the
//! namespace logged no errors. Measured on a live 2026.2 stack (issue comment): `^ERRORS` in the
//! reporting namespace held **2 entries** at that moment, `container` was already set in
//! `.iris-agentic-dev.toml` — so the note named a remedy that was already in force — and
//! `iris_execute` reads `^ERRORS` fine over plain HTTP. The note was not merely useless; it was
//! false, and no note changes the verdict a model reads off `success` and `logs`.
//!
//! The repair is not a nicer empty list. It is three outcomes a caller can tell apart:
//!
//! | outcome | `success` | `log_read` | `logs` |
//! |---|---|---|---|
//! | the log was read and holds entries | `true`  | `true`  | the entries |
//! | the log was read and holds nothing | `true`  | `true`  | `[]`        |
//! | the log could not be read          | `false` | absent  | **absent**  |
//!
//! `logs` is absent — not `[]` — on the failure, because a defaulted read of a present-but-empty
//! array is exactly how a caller turns a failure back into a negative fact.
//!
//! ## The 2026.2 layout the walk has to be right about
//!
//! ```text
//! ^ERRORS(<$H day>, n, "*STACK", level, "V", "$ZE")   <- the error text
//! ^ERRORS(<$H day>, n, "*STACK", level, "L")          <- the frame lines
//! ```
//!
//! ## Why `success: true` needs the end marker, not a row count
//!
//! `execute_via_generator` returns `Ok("")` for a SqlProc that failed at runtime (#362). Zero
//! length-prefixed fields and zero entries are the same string, so "the walk found nothing" and
//! "the walk never ran" are indistinguishable by count. The generated code therefore writes an
//! `EL_END` terminator plus the total it counted, and only a reply carrying that terminator can
//! reach `success: true`.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::error_log::{
    build_error_log_code, parse_error_log_output, sql_error_log_outcome, ErrorLogRead,
    ERROR_LOG_END,
};
use iris_agentic_dev_core::tools::info::{handle_iris_debug, DebugParams};
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "APP";

// ─── the decode: three outcomes, never two ───────────────────────────────────────────────────

/// One entry's six length-prefixed fields: day, date, index, raw top node, `$ZE`, stack.
fn entry(day: &str, date: &str, n: &str, raw: &str, ze: &str, stack: &str) -> String {
    [day, date, n, raw, ze, stack]
        .iter()
        .map(|f| format!("{}:{}", f.chars().count(), f))
        .collect::<String>()
}

/// The terminator and the total, framed the way the generated code writes them. Built from the
/// const so the fixture cannot drift from the marker the walk actually emits.
fn end(total: usize) -> String {
    format!(
        "{}:{}{}:{}",
        ERROR_LOG_END.chars().count(),
        ERROR_LOG_END,
        total.to_string().chars().count(),
        total
    )
}

#[test]
fn an_empty_reply_is_a_failed_read_not_an_empty_log() {
    // The defect, at the layer it lives on. `Ok("")` is what a SqlProc that died at runtime
    // returns, and it must never render as "the log is empty".
    match parse_error_log_output("") {
        ErrorLogRead::Unreadable(why) => assert!(
            why.contains("EL_END"),
            "the reason must name the missing terminator, not describe the log: {why}"
        ),
        other => {
            panic!("an empty reply read nothing, so nothing is known about the log: {other:?}")
        }
    }
}

#[test]
fn a_truncated_reply_is_a_failed_read() {
    // Entries, then the output stops. Generator output can truncate mid-walk; without the
    // terminator this would read as a complete answer that happens to be short.
    let raw = entry(
        "66700",
        "2026-10-22",
        "1",
        "",
        "<UNDEFINED>zRun+3^Foo.1",
        "",
    );
    assert!(
        matches!(parse_error_log_output(&raw), ErrorLogRead::Unreadable(_)),
        "a walk with no terminator did not finish"
    );
}

#[test]
fn an_objectscript_abort_is_a_failed_read() {
    // A user without the privilege on ^ERRORS gets <PROTECT>. That is the issue's third
    // acceptance case and it must carry the reason, not an empty list.
    let raw = "ERROR: <PROTECT>zErrorLog+4^IrisDevTmp.Run1.1 ^ERRORS";
    match parse_error_log_output(raw) {
        ErrorLogRead::Unreadable(why) => assert!(
            why.contains("<PROTECT>"),
            "the reason must carry the abort: {why}"
        ),
        other => panic!("an abort is not a reading of the log: {other:?}"),
    }
}

#[test]
fn two_trailing_fields_that_are_not_the_terminator_are_a_failed_read() {
    // Found by mutation: deleting the marker COMPARISON and keeping only the field-count check
    // left every other test green. Every other truncation shape is caught downstream — a partial
    // entry by the modulus, a non-numeric trailing field by the parse — so this is the one input
    // on which the marker's VALUE is load-bearing: two fields that happen to have the
    // terminator's shape. `1:51:0` decodes to ["5", "0"], i.e. a walk that stopped after writing
    // the first two fields of an entry, and without the marker it reads as a completed walk over
    // an empty log.
    match parse_error_log_output("1:51:0") {
        ErrorLogRead::Unreadable(why) => assert!(
            why.contains("EL_END"),
            "the marker, not the field count, is what rejects this: {why}"
        ),
        other => panic!("two arbitrary fields are not a completed walk: {other:?}"),
    }
}

#[test]
fn a_completed_walk_over_an_empty_log_is_read_and_empty() {
    // The one way to legitimately report nothing: the terminator is present and the total is 0.
    assert_eq!(parse_error_log_output(&end(0)), ErrorLogRead::ReadAndEmpty);
}

#[test]
fn entries_come_back_whole_including_a_multi_line_stack() {
    // Length-prefixed framing, because a $ZE text and a stack hold `^`, `:` and line feeds as
    // ordinary characters. Splitting on a delimiter would have made two phantom entries here.
    let raw = format!(
        "{}{}",
        entry(
            "66700",
            "2026-10-22",
            "2",
            "top node",
            "<UNDEFINED>F2+1^OMCUSTL *X",
            "F2+1^OMCUSTL\nDO+6^OMMAIN",
        ),
        end(1)
    );
    match parse_error_log_output(&raw) {
        ErrorLogRead::Entries { entries, total } => {
            assert_eq!(total, 1, "the walk counted one entry");
            assert_eq!(entries.len(), 1, "and returned it");
            let e = &entries[0];
            assert_eq!(e.day, "66700");
            assert_eq!(e.date, "2026-10-22");
            assert_eq!(e.index, "2");
            assert_eq!(e.raw, "top node");
            assert_eq!(e.error, "<UNDEFINED>F2+1^OMCUSTL *X");
            assert_eq!(
                e.stack,
                vec!["F2+1^OMCUSTL".to_string(), "DO+6^OMMAIN".to_string()],
                "the stack is one field on the wire and a list to the caller"
            );
        }
        other => panic!("expected one entry: {other:?}"),
    }
}

#[test]
fn a_bounded_answer_reports_what_it_withheld() {
    // `limit` bounds what is written; the total is counted independently of it, so the caller
    // learns the log is longer than the page it received.
    let raw = format!(
        "{}{}",
        entry("66700", "2026-10-22", "9", "", "<LIST>", ""),
        end(7)
    );
    match parse_error_log_output(&raw) {
        ErrorLogRead::Entries { entries, total } => {
            assert_eq!((entries.len(), total), (1, 7));
        }
        other => panic!("expected a bounded page: {other:?}"),
    }
}

#[test]
fn a_partial_entry_is_a_failed_read() {
    // Five fields plus the terminator: the walk wrote half an entry. A default for the sixth
    // field would have produced a complete-looking entry with one empty member.
    let raw = format!("5:66700{}", end(1));
    assert!(
        matches!(parse_error_log_output(&raw), ErrorLogRead::Unreadable(_)),
        "a field count that is not a whole number of entries is a broken read"
    );
}

#[test]
fn more_entries_than_the_total_is_a_failed_read() {
    let raw = format!(
        "{}{}{}",
        entry("66700", "2026-10-22", "1", "", "a", ""),
        entry("66700", "2026-10-22", "2", "", "b", ""),
        end(1)
    );
    assert!(
        matches!(parse_error_log_output(&raw), ErrorLogRead::Unreadable(_)),
        "a reply carrying more entries than it counted is incoherent, not a short log"
    );
}

#[test]
fn a_total_above_zero_with_no_entries_is_a_failed_read() {
    // The walk said it found seven and wrote none. Reporting that as an empty log is the defect
    // one layer in; reporting it as a seven-entry log is an invention.
    assert!(
        matches!(parse_error_log_output(&end(7)), ErrorLogRead::Unreadable(_)),
        "counted seven, wrote none: the read did not deliver"
    );
}

// ─── the generated walk ──────────────────────────────────────────────────────────────────────

#[test]
fn the_walk_reads_errors_and_writes_the_terminator() {
    let code = build_error_log_code(20);
    assert!(
        code.contains("^ERRORS"),
        "the walk must address the error-log global: {code}"
    );
    assert!(
        code.contains("\"*STACK\""),
        "the 2026.2 layout keeps the $ZE text under *STACK: {code}"
    );
    assert!(
        code.contains("EL_END"),
        "without the terminator a truncated reply reads as a complete one: {code}"
    );
}

#[test]
fn the_walk_never_asks_for_zero_entries() {
    // A limit of 0 would make "counted some, wrote none" reachable through a legitimate
    // request, and that shape is reported as a failed read above.
    assert!(
        build_error_log_code(0).contains("Set lim = 1"),
        "a zero limit is clamped to one: {}",
        build_error_log_code(0)
    );
}

// ─── the sibling: the same shape over SQL ────────────────────────────────────────────────────

#[test]
fn an_absent_error_table_is_a_failed_read_not_an_empty_one() {
    // `debug_capture_packet` and `debug_get_error_logs` both answered
    // `{"success": true, "errors"/"logs": [], "note": "%SYSTEM.Error not available on this IRIS
    // edition"}` when the SELECT failed. Same bug shape, two more call sites.
    for msg in [
        "ERROR #5540: SQLCODE: -30 Message: Table '%SYSTEM.Error' not found",
        "Table 'SQLUser.Error' not found",
    ] {
        match sql_error_log_outcome(msg) {
            ErrorLogRead::Unreadable(why) => assert!(
                why.contains("%SYSTEM.Error"),
                "the reason must name the table that could not be read: {why}"
            ),
            other => panic!("a failed SELECT is not an empty log ({msg}): {other:?}"),
        }
    }
    // And any other SQL failure is equally a failed read, carrying its own message.
    match sql_error_log_outcome("<PROTECT> *%SYSTEM.Error") {
        ErrorLogRead::Unreadable(why) => assert!(why.contains("<PROTECT>"), "{why}"),
        other => panic!("expected a failed read: {other:?}"),
    }
}

// ─── the whole tool, driven ──────────────────────────────────────────────────────────────────

fn payload<T: serde::Serialize>(r: &T) -> serde_json::Value {
    let v = serde_json::to_value(r).expect("the result must serialise");
    let text = v["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {v}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content was not JSON ({e}): {text}"))
}

fn params() -> DebugParams {
    serde_json::from_value(serde_json::json!({"action": "error_logs", "namespace": NS}))
        .expect("DebugParams must deserialise from this shape")
}

fn conn(server: &MockServer) -> IrisConnection {
    IrisConnection::new(
        server.uri(),
        NS,
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    )
}

/// Everything `execute_via_generator` touches except the SELECT that carries the output.
async fn generator(server: &MockServer) {
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/atelier/v1/APP/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/atelier/v1/APP/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/APP/action/compile"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"log": []}})),
        )
        .mount(server)
        .await;
}

/// Mount the SELECT that returns the walk's output.
async fn walk_output(server: &MockServer, out: &str) {
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/APP/action/query"))
        .and(body_string_contains("_Execute"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"result": {"content": [{"result": out}]}})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_tool_actually_reads_something() {
    // The headline defect: the old arm returned `success: true, logs: []` having issued no
    // request at all. Nothing is mounted, so every request fails — and a handler that asks
    // nothing cannot be told from one that asked and was refused unless we count the requests.
    let server = MockServer::start().await;
    let c = conn(&server);
    let v = payload(
        &handle_iris_debug(&c, &reqwest::Client::new(), params())
            .await
            .expect("the handler returns Ok with an error payload, not an Err"),
    );

    let seen = server
        .received_requests()
        .await
        .expect("the mock server records requests");
    assert!(
        !seen.is_empty(),
        "error_logs must read the log before answering about it; it issued no request: {v}"
    );
    assert_ne!(
        v["success"], true,
        "a failed read must not claim success: {v}"
    );
    assert!(
        v["logs"].is_null(),
        "a failed read must carry no `logs` key — a present `[]` is the negative fact this \
         refuses: {v}"
    );
    assert_eq!(v["error_code"], "ERROR_LOG_UNAVAILABLE", "{v}");
}

#[tokio::test]
async fn an_empty_log_says_it_was_read() {
    let server = MockServer::start().await;
    generator(&server).await;
    walk_output(&server, &end(0)).await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_debug(&c, &reqwest::Client::new(), params())
            .await
            .unwrap(),
    );
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(
        v["log_read"], true,
        "`log_read` is what lets a caller tell this from the refusal above: {v}"
    );
    assert_eq!(v["logs"], serde_json::json!([]), "{v}");
    assert_eq!(v["total_count"], 0, "{v}");
}

#[tokio::test]
async fn the_entries_reach_the_caller() {
    let server = MockServer::start().await;
    generator(&server).await;
    let out = format!(
        "{}{}",
        entry(
            "66700",
            "2026-10-22",
            "1",
            "",
            "<UNDEFINED>F2+1^OMCUSTL *X",
            "F2+1^OMCUSTL",
        ),
        end(2)
    );
    walk_output(&server, &out).await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_debug(&c, &reqwest::Client::new(), params())
            .await
            .unwrap(),
    );
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(v["log_read"], true, "{v}");
    assert_eq!(v["logs"].as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(v["logs"][0]["error"], "<UNDEFINED>F2+1^OMCUSTL *X", "{v}");
    assert_eq!(v["total_count"], 2, "{v}");
    assert_eq!(
        v["truncated"], true,
        "two counted, one returned: the caller must learn it got a page: {v}"
    );
}

#[tokio::test]
async fn a_walk_that_aborted_is_not_an_empty_log() {
    let server = MockServer::start().await;
    generator(&server).await;
    walk_output(&server, "ERROR: <PROTECT>zRun+4^IrisDevTmp.Run1.1 ^ERRORS").await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_debug(&c, &reqwest::Client::new(), params())
            .await
            .unwrap(),
    );
    assert_eq!(v["error_code"], "ERROR_LOG_UNAVAILABLE", "{v}");
    assert_ne!(v["success"], true, "{v}");
    assert!(v["logs"].is_null(), "{v}");
    assert!(
        v["error"].as_str().unwrap_or("").contains("<PROTECT>"),
        "the refusal must name what stopped the read: {v}"
    );
}

#[tokio::test]
async fn the_false_note_is_gone() {
    // The note claimed the log "is not accessible via Atelier REST SQL" and that IRIS_CONTAINER
    // was the way in. Measured: `iris_execute` reads ^ERRORS over plain HTTP, and the reporting
    // instance already had `container` set. Neither half was true.
    let server = MockServer::start().await;
    generator(&server).await;
    walk_output(&server, &end(0)).await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_debug(&c, &reqwest::Client::new(), params())
            .await
            .unwrap(),
    );
    let whole = v.to_string();
    assert!(
        !whole.contains("IRIS_CONTAINER"),
        "docker exec is not required to read ^ERRORS: {whole}"
    );
    assert!(
        !whole.contains("not accessible via Atelier"),
        "the log IS reachable over Atelier; the claim was false: {whole}"
    );
}
