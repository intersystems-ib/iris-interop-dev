//! Issue #2: ONE failure surface for every tool.
//!
//! A genuine tool failure must (1) set `isError: true` on the MCP
//! `CallToolResult`, so PostToolUse hooks, telemetry, and retry logic can
//! branch on "did this call fail" without per-tool field knowledge, and
//! (2) carry one envelope: `{success:false, error_code, error [, hint]}`.
//! Tool-specific detail (`output`, `compile_console`, …) rides along as extra
//! fields — never as the only place the message appears.
//!
//! The one deliberate exception: a red `iris_test` run is a valid outcome,
//! not a tool failure. It keeps `success:false` WITHOUT `isError`, which is
//! exactly why `success` alone is not the failure discriminator.

use rmcp::{model::*, ErrorData as McpError};

/// Success result — `isError: false` on the wire.
pub fn ok_json(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}

/// Failure result: consistent envelope + `isError: true`.
pub fn fail(code: &str, msg: &str) -> Result<CallToolResult, McpError> {
    fail_with(code, msg, serde_json::Value::Null)
}

/// Failure with tool-specific extras merged into the envelope. An explicit
/// `hint` in `extra` wins over the built-in recovery hints; `success`,
/// `error_code`, `error` from `extra` are ignored — the envelope owns them.
pub fn fail_with(
    code: &str,
    msg: &str,
    extra: serde_json::Value,
) -> Result<CallToolResult, McpError> {
    let mut obj = serde_json::Map::new();
    obj.insert("success".into(), false.into());
    obj.insert("error_code".into(), code.into());
    obj.insert("error".into(), msg.into());
    if let Some(h) = builtin_hint(code, msg) {
        obj.insert("hint".into(), h.into());
    }
    if let serde_json::Value::Object(extra) = extra {
        for (k, v) in extra {
            if k == "success" || k == "error_code" || k == "error" {
                continue;
            }
            obj.insert(k, v);
        }
    }
    Ok(CallToolResult::error(vec![Content::text(
        serde_json::Value::Object(obj).to_string(),
    )]))
}

/// Issue #57: a failure to reach or read IRIS is a TOOL failure, not a protocol
/// error. These paths used to `?` an `internal_error` out of the handler, which
/// rmcp turns into a JSON-RPC `-32603` frame carrying no `error_code` and no
/// `hint` — invisible to anything that classifies by the envelope, and the single
/// most common workshop failure (wrong port, container not running).
///
/// `context` names the operation for the message; `err` is the underlying error.
pub fn transport_fail(context: &str, err: &str) -> Result<CallToolResult, McpError> {
    // #58: record it server-side too. Before #57 this condition surfaced as an rmcp
    // "response error" WARN; routing it through the envelope removed that line, so
    // without this the server would go quiet about the failure it just reported.
    tracing::warn!(context, error = err, "IRIS request failed");
    if crate::tools::interop::is_network_error(err) {
        fail(
            "IRIS_UNREACHABLE",
            &format!("{context} could not reach IRIS: {err}"),
        )
    } else {
        fail("IRIS_REQUEST_FAILED", &format!("{context} failed: {err}"))
    }
}

/// Mechanical recoveries for failures the workshop data showed carry no hint
/// (22/38 in issue #2). A hint earns its place only when the fix is known and
/// mechanical — generic advice is noise.
fn builtin_hint(code: &str, msg: &str) -> Option<String> {
    if msg.contains("ErrProductionNotShutdownCleanly") {
        return Some(
            "The production named in the error was not stopped cleanly — it may differ from \
             the one you asked for; it is the one still registered in this namespace. Run \
             iris_production action=recover, then retry."
                .into(),
        );
    }
    if code == "IRIS_UNREACHABLE" {
        return Some(
            "IRIS did not answer on the configured host/port. Check the instance is running \
             (for Docker: `docker ps`), and check IRIS_HOST / IRIS_WEB_PORT — call check_config \
             to see which host, port and namespace this server is actually using."
                .into(),
        );
    }
    if code == "COMPILE_ERROR" {
        return Some(
            "Fix the first reported error and recompile — later errors are often cascades of \
             the first. Full compiler output is in compile_console."
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(r: &CallToolResult) -> serde_json::Value {
        let text = match &r.content[0].raw {
            RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn ok_is_not_error() {
        let r = ok_json(serde_json::json!({"success": true})).unwrap();
        assert_ne!(r.is_error, Some(true));
    }

    #[test]
    fn fail_sets_is_error_and_envelope() {
        let r = fail("SQL_ERROR", "IDENTIFIER expected").unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = payload(&r);
        assert_eq!(v["success"], false);
        assert_eq!(v["error_code"], "SQL_ERROR");
        assert_eq!(v["error"], "IDENTIFIER expected");
    }

    #[test]
    fn fail_with_merges_extras_and_keeps_envelope_fields() {
        let r = fail_with(
            "IRIS_RUNTIME_ERROR",
            "ERROR: <INVALID OREF>",
            serde_json::json!({"output": "ERROR: <INVALID OREF>", "success": true, "error": "spoofed"}),
        )
        .unwrap();
        let v = payload(&r);
        assert_eq!(v["success"], false, "extras must not override the envelope");
        assert_eq!(v["error"], "ERROR: <INVALID OREF>");
        assert_eq!(v["output"], "ERROR: <INVALID OREF>");
    }

    #[test]
    fn explicit_hint_wins_over_builtin() {
        let r = fail_with(
            "COMPILE_ERROR",
            "boom",
            serde_json::json!({"hint": "custom recovery"}),
        )
        .unwrap();
        assert_eq!(payload(&r)["hint"], "custom recovery");
    }

    #[test]
    fn production_not_shutdown_cleanly_gets_hint() {
        let r = fail(
            "INTEROP_ERROR",
            "ERROR <Ens>ErrProductionNotShutdownCleanly: Production 'X' was not shutdown cleanly",
        )
        .unwrap();
        let v = payload(&r);
        assert!(v["hint"].as_str().unwrap().contains("action=recover"));
    }
}
