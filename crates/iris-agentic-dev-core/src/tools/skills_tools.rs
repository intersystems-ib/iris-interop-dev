//! skill, skill_community, kb, agent_info tools via docker exec + ^SKILLS global.

use crate::iris::connection::IrisConnection;
use crate::objectscript::os_str_expr;

/// Issue #67: write one `^SKILLS` entry. Each field is its own escaped ObjectScript
/// expression, concatenated with the pipe separators the readers `$piece` back apart —
/// the old form pasted all four into ONE literal after C-style `\"` escaping, so a
/// description or a skill body containing a quote (or, for an installed skill, a newline,
/// which a literal cannot hold at all) produced code that could not compile.
fn skills_set_code(name: &str, description: &str, body: &str, now: &str) -> String {
    format!(
        "set ^SKILLS({})={}_\"|\"_{}_\"|0|\"_{} write \"ok\"",
        os_str_expr(name),
        os_str_expr(description),
        os_str_expr(body),
        os_str_expr(now),
    )
}
// ── ^SKILLS → JSON (issue #119, upstream intersystems-community/iris-agentic-dev#119) ──
//
// Every reader of `^SKILLS` has to turn a pipe-delimited global into JSON. Hand-rolling
// that concatenation is what broke `skill_list`/`skill_search`/`skill_describe` in
// `tools/mod.rs`: they pasted the RAW value into an array literal, never emitted the
// subscript (the skill NAME), and `serde_json` then rejected the whole payload — silently,
// as an empty registry. These builders are the ONE place that assembly happens.
//
// `$translate(x, q_$CHAR(13,10), "   ")` — the form the `handle_skill` arms below used —
// is not enough either: it neutralises quote/CR/LF but NOT backslash, which JSON also
// requires escaped. Verified against IRIS 2026.2, name `a\qb`:
//     $translate form -> {"name":"a\qb"}   <- serde: `Invalid \escape`, silently []
//     %ToJSON form    -> {"name":"a\\qb"}  <- correct
//
// `%DynamicObject`/`%DynamicArray` + `%ToJSON()` escapes for us. Verified live:
//     name `quo"te`   -> "quo\"te"          (lossless)
//     desc with CRLF  -> "desc\r\nline2"    (escaped — payload stays on ONE line)
//     empty global    -> []                 (not `]`)
//
// The pipe is NOT escaped and must not be: it is the storage delimiter `skills_set_code`
// writes, so `$piece` truncates a description at its first pipe. That is a `^SKILLS`
// on-disk-format limitation, not a JSON one — the emitted JSON stays valid either way.
// Changing it means changing the storage format; deliberately out of scope for #119.
//
// `do arr.%ToJSON(st)` streams into a temp stream. `write arr.%ToJSON()` would materialise
// the whole document as one string and can hit <MAXSTRING> on a large registry.
//
// The leading/trailing `write !` fence the payload onto its own line: the transport is
// `docker exec -i <c> iris session`, which interleaves `USER>` prompts with output, and
// `strip_iris_banner` only drops a prompt line with nothing else on it.
//
// #119 follow-up — the payload must also be pure 7-bit ASCII. `%ToJSON` emits a non-ASCII
// character RAW, and the transport is a `docker exec` terminal device, which is not charset
// transparent: verified live on IRIS 2026.2, `^SKILLS("unicode-café-中")` came back as
// `{"name":"unicode-caf<0xE9>-?"}` — the CJK characters replaced by literal `?` by the
// device and `é` emitted as the single byte 0xE9, which `String::from_utf8_lossy` in
// `IrisConnection::execute` then turned into U+FFFD. isError was false and success was
// true: a silently wrong answer, exactly the class of bug #119 exists to kill. So every
// character above 0x7E is re-emitted as a `\uXXXX` JSON escape (`os_json_ascii_emit`)
// before it ever reaches the device. `\uXXXX` is valid JSON everywhere a raw character is,
// `serde_json` decodes it back to the original character, and an ASCII payload cannot be
// mangled by ANY device translation table.

const SKILLS_GLOBAL: &str = "^SKILLS";

/// Statements that write the JSON of `var` (a `%DynamicObject`/`%DynamicArray`) to the
/// current device as pure ASCII, fenced onto its own line.
///
/// `%ToJSON(st)` streams into a temp stream rather than materialising the document as a
/// string (<MAXSTRING> on a large registry), then each chunk is re-emitted with every
/// character above 0x7E replaced by its `\uXXXX` JSON escape.
///
/// Chunking is safe at any boundary: IRIS holds an astral character as a UTF-16 surrogate
/// PAIR, each half is escaped independently, and `😀` reassembles in the JSON
/// text no matter which chunk each half landed in. (This is why the escape is applied to
/// the JSON text and not `$zconvert(x,"O","UTF8")` to each value: splitting a surrogate
/// pair across a `$zconvert` call would corrupt it, and UTF-8 bytes 0x80-0x9F are not
/// guaranteed to survive the device's output translation table either.)
fn os_json_ascii_emit(var: &str) -> String {
    format!(
        r#"set st=##class(%Stream.TmpCharacter).%New() do {var}.%ToJSON(st) do st.Rewind() write ! while 'st.AtEnd {{ set ch=st.Read(4000) set esc="" for i=1:1:$length(ch) {{ set c=$ascii(ch,i) set esc=esc_$select(c<128:$char(c),1:"\u"_$extract("000"_$zhex(c),*-3,*)) }} write esc }} write !"#
    )
}

/// ObjectScript that writes a JSON array of `^SKILLS` entries to the current device.
///
/// `filter_lower` is an ALREADY-lowercased substring matched against `name|value`;
/// `None` (or `Some("")`) lists everything — `$find(x,"")` is 1, so an empty needle
/// matches every row. `include_body` adds the potentially large skill body; `list`/`search`
/// leave it out so a listing does not ship every body.
pub(crate) fn skills_list_json_code(filter_lower: Option<&str>, include_body: bool) -> String {
    let body = if include_body {
        r#""body":($piece(data,"|",2)),"#
    } else {
        ""
    };
    // #67: the needle is embedded from Rust, so it goes through os_str_expr — never
    // through a hand-rolled `replace('"', "")`, which cannot survive a control character.
    //
    // The haystack is name + description + body, NOT `key_"|"_data`: `data` is the
    // pipe-delimited record, so the old form put a `|` in every haystack and a search for
    // "|" matched the entire registry. Joining the extracted pieces with a space means a
    // pipe only matches when a field genuinely contains one.
    let filter = match filter_lower.unwrap_or("") {
        "" => String::new(),
        f => format!(
            r#"continue:'$find($zconvert(key_" "_$piece(data,"|",1)_" "_$piece(data,"|",2),"L"),{})  "#,
            os_str_expr(f)
        ),
    };
    format!(
        r#"set arr=[] set key="" for {{ set key=$order({g}(key)) quit:key=""  set data=$get({g}(key)) {filter}do arr.%Push({{"name":(key),"description":($piece(data,"|",1)),{body}"usage_count":(+$piece(data,"|",3)),"created_at":($piece(data,"|",4))}}) }} {emit}"#,
        g = SKILLS_GLOBAL,
        filter = filter,
        body = body,
        emit = os_json_ascii_emit("arr")
    )
}

/// ObjectScript that writes ONE `^SKILLS` entry as a JSON object, or `{"found":0}` when
/// the subscript does not exist.
///
/// The `found` sentinel is what lets the caller tell "IRIS answered, no such skill" from
/// "IRIS never answered" — an empty payload cannot, because a dropped connection produces
/// one too. `$data(...)=0` rather than `data=""` so a skill stored with an empty value is
/// still reported as found.
pub(crate) fn skills_describe_json_code(name: &str) -> String {
    format!(
        r#"set skname={n} set data=$get({g}(skname)) if $data({g}(skname))=0 {{ set o={{"found":0}} }} else {{ set o={{"found":1,"name":(skname),"description":($piece(data,"|",1)),"body":($piece(data,"|",2)),"usage_count":(+$piece(data,"|",3)),"created_at":($piece(data,"|",4))}} }} {emit}"#,
        n = os_str_expr(name),
        g = SKILLS_GLOBAL,
        emit = os_json_ascii_emit("o")
    )
}

