//! `hl7_schema_list` / `hl7_schema_inspect` — name HL7 segments and fields instead of
//! addressing them by position.
//!
//! #246 filed this as a PORT with a reference implementation upstream. It is not portable.
//! Upstream's `hl7_schema_list` opens the class query `EnsLib.HL7.Schema:StoredSchemaNames`
//! and its `hl7_schema_inspect` opens `EnsLib.HL7.Schema:SegmentStructureElements`. Neither
//! query exists. Measured on IRIS for Health 2026.1 with a positive control in the same
//! exchange:
//!
//! ```text
//! %ResultSet.%New("EnsLib.HL7.Schema:StoredSchemaNames")        -> no object
//! %ResultSet.%New("EnsLib.HL7.Schema:SegmentStructureElements") -> no object
//! %ResultSet.%New("EnsLib.HL7.Schema:TypeCategories")           -> object, QueryIsValid 1
//! %ResultSet.%New("EnsLib.HL7.Schema:SegTypes")                 -> object, QueryIsValid 1
//! ```
//!
//! `%Dictionary.CompiledQuery` lists exactly six queries on that class and neither upstream
//! name is among them. So both upstream tools can only ever reach `Execute()` on a
//! non-object and report an error — the same shape as `iris_macro` posting to an endpoint
//! that 404s (#247). This module is written against the API that does exist.
//!
//! ## Repeating fields
//!
//! #246's acceptance criterion says `hl7_schema_inspect("2.5", "PID")` must return usable
//! names for REPEATING fields "or state plainly that it cannot", because
//! `EnsLib.HL7.Schema.GetFieldNameFromNumber` returns empty for `PID:3`, `PID:5` and `OBX:5`
//! — the fields people convert first.
//!
//! The names are not missing. `GetFieldNameFromNumber` walks
//! `$$$vaSchemaGbl(cat,"SS",seg,"map",sub)` and returns `sub` only where the stored value
//! EQUALS `FieldNumber`; a repeating field is stored as `3()`, so `"3()" = "3"` is false and
//! the loop falls through to `""`. It is a broken comparison, not absent metadata — measured:
//!
//! ```text
//! GetFieldNameFromNumber("2.5","PID","3") -> ""        map: patientidentifierlist = 3()
//! GetFieldNameFromNumber("2.5","PID","5") -> ""        map: patientname           = 5()
//! GetFieldNameFromNumber("2.5","PID","7") -> "datetimeofbirth"                    = 7
//! ```
//!
//! `getFieldsContentArray` reports the same fields with proper casing, so this tool takes the
//! first branch of the criterion: repeating fields are named, and the `()` marker that broke
//! the other API becomes the `repeating` flag.

use crate::objectscript::os_str_expr;

/// One field of a segment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hl7Field {
    pub number: String,
    pub name: String,
    /// True when the schema marks the field as repeating — the `()` in `:PatientName()`.
    pub repeating: bool,
    pub description: String,
    /// The schema's raw type, e.g. `DS:2.5:CX()` or `SI`.
    pub type_raw: String,
    /// The data type with the schema plumbing removed: `DS:2.5:CX()` -> `CX`.
    pub data_type: String,
    /// `R` required, `O` optional, `B` retained for backward compatibility.
    pub optionality: String,
    /// Enumerated values where the schema restricts them, e.g. `A,F,M,N,O,U`.
    pub values: Vec<String>,
    /// IRIS wrote `...`, meaning it did not list every permitted value. `values` is then a
    /// PARTIAL list and must not be treated as the complete set.
    pub values_truncated: bool,
}

/// A field name arrives as `:PatientIdentifierList()` — a leading colon, and `()` when the
/// field repeats. Returns the bare name and whether it repeats.
pub fn parse_field_name(raw: &str) -> (String, bool) {
    let t = raw.trim().trim_start_matches(':').trim();
    match t.strip_suffix("()") {
        Some(stem) => (stem.trim().to_string(), true),
        None => (t.to_string(), false),
    }
}

