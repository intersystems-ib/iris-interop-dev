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
    // Upstream v1.7.0 added this bound when it collapsed its two name checks into
    // `canonical_global_name`. It is not an injection guard — the pattern below already admits
    // only letters, digits, `.` and a leading `%` — it turns a pathological name into a refusal
    // decided here rather than an abort decided by IRIS, which is what this module's contract
    // says: everything that can be wrong about a request is wrong before an instance is involved.
    if name.chars().count() > 128 {
        let shown: String = name.chars().take(40).collect();
        return Some(format!(
            "'{shown}…' is {} characters; a global name is at most 128 here. Subscripts go in \
             'subscripts', not in the name.",
            name.chars().count()
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
    /// Write one node's value. The only write action here: removal goes through `global_preview`
    /// and `global_kill`, so there is exactly one destructive path and it is the confirmed one.
    Set,
}

impl GlobalAction {
    pub const NAMES: &'static [&'static str] = &["get", "list", "walk", "set"];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "get" => Some(GlobalAction::Get),
            "list" => Some(GlobalAction::List),
            "walk" => Some(GlobalAction::Walk),
            "set" => Some(GlobalAction::Set),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GlobalAction::Get => "get",
            GlobalAction::List => "list",
            GlobalAction::Walk => "walk",
            GlobalAction::Set => "set",
        }
    }

    /// Whether this action changes the global.
    ///
    /// Written as an exhaustive match with no `_` arm, so a later action cannot be added without
    /// deciding this. The read actions were classified here rather than by a `!= Set` test for
    /// the same reason: a negative test silently classifies whatever arrives next as a read.
    pub fn is_write(&self) -> bool {
        match self {
            GlobalAction::Get | GlobalAction::List | GlobalAction::Walk => false,
            GlobalAction::Set => true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrisGlobalParams {
    /// get, list, walk or set. Removal is not an action here: it goes through `global_preview`
    /// and `global_kill`, which name what is about to be deleted before deleting it.
    #[schemars(extend("enum" = ["get", "list", "walk", "set"]))]
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
    /// set: the value to write. Any bytes — quotes, carets, line feeds, accents — are data and
    /// are escaped on the way in. Required for `set` and refused for the read actions, rather
    /// than ignored there, because a value silently dropped reads as a write that happened.
    pub value: Option<String>,
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
        return Err(unknown_action(&p.action));
    };
    if action.is_write() {
        return Err((
            INVALID_PARAMS,
            format!(
                "action={} writes and does not belong on the read path",
                action.as_str()
            ),
        ));
    }

    let name = normalize_global_name(&p.global_name);
    if let Some(why) = global_name_error(&name) {
        return Err((INVALID_GLOBAL_NAME, why));
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
            INVALID_PARAMS,
            format!(
                "after_node resumes a walk; action={} pages with after_subscript",
                action.as_str()
            ),
        ));
    }
    if action != GlobalAction::List && p.after_subscript.is_some() {
        return Err((
            INVALID_PARAMS,
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
        // Unreachable: `is_write` above returns early. Spelled out rather than `unreachable!()`
        // so that adding an action forces a decision here instead of a panic in production.
        GlobalAction::Set => {
            return Err((
                INVALID_PARAMS,
                "action=set writes and does not belong on the read path".to_string(),
            ))
        }
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
        IRIS_UNREADABLE_OUTPUT,
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

// ─── writing one node ────────────────────────────────────────────────────────────────────────
//
// #532's write half. The read actions above needed no opinion about WHICH global they touched;
// a write does, and the opinion is not a taste question. `envelope.rs:233-237` records a measured
// run in which a model wrote `Kill ^%Dictionary(...)`, `Set ^SYS("Compile","Names",...)` and
// `Kill ^oddDEF(...)` into a working namespace across 29 tool calls while chasing a phantom
// SearchTable registration. Those three names are the class dictionary, the compiler's own state
// and the routine index: writing them corrupts the namespace in ways no later tool call can
// explain. So the gate ships WITH the write, not after it.
//
// Upstream has such a list and it is not the thing to copy. Two reasons, both read from its
// source at `v1.6.2`:
//
//  1. It is bypassable exactly where it matters. `system_blocklist_gate.rs:27-29` returns `None`
//     — no refusal — BEFORE consulting the list when `is_kill_op && matches_any_owned(name,
//     kill_allowlist)`, and that allowlist is the operator-settable config key
//     `dataPolicyKillAllowlist`. The module's own doc comment says "Not bypassable by any config"
//     and its remediation string says "No configuration override exists". Both are false for the
//     destructive operation. An exemption that fires only for `kill` is not a narrowing of the
//     guard; it is a hole in it.
//  2. It is a prefix match over a flat list, which upstream's own comments record having to
//     narrow twice: `^SYS*` swallowed `^SYSCONFIG` and `^SYSDATA`, and `^IRIS.Sys*` swallowed
//     `^IRIS.SystemPerformance`. A prefix is the wrong shape for a dotted namespace.
//
// So this is a segment test, not a prefix test, and it has no override.

/// The refusal codes this module emits, as constants.
///
/// Declared rather than written inline because `tests/every_refusal_names_a_remedy.rs` finds a
/// tool's codes by scanning for `const NAME: &str = "VALUE";` and cannot see one returned as a
/// tuple literal. `INVALID_GLOBAL_NAME` and `IRIS_UNREADABLE_OUTPUT` have been live since #550
/// with no remedy row for exactly that reason — invisible to the gate is not absent from the wire.
pub const INVALID_ACTION: &str = "INVALID_ACTION";
pub const INVALID_GLOBAL_NAME: &str = "INVALID_GLOBAL_NAME";
pub const INVALID_PARAMS: &str = "INVALID_PARAMS";
pub const IRIS_UNREADABLE_OUTPUT: &str = "IRIS_UNREADABLE_OUTPUT";
pub const SYSTEM_GLOBAL_WRITE_REFUSED: &str = "SYSTEM_GLOBAL_WRITE_REFUSED";
pub const GLOBAL_FAMILY_UNDETERMINED: &str = "GLOBAL_FAMILY_UNDETERMINED";
pub const WRITE_DID_NOT_LAND: &str = "WRITE_DID_NOT_LAND";

/// Globals whose whole name is reserved. Matched entire, not as a prefix.
///
/// `SYS` is here rather than in [`RESERVED_SEGMENTS`] deliberately: `^SYS` is the compiler's state
/// and must be refused, while `^SYSCONFIG` and `^SYSDATA` are ordinary application globals that a
/// prefix test would swallow — the narrowing upstream had to make after shipping.
pub const RESERVED_EXACT: &[&str] = &[
    "SYS",
    "ROUTINE",
    "rINDEX",
    "rINDEXEXT",
    "rMAP",
    "rOBJ",
    "rBACKUP",
    "FilterAlerts",
    "UnitTestRoot",
];

/// Globals whose FIRST dot-separated segment is reserved. `Ens` refuses `^Ens.MessageHeaderD` and
/// `^Ens`, and leaves `^Ensayo` alone — which a `^Ens*` prefix test would not.
///
/// `IPM`, `AQDev` and `UnitTest` are not in InterSystems' published reserved list and are here on
/// measurement: they are writable on a stock instance, they are nobody's application data, and
/// `^IPM.*` is the package manager's own catalogue. `Ens*` is the interop surface this fork exists
/// to serve, so a write landing there is the most expensive mistake available here.
pub const RESERVED_SEGMENTS: &[&str] = &[
    "Ens",
    "EnsLib",
    "EnsPortal",
    "EnsEDI",
    "EnsHL7",
    "EnsiHL7",
    "IRIS",
    "IPM",
    "AQDev",
    "UnitTest",
    "CacheTemp",
    "CacheTempEns",
    "IRISTemp",
    "ISC",
];

/// `name` up to its first period. `"Ens.MessageHeaderD"` → `"Ens"`, `"IIDTest"` → `"IIDTest"`.
pub fn first_segment(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// True when `name` is `odd` followed by an upper-case letter, or `odd` alone.
///
/// The class dictionary globals are `^oddDEF`, `^oddCOM`, `^oddSQL`, `^oddPROJECT`, `^oddMETA`,
/// `^oddBIX`, `^oddEXT`, `^oddXML`, `^oddTSQL` — `odd` plus an upper-case tag, every one. Testing
/// for the bare prefix would also refuse `^oddments`, which is somebody's data.
pub fn is_class_dictionary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("odd") else {
        return false;
    };
    rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase())
}

/// The same SHAPE with the `odd` spelled in any case: `^OddDEF`, `^ODDDEF`, `^oddDEF`.
///
/// Separate from [`is_class_dictionary`] because the case-folded test cannot be "lowercase it and
/// re-run the strict test" — lowercasing destroys the upper-case tag the strict test keys on, so
/// `^OddDEF` came back ALLOWED. The shape is what survives case folding: three letters spelling
/// `odd`, then a tag that begins upper-case. `^oddments` has a lower-case remainder and is a word,
/// so it is not this shape in any casing.
pub fn resembles_class_dictionary(name: &str) -> bool {
    let Some(head) = name.get(..3) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("odd") {
        return false;
    }
    let rest = &name[3..];
    rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase())
}

/// Whether a write to this global may proceed.
///
/// Three answers, not two. The third exists because the second would otherwise absorb it: IRIS
/// global names are CASE-SENSITIVE, so `^ENS.Foo` is a different global from `^Ens.Foo` and this
/// code cannot know whether the caller meant the reserved family or has a global of their own that
/// differs from it only in case. Answering "allowed" there would let a mis-cased write through to
/// the interop surface; answering "refused" would state a fact about the name that is not
/// established. So it says it cannot tell, and refuses on that basis, naming what it resembles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteVerdict {
    /// No reserved family matches, in any casing.
    Allowed,
    /// A reserved family matches exactly. `family` is the matched rule, for the message.
    Refused { family: String, why: String },
    /// Matches a reserved family only when letter case is ignored, so whether this is that global
    /// is UNKNOWN. Refused, but as a different answer with a different remedy.
    Undetermined { resembles: String, why: String },
}