/// Why a `^SKILLS` read produced no data. #119: collapsing these two into "return an
/// empty list" is the bug — a caller cannot tell a broken payload from an empty registry,
/// and both look identical to "IRIS unreachable".
#[derive(Debug)]
pub(crate) enum SkillsReadError {
    /// Never got an answer: no connection, no IRIS_CONTAINER, docker/HTTP failure.
    Unreachable(String),
    /// Got an answer that is not the JSON we asked for. Carries what came back.
    Unparseable { raw: String, parse_error: String },
}

/// Last line of `raw` that starts a JSON value. `docker exec … iris session` can leave a
/// `USER>` prompt on the payload's own line, and `strip_iris_banner` only drops a prompt
/// line with nothing else on it — so pick the payload out rather than trusting `trim()`.
pub(crate) fn extract_json_line(raw: &str) -> &str {
    // `rfind` (not `find`): take the LAST candidate line, so a banner or a prompt echoed
    // before the payload cannot win over the payload itself. clippy::filter_next requires
    // this form over `.filter(..).next_back()`.
    raw.lines()
        .map(str::trim)
        .rfind(|l| l.starts_with('[') || l.starts_with('{'))
        .unwrap_or("")
}

/// Run one of the builders above and parse what comes back. The single read path for
/// every `^SKILLS` reader in the crate.
pub(crate) async fn read_skills_json(
    iris: &IrisConnection,
    code: &str,
    ns: &str,
) -> Result<serde_json::Value, SkillsReadError> {
    let raw = iris
        .execute(code, ns)
        .await
        .map_err(|e| SkillsReadError::Unreachable(e.to_string()))?;
    classify_skills_payload(&raw)
}

/// Classify what came back off the wire. Split out from `read_skills_json` so the
/// empty-payload rule below is testable without a live connection.
///
/// #119 follow-up: `IrisConnection::execute` discards the child's exit status, so a
/// `docker exec` that FAILED — missing or stopped container, daemon down, permission
/// denied — comes back as `Ok("")` rather than an error. Every builder above ends in a
/// `write`, so a reachable IRIS always answers something: an empty registry writes `[]`,
/// not nothing at all. Empty therefore means the transport produced no output, and
/// calling that `Unparseable` would make the tool assert "IRIS WAS reachable" about an
/// instance it never reached — a worse answer than the empty list this fix replaced.
pub(crate) fn classify_skills_payload(raw: &str) -> Result<serde_json::Value, SkillsReadError> {
    if raw.trim().is_empty() {
        return Err(SkillsReadError::Unreachable(
            "the execution transport returned no output at all (a failed `docker exec` exits \
             non-zero with empty stdout, which reaches this crate as an empty payload)"
                .into(),
        ));
    }
    let line = extract_json_line(raw);
    serde_json::from_str(line).map_err(|e| SkillsReadError::Unparseable {
        raw: raw.chars().take(400).collect(),
        parse_error: e.to_string(),
    })
}

/// #89: the skill count is the LENGTH of the array `skills_list_json_code` emits — never a
/// fallback zero. `agent_info(what=stats)` used to run its OWN `write count` loop over
/// `^SKILLS` and funnel every failure through
/// `.unwrap_or_default().trim().parse().unwrap_or(0)`, so no IRIS_CONTAINER, a failed
/// `docker exec` and a `<UNDEFINED>` all printed `skill_count: 0` with `success: true` —
/// reproduced live against a registry that genuinely held 2 skills, in the same session
/// where `skill(action=list)` correctly answered DOCKER_REQUIRED.
///
/// `[]` is a genuinely empty registry (0). Anything that is not an array means IRIS
/// answered something we did not ask for: a parse failure, NOT an empty registry.
/// Deliberately not `map_or(0, …)` — that is the same silent zero, one refactor from
/// re-opening this issue.
pub(crate) fn skills_count_from_payload(v: &serde_json::Value) -> Result<usize, SkillsReadError> {
    v.as_array()
        .map(Vec::len)
        .ok_or_else(|| SkillsReadError::Unparseable {
            raw: v.to_string().chars().take(400).collect(),
            parse_error: "^SKILLS listing was not a JSON array".into(),
        })
}

/// Turn a `SkillsReadError` into the envelope (issue #2 — one failure surface).
/// `DOCKER_REQUIRED` is the code `skill_forget` already uses for an unreachable IRIS, so
/// callers that branch on it keep working; `SKILLS_PARSE_FAILED` is new and says
/// explicitly that IRIS WAS reached.
pub(crate) fn skills_read_fail(
    tool: &str,
    ns: &str,
    e: SkillsReadError,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match e {
        // #101: `Unreachable` is a string from whatever failed, and this arm rendered EVERY
        // such string as DOCKER_REQUIRED + "Set IRIS_CONTAINER=<container_name>" — advice
        // about the one variable that is provably not the problem once IRIS has answered.
        //
        // Today every producer of this error is `IrisConnection::execute`, which is docker-exec
        // only and fails with the literal "DOCKER_REQUIRED" before any HTTP, so a wrong
        // password genuinely is not why a skills read failed and DOCKER_REQUIRED is the honest
        // answer there — the live observation that correct and wrong credentials produce
        // identical output has that benign cause. This guard is what keeps that true by
        // construction rather than by coincidence: the moment a skills read learns to go over
        // HTTP, an auth refusal must not be renamed into a container problem on its way out.
        SkillsReadError::Unreachable(err)
            if crate::tools::envelope::auth_error_code(&err.to_string()).is_some() =>
        {
            let code = crate::tools::envelope::auth_error_code(&err.to_string())
                .expect("guarded by the match arm");
            crate::tools::envelope::fail_with(
                code,
                &format!(
                    "{tool} could not read ^SKILLS in namespace '{ns}': {err}. IRIS answered \
                     and rejected the request, so it is reachable — this is a credentials \
                     failure, not a missing container."
                ),
                serde_json::json!({"namespace": ns, "source": "^SKILLS"}),
            )
        }
        SkillsReadError::Unreachable(err) => crate::tools::envelope::fail_with(
            "DOCKER_REQUIRED",
            &format!(
                "{tool} could not reach IRIS to read ^SKILLS in namespace '{ns}': {err}. \
                 Set IRIS_CONTAINER=<container_name>.{}",
                super::DOCKER_REQUIRED_HINT
            ),
            serde_json::json!({"namespace": ns, "source": "^SKILLS"}),
        ),
        SkillsReadError::Unparseable { raw, parse_error } => crate::tools::envelope::fail_with(
            "SKILLS_PARSE_FAILED",
            &format!(
                "{tool} read ^SKILLS in namespace '{ns}' but could not parse the response as \
                 JSON: {parse_error}. IRIS WAS reachable — this is NOT an empty registry."
            ),
            serde_json::json!({
                "namespace": ns,
                "source": "^SKILLS",
                "parse_error": parse_error,
                "raw_excerpt": raw,
            }),
        ),
    }
}

use crate::tools::ToolCallEntry;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::VecDeque;

fn ok_json(v: serde_json::Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    Ok(rmcp::model::CallToolResult::success(vec![
        rmcp::model::Content::text(v.to_string()),
    ]))
}
fn err_json(code: &str, msg: &str) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    crate::tools::envelope::fail(code, msg)
}

fn learning_enabled() -> bool {
    std::env::var("OBJECTSCRIPT_LEARNING")
        .map(|v| v != "false")
        .unwrap_or(true)
}

