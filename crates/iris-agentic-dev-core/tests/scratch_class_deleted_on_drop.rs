//! #465: the scratch class outlives the future that created it.
//!
//! `execute_via_generator_once` PUTs `IrisDevTmp.Run<id>`, compiles it, then runs
//! `SELECT <proc>() AS result`. Four call sites wrap the whole thing in `tokio::time::timeout`
//! (`tools/mod.rs`: iris_test twice, iris_coverage, iris_execute), so a TIMEOUT drops the future
//! while the query is in flight — after the PUT and the compile have already succeeded. Nothing
//! then sends the DELETE and a compiled class is left behind. Upstream found 78,183 of them in one
//! namespace; 15 were sitting in namespace APP on the local instance when #465 was triaged
//! (control: the same query shape counted 463 `Ens.%`, so a zero would have been readable).
//!
//! The three `?` returns after the PUT — the compile send, the query send and the body read — leak
//! the same way, for the same reason: an early return is a drop.
//!
//! The second defect is in the cleanup itself. `delete_doc` sent the DELETE and returned `Ok(())`
//! without reading the reply, so a refused delete read as a completed one — this repo's house bug,
//! a failure answered as a negative fact. Two shapes refuse, and this path recognised neither:
//! a non-2xx, and an HTTP **200** whose `status.errors` carries the diagnosis (ERROR #5845 for a
//! locked or checked-out document). `tools/doc.rs:2080` already reads both and says so in a
//! comment; the generator's own delete did not, which is why the sibling looked like the careful
//! one and this one looked fine.
//!
//! ## Why a mock rather than an assertion on the source
//!
//! The thing under test is *which HTTP requests leave the process, and how many*. A source-level
//! guard can say a `Drop` impl exists; only counting requests says the DELETE was actually sent
//! when the future was dropped. Every count below is paired with a case that must produce a
//! different one, so a guard that is never armed and a guard that never disarms both fail.

use std::time::Duration;

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";

/// The PUT and the compile, both clean. The DELETE is mounted per-test, because its reply is
/// what several of these tests vary.
async fn scaffold() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
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

async fn delete_answers(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/atelier/v1/USER/doc/IrisDevTmp\..*"))
        .respond_with(resp)
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

/// A normal reply: one row, one string column.
fn wrote(s: &str) -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [], "summary": ""},
        "console": [],
        "result": {"content": [{"result": s}]}
    })
}

/// What Atelier answers when it will not delete a locked or checked-out document: HTTP 200, with
/// the whole diagnosis in `status.errors`.
fn refused_in_status_errors() -> serde_json::Value {
    serde_json::json!({
        "status": {"errors": [{
            "error": "ERROR #5845: Element 'IrisDevTmp.Run0123456789ab.CLS' is checked out or \
                      locked by another user",
            "code": 5845,
            "domain": "%ObjectErrors",
            "id": "LockFailedToAcquireExclusive"
        }], "summary": "ERROR #5845"},
        "console": [],
        "result": {}
    })
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

async fn deletes(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "DELETE" && r.url.path().contains("/doc/IrisDevTmp"))
        .count()
}

async fn puts(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "PUT" && r.url.path().contains("/doc/IrisDevTmp"))
        .count()
}

