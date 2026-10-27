//! #532, write half: which globals a write refuses, and why each refusal is its own answer.
//!
//! The population tests here iterate the ACTUAL constants rather than a copy of them. A test that
//! restates a list drifts the moment the list changes and then passes while measuring nothing;
//! iterating the real slice cannot. `tests/a-count-in-prose-always-rots` is the same lesson one
//! layer up.

use iris_agentic_dev_core::tools::global::{
    first_segment, is_class_dictionary, plan, plan_write, write_verdict, GlobalAction, GlobalPlan,
    IrisGlobalParams, WriteVerdict, GLOBAL_FAMILY_UNDETERMINED, INVALID_GLOBAL_NAME,
    RESERVED_EXACT, RESERVED_SEGMENTS, SYSTEM_GLOBAL_WRITE_REFUSED,
};

fn params(action: &str, name: &str) -> IrisGlobalParams {
    IrisGlobalParams {
        action: action.to_string(),
        global_name: name.to_string(),
        subscripts: None,
        namespace: None,
        max_subscripts: None,
        max_nodes: None,
        after_subscript: None,
        after_node: None,
        value: None,
    }
}

fn set_of(name: &str, value: &str) -> IrisGlobalParams {
    let mut p = params("set", name);
    p.value = Some(value.to_string());
    p
}

// ─── the rule is a segment test, not a prefix test ───────────────────────────────────────────

/// The reason the segment rule exists. `^Ens.MessageHeaderD` is interop state; `^Ensayo` is a
/// Spanish word and somebody's data. A `^Ens*` prefix test refuses both, and this fork is used in
/// Spanish-speaking shops, so that over-refusal is not hypothetical.
#[test]
fn a_reserved_family_does_not_swallow_a_word_that_merely_starts_with_it() {
    assert!(
        !write_verdict("Ens.MessageHeaderD").is_allowed(),
        "^Ens.MessageHeaderD is interop state and must be refused"
    );
    assert!(
        !write_verdict("Ens").is_allowed(),
        "^Ens itself must be refused, not only its dotted children"
    );
    assert!(
        write_verdict("Ensayo").is_allowed(),
        "^Ensayo is an ordinary global whose name begins with the same three letters; a prefix \
         test refuses it and a segment test does not, which is the whole reason for the segment test"
    );
    assert!(
        write_verdict("EnsayoDeDatos").is_allowed(),
        "a longer word starting with Ens is also not the Ens family"
    );
}

/// Every reserved segment is refused, and for every one of them a sibling that merely starts with
/// the same letters is allowed — measured over the real slice, so adding a family to it adds a
/// case here automatically.
#[test]
fn every_reserved_segment_is_refused_and_no_sibling_of_one_is() {
    for seg in RESERVED_SEGMENTS {
        assert!(
            !write_verdict(seg).is_allowed(),
            "^{seg} is in RESERVED_SEGMENTS but write_verdict allows it"
        );
        assert!(
            !write_verdict(&format!("{seg}.Something")).is_allowed(),
            "^{seg}.Something is in the {seg} family and must be refused"
        );
        // `UnitTest` + `Root` is the deliberate exception: `^UnitTestRoot` is a real reserved
        // global and lives in RESERVED_EXACT, so it must stay refused even though the segment
        // test alone would not catch it.
        let sibling = format!("{seg}zzq");
        let expect_allowed = !RESERVED_EXACT.contains(&sibling.as_str());
        assert_eq!(
            write_verdict(&sibling).is_allowed(),
            expect_allowed,
            "^{sibling} is not the ^{seg} family: a segment test must leave it alone"
        );
    }
}

/// Every exactly-reserved name is refused, and the name plus one letter is not — `^SYS` against
/// `^SYSCONFIG` and `^SYSDATA`, which is the narrowing upstream had to make after shipping a
/// `^SYS*` prefix.
#[test]
fn every_exactly_reserved_name_is_refused_and_a_longer_name_is_not() {
    for name in RESERVED_EXACT {
        assert!(
            !write_verdict(name).is_allowed(),
            "^{name} is in RESERVED_EXACT but write_verdict allows it"
        );
        let longer = format!("{name}CONFIG");
        // Only when the longer name is not itself in a reserved family by some other rule.
        if write_verdict(&longer).is_allowed() {
            continue;
        }
        assert!(
            RESERVED_SEGMENTS.contains(&first_segment(&longer))
                || is_class_dictionary(&longer)
                || longer.starts_with('%'),
            "^{longer} is refused, but it is neither exactly reserved nor in a reserved family — \
             an exact-name rule has grown into a prefix"
        );
    }
    assert!(
        write_verdict("SYSCONFIG").is_allowed(),
        "^SYSCONFIG is application data; upstream's ^SYS* prefix swallowed it and had to be narrowed"
    );
    assert!(
        write_verdict("SYSDATA").is_allowed(),
        "^SYSDATA is application data for the same reason"
    );
}