/// Namespace that holds `^SKILLS` / `^KBCHUNKS` (issue #85).
///
/// `OBJECTSCRIPT_SKILLMCP_NAMESPACE` -> the CONNECTION's namespace -> `USER`.
///
/// The old form read only the env var and defaulted to `USER`, so on the documented dev
/// environment (`IRIS_NAMESPACE=APP`) every `skill_*` tool read `^SKILLS` in a namespace
/// the operator never selected and answered `{"count":0,"skills":[]}` with `success:true`
/// — a false empty registry, the exact misreading #119 set out to kill. Verified live:
/// a skill plainly present in APP was invisible to `skill_list` until
/// `OBJECTSCRIPT_SKILLMCP_NAMESPACE=APP` was set, and `skill action=propose` WROTE its
/// new skill into USER at the same time.
///
/// The connection namespace is NOT re-read from `IRIS_NAMESPACE` here.
/// `IrisConnection.namespace` is where every input settles — `--namespace`,
/// `IRIS_NAMESPACE`, `.iris-agentic-dev.toml`, container/port discovery — so
/// `interop::resolve_namespace` is reused verbatim rather than adding a fourth reader of
/// the env var that would drift from it (and would miss the `--host --namespace`
/// explicit-flag path entirely).
///
/// `resolve_namespace` also treats a blank override as absent, which matters: the value
/// reaches `iris session IRIS -U <ns>` in `IrisConnection::execute`, and `-U ""` is not a
/// namespace.
///
/// EVERY reader AND writer of `^SKILLS`/`^KBCHUNKS` resolves through this ONE function —
/// that is why it takes the connection instead of letting call sites decide, and why a
/// read/write namespace split cannot arise.
pub fn skills_namespace(iris: Option<&IrisConnection>) -> String {
    let explicit = std::env::var("OBJECTSCRIPT_SKILLMCP_NAMESPACE").ok();
    crate::tools::interop::resolve_namespace(explicit.as_deref(), iris)
}

async fn xecute(
    iris: &IrisConnection,
    _client: &reqwest::Client,
    code: &str,
    namespace: &str,
) -> anyhow::Result<String> {
    // /action/xecute does not exist in Atelier REST — use docker exec path
    iris.execute(code, namespace).await
}

// ── skill ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillParams {
    /// Action: list, describe, search, forget, propose
    pub action: String,
    pub name: Option<String>,
    pub query: Option<String>,
}

pub async fn handle_skill(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: SkillParams,
    history: &std::sync::Mutex<VecDeque<ToolCallEntry>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    if !learning_enabled() {
        return err_json(
            "LEARNING_DISABLED",
            "Set OBJECTSCRIPT_LEARNING=true to enable skills",
        );
    }

    let ns = skills_namespace(Some(iris));

    match p.action.as_str() {
        "list" => {
            // #119: was a hand-built `$translate` embed that emitted `\q` for a name
            // containing a backslash — invalid JSON, silently swallowed by unwrap_or([]).
            let code = skills_list_json_code(None, false);
            match read_skills_json(iris, &code, &ns).await {
                Ok(skills) => ok_json(serde_json::json!({
                    "success": true, "skills": skills,
                    "namespace": ns, "source": "^SKILLS"
                })),
                Err(e) => skills_read_fail("skill(action=list)", &ns, e),
            }
        }
        "describe" => {
            let name = p.name.as_deref().unwrap_or("");
            let code = skills_describe_json_code(name);
            match read_skills_json(iris, &code, &ns).await {
                // #119: `found` is the sentinel — an empty payload used to mean BOTH
                // "no such skill" and "the read failed". `usage_count` is now a number,
                // matching what action=list already returned.
                Ok(v) if v.get("found").and_then(|f| f.as_i64()) == Some(1) => {
                    ok_json(serde_json::json!({
                        "success": true,
                        "name": name,
                        "description": v.get("description").cloned().unwrap_or(serde_json::json!("")),
                        "body": v.get("body").cloned().unwrap_or(serde_json::json!("")),
                        "usage_count": v.get("usage_count").cloned().unwrap_or(serde_json::json!(0)),
                        "created_at": v.get("created_at").cloned().unwrap_or(serde_json::json!("")),
                    }))
                }
                Ok(_) => err_json("NOT_FOUND", &format!("Skill '{}' not found", name)),
                Err(e) => skills_read_fail("skill(action=describe)", &ns, e),
            }
        }
        "search" => {
            let query = p.query.as_deref().unwrap_or("").to_lowercase();
            let code = skills_list_json_code(Some(&query), false);
            match read_skills_json(iris, &code, &ns).await {
                Ok(results) => ok_json(serde_json::json!({
                    "success": true, "query": query, "results": results,
                    "namespace": ns, "source": "^SKILLS"
                })),
                Err(e) => skills_read_fail("skill(action=search)", &ns, e),
            }
        }
        "forget" => {
            let name = p.name.as_deref().unwrap_or("");
            let code = format!("kill ^SKILLS({}) write \"ok\"", os_str_expr(name));
            xecute(iris, client, &code, &ns).await.unwrap_or_default();
            ok_json(serde_json::json!({"success": true, "name": name, "action": "forgotten"}))
        }
        "propose" => {
            let calls: Vec<String> = {
                let h = history.lock().unwrap();
                if h.len() < 5 {
                    return err_json(
                        "INSUFFICIENT_HISTORY",
                        &format!(
                            "Need at least 5 tool calls to propose a skill, have {}",
                            h.len()
                        ),
                    );
                }
                h.iter().rev().take(20).map(|c| c.tool.clone()).collect()
            };
            // Synthesize skill name from most frequent tool
            let mut freq = std::collections::HashMap::new();
            for t in &calls {
                *freq.entry(t.as_str()).or_insert(0u32) += 1;
            }
            let top = freq
                .iter()
                .max_by_key(|e| e.1)
                .map(|e| *e.0)
                .unwrap_or("workflow");
            let skill_name = format!("auto-{}-{}", top, chrono::Utc::now().timestamp() % 10000);
            let description = format!(
                "Auto-synthesized from recent tool calls: {}",
                calls.join(", ")
            );
            let body = format!(
                "Recent workflow: {}",
                calls
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" → ")
            );
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let code = skills_set_code(&skill_name, &description, &body, &now);
            xecute(iris, client, &code, &ns).await.unwrap_or_default();
            ok_json(serde_json::json!({
                "success": true,
                "skill": {"name": skill_name, "description": description, "body": body}
            }))
        }
        other => err_json(
            "INVALID_PARAM",
            &format!(
                "Unknown action='{}'. Use: list, describe, search, forget, propose",
                other
            ),
        ),
    }
}

// ── skill_community ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillCommunityParams {
    /// Action: list or install
    pub action: String,
    pub package: Option<String>,
}

pub async fn handle_skill_community(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: SkillCommunityParams,
    registry: &crate::skills::SkillRegistry,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    if !learning_enabled() {
        return err_json(
            "LEARNING_DISABLED",
            "Set OBJECTSCRIPT_LEARNING=true to enable community skills",
        );
    }

    match p.action.as_str() {
        "list" => {
            let items: Vec<serde_json::Value> = registry
                .list_skills()
                .iter()
                .map(|s| serde_json::json!({"name": s.name, "description": s.description}))
                .collect();
            ok_json(serde_json::json!({"success": true, "skills": items}))
        }
        "install" => {
            let pkg = p.package.as_deref().unwrap_or("");
            if pkg.is_empty() {
                return err_json("INVALID_PARAM", "package name required for action=install");
            }
            let skill_opt = registry
                .list_skills()
                .iter()
                .find(|s| s.name == pkg)
                .map(|s| (s.name.clone(), s.description.clone(), s.content.clone()));
            match skill_opt {
                Some((sname, sdesc, scontent)) => {
                    let ns = skills_namespace(Some(iris));
                    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                    let code = skills_set_code(&sname, &sdesc, &scontent, &now);
                    xecute(iris, client, &code, &ns).await.unwrap_or_default();
                    ok_json(serde_json::json!({"success": true, "installed": sname}))
                }
                None => err_json("NOT_FOUND", &format!("Community skill '{}' not found", pkg)),
            }
        }
        other => err_json(
            "INVALID_PARAM",
            &format!("Unknown action='{}'. Use: list, install", other),
        ),
    }
}

// ── kb ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KbParams {
    /// Action: index or recall
    pub action: String,
    /// File path for index, query for recall
    pub path: Option<String>,
    pub query: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize {
    5
}