impl WriteVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, WriteVerdict::Allowed)
    }

    /// The `(code, message)` pair for a verdict that refuses, or `None` for [`WriteVerdict::Allowed`].
    pub fn refusal(&self) -> Option<(&'static str, String)> {
        match self {
            WriteVerdict::Allowed => None,
            WriteVerdict::Refused { why, .. } => Some((SYSTEM_GLOBAL_WRITE_REFUSED, why.clone())),
            WriteVerdict::Undetermined { why, .. } => {
                Some((GLOBAL_FAMILY_UNDETERMINED, why.clone()))
            }
        }
    }
}

/// Classify `name` (already `^`-stripped and validated) against the reserved families.
///
/// There is no configuration override, and that is the point of the comment at the top of this
/// section. A gate with an operator escape hatch for the destructive operation is upstream's
/// shape, and upstream's own guarantee text is false because of it.
pub fn write_verdict(name: &str) -> WriteVerdict {
    // `%z` and `%Z` are reserved for the CUSTOMER, not for InterSystems, so they are the one
    // exemption inside the `%` family and it is an exemption in the permissive direction only.
    if let Some(rest) = name.strip_prefix('%') {
        if rest.starts_with('z') || rest.starts_with('Z') {
            return WriteVerdict::Allowed;
        }
        return WriteVerdict::Refused {
            family: "%".to_string(),
            why: format!(
                "'^{name}' is in the % family, which is InterSystems' own: these globals are \
                 mapped to the system database and hold the class dictionary, the SQL catalogue \
                 and instance configuration. Writing one corrupts state no later tool call can \
                 repair. There is no configuration override for this refusal. Your own globals in \
                 that family are ^%z… and ^%Z…, which are allowed."
            ),
        };
    }

    if is_class_dictionary(name) {
        return WriteVerdict::Refused {
            family: "odd*".to_string(),
            why: format!(
                "'^{name}' is a class dictionary global. The compiler owns it: ^oddDEF holds class \
                 definitions, ^oddCOM the compiled forms, ^oddSQL the SQL projections. Writing one \
                 leaves classes that cannot be compiled or deleted. Change the class with \
                 iris_doc(mode=put) and iris_compile instead."
            ),
        };
    }

    if RESERVED_EXACT.contains(&name) {
        return WriteVerdict::Refused {
            family: name.to_string(),
            why: format!(
                "'^{name}' is a reserved global owned by IRIS itself — the routine index, the \
                 compiler's state or the unit-test harness's root. Writing it corrupts namespace \
                 state. There is no configuration override for this refusal."
            ),
        };
    }

    let seg = first_segment(name);
    if RESERVED_SEGMENTS.contains(&seg) {
        return WriteVerdict::Refused {
            family: format!("{seg}.*"),
            why: format!(
                "'^{name}' is in the ^{seg} family, which IRIS and the interoperability framework \
                 own: message headers, bodies, queues, business-host state and the package \
                 manager's catalogue live there. A write lands in running interop state rather \
                 than in application data. Use the interop tools — iris_interop_query, \
                 iris_production_item, iris_lookup_manage — which understand that state. There is \
                 no configuration override for this refusal."
            ),
        };
    }

    // Same tests again, case-folded. A hit here means the name RESEMBLES a reserved family without
    // being it, and IRIS would treat the two as different globals.
    let folded = name.to_ascii_lowercase();
    let folded_seg = first_segment(&folded);
    let resembles = RESERVED_EXACT
        .iter()
        .find(|r| r.to_ascii_lowercase() == folded)
        .or_else(|| {
            RESERVED_SEGMENTS
                .iter()
                .find(|r| r.to_ascii_lowercase() == folded_seg)
        });
    if let Some(r) = resembles {
        return WriteVerdict::Undetermined {
            resembles: (*r).to_string(),
            why: format!(
                "'^{name}' differs from the reserved global ^{r} only in letter case. IRIS global \
                 names are case-sensitive, so these may well be two different globals and yours \
                 may be perfectly ordinary — but this call cannot establish which you meant, and \
                 guessing either way is a statement about your data that has not been checked. \
                 Nothing was written. If ^{name} really is your own, read it first with \
                 action=get to confirm it is the global you expect."
            ),
        };
    }
    if resembles_class_dictionary(name) {
        return WriteVerdict::Undetermined {
            resembles: "odd*".to_string(),
            why: format!(
                "'^{name}' differs from the class dictionary family ^odd… only in letter case. \
                 IRIS global names are case-sensitive, so this may be your own global — but this \
                 call cannot establish that, and a write to the real ^oddDEF leaves classes that \
                 cannot be compiled. Nothing was written. Read it first with action=get to confirm."
            ),
        };
    }

    WriteVerdict::Allowed
}

