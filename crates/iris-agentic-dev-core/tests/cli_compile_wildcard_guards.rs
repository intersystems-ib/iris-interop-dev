//! #313: the CLI's `compile` reached `compile_document` with a wildcard untouched, so none of
//! iris_compile's #88/#100 guards applied to it.
//!
//! What the pass-through actually did was only ever inferred in that issue. Measured since, against
//! a live instance: Atelier `/action/compile` expands `Pkg.*` SERVER-SIDE, so the CLI really did
//! compile the package — with no scope rule, no cap and no count — and a pattern matching NOTHING
//! came back `{"status":{"errors":[]}}` with "Compilation finished successfully", which the CLI
//! printed as a success and exited 0 on. A typo'd package name was a silent no-op.
//!
//! Only the SCOPE_REQUIRED leg is asserted here, because it is the only one that needs no IRIS:
//! #88 refuses an unqualified pattern BEFORE fetching the listing, so it fires against a dead
//! port. The cap, the miss and the listing failure are covered by the unit tests on
//! `tools::wildcard`, which drive the same shared function through wiremock.
#![allow(clippy::zombie_processes)]

use std::process::Command;

fn iris_dev_bin() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/debug/iris-interop-dev");
    path
}

/// Run `compile <target>` against a port nothing listens on, so any request would fail.
///
/// `IRIS_HOST`/`IRIS_WEB_PORT` are set rather than unset: discovery must find a connection for the
/// command to get as far as the guard, and port 9 (discard) guarantees nothing is reachable if it
/// tries. A test that skipped when the binary is missing would report `ok` for a binary that was
/// never built — the exact shape that gave a false verdict four times in this repo — so a missing
/// binary panics instead.
fn compile_json(target: &str) -> (Option<i32>, serde_json::Value) {
    let bin = iris_dev_bin();
    assert!(
        bin.exists(),
        "{} is missing — run `cargo build --workspace` first. This test refuses to \
         pass without exercising the binary.",
        bin.display()
    );
    let out = Command::new(&bin)
        .args(["compile", target, "--format", "json"])
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
        .env("IRIS_USERNAME", "_SYSTEM")
        .env("IRIS_PASSWORD", "SYS")
        .env("IRIS_NAMESPACE", "APP")
        .env_remove("IRIS_CONTAINER")
        .output()
        .expect("failed to spawn the compile command");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let v = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "compile did not print JSON ({e}).\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code(), v)
}

/// A bare `*` selects on the tail alone — in a real namespace that is every class it holds, in one
/// request. The tool has refused this since #88; the CLI handed it to Atelier, which expands it.
#[test]
fn a_bare_wildcard_is_refused_and_exits_nonzero() {
    let (code, v) = compile_json("*");
    assert_eq!(v["error_code"], "SCOPE_REQUIRED", "{v}");
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(code, Some(1), "a refusal must not exit 0: {v}");
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("Nothing was compiled"),
        "the refusal must say nothing happened: {v}"
    );
}

/// The other two unqualified spellings. `*Foo` is the one that looks scoped and is not.
#[test]
fn the_other_unqualified_spellings_are_refused_too() {
    for target in ["*Foo", "*Pkg.Thing"] {
        let (code, v) = compile_json(target);
        assert_eq!(v["error_code"], "SCOPE_REQUIRED", "target {target}: {v}");
        assert_eq!(code, Some(1), "target {target}: {v}");
    }
}

/// The control. A QUALIFIED wildcard must NOT be refused by the scope rule — it has to reach the
/// listing, and only fail because this port is dead. Without this, every assertion above would be
/// satisfied by a CLI that refuses every wildcard, which would be a regression: the pass-through
/// did compile the package.
#[test]
fn a_qualified_wildcard_is_not_refused_by_the_scope_rule() {
    let (code, v) = compile_json("MyApp.*");
    assert_ne!(
        v["error_code"], "SCOPE_REQUIRED",
        "a qualified pattern must pass the scope rule: {v}"
    );
    assert_eq!(
        v["error_code"], "LISTING_UNAVAILABLE",
        "it must fail on the unreachable listing, not on the pattern: {v}"
    );
    assert_eq!(code, Some(1), "{v}");
}

/// A literal target must not be routed through the wildcard branch at all. `.cls` means "a file on
/// disk" to this command, so the assertion is that it tried to READ it — not that it compiled.
#[test]
fn a_literal_cls_target_is_still_read_as_a_file() {
    let bin = iris_dev_bin();
    assert!(bin.exists(), "{} is missing", bin.display());
    let out = Command::new(&bin)
        .args(["compile", "MyApp.Thing.cls", "--format", "json"])
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
        .env_remove("IRIS_CONTAINER")
        .output()
        .expect("spawn");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("reading MyApp.Thing.cls"),
        "a literal .cls target must still be treated as a path: {err}"
    );
}

/// #313, found by this test rather than reasoned about: `MyApp.*.cls` is a pattern `iris_compile`
/// documents and accepts, and this command read it as a FILENAME — `reading MyApp.*.cls: No such
/// file or directory` — because the `.cls` suffix test ran before the wildcard branch. The scope
/// rule and the cap could not apply to it: not because either was wrong, but because a sibling
/// condition made the branch holding them unreachable.
#[test]
fn a_wildcard_ending_in_cls_is_a_pattern_not_a_filename() {
    let (code, v) = compile_json("MyApp.*.cls");
    assert_eq!(
        v["error_code"], "LISTING_UNAVAILABLE",
        "it must reach the listing, i.e. be treated as a pattern: {v}"
    );
    assert_eq!(code, Some(1), "{v}");

    // And the unqualified spelling of the same shape must be refused by the scope rule.
    let (code, v) = compile_json("*.cls");
    assert_eq!(v["error_code"], "SCOPE_REQUIRED", "{v}");
    assert_eq!(code, Some(1), "{v}");
}