pub async fn handle_kb(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: KbParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    if !learning_enabled() {
        return err_json(
            "LEARNING_DISABLED",
            "Set OBJECTSCRIPT_LEARNING=true to enable KB",
        );
    }

    let ns = skills_namespace(Some(iris));

    match p.action.as_str() {
        "index" => {
            let path = p.path.as_deref().unwrap_or(".");
            let workspace =
                std::env::var("OBJECTSCRIPT_WORKSPACE").unwrap_or_else(|_| ".".to_string());
            let base = if path == "." {
                workspace.as_str()
            } else {
                path
            };

            let mut indexed = 0usize;
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    let fp = entry.path();
                    if fp
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e == "md" || e == "txt")
                        .unwrap_or(false)
                    {
                        if let Ok(content) = std::fs::read_to_string(&fp) {
                            let fname = fp.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            let chunk: String = content.chars().take(2000).collect();
                            // #67: newlines cannot appear inside an ObjectScript literal at
                            // all — the old `\\n` stored two literal characters. os_str_expr
                            // splices them as $CHAR(10), so the chunk is stored verbatim.
                            let code = format!(
                                "set ^KBCHUNKS({})={} write \"ok\"",
                                os_str_expr(fname),
                                os_str_expr(&chunk)
                            );
                            xecute(iris, client, &code, &ns).await.unwrap_or_default();
                            indexed += 1;
                        }
                    }
                }
            }
            ok_json(serde_json::json!({"success": true, "indexed": indexed, "path": base}))
        }
        "recall" => {
            let query = p.query.as_deref().unwrap_or("").to_lowercase();
            let top_k = p.top_k;
            // Bug 9: use separator variable so empty results yield "[]" not "]".
            // #67: the loop's postconditional needs its own parentheses. ObjectScript has no
            // operator precedence — `key="" || count>=N` groups as `((key="")||count)>=N`,
            // which is false on the terminating pass, so the loop read ^KBCHUNKS("") and died
            // with <SUBSCRIPT>. Every recall answered [] because the error was swallowed.
            let code = format!(
                "set q=$CHAR(34) set key=\"\" set out=\"[\" set sep=\"\" set count=0 for {{ set key=$order(^KBCHUNKS(key)) quit:((key=\"\")||(count>={top_k}))  set data=$get(^KBCHUNKS(key)) if $find($zconvert(data,\"L\"),{query})>0 {{ set out=out_sep_\"{{\"_q_\"file\"_q_\":\"_q_$translate(key,q_$CHAR(13,10),\"   \")_q_\",\"_q_\"excerpt\"_q_\":\"_q_$translate($extract(data,1,200),q_$CHAR(13,10),\"   \")_q_\"}}\" set sep=\",\" set count=count+1 }} }} set out=out_\"]\" write out",
                query = os_str_expr(&query),
                top_k = top_k,
            );
            let raw = xecute(iris, client, &code, &ns).await.unwrap_or_default();
            let results: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!([]));
            ok_json(serde_json::json!({"success": true, "query": query, "results": results}))
        }
        other => err_json(
            "INVALID_PARAM",
            &format!("Unknown action='{}'. Use: index, recall", other),
        ),
    }
}

// ── agent_info ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInfoParams {
    /// What to return: stats or history
    pub what: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

/// #99: how many tool calls this session recorded, with the history mutex's poison
/// RECOVERED rather than read as "no calls".
///
/// `agent_stats` computed this as `self.history.lock().map(|h| h.len()).unwrap_or(0)`, so a
/// poisoned mutex — another tool panicked while holding it — reported `session_calls: 0`
/// for a history that is still perfectly intact. That is the same false zero #89 removed
/// from `agent_history` and from `agent_info(what=history)`, surviving one function over.
/// `record_call` only pushes and pops a `VecDeque`, so a panic cannot leave the deque
/// structurally invalid: the entries are there and must be counted.
pub(crate) fn session_call_count(history: &std::sync::Mutex<VecDeque<ToolCallEntry>>) -> usize {
    history.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// #99: the success payload of `agent_stats` / `agent_info(what=stats)`, as a pure function.
///
/// Split out so the field NAMES and the one rule that matters — a count appears only when a
/// count was actually read — are unit-testable with no IRIS and no docker. Every failure
/// path returns `skills_read_fail`'s envelope instead of reaching this function at all, so
/// there is no branch here that could emit a zero nobody measured.
///
/// `subscribed` is `Some((skills, kb_items))` for `agent_stats`, which additionally reports
/// the in-process `--subscribe` population, and `None` for `agent_info(what=stats)`, whose
/// payload must stay field-for-field what it emits today.
///
/// `status: "ok"` rides with `subscribed` because it is `agent_stats`' own legacy field. It
/// is kept rather than dropped: it is only ever emitted on success (the failure envelope
/// carries `success: false` and no `status` at all), so it is not a lie, and removing it
/// would break an existing consumer for nothing. `success: true` is added beside it as the
/// field that actually carries the meaning.
pub(crate) fn agent_stats_json(
    skill_count: usize,
    ns: &str,
    session_calls: usize,
    learning: bool,
    subscribed: Option<(usize, usize)>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "success": true,
        "skill_count": skill_count,
        "session_calls": session_calls,
        "learning_enabled": learning,
        // #85: say WHICH registry the count came from. Every sibling reports these two.
        "namespace": ns,
        "source": SKILLS_GLOBAL,
    });
    if let Some((skills, kb_items)) = subscribed {
        v["status"] = serde_json::json!("ok");
        v["subscribed_skill_count"] = serde_json::json!(skills);
        v["subscribed_kb_item_count"] = serde_json::json!(kb_items);
        v["subscribed_source"] = serde_json::json!(
            "--subscribe github packages (in-process, populated at startup only)"
        );
    }
    v
}

/// #99: the ONE implementation of "learning agent status", shared by `agent_stats` and
/// `agent_info(what=stats)` so the two can no longer drift apart.
///
/// `agent_stats` used to report `self.registry.list_skills().len()` — the in-process
/// `SkillRegistry` — under the bare name `skill_count`, with no namespace and no source.
/// That number is a STARTUP CONSTANT: `SkillRegistry::load_from_github` takes `&mut self`
/// and is called only from `--subscribe <owner/repo>` before the server starts, and the
/// registry is held behind an `Arc` with no interior mutability, so in every session
/// started without `--subscribe` it is frozen at 0 for the whole process lifetime.
///
/// Reproduced live, twice, in one session against the dev instance: `agent_stats` answered
/// `skill_count: 0` while `skill_list` and `agent_info(what=stats)` both answered 3 from
/// `^SKILLS`; and with IRIS unreachable it answered `skill_count: 0, status: "ok"` while
/// both siblings correctly answered DOCKER_REQUIRED. One field, three different meanings —
/// "the registry is empty", "3 skills are present" and "IRIS was never reached" — and
/// nothing in the payload to tell them apart. `agent_stats` is also the only agent_* skill
/// count that survives into the `merged` profile, which prunes `agent_info`.
///
/// So `skill_count` MEANS `^SKILLS` in the connection's namespace here too — same builder,
/// same namespace resolution (#85), same failure classification (#119) as `skill_list` —
/// and the registry population is reported BESIDE it as `subscribed_*`, a name that says
/// what it is. Nothing is dropped; the wrong number simply stops wearing the right name.
///
/// A failed read returns the shared envelope carrying NO count at all (#89/#119): "could
/// not read it" must never arrive as "there are none".
///
/// COST: this makes `agent_stats` fallible and no longer free. Measured on the dev instance:
/// 4.0 / 4.5 ms before (it touched no IRIS) against 51.7 / 54.7 ms for
/// `agent_info(what=stats)` and 49.8 / 60.9 ms for `skill_list`, both of which do exactly
/// this one `^SKILLS` read. `agent_history` still needs no connection.
pub async fn agent_stats_result(
    tool: &str,
    iris: Option<&IrisConnection>,
    history: &std::sync::Mutex<VecDeque<ToolCallEntry>>,
    registry: Option<&crate::skills::SkillRegistry>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    // #85: the namespace resolves from the CONNECTION. With no connection there is nothing
    // to resolve and this falls to "USER" — honest, because the very next line reports that
    // IRIS was never reached. Identical to `skill_list`, deliberately: going through
    // `get_iris_reloaded().await?` instead would raise a protocol-level McpError rather than
    // this envelope, and callers branch on `error_code`.
    let ns = skills_namespace(iris);
    let Some(iris) = iris else {
        return skills_read_fail(
            tool,
            &ns,
            SkillsReadError::Unreachable("no IRIS connection configured".into()),
        );
    };
    let code = skills_list_json_code(None, false);
    let skill_count = match read_skills_json(iris, &code, &ns)
        .await
        .and_then(|v| skills_count_from_payload(&v))
    {
        Ok(n) => n,
        // Never `unwrap_or(0)`, never `map_or(0, …)`: that is the bug this function exists
        // to remove, and it is one refactor away at all times.
        Err(e) => return skills_read_fail(tool, &ns, e),
    };
    ok_json(agent_stats_json(
        skill_count,
        &ns,
        session_call_count(history),
        learning_enabled(),
        registry.map(|r| (r.list_skills().len(), r.list_kb_items().len())),
    ))
}

