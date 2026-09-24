//! `iris_execute_method` — invoke a ClassMethod by name, without a throwaway wrapper class.
//!
//! The interop profile could compile a class and run SQL but not CALL A METHOD, so every
//! runtime measurement had to be wrapped in a scratch class carrying a `[SqlProc]` method:
//! put + compile + SELECT + delete. That pattern was built more than twenty times in one
//! session on the skills side, and three times the WRAPPER was the defect rather than the
//! subject — a `Quit <value>` inside a `Try` reported "all variants failed" (a clean false
//! negative), a wrapper that checked only `status.errors` missed a console-only compile
//! error and surfaced `<CLASS DOES NOT EXIST>`, and one `%New()`d an adapter outside a
//! business host and got `<INVALID OREF>` from the adapter's own logging.
//!
//! Upstream ships this tool with the documented limitation "only string-returning methods".
//! That limitation is the reason the `[SqlProc]` recipe existed in the first place — a
//! SqlProc can only hand back a string — so inheriting it would port the friction along
//! with the tool. Instead this reads the method's declared `ReturnType` from the class
//! dictionary first and interprets the result accordingly: a `%Status` comes back decoded,
//! an object comes back as its class name, and a method that returns nothing is invoked
//! with `do` rather than as an expression.

use crate::objectscript::os_str_expr;

/// What the class dictionary says about the method being invoked.
///
/// `found: false` is NOT a refusal. Dictionary reads are stale inside the process that
/// wrote the class (see #242: a class written AND COMPILED in this process still reads as
/// absent from `%Dictionary.*` until a later one), and "I just wrote this class, now call
/// it" is a normal sequence. So an empty lookup falls through to the expression form and
/// lets IRIS be the authority, rather than refusing a call that would have worked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MethodMeta {
    pub found: bool,
    pub return_type: String,
    pub is_class_method: bool,
}

/// One round trip for the three facts that change the generated call.
pub fn build_method_meta_query() -> &'static str {
    "SELECT ReturnType, ClassMethod FROM %Dictionary.CompiledMethod \
     WHERE parent = ? AND Name = ?"
}

/// `%Library.Status` and `%Status` are the same type; the dictionary stores the long form.
pub fn returns_status(return_type: &str) -> bool {
    let t = return_type
        .trim()
        .trim_start_matches('%')
        .to_ascii_lowercase();
    t == "status" || t == "library.status"
}

/// A method with no declared ReturnType must be invoked with `do`: used as an expression it
/// raises `<COMMAND>` on some versions and silently yields "" on others, and neither is a
/// result worth reporting.
pub fn returns_nothing(meta: &MethodMeta) -> bool {
    meta.found && meta.return_type.trim().is_empty()
}

pub fn parse_method_meta(body: &serde_json::Value) -> MethodMeta {
    let rows = match body["result"]["content"].as_array() {
        Some(r) if !r.is_empty() => r,
        _ => return MethodMeta::default(),
    };
    let row = &rows[0];
    let rt = row["ReturnType"].as_str().unwrap_or("").to_string();
    // THREE shapes, because the Atelier query endpoint really does return a JSON boolean here
    // and the other two forms appear on other driver paths. Measured on IRIS for Health
    // 2026.1: `{"Name":"GetFieldNameFromNumber","ReturnType":"%Library.String",
    // "ClassMethod":true}` — a bare `true`, not 1 and not "1".
    //
    // Handling only the numeric forms made EVERY ClassMethod fall through to false, so every
    // call was refused as "an instance method". The sibling helper this was modelled on reads
    // only 1/"1" for its own column, which is why the omission looked idiomatic.
    let cm = &row["ClassMethod"];
    let is_class_method =
        cm.as_bool() == Some(true) || cm.as_i64() == Some(1) || cm.as_str() == Some("1");
    MethodMeta {
        found: true,
        return_type: rt,
        is_class_method,
    }
}

/// Render one JSON argument as an ObjectScript expression.
///
/// Strings go through `os_str_expr`, which doubles quotes and splices control characters as
/// `$CHAR(...)` — never backslash escaping, which ObjectScript does not have. Numbers and
/// booleans are emitted bare so a method expecting a number is not handed the string "1".
pub fn arg_expr(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => os_str_expr(s),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        serde_json::Value::Null => "\"\"".to_string(),
        // An array or object has no ObjectScript literal. Pass the JSON text so a method
        // that parses JSON still works, rather than silently sending something else.
        other => os_str_expr(&other.to_string()),
    }
}

/// Field markers. Deliberately not tab-delimited-on-one-line like the sibling builders: a
/// return value may itself contain tabs and newlines, so each field gets its own line and
/// the VALUE goes last, preceded by its length. The length is what makes a truncated read
/// detectable — a value that arrives short otherwise looks like a successful short answer.
///
/// #347: `IEM_STATUS_TEXT` and `IEM_ERROR` are not line-oriented values either, and being written
/// as one line each is what dropped every `%Status` error after the first.
/// `$SYSTEM.Status.GetErrorText` returns a chain CRLF-joined, and `ex.DisplayString()` of a
/// `%Exception.StatusException` carrying a chain does the same (both measured on 2026.1 and 2025c —
/// 51 and 46 characters respectively, two pieces each). Both now repeat their marker on every line
/// and declare their character count, so the reader reassembles the whole field and can tell a short
/// arrival from a short answer. Every other field is single-line BY CONSTRUCTION: `IEM_KIND` is one
/// of four literals this generator writes, `IEM_STATUS_OK` is `0`/`1`/empty, and `IEM_RETURN_TYPE` is
/// a class name out of `%Dictionary.CompiledMethod`.
pub const M_KIND: &str = "IEM_KIND\t";
pub const M_RETURN_TYPE: &str = "IEM_RETURN_TYPE\t";
pub const M_STATUS_OK: &str = "IEM_STATUS_OK\t";
pub const M_STATUS_TEXT_LEN: &str = "IEM_STATUS_TEXT_LEN\t";
pub const M_STATUS_TEXT: &str = "IEM_STATUS_TEXT\t";
pub const M_ERROR_LEN: &str = "IEM_ERROR_LEN\t";
pub const M_ERROR: &str = "IEM_ERROR\t";
pub const M_VALUE_LEN: &str = "IEM_VALUE_LEN\t";
pub const M_VALUE: &str = "IEM_VALUE\t";