/// `odd` plus an upper-case tag is the class dictionary. `oddments` is a word.
#[test]
fn the_class_dictionary_is_the_odd_prefix_followed_by_a_capital() {
    for g in [
        "oddDEF",
        "oddCOM",
        "oddSQL",
        "oddPROJECT",
        "oddMETA",
        "oddXML",
    ] {
        assert!(
            !write_verdict(g).is_allowed(),
            "^{g} is the class dictionary"
        );
    }
    assert!(
        write_verdict("oddments").is_allowed(),
        "^oddments is not the class dictionary: the family is odd + an upper-case tag"
    );
    // Case folding must not destroy the rule it is folding. Lowercasing `OddDEF` gives `odddef`,
    // which has no upper-case tag left, so a folded test written as "lowercase it and re-run the
    // strict test" reports ALLOWED — which is what it did. The shape survives case; the text does
    // not. `Oddments` is the control: same folded prefix, lower-case remainder, still allowed.
    for g in ["OddDEF", "ODDDEF"] {
        assert!(
            !write_verdict(g).is_allowed(),
            "^{g} is the class dictionary shape with the prefix mis-cased, so it must not be allowed"
        );
    }
    for g in ["Oddments", "oDDdef"] {
        assert!(
            write_verdict(g).is_allowed(),
            "^{g} has a lower-case remainder, so it is not the odd + UPPER-CASE-TAG shape in any \
             casing — ^Oddments is a word, and ^oDDdef is a mis-cased word, not a mis-cased \
             dictionary global"
        );
    }
}

/// The one exemption inside `%`, and it runs in the permissive direction only.
#[test]
fn the_percent_family_is_refused_except_the_customers_own() {
    for g in ["%Dictionary", "%SYS", "%oddDEF", "%Library"] {
        assert!(
            !write_verdict(g).is_allowed(),
            "^{g} is InterSystems' own % global"
        );
    }
    for g in ["%zMine", "%ZMine", "%z", "%Z"] {
        assert!(
            write_verdict(g).is_allowed(),
            "^{g} is in the customer-reserved %z/%Z range and must be writable"
        );
    }
}

// ─── "I cannot tell" is its own answer ───────────────────────────────────────────────────────

/// IRIS global names are case-sensitive, so `^ENS.Foo` is not `^Ens.Foo`. Allowing it would let a
/// mis-cased write reach interop state; refusing it as reserved would state something about the
/// caller's data that was never checked. It says it cannot tell, under its own code.
#[test]
fn a_name_differing_only_in_case_is_undetermined_rather_than_either_answer() {
    for g in [
        "ENS.Foo",
        "ens.Foo",
        "ipm.Thing",
        "OddDEF",
        "Sys",
        "routine",
    ] {
        let v = write_verdict(g);
        assert!(
            matches!(v, WriteVerdict::Undetermined { .. }),
            "^{g} differs from a reserved family only in case, so the verdict must be \
             Undetermined, not {v:?}"
        );
    }
    // The distinction this test is about, as a pair. My first draft used `IPM.Thing` as the case
    // variant, but `IPM` is itself a reserved segment, so that name is REFUSED outright — the
    // code was right and the example was wrong. The pair makes the example unable to drift back.
    assert!(
        matches!(write_verdict("IPM.Thing"), WriteVerdict::Refused { .. }),
        "^IPM.Thing IS the reserved IPM family, not a case variant of it"
    );
    assert!(
        matches!(
            write_verdict("ipm.Thing"),
            WriteVerdict::Undetermined { .. }
        ),
        "^ipm.Thing differs from that family only in letter case"
    );
}