pub async fn handle_agent_info(
    iris: &IrisConnection,
    // #89: the ^SKILLS read goes through read_skills_json -> IrisConnection::execute
    // (docker exec); no HTTP client is used. Kept for signature parity with the siblings.
    _client: &reqwest::Client,
    p: AgentInfoParams,
    history: &std::sync::Mutex<VecDeque<ToolCallEntry>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match p.what.as_str() {
        // #89: count what `skill(action=list)` lists — SAME builder, SAME namespace (#85),
        // SAME classification (#119) — so the two tools cannot disagree about the registry.
        // #99: and SAME function as `agent_stats`, which had re-forked this arm from the
        // in-process `SkillRegistry` and reintroduced #89's false zero one tool over.
        // `registry: None` means no `subscribed_*` and no `status`, so this tool's payload
        // is field-for-field what it emitted before the two implementations were merged.
        "stats" => agent_stats_result("agent_info(what=stats)", Some(iris), history, None).await,
        "history" => {
            let limit = p.limit;
            // #89: same poison recovery — `.unwrap_or_default()` on the lock rendered an
            // unreadable history as `calls: []` with success:true, i.e. "no history"
            // instead of "history unreadable". Identical behaviour in every non-panic case.
            let guard = history.lock().unwrap_or_else(|e| e.into_inner());
            let calls: Vec<serde_json::Value> = guard
                .iter()
                .rev()
                .take(limit)
                .map(|c| {
                    serde_json::json!({
                        "tool": c.tool,
                        "success": c.success,
                        "ago_secs": c.timestamp.elapsed().as_secs(),
                    })
                })
                .collect();
            ok_json(serde_json::json!({"success": true, "calls": calls}))
        }
        other => err_json(
            "INVALID_PARAM",
            &format!("Unknown what='{}'. Use: stats, history", other),
        ),
    }
}

#[cfg(test)]
mod objectscript_escaping_tests {
    use super::*;