/// The refusal for an action this tool does not have.
///
/// One builder for all three planners rather than three copies of the same `format!`, which is the
/// house rule the comment at `skills_tools.rs:304-328` states for `resolve_namespace`.
///
/// A caller who asked to remove a global is told which tools remove one. Without that, the refusal
/// says only "not an action", the caller reads it as a typo, and retries the same call — the thing
/// `denial-text` discipline is for: name the remedy, not just the rejection.
pub fn unknown_action(requested: &str) -> (&'static str, String) {
    let asked_to_remove = matches!(
        requested.trim().to_lowercase().as_str(),
        "kill" | "delete" | "remove" | "clear" | "zkill"
    );
    let mut msg = format!(
        "'{}' is not an action of iris_global. Valid actions: {}.",
        requested,
        GlobalAction::NAMES.join(", ")
    );
    if asked_to_remove {
        msg.push_str(
            " Removing a global is deliberately NOT an action here: call global_preview to see \
             exactly which nodes would go, then global_kill with the token it returns. That way a \
             deletion is never one call away from a typo, and the preview cannot under-report what \
             the kill removes.",
        );
    }
    (INVALID_ACTION, msg)
}

/// Write one node, and report what `$DATA` said before and after.
///
/// The success oracle is the re-read, not an echoed literal. Upstream decides a kill worked with
/// `out.trim().contains("KILLED")` against a string its own generated code wrote unconditionally —
/// which reports success for any path that reaches the `Write`. Here the final `$DATA` is read
/// back out of IRIS after the `Set`, so "the write landed" is a measurement. A `Set` that aborts
/// never reaches the trailing writes at all, the field count comes up short, and
/// [`parse_set_output`] reports that rather than inventing a node.
///
/// `prior` is carried because "created" and "overwrote" are different events and the caller cannot
/// recover the difference afterwards.
pub fn build_set_code(name: &str, subscripts: &[Subscript], value: &str) -> String {
    let gref = build_global_ref(name, subscripts);
    [
        format!(" Set pr = $Data({gref})"),
        format!(
            " Set {} = {}",
            gref,
            crate::objectscript::os_str_expr(value)
        ),
        format!(" Set st = $Data({gref})"),
        format!(" Set nm = $Name({gref})"),
        r#" Write $Length(pr)_":"_pr"#.to_string(),
        r#" Write $Length(st)_":"_st"#.to_string(),
        r#" Write $Length(nm)_":"_nm,$Char(10)"#.to_string(),
    ]
    .join("\n")
}

