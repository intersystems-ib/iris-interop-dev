//! T023: MCP handshake integration test.
//! Spawns the `iris-interop-dev mcp` binary, sends JSON-RPC initialize + tools/list,
//! asserts the interop profile is returned in full and the response is timely. The profile's
//! size is stated once, in `mcp_server_tools_list_returns_interop_profile`'s assertion — this
//! line said 20 while that assertion said 31.
//!
//! Tests written FIRST — must fail until T015–T022 are implemented.
#![allow(dead_code, clippy::zombie_processes)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `#` followed by one or more digits — an issue reference. Hand-rolled so this test
/// pulls in no new dependency.
struct IssueRef;
impl IssueRef {
    fn find_iter<'a>(&self, hay: &'a str) -> impl Iterator<Item = IssueMatch<'a>> {
        let bytes: Vec<(usize, char)> = hay.char_indices().collect();
        let mut out = vec![];
        for (i, (pos, c)) in bytes.iter().enumerate() {
            if *c != '#' {
                continue;
            }
            let mut end = *pos + 1;
            let mut n = 0;
            for (p2, c2) in bytes.iter().skip(i + 1) {
                if c2.is_ascii_digit() {
                    end = p2 + c2.len_utf8();
                    n += 1;
                } else {
                    break;
                }
            }
            if n > 0 {
                out.push(IssueMatch(&hay[*pos..end]));
            }
        }
        out.into_iter()
    }
}
struct IssueMatch<'a>(&'a str);
impl<'a> IssueMatch<'a> {
    fn as_str(&self) -> &'a str {
        self.0
    }
}
const ISSUE_REF: IssueRef = IssueRef;

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
        // #298: IRIS_HOST as well, and the pair is what matters — IRIS_WEB_PORT alone never enters
        // the env-var leg of the discovery cascade (it is guarded by IRIS_HOST), so the server fell
        // through to auto-discovery and adopted whatever IRIS was reachable. Measured: connected:true,
        // connection_source:"auto_discovered", port 8080. These assertions are about protocol shape and
        // passed either way, but the same code was exercising a connected server locally and a
        // disconnected one on a bare runner. 127.0.0.1:9 keeps the instant refusal this port was chosen
        // for; an unroutable address costs 2036ms against 221ms.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
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

/// #114: a write-disallowed connection must still advertise every tool and answer reads.
///
/// The old gate removed two tool names from the router, which hid their READ actions too —
/// `iris_production_item` gone meant `get_settings` gone — while five genuinely
/// write-capable tools went through untouched. This server is aimed at DEVELOPMENT
/// instances, so a blocked read is the expensive failure, not the safe one.
///
/// `IRIS_NAMESPACE=PROD` makes `is_write_allowed()` false without needing a Live instance.
/// No IRIS is reachable here, which is the point: the gate decides before any connection is
/// used, so the refusal and the pass-through are both observable offline.
///
/// #304: that sentence used to say "(port 9)" and was WRONG — port 9 alone never enters the
/// env-var leg of discovery, so this spawn adopted a reachable instance. The pin below is what
/// makes the sentence true. It also makes the premise independent of the instance: see the note
/// at the spawn.
#[test]
fn a_write_disallowed_connection_still_lists_every_tool() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        // #304: pinned for the same reason as `discovery_waits_for_iris`, and this test needed it
        // more. Its premise is that writes are DISALLOWED, which `is_write_allowed()` decides from
        // IRIS_ALLOW_PROD, `system_mode` and the namespace — no network. But `system_mode` is
        // fetched from the instance by `detect_system_mode`, which reads `^%SYS("SystemMode")`. So
        // while this spawn adopted a live instance, the premise depended on what THAT instance
        // reported: "Development" or "Test" makes `is_write_allowed()` true, and this test would
        // still pass, because every assertion here is about the tool list. Unreachable host ->
        // system_mode stays Unknown -> PROD namespace -> writes disallowed, by construction.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
        .env("IRIS_NAMESPACE", "PROD")
        .env_remove("IRIS_ALLOW_PROD")
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
    let _ = read_jsonrpc(&mut reader);
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    send_jsonrpc(&mut stdin, 2, "tools/list", "{}");
    let listed = read_jsonrpc(&mut reader);
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        names.len(),
        33,
        "the write gate must not shrink the tool list — it used to advertise 21 here, and \
         the two it removed took their read actions with them: {names:?}"
    );
    for tool in ["iris_production_item", "iris_credential_manage"] {
        assert!(
            names.contains(&tool.to_string()),
            "'{tool}' must stay listed on a write-disallowed connection: {names:?}"
        );
    }

    child.kill().ok();
}

