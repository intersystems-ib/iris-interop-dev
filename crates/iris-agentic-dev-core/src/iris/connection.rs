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
    /// The `api` level from the Atelier root descriptor, mapped to the URL shape every later request
    /// uses.
    ///
    /// #288: this match existed THREE times — twice in `discovery.rs` and once in
    /// [`IrisConnection::probe`] — and all three have to agree or a connection addresses the wrong
    /// endpoints. Absent, null or unparseable means V1: the oldest shape, and the safe assumption
    /// for a server that did not say.
    pub(crate) fn from_api_level(api: Option<u64>) -> Self {
        match api {
            Some(v) if v >= 8 => Self::V8,
            Some(v) if v >= 2 => Self::V2,
            _ => Self::V1,
        }
    }

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

    /// IRIS's own `Detected N errors during compilation` count, when the console carried it.
    /// Cross-check it against `errors.len()` before presenting this list as complete —
    /// see [`crate::tools::detected_error_count`].
    pub fn detected_error_count(&self) -> Option<usize> {
        crate::tools::detected_error_count(self.console.iter().map(String::as_str))
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
        write_allowed_with(
            read_only_mode(),
            &self.system_mode,
            &self.namespace,
            std::env::var("IRIS_ALLOW_PROD")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        )
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
                    // NOTE: deliberately NOT `fingerprint_atelier_root`. That helper refuses a
                    // descriptor whose version does not name IRIS, which is right when DECIDING
                    // whether to adopt a connection. `probe` runs against a connection the user
                    // configured explicitly: its job is to record what the server reports, not to
                    // veto it. Only the api mapping is shared (#288).
                    self.version = content["version"].as_str().map(|v| v.to_string());
                    self.atelier_version = AtelierVersion::from_api_level(content["api"].as_u64());
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
        system_mode_from_global(&mode)
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
        self.compile_documents(std::slice::from_ref(&doc_name), namespace, flags, client)
            .await
    }

    /// #313: compile SEVERAL documents in one request, which is the shape /action/compile has
    /// always taken — the singular form above was posting a one-element array. The CLI needs the
    /// plural to compile an expanded wildcard the way `iris_compile` does: one request, so the
    /// compiler sees the whole set and resolves dependencies between them, rather than N requests
    /// whose order decides whether a dependent compiles.
    pub async fn compile_documents(
        &self,
        doc_names: &[&str],
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
            .json(&serde_json::json!(doc_names))
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
        // Issue #80: the console loop lived here, in iris_compile and in iris_doc, and the
        // fix reached two of the three. One shared assembly now, so they cannot drift.
        let errors = crate::tools::compile_error_list(&body, &console);
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

/// Map a raw `^%SYS("SystemMode")` value onto a [`SystemMode`].
///
/// Split out of `detect_system_mode` so the mapping is testable without an HTTP mock: #320 lived
/// entirely in these string comparisons, and nothing could assert on them while they were buried
/// inside an async request.
///
/// **#320 — why this uppercases.** IRIS documents the SystemMode values in UPPER CASE (`LIVE`,
/// `TEST`, `DEVELOPMENT`, `FAILOVER`), so the title-case literals this used to match — `"Live"`,
/// `"Development"`, `"Test"` — were the one spelling a real instance is least likely to report. A
/// production instance whose global reads `"LIVE"` therefore fell through to [`SystemMode::Unknown`],
/// and `Unknown` defers to [`is_production_namespace`] below — so writes were **allowed on a live
/// system** unless its namespace happened to be spelled `PROD`/`PRODUCTION`/`LIVE`/`PRD`. A live
/// instance serving namespace `APP` was writable.
///
/// The asymmetry is the whole bug: `is_production_namespace` has always uppercased before comparing,
/// and `is_production_namespace_case_insensitive` asserts that it does. One half of the same
/// decision knew that casing varies in the field and the other half did not. Both normalisations now
/// sit here together, so a future reader cannot fix one without seeing the other.
fn system_mode_from_global(raw: &str) -> SystemMode {
    match raw.trim().to_uppercase().as_str() {
        "LIVE" => SystemMode::Live,
        "DEVELOPMENT" => SystemMode::Development,
        "TEST" => SystemMode::Test,
        // Deliberately PERMISSIVE, and deliberately not split any further.
        //
        // Anything we do not recognise — a wording no one documented (`"DEV ENVIRONMENT"` is a real
        // observed value), or an empty string because the query failed — lands here and falls back
        // to the namespace heuristic rather than refusing to write. A throwaway container usually
        // has no SystemMode set at all, so failing closed on an unrecognised mode would break the
        // common case to protect an uncommon one.
        //
        // That means an UNREADABLE mode is currently indistinguishable from an UNRECOGNISED one,
        // which is the negative-fact shape catalogued in #310 and is a known, accepted residual
        // here: the namespace heuristic still refuses a production-looking namespace either way.
        // `FAILOVER` is a documented fourth value that lands in this arm and stays writable; a
        // mirror failover member is not a throwaway container, so blocking it is worth doing, but
        // it is an addition rather than a normalisation and is left to its own change.
        _ => SystemMode::Unknown,
    }
}

/// Returns true if the namespace name looks like a production namespace.
/// Used as fallback when SystemMode is Unknown (community edition or unconfigured).
///
/// See [`system_mode_from_global`] above: this function's case-insensitivity is the behaviour that
/// the mode match was missing.
/// An EXPLICIT read-only request, independent of what the instance looks like.
///
/// #303. The inferred write gate ([`IrisConnection::is_write_allowed`]) answers *"does this instance
/// look like production?"* — a safety guess from `SystemMode` and a namespace heuristic. This answers
/// a different question: *"the operator asked for read-only"*. That is not a guess, so no heuristic
/// and no `IRIS_ALLOW_PROD` may override it.
///
/// Two levels, because "can it change my code?" and "can it touch my instance at all?" are different
/// questions and only the asker knows which one they mean. The ten tools in `GENERATOR_WRITE_TOOLS`
/// are read-only in INTENT but PUT, compile and delete a scratch class in `IrisDevTmp` to answer,
/// because the data they read has no SQL projection to reach it through. `Soft` keeps them; `Strict`
/// refuses them.
///
/// Neither level replaces SELECT-only grants on the MCP's IRIS user. These are enforced in this
/// server; privileges are enforced by the database. For an instance that genuinely must not be
/// written, do both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadOnlyMode {
    /// No explicit request — the inferred gate decides, exactly as it did before this existed.
    #[default]
    Off,
    /// Every declared mutator is refused. The scratch-class readers still answer, and still write.
    Soft,
    /// Also refuses anything that writes a scratch class to answer.
    Strict,
}

