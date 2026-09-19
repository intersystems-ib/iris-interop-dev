#![allow(dead_code, clippy::zombie_processes)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn iris_dev_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target/debug/iris-interop-dev");
    p
}

/// #240: the namespace these tests must run against.
///
/// Four tests in this file hardcoded `"namespace":"USER"` in the tool arguments while their own
/// assertion messages said "must succeed on an interop ns". They could therefore only pass on an
/// instance where USER happens to have Interoperability — and since this target runs in no CI
/// job, nobody saw them fail. Run against a namespace without Ens.*, all four fail with
/// "Namespace 'USER' has no Interoperability enabled", which reads as a product defect and is
/// not one.
///
/// The idiom is not new: `test_production_item_disable` in this same file already reads
/// IRIS_NAMESPACE this way.
fn interop_ns() -> String {
    std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string())
}

fn mcp_exchange(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    mcp_exchange_with_toolset(None, messages)
}

/// Same harness, but able to name a toolset.
///
/// #240: `test_search_scope_and_case_insensitive_default` exercises `iris_search`, which is a
/// BASELINE tool and deliberately not in `INTEROP_TOOLS`. Started with the fork's default
/// profile the server never advertises it, so the call comes back "tool not found" and the
/// test's `SCOPE_REQUIRED` assertion can never be reached — it was unrunnable for a reason
/// that had nothing to do with the behaviour under test (#17).
fn mcp_exchange_with_toolset(
    toolset: Option<&str>,
    messages: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    mcp_exchange_timed_with_toolset(toolset, messages).0
}

/// Returns the responses AND how long the LAST id-bearing message took, measured from just
/// before it is written to when its response is read.
///
/// #240: the SC-003 latency assertions timed the whole of `mcp_exchange` — `Command::spawn` of
/// a debug binary, the MCP handshake, and the IRIS connect — and then reported the total as
/// "tool call exceeded 3s". It was never measuring the tool call, and four tests failed the 3s
/// budget on a machine that was merely busy, which would have made the newly-wired CI job
/// flaky on day one for a reason unrelated to any product behaviour. Time the call itself.
fn mcp_exchange_timed_with_toolset(
    toolset: Option<&str>,
    messages: &[serde_json::Value],
) -> (Vec<serde_json::Value>, std::time::Duration) {
    mcp_exchange_full(toolset, None, messages)
}

/// Start the server with an explicit connection namespace.
///
/// #240: `test_execute_and_query_namespace_defaults_to_connection` required the OPERATOR to set
/// `IRIS_NAMESPACE` to something other than USER, because "ran in the connection namespace" and
/// "ran in a hardcoded USER" are indistinguishable when the connection namespace IS USER. That
/// requirement is sound methodology and a bad precondition: CI sets IRIS_NAMESPACE=USER, so the
/// test could not run there at all. Letting the test choose its own connection namespace makes
/// the discrimination intrinsic instead of delegated.
fn mcp_exchange_in_namespace(
    namespace: &str,
    messages: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    mcp_exchange_full(None, Some(namespace), messages).0
}

fn mcp_exchange_full(
    toolset: Option<&str>,
    ns_override: Option<&str>,
    messages: &[serde_json::Value],
) -> (Vec<serde_json::Value>, std::time::Duration) {
    let bin = iris_dev_bin();
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    let iris_port = std::env::var("IRIS_WEB_PORT").unwrap_or_else(|_| "52780".to_string());

    let mut args: Vec<&str> = vec!["mcp"];
    if let Some(ts) = toolset {
        args.push("--toolset");
        args.push(ts);
    }
    let mut child = Command::new(&bin)
        .args(&args)
        .env("IRIS_HOST", &iris_host)
        .env("IRIS_WEB_PORT", &iris_port)
        .env(
            "IRIS_USERNAME",
            std::env::var("IRIS_USERNAME").unwrap_or_else(|_| "_SYSTEM".to_string()),
        )
        .env(
            "IRIS_PASSWORD",
            std::env::var("IRIS_PASSWORD").unwrap_or_else(|_| "SYS".to_string()),
        )
        .env(
            "IRIS_NAMESPACE",
            ns_override.map(str::to_string).unwrap_or_else(|| {
                std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string())
            }),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn iris-dev mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut results = vec![];
    let mut last_call = std::time::Duration::ZERO;

    for msg in messages.iter() {
        let sent_at = std::time::Instant::now();
        stdin
            .write_all((serde_json::to_string(msg).unwrap() + "\n").as_bytes())
            .unwrap();
        stdin.flush().unwrap();
        if msg.get("id").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let mut line = String::new();
                std::thread::sleep(std::time::Duration::from_millis(50));
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        results.push(v);
                        last_call = sent_at.elapsed();
                        break;
                    }
                }
                if std::time::Instant::now() > deadline {
                    break;
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    child.kill().ok();
    (results, last_call)
}

fn find_response(responses: &[serde_json::Value], id: u64) -> Option<serde_json::Value> {
    responses.iter().find(|r| r["id"] == id).cloned()
}

fn parse_tool_text(response: &serde_json::Value) -> serde_json::Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("{}");
    serde_json::from_str(text).unwrap_or_default()
}

#[test]
fn tools_list_returns_interop_profile() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }

    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ]);

    let tools_resp = find_response(&responses, 2).expect("no tools/list response");
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .expect("no tools array");
    let names: Vec<_> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Interop profile (fork default): exactly 29. The old range here allowed for the write
    // gate removing two tools on a read-only connection — #114 stopped it doing that, so the
    // slack was vestigial and would have hidden a tool going missing.
    //
    // #214 shipped with this count stale and only the dispatched CI run found it. The reason is
    // NOT that the test is #[ignore]d — it is not. It self-skips on an empty IRIS_HOST with an
    // early return, and the required gate deliberately runs `env -u IRIS_HOST`, so under the
    // gate this function returns before asserting anything and reports `ok`.
    //
    // Measured 2026-09-19: 8 non-#[ignore]d tests in this workspace self-skip that way, so the
    // gate's pass count includes 8 tests that asserted nothing — `e2e_all_tools_respond` among
    // them. A skip that reports `ok` is indistinguishable from a pass, which is why the CI
    // dispatch (where IRIS_HOST IS set) is the only thing that actually exercises this.
    assert!(
        names.len() == 29,
        "expected the interop profile (29 tools), got {}: {:?}",
        names.len(),
        names
    );
    // Consolidated interop dispatchers are present (not the old individual interop_* tools)
    for req in [
        "iris_production",
        "iris_interop_query",
        "iris_query",
        "iris_execute",
        "iris_test",
    ] {
        assert!(names.contains(&req), "interop tool '{}' missing", req);
    }
    // The old per-action interop tools are consolidated away in this profile
    for gone in [
        "interop_production_status",
        "interop_logs",
        "interop_queues",
        "interop_message_search",
    ] {
        assert!(!names.contains(&gone), "old tool '{}' should be gone", gone);
    }
    for name in &names {
        assert!(!name.contains('.'), "tool '{}' has dot", name);
    }
}