/// #343: `iris_gateway_manage`'s write half must be REFUSED at dispatch on a write-disallowed
/// connection, and refusing must not echo the password back.
///
/// Classifying `create`/`delete` in `mutating_call` is not evidence the gate fires — a registered
/// but unreached gate looks exactly like a working one, which is why this drives the real dispatch
/// path instead of the classifier. The read half is called in the SAME session as the control: if
/// probe were refused too, the write refusals below would prove nothing about writes.
///
/// Offline by construction, the same way `a_write_disallowed_connection_still_lists_every_tool` is:
/// an unreachable host leaves `system_mode` Unknown, and `IRIS_NAMESPACE=PROD` then makes
/// `is_write_allowed()` false with no network involved.
#[test]
fn the_gateway_write_actions_are_refused_on_a_write_disallowed_connection() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }
    // Distinctive enough that a partial echo is still a failure.
    const SENTINEL: &str = "Hunter2-SENTINEL-xyzzy";

    let mut child = Command::new(&bin)
        .arg("mcp")
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
        .env("IRIS_NAMESPACE", "PROD")
        .env_remove("IRIS_ALLOW_PROD")
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
    let _ = read_jsonrpc(&mut reader);
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Returns (what was sent on the wire, what came back).
    let mut call = |id: u64, args: serde_json::Value| -> (String, String) {
        let params = serde_json::json!({"name": "iris_gateway_manage", "arguments": args});
        let sent = params.to_string();
        send_jsonrpc(&mut stdin, id, "tools/call", &sent);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (sent, line)
    };

    // ── the write half: refused at dispatch, by the gate, naming what it would have changed ─
    let (create_sent, created) = call(
        2,
        serde_json::json!({
            "action": "create",
            "connection": "PG_GATE_TEST",
            "url": "jdbc:postgresql://db:5432/X",
            "driver": "org.postgresql.Driver",
            "user": "u",
            "password": SENTINEL,
        }),
    );
    assert!(
        created.contains("WRITE_GATED"),
        "create was not refused by the write gate: {created}"
    );
    let (_, deleted) = call(
        3,
        serde_json::json!({"action": "delete", "connection": "PG_GATE_TEST"}),
    );
    assert!(
        deleted.contains("WRITE_GATED"),
        "delete was not refused by the write gate: {deleted}"
    );

    // ── the control: the READ half must reach the handler ────────────────────────────────
    // It cannot succeed — nothing is listening on port 9 — and that is the point: it must fail
    // for a CONNECTION reason, not the gate's. Without this, a gate that refused everything
    // would satisfy both assertions above and look identical to a working one.
    let (_, probed) = call(4, serde_json::json!({"action": "probe"}));
    assert!(
        !probed.contains("WRITE_GATED"),
        "probe changes nothing and must not be write-gated — otherwise the refusals above are \
         about a gate that blocks everything, not about writes: {probed}"
    );

    // ── and the password must not be anywhere in any of the three frames ─────────────────
    for (label, frame) in [
        ("create", &created),
        ("delete", &deleted),
        ("probe", &probed),
    ] {
        assert!(
            !frame.contains(SENTINEL),
            "the {label} response carries the password: {frame}"
        );
        // A partial echo is a leak too.
        assert!(
            !frame.contains("Hunter2"),
            "the {label} response carries part of the password: {frame}"
        );
    }
    // The control on that sweep: the sentinel really WAS sent, so "absent from the response" is a
    // fact about the response rather than about a test that never used it. Asserted on the wire
    // bytes, because the refusal deliberately echoes none of the arguments back.
    assert!(
        create_sent.contains(SENTINEL),
        "the sentinel was never sent, so the sweep above proves nothing: {create_sent}"
    );

    child.kill().ok();
}

