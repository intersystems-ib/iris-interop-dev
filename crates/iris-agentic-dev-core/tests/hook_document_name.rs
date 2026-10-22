//! #530: a document's name comes from its header, never from its path.
//!
//! `scripts/compile-hook.sh:31-40` built the name as `<one parent folder>.<file name>`, so
//! `src/OM/API/Customer.cls` was compiled as `API.Customer.cls` — a document that does not exist.
//! IRIS answers that with a 404, and the hook reported the 404 as a namespace problem.
//!
//! The fix cannot be "use more parent folders". The three layouts in #530's own table need three
//! different numbers of them, and the last row (`src/Customer.cls` holding `Class OM.Customer`)
//! has no number that works. Only the file says what the file is.
//!
//! Each case below is paired with what the old path rule would have produced, so a regression that
//! reintroduces path-derivation fails rather than merely looking different.

use iris_agentic_dev_core::tools::doc::{doc_name_from_source, SourceDocName};

fn name(content: &str) -> String {
    match doc_name_from_source(content) {
        SourceDocName::Named(n) => n,
        SourceDocName::Unnamed(why) => panic!("expected a name, got refusal: {why}"),
    }
}

fn refusal(content: &str) -> &'static str {
    match doc_name_from_source(content) {
        SourceDocName::Named(n) => panic!("expected a refusal, got the name {n:?}"),
        SourceDocName::Unnamed(why) => why,
    }
}

/// #530 row 1. The path says `API`, the file says `OM.API.Customer`.
#[test]
fn a_nested_package_is_named_by_its_class_line() {
    let got = name("Class OM.API.Customer Extends %Persistent\n{\n}\n");
    assert_eq!(got, "OM.API.Customer.cls");
    assert_ne!(
        got, "API.Customer.cls",
        "that is what one parent folder produced, and the document does not exist"
    );
}

/// #530 row 3, the row no path rule can satisfy: a flat file holding a packaged class.
#[test]
fn a_flat_file_holding_a_packaged_class_follows_the_content() {
    let got = name("Class OM.Customer Extends %Persistent\n{\n}\n");
    assert_eq!(got, "OM.Customer.cls");
    assert_ne!(
        got, "src.Customer.cls",
        "the old rule prefixed the source folder itself"
    );
}

/// The control that proves this is content-driven: the function cannot see a path at all, so two
/// files at different depths holding the same class must name the same document.
#[test]
fn the_same_class_is_the_same_document_wherever_the_file_sits() {
    let content = "Class OM.API.Customer Extends %Persistent\n{\n}\n";
    assert_eq!(name(content), "OM.API.Customer.cls");
    // Same call, same answer — there is no path parameter to vary. Stated as a test because the
    // defect was precisely that the name depended on where the file lived.
    assert_eq!(name(content), name(&format!("{content}\n")));
}

/// The commonest real file in this repo, and a case #530 does not mention: an exported `.cls`
/// opens with `///` doc comments. Reading only the first line would refuse it.
#[test]
fn doc_comments_and_includes_before_the_class_line_do_not_hide_it() {
    let got = name(
        "/// A customer record.\n\
         /// Second line of the comment.\n\
         Include OMMACROS\n\
         \n\
         Class OM.API.Customer Extends %Persistent\n{\n}\n",
    );
    assert_eq!(got, "OM.API.Customer.cls");
}

/// #530 row 4.
#[test]
fn a_routine_with_type_inc_is_an_include() {
    assert_eq!(name("ROUTINE OMMACROS [Type=INC]\n"), "OMMACROS.inc");
}

/// `[ Type = INC ]` is legal ObjectScript. Spaces must not change the answer.
#[test]
fn the_type_flag_survives_spaces() {
    assert_eq!(name("ROUTINE OMMACROS [ Type = INC ]\n"), "OMMACROS.inc");
}

/// Not in #530's table, but it is the third routine type and `iris_doc`'s own
/// `doc_name_with_suffix` gets it wrong (anything not INC becomes `.mac`, with no test pinning
/// it). Filed separately rather than changed there; this path answers it correctly.
#[test]
fn a_routine_with_type_int_is_an_intermediate() {
    assert_eq!(name("ROUTINE OMORDL [Type=INT]\n"), "OMORDL.int");
}

/// The legacy-ObjectScript default: dot syntax in a plain `.mac`.
#[test]
fn a_plain_routine_is_a_mac() {
    assert_eq!(
        name("ROUTINE OMORDL\nLIST ; list orders\n . set x=1\n quit\n"),
        "OMORDL.mac"
    );
}

/// #530 row 5. A guessed name compiles the wrong document; a refusal can be reported.
#[test]
fn a_file_with_no_header_is_refused_rather_than_guessed() {
    let why = refusal(" set x=1\n quit\n");
    assert!(
        why.contains("Class") && why.contains("ROUTINE"),
        "the refusal must say what it looked for, got {why:?}"
    );
}

/// A ROUTINE header is a header only on the first line — IRIS's rule. A body that merely mentions
/// the word must not be read as one, or a `.mac` gets named after its own comment.
///
/// The first fixture here is the one that matters, and the first version of this test did NOT have
/// it. It used only the comment case below, whose `trim_start()` begins with `;` rather than
/// `ROUTINE`, so a mutant that scanned EVERY line for a ROUTINE header found nothing either and
/// the test passed for the wrong reason — it survived the mutation pass. A later line that really
/// does begin `ROUTINE ` at column 0 is what distinguishes the two.
#[test]
fn routine_is_only_a_header_on_the_first_line() {
    // A genuine ROUTINE line, just not the first one. Scanning for it would name this OMOTHER.mac.
    let why = refusal(" set x=1\nROUTINE OMOTHER\n quit\n");
    assert!(
        why.contains("ROUTINE"),
        "the refusal should say it wanted a header, got {why:?}"
    );
    // And the weaker case: the word appearing inside a comment.
    let why = refusal(" ; see ROUTINE OMOTHER for the caller\n set x=1\n");
    assert!(!why.is_empty());
}

/// An empty file settles nothing, and must not become `.cls`.
#[test]
fn an_empty_file_is_refused() {
    assert!(matches!(
        doc_name_from_source(""),
        SourceDocName::Unnamed(_)
    ));
}

/// A `Class` line whose second token is not a name means the line was not what we thought.
#[test]
fn a_class_line_without_a_usable_name_is_refused() {
    assert!(matches!(
        doc_name_from_source("Class {\n}\n"),
        SourceDocName::Unnamed(_)
    ));
}
