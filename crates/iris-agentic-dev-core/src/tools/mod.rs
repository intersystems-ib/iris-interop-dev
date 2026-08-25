use crate::elicitation::ElicitationStore;
use crate::iris::connection::IrisConnection;

/// Remediation hint appended to DOCKER_REQUIRED error strings.
/// Guides native IRIS users (no Docker) toward the HTTP/Atelier REST path.
const DOCKER_REQUIRED_HINT: &str = " Ensure HTTP/Atelier REST is reachable: verify \
    http://<host>:<port>/api/atelier and set host/web_port in .iris-agentic-dev.toml.";

use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters, model::*,
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Wrapper for tools that accept free-form JSON parameters.
/// Uses a manual JsonSchema impl to emit `{"type":"object"}` instead of
/// schemars' default `{"title":"AnyValue"}`, which Claude Code rejects.
#[derive(Debug, Deserialize)]
pub struct AnyParams(pub serde_json::Value);

impl JsonSchema for AnyParams {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AnyParams".into()
    }
    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "object"})
    }
}

impl std::ops::Deref for AnyParams {
    type Target = serde_json::Value;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
pub mod admin;
pub mod concurrency;
pub mod dict;
pub mod doc;
pub mod envelope;
pub mod info;
pub mod interop;
pub mod log_store;
pub mod scm;
pub mod search;
pub mod skills_tools;
pub mod sql_lint;
pub mod symbols_local;

pub use doc::{DocMode, IrisDocParams};
pub use scm::ScmParams;

/// Controls which tools are registered at startup.
/// Read from `IRIS_TOOLSET` env var or `--toolset` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolset {
    /// All 34 tools — current behavior (default when IRIS_TOOLSET unset).
    Baseline,
    /// 29 tools — stub tools/actions removed; no merged tools.
    Nostub,
    /// 23 tools — stubs removed + 4 merger groups consolidated.
    Merged,
    /// ~20 tools — interop-skills-focused profile (see `INTEROP_TOOLS`).
    /// Keeps only the tools the iris-interop skills actually exercise; everything
    /// else (skill_*/kb_*/agent_*/generate_*/individual debug_*/container/scm) is pruned.
    /// Default for this fork. Additive: tool *code* is unchanged so upstream stays mergeable.
    Interop,
}

impl Toolset {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "nostub" => Toolset::Nostub,
            "merged" => Toolset::Merged,
            "interop" => Toolset::Interop,
            _ => Toolset::Baseline,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Toolset::Baseline => "baseline",
            Toolset::Nostub => "nostub",
            Toolset::Merged => "merged",
            Toolset::Interop => "interop",
        }
    }
}

/// The interop-focused keep-list (Toolset::Interop). Source of truth for the
/// `Interop` pruning AND the `registered_tool_names()` Interop branch. A unit test
/// (`test_interop_toolset_exact`) asserts the live router exposes exactly these,
/// which also guards against typos / upstream renames.
pub const INTEROP_TOOLS: &[&str] = &[
    // core execution
    "iris_query",
    "iris_doc",
    "iris_execute",
    "iris_compile",
    "iris_test",
    // diagnostics / introspection
    "iris_symbols",
    "docs_introspect",
    "check_config",
    "iris_get_log",
    "iris_debug",
    // interoperability
    "iris_production",
    "iris_production_item",
    "iris_interop_query",
    "iris_lookup_manage",
    "iris_lookup_transfer",
    "iris_credential_list",
    "iris_credential_manage",
    "extract_message_map_routing",
    "find_subclass_implementations",
    "iris_table_info",
    // 056 interop-depth
    "iris_message_body",
    "iris_business_rule_info",
    "iris_production_diff",
];

pub const ERR_NO_TESTS_FOUND: &str = "NO_TESTS_FOUND";

/// Issue #47: what a caller must be told when a pattern matched nothing.
///
/// `NO_TESTS_FOUND` is a correct answer, but a bare one cannot distinguish its two
/// causes — the class was never compiled, or the pattern is a near miss. Workshop
/// telemetry: 44 hits across 16/18 students, on patterns one segment away from the
/// real class (`Ejercicio3.Test` for `Ejercicio3.Tests.BO.SQLTest`).
///
/// Returns `(hint, did_you_mean)`. `did_you_mean` holds the candidates that are a
/// prefix of the pattern or have it as a prefix — the near-miss family — so the
/// caller's next call is a correction, not another guess.
pub fn no_tests_found_guidance(
    pattern: &str,
    namespace: &str,
    candidates: &[String],
    more_than_listed: bool,
) -> (String, Vec<String>) {
    let pat = pattern.to_lowercase();
    let did_you_mean: Vec<String> = candidates
        .iter()
        // #62: a suggestion identical to the input is not a correction — following it
        // re-sends the same call. When the pattern IS a compiled class, the class is
        // what needs fixing, and `no_runnable_tests_cause` answers that instead.
        .filter(|c| !c.eq_ignore_ascii_case(pattern))
        .filter(|c| {
            let c = c.to_lowercase();
            c.starts_with(&pat) || pat.starts_with(&c)
        })
        .cloned()
        .collect();

    let count = if more_than_listed {
        format!("{}+", candidates.len())
    } else {
        candidates.len().to_string()
    };

    let hint = if candidates.is_empty() {
        format!(
            "No compiled test classes (%UnitTest.TestCase or %UnitTest.TestProduction subclasses) \
             exist in namespace '{namespace}' — so nothing could have matched. Either the class \
             is not compiled yet (compile it with iris_compile, or pass a directory path to load \
             .cls from disk), or your tests live in a different namespace than '{namespace}' — \
             pass `namespace` explicitly to check another one."
        )
    } else if !did_you_mean.is_empty() {
        format!(
            "Pattern '{pattern}' matched no runnable tests, but it is one segment away from a \
             compiled test class in '{namespace}'. Did you mean: {}? Pass the exact class name \
             (class names use /noload automatically). {count} test class(es) exist in this namespace.",
            did_you_mean.join(", ")
        )
    } else {
        format!(
            "Pattern '{pattern}' matched no runnable tests. {count} compiled test class(es) exist \
             in '{namespace}': {}. Pass one of these exact class names (class names use /noload \
             automatically), or a directory path to load from disk.",
            candidates.join(", ")
        )
    };
    (hint, did_you_mean)
}

/// Issue #62: `NO_TESTS_FOUND` covered two states that need OPPOSITE actions — fix
/// the pattern, or fix the class — and always reported the first. A class that
/// matched but exposes no runnable test got told its pattern was "one segment away"
/// from a byte-identical name, with itself as the only `did_you_mean`.
pub const ERR_NO_RUNNABLE_TESTS: &str = "NO_RUNNABLE_TESTS";

/// What the class dictionary says about the class a pattern names. These four facts
/// decide whether %UnitTest can run anything — all read from IRIS, none inferred.
#[derive(Debug, Clone)]
pub struct TestClassShape {
    pub class: String,
    /// `%Dictionary.CompiledClass.PrimarySuper` — the whole inheritance chain.
    pub primary_super: String,
    /// Effective `Parameter PRODUCTION` — empty when unset, absent, or inherited as "".
    pub production: String,
    /// `Test*` instance methods this class DECLARES. Inherited ones are excluded:
    /// %UnitTest.TestProduction contributes its own, which say nothing about whether
    /// the class under test has tests.
    pub own_test_methods: u64,
}

impl TestClassShape {
    pub fn extends_test_case(&self) -> bool {
        self.primary_super.contains("%UnitTest.TestCase")
    }
    pub fn extends_test_production(&self) -> bool {
        self.primary_super.contains("%UnitTest.TestProduction")
    }
}

/// Issue #62: given a class that matched, name what about it stopped the run.
/// Returns `(cause, hint)`. The cause is machine-readable; the hint is the fix.
pub fn no_runnable_tests_cause(shape: &TestClassShape, namespace: &str) -> (&'static str, String) {
    let class = &shape.class;
    if !shape.extends_test_case() {
        return (
            "NOT_A_TEST_CLASS",
            format!(
                "'{class}' is compiled in '{namespace}' but extends neither %UnitTest.TestCase \
                 nor %UnitTest.TestProduction, so %UnitTest has nothing to run. A test class \
                 extends %UnitTest.TestCase — or %UnitTest.TestProduction with \
                 `Parameter PRODUCTION` when the tests need a running production. Add the \
                 superclass and recompile."
            ),
        );
    }
    if shape.own_test_methods == 0 {
        return (
            "NO_TEST_METHODS",
            format!(
                "'{class}' is a compiled test class but declares no Test* instance methods of \
                 its own — %UnitTest only runs instance methods whose name starts with `Test` \
                 (a ClassMethod is skipped). Add e.g. `Method TestX()` and recompile."
            ),
        );
    }
    if shape.extends_test_production() && shape.production.is_empty() {
        return (
            "PRODUCTION_PARAMETER_EMPTY",
            format!(
                "'{class}' extends %UnitTest.TestProduction but its PRODUCTION parameter is \
                 empty, so there is no production to start and the suite runs nothing. Set \
                 `Parameter PRODUCTION = \"<Package.ProductionName>\";` and recompile — IRIS \
                 itself refuses such a class (ERROR #5001: Parameter PRODUCTION must be \
                 specified), which can leave a stale compiled class behind that still matches."
            ),
        );
    }
    // #66 retired the NOT_A_TESTPRODUCTION_SUBCLASS cause: a plain %UnitTest.TestCase
    // class is now named after its suite in the RunTest spec and runs like any other, so
    // its superclass is no longer a reason for an empty run.
    (
        "UNKNOWN",
        format!(
            "'{class}' is compiled in '{namespace}' and looks runnable ({} Test* method(s){}), \
             yet the run produced no method results. The pattern is not the problem — check \
             OnBeforeAllTests/%OnNew for an early Quit, confirm the Test* methods are instance \
             methods, and read the run output with iris_get_log.",
            shape.own_test_methods,
            if shape.extends_test_production() {
                format!(", PRODUCTION = '{}'", shape.production)
            } else {
                String::new()
            }
        ),
    )
}

