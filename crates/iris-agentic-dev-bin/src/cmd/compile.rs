use anyhow::{Context, Result};
use clap::Args;
use iris_agentic_dev_core::iris::connection::CompileResult;
use iris_agentic_dev_core::iris::{
    connection::{DiscoverySource, IrisConnection},
    discovery::{discover_iris, IrisDiscovery},
};
use iris_agentic_dev_core::tools::wildcard;

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
        // #312: a config file that exists and cannot be used is fatal here. Continuing would fall
        // back to IRIS_HOST/auto-discovery — a DIFFERENT instance than the one the file names — and
        // `compile` writes, so the failure mode is a class landing in the wrong namespace with
        // nothing printed at the moment the choice was made.
        let explicit = iris_agentic_dev_core::iris::workspace_config::apply_workspace_config(
            explicit,
            ws_path.as_deref(),
            &self.namespace,
        )
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;

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
        //
        // #313: a target carrying `*` is a PATTERN, never a path, and the suffix test used to win.
        // `MyApp.*.cls` is a spelling `iris_compile` documents and accepts, and here it was read as
        // a filename: `reading MyApp.*.cls: No such file or directory`. The scope rule and the cap
        // could not fire on it, because the branch that applies them was never reached — a guard
        // made unreachable by a sibling condition, not by anything wrong with the guard.
        if target.ends_with(".cls") && !target.contains('*') {
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
        } else if doc_name.contains('*') {
            // #313: the same guards `iris_compile` applies, from the same code. Before this the
            // pattern went straight to /action/compile with no scope rule, no cap and no count.
            // What that actually did was only ever inferred; measured against a live instance:
            // Atelier expands it SERVER-SIDE, so the compile happened — and a pattern matching
            // NOTHING came back with `errors: []` and "Compilation finished successfully", i.e.
            // a typo'd package reported as a successful compile, exit code 0.
            let targets =
                match wildcard::expand_compile_wildcard(&iris, &client, &self.namespace, &doc_name)
                    .await
                {
                    Ok(wildcard::ExpandedTargets::Unqualified) => {
                        fail(
                            &self.format,
                            "SCOPE_REQUIRED",
                            &format!(
                            "target '{doc_name}' has nothing before its first '*', so it names no \
                             package and would select on the tail alone — in namespace {} that is \
                             up to every class the namespace holds, compiled in one request. \
                             Qualify it with a package: 'MyApp.*', 'MyApp.Sub.*.cls', or name a \
                             single document. Nothing was compiled.",
                            self.namespace
                        ),
                            target,
                            &self.namespace,
                        );
                    }
                    Ok(wildcard::ExpandedTargets::TooBroad { matched }) => {
                        fail(
                            &self.format,
                            "TOO_BROAD",
                            &format!(
                            "target '{doc_name}' matches {matched} documents in namespace {} — \
                             more than the {} one wildcard compile may queue. Nothing was \
                             compiled. Name a narrower package (add the next level: 'Pkg.Sub.*') \
                             or compile the documents one at a time.",
                            self.namespace,
                            wildcard::WILDCARD_EXPANSION_CAP
                        ),
                            target,
                            &self.namespace,
                        );
                    }
                    Ok(wildcard::ExpandedTargets::Expanded {
                        targets, scanned, ..
                    }) => {
                        if targets.is_empty() {
                            // The one the old code could not report at all: Atelier answers a
                            // no-match wildcard with success, so the CLI printed success and
                            // exited 0 for a typo.
                            fail(
                                &self.format,
                                "NOT_FOUND",
                                &format!(
                                "no CLS document matches '{doc_name}' in namespace {} ({scanned} \
                                 name(s) scanned). Nothing was compiled. The listing covers \
                                 CLASSES only, so a .mac/.int/.inc routine is never matched by a \
                                 wildcard — compile one by its exact name. Hidden and generated \
                                 classes are also absent from it, and are not compiled by a \
                                 wildcard even when the pattern is passed straight to IRIS.",
                                self.namespace
                            ),
                                target,
                                &self.namespace,
                            );
                        }
                        targets
                    }
                    Err(unavailable) => {
                        fail(
                            &self.format,
                            "LISTING_UNAVAILABLE",
                            &format!(
                            "could not read the class listing for namespace {}, so the wildcard \
                             '{doc_name}' could not be expanded: {}. Nothing was compiled — the \
                             cap and the scope rule cannot be applied without it. Compile a \
                             single document by its exact name, which needs no listing. Listing \
                             URL: {}",
                            self.namespace, unavailable.detail, unavailable.url
                        ),
                            target,
                            &self.namespace,
                        );
                    }
                };
            let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
            let compile_result = iris
                .compile_documents(&refs, &self.namespace, &self.flags, &client)
                .await
                .context("compile request failed")?;
            let mut result = compile_result_to_json(&compile_result, target, &self.namespace);
            // The count the pass-through could never report. A wildcard that compiled 1 document
            // when you expected 40 is the case this exists to make visible.
            result["expanded"] = serde_json::json!(targets.len());
            result["targets"] = serde_json::json!(targets);
            output_result(&result, &self.format);
            if !compile_result.success() {
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

/// #313: render a refusal the way the tool's envelope does — an `error_code` a script can branch
/// on, and a message that says what was NOT done. Exits 1; nothing has been compiled at any call
/// site that reaches here.
fn fail(format: &str, code: &str, message: &str, target: &str, namespace: &str) -> ! {
    let result = serde_json::json!({
        "success": false,
        "error_code": code,
        "error": message,
        "target": target,
        "namespace": namespace,
    });
    output_result(&result, format);
    std::process::exit(1);
}

fn compile_result_to_json(r: &CompileResult, target: &str, namespace: &str) -> serde_json::Value {
    let errors: Vec<serde_json::Value> = r
        .errors
        .iter()
        .map(|e| serde_json::json!({"severity":"error","text":e}))
        .collect();
    let mut out = serde_json::json!({
        "success": r.success(),
        "target": target,
        "namespace": namespace,
        "errors": errors,
        "console": r.console,
    });
    iris_agentic_dev_core::tools::note_error_undercount(
        &mut out,
        r.detected_error_count(),
        r.errors.len(),
        "console",
    );
    out
}

fn output_result(result: &serde_json::Value, format: &str) {
    if format == "json" {
        println!("{}", result);
    } else if result["success"] == true {
        // #313: the count belongs in TEXT mode too — it is the default format, and a wildcard that
        // compiled 1 document when you expected 40 is the case the expansion exists to make
        // visible. Absent for a literal target, which has nothing to count.
        match result["expanded"].as_u64() {
            Some(n) => println!(
                "✓ Compiled: {} ({} document{})",
                result["target"].as_str().unwrap_or(""),
                n,
                if n == 1 { "" } else { "s" }
            ),
            None => println!("✓ Compiled: {}", result["target"].as_str().unwrap_or("")),
        }
    } else {
        eprintln!(
            "✗ Error [{}]: {}",
            result["error_code"].as_str().unwrap_or(""),
            result["error"].as_str().unwrap_or("")
        );
    }
}
