//! T010: Unit tests for IRIS discovery cascade.
//! Tests written FIRST — must fail before implementation is complete.
//!
//! These tests exercise: probe_atelier fingerprinting, cascade ordering,
//! graceful fallthrough when localhost probe fails, env var resolution.

use iris_agentic_dev_core::iris::discovery::{discover_iris, probe_atelier, IrisDiscovery};

// ── probe_atelier ────────────────────────────────────────────────────────────

/// A port that is not IRIS returns None.
///
/// This was named `probe_atelier_returns_connection_on_iris_response` and documented as "a
/// reachable IRIS endpoint returns Some(IrisConnection)" — while asserting only `is_none()` on port
/// 9999. A `probe_atelier` that returned None for EVERYTHING passed it. Its comment also said "since
/// we don't have wiremock yet"; wiremock has been a dev-dependency for some time, so the positive
/// half is tested below and this one now claims only what it checks.
#[tokio::test]
async fn probe_atelier_returns_none_for_a_non_iris_port() {
    let result = probe_atelier("127.0.0.1", 9999, "_SYSTEM", "SYS", "USER", 100).await;
    assert!(result.is_none(), "Non-IRIS port should return None");
}

/// Serve an Atelier root descriptor and hand back (host, port) for `probe_atelier`.
async fn atelier_root(body: serde_json::Value, status: u16) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/atelier/"))
        .respond_with(wiremock::ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn host_port(server: &wiremock::MockServer) -> (String, u16) {
    let addr = server.address();
    (addr.ip().to_string(), addr.port())
}

fn iris_root(version: &str, api: u64) -> serde_json::Value {
    serde_json::json!({"result": {"content": {"version": version, "api": api}}})
}

/// THE POSITIVE HALF, which had no coverage: a root descriptor that fingerprints as IRIS yields a
/// connection, and the fields taken off that descriptor are the ones the caller relies on.
#[tokio::test]
async fn probe_atelier_returns_a_connection_when_the_root_fingerprints_as_iris() {
    let server = atelier_root(iris_root("IRIS for UNIX 2026.1", 8), 200).await;
    let (host, port) = host_port(&server);
    let conn = probe_atelier(&host, port, "_SYSTEM", "SYS", "APP", 2000)
        .await
        .expect("a root descriptor naming IRIS must yield a connection");
    assert_eq!(conn.version.as_deref(), Some("IRIS for UNIX 2026.1"));
    assert_eq!(conn.base_url, format!("http://{host}:{port}"));
    assert_eq!(conn.namespace, "APP");
}

/// The fingerprint is the WHOLE point: a 200 with a descriptor that does not name IRIS must not be
/// adopted. Without this, "returns Some on 200" would pass and the probe would claim any web server.
#[tokio::test]
async fn probe_atelier_rejects_a_root_that_does_not_name_iris() {
    let server = atelier_root(iris_root("Cache for Windows 2018.1", 2), 200).await;
    let (host, port) = host_port(&server);
    assert!(
        probe_atelier(&host, port, "_SYSTEM", "SYS", "USER", 2000)
            .await
            .is_none(),
        "a 200 from something that is not IRIS must not be adopted"
    );
}

/// A descriptor with no `version` at all is not IRIS either — `.as_str()` on a missing field.
#[tokio::test]
async fn probe_atelier_rejects_a_root_with_no_version() {
    let server = atelier_root(serde_json::json!({"result": {"content": {"api": 8}}}), 200).await;
    let (host, port) = host_port(&server);
    assert!(probe_atelier(&host, port, "_SYSTEM", "SYS", "USER", 2000)
        .await
        .is_none());
}

/// 401 and 5xx are refusals, not adoptions. 401 has its own branch (#21, the OS-auth container), so
/// it is worth asserting separately from the generic non-success path.
#[tokio::test]
async fn probe_atelier_rejects_401_and_5xx() {
    for status in [401_u16, 500] {
        let server = atelier_root(iris_root("IRIS for UNIX 2026.1", 8), status).await;
        let (host, port) = host_port(&server);
        assert!(
            probe_atelier(&host, port, "_SYSTEM", "SYS", "USER", 2000)
                .await
                .is_none(),
            "HTTP {status} must not yield a connection even with an IRIS-looking body"
        );
    }
}

/// The `api` level decides which Atelier URL shape every later request uses, so the mapping is part
/// of the contract: >=8 is V8, >=2 is V2, anything else V1.
#[tokio::test]
async fn probe_atelier_maps_the_api_level_to_an_atelier_version() {
    use iris_agentic_dev_core::iris::connection::AtelierVersion;
    for (api, want) in [
        (8_u64, AtelierVersion::V8),
        (9, AtelierVersion::V8),
        (2, AtelierVersion::V2),
        (7, AtelierVersion::V2),
        (1, AtelierVersion::V1),
    ] {
        let server = atelier_root(iris_root("IRIS for UNIX 2026.1", api), 200).await;
        let (host, port) = host_port(&server);
        let conn = probe_atelier(&host, port, "_SYSTEM", "SYS", "USER", 2000)
            .await
            .expect("fingerprints as IRIS");
        assert_eq!(conn.atelier_version, want, "api {api} mapped wrongly");
    }
}

/// probe_atelier respects the timeout — 100ms must not block longer than 250ms.
#[tokio::test]
async fn probe_atelier_respects_timeout() {
    let start = std::time::Instant::now();
    let _result = probe_atelier("10.255.255.1", 52773, "_SYSTEM", "SYS", "USER", 100).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "probe_atelier took {}ms, expected <500ms with 100ms timeout",
        elapsed.as_millis()
    );
}

