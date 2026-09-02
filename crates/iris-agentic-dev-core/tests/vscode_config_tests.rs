//! T027: Unit tests for VS Code settings.json parsing.
//! Tests the vscode_config module for named server resolution.

use iris_agentic_dev_core::iris::vscode_config::parse_vscode_settings;

fn write_settings(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("settings.json");
    std::fs::write(&path, content).unwrap();
    path
}

/// Parse settings.json with a direct host/port connection.
#[test]
fn parse_direct_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(
        dir.path(),
        r#"{
        "objectscript.conn": {
            "active": true,
            "host": "localhost",
            "port": 52773,
            "username": "_SYSTEM",
            "password": "SYS",
            "ns": "USER"
        }
    }"#,
    );

    let settings = parse_vscode_settings(&path).expect("should parse direct connection");
    let conn = settings
        .objectscript_conn
        .expect("objectscript.conn should be present");

    assert_eq!(conn.host.as_deref(), Some("localhost"));
    assert_eq!(conn.port, Some(52773));
    assert_eq!(conn.username.as_deref(), Some("_SYSTEM"));
    assert_eq!(conn.ns.as_deref(), Some("USER"));
    assert!(conn.server.is_none());
}

/// Parse settings.json with a named server — resolves superServer.port.
#[test]
fn parse_named_server_with_super_server_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(
        dir.path(),
        r#"{
        "objectscript.conn": {
            "active": true,
            "server": "opsreview-iris",
            "ns": "USER"
        },
        "intersystems.servers": {
            "opsreview-iris": {
                "webServer": {
                    "scheme": "http",
                    "host": "localhost",
                    "port": 52773
                },
                "superServer": { "port": 1972 },
                "username": "_SYSTEM"
            }
        }
    }"#,
    );

    let settings = parse_vscode_settings(&path).expect("should parse named server");
    let conn = settings
        .objectscript_conn
        .expect("objectscript.conn present");
    assert_eq!(conn.server.as_deref(), Some("opsreview-iris"));

    let servers = settings
        .intersystems_servers
        .expect("intersystems.servers present");
    let server = servers
        .get("opsreview-iris")
        .expect("opsreview-iris server present");
    assert_eq!(server.web_server.host.as_deref(), Some("localhost"));
    assert_eq!(server.web_server.port, Some(52773));
    assert_eq!(server.super_server_port(), Some(1972));
}

/// Named server without superServer.port returns None for super_server_port.
#[test]
fn named_server_without_super_server_port_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(
        dir.path(),
        r#"{
        "objectscript.conn": {"active": true, "server": "myserver"},
        "intersystems.servers": {
            "myserver": {
                "webServer": {"host": "myiris.example.com", "port": 52773},
                "username": "admin"
            }
        }
    }"#,
    );

    let settings = parse_vscode_settings(&path).unwrap();
    let servers = settings.intersystems_servers.unwrap();
    let server = servers.get("myserver").unwrap();
    assert!(
        server.super_server_port().is_none(),
        "superServer absent should return None for super_server_port"
    );
}

/// Active=false connection is respected.
#[test]
fn inactive_connection_is_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(
        dir.path(),
        r#"{
        "objectscript.conn": {
            "active": false,
            "host": "localhost",
            "port": 52773
        }
    }"#,
    );

    let settings = parse_vscode_settings(&path).unwrap();
    let conn = settings.objectscript_conn.unwrap();
    assert_eq!(conn.active, Some(false));
}

/// Missing objectscript.conn is Ok with None.
#[test]
fn settings_without_objectscript_conn_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(dir.path(), r#"{"editor.fontSize": 14}"#);
    let settings = parse_vscode_settings(&path).unwrap();
    assert!(settings.objectscript_conn.is_none());
}

// ── #187: resolution, not just parsing ───────────────────────────────────────
//
// Everything above tests that the FIELDS parse. These test what the parsed
// settings resolve TO, which is where the defect lived: a Server Manager entry
// keeps its password in the OS keychain, so it is absent from settings.json, and
// `unwrap_or("SYS")` turned that absence into a confident wrong connection —
// a 401 naming no cause. `resolve_with` takes the environment fallback as an
// argument so these stay pure and cannot race on process env.

use iris_agentic_dev_core::iris::vscode_config::VsCodeResolution;

