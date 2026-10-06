//! iris_source_control — SCM status, menu, checkout, execute via Atelier xecute.

use crate::elicitation::{ElicitationAction, ElicitationStore};
use crate::iris::connection::IrisConnection;
use schemars::JsonSchema;
use serde::Deserialize;

fn ok_json(v: serde_json::Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    Ok(rmcp::model::CallToolResult::success(vec![
        rmcp::model::Content::text(v.to_string()),
    ]))
}
/// Genuine failures go through the fork's single failure envelope (issue #2):
/// {success:false, error_code, error} + isError on the wire. Elicitation dialogs
/// are NOT failures and stay ok_json (success:false + elicitation_required:true).
fn err_json(code: &str, msg: &str) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    crate::tools::envelope::fail(code, msg)
}

/// Issue #101: a wrong password made every SCM action answer `SCM_UNAVAILABLE` with no hint
/// at all, which reads as "this instance has no source control" — a statement about the
/// server's configuration, for a request IRIS rejected at the door. `SCM_UNAVAILABLE` is
/// still right when the SCM session genuinely could not start; it is not right for a 401,
/// a 403, or a closed port, and those three now say what they are and carry the hint that
/// goes with them.
// pub(crate) since #342: the elicitation-resume path in doc.rs finalizes a checkout too and must
// classify a failed AfterUserAction the same way this module does. One classifier, not two — the
// duplication that hid #342 in the first place was two call sites disagreeing about error handling.
pub(crate) fn scm_error_code(msg: &str) -> &'static str {
    crate::tools::interop::classify_iris_error_or(msg, "SCM_UNAVAILABLE")
}

/// Menu prefix used for source control actions.
pub const SCM_MENU: &str = "%SourceMenu";

/// #302: returned when a `UserAction` snippet produced no output at all.
///
/// Distinct from `SCM_UNAVAILABLE` (the SCM session could not start) and from `SCM_ERROR` (IRIS
/// said something and it was not a code). This one means IRIS said *nothing*, which the generated
/// snippet cannot do if it ran — so the outcome of the action is genuinely unknown.
const SCM_NO_OUTPUT: &str = "SCM_NO_OUTPUT";

/// The action can only be completed in the provider's own web UI (action 2 or 3).
/// An administrative source-control action, refused unless the operator opted in (#418 §5).
pub(crate) const SCM_ADMIN_BLOCKED: &str = "SCM_ADMIN_BLOCKED";

/// Check-in is refused unless the operator opted in.
///
/// These next three are `const` for a reason worth recording, because it is a trap this refactor
/// walked into. They used to be argument-position literals — `err_json("CHECKIN_BLOCKED", …)` —
/// which is one of the shapes `every_refusal_names_a_remedy` can see. Moving the decision into a
/// shared function (`opt_in_refusal`, `after_user_action_outcome`, `undriveable_refusal`) turned
/// each of them into a struct field or a tuple element, visible to nothing, and the gate reported
/// all three as **stale remedy rows for codes the server can no longer emit** — the opposite of the
/// truth. Naming them makes them visible again through the `const` producer that same gate grew for
/// #418. A de-duplication can narrow a guard without touching it.
pub(crate) const CHECKIN_BLOCKED: &str = "CHECKIN_BLOCKED";

/// A checkout was attempted and refused, or could not be confirmed.
pub(crate) const SCM_CHECKOUT_FAILED: &str = "SCM_CHECKOUT_FAILED";

/// The source-control hook raised an error, or answered something this tool cannot drive.
pub(crate) const SCM_ERROR: &str = "SCM_ERROR";

pub(crate) const SCM_NEEDS_UI: &str = "SCM_NEEDS_UI";

/// The provider wants a typed value (action 7) on a path that cannot ask for one.
pub(crate) const SCM_NEEDS_INPUT: &str = "SCM_NEEDS_INPUT";

const EMPTY_OUTPUT_MSG: &str =
    "The source control action produced no output. The generated snippet \
     always writes an action code, so an empty response means it never ran and the outcome of the \
     action is unknown. Check the document's checkout state before retrying.";

/// SCM menu actions as reported by %Studio.SourceControl.Interface:MenuItems.
#[derive(Debug, PartialEq, Eq)]
pub enum ScmAction {
    CheckOut,
    UndoCheckout,
    CheckIn,
    GetLatest,
    AddToSourceControl,
    Diff,
    Disconnect,
    Reconnect,
    Unknown(String),
}

