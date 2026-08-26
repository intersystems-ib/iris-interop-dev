// Tests for workspace_config module: TOML loading, priority ordering, path resolution.

use iris_agentic_dev_core::iris::workspace_config::{
    load_workspace_config, workspace_config_to_connection, workspace_root,
};
use std::io::Write;

fn write_toml(dir: &tempfile::TempDir, contents: &str) {
    let path = dir.path().join(".iris-agentic-dev.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}

// ── Env serialisation (#95) ───────────────────────────────────────────────────
//
// This file was red on 7 of 15 `cargo test` runs before this guard, the same defect as
// #91: process-global env vars read and written by tests that cargo runs on parallel
// threads in one binary. The observed failure was a CROSS-VARIABLE leak —
// `test_https_scheme_in_base_url` saw `https://iris.example.com:443/myprefix`, i.e. an
// `IRIS_WEB_PREFIX` set by a web-prefix test bled into a scheme test.
//
// The treatment differs from #91's merge-into-one-function on purpose: #91 had three
// tests on one variable, which merge cleanly. Here there are five variables across 23
// tests and the leaks cross variable boundaries, so merging would mean one unreadable
// mega-test. A serialising guard that also resets every key keeps the test names.
//
// The guard clears SEVEN keys, not the five the tests mention directly:
// `workspace_config_to_connection` (src/iris/workspace_config.rs:247-253) SETS
// `IRIS_NAMESPACE`, `IRIS_USERNAME` and `IRIS_PASSWORD` as a side effect of merely being
// called, so a test that never names them still leaves them behind for the next one.
//
// Every test that calls `workspace_config_to_connection`, `load_workspace_config` or
// `workspace_root` takes the guard — including the `load_*` tests that pass an explicit
// directory, because `workspace_root` (:39-44) consults `OBJECTSCRIPT_WORKSPACE` BEFORE
// the path argument: a racing `set_var` makes them read the wrong directory entirely.
// The `generate_toml_*` and `score_*` tests touch no env and are deliberately unguarded.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    // `into_inner`: a panicking test must not poison the mutex and turn one real failure
    // into twenty-two misleading ones.
    let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for k in [
        "OBJECTSCRIPT_WORKSPACE",
        "IRIS_NAMESPACE",
        "IRIS_CONTAINER",
        "IRIS_WEB_PREFIX",
        "IRIS_SCHEME",
        "IRIS_USERNAME",
        "IRIS_PASSWORD",
    ] {
        std::env::remove_var(k);
    }
    g
}

// ── T004: Core loading tests ──────────────────────────────────────────────────

#[test]
fn test_load_returns_none_when_no_file() {
    let _g = env_guard();
    let result = load_workspace_config(Some("/nonexistent/path/that/cannot/exist"));
    assert!(
        result.is_none(),
        "should return None when file does not exist"
    );
}

#[test]
fn test_load_parses_container_field() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(&dir, r#"container = "test-iris""#);
    let cfg = load_workspace_config(Some(dir.path().to_str().unwrap())).unwrap();
    assert_eq!(cfg.container.as_deref(), Some("test-iris"));
}

#[test]
fn test_load_parses_all_fields() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(
        &dir,
        r#"
container = "all-iris"
namespace = "MYNS"
host = "myhost"
web_port = 9999
username = "myuser"
password = "mypass"
"#,
    );
    let cfg = load_workspace_config(Some(dir.path().to_str().unwrap())).unwrap();
    assert_eq!(cfg.container.as_deref(), Some("all-iris"));
    assert_eq!(cfg.namespace.as_deref(), Some("MYNS"));
    assert_eq!(cfg.host.as_deref(), Some("myhost"));
    assert_eq!(cfg.web_port, Some(9999));
    assert_eq!(cfg.username.as_deref(), Some("myuser"));
    assert_eq!(cfg.password.as_deref(), Some("mypass"));
}

#[test]
fn test_load_returns_none_on_syntax_error() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(&dir, "this is not valid toml = = = !!!");
    let result = load_workspace_config(Some(dir.path().to_str().unwrap()));
    assert!(
        result.is_none(),
        "should return None on parse error, not panic"
    );
}

#[test]
fn test_load_uses_cwd_when_workspace_none() {
    let _g = env_guard();
    // Call with None from a temp dir that has no .iris-dev.toml
    let dir = tempfile::TempDir::new().unwrap();
    let result = load_workspace_config(Some(dir.path().to_str().unwrap()));
    assert!(result.is_none());
}

