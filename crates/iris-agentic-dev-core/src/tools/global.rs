//! #532: addressing a global node, and reporting what is actually there.
//!
//! Ported from upstream `v1.6.2` `tools/global.rs` rather than copied. Two things changed, and
//! they are the reason this is a port and not a vendor:
//!
//! 1. **Subscripts go through [`crate::objectscript::os_str_expr`].** Upstream restricted every
//!    subscript to `^[a-zA-Z0-9 _.:\-]+$` and then built the reference with
//!    `format!("\"{}\"", s)` — no escaping at all. The allowlist was the only thing making that
//!    safe, so widening it (which legacy data requires: `^CUST("X","ACME, INC.",1)`) and changing
//!    the builder are a SINGLE change. Doing the first alone is ObjectScript injection; doing the
//!    second alone fixes nothing a caller can reach.
//!
//! 2. **`$DATA` is reported as itself.** Upstream emitted `$Data(ref) > 0`, so a node holding the
//!    empty string and a node holding only children were both `defined: true, value: ""`. "The
//!    value is empty" and "there is no value here" are different facts about legacy data, and
//!    routine code branches on exactly that difference.
//!
//! On the canonic-number rule, which the issue originally got backwards: `^X(1)` and `^X("1")` are
//! the SAME node, because IRIS canonicalises a canonic numeric string to a number. So quoting a
//! numeric subscript is harmless. What is not harmless is *unquoting* a non-canonic one — `"01"`,
//! `"1.0"`, `"-0"` are distinct string subscripts that collate after the numbers. Hence the rule
//! below: text is ALWAYS quoted, and only a JSON number is emitted bare, where IRIS does the
//! canonicalisation itself and this code never has to implement it.

use regex::Regex;
use std::sync::OnceLock;

/// One subscript, as the caller expressed it.
///
/// The distinction is about the CALLER'S INTENT, not about addressing: a JSON number means "the
/// numeric subscript", a JSON string means "this exact string". IRIS then collapses the two where
/// it considers them the same node, which is what makes this safe without a canonicaliser here.
#[derive(Debug, Clone, PartialEq)]
pub enum Subscript {
    /// Emitted as a bare numeric literal for IRIS to canonicalise.
    Number(serde_json::Number),
    /// Emitted as a quoted ObjectScript string expression, always.
    Text(String),
}

impl Subscript {
    pub fn from_i64(n: i64) -> Self {
        Subscript::Number(serde_json::Number::from(n))
    }

    /// `None` for a non-finite float, which has no numeric literal and is not valid JSON either.
    pub fn from_f64(n: f64) -> Option<Self> {
        serde_json::Number::from_f64(n).map(Subscript::Number)
    }

    /// Read a subscript straight out of a JSON argument array. Anything that is not a number is
    /// taken as text, including `true` and `null`, because a subscript is a string to IRIS and
    /// refusing here would only move the error further from the caller.
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Number(n) => Subscript::Number(n.clone()),
            serde_json::Value::String(s) => Subscript::Text(s.clone()),
            other => Subscript::Text(other.to_string()),
        }
    }
}

/// Render one subscript as an ObjectScript expression.
///
/// Text is quoted via `os_str_expr`, so an embedded quote is doubled, a control character or any
/// non-ASCII character becomes a `$CHAR(...)` splice, and the result is pure ASCII on one line.
/// That last property is load-bearing: the generated code is assembled line by line, so a raw
/// newline in a subscript would otherwise split a statement.
pub fn subscript_expr(s: &Subscript) -> String {
    match s {
        Subscript::Number(n) => n.to_string(),
        Subscript::Text(t) => crate::objectscript::os_str_expr(t),
    }
}

/// Build `^Name(sub,sub,...)`, or `^Name` when there are no subscripts.
///
/// `name` must already have been checked with [`global_name_error`]; this function splices it.
pub fn build_global_ref(name: &str, subscripts: &[Subscript]) -> String {
    if subscripts.is_empty() {
        return format!("^{name}");
    }
    let parts: Vec<String> = subscripts.iter().map(subscript_expr).collect();
    format!("^{}({})", name, parts.join(","))
}

