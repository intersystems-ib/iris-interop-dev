//! #24 item 070: parse a `%Dictionary.CompiledMethod.FormalSpec` into structured arguments.
//!
//! `docs_introspect` hands the raw string straight through, and for real IRIS methods that string is
//! not readable. This is one value, verbatim, from `%SYS.SQLStatementCache.SaveStatement`:
//!
//! ```text
//! &pStatementText:%String(MAXLEN=""),pStatementHash:%Binary,...,&pParameters:%Binary="",...
//! ```
//!
//! and one from `EnsLib.UDDI.UDDIInquirySoapBinding.findbusiness`:
//!
//! ```text
//! name:%ListOfObjects(ELEMENTTYPE="EnsLib.UDDI.uddi.name",XMLPROJECTION="element",XMLREF=1),...
//! ```
//!
//! THE HAZARD, AND WHY A SPLIT ON ',' IS WRONG
//!
//! Type parameters are parenthesised and CONTAIN COMMAS, and their values are quoted strings that may
//! contain commas too. `spec.split(',')` shreds the second example into six fragments, none of which
//! is an argument. The splitter here is depth- and quote-aware: a comma separates arguments only at
//! paren depth 0 and outside a string.
//!
//! EVERY FORM BELOW WAS FOUND IN LIVE DATA on IRIS for Health 2026.1, by querying
//! %Dictionary.CompiledMethod rather than reading the ObjectScript reference:
//!
//! | form                     | example (real)                        | meaning              |
//! |--------------------------|---------------------------------------|----------------------|
//! | `name`                   | `p1`, `tNode`, `%Client`              | untyped              |
//! | `name:%Type`             | `pStatementHash:%Binary`              | typed                |
//! | `name:%Type=default`     | `&pParameters:%Binary=""`             | typed with default   |
//! | `name=default`           | `containid=0`, `selectmode="RUNTIME"` | untyped with default |
//! | `&name`                  | `&qHandle:%String(MAXLEN="")=""`      | ByRef                |
//! | `*name`                  | `*pCSubs:%String`                     | Output               |
//! | `name...`                | `Args...`, `Subs...`                  | variable arity       |
//! | `%Type(P=v,P="s")`        | `%String(MAXLEN="")`                  | type parameters      |

use serde::Serialize;

/// One argument of a FormalSpec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormalArg {
    pub name: String,
    /// The class, WITHOUT its parameter list — `%String`, not `%String(MAXLEN="")`. Separated
    /// because the bare class is what a caller reasons about; the parameters are detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// The text between the type's parentheses, e.g. `MAXLEN=""`. Kept RAW rather than parsed into
    /// a map: the values are ObjectScript literals, and re-quoting them would lose information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_params: Option<String>,
    /// The default expression as written, e.g. `0`, `""`, `"RUNTIME"`. An argument with a default is
    /// OPTIONAL — that is the single most useful fact this parse recovers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// `&name` — passed by reference; the caller's variable can be modified.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_ref: bool,
    /// `*name` — an OUTPUT argument. Calling without it is the classic silent-wrong-answer bug.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub output: bool,
    /// `name...` — variable arity.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub varargs: bool,
}

/// True when this char index sits outside any quoted string and at paren depth 0.
///
/// IRIS escapes a quote inside a string by DOUBLING it (`""`), so a `"` toggles the state and a
/// doubled pair toggles twice — which lands back in-string, the correct result, with no special case.
fn split_top_level(spec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    for c in spec.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                // Saturating: a malformed spec must not make depth negative and start splitting
                // inside parentheses.
                depth = depth.saturating_sub(1).max(0);
                cur.push(c);
            }
            ',' if !in_str && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Index of the first `needle` at paren depth 0 and outside a string.
fn find_top_level(s: &str, needle: char) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth = depth.saturating_sub(1).max(0),
            c if c == needle && !in_str && depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// `%String(MAXLEN="")` -> (`%String`, Some(`MAXLEN=""`)).
fn split_type(t: &str) -> (String, Option<String>) {
    let t = t.trim();
    match t.find('(') {
        Some(i) if t.ends_with(')') => {
            let base = t[..i].trim().to_string();
            let params = t[i + 1..t.len() - 1].trim().to_string();
            // `%Foo()` — empty parens carry no information, so None rather than Some("").
            (base, (!params.is_empty()).then_some(params))
        }
        _ => (t.to_string(), None),
    }
}

