//! #410: the workspace config file was ignored at startup when `IRIS_*` was set, and won as soon
//! as it was touched — so which IRIS a session reached depended on when you looked.
//!
//! Two defects with one cause, and the second is the dangerous one.
//!
//! ## The inversion
//!
//! `mcp.rs` declared `#[arg(long, env = "IRIS_HOST")]`, so **clap merged the environment variable
//! into the `--host` flag slot** before any connection was built. `resolve_connection` then
//! short-circuited on "an explicit flag was passed" and never opened the file. Because the server
//! is normally registered with `IRIS_HOST` set, that made the file invisible for the common
//! installation — the opposite of the precedence this tree states in five places
//! (`CLI flags > .iris-agentic-dev.toml > env vars > auto-discovery`), and the opposite of what
//! `mcp --help` itself used to list at rank 1. Meanwhile `discovery.rs` step 2 implemented the
//! documented order correctly and was unreachable, because clap had already consumed the variable.
//!
//! The fix is one removal — `env = "IRIS_HOST"` — in BOTH commands that build an explicit
//! connection from `--host` (`mcp` and `compile`; `compile` writes, so an env var silently
//! outranking the file is worse there).
//!
//! ## The provenance lie
//!
//! A file holding only `web_port = 45080` names no instance. The reload path ran the whole
//! discovery cascade and then stamped the result `ConnectionSource::ConfigFile` with the file's
//! path — reporting an instance found by a blind localhost scan, on a port named in neither the
//! file nor the environment, as if the file had chosen it. Setting the path also silenced both
//! `check_config` warnings, since `ignored_config_path` returns early when `config_file` is set.
//!
//! `names_a_target` is the discriminator the two paths lacked, and it cannot be replaced by
//! "did we manage to build a connection": `workspace_config_to_connection` answers `None` both
//! for `container = "x"` — a named target that discovery resolves — and for a file that chose
//! nothing at all.

use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
use iris_agentic_dev_core::iris::workspace_config::{
    apply_workspace_config_with_path, names_a_target, resolve_connection, ConfigOrigin,
    WorkspaceConfig,
};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

// ---------------------------------------------------------------- the discriminator

fn cfg_from(toml: &str) -> WorkspaceConfig {
    toml::from_str(toml).expect("fixture must parse")
}

#[test]
fn only_host_and_container_name_a_target() {
    // Every key that CHOOSES an instance, and every key that merely decorates one. The
    // decorating half is the measured case: `web_port = 45080` alone sent the cascade looking
    // and the answer was then attributed to this file.
    for names in [
        "host = \"h\"",
        "container = \"c\"",
        "host = \"h\"\ncontainer = \"c\"",
    ] {
        assert!(
            names_a_target(&cfg_from(names)),
            "this file chooses an instance: {names}"
        );
    }
    for decorates in [
        "web_port = 45080",
        "namespace = \"APP\"",
        "scheme = \"https\"",
        "web_prefix = \"irishealth\"",
        "username = \"u\"\npassword = \"p\"",
        "docker_only = true",
        "",
    ] {
        assert!(
            !names_a_target(&cfg_from(decorates)),
            "this file names NO instance, so the connection cannot be attributed to it: \
             {decorates:?}"
        );
    }
}

#[test]
fn the_three_origins_do_not_collapse_into_two() {
    let p = std::path::PathBuf::from("/proj/.iris-agentic-dev.toml");
    assert_eq!(
        ConfigOrigin::ConfigFile(p.clone()).provenance_path(),
        Some(p.as_path()),
        "the file chose the target, so it IS the provenance"
    );
    assert_eq!(
        ConfigOrigin::FileNamedNoTarget(p.clone()).provenance_path(),
        None,
        "the file exists and chose nothing — naming it here is the defect"
    );
    assert_eq!(ConfigOrigin::ExplicitFlag.provenance_path(), None);
    assert_eq!(ConfigOrigin::NoFile.provenance_path(), None);

    assert!(ConfigOrigin::ConfigFile(p.clone()).is_config_file());
    assert!(
        !ConfigOrigin::FileNamedNoTarget(p).is_config_file(),
        "a file that was READ is not a file that was FOLLOWED"
    );
}

