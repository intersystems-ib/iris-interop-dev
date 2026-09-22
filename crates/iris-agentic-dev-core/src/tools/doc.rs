//! iris_doc — document CRUD via Atelier REST v8.
//! Handles get/put/delete/head with ETag conflict retry and optional SCM hooks.

use schemars::JsonSchema;
use serde::Deserialize;

/// Doc operation, parsed from a plain string param. Kept as a plain `String` in
/// `IrisDocParams` deliberately: a Rust enum makes schemars emit
/// `mode: {"$ref": "#/$defs/DocMode"}` (rmcp doesn't inline subschemas), which
/// some MCP clients reject outright — this was the only enum-typed tool param
/// left in the fork (issue #18, upstream 9bd133d).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DocMode {
    Get,
    Put,
    Delete,
    Head,
    /// #24: insert lines BEFORE `at`. A write.
    InsertLines,
    /// #24: remove `count` lines starting at `at`. A write, and the destructive one.
    DeleteLines,
}

impl DocMode {
    /// Every mode, and the ONLY definition of the set.
    ///
    /// `parse`, the advertised list in the unknown-mode error, and the write gate in
    /// `mutating_call` all derive from this. Before, the set was written out three times — here, in
    /// that error message, and as `matches!(action, "put" | "delete")` in the gate — and the third
    /// copy is the dangerous one: a mode added to the enum but not to the gate's `matches!` would be
    /// dispatched as an UNGATED WRITE. That is the report-vs-enforce split this repo keeps hitting
    /// (#110, #169, #263), and it is a live hazard the moment a positional-edit mode is added.
    pub const ALL: &'static [DocMode] = &[
        Self::Get,
        Self::Put,
        Self::Delete,
        Self::Head,
        Self::InsertLines,
        Self::DeleteLines,
    ];

    /// The wire spelling. EXHAUSTIVE match, no wildcard — a new variant does not compile until it is
    /// named here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::Head => "head",
            Self::InsertLines => "insert_lines",
            Self::DeleteLines => "delete_lines",
        }
    }

    /// Whether this mode WRITES to IRIS.
    ///
    /// EXHAUSTIVE match, no wildcard, and that is the point: a new variant is a compile ERROR until
    /// someone decides whether it writes. A `_ => false` here would turn that decision into a silent
    /// default of "safe", which is the wrong direction to be wrong in.
    pub fn is_write(self) -> bool {
        match self {
            Self::Get | Self::Head => false,
            // A positional edit reads the document, rewrites it and PUTs it back. Every bit as much a
            // write as `put`, and `delete_lines` destroys content.
            Self::Put | Self::Delete | Self::InsertLines | Self::DeleteLines => true,
        }
    }

    /// Derived from [`Self::ALL`], so the advertised list cannot omit a mode that is dispatchable.
    pub fn valid_values() -> String {
        Self::ALL
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        Self::ALL.iter().copied().find(|m| m.as_str() == lower)
    }
}

fn default_mode() -> String {
    "get".to_string()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IrisDocParams {
    /// Operation: get=fetch source, put=write, delete=remove, head=check existence. Defaults to "get".
    #[serde(default = "default_mode", alias = "action")]
    pub mode: String,
    /// Document name e.g. 'MyApp.Patient.cls'
    #[serde(alias = "document")]
    pub name: Option<String>,
    /// Multiple document names for batch get/delete
    #[serde(default)]
    pub names: Vec<String>,
    /// Source content (required for mode=put)
    pub content: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
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
    // ── NOT a doc comment, deliberately. ──────────────────────────────────────────────────────
    // A `///` here becomes the advertised inputSchema description, shipped on every tools/list, and
    // `mcp_server_tools_list_returns_interop_profile` rejects Rust-internal commentary in a schema
    // for exactly that reason: it is context spent on every client, every session. The rationale
    // below is for whoever reads this file; the one-line description the caller sees is on the field.
    //
    // RETIRED (#331). `put` used to STRIP Storage blocks and then refuse the write rather than
    // discard a layout silently, and this flag was the opt-in. Measured on writable throwaway
    // instances, through plain Atelier REST: IRIS ACCEPTS a class carrying a generated Storage block
    // and preserves it across compile — PUT 201 / 200 with zero errors on 2025.3 and 2026.1, slot
    // list and <DataLocation> byte-intact on read-back. So nothing is stripped, nothing needs
    // regenerating, and there is nothing to opt into.
    //
    // KEPT IN THE SCHEMA so a caller that still passes it is not rejected while deserialising with a
    // bare -32602 carrying no error_code and no hint (#211: name what happened instead of failing to
    // parse). It is read and ignored.
    /// Retired and ignored. Storage blocks are no longer stripped on write, so there is nothing to
    /// opt into; the class is written exactly as you sent it. Accepted for older callers.
    #[serde(default)]
    pub allow_storage_regeneration: bool,
    /// mode=insert_lines / delete_lines: the 1-BASED line to act at. For insert, the new lines go
    /// BEFORE this line; `at` = one past the last line appends.
    #[serde(default)]
    pub at: Option<usize>,
    /// mode=insert_lines: the lines to insert, without newlines.
    #[serde(default)]
    pub lines: Option<Vec<String>>,
    /// mode=delete_lines: how many lines to remove starting at `at`. Defaults to 1.
    #[serde(default)]
    pub count: Option<usize>,
    /// mode=delete_lines (REQUIRED) / insert_lines (optional): the text you believe is currently at
    /// `at`. The edit is REFUSED if it does not match, because your line numbers came from an earlier
    /// read and the document may have changed since — in which case the edit would silently land
    /// somewhere else. Required for delete_lines because a delete on the wrong line destroys content.
    #[serde(default)]
    pub expect: Option<String>,
}

/// A blank/missing `name` on get/put/delete/head produces Atelier requests against
/// `/doc/` — `ERROR #16006: Document '' name is invalid` — which reads as a server
/// error and pushes the calling model into a retry loop. Fail fast and clearly.
fn require_name(
    p: &IrisDocParams,
    mode: &str,
) -> Result<String, Result<rmcp::model::CallToolResult, rmcp::ErrorData>> {
    match p.name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => Ok(n.to_string()),
        _ => Err(crate::tools::envelope::fail_with(
            "MISSING_PARAMS",
            &format!(
                "iris_doc mode={mode} requires `name` (e.g. \"MyApp.Patient.cls\" — \
                 include the .cls/.mac/.int/.inc extension)."
            ),
            serde_json::json!({"mode": mode}),
        )),
    }
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
/// #101: iris_doc was the ONLY site in the tree that got this approximately right, and even
/// here `401 | 403 => "IRIS_AUTH"` collapsed two disjoint remedies into one code and attached
/// no hint at all. The map now lives in `envelope::http_status_code` so the whole surface
/// shares one scheme instead of each file re-deciding — and 401/403 split into
/// IRIS_AUTH_FAILED / IRIS_FORBIDDEN, both of which carry a `builtin_hint`.
pub(crate) fn http_error_code(status: reqwest::StatusCode) -> &'static str {
    crate::tools::envelope::http_status_code(status.as_u16())
}

/// Build an error result from a non-success Atelier response: accurate `error_code`, the response body
/// (previously discarded, which made these failures undiagnosable), and a retry `hint` for the transient
/// concurrency conflicts Atelier raises under parallel writes/compiles — a document lock (423/409) or an
/// empty-body 400 returned when a compile overlaps another. These are NOT unreachability.
/// Issue #71: Atelier addresses documents by name AND type suffix. A bare class name is
/// rejected with `ERROR #16006: Document 'X' name is invalid`, which blames the name — and
/// the name is fine; the suffix is missing. Every other name-taking tool in this server
/// (`iris_test`, `docs_introspect`, `iris_symbols`, `iris_production_item`) accepts a bare
/// class name, so `iris_doc` is the one that surprises, and the error names no remedy.
pub const ATELIER_DOC_SUFFIXES: [&str; 10] = [
    "cls", "mac", "int", "inc", "bas", "mvb", "mvi", "dfi", "csp", "csr",
];

