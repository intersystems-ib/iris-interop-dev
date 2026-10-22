//! #430 item 1: `iris_doc(get, shape='skeleton')`, `strip_license`, and the built-in byte cap.
//!
//! Pure tests — no IRIS, no wiremock. The transformation is a function of a string precisely so
//! that the two properties the safety argument rests on can be asserted directly: a skeleton is
//! self-identifying, and the guard that refuses to write one reads the same constant the builder
//! emits.
//!
//! The wiring lives in `doc.rs`'s `line_edit_mode_tests`
//! (`a_line_edit_never_inherits_a_partial_view`), because inheritance through `..p.clone()` is only
//! observable through the real handler.

use iris_agentic_dev_core::tools::doc_shape::{
    is_builtin_package, looks_like_a_skeleton, skeleton, strip_license_header, DocShape,
    Skeletonized, BUILTIN_DEFAULT_MAX_BYTES, SKELETON_MARKER,
};

/// A class with two real method bodies, a Parameter, a Property, an XData block and a Storage
/// block — one fixture carrying every case the walker has to tell apart.
const CLASS: &str = r#"/* Copyright (c) 2026 by InterSystems Corporation.
   Confidential property of InterSystems Corporation. */
/// The documatic comment. This is prose a reader needs.
Class Demo.Thing Extends %Persistent
{

Parameter VERSION = 3;

Property Name As %String(MAXLEN = 120);

/// Adds two numbers.
Method Add(a As %Integer, b As %Integer) As %Integer
{
	Set total = a + b
	Quit total
}

ClassMethod Reset() As %Status
{
	Set ^DemoCounter = 0
	Quit $$$OK
}

/// Guards a value. Its body contains a NESTED brace block on purpose.
Method Guard(x As %Integer) As %Status
{
	If (x > 0) {
		Quit $$$OK
	}
	Quit $$$ERROR($$$GeneralError, "out of range")
}

XData DTL [ XMLNamespace = "http://www.intersystems.com/dtl" ]
{
<transform sourceClass="A" targetClass="B">
<assign value="source.Foo" property="target.Bar"/>
</transform>
}

Storage Default
{
<Data name="ThingDefaultData">
<Value name="1">
<Value>%%CLASSNAME</Value>
</Value>
</Data>
<DataLocation>^Demo.ThingD</DataLocation>
<DefaultData>ThingDefaultData</DefaultData>
<Type>%Storage.Persistent</Type>
}

}
"#;

// ─── DocShape::parse: strict, like #435's MemberKindFilter ────────────────────────────────────

#[test]
fn an_omitted_shape_is_full() {
    assert_eq!(DocShape::parse(None), Ok(DocShape::Full));
    assert_eq!(DocShape::parse(Some("")), Ok(DocShape::Full));
    assert_eq!(DocShape::parse(Some("  ")), Ok(DocShape::Full));
    assert_eq!(DocShape::parse(Some("full")), Ok(DocShape::Full));
    assert_eq!(DocShape::parse(Some(" skeleton ")), Ok(DocShape::Skeleton));
}

/// A near miss is REFUSED, never widened to `full`.
///
/// #435 settled this for `docs_introspect`: silently treating an unrecognised filter as the
/// permissive case returns the whole thing to a caller who asked to be lean, and nothing in the
/// response says the filter was dropped. Same rule, same reason.
#[test]
fn a_near_miss_shape_is_refused_not_widened() {
    for bad in [
        "skeletons",
        "SKELETON",
        "Skeleton",
        "skel",
        "signatures",
        "none",
        "0",
    ] {
        let r = DocShape::parse(Some(bad));
        assert!(r.is_err(), "{bad:?} must be refused, got {r:?}");
        let msg = r.unwrap_err();
        assert!(
            msg.contains(bad),
            "the refusal must quote what was passed: {msg:?}"
        );
        assert!(
            msg.contains("skeleton") && msg.contains("full"),
            "and name both valid values: {msg:?}"
        );
    }
}

// ─── is_builtin_package: matched on the package segment ──────────────────────────────────────

#[test]
fn the_builtin_packages_are_matched_on_the_segment() {
    for yes in [
        "%Library.File.cls",
        "%String",
        "Ens.Director.cls",
        "EnsLib.HL7.Message.cls",
        "HS.FHIRServer.Service.cls",
    ] {
        assert!(is_builtin_package(yes), "{yes} is built-in");
    }
}

