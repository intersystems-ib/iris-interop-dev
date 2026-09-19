//! #24 item 057 (sql-power): `explain` and `count` modes for `iris_query`.
//!
//! WHAT IS DELIBERATELY NOT HERE: a `write` mode.
//!
//! `iris_query` already has one write path — `force: true` — and it is already wired into the write
//! gate (`mutating_call("iris_query", {"force": true})` returns `Some("run forced SQL")`). A second
//! mode reaching the same capability would be a second gate decision for one capability, which is the
//! report-vs-enforce split this repo has been bitten by repeatedly. #24 also lists upstream's 073
//! (destructive gate) and 074 (write allowlist) as explicit do-NOT-port, and a write mode belongs to
//! that family. Reported on the issue rather than implemented.
//!
//! EVERYTHING BELOW WAS VERIFIED AGAINST IRIS FOR HEALTH 2026.1, not assumed:
//!
//! * `EXPLAIN <select>` works and returns ONE row with ONE column named `Plan`, holding XML with the
//!   normalised SQL, a `Cost:` line and the module plan.
//! * `SELECT COUNT(*) FROM (<select>)` works, and respects a `TOP` in the inner query (`TOP 2` → 2).
//! * A trailing `ORDER BY` in the inner query FAILS: `SQLCODE -1, ) expected, IDENTIFIER (ORDER)
//!   found`. Measured, not guessed — which is why it is stripped.
//! * `WHERE Name = 'ORDER BY x'` is fine, so the strip must be quote-aware or it corrupts that query.

/// What `iris_query` should do with the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// Return the rows. The default, and the behaviour before this existed.
    Rows,
    /// Return how many rows the statement would produce, without transferring them.
    Count,
    /// Return the query plan and its cost, without running the statement.
    Explain,
}

impl QueryMode {
    /// Parse the advertised values, case-insensitively. `None` for anything else, so the caller can
    /// name the valid set rather than silently running the wrong mode.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "rows" => Some(Self::Rows),
            "count" => Some(Self::Count),
            "explain" => Some(Self::Explain),
            _ => None,
        }
    }

    pub fn valid_values() -> &'static str {
        "rows (default), count, explain"
    }
}

/// SQL that has already passed `iris_query`'s read-only gate (or was force-authorised).
///
/// A NEWTYPE rather than a `&str`, so the transform below CANNOT be applied to unvalidated SQL: the
/// only way to obtain one is [`Validated::after_safety_gate`], whose name says where it may be called.
/// The ordering matters — validation runs on the ORIGINAL statement, and wrapping it first would mean
/// the gate inspected `SELECT COUNT(*) FROM (DROP ...)` instead of `DROP ...`. Making that a comment
/// would leave the next reader free to reorder; making it a type does not.
#[derive(Debug, Clone, Copy)]
pub struct Validated<'a>(&'a str);

impl<'a> Validated<'a> {
    /// Call ONLY after the read-only gate has accepted `sql` (or `force` bypassed it).
    pub fn after_safety_gate(sql: &'a str) -> Self {
        Self(sql)
    }

    pub fn as_str(&self) -> &'a str {
        self.0
    }
}