/// The two refusals must not share a code: "that global is reserved" and "I cannot tell which
/// global you mean" have different remedies, and collapsing them is this repo's house defect.
#[test]
fn the_reserved_refusal_and_the_undetermined_one_are_different_answers() {
    let reserved = write_verdict("Ens.MessageHeaderD");
    let unsure = write_verdict("ENS.Foo");

    let (rc, rmsg) = reserved.refusal().expect("a reserved global refuses");
    let (uc, umsg) = unsure.refusal().expect("an undetermined name refuses");

    assert_eq!(rc, SYSTEM_GLOBAL_WRITE_REFUSED);
    assert_eq!(uc, GLOBAL_FAMILY_UNDETERMINED);
    assert_ne!(
        rc, uc,
        "a reserved global and a name of unknown family must not report the same code"
    );
    assert!(
        rmsg.contains("no configuration override"),
        "the reserved refusal must say there is no override, because upstream's own text claims \
         that while its code exempts kill: {rmsg}"
    );
    assert!(
        umsg.contains("case-sensitive"),
        "the undetermined message must explain WHY it cannot tell: {umsg}"
    );
    assert!(
        umsg.contains("Nothing was written"),
        "a refusal must say nothing happened, or a caller cannot tell it from a partial write: {umsg}"
    );
    assert!(
        write_verdict("IIDTest").refusal().is_none(),
        "an ordinary global must not refuse at all"
    );
}

// ─── the planner refuses before it splices ───────────────────────────────────────────────────

#[test]
fn a_write_to_a_reserved_global_is_refused_before_any_objectscript_is_built() {
    let err = plan_write(&set_of("oddDEF", "x")).expect_err("a class dictionary write is refused");
    assert_eq!(err.0, SYSTEM_GLOBAL_WRITE_REFUSED);
    assert!(
        !err.1.contains(" Set ^"),
        "the refusal must not carry generated code: nothing was spliced, and quoting a Set here \
         would hand the caller the statement it was just refused: {}",
        err.1
    );
}

#[test]
fn a_set_without_a_value_is_refused_rather_than_blanking_the_node() {
    let err = plan_write(&params("set", "IIDTest")).expect_err("set with no value is refused");
    assert_eq!(err.0, "INVALID_PARAMS");
    assert!(
        err.1.contains("global_kill") || err.1.contains("global_preview"),
        "the refusal should name the confirmed path for removal: {}",
        err.1
    );
}

/// The two planners each refuse the other's actions, so a request cannot be read as a read in one
/// place and a write in another.
#[test]
fn each_planner_refuses_the_other_halfs_actions() {
    for a in ["get", "list", "walk"] {
        let err = plan_write(&params(a, "IIDTest"))
            .expect_err("a read action must not reach the write planner");
        assert_eq!(err.0, "INVALID_PARAMS", "action={a}");
    }
    let mut p = set_of("IIDTest", "v");
    p.action = "set".to_string();
    assert!(
        iris_agentic_dev_core::tools::global::plan_read(&p).is_err(),
        "a write action must not reach the read planner"
    );
}

#[test]
fn the_plan_routes_each_action_to_the_half_that_handles_it() {
    for a in ["get", "list", "walk"] {
        match plan(&params(a, "IIDTest")) {
            Ok(GlobalPlan::Read(r)) => assert_eq!(r.action.as_str(), a),
            other => panic!("action={a} must plan as a read, got {other:?}"),
        }
    }
    match plan(&set_of("IIDTest", "v")) {
        Ok(GlobalPlan::Write(w)) => {
            assert_eq!(w.action, GlobalAction::Set);
            assert_eq!(w.name, "IIDTest");
        }
        other => panic!("action=set must plan as a write, got {other:?}"),
    }
    let err = plan(&params("destroy", "IIDTest")).expect_err("an unknown action is refused");
    assert_eq!(err.0, "INVALID_ACTION");
    for a in GlobalAction::NAMES {
        assert!(
            err.1.contains(a),
            "the refusal must list every valid action; '{a}' is missing from: {}",
            err.1
        );
    }
}

/// `is_write` is an exhaustive match with no `_` arm. This pins the classification itself, so a
/// new action cannot be quietly classified as a read by a default.
#[test]
fn exactly_one_advertised_action_writes() {
    let writes: Vec<&str> = GlobalAction::NAMES
        .iter()
        .filter(|n| {
            GlobalAction::parse(n)
                .expect("advertised name parses")
                .is_write()
        })
        .copied()
        .collect();
    assert_eq!(
        writes,
        vec!["set"],
        "set is the only write action; removal goes through global_preview and global_kill"
    );
}

