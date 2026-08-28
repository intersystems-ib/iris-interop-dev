//! Lightweight, IRIS-flavoured SQL lints for `iris_query` — pure functions, unit-testable
//! without a live IRIS. Targets the two dominant workshop failure shapes (ObjectScript typed
//! into a SQL tool, ~28 of 447 calls; and unquoted IRIS reserved words / wrong separators,
//! ~26 calls), plus a table-not-found classifier so the handler can point at `iris_table_info`
//! instead of letting the model guess nonexistent catalog tables (84 calls).

/// IRIS SQL reserved words that commonly bite when used as *unquoted* identifiers.
/// Not exhaustive — the high-frequency ones from the workshop logs + the IRIS SQL manual.
const RESERVED: &[&str] = &[
    "CONNECTION",
    "DEFAULT",
    "DOMAIN",
    "LANGUAGE",
    "OUTPUT",
    "USER",
    "VALUE",
    "ROLE",
    "WINDOW",
    "SECTION",
    "SYSTEM",
    "DATA",
    "FILE",
    "NAME",
    "TIME",
    "ZONE",
    "SIZE",
    "PUBLIC",
    "PRIVATE",
    "OPERATION",
    "STATEMENT",
    "WORK",
];

/// If the text looks like ObjectScript rather than SQL, return a short reason.
/// Conservative: only full-word leading commands and ObjectScript-only syntax, so a
/// legitimate `SELECT`/`WITH`/`VALUES` query is never misflagged.
pub fn looks_like_objectscript(query: &str) -> Option<&'static str> {
    let q = query.trim_start();
    let lower = q.to_ascii_lowercase();
    const OS_STARTS: &[&str] = &[
        "set ", "write ", "do ", "kill ", "new ", "quit", "zwrite ", "zn ", "merge ^", "if ",
        "for ", "while ",
    ];
    if OS_STARTS.iter().any(|p| lower.starts_with(p)) {
        return Some("starts with an ObjectScript command (set/write/do/kill/new/quit/...)");
    }
    if lower.contains("##class(") {
        return Some("contains ##class(...) — ObjectScript, not SQL");
    }
    if lower.contains("&sql(") {
        return Some(
            "contains &sql(...) — embedded SQL; run the surrounding ObjectScript via iris_execute",
        );
    }
    if q.starts_with('^') {
        return Some("starts with a ^global reference — ObjectScript, not SQL");
    }
    None
}

/// Non-fatal warnings for unquoted IRIS reserved words used as identifiers.
/// Skips words inside delimited identifiers / string literals.
pub fn reserved_word_warnings(query: &str) -> Vec<String> {
    let stripped = strip_quoted(query).to_ascii_uppercase();
    let mut out = Vec::new();
    for w in RESERVED {
        if contains_word(&stripped, w) {
            out.push(format!(
                "'{w}' is an IRIS SQL reserved word; if it names a column/table, delimit it with double quotes (\"{w}\")."
            ));
        }
    }
    out
}

/// Does the SQL error look like a missing-table/view/field error? (IRIS SQLCODE -30 etc.)
pub fn is_table_not_found(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("table or view not found")
        || low.contains("table not found")
        || low.contains("sqlcode: -30")
        || low.contains("sqlcode=-30")
        || low.contains("sqlcode -30")
        || (low.contains("not found") && (low.contains("table") || low.contains("class")))
}

/// Hint string pointing the model at the real schema-discovery path instead of guessing.
pub const TABLE_NOT_FOUND_HINT: &str = "Table/view not found. Call iris_table_info(schema=...) to \
list the real tables and columns before querying. IRIS SQL uses Schema.Table and maps package \
dots to '_' (e.g. class Ens.Util.Log -> table Ens_Util.Log; Ens.MessageHeader stays Ens.MessageHeader).";

