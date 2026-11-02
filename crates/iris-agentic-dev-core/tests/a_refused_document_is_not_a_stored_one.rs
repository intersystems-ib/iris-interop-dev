//! #566: `iris_doc mode=put` reported `success: true` for a document Atelier refused and threw away.
//!
//! `do_write` decided the outcome from the HTTP status and `status.errors`. Neither can see this
//! class of refusal. Measured with `PUT /api/atelier/v1/USER/doc/{name}` against IRIS for Health
//! **2026.1 and 2025.3**, which answered byte-identically:
//!
//! | outcome | HTTP | `status.errors` | `result.status` |
//! |---|---|---|---|
//! | created | 201 | `[]` | `""` |
//! | updated | 200 | `[]` | `""` |
//! | refused, illegal header line (#16021) | 200 | `[]` | the error |
//! | refused, no class definition (#16022) | 200 | `[]` | the error |
//! | refused, class name is not the document name (#16023) | 201 | `[]` | the error |
//!
//! Two things that make the obvious readings wrong, and both are asserted below rather than
//! described: `status.errors` is EMPTY on every refusal, and the HTTP code is 200 for a successful
//! UPDATE and 201 for a REFUSED name mismatch. So a server answering 201 has not told you it
//! stored anything, and this is why the fix cannot be "check for a 201".
//!
//! The bodies in `recorded::*` are verbatim captures from 2026.1, so these tests speak the real
//! wire format rather than a paraphrase of it.

use iris_agentic_dev_core::elicitation::{CheckoutCache, ElicitationStore};
use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::doc::{
    classify_document_write, handle_iris_doc, DocumentWrite, IrisDocParams,
};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";

/// Verbatim PUT response bodies, captured from IRIS for Health 2026.1.
mod recorded {
    pub const CREATED: &str = r#"{"status":{"errors":[],"summary":""},"console":[],"result":{"name":"IIDRec1.cls","db":"USER","ts":"","upd":false,"cat":"CLS","status":"","enc":false,"flags":0,"content":[]}}"#;
    pub const UPDATED: &str = r#"{"status":{"errors":[],"summary":""},"console":[],"result":{"name":"IIDRec1.cls","db":"USER","ts":"","upd":false,"cat":"CLS","status":"","enc":false,"flags":0,"content":[]}}"#;
    pub const ILLEGAL_HEADER: &str = r#"{"status":{"errors":[],"summary":""},"console":[],"result":{"name":"IIDRec2.cls","db":"","ts":"","cat":"CLS","enc":false,"content":"","status":"ERROR #16021: Illegal Header Line: this is not objectscript at all"}}"#;
    pub const NO_CLASS_DEF: &str = r#"{"status":{"errors":[],"summary":""},"console":[],"result":{"name":"IIDRec3.mac","db":"","ts":"","cat":"RTN","enc":false,"content":"","status":"ERROR #16022: No Class Definition or ROUTINE header found"}}"#;
    pub const NAME_MISMATCH: &str = r#"{"status":{"errors":[],"summary":""},"console":[],"result":{"name":"IIDRec4.cls","db":"","ts":"","cat":"CLS","enc":false,"content":"","status":"ERROR #16023: Unable to get text of User.IIDRec4.cls"}}"#;

    pub const ALL_REFUSALS: [(&str, &str); 3] = [
        ("#16021 illegal header line", ILLEGAL_HEADER),
        ("#16022 no class definition", NO_CLASS_DEF),
        ("#16023 class name is not the document name", NAME_MISMATCH),
    ];
}

fn classify(body: &str) -> DocumentWrite {
    classify_document_write(&serde_json::from_str(body).expect("recorded body must be JSON"))
}

// ── the classifier, over the measured rows ───────────────────────────────────────────────────

#[test]
fn a_created_document_is_stored() {
    assert_eq!(classify(recorded::CREATED), DocumentWrite::Stored);
}

/// The row that forbids keying the decision on a 201: a successful UPDATE is a 200.
#[test]
fn an_updated_document_is_stored_even_though_it_answered_200() {
    assert_eq!(classify(recorded::UPDATED), DocumentWrite::Stored);
}

/// Every refusal, and each must name the server's own reason — not a generic sentence this
/// repo wrote, which would hide which of the three happened.
#[test]
fn every_measured_refusal_is_a_refusal_carrying_the_servers_reason() {
    let mut checked = 0;
    for (label, body) in recorded::ALL_REFUSALS {
        match classify(body) {
            DocumentWrite::Refused(reason) => {
                assert!(
                    reason.contains("ERROR #160"),
                    "{label}: the refusal must carry Atelier's own text, got {reason:?}"
                );
                checked += 1;
            }
            other => panic!(
                "{label} classified as {other:?}, so the document loss is reported as a success"
            ),
        }
    }
    assert_eq!(
        checked,
        recorded::ALL_REFUSALS.len(),
        "every row must be exercised"
    );
    println!("refusal rows asserted: {checked}");
}

/// The sharpest row: Atelier answered **201 Created** and stored nothing. A fix that trusted the
/// status code would pass every other test in this file and still lose the document.
#[test]
fn a_name_mismatch_is_a_refusal_although_atelier_answered_201_created() {
    match classify(recorded::NAME_MISMATCH) {
        DocumentWrite::Refused(reason) => assert!(reason.contains("#16023"), "{reason}"),
        other => panic!("a 201 that stored nothing was classified {other:?}"),
    }
}