/// `DS:2.5:CX()` is a data-structure reference carrying the schema version; the useful part is
/// the last segment. `SI` and `IS` are primitive and pass through unchanged.
pub fn parse_data_type(raw: &str) -> String {
    let t = raw.trim();
    // A repeat suffix may be bare `()` or BOUNDED `(2)` — measured `DS:2.5:CE(2)` on PID:38 and
    // a type of literally `()` on PID:32.
    let t = match (t.rfind('('), t.ends_with(')')) {
        (Some(i), true) => t[..i].trim(),
        _ => t,
    };
    match t.rsplit_once(':') {
        Some((_, last)) if !last.trim().is_empty() => last.trim().to_string(),
        _ => t.to_string(),
    }
}

/// The generator transport turns a written `$CHAR(1)` into a NEWLINE. Measured: IRIS holds the
/// string intact (`$length("A"_$char(1)_"B")` is 3, `$ascii(x,2)` is 1) but the device output
/// arrives as separate lines. A delimiter-separated protocol therefore silently shredded every
/// row — categories split one-column-per-line, and every field line lost its parts and was
/// skipped, which surfaced as `success: true, field_count: 0` for a segment that has 39 fields.
/// One cause, two unrelated-looking symptoms. So IRIS emits JSON and nothing is delimited.
pub fn parse_fields_json(out: &str) -> Result<Vec<Hl7Field>, String> {
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("IRIS did not return JSON: {e}"))?;
    let rows = v
        .as_array()
        .ok_or_else(|| "IRIS returned JSON that is not an array".to_string())?;
    Ok(rows
        .iter()
        .map(|r| {
            let (name, repeating) = parse_field_name(r["name"].as_str().unwrap_or(""));
            let type_raw = r["type"].as_str().unwrap_or("").trim().to_string();
            let (values, values_truncated) = parse_values(r["vals"].as_str().unwrap_or(""));
            Hl7Field {
                number: match &r["n"] {
                    serde_json::Value::Number(n) => n.to_string(),
                    other => other.as_str().unwrap_or("").to_string(),
                },
                name,
                repeating,
                description: r["desc"].as_str().unwrap_or("").trim().to_string(),
                data_type: parse_data_type(&type_raw),
                type_raw,
                optionality: r["opt"].as_str().unwrap_or("").trim().to_string(),
                values,
                values_truncated,
            }
        })
        .collect())
}

/// IRIS writes a literal `...` when the enumerated list is too long to include — measured on
/// PID:12 (`"..."`) and PID:10 (`"...,1002-5,2028-9,2054-5,..."`). Returning that as an allowed
/// value would state something false, so it becomes a flag instead of an entry.
pub fn parse_values(raw: &str) -> (Vec<String>, bool) {
    let mut truncated = false;
    let values: Vec<String> = raw
        .split(',')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .filter(|v| {
            if *v == "..." {
                truncated = true;
                false
            } else {
                true
            }
        })
        .map(|v| v.to_string())
        .collect();
    (values, truncated)
}

pub fn parse_categories_json(out: &str) -> Result<Vec<serde_json::Value>, String> {
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("IRIS did not return JSON: {e}"))?;
    let rows = v
        .as_array()
        .ok_or_else(|| "IRIS returned JSON that is not an array".to_string())?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let category = r["category"].as_str().unwrap_or("").trim();
            if category.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "category": category,
                "description": r["description"].as_str().unwrap_or("").trim(),
                // IsStandard arrives as the string "1"/"0" or a JSON number.
                "is_standard": r["is_standard"].as_str() == Some("1")
                    || r["is_standard"].as_i64() == Some(1)
                    || r["is_standard"].as_bool() == Some(true),
                "base": r["base"].as_str().unwrap_or("").trim(),
            }))
        })
        .collect())
}

