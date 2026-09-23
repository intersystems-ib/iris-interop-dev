//! #329 item 5: a refusal must offer the caller a next step, checked on the ENVELOPE at runtime.
//!
//! This is the property the other four items each fix one instance of, and #310's conclusion was
//! that the rule keeps recurring because nothing makes it checkable. `every_refusal_names_a_remedy`
//! is deliberately NOT this test: it is a review gate over the set of error *codes*, and its own
//! header says it "does not assert the server puts that sentence in the envelope at runtime". This
//! one drives real handlers and reads what a caller would actually receive.
//!
//! ## The rule, and the third form the issue did not anticipate
//!
//! The issue proposes "a `hint` field *or* an imperative in `error`". Driving the population showed
//! that is too narrow. `INVALID_PARAM` from `iris_message_body` reads:
//!
//! > data_policy 'Allow' is not one of block, redact, allow — refusing rather than falling through
//! > to an unredacted read
//!
//! No `hint`, no imperative — and yet it tells the caller exactly what to do, by ENUMERATING the
//! accepted values. Driving the rest of the population turned up a fourth form as well:
//!
//! > dataPolicy=allow requires acknowledgePhi=true — the body is returned unredacted.
//!
//! A stated REQUIREMENT, naming both the parameter and the value, and again neither a hint nor an
//! imperative. So the rule has four forms where the issue proposed two, and both additions came
//! from running it against real envelopes rather than from reasoning about it. That matters: a rule
//! failing either message would have reported a defect that is not there, and the first fix anyone
//! made in response would have made a good message worse.
//!
//! ## What this rule cannot do
//!
//! Judging whether a sentence is *useful* is not something a test can honestly claim, and this does
//! not try. It looks for structure: a hint field, an imperative opener at a clause boundary, or an
//! enumeration. A message can satisfy it and still be unhelpful; a genuinely helpful message phrased
//! around all three forms would fail it. What makes it worth having is that it is CALIBRATED — the
//! controls below pin a known-bare message as failing and known-good ones as passing, using real
//! text from this repo. Without those, a rule like this is just an opinion that compiles.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};

/// The tool's JSON payload, without naming any `rmcp` path (not a dev-dependency here).
fn payload<T: serde::Serialize>(r: &T) -> serde_json::Value {
    let v = serde_json::to_value(r).expect("the result must serialise");
    let text = v["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content, got: {v}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content was not JSON ({e}): {text}"))
}

/// Imperative openers, checked only at a clause boundary so a verb buried mid-sentence does not
/// count. Deliberately short: every entry is a verb that appears in an actual refusal in this tree.
const IMPERATIVES: &[&str] = &[
    "pass", "call", "use", "set", "check", "list", "compile", "run", "retry", "fix", "rewrite",
    "correct", "supply", "add", "remove", "restart", "choose", "pick", "read", "confirm", "drop",
    "resend", "name", "qualify", "unset", "install", "grant", "stop", "work", "wait", "look",
];

/// Phrases that enumerate what WOULD be accepted — actionable without an instruction.
const ENUMERATIONS: &[&str] = &[
    "is not one of",
    "must be one of",
    "must be ",
    "valid:",
    "valid actions",
    "one of:",
    "accepted",
    "expected one of",
];

/// Stated requirements — "X requires Y=Z" names the next step without ever being an instruction.
const REQUIREMENTS: &[&str] = &[
    "requires ",
    "needs ",
    "must include",
    "must have",
    "only with",
];

/// Does this envelope give the caller something to do next?
fn offers_a_next_step(envelope: &serde_json::Value) -> bool {
    if envelope["hint"]
        .as_str()
        .is_some_and(|h| !h.trim().is_empty())
    {
        return true;
    }
    let error = envelope["error"].as_str().unwrap_or("").to_lowercase();
    if ENUMERATIONS.iter().any(|e| error.contains(e)) {
        return true;
    }
    if REQUIREMENTS.iter().any(|r| error.contains(r)) {
        return true;
    }
    // An imperative only counts at a clause boundary: start of text, or after one of these.
    error
        .split(['—', '-', ';', '.', ',', ':', '\n'])
        .any(|clause| {
            let c = clause.trim_start();
            IMPERATIVES
                .iter()
                .any(|v| c.starts_with(v) && c[v.len()..].starts_with(' '))
        })
}