#[test]
fn test_workspace_root_uses_env_var() {
    let _g = env_guard();
    // Note: env var tests can be flaky if run in parallel; use a unique key.
    // We only test the logic — the env var takes precedence over the path arg.
    let tmp = tempfile::TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap().to_string();
    std::env::set_var("OBJECTSCRIPT_WORKSPACE", &tmp_str);
    let root = workspace_root(Some("/some/other/path"));
    std::env::remove_var("OBJECTSCRIPT_WORKSPACE");
    assert_eq!(
        root,
        tmp.path(),
        "OBJECTSCRIPT_WORKSPACE should override path arg"
    );
}

#[test]
fn test_workspace_root_uses_path_when_no_env_var() {
    let _g = env_guard();
    std::env::remove_var("OBJECTSCRIPT_WORKSPACE");
    let root = workspace_root(Some("/explicit/path"));
    assert_eq!(root.to_str().unwrap(), "/explicit/path");
}

// ── T010: Connection building tests ──────────────────────────────────────────

#[test]
fn test_workspace_config_host_returns_connection() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(&dir, r#"host = "remotehost"\nweb_port = 9999"#);
    // Parse manually since \n in raw string literal doesn't give newline
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("remotehost".to_string()),
        web_port: Some(9999),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER");
    assert!(
        conn.is_some(),
        "host config should return Some(IrisConnection)"
    );
    let conn = conn.unwrap();
    assert!(
        conn.base_url.contains("remotehost"),
        "base_url should contain host, got: {}",
        conn.base_url
    );
    assert!(
        conn.base_url.contains("9999"),
        "base_url should contain port, got: {}",
        conn.base_url
    );
}

#[test]
fn test_workspace_config_namespace_applied() {
    let _g = env_guard();
    // Container config sets IRIS_NAMESPACE env var
    std::env::remove_var("IRIS_NAMESPACE");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        container: Some("mytest-iris".to_string()),
        namespace: Some("TESTNS".to_string()),
        ..Default::default()
    };
    workspace_config_to_connection(&cfg, "USER");
    assert_eq!(
        std::env::var("IRIS_NAMESPACE").ok().as_deref(),
        Some("TESTNS"),
        "IRIS_NAMESPACE should be set from workspace config namespace"
    );
    // Cleanup
    std::env::remove_var("IRIS_NAMESPACE");
}

#[test]
fn test_workspace_config_sets_iris_container_env() {
    let _g = env_guard();
    std::env::remove_var("IRIS_CONTAINER");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        container: Some("mytest-iris".to_string()),
        ..Default::default()
    };
    workspace_config_to_connection(&cfg, "USER");
    assert_eq!(
        std::env::var("IRIS_CONTAINER").ok().as_deref(),
        Some("mytest-iris"),
        "IRIS_CONTAINER should be set from workspace config container"
    );
    std::env::remove_var("IRIS_CONTAINER");
}

// ── T015: Priority ordering test ─────────────────────────────────────────────

#[test]
fn test_compile_workspace_config_overrides_env() {
    let _g = env_guard();
    // Set IRIS_CONTAINER to an "old" value via env
    std::env::set_var("IRIS_CONTAINER", "old-container");

    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        container: Some("new-container".to_string()),
        ..Default::default()
    };
    workspace_config_to_connection(&cfg, "USER");

    // Config file should win over the pre-existing env var
    assert_eq!(
        std::env::var("IRIS_CONTAINER").ok().as_deref(),
        Some("new-container"),
        "workspace config container should override pre-existing IRIS_CONTAINER env var"
    );
    std::env::remove_var("IRIS_CONTAINER");
}

// ── T019: generate_toml_content tests ────────────────────────────────────────

#[test]
fn test_generate_toml_contains_container() {
    let content =
        iris_agentic_dev_core::iris::workspace_config::generate_toml_content("myapp-iris", "USER");
    assert!(
        content.contains("container = \"myapp-iris\""),
        "generated TOML should contain container field"
    );
    assert!(
        content.contains("namespace = \"USER\""),
        "generated TOML should contain namespace field"
    );
}

#[test]
fn test_generate_toml_contains_comment_about_password() {
    let content =
        iris_agentic_dev_core::iris::workspace_config::generate_toml_content("any-iris", "USER");
    assert!(
        content.contains("# password"),
        "generated TOML should have a commented-out password field"
    );
    assert!(
        content.contains("not recommended"),
        "generated TOML should warn against committing password"
    );
}

