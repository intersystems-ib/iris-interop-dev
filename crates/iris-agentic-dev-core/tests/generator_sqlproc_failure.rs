//! #362: a SqlProc that fails is not a script that produced no output.
//!
//! `execute_via_generator` is how almost every tool in this server runs ObjectScript — 66 call
//! sites. Step 3 reads the result of `SELECT <proc>()` with
//!
//! ```ignore
//! let query_body: serde_json::Value = query_resp.json().await.unwrap_or_default();
//! let output = query_body["result"]["content"][0]["result"].as_str().unwrap_or("")
//! ```
//!
//! A SqlProc that fails at runtime answers **HTTP 200** with the diagnosis in `status.errors` and
//! no `content` key at all, so that chain yields `Ok("")` and the error text is gone. Callers then
//! read the empty string as "the script produced no output" — `admin.rs` lets it decide a role
//! list, `doc.rs` reads it as "not under source control".
//!
//! Measured while filing the issue: a captured output of 3,600,000 chars returns fine and
//! 3,700,000 fails with `<MAXSTRING>`. So the behaviour is correct right up to IRIS's limit and
//! then reports *nothing found* for the one large document — the failure curve that passes every
//! test written against a normal-sized document.
//!
//! ## Why these are driven against a mock rather than asserted on the source
//!
//! The defect is the composition of `unwrap_or_default()` → a chain of indexes → `unwrap_or("")`,
//! and each step is locally reasonable. What matters is what the CALLER receives, and only driving
//! the function shows that. The legitimate empty output is here too, as its own test: it is the
//! case that breaks if the fix over-reaches, and today it is indistinguishable from the failure.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";

/// Everything the generator does before and after the `SELECT` under test: PUT the scratch class,
/// compile it clean, and accept the DELETE.
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
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/USER/action/compile"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"log": []}})),
        )
        .mount(&server)
        .await;
    server
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

/// The exact response a failing SqlProc returns, captured from a live instance: 200, the whole
/// diagnosis in `status.errors`, and `result` with NO `content` key.
fn sqlproc_failed() -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [{
            "error": "ERROR #5540: SQLCODE: -149 Message: SQL Function IRISDEVTMP.RUN_EXECUTE \
                      failed with error:  SQLCODE=-400,%msg=ERROR #5002: ObjectScript error: \
                      <MAXSTRING>zExecute+14^IrisDevTmp.Run.1",
            "code": 5540,
            "domain": "%ObjectErrors",
            "id": "SQLCode"
        }], "summary": "ERROR #5540: SQLCODE: -149"},
        "console": [],
        "result": {}
    })
}

#[tokio::test]
async fn a_sqlproc_that_fails_is_an_error_not_an_empty_output() {
    let server = scaffold().await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(sqlproc_failed()),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let r = c.execute_via_generator("Write 1", NS, &client).await;

    let e = match r {
        Ok(out) => panic!(
            "a failed SqlProc must not be reported as output; got Ok({out:?}) — an empty string \
             here is what 66 call sites read as 'the script produced no output'"
        ),
        Err(e) => e.to_string(),
    };
    // The caller needs the diagnosis, not just a failure: the SQLCODE is the whole actionable part.
    assert!(
        e.contains("SQLCODE") && e.contains("MAXSTRING"),
        "the error must carry IRIS's own text, which is the only thing that says WHY: {e}"
    );
}

#[tokio::test]
async fn a_script_that_writes_nothing_still_succeeds_with_an_empty_output() {
    let server = scaffold().await;
    // Measured on a live instance: a SqlProc returning "" gives an EMPTY `status.errors` and a
    // `content` array whose `result` is "". Structurally different from the failure above, which is
    // what makes this separable at all.
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": [], "summary": ""},
            "console": [],
            "result": {"content": [{"result": ""}]}
        })),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Set x=1", NS, &client)
        .await
        .expect("a script that writes nothing is a SUCCESS with no output, not a failure");
    assert_eq!(
        out, "",
        "the legitimate empty output must survive the fix — this is the case that breaks if the \
         refusal over-reaches"
    );
}

#[tokio::test]
async fn a_non_json_query_response_is_a_fault_not_an_empty_output() {
    let server = scaffold().await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_string("<html><body>gateway error</body></html>"),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let r = c.execute_via_generator("Write 1", NS, &client).await;
    assert!(
        r.is_err(),
        "a 200 whose body is not JSON is a proxy page, not an empty result: {r:?}"
    );
}

#[tokio::test]
async fn the_scratch_class_is_deleted_even_when_the_query_fails() {
    let server = scaffold().await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(sqlproc_failed()),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let _ = c.execute_via_generator("Write 1", NS, &client).await;

    // The delete used to be unskippable because it sat after the output read. Every error arm added
    // above returns EARLY, so without an explicit delete each failure leaks an IrisDevTmp.Run*
    // class into the namespace.
    let puts = counts(&server, "PUT", "/doc/IrisDevTmp.").await;
    let deletes = counts(&server, "DELETE", "/doc/IrisDevTmp.").await;
    assert!(
        puts > 0,
        "control: the scratch class must have been PUT at all"
    );
    assert_eq!(
        deletes, puts,
        "every scratch class PUT must be deleted, failure path included — {puts} put, {deletes} deleted"
    );
}

#[tokio::test]
async fn a_five_hundred_on_the_query_step_is_retried() {
    let server = scaffold().await;
    query_answers(
        &server,
        ResponseTemplate::new(500).set_body_string("upstream boom"),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let r = c.execute_via_generator("Write 1", NS, &client).await;
    assert!(
        r.is_err(),
        "three 500s in a row must end as an error: {r:?}"
    );
    let attempts = counts(&server, "POST", "/action/query").await;
    assert_eq!(
        attempts, 3,
        "the retry ladder is three attempts; a 5xx on the query step used to become Ok(\"\") and so \
         was never retried at all — got {attempts}"
    );
}

#[tokio::test]
async fn a_successful_query_is_not_retried() {
    // CONTROL for the test above: it must distinguish "retried because it failed" from "always
    // makes three attempts". Without this, a loop that always ran three times would pass.
    let server = scaffold().await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": [], "summary": ""},
            "result": {"content": [{"result": "ok"}]}
        })),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Write \"ok\"", NS, &client)
        .await
        .expect("a well-formed response must succeed");
    assert_eq!(out, "ok");
    assert_eq!(
        counts(&server, "POST", "/action/query").await,
        1,
        "a success must be one attempt, or the retry assertion above proves nothing"
    );
}