/// Strip a leading `^`. `^MyApp` → `MyApp`.
pub fn normalize_global_name(name: &str) -> String {
    name.strip_prefix('^').unwrap_or(name).to_string()
}

static GLOBAL_RE: OnceLock<Regex> = OnceLock::new();

fn global_re() -> &'static Regex {
    GLOBAL_RE.get_or_init(|| {
        Regex::new(r"^%?[A-Za-z][A-Za-z0-9]*(\.[A-Za-z0-9]+)*$").expect("valid regex")
    })
}

/// `None` when `name` is a global name this code may splice, otherwise the refusal to report.
///
/// An extended reference is refused in its own words rather than falling through to the generic
/// message. `^|"OTHER"|G` names a different NAMESPACE, so accepting it would route a read past the
/// `namespace` parameter the caller set — a quiet redirection, which is worse than a refusal.
pub fn global_name_error(name: &str) -> Option<String> {
    if name.starts_with('|') || name.starts_with('[') {
        return Some(format!(
            "'{name}' is an extended reference, which names a different namespace than this call \
             asked for. Pass the target namespace in the 'namespace' parameter and the bare global \
             name here."
        ));
    }
    if global_re().is_match(name) {
        return None;
    }
    Some(format!(
        "'{name}' is not a global name. A global name is an optional %, then a letter, then \
         letters and digits, with single periods between segments (IIDTest, MyApp.Data, %SYS). \
         Subscripts go in 'subscripts' and the namespace in 'namespace', not in the name."
    ))
}

/// What `$DATA` said about one node — all four values IRIS defines.
///
/// A boolean here is the defect being fixed: `$Data > 0` is true for 1, 10 and 11 alike, which
/// made a node holding `""` and a node holding only children the same answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeData {
    /// `$DATA = 0` — nothing here, and nothing below.
    Absent,
    /// `$DATA = 1` — a value, no children. The value may be the empty string.
    ValueOnly,
    /// `$DATA = 10` — children, and NO value of its own. `$GET` returns `""` here, which is why
    /// reading the value alone cannot tell this from `ValueOnly`.
    ChildrenOnly,
    /// `$DATA = 11` — a value AND children.
    ValueAndChildren,
}

impl NodeData {
    pub fn code(&self) -> u8 {
        match self {
            NodeData::Absent => 0,
            NodeData::ValueOnly => 1,
            NodeData::ChildrenOnly => 10,
            NodeData::ValueAndChildren => 11,
        }
    }

    /// Whether this node has a value of its own. False for `ChildrenOnly`, where `$GET`'s `""` is
    /// an artefact of asking rather than a value.
    pub fn has_value(&self) -> bool {
        matches!(self, NodeData::ValueOnly | NodeData::ValueAndChildren)
    }

    pub fn has_children(&self) -> bool {
        matches!(self, NodeData::ChildrenOnly | NodeData::ValueAndChildren)
    }
}

/// Map a raw `$DATA` result. `None` for anything IRIS does not define, so an unexpected number
/// cannot become a plausible state.
pub fn data_status_from_code(code: u8) -> Option<NodeData> {
    match code {
        0 => Some(NodeData::Absent),
        1 => Some(NodeData::ValueOnly),
        10 => Some(NodeData::ChildrenOnly),
        11 => Some(NodeData::ValueAndChildren),
        _ => None,
    }
}

/// Why a subtree walk stopped.
///
/// Three cases, not a boolean. Upstream computed `truncated = (count >= maxNodes)` *after* a
/// 5-second `$ZH` guard could also have stopped the loop, and its own comment called reporting
/// that as complete "conservative and safe". A partial subtree presented as the whole global is
/// the opposite, and the two incomplete cases need different responses from the caller: raise the
/// cap, or narrow the subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEnd {
    /// `$QUERY` left the subtree — every node was returned.
    Complete,
    /// Stopped at the node cap. More nodes exist; ask for more or page on.
    NodeCap,
    /// Stopped at the time limit. How much is left is unknown, so paging from the last node is the
    /// only honest continuation.
    TimeLimit,
}