/// The SQLCODE an IRIS SQL error reports, in any of the three spellings seen in the wild
/// (`SQLCODE: -359`, `SQLCODE=-359`, `SQLCODE -359`).
pub fn sqlcode(err: &str) -> Option<i32> {
    let low = err.to_ascii_lowercase();
    let at = low.find("sqlcode")? + "sqlcode".len();
    let rest = low[at..].trim_start_matches([':', '=', ' ']);
    let end = rest
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && *c == '-')))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Hints for the SQLCODEs the table did not reach (#126).
///
/// The two most common codes (-30 table not found, -29 field not found) were already covered
/// and are demonstrably effective; -359 was the third most common and the largest single
/// unhinted group, and it is the one where a hint helps most. `STRING_AGG` is real in other
/// dialects, so a model reaching for it learns only that THAT name is absent and tries the next
/// synonym — nothing tells it IRIS spells this `LIST()`.
///
/// Both suggestions below were executed against IRIS 2026.1 before being written down.
pub fn sqlcode_hint(err: &str) -> Option<&'static str> {
    match sqlcode(err)? {
        -359 => Some(
            "No such SQL function in IRIS. IRIS does not carry the T-SQL/Postgres aggregate \
             names: string aggregation is LIST(expr) (verified: SELECT LIST(Name) FROM \
             Ens_Config.Production), and $LIST-valued columns use %DLIST. To see which class \
             methods ARE callable from SQL: SELECT parent, Name FROM %Dictionary.CompiledMethod \
             WHERE SqlProc = 1 AND parent %STARTSWITH '<Package>'.",
        ),
        -1 => Some(
            "SQLCODE -1 is a parse error. Two things to check first in IRIS SQL: a package maps \
             to a schema with underscores (class Ens.Config.Item -> table Ens_Config.Item), and \
             string concatenation is || — `+` is NUMERIC addition here and silently yields 0 on \
             text rather than erroring. Call iris_table_info to confirm the real schema, table \
             and column names.",
        ),
        _ => None,
    }
}

/// Targeted redirect for the specific nonexistent catalog tables the model repeatedly guessed
/// in the retest (67% of system-table guesses failed: SQL-Gateway, namespace, production
/// config/status/settings tables). Returns a hint naming the typed tool/approach to use
/// instead of guessing, or None when the SQL doesn't match a known guess. Checked BEFORE the
/// generic TABLE_NOT_FOUND_HINT so the model gets the specific tool, not "go look it up".
pub fn targeted_table_hint(sql: &str) -> Option<&'static str> {
    let u = sql.to_ascii_uppercase();
    // SQL-Gateway / external language gateway connections — there is no SQL catalog table.
    if u.contains("SQLCONNECTION")
        || u.contains("SQLGATEWAY")
        || u.contains("OBJECTGATEWAY")
        || u.contains("CONFIG.GATEWAYS")
        || u.contains("CONFIG.SQLCONNECTIONS")
    {
        return Some("SQL-Gateway / external-gateway connections are NOT a queryable SQL table. \
Use the introspect-dont-guess agent or iris_table_info to resolve real names; the active connection's \
config is in check_config.");
    }
    // Namespace enumeration — not a SQL table in the interop toolset.
    if u.contains("%SYS.NAMESPACE")
        || u.contains("CONFIG.NAMESPACES")
        || u.contains("NAMESPACE_LIST")
        || u.contains("%SYS.NAMESPACES")
    {
        return Some(
            "The namespace list is not a SQL table. The connected namespace is reported by \
check_config; switch namespace with the tool's namespace= argument.",
        );
    }
    // Production items / settings / status — use the typed production tools.
    if u.contains("ENS_CONFIG.SETTING")
        || u.contains("ENS_CONFIG.ITEM")
        || u.contains("ITEMSETTINGS")
        || u.contains("ITEM_SETTINGS")
        || (u.contains("ENS_CONFIG.PRODUCTION") && u.contains("STATUS"))
    {
        return Some(
            "Don't query Ens_Config item/setting/status tables directly — use \
iris_production(action=status) for production state and iris_production_item(action=get_settings) \
for an item's settings.",
        );
    }
    // Search-table indexed properties.
    if u.contains("SEARCHTABLEPROP") {
        return Some(
            "Use iris_table_info on the SearchTable class to see its indexed properties \
instead of guessing Ens_Config.SearchTableProp.",
        );
    }
    None
}