#[test]
fn the_rule_is_calibrated_against_real_text_from_this_repo() {
    // NEGATIVE CONTROLS — the bare refusals #329 measured. If the rule passes these it is
    // worthless, because these are exactly what the issue exists to find.
    for bare in [
        "Document not found: Censo.Msg.Paciente.cls",
        "No body found for message ID 4711",
        "No business rule named 'Foo' found",
        "Production 'Demo.Prod' does not exist in namespace 'APP'",
    ] {
        let e = serde_json::json!({"error": bare});
        assert!(
            !offers_a_next_step(&e),
            "the rule must NOT accept a bare negative fact: {bare:?}"
        );
    }

    // POSITIVE CONTROLS — real remediated text, one per form, so a rule that accepts nothing is
    // also caught.
    let imperative = serde_json::json!({"error":
        "iris_message_body is blocked while dataPolicy=block — message bodies may contain PHI. \
         Pass dataPolicy=redact to blank known HL7 v2 PHI fields."});
    assert!(offers_a_next_step(&imperative), "imperative form must pass");

    let enumeration = serde_json::json!({"error":
        "data_policy 'Allow' is not one of block, redact, allow — refusing rather than falling \
         through to an unredacted read"});
    assert!(
        offers_a_next_step(&enumeration),
        "enumerating the accepted values is a next step even with no imperative — the form the \
         issue's two-way rule would have wrongly flagged"
    );

    let requirement = serde_json::json!({"error":
        "dataPolicy=allow requires acknowledgePhi=true — the body is returned unredacted."});
    assert!(
        offers_a_next_step(&requirement),
        "a stated requirement names the parameter AND the value to send — the fourth form, found \
         by driving PHI_ACK_REQUIRED rather than by reasoning about the rule"
    );

    let hinted =
        serde_json::json!({"error": "Document not found: X", "hint": "call iris_doc_search"});
    assert!(offers_a_next_step(&hinted), "a hint field must pass");

    // And a hint that is present but empty must NOT rescue a bare message: an empty string is the
    // shape a `.unwrap_or_default()` leaves behind, which is the defect class #310 is about.
    let empty_hint = serde_json::json!({"error": "Document not found: X", "hint": ""});
    assert!(
        !offers_a_next_step(&empty_hint),
        "an empty hint is not a next step"
    );
}

/// A connection that is never reached: every refusal below returns before any request is made.
/// Pointing at a closed port is deliberate — if a handler ever starts making a call before these
/// checks, the test fails rather than quietly exercising a different path.
fn unreachable_conn() -> IrisConnection {
    IrisConnection::new(
        "http://127.0.0.1:9",
        "APP",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    )
}

#[tokio::test]
async fn every_argument_refusal_offers_a_next_step() {
    use iris_agentic_dev_core::tools::interop::{handle_iris_message_body, MessageBodyParams};

    // `acknowledge_phi` is the STRUCT field. The dispatcher accepts `acknowledgePhi` too — #151
    // added the snake_case read because the camelCase-only one was undiscoverable from the schema —
    // but this drives the handler directly, below the dispatcher, so only the struct name binds.
    // Spelling it the other way here silently yielded `false` and drove a different refusal
    // altogether, which is #78's shape inside a test fixture.
    let body = |id: &str, ack: bool| -> MessageBodyParams {
        serde_json::from_value(serde_json::json!({
            "message_id": id, "namespace": "APP", "acknowledge_phi": ack
        }))
        .expect("MessageBodyParams must deserialise from this shape")
    };

    // Four refusals reachable with NO connection at all: each returns before `iris` is touched.
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "INVALID_PARAM",
            payload(
                &handle_iris_message_body(None, &body("1", false), "Allow")
                    .await
                    .expect("Ok envelope"),
            ),
        ),
        (
            "PHI_POLICY_BLOCKED",
            payload(
                &handle_iris_message_body(None, &body("1", false), "block")
                    .await
                    .expect("Ok envelope"),
            ),
        ),
        (
            "PHI_ACK_REQUIRED",
            payload(
                &handle_iris_message_body(None, &body("1", false), "allow")
                    .await
                    .expect("Ok envelope"),
            ),
        ),
        (
            "INVALID_MESSAGE_ID",
            payload(
                &handle_iris_message_body(None, &body("not-a-number", true), "allow")
                    .await
                    .expect("Ok envelope"),
            ),
        ),
    ];

    // CONTROL: the population is real and each case is the refusal it claims to be. Without this, a
    // handler that started returning success would make the assertions below vacuous.
    assert_eq!(cases.len(), 4, "the population changed");
    for (expected_code, envelope) in &cases {
        assert_eq!(
            envelope["error_code"], *expected_code,
            "drove the wrong refusal: {envelope}"
        );
    }

    let bare: Vec<&str> = cases
        .iter()
        .filter(|(_, e)| !offers_a_next_step(e))
        .map(|(c, _)| *c)
        .collect();
    assert!(
        bare.is_empty(),
        "these refusals reach a caller with nothing to do next: {bare:?}\n\
         Add a `hint`, an imperative, or name the accepted values."
    );
}

#[tokio::test]
async fn a_scope_refusal_offers_a_next_step() {
    use iris_agentic_dev_core::tools::log_store::LogStore;
    use iris_agentic_dev_core::tools::search::{handle_iris_search, SearchParams};
    use std::sync::{Arc, Mutex};

    // `iris_search` with no document scope refuses before any request, so the closed port above is
    // never dialled.
    let params: SearchParams = serde_json::from_value(serde_json::json!({
        "query": "Patient", "namespace": "APP"
    }))
    .expect("SearchParams must deserialise");
    let conn = unreachable_conn();
    let client = reqwest::Client::new();
    let store = Arc::new(Mutex::new(LogStore::new(50, 600)));

    let envelope = payload(
        &handle_iris_search(&conn, &client, params, store)
            .await
            .expect("Ok envelope"),
    );
    assert_eq!(
        envelope["error_code"], "SCOPE_REQUIRED",
        "drove the wrong refusal: {envelope}"
    );
    assert!(
        offers_a_next_step(&envelope),
        "a scope refusal must say what scope to pass: {envelope}"
    );
}