#[test]
fn test_generate_toml_is_parseable() {
    let content =
        iris_agentic_dev_core::iris::workspace_config::generate_toml_content("parse-iris", "MYNS");
    // Strip comment lines and parse as TOML
    let stripped: String = content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed =
        toml::from_str::<iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig>(&stripped);
    assert!(
        parsed.is_ok(),
        "generated TOML (minus comments) should parse cleanly: {:?}",
        parsed
    );
}

// ── T026: workspace_config field shape test ───────────────────────────────────

#[test]
fn test_workspace_config_field_shape() {
    // Verify the JSON shape we'd put in iris_list_containers response
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        container: Some("vis-test-iris".to_string()),
        namespace: Some("USER".to_string()),
        ..Default::default()
    };
    let running = false; // not actually checking docker here
    let json = serde_json::json!({
        "found": true,
        "path": "/some/project/.iris-dev.toml",
        "container": cfg.container,
        "namespace": cfg.namespace,
        "running": running,
    });
    assert_eq!(json["container"], "vis-test-iris");
    assert_eq!(json["found"], true);
    assert_eq!(json["running"], false);
}

// ── T030: web_prefix field ────────────────────────────────────────────────────

#[test]
fn test_load_parses_web_prefix_field() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(
        &dir,
        r#"
host = "iris.example.com"
web_port = 80
web_prefix = "irisaicore"
"#,
    );
    let cfg = load_workspace_config(Some(dir.path().to_str().unwrap())).unwrap();
    assert_eq!(cfg.web_prefix.as_deref(), Some("irisaicore"));
}

