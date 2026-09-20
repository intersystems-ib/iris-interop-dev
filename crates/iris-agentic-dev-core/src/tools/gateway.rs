//! `iris_gateway_query` (#214) — a read-only SELECT against an external database reached
//! through a configured IRIS SQL Gateway connection.
//!
//! The problem #214 reports is not that the rows are unreachable — it is HOW they get reached.
//! The day's exercise ends in an external PostgreSQL table, so the operational question all day
//! is "how many rows are in `public.menus`?", and the only routes available were `psql` from
//! Bash with `PGPASSWORD` on the command line (32 invocations across 5 of 14 students), a GUI
//! SQL client, or poking the adapter. Two costs follow. The credential lands in the transcript
//! and in shell history. And verifying outside the loop makes it impossible to correlate the row
//! with the interop session that produced it, which was the point of the exercise.
//!
//! So this tool takes a CONNECTION NAME, never a credential. The username and password stay in
//! the IRIS SQL Gateway definition where an administrator put them; nothing secret crosses the
//! MCP boundary in either direction.
//!
//! Verified end-to-end against PostgreSQL 17 on 2026-09-18 (`e2e/gateway/docker-compose.yaml`):
//! 5 rows of `public.menus` read through IRIS for Health 2026.1, UTF-8 intact
//! (`Puré de patata`, `Crema de calabacín`), and an INSERT attempt refused by the database.
//!
//! ## Why the read-only guarantee is not a string check
//!
//! `validate_read_only_sql` was written for IRIS SQL. This tool's target is an arbitrary
//! external dialect, which has mutating statements that list does not know: PostgreSQL alone
//! adds `COPY ... FROM`, `VACUUM`, `GRANT`, `REVOKE`, `DO`, `CALL`, `REFRESH MATERIALIZED VIEW`,
//! `COMMENT`, `REINDEX`, `CLUSTER`, `NOTIFY`. Keyword screening cannot be the guarantee; a
//! dialect this code has never seen would slip through by construction.
//!
//! Three layers, weakest to strongest:
//!   1. `validate_read_only_sql` — the shared IRIS-oriented screen, so the common cases fail fast
//!      with a clear message instead of reaching the database.
//!   2. [`EXTRA_BLOCKED`] — dialect-specific mutators the shared list does not carry.
//!   3. `conn.SetReadOnly(1)` on the JDBC connection, enforced by the DRIVER AND SERVER rather
//!      than by parsing. This is the layer that actually holds.
//!
//! Layer 3 is the one to trust, and the documentation says so: point the gateway connection at a
//! role with `SELECT`-only grants. The e2e rig does exactly that (`gateway_ro`), and proves it —
//! an INSERT through that connection comes back `ERROR: permission denied`.

use crate::objectscript::os_str_expr;

/// Mutating statements the shared IRIS screen does not carry, because they are not IRIS SQL.
/// Not a claim of completeness — see the module docs. The real guarantee is `SetReadOnly(1)`
/// plus a `SELECT`-only role.
pub const EXTRA_BLOCKED: &[&str] = &[
    "COPY",
    "VACUUM",
    "GRANT",
    "REVOKE",
    "DO",
    "CALL",
    "REFRESH",
    "COMMENT",
    "REINDEX",
    "CLUSTER",
    "NOTIFY",
    "LISTEN",
    "UNLISTEN",
    "PREPARE",
    "DEALLOCATE",
    "DISCARD",
    "RESET",
    "CHECKPOINT",
    "ANALYZE",
    "SET",
];

/// Rows returned when the caller names no limit. A workshop question is "how many rows" or "show
/// me the last few", not "stream the table".
pub const DEFAULT_MAX_ROWS: u32 = 100;

/// Hard ceiling. A gateway round-trip materialises every row in the IRIS process before it is
/// serialised, so an unbounded fetch is a memory risk on the IRIS side, not just a slow answer.
pub const MAX_MAX_ROWS: u32 = 1000;