pub fn has_doc_suffix(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| ATELIER_DOC_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// The corrected name, when the CONTENT says what the document is. `mode=put` with source
/// starting `Class <name>` is a `.cls` by construction — nothing is being guessed. Returns
/// `None` when the name already carries a suffix or the content does not settle it; a
/// wrong suffix would be worse than the error.
pub fn doc_name_with_suffix(name: &str, content: Option<&str>) -> Option<String> {
    if has_doc_suffix(name) {
        return None;
    }
    let body = content.unwrap_or("").trim_start();
    let head: String = body
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_uppercase();
    if head.starts_with("CLASS ") {
        return Some(format!("{name}.cls"));
    }
    if head.starts_with("ROUTINE ") {
        let first_line = body.lines().next().unwrap_or("").to_ascii_uppercase();
        // `ROUTINE X [Type=INC]` is an include file; anything else is a .mac.
        return Some(if first_line.replace(' ', "").contains("TYPE=INC") {
            format!("{name}.inc")
        } else {
            format!("{name}.mac")
        });
    }
    None
}

/// What to tell a caller whose document name has no suffix. `(suggestion, hint)`.
pub fn doc_suffix_hint(name: &str) -> Option<(String, String)> {
    if has_doc_suffix(name) {
        return None;
    }
    let suggestion = format!("{name}.cls");
    Some((
        suggestion.clone(),
        format!(
            "Atelier document names need a type suffix and '{name}' has none — pass \
             '{suggestion}' for a class, or .mac/.inc/.int for a routine. iris_doc(put) adds \
             the suffix itself when the content starts with `Class <name>` or `ROUTINE <name>`."
        ),
    ))
}

async fn http_err(
    resp: reqwest::Response,
    doc_name: Option<&str>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
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
    // #71: IRIS says the NAME is invalid; what is missing is the type suffix. Name the
    // remedy rather than sending the caller hunting for a naming-convention problem that
    // does not exist — five consecutive #16006 calls in one measured run.
    if body.contains("16006") {
        if let Some((suggestion, hint)) = doc_name.and_then(doc_suffix_hint) {
            extra["did_you_mean"] = serde_json::json!([suggestion]);
            extra["hint"] = serde_json::Value::String(hint);
        }
    }
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
    checkout_cache: &crate::elicitation::CheckoutCache,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match DocMode::parse(&p.mode) {
        Some(DocMode::Get) => handle_get(iris, client, p).await,
        Some(DocMode::Put) => handle_put(iris, client, p, elicitation_store, checkout_cache).await,
        Some(DocMode::Delete) => handle_delete(iris, client, p).await,
        Some(DocMode::Head) => handle_head(iris, client, p).await,
        Some(DocMode::InsertLines) | Some(DocMode::DeleteLines) => {
            handle_line_edit(iris, client, p, elicitation_store, checkout_cache).await
        }
        None => crate::tools::envelope::fail_with(
            "INVALID_PARAM",
            &format!(
                "Unknown mode='{}'. Use: {}.",
                p.mode,
                DocMode::valid_values()
            ),
            serde_json::json!({"mode": p.mode}),
        ),
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
        // #102 P1: this arm was byte-identical to the baseline while the single-name path
        // below it was fixed, so the BATCH form still answered
        // {"documents":[{"error":"HTTP 404 Not Found"},…],"success":true} — isError:false —
        // for a namespace that does not exist, never naming the namespace. The envelope
        // claiming success made it worse than the single-name bug the issue described.
        // Track the statuses so the summary below is a reading of what happened, not a
        // constant.
        let mut first_404_url: Option<String> = None;
        let mut worst_status: Option<u16> = None;
        let mut ok_count = 0usize;
        while let Some(res) = set.join_next().await {
            if let Ok((name, fetch_result)) = res {
                let entry = match fetch_result {
                    Ok(resp) if resp.status().is_success() => {
                        ok_count += 1;
                        let body: serde_json::Value = resp.json().await.unwrap_or_default();
                        let content = doc_content_to_string(&body);
                        serde_json::json!({"name": name, "content": content})
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        if status.as_u16() == 404 && first_404_url.is_none() {
                            first_404_url = Some(iris.versioned_ns_url(
                                &namespace,
                                &format!("/doc/{}", urlencoding::encode(&name)),
                            ));
                        }
                        // A per-document `error_code` so a caller branches on the code here
                        // exactly as it does on the envelope's — a 401 in this array used to
                        // be indistinguishable from a missing document.
                        //
                        // The status that speaks for the whole batch is the most ACTIONABLE
                        // one, not the numerically largest: in a mixed 401/404 batch the
                        // credentials are the reason to act, and "some of these documents do
                        // not exist" is a detail of a call that was never allowed to run.
                        worst_status = Some(match (worst_status, status.as_u16()) {
                            (Some(w), _)
                                if crate::tools::envelope::auth_status_code(w).is_some() =>
                            {
                                w
                            }
                            (_, s) if crate::tools::envelope::auth_status_code(s).is_some() => s,
                            (Some(w), s) => w.max(s),
                            (None, s) => s,
                        });
                        serde_json::json!({
                            "name": name,
                            "error": format!("HTTP {status}"),
                            "error_code": crate::tools::envelope::http_status_code(status.as_u16()),
                        })
                    }
                    Err(e) => serde_json::json!({
                        "name": name,
                        "error": e.to_string(),
                        "error_code": crate::tools::envelope::transport_error_code(&e.to_string()),
                    }),
                };
                map.insert(name, entry);
            }
        }
        let results: Vec<_> = p.names.iter().filter_map(|n| map.remove(n)).collect();
        // `!p.names.is_empty()`, not `!results.is_empty()`: a join that never produced an
        // entry at all is still a document that was not read, and the empty-`documents`
        // envelope must not come back claiming success either.
        if ok_count == 0 && !p.names.is_empty() {
            // Nothing was read. `success: true` here is the #102 shape verbatim — an envelope
            // claiming the call worked while every document in it failed.
            if let Some(url) = &first_404_url {
                if let Some(explained) = crate::tools::interop::namespace_missing_error(
                    iris,
                    client,
                    &namespace,
                    url,
                    "Nothing was read.",
                )
                .await
                {
                    return explained;
                }
            }
            let code = worst_status.map_or("IRIS_REQUEST_FAILED", |s| {
                crate::tools::envelope::http_status_code(s)
            });
            return crate::tools::envelope::fail_with(
                code,
                &format!(
                    "None of the {} requested documents could be read from namespace \
                     '{namespace}' — see `documents` for the per-document status.",
                    p.names.len()
                ),
                serde_json::json!({"documents": results, "namespace": namespace}),
            );
        }
        // A partial failure keeps success:true — some documents WERE read — but names the
        // namespace it read them from, which the baseline never did either.
        return ok_json(serde_json::json!({
            "success": true,
            "documents": results,
            "namespace": namespace,
        }));
    }

    let name = match require_name(&p, "get") {
        Ok(n) => n,
        Err(r) => return r,
    };
    let name = name.as_str();
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = match client
        .get(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
    {
        Ok(v) => v,
        Err(e) => return crate::tools::envelope::transport_fail("handle_get", &e.to_string()),
    };

    if resp.status().as_u16() == 404 {
        // #102 P1: the 404 body is ZERO bytes, so "no such document" and "no such namespace"
        // are the same wire response — and this named the DOCUMENT for both. `None` from the
        // helper means the namespace is confirmed present (or cannot be checked), and then
        // NOT_FOUND naming the document is right and survives.
        if let Some(missing) = crate::tools::interop::namespace_missing_error(
            iris,
            client,
            &namespace,
            &url,
            "Nothing was read.",
        )
        .await
        {
            return missing;
        }
        return err_json("NOT_FOUND", &format!("Document not found: {name}"));
    }
    if !resp.status().is_success() {
        return http_err(resp, Some(name)).await;
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

/// #24: `insert_lines` / `delete_lines` — a positional edit instead of a full re-upload.
///
/// READ, MODIFY, WRITE, and every step has a way of going quietly wrong:
///
/// * The read goes through `handle_get`, so all of its behaviour is inherited rather than
///   reimplemented — the namespace-404 answer, the not-found envelope, the Atelier error shapes. If it
///   fails, its envelope is returned unchanged.
/// * **`handle_get` PAGINATES.** Editing a partial read and writing it back TRUNCATES THE DOCUMENT. The
///   request forces a full read (`max_bytes: 0`, `offset: 0`) AND the result is checked for
///   `truncated` anyway: the forcing is what should make it impossible, the check is what makes a
///   mistake in that reasoning loud instead of destructive.
/// * The write goes through `write_with_scm`, the same path `put` uses, so source control behaves
///   identically. Calling `do_write` directly would skip the checkout and fail with #5865 on an
///   SCM-enabled instance.
async fn handle_line_edit(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
    elicitation_store: &crate::elicitation::ElicitationStore,
    checkout_cache: &crate::elicitation::CheckoutCache,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    use crate::tools::line_edit::{self, LineOp};

    let mode = DocMode::parse(&p.mode).unwrap_or(DocMode::InsertLines);
    let mode_name = mode.as_str();
    let name = match require_name(&p, mode_name) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let ns = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));

    let Some(at) = p.at else {
        return err_json(
            "MISSING_PARAMS",
            &format!("iris_doc mode={mode_name} requires `at` — the 1-based line to act at."),
        );
    };

    // Build the operation before reading, so a malformed request costs no round trip.
    let op =
        match mode {
            DocMode::DeleteLines => LineOp::Delete {
                at,
                count: p.count.unwrap_or(1),
            },
            _ => match p.lines.clone() {
                Some(lines) => LineOp::Insert { at, lines },
                None => return err_json(
                    "MISSING_PARAMS",
                    "iris_doc mode=insert_lines requires `lines` — the lines to insert, without \
                     newlines.",
                ),
            },
        };

    // ── read ────────────────────────────────────────────────────────────────────────────────
    let get_params = IrisDocParams {
        mode: "get".into(),
        name: Some(name.clone()),
        names: vec![],
        // FORCE a complete read. See the truncation note above.
        max_bytes: 0,
        offset: 0,
        ..p.clone()
    };
    let got = handle_get(iris, client, get_params).await?;
    let payload = match result_payload(&got) {
        Some(v) => v,
        None => return Ok(got),
    };
    if payload["success"] != serde_json::Value::Bool(true) {
        // The read failed. Return its envelope verbatim — it already names the real cause.
        return Ok(got);
    }
    if read_was_truncated(&payload) {
        return err_json(
            "READ_TRUNCATED",
            &format!(
                "REFUSED: the read of {name} came back truncated, and editing a partial document \
                 would write back a TRUNCATED one — destroying everything past the cut. This should \
                 be impossible (the read forces max_bytes=0, offset=0), so treat it as a bug rather \
                 than retrying."
            ),
        );
    }
    let Some(content) = payload["content"].as_str() else {
        return err_json(
            "READ_UNREADABLE",
            &format!("The read of {name} returned no `content` to edit."),
        );
    };

    // ── modify ──────────────────────────────────────────────────────────────────────────────
    let (before, trailing_newline) = line_edit::split_lines(content);
    if let Err(bad) = line_edit::validate(&op, &before, p.expect.as_deref()) {
        return crate::tools::envelope::fail_with(
            bad.code(),
            &bad.message(),
            serde_json::json!({
                "name": name, "namespace": ns, "mode": mode_name,
                "lines_total": before.len(),
            }),
        );
    }
    let after = line_edit::apply(&before, &op);
    let summary = line_edit::summarise(&before, &after, &op);
    let new_content = line_edit::join_lines(&after, trailing_newline);

    // ── write ───────────────────────────────────────────────────────────────────────────────
    let written = write_with_scm(
        iris,
        client,
        &name,
        &new_content,
        &ns,
        p.compile,
        p.allow_storage_regeneration,
        elicitation_store,
        checkout_cache,
    )
    .await?;
    // On failure return the write envelope unchanged: it carries the compile errors or the SCM
    // elicitation, which is what the caller has to act on. Attaching an edit summary to a failed write
    // would read as though the edit had landed.
    if !write_result_succeeded(&written) {
        return Ok(written);
    }
    let mut out = result_payload(&written).unwrap_or_else(|| serde_json::json!({"success": true}));
    out["mode"] = serde_json::Value::String(mode_name.to_string());
    out["line_edit"] = serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
    ok_json(out)
}

/// Whether a `handle_get` payload came back paginated.
///
/// UNREACHABLE by construction on the line-edit path, because the read forces `max_bytes: 0` and
/// `offset: 0` — and `a_caller_supplied_max_bytes_cannot_cause_a_truncated_write` is the test that keeps
/// that forcing in place. This is the second line of defence: if the forcing is ever removed, the guard
/// turns a silent document truncation into a refusal.
///
/// A mutation removing the `if` therefore SURVIVES the handler tests and always will — nothing the mock
/// can return makes a forced-full read truncated. Its logic is tested directly instead, and that
/// asymmetry is stated rather than papered over.
fn read_was_truncated(payload: &serde_json::Value) -> bool {
    payload["truncated"] == serde_json::Value::Bool(true)
}

/// The JSON a handler put in its `CallToolResult`, for composing one handler out of another.
fn result_payload(r: &rmcp::model::CallToolResult) -> Option<serde_json::Value> {
    match &r.content.first()?.raw {
        rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).ok(),
        _ => None,
    }
}

async fn handle_put(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
    elicitation_store: &crate::elicitation::ElicitationStore,
    checkout_cache: &crate::elicitation::CheckoutCache,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let ns = &namespace;

    // Elicitation resume — user answered a prior SCM dialog
    if let (Some(eid), Some(answer)) = (&p.elicitation_id, &p.elicitation_answer) {
        // #305: ONE lookup, matched once. Calling it twice would misreport: the first call removes
        // an expired entry, so the second sees NotFound and the user is told the id never existed.
        let looked_up = elicitation_store.lookup(eid);
        if let crate::elicitation::LookupResult::Found(pending) = looked_up {
            elicitation_store.clear(eid);
            if answer.to_lowercase() != "yes" {
                return crate::tools::envelope::fail("WRITE_ABORTED", "User declined checkout");
            }
            // Finalize the checkout the user just approved. The pre-write check only
            // ran UserAction (which *offers* the dialog); the checkout is not actually
            // committed until AfterUserAction is called. Because do_write is a separate
            // HTTP job, the in-memory %SourceControl session from the pre-write check is
            // already gone — so without this the write hits ERROR #5865 "not checked out
            // of source control". AfterUserAction persists the checkout server-side.
            let after_code = crate::tools::scm::after_user_action_code(
                "%CheckOut",
                &pending.document,
                "yes",
                &iris.username,
                &iris.password,
            );
            if let Ok(out) = iris
                .execute_via_generator(&after_code, &pending.namespace, client)
                .await
            {
                let out = out.lines().next().unwrap_or("").trim().to_string();
                // Non-empty output from after_user_action_code is an SCM error string.
                if !out.is_empty() && out != "SCM_UNAVAILABLE" {
                    return err_json("SCM_CHECKOUT_FAILED", &out);
                }
            }
            // Checkout is now committed server-side — cache it so writes that follow
            // in quick succession skip the redundant pre-write probe.
            checkout_cache.mark(&pending.namespace, &pending.document);

            // User said yes — proceed with the stored content directly
            let resume_content = pending.content.as_deref().unwrap_or("");
            return do_write(
                iris,
                client,
                &pending.document,
                resume_content,
                &pending.namespace,
                p.compile,
                p.allow_storage_regeneration,
            )
            .await;
        }
        // Not found: say WHICH miss it was — the remedies differ. `looked_up` was consumed by the
        // `if let` above only on the Found arm, so this is the same single lookup, not a second one.
        return match looked_up {
            crate::elicitation::LookupResult::Expired => err_json(
                "ELICITATION_EXPIRED",
                "This elicitation has expired — they are held for 5 minutes. Re-run the write to \
                 get a new dialog.",
            ),
            _ => err_json(
                "ELICITATION_NOT_FOUND",
                "No elicitation with that id. Check the `elicitation_id` you sent; note the store \
                 is in-memory, so a server restart discards pending dialogs.",
            ),
        };
    }

    let name = match require_name(&p, "put") {
        Ok(n) => n,
        Err(r) => return r,
    };
    // #71: `Class <name>` in the content settles the type, so add the suffix instead of
    // letting Atelier answer "#16006 name is invalid" about a perfectly good class name.
    let name = match doc_name_with_suffix(&name, p.content.as_deref()) {
        Some(fixed) => {
            tracing::info!(from = %name, to = %fixed, "iris_doc: added the missing document suffix");
            fixed
        }
        None => name,
    };
    let name = name.as_str();

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

    write_with_scm(
        iris,
        client,
        name,
        content,
        ns,
        p.compile,
        p.allow_storage_regeneration,
        elicitation_store,
        checkout_cache,
    )
    .await
}

/// Run the SCM pre-write check, then write. `content` is the full document body to write.
// Args are all distinct scalars/handles threaded straight through from the tool entry point;
// bundling them into a struct would add indirection without clarifying anything.
#[allow(clippy::too_many_arguments)]
async fn write_with_scm(
    iris: &IrisConnection,
    client: &reqwest::Client,
    name: &str,
    content: &str,
    ns: &str,
    compile: bool,
    allow_storage_regeneration: bool,
    elicitation_store: &crate::elicitation::ElicitationStore,
    checkout_cache: &crate::elicitation::CheckoutCache,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    // Fast path: if we already checked this doc out earlier this session (cache hit), skip the
    // pre-write SCM probe entirely — it is one IRIS round-trip that returns the same "proceed"
    // answer every time on a chained write. A stale entry self-heals: the write below still
    // goes through IRIS, and if it is rejected we invalidate so the retry re-probes.
    if checkout_cache.is_checked_out(ns, name) {
        let result = do_write(
            iris,
            client,
            name,
            content,
            ns,
            compile,
            allow_storage_regeneration,
        )
        .await?;
        if !write_result_succeeded(&result) {
            // Cache was stale (checkout lost out-of-band) — drop it so the next call re-probes.
            checkout_cache.invalidate(ns, name);
        }
        return Ok(result);
    }

    // SCM pre-write check — uses SourceControlCreate for a proper session (HTTP-compatible).
    // %GetImplementationObject does not exist on any IRIS version; use Interface API instead.
    //
    // First inspect the MenuItems: if %UndoCheckout is offered, WE already hold the checkout,
    // so we must NOT re-run the %CheckOut probe. Re-invoking %CheckOut on a doc we already hold
    // returns action=1 ("needs confirmation dialog"), which made every chained edit on an
    // already-checked-out doc re-elicit "requires checkout" forever.
    // In that case emit a PROCEED sentinel and write directly.
    let n = name.replace('"', "\"\""); // ObjectScript double-quote escaping
    let scm_check = format!(
        "set scmClass=##class(%Studio.SourceControl.Interface).SourceControlClassGet() if scmClass=\"\" {{ write \"NO_SCM\" }} else {{ set sc=##class(%Studio.SourceControl.Interface).SourceControlCreate(\"{u}\",\"{p}\",.c,.f,.o) set obj=$get(%SourceControl) if '$IsObject(obj) {{ write \"NO_SCM\" }} else {{ set hasUndoCheckout=0 try {{ set rset=##class(%ResultSet).%New(\"%Studio.SourceControl.Interface:MenuItems\") set sc=rset.Execute(\"%SourceMenu\",\"{n}\",\"\") while rset.Next() {{ if rset.GetData(2)&&(rset.GetData(1)=\"%UndoCheckout\") {{ set hasUndoCheckout=1 }} }} }} catch {{}} if hasUndoCheckout {{ write \"PROCEED|\" }} else {{ set action=0 set msg=\"\" set target=\"\" set reload=0 set sc=obj.UserAction(0,\"%SourceMenu,%CheckOut\",\"{n}\",\"\",.action,.target,.msg,.reload) write action_\"|\"_msg }} }} }}",
        u = iris.username.replace('"', "\"\""),
        p = iris.password.replace('"', "\"\""),
    );
    // Whether the probe told us the doc is already writable by us (PROCEED / already checked out).
    // Only such a "we hold it" outcome is safe to cache — NOT NO_SCM (no source control at all),
    // where there is no checkout to remember.
    let mut we_hold_checkout = false;
    if let Ok(out) = iris.execute_via_generator(&scm_check, ns, client).await {
        let out = out.trim().to_string();
        // "NO_SCM"/empty → no source control; "PROCEED" → we already hold the checkout.
        // Both skip the checkout dialog and fall through to do_write below.
        if out.starts_with("PROCEED") {
            we_hold_checkout = true;
        } else if out != "NO_SCM" && !out.is_empty() {
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
                    ns.to_string(),
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

    let result = do_write(
        iris,
        client,
        name,
        content,
        ns,
        compile,
        allow_storage_regeneration,
    )
    .await?;
    // Remember the checkout only when the probe confirmed we hold it AND the write landed, so
    // the next chained edit skips the probe. Never cache when there is no SCM (nothing to hold).
    if we_hold_checkout && write_result_succeeded(&result) {
        checkout_cache.mark(ns, name);
    }
    Ok(result)
}

/// Inspect a `do_write` result and report whether the write succeeded (JSON `success:true`).
fn write_result_succeeded(result: &rmcp::model::CallToolResult) -> bool {
    result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t.text).ok())
        .map(|v| v["success"] == serde_json::Value::Bool(true))
        .unwrap_or(false)
}

// #331: `storage_strip_blocked_message` lived here — the STORAGE_STRIP_BLOCKED text that #217
// rewrote so the FIX preceded the bypass. It is deleted with the refusal it worded, not because #217
// was wrong: its wording was right for a refusal that had to exist. Nothing strips Storage any more
// (the parser accepts it — measured on 2025.3 and 2026.1), so there is no refusal to word, and its
// first instruction — delete the block and write again — is the data-layout loss this issue is about.
// `strip_cls_suffix` went with it: the refusal was its only production caller.

async fn do_write(
    iris: &IrisConnection,
    client: &reqwest::Client,
    name: &str,
    content: &str,
    namespace: &str,
    compile_after: bool,
    allow_storage_regeneration: bool,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    // #331: THE SOURCE IS WRITTEN UNTOUCHED. Storage blocks are no longer stripped.
    //
    // WHY IT USED TO STRIP. `doc.rs` carried "IRIS 2025.1 UDL parser (#5559) fails on Storage XML",
    // stripped every block unconditionally — nothing consulted the server version — and then refused
    // the write rather than discard a layout silently. The refusal was right; the strip it was
    // guarding against was the problem.
    //
    // WHY IT NO LONGER DOES. Measured on two writable throwaway instances, through plain Atelier REST
    // so the transport itself was under test:
    //
    //   2025.3 (the tag ci.yml pins)   PUT 201 errors:[]   compile 200 errors:[]   block preserved
    //   2026.1 (Build 235U)            PUT 200 errors:[]   compile 200 errors:[]   block preserved
    //
    // Read back after compile, the slot list (`%%CLASSNAME`, then each property in order) and
    // `<DataLocation>` were byte-intact on both. So the strip removed a block the server would have
    // accepted, and the guard then refused the write BECAUSE the strip had happened: a loop entirely
    // of our own making. `iris_doc(get)` returns the generated block, so every get -> edit -> put of
    // a compiled %Persistent class hit it — and the refusal's own first instruction, "delete the
    // Storage block and write the class again", is the data-layout loss reached by the other door.
    //
    // WHY PASSING IT THROUGH IS THE ONLY SAFE OPTION. A generated block is not derivable from the
    // current property set: IRIS mints arbitrary global names (`Ens.Config.Credentials` stores to
    // `^Ens.Conf.CredentialsD`) and tracks slots across properties added, deleted and renamed, so a
    // deleted property's slot stays vacant to keep the survivors in place. Regeneration re-packs, and
    // a stored row is a $list addressed by slot number — slot 3 stops being `Username` while every
    // existing row still holds the old layout. Nothing errors, and both may be strings.
    //
    // Not verified: 2025.1 itself, whose community licence has expired, so the container will not
    // start. If the limitation was ever real there it was fixed by 2025.3. A version gate can be
    // added if that support is required; `strip_storage_blocks` and its unit tests are kept for
    // exactly that, unused on this path.
    let content_for_write = content;
    let _ = allow_storage_regeneration; // retired (#331); see the field's doc comment

    // Name the methods that will run ObjectScript at COMPILE time. This is reported on
    // every write and refused only when the caller asked for that (IRIS_BLOCK_CODEGEN=1)
    // — see `compile_time_methods` for why the default is to inform rather than block.
    let generators = compile_time_methods(content_for_write);
    if !generators.is_empty() && block_compile_time_code() {
        return crate::tools::envelope::fail_with(
            "COMPILE_TIME_CODE_BLOCKED",
            &format!(
                "'{name}' declares CodeMode = objectgenerator on {} ({}). A generator body \
                 runs at compile time under the identity of whoever compiles the class, not \
                 only this connection. IRIS_BLOCK_CODEGEN is set, so the write was refused. \
                 Unset it to allow generator methods.",
                if generators.len() == 1 {
                    "one method"
                } else {
                    "these methods"
                },
                generators.join(", ")
            ),
            serde_json::json!({
                "name": name,
                "compile_time_methods": generators,
                "blocked_by": "IRIS_BLOCK_CODEGEN=1",
            }),
        );
    }
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
    let resp = match crate::tools::concurrency::send_with_retry(
        || {
            client
                .put(&url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&put_body)
        },
        false,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return crate::tools::envelope::transport_fail("do_write", &e.to_string()),
    };

    if !resp.status().is_success() {
        return http_err(resp, Some(name)).await;
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
                // #80 was fixed in iris_compile and in `compile_document` and never reached
                // HERE, the third copy of the same loop. This build prefixes per-method
                // diagnostics with `ERROR:` (colon); matching only `ERROR ` (space) missed
                // every one of them. Measured live on IRIS 2026.1 (Build 235U): a 3-method
                // class with 3 undefined macros printed `Detected 13 errors` and this path
                // reported ONE — `#5123 Unable to find entry point`, a cascade, while the
                // macros that caused it never appeared.
                let errs = crate::tools::compile_error_list(&body, &console);
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
            let mut payload = serde_json::json!({
                "name": name,
                "open_uri": open_uri,
                "compiled": false,
                "compile_errors": compile_errors,
                "compile_console": compile_console,
            });
            // #213: #5559 blames braces and is usually wrong. On THIS path the class source is
            // still in scope, so the hint can name the members whose names carry `_`. Written
            // BEFORE note_error_undercount, which overwrites `hint` unconditionally when it
            // fires — that ordering keeps the existing undercount regression green.
            if let Some((h, offenders)) = crate::tools::hint_5559(&first, Some(content_for_write)) {
                payload["hint"] = serde_json::Value::String(h);
                if !offenders.is_empty() {
                    payload["did_you_mean"] = serde_json::Value::Array(
                        offenders
                            .iter()
                            .map(|m| serde_json::Value::String(m.replace('_', "")))
                            .collect(),
                    );
                    payload["underscored_members"] = serde_json::json!(offenders);
                }
            }
            crate::tools::note_error_undercount(
                &mut payload,
                crate::tools::detected_error_count(compile_console.iter().map(String::as_str)),
                compile_errors.len(),
                "compile_console",
            );
            note_compile_time_methods(&mut payload, &generators);
            // #263 proposal 2: run the lookup FIRST, so the hint below can name the delete id
            // instead of prescribing a SELECT. No-op (and no request) for anything else.
            crate::tools::prop_collision::enrich(&mut payload, iris, client, namespace, &first)
                .await;
            // #263: AFTER note_error_undercount, which overwrites `hint` unconditionally when
            // IRIS's count beats the parsed list. Its facts are kept; only the text yields.
            crate::tools::envelope::apply_prop_collision_hint(&mut payload, &first);
            return crate::tools::envelope::fail_with("COMPILE_ERROR", &first, payload);
        }
        let mut payload = serde_json::json!({
            "success": true,
            "name": name,
            "open_uri": open_uri,
            "compiled": true,
            "compile_errors": compile_errors,
            "compile_console": compile_console,
        });
        // Reached with compile_errors EMPTY. If IRIS still counted errors here, "compiled:
        // true" is the undercount at its worst — a failed compile reported as a success.
        crate::tools::note_error_undercount(
            &mut payload,
            crate::tools::detected_error_count(compile_console.iter().map(String::as_str)),
            compile_errors.len(),
            "compile_console",
        );
        note_compile_time_methods(&mut payload, &generators);
        return ok_json(payload);
    }

    // #331: `storage_stripped` is gone from the payload. Nothing strips any more, so the key would
    // be structurally always-false — and a field that cannot vary is the defect #332 documented one
    // tool over: an advertised value that cannot occur teaches a caller to branch on something dead.
    // It was never named in the iris_doc description, so no documented contract changes here.
    let mut payload = serde_json::json!({"success": true, "name": name, "open_uri": open_uri});
    note_compile_time_methods(&mut payload, &generators);
    ok_json(payload)
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
                Ok(r) if r.status().is_success() => {
                    // HTTP 200 doesn't mean the delete happened — a locked / checked-out
                    // doc (ERROR #5845) still returns 200 with the failure in
                    // status.errors. Treat a non-empty status.errors as a failure so a
                    // locked doc lands in `errors`, not `deleted`.
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    match body["status"]["errors"].as_array() {
                        Some(errs) if !errs.is_empty() => {
                            let msg = errs[0]["error"].as_str().unwrap_or("delete failed");
                            errors.push(serde_json::json!({"name": name, "error": msg}));
                        }
                        _ => deleted.push(name.clone()),
                    }
                }
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

    let name = match require_name(&p, "delete") {
        Ok(n) => n,
        Err(r) => return r,
    };
    let name = name.as_str();
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = match client
        .delete(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
    {
        Ok(v) => v,
        Err(e) => return crate::tools::envelope::transport_fail("handle_delete", &e.to_string()),
    };

    if resp.status().as_u16() == 404 {
        // #102 P1: same zero-byte 404 as handle_get — a missing namespace was reported as a
        // missing document, and the caller had no way to tell.
        if let Some(missing) = crate::tools::interop::namespace_missing_error(
            iris,
            client,
            &namespace,
            &url,
            "Nothing was deleted.",
        )
        .await
        {
            return missing;
        }
        return err_json("NOT_FOUND", &format!("Document not found: {name}"));
    }
    if !resp.status().is_success() {
        return http_err(resp, Some(name)).await;
    }
    // Atelier returns HTTP 200 even when the delete failed server-side (e.g. the doc is
    // locked / checked out -> ERROR #5845): the real failure is in the JSON body's
    // status.errors, not the HTTP status. Without this check we'd report success:true
    // for a delete that never happened (mirrors the put path).
    let del_body: serde_json::Value = resp.json().await.unwrap_or_default();
    if let Some(errs) = del_body["status"]["errors"].as_array() {
        if !errs.is_empty() {
            let msg = errs[0]["error"]
                .as_str()
                .unwrap_or("Document delete failed");
            return err_json("DELETE_FAILED", msg);
        }
    }
    ok_json(serde_json::json!({"success": true, "name": name}))
}

async fn handle_head(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: IrisDocParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let name = match require_name(&p, "head") {
        Ok(n) => n,
        Err(r) => return r,
    };
    let name = name.as_str();
    let url = iris.versioned_ns_url(&namespace, &format!("/doc/{}", urlencoding::encode(name)));
    let resp = match client
        .head(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
    {
        Ok(v) => v,
        Err(e) => return crate::tools::envelope::transport_fail("handle_head", &e.to_string()),
    };

    // #102 P0: this was `let exists = resp.status().is_success();` — so EVERY non-2xx became
    // a confident `{"success":true,"exists":false}`. Verified live twice: a document that
    // provably exists was reported absent both for a namespace that does not exist (404) and,
    // separately, for a wrong password against a namespace that does (401). A failed call must
    // become an ERROR, never a negative answer. Only a 404 is a legitimate "not there".
    let status = resp.status();
    if !status.is_success() && status.as_u16() != 404 {
        return http_err(resp, Some(name)).await;
    }
    if status.as_u16() == 404 {
        // ...and even a 404 has two meanings on a zero-byte body: no such document, or no
        // such namespace. This asked `namespace_missing_error`, whose `None` merges "the
        // namespace is confirmed present" with "nothing could be established" — and then fell
        // through to `exists = status.is_success()` for BOTH. Under a wrong IRIS_WEB_PREFIX
        // that produced `{"success":true,"exists":false}` for %Library.String.cls, a class
        // that answers exists:true one call earlier on the right prefix (reproduced live, and
        // again against an all-404 stub). `exists:false` is a FACT claim; it may only be
        // emitted on the arm where the namespace was actually confirmed.
        use crate::tools::interop::FourOhFour;
        match crate::tools::interop::classify_404(
            iris,
            client,
            &namespace,
            &url,
            "Nothing was read.",
        )
        .await
        {
            FourOhFour::Explained(e) => return e,
            // The namespace is there, so the document really is not: the true negative, and
            // the overwhelmingly common case, is preserved exactly.
            FourOhFour::TargetMissing => {}
            FourOhFour::Undetermined => {
                return crate::tools::interop::indeterminate_404_error(
                    iris,
                    &namespace,
                    &url,
                    &format!(
                        "Whether '{name}' exists is therefore unknown — reporting exists:false \
                         here would be a guess dressed as a reading."
                    ),
                )
            }
        }
    }
    let exists = status.is_success();
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
                let opens = line.chars().filter(|&c| c == '{').count() as i32;
                let closes = line.chars().filter(|&c| c == '}').count() as i32;
                brace_depth += opens - closes;
                // Only exit immediately if this line contained a { and it balanced
                // (single-line storage like "Storage Default {}"). If brace_depth==0
                // because no { appeared yet, the { is on the next line — stay in_storage.
                if opens > 0 && brace_depth <= 0 {
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

/// Strip double-quoted ObjectScript literals from `line`, so brace counting and
/// keyword scanning never see syntax that lives inside a string. ObjectScript escapes
/// a quote by doubling it, so a `""` inside a literal does not end it.
fn without_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match (in_str, c) {
            (false, '"') => in_str = true,
            (false, _) => out.push(c),
            (true, '"') => {
                // A doubled quote is an escaped quote, not the end of the literal.
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_str = false;
                }
            }
            (true, _) => {}
        }
    }
    out
}

/// The method name and keyword block of a member signature line, if it starts one.
/// `Method`/`ClassMethod`/`ClientMethod` only — a `Property` or `Parameter` carries no
/// CodeMode, and matching them would only widen the surface for no gain.
fn member_signature_name(sig: &str) -> Option<&str> {
    let rest = ["ClassMethod", "ClientMethod", "Method"]
        .iter()
        .find_map(|kw| sig.strip_prefix(*kw))?;
    // Require real whitespace after the keyword: `Methodical(` is not a method named
    // `ical`, and `ClassMethodFoo` is not a ClassMethod.
    let rest = rest.strip_prefix(char::is_whitespace)?.trim_start();
    let name = rest
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .unwrap_or("");
    (!name.is_empty()).then_some(name)
}

/// `CodeMode` as declared in a member's `[ ... ]` keyword block, lowercased.
fn declared_code_mode(sig: &str) -> Option<String> {
    // The keyword block is the last bracketed run on the signature. Searching from the
    // end skips any `[` that appeared in a default value or a type parameter.
    let open = sig.rfind('[')?;
    let block = &sig[open + 1..];
    let block = block.split(']').next().unwrap_or(block);
    block.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim().eq_ignore_ascii_case("CodeMode")).then(|| v.trim().to_lowercase())
    })
}