/// The premise of the whole fix, asserted rather than asserted-about: the field the old code read
/// is empty on EVERY refusal, so it cannot discriminate. If a future Atelier starts populating it,
/// this is the test that says so.
#[test]
fn status_errors_is_empty_on_every_refusal_so_it_cannot_discriminate() {
    for (label, body) in recorded::ALL_REFUSALS {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let errs = v["status"]["errors"]
            .as_array()
            .expect("errors is an array");
        assert!(errs.is_empty(), "{label}: status.errors was {errs:?}");
    }
    // Positive control for the same reader: a populated status.errors IS seen.
    let populated = serde_json::json!({
        "status": {"errors": [{"error": "ERROR #5540: SQLCODE -99"}]},
        "result": {"status": ""}
    });
    match classify_document_write(&populated) {
        DocumentWrite::Refused(r) => assert!(r.contains("#5540"), "{r}"),
        other => panic!("a populated status.errors must still be a refusal, got {other:?}"),
    }
}

/// The third case. A body that reports neither an error nor a `result.status` has said nothing,
/// and "nothing was reported" is not "the write landed". Folding this into `Stored` is exactly
/// the defect; folding it into `Refused` would invent a refusal the server never made.
#[test]
fn a_body_that_reports_no_outcome_is_unreported_not_stored() {
    let silent = serde_json::json!({"status": {"errors": []}, "console": []});
    assert_eq!(classify_document_write(&silent), DocumentWrite::Unreported);
    let no_status_key = serde_json::json!({"status": {"errors": []}, "result": {"name": "X.cls"}});
    assert_eq!(
        classify_document_write(&no_status_key),
        DocumentWrite::Unreported
    );
}

/// Whitespace is not a reason. An Atelier that pads the field has still reported no error.
#[test]
fn a_blank_result_status_is_stored() {
    let padded = serde_json::json!({"status": {"errors": []}, "result": {"status": "   "}});
    assert_eq!(classify_document_write(&padded), DocumentWrite::Stored);
}

// ── the wiring: what the TOOL answers ────────────────────────────────────────────────────────

/// Drive a real `mode=put` through the real handler against a mocked Atelier.
///
/// The checkout cache is pre-marked so `write_with_scm` skips its pre-write source-control probe,
/// which would otherwise need a generator mocked. That probe is not what is under test here.
async fn put_through_handler(put_body: &str, status: u16) -> (Option<bool>, serde_json::Value) {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_string(put_body)
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;
    let iris = IrisConnection::new(
        server.uri(),
        NS,
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let store = ElicitationStore::new();
    let cache = CheckoutCache::new();
    cache.mark(NS, "IIDGhost.cls");
    let p: IrisDocParams = serde_json::from_value(serde_json::json!({
        "mode": "put", "name": "IIDGhost.cls",
        "content": "Class IIDSomethingElse Extends %RegisteredObject\n{\n}\n",
        "namespace": NS, "compile": false
    }))
    .expect("the params must deserialise");
    let r = handle_iris_doc(&iris, &client, p, &store, &cache)
        .await
        .expect("the tool must answer, not error out of the transport");
    let text = match &r.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    (
        r.is_error,
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}")),
    )
}

/// End to end: the tool must not report a write that did not happen, and must not hand back an
/// `open_uri` for a document that does not exist.
#[tokio::test]
async fn the_tool_refuses_a_document_atelier_threw_away() {
    let (is_error, payload) = put_through_handler(recorded::NAME_MISMATCH, 201).await;
    assert_eq!(
        payload["success"],
        serde_json::Value::Bool(false),
        "the tool reported success for a document that was never stored: {payload}"
    );
    assert_eq!(
        is_error,
        Some(true),
        "it must be flagged as an error: {payload}"
    );
    assert_eq!(payload["error_code"], "UPLOAD_FAILED", "{payload}");
    assert!(
        payload.get("open_uri").is_none(),
        "a document that was not stored must not come back with an open_uri: {payload}"
    );
}

/// The positive control the refusal above is worthless without: the identical path, with a body
/// Atelier accepted, still writes. Without this, "refuses everything" would pass.
#[tokio::test]
async fn the_tool_still_stores_a_document_atelier_accepted() {
    let (is_error, payload) = put_through_handler(recorded::CREATED, 201).await;
    assert_eq!(
        payload["success"],
        serde_json::Value::Bool(true),
        "a document Atelier accepted must still be stored: {payload}"
    );
    assert_ne!(is_error, Some(true), "{payload}");
}

/// The undetermined case, through the tool: it is reported as neither a write nor a document
/// rejection, and it names the read-back.
#[tokio::test]
async fn the_tool_reports_an_unconfirmed_write_as_unconfirmed() {
    let silent = r#"{"status":{"errors":[],"summary":""},"console":[]}"#;
    let (_is_error, payload) = put_through_handler(silent, 200).await;
    assert_eq!(
        payload["success"],
        serde_json::Value::Bool(false),
        "{payload}"
    );
    assert_eq!(payload["error_code"], "UPLOAD_UNCONFIRMED", "{payload}");
    let msg = payload["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("iris_doc(mode='get'"),
        "the refusal must name the read-back, got {msg:?}"
    );
}
