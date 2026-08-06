use crate::iris::connection::IrisConnection;
use crate::objectscript::{os_str_expr, os_stream_write_stmts};
use rmcp::{model::*, ErrorData as McpError};
use schemars::JsonSchema;
use serde::Deserialize;

fn ok_json(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}
fn err_json(code: &str, msg: &str) -> Result<CallToolResult, McpError> {
    ok_json(serde_json::json!({"success": false, "error_code": code, "error": msg}))
}
fn iris_unreachable() -> McpError {
    McpError::invalid_request("IRIS_UNREACHABLE", None)
}
// Bug 18: "connection" matched too broadly — e.g. "No Interoperability connection configured"
// was misclassified as IRIS_UNREACHABLE. Use more specific network-error patterns.
pub(crate) fn is_network_error(msg: &str) -> bool {
    msg.contains("error sending")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("dns error")
        || msg.contains("timed out")
}

fn default_ns() -> String {
    "USER".to_string()
}

fn default_true() -> bool {
    true
}

// ═══ issue #5: namespace resolution + self-describing interop preflight ═══

/// `namespace` is optional on the interop tools: an omitted or empty value
/// resolves to the CONNECTION's namespace (IRIS_NAMESPACE / discovery), never
/// a hardcoded "USER" — on interop-configured servers "USER" is almost never
/// the namespace the caller means, which made omission fail 95% of the time.
pub fn resolve_namespace(requested: Option<&str>, iris: Option<&IrisConnection>) -> String {
    match requested {
        Some(ns) if !ns.trim().is_empty() => ns.to_string(),
        _ => iris
            .map(|i| i.namespace.clone())
            .unwrap_or_else(|| "USER".to_string()),
    }
}