impl ScmAction {
    pub fn from_id(id: &str) -> Self {
        match id.trim_start_matches('%') {
            "CheckOut" => Self::CheckOut,
            "UndoCheckout" => Self::UndoCheckout,
            "CheckIn" => Self::CheckIn,
            "GetLatest" => Self::GetLatest,
            "AddToSourceControl" => Self::AddToSourceControl,
            "Diff" => Self::Diff,
            "Disconnect" => Self::Disconnect,
            "Reconnect" => Self::Reconnect,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Does invoking this action change state on the instance or in the source-control system?
    ///
    /// Exhaustive on purpose — there is no `_` arm. A variant added to `ScmAction` cannot
    /// compile until it is classified here, and [`crate::tools::mutating_call`] then follows
    /// it with no edit there. Same construction as `DocMode::is_write` and
    /// `gateway_manage::Action::is_write`, and for the same stated reason: a second
    /// `matches!` over action strings in the gate would be a duplicate, and an action
    /// dispatched here but missing from that duplicate would be an UNGATED WRITE.
    pub fn is_write(&self) -> bool {
        match self {
            // Each of these mutates: the first three move the document between checked-out
            // states, GetLatest OVERWRITES the document in IRIS from the repository, and
            // AddToSourceControl puts it under control. Disconnect/Reconnect change the
            // server-side source-control connection for the session — #418 names %Disconnect
            // specifically as reachable today with no gate in front of it.
            Self::CheckOut
            | Self::UndoCheckout
            | Self::CheckIn
            | Self::GetLatest
            | Self::AddToSourceControl
            | Self::Disconnect
            | Self::Reconnect => true,
            // Diff reads two versions and returns the comparison.
            Self::Diff => false,
            // An id this fork does not know is still dispatched: it names a method on the
            // SERVER's own %Studio.SourceControl subclass, which is site-written and can do
            // anything. Unknown is therefore mutating. The affordable error here is refusing
            // a read that turns out to be harmless; the unaffordable one is performing an
            // unnamed write on a connection the caller asked to be read-only.
            Self::Unknown(_) => true,
        }
    }
}

/// `1`/`true`/`yes`, case-insensitively. Three copies of this `matches!` existed, two of them in
/// the two places that gated `CheckIn` — so "is CheckIn allowed" was answered twice, which is the
/// shape #418 §2/§4 was about one layer down.
pub(crate) fn env_truthy(var: &str) -> bool {
    std::env::var(var)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// An action refused unless the operator explicitly enabled it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct OptIn {
    /// The environment variable that enables it. Read by `env_truthy`, reported to STDERR, and
    /// deliberately NOT put in the caller-facing refusal — see `opt_in_refusal`.
    pub var: &'static str,
    /// The error code the refusal carries.
    pub code: &'static str,
}

impl ScmAction {
    /// Is this action refused by default even on a connection that PERMITS writes?
    ///
    /// Distinct from `is_write`, and the distinction is the whole point. `is_write` answers "does
    /// this change something", which the read-only write gate consumes — so before #418 §5,
    /// `%Disconnect` was correctly refused on a read-only connection and ran with no confirmation
    /// on every other one, which is the common case. This answers a different question: is the
    /// blast radius the namespace's source-control CONFIGURATION rather than one document.
    ///
    /// `%Disconnect` / `%Reconnect` switch the namespace's provider integration off or on for the
    /// session. In the CCR hook they run immediately — `UserAction` calls `AfterUserAction` itself
    /// and returns action 0 — so there is not even a dialog to decline.
    ///
    /// Exhaustive with no `_` arm, so a variant added to `ScmAction` cannot compile until someone
    /// has decided whether it needs an opt-in.
    pub(crate) fn opt_in(&self) -> Option<OptIn> {
        match self {
            Self::CheckIn => Some(OptIn {
                var: "IRIS_SCM_ALLOW_CHECKIN",
                code: CHECKIN_BLOCKED,
            }),
            // #418 §5. One variable for the pair, as the issue proposed: they are the same
            // capability in two directions, and an operator who wants one wants the other.
            Self::Disconnect | Self::Reconnect => Some(OptIn {
                var: "IRIS_SCM_ALLOW_DISCONNECT",
                code: SCM_ADMIN_BLOCKED,
            }),
            // These act on ONE document and are already covered by the write gate.
            Self::CheckOut
            | Self::UndoCheckout
            | Self::GetLatest
            | Self::AddToSourceControl
            | Self::Diff => None,
            // An id this fork does not know names a method on the server's own site-written
            // %Studio.SourceControl subclass. It is `is_write`, so a read-only connection refuses
            // it — but NOT opt-in-gated, because calling site hooks is what this tool is for and
            // a blanket default-refuse would break every one of them.
            Self::Unknown(_) => None,
        }
    }
}

/// The refusal for an action whose opt-in is not set, or `None` when it may proceed.
///
/// **The variable name is not in the returned message.** #170 moved exactly this remediation to
/// stderr after measuring that a denial naming its own escape hatch is read by an agent as the next
/// step to take — 4/4 recoveries in that corpus wrote the config first. The operator reads stderr;
/// the caller gets told what was refused and that it is an operator setting. The variable is also
/// on record in the remedy table, which is reviewed prose for humans rather than a runtime hint.
pub(crate) fn opt_in_refusal(action_id: &str) -> Option<(&'static str, String)> {
    let OptIn { var, code } = ScmAction::from_id(action_id).opt_in()?;
    if env_truthy(var) {
        return None;
    }
    tracing::warn!(
        action = %action_id,
        env_var = %var,
        "iris_source_control: refused an action that is disabled by default; set this variable in \
         the server's environment and restart it to allow the action"
    );
    Some((
        code,
        format!(
            "{action_id} is disabled by default on this server. It is an operator setting, not \
             something this session can change; the server's administrator can enable it and \
             restart the server. Until then, perform this step in your own source-control client."
        ),
    ))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScmParams {
    // #112, and `every_tool_advertises_the_parameters_it_reads` enforces it: the valid values
    // belong in the SCHEMA, not only in the INVALID_ACTION message. A guessed action is otherwise
    // a round trip the model could have avoided — and this tool was never advertised before #417,
    // so the gate had nothing to check.
    //
    // `//`, not `///`: schemars ships a doc comment as the advertised `description`, so rationale
    // written here would be sent to every client on every tools/list. That is what
    // `mcp_server_tools_list_returns_interop_profile` caught on the first run of this change.
    /// Action: status, menu, checkout, execute
    #[schemars(extend("enum" = ["status", "menu", "checkout", "execute"]))]
    pub action: String,
    pub document: Option<String>,
    // Deliberately NOT an enum, unlike `action` above. The id names a method on the SERVER's own
    // `%Studio.SourceControl` subclass, which is site-written: the eight this fork knows are the
    // common ones, not the closed set, and an enum here would refuse a hook that exists. The write
    // gate is what makes that safe — an id it does not recognise classifies as a WRITE
    // (`ScmAction::is_write`), so an unknown site hook is gated rather than waved through.
    /// SCM action ID for action=execute (e.g. CheckOut, CheckIn, GetLatest, Diff)
    pub action_id: Option<String>,
    /// Elicitation resume answer
    pub answer: Option<String>,
    pub elicitation_id: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

async fn xecute(
    iris: &IrisConnection,
    client: &reqwest::Client,
    code: &str,
    namespace: &str,
) -> anyhow::Result<String> {
    iris.execute_via_generator(code, namespace, client).await
}

/// Escape a string for safe interpolation into an ObjectScript double-quoted literal.
/// Uses ObjectScript conventions: " → "", \n → $Char(10), \r → $Char(13).
pub(crate) fn os_quote(s: &str) -> String {
    s.replace('"', "\"\"")
        .replace('\n', "$Char(10)")
        .replace('\r', "$Char(13)")
}

/// The sentinel that marks this server's own record in a `UserAction` / `AfterUserAction` response.
///
/// **Why a one-line JSON record behind a printable sentinel, and not #418 §1's suggested
/// `$c(1)_"IAD|"…_$c(1)`.** Measured 2026-10-06 through `execute_via_generator` against
/// intersystemsdc/iris-community:2026.1:
///
/// ```text
/// write "A"_$char(1)_"B"   ->   bytes [65, 10, 66, 10]      i.e. "A\nB\n"
/// ```
///
/// The transport turns a written control character into a NEWLINE — the same measurement
/// `hl7_schema.rs` already records, where a delimiter-separated protocol silently shredded every
/// row. So a `$c(1)`-delimited record would be split by the very mechanism it was meant to survive.
/// `%ToJSON()` instead, because it escapes an embedded newline (`\n`, two characters) and therefore
/// keeps a MULTI-LINE provider message inside ONE findable line — measured:
///
/// ```text
/// set r={} set r.msg="line one"_$char(10)_"line two"  write "IADSCM|"_r.%ToJSON()
///   ->   "IADSCM|{\"msg\":\"line one\\nline two\"}\n"        lines = 1
/// ```
///
/// This is also the house pattern: `status_check_code` already emits one `SCMSTATUS|…` line and
/// `derive_scm_status` finds it among noise, which is why `status` was the one arm #418 §1 did not
/// list as broken.
pub(crate) const SCM_RECORD: &str = "IADSCM|";

/// The record itself. `kind` is what the snippet was answering, so a caller never has to infer it
/// from which fields happen to be populated.
#[derive(Debug, Deserialize, PartialEq, Default)]
pub(crate) struct ScmRecord {
    pub kind: String,
    /// The action code, as IRIS gave it. A `Value` rather than a `u8` ON PURPOSE: coercing it in
    /// ObjectScript with `+action` would turn anything non-numeric into 0, and **0 is the success
    /// code** — which is exactly the `unwrap_or(0)` defect #302 and #418 §2 are about, moved into
    /// the snippet where no Rust test could see it. A value that is not a small integer is an
    /// unparseable answer, not a go-ahead.
    #[serde(default)]
    pub code: serde_json::Value,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub target: String,
    /// `AfterUserAction` only: did the action complete?
    #[serde(default)]
    pub ok: u8,
    /// `AfterUserAction` only: the whole `%Status` chain, CRLF-joined by IRIS.
    #[serde(default)]
    pub err: String,
}

impl ScmRecord {
    /// The action code if it really is one.
    pub(crate) fn action_code(&self) -> Option<u8> {
        self.code.as_u64().filter(|n| *n <= 255).map(|n| n as u8)
    }

    /// What to show a human: the provider's message, or its dialog target when the message is empty.
    ///
    /// #418 §3 was that the snippet sent `msg` and dropped `target`, where CCR puts the check-out
    /// prompt and the TEST/UAT/LIVE "revert this afterwards" warning. The old repair was a
    /// `$select(msg'="":msg,target'="":target,1:"")` in ObjectScript, which loses WHICH one it
    /// returned. Both fields now cross the wire and the choice is made here, so `NeedsUi` can hand
    /// back the address specifically.
    pub(crate) fn display(&self) -> &str {
        if self.msg.is_empty() {
            &self.target
        } else {
            &self.msg
        }
    }
}

/// `json!(…).pipe_hook(&h)` — the same thing as `with_hook_output`, spelled so the big `json!`
/// blocks in the two arms keep reading top-to-bottom instead of being wrapped in a call.
trait PipeHook {
    fn pipe_hook(self, hook_output: &str) -> serde_json::Value;
}
impl PipeHook for serde_json::Value {
    fn pipe_hook(self, hook_output: &str) -> serde_json::Value {
        with_hook_output(self, hook_output)
    }
}

/// Attach the hook's own output to a response, when there was any.
///
/// #418 §1 asks for this to be RETURNED rather than discarded or misread. It is omitted entirely
/// when empty, so a quiet provider does not add a null field to every answer.
fn with_hook_output(mut v: serde_json::Value, hook_output: &str) -> serde_json::Value {
    if !hook_output.is_empty() {
        v["hook_output"] = serde_json::Value::String(hook_output.to_string());
    }
    v
}

/// Everything in `out` that is not this server's record: the hook's own chatter.
///
/// #418 §1 asks for this to be RETURNED rather than discarded or misread. CCR writes a `NOTICE:`
/// line when no Perforce user is defined and a `CMD: p4 …` echo whenever it shells out, and both
/// used to be fed to a parser that treated them as the answer.
fn hook_chatter(out: &str, record_line: Option<&str>) -> String {
    out.lines()
        .filter(|l| Some(*l) != record_line && !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a `UserAction` / `AfterUserAction` response actually said.
///
/// ## The history this shape carries
///
/// #302: the code was read with `…parse::<u8>().ok().unwrap_or(0)`, and **0 is the success code**.
/// So `<PROTECT>`, `ERROR #5865`, a provider `NOTICE` and a mistyped sentinel were all read as "the
/// action completed", and the message was lost with them.
///
/// #418 §1: the code was then read from `out.lines().next()`, so a hook that wrote anything before
/// its own record — which CCR does — made the first line the NOTICE rather than the answer, and a
/// multi-line message was truncated at its first newline, dropping the part that says why.
///
/// Both are now structural rather than defended against: the snippet writes one labelled record and
/// this finds it wherever it landed. There is no longer a first line to be wrong about.
#[derive(Debug, PartialEq)]
enum ActionMsg {
    /// A record, parsed.
    Record(ScmRecord, String),
    /// The SCM session could not be created — `scm_init_prefix` writes this bare sentinel and
    /// quits, so there is no record to find.
    Unavailable,
    /// No output at all.
    Empty,
    /// Output with no record in it: IRIS error text, a `<PROTECT>`, a provider refusal that killed
    /// the snippet before it could write. The full text is kept, because it is the only thing that
    /// says what IRIS objected to and `scm_error_code` classifies on content.
    Unparseable(String),
}

fn parse_action_msg(out: &str) -> ActionMsg {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return ActionMsg::Empty;
    }
    // The record, wherever it is. Not the first line — that is the defect.
    if let Some(line) = out.lines().find(|l| l.trim_start().starts_with(SCM_RECORD)) {
        let json = &line.trim_start()[SCM_RECORD.len()..];
        if let Ok(rec) = serde_json::from_str::<ScmRecord>(json) {
            return ActionMsg::Record(rec, hook_chatter(out, Some(line)));
        }
        // A record we cannot read is NOT a success. Fall through to Unparseable with everything,
        // so the classifier sees the malformed record too.
    }
    if trimmed.lines().any(|l| l.trim() == "SCM_UNAVAILABLE") {
        return ActionMsg::Unavailable;
    }
    ActionMsg::Unparseable(trimmed.to_string())
}

/// Did an `AfterUserAction` response mean "completed"?
///
/// `Ok(hook_output)` when it did. `Err((error_code, detail))` otherwise.
///
/// #418 §1's third bullet: this used to be "any output means failure, empty means success", because
/// `after_user_action_code` ended with `write $system.Status.GetErrorText(sc)` and GetErrorText
/// returns `""` for an OK status. That convention was the exact OPPOSITE of the `UserAction`
/// generator's — which always wrote at least `0|` — and the two were pinned against each other by
/// `the_two_generators_disagree_about_empty` precisely because neither could be reasoned about
/// alone. A hook that wrote one line of chatter therefore turned a successful checkout into
/// `SCM_CHECKOUT_FAILED`.
///
/// There is now an explicit `ok` field and the asymmetry is gone: both generators answer the same
/// way, and silence from either means the snippet never ran.
pub(crate) fn after_user_action_outcome(out: &str) -> Result<String, (&'static str, String)> {
    match parse_action_msg(out) {
        ActionMsg::Record(rec, chatter) if rec.kind == "after" => {
            if rec.ok == 1 {
                Ok(chatter)
            } else {
                let detail = if rec.err.is_empty() {
                    "Source control reported the action as not completed, with no reason.".to_string()
                } else {
                    rec.err
                };
                Err((SCM_CHECKOUT_FAILED, detail))
            }
        }
        // A record of the wrong kind means the snippet ran a different path than we asked for.
        ActionMsg::Record(rec, _) => Err((
            SCM_ERROR,
            format!(
                "Expected an AfterUserAction record and got kind '{}' — the finalizer did not run.",
                rec.kind
            ),
        )),
        // Unavailable is NOT success. The previous reading (`aout != "SCM_UNAVAILABLE"`) let it fall
        // through to a committed checkout, which is the same "failure answered as a fact" shape.
        ActionMsg::Unavailable => Err((
            "SCM_UNAVAILABLE",
            "The source-control session could not be created to finalize the action, so whether it \
             completed is unknown and nothing may be assumed about it."
                .to_string(),
        )),
        ActionMsg::Empty => Err((SCM_NO_OUTPUT, EMPTY_OUTPUT_MSG.to_string())),
        ActionMsg::Unparseable(raw) => Err((scm_error_code(&raw), raw)),
    }
}

/// The answer to a `UserAction`, for all three call sites.
#[derive(Debug, PartialEq)]
pub(crate) struct ActionAnswer {
    pub code: u8,
    pub record: ScmRecord,
    /// The hook's own output, so a caller can see what the provider said instead of it being
    /// discarded (#418 §1) — carried into the response rather than dropped here.
    pub hook_output: String,
}

/// Decide what a `UserAction` response means, for every call site.
///
/// `Err((error_code, detail))` is the envelope the caller must return instead — the caller does
/// nothing but hand it to `err_json`.
///
/// **Why this is a function.** The `checkout` and `execute` arms used to carry an identical 15-line
/// copy of this decision, which is the shape #302's first half already got wrong once: a repair
/// applied to one copy leaves the other looking more trustworthy than it is. It also made the
/// decision untestable — the mapping sat inside an async handler behind an HTTP round trip, so a
/// mutation that reverted `Empty` to success passed the whole suite.
pub(crate) fn user_action_outcome(out: &str) -> Result<ActionAnswer, (&'static str, String)> {
    match parse_action_msg(out) {
        // The probe writes this kind when SourceControlCreate produced no session. It must reach
        // the caller as SCM_UNAVAILABLE and not as "expected an action code", which is what the
        // generic arm below would have said about a record with no `code` field.
        ActionMsg::Record(rec, _) if rec.kind == "unavailable" => Err((
            "SCM_UNAVAILABLE",
            "Source control session could not be initialized".to_string(),
        )),
        ActionMsg::Record(rec, hook_output) => match rec.action_code() {
            Some(code) => Ok(ActionAnswer {
                code,
                record: rec,
                hook_output,
            }),
            // A record whose code is not a small integer. Not a go-ahead — see `ScmRecord::code`.
            None => Err((
                SCM_ERROR,
                format!(
                    "Source control returned '{}' where an action code was expected, so what it \
                     did is unknown.",
                    rec.code
                ),
            )),
        },
        ActionMsg::Unavailable => Err((
            "SCM_UNAVAILABLE",
            "Source control session could not be initialized".to_string(),
        )),
        ActionMsg::Empty => Err((SCM_NO_OUTPUT, EMPTY_OUTPUT_MSG.to_string())),
        ActionMsg::Unparseable(raw) => Err((scm_error_code(&raw), raw)),
    }
}

/// What the pre-write probe was answering, when its record says so.
///
/// `iris_doc`'s probe asks a question the two tool arms never do — "is there a provider here at
/// all, and do we already hold this document?" — so it has `kind`s of its own on the shared record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeKind {
    /// `SourceControlClassGet()` returned "": nothing to check out, and nothing to remember.
    NoSourceControl,
    /// The MenuItems pre-step saw `%UndoCheckout` offered, so this session already holds it.
    WeHoldIt,
    /// A `UserAction` answer, to be classified by `user_action_outcome` like any other.
    Action,
}

/// The probe's record kind, or `None` when there is no readable record.
pub(crate) fn parse_probe_record(out: &str) -> Option<ProbeKind> {
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with(SCM_RECORD))?;
    let rec: ScmRecord = serde_json::from_str(&line.trim_start()[SCM_RECORD.len()..]).ok()?;
    match rec.kind.as_str() {
        "none" => Some(ProbeKind::NoSourceControl),
        "hold" => Some(ProbeKind::WeHoldIt),
        "action" => Some(ProbeKind::Action),
        // `unavailable` deliberately falls through to the shared classifier, which refuses it —
        // this must never read as "there is no source control".
        _ => None,
    }
}

/// What a `UserAction` action code means to a caller that has no window to open.
///
/// **Why this exists.** Three sites read the same code and answered differently. Measured on the
/// tree this replaces, the set of codes each one recognised was:
///
/// | site | recognised | everything else |
/// |---|---|---|
/// | `checkout` arm | `0` | presented as a **yes/no question** |
/// | `execute` arm | `0`, `1`, `7` | `SCM_ERROR "Unexpected action code N"` |
/// | `iris_doc`'s pre-write probe | `0`, `1`, `6` | **proceed** — the write went ahead ungated |
///
/// The intersection is `{0}`. So for every code but one, the three sites disagreed, and the
/// disagreement was not visible at any of them: each looked locally reasonable. #418 names two of
/// the consequences (§2, §4) and the third — a CCR login page, action 2, read as "proceed" — is the
/// one that writes a document that is not checked out.
///
/// A `u8` has no exhaustiveness, so the `_` arm below is unavoidable. The tripwire is moved to the
/// CONSUMERS instead: every site matches on this enum with no catch-all, so a variant added here
/// does not compile until all three have decided what it means. `scm_action_codes_mean_one_thing`
/// asserts that no site goes back to matching the integers.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CodeMeaning {
    /// 0 — no dialog is needed. NOT "it happened": `AfterUserAction` still has to run, which is
    /// the false positive #302 fixed in the `checkout` arm.
    NoDialog,
    /// 1 — a yes/no confirmation, and the only code that is one.
    Confirm,
    /// 6 — source control declined by policy. A real answer, not a malfunction.
    Declined,
    /// 7 — a text prompt. Not a confirmation: answering a prompt with "yes" sends the string
    /// "yes" as the value, which is why routing 7 to a yes/no dialog is wrong rather than merely
    /// imprecise.
    Prompt,
    /// 2 (open a CSP page) or 3 (open a URL). CCR returns these for `%CheckIn`, `CommitChanges`,
    /// `CCRControls`, `Diff`, `TakeOwnership` and `CCRFileHistory`, and it rewrites its own
    /// password prompt from 7 into 2 — so this is the code a missing Perforce credential arrives
    /// under. The address is in `target`, which is why the snippets must send it (§3).
    NeedsUi,
    /// Any other code. Unknown to this fork, so not driveable from here.
    Undriveable,
}

impl CodeMeaning {
    pub(crate) fn of(code: u8) -> Self {
        match code {
            0 => Self::NoDialog,
            1 => Self::Confirm,
            2 | 3 => Self::NeedsUi,
            6 => Self::Declined,
            7 => Self::Prompt,
            _ => Self::Undriveable,
        }
    }
}

/// The refusal for a code this path cannot drive. Shared so the three sites word it once.
///
/// Returns `(error_code, detail)`. `msg` is whatever the provider put in `msg` or `target` — for
/// `NeedsUi` that is the page or URL to open, which is the only actionable part of the answer.
pub(crate) fn undriveable_refusal(
    meaning: CodeMeaning,
    code: u8,
    msg: &str,
    doc: &str,
) -> (&'static str, String) {
    let where_ = if msg.is_empty() {
        String::new()
    } else {
        format!(" The provider said: {msg}")
    };
    match meaning {
        CodeMeaning::NeedsUi => (
            SCM_NEEDS_UI,
            format!(
                "Source control answered with action {code}, which means the action has to be                  completed in the provider's own web UI — this tool cannot open it, and nothing                  was changed.{where_}"
            ),
        ),
        CodeMeaning::Prompt => (
            SCM_NEEDS_INPUT,
            format!(
                "Source control is asking for a typed value before it will act on {doc}, and this                  path has no way to supply one — nothing was changed.{where_}"
            ),
        ),
        // Reached only from a site that has already handled the driveable codes.
        CodeMeaning::Undriveable
        | CodeMeaning::NoDialog
        | CodeMeaning::Confirm
        | CodeMeaning::Declined => (
            SCM_ERROR,
            format!("Unexpected action code {code} from UserAction.{where_}"),
        ),
    }
}

pub async fn handle_iris_source_control(
    iris: &IrisConnection,
    client: &reqwest::Client,
    p: ScmParams,
    elicitation_store: &ElicitationStore,
    checkout_cache: &crate::elicitation::CheckoutCache,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let raw_doc = p.document.as_deref().unwrap_or("");
    let doc_owned;
    let raw_lower = raw_doc.to_ascii_lowercase();
    let doc = if !raw_doc.is_empty()
        && !raw_lower.ends_with(".cls")
        && !raw_lower.ends_with(".mac")
        && !raw_lower.ends_with(".inc")
        && !raw_lower.ends_with(".int")
    {
        doc_owned = format!("{}.cls", raw_doc);
        doc_owned.as_str()
    } else {
        raw_doc
    };
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let ns = &namespace;

    // Handle elicitation resume
    if let (Some(eid), Some(answer)) = (&p.elicitation_id, &p.answer) {
        // #305: expired and never-existed are different facts and the caller can act on the
        // difference — retry the dialog, versus check the id you sent.
        let pending = match elicitation_store.lookup(eid) {
            crate::elicitation::LookupResult::Found(p) => p,
            crate::elicitation::LookupResult::Expired => return err_json(
                "ELICITATION_EXPIRED",
                "This elicitation has expired — they are held for 5 minutes. Re-run the action \
                     to get a new dialog.",
            ),
            crate::elicitation::LookupResult::NotFound => {
                return err_json(
                    "ELICITATION_NOT_FOUND",
                    "No elicitation with that id. Check the `elicitation_id` you sent; note the \
                     store is in-memory, so a server restart discards pending dialogs.",
                )
            }
        };
        elicitation_store.clear(eid);
        let action_id = pending.scm_action_id.as_deref().unwrap_or("");
        let after_code = after_user_action_code(
            action_id,
            &pending.document,
            answer,
            &iris.username,
            &iris.password,
        );
        let out = match xecute(iris, client, &after_code, &pending.namespace).await {
            Ok(o) => o,
            Err(e) => {
                let msg = e.to_string();
                let (ec, emsg) = if msg == "DOCKER_REQUIRED" {
                    (
                        "DOCKER_REQUIRED",
                        "SCM operations require docker exec. Set IRIS_CONTAINER=<container_name>."
                            .to_string(),
                    )
                } else {
                    ("SCM_UNAVAILABLE", msg)
                };
                return err_json(ec, &emsg);
            }
        };
        // #418 §1: the one path in this module that claims success. It used to test the output
        // for EMPTINESS, so a hook that wrote a single line of chatter — CCR writes `CMD: p4 …`
        // whenever it shells out — reported failure for a completed action, and a blank first
        // line with an error on the second reported SUCCESS. There is an explicit `ok` field now.
        let out = out.trim().to_string();
        return match after_user_action_outcome(&out) {
            Ok(hook_output) => {
                // Any resumed SCM action (checkout/undo/checkin/disconnect) changes checkout state,
                // so drop the cached entry — the next write re-probes and re-caches if still ours.
                checkout_cache.invalidate(&pending.namespace, &pending.document);
                let mut resp = serde_json::json!({
                    "success": true, "document": pending.document, "action_id": action_id
                });
                if !hook_output.is_empty() {
                    resp["hook_output"] = serde_json::Value::String(hook_output);
                }
                ok_json(resp)
            }
            Err((code, detail)) => err_json(code, &detail),
        };
    }

    match p.action.as_str() {
        "status" => {
            let check_code = status_check_code(doc, &iris.username, &iris.password);
            let raw = match xecute(iris, client, &check_code, ns).await {
                Ok(o) => o,
                Err(e) => {
                    // A transport/exec failure must NOT be reported as "editable" — that is the
                    // very inconsistency this path used to have. Surface it honestly.
                    return err_json(scm_error_code(&e.to_string()), &e.to_string());
                }
            };
            // The executor may append "ERROR($ZERROR): …" on later lines — find the SCMSTATUS
            // sentinel line rather than assuming it is the first one.
            let parsed = raw.lines().find_map(parse_scm_status_line);
            let Some((is_in_sc, has_co, has_undo, has_add, owner)) = parsed else {
                // No SCMSTATUS sentinel. Before giving up, try the provider's native
                // "checked out by user '<name>'" notice, which short-circuits the probe (often
                // with a <PROTECT>) before the sentinel is written. That notice still tells us the
                // document is controlled and locked by another user — report that instead of an
                // opaque SCM_UNAVAILABLE.
                if let Some((other_owner, ts)) = parse_checked_out_by(&raw) {
                    let checked_out_by_me = other_owner.eq_ignore_ascii_case(&iris.username);
                    let mut resp = serde_json::json!({
                        "success": true,
                        "controlled": true,
                        "editable": checked_out_by_me,
                        "locked": !checked_out_by_me,
                        "checked_out_by_me": checked_out_by_me,
                        "owner": other_owner,
                    });
                    if let Some(ts) = ts {
                        resp["checked_out_at"] = serde_json::Value::String(ts);
                    }
                    return ok_json(resp);
                }
                // Echo the raw IRIS output (truncated) so the actual failure — a <PROTECT>, an
                // authentication banner, an empty body — is diagnosable instead of being flattened
                // into an opaque "no status signal".
                let raw_trunc: String = raw.trim().chars().take(600).collect();
                return crate::tools::envelope::fail_with(
                    "SCM_UNAVAILABLE",
                    "Could not determine source control status (no SCMSTATUS sentinel in IRIS output)",
                    serde_json::json!({ "raw_output": raw_trunc }),
                );
            };
            let status =
                derive_scm_status(is_in_sc, has_co, has_undo, has_add, &owner, &iris.username);
            let Some(status) = status else {
                return err_json(
                    "SCM_UNAVAILABLE",
                    "Source control status is indeterminate for this document",
                );
            };
            ok_json(serde_json::json!({
                "success": true,
                "controlled": status.controlled,
                "editable": status.editable,
                "locked": status.locked,
                "checked_out_by_me": status.checked_out_by_me,
                "owner": status.owner,
            }))
        }

        "menu" => {
            let code = menu_all_items_code(doc, &iris.username, &iris.password);
            let raw = xecute(iris, client, &code, ns).await.unwrap_or_default();
            let mut actions = vec![];
            for line in raw.lines() {
                let line = line.trim();
                if line == "SCM_UNAVAILABLE" || line.is_empty() || line.starts_with("ERROR") {
                    continue;
                }
                // format: "name|enabled"
                let mut parts = line.splitn(2, '|');
                let name = parts.next().unwrap_or("").trim();
                let enabled: u8 = parts
                    .next()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                // #418 §5: offer exactly what `execute` would accept. This read
                // IRIS_SCM_ALLOW_CHECKIN itself and compared against ScmAction::CheckIn by name,
                // so the menu and the execute arm each decided "is this allowed" separately —
                // and adding the two administrative actions to both would have made four copies
                // of one policy. Both now ask `opt_in_refusal`.
                if enabled == 1 && !name.is_empty() && opt_in_refusal(name).is_none() {
                    actions.push(serde_json::json!({"id": name, "label": name, "enabled": true}));
                }
            }
            ok_json(serde_json::json!({"success": true, "document": doc, "actions": actions}))
        }

        "checkout" => {
            let code = user_action_code("%CheckOut", doc, &iris.username, &iris.password);
            let raw = match xecute(iris, client, &code, ns).await {
                Ok(o) => o,
                Err(e) => return err_json(scm_error_code(&e.to_string()), &e.to_string()),
            };
            // #418 §1: the record is found wherever the hook's own output put it, so there is no
            // longer a first line to be wrong about.
            let out = raw.trim();
            let answer = match user_action_outcome(out) {
                Ok(v) => v,
                Err((code, detail)) => return err_json(code, &detail),
            };
            let action_code = answer.code;
            let msg = answer.record.display();
            let hook_output = answer.hook_output.clone();

            // #418 §4: dispatch on what the code MEANS, not on whether it is zero. This arm
            // used to read `if action_code == 0 { … }` and then fall through to a yes/no
            // dialog, so EVERY non-zero code became a yes/no question — including action 2,
            // which is how CCR reports "your Perforce credentials are missing, here is the
            // login page". The user was asked to answer yes or no about a URL.
            let meaning = CodeMeaning::of(action_code);
            match meaning {
                CodeMeaning::NoDialog => {
                    // action=0 means UserAction wants no confirmation dialog — but the checkout
                    // is NOT actually committed until AfterUserAction runs. Reporting success
                    // here on UserAction alone was a false positive: the item looked checked out
                    // but a later write failed with ERROR #5865. Finalize with AfterUserAction so
                    // the checkout genuinely persists server-side before we claim success.
                    let after_code = after_user_action_code(
                        "%CheckOut",
                        doc,
                        "yes",
                        &iris.username,
                        &iris.password,
                    );
                    match xecute(iris, client, &after_code, ns).await {
                        Ok(o) => {
                            // #342: the FULL output, not `lines().next()`. This generator ends with
                            // `write $system.Status.GetErrorText(sc)`, which returns the whole %Status
                            // chain CRLF-joined — measured on 2026.1, a 2-error chain came back as
                            // "ERROR #5001: first cause\r\nERROR #5001: second cause". Reporting line 1
                            // discarded the rest, and the specific cause of a checkout failure is
                            // frequently the later element.
                            //
                            // #418 §1: an explicit `ok` field, so this no longer depends on the
                            // generators having opposite conventions about empty — and
                            // `SCM_UNAVAILABLE` is no longer waved through as success, which it
                            // was: `aout != "SCM_UNAVAILABLE"` let a session that could not be
                            // created fall through to a committed checkout.
                            let aout = o.trim();
                            if let Err((code, detail)) = after_user_action_outcome(aout) {
                                return err_json(code, &detail);
                            }
                        }
                        Err(e) => return err_json(scm_error_code(&e.to_string()), &e.to_string()),
                    }
                    // Checkout committed — cache it so a following iris_doc write skips the probe.
                    checkout_cache.mark(ns, doc);
                    ok_json(with_hook_output(
                        serde_json::json!({"success": true, "document": doc, "editable": true}),
                        &hook_output,
                    ))
                }
                CodeMeaning::Confirm => {
                    let eid = elicitation_store.insert(
                        doc,
                        ElicitationAction::ScmExecute,
                        None,
                        Some("%CheckOut".to_string()),
                        ns.clone(),
                    );
                    ok_json(serde_json::json!({
                        "success": false,
                        "elicitation_required": true,
                        "elicitation_id": eid,
                        "message": if msg.is_empty() { format!("Check out {} ?", doc) } else { msg.to_string() },
                        "options": ["yes", "no"],
                    })
                    .pipe_hook(&hook_output))
                }
                // A typed value, not a yes/no. The `execute` arm already answered this shape
                // correctly; `checkout` is `execute %CheckOut` by another name, so it answers
                // it the same way rather than turning a prompt into a confirmation.
                CodeMeaning::Prompt => {
                    let eid = elicitation_store.insert(
                        doc,
                        ElicitationAction::ScmExecute,
                        None,
                        Some("%CheckOut".to_string()),
                        ns.clone(),
                    );
                    ok_json(serde_json::json!({
                        "success": false,
                        "elicitation_required": true,
                        "elicitation_id": eid,
                        "message": if msg.is_empty() { format!("Enter value for %CheckOut on {}:", doc) } else { msg.to_string() },
                        "input_type": "text",
                    })
                    .pipe_hook(&hook_output))
                }
                CodeMeaning::Declined => err_json(
                    "SCM_REJECTED",
                    &format!("Source control declined to check out {doc}: {msg}"),
                ),
                CodeMeaning::NeedsUi | CodeMeaning::Undriveable => {
                    let (code, detail) = undriveable_refusal(meaning, action_code, msg, doc);
                    err_json(code, &detail)
                }
            }
        }

        "execute" => {
            let action_id = p.action_id.as_deref().unwrap_or("");
            if let Some((code, detail)) = opt_in_refusal(action_id) {
                return err_json(code, &detail);
            }
            let code = user_action_code(action_id, doc, &iris.username, &iris.password);
            let raw = match xecute(iris, client, &code, ns).await {
                Ok(o) => o,
                Err(e) => return err_json(scm_error_code(&e.to_string()), &e.to_string()),
            };
            // #418 §1: the record is found wherever the hook's own output put it, so there is no
            // longer a first line to be wrong about.
            let out = raw.trim();
            let answer = match user_action_outcome(out) {
                Ok(v) => v,
                Err((code, detail)) => return err_json(code, &detail),
            };
            let action_code = answer.code;
            let msg = answer.record.display();
            let hook_output = answer.hook_output.clone();

            // #418 §4: the same CodeMeaning table the `checkout` arm and `iris_doc`'s
            // pre-write probe read. This arm answered `SCM_ERROR "Unexpected action code 2"`
            // for every code it did not list — including 6, which is not unexpected at all
            // (source control declined), and 2/3, which name a page the caller can open.
            let meaning = CodeMeaning::of(action_code);
            match meaning {
                CodeMeaning::NoDialog => {
                    // A completed execute (undo checkout / checkin / disconnect / …) changes
                    // checkout state — drop any cached entry so the next write re-probes.
                    checkout_cache.invalidate(ns, doc);
                    ok_json(with_hook_output(
                        serde_json::json!({"success": true, "document": doc, "action_id": action_id}),
                        &hook_output,
                    ))
                }
                CodeMeaning::Confirm => {
                    // Yes/No confirmation
                    let eid = elicitation_store.insert(
                        doc,
                        ElicitationAction::ScmExecute,
                        None,
                        Some(action_id.to_string()),
                        ns.clone(),
                    );
                    ok_json(serde_json::json!({
                        "success": false, "elicitation_required": true, "elicitation_id": eid,
                        "message": if msg.is_empty() { format!("Execute {} on {}?", action_id, doc) } else { msg.to_string() },
                        "options": ["yes", "no"],
                    })
                    .pipe_hook(&hook_output))
                }
                CodeMeaning::Prompt => {
                    // Text prompt
                    let eid = elicitation_store.insert(
                        doc,
                        ElicitationAction::ScmExecute,
                        None,
                        Some(action_id.to_string()),
                        ns.clone(),
                    );
                    ok_json(serde_json::json!({
                        "success": false, "elicitation_required": true, "elicitation_id": eid,
                        "message": if msg.is_empty() { format!("Enter value for {}:", action_id) } else { msg.to_string() },
                        "input_type": "text",
                    })
                    .pipe_hook(&hook_output))
                }
                CodeMeaning::Declined => err_json(
                    "SCM_REJECTED",
                    &format!("Source control declined {action_id} on {doc}: {msg}"),
                ),
                CodeMeaning::NeedsUi | CodeMeaning::Undriveable => {
                    let (code, detail) = undriveable_refusal(meaning, action_code, msg, doc);
                    err_json(code, &detail)
                }
            }
        }

        other => err_json(
            "INVALID_PARAM",
            &format!(
                "Unknown action='{}'. Use: status, menu, checkout, execute",
                other
            ),
        ),
    }
}

/// Build the ObjectScript snippet that determines SCM status for a document.
/// Uses GetStatus for controlled/uncontrolled, then MenuItems to deduce editable/owner
/// since many SCM implementations don't populate GetStatus's editable/owner fields.
///
/// Emits one structured, pipe-delimited line so the caller (`derive_scm_status`) can combine
/// every available signal instead of relying on a single heuristic:
///   `SCMSTATUS|<isErr>|<isInSC>|<editable>|<hasCheckOut>|<hasUndoCheckout>|<hasAddToSC>|<owner>`
/// where the six middle fields are 0/1 and `owner` is the GetStatus owner (may be empty).
/// The `SCMSTATUS` sentinel lets the caller distinguish a real result from a transport/error
/// line (the executor may append `ERROR($ZERROR): …` on subsequent lines).
fn status_check_code(doc: &str, username: &str, password: &str) -> String {
    let doc_q = os_quote(doc);
    let user_q = os_quote(username);
    let pass_q = os_quote(password);
    // Each risky step is wrapped in TRY/CATCH so a runtime error (SourceControlCreate failing,
    // GetStatus <PROTECT>, an SCM provider that has no MenuItems query, …) can never abort the
    // job before the SCMSTATUS sentinel is written. Without this, any partial failure produced
    // "no status signal returned" instead of a usable (possibly indeterminate) status.
    format!(
        "set isErr=0,isInSC=0,editable=0,isCheckedOut=0,owner=\"\" \
         set hasCheckOut=0,hasUndoCheckout=0,hasAddToSC=0 \
         try {{ set sc=##class(%Studio.SourceControl.Interface).SourceControlCreate(\"{user_q}\",\"{pass_q}\",.created,.flags,.outuser) }} catch {{ set isErr=1 }} \
         try {{ set sc=##class(%Studio.SourceControl.Interface).GetStatus(\"{doc_q}\",.isInSC,.editable,.isCheckedOut,.owner) if $system.Status.IsError(sc) {{ set isErr=1 }} }} catch {{ set isErr=1 }} \
         try {{ \
           set rset=##class(%ResultSet).%New(\"%Studio.SourceControl.Interface:MenuItems\") \
           set sc=rset.Execute(\"%SourceMenu\",\"{doc_q}\",\"\") \
           while rset.Next() {{ \
             set itemName=rset.GetData(1),itemEnabled=rset.GetData(2) \
             if itemEnabled&&(itemName=\"%CheckOut\") {{ set hasCheckOut=1 }} \
             if itemEnabled&&(itemName=\"%UndoCheckout\") {{ set hasUndoCheckout=1 }} \
             if itemEnabled&&(itemName=\"%AddToSourceControl\") {{ set hasAddToSC=1 }} \
           }} \
         }} catch {{ set isErr=1 }} \
         write \"SCMSTATUS|\"_isErr_\"|\"_isInSC_\"|\"_editable_\"|\"_hasCheckOut_\"|\"_hasUndoCheckout_\"|\"_hasAddToSC_\"|\"_owner"
    )
}

/// Resolved SCM status for a document, derived from the combined `status_check_code` signals.
#[derive(Debug, PartialEq, Eq)]
pub struct ScmStatus {
    /// The document is under source control.
    pub controlled: bool,
    /// The document can be written right now (uncontrolled, or checked out by the current user).
    pub editable: bool,
    /// The document is locked by someone else (controlled, checked out, not by us).
    pub locked: bool,
    /// The current user holds the checkout.
    pub checked_out_by_me: bool,
    /// The checkout owner, when known (the current user if we hold it, else GetStatus owner).
    pub owner: Option<String>,
}

/// Combine every SCM signal into a coherent status.
///
/// Decision logic:
/// - With zero signal (no in-SC flag, no menu items, no owner) there is no SCM configured for
///   this namespace/document → uncontrolled, freely editable. (This fork deliberately keeps
///   the pre-aa937dd behavior: dev instances without any source control are the common case,
///   and reporting SCM_UNAVAILABLE for every one of them was the real regression.)
/// - `uncontrolled` ⇔ the menu offers `%AddToSourceControl` (you can only add a document that
///   is not yet under source control). Keyed on this positive signal rather than on GetStatus's
///   `isInSC`, which some providers leave unpopulated.
/// - checked-out-by-me ⇔ `%UndoCheckout` is enabled (only the holder can undo their checkout).
/// - available-to-checkout ⇔ controlled and `%CheckOut` is enabled (free to take, not locked).
/// - locked-by-other ⇔ controlled and neither CheckOut nor UndoCheckout is available
///   (someone else holds it).
fn derive_scm_status(
    is_in_sc: bool,
    has_checkout: bool,
    has_undo_checkout: bool,
    has_add_to_sc: bool,
    owner: &str,
    current_user: &str,
) -> Option<ScmStatus> {
    let owner_opt = Some(owner.trim().to_string()).filter(|s| !s.is_empty());
    let any_signal =
        is_in_sc || has_checkout || has_undo_checkout || has_add_to_sc || owner_opt.is_some();
    // No signal at all → no SCM configured, document is freely editable.
    // (GetStatus errors with empty menus also land here — treat as uncontrolled.)
    if !any_signal {
        return Some(ScmStatus {
            controlled: false,
            editable: true,
            locked: false,
            checked_out_by_me: false,
            owner: None,
        });
    }

    // A document is uncontrolled iff we are offered the action to add it to source control.
    if has_add_to_sc {
        return Some(ScmStatus {
            controlled: false,
            editable: true,
            locked: false,
            checked_out_by_me: false,
            owner: None,
        });
    }

    if has_undo_checkout {
        // We hold the checkout → writable by us.
        return Some(ScmStatus {
            controlled: true,
            editable: true,
            locked: false,
            checked_out_by_me: true,
            owner: owner_opt.or_else(|| Some(current_user.to_string())),
        });
    }

    if has_checkout {
        // Controlled and free to check out — not currently editable, but not locked by anyone.
        return Some(ScmStatus {
            controlled: true,
            editable: false,
            locked: false,
            checked_out_by_me: false,
            owner: owner_opt,
        });
    }

    // Controlled, can neither check out nor undo → locked by another user.
    Some(ScmStatus {
        controlled: true,
        editable: false,
        locked: true,
        checked_out_by_me: false,
        owner: owner_opt,
    })
}

/// Parse the `SCMSTATUS|…` line emitted by `status_check_code` into
/// `(isInSC, hasCheckOut, hasUndoCheckout, hasAddToSC, owner)`. Returns `None` if the line is
/// missing the sentinel or has the wrong arity (e.g. a transport error was returned instead).
///
/// The leading `isErr` and the `editable` fields are consumed but not returned: `isErr` only
/// gates nothing now (absence of signal is the real "unknown" test), and GetStatus's `editable`
/// is advisory — the menu action signals are authoritative.
fn parse_scm_status_line(line: &str) -> Option<(bool, bool, bool, bool, String)> {
    // SCMSTATUS|isErr|isInSC|editable|hasCheckOut|hasUndoCheckout|hasAddToSC|owner
    let mut parts = line.trim().splitn(8, '|');
    if parts.next()? != "SCMSTATUS" {
        return None;
    }
    let flag = |p: Option<&str>| p.map(|s| s.trim() != "0" && !s.trim().is_empty());
    let _is_err = flag(parts.next())?;
    let is_in_sc = flag(parts.next())?;
    let _editable = flag(parts.next())?;
    let has_checkout = flag(parts.next())?;
    let has_undo_checkout = flag(parts.next())?;
    let has_add_to_sc = flag(parts.next())?;
    let owner = parts.next().unwrap_or("").trim().to_string();
    Some((
        is_in_sc,
        has_checkout,
        has_undo_checkout,
        has_add_to_sc,
        owner,
    ))
}

/// Fallback owner detection from the SCM provider's native `checked out by user '<name>'` notice.
///
/// Some source-control providers emit a native `NOTICE: … is currently checked out by user
/// 'xxx', and was last updated at 2026-07-07 12:34:56` message (often followed by a `<PROTECT>`)
/// that short-circuits `status_check_code` before the `SCMSTATUS|` sentinel is ever written. In
/// that case `parse_scm_status_line` finds nothing, yet the raw output already tells us the
/// document is controlled and locked by another user — so we scrape it here instead of reporting
/// an opaque `SCM_UNAVAILABLE`.
///
/// The regex is tolerant: the message may be repeated (the probe loops) and truncated mid-line
/// (before `updated at …`). We take the first match, ignore repetitions, and treat the timestamp
/// as optional. Returns `(owner, Option<timestamp>)`.
fn parse_checked_out_by(raw: &str) -> Option<(String, Option<String>)> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // "checked out by user 'xxx'" — timestamp captured only if the line isn't truncated.

        regex::Regex::new(r"checked out by user '([^']+)'(?:.*?updated at ([0-9-]+ [0-9:]+))?")
            .expect("static SCM checked-out regex is valid")
    });
    let caps = re.captures(raw)?;
    let owner = caps.get(1)?.as_str().trim().to_string();
    if owner.is_empty() {
        return None;
    }
    let ts = caps.get(2).map(|m| m.as_str().trim().to_string());
    Some((owner, ts))
}