/// Issue #62: read the class dictionary for the exact class a pattern names.
///
/// `None` means either nothing of that name is compiled — then the PATTERN is the
/// problem and `no_tests_found_guidance` answers — or the catalog could not be read.
/// An unreadable catalog must not become a confident claim about the class.
pub async fn probe_test_class_shape(
    iris: &IrisConnection,
    namespace: &str,
    client: &reqwest::Client,
    class: &str,
) -> Option<TestClassShape> {
    // The class name is caller text: bound parameter, never interpolated.
    // `_Default` is the SQL field name of %Dictionary.CompiledParameter.Default
    // ("Default" is a reserved word). `Origin = c.Name` keeps inherited members out.
    let sql = "SELECT TOP 1 c.Name AS ClassName, c.PrimarySuper AS Supers, \
               (SELECT COUNT(*) FROM %Dictionary.CompiledMethod m WHERE m.parent = c.Name \
                AND m.Name %STARTSWITH 'Test' AND m.ClassMethod = 0 AND m.Origin = c.Name) AS OwnTestMethods, \
               (SELECT TOP 1 p._Default FROM %Dictionary.CompiledParameter p WHERE p.parent = c.Name \
                AND p.Name = 'PRODUCTION') AS ProductionParam \
               FROM %Dictionary.CompiledClass c WHERE c.Name = ?";
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        iris.query(
            sql,
            vec![serde_json::Value::String(class.to_string())],
            namespace,
            client,
        ),
    )
    .await
    .ok()?
    .ok()?;
    let row = body["result"]["content"].as_array()?.first()?;
    let name = row["ClassName"].as_str()?;
    let methods = row["OwnTestMethods"]
        .as_i64()
        .or_else(|| {
            row["OwnTestMethods"]
                .as_str()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(0);
    Some(TestClassShape {
        class: name.to_string(),
        primary_super: row["Supers"].as_str().unwrap_or_default().to_string(),
        production: row["ProductionParam"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_string(),
        own_test_methods: methods.max(0) as u64,
    })
}

/// Issue #66: how a class pattern reaches %UnitTest.
///
/// `%UnitTest.Manager.RunTest` takes a SUITE spec — a directory under `^UnitTestRoot`,
/// optionally `:class:method`. A bare class name therefore names a directory: the one this
/// code pre-creates, which `/noload` deliberately leaves empty. RunTest walks it, finds no
/// test class, and reports "All PASSED" having run nothing. A `%UnitTest.TestProduction`
/// subclass escaped that because it runs through its own `Run()`; anything else — including
/// a well-formed `%UnitTest.TestCase` with correctly-named `Test*` instance methods — ran
/// nothing. That was issue #62's dominant cause (12.5% of BUILD runs).
///
/// `DebugRunTestCase(suite, class, qspec, method, userparam)` is the supported entry point
/// for an ALREADY-COMPILED class: it runs the class directly, needs no suite directory, and
/// still writes `^UnitTest.Result` — which is this tool's primary result source, so per-method
/// results and failures come back the same way they do for a production suite.
///
/// A pattern that is not a compiled class — a package prefix like `MyApp.Tests`, which the
/// tool description advertises — keeps the RunTest spec: there is no class to run, and
/// `no_tests_found_guidance` answers with the near misses.
///
/// `^UnitTestRoot` is platform-aware (a temp dir under mgr on Windows, `/tmp/httest/`
/// elsewhere) and the spec directory is created unconditionally — `CreateDirectoryChain`
/// is idempotent, and keeping it branch-free lets the code pipe through the docker-exec
/// terminal as well as the HTTP path. A stale or invalid root makes RunTest fail with
/// "Directory ... is invalid" and report 0 tests. (A6.2)
pub fn build_class_test_run_code(pattern: &str, flags: &str, token: &str) -> String {
    format!(
        r#"set tIsWin=($zcvt($system.Version.GetOS(),"U")="WINDOWS")
set ^UnitTestRoot=$select(tIsWin:##class(%File).NormalizeDirectory("httest",##class(%File).GetDirectory(##class(%File).TempFilename())),1:"/tmp/httest/")
do ##class(%File).CreateDirectoryChain(^UnitTestRoot)
set specDir=##class(%File).NormalizeDirectory($translate("{pattern}",".","/"),^UnitTestRoot)
do ##class(%File).CreateDirectoryChain(specDir)
set tCls="{pattern}"
set tCC=##class(%Dictionary.CompiledClass).%OpenId(tCls)
if $isobject(tCC)&&(tCC.PrimarySuper["%UnitTest.TestProduction") {{ do $classmethod(tCls,"Run") }} elseif $isobject(tCC)&&(tCC.PrimarySuper["%UnitTest.TestCase") {{ do ##class(%UnitTest.Manager).DebugRunTestCase("",tCls,"{flags}","","{token}") }} else {{ do ##class(%UnitTest.Manager).RunTest("{pattern}","{flags}","{token}") }}"#,
        token = token,
        pattern = pattern,
        flags = flags,
    )
}

pub const ERR_NAMESPACE_NOT_FOUND: &str = "NAMESPACE_NOT_FOUND";
pub const ERR_TEST_EXECUTION_ERROR: &str = "TEST_EXECUTION_ERROR";

// ── Live connection hot-reload types (034) ───────────────────────────────────

/// How the currently active IRIS connection was established.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSource {
    ConfigFile,
    EnvVars,
    IrisSelectContainer,
    AutoDiscovered,
}

/// Snapshot of the active IRIS connection, including metadata for `check_config`.
pub struct ConnectionState {
    pub iris: Option<Arc<IrisConnection>>,
    pub source: ConnectionSource,
    pub config_file: Option<std::path::PathBuf>,
    pub loaded_at: std::time::SystemTime,
    pub write_tools_enabled: bool,
    pub config_parse_error: Option<String>,
}

impl ConnectionState {
    pub fn new_disconnected(source: ConnectionSource) -> Self {
        Self {
            iris: None,
            source,
            config_file: None,
            loaded_at: std::time::SystemTime::now(),
            write_tools_enabled: true,
            config_parse_error: None,
        }
    }

    pub fn from_iris(
        iris: IrisConnection,
        source: ConnectionSource,
        config_file: Option<std::path::PathBuf>,
    ) -> Self {
        let write_tools_enabled = iris.is_write_allowed();
        Self {
            iris: Some(Arc::new(iris)),
            source,
            config_file,
            loaded_at: std::time::SystemTime::now(),
            write_tools_enabled,
            config_parse_error: None,
        }
    }
}

/// Tracks the `.iris-agentic-dev.toml` path and last-seen mtime for lazy hot-reload.
/// Always created (even when the file does not yet exist) so we detect new files appearing.
pub struct ConfigWatcher {
    pub config_path: std::path::PathBuf,
    /// None when the file did not exist at last check.
    pub last_mtime: Option<std::time::SystemTime>,
}

impl ConfigWatcher {
    /// Always returns Some — watcher is active even before the file exists.
    pub fn new(config_path: std::path::PathBuf) -> Option<Self> {
        let last_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        Some(Self {
            config_path,
            last_mtime,
        })
    }

    /// Returns true (and updates stored mtime) if the file has been created, modified,
    /// or has appeared for the first time since last check.
    pub fn has_changed(&mut self) -> bool {
        let current_mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        match (self.last_mtime, current_mtime) {
            // File newly appeared
            (None, Some(mtime)) => {
                self.last_mtime = Some(mtime);
                true
            }
            // File modified
            (Some(old), Some(new)) if new > old => {
                self.last_mtime = Some(new);
                true
            }
            // File deleted — reset so we detect re-creation
            (Some(_), None) => {
                self.last_mtime = None;
                false
            }
            _ => false,
        }
    }
}

// ── &sql macro translation (035) ─────────────────────────────────────────────

/// Result of translating `&sql(...)` macros to `%SQL.Statement` calls.
pub struct TranslationResult {
    /// The code after translation (equals input if `found` is false).
    pub translated_code: String,
    /// Whether any `&sql(...)` macros were found and processed.
    pub found: bool,
    /// Warnings for constructs that could not be safely translated (left unchanged).
    pub warnings: Vec<String>,
}

/// Translate `&sql(...)` embedded SQL macros in ObjectScript code to
/// runtime-compatible `%SQL.Statement` class method calls.
///
/// This is a pure text transformation — no IRIS network call is made.
/// SELECT INTO uses prepare/execute/get; DML uses %ExecDirect.
/// SQLCODE and %msg on the line immediately following the macro are rewritten
/// to read from the generated result set object; all other references are untouched.
pub fn translate_sql_macros(code: &str) -> TranslationResult {
    if !code.contains("&sql(") {
        return TranslationResult {
            translated_code: code.to_string(),
            found: false,
            warnings: vec![],
        };
    }

    let mut output = String::with_capacity(code.len() * 2);
    let mut warnings = vec![];
    let mut rs_counter: u32 = 0;
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut found = false;

    while i < n {
        // Look for &sql(
        if i + 5 < n
            && chars[i] == '&'
            && chars[i + 1] == 's'
            && chars[i + 2] == 'q'
            && chars[i + 3] == 'l'
            && chars[i + 4] == '('
        {
            found = true;
            rs_counter += 1;
            let rs_var = format!("sqlrs{}", rs_counter);
            let sc_var = format!("sqlsc{}", rs_counter);
            let sqlcode_var = format!("sqlSQLCODE{}", rs_counter);

            // Find matching closing paren using depth counting
            let start = i + 5; // after &sql(
            let mut depth = 1usize;
            let mut j = start;
            while j < n && depth > 0 {
                if chars[j] == '(' {
                    depth += 1;
                } else if chars[j] == ')' {
                    depth -= 1;
                }
                if depth > 0 {
                    j += 1;
                }
            }
            let sql_content: String = chars[start..j].iter().collect();
            i = j + 1; // skip past the closing )

            // Classify statement type
            let sql_upper = sql_content.trim().to_uppercase();
            if sql_upper.starts_with("CALL") {
                // Unsupported — leave unchanged with warning
                warnings.push(format!(
                    "&sql(CALL ...) at macro #{} was not translated — CALL statements with OUT parameters are not supported. Use ##class(...).Method() directly.",
                    rs_counter
                ));
                output.push_str(&format!("&sql({})", sql_content));
            } else if sql_upper.starts_with("SELECT") {
                // Translate SELECT INTO
                output.push_str(&translate_select_into(
                    &sql_content,
                    &rs_var,
                    &sc_var,
                    &sqlcode_var,
                ));
                // Check next line for SQLCODE / %msg and rewrite
                i = rewrite_next_line_sqlcode(
                    chars.as_slice(),
                    i,
                    n,
                    &mut output,
                    &sqlcode_var,
                    &rs_var,
                );
                continue;
            } else if sql_upper.starts_with("INSERT")
                || sql_upper.starts_with("UPDATE")
                || sql_upper.starts_with("DELETE")
                || sql_upper.starts_with("MERGE")
            {
                // Translate DML
                output.push_str(&translate_dml(&sql_content, &rs_var));
                // Check next line for SQLCODE / %msg
                i = rewrite_next_line_sqlcode(
                    chars.as_slice(),
                    i,
                    n,
                    &mut output,
                    &sqlcode_var,
                    &rs_var,
                );
                continue;
            } else {
                // Unknown — leave unchanged with warning
                warnings.push(format!(
                    "&sql({}) at macro #{} was not translated — unrecognized SQL statement type.",
                    &sql_content[..sql_content.len().min(50)],
                    rs_counter
                ));
                output.push_str(&format!("&sql({})", sql_content));
            }
        } else {
            output.push(chars[i]);
            i += 1;
        }
    }

    TranslationResult {
        translated_code: output,
        found,
        warnings,
    }
}

/// Translate a SELECT ... INTO :var1, :var2 ... statement.
fn translate_select_into(sql: &str, rs_var: &str, sc_var: &str, sqlcode_var: &str) -> String {
    // Parse: split on INTO to separate column list and host variables + WHERE clause

    // Find INTO keyword (not inside parens)
    let into_pos = find_keyword_pos(sql, "INTO");

    let (select_cols_sql, rest_after_into) = if let Some(pos) = into_pos {
        let before = sql[..pos].trim().to_string();
        let after = &sql[pos + 4..]; // skip "INTO"
        (before, after.trim().to_string())
    } else {
        // SELECT without INTO — translate as result-set loop but no vars to set
        return translate_select_no_into(sql, rs_var, sc_var, sqlcode_var);
    };

    // Extract SELECT column names (between SELECT and INTO)
    // select_cols_sql is like "SELECT Name, Age"
    let col_list_str = if let Some(idx) = select_cols_sql.to_uppercase().find("SELECT") {
        select_cols_sql[idx + 6..].trim().to_string()
    } else {
        select_cols_sql.clone()
    };
    let col_names: Vec<String> = split_csv(&col_list_str)
        .iter()
        .map(|c| {
            // Handle "ColName AS alias" → use alias
            let upper = c.to_uppercase();
            if let Some(as_pos) = upper.find(" AS ") {
                c[as_pos + 4..].trim().to_string()
            } else {
                // Strip table qualifier: "t.Name" → "Name"
                c.trim()
                    .split('.')
                    .next_back()
                    .unwrap_or(c.trim())
                    .to_string()
            }
        })
        .collect();

    // rest_after_into is like ":name, :age FROM table WHERE ..."
    // Split host vars from FROM clause
    let (host_vars_str, from_clause) = split_host_vars_from_rest(&rest_after_into);
    let host_vars: Vec<String> = split_csv(&host_vars_str)
        .iter()
        .map(|v| v.trim().trim_start_matches(':').to_string())
        .collect();

    // Extract WHERE parameters (collect :varname in FROM+WHERE but not the host vars)
    let where_params = extract_where_params(&from_clause);

    // Build the SQL for %Prepare — SELECT cols FROM ... (without INTO clause)
    let prepared_sql = format!("SELECT {} {}", col_list_str, from_clause);
    // Replace :varname in WHERE with ?
    let prepared_sql = replace_host_vars_with_positional(&prepared_sql, &where_params);

    // Build the generated ObjectScript
    let mut out = String::new();
    out.push_str(&format!(
        "set {} = ##class(%SQL.Statement).%New()\n",
        rs_var
    ));
    out.push_str(&format!(
        "set {} = {}.%Prepare(\"{}\")\n",
        sc_var,
        rs_var,
        prepared_sql.replace('"', "\"\"")
    ));
    // Execute with WHERE params
    let exec_args = if where_params.is_empty() {
        String::new()
    } else {
        format!(", {}", where_params.join(", "))
    };
    out.push_str(&format!(
        "set {} = {}.%Execute({}{})\n",
        rs_var,
        rs_var,
        "",
        exec_args.trim_start_matches(", ")
    ));
    // Fetch row — use single-line if/else for compatibility with execute_via_generator
    out.push_str(&format!("if {}.%Next() {{", rs_var));
    for (idx, var) in host_vars.iter().enumerate() {
        let col = col_names
            .get(idx)
            .map(String::as_str)
            .unwrap_or(var.as_str());
        out.push_str(&format!(" set {} = {}.%Get(\"{}\")", var, rs_var, col));
    }
    out.push_str(" } else {");
    for var in &host_vars {
        out.push_str(&format!(" set {} = \"\"", var));
    }
    out.push_str(&format!(" set {} = {}.%SQLCODE", sqlcode_var, rs_var));
    out.push_str(" }");

    out
}

fn translate_select_no_into(sql: &str, rs_var: &str, sc_var: &str, _sqlcode_var: &str) -> String {
    // SELECT without INTO — translate to prepare/execute but no host var assignment
    let where_params = extract_where_params(sql);
    let prepared_sql = replace_host_vars_with_positional(sql, &where_params);
    let mut out = String::new();
    out.push_str(&format!(
        "set {} = ##class(%SQL.Statement).%New()\n",
        rs_var
    ));
    out.push_str(&format!(
        "set {} = {}.%Prepare(\"{}\")\n",
        sc_var,
        rs_var,
        prepared_sql.replace('"', "\"\"")
    ));
    let exec_args = where_params.join(", ");
    out.push_str(&format!(
        "set {} = {}.%Execute({})\n",
        rs_var, rs_var, exec_args
    ));
    out
}

fn translate_dml(sql: &str, rs_var: &str) -> String {
    let params = extract_where_params(sql);
    let prepared_sql = replace_host_vars_with_positional(sql, &params);
    let exec_args = if params.is_empty() {
        String::new()
    } else {
        format!(", {}", params.join(", "))
    };
    format!(
        "set {} = ##class(%SQL.Statement).%ExecDirect(, \"{}\"{})",
        rs_var,
        prepared_sql.replace('"', "\"\""),
        exec_args
    )
}

/// After a translated &sql, check if the immediately following line contains
/// a standalone SQLCODE or %msg reference and rewrite it.
/// Returns the new position in chars after consuming any rewritten line.
fn rewrite_next_line_sqlcode(
    chars: &[char],
    mut i: usize,
    n: usize,
    output: &mut String,
    sqlcode_var: &str,
    rs_var: &str,
) -> usize {
    // Skip whitespace (but not newlines) to find the next line
    // First, collect the rest of the current line (should be empty or whitespace after &sql)
    while i < n && chars[i] != '\n' {
        output.push(chars[i]);
        i += 1;
    }
    if i < n && chars[i] == '\n' {
        output.push('\n');
        i += 1;
    }

    // Collect the next line
    let mut next_line = String::new();
    let line_start = i;
    while i < n && chars[i] != '\n' {
        next_line.push(chars[i]);
        i += 1;
    }

    if next_line.trim().is_empty() {
        // Empty line — output and continue
        output.push_str(&next_line);
        return i;
    }

    if next_line.trim().starts_with("&sql(") {
        // Another &sql macro — don't consume this line; let the main loop re-process it
        // Back up i to the start of this line
        return line_start;
    }

    // Rewrite SQLCODE → sqlcode_var and %msg → rs_var.%Message on this specific line
    let rewritten = next_line
        .replace("SQLCODE", sqlcode_var)
        .replace("%msg", &format!("{}.%Message", rs_var));

    output.push_str(&rewritten);
    i
}

/// Find the position of a keyword in SQL (case-insensitive), not inside parens.
fn find_keyword_pos(sql: &str, keyword: &str) -> Option<usize> {
    let upper = sql.to_uppercase();
    let kw_upper = keyword.to_uppercase();
    let mut depth = 0usize;
    let bytes = upper.as_bytes();
    let kw_bytes = kw_upper.as_bytes();
    let mut i = 0;
    while i + kw_bytes.len() <= bytes.len() {
        if bytes[i] == b'(' {
            depth += 1;
        } else if bytes[i] == b')' && depth > 0 {
            depth -= 1;
        } else if depth == 0 && bytes[i..].starts_with(kw_bytes) {
            // Word boundary check
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphabetic();
            let after_ok = i + kw_bytes.len() >= bytes.len()
                || !bytes[i + kw_bytes.len()].is_ascii_alphabetic();
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Split a comma-separated list, respecting parens.
fn split_csv(s: &str) -> Vec<String> {
    let mut result = vec![];
    let mut current = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        result.push(trimmed);
    }
    result
}

/// Split host variables (:var1, :var2) from the rest of the SQL after INTO.
/// Returns (host_vars_str, from_and_where_clause).
fn split_host_vars_from_rest(after_into: &str) -> (String, String) {
    // after_into looks like ":name, :age FROM table WHERE ..."
    // Find "FROM" keyword
    let upper = after_into.to_uppercase();
    if let Some(from_pos) = find_keyword_pos(after_into, "FROM") {
        let vars = after_into[..from_pos].trim().to_string();
        let rest = after_into[from_pos..].trim().to_string();
        (vars, rest)
    } else if let Some(pos) = upper.find("FROM") {
        (
            after_into[..pos].trim().to_string(),
            after_into[pos..].trim().to_string(),
        )
    } else {
        (after_into.to_string(), String::new())
    }
}

/// Extract :varname host variables from WHERE/VALUES clause in order, returning bare names.
fn extract_where_params(sql: &str) -> Vec<String> {
    let mut params = vec![];
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut string_char = ' ';
    while i < n {
        let c = chars[i];
        if in_string {
            if c == string_char {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            in_string = true;
            string_char = c;
            i += 1;
            continue;
        }
        if c == ':' && i + 1 < n && chars[i + 1].is_alphabetic() {
            i += 1;
            let mut name = String::new();
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                name.push(chars[i]);
                i += 1;
            }
            if !params.contains(&name) {
                params.push(name);
            }
            continue;
        }
        i += 1;
    }
    params
}

/// Replace :varname with ? in SQL string, tracking order.
fn replace_host_vars_with_positional(sql: &str, params: &[String]) -> String {
    let mut result = sql.to_string();
    for param in params {
        result = result.replace(&format!(":{}", param), "?");
    }
    result
}

/// A single tool call entry for the session history ring buffer.
#[derive(Debug, Clone)]
pub struct ToolCallEntry {
    pub tool: String,
    pub success: bool,
    pub timestamp: std::time::Instant,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompileParams {
    pub target: String,
    #[serde(default = "default_flags")]
    pub flags: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub force_writable: bool,
    /// If true, bypass the log store and return all errors/warnings inline regardless of count.
    #[serde(default)]
    pub inline: bool,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TestParams {
    pub pattern: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_test_timeout")]
    pub timeout: u64,
}

fn default_test_timeout() -> u64 {
    // %UnitTest.TestProduction suites (the TDD workshop pattern) routinely exceed 60s.
    // OBJECTSCRIPT_TEST_TIMEOUT overrides for slower instances (issue #22, upstream #59).
    std::env::var("OBJECTSCRIPT_TEST_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(120)
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolsParams {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IntrospectParams {
    pub class_name: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DebugMapParams {
    #[serde(default)]
    pub routine: String,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub error_string: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateClassParams {
    pub description: String,
    #[serde(default)]
    pub overwrite: bool,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateTestParams {
    pub class_name: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillNameParams {
    pub name: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillSearchParams {
    pub query: String,
    #[serde(default = "default_limit")]
    pub top_k: usize,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KbIndexParams {
    pub workspace_path: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KbRecallParams {
    pub query: String,
    #[serde(default = "default_limit")]
    pub top_k: usize,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentHistoryParams {
    #[serde(default = "default_limit")]
    pub limit: usize,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolsLocalParams {
    pub query: String,
    pub workspace_path: Option<String>,
    #[serde(default = "default_symbols_local_limit")]
    pub limit: usize,
}
fn default_symbols_local_limit() -> usize {
    50
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CapturePacketParams {
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ErrorLogsParams {
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
    /// If true, bypass the log store and return all entries inline regardless of count.
    #[serde(default)]
    pub inline: bool,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommunityPkgParams {
    pub name: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetLogParams {
    /// UUID of a stored log entry. If omitted, lists all stored entries.
    pub id: Option<String>,
    /// Max entries to return from the stored result. Must be > 0 if provided.
    pub limit: Option<usize>,
    /// Start index into the stored result. Default 0.
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceMapParams {
    /// Class name to build source map for (e.g. "Graph.KG.NKGAccel" or "Graph.KG.NKGAccel.cls").
    pub cls_name: String,
    /// Not used — kept for backwards compatibility only. May be removed in a future version.
    #[serde(default)]
    pub cls_text: Option<String>,
    pub workspace_path: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteParams {
    pub code: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_execute_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub confirmed: bool,
    /// If true (default), rewrite &sql(...) embedded SQL macros to %SQL.Statement calls before executing.
    /// Set to false to send code as-is for debugging.
    #[serde(default = "default_translate_sql")]
    pub translate_sql: bool,
}
fn default_translate_sql() -> bool {
    true
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryParams {
    pub query: String,
    /// Query parameters as strings (e.g. ["Alice", "42"])
    #[serde(default)]
    pub parameters: Vec<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// If true, bypass SQL safety validation. Use only for intentional administrative queries.
    /// Has no effect on production IRIS instances (where write tools are disabled).
    #[serde(default)]
    pub force: bool,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListContainersParams {
    pub workspace_root: Option<String>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SelectContainerParams {
    pub name: String,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_password")]
    pub password: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartSandboxParams {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_edition")]
    pub edition: String,
}

fn default_flags() -> String {
    "cuk".to_string()
}
fn default_limit() -> usize {
    20
}
fn default_max_entries() -> usize {
    50
}
fn default_execute_timeout() -> u64 {
    30
}
fn default_username() -> String {
    "_SYSTEM".to_string()
}
fn default_password() -> String {
    "SYS".to_string()
}
fn default_edition() -> String {
    "community".to_string()
}

// ── iris_test SQL result types ────────────────────────────────────────────────

/// One row from %UnitTest.Result.TestSuite.
#[derive(Debug, Clone)]
pub struct SuiteRow {
    pub id: String,
    pub name: String,
    pub status: i64,
    pub duration_ms: Option<f64>,
}

/// One row from %UnitTest.Result.TestMethod.
#[derive(Debug, Clone)]
pub struct MethodRow {
    pub suite_id: String,
    pub name: String,
    pub class_name: String,
    pub status: i64,
    pub duration_ms: Option<f64>,
    pub error_description: String,
    pub error_action: String,
}

/// Maps IRIS %UnitTest status integer to a status string.
/// Status=1 → "passed", Status=0 → "failed", other with ErrorAction → "error", other → "failed".
pub fn map_status_int(status: i64, error_action: &str) -> &'static str {
    match status {
        1 => "passed",
        0 => "failed",
        _ => {
            if !error_action.is_empty() {
                "error"
            } else {
                "failed"
            }
        }
    }
}

/// Build the compact (inline) TestRun JSON from SQL rows.
/// When empty rows are provided, returns a NO_TESTS_FOUND response.
pub fn build_test_run_from_sql(suites: &[SuiteRow], methods: &[MethodRow]) -> serde_json::Value {
    if suites.is_empty() {
        return serde_json::json!({
            "success": false,
            "completed": false,
            "outcome": "no_tests",
            "tests_passed": false,
            "error_code": ERR_NO_TESTS_FOUND,
            "error": "Pattern matched no test classes",
            "total": 0,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "skipped": 0,
        });
    }

    let mut total = 0u64;
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut errors = 0u64;
    let skipped = 0u64;
    let mut duration_ms_total = 0.0f64;

    let mut suite_jsons = Vec::new();
    for suite in suites {
        let suite_methods: Vec<&MethodRow> =
            methods.iter().filter(|m| m.suite_id == suite.id).collect();
        let s_tests = suite_methods.len() as u64;
        let s_failures = suite_methods
            .iter()
            .filter(|m| map_status_int(m.status, &m.error_action) == "failed")
            .count() as u64;
        let s_errors = suite_methods
            .iter()
            .filter(|m| map_status_int(m.status, &m.error_action) == "error")
            .count() as u64;
        let s_dur = suite.duration_ms.unwrap_or(0.0);

        total += s_tests;
        passed += suite_methods
            .iter()
            .filter(|m| map_status_int(m.status, &m.error_action) == "passed")
            .count() as u64;
        failed += s_failures;
        errors += s_errors;
        duration_ms_total += s_dur;

        suite_jsons.push(serde_json::json!({
            "name": suite.name,
            "tests": s_tests,
            "failures": s_failures,
            "errors": s_errors,
            "duration_ms": s_dur,
        }));
    }

    let success = failed == 0 && errors == 0;
    let outcome = if errors > 0 {
        "errors"
    } else if failed > 0 {
        "failed"
    } else {
        "passed"
    };
    serde_json::json!({
        "success": success,
        "completed": true,
        "outcome": outcome,
        "tests_passed": success,
        "total": total,
        "passed": passed,
        "failed": failed,
        "errors": errors,
        "skipped": skipped,
        "duration_ms": duration_ms_total,
        "test_suites": suite_jsons,
    })
}

/// Build the full per-case TestRun JSON for log store storage.
pub fn build_test_detail(suites: &[SuiteRow], methods: &[MethodRow]) -> serde_json::Value {
    let mut suite_jsons = Vec::new();
    for suite in suites {
        let suite_methods: Vec<&MethodRow> =
            methods.iter().filter(|m| m.suite_id == suite.id).collect();
        let cases: Vec<serde_json::Value> = suite_methods
            .iter()
            .map(|m| {
                let status = map_status_int(m.status, &m.error_action);
                let failure_message = if !m.error_description.is_empty() {
                    serde_json::Value::String(m.error_description.clone())
                } else {
                    serde_json::Value::Null
                };
                serde_json::json!({
                    "name": m.name,
                    "class_name": m.class_name,
                    "status": status,
                    "duration_ms": m.duration_ms,
                    "failure_message": failure_message,
                })
            })
            .collect();
        suite_jsons.push(serde_json::json!({
            "name": suite.name,
            "tests": cases.len(),
            "failures": cases.iter().filter(|c| c["status"] == "failed").count(),
            "errors": cases.iter().filter(|c| c["status"] == "error").count(),
            "duration_ms": suite.duration_ms,
            "test_cases": cases,
        }));
    }
    serde_json::json!({"test_suites": suite_jsons})
}

fn iris_unreachable() -> McpError {
    McpError::invalid_request("IRIS_UNREACHABLE: no IRIS connection. Set IRIS_HOST and IRIS_WEB_PORT env vars, or ensure IRIS is reachable on a discoverable port (52773, 41773, 51773, 8080).", None)
}
fn ok_json(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(v.to_string())]))
}
fn err_json(code: &str, msg: &str) -> Result<CallToolResult, McpError> {
    crate::tools::envelope::fail(code, msg)
}
/// Issue #46: a compile that IRIS rejected is a genuine tool failure, so it gets
/// `isError` on the wire and the standard envelope — same contract iris_doc's
/// compile path already follows (issue #2). Diagnostics ride along unchanged in
/// `errors`/`warnings`/`console`; `payload` carries `success:false`, which the
/// envelope owns and re-asserts.
fn compile_failure(target: &str, payload: serde_json::Value) -> Result<CallToolResult, McpError> {
    let first = payload["errors"][0]["text"]
        .as_str()
        .map(str::to_string)
        .or_else(|| payload["errors"][0].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("compile of {target} failed — see console"));
    crate::tools::envelope::fail_with(
        "COMPILE_ERROR",
        &first,
        // The built-in COMPILE_ERROR hint names iris_doc's `compile_console`;
        // this payload calls that field `console`.
        merge_hint(
            payload,
            "Fix the first reported error and recompile — later errors are often cascades \
             of the first. Full compiler output is in console.",
        ),
    )
}
fn merge_hint(mut payload: serde_json::Value, hint: &str) -> serde_json::Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("hint")
            .or_insert_with(|| serde_json::Value::String(hint.to_string()));
    }
    payload
}
pub fn write_open_hint(namespace: &str, document: &str) {
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".iris-agentic-dev");
        let _ = std::fs::create_dir_all(&dir);
        let uri = format!("isfs://{}/{}", namespace, document);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let json = serde_json::json!({"uri": uri, "ts": ts});
        let _ = std::fs::write(dir.join("open-hint.json"), json.to_string());
    }
}

// ── SQL safety gate ───────────────────────────────────────────────────────────

/// Validates that a SQL string is read-only before forwarding to IRIS.
///
/// Processing pipeline:
/// 1. Strip `/* ... */` block comments
/// 2. Strip `-- ...` line comments
/// 3. Return `Err("EMPTY")` if result is whitespace-only
/// 4. Walk remaining chars tracking quote depth; skip `'...'` and `"..."` content
/// 5. Check each unquoted word token against the blocked keyword list (case-insensitive, word-boundary)
/// 6. Check for `SELECT ... INTO <non-paren>` pattern (DDL via SELECT INTO)
///
/// Returns `Ok(())` if safe, `Err(keyword)` with the offending keyword if blocked.
pub fn validate_read_only_sql(sql: &str) -> Result<(), String> {
    const BLOCKED: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "MERGE", "TRUNCATE", "EXEC",
        "EXECUTE", "BULK", "LOAD", "KILL", "LOCK",
    ];

    // Step 1: strip /* ... */ block comments
    let mut cleaned = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2; // skip */
            cleaned.push(' '); // preserve word boundary
        } else {
            cleaned.push(bytes[i] as char);
            i += 1;
        }
    }

    // Step 2: strip -- line comments
    let mut no_line_comments = String::with_capacity(cleaned.len());
    for line in cleaned.lines() {
        if let Some(pos) = line.find("--") {
            no_line_comments.push_str(&line[..pos]);
        } else {
            no_line_comments.push_str(line);
        }
        no_line_comments.push(' ');
    }
    let cleaned = no_line_comments;

    // Step 3: empty check
    if cleaned.trim().is_empty() {
        return Err("EMPTY".to_string());
    }

    // Steps 4+5: walk chars, skip quoted content, check word tokens
    let chars: Vec<char> = cleaned.chars().collect();
    let n = chars.len();
    let upper = cleaned.to_uppercase();
    let upper_chars: Vec<char> = upper.chars().collect();

    let mut idx = 0;
    while idx < n {
        let c = chars[idx];
        // Skip single-quoted string literals
        if c == '\'' {
            idx += 1;
            while idx < n && chars[idx] != '\'' {
                if chars[idx] == '\\' {
                    idx += 1;
                }
                idx += 1;
            }
            idx += 1; // closing quote
            continue;
        }
        // Skip double-quoted identifiers
        if c == '"' {
            idx += 1;
            while idx < n && chars[idx] != '"' {
                idx += 1;
            }
            idx += 1;
            continue;
        }
        // Check for keyword match at this position
        for kw in BLOCKED {
            let kw_len = kw.len();
            if idx + kw_len > n {
                continue;
            }
            // Compare against uppercased chars
            let matches = upper_chars[idx..idx + kw_len]
                .iter()
                .zip(kw.chars())
                .all(|(a, b)| *a == b);
            if !matches {
                continue;
            }
            // Word boundary: character before must be non-alphanumeric/non-underscore (or start)
            let before_ok = idx == 0 || {
                let bc = chars[idx - 1];
                !bc.is_alphanumeric() && bc != '_'
            };
            // Word boundary: character after must be non-alphanumeric/non-underscore (or end)
            let after_ok = idx + kw_len >= n || {
                let ac = chars[idx + kw_len];
                !ac.is_alphanumeric() && ac != '_'
            };
            if before_ok && after_ok {
                return Err(kw.to_string());
            }
        }
        idx += 1;
    }

    // Step 6: check for SELECT ... INTO <identifier> (not INTO subquery)
    // Find "INTO" token not followed by '('
    let upper_str = upper.as_str();
    let mut search_start = 0;
    while let Some(pos) = upper_str[search_start..].find("INTO") {
        let abs_pos = search_start + pos;
        // Word boundary check
        let before_ok = abs_pos == 0 || {
            let bc = upper_chars[abs_pos - 1];
            !bc.is_alphanumeric() && bc != '_'
        };
        let after_ok = abs_pos + 4 >= n || {
            let ac = upper_chars[abs_pos + 4];
            !ac.is_alphanumeric() && ac != '_'
        };
        if before_ok && after_ok {
            // Check what follows INTO (skip whitespace)
            let mut after = abs_pos + 4;
            while after < n && chars[after].is_whitespace() {
                after += 1;
            }
            // If followed by '(' it's INTO a subquery — allowed
            // If followed by anything else (identifier, #, @, etc.) — DDL, block it
            if after < n && chars[after] != '(' {
                return Err("SELECT INTO".to_string());
            }
        }
        search_start = abs_pos + 1;
    }

    Ok(())
}

fn err_json_with_url(
    code: &str,
    msg: &str,
    attempted_url: &str,
) -> Result<CallToolResult, McpError> {
    envelope::fail_with(
        code,
        msg,
        serde_json::json!({
            "attempted_url": attempted_url,
            "hint": "Check IRIS_HOST and IRIS_WEB_PORT (and IRIS_WEB_PREFIX if using a non-root gateway)"
        }),
    )
}
// Bug 20: delegate to the canonical implementation in iris::discovery instead of duplicating.
fn score_container(name: &str, workspace_basename: &str) -> i64 {
    crate::iris::discovery::score_container_name(name, workspace_basename) as i64
}

fn extract_port(ports: &str, container_port: &str) -> Option<u16> {
    let pat = format!("(\\d+)->{}", regex::escape(container_port));
    regex::Regex::new(&pat)
        .ok()?
        .captures(ports)
        .and_then(|c| c[1].parse().ok())
}

async fn list_iris_containers(workspace_basename: &str) -> Vec<serde_json::Value> {
    let mut containers: Vec<serde_json::Value> = Vec::new();

    if let Ok(out) = tokio::process::Command::new("idt")
        .args(["container", "list", "--format", "json"])
        .output()
        .await
    {
        if out.status.success() {
            if let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) {
                for item in items {
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let ports = item["ports"].as_str().unwrap_or("");
                    let sp = extract_port(ports, "1972")
                        .map(|p| serde_json::json!(p))
                        .unwrap_or(serde_json::Value::Null);
                    // idt only reports 1972 — get web port from docker inspect fallback
                    let wp = extract_port(ports, "52773")
                        .or_else(|| {
                            // idt didn't include web port — query docker directly
                            std::process::Command::new("docker")
                                .args(["port", &name, "52773"])
                                .output()
                                .ok()
                                .and_then(|o| {
                                    let raw = String::from_utf8_lossy(&o.stdout).to_string();
                                    // output: "0.0.0.0:52780" or "[::]:52780" (one per line)
                                    raw.lines()
                                        .filter_map(|l| l.rsplit_once(':'))
                                        .filter_map(|(_, p)| p.trim().parse::<u16>().ok())
                                        .next()
                                })
                        })
                        .map(|p| serde_json::json!(p))
                        .unwrap_or(serde_json::Value::Null);
                    let score = score_container(&name, workspace_basename);
                    containers.push(serde_json::json!({
                        "name": name, "port_superserver": sp, "port_web": wp,
                        "image": item["image"], "status": item.get("status").unwrap_or(&serde_json::json!("running")),
                        "age": item.get("age").unwrap_or(&serde_json::json!("")), "score": score,
                    }));
                }
                return sort_containers(containers);
            }
        }
    }

    if let Ok(out) = tokio::process::Command::new("docker")
        .args([
            "ps",
            "--format",
            "{{.Names}}\t{{.Image}}\t{{.Ports}}\t{{.Status}}\t{{.RunningFor}}",
        ])
        .output()
        .await
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let parts: Vec<&str> = line.splitn(5, '\t').collect();
                if parts.len() < 5 {
                    continue;
                }
                let (name, image, ports_raw, age) = (parts[0], parts[1], parts[2], parts[4]);
                if !image.to_lowercase().contains("intersystems")
                    && !image.to_lowercase().contains("iris")
                {
                    continue;
                }
                let sp = extract_port(ports_raw, "1972")
                    .map(|p| serde_json::json!(p))
                    .unwrap_or(serde_json::Value::Null);
                let wp = extract_port(ports_raw, "52773")
                    .map(|p| serde_json::json!(p))
                    .unwrap_or(serde_json::Value::Null);
                let score = score_container(name, workspace_basename);
                containers.push(serde_json::json!({
                    "name": name, "port_superserver": sp, "port_web": wp,
                    "image": image, "status": "running", "age": age, "score": score,
                }));
            }
        }
    }
    sort_containers(containers)
}

fn sort_containers(mut v: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    v.sort_by(|a, b| {
        let sa = a["score"].as_i64().unwrap_or(0);
        let sb = b["score"].as_i64().unwrap_or(0);
        sb.cmp(&sa).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        })
    });
    v
}

/// Public accessor for list_iris_containers used by iris-agentic-dev init.
pub async fn list_iris_containers_pub(workspace_basename: &str) -> Vec<serde_json::Value> {
    list_iris_containers(workspace_basename).await
}

/// Translate an iris_symbols query string into a SQL fragment and parameters.
/// Supports: plain substring, `Pkg.*` prefix, `Pkg.` trailing dot, mid-glob `Pkg.*.Name`, bare `*`.
pub fn translate_symbols_query(limit: usize, query: &str) -> (String, Vec<serde_json::Value>) {
    let base = format!("SELECT TOP {} Name FROM %Dictionary.ClassDefinition", limit);
    if query == "*" || query.is_empty() {
        return (format!("{} ORDER BY Name", base), vec![]);
    }
    if let Some(prefix) = query.strip_suffix(".*") {
        return (
            format!("{} WHERE Name %STARTSWITH ? ORDER BY Name", base),
            vec![serde_json::Value::String(format!("{}.", prefix))],
        );
    }
    if query.ends_with('.') {
        return (
            format!("{} WHERE Name %STARTSWITH ? ORDER BY Name", base),
            vec![serde_json::Value::String(query.to_string())],
        );
    }
    if query.contains('*') {
        return (
            format!("{} WHERE Name LIKE ? ORDER BY Name", base),
            vec![serde_json::Value::String(query.replace('*', "%"))],
        );
    }
    (
        format!("{} WHERE Name LIKE ? ORDER BY Name", base),
        vec![serde_json::Value::String(format!("%{}%", query))],
    )
}

#[derive(Clone)]
pub struct IrisTools {
    /// Active connection state — wraps iris, source, config metadata, write gate.
    /// Arc<Mutex> allows atomic swap from &self tool handlers (034-live-connection-reload).
    pub connection: Arc<std::sync::Mutex<ConnectionState>>,
    /// Lazy config file watcher for hot-reload. None when no .iris-agentic-dev.toml exists.
    pub config_watcher: Arc<std::sync::Mutex<Option<ConfigWatcher>>>,
    pub registry: Arc<crate::skills::SkillRegistry>,
    /// Shared HTTP client — created once, reused across all tool calls.
    pub client: Arc<reqwest::Client>,
    /// Ring buffer of recent tool calls for skill_propose pattern mining.
    pub history: Arc<std::sync::Mutex<VecDeque<ToolCallEntry>>>,
    /// Pending elicitation state for SCM dialogs.
    pub elicitation_store: Arc<ElicitationStore>,
    /// Session-scoped cache of documents already checked out by us — lets chained
    /// iris_doc writes skip the pre-write SCM probe (invalidated by SCM actions).
    pub checkout_cache: Arc<crate::elicitation::CheckoutCache>,
    /// UUID-keyed in-memory log store for progressive disclosure (027).
    pub log_store: Arc<std::sync::Mutex<log_store::LogStore>>,
    /// Session-scoped TTL cache for %Dictionary introspection results (037).
    pub metadata_cache: Arc<dict::MetadataCache>,
    /// Active toolset — controls which tools are registered.
    pub toolset: Toolset,
    #[allow(dead_code)] // used by #[tool_router] macro-generated code
    tool_router: ToolRouter<IrisTools>,
}

#[tool_router]
impl IrisTools {
    pub fn new(iris: Option<IrisConnection>) -> anyhow::Result<Self> {
        let client = Arc::new(IrisConnection::http_client()?);
        let conn_state = match iris {
            Some(c) => ConnectionState::from_iris(c, ConnectionSource::EnvVars, None),
            None => ConnectionState::new_disconnected(ConnectionSource::EnvVars),
        };
        let log_max = std::env::var("IRIS_LOG_STORE_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50usize);
        let log_ttl = std::env::var("IRIS_LOG_TTL_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60u64);
        Ok(Self {
            connection: Arc::new(std::sync::Mutex::new(conn_state)),
            config_watcher: Arc::new(std::sync::Mutex::new(None)),
            registry: Arc::new(crate::skills::SkillRegistry::new()),
            client,
            history: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(50))),
            elicitation_store: Arc::new(ElicitationStore::new()),
            checkout_cache: Arc::new(crate::elicitation::CheckoutCache::new()),
            log_store: Arc::new(std::sync::Mutex::new(log_store::LogStore::new(
                log_max, log_ttl,
            ))),
            metadata_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            toolset: Toolset::Baseline,
            tool_router: Self::tool_router(),
        })
    }
    /// Convenience constructor for tests — same as `new` but with explicit toolset.
    pub fn new_with_toolset(
        iris: Option<IrisConnection>,
        toolset: Toolset,
    ) -> anyhow::Result<Self> {
        Self::with_registry_and_toolset(
            iris,
            crate::skills::SkillRegistry::new(),
            toolset,
            None,
            None,
        )
    }

    /// Returns the set of tool names registered for the current toolset.
    /// Used by tests and by the benchmark harness to build valid_tool_names.
    pub fn registered_tool_names(&self) -> std::collections::HashSet<String> {
        // The Interop profile is pruned directly from the live router, so derive its
        // names from the router itself — always in sync, no hardcoded duplicate to drift.
        if self.toolset == Toolset::Interop {
            return self
                .tool_router
                .list_all()
                .into_iter()
                .map(|t| t.name.to_string())
                .collect();
        }
        // Authoritative baseline list — 34 tools matching v0.4.x (audit 2026-04-28).
        // REST(14) + Docker(16) + Local(4) = 34
        // 34 - stubs(4) = nostub(30); 30 - merged_removed(10) + merged_added(4) = merged(24)
        // Note: iris_symbols_local is no longer a stub (025-symbols-local-ts)
        let all_tools: &[&str] = &[
            // REST — 14
            "iris_compile",
            "iris_execute",
            "iris_doc",
            "iris_query",
            "iris_symbols",
            "iris_symbols_local",
            "docs_introspect",
            "iris_search",
            "iris_info",
            "iris_macro",
            "iris_table_info",
            "resolve_dynamic_dispatch",
            "extract_message_map_routing",
            "find_subclass_implementations",
            "debug_capture_packet",
            "debug_get_error_logs",
            "iris_generate",
            "iris_generate_class",
            // Docker exec
            "iris_test",
            "debug_map_int_to_cls",
            "debug_source_map",
            "iris_source_control",
            "skill",
            "skill_propose",
            "skill_optimize",
            // Local/CLI — 4
            "skill_share",
            "skill_community",
            "skill_community_install",
            "kb",
            // Interoperability — available in all tiers (036: removed individual stubs)
            "iris_production",
            "iris_interop_query",
            "iris_production_item",
            "iris_credential_list",
            "iris_credential_manage",
            "iris_lookup_manage",
            "iris_lookup_transfer",
            // 026-admin-tools
            "iris_admin",
            // 034-live-connection-reload
            "check_config",
        ];

        // Tools removed in nostub — 4 stubs returning NOT_IMPLEMENTED
        // iris_symbols_local is NO LONGER a stub (025-symbols-local-ts)
        let stub_tools: &[&str] = &[
            "skill_propose",
            "skill_optimize",
            "skill_share",
            "skill_community_install",
        ];

        // Tools removed in merged (on top of stubs)
        // 036: individual interop stubs removed entirely; merged dispatchers now in all tiers
        let merged_removed: &[&str] = &[
            "debug_capture_packet",
            "debug_get_error_logs",
            "debug_map_int_to_cls",
            "debug_source_map",
        ];
        let merged_removed_2: &[&str] = &[] as &[&str]; // placeholder
        let merged_added: &[&str] = &[
            "iris_debug",
            "iris_containers",
            // 026-admin-tools
            "iris_admin",
            // 027-progressive-disclosure
            "iris_get_log",
        ];

        let mut names: std::collections::HashSet<String> =
            all_tools.iter().map(|s| s.to_string()).collect();

        match self.toolset {
            Toolset::Interop => unreachable!("derived from router and returned early above"),
            Toolset::Baseline => {}
            Toolset::Nostub => {
                for s in stub_tools {
                    names.remove(*s);
                }
            }
            Toolset::Merged => {
                for s in stub_tools {
                    names.remove(*s);
                }
                for s in merged_removed {
                    names.remove(*s);
                }
                let _ = merged_removed_2; // unused in this path
                for s in merged_added {
                    names.insert(s.to_string());
                }
                // Apply write-gate: remove write-only tools if not write-allowed
                if !self.write_tools_enabled() {
                    let write_gated: &[&str] = &["iris_production_item", "iris_credential_manage"];
                    for s in write_gated {
                        names.remove(*s);
                    }
                }
            }
        }
        names
    }

    pub fn with_registry(
        iris: Option<IrisConnection>,
        registry: crate::skills::SkillRegistry,
    ) -> anyhow::Result<Self> {
        Self::with_registry_and_toolset(iris, registry, Toolset::Baseline, None, None)
    }
    pub fn with_registry_and_toolset(
        iris: Option<IrisConnection>,
        registry: crate::skills::SkillRegistry,
        toolset: Toolset,
        config_watcher: Option<ConfigWatcher>,
        config_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        let client = Arc::new(IrisConnection::http_client()?);
        let mut router = Self::tool_router();

        // Remove tools from MCP tool list based on toolset (T017–T019, T033, FR-004–011).
        // The `#[tool_router]` macro registers all tools; we prune at construction time.
        if toolset == Toolset::Interop {
            // Interop profile: keep ONLY the interop keep-list; prune everything else.
            // Derived from the live router so an upstream rename/removal can't silently
            // leave a stale name behind (test_interop_toolset_exact guards the keep-list).
            let keep: std::collections::HashSet<&str> = INTEROP_TOOLS.iter().copied().collect();
            let all: Vec<String> = router
                .list_all()
                .into_iter()
                .map(|t| t.name.to_string())
                .collect();
            for name in all {
                if !keep.contains(name.as_str()) {
                    router.remove_route(&name);
                }
            }
        } else {
            let stubs_to_remove: &[&str] = match toolset {
                Toolset::Baseline => &[],
                // iris_symbols_local is NO LONGER a stub (025-symbols-local-ts)
                Toolset::Nostub | Toolset::Merged => &[
                    "skill_propose",           // FR-005
                    "skill_optimize",          // FR-005
                    "skill_share",             // FR-005
                    "skill_community_install", // FR-006
                ],
                Toolset::Interop => unreachable!("handled above"),
            };
            for name in stubs_to_remove {
                router.remove_route(name);
            }

            // For merged toolset: remove debug tools replaced by iris_debug dispatcher.
            // 036: individual interop stubs removed entirely — iris_production/iris_interop_query
            // are now available in all tiers, so no pruning needed for them.
            if toolset == Toolset::Merged {
                let merged_replaced: &[&str] = &[
                    // Replaced by iris_debug (FR-007)
                    "debug_capture_packet",
                    "debug_get_error_logs",
                    "debug_map_int_to_cls",
                    "debug_source_map",
                    // agent_info removed (FR-011)
                    "agent_info",
                    // iris_containers replaces these in merged
                    "iris_list_containers",
                    "iris_select_container",
                    "iris_start_sandbox",
                ];
                for name in merged_replaced {
                    router.remove_route(name);
                }
            } else {
                // For baseline and nostub: remove merged-only dispatcher tools
                // (iris_production/iris_interop_query/iris_production_item are now available everywhere)
                let merged_only: &[&str] = &[
                    "iris_debug",
                    "iris_containers",
                    // 026-admin-tools
                    "iris_admin",
                    // 027-progressive-disclosure
                    "iris_get_log",
                ];
                for name in merged_only {
                    router.remove_route(name);
                }
            }
        }

        let conn_state = match iris {
            Some(c) => {
                let write_tools_enabled = c.is_write_allowed();
                tracing::info!(
                    system_mode = ?c.system_mode,
                    write_tools_enabled,
                    namespace = %c.namespace,
                    "iris-agentic-dev: write tool gate evaluated"
                );
                // Remove write-capable tools if not allowed (issue #26 env guard).
                // iris_production_item is write-capable; available in all tiers but gated on prod.
                if !write_tools_enabled {
                    let write_gated: &[&str] = &["iris_production_item", "iris_credential_manage"];
                    for name in write_gated {
                        router.remove_route(name);
                    }
                }
                {
                    // Record ConfigFile source (and the path) when the connection came from
                    // a .iris-agentic-dev.toml — so check_config shows config_file at
                    // startup, not just after the first hot-reload (issue #21, upstream #82).
                    let (source, file) = if config_path.is_some() {
                        (ConnectionSource::ConfigFile, config_path)
                    } else {
                        (ConnectionSource::AutoDiscovered, None)
                    };
                    ConnectionState::from_iris(c, source, file)
                }
            }
            None => ConnectionState::new_disconnected(ConnectionSource::EnvVars),
        };

        let log_max = std::env::var("IRIS_LOG_STORE_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50usize);
        let log_ttl = std::env::var("IRIS_LOG_TTL_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60u64);

        Ok(Self {
            connection: Arc::new(std::sync::Mutex::new(conn_state)),
            config_watcher: Arc::new(std::sync::Mutex::new(config_watcher)),
            registry: Arc::new(registry),
            client,
            history: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(50))),
            elicitation_store: Arc::new(ElicitationStore::new()),
            checkout_cache: Arc::new(crate::elicitation::CheckoutCache::new()),
            log_store: Arc::new(std::sync::Mutex::new(log_store::LogStore::new(
                log_max, log_ttl,
            ))),
            metadata_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            toolset,
            tool_router: router,
        })
    }

    /// Returns the active IRIS connection, or IRIS_UNREACHABLE if not connected.
    fn get_iris(&self) -> Result<Arc<IrisConnection>, McpError> {
        self.connection
            .lock()
            .unwrap()
            .iris
            .clone()
            .ok_or_else(iris_unreachable)
    }

    /// Check for config file changes then return the active connection.
    /// Use this in tool handlers instead of get_iris() to enable hot-reload (034).
    async fn get_iris_reloaded(&self) -> Result<Arc<IrisConnection>, McpError> {
        self.check_reload().await;
        self.get_iris()
    }

    /// Returns the active write_tools_enabled flag from connection state.
    fn write_tools_enabled(&self) -> bool {
        self.connection.lock().unwrap().write_tools_enabled
    }

    /// Returns the active connection as Option<Arc>, for interop helpers that take Option<&IrisConnection>.
    fn iris_arc(&self) -> Option<Arc<IrisConnection>> {
        self.connection.lock().unwrap().iris.clone()
    }

    /// Check if `.iris-agentic-dev.toml` has changed since last load; if so, reload and re-probe.
    /// Called at the start of every tool handler for lazy hot-reload (034).
    /// Completely silent — no error returned to caller on reload failure.
    async fn check_reload(&self) {
        // Check if watcher says config changed
        let changed = {
            let mut w = self.config_watcher.lock().unwrap();
            w.as_mut().map(|w| w.has_changed()).unwrap_or(false)
        };
        if !changed {
            return;
        }

        // Config file changed — reload and re-probe
        let config_path = {
            let w = self.config_watcher.lock().unwrap();
            w.as_ref().map(|w| w.config_path.clone())
        };
        let Some(config_path) = config_path else {
            return;
        };

        let config_file_str = config_path
            .parent()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        // Parse the new config
        let cfg = crate::iris::workspace_config::load_workspace_config(config_file_str.as_deref());

        let conn_result = match cfg {
            None => {
                // File parse error or missing — set error in state, keep old connection
                let mut conn = self.connection.lock().unwrap();
                conn.config_parse_error =
                    Some("Config file changed but could not be parsed".to_string());
                return;
            }
            Some(cfg) => {
                crate::iris::workspace_config::workspace_config_to_connection(&cfg, "USER")
            }
        };

        // Probe the new connection
        let mut new_conn = match conn_result {
            Some(c) => c,
            None => {
                // container= config — let discovery find it via IRIS_CONTAINER env
                match crate::iris::discovery::discover_iris(None).await {
                    crate::iris::discovery::IrisDiscovery::Found(c) => c,
                    _ => {
                        let mut conn = self.connection.lock().unwrap();
                        conn.config_parse_error = Some(
                            "Hot-reload: could not discover IRIS connection from updated config"
                                .to_string(),
                        );
                        return;
                    }
                }
            }
        };

        new_conn.probe().await;

        // Atomically swap connection
        let new_state =
            ConnectionState::from_iris(new_conn, ConnectionSource::ConfigFile, Some(config_path));
        let mut conn = self.connection.lock().unwrap();
        *conn = new_state;
        conn.config_parse_error = None;
        tracing::info!("iris-agentic-dev: hot-reloaded connection from .iris-agentic-dev.toml");
    }
    fn http_client(&self) -> &reqwest::Client {
        &self.client
    }
    /// Issue #2: telemetry keyed on Result::is_ok() undercounted failures —
    /// tool errors travel as Ok(CallToolResult{is_error: true}). This is the
    /// success predicate history/stats must use.
    fn call_ok(result: &Result<CallToolResult, McpError>) -> bool {
        matches!(result, Ok(r) if r.is_error != Some(true))
    }

    fn record_call(&self, tool: &str, success: bool) {
        if let Ok(mut h) = self.history.lock() {
            if h.len() == 50 {
                h.pop_front();
            }
            h.push_back(ToolCallEntry {
                tool: tool.to_string(),
                success,
                timestamp: std::time::Instant::now(),
            });
        }
    }

    #[tool(
        description = "Compile an ObjectScript class, routine, or wildcard package on IRIS via Atelier REST. Supports 'MyApp.*.cls' for package-level compilation. Returns structured errors with line numbers, columns, and severity. No Python required."
    )]
    async fn iris_compile(
        &self,
        Parameters(p): Parameters<CompileParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        tracing::info!(namespace = %namespace, target = %p.target, "iris_compile");
        let client = self.http_client();

        // Local file path support: if target looks like a file path (contains / or \,
        // or ends with .cls/.mac/.inc and exists on disk), upload via Atelier PUT first.
        let is_local_path = p.target.contains('/')
            || p.target.contains('\\')
            || (p.target.ends_with(".cls") && std::path::Path::new(&p.target).exists());
        if is_local_path {
            let path = std::path::Path::new(&p.target);
            if path.exists() {
                let content = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(e) => {
                        return err_json(
                            "READ_ERROR",
                            &format!("Could not read {}: {}", p.target, e),
                        )
                    }
                };
                // Derive document name from Class declaration or from file name
                let doc_name = content
                    .lines()
                    .find(|l| l.trim_start().to_lowercase().starts_with("class "))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .map(|cls| format!("{}.cls", cls))
                    .unwrap_or_else(|| {
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("Unknown.cls")
                            .to_string()
                    });
                // Upload via Atelier PUT
                let put_url = iris.versioned_ns_url(
                    &namespace,
                    &format!("/doc/{}?ignoreConflict=1", urlencoding::encode(&doc_name)),
                );
                let lines: Vec<&str> = content.lines().collect();
                let put_resp = match client
                    .put(&put_url)
                    .basic_auth(&iris.username, Some(&iris.password))
                    .json(&serde_json::json!({"enc": false, "content": lines}))
                    .send()
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::tools::envelope::transport_fail(
                            "mod::put_resp",
                            &e.to_string(),
                        )
                    }
                };
                if !put_resp.status().is_success() {
                    return err_json(
                        "UPLOAD_FAILED",
                        &format!("PUT {} returned HTTP {}", doc_name, put_resp.status()),
                    );
                }
                // Check PUT response body for Atelier-level errors (200 OK with status.errors
                // can occur on some IRIS builds when the upload fails internally, e.g. build 110
                // SetTextFromString NULL namespace bug).
                let put_body: serde_json::Value = put_resp.json().await.unwrap_or_default();
                if let Some(errs) = put_body["status"]["errors"].as_array() {
                    if !errs.is_empty() {
                        let msg = errs[0]["error"].as_str().unwrap_or("Upload failed");
                        self.record_call("iris_compile", false);
                        return err_json("UPLOAD_FAILED", msg);
                    }
                }
                // Compile via shared compile_document helper
                let local_src = p.target.clone();
                let cr = iris
                    .compile_document(&doc_name, &namespace, &p.flags, client)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let errors: Vec<serde_json::Value> = cr
                    .errors
                    .iter()
                    .map(|e| serde_json::json!({"severity":"error","code":"","line":0,"column":0,"text":e}))
                    .collect();
                let console: Vec<serde_json::Value> = cr
                    .console
                    .iter()
                    .map(|l| serde_json::Value::String(l.clone()))
                    .collect();
                let success = cr.success();
                self.record_call("iris_compile", success);
                let payload = serde_json::json!({
                    "success": success,
                    "target": doc_name,
                    "uploaded_from": local_src,
                    "targets_compiled": 1,
                    "namespace": namespace,
                    "errors": errors,
                    "warnings": [],
                    "console": console,
                });
                if !success {
                    return compile_failure(&doc_name, payload);
                }
                return ok_json(payload);
            }
        }

        // Expand wildcards: resolve "MyApp.*.cls" to a list of matching class names.
        // Bug 8: use namespace (not iris.namespace) and the correct /docnames/CLS endpoint.
        let targets: Vec<String> = if p.target.contains('*') {
            let list_url = iris.versioned_ns_url(&namespace, "/docnames/CLS");
            match client
                .get(&list_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    let pattern = p.target.replace('.', "\\.").replace('*', ".*");
                    let re = regex::Regex::new(&format!("(?i)^{}$", pattern))
                        .unwrap_or_else(|_| regex::Regex::new(".*").unwrap());
                    // /docnames/ returns an array of strings, not objects with a "name" key.
                    body["result"]["content"]
                        .as_array()
                        .unwrap_or(&vec![])
                        .iter()
                        .filter_map(|d| d.as_str())
                        .filter(|n| re.is_match(n))
                        .map(|n| n.to_string())
                        .collect()
                }
                _ => vec![p.target.clone()],
            }
        } else {
            vec![p.target.clone()]
        };

        if targets.is_empty() {
            return err_json(
                "NOT_FOUND",
                &format!("No documents match pattern: {}", p.target),
            );
        }

        // force_writable: attempt to enable namespace via docker exec if available
        if p.force_writable {
            let code = format!(
                "do ##class(%Library.EnsembleMgr).EnableNamespace(\"{}\",1)",
                namespace
            );
            let _ = iris.execute(&code, &namespace).await;
        }

        // Atelier compile: POST with JSON array of document names (with extensions)
        // e.g. ["MyApp.Patient.cls", "MyApp.Utils.cls"]
        let compile_url = iris.versioned_ns_url(
            &namespace,
            &format!("/action/compile?flags={}", urlencoding::encode(&p.flags)),
        );

        // Ensure targets have extensions.
        // Bug 16: the old check `t.contains('.')` skipped top-level classes (no package dot).
        // Correct check: append .cls only when no known extension is already present.
        let targets_with_ext: Vec<String> = targets
            .iter()
            .map(|t| {
                if !t.ends_with(".cls")
                    && !t.ends_with(".mac")
                    && !t.ends_with(".inc")
                    && !t.ends_with(".int")
                {
                    format!("{}.cls", t)
                } else {
                    t.clone()
                }
            })
            .collect();

        // Serialize compiles in-process and retry transient conflicts: Atelier 400s any overlapping
        // /action/compile (empty body), and a busy doc can 423/409. See tools::concurrency.
        let _compile_permit = crate::tools::concurrency::compile_gate().acquire().await;
        let resp = match crate::tools::concurrency::send_with_retry(
            || {
                client
                    .post(&compile_url)
                    .basic_auth(&iris.username, Some(&iris.password))
                    .json(&targets_with_ext)
            },
            true,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return crate::tools::envelope::transport_fail("iris_compile", &e.to_string())
            }
        };

        // Bug 17: `&& != 200` was dead code since 200 is always is_success().
        if !resp.status().is_success() {
            let url_str = compile_url.clone();
            let status = resp.status().as_u16();
            return err_json_with_url("IRIS_UNREACHABLE", &format!("HTTP {}", status), &url_str);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| McpError::internal_error(format!("JSON parse error: {e}"), None))?;

        // Parse compiler output — console is at top level for query-param compile
        let console = body["console"]
            .as_array()
            .or_else(|| body["result"]["console"].as_array())
            .cloned()
            .unwrap_or_default();

        let mut errors = vec![];
        let mut warnings = vec![];

        // Check status.errors first — populated for parse errors (e.g. ERROR #5559) where
        // result.content/console may be empty even though the compile failed.
        if let Some(status_errors) = body["status"]["errors"].as_array() {
            for se in status_errors {
                let msg = se["error"].as_str().unwrap_or("Compile error");
                errors.push(
                    serde_json::json!({"severity":"error","code":"","line":0,"column":0,"text":msg}),
                );
            }
        }
        // Also check status.summary as a fallback — some IRIS versions put the error only there.
        if errors.is_empty() {
            let summary = body["status"]["summary"].as_str().unwrap_or("");
            if summary.contains("ERROR") {
                errors.push(serde_json::json!({"severity":"error","code":"","line":0,"column":0,"text":summary}));
            }
        }

        // Parse console output for per-line errors and warnings.
        // Atelier compile errors: "  1 ERROR #<code>:<line>: <message>"
        // Warnings: "  2 WARNING #<code>:<line>: <message>"
        for line in &console {
            let text = line.as_str().unwrap_or("");
            if let Some(rest) = text.trim().strip_prefix("ERROR ") {
                let parts: Vec<&str> = rest.splitn(3, ':').collect();
                let (code, line_num, msg) = if parts.len() >= 3 {
                    (
                        parts[0].trim(),
                        parts[1].trim().parse::<u32>().unwrap_or(0),
                        parts[2].trim(),
                    )
                } else {
                    ("", 0, rest)
                };
                // Deduplicate: skip if status.errors already has an identical message
                let already_have = errors
                    .iter()
                    .any(|e| e["text"].as_str().map(|t| t.contains(msg)).unwrap_or(false));
                if !already_have {
                    errors.push(serde_json::json!({"severity":"error","code":code,"line":line_num,"column":0,"text":msg}));
                }
            } else if let Some(rest) = text.trim().strip_prefix("WARNING ") {
                let parts: Vec<&str> = rest.splitn(3, ':').collect();
                let (code, line_num, msg) = if parts.len() >= 3 {
                    (
                        parts[0].trim(),
                        parts[1].trim().parse::<u32>().unwrap_or(0),
                        parts[2].trim(),
                    )
                } else {
                    ("", 0, rest)
                };
                warnings.push(serde_json::json!({"severity":"warning","code":code,"line":line_num,"column":0,"text":msg}));
            }
        }

        let success = errors.is_empty();
        self.record_call("iris_compile", success);

        // Write open hint for single non-wildcard successful compile
        let open_uri = if success && !p.target.contains('*') && targets.len() == 1 {
            write_open_hint(&namespace, &p.target);
            Some(format!("isfs://{}/{}", namespace, p.target))
        } else {
            None
        };

        let mut resp = serde_json::json!({
            "success": success,
            "target": p.target,
            "targets_compiled": targets.len(),
            "namespace": namespace,
            "errors": errors,
            "warnings": warnings,
            "console": console,
        });
        if let Some(uri) = open_uri {
            resp["open_uri"] = serde_json::Value::String(uri);
        }

        // Progressive disclosure (027): truncate errors array when count exceeds threshold.
        // Threshold counts distinct error+warning entries (not raw console lines).
        let threshold = log_store::read_inline_threshold("IRIS_INLINE_COMPILE", 20);
        let error_count = resp["errors"].as_array().map(|a| a.len()).unwrap_or(0)
            + resp["warnings"].as_array().map(|a| a.len()).unwrap_or(0);
        if error_count > threshold {
            // Combine errors+warnings into a single array for storage, truncate inline.
            // errors and warnings are truncated separately to preserve their structure.
            log_store::apply_truncation(
                &mut resp,
                "errors",
                threshold,
                p.inline,
                &self.log_store,
                "iris_compile",
            );
        } else {
            resp["truncated"] = serde_json::Value::Bool(false);
        }

        if !success {
            return compile_failure(&p.target, resp);
        }
        ok_json(resp)
    }

    #[tool(
        description = "Run %UnitTest.Manager tests on IRIS and return structured pass/fail results. Uses pure-HTTP execution via Atelier REST — works with or without IRIS_CONTAINER. Pass a class name pattern like 'MyApp.Tests' or 'ISC.sql.TestFoo' to run already-compiled test classes (uses /noload automatically). Pass a directory path like 'MyApp/Tests' to load from disk. Returns suite-level summary inline plus log_id for per-test-case detail via iris_get_log. Result fields: `completed` (the suite ran — the tool worked), `outcome` ('passed'|'failed'|'errors'|'no_tests'), `tests_passed`, and `success` (==tests_passed, kept for back-compat). A completed run with failed>0 is a REAL test result, NOT a tool failure — fix the test/code, don't retry the tool."
    )]
    async fn iris_test(
        &self,
        Parameters(p): Parameters<TestParams>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(requested_namespace = ?p.namespace, pattern = %p.pattern, "iris_test");
        let timeout = std::time::Duration::from_secs(p.timeout);

        // HTTP path only — docker exec path removed (#46: /noload/run assumed pre-loaded
        // classes which never existed in a fresh iris session, causing false "no test classes"
        // errors; HTTP path with /verbose=1 is reliable and works with or without docker).
        // Reports which transport actually ran the tests: "docker" when an IRIS_CONTAINER is
        // present and docker-exec succeeds, else "http" (Atelier REST). Set below.
        let mut path_label = "http";
        let iris = self.get_iris()?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        let client = self.http_client();

        // US3: namespace existence check before running tests.
        let ns_check_code = format!(
            "write ##class(%SYS.Namespace).Exists(\"{}\")",
            namespace.replace('"', "\\\"")
        );
        let ns_exists = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            iris.execute_via_generator(&ns_check_code, "USER", client),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|s| s.trim().starts_with('1'))
        .unwrap_or(true); // If we can't check, assume it exists and let RunTest fail naturally.

        if !ns_exists {
            self.record_call("iris_test", false);
            return envelope::fail_with(
                ERR_NAMESPACE_NOT_FOUND,
                &format!(
                    "Namespace '{}' does not exist on this IRIS instance",
                    namespace
                ),
                serde_json::json!({"namespace": namespace}),
            );
        }

        // Generate a UUID correlation token; used as UserParam in RunTest.
        let correlation_token = log_store::new_log_id();
        let safe_pattern = p.pattern.replace('"', "\\\"");

        // Detect whether the pattern is a compiled class name or a filesystem directory path.
        // Class names contain dots and no path separators: "ISC.sql.Tests", "MyApp.Tests.*"
        // Directory paths contain / or \ : "MyApp/Tests", "/tmp/tests/MyApp"
        // When the pattern is a class name, pass /noload so RunTest looks in the compiled
        // database rather than scanning the filesystem under ^UnitTestRoot.
        let is_class_pattern = !safe_pattern.contains('/') && !safe_pattern.contains('\\');
        let flags = if is_class_pattern {
            "/verbose=1/nodelete/noload"
        } else {
            "/verbose=1/nodelete"
        };

        // Run tests via execute_via_generator (HTTP path).
        // After RunTest completes, ^UnitTest.Result global IS persisted (globals bypass
        // the objectgenerator transaction boundary; SQL %Save() does not).
        let run_code = if is_class_pattern {
            build_class_test_run_code(&safe_pattern, flags, &correlation_token)
        } else {
            // Directory path: set ^UnitTestRoot and pre-create the pattern subdirectory.
            // ^UnitTestRoot is platform-aware: a portable temp dir on Windows (mgr/Temp via
            // %File.TempFilename), or /tmp/httest/ on Linux (matches the container e2e fixtures).
            format!(
                r#"set tIsWin=($zcvt($system.Version.GetOS(),"U")="WINDOWS")
set utRoot=$select(tIsWin:##class(%File).NormalizeDirectory("httest",##class(%File).GetDirectory(##class(%File).TempFilename())),1:"/tmp/httest/")
if '##class(%File).DirectoryExists(utRoot) {{ do ##class(%File).CreateDirectoryChain(utRoot) }}
set pkgDir=##class(%File).NormalizeDirectory("{pattern}",utRoot)
if '##class(%File).DirectoryExists(pkgDir) {{ do ##class(%File).CreateDirectoryChain(pkgDir) }}
set ^UnitTestRoot=utRoot
do ##class(%UnitTest.Manager).RunTest("{pattern}","{flags}","{token}")"#,
                token = correlation_token,
                pattern = safe_pattern,
                flags = flags,
            )
        };

        // Capture the latest %UnitTest result-instance id BEFORE running, so afterwards we can read
        // back exactly the instance(s) THIS run produces. RunTest's verbose stdout is unreliable over
        // the HTTP capture path, but ^UnitTest.Result (projected as %UnitTest_Result.*) is authoritative.
        let before_id: i64 = match tokio::time::timeout(
            std::time::Duration::from_secs(8),
            iris.query(
                "SELECT MAX(ID) AS m FROM %UnitTest_Result.TestInstance",
                vec![],
                &namespace,
                client,
            ),
        )
        .await
        {
            Ok(Ok(body)) => body["result"]["content"][0]["m"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| body["result"]["content"][0]["m"].as_i64())
                .unwrap_or(0),
            _ => 0,
        };

        // Try HTTP (execute_via_generator) first. Fall back to docker exec if:
        // - IRIS_CONTAINER is set, AND
        // - HTTP returns empty output (RunTest couldn't create the pattern directory
        //   because execute_via_generator restricts filesystem writes)
        // RunTest writes verbose output to $IO (terminal device).
        // execute_via_generator redirects $IO to a temp file but RunTest also needs
        // to create directories under ^UnitTestRoot — which fails in that context.
        // When IRIS_CONTAINER is set, prefer docker exec (full filesystem + real terminal).
        let has_container = std::env::var("IRIS_CONTAINER")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some();

        let run_output = if has_container {
            // Docker exec: full filesystem access, captures terminal output from RunTest
            match tokio::time::timeout(timeout, iris.execute(&run_code, &namespace)).await {
                Err(_) => {
                    self.record_call("iris_test", false);
                    return envelope::fail(
                        "TIMEOUT",
                        &format!("Test run timed out after {}s", p.timeout),
                    );
                }
                Ok(Err(_)) => {
                    // Docker exec unavailable — fall through to HTTP
                    match tokio::time::timeout(
                        timeout,
                        iris.execute_via_generator(&run_code, &namespace, client),
                    )
                    .await
                    {
                        Ok(Ok(out)) => out,
                        _ => {
                            self.record_call("iris_test", false);
                            return envelope::fail(
                                "DOCKER_REQUIRED",
                                &format!("iris_test: IRIS_CONTAINER set but docker exec failed and HTTP fallback also failed.{DOCKER_REQUIRED_HINT}"),
                            );
                        }
                    }
                }
                Ok(Ok(out)) => {
                    path_label = "docker";
                    out
                }
            }
        } else {
            // HTTP path: works for remote IRIS without docker
            match tokio::time::timeout(
                timeout,
                iris.execute_via_generator(&run_code, &namespace, client),
            )
            .await
            {
                Err(_) => {
                    self.record_call("iris_test", false);
                    return envelope::fail(
                        "TIMEOUT",
                        &format!("Test run timed out after {}s", p.timeout),
                    );
                }
                Ok(Err(e)) => {
                    self.record_call("iris_test", false);
                    return envelope::fail(ERR_TEST_EXECUTION_ERROR, &e.to_string());
                }
                Ok(Ok(out)) => out,
            }
        };
        // Parse RunTest stdout to build structured results.
        // IRIS RunTest output format (per-method lines):
        //   "    ClassName begins ..."        ← class scope
        //   "      TestFoo() begins ..."
        //   "      TestFoo() PASSED in 0.0001s"
        //   "      TestBar() FAILED in 0.0001s"
        // ^UnitTest.Result only has suite-level data in the objectgenerator context
        // (class/method %Save() calls are inside nested transactions that don't commit).
        // Stdout parsing is reliable and provides timing data directly.
        let mut test_cases: Vec<serde_json::Value> = Vec::new();
        let mut current_class = String::new();
        let mut passed = 0u64;
        let mut failed = 0u64;
        let errors = 0u64;
        let mut class_map: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();

        // PRIMARY result source: read ^UnitTest.Result (the %UnitTest_Result.* SQL projection) for the
        // instance(s) THIS run created (TestInstance ID > before_id). Authoritative and always persisted,
        // unlike the verbose stdout. (Round 3: tests ran + passed but the stdout parse found 0 → false
        // NO_TESTS_FOUND; reading the global fixes it.) Falls back to stdout parsing if the global is empty.
        let mut from_global = false;
        {
            let read_sql = format!(
                "SELECT tc.Name Class, tm.Name Method, tm.Status St, \
                 (SELECT TOP 1 ta.Description FROM %UnitTest_Result.TestAssert ta WHERE ta.TestMethod=tm.ID AND ta.Status=0) FailMsg \
                 FROM %UnitTest_Result.TestMethod tm, %UnitTest_Result.TestCase tc, %UnitTest_Result.TestSuite ts \
                 WHERE tm.TestCase=tc.ID AND tc.TestSuite=ts.ID AND ts.TestInstance > {} ORDER BY tc.Name, tm.Name",
                before_id
            );
            if let Ok(Ok(body)) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                iris.query(&read_sql, vec![], &namespace, client),
            )
            .await
            {
                if let Some(rows) = body["result"]["content"].as_array() {
                    if !rows.is_empty() {
                        from_global = true;
                        for r in rows {
                            let cls = r["Class"].as_str().unwrap_or("").to_string();
                            let method = r["Method"].as_str().unwrap_or("").to_string();
                            let is_passed = match &r["St"] {
                                serde_json::Value::String(s) => s == "1",
                                serde_json::Value::Number(n) => n.as_i64() == Some(1),
                                _ => false,
                            };
                            let failure_message = r["FailMsg"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(|s| serde_json::Value::String(s.to_string()))
                                .unwrap_or(serde_json::Value::Null);
                            if is_passed {
                                passed += 1;
                            } else {
                                failed += 1;
                            }
                            let tc = serde_json::json!({
                                "name": method,
                                "class_name": cls.clone(),
                                "status": if is_passed { "passed" } else { "failed" },
                                "duration_ms": null,
                                "failure_message": failure_message,
                            });
                            test_cases.push(tc.clone());
                            class_map.entry(cls).or_default().push(tc);
                        }
                    }
                }
            }
        }

        // Fallback: parse RunTest verbose stdout, only if the result global had nothing.
        // With /verbose=1, IRIS RunTest outputs:
        //   "    ClassName begins ..."
        //   "      TestFoo() begins ..."   ← method start
        //   "      TestFoo passed"          ← method result (no parens, no timing)
        //   "      TestFoo FAILED -- <msg>" ← method failure
        //   "    ClassName passed"
        if !from_global {
            for line in run_output.lines() {
                let trimmed = line.trim();
                // Class begin: "IrisDevE2E.SmokeTest begins ..."  (contains dot, no parens)
                if trimmed.ends_with("begins ...")
                    && !trimmed.contains("()")
                    && trimmed.contains('.')
                {
                    current_class = trimmed.trim_end_matches(" begins ...").trim().to_string();
                }
                // Method result: "TestFoo passed" or "TestFoo FAILED" or "TestFoo FAILED -- msg"
                // These lines have no "()" and start with "Test"
                else if !trimmed.contains("()") && !trimmed.ends_with("begins ...") {
                    let upper = trimmed.to_uppercase();
                    let (is_passed, is_failed) = (
                        upper.ends_with(" PASSED") || upper.contains(" PASSED "),
                        upper.ends_with(" FAILED") || upper.contains(" FAILED"),
                    );
                    if !is_passed && !is_failed {
                        continue;
                    }
                    let method_name = if is_passed {
                        trimmed
                            .split(" passed")
                            .next()
                            .unwrap_or("")
                            .split(" PASSED")
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    } else {
                        trimmed
                            .split(" failed")
                            .next()
                            .unwrap_or("")
                            .split(" FAILED")
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    };
                    // Skip suite-level result lines (e.g. "MyClass\Sub FAILED") — these contain
                    // path separators and are not individual test methods.
                    // Skip if no class context (suite-level result without a class "begins" line),
                    // or if name contains path separators (suite-level lines, not method names).
                    if method_name.is_empty()
                        || current_class.is_empty()
                        || (!method_name.starts_with("Test") && !method_name.starts_with("test"))
                        || method_name.contains('\\')
                        || method_name.contains('/')
                        || method_name.contains('.')
                    {
                        continue;
                    }
                    let failure_message = if is_failed {
                        trimmed
                            .split_once(" -- ")
                            .map(|x| x.1)
                            .map(|s| serde_json::Value::String(s.trim().to_string()))
                            .unwrap_or(serde_json::Value::Null)
                    } else {
                        serde_json::Value::Null
                    };
                    if is_passed {
                        passed += 1;
                    } else {
                        failed += 1;
                    }
                    let tc = serde_json::json!({
                        "name": method_name,
                        "class_name": current_class,
                        "status": if is_passed { "passed" } else { "failed" },
                        "duration_ms": null,
                        "failure_message": failure_message,
                    });
                    test_cases.push(tc.clone());
                    class_map.entry(current_class.clone()).or_default().push(tc);
                }
            }
        }

        let test_suites: Vec<serde_json::Value> = class_map
            .iter()
            .map(|(name, cases)| {
                let s_fail = cases.iter().filter(|c| c["status"] == "failed").count() as u64;
                serde_json::json!({
                    "name": name,
                    "tests": cases.len(),
                    "failures": s_fail,
                    "errors": 0,
                    "duration_ms": null,
                })
            })
            .collect();

        let total = passed + failed + errors;

        // IRIS creates a synthetic 1-failure suite when the pattern matches no test classes
        // (e.g. "Test022\NonExistent\NoSuchClass FAILED" at the suite level). The method
        // parser skips these (they contain path separators), so test_cases stays empty.
        // Treat any run with no parsed method results as NO_TESTS_FOUND.
        if total == 0 || test_cases.is_empty() {
            self.record_call("iris_test", false);
            // #62: before proposing a correction to the pattern, check whether the pattern
            // is already right. If it NAMES a compiled class, no pattern could have helped —
            // the class is what exposed no runnable test, and saying otherwise sent callers
            // round a loop whose only suggestion was the input they had just sent.
            if is_class_pattern {
                if let Some(shape) =
                    probe_test_class_shape(&iris, &namespace, client, &p.pattern).await
                {
                    let (cause, hint) = no_runnable_tests_cause(&shape, &namespace);
                    return envelope::fail_with(
                        ERR_NO_RUNNABLE_TESTS,
                        &format!(
                            "Test class '{}' is compiled in namespace '{}' but exposed no runnable \
                             test ({}) — the pattern is correct; the class is what needs fixing.",
                            shape.class, namespace, cause
                        ),
                        serde_json::json!({
                            "hint": hint,
                            "cause": cause,
                            "class": shape.class,
                            "extends_test_production": shape.extends_test_production(),
                            "production_parameter": shape.production,
                            "test_methods": shape.own_test_methods,
                            "pattern": p.pattern,
                            "namespace": namespace,
                            "total": 0,
                            "passed": 0,
                            "failed": 0,
                            "path": path_label,
                        }),
                    );
                }
            }
            // A6.2 discovery probe: instead of a bare NO_TESTS_FOUND, list the compiled
            // %UnitTest.TestCase classes that DO exist so the model can correct the pattern
            // (the workshop's #1 iris_test failure was an unactionable "no test classes").
            // PrimarySuper (full inheritance chain), not Super (direct parent only): the workshop's
            // test classes extend %UnitTest.TestProduction (which extends %UnitTest.TestCase), so a
            // `Super LIKE TestCase` probe missed them and only surfaced system classes. Filter out
            // framework/HealthShare packages so the user's own test classes surface.
            // TOP 26, not 25: a full page means there are more than we list, and the
            // guidance says "25+" rather than silently implying that is all of them.
            let probe_sql = "SELECT TOP 26 Name FROM %Dictionary.CompiledClass WHERE PrimarySuper LIKE '%UnitTest.TestCase%' AND Name NOT LIKE 'EnsLib.%' AND Name NOT LIKE 'HS.%' AND Name NOT LIKE 'Ens.%' AND Name NOT LIKE '\\%%' ESCAPE '\\' ORDER BY Name";
            let candidates: Vec<String> = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                iris.query(probe_sql, vec![], &namespace, client),
            )
            .await
            {
                Ok(Ok(body)) => body["result"]["content"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r["Name"].as_str())
                            .filter(|n| !n.starts_with('%'))
                            .map(|n| n.to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => vec![],
            };
            let mut candidates = candidates;
            let more_than_listed = candidates.len() > 25;
            candidates.truncate(25);
            let (hint, did_you_mean) =
                no_tests_found_guidance(&p.pattern, &namespace, &candidates, more_than_listed);
            // Issue #2: NO_TESTS_FOUND is a genuine tool failure (nothing ran),
            // unlike a red run below, which is a valid result and stays non-error.
            // The message carries the pattern and namespace, not just the fact of the
            // miss: clients that surface only `error` used to show a sentence that was
            // true of every failure and specific to none (issue #47).
            let msg = format!(
                "Pattern '{}' matched no test classes in namespace '{}'",
                p.pattern, namespace
            );
            return envelope::fail_with(
                ERR_NO_TESTS_FOUND,
                &msg,
                serde_json::json!({
                    "hint": hint,
                    "candidates": candidates,
                    "did_you_mean": did_you_mean,
                    "more_candidates_than_listed": more_than_listed,
                    "pattern": p.pattern,
                    "namespace": namespace,
                    "total": 0,
                    "passed": 0,
                    "failed": 0,
                    "path": path_label,
                    "source": "stdout_parse",
                }),
            );
        }

        // `success` keeps its historical meaning (all tests green) for back-compat, but we reached
        // this point only because the suite actually RAN and produced parsed method results — so the
        // tool itself worked. Surface that distinction explicitly (issue #8): a run with failed>0 is
        // a real test result, not a tool failure. `outcome` separates the three states.
        let success = failed == 0 && errors == 0;
        let completed = true;
        let outcome = if errors > 0 {
            "errors"
        } else if failed > 0 {
            "failed"
        } else {
            "passed"
        };

        // Store full per-case detail in log store.
        let log_id = {
            let id = log_store::new_log_id();
            let full = serde_json::json!({
                "test_suites": test_suites.iter().map(|s| {
                    let name = s["name"].as_str().unwrap_or("");
                    let cases: Vec<_> = test_cases.iter()
                        .filter(|c| c["class_name"].as_str() == Some(name))
                        .cloned()
                        .collect();
                    let mut suite = s.clone();
                    suite["test_cases"] = serde_json::Value::Array(cases);
                    suite
                }).collect::<Vec<_>>(),
                "raw_output": run_output.trim(),
            });
            let entry = log_store::LogEntry {
                id: id.clone(),
                tool: "iris_test".to_string(),
                created_at: std::time::Instant::now(),
                preview: vec![],
                full_result: full,
                total_count: total as usize,
            };
            if let Ok(mut s) = self.log_store.lock() {
                s.store(entry);
            }
            id
        };

        // Record the TOOL call by whether the tool worked (the suite ran), not by whether the
        // tests passed — otherwise red tests inflate the tool's failure rate (issue #8).
        self.record_call("iris_test", completed);
        ok_json(serde_json::json!({
            "success": success,
            "completed": completed,
            "outcome": outcome,
            "tests_passed": success,
            "total": total,
            "passed": passed,
            "failed": failed,
            "errors": errors,
            "skipped": 0,
            "duration_ms": null,
            "path": path_label,
            "log_id": log_id,
            "pattern": p.pattern,
            "namespace": namespace,
            "test_suites": test_suites,
        }))
    }

    #[tool(
        description = "Execute arbitrary ObjectScript code on IRIS and return stdout. Uses pure-HTTP execution via CodeMode=objectgenerator (write temp class, compile, query result, delete). Falls back to docker exec if IRIS_CONTAINER env var is set and HTTP fails. &sql(...) embedded SQL macros are automatically translated to %SQL.Statement calls (set translate_sql: false to disable). When translation fires, response includes sql_translated: true and translated_code. Example: code='write $ZVERSION,!' returns the IRIS version string. Use this for side-effecting ObjectScript only — for SELECTs use iris_query, for class/table introspection use docs_introspect/iris_symbols/iris_table_info, for production state use iris_production/iris_interop_query, and to create+compile a class use iris_doc(put,compile) over Atelier (never $SYSTEM.OBJ.Load from a file path — that needs IRIS to share this host's disk). When the code matches one of those, the response includes a `hint` naming the typed tool. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE), never a hardcoded USER; the response echoes the namespace it ran in."
    )]
    async fn iris_execute(
        &self,
        Parameters(p): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        tracing::info!(namespace = %namespace, translate_sql = p.translate_sql, "iris_execute");
        let client = self.http_client();
        let timeout = std::time::Duration::from_secs(p.timeout);

        // Advisory: if this code is really a SELECT, class introspection, production-config read,
        // or a load-from-file, the typed tools do it in one round-trip (and avoid the
        // <SYNTAX>errdone+2^%qaqqt failures). Non-blocking — attached as `hint` to the result.
        let redirect_hint = sql_lint::execute_redirect_hint(&p.code);

        // &sql macro translation — rewrite before sending to IRIS (035)
        let translation = if p.translate_sql {
            let r = translate_sql_macros(&p.code);
            Some(r)
        } else {
            None
        };
        let code_to_run = translation
            .as_ref()
            .filter(|r| r.found)
            .map(|r| r.translated_code.as_str())
            .unwrap_or(&p.code);

        // Try pure-HTTP execution first (write-compile-query via CodeMode=objectgenerator).
        let gen_result = tokio::time::timeout(
            timeout,
            iris.execute_via_generator(code_to_run, &namespace, client),
        )
        .await;

        match gen_result {
            Err(_) => {
                self.record_call("iris_execute", false);
                return envelope::fail(
                    "TIMEOUT",
                    &format!("execution timed out after {}s", p.timeout),
                );
            }
            Ok(Ok(output)) => {
                let trimmed = output.trim();
                // Catch ObjectScript runtime errors written by the Catch block or $ZERROR check.
                let is_runtime_error =
                    trimmed.starts_with("ERROR: ") || trimmed.starts_with("ERROR($ZERROR): ");
                self.record_call("iris_execute", !is_runtime_error);
                let mut resp = serde_json::json!({
                    "success": !is_runtime_error,
                    "output": trimmed,
                    "namespace": namespace,
                    "method": "http",
                });
                if is_runtime_error {
                    resp["error_code"] = serde_json::Value::String("IRIS_RUNTIME_ERROR".into());
                } else if trimmed.is_empty() {
                    // Execution succeeded but produced no captured output. With the RunUser
                    // fix the code really ran (this is no longer the objectgenerator silent
                    // failure) — so this is a side-effecting call, or code that returned a
                    // value via Quit/Return instead of Write. Flag it so the model doesn't
                    // assume a value was returned and silently lose data.
                    resp["no_output"] = serde_json::Value::Bool(true);
                    resp["hint"] = serde_json::Value::String(
                        "iris_execute returns only what your code Writes to the current device. \
                         If you expected a value, use `write <expr>,!`. If this was a side-effecting \
                         call (load/compile/save/start), it ran — verify it with a query."
                            .into(),
                    );
                }
                if let Some(ref tr) = translation {
                    if tr.found {
                        resp["sql_translated"] = serde_json::Value::Bool(true);
                        resp["translated_code"] =
                            serde_json::Value::String(tr.translated_code.clone());
                        if !tr.warnings.is_empty() {
                            resp["translation_warning"] = serde_json::Value::Array(
                                tr.warnings
                                    .iter()
                                    .map(|w| serde_json::Value::String(w.clone()))
                                    .collect(),
                            );
                        }
                    }
                }
                if resp.get("hint").is_none() {
                    if let Some(h) = redirect_hint {
                        resp["hint"] = serde_json::Value::String(h.into());
                    }
                }
                // Issue #2: the runtime-error message must live in `error` (not only
                // `output`) and the result must be flagged isError on the wire.
                if is_runtime_error {
                    return envelope::fail_with("IRIS_RUNTIME_ERROR", trimmed, resp);
                }
                return ok_json(resp);
            }
            Ok(Err(_)) => {
                // HTTP path failed — fall through to docker exec.
            }
        }

        // Fallback: docker exec (requires IRIS_CONTAINER env var).
        let docker_result =
            tokio::time::timeout(timeout, iris.execute(code_to_run, &namespace)).await;
        match docker_result {
            Err(_) => {
                self.record_call("iris_execute", false);
                envelope::fail(
                    "TIMEOUT",
                    &format!("execution timed out after {}s", p.timeout),
                )
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                self.record_call("iris_execute", false);
                if msg == "DOCKER_REQUIRED" {
                    envelope::fail(
                        "DOCKER_REQUIRED",
                        &format!("iris_execute: HTTP execution failed and IRIS_CONTAINER is not set for docker exec fallback.{DOCKER_REQUIRED_HINT}"),
                    )
                } else {
                    envelope::fail("EXECUTION_FAILED", &msg)
                }
            }
            Ok(Ok(output)) => {
                let trimmed = output.trim();
                let is_runtime_error =
                    trimmed.starts_with("ERROR: ") || trimmed.starts_with("ERROR($ZERROR): ");
                self.record_call("iris_execute", !is_runtime_error);
                let mut resp = serde_json::json!({
                    "success": !is_runtime_error,
                    "output": trimmed,
                    "namespace": namespace,
                    "method": "docker",
                });
                if is_runtime_error {
                    resp["error_code"] = serde_json::Value::String("IRIS_RUNTIME_ERROR".into());
                }
                if let Some(ref tr) = translation {
                    if tr.found {
                        resp["sql_translated"] = serde_json::Value::Bool(true);
                        resp["translated_code"] =
                            serde_json::Value::String(tr.translated_code.clone());
                        if !tr.warnings.is_empty() {
                            resp["translation_warning"] = serde_json::Value::Array(
                                tr.warnings
                                    .iter()
                                    .map(|w| serde_json::Value::String(w.clone()))
                                    .collect(),
                            );
                        }
                    }
                }
                if resp.get("hint").is_none() {
                    if let Some(h) = redirect_hint {
                        resp["hint"] = serde_json::Value::String(h.into());
                    }
                }
                if is_runtime_error {
                    return envelope::fail_with("IRIS_RUNTIME_ERROR", trimmed, resp);
                }
                ok_json(resp)
            }
        }
    }

    #[tool(
        description = "Read, write, delete, or check an IRIS document. mode='get' fetches source, mode='put' writes (with automatic SCM checkout if needed), mode='delete' removes, mode='head' checks existence. Supports batch ops via 'names' array and elicitation_id/elicitation_answer for SCM dialog resumption. For large source, paginate get with max_bytes + offset (response includes next_offset), or prefer docs_introspect for signatures/structure instead of full source. No Python required."
    )]
    async fn iris_doc(
        &self,
        Parameters(p): Parameters<IrisDocParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        tracing::info!(namespace = %namespace, "iris_doc");
        let client = self.http_client();
        let result = doc::handle_iris_doc(
            &iris,
            client,
            p,
            &self.elicitation_store,
            &self.checkout_cache,
        )
        .await;
        self.record_call("iris_doc", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Execute a SQL SELECT query on IRIS via Atelier REST. Returns rows as a JSON array with column names as keys. By default, destructive SQL (DROP, DELETE, INSERT, UPDATE, ALTER, CREATE, MERGE, TRUNCATE, EXEC, EXECUTE, BULK, LOAD, KILL, LOCK, SELECT INTO) is blocked before reaching IRIS. Set force: true to bypass validation for intentional administrative queries — has no effect on production instances where write tools are disabled. For table/column discovery call iris_table_info first instead of guessing system tables; ObjectScript (set/write/do/##class/&sql/^globals) must use iris_execute, not iris_query. No Python required. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE), never a hardcoded USER; the response echoes the namespace it ran in."
    )]
    async fn iris_query(
        &self,
        Parameters(p): Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(requested_namespace = ?p.namespace, force = p.force, "iris_query");

        // Pre-flight: ObjectScript typed into a SQL tool — fail fast with a clear redirect
        // (28/447 workshop iris_query calls were ObjectScript, not SQL).
        if let Some(reason) = sql_lint::looks_like_objectscript(&p.query) {
            self.record_call("iris_query", false);
            return envelope::fail_with(
                "NOT_SQL",
                &format!("This looks like ObjectScript, not SQL ({reason})."),
                serde_json::json!({"hint": "iris_query runs SQL SELECTs only. For ObjectScript (set/write/do/##class/&sql/^globals), use iris_execute."}),
            );
        }
        let sql_warnings = sql_lint::reserved_word_warnings(&p.query);

        // SQL safety gate — validate before any network call
        let skip_validation = p.force && self.write_tools_enabled();
        if !skip_validation {
            match validate_read_only_sql(&p.query) {
                Err(ref kw) if kw == "EMPTY" => {
                    self.record_call("iris_query", false);
                    return envelope::fail(
                        "EMPTY_QUERY",
                        "SQL query is empty after removing comments.",
                    );
                }
                Err(kw) => {
                    self.record_call("iris_query", false);
                    let mut extra = serde_json::json!({"blocked_keyword": kw});
                    if p.force && !self.write_tools_enabled() {
                        extra["force_ignored"] = serde_json::Value::Bool(true);
                    }
                    return envelope::fail_with(
                        "SQL_WRITE_BLOCKED",
                        &format!("Destructive SQL keyword '{}' is not allowed. Use force: true to override.", kw),
                        extra,
                    );
                }
                Ok(()) => {}
            }
        }

        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        let client = self.http_client();
        let query_url = iris.versioned_ns_url(&namespace, "/action/query");
        let resp = match client
            .post(&query_url)
            .basic_auth(&iris.username, Some(&iris.password))
            .json(&serde_json::json!({"query": p.query, "parameters": p.parameters}))
            .send()
            .await
        {
            Ok(v) => v,
            Err(e) => return crate::tools::envelope::transport_fail("iris_query", &e.to_string()),
        };

        if !resp.status().is_success() {
            return err_json_with_url(
                "IRIS_UNREACHABLE",
                &format!("HTTP {}", resp.status()),
                &query_url,
            );
        }

        let body: serde_json::Value = resp.json().await.unwrap_or_default();

        if let Some(errors) = body["status"]["errors"].as_array() {
            if !errors.is_empty() {
                let msg = errors[0]["error"].as_str().unwrap_or("SQL error");
                self.record_call("iris_query", false);
                let mut extra = serde_json::json!({});
                // 84/447 workshop failures were guesses at nonexistent tables — route the
                // model to real schema discovery instead of more guessing. Prefer a TARGETED
                // hint that names the typed tool for the specific guessed table (SQL-Gateway,
                // namespace, production config); fall back to the generic discovery hint.
                if let Some(h) = sql_lint::targeted_table_hint(&p.query) {
                    extra["hint"] = serde_json::Value::String(h.into());
                } else if sql_lint::is_table_not_found(msg) {
                    extra["hint"] =
                        serde_json::Value::String(sql_lint::TABLE_NOT_FOUND_HINT.into());
                }
                if !sql_warnings.is_empty() {
                    extra["warnings"] = serde_json::Value::Array(
                        sql_warnings
                            .iter()
                            .map(|w| serde_json::Value::String(w.clone()))
                            .collect(),
                    );
                }
                return envelope::fail_with("SQL_ERROR", msg, extra);
            }
        }

        let rows = body["result"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let count = rows.len();
        self.record_call("iris_query", true);
        let mut resp = serde_json::json!({"success": true, "rows": rows, "count": count, "namespace": namespace});
        if !sql_warnings.is_empty() {
            resp["warnings"] = serde_json::Value::Array(
                sql_warnings
                    .iter()
                    .map(|w| serde_json::Value::String(w.clone()))
                    .collect(),
            );
        }
        ok_json(resp)
    }

    #[tool(
        description = "List running IRIS Docker containers with name-match scoring. Tries iris-devtester first, falls back to docker ps. Containers sorted by score (name similarity to workspace) descending."
    )]
    async fn iris_list_containers(
        &self,
        Parameters(p): Parameters<ListContainersParams>,
    ) -> Result<CallToolResult, McpError> {
        self.check_reload().await;
        let workspace_basename = p
            .workspace_root
            .as_deref()
            .map(|r| {
                std::path::Path::new(r)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default();

        let containers = list_iris_containers(&workspace_basename).await;
        let suggestion = containers.first().map(|c: &serde_json::Value| {
            format!(
                "iris_select_container(name='{}')",
                c["name"].as_str().unwrap_or("")
            )
        });
        // FR-012: show which container .iris-agentic-dev.toml would select and if it's running.
        let workspace_config_json = {
            let ws_path = p.workspace_root.as_deref();
            match crate::iris::workspace_config::load_workspace_config(ws_path) {
                None => serde_json::Value::Null,
                Some(ref cfg) => {
                    let container_name = cfg.container.as_deref().unwrap_or("");
                    let running = !container_name.is_empty()
                        && containers
                            .iter()
                            .any(|c| c["name"].as_str() == Some(container_name));
                    let config_path = crate::iris::workspace_config::workspace_root(ws_path)
                        .join(".iris-agentic-dev.toml")
                        .to_string_lossy()
                        .to_string();
                    serde_json::json!({
                        "found": true,
                        "path": config_path,
                        "container": cfg.container,
                        "namespace": cfg.namespace,
                        "running": running,
                    })
                }
            }
        };
        // Add active_connection info so agents can detect workspace_config mismatches
        // without a separate iris_info call.
        let iris_arc = self.iris_arc();
        let active_connection_json = match &iris_arc {
            None => serde_json::Value::Null,
            Some(conn) => {
                // Extract container name from DiscoverySource if available.
                let container = match &conn.source {
                    crate::iris::connection::DiscoverySource::Docker { container_name } => {
                        serde_json::Value::String(container_name.clone())
                    }
                    _ => serde_json::Value::Null,
                };
                serde_json::json!({
                    "base_url": conn.base_url,
                    "namespace": conn.namespace,
                    "version": conn.version,
                    "container": container,
                })
            }
        };

        // Detect mismatch: workspace_config specifies a container but we're connected
        // to something different (or no container at all).
        let mismatch = if let (Some(cfg_container), Some(conn)) =
            (workspace_config_json["container"].as_str(), &iris_arc)
        {
            match &conn.source {
                crate::iris::connection::DiscoverySource::Docker { container_name } => {
                    container_name != cfg_container
                }
                _ => true, // connected via non-Docker path but .iris-agentic-dev.toml specifies a container
            }
        } else {
            false
        };

        let mismatch_hint = if mismatch {
            let cfg_container = workspace_config_json["container"]
                .as_str()
                .unwrap_or("(unknown)");
            let active_container = active_connection_json["container"].as_str();
            let active_url = active_connection_json["base_url"]
                .as_str()
                .unwrap_or("(unknown)");
            let active = active_container.unwrap_or(active_url);
            serde_json::Value::String(format!(
                "Active connection: {}. .iris-agentic-dev.toml specifies: {}. Restart the MCP session from the workspace directory to apply.",
                active, cfg_container
            ))
        } else {
            serde_json::Value::Null
        };

        ok_json(serde_json::json!({
            "status": "ok",
            "containers": containers,
            "workspace_basename": workspace_basename,
            "suggestion": suggestion,
            "workspace_config": workspace_config_json,
            "active_connection": active_connection_json,
            "mismatch": mismatch,
            "mismatch_hint": mismatch_hint,
        }))
    }

    #[tool(
        description = "Switch the active IRIS connection to the specified running Docker container for this session. After a successful switch, all subsequent tool calls target the new container — no session restart required. Fixes issue #11."
    )]
    async fn iris_select_container(
        &self,
        Parameters(p): Parameters<SelectContainerParams>,
    ) -> Result<CallToolResult, McpError> {
        self.check_reload().await;
        let workspace_basename = String::new();
        let namespace =
            interop::resolve_namespace(p.namespace.as_deref(), self.iris_arc().as_deref());

        let containers = list_iris_containers(&workspace_basename).await;
        let found = containers
            .iter()
            .find(|c| c["name"].as_str() == Some(&p.name));

        let container = match found {
            Some(c) => c.clone(),
            None => {
                let available: Vec<_> = containers
                    .iter()
                    .filter_map(|c| c["name"].as_str())
                    .collect();
                return envelope::fail_with(
                    "CONTAINER_NOT_FOUND",
                    &format!("No container matching '{}' found", p.name),
                    serde_json::json!({"requested": p.name, "available": available}),
                );
            }
        };

        let port_superserver = container["port_superserver"].as_u64().unwrap_or(1972) as u16;
        let port_web = container["port_web"].as_u64().unwrap_or(52773) as u16;
        let base_url = format!("http://localhost:{}", port_web);

        let mut new_conn = crate::iris::connection::IrisConnection::new(
            &base_url,
            &namespace,
            &p.username,
            &p.password,
            crate::iris::connection::DiscoverySource::Docker {
                container_name: p.name.clone(),
            },
        );
        new_conn.port_superserver = Some(port_superserver);
        new_conn.probe().await;

        // Check if probe succeeded (version populated means reachable)
        if new_conn.version.is_none() {
            return envelope::fail_with(
                "CONTAINER_UNREACHABLE",
                "Container found but Atelier REST API did not respond. Check that the container is running and the web server is accessible.",
                serde_json::json!({"container": p.name, "port_web": port_web}),
            );
        }

        let version = new_conn.version.clone();
        let write_tools_enabled = new_conn.is_write_allowed();

        // Atomically swap the active connection (fixes issue #11).
        let new_state =
            ConnectionState::from_iris(new_conn, ConnectionSource::IrisSelectContainer, None);
        {
            let mut conn = self.connection.lock().unwrap();
            *conn = new_state;
        }

        tracing::info!(container = %p.name, "iris-agentic-dev: switched connection via iris_select_container");

        ok_json(serde_json::json!({
            "status": "ok",
            "switched": true,
            "container": p.name,
            "port_superserver": port_superserver,
            "port_web": port_web,
            "namespace": namespace,
            "version": version,
            "write_tools_enabled": write_tools_enabled,
        }))
    }

    #[tool(
        description = "Return the active IRIS connection state + this MCP server's own version. It only reports the cached connection snapshot (no IRIS network call), so it ALWAYS succeeds — when IRIS is down it returns connected:false / connection_source:disconnected instead of erroring with IRIS_UNREACHABLE. That is the point: it's the one tool you can call to DIAGNOSE an unreachable IRIS (read the `connected` field) without the call itself failing — unlike iris_query/iris_execute/etc., which do return IRIS_UNREACHABLE. Also use to: verify hot-reload completed; confirm which container/host is active; validate the loaded MCP build (mcp_version). To switch connection mid-session without restart: call check_config first to get config_watch_path, then write a .iris-agentic-dev.toml to that exact path, then call any tool — the reload fires automatically. Fields: mcp_version, toolset, connected, connection_source (http|docker|disconnected), host, port, namespace, container, config_file, config_watch_path, config_loaded_at, iris_version, write_tools_enabled."
    )]
    async fn check_config(
        &self,
        Parameters(_p): Parameters<crate::tools::NoParams>,
    ) -> Result<CallToolResult, McpError> {
        self.check_reload().await;
        let conn = self.connection.lock().unwrap();

        let (host, port, namespace, container, iris_version) = match &conn.iris {
            Some(iris) => {
                // Parse host and port from base_url (e.g. "http://localhost:52780")
                let base = iris
                    .base_url
                    .trim_start_matches("http://")
                    .trim_start_matches("https://");
                let (host_port, _path) = base.split_once('/').unwrap_or((base, ""));
                let (host_str, port_str) =
                    host_port.rsplit_once(':').unwrap_or((host_port, "52773"));
                let host = host_str.to_string();
                let port = port_str.parse::<u64>().unwrap_or(52773);
                let namespace = iris.namespace.clone();
                let container = match &iris.source {
                    crate::iris::connection::DiscoverySource::Docker { container_name } => {
                        serde_json::Value::String(container_name.clone())
                    }
                    _ => serde_json::Value::Null,
                };
                let version = iris
                    .version
                    .clone()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null);
                (host, port, namespace, container, version)
            }
            None => (
                String::new(),
                52773u64,
                String::new(),
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
        };

        let config_file = conn
            .config_file
            .as_ref()
            .and_then(|p| p.to_str())
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null);

        let config_loaded_at = conn
            .loaded_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| {
                // Format as ISO 8601
                let secs = d.as_secs();
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
                    .unwrap_or_default();
                serde_json::Value::String(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            })
            .unwrap_or(serde_json::Value::Null);

        let connection_source =
            serde_json::to_value(&conn.source).unwrap_or(serde_json::Value::Null);

        // Show where the MCP server is looking for .iris-agentic-dev.toml
        // so agents know where to write it for mid-session config changes.
        let config_watcher_path = {
            let w = self.config_watcher.lock().unwrap();
            w.as_ref()
                .map(|w| w.config_path.to_string_lossy().to_string())
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null)
        };

        let mut response = serde_json::json!({
            "connected": conn.iris.is_some(),
            "connection_source": connection_source,
            "host": host,
            "port": port,
            "namespace": namespace,
            "container": container,
            "config_file": config_file,
            "config_loaded_at": config_loaded_at,
            "iris_version": iris_version,
            "write_tools_enabled": conn.write_tools_enabled,
            "config_watch_path": config_watcher_path,
            // The MCP server's OWN version + active toolset, so the loaded build can be validated
            // from a tool call (the serverInfo version shown by Claude Code's /mcp is the same value).
            "mcp_version": env!("CARGO_PKG_VERSION"),
            "toolset": self.toolset.as_str(),
        });

        if let Some(ref err) = conn.config_parse_error {
            response["config_parse_error"] = serde_json::Value::String(err.clone());
        }

        // Surface fallback discovery explicitly: a connection with no config file and a
        // non-explicit source came from Docker/port-scan discovery, which can silently
        // target the wrong instance (issue #21, upstream #82).
        let is_explicit = matches!(
            conn.source,
            ConnectionSource::ConfigFile | ConnectionSource::EnvVars
        );
        if conn.config_file.is_none() && !is_explicit && conn.iris.is_some() {
            response["fallback_warning"] = serde_json::Value::String(
                "No .iris-agentic-dev.toml config file found. Connection established via \
                 fallback discovery (Docker/port scan). Set OBJECTSCRIPT_WORKSPACE or create \
                 a .iris-agentic-dev.toml in your project root to pin the target instance."
                    .to_string(),
            );
        }

        ok_json(response)
    }

    #[tool(
        description = "Start a dedicated IRIS container for the current project via iris-devtester CLI. Idempotent — returns existing container if already running."
    )]
    async fn iris_start_sandbox(
        &self,
        Parameters(p): Parameters<StartSandboxParams>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = std::env::current_dir().unwrap_or_default();
        let workspace_basename = workspace
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        let container_name = if p.name.is_empty() {
            format!("{}-iris", workspace_basename)
        } else {
            p.name.clone()
        };

        let containers = list_iris_containers(&workspace_basename).await;
        if let Some(c) = containers
            .iter()
            .find(|c| c["name"].as_str() == Some(&container_name))
        {
            if c["port_superserver"].is_number() {
                return ok_json(serde_json::json!({
                    "name": container_name,
                    "port_superserver": c["port_superserver"],
                    "port_web": c["port_web"],
                    "started": false,
                    "idempotent": true,
                }));
            }
        }

        let output = tokio::process::Command::new("idt")
            .args([
                "container",
                "up",
                "--name",
                &container_name,
                "--edition",
                &p.edition,
            ])
            .output()
            .await;

        match output {
            Err(e) => err_json(
                "INTERNAL_ERROR",
                &format!("idt not found: {e}. Install with: pip install iris-devtester"),
            ),
            Ok(out) if !out.status.success() => {
                let msg = String::from_utf8_lossy(&out.stderr);
                err_json("INTERNAL_ERROR", &format!("idt container up failed: {msg}"))
            }
            Ok(_) => {
                let containers2 = list_iris_containers(&workspace_basename).await;
                match containers2
                    .iter()
                    .find(|c| c["name"].as_str() == Some(&container_name))
                {
                    Some(c) => ok_json(serde_json::json!({
                        "name": container_name,
                        "port_superserver": c["port_superserver"],
                        "port_web": c["port_web"],
                        "started": true,
                    })),
                    None => ok_json(serde_json::json!({
                        "name": container_name,
                        "started": true,
                        "warning": "Container started but not yet visible in container list.",
                    })),
                }
            }
        }
    }

    #[tool(
        description = "Search for ObjectScript classes matching a query in the IRIS namespace. Query supports: plain substring ('Patient'), package prefix ('HT.*' or 'HT.'), mid-glob ('HT.*.Service'), or bare '*' for all."
    )]
    async fn iris_symbols(
        &self,
        Parameters(p): Parameters<SymbolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let client = self.http_client();
        let (sql, params) = translate_symbols_query(p.limit, &p.query);
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        match iris.query(&sql, params, &namespace, client).await {
            Ok(resp) => ok_json(serde_json::json!({
                "source": "iris_dictionary",
                "symbols": resp["result"]["content"],
                "count": resp["result"]["content"].as_array().map(|a| a.len()).unwrap_or(0),
                "query_hint": "Supports: plain text (substring), 'Pkg.*' (package prefix), 'Pkg.*.Name' (glob)",
            })),
            Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
        }
    }

    #[tool(
        description = "Search for ObjectScript symbols in local .cls/.mac/.inc files on disk — no IRIS connection required. query: glob pattern (MyApp.*, *Service, MyApp.Foo). workspace_path: optional path (defaults to OBJECTSCRIPT_WORKSPACE or cwd). limit: max symbols to return (default 50)."
    )]
    async fn iris_symbols_local(
        &self,
        Parameters(p): Parameters<SymbolsLocalParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.query.trim().is_empty() {
            return err_json("INVALID_PARAMS", "query must not be empty");
        }
        let limit = p.limit.clamp(1, 500);

        // Resolve workspace path: param → OBJECTSCRIPT_WORKSPACE env → cwd
        let workspace = if let Some(ref ws) = p.workspace_path {
            std::path::PathBuf::from(ws)
        } else if let Ok(ws) = std::env::var("OBJECTSCRIPT_WORKSPACE") {
            std::path::PathBuf::from(ws)
        } else {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        };

        if !workspace.exists() {
            return err_json(
                "WORKSPACE_NOT_FOUND",
                &format!("{} does not exist", workspace.display()),
            );
        }

        let result = symbols_local::scan_workspace(&workspace, &p.query, limit);

        let symbols_json: Vec<serde_json::Value> = result
            .symbols
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or_default())
            .collect();
        let warnings_json: Vec<serde_json::Value> = result
            .parse_warnings
            .iter()
            .map(|w| serde_json::to_value(w).unwrap_or_default())
            .collect();
        let count = symbols_json.len();

        ok_json(serde_json::json!({
            "source": "local_filesystem",
            "symbols": symbols_json,
            "count": count,
            "query_hint": "Supports: plain text (exact), 'Pkg.*' (package prefix), '*Suffix' (suffix), 'Pkg.*.Name' (glob)",
            "parse_warnings": warnings_json,
        }))
    }

    #[tool(
        description = "Introspect an ObjectScript class — returns methods, properties, and type information."
    )]
    async fn docs_introspect(
        &self,
        Parameters(p): Parameters<IntrospectParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let client = self.http_client();
        // Bug 15: use parameterized queries instead of manual string escaping.
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        let methods = iris.query(
            "SELECT Name,FormalSpec,ReturnType FROM %Dictionary.CompiledMethod WHERE parent=? ORDER BY Name",
            vec![serde_json::Value::String(p.class_name.clone())],
            &namespace,
            client,
        ).await.unwrap_or_default();
        let props = iris
            .query(
                "SELECT Name,Type FROM %Dictionary.CompiledProperty WHERE parent=? ORDER BY Name",
                vec![serde_json::Value::String(p.class_name.clone())],
                &namespace,
                client,
            )
            .await
            .unwrap_or_default();
        ok_json(
            serde_json::json!({"success": true, "class_name": p.class_name, "methods": methods["result"]["content"], "properties": props["result"]["content"]}),
        )
    }

    #[tool(
        description = "Map a .INT routine offset to the original .CLS source line. Pass routine+offset OR a raw IRIS error string like '<UNDEFINED>x+3^MyApp.Foo.1'."
    )]
    async fn debug_map_int_to_cls(
        &self,
        Parameters(mut p): Parameters<DebugMapParams>,
    ) -> Result<CallToolResult, McpError> {
        if !p.error_string.is_empty() {
            if let Some((r, o)) = parse_iris_error_string(&p.error_string) {
                p.routine = r;
                p.offset = o;
            }
        }
        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        let _client = self.http_client();
        let code = format!(
            "Write ##class(%Studio.Debugger).SourceLine(\"{}\",{})",
            p.routine.replace('"', "\\\""),
            p.offset
        );
        match iris.execute(&code, &namespace).await {
            Ok(raw) => {
                let (cls_name, cls_line) = parse_source_line(raw.trim());
                ok_json(
                    serde_json::json!({"success": true, "mapping_available": cls_name.is_some(), "cls_name": cls_name, "cls_line": cls_line, "routine": p.routine, "offset": p.offset, "raw_error": if p.error_string.is_empty() { serde_json::Value::Null } else { p.error_string.into() }}),
                )
            }
            Err(e) if e.to_string() == "DOCKER_REQUIRED" => envelope::fail(
                "DOCKER_REQUIRED",
                &format!("debug_map_int requires docker exec. Set IRIS_CONTAINER=<container_name>.{DOCKER_REQUIRED_HINT}"),
            ),
            Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
        }
    }

    #[tool(description = "Capture IRIS error state and recent error log entries for debugging.")]
    async fn debug_capture_packet(
        &self,
        Parameters(_p): Parameters<CapturePacketParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let namespace = interop::resolve_namespace(_p.namespace.as_deref(), Some(&iris));
        let client = self.http_client();
        match iris.query("SELECT TOP 20 ErrorCode,ErrorText,TimeStamp FROM %SYSTEM.Error ORDER BY TimeStamp DESC", vec![], &namespace, client).await {
            Ok(resp) => ok_json(serde_json::json!({"success": true, "errors": resp["result"]["content"]})),
            Err(e) => {
                let msg = e.to_string();
                // %SYSTEM.Error is not available on community edition — return empty gracefully
                if msg.contains("SQLCODE: -30") || msg.contains("Table") && msg.contains("not found") {
                    ok_json(serde_json::json!({"success": true, "errors": [], "note": "%SYSTEM.Error not available on this IRIS edition"}))
                } else {
                    err_json("IRIS_UNREACHABLE", &msg)
                }
            }
        }
    }

    #[tool(description = "Retrieve recent IRIS error log entries.")]
    async fn debug_get_error_logs(
        &self,
        Parameters(p): Parameters<ErrorLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let client = self.http_client();
        // FR-012: cap max_entries to prevent runaway queries.
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        let max_entries = p.max_entries.min(1000);
        let sql = format!("SELECT TOP {} ErrorCode,ErrorText,TimeStamp FROM %SYSTEM.Error ORDER BY TimeStamp DESC", max_entries);
        match iris.query(&sql, vec![], &namespace, client).await {
            Ok(resp) => {
                let mut result =
                    serde_json::json!({"success": true, "logs": resp["result"]["content"]});
                // Progressive disclosure (027): truncate logs when count exceeds threshold.
                let threshold = log_store::read_inline_threshold("IRIS_INLINE_ERROR_LOGS", 20);
                log_store::apply_truncation(
                    &mut result,
                    "logs",
                    threshold,
                    p.inline,
                    &self.log_store,
                    "debug_get_error_logs",
                );
                ok_json(result)
            }
            Err(e) => {
                let msg = e.to_string();
                // %SYSTEM.Error not available on community edition — return empty gracefully
                if msg.contains("SQLCODE: -30")
                    || (msg.contains("Table") && msg.contains("not found"))
                {
                    ok_json(
                        serde_json::json!({"success": true, "logs": [], "note": "%SYSTEM.Error not available on this IRIS edition"}),
                    )
                } else {
                    err_json("IRIS_UNREACHABLE", &msg)
                }
            }
        }
    }

    #[tool(
        description = "Build a .INT source map for a compiled ObjectScript class via Atelier xecute. Maps .INT routine line offsets back to .CLS source lines for stack trace resolution. No Python required."
    )]
    async fn debug_source_map(
        &self,
        Parameters(p): Parameters<SourceMapParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let _client = self.http_client();
        let cls_name = p.cls_name.trim_end_matches(".cls");
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        // Build source map by querying %Studio.Debugger for each .INT method
        let code = format!(
            "set cls=\"{}\" set rtn=$translate(cls,\".\",\".\") set map=\"{{\" set first=1 set method=\"\" for {{ set method=$order(^rIndex(rtn,method)) quit:method=\"\"  set intline=$get(^rIndex(rtn,method)) if 'first {{ set map=map_\",\" }} set map=map_\"\\\"\"_method_\"\\\":\\\"\"_intline_\"\\\"\" set first=0 }} set map=map_\"}}\" write map",
            cls_name.replace('"', "\\\"")
        );
        // Bug 23: use namespace, not the hardcoded "USER".
        match iris.execute(&code, &namespace).await {
            Ok(output) => {
                let map: serde_json::Value =
                    serde_json::from_str(output.trim()).unwrap_or(serde_json::json!({}));
                ok_json(
                    serde_json::json!({"success": true, "cls_name": cls_name, "source_map": map}),
                )
            }
            Err(e) if e.to_string() == "DOCKER_REQUIRED" => envelope::fail(
                "DOCKER_REQUIRED",
                &format!("debug_source_map requires docker exec. Set IRIS_CONTAINER=<container_name>.{DOCKER_REQUIRED_HINT}"),
            ),
            Err(e) => err_json("IRIS_UNREACHABLE", &e.to_string()),
        }
    }

    #[tool(
        description = "Generate an ObjectScript class from a natural language description. Requires IRIS_GENERATE_CLASS_MODEL + OPENAI_API_KEY env vars."
    )]
    async fn iris_generate_class(
        &self,
        Parameters(p): Parameters<GenerateClassParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::generate::{
            extract_class_name, validate_cls_syntax, LlmClient, GENERATE_CLASS_SYSTEM,
            RETRY_TEMPLATE,
        };
        let llm = LlmClient::from_env().ok_or_else(|| {
            McpError::invalid_request(
                "LLM_UNAVAILABLE: Set IRIS_GENERATE_CLASS_MODEL and OPENAI_API_KEY",
                None,
            )
        })?;

        let class_text = llm
            .complete(GENERATE_CLASS_SYSTEM, &p.description)
            .await
            .map_err(|e| McpError {
                code: rmcp::model::ErrorCode::INTERNAL_ERROR,
                message: format!("LLM_TIMEOUT: {}", e).into(),
                data: None,
            })?;

        if !validate_cls_syntax(&class_text) {
            return envelope::fail_with(
                "INVALID_OUTPUT",
                "Generated output is not a valid ObjectScript class",
                serde_json::json!({"raw_llm_output": class_text}),
            );
        }
        let class_name =
            extract_class_name(&class_text).unwrap_or_else(|| "Generated.Class".to_string());

        if let Some(iris) = self.iris_arc().as_deref() {
            let _client = self.http_client();
            let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
            let code = format!(
                "Set sc=$SYSTEM.OBJ.Compile(\"{}\",\"ck-d\") Write $System.Status.IsOK(sc)",
                class_name
            );
            let compile_ok = iris
                .execute(&code, &namespace)
                .await
                .map(|o| o.trim() == "1")
                .unwrap_or(false);

            if !compile_ok {
                let retry_prompt = RETRY_TEMPLATE.replace("{errors}", "compilation failed");
                if let Ok(fixed) = llm
                    .complete(
                        GENERATE_CLASS_SYSTEM,
                        &format!(
                            "{}

Original: {}",
                            retry_prompt, class_text
                        ),
                    )
                    .await
                {
                    let fixed_name = extract_class_name(&fixed).unwrap_or(class_name.clone());
                    let code2 = format!(
                        "Set sc=$SYSTEM.OBJ.Compile(\"{}\",\"ck-d\") Write $System.Status.IsOK(sc)",
                        fixed_name
                    );
                    let ok2 = iris
                        .execute(&code2, &namespace)
                        .await
                        .map(|o| o.trim() == "1")
                        .unwrap_or(false);
                    return ok_json(
                        serde_json::json!({"success": true, "class_name": fixed_name, "class_text": fixed, "compiled": ok2, "retried": true}),
                    );
                }
            }
            return ok_json(
                serde_json::json!({"success": true, "class_name": class_name, "class_text": class_text, "compiled": compile_ok, "retried": false}),
            );
        }
        ok_json(
            serde_json::json!({"success": true, "class_name": class_name, "class_text": class_text, "compiled": false, "retried": false, "note": "No IRIS connection — could not compile"}),
        )
    }

    #[tool(
        description = "Generate a %UnitTest.TestCase for an existing ObjectScript class. Introspects the class first. Requires IRIS_GENERATE_CLASS_MODEL + OPENAI_API_KEY."
    )]
    async fn iris_generate_test(
        &self,
        Parameters(p): Parameters<GenerateTestParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::generate::{
            extract_class_name, validate_cls_syntax, LlmClient, GENERATE_TEST_SYSTEM,
        };
        let llm = LlmClient::from_env().ok_or_else(|| {
            McpError::invalid_request(
                "LLM_UNAVAILABLE: Set IRIS_GENERATE_CLASS_MODEL and OPENAI_API_KEY",
                None,
            )
        })?;

        let introspection_context = if let Some(iris) = self.iris_arc().as_deref() {
            let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
            let client = self.http_client();
            // FR-001/C1: use parameterized query to prevent SQL injection via class_name.
            iris.query(
                "SELECT Name,FormalSpec,ReturnType FROM %Dictionary.CompiledMethod WHERE parent=? ORDER BY Name",
                vec![serde_json::Value::String(p.class_name.clone())],
                &namespace,
                client,
            )
                .await
                .map(|r| {
                    format!(
                        "Class: {}
Methods:
{}",
                        p.class_name,
                        serde_json::to_string_pretty(&r["result"]["content"]).unwrap_or_default()
                    )
                })
                .unwrap_or_else(|_| format!("Class: {} (introspection unavailable)", p.class_name))
        } else {
            format!(
                "Class: {} (no IRIS connection — generating scaffold)",
                p.class_name
            )
        };

        let prompt = format!(
            "Generate tests for the following ObjectScript class:

{}",
            introspection_context
        );
        let test_text = llm
            .complete(GENERATE_TEST_SYSTEM, &prompt)
            .await
            .map_err(|e| McpError {
                code: rmcp::model::ErrorCode::INTERNAL_ERROR,
                message: format!("LLM_TIMEOUT: {}", e).into(),
                data: None,
            })?;

        if !validate_cls_syntax(&test_text) {
            return envelope::fail_with(
                "INVALID_OUTPUT",
                "Generated output is not a valid ObjectScript test class",
                serde_json::json!({"raw_llm_output": test_text}),
            );
        }
        let test_class_name =
            extract_class_name(&test_text).unwrap_or_else(|| format!("Test.{}", p.class_name));
        ok_json(
            serde_json::json!({"success": true, "class_name": p.class_name, "test_class_name": test_class_name, "test_text": test_text, "introspected": !introspection_context.contains("unavailable")}),
        )
    }

    #[tool(description = "List all synthesized skills in the registry.")]
    async fn skill_list(&self, _: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        if let Some(iris) = self.iris_arc().as_deref() {
            let code = "Set key=\"\" Set result=\"[\" Set sep=\"\" For { Set key=$Order(^SKILLS(key)) Quit:key=\"\" Set skill=$Get(^SKILLS(key)) Set result=result_sep_skill Set sep=\",\" } Set result=result_\"]\" Write result";
            if let Ok(output) = iris
                .execute(code, &crate::tools::skills_tools::skills_namespace())
                .await
            {
                if let Ok(skills) = serde_json::from_str::<serde_json::Value>(output.trim()) {
                    let count = skills.as_array().map(|a| a.len()).unwrap_or(0);
                    return ok_json(serde_json::json!({"skills": skills, "count": count}));
                }
            }
        }
        ok_json(serde_json::json!({"skills": [], "count": 0}))
    }

    #[tool(description = "Describe a skill by name.")]
    async fn skill_describe(
        &self,
        Parameters(p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(iris) = self.iris_arc().as_deref() {
            let code = format!("Write $Get(^SKILLS(\"{}\"))", p.name.replace('"', "\\\""));
            if let Ok(output) = iris
                .execute(&code, &crate::tools::skills_tools::skills_namespace())
                .await
            {
                if let Ok(skill) = serde_json::from_str::<serde_json::Value>(output.trim()) {
                    return ok_json(serde_json::json!({"success": true, "skill": skill}));
                }
            }
        }
        err_json("NOT_FOUND", &format!("Skill '{}' not found", p.name))
    }

    #[tool(
        description = "Search synthesized skills by name and description. Returns skills whose name or description contains the query terms."
    )]
    async fn skill_search(
        &self,
        Parameters(p): Parameters<SkillSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(iris) = self.iris_arc().as_deref() {
            let query_lower = p.query.to_lowercase();
            let q = query_lower.replace('"', "");
            let code = format!(
                concat!(
                    r#"Set key="",results="[",sep="" "#,
                    r#"For {{ Set key=$Order(^SKILLS(key)) Quit:key="" "#,
                    r#"Set skill=$Get(^SKILLS(key)) "#,
                    r#"If ($ZConvert(skill,"L")["{0}")||($ZConvert(key,"L")["{0}") "#,
                    r#"{{ Set results=results_sep_skill Set sep="," }} }} "#,
                    r#"Set results=results_"]" Write results"#
                ),
                q
            );
            if let Ok(output) = iris
                .execute(&code, &crate::tools::skills_tools::skills_namespace())
                .await
            {
                if let Ok(skills) = serde_json::from_str::<Vec<serde_json::Value>>(output.trim()) {
                    let limited: Vec<_> = skills.into_iter().take(p.top_k).collect();
                    let count = limited.len();
                    return ok_json(
                        serde_json::json!({"query": p.query, "results": limited, "count": count}),
                    );
                }
            }
        }
        ok_json(serde_json::json!({"query": p.query, "results": [], "count": 0}))
    }

    #[tool(description = "Remove a skill from the registry by name.")]
    async fn skill_forget(
        &self,
        Parameters(p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(iris) = self.iris_arc().as_deref() {
            let code = format!(
                "Kill ^SKILLS(\"{}\") Write \"OK\"",
                p.name.replace('"', "\\\"")
            );
            if iris
                .execute(&code, &crate::tools::skills_tools::skills_namespace())
                .await
                .is_ok()
            {
                return ok_json(serde_json::json!({"success": true, "name": p.name}));
            }
        }
        err_json(
            "DOCKER_REQUIRED",
            &format!("skill_forget requires docker exec. Set IRIS_CONTAINER=<container_name>.{DOCKER_REQUIRED_HINT}"),
        )
    }

    #[tool(
        description = "Trigger pattern miner to synthesize new skills from recorded tool calls."
    )]
    async fn skill_propose(&self, _: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        err_json(
            "NOT_IMPLEMENTED",
            "skill_propose: pattern mining not yet implemented",
        )
    }

    #[tool(description = "Optimize a skill using DSPy. Requires OBJECTSCRIPT_DSPY=true.")]
    async fn skill_optimize(
        &self,
        Parameters(_p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        err_json(
            "NOT_IMPLEMENTED",
            "skill_optimize: DSPy optimization not yet implemented",
        )
    }

    #[tool(description = "Share a skill to the community via GitHub PR.")]
    async fn skill_share(
        &self,
        Parameters(_p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        err_json(
            "NOT_IMPLEMENTED",
            "skill_share: GitHub PR integration not yet implemented",
        )
    }

    #[tool(
        description = "List all skills loaded from --subscribe packages. Use --subscribe owner/repo when starting iris-agentic-dev mcp to load community skills."
    )]
    async fn skill_community_list(
        &self,
        _: Parameters<NoParams>,
    ) -> Result<CallToolResult, McpError> {
        let skills: Vec<_> = self
            .registry
            .list_skills()
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                    "source": s.source_repo,
                })
            })
            .collect();
        let kb_items: Vec<_> = self
            .registry
            .list_kb_items()
            .iter()
            .map(|k| {
                serde_json::json!({
                    "title": k.title,
                    "source": k.source_repo,
                })
            })
            .collect();
        ok_json(serde_json::json!({
            "skills": skills,
            "kb_items": kb_items,
            "skill_count": skills.len(),
            "kb_count": kb_items.len(),
            "hint": "Start iris-agentic-dev mcp with --subscribe owner/repo to load community packages"
        }))
    }

    #[tool(description = "Install a community skill from the GitHub community repo.")]
    async fn skill_community_install(
        &self,
        Parameters(_p): Parameters<CommunityPkgParams>,
    ) -> Result<CallToolResult, McpError> {
        err_json(
            "NOT_IMPLEMENTED",
            "skill_community_install: community registry not yet implemented",
        )
    }

    #[tool(description = "Index markdown files into the IRIS knowledge base for semantic search.")]
    async fn kb_index(
        &self,
        Parameters(p): Parameters<KbIndexParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        skills_tools::handle_kb(
            &iris,
            self.http_client(),
            skills_tools::KbParams {
                action: "index".into(),
                path: p.workspace_path,
                query: None,
                top_k: 0,
            },
        )
        .await
    }

    #[tool(
        description = "Search the knowledge base for relevant guidance. Searches subscribed KB packages and any indexed content."
    )]
    async fn kb_recall(
        &self,
        Parameters(p): Parameters<KbRecallParams>,
    ) -> Result<CallToolResult, McpError> {
        let q = p.query.to_lowercase();
        let mut results: Vec<serde_json::Value> = vec![];

        // Search subscribed KB items (BM25 substring match)
        for item in self.registry.list_kb_items() {
            let content_lower = item.content.to_lowercase();
            if content_lower.contains(&q) || item.title.to_lowercase().contains(&q) {
                // Extract a relevant snippet around the match
                let snippet = content_lower
                    .find(&q)
                    .and_then(|pos| {
                        // FR-018/Mo4: use char-boundary-safe slicing to prevent None on multibyte UTF-8.
                        let snippet_start = {
                            let mut s = pos.saturating_sub(150);
                            while s > 0 && !item.content.is_char_boundary(s) {
                                s -= 1;
                            }
                            s
                        };
                        let snippet_end = {
                            let mut e = (pos + q.len() + 300).min(item.content.len());
                            while e < item.content.len() && !item.content.is_char_boundary(e) {
                                e += 1;
                            }
                            e
                        };
                        item.content.get(snippet_start..snippet_end)
                    })
                    .map(|s| format!("...{}...", s.trim()))
                    .unwrap_or_else(|| item.content.chars().take(300).collect());
                results.push(serde_json::json!({
                    "title": item.title,
                    "snippet": snippet,
                    "source": item.source_repo,
                    "score": if item.title.to_lowercase().contains(&q) { 0.9 } else { 0.7 }
                }));
            }
        }

        // Sort by score descending, limit to top_k
        results.sort_by(|a, b| {
            b["score"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(p.top_k);

        let count = results.len();
        ok_json(serde_json::json!({"query": p.query, "results": results, "count": count}))
    }

    #[tool(description = "Return recent tool call history for this session.")]
    async fn agent_history(
        &self,
        Parameters(p): Parameters<AgentHistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let calls: Vec<serde_json::Value> = self
            .history
            .lock()
            .map(|h| {
                h.iter()
                    .rev()
                    .take(p.limit)
                    .map(|c| {
                        serde_json::json!({
                            "tool": c.tool,
                            "success": c.success,
                            "ago_secs": c.timestamp.elapsed().as_secs(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        ok_json(serde_json::json!({"calls": calls, "limit": p.limit}))
    }

    #[tool(description = "Return learning agent status: skill count, pattern count, KB size.")]
    async fn agent_stats(&self, _: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        let skill_count = self.registry.list_skills().len();
        let session_calls = self.history.lock().map(|h| h.len()).unwrap_or(0);
        let learning_enabled = std::env::var("OBJECTSCRIPT_LEARNING")
            .map(|v| v != "false")
            .unwrap_or(true);
        ok_json(serde_json::json!({
            "status": "ok",
            "skill_count": skill_count,
            "session_calls": session_calls,
            "learning_enabled": learning_enabled,
        }))
    }

    #[tool(
        description = "Full-text search across IRIS documents via Atelier REST v2. documents: REQUIRED wildcard scope (e.g. [\"MyPkg.*.cls\"]) — Atelier greps sequentially, so an unscoped search is refused (SCOPE_REQUIRED) rather than timing out server-side. Case-INsensitive by default (case_sensitive: true for exact case). Supports regex and category filter (CLS/MAC/INT/INC/ALL). Auto-upgrades to async polling when the server defers via workId. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE)."
    )]
    async fn iris_search(
        &self,
        Parameters(p): Parameters<search::SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result =
            search::handle_iris_search(&iris, self.http_client(), p, Arc::clone(&self.log_store))
                .await;
        self.record_call("iris_search", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Discover IRIS namespace contents. what=documents lists all docs, what=modified lists recently changed, what=namespace returns config, what=metadata returns IRIS version, what=jobs lists active jobs, what=csp_apps lists CSP apps, what=csp_debug returns debug ID, what=sa_schema returns SQL Analytics schema."
    )]
    async fn iris_info(
        &self,
        Parameters(p): Parameters<info::InfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result =
            info::handle_iris_info(&iris, self.http_client(), p, Arc::clone(&self.log_store)).await;
        self.record_call("iris_info", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Inspect a SQL table: returns whether it is a class-projected table or DDL-created, the backing data/index globals, and (optionally) an approximate row count. Works for both class-projected tables (with real storage globals from %Dictionary.CompiledStorage) and DDL tables (globals inferred by IRIS naming convention). Use include_row_count=true to add a COUNT(*) estimate. Call this (or docs_introspect) to discover the real schema/table/column names BEFORE iris_query, rather than guessing catalog tables."
    )]
    async fn iris_table_info(
        &self,
        Parameters(p): Parameters<info::TableInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = info::handle_iris_table_info(&iris, self.http_client(), p).await;
        self.record_call("iris_table_info", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Resolve ObjectScript dynamic dispatch: find all compiled classes that implement a given method. Use when you see $classmethod(var, method) or ##class({variable}).Method() and need to know the possible targets. Returns candidates with confidence scores (fewer matches = higher confidence). Confidence: 1 match=0.90, 2-5=0.75, 6-20=0.55, >20=0.30. Results cached 60s per session."
    )]
    async fn resolve_dynamic_dispatch(
        &self,
        Parameters(p): Parameters<dict::ResolveDynamicDispatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = dict::handle_resolve_dynamic_dispatch(
            &iris,
            self.http_client(),
            p,
            &self.metadata_cache,
        )
        .await;
        self.record_call("resolve_dynamic_dispatch", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Extract Ensemble MessageMap routing table from a compiled BusinessProcess or Router class. Returns the MessageType → Method dispatch table with confidence 0.9 (compiled routing = near ground truth). Use to find CALLS edges that static analysis cannot see. Returns has_message_map:false for classes without a MessageMap. Results cached 60s per session."
    )]
    async fn extract_message_map_routing(
        &self,
        Parameters(p): Parameters<dict::ExtractMessageMapParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = dict::handle_extract_message_map_routing(
            &iris,
            self.http_client(),
            p,
            &self.metadata_cache,
        )
        .await;
        self.record_call("extract_message_map_routing", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Find all concrete subclass implementations of a method in the full inheritance hierarchy. Given base class names and a method name, expands to all descendants at any depth and returns classes where the method is defined (Origin = parent, not inherited). Use to resolve polymorphic dispatch: adapter.Execute() → find all EnsLib.*.Adapter subclasses that implement Execute. Results cached 60s per session."
    )]
    async fn find_subclass_implementations(
        &self,
        Parameters(p): Parameters<dict::FindSubclassImplementationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = dict::handle_find_subclass_implementations(
            &iris,
            self.http_client(),
            p,
            &self.metadata_cache,
        )
        .await;
        self.record_call("find_subclass_implementations", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Inspect IRIS macros. action=list returns all macros, action=signature returns parameters, action=location finds definition file/line, action=definition returns text, action=expand expands with arguments."
    )]
    async fn iris_macro(
        &self,
        Parameters(p): Parameters<info::MacroParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = info::handle_iris_macro(&iris, self.http_client(), p).await;
        self.record_call("iris_macro", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "IRIS debug tools. action=map_int maps a runtime error offset to source line, action=error_logs fetches recent error log entries, action=capture captures current error state, action=source_map builds .INT to .CLS mapping."
    )]
    async fn iris_debug(
        &self,
        Parameters(p): Parameters<info::DebugParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = info::handle_iris_debug(&iris, self.http_client(), p).await;
        self.record_call("iris_debug", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Prepare context for generating an ObjectScript class or %UnitTest. Returns a ready-to-use prompt plus IRIS namespace context (existing class names, method signatures). No API key needed — the calling AI agent does the generation using the returned prompt, then saves with iris_doc(mode=put) and compiles with iris_compile. gen_type=class for new classes, gen_type=test for %UnitTest scaffolding."
    )]
    async fn iris_generate(
        &self,
        Parameters(p): Parameters<info::GenerateParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = info::handle_iris_generate(&iris, self.http_client(), p).await;
        self.record_call("iris_generate", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Manage the learning agent skill registry. action=list returns all skills, action=describe returns one skill, action=search finds skills by keyword, action=forget removes a skill, action=propose mines recent tool calls and synthesizes a new skill (requires ≥5 calls)."
    )]
    async fn skill(
        &self,
        Parameters(p): Parameters<skills_tools::SkillParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = skills_tools::handle_skill(&iris, self.http_client(), p, &self.history).await;
        self.record_call("skill", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Community skill registry. action=list browses published skills from subscribed GitHub repos, action=install writes a community skill to the local ^SKILLS global."
    )]
    async fn skill_community(
        &self,
        Parameters(p): Parameters<skills_tools::SkillCommunityParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result =
            skills_tools::handle_skill_community(&iris, self.http_client(), p, &self.registry)
                .await;
        self.record_call("skill_community", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Knowledge base tools. action=index reads markdown/text files and stores them in ^KBCHUNKS, action=recall searches the KB for relevant content by keyword."
    )]
    async fn kb(
        &self,
        Parameters(p): Parameters<skills_tools::KbParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = skills_tools::handle_kb(&iris, self.http_client(), p).await;
        self.record_call("kb", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Session and learning agent information. what=stats returns skill count and session call count, what=history returns recent tool call history."
    )]
    async fn agent_info(
        &self,
        Parameters(p): Parameters<skills_tools::AgentInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result =
            skills_tools::handle_agent_info(&iris, self.http_client(), p, &self.history).await;
        self.record_call("agent_info", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "IRIS source control operations. action=status checks lock state and owner, action=menu lists available SCM actions, action=checkout checks out the document, action=execute runs a specific SCM action by ID. Handles elicitation for interactive SCM dialogs. Pass elicitation_id+answer to resume a pending SCM interaction."
    )]
    async fn iris_source_control(
        &self,
        Parameters(p): Parameters<ScmParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let result = scm::handle_iris_source_control(
            &iris,
            self.http_client(),
            p,
            &self.elicitation_store,
            &self.checkout_cache,
        )
        .await;
        self.record_call("iris_source_control", Self::call_ok(&result));
        result
    }

    // ── Merged tools (T029–T032, registered only when IRIS_TOOLSET=merged) ─────
    // These are always present in the #[tool_router] but removed via remove_route()
    // for Baseline and Nostub toolsets in with_registry_and_toolset().
    // Note: iris_debug already exists above as a real tool — it IS the merged debug dispatcher.

    #[tool(
        description = "Interoperability production lifecycle (merged). action: status=get current state, start=start a production — pass production=<Package.ProductionName> (production_name and name are accepted too), stop=stop the running production, restart=recycle ONE config item (pass item=<config item name>), update=hot-apply config, check=check if update needed, recover=recover troubled production, get_autostart/set_autostart=read or set this namespace's autostart production. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace."
    )]
    async fn iris_production(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let action = p.get("action").and_then(|v| v.as_str()).unwrap_or("status");
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        let namespace =
            interop::resolve_namespace(p.get("namespace").and_then(|v| v.as_str()), iris_opt);
        if let Some(iris) = iris_opt {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_production", false);
                return blocked;
            }
        }
        let result = match action {
            "status" => {
                interop::interop_production_status_impl(
                    iris_opt,
                    interop::ProductionStatusParams {
                        namespace: namespace.clone(),
                        full_status: p.get("full").and_then(|v| v.as_bool()).unwrap_or(false),
                    },
                )
                .await
            }
            "start" => {
                interop::interop_production_start_impl(
                    iris_opt,
                    interop::ProductionNameParams {
                        // #63: production / production_name / name all work.
                        production: interop::production_name_arg(&p),
                        namespace: namespace.clone(),
                    },
                )
                .await
            }
            "stop" => {
                interop::interop_production_stop_impl(
                    iris_opt,
                    interop::ProductionStopParams {
                        // #63: production / production_name / name all work.
                        production: interop::production_name_arg(&p),
                        namespace: namespace.clone(),
                        timeout: p.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30) as u32,
                        force: p.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
                    },
                )
                .await
            }
            "update" => {
                interop::interop_production_update_impl(
                    iris_opt,
                    interop::ProductionUpdateParams {
                        namespace: namespace.clone(),
                        timeout: 30,
                        force: false,
                    },
                )
                .await
            }
            "check" => {
                interop::interop_production_needs_update_impl(
                    iris_opt,
                    interop::ProductionNeedsUpdateParams {
                        namespace: namespace.clone(),
                    },
                )
                .await
            }
            "recover" => {
                interop::interop_production_recover_impl(
                    iris_opt,
                    interop::ProductionRecoverParams {
                        namespace: namespace.clone(),
                    },
                )
                .await
            }
            "get_autostart" => {
                interop::interop_autostart_get_impl(
                    iris_opt,
                    &interop::ProductionAutostartParams {
                        action: "get_autostart".into(),
                        namespace: namespace.clone(),
                        enabled: None,
                        production: None,
                    },
                ).await
            }
            "set_autostart" => {
                interop::interop_autostart_set_impl(
                    iris_opt,
                    &interop::ProductionAutostartParams {
                        action: "set_autostart".into(),
                        namespace: namespace.clone(),
                        enabled: p.get("enabled").and_then(|v| v.as_bool()),
                        production: interop::production_name_arg(&p),
                    },
                ).await
            }
            "restart" => {
                // B: recycle ONE config item (disable+enable, each with UpdateProduction).
                let ns = namespace.as_str();
                let item = p
                    .get("item")
                    .or_else(|| p.get("component"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                interop::interop_production_restart_item_impl(iris_opt, ns, item).await
            }
            _ => err_json(
                "INVALID_ACTION",
                "iris_production: action must be status, start, stop, restart (item), update, check, recover, get_autostart, or set_autostart",
            ),
        };
        self.record_call("iris_production", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Interoperability query dispatcher (merged). what (REQUIRED): logs=Event Log entries, queues=message queue depths, messages=message archive (Ens.MessageHeader), trace=ALL of one session (MessageHeader chain + Event Log events) by session_id, partners=configured Ens.Config.BusinessPartner rows. Filters: component=<config item> and session_id=<n> narrow logs/messages to one item/session; since_id=<n> tails only rows after a watermark (no MAX(ID) round-trip). what=messages can also search message CONTENT — the typed replacement for hand SQL against Ens.MessageHeader: (a) body_class=<msg class> + body_where=<SQL fragment on the body table> + body_select=[cols] joins the body table server-side (SQL name resolved for you); (b) search_table={prop, value|value_like, class?, extent?} searches an indexed Search Table field (extent default EnsLib.HL7.SearchTable; errors list the searchable props). Pass namespace=<production namespace> for a specific interop namespace (defaults to the connection's). For SQL-Gateway connections (no SQL table), use iris_table_info / the introspect-dont-guess agent."
    )]
    async fn iris_interop_query(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        // B9: `what` is required-with-enum — fail fast with the valid set instead of silently
        // defaulting (the workshop's missing-discriminator calls otherwise misfired).
        let what = match p.get("what").and_then(|v| v.as_str()) {
            Some(w) if !w.is_empty() => w,
            _ => {
                self.record_call("iris_interop_query", false);
                return envelope::fail(
                    "MISSING_WHAT",
                    "iris_interop_query requires 'what': one of logs, queues, messages, trace, partners.",
                );
            }
        };
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        #[allow(unused_variables)]
        let result = match what {
            "logs" => {
                interop::interop_logs_impl(
                    iris_opt,
                    interop::LogsParams {
                        namespace: p
                            .get("namespace")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        item_name: p
                            .get("component")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        log_type: p
                            .get("log_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("error,warning")
                            .to_string(),
                        session_id: p.get("session_id").and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                        }),
                        since_id: p.get("since_id").and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                        }),
                        limit: p.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32,
                    },
                )
                .await
            }
            "queues" => {
                let ns = p
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                interop::interop_queues_impl(iris_opt, ns).await
            }
            "messages" => {
                interop::interop_message_search_impl(
                    iris_opt,
                    interop::MessageSearchParams {
                        namespace: p
                            .get("namespace")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        source: p
                            .get("source")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        target: p
                            .get("target")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        class_name: p
                            .get("message_class")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        session_id: p.get("session_id").and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                        }),
                        since_id: p.get("since_id").and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                        }),
                        limit: p.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32,
                        body_class: p
                            .get("body_class")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        body_where: p
                            .get("body_where")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        body_select: p
                            .get("body_select")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        search_table: p
                            .get("search_table")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok()),
                    },
                )
                .await
            }
            "trace" => {
                // A: full session trace (MessageHeader chain + Ens_Util.Log events) by SessionId.
                match p.get("session_id").and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                }) {
                    Some(sid) => {
                        let ns = p
                            .get("namespace")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        interop::interop_trace_impl(iris_opt, ns, sid).await
                    }
                    None => err_json(
                        "MISSING_SESSION_ID",
                        "iris_interop_query what=trace requires a numeric session_id.",
                    ),
                }
            }
            "partners" => {
                // B8: real Ens.Config.BusinessPartner rows instead of guessing config tables.
                let ns = p
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                interop::interop_partners_impl(iris_opt, ns).await
            }
            other => err_json(
                "INVALID_WHAT",
                &format!("iris_interop_query: unknown what='{other}'. Valid: logs, queues, messages, trace, partners."),
            ),
        };
        self.record_call("iris_interop_query", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Container lifecycle dispatcher (merged). action: list=list running IRIS containers, select=validate container connection, start=start sandbox container via iris-devtester."
    )]
    async fn iris_containers(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let action = p.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let workspace = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        let result = match action {
            "list" => {
                let params = ListContainersParams {
                    workspace_root: workspace,
                };
                self.iris_list_containers(Parameters(params)).await
            }
            "select" => {
                let params = SelectContainerParams {
                    name: name.unwrap_or_default(),
                    namespace: None,
                    username: default_username(),
                    password: default_password(),
                };
                self.iris_select_container(Parameters(params)).await
            }
            "start" => {
                let params = StartSandboxParams {
                    name: name.unwrap_or_default(),
                    edition: default_edition(),
                };
                self.iris_start_sandbox(Parameters(params)).await
            }
            _ => err_json(
                "INVALID_ACTION",
                "iris_containers: action must be list, select, or start",
            ),
        };
        self.record_call("iris_containers", Self::call_ok(&result));
        result
    }

    // ─── 024-interop-depth: Production item control (US1) ───

    #[tool(
        description = "Add, remove, enable, disable, or inspect/modify settings of an Interoperability production config item — the typed way to build/manipulate a production without hand-rolling ##class(Ens.Config.*) ObjectScript. action: add|remove|enable|disable|get_settings|set_settings. item: exact config item name. For add: class_name (the BS/BO/BP/adapter class the item runs, required), optional enabled (default true), production (defaults to the running one), pool_size, category, and settings (key-value; prefix a key with 'Adapter.' to target the adapter, e.g. 'Adapter.FilePath', otherwise it targets the Host). For remove: item (+ optional production). settings: key-value map for set_settings. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace, and it is the parameter that matters when an item is 'not found'. Changes apply live via Ens.Director.UpdateProduction when the target production is running (set_settings honours apply=false to batch). Works via HTTP, no Docker required."
    )]
    async fn iris_production_item(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let action = p
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let item = p
            .get("item")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let _iris_arc_hold = self.iris_arc();
        let namespace = interop::resolve_namespace(
            p.get("namespace").and_then(|v| v.as_str()),
            _iris_arc_hold.as_deref(),
        );
        if let Some(iris) = _iris_arc_hold.as_deref() {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_production_item", false);
                return blocked;
            }
        }
        let settings: std::collections::HashMap<String, String> = p
            .get("settings")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        // C: set_settings applies live by default; apply=false batches (apply later via update).
        let apply = p.get("apply").and_then(|v| v.as_bool()).unwrap_or(true);
        // add/remove fields.
        let class_name = p
            .get("class_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let enabled = p.get("enabled").and_then(|v| v.as_bool());
        let production = p
            .get("production")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let pool_size = p.get("pool_size").and_then(|v| v.as_i64());
        let category = p
            .get("category")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let result = interop::interop_production_item_impl(
            self.iris_arc().as_deref(),
            interop::ProductionItemParams {
                action,
                item,
                namespace,
                settings,
                apply,
                class_name,
                enabled,
                production,
                pool_size,
                category,
            },
        )
        .await;
        self.record_call("iris_production_item", Self::call_ok(&result));
        result
    }

    // ─── 056-interop-depth: message bodies, business rules, production diff ───

    #[tool(
        description = "Read an Ensemble message body by message header ID (Ens.StringContainer, Ens.StreamContainer, %Stream.Object). Message bodies may contain PHI, so this tool refuses by default: pass dataPolicy=redact to blank known HL7 v2 PHI fields (PID-3/5/7/8/11/18, MSH-3), or dataPolicy=allow together with acknowledgePhi=true to read it unredacted. max_bytes caps the read (default 65536, hard cap 1048576) and the response reports actual_size and truncated. Use iris_interop_query first to find message IDs. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace."
    )]
    async fn iris_message_body(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        let namespace =
            interop::resolve_namespace(p.get("namespace").and_then(|v| v.as_str()), iris_opt);
        if let Some(iris) = iris_opt {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_message_body", false);
                return blocked;
            }
        }
        // Default block: a body can carry patient data, so reading one is opt-in.
        let data_policy = p
            .get("dataPolicy")
            .and_then(|v| v.as_str())
            .unwrap_or("block")
            .to_string();
        let result = interop::handle_iris_message_body(
            iris_opt,
            &interop::MessageBodyParams {
                message_id: p
                    .get("message_id")
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default(),
                namespace,
                max_bytes: p.get("max_bytes").and_then(|v| v.as_u64()).unwrap_or(65536) as u32,
                acknowledge_phi: p
                    .get("acknowledgePhi")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
            &data_policy,
        )
        .await;
        self.record_call("iris_message_body", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Inspect business rule sets (Ens.Rule.RuleSet). action=list returns every rule set in the namespace with its class name, description and last-modified time; action=get with rule_name returns that rule set's description plus its rule/condition/action counts. Use this to find the real rule name before editing a routing rule. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace."
    )]
    async fn iris_business_rule_info(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        let namespace =
            interop::resolve_namespace(p.get("namespace").and_then(|v| v.as_str()), iris_opt);
        if let Some(iris) = iris_opt {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_business_rule_info", false);
                return blocked;
            }
        }
        let result = interop::handle_iris_business_rule_info(
            iris_opt,
            &interop::BusinessRuleInfoParams {
                action: p
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("list")
                    .to_string(),
                rule_name: p
                    .get("rule_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                namespace,
            },
        )
        .await;
        self.record_call("iris_business_rule_info", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Diff a production's live config items against its committed class source, so you can see what was changed in the Management Portal but never saved to source control. Returns in_sync plus a changes array of added/modified/removed items. production: optional — defaults to the running production in the namespace. Requires source control to be configured for the namespace. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace."
    )]
    async fn iris_production_diff(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        let namespace =
            interop::resolve_namespace(p.get("namespace").and_then(|v| v.as_str()), iris_opt);
        if let Some(iris) = iris_opt {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_production_diff", false);
                return blocked;
            }
        }
        let result = interop::handle_iris_production_diff(
            iris_opt,
            &interop::ProductionDiffParams {
                production: p
                    .get("production")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                namespace,
            },
        )
        .await;
        self.record_call("iris_production_diff", Self::call_ok(&result));
        result
    }

    // ─── 024-interop-depth: Ensemble credentials (US2) ───

    #[tool(
        description = "List all Ensemble credentials (IDs and usernames only — passwords never returned). namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace."
    )]
    async fn iris_credential_list(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let namespace = interop::resolve_namespace(
            p.get("namespace").and_then(|v| v.as_str()),
            _iris_arc_hold.as_deref(),
        );
        if let Some(iris) = _iris_arc_hold.as_deref() {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_credential_list", false);
                return blocked;
            }
        }
        let result = interop::interop_credential_list_impl(
            _iris_arc_hold.as_deref(),
            interop::CredentialListParams { namespace },
        )
        .await;
        self.record_call("iris_credential_list", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Create, update, or delete an Ensemble credential. action: create|update|delete. id: credential ID (required). username/password: required for create, optional for update. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace. Write-gated: suppressed on Live instances unless IRIS_ALLOW_PROD=1."
    )]
    async fn iris_credential_manage(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let namespace = interop::resolve_namespace(
            p.get("namespace").and_then(|v| v.as_str()),
            _iris_arc_hold.as_deref(),
        );
        if let Some(iris) = _iris_arc_hold.as_deref() {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_credential_manage", false);
                return blocked;
            }
        }
        let result = interop::interop_credential_manage_impl(
            _iris_arc_hold.as_deref(),
            interop::CredentialManageParams {
                action: p
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                id: p
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                username: p
                    .get("username")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                password: p
                    .get("password")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                namespace: namespace.clone(),
            },
        )
        .await;
        self.record_call("iris_credential_manage", Self::call_ok(&result));
        result
    }

    // ─── 024-interop-depth: Lookup tables (US3) ───

    #[tool(
        description = "Read, write, delete, or list Ensemble lookup table entries. action: get|set|delete|list_keys|list_tables. table: table name (required except list_tables). key: required for get/set/delete. value: required for set. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace. get/list_keys/list_tables always available; set/delete write-gated."
    )]
    async fn iris_lookup_manage(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let namespace = interop::resolve_namespace(
            p.get("namespace").and_then(|v| v.as_str()),
            _iris_arc_hold.as_deref(),
        );
        if let Some(iris) = _iris_arc_hold.as_deref() {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_lookup_manage", false);
                return blocked;
            }
        }
        let result = interop::interop_lookup_manage_impl(
            _iris_arc_hold.as_deref(),
            interop::LookupManageParams {
                action: p
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                table: p
                    .get("table")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                key: p.get("key").and_then(|v| v.as_str()).map(|s| s.to_string()),
                value: p
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                namespace: namespace.clone(),
            },
        )
        .await;
        self.record_call("iris_lookup_manage", Self::call_ok(&result));
        result
    }

    #[tool(
        description = "Export or import an Ensemble lookup table as XML. action: export|import. table: table name. xml: XML string (required for import). namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace. export always available; import write-gated."
    )]
    async fn iris_lookup_transfer(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let _iris_arc_hold = self.iris_arc();
        let namespace = interop::resolve_namespace(
            p.get("namespace").and_then(|v| v.as_str()),
            _iris_arc_hold.as_deref(),
        );
        if let Some(iris) = _iris_arc_hold.as_deref() {
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &namespace).await {
                self.record_call("iris_lookup_transfer", false);
                return blocked;
            }
        }
        let result = interop::interop_lookup_transfer_impl(
            _iris_arc_hold.as_deref(),
            interop::LookupTransferParams {
                action: p
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                table: p
                    .get("table")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                xml: p.get("xml").and_then(|v| v.as_str()).map(|s| s.to_string()),
                namespace: namespace.clone(),
            },
        )
        .await;
        self.record_call("iris_lookup_transfer", Self::call_ok(&result));
        result
    }

    // ── 026-admin-tools: iris_admin dispatcher ───────────────────────────────

    #[tool(
        description = "IRIS administration dispatcher. action: list_namespaces, list_databases, list_users, list_roles, list_user_roles, check_permission, list_webapps, get_webapp (read — always available); create_user, update_user, delete_user, create_namespace, delete_namespace, create_webapp, delete_webapp (write — requires IRIS_ADMIN_TOOLS=1). All operations run in %SYS namespace. check_permission checks the currently connected user (IRIS_USERNAME), not an arbitrary user."
    )]
    async fn iris_admin(
        &self,
        Parameters(p): Parameters<AnyParams>,
    ) -> Result<CallToolResult, McpError> {
        let action = p.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let _iris_arc_hold = self.iris_arc();
        let iris_opt = _iris_arc_hold.as_deref();
        let result = match action {
            "list_namespaces" => admin::admin_list_namespaces_impl(iris_opt).await,
            "list_databases" => admin::admin_list_databases_impl(iris_opt).await,
            "list_users" => admin::admin_list_users_impl(iris_opt).await,
            "list_roles" => admin::admin_list_roles_impl(iris_opt).await,
            "list_webapps" => {
                let type_filter = p.get("type").and_then(|v| v.as_str());
                admin::admin_list_webapps_impl(iris_opt, type_filter).await
            }
            "list_user_roles" => {
                let username = p.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return err_json("INVALID_PARAMS", "username is required for list_user_roles");
                }
                admin::admin_list_user_roles_impl(iris_opt, username).await
            }
            "get_webapp" => {
                let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return err_json("INVALID_PARAMS", "path is required for get_webapp");
                }
                admin::admin_get_webapp_impl(iris_opt, path).await
            }
            "check_permission" => {
                let resource = p.get("resource").and_then(|v| v.as_str()).unwrap_or("");
                let permission = p.get("permission").and_then(|v| v.as_str()).unwrap_or("USE");
                if resource.is_empty() {
                    return err_json("INVALID_PARAMS", "resource is required for check_permission");
                }
                admin::admin_check_permission_impl(iris_opt, resource, permission).await
            }
            "create_user" => {
                let username = p.get("username").and_then(|v| v.as_str()).unwrap_or("");
                let password = p.get("password").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() || password.is_empty() {
                    return err_json("INVALID_PARAMS", "username and password are required for create_user");
                }
                admin::admin_create_user_impl(
                    iris_opt, username, password,
                    p.get("full_name").and_then(|v| v.as_str()),
                    p.get("roles").and_then(|v| v.as_str()),
                ).await
            }
            "update_user" => {
                let username = p.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return err_json("INVALID_PARAMS", "username is required for update_user");
                }
                admin::admin_update_user_impl(
                    iris_opt, username,
                    p.get("password").and_then(|v| v.as_str()),
                    p.get("enabled").and_then(|v| v.as_bool()),
                    p.get("roles").and_then(|v| v.as_str()),
                ).await
            }
            "delete_user" => {
                let username = p.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return err_json("INVALID_PARAMS", "username is required for delete_user");
                }
                admin::admin_delete_user_impl(iris_opt, username).await
            }
            "create_namespace" => {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let code_db = p.get("code_database").and_then(|v| v.as_str()).unwrap_or("");
                let data_db = p.get("data_database").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() || code_db.is_empty() || data_db.is_empty() {
                    return err_json("INVALID_PARAMS", "name, code_database, and data_database are required");
                }
                admin::admin_create_namespace_impl(iris_opt, name, code_db, data_db).await
            }
            "delete_namespace" => {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    return err_json("INVALID_PARAMS", "name is required for delete_namespace");
                }
                admin::admin_delete_namespace_impl(iris_opt, name).await
            }
            "create_webapp" => {
                let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let ns = p.get("namespace").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() || ns.is_empty() {
                    return err_json("INVALID_PARAMS", "path and namespace are required for create_webapp");
                }
                admin::admin_create_webapp_impl(
                    iris_opt, path, ns,
                    p.get("dispatch_class").and_then(|v| v.as_str()),
                    p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                ).await
            }
            "delete_webapp" => {
                let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return err_json("INVALID_PARAMS", "path is required for delete_webapp");
                }
                admin::admin_delete_webapp_impl(iris_opt, path).await
            }
            _ => err_json(
                "INVALID_ACTION",
                "iris_admin: action must be one of: list_namespaces, list_databases, list_users, list_roles, list_user_roles, check_permission, list_webapps, get_webapp, create_user, update_user, delete_user, create_namespace, delete_namespace, create_webapp, delete_webapp",
            ),
        };
        self.record_call("iris_admin", Self::call_ok(&result));
        result
    }

    // ── iris_get_log (027 — progressive disclosure, Merged tier only) ──────────

    #[tool(
        description = "Retrieve a stored result by log_id from the progressive disclosure store. With id: returns the full result (optionally paginated with limit/offset). Without id: lists all stored log entries with their IDs, tools, timestamps, and total counts. Use after any tool returns truncated:true."
    )]
    async fn iris_get_log(
        &self,
        Parameters(p): Parameters<GetLogParams>,
    ) -> Result<CallToolResult, McpError> {
        match p.id {
            None => {
                // List all non-expired entries
                let summaries = self
                    .log_store
                    .lock()
                    .map(|mut s| s.list())
                    .unwrap_or_default();
                ok_json(serde_json::json!({
                    "success": true,
                    "logs": summaries,
                }))
            }
            Some(ref id) => {
                // Validate limit
                if let Some(lim) = p.limit {
                    if lim == 0 {
                        return err_json("INVALID_PARAMS", "limit must be > 0");
                    }
                }

                // Check TTL / existence first
                let get_result = self
                    .log_store
                    .lock()
                    .map(|s| s.get(id))
                    .unwrap_or(log_store::GetResult::NotFound);

                match get_result {
                    log_store::GetResult::NotFound => err_json(
                        "LOG_NOT_FOUND",
                        &format!("No log entry found with id '{}'", id),
                    ),
                    log_store::GetResult::Expired => err_json(
                        "LOG_EXPIRED",
                        &format!("Log entry '{}' has expired (TTL exceeded)", id),
                    ),
                    log_store::GetResult::Found(_) => {
                        // Now handle pagination
                        let paginated = self
                            .log_store
                            .lock()
                            .ok()
                            .and_then(|s| s.get_paginated(id, p.limit, p.offset));

                        match paginated {
                            None => err_json(
                                "LOG_EXPIRED",
                                &format!("Log entry '{}' expired during retrieval", id),
                            ),
                            Some((result, has_more, total_count)) => {
                                if p.limit.is_some() {
                                    ok_json(serde_json::json!({
                                        "success": true,
                                        "log_id": id,
                                        "total_count": total_count,
                                        "offset": p.offset,
                                        "limit": p.limit,
                                        "has_more": has_more,
                                        "result": result,
                                    }))
                                } else {
                                    ok_json(serde_json::json!({
                                        "success": true,
                                        "log_id": id,
                                        "total_count": total_count,
                                        "result": result,
                                    }))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for IrisTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "iris-interop-dev".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ))
            .with_instructions(
                "iris-interop-dev: streamlined MCP tools for IRIS Interoperability development."
                    .to_string(),
            )
    }

    /// Override list_tools to rewrite JSON Schema 2020-12 nullable types to OpenAPI 3.0 anyOf.
    /// schemars + rmcp emit `"type": ["T", "null"]` which Google Vertex AI and Azure OpenAI
    /// reject. Rewrite to `"anyOf": [{"type": "T", ...siblings}, {"type": "null"}]`.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = self.tool_router.list_all();
        for tool in tools.iter_mut() {
            let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
            normalize_schema_openapi3(schema);
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }
}

/// Recursively rewrite JSON Schema 2020-12 nullable arrays to OpenAPI 3.0 anyOf.
///
/// schemars + rmcp emit `"type": ["integer", "null"]` (JSON Schema 2020-12) which
/// Google Vertex AI and Azure OpenAI reject. Rewrites to OpenAPI 3.0:
/// `"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}]`.
fn normalize_schema_openapi3(schema: &mut serde_json::Map<String, serde_json::Value>) {
    // Recurse into container schemas first (anyOf, allOf, oneOf, items)
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(arr) = schema.get_mut(key).and_then(|v| v.as_array_mut()) {
            for item in arr.iter_mut() {
                if let serde_json::Value::Object(obj) = item {
                    normalize_schema_openapi3(obj);
                }
            }
        }
    }
    if let Some(serde_json::Value::Object(obj)) = schema.get_mut("items") {
        normalize_schema_openapi3(obj);
    }

    // Recurse into properties: extract, fix, re-insert to avoid borrow conflicts
    if let Some(serde_json::Value::Object(mut props)) = schema.remove("properties") {
        let keys: Vec<String> = props.keys().cloned().collect();
        for k in keys {
            if let Some(serde_json::Value::Object(prop)) = props.get_mut(&k) {
                normalize_schema_openapi3(prop);
            }
        }
        schema.insert("properties".to_string(), serde_json::Value::Object(props));
    }

    // Now transform this level if it has a nullable type array
    let type_array = match schema.get("type") {
        Some(serde_json::Value::Array(arr)) if arr.iter().any(|v| v == "null") => arr.clone(),
        _ => return,
    };

    let non_null_types: Vec<serde_json::Value> = type_array
        .iter()
        .filter(|v| *v != "null")
        .cloned()
        .collect();
    schema.remove("type");

    // Move type-specific sibling fields into the non-null branch
    let type_specific = [
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "pattern",
        "enum",
        "const",
        "items",
        "minItems",
        "maxItems",
        "uniqueItems",
        "properties",
        "required",
        "additionalProperties",
    ];
    let mut type_branch: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for key in &type_specific {
        if let Some(val) = schema.remove(*key) {
            type_branch.insert(key.to_string(), val);
        }
    }
    let non_null_type = if non_null_types.len() == 1 {
        non_null_types.into_iter().next().unwrap()
    } else {
        serde_json::Value::Array(non_null_types)
    };
    type_branch.insert("type".to_string(), non_null_type);

    schema.insert(
        "anyOf".to_string(),
        serde_json::Value::Array(vec![
            serde_json::Value::Object(type_branch),
            serde_json::json!({"type": "null"}),
        ]),
    );
}

fn parse_iris_error_string(s: &str) -> Option<(String, i64)> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"<[A-Z]+>\s*[^+\s]+\+(\d+)\^([\w.%]+)").expect("valid regex")
    });
    let caps = re.captures(s)?;
    Some((caps[2].to_string(), caps[1].parse().ok()?))
}

