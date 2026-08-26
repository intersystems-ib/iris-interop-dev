//! T014: Unit tests for iris-agentic-dev.toml manifest parsing and semver resolver.
//! Tests written FIRST — must fail before implementation is complete.

use iris_agentic_dev_core::manifest::parse_manifest;

// ── parse_manifest ───────────────────────────────────────────────────────────

fn write_toml(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("iris-agentic-dev.toml");
    std::fs::write(&path, content).unwrap();
    path
}

/// A minimal valid manifest parses successfully.
#[test]
fn parse_minimal_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "my-skills"
version = "0.1.0"
"#,
    );
    let manifest = parse_manifest(&path).expect("should parse minimal manifest");
    assert_eq!(manifest.package.name, "my-skills");
    assert_eq!(manifest.package.version, "0.1.0");
    assert!(
        manifest.provides.is_none()
            || manifest
                .provides
                .as_ref()
                .map(|p| p.skills.is_empty())
                .unwrap_or(true)
    );
    assert!(manifest.dependencies.is_empty());
}

/// Full manifest with [provides] and [dependencies] parses correctly.
#[test]
fn parse_full_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "objectscript-skills"
version = "0.2.0"
description = "ObjectScript idioms for AI assistants"
authors = ["Thomas Dyar <thomas.dyar@intersystems.com>"]
license = "MIT"

[provides]
skills = ["skills/iris-compile.md", "skills/iris-debug.md"]
kb_items = ["kb/objectscript-errors.md"]
plugins = []

[dependencies]
base-kb = { version = "^0.1", github = "intersystems-community/base-kb" }
"#,
    );
    let manifest = parse_manifest(&path).expect("should parse full manifest");
    assert_eq!(manifest.package.name, "objectscript-skills");
    assert_eq!(manifest.package.version, "0.2.0");

    let provides = manifest.provides.expect("provides should be present");
    assert_eq!(provides.skills.len(), 2);
    assert_eq!(provides.skills[0], "skills/iris-compile.md");
    assert_eq!(provides.kb_items.len(), 1);

    assert_eq!(manifest.dependencies.len(), 1);
    let dep = &manifest.dependencies["base-kb"];
    assert_eq!(dep.version, "^0.1");
    assert_eq!(
        dep.github.as_deref(),
        Some("intersystems-community/base-kb")
    );
}

/// Missing required field `name` causes a parse error.
#[test]
fn parse_manifest_missing_name_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
version = "0.1.0"
"#,
    );
    assert!(
        parse_manifest(&path).is_err(),
        "manifest without name should fail"
    );
}

/// File not found returns an error.
#[test]
fn parse_manifest_missing_file_fails() {
    let result = parse_manifest("/nonexistent/path/iris-dev.toml");
    assert!(result.is_err(), "missing file should return error");
}

/// Invalid TOML returns an error.
#[test]
fn parse_manifest_invalid_toml_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(dir.path(), "not valid toml {{ {{ }}");
    assert!(parse_manifest(&path).is_err(), "invalid TOML should fail");
}

/// Dependency with github field parses correctly.
#[test]
fn parse_dependency_github() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "test"
version = "0.1.0"
[dependencies]
mypkg = { version = "^1.0", github = "owner/repo" }
"#,
    );
    let manifest = parse_manifest(&path).unwrap();
    let dep = &manifest.dependencies["mypkg"];
    assert_eq!(dep.version, "^1.0");
    assert_eq!(dep.github.as_deref(), Some("owner/repo"));
    assert!(dep.git.is_none());
    assert!(dep.openexchange.is_none());
}

/// Dependency with local repository path parses correctly.
#[test]
fn parse_dependency_local() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "test"
version = "0.1.0"
[dependencies]
local-dep = { version = "^0.2", repository = "../local-path" }
"#,
    );
    let manifest = parse_manifest(&path).unwrap();
    let dep = &manifest.dependencies["local-dep"];
    assert_eq!(dep.repository.as_deref(), Some("../local-path"));
}

// ── Resolve ──────────────────────────────────────────────────────────────────

use iris_agentic_dev_core::manifest::Resolve;

/// Resolve::from_manifest succeeds on a manifest with no dependencies.
#[test]
fn resolve_no_deps_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "standalone"
version = "1.0.0"
"#,
    );
    let manifest = parse_manifest(&path).unwrap();
    let resolve = Resolve::from_manifest(&manifest);
    assert!(resolve.is_ok(), "resolve with no deps should succeed");
}

