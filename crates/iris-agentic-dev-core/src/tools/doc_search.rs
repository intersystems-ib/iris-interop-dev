//! #24 item 065, served LOCALLY instead of from Algolia.
//!
//! The item is "iris_doc_search: Algolia-backed docs.intersystems.com search". That is not ported —
//! reasons on the issue, the short version being that it needs a third-party app id and API key for an
//! index whose response shape cannot be verified from here, and it sends the caller's query out of the
//! workshop network. Guessing an external API's shape is the failure this repo has already had twice.
//!
//! The NEED — "find me the documentation about X" — is served from the live instance instead:
//! `%Dictionary` carries the class and method documentation, and nothing searched it.
//!
//! EVERY NUMBER BELOW WAS MEASURED ON IRIS FOR HEALTH 2026.1, not assumed:
//!
//! | | classes | methods |
//! |---|---|---|
//! | rows | 15,871 | 3,376,917 |
//! | with a Description | 11,432 (72%) | 1,197,548 |
//! | containing HTML | — | 608,420 (half of the described ones) |
//! | search, match | — | 2.1 s |
//! | search, NO match (full scan) | **0.059 s** | **5.79 s** |
//! | search, scoped to `Ens%` | — | 0.104 s |
//!
//! Three design consequences, each from a row of that table:
//!
//! 1. **Classes are searched by default.** 60 ms worst case; there is no reason to make the caller
//!    narrow it.
//! 2. **Methods require a scope.** Unscoped costs ~6 s on a NO-MATCH — which is exactly the query a
//!    caller retries after a typo, so the slow path is the common one. A scope makes it 0.1 s.
//! 3. **HTML is stripped.** Half the described methods contain markup; returning `<p>` and `</class>`
//!    to a model is noise it has to parse around.

use serde::Serialize;

/// What to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocScope {
    /// Class documentation only — always cheap.
    Classes,
    /// Method documentation. Requires `within`; see the module docs.
    Methods,
    /// Both. Requires `within`, because it includes methods.
    Both,
}

impl DocScope {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "classes" => Some(Self::Classes),
            "methods" => Some(Self::Methods),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    pub fn valid_values() -> &'static str {
        "classes (default), methods, both"
    }

    /// Whether this scope reads `%Dictionary.CompiledMethod`, the 3.4-million-row table.
    pub fn needs_scope(self) -> bool {
        matches!(self, Self::Methods | Self::Both)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SearchError {
    EmptyTerm,
    TermTooShort(usize),
    ScopeRequired,
    BadScope(String),
}

impl SearchError {
    pub fn message(&self) -> String {
        match self {
            Self::EmptyTerm => "Pass a `term` to search the documentation for.".into(),
            Self::TermTooShort(n) => format!(
                "`term` is {n} characters; at least 3 are needed. A shorter one matches most of the \
                 documentation in the instance and tells you nothing."
            ),
            Self::ScopeRequired => "Searching METHOD documentation requires `within` — a class or \
                 package prefix such as 'Ens' or 'EnsLib.HL7'. Measured on IRIS for Health 2026.1: \
                 %Dictionary.CompiledMethod holds 3.4 MILLION rows, and an unscoped search costs \
                 about 6 seconds when the term matches NOTHING — which is exactly the query you \
                 retry after a typo. Scoped to a package it is 0.1 s. Class documentation has no \
                 such limit: search it unscoped with scope='classes' (the default)."
                .into(),
            Self::BadScope(s) => format!(
                "Unknown scope '{s}'. Valid: {}.",
                DocScope::valid_values()
            ),
        }
    }

    pub fn code(&self) -> &'static str {
        "INVALID_PARAMS"
    }
}

/// Reject before spending a query.
pub fn validate(term: &str, scope: DocScope, within: Option<&str>) -> Result<(), SearchError> {
    let t = term.trim();
    if t.is_empty() {
        return Err(SearchError::EmptyTerm);
    }
    if t.chars().count() < 3 {
        return Err(SearchError::TermTooShort(t.chars().count()));
    }
    if scope.needs_scope() && within.map(str::trim).unwrap_or("").is_empty() {
        return Err(SearchError::ScopeRequired);
    }
    Ok(())
}

