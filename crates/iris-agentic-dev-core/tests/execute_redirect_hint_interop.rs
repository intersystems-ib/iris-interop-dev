//! #330 — `execute_redirect_hint` must name the typed tool when a caller hand-rolls
//! interoperability work through `Ens.Director` / `Ens.Config.*` inside `iris_execute`, and it
//! must keep every redirect it already had.
//!
//! The arms are substring matches on the uppercased code, so one also fires on a comment or a
//! string literal that merely mentions the class. That is accepted for a NON-BLOCKING hint —
//! what is guarded here instead is the mitigation: the text is worded as advice ("If you are
//! …"), so a false positive reads as a suggestion and never as a claim about what the caller
//! did.
//!
//! WHAT THIS FILE DOES NOT COVER, so a clean run is not read as more than it is:
//! * it exercises the pure function only. That the handler ATTACHES the returned hint, and in
//!   which order relative to an abort's own explanation, is guarded in `interop_unit_tests.rs`
//!   (#185) — nothing here would notice the call site being deleted.
//! * the name guards resolve only the tools listed in `PARAM_SOURCES` / `ACTION_SOURCES`. A
//!   parameter named for some other tool (`iris_doc(compile=…)`, `docs_introspect(class_name=…)`)
//!   is not checked against anything, and a wrong one there would pass.
//! * nothing here says a hint is GOOD advice. It says the tool, action and parameter names it
//!   prints are ones this server actually declares.

use iris_agentic_dev_core::tools::sql_lint::execute_redirect_hint;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Every interop arm added by #330 opens with this. It is the advice framing the design
/// constraint asks for, and it is also how a pre-#330 redirect is told apart from a new one.
const INTEROP_ADVICE_PREFIX: &str = "If you are ";

// ── the behaviour table ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Expect {
    /// The redirect fires and its text contains every fragment.
    Names(&'static [&'static str]),
    /// No redirect at all.
    Silent,
}

struct Case {
    /// What this row is here to pin down.
    what: &'static str,
    /// The ObjectScript (or SQL) a caller sends to `iris_execute`.
    code: &'static str,
    expect: Expect,
    /// True for behaviour that existed BEFORE #330. Those rows must keep the hint they had:
    /// the risk a new arm introduces is stealing a redirect, not losing one outright.
    pre330: bool,
}

const LOAD_FROM_FILE: &[&str] = &[
    "Atelier",
    "iris_doc(action=put, compile=true)",
    "iris_compile",
];

