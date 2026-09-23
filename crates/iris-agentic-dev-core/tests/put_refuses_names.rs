//! #327 item 4: `iris_doc` mode=put must refuse a `names` array, not discard it.
//!
//! `names` is read NOWHERE in `handle_put`. Measured on master with a word-boundary scan
//! (`p\.names\b`, which correctly rejects `p.namespace` — a plain substring search reported 11
//! false hits inside `handle_put` because every one was the `names` inside `namespace`):
//!
//! ```text
//! handle_get    5 reads
//! handle_delete 2 reads
//! handle_put    0 reads
//! ```
//!
//! The non-zero siblings are what make put's zero trustworthy rather than a broken scan.
//!
//! So a caller passing `name` AND `names` had the single `name` written, the rest discarded, and
//! `success: true` returned — a partial write reported as a whole one. That is the house rule
//! (CLAUDE.md) with data loss attached, which is why this is a refusal and not a warning field.
//!
//! ## The advertised contract contradicted itself
//!
//! The tool description promised "batch ops via 'names' array" with NO mode restriction, while the
//! `names` field's own doc comment — which ships inside the advertised `inputSchema` — said
//! "Multiple document names for batch get/delete". A caller reading the description concluded put
//! batches; a caller reading the schema concluded it does not. Both were shipped. The description
//! now agrees with the field, and `no_unrestricted_batch_claim_in_the_description` keeps it that way.
//!
//! ## Why the ORDERING is asserted
//!
//! A refusal that arrives after the document has been written is not a refusal. The guard therefore
//! pins that the check precedes the first write in the function, not merely that it exists.

use std::path::PathBuf;

fn doc_rs() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
    p.push("doc.rs");
    p
}

fn mod_rs() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
    p.push("mod.rs");
    p
}

fn read(p: &PathBuf) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — this guard must fail, not skip",
            p.display()
        )
    })
}