// ── the other three outcomes, through the real binary ───────────────────────────────────────────
//
// The scope rule above needs no IRIS. The cap, the miss and the listing failure do need a listing
// to come back, so these point the SPAWNED binary at a wiremock server: a real TCP port, so the
// process under test makes real requests, and `/action/compile` can be forbidden outright. That
// last assertion is the one that matters — "nothing was compiled" is a claim about a request NOT
// being made, and only the server can testify to it.

use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn listing_body(names: &[&str]) -> serde_json::Value {
    serde_json::json!({"result": {"content": names.iter()
        .map(|n| serde_json::json!({"cat":"CLS","db":"APP-CODE","gen":false,"name":n}))
        .collect::<Vec<_>>()}})
}

/// Run the binary against `server`, with `/action/compile` allowed or forbidden.
async fn compile_against(
    server: &MockServer,
    target: &str,
    expect_compiles: u64,
) -> (Option<i32>, serde_json::Value) {
    Mock::given(method("POST"))
        .and(path_regex(r".*/action/compile$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"status":{"errors":[],"summary":""},"console":[],"result":{"content":[]}}),
        ))
        .expect(expect_compiles)
        .mount(server)
        .await;
    let uri = server.uri(); // http://127.0.0.1:PORT
    let port = uri.rsplit(':').next().unwrap().to_string();
    let bin = iris_dev_bin();
    assert!(bin.exists(), "{} is missing", bin.display());
    let out = Command::new(&bin)
        .args(["compile", target, "--format", "json"])
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", &port)
        .env("IRIS_USERNAME", "_SYSTEM")
        .env("IRIS_PASSWORD", "SYS")
        .env("IRIS_NAMESPACE", "APP")
        .env_remove("IRIS_CONTAINER")
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let v = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "no JSON ({e}).\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code(), v)
}

/// THE DEFECT, end to end. Atelier answers a no-match wildcard with `errors: []` and "Compilation
/// finished successfully" — measured on a live instance — so the pass-through printed success and
/// exited 0 for a typo'd package. `expect(0)` on the compile endpoint is the proof that nothing was
/// sent; the exit code is the proof a script can see it.
#[tokio::test]
async fn a_wildcard_matching_nothing_is_not_found_and_compiles_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/docnames/CLS$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing_body(&["Other.Thing.cls"])))
        .mount(&server)
        .await;
    let (code, v) = compile_against(&server, "NoSuchPkg.*", 0).await;
    assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(code, Some(1), "a typo must not exit 0: {v}");
    let msg = v["error"].as_str().unwrap_or_default();
    assert!(msg.contains("Nothing was compiled"), "{v}");
    // #100's negative, kept honest: the listing cannot see everything, so the message must not
    // claim the classes do not exist.
    assert!(
        msg.contains("Hidden and generated"),
        "the message must say what the listing cannot see: {v}"
    );
}

/// #88's cap, through the binary. The count has to appear, or the reader is left guessing at a
/// narrower package.
#[tokio::test]
async fn over_the_cap_is_refused_with_the_count_and_compiles_nothing() {
    let server = MockServer::start().await;
    let names: Vec<String> = (0..505).map(|i| format!("Big.C{i}.cls")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    Mock::given(method("GET"))
        .and(path_regex(r".*/docnames/CLS$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing_body(&refs)))
        .mount(&server)
        .await;
    let (code, v) = compile_against(&server, "Big.*", 0).await;
    assert_eq!(v["error_code"], "TOO_BROAD", "{v}");
    assert_eq!(code, Some(1), "{v}");
    assert!(
        v["error"].as_str().unwrap_or_default().contains("505"),
        "the refusal must state how many matched: {v}"
    );
}

/// TEXT mode is the default format, so the count has to reach it too — a wildcard that compiled one
/// document when forty were expected is exactly what this reports, and a user who never passes
/// `--format json` would not see it. Singular/plural because "1 documents" is the kind of detail
/// that makes output look unmaintained.
#[tokio::test]
async fn text_mode_reports_how_many_documents_were_compiled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/docnames/CLS$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing_body(&[
            "App.One.cls",
            "App.Two.cls",
            "Other.X.cls",
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/action/compile$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"status":{"errors":[],"summary":""},"console":[],"result":{"content":[]}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let port = server.uri().rsplit(':').next().unwrap().to_string();
    let bin = iris_dev_bin();
    assert!(bin.exists(), "{} is missing", bin.display());
    let out = Command::new(&bin)
        .args(["compile", "App.*"]) // no --format: text is the default
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", &port)
        .env("IRIS_USERNAME", "_SYSTEM")
        .env("IRIS_PASSWORD", "SYS")
        .env("IRIS_NAMESPACE", "APP")
        .env_remove("IRIS_CONTAINER")
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ Compiled: App.* (2 documents)"),
        "text mode must state the count: {stdout:?} / stderr {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The control for both refusals above: a package UNDER the cap must still compile, in ONE request,
/// and report how many documents that was. Without this they would be satisfied by a CLI that
/// refuses every wildcard — which would be a regression, since the pass-through did compile.
#[tokio::test]
async fn a_matching_package_compiles_in_one_request_and_reports_the_count() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/docnames/CLS$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing_body(&[
            "App.One.cls",
            "App.Two.cls",
            "Other.Three.cls",
        ])))
        .mount(&server)
        .await;
    let (code, v) = compile_against(&server, "App.*", 1).await;
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(
        v["expanded"], 2,
        "the count the pass-through never had: {v}"
    );
    assert_eq!(
        v["targets"],
        serde_json::json!(["App.One.cls", "App.Two.cls"]),
        "{v}"
    );
}
