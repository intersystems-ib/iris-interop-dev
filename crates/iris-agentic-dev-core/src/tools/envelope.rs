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
/// The `error_code` [`transport_fail`] would use, for the call sites that need the code
/// without the envelope — per-item codes inside a batch result, for instance.
pub fn transport_error_code(err: &str) -> &'static str {
    if crate::tools::interop::is_network_error(err) {
        "IRIS_UNREACHABLE"
    } else {
        "IRIS_REQUEST_FAILED"
    }
}

pub fn transport_fail(context: &str, err: &str) -> Result<CallToolResult, McpError> {
    // #58: record it server-side too. Before #57 this condition surfaced as an rmcp
    // "response error" WARN; routing it through the envelope removed that line, so
    // without this the server would go quiet about the failure it just reported.
    tracing::warn!(context, error = err, "IRIS request failed");
    match transport_error_code(err) {
        "IRIS_UNREACHABLE" => fail(
            "IRIS_UNREACHABLE",
            &format!("{context} could not reach IRIS: {err}"),
        ),
        code => fail(code, &format!("{context} failed: {err}")),
    }
}

/// Issue #101: the classifier for a response that DID arrive. `transport_fail` above covers
/// `send()` returning `Err` — no response ever came back, so IRIS really is unreachable.
/// This covers the other half: IRIS answered, with a status that is not 2xx. An HTTP
/// *response* is proof of reachability, so **no status may map to `IRIS_UNREACHABLE`** —
/// `doc.rs` has pinned that rule for itself since #57 (`test_http_error_code_is_accurate_not_unreachable`);
/// this makes it the one rule for the whole surface instead of one file's private discipline.
pub fn http_status_code(status: u16) -> &'static str {
    match status {
        400 => "IRIS_BAD_REQUEST",
        // #101: 401 and 403 are two different problems with two disjoint remedies, so they
        // get two codes. A 401 caller edits IRIS_USERNAME / IRIS_PASSWORD. A 403 caller
        // cannot: IRIS validated the password BEFORE it could evaluate the `%Development`
        // resource on `/api/atelier`, so on a 403 the password is provably correct and the
        // fix is a granted role. `error_code` exists precisely so a consumer does not have
        // to parse prose to pick a branch; one shared code would leave English text in
        // `hint` as the only thing separating them.
        401 => "IRIS_AUTH_FAILED",
        403 => "IRIS_FORBIDDEN",
        404 => "NOT_FOUND",
        409 => "IRIS_CONFLICT",
        423 => "IRIS_LOCKED",
        s if s >= 500 => "IRIS_SERVER_ERROR",
        _ => "IRIS_HTTP_ERROR",
    }
}

/// `Some(code)` when the status is an authentication/authorization refusal, `None` otherwise.
/// Lets a call site adopt the #101 codes without adopting a remap of every other status —
/// which is how `iris_compile` keeps its #93 behaviour for 404 verbatim.
pub fn auth_status_code(status: u16) -> Option<&'static str> {
    match status {
        401 => Some("IRIS_AUTH_FAILED"),
        403 => Some("IRIS_FORBIDDEN"),
        _ => None,
    }
}

/// The string-level sibling of `interop::is_network_error`, for the many call sites that
/// hold only an `anyhow` message by the time they classify.
///
/// Deliberately narrow, per the Bug-18 lesson recorded at `interop.rs:16` (a loose
/// `contains("connection")` swallowed "No Interoperability connection configured"): a bare
/// `contains("401")` would match an IRIS message ID, a line number or a byte count and would
/// ship a NEW wrong answer to fix an old one. Require the token pair.
pub fn auth_error_code(msg: &str) -> Option<&'static str> {
    if msg.contains("HTTP 401") || msg.contains("401 Unauthorized") {
        return Some("IRIS_AUTH_FAILED");
    }
    if msg.contains("HTTP 403") || msg.contains("403 Forbidden") {
        return Some("IRIS_FORBIDDEN");
    }
    None
}