/// Advisory redirect for `iris_execute` payloads that should have used a typed tool.
/// Pure, non-blocking: the handler runs the code anyway and only attaches the returned hint.
/// Targets the dominant Round-4 waste shapes — ~60% of iris_execute calls were introspection
/// or ad-hoc SQL the typed tools answer in one round-trip, plus the load-from-file anti-pattern.
/// Priority: filesystem-load (worst, host-coupling) > production/catalog config > class
/// dictionary introspection > bare SELECT. Returns None for legitimate side-effecting ObjectScript
/// (object %New/%Save, production control, globals, etc.).
pub fn execute_redirect_hint(code: &str) -> Option<&'static str> {
    let u = code.to_ascii_uppercase();

    // 1. Loading/compiling classes from a filesystem path. Only works when IRIS shares the MCP
    //    host's disk — an anti-pattern. iris_doc/iris_compile push source over Atelier instead.
    if u.contains("$SYSTEM.OBJ.LOAD")
        || u.contains("$SYSTEM.OBJ.IMPORT")
        || u.contains("STUDIOOPENDOCUMENT")
        || u.contains(".OBJ.LOADDIR")
        || u.contains(".OBJ.LOADSTREAM")
    {
        return Some(
            "Loading/compiling a class from a filesystem path makes IRIS read THIS host's disk — \
it only works when IRIS and the MCP share a filesystem (anti-pattern; breaks once they're on \
different hosts). Send the source over Atelier instead: iris_doc(action=put, compile=true) writes \
and compiles a class in-memory, or iris_compile compiles an existing document by name. \
Host-independent and the supported path.",
        );
    }

    // 2. Production / interop config read as ad-hoc SQL — reuse the typed-tool redirects.
    if let Some(h) = targeted_table_hint(code) {
        return Some(h);
    }

    // 3. Class/dictionary introspection via %Dictionary.* SQL — typed tools do this in one call.
    if u.contains("%DICTIONARY.") {
        return Some(
            "Introspect classes with typed tools, not %Dictionary SQL: docs_introspect(class=...) \
for methods/properties, iris_symbols(pattern=...) to find classes, iris_table_info(schema=...) for \
projected tables. One typed call, no guessing at catalog table/column names.",
        );
    }

    // 4. A bare SELECT, or %SQL.Statement used only to READ rows — that's iris_query's job.
    //    Excludes writes (INSERT/UPDATE/DELETE/MERGE/CALL) which iris_query blocks by design.
    let is_write = u.contains("INSERT ")
        || u.contains("UPDATE ")
        || u.contains("DELETE ")
        || u.contains("MERGE ")
        || u.contains(" CALL ");
    let reads_via_sql = u.trim_start().starts_with("SELECT ")
        || (u.contains("%SQL.STATEMENT") && u.contains("SELECT "));
    if reads_via_sql && !is_write {
        return Some(
            "This reads rows via SQL — use iris_query(query=...) instead of hand-rolling \
%SQL.Statement inside iris_execute. iris_query returns typed rows directly and avoids the \
<SYNTAX>errdone+2^%qaqqt failures that malformed dynamic SQL throws through iris_execute.",
        );
    }

    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whole-word match of an already-uppercased `word` in an already-uppercased haystack.
fn contains_word(haystack_upper: &str, word: &str) -> bool {
    let bytes = haystack_upper.as_bytes();
    let mut i = 0;
    while let Some(pos) = haystack_upper[i..].find(word) {
        let start = i + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
        if i >= haystack_upper.len() {
            break;
        }
    }
    false
}