#[test]
fn interop_production_status_returns_structured_json() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }

    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production","arguments":{"action":"status","namespace":interop_ns()}}}),
    ]);

    let resp = find_response(&responses, 2).expect("no tool response");
    let result = parse_tool_text(&resp);
    // Interop-enabled namespace: either a running production (state) or a clean NO_PRODUCTION —
    // both prove the interop engine answered (not a missing-class error).
    assert!(
        result["state"].is_string() || result["error_code"] == "NO_PRODUCTION",
        "iris_production status must be structured (state or NO_PRODUCTION): {}",
        result
    );
}

#[test]
fn interop_logs_returns_structured_entries() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }

    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_interop_query","arguments":{"what":"logs","limit":5,"log_type":"error","namespace":interop_ns()}}}),
    ]);

    let resp = find_response(&responses, 2).expect("no tool response");
    let result = parse_tool_text(&resp);
    assert_eq!(
        result["success"], true,
        "iris_interop_query what=logs must succeed on an interop ns: {}",
        result
    );
}

#[test]
fn interop_queues_returns_array() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }

    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_interop_query","arguments":{"what":"queues","namespace":interop_ns()}}}),
    ]);

    let resp = find_response(&responses, 2).expect("no tool response");
    let result = parse_tool_text(&resp);
    assert_eq!(
        result["success"], true,
        "iris_interop_query what=queues must succeed on an interop ns: {}",
        result
    );
}

// B8/B9: partners introspection + required-with-enum `what`.
#[test]
fn interop_query_partners_and_what_enum() {
    if std::env::var("IRIS_HOST").unwrap_or_default().is_empty() {
        return;
    }
    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_interop_query","arguments":{"what":"partners","namespace":interop_ns()}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"iris_interop_query","arguments":{"what":"bogus","namespace":interop_ns()}}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"iris_interop_query","arguments":{"namespace":interop_ns()}}}),
    ]);
    // B8: partners returns a real (possibly empty) array on an interop ns.
    let partners = parse_tool_text(&find_response(&responses, 2).expect("no partners response"));
    assert_eq!(
        partners["success"], true,
        "partners must succeed: {}",
        partners
    );
    assert!(
        partners["partners"].is_array(),
        "partners must be an array: {}",
        partners
    );
    // B9: unknown / missing `what` fail fast with the valid set.
    let bad = parse_tool_text(&find_response(&responses, 3).expect("no bad-what response"));
    assert_eq!(bad["error_code"], "INVALID_WHAT", "bad what: {}", bad);
    let missing = parse_tool_text(&find_response(&responses, 4).expect("no missing-what response"));
    assert_eq!(
        missing["error_code"], "MISSING_WHAT",
        "missing what: {}",
        missing
    );
}

// ─── 024-interop-depth E2E stubs ───
// These tests run against a live IRIS instance with Interoperability enabled.
// They are #[ignore] by default; run with `cargo test -- --ignored` to execute.

