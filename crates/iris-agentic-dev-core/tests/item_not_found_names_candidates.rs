//! #329 item 2: `ITEM_NOT_FOUND` must name the items the production DOES hold.
//!
//! Four sites emitted a bare `Item not found: X`, so the caller was told the name it had just passed
//! and nothing else — the negative-fact shape CLAUDE.md is about, in the tool a model reaches for
//! when it has guessed a config item name wrong.
//!
//! ## Why the candidates arrive as their own lines
//!
//! `ITEM_CANDIDATES_N:<n>` then one `ITEM_CANDIDATE:<name>` per line. NOT appended to the error
//! line: packing an unbounded list onto a single line is exactly what #347 fixed in
//! `execute_method` and `coverage`, where a CRLF-joined `%Status` chain lost everything after the
//! first line and nothing reported it.
//!
//! The declared count is load-bearing. If fewer names arrive than were declared, the reader reports
//! a FLOOR — never a list — because a short read must not become "the production contains these
//! three items".
//!
//! ## Why this was safe to write at all
//!
//! Each site is `If <bad> { Write … Quit }`, and appending writes before that `Quit` only works if
//! `Quit` returns from the METHOD rather than exiting the block. Measured on IRIS 2026.1 with both
//! constructs in one probe: `If { … Quit }` does return (the statement after the block never ran),
//! while `Try { … Quit }` falls through past the `Catch` — which is the #349 defect. Had it been the
//! other way round, these writes would have been followed by `RemoveAt(0)` and `Set tItem.…` on an
//! invalid handle.

use iris_agentic_dev_core::tools::interop::{
    item_not_found_block, item_not_found_envelope, parse_item_candidates, Candidates,
};
use std::path::PathBuf;

fn interop_rs() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
    p.push("interop.rs");
    p
}

/// `//` comments stripped. Required: the doc comments for this fix quote both `{not_found}` and the
/// call they replaced, and a first pass over these readers counted FIVE because one occurrence was
/// the comment describing the change.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_complete_candidate_list_is_named_in_the_message() {
    let payload = "Item not found: MyApp.BS.Missing\n\
                   ITEM_CANDIDATES_N:3\n\
                   ITEM_CANDIDATE:MyApp.BS.FileIn\n\
                   ITEM_CANDIDATE:MyApp.BO.Rest\n\
                   ITEM_CANDIDATE:MyApp.BP.Router\n";
    let (msg, cands) = parse_item_candidates(payload);
    assert_eq!(msg, "Item not found: MyApp.BS.Missing");
    assert_eq!(
        cands,
        Candidates::All(vec![
            "MyApp.BS.FileIn".into(),
            "MyApp.BO.Rest".into(),
            "MyApp.BP.Router".into()
        ])
    );
    let (rendered, extra) = item_not_found_envelope(payload);
    for n in ["MyApp.BS.FileIn", "MyApp.BO.Rest", "MyApp.BP.Router"] {
        assert!(rendered.contains(n), "{n} must be named: {rendered}");
    }
    assert_eq!(extra["items_total"], 3, "{extra}");
    assert!(
        extra["items_truncated"].is_null(),
        "a complete list must not be flagged truncated: {extra}"
    );
}

#[test]
fn fewer_names_than_declared_is_reported_as_a_floor_not_a_list() {
    // The #347 lesson applied: a short read must not read as the whole set.
    let payload = "Item not found: X\n\
                   ITEM_CANDIDATES_N:9\n\
                   ITEM_CANDIDATE:A\n\
                   ITEM_CANDIDATE:B\n";
    let (_, cands) = parse_item_candidates(payload);
    assert_eq!(
        cands,
        Candidates::Partial {
            names: vec!["A".into(), "B".into()],
            declared: 9
        }
    );
    let (rendered, extra) = item_not_found_envelope(payload);
    assert!(
        rendered.contains("FLOOR") || rendered.contains("floor"),
        "the message must say it is a floor: {rendered}"
    );
    assert_eq!(
        extra["items_total"], 9,
        "the DECLARED total, not the arrived count: {extra}"
    );
    assert_eq!(extra["items_truncated"], true, "{extra}");
}

