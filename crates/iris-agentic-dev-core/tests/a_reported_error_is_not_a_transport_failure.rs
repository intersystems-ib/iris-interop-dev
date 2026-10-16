//! #391 + #390: one causal chain, measured at the layer where each half lives.
//!
//! #391 — step 2 of `execute_via_generator_once` decided "did the scratch class compile?" from
//! `result.log` ALONE. It was the only reader of that key in the workspace, against nine sites that
//! consult `status.errors`, and one stray brace puts its whole diagnosis in `status.errors` with an
//! EMPTY log: `ERROR #5559: … non-matching {} or () characters …`. So a class that never parsed
//! read as a clean compile, and the failure surfaced one step later as "the generated SqlProc
//! failed" — naming the scaffolding this server generates rather than the brace the caller typed.
//!
//! #390 — that error then reached `iris_execute`, which could not tell "the transport failed" from
//! "IRIS ran the code and reported an error", routed it into the docker-exec fallback, and answered
//! `DOCKER_REQUIRED` with a hint saying to verify `/api/atelier` — for a call where `/api/atelier`
//! had answered **HTTP 200** with the diagnosis inside it.
//!
//! ## Why the fix is not a bail at step 2
//!
//! The obvious fix — read `status.errors` at step 2 and refuse there — was implemented by the issue
//! author, driven through the shipped binary, and MEASURED regressing the commoner input: with
//! `flags=cuk` a class can carry a reported compile error and still expose a callable `Execute()`,
//! so `Set x=` went from a good answer (`IRIS_RUNTIME_ERROR`, `<SYNTAX>`, `source_line`) to a bare
//! failure. The diagnosis is therefore RECORDED at step 2 with no control flow and SURFACED at
//! step 3, where whether `Execute()` ran is the authoritative test.
//! `a_recorded_compile_diagnosis_does_not_fail_a_call_whose_proc_ran` is that regression, kept out.
//!
//! The tool-level half (`iris_execute` is private) lives in `tools::reported_error_routing_tests`
//! inside `src/tools/mod.rs`.

use iris_agentic_dev_core::iris::connection::{
    atelier_status, iris_reported, DiscoverySource, IrisConnection,
};
use iris_agentic_dev_core::tools::compile_error_list;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";

/// The PUT and DELETE legs, which every test here shares. The COMPILE response is per-test — it is
/// the input under examination — so it is mounted by `compile_answers` rather than here.
async fn scaffold() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/atelier/v1/USER/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/atelier/v1/USER/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    server
}

async fn compile_answers(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/USER/action/compile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn query_answers(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/USER/action/query"))
        .respond_with(resp)
        .mount(server)
        .await;
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

async fn counts(server: &MockServer, verb: &str, needle: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path().contains(needle))
        .count()
}

/// IRIS's own words for a class that could not be parsed, and the one message in the whole chain
/// that names braces. `result.log` is EMPTY — that is the entire point of #391.
const PARSE_ERROR: &str = "ERROR #5559: The class definition for class 'IrisDevTmp.Run0' could \
     not be parsed correctly, possibly due to non-matching {} or () characters or a missing ; \
     character or non-matched /* */ blocks. Edit the class and correct the problem.";

fn compile_reported_a_parse_error() -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [{"error": PARSE_ERROR, "code": 5559, "domain": "%ObjectErrors"}],
                   "summary": "ERROR #5559"},
        "console": [],
        "result": {"log": []}
    })
}

fn compile_was_clean() -> serde_json::Value {
    serde_json::json!({"status": {"errors": [], "summary": ""}, "console": [], "result": {"log": []}})
}

/// What `SELECT IrisDevTmp.Run<id>_Execute()` answers when the class never compiled: HTTP 200, and
/// a stored-procedure-not-found in `status.errors`. It mentions neither braces nor #5559 — which is
/// why the step-2 diagnosis is the only thing that can name the cause.
fn proc_not_found() -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [{
            "error": "ERROR #5540: SQLCODE: -428 Message: Stored procedure \
                      'IrisDevTmp.Run0_Execute' not found",
            "code": 5540, "domain": "%ObjectErrors", "id": "SQLCode"
        }], "summary": "ERROR #5540: SQLCODE: -428"},
        "console": [],
        "result": {}
    })
}

fn query_returned(value: &str) -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [], "summary": ""},
        "console": [],
        "result": {"content": [{"result": value}]}
    })
}

async fn generator_err(server: &MockServer, code: &str) -> anyhow::Error {
    let c = conn(server);
    let client = reqwest::Client::new();
    match c.execute_via_generator(code, NS, &client).await {
        Ok(out) => panic!("expected an error, got Ok({out:?})"),
        Err(e) => e,
    }
}

// ── #391: the cause is the brace, not the SqlProc ────────────────────────────────────────────

