//! #529: the hook uploads the file, then compiles it, and its verdict is readable.
//!
//! The shell script it replaces sent exactly one request — `POST action/compile` with a document
//! name (compile-hook.sh:53-58) — so IRIS judged the copy it already held. Every verdict then went
//! to stdout with `exit 0`, which a `PostToolUse` hook shows in the transcript and never to the
//! model. Two defects composing into silence about the wrong version.
//!
//! These drive `upload_and_compile` against a mock rather than the CLI, because what has to be
//! true is about REQUESTS — which ones leave the process, and in what order. The CLI's own job is
//! stdin, discovery and the exit code; the one thing it must never do is turn an unknown outcome
//! into a clean one, and that distinction is carried by `HookVerdict` here.

use iris_agentic_dev_core::hook::{is_objectscript_source, upload_and_compile, HookVerdict};
use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NS: &str = "USER";
const DOC: &str = "OM.API.Customer.cls";
const SRC: &str = "Class OM.API.Customer Extends %Persistent\n{\n}\n";

fn conn(server: &MockServer) -> IrisConnection {
    IrisConnection::new(
        server.uri(),
        NS,
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    )
}

async fn mount_put(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/atelier/v1/USER/doc/.*"))
        .respond_with(resp)
        .mount(server)
        .await;
}

async fn mount_compile(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/atelier/v1/USER/action/compile"))
        .respond_with(resp)
        .mount(server)
        .await;
}

fn ok_envelope() -> serde_json::Value {
    serde_json::json!({"status": {"errors": []}, "console": [], "result": {}})
}

/// The request log, in arrival order, as `METHOD path`.
async fn trail(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method.as_str(), r.url.path()))
        .collect()
}

async fn run(server: &MockServer) -> HookVerdict {
    let c = conn(server);
    let client = reqwest::Client::new();
    upload_and_compile(&c, &client, NS, DOC, SRC, "cuk").await
}

/// THE defect, and an ORDER claim rather than a presence one. A test that merely found a PUT and a
/// compile somewhere in the log would pass against a hook that compiles first and uploads
/// afterwards — which judges the old copy just as surely as not uploading at all.
#[tokio::test]
async fn the_source_is_uploaded_before_it_is_compiled() {
    let server = MockServer::start().await;
    mount_put(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;

    let v = run(&server).await;
    assert_eq!(v, HookVerdict::Compiled { doc: DOC.into() });

    let t = trail(&server).await;
    let put = t.iter().position(|r| r.starts_with("PUT "));
    let compile = t.iter().position(|r| r.contains("action/compile"));
    assert!(
        put.is_some(),
        "the file was never uploaded — the whole of #529. Requests seen: {t:?}"
    );
    assert!(
        compile.is_some(),
        "nothing was compiled. Requests seen: {t:?}"
    );
    assert!(
        put < compile,
        "the upload must PRECEDE the compile, or IRIS judges its own older copy. Order: {t:?}"
    );
}

/// A clean compile names the document, which is what makes a #530-style mismatch visible.
#[tokio::test]
async fn a_clean_compile_names_the_document_it_compiled() {
    let server = MockServer::start().await;
    mount_put(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;
    let v = run(&server).await;
    assert_eq!(v.doc(), DOC);
    assert!(!v.is_actionable(), "a clean compile must not interrupt");
}

/// The errors have to survive as text, because they are the only thing the agent can act on.
#[tokio::test]
async fn compile_errors_are_reported_and_are_actionable() {
    let server = MockServer::start().await;
    mount_put(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": [
                {"error": "ERROR #1000: Method Foo: SYNTAX ERROR offset:12 line:4"}
            ]},
            "console": ["Compiling class OM.API.Customer", "Detected 1 errors during compilation"],
            "result": {}
        })),
    )
    .await;

    match run(&server).await {
        HookVerdict::CompileFailed {
            doc,
            errors,
            undisclosed,
        } => {
            assert_eq!(doc, DOC);
            assert!(
                errors.iter().any(|e| e.contains("SYNTAX ERROR")),
                "IRIS's own text must reach the caller, got {errors:?}"
            );
            assert_eq!(
                undisclosed, None,
                "1 detected and 1 itemised agree, so nothing is withheld"
            );
        }
        other => panic!("a reported compile error must be CompileFailed, got {other:?}"),
    }
}

/// The control for the arm above: when IRIS counts MORE than it itemises, the report must say the
/// list is partial instead of presenting it as the whole story.
#[tokio::test]
async fn a_partial_error_list_says_that_it_is_partial() {
    let server = MockServer::start().await;
    mount_put(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": [{"error": "ERROR #1000: first"}]},
            "console": ["Detected 4 errors during compilation"],
            "result": {}
        })),
    )
    .await;

    match run(&server).await {
        HookVerdict::CompileFailed { undisclosed, .. } => assert_eq!(
            undisclosed,
            Some(4),
            "IRIS said 4 and itemised 1; the gap is the finding"
        ),
        other => panic!("expected CompileFailed, got {other:?}"),
    }
}