/// Screen the statement before it leaves for the external database.
///
/// Returns `Err(keyword)` naming what was rejected. `Ok(())` is NOT a promise that the statement
/// is read-only — see the module docs for why that promise belongs to `SetReadOnly(1)` and to the
/// database role.
pub fn validate_gateway_sql(sql: &str) -> Result<(), String> {
    crate::tools::validate_read_only_sql(sql)?;

    // The shared screen has already stripped comments and quoted literals for its own walk, but
    // it does not expose the cleaned text, so this pass re-tokenises the raw statement. A keyword
    // inside a string literal would therefore be a false positive here. That is the safe
    // direction to be wrong in: it refuses a legitimate query with a named reason rather than
    // forwarding a mutating one.
    let upper = sql.to_uppercase();
    for kw in EXTRA_BLOCKED {
        if contains_word(&upper, kw) {
            return Err((*kw).to_string());
        }
    }
    Ok(())
}

/// Word-boundary match, so `SET` does not fire on `OFFSET` and `DO` does not fire on `DOCTOR`.
fn contains_word(haystack_upper: &str, word: &str) -> bool {
    let bytes = haystack_upper.as_bytes();
    let w = word.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = haystack_upper[start..].find(word) {
        let at = start + pos;
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after = at + w.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Clamp a caller-supplied limit into the supported range.
pub fn clamp_max_rows(requested: Option<u32>) -> u32 {
    match requested {
        None => DEFAULT_MAX_ROWS,
        Some(0) => DEFAULT_MAX_ROWS,
        Some(n) if n > MAX_MAX_ROWS => MAX_MAX_ROWS,
        Some(n) => n,
    }
}

/// Does a named SQL Gateway connection exist, and does it connect?
///
/// `$SYSTEM.SQLGateway.TestConnection` is used rather than `%Net.Remote.Gateway.%Connect`,
/// because the latter returns %Status 1 for a port with nothing listening — measured on this
/// instance: ports 53773 and 59999 both reported success. `TestConnection` does discriminate:
/// an undefined name yields 0 and "Connection is not defined".
pub fn build_connection_test_code(connection: &str) -> String {
    format!(
        r#"set err = ""
set ok = $SYSTEM.SQLGateway.TestConnection({name}, 10, 0, .err)
set tOut = ##class(%DynamicObject).%New()
set tOut.ok = $SELECT(+ok=1:1, 1:0)
set tOut.error = $EXTRACT($PIECE(err, $CHAR(13), 1), 1, 400)
write tOut.%ToJSON()"#,
        name = os_str_expr(connection)
    )
}

/// The query itself. Emits JSON — never delimiter-separated text.
///
/// #246 shipped a `$CHAR(1)`-separated reader that reported `field_count: 0` for a 39-field
/// segment: a separator the payload also contains produces a clean, confident, wrong answer.
/// `%ToJSON()` has no such failure mode, and a row here can hold arbitrary external text.
pub fn build_query_code(connection: &str, sql: &str, max_rows: u32) -> String {
    format!(
        r#"set tOut = ##class(%DynamicObject).%New()
set tCols = ##class(%DynamicArray).%New()
set tRows = ##class(%DynamicArray).%New()
set tOut.truncated = 0
try {{
    set conn = ##class(%XDBC.Gateway.JDBC.Connection).GetConnection({name})
    if '$ISOBJECT(conn) {{
        set tOut.ok = 0
        set tOut.error = "GetConnection returned no object for this connection name"
    }} else {{
        do conn.SetReadOnly(1)
        set stmt = conn.CreateStatement()
        set rs = stmt.ExecuteQuery({sql})
        set md = rs.GetMetaData()
        set cc = md.GetColumnCount()
        for i=1:1:cc {{
            set tCol = ##class(%DynamicObject).%New()
            set tCol.name = md.GetColumnName(i)
            set tCol.type = md.GetColumnTypeName(i)
            do tCols.%Push(tCol)
        }}
        set n = 0
        while rs.Next() {{
            if n '< {max} {{
                set tOut.truncated = 1
                quit
            }}
            set n = n + 1
            set tRow = ##class(%DynamicArray).%New()
            for i=1:1:cc {{
                do tRow.%Push(rs.GetData(i))
            }}
            do tRows.%Push(tRow)
        }}
        do conn.Close()
        set tOut.ok = 1
    }}
}} catch e {{
    set tOut.ok = 0
    set tOut.error = $EXTRACT(e.DisplayString(), 1, 600)
}}
set tOut.columns = tCols
set tOut.rows = tRows
write tOut.%ToJSON()"#,
        name = os_str_expr(connection),
        sql = os_str_expr(sql),
        max = max_rows
    )
}

