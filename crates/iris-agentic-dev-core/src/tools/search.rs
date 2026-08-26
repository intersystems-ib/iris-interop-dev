//! iris_search — full-text search via Atelier REST v2 with sync→async fallback.

use crate::iris::connection::IrisConnection;
use crate::tools::log_store;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    pub query: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_sensitive: bool,
    /// Filter to document category: CLS, MAC, INT, INC, or ALL (default)
    pub category: Option<String>,
    /// REQUIRED wildcard document scope, e.g. ["MyPkg.*.cls"] or ["HS.FHIR.**.cls"].
    /// Atelier search is a sequential grep — an empty scope greps the whole namespace
    /// and times out server-side, so at least one scope must be provided.
    #[serde(default)]
    pub documents: Vec<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// If true, bypass the log store and return all results inline regardless of count.
    #[serde(default)]
    pub inline: bool,
}

fn ok_json(v: serde_json::Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    Ok(rmcp::model::CallToolResult::success(vec![
        rmcp::model::Content::text(v.to_string()),
    ]))
}

/// Resolve the Atelier `files` scope for a search.
///
/// Atelier `/action/search` is a sequential grep, not an index: it searches only
/// the documents matched by the `files` wildcard list. A caller who supplies
/// `documents` gets exactly that scope (comma-joined). A caller who supplies none
/// is asking to grep the *entire* namespace — which times out server-side on any
/// real namespace, so we refuse with SCOPE_REQUIRED rather than return a
/// misleading empty result.
fn resolve_files(documents: &[String]) -> Option<String> {
    if documents.is_empty() {
        return None;
    }
    Some(documents.join(","))
}