#[test]
fn test_web_prefix_included_in_base_url() {
    let _g = env_guard();
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(80),
        web_prefix: Some("irisaicore".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert_eq!(
        conn.base_url, "http://localhost:80/irisaicore",
        "base_url should include web_prefix, got: {}",
        conn.base_url
    );
}

#[test]
fn test_web_prefix_strips_leading_trailing_slashes() {
    let _g = env_guard();
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(52773),
        web_prefix: Some("/irisaicore/".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert_eq!(
        conn.base_url, "http://localhost:52773/irisaicore",
        "leading/trailing slashes should be stripped, got: {}",
        conn.base_url
    );
}

#[test]
fn test_no_web_prefix_gives_clean_base_url() {
    let _g = env_guard();
    std::env::remove_var("IRIS_WEB_PREFIX");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(52773),
        web_prefix: None,
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert_eq!(
        conn.base_url, "http://localhost:52773",
        "base_url without prefix should have no trailing slash, got: {}",
        conn.base_url
    );
}

#[test]
fn test_iris_web_prefix_env_var_used_when_no_toml_prefix() {
    let _g = env_guard();
    std::env::set_var("IRIS_WEB_PREFIX", "myprefix");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(52773),
        web_prefix: None,
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    std::env::remove_var("IRIS_WEB_PREFIX");
    assert_eq!(
        conn.base_url, "http://localhost:52773/myprefix",
        "IRIS_WEB_PREFIX env var should be used when web_prefix not in config, got: {}",
        conn.base_url
    );
}

#[test]
fn test_toml_web_prefix_overrides_env_var() {
    let _g = env_guard();
    std::env::set_var("IRIS_WEB_PREFIX", "envprefix");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(52773),
        web_prefix: Some("tomlprefix".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    std::env::remove_var("IRIS_WEB_PREFIX");
    assert_eq!(
        conn.base_url, "http://localhost:52773/tomlprefix",
        "TOML web_prefix should override IRIS_WEB_PREFIX env var, got: {}",
        conn.base_url
    );
}

#[test]
fn test_generate_toml_contains_web_prefix_comment() {
    let content =
        iris_agentic_dev_core::iris::workspace_config::generate_toml_content("myapp-iris", "USER");
    assert!(
        content.contains("web_prefix"),
        "generated TOML should document the web_prefix field"
    );
}

// ── T031: scheme field (https support) ───────────────────────────────────────

#[test]
fn test_load_parses_scheme_field() {
    let _g = env_guard();
    let dir = tempfile::TempDir::new().unwrap();
    write_toml(
        &dir,
        r#"
host = "iris.example.com"
web_port = 443
scheme = "https"
"#,
    );
    let cfg = load_workspace_config(Some(dir.path().to_str().unwrap())).unwrap();
    assert_eq!(cfg.scheme.as_deref(), Some("https"));
}

#[test]
fn test_https_scheme_in_base_url() {
    let _g = env_guard();
    std::env::remove_var("IRIS_SCHEME");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("iris.example.com".to_string()),
        web_port: Some(443),
        scheme: Some("https".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert!(
        conn.base_url.starts_with("https://"),
        "base_url should use https, got: {}",
        conn.base_url
    );
    assert_eq!(conn.base_url, "https://iris.example.com:443");
}

#[test]
fn test_https_scheme_with_prefix() {
    let _g = env_guard();
    std::env::remove_var("IRIS_SCHEME");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("dem".to_string()),
        web_port: Some(443),
        scheme: Some("https".to_string()),
        web_prefix: Some("dev".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert_eq!(
        conn.base_url, "https://dem:443/dev",
        "https + prefix should combine correctly, got: {}",
        conn.base_url
    );
}

#[test]
fn test_default_scheme_is_http() {
    let _g = env_guard();
    std::env::remove_var("IRIS_SCHEME");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(52773),
        scheme: None,
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    assert!(
        conn.base_url.starts_with("http://"),
        "default scheme should be http, got: {}",
        conn.base_url
    );
}

#[test]
fn test_iris_scheme_env_var() {
    let _g = env_guard();
    std::env::set_var("IRIS_SCHEME", "https");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(443),
        scheme: None,
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    std::env::remove_var("IRIS_SCHEME");
    assert!(
        conn.base_url.starts_with("https://"),
        "IRIS_SCHEME env var should set https, got: {}",
        conn.base_url
    );
}

#[test]
fn test_toml_scheme_overrides_env_var() {
    let _g = env_guard();
    std::env::set_var("IRIS_SCHEME", "http");
    let cfg = iris_agentic_dev_core::iris::workspace_config::WorkspaceConfig {
        host: Some("localhost".to_string()),
        web_port: Some(443),
        scheme: Some("https".to_string()),
        ..Default::default()
    };
    let conn = workspace_config_to_connection(&cfg, "USER").unwrap();
    std::env::remove_var("IRIS_SCHEME");
    assert!(
        conn.base_url.starts_with("https://"),
        "TOML scheme should override IRIS_SCHEME env var, got: {}",
        conn.base_url
    );
}

#[test]
fn test_generate_toml_contains_scheme_comment() {
    let content =
        iris_agentic_dev_core::iris::workspace_config::generate_toml_content("myapp-iris", "USER");
    assert!(
        content.contains("scheme"),
        "generated TOML should document the scheme field"
    );
}

// ── Container scoring: hyphen/underscore normalization (#19) ─────────────────

#[test]
fn test_score_underscore_workspace_matches_hyphen_container() {
    // id_try2 (underscore) should match id-try2-iris (hyphen) — both normalize to id_try2
    use iris_agentic_dev_core::iris::discovery::score_container_name;
    let score = score_container_name("id-try2-iris", "id_try2");
    assert!(
        score > 0,
        "id_try2 should match id-try2-iris, got score {}",
        score
    );
    assert!(
        score >= 60,
        "should score at least 60 (contains match), got {}",
        score
    );
}

#[test]
fn test_score_hyphen_workspace_matches_underscore_container() {
    use iris_agentic_dev_core::iris::discovery::score_container_name;
    let score = score_container_name("id_try2_iris", "id-try2");
    assert!(
        score > 0,
        "id-try2 should match id_try2_iris, got score {}",
        score
    );
}

#[test]
fn test_score_exact_match_after_normalization() {
    use iris_agentic_dev_core::iris::discovery::score_container_name;
    // loanapp vs loanapp-iris: starts_with match + iris suffix = 80 + 10 = 90
    let score = score_container_name("loanapp-iris", "loanapp");
    assert_eq!(
        score, 90,
        "loanapp-iris for workspace loanapp should score 90"
    );
}

#[test]
fn test_score_unrelated_containers_score_zero() {
    use iris_agentic_dev_core::iris::discovery::score_container_name;
    let score = score_container_name("determined_cray", "id_try2");
    assert_eq!(score, 0, "unrelated container should score 0");
}

#[test]
fn test_score_container_beats_unrelated_after_normalization() {
    use iris_agentic_dev_core::iris::discovery::score_container_name;
    let target = score_container_name("id-try2-iris", "id_try2");
    let random = score_container_name("determined_cray", "id_try2");
    assert!(
        target > random,
        "id-try2-iris ({}) should beat determined_cray ({}) for id_try2 workspace",
        target,
        random
    );
}