/// Parse one argument. `None` only when nothing usable is left after the sigils.
pub fn parse_arg(raw: &str) -> Option<FormalArg> {
    let s = raw.trim();
    let (by_ref, s) = match s.strip_prefix('&') {
        Some(r) => (true, r.trim()),
        None => (false, s),
    };
    let (output, s) = match s.strip_prefix('*') {
        Some(r) => (true, r.trim()),
        None => (false, s),
    };
    if s.is_empty() {
        return None;
    }
    // `:` binds tighter than `=`: the type comes first, and the default may itself contain a colon
    // inside a string.
    let (head, type_and_default) = match find_top_level(s, ':') {
        Some(i) => (s[..i].trim(), Some(s[i + 1..].trim())),
        None => (s, None),
    };
    let (mut name, mut default) = (head.to_string(), None);
    let mut type_text: Option<&str> = None;
    match type_and_default {
        Some(rest) => match find_top_level(rest, '=') {
            Some(i) => {
                type_text = Some(rest[..i].trim());
                default = Some(rest[i + 1..].trim().to_string());
            }
            None => type_text = Some(rest),
        },
        // No type at all — the `=` then belongs to the NAME half (`containid=0`).
        None => {
            if let Some(i) = find_top_level(head, '=') {
                name = head[..i].trim().to_string();
                default = Some(head[i + 1..].trim().to_string());
            }
        }
    }
    let varargs = name.ends_with("...");
    if varargs {
        name.truncate(name.len() - 3);
        name = name.trim().to_string();
    }
    if name.is_empty() {
        return None;
    }
    let (type_name, type_params) = match type_text.filter(|t| !t.is_empty()) {
        Some(t) => {
            let (base, params) = split_type(t);
            ((!base.is_empty()).then_some(base), params)
        }
        None => (None, None),
    };
    Some(FormalArg {
        name,
        type_name,
        type_params,
        default,
        by_ref,
        output,
        varargs,
    })
}

/// Parse a whole FormalSpec. An empty or whitespace spec yields an empty list — a method with no
/// arguments, which is distinct from a method whose spec could not be read.
pub fn parse(spec: &str) -> Vec<FormalArg> {
    split_top_level(spec)
        .iter()
        .filter_map(|a| parse_arg(a))
        .collect()
}

