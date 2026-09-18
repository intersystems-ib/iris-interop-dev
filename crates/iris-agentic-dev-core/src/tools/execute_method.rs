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
pub const M_KIND: &str = "IEM_KIND\t";
pub const M_RETURN_TYPE: &str = "IEM_RETURN_TYPE\t";
pub const M_STATUS_OK: &str = "IEM_STATUS_OK\t";
pub const M_STATUS_TEXT: &str = "IEM_STATUS_TEXT\t";
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
  write "{M_STATUS_TEXT}"_tStatusText_$C(10)
  write "{M_ERROR}"_tErr_$C(10)
  write "{M_VALUE_LEN}"_$LENGTH(tVal)_$C(10)
  write "{M_VALUE}"_tVal_$C(10)"#,
        invoke = invoke,
        status_block = status_block,
        rt = os_str_expr(&meta.return_type),
        M_KIND = M_KIND,
        M_RETURN_TYPE = M_RETURN_TYPE,
        M_STATUS_OK = M_STATUS_OK,
        M_STATUS_TEXT = M_STATUS_TEXT,
        M_ERROR = M_ERROR,
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
    pub status_text: String,
    pub error: String,
}

/// Parse the marker block. The VALUE marker is last, so everything after it (newlines
/// included) belongs to the value.
pub fn parse_invoke_output(out: &str) -> InvokeResult {
    let mut r = InvokeResult::default();
    if let Some(i) = out.find(M_VALUE) {
        r.value = out[i + M_VALUE.len()..]
            .strip_suffix('\n')
            .unwrap_or(&out[i + M_VALUE.len()..])
            .to_string();
    }
    for line in out.lines() {
        if let Some(v) = line.strip_prefix(M_KIND) {
            r.kind = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_RETURN_TYPE) {
            r.return_type = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_STATUS_OK) {
            let t = v.trim();
            if !t.is_empty() {
                r.status_ok = Some(t == "1");
            }
        } else if let Some(v) = line.strip_prefix(M_STATUS_TEXT) {
            r.status_text = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_ERROR) {
            r.error = v.trim_end().to_string();
        } else if let Some(v) = line.strip_prefix(M_VALUE_LEN) {
            r.value_len = v.trim().parse::<usize>().ok();
        }
    }
    // IRIS reported the exact character count, so use it rather than guessing which trailing
    // whitespace belongs to the value and which is the write that terminated it. Stripping "one
    // trailing newline" left a spurious \n on every value, because the generator's own output
    // adds one of its own; cutting to the declared length cannot make that mistake.
    if let Some(n) = r.value_len {
        let got = r.value.chars().count();
        r.truncated = got < n;
        if got > n {
            r.value = r.value.chars().take(n).collect();
        }
    }
    r
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

    // A <...> thrown inside the method is the method's answer, not a transport failure, so it
    // is reported as a named outcome rather than swallowed into the value.
    if !r.error.is_empty() {
        return crate::tools::envelope::fail_with(
            "METHOD_THREW",
            &format!("'{class}::{method}' raised: {}", r.error),
            serde_json::json!({
                "class": class, "method": method, "namespace": namespace,
                "return_type": r.return_type, "error_detail": r.error,
            }),
        );
    }

    // IRIS measured the value longer than what arrived. Reporting the short value as the
    // answer is the failure mode worth refusing outright.
    if r.truncated {
        return crate::tools::envelope::fail_with(
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
        );
    }

    crate::tools::envelope::ok_json(serde_json::json!({
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
        "args_passed": p.args.len(),
    }))
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

    #[test]
    fn the_parser_reads_every_field() {
        let out = "IEM_KIND\tstatus\nIEM_RETURN_TYPE\t%Status\nIEM_STATUS_OK\t0\n\
                   IEM_STATUS_TEXT\tERROR #5001: boom\nIEM_ERROR\t\nIEM_VALUE_LEN\t3\n\
                   IEM_VALUE\t0 e\n";
        let r = parse_invoke_output(out);
        assert_eq!(r.kind, "status");
        assert_eq!(r.return_type, "%Status");
        assert_eq!(r.status_ok, Some(false));
        assert_eq!(r.status_text, "ERROR #5001: boom");
        assert_eq!(r.value, "0 e");
        assert_eq!(r.value_len, Some(3));
        assert!(!r.truncated);
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
}