impl WalkEnd {
    pub fn is_complete(&self) -> bool {
        matches!(self, WalkEnd::Complete)
    }

    /// The token the generated ObjectScript writes, and the one the parser reads back.
    pub fn wire_code(&self) -> &'static str {
        match self {
            WalkEnd::Complete => "done",
            WalkEnd::NodeCap => "cap",
            WalkEnd::TimeLimit => "time",
        }
    }

    pub fn from_wire(code: &str) -> Option<Self> {
        match code {
            "done" => Some(WalkEnd::Complete),
            "cap" => Some(WalkEnd::NodeCap),
            "time" => Some(WalkEnd::TimeLimit),
            _ => None,
        }
    }
}

// ─── bounds ──────────────────────────────────────────────────────────────────────────────────

pub fn clamp_max_subscripts(v: i64) -> i64 {
    v.clamp(1, 500)
}

pub fn clamp_max_nodes(v: i64) -> i64 {
    v.clamp(1, 1000)
}

// ─── what a read reports ─────────────────────────────────────────────────────────────────────

/// One node, as a read describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReport {
    /// IRIS's own canonical reference for the node, from `$NAME`. Worth returning rather than
    /// echoing the request: it shows which node was actually addressed, so a caller who asked for
    /// `^X("1")` can see that it read `^X(1)`.
    pub name: String,
    pub data: NodeData,
    /// `None` when the node has no value OF ITS OWN — `$DATA` 0 or 10. `$GET` returns `""` for
    /// both of those, and reporting that `""` as a value is the defect being fixed.
    pub value: Option<String>,
}

/// One level of subscripts, and whether that is all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub subscripts: Vec<String>,
    pub end: WalkEnd,
}

// ─── generated ObjectScript ──────────────────────────────────────────────────────────────────
//
// Every field is written as `<characters>:<text>` and decoded by
// `crate::tools::interop::parse_length_prefixed_entries`, which #395 added for exactly this
// problem one tool over: lookup listings were framed with `$CHAR(10)` and split on newlines, so a
// key containing a line feed became two phantom entries and inflated the count. Global VALUES are
// worse than lookup keys — routine-era data holds `^`, `|`, and embedded line feeds as ordinary
// characters — so the delimiter has to be a length, not a character.

/// Read one node: its canonical name, its `$DATA`, and its value.
///
/// Three length-prefixed fields in a fixed order: status, name, value. `$DATA` is written as
/// itself rather than as a boolean, which is what lets the parser tell a node holding `""` from a
/// node holding only children.
pub fn build_get_code(name: &str, subscripts: &[Subscript]) -> String {
    let gref = build_global_ref(name, subscripts);
    [
        format!(" Set st = $Data({gref})"),
        format!(" Set nm = $Name({gref})"),
        format!(" Set vl = $Get({gref})"),
        r#" Write $Length(st)_":"_st"#.to_string(),
        r#" Write $Length(nm)_":"_nm"#.to_string(),
        r#" Write $Length(vl)_":"_vl,$Char(10)"#.to_string(),
    ]
    .join("\n")
}