/// The DELETE from a dropped future is sent by a detached task, so it lands shortly AFTER the
/// call returns. Poll rather than sleep a fixed time: a fixed sleep either flakes or is slow.
/// Returns the count actually reached, so the assertion reports the real number.
async fn deletes_reaching(server: &MockServer, want: usize) -> usize {
    for _ in 0..200 {
        let n = deletes(server).await;
        if n >= want {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    deletes(server).await
}

/// THE defect. A timeout drops the generator future mid-query, after the PUT and the compile have
/// both succeeded, and the scratch class must still be deleted.
#[tokio::test]
async fn a_timeout_that_drops_the_generator_still_deletes_the_scratch_class() {
    let server = scaffold().await;
    delete_answers(&server, ResponseTemplate::new(200)).await;
    // The query never answers in time — exactly what a long-running script does to the four
    // `tokio::time::timeout` wrappers in tools/mod.rs.
    query_answers(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(wrote("never read"))
            .set_delay(Duration::from_secs(30)),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let r = tokio::time::timeout(
        Duration::from_millis(300),
        c.execute_via_generator("Hang 1", NS, &client),
    )
    .await;

    assert!(r.is_err(), "the call must have been dropped by the timeout");
    assert_eq!(
        puts(&server).await,
        1,
        "control: the scratch class must actually have been created, or this test proves nothing"
    );
    let n = deletes_reaching(&server, 1).await;
    assert_eq!(
        n, 1,
        "a timeout dropped the future after the PUT and the compile; without a guard the DELETE \
         is never sent and IrisDevTmp.Run<id> stays compiled in the namespace"
    );
}

/// The control the guard must not break: on the ordinary path the class is deleted once, not
/// twice. A guard that never disarms passes the test above and fails this one.
#[tokio::test]
async fn the_success_path_deletes_the_scratch_class_exactly_once() {
    let server = scaffold().await;
    delete_answers(&server, ResponseTemplate::new(200)).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(wrote("hello")),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Write \"hello\"", NS, &client)
        .await
        .expect("the success path must still succeed");
    assert_eq!(out, "hello");

    // Give a stray second DELETE time to arrive before concluding there wasn't one.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        deletes(&server).await,
        1,
        "the explicit delete ran, so the guard must have disarmed"
    );
}

/// A DELETE that IRIS refused with a non-2xx is not a completed clean-up. The guard stays armed
/// and tries once more, because the alternative is a compiled class left in a user's namespace
/// on the strength of a reply nobody read.
#[tokio::test]
async fn a_delete_refused_with_a_non_2xx_is_not_read_as_a_clean_up() {
    let server = scaffold().await;
    delete_answers(&server, ResponseTemplate::new(500)).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(wrote("hello")),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Write \"hello\"", NS, &client)
        .await
        .expect("a failed clean-up must not fail the call — the script's output is still good");
    assert_eq!(out, "hello");

    let n = deletes_reaching(&server, 2).await;
    assert_eq!(
        n, 2,
        "HTTP 500 on the DELETE means the class is still there; reading the status is the whole \
         point, and ignoring it leaves exactly one DELETE"
    );
}

/// The shape an HTTP-status-only fix still gets wrong. A locked or checked-out document is
/// refused with HTTP **200** and ERROR #5845 in `status.errors` — `tools/doc.rs:2080` has read it
/// that way for a long time, and this path did not.
#[tokio::test]
async fn a_delete_refused_in_status_errors_is_not_read_as_a_clean_up() {
    let server = scaffold().await;
    delete_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(refused_in_status_errors()),
    )
    .await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(wrote("hello")),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Write \"hello\"", NS, &client)
        .await
        .expect("a refused clean-up must not fail the call");
    assert_eq!(out, "hello");

    let n = deletes_reaching(&server, 2).await;
    assert_eq!(
        n, 2,
        "a 200 carrying ERROR #5845 is a refusal; checking only the HTTP status leaves one DELETE"
    );
}

/// A 404 is the one non-2xx that means the job is done: the class is not there. Retrying it would
/// be a second request that cannot change anything.
#[tokio::test]
async fn a_404_is_a_class_that_is_already_gone() {
    let server = scaffold().await;
    delete_answers(&server, ResponseTemplate::new(404)).await;
    query_answers(
        &server,
        ResponseTemplate::new(200).set_body_json(wrote("hello")),
    )
    .await;
    let c = conn(&server);
    let client = reqwest::Client::new();

    let out = c
        .execute_via_generator("Write \"hello\"", NS, &client)
        .await
        .expect("a 404 on the DELETE must not fail the call");
    assert_eq!(out, "hello");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        deletes(&server).await,
        1,
        "absent is the outcome the delete wanted, so the guard must disarm rather than retry"
    );
}
