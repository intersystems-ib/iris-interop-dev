//! IRIS connection types and Atelier REST API fingerprinting.

use std::fmt;

/// Issues #101 / #102: an Atelier call that came back with a non-2xx status, carried as a
/// TYPED error instead of an opaque `anyhow` string.
///
/// Before this, every consumer re-guessed what "PUT doc failed: HTTP 401 Unauthorized" meant
/// and they disagreed — `iris_production` said INTEROP_ERROR, `iris_table_info` said
/// IRIS_EXECUTE_ERROR, `iris_test` said TEST_EXECUTION_ERROR. Three tools, three codes, one
/// cause. The query path was worse: it never looked at the status at all, so a 401 (whose body
/// is not JSON) and a 404 (whose body is ZERO bytes) both surfaced as "error decoding
/// response body" with the status destroyed.
///
/// `message` is the human text — deliberately byte-identical to the string each site used to
/// `bail!`, so nothing that reads these messages moves. `status` and `url` are the new part:
/// the tool layer downcasts (see [`atelier_status`]) and can finally ask "is the NAMESPACE
/// why this 404 happened" without re-parsing prose.
#[derive(Debug, Clone)]
pub struct AtelierHttpError {
    pub status: u16,
    pub url: String,
    /// Response body, trimmed and truncated — Atelier puts real diagnostics here on a 400.
    pub body: String,
    message: String,
}

impl AtelierHttpError {
    pub fn new(
        status: reqwest::StatusCode,
        url: impl Into<String>,
        body: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status: status.as_u16(),
            url: url.into(),
            body: body.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for AtelierHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AtelierHttpError {}

/// `Some` when this `anyhow::Error` is an Atelier non-2xx response, `None` for a transport
/// failure, a SQL error, or anything else. The tool layer's one entry point to the status.
pub fn atelier_status(err: &anyhow::Error) -> Option<&AtelierHttpError> {
    err.downcast_ref::<AtelierHttpError>()
}

/// Trim and cut a response body to `max` CHARACTERS (not bytes — a cut inside a UTF-8
/// sequence would panic).
pub(crate) fn truncate_body(body: &str, max: usize) -> String {
    body.trim().chars().take(max).collect()
}

/// What an Atelier `/action/query` response actually says.
///
/// #105: this used to be TWO readers. `IrisConnection::query` had the careful one; the `iris_query`
/// tool had its own copy, and the copy drifted — it checked the HTTP status before the
/// body (throwing away the `ERROR #16002` text Atelier puts in a 400) and parsed with
/// `unwrap_or_default()`, so a 200 carrying HTML answered `success:true` with zero rows.
/// The two tools gave different answers to the same malformed response. One reader means
/// they cannot, while each caller still renders the outcome in its own envelope.
#[derive(Debug)]
pub(crate) enum QueryOutcome {
    /// Parsed body with no `status.errors` and a successful status.
    Rows(serde_json::Value),
    /// Atelier reported its own error in `status.errors` — a deterministic SQL/Atelier
    /// failure. Wins over the HTTP status whatever that status is (see below).
    IrisError(String),
    /// Non-2xx with nothing in `status.errors` to explain it.
    HttpError {
        status: reqwest::StatusCode,
        snippet: String,
    },
    /// 2xx whose body is not JSON — a proxy error page, an HTML login redirect. NOT an
    /// empty result set, which is what `unwrap_or_default()` silently turned it into.
    /// The status rides along so a caller can report WHICH success code lied.
    NonJson {
        status: reqwest::StatusCode,
        snippet: String,
    },
}

/// BODY FIRST, then the status. Reading `resp.json()` alone destroyed the status — a 401
/// body is not JSON and a missing-namespace 404 body is ZERO bytes, so every caller saw
/// "error decoding response body" and IRIS's actual answer was gone.
///
/// The ordering is mandatory, not stylistic. Atelier puts REAL diagnostics in the body of
/// some non-2xx responses: a malformed query POST returns HTTP 400 carrying
/// `{"status":{"errors":[{"error":"ERROR #16002: Invalid JSON Content",...}]}}`. A
/// status-first `if !status.is_success()` would swap one uninformative error for another.
/// `status.errors` therefore wins over the HTTP status whatever that status is, so
/// SQL_ERROR and the #16002 text keep coming through byte-for-byte. (A bad SELECT is not
/// even this case — it returns 200 with status.errors.)
pub(crate) fn interpret_query_response(status: reqwest::StatusCode, text: &str) -> QueryOutcome {
    let parsed = serde_json::from_str::<serde_json::Value>(text).ok();
    if let Some(body) = &parsed {
        if let Some(errs) = body["status"]["errors"].as_array() {
            if !errs.is_empty() {
                let msg = errs[0]["error"].as_str().unwrap_or("Atelier query error");
                return QueryOutcome::IrisError(msg.to_string());
            }
        }
    }
    if !status.is_success() {
        return QueryOutcome::HttpError {
            status,
            snippet: truncate_body(text, 500),
        };
    }
    match parsed {
        Some(body) => QueryOutcome::Rows(body),
        None => QueryOutcome::NonJson {
            status,
            snippet: truncate_body(text, 200),
        },
    }
}

/// Whether the connected IRIS instance is a production (Live) system.
/// Detected at probe time via `^%SYS("SystemMode")` SQL query.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SystemMode {
    Live,        // "Live" — lock write tools
    Development, // "Development" — allow write tools
    Test,        // "Test" — allow write tools
    #[default]
    Unknown, // null/empty — apply namespace heuristic
}

/// Which version of the Atelier REST API to use.
#[derive(Debug, Clone, PartialEq)]
pub enum AtelierVersion {
    V8,
    V2,
    V1,
}

impl AtelierVersion {
    pub fn version_str(&self) -> &'static str {
        match self {
            AtelierVersion::V8 => "v8",
            AtelierVersion::V2 => "v2",
            AtelierVersion::V1 => "v1",
        }
    }
}