    /// #67: ObjectScript doubles a quote inside a literal; `\\"` escapes nothing, so the
    /// backslash survives and the quote ends the string. Nothing generated here may use it.
    #[test]
    fn a_quote_in_any_field_is_doubled_never_backslashed() {
        let code = skills_set_code(
            r#"say "hi""#,
            r#"a "quoted" description"#,
            "body",
            "2026-08-25T00:00:00Z",
        );
        assert!(code.contains(r#""say ""hi""""#), "{code}");
        assert!(
            !code.contains(r#"\""#),
            "no C-style escaping may survive: {code}"
        );
    }

    /// A skill body is multi-line. A literal cannot span source lines at all — the old code
    /// wrote a two-character `\\n` instead, which stored a backslash and an `n`.
    #[test]
    fn a_newline_in_a_body_is_spliced_as_char_not_written_into_a_literal() {
        let code = skills_set_code("s", "d", "line one\nline two", "2026-08-25T00:00:00Z");
        assert!(code.contains("$CHAR(10)"), "{code}");
        assert!(!code.contains(r#"\n"#), "{code}");
        assert_eq!(
            code.lines().count(),
            1,
            "generated code stays on one line: {code}"
        );
    }

    // ── #119: ^SKILLS -> JSON assembly ───────────────────────────────────────

    /// The bug: the value went in, the subscript (the skill NAME) never did.
    #[test]
    fn list_code_emits_the_subscript_as_name() {
        let code = skills_list_json_code(None, false);
        assert!(code.contains(r#""name":(key)"#), "{code}");
        assert!(code.contains("$order(^SKILLS(key))"), "{code}");
        assert!(
            code.contains(r#""created_at":($piece(data,"|",4))"#),
            "{code}"
        );
    }

    /// The raw value must never be concatenated into the array literal again.
    #[test]
    fn list_code_never_concatenates_the_raw_value() {
        let code = skills_list_json_code(None, false);
        assert!(!code.contains("_sep_skill"), "{code}");
        assert!(!code.contains("result_sep"), "{code}");
        assert!(code.contains("%Push"), "{code}");
        assert!(code.contains("%ToJSON"), "{code}");
    }

    /// `do arr.%ToJSON(st)` streams into a temp stream; `write arr.%ToJSON()` (or
    /// `set j=arr.%ToJSON()`) materialises the whole document as one string and can hit
    /// <MAXSTRING> on a large registry.
    #[test]
    fn list_code_streams_rather_than_materialising() {
        let code = skills_list_json_code(None, false);
        assert!(code.contains("do arr.%ToJSON(st)"), "{code}");
        assert!(code.contains("%Stream.TmpCharacter"), "{code}");
        assert!(!code.contains("write arr.%ToJSON"), "{code}");
        assert!(!code.contains("=arr.%ToJSON"), "{code}");
    }

    /// #67: the search needle is embedded from Rust, so it goes through os_str_expr.
    #[test]
    fn search_needle_is_quote_doubled_never_backslashed() {
        let code = skills_list_json_code(Some(r#"say "hi""#), false);
        assert!(code.contains(r#""say ""hi""""#), "{code}");
        assert!(
            !code.contains(r#"\""#),
            "no C-style escaping may survive: {code}"
        );
    }

    /// A literal cannot span a source line, and build_exec_class splits on '\n'.
    #[test]
    fn search_needle_with_a_newline_is_char_spliced_and_stays_one_line() {
        let code = skills_list_json_code(Some("a\nb"), false);
        assert!(code.contains("$CHAR(10)"), "{code}");
        assert!(!code.contains(r#"\n"#), "{code}");
        assert_eq!(code.lines().count(), 1, "{code}");
    }

    /// An empty needle must not emit a filter at all: `$find(x,"")` is 1, so a `continue:`
    /// clause would be dead code, and `None`/`Some("")` must not diverge.
    #[test]
    fn an_empty_filter_is_the_same_as_no_filter() {
        let none = skills_list_json_code(None, false);
        let empty = skills_list_json_code(Some(""), false);
        assert_eq!(none, empty);
        assert!(!none.contains("continue:"), "{none}");
        assert!(skills_list_json_code(Some("x"), false).contains("continue:'$find("));
    }

    /// list/search must not ship every skill body; describe must.
    #[test]
    fn body_is_opt_in() {
        assert!(!skills_list_json_code(None, false).contains(r#""body""#));
        assert!(skills_list_json_code(None, true).contains(r#""body":($piece(data,"|",2))"#));
        assert!(skills_describe_json_code("x").contains(r#""body":($piece(data,"|",2))"#));
    }

    /// A skill name is user data and can hold a quote.
    #[test]
    fn describe_code_escapes_the_name_and_carries_the_found_sentinel() {
        let code = skills_describe_json_code(r#"quo"te"#);
        assert!(code.contains(r#""quo""te""#), "{code}");
        assert!(!code.contains(r#"\""#), "{code}");
        assert!(code.contains(r#"{"found":0}"#), "{code}");
        assert!(code.contains(r#""found":1"#), "{code}");
        assert!(code.contains("$data(^SKILLS(skname))=0"), "{code}");
        assert_eq!(code.lines().count(), 1, "{code}");
    }

    /// The payload must be fenced onto its own line: the docker-exec transport
    /// interleaves `USER>` prompts and strip_iris_banner only drops BARE prompt lines.
    #[test]
    fn every_builder_fences_its_payload_with_newlines() {
        for code in [
            skills_list_json_code(None, false),
            skills_list_json_code(Some("q"), true),
            skills_describe_json_code("n"),
        ] {
            assert!(code.contains("do st.Rewind() write ! while "), "{code}");
            assert!(code.trim_end().ends_with("write !"), "{code}");
            assert_eq!(
                code.lines().count(),
                1,
                "generated code stays one line: {code}"
            );
        }
    }

    // ── #119 follow-up: non-ASCII must survive an 8-bit transport ─────────────
    //
    // Live repro this pins (IRIS 2026.2, ^SKILLS("unicode-café-中") seeded over the HTTP
    // path, read back over docker exec): skill_list answered
    //     {"name":"unicode-caf\u{fffd}-?","description":"caf\u{fffd} \u{fffd} ?? description"}
    // with isError:false / success:true, and skill_describe answered NOT_FOUND — byte for
    // byte the same envelope as a skill that really does not exist.

    /// RECEIVE: the payload must leave IRIS as pure ASCII, because the docker-exec
    /// terminal device replaces a CJK character with `?` and emits `é` as one byte.
    #[test]
    fn every_builder_emits_a_pure_ascii_payload() {
        for code in [
            skills_list_json_code(None, false),
            skills_list_json_code(Some("café"), true),
            skills_describe_json_code("unicode-café-中"),
        ] {
            assert!(
                code.is_ascii(),
                "generated source must be pure ASCII: {code}"
            );
            // every char above 0x7E is re-emitted as \uXXXX before it reaches the device
            assert!(code.contains(r#"$select(c<128:$char(c),1:"\u""#), "{code}");
            assert!(code.contains("$extract(\"000\"_$zhex(c),*-3,*)"), "{code}");
        }
    }

    /// SEND: the needle and the skill name are embedded from Rust, so a raw non-ASCII
    /// literal would be re-decoded 8-bit by `iris session` and stop matching the data.
    /// This is what made skill_describe answer a FALSE NOT_FOUND.
    #[test]
    fn a_non_ascii_needle_and_name_are_char_spliced() {
        let code = skills_list_json_code(Some("café"), false);
        assert!(code.contains("$CHAR(233)"), "{code}");
        assert!(
            !code.contains("café"),
            "no raw non-ASCII may survive: {code}"
        );

        let code = skills_describe_json_code("unicode-café-中");
        assert!(
            code.contains(r#"set skname="unicode-caf"_$CHAR(233)"#),
            "{code}"
        );
        assert!(code.contains("$CHAR(20013)"), "{code}");
        assert_eq!(code.lines().count(), 1, "{code}");
    }

    /// The escaping contract itself, checked against what live IRIS actually emitted for
    /// the `os_json_ascii_emit` loop — `serde_json` must decode it back to the ORIGINAL
    /// characters, including an astral character carried as a UTF-16 surrogate pair.
    #[test]
    fn ascii_escaped_payloads_decode_back_to_the_original_characters() {
        // Captured verbatim from IRIS 2026.2 running the emit loop over
        // {"name":"unicode-"_$char(233)_"-"_$char(20013),"desc":"caf"_$char(233)_" plain"}
        let live = r#"{"name":"unicode-\u00E9-\u4E2D","desc":"caf\u00E9 plain"}"#;
        assert!(live.is_ascii(), "the wire payload must be 7-bit");
        let v: serde_json::Value = serde_json::from_str(live).expect("must parse");
        assert_eq!(v["name"], "unicode-é-中");
        assert_eq!(v["desc"], "café plain");

        // Astral: IRIS holds 😀 as the surrogate pair 55357/56832 and escapes each half.
        // Captured verbatim for {"emoji":$char(55357)_$char(56832)}.
        let live = r#"{"emoji":"\uD83D\uDE00"}"#;
        let v: serde_json::Value = serde_json::from_str(live).expect("must parse");
        assert_eq!(v["emoji"], "😀");

        // And the shape the bug produced must NOT be mistaken for the fixed one.
        let corrupted = "{\"name\":\"unicode-\u{fffd}-?\"}";
        let v: serde_json::Value = serde_json::from_str(corrupted).unwrap();
        assert_ne!(v["name"], "unicode-é-中");
    }

    /// Pins the exact bytes the transport can wrap the payload in.
    #[test]
    fn extract_json_line_survives_an_interleaved_prompt() {
        assert_eq!(
            extract_json_line("USER>\n[{\"a\":1}]\nUSER>"),
            r#"[{"a":1}]"#
        );
        assert_eq!(extract_json_line("\n\n{\"found\":0}\n\n"), r#"{"found":0}"#);
        assert_eq!(extract_json_line(""), "");
        assert_eq!(extract_json_line("DOCKER_REQUIRED"), "");
    }

    /// The parse contract: upstream's repro plus the two shapes captured VERBATIM from
    /// live IRIS 2026.2. This is the test that would have caught #119 with no IRIS at all.
    #[test]
    fn the_old_shapes_do_not_parse_and_the_new_one_does() {
        // 1. mod.rs skill_list — raw pipe-delimited values concatenated into an array literal.
        let old_modrs = "[desc|body|0|2026-01-01T00:00:00Z,a|b|c|d|e]";
        assert!(serde_json::from_str::<serde_json::Value>(old_modrs).is_err());

        // 2. handle_skill "list" — $translate neutralises quote/CR/LF but NOT backslash.
        let old_translate = r#"[{"name":"a\qb","description":"desc\r","usage_count":0}]"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(old_translate).is_err(),
            "`\\q` is not a valid JSON escape — this is why a backslash in a name silently \
             produced an empty list"
        );

        // 3. What %ToJSON actually emitted for the same data on live IRIS.
        let fixed = r#"[{"name":"quo\"te","description":"desc with \" quote","usage_count":1,"created_at":"2026-01-01T00:00:00Z"},{"name":"a\\qb","description":"desc\\r","usage_count":0,"created_at":""}]"#;
        let v: Vec<serde_json::Value> = serde_json::from_str(fixed).expect("must parse");
        assert_eq!(v[0]["name"], r#"quo"te"#);
        assert_eq!(v[0]["usage_count"], 1);
        assert_eq!(v[1]["name"], r"a\qb");

        // 4. CRLF is escaped, so the payload is still ONE line — this is what keeps
        //    strip_iris_banner from re-joining a raw newline INSIDE a JSON string.
        let crlf =
            r#"[{"name":"nl","description":"desc\r\nline2","usage_count":0,"created_at":""}]"#;
        assert_eq!(crlf.lines().count(), 1);
        let v: Vec<serde_json::Value> = serde_json::from_str(crlf).expect("must parse");
        assert_eq!(v[0]["description"], "desc\r\nline2");

        // 5. Empty registry is `[]`, not `]`. (Regression guard shared with
        //    tests/unit/test_tools_fixes.rs::test_skill_list_empty_global_json.)
        assert!(serde_json::from_str::<serde_json::Value>("[]")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// A pipe in a description is truncated by $piece — that is the ^SKILLS storage
    /// format, NOT a JSON bug. Pin it so nobody "fixes" it inside the JSON builder.
    #[test]
    fn a_pipe_in_a_description_truncates_but_still_yields_valid_json() {
        // Live IRIS, value `a|b|c|d|e`:
        let observed = r#"[{"name":"pipe","description":"a","usage_count":0,"created_at":"d"}]"#;
        let v: Vec<serde_json::Value> = serde_json::from_str(observed).expect("valid JSON");
        assert_eq!(v[0]["description"], "a", "pipe is the storage delimiter");
    }

    /// #119 criterion 3: three states that used to collapse into one empty list.
    #[test]
    fn unreachable_and_unparseable_are_different_error_codes() {
        fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => &t.text,
                _ => panic!("expected text content"),
            };
            serde_json::from_str(text).unwrap()
        }

        let r = skills_read_fail(
            "skill_list",
            "USER",
            SkillsReadError::Unreachable("DOCKER_REQUIRED".into()),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = payload(&r);
        assert_eq!(v["error_code"], "DOCKER_REQUIRED");

        let r = skills_read_fail(
            "skill_list",
            "USER",
            SkillsReadError::Unparseable {
                raw: "<SYNTAX>zRun+3^Foo".into(),
                parse_error: "expected value at line 1 column 1".into(),
            },
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = payload(&r);
        assert_eq!(v["error_code"], "SKILLS_PARSE_FAILED");
        assert_eq!(v["raw_excerpt"], "<SYNTAX>zRun+3^Foo");
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("NOT an empty registry"),
            "the message must say IRIS WAS reached: {v}"
        );
    }
    /// #101: the third state. `DOCKER_REQUIRED` + "Set IRIS_CONTAINER=<container_name>" came
    /// back byte-for-byte identically with CORRECT and with WRONG credentials — verified live
    /// across skill_list / agent_stats / agent_info / skill(action=list). IRIS had answered
    /// 401; the container name is not what is wrong, and a caller following that hint edits
    /// the one variable that was already right.
    #[test]
    fn a_rejected_password_is_not_reported_as_a_missing_container() {
        fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => &t.text,
                _ => panic!("expected text content"),
            };
            serde_json::from_str(text).unwrap()
        }

        for (err, code) in [
            ("PUT doc failed: HTTP 401 Unauthorized", "IRIS_AUTH_FAILED"),
            ("PUT doc failed: HTTP 403 Forbidden", "IRIS_FORBIDDEN"),
        ] {
            let r = skills_read_fail(
                "skill_list",
                "APP",
                SkillsReadError::Unreachable(err.into()),
            )
            .unwrap();
            let v = payload(&r);
            assert_eq!(r.is_error, Some(true), "{v}");
            assert_eq!(v["error_code"], code, "{v}");
            assert!(
                !v.to_string().contains("IRIS_CONTAINER"),
                "the container name is not the knob to turn here: {v}"
            );
        }
        // The guard: a genuine "cannot reach IRIS at all" still says DOCKER_REQUIRED.
        let v = payload(
            &skills_read_fail(
                "skill_list",
                "APP",
                SkillsReadError::Unreachable("DOCKER_REQUIRED".into()),
            )
            .unwrap(),
        );
        assert_eq!(v["error_code"], "DOCKER_REQUIRED", "{v}");
    }

    /// #119 follow-up: an empty payload is an UNREACHABLE transport, not malformed JSON.
    /// `IrisConnection::execute` drops the exit status, so a failed `docker exec` arrives
    /// as `Ok("")`; classifying that as a parse failure made the tool state "IRIS WAS
    /// reachable" about a container that does not exist.
    #[test]
    fn an_empty_payload_is_unreachable_not_unparseable() {
        for raw in ["", "   ", "\n", "\r\n  \n"] {
            match classify_skills_payload(raw) {
                Err(SkillsReadError::Unreachable(msg)) => {
                    assert!(msg.contains("no output"), "{msg}")
                }
                other => panic!("empty payload must be Unreachable, got {other:?}"),
            }
        }
        // Output that is present but not JSON is still a genuine parse failure.
        match classify_skills_payload("<SYNTAX>zRun+3^Foo") {
            Err(SkillsReadError::Unparseable { .. }) => {}
            other => panic!("non-empty garbage must stay Unparseable, got {other:?}"),
        }
        // A reachable but empty registry writes `[]` — that is success, not an error.
        assert_eq!(
            classify_skills_payload("[]").unwrap(),
            serde_json::json!([])
        );
    }

    /// #89: a count is the LENGTH of the listing, never a fallback zero. The old stats arm
    /// ran its own `write count` and funnelled every failure through
    /// `.unwrap_or_default().trim().parse().unwrap_or(0)` — reproduced live saying
    /// `skill_count: 0` about a registry that held 2 skills, in the same session where
    /// `skill(action=list)` correctly answered DOCKER_REQUIRED.
    #[test]
    fn a_skill_count_is_the_array_length_never_a_fallback_zero() {
        assert_eq!(
            skills_count_from_payload(&serde_json::json!([
                {"name": "a", "description": "x"},
                {"name": "b", "description": "y"}
            ]))
            .unwrap(),
            2
        );
        // An empty registry IS legitimately zero — that is the one true zero.
        assert_eq!(
            skills_count_from_payload(&serde_json::json!([])).unwrap(),
            0
        );
        // Anything that is not an array is IRIS answering something we did not ask for.
        // Deliberately not `map_or(0, ..)`: that is the same silent zero, one refactor from
        // re-opening this issue.
        for not_a_list in [
            serde_json::json!({"found": 0}),
            serde_json::json!("[]"),
            serde_json::json!(0),
            serde_json::json!(null),
        ] {
            match skills_count_from_payload(&not_a_list) {
                Err(SkillsReadError::Unparseable { parse_error, .. }) => {
                    assert!(parse_error.contains("not a JSON array"), "{parse_error}")
                }
                other => panic!("{not_a_list} must not be a count, got {other:?}"),
            }
        }
    }

    /// #89: the three states the old arm collapsed into `{"skill_count":0,"success":true}`.
    /// No IRIS_CONTAINER and a failed `docker exec` (which reaches this crate as `Ok("")`)
    /// are DOCKER_REQUIRED; garbage on the wire is SKILLS_PARSE_FAILED. In every case the
    /// envelope must carry NO count at all — a caller that sees `skill_count` must be able
    /// to trust it.
    #[test]
    fn an_unreadable_registry_is_never_a_count_of_zero() {
        fn payload(r: &rmcp::model::CallToolResult) -> serde_json::Value {
            let text = match &r.content[0].raw {
                rmcp::model::RawContent::Text(t) => &t.text,
                _ => panic!("expected text content"),
            };
            serde_json::from_str(text).unwrap()
        }

        for (raw, expected_code) in [
            ("", "DOCKER_REQUIRED"),
            ("   ", "DOCKER_REQUIRED"),
            ("<SYNTAX>zRun+3^Foo", "SKILLS_PARSE_FAILED"),
        ] {
            let e = classify_skills_payload(raw)
                .and_then(|v| skills_count_from_payload(&v).map(|_| ()))
                .expect_err("none of these payloads is a readable registry");
            let r = skills_read_fail("agent_info(what=stats)", "APP", e).unwrap();
            assert_eq!(r.is_error, Some(true), "{raw:?}");
            let v = payload(&r);
            assert_eq!(v["error_code"], expected_code, "{v}");
            assert!(v.get("skill_count").is_none(), "no count may survive: {v}");
            let msg = v["error"].as_str().unwrap();
            assert!(msg.contains("agent_info(what=stats)"), "{v}");
            assert!(msg.contains("'APP'"), "#85: name the registry read: {v}");
            assert_eq!(v["namespace"], "APP", "{v}");
            assert_eq!(v["source"], "^SKILLS", "{v}");
        }

        // A payload that parses but is not a list is a parse failure, not a count: this is
        // the leg `skills_count_from_payload` adds on top of #119's classification.
        let e = classify_skills_payload("{\"found\":0}")
            .and_then(|v| skills_count_from_payload(&v).map(|_| ()))
            .expect_err("an object is not a listing");
        let v = payload(&skills_read_fail("agent_info(what=stats)", "APP", e).unwrap());
        assert_eq!(v["error_code"], "SKILLS_PARSE_FAILED", "{v}");
    }

    /// #89: stats must count what `skill(action=list)` lists — the SAME builder, so the two
    /// tools can no longer disagree about the registry. The old arm had its own hand-rolled
    /// `^SKILLS` reader, the last one in the crate to bypass `read_skills_json`.
    #[test]
    fn stats_and_skill_list_read_the_registry_the_same_way() {
        let code = skills_list_json_code(None, false);
        assert!(code.contains(SKILLS_GLOBAL), "{code}");
        assert!(
            !code.contains("write count"),
            "the private `write count` reader is gone for good: {code}"
        );
        // Bodies stay out of a count.
        assert!(!code.contains(r#""body":"#), "{code}");
    }

    /// #119 follow-up: the search haystack was `key_"|"_data`, and `data` is the
    /// pipe-delimited record — so every entry contained a `|` and a search for "|"
    /// returned the entire registry.
    #[test]
    fn the_search_haystack_is_the_fields_not_the_delimited_record() {
        let code = skills_list_json_code(Some("|"), false);
        assert!(
            !code.contains(r#"key_"|"_data"#),
            "the delimiter must not be spliced into the haystack: {code}"
        );
        assert!(
            code.contains(r#"$piece(data,"|",1)_" "_$piece(data,"|",2)"#),
            "name + description + body is the haystack: {code}"
        );
        // The needle itself still goes through os_str_expr (#67).
        assert!(code.contains(r#"$find($zconvert("#), "{code}");
        // No filter at all when no needle was given.
        assert!(!skills_list_json_code(None, false).contains("continue:"));
    }
}

#[cfg(test)]
mod skills_namespace_tests {
    use super::skills_namespace;
    use crate::iris::connection::{DiscoverySource, IrisConnection};

    /// `skills_namespace` reads the connection's NAMESPACE and nothing else — nothing here
    /// is ever dialled. The URL is an RFC 2606 `.invalid` host so it cannot be mistaken for
    /// a real instance: it read `http://localhost:43080`, which is a live dev IRIS on the
    /// maintainer's machine, and a test URL that resolves is one edit away from a test that
    /// talks to it.
    fn conn(ns: &str) -> IrisConnection {
        IrisConnection::new(
            "http://never-dialled.invalid",
            ns,
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        )
    }

    /// Issue #85 — the whole fallback chain, in ONE test function on purpose:
    /// `OBJECTSCRIPT_SKILLMCP_NAMESPACE` is process-global and cargo runs tests in
    /// parallel threads, so splitting these into four `#[test]`s would race.
    #[test]
    fn skills_namespace_fallback_chain() {
        std::env::remove_var("OBJECTSCRIPT_SKILLMCP_NAMESPACE");

        // 1. No override -> the CONNECTION's namespace. This is the bug: it used to
        //    answer "USER" for an operator running IRIS_NAMESPACE=APP, and every
        //    skill_* tool then reported a false empty registry with success:true.
        assert_eq!(skills_namespace(Some(&conn("APP"))), "APP");

        // 2. No override and no connection -> USER (unchanged last resort).
        assert_eq!(skills_namespace(None), "USER");

        // 3. The explicit override still wins over the connection — for anyone who
        //    deliberately centralises ^SKILLS in one namespace.
        std::env::set_var("OBJECTSCRIPT_SKILLMCP_NAMESPACE", "SKILLS");
        assert_eq!(skills_namespace(Some(&conn("APP"))), "SKILLS");
        assert_eq!(skills_namespace(None), "SKILLS");

        // 4. A blank override is not a namespace — it would reach
        //    `iris session IRIS -U ""` — so it falls through to the connection.
        std::env::set_var("OBJECTSCRIPT_SKILLMCP_NAMESPACE", "   ");
        assert_eq!(skills_namespace(Some(&conn("APP"))), "APP");
        assert_eq!(skills_namespace(None), "USER");

        std::env::remove_var("OBJECTSCRIPT_SKILLMCP_NAMESPACE");
    }
}

// ── Issue #99: the two false zeros in agent_stats, as pure functions ──────────
#[cfg(test)]
mod agent_stats_shape_tests {
    use super::*;

    /// #99, the SECOND false zero — and the one that had no coverage anywhere in the tree.
    /// `agent_stats` computed `session_calls` as `self.history.lock().map(|h| h.len())
    /// .unwrap_or(0)`, so a mutex poisoned by an unrelated panic reported "0 calls" about a
    /// deque that still holds every entry. `agent_history` got this fix in #89 and never got
    /// a test; this is that test, on the path #89 missed.
    #[test]
    fn a_poisoned_history_is_not_zero_session_calls() {
        let history = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
        for i in 0..3 {
            history.lock().unwrap().push_back(ToolCallEntry {
                tool: format!("tool{i}"),
                success: true,
                timestamp: std::time::Instant::now(),
            });
        }
        assert_eq!(session_call_count(&history), 3, "control: not yet poisoned");

        // Poison it exactly the way a panicking tool would: panic while holding the guard.
        let h = std::sync::Arc::clone(&history);
        let _ = std::thread::spawn(move || {
            let _guard = h.lock().unwrap();
            panic!("a tool panicked while holding the history");
        })
        .join();
        assert!(history.lock().is_err(), "the mutex must really be poisoned");

        assert_eq!(
            session_call_count(&history),
            3,
            "a poisoned mutex means a tool panicked, not that the session made no calls"
        );
    }

    /// #99: the registry population is REPORTED, not dropped — under a name that says what
    /// it is. `skill_count` is the ^SKILLS number; the `--subscribe` number lives in
    /// `subscribed_skill_count`, and the two are deliberately different here so a rename
    /// that swapped them could not pass.
    #[test]
    fn agent_stats_names_the_registry_population_separately() {
        let v = agent_stats_json(7, "APP", 4, true, Some((2, 9)));
        assert_eq!(
            v["skill_count"], 7,
            "^SKILLS is what skill_count MEANS: {v}"
        );
        assert_eq!(v["namespace"], "APP", "{v}");
        assert_eq!(v["source"], "^SKILLS", "{v}");
        assert_eq!(v["subscribed_skill_count"], 2, "{v}");
        assert_eq!(v["subscribed_kb_item_count"], 9, "{v}");
        assert!(
            v["subscribed_source"]
                .as_str()
                .unwrap()
                .contains("--subscribe"),
            "the second population must say where it came from: {v}"
        );
        assert!(
            v["subscribed_source"].as_str().unwrap().contains("startup"),
            "…and that it is frozen at startup, which is why it is not skill_count: {v}"
        );
    }

    /// #99: pin the field names, so a rename cannot silently put one population back under
    /// the other's name. `status` and the `subscribed_*` trio belong to `agent_stats` only —
    /// `agent_info(what=stats)` must keep emitting exactly what it emits today.
    #[test]
    fn agent_stats_success_payload_shape() {
        let stats = agent_stats_json(3, "APP", 1, true, Some((0, 0)));
        for k in [
            "success",
            "status",
            "skill_count",
            "namespace",
            "source",
            "subscribed_skill_count",
            "subscribed_kb_item_count",
            "subscribed_source",
            "session_calls",
            "learning_enabled",
        ] {
            assert!(stats.get(k).is_some(), "agent_stats must emit {k}: {stats}");
        }
        assert_eq!(stats["success"], true, "{stats}");
        assert_eq!(stats["status"], "ok", "{stats}");

        let info = agent_stats_json(3, "APP", 1, true, None);
        assert_eq!(
            info,
            serde_json::json!({
                "success": true,
                "skill_count": 3,
                "session_calls": 1,
                "learning_enabled": true,
                "namespace": "APP",
                "source": "^SKILLS",
            }),
            "agent_info(what=stats) must stay field-for-field what it emitted before #99"
        );
    }

    /// #99/#89/#119: there is no path through the success payload that can carry a count
    /// nobody measured, because the count is a `usize` parameter — a failed read returns
    /// `skills_read_fail`'s envelope and never reaches this function. Pinned so a future
    /// "partial success" refactor (`skill_count: null` + `skill_count_error`) has to argue
    /// with a test: JSON null coerces to 0 in JS and is `or 0`-ed in Python, i.e. the same
    /// false zero one hop downstream.
    #[test]
    fn a_failed_read_has_no_shape_here_at_all() {
        let fail = skills_read_fail(
            "agent_stats",
            "APP",
            SkillsReadError::Unreachable("no IRIS connection configured".into()),
        )
        .unwrap();
        let text = match &fail.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(fail.is_error, Some(true), "{v}");
        assert_eq!(v["error_code"], "DOCKER_REQUIRED", "{v}");
        assert!(v.get("skill_count").is_none(), "no count may survive: {v}");
        assert!(v.get("status").is_none(), "no \"ok\" on a failure: {v}");
        assert_eq!(v["namespace"], "APP", "{v}");
        assert_eq!(v["source"], "^SKILLS", "{v}");
    }
}