/// Names of the methods in `content` that declare `CodeMode = objectgenerator`,
/// in source order.
///
/// Why this one keyword is worth naming: a generator method's body is ObjectScript
/// that runs at COMPILE time, under the identity of whoever triggers the compile —
/// which need not be the agent that wrote the class. `iris_execute` also runs
/// arbitrary ObjectScript, so on a write-allowed connection a generator grants no new
/// capability *now*; what it adds is code that fires LATER and as SOMEONE ELSE.
///
/// `CodeMode = expression` and `CodeMode = call` are deliberately not reported. They
/// inline the body at the call site; they do not execute a generator at compile time.
/// Upstream's gate refuses all three, which is why this one does not simply port it.
///
/// Only member SIGNATURES are scanned, never bodies. Braces are counted with string
/// literals removed, so a method body that merely mentions `CodeMode = objectgenerator`
/// in a comment, a string or a doc sample sits at depth >= 2 and is never considered.
pub fn compile_time_methods(content: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut depth: i32 = 0;
    // A signature may wrap across lines; accumulate until the body opens.
    let mut pending: Option<(String, String)> = None;

    for line in content.lines() {
        let code = without_literals(line);
        let trimmed = code.trim();
        // `///` and `//` are documentation/comment lines; `;` is the ObjectScript
        // comment form. None of them can carry a signature.
        let is_comment = trimmed.starts_with("//") || trimmed.starts_with(';');

        if pending.is_none() && depth <= 1 && !is_comment {
            if let Some(name) = member_signature_name(trimmed) {
                pending = Some((name.to_string(), String::new()));
            }
        }
        if let Some((name, sig)) = pending.as_mut() {
            sig.push(' ');
            sig.push_str(trimmed);
            // The signature ends where the body opens, or at a `;` for the bodyless
            // forms. Either way the keyword block is complete by then.
            if trimmed.contains('{') || trimmed.ends_with(';') {
                if declared_code_mode(sig).as_deref() == Some("objectgenerator") {
                    found.push(name.clone());
                }
                pending = None;
            }
        }

        depth += code.matches('{').count() as i32;
        depth -= code.matches('}').count() as i32;
        if depth < 0 {
            depth = 0;
        }
    }
    found
}