/// The ObjectScript written into the generator's RunUser().
pub fn build_invoke_code(
    class: &str,
    method: &str,
    args: &[serde_json::Value],
    meta: &MethodMeta,
) -> String {
    let arg_list: Vec<String> = args.iter().map(arg_expr).collect();
    let trailing = if arg_list.is_empty() {
        String::new()
    } else {
        format!(",{}", arg_list.join(","))
    };
    let call = format!(
        "$classmethod({cls},{meth}{trailing})",
        cls = os_str_expr(class),
        meth = os_str_expr(method),
    );

    // A void method cannot be read as an expression; anything else can.
    let invoke = if returns_nothing(meta) {
        format!("  do {call}\n  set tKind=\"void\"")
    } else {
        format!("  set tVal={call}")
    };

    // The status decode is driven by the DECLARED return type, not by guessing from the
    // value: "1" is a perfectly ordinary string return and must not be reported as an OK
    // status just because it looks like one.
    let status_block = if meta.found && returns_status(&meta.return_type) {
        // $SYSTEM.Status.IsOK is a real class method, deliberately NOT the $$$ISOK / $$$ISERR
        // macros: macro availability depends on the Include list of the enclosing class, and
        // the generated wrapper carries none. A class method has no such dependency.
        //
        // This line first read `set tOk=` followed by FOUR NOT operators and $$$ISERR — two
        // double-negations, so tOk came out equal to ISERR. A failing %Status would have been
        // reported as status_ok: true, the one direction of this that turns a failure into a
        // success. Asserting "GetErrorText is present" could not see it; the guard below can.
        "  set tKind=\"status\"\n  \
         set tOk=$SYSTEM.Status.IsOK(tVal)\n  \
         if 'tOk { set tStatusText=$SYSTEM.Status.GetErrorText(tVal) }"
    } else {
        ""
    };

    // The two free-text fields go out one marker line per line of their content, with their
    // character count declared in front. See M_STATUS_TEXT's doc comment for why (#347).
    let status_text_block = crate::objectscript::write_marker_lines(
        "tStatusText",
        M_STATUS_TEXT_LEN,
        M_STATUS_TEXT,
        crate::objectscript::Declared::Chars,
    );
    let error_block = crate::objectscript::write_marker_lines(
        "tErr",
        M_ERROR_LEN,
        M_ERROR,
        crate::objectscript::Declared::Chars,
    );

    format!(
        r#"  set tKind="value"
  set tVal=""
  set tOk=""
  set tStatusText=""
  set tErr=""
  try {{
{invoke}
  }} catch ex {{
    set tErr=ex.DisplayString()
  }}
  if $isobject(tVal) {{ set tKind="oref" set tVal=$classname(tVal) }}
{status_block}
  write "{M_KIND}"_tKind_$C(10)
  write "{M_RETURN_TYPE}"_{rt}_$C(10)
  write "{M_STATUS_OK}"_tOk_$C(10)
{status_text_block}
{error_block}
  write "{M_VALUE_LEN}"_$LENGTH(tVal)_$C(10)
  write "{M_VALUE}"_tVal_$C(10)"#,
        invoke = invoke,
        status_block = status_block,
        status_text_block = status_text_block,
        error_block = error_block,
        rt = os_str_expr(&meta.return_type),
        M_KIND = M_KIND,
        M_RETURN_TYPE = M_RETURN_TYPE,
        M_STATUS_OK = M_STATUS_OK,
        M_VALUE_LEN = M_VALUE_LEN,
        M_VALUE = M_VALUE,
    )
}

/// What came back from one invocation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InvokeResult {
    pub kind: String,
    pub return_type: String,
    pub value: String,
    /// Declared length of `value` as IRIS measured it, when reported.
    pub value_len: Option<usize>,
    /// True when `value` is shorter than IRIS said it was — the output was cut, and the
    /// value must not be presented as the method's answer.
    pub truncated: bool,
    pub status_ok: Option<bool>,
    /// The decoded `%Status` chain, its elements newline-joined — ALL of them, not just the first
    /// (#347).
    pub status_text: String,
    /// Declared character count of `status_text`, when reported.
    pub status_text_len: Option<usize>,
    /// True when less of the `%Status` chain arrived than IRIS declared. The third case: not a
    /// shorter error message, but an error message whose missing part may be the cause.
    pub status_text_truncated: bool,
    pub error: String,
    /// Declared character count of `error`, when reported.
    pub error_len: Option<usize>,
    /// True when less of the exception text arrived than IRIS declared.
    pub error_truncated: bool,
}

/// Cut `text` to the length IRIS declared and say whether it arrived SHORT.
///
/// IRIS reported the exact character count, so use it rather than guessing which trailing
/// whitespace belongs to the field and which is the write that terminated it. Stripping "one
/// trailing newline" left a spurious `\n` on every value, because the generator's own output adds
/// one of its own; cutting to the declared length cannot make that mistake.
///
/// A short arrival is returned rather than silently accepted: without it, a field that got cut is
/// indistinguishable from a field that was genuinely that short.
fn fit_to_declared(text: &mut String, declared: Option<usize>) -> bool {
    let Some(n) = declared else {
        return false;
    };
    let got = text.chars().count();
    if got > n {
        *text = text.chars().take(n).collect();
    }
    got < n
}