/// Prefix that initializes a SCM session via SourceControlCreate and binds obj=%SourceControl.
/// All SCM methods are instance methods — they require an active %SourceControl object.
fn scm_init_prefix(username: &str, password: &str) -> String {
    let user_q = os_quote(username);
    let pass_q = os_quote(password);
    format!(
        "set sc=##class(%Studio.SourceControl.Interface).SourceControlCreate(\"{user_q}\",\"{pass_q}\",.created,.flags,.outuser) \
         set obj=$get(%SourceControl) \
         if '$IsObject(obj) {{ write \"SCM_UNAVAILABLE\" quit }} "
    )
}

/// Build the ObjectScript snippet that invokes `UserAction` on the SCM instance, writing ONE
/// labelled record (see `SCM_RECORD`) that the parser finds wherever the hook's own output put it.
///
/// `msg` and `target` are both sent. #418 §3 was that only `msg` crossed the wire, and CCR puts the
/// check-out prompt — and in TEST/UAT/LIVE the warning that changes there must be reverted — in
/// `target`. Choosing between them is now `ScmRecord::display`, in Rust, so which one arrived is
/// still known: `NeedsUi` needs the address specifically.
pub(crate) fn user_action_code(
    action_id: &str,
    doc: &str,
    username: &str,
    password: &str,
) -> String {
    let prefix = scm_init_prefix(username, password);
    format!(
        "{prefix}set action=0 set target=\"\" set msg=\"\" set reload=0 \
         set sc=obj.UserAction(0,\"%SourceMenu,{}\",\"{}\",\"\",.action,.target,.msg,.reload) \
         set r={{}} set r.kind=\"action\" set r.code=action set r.msg=msg set r.target=target \
         write \"{SCM_RECORD}\"_r.%ToJSON()",
        os_quote(action_id),
        os_quote(doc),
    )
}