/// Attach the generator finding to a write result. Absent when there is nothing to
/// report, so an ordinary class keeps the payload it has always had.
fn note_compile_time_methods(payload: &mut serde_json::Value, generators: &[String]) {
    if generators.is_empty() {
        return;
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "compile_time_methods".to_string(),
            serde_json::json!(generators),
        );
        obj.insert(
            "compile_time_code_note".to_string(),
            serde_json::json!(
                "These methods declare CodeMode = objectgenerator: their bodies run at \
                 compile time, under the identity of whoever compiles the class. The write \
                 was allowed; set IRIS_BLOCK_CODEGEN=1 to refuse writes like this one."
            ),
        );
    }
}

/// Whether the caller opted in to refusing generator writes outright.
/// Off by default: this server targets development instances, where the write is
/// already allowed and `iris_execute` runs arbitrary ObjectScript anyway. On a Live
/// instance the #114 gate refuses the whole write before this is ever consulted, so
/// blocking by default would buy nothing and would break writing back an
/// IRIS-generated class that legitimately carries a generator.
pub fn block_compile_time_code() -> bool {
    std::env::var("IRIS_BLOCK_CODEGEN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub(crate) fn doc_content_to_string(body: &serde_json::Value) -> String {
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
mod compile_time_code_tests {
    use super::{block_compile_time_code, compile_time_methods, without_literals};

    const GENERATOR: &str = r#"Class Demo.BS.Probe Extends %RegisteredObject
{

Parameter DOMAIN = "Demo";

/// Builds the dispatch table at compile time.
ClassMethod BuildTable() As %Status [ CodeMode = objectgenerator ]
{
    Do %code.WriteLine(" Quit 1")
    Quit $$$OK
}

Method Plain() As %String
{
    Quit "CodeMode = objectgenerator"
}

}
"#;

    #[test]
    fn finds_a_generator_method_by_name() {
        assert_eq!(compile_time_methods(GENERATOR), vec!["BuildTable"]);
    }

    #[test]
    fn a_body_that_merely_mentions_the_keyword_is_not_a_generator() {
        // The `Plain` body contains the exact phrase in a string literal. Reporting it
        // would be the false positive that makes an advisory worth ignoring.
        let only_bodies = r#"Class Demo.BS.Probe Extends %RegisteredObject
{

Method Plain() As %String
{
    // CodeMode = objectgenerator
    Quit "CodeMode = objectgenerator"
}

}
"#;
        assert!(compile_time_methods(only_bodies).is_empty());
    }

    #[test]
    fn an_ordinary_class_reports_nothing() {
        let plain = "Class Demo.MSG.Order Extends Ens.Request\n{\n\nProperty Id As %String;\n\n}\n";
        assert!(compile_time_methods(plain).is_empty());
    }

    #[test]
    fn expression_and_call_are_not_reported() {
        // They inline at the call site; they do not execute a generator at compile time.
        // Upstream's gate refuses them, which is the part this fork deliberately drops.
        let inlined = r#"Class Demo.Util Extends %RegisteredObject
{

ClassMethod Version() As %String [ CodeMode = expression ]
{
"1.0"
}

ClassMethod Legacy() [ CodeMode = call ]
{
Legacy^DemoUtil
}

}
"#;
        assert!(compile_time_methods(inlined).is_empty());
    }

    #[test]
    fn a_signature_wrapped_across_lines_still_matches() {
        let wrapped = r#"Class Demo.Util Extends %RegisteredObject
{

ClassMethod Build(pName As %String = "x") As %Status [ CodeMode = objectgenerator,
    Private ]
{
    Quit $$$OK
}

}
"#;
        assert_eq!(compile_time_methods(wrapped), vec!["Build"]);
    }

    #[test]
    fn the_keyword_is_matched_case_insensitively() {
        let odd = "Class D.U Extends %RegisteredObject\n{\n\nClassMethod G() [ codemode = ObjectGenerator ]\n{\n}\n\n}\n";
        assert_eq!(compile_time_methods(odd), vec!["G"]);
    }

    #[test]
    fn a_method_whose_name_merely_starts_with_the_keyword_is_not_a_member_start() {
        // `ClassMethodical` must not parse as a ClassMethod named `ical`.
        let odd =
            "Class D.U Extends %RegisteredObject\n{\n\nProperty ClassMethodical As %String;\n\n}\n";
        assert!(compile_time_methods(odd).is_empty());
    }

    #[test]
    fn every_generator_in_a_class_is_listed_in_source_order() {
        let two = r#"Class D.U Extends %RegisteredObject
{

ClassMethod First() [ CodeMode = objectgenerator ]
{
}

ClassMethod Second() [ CodeMode = objectgenerator ]
{
}

}
"#;
        assert_eq!(compile_time_methods(two), vec!["First", "Second"]);
    }

    #[test]
    fn a_brace_inside_a_string_does_not_swallow_the_rest_of_the_class() {
        // Unbalanced braces in a literal would leave the scanner stuck at depth >= 2
        // and silently miss every later generator — a false negative, not a noisy one.
        let tricky = r#"Class D.U Extends %RegisteredObject
{

Method Noisy() As %String
{
    Quit "{{{"
}

ClassMethod Gen() [ CodeMode = objectgenerator ]
{
}

}
"#;
        assert_eq!(compile_time_methods(tricky), vec!["Gen"]);
    }

    #[test]
    fn without_literals_keeps_doubled_quotes_inside_the_literal() {
        assert_eq!(without_literals(r#"Set x = "a""b" _ y"#), "Set x =  _ y");
    }

    #[test]
    fn blocking_is_off_unless_the_env_var_is_set() {
        // The default must stay "inform, do not block": on a Live instance the #114 gate
        // already refused the write, and on a dev instance friction is the thing this
        // fork exists to avoid.
        assert!(!block_compile_time_code());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_storage_blocks: brace-on-next-line (issue #18 / upstream #80) ──
    #[test]
    fn test_strip_storage_blocks_brace_on_next_line() {
        // "Storage Default" on its own line, opening "{" on the following line.
        // Prior to the fix, brace_depth was 0 after the "Storage Default" line,
        // triggering immediate in_storage=false and leaving the XML body and
        // closing "}" orphaned in the output.
        let cls = "Class App.Data.Probe Extends %Persistent\n{\nProperty Nome As %String;\n\nStorage Default\n{\n<Data name=\"ProbeDefaultData\">\n<Value name=\"1\"><Value>%%CLASSNAME</Value></Value>\n</Data>\n<DataLocation>^ProbeD</DataLocation>\n}\n}\n";
        let (stripped, flag) = strip_storage_blocks(cls);
        assert!(flag, "storage must be detected");
        assert!(
            !stripped.contains("<Data") && !stripped.contains("DataLocation"),
            "storage XML must not leak into the written class: {stripped}"
        );
        // Class body must survive, including its own closing brace.
        assert!(stripped.contains("Property Nome"));
        assert!(stripped.trim_end().ends_with('}'));
    }

    #[test]
    fn test_strip_storage_blocks_single_line_still_works() {
        let cls = "Class A.B\n{\nStorage Default {}\nProperty X As %String;\n}\n";
        let (stripped, flag) = strip_storage_blocks(cls);
        assert!(flag);
        assert!(stripped.contains("Property X"));
    }

    // ── DocMode string parsing (issue #18 / upstream 9bd133d) ────────────────
    #[test]
    fn test_docmode_parses_case_insensitive_and_rejects_unknown() {
        assert!(matches!(DocMode::parse("get"), Some(DocMode::Get)));
        assert!(matches!(DocMode::parse("PUT"), Some(DocMode::Put)));
        assert!(matches!(DocMode::parse("Delete"), Some(DocMode::Delete)));
        assert!(matches!(DocMode::parse("head"), Some(DocMode::Head)));
        assert!(DocMode::parse("fragment").is_none());
        assert!(DocMode::parse("").is_none());
    }

    #[test]
    fn test_doc_params_mode_is_plain_string_in_schema() {
        // The enum-typed param shipped `mode: {"$ref": "#/$defs/DocMode"}`, which
        // some MCP clients reject. As a String the schema must inline a type.
        let schema = schemars::schema_for!(IrisDocParams);
        let v = serde_json::to_value(&schema).unwrap();
        let mode = &v["properties"]["mode"];
        assert!(
            mode.get("$ref").is_none(),
            "mode must not be a $ref: {mode}"
        );
    }

    #[test]
    fn test_http_error_code_is_accurate_not_unreachable() {
        use reqwest::StatusCode;
        // Concurrency conflicts and client errors must NOT be reported as "unreachable".
        assert_eq!(http_error_code(StatusCode::BAD_REQUEST), "IRIS_BAD_REQUEST");
        assert_eq!(http_error_code(StatusCode::LOCKED), "IRIS_LOCKED");
        assert_eq!(http_error_code(StatusCode::CONFLICT), "IRIS_CONFLICT");
        // #101: 401 and 403 are two problems with two disjoint remedies — a 401 caller edits
        // IRIS_PASSWORD, a 403 caller cannot (IRIS validated the password before it could
        // evaluate %Development). One code would have left English prose in `hint` as the
        // only thing separating them.
        assert_eq!(
            http_error_code(StatusCode::UNAUTHORIZED),
            "IRIS_AUTH_FAILED"
        );
        assert_eq!(http_error_code(StatusCode::FORBIDDEN), "IRIS_FORBIDDEN");
        assert_ne!(
            http_error_code(StatusCode::UNAUTHORIZED),
            http_error_code(StatusCode::FORBIDDEN),
            "a 401 and a 403 must not share an error_code"
        );
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

#[cfg(test)]
mod doc_name_suffix_tests {
    use super::{doc_name_with_suffix, doc_suffix_hint, has_doc_suffix};

    /// #71: `Class <name>` settles the type — nothing is guessed, so put can add `.cls`
    /// instead of letting Atelier answer "#16006 name is invalid" about a good class name.
    #[test]
    fn class_content_settles_a_bare_name() {
        let content = "Class ZZVerify.BareName Extends %RegisteredObject\n{\n}\n";
        assert_eq!(
            doc_name_with_suffix("ZZVerify.BareName", Some(content)).as_deref(),
            Some("ZZVerify.BareName.cls")
        );
    }

    /// A name that already carries a suffix is left exactly as sent — including one this
    /// server does not add itself.
    #[test]
    fn an_existing_suffix_is_never_touched() {
        let content = "Class X Extends %RegisteredObject\n{\n}\n";
        assert_eq!(doc_name_with_suffix("X.cls", Some(content)), None);
        assert_eq!(doc_name_with_suffix("X.CLS", Some(content)), None);
        assert_eq!(doc_name_with_suffix("X.dfi", None), None);
        assert!(has_doc_suffix("Pkg.Sub.Thing.mac"));
        assert!(!has_doc_suffix("Pkg.Sub.Thing"));
    }

    /// Routines split by their header: `[Type=INC]` is an include, anything else a .mac.
    #[test]
    fn a_routine_header_picks_mac_or_inc() {
        assert_eq!(
            doc_name_with_suffix("MyApp.Util", Some("ROUTINE MyApp.Util\n w 1")).as_deref(),
            Some("MyApp.Util.mac")
        );
        assert_eq!(
            doc_name_with_suffix("MyApp.Macros", Some("ROUTINE MyApp.Macros [Type=INC]\n"))
                .as_deref(),
            Some("MyApp.Macros.inc")
        );
    }

    /// When the content does not settle it, nothing is invented — a wrong suffix would be
    /// worse than the error, and the error now carries the remedy.
    #[test]
    fn ambiguous_content_is_left_for_the_error_to_explain() {
        assert_eq!(doc_name_with_suffix("MyApp.Thing", Some(" quit 1")), None);
        assert_eq!(doc_name_with_suffix("MyApp.Thing", None), None);
        let (suggestion, hint) = doc_suffix_hint("MyApp.Thing").unwrap();
        assert_eq!(suggestion, "MyApp.Thing.cls");
        assert!(hint.contains("type suffix"), "{hint}");
        assert!(hint.contains("MyApp.Thing.cls"), "{hint}");
        assert!(doc_suffix_hint("MyApp.Thing.cls").is_none());
    }
}

// ── Issue #102: a failed call may never become a negative ANSWER ─────────────
//
// The matrix is the same for every site, because the defect was NOT 404-blindness — it was
// non-2xx blindness. Case (d) is the one the issue text missed and the one written first: a
// wrong password against a namespace that EXISTS produced the identical lie.
#[cfg(test)]
mod head_get_delete_status_tests {
    use super::*;
    use crate::iris::connection::DiscoverySource;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn root_descriptor(namespaces: &[&str]) -> serde_json::Value {
        serde_json::json!({"result": {"content": {
            "version": "IRIS for UNIX 2026.1", "api": 8, "namespaces": namespaces
        }}})
    }

    fn params(mode: &str, namespace: &str) -> IrisDocParams {
        serde_json::from_value(serde_json::json!({
            "mode": mode, "name": "Ens.Director.cls", "namespace": namespace
        }))
        .unwrap()
    }

    fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
        match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    /// Mount a `/doc/...` response for `verb` plus a root descriptor, then run the handler.
    async fn run(
        verb: &str,
        doc: ResponseTemplate,
        root: Option<ResponseTemplate>,
        ns: &str,
    ) -> (Option<bool>, serde_json::Value) {
        let server = MockServer::start().await;
        Mock::given(method(verb))
            .and(path_regex(r".*/doc/.*"))
            .respond_with(doc)
            .mount(&server)
            .await;
        if let Some(root) = root {
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(root)
                .mount(&server)
                .await;
        }
        let iris = IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let client = reqwest::Client::new();
        let mode = match verb {
            "HEAD" => "head",
            "DELETE" => "delete",
            _ => "get",
        };
        let p = params(mode, ns);
        let r = match mode {
            "head" => handle_head(&iris, &client, p).await,
            "delete" => handle_delete(&iris, &client, p).await,
            _ => handle_get(&iris, &client, p).await,
        }
        .expect("the tool must answer, not error out of the transport");
        (r.is_error, payload(&r))
    }

    fn ok_root() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP", "USER"]))
    }

    /// (a) A document that is there is still reported as there.
    #[test]
    fn head_still_answers_exists_true_for_a_document_that_exists() {
        rt().block_on(async {
            let (is_err, v) = run("HEAD", ResponseTemplate::new(200), None, "APP").await;
            assert_ne!(is_err, Some(true), "{v}");
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["exists"], true, "{v}");
        });
    }

    /// (b) The legitimate negative — the namespace IS there, the document is not. The guard
    /// against over-firing: this must NOT become NAMESPACE_NOT_FOUND.
    #[test]
    fn head_keeps_exists_false_when_the_namespace_is_confirmed_present() {
        rt().block_on(async {
            let (is_err, v) = run("HEAD", ResponseTemplate::new(404), Some(ok_root()), "APP").await;
            assert_ne!(is_err, Some(true), "{v}");
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["exists"], false, "{v}");
        });
    }

    /// (c) #93 applied to iris_doc: a 404 body is ZERO bytes, so only a second question can
    /// tell "no such document" from "no such namespace".
    #[test]
    fn head_names_the_namespace_when_the_namespace_is_the_reason() {
        rt().block_on(async {
            let (is_err, v) = run(
                "HEAD",
                ResponseTemplate::new(404),
                Some(ok_root()),
                "ZZNOSUCHNS",
            )
            .await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert_eq!(
                v["available_namespaces"],
                serde_json::json!(["APP", "USER"]),
                "{v}"
            );
            assert!(
                v["attempted_url"].as_str().unwrap().contains("ZZNOSUCHNS"),
                "{v}"
            );
            assert!(!v.to_string().contains("Check IRIS_HOST"), "{v}");
            assert!(
                v["hint"].as_str().unwrap().contains("Nothing was read"),
                "{v}"
            );
        });
    }

    /// (d) THE ASSERTION THAT ENCODES THE ISSUE. Live, with a wrong password against APP — a
    /// namespace that exists, on an instance that answers — `iris_doc(head)` said the document
    /// did not exist. It provably did. A 401 is not an answer about a document.
    #[test]
    fn head_reports_a_401_instead_of_claiming_the_document_is_absent() {
        rt().block_on(async {
            let (is_err, v) = run(
                "HEAD",
                ResponseTemplate::new(401).set_body_string("Unauthorized"),
                Some(ok_root()),
                "APP",
            )
            .await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert_eq!(
                v["exists"],
                serde_json::Value::Null,
                "no negative FACT: {v}"
            );
            assert_eq!(v["success"], false, "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
        });
    }

    /// (d, continued) 403 and 5xx are failures too, and each says which kind.
    #[test]
    fn head_codes_a_403_and_a_500_from_the_status() {
        rt().block_on(async {
            for (status, code) in [(403u16, "IRIS_FORBIDDEN"), (500, "IRIS_SERVER_ERROR")] {
                let (is_err, v) = run(
                    "HEAD",
                    ResponseTemplate::new(status),
                    Some(ok_root()),
                    "APP",
                )
                .await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_eq!(v["error_code"], code, "{v}");
                assert_eq!(v["exists"], serde_json::Value::Null, "{v}");
            }
        });
    }

    /// (e) The cannot-tell contract, corrected. It used to read "leave the caller's own
    /// ANSWER alone" and was pinned as `..._leaves_exists_false_alone` — which made a test
    /// out of the P0 itself. `exists:false` is not the caller's error being preserved; it is
    /// a FACT claim, `success:true`, `isError:false`, indistinguishable from the true
    /// negative, and it was emitted precisely when the server had established nothing.
    ///
    /// Reproduced live against IRIS 2026.1 with `IRIS_WEB_PREFIX=/zznoprefix`:
    /// `head(%Library.String.cls)` answered `{"exists":false,"success":true}` for a class
    /// that answered `exists:true` on the correct prefix seconds earlier.
    ///
    /// Cannot-tell means the tool says so. Nothing here may read as a reading.
    #[test]
    fn an_unreadable_root_descriptor_never_becomes_exists_false() {
        rt().block_on(async {
            for root in [
                // A sick instance, and an older Atelier that does not list namespaces.
                ResponseTemplate::new(500),
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"result": {"content": {"version": "IRIS 2019.1"}}}),
                ),
            ] {
                let (is_err, v) =
                    run("HEAD", ResponseTemplate::new(404), Some(root), "ZZNOSUCHNS").await;
                assert_eq!(is_err, Some(true), "an absent answer is an error: {v}");
                assert_eq!(v["error_code"], "INDETERMINATE", "{v}");
                assert_eq!(
                    v["exists"],
                    serde_json::Value::Null,
                    "no negative FACT may survive: {v}"
                );
                assert_eq!(v["success"], false, "{v}");
            }
        });
    }

    /// ...and the case that IS knowable is answered, not shrugged at. A 404 root descriptor
    /// means the Atelier application is not at this URL at all — the wrong-IRIS_WEB_PREFIX
    /// signature — so head names that instead of either guessing or giving up.
    #[test]
    fn a_404_root_descriptor_makes_head_name_the_prefix() {
        rt().block_on(async {
            let (is_err, v) = run(
                "HEAD",
                ResponseTemplate::new(404),
                Some(ResponseTemplate::new(404)),
                "APP",
            )
            .await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "ATELIER_NOT_FOUND", "{v}");
            assert_eq!(v["exists"], serde_json::Value::Null, "{v}");
            assert!(
                v["hint"].as_str().unwrap().contains("IRIS_WEB_PREFIX"),
                "{v}"
            );
        });
    }

    // The guard that keeps (e) from swallowing the common case — a confirmed namespace still
    // answers `exists:false` — is `head_keeps_exists_false_when_the_namespace_is_confirmed_present`
    // above, unchanged by any of this. That is the point: only the arm with no evidence moved.

    /// #102 P1 for get: a missing NAMESPACE was reported as a missing DOCUMENT.
    #[test]
    fn get_names_the_namespace_but_keeps_not_found_for_a_real_document_miss() {
        rt().block_on(async {
            let (_, v) = run("GET", ResponseTemplate::new(404), Some(ok_root()), "APP").await;
            assert_eq!(v["error_code"], "NOT_FOUND", "the namespace is there: {v}");
            assert!(
                v["error"].as_str().unwrap().contains("Ens.Director.cls"),
                "{v}"
            );

            let (_, v) = run(
                "GET",
                ResponseTemplate::new(404),
                Some(ok_root()),
                "ZZNOSUCHNS",
            )
            .await;
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert!(
                v["hint"].as_str().unwrap().contains("Nothing was read"),
                "{v}"
            );
        });
    }

    /// The same for delete — and its effect line says nothing was DELETED, not compiled.
    #[test]
    fn delete_names_the_namespace_but_keeps_not_found_for_a_real_document_miss() {
        rt().block_on(async {
            let (_, v) = run("DELETE", ResponseTemplate::new(404), Some(ok_root()), "APP").await;
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");

            let (_, v) = run(
                "DELETE",
                ResponseTemplate::new(404),
                Some(ok_root()),
                "ZZNOSUCHNS",
            )
            .await;
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert!(
                v["hint"].as_str().unwrap().contains("Nothing was deleted"),
                "the hint must not claim a compile that never happened: {v}"
            );
        });
    }

    // ── #102 P1, the BATCH arm of get ────────────────────────────────────────
    //
    // The single-name path above was fixed while `names: [...]`, twenty lines higher in the
    // same function, stayed byte-identical to the baseline. It answered
    // {"documents":[{"error":"HTTP 404 Not Found"},…],"success":true} — isError:false — for a
    // namespace that does not exist, which is worse than the bug the issue described: the
    // envelope claimed the call worked.

    /// Run batch get for `names` against a `/doc/...` mock and the given root descriptor.
    async fn run_batch(
        doc: ResponseTemplate,
        root: Option<ResponseTemplate>,
        ns: &str,
        names: &[&str],
    ) -> (Option<bool>, serde_json::Value) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r".*/doc/.*"))
            .respond_with(doc)
            .mount(&server)
            .await;
        if let Some(root) = root {
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(root)
                .mount(&server)
                .await;
        }
        let iris = IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let p: IrisDocParams = serde_json::from_value(serde_json::json!({
            "mode": "get", "names": names, "namespace": ns
        }))
        .unwrap();
        let r = handle_get(&iris, &reqwest::Client::new(), p)
            .await
            .expect("the tool must answer, not error out of the transport");
        (r.is_error, payload(&r))
    }

    /// The filed shape: every document 404s because the NAMESPACE is missing, and the
    /// envelope said success while never mentioning the namespace.
    #[test]
    fn a_batch_get_in_a_missing_namespace_names_the_namespace_and_fails() {
        rt().block_on(async {
            let (is_err, v) = run_batch(
                ResponseTemplate::new(404),
                Some(ok_root()),
                "ZZNOSUCHNS",
                &["%Library.String.cls", "%Library.Integer.cls"],
            )
            .await;
            assert_eq!(
                is_err,
                Some(true),
                "nothing was read — that is a failure: {v}"
            );
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert_eq!(v["success"], false, "{v}");
            assert!(
                v["available_namespaces"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                "#93 asks for the namespaces that DO exist: {v}"
            );
        });
    }

    /// A 401 across the batch is credentials, not a pile of missing documents — and it must
    /// carry the same code and hint the single-name path gives (#101).
    #[test]
    fn a_batch_get_rejected_by_credentials_says_so() {
        rt().block_on(async {
            let (is_err, v) = run_batch(
                ResponseTemplate::new(401),
                Some(ok_root()),
                "APP",
                &["A.cls", "B.cls"],
            )
            .await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
            // The per-document detail survives, and each entry is coded too.
            assert_eq!(v["documents"][0]["error_code"], "IRIS_AUTH_FAILED", "{v}");
        });
    }

    /// A batch where nothing was read but the statuses disagree speaks with the most
    /// ACTIONABLE one. A 401 mixed with a 404 is a call that was never allowed to run — a
    /// numerically-largest rule would have summarised it as NOT_FOUND and sent the caller
    /// looking for documents instead of at the password.
    #[test]
    fn a_mixed_status_batch_leads_with_the_credentials() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/LOCKED\.cls$"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP"])))
                .mount(&server)
                .await;
            let iris = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let p: IrisDocParams = serde_json::from_value(serde_json::json!({
                "mode": "get", "names": ["GONE.cls", "LOCKED.cls"], "namespace": "APP"
            }))
            .unwrap();
            let r = handle_get(&iris, &reqwest::Client::new(), p).await.unwrap();
            let v = payload(&r);
            assert_eq!(r.is_error, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            // Both per-document verdicts still survive intact.
            assert_eq!(v["documents"][0]["error_code"], "NOT_FOUND", "{v}");
            assert_eq!(v["documents"][1]["error_code"], "IRIS_AUTH_FAILED", "{v}");
        });
    }

    /// The guard: a batch where SOMETHING was read is still a success, and the per-document
    /// errors ride along exactly as before — only now they are coded and the namespace is
    /// named.
    #[test]
    fn a_batch_get_that_read_something_is_still_a_success() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/GOOD\.cls$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"result": {"content": ["Class GOOD {", "}"]}}),
                ))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            let iris = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let p: IrisDocParams = serde_json::from_value(serde_json::json!({
                "mode": "get", "names": ["GOOD.cls", "MISSING.cls"], "namespace": "APP"
            }))
            .unwrap();
            let r = handle_get(&iris, &reqwest::Client::new(), p).await.unwrap();
            let v = payload(&r);
            assert_ne!(r.is_error, Some(true), "{v}");
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["namespace"], "APP", "{v}");
            assert!(v["documents"][0]["content"].is_string(), "{v}");
            assert_eq!(v["documents"][1]["error_code"], "NOT_FOUND", "{v}");
        });
    }
}

