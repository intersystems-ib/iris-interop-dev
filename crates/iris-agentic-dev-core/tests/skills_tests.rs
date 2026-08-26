//! T032: Unit tests for KB skills subscription loader.
//! Uses mock GitHub API responses.

use iris_agentic_dev_core::skills::SkillRegistry;

/// SkillRegistry starts empty.
#[test]
fn registry_starts_empty() {
    let registry = SkillRegistry::new();
    assert_eq!(registry.list_skills().len(), 0);
}

// ── #92: the subscription path, offline ───────────────────────────────────────
//
// These two tests used to reach raw.githubusercontent.com unconditionally on every
// required-gate run — 3 live GETs — and, worse, asserted NOTHING: both ended in
// `let _ = result;`, so a GitHub outage, a 429 or a DNS hijack passed them just as
// happily as a correct answer. That is the same fake-coverage pathology #87 removed
// from manifest_tests. They now drive `load_from_github_at` against a wiremock server
// on localhost and assert on the actual outcome.
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A 404 manifest must surface WHY it failed, naming the repo.
#[tokio::test]
async fn load_invalid_repo_returns_error_offline() {
    let server = MockServer::start().await;
    // Every path 404s — the shape of a repo with no manifest at its root.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let mut registry = SkillRegistry::new();
    let err = registry
        .load_from_github_at(&server.uri(), "nonexistent/repo-that-does-not-exist-xyzzy")
        .await
        .expect_err("a repo with no iris-agentic-dev.toml must be an Err");

    let msg = format!("{:#}", err);
    assert!(
        msg.contains("no iris-agentic-dev.toml found"),
        "error must say what was missing, got: {msg}"
    );
    assert!(
        msg.contains("nonexistent") && msg.contains("repo-that-does-not-exist-xyzzy"),
        "error must name the owner/repo, got: {msg}"
    );
    assert_eq!(
        registry.list_skills().len(),
        0,
        "a failed load must not leave partial skills behind"
    );
}

/// Two successful subscriptions accumulate, and each skill remembers its own source repo.
#[tokio::test]
async fn multiple_subscriptions_accumulate_offline() {
    let server = MockServer::start().await;

    for (owner, repo, skill) in [
        ("owner1", "repo1", "alpha-skill"),
        ("owner2", "repo2", "beta-skill"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{owner}/{repo}/HEAD/iris-agentic-dev.toml")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"
[package]
name = "{repo}"
version = "1.0.0"
[provides]
skills = ["skills/{skill}"]
kb_items = ["kb/{skill}-notes.md"]
"#
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/{owner}/{repo}/HEAD/skills/{skill}/SKILL.md"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "---\nname: {skill}\ndescription: a mocked skill\n---\n\n# {skill}\n"
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{owner}/{repo}/HEAD/kb/{skill}-notes.md")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "---\ntitle: {skill} notes\n---\n\nmocked kb body\n"
            )))
            .mount(&server)
            .await;
    }

    let mut registry = SkillRegistry::new();
    registry
        .load_from_github_at(&server.uri(), "owner1/repo1")
        .await
        .expect("first subscription should load");
    registry
        .load_from_github_at(&server.uri(), "owner2/repo2")
        .await
        .expect("second subscription should load");

    // The point of the test: the second load ADDS to the first, it does not replace it.
    assert_eq!(
        registry.list_skills().len(),
        2,
        "two subscriptions must accumulate"
    );
    let names: Vec<&str> = registry.list_skills().iter().map(|s| &*s.name).collect();
    assert!(names.contains(&"alpha-skill"), "got: {names:?}");
    assert!(names.contains(&"beta-skill"), "got: {names:?}");

    let repos: Vec<&str> = registry
        .list_skills()
        .iter()
        .map(|s| &*s.source_repo)
        .collect();
    assert!(repos.contains(&"owner1/repo1"), "got: {repos:?}");
    assert!(repos.contains(&"owner2/repo2"), "got: {repos:?}");

    // #92 follow-up: retargeting the live canary dropped the only assertion covering the
    // kb-item fetch/registration path (the new target publishes no kb_items), and the
    // offline replacement did not pick it up — leaving load_from_github -> list_kb_items
    // asserted nowhere. Only kb_items TOML *parsing* was still covered.
    let kb_titles: Vec<&str> = registry.list_kb_items().iter().map(|k| &*k.title).collect();
    assert_eq!(
        kb_titles.len(),
        2,
        "kb items must accumulate too: {kb_titles:?}"
    );
    assert!(
        kb_titles.contains(&"alpha-skill notes"),
        "the frontmatter title must be used: {kb_titles:?}"
    );
}