fn parse_source_line(raw: &str) -> (Option<String>, Option<i64>) {
    if raw.is_empty() {
        return (None, None);
    }
    if let Some((cls, line)) = raw.split_once(':') {
        return (
            Some(cls.trim_end_matches(".cls").to_string()),
            line.trim().parse().ok(),
        );
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_port ──────────────────────────────────────────────────────────
    #[test]
    fn test_extract_port_standard() {
        assert_eq!(
            extract_port("0.0.0.0:52780->52773/tcp", "52773"),
            Some(52780)
        );
    }
    #[test]
    fn test_extract_port_superserver() {
        assert_eq!(extract_port("0.0.0.0:1974->1972/tcp", "1972"), Some(1974));
    }
    #[test]
    fn test_extract_port_not_present() {
        assert_eq!(extract_port("0.0.0.0:52780->52773/tcp", "1972"), None);
    }
    #[test]
    fn test_extract_port_multiple_mappings() {
        let ports = "0.0.0.0:1974->1972/tcp, 0.0.0.0:52775->52773/tcp";
        assert_eq!(extract_port(ports, "52773"), Some(52775));
        assert_eq!(extract_port(ports, "1972"), Some(1974));
    }
    #[test]
    fn test_extract_port_empty_string() {
        assert_eq!(extract_port("", "52773"), None);
    }

    // ── parse_iris_error_string ───────────────────────────────────────────────
    #[test]
    fn test_parse_iris_error_standard() {
        let s = "<UNDEFINED>x+3^Ens.Director.1";
        let result = parse_iris_error_string(s);
        assert_eq!(result, Some(("Ens.Director.1".to_string(), 3)));
    }
    #[test]
    fn test_parse_iris_error_divide() {
        let s = "<DIVIDE>x+1^MyApp.Foo.1";
        let result = parse_iris_error_string(s);
        assert_eq!(result, Some(("MyApp.Foo.1".to_string(), 1)));
    }
    #[test]
    fn test_parse_iris_error_no_match() {
        assert!(parse_iris_error_string("just a plain error").is_none());
        assert!(parse_iris_error_string("").is_none());
    }
    #[test]
    fn test_parse_iris_error_large_offset() {
        let s = "<ERROR>routine+99^Some.Class.INT";
        let result = parse_iris_error_string(s);
        assert_eq!(result, Some(("Some.Class.INT".to_string(), 99)));
    }

    // ── parse_source_line ─────────────────────────────────────────────────────
    #[test]
    fn test_parse_source_line_with_cls() {
        let (cls, line) = parse_source_line("MyApp.Foo.cls:42");
        assert_eq!(cls.as_deref(), Some("MyApp.Foo"));
        assert_eq!(line, Some(42));
    }
    #[test]
    fn test_parse_source_line_without_cls() {
        let (cls, line) = parse_source_line("MyApp.Foo:10");
        assert_eq!(cls.as_deref(), Some("MyApp.Foo"));
        assert_eq!(line, Some(10));
    }
    #[test]
    fn test_parse_source_line_empty() {
        let (cls, line) = parse_source_line("");
        assert!(cls.is_none());
        assert!(line.is_none());
    }
    #[test]
    fn test_parse_source_line_no_colon() {
        let (cls, line) = parse_source_line("NoColonHere");
        assert!(cls.is_none());
        assert!(line.is_none());
    }

    // ── translate_symbols_query ───────────────────────────────────────────────
    #[test]
    fn test_translate_bare_star_no_where() {
        let (sql, params) = translate_symbols_query(20, "*");
        assert!(!sql.contains("WHERE"), "bare * has no WHERE: {}", sql);
        assert!(params.is_empty());
    }
    #[test]
    fn test_translate_empty_no_where() {
        let (sql, params) = translate_symbols_query(20, "");
        assert!(!sql.contains("WHERE"), "empty has no WHERE: {}", sql);
        assert!(params.is_empty());
    }
    #[test]
    fn test_translate_glob_suffix() {
        let (sql, params) = translate_symbols_query(10, "HT.*");
        assert!(sql.contains("%STARTSWITH"));
        assert_eq!(params[0].as_str(), Some("HT."));
    }
    #[test]
    fn test_translate_trailing_dot() {
        let (sql, params) = translate_symbols_query(10, "Ens.");
        assert!(sql.contains("%STARTSWITH"));
        assert_eq!(params[0].as_str(), Some("Ens."));
    }
    #[test]
    fn test_translate_mid_glob() {
        let (sql, params) = translate_symbols_query(5, "A.*.B");
        assert!(sql.contains("LIKE"));
        let p = params[0].as_str().unwrap();
        assert_eq!(p, "A.%.B");
    }
    #[test]
    fn test_translate_plain_wraps_in_percent() {
        let (sql, params) = translate_symbols_query(20, "Patient");
        assert!(sql.contains("LIKE"));
        assert_eq!(params[0].as_str(), Some("%Patient%"));
    }
    #[test]
    fn test_translate_limit_in_sql() {
        let (sql, _) = translate_symbols_query(42, "Foo");
        assert!(sql.contains("42"), "limit must appear in SQL: {}", sql);
    }

    // ── sort_containers ───────────────────────────────────────────────────────
    #[test]
    fn test_sort_containers_by_score() {
        let containers = vec![
            serde_json::json!({"name":"z-iris","score":10}),
            serde_json::json!({"name":"a-iris","score":90}),
            serde_json::json!({"name":"m-iris","score":50}),
        ];
        let sorted = sort_containers(containers);
        assert_eq!(sorted[0]["name"].as_str(), Some("a-iris"));
        assert_eq!(sorted[1]["name"].as_str(), Some("m-iris"));
        assert_eq!(sorted[2]["name"].as_str(), Some("z-iris"));
    }
    #[test]
    fn test_sort_containers_tiebreak_by_name() {
        let containers = vec![
            serde_json::json!({"name":"z-iris","score":50}),
            serde_json::json!({"name":"a-iris","score":50}),
        ];
        let sorted = sort_containers(containers);
        assert_eq!(sorted[0]["name"].as_str(), Some("a-iris"));
    }
}

#[cfg(test)]
mod config_watcher_tests {
    use super::ConfigWatcher;
    #[test]
    fn test_config_watcher_detects_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".iris-agentic-dev.toml");

        // File does not exist yet — watcher created but last_mtime is None
        let mut watcher = ConfigWatcher::new(path.clone()).unwrap();
        assert!(
            watcher.last_mtime.is_none(),
            "mtime should be None before file exists"
        );
        assert!(!watcher.has_changed(), "no change if file still absent");

        // File appears
        std::fs::write(&path, "[connection]\nhost = \"localhost\"\n").unwrap();
        assert!(watcher.has_changed(), "should detect newly-created file");
        assert!(
            watcher.last_mtime.is_some(),
            "mtime should be set after detection"
        );
        assert!(
            !watcher.has_changed(),
            "no change on second check after detection"
        );
    }

    #[test]
    fn test_config_watcher_detects_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".iris-agentic-dev.toml");
        std::fs::write(&path, "[connection]\nhost = \"localhost\"\n").unwrap();

        let mut watcher = ConfigWatcher::new(path.clone()).unwrap();
        assert!(watcher.last_mtime.is_some());
        assert!(
            !watcher.has_changed(),
            "no change immediately after creation"
        );

        // Wind the stored mtime back by 2 seconds to simulate a future write being newer.
        if let Some(ref mut mtime) = watcher.last_mtime {
            *mtime = mtime
                .checked_sub(std::time::Duration::from_secs(2))
                .unwrap();
        }
        assert!(watcher.has_changed(), "should detect file with newer mtime");
    }

    #[test]
    fn test_config_watcher_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".iris-agentic-dev.toml");
        std::fs::write(&path, "[connection]\nhost = \"localhost\"\n").unwrap();

        let mut watcher = ConfigWatcher::new(path.clone()).unwrap();
        assert!(watcher.last_mtime.is_some());
        assert!(
            !watcher.has_changed(),
            "no spurious change for existing file"
        );
    }
}

