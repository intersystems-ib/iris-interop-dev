//! #360: a comparison verdict must never be rendered from a side that could not be read.
//!
//! `iris_production_diff` compares the RUNNING item set (SQL over `Ens_Config.Item`) with the
//! COMMITTED one (Atelier `GET /doc/<production>.cls`). #153 hardened the committed side's error
//! arms and left two ways to reach an empty set through success:
//!
//! 1. the committed side's SUCCESS arm read the body with `resp.json().await.unwrap_or_default()`,
//!    so a 200 whose body does not parse became `Value::Null` → `doc_content_to_string` → `""` →
//!    zero items, and the diff reported EVERY running item as `added`;
//! 2. the current side never had the guard at all: `result.content` missing or not an array became
//!    an empty item set, and the diff reported EVERY committed item as `removed`.
//!
//! Neither is a quiet wrong answer. `in_sync` is `changes.is_empty()`, so each renders a maximally
//! alarming diff under `success: true` — indistinguishable from real, catastrophic drift, which is
//! what makes someone act on it.
//!
//! ## Driven, not asserted on the source
//!
//! Both defects live in the COMPOSITION of functions that are each locally reasonable
//! (`unwrap_or_default` → `as_array()` → a loop over zero rows), so a source-level guard would name
//! constructs rather than behaviour and still not say what a caller receives. These drive the whole
//! tool against a mock Atelier and read the envelope.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::interop::{handle_iris_production_diff, ProductionDiffParams};
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "APP";
const PROD: &str = "Demo.Prod";

/// The tool's JSON payload, without naming any `rmcp` path (not a dev-dependency here).
fn payload<T: serde::Serialize>(r: &T) -> serde_json::Value {
    let v = serde_json::to_value(r).expect("the result must serialise");
    let text = v["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {v}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content was not JSON ({e}): {text}"))
}

fn params() -> ProductionDiffParams {
    serde_json::from_value(serde_json::json!({ "production": PROD, "namespace": NS }))
        .expect("ProductionDiffParams must deserialise from this shape")
}

/// A production class source carrying two items, as Atelier returns it: one string per line.
fn committed_source() -> serde_json::Value {
    serde_json::json!({"result": {"content": [
        "Class Demo.Prod Extends Ens.Production",
        "{",
        "XData ProductionDefinition",
        "{",
        "<Production Name=\"Demo.Prod\">",
        "  <Item Name=\"FileIn\" ClassName=\"Demo.BS.FileIn\" Enabled=\"true\"/>",
        "  <Item Name=\"FileOut\" ClassName=\"Demo.BO.FileOut\" Enabled=\"true\"/>",
        "</Production>",
        "}",
        "}",
    ]}})
}

/// One running item, so a well-formed run has something to disagree about.
fn current_rows() -> serde_json::Value {
    serde_json::json!({"result": {"content": [
        {"Name": "FileIn", "ClassName": "Demo.BS.FileIn", "Category": "", "Enabled": 1},
    ]}})
}

/// Everything `iris_production_diff` touches before the two sides being tested.
///
/// The two `execute_via_generator` probes (source-control status, then `%ExistsId`) both read their
/// output from the same `SELECT IrisDevTmp.Run<uuid>_Execute()`, and the id is random, so they
/// cannot be told apart by request. One constant output satisfies both: it is not `IN_SCM`, so the
/// baseline is reported as `class_definition`, and it is not `0`, so the production exists.
async fn atelier() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/atelier/v1/APP/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/atelier/v1/APP/doc/IrisDevTmp\..*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/APP/action/compile"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {"log": []}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/APP/action/query"))
        .and(body_string_contains("_Execute"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"result": {"content": [{"result": "1"}]}})),
        )
        .mount(&server)
        .await;
    server
}