// ── #92: the live canary ──────────────────────────────────────────────────────

// Copy of `live_github_enabled` from tests/manifest_tests.rs:209 — integration-test
// binaries are separate crates and cannot share a helper without a tests/common/mod.rs.
// Keep the two in step if either changes.
//
// Reuses #87's IRIS_DEV_LIVE_GITHUB rather than keeping a second flag name
// (IRIS_DEV_NETWORK_TESTS, now retired) for the same concept.
fn live_github_enabled() -> bool {
    let on = std::env::var("IRIS_DEV_LIVE_GITHUB")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        })
        .unwrap_or(false);
    if !on {
        eprintln!(
            "SKIP: live GitHub subscription canary — set IRIS_DEV_LIVE_GITHUB=1 to run it. \
             Offline coverage of the same logic runs unconditionally in the *_offline tests."
        );
    }
    on
}

/// Live canary: subscribe to a real package over the real network. Advisory only.
///
/// RETARGETED (#92). This was `e2e_subscribe_to_iris_vector_rag`, pointed at
/// intersystems-community/iris-vector-rag — which publishes no manifest: the URL returns
/// HTTP 404, so the test FAILED whenever it was actually enabled, and its
/// `IRIS_DEV_NETWORK_TESTS` guard meant nobody saw that between v0.5.0 (7eb8b96) and now.
/// Upstream's own iris-agentic-dev.toml records the cause — iris-vector-graph and
/// iris-vector-rag "moved to their own repos (commit 1fb7a8c) — do not re-add here, the
/// paths 404 on install."
///
/// It now points at intersystems-community/iris-agentic-dev, whose root manifest returns
/// 200 and lists ~35 skills. Assertions are deliberately LOOSE — two long-standing skill
/// names and a lower bound, not an exact count — because this depends on an upstream
/// repo's file staying put, and a brittle assertion here would rot the same way.
#[tokio::test]
async fn live_subscribe_canary() {
    if !live_github_enabled() {
        return;
    }
    let mut registry = SkillRegistry::new();
    registry
        .load_from_github("intersystems-community/iris-agentic-dev")
        .await
        .expect("should load the iris-agentic-dev skill pack");

    let names: Vec<&str> = registry.list_skills().iter().map(|s| &*s.name).collect();
    assert!(
        names.len() >= 10,
        "expected the published skill pack, got {} skills: {names:?}",
        names.len()
    );
    assert!(
        names.contains(&"objectscript-review"),
        "objectscript-review must be present, got: {names:?}"
    );
    assert!(
        names.contains(&"iris-sql"),
        "iris-sql must be present, got: {names:?}"
    );
}

