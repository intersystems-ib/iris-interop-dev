use crate::elicitation::ElicitationStore;
use crate::iris::connection::IrisConnection;
use crate::objectscript::os_str_expr;

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

// #112: the advertised shape of `iris_production`. Schema only — the handler still reads
// the raw object, because the nine actions take overlapping subsets and a strict struct
// would reject valid calls. Every field below is one the handler actually reads; the
// per-action applicability is in each field's description, which is where a model looks.
//
// `//`, not `///`, deliberately: schemars promotes a struct doc comment to the schema's
// top-level `description`, which then ships on every tools/list. That is #82's rule.
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)] // read via the raw Value; this type exists to be the schema
pub struct ProductionDispatchSchema {
    /// REQUIRED. status=current state, start=start a production, stop=stop the running one,
    /// restart=recycle ONE config item (pass item), update=hot-apply config changes,
    /// check=report whether an update is needed, recover=recover a troubled production,
    /// get_autostart / set_autostart=read or set this namespace's autostart production.
    pub action: ProductionAction,
    /// Interop-enabled namespace. Omit to use the connection namespace (IRIS_NAMESPACE).
    #[serde(default)]
    pub namespace: Option<String>,
    /// start / stop / set_autostart: the production class, e.g. "MyPkg.MyProduction".
    /// `production_name` and `name` are accepted as spellings of this same field.
    #[serde(default)]
    pub production: Option<String>,
    /// restart: the config item to recycle. `component` is accepted for this field too.
    #[serde(default)]
    pub item: Option<String>,
    /// status: include per-item detail rather than the production summary alone.
    #[serde(default)]
    pub full: Option<bool>,
    /// start / stop / update: seconds to wait for the operation (default 30).
    #[serde(default)]
    pub timeout: Option<u32>,
    /// start / stop / update: force the operation past a busy or troubled state.
    #[serde(default)]
    pub force: Option<bool>,
    /// set_autostart: whether this namespace should autostart the production.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// The `action` discriminator, as a schema `enum` so a model picks rather than guesses.
///
/// The `JsonSchema` impl is hand-written for one reason: a derived enum is emitted as
/// `{"$ref": "#/$defs/ProductionAction"}`, and an MCP client that does not resolve `$ref`
/// sees a property with no enum at all — which is #112 again, one level down.
/// `inline_schema()` is what keeps the advertised inputSchema self-contained.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum ProductionAction {
    Status,
    Start,
    Stop,
    Restart,
    Update,
    Check,
    Recover,
    GetAutostart,
    SetAutostart,
}

impl JsonSchema for ProductionAction {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProductionAction".into()
    }
    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "enum": [
            "status", "start", "stop", "restart", "update",
            "check", "recover", "get_autostart", "set_autostart"
        ]})
    }
    fn inline_schema() -> bool {
        true
    }
}

// #112: the advertised shape of `iris_interop_query`. All 22 MISSING_WHAT errors in the
// campaign were calls of exactly `{}` — with `what` declared and required there is nothing
// left to omit. Schema only, for the same reason as ProductionDispatchSchema: the five
// `what` values take different subsets. `//` not `///` — see the note there.
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct InteropQueryDispatchSchema {
    /// REQUIRED. logs=Ens_Util.Log entries, queues=message queue depths,
    /// messages=the Ens.MessageHeader archive (optionally searching message CONTENT),
    /// trace=one whole session by session_id, partners=Ens.Config.BusinessPartner rows.
    pub what: InteropQueryWhat,
    /// Interop-enabled namespace. Omit to use the connection namespace (IRIS_NAMESPACE).
    #[serde(default)]
    pub namespace: Option<String>,
    /// logs: narrow to one config item. (`component` is this field's name on the wire.)
    #[serde(default)]
    pub component: Option<String>,
    /// logs: comma-separated severities to include. Default "error,warning".
    #[serde(default)]
    pub log_type: Option<String>,
    /// logs / messages / trace: narrow to one session. REQUIRED for what=trace.
    #[serde(default)]
    pub session_id: Option<i64>,
    /// logs / messages: return only rows after this id — tails without a MAX(ID) round trip.
    #[serde(default)]
    pub since_id: Option<i64>,
    /// logs / messages: row cap (default 50).
    #[serde(default)]
    pub limit: Option<u32>,
    /// messages: narrow to messages sent by this config item.
    #[serde(default)]
    pub source: Option<String>,
    /// messages: narrow to messages sent to this config item.
    #[serde(default)]
    pub target: Option<String>,
    /// messages: narrow to one message class.
    #[serde(default)]
    pub message_class: Option<String>,
    /// messages: join the body table of this message class server-side, so a WHERE can run
    /// against message CONTENT. The SQL table name is resolved for you.
    #[serde(default)]
    pub body_class: Option<String>,
    /// messages: SQL fragment applied to the body table named by body_class.
    #[serde(default)]
    pub body_where: Option<String>,
    /// messages: body columns to return alongside the header.
    #[serde(default)]
    pub body_select: Option<Vec<String>>,
    /// messages: search an indexed Search Table field instead of the body table —
    /// {prop, value | value_like, class?, extent?}. extent defaults to
    /// EnsLib.HL7.SearchTable; an error lists the searchable props.
    #[serde(default)]
    pub search_table: Option<serde_json::Value>,
}

/// The `what` discriminator, as a schema `enum`. Hand-written for the same reason as
/// [`ProductionAction`] — it must inline rather than `$ref`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum InteropQueryWhat {
    Logs,
    Queues,
    Messages,
    Trace,
    Partners,
}

impl JsonSchema for InteropQueryWhat {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "InteropQueryWhat".into()
    }
    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "enum": [
            "logs", "queues", "messages", "trace", "partners"
        ]})
    }
    fn inline_schema() -> bool {
        true
    }
}

/// Free-form JSON to the handler, `S`'s real schema on the wire.
///
/// #112: ten interop tools took [`AnyParams`], so `tools/list` advertised
/// `{"type":"object"}` for each — no properties, no `required`. A model had nothing to
/// work from but the prose description, and `{}` was a schema-valid call. Measured over a
/// 1121-call OpenCode campaign: the 11 schema-less tools took 31 parameter errors in 223
/// calls (13.9%), the 12 tools with real schemas took **zero** in 898. All 22
/// `MISSING_WHAT` calls sent exactly `{}`, across 8 runs.
///
/// The handlers already had the answer: each one builds a typed `…Params` struct that
/// derives `JsonSchema` and carries doc comments. The schema existed; the tool signature
/// simply did not point at it. This wrapper publishes `S`'s schema while still handing the
/// handler the raw `Value` it destructures today — so nothing about dispatch, defaults or
/// the permissive per-action shapes changes. A field the schema does not name is still
/// accepted, which is what keeps the dispatchers' unions working.
pub struct Described<S>(pub serde_json::Value, std::marker::PhantomData<S>);

impl<S> Described<S> {
    /// Wrap a raw object — for tests and any caller driving a handler directly.
    pub fn new(value: serde_json::Value) -> Self {
        Described(value, std::marker::PhantomData)
    }
}

impl<S> std::fmt::Debug for Described<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl<S> std::ops::Deref for Described<S> {
    type Target = serde_json::Value;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'de, S> Deserialize<'de> for Described<S> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Deliberately infallible over any JSON object: the schema advertises the shape,
        // the handler validates it and answers with a message that names the valid values.
        // Deserialising strictly here would replace MISSING_WHAT ("one of logs, queues,
        // messages, trace, partners") with rmcp's generic "missing field `what`".
        serde_json::Value::deserialize(d).map(|v| Described(v, std::marker::PhantomData))
    }
}

impl<S: JsonSchema> JsonSchema for Described<S> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        S::schema_name()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        S::schema_id()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        S::json_schema(generator)
    }
    fn inline_schema() -> bool {
        // The advertised inputSchema must be self-contained — a `$ref` into `$defs` is not
        // something every MCP client resolves.
        true
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
    /// 54 tools advertised (measured 2026-08-26 on v0.8.3). NOT this fork's default —
    /// `--toolset` defaults to `interop`; baseline is opt-in via IRIS_TOOLSET/--toolset.
    /// Note this is already a pruned router: the 58 tools the `#[tool_router]` macro
    /// registers minus the 4 merged-only ones.
    Baseline,
    /// 50 tools advertised (measured 2026-08-26). Baseline minus the 4 NOT_IMPLEMENTED
    /// stubs (skill_propose, skill_optimize, skill_share, skill_community_install).
    /// No merged dispatchers. Not this fork's default.
    Nostub,
    /// 46 tools advertised (measured 2026-08-26). Nostub (50) minus 8 — the 4 debug_*
    /// folded into iris_debug, the 3 container tools folded into iris_containers, and
    /// agent_info dropped outright — plus the 4 merged-only tools iris_debug,
    /// iris_containers, iris_admin, iris_get_log. 50 - 8 + 4 = 46.
    /// Not this fork's default.
    Merged,
    /// 23 tools advertised (measured 2026-08-26) — exactly `INTEROP_TOOLS`. THIS FORK'S
    /// DEFAULT: `--toolset` carries `default_value = "interop"` (see
    /// crates/iris-agentic-dev-bin/src/cmd/mcp.rs). Keeps only the tools the iris-interop
    /// skills actually exercise; everything else (skill_*/kb_*/agent_*/generate_*/
    /// individual debug_*/container/scm) is pruned. Two of the 23 (iris_production_item,
    /// iris_credential_manage) are write-gated off when the connection is not
    /// write-allowed, so a Live-mode server advertises 21.
    /// Additive: tool *code* is unchanged so upstream stays mergeable.
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

/// The interop-focused keep-list (Toolset::Interop). Source of truth for the `Interop`
/// pruning; `registered_tool_names()` now derives from the pruned router for every
/// toolset, so this list and the advertised surface cannot diverge. A unit test
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
                "'{class}' is a compiled test class but declares no Test* method of its own. \
                 %UnitTest discovers by NAME, not by method kind — `ClassMethod TestX()` is \
                 run too — but only an instance method can use `$$$Assert*`, so declare tests \
                 as `Method TestX()` and recompile. Watch the converse: a helper named Test* \
                 IS discovered and run with no arguments, and passes if it does not throw."
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
             OnBeforeAllTests/%OnNew for an early Quit, and read the run output with \
             iris_get_log.",
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
    // #136: the Test* count deliberately does NOT filter on ClassMethod. %UnitTest
    // discovers by NAME, not by method kind — verified live: a class whose only Test*
    // member is a ClassMethod runs it (total 1, passed 1). Counting instance methods
    // only made this probe disagree with the runner, so a class that really does have a
    // runnable test could be reported as NO_TEST_METHODS.
    let sql = "SELECT TOP 1 c.Name AS ClassName, c.PrimarySuper AS Supers, \
               (SELECT COUNT(*) FROM %Dictionary.CompiledMethod m WHERE m.parent = c.Name \
                AND m.Name %STARTSWITH 'Test' AND m.Origin = c.Name) AS OwnTestMethods, \
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
    // #67: one escaped expression, reused. ObjectScript doubles quotes; a backslash escapes
    // nothing, so the old `"{pattern}"` broke the moment a pattern contained one.
    let pattern = os_str_expr(pattern);
    format!(
        r#"set tIsWin=($zcvt($system.Version.GetOS(),"U")="WINDOWS")
set ^UnitTestRoot=$select(tIsWin:##class(%File).NormalizeDirectory("httest",##class(%File).GetDirectory(##class(%File).TempFilename())),1:"/tmp/httest/")
do ##class(%File).CreateDirectoryChain(^UnitTestRoot)
set specDir=##class(%File).NormalizeDirectory($translate({pattern},".","/"),^UnitTestRoot)
do ##class(%File).CreateDirectoryChain(specDir)
set tCls={pattern}
set tCC=##class(%Dictionary.CompiledClass).%OpenId(tCls)
if $isobject(tCC)&&(tCC.PrimarySuper["%UnitTest.TestProduction") {{ do $classmethod(tCls,"Run") }} elseif $isobject(tCC)&&(tCC.PrimarySuper["%UnitTest.TestCase") {{ do ##class(%UnitTest.Manager).DebugRunTestCase("",tCls,"{flags}","","{token}") }} else {{ do ##class(%UnitTest.Manager).RunTest({pattern},"{flags}","{token}") }}"#,
        token = token,
        pattern = pattern,
        flags = flags,
    )
}

pub const ERR_NAMESPACE_NOT_FOUND: &str = "NAMESPACE_NOT_FOUND";
pub const ERR_TEST_EXECUTION_ERROR: &str = "TEST_EXECUTION_ERROR";
/// #101/#102: IRIS answered, but `/api/atelier` is not published where this server looks —
/// the request never reached a namespace or a document. Distinct from `NOT_FOUND` (which is
/// a claim about a document) and from `IRIS_UNREACHABLE` (which is a claim about the host
/// and port, both of which just answered).
pub const ERR_ATELIER_NOT_FOUND: &str = "ATELIER_NOT_FOUND";
/// #102: a 404 whose cause could not be established. The honest answer when the only
/// alternative is a negative FACT the server cannot back — never a substitute for an error
/// a caller already has.
pub const ERR_INDETERMINATE: &str = "INDETERMINATE";

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
    /// #110: the connection the CLI flags / workspace config described, kept so a LATER
    /// re-probe targets the same instance instead of falling back to env-var discovery.
    /// `None` means discovery was env/docker-driven and a retry can rediscover from scratch.
    pub discovery_seed: Option<IrisConnection>,
    /// True while the startup discovery task is still running. Separates "the probe has
    /// not answered yet" from "nothing is configured" — two states that had one message.
    pub discovery_pending: bool,
    /// When the last lazy re-probe ran, so a genuinely absent IRIS is not probed on every
    /// single tool call.
    pub last_retry: Option<std::time::Instant>,
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
            discovery_seed: None,
            discovery_pending: false,
            last_retry: None,
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
            discovery_seed: None,
            discovery_pending: false,
            last_retry: None,
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
    // #145: also on the FOUND branch — a successful SELECT INTO left SQLCODE
    // undefined, so `If SQLCODE=0` threw exactly like the DML case.
    out.push_str(&sqlcode_epilogue(rs_var));

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
        "set {} = {}.%Execute({}){}\n",
        rs_var,
        rs_var,
        exec_args,
        sqlcode_epilogue(rs_var)
    ));
    out
}

/// #145: the `&sql` contract is not just "the statement runs" — it is that
/// `SQLCODE` and `%ROWCOUNT` are set afterwards. The translation ran the
/// statement and left both UNDEFINED, so the idiomatic
/// `&sql(INSERT ...) If SQLCODE<0 {...}` threw `<UNDEFINED>` *after the write
/// had already committed*, and the caller reasonably read that as a failed
/// write. Emitting the two variables under their real names means a read works
/// wherever it appears — same line, next line, ten lines later — and makes the
/// next-line rewrite below a belt-and-braces path rather than the only one.
///
/// Kept to ONE line: `execute_via_generator` maps submitted line N to
/// `RunUser+N`, and a multi-line expansion here would break that 1:1 (#124).
fn sqlcode_epilogue(rs_var: &str) -> String {
    // -400 is IRIS's "fatal error" SQLCODE — the honest answer when %ExecDirect
    // returned no result object at all, and never a value that reads as success.
    format!(
        " set SQLCODE=$Select($IsObject({rs}):{rs}.%SQLCODE,1:-400),%ROWCOUNT=$Select($IsObject({rs}):{rs}.%ROWCOUNT,1:0)"
    , rs = rs_var)
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
        "set {} = ##class(%SQL.Statement).%ExecDirect(, \"{}\"{}){}",
        rs_var,
        prepared_sql.replace('"', "\"\""),
        exec_args,
        sqlcode_epilogue(rs_var)
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
    /// Include members inherited from superclasses (default false — only what this class
    /// declares). Inherited %Persistent plumbing dominates the answer: Ens.Config.Production
    /// has 451 methods of which 70 are its own.
    #[serde(default)]
    pub include_inherited: bool,
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

// Issue #82: `log_id` is a DECLARED property, not just a serde alias.
//
// A `#[serde(alias)]` is invisible to `schemars`, so `log_id` never reached the advertised
// `inputSchema`. Strict/filtering clients (OpenAI strict function calling, the class of
// client #73 is about) can only emit properties the schema names, so the one key every
// truncating tool hands back (`log_id`, see `log_store::apply_truncation`) was unusable.
// Declaring it as a second field is the only thing that puts it in the generated schema
// while keeping the derived `JsonSchema` (descriptions, the nullable→anyOf rewrite, and
// the `additionalProperties: true` that `drop_default_additional_properties` strips).
//
// `Deserialize` is hand-written below rather than derived — see `GetLogIssue` (issue #81).
//
// Deliberately `//` and not `///`: schemars promotes a struct-level doc comment into the
// schema's top-level `description`, which is shipped to every client on every tools/list.
// As `///` this block put 761 characters of maintainer commentary — serde, schemars,
// private function names, issue numbers — on the wire, and made iris_get_log the only
// tool of the interop 23 carrying a top-level description at all. The rationale belongs
// in the source; the CALLER-facing text is the per-property docs below.
#[derive(Debug, Default, JsonSchema)]
pub struct GetLogParams {
    /// The log_id a previous truncated:true result returned. If omitted (and `log_id` is
    /// omitted too), lists the stored entries. `log_id` is the SAME parameter, declared
    /// separately so strict clients can emit it; pass either one, or the same
    /// value in both. A number is read as its decimal string.
    pub id: Option<String>,
    /// Identical to `id` — the name every truncating tool emits in its `log_id` field
    /// Pass either; passing both with DIFFERENT values is an error.
    pub log_id: Option<String>,
    /// Max entries to return. Must be > 0 if provided. Paginates BOTH forms: the stored
    /// result when an id is given, and the index listing when it is not.
    // The runtime rejects 0 with INVALID_PARAMS, so the schema has to reject it too:
    // `usize` alone advertises `minimum: 0`, and a client that validates against the
    // published schema then believes 0 is legal and only learns otherwise from an error.
    #[schemars(range(min = 1))]
    pub limit: Option<usize>,
    /// Start index. Default 0. Paginates both forms.
    // Advertised as nullable even though it is read as a plain `usize`, because that is
    // the whole point of #82: a strict function-calling client puts EVERY declared
    // property in `required` and sends `null` for the ones it is not using. `offset` was
    // the only declared property NOT nullable, so the exact payload
    // `{"id":null,"log_id":"x","limit":null,"offset":null}` — the one #81/#82 exist to
    // serve — violated the advertised schema on this one field. The hand-written
    // `Deserialize` has always read `null` here as "absent" (-> 0); this makes the schema
    // say so.
    #[serde(default)]
    #[schemars(with = "Option<usize>")]
    pub offset: usize,
    // Issue #78: every key serde did not recognise. Captured rather than dropped so a
    // mistyped addressing key (`logid`) can be NAMED instead of silently falling through
    // to the index listing, which is a different response shape. Not a caller-facing
    // parameter: schemars emits no property for a flattened map, only
    // `additionalProperties: true` (which `drop_default_additional_properties` removes
    // again on the wire).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    // Issue #81: what the deserializer coerced or could not use. Never a wire parameter —
    // `Deserialize` fills it, `get_log_impl` decides error vs warning.
    #[serde(skip)]
    #[schemars(skip)]
    pub issues: Vec<GetLogIssue>,
}

/// Issue #81: a parameter problem RECORDED rather than raised.
///
/// rmcp turns any `Deserialize` failure into a raw JSON-RPC -32602 frame before the handler
/// ever runs (`FromContextPart for Parameters<P>` maps every serde error to
/// `invalid_params`), and that frame carries no `error_code`, no `hint` and no
/// `valid_params` — it bypasses the issue-#2 envelope completely. Eight distinct payloads
/// escaped that way, including `{"id":null,"log_id":"x","limit":null,"offset":null}`, which
/// is exactly the shape a strict function-calling client produces once `log_id` is declared
/// (issue #82). So `GetLogParams` must NEVER fail to deserialize; what went wrong is
/// captured here and reported through the envelope instead. Same move issue #57 made for
/// connection failures.
#[derive(Debug, Clone, PartialEq)]
pub enum GetLogIssue {
    /// Read anyway, and said so: `{"log_id": 12345}` -> `"12345"`. Non-fatal.
    Coerced {
        param: &'static str,
        from: String,
        to: String,
    },
    /// Nothing sensible to be made of it. FATAL — but through the envelope. It must not
    /// silently become a default: a broken `id` falling through to the index listing is the
    /// wrong-shape failure issue #78 exists to prevent.
    WrongType {
        param: &'static str,
        expected: &'static str,
        got: String,
    },
    /// `arguments` was not a JSON object. Unreachable through rmcp (it always hands an
    /// object), reachable from a direct unit test — and must not panic there.
    NotAnObject { got: String },
}

impl GetLogIssue {
    /// The message for issues that must fail the call, or `None` for the tolerable ones.
    fn fatal_message(&self) -> Option<String> {
        match self {
            GetLogIssue::Coerced { .. } => None,
            GetLogIssue::WrongType {
                param,
                expected,
                got,
            } => Some(format!("`{param}` must be {expected}, but was {got}")),
            GetLogIssue::NotAnObject { got } => {
                Some(format!("parameters must be a JSON object, but were {got}"))
            }
        }
    }

    /// The non-fatal notice for issues the call recovered from, or `None`.
    fn warning(&self) -> Option<serde_json::Value> {
        match self {
            GetLogIssue::Coerced { param, from, to } => {
                let tail = if *param == "id" || *param == "log_id" {
                    "Log ids are strings — pass the value a truncated:true result returned, \
                     verbatim."
                } else {
                    "The advertised schema declares it as an integer — pass it as a number."
                };
                Some(serde_json::json!({
                    "code": "COERCED_PARAM",
                    "param": param,
                    "message": format!("`{param}` was sent as {from} and read as {to}. {tail}"),
                }))
            }
            _ => None,
        }
    }
}

/// Issue #81: name a JSON value's type the way an error message should read it.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Issue #81: one of the two addressing keys. `null` means absent — strict function-calling
/// clients put EVERY declared property in `required` and send `null` for the ones they are
/// not using, so `{"id":null,"log_id":"x"}` has to mean "log_id only".
///
/// A BLANK string is the same statement in a different dialect, and it has to mean the same
/// thing. Only `null` was treated as absent, so `{"id":"<valid>","log_id":""}` was a hard
/// `id`/`log_id` conflict while `{"id":"<valid>","log_id":null}` succeeded — the same
/// class of failure #81 fixed for `null`, one dialect later. `{"id":""}` alone now falls
/// back to the index listing instead of answering LOG_NOT_FOUND "with id ''", which named
/// an id nobody passed.
fn take_log_id(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &'static str,
    issues: &mut Vec<GetLogIssue>,
) -> Option<String> {
    match map.remove(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => None,
        Some(serde_json::Value::String(s)) => Some(s),
        // Log ids look numeric in other tools' output, so a bare number is a reasonable
        // thing for a client to send. Coerce it, and say so.
        Some(serde_json::Value::Number(n)) => {
            let to = n.to_string();
            issues.push(GetLogIssue::Coerced {
                param: key,
                from: format!("the number {n}"),
                to: format!("the string \"{to}\""),
            });
            Some(to)
        }
        Some(other) => {
            issues.push(GetLogIssue::WrongType {
                param: key,
                expected: "a string",
                got: json_type_name(&other).to_string(),
            });
            None
        }
    }
}

/// Issue #81: `limit` / `offset`. Mirrors `take_log_id`'s tolerance — a client that
/// stringifies its numbers is the same client that sends a numeric log id.
fn take_index(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &'static str,
    issues: &mut Vec<GetLogIssue>,
) -> Option<usize> {
    const EXPECTED: &str = "a non-negative integer";
    match map.remove(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => {
            match n.as_u64().and_then(|u| usize::try_from(u).ok()) {
                Some(u) => Some(u),
                None => {
                    issues.push(GetLogIssue::WrongType {
                        param: key,
                        expected: EXPECTED,
                        got: n.to_string(),
                    });
                    None
                }
            }
        }
        Some(serde_json::Value::String(s)) => match s.parse::<usize>() {
            Ok(u) => {
                issues.push(GetLogIssue::Coerced {
                    param: key,
                    from: format!("the string \"{s}\""),
                    to: format!("the number {u}"),
                });
                Some(u)
            }
            Err(_) => {
                issues.push(GetLogIssue::WrongType {
                    param: key,
                    expected: EXPECTED,
                    got: format!("\"{s}\""),
                });
                None
            }
        },
        Some(other) => {
            issues.push(GetLogIssue::WrongType {
                param: key,
                expected: EXPECTED,
                got: json_type_name(&other).to_string(),
            });
            None
        }
    }
}

impl<'de> serde::Deserialize<'de> for GetLogParams {
    /// Issue #81: infallible by construction. Every branch below records and continues; the
    /// only `?` is `Value::deserialize`, which cannot fail over serde_json's own
    /// `Deserializer` (and rmcp always hands us a `Value::Object`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let mut p = GetLogParams::default();
        let mut map = match v {
            serde_json::Value::Object(m) => m,
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                p.issues.push(GetLogIssue::NotAnObject {
                    got: json_type_name(&other).to_string(),
                });
                return Ok(p);
            }
        };
        p.id = take_log_id(&mut map, "id", &mut p.issues);
        p.log_id = take_log_id(&mut map, "log_id", &mut p.issues);
        p.limit = take_index(&mut map, "limit", &mut p.issues);
        p.offset = take_index(&mut map, "offset", &mut p.issues).unwrap_or(0);
        // Whatever is left is exactly what `#[serde(flatten)]` used to capture (issue #78).
        p.extra = map;
        Ok(p)
    }
}

/// Issue #78: the keys iris_get_log tolerates without acting on them.
///
/// Not leniency for its own sake. `namespace` is advertised by 11 of the 23 tools in
/// this fork's default (interop) profile — the only key that spans tool families — and
/// the agent harness sends it on nearly every call, including the correct index call in
/// the issue's own repro. It cannot mean anything here: the log store is a single
/// process-global ring buffer (`log_store::LogStore`) with no namespace dimension, so
/// ignoring it cannot hide a filter the caller asked for. Rejecting it would turn a call
/// that is correct today into a hard error.
const GET_LOG_IGNORED_PARAMS: &[&str] = &["namespace"];

/// The parameters iris_get_log actually reads — named in the error so the next call is a
/// correction, not another guess (same contract as `no_tests_found_guidance`, issue #47).
/// In schema-declaration order, so the error and the advertised `inputSchema` agree
/// (issue #82).
const GET_LOG_VALID_PARAMS: &[&str] = &["id", "log_id", "limit", "offset"];

/// Issue #82: what `logid` / `log-id` / `logId` were reaching for. `log_id` leads: it is
/// one character from every near miss, AND it is the name every truncating tool emits, so
/// a client that copies the suggestion straight back gets the key it already had in hand.
/// `id` follows because it is the shorter of the two identical parameters.
const GET_LOG_ID_SUGGESTIONS: &[&str] = &["log_id", "id"];

/// The two parameter lists every iris_get_log error and warning carries, always together.
///
/// `valid_params` alone said too little. `namespace` is accepted and ignored
/// (`GET_LOG_IGNORED_PARAMS`) — the hint has always said so in prose, but a client reading
/// `valid_params` programmatically saw a key that was not in it and concluded the call it
/// had just made successfully was invalid. Two payload fields, one per constant, written by
/// one function so the four surfaces that report parameters cannot drift apart.
///
/// They stay two fields rather than one merged list: `valid_params` is exactly the set of
/// DECLARED schema properties, which is what makes it agree with the advertised
/// `inputSchema` (issue #82). `namespace` is not a property of this tool and must not start
/// looking like one.
fn insert_get_log_param_lists(payload: &mut serde_json::Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "valid_params".to_string(),
            serde_json::json!(GET_LOG_VALID_PARAMS),
        );
        obj.insert(
            "accepted_and_ignored".to_string(),
            serde_json::json!(GET_LOG_IGNORED_PARAMS),
        );
    }
}

/// Issue #81/#84: one recovery hint, quoted by the fatal envelope, the unknown-key envelope
/// and the non-fatal warning alike — three surfaces that must not drift apart.
const GET_LOG_HINT: &str = "Pass `id` or `log_id` (the same parameter) — the value a \
    previous truncated:true result returned — to retrieve that result; limit/offset \
    paginate it (both the stored result and the index listing). Call iris_get_log with NO \
    parameters to list the stored entries. `namespace` is accepted and ignored (the store \
    is process-global).";

/// Issue #78: what an empty index must say. `{"logs":[]}` alone reads as "there is no
/// relevant log", which is the wrong and expensive conclusion the issue documents.
const EMPTY_LOG_INDEX_NOTE: &str = "Empty because no tool in THIS session has truncated \
    its output yet. This store holds only results a PREVIOUS tool marked truncated:true \
    and handed back a log_id for — it is NOT the IRIS event log. To read IRIS \
    interoperability logs use iris_interop_query with what='logs' (Ens_Util.Log entries; \
    filter with component / session_id / since_id), or what='trace' with session_id (one \
    message flow plus its Event Log events).";

/// Issue #78: leftover keys that must not be swallowed. Sorted, so the message is
/// deterministic. Keys beginning with `_` are skipped: those are client/protocol
/// extension markers (`_meta`), never tool parameters.
fn unknown_get_log_params(extra: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut keys: Vec<String> = extra
        .keys()
        .filter(|k| !k.starts_with('_'))
        .filter(|k| !GET_LOG_IGNORED_PARAMS.contains(&k.as_str()))
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Issue #78: `logid`, `logId`, `LOG-ID` all mean `log_id`, which iris_get_log reads as a
/// declared parameter (issue #82). Normalise away case and separators so the error can name
/// the fix.
///
/// `id` belongs in the list for the same reason `log_id` does, and it was missing: the
/// normaliser folds `ID`, `Id` and `id_` down to `id`, which was not a listed spelling, so
/// the near-miss of the SHORTER addressing key — the one a caller is most likely to fumble
/// the case of — got no `did_you_mean` while `log-id` did. Only a variant SPELLING can
/// reach this function: the exact keys `id` and `log_id` are consumed by the deserializer
/// and never land in `extra`.
fn is_log_id_near_miss(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        norm.as_str(),
        "id" | "ids" | "logid" | "logids" | "loguuid" | "entryid" | "logentryid"
    )
}

/// Issue #78: an unrecognised key with NO id present. The response shape is the whole
/// problem — answering with the index reads as "the log is empty" — so this is fatal.
fn unknown_params_error(unknown: &[String]) -> Result<CallToolResult, McpError> {
    let named = unknown
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut extra = serde_json::json!({
        "unknown_params": unknown,
        "hint": GET_LOG_HINT,
    });
    insert_get_log_param_lists(&mut extra);
    if unknown.iter().any(|k| is_log_id_near_miss(k)) {
        extra["did_you_mean"] = serde_json::json!(GET_LOG_ID_SUGGESTIONS);
    }
    envelope::fail_with(
        "INVALID_PARAMS",
        &format!(
            "iris_get_log: unknown parameter(s) {named}. This call did NOT list the \
             log index — an unrecognised parameter is an error here, not a different mode."
        ),
        extra,
    )
}

/// Issue #84: the same unrecognised key WITH an id present. The shape is unambiguous there
/// (the entry, or LOG_NOT_FOUND), so the call still answers — but the typo must not be
/// swallowed: the guard used to run only when `id` was absent, so `{"id":X,"logid":"…"}`
/// left no trace at all and the next call, which may have no id, repeats the mistake.
fn unknown_params_warning(unknown: &[String]) -> serde_json::Value {
    let named = unknown
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut w = serde_json::json!({
        "code": "UNKNOWN_PARAMS",
        "message": format!(
            "iris_get_log ignored unknown parameter(s) {named}. It answered the id you gave; \
             the next call may not have one, and then this is an error."
        ),
        "unknown_params": unknown,
    });
    insert_get_log_param_lists(&mut w);
    if unknown.iter().any(|k| is_log_id_near_miss(k)) {
        w["did_you_mean"] = serde_json::json!(GET_LOG_ID_SUGGESTIONS);
    }
    w
}

/// Issue #78: all of iris_get_log, as a free function over the store. The handler touches
/// no IRIS connection, so this is unit-testable with nothing but a `LogStore` — no
/// connection, no runtime. Mirrors `search::handle_iris_search`, which is already handed
/// the store rather than `&self`.
fn get_log_impl(
    store: &Arc<std::sync::Mutex<log_store::LogStore>>,
    p: GetLogParams,
) -> Result<CallToolResult, McpError> {
    let unknown = unknown_get_log_params(&p.extra);

    // ── 1. #81: reconcile the two addressing keys. Equal is exactly what a hedging agent
    //    sends, so accept it silently; different values are a real ambiguity.
    let (effective_id, conflict) = match (p.id.as_deref(), p.log_id.as_deref()) {
        (None, None) => (None, None),
        (Some(a), None) | (None, Some(a)) => (Some(a.to_string()), None),
        (Some(a), Some(b)) if a == b => (Some(a.to_string()), None),
        (Some(a), Some(b)) => (None, Some((a.to_string(), b.to_string()))),
    };

    // ── 2. #81: anything unusable is INVALID_PARAMS through the envelope, never a silent
    //    default — a broken `id` must NOT fall through to the index listing.
    let mut fatal: Vec<String> = p
        .issues
        .iter()
        .filter_map(GetLogIssue::fatal_message)
        .collect();
    if let Some((a, b)) = &conflict {
        fatal.push(format!(
            "`id` and `log_id` are the same parameter but were given different values \
             ('{a}' vs '{b}') — pass one, or the same value in both"
        ));
    }
    if !fatal.is_empty() {
        let mut extra = serde_json::json!({
            "hint": GET_LOG_HINT,
        });
        insert_get_log_param_lists(&mut extra);
        if let Some((a, b)) = &conflict {
            extra["id"] = serde_json::json!(a);
            extra["log_id"] = serde_json::json!(b);
        }
        if !unknown.is_empty() {
            extra["unknown_params"] = serde_json::json!(unknown);
        }
        return envelope::fail_with(
            "INVALID_PARAMS",
            &format!("iris_get_log: {}", fatal.join("; ")),
            extra,
        );
    }

    // ── 3. #83: `limit` now paginates BOTH forms, so it is validated for both. Unguarded,
    //    `{"limit":0}` on the paginated index would answer `{"logs":[]}` — a fresh instance
    //    of the "so there are no logs" failure issue #78 was filed for.
    if p.limit == Some(0) {
        // The envelope is the whole point of these errors (issue #2): this was the one
        // INVALID_PARAMS path in the tool that answered with a bare message, so the one
        // path that did not teach the caller the fix.
        let mut extra = serde_json::json!({ "hint": GET_LOG_HINT });
        insert_get_log_param_lists(&mut extra);
        return envelope::fail_with(
            "INVALID_PARAMS",
            "iris_get_log: limit must be > 0 if provided — it caps the page size; omit it \
             to return everything (the advertised schema says minimum 1)",
            extra,
        );
    }

    // ── 4. #84: the unknown-key guard runs UNCONDITIONALLY now. Without an id it stays
    //    fatal (the index is a different response shape); with one it becomes a warning on
    //    an otherwise successful answer, so nothing that works today starts failing.
    let mut warnings: Vec<serde_json::Value> = Vec::new();
    if !unknown.is_empty() {
        if effective_id.is_none() {
            return unknown_params_error(&unknown);
        }
        warnings.push(unknown_params_warning(&unknown));
    }
    warnings.extend(p.issues.iter().filter_map(GetLogIssue::warning));

    match effective_id {
        None => {
            // #83: the index paginates with the same limit/offset the entry form uses —
            // they were advertised and accepted here, but silently ignored.
            let (summaries, has_more, total) = store
                .lock()
                .map(|mut s| s.list_paginated(p.limit, p.offset))
                .unwrap_or((Vec::new(), false, 0));
            let mut out = serde_json::json!({
                "success": true,
                "logs": summaries,
                "total_count": total,
            });
            if p.limit.is_some() || p.offset > 0 {
                // The same block the entry form emits, on the same condition, so the two
                // forms read identically.
                out["offset"] = serde_json::json!(p.offset);
                out["limit"] = serde_json::json!(p.limit);
                out["has_more"] = serde_json::json!(has_more);
            }
            if total == 0 {
                // #78: an empty index must say what this store is and is NOT, or the agent
                // concludes "no logs exist" and stops looking. Keyed on the STORE being
                // empty, never on this PAGE being empty — otherwise pagination turns the
                // note into a lie.
                out["note"] = serde_json::Value::String(EMPTY_LOG_INDEX_NOTE.to_string());
            } else if out["logs"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
            {
                // #83: an offset past the end must not read as "no logs exist" either.
                out["note"] = serde_json::Value::String(format!(
                    "No entries at offset {}: the store holds {total}. Retry with a smaller \
                     offset.",
                    p.offset
                ));
            }
            if !warnings.is_empty() {
                out["warnings"] = serde_json::json!(warnings);
            }
            ok_json(out)
        }
        Some(ref id) => {
            // Check TTL / existence first
            let get_result = store
                .lock()
                .map(|s| s.get(id))
                .unwrap_or(log_store::GetResult::NotFound);

            // #84: a lookup that FAILS is exactly when the caller most needs to learn the
            // real parameter name, so the warning rides on the error envelope too. With no
            // warnings the error stays byte-identical to before.
            let fail = |code: &str, msg: String| -> Result<CallToolResult, McpError> {
                if warnings.is_empty() {
                    err_json(code, &msg)
                } else {
                    envelope::fail_with(code, &msg, serde_json::json!({"warnings": warnings}))
                }
            };

            match get_result {
                log_store::GetResult::NotFound => fail(
                    "LOG_NOT_FOUND",
                    format!("No log entry found with id '{id}'"),
                ),
                log_store::GetResult::Expired => fail(
                    "LOG_EXPIRED",
                    format!("Log entry '{id}' has expired (TTL exceeded)"),
                ),
                log_store::GetResult::Found(_) => {
                    // Now handle pagination
                    let paginated = store
                        .lock()
                        .ok()
                        .and_then(|s| s.get_paginated(id, p.limit, p.offset));

                    match paginated {
                        None => fail(
                            "LOG_EXPIRED",
                            format!("Log entry '{id}' expired during retrieval"),
                        ),
                        Some(page) => {
                            let total_count = page.total;
                            let mut out = serde_json::json!({
                                "success": true,
                                "log_id": id,
                                "total_count": total_count,
                                "result": page.result,
                            });
                            let asked_to_paginate = p.limit.is_some() || p.offset > 0;
                            if asked_to_paginate && page.sliced {
                                // #83: `offset` alone paginates too — it used to be accepted
                                // and dropped on the floor here, the same defect one branch over.
                                out["offset"] = serde_json::json!(p.offset);
                                out["limit"] = serde_json::json!(p.limit);
                                out["has_more"] = serde_json::json!(page.has_more);
                                if total_count > 0
                                    && out["result"]
                                        .as_array()
                                        .map(|a| a.is_empty())
                                        .unwrap_or(false)
                                {
                                    // #83: the index rescues an overshot offset with a note.
                                    // Without the same note here, an empty `result` reads as
                                    // "this entry is empty" — the wrong-conclusion failure
                                    // #78 exists to prevent, guarded on one branch only.
                                    out["note"] = serde_json::Value::String(format!(
                                        "No items at offset {}: this entry holds {total_count}. \
                                         Retry with a smaller offset.",
                                        p.offset
                                    ));
                                }
                            } else if asked_to_paginate {
                                // #83: the stored result is a single JSON object, not a list,
                                // so nothing was sliced — `iris_test` stores
                                // `{test_suites, raw_output}`, and it is the tool this store's
                                // own hint sends the agent to. Echoing offset/limit/has_more
                                // here would ASSERT a page that was never taken: `offset:99`
                                // answering `has_more:false` over a complete payload says "you
                                // have reached the end" when nothing was skipped. Say what
                                // actually happened instead.
                                out["pagination_applied"] = serde_json::Value::Bool(false);
                                out["note"] = serde_json::Value::String(format!(
                                    "`limit`/`offset` were ignored: {} stored this result as a \
                                     single JSON object, not a list, so there is nothing to \
                                     slice — the WHOLE result is returned and nothing was \
                                     skipped. Page inside `result` yourself.",
                                    page.tool
                                ));
                            }
                            if !warnings.is_empty() {
                                out["warnings"] = serde_json::json!(warnings);
                            }
                            ok_json(out)
                        }
                    }
                }
            }
        }
    }
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
            // #166: no zeroed counters on a failure. `success:false` + `completed:false` +
            // `outcome:"no_tests"` already say what happened; `failed: 0` beside them reads
            // as a passing run to anything checking the count rather than the code.
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

/// #110: "discovery has not answered yet" and "nothing is configured" had one message,
/// and it only described the second. A user with IRIS_HOST set correctly was told to set
/// IRIS_HOST — so the report reads as "the MCP tools are broken", and the actual cause
/// (a probe that missed its window on a loaded machine) is nowhere in the text.
fn iris_unreachable_detail(pending: bool, configured: bool) -> McpError {
    let msg = if pending {
        "IRIS_UNREACHABLE: IRIS discovery is still running — the startup probe has not \
         answered yet. This is usually a slow container start or a loaded machine, not a \
         configuration problem. Retry this call in a few seconds; the connection is adopted \
         as soon as the probe completes, and every tool call re-probes after a cooldown."
    } else if configured {
        "IRIS_UNREACHABLE: IRIS is configured (IRIS_HOST / IRIS_CONTAINER is set) but the \
         probe did not reach it. Check that the instance is up and the web port is right — \
         `curl <host>:<port>/api/atelier/` should return JSON. call check_config to see the \
         host, port, namespace and user this server is actually using. The probe is retried \
         on later tool calls, so a session started before IRIS was ready recovers on its own."
    } else {
        "IRIS_UNREACHABLE: no IRIS connection. Set IRIS_HOST and IRIS_WEB_PORT env vars, or \
         ensure IRIS is reachable on a discoverable port (52773, 41773, 51773, 8080)."
    };
    McpError::invalid_request(msg.to_string(), None)
}

/// Whether the environment names an IRIS to connect to at all.
fn iris_is_configured() -> bool {
    ["IRIS_HOST", "IRIS_CONTAINER"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// How long to wait before a disconnected session re-probes. Short enough that a session
/// started 20 seconds before its container heals on its own; long enough that a machine
/// with no IRIS at all is not probed on every tool call.
fn discovery_retry_cooldown() -> std::time::Duration {
    let secs = std::env::var("IRIS_DISCOVERY_RETRY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15);
    std::time::Duration::from_secs(secs)
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

/// Rows in an Atelier query body, or 0 when there are none.
fn row_count(body: &serde_json::Value) -> usize {
    body["result"]["content"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0)
}

/// #107: what the class dictionary says about a class that introspected to nothing.
/// The three states need three different actions, and `Undetermined` is deliberately one
/// of them — a failed existence check must not become a claim in either direction.
#[derive(Debug, PartialEq)]
enum ClassPresence {
    Compiled,
    /// The .cls exists but has never been compiled, so the Compiled* tables are empty for
    /// it. Reads identically to "absent" through %Dictionary.CompiledMethod alone.
    DefinedNotCompiled,
    Absent,
    /// The check itself failed. Say nothing rather than guess.
    Undetermined,
}

/// One round trip for both dictionary tables — this runs only when introspection came back
/// empty, so the happy path is untouched.
async fn class_presence(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    class_name: &str,
) -> ClassPresence {
    let sql = "SELECT 1 AS IsCompiled FROM %Dictionary.CompiledClass WHERE Name = ? \
               UNION ALL \
               SELECT 0 AS IsCompiled FROM %Dictionary.ClassDefinition WHERE Name = ?";
    let params = vec![
        serde_json::Value::String(class_name.to_string()),
        serde_json::Value::String(class_name.to_string()),
    ];
    let body = match iris.query(sql, params, namespace, client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("class_presence check failed for {class_name}: {e}");
            return ClassPresence::Undetermined;
        }
    };
    let rows = match body["result"]["content"].as_array() {
        Some(r) => r,
        None => return ClassPresence::Undetermined,
    };
    if rows.is_empty() {
        return ClassPresence::Absent;
    }
    // IRIS returns the column as a number or a numeric string depending on the driver path.
    let compiled = rows.iter().any(|r| {
        let v = &r["IsCompiled"];
        v.as_i64() == Some(1) || v.as_str() == Some("1")
    });
    if compiled {
        ClassPresence::Compiled
    } else {
        ClassPresence::DefinedNotCompiled
    }
}

/// Names one segment away from `class_name`, so the caller's next call is a correction
/// rather than another guess — the #62 treatment `iris_test` already gives NO_TESTS_FOUND.
/// Searches the parent package, which is where a typo's real class almost always lives.
/// Returns `(suggestions, classes_in_package)`.
async fn near_miss_classes(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    class_name: &str,
) -> (Vec<String>, usize) {
    let prefix = match class_name.rfind('.') {
        Some(i) => &class_name[..=i],
        None => class_name,
    };
    let body = match iris
        .query(
            "SELECT TOP 200 Name FROM %Dictionary.CompiledClass WHERE Name %STARTSWITH ? ORDER BY Name",
            vec![serde_json::Value::String(prefix.to_string())],
            namespace,
            client,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("near-miss lookup failed for {class_name}: {e}");
            return (vec![], 0);
        }
    };
    let all: Vec<String> = body["result"]["content"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["Name"].as_str().map(str::to_string))
                .filter(|c| !c.eq_ignore_ascii_case(class_name))
                .collect()
        })
        .unwrap_or_default();
    (rank_near_misses(class_name, &all), all.len())
}

/// Which of `candidates` are plausibly the class the caller MEANT.
///
/// The whole package is not an answer — "did you mean one of these 20?" is the same guess
/// the caller already made. Two things count as a near miss: a full-name prefix relation
/// in either direction (`A.B` for `A.B.C`, the #62 rule `iris_test` uses), and a shared
/// opening on the FINAL segment, which is where typos land (`Ens.Util.LogXX` → `Ens.Util.Log`).
/// Three characters is the floor — below that every class in the package "matches".
fn rank_near_misses(class_name: &str, candidates: &[String]) -> Vec<String> {
    fn last_segment(n: &str) -> &str {
        n.rsplit('.').next().unwrap_or(n)
    }
    fn shared_prefix_len(a: &str, b: &str) -> usize {
        a.chars()
            .zip(b.chars())
            .take_while(|(x, y)| x.eq_ignore_ascii_case(y))
            .count()
    }
    let lower = class_name.to_lowercase();
    let target_seg = last_segment(class_name).to_lowercase();

    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter_map(|c| {
            let cl = c.to_lowercase();
            if cl.starts_with(&lower) || lower.starts_with(&cl) {
                // A whole-name prefix relation is the strongest signal there is.
                return Some((usize::MAX, c));
            }
            let n = shared_prefix_len(last_segment(&cl), &target_seg);
            (n >= 3).then_some((n, c))
        })
        .collect();
    // Longest shared opening first; name order breaks ties so the output is deterministic.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(5).map(|(_, c)| c.clone()).collect()
}

/// `%Foo` is ObjectScript shorthand for `%Library.Foo` (#157).
///
/// `%Dictionary.CompiledClass` stores only the expanded name, so a lookup by the abbreviation
/// misses and the class reads as absent — `docs_introspect("%File")` answered CLASS_NOT_FOUND
/// while `%Library.File` introspected 74 methods. Expanding up front cannot lose a match:
/// verified on 2026.1 that NO stored class name begins with `%` and contains no `.`
/// (0 rows), while `%Library.*` holds 212, so the bare form can only ever be the shorthand.
///
/// Returns `None` for anything already qualified, so `%Library.File` and `Ens.Director` are
/// untouched.
pub fn expand_percent_class(name: &str) -> Option<String> {
    let rest = name.strip_prefix('%')?;
    (!rest.is_empty() && !rest.contains('.')).then(|| format!("%Library.{rest}"))
}

pub const ERR_CLASS_NOT_FOUND: &str = "CLASS_NOT_FOUND";

/// The #107 envelope. Kept out of `docs_introspect` so the message is testable without a
/// connection.
fn class_not_found_error(
    class_name: &str,
    namespace: &str,
    candidates: &[String],
    in_package: usize,
    requested: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let mut extra = serde_json::json!({
        "class_name": class_name,
        "namespace": namespace,
        "classes_in_package": in_package,
    });
    // #157: when the name was expanded, the caller asked about `%Foo` and is being told
    // `%Library.Foo` is absent. Name both, or the answer is about a string they never sent.
    if let Some(r) = requested {
        extra["requested_class_name"] = serde_json::Value::String(r.to_string());
        extra["resolved"] =
            serde_json::Value::String(format!("'{r}' is shorthand for '{class_name}'"));
    }
    let absent = format!(
        "Class '{class_name}' does not exist in namespace '{namespace}' — it is in neither \
         %Dictionary.CompiledClass nor %Dictionary.ClassDefinition. Nothing was introspected. \
         (An empty methods/properties list would have meant a class that exists and has no \
         members, which is a different fact.)"
    );
    let msg = if !candidates.is_empty() {
        extra["did_you_mean"] = serde_json::json!(candidates);
        format!(
            "Class '{class_name}' does not exist in namespace '{namespace}'. Did you mean: {}? \
             Nothing was introspected.",
            candidates.join(", ")
        )
    } else if in_package > 0 {
        // The package is real and the class is not — worth saying, because it rules out
        // "wrong namespace" without listing every class in it.
        format!(
            "{absent} Its package does exist here and holds {in_package} compiled class(es), \
             none of them a near match."
        )
    } else {
        absent
    };
    extra["hint"] = serde_json::json!(
        "Check the name with iris_doc(mode='head', name='<class>.cls'), or compile the class \
         first if it has not been compiled in this namespace."
    );
    envelope::fail_with(ERR_CLASS_NOT_FOUND, &msg, extra)
}

/// Issue #102: the failure envelope `docs_introspect` never had. Names the namespace when a
/// 404 is its fault (#93), otherwise codes the failure from the status IRIS actually returned
/// — never `null` fields on a `success:true` answer.
async fn introspect_failure(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    class_name: &str,
    e: &anyhow::Error,
) -> Result<CallToolResult, McpError> {
    if let Some(missing) = interop::namespace_missing_error_for(
        iris,
        client,
        namespace,
        "Nothing was introspected.",
        e,
    )
    .await
    {
        return missing;
    }
    let msg = e.to_string();
    let code = match crate::iris::connection::atelier_status(e) {
        Some(http) => envelope::http_status_code(http.status),
        None => interop::classify_iris_error_or(&msg, "IRIS_REQUEST_FAILED"),
    };
    envelope::fail_with(
        code,
        &msg,
        serde_json::json!({ "class_name": class_name, "namespace": namespace }),
    )
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

/// Turn the wrapper's opaque frame into something the caller can act on (#124).
///
/// Adds the caller's OWN failing line (an IRIS terminal would have echoed it with a caret) and,
/// when the trap names a member on a class, that class's declared members. Both come from
/// information the server already holds at the moment it builds the error.
async fn enrich_abort(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    abort: &str,
    submitted: &str,
    resp: &mut serde_json::Value,
) {
    let Some(frame) = parse_abort_frame(abort) else {
        return;
    };
    resp["signal"] = serde_json::Value::String(frame.signal.to_string());

    if let Some(n) = frame.line {
        if let Some(text) = submitted.lines().nth(n - 1) {
            resp["source_line_number"] = serde_json::Value::from(n);
            resp["source_line"] = serde_json::Value::String(text.trim_end().to_string());
        }
    }

    let mut hint = match (resp.get("source_line").and_then(|v| v.as_str()), frame.line) {
        (Some(line), Some(n)) => format!("Line {n} of the code you sent is what failed: {line}"),
        _ => String::new(),
    };

    if abort_wants_member_list(frame.signal) {
        if let (Some(member), Some(class)) = (frame.member, frame.class) {
            let (members, member_kind) =
                declared_members(iris, client, namespace, class, Some(member)).await;

            // #178: the name the caller wrote DOES exist on the class — under the other
            // kind. No list of other names helps here; the syntax is the bug.
            if let Some(kind) = member_kind {
                if let Some(mismatch) = member_kind_mismatch_hint(class, member, frame.signal, kind)
                {
                    if !hint.is_empty() {
                        hint.push(' ');
                    }
                    hint.push_str(&mismatch);
                    resp["hint"] = serde_json::Value::String(hint);
                    return;
                }
            }

            // #62, applied here at last: a suggestion identical to the input is not a
            // correction — following it re-sends the same call. The guard has existed in
            // `no_tests_found_guidance` since #62 and this second producer of the same
            // field never came under it, so `did_you_mean[0]` was routinely the exact
            // identifier that had just failed.
            let members: Vec<String> = members
                .into_iter()
                .filter(|m| !m.eq_ignore_ascii_case(member))
                .collect();

            if !members.is_empty() {
                resp["did_you_mean"] = members
                    .iter()
                    .take(8)
                    .map(|m| serde_json::Value::String(m.clone()))
                    .collect();
                if !hint.is_empty() {
                    hint.push(' ');
                }
                hint.push_str(&format!(
                    "'{class}' has no '{member}'. It declares: {}. \
                     (docs_introspect(class_name='{class}') lists all of them.)",
                    members
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    if !hint.is_empty() && resp.get("hint").is_none() {
        resp["hint"] = serde_json::Value::String(hint);
    }
}

/// Which kind of member a name is declared as. #178: the two are not interchangeable at the
/// call site, and IRIS reports the mismatch as "does not exist", which is true of the syntax
/// and false of the class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemberKind {
    Method,
    Property,
}

/// #178: the caller's name EXISTS on the class, under the other kind.
///
/// The verbatim report: `write $G(s.GetValueAt("1"))` answered
/// `'EnsLib.HL7.Segment' has no 'GetValueAt'. It declares: GetValueAt, ...` — a sentence that
/// denies a name and then lists it first — with `did_you_mean[0]` set to the identifier that
/// had just failed, so an agent following the suggestion re-sent the identical call.
///
/// The cause is that `$GET()` forces PROPERTY syntax, so a method call inside it resolves as a
/// property and IRIS reports `<PROPERTY DOES NOT EXIST>`. The class is fine; the wrapper is the
/// bug, and no list of other member names can say that. Returns `None` when the kind matches
/// the syntax the caller used — then the name genuinely is not there and the list is the answer.
fn member_kind_mismatch_hint(
    class: &str,
    member: &str,
    signal: &str,
    declared_as: MemberKind,
) -> Option<String> {
    match (signal, declared_as) {
        ("PROPERTY DOES NOT EXIST" | "CLASS PROPERTY", MemberKind::Method) => Some(format!(
            "'{member}' exists on '{class}' as a METHOD, not a property. Property syntax cannot \
             reach it — and $GET() forces property syntax, so `$GET(obj.{member}(...))` fails \
             even where `obj.{member}(...)` works. Call it directly; if you were guarding \
             against an undefined value, guard the OBJECT ($IsObject) rather than the call."
        )),
        ("METHOD DOES NOT EXIST", MemberKind::Property) => Some(format!(
            "'{member}' exists on '{class}' as a PROPERTY, not a method. Read it as `obj.{member}` \
             on an INSTANCE, with no parentheses — `##class({class}).{member}()` is a class-method \
             call and cannot reach an instance property."
        )),
        _ => None,
    }
}

/// The members a class DECLARES (not inherited, not `%`-prefixed), ranked against the name the
/// caller got wrong (#124). Runs only on the error path, so a working call never pays for it.
///
/// #178: also reports which kind the caller's own `wanted` name is declared as, when it is
/// declared at all — read from the same round-trip, never inferred.
async fn declared_members(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
    class: &str,
    wanted: Option<&str>,
) -> (Vec<String>, Option<MemberKind>) {
    let sql = "SELECT Name, 'M' AS Kind FROM %Dictionary.CompiledMethod \
               WHERE parent = ? AND Origin = parent AND SUBSTRING(Name,1,1) <> '%' \
               UNION \
               SELECT Name, 'P' AS Kind FROM %Dictionary.CompiledProperty \
               WHERE parent = ? AND Origin = parent AND SUBSTRING(Name,1,1) <> '%'";
    let rows = match iris
        .query(
            sql,
            vec![
                serde_json::Value::String(class.to_string()),
                serde_json::Value::String(class.to_string()),
            ],
            namespace,
            client,
        )
        .await
    {
        Ok(v) => v,
        Err(_) => return (Vec::new(), None),
    };
    // `iris.query` answers `{"result":{"content":[…]}}` — the same shape `row_count` reads.
    let empty = Vec::new();
    let rows = rows["result"]["content"].as_array().unwrap_or(&empty);

    // #178: the kind of the caller's OWN name, if the class declares it at all.
    let wanted_kind = wanted.and_then(|w| {
        rows.iter().find_map(|r| {
            let name = r.get("Name").and_then(|n| n.as_str())?;
            if !name.eq_ignore_ascii_case(w) {
                return None;
            }
            match r.get("Kind").and_then(|k| k.as_str()) {
                Some("M") => Some(MemberKind::Method),
                Some("P") => Some(MemberKind::Property),
                _ => None,
            }
        })
    });

    let mut names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("Name").and_then(|n| n.as_str()))
        .map(str::to_string)
        .collect();

    // Nearest first: the caller is looking for one name.
    match wanted {
        Some(w) => rank_members(&mut names, w),
        None => names.sort(),
    }
    names.truncate(25);
    (names, wanted_kind)
}

/// Split a CamelCase identifier into lowercase words: `ValidateProduction` -> `[validate, production]`.
fn camel_words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if ch.is_ascii_alphanumeric() {
            cur.push(ch.to_ascii_lowercase());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Rank a class's members against the name the caller got wrong (#124).
///
/// A leading-prefix score alone is useless here: nothing in `Ens.Director` starts with a `V`, so
/// `ValidateProduction` scored zero against every member and the caller was offered `Console`.
/// What the caller wants shares a WORD — `StartProduction`, `StopProduction`,
/// `GetProductionStatus`. Words of three characters or fewer (`get`, `is`, `on`) are skipped:
/// they match everything and therefore rank nothing.
fn rank_members(names: &mut [String], wanted: &str) {
    let words: Vec<String> = camel_words(wanted)
        .into_iter()
        .filter(|w| w.len() > 3)
        .collect();
    let want_lower = wanted.to_ascii_lowercase();
    let want_words = camel_words(wanted).len();
    names.sort_by_cached_key(|n| {
        let lower = n.to_ascii_lowercase();
        let shared_words = words.iter().filter(|w| lower.contains(w.as_str())).count();
        let prefix = lower
            .chars()
            .zip(want_lower.chars())
            .take_while(|(a, b)| a == b)
            .count();
        // Among members sharing the same word, the one SHAPED like what the caller wrote is
        // the better guess: `ValidateProduction` wants `StartProduction`, not
        // `actualizeProductionDifferences`. Word count first, then length.
        let shape = camel_words(n).len().abs_diff(want_words);
        let length = lower.len().abs_diff(want_lower.len());
        (
            std::cmp::Reverse(shared_words),
            std::cmp::Reverse(prefix),
            shape,
            length,
            lower,
        )
    });
}

/// What the wrapper's abort line actually names (#124).
///
/// `ERROR: <METHOD DOES NOT EXIST> 148 RunUser+3^IrisDevTmp.Run32b6783ae37d.1 ValidateProduction,Ens.Director`
///
/// `RunUser+3` is a location in a routine the caller never submitted, never sees and cannot
/// fetch, and the hash changes every call — so two identical errors did not even look
/// identical. But the server wrote that routine: `RunUser+N` is submitted line N (verified on
/// 2026.1 against a script whose third line trapped), and the trailing `Member,Class` pair is
/// already parsed out for us by IRIS.
#[derive(Debug, PartialEq)]
pub struct AbortFrame<'a> {
    pub signal: &'a str,
    pub line: Option<usize>,
    pub member: Option<&'a str>,
    pub class: Option<&'a str>,
}

pub fn parse_abort_frame(abort: &str) -> Option<AbortFrame<'_>> {
    let open = abort.find('<')?;
    let close = abort[open..].find('>')? + open;
    let signal = abort[open + 1..close].trim();
    let rest = &abort[close + 1..];

    // `RunUser+N^Routine` — N is the submitted line, 1-based.
    let line = rest.find("RunUser+").and_then(|i| {
        rest[i + "RunUser+".len()..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|d| d.parse::<usize>().ok())
    });

    // Trailing detail after the routine reference, e.g. "ValidateProduction,Ens.Director".
    let tail = rest
        .split('^')
        .nth(1)
        .and_then(|after| after.split_whitespace().nth(1))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let (member, class) = match tail.and_then(|t| t.split_once(',')) {
        Some((m, c)) => (Some(m.trim()), Some(c.trim())),
        None => (tail, None),
    };

    Some(AbortFrame {
        signal,
        line,
        member,
        class,
    })
}

/// The signals for which naming the class's declared members answers the question (#124).
///
/// Three quarters of the opaque frames in the eval corpus are a name the agent got wrong —
/// a method, class or property that does not exist. 54% of affected runs never called an
/// introspection tool afterwards and 70% of missing symbols were never retried: agents do not
/// recover from these, they abandon the line of enquiry. So the error has to carry the answer.
fn abort_wants_member_list(signal: &str) -> bool {
    matches!(
        signal,
        "METHOD DOES NOT EXIST" | "PROPERTY DOES NOT EXIST" | "CLASS PROPERTY"
    )
}

/// The wrapper's own abort rendering, wherever it lands in the captured output (#123).
///
/// `iris_execute` used to decide success with `trimmed.starts_with("ERROR: ")`, which made the
/// flag a function of whether the script had written anything BEFORE the trap fired. The same
/// `<SYNTAX>` abort came back `success:false` when it hit line 1 and `success:true` when a
/// `Write` landed first — 352 of 720 abort-carrying envelopes in the eval corpus were reported
/// as successes, every one of them with no `error_code` for a caller to branch on. On a client
/// that raises on `success:false` (opencode does) that is the difference between the agent
/// seeing a failure and seeing nothing.
///
/// The trap is written by the wrapper's Catch block and aborts execution, so it is the LAST
/// non-empty line. Anchoring there rather than matching anywhere keeps a script that
/// legitimately prints the word ERROR mid-run from being reported as an abort. The
/// leading-prefix test is kept as well, so nothing detected before stops being detected.
pub fn runtime_abort_line(output: &str) -> Option<&str> {
    /// #159: the abort is not always at the START of the last line. A script whose final
    /// `Write` did not end in `,!` leaves the trap concatenated onto its own output —
    /// `ROW=ERROR: <SYNTAX> …` — and a `strip_prefix` test misses it, so a hard abort came
    /// back `success:true` or not depending on one character. Measured over the eval corpus
    /// at 0.8.4: 209 aborts at line start, 189 mid-line.
    ///
    /// Still anchored to the last non-empty line, which is what keeps a script that
    /// legitimately prints the word ERROR mid-run from being called an abort — a bare
    /// substring test over the whole output would lose that. Taking the LAST marker on that
    /// line to end-of-line also drops the script's own output out of the `error` field,
    /// which returning the whole line would have carried into it.
    fn abort_tail(line: &str) -> Option<&str> {
        let mut best: Option<usize> = None;
        for marker in ["ERROR: ", "ERROR($ZERROR): "] {
            let mut from = 0;
            while let Some(i) = line[from..].find(marker) {
                let at = from + i;
                if line[at + marker.len()..].starts_with('<') {
                    best = Some(best.map_or(at, |b: usize| b.max(at)));
                }
                from = at + 1;
            }
        }
        best.map(|i| line[i..].trim_end())
    }
    let last = output.lines().rev().find(|l| !l.trim().is_empty())?;
    if let Some(tail) = abort_tail(last.trim()) {
        return Some(tail);
    }
    // The wrapper's one non-`<signal>` failure, emitted when the device redirect could not be
    // established. Matched exactly rather than by the old blanket `starts_with("ERROR: ")`,
    // which also caught a script whose own first line happened to print the word.
    output
        .lines()
        .map(str::trim)
        .find(|l| *l == "ERROR: output capture unavailable")
}

/// `<ENDOFFILE>` is a `READ` loop reaching the end of a file — an abort, but usually the
/// intended terminator rather than a defect. It is still reported as a failure, because the
/// script did stop early and the ask in #123 was consistency; the hint says so, so a caller
/// can tell this apart from a real fault without the flag having to lie.
fn abort_hint(abort: &str) -> Option<&'static str> {
    abort.contains("<ENDOFFILE>").then_some(
        "<ENDOFFILE> is usually a READ loop reaching the end of the file, not a defect — the \
         output above is everything that was read. If that is what you intended, treat this as \
         done; guard the loop with `Quit:$ZEOF` to end without the trap.",
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
    /// #169: monotonic write-gate latch. Once the gate has been observed CLOSED it stays
    /// closed for the life of the process. A hot-reload may still NARROW the gate — that is
    /// exactly what #114 built it for — but it can never widen one that has shut, because the
    /// value the gate is inferred from (the namespace, via `is_write_allowed`) is read from a
    /// config file that lives inside the caller's own workspace. Without the latch a caller
    /// refused a write can rewrite `namespace` to something that does not look like production
    /// and retry, which was reproducible on 0.13.0. Reopening now needs a restart, where an
    /// operator is present.
    write_gate_latched: Arc<std::sync::atomic::AtomicBool>,
    /// The PRUNED router. Both `list_tools` and `call_tool` read this one — see the
    /// `#[tool_handler(router = ...)]` note on the ServerHandler impl.
    tool_router: ToolRouter<IrisTools>,
}

/// Whether a specific CALL would mutate the instance, and under which name.
///
/// #114. The gate used to be a list of two tool NAMES removed from the router when the
/// connection was not write-allowed, which was wrong in both directions:
///
/// - It let five write-capable tools straight through. `iris_doc {mode:put}`,
///   `iris_execute`, `iris_compile`, `iris_lookup_manage {action:set}` and `iris_test` all
///   dispatched and reached IRIS on a write-disallowed connection — proven at the wire.
///   Two refusals and a tool list that shrank to 21 made a Live server *look* guarded.
/// - It also blocked reads. Removing `iris_production_item` wholesale took `get_settings`
///   with it, so you could not even look at a config item. This server is aimed at
///   DEVELOPMENT instances; friction there is the expensive failure, not the safe one.
///
/// So the gate is per-CALL and mutation-only: reading is never blocked anywhere, and a
/// mutation is refused only on a connection that is not write-allowed. Every tool stays
/// listed and reachable on every connection.
///
/// Returns `Some(action)` naming what would have been mutated, or `None` for a read.
/// The classification is per-tool-and-arguments and comes from each tool's verified reach
/// — a name-based guess got this wrong twice (it read `iris_get_log`, an in-memory store
/// lookup, as a writer and missed `iris_doc` entirely).
pub(crate) fn mutating_call(tool: &str, args: &serde_json::Value) -> Option<&'static str> {
    // The discriminator, whatever this tool calls it.
    let action = args
        .get("action")
        .or_else(|| args.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match tool {
        // Unconditionally mutating: these have no read-only mode at all.
        // iris_execute runs arbitrary ObjectScript, and its generator path writes, compiles
        // and deletes a scratch class even for a read-shaped `write` statement.
        "iris_execute" => Some("run ObjectScript"),
        // Compiling regenerates storage and replaces the compiled class.
        "iris_compile" => Some("compile"),
        // %UnitTest runs arbitrary test code, and TestProduction starts productions.
        "iris_test" => Some("run tests"),
        // Every action of this tool writes a credential.
        "iris_credential_manage" => Some("change credentials"),

        // Mode/action-aware: the read half must keep working.
        "iris_doc" => matches!(action, "put" | "delete").then_some("write a document"),
        "iris_lookup_manage" => {
            matches!(action, "set" | "delete").then_some("change a lookup table")
        }
        "iris_lookup_transfer" => (action == "import").then_some("import a lookup table"),
        "iris_production" => matches!(
            action,
            "start" | "stop" | "restart" | "update" | "recover" | "set_autostart"
        )
        .then_some("change production state"),
        "iris_production_item" => matches!(
            action,
            "add" | "remove" | "enable" | "disable" | "set_settings"
        )
        .then_some("change a production item"),
        // source_map is the one iris_debug action that writes — it goes through
        // execute_via_generator, which PUTs and compiles a scratch class.
        "iris_debug" => (action == "source_map").then_some("build a source map"),
        // iris_query keeps its own gate (`force` + write_tools_enabled) because it has to
        // parse the SQL to know; this is the same verdict expressed for the dispatcher, so
        // a forced destructive statement is refused here first and identically.
        "iris_query" => args
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            .then_some("run forced SQL"),

        // Everything else reads. Named exhaustively rather than defaulted, so a NEW tool
        // does not inherit "safe" by omission — `write_capable_tools_are_all_classified`
        // fails on any interop tool missing from this match.
        "check_config"
        | "docs_introspect"
        | "extract_message_map_routing"
        | "find_subclass_implementations"
        | "iris_business_rule_info"
        | "iris_credential_list"
        | "iris_get_log"
        | "iris_interop_query"
        | "iris_message_body"
        | "iris_production_diff"
        | "iris_symbols"
        | "iris_table_info" => None,
        _ => None,
    }
}

/// #143: a list argument, however the caller spelled it.
///
/// `body_select` was read with a bare `as_array()`, so ANY other shape fell
/// through `unwrap_or_default()` to an empty vec — and an empty `body_select`
/// is indistinguishable from "not asked for". The join was still built and
/// still filtered on, so the call returned `success: true` with the body
/// columns silently missing. A model emitting `"AccessionNumber,ExamCode"`
/// instead of `["AccessionNumber","ExamCode"]` is the single most common way
/// to reach that, and nothing told it.
///
/// Accepted: a JSON array of strings; a JSON-encoded array in a string; a
/// comma-separated string; a single bare name. Anything else is an ERROR —
/// the one thing it must never do again is quietly return `[]`.
pub(crate) fn string_list_arg(
    name: &str,
    v: Option<&serde_json::Value>,
) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(vec![]) };
    if v.is_null() {
        return Ok(vec![]);
    }
    if let Some(arr) = v.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            match item.as_str() {
                Some(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
                _ => {
                    return Err(format!(
                        "{name} must be a list of column names; it contained {item}"
                    ))
                }
            }
        }
        return Ok(out);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if s.is_empty() {
            return Ok(vec![]);
        }
        // A JSON array that arrived as a string — common when a client stringifies
        // its own arguments.
        if s.starts_with('[') {
            return match serde_json::from_str::<serde_json::Value>(s) {
                Ok(parsed) => string_list_arg(name, Some(&parsed)),
                Err(e) => Err(format!(
                    "{name} looks like a JSON array but did not parse: {e}"
                )),
            };
        }
        return Ok(s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect());
    }
    Err(format!(
        "{name} must be a list of column names (or a comma-separated string); got {v}"
    ))
}

/// The call's arguments as a JSON object — `{}` when the client sent none.
fn args_of(request: &rmcp::model::CallToolRequestParams) -> serde_json::Value {
    request
        .arguments
        .clone()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// The interop tools this file has deliberately classified in [`mutating_call`].
/// Guarded by a test so a tool added to `INTEROP_TOOLS` cannot skip the decision.
#[cfg(test)]
pub(crate) const CLASSIFIED_TOOLS: &[&str] = &[
    "check_config",
    "docs_introspect",
    "extract_message_map_routing",
    "find_subclass_implementations",
    "iris_business_rule_info",
    "iris_compile",
    "iris_credential_list",
    "iris_credential_manage",
    "iris_debug",
    "iris_doc",
    "iris_execute",
    "iris_get_log",
    "iris_interop_query",
    "iris_lookup_manage",
    "iris_lookup_transfer",
    "iris_message_body",
    "iris_production",
    "iris_production_diff",
    "iris_production_item",
    "iris_query",
    "iris_symbols",
    "iris_table_info",
    "iris_test",
];

#[tool_router]
impl IrisTools {
    /// Baseline-toolset constructor. Delegates to `with_registry_and_toolset` so the
    /// router is PRUNED the same way every other constructor prunes it — it used to
    /// stamp `toolset: Toolset::Baseline` while assigning the raw 58-tool
    /// `Self::tool_router()`, i.e. it carried the 4 merged-only tools baseline does not
    /// advertise. Harmless only while `registered_tool_names` ignored the router; now
    /// that it derives from the router (2026-08-26) the two must agree.
    pub fn new(iris: Option<IrisConnection>) -> anyhow::Result<Self> {
        Self::with_registry_and_toolset(
            iris,
            crate::skills::SkillRegistry::new(),
            Toolset::Baseline,
            None,
            None,
        )
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
    ///
    /// Derived from the live `tool_router` for EVERY toolset. The router is built in
    /// `with_registry_and_toolset`, which applies toolset pruning AND the write gate,
    /// so this can never disagree with what `tools/list` advertises.
    /// (Until 2026-08-26 the non-Interop tiers used a hardcoded list last audited
    /// against v0.4.x; it had drifted 17 tools short and carried one phantom —
    /// `iris_admin`, which the router removes for baseline/nostub. The tests that
    /// guarded it compared the hardcoded list against itself, so the drift was
    /// invisible; `test_toolset_counts_match_doc_comments` now pins the real numbers.)
    pub fn registered_tool_names(&self) -> std::collections::HashSet<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Whether `name` would actually DISPATCH — `has_route` consults the same map
    /// `ToolRouter::call` looks the name up in, so this is reachability, not listing.
    ///
    /// `registered_tool_names()` answers a different question (what tools/list
    /// advertises). Until #104 the two could disagree completely, because dispatch ran
    /// against a fresh unpruned router; asserting only on the listing is what let that
    /// survive. Prefer this one for anything that is meant to be a GUARD.
    pub fn is_tool_reachable(&self, name: &str) -> bool {
        self.tool_router.has_route(name)
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
                // #114: NOTHING is pruned for the write gate any more. Removing a tool took
                // its read actions with it — `iris_production_item` gone meant `get_settings`
                // gone — and this server is aimed at development instances, where that
                // friction costs far more than it protects. The gate now runs per CALL in
                // `call_tool`, refusing only the mutating ones, so every tool stays listed
                // and every read works on every connection.
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
            write_gate_latched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_router: router,
        })
    }

    /// Returns the active IRIS connection, or IRIS_UNREACHABLE if not connected.
    fn get_iris(&self) -> Result<Arc<IrisConnection>, McpError> {
        let (iris, pending) = {
            let c = self.connection.lock().unwrap();
            (c.iris.clone(), c.discovery_pending)
        };
        iris.ok_or_else(|| iris_unreachable_detail(pending, iris_is_configured()))
    }

    /// #110: a connection missed at startup used to poison the whole session — `iris`
    /// stayed `None` for its lifetime and every tool answered IRIS_UNREACHABLE while IRIS
    /// was up the entire time. The 2-second startup cap is a real window to miss: it was
    /// hit twice on a machine running concurrent `cargo clippy --all-targets`, and it
    /// presents as "the MCP tools are broken", not "the probe was slow". A room of laptops
    /// starting containers at once is exactly that load profile.
    ///
    /// So: re-probe lazily, on the first tool call that needs a connection, behind a
    /// cooldown. Seeded with whatever the CLI flags / workspace config described, so a
    /// retry targets the same instance rather than falling back to env discovery.
    async fn retry_discovery(&self) {
        let (seed, wait) = {
            let c = self.connection.lock().unwrap();
            if c.iris.is_some() || c.discovery_pending {
                // Connected, or the startup task is still running and will adopt its own
                // result — a second concurrent probe would only add load.
                return;
            }
            let cooldown = discovery_retry_cooldown();
            let too_soon = c.last_retry.is_some_and(|t| t.elapsed() < cooldown);
            (c.discovery_seed.clone(), !too_soon)
        };
        if !wait {
            return;
        }
        self.connection.lock().unwrap().last_retry = Some(std::time::Instant::now());

        match crate::iris::discovery::discover_iris(seed.clone()).await {
            crate::iris::discovery::IrisDiscovery::Found(c) => {
                tracing::info!(
                    base_url = %c.base_url,
                    "IRIS reached on a lazy re-probe — the session recovered without a restart"
                );
                let source = self.connection.lock().unwrap().source.clone();
                let mut state = ConnectionState::from_iris(c, source, None);
                state.discovery_seed = seed;
                state.last_retry = Some(std::time::Instant::now());
                *self.connection.lock().unwrap() = state;
            }
            _ => {
                tracing::debug!("lazy IRIS re-probe found nothing; will retry after the cooldown");
            }
        }
    }

    /// Adopt a connection discovered after startup, or by any other out-of-band route.
    /// The write gate is re-evaluated per call (see `call_tool`), so a late connection to a
    /// Live instance is gated even though the router was built while disconnected.
    pub fn adopt_connection(&self, conn: IrisConnection, source: ConnectionSource) {
        let seed = self.connection.lock().unwrap().discovery_seed.clone();
        let mut state = ConnectionState::from_iris(conn, source, None);
        state.discovery_seed = seed;
        *self.connection.lock().unwrap() = state;
    }

    /// Record what a re-probe should target, and whether startup discovery is still running.
    pub fn set_discovery_state(&self, seed: Option<IrisConnection>, pending: bool) {
        let mut c = self.connection.lock().unwrap();
        c.discovery_seed = seed;
        c.discovery_pending = pending;
    }

    /// Startup discovery has finished (successfully or not) — stop reporting "still running".
    pub fn clear_discovery_pending(&self) {
        self.connection.lock().unwrap().discovery_pending = false;
    }

    /// Check for config file changes then return the active connection.
    /// Use this in tool handlers instead of get_iris() to enable hot-reload (034).
    async fn get_iris_reloaded(&self) -> Result<Arc<IrisConnection>, McpError> {
        self.check_reload().await;
        // #110: heal a session that started before IRIS was ready, instead of answering
        // IRIS_UNREACHABLE for its whole lifetime. No-op when connected.
        self.retry_discovery().await;
        self.get_iris()
    }

    /// Returns the active write_tools_enabled flag from connection state.
    /// Public so a test can assert the CONNECTION is read-only without inferring it from
    /// which tools are listed — #114 stopped the gate expressing itself in the listing.
    pub fn write_tools_enabled(&self) -> bool {
        let (stored, iris) = {
            let c = self.connection.lock().unwrap();
            (c.write_tools_enabled, c.iris.clone())
        };
        // #169: report what is ENFORCED, latch included, not the value cached at swap time.
        match iris.as_deref() {
            Some(c) => self.write_gate_open(c),
            None => stored,
        }
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
        description = "Compile an ObjectScript class, routine, or wildcard package on IRIS via Atelier REST. Wildcards ('MyApp.*', 'MyApp.*.cls') expand CLASS documents only and are guarded: the pattern MUST begin with a literal package prefix before its first '*' (bare '*', '*.cls', '*Foo' are refused as SCOPE_REQUIRED — they would select the whole namespace), and a pattern matching more than 500 documents is refused as TOO_BROAD with the count, so narrow the package rather than retrying. Matching ignores the document suffix, so 'Pkg.Class.*' means the SUBPACKAGE of Pkg.Class, never Pkg.Class itself; a pattern that matches nothing is NOT_FOUND. Atelier's listing omits Hidden and generated classes, so a wildcard cannot expand them: any that match are named in `not_expanded` with a count, and are NOT compiled — compile those by exact name. Compile a .mac/.int/.inc routine by its exact name. Returns structured errors with line numbers, columns, and severity. No Python required."
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
                    // #93: an Atelier 404 for a missing NAMESPACE has a zero-byte body, so
                    // it is indistinguishable from a missing document — this path reported
                    // it as UPLOAD_FAILED "PUT X returned HTTP 404 Not Found" and never
                    // named the namespace. Only on 404: 401/403 and 5xx keep their meaning.
                    if put_resp.status().as_u16() == 404 {
                        if let Some(e) = interop::namespace_missing_error(
                            &iris,
                            client,
                            &namespace,
                            &put_url,
                            "Nothing was compiled.",
                        )
                        .await
                        {
                            return e;
                        }
                    }
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

        // Expand wildcards: resolve "MyApp.*" / "MyApp.*.cls" to matching document names.
        // Bug 8: use namespace (not iris.namespace) and the correct /docnames/CLS endpoint.
        // Issue #88: `result.content` elements are OBJECTS on this build, not bare strings —
        // see `docnames_in_body`. `scanned` is carried so a NOT_FOUND can distinguish "the
        // pattern matched nothing" from "the listing itself came back empty".
        let mut scanned = 0usize;
        // #94: whether the listing was narrowed server-side, and with what. Hoisted out of
        // the wildcard arm because the NOT_FOUND message below MUST branch on it: once the
        // listing is filtered, `scanned` counts CANDIDATES, not the namespace.
        let mut listing_narrowed = false;
        let mut listing_filter_used: Option<&str> = None;
        let targets: Vec<String> = if p.target.contains('*') {
            // #88: an unqualified pattern is refused BEFORE the listing is fetched — there
            // is no expansion to inspect, and no reason to pull 10k names to say so.
            if wildcard_target_is_unqualified(&p.target) {
                return unqualified_wildcard_error(&p.target, &namespace);
            }
            let list_url = iris.versioned_ns_url(&namespace, "/docnames/CLS");
            // #94: narrow the listing SERVER-SIDE. `?filter=X` becomes `Name Like '%X%'`
            // inside the query GetDocNames already runs, so the response is a SUPERSET of
            // what the client regex selects — see `wildcard_listing_filter`. 1,696,950
            // bytes -> ~2,066; a wildcard compile 331 ms -> ~45 ms.
            //
            // DELIBERATELY NO CACHE, and do not add one. What is left after narrowing is
            // ~38 ms of server-side index walk that no filter can avoid; a perfect cache
            // would buy back ~33 ms. In-process invalidation cannot see another MCP process,
            // a human saving a class in VS Code / Studio / the Portal, an ImportDir or IPM
            // install, a mapping change, or generated dependents — and any of those inside
            // the TTL makes `Pkg.*` skip a class while still reporting success:true. That is
            // the exact failure mode this issue series exists to eliminate; 33 ms does not
            // buy it. `e2e_compile_wildcard_package` is the regression test.
            let listing_filter = wildcard_listing_filter(&p.target);
            let mut fetch_url = match listing_filter {
                Some(f) => format!("{list_url}?filter={}", urlencoding::encode(f)),
                None => list_url.clone(),
            };
            listing_filter_used = listing_filter;
            listing_narrowed = listing_filter.is_some();
            let mut listing = client
                .get(&fetch_url)
                .basic_auth(&iris.username, Some(&iris.password))
                .send()
                .await;
            // An Atelier build that rejects the parameter must degrade to exactly today's
            // behaviour, not to a new failure: retry once, unfiltered.
            if listing_narrowed && !matches!(&listing, Ok(r) if r.status().is_success()) {
                fetch_url = list_url.clone();
                listing_narrowed = false;
                listing_filter_used = None;
                listing = client
                    .get(&fetch_url)
                    .basic_auth(&iris.username, Some(&iris.password))
                    .send()
                    .await;
            }
            match listing {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    // One pass over the listing, not two: `scanned` and the expansion read
                    // the same Vec (15810 elements on the dev instance).
                    let names = docnames_in_body(&body);
                    scanned = names.len();
                    match expand_wildcard_target(&names, &p.target) {
                        // Unreachable — the guard above already returned — but the outcome
                        // is the pure function's to own, not this call site's.
                        WildcardExpansion::Unqualified => {
                            return unqualified_wildcard_error(&p.target, &namespace)
                        }
                        WildcardExpansion::TooBroad { matched } => {
                            return too_broad_wildcard_error(&p.target, &namespace, matched)
                        }
                        WildcardExpansion::Matched(t) => t,
                    }
                }
                // #88 follow-up: when the listing is unavailable there is NOTHING to expand
                // against, so the cap and the scope rule cannot be applied. Falling back to
                // the raw pattern used to hand `Pkg.*` straight to /action/compile with no
                // expansion, no count and no cap — the guard silently off exactly when the
                // instance is unhealthy. A wildcard therefore fails here instead of guessing.
                other => {
                    // #93: a 404 here used to become LISTING_UNAVAILABLE, which never said
                    // the namespace does not exist and never named the ones that do — the
                    // 404 body is zero bytes, so only a second question can tell them apart.
                    if let Ok(resp) = &other {
                        if resp.status().as_u16() == 404 {
                            if let Some(e) = interop::namespace_missing_error(
                                &iris,
                                client,
                                &namespace,
                                &fetch_url,
                                "Nothing was compiled.",
                            )
                            .await
                            {
                                return e;
                            }
                        }
                    }
                    let detail = match other {
                        Ok(resp) => format!("HTTP {}", resp.status().as_u16()),
                        Err(e) => e.to_string(),
                    };
                    return crate::tools::envelope::fail_with(
                        "LISTING_UNAVAILABLE",
                        &format!(
                            "Could not read the class listing for namespace {namespace}, so \
                             the wildcard '{}' could not be expanded: {detail}. Nothing was \
                             compiled.",
                            p.target
                        ),
                        serde_json::json!({
                            "pattern": p.target,
                            "namespace": namespace,
                            // #94: the URL actually requested, so the caller can reproduce it.
                            "listing_url": fetch_url,
                            "listing_filter": listing_filter_used,
                            "hint": "Compile a single document by its exact name, which needs \
                                     no listing. If the namespace is wrong, iris_query can \
                                     confirm it exists.",
                        }),
                    );
                }
            }
        } else {
            vec![p.target.clone()]
        };

        if targets.is_empty() {
            // #88: the old message was indistinguishable from a genuinely empty namespace.
            // It also has to say what was NOT searched: the listing is /docnames/CLS, so a
            // wildcard can never see a .mac/.int/.inc routine, and a bare NOT_FOUND there
            // reads as "that routine does not exist" when it does and compiles by name.
            // #94: `scanned` STOPS meaning "documents in the namespace" the moment the
            // listing is narrowed server-side.
            // #100: and neither wording could support the negative it asserted — the CLS
            // listing omits `Hidden = 1 OR GeneratedBy <> ''`, so "No documents match
            // pattern" was a claim about the world, made from a source that cannot see all
            // of it. Reproduced live: `EnsPortal.I*` answered NOT_FOUND while
            // EnsPortal.InterfaceMaps existed and compiled fine by its exact name.
            //
            // All three concerns now live in `compile_not_found_error`, which is pure and
            // therefore pinned by unit tests rather than by reading the call site.
            //
            // The cross-check is called from HERE and nowhere else. This block is reachable
            // only from `WildcardExpansion::Matched(vec![])` — after a listing that returned
            // 200 and matched nothing — so it cannot fire on the happy path, and it sits
            // downstream of every #88/#93 guard by construction rather than by a flag anyone
            // can flip: SCOPE_REQUIRED, TOO_BROAD, LISTING_UNAVAILABLE and the
            // missing-namespace attribution have all already returned above.
            let cross = if p.target.contains('*') {
                not_listable_crosscheck(&iris, client, &p.target, &namespace).await
            } else {
                // An exact target never consulted a listing, so there is no listing blindness
                // to second-guess and no query to pay for. (Unreachable today — an exact
                // target yields exactly one element — but the outcome is stated, not assumed.)
                CrossCheck::Unavailable("an exact target is not expanded from a listing".into())
            };
            return compile_not_found_error(
                &p.target,
                &namespace,
                listing_filter_used,
                listing_narrowed,
                scanned,
                cross,
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
            // #93, the verbatim repro: compiling an EXACT target in a namespace that does
            // not exist answered IRIS_UNREACHABLE + "Check IRIS_HOST and IRIS_WEB_PORT"
            // while IRIS was answering perfectly on that host and port. The 404 body is
            // zero bytes, so the transport alone cannot tell a missing namespace from a
            // missing document; ask the root descriptor. Only on 404 — a 401/403 is
            // credentials and a 5xx is a sick instance, and both keep their current error.
            if status == 404 {
                if let Some(e) = interop::namespace_missing_error(
                    &iris,
                    client,
                    &namespace,
                    &url_str,
                    "Nothing was compiled.",
                )
                .await
                {
                    return e;
                }
            }
            // #101: 401/403 are credentials and privileges, not connectivity — IRIS
            // answered, so it is reachable by definition. Only these two statuses move:
            // every other status keeps its current code verbatim, which is what keeps
            // `a_namespace_that_differs_only_in_case_is_not_reported_as_missing` (the #93
            // scope tripwire, asserting IRIS_UNREACHABLE for a 404 whose namespace DOES
            // exist) green. A failure there is a scope violation, not a test to update.
            if envelope::auth_status_code(status).is_some() {
                return envelope::http_status_fail("iris_compile", resp.status(), &url_str);
            }
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
                // #80: `location` is emitted on every entry, including this wrapper, so
                // the array stays homogeneous once the console parser starts filling it.
                errors.push(
                    serde_json::json!({"severity":"error","code":"","line":0,"column":0,"location":"","text":msg}),
                );
            }
        }
        // Also check status.summary as a fallback — some IRIS versions put the error only there.
        if errors.is_empty() {
            let summary = body["status"]["summary"].as_str().unwrap_or("");
            if summary.contains("ERROR") {
                errors.push(serde_json::json!({"severity":"error","code":"","line":0,"column":0,"location":"","text":summary}));
            }
        }

        // Parse console output for per-line errors and warnings.
        // Issue #80: this build prefixes per-method diagnostics with "ERROR:" (colon), not
        // "ERROR " (space) — `parse_console_diag` accepts both, so a class with N broken
        // methods now yields ~2N individually addressable entries instead of collapsing to
        // the one status.errors wrapper. `location` is additive: "M1+2" / "Foo.Bar.1".
        for line in &console {
            let text = line.as_str().unwrap_or("");
            if let Some(d) = parse_console_diag(text, "ERROR:", "ERROR ") {
                if !console_diag_already_reported(&errors, &d.text) {
                    errors.push(serde_json::json!({"severity":"error","code":d.code,"line":d.line,"column":0,"location":d.location,"text":d.text}));
                }
            } else if let Some(d) = parse_console_diag(text, "WARNING:", "WARNING ") {
                warnings.push(serde_json::json!({"severity":"warning","code":d.code,"line":d.line,"column":0,"location":d.location,"text":d.text}));
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
        // #100 on the SUCCESS path, finished by #109. The listing blindness does not only
        // produce false NOT_FOUNDs; it under-compiles and reports that as success.
        // Reproduced live: `iris_compile {"target":"EnsPortal.*Maps"}` returned success:true
        // / targets_compiled:1 while EnsPortal.InterfaceMaps — Hidden=1, same pattern — was
        // silently never compiled.
        //
        // #100 disclosed that generically, as a fixed string, to keep the happy path at
        // #94's ~42 ms. That was not enough: a caller reading `success: true` has no reason
        // to look, and the specific class was never named. So the cross-check now runs here
        // too and NAMES what it skipped. The cost is bounded by construction — a wildcard
        // must already carry a literal package prefix before its first `*` (SCOPE_REQUIRED),
        // so the LIKE is always anchored and indexed — and it is one indexed SELECT against
        // a POST /action/compile that takes orders of magnitude longer. "Unavailable" is
        // reported as unavailable, never as "nothing was skipped".
        if p.target.contains('*') {
            let compiled: std::collections::HashSet<String> = targets
                .iter()
                .map(|t| docname_stem(t).to_string())
                .collect();
            resp["expansion_source"] = serde_json::Value::String("atelier /docnames/CLS".into());
            match not_listable_crosscheck(&iris, client, &p.target, &namespace).await {
                CrossCheck::Found { rows, truncated } => {
                    let skipped: Vec<&NotListable> = rows
                        .iter()
                        .filter(|r| !compiled.contains(&r.name))
                        .collect();
                    resp["not_expanded_count"] = serde_json::json!(skipped.len());
                    resp["not_expanded_truncated"] = serde_json::json!(truncated);
                    resp["not_expanded"] = serde_json::Value::Array(
                        skipped
                            .iter()
                            .take(NOT_LISTABLE_NAMES_SHOWN)
                            .map(|r| {
                                serde_json::json!({
                                    "name": r.name,
                                    "hidden": r.hidden,
                                    "generated_by": r.generated_by,
                                })
                            })
                            .collect(),
                    );
                    if !skipped.is_empty() {
                        let names: Vec<&str> = skipped
                            .iter()
                            .map(|r| r.name.as_str())
                            .take(NOT_LISTABLE_NAMES_SHOWN)
                            .collect();
                        resp["expansion_note"] = serde_json::Value::String(format!(
                            "{} class(es) also match '{}' but are Hidden or generated, so \
                             /docnames/CLS does not list them and they were NOT compiled: {}{}. \
                             Compile them by exact name if you meant to include them.",
                            skipped.len(),
                            p.target,
                            names.join(", "),
                            if skipped.len() > names.len() {
                                format!(" (showing {} of {})", names.len(), skipped.len())
                            } else {
                                String::new()
                            },
                        ));
                    }
                }
                CrossCheck::NoSuchClass => {
                    // Every class matching the pattern is in the listing — nothing was
                    // skipped, and that is now a checked fact rather than an untested hope.
                    resp["not_expanded_count"] = serde_json::json!(0);
                    resp["not_expanded"] = serde_json::json!([]);
                }
                CrossCheck::Unavailable(why) => {
                    resp["not_expanded_count"] = serde_json::Value::Null;
                    resp["expansion_note"] = serde_json::Value::String(format!(
                        "Hidden and generated classes are not listed by /docnames/CLS and were \
                         not expanded. The %Dictionary.ClassDefinition cross-check that would \
                         say whether any such class matches '{}' could not be run ({why}), so \
                         this compile is not proof that none were skipped.",
                        p.target
                    ));
                }
            }
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

        // US3 / #102: namespace pre-flight before running tests.
        //
        // This used to spend a whole `execute_via_generator` cycle — PUT + compile + SQL query
        // + DELETE, four HTTP round trips — hardcoded into namespace USER, inside a 10s
        // timeout, to ask `%SYS.Namespace.Exists`, and then `.unwrap_or(true)`: assume it
        // exists. The #93 helper answers the same question with ONE GET of `/api/atelier/`,
        // and answers it better — it lists the namespaces that DO exist, and it says "or is
        // not accessible to user X", a distinction `%SYS.Namespace.Exists` cannot make (it
        // reports raw existence, so a namespace the credentials cannot reach passed this
        // pre-flight and failed later with a confusing error). `None` still means CANNOT TELL,
        // which is the old optimistic default stated honestly. Same ERR_NAMESPACE_NOT_FOUND.
        if let Some(missing) = interop::namespace_missing_error(
            &iris,
            client,
            &namespace,
            &iris.versioned_ns_url(&namespace, "/action/query"),
            "No tests were run.",
        )
        .await
        {
            self.record_call("iris_test", false);
            return missing;
        }

        // Generate a UUID correlation token; used as UserParam in RunTest.
        let correlation_token = log_store::new_log_id();

        // Detect whether the pattern is a compiled class name or a filesystem directory path.
        // Class names contain dots and no path separators: "ISC.sql.Tests", "MyApp.Tests.*"
        // Directory paths contain / or \ : "MyApp/Tests", "/tmp/tests/MyApp"
        // When the pattern is a class name, pass /noload so RunTest looks in the compiled
        // database rather than scanning the filesystem under ^UnitTestRoot.
        // #67: classify the pattern the caller actually sent. It used to be classified after
        // C-style quote escaping, so a pattern containing `"` grew a backslash and was taken
        // for a directory path.
        let is_class_pattern = !p.pattern.contains('/') && !p.pattern.contains('\\');
        let flags = if is_class_pattern {
            "/verbose=1/nodelete/noload"
        } else {
            "/verbose=1/nodelete"
        };

        // Run tests via execute_via_generator (HTTP path).
        // After RunTest completes, the ^UnitTest.Result global IS persisted.
        // This used to be explained by "globals bypass the objectgenerator transaction
        // boundary". That explanation is dead: user code has not run at compile time since
        // build_exec_class moved it into RunUser(), and `write $TLEVEL` through this path
        // reports 0 on 2026.1 — there is no enclosing transaction to bypass.
        let run_code = if is_class_pattern {
            build_class_test_run_code(&p.pattern, flags, &correlation_token)
        } else {
            // Directory path: set ^UnitTestRoot and pre-create the pattern subdirectory.
            // ^UnitTestRoot is platform-aware: a portable temp dir on Windows (mgr/Temp via
            // %File.TempFilename), or /tmp/httest/ on Linux (matches the container e2e fixtures).
            format!(
                r#"set tIsWin=($zcvt($system.Version.GetOS(),"U")="WINDOWS")
set utRoot=$select(tIsWin:##class(%File).NormalizeDirectory("httest",##class(%File).GetDirectory(##class(%File).TempFilename())),1:"/tmp/httest/")
if '##class(%File).DirectoryExists(utRoot) {{ do ##class(%File).CreateDirectoryChain(utRoot) }}
set pkgDir=##class(%File).NormalizeDirectory({pattern},utRoot)
if '##class(%File).DirectoryExists(pkgDir) {{ do ##class(%File).CreateDirectoryChain(pkgDir) }}
set ^UnitTestRoot=utRoot
do ##class(%UnitTest.Manager).RunTest({pattern},"{flags}","{token}")"#,
                token = correlation_token,
                pattern = os_str_expr(&p.pattern),
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
                        Err(_) => {
                            self.record_call("iris_test", false);
                            return envelope::fail(
                                "TIMEOUT",
                                &format!("Test run timed out after {}s", p.timeout),
                            );
                        }
                        Ok(Err(e)) => {
                            // #101/#102: the HTTP leg's error was discarded here, so a wrong
                            // password and a mistyped namespace both answered DOCKER_REQUIRED
                            // — naming an env var with no bearing on either problem.
                            self.record_call("iris_test", false);
                            if let Some(missing) = interop::namespace_missing_error_for(
                                &iris,
                                client,
                                &namespace,
                                "No tests were run.",
                                &e,
                            )
                            .await
                            {
                                return missing;
                            }
                            let msg = e.to_string();
                            let code = interop::classify_iris_error_or(&msg, "DOCKER_REQUIRED");
                            let text = if code == "DOCKER_REQUIRED" {
                                format!("iris_test: IRIS_CONTAINER set but docker exec failed and HTTP fallback also failed: {msg}.{DOCKER_REQUIRED_HINT}")
                            } else {
                                format!("iris_test: docker exec failed and the HTTP fallback was rejected: {msg}")
                            };
                            return envelope::fail(code, &text);
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
                    // #101: "PUT doc failed: HTTP 401 Unauthorized" is a credentials failure,
                    // not a test-execution failure — three tools used to give one cause three
                    // different codes. #102: and a 404 here is a mistyped namespace.
                    if let Some(missing) = interop::namespace_missing_error_for(
                        &iris,
                        client,
                        &namespace,
                        "No tests were run.",
                        &e,
                    )
                    .await
                    {
                        return missing;
                    }
                    let msg = e.to_string();
                    return envelope::fail(
                        interop::classify_iris_error_or(&msg, ERR_TEST_EXECUTION_ERROR),
                        &msg,
                    );
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
        // Stdout is parsed rather than read back from ^UnitTest.Result because it carries the
        // per-method timing directly. The original reason given — that the global holds only
        // suite-level rows because class/method %Save() calls sit in nested transactions that
        // never commit — no longer applies: execution moved out of the objectgenerator context
        // (see build_exec_class) and `write $TLEVEL` reports 0 here. Whether the global is now
        // populated per method has NOT been re-measured; parsing stdout is independent of it.
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
                            // #166: no `total`/`passed`/`failed` here. A failure envelope must
                            // not carry counters describing a run that did not happen — `failed: 0`
                            // is exactly the field a naive green-check reaches for, and it reads
                            // as a passing run. `success:false` + `error_code` is the contract.
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
                    // #166: see the NO_RUNNABLE_TESTS envelope above — a failure must not be
                    // shaped like a zeroed scoreboard.
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
        description = "Execute arbitrary ObjectScript code on IRIS and return stdout. Uses pure-HTTP execution: your code is written into the RunUser() method of a temp class, compiled, run at CALL time by an Execute() SqlProc that captures the device output, then the class is deleted. Falls back to docker exec if IRIS_CONTAINER env var is set and HTTP fails. &sql(...) embedded SQL macros are automatically translated to %SQL.Statement calls (set translate_sql: false to disable). When translation fires, response includes sql_translated: true and translated_code. Example: code='write $ZVERSION,!' returns the IRIS version string. Use this for side-effecting ObjectScript only — for SELECTs use iris_query, for class/table introspection use docs_introspect/iris_symbols/iris_table_info, for production state use iris_production/iris_interop_query, and to create+compile a class use iris_doc(put,compile) over Atelier (never $SYSTEM.OBJ.Load from a file path — that needs IRIS to share this host's disk). When the code matches one of those, the response includes a `hint` naming the typed tool. namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE), never a hardcoded USER; the response echoes the namespace it ran in."
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

        // Try pure-HTTP execution first (write-compile-query; see build_exec_class — user code
        // runs at CALL time in RunUser(), NOT at compile time via CodeMode=objectgenerator).
        let gen_result = tokio::time::timeout(
            timeout,
            iris.execute_via_generator(code_to_run, &namespace, client),
        )
        .await;

        // #101: what the HTTP leg said, kept for the docker fallback's error message. The
        // `Ok(Err(_))` arm below used to DISCARD it, so a wrong password was reported as
        // DOCKER_REQUIRED — "set IRIS_CONTAINER", an env var with no bearing on the problem.
        // Bound as a match EXPRESSION: the other two arms diverge (both `return`), so only
        // the fall-through arm yields a value. A deferred `let` + assignment trips
        // clippy::needless_late_init on the CI toolchain (1.98) though not on 1.96.
        let http_leg_error: String = match gen_result {
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
                // #123: anywhere in the output, not only at the start — see runtime_abort_line.
                let abort = runtime_abort_line(trimmed);
                let is_runtime_error = abort.is_some();
                self.record_call("iris_execute", !is_runtime_error);
                let mut resp = serde_json::json!({
                    "success": !is_runtime_error,
                    "output": trimmed,
                    "namespace": namespace,
                    "method": "http",
                });
                if let Some(abort) = abort {
                    resp["error_code"] = serde_json::Value::String("IRIS_RUNTIME_ERROR".into());
                    if let Some(h) = abort_hint(abort) {
                        resp["hint"] = serde_json::Value::String(h.into());
                    }
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
                // `output`) and the result must be flagged isError on the wire. #123: `error`
                // carries the abort LINE — `output` still holds everything the script wrote
                // before it, which the caller asked to keep.
                if let Some(abort) = abort {
                    enrich_abort(&iris, client, &namespace, abort, code_to_run, &mut resp).await;
                    return envelope::fail_with("IRIS_RUNTIME_ERROR", abort, resp);
                }
                return ok_json(resp);
            }
            Ok(Err(e)) => {
                // #102: a 404 here is very often a mistyped NAMESPACE. Docker was never the
                // problem and cannot be the remedy, so answer it now rather than falling
                // through and reporting the fallback's failure as the cause.
                if let Some(missing) = interop::namespace_missing_error_for(
                    &iris,
                    client,
                    &namespace,
                    "Nothing was executed.",
                    &e,
                )
                .await
                {
                    self.record_call("iris_execute", false);
                    return missing;
                }
                // #101: the fall-through STAYS. `IrisConnection::execute` shells out via
                // `docker exec ... iris session` and does not use IRIS_USERNAME/IRIS_PASSWORD
                // at all, so with a valid IRIS_CONTAINER a wrong Atelier password genuinely IS
                // recoverable. Only what gets REPORTED when the docker leg also fails changes.
                e.to_string()
            }
        };

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
                    // #101: both legs have now failed. If the HTTP leg was an auth refusal,
                    // that is the cause and IRIS_CONTAINER is not the remedy — name both legs.
                    // DOCKER_REQUIRED survives verbatim for every non-auth HTTP failure.
                    if let Some(code) = envelope::auth_error_code(&http_leg_error) {
                        return envelope::fail(
                            code,
                            &format!(
                                "iris_execute: the HTTP path was rejected ({http_leg_error}); \
                                 the docker exec fallback is unavailable because IRIS_CONTAINER \
                                 is not set."
                            ),
                        );
                    }
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
                // #123: same rule as the HTTP arm — the flag must not depend on prior output.
                let abort = runtime_abort_line(trimmed);
                let is_runtime_error = abort.is_some();
                self.record_call("iris_execute", !is_runtime_error);
                let mut resp = serde_json::json!({
                    "success": !is_runtime_error,
                    "output": trimmed,
                    "namespace": namespace,
                    "method": "docker",
                });
                if let Some(abort) = abort {
                    resp["error_code"] = serde_json::Value::String("IRIS_RUNTIME_ERROR".into());
                    if let Some(h) = abort_hint(abort) {
                        resp["hint"] = serde_json::Value::String(h.into());
                    }
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
                if let Some(abort) = abort {
                    return envelope::fail_with("IRIS_RUNTIME_ERROR", abort, resp);
                }
                ok_json(resp)
            }
        }
    }

    #[tool(
        description = "Read, write, delete, or check an IRIS document. mode='get' fetches source, mode='put' writes (with automatic SCM checkout if needed), mode='delete' removes, mode='head' checks existence. name needs the Atelier type suffix — 'MyApp.Patient.cls', not 'MyApp.Patient' (put adds it for you when the content starts with `Class <name>` or `ROUTINE <name>`). Supports batch ops via 'names' array and elicitation_id/elicitation_answer for SCM dialog resumption. For large source, paginate get with max_bytes + offset (response includes next_offset), or prefer docs_introspect for signatures/structure instead of full source. No Python required."
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

        // #105: `iris_query` used to carry its own copy of this request — which meant its own
        // (drifted) response reading AND no retry at all, while every `query`-backed tool rode
        // out transient drops. One OpenCode campaign run took four consecutive
        // IRIS_UNREACHABLEs here over ~190s of a sandbox blip that the shared retry absorbs.
        // Now the request, the retry policy and the reading are all shared; what stays local
        // is the PRESENTATION — SQL_ERROR with the table hints, and the #93 404 probe.
        let params: Vec<serde_json::Value> = p
            .parameters
            .iter()
            .map(|v| serde_json::Value::String(v.clone()))
            .collect();
        let outcome = match iris
            .query_outcome(&p.query, params, &namespace, client)
            .await
        {
            Ok(o) => o,
            Err(e) => return crate::tools::envelope::transport_fail("iris_query", &e.to_string()),
        };
        let body = match outcome {
            crate::iris::connection::QueryOutcome::Rows(body) => body,
            crate::iris::connection::QueryOutcome::HttpError { status, .. } => {
                // #93 first: the 404 body is ZERO bytes, so the transport alone cannot tell a
                // missing namespace from a missing endpoint — ask the root descriptor. `None`
                // means cannot tell, and the status-coded error below survives.
                if status.as_u16() == 404 {
                    if let Some(missing) = interop::namespace_missing_error(
                        &iris,
                        client,
                        &namespace,
                        &query_url,
                        "No rows were read.",
                    )
                    .await
                    {
                        return missing;
                    }
                }
                // #101, the filed repro: this answered IRIS_UNREACHABLE + "Check IRIS_HOST and
                // IRIS_WEB_PORT" for a 401 while IRIS was answering perfectly on that very host
                // and port. An HTTP *response* is proof of reachability. The hint goes with the
                // code: `http_status_fail` attaches `attempted_url` (useful) and lets
                // `builtin_hint` supply the credentials advice (correct), instead of hard-coding
                // networking advice that was wrong for every status.
                self.record_call("iris_query", false);
                return envelope::http_status_fail("iris_query", status, &query_url);
            }
            // A 200 whose body is not JSON is a proxy error page or an HTML login redirect,
            // NOT an empty result set. `unwrap_or_default()` used to make it the latter:
            // `{"success":true,"rows":[],"count":0}` — a confident wrong answer, and the
            // exact shape #102 was filed about, surviving in the one tool that copy missed.
            crate::iris::connection::QueryOutcome::NonJson { status, snippet } => {
                self.record_call("iris_query", false);
                return envelope::fail_with(
                    "IRIS_REQUEST_FAILED",
                    &format!("non-JSON response from {query_url}: {snippet}"),
                    serde_json::json!({
                        "attempted_url": query_url,
                        "http_status": status.as_u16(),
                        "hint": "IRIS answered, but not with JSON — typically a proxy error \
                                 page or an HTML login redirect in front of the Atelier API. \
                                 No rows were read; this is not an empty result set.",
                    }),
                );
            }
            crate::iris::connection::QueryOutcome::IrisError(msg) => {
                let msg = msg.as_str();
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
                } else if let Some(h) = sql_lint::sqlcode_hint(msg) {
                    // #126: the table covered the two most common codes and stopped.
                    extra["hint"] = serde_json::Value::String(h.into());
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
        };

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

        // Atomically swap the active connection (fixes issue #11).
        let new_state =
            ConnectionState::from_iris(new_conn, ConnectionSource::IrisSelectContainer, None);
        {
            let mut conn = self.connection.lock().unwrap();
            *conn = new_state;
        }
        // #169: read the gate AFTER the swap and through the latch, so switching containers
        // cannot report a gate that a later write would not actually get.
        let write_tools_enabled = self.write_tools_enabled();

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
        description = "Return the active IRIS connection state + this MCP server's own version. It only reports the cached connection snapshot (no IRIS network call), so it ALWAYS succeeds — when IRIS is down it returns connected:false / connection_source:disconnected instead of erroring with IRIS_UNREACHABLE. That is the point: it's the one tool you can call to DIAGNOSE an unreachable IRIS (read the `connected` field) without the call itself failing — unlike iris_query/iris_execute/etc., which do return IRIS_UNREACHABLE. If those tools instead return IRIS_AUTH_FAILED (HTTP 401) or IRIS_FORBIDDEN (HTTP 403), IRIS is REACHABLE and the credentials are the problem: read `auth_ok` and `probe_status` here — auth_ok:false means IRIS rejected IRIS_USERNAME/IRIS_PASSWORD (401) or refused this user the %Development privilege (403), and `connected` is false for that reason, not a network one. Also use to: verify hot-reload completed; confirm which container/host is active; validate the loaded MCP build (mcp_version). Hot-reload is operator-driven: an edit to the .iris-agentic-dev.toml at config_watch_path is picked up on the next tool call, and config_loaded_at moves when it happens. A reload can only NARROW the write gate — once write_tools_enabled has gone false it stays false until the server restarts (write_gate_latched reports that), so a reload cannot reopen writes. Fields: mcp_version, toolset, connected, auth_ok, probe_status, connection_source (http|docker|disconnected), host, port, namespace, container, config_file, config_watch_path, config_loaded_at, iris_version, write_tools_enabled, write_gate_latched."
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

        // #101: `connected` was `conn.iris.is_some()` — a claim about whether an object
        // exists, not about IRIS. Under a wrong password it answered `true`, and this tool's
        // own description is what sends a model here to diagnose that. The root probe already
        // knows: a 401/403 means these credentials cannot use this instance, so `connected` is
        // false and `auth_ok` says why. Anything else (no probe yet, a transport failure, an
        // odd status) leaves the old answer alone — cannot-tell must not become a claim.
        let probe_status = conn.iris.as_ref().and_then(|i| i.probe_status);
        let auth_ok = match probe_status {
            Some(401) | Some(403) => Some(false),
            Some(s) if (200..300).contains(&s) => Some(true),
            _ => None,
        };
        // #101/#102, the rest of the same sentence. Keying `connected` on the 401/403 arm
        // alone still left it `true` for a CLOSED PORT, an unroutable host, a wrong
        // IRIS_WEB_PREFIX (probe_status 404) and a sick instance (500) — the exact states
        // this tool's own description promises to report as `connected: false`, and the ones
        // a model is sent here to diagnose. `connected` is a claim about IRIS, so it is
        // answered from what the root probe saw:
        //   2xx                       -> usable
        //   any other status          -> IRIS answered, but this server cannot use it here
        //   probe ran, no response    -> IRIS did not answer
        //   probe never ran           -> cannot tell, and cannot-tell must not become a claim
        let usable = match (
            probe_status,
            conn.iris.as_ref().and_then(|i| i.probe_reached),
        ) {
            (Some(s), _) if (200..300).contains(&s) => Some(true),
            (Some(_), _) => Some(false),
            (None, Some(false)) => Some(false),
            (None, _) => None,
        };
        let mut response = serde_json::json!({
            "connected": conn.iris.is_some() && usable != Some(false),
            "auth_ok": auth_ok,
            "probe_status": probe_status,
            "connection_source": connection_source,
            "host": host,
            "port": port,
            "namespace": namespace,
            "container": container,
            "config_file": config_file,
            "config_loaded_at": config_loaded_at,
            "iris_version": iris_version,
            "write_tools_enabled": conn
                .iris
                .as_deref()
                .map(|c| self.write_gate_open(c))
                .unwrap_or(conn.write_tools_enabled),
            "write_gate_latched": self.write_gate_is_latched(),
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
            Err(e) => {
                // #102: this said IRIS_UNREACHABLE for BOTH a missing namespace and a wrong
                // password — and, because `query_once` never looked at the status, the message
                // was "error decoding response body" (the 404 body is zero bytes, so serde
                // reported EOF and the status was thrown away). Name the namespace when that
                // is the cause, otherwise code the failure from the status IRIS actually sent.
                // Only a genuine transport failure — no response at all — keeps
                // IRIS_UNREACHABLE.
                if let Some(missing) = interop::namespace_missing_error_for(
                    &iris,
                    client,
                    &namespace,
                    "No symbols were read.",
                    &e,
                )
                .await
                {
                    return missing;
                }
                match crate::iris::connection::atelier_status(&e) {
                    Some(http) => envelope::fail_with(
                        envelope::http_status_code(http.status),
                        &e.to_string(),
                        serde_json::json!({ "attempted_url": http.url }),
                    ),
                    None => err_json(
                        interop::classify_iris_error_or(&e.to_string(), "IRIS_REQUEST_FAILED"),
                        &e.to_string(),
                    ),
                }
            }
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
        description = "Introspect an ObjectScript class — returns methods, properties, and type information. Returns only what the class DECLARES by default; pass include_inherited=true to also get everything it inherits (that is usually an order of magnitude more — Ens.Config.Production declares 70 methods and inherits 381)."
    )]
    async fn docs_introspect(
        &self,
        Parameters(p): Parameters<IntrospectParams>,
    ) -> Result<CallToolResult, McpError> {
        let iris = self.get_iris_reloaded().await?;
        let client = self.http_client();
        // Bug 15: use parameterized queries instead of manual string escaping.
        let namespace = interop::resolve_namespace(p.namespace.as_deref(), Some(&iris));
        // #157: `%Foo` is shorthand for `%Library.Foo` and only the expanded name is stored,
        // so the abbreviation reported CLASS_NOT_FOUND for a class that exists and then
        // suggested the wrong package (`%File` -> `%FileMan.*`). Resolve before querying;
        // `expand_percent_class` returns None for anything already qualified.
        let class_name =
            expand_percent_class(&p.class_name).unwrap_or_else(|| p.class_name.clone());
        let expanded_from = (class_name != p.class_name).then(|| p.class_name.clone());
        // #102 P0: both queries used to end in `.unwrap_or_default()`, so ANY failure —
        // a namespace that does not exist, a wrong password, a 500 — became
        // `{"success":true,"methods":null,"properties":null}`. A class that genuinely has no
        // methods answers `methods:[]`, so `null` vs `[]` was the ONLY thing separating
        // "unreachable" from "no methods" and no caller makes that distinction. A failed call
        // must never be answered with a negative FACT.
        // #124: the documented way out of "which members does this class have?" returned every
        // inherited member — a 16 KB median response on the current pin, 78% of them carrying
        // %AddToSaveSet/%BindExport plumbing, and often a note that most of the answer was
        // truncated away. `Origin = parent` is the declared-only filter; it is not interpolated
        // caller input, only this flag.
        let origin_filter = if p.include_inherited {
            ""
        } else {
            " AND Origin = parent"
        };
        let methods = match iris.query(
            &format!("SELECT Name,FormalSpec,ReturnType FROM %Dictionary.CompiledMethod WHERE parent=?{origin_filter} ORDER BY Name"),
            vec![serde_json::Value::String(class_name.clone())],
            &namespace,
            client,
        ).await {
            Ok(v) => v,
            Err(e) => {
                return introspect_failure(&iris, client, &namespace, &class_name, &e).await
            }
        };
        let props = match iris
            .query(
                &format!("SELECT Name,Type FROM %Dictionary.CompiledProperty WHERE parent=?{origin_filter} ORDER BY Name"),
                vec![serde_json::Value::String(class_name.clone())],
                &namespace,
                client,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return introspect_failure(&iris, client, &namespace, &class_name, &e).await
            }
        };
        // #107: "no members" and "no such class" arrived in the same shape —
        // `{"methods":[],"properties":[],"success":true}` — which reads as "this class
        // exists and is empty". An agent introspecting before writing code concludes there
        // is nothing to call and generates against a class that was never there.
        //
        // The check is on the EMPTY path only: a class with any member never pays for it,
        // and a class with none is exactly the case that is ambiguous. `Undetermined` keeps
        // the empty answer rather than inventing a second wrong fact.
        let empty = row_count(&methods) == 0 && row_count(&props) == 0;
        if empty {
            match class_presence(&iris, client, &namespace, &class_name).await {
                ClassPresence::Absent => {
                    let (candidates, in_package) =
                        near_miss_classes(&iris, client, &namespace, &class_name).await;
                    return class_not_found_error(
                        &class_name,
                        &namespace,
                        &candidates,
                        in_package,
                        expanded_from.as_deref(),
                    );
                }
                ClassPresence::DefinedNotCompiled => {
                    return envelope::fail_with(
                        "CLASS_NOT_COMPILED",
                        &format!(
                            "Class '{}' exists in namespace '{}' but is not compiled, so it has \
                             no compiled methods or properties to introspect. Nothing was read.",
                            class_name, namespace
                        ),
                        serde_json::json!({
                            "class_name": class_name,
                            "namespace": namespace,
                            "hint": "Compile it first: iris_compile(target='<class>.cls'). \
                                     docs_introspect reads %Dictionary.CompiledMethod / \
                                     CompiledProperty, which are populated by compilation.",
                        }),
                    );
                }
                ClassPresence::Compiled | ClassPresence::Undetermined => {}
            }
        }
        let mut payload = serde_json::json!({"success": true, "class_name": class_name, "methods": methods["result"]["content"], "properties": props["result"]["content"], "include_inherited": p.include_inherited});
        if let Some(requested) = &expanded_from {
            // #157: say that the name was expanded, or the caller cannot learn the rule.
            payload["requested_class_name"] = serde_json::Value::String(requested.clone());
            payload["resolved"] =
                serde_json::Value::String(format!("'{requested}' is shorthand for '{class_name}'"));
        }
        if !p.include_inherited {
            // #156: the prose note below was PRESENT on all 32 empty responses in the eval
            // corpus and IGNORED by 29 of them — an empty list with a sentence beside it is
            // still an empty list to a model that does not read the sentence, and these
            // agents pass include_inherited=false explicitly, so prose confirming their own
            // choice does not prompt them to revisit it. Give the omission as a NUMBER.
            // Counted only when the declared answer is empty, so a class with members pays
            // nothing — the same rule #107 uses for its existence probe.
            let mut inherited_omitted: Option<i64> = None;
            if empty {
                if let Ok(v) = iris
                    .query(
                        "SELECT COUNT(*) AS N FROM %Dictionary.CompiledMethod WHERE parent=? \
                         UNION ALL \
                         SELECT COUNT(*) AS N FROM %Dictionary.CompiledProperty WHERE parent=?",
                        vec![
                            serde_json::Value::String(class_name.clone()),
                            serde_json::Value::String(class_name.clone()),
                        ],
                        &namespace,
                        client,
                    )
                    .await
                {
                    let total: i64 = v["result"]["content"]
                        .as_array()
                        .map(|rows| rows.iter().filter_map(|r| r["N"].as_i64()).sum())
                        .unwrap_or(0);
                    inherited_omitted = Some(total);
                }
            }
            payload["note"] = serde_json::Value::String(match inherited_omitted {
                Some(n) if n > 0 => format!(
                    "This class declares no members of its own. {n} inherited member(s) were \
                     omitted — pass include_inherited=true to see them."
                ),
                Some(_) => "This class has no members at all, declared or inherited.".to_string(),
                None => "Declared members only — inherited members are omitted. Pass \
                         include_inherited=true for the full surface."
                    .to_string(),
            });
            if let Some(n) = inherited_omitted {
                payload["inherited_omitted"] = serde_json::json!(n);
            }
        }
        ok_json(payload)
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
            "Write ##class(%Studio.Debugger).SourceLine({},{})",
            os_str_expr(&p.routine),
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
            Err(e) => err_json(interop::classify_iris_error_or(&e.to_string(), "IRIS_REQUEST_FAILED"), &e.to_string()),
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
                    err_json(interop::classify_iris_error_or(&msg, "IRIS_REQUEST_FAILED"), &msg)
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
                    err_json(
                        interop::classify_iris_error_or(&msg, "IRIS_REQUEST_FAILED"),
                        &msg,
                    )
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
            "set cls={} set rtn=$translate(cls,\".\",\".\") set map=\"{{\" set first=1 set method=\"\" for {{ set method=$order(^rIndex(rtn,method)) quit:method=\"\"  set intline=$get(^rIndex(rtn,method)) if 'first {{ set map=map_\",\" }} set map=map_\"\\\"\"_method_\"\\\":\\\"\"_intline_\"\\\"\" set first=0 }} set map=map_\"}}\" write map",
            os_str_expr(cls_name)
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
            Err(e) => err_json(interop::classify_iris_error_or(&e.to_string(), "IRIS_REQUEST_FAILED"), &e.to_string()),
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

    // ── ^SKILLS readers (issue #119) ─────────────────────────────────────────
    // All three used to hand-build JSON by concatenating the RAW pipe-delimited value
    // into an array literal, never emitting the subscript (the skill NAME). serde then
    // rejected the payload and every failure fell through to an empty list / NOT_FOUND —
    // indistinguishable from "no IRIS". The assembly now lives in ONE place,
    // `skills_tools::skills_list_json_code` / `skills_describe_json_code`, which uses
    // %DynamicArray + %ToJSON so IRIS does the escaping.

    #[tool(
        description = "List all synthesized skills in the ^SKILLS registry (name, description, usage_count, created_at)."
    )]
    async fn skill_list(&self, _: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        // #85: the namespace comes from the CONNECTION, so take the Arc first. With no
        // connection there is no namespace to resolve and this falls to "USER" — which is
        // honest here, because the very next line reports that IRIS was never reached.
        let iris_opt = self.iris_arc();
        let ns = skills_tools::skills_namespace(iris_opt.as_deref());
        let Some(iris) = iris_opt else {
            return skills_tools::skills_read_fail(
                "skill_list",
                &ns,
                skills_tools::SkillsReadError::Unreachable("no IRIS connection configured".into()),
            );
        };
        let code = skills_tools::skills_list_json_code(None, false);
        match skills_tools::read_skills_json(&iris, &code, &ns).await {
            // #89 follow-up: count via the shared helper, not `unwrap_or(0)`. This is the
            // tool agent_info is meant to agree with, and the old shape would have made the
            // two disagree in the opposite direction — count:0 success:true here against
            // SKILLS_PARSE_FAILED there — for any payload that parsed to a non-array.
            Ok(skills) => match skills_tools::skills_count_from_payload(&skills) {
                Err(e) => skills_tools::skills_read_fail("skill_list", &ns, e),
                Ok(count) => ok_json(serde_json::json!({
                    "success": true, "skills": skills, "count": count,
                    "namespace": ns, "source": "^SKILLS"
                })),
            },
            Err(e) => skills_tools::skills_read_fail("skill_list", &ns, e),
        }
    }

    #[tool(description = "Describe one skill in the ^SKILLS registry by name.")]
    async fn skill_describe(
        &self,
        Parameters(p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        // #85: the namespace comes from the CONNECTION, so take the Arc first. With no
        // connection there is no namespace to resolve and this falls to "USER" — which is
        // honest here, because the very next line reports that IRIS was never reached.
        let iris_opt = self.iris_arc();
        let ns = skills_tools::skills_namespace(iris_opt.as_deref());
        let Some(iris) = iris_opt else {
            return skills_tools::skills_read_fail(
                "skill_describe",
                &ns,
                skills_tools::SkillsReadError::Unreachable("no IRIS connection configured".into()),
            );
        };
        // #119: was `Write $Get(^SKILLS(name))` parsed as JSON — a pipe-delimited string
        // never parses, so this tool returned NOT_FOUND for every skill that existed.
        let code = skills_tools::skills_describe_json_code(&p.name);
        match skills_tools::read_skills_json(&iris, &code, &ns).await {
            Ok(v) if v.get("found").and_then(|f| f.as_i64()) == Some(1) => {
                ok_json(serde_json::json!({
                    "success": true, "name": p.name, "skill": v,
                    "namespace": ns, "source": "^SKILLS"
                }))
            }
            // The `found` sentinel is what separates "IRIS answered, no such skill" from
            // "IRIS never answered" — an empty payload cannot tell them apart.
            Ok(_) => envelope::fail_with(
                "NOT_FOUND",
                &format!(
                    "Skill '{}' not found in ^SKILLS (namespace '{}')",
                    p.name, ns
                ),
                serde_json::json!({"namespace": ns, "source": "^SKILLS"}),
            ),
            Err(e) => skills_tools::skills_read_fail("skill_describe", &ns, e),
        }
    }

    #[tool(
        description = "Search the ^SKILLS registry by name and description. Returns skills whose name or description contains the query (case-insensitive substring)."
    )]
    async fn skill_search(
        &self,
        Parameters(p): Parameters<SkillSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        // #85: the namespace comes from the CONNECTION, so take the Arc first. With no
        // connection there is no namespace to resolve and this falls to "USER" — which is
        // honest here, because the very next line reports that IRIS was never reached.
        let iris_opt = self.iris_arc();
        let ns = skills_tools::skills_namespace(iris_opt.as_deref());
        let Some(iris) = iris_opt else {
            return skills_tools::skills_read_fail(
                "skill_search",
                &ns,
                skills_tools::SkillsReadError::Unreachable("no IRIS connection configured".into()),
            );
        };
        // #67: the needle goes through os_str_expr inside the builder. The old form
        // hand-stripped quotes (`query_lower.replace('"', "")`) and interpolated the
        // result raw into an ObjectScript literal — a query with a newline could not
        // compile at all.
        let query_lower = p.query.to_lowercase();
        let code = skills_tools::skills_list_json_code(Some(&query_lower), false);
        match skills_tools::read_skills_json(&iris, &code, &ns).await {
            Ok(skills) => {
                let all = skills.as_array().cloned().unwrap_or_default();
                let matched = all.len();
                // top_k of 0 used to silently return nothing; clamp to at least 1.
                let limited: Vec<_> = all.into_iter().take(p.top_k.max(1)).collect();
                let count = limited.len();
                ok_json(serde_json::json!({
                    "success": true, "query": p.query, "results": limited,
                    "count": count, "matched": matched,
                    "namespace": ns, "source": "^SKILLS"
                }))
            }
            Err(e) => skills_tools::skills_read_fail("skill_search", &ns, e),
        }
    }

    #[tool(description = "Remove a skill from the registry by name.")]
    async fn skill_forget(
        &self,
        Parameters(p): Parameters<SkillNameParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(iris) = self.iris_arc().as_deref() {
            // #85: kill the skill in the SAME namespace skill_list reads.
            let ns = crate::tools::skills_tools::skills_namespace(Some(iris));
            let code = format!("Kill ^SKILLS({}) Write \"OK\"", os_str_expr(&p.name));
            if iris.execute(&code, &ns).await.is_ok() {
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
            // #89 follow-up: recover the guard rather than reporting an unreadable
            // history as an empty one, matching handle_agent_info(what=history).
            .unwrap_or_else(|e| {
                let h = e.into_inner();
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
            });
        ok_json(serde_json::json!({"calls": calls, "limit": p.limit}))
    }

    // #99: the old description promised "skill count, pattern count, KB size" and delivered
    // none of the three honestly — no pattern count and no KB size at all, and a
    // `skill_count` that was the in-process `--subscribe` registry, not `^SKILLS`. It now
    // says what it returns and, more importantly, what it does NOT do: report a zero it
    // could not measure.
    #[tool(
        description = "Learning agent status. Returns the ^SKILLS skill count for the connection's namespace (skill_count, with namespace + source), the count of skills/KB items loaded from --subscribe GitHub packages (subscribed_skill_count / subscribed_kb_item_count / subscribed_source), the session call count and the learning flag. Reads ^SKILLS, so it FAILS with DOCKER_REQUIRED / SKILLS_PARSE_FAILED if that registry cannot be read — it never reports 0 for a registry it could not read. For session history alone, which needs no IRIS, use agent_history."
    )]
    async fn agent_stats(&self, _: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        // #99: ONE implementation, shared with `agent_info(what=stats)` — the two used to be
        // separate and reported different numbers for the same field in the same session.
        //
        // Deliberately NO `record_call` here, so `agent_stats` and `agent_info(what=stats)`
        // report session_calls that differ by one (agent_info does record itself, at its own
        // call site). Adding it would make this tool inflate the very number it is reporting;
        // the documented off-by-one is the lesser evil, and it is stated in the PR rather
        // than left to be discovered.
        let iris = self.iris_arc();
        skills_tools::agent_stats_result(
            "agent_stats",
            iris.as_deref(),
            &self.history,
            Some(self.registry.as_ref()),
        )
        .await
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
        description = "Inspect a SQL table: returns whether it is a class-projected table or DDL-created, the backing data/index globals, and (optionally) an approximate row count. Works for both class-projected tables (with real storage globals from %Dictionary.CompiledStorage) and DDL tables (globals inferred by IRIS naming convention). Use include_row_count=true to add a COUNT(*) estimate. Accepts either the SQL name (Ens_Config.Item) or the CLASS name (Ens.Config.Item) — the class→table projection is resolved for you, and a miss lists the tables that do exist in that package. Call this (or docs_introspect) to discover the real schema/table/column names BEFORE iris_query, rather than guessing catalog tables."
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
        description = "Find all concrete subclass implementations of a method in the full inheritance hierarchy. Given base class names and a method name, expands to all descendants at any depth and returns classes where the method is defined (Origin = parent, not inherited). Answers the structural question the other introspection tools cannot: WHICH CLASSES DEFINE a method rather than inherit it, across descendants you have not enumerated. docs_introspect takes one class you must already know the name of; iris_symbols will not walk a hierarchy; neither distinguishes a definition from an inherited copy. Example: adapter.Execute() → every EnsLib.*.Adapter subclass that implements Execute. Results cached 60s per session."
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
        description = "IRIS debug tools. action=map_int DECOMPOSES an ObjectScript error frame (<SIGNAL>label+offset^Pkg.Class.1) into signal/class/method/offset and verifies the class is compiled — it returns ROUTINE_NOT_FOUND for a class that does not exist and INVALID_PARAM for a string that is not a frame, and it does NOT return a source line: %Studio.Debugger.SourceLine answers identically for every input on IRIS 2026.1, so `mapped_to_source_line` is false and the note says what to read instead. For an error raised by code you ran through iris_execute, that tool already reports source_line and source_line_number directly — prefer it. action=error_logs fetches recent error log entries. action=capture captures current error state. action=source_map builds a .INT to .CLS mapping, and fails with UNSUPPORTED_IRIS_VERSION where %Studio.Debugger.MapToINT is absent (it is absent on 2026.1)."
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
        description = "Session and learning agent information. what=stats returns the ^SKILLS skill count for the connection's namespace plus the session call count — it FAILS with DOCKER_REQUIRED / SKILLS_PARSE_FAILED if the registry cannot be read, and never reports 0 for a registry it could not read. what=history returns recent tool call history and needs no IRIS connection."
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
        Parameters(p): Parameters<Described<ProductionDispatchSchema>>,
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
        Parameters(p): Parameters<Described<InteropQueryDispatchSchema>>,
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
        // #102: the one interop tool with no `ensure_interop_namespace` pre-flight. Its five
        // impls call `iris.query` directly, so a mistyped namespace surfaced as a raw decode
        // failure and a non-interop namespace as a raw SQL error. One guard here aligns it
        // with iris_production, iris_production_item, iris_message_body,
        // iris_business_rule_info, iris_production_diff, iris_credential_list,
        // iris_credential_manage, iris_lookup_manage and iris_lookup_transfer — and the
        // #93 404 attribution rides along through that helper's Err arm.
        if let Some(iris) = iris_opt {
            let ns =
                interop::resolve_namespace(p.get("namespace").and_then(|v| v.as_str()), Some(iris));
            if let Some(blocked) = interop::ensure_interop_namespace(iris, &ns).await {
                self.record_call("iris_interop_query", false);
                return blocked;
            }
        }
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
                // #143: resolved BEFORE the struct is built so an unusable shape can
                // be refused. Silently returning [] here is what made the dropped
                // projection invisible.
                let body_select = match string_list_arg("body_select", p.get("body_select")) {
                    Ok(v) => v,
                    Err(e) => return crate::tools::envelope::fail_with(
                        "INVALID_PARAM",
                        &e,
                        serde_json::json!({
                            "parameter": "body_select",
                            "expected": "array of column names, e.g. [\"AccessionNumber\",\"ExamCode\"]",
                        }),
                    ),
                };
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
                        body_select,
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
        Parameters(p): Parameters<Described<interop::ProductionItemParams>>,
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
        Parameters(p): Parameters<Described<interop::MessageBodyParams>>,
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
        // #151: read the DECLARED (snake_case) name first — the schema is generated from
        // MessageBodyParams, so that is the only spelling a conformant client can send.
        // The camelCase spelling stays as an alias so existing callers keep working.
        let data_policy = p
            .get("data_policy")
            .or_else(|| p.get("dataPolicy"))
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
                // #151: declared as `acknowledge_phi`, previously read only as
                // `acknowledgePhi` — so the one name a conformant client could discover
                // was the one that did nothing, and the refusal blamed the caller for
                // omitting what it had supplied.
                acknowledge_phi: p
                    .get("acknowledge_phi")
                    .or_else(|| p.get("acknowledgePhi"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                data_policy: data_policy.clone(),
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
        Parameters(p): Parameters<Described<interop::BusinessRuleInfoParams>>,
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
        Parameters(p): Parameters<Described<interop::ProductionDiffParams>>,
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
        Parameters(p): Parameters<Described<interop::CredentialListParams>>,
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
        Parameters(p): Parameters<Described<interop::CredentialManageParams>>,
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
        Parameters(p): Parameters<Described<interop::LookupManageParams>>,
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
        description = "Export or import an Ensemble lookup table as XML. action: export|import. xml: XML string (required for import). namespace: optional — defaults to the connection namespace (IRIS_NAMESPACE); must be an interop-enabled namespace. export always available; import write-gated. IMPORT HAS TWO MODES AND `table` PICKS WHICH: passing table=<name> is a REPLACE — that table is CLEARED first and ends up holding ONLY the entries in the XML, so any row you did not include is DELETED; omitting table is a MERGE — entries are upserted into whatever table(s) the XML names and nothing is deleted. Exporting a table, deleting the bad rows and importing the file back WITHOUT table therefore keeps the rows you meant to remove. The response echoes `mode` (replace|merge) and `entries_applied`. For export, table names the table to export."
    )]
    async fn iris_lookup_transfer(
        &self,
        Parameters(p): Parameters<Described<interop::LookupTransferParams>>,
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
        description = "Retrieve a result a PREVIOUS tool stored, from this server's in-memory result store. This is NOT the IRIS event log — for interoperability logs use iris_interop_query (what=logs, or what=trace with session_id). With id or log_id (the SAME parameter — pass either, or the same value in both — the value a previous result handed back): returns the full result, optionally paginated with limit/offset. With NO id: lists the stored entries (id, tool, timestamp, total_count), paginated with the same limit/offset. Any other parameter name is an error when no id is given, and a warning when one is. Use after any tool returns truncated:true — and after iris_test, which always stores per-test-case detail and returns a log_id even when nothing was truncated."
    )]
    async fn iris_get_log(
        &self,
        Parameters(p): Parameters<GetLogParams>,
    ) -> Result<CallToolResult, McpError> {
        // #78: the body lives in the free function `get_log_impl` so it is unit-testable
        // with nothing but a LogStore — this handler reaches no IRIS connection at all.
        get_log_impl(&self.log_store, p)
    }
}

// `router = self.tool_router` is LOAD-BEARING, not decoration. The macro's default
// router expression is `Self::tool_router()` — a FRESH, UNPRUNED router built on every
// single tools/call. Dispatch therefore never saw toolset pruning or the write gate:
// `remove_route` mutates the instance field, which only our `list_tools` override read.
// The result was a server that hid tools from the listing and happily ran them anyway
// (#104: under `interop`, `iris_search` — not in INTEROP_TOOLS — returned real data over
// the wire; under `baseline`, so did the pruned `iris_get_log`). Binding dispatch to the
// same pruned router makes every `remove_route` in `with_registry_and_toolset`
// authoritative, including the write gate at ~line 2568 that is supposed to keep
// iris_credential_manage / iris_production_item away from a Live instance.
// `toolset_pruning_is_enforced_at_dispatch` in tests/unit/test_toolset.rs guards this.
#[tool_handler(router = self.tool_router)]
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

    /// Override call_tool ONLY to replace rmcp's bare `tool not found` with a message
    /// that says WHY the tool is unreachable. The macro would generate this exact body
    /// minus the pre-check; the routing itself is unchanged.
    ///
    /// Three causes produce the same rmcp error and have three different fixes:
    /// the name is not a tool at all; the tool exists but this toolset prunes it
    /// (restart with --toolset baseline); or it is write-gated off because the
    /// connection is Live (set IRIS_ALLOW_PROD=1, deliberately). Rebuilding the full
    /// router to tell them apart is only on the rejection path, never on a live call.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if !self.tool_router.has_route(&request.name) {
            return Err(self.unreachable_tool_error(&request.name));
        }
        // #114: the write gate, per CALL rather than per tool. Reading is never blocked —
        // this server is for development instances, and the friction of a hidden tool costs
        // more than it protects. A MUTATION on a connection that is not write-allowed is
        // refused here, and only here: the router no longer prunes anything for the gate, so
        // the decision follows the CURRENT connection rather than the one that existed at
        // startup. That matters because the connection can change under a long-lived router
        // — `check_reload` re-probes a changed .iris-agentic-dev.toml and can land on a Live
        // instance, and #110's lazy re-probe adopts a connection into a router that was built
        // while disconnected.
        if let Some(action) = mutating_call(&request.name, &args_of(&request)) {
            if self.connected_but_read_only() {
                return Err(self.write_gated_error(&request.name, action));
            }
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
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
            drop_default_additional_properties(schema);
            drop_struct_name_title(schema);
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }
}

impl IrisTools {
    /// #114: a mutation refused because the connection is not write-allowed. Says what it
    /// would have changed. Reads are never refused, so reaching this means a write was
    /// attempted.
    ///
    /// #169: the escape hatch used to be named here, on the reasoning that a false positive
    /// on a development instance must be a five-second fix and not a mystery. That reasoning
    /// holds for an operator and fails for the reader this envelope actually has: the model,
    /// which is the party the gate exists to constrain. A denial that names the setting that
    /// lifts it is an instruction to lift it. The remediation now goes to stderr, where the
    /// operator reads it and the caller does not.
    fn write_gated_error(&self, tool: &str, action: &str) -> McpError {
        let latched = self.write_gate_is_latched();
        let (mode, namespace) = {
            let c = self.connection.lock().unwrap();
            match c.iris.as_ref() {
                Some(i) => (format!("{:?}", i.system_mode), i.namespace.clone()),
                None => ("Unknown".to_string(), String::new()),
            }
        };
        // Operator-facing remediation, deliberately NOT in the envelope (#169).
        tracing::warn!(
            tool = %tool,
            mode = %mode,
            namespace = %namespace,
            latched = latched,
            "iris-agentic-dev: write refused by the gate. If writing to this instance is \
             intended, set IRIS_ALLOW_PROD=1 and restart the server. A gate that has closed \
             once stays closed until restart."
        );
        McpError::invalid_params(
            format!(
                "'{tool}' would {action} on a connection that is not write-allowed \
                 (system mode {mode}, namespace '{namespace}'), so it was not called. \
                 Reads are never blocked — the read actions of this tool still work."
            ),
            Some(serde_json::json!({
                "error_code": "WRITE_GATED",
                "tool": tool,
                "would": action,
                "system_mode": mode,
                "namespace": namespace,
            })),
        )
    }

    /// The write gate as ENFORCED, after the #169 latch.
    ///
    /// Evaluating it is what arms the latch: any observation of a closed gate closes it for
    /// good. Reporting has to go through here too — `check_config` promising a gate the
    /// enforcement path does not honour is the failure mode upstream shipped as their #110,
    /// where the flag read `false` while every write still landed.
    fn write_gate_open(&self, c: &IrisConnection) -> bool {
        use std::sync::atomic::Ordering;
        if !c.is_write_allowed() {
            self.write_gate_latched.store(true, Ordering::Relaxed);
            return false;
        }
        !self.write_gate_latched.load(Ordering::Relaxed)
    }

    /// True once the gate has ever been observed closed in this process (#169).
    fn write_gate_is_latched(&self) -> bool {
        self.write_gate_latched
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// There IS a connection and it is not write-allowed — a Live system mode, a
    /// production-looking namespace without IRIS_ALLOW_PROD, or the #169 latch. Disconnected
    /// is deliberately NOT this: those calls fail with IRIS_UNREACHABLE anyway, and reporting
    /// them as write-gated would name the wrong problem.
    fn connected_but_read_only(&self) -> bool {
        let iris = { self.connection.lock().unwrap().iris.clone() };
        iris.as_deref().is_some_and(|c| !self.write_gate_open(c))
    }

    /// Explain why `name` did not dispatch. See `call_tool` for why this exists.
    fn unreachable_tool_error(&self, name: &str) -> McpError {
        let exists_unpruned = Self::tool_router().has_route(name);
        if !exists_unpruned {
            return McpError::invalid_params(
                format!("Unknown tool '{name}'. Call tools/list for the tools this server offers."),
                Some(serde_json::json!({
                    "error_code": "UNKNOWN_TOOL",
                    "tool": name,
                    "toolset": self.toolset.as_str(),
                })),
            );
        }
        // A pruned-tool rejection never mentions the write gate: #114 stopped the gate
        // pruning anything, so an unreachable name can only be a toolset decision.
        McpError::invalid_params(
            format!(
                "Tool '{name}' exists but is not part of the '{ts}' toolset, so it was not \
                 called. Restart the server with --toolset baseline (or IRIS_TOOLSET=baseline) \
                 to reach it.",
                ts = self.toolset.as_str()
            ),
            Some(serde_json::json!({
                "error_code": "TOOL_NOT_IN_TOOLSET",
                "tool": name,
                "toolset": self.toolset.as_str(),
            })),
        )
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

/// Issue #78: `#[serde(flatten)]` on GetLogParams makes schemars emit
/// `"additionalProperties": true` — JSON Schema's DEFAULT, so dropping it changes nothing
/// for any validator, and it keeps every advertised inputSchema byte-identical to the
/// pre-#78 one. No tool in either profile emits this keyword today; strict clients are the
/// reason `AnyParams` carries a hand-written JsonSchema impl (see also #113/#115).
/// Top level only — a nested `additionalProperties` would be load-bearing.
fn drop_default_additional_properties(schema: &mut serde_json::Map<String, serde_json::Value>) {
    if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(true)) {
        schema.remove("additionalProperties");
    }
}

/// The Rust struct name schemars puts in the schema's top-level `title` — `GetLogParams`,
/// `CompileParams`, `QueryParams`, … . It is a private identifier that names nothing the
/// caller can act on (the tool carries its own `name`), and it goes out to every client on
/// every tools/list, so it is the same class of leak as a struct-level `///` (#82).
/// Dropped here, with the other wire-only fixups, rather than by renaming 23 structs.
///
/// Top level only: a NESTED `title` can be load-bearing — schemars' default
/// `{"title":"AnyValue"}` is exactly why `AnyParams` carries a hand-written JsonSchema impl
/// (#113/#115).
fn drop_struct_name_title(schema: &mut serde_json::Map<String, serde_json::Value>) {
    schema.remove("title");
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

// ── Atelier /docnames helpers (issue #88) ─────────────────────────────────────

/// Issue #88: compile an iris_compile wildcard target into a case-insensitive anchored
/// regex. `*` is "any run of characters"; EVERY other character is literal. An unbuildable
/// pattern matches NOTHING — the old `.unwrap_or(".*")` fallback would have queued a whole
/// namespace for recompile on a typo.
///
/// The escape has to be total, not just `.`: hand-escaping only the dot left
/// `? ( ) + | ^ $ { } [ ]` reaching the regex engine verbatim, so a target the caller meant
/// literally could match documents it does not name. `A|*` is the catastrophic one —
/// `^A|.*$` anchors the LEFT branch only, so the right branch matches every document in the
/// namespace and iris_compile POSTs a namespace-wide recompile. `regex::escape` first, then
/// translate the (now `\*`) wildcards, so nothing but `*` can ever be a metacharacter.
fn docname_pattern_regex(pattern: &str) -> Option<regex::Regex> {
    let escaped = regex::escape(pattern).replace("\\*", ".*");
    regex::Regex::new(&format!("(?i)^{escaped}$")).ok()
}

/// Issue #88: document names out of an Atelier `/docnames/{cat}` body.
///
/// This build (IRIS 2026.1, Atelier v1) returns `result.content` as an array of OBJECTS —
/// `{"name":"APPPKG.FoundationProduction.cls","cat":"CLS","db":"APP-CODE",…}` — and `name`
/// carries the extension. Older builds returned bare strings. Only the string shape was
/// handled (the comment there claimed objects were impossible), so every element yielded
/// `None`, every wildcard expanded to nothing, and iris_compile answered NOT_FOUND for
/// targets that exist. Both shapes are read now.
fn docnames_in_body(body: &serde_json::Value) -> Vec<&str> {
    body["result"]["content"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    d.as_str()
                        .or_else(|| d.get("name").and_then(|n| n.as_str()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Issue #88: an Atelier document name matches a pattern on its STEM — the name minus its
/// document suffix. `/docnames` names carry the suffix (`APPPKG.Foo.cls`), user patterns
/// usually do not, and matching the suffixed name directly is what let `Pkg.Class.*` compile
/// `Pkg.Class` itself: `^Pkg\.Class\..*$` matched the literal document "Pkg.Class.cls"
/// because the `.cls` filled the `.*`. So a typo'd subpackage came back as a one-class
/// compile instead of NOT_FOUND. Strip, then match.
fn docname_stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext))
            if matches!(
                ext.to_ascii_lowercase().as_str(),
                "cls" | "mac" | "int" | "inc"
            ) =>
        {
            stem
        }
        _ => name,
    }
}

/// Issue #88: the pattern side of `docname_stem`. A caller who spells the extension out
/// (`MyPkg.*.cls`) is naming the same documents as `MyPkg.*`, so the pattern loses its
/// `.cls` too — otherwise it could never match a stem.
///
/// ONLY `.cls`. The listing this expands against is `/docnames/CLS`, so `MyPkg.*.mac` keeps
/// its suffix, matches no stem, and falls through to the NOT_FOUND that tells the caller to
/// compile a routine by its exact name. Stripping it would silently compile the package's
/// CLASSES instead — the wrong documents, reported as success.
fn wildcard_pattern_stem(pattern: &str) -> &str {
    match pattern.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("cls") => stem,
        _ => pattern,
    }
}

/// Issue #88: a wildcard with nothing literal in front of its first `*` — `*`, `*.cls`,
/// `*Foo`. It names no package, so it selects on the tail alone and its expansion is
/// bounded only by the size of the namespace. Refused, never expanded: see
/// `WildcardExpansion::Unqualified`.
fn wildcard_target_is_unqualified(pattern: &str) -> bool {
    matches!(pattern.find('*'), Some(0))
}

/// Issue #94: the literal text before a wildcard's first `*`, when it is safe to hand to
/// the Atelier `?filter=` query parameter.
///
/// Every wildcard compile used to GET the WHOLE `/docnames/CLS` listing: measured on the
/// dev instance at 1,696,950 bytes / 0.294 s server-side / 12,750 documents, ~98% of the
/// cost of the call. `%Api.Atelier.v1:GetDocNames` accepts `filter=X` and turns it into
/// `Name Like '%X%'` inside the same `%Library.RoutineMgr:StudioOpenDialog` query it
/// already runs — 2,066 bytes / 0.040 s for `filter=Ens.Alerting.`.
///
/// The safety argument is the SUPERSET INVARIANT: `LIKE '%X%'` is a *contains* match and
/// `docname_pattern_regex` is anchored on the same literal prefix, so every name the client
/// regex would have matched necessarily contains that prefix and survives the server
/// filter. The server can only over-deliver; the client-side regex still does the real
/// selection, so `Matched` / `TooBroad` / `NOT_FOUND` are bit-for-bit unchanged. (Measured:
/// the filter is case-insensitive, matching the regex's `(?i)`.)
///
/// `%` and `_` are PERMITTED. In `LIKE` they are wildcards, so they can only broaden the
/// match, never narrow it — and `%Pkg.*` needs its leading `%` to reach a system package.
/// A quote must never get through: `GetDocNames` builds that `Name Like` clause by STRING
/// CONCATENATION, and a probe with `filter=%27` returned 200 with an empty content array —
/// i.e. an unguarded quote would narrow WRONGLY rather than fail loudly. Hence an allowlist
/// rather than an escape, and a rejected prefix means "fetch the whole listing" (today's
/// behaviour), never "narrow it wrongly".
fn wildcard_listing_filter(pattern: &str) -> Option<&str> {
    let prefix = &pattern[..pattern.find('*')?];
    if prefix.is_empty() {
        // Unqualified — already refused before any fetch by `wildcard_target_is_unqualified`.
        return None;
    }
    prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '-'))
        .then_some(prefix)
}

/// Issue #88: the most documents ONE wildcard compile may queue.
///
/// Picked from the shape of a real namespace, not as a round number. The dev instance's APP
/// namespace lists 12744 CLS documents, 10094 of them outside the `%` system packages —
/// and 10094 is exactly what `iris_compile {"target":"*"}` expanded to and POSTed to
/// /action/compile in ONE request, from a typo, with no dry run and no confirmation.
///
/// Across the 1726 distinct two-level packages in that namespace's class dictionary,
/// exactly TWO hold more than 500: the vendor FHIR trees, HS.FHIR (which this guard now
/// measures at 3693 documents) and HS.FHIRModel (1081). EVERY other two-level package in
/// the namespace holds at most 378. So 500 takes whole any package a developer actually
/// authors, and refuses only whole-library trees and whole-namespace expansions.
///
/// Deliberately no `force`/`confirm` escape hatch: that would widen the advertised schema
/// of a tool in the locked 23-tool interop profile, and a caller who genuinely wants 500+
/// classes can name the subpackages.
const WILDCARD_EXPANSION_CAP: usize = 500;

/// Issue #88: what a wildcard compile target expanded to. The two refusals are outcomes,
/// not errors, so the guard is a pure function the unit tests can drive with no IRIS.
#[derive(Debug, PartialEq)]
pub(crate) enum WildcardExpansion {
    /// Nothing literal precedes the first `*` — refused before the listing is even fetched.
    Unqualified,
    /// More than `WILDCARD_EXPANSION_CAP` documents matched. Carries the real count so the
    /// error can state it.
    TooBroad { matched: usize },
    /// The documents to compile, suffix included. EMPTY is a genuine miss and stays
    /// NOT_FOUND — a typo must never become a compile.
    Matched(Vec<String>),
}

/// Issue #88: expand a wildcard compile target against the names in a /docnames body.
///
/// `%`-prefixed system documents are reachable only when the pattern itself starts with
/// `%` — otherwise a package wildcard would drag library classes in behind it.
fn expand_wildcard_target(names: &[&str], pattern: &str) -> WildcardExpansion {
    if wildcard_target_is_unqualified(pattern) {
        return WildcardExpansion::Unqualified;
    }
    // #100: the selector is shared with `not_listable_crosscheck` so the two can never
    // disagree about what a pattern names. `docname_stem` is applied HERE and only here:
    // /docnames names carry their extension, %Dictionary.ClassDefinition names do not.
    let (re, want_system) = match wildcard_matcher(pattern) {
        Some(m) => m,
        None => return WildcardExpansion::Matched(Vec::new()),
    };
    let matched: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| want_system || !n.starts_with('%'))
        .filter(|n| re.is_match(docname_stem(n)))
        .collect();
    if matched.len() > WILDCARD_EXPANSION_CAP {
        return WildcardExpansion::TooBroad {
            matched: matched.len(),
        };
    }
    WildcardExpansion::Matched(matched.into_iter().map(str::to_string).collect())
}

/// Issue #88: the refusal for a pattern with no package in front of its `*`.
fn unqualified_wildcard_error(pattern: &str, namespace: &str) -> Result<CallToolResult, McpError> {
    envelope::fail_with(
        "SCOPE_REQUIRED",
        &format!(
            "iris_compile: target '{pattern}' has nothing before its first '*', so it names \
             no package and would select on the tail alone — in namespace {namespace} that \
             is up to every class the namespace holds, compiled in one request. Qualify it \
             with a package: 'MyApp.*', 'MyApp.Sub.*.cls', or a single document by name."
        ),
        serde_json::json!({
            "target": pattern,
            "namespace": namespace,
            "hint": "A wildcard compile target must start with a literal package prefix. \
                     Nothing was compiled.",
        }),
    )
}

/// Issue #88: the refusal for a qualified pattern that still selects too much.
fn too_broad_wildcard_error(
    pattern: &str,
    namespace: &str,
    matched: usize,
) -> Result<CallToolResult, McpError> {
    envelope::fail_with(
        "TOO_BROAD",
        &format!(
            "iris_compile: target '{pattern}' matches {matched} documents in namespace \
             {namespace} — more than the {WILDCARD_EXPANSION_CAP} one wildcard compile may \
             queue. Nothing was compiled. Name a narrower package (add the next level: \
             'Pkg.Sub.*') or compile the documents one at a time."
        ),
        serde_json::json!({
            "target": pattern,
            "namespace": namespace,
            "matched": matched,
            "limit": WILDCARD_EXPANSION_CAP,
            "hint": "Narrow the pattern — there is no override flag; a compile this size is \
                     a namespace-wide recompile, which is what this guard exists to stop.",
        }),
    )
}

// ── Issue #100: the classes an Atelier listing cannot see ─────────────────────

/// Issue #100: the ONE selector a wildcard compile target uses, so the `/docnames` expansion
/// and the `%Dictionary.ClassDefinition` cross-check below can never select differently.
///
/// Returns the anchored, case-insensitive regex for the pattern's stem plus whether the
/// caller asked for `%`-prefixed system documents (only a pattern that itself starts with
/// `%` reaches those — otherwise a package wildcard would drag library classes in behind
/// it). `None` means the pattern cannot be compiled to a regex at all, which selects
/// NOTHING; never `.*`.
///
/// TRAP — the two call sites legitimately differ on STEMMING, and getting it wrong is
/// silent. `/docnames` names carry their extension (`Pkg.Foo.cls`), so
/// `expand_wildcard_target` must run each name through `docname_stem` before matching.
/// Names out of `%Dictionary.ClassDefinition` do NOT carry one, so they are matched
/// DIRECTLY: `docname_stem` strips a trailing `.cls`/`.mac`/`.int`/`.inc`, and a real class
/// named `Pkg.Int` would be mangled to `Pkg` and then fail to match its own pattern
/// `Pkg.*`. Asserted, not merely commented — see
/// `the_shared_matcher_selects_identically_for_docnames_and_sql_names`.
fn wildcard_matcher(pattern: &str) -> Option<(regex::Regex, bool)> {
    let re = docname_pattern_regex(wildcard_pattern_stem(pattern))?;
    Some((re, pattern.starts_with('%')))
}

/// Issue #100: a `*` wildcard pattern as a SQL `LIKE` pattern, under `ESCAPE '\'`.
///
/// The pattern's own `\`, `%` and `_` are escaped FIRST and only then is `*` translated to
/// `%`, so the SQL selection stays a superset of what `wildcard_matcher`'s regex selects and
/// never a WIDER one. That ordering is the whole point: `%Library.*` left unescaped becomes
/// `LIKE '%Library.%'`, a leading-wildcard *contains* match over the entire dictionary, and
/// with a `TOP` cap in front of it the real match can be truncated away before the client
/// regex ever sees it — inventing a brand-new false "no such class" inside the very fix that
/// exists to remove one. Verified live: `\%Library.%` returns %Library classes only, while
/// `%Library.%` returns the whole alphabet.
///
/// The `ESCAPE '\'` idiom is already used in this crate (mod.rs `probe_sql`, info.rs,
/// interop.rs), so this is not a new technique here.
fn like_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    for c in pattern.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '%' => out.push_str(r"\%"),
            '_' => out.push_str(r"\_"),
            '*' => out.push('%'),
            _ => out.push(c),
        }
    }
    out
}

/// Issue #100: one class that exists in `%Dictionary.ClassDefinition` but that Atelier's
/// `/docnames/CLS` listing does not offer.
#[derive(Debug, PartialEq)]
pub(crate) struct NotListable {
    pub(crate) name: String,
    pub(crate) hidden: bool,
    /// The generator's document name (`Ens.Alerting.AlertManager.CLS`), or `None` when the
    /// class was authored. This is why `GeneratedBy` is selected at all: the advice SPLITS
    /// on it — see `not_listable_advice`.
    pub(crate) generated_by: Option<String>,
}

/// Issue #100: what the `%Dictionary.ClassDefinition` cross-check concluded.
///
/// Three states, never two. `Unavailable` exists because #89/#119 keep being reintroduced by
/// folding "we could not check" into "there is nothing": a cross-check that failed must not
/// be reported as an empty result, or the new code carries the old bug.
#[derive(Debug, PartialEq)]
pub(crate) enum CrossCheck {
    /// Classes matching the pattern that the listing did not offer. `truncated` means the
    /// row cap was hit, so the count is a floor.
    Found {
        rows: Vec<NotListable>,
        truncated: bool,
    },
    /// The query ran, and nothing anywhere matches. The ONLY state that may say "no such
    /// class".
    NoSuchClass,
    /// The query could not be run or could not be read. NOT a verdict.
    Unavailable(String),
}

/// Issue #100: the most rows the cross-check will name. One more than this is fetched so a
/// full page can be reported as a floor rather than as the truth.
const NOT_LISTABLE_ROW_CAP: usize = 100;

/// Issue #100: the most class names the cross-check prints into its message.
const NOT_LISTABLE_NAMES_SHOWN: usize = 20;

/// Issue #100: does a class matching `pattern` exist even though the CLS listing did not
/// offer one?
///
/// `%Api.Atelier.v1:GetDocNames` runs `%Library.RoutineMgr:StudioOpenDialog`, which omits
/// precisely `Hidden = 1 OR GeneratedBy <> ''`. Measured on the dev instance, per package,
/// as (ClassDefinition rows / listed / delta / rows with `Hidden=1 OR GeneratedBy<>''`):
/// `EnsPortal*` 251/224/27/27, `EnsLib.HL7*` 67/58/9/9, `Ens.Alerting*` 23/16/7/7,
/// `EnsPortal.I*` 2/0/2/2. Passing `ShowGenerated=1` does NOT recover them (`Ens.Alerting*`
/// stays 16 either way), so no Atelier flag can fix this — a second source is the only route.
///
/// SOURCE TABLE: `%Dictionary.ClassDefinition` alone. `%Dictionary.CompiledClass` returned
/// identical counts for all three affected packages on this instance (67 / 251 / 2), so
/// unioning it would exactly double the cost of a path chosen for being cheap. That is a
/// measurement, not a preference — do not "improve" it back without re-measuring.
///
/// COST, measured end-to-end through the tool against the dev instance, not server-side:
/// the empty wildcard path went from 39–43 ms to 53–64 ms, i.e. +20 ms / roughly +50%, on a
/// call that has already failed and compiled nothing. The query itself is ~22 ms warm
/// through the full MCP + HTTP round trip. That is MORE than the +5–11 ms a server-side-only
/// measurement predicts — the number here is the one a caller actually feels, and the
/// difference is the round trip plus the index trade-off below. It is worth paying: it turns
/// a confident wrong answer into a correct one that names the classes. The happy path pays
/// ZERO — it never reaches this function — pinned by
/// `the_cross_check_never_runs_on_the_happy_path`.
///
/// `LIKE` on the WHOLE stem, not `%STARTSWITH` on the literal prefix: a mid-pattern `*`
/// (`EnsPortal.*Maps`) under a prefix query returns `EnsPortal.…` in alphabetical order and
/// the row cap could truncate the real match away — a new false negative. Pushing the whole
/// pattern into `LIKE` makes the cap a genuine "too many to name".
///
/// `UPPER(Name) LIKE UPPER(?)` AND NOT the cheaper `Name LIKE ?`, which is the one place
/// this function knowingly gives up an index. Measured on this instance: plain `LIKE` 5.8–6.8
/// ms, `UPPER(...)` 15.9–23.5 ms. The plain form is CASE-SENSITIVE — verified live,
/// `Name LIKE 'ensportal.i%'` returns 0 rows where `'EnsPortal.I%'` returns 2, and
/// `%STARTSWITH 'ensportal.i'` returns 0 as well — while `wildcard_matcher`'s regex is
/// `(?i)` and the /docnames expansion it is checking is case-insensitive too. A cross-check
/// NARROWER than the selection it checks can only manufacture false negatives, which is the
/// exact bug class this whole change exists to remove. So the ~16 ms is bought deliberately.
/// Do not "optimise" the UPPER away without re-reading this paragraph.
async fn not_listable_crosscheck(
    iris: &IrisConnection,
    client: &reqwest::Client,
    pattern: &str,
    namespace: &str,
) -> CrossCheck {
    let Some((re, want_system)) = wildcard_matcher(pattern) else {
        // A pattern that compiles to no regex selects nothing anywhere — the expansion
        // already reached the same conclusion by the same route.
        return CrossCheck::NoSuchClass;
    };
    let like = like_pattern(wildcard_pattern_stem(pattern));
    // Parameterised, via the existing `/action/query` path — no interpolation, no embedded
    // ObjectScript, so neither the #67 escaping rule nor the postconditional rule gains any
    // new surface here.
    let sql = format!(
        "SELECT TOP {} Name, Hidden, GeneratedBy FROM %Dictionary.ClassDefinition \
         WHERE UPPER(Name) LIKE UPPER(?) ESCAPE '\\' ORDER BY Name",
        NOT_LISTABLE_ROW_CAP + 1
    );
    let body = match iris
        .query(
            &sql,
            vec![serde_json::Value::String(like)],
            namespace,
            client,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => return CrossCheck::Unavailable(e.to_string()),
    };
    let Some(content) = body["result"]["content"].as_array() else {
        return CrossCheck::Unavailable(
            "the /action/query response carried no result.content array".into(),
        );
    };
    let truncated = content.len() > NOT_LISTABLE_ROW_CAP;
    let rows: Vec<NotListable> = content
        .iter()
        .filter_map(|r| {
            let name = r["Name"].as_str()?;
            // NOT `docname_stem`: these names carry no extension. See `wildcard_matcher`.
            if (!want_system && name.starts_with('%')) || !re.is_match(name) {
                return None;
            }
            Some(NotListable {
                name: name.to_string(),
                // Atelier's JSON renders the boolean column as `true`/`false`; older shapes
                // send 1/0. Accept both rather than silently reading "not hidden".
                hidden: r["Hidden"].as_bool().unwrap_or(false)
                    || matches!(r["Hidden"].as_i64(), Some(n) if n != 0),
                generated_by: r["GeneratedBy"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            })
        })
        .collect();
    match (rows.is_empty(), truncated) {
        // The cap was hit AND the client re-filter kept nothing: the rows that would match
        // may have been truncated away before we ever saw them. That is "cannot tell", not
        // "does not exist" — the whole reason `Unavailable` exists.
        (true, true) => CrossCheck::Unavailable(format!(
            "the cross-check hit its {NOT_LISTABLE_ROW_CAP}-row cap and none of the rows it \
             saw matched, so a matching class may have been truncated away"
        )),
        (true, false) => CrossCheck::NoSuchClass,
        (false, _) => CrossCheck::Found { rows, truncated },
    }
}

/// Issue #100: what to tell the caller about classes the listing cannot reach.
///
/// The advice SPLITS, and the issue's own suggested wording gets one half wrong. Verified
/// live on the dev instance:
///   `iris_compile {"target":"EnsPortal.InterfaceMaps.cls"}` -> success, targets_compiled 1
///   `iris_compile {"target":"Ens.Alerting.AlertManagerMessagesReceived.cls"}`
///        -> COMPILE_ERROR "ERROR #5202: Nothing to compile"
/// "Compile it by exact name" is right for a Hidden-but-authored class and WRONG for a
/// generated one (`GeneratedBy = "Ens.Alerting.AlertManager.CLS"`), which has no source of
/// its own — its generator must be compiled instead. The cross-check already has to read
/// `GeneratedBy` to say this, and it arrives in the same row for free.
fn not_listable_advice(rows: &[NotListable]) -> String {
    let mut out = String::new();
    let authored: Vec<&str> = rows
        .iter()
        .filter(|r| r.generated_by.is_none())
        .map(|r| r.name.as_str())
        .take(NOT_LISTABLE_NAMES_SHOWN)
        .collect();
    if !authored.is_empty() {
        out.push_str(&format!(
            " Compile these by exact name: {}.",
            authored.join(", ")
        ));
    }
    for r in rows
        .iter()
        .filter(|r| r.generated_by.is_some())
        .take(NOT_LISTABLE_NAMES_SHOWN)
    {
        let gen = r.generated_by.as_deref().unwrap_or_default();
        out.push_str(&format!(
            " {} is generated by {} — compiling {} by name fails with ERROR #5202: Nothing \
             to compile; compile {} instead.",
            r.name,
            docname_stem(gen),
            r.name,
            docname_stem(gen)
        ));
    }
    out
}

/// Issue #100 (and #88/#94 before it): the NOT_FOUND a wildcard compile returns when the
/// listing matched nothing.
///
/// Pure, so the wording and — far more importantly — the payload RULES are testable with no
/// IRIS. The rules, in the order they matter:
///
/// 1. The sentence is about the LISTING, never about the world. `/docnames/CLS` omits Hidden
///    and generated classes, so "no documents match" was asserting a negative the listing
///    cannot support. Reproduced live: `iris_compile {"target":"EnsPortal.I*"}` answered
///    NOT_FOUND while `EnsPortal.InterfaceMaps` existed and compiled fine by exact name.
/// 2. `error_code` stays NOT_FOUND in every branch. Nothing was compiled, so it must remain
///    an error, and callers branch on the code. The new information rides on `reason` /
///    `crosscheck` / `not_listable`, the way #94 added `listing_narrowed` beside prose
///    instead of minting a code.
/// 3. `reason` exists ONLY when `crosscheck == "ok"`, and `not_listable` ONLY when the
///    cross-check actually ran. A failed cross-check emitting `not_listable: []` would be
///    #89 rewritten in a new function.
fn compile_not_found_error(
    target: &str,
    namespace: &str,
    listing_filter: Option<&str>,
    listing_narrowed: bool,
    scanned: usize,
    cross: CrossCheck,
) -> Result<CallToolResult, McpError> {
    // #94: `scanned` STOPS meaning "documents in the namespace" the moment the listing is
    // narrowed server-side, so the two cases get two sentences. Left alone, this would say
    // "scanned 0 CLS document(s) in namespace APP" for a typo'd package — which reads as
    // "the namespace is empty".
    let mut msg = match listing_filter {
        Some(f) => format!(
            "iris_compile: the Atelier CLS listing for namespace {namespace} — narrowed \
             server-side to names containing '{f}' — returned {scanned} candidate \
             document(s), none matching '{target}'."
        ),
        None => format!(
            "iris_compile: scanned {scanned} CLS document(s) in namespace {namespace}; none \
             matches '{target}'."
        ),
    };
    // The floor, present in every branch and costing nothing: what the listing cannot see.
    msg.push_str(
        " That listing (/docnames/CLS) does not include Hidden or generated classes, so a \
         wildcard can never reach those — a class can still be compiled by its exact name, \
         which needs no listing.",
    );

    let mut extra = serde_json::json!({
        "pattern": target,
        "namespace": namespace,
        // Machine-readable, so the distinction is not only in the prose.
        "listing_narrowed": listing_narrowed,
        "listing_filter": listing_filter,
        "candidates_scanned": scanned,
        // Static, zero-cost, and the reason this NOT_FOUND is not proof of absence.
        "listing_source": "atelier /docnames/CLS",
        "listing_omits": ["Hidden", "generated"],
    });

    match cross {
        CrossCheck::Found { rows, truncated } => {
            let shown: Vec<&str> = rows
                .iter()
                .map(|r| r.name.as_str())
                .take(NOT_LISTABLE_NAMES_SHOWN)
                .collect();
            let count = if truncated {
                format!("at least {}", rows.len())
            } else {
                rows.len().to_string()
            };
            msg = format!(
                "iris_compile: no LISTABLE class matches '{target}' in namespace \
                 {namespace}, but %Dictionary.ClassDefinition holds {count} class(es) that \
                 do: {}{}. Atelier's /docnames/CLS omits Hidden and generated classes, \
                 which is why the wildcard cannot reach them.{}",
                shown.join(", "),
                if rows.len() > shown.len() {
                    format!(" (showing {} of {count})", shown.len())
                } else {
                    String::new()
                },
                not_listable_advice(&rows),
            );
            extra["reason"] = serde_json::json!("not_listable");
            extra["crosscheck"] = serde_json::json!("ok");
            extra["not_listable_total"] = serde_json::json!(rows.len());
            extra["not_listable_truncated"] = serde_json::json!(truncated);
            extra["not_listable"] = serde_json::Value::Array(
                rows.iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r.name,
                            "hidden": r.hidden,
                            "generated_by": r.generated_by,
                        })
                    })
                    .collect(),
            );
        }
        CrossCheck::NoSuchClass => {
            msg.push_str(
                " %Dictionary.ClassDefinition — which DOES include Hidden and generated \
                 classes — holds no match either, so this is a genuine miss.",
            );
            extra["reason"] = serde_json::json!("no_such_class");
            extra["crosscheck"] = serde_json::json!("ok");
            extra["not_listable"] = serde_json::json!([]);
        }
        CrossCheck::Unavailable(why) => {
            // #89/#119 on the NEW path. "We could not check" must never be rendered as
            // "it does not exist": no `reason`, and no empty `not_listable` array to be
            // misread as one.
            msg.push_str(&format!(
                " The %Dictionary.ClassDefinition cross-check that would settle whether a \
                 Hidden or generated class matches could not be run ({why}), so this is not \
                 proof that no such class exists."
            ));
            extra["crosscheck"] = serde_json::json!("unavailable");
            extra["crosscheck_error"] = serde_json::json!(why);
        }
    }

    msg.push_str(
        " Wildcards expand CLASS documents only: compile a .mac/.int/.inc routine by its \
         exact name. Nothing was compiled.",
    );
    crate::tools::envelope::fail_with("NOT_FOUND", &msg, extra)
}

// ── Compile-console diagnostics (issue #80) ───────────────────────────────────

/// One diagnostic parsed out of an Atelier compile `console` line (issue #80).
pub(crate) struct ConsoleDiag {
    pub(crate) code: String,
    pub(crate) line: u32,
    pub(crate) location: String,
    pub(crate) text: String,
}

/// Issue #80: is this console diagnostic already in `errors` from `status.errors`?
///
/// A plain `contains` was too greedy. The `#5475` wrapper IRIS puts in `status.errors`
/// embeds the FIRST per-method message verbatim inside a multi-line blob, so containment
/// against it swallowed exactly one method's diagnostic while every later one survived:
/// N broken methods yielded N-1 routine-level entries, and a consumer counting them was
/// quietly off by one. Only a SINGLE-LINE entry can stand in for a console line — that is
/// the case the dedup was written for (one status error repeating one console line, minus
/// its `ERROR ` prefix). A multi-line wrapper is a summary of many, never a substitute.
fn console_diag_already_reported(errors: &[serde_json::Value], text: &str) -> bool {
    errors.iter().any(|e| {
        e["text"]
            .as_str()
            .map(|t| !t.contains('\n') && t.contains(text))
            .unwrap_or(false)
    })
}

/// A console line that is only a document name. IRIS repeats `ERROR: Foo.Bar.cls` once per
/// routine error as a header; it carries no diagnostic of its own.
fn is_bare_docname(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    (lower.ends_with(".cls")
        || lower.ends_with(".mac")
        || lower.ends_with(".int")
        || lower.ends_with(".inc"))
        && !s.contains(' ')
        && !s.contains('(')
        && !s.contains(':')
}

/// Issue #80: parse one compile-console line, accepting BOTH the `KEYWORD:` and the
/// `KEYWORD ` prefix.
///
/// This build emits per-method errors with a COLON —
///   `ERROR: Foo.Bar.cls(M1+2) #1002: Invalid character in tag : '$$$X' : Offset:15 […]`
///   `ERROR:  Foo.Bar.1(3) : MPP5610 : Referenced macro not defined: 'X'`
/// while the parser only matched `ERROR ` (space), so the 24 real per-method errors of a
/// 12-broken-method class collapsed into the single `status.errors` wrapper entry and the
/// agent had to recompile once per macro to discover them.
///
/// `text` keeps the WHOLE line after the prefix: the old `splitn(3, ':')` re-slicing
/// mangled any message that itself contained a colon (measured on
/// `ERROR #5373: Class 'X', used by 'Y:property:P1', does not exist`, which it cut down to
/// `property:P1', does not exist`).
pub(crate) fn parse_console_diag(
    raw: &str,
    colon_prefix: &str,
    space_prefix: &str,
) -> Option<ConsoleDiag> {
    let t = raw.trim();
    let rest = t
        .strip_prefix(colon_prefix)
        .or_else(|| t.strip_prefix(space_prefix))?
        .trim();
    if rest.is_empty() || is_bare_docname(rest) {
        return None;
    }
    let paren = match (rest.find('('), rest.find(')')) {
        (Some(open), Some(close)) if open < close => {
            // Only a BARE document/routine name immediately followed by `(` is a location.
            // A parenthesis inside prose (`Class 'X' (generated)`) is not.
            let head = &rest[..open];
            if head.is_empty() || head.contains(' ') || head.contains(':') {
                None
            } else {
                Some((head, &rest[open + 1..close], rest[close + 1..].trim_start()))
            }
        }
        _ => None,
    };
    let (code, line, location) = match paren {
        Some((head, inner, tail)) => {
            // `Foo.Bar.1(3) : MPP5610 : msg` — a routine line number, so the routine name
            // is the location. `Foo.Bar.cls(M1+2) #1002: msg` — a label+offset, which IS
            // the location and has no line number.
            let (line, location) = match inner.trim().parse::<u32>() {
                Ok(n) => (n, head.to_string()),
                Err(_) => (0, inner.trim().to_string()),
            };
            let code = if let Some(after) = tail.strip_prefix('#') {
                after.split(':').next().unwrap_or("").trim().to_string()
            } else if let Some(after) = tail.strip_prefix(':') {
                after.split(':').next().unwrap_or("").trim().to_string()
            } else {
                String::new()
            };
            (code, line, location)
        }
        None => {
            // Classic space-prefixed shapes: `#5373: msg` and `#5001:12: msg`.
            let mut parts = rest.trim_start_matches('#').split(':');
            let code = parts
                .next()
                .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit()))
                .unwrap_or("")
                .to_string();
            let line = if code.is_empty() {
                0
            } else {
                parts
                    .next()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(0)
            };
            (code, line, String::new())
        }
    };
    Some(ConsoleDiag {
        code,
        line,
        location,
        text: rest.to_string(),
    })
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
    use super::drop_default_additional_properties;
    use super::drop_struct_name_title;
    use super::normalize_schema_openapi3;
    use super::GetLogParams;
    use super::DOCKER_REQUIRED_HINT;
    use super::GET_LOG_VALID_PARAMS;

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

    // ── Issue #78: GetLogParams' advertised schema ───────────────────────────

    /// Issue #82: `log_id` must be a DECLARED property, not just a serde alias — a strict
    /// or filtering client can only emit what the schema names, and `log_id` is the key
    /// every truncating tool hands back. The flattened leftover-capture map must still not
    /// leak a property, and the one keyword it does add (`additionalProperties: true`, JSON
    /// Schema's default) must still be stripped on the wire — no tool in either profile
    /// emits that keyword, and strict clients are why `AnyParams` carries a hand-written
    /// JsonSchema impl (#113/#115).
    #[test]
    fn get_log_input_schema_declares_log_id() {
        let schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        let props = schema["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["id", "limit", "log_id", "offset"],
            "#82: log_id must be declared; the #[serde(flatten)] capture and the #81 issue \
             log must not surface as caller-facing parameters"
        );
        assert!(
            props["id"]["description"]
                .as_str()
                .unwrap()
                .contains("log_id"),
            "the alias must be documented where the agent reads it: {}",
            props["id"]["description"]
        );
        assert!(
            props["log_id"]["description"]
                .as_str()
                .unwrap()
                .contains("id"),
            "log_id's description must say it is the same parameter as id: {}",
            props["log_id"]["description"]
        );
        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(true),
            "schemars emits exactly this for a flattened map — if that changes, revisit \
             drop_default_additional_properties"
        );

        let mut obj = schema.as_object().unwrap().clone();
        drop_default_additional_properties(&mut obj);
        assert!(
            obj.get("additionalProperties").is_none(),
            "the advertised schema must stay byte-identical to the pre-#78 one"
        );
    }

    /// The published schema must agree with what the runtime accepts. Two disagreements
    /// shipped with #82/#83, and both punish exactly the strict client those issues exist
    /// to serve: `limit` advertised `minimum: 0` while the runtime answers INVALID_PARAMS
    /// for 0, and `offset` was the ONE declared property that was not nullable — so
    /// `{"id":null,"log_id":"x","limit":null,"offset":null}`, the payload a client that
    /// puts every declared property in `required` produces, violated the very schema it
    /// was validating against.
    #[test]
    fn get_log_schema_agrees_with_the_runtime_on_limit_and_offset() {
        let mut schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        let obj = schema.as_object_mut().unwrap();
        normalize_schema_openapi3(obj);
        let props = obj["properties"].clone();

        let limit = &props["limit"]["anyOf"][0];
        assert_eq!(limit["type"], "integer", "{limit}");
        assert!(
            limit["minimum"].as_u64() == Some(1) || limit["exclusiveMinimum"].as_u64() == Some(0),
            "the runtime rejects limit 0, so the schema must too: {limit}"
        );

        for name in GET_LOG_VALID_PARAMS {
            let branches = props[*name]["anyOf"].as_array().unwrap_or_else(|| {
                panic!(
                    "`{name}` must be nullable, so it must normalise to anyOf: {}",
                    props[*name]
                )
            });
            assert!(
                branches.iter().any(|b| b["type"] == "null"),
                "`{name}` must accept null — a strict client sends null for every declared \
                 property it is not using: {}",
                props[*name]
            );
        }
    }

    /// schemars puts the Rust struct name in the schema's top-level `title`, and every
    /// tools/list ships it to every client. It names nothing the caller can act on.
    #[test]
    fn the_advertised_schema_does_not_ship_the_rust_struct_name() {
        let mut schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        let obj = schema.as_object_mut().unwrap();
        assert_eq!(
            obj["title"], "GetLogParams",
            "schemars stopped emitting the struct name — if so, drop_struct_name_title is \
             dead code and can go"
        );
        drop_struct_name_title(obj);
        assert!(obj.get("title").is_none(), "{obj:?}");
        // Nested titles are load-bearing (schemars' `{"title":"AnyValue"}` is why AnyParams
        // has a hand-written impl) — only the top level is dropped.
        let mut nested = serde_json::json!({"properties": {"x": {"title": "AnyValue"}}});
        let obj = nested.as_object_mut().unwrap();
        drop_struct_name_title(obj);
        assert_eq!(obj["properties"]["x"]["title"], "AnyValue");
    }

    /// Issue #82, the part that reached the wire by accident: schemars promotes a
    /// STRUCT-level `///` into the schema's top-level `description`, and `inputSchema` is
    /// shipped to every client on every tools/list. The #82 rationale sat there as 761
    /// characters naming serde, schemars, `drop_default_additional_properties` and issue
    /// numbers — context spent on every call, and the only top-level description in the
    /// interop 23. Per-property docs are caller-facing and stay; the rationale is a `//`
    /// comment in the source.
    #[test]
    fn get_log_schema_ships_no_maintainer_commentary() {
        let schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        assert!(
            schema.get("description").is_none(),
            "a struct-level `///` became wire traffic — make it `//`: {}",
            schema["description"]
        );
        let text = schema.to_string();
        for jargon in [
            "serde",
            "schemars",
            "Deserialize",
            "drop_default_additional_properties",
            "GetLogIssue",
        ] {
            assert!(
                !text.contains(jargon),
                "the advertised schema names the Rust internal '{jargon}': {text}"
            );
        }
        // …and the caller-facing property docs are still there.
        assert!(schema["properties"]["limit"]["description"].is_string());
    }

    /// `normalize_schema_openapi3` lists "additionalProperties" among the keys it relocates
    /// into an `anyOf` branch — but only where the schema has a nullable TYPE ARRAY.
    /// GetLogParams' top level is `"type": "object"`, so it must never take that path.
    #[test]
    fn normalize_leaves_the_get_log_object_schema_intact() {
        let schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        let mut obj = schema.as_object().unwrap().clone();
        normalize_schema_openapi3(&mut obj);
        assert!(
            obj["properties"]["id"]["anyOf"].is_array(),
            "the nullable rewrite must still fire on Option<String>: {obj:?}"
        );
        assert!(
            obj["properties"]["log_id"]["anyOf"].is_array(),
            "#82: the newly declared log_id must take the same nullable path as id: {obj:?}"
        );
        assert_eq!(
            obj["additionalProperties"],
            serde_json::Value::Bool(true),
            "the object level must not be rewritten into an anyOf branch"
        );
        assert!(obj.get("anyOf").is_none(), "{obj:?}");
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

    /// #67: the pattern is caller text. ObjectScript doubles a quote inside a literal, so
    /// the old `"{pattern}"` with C-style escaping produced code that could not compile —
    /// and the tool reported it as "no tests found", never as a broken call.
    #[test]
    fn a_quote_in_the_pattern_is_doubled_not_backslashed() {
        let code = build_class_test_run_code(r#"Foo"Bar.Tests"#, FLAGS, "tok");
        assert!(code.contains(r#"set tCls="Foo""Bar.Tests""#), "{code}");
        assert!(
            !code.contains("\\\""),
            "no C-style escaping may survive: {code}"
        );
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
mod string_list_arg_tests {
    use super::string_list_arg;
    use serde_json::json;

    fn ok(v: serde_json::Value) -> Vec<String> {
        string_list_arg("body_select", Some(&v)).expect("should parse")
    }

    #[test]
    fn the_documented_array_form_works() {
        assert_eq!(ok(json!(["A", "B"])), vec!["A", "B"]);
    }

    /// #143's actual cause: a model writing a comma string instead of an array.
    /// It used to fall through as_array() to [] and the projection vanished with
    /// success:true.
    #[test]
    fn a_comma_separated_string_is_accepted() {
        assert_eq!(ok(json!("A,B , C")), vec!["A", "B", "C"]);
    }

    #[test]
    fn a_single_bare_name_is_accepted() {
        assert_eq!(ok(json!("AccessionNumber")), vec!["AccessionNumber"]);
    }

    #[test]
    fn a_json_array_that_arrived_as_a_string_is_accepted() {
        assert_eq!(ok(json!(r#"["A","B"]"#)), vec!["A", "B"]);
    }

    #[test]
    fn absent_null_and_empty_all_mean_not_asked_for() {
        assert!(string_list_arg("body_select", None).unwrap().is_empty());
        assert!(ok(json!(null)).is_empty());
        assert!(ok(json!("")).is_empty());
        assert!(ok(json!([])).is_empty());
    }

    /// The one behaviour that must never come back: a shape it cannot read
    /// turning into an empty list, which reads as "the caller wanted nothing".
    #[test]
    fn an_unusable_shape_is_an_error_not_an_empty_list() {
        for bad in [json!(7), json!({"col": "A"}), json!(true), json!([1, 2])] {
            let r = string_list_arg("body_select", Some(&bad));
            assert!(r.is_err(), "{bad} must be refused, not silently dropped");
            assert!(
                r.unwrap_err().contains("body_select"),
                "the error has to name the parameter"
            );
        }
    }

    #[test]
    fn a_malformed_json_array_string_is_an_error() {
        assert!(string_list_arg("body_select", Some(&json!("[\"A\","))).is_err());
    }
}

#[cfg(test)]
mod member_kind_tests {
    use super::{member_kind_mismatch_hint, MemberKind};

    /// The verbatim #178 report: `write $G(s.GetValueAt("1"))` on EnsLib.HL7.Segment.
    /// The method exists and works when called directly — the `$GET()` wrapper is the bug.
    #[test]
    fn property_syntax_on_a_method_names_the_wrapper_not_the_class() {
        let h = member_kind_mismatch_hint(
            "EnsLib.HL7.Segment",
            "GetValueAt",
            "PROPERTY DOES NOT EXIST",
            MemberKind::Method,
        )
        .expect("a declared method reached through property syntax must be explained");
        assert!(h.contains("as a METHOD, not a property"), "{h}");
        assert!(h.contains("$GET()"), "the cause has to be named: {h}");
        // The sentence the old hint produced was "'C' has no 'X'. It declares: X, ..." —
        // a denial and a listing of the same name, one clause apart.
        assert!(
            !h.contains("has no 'GetValueAt'"),
            "must not deny a member it can see: {h}"
        );
    }

    #[test]
    fn method_syntax_on_a_property_is_the_same_bug_mirrored() {
        let h = member_kind_mismatch_hint(
            "My.Cls",
            "Name",
            "METHOD DOES NOT EXIST",
            MemberKind::Property,
        )
        .expect("a declared property called with parentheses must be explained");
        assert!(h.contains("as a PROPERTY, not a method"), "{h}");
        assert!(h.contains("no parentheses"), "{h}");
        // The shape that actually reaches this branch at the wire is `##class(C).Prop()`:
        // `obj.Prop()` on an instance answers <OBJECT DISPATCH>, a different signal that
        // `abort_wants_member_list` does not route (see #178 follow-up note).
        assert!(h.contains("##class(My.Cls).Name()"), "{h}");
    }

    /// When the kind MATCHES the syntax the caller used, the name really is absent and the
    /// list of declared members is the right answer — this branch must stay out of the way.
    #[test]
    fn a_matching_kind_is_not_a_mismatch() {
        assert!(member_kind_mismatch_hint(
            "My.Cls",
            "Missing",
            "METHOD DOES NOT EXIST",
            MemberKind::Method
        )
        .is_none());
        assert!(member_kind_mismatch_hint(
            "My.Cls",
            "Missing",
            "PROPERTY DOES NOT EXIST",
            MemberKind::Property
        )
        .is_none());
    }

    /// `CLASS PROPERTY` is routed to the same place by `abort_wants_member_list`, so it
    /// needs the same treatment — a sibling signal is exactly how these get missed.
    #[test]
    fn the_class_property_signal_is_covered_too() {
        assert!(
            member_kind_mismatch_hint("My.Cls", "Thing", "CLASS PROPERTY", MemberKind::Method)
                .is_some()
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

    /// #136: the hint used to say "(a ClassMethod is skipped)". It is not — %UnitTest
    /// discovers by name, and a `Test*` ClassMethod is run. Verified live on IRIS 2026.1:
    /// a probe with an instance Test*, a ClassMethod Test* and a non-Test* helper set the
    /// first two globals and not the third (total 2, passed 2). Believing the old text
    /// hides a helper named Test* that %UnitTest runs with no arguments and reports green.
    #[test]
    fn no_hint_claims_a_test_classmethod_is_skipped() {
        let hints = [
            no_runnable_tests_cause(&shape(TESTPROD, "APP.Production", 0), "APP").1,
            no_runnable_tests_cause(&shape(TESTCASE, "", 4), "EVALNS").1,
        ];
        for hint in hints {
            let lower = hint.to_lowercase();
            assert!(
                !lower.contains("classmethod is skipped"),
                "a Test* ClassMethod IS discovered and run: {hint}"
            );
            assert!(
                !lower.contains("only runs instance methods"),
                "discovery is by name, not by method kind: {hint}"
            );
            assert!(
                !lower.contains("are instance methods"),
                "an instance-method check cannot explain an empty run: {hint}"
            );
        }
    }

    /// The replacement must still push the caller to the form that can actually assert —
    /// correcting the false clause is not licence to drop the working advice.
    #[test]
    fn the_no_test_methods_hint_still_asks_for_an_instance_method() {
        let (_, hint) = no_runnable_tests_cause(&shape(TESTPROD, "APP.Production", 0), "APP");
        assert!(hint.contains("Method TestX()"), "{hint}");
        assert!(
            hint.contains("$$$Assert"),
            "the reason an instance method is wanted is the assertion macros: {hint}"
        );
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

/// #114: the write gate, as a pure decision. Reading is never blocked; a mutation is
/// refused only on a connection that is not write-allowed.
#[cfg(test)]
mod write_gate_tests {
    use super::*;

    fn call(tool: &str, args: serde_json::Value) -> Option<&'static str> {
        mutating_call(tool, &args)
    }

    /// The half the user cares about most: on a development instance nothing is blocked,
    /// and on ANY instance a read is never blocked. These are the calls that must pass
    /// through untouched no matter what the connection is.
    #[test]
    fn reads_are_never_mutations() {
        let reads = [
            (
                "iris_doc",
                serde_json::json!({"mode": "get", "name": "A.B.cls"}),
            ),
            (
                "iris_doc",
                serde_json::json!({"mode": "head", "name": "A.B.cls"}),
            ),
            ("iris_query", serde_json::json!({"query": "SELECT 1"})),
            ("iris_interop_query", serde_json::json!({"what": "logs"})),
            ("iris_production", serde_json::json!({"action": "status"})),
            ("iris_production", serde_json::json!({"action": "check"})),
            (
                "iris_production",
                serde_json::json!({"action": "get_autostart"}),
            ),
            // The read that the OLD gate made unreachable: removing the whole tool took
            // get_settings with it, so you could not look at a config item at all.
            (
                "iris_production_item",
                serde_json::json!({"action": "get_settings"}),
            ),
            ("iris_lookup_manage", serde_json::json!({"action": "get"})),
            (
                "iris_lookup_manage",
                serde_json::json!({"action": "list_tables"}),
            ),
            (
                "iris_lookup_manage",
                serde_json::json!({"action": "list_keys"}),
            ),
            (
                "iris_lookup_transfer",
                serde_json::json!({"action": "export"}),
            ),
            (
                "iris_business_rule_info",
                serde_json::json!({"action": "list"}),
            ),
            ("iris_debug", serde_json::json!({"action": "error_logs"})),
            ("iris_debug", serde_json::json!({"action": "map_int"})),
            ("check_config", serde_json::json!({})),
            ("docs_introspect", serde_json::json!({"class_name": "A.B"})),
            ("iris_symbols", serde_json::json!({"query": "A"})),
            ("iris_table_info", serde_json::json!({"table": "A.B"})),
            ("iris_credential_list", serde_json::json!({})),
            ("iris_message_body", serde_json::json!({"message_id": "1"})),
            ("iris_production_diff", serde_json::json!({})),
            ("iris_get_log", serde_json::json!({})),
            (
                "extract_message_map_routing",
                serde_json::json!({"class_name": "A.B"}),
            ),
            (
                "find_subclass_implementations",
                serde_json::json!({"method_name": "m"}),
            ),
        ];
        for (tool, args) in reads {
            assert_eq!(
                call(tool, args.clone()),
                None,
                "{tool} {args} is a READ and must never be gated — this server is aimed at \
                 development instances, where a blocked read costs more than it protects"
            );
        }
    }

    /// The five that reached a Live instance ungated before #114, plus the two that were
    /// already gated. Each must be recognised as a mutation.
    #[test]
    fn every_write_path_is_recognised() {
        let writes = [
            (
                "iris_doc",
                serde_json::json!({"mode": "put", "name": "A.B.cls"}),
            ),
            (
                "iris_doc",
                serde_json::json!({"mode": "delete", "name": "A.B.cls"}),
            ),
            ("iris_execute", serde_json::json!({"code": "write 1"})),
            ("iris_compile", serde_json::json!({"target": "A.B.cls"})),
            ("iris_test", serde_json::json!({"pattern": "A"})),
            ("iris_lookup_manage", serde_json::json!({"action": "set"})),
            (
                "iris_lookup_manage",
                serde_json::json!({"action": "delete"}),
            ),
            (
                "iris_lookup_transfer",
                serde_json::json!({"action": "import"}),
            ),
            ("iris_production", serde_json::json!({"action": "start"})),
            ("iris_production", serde_json::json!({"action": "stop"})),
            ("iris_production", serde_json::json!({"action": "restart"})),
            ("iris_production", serde_json::json!({"action": "update"})),
            ("iris_production", serde_json::json!({"action": "recover"})),
            (
                "iris_production",
                serde_json::json!({"action": "set_autostart"}),
            ),
            ("iris_production_item", serde_json::json!({"action": "add"})),
            (
                "iris_production_item",
                serde_json::json!({"action": "remove"}),
            ),
            (
                "iris_production_item",
                serde_json::json!({"action": "enable"}),
            ),
            (
                "iris_production_item",
                serde_json::json!({"action": "disable"}),
            ),
            (
                "iris_production_item",
                serde_json::json!({"action": "set_settings"}),
            ),
            (
                "iris_credential_manage",
                serde_json::json!({"action": "create"}),
            ),
            (
                "iris_credential_manage",
                serde_json::json!({"action": "update"}),
            ),
            (
                "iris_credential_manage",
                serde_json::json!({"action": "delete"}),
            ),
            (
                "iris_query",
                serde_json::json!({"query": "DELETE FROM T", "force": true}),
            ),
            // source_map is the one iris_debug action that writes — execute_via_generator
            // PUTs and compiles a scratch class.
            ("iris_debug", serde_json::json!({"action": "source_map"})),
        ];
        for (tool, args) in writes {
            assert!(
                call(tool, args.clone()).is_some(),
                "{tool} {args} MUTATES and must be gated on a Live connection"
            );
        }
    }

    /// A tool added to the interop profile must be classified deliberately, not inherit
    /// "safe" from the fall-through arm. This is what stops the next `iris_execute` from
    /// quietly becoming ungated.
    #[test]
    fn every_interop_tool_is_classified() {
        for tool in INTEROP_TOOLS {
            assert!(
                CLASSIFIED_TOOLS.contains(tool),
                "'{tool}' is in INTEROP_TOOLS but not in `mutating_call`'s explicit arms. \
                 Decide whether it writes and add it — the fall-through returns None, so \
                 forgetting means it is treated as read-only on a Live instance"
            );
        }
    }

    /// An unknown discriminator is not a write. A typo'd action fails in the handler with a
    /// message naming the valid set (#112); it must not be reported as a blocked write,
    /// which would send the caller to IRIS_ALLOW_PROD for a parameter mistake.
    #[test]
    fn an_unknown_action_is_not_treated_as_a_write() {
        assert_eq!(
            call("iris_production", serde_json::json!({"action": "wibble"})),
            None
        );
        assert_eq!(
            call("iris_doc", serde_json::json!({"mode": "wibble"})),
            None
        );
        assert_eq!(call("iris_production_item", serde_json::json!({})), None);
    }
}

/// #110: three different unreachable states had one message, and it described only the
/// third. A user with IRIS_HOST set correctly was told to set IRIS_HOST.
#[cfg(test)]
mod unreachable_message_tests {
    use super::*;

    fn msg(pending: bool, configured: bool) -> String {
        iris_unreachable_detail(pending, configured)
            .message
            .to_string()
    }

    /// The probe has not answered yet. Telling this user to set IRIS_HOST sends them to
    /// debug a configuration that is already correct — the reported failure mode.
    #[test]
    fn a_pending_probe_says_so_and_does_not_blame_the_configuration() {
        let m = msg(true, true);
        assert!(m.contains("still running"), "{m}");
        assert!(m.contains("Retry"), "{m}");
        assert!(
            !m.contains("Set IRIS_HOST"),
            "the configuration is not the problem here: {m}"
        );
    }

    /// Configured, probe finished, still no connection: the instance is the thing to check.
    #[test]
    fn a_configured_but_unreachable_instance_points_at_the_instance() {
        let m = msg(false, true);
        assert!(m.contains("configured"), "{m}");
        assert!(
            m.contains("api/atelier"),
            "a check they can actually run: {m}"
        );
        assert!(m.contains("recovers"), "say the session will retry: {m}");
    }

    /// Nothing configured — the original message, still correct for the case it was
    /// written for.
    #[test]
    fn an_unconfigured_server_still_gets_the_setup_instructions() {
        let m = msg(false, false);
        assert!(m.contains("Set IRIS_HOST"), "{m}");
        assert!(m.contains("52773"), "{m}");
    }

    /// All three are distinguishable — a caller (or a log grep) can tell them apart.
    #[test]
    fn the_three_states_do_not_share_a_message() {
        let all = [msg(true, true), msg(false, true), msg(false, false)];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two states share one message");
            }
        }
    }
}

/// #107: the near-miss ranking `docs_introspect` uses when a class does not exist.
/// Pure — no connection, no IRIS.
#[cfg(test)]
mod near_miss_tests {
    use super::*;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A typo in the final segment is the case this exists for.
    #[test]
    fn a_typo_in_the_last_segment_finds_the_real_class() {
        let pkg = v(&[
            "Ens.Util.Log",
            "Ens.Util.LogBase",
            "Ens.Util.Calendar",
            "Ens.Util.File",
        ]);
        let got = rank_near_misses("Ens.Util.LogXX", &pkg);
        assert_eq!(got, v(&["Ens.Util.Log", "Ens.Util.LogBase"]));
    }

    /// Handing back the whole package is the same guess the caller already made. A
    /// package with nothing resembling the name must produce NO suggestions, so the
    /// caller is told the class is absent rather than offered twenty wrong ones.
    #[test]
    fn an_unrelated_package_yields_no_suggestions() {
        let pkg = v(&["Ens.Util.Calendar", "Ens.Util.File", "Ens.Util.IO"]);
        assert!(rank_near_misses("Ens.Util.Zebra", &pkg).is_empty());
    }

    /// Two characters in common is every class in a package; three is a name.
    #[test]
    fn the_shared_opening_floor_is_three_characters() {
        let pkg = v(&["A.Foobar", "A.Foxtrot"]);
        // "Foo" shares 3 with Foobar, 2 with Foxtrot.
        assert_eq!(rank_near_misses("A.Foo", &pkg), v(&["A.Foobar"]));
    }

    /// The #62 rule: a whole-name prefix relation in either direction outranks any
    /// last-segment overlap, and is reported first.
    #[test]
    fn a_whole_name_prefix_relation_ranks_first() {
        let pkg = v(&["Ejercicio3.Tests.BO.SQLTest", "Ejercicio3.Teardown"]);
        let got = rank_near_misses("Ejercicio3.Te", &pkg);
        assert_eq!(got[0], "Ejercicio3.Teardown");
        assert!(got.contains(&"Ejercicio3.Tests.BO.SQLTest".to_string()));
    }

    /// A suggestion identical to the input is not a correction — following it re-sends
    /// the same call. (The SQL filters exact matches; this pins the ranking half.)
    #[test]
    fn the_list_is_capped_and_deterministic() {
        let pkg = v(&[
            "P.Logger",
            "P.Logging",
            "P.LogItem",
            "P.LogFile",
            "P.LogSink",
            "P.LogTail",
        ]);
        let got = rank_near_misses("P.Log", &pkg);
        assert_eq!(got.len(), 5, "at most five, or it is a package dump again");
        assert_eq!(
            got,
            rank_near_misses("P.Log", &pkg),
            "must be deterministic"
        );
    }

    // ─── #157: %Foo is shorthand for %Library.Foo ───

    /// The bare form can only ever be the shorthand: verified on 2026.1 that NO stored class
    /// name starts with `%` and contains no `.` (0 rows) while `%Library.*` holds 212, so
    /// expanding up front cannot lose a match.
    #[test]
    fn a_bare_percent_name_expands_to_the_library_package() {
        assert_eq!(
            expand_percent_class("%File").as_deref(),
            Some("%Library.File")
        );
        assert_eq!(
            expand_percent_class("%String").as_deref(),
            Some("%Library.String")
        );
    }

    /// An already-qualified name must pass through untouched, or `%Library.File` would
    /// become `%Library.Library.File` and `%SYSTEM.OBJ` would be rewritten.
    #[test]
    fn an_already_qualified_percent_name_is_left_alone() {
        for n in ["%Library.File", "%SYSTEM.OBJ", "%Dictionary.CompiledClass"] {
            assert_eq!(expand_percent_class(n), None, "{n} must not be rewritten");
        }
    }

    /// Nothing without a leading `%` is shorthand, and a lone `%` is not a class.
    #[test]
    fn a_non_percent_name_is_never_expanded() {
        for n in ["Ens.Director", "Demo.BS.FileIn", "File", "%", ""] {
            assert_eq!(expand_percent_class(n), None, "{n} must not be rewritten");
        }
    }

    /// The envelope must never read as "exists and is empty", with or without candidates.
    #[test]
    fn the_envelope_says_absent_not_empty() {
        for (candidates, in_package) in [(v(&[]), 0usize), (v(&["A.Bar"]), 4usize)] {
            let r = class_not_found_error("A.Baz", "APP", &candidates, in_package, None).unwrap();
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => t.text.clone(),
                _ => panic!("expected text"),
            };
            let j: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(j["error_code"], ERR_CLASS_NOT_FOUND);
            assert_eq!(j["success"], serde_json::json!(false));
            assert!(
                j.get("methods").is_none() && j.get("properties").is_none(),
                "a class that does not exist has no member lists, not empty ones: {j}"
            );
            assert!(j["error"].as_str().unwrap().contains("does not exist"));
        }
    }
}

/// #105: `iris_query` and the `query_once`-backed tools must read the SAME response the
/// same way. They did not: `iris_query` carried a private copy of the HTTP path that had
/// drifted twice. These drive the tool handler and `IrisConnection::query` against ONE
/// mock and assert they agree — a copy that drifts again fails here.
#[cfg(test)]
mod query_parity_tests {
    use super::*;
    use crate::iris::connection::{DiscoverySource, IrisConnection};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn conn(server: &MockServer) -> IrisConnection {
        IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        )
    }

    /// Run `iris_query` against the mock and return its parsed envelope.
    async fn tool_answer(server: &MockServer) -> serde_json::Value {
        let tools = IrisTools::new_with_toolset(Some(conn(server)), Toolset::Interop).unwrap();
        let r = tools
            .iris_query(rmcp::handler::server::wrapper::Parameters(QueryParams {
                query: "SELECT 1".into(),
                parameters: vec![],
                namespace: None,
                force: false,
            }))
            .await
            .expect("the tool must answer, not error out of the transport");
        let text = match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str(&text).unwrap()
    }

    /// Run the connection-level path (`query` → `query_once`) against the same mock.
    async fn conn_answer(server: &MockServer) -> Result<serde_json::Value, String> {
        let c = conn(server);
        let client = reqwest::Client::new();
        c.query("SELECT 1", vec![], "APP", &client)
            .await
            .map_err(|e| e.to_string())
    }

    async fn mock_query(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/query$"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
            .mount(&server)
            .await;
        server
    }

    /// A 200 carrying HTML is a proxy page or a login redirect, not an empty result set.
    /// `unwrap_or_default()` in the duplicate turned it into `{"success":true,"count":0}` —
    /// a confident wrong answer — while the sibling reported the body verbatim.
    #[test]
    fn non_json_200_is_not_an_empty_result_set() {
        rt().block_on(async {
            let server = mock_query(200, "<html>oops not json</html>").await;

            let tool = tool_answer(&server).await;
            assert_ne!(
                tool["success"],
                serde_json::json!(true),
                "a 200 with a non-JSON body must not be reported as a successful query: {tool}"
            );
            assert_eq!(tool["error_code"], "IRIS_REQUEST_FAILED", "got: {tool}");
            assert!(
                tool["error"].as_str().unwrap().contains("oops not json"),
                "the body IRIS actually sent should survive into the error: {tool}"
            );
            assert!(
                tool.get("rows").is_none() && tool.get("count").is_none(),
                "no row count may be reported for a query that read no rows: {tool}"
            );

            let conn = conn_answer(&server).await;
            let conn_err = conn.expect_err("the connection path must fail too");
            assert!(
                conn_err.contains("non-JSON response"),
                "both paths must agree this was not JSON, got: {conn_err}"
            );
        });
    }

    /// Atelier puts the real diagnostic in the BODY of a 400. A status-first read replaced
    /// `ERROR #16002` with a bare `HTTP 400 Bad Request` in the tool while the sibling kept
    /// it. Body-first, in both.
    #[test]
    fn atelier_errors_beat_the_http_status_in_both_paths() {
        rt().block_on(async {
            let server = mock_query(
                400,
                r#"{"status":{"errors":[{"error":"ERROR #16002: Invalid JSON Content"}]}}"#,
            )
            .await;

            let tool = tool_answer(&server).await;
            assert_eq!(
                tool["error_code"], "SQL_ERROR",
                "IRIS's own diagnostic must win over the HTTP status: {tool}"
            );
            assert!(
                tool["error"].as_str().unwrap().contains("ERROR #16002"),
                "the Atelier error text must survive byte-for-byte: {tool}"
            );

            let conn_err = conn_answer(&server)
                .await
                .expect_err("the connection path must fail too");
            assert!(
                conn_err.contains("ERROR #16002"),
                "both paths must surface the same Atelier text, got: {conn_err}"
            );
        });
    }

    /// #105, the half the first pass missed. Sharing the response READER left `iris_query`
    /// still issuing its own request — and therefore still with no retry, while every
    /// `query`-backed tool rode out transient drops. The OpenCode campaign logs show the
    /// cost: one run took four consecutive IRIS_UNREACHABLEs from `iris_query` across ~190s
    /// of a sandbox blip, with `check_config` answering fine in between.
    #[test]
    fn a_transient_5xx_is_retried_rather_than_reported() {
        rt().block_on(async {
            let server = MockServer::start().await;
            // Two failures, then the real answer — exactly what a restarting instance does.
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(ResponseTemplate::new(503))
                .up_to_n_times(2)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(
                        r#"{"status":{"errors":[]},"result":{"content":[{"N":1}]}}"#,
                    ),
                )
                .mount(&server)
                .await;

            let tool = tool_answer(&server).await;
            assert_eq!(
                tool["success"],
                serde_json::json!(true),
                "the third attempt succeeded — the caller must never see the first two: {tool}"
            );
            assert_eq!(tool["count"], 1, "{tool}");
        });
    }

    /// The retry must not paper over a DETERMINISTIC failure. A SQL error is the same on
    /// every attempt, so retrying it only delays the answer.
    #[test]
    fn a_sql_error_is_answered_immediately_not_retried() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"status":{"errors":[{"error":"SQLCODE: -30 Table not found"}]}}"#,
                ))
                .expect(1)
                .mount(&server)
                .await;

            let tool = tool_answer(&server).await;
            assert_eq!(tool["error_code"], "SQL_ERROR", "{tool}");
        });
    }

    /// The happy path still works — otherwise the two tests above would pass on a tool
    /// that rejects everything.
    #[test]
    fn a_good_query_still_returns_its_rows() {
        rt().block_on(async {
            let server = mock_query(
                200,
                r#"{"status":{"errors":[]},"result":{"content":[{"Expression_1":1}]}}"#,
            )
            .await;
            let tool = tool_answer(&server).await;
            assert_eq!(tool["success"], serde_json::json!(true), "got: {tool}");
            assert_eq!(tool["count"], 1, "got: {tool}");
        });
    }
}

// ── Issue #88: /docnames wildcard expansion ───────────────────────────────────
#[cfg(test)]
mod wildcard_listing_failure_tests {
    use super::*;
    use crate::iris::connection::{DiscoverySource, IrisConnection};
    use wiremock::matchers::{method, path_regex, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Drive the real `iris_compile` handler against a mock Atelier and read its envelope.
    /// `namespace: None` targets the connection namespace, exactly as an omitted parameter
    /// does in production.
    async fn compile_against(
        server: &MockServer,
        conn_ns: &str,
        namespace: Option<&str>,
        target: &str,
    ) -> (Option<bool>, serde_json::Value) {
        let conn = IrisConnection::new(
            server.uri(),
            conn_ns,
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();
        let r = tools
            .iris_compile(rmcp::handler::server::wrapper::Parameters(CompileParams {
                target: target.into(),
                flags: "cuk".into(),
                namespace: namespace.map(str::to_string),
                force_writable: false,
                inline: false,
            }))
            .await
            .expect("the tool must answer, not error out of the transport");
        let text = match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        (r.is_error, serde_json::from_str(&text).unwrap())
    }

    /// An Atelier `/docnames/CLS` body over the given document names.
    fn listing(names: &[&str]) -> serde_json::Value {
        serde_json::json!({"result": {"content": names.iter()
            .map(|n| serde_json::json!({"cat":"CLS","db":"APP-CODE","gen":false,"name":n}))
            .collect::<Vec<_>>()}})
    }

    /// A compile response with no errors.
    fn clean_compile() -> serde_json::Value {
        serde_json::json!({"status":{"errors":[],"summary":""},"console":[],"result":{"content":[]}})
    }

    /// The Atelier root descriptor — `result.content.namespaces` is what #93 reads.
    fn root_descriptor(namespaces: &[&str]) -> serde_json::Value {
        serde_json::json!({"result": {"content": {
            "version": "IRIS for UNIX 2026.1", "api": 8, "namespaces": namespaces
        }}})
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// The POST bodies `/action/compile` actually received.
    async fn compile_payloads(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/action/compile"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// #88 follow-up: the guard lives on the expansion, so when the LISTING cannot be read
    /// there is nothing to apply it to. The old `_ => vec![p.target.clone()]` fallback handed
    /// the raw pattern to /action/compile — the cap and the scope rule silently off exactly
    /// when the instance is unhealthy. Every other #88 test drives the pure expansion; this
    /// one drives the call site, which is where the hole was.
    #[test]
    fn a_wildcard_fails_when_the_listing_cannot_be_read_and_never_reaches_compile() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;

            // The listing is down...
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            // ...and /action/compile must NEVER be called. `expect(0)` is asserted on drop.
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            let conn = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();

            let r = tools
                .iris_compile(rmcp::handler::server::wrapper::Parameters(CompileParams {
                    target: "APPPKG.*".into(),
                    flags: "cuk".into(),
                    namespace: None,
                    force_writable: false,
                    inline: false,
                }))
                .await
                .expect("the tool must answer, not error out of the transport");

            assert_eq!(r.is_error, Some(true), "a wildcard must FAIL, not guess");
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => t.text.clone(),
                _ => panic!("expected text content"),
            };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["error_code"], "LISTING_UNAVAILABLE", "{v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("Nothing was compiled"),
                "the caller must be told nothing happened: {v}"
            );
            assert_eq!(v["pattern"], "APPPKG.*");
        });
    }

    /// #94: the listing GET must actually carry `?filter=<prefix>` — the whole point is
    /// that the 1,696,950-byte namespace listing never crosses the wire. The mock only
    /// answers a request that HAS the parameter, so an unfiltered fetch would fall through
    /// to LISTING_UNAVAILABLE and fail this test loudly.
    #[test]
    fn a_wildcard_narrows_the_listing_server_side_and_compiles_what_it_matched() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .and(query_param("filter", "APPPKG."))
                .respond_with(ResponseTemplate::new(200).set_body_json(listing(&[
                    "APPPKG.FoundationProduction.cls",
                    "APPPKG.Sub.Helper.cls",
                ])))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .expect(1)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
            assert_eq!(is_err, Some(false), "{v}");
            assert_eq!(v["targets_compiled"], 2, "{v}");
            assert_eq!(
                compile_payloads(&server).await,
                vec![serde_json::json!([
                    "APPPKG.FoundationProduction.cls",
                    "APPPKG.Sub.Helper.cls"
                ])],
                "the narrowed listing must drive the compile set unchanged"
            );
        });
    }

    /// #94: an Atelier build that rejects `?filter=` must degrade to EXACTLY today's
    /// behaviour — one unfiltered retry — not to a new failure mode. Without this the
    /// performance fix would break wildcards on every server that does not know the
    /// parameter.
    #[test]
    fn a_rejected_filter_falls_back_to_the_unfiltered_listing() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .and(query_param("filter", "APPPKG."))
                .respond_with(ResponseTemplate::new(400))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .and(query_param_is_missing("filter"))
                .respond_with(ResponseTemplate::new(200).set_body_json(listing(&[
                    "APPPKG.FoundationProduction.cls",
                    "WebTerminal.Common.cls",
                ])))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .expect(1)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
            assert_eq!(is_err, Some(false), "{v}");
            assert_eq!(v["targets_compiled"], 1, "{v}");
        });
    }

    /// #94 B3: once the listing is narrowed, `scanned` counts CANDIDATES, not the
    /// namespace — so the old sentence would start reporting "scanned 0 CLS document(s) in
    /// namespace APP" for a typo'd package, which reads as "the namespace is empty". A
    /// performance fix must not ship a new silent wrong answer.
    #[test]
    fn a_narrowed_listing_never_says_it_scanned_zero_documents_in_the_namespace() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .and(query_param("filter", "ZZNOPKG."))
                .respond_with(ResponseTemplate::new(200).set_body_json(listing(&[])))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "ZZNOPKG.*").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
            let msg = v["error"].as_str().unwrap();
            assert!(msg.contains("ZZNOPKG."), "the filter must be named: {v}");
            assert!(
                !msg.contains("scanned 0 CLS document(s)"),
                "a narrowed listing must not claim the namespace is empty: {v}"
            );
            assert_eq!(v["listing_narrowed"], true, "{v}");
            assert_eq!(v["listing_filter"], "ZZNOPKG.", "{v}");
            assert_eq!(v["candidates_scanned"], 0, "{v}");
        });
    }

    /// #93, the verbatim repro: an EXACT target in a namespace that does not exist answered
    /// `IRIS_UNREACHABLE` + "Check IRIS_HOST and IRIS_WEB_PORT" while IRIS was answering
    /// perfectly on that host and port. The 404 body is zero bytes, so the fix asks the
    /// root descriptor a second question.
    #[test]
    fn a_missing_namespace_is_named_instead_of_blaming_the_host_and_port() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP", "USER"])),
                )
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", Some("ZZNOSUCHNS"), "Foo.Bar").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert_eq!(v["namespace"], "ZZNOSUCHNS", "{v}");
            assert_eq!(
                v["available_namespaces"],
                serde_json::json!(["APP", "USER"]),
                "#93 asks for the namespaces that DO exist: {v}"
            );
            let text = v.to_string();
            assert!(
                !text.contains("Check IRIS_HOST"),
                "the host/port hint is the wrong advice here and must not survive: {v}"
            );
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("not accessible to user"),
                "the list reflects ACCESS, not raw existence — say so: {v}"
            );
        });
    }

    /// #93 false-positive guard: Atelier serves `/v8/app/docnames/CLS` happily and
    /// `resolve_namespace` passes the caller's string through verbatim, so a case-sensitive
    /// comparison would invent a NAMESPACE_NOT_FOUND for a namespace that works. Pinned as
    /// a test, not a comment.
    #[test]
    fn a_namespace_that_differs_only_in_case_is_not_reported_as_missing() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP"])))
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", Some("app"), "Foo.Bar").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_ne!(
                v["error_code"], "NAMESPACE_NOT_FOUND",
                "'app' IS 'APP' — today's error must survive: {v}"
            );
            assert_eq!(v["error_code"], "IRIS_UNREACHABLE", "{v}");
        });
    }

    /// #93 on the wildcard listing arm: a 404 used to become LISTING_UNAVAILABLE, which
    /// never said the namespace does not exist and never named the ones that do.
    #[test]
    fn a_wildcard_in_a_missing_namespace_names_the_namespace_not_the_listing() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP", "USER"])),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", Some("ZZNOSUCHNS"), "APPPKG.*").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "NAMESPACE_NOT_FOUND", "{v}");
            assert_eq!(
                v["available_namespaces"],
                serde_json::json!(["APP", "USER"]),
                "{v}"
            );
        });
    }

    /// #93 "cannot tell": a wrong IRIS_WEB_PREFIX 404s the listing AND the root descriptor,
    /// and that is exactly the case the host/port advice is right for. `None` from
    /// `accessible_namespaces` must never become a positive claim about a namespace —
    /// today's error survives verbatim, still saying nothing was compiled.
    #[test]
    fn a_blind_root_descriptor_keeps_todays_error_instead_of_guessing() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r".*/docnames/CLS$"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", Some("ZZNOSUCHNS"), "APPPKG.*").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "LISTING_UNAVAILABLE", "{v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("Nothing was compiled"),
                "{v}"
            );
        });
    }

    // ── Issue #100: the classes /docnames/CLS cannot see ─────────────────────

    /// An Atelier `/action/query` body for the cross-check: (Name, Hidden, GeneratedBy).
    fn class_rows(rows: &[(&str, bool, &str)]) -> serde_json::Value {
        serde_json::json!({"result": {"content": rows.iter()
            .map(|(n, h, g)| serde_json::json!({"Name": n, "Hidden": h, "GeneratedBy": g}))
            .collect::<Vec<_>>()}})
    }

    /// Mount `/action/query` with a fixed body, expecting exactly `times` calls.
    async fn mount_query(server: &MockServer, body: serde_json::Value, times: u64) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(times)
            .mount(server)
            .await;
    }

    /// Mount `/docnames/CLS?filter=<f>` returning `names`.
    async fn mount_listing(server: &MockServer, filter: &str, names: &[&str]) {
        Mock::given(method("GET"))
            .and(path_regex(r".*/docnames/CLS$"))
            .and(query_param("filter", filter))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(names)))
            .mount(server)
            .await;
    }

    /// `/action/compile` must NEVER be called. `expect(0)` is asserted on drop.
    async fn forbid_compile(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/action/compile$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(server)
            .await;
    }

    /// #100, the filed repro. `iris_compile {"target":"EnsPortal.I*"}` answered NOT_FOUND
    /// "No documents match pattern" while `EnsPortal.InterfaceMaps` existed — `iris_doc`
    /// said `exists:true` in the same session and the class compiled fine by exact name.
    /// `%Api.Atelier.v1:GetDocNames` omits precisely `Hidden = 1 OR GeneratedBy <> ''`
    /// (measured: EnsPortal.I* is 2 rows in %Dictionary.ClassDefinition and 0 in the
    /// listing), and `ShowGenerated=1` does not recover them — so the listing alone can
    /// never support the negative the old message asserted.
    #[test]
    fn a_wildcard_that_matches_no_listable_class_names_the_classes_that_exist() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "EnsPortal.I", &[]).await;
            mount_query(
                &server,
                class_rows(&[
                    ("EnsPortal.InterfaceMaps", true, ""),
                    ("EnsPortal.InterfaceReferences", true, ""),
                ]),
                1,
            )
            .await;
            forbid_compile(&server).await;

            let (is_err, v) = compile_against(&server, "APP", None, "EnsPortal.I*").await;
            assert_eq!(is_err, Some(true), "nothing was compiled: {v}");
            assert_eq!(v["error_code"], "NOT_FOUND", "the contract is stable: {v}");
            assert_eq!(v["reason"], "not_listable", "{v}");
            assert_eq!(v["crosscheck"], "ok", "{v}");
            assert_eq!(v["not_listable_total"], 2, "{v}");
            assert_eq!(v["not_listable_truncated"], false, "{v}");
            assert_eq!(
                v["not_listable"][0]["name"], "EnsPortal.InterfaceMaps",
                "{v}"
            );
            assert_eq!(v["not_listable"][0]["hidden"], true, "{v}");
            assert!(v["not_listable"][0]["generated_by"].is_null(), "{v}");

            let msg = v["error"].as_str().unwrap();
            assert!(msg.contains("EnsPortal.InterfaceMaps"), "{v}");
            assert!(msg.contains("EnsPortal.InterfaceReferences"), "{v}");
            assert!(msg.contains("no LISTABLE class matches"), "{v}");
            assert!(
                msg.contains("Compile these by exact name"),
                "a Hidden but AUTHORED class compiles by name — verified live: {v}"
            );
            assert!(
                !msg.contains("No documents match pattern"),
                "the old sentence asserted a negative the listing cannot support: {v}"
            );
            assert!(msg.contains("Nothing was compiled"), "{v}");
            assert_eq!(v["listing_source"], "atelier /docnames/CLS", "{v}");
            assert_eq!(
                v["listing_omits"],
                serde_json::json!(["Hidden", "generated"]),
                "{v}"
            );
        });
    }

    /// #100: the advice SPLITS, and the issue's own suggested wording gets one half wrong.
    /// Verified live: `iris_compile {"target":"EnsPortal.InterfaceMaps.cls"}` succeeds, but
    /// `iris_compile {"target":"Ens.Alerting.AlertManagerMessagesReceived.cls"}` fails with
    /// COMPILE_ERROR "ERROR #5202: Nothing to compile" — a generated class has no source of
    /// its own. Telling a caller to "compile it by exact name" would send them straight into
    /// that error, which is why `GeneratedBy` is selected at all.
    #[test]
    fn a_generated_class_is_told_to_compile_its_generator_not_itself() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "Ens.Alerting.AlertManagerM", &[]).await;
            mount_query(
                &server,
                class_rows(&[(
                    "Ens.Alerting.AlertManagerMessagesReceived",
                    true,
                    "Ens.Alerting.AlertManager.CLS",
                )]),
                1,
            )
            .await;
            forbid_compile(&server).await;

            let (_, v) = compile_against(&server, "APP", None, "Ens.Alerting.AlertManagerM*").await;
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
            assert_eq!(v["reason"], "not_listable", "{v}");
            assert_eq!(
                v["not_listable"][0]["generated_by"], "Ens.Alerting.AlertManager.CLS",
                "{v}"
            );
            let msg = v["error"].as_str().unwrap();
            assert!(
                msg.contains("is generated by Ens.Alerting.AlertManager "),
                "name the GENERATOR, stemmed of its .CLS: {v}"
            );
            assert!(
                msg.contains("ERROR #5202"),
                "say what compiling it by name would actually do: {v}"
            );
            assert!(
                !msg.contains("Compile these by exact name"),
                "that advice is WRONG for a generated class and must not appear: {v}"
            );
        });
    }

    /// #100 B4, the explicit false-positive criterion. A genuine typo must still be a clean
    /// NOT_FOUND — the cross-check exists to sharpen the verdict, not to soften it. Verified
    /// live that `Zzz.Nope.*` and `%ZzzNope.*` really do return zero rows.
    #[test]
    fn a_pattern_that_matches_nothing_anywhere_is_still_not_found() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "Zzz.Nope.", &[]).await;
            mount_query(&server, class_rows(&[]), 1).await;
            forbid_compile(&server).await;

            let (is_err, v) = compile_against(&server, "APP", None, "Zzz.Nope.*").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
            assert_eq!(v["reason"], "no_such_class", "{v}");
            assert_eq!(v["crosscheck"], "ok", "{v}");
            assert_eq!(v["not_listable"], serde_json::json!([]), "{v}");
            assert!(
                v["error"].as_str().unwrap().contains("genuine miss"),
                "the cross-check RAN and settled it — say so: {v}"
            );
        });
    }

    /// THE COST GUARD, restated for #109. It used to read `expect(0)`: the cross-check was
    /// forbidden on the happy path, to hold #94's ~42 ms. #109 changed that deliberately —
    /// a partial match that silently skips a Hidden sibling and reports `success: true` is
    /// a wrong answer, and no latency budget buys that back. So the rule is now ONE query
    /// per successful wildcard compile, and this test pins the number: `expect(1)` fails on
    /// a second query per compile just as loudly as the old `expect(0)` failed on the first.
    /// (Measured against the dev instance: ~70–110 ms for an anchored, indexed LIKE — a
    /// wildcard must carry a literal package prefix before its `*`, so it always is one.)
    #[test]
    fn the_cross_check_runs_exactly_once_on_the_happy_path() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(
                &server,
                "APPPKG.",
                &["APPPKG.FoundationProduction.cls", "APPPKG.Sub.Helper.cls"],
            )
            .await;
            mount_query(
                &server,
                class_rows(&[
                    ("APPPKG.FoundationProduction", false, ""),
                    ("APPPKG.Sub.Helper", false, ""),
                ]),
                1,
            )
            .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .expect(1)
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
            assert_eq!(is_err, Some(false), "{v}");
            assert_eq!(v["targets_compiled"], 2, "{v}");
            // The dictionary and the listing agree — nothing was skipped, and that is a
            // CHECKED zero, not an assumed one.
            assert_eq!(v["not_expanded_count"], 0, "{v}");
            assert!(
                v.get("expansion_note").is_none(),
                "no omission, so no note to add: {v}"
            );
        });
    }

    /// #109, the filed repro, at the call site. `EnsPortal.*Maps` matches two classes in
    /// %Dictionary.ClassDefinition — RecordMaps (listable) and InterfaceMaps (Hidden=1) —
    /// and /docnames/CLS offers only the first. The compile then reported `success: true`,
    /// `targets_compiled: 1`, and named nothing. A caller had no reason to look.
    #[test]
    fn a_partial_wildcard_match_names_the_siblings_it_skipped() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "EnsPortal.", &["EnsPortal.RecordMaps.cls"]).await;
            mount_query(
                &server,
                class_rows(&[
                    ("EnsPortal.RecordMaps", false, ""),
                    ("EnsPortal.InterfaceMaps", true, ""),
                ]),
                1,
            )
            .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "EnsPortal.*Maps").await;
            assert_eq!(is_err, Some(false), "{v}");
            assert_eq!(v["targets_compiled"], 1, "{v}");
            assert_eq!(v["not_expanded_count"], 1, "{v}");
            assert_eq!(
                v["not_expanded"][0]["name"], "EnsPortal.InterfaceMaps",
                "the skipped class must be NAMED, not merely alluded to: {v}"
            );
            assert_eq!(v["not_expanded"][0]["hidden"], true, "{v}");
            let note = v["expansion_note"].as_str().unwrap_or_default();
            assert!(
                note.contains("EnsPortal.InterfaceMaps") && note.contains("NOT compiled"),
                "the prose must say what was left out: {v}"
            );
            // The compile itself must be unchanged — disclosure, not a behaviour change.
            assert_eq!(
                compile_payloads(&server).await,
                vec![serde_json::json!(["EnsPortal.RecordMaps.cls"])],
                "naming the omission must not silently start compiling Hidden classes"
            );
        });
    }

    /// "We could not check" must not render as "nothing was skipped". A failed cross-check
    /// leaves `not_expanded_count` null and says so — the #89/#119 rule, on the success path.
    #[test]
    fn an_unavailable_cross_check_is_not_reported_as_a_clean_expansion() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "APPPKG.", &["APPPKG.FoundationProduction.cls"]).await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
            assert_eq!(is_err, Some(false), "the compile itself succeeded: {v}");
            assert!(
                v["not_expanded_count"].is_null(),
                "an unrunnable check is not a zero: {v}"
            );
            let note = v["expansion_note"].as_str().unwrap_or_default();
            assert!(
                note.contains("could not be run") && note.contains("not proof"),
                "say that nothing was settled: {v}"
            );
        });
    }

    /// The success payload still names its source, so a reader knows the expansion came
    /// from the CLS listing rather than from the class dictionary. (#100 carried the whole
    /// caveat in this string because nothing checked; #109 checks, so the caveat lives in
    /// `expansion_note` only when there is something to caveat.)
    #[test]
    fn the_happy_path_payload_names_its_expansion_source() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "APPPKG.", &["APPPKG.FoundationProduction.cls"]).await;
            mount_query(
                &server,
                class_rows(&[("APPPKG.FoundationProduction", false, "")]),
                1,
            )
            .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/compile$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(clean_compile()))
                .mount(&server)
                .await;

            let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
            assert_eq!(is_err, Some(false), "{v}");
            assert_eq!(v["expansion_source"], "atelier /docnames/CLS", "{v}");
        });
    }

    /// #100: every #88/#93 guard returns BEFORE the emptiness check, so the cross-check is
    /// downstream of them by construction rather than by a flag anyone can flip. Pinned with
    /// `expect(0)` mocks instead of by reading the call site, and each message asserted
    /// unchanged.
    #[test]
    fn the_cross_check_never_runs_for_the_88_guards() {
        rt().block_on(async {
            // (a) SCOPE_REQUIRED — refused before the listing is even fetched.
            {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path_regex(r".*/docnames/CLS$"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(listing(&[])))
                    .expect(0)
                    .mount(&server)
                    .await;
                mount_query(&server, class_rows(&[]), 0).await;
                forbid_compile(&server).await;
                let (is_err, v) = compile_against(&server, "APP", None, "*").await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_eq!(v["error_code"], "SCOPE_REQUIRED", "{v}");
                assert!(
                    v["error"]
                        .as_str()
                        .unwrap()
                        .contains("has nothing before its first '*'"),
                    "{v}"
                );
            }
            // (b) TOO_BROAD — the expansion matched more than the cap.
            {
                let server = MockServer::start().await;
                let names: Vec<String> = (0..WILDCARD_EXPANSION_CAP + 1)
                    .map(|i| format!("APPPKG.C{i}.cls"))
                    .collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                mount_listing(&server, "APPPKG.", &refs).await;
                mount_query(&server, class_rows(&[]), 0).await;
                forbid_compile(&server).await;
                let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_eq!(v["error_code"], "TOO_BROAD", "{v}");
                assert_eq!(v["matched"], WILDCARD_EXPANSION_CAP + 1, "{v}");
                assert!(
                    v["error"]
                        .as_str()
                        .unwrap()
                        .contains("Nothing was compiled"),
                    "{v}"
                );
            }
            // (c) LISTING_UNAVAILABLE — nothing to expand against, so nothing to cross-check.
            {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path_regex(r".*/docnames/CLS$"))
                    .respond_with(ResponseTemplate::new(500))
                    .mount(&server)
                    .await;
                mount_query(&server, class_rows(&[]), 0).await;
                forbid_compile(&server).await;
                let (is_err, v) = compile_against(&server, "APP", None, "APPPKG.*").await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_eq!(v["error_code"], "LISTING_UNAVAILABLE", "{v}");
                assert!(
                    v["error"]
                        .as_str()
                        .unwrap()
                        .contains("Nothing was compiled"),
                    "{v}"
                );
            }
        });
    }

    /// #100 B5 — the #89/#119 rule applied to the NEW path, and the one that is easiest to
    /// get wrong. A cross-check that could not run (SQL error, HTTP error, transport drop,
    /// no privilege on %Dictionary) must NOT emit `not_listable: []` and must NOT set
    /// `reason` to "no_such_class": an empty array here would read as "it does not exist"
    /// and would be #89 rewritten one function over. The verdict stays NOT_FOUND — a caller
    /// who could compile before must still be told the same thing — and only the extra
    /// sentence changes.
    #[test]
    fn a_failed_cross_check_does_not_read_as_no_such_class() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "EnsPortal.I", &[]).await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            forbid_compile(&server).await;

            let (is_err, v) = compile_against(&server, "APP", None, "EnsPortal.I*").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(
                v["error_code"], "NOT_FOUND",
                "the verdict must not move: {v}"
            );
            assert_eq!(v["crosscheck"], "unavailable", "{v}");
            assert!(
                v.get("reason").is_none(),
                "no verdict may be recorded when the check did not run: {v}"
            );
            assert!(
                v.get("not_listable").is_none(),
                "an empty array here IS the bug: {v}"
            );
            let msg = v["error"].as_str().unwrap();
            assert!(msg.contains("could not be run"), "{v}");
            assert!(
                msg.contains("not proof that no such class exists"),
                "cannot-tell must never be stated as a fact: {v}"
            );
        });
    }

    /// #100, pure: the escape must happen BEFORE `*` is translated, or a pattern's own
    /// literal `%` widens the LIKE into a leading-wildcard scan and the row cap can truncate
    /// the real match away — a brand-new false "no such class" inside the fix for one.
    #[test]
    fn like_pattern_escapes_literal_wildcards_before_translating_star() {
        assert_eq!(like_pattern("EnsPortal.I*"), "EnsPortal.I%");
        assert_eq!(like_pattern("%Library.*"), r"\%Library.%");
        assert_eq!(like_pattern("A_B.*"), r"A\_B.%");
        assert_eq!(like_pattern("EnsPortal.*Maps"), "EnsPortal.%Maps");
        // A literal backslash is escaped too, so `ESCAPE '\'` cannot swallow the next char.
        assert_eq!(like_pattern(r"Odd\Name*"), r"Odd\\Name%");
    }

    /// #100, pure: the shared matcher, and the stemming TRAP it makes easy to get wrong.
    /// `/docnames` names carry `.cls` and must be stemmed; `%Dictionary.ClassDefinition`
    /// names do not and must NOT be — `docname_stem` would mangle a real class named
    /// `Pkg.Int` into `Pkg` and it would then fail to match its own pattern. Asserted rather
    /// than merely commented.
    #[test]
    fn the_shared_matcher_selects_identically_for_docnames_and_sql_names() {
        let (re, want_system) = wildcard_matcher("EnsPortal.I*").unwrap();
        assert!(!want_system);
        assert!(re.is_match("EnsPortal.InterfaceMaps"), "raw SQL name");
        assert!(
            re.is_match(docname_stem("EnsPortal.InterfaceMaps.cls")),
            "stemmed docname"
        );

        // The trap: a SQL name whose last segment happens to be a document extension.
        let (re, _) = wildcard_matcher("Pkg.*").unwrap();
        assert!(
            re.is_match("Pkg.Int"),
            "a SQL name must be matched DIRECTLY — stemming it would leave 'Pkg'"
        );
        assert!(
            !re.is_match(docname_stem("Pkg.Int")),
            "…and this is what stemming it would do, which is why it must not happen"
        );

        // `%` system documents need an explicit `%` pattern, on both sides.
        let (re, want_system) = wildcard_matcher("Foo.*").unwrap();
        assert!(!want_system, "a non-% pattern must not drag % classes in");
        assert!(!re.is_match("%Foo.Bar"));
        let (_, want_system) = wildcard_matcher("%Api.*").unwrap();
        assert!(want_system);

        // Regex metacharacters in the pattern are LITERAL — `docname_pattern_regex` runs
        // `regex::escape` before translating `*`, so `A|*` cannot become an unanchored
        // alternation that selects the whole namespace.
        let (re, _) = wildcard_matcher("A[*").unwrap();
        assert!(re.is_match("A[Anything"), "'[' is literal");
        assert!(!re.is_match("AAnything"), "'[' is not a character class");
        let (re, _) = wildcard_matcher("A|*").unwrap();
        assert!(
            !re.is_match("Zzz.Whatever"),
            "the right branch must not float free"
        );
    }

    /// #100: the row cap is a "too many to name" cap, so a full page must be reported as a
    /// FLOOR. Printing 20 names and a bare count of 101 would understate a package the
    /// caller is about to go looking through by hand.
    #[test]
    fn a_capped_cross_check_reports_a_floor_not_a_count() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "Big.Pkg.", &[]).await;
            let names: Vec<String> = (0..NOT_LISTABLE_ROW_CAP + 1)
                .map(|i| format!("Big.Pkg.C{i:03}"))
                .collect();
            let rows: Vec<(&str, bool, &str)> =
                names.iter().map(|n| (n.as_str(), true, "")).collect();
            mount_query(&server, class_rows(&rows), 1).await;
            forbid_compile(&server).await;

            let (_, v) = compile_against(&server, "APP", None, "Big.Pkg.*").await;
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
            assert_eq!(v["reason"], "not_listable", "{v}");
            assert_eq!(v["not_listable_truncated"], true, "{v}");
            let msg = v["error"].as_str().unwrap();
            assert!(
                msg.contains(&format!("at least {}", NOT_LISTABLE_ROW_CAP + 1)),
                "a full page is a floor, not a total: {v}"
            );
            assert!(
                msg.contains(&format!("showing {NOT_LISTABLE_NAMES_SHOWN} of at least")),
                "say how many of them are actually printed: {v}"
            );
        });
    }

    /// #100: the truncation failure mode `like_pattern`'s escaping exists to prevent, caught
    /// on the other side as well. If the cap was hit AND the client re-filter kept nothing,
    /// the rows that would have matched may have been cut off before we ever saw them — that
    /// is "cannot tell", and folding it into `no_such_class` would manufacture exactly the
    /// false negative this whole issue is about.
    #[test]
    fn a_truncated_cross_check_that_matched_nothing_is_unavailable_not_absent() {
        rt().block_on(async {
            let server = MockServer::start().await;
            mount_listing(&server, "Big.Pkg.", &[]).await;
            // A full page, none of which survives the client-side regex.
            let names: Vec<String> = (0..NOT_LISTABLE_ROW_CAP + 1)
                .map(|i| format!("Unrelated.C{i:03}"))
                .collect();
            let rows: Vec<(&str, bool, &str)> =
                names.iter().map(|n| (n.as_str(), true, "")).collect();
            mount_query(&server, class_rows(&rows), 1).await;
            forbid_compile(&server).await;

            let (_, v) = compile_against(&server, "APP", None, "Big.Pkg.*").await;
            assert_eq!(v["error_code"], "NOT_FOUND", "{v}");
            assert_eq!(v["crosscheck"], "unavailable", "{v}");
            assert!(v.get("reason").is_none(), "{v}");
            assert!(v.get("not_listable").is_none(), "{v}");
            assert!(
                v["error"].as_str().unwrap().contains("truncated away"),
                "say WHY it could not tell: {v}"
            );
        });
    }
}

// ── Issue #94: server-side narrowing of the /docnames listing ─────────────────
#[cfg(test)]
mod wildcard_listing_filter_tests {
    use super::*;

    /// The prefix handed to `?filter=`, per pattern shape.
    #[test]
    fn wildcard_listing_filter_takes_the_literal_prefix() {
        assert_eq!(wildcard_listing_filter("MyApp.*"), Some("MyApp."));
        assert_eq!(wildcard_listing_filter("MyApp.*.cls"), Some("MyApp."));
        assert_eq!(wildcard_listing_filter("MyApp.Sub.*"), Some("MyApp.Sub."));
        assert_eq!(
            wildcard_listing_filter("WebTerminal.C*"),
            Some("WebTerminal.C")
        );
        // Mid-pattern star: the prefix is still everything before the FIRST `*`.
        assert_eq!(wildcard_listing_filter("Pkg.*.Foo"), Some("Pkg."));
        // The leading `%` of a system package must survive — in LIKE it only broadens.
        assert_eq!(wildcard_listing_filter("%Pkg.*"), Some("%Pkg."));
        // No wildcard at all: nothing to narrow (this path never fetches a listing).
        assert_eq!(wildcard_listing_filter("MyApp.Foo"), None);
    }

    /// Unqualified patterns have no prefix. They are already refused before any fetch;
    /// the helper must not answer `Some("")`, which would mean `Name Like '%%'`.
    #[test]
    fn wildcard_listing_filter_refuses_an_unqualified_pattern() {
        assert_eq!(wildcard_listing_filter("*"), None);
        assert_eq!(wildcard_listing_filter("*.cls"), None);
        assert_eq!(wildcard_listing_filter("*Foo"), None);
    }

    /// #94 injection guard. `%Api.Atelier.v1:GetDocNames` builds `Name Like '%<filter>%'`
    /// by STRING CONCATENATION, and a probe with `filter=%27` returned 200 with an EMPTY
    /// content array — so an unguarded quote narrows WRONGLY rather than failing loudly,
    /// i.e. a wildcard compile that silently skips classes. Rejection fails SAFE: no
    /// filter, full listing, exactly today's behaviour.
    #[test]
    fn wildcard_listing_filter_rejects_anything_that_could_reach_the_like_clause() {
        for pattern in [
            "Foo'.*",
            "Foo';--.*",
            "Foo\".*",
            "Foo /*x*/.*",
            "Foo\n.*",
            "Ens\u{00e9}.*",
        ] {
            assert_eq!(
                wildcard_listing_filter(pattern),
                None,
                "'{pattern}' must fall back to the unfiltered listing"
            );
        }
    }
}

#[cfg(test)]
mod docname_expansion_tests {
    use super::*;

    /// The exact production pair: parse the listing once, expand over it. `iris_compile`
    /// calls these two in this order.
    fn expand(body: &serde_json::Value, pattern: &str) -> WildcardExpansion {
        expand_wildcard_target(&docnames_in_body(body), pattern)
    }

    /// The documents a pattern would compile. Panics on a refusal — the refusal tests below
    /// assert the outcome itself, so anything reaching this helper is expected to expand.
    fn expand_wildcard_targets(body: &serde_json::Value, pattern: &str) -> Vec<String> {
        match expand(body, pattern) {
            WildcardExpansion::Matched(v) => v,
            other => panic!("'{pattern}' was refused, expected an expansion: {other:?}"),
        }
    }

    /// Verbatim element shape from `GET /api/atelier/v1/APP/docnames/CLS` on IRIS 2026.1 —
    /// objects, and `name` carries the `.cls` extension. Both facts broke the old parser.
    fn object_body() -> serde_json::Value {
        serde_json::json!({"status":{"errors":[],"summary":""},"console":[],"result":{"content":[
            {"cat":"CLS","db":"IRISLIB","gen":false,"name":"%Api.Atelier.v1.cls",
             "ts":"2026-04-07 15:16:26.154","upd":true},
            {"cat":"CLS","db":"APP-CODE","gen":false,"name":"APPPKG.FoundationProduction.cls",
             "ts":"2026-08-01 09:00:00.000","upd":true},
            {"cat":"CLS","db":"APP-CODE","gen":false,"name":"WebTerminal.Common.cls",
             "ts":"2026-08-01 09:00:00.000","upd":true},
            {"cat":"CLS","db":"APP-CODE","gen":false,"name":"WebTerminal.Core.cls",
             "ts":"2026-08-01 09:00:00.000","upd":true}
        ]}})
    }

    /// #94 THE test that matters: the server-side `?filter=` narrowing must be a pure
    /// SUPERSET of what the client regex selects, so every expansion outcome is unchanged.
    ///
    /// `filter=X` is `Name Like '%X%'` — a CONTAINS match — and `docname_pattern_regex` is
    /// `(?i)^…$` anchored on the same literal prefix, so any name the regex matches begins
    /// with that prefix, therefore contains it, therefore survives the filter. This test
    /// simulates the server by keeping only the elements a `LIKE '%prefix%'` would keep,
    /// then asserts the expansion is identical either way.
    ///
    /// `Pkg.*.Foo` is the load-bearing case: its prefix is `Pkg.` while its matches are
    /// deeper, so it is what breaks FIRST if a future IRIS ever makes `filter` an anchored
    /// or prefix match — flipping the relation to a subset and making a wildcard compile
    /// silently skip documents while reporting success. If this test ever goes red, the
    /// narrowing must be removed, not the assertion.
    fn filtered_like_the_server_would(body: &serde_json::Value, prefix: &str) -> serde_json::Value {
        // Case-insensitive, matching the measured behaviour of the Atelier LIKE filter.
        let needle = prefix.to_lowercase();
        let kept: Vec<serde_json::Value> = body["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| {
                let name = d
                    .as_str()
                    .or_else(|| d.get("name").and_then(|n| n.as_str()))
                    .unwrap();
                name.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        serde_json::json!({"result": {"content": kept}})
    }

    /// A listing with enough shape to make narrowing observable: sub-packages, a system
    /// class, and near-miss names that the filter keeps but the regex must still reject.
    fn wide_body() -> serde_json::Value {
        serde_json::json!({"result":{"content":[
            {"cat":"CLS","name":"%Api.Atelier.v1.cls"},
            {"cat":"CLS","name":"%Api.DocDB.v1.cls"},
            {"cat":"CLS","name":"APPPKG.FoundationProduction.cls"},
            {"cat":"CLS","name":"APPPKG.Sub.Helper.cls"},
            {"cat":"CLS","name":"APPPKGX.NotMine.cls"},
            {"cat":"CLS","name":"Pkg.Class.Foo.cls"},
            {"cat":"CLS","name":"Pkg.Class.Bar.cls"},
            {"cat":"CLS","name":"Pkg.Other.Foo.cls"},
            {"cat":"CLS","name":"Pkg.Foo.cls"},
            {"cat":"CLS","name":"Elsewhere.APPPKG.Shadow.cls"},
            {"cat":"CLS","name":"WebTerminal.Common.cls"}
        ]}})
    }

    #[test]
    fn a_server_narrowed_listing_expands_to_exactly_the_same_documents() {
        for body in [object_body(), wide_body()] {
            for pattern in [
                "APPPKG.*",
                "APPPKG.*.cls",
                "APPPKG.Sub.*",
                "Pkg.Class.*",
                "Pkg.*.Foo",
                "%Api.*",
                "WebTerminal.C*",
                "ZZNOPKG.*",
            ] {
                let prefix = wildcard_listing_filter(pattern)
                    .unwrap_or_else(|| panic!("'{pattern}' must yield a filter prefix"));
                let narrowed = filtered_like_the_server_would(&body, prefix);
                assert_eq!(
                    expand_wildcard_target(&docnames_in_body(&body), pattern),
                    expand_wildcard_target(&docnames_in_body(&narrowed), pattern),
                    "'{pattern}': server-side narrowing changed the expansion"
                );
            }
        }
    }

    /// The shape older builds return — the only one the old code handled. Must keep working.
    fn string_body() -> serde_json::Value {
        serde_json::json!({"result":{"content":[
            "APPPKG.FoundationProduction.cls", "WebTerminal.Common.cls"
        ]}})
    }

    /// The exact call from the issue: NOT_FOUND before the fix, on a class that exists.
    #[test]
    fn test_expand_object_shape_package_wildcard() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "APPPKG.*"),
            vec!["APPPKG.FoundationProduction.cls"]
        );
    }

    #[test]
    fn test_expand_object_shape_prefix_wildcard() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "WebTerminal.C*"),
            vec!["WebTerminal.Common.cls", "WebTerminal.Core.cls"]
        );
    }

    #[test]
    fn test_expand_string_shape_still_works() {
        assert_eq!(
            expand_wildcard_targets(&string_body(), "APPPKG.*"),
            vec!["APPPKG.FoundationProduction.cls"]
        );
        assert_eq!(
            expand_wildcard_targets(&string_body(), "WebTerminal.C*"),
            vec!["WebTerminal.Common.cls"]
        );
    }

    #[test]
    fn test_expand_pattern_with_explicit_extension() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "APPPKG.*.cls"),
            expand_wildcard_targets(&object_body(), "APPPKG.*")
        );
    }

    /// The half of the bug the issue's suggested one-liner missed: `name` carries `.cls`,
    /// so a pattern with a non-wildcard tail matches nothing unless the name is stripped
    /// to its stem first.
    #[test]
    fn test_expand_matches_when_pattern_omits_extension() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "APPPKG.FoundationProductio*n"),
            vec!["APPPKG.FoundationProduction.cls"]
        );
        assert!(
            expand_wildcard_targets(&object_body(), "APPPKG.FoundationProductio*nX").is_empty()
        );
    }

    /// A `%` pattern reaches the system documents; nothing else does.
    #[test]
    fn test_expand_reaches_system_docs_only_for_a_system_pattern() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "%Api.*"),
            vec!["%Api.Atelier.v1.cls"]
        );
        // The one pattern that could otherwise pull `%Api.Atelier.v1` in behind a package
        // wildcard is a leading `*` — and that is refused outright, below.
        assert!(!expand_wildcard_targets(&object_body(), "APPPKG.*")
            .iter()
            .any(|n| n.starts_with('%')));
    }

    // ── #88 follow-up: the guard. Before it, `iris_compile {"target":"*"}` expanded to
    //    every non-% class in the namespace (10094 in APP) and POSTed them all to
    //    /action/compile in ONE request — no dry run, no confirmation, from a typo. ──

    /// (a) Nothing literal before the first `*`: refused, never expanded.
    #[test]
    fn test_unqualified_wildcard_is_refused_not_expanded() {
        for pattern in ["*", "*.cls", "*Foo", "*.*", "**"] {
            assert_eq!(
                expand(&object_body(), pattern),
                WildcardExpansion::Unqualified,
                "'{pattern}' names no package — it must be refused, not expanded"
            );
        }
    }

    /// …and the same patterns are refused by the predicate the handler calls BEFORE it
    /// fetches a 15810-name listing, so the refusal costs no round trip.
    #[test]
    fn test_unqualified_is_decided_from_the_pattern_alone() {
        for pattern in ["*", "*.cls", "*Foo"] {
            assert!(wildcard_target_is_unqualified(pattern), "{pattern}");
        }
        for pattern in ["APPPKG.*", "WebTerminal.C*", "%Api.*", "Odd\\Name*", "A*"] {
            assert!(!wildcard_target_is_unqualified(pattern), "{pattern}");
        }
    }

    /// A body of `n` classes in one package — the shape a real package wildcard meets.
    fn package_body(n: usize) -> serde_json::Value {
        let content: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"cat":"CLS","name":format!("Big.Pkg.C{i}.cls")}))
            .collect();
        serde_json::json!({ "result": { "content": content } })
    }

    /// (b) A qualified pattern that still selects too much is refused WITH the count.
    #[test]
    fn test_expansion_over_the_cap_is_refused_with_the_count() {
        assert_eq!(
            expand(&package_body(WILDCARD_EXPANSION_CAP + 1), "Big.Pkg.*"),
            WildcardExpansion::TooBroad {
                matched: WILDCARD_EXPANSION_CAP + 1
            }
        );
        // The count is the REAL one, not the cap: the error has to tell the caller how far
        // over they are, or "narrow it" is a guess.
        assert_eq!(
            expand(&package_body(4000), "Big.Pkg.*"),
            WildcardExpansion::TooBroad { matched: 4000 }
        );
    }

    /// The cap is inclusive — exactly `WILDCARD_EXPANSION_CAP` documents still compile, so
    /// the boundary is not an off-by-one refusal.
    #[test]
    fn test_expansion_at_the_cap_still_compiles() {
        match expand(&package_body(WILDCARD_EXPANSION_CAP), "Big.Pkg.*") {
            WildcardExpansion::Matched(v) => assert_eq!(v.len(), WILDCARD_EXPANSION_CAP),
            other => panic!("{WILDCARD_EXPANSION_CAP} documents must compile: {other:?}"),
        }
    }

    /// The refusal is on the MATCH count, not the listing size: a narrow pattern over a
    /// huge namespace is fine.
    #[test]
    fn test_a_narrow_pattern_over_a_huge_listing_is_not_too_broad() {
        let mut body = package_body(4000);
        body["result"]["content"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"cat":"CLS","name":"Small.Pkg.Only.cls"}));
        assert_eq!(
            expand_wildcard_targets(&body, "Small.Pkg.*"),
            vec!["Small.Pkg.Only.cls"]
        );
    }

    /// (d) `Pkg.Class.*` is the SUBPACKAGE of Pkg.Class, never Pkg.Class itself.
    ///
    /// It compiled it before: /docnames names carry the extension, so `^Pkg\.Class\..*$`
    /// matched the literal document "Pkg.Class.cls" — the `.cls` filled the `.*`. A typo'd
    /// subpackage came back as a successful one-class compile instead of NOT_FOUND.
    #[test]
    fn test_trailing_wildcard_does_not_match_the_parent_document() {
        let body = serde_json::json!({"result":{"content":[
            {"cat":"CLS","name":"Pkg.Class.cls"},
            {"cat":"CLS","name":"Pkg.Class.Sub.cls"}
        ]}});
        // The subpackage exists: only IT is selected.
        assert_eq!(
            expand_wildcard_targets(&body, "Pkg.Class.*"),
            vec!["Pkg.Class.Sub.cls"]
        );
        // Spelling the extension out must not change that.
        assert_eq!(
            expand_wildcard_targets(&body, "Pkg.Class.*.cls"),
            vec!["Pkg.Class.Sub.cls"]
        );
        // And a typo'd subpackage is a MISS — an empty expansion the caller sees as
        // NOT_FOUND — not a quiet compile of the parent class.
        let only_parent = serde_json::json!({"result":{"content":[
            {"cat":"CLS","name":"Pkg.Class.cls"}
        ]}});
        assert!(
            expand_wildcard_targets(&only_parent, "Pkg.Class.*").is_empty(),
            "'Pkg.Class.*' must not compile 'Pkg.Class'"
        );
        assert!(expand_wildcard_targets(&only_parent, "Pkg.Clas.*").is_empty());
    }

    /// The stem strip is on the pattern too, and ONLY for `.cls`: the listing is
    /// /docnames/CLS, so a `.mac` wildcard must match nothing and fall through to the
    /// NOT_FOUND that says "compile a routine by its exact name" — not silently compile
    /// the package's classes.
    #[test]
    fn test_routine_extension_wildcards_match_no_class() {
        for pattern in ["APPPKG.*.mac", "APPPKG.*.int", "APPPKG.*.inc"] {
            assert!(
                expand_wildcard_targets(&object_body(), pattern).is_empty(),
                "'{pattern}' asks for a routine; the CLS listing holds none"
            );
        }
    }

    #[test]
    fn test_expand_is_case_insensitive() {
        assert_eq!(
            expand_wildcard_targets(&object_body(), "apppkg.*"),
            vec!["APPPKG.FoundationProduction.cls"]
        );
    }

    /// A pattern that matches nothing must expand to nothing — the old `.unwrap_or(".*")`
    /// fallback would have turned a typo into a namespace-wide recompile.
    #[test]
    fn test_expand_unmatchable_pattern_returns_empty() {
        // `[` is a literal character in a docname pattern, so this asks for a document
        // whose name begins `A[` — there is none.
        assert!(expand_wildcard_targets(&object_body(), "A[*").is_empty());
        assert!(expand_wildcard_targets(&object_body(), "ZzNoSuchPkg.*").is_empty());
    }

    /// `*` is the ONLY metacharacter. Everything else is literal, and a target that names
    /// no real document must compile no real document.
    ///
    /// Measured against live IRIS before the escape was made total: `ZZVerify88.Goo?dA*`
    /// and `ZZVerify88.G(o)odA*` both returned success:true and compiled ZZVerify88.GoodA,
    /// a class neither pattern names.
    #[test]
    fn test_expand_treats_regex_metacharacters_as_literal() {
        for pattern in [
            "APPPKG.Foundatio?nProduction*", // `?` made the preceding char optional
            "APPPKG.(F)oundationProduction*", // a group matched its own contents
            "APPPKG.Foundation+Production*",
            "APPPKG.FoundationProductio{1}n*",
            "APPPKG.FoundationProduction$*",
            "APPPKG.^FoundationProduction*",
        ] {
            assert!(
                expand_wildcard_targets(&object_body(), pattern).is_empty(),
                "'{pattern}' names no document, so it must compile none"
            );
        }
    }

    /// The escalation: one `|` anchored `^` to the LEFT branch only, so the right branch
    /// matched EVERY document the namespace lists (12744 on the dev instance) and
    /// iris_compile would have POSTed a namespace-wide recompile with flags "cuk".
    #[test]
    fn test_a_pipe_in_a_target_cannot_select_the_whole_namespace() {
        for pattern in [
            "APPPKG.FoundationProduction|*",
            "A|*",
            "APPPKG.FoundationProduction|WebTerminal.Common",
        ] {
            let got = expand_wildcard_targets(&object_body(), pattern);
            assert!(
                got.is_empty(),
                "'{pattern}' must be read literally, not as regex alternation — it \
                 selected {got:?}"
            );
        }
        // `*|APPPKG.*` now never even reaches the matcher: it starts with `*`.
        assert_eq!(
            expand(&object_body(), "*|APPPKG.*"),
            WildcardExpansion::Unqualified
        );
        // …while the same names, spelled as the two real targets they are, still work.
        assert_eq!(
            expand_wildcard_targets(&object_body(), "APPPKG.FoundationProduction*"),
            vec!["APPPKG.FoundationProduction.cls"]
        );
    }

    /// A backslash is a literal character in a document name pattern, not an escape.
    #[test]
    fn test_backslash_is_literal() {
        let body = serde_json::json!({"result":{"content":["Odd\\Name.cls", "OddName.cls"]}});
        assert_eq!(
            expand_wildcard_targets(&body, "Odd\\Name*"),
            vec!["Odd\\Name.cls"]
        );
    }

    /// (a)/(b) again, at the surface the caller actually reads. A refusal is only useful if
    /// it names the problem AND the way out; the cap one has to carry the count, or
    /// "narrow it" is a guess.
    #[test]
    fn test_refusal_envelopes_name_the_pattern_and_the_fix() {
        let payload = |r: &CallToolResult| -> serde_json::Value {
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => &t.text,
                _ => panic!("expected text content"),
            };
            serde_json::from_str(text).unwrap()
        };

        let r = unqualified_wildcard_error("*", "APP").unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = payload(&r);
        assert_eq!(v["error_code"], "SCOPE_REQUIRED");
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("'*'") && msg.contains("APP"), "{msg}");
        assert!(msg.contains("Qualify it with a package"), "{msg}");

        let r = too_broad_wildcard_error("HS.FHIR.*", "APP", 3719).unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = payload(&r);
        assert_eq!(v["error_code"], "TOO_BROAD");
        assert_eq!(v["matched"], 3719);
        assert_eq!(v["limit"], WILDCARD_EXPANSION_CAP);
        let msg = v["error"].as_str().unwrap();
        assert!(
            msg.contains("3719"),
            "the COUNT must be in the message: {msg}"
        );
        assert!(msg.contains("HS.FHIR.*"), "{msg}");
        assert!(
            msg.contains("Nothing was compiled"),
            "the caller must know the refusal was total: {msg}"
        );
    }

    #[test]
    fn test_docnames_in_body_ignores_junk_elements() {
        let body = serde_json::json!({"result":{"content":[
            42, null, {"nope": 1}, "Ok.cls"
        ]}});
        assert_eq!(docnames_in_body(&body), vec!["Ok.cls"]);
    }

    #[test]
    fn test_docnames_in_body_missing_result() {
        assert!(docnames_in_body(&serde_json::json!({})).is_empty());
    }
}

// ── Issue #80: compile-console diagnostics ────────────────────────────────────
#[cfg(test)]
mod console_diag_tests {
    use super::*;

    fn err(raw: &str) -> Option<ConsoleDiag> {
        parse_console_diag(raw, "ERROR:", "ERROR ")
    }

    /// Real line captured from this build: `ERROR:` with a colon, which the old
    /// `strip_prefix("ERROR ")` never matched.
    #[test]
    fn test_error_colon_method_line() {
        let d = err(
            "ERROR: Zz.Broken.cls(M1+2) #1002: Invalid character in tag : \
             '$$$Zz8880UndefinedMacro1' : Offset:15 [M1+1^Zz.Broken.1]",
        )
        .expect("a colon-prefixed diagnostic must parse");
        assert_eq!(d.code, "1002");
        assert_eq!(d.location, "M1+2");
        assert_eq!(d.line, 0);
        assert!(d.text.starts_with("Zz.Broken.cls(M1+2)"), "{}", d.text);
        assert!(
            d.text.contains("Offset:15"),
            "the whole line must survive — the old splitn(3, ':') cut it: {}",
            d.text
        );
    }

    #[test]
    fn test_error_colon_routine_line() {
        let d = err("ERROR:  Zz.Broken.1(3) : MPP5610 : Referenced macro not defined: 'X'")
            .expect("a routine-level diagnostic must parse");
        assert_eq!(d.code, "MPP5610");
        assert_eq!(d.line, 3);
        assert_eq!(d.location, "Zz.Broken.1");
    }

    /// IRIS repeats a bare document name as a header once per routine error; it carries
    /// nothing and must not inflate the count (or the truncation threshold).
    #[test]
    fn test_error_colon_bare_docname_is_skipped() {
        assert!(err("ERROR: Zz.Broken.cls").is_none());
        assert!(err("ERROR: ").is_none());
    }

    /// Regression guard for the old re-slicing: the message contains colons of its own.
    #[test]
    fn test_error_space_shape_still_parsed() {
        let d = err("ERROR #5373: Class 'Zz8880No.Such.Type1', used by 'Zz.Broken2:property:P1', does not exist")
            .expect("the classic space-prefixed shape must still parse");
        assert_eq!(d.code, "5373");
        assert_eq!(d.line, 0);
        assert!(d.text.ends_with("does not exist"), "{}", d.text);
        assert!(d.text.contains("Zz8880No.Such.Type1"), "{}", d.text);
    }

    #[test]
    fn test_error_space_shape_with_line() {
        let d = err("ERROR #5001:12: Something bad").expect("must parse");
        assert_eq!(d.code, "5001");
        assert_eq!(d.line, 12);
    }

    /// A parenthesis inside prose is not a location.
    #[test]
    fn test_parenthesis_in_prose_is_not_a_location() {
        let d = err("ERROR #5030: An error occurred while compiling class (Zz.Broken)")
            .expect("must parse");
        assert_eq!(d.code, "5030");
        assert_eq!(d.location, "");
    }

    #[test]
    fn test_text_line_is_not_an_error() {
        for raw in [
            " TEXT:     set x = $$$Zz8880UndefinedMacro1",
            "Compilation started on 08/26/2026 10:00:00 with qualifiers 'cuk'",
            "Detected 16 errors during compilation in 0.005s.",
            "Skipping class Zz.Broken2",
            "Compilation finished successfully in 0.003s.",
        ] {
            assert!(err(raw).is_none(), "{raw} must not be a diagnostic");
        }
    }

    /// The WARNING shape is unmeasured on this build — assert only that the same parser
    /// recognises both prefixes, not that IRIS emits either one.
    #[test]
    fn test_warning_colon_and_space() {
        for raw in [
            "WARNING: Zz.Broken.cls(M1+2) #6001: Something questionable",
            "WARNING #6001:4: Something questionable",
        ] {
            let d = parse_console_diag(raw, "WARNING:", "WARNING ")
                .unwrap_or_else(|| panic!("{raw} must parse"));
            assert_eq!(d.code, "6001");
        }
        assert!(parse_console_diag(
            "ERROR: Zz.Broken.cls(M1+2) #1002: x",
            "WARNING:",
            "WARNING "
        )
        .is_none());
    }

    /// The #80 arithmetic, offline: a 12-broken-method class produced 65 console lines and
    /// "Detected 24 errors during compilation". Today's parser finds ONE (the status.errors
    /// wrapper). The new one finds enough to clear the default IRIS_INLINE_COMPILE
    /// threshold of 20, which is what makes compile truncation reachable at all.
    #[test]
    fn test_broken_class_console_now_clears_the_truncation_threshold() {
        let mut console: Vec<String> =
            vec!["Compilation started on 08/26/2026 10:00:00 with qualifiers 'cuk'".into()];
        for i in 0..12 {
            console.push("ERROR: Zz.Broken3.cls".into());
            console.push(format!(
                "ERROR:  Zz.Broken3.1({}) : MPP5610 : Referenced macro not defined: 'Zz{i}'",
                i + 3
            ));
            console.push(format!(" TEXT:     set x = $$$Zz{i}"));
        }
        for i in 0..12 {
            console.push(format!(
                "ERROR: Zz.Broken3.cls(M{i}+2) #1002: Invalid character in tag : '$$$Zz{i}' : \
                 Offset:15 [M{i}+1^Zz.Broken3.1]"
            ));
            console.push(format!(" TEXT:     set x = $$$Zz{i}"));
        }
        console.push("Detected 24 errors during compilation in 0.007s.".into());

        // The status.errors wrapper the real response carries first. Verbatim shape from
        // the live run: a MULTI-LINE blob that embeds the FIRST per-method message inside
        // itself. Under a plain `contains` dedup that swallowed macro 1's routine-level
        // entry while macros 2-12 survived — N broken methods, N-1 entries.
        let mut errors: Vec<serde_json::Value> = vec![serde_json::json!({
            "severity":"error","code":"","line":0,"column":0,"location":"",
            "text":"ERROR #5475: Error compiling routine: Zz.Broken3.  Errors:  \
                    Zz.Broken3.cls\r\nERROR:  Zz.Broken3.1(3) : MPP5610 : Referenced macro \
                    not defined: 'Zz0'\r\n TEXT: \tset x = $$$Zz0"
        })];
        for raw in &console {
            if let Some(d) = parse_console_diag(raw, "ERROR:", "ERROR ") {
                if !console_diag_already_reported(&errors, &d.text) {
                    errors.push(serde_json::json!({
                        "severity":"error","code":d.code,"line":d.line,
                        "column":0,"location":d.location,"text":d.text
                    }));
                }
            }
        }
        assert_eq!(
            errors.len(),
            25,
            "1 status.errors wrapper + 12 MPP5610 + 12 #1002; the 12 repeated bare-docname \
             headers must be dropped, and the multi-line wrapper must NOT swallow the \
             first method's entry: {errors:#?}"
        );
        // The asymmetry the substring dedup introduced: every broken method must be
        // individually addressable, the first one included.
        for i in 0..12 {
            assert!(
                errors.iter().any(|e| {
                    e["code"] == "MPP5610"
                        && e["text"]
                            .as_str()
                            .unwrap_or("")
                            .contains(&format!("'Zz{i}'"))
                }),
                "macro Zz{i} has no routine-level entry of its own: {errors:#?}"
            );
        }
        // …and a status error that really does repeat a console line is still deduped.
        let single = vec![serde_json::json!({
            "severity":"error","code":"","line":0,"column":0,"location":"",
            "text":"ERROR #5373: Class 'No.Such', used by 'Zz.Broken3:property:P1', does not exist"
        })];
        let d = parse_console_diag(
            "ERROR #5373: Class 'No.Such', used by 'Zz.Broken3:property:P1', does not exist",
            "ERROR:",
            "ERROR ",
        )
        .expect("must parse");
        assert!(
            console_diag_already_reported(&single, &d.text),
            "a single-line status error still stands in for its console line"
        );
        assert!(
            errors.len() > log_store::read_inline_threshold("IRIS_INLINE_COMPILE", 20),
            "truncation must now be reachable in the default profile"
        );
        // Every per-method entry must be individually addressable, and every entry —
        // the status.errors wrapper included — must carry the key.
        assert!(errors.iter().all(|e| e["location"].is_string()));
        assert!(errors[1..]
            .iter()
            .all(|e| !e["location"].as_str().unwrap_or("").is_empty()));
    }
}

// ── Issue #78: iris_get_log addressing ────────────────────────────────────────
#[cfg(test)]
mod get_log_params_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn empty_store() -> Arc<Mutex<log_store::LogStore>> {
        Arc::new(Mutex::new(log_store::LogStore::new(50, 60)))
    }

    /// Stores `n` entries, each holding `items` result rows. Returns the store and the ids.
    fn store_with(n: usize, items: usize) -> (Arc<Mutex<log_store::LogStore>>, Vec<String>) {
        let store = empty_store();
        let mut ids = Vec::new();
        {
            let mut s = store.lock().unwrap();
            for i in 0..n {
                let rows: Vec<serde_json::Value> =
                    (0..items).map(|j| serde_json::json!({"row": j})).collect();
                let id = log_store::new_log_id();
                s.store(log_store::LogEntry {
                    id: id.clone(),
                    tool: format!("iris_test_{i}"),
                    created_at: std::time::Instant::now(),
                    preview: rows.iter().take(1).cloned().collect(),
                    full_result: serde_json::Value::Array(rows.clone()),
                    total_count: rows.len(),
                });
                ids.push(id);
            }
        }
        (store, ids)
    }

    /// Stores one entry under an EXACT id (the numeric-coercion tests need a known id).
    fn store_one_with_id(id: &str, items: usize) -> Arc<Mutex<log_store::LogStore>> {
        let store = empty_store();
        let rows: Vec<serde_json::Value> =
            (0..items).map(|j| serde_json::json!({"row": j})).collect();
        store.lock().unwrap().store(log_store::LogEntry {
            id: id.to_string(),
            tool: "iris_test".to_string(),
            created_at: std::time::Instant::now(),
            preview: rows.iter().take(1).cloned().collect(),
            full_result: serde_json::Value::Array(rows.clone()),
            total_count: rows.len(),
        });
        store
    }

    /// The shape `iris_test` really stores (mod.rs builds `{test_suites, raw_output}`) —
    /// an OBJECT, not a list. `iris_test` is the tool this store's own empty-index note
    /// sends the agent to, so it is the entry shape pagination meets in practice.
    fn store_object_entry(total: usize) -> (Arc<Mutex<log_store::LogStore>>, String) {
        let store = empty_store();
        let id = log_store::new_log_id();
        store.lock().unwrap().store(log_store::LogEntry {
            id: id.clone(),
            tool: "iris_test".to_string(),
            created_at: std::time::Instant::now(),
            preview: vec![],
            full_result: serde_json::json!({
                "test_suites": [{"name": "S", "tests": total}],
                "raw_output": "All PASSED",
            }),
            total_count: total,
        });
        (store, id)
    }

    /// Issue #81: this `expect` is itself an assertion — rmcp turns any deserialize failure
    /// into a raw JSON-RPC -32602 that bypasses the issue-#2 envelope, so EVERY payload in
    /// this module must survive `from_value`.
    fn parse(v: serde_json::Value) -> GetLogParams {
        serde_json::from_value(v).expect("#81: GetLogParams must never fail to deserialize")
    }

    fn payload(r: &CallToolResult) -> serde_json::Value {
        let text = match &r.content[0].raw {
            RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn call(
        store: &Arc<Mutex<log_store::LogStore>>,
        args: serde_json::Value,
    ) -> (CallToolResult, serde_json::Value) {
        let r = get_log_impl(store, parse(args)).unwrap();
        let v = payload(&r);
        (r, v)
    }

    /// Criterion 1: `log_id` is the name every truncating tool emits
    /// (`log_store::apply_truncation`), so it must address the same entry as `id`.
    #[test]
    fn log_id_alias_reaches_the_same_entry_as_id() {
        let (store, ids) = store_with(1, 3);
        let (r1, v1) = call(&store, serde_json::json!({"log_id": ids[0]}));
        let (r2, v2) = call(&store, serde_json::json!({"id": ids[0]}));
        assert_ne!(r1.is_error, Some(true));
        assert_ne!(r2.is_error, Some(true));
        assert_eq!(v1, v2, "log_id and id must be the same addressing key");
        assert_eq!(v1["total_count"], 3);
    }

    /// The issue's headline: `log_id` on a missing entry used to return the INDEX
    /// (`{"logs":[],"success":true}`), a different response shape that reads as
    /// "no logs exist". It must be LOG_NOT_FOUND, and it must carry no `logs` key.
    #[test]
    fn log_id_on_a_missing_entry_is_log_not_found_not_the_index() {
        let store = empty_store();
        let (r, v) = call(
            &store,
            serde_json::json!({"namespace": "APP", "log_id": "1"}),
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "LOG_NOT_FOUND");
        assert!(
            v.get("logs").is_none(),
            "the index shape must never answer an addressed call: {v}"
        );
    }

    /// Criterion 2: a mistyped addressing key is named, not swallowed.
    #[test]
    fn a_mistyped_addressing_key_is_an_error_that_names_it() {
        let store = empty_store();
        let (r, v) = call(
            &store,
            serde_json::json!({"namespace": "APP", "logid": "1"}),
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "INVALID_PARAMS");
        assert!(
            v["error"].as_str().unwrap().contains("'logid'"),
            "the error must name the offending key: {v}"
        );
        assert_eq!(v["unknown_params"], serde_json::json!(["logid"]));
        assert_eq!(
            v["valid_params"],
            // #82: log_id is a declared parameter now, so the error names it too.
            serde_json::json!(["id", "log_id", "limit", "offset"])
        );
        assert_eq!(
            v["did_you_mean"],
            serde_json::json!(["log_id", "id"]),
            "#82: `logid` is one edit from the DECLARED `log_id`, and log_id is the name \
             the truncating tool handed the caller — suggesting only `id` sends them to \
             the parameter they were not reaching for: {v}"
        );
        assert!(
            v.get("logs").is_none(),
            "an error must not also look like the index: {v}"
        );
    }

    /// Criterion 4 (no regression): `namespace` is sprayed by the harness on nearly every
    /// call, including the correct index call, so it must stay ignorable.
    #[test]
    fn bare_namespace_still_lists_the_index() {
        let store = empty_store();
        for args in [
            serde_json::json!({"namespace": "APP"}),
            serde_json::json!({}),
        ] {
            let (r, v) = call(&store, args.clone());
            assert_ne!(r.is_error, Some(true), "{args} must not be an error");
            assert_eq!(v["success"], true);
            assert!(v["logs"].is_array(), "{v}");
        }
    }

    #[test]
    fn namespace_does_not_suppress_a_populated_index() {
        let (store, _ids) = store_with(2, 1);
        let (r, v) = call(&store, serde_json::json!({"namespace": "APP"}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["logs"].as_array().unwrap().len(), 2);
        assert!(
            v.get("note").is_none(),
            "the note is for the EMPTY index only — it would be noise here: {v}"
        );
    }

    /// Criterion 3: an empty index must say what this store is and is NOT, and must point
    /// at the tools that really read IRIS logs. The tool names are pinned deliberately —
    /// the whole value of the note is that they are real.
    #[test]
    fn the_empty_index_says_what_this_store_is_and_is_not() {
        let store = empty_store();
        let (_r, v) = call(&store, serde_json::json!({}));
        let note = v["note"].as_str().expect("empty index must carry a note");
        for needle in [
            "truncated:true",
            "log_id",
            "NOT the IRIS event log",
            "iris_interop_query",
            "what='logs'",
            "what='trace'",
        ] {
            assert!(note.contains(needle), "note must mention {needle}: {note}");
        }
    }

    /// With `id` present the response shape is unambiguous, so an extra key cannot be
    /// mistaken for the index — the call still answers the id. Issue #84: it is no longer
    /// SILENT about the key, though; the guard used to run only when `id` was absent.
    #[test]
    fn an_unknown_key_alongside_id_still_answers_the_id() {
        let store = empty_store();
        let (r, v) = call(
            &store,
            serde_json::json!({"id": "nope", "tool": "iris_test"}),
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "LOG_NOT_FOUND");
        assert_eq!(
            v["warnings"][0]["unknown_params"],
            serde_json::json!(["tool"]),
            "#84: the typo must be named even though the id was answered: {v}"
        );
    }

    /// `_meta` and friends are client/protocol markers, never tool parameters.
    #[test]
    fn underscore_prefixed_keys_are_never_reported() {
        let store = empty_store();
        let (r, v) = call(&store, serde_json::json!({"_meta": {"x": 1}}));
        assert_ne!(r.is_error, Some(true));
        assert!(v["logs"].is_array(), "{v}");
    }

    #[test]
    fn near_miss_normalisation() {
        for k in [
            "logid",
            "logId",
            "LOG-ID",
            "log id",
            "entryid",
            "log_entry_id",
            // The `id` family. These reached the caller with no `did_you_mean` at all: the
            // normaliser folds them to `id`, which was not a listed spelling, so the near
            // miss of the SHORTER addressing key — the one whose case is easiest to
            // fumble — was the one the tool stayed silent about.
            "ID",
            "Id",
            "id_",
            "i-d",
        ] {
            assert!(is_log_id_near_miss(k), "{k} must be a near miss");
        }
        for k in ["session_id", "tool", "namespace", "limit"] {
            assert!(!is_log_id_near_miss(k), "{k} must NOT be a near miss");
        }
        // The exact declared keys never reach this function — the deserializer consumes
        // them — so listing them here would test nothing. What matters is that a VARIANT
        // spelling of either one gets the same suggestion.
        let store = empty_store();
        for k in ["ID", "Id", "id_", "log-id"] {
            let (_r, v) = call(&store, serde_json::json!({ k: "whatever" }));
            assert_eq!(
                v["did_you_mean"],
                serde_json::json!(GET_LOG_ID_SUGGESTIONS),
                "'{k}' is one keystroke from an addressing key and must be told so: {v}"
            );
        }
    }

    /// A BLANK addressing key means absent, exactly as `null` does (#81). Only `null` was
    /// treated that way, so `{"id":"<valid>","log_id":""}` was a hard conflict while the
    /// same payload with `null` succeeded — one dialect of "I am not using this field"
    /// worked and the other failed.
    #[test]
    fn a_blank_addressing_key_is_absent_not_a_conflict() {
        let (store, ids) = store_with(1, 3);
        for blank in ["", "   "] {
            let (r, v) = call(&store, serde_json::json!({"id": ids[0], "log_id": blank}));
            assert_ne!(r.is_error, Some(true), "'{blank}' must not conflict: {v}");
            assert_eq!(v["total_count"], 3);

            let (r, v) = call(&store, serde_json::json!({"id": blank, "log_id": ids[0]}));
            assert_ne!(r.is_error, Some(true), "'{blank}' must not conflict: {v}");
            assert_eq!(v["total_count"], 3);
        }
    }

    /// …and a blank id ALONE falls back to the index, rather than answering LOG_NOT_FOUND
    /// "with id ''" — an id nobody passed.
    #[test]
    fn a_blank_id_alone_lists_the_index() {
        let (store, _ids) = store_with(2, 1);
        for args in [
            serde_json::json!({"id": ""}),
            serde_json::json!({"log_id": ""}),
            serde_json::json!({"id": "", "log_id": ""}),
        ] {
            let (r, v) = call(&store, args.clone());
            assert_ne!(r.is_error, Some(true), "{args} -> {v}");
            assert_eq!(v["total_count"], 2, "{args} -> {v}");
            assert!(v["logs"].is_array(), "{args} -> {v}");
        }
    }

    /// Every surface that reports parameters reports BOTH lists. `namespace` is accepted
    /// and ignored, but it was absent from `valid_params`, so a client reading that array
    /// programmatically concluded the call it had just made successfully was invalid.
    #[test]
    fn every_param_report_names_the_accepted_and_ignored_keys() {
        let (store, ids) = store_with(1, 3);
        let reports = [
            // unknown key, no id -> fatal
            call(&store, serde_json::json!({"nope": 1})).1,
            // unknown key WITH an id -> warning on a successful answer
            call(&store, serde_json::json!({"id": ids[0], "nope": 1})).1["warnings"][0].clone(),
            // unusable value -> fatal
            call(&store, serde_json::json!({"id": []})).1,
            // limit 0 -> fatal
            call(&store, serde_json::json!({"limit": 0})).1,
        ];
        for v in reports {
            assert_eq!(
                v["valid_params"],
                serde_json::json!(GET_LOG_VALID_PARAMS),
                "{v}"
            );
            assert_eq!(
                v["accepted_and_ignored"],
                serde_json::json!(["namespace"]),
                "a key the tool accepts must not read as invalid: {v}"
            );
        }
        // …and `namespace` is genuinely accepted: it is never reported as unknown.
        let (r, v) = call(&store, serde_json::json!({"namespace": "APP"}));
        assert_ne!(r.is_error, Some(true), "{v}");
    }

    #[test]
    fn unknown_params_are_sorted_for_a_deterministic_message() {
        let store = empty_store();
        let (_r, v) = call(&store, serde_json::json!({"zeta": 1, "alpha": 2}));
        assert_eq!(v["unknown_params"], serde_json::json!(["alpha", "zeta"]));
    }

    /// `GetLogParams` carries a hand-written `Deserialize` (issue #81) alongside the
    /// leftover-capture map — pin that limit/offset still deserialize and validate exactly
    /// as before.
    #[test]
    fn pagination_and_limit_validation_survive_the_flatten() {
        let (store, ids) = store_with(1, 5);
        let (r, v) = call(
            &store,
            serde_json::json!({"id": ids[0], "limit": 2, "offset": 0}),
        );
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["total_count"], 5);
        assert_eq!(v["has_more"], true);
        assert_eq!(v["result"].as_array().unwrap().len(), 2);

        let (r, v) = call(&store, serde_json::json!({"id": ids[0], "limit": 0}));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "INVALID_PARAMS");
        assert!(v["error"].as_str().unwrap().contains("limit must be > 0"));
        assert!(
            v["hint"].is_string() && v["valid_params"].is_array(),
            "this was the one INVALID_PARAMS in the tool that taught the caller nothing: {v}"
        );
    }

    // ── Issue #82: the advertised schema and the deserializer must agree ──────

    /// Anti-drift guard for the hand-written `Deserialize` (issue #81): a property declared
    /// on the struct but never read would appear in the schema, land in the leftover
    /// capture, and then be reported as an unknown parameter the server itself advertises.
    #[test]
    fn every_declared_property_is_read_by_the_deserializer() {
        let schema = serde_json::to_value(schemars::schema_for!(GetLogParams)).unwrap();
        let props = schema["properties"].as_object().unwrap().clone();
        assert!(!props.is_empty());
        for key in props.keys() {
            let sample = match key.as_str() {
                "id" | "log_id" => serde_json::json!("x"),
                "limit" => serde_json::json!(1),
                "offset" => serde_json::json!(0),
                other => panic!(
                    "#82: '{other}' is advertised but this test has no sample for it — teach \
                     it one, and teach GetLogParams::deserialize to read the property"
                ),
            };
            let mut m = serde_json::Map::new();
            m.insert(key.clone(), sample);
            let p = parse(serde_json::Value::Object(m));
            assert!(
                p.extra.is_empty(),
                "advertised property '{key}' fell through to the leftover capture, so the \
                 unknown-parameter guard would reject a schema-conformant call"
            );
            assert!(
                p.issues.is_empty(),
                "advertised property '{key}' produced {:?} for a schema-conformant value",
                p.issues
            );
        }
    }

    // ── Issue #81: nothing escapes as a raw JSON-RPC -32602 ───────────────────

    /// The invariant. rmcp maps every `Deserialize` error to `invalid_params` BEFORE the
    /// handler runs, so such a frame carries no `error_code`, no `hint` and no
    /// `valid_params` — it bypasses the issue-#2 envelope entirely. Each payload marked
    /// below was measured as a hard -32602 against the pre-fix build.
    #[test]
    fn no_arguments_payload_ever_fails_to_deserialize() {
        for args in [
            serde_json::json!({"id": "x", "log_id": "x"}), // -32602: duplicate field `id`
            serde_json::json!({"id": "a", "log_id": "b"}), // -32602: duplicate field `id`
            serde_json::json!({"log_id": 12345}),          // -32602: invalid type: integer
            serde_json::json!({"id": 12345}),              // -32602: invalid type: integer
            serde_json::json!({"limit": "5"}),             // -32602: expected usize
            serde_json::json!({"offset": -1}),             // -32602: expected usize
            serde_json::json!({"id": null, "log_id": "x"}), // -32602: duplicate field `id`
            // The shape OpenAI strict function calling produces once log_id is declared
            // (#82): every property in `required`, `null` for the unused ones.
            serde_json::json!({"id": null, "log_id": null, "limit": null, "offset": null}),
            serde_json::json!({"offset": 1.5}),
            serde_json::json!({"id": true}),
            serde_json::json!({"limit": []}),
            serde_json::json!({"id": {"a": 1}}),
            serde_json::json!({}),
            serde_json::Value::Null,
        ] {
            assert!(
                serde_json::from_value::<GetLogParams>(args.clone()).is_ok(),
                "#81: {args} must reach the handler, not bounce at the rmcp edge"
            );
        }
    }

    /// Equal `id` and `log_id` is exactly what a hedging agent sends. Accept it silently.
    #[test]
    fn equal_id_and_log_id_is_accepted() {
        let (store, ids) = store_with(1, 3);
        let (r1, v1) = call(&store, serde_json::json!({"id": ids[0], "log_id": ids[0]}));
        let (_r2, v2) = call(&store, serde_json::json!({"id": ids[0]}));
        assert_ne!(r1.is_error, Some(true));
        assert_eq!(v1, v2, "the same value in both keys must change nothing");
        assert!(v1.get("warnings").is_none(), "and must add no noise: {v1}");
    }

    /// Replaces `both_id_and_log_id_is_rejected_rather_than_listing_the_index`, which pinned
    /// the serde `duplicate field` -32602. That frame WAS loud rather than silent — the
    /// property that mattered — but it was loud OUTSIDE the envelope. Same property, now
    /// carried by the envelope, and strictly more is asserted: both values are named.
    #[test]
    fn different_id_and_log_id_is_invalid_params_naming_both() {
        let store = empty_store();
        let (r, v) = call(&store, serde_json::json!({"id": "a", "log_id": "b"}));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "INVALID_PARAMS");
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("'a'") && msg.contains("'b'"), "{v}");
        assert_eq!(v["id"], "a");
        assert_eq!(v["log_id"], "b");
        assert!(v["valid_params"].is_array(), "{v}");
        assert!(
            v.get("logs").is_none(),
            "two addressing keys must not silently resolve to the index: {v}"
        );
    }

    /// Log ids look numeric in other tools' output, so a bare number is a reasonable thing
    /// for a client to send. Read it, and say so — never bounce it, and never fall through
    /// to the index.
    #[test]
    fn a_numeric_log_id_is_coerced_not_rejected() {
        let store = store_one_with_id("12345", 3);
        let (r, v) = call(&store, serde_json::json!({"log_id": 12345}));
        assert_ne!(r.is_error, Some(true), "{v}");
        assert_eq!(v["log_id"], "12345");
        assert_eq!(v["total_count"], 3);
        assert_eq!(v["warnings"][0]["code"], "COERCED_PARAM");
        assert_eq!(v["warnings"][0]["param"], "log_id");

        let empty = empty_store();
        let (r, v) = call(&empty, serde_json::json!({"log_id": 12345}));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "LOG_NOT_FOUND");
        assert!(v["error"].as_str().unwrap().contains("'12345'"), "{v}");
        assert!(v.get("logs").is_none(), "{v}");
    }

    /// The strict-function-calling shape: a hard -32602 before the fix.
    #[test]
    fn all_nulls_is_the_index_form() {
        let (store, _ids) = store_with(2, 1);
        let (r, v) = call(
            &store,
            serde_json::json!({"id": null, "log_id": null, "limit": null, "offset": null}),
        );
        assert_ne!(r.is_error, Some(true), "{v}");
        assert_eq!(v["success"], true);
        assert_eq!(v["logs"].as_array().unwrap().len(), 2);
    }

    /// The anti-silent-default guard. Never-failing deserialization moves the failure from
    /// the protocol edge into the handler, so an unusable value MUST still be fatal — a
    /// broken `id` quietly becoming `None` would answer with the index, which is the
    /// wrong-shape failure issue #78 exists to prevent.
    #[test]
    fn an_unusable_value_is_invalid_params_and_never_the_index() {
        let (store, _ids) = store_with(2, 1);
        for (args, param) in [
            (serde_json::json!({"id": {"a": 1}}), "id"),
            (serde_json::json!({"id": true}), "id"),
            (serde_json::json!({"log_id": []}), "log_id"),
            (serde_json::json!({"limit": "abc"}), "limit"),
            (serde_json::json!({"offset": -1}), "offset"),
            (serde_json::json!({"offset": 1.5}), "offset"),
        ] {
            let (r, v) = call(&store, args.clone());
            assert_eq!(r.is_error, Some(true), "{args} must fail: {v}");
            assert_eq!(v["error_code"], "INVALID_PARAMS", "{v}");
            assert!(
                v["error"].as_str().unwrap().contains(param),
                "the error must name `{param}`: {v}"
            );
            assert!(v["hint"].is_string(), "and carry the envelope's hint: {v}");
            assert!(
                v.get("logs").is_none(),
                "{args} must never fall through to the index: {v}"
            );
        }
    }

    /// #83 made `limit` paginate the index too, so `limit must be > 0` has to hold for both
    /// forms. Unguarded, `{"limit":0}` would answer `{"logs":[]}` — a fresh instance of the
    /// exact "so there are no logs" failure this cluster exists to prevent.
    #[test]
    fn limit_zero_is_rejected_in_the_index_form_too() {
        let (store, _ids) = store_with(3, 1);
        let (r, v) = call(&store, serde_json::json!({"limit": 0}));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "INVALID_PARAMS");
        assert!(v["error"].as_str().unwrap().contains("limit must be > 0"));
        assert!(
            v["hint"].is_string() && v["valid_params"].is_array(),
            "every INVALID_PARAMS from this tool carries the envelope, this one included: {v}"
        );
        assert!(v.get("logs").is_none(), "{v}");
    }

    // ── Issue #83: the index paginates, in the entry form's shape ─────────────

    /// `limit` and `offset` were advertised and accepted here and then dropped on the floor:
    /// `{}`, `{"limit":1}`, `{"offset":99}` and `{"limit":1,"offset":2}` all returned the
    /// byte-identical full index.
    #[test]
    fn the_index_paginates_with_limit_and_offset() {
        let (store, _ids) = store_with(5, 1);

        let (_r, v) = call(&store, serde_json::json!({"limit": 2}));
        assert_eq!(v["logs"].as_array().unwrap().len(), 2);
        assert_eq!(v["total_count"], 5);
        assert_eq!(v["offset"], 0);
        assert_eq!(v["limit"], 2);
        assert_eq!(v["has_more"], true);

        let (_r, v) = call(&store, serde_json::json!({"limit": 2, "offset": 4}));
        assert_eq!(v["logs"].as_array().unwrap().len(), 1);
        assert_eq!(v["has_more"], false);

        let (_r, v) = call(&store, serde_json::json!({"offset": 2}));
        assert_eq!(v["logs"].as_array().unwrap().len(), 3);
        assert_eq!(v["total_count"], 5);
        assert_eq!(v["has_more"], false);
        assert_eq!(v["limit"], serde_json::Value::Null);
    }

    /// #83 asks the index to match the entry form's shape EXACTLY. Encoded as an assertion
    /// rather than a comment: the two responses may differ only in what they carry the rows
    /// in (`logs` vs `result` + `log_id`).
    #[test]
    fn the_paginated_index_shape_matches_the_entry_form() {
        let (store, ids) = store_with(3, 5);
        let (_r, index) = call(&store, serde_json::json!({"limit": 2}));
        let (_r, entry) = call(&store, serde_json::json!({"id": ids[0], "limit": 2}));
        for key in ["success", "total_count", "offset", "limit", "has_more"] {
            assert!(index.get(key).is_some(), "index is missing {key}: {index}");
            assert!(entry.get(key).is_some(), "entry is missing {key}: {entry}");
        }
        let mut index_only: Vec<&str> = index
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| entry.get(k.as_str()).is_none())
            .map(String::as_str)
            .collect();
        index_only.sort();
        assert_eq!(index_only, vec!["logs"], "{index}");
        let mut entry_only: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| index.get(k.as_str()).is_none())
            .map(String::as_str)
            .collect();
        entry_only.sort();
        assert_eq!(entry_only, vec!["log_id", "result"], "{entry}");
    }

    /// A plain `{}` index call must not sprout pagination noise it did not ask for.
    #[test]
    fn an_unpaginated_index_is_unchanged_apart_from_total_count() {
        let (store, _ids) = store_with(3, 1);
        let (r, v) = call(&store, serde_json::json!({}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["logs"].as_array().unwrap().len(), 3);
        assert_eq!(v["total_count"], 3);
        for key in ["offset", "limit", "has_more", "note", "warnings"] {
            assert!(v.get(key).is_none(), "{key} must not appear: {v}");
        }
    }

    /// The trap pagination introduces: `EMPTY_LOG_INDEX_NOTE` says "no tool in THIS session
    /// has truncated its output yet". Keyed on the PAGE being empty rather than the STORE,
    /// that becomes a lie on a store holding three entries.
    #[test]
    fn an_offset_past_the_end_does_not_claim_the_store_is_empty() {
        let (store, _ids) = store_with(3, 1);
        let (r, v) = call(&store, serde_json::json!({"offset": 99}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["logs"].as_array().unwrap().len(), 0);
        assert_eq!(v["total_count"], 3);
        let note = v["note"].as_str().expect("an empty page must say why: {v}");
        assert!(
            !note.contains("no tool in THIS session"),
            "the store is NOT empty: {note}"
        );
        assert!(note.contains("99") && note.contains("3"), "{note}");
    }

    /// …and the genuine empty-store note must survive the new code path.
    #[test]
    fn the_empty_store_note_survives_pagination() {
        let store = empty_store();
        for args in [
            serde_json::json!({"limit": 1}),
            serde_json::json!({"offset": 5}),
        ] {
            let (_r, v) = call(&store, args.clone());
            assert_eq!(v["note"], EMPTY_LOG_INDEX_NOTE, "{args}");
            assert_eq!(v["total_count"], 0);
        }
    }

    /// #83 one branch over: `offset` with no `limit` was accepted and ignored on the ENTRY
    /// form too. `offset: 0` stays byte-identical to the old behaviour.
    #[test]
    fn offset_without_limit_paginates_the_entry_form_too() {
        let (store, ids) = store_with(1, 5);
        let (r, v) = call(&store, serde_json::json!({"id": ids[0], "offset": 3}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["result"].as_array().unwrap().len(), 2);
        assert_eq!(v["total_count"], 5);
        assert_eq!(v["offset"], 3);
        assert_eq!(v["has_more"], false);
    }

    /// #83's real target. `iris_test` stores an OBJECT, so there is no list to slice:
    /// `{id}`, `{id,limit:1}`, `{id,offset:1}`, `{id,offset:3}`, `{id,limit:1,offset:2}` and
    /// `{id,offset:99}` all returned the byte-identical payload — measured, six identical
    /// sha256s over 1712 bytes with all four test cases every time. Returning everything is
    /// the only thing an object CAN do; claiming otherwise is not. The response must say
    /// that pagination did not apply, and must NOT echo has_more.
    #[test]
    fn pagination_on_an_object_entry_says_it_did_not_apply() {
        let (store, id) = store_object_entry(4);
        let (_r, whole) = call(&store, serde_json::json!({"id": id}));
        for args in [
            serde_json::json!({"id": id, "limit": 1}),
            serde_json::json!({"id": id, "offset": 1}),
            serde_json::json!({"id": id, "offset": 99}),
            serde_json::json!({"id": id, "limit": 1, "offset": 2}),
        ] {
            let (r, v) = call(&store, args.clone());
            assert_ne!(r.is_error, Some(true), "{args} must still answer: {v}");
            assert_eq!(
                v["result"], whole["result"],
                "an object cannot be sliced, so the whole result is correct: {args}"
            );
            assert_eq!(
                v["pagination_applied"], false,
                "{args} must not leave the caller believing it got a page: {v}"
            );
            assert!(
                v.get("has_more").is_none(),
                "`has_more:false` over a COMPLETE payload reads as 'you reached the end' \
                 when nothing was skipped: {args} -> {v}"
            );
            let note = v["note"].as_str().unwrap_or_default();
            assert!(
                note.contains("iris_test") && note.contains("not a list"),
                "the note must name the tool and the shape: {note}"
            );
        }
    }

    /// …and an object entry asked for NO pagination stays byte-identical to before: no
    /// note, no pagination_applied, nothing new on the wire.
    #[test]
    fn an_unpaginated_object_entry_is_unchanged() {
        let (store, id) = store_object_entry(4);
        let (r, v) = call(&store, serde_json::json!({"id": id}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(v["total_count"], 4);
        for key in ["offset", "limit", "has_more", "note", "pagination_applied"] {
            assert!(v.get(key).is_none(), "{key} must not appear: {v}");
        }
    }

    /// #83 asymmetry: the index rescues an overshot offset with a note, the entry form did
    /// not. A bare empty `result` is the same "so there is nothing here" misread #78 was
    /// filed for — it must be guarded on BOTH branches, not one.
    #[test]
    fn an_offset_past_the_end_of_an_entry_says_why_it_is_empty() {
        let (store, ids) = store_with(1, 5);
        let (r, v) = call(&store, serde_json::json!({"id": ids[0], "offset": 99}));
        assert_ne!(r.is_error, Some(true));
        assert!(v["result"].as_array().unwrap().is_empty());
        assert_eq!(v["total_count"], 5);
        assert_eq!(v["has_more"], false);
        let note = v["note"].as_str().expect("an empty page must say why: {v}");
        assert!(note.contains("99") && note.contains('5'), "{note}");

        // A page that is empty because the ENTRY is empty must not claim an offset problem.
        let (_r, v) = call(&store, serde_json::json!({"id": ids[0], "limit": 2}));
        assert!(v.get("note").is_none(), "a full page needs no note: {v}");
    }

    // ── Issue #84: the guard runs unconditionally, non-fatally with an id ─────

    /// The headline: `{"id":X,"logid":"garbage"}` was byte-identical to `{"id":X}` — the
    /// typo left no trace at all. It must be reported, and the call must NOT start failing.
    #[test]
    fn a_typo_alongside_a_valid_id_is_warned_about_not_swallowed() {
        let (store, ids) = store_with(1, 3);
        let (r, v) = call(
            &store,
            serde_json::json!({"id": ids[0], "logid": "garbage", "limit": 2}),
        );
        assert_ne!(
            r.is_error,
            Some(true),
            "a warning must not fail the call: {v}"
        );
        assert_eq!(v["success"], true);
        assert_eq!(v["total_count"], 3);
        assert!(v["result"].is_array(), "{v}");
        assert_eq!(v["warnings"][0]["code"], "UNKNOWN_PARAMS");
        assert_eq!(
            v["warnings"][0]["unknown_params"],
            serde_json::json!(["logid"])
        );
        assert_eq!(
            v["warnings"][0]["did_you_mean"],
            serde_json::json!(["log_id", "id"]),
            "the warning path must suggest the same pair as the error path: {v}"
        );
    }

    /// A failed lookup is when the caller most needs to learn the real parameter name.
    #[test]
    fn a_typo_alongside_an_unknown_id_warns_on_the_error_too() {
        let store = empty_store();
        let (r, v) = call(&store, serde_json::json!({"id": "nope", "logid": "x"}));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(v["error_code"], "LOG_NOT_FOUND");
        assert_eq!(v["warnings"][0]["code"], "UNKNOWN_PARAMS");
    }

    /// Both issue-#78 exemptions must hold on the newly-live path: `namespace` is sprayed by
    /// the harness on nearly every call and `_meta` is a protocol marker. Neither may become
    /// a warning, or the noise floor rises on every single get_log call.
    #[test]
    fn ignored_and_underscore_keys_never_warn_with_an_id() {
        let (store, ids) = store_with(1, 3);
        let (r, with_noise) = call(
            &store,
            serde_json::json!({"id": ids[0], "namespace": "APP", "_meta": {"progressToken": 1}}),
        );
        let (_r, bare) = call(&store, serde_json::json!({"id": ids[0]}));
        assert_ne!(r.is_error, Some(true));
        assert_eq!(with_noise, bare, "the exemptions must be invisible");
        assert!(with_noise.get("warnings").is_none(), "{with_noise}");
    }
}

// ── Issues #101 / #102: what a tool says when IRIS answers something other than 200 ──
//
// Drives the REAL tool handlers against a mock Atelier, so every assertion here is the wire
// answer a caller gets. The matrix is deliberately the same at every site: a 2xx keeps
// today's answer, a 404 whose namespace IS listed keeps the tool's own not-found answer, a
// 404 whose namespace is NOT listed names the namespace, and a 401 is an ERROR — never a
// confident negative fact. The 401 cases were written first: the issue text described the
// defect as 404-blindness, and reproducing it with a wrong password against a namespace that
// EXISTS showed the sites were blind to every non-2xx.
#[cfg(test)]
mod http_status_answer_tests {
    use super::*;
    use crate::iris::connection::{DiscoverySource, IrisConnection};
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

    fn tools_for(server: &MockServer) -> IrisTools {
        let conn = IrisConnection::new(
            server.uri(),
            "APP",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap()
    }

    fn payload(r: &CallToolResult) -> serde_json::Value {
        match &r.content[0].raw {
            rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    /// A mock Atelier: `verb`+`path` answers `tpl`, and the root descriptor lists `["APP"]`.
    async fn server_with(verb: &str, path: &str, tpl: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method(verb))
            .and(path_regex(path))
            .respond_with(tpl)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/atelier/$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP"])))
            .mount(&server)
            .await;
        server
    }

    async fn introspect(server: &MockServer, ns: &str) -> (Option<bool>, serde_json::Value) {
        let r = tools_for(server)
            .docs_introspect(Parameters(
                serde_json::from_value(
                    serde_json::json!({"class_name": "Ens.Director", "namespace": ns}),
                )
                .unwrap(),
            ))
            .await
            .expect("the tool must answer, not error out of the transport");
        (r.is_error, payload(&r))
    }

    /// The control that must NOT change: a class that EXISTS and genuinely has no members
    /// still answers `methods: []` on a success envelope. #107 turns the *other* empty case
    /// into an error, and this is what keeps that from swallowing the legitimate one.
    #[test]
    fn introspect_still_answers_an_empty_method_list_for_a_class_that_exists() {
        rt().block_on(async {
            let server = MockServer::start().await;
            // The existence check runs only because the member lists came back empty; it is
            // the one query whose SQL names %Dictionary.CompiledClass. Answer THAT one with
            // a row, and the class exists with no members. Mounted FIRST — wiremock matches
            // in insertion order, so the catch-all below must come second.
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .and(wiremock::matchers::body_string_contains(
                    "%Dictionary.CompiledClass WHERE Name",
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"result": {"content": [{"IsCompiled": 1}]}}),
                    ),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r".*/action/query$"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"result": {"content": []}})),
                )
                .mount(&server)
                .await;
            let (is_err, v) = introspect(&server, "APP").await;
            assert_ne!(is_err, Some(true), "{v}");
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(v["methods"], serde_json::json!([]), "{v}");
            assert_eq!(v["properties"], serde_json::json!([]), "{v}");
        });
    }

    /// #107: the same empty member lists, but the class is in neither dictionary table.
    /// This used to be indistinguishable from the test above — `{"methods":[],
    /// "properties":[],"success":true}` for `No.Such.Class.At.All` — so an agent
    /// introspecting before writing code concluded the class was empty and generated
    /// against nothing.
    #[test]
    fn introspect_reports_a_nonexistent_class_as_missing_not_as_empty() {
        rt().block_on(async {
            let server = server_with(
                "POST",
                r".*/action/query$",
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"result": {"content": []}})),
            )
            .await;
            let (is_err, v) = introspect(&server, "APP").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], ERR_CLASS_NOT_FOUND, "{v}");
            assert_eq!(v["success"], false, "{v}");
            assert!(
                v.get("methods").is_none() && v.get("properties").is_none(),
                "a class that does not exist has no member lists, not empty ones: {v}"
            );
            assert_eq!(v["class_name"], "Ens.Director", "{v}");
            assert_eq!(v["namespace"], "APP", "{v}");
        });
    }

    /// #102 P0. `null` vs `[]` was the ONLY thing separating "I could not reach it" from "it
    /// has no methods", and no caller makes that distinction.
    #[test]
    fn introspect_never_reports_null_methods_on_a_successful_envelope() {
        rt().block_on(async {
            for (status, expected) in [
                (401u16, "IRIS_AUTH_FAILED"),
                (403, "IRIS_FORBIDDEN"),
                (500, "IRIS_SERVER_ERROR"),
            ] {
                let server =
                    server_with("POST", r".*/action/query$", ResponseTemplate::new(status)).await;
                let (is_err, v) = introspect(&server, "APP").await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_eq!(v["error_code"], expected, "{v}");
                assert_eq!(v["success"], false, "{v}");
                assert!(v.get("methods").is_none(), "no negative FACT: {v}");
                assert_eq!(v["namespace"], "APP", "{v}");
            }
        });
    }

    #[test]
    fn introspect_names_the_namespace_on_a_404() {
        rt().block_on(async {
            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(404)).await;
            let (is_err, v) = introspect(&server, "ZZNOSUCHNS").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert!(
                v["hint"]
                    .as_str()
                    .unwrap()
                    .contains("Nothing was introspected"),
                "the hint must not claim a compile that never happened: {v}"
            );
        });
    }

    async fn query(server: &MockServer, ns: &str) -> (Option<bool>, serde_json::Value) {
        let r = tools_for(server)
            .iris_query(Parameters(
                serde_json::from_value(
                    serde_json::json!({"query": "SELECT 1 AS n", "namespace": ns}),
                )
                .unwrap(),
            ))
            .await
            .expect("the tool must answer");
        (r.is_error, payload(&r))
    }

    /// The verbatim #101 repro. A 401 sent the caller to debug networking — "Check IRIS_HOST
    /// and IRIS_WEB_PORT" — while IRIS was answering perfectly on that very host and port.
    #[test]
    fn a_wrong_password_is_named_instead_of_blaming_the_host_and_port() {
        rt().block_on(async {
            let server = server_with(
                "POST",
                r".*/action/query$",
                ResponseTemplate::new(401).set_body_string("Unauthorized"),
            )
            .await;
            let (is_err, v) = query(&server, "APP").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(
                !v.to_string().contains("Check IRIS_HOST"),
                "the host and port are fine — IRIS answered: {v}"
            );
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
            assert!(
                v["attempted_url"]
                    .as_str()
                    .unwrap()
                    .contains("/action/query"),
                "{v}"
            );
        });
    }

    /// 401's sibling. If someone later collapses the two codes into one, this fails.
    #[test]
    fn a_forbidden_response_does_not_tell_the_caller_to_check_their_password() {
        rt().block_on(async {
            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(403)).await;
            let (_, v) = query(&server, "APP").await;
            assert_eq!(v["error_code"], "IRIS_FORBIDDEN", "{v}");
            let hint = v["hint"].as_str().unwrap();
            assert!(!v.to_string().contains("Check IRIS_HOST"), "{v}");
            // The password is mentioned exactly once, to rule it OUT. IRIS validated it
            // before it could evaluate %Development, so on a 403 it is provably correct and
            // sending the caller back to it burns the session on the wrong variable.
            assert!(hint.contains("password is not the problem"), "{v}");
            assert!(
                hint.contains("Do not change IRIS_USERNAME / IRIS_PASSWORD"),
                "{v}"
            );
            assert!(hint.contains("%Development"), "{v}");
        });
    }

    /// #102 P2: `iris_query` builds its own POST and checked the status itself, so fixing
    /// `query_once` alone would never have reached it. No HTTP status may leave this tool as
    /// IRIS_UNREACHABLE — an HTTP response is proof of reachability.
    #[test]
    fn iris_query_never_answers_unreachable_for_a_response_that_arrived() {
        rt().block_on(async {
            for status in [400u16, 401, 403, 404, 409, 423, 500, 503] {
                let server =
                    server_with("POST", r".*/action/query$", ResponseTemplate::new(status)).await;
                let (is_err, v) = query(&server, "APP").await;
                assert_eq!(is_err, Some(true), "{v}");
                assert_ne!(v["error_code"], "IRIS_UNREACHABLE", "HTTP {status}: {v}");
            }
            // ...and a 404 whose namespace is absent from the root descriptor is attributed.
            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(404)).await;
            let (_, v) = query(&server, "ZZNOSUCHNS").await;
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert_eq!(v["available_namespaces"], serde_json::json!(["APP"]), "{v}");
        });
    }

    /// A SQL error still arrives byte-for-byte: Atelier reports it as 200 + `status.errors`,
    /// and the body-first ordering in `query_once` keeps `status.errors` winning.
    #[test]
    fn a_sql_error_is_still_a_sql_error() {
        rt().block_on(async {
            let server = server_with(
                "POST",
                r".*/action/query$",
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "status": {"errors": [{"error": "ERROR #5540: SQLCODE: -30 Table not found"}]}
                })),
            )
            .await;
            let (_, v) = query(&server, "APP").await;
            assert_eq!(v["error_code"], "SQL_ERROR", "{v}");
            assert!(v["error"].as_str().unwrap().contains("SQLCODE: -30"), "{v}");
        });
    }

    /// #102 P2 for the two tools that DO go through `query_once`. Both used to answer
    /// IRIS_UNREACHABLE + "error decoding response body".
    #[test]
    fn iris_symbols_names_the_namespace_and_the_credentials() {
        rt().block_on(async {
            let symbols = |server: &MockServer, ns: &str| {
                let tools = tools_for(server);
                let ns = ns.to_string();
                async move {
                    let r = tools
                        .iris_symbols(Parameters(
                            serde_json::from_value(
                                serde_json::json!({"query": "Ens.*", "namespace": ns}),
                            )
                            .unwrap(),
                        ))
                        .await
                        .expect("the tool must answer");
                    payload(&r)
                }
            };
            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(404)).await;
            let v = symbols(&server, "ZZNOSUCHNS").await;
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert!(
                !v.to_string().contains("error decoding response body"),
                "{v}"
            );

            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(401)).await;
            let v = symbols(&server, "APP").await;
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
        });
    }

    async fn macros(server: &MockServer, ns: &str) -> (Option<bool>, serde_json::Value) {
        let r = tools_for(server)
            .iris_macro(Parameters(
                serde_json::from_value(serde_json::json!({"action": "list", "namespace": ns}))
                    .unwrap(),
            ))
            .await
            .expect("the tool must answer");
        (r.is_error, payload(&r))
    }

    /// #102 P0, the third lying-with-success site. A 2xx with nothing in it keeps today's
    /// answer; a 401 must not be dressed up as "this namespace has no include files".
    #[test]
    fn iris_macro_list_separates_no_macros_from_no_answer() {
        rt().block_on(async {
            // The RTN listing carries .mac / .int / .inc together as OBJECTS. Unmasking the
            // status (below) is what exposed that this tool asked for `/docnames/INC` — not an
            // Atelier category at all, HTTP 400 on every instance — so the listing had never
            // once worked and the swallowed non-2xx made it look like an empty namespace.
            let server = server_with(
                "GET",
                r".*/docnames/RTN$",
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"result": {"content": [
                        {"cat": "RTN", "name": "%assert.inc", "gen": false},
                        {"cat": "RTN", "name": "MyApp.Util.mac", "gen": false},
                        {"cat": "RTN", "name": "MyApp.Macros.INC", "gen": false},
                    ]}}),
                ),
            )
            .await;
            let (is_err, v) = macros(&server, "APP").await;
            assert_ne!(is_err, Some(true), "{v}");
            assert_eq!(v["success"], true, "{v}");
            assert_eq!(
                v["macros"],
                serde_json::json!(["%assert.inc", "MyApp.Macros.INC"]),
                "only include files, case-insensitively, and .mac must not leak in: {v}"
            );

            let server = server_with(
                "GET",
                r".*/docnames/RTN$",
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"result": {"content": []}})),
            )
            .await;
            let (is_err, v) = macros(&server, "APP").await;
            assert_ne!(is_err, Some(true), "{v}");
            assert_eq!(
                v["macros"],
                serde_json::json!([]),
                "the legitimate empty answer: {v}"
            );

            let server = server_with("GET", r".*/docnames/RTN$", ResponseTemplate::new(401)).await;
            let (is_err, v) = macros(&server, "APP").await;
            assert_eq!(is_err, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(
                !v.to_string().contains("No include files found"),
                "a failed call must not answer with a negative fact: {v}"
            );

            let server = server_with("GET", r".*/docnames/RTN$", ResponseTemplate::new(404)).await;
            let (_, v) = macros(&server, "ZZNOSUCHNS").await;
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
        });
    }

    /// #102 P3: `iris_execute` threw the HTTP error away and then reported the DOCKER
    /// fallback's failure, so a mistyped namespace answered "set IRIS_CONTAINER".
    #[test]
    fn iris_execute_names_the_namespace_instead_of_demanding_docker() {
        rt().block_on(async {
            let server = server_with("PUT", r".*/doc/.*", ResponseTemplate::new(404)).await;
            let r = tools_for(&server)
                .iris_execute(Parameters(
                    serde_json::from_value(
                        serde_json::json!({"code": "write 1", "namespace": "ZZNOSUCHNS"}),
                    )
                    .unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert_ne!(
                v["error_code"], "DOCKER_REQUIRED",
                "Docker was never the problem: {v}"
            );
            assert!(
                v["hint"].as_str().unwrap().contains("Nothing was executed"),
                "{v}"
            );
        });
    }

    /// #101 for the same tool: with the HTTP leg rejected and no container, the answer must
    /// name the 401 rather than an env var with no bearing on the problem. The docker attempt
    /// itself still happens — `IrisConnection::execute` does not use IRIS_USERNAME/IRIS_PASSWORD,
    /// so a wrong Atelier password IS genuinely recoverable when a container is configured.
    #[test]
    fn iris_execute_reports_a_401_rather_than_docker_when_both_legs_fail() {
        rt().block_on(async {
            if std::env::var("IRIS_CONTAINER")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                eprintln!("IRIS_CONTAINER is set — the docker leg would really run; skipping");
                return;
            }
            let server = server_with("PUT", r".*/doc/.*", ResponseTemplate::new(401)).await;
            let r = tools_for(&server)
                .iris_execute(Parameters(
                    serde_json::from_value(
                        serde_json::json!({"code": "write 1", "namespace": "APP"}),
                    )
                    .unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(v["error"].as_str().unwrap().contains("HTTP 401"), "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
        });
    }

    /// The nine-tool lever, end to end: `iris_production` never touches
    /// `namespace_missing_error` itself — it inherits the answer from the one Err arm in
    /// `ensure_interop_namespace`.
    #[test]
    fn iris_production_inherits_the_namespace_answer_from_the_interop_preflight() {
        rt().block_on(async {
            let server = server_with("POST", r".*/action/query$", ResponseTemplate::new(404)).await;
            let r = tools_for(&server)
                .iris_production(Parameters(Described::new(
                    serde_json::json!({"action": "status", "namespace": "ZZNOSUCHNS"}),
                )))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert!(
                !v.to_string().contains("PUT doc failed"),
                "the generator's scaffolding is not the caller's mistake: {v}"
            );
        });
    }

    /// #102 step 5: the pre-flight used to spend a whole `execute_via_generator` cycle — PUT +
    /// compile + SQL + DELETE — in namespace USER to ask %SYS.Namespace.Exists. This is
    /// simultaneously the correctness test and the four-round-trips-to-one proof.
    #[test]
    fn iris_test_answers_a_missing_namespace_in_exactly_one_request() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP", "USER"])),
                )
                .mount(&server)
                .await;
            let r = tools_for(&server)
                .iris_test(Parameters(
                    serde_json::from_value(
                        serde_json::json!({"pattern": "Nope.Test", "namespace": "ZZNOSUCHNS"}),
                    )
                    .unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], ERR_NAMESPACE_NOT_FOUND, "{v}");
            assert!(
                v["hint"].as_str().unwrap().contains("No tests were run"),
                "{v}"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "one GET of the root descriptor — not a PUT/compile/query/DELETE cycle"
            );
        });
    }

    /// #101 Phase 5, closing the diagnostic loop. `check_config`'s own description is what
    /// tells a model to call it to diagnose exactly this — and under a wrong password it
    /// answered `connected: true`, actively confirming the misdiagnosis. The server already
    /// knew: `probe()` saw the 401 and logged it to a level nobody reads.
    #[test]
    fn check_config_stops_claiming_connected_under_a_rejected_password() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            let mut conn = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "WRONGPW",
                DiscoverySource::EnvVar,
            );
            conn.probe().await;
            assert_eq!(conn.probe_status, Some(401), "the probe must remember");

            let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();
            let v = payload(
                &tools
                    .check_config(Parameters(NoParams {}))
                    .await
                    .expect("check_config always answers"),
            );
            assert_eq!(v["connected"], false, "{v}");
            assert_eq!(v["auth_ok"], false, "{v}");
            assert_eq!(v["probe_status"], 401, "{v}");
        });
    }

    /// The converse, and the guard against over-firing: a probe that succeeded, or one that
    /// never ran, must not start reporting a credentials problem.
    #[test]
    fn check_config_still_reports_connected_when_the_probe_was_accepted() {
        rt().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_regex(r"^/api/atelier/$"))
                .respond_with(ResponseTemplate::new(200).set_body_json(root_descriptor(&["APP"])))
                .mount(&server)
                .await;
            let mut conn = IrisConnection::new(
                server.uri(),
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            conn.probe().await;
            let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();
            let v = payload(&tools.check_config(Parameters(NoParams {})).await.unwrap());
            assert_eq!(v["connected"], true, "{v}");
            assert_eq!(v["auth_ok"], true, "{v}");

            // Never probed: cannot tell, so the old answer stands.
            let unprobed = IrisConnection::new(
                "http://127.0.0.1:9",
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            let tools = IrisTools::new_with_toolset(Some(unprobed), Toolset::Interop).unwrap();
            let v = payload(&tools.check_config(Parameters(NoParams {})).await.unwrap());
            assert_eq!(v["connected"], true, "{v}");
            assert_eq!(v["auth_ok"], serde_json::Value::Null, "{v}");
        });
    }

    /// #101 at the second `err_json_with_url` call site. `iris_compile`'s 404 behaviour is
    /// deliberately untouched (see `a_namespace_that_differs_only_in_case_is_not_reported_as_missing`,
    /// the #93 scope tripwire); only 401/403 move here.
    #[test]
    fn iris_compile_names_the_credentials_on_a_401() {
        rt().block_on(async {
            let server =
                server_with("POST", r".*/action/compile$", ResponseTemplate::new(401)).await;
            let r = tools_for(&server)
                .iris_compile(Parameters(CompileParams {
                    target: "Ens.Director.cls".into(),
                    flags: "cuk".into(),
                    namespace: Some("APP".into()),
                    force_writable: false,
                    inline: false,
                }))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(!v.to_string().contains("Check IRIS_HOST"), "{v}");
        });
    }

    /// A mock Atelier where EVERY route answers `tpl` — no root descriptor either. The state
    /// a wrong password, a wrong prefix or a dead app actually produces.
    async fn all_routes(tpl_status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(path_regex(r".*"))
            .respond_with(ResponseTemplate::new(tpl_status))
            .mount(&server)
            .await;
        server
    }

    /// #101 THE FILED REPRO, still standing on an interop-profile tool after the fix shipped:
    /// `iris_production_diff` answered `{"error":"PUT doc failed: HTTP 401 Unauthorized",
    /// "error_code":"IRIS_UNREACHABLE","hint":"IRIS did not answer on the configured
    /// host/port …"}`. Four arms in `handle_iris_production_diff` still hard-coded
    /// IRIS_UNREACHABLE while `interop.rs`'s header claimed the shared classifier had
    /// replaced 34 of them.
    #[test]
    fn iris_production_diff_names_the_credentials_on_a_401() {
        rt().block_on(async {
            let server = all_routes(401).await;
            let r = tools_for(&server)
                .iris_production_diff(Parameters(
                    serde_json::from_value(serde_json::json!({"namespace": "APP"})).unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
            assert!(
                !v.to_string().contains("IRIS_WEB_PORT"),
                "IRIS answered on that port — the host/port hint is the wrong advice: {v}"
            );
        });
    }

    /// #101 on `dict.rs`, the file the "one shared helper so every tool inherits it" claim
    /// never reached (zero lines of diff). `extract_message_map_routing` reported a wrong
    /// password as `IRIS_EXECUTE_ERROR` with no hint and no mention of a credential.
    #[test]
    fn extract_message_map_routing_names_the_credentials_on_a_401() {
        rt().block_on(async {
            let server = all_routes(401).await;
            let r = tools_for(&server)
                .extract_message_map_routing(Parameters(
                    serde_json::from_value(
                        serde_json::json!({"class_name": "Ens.Director", "namespace": "APP"}),
                    )
                    .unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
        });
    }

    /// #101 + #57: `find_subclass_implementations` escaped the envelope entirely on a 401 —
    /// raw JSON-RPC `-32603 "hierarchy expansion failed: PUT doc failed: HTTP 401
    /// Unauthorized"`, with no `error_code`, no `hint` and no `isError` for anything
    /// downstream to branch on. A failure to read IRIS is a TOOL failure.
    #[test]
    fn find_subclass_implementations_stays_inside_the_envelope_on_a_401() {
        rt().block_on(async {
            let server = all_routes(401).await;
            let r = tools_for(&server)
                .find_subclass_implementations(Parameters(
                    serde_json::from_value(serde_json::json!({
                        "base_classes": ["Ens.BusinessOperation"],
                        "method_name": "OnMessage",
                        "namespace": "APP"
                    }))
                    .unwrap(),
                ))
                .await
                .expect("a 401 must not become a JSON-RPC -32603");
            let v = payload(&r);
            assert_eq!(r.is_error, Some(true), "{v}");
            assert_eq!(v["error_code"], "IRIS_AUTH_FAILED", "{v}");
            assert!(v["hint"].as_str().unwrap().contains("IRIS_PASSWORD"), "{v}");
        });
    }

    /// #101 REGRESSION, on the tool the issue's own repro used. A wrong `IRIS_WEB_PREFIX`
    /// used to answer `IRIS_UNREACHABLE` + "Check IRIS_HOST and IRIS_WEB_PORT (and
    /// IRIS_WEB_PREFIX if using a non-root gateway)". The fix replaced that with a bare
    /// `{"error_code":"NOT_FOUND","error":"HTTP 404 Not Found"}` — no hint, and
    /// `IRIS_WEB_PREFIX` named nowhere. Both are wrong in different directions: the first
    /// blames a host and port that are answering, the second states a negative about a
    /// document the request never went looking for.
    #[test]
    fn a_wrong_web_prefix_names_the_prefix_on_iris_query() {
        rt().block_on(async {
            let server = all_routes(404).await;
            let r = tools_for(&server)
                .iris_query(Parameters(
                    serde_json::from_value(
                        serde_json::json!({"query": "SELECT 1 AS n", "namespace": "APP"}),
                    )
                    .unwrap(),
                ))
                .await
                .expect("the tool must answer");
            let v = payload(&r);
            assert_eq!(v["error_code"], "ATELIER_NOT_FOUND", "{v}");
            let hint = v["hint"].as_str().unwrap_or_default();
            assert!(hint.contains("IRIS_WEB_PREFIX"), "{v}");
            assert!(
                !hint.contains("Check IRIS_HOST and IRIS_WEB_PORT"),
                "IRIS answered twice — the host and port are the two things this proves: {v}"
            );
            assert_ne!(v["error_code"], "IRIS_UNREACHABLE", "{v}");
            assert_ne!(
                v["error_code"], "NOT_FOUND",
                "nothing here is a statement about a document: {v}"
            );
        });
    }

    /// #101/#102 caveat: `check_config` is the tool whose own description sends a model here
    /// to diagnose a dead IRIS, and it reported `connected: true` for a closed port, an
    /// unroutable host, a wrong prefix (probe_status 404) and a 500. Keying only on 401/403
    /// fixed the credential case and left the rest.
    #[test]
    fn check_config_reports_connected_false_when_iris_did_not_answer_usefully() {
        rt().block_on(async {
            // (a) IRIS answered, but not with a usable root descriptor.
            for status in [404u16, 500] {
                let server = all_routes(status).await;
                let mut conn = IrisConnection::new(
                    server.uri(),
                    "APP",
                    "_SYSTEM",
                    "SYS",
                    DiscoverySource::EnvVar,
                );
                conn.probe().await;
                let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();
                let v = payload(&tools.check_config(Parameters(NoParams {})).await.unwrap());
                assert_eq!(v["connected"], false, "HTTP {status}: {v}");
                assert_eq!(v["probe_status"], status, "{v}");
            }

            // (b) The probe ran and nothing came back at all — port 1 is reserved and closed.
            let mut conn = IrisConnection::new(
                "http://127.0.0.1:1",
                "APP",
                "_SYSTEM",
                "SYS",
                DiscoverySource::EnvVar,
            );
            conn.probe().await;
            assert_eq!(conn.probe_reached, Some(false), "the probe must remember");
            let tools = IrisTools::new_with_toolset(Some(conn), Toolset::Interop).unwrap();
            let v = payload(&tools.check_config(Parameters(NoParams {})).await.unwrap());
            assert_eq!(v["connected"], false, "{v}");
            assert_eq!(
                v["auth_ok"],
                serde_json::Value::Null,
                "nothing was learned about the credentials: {v}"
            );
        });
    }
}

// ── Issue #99: the agent_stats tool's own wiring ──────────────────────────────
//
// The shared implementation is exercised in `skills_tools::agent_stats_shape_tests` (pure)
// and in `tests/skills_tests.rs` (both tools, same unreachable registry). What is left to
// pin HERE is the three lines of wiring the `#[tool]` handler contributes — that it reaches
// the shared builder at all, rather than reading `self.registry.list_skills().len()` again.
//
// Deliberately no assertion on `namespace`: `OBJECTSCRIPT_SKILLMCP_NAMESPACE` is
// process-global and `skills_tools`' own `skills_namespace_fallback_chain` sets it, and
// cargo runs lib tests in parallel threads. This test is written so no environment variable
// can change its outcome — with no connection at all, `agent_stats_result` returns before
// anything is dialled, spawned or read.
#[cfg(test)]
mod agent_stats_wiring_tests {
    use super::*;

    /// #99: `agent_stats` used to answer `{"status":"ok","skill_count":0,…}` for a registry
    /// it had never read — the same 0 it printed for "empty" and for "3 skills present". It
    /// must now fail, and carry no count at all.
    #[test]
    fn agent_stats_never_answers_ok_about_a_registry_it_did_not_read() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tools = IrisTools::new_with_toolset(None, Toolset::Merged).unwrap();
            let r = tools
                .agent_stats(rmcp::handler::server::wrapper::Parameters(NoParams {}))
                .await
                .expect("the tool must answer, not error out of the transport");
            let v: serde_json::Value = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => serde_json::from_str(&t.text).unwrap(),
                _ => panic!("expected text content"),
            };
            assert_eq!(r.is_error, Some(true), "a false zero is not success: {v}");
            assert_eq!(v["error_code"], "DOCKER_REQUIRED", "{v}");
            assert!(
                v.get("skill_count").is_none(),
                "the in-process registry must not stand in for ^SKILLS: {v}"
            );
            assert!(
                v.get("status").is_none(),
                "\"ok\" was an unconditional literal — it must not outlive a failure: {v}"
            );
            assert_eq!(
                v["source"], "^SKILLS",
                "say WHICH registry was unreadable: {v}"
            );
        });
    }
}

/// #123 / #124 — what `iris_execute` makes of the wrapper's abort line.
#[cfg(test)]
mod abort_tests {
    use super::{parse_abort_frame, runtime_abort_line};

    const AFTER_OUTPUT: &str =
        "one\ntwo\nERROR: <METHOD DOES NOT EXIST> 148 RunUser+3^IrisDevTmp.Run32b6783ae37d.1 ValidateProduction,Ens.Director";
    const IMMEDIATE: &str =
        "ERROR: <METHOD DOES NOT EXIST> 148 RunUser+1^IrisDevTmp.Runc793621a6a7d.1 ValidateProduction,Ens.Director";

    /// The defect: `success` was `!trimmed.starts_with("ERROR: ")`, so the identical trap was
    /// a failure when it hit line 1 and a success when a Write landed first.
    #[test]
    fn the_same_abort_is_detected_with_and_without_prior_output() {
        assert!(runtime_abort_line(IMMEDIATE).is_some());
        assert!(
            runtime_abort_line(AFTER_OUTPUT).is_some(),
            "an abort after output is still an abort"
        );
    }

    #[test]
    fn the_reported_error_is_the_abort_line_not_the_whole_output() {
        let abort = runtime_abort_line(AFTER_OUTPUT).unwrap();
        assert!(
            abort.starts_with("ERROR: <METHOD DOES NOT EXIST>"),
            "{abort}"
        );
        assert!(
            !abort.contains("one"),
            "partial output leaked into error: {abort}"
        );
    }

    #[test]
    fn the_zerror_shape_is_detected_too() {
        let out =
            "partial\nERROR($ZERROR): <UNDEFINED>RunUser+1^IrisDevTmp.Runde1eee3e4ec3.1 *pName";
        assert!(runtime_abort_line(out).is_some());
    }

    // ─── #159: the abort concatenated onto the script's own output ───

    /// One character decided this. `Write "ROW=",!` reported the abort and `Write "ROW="`
    /// did not, because the trap landed on the same line as the output and the old
    /// `strip_prefix` test saw `ROW=`. Reproduced live on 0.11.0 before the fix.
    #[test]
    fn an_abort_concatenated_onto_the_output_line_is_still_an_abort() {
        let out = "ROW=ERROR: <CLASS DOES NOT EXIST> 150 RunUser+1^IrisDevTmp.Runa8.1 No.Such";
        assert!(runtime_abort_line(out).is_some(), "{out}");
    }

    /// The reported error must be the abort, not the script's output with the abort glued on.
    ///
    /// PROVENANCE: this string is a REAL captured payload from the eval corpus, not a
    /// constructed one, and `<OBJECT DISPATCH>` has no live reproduction here — the obvious
    /// attempt (dispatching a missing method on a %DynamicObject) yields
    /// `<METHOD DOES NOT EXIST>` on 2026.1 instead. So this test is the only record that the
    /// `<OBJECT DISPATCH>` form occurs in the wild. Do not "simplify" it to a signal that is
    /// easier to reproduce: the point is that the detector is signal-agnostic, and this is
    /// the observed evidence for a class that a repro cannot currently produce.
    #[test]
    fn the_reported_abort_drops_the_scripts_own_output() {
        let out = "PatientId=[ERROR: <OBJECT DISPATCH> 230 RunUser+9^IrisDevTmp.Runb2.1";
        let abort = runtime_abort_line(out).unwrap();
        assert!(abort.starts_with("ERROR: <OBJECT DISPATCH>"), "{abort}");
        assert!(!abort.contains("PatientId="), "{abort}");
    }

    /// The `<` is what separates an IRIS signal from a script printing the word. A final
    /// line mentioning `ERROR: ` with no signal after it must NOT become an abort.
    #[test]
    fn a_final_line_mentioning_error_without_a_signal_is_not_an_abort() {
        for out in [
            "done. ERROR: none",
            "summary: 0 rows, ERROR: not applicable",
            "ERROR: something broke",
        ] {
            assert!(runtime_abort_line(out).is_none(), "{out}");
        }
    }

    /// When the line carries more than one marker the LAST one is the trap — anything
    /// earlier is the script quoting a previous error.
    #[test]
    fn the_last_marker_on_the_line_wins() {
        let out = "prev=ERROR: <UNDEFINED> x ERROR: <SYNTAX> 3 RunUser+1^IrisDevTmp.Runc3.1";
        let abort = runtime_abort_line(out).unwrap();
        assert!(abort.starts_with("ERROR: <SYNTAX>"), "{abort}");
    }

    /// The `$ZERROR` spelling must work mid-line too.
    #[test]
    fn the_zerror_spelling_is_found_mid_line() {
        let out = "src=0 copy=ERROR($ZERROR): <METHOD DOES NOT EXIST> 148 RunUser+1^X.1 Copy";
        let abort = runtime_abort_line(out).unwrap();
        assert!(abort.starts_with("ERROR($ZERROR): <METHOD"), "{abort}");
    }

    // ─── #159: verbatim captures from the eval corpus (opencode/deepseek cell) ───

    /// Real stored output, whitespace exact. The abort is glued to the last row of a
    /// namespace listing (`USER:1:0:`) with no newline, and the signal is `<COMMAND>` —
    /// a class none of the hand-written fixtures covered.
    #[test]
    fn a_captured_command_abort_after_a_namespace_listing_is_detected() {
        let out = "Nsp:Status:Remote:\n%SYS:1:0:\nAPP:1:0:\nHSCUSTOM:1:0:\nHSLIB:1:0:\n\
                   HSSYS:1:0:\nHSSYSLOCALTEMP:1:0:\nUSER:1:0:ERROR: <COMMAND> 101 \
                   RunUser+1^IrisDevTmp.Run2a3eda04018b.1 Function must return a value at \
                   RunQuery+9^%Library.AbstractResultSet.1";
        let abort = runtime_abort_line(out).expect("captured abort must be detected");
        assert!(abort.starts_with("ERROR: <COMMAND>"), "{abort}");
        assert!(
            !abort.contains("USER:1:0:"),
            "must not carry the listing: {abort}"
        );
    }

    /// Real stored output. `<INVALID OREF>`, glued to `global=`, with an earlier line that
    /// must not be mistaken for the abort.
    #[test]
    fn a_captured_invalid_oref_abort_is_detected() {
        let out =
            "ns=HSCUSTOM\nglobal=ERROR: <INVALID OREF> 192 RunUser+3^IrisDevTmp.Run39336ac9f214.1";
        let abort = runtime_abort_line(out).expect("captured abort must be detected");
        assert!(abort.starts_with("ERROR: <INVALID OREF>"), "{abort}");
        assert!(
            !abort.contains("global="),
            "must not carry the prefix: {abort}"
        );
    }

    /// The wrapper's own non-`<signal>` failure must still be caught.
    #[test]
    fn the_capture_unavailable_sentinel_is_an_abort() {
        assert!(runtime_abort_line("ERROR: output capture unavailable").is_some());
    }

    /// A script that prints the word ERROR mid-run and then completes is not an abort.
    #[test]
    fn ordinary_output_mentioning_error_is_not_an_abort() {
        for out in [
            "ERROR: something the script itself printed\nand then it carried on",
            "checking...\nno problems found",
            "",
        ] {
            assert!(
                runtime_abort_line(out).is_none(),
                "false positive on: {out}"
            );
        }
    }

    /// `RunUser+N` is submitted line N — verified on 2026.1 against a four-line script whose
    /// third line trapped.
    #[test]
    fn the_frame_names_the_submitted_line_and_the_member() {
        let f = parse_abort_frame(runtime_abort_line(AFTER_OUTPUT).unwrap()).unwrap();
        assert_eq!(f.signal, "METHOD DOES NOT EXIST");
        assert_eq!(f.line, Some(3));
        assert_eq!(f.member, Some("ValidateProduction"));
        assert_eq!(f.class, Some("Ens.Director"));

        let submitted = "write \"one\",!\nwrite \"two\",!\nset x = ##class(Ens.Director).ValidateProduction()\nwrite \"four\",!";
        assert_eq!(
            submitted.lines().nth(f.line.unwrap() - 1),
            Some("set x = ##class(Ens.Director).ValidateProduction()")
        );
    }

    /// The suggestion list is only worth attaching if it is ranked. The first cut scored on
    /// leading prefix alone, so `ValidateProduction` — nothing in `Ens.Director` starts with V —
    /// scored zero everywhere and the caller was offered `Console` and
    /// `actualizeProductionDifferences`. These are the real declared members of Ens.Director.
    #[test]
    fn members_are_ranked_by_shared_word_then_shape() {
        let mut names: Vec<String> = [
            "actualizeProductionDifferences",
            "CleanProduction",
            "Console",
            "CreateBusinessService",
            "DeleteProduction",
            "GetProductionStatus",
            "RestartProduction",
            "StartProduction",
            "StopProduction",
            "SystemStart",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        super::rank_members(&mut names, "ValidateProduction");

        let top: Vec<&str> = names.iter().take(6).map(String::as_str).collect();
        for want in ["StartProduction", "StopProduction", "RestartProduction"] {
            assert!(top.contains(&want), "{want} missing from top 6: {top:?}");
        }
        for noise in ["Console", "CreateBusinessService", "SystemStart"] {
            assert!(
                !top.contains(&noise),
                "{noise} should not outrank a *Production: {top:?}"
            );
        }
        // Two-word members beat the three-word one that merely shares the word.
        assert!(
            names.iter().position(|n| n == "StartProduction")
                < names
                    .iter()
                    .position(|n| n == "actualizeProductionDifferences"),
            "{names:?}"
        );
    }

    #[test]
    fn camel_words_splits_on_case() {
        assert_eq!(
            super::camel_words("ValidateProduction"),
            ["validate", "production"]
        );
        assert_eq!(
            super::camel_words("getProductionItems"),
            ["get", "production", "items"]
        );
        assert_eq!(super::camel_words("Name"), ["name"]);
    }

    /// A `<SYNTAX>` abort carries no member pair; the line number is still the useful half.
    #[test]
    fn a_syntax_abort_still_yields_its_line() {
        let f =
            parse_abort_frame("ERROR: <SYNTAX> 3 RunUser+1^IrisDevTmp.Runc793621a6a7d.1").unwrap();
        assert_eq!(f.signal, "SYNTAX");
        assert_eq!(f.line, Some(1));
        assert_eq!(f.class, None);
    }

    // ─── #166: a failure envelope must not be shaped like a zeroed scoreboard ───

    /// `failed: 0` beside `success: false` is exactly what a naive green-check reaches for.
    /// The skills plugin's `tdd_first_green.py` was safe only by the accident of keying on
    /// `success`/`outcome` instead. Same family as #123, #143, #153 and #164: a failure must
    /// never be answered with a negative FACT.
    #[test]
    fn no_failure_envelope_carries_a_zeroed_counter() {
        let src = include_str!("mod.rs");
        // Scope to the handlers — `include_str!` also reads this test's own literals.
        let handlers = src
            .split("mod tests")
            .next()
            .expect("handlers precede the test module");
        for frag in ["\"total\": 0,", "\"passed\": 0,", "\"failed\": 0,"] {
            assert!(
                !handlers.contains(frag),
                "a zeroed counter ({frag}) is being emitted; a failure envelope must carry \
                 the cause, not a scoreboard describing a run that did not happen"
            );
        }
    }
}