const CASES: &[Case] = &[
    // ── pre-#330: load/compile from a filesystem path ────────────────────────────────────
    Case {
        what: "$SYSTEM.OBJ.Load",
        code: r#"set sc = $System.OBJ.Load("/tmp/Cocina/Production.cls","cuk")"#,
        expect: Expect::Names(LOAD_FROM_FILE),
        pre330: true,
    },
    Case {
        what: "$SYSTEM.OBJ.Import",
        code: r#"do $SYSTEM.OBJ.Import("/tmp/Cocina.xml",,.err)"#,
        expect: Expect::Names(LOAD_FROM_FILE),
        pre330: true,
    },
    Case {
        what: "StudioOpenDocument",
        code: "Do ##class(%Studio.Project).StudioOpenDocument(f)",
        expect: Expect::Names(LOAD_FROM_FILE),
        pre330: true,
    },
    Case {
        what: "OBJ.LoadDir",
        code: r#"set sc=$system.OBJ.LoadDir("/tmp/src","ck",.err,1)"#,
        expect: Expect::Names(LOAD_FROM_FILE),
        pre330: true,
    },
    Case {
        what: "OBJ.LoadStream",
        code: r#"set sc=$system.OBJ.LoadStream(stream,"ck")"#,
        expect: Expect::Names(LOAD_FROM_FILE),
        pre330: true,
    },
    // ── pre-#330: catalog / dictionary / ad-hoc SQL ──────────────────────────────────────
    Case {
        what: "%Dictionary introspection",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"SELECT Name FROM %Dictionary.ClassDefinition WHERE Name LIKE 'Cocina.%'")"#,
        expect: Expect::Names(&[
            "docs_introspect(class_name=...)",
            "iris_symbols(pattern=...)",
            "iris_table_info(table=...)",
        ]),
        pre330: true,
    },
    Case {
        what: "Ens_Config.Item read as SQL (the TABLE family, not the CLASS family)",
        code: "SELECT Name, Enabled FROM Ens_Config.Item",
        expect: Expect::Names(&[
            "iris_production(action=status)",
            "iris_production_item(action=get_settings)",
        ]),
        pre330: true,
    },
    Case {
        what: "bare SELECT",
        code: "SELECT Name FROM Hospital.Patient",
        expect: Expect::Names(&["iris_query(query=...)"]),
        pre330: true,
    },
    Case {
        what: "%SQL.Statement used to read",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"SELECT Nsp FROM Sample.Person")"#,
        expect: Expect::Names(&["iris_query(query=...)"]),
        pre330: true,
    },
    // The write patterns are EXCLUSIONS: each of these reads rows AND writes, so the
    // iris_query redirect must stay off (iris_query blocks writes by design).
    Case {
        what: "INSERT ... SELECT is a write, not an iris_query call",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"INSERT INTO Cocina.Menus (n) SELECT n FROM Cocina.Staging")"#,
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "UPDATE with a subselect is a write",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"UPDATE Cocina.Menus SET n=(SELECT MAX(n) FROM Cocina.Staging)")"#,
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "DELETE with a subselect is a write",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"DELETE FROM Cocina.Menus WHERE id IN (SELECT id FROM Cocina.Staging)")"#,
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "MERGE with a subselect is a write",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"MERGE INTO Cocina.Menus t USING (SELECT 1 id) s ON t.id=s.id")"#,
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "a CALL alongside a read keeps the redirect off",
        code: r#"set rs=##class(%SQL.Statement).%ExecDirect(,"SELECT id FROM Cocina.Menus") do rs.%Display() ; then CALL Cocina.Purge()"#,
        expect: Expect::Silent,
        pre330: true,
    },
    // ── #330: Ens.Director → iris_production / iris_production_item ──────────────────────
    // These six share ONE hint by design; each row asserts that the hint names the action
    // for the Ens.Director entry point the snippet actually called.
    Case {
        what: "Ens.Director.StartProduction",
        code: r#"set sc=##class(Ens.Director).StartProduction("Cocina.Production")"#,
        expect: Expect::Names(&["iris_production", "action=start for StartProduction"]),
        pre330: false,
    },
    Case {
        what: "Ens.Director.StopProduction",
        code: "set tSC=##class(Ens.Director).StopProduction(10,1) write $System.Status.GetErrorText(tSC)",
        expect: Expect::Names(&["iris_production", "action=stop for StopProduction"]),
        pre330: false,
    },
    Case {
        what: "Ens.Director.GetProductionStatus",
        code: "set st=##class(Ens.Director).GetProductionStatus(.prod,.state)",
        expect: Expect::Names(&["iris_production", "action=status for GetProductionStatus"]),
        pre330: false,
    },
    Case {
        what: "Ens.Director.UpdateProduction",
        code: "set sc=##class(Ens.Director).UpdateProduction(10,0)",
        expect: Expect::Names(&["iris_production", "action=update for UpdateProduction"]),
        pre330: false,
    },
    Case {
        what: "Ens.Director.RecoverProduction",
        code: "do ##class(Ens.Director).RecoverProduction()",
        expect: Expect::Names(&["iris_production", "action=recover for RecoverProduction"]),
        pre330: false,
    },
    Case {
        what: "Ens.Director.EnableConfigItem is the ITEM tool, not the production tool",
        code: r#"set sc=##class(Ens.Director).EnableConfigItem("HL7 In",0,1)"#,
        expect: Expect::Names(&["iris_production_item(action=enable or action=disable"]),
        pre330: false,
    },
    // ── #330: Ens.Config.Item / Ens.Config.Production → iris_production_item ─────────────
    Case {
        what: "Ens.Config.Item built by hand",
        code: r#"set item=##class(Ens.Config.Item).%New() set item.Name="HL7 In" set item.ClassName="EnsLib.HL7.Service.TCPService" do item.%Save()"#,
        expect: Expect::Names(&[
            "iris_production_item",
            "action=add",
            "class_name=",
            "'Adapter.'",
        ]),
        pre330: false,
    },
    Case {
        what: "Ens.Config.Production opened to disable an item",
        code: r#"set prod=##class(Ens.Config.Production).%OpenId("Cocina.Production") set it=prod.Items.GetAt(1) set it.Enabled=0 do prod.%Save()"#,
        expect: Expect::Names(&["iris_production_item", "action=disable"]),
        pre330: false,
    },
    // ── #330: Ens.Config.Credentials → iris_credential_manage ───────────────────────────
    Case {
        what: "Ens.Config.Credentials created by hand",
        code: r#"set cr=##class(Ens.Config.Credentials).%New() set cr.SystemName="PG" set cr.Username="app" set cr.Password="s3cret" do cr.%Save()"#,
        expect: Expect::Names(&[
            "iris_credential_manage(action=create|update|delete",
            "iris_credential_list",
        ]),
        pre330: false,
    },
    // ── controls: a hint for everything is not a hint ───────────────────────────────────
    Case {
        what: "a message object is not interop CONFIG",
        code: r#"set msg=##class(Ens.StringRequest).%New() set msg.StringValue="x" do msg.%Save()"#,
        expect: Expect::Silent,
        pre330: false,
    },
    Case {
        what: "an Ens utility call the typed tools do not replace",
        code: "do ##class(Ens.Util.Log).Purge(.deleted,30)",
        expect: Expect::Silent,
        pre330: false,
    },
    Case {
        what: "a domain object save",
        code: "set o=##class(Cocina.MSG.MenuRequest).%New() do o.%Save()",
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "a global write",
        code: r#"set ^Cocina.Menu(1)="soup""#,
        expect: Expect::Silent,
        pre330: true,
    },
    Case {
        what: "a version write",
        code: "write $ZVERSION,!",
        expect: Expect::Silent,
        pre330: true,
    },
];

