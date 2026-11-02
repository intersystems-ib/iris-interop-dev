//! #534, the wiring: `iris_debug(action=map_int)` reads the routine itself, and says when the
//! routine is genuinely absent.
//!
//! #564 classified the frame and did the arithmetic, but only against text the CALLER supplied.
//! A frame naming a routine that does not exist came back `INT_SOURCE_REQUIRED` — true, and
//! never an answer about the routine. These tests drive the real handler against a mocked
//! Atelier, which is the layer the pure-function tests in
//! `an_int_line_maps_back_to_its_mac_line.rs` cannot reach.
//!
//! ## Why the absence probe is not just "HEAD said 404"
//!
//! The 404 goes through `classify_404`, so `Absent` is only claimed on the arm where the
//! namespace was CONFIRMED reachable. #102 measured a document that provably exists reported
//! absent both for a namespace that does not exist and for a wrong password, and a wrong
//! `IRIS_WEB_PREFIX` 404s everything — so a bare 404 is not evidence of absence. The
//! `…_is_not_reported_as_a_missing_routine` test below is that distinction, and it is the one
//! that would regress silently.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::info::{handle_iris_debug, DebugParams};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";
const FRAME: &str = "<UNDEFINED>BOOM+2^IIDFrame *zzUndefinedVar";

fn int_lines() -> Vec<&'static str> {
    vec![
        "ROUTINE IIDFrame [Type=INT,Generated]",
        " ; comment on MAC line 2",
        " quit",
        " ;",
        "MAIN ; label MAIN is on MAC line 5",
        " new a",
        " set a=1",
        " set a=a+1",
        " quit a",
        "BOOM ; label BOOM is on MAC line 11",
        " new b",
        " set b=zzUndefinedVar",
        " quit b",
    ]
}

fn mac_lines() -> Vec<&'static str> {
    vec![
        "ROUTINE IIDFrame [Type=MAC]",
        " ; comment on MAC line 2",
        " quit",
        " ;",
        "MAIN ; label MAIN is on MAC line 5",
        " new a",
        " set a=1",
        "",
        " set a=a+1",
        " quit a",
        "BOOM ; label BOOM is on MAC line 11",
        " new b",
        " set b=zzUndefinedVar",
        " quit b",
    ]
}

/// The Atelier root descriptor, which `classify_404` reads to decide whether the NAMESPACE is
/// there. Answering it with a namespace list is what makes a 404 on the document mean "that
/// document is missing" rather than "nothing here is reachable".
async fn mount_root(server: &MockServer, namespaces: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/atelier/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"result": {"content": {"namespaces": namespaces}}}),
            ),
        )
        .mount(server)
        .await;
}