// ── discover_iris cascade ────────────────────────────────────────────────────

/// When IRIS_HOST + IRIS_WEB_PORT env vars are set and valid, discover_iris
/// should attempt to connect (and fail gracefully if not reachable).
#[tokio::test]
async fn discover_iris_reads_env_vars() {
    // Set env vars to a non-existent host
    std::env::set_var("IRIS_HOST", "nonexistent.invalid");
    std::env::set_var("IRIS_WEB_PORT", "52773");
    std::env::set_var("IRIS_USERNAME", "testuser");
    std::env::set_var("IRIS_PASSWORD", "testpass");

    let result = discover_iris(None).await;
    // Env vars found but host unreachable — should return NotFound, not panic
    assert!(
        !matches!(result, IrisDiscovery::Found(_)),
        "unreachable host should not return Found"
    );

    // Clean up
    std::env::remove_var("IRIS_HOST");
    std::env::remove_var("IRIS_WEB_PORT");
    std::env::remove_var("IRIS_USERNAME");
    std::env::remove_var("IRIS_PASSWORD");
}

/// Without any config, discover_iris returns Ok(None) — not an error.
#[tokio::test]
async fn discover_iris_returns_none_when_nothing_found() {
    // Ensure no env vars interfere
    std::env::remove_var("IRIS_HOST");
    std::env::remove_var("IRIS_WEB_PORT");

    // With no IRIS running and no config, should return NotFound (not panic)
    let result = discover_iris(None).await;
    assert!(
        matches!(result, IrisDiscovery::NotFound | IrisDiscovery::Explained),
        "discover_iris should return NotFound or Explained when nothing found"
    );
}

/// Explicit connection passed to discover_iris is returned immediately without scanning.
#[tokio::test]
async fn discover_iris_explicit_wins_immediately() {
    use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};

    let explicit = IrisConnection::new(
        "http://explicit.example.com:52773",
        "MYNS",
        "admin",
        "secret",
        DiscoverySource::ExplicitFlag,
    );

    let result = discover_iris(Some(explicit)).await;
    let conn = match result {
        IrisDiscovery::Found(c) => c,
        other => panic!("expected Found, got {:?}", other),
    };
    assert_eq!(conn.base_url, "http://explicit.example.com:52773");
    assert_eq!(conn.namespace, "MYNS");
    assert!(matches!(conn.source, DiscoverySource::ExplicitFlag));
}

// ── IrisConnection ────────────────────────────────────────────────────────────

#[test]
fn iris_connection_atelier_url_format() {
    use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};

    let conn = IrisConnection::new(
        "http://localhost:52773",
        "USER",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );

    assert_eq!(
        conn.atelier_url("/v1/USER/action/query"),
        "http://localhost:52773/api/atelier/v1/USER/action/query"
    );
    assert_eq!(conn.atelier_url("/"), "http://localhost:52773/api/atelier/");
}