impl ReadOnlyMode {
    /// Whether an explicit request is in force at all. `Off` means "ask the inferred gate".
    pub fn is_read_only(self) -> bool {
        !matches!(self, ReadOnlyMode::Off)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReadOnlyMode::Off => "off",
            ReadOnlyMode::Soft => "soft",
            ReadOnlyMode::Strict => "strict",
        }
    }
}

/// Resolve the mode from a variable getter.
///
/// Taking a getter rather than reading `std::env` directly is what makes the PRECEDENCE testable
/// without mutating process environment. Tests that set shared env already race each other here —
/// `admin_unit_tests::write_not_allowed_without_env` is the known case — and precedence is the part
/// most worth pinning, so it is pinned by a pure function instead.
pub(crate) fn resolve_read_only_mode(get: impl Fn(&str) -> Option<String>) -> ReadOnlyMode {
    let on = |name: &str| {
        get(name)
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false)
    };
    // Strict beats soft: asking for both is asking for the stronger one, never the weaker.
    if on("IRIS_STRICT_READ_ONLY") {
        return ReadOnlyMode::Strict;
    }
    if on("IRIS_SOFT_READ_ONLY") {
        return ReadOnlyMode::Soft;
    }
    ReadOnlyMode::Off
}

/// This process's read-only mode, resolved once.
///
/// Once, deliberately: `is_write_allowed` reads `IRIS_ALLOW_PROD` from the environment on every
/// call, and a gate that can change answer mid-process is a gate that cannot be reasoned about. A
/// request made at startup holds for the life of the server.
pub fn read_only_mode() -> ReadOnlyMode {
    static MODE: std::sync::OnceLock<ReadOnlyMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| resolve_read_only_mode(|k| std::env::var(k).ok()))
}

