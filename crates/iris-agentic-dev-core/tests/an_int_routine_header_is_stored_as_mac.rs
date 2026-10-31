//! #537: `iris_doc mode=put` stored a `[Type=INT]` routine as `.mac`, and nothing pinned it.
//!
//! `.int` is not a spelling of `.mac`: it is the INTERMEDIATE code IRIS generates from a `.mac`.
//! `doc_name_with_suffix` read exactly one type flag (`INC`) and sent everything else to `.mac`,
//! so `ROUTINE OMORDL [Type=INT]` landed in `OMORDL.mac` — a document whose own header contradicts
//! its type. `doc_name_from_source`, added by #530 in the SAME file, reads all three correctly;
//! the two disagreed, and the put path used the blind one.
//!
//! ## The rule this pins, which is NOT "store it as .int" and NOT "refuse it"
//!
//! Settled on the issue thread, per-document rather than per-type:
//!
//! | state of the namespace | answer |
//! |---|---|
//! | no `X.mac` | store the content as `X.int` — a legacy site whose `.mac` source was lost years ago is a real configuration, and the `routines` profile exists to work on it |
//! | `X.mac` exists | REFUSE with `INT_GENERATED_FROM_MAC`, write NOTHING, and say to edit `X.mac` |
//!
//! The refusal is the half that is not cosmetic. Where `X.mac` is present, `X.int` *is* its
//! generated output: the next compile of `X.mac` overwrites whatever the put stored, so accepting
//! the write reports a success whose effect is silently discarded — this repo's house defect shape.
//!
//! So the decision needs the namespace PROBED for the sibling, which is why the gap survived:
//! `doc_name_with_suffix` is a pure function of a name and a string and cannot look.
//!
//! ## The third state the issue thread did not name
//!
//! A probe can also FAIL — 401, 500, a timeout. Reading that as "no `X.mac` there" stores the
//! `.int` on the strength of a measurement that never happened, and reading it as "a `.mac` is
//! there" refuses on an equally invented fact. It is therefore its own outcome,
//! `INT_SIBLING_UNKNOWN`, and `a_probe_that_did_not_answer_is_not_a_namespace_with_no_mac_in_it`
//! is the test that keeps it from collapsing into either neighbour.
//!
//! ## How the write is observed
//!
//! Through the real `handle_iris_doc`, against wiremock, reading `received_requests()`. A refusal
//! that still wrote something is the bug wearing a better error message, and the only way to see
//! that is to count what reached the server. The PUT leg is mounted as a SUCCESS in every test
//! here, including the ones that must not write: a mock that rejected the write would make
//! "nothing was written" pass for the wrong reason.

use iris_agentic_dev_core::elicitation::{CheckoutCache, ElicitationStore};
use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::tools::doc::{doc_name_with_suffix, handle_iris_doc, IrisDocParams};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";

/// `ROUTINE X [Type=INT]` — the input the whole issue is about.
const INT_SOURCE: &str = "ROUTINE ZZ537Legacy [Type=INT]\nLIST ; list orders\n quit\n";

/// Mount the HEAD leg (the sibling probe's answer) and a SUCCEEDING PUT leg.
async fn server_with(head: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(head)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r".*/doc/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"status": {"errors": [], "summary": ""}, "console": []}),
        ))
        .mount(&server)
        .await;
    server
}

/// The paths of every request of one verb that reached the server, in arrival order.
async fn requests(server: &MockServer, verb: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("wiremock must be recording requests, or 'nothing was written' means nothing")
        .iter()
        .filter(|r| r.method.as_str() == verb)
        .map(|r| r.url.path().to_string())
        .collect()
}

/// Drive a real `mode=put` through the real handler.
///
/// `already_checked_out` pre-marks the checkout cache so `write_with_scm` skips its pre-write
/// source-control probe (which runs ObjectScript through a scratch class and would need a
/// generator mocked). BOTH candidate names are marked on purpose: whichever document the handler
/// decides to write, the write itself is what reaches the mock, so a wrong decision shows up as
/// the wrong PUT path rather than as a source-control failure that hides which name was chosen.
async fn put(
    server: &MockServer,
    name: &str,
    content: &str,
    already_checked_out: &[&str],
) -> (Option<bool>, serde_json::Value) {
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
    for doc in already_checked_out {
        cache.mark(NS, doc);
    }
    let p: IrisDocParams = serde_json::from_value(serde_json::json!({
        "mode": "put", "name": name, "content": content, "namespace": NS
    }))
    .expect("the params must deserialise");
    let r = handle_iris_doc(&iris, &client, p, &store, &cache)
        .await
        .expect("the tool must answer, not error out of the transport");
    let text = match &r.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    let payload = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"));
    (r.is_error, payload)
}