// ---------------------------------------------------------------- startup resolution

fn workspace_with(toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".iris-agentic-dev.toml"), toml).unwrap();
    dir
}

#[test]
fn a_file_naming_only_a_port_is_read_but_is_not_the_provenance() {
    let dir = workspace_with("web_port = 45080\n");
    let ws = dir.path().to_str().unwrap();

    let (conn, origin) = resolve_connection(None, Some(ws), "USER").expect("a parseable file");
    assert!(
        conn.is_none(),
        "this file names no host and no container, so it builds no connection"
    );
    match &origin {
        ConfigOrigin::FileNamedNoTarget(p) => assert!(p.ends_with(".iris-agentic-dev.toml")),
        other => panic!("expected FileNamedNoTarget, got {other:?}"),
    }

    // The startup half of the same defect: this is what `check_config` reports as `config_file`,
    // and what `ConnectionSource::for_connection` keys on.
    let (_, path) = apply_workspace_config_with_path(None, Some(ws), "USER").unwrap();
    assert_eq!(
        path, None,
        "a file that chose nothing must not be reported as the connection's source"
    );
}

#[test]
fn a_file_naming_a_host_is_followed_and_reported() {
    // The positive control for the test above: the SAME call, one key different, must come back
    // with both a connection and a path — otherwise the assertion above would also pass against
    // a function that never reports a provenance at all.
    let dir = workspace_with("host = \"127.0.0.1\"\nweb_port = 45080\nnamespace = \"FILEWINS\"\n");
    let ws = dir.path().to_str().unwrap();

    let (conn, origin) = resolve_connection(None, Some(ws), "USER").expect("a parseable file");
    let conn = conn.expect("a file naming a host builds a connection");
    assert!(conn.base_url.contains("45080"), "{}", conn.base_url);
    assert_eq!(conn.namespace, "FILEWINS");
    assert!(origin.is_config_file(), "got {origin:?}");
    assert!(origin.provenance_path().is_some());

    let (_, path) = apply_workspace_config_with_path(None, Some(ws), "USER").unwrap();
    assert!(path.is_some(), "the file chose the target — report it");
}

#[test]
fn a_real_flag_still_bypasses_an_unusable_file() {
    // The escape hatch #410 must not cost: an explicit --host keeps working past a file that
    // cannot even be parsed. (`resolve_connection` returning `Err` here would make a broken file
    // fatal for someone who named their target on the command line.)
    let dir = workspace_with("this is not toml {{{\n");
    let flag = IrisConnection::new(
        "http://127.0.0.1:59999",
        "FLAGWINS",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );
    let (conn, origin) = resolve_connection(Some(flag), Some(dir.path().to_str().unwrap()), "USER")
        .expect("a broken file must not be fatal when the target was named on the command line");
    assert_eq!(
        conn.expect("the flag's connection survives").namespace,
        "FLAGWINS"
    );
    assert_eq!(origin, ConfigOrigin::ExplicitFlag);
    assert_eq!(origin.provenance_path(), None);
}

#[test]
fn no_file_is_not_an_ignored_file() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, origin) =
        resolve_connection(None, Some(dir.path().to_str().unwrap()), "USER").unwrap();
    assert!(conn.is_none());
    assert_eq!(origin, ConfigOrigin::NoFile);
}

// ---------------------------------------------------------------- the precedence, end to end

fn iris_dev_bin() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/debug/iris-interop-dev");
    path
}

