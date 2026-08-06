//! iris_doc — document CRUD via Atelier REST v8.
//! Handles get/put/delete/head with ETag conflict retry and optional SCM hooks.

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DocMode {
    Get,
    Put,
    Delete,
    Head,
}

fn default_mode() -> DocMode {
    DocMode::Get
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IrisDocParams {
    /// Operation: get=fetch source, put=write, delete=remove, head=check existence. Defaults to "get".
    #[serde(default = "default_mode", alias = "action")]
    pub mode: DocMode,
    /// Document name e.g. 'MyApp.Patient.cls'
    #[serde(alias = "document")]
    pub name: Option<String>,
    /// Multiple document names for batch get/delete
    #[serde(default)]
    pub names: Vec<String>,
    /// Source content (required for mode=put)
    pub content: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    /// Elicitation resume ID (from a prior elicitation_required response)
    pub elicitation_id: Option<String>,
    /// User's answer to the elicitation question ("yes" or "no")
    pub elicitation_answer: Option<String>,
    /// If true and mode=put, compile the document after writing (default false).
    /// Saves a round-trip vs calling iris_doc(put) then iris_compile separately.
    #[serde(default)]
    pub compile: bool,
    /// For mode=get: cap returned source to this many bytes (0 = unlimited). Large class source is
    /// the biggest iris_doc token sink — page through with `offset` + `max_bytes`.
    #[serde(default)]
    pub max_bytes: usize,
    /// For mode=get: byte offset to start returning from (use with max_bytes to paginate).
    #[serde(default)]
    pub offset: usize,
}

use crate::iris::connection::IrisConnection;

fn ok_json(v: serde_json::Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    Ok(rmcp::model::CallToolResult::success(vec![
        rmcp::model::Content::text(v.to_string()),
    ]))
}
fn err_json(code: &str, msg: &str) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    crate::tools::envelope::fail(code, msg)
}

/// Map a non-success Atelier HTTP status to an accurate error_code. `IRIS_UNREACHABLE` is reserved for
/// real transport failures (reqwest `send()` errors) and the no-connection guard — an HTTP *response*
/// means IRIS is reachable, so a 4xx/5xx must never be reported as "unreachable".
fn http_error_code(status: reqwest::StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "IRIS_BAD_REQUEST",
        401 | 403 => "IRIS_AUTH",
        404 => "NOT_FOUND",
        409 => "IRIS_CONFLICT",
        423 => "IRIS_LOCKED",
        s if s >= 500 => "IRIS_SERVER_ERROR",
        _ => "IRIS_HTTP_ERROR",
    }
}

/// Build an error result from a non-success Atelier response: accurate `error_code`, the response body
/// (previously discarded, which made these failures undiagnosable), and a retry `hint` for the transient
/// concurrency conflicts Atelier raises under parallel writes/compiles — a document lock (423/409) or an
/// empty-body 400 returned when a compile overlaps another. These are NOT unreachability.
async fn http_err(resp: reqwest::Response) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let status = resp.status();
    let code = http_error_code(status);
    let body: String = resp
        .text()
        .await
        .unwrap_or_default()
        .trim()
        .chars()
        .take(500)
        .collect();
    let msg = if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    };
    let transient =
        matches!(status.as_u16(), 409 | 423) || (status.as_u16() == 400 && body.is_empty());
    let mut extra = serde_json::json!({});
    if transient {
        extra["hint"] = serde_json::Value::String(
            "Transient concurrency conflict (document lock or overlapping compile) — IRIS is up. \
             Retry the same call after a short backoff, and avoid issuing many parallel \
             iris_doc(compile=true) writes at once."
                .to_string(),
        );
    }
    crate::tools::envelope::fail_with(code, &msg, extra)
}