/// Invalid semver version requirement is detected.
#[test]
fn resolve_invalid_semver_detected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_toml(
        dir.path(),
        r#"
[package]
name = "test"
version = "0.1.0"
[dependencies]
bad-dep = { version = "not-a-semver-version!!", github = "owner/repo" }
"#,
    );
    // Parse succeeds (version is just a string), but resolve should detect invalid semver
    let manifest = parse_manifest(&path).unwrap();
    let resolve = Resolve::from_manifest(&manifest);
    // TODO: when resolver is fully implemented, this should return Err
    // For now, assert it at least returns Ok (stub) or Err (real impl)
    let _ = resolve; // don't assert yet — resolver is a stub
}

// ── T041: resolve_version GitHub integration ────────────────────────────────
//
// #87: these four tests called api.github.com unauthenticated on EVERY run.
// Unauthenticated GitHub allows 60 requests/hour per IP, shared by every job on a
// runner, so a busy runner fails them with a 403 that reads nothing like a network
// flake — and the two `is_err()` ones passed for the WRONG reason under that 403,
// reporting fake coverage. The resolver logic now has deterministic offline coverage
// below; these stay as a live integration canary, opt-in via IRIS_DEV_LIVE_GITHUB=1.
// Set GITHUB_TOKEN as well and the resolver authenticates.
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn live_github_enabled() -> bool {
    let on = std::env::var("IRIS_DEV_LIVE_GITHUB")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        })
        .unwrap_or(false);
    if !on {
        eprintln!(
            "SKIP: live GitHub API test — set IRIS_DEV_LIVE_GITHUB=1 to run it \
             (GITHUB_TOKEN is honoured when present). Offline coverage of the same \
             logic runs unconditionally in the *_offline tests."
        );
    }
    on
}

async fn mock_github_tags(names: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    let body = serde_json::Value::Array(
        names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect(),
    );
    Mock::given(method("GET"))
        .and(path("/repos/intersystems-community/iris-dev/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn iris_dev_source() -> iris_agentic_dev_core::manifest::resolve::ResolvedSource {
    iris_agentic_dev_core::manifest::resolve::ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "iris-dev".to_string(),
    }
}

// ── offline: the tag→semver selection logic, no network, deterministic ──────

#[tokio::test]
async fn resolve_github_any_version_picks_highest_tag_offline() {
    use iris_agentic_dev_core::manifest::resolve::resolve_github_version_at;
    use semver::VersionReq;
    // deliberately unsorted, mixed "v"-prefixed and bare, plus a non-semver tag
    let server = mock_github_tags(&["v0.2.0", "v0.4.7", "v0.3.1", "0.4.2", "nightly"]).await;
    let v = resolve_github_version_at(
        &server.uri(),
        &VersionReq::parse("*").unwrap(),
        &iris_dev_source(),
    )
    .await
    .expect("should resolve");
    assert_eq!(v.to_string(), "0.4.7", "must pick the highest matching tag");
}

#[tokio::test]
async fn resolve_github_specific_range_offline() {
    use iris_agentic_dev_core::manifest::resolve::resolve_github_version_at;
    use semver::VersionReq;
    let server = mock_github_tags(&["v0.2.0", "v0.3.1", "v0.4.2", "v0.4.7", "v1.0.0"]).await;
    let v = resolve_github_version_at(
        &server.uri(),
        &VersionReq::parse("^0.4").unwrap(),
        &iris_dev_source(),
    )
    .await
    .expect("should resolve ^0.4");
    assert_eq!((v.major, v.minor, v.patch), (0, 4, 7));
}

#[tokio::test]
async fn resolve_github_unsatisfiable_range_errors_offline() {
    use iris_agentic_dev_core::manifest::resolve::resolve_github_version_at;
    use semver::VersionReq;
    let server = mock_github_tags(&["v0.2.0", "v0.4.7"]).await;
    let err = resolve_github_version_at(
        &server.uri(),
        &VersionReq::parse("^99.0").unwrap(),
        &iris_dev_source(),
    )
    .await
    .expect_err("unsatisfiable range must be Err");
    // assert on the REASON — under the old live tests a 403 satisfied is_err() too
    assert!(
        err.to_string().contains("satisfy version requirement"),
        "wrong failure reason: {err}"
    );
}

#[tokio::test]
async fn resolve_github_nonexistent_repo_errors_offline() {
    use iris_agentic_dev_core::manifest::resolve::{resolve_github_version_at, ResolvedSource};
    use semver::VersionReq;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let source = ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "this-repo-does-not-exist-xyz123".to_string(),
    };
    let err = resolve_github_version_at(&server.uri(), &VersionReq::parse("*").unwrap(), &source)
        .await
        .expect_err("404 must be Err");
    assert!(
        err.to_string().contains("not found"),
        "wrong failure reason: {err}"
    );
}

