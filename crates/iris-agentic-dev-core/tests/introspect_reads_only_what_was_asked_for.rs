//! #430: `docs_introspect` had no way to ask for PART of a class.
//!
//! Measured in a benchmarked interop build: 21 calls, ~148k chars, p90 17.8k, max 25.2k — and the
//! outlier (`HL7ToSDA3`) serialises dozens of helper methods nobody asked about. The saving is taken
//! losslessly, by narrowing what is READ rather than by trimming what is returned: `members`,
//! `kind` and `limit` all constrain the SQL.
//!
//! Two things make this more than adding three parameters.
//!
//! ## A filter miss is not a missing class
//!
//! The handler's empty-result path is what decides `CLASS_NOT_FOUND` (#107) and
//! `CLASS_NOT_COMPILED`. Narrow the query and "no member matched" arrives in exactly the shape of
//! "no such class" — the dominant bug of this repo, newly manufactured. So an empty answer under a
//! filter asks a second question: how many members exist with the filter removed, in the SAME
//! population (`include_inherited` carried over). Zero means the absence probe was right after all;
//! non-zero means the class is fine and the pattern missed, reported as a count.
//!
//! ## `%` is a real character in IRIS member names
//!
//! `%OnNew`, `%Save`, `%GetParameter`. A pattern compiled into `LIKE` without escaping silently
//! answers a different question: measured against `Ens.Config.Production` on a live instance,
//! `members: "%Get"` returns **3** methods escaped and **62** unescaped. Both look like a working
//! filter. Hence `ESCAPE '\'` and [`member_like`], and hence these tests.

use iris_agentic_dev_core::tools::{member_like, MemberKindFilter};

#[test]
fn a_glob_becomes_an_anchored_like_and_a_bare_word_becomes_a_substring() {
    // `*` is the wildcard; without one, the caller means "contains".
    assert_eq!(member_like("Get*"), "Get%", "prefix");
    assert_eq!(member_like("*Stored"), "%Stored", "suffix");
    assert_eq!(member_like("*Settings*"), "%Settings%", "both ends");
    assert_eq!(member_like("Settings"), "%Settings%", "no glob = substring");
    assert_eq!(member_like("*"), "%", "everything");
}

#[test]
fn the_sql_wildcards_in_a_member_name_are_escaped_not_honoured() {
    // The measured case. `%Get` names three real methods on Ens.Config.Production; unescaped it
    // is the wildcard pattern that matches all 62 containing "Get".
    assert_eq!(
        member_like("%Get"),
        r"%\%Get%",
        "a literal % must be escaped, or the filter answers a different question"
    );
    assert_eq!(
        member_like("%OnNew*"),
        r"\%OnNew%",
        "escaped, and * still globs"
    );
    assert_eq!(
        member_like("a_b"),
        r"%a\_b%",
        "_ is a single-character wildcard in SQL and a plain character in a member name"
    );
    assert_eq!(
        member_like(r"back\slash"),
        r"%back\\slash%",
        "the escape char itself"
    );
}

#[test]
fn an_escaped_pattern_cannot_be_confused_with_the_wildcard_form() {
    // The two must not collapse: if they did, the escaping would be decorative.
    assert_ne!(
        member_like("%Get"),
        member_like("*Get*"),
        "a literal %Get and a wildcard *Get* are different searches"
    );
}

#[test]
fn an_omitted_kind_means_both_and_an_unknown_kind_is_refused() {
    for omitted in [None, Some(""), Some("both"), Some("BOTH"), Some(" both ")] {
        assert_eq!(
            MemberKindFilter::parse(omitted),
            Ok(MemberKindFilter::Both),
            "{omitted:?} must mean both"
        );
    }
    for m in ["methods", "Methods", "METHODS"] {
        assert_eq!(
            MemberKindFilter::parse(Some(m)),
            Ok(MemberKindFilter::Methods)
        );
    }
    for p in ["properties", "Properties"] {
        assert_eq!(
            MemberKindFilter::parse(Some(p)),
            Ok(MemberKindFilter::Properties)
        );
    }
    // The singular is REFUSED rather than guessed. A caller who sent `kind: "method"` asked for a
    // narrowing; answering with everything looks like a class with more members than they asked
    // about — the same shape as a filter the server dropped (#202).
    for bad in ["method", "property", "Method", "fields", "all"] {
        assert_eq!(
            MemberKindFilter::parse(Some(bad)),
            Err(bad.to_string()),
            "{bad} must be refused, not treated as both"
        );
    }
}

#[test]
fn no_kind_reads_nothing() {
    // The enum exists so "neither" is unrepresentable: a call that queried neither table would
    // answer `{methods: [], properties: []}`, which reads as a fact about the class.
    for k in [
        MemberKindFilter::Methods,
        MemberKindFilter::Properties,
        MemberKindFilter::Both,
    ] {
        assert!(
            k.wants_methods() || k.wants_properties(),
            "{k:?} would read no table at all"
        );
    }
    assert!(
        MemberKindFilter::Methods.wants_methods() && !MemberKindFilter::Methods.wants_properties()
    );
    assert!(
        !MemberKindFilter::Properties.wants_methods()
            && MemberKindFilter::Properties.wants_properties()
    );
    assert!(MemberKindFilter::Both.wants_methods() && MemberKindFilter::Both.wants_properties());
}