#[cfg(test)]
mod line_edit_mode_tests {
    //! #24: `insert_lines` / `delete_lines` driven through the REAL handler with wiremock, not through
    //! the pure helpers — those are covered in `line_edit`. What is only testable here is the wiring:
    //! that the read is complete, that the bytes PUT back are the edited document, and that a refusal
    //! writes nothing at all.
    use super::*;
    use crate::iris::connection::DiscoverySource;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// LF, because that is what this transport can actually produce.
    ///
    /// MEASURED, after a first attempt built a CRLF fixture and asserted the CRs survived a write:
    /// Atelier is LINE-BASED IN BOTH DIRECTIONS. `doc_content_to_string` joins the returned lines with
    /// LF, and `do_write` sends `content.lines()` — terminators stripped. So a document read through
    /// this path never carries a CR, and one written back cannot either. Line endings are the
    /// transport's business, not this feature's.
    ///
    /// `line_edit::split_lines` is still CR-safe, and `the_split_is_cr_safe_even_though_the_transport_is_not`
    /// in that module covers it — but asserting CRLF survives an `iris_doc` write would be asserting
    /// something the write path deliberately does not do.
    fn doc() -> String {
        let mut d = [
            "Class Demo.T Extends %RegisteredObject",
            "{",
            "Method A()",
            "{",
            "}",
            "}",
        ]
        .join("\n");
        d.push('\n');
        d
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn params(mode: &str) -> IrisDocParams {
        serde_json::from_value(serde_json::json!({
            "mode": mode, "name": "Demo.T.cls", "namespace": "APP"
        }))
        .unwrap()
    }

    /// GET returns `DOC`; PUT and the compile succeed. Returns the handler payload AND every PUT body
    /// the server received, so a test can assert on the BYTES that were written — the only way to know
    /// the document was not corrupted.
    async fn run(p: IrisDocParams) -> (serde_json::Value, Vec<String>) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r".*/doc/.*"))
            // Atelier returns result.content as a FLAT array of line strings — see
            // doc_content_to_string. Wrapping it as content[0].content (my first attempt) makes
            // filter_map(as_str) drop everything, so the document parses as ZERO lines and every edit
            // is refused as out of range. The tests caught the harness, which is the right order.
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": {"content": doc().split('\n').collect::<Vec<_>>()}
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r".*/doc/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/compile.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": {"errors": []}, "console": []
            })))
            .mount(&server)
            .await;
        let iris = IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let client = reqwest::Client::new();
        let store = crate::elicitation::ElicitationStore::default();
        let cache = crate::elicitation::CheckoutCache::default();
        let r = handle_iris_doc(&iris, &client, p, &store, &cache)
            .await
            .expect("the tool must answer, not error out of the transport");
        let payload = match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => {
                serde_json::from_str(&t.text).unwrap_or(serde_json::Value::Null)
            }
            _ => serde_json::Value::Null,
        };
        // Every PUT body the mock saw.
        let puts: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            // Only the DOCUMENT put — not write_with_scm's temp IrisDevTmp probe class.
            .filter(|r| r.method.as_str() == "PUT" && r.url.path().ends_with("/Demo.T.cls"))
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        (payload, puts)
    }

    /// The document that gets written back must be the edited one — reconstructed from the `content`
    /// line array the PUT body carries.
    fn written_doc(puts: &[String]) -> String {
        // There are TWO PUTs: write_with_scm first PUTs a temp class (IrisDevTmp.Run*.cls) for its
        // source-control probe, then PUTs the document. Selecting by path rather than assuming one PUT —
        // asserting `len == 1` failed here and the cause was the SCM probe, not the edit.
        assert_eq!(puts.len(), 1, "expected one DOCUMENT PUT, got: {puts:?}");
        let v: serde_json::Value = serde_json::from_str(&puts[0]).expect("PUT body is JSON");
        v["content"]
            .as_array()
            .expect("content array")
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// FIXTURE CONTROL. Six lines and a trailing newline — if the fixture drifted, every count
    /// assertion below would be measuring something else. It also pins that the fixture carries NO CR,
    /// which is what this transport really produces; a CRLF fixture here would test a case that cannot
    /// occur and did, on the first attempt, assert something the write path does not do.
    #[test]
    fn the_fixture_matches_what_atelier_can_actually_return() {
        let d = doc();
        assert!(!d.contains('\r'), "Atelier joins lines with LF: {d:?}");
        let (lines, trailing) = crate::tools::line_edit::split_lines(&d);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(trailing, "a .cls ends with a newline");
    }

    /// The transport NORMALISES terminators, and that is worth pinning so the next reader does not
    /// reintroduce a CRLF expectation: whatever is written, `do_write` sends `content.lines()`, so the
    /// PUT body is an array of terminator-free lines.
    #[test]
    fn the_put_body_is_an_array_of_terminator_free_lines() {
        rt().block_on(async {
            let mut p = params("insert_lines");
            p.at = Some(1);
            p.lines = Some(vec!["// top".into()]);
            let (_v, puts) = run(p).await;
            let body: serde_json::Value = serde_json::from_str(&puts[0]).expect("JSON");
            let arr = body["content"].as_array().expect("content array");
            assert!(!arr.is_empty());
            for l in arr {
                let t = l.as_str().unwrap_or("");
                assert!(
                    !t.contains('\r') && !t.contains('\n'),
                    "line carries a terminator: {t:?}"
                );
            }
        });
    }

    /// A FAILED READ must return the read's own envelope and write NOTHING.
    ///
    /// A MUTATION SURVIVED without this: deleting the `success != true` check passed, because every mock
    /// made the read succeed. The handler would then have edited whatever `content` an error payload
    /// happened to carry — most likely nothing — and PUT it, replacing the document with an empty one.
    #[test]
    fn a_failed_read_returns_its_own_envelope_and_writes_nothing() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;
            let iris = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let client = reqwest::Client::new();
            let store = crate::elicitation::ElicitationStore::default();
            let cache = crate::elicitation::CheckoutCache::default();
            let mut p = params("insert_lines");
            p.at = Some(1);
            p.lines = Some(vec!["x".into()]);
            let r = handle_iris_doc(&iris, &client, p, &store, &cache)
                .await
                .unwrap();
            let v = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => {
                    serde_json::from_str::<serde_json::Value>(&t.text).unwrap()
                }
                _ => panic!("text"),
            };
            assert_eq!(v["success"], false, "{v}");
            assert!(
                v["line_edit"].is_null(),
                "a failed read must not report an edit: {v}"
            );
            // The caller must get the READ's own envelope, which names the real cause (the missing
            // document / namespace), NOT this handler's generic fallback. Both refuse and both write
            // nothing, so only the error_code distinguishes them — and a mutation deleting the
            // success check falls through to READ_UNREADABLE and would otherwise pass.
            assert_ne!(
                v["error_code"], "READ_UNREADABLE",
                "the read's own diagnosis must survive, not be replaced by a generic one: {v}"
            );
            let puts = server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|q| q.method.as_str() == "PUT")
                .count();
            assert_eq!(puts, 0, "nothing may be written after a failed read");
        });
    }

    /// A FAILED WRITE must return the write's envelope UNCHANGED — with its compile errors, and with no
    /// `line_edit` summary.
    ///
    /// A MUTATION SURVIVED without this too: removing the `write_result_succeeded` check passed, because
    /// every mock made the compile succeed. Attaching a summary to a failed write tells the caller the
    /// edit landed when it did not, which is worse than a bare failure.
    #[test]
    fn a_failed_write_keeps_its_own_envelope_and_reports_no_edit() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "result": {"content": doc().split('\n').collect::<Vec<_>>()}
                })))
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path_regex(r".*/doc/.*"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;
            // the compile REJECTS the class
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile.*"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "status": {"errors": [{"error": "ERROR #1026: Invalid command"}]},
                    "console": ["ERROR #1026: Invalid command"]
                })))
                .mount(&server)
                .await;
            let iris = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let client = reqwest::Client::new();
            let store = crate::elicitation::ElicitationStore::default();
            let cache = crate::elicitation::CheckoutCache::default();
            let mut p = params("insert_lines");
            p.at = Some(1);
            p.lines = Some(vec!["// oops".into()]);
            p.compile = true;
            let r = handle_iris_doc(&iris, &client, p, &store, &cache)
                .await
                .unwrap();
            let v = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => {
                    serde_json::from_str::<serde_json::Value>(&t.text).unwrap()
                }
                _ => panic!("text"),
            };
            assert_eq!(v["error_code"], "COMPILE_ERROR", "{v}");
            assert!(
                v["line_edit"].is_null(),
                "a failed write must not claim the edit landed: {v}"
            );
            assert!(
                v["compile_errors"].is_array(),
                "the compile diagnostics must survive: {v}"
            );
        });
    }

    /// The truncation guard's LOGIC, tested directly.
    ///
    /// The integration path cannot produce a truncated read — it forces `max_bytes: 0`, and
    /// `a_caller_supplied_max_bytes_cannot_cause_a_truncated_write` keeps that forcing. So a mutation
    /// removing the `if` survives the handler tests and always will; what is testable is the predicate,
    /// and it matters because the guard is what saves the document if the forcing is ever dropped.
    #[test]
    fn the_truncation_predicate_only_fires_on_a_truncated_payload() {
        assert!(read_was_truncated(
            &serde_json::json!({"success": true, "truncated": true})
        ));
        assert!(!read_was_truncated(
            &serde_json::json!({"success": true, "content": "x"})
        ));
        // not a string "true", and not merely present
        assert!(!read_was_truncated(
            &serde_json::json!({"truncated": "true"})
        ));
        assert!(!read_was_truncated(&serde_json::json!({})));
    }

    #[test]
    fn insert_writes_back_the_document_with_the_lines_added() {
        rt().block_on(async {
            let mut p = params("insert_lines");
            p.at = Some(3);
            p.lines = Some(vec!["/// doc".to_string()]);
            let (v, puts) = run(p).await;
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["line_edit"]["lines_before"], 6, "{v}");
            assert_eq!(v["line_edit"]["lines_after"], 7, "{v}");
            assert_eq!(v["line_edit"]["inserted"][0], "/// doc", "{v}");
            let doc = written_doc(&puts);
            assert!(
                doc.contains("/// doc"),
                "the new line must be written: {doc:?}"
            );
            // No CR assertions: the transport strips terminators, see
            // the_put_body_is_an_array_of_terminator_free_lines.
            assert!(doc.contains("Method A()"), "the rest survives: {doc:?}");
        });
    }

    #[test]
    fn delete_writes_back_the_document_with_the_line_gone() {
        rt().block_on(async {
            let mut p = params("delete_lines");
            p.at = Some(3);
            p.expect = Some("Method A()".into());
            let (v, puts) = run(p).await;
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["line_edit"]["removed"][0], "Method A()", "{v}");
            assert_eq!(v["line_edit"]["lines_after"], 5, "{v}");
            let doc = written_doc(&puts);
            assert!(
                !doc.contains("Method A()"),
                "the line must be gone: {doc:?}"
            );
            assert!(
                doc.starts_with("Class Demo.T"),
                "the rest survives: {doc:?}"
            );
        });
    }

    /// THE REFUSAL MUST NOT WRITE. A rejected edit that still PUT something would be the worst outcome
    /// available: the caller is told no, and the document changed anyway.
    #[test]
    fn a_wrong_expect_refuses_and_writes_nothing() {
        rt().block_on(async {
            let mut p = params("delete_lines");
            p.at = Some(3);
            p.expect = Some("Method NOPE()".into());
            let (v, puts) = run(p).await;
            assert_eq!(v["error_code"], "LINE_EXPECT_MISMATCH", "{v}");
            assert!(puts.is_empty(), "a refused edit must PUT nothing: {puts:?}");
        });
    }

    /// Same for delete without `expect`: refused, and nothing written.
    #[test]
    fn delete_without_expect_refuses_and_writes_nothing() {
        rt().block_on(async {
            let mut p = params("delete_lines");
            p.at = Some(3);
            let (v, puts) = run(p).await;
            assert_eq!(v["success"], false, "{v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("requires `expect`"),
                "{v}"
            );
            assert!(puts.is_empty(), "{puts:?}");
        });
    }

    /// An out-of-range `at` is refused before any write, and the message carries the real line count.
    #[test]
    fn an_out_of_range_at_refuses_and_writes_nothing() {
        rt().block_on(async {
            let mut p = params("insert_lines");
            p.at = Some(99);
            p.lines = Some(vec!["x".into()]);
            let (v, puts) = run(p).await;
            assert_eq!(v["success"], false, "{v}");
            assert_eq!(v["lines_total"], 6, "must report the real total: {v}");
            assert!(puts.is_empty(), "{puts:?}");
        });
    }

    /// A missing `at` is refused with no round trip at all — not even the read.
    #[test]
    fn a_missing_at_is_refused_before_reading() {
        rt().block_on(async {
            let p = params("insert_lines");
            let (v, puts) = run(p).await;
            assert_eq!(v["error_code"], "MISSING_PARAMS", "{v}");
            assert!(puts.is_empty(), "{puts:?}");
        });
    }

    /// insert_lines without `lines` is refused — inserting nothing is not an edit.
    #[test]
    fn insert_without_lines_is_refused() {
        rt().block_on(async {
            let mut p = params("insert_lines");
            p.at = Some(1);
            let (v, puts) = run(p).await;
            assert_eq!(v["error_code"], "MISSING_PARAMS", "{v}");
            assert!(puts.is_empty(), "{puts:?}");
        });
    }

    /// The read must be forced COMPLETE. A caller passing max_bytes must not cause a partial read to be
    /// edited and written back — that would truncate the document at the byte cap.
    #[test]
    fn a_caller_supplied_max_bytes_cannot_cause_a_truncated_write() {
        rt().block_on(async {
            let mut p = params("insert_lines");
            p.at = Some(1);
            p.lines = Some(vec!["// top".into()]);
            p.max_bytes = 10; // would slice the document to 10 bytes if it were honoured
            let (v, puts) = run(p).await;
            assert_eq!(v["success"], true, "{v}");
            let doc = written_doc(&puts);
            // the WHOLE document came back, plus the inserted line
            assert!(doc.contains("Method A()"), "tail must survive: {doc:?}");
            assert!(doc.ends_with("}"), "and the last line: {doc:?}");
            assert_eq!(v["line_edit"]["lines_before"], 6, "a full read: {v}");
        });
    }
}