#[cfg(test)]
mod schema_normalization_tests {
    use super::normalize_schema_openapi3;
    use super::DOCKER_REQUIRED_HINT;

    #[test]
    fn test_normalize_nullable_integer() {
        let mut schema = serde_json::json!({
            "type": ["integer", "null"],
            "format": "uint",
            "minimum": 0,
            "description": "Max entries"
        })
        .as_object()
        .unwrap()
        .clone();
        normalize_schema_openapi3(&mut schema);
        assert!(schema.get("type").is_none(), "type should be removed");
        let any_of = schema["anyOf"].as_array().unwrap();
        assert_eq!(any_of.len(), 2);
        assert_eq!(any_of[0]["type"], "integer");
        assert_eq!(any_of[0]["format"], "uint");
        assert_eq!(any_of[0]["minimum"], 0);
        assert_eq!(any_of[1]["type"], "null");
        assert_eq!(
            schema["description"], "Max entries",
            "description stays at top level"
        );
    }

    #[test]
    fn test_normalize_nullable_string() {
        let mut schema = serde_json::json!({
            "type": ["string", "null"],
            "description": "Optional string"
        })
        .as_object()
        .unwrap()
        .clone();
        normalize_schema_openapi3(&mut schema);
        let any_of = schema["anyOf"].as_array().unwrap();
        assert_eq!(any_of[0]["type"], "string");
        assert_eq!(any_of[1]["type"], "null");
    }