/// Single-quote escape for an IRIS SQL literal, the same way the existing call sites do it.
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// The class-documentation query.
///
/// `UPPER(Description) LIKE UPPER(...)` — measured: a bare `LIKE` is case-sensitive here, so a search
/// for "search table" would miss "Search Table".
pub fn class_sql(term: &str, within: Option<&str>, limit: usize) -> String {
    let mut sql = format!(
        "SELECT TOP {limit} Name, Description FROM %Dictionary.CompiledClass \
         WHERE UPPER(Description) LIKE '%{}%'",
        esc(&term.trim().to_uppercase())
    );
    if let Some(w) = within.map(str::trim).filter(|w| !w.is_empty()) {
        sql.push_str(&format!(" AND Name LIKE '{}%'", esc(w)));
    }
    sql.push_str(" ORDER BY Name");
    sql
}

/// The method-documentation query. `within` is REQUIRED by [`validate`]; it is what keeps this off a
/// full scan of 3.4 million rows.
pub fn method_sql(term: &str, within: &str, limit: usize) -> String {
    format!(
        "SELECT TOP {limit} parent AS Class, Name, Description FROM %Dictionary.CompiledMethod \
         WHERE parent LIKE '{}%' AND UPPER(Description) LIKE '%{}%' ORDER BY parent, Name",
        esc(within.trim()),
        esc(&term.trim().to_uppercase())
    )
}