/// `check_config` from a real server process. Deliberately NOT a self-skip when the binary is
/// missing: a test that spawns the binary and prints "skipping" reports `ok`, and this file's
/// whole subject is a false green. CI builds the binary before the test step.
fn check_config_of(args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let bin = iris_dev_bin();
    assert!(
        bin.exists(),
        "build the workspace first — these tests spawn {} (cargo build --workspace)",
        bin.display()
    );
    let mut cmd = Command::new(&bin);
    cmd.arg("mcp");
    for a in args {
        cmd.arg(a);
    }
    cmd.env_remove("IRIS_HOST")
        .env_remove("IRIS_WEB_PORT")
        .env_remove("IRIS_WEB_PREFIX")
        .env_remove("IRIS_SCHEME")
        .env_remove("IRIS_NAMESPACE")
        .env_remove("IRIS_CONTAINER")
        .env_remove("OBJECTSCRIPT_WORKSPACE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n";
    stdin.write_all(init.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"check_config\",\"arguments\":{}}}\n")
        .unwrap();
    stdin.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let _ = child.kill();
    let _ = child.wait(); // reap it — clippy::zombie_processes, and a leaked server holds the port

    let frame: serde_json::Value = serde_json::from_str(&line).expect("invalid JSON-RPC");
    let text = frame["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no check_config text in {frame}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("check_config is not JSON ({e}): {text}"))
}

/// The reported repro, step 3 — now inverted.
#[test]
fn a_workspace_file_outranks_iris_host() {
    let dir = workspace_with("host = \"127.0.0.1\"\nweb_port = 45080\nnamespace = \"FILEWINS\"\n");
    let cfg = check_config_of(
        &["--workspace", dir.path().to_str().unwrap()],
        &[
            ("IRIS_HOST", "127.0.0.1"),
            ("IRIS_WEB_PORT", "59999"),
            ("IRIS_NAMESPACE", "ENVWINS"),
        ],
    );
    assert_eq!(
        cfg["connection_source"], "config_file",
        "IRIS_HOST must no longer outrank the file: {cfg}"
    );
    assert_eq!(cfg["port"], 45080, "{cfg}");
    assert_eq!(cfg["namespace"], "FILEWINS", "{cfg}");
    assert!(
        cfg["config_file"].is_string(),
        "the file chose the target, so it is named: {cfg}"
    );
    assert!(
        cfg["config_file_warning"].is_null(),
        "the file is IN USE — there is nothing to warn about: {cfg}"
    );
}

/// The positive control for the test above. Without it, that assertion would also pass on a
/// server that had stopped reading `IRIS_*` altogether.
///
/// The port is dead on purpose. That matters twice over: it proves the env step keeps a target it
/// was given rather than abandoning it to the blind scans (#57 — a named-but-down instance must
/// still produce a connection, so a tool call gets the standard envelope and a seed to re-probe),
/// and it keeps this test about provenance rather than reachability.
#[test]
fn iris_host_alone_still_connects_and_says_so() {
    let dir = tempfile::tempdir().unwrap(); // no config file
    let cfg = check_config_of(
        &["--workspace", dir.path().to_str().unwrap()],
        &[
            ("IRIS_HOST", "127.0.0.1"),
            ("IRIS_WEB_PORT", "59999"),
            ("IRIS_NAMESPACE", "ENVWINS"),
        ],
    );
    assert_eq!(
        cfg["connection_source"], "env_vars",
        "with no file to outrank it, IRIS_HOST is still honoured — and now reports the step it \
         actually came from: {cfg}"
    );
    assert_eq!(cfg["port"], 59999, "{cfg}");
    assert_eq!(cfg["namespace"], "ENVWINS", "{cfg}");
}

/// The escape hatch the issue required to survive: a real flag outranks the file, and now says
/// so consistently — including that an edit will NOT take the connection over, which is only
/// true because `check_reload` honours the flag too.
#[test]
fn a_real_host_flag_outranks_the_file_and_the_warning_says_so() {
    let dir = workspace_with("host = \"127.0.0.1\"\nweb_port = 45080\nnamespace = \"FILEWINS\"\n");
    let cfg = check_config_of(
        &[
            "--workspace",
            dir.path().to_str().unwrap(),
            "--host",
            "127.0.0.1",
            "--web-port",
            "59999",
            "--namespace",
            "FLAGWINS",
        ],
        &[],
    );
    assert_eq!(cfg["connection_source"], "explicit_flag", "{cfg}");
    assert_eq!(cfg["port"], 59999, "{cfg}");
    assert_eq!(cfg["namespace"], "FLAGWINS", "{cfg}");
    assert!(
        cfg["config_file_ignored"].is_string(),
        "a file is sitting there unused — #414's warning must still fire: {cfg}"
    );
    let w = cfg["config_file_warning"].as_str().unwrap_or("");
    assert!(
        w.contains("Editing the file will NOT replace this connection"),
        "the flag branch of the warning, true only because check_reload honours the flag: {w}"
    );
}