/// Replace double-quoted delimited identifiers and single-quoted string literals with
/// spaces, so reserved-word scanning doesn't flag already-delimited names or string contents.
fn strip_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for d in chars.by_ref() {
                    if d == '"' {
                        break;
                    }
                }
                out.push(' ');
            }
            '\'' => {
                for d in chars.by_ref() {
                    if d == '\'' {
                        break;
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objectscript_detected() {
        assert!(looks_like_objectscript("set x = 1").is_some());
        assert!(looks_like_objectscript("  write $ZVERSION").is_some());
        assert!(looks_like_objectscript("do ##class(Foo).Bar()").is_some());
        assert!(looks_like_objectscript("SELECT id, ##class(X).Y() FROM t").is_some());
        assert!(looks_like_objectscript("^Ens.Config").is_some());
        assert!(looks_like_objectscript("&sql(select 1 into :x)").is_some());
    }

    #[test]
    fn real_sql_not_flagged_as_objectscript() {
        assert!(looks_like_objectscript("SELECT * FROM Ens_Util.Log").is_none());
        assert!(looks_like_objectscript("  select TimeLogged from Ens_Util.Log").is_none());
        assert!(looks_like_objectscript("WITH t AS (SELECT 1 x) SELECT * FROM t").is_none());
    }

    #[test]
    fn reserved_words_flagged_unless_quoted() {
        let w = reserved_word_warnings("SELECT Connection, Default FROM Config.SQLGateways");
        assert!(w.iter().any(|m| m.contains("CONNECTION")));
        assert!(w.iter().any(|m| m.contains("DEFAULT")));
        // delimited identifiers must NOT be flagged
        let w2 = reserved_word_warnings("SELECT \"Connection\", \"Default\" FROM t");
        assert!(
            w2.is_empty(),
            "delimited identifiers should not warn: {w2:?}"
        );
        // substrings of larger identifiers must NOT be flagged
        let w3 = reserved_word_warnings("SELECT UserName, DataValue FROM t");
        assert!(w3.is_empty(), "substring matches should not warn: {w3:?}");
    }

    #[test]
    fn table_not_found_classified() {
        assert!(is_table_not_found("Table 'SQLUSER.FOO' not found"));
        assert!(is_table_not_found("[SQLCODE: -30] Table or view not found"));
        assert!(!is_table_not_found("Syntax error near 'FROM'"));
    }

    #[test]
    fn targeted_hints_redirect_known_guesses() {
        assert!(targeted_table_hint("SELECT * FROM Config.Gateways")
            .unwrap()
            .contains("SQL-Gateway"));
        assert!(targeted_table_hint("SELECT * FROM %Library.SQLConnection")
            .unwrap()
            .contains("SQL-Gateway"));
        assert!(targeted_table_hint("SELECT NAME FROM %SYS.Namespace")
            .unwrap()
            .contains("namespace"));
        assert!(targeted_table_hint("SELECT STATUS FROM Ens_Config.Item")
            .unwrap()
            .contains("iris_production"));
        // real domain tables must NOT be hinted
        assert!(targeted_table_hint("SELECT * FROM Hospital.Patient").is_none());
        assert!(targeted_table_hint("SELECT * FROM Ens.MessageHeader").is_none());
    }

    #[test]
    fn execute_redirect_flags_load_from_file() {
        let h = execute_redirect_hint(
            "set sc = $System.OBJ.Load(\"C:\\src\\Cocina\\Production.cls\",\"cuk\")",
        )
        .unwrap();
        assert!(h.contains("Atelier"), "{h}");
        assert!(h.contains("iris_doc"), "{h}");
        assert!(
            execute_redirect_hint("Do ##class(%Studio.Project).StudioOpenDocument(f)").is_some()
        );
    }

    #[test]
    fn execute_redirect_flags_adhoc_sql_and_introspection() {
        // bare SELECT pasted into iris_execute
        assert!(execute_redirect_hint("SELECT Name FROM Hospital.Patient")
            .unwrap()
            .contains("iris_query"));
        // %SQL.Statement read
        assert!(execute_redirect_hint(
            "set rs=##class(%SQL.Statement).%ExecDirect(,\"SELECT Nsp FROM %SYS.Namespace_List()\")"
        )
        .is_some());
        // %Dictionary introspection -> typed tools
        assert!(execute_redirect_hint(
            "set rs=##class(%SQL.Statement).%ExecDirect(,\"SELECT Name FROM %Dictionary.ClassDefinition WHERE Name LIKE 'Cocina.%'\")"
        )
        .unwrap()
        .contains("docs_introspect"));
        // Ens_Config catalog -> production tools (via targeted_table_hint)
        assert!(execute_redirect_hint("SELECT Name FROM Ens_Config.Item")
            .unwrap()
            .contains("iris_production"));
    }

    #[test]
    fn execute_redirect_silent_on_legit_objectscript() {
        // object save, production control, globals, a writing %SQL.Statement — no redirect
        assert!(
            execute_redirect_hint("set o=##class(Cocina.MSG.MenuRequest).%New() do o.%Save()")
                .is_none()
        );
        assert!(execute_redirect_hint(
            "set sc=##class(Ens.Director).StartProduction(\"Cocina.Production\")"
        )
        .is_none());
        assert!(execute_redirect_hint("write $ZVERSION,!").is_none());
        assert!(execute_redirect_hint(
            "set rs=##class(%SQL.Statement).%ExecDirect(,\"INSERT INTO public.menus VALUES (?)\",1)"
        )
        .is_none());
    }
}

/// #126 — the SQLCODEs the hint table did not reach. Messages are verbatim from the corpus.
#[cfg(test)]
mod sqlcode_hint_tests {
    use super::{is_table_not_found, sqlcode, sqlcode_hint};

    const F359: &str = "ERROR #5540: SQLCODE: -359 Message:  User defined SQL function 'SQLUSER.STRING_AGG' does not exist";
    const F30: &str =
        "ERROR #5540: SQLCODE: -30 Message: Table 'SQLUSER.LABCSV_DATA_LABRESULT' not found";
    const F29: &str = "ERROR #5540: SQLCODE: -29 Message: Field 'VALUE' not found in the applicable tables^ SELECT Name , Value FROM";

    #[test]
    fn the_code_is_read_in_every_spelling() {
        assert_eq!(sqlcode(F359), Some(-359));
        assert_eq!(sqlcode("SQLCODE=-30 something"), Some(-30));
        assert_eq!(sqlcode("sqlcode -1 near ')'"), Some(-1));
        assert_eq!(sqlcode("no code here"), None);
    }

    /// The largest unhinted group: 16 occurrences, no hint at all.
    #[test]
    fn a_missing_sql_function_now_names_the_iris_equivalent() {
        let h = sqlcode_hint(F359).expect("-359 must be hinted");
        assert!(h.contains("LIST("), "must name the IRIS spelling: {h}");
        assert!(h.contains("SqlProc"), "must point at the catalog: {h}");
    }

    #[test]
    fn a_syntax_error_names_only_verified_iris_behaviour() {
        let h = sqlcode_hint("ERROR #5540: SQLCODE: -1 Message: Syntax error").expect("-1 hinted");
        // Only claims verified against IRIS 2026.1 belong here. An earlier draft said "there is
        // no LIMIT/OFFSET"; the instance accepts both, so that sentence was removed rather than
        // shipped. `||` concatenates and `+` coerces text to 0 — both executed before writing.
        assert!(
            h.contains("||"),
            "must name the concatenation operator: {h}"
        );
        assert!(
            !h.contains("LIMIT"),
            "unverified LIMIT claim came back: {h}"
        );
    }

    /// The two codes that already had coverage must keep reaching their own hint, not this one:
    /// the new branch runs only after `is_table_not_found` declines.
    #[test]
    fn the_codes_that_were_already_covered_are_untouched() {
        for msg in [F30, F29] {
            assert!(is_table_not_found(msg), "existing coverage lost for: {msg}");
        }
    }

    #[test]
    fn an_unmapped_code_still_gets_no_invented_advice() {
        assert!(sqlcode_hint("ERROR #5540: SQLCODE: -12 Message: whatever").is_none());
    }
}