/// Flatten Atelier search response content into one result per MATCH.
///
/// Response shape varies by API version:
///   • v8 (IRIS 2024+): `result` is itself the array of document entries.
///   • older/async:      `result.content` holds the array.
/// Entries either nest matches (`{doc, matches:[{text, line, member}]}`) or put a
/// single match flat on the entry (`{doc, atLine, text, member}`). Flatten both so
/// `total_found` counts matches, not documents.
fn flatten_results(body: &serde_json::Value) -> Vec<serde_json::Value> {
    let content = body["result"]
        .as_array()
        .or_else(|| body["result"]["content"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut results: Vec<serde_json::Value> = Vec::new();
    for item in content {
        let doc = item["doc"].clone();
        match item["matches"].as_array() {
            Some(matches) if !matches.is_empty() => {
                for m in matches {
                    results.push(serde_json::json!({
                        "document": doc,
                        // v2 nested matches use `line`; tolerate `atLine` too.
                        "line": if m["line"].is_null() { m["atLine"].clone() } else { m["line"].clone() },
                        "member": m["member"],
                        "content": m["text"],
                    }));
                }
            }
            _ => {
                results.push(serde_json::json!({
                    "document": doc,
                    "line": if item["line"].is_null() { item["atLine"].clone() } else { item["line"].clone() },
                    "member": item["member"],
                    "content": item["text"],
                }));
            }
        }
    }
    results
}

pub async fn handle_iris_search(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: SearchParams,
    log_store: Arc<Mutex<log_store::LogStore>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let category = p.category.as_deref().unwrap_or("ALL");
    // A scope is mandatory. Without one Atelier would grep the whole namespace and
    // time out server-side, returning nothing — an empty result that reads as
    // "term not found" when the term is simply out of a (nonexistent) scope.
    let files = match resolve_files(&p.documents) {
        Some(f) => f,
        None => {
            return crate::tools::envelope::fail_with(
                "SCOPE_REQUIRED",
                "iris_search requires a document scope. Namespace-wide search greps \
                 every document sequentially and times out server-side. Pass \
                 `documents` with a wildcard scope, e.g. [\"MyPkg.*.cls\"].",
                serde_json::json!({"query": p.query}),
            );
        }
    };
    // Atelier `/action/search` treats a *missing* `case` param as case-SENSITIVE,
    // so omitting it (the old behaviour when `case_sensitive=false`) silently made
    // every default search exact-case. Always send `case` explicitly:
    // `case=0` = insensitive (the tool default), `case=1` = sensitive.
    let case_flag = if p.case_sensitive { 1 } else { 0 };
    let query_string = format!(
        "query={}&regex={}&sys=false&category={}&files={}&case={}",
        urlencoding::encode(&p.query),
        p.regex,
        category,
        urlencoding::encode(&files),
        case_flag,
    );

    let sync_url = iris.versioned_ns_url(&namespace, &format!("/action/search?{}", query_string));

    // Try the synchronous search first. Many IRIS servers answer `/action/search`
    // synchronously — even for wildcard scopes that take several seconds — and
    // never hand back a `workId` to poll. A tight timeout trips on those, falls
    // through to the async POST, which on those servers returns an empty `{}`
    // (no workId) and parses as zero hits. Give the sync path a generous,
    // env-overridable budget; async polling remains the fallback for servers
    // that genuinely defer via workId.
    let sync_timeout_secs = std::env::var("IRIS_SEARCH_SYNC_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(30);
    let sync_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(sync_timeout_secs))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| client.clone());

    let sync_result = sync_client
        .get(&sync_url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await;

    // #106: what the sync leg answered, when it answered with a refusal rather than a
    // timeout. Both used to land in the same `_` arm and vanish. The fallback POST below
    // is kept for BOTH cases (a server that 404s the GET form is speculative, and
    // preserving the fallback costs one request), but a refusal is now carried forward so
    // the failure can be reported instead of becoming zero hits.
    let mut sync_refusal: Option<reqwest::StatusCode> = None;
    match sync_result {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // If we got a workId, it's async — fall through to polling
            if body["result"]["workId"].is_null() {
                return parse_search_results(body, &p.query, p.inline, &log_store);
            }
            let work_id = body["result"]["workId"].as_str().unwrap_or("").to_string();
            poll_async_search(
                iris, client, &work_id, &namespace, &p.query, p.inline, &log_store,
            )
            .await
        }
        _ => {
            if let Ok(resp) = &sync_result {
                sync_refusal = Some(resp.status());
            }
            // Timeout or error — fall back to async POST
            let post_url = iris.versioned_ns_url(&namespace, "/action/search");
            let post_body = serde_json::json!({
                "query": p.query,
                "regex": p.regex,
                "sys": false,
                "category": category,
                "files": files,
                // Always explicit — a missing `case` defaults to case-sensitive server-side.
                "case": case_flag,
            });
            let resp = client
                .post(&post_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&post_body)
                .send()
                .await
                .map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("Search request failed: {e}"), None)
                })?;

            // #106, the filed repro: this never looked at the status and parsed with
            // `unwrap_or_default()`, so `IRIS_PASSWORD=WRONGPW` answered
            // `{"success":true,"total_found":0}` — a caller reads "that string is not in
            // that document" from a search IRIS refused to run. A search that did not run
            // has no result, empty or otherwise.
            if !resp.status().is_success() {
                return search_request_failed(
                    iris,
                    client,
                    &namespace,
                    resp.status(),
                    &post_url,
                    sync_refusal,
                )
                .await;
            }
            let text = resp.text().await.map_err(|e| {
                rmcp::ErrorData::internal_error(format!("Search request failed: {e}"), None)
            })?;
            let body: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => {
                    return crate::tools::envelope::fail_with(
                        "IRIS_REQUEST_FAILED",
                        &format!(
                            "non-JSON response from {post_url}: {}",
                            text.trim().chars().take(200).collect::<String>()
                        ),
                        serde_json::json!({
                            "attempted_url": post_url,
                            "query": p.query,
                            "hint": "IRIS answered, but not with JSON — typically a proxy \
                                     error page or an HTML login redirect in front of the \
                                     Atelier API. The search did not run; this is not a \
                                     zero-hit result.",
                        }),
                    );
                }
            };
            if let Some(work_id) = body["result"]["workId"].as_str() {
                poll_async_search(
                    iris, client, work_id, &namespace, &p.query, p.inline, &log_store,
                )
                .await
            } else {
                parse_search_results(body, &p.query, p.inline, &log_store)
            }
        }
    }
}

/// Render a refused `/action/search` request. A 404 goes through the shared classifier so
/// a bad IRIS_WEB_PREFIX and a nonexistent namespace stay distinguishable; anything else
/// keeps its status and lets `builtin_hint` supply the advice that fits it.
async fn search_request_failed(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    status: reqwest::StatusCode,
    url: &str,
    sync_status: Option<reqwest::StatusCode>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    if status.as_u16() == 404 {
        if let crate::tools::interop::FourOhFour::Explained(e) =
            crate::tools::interop::classify_404(
                iris,
                client,
                namespace,
                url,
                "The search did not run — no documents were searched.",
            )
            .await
        {
            return e;
        }
    }
    if let Some(sync) = sync_status {
        tracing::debug!(
            sync_status = sync.as_u16(),
            post_status = status.as_u16(),
            "iris_search: both the sync and async legs were refused"
        );
    }
    crate::tools::envelope::http_status_fail("iris_search", status, url)
}