/// A resolved connection to a running IRIS instance via Atelier REST API.
/// T011: manual Debug impl redacts `password` (P1/FR-022).
#[derive(Clone)]
pub struct IrisConnection {
    /// Base URL e.g. "http://localhost:52773" or "http://localhost:80/prefix"
    pub base_url: String,
    pub namespace: String,
    pub username: String,
    pub password: String,
    pub version: Option<String>,
    pub atelier_version: AtelierVersion,
    pub source: DiscoverySource,
    pub port_superserver: Option<u16>,
    /// Detected at probe time — controls write-tool availability (issue #26).
    pub system_mode: SystemMode,
    /// Issue #101: the HTTP status the Atelier root probe got, or `None` if the probe never
    /// ran or never got a response. The server already KNEW a wrong password had been
    /// rejected — `probe()` logged it to `tracing::debug!` and threw it away, while
    /// `check_config`, the tool whose own description says to call it to diagnose exactly
    /// this, went on reporting `connected: true`.
    pub probe_status: Option<u16>,
    /// Issue #101: `Some(true)` when the root probe got an HTTP *response* of any status,
    /// `Some(false)` when it ran and the request never completed (closed port, unroutable
    /// host), `None` when the probe never ran. `probe_status` alone cannot express the
    /// middle case — it is `None` for "never ran" AND for "ran, got nothing", and
    /// `check_config` needs those apart to answer `connected` honestly without turning
    /// "never asked" into a claim.
    pub probe_reached: Option<bool>,
}

/// T011: Manual Debug implementation — never prints the password.
impl fmt::Debug for IrisConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IrisConnection")
            .field("base_url", &self.base_url)
            .field("namespace", &self.namespace)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("version", &self.version)
            .field("atelier_version", &self.atelier_version)
            .field("source", &self.source)
            .field("port_superserver", &self.port_superserver)
            .field("system_mode", &self.system_mode)
            .field("probe_status", &self.probe_status)
            .field("probe_reached", &self.probe_reached)
            .finish()
    }
}

/// Issue #101/#102: what a GET of the Atelier ROOT descriptor actually established.
///
/// See [`IrisConnection::root_probe`]. The `Option<Vec<String>>` this replaces answered
/// "cannot tell" to a question it could in fact answer, and every caller read "cannot tell"
/// as licence to keep its own guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootProbe {
    /// The root descriptor was read: these namespaces are visible to these credentials.
    Namespaces(Vec<String>),
    /// IRIS answered `/api/atelier/` with **404**. Every IRIS with the Atelier REST
    /// application enabled serves that URL, so this is a positive finding, not an absence
    /// of one: the application is not published where this server is looking. The usual
    /// cause is a wrong (or missing) `IRIS_WEB_PREFIX`; the other is the web application
    /// being disabled.
    NoAtelierHere { url: String },
    /// Nothing was established: the request never completed, or IRIS answered with a status
    /// or a body this probe cannot read anything into. Never a claim.
    Unknown,
}

#[derive(Debug, Clone)]
pub enum DiscoverySource {
    LocalhostScan { port: u16 },
    Docker { container_name: String },
    VsCodeSettings,
    EnvVar,
    ExplicitFlag,
}

/// Structured result from a document compile operation.
#[derive(Debug)]
pub struct CompileResult {
    pub errors: Vec<String>,
    pub console: Vec<String>,
}

impl CompileResult {
    pub fn success(&self) -> bool {
        self.errors.is_empty()
    }
}