/// Parse the marker block. The VALUE marker is last, so everything after it (newlines
/// included) belongs to the value.
///
/// `IEM_STATUS_TEXT` and `IEM_ERROR` arrive as one marker line per line of their content and are
/// reassembled by joining with `\n`, then checked against the count their `..._LEN` marker declared
/// (#347). Before that they were read with a single `strip_prefix` assignment, which captured the
/// first line of a CRLF-joined `%Status` chain and dropped the rest on iterations that matched no arm.
pub fn parse_invoke_output(out: &str) -> InvokeResult {
    let mut r = InvokeResult::default();
    let value_at = out.find(M_VALUE);
    if let Some(i) = value_at {
        r.value = out[i + M_VALUE.len()..]
            .strip_suffix('\n')
            .unwrap_or(&out[i + M_VALUE.len()..])
            .to_string();
    }
    // Scan for the other fields only in front of the VALUE marker. Everything after it is the
    // method's own return value — arbitrary text, possibly multi-line — and a line of it beginning
    // with a marker literal would otherwise be read as that field.
    let fields = match value_at {
        Some(i) => &out[..i],
        None => out,
    };
    let mut status_text: Vec<&str> = Vec::new();
    let mut error: Vec<&str> = Vec::new();
    for line in fields.lines() {
        // The `..._LEN` arms come first so a declaration can never be mistaken for content. They
        // cannot collide today — the marker's separator is a tab and `_LEN`'s is an underscore —
        // but the ordering makes that independent of the separator choice.
        if let Some(v) = line.strip_prefix(M_STATUS_TEXT_LEN) {
            r.status_text_len = v.trim().parse::<usize>().ok();
        } else if let Some(v) = line.strip_prefix(M_ERROR_LEN) {
            r.error_len = v.trim().parse::<usize>().ok();
        } else if let Some(v) = line.strip_prefix(M_VALUE_LEN) {
            r.value_len = v.trim().parse::<usize>().ok();
        } else if let Some(v) = line.strip_prefix(M_KIND) {
            r.kind = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_RETURN_TYPE) {
            r.return_type = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_STATUS_OK) {
            let t = v.trim();
            if !t.is_empty() {
                r.status_ok = Some(t == "1");
            }
        } else if let Some(v) = line.strip_prefix(M_STATUS_TEXT) {
            // APPEND, never assign: one line per chain element.
            status_text.push(v);
        } else if let Some(v) = line.strip_prefix(M_ERROR) {
            error.push(v);
        }
    }
    r.status_text = status_text.join("\n");
    r.error = error.join("\n");
    r.truncated = fit_to_declared(&mut r.value, r.value_len);
    r.status_text_truncated = fit_to_declared(&mut r.status_text, r.status_text_len);
    r.error_truncated = fit_to_declared(&mut r.error, r.error_len);
    r
}

/// The tool-level outcome when the decoded `%Status` chain did not arrive whole (#347): error code
/// and message.
///
/// A named function rather than inline text in the handler, for the same reason
/// `coverage::Refusal::outcome` is one — the mapping from a parsed third case to what the caller is
/// told is otherwise reachable only through a live connection, and an untested third case is worth as
/// much as none.
pub fn status_text_truncated_report(
    class: &str,
    method: &str,
    declared: usize,
    received: usize,
) -> (&'static str, String) {
    (
        "STATUS_TEXT_TRUNCATED",
        format!(
            "'{class}::{method}' returned a failing %Status whose decoded text IRIS measured at \
             {declared} characters, but only {received} arrived — so the error chain is incomplete \
             and is not reported as the status text. On a chained %Status the first element is often \
             a generic wrapper and the specific cause is further down, so the part that is missing \
             may be the part you need. What did arrive is in `status_text_partial`."
        ),
    )
}

