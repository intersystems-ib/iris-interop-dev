//! iris_info — namespace/document discovery via Atelier REST.
//! iris_macro — macro introspection.
//! iris_debug — debug tools via Atelier xecute + SQL.
//! iris_generate — LLM-based class/test generation.

use crate::iris::connection::IrisConnection;
use crate::objectscript::os_str_expr;
use crate::tools::log_store;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};

fn ok_json(v: serde_json::Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    Ok(rmcp::model::CallToolResult::success(vec![
        rmcp::model::Content::text(v.to_string()),
    ]))
}
fn err_json(code: &str, msg: &str) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    crate::tools::envelope::fail(code, msg)
}
fn default_limit() -> usize {
    20
}

// ── iris_info ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InfoParams {
    /// What to fetch: documents, modified, namespace, metadata, jobs, csp_apps, csp_debug, sa_schema
    pub what: String,
    /// Document type filter for what=documents: CLS, MAC, INT, INC, CSP, ALL
    pub doc_type: Option<String>,
    /// Schema/cube name for what=sa_schema
    pub name: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// If true, bypass the log store and return all results inline regardless of count.
    #[serde(default)]
    pub inline: bool,
}

pub async fn handle_iris_info(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: InfoParams,
    log_store: Arc<Mutex<log_store::LogStore>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let ns = &namespace;
    let url = match p.what.as_str() {
        "documents" => {
            // Bug 14: use versioned_ns_url so future API versions are used automatically.
            let cat = match p.doc_type.as_deref().unwrap_or("ALL") {
                "ALL" => "CLS".to_string(),
                t => t.to_uppercase(),
            };
            iris.versioned_ns_url(ns, &format!("/docnames/{}", cat))
        }
        "modified" => iris.versioned_ns_url(ns, "/modified/0"),
        "namespace" => iris.versioned_ns_url(ns, ""), // namespace metadata endpoint
        "metadata" => iris.atelier_url("/"), // root endpoint returns server metadata
        "jobs" => iris.versioned_ns_url(ns, "/jobs"),
        "csp_apps" => iris.versioned_ns_url(ns, "/cspapps"),
        "csp_debug" => iris.versioned_ns_url(ns, "/cspdebugid"),
        "sa_schema" => {
            let name = p.name.as_deref().unwrap_or("");
            iris.versioned_ns_url(ns, &format!("/saschema/{}", urlencoding::encode(name)))
        }
        other => return err_json("INVALID_PARAM", &format!("Unknown what='{}'. Use: documents, modified, namespace, metadata, jobs, csp_apps, csp_debug, sa_schema", other)),
    };

    let resp = match client
        .get(&url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_iris_info", &e.to_string())
        }
    };

    if !resp.status().is_success() {
        // #108: a 404 here had two very different causes and one bare answer. A wrong
        // IRIS_WEB_PREFIX and a nonexistent namespace both produced `NOT_FOUND` with no
        // hint — iris_info was the last Atelier-backed tool not reaching the shared
        // classifier, which names the prefix in the first case and lists the accessible
        // namespaces in the second. `Undetermined` means the root probe could not tell,
        // and the status-coded error below still stands.
        if resp.status().as_u16() == 404 {
            if let crate::tools::interop::FourOhFour::Explained(e) =
                crate::tools::interop::classify_404(
                    iris,
                    client,
                    ns,
                    &url,
                    &format!("Nothing was read for what='{}'.", p.what),
                )
                .await
            {
                return e;
            }
        }
        // #101: IRIS answered — it is reachable by definition. This said IRIS_UNREACHABLE for
        // every status, so a wrong password sent the caller to debug networking.
        return crate::tools::envelope::http_status_fail("iris_info", resp.status(), &url);
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let mut result_json = serde_json::json!({"success": true, "what": p.what, "namespace": namespace, "result": body["result"]});

    // Progressive disclosure (027): for what=documents, truncate the document list.
    // The document names are in result["content"] — flatten to a top-level "documents" key.
    if p.what == "documents" {
        if let Some(content) = result_json["result"]["content"].as_array().cloned() {
            result_json["documents"] = serde_json::Value::Array(content);
            let threshold = log_store::read_inline_threshold("IRIS_INLINE_INFO", 30);
            log_store::apply_truncation(
                &mut result_json,
                "documents",
                threshold,
                p.inline,
                &log_store,
                "iris_info",
            );
        }
    }

    ok_json(result_json)
}