/// List one level of subscripts under `parent`, resuming after `after` when given.
///
/// The reference is assembled from the NAME and the parent subscripts, never by string surgery on
/// a rendered one. Upstream derived it with `gref.rsplit_once(')')`, which happens to work but
/// makes the result depend on punctuation inside a quoted subscript; building it from the parts
/// cannot.
///
/// `more` is read off the loop variable rather than compared against the cap. When the loop stops
/// because `$ORDER` returned `""` the level is exhausted and `sub` is empty; when it stops at the
/// cap, `sub` already holds the first subscript it did not write. So a level holding EXACTLY
/// `max_subscripts` entries reports `done`, where upstream's `truncated = (count >= max)` reports
/// it as truncated and sends the caller back for a page that does not exist.
pub fn build_list_code(
    name: &str,
    parent: &[Subscript],
    max_subscripts: i64,
    after: Option<&Subscript>,
) -> String {
    let mut parts: Vec<String> = parent.iter().map(subscript_expr).collect();
    parts.push("sub".to_string());
    let order_ref = format!("^{}({})", name, parts.join(","));
    let seed = after
        .map(subscript_expr)
        .unwrap_or_else(|| "\"\"".to_string());
    [
        format!(" Set maxSubs = {}", clamp_max_subscripts(max_subscripts)),
        format!(" Set sub = {seed}"),
        " Set cnt = 0".to_string(),
        // One line for the whole For body: dot-continuation inside a Try block is unreliable, and
        // the generator wraps this in one.
        format!(
            r#" For  Set sub=$Order({order_ref})  Quit:sub=""  Quit:(cnt'<maxSubs)  Write $Length(sub)_":"_sub  Set cnt=cnt+1"#
        ),
        r#" Set endr=$Select(sub'="":"cap",1:"done")"#.to_string(),
        r#" Write $Length(endr)_":"_endr,$Char(10)"#.to_string(),
    ]
    .join("\n")
}

// ─── reading the wire back ───────────────────────────────────────────────────────────────────

/// Decode [`build_get_code`]'s three fields.
///
/// A field count other than three is an error rather than a partial report: the generator can
/// truncate output, and two fields plus a default would read as a node whose value is empty.
pub fn parse_get_output(raw: &str) -> Result<NodeReport, String> {
    let fields = crate::tools::interop::parse_length_prefixed_entries(raw)?;
    if fields.len() != 3 {
        return Err(format!(
            "a node read returns exactly 3 fields (status, name, value); got {} from {raw:?}",
            fields.len()
        ));
    }
    let code: u8 = fields[0]
        .trim()
        .parse()
        .map_err(|_| format!("'{}' is not a $DATA value", fields[0]))?;
    let data = data_status_from_code(code)
        .ok_or_else(|| format!("$DATA returned {code}, which is not one of 0, 1, 10 or 11"))?;
    // The value is kept only when the node HAS one. `$GET` gives "" for $DATA 0 and 10 alike, and
    // passing that through is what made "empty" and "absent" the same answer.
    let value = if data.has_value() {
        Some(fields[2].clone())
    } else {
        None
    };
    Ok(NodeReport {
        name: fields[1].clone(),
        data,
        value,
    })
}

/// Decode [`build_list_code`]: the subscripts, then the end marker as the final field.
///
/// A stream with no end marker is an error. Zero subscripts and no marker is not an empty level —
/// it is a read that did not finish — and upstream's `global_preview` had to learn the same thing
/// the hard way: it minted a kill-authorising token off output it had never seen a terminator in.
pub fn parse_list_output(raw: &str) -> Result<ListPage, String> {
    let mut fields = crate::tools::interop::parse_length_prefixed_entries(raw)?;
    let marker = fields.pop().ok_or_else(|| {
        format!("a subscript listing ends with its end marker; the output was empty: {raw:?}")
    })?;
    let end = WalkEnd::from_wire(&marker).ok_or_else(|| {
        format!(
            "'{marker}' is not an end marker, so this listing did not finish; expected one of \
             done, cap, time"
        )
    })?;
    Ok(ListPage {
        subscripts: fields,
        end,
    })
}

/// A subtree, and why the walk stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkPage {
    pub nodes: Vec<NodeReport>,
    pub end: WalkEnd,
    /// The last node returned, as `$NAME` gave it. Pass it back as `after` to continue.
    ///
    /// Present whenever at least one node was returned, including on a complete walk, so a caller
    /// never has to reconstruct a reference by string-joining subscripts it just received.
    pub resume_after: Option<String>,
}

/// How long the generated `$QUERY` loop may run before it stops and says so.
///
/// Upstream used the same five seconds and then reported the result as complete. The number is not
/// the problem; reporting a clock-stop as the end of the data is.
pub const WALK_SECONDS: u64 = 5;