/// The prefix trap. A bare `starts_with("Ens")` would swallow `Ensemble.*`, and
/// `starts_with("HS")` would swallow a user's `HSCustom.*` — truncating the caller's OWN class by
/// default, which is the exact opposite of what the cap is for.
#[test]
fn a_package_that_merely_starts_with_a_builtin_name_is_not_builtin() {
    for no in [
        "Ensemble.Foo.cls",
        "EnsLibrary.Thing.cls",
        "HSCustom.Patient.cls",
        "MyApp.Patient.cls",
        "Demo.Thing.cls",
        "MyRoutine",
        "",
    ] {
        assert!(!is_builtin_package(no), "{no:?} is NOT built-in");
    }
}

#[test]
fn the_builtin_cap_is_between_the_measured_p90_and_max() {
    // #430 measured p90 5.3k and max 12.6k chars on iris_doc(get). A cap below p90 would truncate
    // the ordinary case; one above the max would bound nothing. This asserts the derivation rather
    // than restating the number in prose (CLAUDE.md).
    // `black_box` so this is a RUNTIME comparison. Both sides are consts, and clippy's
    // `assertions_on_constants` is right that a const-folded assert tests nothing — it is decided
    // by the compiler, and it would still "pass" if the constant were edited to a value the
    // comparison rejects, because the whole statement would simply be removed.
    let cap = std::hint::black_box(BUILTIN_DEFAULT_MAX_BYTES);
    assert!(cap > 5_300, "cap {cap} must clear the measured p90");
    assert!(
        cap < 12_600,
        "cap {cap} must actually bound the measured max"
    );
}

// ─── skeleton(): what comes out, and what must not ───────────────────────────────────────────

fn elided(src: &str) -> (String, usize, bool) {
    match skeleton(src) {
        Skeletonized::Elided {
            content,
            bodies,
            storage,
        } => (content, bodies, storage),
        other => panic!("expected Elided, got {other:?}"),
    }
}

#[test]
fn a_skeleton_drops_method_bodies_and_counts_them() {
    let (out, bodies, storage) = elided(CLASS);
    assert_eq!(bodies, 3, "Add, Reset and Guard: {out}");
    assert!(storage, "the Storage block was dropped: {out}");
    // The bodies are gone...
    assert!(!out.contains("Set total = a + b"), "body of Add: {out}");
    assert!(
        !out.contains("Set ^DemoCounter = 0"),
        "body of Reset: {out}"
    );
    // ...and the Storage internals with them.
    assert!(!out.contains("^Demo.ThingD"), "DataLocation: {out}");
    assert!(!out.contains("%Storage.Persistent"), "Storage type: {out}");
    // A body holding a NESTED brace must be removed WHOLE. Cutting at the first `}` instead of the
    // last would stop inside `If (x > 0) { ... }` and leave the tail of the method behind —
    // output that is both leaky and no longer parseable as a class.
    assert!(
        !out.contains("out of range"),
        "the tail after a nested brace block must go too: {out}"
    );
    assert!(
        !out.contains("If (x > 0)"),
        "and the nested block itself: {out}"
    );
}

/// The API surface survives byte-for-byte. Each of these is a thing a model reading an unfamiliar
/// class actually needs, so a skeleton that drops any of them is not lean, it is broken.
#[test]
fn a_skeleton_keeps_the_whole_api_surface() {
    let (out, _, _) = elided(CLASS);
    for kept in [
        "Class Demo.Thing Extends %Persistent",
        "/// The documatic comment. This is prose a reader needs.",
        "Parameter VERSION = 3;",
        "Property Name As %String(MAXLEN = 120);",
        "/// Adds two numbers.",
        "Method Add(a As %Integer, b As %Integer) As %Integer",
        "ClassMethod Reset() As %Status",
        "Method Guard(x As %Integer) As %Status",
        "/// Guards a value. Its body contains a NESTED brace block on purpose.",
    ] {
        assert!(out.contains(kept), "must keep {kept:?}:\n{out}");
    }
}

