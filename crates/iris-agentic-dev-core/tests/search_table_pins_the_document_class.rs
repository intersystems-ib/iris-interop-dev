//! #409: `iris_interop_query(what=search_table)` matched message bodies of other classes.
//!
//! A Search Table's `DocId` **is** `Ens.MessageHeader.MessageBodyId`, but `MessageBodyId` is unique
//! only within one document class's extent. So joining on it alone also matches a custom
//! `%Persistent` body whose numeric ID happens to collide with an indexed document's ID — which is
//! what the reporter saw: `Pkg.MSG.*` headers returned next to the HL7 ones.
//!
//! ## Why this was a sibling asymmetry, not an oversight about SQL
//!
//! Both joins in `interop.rs` were added by #4. The body-class one already pinned the class, and
//! carried a doc comment giving this exact reason ("MessageBodyId is only unique per body table, so
//! without it same-numbered rows of OTHER body classes would match"). The search-table one did not.
//! The hazard was understood when the code was written and one of the two siblings got the guard, so
//! `neither_join_leaves_the_body_class_unconstrained` below asserts the property of **both** — a
//! test that only covered the builder I fixed would let the pair drift apart again.
//!
//! ## Why the pin is a list and not an equality
//!
//! The report suggested `MessageBodyClassName = 'EnsLib.HL7.Message'`. Measured on IRIS for Health
//! 2026.1, that is right for HL7 and wrong in general:
//!
//! | extent | DOCCLASS | classes in the family |
//! |---|---|---:|
//! | `EnsLib.HL7.SearchTable` | `EnsLib.HL7.Message` | 1 |
//! | `EnsLib.EDI.X12.SearchTable` | `EnsLib.EDI.X12.Document` | 1 |
//! | `EnsLib.XML.SearchTable` | `Ens.StreamContainer` | **5** |
//!
//! A body class extending the document class is still a document. Under a bare equality the four
//! `Ens.StreamContainer` subclasses would vanish from results — trading over-matching for
//! under-matching, which is the worse of the two because the caller cannot see it.
//!
//! ## Why an unresolved document class must NOT become a predicate
//!
//! `Ens.MessageHeader` has no `DOCCLASS`, and that query returns 0 rows rather than an error. An
//! extent in that state cannot be pinned. The fix therefore emits the join **unpinned** and says so
//! in a `warning`. Emitting `IN ()` or a `1=0` would answer the caller with zero rows, and zero rows
//! from a search read as "no message matched" — the negative-fact shape CLAUDE.md is about, and the
//! reason `an_unresolved_document_class_is_not_an_impossible_predicate` asserts the absence of both.

use iris_agentic_dev_core::tools::interop::{
    build_body_join_sql, build_search_table_sql, doc_class_family_sql, unpinned_search_warning,
};

const HL7: &str = "EnsLib.HL7.Message";

fn hl7_family() -> Vec<String> {
    vec![HL7.to_string()]
}

