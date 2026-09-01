// E2E tests for iris_execute &sql macro translation against live iris-dev-iris.
// All tests are #[ignore] — run with:
//   IRIS_HOST=localhost IRIS_WEB_PORT=52780 cargo test --test test_sql_translate_e2e -- --ignored --nocapture

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

fn execute(code: &str, translate_sql: Option<bool>) -> serde_json::Value {
    let mut args = serde_json::json!({"code": code, "namespace": "USER"});
    if let Some(t) = translate_sql {
        args["translate_sql"] = serde_json::Value::Bool(t);
    }
    let mut msgs = init_msgs();
    msgs.push(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"iris_execute","arguments":args}
    }));
    let responses = mcp_call(&msgs);
    tool_result(&responses, 2)
}

/// T025: SELECT INTO translates and executes correctly — output is the expected value.
#[test]
#[ignore]
fn test_e2e_select_into_translates_and_runs() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let code = "set id=\"%ASQ.AST\"\nset name=\"\"\n&sql(SELECT Name INTO :name FROM %Dictionary.ClassDefinition WHERE ID = :id)\nwrite name,!";
    let result = execute(code, None);
    eprintln!("T025: {}", serde_json::to_string_pretty(&result).unwrap());
    assert_eq!(result["success"], true);
    assert_eq!(result["sql_translated"], true);
    let output = result["output"].as_str().unwrap_or("");
    assert!(
        output.contains("%ASQ.AST"),
        "output should contain %ASQ.AST, got: {output}"
    );
    assert!(
        result["translated_code"].as_str().is_some(),
        "translated_code should be present"
    );
}

/// #177: a SELECT INTO that FINDS a row and then reads SQLCODE.
///
/// This is the shape that was broken in every shipped release up to 0.15.1 and that no
/// test held: T025 above executes a SELECT INTO but never reads `SQLCODE`, and the unit
/// suite asserted the rename that caused the failure. The failure was inverted — the
/// generated variable was assigned in the `else` branch only, so the statement threw
/// `<UNDEFINED>` precisely when the query SUCCEEDED.
///
/// It lives at the wire deliberately. The unit test and the translator share a frame; only
/// running the generated ObjectScript through IRIS proves the names actually resolve.
#[test]
#[ignore]
fn test_e2e_sqlcode_is_readable_after_a_successful_select_into() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let code = "&sql(SELECT Name INTO :name FROM %Dictionary.ClassDefinition WHERE Name = '%ASQ.AST')\nwrite \"sqlcode=\", SQLCODE, \" rows=\", %ROWCOUNT, \" name=\", name, !";
    let result = execute(code, None);
    eprintln!("#177: {}", serde_json::to_string_pretty(&result).unwrap());
    let output = result["output"].as_str().unwrap_or("");
    assert!(
        !output.contains("sqlSQLCODE"),
        "the caller's SQLCODE read was renamed to a variable nothing sets: {output}"
    );
    assert_eq!(result["success"], true, "output was: {output}");
    assert!(
        output.contains("sqlcode=0"),
        "SQLCODE must be readable and 0 after a successful SELECT INTO, got: {output}"
    );
    assert!(
        output.contains("name=%ASQ.AST"),
        "the host variable must still be bound, got: {output}"
    );
}

/// #179: a SELECT with no INTO binds nothing, so the caller's column read is
/// `<UNDEFINED>`. The translation must say so rather than emit code that cannot work.
#[test]
#[ignore]
fn test_e2e_select_without_into_warns_that_nothing_is_bound() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let result = execute(
        "&sql(SELECT Name FROM %Dictionary.ClassDefinition WHERE Name = '%ASQ.AST')",
        None,
    );
    eprintln!("#179: {}", serde_json::to_string_pretty(&result).unwrap());
    assert_eq!(result["sql_translated"], true);
    let warnings = result["translation_warning"].to_string();
    assert!(
        warnings.contains("no INTO clause"),
        "expected an unbound-host-variable warning, got: {warnings}"
    );

    // Positive control for the warning itself: the INTO form must NOT carry it, or the
    // assertion above would pass on a translator that warns unconditionally.
    let with_into = execute(
        "&sql(SELECT Name INTO :n FROM %Dictionary.ClassDefinition WHERE Name = '%ASQ.AST')",
        None,
    );
    assert!(
        !with_into["translation_warning"]
            .to_string()
            .contains("no INTO clause"),
        "a SELECT INTO must not warn: {}",
        with_into["translation_warning"]
    );
}

/// T026: INSERT translation fires — sql_translated: true (don't actually execute insert against real table).
/// Uses a read-only table to verify translation, then checks sql_translated even if INSERT fails.
#[test]
#[ignore]
fn test_e2e_insert_translation_fires() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    // We just verify translation fires — the INSERT may fail due to permissions, but sql_translated should be true
    let code = "set msg=\"test\"\n&sql(INSERT INTO %sqltemp.Test (Message) VALUES (:msg))";
    let result = execute(code, None);
    eprintln!("T026: {}", serde_json::to_string_pretty(&result).unwrap());
    // Translation should fire regardless of whether INSERT succeeds
    assert_eq!(
        result["sql_translated"], true,
        "sql_translated should be true even if INSERT fails due to permissions"
    );
}

/// T033: translate_sql: false — no translation, raw IRIS error.
#[test]
#[ignore]
fn test_e2e_translate_false_no_translation() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    let code = "set x=\"\"\n&sql(SELECT 1 INTO :x)\nwrite x,!";
    let result = execute(code, Some(false));
    eprintln!("T033: {}", serde_json::to_string_pretty(&result).unwrap());
    // Should NOT have sql_translated field
    assert!(
        result.get("sql_translated").is_none(),
        "sql_translated should be absent when translate_sql=false"
    );
    // The code will fail with a compilation/execution error from IRIS
    // (either success=false or the output will be empty/wrong — either is acceptable)
}

/// T039: CALL with translate_sql: true — translation_warning present, tool doesn't crash.
#[test]
#[ignore]
fn test_e2e_call_produces_warning() {
    if iris_host().is_empty() {
        eprintln!("Skipping: IRIS_HOST not set");
        return;
    }
    // &sql(CALL ...) should produce a warning and not crash
    let code = "&sql(CALL %SYSTEM.Status.OK())\nwrite \"done\",!";
    let result = execute(code, None);
    eprintln!("T039: {}", serde_json::to_string_pretty(&result).unwrap());
    // Tool should not crash — either succeeds or fails gracefully
    assert!(
        result.get("success").is_some(),
        "should return a valid response (no crash)"
    );
    // If sql_translated is true, translation_warning should be present
    if result["sql_translated"].as_bool().unwrap_or(false) {
        assert!(
            result.get("translation_warning").is_some(),
            "translation_warning should be present for CALL"
        );
    }
}