#[cfg(test)]
mod prop_collision_put_tests {
    //! #263 on the tool the report actually used: `iris_doc{mode:put, compile:true}`.
    //!
    //! The first attempt at this fix put the branch in `builtin_hint` and verified it there. That
    //! is invisible to the real defect: `fail_with` lets the payload's own `hint` beat the built-in
    //! one, and this path sets `hint` whenever `note_error_undercount` fires. So the coverage has
    //! to run through `do_write`, which is what this does.
    //!
    //! NOTE: `do_write` writes the VS Code breadcrumb `~/.iris-agentic-dev/open-hint.json` on every
    //! successful PUT, so running these rewrites that one transient file — exactly as a real put
    //! does. Nothing else outside the process is touched.
    use super::*;
    use crate::iris::connection::DiscoverySource;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The verbatim console line from the report.
    const PROP_COLLISION: &str = "ERROR <EnsSearchTable>PropCollision: SearchTable property \
         collision: Property 'PatientFirstName' in class 'HOSPITAL.Search.HL7' cannot override \
         the definition from class 'Hospital.SearchTable.PatientFirstName'";

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// PUT succeeds, then the compile comes back with `console` as given and the first line as the
    /// structured error — the shape `compile_error_list` reads.
    async fn put_then_compile(first_error: &str, console: Vec<&str>) -> serde_json::Value {
        put_compile_and_query(first_error, console, None, None).await
    }