    #[test]
    fn test_normalize_nested_properties() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": ["integer", "null"],
                    "format": "uint",
                    "minimum": 0,
                    "description": "Max"
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();
        normalize_schema_openapi3(&mut schema);
        assert_eq!(schema["type"], "object", "top-level type unchanged");
        let limit = &schema["properties"]["limit"];
        assert!(limit.get("type").is_none());
        let any_of = limit["anyOf"].as_array().unwrap();
        assert_eq!(any_of[0]["type"], "integer");
        assert_eq!(any_of[0]["format"], "uint");
        assert_eq!(any_of[1]["type"], "null");
        assert_eq!(limit["description"], "Max");
    }

    #[test]
    fn test_normalize_non_nullable_unchanged() {
        let mut schema = serde_json::json!({
            "type": "integer",
            "format": "uint",
            "minimum": 0
        })
        .as_object()
        .unwrap()
        .clone();
        let original = schema.clone();
        normalize_schema_openapi3(&mut schema);
        assert_eq!(schema, original, "non-nullable schema should be unchanged");
    }

    // ── check_config field ordering ───────────────────────────────────────────
    #[test]
    fn check_config_connection_source_before_host() {
        // Verify connection_source appears before host in the JSON key order.
        // serde_json::json! preserves insertion order — this test guards that ordering.
        let sample = serde_json::json!({
            "connected": true,
            "connection_source": "http",
            "host": "localhost",
            "port": 52773_u16,
            "namespace": "USER",
            "container": serde_json::Value::Null,
            "config_file": serde_json::Value::Null,
            "config_loaded_at": serde_json::Value::Null,
            "iris_version": serde_json::Value::Null,
            "write_tools_enabled": true,
            "config_watch_path": serde_json::Value::Null,
        });
        let serialized = serde_json::to_string(&sample).unwrap();
        let conn_src_pos = serialized.find("connection_source").unwrap();
        let host_pos = serialized.find("\"host\"").unwrap();
        assert!(
            conn_src_pos < host_pos,
            "connection_source must appear before host in check_config output (got positions {conn_src_pos} vs {host_pos})"
        );
    }

    // ── DOCKER_REQUIRED remediation hint ─────────────────────────────────────
    #[test]
    fn docker_required_hint_contains_http_guidance() {
        assert!(
            DOCKER_REQUIRED_HINT.contains("http://"),
            "DOCKER_REQUIRED hint must reference HTTP URL pattern"
        );
        assert!(
            DOCKER_REQUIRED_HINT.contains(".iris-agentic-dev.toml"),
            "DOCKER_REQUIRED hint must reference the toml config file"
        );
        assert!(
            !DOCKER_REQUIRED_HINT.to_lowercase().contains("docker run"),
            "DOCKER_REQUIRED hint must not suggest 'docker run' (guides non-Docker users)"
        );
    }
}