pub fn parse_segments_json(out: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("IRIS did not return JSON: {e}"))?;
    let rows = v
        .as_array()
        .ok_or_else(|| "IRIS returned JSON that is not an array".to_string())?;
    Ok(rows
        .iter()
        .filter_map(|r| r.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `EnsLib.HL7.Schema` ships only with IRIS for Health / HealthShare. Probing for the class is
/// cheaper than letting every call fail with `<CLASS DOES NOT EXIST>`.
pub fn build_availability_code() -> &'static str {
    r#"write ##class(%Dictionary.CompiledClass).%ExistsId("EnsLib.HL7.Schema"),!"#
}

pub fn build_categories_code() -> &'static str {
    r#"set tRS=##class(%ResultSet).%New("EnsLib.HL7.Schema:TypeCategories")
if '$isobject(tRS) { write "HSERR:TypeCategories query is not available on this instance" quit }
set tSC=tRS.Execute("")
if '$SYSTEM.Status.IsOK(tSC) { write "HSERR:"_$SYSTEM.Status.GetErrorText(tSC) quit }
set tOut=[]
while tRS.Next() {
  do tOut.%Push({"category":(tRS.GetData(1)),"description":(tRS.GetData(2)),"is_standard":(tRS.GetData(3)),"base":(tRS.GetData(4))})
}
write tOut.%ToJSON()"#
}

/// `SegTypes` returns one row per FIELD — 17768 rows for category 2.5, across 153 segments — so
/// the segment names are deduplicated in IRIS rather than shipped over the wire.
pub fn build_segments_code(category: &str) -> String {
    format!(
        r#"set tRS=##class(%ResultSet).%New("EnsLib.HL7.Schema:SegTypes")
if '$isobject(tRS) {{ write "HSERR:SegTypes query is not available on this instance" quit }}
set tSC=tRS.Execute({cat},"",0)
if '$SYSTEM.Status.IsOK(tSC) {{ write "HSERR:"_$SYSTEM.Status.GetErrorText(tSC) quit }}
kill tSegs
while tRS.Next() {{
  set tSeg=$piece(tRS.GetData(1),":",2)
  if tSeg'="" {{ set tSegs(tSeg)="" }}
}}
set tOut=[]
set tK=""
for  {{
  set tK=$order(tSegs(tK))
  quit:tK=""
  do tOut.%Push(tK)
}}
write tOut.%ToJSON()"#,
        cat = os_str_expr(category)
    )
}

/// `getFieldsContentArray(.contents, "source", category, segment, level, extraDetails)` is the
/// supported accessor. `extraDetails=1` is what adds `desc` and `vals`.
pub fn build_fields_code(category: &str, segment: &str) -> String {
    format!(
        r#"kill tArr
set tSC=##class(EnsLib.HL7.Schema).getFieldsContentArray(.tArr,"source",{cat},{seg},1,1)
if '$SYSTEM.Status.IsOK(tSC) {{ write "HSERR:"_$SYSTEM.Status.GetErrorText(tSC) quit }}
set tOut=[]
set tK=""
for  {{
  set tK=$order(tArr(tK))
  quit:tK=""
  do tOut.%Push({{"n":(tK),"name":($get(tArr(tK,"name"))),"opt":($get(tArr(tK,"opt"))),"type":($get(tArr(tK,"type"))),"vals":($get(tArr(tK,"vals"))),"desc":($get(tArr(tK,"desc")))}})
}}
write tOut.%ToJSON()"#,
        cat = os_str_expr(category),
        seg = os_str_expr(segment)
    )
}

/// An unknown category and an unknown segment produce the SAME IRIS error — measured:
/// `getFieldsContentArray(...,"2.5","ZZQ",...)` and `(...,"9.9","PID",...)` both return
/// `ERROR <Ens>ErrGeneral: Unknown segment type '<cat>:<seg>'`. So the message must not claim
/// which half is wrong; it must name both and say how to check each.
pub fn unknown_segment_message(category: &str, segment: &str) -> String {
    format!(
        "IRIS reports 'Unknown segment type' for category '{category}' segment '{segment}'. \
         The error does not say which of the two it did not recognise, so check both: \
         hl7_schema_list gives the categories this instance has, and hl7_schema_inspect with \
         the category alone lists that category's segment names."
    )
}

/// The generator writes `HSERR:` when IRIS reported a status error, so an error is never
/// returned as a successful empty list.
pub fn generator_error(out: &str) -> Option<&str> {
    out.trim().strip_prefix("HSERR:").map(|m| m.trim())
}