/// What a `set` did, read back from IRIS rather than assumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetReport {
    /// IRIS's own `$NAME` of the node that was written.
    pub name: String,
    /// `$DATA` before the write.
    pub prior: NodeData,
    /// `$DATA` after the write. A node that was just given a value always has one, so a `current`
    /// without a value is a failed write, not a node with no value.
    pub current: NodeData,
}

impl SetReport {
    /// True when the node had no value of its own before this write.
    ///
    /// Read off `prior`, so a node that already had children but no value reports `created` —
    /// which is what happened to its value, and the children are reported separately.
    pub fn created(&self) -> bool {
        !self.prior.has_value()
    }
}

/// Decode [`build_set_code`]'s three fields.
///
/// A short read is an error. The write may or may not have landed, and that is exactly what this
/// says: the alternative — filling in a plausible `current` — would report a node this code never
/// saw.
pub fn parse_set_output(raw: &str) -> Result<SetReport, String> {
    let fields = crate::tools::interop::parse_length_prefixed_entries(raw)?;
    if fields.len() != 3 {
        return Err(format!(
            "a node write returns exactly 3 fields (prior status, new status, name); got {} from \
             {raw:?}. Whether the write landed is UNKNOWN — read the node with action=get",
            fields.len()
        ));
    }
    let parse_code = |s: &str| -> Result<NodeData, String> {
        let code: u8 = s
            .trim()
            .parse()
            .map_err(|_| format!("'{s}' is not a $DATA value"))?;
        data_status_from_code(code)
            .ok_or_else(|| format!("$DATA returned {code}, which is not one of 0, 1, 10 or 11"))
    };
    let prior = parse_code(&fields[0])?;
    let current = parse_code(&fields[1])?;
    Ok(SetReport {
        name: fields[2].clone(),
        prior,
        current,
    })
}