#[test]
fn every_case_behaves_as_the_table_says() {
    for c in CASES {
        match c.expect {
            Expect::Silent => assert_eq!(
                execute_redirect_hint(c.code),
                None,
                "{} must produce no redirect at all — a hint for everything is not a hint: {}",
                c.what,
                c.code
            ),
            Expect::Names(fragments) => {
                let h = execute_redirect_hint(c.code)
                    .unwrap_or_else(|| panic!("{}: no redirect for {}", c.what, c.code));
                for f in fragments {
                    assert!(
                        h.contains(f),
                        "{}: the hint does not name {f:?}: {h}",
                        c.what
                    );
                }
            }
        }
    }
}

/// The failure mode a new arm introduces is not a missing hint — it is a STOLEN one. Every
/// pre-#330 row must still be answered by the arm that answered it before, and the interop
/// arms are the ones that open with the advice prefix.
#[test]
fn the_new_arms_did_not_take_a_redirect_that_already_existed() {
    let mut checked = 0;
    for c in CASES.iter().filter(|c| c.pre330) {
        if let Expect::Names(_) = c.expect {
            let h = execute_redirect_hint(c.code)
                .unwrap_or_else(|| panic!("{}: pre-#330 redirect disappeared", c.what));
            assert!(
                !h.starts_with(INTEROP_ADVICE_PREFIX),
                "a #330 interop arm stole the redirect for {}: {h}",
                c.what
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "this guard checked nothing — the pre330 rows lost their Names expectations"
    );
}

/// The accepted false positive, and its mitigation. A mention inside a comment fires the arm
/// (the match is a substring; parsing ObjectScript is explicitly out of scope), so the wording
/// has to be advice a caller can ignore rather than an assertion about what they did.
#[test]
fn a_mention_in_a_comment_still_fires_and_still_reads_as_advice() {
    let commented =
        "; TODO: the ##class(Ens.Director).StartProduction call moved to the setup\n set x=1";
    let h = execute_redirect_hint(commented).expect("substring match: the arm fires here");
    assert!(
        h.starts_with(INTEROP_ADVICE_PREFIX),
        "a false positive must read as a suggestion, not an accusation: {h}"
    );
    for c in CASES.iter().filter(|c| !c.pre330) {
        if let Expect::Names(_) = c.expect {
            let h = execute_redirect_hint(c.code).expect("a #330 row must produce a hint");
            assert!(
                h.starts_with(INTEROP_ADVICE_PREFIX),
                "{} is not worded as advice: {h}",
                c.what
            );
        }
    }
}

// ── source-derived guards ─────────────────────────────────────────────────────────────────

fn read_src(rel: &str) -> String {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "this guard cannot read {} ({e}) — failing rather than skipping",
            p.display()
        )
    })
}