/// A segment IRIS ACCEPTED always has fields — PID in 2.5 has 39. So an empty list is not a
/// fact about the schema, it is a sign the read did not work, and returning it as a clean
/// `success: true, field_count: 0` is how the $CHAR(1) transport bug stayed invisible. Refuse it.
pub fn empty_fields_message(category: &str, segment: &str) -> String {
    format!(
        "IRIS accepted category '{category}' segment '{segment}' but returned no fields. A \
         segment the schema recognises always has at least one, so this is a failed read rather \
         than an empty segment, and it is NOT evidence that '{segment}' has no fields. Nothing \
         is reported rather than reporting an empty list that would read as a fact."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `()` that broke GetFieldNameFromNumber is the repeat marker, and it is the whole
    // reason #246 thought repeating fields could not be named.
    #[test]
    fn a_repeating_field_is_named_and_flagged() {
        assert_eq!(
            parse_field_name(":PatientIdentifierList()"),
            ("PatientIdentifierList".to_string(), true)
        );
        assert_eq!(
            parse_field_name(":PatientName()"),
            ("PatientName".to_string(), true)
        );
        assert_eq!(
            parse_field_name(":DateTimeofBirth"),
            ("DateTimeofBirth".to_string(), false)
        );
        assert_eq!(
            parse_field_name(" SetIDPID "),
            ("SetIDPID".to_string(), false)
        );
    }

    #[test]
    fn the_data_type_drops_the_schema_plumbing_and_the_repeat_suffix() {
        assert_eq!(parse_data_type("DS:2.5:CX()"), "CX");
        assert_eq!(parse_data_type("DS:2.5:TS"), "TS");
        // A BOUNDED repeat, measured on PID:38 as `DS:2.5:CE(2)`. Guessing `()` only would
        // have left the type as "CE(2)".
        assert_eq!(parse_data_type("DS:2.5:CE(2)"), "CE");
        // PID:32's type is literally `()`.
        assert_eq!(parse_data_type("()"), "");
        assert_eq!(parse_data_type("SI"), "SI");
        assert_eq!(parse_data_type(""), "");
    }

    // IRIS's `...` means "I did not list them all". Reporting it as a permitted value would
    // state something false about the schema.
    #[test]
    fn an_ellipsis_is_a_truncation_flag_not_a_value() {
        let (v, trunc) = parse_values("...");
        assert!(v.is_empty(), "'...' is not a value: {v:?}");
        assert!(trunc);

        let (v, trunc) = parse_values("...,1002-5,2028-9");
        assert_eq!(v, vec!["1002-5", "2028-9"]);
        assert!(trunc, "a partial list must say so");

        let (v, trunc) = parse_values("A,F,M,N,O,U");
        assert_eq!(v, vec!["A", "F", "M", "N", "O", "U"]);
        assert!(!trunc, "a complete list must not be flagged");

        let (v, trunc) = parse_values("");
        assert!(v.is_empty());
        assert!(!trunc);
    }

    /// Real rows, exactly as IRIS emitted them for PID in 2.5.
    #[test]
    fn fields_parse_from_the_json_iris_actually_sends() {
        let out = r#"[
          {"n":3,"name":":PatientIdentifierList()","opt":"R","type":"DS:2.5:CX()","vals":"","desc":"Patient Identifier List"},
          {"n":7,"name":":DateTimeofBirth","opt":"O","type":"DS:2.5:TS","vals":"","desc":"Date/Time of Birth"},
          {"n":8,"name":":AdministrativeSex","opt":"O","type":"IS","vals":"A,F,M,N,O,U","desc":"Administrative Sex"},
          {"n":12,"name":":CountyCode","opt":"B","type":"IS","vals":"...","desc":"County Code"},
          {"n":35,"name":":SpeciesCode","opt":"C","type":"DS:2.5:CE","vals":"...","desc":"Species Code"}
        ]"#;
        let f = parse_fields_json(out).expect("must parse");
        assert_eq!(f.len(), 5);

        // `n` arrives as a JSON NUMBER, not a string.
        assert_eq!(f[0].number, "3");
        assert_eq!(f[0].name, "PatientIdentifierList");
        assert!(f[0].repeating, "PID:3 repeats");
        assert_eq!(f[0].data_type, "CX");
        assert_eq!(f[0].optionality, "R");
        assert_eq!(f[0].description, "Patient Identifier List");

        assert!(!f[1].repeating, "PID:7 does not repeat");
        assert_eq!(f[2].values, vec!["A", "F", "M", "N", "O", "U"]);
        assert!(!f[2].values_truncated);
        assert!(f[3].values_truncated, "PID:12's list is truncated");
        // `C` (conditional) is a fourth optionality beyond R/O/B.
        assert_eq!(f[4].optionality, "C");
    }

    // Free text keeps its commas and apostrophes because nothing is delimited any more.
    #[test]
    fn free_text_survives_because_the_protocol_is_json() {
        let out = r#"[{"n":6,"name":":MothersMaidenName()","opt":"O","type":"DS:2.5:XPN()","vals":"","desc":"Mother's Maiden Name, first|last"}]"#;
        let f = parse_fields_json(out).expect("must parse");
        assert_eq!(f[0].description, "Mother's Maiden Name, first|last");
        assert_eq!(f[0].name, "MothersMaidenName");
    }

    // The transport bug returned plain text where JSON was expected. That must surface as an
    // error, never as an empty field list.
    #[test]
    fn non_json_output_is_an_error_not_an_empty_list() {
        assert!(parse_fields_json("3\n:PatientName()\nR\n").is_err());
        assert!(parse_fields_json("").is_err());
        // Valid JSON of the wrong shape is also an error, not a silent empty.
        assert!(parse_fields_json(r#"{"oops":1}"#).is_err());
        assert!(parse_segments_json("not json").is_err());
        assert!(parse_categories_json("not json").is_err());
    }

    #[test]
    fn an_iris_status_error_is_not_an_empty_list() {
        assert_eq!(
            generator_error("HSERR:ERROR <Ens>ErrGeneral: Unknown segment type '2.5:ZZQ'"),
            Some("ERROR <Ens>ErrGeneral: Unknown segment type '2.5:ZZQ'")
        );
        assert_eq!(generator_error("[]"), None);
        assert_eq!(generator_error(""), None);
    }

    // Both halves produce the same IRIS message, so the text must not accuse one of them.
    #[test]
    fn the_unknown_segment_message_blames_neither_half_alone() {
        let m = unknown_segment_message("9.9", "PID");
        assert!(m.contains("9.9") && m.contains("PID"), "{m}");
        assert!(
            m.contains("does not say which"),
            "must not claim which half is wrong: {m}"
        );
        assert!(m.contains("hl7_schema_list"), "must route onward: {m}");
    }

    // The $CHAR(1) bug produced `success: true, field_count: 0` for a 39-field segment. The
    // message must deny that an empty read is a fact about the schema.
    #[test]
    fn an_empty_field_read_is_refused_not_reported() {
        let m = empty_fields_message("2.5", "PID");
        assert!(m.contains("2.5") && m.contains("PID"), "{m}");
        assert!(m.contains("NOT evidence"), "{m}");
        assert!(
            m.contains("failed read"),
            "must name it a failed read, not an empty segment: {m}"
        );
    }

    #[test]
    fn categories_parse_with_the_standard_flag() {
        // IsStandard arrives as the STRING "1", which is why the parse accepts three shapes.
        let out = r#"[
          {"category":"2.5","description":"","is_standard":"1","base":""},
          {"category":"Custom","description":"my schema","is_standard":"0","base":"2.5"}
        ]"#;
        let c = parse_categories_json(out).expect("must parse");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["category"], "2.5");
        assert_eq!(c[0]["is_standard"], serde_json::json!(true));
        assert_eq!(c[1]["is_standard"], serde_json::json!(false));
        assert_eq!(c[1]["base"], "2.5");
        assert_eq!(c[1]["description"], "my schema");
    }

    #[test]
    fn segments_parse_and_drop_blanks() {
        assert_eq!(
            parse_segments_json(r#"["ABS","ACC","","PID"]"#).expect("parse"),
            vec!["ABS", "ACC", "PID"]
        );
        assert!(parse_segments_json("[]").expect("parse").is_empty());
    }

    // Upstream interpolated the caller's strings straight into ObjectScript. A category with a
    // quote in it would have broken the line; here it must be escaped, not spliced.
    #[test]
    fn caller_strings_are_escaped_into_the_generated_code() {
        let code = build_fields_code("2.5\"evil", "PID");
        assert!(
            code.contains("\"2.5\"\"evil\""),
            "a quote must be DOUBLED, never backslash-escaped: {code}"
        );
        assert!(code.contains("\"PID\""), "{code}");
        let seg_code = build_segments_code("2.5");
        assert!(seg_code.contains("Execute(\"2.5\",\"\",0)"), "{seg_code}");
    }

    #[test]
    fn the_generated_code_asks_for_extra_details_and_emits_json() {
        let code = build_fields_code("2.5", "PID");
        // extraDetails=1 is what populates desc and vals.
        assert!(code.contains(",1,1)"), "{code}");
        assert!(code.contains("getFieldsContentArray"), "{code}");
        // JSON, not a delimiter — a written $CHAR(1) arrives as a newline.
        assert!(code.contains("%ToJSON()"), "{code}");
        assert!(!code.contains("$char(1)"), "delimiters are the bug: {code}");
        assert!(build_categories_code().contains("%ToJSON()"));
        assert!(!build_categories_code().contains("$char(1)"));
        assert!(build_segments_code("2.5").contains("%ToJSON()"));
    }
}

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Hl7SchemaListParams {
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Hl7SchemaInspectParams {
    /// Schema category, e.g. "2.5" or "2.5.1" — call hl7_schema_list for what this instance
    /// has. A custom category name works too.
    pub version: String,
    /// Segment to describe, e.g. "PID" or "OBX". OMIT to list the category's segment names
    /// instead of one segment's fields.
    #[serde(default)]
    pub segment: Option<String>,
    /// IRIS namespace. OMIT this field to use the connection's configured namespace
    /// (IRIS_NAMESPACE) — only pass a value to deliberately target a different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

const NOT_AVAILABLE: &str = "EnsLib.HL7.Schema is not present in this namespace. HL7 schemas \
     ship with IRIS for Health and HealthShare; a plain IRIS or Community instance does not \
     have them, and no schema question can be answered here.";

async fn hl7_available(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
) -> Result<bool, String> {
    match iris
        .execute_via_generator(build_availability_code(), namespace, client)
        .await
    {
        Ok(out) => Ok(out.trim() == "1"),
        Err(e) => Err(e.to_string()),
    }
}

pub async fn handle_hl7_schema_list(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: Hl7SchemaListParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    match hl7_available(iris, client, &namespace).await {
        Ok(false) => {
            return crate::tools::envelope::fail_with(
                "HL7_NOT_AVAILABLE",
                NOT_AVAILABLE,
                serde_json::json!({ "namespace": namespace }),
            )
        }
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_hl7_schema_list", &e);
        }
        Ok(true) => {}
    }

    let out = match iris
        .execute_via_generator(build_categories_code(), &namespace, client)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_hl7_schema_list", &e.to_string())
        }
    };
    if let Some(msg) = generator_error(&out) {
        return crate::tools::envelope::fail_with(
            "HL7_SCHEMA_ERROR",
            msg,
            serde_json::json!({ "namespace": namespace }),
        );
    }
    let categories = match parse_categories_json(&out) {
        Ok(c) => c,
        Err(e) => {
            return crate::tools::envelope::fail_with(
                "HL7_SCHEMA_ERROR",
                &e,
                serde_json::json!({ "namespace": namespace }),
            )
        }
    };
    crate::tools::envelope::ok_json(serde_json::json!({
        "success": true,
        "namespace": namespace,
        "count": categories.len(),
        "categories": categories,
    }))
}