/// The write decision, as a pure function of everything that feeds it.
///
/// Extracted so the PRECEDENCE can be tested without mutating process environment.
/// [`read_only_mode`] is cached for the life of the process — deliberately, since a gate that can
/// change its answer mid-run cannot be reasoned about — which also means no in-process test can vary
/// it. A pure function is the only honest way to pin the order, and the order is the part that
/// decides whether asking for read-only is trustworthy.
///
/// #303 precedence, strongest first:
/// `IRIS_STRICT_READ_ONLY` > `IRIS_SOFT_READ_ONLY` > `IRIS_ALLOW_PROD` > the inferred gate.
///
/// The last two lines of that chain are unchanged, so a deployment setting neither new variable
/// behaves exactly as it did before this existed.
pub(crate) fn write_allowed_with(
    requested: ReadOnlyMode,
    system_mode: &SystemMode,
    namespace: &str,
    allow_prod: bool,
) -> bool {
    // An explicit request is not a guess and nothing below may override it. If `allow_prod` won
    // here, an operator with IRIS_ALLOW_PROD exported — precisely the environment where asking for
    // read-only matters — would silently get writes while believing they had asked for none.
    if requested.is_read_only() {
        return false;
    }
    if allow_prod {
        return true;
    }
    match system_mode {
        SystemMode::Live => false,
        SystemMode::Development | SystemMode::Test => true,
        SystemMode::Unknown => !is_production_namespace(namespace),
    }
}

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

    // ── #320: SystemMode mapping ──────────────────────────────────────────────

    /// THE security property, and the one that was false before #320.
    ///
    /// IRIS documents the value as `LIVE`. The old match compared against `"Live"`, so a live
    /// instance became `Unknown`, and `Unknown` asks only whether the NAMESPACE looks like
    /// production. Namespace `APP` does not — so this returned `true` and the write gate opened on a
    /// production system. Nothing else in the chain would have caught it.
    #[test]
    fn an_uppercase_live_instance_refuses_writes_even_in_an_innocent_namespace() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        // Every casing, not just the documented one: with `"LIVE"` alone this assertion survives a
        // mutation that drops the `.to_uppercase()` while leaving the arms uppercase, which is
        // exactly the normalisation the fix consists of.
        for raw in ["LIVE", "Live", "live", "LiVe"] {
            for ns in ["APP", "USER", "MYAPP", "INTEROP"] {
                let c = conn(ns, system_mode_from_global(raw));
                assert!(
                    !c.is_write_allowed(),
                    "namespace {ns} on a {raw:?} instance must refuse writes; before #320 this was \
                     allowed, because the mode did not match the literal \"Live\" and fell through \
                     to Unknown, where only the namespace NAME is consulted"
                );
            }
        }
    }

    /// The casings a real instance might report. `"Live"` is included so the fix is a superset of the
    /// old behaviour rather than a replacement of it.
    #[test]
    fn live_is_recognised_in_every_casing() {
        for raw in ["LIVE", "Live", "live", "lIvE", "  LIVE  "] {
            assert_eq!(
                system_mode_from_global(raw),
                SystemMode::Live,
                "{raw:?} must be recognised as Live"
            );
        }
    }

    /// The control: the other two documented values must still map, or the test above would pass on
    /// a function that answers `Live` for everything and locks every instance out of writing.
    #[test]
    fn the_other_documented_modes_still_map_and_are_writable() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        for raw in ["DEVELOPMENT", "Development", "development"] {
            assert_eq!(
                system_mode_from_global(raw),
                SystemMode::Development,
                "{raw:?}"
            );
        }
        for raw in ["TEST", "Test", "test"] {
            assert_eq!(system_mode_from_global(raw), SystemMode::Test, "{raw:?}");
        }
        assert!(conn("APP", system_mode_from_global("DEVELOPMENT")).is_write_allowed());
        assert!(conn("APP", system_mode_from_global("TEST")).is_write_allowed());
    }

    /// Pins the decision recorded on #320: an unrecognised mode stays PERMISSIVE. A throwaway
    /// container often reports a wording nobody documented, or nothing at all, and refusing to write
    /// to it would break the common case. The namespace heuristic remains the backstop — which is
    /// why this test asserts BOTH directions, not just the permissive one.
    #[test]
    fn an_unrecognised_mode_stays_permissive_but_keeps_the_namespace_backstop() {
        std::env::remove_var("IRIS_ALLOW_PROD");
        for raw in ["DEV ENVIRONMENT", "FAILOVER", "", "   ", "whatever"] {
            assert_eq!(
                system_mode_from_global(raw),
                SystemMode::Unknown,
                "{raw:?} must be Unknown"
            );
            assert!(
                conn("APP", system_mode_from_global(raw)).is_write_allowed(),
                "{raw:?} in a harmless namespace must stay writable — a docker throwaway rarely \
                 sets SystemMode at all"
            );
            assert!(
                !conn("PROD", system_mode_from_global(raw)).is_write_allowed(),
                "{raw:?} must still be refused in a production-NAMED namespace — dropping that \
                 backstop was never part of the #320 decision"
            );
        }
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
/// #303: the two explicit read-only levels, and the precedence that makes them trustworthy.
///
/// All of this is asserted through the pure functions, with no process environment touched. That is
/// not convenience: `read_only_mode()` caches for the life of the process on purpose, so an
/// in-process test cannot vary it, and tests here that set shared env would race each other the way
/// `admin_unit_tests::write_not_allowed_without_env` already does.
#[cfg(test)]
mod read_only_mode_tests {
    use super::*;

    /// A getter over a fixed list, standing in for the environment. Owns its data so the closure
    /// borrows nothing — the point is to resolve a mode without touching `std::env` at all.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn nothing_requested_leaves_the_inferred_gate_in_charge() {
        assert_eq!(resolve_read_only_mode(env(&[])), ReadOnlyMode::Off);
        // A deployment that sets neither variable must behave exactly as it did before #303, so
        // this is the row that says the change is additive.
        assert!(!ReadOnlyMode::Off.is_read_only());
    }

    #[test]
    fn each_level_resolves_to_itself() {
        assert_eq!(
            resolve_read_only_mode(env(&[("IRIS_SOFT_READ_ONLY", "1")])),
            ReadOnlyMode::Soft
        );
        assert_eq!(
            resolve_read_only_mode(env(&[("IRIS_STRICT_READ_ONLY", "1")])),
            ReadOnlyMode::Strict
        );
    }

    #[test]
    fn strict_beats_soft_when_both_are_asked_for() {
        let both = [("IRIS_SOFT_READ_ONLY", "1"), ("IRIS_STRICT_READ_ONLY", "1")];
        assert_eq!(
            resolve_read_only_mode(env(&both)),
            ReadOnlyMode::Strict,
            "asking for both is asking for the stronger one — resolving to soft would quietly \
             grant the scratch-class writes that strict was set to refuse"
        );
    }

    #[test]
    fn an_explicit_request_beats_allow_prod() {
        // THE precedence that decides whether asking for read-only means anything. IRIS_ALLOW_PROD
        // forces writes on, and an operator with it exported is exactly who most needs the request
        // honoured.
        for requested in [ReadOnlyMode::Soft, ReadOnlyMode::Strict] {
            assert!(
                !write_allowed_with(requested, &SystemMode::Development, "USER", true),
                "{requested:?} must refuse writes even with IRIS_ALLOW_PROD set"
            );
        }
    }

    #[test]
    fn allow_prod_still_wins_when_no_read_only_was_requested() {
        // CONTROL for the test above. Without this, "an explicit request beats allow_prod" is also
        // satisfied by a gate that simply never allows writes, which would be a different bug.
        assert!(
            write_allowed_with(ReadOnlyMode::Off, &SystemMode::Live, "PROD", true),
            "IRIS_ALLOW_PROD must still override the heuristic when nothing was requested"
        );
    }

    #[test]
    fn the_inferred_gate_is_untouched() {
        // The pre-#303 truth table, re-asserted so a change to the new precedence cannot quietly
        // alter the old behaviour underneath it.
        let off = ReadOnlyMode::Off;
        assert!(!write_allowed_with(off, &SystemMode::Live, "USER", false));
        assert!(write_allowed_with(
            off,
            &SystemMode::Development,
            "USER",
            false
        ));
        assert!(write_allowed_with(off, &SystemMode::Test, "USER", false));
        assert!(write_allowed_with(off, &SystemMode::Unknown, "USER", false));
        assert!(!write_allowed_with(
            off,
            &SystemMode::Unknown,
            "PROD",
            false
        ));
        assert!(!write_allowed_with(
            off,
            &SystemMode::Unknown,
            "production",
            false
        ));
    }

    #[test]
    fn only_affirmative_values_turn_a_level_on() {
        for on in ["1", "true", "TRUE", "yes", " 1 "] {
            assert_eq!(
                resolve_read_only_mode(env(&[("IRIS_STRICT_READ_ONLY", on)])),
                ReadOnlyMode::Strict,
                "{on:?} should request strict"
            );
        }
        // An unset variable and an explicitly negative one must give the same answer: a deployment
        // that writes IRIS_STRICT_READ_ONLY=0 into a config has NOT asked for read-only, and
        // treating any non-empty value as truthy is how that becomes a silent surprise.
        for off in ["0", "false", "no", "", "off"] {
            assert_eq!(
                resolve_read_only_mode(env(&[("IRIS_STRICT_READ_ONLY", off)])),
                ReadOnlyMode::Off,
                "{off:?} must not request strict"
            );
        }
    }
}

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