/// Issue #101: the `transport_fail` slot one layer up — a failure envelope for a response
/// that arrived. `attempted_url` rides along because it is genuinely useful (unlike the
/// "Check IRIS_HOST and IRIS_WEB_PORT" hint this replaces, which was wrong for every status).
pub fn http_status_fail(
    context: &str,
    status: reqwest::StatusCode,
    attempted_url: &str,
) -> Result<CallToolResult, McpError> {
    let code = http_status_code(status.as_u16());
    // #58: record it server-side too, exactly as `transport_fail` does — the server must not
    // go quiet about the failure it just reported.
    tracing::warn!(
        context,
        status = status.as_u16(),
        url = attempted_url,
        code,
        "IRIS answered with a non-success status"
    );
    fail_with(
        code,
        &format!("HTTP {status}"),
        serde_json::json!({ "attempted_url": attempted_url }),
    )
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
    // #101: one wrong environment variable used to produce six different error codes and
    // not one of them contained the word "password". These two say which variable, and the
    // 403 one says out loud that the password is NOT the problem — the sentence that stops a
    // caller burning a session on the wrong knob.
    if code == "IRIS_AUTH_FAILED" {
        return Some(
            "IRIS answered on the configured host and port and rejected the credentials \
             (HTTP 401) — the host and port are fine. Check IRIS_USERNAME / IRIS_PASSWORD; \
             call check_config to see which host, port, namespace and user this server is \
             actually using. On a community container started without IRIS_PASSWORD, basic \
             auth is disabled entirely — restart it with -e IRIS_PASSWORD=... . A user \
             lacking the %Development resource on /api/atelier can also surface here on some \
             web-application configurations."
                .into(),
        );
    }
    if code == "IRIS_FORBIDDEN" {
        return Some(
            "IRIS accepted these credentials and refused the operation (HTTP 403) — the \
             password is not the problem. This user lacks a required privilege: the \
             /api/atelier web application requires the %Development resource, and the target \
             namespace requires its %DB_* resource. Grant the role in the Management Portal \
             (System Administration > Security > Users), or target a namespace this user can \
             reach. Do not change IRIS_USERNAME / IRIS_PASSWORD."
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

    // ── Issue #101: an HTTP response is proof of reachability ────────────────
    //
    /// The durable guard for this whole issue class. `doc.rs` has asserted this for its own
    /// private map since #57; the map now lives here, so the rule is enforced for every tool
    /// that classifies a status — including the two files that violated it from outside
    /// doc.rs (`iris_query`, `iris_symbols`).
    #[test]
    fn no_http_status_is_ever_reported_as_unreachable() {
        for status in [400u16, 401, 403, 404, 409, 418, 423, 500, 502, 503] {
            assert_ne!(
                http_status_code(status),
                "IRIS_UNREACHABLE",
                "HTTP {status} arrived, so IRIS is reachable"
            );
            let r = http_status_fail(
                "t",
                reqwest::StatusCode::from_u16(status).unwrap(),
                "http://h:1/api/atelier/v1/APP/action/query",
            )
            .unwrap();
            let v = payload(&r);
            assert_ne!(v["error_code"], "IRIS_UNREACHABLE", "{v}");
            assert_eq!(r.is_error, Some(true), "{v}");
            assert_eq!(
                v["attempted_url"], "http://h:1/api/atelier/v1/APP/action/query",
                "{v}"
            );
            assert!(
                !v.to_string().contains("Check IRIS_HOST"),
                "the host/port hint is for transport failures only: {v}"
            );
        }
    }

    /// Pins the DECISION, not only its consequences: if someone later collapses 401 and 403
    /// into one code with one hint, this fails.
    #[test]
    fn a_401_and_a_403_do_not_share_an_error_code() {
        assert_eq!(http_status_code(401), "IRIS_AUTH_FAILED");
        assert_eq!(http_status_code(403), "IRIS_FORBIDDEN");
        assert_ne!(http_status_code(401), http_status_code(403));
        assert_eq!(auth_status_code(401), Some("IRIS_AUTH_FAILED"));
        assert_eq!(auth_status_code(403), Some("IRIS_FORBIDDEN"));
        assert_eq!(auth_status_code(404), None);
        assert_eq!(auth_status_code(500), None);
    }

    /// A 401 caller edits an env var. A 403 caller must NOT: IRIS validated the password
    /// before it could evaluate `%Development`, so on a 403 the password is provably correct
    /// and sending the caller to re-check it is the same category of wrong answer as sending a
    /// 401 caller to re-check IRIS_HOST.
    #[test]
    fn the_403_hint_never_tells_the_caller_to_check_their_password() {
        let unauthorized = payload(&fail("IRIS_AUTH_FAILED", "HTTP 401 Unauthorized").unwrap());
        let hint = unauthorized["hint"].as_str().unwrap();
        assert!(hint.contains("IRIS_PASSWORD"), "{hint}");
        assert!(!hint.contains("Check IRIS_HOST"), "{hint}");

        let forbidden = payload(&fail("IRIS_FORBIDDEN", "HTTP 403 Forbidden").unwrap());
        let hint = forbidden["hint"].as_str().unwrap();
        assert!(
            hint.contains("password is not the problem"),
            "the sentence that stops a caller burning a session on the wrong variable: {hint}"
        );
        assert!(hint.contains("%Development"), "{hint}");
        assert!(!hint.contains("Check IRIS_HOST"), "{hint}");
    }

    /// The Bug-18 guard, and the thing most likely to bite: a loose `contains("401")` would
    /// match an IRIS message ID, a line number or a byte count and would ship a NEW wrong
    /// answer to fix an old one.
    #[test]
    fn auth_error_code_requires_the_token_pair() {
        assert_eq!(
            auth_error_code("PUT doc failed: HTTP 401 Unauthorized"),
            Some("IRIS_AUTH_FAILED")
        );
        assert_eq!(
            auth_error_code("HTTP 403 from http://h/api/atelier/v1/APP/action/query"),
            Some("IRIS_FORBIDDEN")
        );
        for innocent in [
            "ERROR #401: something unrelated",
            "compile errors at line 401",
            "read 403 bytes",
            "SQLCODE: -401",
            "error sending request for url",
        ] {
            assert_eq!(auth_error_code(innocent), None, "{innocent}");
        }
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