/// One column of a gateway result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewayColumn {
    pub name: String,
    /// The EXTERNAL database's own type name (`int4`, `text`, `serial` for PostgreSQL), not an
    /// IRIS type. Reporting the external name is the point: it tells the caller what the remote
    /// schema actually says.
    pub type_name: String,
}

/// A parsed gateway result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GatewayResult {
    pub columns: Vec<GatewayColumn>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
}

/// What the generator's output said about a gateway call — three outcomes, not two.
///
/// This was `Option<String>`: `Some(msg)` for a failure, `None` for anything else. The `None` arm
/// covered two unrelated facts, because the function opens with
/// `serde_json::from_str(..).ok()?` — so output that is **not JSON at all** returned `None`, which
/// reads as "no failure reported".
///
/// At the query site that was harmless: `parse_gateway_json` runs next and rejects non-JSON with
/// `GATEWAY_BAD_OUTPUT`. At the **connection-test** site it was the only check, and the test program
/// carries no `$ZTRAP` or `try`, so an IRIS-side exception escapes as raw text. Measured:
///
/// | connection-test output | old verdict |
/// |---|---|
/// | `"<CLASS DOES NOT EXIST> *%SYSTEM.SQLGateway"` | no error — proceed |
/// | `"ERROR #5002: ObjectScript error: <UNDEFINED>"` | no error — proceed |
/// | `""` | no error — proceed |
///
/// So the test silently passed and the stated guarantee — *"asked first so that 'not defined' and 'the
/// database refused the query' are different answers rather than one opaque failure"* — did not hold.
/// The module already knew non-JSON happens: `parse_gateway_json("not json").is_err()` is an existing
/// test. Only this parser assumed it.
#[derive(Debug, PartialEq, Eq)]
pub enum GatewayVerdict {
    /// `ok:1` — the call reported success.
    Reported,
    /// `ok:0`, or a missing `ok`, with whatever message came with it.
    Failed(String),
    /// Not JSON: the program died before writing its object, so there is NO verdict to read.
    NoVerdict(String),
}

/// Read the generator's output as a verdict about the gateway call.
pub fn parse_gateway_verdict(out: &str) -> GatewayVerdict {
    let trimmed = out.trim();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return GatewayVerdict::NoVerdict(trimmed.chars().take(400).collect());
    };
    let ok = v.get("ok").and_then(|o| o.as_i64()).unwrap_or(0);
    if ok == 1 {
        return GatewayVerdict::Reported;
    }
    let msg = v
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("the gateway call failed and reported no message")
        .trim();
    GatewayVerdict::Failed(if msg.is_empty() {
        "the gateway call failed and reported no message".to_string()
    } else {
        msg.to_string()
    })
}

/// `Some(message)` when the generator reported a failure rather than a result set.
///
/// Kept for the QUERY path, where a `None` on non-JSON is caught immediately afterwards by
/// `parse_gateway_json`. Do not use it for a check that has no second stage — see
/// [`GatewayVerdict`].
pub fn parse_gateway_error(out: &str) -> Option<String> {
    match parse_gateway_verdict(out) {
        GatewayVerdict::Failed(m) => Some(m),
        _ => None,
    }
}