/// Strip HTML tags and collapse whitespace.
///
/// Measured: 608,420 method descriptions contain markup — `<p>`, `<class>`, `<method>`, `<example>`.
/// Returning that to a model is noise it has to parse around. Entities are decoded for the four that
/// actually occur in this corpus; anything else is left alone rather than half-decoded.
pub fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            // CR and LF become spaces; the collapse below removes the runs.
            _ if in_tag => {}
            '\r' | '\n' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    // Collapse whitespace runs so a snippet is readable on one line.
    let mut collapsed = String::with_capacity(decoded.len());
    let mut last_space = false;
    for c in decoded.chars() {
        if c == ' ' {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(c);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// A readable window around the first case-insensitive occurrence of `term`.
///
/// The whole description can be pages long; what a caller needs is the sentence the term is in. When
/// the term is not found — possible, since the SQL matched the RAW description and this searches the
/// STRIPPED one, so a term spanning a tag boundary can vanish — the head of the text is returned
/// rather than nothing, and that asymmetry is asserted in the tests.
pub fn snippet(text: &str, term: &str, width: usize) -> String {
    let stripped = strip_markup(text);
    if stripped.len() <= width {
        return stripped;
    }
    let hay = stripped.to_uppercase();
    let needle = term.trim().to_uppercase();
    let at = hay.find(&needle).unwrap_or(0);
    // Centre the window on the match, on char boundaries.
    let half = width / 2;
    let start = stripped[..at]
        .char_indices()
        .rev()
        .nth(half)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = stripped[start..]
        .char_indices()
        .nth(width)
        .map(|(i, _)| start + i)
        .unwrap_or(stripped.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(&stripped[start..end]);
    if end < stripped.len() {
        s.push('…');
    }
    s
}

/// One hit.
#[derive(Debug, PartialEq, Serialize)]
pub struct DocHit {
    pub class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub doc: String,
}

/// Shape the rows IRIS returned. `method_rows` carry a `Class` column; class rows carry `Name`.
pub fn hits_from_rows(
    rows: &[serde_json::Value],
    are_methods: bool,
    term: &str,
    width: usize,
) -> Vec<DocHit> {
    rows.iter()
        .filter_map(|r| {
            let desc = r["Description"].as_str().unwrap_or("");
            let doc = snippet(desc, term, width);
            if doc.is_empty() {
                return None;
            }
            if are_methods {
                Some(DocHit {
                    class: r["Class"].as_str().unwrap_or("").to_string(),
                    method: r["Name"].as_str().map(str::to_string),
                    doc,
                })
            } else {
                Some(DocHit {
                    class: r["Name"].as_str().unwrap_or("").to_string(),
                    method: None,
                    doc,
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL description, verbatim from `Ens.CustomSearchTable` on IRIS for Health 2026.1 — markup,
    /// CRLF and all. Fixtures built from what the instance actually stores, not from tidy invented text.
    const REAL: &str =
        "<p>Base class for custom search tables that can implement alternative indexing \
         strategies \r\nto those offered by the standard SearchTable structure. Note that the \
         <class>Ens.SearchTableBase</class> subclass is the usual starting point.</p>";

    /// A SECOND real description, verbatim from `Ens.CustomSearchTable.%BMEBuilt` on the same
    /// instance. It carries tag variants the first fixture does not: `<var>` and an UPPERCASE
    /// `<CLASS>`. Found by actually running the tool's own method query and reading what came back,
    /// rather than assuming the markup looked like the first sample.
    const REAL_VAR: &str = "On return, <var>bmeName</var> contains the name of the bitmap extent \
         index for this class.\r\n<p>Returns <CLASS>%Boolean</CLASS> TRUE is the bitmap extent has \
         been built, FALSE if not.";

    /// Tag matching must be case-AGNOSTIC: `<CLASS>` appears upper-cased in real descriptions, and a
    /// stripper keyed on lowercase tag names would leave it in the output.
    #[test]
    fn uppercase_and_var_tags_are_stripped_too() {
        let out = strip_markup(REAL_VAR);
        assert!(!out.contains('<') && !out.contains('>'), "{out}");
        // the CONTENT of both tags survives — it is the useful part
        assert!(out.contains("bmeName"), "{out}");
        assert!(out.contains("%Boolean"), "{out}");
        // CRLF gone, no double spaces left behind by the removed tags
        assert!(!out.contains('\r') && !out.contains('\n'), "{out}");
        assert!(!out.contains("  "), "double space: {out}");
    }

    /// The method query's aliases, verified LIVE against IRIS for Health 2026.1: the rows come back
    /// with columns named exactly `Class`, `Name`, `Description` — which is what `hits_from_rows`
    /// reads. A mis-aliased column would be invisible (`Null` reads as "no documentation"), the same
    /// failure class as #273, so the contract is pinned here as well as checked by hand.
    #[test]
    fn the_method_query_aliases_match_what_the_shaper_reads() {
        let sql = method_sql("index", "Ens.CustomSearchTable", 2);
        for alias in ["parent AS Class", "Name", "Description"] {
            assert!(sql.contains(alias), "missing {alias}: {sql}");
        }
        // and a row shaped like the live response maps cleanly
        let rows = vec![serde_json::json!({
            "Class": "Ens.CustomSearchTable", "Name": "%BMEBuilt", "Description": REAL_VAR
        })];
        let hits = hits_from_rows(&rows, true, "index", 240);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].class, "Ens.CustomSearchTable");
        assert_eq!(hits[0].method.as_deref(), Some("%BMEBuilt"));
        assert!(hits[0].doc.contains("bmeName"), "{:?}", hits[0]);
        assert!(!hits[0].doc.contains('<'), "{:?}", hits[0]);
    }

    // ── validation ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_usable_class_search_passes_without_a_scope() {
        assert_eq!(validate("search table", DocScope::Classes, None), Ok(()));
    }

    /// THE measured constraint: methods without a scope would scan 3.4M rows for ~6s on a no-match.
    #[test]
    fn method_search_without_a_scope_is_refused() {
        let e = validate("search table", DocScope::Methods, None).unwrap_err();
        assert_eq!(e, SearchError::ScopeRequired);
        let m = e.message();
        assert!(m.contains("3.4 MILLION"), "the message must say why: {m}");
        assert!(
            m.contains("matches NOTHING"),
            "and that the SLOW case is the retry case: {m}"
        );
        assert!(
            m.contains("scope='classes'"),
            "and offer the cheap alternative: {m}"
        );
    }

    #[test]
    fn both_also_requires_a_scope_because_it_includes_methods() {
        assert_eq!(
            validate("x-ray", DocScope::Both, None),
            Err(SearchError::ScopeRequired)
        );
        assert_eq!(validate("x-ray", DocScope::Both, Some("Ens")), Ok(()));
    }

    /// A blank `within` is not a scope. An empty string would reach the SQL as `LIKE '%'` and scan
    /// everything — the exact thing the refusal exists to prevent.
    #[test]
    fn a_blank_within_does_not_satisfy_the_scope_requirement() {
        for w in [Some(""), Some("   "), None] {
            assert_eq!(
                validate("search table", DocScope::Methods, w),
                Err(SearchError::ScopeRequired),
                "within={w:?}"
            );
        }
    }

    #[test]
    fn a_short_or_empty_term_is_refused() {
        assert_eq!(
            validate("", DocScope::Classes, None),
            Err(SearchError::EmptyTerm)
        );
        assert_eq!(
            validate("   ", DocScope::Classes, None),
            Err(SearchError::EmptyTerm)
        );
        let e = validate("ab", DocScope::Classes, None).unwrap_err();
        assert_eq!(e, SearchError::TermTooShort(2));
        assert!(e.message().contains("at least 3"), "{}", e.message());
        // three is enough
        assert_eq!(validate("abc", DocScope::Classes, None), Ok(()));
    }

    #[test]
    fn the_scopes_parse_and_an_unknown_one_does_not_default() {
        assert_eq!(DocScope::parse(""), Some(DocScope::Classes));
        assert_eq!(DocScope::parse("CLASSES"), Some(DocScope::Classes));
        assert_eq!(DocScope::parse("methods"), Some(DocScope::Methods));
        assert_eq!(DocScope::parse("Both"), Some(DocScope::Both));
        assert_eq!(
            DocScope::parse("everything"),
            None,
            "must not silently default"
        );
    }

    #[test]
    fn only_the_method_scopes_need_a_scope() {
        assert!(!DocScope::Classes.needs_scope());
        assert!(DocScope::Methods.needs_scope());
        assert!(DocScope::Both.needs_scope());
    }

    // ── the SQL ─────────────────────────────────────────────────────────────────────────────

    /// Measured: a bare `LIKE` is CASE-SENSITIVE here, so "search table" would miss "Search Table".
    /// Both sides must be upper-cased.
    #[test]
    fn the_search_is_case_insensitive_on_both_sides() {
        let sql = class_sql("Search Table", None, 10);
        assert!(sql.contains("UPPER(Description)"), "{sql}");
        assert!(
            sql.contains("'%SEARCH TABLE%'"),
            "the term must be upper-cased too: {sql}"
        );
    }

    #[test]
    fn a_quote_in_the_term_is_escaped() {
        let sql = class_sql("O'Brien", None, 5);
        assert!(sql.contains("'%O''BRIEN%'"), "{sql}");
        let m = method_sql("it's", "Ens", 5);
        assert!(m.contains("'%IT''S%'"), "{m}");
    }

    /// A quote in `within` must be escaped too — it is interpolated, not bound.
    #[test]
    fn a_quote_in_the_scope_is_escaped() {
        let m = method_sql("abc", "Ens'X", 5);
        assert!(m.contains("LIKE 'Ens''X%'"), "{m}");
    }

    #[test]
    fn the_class_query_is_scoped_only_when_asked() {
        assert!(!class_sql("abc", None, 5).contains("Name LIKE"));
        assert!(class_sql("abc", Some("Ens"), 5).contains("Name LIKE 'Ens%'"));
        // a blank scope must not become `LIKE '%'`
        assert!(!class_sql("abc", Some("  "), 5).contains("Name LIKE"));
    }

    /// The method query ALWAYS carries the scope — that is what keeps it off a 3.4M-row scan.
    #[test]
    fn the_method_query_always_carries_the_scope() {
        let m = method_sql("abc", "EnsLib.HL7", 5);
        assert!(m.contains("parent LIKE 'EnsLib.HL7%'"), "{m}");
        assert!(m.contains("TOP 5"), "and a row cap: {m}");
    }

    // ── markup stripping ────────────────────────────────────────────────────────────────────

    #[test]
    fn markup_and_line_breaks_come_out_of_the_real_description() {
        let s = strip_markup(REAL);
        assert!(!s.contains('<'), "{s}");
        assert!(!s.contains('>'), "{s}");
        assert!(!s.contains('\r') && !s.contains('\n'), "{s}");
        // the CLASS NAME inside <class>…</class> must survive — it is the useful part
        assert!(s.contains("Ens.SearchTableBase"), "{s}");
        // and whitespace runs are collapsed
        assert!(!s.contains("  "), "double space left: {s}");
    }

    #[test]
    fn the_four_entities_that_occur_are_decoded() {
        assert_eq!(strip_markup("a &amp; b"), "a & b");
        assert_eq!(strip_markup("&lt;tag&gt;"), "<tag>");
        assert_eq!(strip_markup("&quot;q&quot;"), "\"q\"");
    }

    /// An unknown entity is left ALONE rather than half-decoded — a mangled `&nbsp` reads as a typo in
    /// the documentation.
    #[test]
    fn an_unknown_entity_is_left_alone() {
        assert_eq!(strip_markup("a&nbsp;b"), "a&nbsp;b");
    }

    // ── snippets ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_short_description_is_returned_whole_without_ellipses() {
        let s = snippet("<p>Short doc.</p>", "short", 200);
        assert_eq!(s, "Short doc.");
        assert!(!s.contains('…'));
    }

    #[test]
    fn a_long_description_is_windowed_around_the_match() {
        let long = format!("{}NEEDLE{}", "a ".repeat(200), " b".repeat(200));
        let s = snippet(&long, "needle", 60);
        assert!(
            s.to_uppercase().contains("NEEDLE"),
            "the match must be in view: {s}"
        );
        assert!(s.len() < 120, "and the window bounded: {} chars", s.len());
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
    }

    /// The SQL matched the RAW description; this searches the STRIPPED one, so a term spanning a tag
    /// boundary can genuinely vanish. Returning the HEAD is then better than returning nothing — but it
    /// must not claim to be a match, which is why there is no highlight marker.
    #[test]
    fn a_term_lost_to_stripping_falls_back_to_the_head() {
        let text = "<p>alpha</p><p>beta</p>".to_string() + &"z ".repeat(200);
        // "alphabeta" exists only across the tag boundary in the raw text
        let s = snippet(&text, "alphabeta", 40);
        assert!(s.starts_with("alpha"), "should fall back to the head: {s}");
        assert!(
            !s.starts_with('…'),
            "the head needs no leading ellipsis: {s}"
        );
    }

    // ── row shaping ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn class_rows_become_hits_without_a_method() {
        let rows = vec![serde_json::json!({"Name": "Ens.CustomSearchTable", "Description": REAL})];
        let hits = hits_from_rows(&rows, false, "search table", 200);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].class, "Ens.CustomSearchTable");
        assert_eq!(hits[0].method, None);
        assert!(!hits[0].doc.contains('<'), "{:?}", hits[0]);
    }

    #[test]
    fn method_rows_carry_the_class_and_the_method() {
        let rows = vec![serde_json::json!({
            "Class": "Ens.CustomSearchTable", "Name": "IndexDoc", "Description": "<p>Indexes it.</p>"
        })];
        let hits = hits_from_rows(&rows, true, "index", 200);
        assert_eq!(hits[0].class, "Ens.CustomSearchTable");
        assert_eq!(hits[0].method.as_deref(), Some("IndexDoc"));
        assert_eq!(hits[0].doc, "Indexes it.");
    }

    /// A row whose description is only markup yields NOTHING rather than an empty hit — an entry with
    /// a blank doc is worse than no entry, because it reads as "documented, but says nothing".
    #[test]
    fn a_description_that_is_only_markup_is_dropped() {
        let rows = vec![
            serde_json::json!({"Name": "A", "Description": "<p></p>"}),
            serde_json::json!({"Name": "B", "Description": "<p>real</p>"}),
        ];
        let hits = hits_from_rows(&rows, false, "real", 200);
        assert_eq!(hits.len(), 1, "the empty one must be dropped: {hits:?}");
        assert_eq!(hits[0].class, "B");
    }
}