/// Mount the running-item SELECT with a chosen body.
async fn current_side(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/APP/action/query"))
        .and(body_string_contains("Ens_Config.Item"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Mount the committed-source GET with a chosen response.
async fn committed_side(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/api/atelier/v1/APP/doc/Demo.Prod.cls"))
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

#[tokio::test]
async fn an_unparseable_committed_body_is_a_fault_not_an_empty_baseline() {
    let server = atelier().await;
    current_side(&server, current_rows()).await;
    committed_side(
        &server,
        ResponseTemplate::new(200).set_body_string("<html><body>gateway error</body></html>"),
    )
    .await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_production_diff(Some(&c), &params())
            .await
            .expect("the handler returns Ok with an error payload, not an Err"),
    );

    assert_eq!(
        v["error_code"], "BASELINE_UNAVAILABLE",
        "a 200 whose body does not parse is a failed read of the baseline: {v}"
    );
    // The two properties that made the old behaviour dangerous, asserted separately: it must not
    // claim success, and it must not report a diff at all.
    assert_ne!(v["success"], true, "it must not claim success: {v}");
    assert!(
        v["changes"].is_null(),
        "it must not report changes — `every running item added` is the confident wrong answer \
         this refuses: {v}"
    );
    // The CAUSE must be the one that happened. Both committed-side guards refuse, so the code
    // alone cannot distinguish them: a mutation that deletes the parse guard leaves the
    // no-source-lines guard to catch `Value::Null` and every assertion above still passes. What
    // changes is what the caller is told — "the body could not be parsed" is a fact about the
    // READ, while "no source lines" is a fact about the DOCUMENT, and only one of them is true
    // here. Pinning the message is what makes the guard load-bearing.
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("could not be parsed"),
        "the refusal must name the parse failure, not describe the document: {msg}"
    );
}

#[tokio::test]
async fn a_committed_body_with_no_content_array_is_also_a_fault() {
    let server = atelier().await;
    current_side(&server, current_rows()).await;
    // Valid JSON, 200, but not the shape a document read returns. The production class was already
    // proved to exist by the `%ExistsId` probe two calls earlier, so this is a read that failed,
    // not a class with no source.
    committed_side(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": {}})),
    )
    .await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_production_diff(Some(&c), &params())
            .await
            .unwrap(),
    );
    assert_eq!(
        v["error_code"], "BASELINE_UNAVAILABLE",
        "a 200 carrying no source lines is not an empty production: {v}"
    );
    assert!(v["changes"].is_null(), "no diff may be reported: {v}");
    // The mirror of the assertion in the test above: this body PARSED, so blaming the parse
    // would be a wrong diagnosis. Together the two pin which guard handled which failure.
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("no source lines"),
        "the refusal must say the document arrived without source lines: {msg}"
    );
    assert!(
        !msg.contains("could not be parsed"),
        "this body is valid JSON — reporting a parse failure would misdescribe it: {msg}"
    );
}

#[tokio::test]
async fn a_current_set_that_could_not_be_read_is_a_fault_not_an_empty_production() {
    let server = atelier().await;
    // The sibling #153 never hardened: `result.content` absent, which used to become an empty
    // running set and report every committed item as `removed`.
    current_side(&server, serde_json::json!({"result": {}})).await;
    committed_side(
        &server,
        ResponseTemplate::new(200).set_body_json(committed_source()),
    )
    .await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_production_diff(Some(&c), &params())
            .await
            .unwrap(),
    );
    assert_eq!(
        v["error_code"], "CURRENT_UNAVAILABLE",
        "an unreadable running set is not a production with no items: {v}"
    );
    assert!(
        v["changes"].is_null(),
        "it must not report every committed item as removed: {v}"
    );
}

#[tokio::test]
async fn a_well_formed_diff_still_reports_its_changes_and_the_size_of_both_sides() {
    let server = atelier().await;
    current_side(&server, current_rows()).await;
    committed_side(
        &server,
        ResponseTemplate::new(200).set_body_json(committed_source()),
    )
    .await;

    let c = conn(&server);
    let v = payload(
        &handle_iris_production_diff(Some(&c), &params())
            .await
            .unwrap(),
    );

    // POSITIVE CONTROL: this harness can drive the tool to a real verdict, so the three refusals
    // above are the tool refusing rather than the mocks failing to answer.
    assert_eq!(v["success"], true, "a well-formed run must succeed: {v}");
    assert_eq!(
        v["in_sync"], false,
        "one committed item is not running: {v}"
    );
    let changes = v["changes"].as_array().expect("changes must be an array");
    assert_eq!(changes.len(), 1, "exactly FileOut is missing: {v}");
    assert_eq!(changes[0]["item_name"], "FileOut");
    assert_eq!(changes[0]["status"], "removed");
    // #360: a verdict is not interpretable without the size of both sides. A diff claiming every
    // item changed reads identically to real drift unless the caller can see the denominators.
    assert_eq!(
        v["committed_item_count"], 2,
        "both committed items counted: {v}"
    );
    assert_eq!(
        v["current_item_count"], 1,
        "the one running item counted: {v}"
    );
}