#[test]
fn a_missing_candidate_block_is_unavailable_not_an_empty_production() {
    // The whole point of the third state. An older server, or a listing that failed, tells us
    // NOTHING about what the production holds.
    let payload = "Item not found: X";
    let (_, cands) = parse_item_candidates(payload);
    assert!(
        matches!(cands, Candidates::Unavailable(_)),
        "no block must be Unavailable, not All(vec![]): {cands:?}"
    );
    let (rendered, extra) = item_not_found_envelope(payload);
    assert!(
        extra["items_in_production"].is_null(),
        "must be null, never []: {extra}"
    );
    assert!(
        rendered.contains("NOT") && rendered.contains("empty"),
        "the message must deny that this means empty: {rendered}"
    );
    assert_eq!(extra["items_total"], serde_json::Value::Null, "{extra}");
}

#[test]
fn a_genuinely_empty_production_says_so_and_is_not_unavailable() {
    // The other side of the same coin, and the reason `All(vec![])` exists: zero DECLARED and zero
    // arrived is a real answer, and must not be rendered as a failure.
    let payload = "Item not found: X\nITEM_CANDIDATES_N:0\n";
    let (_, cands) = parse_item_candidates(payload);
    assert_eq!(cands, Candidates::All(vec![]));
    let (rendered, extra) = item_not_found_envelope(payload);
    assert_eq!(
        extra["items_in_production"],
        serde_json::json!([]),
        "{extra}"
    );
    assert_eq!(extra["items_total"], 0, "{extra}");
    assert!(
        rendered.contains("no configured items"),
        "an empty production must say so plainly: {rendered}"
    );
    assert!(
        extra["items_unavailable"].is_null(),
        "empty is not unavailable: {extra}"
    );
}

#[test]
fn an_inconsistent_count_is_unavailable_rather_than_believed() {
    // MORE names than declared: nothing sane produces this, so the declared count cannot be trusted
    // and neither can the list. Rendering either would be inventing a fact.
    let payload = "Item not found: X\nITEM_CANDIDATES_N:1\nITEM_CANDIDATE:A\nITEM_CANDIDATE:B\n";
    let (_, cands) = parse_item_candidates(payload);
    assert!(
        matches!(cands, Candidates::Unavailable(_)),
        "an inconsistent block must be Unavailable: {cands:?}"
    );
}

#[test]
fn the_emitter_puts_the_names_on_their_own_lines() {
    let code = item_not_found_block("\"MyApp.Missing\"");
    let lines: Vec<&str> = code.lines().collect();
    // The error line must END after the item — a name list appended here is the #347 defect.
    let err_line = lines
        .iter()
        .find(|l| l.contains("ERROR:ITEM_NOT_FOUND:"))
        .expect("the emitter must write the error line");
    assert!(
        err_line.contains("$C(10)"),
        "the error line must be newline-terminated so the candidates start a new line: {err_line}"
    );
    assert!(
        !err_line.contains("ITEM_CANDIDATE"),
        "candidates must NOT be packed onto the error line: {err_line}"
    );
    assert!(
        code.contains("ITEM_CANDIDATES_N:"),
        "the declared count is what makes a short read detectable: {code}"
    );
    assert!(
        code.contains("ITEM_CANDIDATE:"),
        "the per-name marker is missing: {code}"
    );
    assert!(
        code.trim_end().ends_with("Quit"),
        "the block must end by returning, or execution continues onto an invalid handle: {code}"
    );
}

/// Count CALLS to `name`, not every occurrence of the string.
///
/// Two things must not count, and a naive `matches(name)` counts both: the function's own
/// declaration, and an identifier that merely ENDS with the name — `interop.rs` has a test called
/// `production_item_error_mapping_item_not_found`, which is why the spelling-independent count first
/// read 5 where 4 was right. A name appearing inside a longer identifier is the commonest false
/// witness in a source-reading guard.
fn call_sites(text: &str, name: &str) -> usize {
    let needle = format!("{name}(");
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("fn ") || t.starts_with("pub fn ") || t.starts_with("async fn "))
        })
        .map(|l| {
            let mut n = 0;
            let mut from = 0;
            while let Some(at) = l[from..].find(&needle) {
                let abs = from + at;
                let prev_is_ident = l[..abs]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !prev_is_ident {
                    n += 1;
                }
                from = abs + needle.len();
            }
            n
        })
        .sum()
}