/// The message for a method the dictionary says is an INSTANCE method. Naming the
/// distinction matters: `$classmethod` on an instance method fails with a bare
/// `<METHOD DOES NOT EXIST>`, which reads as "wrong name" and sends the caller looking for
/// a typo that is not there.
pub fn instance_method_message(class: &str, method: &str) -> String {
    format!(
        "'{class}::{method}' is an instance method, not a ClassMethod, so it cannot be \
         invoked without an object. Create the instance and call it from iris_execute, or \
         name a ClassMethod here."
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IrisExecuteMethodParams {
    /// Class that declares the ClassMethod, e.g. EnsLib.HL7.Schema.
    pub class: String,
    /// ClassMethod to invoke, e.g. GetFieldNameFromNumber. Must be a ClassMethod: an instance
    /// method needs an object and is refused with a message saying so.
    pub method: String,
    /// Positional arguments in declaration order. A string, number or boolean is passed with
    /// its own type, so a method expecting a number is not handed the text "1"; an array or
    /// object is passed as its JSON text.
    #[serde(default)]
    pub args: Vec<serde_json::Value>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn handle_iris_execute_method(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: IrisExecuteMethodParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let class = p.class.trim();
    let method = p.method.trim();
    if class.is_empty() || method.is_empty() {
        return crate::tools::envelope::fail_with(
            "MISSING_PARAMS",
            "Both 'class' and 'method' are required and neither may be blank. Nothing was run.",
            serde_json::json!({ "class": p.class, "method": p.method }),
        );
    }

    // Ask the dictionary what this method returns, so the invocation and the interpretation
    // match the declaration rather than a guess about the value. A failed or empty lookup is
    // NOT fatal: the read is stale inside the process that wrote the class, and "write the
    // class then call it" is an ordinary sequence, so fall through and let IRIS decide.
    let meta = match iris
        .query(
            build_method_meta_query(),
            vec![
                serde_json::Value::String(class.to_string()),
                serde_json::Value::String(method.to_string()),
            ],
            &namespace,
            client,
        )
        .await
    {
        Ok(body) => parse_method_meta(&body),
        Err(e) => {
            tracing::debug!("method meta lookup failed for {class}::{method}: {e}");
            MethodMeta::default()
        }
    };

    // Only refuse when the dictionary POSITIVELY says it is an instance method. `$classmethod`
    // on one fails with a bare <METHOD DOES NOT EXIST>, which reads as a typo.
    if meta.found && !meta.is_class_method {
        return crate::tools::envelope::fail_with(
            "NOT_A_CLASS_METHOD",
            &instance_method_message(class, method),
            serde_json::json!({
                "class": class, "method": method, "namespace": namespace,
                "return_type": meta.return_type,
            }),
        );
    }

    let code = build_invoke_code(class, method, &p.args, &meta);
    let out = match iris.execute_via_generator(&code, &namespace, client).await {
        Ok(o) => o,
        Err(e) => {
            return crate::tools::envelope::fail_with(
                "EXECUTE_FAILED",
                &format!("Could not run '{class}::{method}' in namespace '{namespace}': {e}"),
                serde_json::json!({ "class": class, "method": method, "namespace": namespace }),
            )
        }
    };

    let r = parse_invoke_output(&out);

    match classify(&r) {
        Outcome::Threw => report_threw(class, method, &namespace, &r),
        Outcome::ValueTruncated => report_value_truncated(class, method, &namespace, &r),
        Outcome::StatusTextUnreassembled => {
            report_status_text_unreassembled(class, method, &namespace, &r)
        }
        Outcome::Answered => crate::tools::envelope::ok_json(answered_payload(
            class,
            method,
            &namespace,
            &r,
            p.args.len(),
        )),
    }
}

/// What one successful invocation reports.
///
/// Pure, and separate from the async handler for the same reason [`classify`] is: the `status` key's
/// presence rule (#323) is only testable without a connection if the payload can be built without
/// one. The handler had no test over this arm at all.
pub fn answered_payload(
    class: &str,
    method: &str,
    namespace: &str,
    r: &InvokeResult,
    args_passed: usize,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "success": true,
        "class": class,
        "method": method,
        "namespace": namespace,
        // "value" | "status" | "oref" | "void" — what the DECLARED return type made this.
        "kind": r.kind,
        "return_type": r.return_type,
        "value": r.value,
        "value_len": r.value_len,
        // Present only for a method declared to return %Status: the decoded verdict, so the
        // caller does not have to recognise a status string by eye.
        "status_ok": r.status_ok,
        "status_text": r.status_text,
        "args_passed": args_passed,
    });
    // #323: the chain decoded into per-error {code, text}, so a caller keys on the number instead of
    // matching a substring of the joined text. INSERTED rather than written into the literal above:
    // `json!` turns a `None` into an explicit `null`, and a `status` key present-and-null on every
    // method that does not return a %Status reads as "there was a status and it was nothing" — the
    // absent case has to be absent.
    if let Some(status) = status_block(r) {
        payload["status"] = status;
    }
    payload
}

/// #323: the `status` block for an invocation — the chain split into per-error `{code, text}`.
///
/// Unlike `iris_execute`, which sees only what a script chose to print, this path reads IRIS's own
/// `$$$ISOK` verdict off the generated program (`InvokeResult::status_ok`). So here an OK status is
/// a MEASURED fact rather than an absence, and `Some(true)` is reported as `ok: true`.
///
/// `Some(false)` goes through [`crate::status::decode_known_error`] and never through the plain
/// decoder: a failing status whose text this build cannot parse must not come back as "no status
/// here". `None` — any method whose declared return type is not `%Status` — carries no block.
pub fn status_block(r: &InvokeResult) -> Option<serde_json::Value> {
    match r.status_ok {
        None => None,
        Some(true) => crate::status::StatusChain::Ok.payload(),
        Some(false) => crate::status::decode_known_error(&r.status_text).payload(),
    }
}

/// What one invocation amounts to, decided from the parsed marker block alone.
///
/// A named enum rather than a chain of `if`s inside the async handler, so the precedence AND the rule
/// that an unreassembled field is its OWN outcome are testable without a connection. The `if
/// r.truncated` this replaces had no test at all: reaching it needs a live short read, which is
/// exactly the shape of branch that gets written once and never exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The method raised. Its text may itself have arrived incomplete.
    Threw,
    /// IRIS measured the return value longer than what arrived.
    ValueTruncated,
    /// IRIS measured the decoded `%Status` chain longer than what arrived (#347).
    StatusTextUnreassembled,
    /// Everything arrived. Report the result.
    Answered,
}

/// The precedence: what the METHOD did comes before what the transport did to it, and a value that
/// did not arrive comes before a status text that did not, because the value is the answer.
pub fn classify(r: &InvokeResult) -> Outcome {
    if !r.error.is_empty() {
        Outcome::Threw
    } else if r.truncated {
        Outcome::ValueTruncated
    } else if r.status_text_truncated {
        Outcome::StatusTextUnreassembled
    } else {
        Outcome::Answered
    }
}

/// A `<...>` thrown inside the method is the method's answer, not a transport failure, so it is
/// reported as a named outcome rather than swallowed into the value.
fn report_threw(
    class: &str,
    method: &str,
    namespace: &str,
    r: &InvokeResult,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    // `ex.DisplayString()` of a %Exception.StatusException carrying a chained %Status is
    // CRLF-joined, so the exception text is multi-line too (#347). If less of it arrived than
    // IRIS declared, say so — a prefix of an exception message reads exactly like the whole one.
    let cut = if r.error_truncated {
        format!(
            " [INCOMPLETE: IRIS declared {} characters of exception text and {} arrived, so the \
             text above is a PREFIX, not the whole exception]",
            r.error_len.unwrap_or(0),
            r.error.chars().count(),
        )
    } else {
        String::new()
    };
    crate::tools::envelope::fail_with(
        "METHOD_THREW",
        &format!("'{class}::{method}' raised: {}{cut}", r.error),
        serde_json::json!({
            "class": class, "method": method, "namespace": namespace,
            "return_type": r.return_type, "error_detail": r.error,
            "error_detail_complete": !r.error_truncated,
            "error_detail_len": r.error_len,
        }),
    )
}

/// IRIS measured the value longer than what arrived. Reporting the short value as the answer is the
/// failure mode worth refusing outright.
fn report_value_truncated(
    class: &str,
    method: &str,
    namespace: &str,
    r: &InvokeResult,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    crate::tools::envelope::fail_with(
        "VALUE_TRUNCATED",
        &format!(
            "'{class}::{method}' returned {} characters but only {} arrived, so the value \
             is incomplete and is not reported as the result.",
            r.value_len.unwrap_or(0),
            r.value.chars().count()
        ),
        serde_json::json!({
            "class": class, "method": method, "namespace": namespace,
            "value_len": r.value_len, "received_len": r.value.chars().count(),
        }),
    )
}