/// #112: every tool that takes parameters must SAY SO in its advertised schema.
///
/// Ten of the 23 interop tools shipped `{"type":"object"}` — no properties, no `required` —
/// because their handler signature was `Parameters<AnyParams>`. A model had nothing to work
/// from but the tool description, and `{}` was a schema-valid call. Measured over a
/// 1121-call OpenCode campaign: 31 parameter errors in 223 calls to those tools (13.9%),
/// and ZERO in 898 calls to the twelve that had real schemas. All 22 `MISSING_WHAT` errors
/// were calls of exactly `{}`.
///
/// This asserts at the wire, over the whole profile, so a new tool cannot quietly join the
/// schema-less group.
#[test]
fn every_tool_advertises_the_parameters_it_reads() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        // #298: IRIS_HOST pins the env-var leg so discovery cannot adopt a real instance.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
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
    let _ = read_jsonrpc(&mut reader);
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    send_jsonrpc(&mut stdin, 2, "tools/list", "{}");
    let response = read_jsonrpc(&mut reader);
    let tools = response["result"]["tools"].as_array().unwrap();

    // The ONLY tool that legitimately advertises no properties is the one that reads none.
    // Adding a name here is a claim that the tool takes no parameters at all — check the
    // handler before you do.
    const TAKES_NO_PARAMETERS: &[&str] = &["check_config"];

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("?");
        let schema = &tool["inputSchema"];
        let props = schema.get("properties").and_then(|p| p.as_object());
        if TAKES_NO_PARAMETERS.contains(&name) {
            continue;
        }
        let props = props.unwrap_or_else(|| {
            panic!(
                "tool '{name}' advertises no properties. A caller — human or model — has \
                 only the prose description to guess parameter names from, and `{{}}` is a \
                 schema-valid call. Give the handler a typed params struct (or wrap the \
                 existing one in `Described<…>`): {schema}"
            )
        });
        assert!(
            !props.is_empty(),
            "tool '{name}' advertises an EMPTY properties map: {schema}"
        );

        // #202, which is #112 one level down: a PROPERTY with no type is the same defect
        // as a TOOL with no properties. `search_table` was declared `Option<Value>`, so it
        // shipped as `{"default": null, "description": "..."}` — a client had no shape to
        // serialise against, sent the object as a JSON string, and the filter was dropped
        // on the floor. Prose is not a schema at either level.
        for (key, prop) in props {
            let typed = ["type", "anyOf", "oneOf", "allOf", "enum", "const", "$ref"]
                .iter()
                .any(|k| prop.get(k).is_some());
            assert!(
                typed,
                "tool '{name}' advertises '{key}' with no type — a caller has only the \
                 prose to serialise against, and whatever it guesses cannot be validated \
                 before it is sent. Give the field a concrete Rust type rather than \
                 serde_json::Value: {prop}"
            );
        }

        // A dispatcher's discriminator must be required AND enumerated. Naming the field
        // without its values only moves the guess one level down — which is what the nine
        // INVALID_ACTION errors in the campaign were.
        for key in ["action", "what"] {
            let Some(prop) = props.get(key) else { continue };
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            assert!(
                required.contains(&key),
                "tool '{name}' has a '{key}' discriminator that is not in `required` — a \
                 model may legitimately omit it and will: {schema}"
            );
            assert!(
                prop.get("enum")
                    .and_then(|e| e.as_array())
                    .is_some_and(|e| !e.is_empty()),
                "tool '{name}' declares '{key}' without its valid values. The runtime error \
                 names them correctly; it just arrives a round trip late: {prop}"
            );
        }
    }

    child.kill().ok();
}