/// The text of one item, from an anchor to the first closing brace in column 0.
fn block_after(text: &str, anchor: &str, what: &str) -> String {
    let start = text
        .find(anchor)
        .unwrap_or_else(|| panic!("{what}: anchor {anchor:?} is gone — re-point this guard"));
    let rest = &text[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{what}: no closing brace after {anchor:?}"));
    rest[..end].to_string()
}

/// Every `contains("…")` pattern inside `execute_redirect_hint`, in source order.
fn redirect_patterns() -> Vec<String> {
    let body = block_after(
        &read_src("tools/sql_lint.rs"),
        "pub fn execute_redirect_hint(",
        "execute_redirect_hint",
    );
    // Positive control: an empty or short parse must not read as "nothing left to check".
    // This literal is in the function's first arm, so its absence means the SLICE is wrong.
    assert!(
        body.contains("\"$SYSTEM.OBJ.LOAD\""),
        "the sliced body is missing the first arm's pattern — the slice is wrong, not the code"
    );

    // TWO call shapes, and the guard must know both:
    //   u.contains("LITERAL")            — substring match, used by the pre-#330 arms
    //   contains_word(&u, "LITERAL")     — identifier-boundary match, used by the Ens.* arms
    //
    // The second shape exists because bare `contains` fired the Director arm on
    // `Cocina.Ens.DirectoryWatcher`. When that fix landed, this parse — which knew only the first
    // shape — went from 14 patterns to 13 and the assertion below caught it. That is the guard
    // working: a parse that silently finds FEWER patterns than the function has calls would shrink
    // the set this file claims to cover, which is the failure mode it exists to prevent.
    let mut out = Vec::new();
    for needle in ["contains(\"", "contains_word(&u, \""] {
        let mut i = 0;
        while let Some(p) = body[i..].find(needle) {
            let s = i + p + needle.len();
            let e = body[s..]
                .find('"')
                .expect("unterminated pattern literal in execute_redirect_hint");
            out.push(body[s..s + e].to_string());
            i = s + e;
        }
    }

    // The guard must refuse to pass when its own parse found less than the function holds:
    // a `contains(SOME_CONST)` is invisible to the scan above and would silently shrink the
    // set this file claims to cover. Both sides are derived; neither is written down.
    //
    // `.contains(` also matches the `_word` shape's inner call, so count both call forms the same
    // way the scan reads them.
    let calls = body.matches(".contains(").count() + body.matches("contains_word(&u, ").count();
    assert_eq!(
        out.len(),
        calls,
        "parsed {} literal patterns but the function makes {calls} matching calls — \
a non-literal argument is not covered by this guard",
        out.len()
    );
    assert!(!out.is_empty(), "no patterns parsed at all");
    out
}

/// A new arm with no test looks exactly like a working one. Every pattern the function
/// branches on must be exercised by a row in `CASES`.
#[test]
fn every_pattern_the_function_matches_on_is_exercised_by_a_case() {
    for p in redirect_patterns() {
        assert!(
            CASES
                .iter()
                .any(|c| c.code.to_ascii_uppercase().contains(p.as_str())),
            "no row in CASES sends code containing {p:?} — an arm of execute_redirect_hint \
is untested"
        );
    }
}

/// Every tool this server declares, derived from the `#[tool(...)]` handlers.
fn declared_tools() -> BTreeSet<String> {
    let text = read_src("tools/mod.rs");
    let mut out = BTreeSet::new();
    let attr = "#[tool(";
    let f_kw = "async fn ";
    let mut i = 0;
    while let Some(p) = text[i..].find(attr) {
        let at = i + p;
        if let Some(f) = text[at..].find(f_kw) {
            let s = at + f + f_kw.len();
            if let Some(e) = text[s..].find('(') {
                out.insert(text[s..s + e].trim().to_string());
            }
        }
        i = at + attr.len();
    }
    // Positive control: a broken scan returns an empty set, and an empty set would let every
    // name below pass unchecked.
    for control in ["iris_execute", "iris_production", "iris_production_item"] {
        assert!(
            out.contains(control),
            "tool scan did not find {control} — the derivation is broken, not the hints"
        );
    }
    out
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whole-word occurrences of `word` in `text`, as byte offsets.
fn word_positions(text: &str, word: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = text[i..].find(word) {
        let s = i + p;
        let e = s + word.len();
        let before = s == 0 || !is_name_byte(bytes[s - 1]);
        let after = e >= bytes.len() || !is_name_byte(bytes[e]);
        if before && after {
            out.push(s);
        }
        i = s + 1;
    }
    out
}

/// Every `key=value` a hint prints, attributed to the nearest tool name before it.
fn keyed_values(hint: &str, tools: &BTreeSet<String>) -> Vec<(String, String, String)> {
    let mut anchors: Vec<(usize, &str)> = Vec::new();
    for t in tools {
        for pos in word_positions(hint, t) {
            anchors.push((pos, t.as_str()));
        }
    }
    anchors.sort_unstable();

    let bytes = hint.as_bytes();
    let mut out = Vec::new();
    for (eq, _) in hint.char_indices().filter(|(_, c)| *c == '=') {
        let mut s = eq;
        while s > 0 && is_name_byte(bytes[s - 1]) {
            s -= 1;
        }
        if s == eq {
            continue; // not `identifier=`
        }
        let key = &hint[s..eq];
        let mut v = eq + 1;
        while v < bytes.len() && (is_name_byte(bytes[v]) || bytes[v] == b'|') {
            v += 1;
        }
        let value = &hint[eq + 1..v];
        let tool = anchors
            .iter()
            .rev()
            .find(|(pos, _)| *pos < s)
            .map(|(_, t)| *t);
        if let Some(tool) = tool {
            out.push((tool.to_string(), key.to_string(), value.to_string()));
        }
    }
    out
}

/// Quoted string literals between the first `[` after `"enum"` and its matching `]`.
fn enum_members(block: &str, what: &str) -> BTreeSet<String> {
    let at = block
        .find("\"enum\"")
        .unwrap_or_else(|| panic!("{what}: no \"enum\" in the schema block — re-point this guard"));
    let open = block[at..]
        .find('[')
        .unwrap_or_else(|| panic!("{what}: no '[' after \"enum\""))
        + at;
    let close = block[open..]
        .find(']')
        .unwrap_or_else(|| panic!("{what}: no ']' closing the enum"))
        + open;
    let list = &block[open..close];
    let mut out = BTreeSet::new();
    let mut i = 0;
    while let Some(p) = list[i..].find('"') {
        let s = i + p + 1;
        let e = match list[s..].find('"') {
            Some(e) => s + e,
            None => break,
        };
        out.insert(list[s..e].to_string());
        i = e + 1;
    }
    assert!(!out.is_empty(), "{what}: parsed an empty action enum");
    assert!(
        !out.contains("frobnicate"),
        "{what}: the parse is returning everything, not the enum"
    );
    out
}

/// `pub` field names of a params struct.
fn struct_fields(block: &str, what: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in block.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("pub ") {
            if let Some((name, _)) = rest.split_once(':') {
                if !name.is_empty() && name.bytes().all(is_name_byte) {
                    out.insert(name.to_string());
                }
            }
        }
    }
    assert!(!out.is_empty(), "{what}: parsed no fields");
    out
}

/// Where each tool's parameter names are declared. Tools absent from this map are not checked.
const PARAM_SOURCES: &[(&str, &str, &str)] = &[
    (
        "iris_production",
        "tools/mod.rs",
        "pub struct ProductionDispatchSchema {",
    ),
    (
        "iris_production_item",
        "tools/interop.rs",
        "pub struct ProductionItemParams {",
    ),
    (
        "iris_credential_manage",
        "tools/interop.rs",
        "pub struct CredentialManageParams {",
    ),
    (
        "iris_table_info",
        "tools/info.rs",
        "pub struct TableInfoParams {",
    ),
];

/// Where each tool's `action` enum is declared.
const ACTION_SOURCES: &[(&str, &str, &str)] = &[
    (
        "iris_production",
        "tools/mod.rs",
        "impl JsonSchema for ProductionAction {",
    ),
    (
        "iris_production_item",
        "tools/interop.rs",
        "pub struct ProductionItemParams {",
    ),
    (
        "iris_credential_manage",
        "tools/interop.rs",
        "pub struct CredentialManageParams {",
    ),
];

fn lookup(
    map: &[(&'static str, &'static str, &'static str)],
    tool: &str,
) -> Option<(&'static str, &'static str)> {
    map.iter()
        .find(|(t, _, _)| *t == tool)
        .map(|(_, f, a)| (*f, *a))
}

/// A hint is read as a contract. #208 shipped one naming `iris_table_info(schema=…)`, a
/// parameter that tool has never had, and the test written then covered a single hint string
/// while the sibling in `execute_redirect_hint` kept the wrong name. This checks every hint
/// the table produces, against the parameter and action names parsed out of the schemas.
#[test]
fn every_tool_action_and_parameter_a_hint_prints_is_one_the_server_declares() {
    let tools = declared_tools();
    let mut checked_actions: BTreeMap<String, usize> = BTreeMap::new();
    let mut checked_params: BTreeMap<String, usize> = BTreeMap::new();
    let mut tool_mentions = 0;

    for c in CASES {
        let Expect::Names(_) = c.expect else { continue };
        let h = execute_redirect_hint(c.code).expect("a Names row produces a hint");

        // Every iris_* token a hint prints must be a tool that exists. A hint naming a tool
        // this server does not register is indistinguishable, to the reader, from one that does.
        for word in h.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')) {
            if word.starts_with("iris_") {
                assert!(
                    tools.contains(word),
                    "{}: the hint names {word}, which this server does not declare as a tool: {h}",
                    c.what
                );
                tool_mentions += 1;
            }
        }

        for (tool, key, value) in keyed_values(h, &tools) {
            if let Some((file, anchor)) = lookup(PARAM_SOURCES, &tool) {
                let block = block_after(&read_src(file), anchor, anchor);
                let fields = struct_fields(&block, anchor);
                assert!(
                    fields.contains(&key),
                    "{}: the hint writes {tool}({key}=…) but {tool} declares no {key} \
parameter (it has {fields:?}): {h}",
                    c.what
                );
                *checked_params.entry(tool.clone()).or_default() += 1;
            }
            if key != "action" {
                continue;
            }
            if let Some((file, anchor)) = lookup(ACTION_SOURCES, &tool) {
                let block = block_after(&read_src(file), anchor, anchor);
                let actions = enum_members(&block, anchor);
                for one in value.split('|').filter(|v| !v.is_empty()) {
                    assert!(
                        actions.contains(one),
                        "{}: the hint offers {tool}(action={one}) but the schema declares \
{actions:?}: {h}",
                        c.what
                    );
                    *checked_actions.entry(tool.clone()).or_default() += 1;
                }
            }
        }
    }

    // A guard that checked nothing passes. These are the tools #330 exists to name, so if
    // an arm stops naming one with an action, this fails rather than quietly covering less.
    for tool in [
        "iris_production",
        "iris_production_item",
        "iris_credential_manage",
    ] {
        assert!(
            checked_actions.get(tool).copied().unwrap_or(0) > 0,
            "no hint named an action of {tool} — checked {checked_actions:?}"
        );
    }
    assert!(
        checked_params.contains_key("iris_table_info"),
        "no hint named an iris_table_info parameter — the #208 sibling is no longer covered"
    );
    assert!(tool_mentions > 0, "no tool names were checked at all");
}

// ── #330 follow-up: the boundary cases a bare `contains` let through ─────────
//
// The first version of these arms used `u.contains("ENS.DIRECTOR")`. An adversarial pass ran the
// function and found both of the cases below firing. They are the tests that were missing, not a
// hypothetical: a class whose name merely BEGINS with a pattern is a different class.
//
// The in-file control that was supposed to cover this was `Ens.StringRequest` — a different
// package entirely, which rules out nothing about prefixes. A control that cannot fail for the
// reason you care about is not a control.

/// A class in someone else's package whose name starts with the pattern. `Cocina.Ens.Director` +
/// `yWatcher`: the arm must not fire, because this is not `Ens.Director`.
#[test]
fn a_class_whose_name_merely_starts_with_ens_director_gets_no_hint() {
    for code in [
        "set w=##class(Cocina.Ens.DirectoryWatcher).%New() do w.Start()",
        "do ##class(Ens.DirectorySync).Run()",
        "set x=##class(Ens.DirectorHelper).Probe()",
    ] {
        assert_eq!(
            execute_redirect_hint(code),
            None,
            "{code:?} names a DIFFERENT class; firing the Ens.Director redirect on it tells the \
             caller to replace a call that iris_production cannot make"
        );
    }
}

/// Same shape on the config family. `Ens.Config.ItemSettings` is not `Ens.Config.Item`.
#[test]
fn a_class_whose_name_merely_starts_with_ens_config_item_gets_no_hint() {
    for code in [
        "set s=##class(Ens.Config.ItemSettings).%OpenId(1)",
        "do ##class(Ens.Config.ProductionSettings).Apply()",
        "set c=##class(Ens.Config.CredentialsCache).%New()",
    ] {
        assert_eq!(
            execute_redirect_hint(code),
            None,
            "{code:?} names a DIFFERENT class from the one the arm redirects"
        );
    }
}

/// THE CONTROL for the two tests above. Without it, matching nothing at all would pass them — and
/// the reviewer found exactly that failure mode in the arm's original control (`Ens.Util.Log.Purge`
/// matched no arm in the function, so it passed whatever the interop arms did).
#[test]
fn the_exact_class_names_still_fire_so_the_boundary_tests_are_not_vacuous() {
    let cases = [
        (
            "do ##class(Ens.Director).StartProduction(\"App.Prod\")",
            "iris_production",
        ),
        (
            "set sc=##class(Ens.Config.Item).%OpenId(id)",
            "iris_production_item",
        ),
        (
            "set c=##class(Ens.Config.Credentials).%New()",
            "iris_credential_manage",
        ),
    ];
    for (code, tool) in cases {
        let hint = execute_redirect_hint(code)
            .unwrap_or_else(|| panic!("{code:?} must still produce a redirect"));
        assert!(
            hint.contains(tool),
            "{code:?} must name {tool}; got: {hint}"
        );
    }
}

/// The Director arm used to claim iris_production "does the same job typed" without qualification.
/// It maps six actions; `Ens.Director` has more methods than that, and telling a caller to replace
/// `CreateBusinessService` with a tool that cannot do it is worse than silence.
#[test]
fn the_director_hint_admits_what_it_does_not_cover() {
    let hint = execute_redirect_hint("do ##class(Ens.Director).CreateBusinessService(\"X\",.svc)")
        .expect("the arm fires — the class IS Ens.Director");
    assert!(
        hint.contains("CreateBusinessService"),
        "the hint must name at least one uncovered method rather than implying full coverage: {hint}"
    );
    assert!(
        hint.contains("no typed equivalent"),
        "the hint must say plainly that those methods are not covered: {hint}"
    );
}
