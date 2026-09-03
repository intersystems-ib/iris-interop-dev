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

use iris_agentic_dev_core::iris::vscode_config::{
    parse_code_workspace, VsCodeResolution, VsCodeSettings,
};

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

/// The shape that actually exists in the wild, found by the SKILLS session in three
/// workshop directories: `active: true` and a `server:` name, with NO
/// `intersystems.servers` map at all — because Server Manager keeps the server
/// definition in USER-scope settings, which this binary does not read.
///
/// `a_named_server_with_no_matching_entry_is_not_configured` covers the neighbouring
/// case (map present, key absent). This one takes a different branch —
/// `intersystems_servers.as_ref()` yields `None` before `.get()` is ever reached — and
/// the two must not be assumed to agree just because they should.
///
/// It must resolve to NotConfigured so discovery falls THROUGH to the scans. Anything
/// else would make 0.17.0 break every workshop directory: under the new precedence this
/// entry is reached at step 4, ahead of the port scan that currently serves it.
#[test]
fn a_server_name_with_no_servers_map_at_all_is_not_configured() {
    let settings = r#"{
        "objectscript.conn": { "active": true, "server": "workshop-iris", "ns": "HOSPITAL" },
        "objectscript.export": { "folder": "src", "atelier": true }
    }"#;
    match resolve(settings, None) {
        VsCodeResolution::NotConfigured => {}
        VsCodeResolution::Resolved(conn) => panic!(
            "guessed a connection to {} for a server this binary cannot resolve — \
             discovery would stop here instead of falling through to the scans",
            conn.base_url
        ),
        VsCodeResolution::MissingPassword { .. } => panic!(
            "reported a missing password for a server that was never resolved — \
             the warning would name a cause that is not the real one"
        ),
    }
}

// ── #187 (2): user-scope settings ────────────────────────────────────────────
//
// The workshop shape is split across TWO files by design. Server Manager writes
// `intersystems.servers` into USER-scope settings; the workspace `.vscode/settings.json`
// only references the server by name. Reading either file alone yields NotConfigured,
// which is why adding the user path as another loop candidate would not have fixed
// anything — the two scopes have to be MERGED before resolution.

fn parse(content: &str) -> VsCodeSettings {
    let dir = tempfile::tempdir().unwrap();
    let path = write_settings(dir.path(), content);
    parse_vscode_settings(&path).unwrap()
}