// ─── the three bypasses upstream's own doc comment records ───────────────────────────────────
//
// `canonical_global_name` at upstream v1.7.0 carries this, verbatim:
//
//   "Before it existed the gate compared the string as typed while the executor stripped a caret
//    and built `^{name}`: `^ROUTINE` slipped past a rule that refused `ROUTINE`, `oddDEF("x")`
//    slipped past an exact-match rule, and a name containing `"` ended the string literal
//    `global_preview` builds."
//
// All three are closed here by ORDERING — `plan_write` normalises, then validates the name, then
// consults `write_verdict` — and ordering is exactly what no test was pinning. Source order can
// never express "on every path", so these drive the planner and read the answer.

/// The caret bypass. `^ROUTINE` and `ROUTINE` are the same global, and a gate that compares the
/// string as typed refuses only the second.
#[test]
fn a_leading_caret_does_not_carry_a_reserved_name_past_the_refusal() {
    for spelling in [
        "ROUTINE",
        "^ROUTINE",
        "^oddDEF",
        "^%Dictionary",
        "^Ens.MessageHeaderD",
    ] {
        let mut p = params("set", spelling);
        p.value = Some("never".to_string());
        let (code, _) = plan_write(&p).expect_err(&format!(
            "{spelling} names a reserved global however it is spelled"
        ));
        assert_eq!(
            code, SYSTEM_GLOBAL_WRITE_REFUSED,
            "{spelling} must reach the blocklist, not slip past it on punctuation"
        );
    }
    // The control: the caret is stripped, not merely tolerated — the same name without one is
    // still accepted when it is not reserved.
    for spelling in ["IIDTest", "^IIDTest"] {
        let mut p = params("set", spelling);
        p.value = Some("v".to_string());
        let plan = plan_write(&p).unwrap_or_else(|e| panic!("{spelling} is writable: {e:?}"));
        assert_eq!(
            plan.name, "IIDTest",
            "the caret is stripped before anything else sees it"
        );
    }
}

/// The subscript-in-the-name bypass. `oddDEF("x")` is not a name, and an exact-match rule that
/// compares it against `oddDEF` finds no match and allows it.
#[test]
fn a_subscript_smuggled_into_the_name_is_refused_as_a_name() {
    for smuggled in [
        "oddDEF(\"x\")",
        "Ens.MessageHeaderD(1)",
        "IIDTest(\"a\")",
        "%Dictionary(1,2)",
    ] {
        let mut p = params("set", smuggled);
        p.value = Some("never".to_string());
        let (code, msg) = plan_write(&p).expect_err(&format!("{smuggled} is not a global name"));
        assert_eq!(
            code, INVALID_GLOBAL_NAME,
            "{smuggled} must be refused as a NAME before the blocklist compares it to anything: \
             {msg}"
        );
        assert!(
            msg.contains("subscripts"),
            "and the refusal must say where subscripts go: {msg}"
        );
    }
}

/// The quote bypass. A name carrying a `"` would end the literal the generated code builds.
#[test]
fn a_quote_or_a_caret_inside_the_name_is_refused_rather_than_spliced() {
    for hostile in [
        "IID\"Test",
        "IIDTest\" Kill ^IIDTest Set x=\"",
        "IID^Test",
        "IIDTest)",
        "IID Test",
    ] {
        let mut p = params("set", hostile);
        p.value = Some("never".to_string());
        let (code, _) = plan_write(&p).expect_err(&format!("{hostile:?} is not a global name"));
        assert_eq!(code, INVALID_GLOBAL_NAME, "{hostile:?}");
    }
}

/// The bound upstream added with the same fix. A name long enough to be pathological is refused
/// where every other bad request is refused — before an instance is involved.
#[test]
fn an_absurdly_long_name_is_refused_here_rather_than_by_iris() {
    let long = "A".repeat(129);
    let mut p = params("set", &long);
    p.value = Some("v".to_string());
    let (code, msg) = plan_write(&p).expect_err("129 characters is past the bound");
    assert_eq!(code, INVALID_GLOBAL_NAME);
    assert!(
        msg.contains("129"),
        "the refusal states the measured length: {msg}"
    );
    assert!(
        !msg.contains(&long),
        "and it must not echo the whole name back into the message: {} chars",
        msg.len()
    );
    // The control, one character under: the bound refuses 129 and not 128.
    let ok = "A".repeat(128);
    let mut q = params("set", &ok);
    q.value = Some("v".to_string());
    assert!(
        plan_write(&q).is_ok(),
        "128 characters is inside the bound; an off-by-one here refuses legitimate names"
    );
}