/// Walk a subtree with `$QUERY`, bounded by nodes AND by time, reporting which bound it hit.
///
/// `$QUERY` starts *after* the node it is given and never returns the base itself, so a base
/// holding both a value and children (`$DATA = 11`) would lose its own value. It is emitted first,
/// whenever `$DATA > 0`, including the `$DATA = 10` case — "this node exists and has only
/// children" is a fact worth returning, not an empty row.
///
/// The descendant test compares against the reference up to its closing parenthesis plus a comma
/// (`^G("a")` → `^G("a",`). Comparing against the whole reference never matches below a
/// subscripted node, and a prefix without the comma would accept `^G(10)` as a descendant of
/// `^G(1)`.
pub fn build_walk_code(
    name: &str,
    subscripts: &[Subscript],
    max_nodes: i64,
    after: Option<&str>,
) -> String {
    let gref = build_global_ref(name, subscripts);
    // A resume point arrives as a canonical reference from a previous page, so it is spliced as a
    // STRING and dereferenced with @, never pasted in as code.
    let seed = match after {
        Some(a) => crate::objectscript::os_str_expr(a),
        None => "base".to_string(),
    };
    [
        " Set t0 = $ZH".to_string(),
        format!(" Set maxN = {}", clamp_max_nodes(max_nodes)),
        format!(" Set base = $Name({gref})"),
        format!(" Set node = {seed}"),
        " Set cnt = 0".to_string(),
        " Set bst = $Data(@base)".to_string(),
        " Set bv = $Get(@base)".to_string(),
        // The base is emitted only on a FIRST page. On a resumed page it was already returned.
        " Set emitBase = ((bst>0)&(node=base))".to_string(),
        r#" If emitBase Write $Length(bst)_":"_bst,$Length(base)_":"_base,$Length(bv)_":"_bv"#
            .to_string(),
        " If emitBase Set cnt=cnt+1".to_string(),
        r#" Set prefix = $Select($Extract(base,$Length(base))=")":$Extract(base,1,$Length(base)-1)_",",1:base_"(")"#
            .to_string(),
        // One line for the whole For body: dot-continuation inside the generator's Try block is
        // unreliable. Quit order matters — the two "left the subtree" tests come before the two
        // bounds, so an exhausted subtree is never misreported as a bound.
        format!(
            r#" For  Set node=$Query(@node)  Quit:node=""  Quit:($Extract(node,1,$Length(prefix))'=prefix)  Quit:(cnt'<maxN)  Quit:(($ZH-t0)>{WALK_SECONDS})  Set st=$Data(@node),vl=$Get(@node)  Write $Length(st)_":"_st,$Length(node)_":"_node,$Length(vl)_":"_vl  Set cnt=cnt+1  Set last=node"#
        ),
        // Why it stopped, decided from STATE rather than from a count comparison. If the loop left
        // a node that is still inside the subtree, it stopped on a bound — and `cnt'<maxN` says
        // which, because the node cap is tested before the clock. If it ran out of subtree, `node`
        // is empty or outside the prefix and the walk is complete.
        r#" Set inSub = ((node'="")&($Extract(node,1,$Length(prefix))=prefix))"#.to_string(),
        r#" Set endr = $Select('inSub:"done",cnt'<maxN:"cap",1:"time")"#.to_string(),
        r#" Write $Length(endr)_":"_endr,$Char(10)"#.to_string(),
    ]
    .join("\n")
}

/// Decode [`build_walk_code`]: triples of (status, name, value), then the end marker.
///
/// A field count that is not `3n + 1` is an error. The generator can truncate its output, and
/// dropping a partial triple would turn a cut-off read into a shorter subtree that looks whole.
pub fn parse_walk_output(raw: &str) -> Result<WalkPage, String> {
    let mut fields = crate::tools::interop::parse_length_prefixed_entries(raw)?;
    let marker = fields.pop().ok_or_else(|| {
        format!("a subtree walk ends with its end marker; the output was empty: {raw:?}")
    })?;
    let end = WalkEnd::from_wire(&marker).ok_or_else(|| {
        format!(
            "'{marker}' is not an end marker, so this walk did not finish; expected one of done, \
             cap, time"
        )
    })?;
    if fields.len() % 3 != 0 {
        return Err(format!(
            "a subtree walk returns 3 fields per node; {} fields is a truncated read, not a \
             shorter subtree",
            fields.len()
        ));
    }
    let mut nodes = Vec::with_capacity(fields.len() / 3);
    for triple in fields.chunks(3) {
        let code: u8 = triple[0]
            .trim()
            .parse()
            .map_err(|_| format!("'{}' is not a $DATA value", triple[0]))?;
        let data = data_status_from_code(code)
            .ok_or_else(|| format!("$DATA returned {code}, which is not one of 0, 1, 10 or 11"))?;
        let value = if data.has_value() {
            Some(triple[2].clone())
        } else {
            None
        };
        nodes.push(NodeReport {
            name: triple[1].clone(),
            data,
            value,
        });
    }
    let resume_after = nodes.last().map(|n| n.name.clone());
    Ok(WalkPage {
        nodes,
        end,
        resume_after,
    })
}

