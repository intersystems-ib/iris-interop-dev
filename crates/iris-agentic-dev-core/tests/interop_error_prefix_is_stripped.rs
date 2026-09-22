//! #329 item 3, Group A: the `ERROR:INTEROP_ERROR:` marker must not reach the caller's message.
//!
//! Two autostart sites passed their generator's raw output straight into the error envelope. Their
//! programs write `ERROR:INTEROP_ERROR:"_$System.Status.GetErrorText(tSC)`, so the caller received
//!
//! ```text
//! error: "ERROR:INTEROP_ERROR:Cannot open production Foo"
//! ```
//!
//! — the wire marker inside the human-readable field. `interop_fail` (`interop.rs:893`) already
//! strips it and classifies by content, and twelve other call sites already used it; these two did
//! not. Sibling divergence, not a missing feature.
//!
//! ## Scope: Group A only
//!
//! The six bypass sites are three mechanisms, and only these two leak a prefix:
//!
//! * **A** (`SetAutoStart` disable / enable) — programs DO write `ERROR:INTEROP_ERROR:`, so the
//!   prefix reaches the message. Fixed here.
//! * **B** — measured: those programs write no `ERROR:` codes and do not interpolate the prologue,
//!   so their catch-all receives unstructured output. Routing them would be enrichment, and could
//!   CHANGE the emitted code via content classification — it needs a test per site, not a blanket
//!   substitution.
//! * **C** — already `strip_prefix`-matched, so the code and message are already right.

use iris_agentic_dev_core::tools::interop::classify_interop_failure;
use std::path::PathBuf;

fn interop_rs() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("tools");
    p.push("interop.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — this guard must fail, not skip",
            p.display()
        )
    })
}

fn code_only(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_marker_prefix_never_reaches_the_message() {
    let raw = "ERROR:INTEROP_ERROR:Cannot open production Foo";
    let f = classify_interop_failure(raw, None);
    assert!(
        !f.message.contains("ERROR:INTEROP_ERROR:"),
        "the wire marker must be stripped from the human-readable message: {:?}",
        f.message
    );
    assert_eq!(
        f.message, "Cannot open production Foo",
        "the message should be exactly the text after the marker: {:?}",
        f.message
    );
    // CONTROL: the raw form really does carry the prefix, so the assertion above is about
    // stripping rather than about a string that was never there.
    assert!(
        raw.contains("ERROR:INTEROP_ERROR:"),
        "the fixture must contain what we claim is stripped"
    );
}

#[test]
fn the_production_is_named_when_it_is_known() {
    // The enable path knows which production it was pointed at; the disable path does not.
    let f = classify_interop_failure("ERROR:INTEROP_ERROR:boom", Some("NightLab.Production"));
    assert_eq!(
        f.extra.get("production").and_then(|v| v.as_str()),
        Some("NightLab.Production"),
        "a known production must be named in the envelope: {:?}",
        f.extra
    );
    let g = classify_interop_failure("ERROR:INTEROP_ERROR:boom", None);
    assert!(
        g.extra.get("production").is_none(),
        "an unknown production must be ABSENT, not an empty string: {:?}",
        g.extra
    );
}

#[test]
fn neither_autostart_site_still_passes_raw_output_through() {
    let text = code_only(&interop_rs());
    let bare = text
        .matches(r#"err_json("INTEROP_ERROR", out.trim())"#)
        .count();
    let routed = text.matches("interop_fail(out.trim()").count();
    // CONTROL FIRST: the scan finds the routed form. If it found neither, `bare == 0` would be
    // satisfied by a broken search and would read exactly like a fixed tree.
    assert_eq!(
        routed, 2,
        "expected both autostart sites routed through interop_fail, found {routed}"
    );
    assert_eq!(
        bare, 0,
        "an autostart site still passes the generator's raw output into the message, prefix and all"
    );
    eprintln!("autostart sites routed: {routed}, bare: {bare}");
}

#[test]
fn the_scan_ignores_a_comment_naming_the_old_form() {
    // The comments introducing this fix quote the very call they replaced — the false witness that
    // has broken guards in this repo three times.
    let sample = "// was: err_json(\"INTEROP_ERROR\", out.trim())\n\
                  Ok(out) => interop_fail(out.trim(), None),";
    let out = code_only(sample);
    assert_eq!(
        out.matches(r#"err_json("INTEROP_ERROR", out.trim())"#)
            .count(),
        0,
        "{out}"
    );
    assert_eq!(out.matches("interop_fail(out.trim()").count(), 1, "{out}");
}