/// Unit test: subscribe parsing uses iris-dev.toml from the skills/ subdirectory.
/// Verifies the path convention: light-skills/ as the package root.
#[tokio::test]
async fn subscribe_path_convention_is_light_skills_subdir() {
    // The iris-dev.toml lives at light-skills/iris-dev.toml in the repo.
    // The GitHub raw URL would be:
    // https://raw.githubusercontent.com/intersystems-community/vscode-objectscript-mcp/HEAD/light-skills/iris-dev.toml
    // This test verifies our manifest parser handles the skills paths correctly.
    use iris_agentic_dev_core::manifest::parse_manifest;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("iris-agentic-dev.toml"),
        r#"
[package]
name = "iris-vector-rag-skills"
version = "1.0.0"
[provides]
skills = ["skills/iris-rag-pipeline", "skills/iris-vector-search"]
kb_items = ["kb/iris-vector-patterns.md"]
"#,
    )
    .unwrap();
    let manifest = parse_manifest(dir.path().join("iris-agentic-dev.toml")).unwrap();
    let provides = manifest.provides.unwrap();
    assert_eq!(provides.skills.len(), 2);
    assert_eq!(provides.kb_items.len(), 1);
}

// ── Issue #89: agent_info(what=stats) must never invent a count ───────────────

/// #89 regression test, exactly as filed: `agent_info(what=stats)` reported
/// `{"skill_count":0,"success":true}` while `skill(action=list)` in the SAME session
/// answered `DOCKER_REQUIRED` — reproduced live against a registry that genuinely held 2
/// skills. `IrisConnection::execute` is docker-exec ONLY, so with no `IRIS_CONTAINER` the
/// read fails before anything is dialled or spawned: this test touches neither the network
/// nor docker.
///
/// ONE `#[test]` on purpose — `IRIS_CONTAINER` is process-global and cargo runs tests in
/// parallel threads, so splitting the sub-cases would race (same rule as
/// `skills_namespace_fallback_chain`).
#[test]
fn agent_info_stats_reports_unreachable_not_a_count_of_zero() {
    use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
    use iris_agentic_dev_core::tools::skills_tools::{handle_agent_info, AgentInfoParams};

    fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
        let text = match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        serde_json::from_str(text).unwrap()
    }

    std::env::remove_var("IRIS_CONTAINER");
    std::env::remove_var("OBJECTSCRIPT_SKILLMCP_NAMESPACE");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // RFC 2606 `.invalid` host so a future edit cannot accidentally point this at the
    // maintainer's live dev IRIS.
    let iris = IrisConnection::new(
        "http://never-dialled.invalid",
        "APP",
        "_SYSTEM",
        "SYS",
        DiscoverySource::EnvVar,
    );
    let client = reqwest::Client::new();
    let history = std::sync::Mutex::new(std::collections::VecDeque::new());

    rt.block_on(async {
        // 1. The issue itself: an unreadable registry is an ERROR, not a count of zero.
        let r = handle_agent_info(
            &iris,
            &client,
            AgentInfoParams {
                what: "stats".into(),
                limit: 20,
            },
            &history,
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(true), "a false zero is not success");
        let v = payload(&r);
        assert_eq!(v["error_code"], "DOCKER_REQUIRED", "{v}");
        assert!(v.get("skill_count").is_none(), "no count may survive: {v}");
        // #85: say WHICH registry — the count resolves from the connection (APP, not USER).
        assert_eq!(v["namespace"], "APP", "{v}");
        assert_eq!(v["source"], "^SKILLS", "{v}");

        // 2. what=history touches no connection and must NOT be dragged into the failure.
        let r = handle_agent_info(
            &iris,
            &client,
            AgentInfoParams {
                what: "history".into(),
                limit: 20,
            },
            &history,
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(false), "history needs no IRIS");
        let v = payload(&r);
        assert_eq!(v["success"], true, "{v}");
        assert_eq!(v["calls"], serde_json::json!([]), "{v}");

        // 3. An unknown `what` is still a parameter error, not a registry error.
        let r = handle_agent_info(
            &iris,
            &client,
            AgentInfoParams {
                what: "bogus".into(),
                limit: 20,
            },
            &history,
        )
        .await
        .unwrap();
        assert_eq!(payload(&r)["error_code"], "INVALID_PARAM");
    });

    std::env::remove_var("IRIS_CONTAINER");
}