pub async fn handle_hl7_schema_inspect(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: Hl7SchemaInspectParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));
    let version = p.version.trim();
    if version.is_empty() {
        return crate::tools::envelope::fail_with(
            "MISSING_PARAMS",
            "'version' is required — the schema category, e.g. \"2.5\". Call hl7_schema_list \
             for the categories this instance has. Nothing was run.",
            serde_json::json!({ "version": p.version }),
        );
    }
    match hl7_available(iris, client, &namespace).await {
        Ok(false) => {
            return crate::tools::envelope::fail_with(
                "HL7_NOT_AVAILABLE",
                NOT_AVAILABLE,
                serde_json::json!({ "namespace": namespace }),
            )
        }
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_hl7_schema_inspect", &e);
        }
        Ok(true) => {}
    }

    let segment = p
        .segment
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let code = match segment {
        Some(seg) => build_fields_code(version, seg),
        None => build_segments_code(version),
    };
    let out = match iris.execute_via_generator(&code, &namespace, client).await {
        Ok(v) => v,
        Err(e) => {
            return crate::tools::envelope::transport_fail(
                "handle_hl7_schema_inspect",
                &e.to_string(),
            )
        }
    };
    if let Some(msg) = generator_error(&out) {
        // `Unknown segment type` is the one error a caller can act on, and IRIS raises it for a
        // bad CATEGORY as readily as a bad segment — so say so rather than echoing the message.
        if msg.contains("Unknown segment type") {
            return crate::tools::envelope::fail_with(
                "UNKNOWN_SEGMENT_OR_CATEGORY",
                &unknown_segment_message(version, segment.unwrap_or("")),
                serde_json::json!({
                    "version": version,
                    "segment": segment,
                    "iris_error": msg,
                }),
            );
        }
        return crate::tools::envelope::fail_with(
            "HL7_SCHEMA_ERROR",
            msg,
            serde_json::json!({ "version": version, "segment": segment }),
        );
    }

    match segment {
        Some(seg) => {
            let fields = match parse_fields_json(&out) {
                Ok(f) => f,
                Err(e) => {
                    return crate::tools::envelope::fail_with(
                        "HL7_SCHEMA_ERROR",
                        &e,
                        serde_json::json!({ "version": version, "segment": seg }),
                    )
                }
            };
            // A segment IRIS accepted always has fields; an empty list means the read failed.
            if fields.is_empty() {
                return crate::tools::envelope::fail_with(
                    "HL7_EMPTY_SCHEMA_READ",
                    &empty_fields_message(version, seg),
                    serde_json::json!({ "version": version, "segment": seg }),
                );
            }
            let repeating: Vec<&str> = fields
                .iter()
                .filter(|f| f.repeating)
                .map(|f| f.name.as_str())
                .collect();
            crate::tools::envelope::ok_json(serde_json::json!({
                "success": true,
                "namespace": namespace,
                "version": version,
                "segment": seg,
                "field_count": fields.len(),
                "fields": fields.iter().map(|f| serde_json::json!({
                    "number": f.number,
                    "name": f.name,
                    "repeating": f.repeating,
                    "description": f.description,
                    "data_type": f.data_type,
                    "type_raw": f.type_raw,
                    "optionality": f.optionality,
                    "values": f.values,
                    "values_truncated": f.values_truncated,
                })).collect::<Vec<_>>(),
                // Named because these are the fields `GetFieldNameFromNumber` cannot report
                // (#246): it matches the stored `N()` against a bare `N` and returns "".
                "repeating_fields": repeating,
            }))
        }
        None => {
            let segments = match parse_segments_json(&out) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::envelope::fail_with(
                        "HL7_SCHEMA_ERROR",
                        &e,
                        serde_json::json!({ "version": version }),
                    )
                }
            };
            crate::tools::envelope::ok_json(serde_json::json!({
                "success": true,
                "namespace": namespace,
                "version": version,
                "segment_count": segments.len(),
                "segments": segments,
                "note": "Pass 'segment' to get one segment's field names, types and repeat flags.",
            }))
        }
    }
}