// ── (a) `[Type=INT]` with no `X.mac` present: store the `.int` ────────────────────────────────

/// The legacy case the `routines` profile exists to serve: an INT-only routine whose `.mac` source
/// is gone. The header is the document's own statement about what it is, so it is honoured.
#[tokio::test]
async fn an_int_header_with_no_mac_beside_it_is_stored_as_an_int() {
    // 404 = measured absent. The namespace answered; there is no ZZ537Legacy.mac in it.
    let server = server_with(ResponseTemplate::new(404)).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Legacy",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    let puts = requests(&server, "PUT").await;
    assert_eq!(
        puts,
        vec![format!("/api/atelier/v1/{NS}/doc/ZZ537Legacy.int")],
        "the content declares [Type=INT], so the put must land in the .int — payload: {payload}"
    );
    assert_eq!(is_error, Some(false), "the write must succeed: {payload}");
    assert_eq!(payload["success"], serde_json::Value::Bool(true));
}

/// `[ Type = INT ]` is legal ObjectScript. Spaces must not route the write to a different
/// document — the same control `the_type_flag_survives_spaces` applies to the #530 reader.
#[tokio::test]
async fn the_int_flag_survives_spaces() {
    let server = server_with(ResponseTemplate::new(404)).await;
    let (_, payload) = put(
        &server,
        "ZZ537Legacy",
        "ROUTINE ZZ537Legacy [ Type = INT ]\n quit\n",
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        requests(&server, "PUT").await,
        vec![format!("/api/atelier/v1/{NS}/doc/ZZ537Legacy.int")],
        "payload: {payload}"
    );
}

// ── (b) `[Type=INT]` WITH an `X.mac` present: refuse, and write nothing ───────────────────────

/// The whole reason the answer is not simply "store it as .int". `ZZ537Legacy.int` is the output
/// IRIS generates from `ZZ537Legacy.mac`, so the next compile of the `.mac` discards this content.
#[tokio::test]
async fn an_int_header_beside_an_existing_mac_is_refused() {
    // 200 = measured present: ZZ537Legacy.mac is in the namespace.
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Legacy",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        payload["error_code"], "INT_GENERATED_FROM_MAC",
        "the refusal must be named so a caller can tell it from the other two outcomes: {payload}"
    );
    assert_eq!(is_error, Some(true), "a refusal is an error envelope");
    assert_eq!(payload["success"], serde_json::Value::Bool(false));
    let error = payload["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("ZZ537Legacy.mac"),
        "the refusal must NAME the document to edit instead: {error}"
    );
    assert!(
        error.contains("ZZ537Legacy.int"),
        "and the document it refused to write: {error}"
    );
}

/// The assertion the issue asks for in its own words: a refusal that still wrote something is the
/// bug wearing a better error message. The PUT leg is mounted and succeeding, so this counts a
/// write that COULD have landed — and the whole-population form (no request of any verb but the
/// one probe) rules out a write to the `.mac`, to the `.int`, or to a scratch class.
#[tokio::test]
async fn the_refusal_writes_nothing_at_all() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (_, payload) = put(
        &server,
        "ZZ537Legacy",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        requests(&server, "PUT").await,
        Vec::<String>::new(),
        "nothing may be written: {payload}"
    );
    let all: Vec<String> = server
        .received_requests()
        .await
        .expect("wiremock must be recording")
        .iter()
        .map(|r| format!("{} {}", r.method.as_str(), r.url.path()))
        .collect();
    assert_eq!(
        all,
        vec![format!("HEAD /api/atelier/v1/{NS}/doc/ZZ537Legacy.mac")],
        "the only call the refusal may make is the sibling probe itself"
    );
}

// ── The third state: a probe that did not answer ───────────────────────────────────────────────

/// A 401 says nothing about whether `ZZ537Legacy.mac` exists. Storing the `.int` would be acting
/// on a fact nobody measured, which is the defect shape this repo keeps hitting; so would refusing
/// with `INT_GENERATED_FROM_MAC`, which asserts the `.mac` IS there. The failure gets its own code.
#[tokio::test]
async fn a_probe_that_did_not_answer_is_not_a_namespace_with_no_mac_in_it() {
    let server = server_with(ResponseTemplate::new(401)).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Legacy",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        payload["error_code"], "INT_SIBLING_UNKNOWN",
        "a failed probe is its own outcome, not either of the two measured ones: {payload}"
    );
    assert_eq!(is_error, Some(true));
    assert_eq!(
        requests(&server, "PUT").await,
        Vec::<String>::new(),
        "and nothing may be written on an unknown: {payload}"
    );
}