#[test]
#[ignore = "requires live IRIS with Interoperability and a running production"]
fn test_production_item_enable_disable() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let item = std::env::var("TEST_PROD_ITEM").unwrap_or_else(|_| "TestService".to_string());
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());

    // disable
    let (responses, call_took) = mcp_exchange_timed_with_toolset(
        None,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production_item","arguments":{"action":"disable","item":item,"namespace":ns}}}),
        ],
    );
    assert!(
        call_took.as_secs() < 3,
        "SC-003: the tool call itself took {call_took:?} (budget 3s)"
    );
    let resp = find_response(&responses, 2).expect("no response");
    let result = parse_tool_text(&resp);
    assert!(
        result.get("success").is_some() || result.get("error_code").is_some(),
        "must return success or error_code"
    );

    // re-enable
    let responses2 = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production_item","arguments":{"action":"enable","item":item,"namespace":ns}}}),
    ]);
    let resp2 = find_response(&responses2, 2).expect("no response");
    let result2 = parse_tool_text(&resp2);
    assert!(result2.get("success").is_some() || result2.get("error_code").is_some());
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_credential_crud() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());
    let cred_id = "IrisDevTestCred";

    // list — assert no password in response
    let (responses, call_took) = mcp_exchange_timed_with_toolset(
        None,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_credential_list","arguments":{"namespace":ns}}}),
        ],
    );
    assert!(
        call_took.as_secs() < 3,
        "SC-003: the list call itself took {call_took:?} (budget 3s)"
    );
    let resp = find_response(&responses, 2).expect("no response");
    let raw_text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !raw_text.contains("\"password\""),
        "password must not appear in credential list"
    );
    assert!(
        !raw_text.contains("\"Password\""),
        "Password must not appear in credential list"
    );

    // create
    let responses2 = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_credential_manage","arguments":{"action":"create","id":cred_id,"username":"testuser","password":"testpass","namespace":ns}}}),
    ]);
    let r2 = parse_tool_text(&find_response(&responses2, 2).expect("no response"));
    assert!(r2["success"] == true || r2.get("error_code").is_some());

    // delete (cleanup)
    let responses3 = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_credential_manage","arguments":{"action":"delete","id":cred_id,"namespace":ns}}}),
    ]);
    let r3 = parse_tool_text(&find_response(&responses3, 2).expect("no response"));
    assert!(r3["success"] == true || r3.get("error_code").is_some());
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_lookup_crud() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());
    let table = "IrisDevTestTable";

    // set 3 keys — Key3's value carries a quote, an apostrophe and an accent:
    // the old SQL-style escaping corrupted ' to '' and died with <SYNTAX> on "
    for (key, val) in &[("Key1", "Val1"), ("Key2", "Val2"), ("Key3", "Va\"l'ñ3")] {
        let (responses, call_took) = mcp_exchange_timed_with_toolset(
            None,
            &[
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_manage","arguments":{"action":"set","table":table,"key":key,"value":val,"namespace":ns}}}),
            ],
        );
        assert!(
            call_took.as_secs() < 3,
            "SC-003: the set call itself took {call_took:?} (budget 3s)"
        );
        let r = parse_tool_text(&find_response(&responses, 2).expect("no response"));
        assert_eq!(r["success"], true, "set {key} failed: {r}");
    }

    // list_tables — assert table present
    let resp_lt = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_manage","arguments":{"action":"list_tables","namespace":ns}}}),
    ]);
    let lt = parse_tool_text(&find_response(&resp_lt, 2).expect("no response"));
    assert_eq!(lt["success"], true, "list_tables failed: {lt}");
    let empty = vec![];
    let tables = lt["tables"].as_array().unwrap_or(&empty);
    assert!(
        tables.iter().any(|t| t.as_str() == Some(table)),
        "table must appear in list_tables"
    );

    // export
    let resp_ex = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_transfer","arguments":{"action":"export","table":table,"namespace":ns}}}),
    ]);
    let ex = parse_tool_text(&find_response(&resp_ex, 2).expect("no response"));
    assert_eq!(ex["success"], true, "export failed: {ex}");
    let xml = ex["xml"].as_str().unwrap_or("");
    assert!(!xml.is_empty(), "export must return the XML");

    // delete keys
    for key in &["Key1", "Key2", "Key3"] {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_manage","arguments":{"action":"delete","table":table,"key":key,"namespace":ns}}}),
        ]);
        let _ = find_response(&responses, 2);
    }

    // import and verify round-trip — issue #6: this failed 7/7 with <SYNTAX>
    let resp_im = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_transfer","arguments":{"action":"import","table":table,"xml":xml,"namespace":ns}}}),
    ]);
    let im = parse_tool_text(&find_response(&resp_im, 2).expect("no response"));
    assert_eq!(im["success"], true, "import failed: {im}");

    // verify values restored, including the quote/apostrophe/accent one
    for (key, want) in &[("Key1", "Val1"), ("Key3", "Va\"l'ñ3")] {
        let resp_get = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_manage","arguments":{"action":"get","table":table,"key":key,"namespace":ns}}}),
        ]);
        let g = parse_tool_text(&find_response(&resp_get, 2).expect("no response"));
        assert_eq!(g["success"], true, "get {key} after import failed: {g}");
        assert_eq!(
            g["value"].as_str(),
            Some(*want),
            "SC-005: round-trip value must match for {key}"
        );
    }

    // leave the namespace clean
    for key in &["Key1", "Key2", "Key3"] {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_lookup_manage","arguments":{"action":"delete","table":table,"key":key,"namespace":ns}}}),
        ]);
        let _ = find_response(&responses, 2);
    }
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_production_autostart() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());

    // get current state
    let (responses, call_took) = mcp_exchange_timed_with_toolset(
        None,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production","arguments":{"action":"get_autostart","namespace":ns}}}),
        ],
    );
    assert!(
        call_took.as_secs() < 3,
        "SC-003: the get_autostart call itself took {call_took:?} (budget 3s)"
    );
    let r = parse_tool_text(&find_response(&responses, 2).expect("no response"));
    assert!(
        r["success"] == true || r.get("error_code").is_some(),
        "must return success or error_code"
    );

    // set disabled
    let r2_resp = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production","arguments":{"action":"set_autostart","namespace":ns,"enabled":false}}}),
    ]);
    let r2 = parse_tool_text(&find_response(&r2_resp, 2).expect("no response"));
    assert!(r2["success"] == true || r2.get("error_code").is_some());

    // confirm disabled
    let r3_resp = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_production","arguments":{"action":"get_autostart","namespace":ns}}}),
    ]);
    let r3 = parse_tool_text(&find_response(&r3_resp, 2).expect("no response"));
    if r3["success"] == true {
        assert_eq!(
            r3["autostart_enabled"], false,
            "autostart must be disabled after set_autostart false"
        );
    }
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_namespace_default_and_interop_hint() {
    // Issue #5: omitting `namespace` failed 95% of the time because it fell
    // back to a hardcoded "USER" instead of the connection namespace, and the
    // failure surfaced as raw internals ("Table 'ENS_CONFIG.CREDENTIALS' not
    // found") that never named the cause.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");

    // #240: this used to require `IRIS_NAMESPACE != "USER"` and then assert that namespace
    // "USER" comes back NAMESPACE_NOT_INTEROP. Both halves are assumptions about how the
    // instance happens to be configured, and on CI's pinned image BOTH are false: the job sets
    // IRIS_NAMESPACE=USER, and on intersystemsdc/iris-community:2025.3 `USER` IS
    // interop-enabled (measured with the product's own predicate, plus a positive control on
    // %Studio.Project and a negative control on a class that does not exist). The test was
    // therefore unsatisfiable on that container — not merely unmet, unsatisfiable, since the
    // image has only %SYS and USER.
    //
    // `%SYS` answers the question without assuming anything: it exists on every IRIS and is
    // never interop-enabled. Measured against a live instance it returns exactly what this test
    // is about — NAMESPACE_NOT_INTEROP, a hint naming `namespace=`, and the namespaces listed.
    let conn_ns = std::env::var("IRIS_NAMESPACE").unwrap_or_default();

    // 1. No namespace argument at all → must target the connection namespace
    //    and succeed (this exact call failed 14/14 in the workshop data).
    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_credential_list","arguments":{}}}),
    ]);
    let r = parse_tool_text(&find_response(&responses, 2).expect("no response"));
    // If the connection namespace is not interop-enabled, this test's premise is about the
    // operator's configuration rather than the product. Say so out loud and stop — a silent
    // pass would read as coverage.
    if r["error_code"] == "NAMESPACE_NOT_INTEROP" {
        // Same rule as #240 applies to the container test: a skip that can fire inside the CI
        // job is green-by-absence. CI's image has an interop-enabled USER, so this must not
        // trigger there.
        let on_ci = std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "false");
        assert!(
            !on_ci,
            "on CI the connection namespace must be interop-enabled — skipping here would \
             report coverage this job did not have: {r}"
        );
        println!(
            "SKIP test_namespace_default_and_interop_hint: the connection namespace ({}) is \
             not interop-enabled, so part 1 cannot succeed here: {r}",
            if conn_ns.is_empty() {
                "unset"
            } else {
                &conn_ns
            }
        );
        return;
    }
    assert_eq!(r["success"], true, "credential_list without namespace: {r}");

    // 2. Explicit non-interop namespace → self-describing error with a hint,
    //    not a raw SQL/table error.
    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_credential_list","arguments":{"namespace":"%SYS"}}}),
    ]);
    let r = parse_tool_text(&find_response(&responses, 2).expect("no response"));
    assert_eq!(r["success"], false, "%SYS must be rejected: {r}");
    assert_eq!(
        r["error_code"], "NAMESPACE_NOT_INTEROP",
        "self-describing code, got: {r}"
    );
    let hint = r["hint"].as_str().unwrap_or("");
    assert!(
        hint.contains("namespace="),
        "hint must name the parameter: {hint}"
    );
    let listed = r["interop_namespaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !listed.is_empty(),
        "the hint must LIST the interop namespaces, not just say there are some: {r}"
    );
    // Part 1 proved the connection namespace is interop-enabled, so it must appear here. Only
    // assert it when IRIS_NAMESPACE actually named one — unset means the server chose, and this
    // test cannot know what it chose.
    if !conn_ns.is_empty() {
        assert!(
            listed.iter().any(|v| v.as_str() == Some(conn_ns.as_str())),
            "the connection namespace '{conn_ns}' succeeded in part 1, so it must be in the \
             listed interop namespaces: {r}"
        );
    }
}