/// `//` comments removed.
///
/// Required, not hygiene: the comment introducing this very refusal quotes `p.names` and the scan
/// pattern, so a guard greping raw source would match its own rationale. That false witness has
/// broken two guards in this repo.
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("//") {
            Some(at) => &l[..at],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Body of `async fn <name>(` up to its matching brace.
fn fn_body(text: &str, sig: &str) -> String {
    let at = text.find(sig).unwrap_or_else(|| {
        panic!("{sig} not found — renamed? then update this guard, do not delete it")
    });
    let open = text[at..].find('{').map(|o| at + o).expect("body brace");
    let mut depth = 0i32;
    for (off, ch) in text[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return text[open..open + off + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after {sig}");
}

/// Index just past the `}` matching the `{` at `open`.
fn end_of_block(text: &str, open: usize) -> usize {
    let mut depth = 0i32;
    for (off, ch) in text[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + off + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// Count of real `p.names` reads — word-boundary aware, so `p.namespace` does not count.
fn p_names_reads(code: &str) -> usize {
    let mut n = 0usize;
    let pat = "p.names";
    let mut from = 0usize;
    while let Some(rel) = code[from..].find(pat) {
        let at = from + rel;
        let next = code[at + pat.len()..].chars().next();
        // `p.namespace` continues with an alphanumeric, so it is not a `p.names` read.
        if !next.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            n += 1;
        }
        from = at + pat.len();
    }
    n
}

#[test]
fn put_refuses_a_names_array_before_writing_anything() {
    let text = strip_line_comments(&read(&doc_rs()));
    let body = fn_body(&text, "async fn handle_put(");
    assert!(
        body.len() > 1000,
        "handle_put body looks wrong ({} bytes) — the scan broke, not the function",
        body.len()
    );

    let check = body.find("p.names.is_empty()").expect(
        "handle_put must test `names` — without it a batch put silently discards documents",
    );

    // ORDERING: the refusal must precede the first thing that can write. `do_write` is the Atelier
    // PUT; `execute_via_generator` PUTs and compiles a scratch class. Either one before the check
    // would mean content had already moved.
    for writer in ["do_write", "execute_via_generator"] {
        if let Some(at) = body.find(writer) {
            assert!(
                check < at,
                "the `names` refusal is at byte {check} but `{writer}` appears at {at} — a refusal \
                 after the write is not a refusal"
            );
        }
    }
    // It must be a refusal, not a log line: an error envelope between the check and the next 30 lines.
    let window = &body[check..(check + 900).min(body.len())];
    assert!(
        window.contains("INVALID_PARAMS"),
        "the `names` check must return an error envelope; found none near it:\n{window}"
    );
    eprintln!("handle_put refuses names at byte {check}, before any write");
}

#[test]
fn only_get_and_delete_read_the_names_array() {
    // The measurement this fix rests on, re-taken every run so it cannot rot into a comment.
    let text = strip_line_comments(&read(&doc_rs()));
    let get = p_names_reads(&fn_body(&text, "async fn handle_get("));
    let del = p_names_reads(&fn_body(&text, "async fn handle_delete("));
    let put = p_names_reads(&fn_body(&text, "async fn handle_put("));

    // CONTROL FIRST: the counter finds reads where they exist. Without this, `put == 1` below would
    // be satisfied by a counter that matches nothing but the one line we just added.
    assert!(
        get >= 3 && del >= 1,
        "the counter found get={get} delete={del} — it is broken, so put={put} proves nothing"
    );
    // put's reads must ALL sit inside the refusal block. Asserting a count was wrong: the refusal
    // legitimately reads `names` three times (`is_empty`, `len` for the message, and the payload
    // echo). What matters is that nothing reads it AFTER the refusal — a later read would mean
    // something still consumes the array, which is the silent-discard path returning.
    let body = fn_body(&text, "async fn handle_put(");
    let check = body
        .find("p.names.is_empty()")
        .expect("the refusal must exist");
    // BRACE-MATCHED, not a byte fudge. The first version used `check + 900`, and a mutation
    // inserting a `p.names.len()` read immediately after the refusal SURVIVED, because the injected
    // read landed inside that arbitrary window. A window wider than the claim is the recurring way a
    // guard in this repo passes on text it was never asserting about.
    let brace = check
        + body[check..]
            .find('{')
            .expect("the refusal must be a block");
    let after = &body[end_of_block(&body, brace)..];
    assert_eq!(
        p_names_reads(after),
        0,
        "handle_put reads `names` after the refusal block, so something still consumes it:\n{}",
        &after[..400.min(after.len())]
    );
    assert!(
        put >= 1,
        "handle_put must read `names` at least once — the refusal itself"
    );
    eprintln!("p.names reads — get:{get} delete:{del} put:{put} (put's one is the refusal)");
}

#[test]
fn the_counter_rejects_namespace() {
    // Positive AND negative control for the word-boundary logic, which a plain substring search got
    // wrong: `p.namespace` contains `p.names`.
    assert_eq!(p_names_reads("if !p.names.is_empty() {"), 1);
    assert_eq!(p_names_reads("for name in &p.names {"), 1);
    assert_eq!(
        p_names_reads("resolve_namespace(p.namespace.as_deref())"),
        0
    );
    assert_eq!(
        p_names_reads("let ns = p.namespace.clone(); p.names.len()"),
        1
    );
}

#[test]
fn no_unrestricted_batch_claim_in_the_description() {
    // The two halves of the advertised contract must agree. The field's own doc comment restricts
    // `names` to get/delete; the description used to promise batch with no restriction at all.
    let text = read(&mod_rs());
    let claim = text
        .match_indices("'names' array")
        .map(|(i, _)| {
            let start = i.saturating_sub(160);
            text[start..(i + 160).min(text.len())].to_string()
        })
        .collect::<Vec<_>>();
    assert!(
        !claim.is_empty(),
        "the description no longer mentions the `names` array at all — if it was removed on purpose \
         this guard needs rewriting, but silence is worse than the overclaim it replaced"
    );
    let mentions_modes = claim
        .iter()
        .any(|c| c.contains("get") && c.contains("delete"));
    assert!(
        mentions_modes,
        "the description must say WHICH modes accept `names` (get and delete). Found:\n{claim:#?}"
    );
    let overclaims = claim
        .iter()
        .any(|c| c.contains("Supports batch ops via 'names' array and"));
    assert!(
        !overclaims,
        "the description claims batch with no mode restriction, contradicting the `names` field's \
         own schema doc:\n{claim:#?}"
    );
}
