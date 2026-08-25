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

pub fn skills_namespace() -> String {
    std::env::var("OBJECTSCRIPT_SKILLMCP_NAMESPACE").unwrap_or_else(|_| "USER".to_string())
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

    let ns = skills_namespace();

    match p.action.as_str() {
        "list" => {
            // Bug 9: use separator variable so empty global yields "[]" not "]".
            // #67: a JSON quote inside an ObjectScript literal is $CHAR(34), never \" —
            // backslash escapes nothing, so the old form was not valid ObjectScript and this
            // tool could only ever answer []. Stored values are stripped of quotes/newlines
            // so the assembled JSON stays parseable.
            let code = "set q=$CHAR(34) set key=\"\" set out=\"[\" set sep=\"\" for  { set key=$order(^SKILLS(key)) quit:key=\"\"  set data=$get(^SKILLS(key)) set out=out_sep_\"{\"_q_\"name\"_q_\":\"_q_$translate(key,q_$CHAR(13,10),\"   \")_q_\",\"_q_\"description\"_q_\":\"_q_$translate($piece(data,\"|\",1),q_$CHAR(13,10),\"   \")_q_\",\"_q_\"usage_count\"_q_\":\"_+$piece(data,\"|\",3)_\"}\" set sep=\",\" } set out=out_\"]\" write out";
            let raw = xecute(iris, client, code, &ns).await.unwrap_or_default();
            let skills: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!([]));
            ok_json(serde_json::json!({"success": true, "skills": skills}))
        }
        "describe" => {
            let name = p.name.as_deref().unwrap_or("");
            let code = format!("set data=$get(^SKILLS({})) write data", os_str_expr(name));
            let raw = xecute(iris, client, &code, &ns).await.unwrap_or_default();
            if raw.is_empty() {
                return err_json("NOT_FOUND", &format!("Skill '{}' not found", name));
            }
            let parts: Vec<&str> = raw.splitn(4, '|').collect();
            ok_json(serde_json::json!({
                "success": true,
                "name": name,
                "description": parts.first().unwrap_or(&""),
                "body": parts.get(1).unwrap_or(&""),
                "usage_count": parts.get(2).unwrap_or(&"0"),
                "created_at": parts.get(3).unwrap_or(&""),
            }))
        }
        "search" => {
            let query = p.query.as_deref().unwrap_or("").to_lowercase();
            // Bug 9: use separator variable so empty results yield "[]" not "]".
            let code = format!(
                "set q=$CHAR(34) set key=\"\" set out=\"[\" set sep=\"\" for {{ set key=$order(^SKILLS(key)) quit:key=\"\"  set data=$get(^SKILLS(key)) if $find($zconvert(key_data,\"L\"),{})>0 {{ set out=out_sep_\"{{\"_q_\"name\"_q_\":\"_q_$translate(key,q_$CHAR(13,10),\"   \")_q_\",\"_q_\"description\"_q_\":\"_q_$translate($piece(data,\"|\",1),q_$CHAR(13,10),\"   \")_q_\"}}\" set sep=\",\" }} }} set out=out_\"]\" write out",
                os_str_expr(&query)
            );
            let raw = xecute(iris, client, &code, &ns).await.unwrap_or_default();
            let results: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!([]));
            ok_json(serde_json::json!({"success": true, "query": query, "results": results}))
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
                    let ns = skills_namespace();
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

    let ns = skills_namespace();

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

pub async fn handle_agent_info(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: AgentInfoParams,
    history: &std::sync::Mutex<VecDeque<ToolCallEntry>>,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    match p.what.as_str() {
        "stats" => {
            let ns = skills_namespace();
            let code = "set count=0 set key=\"\" for { set key=$order(^SKILLS(key)) quit:key=\"\"  set count=count+1 } write count";
            let skill_count: usize = xecute(iris, client, code, &ns)
                .await
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(0);
            let session_calls = history.lock().map(|h| h.len()).unwrap_or(0);
            ok_json(serde_json::json!({
                "success": true,
                "skill_count": skill_count,
                "session_calls": session_calls,
                "learning_enabled": learning_enabled(),
            }))
        }
        "history" => {
            let limit = p.limit;
            let calls: Vec<serde_json::Value> = history
                .lock()
                .map(|h| {
                    h.iter()
                        .rev()
                        .take(limit)
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
}
