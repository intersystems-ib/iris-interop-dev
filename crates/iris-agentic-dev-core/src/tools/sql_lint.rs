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
}