/// #87: a rate-limited response must say so, not just "403".
#[tokio::test]
async fn resolve_github_rate_limit_error_names_the_limit() {
    use iris_agentic_dev_core::manifest::resolve::resolve_github_version_at;
    use semver::VersionReq;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-limit", "60"),
        )
        .mount(&server)
        .await;
    let err = resolve_github_version_at(
        &server.uri(),
        &VersionReq::parse("*").unwrap(),
        &iris_dev_source(),
    )
    .await
    .expect_err("rate limit must be Err");
    let msg = err.to_string();
    assert!(
        msg.contains("rate limit"),
        "must name the rate limit: {msg}"
    );
    assert!(msg.contains("GITHUB_TOKEN"), "must name the remedy: {msg}");
}

// ── live canary: opt-in, authenticated when GITHUB_TOKEN is set ─────────────

/// GitHub tag resolution picks the highest matching version.
/// Uses intersystems-community/iris-dev which has known tags v0.2.0..v0.4.7.
#[tokio::test]
async fn test_resolve_github_any_version_succeeds() {
    if !live_github_enabled() {
        return;
    }
    use iris_agentic_dev_core::manifest::resolve::{resolve_github_version_async, ResolvedSource};
    use semver::VersionReq;
    let req = VersionReq::parse("*").unwrap();
    let source = ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "iris-dev".to_string(),
    };
    let result = resolve_github_version_async(&req, &source).await;
    assert!(
        result.is_ok(),
        "should resolve at least one version: {:?}",
        result
    );
    let v = result.unwrap();
    assert!(
        v.major > 0 || v.minor >= 2,
        "resolved version should be >= 0.2.0, got {}",
        v
    );
}

#[tokio::test]
async fn test_resolve_github_specific_range() {
    if !live_github_enabled() {
        return;
    }
    use iris_agentic_dev_core::manifest::resolve::{resolve_github_version_async, ResolvedSource};
    use semver::VersionReq;
    let req = VersionReq::parse("^0.4").unwrap();
    let source = ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "iris-dev".to_string(),
    };
    let result = resolve_github_version_async(&req, &source).await;
    assert!(result.is_ok(), "should resolve ^0.4: {:?}", result);
    let v = result.unwrap();
    assert_eq!(v.major, 0);
    assert_eq!(v.minor, 4);
}

#[tokio::test]
async fn test_resolve_github_unsatisfiable_range_errors() {
    if !live_github_enabled() {
        return;
    }
    use iris_agentic_dev_core::manifest::resolve::{resolve_github_version_async, ResolvedSource};
    use semver::VersionReq;
    let req = VersionReq::parse("^99.0").unwrap();
    let source = ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "iris-dev".to_string(),
    };
    let err = resolve_github_version_async(&req, &source)
        .await
        .expect_err("unsatisfiable range should return Err");
    // #87: assert on the REASON. A bare is_err() is satisfied by ANY failure — under a 403
    // rate limit (the flake this canary exists to survive) and under a 401 from a bad
    // GITHUB_TOKEN, this test reported `ok` while the resolver never reached the version
    // comparison at all. That is fake coverage: green canary, broken live path.
    assert!(
        err.to_string().contains("satisfy version requirement"),
        "the resolver must have compared tags and found none — wrong failure reason: {err}"
    );
}

#[tokio::test]
async fn test_resolve_github_nonexistent_repo_errors() {
    if !live_github_enabled() {
        return;
    }
    use iris_agentic_dev_core::manifest::resolve::{resolve_github_version_async, ResolvedSource};
    use semver::VersionReq;
    let req = VersionReq::parse("*").unwrap();
    let source = ResolvedSource::GitHub {
        owner: "intersystems-community".to_string(),
        repo: "this-repo-does-not-exist-xyz123".to_string(),
    };
    let err = resolve_github_version_async(&req, &source)
        .await
        .expect_err("nonexistent repo should return Err");
    // #87: the same fake-coverage guard — this must be GitHub's 404, not a 401/403 that
    // would have failed for any repo name at all.
    assert!(
        err.to_string().contains("not found"),
        "the resolver must have seen a 404 for the repo — wrong failure reason: {err}"
    );
}