const KEYCHAIN_SERVER: &str = r#"{
    "objectscript.conn": {"active": true, "server": "workshop-iris", "ns": "IRISAPP"},
    "intersystems.servers": {
        "workshop-iris": {
            "webServer": {"scheme": "http", "host": "localhost", "port": 52773},
            "username": "_SYSTEM"
        }
    }
}"#;

fn resolve(content: &str, env_password: Option<&str>) -> VsCodeResolution {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(dir.path(), content);
    parse_vscode_settings(&path)
        .unwrap()
        .resolve_with(env_password)
}

/// The #187 defect itself: a named server whose password lives in the keychain
/// must NOT come back as a connection carrying "SYS".
#[test]
fn named_server_without_a_password_is_not_handed_sys() {
    match resolve(KEYCHAIN_SERVER, None) {
        VsCodeResolution::MissingPassword { server } => {
            assert_eq!(server.as_deref(), Some("workshop-iris"));
        }
        VsCodeResolution::Resolved(conn) => panic!(
            "resolved with a fabricated password {:?} — this is the 401 that names no cause",
            conn.password
        ),
        VsCodeResolution::NotConfigured => panic!("a configured server read as NotConfigured"),
    }
}

/// Same for the direct host/port form, which had its own copy of the default.
#[test]
fn direct_connection_without_a_password_is_not_handed_sys() {
    let settings = r#"{"objectscript.conn": {"active": true, "host": "localhost", "port": 52773}}"#;
    match resolve(settings, None) {
        VsCodeResolution::MissingPassword { server } => assert!(server.is_none()),
        VsCodeResolution::Resolved(conn) => {
            panic!("resolved with a fabricated password {:?}", conn.password)
        }
        VsCodeResolution::NotConfigured => panic!("a configured connection read as NotConfigured"),
    }
}

/// An empty string is an absent password, not a password of length zero.
#[test]
fn an_empty_password_counts_as_missing() {
    let settings = r#"{"objectscript.conn": {"active": true, "host": "h", "password": ""}}"#;
    assert!(matches!(
        resolve(settings, None),
        VsCodeResolution::MissingPassword { .. }
    ));
}

/// The intended composition: VS Code supplies host/port/namespace, the
/// environment supplies the secret that lives in the keychain.
#[test]
fn iris_password_from_the_environment_completes_a_keychain_server() {
    match resolve(KEYCHAIN_SERVER, Some("from-the-env")) {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(conn.password, "from-the-env");
            assert_eq!(conn.username, "_SYSTEM");
            assert_eq!(conn.namespace, "IRISAPP");
            assert_eq!(conn.base_url, "http://localhost:52773");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// A password written in settings.json wins over the environment fallback.
#[test]
fn a_password_in_settings_beats_the_environment() {
    let settings = r#"{
        "objectscript.conn": {"active": true, "server": "s"},
        "intersystems.servers": {
            "s": {"webServer": {"host": "localhost", "port": 52773}, "password": "inline"}
        }
    }"#;
    match resolve(settings, Some("from-the-env")) {
        VsCodeResolution::Resolved(conn) => assert_eq!(conn.password, "inline"),
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// active:false means "do not use this", not "use it with defaults".
#[test]
fn inactive_connection_resolves_to_not_configured() {
    let settings = r#"{"objectscript.conn": {"active": false, "host": "h", "password": "p"}}"#;
    assert!(matches!(
        resolve(settings, None),
        VsCodeResolution::NotConfigured
    ));
}

/// A server: name with no matching entry is a typo, not a connection to localhost.
#[test]
fn a_named_server_with_no_matching_entry_is_not_configured() {
    let settings = r#"{
        "objectscript.conn": {"active": true, "server": "ghost"},
        "intersystems.servers": {"other": {"webServer": {"host": "h"}, "password": "p"}}
    }"#;
    assert!(matches!(
        resolve(settings, None),
        VsCodeResolution::NotConfigured
    ));
}

/// pathPrefix still lands in the base URL (regression guard for the rewrite).
#[test]
fn path_prefix_survives_resolution() {
    let settings = r#"{
        "objectscript.conn": {"active": true, "server": "s"},
        "intersystems.servers": {
            "s": {
                "webServer": {"scheme": "https", "host": "iris.example.com", "port": 443,
                              "pathPrefix": "/gateway/"},
                "password": "p"
            }
        }
    }"#;
    match resolve(settings, None) {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(conn.base_url, "https://iris.example.com:443/gateway");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}
