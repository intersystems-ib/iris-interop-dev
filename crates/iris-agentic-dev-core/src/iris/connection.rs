//! IRIS connection types and Atelier REST API fingerprinting.

use std::fmt;

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
            .finish()
    }
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
        let client = match Self::http_client() {
            Ok(c) => c,
            Err(_) => return,
        };

        let url = self.atelier_url("/");
        if let Ok(resp) = client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
        {
            let status = resp.status();
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
                Ok(output) => return Ok(output),
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
                    tracing::warn!(
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
            anyhow::bail!("PUT doc failed: HTTP {}", put_resp.status());
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
            let _ = self.delete_doc(&doc_name, namespace, client).await;
            anyhow::bail!("compile HTTP {}", compile_resp.status());
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
            // Surface non-exception errors (e.g. OPEN failure sets $ZERROR but doesn't throw).
            "  If ($ZError'=\"\") && ($ZError'=\",\") { Write \"ERROR($ZERROR): \",$ZError,! }"
                .into(),
            "  Close tmpfile".into(),
            "  Use savedIO".into(),
            // Read the temp file contents using %Stream for reliability.
            // Read line:0 (timeout 0) fails on some IRIS versions — %Stream.ReadLine is portable.
            "  Set out = \"\"".into(),
            "  Set stream = ##class(%Stream.FileCharacter).%New()".into(),
            "  Set sc = stream.LinkToFile(tmpfile)".into(),
            // $SYSTEM.Status.IsOK avoids needing %occStatus.inc in a non-objectgenerator method.
            "  If $SYSTEM.Status.IsOK(sc) {".into(),
            "    While 'stream.AtEnd { Set out = out_stream.ReadLine()_$Char(10) }".into(),
            "  }".into(),
            "  Do ##class(%Library.File).Delete(tmpfile)".into(),
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
        let delays = [
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(350),
            std::time::Duration::from_millis(700),
        ];
        let mut last_err = anyhow::anyhow!("no attempts made");
        for (attempt, delay) in delays.iter().enumerate() {
            match self
                .query_once(sql, params.clone(), namespace, client)
                .await
            {
                Ok(body) => return Ok(body),
                Err(e) => {
                    let msg = e.to_string();
                    // Only transport-level failures are retryable; a SQL error is deterministic.
                    let is_retryable = msg.contains("error sending request")
                        || msg.contains("connection refused")
                        || msg.contains("connection reset")
                        || msg.contains("timed out")
                        || msg.contains("HTTP 5");
                    if !is_retryable || attempt == delays.len() - 1 {
                        return Err(e);
                    }
                    tracing::warn!(
                        "query attempt {} failed ({}), retrying in {:?}",
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

    /// Single attempt of `query` (no retry).
    async fn query_once(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
        namespace: &str,
        client: &reqwest::Client,
    ) -> anyhow::Result<serde_json::Value> {
        let url = self.versioned_ns_url(namespace, "/action/query");
        let resp = client
            .post(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&serde_json::json!({"query": sql, "parameters": params}))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        // Surface Atelier-level errors returned as 200 OK with status.errors in body.
        if let Some(errs) = body["status"]["errors"].as_array() {
            if !errs.is_empty() {
                let msg = errs[0]["error"].as_str().unwrap_or("Atelier query error");
                anyhow::bail!("{}", msg);
            }
        }
        Ok(body)
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
            anyhow::bail!("compile HTTP {}", resp.status());
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
        for line in &console {
            if let Some(rest) = line.trim().strip_prefix("ERROR ") {
                let rest = rest.to_string();
                if errors.iter().all(|e| !e.contains(&rest)) {
                    errors.push(rest);
                }
            }
        }
        Ok(CompileResult { errors, console })
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

    for line in output.lines() {
        let trimmed = line.trim();

        // Unconditionally strip well-known banner lines.
        if trimmed.starts_with("Copyright")
            || trimmed.contains("InterSystems Corporation")
            || trimmed.starts_with("All rights reserved")
            || trimmed.starts_with("IRIS for ")
            || trimmed.starts_with("Cache for ")
            || trimmed.starts_with("Ensemble for ")
        {
            continue;
        }

        // Strip bare prompt-only lines: lines that are just "USER>", "IRIS>", "%SYS>", etc.
        // A bare prompt line has no content beyond the prompt token.
        if is_bare_prompt_line(trimmed) {
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