/// #104: a tool pruned from the profile must not DISPATCH, not merely be unlisted.
///
/// This is the test the fork did not have. `mcp_server_tools_list_returns_interop_profile`
/// asserts `iris_search` is absent from tools/list and passed for the entire life of the
/// bug; the server was answering `tools/call {"name":"iris_search"}` with real IRIS data
/// the whole time, because `#[tool_handler]`'s default router expression builds a fresh
/// UNPRUNED router per call while only `list_tools` read the pruned instance field.
/// Nothing below inspects the listing — it goes at the wire, which is where the exposure
/// was. The same mechanism carries the write gate, so this covers that too.
#[test]
fn pruned_tool_is_rejected_at_dispatch_not_merely_unlisted() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    let mut child = Command::new(&bin)
        .arg("mcp")
        .arg("--toolset")
        .arg("interop")
        // #298: IRIS_HOST as well as the port, and the pair matters. `IRIS_WEB_PORT` alone never
        // triggers the env-var leg of the discovery cascade — that branch is entered only when
        // IRIS_HOST is set — so the server fell through to auto-discovery and adopted whatever IRIS
        // was reachable on the machine. Measured before this change: `check_config` reported
        // `connected:true, connection_source:"auto_discovered", port:8080`. This test's premise is
        // that the rejection happens BEFORE any connection use, and it could not show that while
        // holding a working connection.
        //
        // 127.0.0.1:9 (discard) rather than an unroutable address: both produce a disconnected
        // server, but TEST-NET-1 waits on a TCP timeout — measured 2036ms against 221ms here.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
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
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // iris_search is not in INTEROP_TOOLS. Ask for it by name anyway — a scripted client,
    // a replayed transcript or a model that learned the name elsewhere can all do this.
    send_jsonrpc(
        &mut stdin,
        2,
        "tools/call",
        r#"{"name":"iris_search","arguments":{"query":"Copyright","documents":["%Library.String.cls"]}}"#,
    );
    let response = read_jsonrpc(&mut reader);

    assert!(
        response.get("result").is_none(),
        "a pruned tool RAN: tools/call returned a result for iris_search under the \
         interop toolset. Pruning must be enforced at dispatch, not only in tools/list: {response}"
    );
    let err = response
        .get("error")
        .unwrap_or_else(|| panic!("expected a JSON-RPC error, got: {response}"));
    assert_eq!(
        err["data"]["error_code"], "TOOL_NOT_IN_TOOLSET",
        "the rejection should say WHY the tool is unreachable (which toolset pruned it), \
         not just that it was not found: {err}"
    );

    // A kept tool must still dispatch — otherwise this test would pass on a server that
    // rejects everything.
    send_jsonrpc(
        &mut stdin,
        3,
        "tools/call",
        r#"{"name":"check_config","arguments":{}}"#,
    );
    let ok = read_jsonrpc(&mut reader);
    let ok_result = ok.get("result").unwrap_or_else(|| {
        panic!("check_config is in the interop keep-list and must still dispatch: {ok}")
    });

    // Pin the premise this test rests on, rather than trusting the env vars above to deliver it.
    // 127.0.0.1:9 is refused on every machine, so this cannot false-fail — and if IRIS_HOST is ever
    // dropped from the spawn, auto-discovery adopts a reachable instance and this fires. That is not
    // hypothetical: it is what the spawn did before #298.
    let cfg: serde_json::Value = serde_json::from_str(
        ok_result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("check_config returned no text payload: {ok}")),
    )
    .expect("check_config payload is JSON");
    assert_eq!(
        cfg["connected"],
        serde_json::json!(false),
        "this test asserts the pruning rejection precedes any CONNECTION USE, so the server must \
         not hold a working connection. connection_source={:?}: {cfg}",
        cfg["connection_source"]
    );

    // An unknown name is a different fact from a pruned one, and gets a different code.
    send_jsonrpc(
        &mut stdin,
        4,
        "tools/call",
        r#"{"name":"no_such_tool_at_all","arguments":{}}"#,
    );
    let unknown = read_jsonrpc(&mut reader);
    assert_eq!(
        unknown["error"]["data"]["error_code"], "UNKNOWN_TOOL",
        "a name that is not a tool at all must not be reported as toolset pruning: {unknown}"
    );

    child.kill().ok();
}