async fn doc_get(server: &MockServer, doc: &str, lines: Vec<&str>) {
    Mock::given(method("GET"))
        .and(path(format!("/api/atelier/v1/{NS}/doc/{doc}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"result": {"content": lines}})),
        )
        .mount(server)
        .await;
}

async fn call(server: &MockServer) -> (Option<bool>, serde_json::Value) {
    let iris = IrisConnection::new(
        server.uri(),
        NS,
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let client = reqwest::Client::new();
    let p: DebugParams = serde_json::from_value(serde_json::json!({
        "action": "map_int", "error_string": FRAME, "namespace": NS
    }))
    .expect("the params must deserialise");
    let r = handle_iris_debug(&iris, &client, p)
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

// ── the routine is there ─────────────────────────────────────────────────────────────────────

/// The whole point of the issue: the caller passes a frame and nothing else, and gets back the
/// exact INT line AND the MAC line it came from. Ground truth, from a real error raised on
/// 2026.1: `BOOM` is INT 10, so `+2` is INT 12, which is MAC 13.
#[tokio::test]
async fn a_frame_alone_resolves_to_the_int_line_and_the_mac_line() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    doc_get(&server, "IIDFrame.int", int_lines()).await;
    doc_get(&server, "IIDFrame.mac", mac_lines()).await;

    let (is_error, p) = call(&server).await;
    assert_ne!(is_error, Some(true), "{p}");
    assert_eq!(p["int_line_number"], 12, "{p}");
    assert_eq!(p["int_line"], " set b=zzUndefinedVar", "{p}");
    assert_eq!(p["mac_line_number"], 13, "{p}");
    assert_eq!(p["mac_line"], " set b=zzUndefinedVar", "{p}");
    assert_eq!(p["mac_mapping"], "exact", "{p}");
    assert_eq!(p["mapped_to_source_line"], true, "{p}");
    assert_eq!(p["frame_kind"], "routine", "{p}");
}

/// An INT-only legacy routine: no `.mac` to map to. The INT line is still exact, and the absence
/// of the `.mac` must NOT be reported as the routine being missing — that conflation is #534.
#[tokio::test]
async fn a_routine_with_no_mac_still_gives_the_int_line_and_claims_nothing_missing() {
    let server = MockServer::start().await;
    mount_root(&server, serde_json::json!(["USER", "%SYS"])).await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    doc_get(&server, "IIDFrame.int", int_lines()).await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/doc/IIDFrame\.mac$"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let (is_error, p) = call(&server).await;
    assert_ne!(is_error, Some(true), "{p}");
    assert_eq!(p["int_line_number"], 12, "{p}");
    assert_eq!(p["mapped_to_source_line"], false, "{p}");
    assert!(p.get("mac_line_number").is_none(), "{p}");
    let whole = p.to_string();
    assert!(
        !whole.contains("ROUTINE_NOT_FOUND"),
        "a missing .mac is not a missing routine: {p}"
    );
}

// ── the routine is not there ─────────────────────────────────────────────────────────────────

/// Acceptance: a frame for a routine that does not exist gives `ROUTINE_NOT_FOUND`, worded for
/// ROUTINES. The old answer named a CLASS, which is the sentence #534 is about, so the payload is
/// swept for it rather than only checking the code.
#[tokio::test]
async fn a_frame_for_an_absent_routine_is_refused_in_a_sentence_about_routines() {
    let server = MockServer::start().await;
    mount_root(&server, serde_json::json!(["USER", "%SYS"])).await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let (is_error, p) = call(&server).await;
    assert_eq!(is_error, Some(true), "{p}");
    assert_eq!(p["error_code"], "ROUTINE_NOT_FOUND", "{p}");
    let msg = p["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(
        msg.contains("routine"),
        "the refusal must be about a routine: {msg}"
    );
    assert!(
        !p.to_string().contains("is not a compiled class"),
        "the class sentence must not come back for a routine frame: {p}"
    );
}

/// The distinction that would regress silently. The document 404s AND so does the Atelier root,
/// so nothing on this connection is reachable — which says nothing about the routine. This must
/// not be `ROUTINE_NOT_FOUND`.
#[tokio::test]
async fn an_unreachable_namespace_is_not_reported_as_a_missing_routine() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/atelier/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let (_is_error, p) = call(&server).await;
    assert_ne!(
        p["error_code"], "ROUTINE_NOT_FOUND",
        "an all-404 connection must not be reported as a missing routine: {p}"
    );
}

/// A reply that answered a different question — a 500 — establishes nothing either way.
#[tokio::test]
async fn a_probe_that_errors_is_undetermined_not_absent() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let (is_error, p) = call(&server).await;
    assert_eq!(is_error, Some(true), "{p}");
    assert_eq!(p["error_code"], "ROUTINE_PRESENCE_UNDETERMINED", "{p}");
    assert!(
        !p.to_string().contains("ROUTINE_NOT_FOUND"),
        "a 500 is not an absence: {p}"
    );
}

/// The routine is there and the READ fails. That is a failed read, not a missing routine, and no
/// line may be counted from text that never arrived.
#[tokio::test]
async fn a_readable_probe_with_an_unreadable_listing_is_its_own_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let (is_error, p) = call(&server).await;
    assert_eq!(is_error, Some(true), "{p}");
    assert_eq!(p["error_code"], "INT_SOURCE_UNREADABLE", "{p}");
    assert!(
        p.get("int_line_number").is_none(),
        "nothing may be counted: {p}"
    );
}

/// Supplying `int_source` still wins, and still skips the read — so a caller can map a frame
/// against a listing this server cannot reach.
#[tokio::test]
async fn a_caller_supplied_listing_is_used_without_reading_the_document() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/doc/IIDFrame\.int$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/doc/IIDFrame\.mac$"))
        .respond_with(ResponseTemplate::new(404))
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
    let p: DebugParams = serde_json::from_value(serde_json::json!({
        "action": "map_int", "error_string": FRAME, "namespace": NS,
        "int_source": int_lines().join("\n")
    }))
    .expect("params must deserialise");
    let r = handle_iris_debug(&iris, &client, p)
        .await
        .expect("must answer");
    let text = match &r.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("{other:?}"),
    };
    let payload: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        payload["int_line_number"], 12,
        "the supplied listing must be used even though the GET would fail: {payload}"
    );
}