// ── iris_macro ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MacroParams {
    /// Action: list, signature, location, definition, expand
    pub action: String,
    pub name: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn handle_iris_macro(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: MacroParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    match p.action.as_str() {
        "list" => {
            // Bug 14: use versioned_ns_url instead of hardcoded /v1/.
            // `INC` is not an Atelier document CATEGORY — the categories are CLS / RTN / CSP /
            // OTH, and .inc files live under RTN. `/docnames/INC` answers HTTP 400 Bad Request
            // on every instance (verified live on IRIS 2026.1 Build 235U), so this listing has
            // never once worked. Nobody noticed because the non-2xx arm below swallowed it into
            // `success:true, macros:[], "No include files found in this namespace"` — the very
            // #102 P0 lie. Unmasking the status is what made the broken URL visible.
            let url = iris.versioned_ns_url(&namespace, "/docnames/RTN");
            let resp = match client
                .get(&url)
                .basic_auth(&iris.username, Some(&iris.password))
                .send()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_iris_macro",
                        &e.to_string(),
                    )
                }
            };
            // #102 P0: EVERY non-2xx used to become `success:true, macros:[], "No include
            // files found in this namespace"` — a confident negative FACT produced by a call
            // that never succeeded. Verified live with a wrong password against a namespace
            // that exists and is reachable: HTTP 401, and the tool said there were no include
            // files. A 2xx with an empty content array still answers macros:[]; that is the
            // legitimate negative and it is unchanged.
            if !resp.status().is_success() {
                let status = resp.status();
                if status.as_u16() == 404 {
                    if let Some(missing) = crate::tools::interop::namespace_missing_error(
                        iris,
                        client,
                        &namespace,
                        &url,
                        "No macros were listed.",
                    )
                    .await
                    {
                        return missing;
                    }
                }
                return crate::tools::envelope::http_status_fail("iris_macro", status, &url);
            }
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // RTN carries .mac / .int / .inc / .bas together, and each element is an object
            // (`{"cat":"RTN","name":"%apiCBIND.inc",...}`), not a bare string — the old
            // `as_str()` would have dropped every name even if the URL had been right.
            let inc_files: Vec<String> = body["result"]["content"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v["name"].as_str().or_else(|| v.as_str()))
                .filter(|n| n.to_ascii_lowercase().ends_with(".inc"))
                .map(|n| n.to_string())
                .collect();
            ok_json(serde_json::json!({
                "success": true,
                "macros": inc_files,
                "note": "Lists .inc include files — macro definitions are found within these files"
            }))
        }
        action @ ("signature" | "location" | "definition" | "expand") => {
            let name = p.name.as_deref().unwrap_or("");
            let url = iris.versioned_ns_url(&namespace, "/action/getmacro");
            let arg_count = p.args.len();
            let resp= match client .post(&url) .basic_auth(&iris.username, Some(&iris.password)) .json(&serde_json::json!({ "macros": [{"name": name, "arguments": arg_count}], "action": action, "args": p.args, })) .send() .await {
                Ok(v) => v,
                Err(e) => return crate::tools::envelope::transport_fail("handle_iris_macro", &e.to_string()),
            };
            // Same #106 shape: a refused getmacro POST used to answer `success:true` with a
            // null result, i.e. "that macro does not exist" for a call IRIS never ran.
            if !resp.status().is_success() {
                return crate::tools::envelope::http_status_fail("iris_macro", resp.status(), &url);
            }
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            ok_json(
                serde_json::json!({"success": true, "name": name, "action": action, "result": body["result"]}),
            )
        }
        other => err_json(
            "INVALID_PARAM",
            &format!(
                "Unknown action='{}'. Use: list, signature, location, definition, expand",
                other
            ),
        ),
    }
}