/// THE shape the shell script got wrong: HTTP 401 with a zero-byte body read as "Compiled OK".
/// Unknown is its own verdict, and it must not be a clean compile.
#[tokio::test]
async fn an_upload_refused_with_401_is_unknown_not_a_clean_compile() {
    let server = MockServer::start().await;
    mount_put(&server, ResponseTemplate::new(401)).await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;

    match run(&server).await {
        HookVerdict::CouldNotTell { doc, why } => {
            assert_eq!(doc, DOC);
            assert!(
                why.contains("401") && why.to_lowercase().contains("credential"),
                "the reason must name the status and the likely cause, got {why:?}"
            );
        }
        other => panic!("a refused upload is not a verdict about the code, got {other:?}"),
    }
}

/// And it must not go on to compile. A compile after a failed upload produces a verdict about the
/// copy IRIS already had — which is precisely the bug, arrived at by a longer route.
#[tokio::test]
async fn a_failed_upload_is_not_followed_by_a_compile() {
    let server = MockServer::start().await;
    mount_put(&server, ResponseTemplate::new(401)).await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;

    let _ = run(&server).await;
    let t = trail(&server).await;
    assert!(
        t.iter().any(|r| r.starts_with("PUT ")),
        "control: the upload must have been attempted, or this proves nothing. Saw {t:?}"
    );
    assert!(
        !t.iter().any(|r| r.contains("action/compile")),
        "no compile may follow a refused upload. Saw {t:?}"
    );
}

/// A 2xx can still carry a refusal in the envelope. Reading only the status would let the compile
/// run against the stored copy and report it as this edit's result.
#[tokio::test]
async fn an_upload_refused_inside_a_200_is_unknown_and_stops_there() {
    let server = MockServer::start().await;
    mount_put(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": {"errors": [{"error": "ERROR #5845: Element is locked by another user"}]},
            "result": {}
        })),
    )
    .await;
    mount_compile(
        &server,
        ResponseTemplate::new(200).set_body_json(ok_envelope()),
    )
    .await;

    match run(&server).await {
        HookVerdict::CouldNotTell { why, .. } => assert!(
            why.contains("5845"),
            "IRIS's own refusal text must survive, got {why:?}"
        ),
        other => panic!("a 200 carrying status.errors is not a stored document, got {other:?}"),
    }
    let t = trail(&server).await;
    assert!(
        !t.iter().any(|r| r.contains("action/compile")),
        "no compile may follow a refusal, whatever its HTTP status. Saw {t:?}"
    );
}

/// An unreachable instance is unknown, not clean — and the reason has to say so.
#[tokio::test]
async fn an_unreachable_instance_is_unknown_not_a_clean_compile() {
    // A server that was started and dropped leaves a port with nothing listening, which is a
    // transport failure rather than any HTTP status.
    let uri = {
        let s = MockServer::start().await;
        s.uri()
    };
    let c = IrisConnection::new(uri, NS, "_SYSTEM", "SYS", DiscoverySource::ExplicitFlag);
    let client = reqwest::Client::new();
    let v = upload_and_compile(&c, &client, NS, DOC, SRC, "cuk").await;
    // Matched by reference: `v` is still needed for `is_actionable` afterwards.
    match &v {
        HookVerdict::CouldNotTell { why, .. } => {
            assert!(!why.is_empty(), "the reason must not be blank");
            assert!(v.is_actionable(), "an unknown outcome has to interrupt");
        }
        other => panic!("an unreachable instance cannot be a clean compile, got {other:?}"),
    }
}

/// The silent path. A hook that comments on every `.md` edit is one the user disables, and a
/// disabled hook protects nothing.
#[test]
fn only_objectscript_sources_are_acted_on() {
    for yes in ["src/OM/Customer.cls", "src/OMORDL.mac", "src/OMMACROS.inc"] {
        assert!(is_objectscript_source(yes), "{yes} is ObjectScript");
    }
    for no in ["README.md", "Cargo.toml", "src/main.rs", "noextension"] {
        assert!(!is_objectscript_source(no), "{no} is not ObjectScript");
    }
    // `.int` is generated FROM a .mac, so compiling an edited one would compile an artefact.
    assert!(!is_objectscript_source("OMORDL.int"));
    // Case is IRIS's, not the filesystem's.
    assert!(is_objectscript_source("Customer.CLS"));
}