/// The same refusal, for the decoded `%Status` chain (#347). A chain whose later elements did not
/// arrive is not a shorter error message: on a start or checkout failure the first element is
/// frequently a generic wrapper and the specific cause is the one further down, so reporting the
/// prefix as the status text would hand the caller the least useful half and call it complete.
fn report_status_text_unreassembled(
    class: &str,
    method: &str,
    namespace: &str,
    r: &InvokeResult,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let (code, message) = status_text_truncated_report(
        class,
        method,
        r.status_text_len.unwrap_or(0),
        r.status_text.chars().count(),
    );
    crate::tools::envelope::fail_with(
        code,
        &message,
        serde_json::json!({
            "class": class, "method": method, "namespace": namespace,
            "kind": r.kind, "return_type": r.return_type,
            "status_ok": r.status_ok,
            "status_text_len": r.status_text_len,
            "received_len": r.status_text.chars().count(),
            // NOT `status_text`: the caller must not read a prefix under the name of the whole field.
            "status_text_partial": r.status_text,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_return_type_is_recognised_in_both_spellings() {
        assert!(returns_status("%Status"));
        assert!(returns_status("%Library.Status"));
        assert!(returns_status("status"));
        // Not a status: these must not be decoded as one.
        assert!(!returns_status("%String"));
        assert!(!returns_status("%Integer"));
        assert!(!returns_status(""));
        assert!(!returns_status("%StatusReport"));
    }

    #[test]
    fn a_void_method_is_invoked_with_do_not_as_an_expression() {
        let meta = MethodMeta {
            found: true,
            return_type: String::new(),
            is_class_method: true,
        };
        let code = build_invoke_code("App.Util", "Reset", &[], &meta);
        assert!(code.contains("do $classmethod("), "{code}");
        assert!(
            !code.contains("set tVal=$classmethod("),
            "a void method read as an expression raises <COMMAND>: {code}"
        );
        assert!(code.contains("set tKind=\"void\""), "{code}");
    }

    /// The whole point of the dictionary lookup: `"1"` returned as a %String must not be
    /// reported as an OK %Status.
    #[test]
    fn a_string_return_is_never_decoded_as_a_status() {
        let meta = MethodMeta {
            found: true,
            return_type: "%String".into(),
            is_class_method: true,
        };
        let code = build_invoke_code("App.Util", "Name", &[], &meta);
        assert!(
            !code.contains("$SYSTEM.Status.GetErrorText"),
            "a %String return must not go through the status decode: {code}"
        );
    }

    #[test]
    fn a_status_return_is_decoded() {
        let meta = MethodMeta {
            found: true,
            return_type: "%Status".into(),
            is_class_method: true,
        };
        let code = build_invoke_code("Ens.Director", "StartProduction", &[], &meta);
        assert!(code.contains("$SYSTEM.Status.GetErrorText"), "{code}");
        assert!(code.contains("set tKind=\"status\""), "{code}");
    }

    /// The OK flag is read by asking IsOK, never derived by negating ISERR an even number of
    /// times. The first version used four NOT operators and therefore set tOk = ISERR: a
    /// failing status reported as OK. No assertion on "GetErrorText is present" could see it.
    #[test]
    fn the_status_ok_flag_is_not_inverted() {
        let meta = MethodMeta {
            found: true,
            return_type: "%Status".into(),
            is_class_method: true,
        };
        let code = build_invoke_code("Ens.Director", "StartProduction", &[], &meta);
        assert!(
            code.contains("set tOk=$SYSTEM.Status.IsOK(tVal)"),
            "OK must be read directly, not derived by negation: {code}"
        );
        // Two or more stacked NOT operators is a double negation — how the inversion got in.
        assert!(
            !code.contains("''"),
            "stacked NOT operators in generated code: {code}"
        );
    }

    /// Macro availability depends on the enclosing class's Include list, and the generated
    /// wrapper has none — so the generated code must not depend on a macro at all.
    #[test]
    fn the_generated_code_uses_no_dollar_macros() {
        for rt in ["%Status", "%String", ""] {
            let meta = MethodMeta {
                found: true,
                return_type: rt.into(),
                is_class_method: true,
            };
            let code = build_invoke_code("App.U", "Go", &[], &meta);
            assert!(
                !code.contains("$$$"),
                "return type {rt:?} emitted a macro the wrapper may not carry: {code}"
            );
        }
    }

    /// An unknown method (dictionary stale per #242, or simply not compiled) must still
    /// produce a call — refusing here would break "write the class, then call it".
    #[test]
    fn an_unfound_method_still_generates_a_call() {
        let code = build_invoke_code("App.New", "Probe", &[], &MethodMeta::default());
        assert!(code.contains("set tVal=$classmethod("), "{code}");
        assert!(!code.contains("do $classmethod("), "{code}");
    }

    #[test]
    fn arguments_are_typed_not_all_stringified() {
        assert_eq!(arg_expr(&serde_json::json!("PID")), "\"PID\"");
        assert_eq!(arg_expr(&serde_json::json!(2)), "2");
        assert_eq!(arg_expr(&serde_json::json!(true)), "1");
        assert_eq!(arg_expr(&serde_json::json!(false)), "0");
        assert_eq!(arg_expr(&serde_json::json!(null)), "\"\"");
    }

    /// ObjectScript has no backslash escape — a quote is doubled. Getting this wrong is how
    /// generated code turns into a syntax error the caller cannot see.
    #[test]
    fn a_quote_in_an_argument_is_doubled_never_backslashed() {
        let e = arg_expr(&serde_json::json!(r#"say "hi""#));
        assert!(e.contains("\"\""), "quotes must be doubled: {e}");
        assert!(
            !e.contains('\\'),
            "no backslash escaping in ObjectScript: {e}"
        );
    }

    #[test]
    fn the_arg_list_is_comma_joined_after_class_and_method() {
        let code = build_invoke_code(
            "EnsLib.HL7.Schema",
            "GetFieldNameFromNumber",
            &[
                serde_json::json!("2.5"),
                serde_json::json!("PV1"),
                serde_json::json!(2),
            ],
            &MethodMeta::default(),
        );
        assert!(
            code.contains(
                "$classmethod(\"EnsLib.HL7.Schema\",\"GetFieldNameFromNumber\",\"2.5\",\"PV1\",2)"
            ),
            "{code}"
        );
    }

    #[test]
    fn no_args_produces_no_trailing_comma() {
        let code = build_invoke_code("App.U", "Go", &[], &MethodMeta::default());
        assert!(code.contains("$classmethod(\"App.U\",\"Go\")"), "{code}");
    }

    /// The fixture is the shape `build_invoke_code` actually emits, declaration lines included.
    #[test]
    fn the_parser_reads_every_field() {
        let out = "IEM_KIND\tstatus\nIEM_RETURN_TYPE\t%Status\nIEM_STATUS_OK\t0\n\
                   IEM_STATUS_TEXT_LEN\t17\nIEM_STATUS_TEXT\tERROR #5001: boom\n\
                   IEM_ERROR_LEN\t0\nIEM_ERROR\t\nIEM_VALUE_LEN\t3\n\
                   IEM_VALUE\t0 e\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.kind, "status");
        assert_eq!(r.return_type, "%Status");
        assert_eq!(r.status_ok, Some(false));
        assert_eq!(r.status_text, "ERROR #5001: boom");
        assert!(!r.status_text_truncated);
        assert_eq!(r.error, "");
        assert_eq!(r.value, "0 e");
        assert_eq!(r.value_len, Some(3));
        assert!(!r.truncated);
    }

    /// #347. The lengths are the MEASURED ones: on IRIS 2026.1 and 2025c a two-element chain built
    /// with `$system.Status.AppendStatus` decodes to 51 characters CRLF-joined, which is 50 once the
    /// CR is removed — 24 + 1 + 25.
    #[test]
    fn every_element_of_a_status_chain_is_reassembled() {
        let out = "IEM_KIND\tstatus\nIEM_RETURN_TYPE\t%Library.Status\nIEM_STATUS_OK\t0\n\
                   IEM_STATUS_TEXT_LEN\t50\n\
                   IEM_STATUS_TEXT\tERROR #5001: first cause\n\
                   IEM_STATUS_TEXT\tERROR #5001: second cause\n\
                   IEM_ERROR_LEN\t0\nIEM_ERROR\t\nIEM_VALUE_LEN\t0\nIEM_VALUE\t\n";
        let r = parse_invoke_output(out);
        assert_eq!(
            r.status_text, "ERROR #5001: first cause\nERROR #5001: second cause",
            "both elements must arrive, newline-joined"
        );
        assert_eq!(r.status_text_len, Some(50));
        assert!(
            !r.status_text_truncated,
            "50 declared, 50 present: {:?}",
            r.status_text
        );
    }

    /// The third case. A chain whose later elements did not arrive is not a shorter error message —
    /// it must be reportable as unreassembled, or the caller acts on a prefix that looks whole.
    #[test]
    fn a_status_chain_that_arrives_short_is_flagged_not_silently_shortened() {
        let out = "IEM_STATUS_TEXT_LEN\t50\nIEM_STATUS_TEXT\tERROR #5001: first cause\n\
                   IEM_VALUE_LEN\t0\nIEM_VALUE\t\n";
        let r = parse_invoke_output(out);
        assert!(
            r.status_text_truncated,
            "IRIS declared 50 characters of chain and 24 arrived: {r:?}"
        );
        assert_eq!(
            r.status_text, "ERROR #5001: first cause",
            "what arrived is still reported, as the partial it is"
        );
    }

    /// The sibling field. `ex.DisplayString()` of a %Exception.StatusException carrying a chained
    /// %Status is CRLF-joined too — measured at 46 characters for a two-element chain, 45 with the CR
    /// removed (22 + 1 + 22). Fixing only `status_text` would have left this one looking correct.
    #[test]
    fn every_line_of_a_thrown_exception_is_reassembled() {
        let out = "IEM_KIND\tvalue\nIEM_STATUS_TEXT_LEN\t0\nIEM_STATUS_TEXT\t\n\
                   IEM_ERROR_LEN\t45\n\
                   IEM_ERROR\tERROR #5001: throw one\n\
                   IEM_ERROR\tERROR #5001: throw two\n\
                   IEM_VALUE_LEN\t0\nIEM_VALUE\t\n";
        let r = parse_invoke_output(out);
        assert_eq!(
            r.error, "ERROR #5001: throw one\nERROR #5001: throw two",
            "the exception text is a chain as well"
        );
        assert!(!r.error_truncated, "{r:?}");
    }

    /// A field with no declaration must be reported as it arrived, NOT cut to zero. This is the
    /// direction a defaulted length would break: `unwrap_or(0)` would empty every field.
    #[test]
    fn an_undeclared_field_is_kept_whole_rather_than_cut() {
        let out = "IEM_STATUS_TEXT\tERROR #5001: boom\nIEM_VALUE\tx\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.status_text, "ERROR #5001: boom");
        assert_eq!(r.status_text_len, None);
        assert!(
            !r.status_text_truncated,
            "absence of a count is not a short read"
        );
    }

    /// The VALUE field is arbitrary method output and comes last on purpose. A line of it that
    /// begins with another marker's literal must stay part of the value — reading it as that field
    /// is how a truncation would have become a corruption.
    ///
    /// The canary is `IEM_KIND`, and deliberately not `IEM_ERROR`. `IEM_ERROR` HAS a declared length,
    /// and when the first version of this test injected an `IEM_ERROR` line into the value the
    /// unbounded-scan mutant SURVIVED: the injected text was appended, then cut back to the declared
    /// 0 characters, and `error.is_empty()` passed for entirely the wrong reason. `IEM_KIND` carries
    /// no declaration, so nothing can mask an overwrite of it.
    #[test]
    fn a_marker_literal_inside_the_value_is_not_read_as_a_field() {
        let out = "IEM_KIND\tvalue\nIEM_STATUS_TEXT_LEN\t0\nIEM_STATUS_TEXT\t\n\
                   IEM_ERROR_LEN\t0\nIEM_ERROR\t\n\
                   IEM_VALUE_LEN\t47\nIEM_VALUE\tline1\nIEM_KIND\tcorrupted\n\
                   IEM_ERROR\tnot an error\n";
        let r = parse_invoke_output(out);
        assert_eq!(
            r.kind, "value",
            "a line of the VALUE overwrote the kind field: {r:?}"
        );
        assert!(
            r.error.is_empty(),
            "a line of the VALUE was parsed as the error field: {r:?}"
        );
        assert_eq!(
            r.value, "line1\nIEM_KIND\tcorrupted\nIEM_ERROR\tnot an error",
            "the value must arrive verbatim, marker literals and all"
        );
        assert!(!r.truncated, "{r:?}");
    }

    /// Codegen. Both assertions name the marker WITH its tab: a bare `"IEM_STATUS_TEXT"` is also a
    /// prefix of `IEM_STATUS_TEXT_LEN`, so a check without the separator would be satisfied by the
    /// declaration line alone and stay green with every per-line write deleted.
    #[test]
    fn the_status_chain_is_written_one_marker_line_per_line() {
        let meta = MethodMeta {
            found: true,
            return_type: "%Status".into(),
            is_class_method: true,
        };
        let code = build_invoke_code("Ens.Director", "StartProduction", &[], &meta);
        assert!(
            code.contains("write \"IEM_STATUS_TEXT_LEN\"_$CHAR(9)_$LENGTH(tStatusText)_$CHAR(10)"),
            "the chain's character count must be declared: {code}"
        );
        assert!(
            code.contains("$PIECE(tStatusText,$CHAR(10),tMLI)"),
            "the chain must be written one piece per line, or elements 2..n are lost: {code}"
        );
        assert!(
            code.contains("write \"IEM_ERROR_LEN\"_$CHAR(9)_$LENGTH(tErr)_$CHAR(10)")
                && code.contains("$PIECE(tErr,$CHAR(10),tMLI)"),
            "the exception text is a chain too and gets the same treatment: {code}"
        );
        // CR must be removed before the count is taken, or the declaration counts characters the
        // reader never sees and every chain reports as a short read.
        assert!(
            code.contains("set tStatusText=$TRANSLATE(tStatusText,$CHAR(13))"),
            "{code}"
        );
    }

    /// A value containing newlines is why the VALUE marker is last and length-prefixed.
    #[test]
    fn a_multiline_value_survives_and_is_not_split() {
        let out = "IEM_KIND\tvalue\nIEM_RETURN_TYPE\t%String\nIEM_STATUS_OK\t\n\
                   IEM_STATUS_TEXT\t\nIEM_ERROR\t\nIEM_VALUE_LEN\t11\nIEM_VALUE\tline1\nline2\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.value, "line1\nline2");
        assert!(!r.truncated, "11 chars expected, 11 present");
    }

    /// The value is cut to the length IRIS declared, so the newline that terminates the write
    /// never arrives as part of the answer. Before this, every value came back with a trailing
    /// \n — "patientclass\n" instead of "patientclass" — which a caller comparing strings
    /// would see as a mismatch with no visible cause.
    #[test]
    fn the_value_is_cut_to_the_length_iris_declared() {
        let out = "IEM_VALUE_LEN\t12\nIEM_VALUE\tpatientclass\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.value, "patientclass", "no trailing newline may survive");
        assert!(!r.truncated);
    }

    /// Cutting to length must count CHARACTERS, not bytes, or a multibyte value is corrupted.
    #[test]
    fn cutting_to_length_counts_characters_not_bytes() {
        let out = "IEM_VALUE_LEN\t3\nIEM_VALUE\taño\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.value, "año");
        assert!(!r.truncated);
    }

    /// The length exists to catch exactly this: a short read must not pass as the answer.
    #[test]
    fn a_short_value_is_reported_as_truncated() {
        let out = "IEM_KIND\tvalue\nIEM_VALUE_LEN\t500\nIEM_VALUE\tonly this much\n";
        let r = parse_invoke_output(out);
        assert!(
            r.truncated,
            "IRIS said 500 characters and 14 arrived: {r:?}"
        );
    }

    #[test]
    fn an_absent_status_field_stays_none_rather_than_false() {
        let out = "IEM_KIND\tvalue\nIEM_STATUS_OK\t\nIEM_VALUE_LEN\t1\nIEM_VALUE\tx\n";
        let r = parse_invoke_output(out);
        assert_eq!(
            r.status_ok, None,
            "a non-status method has no status, which is not the same as a failed one"
        );
    }

    /// The WIRING. `classify` exists because the `if r.status_text_truncated` it replaced survived a
    /// mutation: the mapping was tested, the decision to use it was not, and a handler `if` reachable
    /// only through a live short read is a branch nothing ever runs.
    #[test]
    fn an_unreassembled_status_chain_is_its_own_outcome_not_an_answer() {
        let mut r = InvokeResult {
            kind: "status".into(),
            status_ok: Some(false),
            status_text: "ERROR #5001: first cause".into(),
            status_text_len: Some(50),
            status_text_truncated: true,
            ..Default::default()
        };
        assert_eq!(
            classify(&r),
            Outcome::StatusTextUnreassembled,
            "a chain that did not arrive whole must not be reported as an answer"
        );
        assert_ne!(classify(&r), Outcome::Answered);

        // CONTROL: the same result with the chain complete IS an answer. Without this, "classify
        // always returns StatusTextUnreassembled" would satisfy the assertion above.
        r.status_text_truncated = false;
        assert_eq!(classify(&r), Outcome::Answered);
    }

    /// Precedence. What the METHOD did outranks what the transport did to the report of it, and a
    /// value that did not arrive outranks a status text that did not, because the value is the answer.
    #[test]
    fn the_outcome_precedence_puts_the_method_first_and_the_value_before_the_status_text() {
        let threw = InvokeResult {
            error: "<UNDEFINED> zzz".into(),
            truncated: true,
            status_text_truncated: true,
            ..Default::default()
        };
        assert_eq!(classify(&threw), Outcome::Threw);

        let cut_value = InvokeResult {
            truncated: true,
            status_text_truncated: true,
            ..Default::default()
        };
        assert_eq!(classify(&cut_value), Outcome::ValueTruncated);

        assert_eq!(classify(&InvokeResult::default()), Outcome::Answered);
    }

    /// The third case must reach the caller as its OWN code, not as a shorter status text under
    /// `success: true`, and the message must carry both numbers so the caller can see how much is
    /// missing. Both halves asserted: a right message under the wrong code is still a false answer.
    #[test]
    fn an_unreassembled_status_chain_gets_its_own_error_code_and_both_numbers() {
        let (code, msg) = status_text_truncated_report("Ens.Director", "StartProduction", 50, 24);
        assert_eq!(code, "STATUS_TEXT_TRUNCATED");
        assert_ne!(
            code, "VALUE_TRUNCATED",
            "the value and the status text are different fields"
        );
        assert!(msg.contains("50") && msg.contains("24"), "{msg}");
        assert!(msg.contains("Ens.Director::StartProduction"), "{msg}");
        assert!(
            msg.contains("status_text_partial"),
            "the caller must be told where the partial went: {msg}"
        );
    }

    #[test]
    fn an_instance_method_is_named_as_such() {
        let m = instance_method_message("App.Data.Rec", "Save");
        assert!(m.contains("instance method"), "{m}");
        assert!(m.contains("App.Data.Rec::Save"), "{m}");
    }

    #[test]
    /// All three shapes the ClassMethod column arrives in. The BOOLEAN is what the Atelier
    /// query endpoint actually sends — measured on a live instance, not assumed — and it was
    /// the shape originally missing, which made every ClassMethod read as an instance method
    /// and refused every call. The earlier version of this test covered 1 and "1", passed, and
    /// the tool was nonetheless broken against every real instance.
    fn method_meta_parses_all_three_class_method_shapes() {
        let boolean =
            serde_json::json!({"result":{"content":[{"ReturnType":"%Status","ClassMethod":true}]}});
        let numeric =
            serde_json::json!({"result":{"content":[{"ReturnType":"%Status","ClassMethod":1}]}});
        let stringy =
            serde_json::json!({"result":{"content":[{"ReturnType":"%Status","ClassMethod":"1"}]}});
        for b in [boolean, numeric, stringy] {
            let m = parse_method_meta(&b);
            assert!(m.found, "{b}");
            assert!(m.is_class_method, "{b}");
            assert_eq!(m.return_type, "%Status");
        }
    }

    /// The false side must work in every shape too, or a genuine instance method slips through
    /// to `$classmethod` and fails with a bare <METHOD DOES NOT EXIST>.
    #[test]
    fn an_instance_method_reads_as_false_in_every_shape() {
        for v in [
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!("0"),
        ] {
            let b = serde_json::json!({"result":{"content":[{"ReturnType":"%String","ClassMethod":v}]}});
            let m = parse_method_meta(&b);
            assert!(m.found, "{b}");
            assert!(!m.is_class_method, "{b}");
        }
    }

    #[test]
    fn an_empty_dictionary_answer_is_not_found_and_not_a_class_method_claim() {
        let m = parse_method_meta(&serde_json::json!({"result":{"content":[]}}));
        assert!(!m.found);
        assert!(!m.is_class_method, "absence must not assert either way");
    }

    // ── #323: the decoded %Status block ─────────────────────────────────────────

    fn status_result(status_ok: Option<bool>, status_text: &str) -> InvokeResult {
        InvokeResult {
            kind: if status_ok.is_some() {
                "status"
            } else {
                "value"
            }
            .into(),
            return_type: if status_ok.is_some() {
                "%Status"
            } else {
                "%String"
            }
            .into(),
            status_ok,
            status_text: status_text.into(),
            ..Default::default()
        }
    }

    /// Measured on IRIS 2026.1: a two-element chain built with `$system.Status.AppendStatus`,
    /// reassembled by `parse_invoke_output` with its elements newline-joined (#347).
    const CHAIN: &str = "ERROR #5002: ObjectScript error: first problem\n\
                         ERROR #6301: SAX XML Parser Error: second problem";

    /// The whole point: per-error CODES, so a caller keys a remedy on 6301 rather than matching a
    /// substring of the joined text.
    #[test]
    fn a_failing_status_is_reported_element_by_element_with_its_codes() {
        let p = answered_payload("Pkg.C", "M", "APP", &status_result(Some(false), CHAIN), 0);
        assert_eq!(p["status"]["ok"], false);
        let codes: Vec<u64> = p["status"]["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .map(|e| e["code"].as_u64().expect("a code"))
            .collect();
        assert_eq!(codes, vec![5002, 6301]);
        assert_eq!(
            p["status"]["errors"][1]["text"],
            "SAX XML Parser Error: second problem"
        );
        // The raw field stays, so nothing this build cannot parse is lost.
        assert_eq!(p["status_text"], CHAIN);
    }

    /// Here — and only here — `ok: true` is a MEASURED fact: it comes off IRIS's own `$$$ISOK` in
    /// the generated program, not from failing to find an error marker.
    #[test]
    fn an_ok_status_is_reported_as_ok_because_iris_said_so_not_because_the_text_was_empty() {
        let p = answered_payload("Pkg.C", "M", "APP", &status_result(Some(true), ""), 0);
        assert_eq!(p["status"]["ok"], true);
        assert_eq!(p["status"]["errors"].as_array().map(|a| a.len()), Some(0));
    }

    /// A method whose declared return type is not `%Status` must carry NO `status` key — not a key
    /// set to null, which reads as "there was a status and it was nothing".
    #[test]
    fn a_method_that_returns_no_status_carries_no_status_key_at_all() {
        let p = answered_payload("Pkg.C", "M", "APP", &status_result(None, ""), 0);
        assert!(
            p.get("status").is_none(),
            "expected the key to be ABSENT, got {:?}",
            p.get("status")
        );
        // Control: the same builder DOES attach one when there is a status, so the assertion above
        // is not passing because nothing is ever attached.
        let with = answered_payload("Pkg.C", "M", "APP", &status_result(Some(false), CHAIN), 0);
        assert!(with.get("status").is_some());
    }

    /// A status IRIS called an error, whose text this build cannot parse, must not lose the verdict.
    /// Reporting no block would say "no status was involved" about a failure (#310).
    #[test]
    fn a_failing_status_with_unparseable_text_still_reports_a_failing_status() {
        let p = answered_payload(
            "Pkg.C",
            "M",
            "APP",
            &status_result(Some(false), "no marker anywhere in here"),
            0,
        );
        assert_eq!(p["status"]["ok"], false);
        assert_eq!(p["status"]["complete"], false);
        assert_eq!(p["status"]["undecoded"][0], "no marker anywhere in here");
    }
}
