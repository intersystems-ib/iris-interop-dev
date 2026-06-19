// E2E tests for iris_query SQL safety gate against a live iris-dev-iris container.
// All tests are #[ignore] — run with:
//   IRIS_HOST=localhost IRIS_WEB_PORT=52780 cargo test --test test_sql_safety_e2e -- --ignored --nocapture

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn iris_dev_bin() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("IRIS_DEV_BIN") {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target/debug/iris-interop-dev");
    if !p.exists() {
        p.pop();
        p.push("release/iris-dev");
    }
    p
}

fn iris_host() -> String {
    std::env::var("IRIS_HOST").unwrap_or_default()
}

fn mcp_call(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let bin = iris_dev_bin();
    if !bin.exists() {
        return vec![];
    }
    let mut cmd = Command::new(&bin);
    cmd.args(["mcp"]);
    cmd.env_remove("IRIS_CONTAINER");
    for key in &[
        "IRIS_HOST",
        "IRIS_WEB_PORT",
        "IRIS_USERNAME",
        "IRIS_PASSWORD",
    ] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn iris-dev mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut results = vec![];

    for msg in messages {
        stdin
            .write_all((serde_json::to_string(msg).unwrap() + "\n").as_bytes())
            .unwrap();
        stdin.flush().unwrap();
        if msg.get("id").is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        results.push(v);
                        break;
                    }
                }
                if std::time::Instant::now() > deadline {
                    break;
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    child.kill().ok();
    child.wait().ok();
    results
}

fn init_msgs() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    ]
}

fn tool_result(responses: &[serde_json::Value], id: u64) -> serde_json::Value {
    let resp = responses
        .iter()
        .find(|r| r["id"] == id)
        .cloned()
        .unwrap_or_default();
    let text = resp["result"]["content"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|c| c["text"].as_str())
        .unwrap_or("{}");
    serde_json::from_str(text).unwrap_or_default()
}

fn query_call(sql: &str, force: Option<bool>) -> serde_json::Value {
    let mut args = serde_json::json!({"query": sql, "namespace": "USER"});
    if let Some(f) = force {
        args["force"] = serde_json::Value::Bool(f);
    }
    let mut msgs = init_msgs();
    msgs.push(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"iris_query","arguments":args}}));
    let responses = mcp_call(&msgs);
    tool_result(&responses, 2)
}

/// T021: Blocked query returns SQL_WRITE_BLOCKED with blocked_keyword, no IRIS error.
#[test]
#[ignore]
fn test_e2e_blocked_query() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let result = query_call("DROP TABLE NonExistent", None);
    eprintln!(
        "T021 result: {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    assert_eq!(result["error_code"], "SQL_WRITE_BLOCKED");
    assert!(
        result["blocked_keyword"].as_str().is_some(),
        "blocked_keyword field missing"
    );
    assert_eq!(result["success"], false);
}

/// T022: Normal SELECT returns rows and success: true.
#[test]
#[ignore]
fn test_e2e_normal_select() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let result = query_call("SELECT 1 AS n", None);
    eprintln!(
        "T022 result: {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    assert_eq!(
        result["success"], true,
        "SELECT should succeed, got: {result}"
    );
    assert!(result.get("rows").is_some(), "rows field missing");
}

/// T029: force: true on dev instance — query reaches IRIS (not blocked).
#[test]
#[ignore]
fn test_e2e_force_bypass_dev() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let result = query_call("DELETE FROM NonExistentTable123", Some(true));
    eprintln!(
        "T029 result: {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    // Should NOT be SQL_WRITE_BLOCKED — it should reach IRIS and get a SQL error or succeed
    assert_ne!(
        result["error_code"], "SQL_WRITE_BLOCKED",
        "force:true should bypass the gate on dev, got: {result}"
    );
}

/// T035: Mixed-case DELETE blocked end-to-end.
#[test]
#[ignore]
fn test_e2e_mixed_case_blocked() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let result = query_call("DeLeTe FROM NonExistentTable", None);
    eprintln!(
        "T035 result: {}",
        serde_json::to_string_pretty(&result).unwrap()
    );
    assert_eq!(result["error_code"], "SQL_WRITE_BLOCKED");
}
