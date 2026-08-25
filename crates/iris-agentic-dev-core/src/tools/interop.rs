use crate::iris::connection::IrisConnection;
use crate::objectscript::{os_str_expr, os_stream_write_stmts};
use rmcp::{model::*, ErrorData as McpError};
use schemars::JsonSchema;
use serde::Deserialize;

fn ok_json(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}
fn err_json(code: &str, msg: &str) -> Result<CallToolResult, McpError> {
    crate::tools::envelope::fail(code, msg)
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
    Some(crate::tools::envelope::fail_with(
        "NAMESPACE_NOT_INTEROP",
        &format!(
            "Namespace '{ns}' has no Interoperability enabled — the Ens.* classes and \
             Ens_* tables do not exist there, so interop tools cannot run in it."
        ),
        serde_json::json!({
            "hint": hint,
            "namespace": ns,
            "interop_namespaces": available
                .map(|l| l.split(',').map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_default(),
        }),
    ))
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
    /// Issue #4: body-class join — search on what the message SAYS. The body
    /// class whose table is joined on h.MessageBodyId = r.ID; required when
    /// body_where / body_select are used.
    pub body_class: Option<String>,
    /// SQL WHERE fragment over the body table's columns, e.g. "PacienteId = '4003'".
    pub body_where: Option<String>,
    /// Body columns to return alongside the header fields.
    #[serde(default)]
    pub body_select: Vec<String>,
    /// Issue #4: search by indexed Search Table field.
    pub search_table: Option<SearchTableFilter>,
}

/// Search-Table filter (issue #4). Every subclass of a virtual-document search
/// table shares its BASE extent's SQL table (rows are told apart by PropId), so
/// `class` scopes props via Ens_Config.SearchTableProp.ClassDerivation — never
/// via a table name of its own.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchTableFilter {
    /// Search-table subclass (e.g. "Hospital.Search.HL7") — scopes props to
    /// the ones that subclass defines or inherits.
    pub class: Option<String>,
    /// Base extent class (default "EnsLib.HL7.SearchTable"; other families:
    /// EnsLib.EDI.X12.SearchTable, EnsLib.EDI.XML.SearchTable, …, or an
    /// Ens.CustomSearchTable subclass, which is its own extent).
    pub extent: Option<String>,
    /// Search Table property name, e.g. "PatientID".
    pub prop: String,
    /// Exact PropValue match. Exactly one of value / value_like is required.
    pub value: Option<String>,
    /// LIKE pattern for PropValue, e.g. "AMOX%".
    pub value_like: Option<String>,
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
                // "No production running" answered to a STATUS question is a normal
                // state, not a failure (issue #32) — fresh instances live here. Genuine
                // failures (unreachable, non-interop ns) still route through err_json.
                Err(_) => ok_json(serde_json::json!({
                    "success": true,
                    "state": "stopped",
                    "production": serde_json::Value::Null,
                    "note": "No production is running in this namespace",
                })),
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

/// Header-level filters, prefixed so they survive a JOIN (`pfx` = "h." there,
/// "" for the plain query).
fn header_filters(params: &MessageSearchParams, pfx: &str) -> Vec<String> {
    let mut filters = vec![];
    if let Some(src) = &params.source {
        filters.push(format!(
            "{pfx}SourceConfigName = '{}'",
            src.replace('\'', "''")
        ));
    }
    if let Some(tgt) = &params.target {
        filters.push(format!(
            "{pfx}TargetConfigName = '{}'",
            tgt.replace('\'', "''")
        ));
    }
    if let Some(cls) = &params.class_name {
        filters.push(format!(
            "{pfx}MessageBodyClassName = '{}'",
            cls.replace('\'', "''")
        ));
    }
    // session_id / since_id are numeric (i64) — safe to inline.
    if let Some(sid) = params.session_id {
        filters.push(format!("{pfx}SessionId = {}", sid));
    }
    if let Some(since) = params.since_id {
        filters.push(format!("{pfx}ID > {}", since));
    }
    filters
}

const HEADER_COLS: &str =
    "ID, TimeCreated, SourceConfigName, TargetConfigName, MessageBodyClassName, SessionId, Status";

/// A plausible SQL column/identifier — the only thing body_select accepts.
fn is_sql_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '%')
}