/// The five classes `EnsLib.XML.SearchTable` resolves to, measured on IRIS for Health 2026.1.
fn stream_family() -> Vec<String> {
    [
        "Ens.MFT.StreamContainer",
        "Ens.StreamContainer",
        "EnsLib.HTTP.GenericMessage",
        "EnsLib.REST.GenericMessage",
        "EnsLib.SOAP.GenericMessage",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn hl7_sql(family: &[String]) -> String {
    build_search_table_sql(
        10,
        vec![],
        "EnsLib_HL7.SearchTable",
        &[4],
        Some("123456"),
        None,
        family,
    )
}

#[test]
fn the_join_pins_the_message_body_class() {
    let sql = hl7_sql(&hl7_family());
    assert!(
        sql.contains("h.MessageBodyClassName IN ('EnsLib.HL7.Message')"),
        "the document class is not pinned: {sql}"
    );
}

#[test]
fn every_class_of_the_family_is_accepted_in_one_predicate() {
    let sql = hl7_sql(&stream_family());
    // One IN list, not five ORs and not five separate conjuncts: five predicates
    // ANDed together can never all hold, which would return zero rows.
    assert_eq!(
        sql.matches("h.MessageBodyClassName IN (").count(),
        1,
        "the family must be ONE predicate: {sql}"
    );
    assert!(
        sql.contains(
            "h.MessageBodyClassName IN ('Ens.MFT.StreamContainer','Ens.StreamContainer','EnsLib.HTTP.GenericMessage','EnsLib.REST.GenericMessage','EnsLib.SOAP.GenericMessage')"
        ),
        "a subclass of the document class is still a document: {sql}"
    );
}

#[test]
fn an_unresolved_document_class_is_not_an_impossible_predicate() {
    let sql = hl7_sql(&[]);
    // NB `MessageBodyClassName` is one of HEADER_COLS, so it appears in the SELECT
    // list either way — the claim is about the WHERE clause, and the assertion has
    // to say so or it is testing the projection.
    assert!(
        !sql.contains("h.MessageBodyClassName IN ("),
        "an unresolvable document class must leave the join unpinned, not pin it to nothing: {sql}"
    );
    let w = sql.find(" WHERE ").expect("a WHERE clause");
    assert!(
        !sql[w..].contains("MessageBodyClassName"),
        "nothing about the body class may reach the WHERE clause when it is unknown: {sql}"
    );
    // The two shapes that would turn "cannot pin" into "no message matched".
    for impossible in ["IN ()", "1=0", "1 = 0", "IN (NULL)", "IN ('')"] {
        assert!(
            !sql.contains(impossible),
            "unresolved must not become the impossible predicate {impossible}: {sql}"
        );
    }
    // …and the rest of the query still works, i.e. it degrades to the pre-#409 join.
    assert!(sql.contains("JOIN EnsLib_HL7.SearchTable st ON st.DocId = h.MessageBodyId"));
    assert!(sql.contains("st.PropId IN (4)"));
    assert!(sql.contains("st.PropValue = '123456'"));
}

#[test]
fn the_pin_is_a_conjunct_of_the_where_clause() {
    let sql = hl7_sql(&hl7_family());
    let w = sql.find(" WHERE ").expect("a WHERE clause");
    let o = sql.find(" ORDER BY ").expect("an ORDER BY clause");
    let pin = sql
        .find("h.MessageBodyClassName IN (")
        .expect("the pin is present");
    // Position, not presence: a filter emitted after ORDER BY is not a filter, and
    // one emitted before WHERE is not valid SQL at all.
    assert!(
        w < pin && pin < o,
        "the pin must sit inside the WHERE clause (WHERE@{w}, pin@{pin}, ORDER BY@{o}): {sql}"
    );
    assert!(
        sql[w..o].contains(" AND h.MessageBodyClassName IN ("),
        "the pin must be ANDed with the other filters, not replace them: {sql}"
    );
}

#[test]
fn the_pin_does_not_displace_the_callers_own_filters() {
    // A caller-supplied filter and the pin must both survive.
    let sql = build_search_table_sql(
        10,
        vec!["h.SourceConfigName = 'Census.BS.HL7'".to_string()],
        "EnsLib_HL7.SearchTable",
        &[4],
        None,
        Some("AMOX%"),
        &hl7_family(),
    );
    assert!(sql.contains("h.SourceConfigName = 'Census.BS.HL7'"));
    assert!(sql.contains("h.MessageBodyClassName IN ('EnsLib.HL7.Message')"));
    assert!(sql.contains("st.PropValue LIKE 'AMOX%'"));
    assert!(sql.contains("st.PropId IN (4)"));
}

#[test]
fn neither_join_leaves_the_body_class_unconstrained() {
    // The property of the PAIR — this is the asymmetry #409 actually was.
    let search = hl7_sql(&hl7_family());
    let body = build_body_join_sql(
        10,
        vec![],
        HL7,
        "EnsLib_HL7.Message",
        None,
        &["Name".to_string()],
    );
    for (which, sql) in [("search-table join", &search), ("body-class join", &body)] {
        // Scoped to the WHERE clause. `MessageBodyClassName` is one of HEADER_COLS and both
        // builders prefix those with `h.`, so `contains("h.MessageBodyClassName")` is satisfied
        // by the PROJECTION and holds even with no filter at all. Caught by mutation: removing
        // the body-class join's pin left this assertion green. It is the same trap as in
        // `an_unresolved_document_class_is_not_an_impossible_predicate`, and fixing it there and
        // not here is the very "one sibling got the guard" failure this test exists to prevent.
        let w = sql.find(" WHERE ").expect("a WHERE clause");
        assert!(
            sql[w..].contains("MessageBodyClassName"),
            "{which} does not constrain the body class in its WHERE clause: {sql}"
        );
        assert!(
            sql.contains("h.MessageBodyId"),
            "{which} should still join on MessageBodyId: {sql}"
        );
    }
}

#[test]
fn a_quote_in_a_class_name_cannot_close_the_literal() {
    let sql = hl7_sql(&["Pkg.MSG.O'Brien".to_string()]);
    assert!(
        sql.contains("IN ('Pkg.MSG.O''Brien')"),
        "a quote in a class name must be doubled: {sql}"
    );
}

#[test]
fn the_document_class_is_read_from_the_docclass_parameter() {
    let sql = doc_class_family_sql("EnsLib.EDI.X12.SearchTable");
    assert!(sql.contains("p.parent = 'EnsLib.EDI.X12.SearchTable'"));
    assert!(sql.contains("p.Name = 'DOCCLASS'"));
    // `Default` is reserved; the column really is `_Default`. Getting this wrong
    // fails as SQLCODE -29 at runtime, which no unit test would otherwise catch.
    assert!(
        sql.contains("p._Default"),
        "the parameter default column is _Default: {sql}"
    );
    assert!(
        !sql.contains("HL7"),
        "no family may be special-cased in the resolver: {sql}"
    );
}

#[test]
fn the_family_query_matches_the_class_itself_and_its_subclasses() {
    let sql = doc_class_family_sql("EnsLib.XML.SearchTable");
    // PrimarySuper is `~`-delimited and INCLUDES the class itself, so both `~` are
    // load-bearing: drop the leading one and `X.Message` matches `Other.X.Message`;
    // drop the trailing one and it matches `EnsLib.HL7.MessageArchive`.
    assert!(
        sql.contains("c.PrimarySuper LIKE '%~' || p._Default || '~%'"),
        "the family LIKE must be delimited by ~ on BOTH sides: {sql}"
    );
    assert!(
        sql.contains("%Dictionary.CompiledClass"),
        "subclasses come from the compiled dictionary: {sql}"
    );
}

#[test]
fn the_extent_is_escaped_in_the_family_query() {
    let sql = doc_class_family_sql("Pkg.ST.O'Brien");
    assert!(sql.contains("p.parent = 'Pkg.ST.O''Brien'"), "{sql}");
}

#[test]
fn the_unpinned_warning_names_the_remedy() {
    let w = unpinned_search_warning("Ens.MessageHeader", "HOSPITAL");
    assert!(w.contains("Ens.MessageHeader"), "names the extent: {w}");
    assert!(w.contains("HOSPITAL"), "names the namespace: {w}");
    assert!(
        w.contains("message_class"),
        "a warning about a missing guarantee must hand over the way to get it: {w}"
    );
    assert!(
        w.contains("DocId is unique only within"),
        "it must say WHY other classes can appear, or the caller cannot judge the rows: {w}"
    );
}

#[test]
fn the_unpinned_warning_does_not_claim_the_search_failed() {
    let w = unpinned_search_warning("Ens.MessageHeader", "HOSPITAL");
    // The rows ARE returned; the warning is about the constraint, not the result.
    for wrong in ["no message", "not found", "failed", "0 rows", "no rows"] {
        assert!(
            !w.to_lowercase().contains(wrong),
            "the warning must not read as a failed search ({wrong}): {w}"
        );
    }
    assert!(
        w.contains("may be included"),
        "it should say the rows may be over-inclusive, which is what actually happened: {w}"
    );
}
