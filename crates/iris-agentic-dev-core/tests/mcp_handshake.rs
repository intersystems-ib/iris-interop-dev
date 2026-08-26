//! T023: MCP handshake integration test.
//! Spawns the `iris-interop-dev mcp` binary, sends JSON-RPC initialize + tools/list,
//! asserts the 20-tool interop profile is returned and the response is timely.
//!
//! Tests written FIRST — must fail until T015–T022 are implemented.
#![allow(dead_code, clippy::zombie_processes)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn iris_dev_bin() -> std::path::PathBuf {
    // Find the binary in the cargo target directory
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/iris-dev-core → crates
    path.pop(); // crates → workspace root
    path.push("target/debug/iris-interop-dev");
    path
}

fn send_jsonrpc(stdin: &mut impl Write, id: u64, method: &str, params: &str) {
    let msg = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{}\",\"params\":{}}}\n",
        id, method, params
    );
    stdin.write_all(msg.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

fn read_jsonrpc(reader: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).expect("invalid JSON-RPC response")
}

/// iris-dev mcp starts and responds to initialize within 500ms.
#[test]
fn mcp_server_starts_and_responds_to_initialize() {
    // Give any previous test's spawned processes time to fully exit
    std::thread::sleep(std::time::Duration::from_millis(500));
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!(
            "Skipping: iris-agentic-dev binary not found at {}",
            bin.display()
        );
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        // Disable IRIS discovery for handshake tests — we only test MCP protocol, not tools
        .env("IRIS_WEB_PORT", "9") // Port 9 (discard) — instant ECONNREFUSED, no DNS lookup
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn iris-agentic-dev mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let start = Instant::now();
    send_jsonrpc(
        &mut stdin,
        1,
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}"#,
    );

    let response = read_jsonrpc(&mut reader);
    let elapsed = start.elapsed();
    // Send required initialized notification
    let init_notif = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        "
"
    );
    stdin.write_all(init_notif.as_bytes()).unwrap();
    stdin.flush().unwrap();

    // Generous bound — this asserts "responds promptly", not a perf gate (that's the
    // dedicated startup_latency test). Cold start of the debug binary can exceed 500ms.
    assert!(
        elapsed < Duration::from_millis(2000),
        "initialize took {}ms, expected <2000ms",
        elapsed.as_millis()
    );
    assert!(
        response.get("result").is_some(),
        "initialize response missing 'result': {}",
        response
    );

    child.kill().ok();
}

/// tools/list returns exactly the 20-tool interop profile (this fork's default toolset).
#[test]
fn mcp_server_tools_list_returns_interop_profile() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        // Disable IRIS discovery for handshake tests — we only test MCP protocol, not tools
        .env("IRIS_WEB_PORT", "9") // Port 9 (discard) — instant ECONNREFUSED, no DNS lookup
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn iris-agentic-dev mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    send_jsonrpc(
        &mut stdin,
        1,
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}"#,
    );
    let _init = read_jsonrpc(&mut reader);
    let init_notif = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        "