// ─── the tool's parameters and its read plan ─────────────────────────────────────────────────

use schemars::JsonSchema;
use serde::Deserialize;

/// What `iris_global` was asked to do.
///
/// An enum with a named set rather than a string compared in a `match`, so an unrecognised action
/// is refused naming the alternatives. #541 is the cost of the other shape: a value nobody
/// recognised became a silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalAction {
    /// One node: its `$DATA`, its canonical name, and its value if it has one.
    Get,
    /// One level of subscripts under a node, paged.
    List,
    /// A bounded `$QUERY` walk of a subtree, paged, reporting which bound stopped it.
    Walk,
}

impl GlobalAction {
    pub const NAMES: &'static [&'static str] = &["get", "list", "walk"];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "get" => Some(GlobalAction::Get),
            "list" => Some(GlobalAction::List),
            "walk" => Some(GlobalAction::Walk),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GlobalAction::Get => "get",
            GlobalAction::List => "list",
            GlobalAction::Walk => "walk",
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrisGlobalParams {
    /// get, list or walk. Read actions only — this build does not write or kill.
    #[schemars(extend("enum" = ["get", "list", "walk"]))]
    pub action: String,
    /// The global, with or without its leading `^`. Subscripts go in `subscripts`, not here.
    pub global_name: String,
    /// Subscripts as JSON values. A NUMBER addresses the numeric subscript; a STRING addresses
    /// that exact string. The two are the same node when the string is a canonic number — IRIS
    /// canonicalises `"1"` to `1` — and different nodes when it is not, so `"01"` is its own key.
    /// Commas, quotes, carets, accents and control characters are all data.
    pub subscripts: Option<Vec<serde_json::Value>>,
    /// Defaults to the connection's namespace.
    pub namespace: Option<String>,
    /// list: how many subscripts per page (default 50, max 500).
    pub max_subscripts: Option<i64>,
    /// walk: how many nodes per page (default 100, max 1000).
    pub max_nodes: Option<i64>,
    /// list: resume after this subscript, as returned in a previous page.
    pub after_subscript: Option<serde_json::Value>,
    /// walk: resume after this node, passing back a previous page's `resume_after` verbatim.
    pub after_node: Option<String>,
}

/// A validated request plus the ObjectScript that answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub action: GlobalAction,
    /// The global name, `^` stripped and validated.
    pub name: String,
    pub code: String,
}