// ── iris_debug ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DebugParams {
    /// Action: map_int, error_logs, capture, source_map
    // #112: the valid set was in the description prose only, so the schema said "any
    // string". Caught by `every_tool_advertises_the_parameters_it_reads` — iris_debug was
    // the tool held up as the example of a well-described dispatcher, and it had the same
    // gap one level down.
    #[schemars(extend("enum" = ["map_int", "error_logs", "capture", "source_map"]))]
    pub action: String,
    /// Error string for map_int e.g. "<UNDEFINED>x+3^MyApp.Foo.1"
    pub error_string: Option<String>,
    /// Class name for source_map
    pub class_name: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn handle_iris_debug(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: DebugParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));

    match p.action.as_str() {
        "map_int" => {
            let err = p.error_string.as_deref().unwrap_or("");
            let code = format!(
                "set err={} set routine=$piece($piece(err,\"^\",2),\".\",1) set offset=$piece(err,\"+\",2) set offset=$piece(offset,\"^\",1) write ##class(%Studio.Debugger).SourceLine(routine,+offset)",
                os_str_expr(err)
            );
            // execute_via_generator works over plain Atelier HTTP — no docker exec
            // needed (issue #20; the DOCKER_REQUIRED bail-out made iris_debug fail on
            // every HTTP-only connection).
            match iris.execute_via_generator(&code, &namespace, client).await {
                Ok(output) => ok_json(
                    serde_json::json!({"success": true, "error_string": err, "source_location": output.trim()}),
                ),
                Err(e) => err_json("EXECUTION_FAILED", &e.to_string()),
            }
        }
        "error_logs" => {
            // IRIS error log tables (%SYSTEM.Error, %SYS.ErrorLog) are not SQL-accessible
            // via Atelier REST in IRIS Community edition.
            // Return empty list with a clear note rather than null.
            ok_json(serde_json::json!({
                "success": true,
                "logs": [],
                "note": "IRIS error log is not accessible via Atelier REST SQL. Set IRIS_CONTAINER to enable docker exec access to the full error log."
            }))
        }
        "capture" => {
            let code = "set err=$ZERROR write \"error:\"_err,! set loc=$ZPOSITION write \"position:\"_loc,!";
            match iris.execute_via_generator(code, &namespace, client).await {
                Ok(output) => {
                    ok_json(serde_json::json!({"success": true, "capture": output.trim()}))
                }
                Err(e) => err_json("EXECUTION_FAILED", &e.to_string()),
            }
        }
        "source_map" => {
            let cls = p.class_name.as_deref().unwrap_or("");
            let code = format!(
                "set map=\"\" set line=1 do {{set int=##class(%Studio.Debugger).MapToINT({cls},line,.intline) if int=\"\" quit set map=map_line_\"->\"_intline_\",\" set line=line+1 }} while 1 write map",
                cls = crate::objectscript::os_str_expr(cls)
            );
            match iris.execute_via_generator(&code, &namespace, client).await {
                Ok(output) => ok_json(
                    serde_json::json!({"success": true, "class": cls, "mapping": output.trim()}),
                ),
                Err(e) => err_json("EXECUTION_FAILED", &e.to_string()),
            }
        }
        other => err_json(
            "INVALID_PARAM",
            &format!(
                "Unknown action='{}'. Use: map_int, error_logs, capture, source_map",
                other
            ),
        ),
    }
}

// ── iris_generate ─────────────────────────────────────────────────────────────
//
// Context-provider design: returns everything the calling AI agent needs to
// write the class itself. No API key, no server-side LLM call, works with
// Copilot, Claude Code, or any MCP client.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateParams {
    /// What to generate — natural language description, e.g. "a Patient class with Name and DOB properties"
    pub description: String,
    /// Type: "class" (default) or "test"
    #[serde(default = "default_type")]
    pub gen_type: String,
    /// Existing class name to generate tests for (gen_type=test only)
    pub class_name: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

fn default_type() -> String {
    "class".to_string()
}

/// The outcome of reading an Atelier `/action/query` response inside this module.
///
/// #106: `iris_generate` parsed its context queries with `.unwrap_or_default()` and never
/// looked at the status. A 401 therefore became `existing_classes: []`, and the tool went
/// on to instruct the model *"Use package prefix 'MyApp' to match existing classes in this
/// namespace"* — advice derived entirely from a request IRIS had rejected. That is worse
/// than a wrong error code: it launders an auth failure into a confident instruction to a
/// model. An empty namespace and a refused request are different facts and must not share
/// a response shape.
enum QueryRead {
    Body(serde_json::Value),
    /// Already-rendered refusal envelope — the caller returns it unchanged.
    Failed(Result<rmcp::model::CallToolResult, rmcp::ErrorData>),
}