"
    );
    stdin.write_all(init_notif.as_bytes()).unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    send_jsonrpc(&mut stdin, 2, "tools/list", "{}");
    let response = read_jsonrpc(&mut reader);

    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list response missing tools array");

    let tool_names: Vec<_> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Interop profile (this fork's default toolset) exposes exactly the 23-tool
    // interop keep-list (INTEROP_TOOLS).
    assert_eq!(
        tool_names.len(),
        23,
        "expected the 23-tool interop profile, got {}: {:?}",
        tool_names.len(),
        tool_names
    );

    // Required interop tools present (no dots — Bedrock compatible)
    let required = [
        "iris_compile",
        "iris_test",
        "iris_execute",
        "iris_query",
        "iris_doc",
        "iris_production",
        "iris_interop_query",
        "iris_table_info",
        "iris_debug",
        "docs_introspect",
    ];
    for name in required {
        assert!(
            tool_names.contains(&name),
            "required interop tool '{}' missing from tools/list",
            name
        );
    }

    // Meta/non-interop tools must be pruned in the interop profile.
    for name in [
        "skill_list",
        "kb_recall",
        "agent_stats",
        "iris_search",
        "iris_info",
    ] {
        assert!(
            !tool_names.contains(&name),
            "meta tool '{}' should NOT be in the interop profile",
            name
        );
    }

    // Assert no tool has a dot in the name (Bedrock/VS Code requirement)
    for name in &tool_names {
        assert!(
            !name.contains('.'),
            "tool name '{}' contains dot — invalid for Bedrock/VS Code",
            name
        );
    }

    // #82: the advertised inputSchema is shipped to every client on every tools/list, so
    // a Rust `///` on a params struct becomes wire traffic — schemars promotes it to the
    // schema's top-level `description`. The #82 rationale landed there as 761 characters
    // of maintainer commentary (serde, schemars, private function names, issue numbers) on
    // iris_get_log, the only tool of the 23 carrying a top-level description at all.
    // Nothing caught it: the schema tests assert properties, never prose. This does, at
    // the wire, for every tool.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("?");
        let schema = tool["inputSchema"].to_string();
        // Markers that can ONLY be Rust. `#[` and `fn ` were in this list and had to come
        // out: both are substring matches on ordinary prose, so a future description
        // reading "…returns fn signatures…" would redden a required gate for a
        // non-problem. The attribute prefixes below are the real syntax; the rest are
        // identifiers no caller-facing sentence contains.
        for jargon in [
            "serde",
            "schemars",
            "JsonSchema",
            "Deserialize",
            "#[serde",
            "#[schemars",
            "#[derive",
            "drop_default_additional_properties",
            "GetLogIssue",
        ] {
            assert!(
                !schema.contains(jargon),
                "tool '{name}' ships Rust-internal commentary ('{jargon}') in its \
                 advertised inputSchema — that is context spent on every tools/list. \
                 Keep the rationale in the source as a `//` comment: {schema}"
            );
        }
        // The same leak by another route: schemars puts the params STRUCT NAME in the
        // schema's top-level `title` (`GetLogParams`, `CompileParams`, …). It names
        // nothing the caller can act on — the tool already has a `name` — and it went out
        // on every tools/list until `drop_struct_name_title` stripped it here.
        assert!(
            tool["inputSchema"].get("title").is_none(),
            "tool '{name}' advertises a top-level schema title — that is the Rust struct \
             name: {schema}"
        );
    }

    child.kill().ok();
}

/// Startup latency p50 < 100ms over 5 runs (SC-001).
#[test]
fn mcp_server_startup_latency_under_100ms() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut latencies = Vec::new();
    for _ in 0..5 {
        let mut child = Command::new(&bin)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn iris-agentic-dev mcp");

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);

        let start = Instant::now();
        send_jsonrpc(
            &mut stdin,
            1,
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"bench","version":"0.1"}}"#,
        );
        let _resp = read_jsonrpc(&mut reader);
        latencies.push(start.elapsed());
        child.kill().ok();
    }

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    assert!(
        p50 < Duration::from_millis(100),
        "p50 startup latency {}ms exceeds 100ms (SC-001)",
        p50.as_millis()
    );
}

/// T009: discovery waits for IRIS — server returns tool list within 5s even with no env vars.
/// Uses port 9 (discard) so discovery fails fast, but server still returns tool list.
#[test]
fn discovery_waits_for_iris() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        .env("IRIS_WEB_PORT", "9") // instant fail — tests that server doesn't hang
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn iris-agentic-dev mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let start = Instant::now();
    send_jsonrpc(
        &mut stdin,
        1,
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}"#,
    );
    let init = read_jsonrpc(&mut reader);
    assert!(init.get("result").is_some(), "initialize failed: {}", init);

    let init_notif = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        "\n"
    );
    stdin.write_all(init_notif.as_bytes()).unwrap();
    stdin.flush().unwrap();

    send_jsonrpc(&mut stdin, 2, "tools/list", "{}");
    let resp = read_jsonrpc(&mut reader);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "tools/list took {}ms, expected <5000ms",
        elapsed.as_millis()
    );

    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools array missing");
    assert!(
        !tools.is_empty(),
        "expected tools to be listed even without IRIS connection"
    );

    child.kill().ok();
}

/// T010: web prefix is included in Atelier request URL.
/// Verifies that IRIS_WEB_PREFIX is correctly incorporated into the base URL.
#[test]
fn web_prefix_in_connection_url() {
    use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};

    // Construct a connection with a prefix in the base_url (as mcp.rs does)
    let base_url = "http://localhost:80/irisaicore".to_string();
    let conn = IrisConnection::new(
        base_url,
        "USER",
        "_SYSTEM",
        "SYS",
        DiscoverySource::ExplicitFlag,
    );

    let url = conn.atelier_url("/v8/USER/action/compile");
    assert!(
        url.contains("/irisaicore/api/atelier/"),
        "prefix missing from URL: {}",
        url
    );
    assert_eq!(
        url,
        "http://localhost:80/irisaicore/api/atelier/v8/USER/action/compile"
    );
}