/// Build the ObjectScript snippet that re-runs `UserAction` then immediately calls
/// `AfterUserAction` in the same job, so %SourceControl state is preserved.
///
/// #418 §1: this used to end `write $system.Status.GetErrorText(sc)`, so "no output" meant success
/// and ANY output meant failure — the exact opposite of the `UserAction` generator's convention, and
/// a hook that wrote one line of chatter turned a successful checkout into `SCM_CHECKOUT_FAILED`.
/// There is an explicit `ok` field now, so the two generators answer the same way and silence from
/// either means the snippet never ran.
pub(crate) fn after_user_action_code(
    action_id: &str,
    doc: &str,
    answer: &str,
    username: &str,
    password: &str,
) -> String {
    let prefix = scm_init_prefix(username, password);
    // #418 §4: `AfterUserAction`'s fourth argument is the ANSWER, and for a code-7 prompt that
    // is the typed string — not a flag. This read `if answer == "yes" { "1" } else { "0" }`, so a
    // typed value arrived as 0 and the prompt could never be answered. yes/no keep their exact
    // previous spelling, so every existing path is byte-identical; anything else is passed as an
    // ObjectScript string literal. A non-numeric string is not `1`, so a hook testing `If Answer=1`
    // still reads an unrecognised answer as "no" — the refusal direction, unchanged.
    let answer_owned;
    let answer_os: &str = match answer {
        "yes" => "1",
        "no" => "0",
        typed => {
            answer_owned = format!("\"{}\"", os_quote(typed));
            &answer_owned
        }
    };
    let action_id_q = os_quote(action_id);
    let doc_q = os_quote(doc);
    format!(
        "{prefix}\
         set action=0 set target=\"\" set msg=\"\" set reload=0 \
         set sc=obj.UserAction(0,\"%SourceMenu,{action_id_q}\",\"{doc_q}\",\"\",.action,.target,.msg,.reload) \
         set sc=obj.AfterUserAction(0,\"%SourceMenu,{action_id_q}\",\"{doc_q}\",{answer_os},\"\") \
         set r={{}} set r.kind=\"after\" set r.ok=$select(sc=1:1,1:0) \
         set r.err=$system.Status.GetErrorText(sc) \
         write \"{SCM_RECORD}\"_r.%ToJSON()"
    )
}