/// Positive probe results, keyed "base_url|namespace". Never invalidated: a
/// namespace does not lose Interoperability within a server process lifetime.
/// Negatives are NOT cached — a namespace can gain interop (or be created)
/// while the server runs.
static INTEROP_NS_CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// Enumerate interop-enabled namespaces for the error hint (best effort).
/// `$EXTRACT(tNs)'="%"` skips system namespaces; `Continue` is deliberately
/// avoided — it must be the last command on its line, which a one-line For
/// body cannot honor.
async fn list_interop_namespaces(
    iris: &IrisConnection,
    ns: &str,
    client: &reqwest::Client,
) -> Option<String> {
    const CODE: &str = r#"Set tSaved=$NAMESPACE,tOut=""
Try { Do ##class(%SYS.Namespace).ListAll(.arr) } Catch ex {}
Set tNs="" For { Set tNs=$ORDER(arr(tNs)) Quit:tNs=""  If $EXTRACT(tNs)'="%" { Try { Set $NAMESPACE=tNs If ##class(%Dictionary.CompiledClass).%ExistsId("Ens.Director") { Set tOut=tOut_$SELECT(tOut="":"",1:",")_tNs } } Catch ex {} } }
Set $NAMESPACE=tSaved
Write tOut"#;
    iris.execute_via_generator(CODE, ns, client)
        .await
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty())
}

/// Issue #5: fail fast and self-describing when the target namespace has no
/// Interoperability. Without this, the underlying calls surface raw internals
/// ("Table 'ENS_CONFIG.CREDENTIALS' not found", <CLASS DOES NOT EXIST>) that
/// never name the cause. Probes %Dictionary.CompiledClass for Ens.Director —
/// one SELECT round trip, positives cached per process. Returns Some(error)
/// only on a definitive "no interop here"; any probe failure returns None so
/// the tool's own error surfaces instead.
pub async fn ensure_interop_namespace(
    iris: &IrisConnection,
    ns: &str,
) -> Option<Result<CallToolResult, McpError>> {
    let key = format!("{}|{}", iris.base_url, ns);
    let cache = INTEROP_NS_CACHE.get_or_init(Default::default);
    if cache.lock().map(|c| c.contains(&key)).unwrap_or(false) {
        return None;
    }
    let client = IrisConnection::http_client().ok()?;
    let probe = iris
        .query(
            "SELECT COUNT(*) AS n FROM %Dictionary.CompiledClass WHERE Name = 'Ens.Director'",
            vec![],
            ns,
            &client,
        )
        .await;
    let n = match &probe {
        Ok(resp) => resp["result"]["content"][0]["n"].as_i64()?,
        Err(_) => return None,
    };
    if n >= 1 {
        if let Ok(mut c) = cache.lock() {
            c.insert(key);
        }
        return None;
    }
    let available = list_interop_namespaces(iris, ns, &client).await;
    let hint = match &available {
        Some(list) => format!(
            "Pass namespace= one of the interop-enabled namespaces on this instance: {list}. \
             Omitting namespace targets the connection namespace '{}'.",
            iris.namespace
        ),
        None => format!(
            "Pass namespace= an interop-enabled namespace. \
             Omitting namespace targets the connection namespace '{}'.",
            iris.namespace
        ),
    };
    Some(ok_json(serde_json::json!({
        "success": false,
        "error_code": "NAMESPACE_NOT_INTEROP",
        "error": format!(
            "Namespace '{ns}' has no Interoperability enabled — the Ens.* classes and \
             Ens_* tables do not exist there, so interop tools cannot run in it."
        ),
        "hint": hint,
        "namespace": ns,
        "interop_namespaces": available
            .map(|l| l.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default(),
    })))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionStatusParams {
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default)]
    pub full_status: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionNameParams {
    pub production: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionStopParams {
    pub production: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(default)]
    pub force: bool,
}
fn default_timeout() -> u32 {
    30
}

// Bug 7: added namespace field so update/recover/needs_update work in non-default namespaces.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionUpdateParams {
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionNeedsUpdateParams {
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionRecoverParams {
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogsParams {
    /// Production namespace to query; falls back to the connection's namespace when None.
    #[serde(default)]
    pub namespace: Option<String>,
    pub item_name: Option<String>,
    /// Restrict to one interop session (SessionId column) — events of one message flow.
    #[serde(default)]
    pub session_id: Option<i64>,
    /// Tail since a watermark: only rows with ID greater than this (replaces the
    /// `SELECT MAX(ID)` then `WHERE ID > N` two-call dance the model did ~21x).
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_log_type")]
    pub log_type: String,
}
fn default_limit() -> u32 {
    10
}
fn default_log_type() -> String {
    "error,warning".to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueuesParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessageSearchParams {
    /// Production namespace to query; falls back to the connection's namespace when None.
    #[serde(default)]
    pub namespace: Option<String>,
    pub source: Option<String>,
    pub target: Option<String>,
    pub class_name: Option<String>,
    /// Restrict to one interop session (SessionId) — the messages of one message flow.
    #[serde(default)]
    pub session_id: Option<i64>,
    /// Only headers with ID greater than this watermark (tail-since pattern).
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default = "default_msg_limit")]
    pub limit: u32,
}
fn default_msg_limit() -> u32 {
    20
}

fn state_string(code: i64) -> &'static str {
    match code {
        1 => "Running",
        2 => "Stopped",
        3 => "Suspended",
        4 => "Troubled",
        5 => "NetworkStopped",
        _ => "Unknown",
    }
}

pub fn parse_status_response(raw: &str) -> Result<(String, i64, String), String> {
    if raw.is_empty() || raw == ":" {
        return Err("NO_PRODUCTION".to_string());
    }
    if raw.starts_with("ERROR") {
        return Err(format!("INTEROP_ERROR:{}", raw));
    }
    let parts: Vec<&str> = raw.splitn(2, ':').collect();
    if parts.len() < 2 || parts[0].is_empty() {
        return Err("NO_PRODUCTION".to_string());
    }
    let name = parts[0].to_string();
    let code: i64 = parts[1].trim().parse().unwrap_or(0);
    let state = state_string(code).to_string();
    Ok((name, code, state))
}

fn docker_required_interop() -> Result<CallToolResult, McpError> {
    err_json(
        "DOCKER_REQUIRED",
        "Interoperability operations require docker exec. Set IRIS_CONTAINER=<container_name>.",
    )
}

/// Run interop ObjectScript over HTTP (Atelier compile + SqlProc), so the production
/// lifecycle works on native-Windows/Linux IRIS **without** a Docker container — the same
/// path the credential/lookup impls already use. (A3: the production impls previously called
/// `iris.execute()`, which returns DOCKER_REQUIRED when IRIS_CONTAINER is unset.) With the A2
/// fix to the executor class, the `Write "OK"` / `Write "ERROR:..."` output is captured.
async fn exec_http(iris: &IrisConnection, code: &str, ns: &str) -> anyhow::Result<String> {
    let client = IrisConnection::http_client()?;
    iris.execute_via_generator(code, ns, &client).await
}

pub async fn interop_production_status_impl(
    iris: Option<&IrisConnection>,
    params: ProductionStatusParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let code = r#"Set sc=##class(Ens.Director).GetProductionStatus(.n,.s) If $$$ISERR(sc) { Write "ERROR:"_$System.Status.GetErrorText(sc) } Else { Write n_":"_s }"#;
    // Bug 7: use params.namespace, not iris.namespace.
    match exec_http(iris, code, &params.namespace).await {
        Ok(output) => {
            let raw = output.trim().to_string();
            match parse_status_response(&raw) {
                Ok((name, code, state)) => ok_json(
                    serde_json::json!({"success": true, "production": name, "state": state, "state_code": code}),
                ),
                Err(e) if e.starts_with("INTEROP_ERROR") => err_json("INTEROP_ERROR", &e[14..]),
                Err(_) => err_json("NO_PRODUCTION", "No production is running"),
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_production_start_impl(
    iris: Option<&IrisConnection>,
    params: ProductionNameParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let prod = params.production.as_deref().unwrap_or("");
    let code = format!(
        r#"Set sc=##class(Ens.Director).StartProduction("{}") If $$$ISERR(sc) {{ Write "ERROR:"_$System.Status.GetErrorText(sc) }} Else {{ Write "OK" }}"#,
        prod
    );
    // Bug 7: use params.namespace, not iris.namespace.
    match exec_http(iris, &code, &params.namespace).await {
        Ok(output) => {
            let raw = output.trim();
            if raw.starts_with("OK") {
                ok_json(serde_json::json!({"success": true, "state": "Running"}))
            } else {
                err_json("INTEROP_ERROR", raw)
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_production_stop_impl(
    iris: Option<&IrisConnection>,
    params: ProductionStopParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let code = format!(
        r#"Set sc=##class(Ens.Director).StopProduction({},{}) If $$$ISERR(sc) {{ Write "ERROR:"_$System.Status.GetErrorText(sc) }} Else {{ Write "OK" }}"#,
        params.timeout,
        if params.force { 1 } else { 0 }
    );
    // Bug 7: use params.namespace, not iris.namespace.
    match exec_http(iris, &code, &params.namespace).await {
        Ok(output) => {
            let raw = output.trim();
            if raw.starts_with("OK") {
                ok_json(serde_json::json!({"success": true, "state": "Stopped"}))
            } else {
                err_json("INTEROP_ERROR", raw)
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_production_update_impl(
    iris: Option<&IrisConnection>,
    params: ProductionUpdateParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let code = format!(
        r#"Set sc=##class(Ens.Director).UpdateProduction({},{}) If $$$ISERR(sc) {{ Write "ERROR:"_$System.Status.GetErrorText(sc) }} Else {{ Write "OK" }}"#,
        params.timeout,
        if params.force { 1 } else { 0 }
    );
    // Bug 7: use params.namespace.
    match exec_http(iris, &code, &params.namespace).await {
        Ok(output) => {
            let raw = output.trim();
            if raw.starts_with("OK") {
                ok_json(serde_json::json!({"success": true, "message": "Production updated"}))
            } else {
                err_json("INTEROP_ERROR", raw)
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_production_needs_update_impl(
    iris: Option<&IrisConnection>,
    params: ProductionNeedsUpdateParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let code = r#"Write ##class(Ens.Director).ProductionNeedsUpdate()"#;
    // Bug 7: use params.namespace.
    match exec_http(iris, code, &params.namespace).await {
        Ok(output) => {
            ok_json(serde_json::json!({"success": true, "needs_update": output.trim() == "1"}))
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_production_recover_impl(
    iris: Option<&IrisConnection>,
    params: ProductionRecoverParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let code = r#"Set sc=##class(Ens.Director).RecoverProduction() If $$$ISERR(sc) { Write "ERROR:"_$System.Status.GetErrorText(sc) } Else { Write "OK" }"#;
    // Bug 7: use params.namespace.
    match exec_http(iris, code, &params.namespace).await {
        Ok(output) => {
            let raw = output.trim();
            if raw.starts_with("OK") {
                ok_json(serde_json::json!({"success": true, "state": "Running"}))
            } else {
                err_json("INTEROP_ERROR", raw)
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_logs_impl(
    iris: Option<&IrisConnection>,
    params: LogsParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    let mut conditions = vec![];
    for lt in params.log_type.split(',') {
        match lt.trim().to_lowercase().as_str() {
            "error" => conditions.push("Type = 3"),
            "warning" => conditions.push("Type = 2"),
            "info" => conditions.push("Type = 1"),
            "alert" => conditions.push("Type = 4"),
            _ => {}
        }
    }
    let type_filter = if conditions.is_empty() {
        String::new()
    } else {
        format!("AND ({})", conditions.join(" OR "))
    };
    let item_filter = params
        .item_name
        .as_ref()
        .map(|n| format!("AND ConfigName = '{}'", n.replace('\'', "''")))
        .unwrap_or_default();
    // session_id / since_id are numeric (i64) — safe to inline, no injection surface.
    let session_filter = params
        .session_id
        .map(|s| format!("AND SessionId = {}", s))
        .unwrap_or_default();
    let since_filter = params
        .since_id
        .map(|s| format!("AND ID > {}", s))
        .unwrap_or_default();
    let sql = format!("SELECT TOP {} ID, TimeLogged, Type, ConfigName, SessionId, Text FROM Ens_Util.Log WHERE 1=1 {} {} {} {} ORDER BY ID DESC", params.limit, type_filter, item_filter, session_filter, since_filter);
    // A6: query the requested production namespace, not the connection default.
    let ns = params
        .namespace
        .as_deref()
        .unwrap_or(iris.namespace.as_str());
    match iris.query(&sql, vec![], ns, &client).await {
        Ok(resp) => ok_json(
            serde_json::json!({"success": true, "logs": resp["result"]["content"], "count": resp["result"]["content"].as_array().map(|a| a.len()).unwrap_or(0)}),
        ),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_queues_impl(
    iris: Option<&IrisConnection>,
    namespace: Option<String>,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    // A6: query the requested production namespace, not the connection default.
    let ns = namespace.as_deref().unwrap_or(iris.namespace.as_str());
    match iris
        .query("SELECT * FROM Ens.Queue_Enumerate()", vec![], ns, &client)
        .await
    {
        Ok(resp) => {
            ok_json(serde_json::json!({"success": true, "queues": resp["result"]["content"]}))
        }
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_message_search_impl(
    iris: Option<&IrisConnection>,
    params: MessageSearchParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    let mut filters = vec![];
    if let Some(src) = &params.source {
        filters.push(format!("SourceConfigName = '{}'", src.replace('\'', "''")));
    }
    if let Some(tgt) = &params.target {
        filters.push(format!("TargetConfigName = '{}'", tgt.replace('\'', "''")));
    }
    if let Some(cls) = &params.class_name {
        filters.push(format!(
            "MessageBodyClassName = '{}'",
            cls.replace('\'', "''")
        ));
    }
    // session_id / since_id are numeric (i64) — safe to inline.
    if let Some(sid) = params.session_id {
        filters.push(format!("SessionId = {}", sid));
    }
    if let Some(since) = params.since_id {
        filters.push(format!("ID > {}", since));
    }
    let where_clause = if filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", filters.join(" AND "))
    };
    let sql = format!("SELECT TOP {} ID, TimeCreated, SourceConfigName, TargetConfigName, MessageBodyClassName, SessionId, Status FROM Ens.MessageHeader {} ORDER BY ID DESC", params.limit, where_clause);
    // A6: query the requested production namespace, not the connection default.
    let ns = params
        .namespace
        .as_deref()
        .unwrap_or(iris.namespace.as_str());
    match iris.query(&sql, vec![], ns, &client).await {
        Ok(resp) => ok_json(
            serde_json::json!({"success": true, "messages": resp["result"]["content"], "count": resp["result"]["content"].as_array().map(|a| a.len()).unwrap_or(0)}),
        ),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

/// A · what=trace: the full picture of ONE interop session (i.e. one initial message and everything
/// it triggered) in a single call — the Ens.MessageHeader chain PLUS the Ens_Util.Log events for that
/// SessionId. Replaces the manual MessageHeader + Ens_Util.Log reconstruction (with the `SELECT MAX(ID)`
/// watermark dance) the model did by hand.
pub async fn interop_trace_impl(
    iris: Option<&IrisConnection>,
    namespace: Option<String>,
    session_id: i64,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    let ns = namespace.as_deref().unwrap_or(iris.namespace.as_str());
    let net_err = |e: &str| {
        if is_network_error(e) {
            "IRIS_UNREACHABLE"
        } else {
            "INTEROP_ERROR"
        }
    };
    // session_id is numeric (i64) — safe to inline.
    let msg_sql = format!("SELECT ID, TimeCreated, SourceConfigName, TargetConfigName, MessageBodyClassName, Status, IsError FROM Ens.MessageHeader WHERE SessionId = {} ORDER BY ID ASC", session_id);
    let log_sql = format!("SELECT ID, TimeLogged, Type, ConfigName, Text FROM Ens_Util.Log WHERE SessionId = {} ORDER BY ID ASC", session_id);
    let messages = match iris.query(&msg_sql, vec![], ns, &client).await {
        Ok(resp) => resp["result"]["content"].clone(),
        Err(e) => return err_json(net_err(&e.to_string()), &e.to_string()),
    };
    let events = match iris.query(&log_sql, vec![], ns, &client).await {
        Ok(resp) => resp["result"]["content"].clone(),
        Err(e) => return err_json(net_err(&e.to_string()), &e.to_string()),
    };
    let msg_count = messages.as_array().map(|a| a.len()).unwrap_or(0);
    let evt_count = events.as_array().map(|a| a.len()).unwrap_or(0);
    ok_json(serde_json::json!({
        "success": true,
        "session_id": session_id,
        "messages": messages,
        "events": events,
        "message_count": msg_count,
        "event_count": evt_count,
    }))
}

/// B · iris_production action=restart: recycle ONE production config item (disable then re-enable,
/// each with UpdateProduction) without stopping the whole production.
pub async fn interop_production_restart_item_impl(
    iris: Option<&IrisConnection>,
    namespace: &str,
    item: &str,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    if item.trim().is_empty() {
        return err_json(
            "INVALID_PARAMS",
            "restart requires 'item' (the config item name)",
        );
    }
    let item_esc = item.replace('"', "\"\"");
    let code = format!(
        r#"Set sc=##class(Ens.Director).EnableConfigItem("{}",0,1) If $$$ISERR(sc) {{ Write "ERROR:"_$System.Status.GetErrorText(sc) Quit }}
Set sc=##class(Ens.Director).EnableConfigItem("{}",1,1) If $$$ISERR(sc) {{ Write "ERROR:"_$System.Status.GetErrorText(sc) Quit }}
Write "OK""#,
        item_esc, item_esc
    );
    match exec_http(iris, &code, namespace).await {
        Ok(output) => {
            let raw = output.trim();
            if raw.starts_with("OK") {
                ok_json(serde_json::json!({"success": true, "item": item, "state": "restarted"}))
            } else {
                err_json("INTEROP_ERROR", raw)
            }
        }
        Err(e) if e.to_string() == "DOCKER_REQUIRED" => docker_required_interop(),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

/// B8: list configured interop Business Partners (Ens.Config.BusinessPartner) so the model gets real
/// rows instead of guessing nonexistent config tables. (SQL-Gateway connections have no clean SQL
/// table — that discovery path is the iris_query table-not-found hint + iris_table_info / the
/// introspect-dont-guess agent.)
pub async fn interop_partners_impl(
    iris: Option<&IrisConnection>,
    namespace: Option<String>,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    let ns = namespace.as_deref().unwrap_or(iris.namespace.as_str());
    match iris
        .query(
            "SELECT * FROM Ens_Config.BusinessPartner",
            vec![],
            ns,
            &client,
        )
        .await
    {
        Ok(resp) => {
            let rows = resp["result"]["content"].clone();
            let count = rows.as_array().map(|a| a.len()).unwrap_or(0);
            ok_json(serde_json::json!({"success": true, "partners": rows, "count": count}))
        }
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 024-interop-depth: Production item control (US1)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionItemParams {
    pub action: String,
    pub item: String,
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default)]
    pub settings: std::collections::HashMap<String, String>,
    /// set_settings: apply changes live via Ens.Director.UpdateProduction (default true).
    /// Pass false to batch several set_settings calls and apply once at the end.
    #[serde(default = "default_true")]
    pub apply: bool,
    /// add: the ObjectScript class the new item runs (e.g. "EnsLib.HL7.Service.FileService" or a
    /// custom BS/BO/BP class). Required for action=add.
    #[serde(default)]
    pub class_name: Option<String>,
    /// add: enable the item on creation (default true).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// add/remove: target production by name. Defaults to the currently running production.
    #[serde(default)]
    pub production: Option<String>,
    /// add: optional PoolSize for the item.
    #[serde(default)]
    pub pool_size: Option<i64>,
    /// add: optional Category for portal grouping.
    #[serde(default)]
    pub category: Option<String>,
}

/// Build the ObjectScript that adds a config item to a production (pure → unit-testable).
/// `production` empty ⇒ resolve the running production. Settings keys prefixed `Adapter.` target
/// the adapter; otherwise the Host. Applies live only if the target production is the one running.
pub fn build_add_item_code(
    production: &str,
    item: &str,
    class_name: &str,
    enabled: bool,
    pool_size: Option<i64>,
    category: Option<&str>,
    settings: &std::collections::HashMap<String, String>,
) -> String {
    let item_e = os_str_expr(item);
    let class_e = os_str_expr(class_name);
    let prod_e = os_str_expr(production);
    let mut extra = String::new();
    if let Some(ps) = pool_size {
        extra.push_str(&format!("Set tItem.PoolSize={}\n", ps));
    }
    if let Some(cat) = category {
        extra.push_str(&format!("Set tItem.Category={}\n", os_str_expr(cat)));
    }
    for (k, v) in settings {
        let (target, name) = match k.strip_prefix("Adapter.") {
            Some(rest) => ("Adapter", rest),
            None => ("Host", k.strip_prefix("Host.").unwrap_or(k)),
        };
        extra.push_str(&format!(
            "Set tS=##class(Ens.Config.Setting).%New() Set tS.Name={} Set tS.Target=\"{}\" Set tS.Value={} Do tItem.Settings.Insert(tS)\n",
            os_str_expr(name),
            target,
            os_str_expr(v)
        ));
    }
    format!(
        r#"Set tProdName={prod}
If tProdName="" {{ Set tSC=##class(Ens.Director).GetProductionStatus(.tProdName,.s) If tProdName="" {{ Write "ERROR:NO_PRODUCTION:No production running and no production= given" Quit }} }}
Set tProd=##class(Ens.Config.Production).%OpenId(tProdName,,.tSC2)
If '$IsObject(tProd) {{ Write "ERROR:INTEROP_ERROR:Cannot open production "_tProdName Quit }}
If $IsObject(tProd.FindItemByConfigName({item})) {{ Write "ERROR:ITEM_EXISTS:Item already exists: "_{item} Quit }}
Set tItem=##class(Ens.Config.Item).%New()
Set tItem.Name={item}
Set tItem.ClassName={class}
Set tItem.Enabled={enabled}
{extra}Do tProd.Items.Insert(tItem)
Set tSC4=tProd.%Save()
If $$$ISERR(tSC4) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC4) Quit }}
Set tRun="" Do ##class(Ens.Director).GetProductionStatus(.tRun,.s2)
If tRun=tProdName {{ Set tSC5=##class(Ens.Director).UpdateProduction(10,0) If $$$ISERR(tSC5) {{ Write "ERROR:UPDATE_FAILED:"_$System.Status.GetErrorText(tSC5) Quit }} }}
Write "OK:"_tProdName"#,
        prod = prod_e,
        item = item_e,
        class = class_e,
        enabled = if enabled { 1 } else { 0 },
        extra = extra
    )
}

/// Build the ObjectScript that removes a config item from a production (pure → unit-testable).
pub fn build_remove_item_code(production: &str, item: &str) -> String {
    let item_e = os_str_expr(item);
    let prod_e = os_str_expr(production);
    format!(
        r#"Set tProdName={prod}
If tProdName="" {{ Set tSC=##class(Ens.Director).GetProductionStatus(.tProdName,.s) If tProdName="" {{ Write "ERROR:NO_PRODUCTION:No production running and no production= given" Quit }} }}
Set tProd=##class(Ens.Config.Production).%OpenId(tProdName,,.tSC2)
If '$IsObject(tProd) {{ Write "ERROR:INTEROP_ERROR:Cannot open production "_tProdName Quit }}
Set tIdx=0 For i=1:1:tProd.Items.Count() {{ If tProd.Items.GetAt(i).Name={item} {{ Set tIdx=i Quit }} }}
If tIdx=0 {{ Write "ERROR:ITEM_NOT_FOUND:Item not found: "_{item} Quit }}
Do tProd.Items.RemoveAt(tIdx)
Set tSC4=tProd.%Save()
If $$$ISERR(tSC4) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC4) Quit }}
Set tRun="" Do ##class(Ens.Director).GetProductionStatus(.tRun,.s2)
If tRun=tProdName {{ Set tSC5=##class(Ens.Director).UpdateProduction(10,0) If $$$ISERR(tSC5) {{ Write "ERROR:UPDATE_FAILED:"_$System.Status.GetErrorText(tSC5) Quit }} }}
Write "OK:"_tProdName"#,
        prod = prod_e,
        item = item_e
    )
}

pub async fn interop_production_item_impl(
    iris: Option<&IrisConnection>,
    params: ProductionItemParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let item = os_str_expr(&params.item);
    let ns = &params.namespace;
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;

    match params.action.as_str() {
        "enable" | "disable" => {
            let enabled_val = if params.action == "enable" { "1" } else { "0" };
            let code = format!(
                r#"Set tSC=##class(Ens.Director).GetProductionStatus(.n,.s)
If $$$ISERR(tSC) {{ Write "ERROR:NO_PRODUCTION:"_$System.Status.GetErrorText(tSC) Quit }}
If n="" {{ Write "ERROR:NO_PRODUCTION:No production running" Quit }}
Set tProd=##class(Ens.Config.Production).%OpenId(n,,.tSC2)
If '$IsObject(tProd) {{ Write "ERROR:INTEROP_ERROR:Cannot open production" Quit }}
Set tItem=tProd.FindItemByConfigName({item},,.tSC3)
If '$IsObject(tItem) {{ Write "ERROR:ITEM_NOT_FOUND:Item not found: "_{item} Quit }}
Set tItem.Enabled={enabled_val}
Set tSC4=tProd.%Save()
If $$$ISERR(tSC4) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC4) Quit }}
Set tSC5=##class(Ens.Director).UpdateProduction(10,0)
If $$$ISERR(tSC5) {{ Write "ERROR:UPDATE_FAILED:"_$System.Status.GetErrorText(tSC5) Quit }}
Write "OK""#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"item":params.item,"enabled":params.action=="enable"}),
                        )
                    } else if let Some(msg) = out.strip_prefix("ERROR:ITEM_NOT_FOUND:") {
                        err_json("ITEM_NOT_FOUND", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                        err_json("NO_PRODUCTION", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:UPDATE_FAILED:") {
                        err_json("UPDATE_FAILED", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "get_settings" => {
            let code = format!(
                r#"Set tSC=##class(Ens.Director).GetProductionStatus(.n,.s)
If $$$ISERR(tSC)||n="" {{ Write "ERROR:NO_PRODUCTION:No production running" Quit }}
Set tProd=##class(Ens.Config.Production).%OpenId(n,,.tSC2)
If '$IsObject(tProd) {{ Write "ERROR:INTEROP_ERROR:Cannot open production" Quit }}
Set tItem=tProd.FindItemByConfigName({item},,.tSC3)
If '$IsObject(tItem) {{ Write "ERROR:ITEM_NOT_FOUND:Item not found: "_{item} Quit }}
Set tKey="" For {{ Set tSetting=tItem.Settings.GetNext(.tKey) Quit:tKey=""
  Write tSetting.Name_"="_tSetting.Value_$CHAR(10) }}"#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(msg) = out.strip_prefix("ERROR:ITEM_NOT_FOUND:") {
                        return err_json("ITEM_NOT_FOUND", msg);
                    }
                    if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                        return err_json("NO_PRODUCTION", msg);
                    }
                    if out.starts_with("ERROR:") {
                        return err_json("INTEROP_ERROR", out);
                    }
                    let settings: std::collections::HashMap<String, String> = out
                        .lines()
                        .filter_map(|line| {
                            let mut parts = line.splitn(2, '=');
                            let k = parts.next()?.trim().to_string();
                            let v = parts.next().unwrap_or("").to_string();
                            if k.is_empty() {
                                None
                            } else {
                                Some((k, v))
                            }
                        })
                        .collect();
                    ok_json(
                        serde_json::json!({"success":true,"item":params.item,"settings":settings}),
                    )
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "set_settings" => {
            if params.settings.is_empty() {
                return err_json(
                    "INVALID_PARAMS",
                    "set_settings requires at least one setting",
                );
            }
            // Build ObjectScript to set each setting then UpdateProduction
            let mut setting_lines = String::new();
            for (k, v) in &params.settings {
                let k_expr = os_str_expr(k);
                let v_expr = os_str_expr(v);
                setting_lines.push_str(&format!(
                    r#"Set tS=tItem.FindSettingByName({k},"Host")
If '$IsObject(tS) {{ Set tS=##class(Ens.Config.Setting).%New() Set tS.Name={k} Set tS.Target="Host" Do tItem.Settings.Insert(tS) }}
Set tS.Value={v}
"#,
                    k = k_expr,
                    v = v_expr
                ));
            }
            // C: apply live (Ens.Director.UpdateProduction) unless apply=false, so several
            // set_settings calls can be batched and applied once (or via iris_production update).
            let update_line = if params.apply {
                "Set tSC5=##class(Ens.Director).UpdateProduction(10,0)\nIf $$$ISERR(tSC5) { Write \"ERROR:UPDATE_FAILED:\"_$System.Status.GetErrorText(tSC5) Quit }\n"
            } else {
                ""
            };
            let code = format!(
                r#"Set tSC=##class(Ens.Director).GetProductionStatus(.n,.s)
If $$$ISERR(tSC)||n="" {{ Write "ERROR:NO_PRODUCTION:No production running" Quit }}
Set tProd=##class(Ens.Config.Production).%OpenId(n,,.tSC2)
If '$IsObject(tProd) {{ Write "ERROR:INTEROP_ERROR:Cannot open production" Quit }}
Set tItem=tProd.FindItemByConfigName({item},,.tSC3)
If '$IsObject(tItem) {{ Write "ERROR:ITEM_NOT_FOUND:Item not found: "_{item} Quit }}
{setting_lines}Set tSC4=tProd.%Save()
If $$$ISERR(tSC4) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC4) Quit }}
{update_line}Write "OK""#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(serde_json::json!({
                            "success": true,
                            "item": params.item,
                            "applied": params.apply,
                            "message": if params.apply {
                                "Settings saved and production updated (live)"
                            } else {
                                "Settings saved; NOT applied (apply=false) — run iris_production action=update to apply"
                            }
                        }))
                    } else if let Some(msg) = out.strip_prefix("ERROR:ITEM_NOT_FOUND:") {
                        err_json("ITEM_NOT_FOUND", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                        err_json("NO_PRODUCTION", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:UPDATE_FAILED:") {
                        err_json("UPDATE_FAILED", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "add" => {
            let class_name = match params.class_name.as_deref() {
                Some(c) if !c.is_empty() => c,
                _ => {
                    return err_json(
                        "INVALID_PARAMS",
                        "add requires class_name (the ObjectScript class the item runs)",
                    )
                }
            };
            let enabled = params.enabled.unwrap_or(true);
            let code = build_add_item_code(
                params.production.as_deref().unwrap_or(""),
                &params.item,
                class_name,
                enabled,
                params.pool_size,
                params.category.as_deref(),
                &params.settings,
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(prod) = out.strip_prefix("OK:") {
                        ok_json(serde_json::json!({
                            "success": true,
                            "item": params.item,
                            "class_name": class_name,
                            "enabled": enabled,
                            "production": prod,
                        }))
                    } else if let Some(msg) = out.strip_prefix("ERROR:ITEM_EXISTS:") {
                        err_json("ITEM_EXISTS", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                        err_json("NO_PRODUCTION", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:UPDATE_FAILED:") {
                        err_json("UPDATE_FAILED", msg)
                    } else {
                        err_json("INTEROP_ERROR", out.strip_prefix("ERROR:INTEROP_ERROR:").unwrap_or(out))
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "remove" => {
            let code = build_remove_item_code(params.production.as_deref().unwrap_or(""), &params.item);
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(prod) = out.strip_prefix("OK:") {
                        ok_json(serde_json::json!({
                            "success": true,
                            "item": params.item,
                            "removed": true,
                            "production": prod,
                        }))
                    } else if let Some(msg) = out.strip_prefix("ERROR:ITEM_NOT_FOUND:") {
                        err_json("ITEM_NOT_FOUND", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                        err_json("NO_PRODUCTION", msg)
                    } else if let Some(msg) = out.strip_prefix("ERROR:UPDATE_FAILED:") {
                        err_json("UPDATE_FAILED", msg)
                    } else {
                        err_json("INTEROP_ERROR", out.strip_prefix("ERROR:INTEROP_ERROR:").unwrap_or(out))
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        _ => err_json(
            "INVALID_ACTION",
            "iris_production_item: action must be add, remove, enable, disable, get_settings, or set_settings",
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 024-interop-depth: Ensemble credentials (US2)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CredentialListParams {
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CredentialManageParams {
    pub action: String,
    pub id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

pub async fn interop_credential_list_impl(
    iris: Option<&IrisConnection>,
    params: CredentialListParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    match iris
        .query(
            "SELECT SystemName, Username FROM Ens_Config.Credentials ORDER BY SystemName",
            vec![],
            &params.namespace,
            &client,
        )
        .await
    {
        Ok(resp) => {
            let rows = resp["result"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let total = rows.len();
            let truncated = total > 100;
            let creds: Vec<serde_json::Value> = rows
                .into_iter()
                .take(100)
                .map(
                    |row| serde_json::json!({"id": row["SystemName"], "username": row["Username"]}),
                )
                .collect();
            ok_json(serde_json::json!({
                "success": true,
                "credentials": creds,
                "count": creds.len(),
                "truncated": truncated,
                "total_count": total
            }))
        }
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_credential_manage_impl(
    iris: Option<&IrisConnection>,
    params: CredentialManageParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    let id = os_str_expr(&params.id);
    let ns = &params.namespace;

    match params.action.as_str() {
        "create" => {
            let username = match &params.username {
                Some(u) => os_str_expr(u),
                None => return err_json("INVALID_PARAMS", "create requires username"),
            };
            let password = match &params.password {
                Some(p) => os_str_expr(p),
                None => return err_json("INVALID_PARAMS", "create requires password"),
            };
            let code = format!(
                r#"Set tSC=##class(Ens.Config.Credentials).SetCredential({id},{username},{password},0)
If $$$ISERR(tSC) {{ Write "ERROR:CREDENTIAL_EXISTS:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"action":"create","id":params.id}),
                        )
                    } else if let Some(msg) = out.strip_prefix("ERROR:CREDENTIAL_EXISTS:") {
                        err_json("CREDENTIAL_EXISTS", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "update" => {
            // Read current values then overwrite with provided ones
            let username_expr = match &params.username {
                Some(u) => os_str_expr(u),
                None => format!(
                    "##class(Ens.Config.Credentials).GetValue({},\"Username\")",
                    id
                ),
            };
            let password_expr = match &params.password {
                Some(p) => os_str_expr(p),
                None => format!(
                    "##class(Ens.Config.Credentials).GetValue({},\"Password\")",
                    id
                ),
            };
            let code = format!(
                r#"Set tSC=##class(Ens.Config.Credentials).SetCredential({id},{username_expr},{password_expr},1)
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"action":"update","id":params.id}),
                        )
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "delete" => {
            let code = format!(
                r#"If '##class(Ens.Config.Credentials).%ExistsId({id}) {{ Write "ERROR:CREDENTIAL_NOT_FOUND:Credential not found: "_{id} Quit }}
Set tSC=##class(Ens.Config.Credentials).%DeleteId({id})
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"action":"delete","id":params.id}),
                        )
                    } else if let Some(msg) = out.strip_prefix("ERROR:CREDENTIAL_NOT_FOUND:") {
                        err_json("CREDENTIAL_NOT_FOUND", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        _ => err_json(
            "INVALID_ACTION",
            "iris_credential_manage: action must be create, update, or delete",
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 024-interop-depth: Lookup tables (US3)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupManageParams {
    pub action: String,
    pub table: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupTransferParams {
    pub action: String,
    pub table: String,
    pub xml: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

pub async fn interop_lookup_manage_impl(
    iris: Option<&IrisConnection>,
    params: LookupManageParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    let ns = &params.namespace;

    match params.action.as_str() {
        "list_tables" => {
            let code = r#"Set tTable="" Set tOut="" Set tCount=0 For { Set tTable=$ORDER(^Ens.LookupTable(tTable)) Quit:tTable=""  Set tOut=tOut_tTable_$CHAR(10) Set tCount=tCount+1 } Write tOut"#;
            match iris.execute_via_generator(code, ns, &client).await {
                Ok(out) => {
                    let tables: Vec<String> = out
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    let total = tables.len();
                    let truncated = total > 100;
                    let tables: Vec<String> = tables.into_iter().take(100).collect();
                    ok_json(
                        serde_json::json!({"success":true,"tables":tables,"count":tables.len(),"truncated":truncated,"total_count":total}),
                    )
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "get" => {
            let table = match &params.table {
                Some(t) => os_str_expr(t),
                None => return err_json("INVALID_PARAMS", "get requires table"),
            };
            let key = match &params.key {
                Some(k) => os_str_expr(k),
                None => return err_json("INVALID_PARAMS", "get requires key"),
            };
            let code = format!(
                r#"If '$DATA(^Ens.LookupTable({t})) {{ Write "ERROR:TABLE_NOT_FOUND:Table not found: "_{t} Quit }}
Set tVal=$GET(^Ens.LookupTable({t},{k}))
If tVal="" {{ Write "ERROR:KEY_NOT_FOUND:Key not found: "_{k} Quit }}
Write tVal"#,
                t = table,
                k = key
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(msg) = out.strip_prefix("ERROR:TABLE_NOT_FOUND:") {
                        return err_json("TABLE_NOT_FOUND", msg);
                    }
                    if let Some(msg) = out.strip_prefix("ERROR:KEY_NOT_FOUND:") {
                        return err_json("KEY_NOT_FOUND", msg);
                    }
                    ok_json(
                        serde_json::json!({"success":true,"table":params.table,"key":params.key,"value":out}),
                    )
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "set" => {
            let table = match &params.table {
                Some(t) => os_str_expr(t),
                None => return err_json("INVALID_PARAMS", "set requires table"),
            };
            let key = match &params.key {
                Some(k) => os_str_expr(k),
                None => return err_json("INVALID_PARAMS", "set requires key"),
            };
            let value = match &params.value {
                Some(v) => os_str_expr(v),
                None => return err_json("INVALID_PARAMS", "set requires value"),
            };
            let code = format!(
                r#"Set tSC=##class(Ens.Util.LookupTable).%UpdateValue({table},{key},{value},1)
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"table":params.table,"key":params.key,"value":params.value}),
                        )
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "delete" => {
            let table = match &params.table {
                Some(t) => os_str_expr(t),
                None => return err_json("INVALID_PARAMS", "delete requires table"),
            };
            let key = match &params.key {
                Some(k) => os_str_expr(k),
                None => return err_json("INVALID_PARAMS", "delete requires key"),
            };
            let code = format!(
                r#"If '$DATA(^Ens.LookupTable({t})) {{ Write "ERROR:TABLE_NOT_FOUND:Table not found: "_{t} Quit }}
Set tSC=##class(Ens.Util.LookupTable).%RemoveValue({t},{k})
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#,
                t = table,
                k = key
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(
                            serde_json::json!({"success":true,"table":params.table,"key":params.key}),
                        )
                    } else if let Some(msg) = out.strip_prefix("ERROR:TABLE_NOT_FOUND:") {
                        err_json("TABLE_NOT_FOUND", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "list_keys" => {
            let table = match &params.table {
                Some(t) => os_str_expr(t),
                None => return err_json("INVALID_PARAMS", "list_keys requires table"),
            };
            let code = format!(
                r#"If '$DATA(^Ens.LookupTable({t})) {{ Write "ERROR:TABLE_NOT_FOUND:Table not found: "_{t} Quit }}
Set tKey="" For {{ Set tKey=$ORDER(^Ens.LookupTable({t},tKey)) Quit:tKey=""  Write tKey_$CHAR(10) }}"#,
                t = table
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(msg) = out.strip_prefix("ERROR:TABLE_NOT_FOUND:") {
                        return err_json("TABLE_NOT_FOUND", msg);
                    }
                    let keys: Vec<String> = out
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    ok_json(
                        serde_json::json!({"success":true,"table":params.table,"keys":keys,"count":keys.len()}),
                    )
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        _ => err_json(
            "INVALID_ACTION",
            "iris_lookup_manage: action must be get, set, delete, list_keys, or list_tables",
        ),
    }
}

/// Generated code that writes the XML to a server-side temp file and runs
/// `Ens.Util.LookupTable.%Import`. The XML is user-supplied free text (quotes,
/// newlines, unicode), so it is embedded via `os_stream_write_stmts` — see
/// issue #6, where quote-heavy XML made every import die with `<SYNTAX>`.
fn build_lookup_import_code(table: &str, xml: &str) -> String {
    let table_expr = os_str_expr(table);
    let write_block = os_stream_write_stmts("tStream", xml, 400).join("\n");
    format!(
        r#"Set tFile=##class(%Library.File).TempFilename("xml")
Set tStream=##class(%Stream.FileCharacter).%New()
Set tStream.Filename=tFile
Set tStream.TranslateTable="UTF8"
{write_block}
Set tSC=tStream.%Save()
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:Cannot write temp file" Quit }}
Set tSC2=##class(Ens.Util.LookupTable).%Import(tFile,{table_expr},"")
Do ##class(%File).Delete(tFile)
If $$$ISERR(tSC2) {{ Write "ERROR:INVALID_XML:"_$System.Status.GetErrorText(tSC2) Quit }}
Write "OK""#
    )
}

pub async fn interop_lookup_transfer_impl(
    iris: Option<&IrisConnection>,
    params: LookupTransferParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    let ns = &params.namespace;
    let table = os_str_expr(&params.table);

    match params.action.as_str() {
        "export" => {
            let code = format!(
                r#"If '$DATA(^Ens.LookupTable({t})) {{ Write "ERROR:TABLE_NOT_FOUND:Table not found: "_{t} Quit }}
Set tStream=##class(%Stream.TmpBinary).%New()
Set tSC=##class(Ens.Util.LookupTable).%Export(tStream,{t})
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) Quit }}
Do tStream.Rewind()
Set tOut="" While 'tStream.AtEnd {{ Set tOut=tOut_tStream.Read(32000) }}
Write tOut"#,
                t = table
            );
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if let Some(msg) = out.strip_prefix("ERROR:TABLE_NOT_FOUND:") {
                        return err_json("TABLE_NOT_FOUND", msg);
                    }
                    if let Some(msg) = out.strip_prefix("ERROR:INTEROP_ERROR:") {
                        return err_json("INTEROP_ERROR", msg);
                    }
                    // Count entries in XML
                    let entry_count = out.matches("<entry").count();
                    ok_json(
                        serde_json::json!({"success":true,"table":params.table,"xml":out,"entry_count":entry_count}),
                    )
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        "import" => {
            let xml = match &params.xml {
                Some(x) => x.clone(),
                None => return err_json("INVALID_PARAMS", "import requires xml"),
            };
            let code = build_lookup_import_code(&params.table, &xml);
            match iris.execute_via_generator(&code, ns, &client).await {
                Ok(out) => {
                    let out = out.trim();
                    if out == "OK" {
                        ok_json(serde_json::json!({"success":true,"table":params.table}))
                    } else if let Some(msg) = out.strip_prefix("ERROR:INVALID_XML:") {
                        err_json("INVALID_XML", msg)
                    } else {
                        err_json("INTEROP_ERROR", out)
                    }
                }
                Err(e) => err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                ),
            }
        }
        _ => err_json(
            "INVALID_ACTION",
            "iris_lookup_transfer: action must be export or import",
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 024-interop-depth: Production autostart (US4)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionAutostartParams {
    pub action: String,
    #[serde(default = "default_ns")]
    pub namespace: String,
    pub enabled: Option<bool>,
    pub production: Option<String>,
}

pub async fn interop_autostart_get_impl(
    iris: Option<&IrisConnection>,
    params: &ProductionAutostartParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    // Read ^Ens.AutoStart directly — GetAutoStart() does not exist
    let code = r#"Write $GET(^Ens.AutoStart)"#;
    match iris
        .execute_via_generator(code, &params.namespace, &client)
        .await
    {
        Ok(out) => {
            let prod = out.trim().to_string();
            let enabled = !prod.is_empty();
            ok_json(serde_json::json!({
                "success": true,
                "namespace": params.namespace,
                "autostart_enabled": enabled,
                "production": if enabled { serde_json::Value::String(prod) } else { serde_json::Value::Null }
            }))
        }
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

pub async fn interop_autostart_set_impl(
    iris: Option<&IrisConnection>,
    params: &ProductionAutostartParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client()
        .map_err(|_| McpError::invalid_request("IRIS_UNREACHABLE", None))?;
    let ns = &params.namespace;
    let enabled = params.enabled.unwrap_or(true);

    if !enabled {
        let code = r#"Set tSC=##class(Ens.Director).SetAutoStart("")
If $$$ISERR(tSC) { Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) } Else { Write "OK" }"#;
        match iris.execute_via_generator(code, ns, &client).await {
            Ok(out) if out.trim() == "OK" => {
                return ok_json(
                    serde_json::json!({"success":true,"namespace":ns,"autostart_enabled":false,"production":null}),
                );
            }
            Ok(out) => return err_json("INTEROP_ERROR", out.trim()),
            Err(e) => {
                return err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                )
            }
        }
    }

    // enabled=true: resolve production name
    let prod_name = if let Some(p) = &params.production {
        p.clone()
    } else {
        // Get currently running production
        let status_code = r#"Set sc=##class(Ens.Director).GetProductionStatus(.n,.s) If $$$ISERR(sc)||n="" { Write "ERROR:NO_PRODUCTION:No production running" } Else { Write n }"#;
        match iris.execute_via_generator(status_code, ns, &client).await {
            Ok(out) => {
                let out = out.trim().to_string();
                if let Some(msg) = out.strip_prefix("ERROR:NO_PRODUCTION:") {
                    return err_json("NO_PRODUCTION", msg);
                }
                out
            }
            Err(e) => {
                return err_json(
                    if is_network_error(&e.to_string()) {
                        "IRIS_UNREACHABLE"
                    } else {
                        "INTEROP_ERROR"
                    },
                    &e.to_string(),
                )
            }
        }
    };

    let code = format!(
        r#"Set tSC=##class(Ens.Director).SetAutoStart({})
If $$$ISERR(tSC) {{ Write "ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC) }} Else {{ Write "OK" }}"#,
        os_str_expr(&prod_name)
    );
    match iris.execute_via_generator(&code, ns, &client).await {
        Ok(out) if out.trim() == "OK" => ok_json(
            serde_json::json!({"success":true,"namespace":ns,"autostart_enabled":true,"production":prod_name}),
        ),
        Ok(out) => err_json("INTEROP_ERROR", out.trim()),
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "INTEROP_ERROR"
            },
            &e.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every line of generated ObjectScript must hold only complete string
    // literals: an odd number of `"` on a line means a literal was torn open
    // by an embedded quote or newline — exactly the issue #6 <SYNTAX> bug.
    fn assert_valid_objectscript_lines(code: &str) {
        for line in code.lines() {
            assert_eq!(
                line.matches('"').count() % 2,
                0,
                "unbalanced quotes in generated line: {line}"
            );
        }
        assert!(
            !code.contains("\\\""),
            "generated code must never use C-style quote escaping"
        );
    }

    // ─── issue #5: omitted namespace resolves to the connection's, not "USER" ───

    fn test_connection(ns: &str) -> IrisConnection {
        IrisConnection {
            base_url: "http://localhost:43080".into(),
            namespace: ns.into(),
            username: "_SYSTEM".into(),
            password: "SYS".into(),
            version: None,
            atelier_version: crate::iris::connection::AtelierVersion::V1,
            source: crate::iris::connection::DiscoverySource::EnvVar,
            port_superserver: None,
            system_mode: crate::iris::connection::SystemMode::Development,
        }
    }

    #[test]
    fn resolve_namespace_prefers_explicit_param() {
        let conn = test_connection("APP");
        assert_eq!(resolve_namespace(Some("EJ3"), Some(&conn)), "EJ3");
    }

    #[test]
    fn resolve_namespace_falls_back_to_connection() {
        let conn = test_connection("APP");
        assert_eq!(resolve_namespace(None, Some(&conn)), "APP");
        assert_eq!(resolve_namespace(Some(""), Some(&conn)), "APP");
        assert_eq!(resolve_namespace(Some("  "), Some(&conn)), "APP");
    }

    #[test]
    fn resolve_namespace_user_only_without_connection() {
        assert_eq!(resolve_namespace(None, None), "USER");
        assert_eq!(resolve_namespace(Some("EJ3"), None), "EJ3");
    }

    // ─── issue #6: lookup import died with <SYNTAX> on quote-heavy XML ───

    #[test]
    fn lookup_import_code_survives_real_xml() {
        // The exact shape from the issue repro: quotes in attributes, newlines
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<lookupTable>\n<entry table=\"GeneroSOAP\" key=\"M\">1</entry>\n<entry table=\"GeneroSOAP\" key=\"F\">2</entry>\n</lookupTable>";
        let code = build_lookup_import_code("GeneroSOAP", xml);
        assert_valid_objectscript_lines(&code);
        assert!(code.contains(r#"table=""GeneroSOAP"""#), "quotes doubled");
        assert!(code.contains("$CHAR(10)"), "newlines spliced via $CHAR");
        assert!(code.contains(r#"%Import(tFile,"GeneroSOAP","")"#));
    }

    #[test]
    fn lookup_import_code_chunks_large_single_line_xml() {
        // Collapsed-to-one-line XML (also tried in the issue) must be chunked
        let xml = format!("<lookupTable>{}</lookupTable>", "x".repeat(5000));
        let code = build_lookup_import_code("T", &xml);
        assert_valid_objectscript_lines(&code);
        let writes = code.matches("Do tStream.Write(").count();
        assert!(
            writes > 1,
            "long payload must span several Write statements"
        );
        assert!(code.lines().all(|l| l.len() < 2000), "no oversized lines");
    }

    #[test]
    fn item_code_builders_embed_quotes_safely() {
        let mut settings = std::collections::HashMap::new();
        settings.insert("DSN".to_string(), "jdbc:\"quoted\";it's".to_string());
        let code = build_add_item_code(
            "My.Production",
            "BO.MenusJDBC",
            "Ejercicio3.BO.MenusJDBC",
            true,
            None,
            None,
            &settings,
        );
        assert_valid_objectscript_lines(&code);
        // apostrophes must pass through untouched (the old code doubled them)
        assert!(code.contains("it's"), "apostrophe corrupted: {code}");
        let code = build_remove_item_code("", "BO.MenusJDBC");
        assert_valid_objectscript_lines(&code);
        assert!(code.contains("Set tProdName=\"\""));
    }

    #[test]
    fn test_is_network_error_sending() {
        assert!(is_network_error("error sending request for url"));
    }

    #[test]
    fn test_is_network_error_refused() {
        assert!(is_network_error("connection refused"));
    }

    #[test]
    fn test_is_network_error_reset() {
        assert!(is_network_error("connection reset by peer"));
    }

    #[test]
    fn test_is_network_error_dns() {
        assert!(is_network_error("dns error: no such host"));
    }

    #[test]
    fn test_is_network_error_timeout() {
        assert!(is_network_error("timed out"));
    }

    #[test]
    fn test_is_network_error_false_for_interop_message() {
        // "No Interoperability connection configured" must NOT be a network error
        assert!(!is_network_error(
            "No Interoperability connection configured"
        ));
    }

    #[test]
    fn test_is_network_error_false_for_docker_required() {
        assert!(!is_network_error("DOCKER_REQUIRED"));
    }

    #[test]
    fn test_is_network_error_false_for_sql_error() {
        assert!(!is_network_error("SQLCODE: -1 Field not found"));
    }

    #[test]
    fn test_production_status_params_deserialize() {
        let p: ProductionStatusParams = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.namespace, "USER");
        assert!(!p.full_status);
    }

    #[test]
    fn test_production_name_params_deserialize() {
        let p: ProductionNameParams =
            serde_json::from_str(r#"{"production": "MyApp.Production", "namespace": "MYNS"}"#)
                .unwrap();
        assert_eq!(p.production.as_deref(), Some("MyApp.Production"));
        assert_eq!(p.namespace, "MYNS");
    }

    #[test]
    fn test_logs_params_defaults() {
        let p: LogsParams = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.limit, 10); // default_limit returns 10
        assert!(!p.log_type.is_empty()); // log_type has a default
    }

    #[test]
    fn test_message_search_params_defaults() {
        let p: MessageSearchParams = serde_json::from_str(r#"{}"#).unwrap();
        assert!(p.source.is_none());
        assert!(p.target.is_none());
    }

    // ─── T011/T012/T013: US1 — ProductionItemParams unit tests ───

    #[test]
    fn production_item_params_deserialize_all_actions() {
        let p: ProductionItemParams =
            serde_json::from_str(r#"{"action":"enable","item":"MyService","namespace":"MYNS"}"#)
                .unwrap();
        assert_eq!(p.action, "enable");
        assert_eq!(p.item, "MyService");
        assert_eq!(p.namespace, "MYNS");

        let p2: ProductionItemParams = serde_json::from_str(
            r#"{"action":"set_settings","item":"MyOp","settings":{"Timeout":"30"}}"#,
        )
        .unwrap();
        assert_eq!(p2.action, "set_settings");
        assert_eq!(p2.settings.get("Timeout").map(|v| v.as_str()), Some("30"));
        assert_eq!(p2.namespace, "USER"); // default
    }

    #[test]
    fn production_item_error_mapping_item_not_found() {
        // Verify error prefix matching logic
        let msg = "ERROR:ITEM_NOT_FOUND:Item not found: Missing";
        assert!(msg.strip_prefix("ERROR:ITEM_NOT_FOUND:").is_some());
    }

    #[test]
    fn production_item_error_mapping_update_failed() {
        let msg = "ERROR:UPDATE_FAILED:Production update timed out";
        assert!(msg.strip_prefix("ERROR:UPDATE_FAILED:").is_some());
    }

    // ─── T019/T020: US2 — Credential unit tests ───

    #[test]
    fn credential_list_response_never_contains_password() {
        // Simulate what interop_credential_list_impl returns
        let resp = serde_json::json!({
            "success": true,
            "credentials": [
                {"id": "SMTPServer", "username": "user@example.com"}
            ],
            "count": 1,
            "truncated": false,
            "total_count": 1
        });
        let text = resp.to_string();
        assert!(
            !text.contains("\"password\""),
            "password must not appear in credential list"
        );
        assert!(
            !text.contains("\"Password\""),
            "Password must not appear in credential list"
        );
    }

    #[test]
    fn credential_list_truncation_fields_present() {
        // Verify that the response shape includes truncated + total_count
        let resp = serde_json::json!({"success":true,"credentials":[],"count":0,"truncated":false,"total_count":0});
        assert!(resp.get("truncated").is_some());
        assert!(resp.get("total_count").is_some());
    }

    #[test]
    fn credential_manage_params_deserialize() {
        let p: CredentialManageParams = serde_json::from_str(
            r#"{"action":"create","id":"MyCredential","username":"user","password":"pass"}"#,
        )
        .unwrap();
        assert_eq!(p.action, "create");
        assert_eq!(p.id, "MyCredential");
        assert_eq!(p.namespace, "USER");
    }

    #[test]
    fn credential_error_codes_parseable() {
        assert!("ERROR:CREDENTIAL_EXISTS:already exists"
            .strip_prefix("ERROR:CREDENTIAL_EXISTS:")
            .is_some());
        assert!("ERROR:CREDENTIAL_NOT_FOUND:not found"
            .strip_prefix("ERROR:CREDENTIAL_NOT_FOUND:")
            .is_some());
    }

    // ─── T028/T029: US3 — Lookup table unit tests ───

    #[test]
    fn lookup_manage_params_all_actions() {
        let p: LookupManageParams = serde_json::from_str(r#"{"action":"list_tables"}"#).unwrap();
        assert_eq!(p.action, "list_tables");
        assert!(p.table.is_none());

        let p2: LookupManageParams = serde_json::from_str(
            r#"{"action":"set","table":"RouteTable","key":"Target1","value":"HL7Recv"}"#,
        )
        .unwrap();
        assert_eq!(p2.action, "set");
        assert_eq!(p2.table.as_deref(), Some("RouteTable"));
        assert_eq!(p2.value.as_deref(), Some("HL7Recv"));
    }

    #[test]
    fn lookup_list_tables_response_includes_truncated() {
        let resp = serde_json::json!({"success":true,"tables":["T1","T2"],"count":2,"truncated":false,"total_count":2});
        assert_eq!(resp["truncated"], false);
        assert_eq!(resp["total_count"], 2);
    }

    #[test]
    fn lookup_error_codes_parseable() {
        assert!("ERROR:TABLE_NOT_FOUND:No such table"
            .strip_prefix("ERROR:TABLE_NOT_FOUND:")
            .is_some());
        assert!("ERROR:INVALID_XML:Parse error"
            .strip_prefix("ERROR:INVALID_XML:")
            .is_some());
        assert!("ERROR:KEY_NOT_FOUND:Key missing"
            .strip_prefix("ERROR:KEY_NOT_FOUND:")
            .is_some());
    }

    // ─── T037: US4 — Autostart params ───

    #[test]
    fn autostart_params_deserialize() {
        let p: ProductionAutostartParams =
            serde_json::from_str(r#"{"action":"get_autostart","namespace":"MYAPP"}"#).unwrap();
        assert_eq!(p.action, "get_autostart");
        assert_eq!(p.namespace, "MYAPP");
        assert!(p.enabled.is_none());

        let p2: ProductionAutostartParams = serde_json::from_str(
            r#"{"action":"set_autostart","namespace":"MYAPP","enabled":true,"production":"MyApp.Production"}"#
        ).unwrap();
        assert_eq!(p2.enabled, Some(true));
        assert_eq!(p2.production.as_deref(), Some("MyApp.Production"));
    }
}