#[test]
#[ignore = "requires live IRIS"]
fn test_execute_and_query_namespace_defaults_to_connection() {
    // Issue #15: iris_execute (and the other general tools) with no `namespace`
    // argument ran in a hardcoded USER — never the server's configured
    // IRIS_NAMESPACE — and succeeded silently against the wrong database.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");

    // #240: this required the operator to point IRIS_NAMESPACE at a non-USER namespace, because
    // "ran in the connection namespace" cannot be told from "ran in a hardcoded USER" when the
    // connection namespace IS USER. Sound reasoning, unusable precondition: CI sets
    // IRIS_NAMESPACE=USER, so the test failed there on its own third line.
    //
    // The discrimination is now intrinsic — the test starts the server in `%SYS`, which exists
    // on every IRIS and is never USER. Measured: `WRITE $NAMESPACE` answers `%SYS`, and
    // `Security.Users` is present in %SYS (1) and absent from USER (0), so the query half
    // distinguishes the two namespaces by itself rather than by assuming the operator's setup.
    let conn_ns = "%SYS";

    let exchange = |args: serde_json::Value, tool: &str| {
        let responses = mcp_exchange_in_namespace(
            conn_ns,
            &[
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
            ],
        );
        find_response(&responses, 2).expect("no response")
    };

    // 1. iris_execute without namespace → runs in the connection namespace,
    //    and the response names the namespace it ran in.
    let v = parse_tool_text(&exchange(
        serde_json::json!({"code":"WRITE $NAMESPACE"}),
        "iris_execute",
    ));
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(
        v["output"], conn_ns,
        "iris_execute ran in the wrong namespace: {v}"
    );
    assert_eq!(
        v["namespace"], conn_ns,
        "response must name the namespace it ran in: {v}"
    );

    // 2. iris_query without namespace → connection namespace too. `Security.Users` is a %SYS
    //    class: 1 there, 0 in USER, so the count itself proves which namespace ran and a
    //    hardcoded USER cannot produce this answer.
    let v = parse_tool_text(&exchange(
        serde_json::json!({"query":"SELECT COUNT(*) AS n FROM %Dictionary.CompiledClass WHERE Name = 'Security.Users'"}),
        "iris_query",
    ));
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(v["namespace"], conn_ns, "{v}");
    let n = v["rows"][0]["n"]
        .as_i64()
        .or_else(|| v["rows"][0]["n"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(-1);
    assert_eq!(
        n, 1,
        "Security.Users must be found — 0 would mean the query ran in USER, not the \
         connection namespace: {v}"
    );
}

#[test]
#[ignore = "requires live IRIS"]
fn test_search_scope_and_case_insensitive_default() {
    // Issue #17: iris_search never sent a `files=` scope (Atelier greps nothing
    // without one) and omitted `case=`, which Atelier treats as case-SENSITIVE —
    // so the natural lowercase search missed mixed-case code.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");

    // `iris_search` is a BASELINE tool, deliberately outside INTEROP_TOOLS, so the fork's
    // default profile never advertises it and every call here came back "tool not found" —
    // the SCOPE_REQUIRED assertion below was unreachable for a reason unrelated to #17.
    let exchange = |args: serde_json::Value, tool: &str| {
        let responses = mcp_exchange_with_toolset(
            Some("baseline"),
            &[
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
            ],
        );
        find_response(&responses, 2).expect("no response")
    };

    // Marker class with a MiXeD-case token; searched lowercase below.
    let marker =
        "Class IrisDevE2E.SearchMarker\n{\n/// carries the SeArChMaRkEr17 token\nClassMethod Noop()\n{\n    Quit\n}\n}\n";
    let frame = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.SearchMarker.cls","content":marker,"compile":false}),
        "iris_doc",
    );
    assert_ne!(
        frame["result"]["isError"], true,
        "marker PUT failed: {frame}"
    );

    // 1. No `documents` scope → explicit SCOPE_REQUIRED error, not empty results.
    let frame = exchange(serde_json::json!({"query":"searchmarker17"}), "iris_search");
    assert_eq!(
        frame["result"]["isError"], true,
        "scopeless search must be refused: {frame}"
    );
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "SCOPE_REQUIRED", "{v}");

    // 2. Lowercase query + scope → finds the MiXeD-case token (case-insensitive
    //    default). The pre-fix code returned 0 results here (no files= param).
    let frame = exchange(
        serde_json::json!({"query":"searchmarker17","documents":["IrisDevE2E.*.cls"]}),
        "iris_search",
    );
    assert_ne!(frame["result"]["isError"], true, "{frame}");
    let v = parse_tool_text(&frame);
    assert_eq!(v["success"], true, "{v}");
    let total = v["total_found"].as_i64().unwrap_or(0);
    assert!(
        total >= 1,
        "case-insensitive scoped search found nothing: {v}"
    );
    let hits = v["results"].as_array().cloned().unwrap_or_default();
    assert!(
        hits.iter().any(|r| r["document"]
            .as_str()
            .unwrap_or("")
            .contains("SearchMarker")),
        "marker class not in results: {v}"
    );

    // 3. case_sensitive:true with the wrong case → no hits for the marker.
    let frame = exchange(
        serde_json::json!({"query":"searchmarker17","documents":["IrisDevE2E.*.cls"],"case_sensitive":true}),
        "iris_search",
    );
    let v = parse_tool_text(&frame);
    let hits = v["results"].as_array().cloned().unwrap_or_default();
    assert!(
        !hits.iter().any(|r| r["document"]
            .as_str()
            .unwrap_or("")
            .contains("SearchMarker")),
        "case-sensitive search must miss the mixed-case token: {v}"
    );

    // Cleanup (best-effort).
    let _ = exchange(
        serde_json::json!({"mode":"delete","name":"IrisDevE2E.SearchMarker.cls"}),
        "iris_doc",
    );
}