/// Attach `args` to every method row that carries a `FormalSpec`.
///
/// The raw `FormalSpec` is LEFT IN PLACE. It is the only lossless record of what IRIS holds, and a
/// caller that already reads it must not break; `args` is strictly additive.
pub fn annotate_methods(rows: &mut serde_json::Value) -> usize {
    let Some(arr) = rows.as_array_mut() else {
        return 0;
    };
    let mut n = 0;
    for row in arr.iter_mut() {
        let spec = row
            .get("FormalSpec")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        // An absent FormalSpec and an empty one both mean "no arguments", so both get `[]`. The
        // alternative — omitting the field — makes a caller unable to tell "no args" from "this
        // build does not report args".
        let args = parse(&spec);
        obj.insert("args".into(), serde_json::json!(args));
        obj.insert("arg_count".into(), serde_json::json!(args.len()));
        // The facts a caller most often wants without walking the list.
        let required = args
            .iter()
            .filter(|a| a.default.is_none() && !a.varargs)
            .count();
        obj.insert("required_arg_count".into(), serde_json::json!(required));
        if args.iter().any(|a| a.output) {
            obj.insert("has_output_args".into(), serde_json::json!(true));
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(spec: &str) -> Vec<String> {
        parse(spec).into_iter().map(|a| a.name).collect()
    }

    // ── the hazard: commas that are not separators ──────────────────────────────────────────

    /// VERBATIM from `EnsLib.UDDI.UDDIInquirySoapBinding.findbusiness` on IRIS for Health 2026.1.
    /// A `split(',')` returns 6 fragments here; there are 2 arguments.
    const UDDI: &str = r#"name:%ListOfObjects(ELEMENTTYPE="EnsLib.UDDI.uddi.name",XMLPROJECTION="element",REFELEMENTQUALIFIED=1,XMLNAME="name",XMLREF=1),maxRows:%xsd.int(XMLPROJECTION="attribute")"#;

    #[test]
    fn commas_inside_type_parameters_do_not_split_arguments() {
        assert_eq!(
            names(UDDI),
            vec!["name", "maxRows"],
            "a naive split on ',' yields 6 fragments for this real spec"
        );
        let a = &parse(UDDI)[0];
        assert_eq!(a.type_name.as_deref(), Some("%ListOfObjects"));
        assert!(
            a.type_params
                .as_deref()
                .unwrap_or_default()
                .contains("XMLNAME=\"name\""),
            "the parameters must survive intact: {a:?}"
        );
    }

    /// A comma inside a QUOTED string is not a separator either.
    #[test]
    fn a_comma_inside_a_string_literal_does_not_split() {
        let a = parse(r#"sep:%String="a,b",n:%Integer"#);
        assert_eq!(
            a.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["sep", "n"]
        );
        assert_eq!(a[0].default.as_deref(), Some(r#""a,b""#));
    }

    /// IRIS escapes a quote inside a string by doubling it. A doubled pair must leave the parser
    /// in-string, so a following comma is still not a separator.
    #[test]
    fn a_doubled_quote_does_not_end_the_string_early() {
        let a = parse(r#"q:%String="he said ""hi,there""",n:%Integer"#);
        assert_eq!(
            a.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["q", "n"],
            "got {a:?}"
        );
    }

    // ── every form found in live data ───────────────────────────────────────────────────────

    /// VERBATIM from `%XSQL.DS.DML.%Prepare` — by-ref with a parameterised type AND a default,
    /// beside untyped-with-default and bare positional arguments.
    #[test]
    fn the_prepare_spec_parses_every_form_it_mixes() {
        let a = parse(r#"&qHandle:%String(MAXLEN="")="",containid=0,selectmode="RUNTIME",p1,p2"#);
        assert_eq!(a.len(), 5, "{a:?}");

        assert_eq!(a[0].name, "qHandle");
        assert!(a[0].by_ref, "leading & is ByRef: {:?}", a[0]);
        assert_eq!(a[0].type_name.as_deref(), Some("%String"));
        assert_eq!(a[0].type_params.as_deref(), Some(r#"MAXLEN="""#));
        assert_eq!(a[0].default.as_deref(), Some(r#""""#));

        // untyped WITH a default — the `=` belongs to the name half when there is no `:`
        assert_eq!(a[1].name, "containid");
        assert_eq!(a[1].type_name, None);
        assert_eq!(a[1].default.as_deref(), Some("0"));

        assert_eq!(a[2].name, "selectmode");
        assert_eq!(a[2].default.as_deref(), Some(r#""RUNTIME""#));

        // bare positional
        assert_eq!(a[3].name, "p1");
        assert_eq!(a[3].default, None);
        assert!(!a[3].by_ref && !a[3].output && !a[3].varargs, "{:?}", a[3]);
    }

    /// `*name` is an OUTPUT argument — from `EnsLib.EDI.XML.DOM.domSetAtIndex`. Calling such a
    /// method without passing the output variable is a silent-wrong-answer bug, so this flag is the
    /// most valuable thing the parse surfaces.
    #[test]
    fn a_star_prefix_is_an_output_argument() {
        let a = parse("pDOMPath:%String,*pCSubs:%String,*pTextOrdinal:%String=1");
        assert!(!a[0].output, "{:?}", a[0]);
        assert!(a[1].output, "{:?}", a[1]);
        assert_eq!(a[1].name, "pCSubs", "the * must not survive in the name");
        // output AND a default together
        assert!(a[2].output);
        assert_eq!(a[2].default.as_deref(), Some("1"));
    }

    /// From `%ASQ.AST.%DispatchClassMethod` — `Args...` is variable arity, and the `...` must not
    /// remain part of the name.
    #[test]
    fn a_trailing_ellipsis_is_varargs_and_is_stripped_from_the_name() {
        let a = parse("Class:%String,Method:%String,Args...");
        assert_eq!(a.len(), 3);
        assert!(a[2].varargs, "{:?}", a[2]);
        assert_eq!(a[2].name, "Args", "the ... must not survive: {:?}", a[2]);
        assert!(!a[0].varargs);
    }

    /// The SOAP binding specials arrive as bare names and must survive as arguments, not be dropped.
    #[test]
    fn the_soap_special_arguments_are_kept() {
        let a = parse("%Client,%Action,authInfo:EnsLib.UDDI.uddi.authInfo");
        assert_eq!(
            a.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["%Client", "%Action", "authInfo"]
        );
    }

    // ── edges that must not become wrong answers ────────────────────────────────────────────

    /// A method with no arguments is an empty list, not one empty argument.
    #[test]
    fn an_empty_spec_is_no_arguments_not_one_blank_one() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    /// `%Foo()` carries no parameter information, so `type_params` is None rather than Some("") —
    /// absent and empty stay distinct.
    #[test]
    fn empty_type_parentheses_yield_no_type_params() {
        let a = parse("x:%Foo()");
        assert_eq!(a[0].type_name.as_deref(), Some("%Foo"));
        assert_eq!(a[0].type_params, None, "{a:?}");
    }

    /// A malformed spec must degrade, never panic and never start splitting inside parentheses —
    /// which is what an unbalanced `)` would do if depth were allowed to go negative.
    /// A MUTATION SURVIVED the first version of this test: letting paren depth go NEGATIVE passed,
    /// because the test only asserted `!result.is_empty()` — "something came back" rather than "the
    /// right thing came back". With a negative depth, one stray `)` makes every later comma a
    /// non-separator, so the whole spec collapses into a single argument. The saturating clamp is what
    /// keeps a comma after a stray `)` working as a separator, so the assertion has to COUNT.
    #[test]
    fn an_unbalanced_paren_does_not_panic_or_split_wrongly() {
        // unclosed `(` — everything after it is legitimately inside the parens, so one argument
        let a = parse("x:%String(MAXLEN=,y:%Integer");
        assert_eq!(a.len(), 1, "an unclosed paren swallows the rest: {a:?}");
        assert_eq!(a[0].name, "x");

        // a STRAY `)` must not drive depth below zero; the following comma still separates
        let b = parse("x)y,z:%Integer");
        assert_eq!(
            b.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            vec!["x)y", "z"],
            "a stray ')' must not stop later commas separating: {b:?}"
        );
    }

    /// The SAME clamp exists in `find_top_level`, and mutating THAT copy survived the test above —
    /// two copies of one guard, only one exercised. A stray `)` before the `:` drives that function's
    /// depth negative, so the `:` is no longer seen at depth 0 and the type is silently lost.
    #[test]
    fn a_stray_paren_before_the_colon_does_not_hide_the_type() {
        let a = parse("x):%String");
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].name, "x)", "{a:?}");
        assert_eq!(
            a[0].type_name.as_deref(),
            Some("%String"),
            "a stray ')' must not swallow the type: {a:?}"
        );
        // and the same for the `=` scan
        let b = parse("y)=3");
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(b[0].default.as_deref(), Some("3"), "{b:?}");
    }

    /// A colon inside a default string must not be mistaken for the type separator.
    #[test]
    fn a_colon_inside_a_default_string_is_not_a_type_separator() {
        let a = parse(r#"url:%String="http://x:8080/p""#);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].type_name.as_deref(), Some("%String"));
        assert_eq!(a[0].default.as_deref(), Some(r#""http://x:8080/p""#));
    }

    // ── the payload annotation ──────────────────────────────────────────────────────────────

    #[test]
    fn annotate_adds_args_and_counts_without_removing_the_raw_spec() {
        let mut rows = serde_json::json!([
            {"Name": "Run", "FormalSpec": "a:%String,b:%Integer=1,*out:%String", "ReturnType": "%Status"},
            {"Name": "NoArgs", "FormalSpec": "", "ReturnType": "%Status"}
        ]);
        assert_eq!(annotate_methods(&mut rows), 2);

        let r0 = &rows[0];
        // STRICTLY ADDITIVE: the raw spec is the only lossless record and a caller may already read it
        assert_eq!(
            r0["FormalSpec"], "a:%String,b:%Integer=1,*out:%String",
            "the raw spec must survive"
        );
        assert_eq!(r0["arg_count"], 3);
        // `b` has a default and is therefore optional; `out` has none
        assert_eq!(r0["required_arg_count"], 2);
        assert_eq!(r0["has_output_args"], true);
        assert_eq!(r0["args"][2]["name"], "out");
        assert_eq!(r0["args"][2]["output"], true);
        // a flag that is false is omitted, so the JSON stays small
        assert!(r0["args"][0].get("output").is_none(), "{r0}");

        // no arguments is [] and 0 — never a missing field, which would be indistinguishable from
        // "this build does not report args"
        assert_eq!(rows[1]["arg_count"], 0);
        assert_eq!(rows[1]["args"], serde_json::json!([]));
        assert!(
            rows[1].get("has_output_args").is_none(),
            "no output args means the flag is absent, not false"
        );
    }

    /// varargs is not a REQUIRED argument — counting it as one would tell a caller it must pass
    /// something it may legally omit.
    #[test]
    fn varargs_does_not_count_as_required() {
        let mut rows = serde_json::json!([{"Name": "D", "FormalSpec": "Method:%String,Args..."}]);
        annotate_methods(&mut rows);
        assert_eq!(rows[0]["arg_count"], 2);
        assert_eq!(rows[0]["required_arg_count"], 1, "{}", rows[0]);
    }

    /// A row with no FormalSpec key at all still gets the fields.
    #[test]
    fn a_row_without_a_formalspec_key_still_gets_empty_args() {
        let mut rows = serde_json::json!([{"Name": "X", "ReturnType": "%Status"}]);
        assert_eq!(annotate_methods(&mut rows), 1);
        assert_eq!(rows[0]["args"], serde_json::json!([]));
        assert_eq!(rows[0]["arg_count"], 0);
    }

    /// Not an array — must be a no-op rather than a panic.
    #[test]
    fn a_non_array_is_a_no_op() {
        let mut v = serde_json::json!({"not": "an array"});
        assert_eq!(annotate_methods(&mut v), 0);
    }
}