/// Largest char boundary <= idx (stable-Rust stand-in for str::floor_char_boundary), so byte-offset
/// pagination never slices through a multi-byte UTF-8 char.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub async fn handle_iris_doc(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
    elicitation_store: &crate::elicitation::ElicitationStore,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match p.mode {
        DocMode::Get => handle_get(iris, client, p).await,
        DocMode::Put => handle_put(iris, client, p, elicitation_store).await,
        DocMode::Delete => handle_delete(iris, client, p).await,
        DocMode::Head => handle_head(iris, client, p).await,
    }
}

async fn handle_get(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    // Batch get — Bug 19: fetch concurrently instead of sequentially.
    if !p.names.is_empty() {
        // Build a fresh client for batch gets with a shorter timeout so concurrent
        // requests fail fast and the handler returns within the MCP response deadline.
        let batch_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .danger_accept_invalid_certs(
                std::env::var("IRIS_INSECURE")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(false),
            )
            .build()
            .unwrap_or_else(|_| client.clone());
        let mut set = tokio::task::JoinSet::new();
        for name in &p.names {
            let url =
                iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
            let username = iris.username.clone();
            let password = iris.password.clone();
            let name = name.clone();
            let c = batch_client.clone();
            set.spawn(async move {
                let result = c
                    .get(&url)
                    .basic_auth(&username, Some(&password))
                    .send()
                    .await;
                (name, result)
            });
        }
        // Collect results, preserving insertion order via a map then re-order.
        let mut map: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        while let Some(res) = set.join_next().await {
            if let Ok((name, fetch_result)) = res {
                let entry = match fetch_result {
                    Ok(resp) if resp.status().is_success() => {
                        let body: serde_json::Value = resp.json().await.unwrap_or_default();
                        let content = doc_content_to_string(&body);
                        serde_json::json!({"name": name, "content": content})
                    }
                    Ok(resp) => {
                        serde_json::json!({"name": name, "error": format!("HTTP {}", resp.status())})
                    }
                    Err(e) => serde_json::json!({"name": name, "error": e.to_string()}),
                };
                map.insert(name, entry);
            }
        }
        let results: Vec<_> = p.names.iter().filter_map(|n| map.remove(n)).collect();
        return ok_json(serde_json::json!({"success": true, "documents": results}));
    }

    let name = p.name.as_deref().unwrap_or("");
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = client
        .get(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(format!("HTTP error: {e}"), None))?;

    if resp.status().as_u16() == 404 {
        return err_json("NOT_FOUND", &format!("Document not found: {name}"));
    }
    if !resp.status().is_success() {
        return http_err(resp).await;
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let content = doc_content_to_string(&body);
    let ts = body["result"]["content"][0]["ts"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Pagination: cap large source to avoid huge token blowups (iris_doc was the #1 token sink —
    // 274K tokens in the workshop, much of it re-fetching whole library classes). UTF-8 safe.
    let total_bytes = content.len();
    let start = floor_char_boundary(&content, p.offset.min(total_bytes));
    let end = if p.max_bytes == 0 {
        total_bytes
    } else {
        floor_char_boundary(&content, (start + p.max_bytes).min(total_bytes))
    };
    let slice = &content[start..end];
    let mut out = serde_json::json!({
        "success": true, "name": name, "content": slice, "timestamp": ts,
    });
    if start > 0 || end < total_bytes {
        out["truncated"] = serde_json::Value::Bool(true);
        out["total_bytes"] = serde_json::json!(total_bytes);
        out["offset"] = serde_json::json!(start);
        out["returned_bytes"] = serde_json::json!(end - start);
        if end < total_bytes {
            out["next_offset"] = serde_json::json!(end);
            out["hint"] = serde_json::Value::String(format!(
                "Truncated at {end}/{total_bytes} bytes. Fetch the rest with iris_doc(get, name='{name}', offset={end}, max_bytes=…), or use docs_introspect for signatures/structure instead of full source."
            ));
        }
    }
    ok_json(out)
}

async fn handle_put(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
    elicitation_store: &crate::elicitation::ElicitationStore,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let name = p.name.as_deref().unwrap_or("");
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let ns = &namespace;

    // Elicitation resume — user answered a prior SCM dialog
    if let (Some(eid), Some(answer)) = (&p.elicitation_id, &p.elicitation_answer) {
        if let Some(pending) = elicitation_store.lookup(eid) {
            elicitation_store.clear(eid);
            if answer.to_lowercase() != "yes" {
                return crate::tools::envelope::fail("WRITE_ABORTED", "User declined checkout");
            }
            // User said yes — proceed with the stored content directly
            let resume_content = pending.content.as_deref().unwrap_or("");
            return do_write(
                iris,
                client,
                &pending.document,
                resume_content,
                &pending.namespace,
                p.compile,
            )
            .await;
        }
        return err_json(
            "ELICITATION_EXPIRED",
            "Elicitation session expired or not found",
        );
    }

    // Inject ROUTINE header for .mac/.inc if missing
    let raw_content = p.content.as_deref().unwrap_or("");
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let routine_name = name.rsplit_once('.').map(|(n, _)| n).unwrap_or(name);
    let needs_header = !raw_content
        .trim_start()
        .to_uppercase()
        .starts_with("ROUTINE ");
    let content_owned: String;
    let content: &str = match ext.as_str() {
        "mac" if needs_header => {
            content_owned = format!("ROUTINE {}\n{}", routine_name, raw_content);
            &content_owned
        }
        "inc" if needs_header => {
            content_owned = format!("ROUTINE {} [Type=INC]\n{}", routine_name, raw_content);
            &content_owned
        }
        _ => raw_content,
    };

    // SCM OnBeforeSave — check if write is allowed (requires docker exec; skipped if unavailable)
    let scm_check = format!(
        "set scmObj=##class(%Studio.SourceControl.Base).%GetImplementationObject(\"{n}\") if '$IsObject(scmObj) {{ write \"NO_SCM\" }} else {{ set action=0 set msg=\"\" set target=\"\" set reload=0 set sc=scmObj.UserAction(0,\"%SourceMenu,CheckOut\",\"{n}\",\"\",.action,.target,.msg,.reload) write action_\"|\"_msg }}",
        n = name.replace('"', "\\\"")
    );
    if let Ok(out) = iris.execute(&scm_check, ns).await {
        let out = out.trim().to_string();
        if out != "NO_SCM" && !out.is_empty() {
            let parts: Vec<&str> = out.splitn(2, '|').collect();
            let action_code = parts
                .first()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(0);
            let msg = parts.get(1).map(|s| s.trim()).unwrap_or("");

            if action_code == 1 {
                let eid = elicitation_store.insert(
                    name,
                    crate::elicitation::ElicitationAction::Put,
                    Some(content.to_string()),
                    None,
                    ns.clone(),
                );
                return ok_json(serde_json::json!({
                    "success": false,
                    "elicitation_required": true,
                    "elicitation_id": eid,
                    "message": if msg.is_empty() { format!("{} requires checkout. Check out and write?", name) } else { msg.to_string() },
                    "options": ["yes", "no"],
                }));
            } else if action_code == 6 {
                return err_json("SCM_REJECTED", &format!("Source control rejected: {}", msg));
            }
            // action_code == 0: proceed
        }
    }

    do_write(iris, client, name, content, ns, p.compile).await
}

async fn do_write(
    iris: &IrisConnection,
    client: &reqwest::Client,
    name: &str,
    content: &str,
    namespace: &str,
    compile_after: bool,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    // I-3: strip Storage blocks — IRIS 2025.1 UDL parser (#5559) fails on Storage XML.
    // IRIS will auto-generate correct storage on first compile.
    // strip_storage_blocks handles the no-block case cheaply (single pass, no alloc).
    let (content_for_write, storage_stripped) = strip_storage_blocks(content);
    let lines: Vec<&str> = content_for_write.lines().collect();

    // I-4: use ?ignoreConflict=1 — IRIS accepts the write unconditionally, never returns 409.
    let url = iris.versioned_ns_url(
        namespace,
        &format!("/doc/{}?ignoreConflict=1", urlencoding::encode(name)),
    );

    // Retry transient same-document locks (423) / conflicts (409). The `?ignoreConflict=1` flag avoids
    // 409 version conflicts but NOT the 423 lock taken when another write/compile of the same doc is in
    // flight (reproduced under concurrency), so a bounded retry is still needed — also for the
    // cross-process case (multiple MCP processes) the in-process compile gate cannot coordinate.
    let put_body = serde_json::json!({"enc": false, "content": lines});
    let resp = crate::tools::concurrency::send_with_retry(
        || {
            client
                .put(&url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&put_body)
        },
        false,
    )
    .await
    .map_err(|e| rmcp::ErrorData::internal_error(format!("HTTP error: {e}"), None))?;

    if !resp.status().is_success() {
        return http_err(resp).await;
    }
    // Check body for Atelier-level errors (200 OK with status.errors, e.g. build 110
    // SetTextFromString NULL namespace bug via web gateway).
    let put_body: serde_json::Value = resp.json().await.unwrap_or_default();
    if let Some(errs) = put_body["status"]["errors"].as_array() {
        if !errs.is_empty() {
            let msg = errs[0]["error"]
                .as_str()
                .unwrap_or("Document upload failed");
            return err_json("UPLOAD_FAILED", msg);
        }
    }

    // Write open hint for VS Code auto-open
    crate::tools::write_open_hint(namespace, name);

    let open_uri = format!("isfs://{}/{}", namespace, name);

    if compile_after {
        let compile_url = iris.versioned_ns_url(namespace, "/action/compile?flags=cuk");
        let compile_body = serde_json::json!([name]);
        // Atelier 400s ANY overlapping compile, so serialize compiles in-process (the gate) and retry
        // the transient empty-body 400 / locks (covers cross-process collisions the gate can't see).
        // The permit is held until the end of this block.
        let _compile_permit = crate::tools::concurrency::compile_gate().acquire().await;
        let compile_resp = crate::tools::concurrency::send_with_retry(
            || {
                client
                    .post(&compile_url)
                    .basic_auth(&iris.username, Some(&iris.password))
                    .json(&compile_body)
            },
            true,
        )
        .await;

        let (compile_ok, compile_errors, compile_console) = match compile_resp {
            Err(e) => (false, vec![e.to_string()], vec![]),
            // A non-2xx compile response is a FAILURE, not success. Previously the code fell straight to
            // `r.json().unwrap_or_default()`, and Atelier's empty-body 400 (returned when a compile
            // overlaps another under concurrency) parsed to null → no errors → `compiled: true`, a silent
            // false positive. Surface it honestly with the status + body and a retry hint.
            Ok(r) if !r.status().is_success() => {
                let status = r.status();
                let body: String = r
                    .text()
                    .await
                    .unwrap_or_default()
                    .trim()
                    .chars()
                    .take(500)
                    .collect();
                let msg = if body.is_empty() {
                    format!(
                        "compile HTTP {status} (empty body — likely an overlapping concurrent compile; \
                         retry after a short backoff)"
                    )
                } else {
                    format!("compile HTTP {status}: {body}")
                };
                (false, vec![msg], vec![])
            }
            Ok(r) => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                let console: Vec<String> = body["console"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut errs: Vec<String> = vec![];
                if let Some(se) = body["status"]["errors"].as_array() {
                    for e in se {
                        if let Some(msg) = e["error"].as_str() {
                            errs.push(msg.to_string());
                        }
                    }
                }
                for line in &console {
                    if line.trim().starts_with("ERROR ") {
                        let msg = line.trim().to_string();
                        if errs.iter().all(|e| !e.contains(line.trim())) {
                            errs.push(msg);
                        }
                    }
                }
                (errs.is_empty(), errs, console)
            }
        };

        // Issue #2: a compile failure is a genuine tool failure — it gets an
        // error_code, the first compiler error as `error`, and isError on the
        // wire; the full console stays as detail.
        if !compile_ok {
            let first = compile_errors
                .first()
                .cloned()
                .unwrap_or_else(|| "compile failed — see compile_console".to_string());
            return crate::tools::envelope::fail_with(
                "COMPILE_ERROR",
                &first,
                serde_json::json!({
                    "name": name,
                    "open_uri": open_uri,
                    "storage_stripped": storage_stripped,
                    "compiled": false,
                    "compile_errors": compile_errors,
                    "compile_console": compile_console,
                }),
            );
        }
        return ok_json(serde_json::json!({
            "success": true,
            "name": name,
            "open_uri": open_uri,
            "storage_stripped": storage_stripped,
            "compiled": true,
            "compile_errors": compile_errors,
            "compile_console": compile_console,
        }));
    }

    ok_json(
        serde_json::json!({"success": true, "name": name, "open_uri": open_uri, "storage_stripped": storage_stripped}),
    )
}

async fn handle_delete(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    // Batch delete
    if !p.names.is_empty() {
        let mut deleted = vec![];
        let mut errors = vec![];
        for name in &p.names {
            let url =
                iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
            match client
                .delete(&url)
                .basic_auth(&iris.username, Some(&iris.password))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => deleted.push(name.clone()),
                Ok(r) => errors.push(
                    serde_json::json!({"name": name, "error": format!("HTTP {}", r.status())}),
                ),
                Err(e) => errors.push(serde_json::json!({"name": name, "error": e.to_string()})),
            }
        }
        return ok_json(
            serde_json::json!({"success": errors.is_empty(), "deleted": deleted, "errors": errors}),
        );
    }

    let name = p.name.as_deref().unwrap_or("");
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = client
        .delete(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(format!("HTTP error: {e}"), None))?;

    if resp.status().as_u16() == 404 {
        return err_json("NOT_FOUND", &format!("Document not found: {name}"));
    }
    if !resp.status().is_success() {
        return http_err(resp).await;
    }
    ok_json(serde_json::json!({"success": true, "name": name}))
}

async fn handle_head(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let name = p.name.as_deref().unwrap_or("");
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = client
        .head(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(format!("HTTP error: {e}"), None))?;

    let exists = resp.status().is_success();
    let ts = resp
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    ok_json(serde_json::json!({"success": true, "name": name, "exists": exists, "timestamp": ts}))
}

/// Strip `Storage Name { ... }` blocks from ObjectScript class content.
/// Returns (content_without_storage, storage_was_present).
/// IRIS 2025.1 UDL parser fails on explicit Storage XML blocks (#5559);
/// omitting them lets IRIS auto-generate correct storage on first compile.
pub fn strip_storage_blocks(content: &str) -> (String, bool) {
    let mut result = Vec::new();
    let mut in_storage = false;
    let mut brace_depth: i32 = 0;
    let mut found = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if !in_storage {
            // Detect start of Storage block: "Storage Name" or "Storage Name {"
            let is_storage_start = {
                let mut parts = trimmed.split_whitespace();
                parts.next() == Some("Storage") && parts.next().is_some()
            };
            if is_storage_start {
                in_storage = true;
                found = true;
                // Count any opening braces on this line
                brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
                brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
                if brace_depth <= 0 {
                    // Single-line storage (rare) — done immediately
                    in_storage = false;
                    brace_depth = 0;
                }
                continue; // skip this line
            }
            result.push(line);
        } else {
            // Inside storage block — track brace depth
            brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
            brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
            if brace_depth <= 0 {
                in_storage = false;
                brace_depth = 0;
                // Don't add this closing-brace line to result
            }
            // Skip all lines inside storage block
        }
    }

    if found {
        // Remove trailing blank lines that were before the storage block
        while result
            .last()
            .map(|l: &&str| l.trim().is_empty())
            .unwrap_or(false)
        {
            result.pop();
        }
        (result.join("\n") + "\n", true)
    } else {
        (content.to_string(), false)
    }
}

fn doc_content_to_string(body: &serde_json::Value) -> String {
    // Atelier GET /doc/<name> returns result.content as a flat array of line strings.
    body["result"]["content"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_error_code_is_accurate_not_unreachable() {
        use reqwest::StatusCode;
        // Concurrency conflicts and client errors must NOT be reported as "unreachable".
        assert_eq!(http_error_code(StatusCode::BAD_REQUEST), "IRIS_BAD_REQUEST");
        assert_eq!(http_error_code(StatusCode::LOCKED), "IRIS_LOCKED");
        assert_eq!(http_error_code(StatusCode::CONFLICT), "IRIS_CONFLICT");
        assert_eq!(http_error_code(StatusCode::UNAUTHORIZED), "IRIS_AUTH");
        assert_eq!(http_error_code(StatusCode::FORBIDDEN), "IRIS_AUTH");
        assert_eq!(http_error_code(StatusCode::NOT_FOUND), "NOT_FOUND");
        assert_eq!(
            http_error_code(StatusCode::INTERNAL_SERVER_ERROR),
            "IRIS_SERVER_ERROR"
        );
        assert_eq!(http_error_code(StatusCode::IM_A_TEAPOT), "IRIS_HTTP_ERROR");
        // The whole point: no HTTP status maps to IRIS_UNREACHABLE (that's transport-only).
        for code in [400u16, 401, 403, 404, 409, 423, 500, 502, 503, 418] {
            let s = StatusCode::from_u16(code).unwrap();
            assert_ne!(http_error_code(s), "IRIS_UNREACHABLE");
        }
    }

    #[test]
    fn test_doc_content_to_string_flat_array() {
        let body = serde_json::json!({
            "result": {
                "content": ["Class Foo", "{", "}", ""]
            }
        });
        let s = doc_content_to_string(&body);
        assert!(s.contains("Class Foo"));
        assert!(s.contains("{"));
    }

    #[test]
    fn test_doc_content_to_string_empty_array() {
        let body = serde_json::json!({"result": {"content": []}});
        let s = doc_content_to_string(&body);
        assert_eq!(s, "");
    }

    #[test]
    fn test_doc_content_to_string_missing_result() {
        let body = serde_json::json!({});
        let s = doc_content_to_string(&body);
        assert_eq!(s, "");
    }

    #[test]
    fn test_strip_storage_blocks_single_line_storage() {
        // Storage on one line (unusual but possible)
        let cls = "Class Foo {\nStorage Default {}\n}";
        let (stripped, flag) = strip_storage_blocks(cls);
        assert!(flag, "should detect storage");
        assert!(!stripped.contains("Storage Default"), "should strip");
    }

    #[test]
    fn test_strip_storage_blocks_preserves_class_wrapper() {
        // Storage block with opening brace on same line as Storage keyword
        let cls = "Class Foo {\nProperty X As %String;\nStorage Default {\n<Type>T</Type>\n}\n}";
        let (stripped, _) = strip_storage_blocks(cls);
        assert!(stripped.contains("Class Foo"), "class wrapper preserved");
        assert!(stripped.contains("Property X"), "property preserved");
        assert!(
            stripped.trim_end().ends_with('}'),
            "closing brace preserved"
        );
    }

    #[test]
    fn test_strip_storage_blocks_inline_brace_strips_content() {
        // Storage block with { on same line — content including nested braces is stripped
        let cls =
            "Class Foo {\nStorage Default {\n<Data>\n<Value>{ nested }</Value>\n</Data>\n}\n}";
        let (stripped, flag) = strip_storage_blocks(cls);
        assert!(flag);
        assert!(!stripped.contains("Storage Default"));
        assert!(!stripped.contains("nested"));
    }
}