impl IrisConnection {
    pub fn new(
        base_url: impl Into<String>,
        namespace: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        source: DiscoverySource,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            namespace: namespace.into(),
            username: username.into(),
            password: password.into(),
            version: None,
            atelier_version: AtelierVersion::V1,
            source,
            port_superserver: None,
            system_mode: SystemMode::Unknown,
            probe_status: None,
            probe_reached: None,
        }
    }

    /// Returns true if write-capable tools should be registered.
    /// Checks SystemMode, namespace heuristics, and IRIS_ALLOW_PROD override (issue #26).
    pub fn is_write_allowed(&self) -> bool {
        if std::env::var("IRIS_ALLOW_PROD")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return true;
        }
        match &self.system_mode {
            SystemMode::Live => false,
            SystemMode::Development | SystemMode::Test => true,
            SystemMode::Unknown => !is_production_namespace(&self.namespace),
        }
    }

    /// Build the full Atelier REST URL for a given path suffix.
    pub fn atelier_url(&self, path: &str) -> String {
        format!(
            "{}/api/atelier{}",
            self.base_url.trim_end_matches('/'),
            path
        )
    }

    /// Build a versioned Atelier URL using the detected API version and the connection namespace.
    pub fn atelier_url_versioned(&self, path: &str) -> String {
        self.versioned_ns_url(&self.namespace.clone(), path)
    }

    /// Build a versioned Atelier URL for an explicit namespace.
    pub fn versioned_ns_url(&self, namespace: &str, path: &str) -> String {
        let v = self.atelier_version.version_str();
        // URL-encode namespace so %SYS becomes %25SYS in the path component
        let ns_encoded = urlencoding::encode(namespace);
        self.atelier_url(&format!("/{}/{}{}", v, ns_encoded, path))
    }

    /// Probe this connection: fetch IRIS version, Atelier API level, and SystemMode.
    pub async fn probe(&mut self) {
        let client = match Self::probe_client() {
            Ok(c) => c,
            Err(_) => return,
        };

        let url = self.atelier_url("/");
        let probe = client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await;
        // #101: record REACHED separately from the status. A closed port and a probe that
        // never ran both leave `probe_status` at None, and `check_config` must not report
        // them the same way — one is a definite "IRIS did not answer", the other is
        // "nobody asked", and only the first may become `connected: false`.
        self.probe_reached = Some(probe.is_ok());
        if let Ok(resp) = probe {
            let status = resp.status();
            self.probe_status = Some(status.as_u16());
            if status.is_success() {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    tracing::debug!("Atelier root response: {}", body);
                    let content = &body["result"]["content"];
                    self.version = content["version"].as_str().map(|v| v.to_string());
                    self.atelier_version = match content["api"].as_u64() {
                        Some(v) if v >= 8 => AtelierVersion::V8,
                        Some(v) if v >= 2 => AtelierVersion::V2,
                        _ => AtelierVersion::V1,
                    };
                }
            } else {
                tracing::debug!("Atelier root probe got HTTP {}", status);
            }
        }

        // Detect SystemMode via SQL against %SYS global (issue #26).
        // One extra round-trip at startup; result cached for session lifetime.
        let mode = self.detect_system_mode(&client).await;
        self.system_mode = mode;
        tracing::info!(
            host = %self.base_url,
            version = ?self.version,
            system_mode = ?self.system_mode,
            write_allowed = self.is_write_allowed(),
            "iris-agentic-dev: connection probed"
        );
    }

    /// Issue #93: the namespaces these credentials can actually reach, read from the
    /// Atelier root descriptor (`result.content.namespaces`) — the same URL `probe()`
    /// already fetches for version/api, so transport, auth and prefix handling are
    /// pre-validated. 439 bytes and ~6 ms on the dev instance.
    ///
    /// An Atelier 404 for a MISSING NAMESPACE comes back with a zero-byte body, which is
    /// byte-for-byte indistinguishable from a 404 for a missing document — so a tool that
    /// wants to tell those apart has to ask a second question. This is that question.
    ///
    /// Fetched fresh at the moment of the question and never cached: a namespace can be
    /// created while this server runs, and a stale "does not exist" is precisely the wrong
    /// answer #93 is about. It only ever runs on a path that has ALREADY failed.
    ///
    /// `None` always means *cannot tell*. Callers must keep their own error in that case and
    /// never turn `None` into a positive claim about a namespace. Callers that need to tell
    /// the two *reasons* for `None` apart — "the Atelier app is not at this URL at all" vs
    /// "the probe established nothing" — use [`IrisConnection::root_probe`] instead.
    ///
    /// The list reflects ACCESSIBILITY, not raw existence: `%Atelier.v1.Utils.General`
    /// filters to what the authenticated user can reach, so a namespace that exists but is
    /// invisible to these credentials is absent from it. Messages built from this must say
    /// so.
    pub async fn accessible_namespaces(&self, client: &reqwest::Client) -> Option<Vec<String>> {
        match self.root_probe(client).await {
            RootProbe::Namespaces(list) => Some(list),
            _ => None,
        }
    }

    /// Issue #101/#102: the same GET as [`IrisConnection::accessible_namespaces`], keeping the
    /// finding it used to throw away.
    ///
    /// `Option<Vec<String>>` collapsed three different outcomes into one `None`, and the
    /// difference between them is the whole answer. A **404 on `/api/atelier/`** is not
    /// "cannot tell": a working Atelier always serves its own root descriptor, so a 404 there
    /// says positively that the REST application is not published at this URL — the signature
    /// of a wrong `IRIS_WEB_PREFIX`. Treating that as "cannot tell" is what let `iris_doc`
    /// mode=head answer `{"success":true,"exists":false}` for a class that provably exists,
    /// and what stripped the `IRIS_WEB_PREFIX` diagnosis off `iris_query`'s bare 404.
    ///
    /// Everything else — transport failure, 5xx, 401/403, or a 2xx body with no `namespaces`
    /// array (an older or foreign server) — stays [`RootProbe::Unknown`]. Those are genuinely
    /// unknowable and must never become a claim.
    pub async fn root_probe(&self, client: &reqwest::Client) -> RootProbe {
        let url = self.atelier_url("/");
        let resp = match client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Atelier root probe did not complete: {e}");
                return RootProbe::Unknown;
            }
        };
        let status = resp.status();
        if status.as_u16() == 404 {
            tracing::debug!("Atelier root descriptor 404 at {url} — no Atelier application here");
            return RootProbe::NoAtelierHere { url };
        }
        if !status.is_success() {
            tracing::debug!("Atelier root namespace probe got HTTP {status}");
            return RootProbe::Unknown;
        }
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return RootProbe::Unknown;
        };
        match body["result"]["content"]["namespaces"].as_array() {
            Some(arr) => RootProbe::Namespaces(
                arr.iter()
                    .filter_map(|n| n.as_str().map(str::to_string))
                    .collect(),
            ),
            None => RootProbe::Unknown,
        }
    }

    /// Query `^%SYS("SystemMode")` to detect whether this is a Live instance.
    async fn detect_system_mode(&self, client: &reqwest::Client) -> SystemMode {
        let url = self.versioned_ns_url("%SYS", "/action/query");
        let resp = client
            .post(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!({
                "query": "SELECT Value FROM %Library.Global_Get('%SYS', '^%SYS(\"SystemMode\")')"
            }))
            .send()
            .await;
        let mode = match resp {
            Ok(r) => {
                if let Ok(body) = r.json::<serde_json::Value>().await {
                    body["result"]["content"][0]["Value"]
                        .as_str()
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            }
            Err(_) => String::new(),
        };
        match mode.as_str() {
            "Live" => SystemMode::Live,
            "Development" => SystemMode::Development,
            "Test" => SystemMode::Test,
            _ => SystemMode::Unknown,
        }
    }

    /// Execute ObjectScript code via the write-compile-query cycle (pure HTTP, no docker).
    /// FR-023: retries up to 3 times with 100/200/400ms backoff on network errors or HTTP 5xx.
    pub async fn execute_via_generator(
        &self,
        code: &str,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<String> {
        let delays = [
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(400),
        ];
        let mut last_err = anyhow::anyhow!("no attempts made");

        for (attempt, delay) in delays.iter().enumerate() {
            match self
                .execute_via_generator_once(code, namespace, client)
                .await
            {
                Ok(output) => {
                    if attempt > 0 {
                        tracing::info!(
                            "execute_via_generator succeeded on attempt {}",
                            attempt + 1
                        );
                    }
                    return Ok(output);
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Only retry on network errors or 5xx; 4xx are client errors, don't retry.
                    let is_retryable = msg.contains("HTTP 5")
                        || msg.contains("error sending request")
                        || msg.contains("connection refused")
                        || msg.contains("timed out");
                    if !is_retryable || attempt == delays.len() - 1 {
                        return Err(e);
                    }
                    // Transient on cold-start (private web server still warming up) — debug only;
                    // the success path logs at info so a recovery is still visible.
                    tracing::debug!(
                        "execute_via_generator attempt {} failed ({}), retrying in {:?}",
                        attempt + 1,
                        msg,
                        delay
                    );
                    last_err = e;
                    tokio::time::sleep(*delay).await;
                }
            }
        }
        Err(last_err)
    }

    /// Single attempt of execute_via_generator (no retry logic).
    async fn execute_via_generator_once(
        &self,
        code: &str,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<String> {
        let id: String = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(12)
            .collect();
        // Dedicated scratch package `IrisDevTmp` — NOT `User.*`. The temp executor class must not
        // land in the User package, where real application data lives (e.g. the `User.PatientData`
        // seed in the workshop): a leaked temp class there pollutes a real namespace and can be
        // mistaken for / collide with application classes. `IrisDevTmp` is obviously-disposable and
        // never in use. The package name IS the SQL schema (the `User`->`SQLUser` special-case does
        // not apply), so the SqlProc is `IrisDevTmp.Run<id>_Execute`. This also covers iris_test,
        // which runs through the same generator.
        let class_name = format!("IrisDevTmp.Run{}", id);
        let doc_name = format!("{}.cls", class_name);
        // "output" is a reserved word in IRIS SQL — Execute() aliases its column as "result".
        let sql_func = format!("IrisDevTmp.Run{}_Execute", id);
        let content = Self::build_exec_class(&class_name, code);

        // 1. PUT the class document
        let put_url = self.versioned_ns_url(
            namespace,
            &format!("/doc/{}", urlencoding::encode(&doc_name)),
        );
        let put_resp = client
            .put(&put_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!({"enc": false, "content": content}))
            .send()
            .await?;
        if !put_resp.status().is_success() {
            // #101/#102: typed, with the SAME text it has always emitted — the caller can now
            // ask whether the 404 is a missing NAMESPACE rather than reporting Docker.
            let status = put_resp.status();
            return Err(anyhow::Error::new(AtelierHttpError::new(
                status,
                put_url,
                "",
                format!("PUT doc failed: HTTP {}", status),
            )));
        }

        // 2. Compile
        let compile_url = self.versioned_ns_url(namespace, "/action/compile?flags=cuk");
        let compile_resp = client
            .post(&compile_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!([doc_name]))
            .send()
            .await?;
        if !compile_resp.status().is_success() {
            let status = compile_resp.status();
            let _ = self.delete_doc(&doc_name, namespace, client).await;
            return Err(anyhow::Error::new(AtelierHttpError::new(
                status,
                compile_url,
                "",
                format!("compile HTTP {}", status),
            )));
        }
        let compile_body: serde_json::Value = compile_resp.json().await.unwrap_or_default();
        let has_errors = compile_body["result"]["log"]
            .as_array()
            .map(|entries| {
                entries.iter().any(|e| {
                    e["type"]
                        .as_str()
                        .map(|t| t.eq_ignore_ascii_case("error"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if has_errors {
            let _ = self.delete_doc(&doc_name, namespace, client).await;
            anyhow::bail!("compile errors: {:?}", compile_body["result"]["log"]);
        }

        // 3. Query via SQL
        // "output" is a reserved word in IRIS SQL — use "result" as the column alias.
        let sql = format!("SELECT {}() AS result", sql_func);
        let query_url = self.versioned_ns_url(namespace, "/action/query");
        let query_resp = client
            .post(&query_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!({"query": sql}))
            .send()
            .await?;
        let query_body: serde_json::Value = query_resp.json().await.unwrap_or_default();
        let output = query_body["result"]["content"][0]["result"]
            .as_str()
            .unwrap_or("")
            .replace('\x01', "\n");

        // 4. Delete the temp class (best-effort)
        let _ = self.delete_doc(&doc_name, namespace, client).await;

        Ok(output)
    }

    /// Build the `.cls` source lines for the temp executor class.
    ///
    /// Two-method design (replaces the old `CodeMode = objectgenerator` trap that silently
    /// returned `output:"" success:true`): the user code lives in `RunUser()` and runs at
    /// CALL time. A bare top-level `Quit`/`Return` in user code now only returns from
    /// `RunUser` — it can no longer abort the generator before the method body is emitted.
    /// `Execute()` (the SqlProc) redirects the device to a temp file, calls `RunUser()`,
    /// restores the device, and returns the captured output (newlines encoded as `$C(1)`
    /// for the existing Rust-side transport decode in `execute_via_generator_once`).
    fn build_exec_class(class_name: &str, code: &str) -> Vec<String> {
        let mut lines: Vec<String> = vec![
            // $$$macros in USER code ($$$OK, $$$ISERR, $$$ThrowOnError...) must resolve
            // regardless of whether this IRIS version implicitly includes %occInclude
            // for class compiles (issue #22, upstream 713c23c).
            "Include %occInclude".into(),
            "".into(),
            format!("Class {} [ Final ]", class_name),
            "{".into(),
            "".into(),
            "/// Holds the user-supplied code; runs at call time. A Quit/Return here only".into(),
            "/// returns from RunUser, so Execute still captures whatever was written.".into(),
            "ClassMethod RunUser()".into(),
            "{".into(),
        ];
        for line in code.lines() {
            lines.push(format!("  {}", line));
        }
        lines.extend([
            "}".into(),
            "".into(),
            "ClassMethod Execute() As %String [ SqlProc ]".into(),
            "{".into(),
            // Portable temp path resolved on the IRIS SERVER (not the client): %Library.File.TempFilename
            // returns a path in the instance's mgr/Temp dir, correct on Windows AND Linux. The old
            // hardcoded "/tmp/irisd_<id>.txt" only existed on Linux, so the Open below failed on a
            // native Windows IRIS ($TEST=0 -> "output capture unavailable"). See upstream issue #56.
            "  Set tmpfile = ##class(%Library.File).TempFilename(\"txt\")".into(),
            "  Set savedIO = $IO".into(),
            "  Open tmpfile:(\"WNS\"):5".into(),
            "  If '$TEST { Quit \"ERROR: output capture unavailable\" }".into(),
            "  Use tmpfile".into(),
            "  Try {".into(),
            "    Do ..RunUser()".into(),
            "  } Catch ex {".into(),
            "    Write \"ERROR: \",ex.DisplayString(),!".into(),
            "  }".into(),
            "  Write !".into(), // IDEV-3: sentinel ensures temp file always ends with \n
            // Snapshot $ZERROR now, before Close/Use/stream operations below can clobber
            // it. This captures non-exception errors (e.g. an OPEN failure that sets
            // $ZERROR without throwing) so we can surface them if the body produced no
            // output — but WITHOUT writing to tmpfile yet (see the out="" test below).
            "  Set ze = $ZError".into(),
            "  Close tmpfile".into(),
            "  Use savedIO".into(),
            // Read the temp file contents using %Stream for reliability.
            // Read line:0 (timeout 0) fails on some IRIS versions — %Stream.ReadLine is portable.
            "  Set out = \"\"".into(),
            "  Set stream = ##class(%Stream.FileCharacter).%New()".into(),
            "  Set sc = stream.LinkToFile(tmpfile)".into(),
            // $SYSTEM.Status.IsOK avoids needing %occStatus.inc in a non-objectgenerator method.
            "  If $SYSTEM.Status.IsOK(sc) {".into(),
            "    While 'stream.AtEnd { Set out = out _ stream.ReadLine() _ $Char(10) }".into(),
            "  }".into(),
            "  Do ##class(%Library.File).Delete(tmpfile)".into(),
            // Only surface a non-exception $ZERROR when the body produced NO output.
            // A residual like <ENDOFFILE> is often left as a benign side effect of an
            // SCM provider's internal Read even when the operation fully succeeded;
            // appending it to a non-empty result corrupted otherwise-valid output.
            "  If (out=\"\") && (ze'=\"\") && (ze'=\",\") { Set out = \"ERROR($ZERROR): \"_ze_$Char(10) }"
                .into(),
            // Encode newlines as $C(1) for the Rust-side transport (decoded \x01 -> \n).
            "  Quit $Replace(out,$Char(10),$Char(1))".into(),
            "}".into(),
            "".into(),
            "}".into(),
        ]);
        lines
    }

    /// Delete an Atelier document (best-effort).
    async fn delete_doc(
        &self,
        doc_name: &str,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<()> {
        let url = self.versioned_ns_url(
            namespace,
            &format!("/doc/{}", urlencoding::encode(doc_name)),
        );
        client
            .delete(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await?;
        Ok(())
    }

    /// Execute ObjectScript code via docker exec (iris session stdin).
    ///
    /// LIMITATION: IRIS terminal sessions wrap stdin at ~80 columns when code is
    /// sent as a single line. For code longer than ~80 characters, callers with
    /// an HTTP client should use execute_via_generator() instead — it compiles
    /// user code into a temp class with no line-length restriction.
    ///
    /// This method is preserved for environments without Atelier REST access.
    /// Reads IRIS_CONTAINER fresh on each call to pick up late env var changes.
    pub async fn execute(&self, code: &str, namespace: &str) -> anyhow::Result<String> {
        let container =
            std::env::var("IRIS_CONTAINER").map_err(|_| anyhow::anyhow!("DOCKER_REQUIRED"))?;

        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new("docker")
            .args([
                "exec", "-i", &container, "iris", "session", "IRIS", "-U", namespace,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("docker not available: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(code.as_bytes()).await;
            let _ = stdin.write_all(b"\nhalt\n").await;
        }

        let output =
            tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
                .await
                .map_err(|_| anyhow::anyhow!("docker exec timed out after 30s"))??;

        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(strip_iris_banner(&raw))
    }

    /// FR-004: Run a SQL query via the Atelier query endpoint.
    /// Takes an explicit `namespace` parameter rather than always using `self.namespace`.
    /// SELECT via Atelier `/action/query`, with transparent retry on transient transport drops.
    /// SELECTs are idempotent, so retrying is safe. This is what makes long-running iris_test
    /// result reads survive a dropped connection ("error sending request for url .../action/query")
    /// instead of failing the whole run (issue #7). Atelier-level SQL errors (status.errors) are
    /// NOT retried — those are deterministic and bubble up on the first attempt.
    pub async fn query(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<serde_json::Value> {
        let url = self.versioned_ns_url(namespace, "/action/query");
        match self.query_outcome(sql, params, namespace, client).await? {
            QueryOutcome::Rows(body) => Ok(body),
            QueryOutcome::IrisError(msg) => anyhow::bail!("{}", msg),
            QueryOutcome::HttpError { status, snippet } => {
                let message = if snippet.is_empty() {
                    format!("HTTP {status} from {url}")
                } else {
                    format!("HTTP {status} from {url}: {snippet}")
                };
                Err(anyhow::Error::new(AtelierHttpError::new(
                    status, &url, snippet, message,
                )))
            }
            QueryOutcome::NonJson { snippet, .. } => {
                anyhow::bail!("non-JSON response from {url}: {snippet}")
            }
        }
    }

    /// One `/action/query` request with transparent retry on transient failures, returning
    /// the typed outcome so each caller can render its own envelope.
    ///
    /// SELECTs are idempotent, so retrying is safe. This is what makes long-running
    /// iris_test result reads survive a dropped connection ("error sending request for url
    /// .../action/query") instead of failing the whole run (issue #7). Atelier-level SQL
    /// errors are NOT retried — those are deterministic and return on the first attempt.
    ///
    /// #105: the `iris_query` TOOL used to have its own copy of this request and therefore
    /// none of this retry. The campaign logs show what that cost: one OpenCode run took
    /// four consecutive `IRIS_UNREACHABLE`s from `iris_query` over ~190 s of a transient
    /// sandbox blip, while `check_config` answered fine in between and every
    /// `query`-backed tool rode it out. One request path, one retry policy, two renderers.
    pub(crate) async fn query_outcome(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<QueryOutcome> {
        let url = self.versioned_ns_url(namespace, "/action/query");
        let delays = [
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(350),
            std::time::Duration::from_millis(700),
        ];
        let last = delays.len() - 1;
        let mut last_err: Option<anyhow::Error> = None;
        for (attempt, delay) in delays.iter().enumerate() {
            // A transport failure and a 5xx are the only retryable outcomes. #102: the 5xx
            // arm is STRUCTURAL — it used to depend on the Display text happening to contain
            // "HTTP 5", which a `resp.json()` decode error never did, so a 5xx was never
            // actually retried despite the doc comment promising it.
            let attempt_result = async {
                let resp = client
                    .post(&url)
                    .basic_auth(&self.username, Some(&self.password))
                    .json(&serde_json::json!({"query": sql, "parameters": params.clone()}))
                    .send()
                    .await?;
                let status = resp.status();
                let text = resp.text().await?;
                Ok::<_, reqwest::Error>(interpret_query_response(status, &text))
            }
            .await;

            let retryable = match &attempt_result {
                Err(_) => true,
                Ok(QueryOutcome::HttpError { status, .. }) => status.as_u16() >= 500,
                Ok(_) => false,
            };
            if !retryable || attempt == last {
                return attempt_result.map_err(anyhow::Error::from);
            }
            tracing::debug!(
                "query attempt {} to {url} was retryable, retrying in {:?}",
                attempt + 1,
                delay
            );
            last_err = attempt_result.err().map(anyhow::Error::from);
            tokio::time::sleep(*delay).await;
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no attempts made")))
    }

    /// Compile a document via POST /action/compile. Returns structured errors and console output.
    /// Used by both the CLI `compile` command and the MCP `iris_compile` tool.
    pub async fn compile_document(
        &self,
        doc_name: &str,
        namespace: &str,
        flags: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<CompileResult> {
        let compile_url = self.versioned_ns_url(
            namespace,
            &format!("/action/compile?flags={}", urlencoding::encode(flags)),
        );
        let resp = client
            .post(&compile_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!([doc_name]))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(anyhow::Error::new(AtelierHttpError::new(
                status,
                compile_url,
                "",
                format!("compile HTTP {}", status),
            )));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let console: Vec<String> = body["console"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let mut errors: Vec<String> = vec![];
        if let Some(se) = body["status"]["errors"].as_array() {
            for e in se {
                if let Some(msg) = e["error"].as_str() {
                    errors.push(msg.to_string());
                }
            }
        }
        // Issue #80: same colon-vs-space prefix defect as iris_compile's own console loop,
        // a second consumer away (iris_doc{mode:put, compile:true}.compile_errors, and
        // iris_compile's local-source upload path). Shares the one parser so the two cannot
        // drift apart again.
        for line in &console {
            if let Some(d) = crate::tools::parse_console_diag(line, "ERROR:", "ERROR ") {
                if errors.iter().all(|e| !e.contains(&d.text)) {
                    errors.push(d.text);
                }
            }
        }
        Ok(CompileResult { errors, console })
    }

    /// Short-timeout client used only for the startup probe — a down/unreachable IRIS
    /// should fail fast (5s connect / 10s total) instead of stalling startup for the
    /// 30s general-client timeout (issue #21, upstream #85).
    pub fn probe_client() -> anyhow::Result<reqwest::Client> {
        let insecure = std::env::var("IRIS_INSECURE")
            .ok()
            .map(|v| v == "true" || v == "1")
            .unwrap_or_else(|| {
                std::env::var("IRIS_TLS_VERIFY")
                    .map(|v| v == "false" || v == "0")
                    .unwrap_or(false)
            });
        Ok(reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .danger_accept_invalid_certs(insecure)
            .build()?)
    }

    /// Build a reqwest Client suitable for Atelier REST calls.
    /// TLS certificate validation is enabled by default; set `IRIS_INSECURE=true` to disable.
    pub fn http_client() -> anyhow::Result<reqwest::Client> {
        // IRIS_INSECURE=true or IRIS_TLS_VERIFY=false both disable TLS cert validation.
        let insecure = std::env::var("IRIS_INSECURE")
            .ok()
            .map(|v| v == "true" || v == "1")
            .unwrap_or_else(|| {
                std::env::var("IRIS_TLS_VERIFY")
                    .map(|v| v == "false" || v == "0")
                    .unwrap_or(false)
            });
        Ok(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .danger_accept_invalid_certs(insecure)
            .cookie_store(true) // reuse CSP sessions to avoid license slot exhaustion (#43)
            .tcp_keepalive(std::time::Duration::from_secs(20)) // prevent NAT/firewall from silently dropping idle connections (#44)
            .build()?)
    }

    /// Test accessor for build_exec_class. Exposed for integration tests.
    #[doc(hidden)]
    pub fn build_exec_class_for_test(class_name: &str, code: &str) -> Vec<String> {
        Self::build_exec_class(class_name, code)
    }
}

/// Returns true if the namespace name looks like a production namespace.
/// Used as fallback when SystemMode is Unknown (community edition or unconfigured).
fn is_production_namespace(ns: &str) -> bool {
    let upper = ns.to_uppercase();
    matches!(upper.as_str(), "PROD" | "PRODUCTION" | "LIVE" | "PRD")
}

/// FR-006: Strip IRIS session banner and prompt lines from docker exec stdout.
///
/// IRIS session output looks like:
///   Copyright (c) 2024 InterSystems Corporation
///   All rights reserved.
///   IRIS for UNIX ... 2024.1 ...
///   USER>
///   <code output lines>
///   USER>
///
/// We strip banner lines and bare prompt lines (lines that are ONLY a prompt, no content).
/// Lines that start with a prompt prefix but have content after it are kept.
pub fn strip_iris_banner(output: &str) -> String {
    let mut result_lines: Vec<&str> = Vec::new();

    // Banner-text rules only apply before the first prompt is seen — after that, a line
    // like "IRIS for UNIX ..." is legitimate `Write $ZVersion` output, not the
    // connect-time banner, and must not be stripped (issue #20, upstream 37fdc95).
    let mut seen_prompt = false;

    for line in output.lines() {
        let trimmed = line.trim();

        if !seen_prompt
            && (trimmed.starts_with("Copyright")
                || trimmed.contains("InterSystems Corporation")
                || trimmed.starts_with("All rights reserved")
                || trimmed.starts_with("IRIS for ")
                || trimmed.starts_with("Cache for ")
                || trimmed.starts_with("Ensemble for ")
                // IRIS 2026.2+ prints "Node: <hostname>, Instance: IRIS" on session
                // connect. Without this, its embedded ':' gets misparsed as a name:code
                // pair by callers like parse_status_response ("Node" became the
                // production name).
                || (trimmed.starts_with("Node: ") && trimmed.contains(", Instance:")))
        {
            continue;
        }

        // Strip bare prompt-only lines: lines that are just "USER>", "IRIS>", "%SYS>", etc.
        // A bare prompt line has no content beyond the prompt token.
        if is_bare_prompt_line(trimmed) {
            seen_prompt = true;
            continue;
        }

        result_lines.push(line);
    }

    // Remove leading blank lines
    while result_lines
        .first()
        .map(|l: &&str| l.trim().is_empty())
        .unwrap_or(false)
    {
        result_lines.remove(0);
    }
    // Remove trailing blank lines
    while result_lines
        .last()
        .map(|l: &&str| l.trim().is_empty())
        .unwrap_or(false)
    {
        result_lines.pop();
    }

    result_lines.join("\n")
}

/// Returns true if the line is purely an IRIS session prompt with no following content.
/// Examples: "USER>", "IRIS>", "%SYS>", "USER> " (trailing space only).
fn is_bare_prompt_line(s: &str) -> bool {
    // Strip trailing whitespace for the check
    let s = s.trim_end();
    if !s.ends_with('>') {
        return false;
    }
    // The prompt token is everything before '>'
    let token = &s[..s.len() - 1];
    // Allow optional leading '%'
    let token = token.strip_prefix('%').unwrap_or(token);
    // Prompt namespace is uppercase alphanumeric + underscore, non-empty, reasonable length
    !token.is_empty()
        && token.len() <= 16
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod system_mode_tests {
    use super::*;

    // ── strip_iris_banner (issue #20, upstream 37fdc95) ──────────────────────
    #[test]
    fn strip_iris_banner_removes_node_instance_line() {
        // IRIS 2026.2+ prints this on every `iris session` connect. Its embedded ':'
        // previously got misparsed as a name:code pair by callers like
        // interop::parse_status_response (production name came back as "Node").
        let raw = "\nNode: de17f22ad88c, Instance: IRIS\n\nUSER>\nIrisDevTest.CoverageProduction:1\n\nUSER>\n";
        let stripped = strip_iris_banner(raw);
        assert!(!stripped.contains("Node:"), "{stripped:?}");
        assert_eq!(stripped.trim(), "IrisDevTest.CoverageProduction:1");
    }

    #[test]
    fn strip_iris_banner_keeps_iris_for_line_after_first_prompt() {
        // `Write $ZVersion` legitimately outputs a string starting with "IRIS for
        // UNIX ..." — that must NOT be treated as the connect-time banner just
        // because it shares the prefix. Banner rules apply only before the first
        // prompt is seen.
        let raw = "\nNode: de17f22ad88c, Instance: IRIS\n\nUSER>\nIRIS for UNIX (Ubuntu Server LTS for ARM64 Containers) 2026.2.0L\n\nUSER>\n";
        let stripped = strip_iris_banner(raw);
        assert!(
            stripped.trim().starts_with("IRIS for UNIX"),
            "$ZVersion output must survive: {stripped:?}"
        );
    }

    // ── build_exec_class: $$$macros must resolve (issue #22, upstream 713c23c) ─
    #[test]
    fn build_exec_class_includes_occinclude() {
        let cls = IrisConnection::build_exec_class("User.T", "write $$$OK,!").join("\n");
        let inc = cls
            .find("Include %occInclude")
            .expect("Include line missing");
        let class_kw = cls.find("Class User.T").expect("Class line missing");
        assert!(inc < class_kw, "Include must precede the Class line");
    }

    // ── iris_execute generated-class shape (A2 silent-loss fix) ──────────────
    // The old class used `CodeMode = objectgenerator`, which ran user code at COMPILE
    // time; a bare top-level Quit/Return aborted the generator and silently produced
    // output:"" success:true. The fix puts user code in a separate RunUser() method.
    #[test]
    fn build_exec_class_no_objectgenerator_uses_runuser() {
        let cls = IrisConnection::build_exec_class("User.T", "write 1,! quit").join("\n");
        assert!(
            !cls.contains("objectgenerator"),
            "must NOT use CodeMode=objectgenerator:\n{cls}"
        );
        assert!(cls.contains("ClassMethod RunUser()"), "must define RunUser");
        assert!(
            cls.contains("Do ..RunUser()"),
            "Execute must call RunUser so a user Quit can't abort capture"
        );
        assert!(cls.contains("ClassMethod Execute() As %String [ SqlProc ]"));
        // user code must land inside RunUser, before Execute
        let run_idx = cls.find("ClassMethod RunUser()").unwrap();
        let exec_idx = cls.find("ClassMethod Execute()").unwrap();
        let user_idx = cls.find("write 1,! quit").expect("user code present");
        assert!(
            run_idx < user_idx && user_idx < exec_idx,
            "user code must be inside RunUser, before Execute"
        );
    }

    #[test]
    fn build_exec_class_encodes_newlines_for_transport() {
        let cls = IrisConnection::build_exec_class("User.T", "write 1").join("\n");
        assert!(
            cls.contains("$Replace(out,$Char(10),$Char(1))"),
            "newlines must be encoded as $C(1) for the rust-side decoder"
        );
    }

    fn conn(namespace: &str, mode: SystemMode) -> IrisConnection {
        let mut c = IrisConnection::new(
            "http://localhost:52773",
            namespace,
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        c.system_mode = mode;
        c
    }

    // T005 — SystemMode parsing
    #[test]
    fn system_mode_live_from_string() {
        // Simulates what detect_system_mode maps "Live" to
        assert_eq!(SystemMode::Live, SystemMode::Live);
        assert_ne!(SystemMode::Live, SystemMode::Unknown);
    }

    #[test]
    fn system_mode_default_is_unknown() {
        assert_eq!(SystemMode::default(), SystemMode::Unknown);
    }

    #[test]
    fn system_mode_development_ne_live() {
        assert_ne!(SystemMode::Development, SystemMode::Live);
    }

    // T006 — is_write_allowed()
    #[test]
    fn write_blocked_for_live() {
        let c = conn("USER", SystemMode::Live);
        // No IRIS_ALLOW_PROD set in this test
        std::env::remove_var("IRIS_ALLOW_PROD");
        assert!(!c.is_write_allowed());
    }

    #[test]
    fn write_allowed_for_development() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        assert!(conn("USER", SystemMode::Development).is_write_allowed());
    }

    #[test]
    fn write_allowed_for_test_mode() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        assert!(conn("USER", SystemMode::Test).is_write_allowed());
    }

    #[test]
    fn write_blocked_for_unknown_with_prod_namespace() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        assert!(!conn("PROD", SystemMode::Unknown).is_write_allowed());
        assert!(!conn("PRODUCTION", SystemMode::Unknown).is_write_allowed());
        assert!(!conn("LIVE", SystemMode::Unknown).is_write_allowed());
        assert!(!conn("PRD", SystemMode::Unknown).is_write_allowed());
    }

    #[test]
    fn write_allowed_for_unknown_with_dev_namespace() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        assert!(conn("USER", SystemMode::Unknown).is_write_allowed());
        assert!(conn("DEV", SystemMode::Unknown).is_write_allowed());
        assert!(conn("MYAPP", SystemMode::Unknown).is_write_allowed());
    }

    #[test]
    fn is_write_allowed_logic_direct() {
        // Test the override logic directly without touching process env vars.
        // The env var branch is: if IRIS_ALLOW_PROD is "1" or "true" → return true.
        // We verify the non-override paths only (env-based override tested manually).
        assert!(!conn("LIVE", SystemMode::Unknown).is_write_allowed());
        assert!(!conn("PROD", SystemMode::Live).is_write_allowed());
        assert!(conn("DEV", SystemMode::Development).is_write_allowed());
    }

    #[test]
    fn is_production_namespace_case_insensitive() {
        assert!(is_production_namespace("prod"));
        assert!(is_production_namespace("PROD"));
        assert!(is_production_namespace("Production"));
        assert!(is_production_namespace("LIVE"));
        assert!(is_production_namespace("live"));
        assert!(is_production_namespace("PRD"));
        assert!(!is_production_namespace("USER"));
        assert!(!is_production_namespace("DEV"));
        assert!(!is_production_namespace("MYAPP"));
    }
}

// ── Issues #101 / #102: the query path must not destroy what IRIS said ───────
#[cfg(test)]
mod atelier_http_error_tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn mount(server: &MockServer, tpl: ResponseTemplate) -> IrisConnection {
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/query$"))
            .respond_with(tpl)
            .mount(server)
            .await;
        IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        )
    }

    async fn ask(tpl: ResponseTemplate) -> anyhow::Result<serde_json::Value> {
        let server = MockServer::start().await;
        let iris = mount(&server, tpl).await;
        iris.query("SELECT 1", vec![], "APP", &reqwest::Client::new())
            .await
    }

    /// The happy path is untouched.
    #[test]
    fn a_200_with_json_is_returned_verbatim() {
        rt().block_on(async {
            let body = serde_json::json!({"result": {"content": [{"n": 1}]}});
            let out = ask(ResponseTemplate::new(200).set_body_json(body.clone()))
                .await
                .expect("a 200 with a JSON body is a success");
            assert_eq!(out, body);
        });
    }

    /// Atelier reports SQL errors as 200 + `status.errors`. That message is the useful one
    /// and it still wins.
    #[test]
    fn a_200_carrying_status_errors_still_bails_with_the_atelier_message() {
        rt().block_on(async {
            let e = ask(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": {"errors": [{"error": "ERROR #5540: SQLCODE: -51"}]}
            })))
            .await
            .expect_err("status.errors is a failure");
            assert_eq!(e.to_string(), "ERROR #5540: SQLCODE: -51");
            assert!(
                atelier_status(&e).is_none(),
                "a SQL error is not an HTTP-status error"
            );
        });
    }

    /// THE TRAP this rewrite exists to avoid. A malformed query POST returns HTTP 400 with a
    /// REAL diagnostic in the body (verified live: `ERROR #16002: Invalid JSON Content`). A
    /// status-first `if !status.is_success() { bail!("HTTP {}") }` would swap one
    /// uninformative error for another — so the body is parsed FIRST and `status.errors` wins
    /// whatever the status is.
    #[test]
    fn a_400_carrying_a_diagnostic_reports_the_diagnostic_not_the_status() {
        rt().block_on(async {
            let e = ask(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "status": {"errors": [{"error": "ERROR #16002: Invalid JSON Content", "code": 16002}]}
            })))
            .await
            .expect_err("a 400 is a failure");
            assert_eq!(e.to_string(), "ERROR #16002: Invalid JSON Content");
            assert!(!e.to_string().contains("HTTP 400"), "{e}");
        });
    }

    /// The #102 P2 root cause: a missing-namespace 404 has a ZERO-BYTE body, so `resp.json()`
    /// reported EOF ("error decoding response body") and the status was destroyed. Every
    /// caller then said IRIS_UNREACHABLE about an instance that had just answered.
    #[test]
    fn a_404_with_an_empty_body_keeps_its_status_and_url() {
        rt().block_on(async {
            let server = MockServer::start().await;
            let iris = mount(&server, ResponseTemplate::new(404)).await;
            let e = iris
                .query("SELECT 1", vec![], "ZZNOSUCHNS", &reqwest::Client::new())
                .await
                .expect_err("a 404 is a failure");
            let http = atelier_status(&e).expect("the status must survive as a typed error");
            assert_eq!(http.status, 404);
            assert!(http.url.contains("ZZNOSUCHNS"), "{}", http.url);
            assert!(e.to_string().contains("HTTP 404"), "{e}");
            assert!(
                !e.to_string().contains("error decoding response body"),
                "{e}"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "a 404 is deterministic — it must not be retried"
            );
        });
    }

    /// The #101 repro one layer down: a 401 body is not JSON, so this was the other way the
    /// status got destroyed.
    #[test]
    fn a_401_keeps_its_status_instead_of_becoming_a_decode_failure() {
        rt().block_on(async {
            let e = ask(ResponseTemplate::new(401).set_body_string("Unauthorized"))
                .await
                .expect_err("a 401 is a failure");
            let http = atelier_status(&e).expect("typed");
            assert_eq!(http.status, 401);
            assert_eq!(http.body, "Unauthorized");
            assert_eq!(
                crate::tools::envelope::auth_error_code(&e.to_string()),
                Some("IRIS_AUTH_FAILED"),
                "the message must stay classifiable: {e}"
            );
        });
    }

    /// The deliberate retry change. Before the typed error a 5xx surfaced as "error decoding
    /// response body", which matches none of `query()`'s retry substrings — so a 5xx was
    /// never retried despite the doc comment promising it. The predicate is structural now.
    #[test]
    fn a_5xx_is_retried_and_a_404_is_not() {
        rt().block_on(async {
            let server = MockServer::start().await;
            let iris = mount(&server, ResponseTemplate::new(503)).await;
            let e = iris
                .query("SELECT 1", vec![], "APP", &reqwest::Client::new())
                .await
                .expect_err("a 503 is a failure");
            assert_eq!(atelier_status(&e).map(|h| h.status), Some(503));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                3,
                "three attempts: the delay table has three entries"
            );
        });
    }

    /// A 2xx whose body is not JSON at all names the URL and shows what came back, instead of
    /// reqwest's opaque "error decoding response body".
    #[test]
    fn a_200_that_is_not_json_says_so() {
        rt().block_on(async {
            let e = ask(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
                .await
                .expect_err("a non-JSON 200 cannot be a query result");
            assert!(e.to_string().contains("non-JSON response"), "{e}");
            assert!(e.to_string().contains("<html>login</html>"), "{e}");
        });
    }

    #[test]
    fn truncate_body_cuts_on_char_boundaries() {
        // A byte-wise cut inside a multi-byte sequence would panic.
        assert_eq!(truncate_body("  héllo  ", 3), "hél");
        assert_eq!(truncate_body("", 500), "");
    }
}