/// Validate parameters and generate the code, without touching IRIS.
///
/// Everything that can be wrong about a request is decided here, so the transport layer has no
/// judgement to make and the whole of it is testable without an instance. `Err` carries the error
/// code and the message, in that order.
pub fn plan_read(p: &IrisGlobalParams) -> Result<ReadPlan, (&'static str, String)> {
    let Some(action) = GlobalAction::parse(&p.action) else {
        return Err((
            "INVALID_ACTION",
            format!(
                "'{}' is not an action of iris_global. Valid actions: {}. This build reads only — \
                 set and kill are not implemented here.",
                p.action,
                GlobalAction::NAMES.join(", ")
            ),
        ));
    };

    let name = normalize_global_name(&p.global_name);
    if let Some(why) = global_name_error(&name) {
        return Err(("INVALID_GLOBAL_NAME", why));
    }

    let subscripts: Vec<Subscript> = p
        .subscripts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(Subscript::from_json)
        .collect();

    // `after_node` belongs to `walk` and `after_subscript` to `list`. Taking the wrong one
    // silently would resume from the beginning, which reads as a complete answer that quietly
    // repeats a page.
    if action != GlobalAction::Walk && p.after_node.is_some() {
        return Err((
            "INVALID_PARAMS",
            format!(
                "after_node resumes a walk; action={} pages with after_subscript",
                action.as_str()
            ),
        ));
    }
    if action != GlobalAction::List && p.after_subscript.is_some() {
        return Err((
            "INVALID_PARAMS",
            format!(
                "after_subscript resumes a list; action={} pages with after_node",
                action.as_str()
            ),
        ));
    }

    let code = match action {
        GlobalAction::Get => build_get_code(&name, &subscripts),
        GlobalAction::List => {
            let after = p.after_subscript.as_ref().map(Subscript::from_json);
            build_list_code(
                &name,
                &subscripts,
                p.max_subscripts.unwrap_or(50),
                after.as_ref(),
            )
        }
        GlobalAction::Walk => build_walk_code(
            &name,
            &subscripts,
            p.max_nodes.unwrap_or(100),
            p.after_node.as_deref(),
        ),
    };

    Ok(ReadPlan { action, name, code })
}

/// Turn one of this module's parse errors into the tool's error envelope body.
///
/// A malformed reply is `IRIS_UNREADABLE_OUTPUT`, never an empty result: the reads here answer
/// "what is in this global", and an empty answer to that question is indistinguishable from "this
/// global is empty" unless the failure is named.
pub fn unreadable(detail: &str) -> (&'static str, String) {
    (
        "IRIS_UNREADABLE_OUTPUT",
        format!(
            "IRIS answered, but the reply could not be read, so what is in this global is UNKNOWN \
             — this is not an empty result: {detail}"
        ),
    )
}

// ─── the response shape ──────────────────────────────────────────────────────────────────────

impl NodeData {
    /// Plain words for the `$DATA` number, so a reader who does not know the convention is not
    /// left to guess which of 1, 10 and 11 means what.
    pub fn meaning(&self) -> &'static str {
        match self {
            NodeData::Absent => "no value and no children",
            NodeData::ValueOnly => "a value, no children",
            NodeData::ChildrenOnly => "children only, no value of its own",
            NodeData::ValueAndChildren => "a value and children",
        }
    }
}

/// Render one node.
///
/// `value` is `null` when the node has none, and `has_value` is what distinguishes that from a
/// value which happens to be the empty string — the two are the same JSON otherwise, and telling
/// them apart is the whole reason `$DATA` is carried.
pub fn node_json(n: &NodeReport) -> serde_json::Value {
    serde_json::json!({
        "name": n.name,
        "data": n.data.code(),
        "data_meaning": n.data.meaning(),
        "has_value": n.data.has_value(),
        "has_children": n.data.has_children(),
        "value": n.value,
    })
}

/// Render a listing. `complete` is stated as well as `end`, because the question a caller asks is
/// "is that all of them".
pub fn list_json(page: &ListPage, namespace: &str) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "action": "list",
        "namespace": namespace,
        "subscripts": page.subscripts,
        "count": page.subscripts.len(),
        "complete": page.end.is_complete(),
        "stopped_because": page.end.wire_code(),
        "resume_after_subscript": page.subscripts.last(),
    })
}

/// Render a walk. The two incomplete reasons are reported separately because the caller's next
/// move differs: `cap` means ask for another page, `time` means the subtree is too large to
/// enumerate at this rate and should be narrowed.
pub fn walk_json(page: &WalkPage, namespace: &str) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "action": "walk",
        "namespace": namespace,
        "nodes": page.nodes.iter().map(node_json).collect::<Vec<_>>(),
        "count": page.nodes.len(),
        "complete": page.end.is_complete(),
        "stopped_because": page.end.wire_code(),
        "resume_after": page.resume_after,
    })
}