#[test]
#[ignore = "requires live IRIS"]
fn test_doc_guards_storage_name_and_mode() {
    // Issue #18: STORAGE_STRIP_BLOCKED opt-in, MISSING_PARAMS on blank name,
    // INVALID_PARAM on unknown mode.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");

    let exchange = |args: serde_json::Value, tool: &str| {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
        ]);
        find_response(&responses, 2).expect("no response")
    };

    // 1. PUT of a class carrying an explicit Storage block → refused by default.
    let with_storage = "Class IrisDevE2E.StorageGuard Extends %Persistent\n{\nProperty P As %String;\n\nStorage Default\n{\n<Data name=\"D\">\n<Value name=\"1\"><Value>P</Value></Value>\n</Data>\n<DataLocation>^IrisDevE2E.SGD</DataLocation>\n}\n}\n";
    let frame = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.StorageGuard.cls","content":with_storage,"compile":false}),
        "iris_doc",
    );
    assert_eq!(frame["result"]["isError"], true, "{frame}");
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "STORAGE_STRIP_BLOCKED", "{v}");
    // #217: the refusal must name the fix (delete the block) ahead of the bypass flag.
    let refusal = v["error"].as_str().unwrap_or("");
    assert!(
        refusal.contains("FIX: delete the Storage block"),
        "refusal must lead with the fix: {refusal}"
    );

    // 2. Same PUT with the opt-in → proceeds (storage stripped, class written).
    let frame = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.StorageGuard.cls","content":with_storage,"compile":false,"allow_storage_regeneration":true}),
        "iris_doc",
    );
    assert_ne!(frame["result"]["isError"], true, "{frame}");
    let v = parse_tool_text(&frame);
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(v["storage_stripped"], true, "{v}");

    // 3. Blank name → MISSING_PARAMS, not an Atelier #16006 retry loop.
    let frame = exchange(serde_json::json!({"mode":"get"}), "iris_doc");
    assert_eq!(frame["result"]["isError"], true, "{frame}");
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "MISSING_PARAMS", "{v}");

    // 4. Unknown mode string → INVALID_PARAM (mode is a plain string now).
    let frame = exchange(
        serde_json::json!({"mode":"fragment","name":"X.cls"}),
        "iris_doc",
    );
    assert_eq!(frame["result"]["isError"], true, "{frame}");
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "INVALID_PARAM", "{v}");

    // Cleanup (best-effort).
    let _ = exchange(
        serde_json::json!({"mode":"delete","name":"IrisDevE2E.StorageGuard.cls"}),
        "iris_doc",
    );
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_error_envelope_and_is_error_flag() {
    // Issue #2: genuine tool failures must set isError on the CallToolResult
    // and carry one envelope {success:false, error_code, error [, hint]} —
    // 38/38 workshop failures came back unflagged, in per-tool shapes.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());

    let exchange = |args: serde_json::Value, tool: &str| {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
        ]);
        find_response(&responses, 2).expect("no response")
    };

    // 1. iris_query with bad SQL → isError + SQL_ERROR envelope
    let frame = exchange(
        serde_json::json!({"query":"SELECT COUNT(*) AS n FROM public.menus","namespace":ns}),
        "iris_query",
    );
    assert_eq!(
        frame["result"]["isError"], true,
        "SQL failure unflagged: {frame}"
    );
    let v = parse_tool_text(&frame);
    assert_eq!(v["success"], false);
    assert_eq!(v["error_code"], "SQL_ERROR");
    assert!(
        !v["error"].as_str().unwrap_or("").is_empty(),
        "message must live in `error`"
    );

    // 2. a good query stays non-error
    let frame = exchange(
        serde_json::json!({"query":"SELECT 1 AS one","namespace":ns}),
        "iris_query",
    );
    assert_ne!(
        frame["result"]["isError"], true,
        "success wrongly flagged: {frame}"
    );

    // 3. iris_execute runtime error → isError, message in `error`, raw kept in `output`
    let frame = exchange(
        serde_json::json!({"code":"Set x=oref.Nope()","namespace":ns}),
        "iris_execute",
    );
    assert_eq!(
        frame["result"]["isError"], true,
        "runtime error unflagged: {frame}"
    );
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "IRIS_RUNTIME_ERROR");
    assert!(v["error"].as_str().unwrap_or("").contains("ERROR"), "{v}");
    assert!(
        v["output"].as_str().is_some(),
        "raw output must stay as detail: {v}"
    );

    // 4. iris_doc compile failure → COMPILE_ERROR + hint + console kept (issue repro 3)
    let broken =
        "Class IrisDevE2E.Broken\n{\nClassMethod X()\n{\n    this is not objectscript\n}\n}\n";
    let frame = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.Broken.cls","content":broken,"compile":true,"namespace":ns}),
        "iris_doc",
    );
    assert_eq!(
        frame["result"]["isError"], true,
        "compile failure unflagged: {frame}"
    );
    let v = parse_tool_text(&frame);
    assert_eq!(v["error_code"], "COMPILE_ERROR", "{v}");
    assert!(!v["error"].as_str().unwrap_or("").is_empty());
    assert!(
        v["hint"].as_str().unwrap_or("").contains("compile_console"),
        "{v}"
    );
    assert!(
        v["compile_console"].is_array(),
        "console must stay as detail: {v}"
    );
    // clean up the broken class
    let _ = exchange(
        serde_json::json!({"mode":"delete","name":"IrisDevE2E.Broken.cls","namespace":ns}),
        "iris_doc",
    );

    // 5. iris_compile on a class that does not compile → isError + COMPILE_ERROR.
    //    Issue #46: this branch returned ok_json with success:false and no isError,
    //    so a spec-compliant client read a failed compile as a successful call and
    //    the agent kept iterating against a class it believed it had compiled.
    let broken_src = "Class IrisDevE2E.BrokenCompile\n{\nClassMethod X()\n{\n    this is not objectscript\n}\n}\n";
    let _ = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.BrokenCompile.cls","content":broken_src,"namespace":ns}),
        "iris_doc",
    );
    let frame = exchange(
        serde_json::json!({"target":"IrisDevE2E.BrokenCompile.cls","namespace":ns}),
        "iris_compile",
    );
    assert_eq!(
        frame["result"]["isError"], true,
        "failed compile unflagged: {frame}"
    );
    let v = parse_tool_text(&frame);
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(v["error_code"], "COMPILE_ERROR", "{v}");
    assert!(
        !v["error"].as_str().unwrap_or("").is_empty(),
        "message must live in `error`: {v}"
    );
    assert!(
        v["errors"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "diagnostics must stay as detail: {v}"
    );

    // 6. a compile that succeeds must NOT be flagged
    let good_src =
        "Class IrisDevE2E.GoodCompile\n{\nClassMethod X() As %String\n{\n    Return \"ok\"\n}\n}\n";
    let _ = exchange(
        serde_json::json!({"mode":"put","name":"IrisDevE2E.GoodCompile.cls","content":good_src,"namespace":ns}),
        "iris_doc",
    );
    let frame = exchange(
        serde_json::json!({"target":"IrisDevE2E.GoodCompile.cls","namespace":ns}),
        "iris_compile",
    );
    assert_ne!(
        frame["result"]["isError"], true,
        "successful compile wrongly flagged: {frame}"
    );
    assert_eq!(parse_tool_text(&frame)["success"], true, "{frame}");

    for name in ["IrisDevE2E.BrokenCompile.cls", "IrisDevE2E.GoodCompile.cls"] {
        let _ = exchange(
            serde_json::json!({"mode":"delete","name":name,"namespace":ns}),
            "iris_doc",
        );
    }
}