// ── (c) The control: a normal MAC routine is unaffected ───────────────────────────────────────

/// Green before the fix as well as after — that is what makes it the control. It pins the two
/// things the fix must not change: a plain `ROUTINE` header still writes the `.mac`, and a put
/// that is not `[Type=INT]` costs NO extra round trip, so the sibling probe did not become a HEAD
/// on every write.
///
/// The HEAD leg here answers 200 on purpose: if the INT rule leaked into the MAC path, a `.mac`
/// "already present" would refuse this write.
#[tokio::test]
async fn a_plain_mac_routine_is_unaffected() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Plain",
        "ROUTINE ZZ537Plain\nLIST ; list orders\n quit\n",
        &["ZZ537Plain.mac", "ZZ537Plain.int"],
    )
    .await;
    assert_eq!(
        requests(&server, "PUT").await,
        vec![format!("/api/atelier/v1/{NS}/doc/ZZ537Plain.mac")],
        "payload: {payload}"
    );
    assert_eq!(is_error, Some(false), "the write must succeed: {payload}");
    assert_eq!(
        requests(&server, "HEAD").await,
        Vec::<String>::new(),
        "a MAC put must not pay for the INT sibling probe"
    );
}

/// The second control, and the one that proves the type flag is read at all rather than the
/// suffix being hardcoded per call: `[Type=INC]` is an include, and it is not probed either.
#[tokio::test]
async fn an_inc_routine_is_unaffected() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Macros",
        "ROUTINE ZZ537Macros [Type=INC]\n#define ZZ537 1\n",
        &["ZZ537Macros.inc", "ZZ537Macros.mac"],
    )
    .await;
    assert_eq!(
        requests(&server, "PUT").await,
        vec![format!("/api/atelier/v1/{NS}/doc/ZZ537Macros.inc")],
        "payload: {payload}"
    );
    assert_eq!(is_error, Some(false), "the write must succeed: {payload}");
    assert_eq!(requests(&server, "HEAD").await, Vec::<String>::new());
}

// ── The suffix reader itself ──────────────────────────────────────────────────────────────────

/// The smallest statement of the defect, with the two flags it already read beside it as the
/// control that the header is parsed at all. `doc_name_with_suffix` answered `OMORDL.mac` for
/// content whose first line says `[Type=INT]`.
#[test]
fn the_suffix_reader_knows_all_three_routine_types() {
    let cases: &[(&str, &str)] = &[
        ("ROUTINE OMORDL [Type=INT]\n", "OMORDL.int"),
        ("ROUTINE OMORDL [ Type = INT ]\n", "OMORDL.int"),
        ("ROUTINE OMORDL [Type=INC]\n", "OMORDL.inc"),
        ("ROUTINE OMORDL\n quit\n", "OMORDL.mac"),
    ];
    let got: Vec<Option<String>> = cases
        .iter()
        .map(|(content, _)| doc_name_with_suffix("OMORDL", Some(content)))
        .collect();
    let want: Vec<Option<String>> = cases
        .iter()
        .map(|(_, want)| Some((*want).to_string()))
        .collect();
    assert_eq!(
        got,
        want,
        "{} of {} routine headers read correctly",
        got.iter().zip(want.iter()).filter(|(g, w)| g == w).count(),
        cases.len()
    );
}

// ── The SIBLING: the suffix the CALLER typed ───────────────────────────────────────────────────
//
// `plan_put_name` asks `doc_name_with_suffix`, which returns `None` the moment the name already
// carries a suffix — it exists to ADD a missing one. So `name="X.int"` took the `AsGiven` branch
// and reached `do_write` with no probe: the refusal above could be stepped round by typing the
// three characters it is about.
//
// That is not the rarer input, it is the commoner one. `iris_doc(mode=get, name='X.int')` →
// edit → `iris_doc(mode=put, name='X.int')` is the round trip a caller actually performs, and it
// carries the suffix every time. The issue thread states the rule per-DOCUMENT — "an `.int` put
// when a `.mac` of the same name exists" — and says nothing about where the suffix came from, so
// the probe is keyed on the document about to be written rather than on which branch named it.