/// Read a `/action/query` response through the SAME interpreter `query_once` uses (#105),
/// so a refusal cannot be mistaken for an empty answer here either.
async fn read_query_context(tool: &str, resp: reqwest::Response, url: &str) -> QueryRead {
    use crate::iris::connection::{interpret_query_response, QueryOutcome};
    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return QueryRead::Failed(crate::tools::envelope::transport_fail(tool, &e.to_string()))
        }
    };
    match interpret_query_response(status, &text) {
        QueryOutcome::Rows(body) => QueryRead::Body(body),
        QueryOutcome::IrisError(msg) => QueryRead::Failed(crate::tools::envelope::fail_with(
            "SQL_ERROR",
            &msg,
            serde_json::json!({
                "attempted_url": url,
                "hint": "IRIS rejected the introspection query this tool runs to gather \
                         context. No context was gathered; nothing was generated.",
            }),
        )),
        QueryOutcome::HttpError { status, .. } => {
            QueryRead::Failed(crate::tools::envelope::http_status_fail(tool, status, url))
        }
        QueryOutcome::NonJson { snippet, .. } => {
            QueryRead::Failed(crate::tools::envelope::fail_with(
                "IRIS_REQUEST_FAILED",
                &format!("non-JSON response from {url}: {snippet}"),
                serde_json::json!({
                    "attempted_url": url,
                    "http_status": status.as_u16(),
                    "hint": "IRIS answered, but not with JSON — typically a proxy error page or \
                             an HTML login redirect in front of the Atelier API. No context was \
                             gathered; this is not an empty namespace.",
                }),
            ))
        }
    }
}

pub async fn handle_iris_generate(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: GenerateParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let ns = &namespace;
    let query_url = iris.versioned_ns_url(ns, "/action/query");

    match p.gen_type.as_str() {
        "test" => {
            let cls = p.class_name.as_deref().unwrap_or("");

            // Fetch the class's methods and properties as generation context
            let sql = format!(
                "SELECT Name, FormalSpec, ReturnType, Description \
                 FROM %Dictionary.CompiledMethod WHERE parent = '{}' ORDER BY Name",
                cls.replace('\'', "''")
            );
            let resp = match client
                .post(&query_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&serde_json::json!({"query": sql}))
                .send()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_iris_generate",
                        &e.to_string(),
                    )
                }
            };
            let body = match read_query_context("iris_generate", resp, &query_url).await {
                QueryRead::Body(b) => b,
                QueryRead::Failed(e) => return e,
            };
            let methods = body["result"]["content"].clone();

            let prompt = format!(
                "Write an InterSystems IRIS %UnitTest.TestCase subclass to test '{}'. \
                 Requirements: {}. \
                 The class has these methods: {}. \
                 Rules: extend %UnitTest.TestCase, prefix test methods with 'Test', \
                 use $$$AssertEquals/$$$AssertTrue macros, include ##class({}).%New() in setup. \
                 Write only valid ObjectScript — no explanations, no markdown fences.",
                cls,
                p.description,
                serde_json::to_string(&methods).unwrap_or_default(),
                cls
            );

            ok_json(serde_json::json!({
                "success": true,
                "gen_type": "test",
                "target_class": cls,
                "namespace": ns,
                "prompt": prompt,
                "context": {
                    "methods": methods,
                    "suggested_class_name": format!("{}.Test", cls),
                },
                "instructions": "Use the prompt above to write the class, then call iris_doc(mode=put) to save it and iris_compile to compile it."
            }))
        }

        _ => {
            // Fetch existing classes in the namespace as naming/style context
            let sql = "SELECT TOP 10 Name FROM %Dictionary.ClassDefinition \
                       WHERE Name NOT LIKE '%\\%%' ESCAPE '\\' ORDER BY Name";
            let resp = match client
                .post(&query_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&serde_json::json!({"query": sql}))
                .send()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::transport_fail(
                        "handle_iris_generate",
                        &e.to_string(),
                    )
                }
            };
            let body = match read_query_context("iris_generate", resp, &query_url).await {
                QueryRead::Body(b) => b,
                QueryRead::Failed(e) => return e,
            };
            let existing: Vec<String> = body["result"]["content"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|r| r["Name"].as_str().map(|s| s.to_string()))
                .collect();

            // Detect likely package prefix from existing classes
            let package = existing
                .first()
                .and_then(|n| n.split('.').next())
                .unwrap_or("MyApp")
                .to_string();

            let prompt = format!(
                "Write an InterSystems IRIS ObjectScript class. \
                 Requirements: {}. \
                 Use package prefix '{}' to match existing classes in this namespace. \
                 Rules: valid ObjectScript syntax, extend %Persistent or %RegisteredObject \
                 as appropriate, include property definitions with types, add basic accessor \
                 methods if needed. Write only the class code — no explanations, no markdown fences.",
                p.description, package
            );

            ok_json(serde_json::json!({
                "success": true,
                "gen_type": "class",
                "namespace": ns,
                "prompt": prompt,
                "context": {
                    "existing_classes": existing,
                    "suggested_package": package,
                    "iris_version": iris.version.as_deref().unwrap_or("unknown"),
                },
                "instructions": "Use the prompt above to write the class, then call iris_doc(mode=put) to save it and iris_compile to compile it."
            }))
        }
    }
}

