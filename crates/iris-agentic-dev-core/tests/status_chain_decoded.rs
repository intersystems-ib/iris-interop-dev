//! #323: BOTH `iris_execute` transports decode the `%Status` chain they hand back.
//!
//! The behaviour is unit-tested in `tools::status_chain_attachment_tests` (the payload rules) and in
//! `status::tests` (the parser). What neither can see is whether a transport CALLS it: the two
//! `Ok(Ok(output))` arms of `iris_execute` build near-identical payloads, and the docker one was
//! already missing the attachment once while the HTTP one had it — the #105 shape, where a second
//! copy of a path quietly keeps the old behaviour and looks more trustworthy for having a sibling
//! that works.
//!
//! Reaching the docker arm needs `IRIS_CONTAINER` and a container, so no test in this suite can
//! drive it. This reads the source instead, and the window it reads is exactly the claim: between
//! building that transport's payload and returning it.

use std::path::Path;

fn src() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/mod.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The text between `"method": "<transport>",` — the line that identifies which arm this is — and
/// the `ok_json(resp)` that returns it. `None` when the transport label is not there at all, which
/// is what makes a vacuous pass impossible.
fn arm<'a>(src: &'a str, transport: &str) -> Option<&'a str> {
    let label = format!("\"method\": \"{transport}\",");
    let start = src.find(&label)?;
    let rest = &src[start..];
    let end = rest.find("ok_json(resp)")?;
    Some(&rest[..end])
}

#[test]
fn both_execute_transports_decode_the_status_chain() {
    let s = src();
    for transport in ["http", "docker"] {
        let arm = arm(&s, transport).unwrap_or_else(|| {
            panic!(
                "iris_execute has no `\"method\": \"{transport}\"` payload followed by \
                 ok_json(resp) — either the arm moved or this guard is reading the wrong thing"
            )
        });
        assert!(
            arm.contains("attach_status_chain(&mut resp"),
            "the {transport} arm of iris_execute builds its payload and returns it without \
             decoding the %Status chain in `output` (#323). The model hand-writes that decode on \
             every call — 785 of them across 364 transcripts. Call attach_status_chain, which both \
             arms must share so the same call cannot come back structured over one transport and \
             unstructured over the other."
        );
    }
}

/// CONTROL for the locator: a transport that does not exist must find no arm. Without this, an
/// `arm()` that silently returned the whole file would pass the guard above for any label.
#[test]
fn the_arm_locator_finds_nothing_for_a_transport_that_does_not_exist() {
    let s = src();
    assert!(
        arm(&s, "carrier-pigeon").is_none(),
        "the locator matched a fabricated transport, so it is not reading the arm it claims to"
    );
    // And the two real ones are found, so the negative above is not a locator that finds nothing.
    assert!(arm(&s, "http").is_some());
    assert!(arm(&s, "docker").is_some());
}

/// The window must be the arm and not the whole file: a call in the HTTP arm must not satisfy the
/// docker one. Pinned by length — both windows are a fraction of the file — because a locator whose
/// `end` search failed and fell through to the file's end would pass every assertion above.
#[test]
fn each_window_is_one_arm_not_the_whole_file() {
    let s = src();
    for transport in ["http", "docker"] {
        let w = arm(&s, transport).expect("an arm");
        assert!(
            w.len() < s.len() / 4,
            "the {transport} window is {} of {} bytes — that is not one match arm, so the guard \
             is not measuring what it claims",
            w.len(),
            s.len()
        );
    }
    // The two windows must be DIFFERENT text, or one arm is being checked twice.
    assert_ne!(arm(&s, "http"), arm(&s, "docker"));
}