/// An XData block is where a DTL and a BPL LIVE. Its body is spelled with the same node kind as a
/// method body (`external_method_body_content`), so a walker keying on the node kind alone would
/// delete the transform and leave a class that no longer describes anything.
#[test]
fn a_skeleton_keeps_xdata_because_that_is_where_a_dtl_lives() {
    let (out, _, _) = elided(CLASS);
    assert!(
        out.contains(r#"<transform sourceClass="A" targetClass="B">"#),
        "the DTL body must survive:\n{out}"
    );
    assert!(
        out.contains(r#"<assign value="source.Foo" property="target.Bar"/>"#),
        "including its assigns:\n{out}"
    );
    assert!(
        out.contains(r#"XData DTL [ XMLNamespace = "http://www.intersystems.com/dtl" ]"#),
        "and its header:\n{out}"
    );
}

/// An elided body is never left as a bare `{}`.
///
/// An empty method body is a legitimate thing for a class to contain, so a bare `{}` would turn
/// "I did not fetch this" into "this method does nothing" — the repo's dominant bug shape
/// (CLAUDE.md), applied to source a model is about to reason about.
#[test]
fn every_elision_says_so_in_place() {
    let (out, bodies, _) = elided(CLASS);
    let markers = out.matches(SKELETON_MARKER).count();
    // One per body, one for Storage, one in the banner. Derived from `bodies` rather than
    // written as a literal, so adding a method to the fixture cannot leave this stale.
    assert_eq!(
        markers,
        bodies + 2,
        "every removal plus the banner must be marked, got {markers}:\n{out}"
    );
    assert!(
        !out.contains("{\n}"),
        "no body may read as a deliberately empty one:\n{out}"
    );
}

#[test]
fn a_skeleton_announces_itself_in_its_first_line() {
    let (out, _, _) = elided(CLASS);
    let first = out.split('\n').next().unwrap_or_default();
    assert!(
        first.contains(SKELETON_MARKER),
        "a caller keeping only the first line still holds a detectable skeleton: {first:?}"
    );
    assert!(
        first.contains("shape='full'"),
        "and the banner names the way back: {first:?}"
    );
}

/// The round trip the write guard depends on: whatever `skeleton` produces,
/// `looks_like_a_skeleton` must recognise. Asserted by COMPOSING the two rather than by checking
/// for a literal, so rewording the marker cannot leave the guard passing while it stops matching.
#[test]
fn a_skeleton_is_recognised_by_the_write_guard() {
    let (out, _, _) = elided(CLASS);
    assert!(looks_like_a_skeleton(&out), "skeleton must be detected");
    assert!(
        !looks_like_a_skeleton(CLASS),
        "ordinary source must NOT be refused"
    );
}

/// CRLF must survive. The lesson is borrowed from the line-edit core: `str::lines()` strips the
/// `\r`, so a module that used it would LF-convert a whole CRLF class on every read — a
/// whole-file diff out of a view that was supposed to be read-only.
#[test]
fn crlf_survives_the_skeleton() {
    let crlf = CLASS.replace('\n', "\r\n");
    let (out, bodies, _) = elided(&crlf);
    // The SAME count the LF fixture yields, derived rather than restated: the two must agree, and
    // a literal here goes stale the moment a method is added to the fixture (it just did).
    let lf_bodies = match skeleton(CLASS) {
        Skeletonized::Elided { bodies, .. } => bodies,
        other => panic!("the LF fixture must elide, got {other:?}"),
    };
    assert_eq!(
        bodies, lf_bodies,
        "CRLF must elide exactly what LF does: {out}"
    );
    assert!(
        out.contains("Extends %Persistent\r\n"),
        "CR must survive on a kept line:\n{out:?}"
    );
    assert!(
        !out.contains("Set total = a + b"),
        "and the body is still gone:\n{out}"
    );
}

// ─── the two non-Elided cases ────────────────────────────────────────────────────────────────

/// A class with nothing to remove is NOT a failure, and is deliberately returned WITHOUT the
/// banner: it is byte-identical to its full source, so marking it would make `put` refuse a
/// document that is entirely safe to write.
#[test]
fn a_class_with_no_bodies_and_no_storage_has_nothing_to_elide() {
    let src = "Class Demo.Bare Extends %RegisteredObject\n{\nProperty A As %String;\n}\n";
    assert_eq!(skeleton(src), Skeletonized::NothingToElide);
}

/// A class whose METHOD BODIES ARE EMPTY also has nothing to elide — and must not be marked.
///
/// Added because a mutation that made an empty body count as an elision SURVIVED: the fixture
/// above has no method at all, so `body_span` never ran and the test could not see the change. The
/// case matters on its own — `doc.rs`'s own line-edit fixture is exactly this class, and an
/// empty-bodied class that came back banner-marked would make `put` refuse a document that loses
/// nothing by being written back.
#[test]
fn a_method_with_an_empty_body_is_not_an_elision() {
    let src = "Class Demo.Empty Extends %RegisteredObject\n{\nMethod A()\n{\n}\n}\n";
    assert_eq!(
        skeleton(src),
        Skeletonized::NothingToElide,
        "an empty body is not something that was removed"
    );
    assert!(
        !looks_like_a_skeleton(src),
        "and so it is not refused on write"
    );
}

/// The brace pair a method body sits between is a pair of DIRECT children of `method_definition`,
/// exactly one of each — a nested `If x { ... }` block lives inside a `statement` node, so its
/// braces are grandchildren and never reach `body_span`.
///
/// Measured, because the first version of `body_span` carried a comment claiming the opposite (that
/// taking the LAST `}` is what stops the cut landing on an inner brace). A mutation to take the
/// FIRST `}` survived, which is what exposed the claim as untrue: with one `}` among the direct
/// children the two are the same node. This test is what makes that a measured fact rather than a
/// rationale, and it will fail if the grammar ever flattens nested blocks.
#[test]
fn a_method_definition_has_exactly_one_brace_pair_as_direct_children() {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_objectscript::LANGUAGE_OBJECTSCRIPT_UDL.into())
        .expect("the UDL grammar loads");
    let tree = parser.parse(CLASS, None).expect("the fixture parses");

    fn count_braces(n: tree_sitter::Node, found: &mut Vec<(usize, usize)>) {
        if n.kind() == "method_definition" {
            let mut c = n.walk();
            let mut open = 0;
            let mut close = 0;
            for ch in n.children(&mut c) {
                match ch.kind() {
                    "{" => open += 1,
                    "}" => close += 1,
                    _ => {}
                }
            }
            found.push((open, close));
            return;
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            count_braces(ch, found);
        }
    }

    let mut found = Vec::new();
    count_braces(tree.root_node(), &mut found);
    // The denominator: the fixture's three methods must all have been visited (CLAUDE.md — a
    // clean pass over an empty population is not evidence).
    assert_eq!(found.len(), 3, "Add, Reset and Guard: {found:?}");
    for (open, close) in &found {
        assert_eq!(
            (*open, *close),
            (1, 1),
            "one brace pair per method: {found:?}"
        );
    }
}

/// A routine is not a class, and saying so is not the same as saying "nothing to elide". Returning
/// the source labelled `skeleton` would state that bodies were removed when none were.
#[test]
fn a_routine_is_not_a_class() {
    let routine = "ROUTINE Demo.Util\nMain()\n\tWrite \"hi\",!\n\tQuit\n";
    assert_eq!(skeleton(routine), Skeletonized::NotAClass);
    assert_eq!(skeleton(""), Skeletonized::NotAClass);
}

// ─── strip_license_header ────────────────────────────────────────────────────────────────────

#[test]
fn a_leading_block_comment_is_the_license_and_comes_off() {
    let (out, removed) = strip_license_header(CLASS);
    assert!(removed, "a /* */ header was present");
    assert!(
        !out.contains("Confidential property of InterSystems"),
        "the license is gone:\n{out}"
    );
    assert!(
        out.starts_with("/// The documatic comment."),
        "and what follows starts the document:\n{out:?}"
    );
}

/// `///` is DOCUMENTATION, not a license. #430 groups "license header" and "verbose /// comments"
/// in one phrase; they are different things, and the documatic comment is the single most useful
/// line in an unfamiliar class.
#[test]
fn a_documatic_comment_is_not_a_license() {
    let src = "/// Adds two numbers.\nClass Demo.T Extends %RegisteredObject\n{\n}\n";
    let (out, removed) = strip_license_header(src);
    assert!(!removed, "nothing to strip");
    assert_eq!(out, src, "and the text is untouched");
}

#[test]
fn no_license_block_reports_false_rather_than_erroring() {
    let src = "Class Demo.T Extends %RegisteredObject\n{\n}\n";
    let (out, removed) = strip_license_header(src);
    assert!(!removed);
    assert_eq!(out, src);
}

/// An unterminated `/*` means the whole document is one comment, or the source arrived truncated.
/// Stripping to end-of-file would return nothing at all, so the text is left alone and `false`
/// says no license was removed — "I could not tell" answered as itself, not as success.
#[test]
fn an_unterminated_license_block_is_left_alone() {
    let src = "/* Copyright (c) 2026 and then the file was cut off\nClass Demo.T\n";
    let (out, removed) = strip_license_header(src);
    assert!(!removed, "nothing was safely removable");
    assert_eq!(out, src, "and nothing was lost:\n{out:?}");
}