/// Build a single ObjectScript snippet that queries all enabled SCM menu items via
/// the MenuItems ResultSet, writing one "name|enabled|displayName" line per item.
fn menu_all_items_code(doc: &str, username: &str, password: &str) -> String {
    let prefix = scm_init_prefix(username, password);
    let doc_q = os_quote(doc);
    format!(
        "{prefix}\
         set rset=##class(%ResultSet).%New(\"%Studio.SourceControl.Interface:MenuItems\") \
         set sc=rset.Execute(\"%SourceMenu\",\"{doc_q}\",\"\") \
         while rset.Next() {{ write rset.GetData(1)_\"|\"_rset.GetData(2),! }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AfterUserAction`'s fourth argument is the ANSWER. For a yes/no dialog that is 1 or 0; for a
    /// code-7 prompt it is the typed string, which used to arrive as 0.
    #[test]
    fn a_typed_answer_reaches_the_hook_as_a_string() {
        let c = after_user_action_code("%CheckOut", "D.cls", "P@ssw0rd", "u", "p");
        assert!(
            c.contains(r#","P@ssw0rd","#),
            "the typed value is not passed to AfterUserAction:\n{c}"
        );
        // CONTROL: yes/no are UNCHANGED, so no existing path moves. If this ever fails, every
        // confirmation dialog in the module changed meaning.
        let yes = after_user_action_code("%CheckOut", "D.cls", "yes", "u", "p");
        assert!(yes.contains(r#""D.cls",1,"#), "yes is no longer 1:\n{yes}");
        let no = after_user_action_code("%CheckOut", "D.cls", "no", "u", "p");
        assert!(no.contains(r#""D.cls",0,"#), "no is no longer 0:\n{no}");
    }

    /// A typed answer containing a quote must not break out of the literal.
    #[test]
    fn a_typed_answer_is_escaped() {
        let c = after_user_action_code("%CheckOut", "D.cls", r#"a"b"#, "u", "p");
        assert!(c.contains(r#""a""b""#), "the answer is not escaped:\n{c}");
    }
    // ── #418 §1: the snippets COMPILE and their record survives the real transport ──

    /// Run every SCM generator against a live instance.
    ///
    /// ## What this proves, measured rather than assumed
    ///
    /// The #418 §1 change rewrote three generated ObjectScript snippets, and every other assertion
    /// about them is a `contains` on a Rust string. So this runs them.
    ///
    /// **It does NOT prove they compile, and the first version of this comment claimed it did.**
    /// Measured against intersystemsdc/iris-community:2026.1 through `execute_via_generator`:
    ///
    /// ```text
    /// write "OK"_$zconvert("x"            -> Ok("ERROR: <SYNTAX> 3 RunUser+1^IrisDevTmp.Run….1")
    /// write "OK" quit / $zconvert("x"     -> Ok("OK")
    /// write "OK" quit / @@@ not code @@@  -> Ok("OK")
    /// ```
    ///
    /// A syntax error does not fail the COMPILE — it surfaces at RUNTIME, on the line that executes.
    /// So a line after a `quit` is never validated: literal garbage there is invisible. On an
    /// instance with no source-control class, `scm_init_prefix` writes `SCM_UNAVAILABLE` and quits,
    /// which is before the record write in the two tool snippets. A deliberately broken `%ToJSON(`
    /// in `user_action_code` therefore PASSES this test — verified, and it is
    /// `the_two_generators_now_agree_about_empty` that catches it, textually.
    ///
    /// What it does establish, and what nothing offline can:
    ///
    /// * the lines that DO execute are valid — a reached `<SYNTAX>` lands in the output and is
    ///   asserted against below;
    /// * the probe's `scmClass=""` branch writes a REAL record on this instance, so the record
    ///   format is exercised generator -> transport -> parser with bytes IRIS produced;
    /// * that record arrives on ONE line, which is the property that made `%ToJSON()` the right
    ///   format and the issue's suggested `$c(1)` delimiter the wrong one — a written control
    ///   character arrives as a newline;
    /// * `parse_probe_record` reads what IRIS actually wrote, not what we assumed it would.
    ///
    /// Covering the unreached lines needs an instance with a source-control class configured, which
    /// is what #418's CCR sandbox spike is for.
    ///
    /// `#[ignore]` rather than an `if env.is_none() { return }` guard: an env-guarded early return
    /// prints `... ok` and reads as coverage, whereas an ignored test reports `ignored`. The e2e job
    /// runs the whole list with `--include-ignored` and IRIS_HOST set.
    #[test]
    #[ignore = "requires a live IRIS (IRIS_HOST); the e2e job runs it with --include-ignored"]
    fn the_generated_snippets_run_and_the_record_round_trips() {
        let host = std::env::var("IRIS_HOST")
            .ok()
            .filter(|h| !h.is_empty())
            .expect(
                "IRIS_HOST is unset. This test was asked to run (--include-ignored) and cannot: \
                 skipping would report success for a snippet nobody compiled.",
            );
        let port = std::env::var("IRIS_WEB_PORT").unwrap_or_else(|_| "52773".into());
        let user = std::env::var("IRIS_USERNAME").unwrap_or_else(|_| "_SYSTEM".into());
        let pass = std::env::var("IRIS_PASSWORD").unwrap_or_else(|_| "SYS".into());
        let ns = std::env::var("IRIS_NAMESPACE").unwrap_or_else(|_| "USER".into());
        let c = crate::iris::connection::IrisConnection::new(
            format!("http://{host}:{port}"),
            &ns,
            &user,
            &pass,
            crate::iris::connection::DiscoverySource::EnvVar,
        );
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let client = crate::iris::connection::IrisConnection::http_client().expect("client");

            // Anything IRIS says when it cannot PARSE or RUN the snippet. A compile failure comes
            // back as an error from the generator, so the `expect` below is the first guard; these
            // catch the case where it answers 200 with a diagnosis in the body.
            let compile_noise = ["<SYNTAX>", "MPP", "#5373", "#1026", "<UNDEFINED>", "<COMMAND>"];

            for (name, code) in [
                ("user_action_code", user_action_code("%CheckOut", "D.cls", &user, &pass)),
                (
                    "after_user_action_code",
                    after_user_action_code("%CheckOut", "D.cls", "yes", &user, &pass),
                ),
                (
                    "after_user_action_code (typed answer)",
                    after_user_action_code("%CheckOut", "D.cls", "P@ss", &user, &pass),
                ),
                ("menu_all_items_code", menu_all_items_code("D.cls", &user, &pass)),
            ] {
                let out = c
                    .execute_via_generator(&code, &ns, &client)
                    .await
                    .unwrap_or_else(|e| panic!("{name} did not run: {e}\n{code}"));
                for bad in compile_noise {
                    assert!(
                        !out.contains(bad),
                        "{name} produced {bad} on a line that RAN — the snippet is not valid ObjectScript. Note \
                         this only covers executed lines; see this test's doc comment.\n{out}\n{code}"
                    );
                }
                // No source control here, so `scm_init_prefix` answers and quits.
                assert!(
                    out.contains("SCM_UNAVAILABLE"),
                    "{name} did not reach the no-provider branch, so what it did is unknown:\n{out}"
                );
            }

            // The probe writes a REAL record on this instance. Generator -> transport -> parser.
            let probe = crate::tools::doc::scm_precheck_code_for_test("D.cls", &user, &pass);
            let out = c
                .execute_via_generator(&probe, &ns, &client)
                .await
                .unwrap_or_else(|e| panic!("the probe did not run: {e}\n{probe}"));
            for bad in compile_noise {
                assert!(!out.contains(bad), "the probe produced {bad} on a line that RAN:\n{out}\n{probe}");
            }
            assert!(
                out.contains(SCM_RECORD),
                "the probe wrote no record, so the sentinel does not survive the transport:\n{out:?}"
            );
            // It is ONE line — the property that made JSON the right format. A written control
            // character arrives as a newline (measured), which is why `$c(1)` was rejected.
            let rec_line = out
                .lines()
                .find(|l| l.trim_start().starts_with(SCM_RECORD))
                .expect("the record line");
            assert!(
                !rec_line.trim_end().contains('\n'),
                "the record spans lines: {rec_line:?}"
            );
            // And the Rust side reads what IRIS actually wrote.
            assert_eq!(
                parse_probe_record(&out),
                Some(ProbeKind::NoSourceControl),
                "the parser does not read the bytes the generator produced: {out:?}"
            );
        });
    }

    // ── the opt-in table (#418 §5) ────────────────────────────────────────────

    /// §5. `%Disconnect`/`%Reconnect` switch the namespace's provider integration off or on, and
    /// in the CCR hook they run immediately with no dialog to decline. Before this they were
    /// `is_write`, which refuses them on a READ-ONLY connection and allows them everywhere else —
    /// and everywhere else is the common case, which is what §5 is about.
    #[test]
    fn the_administrative_actions_need_an_opt_in() {
        for id in ["Disconnect", "Reconnect", "%Disconnect", "%Reconnect"] {
            let o = ScmAction::from_id(id)
                .opt_in()
                .unwrap_or_else(|| panic!("{id} is not opt-in gated, so it runs by default"));
            assert_eq!(o.var, "IRIS_SCM_ALLOW_DISCONNECT", "{id}");
            assert_eq!(o.code, SCM_ADMIN_BLOCKED, "{id}");
        }
    }

    /// CheckIn keeps its own variable and its own code. This is the control for the table: a
    /// refactor that collapsed every opt-in onto one variable would satisfy the test above and
    /// silently widen what `IRIS_SCM_ALLOW_CHECKIN` grants.
    #[test]
    fn checkin_keeps_the_variable_and_code_it_already_had() {
        let o = ScmAction::from_id("%CheckIn")
            .opt_in()
            .expect("CheckIn is gated");
        assert_eq!(o.var, "IRIS_SCM_ALLOW_CHECKIN");
        assert_eq!(o.code, "CHECKIN_BLOCKED");
        assert_ne!(
            o.var,
            ScmAction::from_id("%Disconnect").opt_in().unwrap().var,
            "the two opt-ins share a variable, so enabling one enables the other"
        );
    }

    /// An action on ONE document is not opt-in gated — the write gate already covers it, and
    /// default-refusing `%CheckOut` would make the tool useless.
    #[test]
    fn a_single_document_action_is_not_opt_in_gated() {
        for id in [
            "CheckOut",
            "UndoCheckout",
            "GetLatest",
            "AddToSourceControl",
            "Diff",
        ] {
            assert!(
                ScmAction::from_id(id).opt_in().is_none(),
                "{id} is opt-in gated, which refuses ordinary document work by default"
            );
        }
        // CONTROL: opt_in() does not simply return None for everything.
        assert!(ScmAction::from_id("%CheckIn").opt_in().is_some());
    }

    /// The two questions are different, and this is the pair that shows it. An unrecognised id
    /// names a method on the server's own site-written subclass: it IS a write (so a read-only
    /// connection refuses it) and it is NOT opt-in gated (so calling site hooks still works).
    #[test]
    fn an_unknown_action_is_a_write_but_not_opt_in_gated() {
        let a = ScmAction::from_id("SomeSiteHook");
        assert!(matches!(a, ScmAction::Unknown(_)));
        assert!(
            a.is_write(),
            "an unknown hook must not run on a read-only connection"
        );
        assert!(
            a.opt_in().is_none(),
            "a blanket opt-in on unknown ids would default-refuse every site hook, which is what \
             this tool exists to call"
        );
    }

    /// #170's rule, as a test. A denial that names its own escape hatch is read by an agent as the
    /// next step to take — measured on this fork, where 4/4 recoveries wrote the config first. The
    /// variable goes to stderr and to the reviewed remedy table; not to the caller.
    #[test]
    fn the_refusal_does_not_hand_over_the_variable_that_lifts_it() {
        let prev = std::env::var("IRIS_SCM_ALLOW_DISCONNECT").ok();
        std::env::remove_var("IRIS_SCM_ALLOW_DISCONNECT");
        let (code, detail) = opt_in_refusal("%Disconnect").expect("must refuse when unset");
        assert_eq!(code, SCM_ADMIN_BLOCKED);
        assert!(
            !detail.contains("IRIS_SCM_ALLOW"),
            "the refusal names the variable that lifts it: {detail}"
        );
        assert!(
            !detail.contains('='),
            "the refusal looks like an assignment the reader can copy: {detail}"
        );
        // CONTROL: it still says what was refused and that it is an operator setting, so the
        // assertions above are not satisfied by an empty or useless message.
        assert!(detail.contains("%Disconnect"), "{detail}");
        assert!(detail.contains("operator"), "{detail}");
        if let Some(v) = prev {
            std::env::set_var("IRIS_SCM_ALLOW_DISCONNECT", v);
        }
    }

    /// And the opt-in actually lifts it. Without this the table could refuse unconditionally.
    #[test]
    fn setting_the_opt_in_lifts_the_refusal() {
        let prev = std::env::var("IRIS_SCM_ALLOW_DISCONNECT").ok();
        for v in ["1", "true", "YES"] {
            std::env::set_var("IRIS_SCM_ALLOW_DISCONNECT", v);
            assert!(
                opt_in_refusal("%Disconnect").is_none(),
                "IRIS_SCM_ALLOW_DISCONNECT={v} did not enable the action"
            );
        }
        for v in ["0", "no", "", "maybe"] {
            std::env::set_var("IRIS_SCM_ALLOW_DISCONNECT", v);
            assert!(
                opt_in_refusal("%Disconnect").is_some(),
                "IRIS_SCM_ALLOW_DISCONNECT={v} should NOT enable the action"
            );
        }
        match prev {
            Some(v) => std::env::set_var("IRIS_SCM_ALLOW_DISCONNECT", v),
            None => std::env::remove_var("IRIS_SCM_ALLOW_DISCONNECT"),
        }
        // An action with no opt-in is never refused by this path, whatever the environment.
        assert!(opt_in_refusal("%CheckOut").is_none());
    }

    // ── CodeMeaning (#418 §4) ─────────────────────────────────────────────────

    /// The table itself. These are the codes `%Studio.SourceControl` defines and CCR returns, so a
    /// change to any row here is a change to what the server believes IRIS told it.
    #[test]
    fn each_action_code_means_what_the_hook_api_says_it_means() {
        for (code, want) in [
            (0u8, CodeMeaning::NoDialog),
            (1, CodeMeaning::Confirm),
            (2, CodeMeaning::NeedsUi),
            (3, CodeMeaning::NeedsUi),
            (6, CodeMeaning::Declined),
            (7, CodeMeaning::Prompt),
        ] {
            assert_eq!(
                CodeMeaning::of(code),
                want,
                "action {code} classified as {:?}, expected {want:?}",
                CodeMeaning::of(code)
            );
        }
        // CONTROL: the function is not returning one value for everything. Without this, a body of
        // `_ => CodeMeaning::NoDialog` would satisfy the row for 0 and nothing above would notice
        // that the others were wrong — which is the shape the `checkout` arm actually had.
        let distinct: std::collections::BTreeSet<_> = (0u8..=7)
            .map(|c| format!("{:?}", CodeMeaning::of(c)))
            .collect();
        assert!(
            distinct.len() >= 5,
            "codes 0-7 produced only {} distinct meanings ({distinct:?}) — the table is collapsed",
            distinct.len()
        );
    }

    /// Codes outside the table are not driveable. The affordable error is refusing an action that
    /// turns out to have been harmless; the unaffordable one is reporting an unknown answer as done.
    #[test]
    fn an_unlisted_code_is_undriveable_not_a_success() {
        for code in [4u8, 5, 8, 9, 42, 255] {
            assert_eq!(
                CodeMeaning::of(code),
                CodeMeaning::Undriveable,
                "action {code} is not classified as Undriveable"
            );
        }
    }

    /// The `checkout` arm's defect, as a property: it read `if action_code == 0` and let everything
    /// else fall into a yes/no dialog, so a prompt for a typed value became a yes/no question —
    /// where answering "yes" sends the literal string "yes" as the value.
    #[test]
    fn a_prompt_is_not_a_confirmation() {
        assert_ne!(
            CodeMeaning::of(7),
            CodeMeaning::Confirm,
            "action 7 asks for a typed value; answering it yes/no sends \"yes\" as the value"
        );
        assert_eq!(CodeMeaning::of(7), CodeMeaning::Prompt);
    }

    /// The costliest row. CCR rewrites its own password prompt from action 7 into action 2, so this
    /// is the code that arrives when Perforce credentials are missing. The `checkout` arm asked the
    /// user yes/no about a URL; `iris_doc`'s probe read it as "proceed" and wrote the document with
    /// no check-out at all.
    #[test]
    fn the_login_page_code_is_neither_a_question_nor_a_go_ahead() {
        let m = CodeMeaning::of(2);
        assert_ne!(
            m,
            CodeMeaning::Confirm,
            "action 2 is a page, not a yes/no question"
        );
        assert_ne!(
            m,
            CodeMeaning::NoDialog,
            "action 2 is not permission to proceed"
        );
        assert_eq!(m, CodeMeaning::NeedsUi);
    }

    /// A refusal must carry the address, because for `NeedsUi` the address IS the remedy. This is
    /// also the pay-off of §3: the page only reaches us at all because the snippet now sends
    /// `target` as well as `msg`.
    #[test]
    fn a_needs_ui_refusal_carries_the_page_to_open() {
        let url = "https://ccr.example/csp/ccr/login.csp?ns=APP";
        let (code, detail) = undriveable_refusal(CodeMeaning::NeedsUi, 2, url, "My.Cls.cls");
        assert_eq!(code, SCM_NEEDS_UI);
        assert!(
            detail.contains(url),
            "the refusal drops the only actionable part of the answer: {detail}"
        );
        // CONTROL: the detail is not simply the message echoed back — it says what happened.
        assert!(
            detail.len() > url.len() + 20,
            "the refusal is barely more than the url: {detail}"
        );
    }

    /// A prompt refusal names the document, since the caller may have several writes in flight.
    #[test]
    fn a_prompt_refusal_names_the_document_and_its_own_code() {
        let (code, detail) = undriveable_refusal(CodeMeaning::Prompt, 7, "", "My.Cls.cls");
        assert_eq!(code, SCM_NEEDS_INPUT);
        assert!(detail.contains("My.Cls.cls"), "{detail}");
        // No provider text, so none is invented.
        assert!(
            !detail.contains("provider said"),
            "an empty message produced a dangling 'provider said': {detail}"
        );
    }

    /// An unknown code keeps the old wording and the old code, so nothing that parsed the previous
    /// message loses its footing — but it reports the number, which is the diagnosable part.
    #[test]
    fn an_undriveable_code_is_reported_with_its_number() {
        let (code, detail) = undriveable_refusal(CodeMeaning::Undriveable, 9, "odd", "D.cls");
        assert_eq!(code, "SCM_ERROR");
        assert!(
            detail.contains('9'),
            "the code number is not in the message: {detail}"
        );
        assert!(
            detail.contains("odd"),
            "the provider text is dropped: {detail}"
        );
    }

    // ── os_quote ──────────────────────────────────────────────────────────────
    #[test]
    fn test_os_quote_double_quotes() {
        assert_eq!(os_quote(r#"say "hi""#), r#"say ""hi"""#);
    }
    #[test]
    fn test_os_quote_newline() {
        assert_eq!(os_quote("a\nb"), "a$Char(10)b");
    }
    #[test]
    fn test_os_quote_cr() {
        assert_eq!(os_quote("a\rb"), "a$Char(13)b");
    }
    #[test]
    fn test_os_quote_plain() {
        assert_eq!(os_quote("hello"), "hello");
    }
    #[test]
    fn test_os_quote_empty() {
        assert_eq!(os_quote(""), "");
    }
    #[test]
    fn test_os_quote_mixed_quote_and_newline() {
        // String with both double-quote and newline
        assert_eq!(os_quote("say \"hi\"\nbye"), "say \"\"hi\"\"$Char(10)bye");
    }
    #[test]
    fn test_os_quote_cr_and_newline() {
        // String with carriage return AND newline
        assert_eq!(os_quote("line\r\nend"), "line$Char(13)$Char(10)end");
    }

    // ── #418 §1: the answer is a labelled RECORD, found wherever the hook put it ──

    /// What the generators actually write, so every test below speaks the real wire format rather
    /// than a convenient approximation of it. Measured against a live instance; see
    /// `SCM_RECORD`'s doc comment for the bytes.
    fn record(json: &str) -> String {
        format!("{SCM_RECORD}{json}")
    }

    #[test]
    fn a_record_is_read_field_by_field() {
        let out = record(r#"{"kind":"action","code":1,"msg":"Check out?","target":""}"#);
        match parse_action_msg(&out) {
            ActionMsg::Record(r, chatter) => {
                assert_eq!(r.action_code(), Some(1));
                assert_eq!(r.msg, "Check out?");
                assert_eq!(r.display(), "Check out?");
                assert_eq!(
                    chatter, "",
                    "a quiet provider produced chatter: {chatter:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// **THE §1 DEFECT.** CCR writes a `NOTICE:` line when no Perforce user is defined and a
    /// `CMD: p4 …` echo whenever it shells out. The code used to be read from `lines().next()`, so
    /// the NOTICE became the answer. There is no first line to be wrong about any more.
    #[test]
    fn the_record_is_found_after_the_hooks_own_chatter() {
        let out = format!(
            "NOTICE: no Perforce user is defined for this namespace\n\n{}\nCMD: p4 edit MyApp.cls\n",
            record(r#"{"kind":"action","code":2,"target":"https://ccr/login.csp"}"#)
        );
        match parse_action_msg(&out) {
            ActionMsg::Record(r, chatter) => {
                assert_eq!(
                    r.action_code(),
                    Some(2),
                    "the hook's NOTICE was read as the answer"
                );
                assert_eq!(r.display(), "https://ccr/login.csp");
                // §1's second half: the chatter is RETURNED, not discarded.
                assert!(chatter.contains("NOTICE: no Perforce user"), "{chatter:?}");
                assert!(chatter.contains("CMD: p4 edit"), "{chatter:?}");
                assert!(
                    !chatter.contains(SCM_RECORD),
                    "our own record is being reported back as hook output: {chatter:?}"
                );
            }
            other => panic!("the record was not found among chatter: {other:?}"),
        }
    }

    /// A multi-line provider message survives, because `%ToJSON()` escapes the newline and the
    /// record stays one line. This is the property that made JSON the right format and the issue's
    /// suggested `$c(1)` delimiter the wrong one — a written control character arrives as a newline.
    #[test]
    fn a_multi_line_message_survives_the_parse() {
        let out = record(
            r#"{"kind":"action","code":1,"msg":"Cannot check out\nERROR #5803: Lock held by 'alice'"}"#,
        );
        match parse_action_msg(&out) {
            ActionMsg::Record(r, _) => {
                assert!(r.msg.contains("ERROR #5803"), "truncated: {:?}", r.msg);
                assert!(r.msg.contains("'alice'"), "truncated: {:?}", r.msg);
                assert!(
                    r.msg.contains('\n'),
                    "the newline did not survive: {:?}",
                    r.msg
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// §3: `target` crosses the wire on its own, so a dialog whose text IRIS put there is not lost.
    /// Both fields are sent now, instead of an ObjectScript `$select` that hid which one arrived.
    #[test]
    fn target_is_used_when_msg_is_empty_and_both_are_kept() {
        let out =
            record(r#"{"kind":"action","code":1,"msg":"","target":"THIS IS LIVE. Revert after."}"#);
        match parse_action_msg(&out) {
            ActionMsg::Record(r, _) => {
                assert_eq!(r.display(), "THIS IS LIVE. Revert after.");
                assert_eq!(r.target, "THIS IS LIVE. Revert after.");
                assert_eq!(r.msg, "");
            }
            other => panic!("{other:?}"),
        }
        // CONTROL: when msg IS set, it wins — target does not shadow it.
        let out = record(r#"{"kind":"action","code":1,"msg":"m","target":"t"}"#);
        match parse_action_msg(&out) {
            ActionMsg::Record(r, _) => assert_eq!(r.display(), "m"),
            other => panic!("{other:?}"),
        }
    }

    /// The snippet writes `set r.code=action` rather than `+action` ON PURPOSE. Coercing it in
    /// ObjectScript would turn anything non-numeric into 0, and 0 is the success code — the
    /// `unwrap_or(0)` defect of #302 and #418 §2, moved somewhere no Rust test could reach it.
    #[test]
    fn a_code_that_is_not_a_number_is_not_a_go_ahead() {
        for json in [
            r#"{"kind":"action","code":"","msg":"x"}"#,
            r#"{"kind":"action","code":"oops","msg":"x"}"#,
            r#"{"kind":"action","msg":"x"}"#,
            r#"{"kind":"action","code":999,"msg":"x"}"#,
        ] {
            let out = record(json);
            match parse_action_msg(&out) {
                ActionMsg::Record(r, _) => assert_eq!(
                    r.action_code(),
                    None,
                    "{json} produced an action code, and a wrong one reads as success"
                ),
                other => panic!("{json}: {other:?}"),
            }
            assert!(
                user_action_outcome(&out).is_err(),
                "{json} was accepted as an answer"
            );
        }
        // CONTROL: a real code still parses.
        let ok = record(r#"{"kind":"action","code":0}"#);
        assert_eq!(user_action_outcome(&ok).unwrap().code, 0);
    }

    /// Output with no record is not an answer, and every line is kept because `scm_error_code`
    /// classifies on content — a code whose distinguishing text sits on a later line must be
    /// reachable.
    #[test]
    fn output_with_no_record_keeps_every_line_for_the_classifier() {
        let raw = "<PROTECT>zCheckOut+4^%Studio.SourceControl.ISC.1\nProtection: ^SYS(\"SCM\")";
        match parse_action_msg(raw) {
            ActionMsg::Unparseable(text) => {
                assert!(text.contains("<PROTECT>"), "{text:?}");
                assert!(text.contains("Protection:"), "line 2 was dropped: {text:?}");
            }
            other => panic!("{other:?}"),
        }
        // A record we cannot PARSE is also not a go-ahead.
        match parse_action_msg(&record("{not json")) {
            ActionMsg::Unparseable(_) => {}
            other => panic!("a malformed record was accepted: {other:?}"),
        }
    }

    #[test]
    fn empty_output_is_refused_and_is_distinguishable_from_code_zero() {
        assert_eq!(parse_action_msg(""), ActionMsg::Empty);
        assert_eq!(parse_action_msg("   \n\n "), ActionMsg::Empty);
        let (code, _) = user_action_outcome("").unwrap_err();
        assert_eq!(code, SCM_NO_OUTPUT);
        // CONTROL: code 0 is NOT empty — it is a real answer and must stay one.
        assert_eq!(
            user_action_outcome(&record(r#"{"kind":"action","code":0}"#))
                .unwrap()
                .code,
            0
        );
    }

    #[test]
    fn the_unavailable_sentinel_and_the_unavailable_record_both_keep_their_code() {
        // `scm_init_prefix` writes the bare sentinel and quits, so there is no record to find.
        let (code, _) = user_action_outcome("SCM_UNAVAILABLE").unwrap_err();
        assert_eq!(code, "SCM_UNAVAILABLE");
        // The probe writes it as a record kind instead.
        let (code, _) = user_action_outcome(&record(r#"{"kind":"unavailable"}"#)).unwrap_err();
        assert_eq!(
            code, "SCM_UNAVAILABLE",
            "an unavailable record did not reach the caller as SCM_UNAVAILABLE"
        );
        // CONTROL: a near-miss sentinel is NOT silently accepted as one.
        let (code, _) = user_action_outcome("SCM_UNAVAILABLE_TYPO").unwrap_err();
        assert_ne!(code, "SCM_NO_OUTPUT");
    }

    // ── #418 §1, third bullet: AfterUserAction answers explicitly ─────────────

    #[test]
    fn after_user_action_reads_an_explicit_ok_field() {
        let out = record(r#"{"kind":"after","ok":1,"err":""}"#);
        assert_eq!(after_user_action_outcome(&out), Ok(String::new()));
        let out = record(
            r#"{"kind":"after","ok":0,"err":"ERROR #5803: locked\r\nERROR #5001: by alice"}"#,
        );
        match after_user_action_outcome(&out) {
            Err((code, detail)) => {
                assert_eq!(code, "SCM_CHECKOUT_FAILED");
                assert!(detail.contains("#5803"), "{detail}");
                assert!(
                    detail.contains("#5001"),
                    "the chain was truncated: {detail}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// **The defect this bullet names.** The finalizer used to treat ANY output as the error text,
    /// so a hook that echoed one `CMD: p4 edit …` line turned a successful checkout into
    /// `SCM_CHECKOUT_FAILED`. The explicit `ok` field is what removes that.
    #[test]
    fn hook_chatter_no_longer_turns_a_successful_finalize_into_a_failure() {
        let out = format!(
            "CMD: p4 edit //depot/MyApp.cls\n{}\n",
            record(r#"{"kind":"after","ok":1,"err":""}"#)
        );
        match after_user_action_outcome(&out) {
            Ok(chatter) => assert!(
                chatter.contains("CMD: p4 edit"),
                "the chatter was dropped instead of returned: {chatter:?}"
            ),
            other => panic!("chatter was read as a failure: {other:?}"),
        }
    }

    /// `SCM_UNAVAILABLE` on the finalize path is no longer waved through as success. Both call
    /// sites read `!out.is_empty() && out != "SCM_UNAVAILABLE"`, so a session that could not be
    /// created fell through to a COMMITTED checkout — a failure answered as a fact.
    #[test]
    fn an_unavailable_session_does_not_finalize_a_checkout() {
        match after_user_action_outcome("SCM_UNAVAILABLE") {
            Err((code, _)) => assert_eq!(code, "SCM_UNAVAILABLE"),
            other => panic!("an unavailable session reported a committed checkout: {other:?}"),
        }
        // Silence means the snippet never ran — on this path too, now.
        assert!(after_user_action_outcome("").is_err());
    }

    /// Replaces `the_two_generators_disagree_about_empty`, which pinned an asymmetry that no longer
    /// exists — and pinning it was right while it did, because neither convention could be reasoned
    /// about from one side.
    ///
    /// `user_action_code` always wrote at least `0|`, so empty meant "never ran".
    /// `after_user_action_code` ended with `GetErrorText`, which returns `""` for an OK status, so
    /// empty meant SUCCESS. Opposite meanings for the same observation, three call sites between
    /// them, and a fix applied to either one was wrong at the other. Both now write a labelled
    /// record with an explicit field, so empty means "never ran" at both and there is nothing left
    /// to keep straight.
    #[test]
    fn the_two_generators_now_agree_about_empty() {
        let ua = user_action_code("%CheckOut", "D.cls", "u", "p");
        let aua = after_user_action_code("%CheckOut", "D.cls", "yes", "u", "p");
        for (name, code) in [("user_action_code", &ua), ("after_user_action_code", &aua)] {
            assert!(
                code.contains(SCM_RECORD) && code.contains("%ToJSON()"),
                "{name} does not write the labelled record:\n{code}"
            );
        }
        assert!(
            !aua.contains("write $system.Status.GetErrorText(sc)"),
            "after_user_action_code still ends by writing the error text alone, so empty means \
             success there and 'never ran' everywhere else:\n{aua}"
        );
        assert!(aua.contains("r.ok="), "no explicit ok field:\n{aua}");
        // And empty is now refused on BOTH paths — the assertion the old test could not make.
        assert!(user_action_outcome("").is_err());
        assert!(after_user_action_outcome("").is_err());
    }

    /// Population guard: no generator in this file writes the old bare `action|msg` form, so a
    /// snippet added later cannot quietly reintroduce a format the parser no longer finds.
    #[test]
    fn no_generator_writes_the_old_pipe_form() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/scm.rs"))
            .expect("scm.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => &src[..],
        };
        // Comments are stripped: this file discusses `action_"|"_msg` in several places while
        // explaining why it is gone, and a guard that flags its own prose gets loosened until it
        // catches nothing.
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        // Assembled from fragments so the literal never appears in this file at all — the first
        // version of this guard's sibling flagged its OWN control line.
        let needle = concat!("action_", "\\\"|\\\"");
        assert!(
            !code.contains(needle),
            "a generator writes the old bare pipe form, which the parser no longer looks for"
        );
        // CONTROL: the window really is the production half and really was searched.
        assert!(
            code.contains("fn user_action_code"),
            "the window does not contain the generators"
        );
        assert!(
            prod.len() > src.len() / 4,
            "the production window is {} of {} bytes — implausibly small, so the search above \
             covered almost nothing",
            prod.len(),
            src.len()
        );
    }

    // ── user_action_code ──────────────────────────────────────────────────────
    #[test]
    fn test_user_action_code_no_backslash_quote() {
        let code = user_action_code("CheckOut", "MyApp.Patient.cls", "user", "pass");
        assert!(
            !code.contains("\\\""),
            "must use ObjectScript quoting, not backslash: {}",
            code
        );
        assert!(
            code.contains("CheckOut"),
            "must contain action_id: {}",
            code
        );
        assert!(
            code.contains("MyApp.Patient.cls"),
            "must contain doc: {}",
            code
        );
    }
    #[test]
    fn test_user_action_code_escapes_quotes_in_action() {
        let code = user_action_code("Check\"Out", "Doc.cls", "user", "pass");
        assert!(
            code.contains("\"\""),
            "double-quote must become \"\": {}",
            code
        );
        assert!(!code.contains("\\\""), "no backslash-quote: {}", code);
    }
    #[test]
    fn test_user_action_code_escapes_newline_in_doc() {
        let code = user_action_code("CheckOut", "Doc\nwith\nnewlines.cls", "user", "pass");
        assert!(
            code.contains("$Char(10)"),
            "newline must become $Char(10): {}",
            code
        );
    }

    // ── status_check_code ─────────────────────────────────────────────────────
    #[test]
    fn test_status_check_code_uses_get_status() {
        let code = status_check_code("MyApp.Patient.cls", "user", "pass");
        assert!(
            code.contains("%Studio.SourceControl.Interface"),
            "must use Interface class: {code}"
        );
        assert!(code.contains("GetStatus"), "must call GetStatus: {code}");
        assert!(
            code.contains("SourceControlCreate"),
            "must init session: {code}"
        );
        assert!(
            !code.contains("%GetImplementationObject"),
            "must not use removed method: {code}"
        );
    }

    #[test]
    fn test_status_check_code_contains_doc() {
        let code = status_check_code("MyApp.Patient.cls", "user", "pass");
        assert!(
            code.contains("MyApp.Patient.cls"),
            "must embed document name: {code}"
        );
    }

    #[test]
    fn test_status_check_code_escapes_quotes_in_doc() {
        let code = status_check_code("My\"App.cls", "user", "pass");
        assert!(
            code.contains("\"\""),
            "double-quote must become \"\": {code}"
        );
        assert!(!code.contains("\\\""), "no backslash-quote: {code}");
    }

    #[test]
    fn test_status_check_code_escapes_quotes_in_credentials() {
        let code = status_check_code("Any.cls", "us\"er", "p\"ass");
        assert!(
            code.contains("\"\""),
            "double-quote in credentials must be escaped: {code}"
        );
    }

    #[test]
    fn test_status_check_code_emits_structured_sentinel() {
        let code = status_check_code("Any.cls", "user", "pass");
        assert!(
            code.contains("SCMSTATUS|"),
            "must emit the SCMSTATUS sentinel line: {code}"
        );
    }

    #[test]
    fn test_status_check_code_uses_menu_items_for_checkout_state() {
        let code = status_check_code("Any.cls", "user", "pass");
        assert!(
            code.contains("MenuItems"),
            "must use MenuItems to deduce checkout state: {code}"
        );
        assert!(
            code.contains("%UndoCheckout"),
            "must check for UndoCheckout to detect checked-out: {code}"
        );
        assert!(
            code.contains("%CheckOut"),
            "must check for CheckOut availability: {code}"
        );
        assert!(
            code.contains("%AddToSourceControl"),
            "must check AddToSourceControl to distinguish uncontrolled docs: {code}"
        );
    }

    #[test]
    fn test_status_check_code_contains_username() {
        let code = status_check_code("Doc.cls", "myuser", "pass");
        assert!(code.contains("myuser"), "must contain the username: {code}");
    }

    #[test]
    fn test_status_check_code_contains_document_name() {
        let code = status_check_code("MyClass.cls", "user", "pass");
        assert!(
            code.contains("MyClass.cls"),
            "must contain the document name: {code}"
        );
    }

    #[test]
    fn test_status_check_code_contains_scmstatus_sentinel() {
        let code = status_check_code("Doc.cls", "user", "pass");
        assert!(
            code.contains("SCMSTATUS"),
            "must contain SCMSTATUS sentinel: {code}"
        );
    }

    // ── parse_scm_status_line ─────────────────────────────────────────────────
    #[test]
    fn test_parse_scm_status_line_full() {
        let (in_sc, co, undo, add, owner) =
            parse_scm_status_line("SCMSTATUS|0|1|0|0|0|0|test").unwrap();
        assert!(in_sc);
        assert!(!co);
        assert!(!undo);
        assert!(!add);
        assert_eq!(owner, "test");
    }

    #[test]
    fn test_parse_scm_status_line_finds_sentinel_amid_noise() {
        // Executor may prepend/append error lines; find_map over lines must still parse it.
        let raw = "SCMSTATUS|0|0|0|1|0|1|\nERROR($ZERROR): <ENDOFFILE>";
        let parsed = raw.lines().find_map(parse_scm_status_line);
        assert!(parsed.is_some());
    }

    #[test]
    fn test_parse_scm_status_line_rejects_non_sentinel() {
        assert!(parse_scm_status_line("ERROR: something broke").is_none());
        assert!(parse_scm_status_line("").is_none());
    }

    #[test]
    fn test_parse_scm_status_line_with_owner() {
        // Line with owner field populated
        let (in_sc, co, undo, add, owner) =
            parse_scm_status_line("SCMSTATUS|0|1|0|1|1|0|alice").unwrap();
        assert!(in_sc);
        assert!(co);
        assert!(undo);
        assert!(!add);
        assert_eq!(owner, "alice");
    }

    #[test]
    fn test_parse_scm_status_line_malformed_missing_pipe() {
        // Malformed line (missing pipe) should return None
        assert!(parse_scm_status_line("SCMSTATUS0101010").is_none());
    }

    #[test]
    fn test_parse_scm_status_line_all_fields_zero() {
        // Valid line with all boolean fields as 0
        let (in_sc, co, undo, add, owner) =
            parse_scm_status_line("SCMSTATUS|0|0|0|0|0|0|").unwrap();
        assert!(!in_sc);
        assert!(!co);
        assert!(!undo);
        assert!(!add);
        assert_eq!(owner, "");
    }

    // ── parse_checked_out_by (native NOTICE fallback, bug #3) ─────────────────
    #[test]
    fn test_parse_checked_out_by_with_timestamp() {
        let raw = "NOTICE: 'My.Class.cls' is currently checked out by user 'todor', and was last updated at 2026-07-07 12:34:56";
        let (owner, ts) = parse_checked_out_by(raw).unwrap();
        assert_eq!(owner, "todor");
        assert_eq!(ts.as_deref(), Some("2026-07-07 12:34:56"));
    }

    #[test]
    fn test_parse_checked_out_by_truncated_no_timestamp() {
        // Real-world case: the message is truncated mid-line before "updated at …".
        let raw = "...is currently checked out by user 'todor', and was last";
        let (owner, ts) = parse_checked_out_by(raw).unwrap();
        assert_eq!(owner, "todor");
        assert_eq!(ts, None);
    }

    #[test]
    fn test_parse_checked_out_by_takes_first_of_repeated() {
        // The probe loops, so the notice repeats. First occurrence wins.
        let raw =
            "checked out by user 'todor', and was last\nchecked out by user 'alice', and was last";
        let (owner, _) = parse_checked_out_by(raw).unwrap();
        assert_eq!(owner, "todor");
    }

    #[test]
    fn test_parse_checked_out_by_none_when_absent() {
        assert!(parse_checked_out_by("ERROR: <PROTECT>").is_none());
        assert!(parse_checked_out_by("").is_none());
    }

    #[test]
    fn test_parse_checked_out_by_standard_format_no_timestamp() {
        // Standard format without timestamp
        let raw = "checked out by user 'james'";
        let (owner, ts) = parse_checked_out_by(raw).unwrap();
        assert_eq!(owner, "james");
        assert_eq!(ts, None);
    }

    #[test]
    fn test_parse_checked_out_by_empty_string() {
        // Empty string should return None
        assert!(parse_checked_out_by("").is_none());
    }

    #[test]
    fn test_parse_checked_out_by_unrelated_text() {
        // Unrelated text without the pattern should return None
        assert!(parse_checked_out_by("This document has some other information").is_none());
        assert!(
            parse_checked_out_by("checked out by admin but not in the expected format").is_none()
        );
    }

    // ── derive_scm_status ─────────────────────────────────────────────────────
    #[test]
    fn test_derive_uncontrolled_is_editable() {
        // Menu offers AddToSourceControl → uncontrolled, editable.
        let s = derive_scm_status(false, false, false, true, "", "me").unwrap();
        assert!(!s.controlled);
        assert!(s.editable);
        assert!(!s.locked);
        assert_eq!(s.owner, None);
    }

    #[test]
    fn test_derive_checked_out_by_me() {
        // In SC, UndoCheckout enabled → I hold it, editable.
        let s = derive_scm_status(true, false, true, false, "", "me").unwrap();
        assert!(s.controlled);
        assert!(s.editable);
        assert!(!s.locked);
        assert!(s.checked_out_by_me);
        assert_eq!(s.owner.as_deref(), Some("me"));
    }

    #[test]
    fn test_derive_locked_by_other() {
        // controlled, no CheckOut and no UndoCheckout available, GetStatus
        // owner reported → locked by someone else, NOT editable.
        let s = derive_scm_status(true, false, false, false, "test", "me").unwrap();
        assert!(s.controlled);
        assert!(
            !s.editable,
            "must NOT claim editable when locked by another user"
        );
        assert!(s.locked);
        assert!(!s.checked_out_by_me);
        assert_eq!(s.owner.as_deref(), Some("test"));
    }

    #[test]
    fn test_derive_controlled_available_to_checkout() {
        // Controlled, CheckOut offered (free to take) → not editable yet, but not locked.
        let s = derive_scm_status(true, true, false, false, "", "me").unwrap();
        assert!(s.controlled);
        assert!(!s.editable);
        assert!(!s.locked);
        assert!(!s.checked_out_by_me);
    }

    #[test]
    fn test_derive_locked_by_other_detected_via_owner_only() {
        // GetStatus didn't report isInSC and the menu offered nothing, but an owner came back →
        // controlled and locked by that other user (not a false "editable").
        let s = derive_scm_status(false, false, false, false, "test", "me").unwrap();
        assert!(s.controlled);
        assert!(s.locked);
        assert!(!s.editable);
        assert_eq!(s.owner.as_deref(), Some("test"));
    }

    #[test]
    fn test_derive_no_signal_is_uncontrolled() {
        // No in-SC flag, no menu items, no owner → no SCM configured in this namespace.
        // Must return controlled:false, editable:true — NOT None/SCM_UNAVAILABLE, which
        // was a false-positive error for namespaces that simply have no SCM.
        let s = derive_scm_status(false, false, false, false, "", "me").unwrap();
        assert!(!s.controlled);
        assert!(s.editable);
        assert!(!s.locked);
        assert!(!s.checked_out_by_me);
        assert!(s.owner.is_none());
    }

    #[test]
    fn test_derive_resolves_from_menu_signal_alone() {
        // GetStatus gave nothing, but the menu offered CheckOut → controlled, available.
        let s = derive_scm_status(false, true, false, false, "", "me").unwrap();
        assert!(s.controlled);
        assert!(!s.editable);
        assert!(!s.locked);
    }

    // ── SCM_MENU ──────────────────────────────────────────────────────────────
    #[test]
    fn test_scm_menu_prefix() {
        assert_eq!(SCM_MENU, "%SourceMenu");
    }

    // ── scm_init_prefix ──────────────────────────────────────────────────────
    #[test]
    fn test_scm_init_prefix_contains_source_control_create() {
        let code = scm_init_prefix("user", "pass");
        assert!(code.contains("SourceControlCreate"), "{code}");
        assert!(code.contains("%Studio.SourceControl.Interface"), "{code}");
    }

    #[test]
    fn test_scm_init_prefix_escapes_quotes_in_user() {
        let code = scm_init_prefix("us\"er", "pass");
        assert!(
            code.contains("\"\""),
            "double-quote must be doubled: {code}"
        );
        assert!(!code.contains("\\\""), "no backslash-quote: {code}");
    }

    // ── menu_all_items_code ──────────────────────────────────────────────────
    #[test]
    fn test_menu_all_items_code_contains_menu_items() {
        let code = menu_all_items_code("MyApp.cls", "user", "pass");
        assert!(code.contains("MenuItems"), "{code}");
    }

    #[test]
    fn test_menu_all_items_code_contains_doc() {
        let code = menu_all_items_code("MyApp.Patient.cls", "user", "pass");
        assert!(code.contains("MyApp.Patient.cls"), "{code}");
    }

    // ── after_user_action_code ───────────────────────────────────────────────
    #[test]
    fn test_after_user_action_code_contains_after_user_action() {
        let code = after_user_action_code("CheckOut", "MyApp.cls", "yes", "user", "pass");
        assert!(code.contains("AfterUserAction"), "{code}");
    }

    #[test]
    fn test_after_user_action_code_contains_doc() {
        let code = after_user_action_code("CheckOut", "MyApp.Patient.cls", "no", "user", "pass");
        assert!(code.contains("MyApp.Patient.cls"), "{code}");
    }

    #[test]
    fn test_after_user_action_code_yes_becomes_1() {
        let code = after_user_action_code("CheckOut", "Doc.cls", "yes", "user", "pass");
        assert!(code.contains(",1,"), "yes should become 1: {code}");
    }

    #[test]
    fn test_after_user_action_code_no_becomes_0() {
        let code = after_user_action_code("CheckOut", "Doc.cls", "no", "user", "pass");
        assert!(code.contains(",0,"), "no should become 0: {code}");
    }

    // ── Document name normalization ──────────────────────────────────────────
    #[test]
    fn test_normalize_cls_extension_appended_for_bare_class() {
        let doc = "MyApp.Patient";
        let normalized = if !doc.contains('.')
            || doc.ends_with(".cls")
            || doc.ends_with(".mac")
            || doc.ends_with(".inc")
            || doc.ends_with(".int")
        {
            doc.to_string()
        } else {
            format!("{}.cls", doc)
        };
        // "MyApp.Patient" has a dot but no extension suffix → should get .cls
        assert_eq!(normalized, "MyApp.Patient.cls");
    }

    // ── scm_init_prefix additional ───────────────────────────────────────────
    #[test]
    fn test_scm_init_prefix_contains_get_source_control() {
        let code = scm_init_prefix("user", "pass");
        // Must bind obj to %SourceControl for instance method calls
        assert!(
            code.contains("%SourceControl"),
            "must bind %SourceControl: {code}"
        );
    }

    #[test]
    fn test_scm_init_prefix_writes_scm_unavailable_on_no_obj() {
        let code = scm_init_prefix("user", "pass");
        assert!(
            code.contains("SCM_UNAVAILABLE"),
            "must write SCM_UNAVAILABLE when obj unavailable: {code}"
        );
    }

    #[test]
    fn test_scm_init_prefix_escapes_quotes_in_password() {
        let code = scm_init_prefix("user", "p\"ass");
        assert!(
            code.contains("\"\""),
            "double-quote in password must be doubled: {code}"
        );
        assert!(
            !code.contains("\\\""),
            "no backslash-quote in password: {code}"
        );
    }

    // ── user_action_code additional ───────────────────────────────────────────
    #[test]
    fn test_user_action_code_contains_user_action() {
        let code = user_action_code("CheckOut", "MyApp.cls", "user", "pass");
        assert!(
            code.contains("UserAction"),
            "must invoke UserAction: {code}"
        );
    }

    #[test]
    fn test_user_action_code_contains_source_menu() {
        let code = user_action_code("CheckOut", "MyApp.cls", "user", "pass");
        assert!(
            code.contains("%SourceMenu"),
            "must pass %SourceMenu prefix: {code}"
        );
    }

    #[test]
    fn test_user_action_code_escapes_quotes_in_credentials() {
        let code = user_action_code("CheckOut", "Doc.cls", "us\"er", "p\"ass");
        assert!(
            code.contains("\"\""),
            "double-quote in credentials must be doubled: {code}"
        );
        assert!(!code.contains("\\\""), "no backslash-quote: {code}");
    }

    // ── menu_all_items_code additional ────────────────────────────────────────
    #[test]
    fn test_menu_all_items_code_contains_source_menu() {
        let code = menu_all_items_code("MyApp.cls", "user", "pass");
        assert!(
            code.contains("%SourceMenu"),
            "must pass %SourceMenu to Execute: {code}"
        );
    }

    #[test]
    fn test_menu_all_items_code_escapes_quotes_in_doc() {
        let code = menu_all_items_code("My\"App.cls", "user", "pass");
        assert!(
            code.contains("\"\""),
            "double-quote in doc must be doubled: {code}"
        );
        assert!(!code.contains("\\\""), "no backslash-quote: {code}");
    }

    #[test]
    fn test_menu_all_items_code_contains_source_control_create() {
        let code = menu_all_items_code("MyApp.cls", "user", "pass");
        assert!(
            code.contains("SourceControlCreate"),
            "must init session: {code}"
        );
    }

    // ── after_user_action_code additional ────────────────────────────────────
    #[test]
    fn test_after_user_action_code_contains_source_menu() {
        let code = after_user_action_code("CheckOut", "MyApp.cls", "yes", "user", "pass");
        assert!(
            code.contains("%SourceMenu"),
            "must pass %SourceMenu: {code}"
        );
    }

    #[test]
    fn test_after_user_action_code_contains_user_action() {
        let code = after_user_action_code("CheckOut", "MyApp.cls", "yes", "user", "pass");
        assert!(
            code.contains("UserAction"),
            "must call UserAction first: {code}"
        );
    }

    #[test]
    fn test_after_user_action_code_writes_error_text() {
        let code = after_user_action_code("CheckOut", "MyApp.cls", "yes", "user", "pass");
        assert!(
            code.contains("GetErrorText"),
            "must write error text from AfterUserAction: {code}"
        );
    }

    // ── parse_action_msg edge cases ───────────────────────────────────────────
    #[test]
    fn test_parse_action_msg_empty_string() {
        // The callers now refuse on this rather than reading it as code 0 (#302, second half).
        assert_eq!(parse_action_msg(""), ActionMsg::Empty);
    }

    /// The record is found even when the hook's device left padding around it — IRIS writes with
    /// `write`, and what surrounds a line is not under our control.
    #[test]
    fn a_padded_record_line_is_still_found() {
        let out = format!(
            "   {}   ",
            record(r#"{"kind":"action","code":1,"msg":"some msg"}"#)
        );
        match parse_action_msg(&out) {
            ActionMsg::Record(r, _) => {
                assert_eq!(r.action_code(), Some(1));
                assert_eq!(r.msg, "some msg");
            }
            other => panic!("leading whitespace hid the record: {other:?}"),
        }
    }

    // ── ScmAction ─────────────────────────────────────────────────────────────
    #[test]
    fn test_scm_action_from_id() {
        assert_eq!(ScmAction::from_id("CheckOut"), ScmAction::CheckOut);
        assert_eq!(ScmAction::from_id("%CheckIn"), ScmAction::CheckIn);
        assert_eq!(ScmAction::from_id("%GetLatest"), ScmAction::GetLatest);
        assert_eq!(
            ScmAction::from_id("Unknown"),
            ScmAction::Unknown("Unknown".to_string())
        );
    }

    #[test]
    fn test_checkin_action_recognized() {
        assert_eq!(ScmAction::from_id("%CheckIn"), ScmAction::CheckIn);
        assert_eq!(ScmAction::from_id("CheckIn"), ScmAction::CheckIn);
    }

    #[test]
    fn test_iris_scm_allow_checkin_gate() {
        // Default: blocked
        std::env::remove_var("IRIS_SCM_ALLOW_CHECKIN");
        let allowed = std::env::var("IRIS_SCM_ALLOW_CHECKIN")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        assert!(!allowed, "CheckIn should be blocked by default");

        // Opt-in variants
        for val in &["1", "true", "yes", "TRUE", "YES"] {
            std::env::set_var("IRIS_SCM_ALLOW_CHECKIN", val);
            let allowed = std::env::var("IRIS_SCM_ALLOW_CHECKIN")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            assert!(
                allowed,
                "IRIS_SCM_ALLOW_CHECKIN={val} should enable CheckIn"
            );
        }

        // Explicitly disabled
        for val in &["0", "false", "no"] {
            std::env::set_var("IRIS_SCM_ALLOW_CHECKIN", val);
            let allowed = std::env::var("IRIS_SCM_ALLOW_CHECKIN")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            assert!(
                !allowed,
                "IRIS_SCM_ALLOW_CHECKIN={val} should keep CheckIn blocked"
            );
        }

        std::env::remove_var("IRIS_SCM_ALLOW_CHECKIN");
    }
}