#[cfg(test)]
mod compile_envelope_tests {
    use super::compile_failure;
    use rmcp::model::RawContent;

    fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
        let text = match &r.content[0].raw {
            RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        serde_json::from_str(text).unwrap()
    }

    /// Issue #46: a compile IRIS rejected came back as a success-shaped result —
    /// `success:false` with no `isError` — so clients read it as a working call.
    #[test]
    fn failed_compile_is_flagged_and_keeps_diagnostics() {
        let r = compile_failure(
            "MyApp.Broken.cls",
            serde_json::json!({
                "success": false,
                "target": "MyApp.Broken.cls",
                "targets_compiled": 1,
                "namespace": "USER",
                "errors": [{"severity":"error","text":"ERROR #1026: Invalid command"}],
                "warnings": [],
                "console": ["Detected 1 errors during compilation"],
            }),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true), "failed compile must set isError");
        let v = payload(&r);
        assert_eq!(v["success"], false);
        assert_eq!(v["error_code"], "COMPILE_ERROR");
        assert_eq!(v["error"], "ERROR #1026: Invalid command");
        assert!(
            v["console"].is_array(),
            "console must survive as detail: {v}"
        );
        assert_eq!(v["targets_compiled"], 1, "extras must survive: {v}");
        assert!(
            v["hint"].as_str().unwrap().contains("console"),
            "hint must name this payload's console field, not iris_doc's: {v}"
        );
    }

    #[test]
    fn failed_compile_without_parsed_errors_still_carries_a_message() {
        let r = compile_failure(
            "MyApp.Silent.cls",
            serde_json::json!({"success": false, "errors": [], "console": []}),
        )
        .unwrap();
        let v = payload(&r);
        assert_eq!(r.is_error, Some(true));
        assert!(
            v["error"].as_str().unwrap().contains("MyApp.Silent.cls"),
            "fallback message must name the target: {v}"
        );
    }
}