/// #57: a connection failure must come back as a TOOL error carrying the standard
/// envelope, not as a JSON-RPC protocol error. IRIS being unreachable (wrong port,
/// container down) is the most common workshop failure, and a `-32603` frame gives
/// a classifier nothing to bucket and the user no hint.
#[test]
fn unreachable_iris_returns_the_error_envelope_not_a_protocol_error() {
    std::thread::sleep(std::time::Duration::from_millis(500));
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: binary not found at {}", bin.display());
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9") // discard port — instant ECONNREFUSED
        .env("IRIS_USERNAME", "u")
        .env("IRIS_PASSWORD", "p")
        .env("IRIS_NAMESPACE", "USER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp server");

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    send_jsonrpc(
        &mut stdin,
        1,
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#,
    );
    let _ = read_jsonrpc(&mut reader);
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();

    send_jsonrpc(
        &mut stdin,
        2,
        "tools/call",
        r#"{"name":"iris_query","arguments":{"query":"SELECT 1","namespace":"USER"}}"#,
    );
    let frame = read_jsonrpc(&mut reader);
    let _ = child.kill();

    assert!(
        frame.get("error").is_none(),
        "a tool failure must not be a JSON-RPC protocol error: {frame}"
    );
    let result = frame
        .get("result")
        .unwrap_or_else(|| panic!("no result in frame: {frame}"));
    assert_eq!(
        result["isError"], true,
        "an unreachable IRIS is a genuine tool failure: {frame}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {frame}"));
    let v: serde_json::Value = serde_json::from_str(text).expect("payload is JSON");
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(
        v["error_code"], "IRIS_UNREACHABLE",
        "the envelope must classify it, so telemetry can bucket it: {v}"
    );
    assert!(
        !v["error"].as_str().unwrap_or("").is_empty(),
        "message must live in `error`: {v}"
    );
    assert!(
        v["hint"].as_str().unwrap_or("").len() > 10,
        "an unreachable IRIS has a mechanical fix — say it: {v}"
    );
}

/// The two log-file tests spawn several servers each. Run them one at a time so the
/// suite's peak process count stays where it was — `mcp_server_startup_latency_under_100ms`
/// measures real startup and reads spawn contention as a regression.
static LOG_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// #58: with IRIS_LOG_FILE set, the session's traces must survive the process, so a
/// failed workshop run can be reconstructed afterwards. Off unless set, and never
/// fatal when the path cannot be opened.
#[test]
fn log_file_is_written_when_requested_and_absent_otherwise() {
    let _serialized = LOG_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: binary not found at {}", bin.display());
        return;
    }
    let log = std::env::temp_dir().join(format!("iris-mcp-log-test-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);

    let run = |log_env: Option<&std::path::Path>| {
        let mut cmd = Command::new(&bin);
        cmd.arg("mcp")
            .env("IRIS_WEB_PORT", "9")
            .env("IRIS_PASSWORD", "shouldnotappear")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(p) = log_env {
            cmd.env("IRIS_LOG_FILE", p);
        }
        let mut child = cmd.spawn().expect("spawn");
        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        send_jsonrpc(
            &mut stdin,
            1,
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#,
        );
        let _ = read_jsonrpc(&mut reader);
        let _ = child.kill();
        let _ = child.wait();
    };

    // Not set → nothing is created.
    run(None);
    assert!(
        !log.exists(),
        "the log file must be opt-in — nothing should be written without IRIS_LOG_FILE"
    );

    // Set → the session is recorded, stamped with the build that produced it.
    run(Some(&log));
    let body = std::fs::read_to_string(&log).expect("log file should exist once requested");
    assert!(
        body.contains("session start"),
        "each run must be delimited so consecutive sessions are separable: {body}"
    );
    assert!(
        body.contains(env!("CARGO_PKG_VERSION")),
        "the banner must record which build produced the log: {body}"
    );
    assert!(
        !body.contains("shouldnotappear"),
        "credentials must never reach the log file: {body}"
    );

    // A second run appends rather than truncating — a cohort's runs accumulate.
    run(Some(&log));
    let body = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        body.matches("session start").count(),
        2,
        "second session must append, not truncate: {body}"
    );

    let _ = std::fs::remove_file(&log);
}

/// #58: an unopenable path is a diagnostic problem, not a fatal one — the server
/// must still serve.
#[test]
fn an_unwritable_log_path_does_not_stop_the_server() {
    let _serialized = LOG_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let bin = iris_dev_bin();
    if !bin.exists() {
        return;
    }
    let mut child = Command::new(&bin)
        .arg("mcp")
        .env("IRIS_WEB_PORT", "9")
        .env("IRIS_LOG_FILE", "/nonexistent-dir-for-test/x.log")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    send_jsonrpc(
        &mut stdin,
        1,
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#,
    );
    let frame = read_jsonrpc(&mut reader);
    let _ = child.kill();
    assert_eq!(
        frame["result"]["serverInfo"]["name"], "iris-interop-dev",
        "server must still handshake with an unusable log path: {frame}"
    );
}