#[test]
#[ignore = "requires live IRIS with Interoperability"]
fn test_message_content_search() {
    // Issue #4: typed search on message CONTENT — body-class join and Search
    // Tables — replacing the hand SQL that produced 57 <SYNTAX> errors in one
    // workshop day.
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    assert!(!iris_host.is_empty(), "IRIS_HOST must be set");
    let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".to_string());

    let call = |tool: &str, args: serde_json::Value| {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
        ]);
        parse_tool_text(&find_response(&responses, 2).expect("no response"))
    };

    // Seed one header+body fixture (no production needed — plain %Persistent rows).
    let needle = "e2e-needle-content-search";
    let seed = format!(
        "Set body=##class(Ens.StringContainer).%New()\n\
         Set body.StringValue=\"{needle}\"\n\
         Set tSC=body.%Save()\n\
         If $$$ISERR(tSC) {{ Write \"BODYFAIL\" Quit }}\n\
         Set hdr=##class(Ens.MessageHeader).%New()\n\
         Set hdr.MessageBodyClassName=\"Ens.StringContainer\"\n\
         Set hdr.MessageBodyId=body.%Id()\n\
         Set hdr.SourceConfigName=\"E2E.Source\"\n\
         Set tSC2=hdr.%Save()\n\
         If $$$ISERR(tSC2) {{ Write \"HDRFAIL\" Quit }}\n\
         Write \"OK:\"_hdr.%Id()_\":\"_body.%Id()"
    );
    let r = call(
        "iris_execute",
        serde_json::json!({"namespace": ns, "code": seed}),
    );
    let out = r["output"].as_str().unwrap_or("");
    assert!(out.starts_with("OK:"), "fixture seed failed: {r}");
    let ids: Vec<&str> = out.trim_start_matches("OK:").split(':').collect();
    let (hdr_id, body_id) = (ids[0].to_string(), ids[1].to_string());

    // 1. body-class join finds the needle and returns the body column.
    let r = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns,
            "body_class":"Ens.StringContainer",
            "body_where": format!("StringValue = '{needle}'"),
            "body_select":["StringValue"]}),
    );
    assert_eq!(r["success"], true, "body join failed: {r}");
    assert_eq!(r["count"], 1, "expected exactly the fixture: {r}");
    assert_eq!(r["messages"][0]["StringValue"], needle, "{r}");
    assert!(r["messages"][0]["SourceConfigName"].is_string(), "{r}");

    // 2. body_where without body_class → error that lists the real classes.
    let r = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns,"body_where":"X=1"}),
    );
    assert_eq!(r["error_code"], "INVALID_PARAMS", "{r}");
    assert!(
        r["hint"]
            .as_str()
            .unwrap_or("")
            .contains("Ens.StringContainer"),
        "hint must list case-exact body classes: {r}"
    );

    // 3. unknown body class → BODY_CLASS_NOT_FOUND.
    let r = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns,
            "body_class":"No.Such.Class","body_where":"X=1"}),
    );
    assert_eq!(r["error_code"], "BODY_CLASS_NOT_FOUND", "{r}");

    // 4. unknown search-table prop → the searchable props are listed.
    let r = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns,
            "search_table":{"prop":"NoSuchProp","value":"x"}}),
    );
    assert_eq!(r["error_code"], "SEARCH_PROP_NOT_FOUND", "{r}");
    // #240: this asserted `available_props` contains "PatientID", which requires a
    // SearchTableClass to be configured on some production item. CI's bare container has no
    // production at all, so `EnsLib.HL7.SearchTable` has no registered properties there and the
    // assertion could not hold — the tool's answer was correct.
    //
    // Both branches assert something real, so neither is a silent skip. Where the extent HAS
    // properties, they must be listed. Where it has none, the interesting property is the one
    // this repo keeps having to fix: an empty list must arrive WITH the reason, not bare.
    let props = r["available_props"].as_array().cloned().unwrap_or_default();
    let search_props_registered = !props.is_empty();
    if props.is_empty() {
        let hint = r["hint"].as_str().unwrap_or("");
        assert!(
            hint.contains("no registered properties"),
            "an empty available_props must say WHY it is empty, or it reads as 'this extent \
             has no such field': {r}"
        );
        assert!(
            hint.contains("SearchTableClass"),
            "the hint must name what to configure: {r}"
        );
    } else {
        assert!(
            props.iter().any(|v| v.as_str() == Some("PatientID")),
            "available_props must list the extent's fields: {r}"
        );
    }

    // ── #202: a filter that is DROPPED rather than applied ────────────────────
    //
    // Steps 4 and 5 both assert on an error or a zero, and a no-op filter produces
    // neither — it returns the whole archive with success:true. What distinguishes the
    // two is the row COUNT against a known corpus, so measure the unfiltered baseline
    // first and require the filtered calls to differ from it. A zero is only evidence
    // once the same call has been seen returning non-zero.
    let unfiltered = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns}),
    );
    assert_eq!(unfiltered["success"], true, "{unfiltered}");
    let baseline = unfiltered["count"].as_u64().unwrap_or(0);
    assert!(
        baseline > 0,
        "positive control: the fixture header must be in the unfiltered archive, \
         else `count == 0` below proves nothing: {unfiltered}"
    );

    // 5 and 6 need a prop that is actually REGISTERED on the extent, which requires a
    // SearchTableClass configured on some production item. #240: CI's container has no
    // production, so `EnsLib.HL7.SearchTable` has no registered properties there and no prop
    // name is valid — step 4 above measured that.
    //
    // The #202 property survives either way, and it is the one that matters: a filter that
    // cannot be honoured must NEVER degrade into an unfiltered search. So both environments
    // assert it; only the shape of "honoured" differs.
    let filters = [
        serde_json::json!({"prop":"MSHControlID","value":"e2e-no-such-value"}),
        // The #202 trigger: the filter arriving as a JSON *string*, which is what a client
        // with no shape to serialise against sends. It used to fail from_value(), become
        // None, and return all `baseline` rows as a success.
        serde_json::json!(r#"{"prop":"MSHControlID","value":"e2e-no-such-value"}"#),
    ];
    for filter in &filters {
        let r = call(
            "iris_interop_query",
            serde_json::json!({"what":"messages","namespace":ns,"search_table":filter.clone()}),
        );
        if search_props_registered {
            // 5/6. valid prop, zero rows → success with the config-time indexing hint.
            assert_eq!(
                r["success"], true,
                "a filter on a registered prop must be honoured: {r}"
            );
            assert_eq!(
                r["count"], 0,
                "the filter must FILTER, not degrade into no filter (baseline is \
                 {baseline} rows): {r}"
            );
            assert!(
                r["hint"].as_str().unwrap_or("").contains("back-indexed"),
                "zero rows must explain config-time indexing: {r}"
            );
        } else {
            // No prop can be valid here, so the filter cannot be honoured — and that is
            // precisely when #202's regression would return the whole archive as a success.
            assert_eq!(
                r["success"], false,
                "an unhonourable filter must be refused, not answered: {r}"
            );
            assert_eq!(r["error_code"], "SEARCH_PROP_NOT_FOUND", "{r}");
            // `success == false` above is the real #202 guard. This one only bites when a
            // count IS reported, so say that explicitly rather than let `unwrap_or(MAX)`
            // turn an absent field into a silent pass — the defect being fixed all over
            // this commit.
            if let Some(n) = r["count"].as_u64() {
                assert_ne!(
                    n, baseline,
                    "a filter that cannot be honoured must never return the {baseline} \
                     unfiltered rows — that is #202: {r}"
                );
            }
        }
    }

    // 7. A filter that cannot be honoured is an error — never an unfiltered search.
    //    Each of these used to deserialise to None and return all `baseline` rows.
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"value": "x"}),
        serde_json::json!({"prop": "MSHControlID", "value": 42}),
        serde_json::json!("MSHControlID"),
    ] {
        let r = call(
            "iris_interop_query",
            serde_json::json!({"what":"messages","namespace":ns,"search_table":bad.clone()}),
        );
        assert_eq!(
            r["success"], false,
            "search_table={bad} must be refused, not answered with {baseline} unfiltered rows: {r}"
        );
        assert_eq!(r["error_code"], "INVALID_PARAM", "{r}");
        assert!(
            r["error"].as_str().unwrap_or("").contains("search_table"),
            "the error has to name the parameter: {r}"
        );
    }

    // 8. The no-value filter reaches the handler's own check instead of vanishing.
    //    In the #202 repro this call returned rows, which is what proved the filter
    //    never got there: this error is unreachable unless it did.
    let r = call(
        "iris_interop_query",
        serde_json::json!({"what":"messages","namespace":ns,
            "search_table":{"prop":"MSHControlID"}}),
    );
    assert_eq!(r["success"], false, "{r}");
    assert_eq!(r["error_code"], "INVALID_PARAMS", "{r}");
    assert!(
        r["error"].as_str().unwrap_or("").contains("value_like"),
        "the message must name the two alternatives: {r}"
    );

    // Clean up the fixture.
    let cleanup = format!(
        "Do ##class(Ens.MessageHeader).%DeleteId({hdr_id})\n\
         Do ##class(Ens.StringContainer).%DeleteId({body_id})\n\
         Write \"CLEAN\""
    );
    let r = call(
        "iris_execute",
        serde_json::json!({"namespace": ns, "code": cleanup}),
    );
    assert_eq!(r["output"].as_str(), Some("CLEAN"), "cleanup failed: {r}");
}