/// A validated write request plus the ObjectScript that performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePlan {
    pub action: GlobalAction,
    pub name: String,
    pub code: String,
}

/// Either half of [`plan`], so a caller holding one knows which it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalPlan {
    Read(ReadPlan),
    Write(WritePlan),
}

/// Validate a write and generate its code, without touching IRIS.
///
/// Refuses before anything is spliced, in this order: the action must be a write, the name must be
/// a name, the name must not be reserved, and a `set` must carry a value. The blocklist is
/// consulted HERE rather than at the transport layer so that it is decided in the same place as
/// every other refusal and is testable without an instance.
pub fn plan_write(p: &IrisGlobalParams) -> Result<WritePlan, (&'static str, String)> {
    let Some(action) = GlobalAction::parse(&p.action) else {
        return Err(unknown_action(&p.action));
    };
    if !action.is_write() {
        return Err((
            INVALID_PARAMS,
            format!(
                "action={} is a read and does not belong on the write path",
                action.as_str()
            ),
        ));
    }

    let name = normalize_global_name(&p.global_name);
    if let Some(why) = global_name_error(&name) {
        return Err((INVALID_GLOBAL_NAME, why));
    }
    if let Some(refusal) = write_verdict(&name).refusal() {
        return Err(refusal);
    }

    let Some(value) = p.value.as_deref() else {
        return Err((
            INVALID_PARAMS,
            "action=set requires a 'value'. To remove a node use global_preview and global_kill, \
             which confirm what is being deleted; a set with no value would otherwise be a quiet \
             way to blank one."
                .to_string(),
        ));
    };

    if p.after_node.is_some() || p.after_subscript.is_some() {
        return Err((
            INVALID_PARAMS,
            "after_node and after_subscript page a read; a write addresses one node".to_string(),
        ));
    }

    let subscripts: Vec<Subscript> = p
        .subscripts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(Subscript::from_json)
        .collect();

    Ok(WritePlan {
        action,
        name: name.clone(),
        code: build_set_code(&name, &subscripts, value),
    })
}

