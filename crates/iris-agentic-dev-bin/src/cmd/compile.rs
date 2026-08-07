use anyhow::{Context, Result};
use clap::Args;
use iris_agentic_dev_core::iris::connection::CompileResult;
use iris_agentic_dev_core::iris::{
    connection::{DiscoverySource, IrisConnection},
    discovery::{discover_iris, IrisDiscovery},
};

#[derive(Args)]
pub struct CompileCommand {
    pub target: Option<String>,
    #[arg(long, env = "IRIS_HOST")]
    pub host: Option<String>,
    #[arg(long, env = "IRIS_WEB_PORT", default_value = "52773")]
    pub web_port: u16,
    /// URL path prefix for webgateway/IIS-fronted instances (e.g. irishealth)
    #[arg(long, env = "IRIS_WEB_PREFIX", default_value = "")]
    pub web_prefix: String,
    /// URL scheme: http or https
    #[arg(long, env = "IRIS_SCHEME", default_value = "http")]
    pub scheme: String,
    #[arg(long, env = "IRIS_NAMESPACE", default_value = "USER")]
    pub namespace: String,
    #[arg(long, env = "IRIS_USERNAME")]
    pub username: Option<String>,
    #[arg(long, env = "IRIS_PASSWORD")]
    pub password: Option<String>,
    #[arg(long, default_value = "cuk")]
    pub flags: String,
    #[arg(long)]
    pub force_writable: bool,
    #[arg(long, default_value = "text")]
    pub format: String,
}

impl CompileCommand {
    pub async fn run(self) -> Result<()> {
        let explicit = self.host.as_ref().map(|host| {
            // Honor prefix + scheme — behind a webgateway (IRIS_WEB_PREFIX) the bare
            // http://host:port form can't reach Atelier at all (issue #21, upstream #85).
            let scheme = self.scheme.trim_matches('/');
            let prefix = self.web_prefix.trim_matches('/');
            let base_url = if prefix.is_empty() {
                format!("{}://{}:{}", scheme, host, self.web_port)
            } else {
                format!("{}://{}:{}/{}", scheme, host, self.web_port, prefix)
            };
            let username = self.username.as_deref().unwrap_or("_SYSTEM");
            let password = self.password.as_deref().unwrap_or("SYS");
            IrisConnection::new(
                base_url,
                &self.namespace,
                username,
                password,
                DiscoverySource::ExplicitFlag,
            )
        });

        // Load .iris-agentic-dev.toml — takes precedence over env vars but not CLI flags (FR-006, FR-007).
        let ws_path = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        let explicit = iris_agentic_dev_core::iris::workspace_config::apply_workspace_config(
            explicit,
            ws_path.as_deref(),
            &self.namespace,
        );

        let iris = match discover_iris(explicit).await {
            IrisDiscovery::Found(c) => c,
            IrisDiscovery::NotFound => {
                anyhow::bail!(
                    "No IRIS connection found — set IRIS_HOST or run iris-agentic-dev mcp for auto-discovery"
                );
            }
            IrisDiscovery::Explained => {
                // Specific actionable message already emitted to stderr — exit cleanly.
                std::process::exit(1);
            }
        };

        let client = IrisConnection::http_client()?;
        let target = self.target.as_deref().unwrap_or(".");

        // ── .cls file: upload via Atelier PUT then compile via /action/compile ──
        if target.ends_with(".cls") {
            let cls_text =
                std::fs::read_to_string(target).with_context(|| format!("reading {}", target))?;
            let cls_name = cls_text
                .lines()
                .find(|l| l.trim_start().starts_with("Class "))
                .and_then(|l| l.split_whitespace().nth(1))
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    target
                        .trim_end_matches(".cls")
                        .replace(['/', '\\'], ".")
                        .trim_start_matches('.')
                        .to_string()
                });
            let doc_name = format!("{}.cls", cls_name);

            // Upload
            let put_url = iris.versioned_ns_url(
                &self.namespace,
                &format!("/doc/{}?ignoreConflict=1", urlencoding::encode(&doc_name)),
            );
            let lines: Vec<&str> = cls_text.lines().collect();
            let put_resp = client
                .put(&put_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .json(&serde_json::json!({"enc": false, "content": lines}))
                .send()
                .await
                .context("PUT /doc failed")?;
            if !put_resp.status().is_success() {
                anyhow::bail!("Upload failed: HTTP {}", put_resp.status());
            }
            let put_body: serde_json::Value = put_resp.json().await.unwrap_or_default();
            if let Some(errs) = put_body["status"]["errors"].as_array() {
                if !errs.is_empty() {
                    let msg = errs[0]["error"].as_str().unwrap_or("Upload failed");
                    let result = serde_json::json!({"success": false, "error_code": "UPLOAD_FAILED", "error": msg, "target": target});
                    output_result(&result, &self.format);
                    std::process::exit(1);
                }
            }

            // Compile via /action/compile (structured errors, line numbers)
            let compile_result = iris
                .compile_document(&doc_name, &self.namespace, &self.flags, &client)
                .await
                .context("compile request failed")?;
            let result = compile_result_to_json(&compile_result, target, &self.namespace);
            output_result(&result, &self.format);
            if !compile_result.success() {
                std::process::exit(1);
            }
            return Ok(());
        }

        // ── non-.cls target: compile by name via /action/compile ──
        let doc_name = if target == "." {
            // CompileAll — use a special marker; handled below
            target.to_string()
        } else {
            target.to_string()
        };

        if doc_name == "." {
            // CompileAll via ObjectScript (no Atelier endpoint for this)
            let code = format!(
                "Set sc=$SYSTEM.OBJ.CompileAll(\"{}\") If $System.Status.IsOK(sc) {{Write \"OK\"}} Else {{Write $System.Status.GetErrorText(sc)}}",
                self.flags
            );
            let out = iris
                .execute_via_generator(&code, &self.namespace, &client)
                .await
                .context("CompileAll failed")?;
            let out = out.trim();
            if out.ends_with("OK") || out == "OK" {
                let result = serde_json::json!({"success": true, "target": ".", "namespace": self.namespace});
                output_result(&result, &self.format);
            } else {
                let result = serde_json::json!({"success": false, "error_code": "IRIS_COMPILE_FAILED", "error": out, "target": "."});
                output_result(&result, &self.format);
                std::process::exit(1);
            }
        } else {
            let compile_result = iris
                .compile_document(&doc_name, &self.namespace, &self.flags, &client)
                .await
                .context("compile request failed")?;
            let result = compile_result_to_json(&compile_result, target, &self.namespace);
            output_result(&result, &self.format);
            if !compile_result.success() {
                std::process::exit(1);
            }
        }
        Ok(())
    }
}

fn compile_result_to_json(r: &CompileResult, target: &str, namespace: &str) -> serde_json::Value {
    let errors: Vec<serde_json::Value> = r
        .errors
        .iter()
        .map(|e| serde_json::json!({"severity":"error","text":e}))
        .collect();
    serde_json::json!({
        "success": r.success(),
        "target": target,
        "namespace": namespace,
        "errors": errors,
        "console": r.console,
    })
}

fn output_result(result: &serde_json::Value, format: &str) {
    if format == "json" {
        println!("{}", result);
    } else if result["success"] == true {
        println!("✓ Compiled: {}", result["target"].as_str().unwrap_or(""));
    } else {
        eprintln!(
            "✗ Error [{}]: {}",
            result["error_code"].as_str().unwrap_or(""),
            result["error"].as_str().unwrap_or("")
        );
    }
}