/// The refusal the header path already gives, through the path a get/edit/put round trip uses.
#[tokio::test]
async fn a_caller_supplied_int_suffix_is_refused_when_the_mac_exists() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Legacy.int",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        payload["error_code"], "INT_GENERATED_FROM_MAC",
        "typing the .int suffix must not be a way round the refusal: {payload}"
    );
    assert_eq!(is_error, Some(true));
    assert_eq!(
        requests(&server, "PUT").await,
        Vec::<String>::new(),
        "and nothing may be written: {payload}"
    );
}

/// The probe keys on the DOCUMENT, not on the header: here the caller's name is the only thing
/// that says `.int`, and the content's bare `ROUTINE` header would have settled on `.mac`.
#[tokio::test]
async fn a_caller_supplied_int_suffix_is_probed_even_when_the_content_says_nothing() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (_, payload) = put(
        &server,
        "ZZ537Legacy.int",
        "ROUTINE ZZ537Legacy\nLIST ; list orders\n quit\n",
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        payload["error_code"], "INT_GENERATED_FROM_MAC",
        "the hazard is the document being written, not the header that named it: {payload}"
    );
    assert_eq!(
        requests(&server, "PUT").await,
        Vec::<String>::new(),
        "nothing may be written: {payload}"
    );
}

/// `has_doc_suffix` lower-cases before comparing, so `X.INT` reaches `mode=put` as an `.int`
/// write. A case-sensitive test in the probe would have left this spelling as the bypass.
#[tokio::test]
async fn an_upper_case_int_suffix_is_not_a_way_round_the_probe() {
    let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
    let (_, payload) = put(
        &server,
        "ZZ537Legacy.INT",
        INT_SOURCE,
        &["ZZ537Legacy.INT", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        payload["error_code"], "INT_GENERATED_FROM_MAC",
        "the suffix test must fold case, as has_doc_suffix does: {payload}"
    );
    assert_eq!(
        requests(&server, "PUT").await,
        Vec::<String>::new(),
        "nothing may be written: {payload}"
    );
}

/// The legacy case still works through the explicit-name path: no `.mac` in the namespace, so the
/// `.int` the caller named is written. The refusal must not become "no caller may ever put an
/// .int", which is the blanket (b) the issue thread rejected.
#[tokio::test]
async fn a_caller_supplied_int_suffix_with_no_mac_beside_it_is_stored() {
    let server = server_with(ResponseTemplate::new(404)).await;
    let (is_error, payload) = put(
        &server,
        "ZZ537Legacy.int",
        INT_SOURCE,
        &["ZZ537Legacy.int", "ZZ537Legacy.mac"],
    )
    .await;
    assert_eq!(
        requests(&server, "PUT").await,
        vec![format!("/api/atelier/v1/{NS}/doc/ZZ537Legacy.int")],
        "payload: {payload}"
    );
    assert_eq!(is_error, Some(false), "the write must succeed: {payload}");
}

/// The control for the whole sibling fix, as a POPULATION rather than one name: every other
/// explicitly-suffixed document still writes with NO extra round trip. Without this, keying the
/// probe on the target name could have made every put pay a HEAD, and each test above would still
/// have passed.
#[tokio::test]
async fn no_other_explicitly_named_document_is_probed() {
    let cases: &[(&str, &str)] = &[
        ("ZZ537Legacy.mac", "ROUTINE ZZ537Legacy\n quit\n"),
        (
            "ZZ537Macros.inc",
            "ROUTINE ZZ537Macros [Type=INC]\n#define ZZ537 1\n",
        ),
        (
            "ZZ537.Thing.cls",
            "Class ZZ537.Thing Extends %RegisteredObject\n{\n}\n",
        ),
    ];
    let mut probed: Vec<&str> = Vec::new();
    for (name, content) in cases {
        let server = server_with(ResponseTemplate::new(200).insert_header("ETag", "64")).await;
        let (is_error, payload) = put(&server, name, content, &[name]).await;
        assert_eq!(
            requests(&server, "PUT").await,
            vec![format!("/api/atelier/v1/{NS}/doc/{name}")],
            "{name} must be written as named: {payload}"
        );
        assert_eq!(is_error, Some(false), "{name} must succeed: {payload}");
        if !requests(&server, "HEAD").await.is_empty() {
            probed.push(name);
        }
    }
    assert_eq!(
        probed,
        Vec::<&str>::new(),
        "{} of {} non-.int documents paid for the sibling probe",
        probed.len(),
        cases.len()
    );
}