/// #247: the includes are DERIVED from `docname`, not demanded from the caller.
///
/// This is the assertion that a unit test cannot make. Measured against the worker the REST
/// layer calls (`$$GetMacroLocation^%qccServer`) on IRIS for Health 2026.1, with both controls
/// on the very class used here:
///
/// ```text
/// includes %occInclude,Ensemble (Ens.Director's own IncludeCode) -> %occErrors.inc(1415)
/// no includes at all                                             -> empty
/// ```
///
/// So an empty answer here would mean the derivation never reached IRIS, and a resolved one
/// cannot have come from anywhere else: the caller passes NO includes.
#[test]
#[ignore = "requires live IRIS"]
fn macro_includes_are_derived_from_the_document() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }

    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        // Ens.Director exists in every interop-enabled namespace and its IncludeCode is
        // "%occInclude,Ensemble". No `includes` argument is sent.
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_macro","arguments":{"action":"location","name":"$$$GeneralError","docname":"Ens.Director.cls","namespace":interop_ns()}}}),
    ]);

    let resp = find_response(&responses, 2).expect("no tool response");
    let result = parse_tool_text(&resp);

    assert_eq!(
        result["includes_from_document"],
        serde_json::json!(["%occInclude", "Ensemble"]),
        "Ens.Director's Include list was not read back: {result}"
    );
    assert_eq!(
        result["resolved"],
        serde_json::json!(true),
        "the derived includes did not reach IRIS — this is the empty answer that reads like \
         'no such macro': {result}"
    );
    assert!(
        result["result"]["document"]
            .as_str()
            .unwrap_or_default()
            .contains("%occErrors.inc"),
        "GeneralError must resolve to %occErrors.inc: {result}"
    );
}

/// #246: the categories come from IRIS, not from a hardcoded list.
#[test]
#[ignore = "requires live IRIS"]
fn hl7_schema_list_returns_this_instances_categories() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }
    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hl7_schema_list","arguments":{"namespace":interop_ns()}}}),
    ]);
    let result = parse_tool_text(&find_response(&responses, 2).expect("no tool response"));

    // A plain IRIS has no HL7 schemas at all, and saying so is a correct answer — but it must
    // arrive as HL7_NOT_AVAILABLE, never as an empty list of categories.
    if result["error_code"] == "HL7_NOT_AVAILABLE" {
        return;
    }
    let cats = result["categories"]
        .as_array()
        .unwrap_or_else(|| panic!("no categories array: {result}"));
    assert!(
        !cats.is_empty(),
        "an IRIS for Health instance has schema categories; an empty list means the read \
         failed: {result}"
    );
    let names: Vec<&str> = cats.iter().filter_map(|c| c["category"].as_str()).collect();
    assert!(
        names.contains(&"2.5"),
        "2.5 ships with every IRIS for Health: {names:?}"
    );
}

/// #246's acceptance criterion, as a test.
///
/// `EnsLib.HL7.Schema.GetFieldNameFromNumber("2.5","PID","3")` returns `""` because it compares
/// the stored `3()` against a bare `3`. This asserts the tool does NOT inherit that gap: PID:3
/// and PID:5 must come back NAMED and flagged as repeating, and PID:7 — which does not repeat —
/// must not be flagged. Both directions, so a hardcoded `repeating: true` would fail.
#[test]
#[ignore = "requires live IRIS"]
fn hl7_schema_inspect_names_repeating_fields() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }
    let responses = mcp_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hl7_schema_inspect","arguments":{"version":"2.5","segment":"PID","namespace":interop_ns()}}}),
    ]);
    let result = parse_tool_text(&find_response(&responses, 2).expect("no tool response"));
    if result["error_code"] == "HL7_NOT_AVAILABLE" {
        return;
    }

    let fields = result["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("no fields array: {result}"));
    assert!(
        fields.len() > 30,
        "PID in 2.5 has 39 fields; a short list means a partial read: {}",
        fields.len()
    );

    let by_number = |n: &str| -> serde_json::Value {
        fields
            .iter()
            .find(|f| f["number"] == n)
            .unwrap_or_else(|| panic!("PID:{n} missing from {} fields", fields.len()))
            .clone()
    };

    let f3 = by_number("3");
    assert_eq!(
        f3["name"], "PatientIdentifierList",
        "PID:3 must be NAMED — GetFieldNameFromNumber returns \"\" here: {f3}"
    );
    assert_eq!(f3["repeating"], serde_json::json!(true), "{f3}");

    let f5 = by_number("5");
    assert_eq!(f5["name"], "PatientName", "{f5}");
    assert_eq!(f5["repeating"], serde_json::json!(true), "{f5}");

    // The other direction: a non-repeating field must not be flagged, or `repeating` carries
    // no information.
    let f7 = by_number("7");
    assert_eq!(
        f7["repeating"],
        serde_json::json!(false),
        "PID:7 does not repeat: {f7}"
    );
    assert_eq!(f7["data_type"], "TS", "{f7}");
}

/// #248: the stream global, which is where an HL7 message's content actually lives.
///
/// Filed as "port `resolve_storage`". Both of that issue's acceptance criteria —
/// `DataLocation` and the index globals — were already met by `iris_table_info`, and upstream's
/// `resolve_storage` selects neither `StreamLocation` nor anything else this tool lacked. So the
/// gap was one column, not a tool.
///
/// `EnsLib.HL7.Message` is the case that matters: measured on IRIS for Health 2026.1 it stores
/// to `^EnsLib.H.MessageD` / `^EnsLib.H.MessageS` — abbreviated globals that cannot be derived
/// from the class name, which is exactly why a tool has to report them.
#[test]
#[ignore = "requires live IRIS"]
fn table_info_reports_the_stream_global() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }
    let ask = |table: &str| -> serde_json::Value {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_table_info","arguments":{"table":table,"namespace":interop_ns()}}}),
        ]);
        parse_tool_text(&find_response(&responses, 2).expect("no tool response"))
    };

    // Ens.MessageHeader exists in every interop-enabled namespace.
    let r = ask("Ens.MessageHeader");
    let res = &r["result"];
    assert_eq!(res["type"], "class_projection", "{r}");
    assert_eq!(res["data_global"], "^Ens.MessageHeaderD", "{r}");
    assert_eq!(res["index_global"], "^Ens.MessageHeaderI", "{r}");
    // Both of these were previously unreported: IDLocation was SELECTed and discarded, and
    // StreamLocation was never asked for.
    assert_eq!(
        res["stream_global"], "^Ens.MessageHeaderS",
        "the stream global is the #248 gap: {r}"
    );
    assert!(
        res["id_global"].is_string(),
        "id_global was already measured and thrown away: {r}"
    );

    // ExtentSize is deliberately absent — it is the optimizer's declared hint (measured: 1476
    // of 3177 storage rows carry exactly "100000"), not a row count, and `include_row_count`
    // gives the real figure.
    assert!(
        res["extent_size"].is_null(),
        "extent_size must not be reported as if it were a row count: {r}"
    );

    // An HL7 message body: the global name is abbreviated and unguessable, which is the whole
    // reason this has to come from IRIS rather than from a naming convention.
    let r = ask("EnsLib.HL7.Message");
    let res = &r["result"];
    // Not `if type == "class_projection" { .. }`: that lets a resolution failure silently delete
    // the assertion that carries the whole argument. Either the class resolved and the stream
    // global is the abbreviated one, or the tool said plainly that it could not find the table.
    match res["type"].as_str() {
        Some("class_projection") => {
            let stream = res["stream_global"]
                .as_str()
                .unwrap_or_else(|| panic!("class_projection with no stream_global: {r}"));
            assert!(
                stream.starts_with('^') && stream.ends_with('S'),
                "stream global must be a global name: {r}"
            );
            assert_ne!(
                stream, "^EnsLib.HL7.MessageS",
                "the point of reporting it: IRIS abbreviates this one, so a name derived from the \
                 class would be wrong: {r}"
            );
            assert_eq!(
                stream, "^EnsLib.H.MessageS",
                "measured on IRIS for Health 2026.1: {r}"
            );
        }
        // HL7 is not installed in every namespace. That is allowed — but the tool has to SAY so,
        // not return a success envelope with the globals missing.
        _ => assert!(
            r["success"] == false || res["error_code"].is_string() || r["error_code"].is_string(),
            "EnsLib.HL7.Message neither resolved nor produced an explicit miss: {r}"
        ),
    }
}