/// Strip a trailing top-level `ORDER BY`, returning the remainder and whether anything was removed.
///
/// Only a TRAILING one at paren depth 0 and outside a string literal:
///
/// * inside parentheses it belongs to a subquery, where it is legal and must stay;
/// * inside quotes it is data (`WHERE Name = 'ORDER BY x'` — verified live);
/// * not trailing means something follows it that would be orphaned.
///
/// Safe for a count because ordering cannot change how many rows there are.
pub fn strip_trailing_order_by(sql: &str) -> (String, bool) {
    let bytes = sql.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut found: Option<usize> = None;
    let upper = sql.to_ascii_uppercase();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '\'' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth = depth.saturating_sub(1).max(0),
            'O' | 'o' if !in_str && depth == 0 => {
                // Word-boundary match on ORDER ... BY, tolerating any run of whitespace between.
                if upper[i..].starts_with("ORDER")
                    && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
                {
                    let rest = &upper[i + 5..];
                    let trimmed = rest.trim_start();
                    if trimmed.starts_with("BY")
                        && rest.len() != trimmed.len()
                        && trimmed[2..]
                            .chars()
                            .next()
                            .is_none_or(|n| !n.is_alphanumeric() && n != '_')
                    {
                        // LAST one wins: an outer ORDER BY after an inner subquery's is the trailing
                        // one, and only the last can be trailing.
                        found = Some(i);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    match found {
        Some(i) => (sql[..i].trim_end().to_string(), true),
        None => (sql.trim_end().to_string(), false),
    }
}

/// `SELECT COUNT(*) AS row_count FROM (<sql>)`, with a trailing ORDER BY removed.
///
/// A trailing semicolon is dropped too: it would land inside the parentheses and fail.
pub fn count_sql(sql: Validated<'_>) -> String {
    let inner = sql.as_str().trim().trim_end_matches(';');
    let (inner, _) = strip_trailing_order_by(inner);
    format!("SELECT COUNT(*) AS row_count FROM ({inner})")
}

/// `EXPLAIN <sql>`. IRIS does not execute the statement; it returns the plan.
pub fn explain_sql(sql: Validated<'_>) -> String {
    format!("EXPLAIN {}", sql.as_str().trim().trim_end_matches(';'))
}

/// The `Cost:` figure out of an EXPLAIN plan.
///
/// The single most actionable number in the plan, and the one a caller would otherwise have to parse
/// out of XML themselves. `None` when the plan does not carry one — never 0, which would read as a
/// free query.
pub fn plan_cost(plan: &str) -> Option<i64> {
    let i = plan.find("Cost:")? + 5;
    let digits: String = plan[i..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Shape the `count` response from the row IRIS returned.
///
/// Reads the value by POSITION rather than by the `row_count` alias, because the alias is ours and a
/// build that upper-cases identifiers would break a name lookup while the position holds.
pub fn count_payload(rows: &[serde_json::Value], namespace: &str, sql: &str) -> serde_json::Value {
    let n = rows.first().and_then(|r| {
        r.as_object().and_then(|o| o.values().next()).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    });
    let mut out = serde_json::json!({
        "success": true,
        "mode": "count",
        "namespace": namespace,
        "counted_sql": sql,
    });
    match n {
        Some(n) => {
            out["row_count"] = serde_json::json!(n);
        }
        // The query ran but the count could not be read. Say that, rather than reporting 0 — a
        // wrong zero here reads as "the table is empty".
        None => {
            out["success"] = serde_json::json!(false);
            out["error_code"] = serde_json::json!("COUNT_UNREADABLE");
            out["error"] = serde_json::json!(
                "COUNT(*) returned no readable value. The rows are unchanged on IRIS; this is a \
                 reporting failure, not an empty result."
            );
        }
    }
    out
}

/// Shape the `explain` response.
pub fn explain_payload(rows: &[serde_json::Value], namespace: &str) -> serde_json::Value {
    let plan = rows
        .first()
        .and_then(|r| r.as_object())
        .and_then(|o| o.values().next())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut out = serde_json::json!({
        "success": true,
        "mode": "explain",
        "namespace": namespace,
        "plan": plan,
    });
    if let Some(c) = plan_cost(&plan) {
        out["cost"] = serde_json::json!(c);
    }
    if plan.is_empty() {
        out["success"] = serde_json::json!(false);
        out["error_code"] = serde_json::json!("PLAN_UNREADABLE");
        out["error"] = serde_json::json!("EXPLAIN returned no plan text.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Validated<'_> {
        Validated::after_safety_gate(s)
    }

    // ── mode parsing ────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_advertised_modes_parse_case_insensitively() {
        assert_eq!(QueryMode::parse("rows"), Some(QueryMode::Rows));
        assert_eq!(QueryMode::parse("COUNT"), Some(QueryMode::Count));
        assert_eq!(QueryMode::parse(" Explain "), Some(QueryMode::Explain));
    }

    /// An absent mode is `rows` — the behaviour before this existed, so no caller changes meaning.
    #[test]
    fn an_empty_mode_is_rows_not_an_error() {
        assert_eq!(QueryMode::parse(""), Some(QueryMode::Rows));
    }

    /// An unknown mode must NOT fall back to rows. Silently answering a different question than the
    /// one asked is worse than refusing, because the answer looks correct.
    #[test]
    fn an_unknown_mode_is_none_rather_than_a_silent_fallback() {
        assert_eq!(QueryMode::parse("write"), None);
        assert_eq!(QueryMode::parse("plan"), None);
        assert_eq!(QueryMode::parse("cuont"), None);
    }

    // ── the ORDER BY hazard, measured on IRIS ───────────────────────────────────────────────

    /// A trailing ORDER BY breaks the count wrap on IRIS for Health 2026.1:
    /// `SQLCODE -1, ) expected, IDENTIFIER (ORDER) found`. So it is stripped — ordering cannot
    /// change a count.
    #[test]
    fn a_trailing_order_by_is_stripped() {
        let (s, stripped) = strip_trailing_order_by("SELECT Name FROM T ORDER BY Name");
        assert!(stripped);
        assert_eq!(s, "SELECT Name FROM T");
    }

    #[test]
    fn the_count_wrap_removes_it_so_the_statement_is_valid() {
        let sql = count_sql(v("SELECT Name FROM T ORDER BY Name DESC"));
        assert_eq!(
            sql,
            "SELECT COUNT(*) AS row_count FROM (SELECT Name FROM T)"
        );
        assert!(!sql.contains("ORDER"), "{sql}");
    }

    /// `WHERE Name = 'ORDER BY x'` is a VALID query — verified live — and stripping inside the string
    /// would corrupt it into a different query that still parses. The worst kind of bug: a wrong
    /// answer, not an error.
    #[test]
    fn an_order_by_inside_a_string_literal_is_not_stripped() {
        let q = "SELECT Name FROM T WHERE Name = 'ORDER BY x'";
        let (s, stripped) = strip_trailing_order_by(q);
        assert!(!stripped, "must not touch data: {s}");
        assert_eq!(s, q);
    }

    /// Inside parentheses it belongs to a subquery, where it is legal and load-bearing.
    #[test]
    fn an_order_by_inside_a_subquery_is_kept() {
        let q = "SELECT * FROM (SELECT TOP 5 Name FROM T ORDER BY Name) x";
        let (s, stripped) = strip_trailing_order_by(q);
        assert!(!stripped, "{s}");
        assert_eq!(s, q);
    }

    /// With both, only the OUTER one goes — the subquery's must survive or the meaning changes.
    #[test]
    fn only_the_outer_order_by_is_stripped() {
        let q = "SELECT * FROM (SELECT TOP 5 Name FROM T ORDER BY Name) x ORDER BY x.Name";
        let (s, stripped) = strip_trailing_order_by(q);
        assert!(stripped);
        assert_eq!(
            s,
            "SELECT * FROM (SELECT TOP 5 Name FROM T ORDER BY Name) x"
        );
        assert!(
            s.contains("ORDER BY Name"),
            "the inner one must remain: {s}"
        );
    }

    /// Word boundary: a column called `ORDERS` or `REORDER` must not look like the keyword.
    #[test]
    fn a_column_named_like_the_keyword_is_not_mistaken_for_it() {
        for q in [
            "SELECT ORDERS FROM T",
            "SELECT REORDER FROM T",
            "SELECT ORDERBY FROM T",
        ] {
            let (s, stripped) = strip_trailing_order_by(q);
            assert!(!stripped, "{q} -> {s}");
            assert_eq!(s, q);
        }
    }

    /// `TOP` in the inner query is respected by the count — verified live (`TOP 2` → 2), so it must
    /// survive the wrap.
    #[test]
    fn a_top_clause_survives_the_wrap() {
        let sql = count_sql(v("SELECT TOP 2 Name FROM T"));
        assert!(sql.contains("TOP 2"), "{sql}");
    }

    /// A trailing semicolon would land inside the parentheses and fail.
    #[test]
    fn a_trailing_semicolon_is_dropped_from_the_wrap() {
        let sql = count_sql(v("SELECT Name FROM T;"));
        assert_eq!(
            sql,
            "SELECT COUNT(*) AS row_count FROM (SELECT Name FROM T)"
        );
    }

    // ── explain ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn explain_prefixes_the_statement() {
        assert_eq!(explain_sql(v("SELECT 1")), "EXPLAIN SELECT 1");
        assert_eq!(explain_sql(v("SELECT 1;")), "EXPLAIN SELECT 1");
    }

    /// The real plan text from IRIS for Health 2026.1, abbreviated only in the middle.
    const PLAN: &str = "<plans>\r\n <plan>\r\n   SQL:\r\n    SELECT TOP ? Name FROM T\r\n   \r\n   \
                        Cost: 1040\r\n   \r\n   Module-FIRST:\r\n     Read master map.\r\n </plan>\r\n</plans>";

    #[test]
    fn the_cost_is_extracted_from_a_real_plan() {
        assert_eq!(plan_cost(PLAN), Some(1040));
    }

    /// No cost line means None, NEVER 0 — a zero would read as a free query.
    #[test]
    fn a_plan_without_a_cost_yields_none_not_zero() {
        assert_eq!(plan_cost("<plans><plan>no cost here</plan></plans>"), None);
        assert_eq!(plan_cost("Cost: abc"), None);
    }

    #[test]
    fn the_explain_payload_carries_the_plan_and_the_cost() {
        let rows = vec![serde_json::json!({"Plan": PLAN})];
        let p = explain_payload(&rows, "APP");
        assert_eq!(p["success"], true);
        assert_eq!(p["mode"], "explain");
        assert_eq!(p["cost"], 1040);
        assert!(p["plan"].as_str().unwrap().contains("Module-FIRST"), "{p}");
    }

    /// An empty plan is a FAILURE, not a successful empty answer.
    #[test]
    fn an_empty_plan_is_reported_as_a_failure() {
        let p = explain_payload(&[], "APP");
        assert_eq!(p["success"], false, "{p}");
        assert_eq!(p["error_code"], "PLAN_UNREADABLE");
    }

    // ── count payload ──────────────────────────────────────────────────────────────────────

    #[test]
    fn the_count_payload_reads_the_number() {
        let rows = vec![serde_json::json!({"row_count": 42})];
        let p = count_payload(&rows, "APP", "SELECT COUNT(*) AS row_count FROM (SELECT 1)");
        assert_eq!(p["success"], true);
        assert_eq!(p["row_count"], 42);
        assert_eq!(p["mode"], "count");
        assert!(
            p["counted_sql"].as_str().unwrap().contains("COUNT(*)"),
            "{p}"
        );
    }

    /// Read by POSITION, not by the alias: the alias is ours, and a build that upper-cases identifiers
    /// would break a name lookup while the position holds. Also covers the numeric-string driver path.
    #[test]
    fn the_count_is_read_by_position_and_tolerates_a_numeric_string() {
        let upper = vec![serde_json::json!({"ROW_COUNT": 7})];
        assert_eq!(count_payload(&upper, "APP", "x")["row_count"], 7);
        let as_str = vec![serde_json::json!({"row_count": "9"})];
        assert_eq!(count_payload(&as_str, "APP", "x")["row_count"], 9);
    }

    /// An unreadable count must NOT be reported as 0 — a wrong zero reads as "the table is empty",
    /// which is a confident wrong answer rather than a visible failure.
    #[test]
    fn an_unreadable_count_is_a_failure_not_a_zero() {
        let p = count_payload(&[], "APP", "x");
        assert_eq!(p["success"], false, "{p}");
        assert_eq!(p["error_code"], "COUNT_UNREADABLE");
        assert!(p["row_count"].is_null(), "must not invent a number: {p}");
        assert!(
            p["error"].as_str().unwrap().contains("not an empty result"),
            "the message must say which it is: {p}"
        );
    }

    /// The transform is NOT a safety boundary — it wraps whatever it is given. The gate runs upstream,
    /// on the ORIGINAL statement, and the `Validated` newtype is what makes that order structural:
    /// there is no way to call these with a bare &str. This test documents the division rather than
    /// asserting the transform rejects anything, which it must not try to do.
    #[test]
    fn the_transform_does_not_second_guess_the_gate() {
        let sql = count_sql(v("DROP TABLE T"));
        assert_eq!(
            sql, "SELECT COUNT(*) AS row_count FROM (DROP TABLE T)",
            "the wrap is mechanical; refusing this is the gate's job, upstream, on the original text"
        );
    }
}