// ── iris_table_info ───────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TableInfoParams {
    /// SQL table in Schema.Table form ("SQLUser.MyTable", "Ens_Config.Item") — or the CLASS name
    /// ("Ens.Config.Item"), which is resolved for you: IRIS projects class `A.B.C.Name` onto
    /// schema `A_B_C`, table `Name`. A miss names the tables that do exist in that package.
    pub table: String,
    /// IRIS namespace to query. Defaults to the connection namespace (IRIS_NAMESPACE).
    #[serde(default)]
    pub namespace: Option<String>,
    /// Include approximate row count (runs SELECT COUNT(*) — may be slow on large tables).
    #[serde(default)]
    pub include_row_count: bool,
}

/// Every `(schema, table)` pair worth trying for one caller-supplied name, best first (#120).
///
/// `iris_table_info` used to accept the exact SQL table name and nothing else, so handing it the
/// CLASS name — which is what a caller actually has, and what every other tool here takes —
/// returned a bare `TABLE_NOT_FOUND`. Agents then brute-forced the separator, three and four
/// calls at a time, while the success payload was already reporting `class` back to them.
///
/// IRIS projects class `A.B.C.Name` onto SQL schema `A_B_C`, table `Name`. That mapping is
/// mechanical, so it is applied here rather than left to the caller:
///
/// * as given — split at the FIRST dot, `SQLUser` when there is none. Unchanged, so a name that
///   resolved before still resolves first and no existing call changes meaning.
/// * two dots or more — split at the LAST dot and underscore the schema
///   (`EnsLib.HL7.Message` → `EnsLib_HL7` / `Message`).
/// * no dot but underscores — split at the LAST underscore
///   (`Admissions_MSG_AdmitNoticeReq` → `Admissions_MSG` / `AdmitNoticeReq`).
pub fn sql_table_candidates(input: &str) -> Vec<(String, String)> {
    let name = input.trim();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |schema: String, table: String| {
        if !table.is_empty() && !out.iter().any(|(s, t)| *s == schema && *t == table) {
            out.push((schema, table));
        }
    };

    match name.find('.') {
        Some(idx) => push(name[..idx].to_string(), name[idx + 1..].to_string()),
        None => push("SQLUser".to_string(), name.to_string()),
    }

    if name.matches('.').count() >= 2 {
        if let Some(idx) = name.rfind('.') {
            push(name[..idx].replace('.', "_"), name[idx + 1..].to_string());
        }
    }

    if !name.contains('.') {
        if let Some(idx) = name.rfind('_') {
            push(name[..idx].to_string(), name[idx + 1..].to_string());
        }
    }

    out
}