/// Exactly what a student has after the workshop VM is set up: the workspace names
/// `workshop-iris`, Server Manager defines it in user scope, the password is in the
/// keychain and supplied by the environment. Host and port come from user scope, the
/// namespace from the workspace — which is the whole point of the merge.
#[test]
fn the_workshop_shape_resolves_once_user_scope_is_merged() {
    let workspace = parse(
        r#"{"objectscript.conn": {"active": true, "server": "workshop-iris", "ns": "HOSPITAL"}}"#,
    );
    let user = parse(
        r#"{"intersystems.servers": {
              "workshop-iris": {
                "webServer": {"scheme": "http", "host": "iris.workshop.local", "port": 52773},
                "username": "alumno"
              }
            }}"#,
    );
    match workspace
        .overlay_on(user)
        .resolve_with(Some("from-the-env"))
    {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(conn.base_url, "http://iris.workshop.local:52773");
            assert_eq!(conn.namespace, "HOSPITAL");
            assert_eq!(conn.username, "alumno");
            assert_eq!(conn.password, "from-the-env");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// Without the env password it must name the server rather than fall silent — the
/// student gets told which entry needs IRIS_PASSWORD instead of an unexplained 401.
#[test]
fn a_merged_server_with_no_password_names_itself() {
    let workspace = parse(r#"{"objectscript.conn": {"active": true, "server": "workshop-iris"}}"#);
    let user = parse(
        r#"{"intersystems.servers": {"workshop-iris": {"webServer": {"host": "h", "port": 52773}}}}"#,
    );
    match workspace.overlay_on(user).resolve_with(None) {
        VsCodeResolution::MissingPassword { server } => {
            assert_eq!(server.as_deref(), Some("workshop-iris"))
        }
        other => panic!("expected MissingPassword, got {other:?}"),
    }
}

/// Workspace wins key-by-key, the way VS Code resolves scopes.
#[test]
fn a_workspace_server_entry_overrides_the_user_one_of_the_same_name() {
    let workspace = parse(
        r#"{"objectscript.conn": {"active": true, "server": "iris"},
            "intersystems.servers": {"iris": {"webServer": {"host": "workspace.example", "port": 443}, "password": "p"}}}"#,
    );
    let user = parse(
        r#"{"intersystems.servers": {"iris": {"webServer": {"host": "user.example", "port": 52773}, "password": "p"}}}"#,
    );
    match workspace.overlay_on(user).resolve_with(None) {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(conn.base_url, "http://workspace.example:443")
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// A user-scope entry the workspace does not mention stays available.
#[test]
fn user_scope_entries_survive_the_overlay() {
    let workspace = parse(
        r#"{"objectscript.conn": {"active": true, "server": "other"},
            "intersystems.servers": {"iris": {"webServer": {"host": "workspace.example"}, "password": "p"}}}"#,
    );
    let user = parse(
        r#"{"intersystems.servers": {"other": {"webServer": {"host": "user.example", "port": 52773}, "password": "p"}}}"#,
    );
    match workspace.overlay_on(user).resolve_with(None) {
        VsCodeResolution::Resolved(conn) => assert_eq!(conn.base_url, "http://user.example:52773"),
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// A workspace with no conn of its own falls back to the user-scope one.
#[test]
fn a_user_scope_conn_is_used_when_the_workspace_has_none() {
    let workspace = parse(r#"{"editor.fontSize": 14}"#);
    let user = parse(
        r#"{"objectscript.conn": {"active": true, "host": "user.example", "port": 52773, "password": "p", "ns": "USER"}}"#,
    );
    match workspace.overlay_on(user).resolve_with(None) {
        VsCodeResolution::Resolved(conn) => assert_eq!(conn.base_url, "http://user.example:52773"),
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// `active: false` in the workspace opts out even when user scope would connect —
/// otherwise the opt-out documented in the README would be silently overridable.
#[test]
fn a_workspace_opt_out_is_not_undone_by_user_scope() {
    let workspace =
        parse(r#"{"objectscript.conn": {"active": false, "host": "h", "password": "p"}}"#);
    let user = parse(
        r#"{"objectscript.conn": {"active": true, "host": "user.example", "port": 52773, "password": "p"}}"#,
    );
    assert!(matches!(
        workspace.overlay_on(user).resolve_with(None),
        VsCodeResolution::NotConfigured
    ));
}

/// Two empty scopes are still nothing — no accidental localhost default.
#[test]
fn merging_two_unconfigured_scopes_is_still_unconfigured() {
    assert!(matches!(
        parse(r#"{"editor.fontSize": 14}"#)
            .overlay_on(parse(r#"{"telemetry.telemetryLevel": "off"}"#))
            .resolve_with(None),
        VsCodeResolution::NotConfigured
    ));
}

// ── #187 (2b): the .code-workspace scope, and the namespace guardrail ────────
//
// Verbatim from the workshop's Alumno.code-workspace. The server is defined ONCE
// here and referenced by name from each exercise folder — so neither user scope nor
// .vscode/settings.json carries it, and a reader that does not walk up finds a name
// it cannot resolve. Note port 80 + pathPrefix: a Web Gateway, which no port scan of
// 52773 would ever have found.
const WORKSHOP_CODE_WORKSPACE: &str = r#"{
  "folders": [ { "name": "Hospital", "path": "./Ejercicios/Hospital" } ],
  "settings": {
    "intersystems.servers": {
      "workshop-iris": {
        "webServer": { "scheme": "http", "host": "localhost", "port": 80, "pathPrefix": "/irishealth" },
        "username": "_SYSTEM",
        "password": "SYS",
        "description": "Workshop IRIS for Health instance (local)"
      }
    },
    "files.associations": { "*.cls": "objectscript-class" }
  },
  "extensions": { "recommendations": [ "intersystems-community.servermanager" ] }
}"#;

fn parse_ws(content: &str) -> VsCodeSettings {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Alumno.code-workspace");
    std::fs::write(&path, content).unwrap();
    parse_code_workspace(&path).unwrap()
}

/// The settings live under a nested `settings` key; the surrounding `folders` and
/// `extensions` must not derail the parse.
#[test]
fn a_code_workspace_yields_the_settings_nested_inside_it() {
    let s = parse_ws(WORKSHOP_CODE_WORKSPACE);
    let servers = s
        .intersystems_servers
        .expect("intersystems.servers from the .code-workspace settings block");
    let ws = servers.get("workshop-iris").expect("workshop-iris defined");
    assert_eq!(ws.web_server.port, Some(80));
    assert_eq!(ws.web_server.path_prefix.as_deref(), Some("/irishealth"));
}

/// End to end for a student: folder names the server, .code-workspace defines it.
/// The Web Gateway URL is the proof — no scan produces port 80 with a path prefix.
#[test]
fn the_student_layout_resolves_across_folder_and_code_workspace() {
    let folder = parse(
        r#"{"objectscript.conn": {"active": true, "server": "workshop-iris", "ns": "HOSPITAL"}}"#,
    );
    match folder
        .overlay_on(parse_ws(WORKSHOP_CODE_WORKSPACE))
        .resolve_with(None)
    {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(conn.base_url, "http://localhost:80/irishealth");
            assert_eq!(conn.username, "_SYSTEM");
            assert_eq!(conn.namespace, "HOSPITAL");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// THE GUARDRAIL. A workshop pins IRIS_NAMESPACE=DONOTUSE on purpose: a read-only
/// namespace so a write that forgets to name its namespace fails at once instead of
/// landing somewhere real. Taking `ns` from the editor would hand back a namespace
/// that WORKS and delete that guardrail silently — 95% of namespace-omitted calls
/// currently fail loudly, which is the point.
#[test]
fn the_operator_namespace_overrides_the_one_the_editor_configured() {
    let folder = parse(
        r#"{"objectscript.conn": {"active": true, "server": "workshop-iris", "ns": "HOSPITAL"}}"#,
    );
    match folder
        .overlay_on(parse_ws(WORKSHOP_CODE_WORKSPACE))
        .resolve_with_env(None, Some("DONOTUSE"))
    {
        VsCodeResolution::Resolved(conn) => {
            assert_eq!(
                conn.namespace, "DONOTUSE",
                "the editor's ns silently replaced the operator's guardrail"
            );
            assert_eq!(conn.base_url, "http://localhost:80/irishealth");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// An unset IRIS_NAMESPACE must not blank the namespace out.
#[test]
fn no_operator_namespace_leaves_the_editors_ns_alone() {
    let folder =
        parse(r#"{"objectscript.conn": {"active": true, "server": "s", "ns": "HOSPITAL"}}"#);
    let ws =
        parse(r#"{"intersystems.servers": {"s": {"webServer": {"host": "h"}, "password": "p"}}}"#);
    match folder.overlay_on(ws).resolve_with_env(None, None) {
        VsCodeResolution::Resolved(conn) => assert_eq!(conn.namespace, "HOSPITAL"),
        other => panic!("expected Resolved, got {other:?}"),
    }
}