/// tools/list returns exactly the interop profile (this fork's default toolset), whose size is
/// asserted below against `INTEROP_TOOLS` rather than restated here — this line said 20.
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
        // #298: IRIS_HOST pins the env-var leg so discovery cannot adopt a real instance.
        .env("IRIS_HOST", "127.0.0.1")
        .env("IRIS_WEB_PORT", "9")
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

    // Interop profile (this fork's default toolset) exposes exactly the interop keep-list
    // (INTEROP_TOOLS). #352 re-recorded this from 32 to 33 for `stream_inspect`.
    assert_eq!(
        tool_names.len(),
        33,
        "expected the 33-tool interop profile, got {}: {:?}",
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
        // The count assertion above is satisfied by ANY 32 tools. This pins the one whose
        // absence was invisible: it is the only consumer of the ObjectScript tree-sitter
        // grammars, so dropping it from the keep-list makes those grammars unreachable
        // again without changing a single number.
        "iris_symbols_local",
        // Runs arbitrary code and is write-gated: if it ever silently leaves the profile,
        // the gate tests still pass and callers quietly lose the ability to call a method.
        "iris_execute_method",
        // Four of its five actions were broken and unnoticed precisely because it sat outside
        // the profile. Pinned by name so it cannot quietly drop back out.
        "iris_macro",
        // #246. Upstream's versions of these call class queries that do not exist, so an
        // accidental "port" would reintroduce two tools that only ever error. Pinned by name.
        "hl7_schema_list",
        "iris_gateway_query",
        "hl7_schema_inspect",
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

    // The generated executor class has carried a test since the RunUser()/Execute() rewrite
    // asserting it does NOT use `CodeMode = objectgenerator` — user code runs at CALL time,
    // not compile time. The DESCRIPTION kept claiming the opposite for four minor versions,
    // which is the half a model actually reads. Worse, it is the exact construct upstream is
    // adding a security gate against, so the stale text made this server look like it does
    // the risky thing it deliberately stopped doing. Pair the two claims here.
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("?");
        let described = format!(
            "{} {}",
            tool["description"].as_str().unwrap_or(""),
            tool["inputSchema"]
        );
        let claims_use = described.contains("via CodeMode=objectgenerator")
            || described.contains("via CodeMode = objectgenerator")
            || described.contains("using CodeMode=objectgenerator");
        assert!(
            !claims_use,
            "tool '{name}' advertises execution via CodeMode=objectgenerator, which \
             build_exec_class has a test forbidding — see connection.rs \
             build_exec_class_no_objectgenerator_uses_runuser"
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
        // #112, a third route for the same leak, and the one the jargon list above could
        // not see: an ISSUE NUMBER. `#82` shipped its own rationale — `iris_get_log`
        // advertised "(issue #82)", "(issue #81)", "(issue #78)" and "(issue #83)" in its
        // property descriptions for as long as that fix has existed, because none of those
        // strings is Rust syntax. No caller-facing sentence needs a tracker reference:
        // the reader of a tools/list cannot open the issue and does not want to. This is a
        // precise marker — `#` followed by digits — not a substring match on prose.
        let issue_refs: Vec<&str> = ISSUE_REF.find_iter(&schema).map(|m| m.as_str()).collect();
        assert!(
            issue_refs.is_empty(),
            "tool '{name}' ships issue references {issue_refs:?} in its advertised \
             inputSchema — maintainer bookkeeping, sent to every client on every \
             tools/list. Keep it in the source as a `//` comment: {schema}"
        );

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

/// SC-001: `initialize` must not block on discovery.
///
/// #200: this asserted `p50 < 100ms` over 5 samples and failed about 1 run in 5 on an
/// unmodified master. Measured 2026-09-19 on this machine (16 cores, debug profile), timing the
/// initialize round-trip exactly as below:
///
/// ```text
/// idle, no IRIS env         p50 16-18ms over 8 simulated runs   0/8 failed
/// idle, IRIS_HOST pinned    p50 20.8ms, max 25.0ms              0/10 over 100ms
/// idle, IRIS_HOST dead port p50  7.4ms, max  8.0ms              0/10 over 100ms
/// 16 busy-loop hogs running p50 112-238ms                       6/6 FAILED
/// ```
///
/// So the old guard was a coin flip on machine load, not a property of the code: the steady
/// state has 5x margin, and any co-scheduled build erases it. The comment on `LOG_TEST_GUARD`
/// below already conceded this — it serialises two other tests so this one would not read their
/// spawns as a regression. That is a workaround for a threshold set too tight to survive the
/// suite it lives in.
///
/// What the guard is actually for: a regression that makes `initialize` wait on discovery
/// network work before answering. That costs SECONDS, not tens of milliseconds — a cold first
/// spawn measured 2322ms here, and `discover_iris`'s localhost probe carries a 2s cap. So the
/// budget is 1000ms: an order of magnitude below the ~2.3s a blocking probe costs, and ~50x
/// above the measured steady-state floor. It still fails loudly on the regression it exists to
/// catch, without failing on a busy laptop.
///
/// Two deliberate choices:
///
///   * The environment is NOT pinned. Pinning IRIS_HOST would stabilise the number by routing
///     `discover_iris` through step 1 (explicit) and never exercising steps 2-6 — it would buy
///     a steady measurement by silently narrowing what is measured.
///   * The FASTEST sample is used, not the p50, and sampling stops as soon as one comes in under
///     budget. A latency floor is a property of the code; the median under contention is a
///     property of the scheduler. Observed floors stayed at 42-108ms with 16 busy loops running,
///     and 58ms on a machine whose other five samples were 1.6s-21.8s — exactly the spread the
///     old p50 turned into a coin flip. On a quiet machine it takes ONE sample and 0.09s.
///
/// This is load-TOLERANT, not load-immune, and the distinction was measured rather than assumed:
/// at a load average of ~177 (16 runaway busy loops plus overlapping cargo runs) every sample
/// came in at ~3000ms and this assertion failed with nothing wrong in the code. A budget that
/// survives THAT would have to be so wide it could no longer see a blocking probe. So the
/// failure message says to check the machine first.
///
/// Every sample is printed unconditionally, so a slow machine is visible in the log instead of
/// being invisible until the day it crosses a threshold.
#[test]
fn initialize_does_not_block_on_discovery() {
    let bin = iris_dev_bin();
    if !bin.exists() {
        eprintln!("Skipping: iris-agentic-dev binary not found");
        return;
    }

    // Budget, and what it guards. Named so a future reader changing it knows what it protects.
    const BUDGET: Duration = Duration::from_millis(1000);
    const ATTEMPTS: usize = 6;

    // Stop at the FIRST sample under budget. One clean round-trip proves what the guard claims —
    // that `initialize` can answer without waiting on discovery — and no number of slow samples
    // afterwards would add to that. This also keeps the test fast: it normally exits after one
    // spawn. Sampling all 6 unconditionally took 60.8s on a machine running concurrent builds,
    // which is a cost paid to learn nothing.
    let mut samples = Vec::new();
    for _ in 0..ATTEMPTS {
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
        let resp = read_jsonrpc(&mut reader);
        let elapsed = start.elapsed();
        child.kill().ok();

        // A timing sample means nothing unless the server actually answered. Without this the
        // measurement could be timing a read that returned nothing, and a fast empty read would
        // look like excellent latency.
        assert_eq!(
            resp["id"], 1,
            "initialize did not answer, so this sample times nothing: {resp}"
        );
        samples.push(elapsed);
        if elapsed < BUDGET {
            break;
        }
    }

    let floor = *samples.iter().min().expect("at least one sample was taken");
    // Printed unconditionally, so a slow machine is visible in the log rather than invisible
    // until the day it crosses a threshold.
    eprintln!(
        "initialize round-trip samples (ms): {:?} — fastest {}ms, budget {}ms",
        samples.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        floor.as_millis(),
        BUDGET.as_millis()
    );

    assert!(
        floor < BUDGET,
        "none of {} initialize round-trips came in under the {}ms budget; fastest was {}ms \
         (SC-001). This budget is not a performance target — it sits an order of magnitude below \
         the ~2.3s that blocking on discovery costs, with roughly 10x margin over the floor \
         measured while a build saturates every core. It is load-TOLERANT, not load-immune: at a \
         load average of ~177 every sample here measured ~3000ms and this assertion failed with \
         nothing wrong in the code. So check the machine first — if it is merely busy, that is \
         the finding; if it is idle, initialize is waiting on discovery network work before \
         answering. All samples (ms): {:?}",
        samples.len(),
        BUDGET.as_millis(),
        floor.as_millis(),
        samples.iter().map(|d| d.as_millis()).collect::<Vec<_>>()
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
        // #304: `IRIS_WEB_PORT=9` alone does NOT make this hermetic. The env-var leg of the
        // discovery cascade is guarded by IRIS_HOST, so port 9 by itself is never read and the
        // server adopted whatever IRIS was reachable on the machine — measured in #298 as
        // `connected:true, connection_source:"auto_discovered", port:8080`. The assertion below
        // says "even without IRIS connection"; that was false until this pin.
        //
        // Pinning does not defeat this test's subject. What it asserts is a LATENCY bound plus a
        // non-empty tool list — that the server does not block on discovery — and an unreachable
        // host exercises exactly that, deterministically. 127.0.0.1 rather than a TEST-NET-1
        // address because a refused connection is ~221ms against 2036ms for one that must time out.
        .env("IRIS_HOST", "127.0.0.1")
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
/// suite's peak process count stays where it was.
///
/// #200: this used to be load-bearing, because `mcp_server_startup_latency_under_100ms` read
/// spawn contention as a regression. Its replacement, `initialize_does_not_block_on_discovery`,
/// asserts the floor of warm samples against a 1000ms budget and no longer fails on
/// contention — measured 6/6 failures under 16 busy loops before, 0 after. Kept anyway: it
/// bounds the suite's peak process count, which is worth keeping on its own.
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
            // #298: IRIS_HOST pins the env-var leg so discovery cannot adopt a real instance.
            .env("IRIS_HOST", "127.0.0.1")
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
        // #298: IRIS_HOST pins the env-var leg so discovery cannot adopt a real instance.
        .env("IRIS_HOST", "127.0.0.1")
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