/// Order and filter the tables offered back as `did_you_mean` (#120).
///
/// `pkg` are tables in the package the caller's name points at ("what does exist in `Ens_Rule`?");
/// `near` are tables anywhere whose name starts the same way ("this table, in another schema?").
/// Package hits rank first because they answer the more useful question.
///
/// A suggestion sharing nothing with the request is worse than no suggestion. The first cut of
/// this offered `%DeepSee_Dashboard.Definition` for `Ens.Rule.Definition` and told the caller to
/// pass it — so a candidate must share the request's leading segment, and a `%`-schema table is
/// only ever offered to a caller who asked for one.
pub fn rank_table_suggestions(requested: &str, pkg: Vec<String>, near: Vec<String>) -> Vec<String> {
    let wants_system = requested.starts_with('%');
    let lead = requested
        .split(['.', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let plausible = |cand: &String| {
        if cand.starts_with('%') && !wants_system {
            return false;
        }
        lead.is_empty() || cand.to_ascii_lowercase().starts_with(&lead)
    };
    let mut out: Vec<String> = Vec::new();
    for cand in pkg.into_iter().chain(near) {
        if plausible(&cand) && !out.contains(&cand) {
            out.push(cand);
        }
    }
    out.truncate(5);
    out
}

pub async fn handle_iris_table_info(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: TableInfoParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let candidates = sql_table_candidates(&p.table);
    // Two needles for did_you_mean, because they answer different questions. The most-normalised
    // candidate's TABLE half ("did you mean this table, in some other schema?") and its SCHEMA
    // half ("what does exist in this package?"). The second is the more useful of the two:
    // `Ens.Rule.Definition` has no table anywhere, but `Ens_Rule.*` has five.
    let (needle_schema, needle_table) = candidates
        .last()
        .cloned()
        .unwrap_or_else(|| (String::new(), p.table.clone()));
    let (mut sql_schema, mut sql_table) = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| ("SQLUser".to_string(), p.table.clone()));

    let cand_list = candidates
        .iter()
        .map(|(s, t)| os_str_expr(&format!("{s}^{t}")))
        .collect::<Vec<_>>()
        .join(",");

    // Look up class projection: find a compiled class whose SQL mapping matches.
    // Every candidate is tried inside ONE round trip — resolution must not cost the caller the
    // extra calls it was meant to save. The `while` condition is fully parenthesised on purpose:
    // ObjectScript has no operator precedence (see #118).
    let lookup_code = format!(
        r#"
set cands = $LISTBUILD({cands})
set found = "", ci = 0
while (found = "") && (ci < $LISTLENGTH(cands)) {{
    set ci = ci + 1
    set one = $LIST(cands, ci)
    set cs = $PIECE(one, "^", 1), ct = $PIECE(one, "^", 2)
    set rsEx = ##class(%SQL.Statement).%ExecDirect(,"SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?", cs, ct)
    if rsEx.%Next() {{ if (rsEx.%GetData(1) '= 0) {{ set found = one }} }}
}}
if found = "" {{
    set rsP = ##class(%SQL.Statement).%ExecDirect(,"SELECT TOP 5 TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA %STARTSWITH ? ORDER BY TABLE_SCHEMA, TABLE_NAME", {needle_schema})
    while rsP.%Next() {{ write "PKG:",rsP.%GetData(1),".",rsP.%GetData(2),! }}
    set rsN = ##class(%SQL.Statement).%ExecDirect(,"SELECT TOP 8 TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME %STARTSWITH ? ORDER BY TABLE_SCHEMA", {needle_table})
    while rsN.%Next() {{ write "NEAR:",rsN.%GetData(1),".",rsN.%GetData(2),! }}
    write "NOT_FOUND",!
    quit
}}
set sqlSchema = $PIECE(found, "^", 1), sqlTable = $PIECE(found, "^", 2)
write "RESOLVED:",sqlSchema,".",sqlTable,!
// Look for backing class
set rs = ##class(%SQL.Statement).%ExecDirect(,"SELECT c.Name, c.ClassType, s.DataLocation, s.IndexLocation, s.IDLocation FROM %Dictionary.CompiledClass c LEFT JOIN %Dictionary.CompiledStorage s ON s.parent = c.Name WHERE c.SqlSchemaName = ? AND c.SqlTableName = ?", sqlSchema, sqlTable)
if rs.%Next() {{
    write "CLASS:",rs.Name,!
    write "CLASSTYPE:",rs.ClassType,!
    write "DATA:",rs.DataLocation,!
    write "INDEX:",rs.IndexLocation,!
    write "ID:",rs.IDLocation,!
}} else {{
    write "DDL_TABLE",!
}}
"#,
        cands = cand_list,
        needle_schema = os_str_expr(&needle_schema),
        needle_table = os_str_expr(&needle_table),
    );

    let output = match iris
        .execute_via_generator(&lookup_code, &namespace, client)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            // #102: a mistyped namespace answered `info::output: PUT doc failed: HTTP 404 Not
            // Found` — the generator's scaffolding, not the caller's mistake. #101: a wrong
            // password answered the same shape under IRIS_EXECUTE_ERROR, which named neither
            // credentials nor the 401.
            if let Some(missing) = crate::tools::interop::namespace_missing_error_for(
                iris,
                client,
                &namespace,
                "No table info was read.",
                &e,
            )
            .await
            {
                return missing;
            }
            let msg = e.to_string();
            return crate::tools::envelope::fail(
                crate::tools::interop::classify_iris_error_or(&msg, "IRIS_EXECUTE_ERROR"),
                &format!("info::output: {msg}"),
            );
        }
    };

    let lines: std::collections::HashMap<&str, &str> =
        output.lines().filter_map(|l| l.split_once(':')).collect();

    if output.lines().any(|l| l.trim() == "NOT_FOUND") {
        // #120: a truthful "not found" that names no alternative is what made callers guess the
        // separator. Same treatment #107 gave docs_introspect: say what was tried, and name what
        // actually exists.
        let collect = |prefix: &str| -> Vec<String> {
            output
                .lines()
                .filter_map(|l| l.strip_prefix(prefix))
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        };
        let did_you_mean = rank_table_suggestions(&p.table, collect("PKG:"), collect("NEAR:"));
        let tried: Vec<String> = candidates
            .iter()
            .map(|(sc, tb)| format!("{sc}.{tb}"))
            .collect();
        let mut extra = serde_json::json!({
            "table": p.table,
            "namespace": namespace,
            "tried": tried,
        });
        if !did_you_mean.is_empty() {
            extra["did_you_mean"] = did_you_mean.clone().into();
            extra["hint"] = format!(
                "No such table. IRIS projects class 'A.B.C.Name' onto SQL schema 'A_B_C', table \
                 'Name' — pass the SQL name, not the class name. These exist in this namespace: \
                 {}.",
                did_you_mean.join(", ")
            )
            .into();
        } else {
            extra["hint"] = format!(
                "No such table in namespace '{namespace}'. IRIS projects class 'A.B.C.Name' onto \
                 SQL schema 'A_B_C', table 'Name'. Check the namespace, or call \
                 iris_symbols/docs_introspect to confirm the class exists and is %Persistent."
            )
            .into();
        }
        return crate::tools::envelope::fail_with(
            "TABLE_NOT_FOUND",
            &format!("Table '{}' not found in namespace '{}'", p.table, namespace),
            extra,
        );
    }

    // Which candidate actually resolved. Everything downstream — the row count, the DDL global
    // names — must use the resolved pair, not what the caller typed.
    let requested = format!("{sql_schema}.{sql_table}");
    if let Some(resolved) = lines.get("RESOLVED").map(|v| v.trim()) {
        if let Some(idx) = resolved.rfind('.') {
            sql_schema = resolved[..idx].to_string();
            sql_table = resolved[idx + 1..].to_string();
        }
    }
    let resolved_table = format!("{sql_schema}.{sql_table}");
    let renamed = resolved_table != requested;

    let result = if lines.contains_key("CLASS") {
        // Class-projected table
        let class_name = lines.get("CLASS").copied().unwrap_or("").trim();
        let data_global = lines.get("DATA").copied().unwrap_or("").trim();
        let index_global = lines.get("INDEX").copied().unwrap_or("").trim();

        let mut obj = serde_json::json!({
            "table": resolved_table,
            "type": "class_projection",
            "class": class_name,
            "namespace": namespace,
            "data_global": if data_global.is_empty() { serde_json::Value::Null } else { data_global.into() },
            "index_global": if index_global.is_empty() { serde_json::Value::Null } else { index_global.into() },
            "accessible_from_embedded_python": true,
        });

        if p.include_row_count {
            let count = get_row_count(iris, client, &namespace, &sql_schema, &sql_table).await;
            obj["row_count"] = count;
        }
        obj
    } else {
        // DDL-created table — infer global names by IRIS naming convention
        let data_global = format!("^{}.{}D", sql_schema, sql_table);
        let index_global = format!("^{}.{}I", sql_schema, sql_table);
        let id_counter_global = format!("^{}.{}C", sql_schema, sql_table);

        let mut obj = serde_json::json!({
            "table": resolved_table,
            "type": "ddl_table",
            "namespace": namespace,
            "data_global": data_global,
            "index_global": index_global,
            "id_counter_global": id_counter_global,
            "accessible_from_embedded_python": true,
        });

        if p.include_row_count {
            let count = get_row_count(iris, client, &namespace, &sql_schema, &sql_table).await;
            obj["row_count"] = count;
        }
        obj
    };

    let mut payload = serde_json::json!({
        "success": true,
        "result": result,
    });
    if renamed {
        // Say so rather than silently answering about a different name than the one asked for.
        payload["requested"] = p.table.clone().into();
        payload["note"] = format!(
            "'{}' is a class name; it resolves to SQL table '{}'.",
            p.table, resolved_table
        )
        .into();
    }
    crate::tools::ok_json(payload)
}

async fn get_row_count(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    schema: &str,
    table: &str,
) -> serde_json::Value {
    // #67: build the SQL in Rust, then hand the whole statement to ObjectScript as ONE
    // escaped expression. The delimited-identifier quotes around the schema are part of the
    // SQL text, and os_str_expr doubles them for the ObjectScript literal.
    let sql = format!(r#"SELECT COUNT(*) FROM "{schema}".{table}"#);
    let code = format!(
        r#"set rs = ##class(%SQL.Statement).%ExecDirect(,{sql})
if rs.%Next() {{ write rs.%GetData(1),! }} else {{ write "error",! }}"#,
        sql = os_str_expr(&sql),
    );
    match iris.execute_via_generator(&code, namespace, client).await {
        Ok(out) => out
            .trim()
            .parse::<u64>()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod table_candidate_tests {
    use super::sql_table_candidates;

    fn tried(input: &str) -> Vec<String> {
        sql_table_candidates(input)
            .into_iter()
            .map(|(s, t)| format!("{s}.{t}"))
            .collect()
    }

    /// The exact corpus failure: 37 `TABLE_NOT_FOUND`s, agents guessing separators.
    #[test]
    fn a_class_name_yields_the_projected_table() {
        assert!(tried("EnsLib.HL7.Message").contains(&"EnsLib_HL7.Message".to_string()));
        assert!(tried("Admissions.MSG.AdmitNoticeReq")
            .contains(&"Admissions_MSG.AdmitNoticeReq".to_string()));
    }

    /// `Admissions_MSG_AdmitNoticeReq` was the second guess in the corpus; one underscore short.
    #[test]
    fn an_all_underscore_name_splits_at_the_last_underscore() {
        assert!(tried("Admissions_MSG_AdmitNoticeReq")
            .contains(&"Admissions_MSG.AdmitNoticeReq".to_string()));
    }

    /// No regression: whatever resolved before still resolves, and still resolves FIRST.
    #[test]
    fn the_exact_sql_name_is_always_tried_first() {
        assert_eq!(tried("EnsLib_HL7.Message")[0], "EnsLib_HL7.Message");
        assert_eq!(tried("SQLUser.MyTable")[0], "SQLUser.MyTable");
        assert_eq!(tried("MyTable")[0], "SQLUser.MyTable");
        assert_eq!(tried("Billing.LabCharge")[0], "Billing.LabCharge");
    }

    #[test]
    fn candidates_are_deduped_and_never_empty_tabled() {
        for input in ["A.B", "A_B", "A", "A.B.C", "A_B_C", "  Padded.Name  "] {
            let c = sql_table_candidates(input);
            assert!(!c.is_empty(), "{input} produced no candidate");
            assert!(c.iter().all(|(_, t)| !t.is_empty()), "{input} empty table");
            let mut seen = c.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), c.len(), "{input} produced duplicates");
        }
    }
}

#[cfg(test)]
mod table_suggestion_tests {
    use super::rank_table_suggestions;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The regression this ranker exists for: the first cut answered `Ens.Rule.Definition` with
    /// `%DeepSee_Dashboard.Definition` and told the caller to pass it.
    #[test]
    fn an_unrelated_system_table_is_never_offered() {
        let out = rank_table_suggestions(
            "Ens.Rule.Definition",
            v(&["Ens_Rule.Assign", "Ens_Rule.Rule"]),
            v(&["%DeepSee_Dashboard.Definition", "%IPM_Repo.Definition"]),
        );
        assert_eq!(out, v(&["Ens_Rule.Assign", "Ens_Rule.Rule"]));
    }

    #[test]
    fn nothing_plausible_yields_nothing_rather_than_noise() {
        let out = rank_table_suggestions(
            "Totally.Bogus.Name",
            vec![],
            v(&["%Dictionary.ClassDefinition", "Ens_Rule.Rule"]),
        );
        assert!(out.is_empty(), "{out:?}");
    }

    /// A caller who asks for a `%` schema is asking for system tables.
    #[test]
    fn a_system_request_still_gets_system_answers() {
        let out = rank_table_suggestions(
            "%Dictionary.Nope",
            v(&["%Dictionary.CompiledClass"]),
            vec![],
        );
        assert_eq!(out, v(&["%Dictionary.CompiledClass"]));
    }

    #[test]
    fn package_hits_rank_ahead_of_name_hits_and_duplicates_collapse() {
        let out = rank_table_suggestions(
            "Ens.Config.Nope",
            v(&["Ens_Config.Item"]),
            v(&["Ens_Config.Item", "Ens_Util.Nope"]),
        );
        assert_eq!(out, v(&["Ens_Config.Item", "Ens_Util.Nope"]));
    }

    #[test]
    fn at_most_five_are_offered() {
        let many = v(&[
            "Ens_A.1", "Ens_A.2", "Ens_A.3", "Ens_A.4", "Ens_A.5", "Ens_A.6",
        ]);
        assert_eq!(rank_table_suggestions("Ens.A.X", many, vec![]).len(), 5);
    }
}