#[cfg(test)]
mod class_run_code_tests {
    use super::build_class_test_run_code;

    const FLAGS: &str = "/verbose=1/nodelete/noload";

    /// #66: a compiled %UnitTest.TestCase class runs through DebugRunTestCase, which takes
    /// the CLASS. Handing the same name to RunTest names an empty suite directory instead,
    /// which reports "All PASSED" having run nothing.
    #[test]
    fn a_testcase_class_runs_through_debugruntestcase() {
        let code = build_class_test_run_code("Admissions.Tests.MSG.AdmitNotice", FLAGS, "tok");
        assert!(
            code.contains(r#"DebugRunTestCase("",tCls,"/verbose=1/nodelete/noload","","tok")"#),
            "the class itself must be passed, not a suite: {code}"
        );
    }

    /// The TestProduction path is untouched: Run() starts the production, RunTest does not.
    #[test]
    fn a_testproduction_class_still_runs_through_run() {
        let code = build_class_test_run_code("Wk47.Tests.SmokeTest", FLAGS, "tok");
        assert!(
            code.contains(r#"PrimarySuper["%UnitTest.TestProduction""#),
            "{code}"
        );
        assert!(code.contains(r#"do $classmethod(tCls,"Run")"#), "{code}");
    }

    /// A pattern that is not a compiled class — the package prefix the tool description
    /// advertises — keeps the bare spec: there is no class to name after the suite.
    #[test]
    fn a_non_class_pattern_keeps_the_bare_spec() {
        let code = build_class_test_run_code("MyApp.Tests", FLAGS, "tok");
        assert!(
            code.contains(r#"RunTest("MyApp.Tests","/verbose=1/nodelete/noload","tok")"#),
            "{code}"
        );
    }

    /// A stale or invalid ^UnitTestRoot makes RunTest fail with "Directory ... is invalid"
    /// and report 0 tests, so the root and the spec directory must survive the rework (A6.2).
    #[test]
    fn the_spec_directory_is_still_created_under_a_fresh_root() {
        let code = build_class_test_run_code("MyApp.Tests.Thing", FLAGS, "tok");
        assert!(code.contains("set ^UnitTestRoot="), "{code}");
        assert!(code.contains("CreateDirectoryChain(specDir)"), "{code}");
    }

    /// The dispatch stays on ONE line: this code is piped through a docker-exec terminal as
    /// well as the HTTP executor, and an ObjectScript literal cannot span source lines.
    #[test]
    fn the_dispatch_stays_on_one_line() {
        let code = build_class_test_run_code("MyApp.Tests.Thing", FLAGS, "tok");
        let dispatch = code.lines().last().unwrap();
        assert!(dispatch.starts_with("if $isobject(tCC)"), "{dispatch}");
        assert!(dispatch.contains("elseif"), "{dispatch}");
        assert_eq!(
            dispatch.matches("DebugRunTestCase").count(),
            1,
            "the compiled-class branch runs the class directly: {dispatch}"
        );
    }
}

#[cfg(test)]
mod no_runnable_tests_tests {
    use super::{no_runnable_tests_cause, no_tests_found_guidance, TestClassShape};

    fn shape(super_chain: &str, production: &str, methods: u64) -> TestClassShape {
        TestClassShape {
            class: "Admissions.Tests.MSG.AdmitNotice".into(),
            primary_super: super_chain.into(),
            production: production.into(),
            own_test_methods: methods,
        }
    }

    const TESTPROD: &str = "~Admissions.Tests.MSG.AdmitNotice~%UnitTest.TestProduction~%UnitTest.TestCase~%Library.RegisteredObject~";
    const TESTCASE: &str =
        "~Admissions.Tests.MSG.AdmitNotice~%UnitTest.TestCase~%Library.RegisteredObject~";

    /// The verbatim #62 report: the only `did_you_mean` was the caller's own input, so
    /// following it re-sent the same call. A suggestion equal to the input is not one.
    #[test]
    fn did_you_mean_never_proposes_the_input_back() {
        let cands = vec!["Admissions.Tests.MSG.AdmitNotice".to_string()];
        let (hint, dym) =
            no_tests_found_guidance("Admissions.Tests.MSG.AdmitNotice", "EVALNS", &cands, false);
        assert!(
            dym.is_empty(),
            "the input itself is not a correction: {dym:?}"
        );
        assert!(
            !hint.contains("Did you mean") && !hint.contains("one segment away"),
            "nothing is one segment away from a byte-identical name: {hint}"
        );
    }

    /// Case differences are still a real correction — only an identical name is not.
    #[test]
    fn a_case_variant_is_still_offered() {
        let cands = vec!["Wk47.Tests.SmokeTest".to_string()];
        let (_, dym) = no_tests_found_guidance("wk47.tests.smoketest", "APP", &cands, false);
        assert!(
            dym.is_empty(),
            "an exact case-insensitive match is the input"
        );
        let cands = vec!["Wk47.Tests.SmokeTest".to_string()];
        let (_, dym) = no_tests_found_guidance("Wk47.Tests", "APP", &cands, false);
        assert_eq!(dym, vec!["Wk47.Tests.SmokeTest".to_string()]);
    }

    /// #66 made a plain %UnitTest.TestCase class runnable, so its superclass is no longer
    /// a cause. It must not be blamed — and neither must the pattern.
    #[test]
    fn a_plain_testcase_is_no_longer_blamed_on_its_superclass() {
        let (cause, hint) = no_runnable_tests_cause(&shape(TESTCASE, "", 4), "EVALNS");
        assert_eq!(cause, "UNKNOWN");
        assert!(
            !hint.contains("PRODUCTION = "),
            "PRODUCTION means nothing to a %UnitTest.TestCase class: {hint}"
        );
        assert!(hint.contains("pattern is not the problem"), "{hint}");
    }

    #[test]
    fn an_empty_production_parameter_is_named_as_the_cause() {
        let (cause, hint) = no_runnable_tests_cause(&shape(TESTPROD, "", 5), "EVALNS");
        assert_eq!(cause, "PRODUCTION_PARAMETER_EMPTY");
        assert!(hint.contains("PRODUCTION"), "{hint}");
    }

    #[test]
    fn a_class_with_no_test_methods_is_named_as_the_cause() {
        let (cause, hint) = no_runnable_tests_cause(&shape(TESTPROD, "APP.Production", 0), "APP");
        assert_eq!(cause, "NO_TEST_METHODS");
        assert!(hint.contains("Test"), "{hint}");
    }

    #[test]
    fn a_non_test_class_is_named_as_the_cause() {
        let (cause, _) = no_runnable_tests_cause(
            &shape(
                "~Admissions.MSG.AdmitNotice~Ens.Request~%Library.Persistent~",
                "",
                3,
            ),
            "APP",
        );
        assert_eq!(cause, "NOT_A_TEST_CLASS");
    }

    /// A well-formed class that still ran nothing gets an honest answer: not a pattern
    /// problem, and not a guess about the class either.
    #[test]
    fn a_well_formed_class_that_ran_nothing_is_not_blamed_on_the_pattern() {
        let (cause, hint) =
            no_runnable_tests_cause(&shape(TESTPROD, "Admissions.Production", 6), "EVALNS");
        assert_eq!(cause, "UNKNOWN");
        assert!(
            hint.contains("pattern is not the problem"),
            "the one thing we know for certain must still be said: {hint}"
        );
    }
}

#[cfg(test)]
mod no_tests_found_guidance_tests {
    use super::no_tests_found_guidance;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// The workshop's actual shape: pattern one segment short of the real class.
    #[test]
    fn near_miss_is_named_as_did_you_mean() {
        let cands = v(&[
            "Ejercicio3.Tests.BO.SQLTest",
            "Ejercicio3.Tests.DTL.MapTest",
            "Otro.Tests.Thing",
        ]);
        let (hint, dym) = no_tests_found_guidance("Ejercicio3.Test", "Ejercicio3", &cands, false);
        assert_eq!(
            dym,
            v(&[
                "Ejercicio3.Tests.BO.SQLTest",
                "Ejercicio3.Tests.DTL.MapTest"
            ]),
            "both near misses must be offered, the unrelated class must not"
        );
        assert!(hint.contains("Did you mean"), "{hint}");
        assert!(hint.contains("Ejercicio3.Tests.BO.SQLTest"), "{hint}");
    }

    /// A package prefix of a compiled class is a near miss too — the tool's own
    /// description advertises 'MyApp.Tests' as a pattern, so this must self-correct.
    #[test]
    fn package_prefix_of_a_test_class_is_a_near_miss() {
        let cands = v(&["Wk47.Tests.SmokeTest"]);
        let (hint, dym) = no_tests_found_guidance("Wk47.Tests", "APP", &cands, false);
        assert_eq!(dym, v(&["Wk47.Tests.SmokeTest"]));
        assert!(hint.contains("Did you mean"), "{hint}");
    }

    /// Cause (1) — nothing compiled — must read differently from cause (2), and must
    /// name the namespace it looked in, since a wrong namespace produces this too.
    #[test]
    fn empty_candidates_separates_not_compiled_from_wrong_pattern() {
        let (hint, dym) = no_tests_found_guidance("APP.Test", "USER", &[], false);
        assert!(dym.is_empty());
        assert!(hint.contains("No compiled test classes"), "{hint}");
        assert!(
            hint.contains("TestProduction"),
            "this fork's tests extend %UnitTest.TestProduction — the guidance must say so: {hint}"
        );
        assert!(
            hint.contains("USER") && hint.contains("different namespace"),
            "a wrong namespace produces this exact result, so say which one was searched: {hint}"
        );
    }

    /// Candidates exist but none resemble the pattern: list them, no false "did you mean".
    #[test]
    fn unrelated_candidates_are_listed_without_a_did_you_mean() {
        let cands = v(&["Foo.Tests.A", "Bar.Tests.B"]);
        let (hint, dym) = no_tests_found_guidance("Zzz.Nope", "APP", &cands, false);
        assert!(dym.is_empty(), "nothing here is a near miss");
        assert!(!hint.contains("Did you mean"), "{hint}");
        assert!(
            hint.contains("Foo.Tests.A") && hint.contains("Bar.Tests.B"),
            "{hint}"
        );
    }

    /// No silent caps: a truncated candidate list must say the count is a floor.
    #[test]
    fn truncated_candidate_list_is_reported_as_a_floor() {
        let cands: Vec<String> = (0..25).map(|i| format!("Pkg.Tests.T{i}")).collect();
        let (hint, _) = no_tests_found_guidance("Nope", "APP", &cands, true);
        assert!(
            hint.contains("25+"),
            "a full page means more exist — say so rather than implying 25 is all: {hint}"
        );
    }

    /// Case-insensitive: IRIS class names are case-sensitive but callers fumble case.
    #[test]
    fn near_miss_matching_ignores_case() {
        let cands = v(&["Wk47.Tests.SmokeTest"]);
        let (_, dym) = no_tests_found_guidance("wk47.tests", "APP", &cands, false);
        assert_eq!(dym, v(&["Wk47.Tests.SmokeTest"]));
    }
}
