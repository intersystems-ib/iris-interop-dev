//! A 200 whose body does not parse is NOT a search that found nothing.
//!
//! Both `iris_search` legs read the response with `resp.json().await.unwrap_or_default()`, which
//! yields `Value::Null` on an unparseable body. Every step after that treats Null as "finished with
//! no hits":
//!
//! 1. `Null["result"]["workId"].is_null()` is TRUE, so the sync leg takes the "not async" path and
//!    the poll leg concludes the search completed rather than continuing or failing;
//! 2. `flatten_results(&Null)` looks for `result` then `result.content` as arrays, finds neither, and
//!    ends in its own `unwrap_or_default()` → empty;
//! 3. `parse_search_results` reports `success: true, total_found: 0`.
//!
//! So the caller received a confident "no matches" for a search this server cannot show was ever
//! performed. #106 hardened the REFUSAL path in both of these functions — its comments sit directly
//! above the arms this fixes — and left the parse path, which is the sibling asymmetry CLAUDE.md
//! warns about and the same shape as #360 in `iris_production_diff`.
//!
//! ## Driven, not asserted on the source
//!
//! This is the point of using wiremock here. The defect lives in the COMPOSITION of three functions
//! and is invisible in any one of them: each `unwrap_or_default()` is locally reasonable. A
//! source-level guard would have to name all three and would still not prove what a caller receives.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::log_store::LogStore;
use iris_agentic_dev_core::tools::search::{handle_iris_search, SearchParams};
use std::sync::{Arc, Mutex};

/// The tool's JSON payload, without naming any `rmcp` path (not a dev-dependency here).
fn payload<T: serde::Serialize>(r: &T) -> serde_json::Value {
    let v = serde_json::to_value(r).expect("the result must serialise");
    // The payload is the text of the first content block, itself JSON.
    let text = v["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {v}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content was not JSON ({e}): {text}"))
}

fn params(query: &str) -> SearchParams {
    serde_json::from_value(serde_json::json!({
        "query": query,
        "documents": ["*.cls"],
        "namespace": "APP",
    }))
    .expect("SearchParams must deserialise from this shape")
}

/// A server that answers every request 200 with a body that is not JSON.
async fn unparseable_200() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string("<html><body>gateway error</body></html>"),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn an_unparseable_two_hundred_is_reported_as_a_fault_not_as_zero_hits() {
    let server = unparseable_200().await;
    let conn = IrisConnection::new(
        server.uri(),
        "APP",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let store = Arc::new(Mutex::new(LogStore::new(50, 600)));

    let r = handle_iris_search(&conn, &client, params("Patient"), store)
        .await
        .expect("the handler returns Ok with an error payload, not an Err");
    let v = payload(&r);

    assert_eq!(
        v["error_code"], "PARSE_ERROR",
        "an unparseable 200 must be a fault, not a result: {v}"
    );
    // The two properties that made the old behaviour dangerous.
    assert_ne!(v["success"], true, "it must not claim success: {v}");
    assert!(
        v["total_found"].is_null(),
        "it must not report a hit count at all — 0 here is the confident wrong answer: {v}"
    );
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("not") && (msg.contains("zero") || msg.contains("matches")),
        "the message must deny that this means zero matches, since that is what a caller would \
         otherwise assume: {msg}"
    );
}

#[tokio::test]
async fn the_harness_can_observe_a_real_result_so_the_test_above_is_not_vacuous() {
    // POSITIVE CONTROL. Without this, `error_code == PARSE_ERROR` could be produced by any failure
    // on any path — a connection refused, a missing scope, a panic in the harness — and the test
    // would pass while proving nothing about the parse. Here the SAME handler, against a server
    // that answers well-formed JSON with no matches, must produce the ordinary empty result.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"result": {"content": []}})),
        )
        .mount(&server)
        .await;
    let conn = IrisConnection::new(
        server.uri(),
        "APP",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let store = Arc::new(Mutex::new(LogStore::new(50, 600)));

    let r = handle_iris_search(&conn, &client, params("Patient"), store)
        .await
        .expect("handler must return Ok");
    let v = payload(&r);
    assert_eq!(
        v["success"], true,
        "a well-formed empty answer IS a successful search with no matches: {v}"
    );
    assert_eq!(v["total_found"], 0, "{v}");
    assert!(
        v["error_code"].is_null(),
        "a genuine zero-match search must carry no error code: {v}"
    );
}

// ── the ASYNC POLL leg ────────────────────────────────────────────────────────────
//
// Added because a mutation reverting the poll leg to `unwrap_or_default()` SURVIVED the two tests
// above: they drive the sync leg only, and once that leg refuses an unparseable body it returns
// immediately, so the poll code is never reached. A surviving mutant naming a missing assertion,
// exactly as CLAUDE.md describes.
//
// The two legs share the path `/action/search` and differ only by a `workId` query parameter, so the
// stages are separated by matching on its presence.

/// Sync answer that hands back a workId, so the handler proceeds to polling.
async fn async_handoff_then(poll_body: wiremock::ResponseTemplate) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    // Stage 1: no workId in the request -> this is the sync leg. Answer with a workId.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::query_param_is_missing("workId"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"result": {"workId": "W1"}})),
        )
        .mount(&server)
        .await;
    // Stage 2: the poll.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::query_param("workId", "W1"))
        .respond_with(poll_body)
        .mount(&server)
        .await;
    server
}

async fn run_against(server: &wiremock::MockServer) -> serde_json::Value {
    let conn = IrisConnection::new(
        server.uri(),
        "APP",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let store = Arc::new(Mutex::new(LogStore::new(50, 600)));
    let r = handle_iris_search(&conn, &client, params("Patient"), store)
        .await
        .expect("handler must return Ok");
    payload(&r)
}

#[tokio::test]
async fn an_unparseable_poll_response_is_a_fault_not_a_finished_search() {
    // The poll leg's failure mode is subtler than the sync leg's: a Null body makes
    // `body["result"]["workId"].is_null()` TRUE, which the loop reads as "the work is done" rather
    // than "still pending" or "this broke" — so it returned zero results and stopped polling.
    let server = async_handoff_then(
        wiremock::ResponseTemplate::new(200).set_body_string("<html>gateway error</html>"),
    )
    .await;
    let v = run_against(&server).await;
    assert_eq!(
        v["error_code"], "PARSE_ERROR",
        "an unparseable poll body must be a fault: {v}"
    );
    assert_ne!(v["success"], true, "it must not claim success: {v}");
    assert!(
        v["total_found"].is_null(),
        "it must not report a hit count — 0 here would claim the search completed: {v}"
    );
}

#[tokio::test]
async fn a_well_formed_poll_result_still_completes_the_search() {
    // POSITIVE CONTROL for the poll leg specifically. Without it, the test above could pass because
    // the handoff itself broke — never reaching the poll at all — and PARSE_ERROR would be coming
    // from the wrong place entirely.
    let server = async_handoff_then(
        wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "result": {"content": [{"doc": "MyApp.Patient.cls", "matches": [{"line": 12, "text": "Patient"}]}]}
        })),
    )
    .await;
    let v = run_against(&server).await;
    assert_eq!(
        v["success"], true,
        "a well-formed poll answer must complete the search: {v}"
    );
    assert_eq!(
        v["total_found"], 1,
        "and it must carry the match the poll returned, which proves the POLL leg ran: {v}"
    );
    assert!(v["error_code"].is_null(), "{v}");
}