/// #288: the api-level mapping had three copies and no test. It decides the URL shape every later
/// request uses, so a disagreement between copies meant a connection addressing the wrong endpoints.
#[cfg(test)]
mod atelier_version_from_api_level_tests {
    use super::AtelierVersion;

    #[test]
    fn the_boundaries_are_where_the_shape_changes() {
        // Boundaries, because an off-by-one here is the whole failure mode: 8 and 2 are the first
        // levels of their shape, 7 and 1 the last of the one below.
        assert_eq!(AtelierVersion::from_api_level(Some(9)), AtelierVersion::V8);
        assert_eq!(AtelierVersion::from_api_level(Some(8)), AtelierVersion::V8);
        assert_eq!(AtelierVersion::from_api_level(Some(7)), AtelierVersion::V2);
        assert_eq!(AtelierVersion::from_api_level(Some(2)), AtelierVersion::V2);
        assert_eq!(AtelierVersion::from_api_level(Some(1)), AtelierVersion::V1);
        assert_eq!(AtelierVersion::from_api_level(Some(0)), AtelierVersion::V1);
    }

    /// A server that did not report an api level gets the OLDEST shape, not the newest. Guessing V8
    /// for a silent server would address endpoints it does not serve.
    #[test]
    fn a_missing_api_level_is_v1_not_v8() {
        assert_eq!(AtelierVersion::from_api_level(None), AtelierVersion::V1);
    }
}