async fn poll_async_search(
    iris: &IrisConnection,
    client: &reqwest::Client,
    work_id: &str,
    namespace: &str,
    query: &str,
    inline: bool,
    log_store: &Arc<Mutex<log_store::LogStore>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let poll_url = iris.versioned_ns_url(
        namespace,
        &format!("/action/search?workId={}", urlencoding::encode(work_id)),
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if std::time::Instant::now() > deadline {
            return crate::tools::envelope::fail_with(
                "SEARCH_TIMEOUT",
                "Async search did not complete within 5 minutes",
                serde_json::json!({"query": query}),
            );
        }

        let resp = client
            .get(&poll_url)
            .basic_auth(&iris.username, Some(&iris.password))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                if body["result"]["workId"].is_null() {
                    return parse_search_results(body, query, inline, log_store);
                }
                // Still pending — keep polling
            }
            // #106: a refusal is not "still pending". Polling through a 401 for the full
            // five minutes and then reporting SEARCH_TIMEOUT describes the wrong problem
            // and costs five minutes to say it. A transport blip stays retryable.
            Ok(r) => {
                return search_request_failed(iris, client, namespace, r.status(), &poll_url, None)
                    .await;
            }
            Err(_) => continue,
        }
    }
}

fn parse_search_results(
    body: serde_json::Value,
    query: &str,
    inline: bool,
    log_store: &Arc<Mutex<log_store::LogStore>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let results = flatten_results(&body);
    let total = results.len();

    let mut resp = serde_json::json!({
        "success": true,
        "query": query,
        "results": results,
        "total_found": total,
    });

    // Progressive disclosure (027): truncate results when count exceeds threshold.
    let threshold = log_store::read_inline_threshold("IRIS_INLINE_SEARCH", 30);
    log_store::apply_truncation(
        &mut resp,
        "results",
        threshold,
        inline,
        log_store,
        "iris_search",
    );

    ok_json(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SearchParams serde ────────────────────────────────────────────────────
    #[test]
    fn test_search_params_minimal() {
        let p: SearchParams =
            serde_json::from_str(r#"{"query":"test","namespace":"USER"}"#).unwrap();
        assert_eq!(p.query, "test");
    }

    #[test]
    fn test_search_params_namespace_optional_field() {
        // Explicit namespace is kept; omitted resolves to the connection
        // namespace at call time (issue #15).
        let result: Result<SearchParams, _> =
            serde_json::from_str(r#"{"query":"x","namespace":"MYNS"}"#);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().namespace.as_deref(), Some("MYNS"));
        let omitted: SearchParams = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(omitted.namespace, None);
    }

    // ── resolve_files (issue #17: scope must reach the query) ─────────────────
    #[test]
    fn resolve_files_joins_scopes() {
        assert_eq!(
            resolve_files(&["A.*.cls".into(), "B.*.mac".into()]).as_deref(),
            Some("A.*.cls,B.*.mac")
        );
        assert_eq!(
            resolve_files(&["Pkg.**.cls".into()]).as_deref(),
            Some("Pkg.**.cls")
        );
    }

    #[test]
    fn resolve_files_empty_means_no_scope() {
        assert_eq!(resolve_files(&[]), None);
    }

    // ── flatten_results (issue #17: v8 array + matches[] shapes) ──────────────
    #[test]
    fn flatten_handles_legacy_content_shape() {
        let body = serde_json::json!({"result": {"content": [
            {"doc": "A.cls", "atLine": 3, "member": "M", "text": "hit"}
        ]}});
        let r = flatten_results(&body);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["document"], "A.cls");
        assert_eq!(r[0]["line"], 3);
        assert_eq!(r[0]["content"], "hit");
    }

    #[test]
    fn flatten_handles_v8_result_array_with_nested_matches() {
        let body = serde_json::json!({"result": [
            {"doc": "A.cls", "matches": [
                {"text": "one", "line": 1, "member": "X"},
                {"text": "two", "line": 9, "member": "Y"}
            ]},
            {"doc": "B.cls", "matches": [{"text": "three", "atLine": 5}]}
        ]});
        let r = flatten_results(&body);
        // one result per MATCH (3), not per document (2)
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["document"], "A.cls");
        assert_eq!(r[1]["line"], 9);
        assert_eq!(r[2]["document"], "B.cls");
        assert_eq!(
            r[2]["line"], 5,
            "atLine must be tolerated in nested matches"
        );
    }

    #[test]
    fn flatten_empty_body_is_empty() {
        assert!(flatten_results(&serde_json::json!({})).is_empty());
        assert!(flatten_results(&serde_json::json!({"result": {}})).is_empty());
    }
}