/// Parse the generator's JSON into a result set.
///
/// A NULL and an empty string both arrive as `""` from `rs.GetData(i)` — the JDBC result-set
/// wrapper has no null indicator on that accessor. This is documented rather than papered over:
/// inventing a sentinel like "NULL" would collide with a row whose text really is `NULL`.
pub fn parse_gateway_json(out: &str) -> Result<GatewayResult, String> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(
            "the gateway returned no output at all — not an empty result set, no output. \
                    The generator did not run."
                .to_string(),
        );
    }
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("the gateway returned output that is not JSON: {e}"))?;

    let mut columns = Vec::new();
    if let Some(arr) = v.get("columns").and_then(|c| c.as_array()) {
        for c in arr {
            columns.push(GatewayColumn {
                name: c
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string(),
                type_name: c
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    let mut rows = Vec::new();
    if let Some(arr) = v.get("rows").and_then(|r| r.as_array()) {
        for r in arr {
            let mut row = Vec::new();
            if let Some(cells) = r.as_array() {
                for cell in cells {
                    row.push(match cell {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    });
                }
            }
            rows.push(row);
        }
    }

    // %DynamicObject stores a boolean set from an integer as a number, so accept either shape.
    // The sibling defect this avoids: iris_execute_method once parsed only 1/"1" and broke
    // outright when %Dictionary handed it JSON `true`.
    let truncated = match v.get("truncated") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    };

    Ok(GatewayResult {
        columns,
        rows,
        truncated,
    })
}

/// What to say when the named connection is not defined. Names the fix without naming a
/// credential.
pub fn connection_not_defined_message(connection: &str) -> String {
    format!(
        "SQL Gateway connection '{connection}' is not defined on this IRIS instance, so no \
         query was sent. Gateway connections are created in the Management Portal under System \
         Administration > Configuration > Connectivity > SQL Gateway Connections, and the \
         username and password are stored there — this tool takes only the connection NAME and \
         never accepts a credential. Ask an administrator which connection name to use."
    )
}

/// Rejection text for a statement that did not pass the screen.
pub fn rejected_sql_message(keyword: &str) -> String {
    if keyword == "EMPTY" {
        return "'query' is empty after comments were stripped, so nothing was sent.".to_string();
    }
    format!(
        "'{keyword}' is not allowed: iris_gateway_query is read-only and sends SELECT only. \
         Nothing was sent to the external database. The connection is also opened read-only, \
         and a gateway connection should point at a role with SELECT-only grants — that, not \
         this keyword screen, is what actually guarantees it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The connection test's only check used to be "did it report a failure", and non-JSON answered
    /// no. So an IRIS-side exception in a program with no $ZTRAP silently passed the test, and the
    /// guarantee it exists for — telling "connection not defined" apart from "the database refused the
    /// query" — quietly did not hold.
    #[test]
    fn output_that_is_not_json_is_no_verdict_not_a_pass() {
        for raw in [
            "<CLASS DOES NOT EXIST> *%SYSTEM.SQLGateway",
            "ERROR #5002: ObjectScript error: <UNDEFINED>",
            "",
            "   ",
        ] {
            match parse_gateway_verdict(raw) {
                GatewayVerdict::NoVerdict(got) => {
                    assert_eq!(
                        got,
                        raw.trim(),
                        "the raw text must survive: it is the only diagnosis"
                    )
                }
                other => panic!("{raw:?} read as {other:?} — the caller treats that as testable"),
            }
        }
    }

    /// The control: real verdicts must still be read, or the test above would pass on a parser that
    /// calls everything a non-verdict and breaks every gateway query.
    #[test]
    fn real_verdicts_are_still_read() {
        assert_eq!(
            parse_gateway_verdict(r#"{"ok":1,"columns":[],"rows":[]}"#),
            GatewayVerdict::Reported
        );
        assert_eq!(
            parse_gateway_verdict(r#"{"ok":0,"error":"refused"}"#),
            GatewayVerdict::Failed("refused".to_string())
        );
        // A missing `ok` stays a failure, as before — that case already had a test.
        assert!(matches!(
            parse_gateway_verdict(r#"{"columns":[]}"#),
            GatewayVerdict::Failed(_)
        ));
    }

    /// The old helper keeps its exact contract for the query path, where `parse_gateway_json` catches
    /// non-JSON immediately afterwards.
    #[test]
    fn parse_gateway_error_still_reports_none_for_non_json() {
        assert!(parse_gateway_error("<CLASS DOES NOT EXIST>").is_none());
        assert_eq!(
            parse_gateway_error(r#"{"ok":0,"error":"refused"}"#),
            Some("refused".to_string())
        );
    }

    #[test]
    fn a_plain_select_passes() {
        assert!(validate_gateway_sql("SELECT count(*) FROM public.menus").is_ok());
        assert!(validate_gateway_sql("select id_menu, descripcion from public.menus").is_ok());
    }

    #[test]
    fn the_shared_iris_screen_still_applies() {
        // Delegation is real, not re-implemented: these come from validate_read_only_sql.
        assert_eq!(
            validate_gateway_sql("INSERT INTO public.menus VALUES (1)").unwrap_err(),
            "INSERT"
        );
        assert_eq!(
            validate_gateway_sql("DROP TABLE public.menus").unwrap_err(),
            "DROP"
        );
        assert_eq!(validate_gateway_sql("   ").unwrap_err(), "EMPTY");
    }

    #[test]
    fn postgres_mutators_the_shared_screen_does_not_know_are_blocked() {
        // Each of these passes validate_read_only_sql, which is the whole reason this list
        // exists. If the shared screen ever learns them, these still hold.
        for (sql, kw) in [
            ("COPY public.menus FROM '/tmp/x.csv'", "COPY"),
            ("VACUUM FULL public.menus", "VACUUM"),
            ("GRANT ALL ON public.menus TO bob", "GRANT"),
            ("REVOKE SELECT ON public.menus FROM bob", "REVOKE"),
            ("DO $$ BEGIN PERFORM 1; END $$", "DO"),
            ("CALL some_procedure()", "CALL"),
            ("REFRESH MATERIALIZED VIEW mv", "REFRESH"),
            ("COMMENT ON TABLE public.menus IS 'x'", "COMMENT"),
            ("REINDEX TABLE public.menus", "REINDEX"),
            ("NOTIFY channel", "NOTIFY"),
            ("SET search_path TO evil", "SET"),
            ("CHECKPOINT", "CHECKPOINT"),
        ] {
            assert_eq!(
                validate_gateway_sql(sql).unwrap_err(),
                kw,
                "should have been rejected: {sql}"
            );
        }
    }

    #[test]
    fn word_boundaries_do_not_produce_false_rejections() {
        // OFFSET contains SET; a column named "docentes" contains DO; "clustered" contains
        // CLUSTER. A tool that refuses LIMIT/OFFSET is useless for "show me the last few".
        assert!(validate_gateway_sql("SELECT * FROM t LIMIT 10 OFFSET 20").is_ok());
        assert!(validate_gateway_sql("SELECT docentes FROM escuela").is_ok());
        assert!(validate_gateway_sql("SELECT clustered_flag FROM t").is_ok());
        assert!(validate_gateway_sql("SELECT calorias FROM public.menus").is_ok());
        // ANALYZE is blocked, but "analyzed_at" as a column must not be.
        assert!(validate_gateway_sql("SELECT analyzed_at FROM t").is_ok());
    }

    #[test]
    fn max_rows_is_clamped_and_defaulted() {
        assert_eq!(clamp_max_rows(None), DEFAULT_MAX_ROWS);
        assert_eq!(clamp_max_rows(Some(0)), DEFAULT_MAX_ROWS);
        assert_eq!(clamp_max_rows(Some(5)), 5);
        assert_eq!(clamp_max_rows(Some(MAX_MAX_ROWS)), MAX_MAX_ROWS);
        assert_eq!(clamp_max_rows(Some(MAX_MAX_ROWS + 1)), MAX_MAX_ROWS);
        assert_eq!(clamp_max_rows(Some(u32::MAX)), MAX_MAX_ROWS);
    }

    #[test]
    fn generated_code_escapes_and_never_uses_a_delimiter() {
        let code = build_query_code("PG_X", "SELECT 'it''s' FROM t", 10);
        assert!(code.contains("%ToJSON()"), "{code}");
        assert!(
            !code.contains("$CHAR(1)"),
            "delimiters are the #246 bug: {code}"
        );
        assert!(
            code.contains("SetReadOnly(1)"),
            "the real guarantee: {code}"
        );
        // SQL single quotes are NOT special to ObjectScript and pass through unchanged. An
        // earlier version of this test asserted they were doubled; they are not. os_str_expr
        // doubles the ObjectScript delimiter, which is the DOUBLE quote.
        assert!(code.contains("SELECT 'it''s' FROM t"), "{code}");

        // The double quote is what gets doubled — never backslash-escaped.
        let dq = build_query_code("PG_X", "SELECT \"col\" FROM t", 10);
        assert!(dq.contains("SELECT \"\"col\"\" FROM t"), "{dq}");
        assert!(
            !dq.contains('\\'),
            "backslash is not an ObjectScript escape: {dq}"
        );

        // Non-ASCII is spliced as $CHAR so the rendered expression stays pure ASCII. The
        // measured rows carry accents, so this path is real rather than hypothetical.
        let accented = build_query_code("PG_X", "SELECT 'Pur\u{e9}' FROM t", 10);
        assert!(accented.contains("$CHAR(233)"), "{accented}");
    }

    #[test]
    fn generated_code_carries_the_row_cap() {
        assert!(build_query_code("C", "SELECT 1", 42).contains("n '< 42"));
    }

    #[test]
    fn parses_a_real_result_set() {
        let out = r#"{"truncated":0,"ok":1,
            "columns":[{"name":"id_menu","type":"serial"},{"name":"descripcion","type":"text"}],
            "rows":[["1","Puré de patata"],["3","Crema de calabacín"]]}"#;
        let r = parse_gateway_json(out).expect("should parse");
        assert_eq!(r.columns.len(), 2);
        assert_eq!(r.columns[0].name, "id_menu");
        assert_eq!(r.columns[0].type_name, "serial");
        assert_eq!(r.rows.len(), 2);
        // UTF-8 must survive the round trip — the measured rows contain accents.
        assert_eq!(r.rows[0][1], "Puré de patata");
        assert_eq!(r.rows[1][1], "Crema de calabacín");
        assert!(!r.truncated);
    }

    #[test]
    fn truncated_is_read_whether_it_arrives_as_number_bool_or_string() {
        for raw in [
            r#"{"ok":1,"truncated":1,"columns":[],"rows":[]}"#,
            r#"{"ok":1,"truncated":true,"columns":[],"rows":[]}"#,
            r#"{"ok":1,"truncated":"1","columns":[],"rows":[]}"#,
        ] {
            assert!(parse_gateway_json(raw).unwrap().truncated, "{raw}");
        }
        for raw in [
            r#"{"ok":1,"truncated":0,"columns":[],"rows":[]}"#,
            r#"{"ok":1,"truncated":false,"columns":[],"rows":[]}"#,
            r#"{"ok":1,"columns":[],"rows":[]}"#,
        ] {
            assert!(!parse_gateway_json(raw).unwrap().truncated, "{raw}");
        }
    }

    #[test]
    fn an_empty_result_set_is_not_an_error_but_no_output_is() {
        let empty = parse_gateway_json(r#"{"ok":1,"truncated":0,"columns":[],"rows":[]}"#).unwrap();
        assert_eq!(empty.rows.len(), 0);

        // Silence must NOT read as "zero rows" — that is the failure this repo keeps hitting.
        let err = parse_gateway_json("   ").unwrap_err();
        assert!(err.contains("no output at all"), "{err}");
        assert!(parse_gateway_json("not json").is_err());
    }

    #[test]
    fn a_gateway_failure_is_reported_not_swallowed() {
        let out = r#"{"ok":0,"error":"<GATEWAY> PSQLException ERROR: permission denied","columns":[],"rows":[]}"#;
        let msg = parse_gateway_error(out).expect("should be an error");
        assert!(msg.contains("permission denied"), "{msg}");
        // A successful call reports no error.
        assert!(parse_gateway_error(r#"{"ok":1,"columns":[],"rows":[]}"#).is_none());
        // ok missing entirely must be treated as failure, not success.
        assert!(parse_gateway_error(r#"{"columns":[]}"#).is_some());
    }

    #[test]
    fn the_not_defined_message_names_the_fix_and_no_credential() {
        let m = connection_not_defined_message("PG_COCINA");
        assert!(m.contains("PG_COCINA"));
        assert!(m.contains("SQL Gateway Connections"));
        assert!(m.contains("never accepts a credential"));
        // Naming a remediation must not hand out a bypass, and must not leak a secret shape.
        assert!(!m.to_lowercase().contains("pgpassword"), "{m}");
    }

    #[test]
    fn the_rejection_message_says_where_the_guarantee_lives() {
        let m = rejected_sql_message("COPY");
        assert!(m.contains("COPY"));
        assert!(m.contains("Nothing was sent"));
        assert!(m.contains("SELECT-only"), "{m}");
        assert!(rejected_sql_message("EMPTY").contains("empty"));
    }

    #[test]
    fn the_test_connection_code_uses_the_api_that_discriminates() {
        let code = build_connection_test_code("PG_X");
        assert!(code.contains("$SYSTEM.SQLGateway.TestConnection"), "{code}");
        // %Connect returns success for a dead port — measured on ports 53773 and 59999.
        assert!(
            !code.contains("%Net.Remote.Gateway"),
            "that API cannot tell a live port from a dead one: {code}"
        );
    }
}

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GatewayQueryParams {
    /// NAME of a SQL Gateway connection configured on this IRIS instance (for example
    /// "PG_COCINA"). This tool never accepts a host, user, or password: the credential stays in
    /// the IRIS gateway definition, which is the point — running `psql` by hand puts PGPASSWORD
    /// into the transcript and shell history.
    pub connection: String,
    /// A read-only SELECT to run on the EXTERNAL database, in THAT database's SQL dialect (not
    /// IRIS SQL). Mutating statements are refused and never sent.
    pub query: String,
    /// Maximum rows to return. Default 100, hard maximum 1000. A truncated result says so.
    #[serde(default)]
    pub max_rows: Option<u32>,
    /// IRIS namespace whose gateway definitions to use. OMIT this field to use the connection's
    /// configured namespace (IRIS_NAMESPACE) — only pass a value to deliberately target a
    /// different namespace.
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn handle_gateway_query(
    iris: &crate::iris::connection::IrisConnection,
    client: &reqwest::Client,
    p: GatewayQueryParams,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let namespace = crate::tools::interop::resolve_namespace(p.namespace.as_deref(), Some(iris));

    let connection = p.connection.trim();
    if connection.is_empty() {
        return crate::tools::envelope::fail_with(
            "MISSING_PARAMS",
            "'connection' is required — the NAME of a SQL Gateway connection configured on this \
             IRIS instance. Nothing was run.",
            serde_json::json!({ "namespace": namespace }),
        );
    }
    let sql = p.query.trim();
    if sql.is_empty() {
        return crate::tools::envelope::fail_with(
            "MISSING_PARAMS",
            "'query' is required — a read-only SELECT in the external database's SQL dialect. \
             Nothing was run.",
            serde_json::json!({ "connection": connection, "namespace": namespace }),
        );
    }

    if let Err(keyword) = validate_gateway_sql(sql) {
        return crate::tools::envelope::fail_with(
            "SQL_NOT_READ_ONLY",
            &rejected_sql_message(&keyword),
            serde_json::json!({
                "connection": connection,
                "namespace": namespace,
                "rejected": keyword,
            }),
        );
    }

    let max_rows = clamp_max_rows(p.max_rows);

    // Does the connection exist and connect? Asked first so that "not defined" and "the database
    // refused the query" are different answers rather than one opaque failure.
    let test_out = match iris
        .execute_via_generator(&build_connection_test_code(connection), &namespace, client)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_gateway_query", &e.to_string())
        }
    };
    // The connection test has no second stage behind it, so "found no failure" must not be the same
    // answer as "could not tell". Without this, an IRIS-side exception in the test program — which
    // carries no $ZTRAP — escaped as raw text, read as no-error, and the query ran anyway.
    if let GatewayVerdict::NoVerdict(raw) = parse_gateway_verdict(&test_out) {
        return crate::tools::envelope::fail_with(
            "GATEWAY_BAD_OUTPUT",
            &format!(
                "the SQL Gateway connection test for '{connection}' returned output that is not a \
                 verdict, so whether the connection works is unknown and no query was sent. IRIS \
                 wrote: {raw}"
            ),
            serde_json::json!({
                "connection": connection,
                "namespace": namespace,
                "iris_output": raw,
            }),
        );
    }
    if let Some(msg) = parse_gateway_error(&test_out) {
        let not_defined = msg.to_lowercase().contains("not defined");
        return crate::tools::envelope::fail_with(
            if not_defined {
                "GATEWAY_CONNECTION_NOT_DEFINED"
            } else {
                "GATEWAY_CONNECT_FAILED"
            },
            &if not_defined {
                connection_not_defined_message(connection)
            } else {
                format!(
                    "SQL Gateway connection '{connection}' is defined but did not connect, so no \
                     query was sent. IRIS reported: {msg}"
                )
            },
            serde_json::json!({
                "connection": connection,
                "namespace": namespace,
                "iris_error": msg,
            }),
        );
    }

    let out = match iris
        .execute_via_generator(
            &build_query_code(connection, sql, max_rows),
            &namespace,
            client,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return crate::tools::envelope::transport_fail("handle_gateway_query", &e.to_string())
        }
    };

    if let Some(msg) = parse_gateway_error(&out) {
        return crate::tools::envelope::fail_with(
            "GATEWAY_QUERY_FAILED",
            &format!(
                "the external database rejected the query. This is the remote database's own \
                 error, passed through unchanged: {msg}"
            ),
            serde_json::json!({
                "connection": connection,
                "namespace": namespace,
                "iris_error": msg,
            }),
        );
    }

    let result = match parse_gateway_json(&out) {
        Ok(r) => r,
        Err(e) => {
            return crate::tools::envelope::fail_with(
                "GATEWAY_BAD_OUTPUT",
                &e,
                serde_json::json!({ "connection": connection, "namespace": namespace }),
            )
        }
    };

    let mut obj = serde_json::json!({
        "success": true,
        "connection": connection,
        "namespace": namespace,
        "columns": result.columns,
        "rows": result.rows,
        "row_count": result.rows.len(),
        "truncated": result.truncated,
    });
    if result.truncated {
        obj["note"] = format!(
            "truncated at max_rows={max_rows}; there are more rows. Raise max_rows (maximum \
             {MAX_MAX_ROWS}) or narrow the query — a COUNT(*) is cheaper than paging."
        )
        .into();
    }
    if result.rows.is_empty() {
        obj["note"] = "the query succeeded and matched zero rows. This is an empty result set, \
                       not a failure — the connection worked and the external database answered."
            .into();
    }
    crate::tools::envelope::ok_json(obj)
}