#[tokio::test]
async fn a_class_that_did_not_parse_names_the_brace_not_the_sqlproc() {
    let server = scaffold().await;
    compile_answers(&server, compile_reported_a_parse_error()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;

    let msg = generator_err(&server, "Write \"a\" }").await.to_string();

    // Before the fix the whole message was `the generated SqlProc failed: ERROR #5540: SQLCODE:
    // -428 …` and carried neither token: the compile response was read for `result.log` only, and
    // that array is empty here.
    assert!(
        msg.contains("5559"),
        "the compile diagnosis IRIS put in status.errors must reach the caller: {msg}"
    );
    assert!(
        msg.contains("non-matching {} or ()"),
        "this is the one sentence in the whole chain that names braces: {msg}"
    );
    assert!(
        msg.contains("did not compile"),
        "the cause must be stated as a compile failure, not as a SqlProc failure: {msg}"
    );
}

/// The DISCRIMINATOR. The `#5559` text is byte-identical to the test above; only the KEY holding it
/// changes, from `status.errors` to `status.warnings`. A fix that string-searched the response body
/// — or read `status.summary`, or matched "5559" anywhere — passes the test above and fails here.
#[tokio::test]
async fn the_diagnosis_is_read_from_status_errors_and_not_from_anywhere_in_the_body() {
    let server = scaffold().await;
    compile_answers(
        &server,
        serde_json::json!({
            "status": {"warnings": [{"error": PARSE_ERROR}], "errors": [], "summary": PARSE_ERROR},
            "console": [],
            "result": {"log": []}
        }),
    )
    .await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;

    let msg = generator_err(&server, "Write \"a\" }").await.to_string();

    assert!(
        !msg.contains("5559") && !msg.contains("non-matching"),
        "a diagnosis under a key this code does not read must NOT appear — it did, so the \
         compile verdict is being taken from the body as text rather than from status.errors: {msg}"
    );
}

/// Tri-state, not an unconditional prefix: with `status.errors` EMPTY and the query still failing,
/// the message must be exactly what it has always been.
#[tokio::test]
async fn an_empty_status_errors_leaves_the_sqlproc_message_alone() {
    let server = scaffold().await;
    compile_answers(&server, compile_was_clean()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;

    let msg = generator_err(&server, "Write 1").await.to_string();

    assert!(
        msg.starts_with("the generated SqlProc failed: "),
        "a run with no compile diagnosis keeps today's wording byte-for-byte: {msg}"
    );
    assert!(
        !msg.contains("did not compile"),
        "nothing was reported about the compile, so nothing may be claimed about it: {msg}"
    );
}

/// `result.log` was the ONE source the old step 2 did read. Folding it into the shared reader must
/// not lose it — otherwise #391's fix would trade one blind spot for another.
#[tokio::test]
async fn a_log_only_diagnosis_still_reaches_the_caller() {
    let server = scaffold().await;
    compile_answers(
        &server,
        serde_json::json!({
            "status": {"errors": [], "summary": ""},
            "console": [],
            "result": {"log": [
                {"type": "info", "text": "Compiling class IrisDevTmp.Run0"},
                {"type": "ERROR", "text": "MPP5610 : Referenced macro not defined: 'ZzUndefined0'"}
            ]}
        }),
    )
    .await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;

    let msg = generator_err(&server, "Write $$$ZzUndefined0")
        .await
        .to_string();

    assert!(
        msg.contains("MPP5610") && msg.contains("did not compile"),
        "a diagnosis that arrives only in result.log must still name the cause: {msg}"
    );
    assert!(
        !msg.contains("Compiling class"),
        "a non-error log entry is not a diagnosis and must not be reported as one: {msg}"
    );
}

/// THE REGRESSION THE ISSUE AUTHOR MEASURED, kept out. `flags=cuk` can leave a callable method
/// behind on a class that reported a compile error, and the commoner input (`Set x=`) is exactly
/// that shape: a step-2 bail turns today's good answer into a bare failure. A diagnosis is a
/// hypothesis; `Execute()` running is the verdict.
#[tokio::test]
async fn a_recorded_compile_diagnosis_does_not_fail_a_call_whose_proc_ran() {
    let server = scaffold().await;
    compile_answers(&server, compile_reported_a_parse_error()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(query_returned("a")),
    )
    .await;

    let c = conn(&server);
    let client = reqwest::Client::new();
    let out = c
        .execute_via_generator("Set x=", NS, &client)
        .await
        .expect("a reported compile error whose Execute() still ran is NOT a failed call");
    assert_eq!(
        out, "a",
        "the output must survive untouched — the Rows arm discards the diagnosis on purpose"
    );
}

#[tokio::test]
async fn the_scratch_class_is_deleted_when_the_class_did_not_compile() {
    let server = scaffold().await;
    compile_answers(&server, compile_reported_a_parse_error()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;

    let _ = generator_err(&server, "Write \"a\" }").await;

    let puts = counts(&server, "PUT", "/doc/IrisDevTmp.").await;
    let deletes = counts(&server, "DELETE", "/doc/IrisDevTmp.").await;
    assert!(puts > 0, "control: the scratch class must have been PUT");
    assert_eq!(
        deletes, puts,
        "the compile-diagnosis path must not leak an IrisDevTmp.Run* class — {puts} put, \
         {deletes} deleted"
    );
}

// ── #390: the boundary's verdict is TYPED, so the tool layer need not re-read prose ──────────

/// The discriminant `iris_execute`'s fallback depends on, with its negative controls beside it.
/// Before #390 all three of these came back as an untyped `anyhow` whose only distinguishing
/// feature was a message prefix — which nothing read.
#[tokio::test]
async fn iris_reported_separates_an_iris_verdict_from_a_transport_fault() {
    // (a) IRIS answered 200 and reported its own error: a verdict about the code.
    let server = scaffold().await;
    compile_answers(&server, compile_was_clean()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(proc_not_found()),
    )
    .await;
    let e = generator_err(&server, "Write 1").await;
    let reported = iris_reported(&e).expect("an error IRIS itself reported must be downcastable");
    assert!(
        reported.text.contains("SQLCODE: -428"),
        "the typed error must carry IRIS's words: {}",
        reported.text
    );
    assert_eq!(
        reported.text,
        e.to_string(),
        "Display and `text` must be the same string, or a caller reading one gets less than the \
         caller reading the other"
    );
    assert!(
        atelier_status(&e).is_none(),
        "an IRIS-reported error is not an HTTP status error"
    );

    // (b) NEGATIVE CONTROL — a 200 whose body is not JSON is a proxy page, i.e. a transport
    // fault. It must stay UNTYPED, because for that the docker leg genuinely is a second
    // opinion and #390 must not remove the fallback wholesale.
    let server = scaffold().await;
    compile_answers(&server, compile_was_clean()).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_string("<html><body>gateway error</body></html>"),
    )
    .await;
    let e = generator_err(&server, "Write 1").await;
    assert!(
        iris_reported(&e).is_none(),
        "a non-JSON body is not IRIS reporting an error: {e}"
    );

    // (c) NEGATIVE CONTROL — a non-2xx is an Atelier HTTP error and keeps its own type.
    let server = scaffold().await;
    compile_answers(&server, compile_was_clean()).await;
    query_answers(&server, ResponseTemplate::new(404)).await;
    let e = generator_err(&server, "Write 1").await;
    assert!(
        iris_reported(&e).is_none(),
        "an HTTP 404 is not IRIS reporting an error: {e}"
    );
    assert!(
        atelier_status(&e).is_some(),
        "control: the 404 must still arrive as a typed AtelierHttpError, or (c) proves nothing"
    );
}

// ── #391 step 1: the shared reader covers all three sources IRIS uses ────────────────────────

#[test]
fn compile_error_list_reads_status_errors_and_the_log_and_the_console() {
    let body = serde_json::json!({
        "status": {"errors": [{"error": "ERROR #5475: Error compiling routine: Zz.One"}]},
        "result": {"log": [
            {"type": "error", "text": "MPP5610 : Referenced macro not defined: 'ZzA'"},
            {"type": "warning", "text": "a warning is not an error"},
            {"type": "ERROR", "message": "a log entry that spells its text `message`"},
            {"type": "ERROR", "other": "a log entry whose wording nobody here knows"}
        ]}
    });
    let console = vec!["ERROR: Zz.One.1(3) : MPP5611 : something else".to_string()];
    let errors = compile_error_list(&body, &console);

    assert_eq!(
        errors.len(),
        5,
        "1 status.errors + 3 log errors + 1 console line: {errors:#?}"
    );
    assert!(errors.iter().any(|e| e.contains("#5475")), "{errors:#?}");
    assert!(errors.iter().any(|e| e.contains("MPP5610")), "{errors:#?}");
    assert!(errors.iter().any(|e| e.contains("MPP5611")), "{errors:#?}");
    assert!(
        errors.iter().any(|e| e.contains("spells its text")),
        "`message` must be read as well as `text`: {errors:#?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("nobody here knows")),
        "an entry whose wording is unknown must arrive verbatim rather than be dropped: {errors:#?}"
    );
    assert!(
        !errors.iter().any(|e| e.contains("a warning is not")),
        "only type=error entries are errors: {errors:#?}"
    );
}

/// CONTROL for the test above: with no `result` key at all the list is unchanged, which is the
/// shape of every compile response in the tree and why extending the reader moved no expectation.
#[test]
fn a_response_with_no_result_log_is_read_exactly_as_before() {
    let body = serde_json::json!({"status": {"errors": [{"error": "ERROR #5475: boom"}]}});
    assert_eq!(compile_error_list(&body, &[]), vec!["ERROR #5475: boom"]);
    assert!(compile_error_list(&serde_json::json!({}), &[]).is_empty());
}