/// #214: read an EXTERNAL PostgreSQL table through an IRIS SQL Gateway connection.
///
/// The issue's operational question is "how many rows are in `public.menus`?", and its cost is
/// that answering it meant `PGPASSWORD=... psql` — 32 invocations across 5 of 14 students — which
/// puts the credential in the transcript and in shell history, and verifies outside the interop
/// trace. This tool takes a connection NAME and no credential.
///
/// The rig is `e2e/gateway/docker-compose.yaml` (PostgreSQL 17 joined to the network that hosts
/// the dev IRIS) plus a `PG_COCINA_E2E` SQL Gateway connection pointing at a SELECT-only role.
/// Where the rig is absent the tool must say so in a way that names the fix — so that branch
/// asserts too, rather than returning and calling itself a pass.
#[test]
#[ignore = "requires live IRIS"]
fn gateway_query_reads_the_external_postgres_table() {
    let iris_host = std::env::var("IRIS_HOST").unwrap_or_default();
    if iris_host.is_empty() {
        return;
    }
    let ask = |args: serde_json::Value| -> serde_json::Value {
        let responses = mcp_exchange(&[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_gateway_query","arguments":args}}),
        ]);
        parse_tool_text(&find_response(&responses, 2).expect("no tool response"))
    };

    // A mutating statement must be refused BEFORE any connection is touched. This holds whether
    // or not the rig is present, so it is asserted first and unconditionally.
    let refused = ask(serde_json::json!({
        "connection": "PG_COCINA_E2E",
        "query": "INSERT INTO public.menus (paciente_id) VALUES (9)",
        "namespace": "%SYS",
    }));
    assert_eq!(
        refused["error_code"], "SQL_NOT_READ_ONLY",
        "a write must be refused before the database is reached: {refused}"
    );
    let refused_msg = refused["error"].as_str().unwrap_or_default().to_string();
    assert!(
        refused_msg.contains("Nothing was sent"),
        "the refusal must say nothing was sent: {refused}"
    );

    // A PostgreSQL-specific mutator that the shared IRIS screen does not know.
    let copy = ask(serde_json::json!({
        "connection": "PG_COCINA_E2E",
        "query": "COPY public.menus FROM '/tmp/evil.csv'",
        "namespace": "%SYS",
    }));
    assert_eq!(
        copy["error_code"], "SQL_NOT_READ_ONLY",
        "COPY is a Postgres mutator the IRIS keyword list does not carry: {copy}"
    );

    let r = ask(serde_json::json!({
        "connection": "PG_COCINA_E2E",
        "query": "SELECT id_menu, paciente_id, descripcion, calorias FROM public.menus ORDER BY id_menu",
        "namespace": "%SYS",
    }));

    // Where the rig is not deployed the tool must name the fix. This branch is asserted, not
    // skipped: a helpful refusal is the contract when the connection is absent.
    if r["error_code"] == "GATEWAY_CONNECTION_NOT_DEFINED" {
        let msg = r["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("PG_COCINA_E2E") && msg.contains("SQL Gateway Connections"),
            "the not-defined message must name the connection and where to define it: {r}"
        );
        assert!(
            msg.contains("never accepts a credential"),
            "the refusal must say the tool takes no credential: {r}"
        );
        eprintln!("gateway rig absent; asserted the refusal contract instead");
        return;
    }

    // ok_json puts the payload at the TOP level; only iris_table_info nests a `result` key.
    let res = &r;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(res["connection"], "PG_COCINA_E2E", "{r}");

    // The remote schema's OWN column names and types, which is what makes the answer usable.
    let cols = res["columns"].as_array().expect("columns array");
    assert_eq!(cols.len(), 4, "{r}");
    assert_eq!(cols[0]["name"], "id_menu", "{r}");
    assert_eq!(cols[2]["name"], "descripcion", "{r}");
    assert_eq!(
        cols[2]["type_name"], "text",
        "the EXTERNAL type name, not an IRIS type: {r}"
    );

    // The question #214 says no tool could answer.
    assert_eq!(res["row_count"], 5, "public.menus has 5 seeded rows: {r}");
    let rows = res["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 5, "{r}");

    // UTF-8 must survive IRIS -> JDBC -> Postgres and back. os_str_expr splices non-ASCII as
    // $CHAR, so this is the path that would break silently if that went wrong.
    let joined = rows
        .iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("Puré de patata"),
        "accented external text must round-trip: {joined}"
    );
    assert!(
        joined.contains("calabacín"),
        "accented external text must round-trip: {joined}"
    );

    // It reads the table it was ASKED for. pacientes has 3 rows where menus has 5, so a tool
    // returning "whatever the connection offers first" cannot pass both assertions.
    let p = ask(serde_json::json!({
        "connection": "PG_COCINA_E2E",
        "query": "SELECT count(*) FROM public.pacientes",
        "namespace": "%SYS",
    }));
    assert_eq!(p["success"], true, "{p}");
    assert_eq!(
        p["rows"][0][0], "3",
        "pacientes has 3 rows, menus has 5: {p}"
    );

    // max_rows truncates and SAYS it truncated. A silent cap would read as "that is all there is".
    let capped = ask(serde_json::json!({
        "connection": "PG_COCINA_E2E",
        "query": "SELECT id_menu FROM public.menus ORDER BY id_menu",
        "max_rows": 2,
        "namespace": "%SYS",
    }));
    assert_eq!(capped["row_count"], 2, "{capped}");
    assert_eq!(capped["truncated"], true, "{capped}");
    assert!(
        capped["note"]
            .as_str()
            .unwrap_or_default()
            .contains("truncated"),
        "a truncated result must say so: {capped}"
    );

    // An undefined connection names the fix rather than failing opaquely.
    let missing = ask(serde_json::json!({
        "connection": "NO_SUCH_GATEWAY_CONN_ZZZ",
        "query": "SELECT 1",
        "namespace": "%SYS",
    }));
    assert_eq!(
        missing["error_code"], "GATEWAY_CONNECTION_NOT_DEFINED",
        "{missing}"
    );
}