/// Route a request to the read or the write planner by its action.
///
/// One parse of the action for the whole call, so a request cannot be read as one action here and
/// another there.
pub fn plan(p: &IrisGlobalParams) -> Result<GlobalPlan, (&'static str, String)> {
    match GlobalAction::parse(&p.action) {
        Some(a) if a.is_write() => plan_write(p).map(GlobalPlan::Write),
        Some(_) => plan_read(p).map(GlobalPlan::Read),
        None => Err(unknown_action(&p.action)),
    }
}

/// Render a completed write.
///
/// `created` and the two `$DATA` states are all reported, because "I set a node that did not
/// exist" and "I overwrote a value" are different outcomes and the caller cannot tell them apart
/// from the new state alone.
pub fn set_json(r: &SetReport, namespace: &str) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "action": "set",
        "namespace": namespace,
        "name": r.name,
        "created": r.created(),
        "previous_data": r.prior.code(),
        "previous_data_meaning": r.prior.meaning(),
        "data": r.current.code(),
        "data_meaning": r.current.meaning(),
        "has_children": r.current.has_children(),
    })
}

/// Decide what a completed `set` means, as a pure function.
///
/// This lives here rather than in the dispatch for a reason a mutation test makes concrete: a
/// mutant that neuters the read-back check inside `call_tool` SURVIVES, because that branch needs
/// a live connection to reach and no offline test can get there. Moved here, the same break is
/// killed by `a_write_whose_read_back_is_absent_is_refused_not_reported`. The transport layer is
/// left with no judgement to make, which is what `plan_read`'s doc comment already claims for the
/// read half.
pub fn set_outcome(
    r: &SetReport,
    namespace: &str,
) -> Result<serde_json::Value, (&'static str, String)> {
    if !r.current.has_value() {
        return Err(write_did_not_land(r));
    }
    Ok(set_json(r, namespace))
}

/// The refusal for a write whose read-back says nothing is there.
///
/// Reachable only if IRIS accepted the `Set`, answered all three fields, and still reports
/// `$DATA` without a value. That should be impossible; if it happens the honest answer is that the
/// write did not land, not a success envelope describing a node with no value in it.
pub fn write_did_not_land(r: &SetReport) -> (&'static str, String) {
    (
        WRITE_DID_NOT_LAND,
        format!(
            "the Set was accepted but reading '{}' back reports $DATA={} ({}), so the value is \
             NOT there — this is not a successful write. Check the namespace is writable and that \
             no mapping sends this global to a read-only database.",
            r.name,
            r.current.code(),
            r.current.meaning()
        ),
    )
}