    /// As above, but also answers `/action/query`. #263 proposal 2 issues two of them and they must
    /// be told apart, so each is routed on the SQL text in the request body:
    /// `SearchTableProp` for the registration lookup, `CompiledClass` for the class probe.
    /// `None` leaves that route unmounted, which is how the "the lookup failed" case is produced —
    /// wiremock answers an unmatched request with a 404.
    async fn put_compile_and_query(
        first_error: &str,
        console: Vec<&str>,
        registration_rows: Option<serde_json::Value>,
        class_probe_rows: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let server = MockServer::start().await;
        if let Some(rows) = registration_rows {
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query.*"))
                .and(wiremock::matchers::body_string_contains("SearchTableProp"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"result": {"content": rows}})),
                )
                .mount(&server)
                .await;
        }
        if let Some(rows) = class_probe_rows {
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query.*"))
                .and(wiremock::matchers::body_string_contains("CompiledClass"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"result": {"content": rows}})),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("PUT"))
            .and(path_regex(r".*/doc/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/compile.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": {"errors": [{"error": first_error}]},
                "console": console,
            })))
            .mount(&server)
            .await;
        let iris = IrisConnection::new(
            server.uri(),
            "HOSPITAL",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let client = reqwest::Client::new();
        let r = do_write(
            &iris,
            &client,
            "HOSPITAL.Search.HL7.cls",
            "Class HOSPITAL.Search.HL7 Extends EnsLib.HL7.SearchTable\n{\n}\n",
            "HOSPITAL",
            true,
            false,
        )
        .await
        .expect("the tool must answer, not error out of the transport");
        match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    /// End to end on the reported tool. NOTE: this one passes even without the explicit
    /// `apply_prop_collision_hint` call on this path, because with no undercount nothing sets
    /// `hint` and the envelope's built-in branch supplies it. It is here to pin the OUTCOME the
    /// report asked for; `the_diagnosis_beats_the_undercount_hint_on_the_put_path` is the one that
    /// pins the call.
    #[test]
    fn iris_doc_put_carries_the_prop_collision_diagnosis() {
        rt().block_on(async {
            let v = put_then_compile(PROP_COLLISION, vec![PROP_COLLISION]).await;
            assert_eq!(v["error_code"], "COMPILE_ERROR", "{v}");
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(h.contains("Ens_Config.SearchTableProp"), "{v}");
            assert!(h.contains("%DeleteId"), "{v}");
            assert!(
                !h.contains("cascades of the first"),
                "the generic text must not be what the caller gets: {v}"
            );
        });
    }

    /// The case that defeats a naive fix, and the one that kills the mutation: IRIS counted more
    /// errors than were parsed, so `note_error_undercount` overwrites `hint` AFTER everything else.
    /// The diagnosis must still win, and the incompleteness must survive as a fact and in the text.
    #[test]
    fn the_diagnosis_beats_the_undercount_hint_on_the_put_path() {
        rt().block_on(async {
            let v = put_then_compile(
                PROP_COLLISION,
                vec![
                    PROP_COLLISION,
                    "  > ERROR #5490: Error running generator for method 'IndexDoc'",
                    "Detected 7 errors during compilation in 0.03s.",
                ],
            )
            .await;
            assert_eq!(v["errors_incomplete"], true, "precondition — {v}");
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(
                h.contains("Ens_Config.SearchTableProp"),
                "the diagnosis must beat the undercount text: {v}"
            );
            assert!(
                h.contains("INCOMPLETE"),
                "and must not silently swallow the undercount: {v}"
            );
        });
    }

    // ── #263 proposal 2: the answer on the payload ──────────────────────────────────────────

    /// The whole point: the delete argument arrives with the failure, and the hint names it, so the
    /// fix is one `%DeleteId` with no diagnostic query.
    #[test]
    fn the_stale_registration_is_looked_up_and_the_hint_names_the_delete_id() {
        rt().block_on(async {
            let v = put_compile_and_query(
                PROP_COLLISION,
                vec![PROP_COLLISION],
                Some(serde_json::json!([{
                    "ID": "EnsLib.HL7.SearchTable||PatientFirstName",
                    "Name": "PatientFirstName",
                    "PropId": 5,
                    "ClassExtent": "EnsLib.HL7.SearchTable",
                    "ClassDerivation": "Hospital.SearchTable.PatientFirstName~EnsLib.HL7.SearchTable",
                }])),
                // the accused class probe comes back EMPTY — the class really is gone
                Some(serde_json::json!([])),
            )
            .await;
            let sr = &v["stale_registration"];
            assert_eq!(
                sr["delete_id"], "EnsLib.HL7.SearchTable||PatientFirstName",
                "{v}"
            );
            assert_eq!(sr["prop"], "PatientFirstName", "{v}");
            assert_eq!(
                sr["accused_class"], "Hospital.SearchTable.PatientFirstName",
                "{v}"
            );
            assert_eq!(
                sr["accused_class_exists"], false,
                "an empty dictionary probe means the accused class is gone: {v}"
            );
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(
                h.contains("ALREADY ON THIS PAYLOAD"),
                "the hint must stop prescribing a SELECT once the answer is present: {v}"
            );
            assert!(
                h.contains("EnsLib.HL7.SearchTable||PatientFirstName"),
                "the hint must name the actual id: {v}"
            );
        });
    }

    /// The honest-silence case. With no `/action/query` route mounted the lookup gets a 404, and a
    /// broken query must NOT be reported as "no stale registration" — that would read as a clean
    /// answer. The field is absent and the hint falls back to prescribing the SELECT.
    #[test]
    fn a_failed_lookup_attaches_nothing_and_keeps_the_select_in_the_hint() {
        rt().block_on(async {
            let v = put_compile_and_query(PROP_COLLISION, vec![PROP_COLLISION], None, None).await;
            assert!(
                v["stale_registration"].is_null(),
                "a failed lookup must not invent an answer: {v}"
            );
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(
                h.contains("Ens_Config.SearchTableProp"),
                "the hint is still correct and still actionable: {v}"
            );
            assert!(
                !h.contains("ALREADY ON THIS PAYLOAD"),
                "must not claim an answer it does not have: {v}"
            );
        });
    }

    /// Found by a SURVIVING MUTATION: changing `Undetermined => None` to `Undetermined => Some(false)`
    /// in `enrich` passed every test, because the only assertion on that mapping went through
    /// `build` with a hand-written `None`. The real path was unasserted — so a broken class probe
    /// could have reported `accused_class_exists: false`, i.e. "that class is gone", on the strength
    /// of a failed query. The registration route is mounted and the class probe is NOT, which is
    /// what makes `class_presence` return `Undetermined`.
    #[test]
    fn an_unanswerable_class_probe_omits_the_existence_claim_on_the_real_path() {
        rt().block_on(async {
            let v = put_compile_and_query(
                PROP_COLLISION,
                vec![PROP_COLLISION],
                Some(serde_json::json!([{
                    "ID": "EnsLib.HL7.SearchTable||PatientFirstName",
                    "Name": "PatientFirstName",
                    "PropId": 5,
                    "ClassExtent": "EnsLib.HL7.SearchTable",
                    "ClassDerivation": "Hospital.SearchTable.PatientFirstName~EnsLib.HL7.SearchTable",
                }])),
                None,
            )
            .await;
            let sr = &v["stale_registration"];
            // the lookup itself still succeeded, so the useful part is present
            assert_eq!(
                sr["delete_id"], "EnsLib.HL7.SearchTable||PatientFirstName",
                "{v}"
            );
            // but nothing is claimed about the accused class
            assert!(
                sr.get("accused_class_exists").is_none(),
                "an undetermined probe must not become a false: {v}"
            );
        });
    }

    /// Zero rows is a different real answer, and the hint must not then promise a delete id.
    #[test]
    fn no_registered_row_is_reported_as_a_live_collision_not_a_stale_one() {
        rt().block_on(async {
            let v = put_compile_and_query(
                PROP_COLLISION,
                vec![PROP_COLLISION],
                Some(serde_json::json!([])),
                Some(serde_json::json!([{"IsCompiled": 1}])),
            )
            .await;
            let sr = &v["stale_registration"];
            assert!(sr["delete_id"].is_null(), "{v}");
            assert!(
                sr["note"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("NOT a stale"),
                "{v}"
            );
            assert_eq!(
                sr["accused_class_exists"], true,
                "the probe found the class compiled: {v}"
            );
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(!h.contains("ALREADY ON THIS PAYLOAD"), "{v}");
        });
    }

    /// POSITIVE CONTROL on this path: an ordinary compile error is untouched.
    #[test]
    fn an_ordinary_put_compile_error_keeps_the_generic_hint() {
        rt().block_on(async {
            let v = put_then_compile(
                "ERROR #1026: Invalid command",
                vec!["ERROR #1026: Invalid command"],
            )
            .await;
            let h = v["hint"].as_str().unwrap_or_default();
            assert!(!h.contains("SearchTableProp"), "{v}");
            assert!(h.contains("cascades of the first"), "{v}");
        });
    }
}

// #331: `storage_strip_message_tests` lived here — eight tests pinning the properties of the
// STORAGE_STRIP_BLOCKED message (the FIX before the bypass; the bypass unnamed when the class is
// absent). Removed with the message they assert on. They were good tests of a message that should no
// longer exist, and the guard that replaces them is
// `the_5559_hint_never_advises_deleting_a_storage_block` in mod.rs — which asserts the destructive
// INSTRUCTION appears nowhere, rather than that it is well worded.

#[cfg(test)]
mod doc_mode_single_source_tests {
    //! `DocMode` is the only definition of the mode set. These tests pin the three things that used
    //! to be written out separately: what parses, what is advertised, and what the write gate treats
    //! as a write.
    use super::*;

    /// Every variant, listed ONCE for the tests.
    ///
    /// The `match` below is what makes this list impossible to leave stale: it has no wildcard, so
    /// adding a `DocMode` variant is a COMPILE error here until it is named. That is the guard that a
    /// hand-written count or a `ALL.len() == 4` assertion cannot give — those go stale silently.
    fn every_variant() -> Vec<DocMode> {
        let exhaustiveness_probe = DocMode::Get;
        match exhaustiveness_probe {
            DocMode::Get
            | DocMode::Put
            | DocMode::Delete
            | DocMode::Head
            | DocMode::InsertLines
            | DocMode::DeleteLines => {}
        }
        vec![
            DocMode::Get,
            DocMode::Put,
            DocMode::Delete,
            DocMode::Head,
            DocMode::InsertLines,
            DocMode::DeleteLines,
        ]
    }

    /// `ALL` must contain every variant. If one is missing, `parse` can never produce it, so its
    /// dispatch arm is dead code and the mode is silently unreachable.
    #[test]
    fn all_contains_every_variant() {
        for v in every_variant() {
            assert!(
                DocMode::ALL.contains(&v),
                "{v:?} is missing from DocMode::ALL — parse() can never return it, so its dispatch \
                 arm is unreachable"
            );
        }
        assert_eq!(
            DocMode::ALL.len(),
            every_variant().len(),
            "ALL has an entry that is not a real variant, or a duplicate"
        );
    }

    #[test]
    fn every_variant_round_trips_through_its_wire_spelling() {
        for v in every_variant() {
            assert_eq!(DocMode::parse(v.as_str()), Some(v), "{v:?}");
        }
    }

    /// Parsing stays case-insensitive — the behaviour before `parse` was rewritten to derive from
    /// `ALL`. A caller sending `PUT` must still write.
    #[test]
    fn parsing_is_still_case_insensitive() {
        assert_eq!(DocMode::parse("PUT"), Some(DocMode::Put));
        assert_eq!(DocMode::parse("Delete"), Some(DocMode::Delete));
        assert_eq!(DocMode::parse("HeAd"), Some(DocMode::Head));
        assert_eq!(DocMode::parse("nonsense"), None);
    }

    /// The advertised list must name every mode, so the unknown-mode error cannot omit one that
    /// actually works.
    #[test]
    fn the_advertised_list_names_every_variant() {
        let advertised = DocMode::valid_values();
        for v in every_variant() {
            assert!(
                advertised.contains(v.as_str()),
                "{} is dispatchable but not advertised in {advertised:?}",
                v.as_str()
            );
        }
    }

    /// The expected verdict for every mode, stated as DATA and independently of `is_write`.
    ///
    /// A MUTATION SURVIVED without this. The gate now DERIVES from `is_write`, which is right — but it
    /// makes "the gate agrees with is_write" TAUTOLOGICAL: a mode dispatched to a write handler and
    /// classified as a read has both sides reading the same wrong answer, and they agree. The risk
    /// moved from "two copies disagree" to "the one copy is wrong", and only an independent statement
    /// of the verdict catches that.
    ///
    /// Same shape as `every_interop_tool_is_classified` for tools: a second, deliberate assertion so a
    /// new entry cannot inherit a default.
    const EXPECTED_WRITE: &[(&str, bool)] = &[
        ("get", false),
        ("head", false),
        ("put", true),
        ("delete", true),
        // #24: a positional edit reads the document, rewrites it and PUTs it back — as much a write as
        // `put`, and `delete_lines` destroys content. Stated here deliberately: the compile error from
        // the exhaustiveness probe above is what forced this decision rather than letting the new modes
        // inherit whatever `is_write` happened to say.
        ("insert_lines", true),
        ("delete_lines", true),
    ];

    /// Every mode must appear in `EXPECTED_WRITE`. A new mode FAILS here, by name, until someone
    /// states whether it writes — rather than silently taking whatever `is_write` happens to say.
    #[test]
    fn every_mode_has_an_independently_stated_verdict() {
        for v in every_variant() {
            let stated = EXPECTED_WRITE
                .iter()
                .find(|(name, _)| *name == v.as_str())
                .map(|(_, w)| *w);
            let Some(stated) = stated else {
                panic!(
                    "mode '{}' has no entry in EXPECTED_WRITE. Add one saying whether it WRITES — \
                     deliberately, because the write gate derives from is_write() and cannot \
                     second-guess it. If it writes, a missing entry means an ungated write.",
                    v.as_str()
                );
            };
            assert_eq!(
                v.is_write(),
                stated,
                "mode '{}': is_write() says {} but EXPECTED_WRITE says {}",
                v.as_str(),
                v.is_write(),
                stated
            );
        }
        assert_eq!(
            EXPECTED_WRITE.len(),
            every_variant().len(),
            "EXPECTED_WRITE names a mode that does not exist, or duplicates one"
        );
    }

    /// CONTROL: both polarities occur. If `is_write` were stuck on or off, the per-variant test above
    /// would catch it — but this says so directly, so a future all-write or all-read enum is visible.
    #[test]
    fn both_polarities_exist() {
        let writes = every_variant().iter().filter(|m| m.is_write()).count();
        assert!(writes > 0, "no write modes at all?");
        assert!(writes < every_variant().len(), "every mode is a write?");
    }
}