/// The sibling-defect guard: this fix is only correct if EVERY site carries it, because three out of
/// four leaves the fourth looking more trustworthy than it is.
///
/// #327 item 3 changed the POPULATION, and that is what this test is for. `get_settings` no longer
/// interpolates the shared block: it reads several items in one program, and the shared block ends in
/// `Quit` by design — which returns from the METHOD, so one missing item would have ended the run and
/// every later item would have been absent from the output. It emits the same declared-count
/// candidate protocol itself, once, after the last item.
///
/// So the instrument counts what the claim is about — every generator that can report a missing item
/// emits a DECLARED candidate count, and every reader routes through the one envelope builder — not
/// one particular call spelling. The count stays 4 in both halves; the fourth member changed.
#[test]
fn all_four_sites_and_all_four_readers_use_the_shared_definitions() {
    let text =
        code_only(&std::fs::read_to_string(interop_rs()).expect("interop.rs must be readable"));
    // Three single-item sites interpolate the shared block...
    let shared = text.matches("{not_found}").count();
    // ...and the batch reader emits the protocol inline, for the reason above. Identified by the
    // marker only it writes, so this cannot be satisfied by any of the three above.
    let inline = text
        .matches(r#"Write \"ITEM_CANDIDATES_N:\"_tProd.Items.Count()"#)
        .count();
    let emit = shared + inline;
    assert_eq!(
        inline, 1,
        "expected exactly one generator emitting the candidate protocol inline (the batch read), \
         found {inline} — if get_settings went back to the shared block, one missing item now ends \
         the whole program"
    );
    // Every CALL to the envelope builder — see `call_sites` for the two things that must not count.
    let readers = call_sites(&text, "item_not_found");
    assert_eq!(
        emit, 4,
        "expected 4 ObjectScript generators emitting a declared candidate count, found {emit} \
         ({shared} via the shared block + {inline} inline)"
    );
    assert_eq!(
        readers, 4,
        "expected 4 readers routed through the shared envelope builder, found {readers}"
    );
    // CONTROLS for the counter itself.
    assert!(
        text.contains("fn item_not_found("),
        "the definition must be in the file this test reads, or the counter is reading the wrong file"
    );
    assert_eq!(
        call_sites("pub fn item_not_found(payload: &str) {}", "item_not_found"),
        0,
        "a declaration is not a call site"
    );
    assert_eq!(
        call_sites(
            "    fn production_item_error_mapping_item_not_found() {",
            "item_not_found"
        ),
        0,
        "an identifier that merely ENDS with the name is not a call site — this is the occurrence \
         that made the first version of this counter read 5"
    );
    assert_eq!(
        call_sites("        return item_not_found(msg);", "item_not_found"),
        1,
        "and a real call must still count, or the two negatives above are satisfied by counting \
         nothing at all"
    );
    // CONTROL: no site still emits the bare form. Without this, adding a 5th site with the old
    // text would pass the two counts above.
    let bare = text
        .matches("Write \"ERROR:ITEM_NOT_FOUND:Item not found: \"_{item} Quit")
        .count();
    assert_eq!(
        bare, 0,
        "a site still emits the bare refusal: {bare} occurrence(s)"
    );
    eprintln!("shared block at {emit} emit sites, shared envelope at {readers} readers");
}

#[test]
fn the_comment_stripper_keeps_code_and_drops_prose() {
    // Positive AND negative control for `code_only`, which the count test depends on: the doc
    // comments for this change quote both markers it counts.
    let sample = "// four readers used to call item_not_found(msg) here\n\
                  let x = item_not_found(msg);\n\
                  /// and {not_found} is interpolated below\n\
                  code = format!(\"{not_found}\");";
    let out = code_only(sample);
    assert_eq!(out.matches("item_not_found(msg)").count(), 1, "{out}");
    assert_eq!(out.matches("{not_found}").count(), 1, "{out}");
}