/// Body-class join (issue #4). The join also pins h.MessageBodyClassName to
/// the body class: MessageBodyId is only unique per body table, so without it
/// same-numbered rows of OTHER body classes would match.
fn build_body_join_sql(
    limit: u32,
    mut filters: Vec<String>,
    body_class: &str,
    body_table: &str,
    body_where: Option<&str>,
    body_select: &[String],
) -> String {
    filters.push(format!(
        "h.MessageBodyClassName = '{}'",
        body_class.replace('\'', "''")
    ));
    if let Some(w) = body_where {
        filters.push(format!("({w})"));
    }
    let header_cols = HEADER_COLS
        .split(", ")
        .map(|c| format!("h.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body_cols = body_select
        .iter()
        .map(|c| format!(", r.{c}"))
        .collect::<String>();
    format!(
        "SELECT TOP {limit} {header_cols}{body_cols} FROM Ens.MessageHeader h JOIN {body_table} r ON h.MessageBodyId = r.ID WHERE {} ORDER BY h.ID DESC",
        filters.join(" AND ")
    )
}

/// Search-Table join (issue #4) — the canonical shape from the issue: DocId IS
/// MessageBodyId, and PropId must be resolved through Ens_Config.SearchTableProp
/// first (PropId is only unique within one extent).
fn build_search_table_sql(
    limit: u32,
    mut filters: Vec<String>,
    extent_table: &str,
    prop_ids: &[i64],
    value: Option<&str>,
    value_like: Option<&str>,
) -> String {
    let ids = prop_ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    filters.push(format!("st.PropId IN ({ids})"));
    if let Some(v) = value {
        filters.push(format!("st.PropValue = '{}'", v.replace('\'', "''")));
    } else if let Some(v) = value_like {
        filters.push(format!("st.PropValue LIKE '{}'", v.replace('\'', "''")));
    }
    let header_cols = HEADER_COLS
        .split(", ")
        .map(|c| format!("h.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT TOP {limit} {header_cols}, st.PropValue FROM Ens.MessageHeader h JOIN {extent_table} st ON st.DocId = h.MessageBodyId WHERE {} ORDER BY h.ID DESC",
        filters.join(" AND ")
    )
}

/// SQL projection of a class via the dictionary (handles SqlTableName
/// overrides the dots→underscores rule would get wrong). Ok(None) = class not
/// compiled in that namespace.
async fn resolve_sql_table(
    iris: &IrisConnection,
    ns: &str,
    client: &reqwest::Client,
    class: &str,
) -> Result<Option<String>, String> {
    let sql = format!(
        "SELECT SqlSchemaName, SqlTableName FROM %Dictionary.CompiledClass WHERE Name = '{}'",
        class.replace('\'', "''")
    );
    let resp = iris
        .query(&sql, vec![], ns, client)
        .await
        .map_err(|e| e.to_string())?;
    let row = &resp["result"]["content"][0];
    match (row["SqlSchemaName"].as_str(), row["SqlTableName"].as_str()) {
        (Some(s), Some(t)) if !s.is_empty() && !t.is_empty() => Ok(Some(format!("{s}.{t}"))),
        _ => Ok(None),
    }
}

/// Distinct body classes present in the namespace — the discovery hint when a
/// body_class is missing or wrong.
async fn list_body_classes(
    iris: &IrisConnection,
    ns: &str,
    client: &reqwest::Client,
) -> Option<Vec<String>> {
    // %EXACT beats the column's SQLUPPER collation — the hint must show the
    // case-sensitive class name the body_class parameter needs.
    let sql = "SELECT DISTINCT TOP 20 %EXACT(MessageBodyClassName) AS MessageBodyClassName FROM Ens.MessageHeader WHERE MessageBodyClassName IS NOT NULL ORDER BY 1";
    let resp = iris.query(sql, vec![], ns, client).await.ok()?;
    let names: Vec<String> = resp["result"]["content"]
        .as_array()?
        .iter()
        .filter_map(|r| r["MessageBodyClassName"].as_str())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
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
    // A6: query the requested production namespace, not the connection default.
    let ns = params
        .namespace
        .as_deref()
        .unwrap_or(iris.namespace.as_str());
    let net_err = |e: &str| {
        if is_network_error(e) {
            "IRIS_UNREACHABLE"
        } else {
            "INTEROP_ERROR"
        }
    };

    let body_mode = params.body_class.is_some()
        || params.body_where.is_some()
        || !params.body_select.is_empty();
    if body_mode && params.search_table.is_some() {
        return err_json(
            "INVALID_PARAMS",
            "body_class/body_where and search_table are separate search modes — pass one or the other",
        );
    }

    // ── Mode 1 (issue #4): body-class join ───────────────────────────────
    if body_mode {
        let body_class = match &params.body_class {
            Some(c) => c.clone(),
            None => {
                let known = list_body_classes(iris, ns, &client).await;
                let hint = match known {
                    Some(names) => format!(
                        "body_where/body_select need body_class. Body classes present in '{}': {}.",
                        ns,
                        names.join(", ")
                    ),
                    None => format!(
                        "body_where/body_select need body_class (no messages found in '{}' to list candidates from).",
                        ns
                    ),
                };
                return crate::tools::envelope::fail_with(
                    "INVALID_PARAMS",
                    "body_where/body_select require body_class",
                    serde_json::json!({"hint": hint}),
                );
            }
        };
        if let Some(w) = &params.body_where {
            if w.contains(';') {
                return err_json(
                    "INVALID_PARAMS",
                    "body_where must be a WHERE fragment, not a statement (no ';')",
                );
            }
        }
        if let Some(bad) = params.body_select.iter().find(|c| !is_sql_identifier(c)) {
            return err_json(
                "INVALID_PARAMS",
                &format!("body_select entries must be plain column names; '{bad}' is not"),
            );
        }
        let body_table = match resolve_sql_table(iris, ns, &client, &body_class).await {
            Err(e) => return err_json(net_err(&e), &e),
            Ok(Some(t)) => t,
            Ok(None) => {
                let known = list_body_classes(iris, ns, &client).await;
                let hint = match known {
                    Some(names) => format!(
                        "Body classes actually present in '{}': {}. Class names are case-sensitive and must be compiled in the namespace.",
                        ns,
                        names.join(", ")
                    ),
                    None => "Class names are case-sensitive and must be compiled in the namespace."
                        .to_string(),
                };
                return crate::tools::envelope::fail_with(
                    "BODY_CLASS_NOT_FOUND",
                    &format!(
                        "Class '{}' is not compiled in namespace '{}'",
                        body_class, ns
                    ),
                    serde_json::json!({"hint": hint, "body_class": body_class, "namespace": ns}),
                );
            }
        };
        let sql = build_body_join_sql(
            params.limit,
            header_filters(&params, "h."),
            &body_class,
            &body_table,
            params.body_where.as_deref(),
            &params.body_select,
        );
        return match iris.query(&sql, vec![], ns, &client).await {
            Ok(resp) => ok_json(serde_json::json!({
                "success": true,
                "messages": resp["result"]["content"],
                "count": resp["result"]["content"].as_array().map(|a| a.len()).unwrap_or(0),
                "body_table": body_table,
                "sql": sql,
            })),
            Err(e) => crate::tools::envelope::fail_with(
                net_err(&e.to_string()),
                &e.to_string(),
                serde_json::json!({"hint": format!(
                    "body_where/body_select must use the SQL column names of {} (the SQL projection of {}). Check them with iris_table_info.",
                    body_table, body_class
                ), "sql": sql}),
            ),
        };
    }

    // ── Mode 2 (issue #4): Search-Table filter ───────────────────────────
    if let Some(st) = &params.search_table {
        if st.value.is_some() == st.value_like.is_some() {
            return err_json(
                "INVALID_PARAMS",
                "search_table needs exactly one of value (exact) or value_like (LIKE pattern)",
            );
        }
        // Resolve the base extent: explicit > derived from `class` > HL7 default.
        let extent = match (&st.extent, &st.class) {
            (Some(e), _) => e.clone(),
            (None, Some(class)) => {
                let c = class.replace('\'', "''");
                let sql = format!(
                    "SELECT TOP 1 ClassExtent FROM Ens_Config.SearchTableProp WHERE ClassDerivation = '{c}' OR ClassDerivation LIKE '{c}~%'"
                );
                match iris.query(&sql, vec![], ns, &client).await {
                    Err(e) => return err_json(net_err(&e.to_string()), &e.to_string()),
                    Ok(resp) => match resp["result"]["content"][0]["ClassExtent"].as_str() {
                        Some(e) => e.to_string(),
                        None => {
                            return crate::tools::envelope::fail_with(
                                "SEARCH_TABLE_NOT_FOUND",
                                &format!(
                                    "No Search Table class '{}' is registered in namespace '{}'",
                                    class, ns
                                ),
                                serde_json::json!({"hint": "The class must extend a search-table family (EnsLib.HL7.SearchTable, …) and be compiled; registration happens on first index build. Check Ens_Config.SearchTableProp."}),
                            )
                        }
                    },
                }
            }
            (None, None) => "EnsLib.HL7.SearchTable".to_string(),
        };
        // Resolve prop name -> PropId set within the extent (PropId is only
        // unique per extent; `class` additionally scopes via ClassDerivation).
        let e_esc = extent.replace('\'', "''");
        let scope = match &st.class {
            Some(class) => {
                let c = class.replace('\'', "''");
                format!(" AND (ClassDerivation = '{c}' OR ClassDerivation LIKE '{c}~%')")
            }
            None => String::new(),
        };
        let prop_sql = format!(
            "SELECT PropId FROM Ens_Config.SearchTableProp WHERE ClassExtent = '{e_esc}' AND Name = '{}'{scope}",
            st.prop.replace('\'', "''")
        );
        let prop_ids: Vec<i64> = match iris.query(&prop_sql, vec![], ns, &client).await {
            Err(e) => return err_json(net_err(&e.to_string()), &e.to_string()),
            Ok(resp) => resp["result"]["content"]
                .as_array()
                .map(|rows| rows.iter().filter_map(|r| r["PropId"].as_i64()).collect())
                .unwrap_or_default(),
        };
        if prop_ids.is_empty() {
            // Issue #4 hint: list what IS searchable instead of returning empty.
            let names_sql = format!(
                "SELECT DISTINCT Name FROM Ens_Config.SearchTableProp WHERE ClassExtent = '{e_esc}' ORDER BY Name"
            );
            let names = iris
                .query(&names_sql, vec![], ns, &client)
                .await
                .ok()
                .and_then(|resp| {
                    resp["result"]["content"].as_array().map(|rows| {
                        rows.iter()
                            .filter_map(|r| r["Name"].as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            return crate::tools::envelope::fail_with(
                "SEARCH_PROP_NOT_FOUND",
                &format!(
                    "No Search Table property '{}' in extent '{}'",
                    st.prop, extent
                ),
                serde_json::json!({
                    "hint": if names.is_empty() {
                        format!("Extent '{}' has no registered properties in namespace '{}' — is a SearchTableClass configured on any item?", extent, ns)
                    } else {
                        format!("Searchable properties of '{}': {}.", extent, names.join(", "))
                    },
                    "available_props": names,
                }),
            );
        }
        let extent_table = match resolve_sql_table(iris, ns, &client, &extent).await {
            Err(e) => return err_json(net_err(&e), &e),
            Ok(Some(t)) => t,
            Ok(None) => {
                return err_json(
                    "SEARCH_TABLE_NOT_FOUND",
                    &format!(
                        "Extent class '{}' is not compiled in namespace '{}'",
                        extent, ns
                    ),
                )
            }
        };
        let sql = build_search_table_sql(
            params.limit,
            header_filters(&params, "h."),
            &extent_table,
            &prop_ids,
            st.value.as_deref(),
            st.value_like.as_deref(),
        );
        return match iris.query(&sql, vec![], ns, &client).await {
            Ok(resp) => {
                let rows = resp["result"]["content"].clone();
                let count = rows.as_array().map(|a| a.len()).unwrap_or(0);
                let mut out = serde_json::json!({
                    "success": true,
                    "messages": rows,
                    "count": count,
                    "extent": extent,
                    "prop_ids": prop_ids,
                    "sql": sql,
                });
                if count == 0 {
                    // Issue #4: valid prop + zero rows is usually a config-time effect.
                    out["hint"] = serde_json::Value::String(
                        "A Search Table indexes only messages received AFTER SearchTableClass was configured on the item — existing messages are not back-indexed. Also check the value: PropValue matching is exact unless value_like is used.".into(),
                    );
                }
                ok_json(out)
            }
            Err(e) => err_json(net_err(&e.to_string()), &e.to_string()),
        };
    }

    // ── Plain header search (unchanged behavior) ─────────────────────────
    let filters = header_filters(&params, "");
    let where_clause = if filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", filters.join(" AND "))
    };
    let sql = format!(
        "SELECT TOP {} {HEADER_COLS} FROM Ens.MessageHeader {} ORDER BY ID DESC",
        params.limit, where_clause
    );
    match iris.query(&sql, vec![], ns, &client).await {
        Ok(resp) => ok_json(
            serde_json::json!({"success": true, "messages": resp["result"]["content"], "count": resp["result"]["content"].as_array().map(|a| a.len()).unwrap_or(0)}),
        ),
        Err(e) => err_json(net_err(&e.to_string()), &e.to_string()),
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

// ── 056 interop-depth ────────────────────────────────────────────────────────
// Ported from upstream's 056-interop-depth (f92da6d), adapted to this fork:
// every user string reaching ObjectScript goes through `os_str_expr` (issue #6),
// SQL uses bound parameters instead of interpolation, and the namespace arrives
// already resolved by the caller (issue #15).

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessageBodyParams {
    pub message_id: String,
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u32,
    #[serde(default)]
    pub acknowledge_phi: bool,
}
fn default_max_bytes() -> u32 {
    65536
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BusinessRuleInfoParams {
    pub action: String,
    pub rule_name: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProductionDiffParams {
    pub production: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
}

/// Detect a body's content type from its leading characters.
/// `"HL7v2"` for MSH|-prefixed content, `"JSON"`/`"XML"` for {/[ and < prefixes,
/// `"text"` for everything else.
pub fn detect_content_type(body: &str) -> &'static str {
    let trimmed = body.trim_start();
    if trimmed.starts_with("MSH|") {
        "HL7v2"
    } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
        "JSON"
    } else if trimmed.starts_with('<') {
        "XML"
    } else {
        "text"
    }
}

/// Truncate `body` to at most `max_bytes`, breaking at a UTF-8 char boundary at or
/// before the limit. Returns `(content, was_truncated, original_byte_len)` — the
/// length is the whole body's, so callers can report what they did not return.
pub fn truncate_body(body: &str, max_bytes: usize) -> (String, bool, usize) {
    let original_len = body.len();
    if original_len <= max_bytes {
        return (body.to_string(), false, original_len);
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (body[..end].to_string(), true, original_len)
}

/// Replace the standard HL7 v2 PHI field positions (PID-3, PID-5, PID-7, PID-8,
/// PID-11, PID-18, MSH-3) with `[REDACTED]`. Content without an `MSH|` segment is
/// returned unchanged — only HL7 v2 has known PHI positions to blank.
pub fn redact_hl7v2(body: &str) -> String {
    if detect_content_type(body) != "HL7v2" {
        return body.to_string();
    }
    let line_ending = if body.contains("\r\n") {
        "\r\n"
    } else if body.contains('\r') {
        "\r"
    } else {
        "\n"
    };
    let redact_fields = |line: &str, segment: &str, field_indices: &[usize]| -> String {
        let mut fields: Vec<&str> = line.split('|').collect();
        if fields.first().copied() != Some(segment) {
            return line.to_string();
        }
        for &idx in field_indices {
            if idx < fields.len() {
                fields[idx] = "[REDACTED]";
            }
        }
        fields.join("|")
    };
    body.split(line_ending)
        .map(|line| {
            // MSH-1 is the implicit field-separator char (not a split element), so
            // fields[1] = MSH-2 (encoding chars) and fields[2] = MSH-3 (sending app).
            let redacted = redact_fields(line, "MSH", &[2]);
            redact_fields(&redacted, "PID", &[3, 5, 7, 8, 11, 18])
        })
        .collect::<Vec<_>>()
        .join(line_ending)
}

/// Parse `<Item Name="..." ClassName="..." Enabled="..."/>` entries out of a
/// production class's UDL/XData source. Returns `(name, class_name, enabled)`.
pub fn parse_production_items_from_source(source: &str) -> Vec<(String, String, bool)> {
    let mut items = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("<Item ") {
            continue;
        }
        let name = extract_xml_attr(trimmed, "Name");
        let class_name = extract_xml_attr(trimmed, "ClassName");
        // IRIS treats a missing Enabled as enabled.
        let enabled = extract_xml_attr(trimmed, "Enabled")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(true);
        if let (Some(name), Some(class_name)) = (name, class_name) {
            items.push((name, class_name, enabled));
        }
    }
    items
}

fn extract_xml_attr(line: &str, attr: &str) -> Option<String> {
    // Require whitespace before the attribute name so `Name="` does not match
    // inside `ClassName="`. Attributes are always preceded by a space.
    let needle = format!(" {attr}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Read an Ensemble message body (`Ens.StringContainer`, `Ens.StreamContainer`,
/// `%Stream.Object`). `data_policy` gates PHI: `block` refuses outright, `allow`
/// requires an explicit acknowledgement, `redact` blanks known HL7 v2 PHI fields.
pub async fn handle_iris_message_body(
    iris: Option<&IrisConnection>,
    params: &MessageBodyParams,
    data_policy: &str,
) -> Result<CallToolResult, McpError> {
    if data_policy == "block" {
        return err_json(
            "PHI_POLICY_BLOCKED",
            "iris_message_body is blocked while dataPolicy=block — message bodies may contain PHI. \
             Pass dataPolicy=redact to blank known HL7 v2 PHI fields, or dataPolicy=allow with \
             acknowledgePhi=true to read the body as-is.",
        );
    }
    if data_policy == "allow" && !params.acknowledge_phi {
        return err_json(
            "PHI_ACK_REQUIRED",
            "dataPolicy=allow requires acknowledgePhi=true — the body is returned unredacted.",
        );
    }
    let message_id: i64 = match params.message_id.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return err_json(
                "INVALID_MESSAGE_ID",
                &format!(
                    "message_id '{}' is not a valid integer — pass the Ens.MessageHeader ID",
                    params.message_id
                ),
            )
        }
    };
    let mut max_bytes = params.max_bytes.max(1);
    let mut max_bytes_clamped = false;
    if max_bytes > 1_048_576 {
        max_bytes = 1_048_576;
        max_bytes_clamped = true;
    }

    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;

    // message_id is an i64 and max_bytes a u32 — both are numeric, so nothing
    // user-controlled reaches the generated source as a string here.
    let code = format!(
        r#"Set hdr=##class(Ens.MessageHeader).%OpenId({message_id})
If '$IsObject(hdr) {{ Write "ERROR:MESSAGE_NOT_FOUND" Quit }}
Set bodyClass=hdr.MessageBodyClassName
Set bodyId=hdr.MessageBodyId
If bodyClass="" {{ Write "ERROR:MESSAGE_NOT_FOUND" Quit }}
Set body=$ClassMethod(bodyClass,"%OpenId",bodyId)
If '$IsObject(body) {{ Write "ERROR:MESSAGE_NOT_FOUND" Quit }}
If body.%IsA("Ens.StreamContainer") {{
  Set stream=body.Stream
  If '$IsObject(stream) {{ Write "ERROR:STREAM_READ_ERROR:no stream object" Quit }}
  Set full=stream.Size
  Set content=stream.Read({max_bytes})
  Write "OK:"_full_":"
  Write content
}} ElseIf body.%IsA("%Stream.Object") {{
  Set full=body.Size
  Set content=body.Read({max_bytes})
  Write "OK:"_full_":"
  Write content
}} ElseIf body.%IsA("Ens.StringContainer") {{
  Set content=body.StringValue
  Set full=$Length(content)
  If full>{max_bytes} {{ Set content=$Extract(content,1,{max_bytes}) }}
  Write "OK:"_full_":"
  Write content
}} Else {{
  Write "ERROR:UNSUPPORTED_BODY_CLASS:"_bodyClass
}}"#
    );

    match iris
        .execute_via_generator(&code, &params.namespace, &client)
        .await
    {
        Ok(out) => {
            if out.starts_with("ERROR:MESSAGE_NOT_FOUND") {
                return err_json(
                    "MESSAGE_NOT_FOUND",
                    &format!("No body found for message ID {message_id}"),
                );
            }
            if let Some(rest) = out.strip_prefix("ERROR:STREAM_READ_ERROR:") {
                return err_json("STREAM_READ_ERROR", rest.trim());
            }
            if let Some(rest) = out.strip_prefix("ERROR:UNSUPPORTED_BODY_CLASS:") {
                return err_json(
                    "UNSUPPORTED_BODY_CLASS",
                    &format!(
                        "Body class '{}' is not a recognized stream/text body type",
                        rest.trim()
                    ),
                );
            }
            let Some(rest) = out.strip_prefix("OK:") else {
                return err_json("IRIS_EXECUTE_ERROR", &out);
            };
            let mut parts = rest.splitn(2, ':');
            // The prefix is the body's FULL size. IRIS caps what it hands back at
            // max_bytes, so the returned content alone cannot say how much was left
            // behind — reporting its length would understate a truncated body.
            let full_size: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let body_content = parts.next().unwrap_or("");

            let (truncated_body, locally_truncated, _) =
                truncate_body(body_content, max_bytes as usize);
            let actual_size = full_size.max(body_content.len());
            let was_truncated = locally_truncated || actual_size > body_content.len();
            let content_type = detect_content_type(&truncated_body);

            let final_body = if data_policy == "redact" {
                redact_hl7v2(&truncated_body)
            } else {
                truncated_body
            };

            let mut resp = serde_json::json!({
                "success": true,
                "message_id": params.message_id,
                "content_type": content_type,
                "body": final_body,
                "truncated": was_truncated,
                "actual_size": actual_size,
                "redacted": data_policy == "redact",
            });
            if max_bytes_clamped {
                resp["max_bytes_clamped"] = serde_json::Value::Bool(true);
            }
            ok_json(resp)
        }
        Err(e) => err_json(
            if is_network_error(&e.to_string()) {
                "IRIS_UNREACHABLE"
            } else {
                "IRIS_EXECUTE_ERROR"
            },
            &e.to_string(),
        ),
    }
}

/// List the namespace's business rule sets, or describe one.
/// `Ens.Rule.RuleSet` is the rule-set persistent class (upstream's research
/// confirmed `EnsLib.Rules.Definition` does not exist).
pub async fn handle_iris_business_rule_info(
    iris: Option<&IrisConnection>,
    params: &BusinessRuleInfoParams,
) -> Result<CallToolResult, McpError> {
    match params.action.as_str() {
        "list" => {}
        "get" => {
            if params.rule_name.as_deref().unwrap_or("").trim().is_empty() {
                return err_json("INVALID_PARAMS", "rule_name is required for action=get");
            }
        }
        other => {
            return err_json(
                "INVALID_ACTION",
                &format!("action must be 'list' or 'get', got '{other}'"),
            )
        }
    }

    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;

    let exists_code = r#"Write ##class(%Dictionary.ClassDefinition).%ExistsId("Ens.Rule.RuleSet")"#;
    match iris
        .execute_via_generator(exists_code, &params.namespace, &client)
        .await
    {
        Ok(out) if out.trim() == "0" => {
            return err_json(
                "INTEROP_NOT_AVAILABLE",
                &format!(
                    "Ens.Rule.RuleSet is not available in namespace '{}' — Interoperability is not enabled there",
                    params.namespace
                ),
            )
        }
        Ok(_) => {}
        Err(e) => {
            return err_json(
                if is_network_error(&e.to_string()) {
                    "IRIS_UNREACHABLE"
                } else {
                    "IRIS_EXECUTE_ERROR"
                },
                &e.to_string(),
            )
        }
    }

    if params.action == "list" {
        let sql = "SELECT Name, FullName, ShortDescription, TimeModified \
                   FROM Ens_Rule.RuleSet ORDER BY Name";
        return match iris.query(sql, vec![], &params.namespace, &client).await {
            Ok(resp) => {
                let rows = resp["result"]["content"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let rules: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r["Name"],
                            "class_name": r["FullName"],
                            "description": r["ShortDescription"],
                            "modified": r["TimeModified"],
                        })
                    })
                    .collect();
                if !rules.is_empty() {
                    return ok_json(serde_json::json!({
                        "success": true,
                        "namespace": params.namespace,
                        "count": rules.len(),
                        "rules": rules,
                        "source": "Ens_Rule.RuleSet",
                    }));
                }
                // Ens_Rule.RuleSet is populated by the rule editor's projection, and
                // is empty on instances where rules exist only as compiled
                // Ens.Rule.Definition subclasses. Answering "no rules" there, while
                // rule classes plainly exist, is the #47 failure again — so fall
                // back to the class catalog and say which source answered.
                let (rules, source) =
                    match rule_classes_from_catalog(iris, &params.namespace, &client).await {
                        Some(found) if !found.is_empty() => (found, "%Dictionary.CompiledClass"),
                        _ => (rules, "Ens_Rule.RuleSet"),
                    };
                ok_json(serde_json::json!({
                    "success": true,
                    "namespace": params.namespace,
                    "count": rules.len(),
                    "rules": rules,
                    "source": source,
                }))
            }
            Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
        };
    }

    // action == "get". Ens.Rule.RuleSet's ID is a composite key
    // (HostClass||Name||Version), not the Name — look the ID up first, newest
    // version (highest ID) wins. Bound parameter, not interpolation.
    let rule_name = params.rule_name.as_deref().unwrap_or("").trim();
    let sql = "SELECT ID, ShortDescription FROM Ens_Rule.RuleSet WHERE Name = ? ORDER BY ID DESC";
    match iris
        .query(
            sql,
            vec![serde_json::Value::String(rule_name.to_string())],
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
            if rows.is_empty() {
                // Distinguish "no such rule" from "the rule exists as a compiled class
                // but the RuleSet projection has no row for it" — very different fixes.
                let known: Vec<String> =
                    rule_classes_from_catalog(iris, &params.namespace, &client)
                        .await
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|r| r["name"].as_str().map(|s| s.to_string()))
                        .collect();
                if known.iter().any(|k| k == rule_name) {
                    return err_json(
                        "RULE_NOT_PROJECTED",
                        &format!(
                            "'{rule_name}' is a compiled Ens.Rule.Definition class in '{}', but it has no \
                             Ens_Rule.RuleSet row — the rule-set projection is written when the rule is saved \
                             from the Rule Editor. Read the class source with iris_doc to inspect its \
                             RuleDefinition XData.",
                            params.namespace
                        ),
                    );
                }
                return err_json(
                    "RULE_NOT_FOUND",
                    &format!(
                        "No business rule named '{rule_name}' in namespace '{}' — call action=list to see what is there",
                        params.namespace
                    ),
                );
            }
            let description = rows[0]["ShortDescription"].clone();
            let rule_id = rows[0]["ID"].as_str().unwrap_or("").to_string();

            let code = format!(
                r#"Set rs=##class(Ens.Rule.RuleSet).%OpenId({rule_id_expr})
If '$IsObject(rs) {{ Write "ERROR:RULE_NOT_FOUND" Quit }}
Write "OK:"
Set count=rs.Rules.Count()
For i=1:1:count {{
  Set rule=rs.Rules.GetAt(i)
  Write "COND:"_$Select($IsObject(rule.Conditions):rule.Conditions.Count(),1:0)_"|"
  Write "ACT:"_$Select($IsObject(rule.Actions):rule.Actions.Count(),1:0)_"|"
}}"#,
                rule_id_expr = os_str_expr(&rule_id)
            );
            match iris
                .execute_via_generator(&code, &params.namespace, &client)
                .await
            {
                Ok(out) => {
                    if out.trim().starts_with("ERROR:RULE_NOT_FOUND") {
                        return err_json(
                            "RULE_NOT_FOUND",
                            &format!("No business rule named '{rule_name}' found"),
                        );
                    }
                    let mut conditions = 0usize;
                    let mut actions = 0usize;
                    let mut rule_count = 0usize;
                    if let Some(rest) = out.trim().strip_prefix("OK:") {
                        for part in rest.split('|') {
                            if let Some(n) = part.strip_prefix("COND:") {
                                conditions += n.parse::<usize>().unwrap_or(0);
                                rule_count += 1;
                            } else if let Some(n) = part.strip_prefix("ACT:") {
                                actions += n.parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                    ok_json(serde_json::json!({
                        "success": true,
                        "name": rule_name,
                        "namespace": params.namespace,
                        "description": description,
                        "rules": rule_count,
                        "conditions": conditions,
                        "actions": actions,
                    }))
                }
                Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
            }
        }
        Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
    }
}

/// Compiled `Ens.Rule.Definition` subclasses in `namespace`, excluding the framework's
/// own. Used when `Ens_Rule.RuleSet` has no rows so the caller still learns what exists.
async fn rule_classes_from_catalog(
    iris: &IrisConnection,
    namespace: &str,
    client: &reqwest::Client,
) -> Option<Vec<serde_json::Value>> {
    let sql = "SELECT Name FROM %Dictionary.CompiledClass \
               WHERE PrimarySuper LIKE '%Ens.Rule.Definition%' \
                 AND Name NOT LIKE 'Ens.%' AND Name NOT LIKE 'EnsLib.%' \
                 AND Name NOT LIKE 'HS.%' AND Name NOT LIKE '\\%%' ESCAPE '\\' \
               ORDER BY Name";
    let resp = iris.query(sql, vec![], namespace, client).await.ok()?;
    Some(
        resp["result"]["content"]
            .as_array()?
            .iter()
            .filter_map(|r| r["Name"].as_str())
            .map(|n| {
                serde_json::json!({
                    "name": n,
                    "class_name": n,
                    "description": serde_json::Value::Null,
                    "modified": serde_json::Value::Null,
                })
            })
            .collect(),
    )
}

/// Diff a running production's config items against the committed class source.
pub async fn handle_iris_production_diff(
    iris: Option<&IrisConnection>,
    params: &ProductionDiffParams,
) -> Result<CallToolResult, McpError> {
    let iris = match iris {
        Some(i) => i,
        None => return err_json("IRIS_UNREACHABLE", "No IRIS connection"),
    };
    let client = IrisConnection::http_client().map_err(|_| iris_unreachable())?;
    let ns = &params.namespace;

    let prod_name = if let Some(p) = &params.production {
        p.clone()
    } else {
        let status_code = r#"Set sc=##class(Ens.Director).GetProductionStatus(.n,.s) If $System.Status.IsError(sc)||(n="") { Write "ERROR:NO_PRODUCTION" } Else { Write n }"#;
        match iris.execute_via_generator(status_code, ns, &client).await {
            Ok(out) => {
                let out = out.trim().to_string();
                if out.starts_with("ERROR:NO_PRODUCTION") {
                    return err_json(
                        "NO_PRODUCTION",
                        "No production is running in this namespace — pass `production` to diff a specific one",
                    );
                }
                out
            }
            Err(e) => return err_json("IRIS_UNREACHABLE", &e.to_string()),
        }
    };

    let doc_name = format!("{prod_name}.cls");
    let scm_code = format!(
        r#"Set sc=##class(%Studio.SourceControl.Interface).SourceControlCreate({user_expr},{pass_expr},.created,.flags,.outuser)
Set isInSC=0
Set sc=##class(%Studio.SourceControl.Interface).GetStatus({doc_expr},.isInSC,.editable,.isCheckedOut,.owner)
If $System.Status.IsError(sc)||('isInSC) {{ Write "NO_SCM" }} Else {{ Write "IN_SCM" }}"#,
        user_expr = os_str_expr(&iris.username),
        pass_expr = os_str_expr(&iris.password),
        doc_expr = os_str_expr(&doc_name),
    );
    match iris.execute_via_generator(&scm_code, ns, &client).await {
        Ok(out) if out.trim() == "NO_SCM" => {
            return err_json(
                "NO_SCM",
                &format!(
                    "No source control is configured for '{doc_name}' in namespace '{ns}' — there is no committed source to diff against"
                ),
            )
        }
        Ok(_) => {}
        Err(e) => return err_json("IRIS_UNREACHABLE", &e.to_string()),
    }

    let exists_code = format!(
        r#"Write ##class(%Dictionary.ClassDefinition).%ExistsId({prod_expr})"#,
        prod_expr = os_str_expr(&prod_name)
    );
    match iris.execute_via_generator(&exists_code, ns, &client).await {
        Ok(out) if out.trim() == "0" => {
            return err_json(
                "PRODUCTION_NOT_FOUND",
                &format!("Production '{prod_name}' does not exist in namespace '{ns}'"),
            )
        }
        Ok(_) => {}
        Err(e) => return err_json("IRIS_UNREACHABLE", &e.to_string()),
    }

    // Current in-memory item set. Bound parameter, not interpolation.
    let sql = "SELECT Name, ClassName, Category, Enabled FROM Ens_Config.Item \
               WHERE Production->Name = ?";
    let current_items: Vec<(String, String, bool)> = match iris
        .query(
            sql,
            vec![serde_json::Value::String(prod_name.clone())],
            ns,
            &client,
        )
        .await
    {
        Ok(resp) => resp["result"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|r| {
                (
                    r["Name"].as_str().unwrap_or("").to_string(),
                    r["ClassName"].as_str().unwrap_or("").to_string(),
                    // Enabled comes back as a SQL boolean on some builds and 0/1 on others.
                    r["Enabled"]
                        .as_bool()
                        .unwrap_or_else(|| r["Enabled"].as_i64().unwrap_or(0) != 0),
                )
            })
            .collect(),
        Err(e) => return err_json("IRIS_UNREACHABLE", &e.to_string()),
    };

    // Committed source via Atelier REST GET /doc/<name>.
    let doc_url = iris.versioned_ns_url(ns, &format!("/doc/{}", urlencoding::encode(&doc_name)));
    let committed_items: Vec<(String, String, bool)> = match client
        .get(&doc_url)
        .basic_auth(&iris.username, Some(&iris.password))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let source = crate::tools::doc::doc_content_to_string(&body);
            parse_production_items_from_source(&source)
        }
        _ => Vec::new(),
    };

    let mut changes = Vec::new();
    for (name, class_name, enabled) in &current_items {
        match committed_items.iter().find(|(n, _, _)| n == name) {
            None => changes.push(serde_json::json!({
                "item_name": name, "item_type": class_name, "status": "added"
            })),
            Some((_, c_class, c_enabled)) => {
                if c_class != class_name || c_enabled != enabled {
                    changes.push(serde_json::json!({
                        "item_name": name, "item_type": class_name, "status": "modified"
                    }));
                }
            }
        }
    }
    for (name, class_name, _) in &committed_items {
        if !current_items.iter().any(|(n, _, _)| n == name) {
            changes.push(serde_json::json!({
                "item_name": name, "item_type": class_name, "status": "removed"
            }));
        }
    }

    ok_json(serde_json::json!({
        "success": true,
        "production": prod_name,
        "namespace": ns,
        "in_sync": changes.is_empty(),
        "changes": changes,
    }))
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

    // ─── issue #4: typed message-content search ───

    fn msg_params(source: Option<&str>) -> MessageSearchParams {
        MessageSearchParams {
            namespace: None,
            source: source.map(str::to_string),
            target: None,
            class_name: None,
            session_id: None,
            since_id: None,
            limit: 5,
            body_class: None,
            body_where: None,
            body_select: vec![],
            search_table: None,
        }
    }

    #[test]
    fn body_join_sql_matches_issue_shape() {
        let p = msg_params(Some("Router.Censo"));
        let sql = build_body_join_sql(
            5,
            header_filters(&p, "h."),
            "Ejercicio3.MSG.MenuReq",
            "Ejercicio3_MSG.MenuReq",
            Some("PacienteId = '4003'"),
            &["PacienteId".to_string(), "FechaNacimiento".to_string()],
        );
        assert!(sql.starts_with("SELECT TOP 5 h.ID, h.TimeCreated"));
        assert!(sql.contains(", r.PacienteId, r.FechaNacimiento"));
        assert!(sql.contains("JOIN Ejercicio3_MSG.MenuReq r ON h.MessageBodyId = r.ID"));
        assert!(sql.contains("h.SourceConfigName = 'Router.Censo'"));
        // MessageBodyId is only unique per body table — the class pin is what
        // stops same-numbered rows of other classes matching.
        assert!(sql.contains("h.MessageBodyClassName = 'Ejercicio3.MSG.MenuReq'"));
        assert!(sql.contains("(PacienteId = '4003')"));
        assert!(sql.ends_with("ORDER BY h.ID DESC"));
    }

    #[test]
    fn search_table_sql_matches_issue_shape() {
        let sql = build_search_table_sql(
            10,
            header_filters(&msg_params(None), "h."),
            "EnsLib_HL7.SearchTable",
            &[4],
            Some("16284718"),
            None,
        );
        assert!(sql.contains("JOIN EnsLib_HL7.SearchTable st ON st.DocId = h.MessageBodyId"));
        assert!(sql.contains("st.PropId IN (4)"));
        assert!(sql.contains("st.PropValue = '16284718'"));
        assert!(sql.contains(", st.PropValue FROM"));
        let like = build_search_table_sql(
            10,
            vec![],
            "EnsLib_HL7.SearchTable",
            &[12, 14],
            None,
            Some("AMOX%"),
        );
        assert!(like.contains("st.PropId IN (12,14)"));
        assert!(like.contains("st.PropValue LIKE 'AMOX%'"));
    }

    #[test]
    fn sql_identifier_gate() {
        assert!(is_sql_identifier("PacienteId"));
        assert!(is_sql_identifier("%ID"));
        assert!(!is_sql_identifier("a b"));
        assert!(!is_sql_identifier("x; DROP"));
        assert!(!is_sql_identifier(""));
    }

    #[test]
    fn search_params_deserialize_new_filters() {
        let p: MessageSearchParams = serde_json::from_str(
            r#"{"body_class":"E.MSG.R","body_where":"X=1","body_select":["X"],
                "search_table":{"prop":"PatientID","value":"1"}}"#,
        )
        .unwrap();
        assert_eq!(p.body_class.as_deref(), Some("E.MSG.R"));
        assert_eq!(p.body_select, vec!["X"]);
        let st = p.search_table.unwrap();
        assert_eq!(st.prop, "PatientID");
        assert_eq!(st.value.as_deref(), Some("1"));
        assert!(st.extent.is_none());
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
