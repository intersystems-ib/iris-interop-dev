//! #417 item 1 / #418 item 6: `iris_source_control` is advertised in the interop profile, and
//! every action of it that changes state goes through the write gate.
//!
//! These two facts are ONE change and cannot be split. `every_interop_tool_is_classified` already
//! fails if a keep-list entry is missing from `mutating_call`'s arms, so adding the tool without
//! the gate does not compile-and-pass — it goes red. That coupling is deliberate: the tool can
//! `%CheckIn`, `%GetLatest` and `%Disconnect`, and shipping it ungated in the DEFAULT profile would
//! mean those run on a connection the caller asked to be read-only.
//!
//! What is NOT asserted here: that the SCM hooks behave correctly against a live instance. That
//! needs a CCR-configured IRIS and belongs in the e2e job. These tests pin the SURFACE — what is
//! advertised, and what the gate says about each action — which is the half that can be wrong
//! silently.

use iris_agentic_dev_core::tools::scm::ScmAction;
use iris_agentic_dev_core::tools::INTEROP_TOOLS;

/// The keep-list is the source of truth for the interop profile, so this is what "visible in the
/// MCP" means mechanically.
#[test]
fn iris_source_control_is_in_the_interop_profile() {
    assert!(
        INTEROP_TOOLS.contains(&"iris_source_control"),
        "iris_source_control is not advertised, so no caller can reach the SCM hooks: {INTEROP_TOOLS:?}"
    );
    // CONTROL: the list was really read, and this assertion is not passing against an empty one.
    assert!(
        INTEROP_TOOLS.len() > 20 && INTEROP_TOOLS.contains(&"iris_doc"),
        "INTEROP_TOOLS looks wrong ({} entries) — the check above proves nothing",
        INTEROP_TOOLS.len()
    );
}

/// The classification lives on `ScmAction`, exhaustively. This is the table the write gate reads.
#[test]
fn every_scm_action_is_classified_and_the_writes_are_writes() {
    for (id, want_write) in [
        ("CheckOut", true),
        ("UndoCheckout", true),
        ("CheckIn", true),
        ("GetLatest", true),
        ("AddToSourceControl", true),
        ("Disconnect", true),
        ("Reconnect", true),
        ("Diff", false),
    ] {
        let a = ScmAction::from_id(id);
        assert!(
            !matches!(a, ScmAction::Unknown(_)),
            "'{id}' did not parse to a known action, so the gate would treat it as Unknown"
        );
        assert_eq!(
            a.is_write(),
            want_write,
            "{id}: is_write() = {}, expected {want_write}",
            a.is_write()
        );
    }
}

/// `%`-prefixed ids are what IRIS actually sends, and they must classify the same.
#[test]
fn the_percent_prefix_does_not_change_the_verdict() {
    for id in ["CheckIn", "GetLatest", "Disconnect", "Diff"] {
        assert_eq!(
            ScmAction::from_id(id).is_write(),
            ScmAction::from_id(&format!("%{id}")).is_write(),
            "'{id}' and '%{id}' classify differently, so the gate depends on how IRIS spelled it"
        );
    }
}

/// An id this fork does not know names a method on the SERVER's own %Studio.SourceControl
/// subclass, which is site-written and can do anything. The affordable error is refusing a read;
/// the unaffordable one is performing an unnamed write on a read-only connection.
#[test]
fn an_unknown_action_is_treated_as_a_write() {
    for id in ["", "SomeSiteHook", "%Zzz", "CheckOutAll"] {
        let a = ScmAction::from_id(id);
        assert!(
            matches!(a, ScmAction::Unknown(_)),
            "'{id}' unexpectedly parsed to a known action: {a:?}"
        );
        assert!(
            a.is_write(),
            "an unrecognised action id ('{id}') is not gated, so a site hook of unknown effect \
             runs on a connection the caller asked to be read-only"
        );
    }
}

/// `Diff` is the one read among the execute actions, and it must stay readable — a blanket
/// mutating verdict for the tool would take it, and the status/menu path, with it.
#[test]
fn diff_is_not_gated() {
    assert!(
        !ScmAction::from_id("Diff").is_write(),
        "Diff only compares two versions; gating it removes the read half of the tool"
    );
    // CONTROL: is_write() does not simply return false for everything.
    assert!(
        ScmAction::from_id("CheckIn").is_write(),
        "is_write() returns false for CheckIn too, so the assertion above is vacuous"
    );
}

/// The classification must be DERIVED from `ScmAction::is_write`, not restated as a second
/// `matches!` over action strings in `mutating_call`. A duplicate is the live hazard: an action
/// added to the enum and dispatched, but missing from the copy, is an ungated write.
///
/// Asserted at the source, because that is where the duplication would be. The window is the one
/// match arm.
#[test]
fn the_write_gate_derives_from_the_enum_rather_than_restating_it() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools/mod.rs");
    let src = std::fs::read_to_string(&p).expect("mod.rs");

    let at = src
        .find("\"iris_source_control\" => match action {")
        .expect("the iris_source_control arm must be findable in mutating_call");
    let rest = &src[at..];
    // to the end of that arm: the next arm at the same indentation
    let end = rest[1..]
        .find("\n        \"")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    let arm = &rest[..end];

    assert!(
        arm.contains("ScmAction::from_id") && arm.contains("is_write()"),
        "the arm does not derive from ScmAction::is_write:\n{arm}"
    );
    // And it must not ALSO restate the classification. A second `matches!` over the action ids
    // here is the live hazard: an action added to the enum and dispatched, but missing from the
    // copy, is an ungated write that both halves look fine about.
    assert!(
        !arm.contains("matches!"),
        "the arm restates the action classification instead of deriving it:\n{arm}"
    );
    for id in ["CheckIn", "GetLatest", "Disconnect", "AddToSourceControl"] {
        assert!(
            !arm.contains(id),
            "the arm names '{id}' directly, which duplicates ScmAction::is_write:\n{arm}"
        );
    }
    // The read actions stay read, the checkout is a write, and an unrecognised top-level action
    // is NOT allowed to fall through as read-only.
    assert!(
        arm.contains("\"status\" | \"menu\" => None"),
        "status/menu are not read-only in the gate, so the tool is unusable on a read-only \
         connection:\n{arm}"
    );
    assert!(
        arm.contains("\"checkout\" => Some("),
        "checkout is not gated, and it takes a lock in the SCM:\n{arm}"
    );
    assert!(
        !arm.contains("_ => None"),
        "an unrecognised top-level action falls through as read-only:\n{arm}"
    );

    // CONTROLS: the window is ONE arm of the match, and it is the right one.
    assert!(
        arm.len() < src.len() / 50,
        "the window is {} of {} bytes — not one match arm",
        arm.len(),
        src.len()
    );
    assert!(
        !arm.contains("\"iris_symbols\""),
        "the window ran past the end of the arm:\n{arm}"
    );
}

/// `ScmAction::is_write` must have no `_` arm, so a variant added to the enum cannot compile until
/// it is classified. Same construction as `DocMode::is_write` and `gateway_manage::Action::is_write`.
#[test]
fn is_write_has_no_catch_all_arm() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools/scm.rs");
    let src = std::fs::read_to_string(&p).expect("scm.rs");

    let at = src
        .find("pub fn is_write(&self) -> bool {")
        .expect("ScmAction::is_write must exist");
    let rest = &src[at..];
    let end = rest.find("\n    }\n").map(|i| i + 6).unwrap_or(rest.len());
    let body = &rest[..end];

    // Comments are stripped first: a comment naming the construct is the commonest false witness
    // in a guard like this, and one of these arms explains why Unknown is a write.
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    // Every arm pattern must name variants explicitly. `_ => ` is only ONE spelling of a
    // catch-all; `other => true` binds the same way and would pass a `_ =>` search, so the
    // assertion is on the SHAPE of each arm rather than on one forbidden string.
    let arms: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("=>"))
        .collect();
    assert!(
        !arms.is_empty(),
        "no match arms found in the window, so this test proves nothing:\n{code}"
    );
    for arm in &arms {
        // The write arm spans several lines (`Self::CheckOut\n | Self::UndoCheckout\n ... => true`),
        // so a line of it reads `| Self::Reconnect => true,` — the leading `|` must be stripped
        // too, and empty segments ignored, or the split yields "" and every multi-line arm looks
        // like a catch-all. That is what this test reported on its first run.
        let pat = arm.split("=>").next().unwrap().trim();
        let pat = pat.trim_start_matches('|').trim_end_matches('|').trim();
        assert!(
            pat.split('|')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .all(|p| p.starts_with("Self::")),
            "`{arm}` is a catch-all or a binding pattern, so a new ScmAction variant would \
             silently inherit a verdict instead of failing to compile"
        );
    }
    // CONTROLS: the window is the function body, and comments really were stripped.
    assert!(
        code.contains("Self::Diff => false"),
        "the window does not contain the body's own arms:\n{code}"
    );
    assert!(
        code.contains("Self::Unknown(_) => true"),
        "the Unknown arm is missing from the window:\n{code}"
    );
    assert!(
        !code.contains("affordable error"),
        "comments were not stripped, so the `_ =>` search above can match prose"
    );
}

/// The manifest and the keep-list must agree, because `scripts/validate-tools.sh` fails the build
/// when a tool is advertised but absent from `tools-status.json` — and that gate runs in CI, not
/// here, so a missing row would surface as a confusing shell failure instead of a test.
#[test]
fn the_manifest_carries_a_row_for_the_tool() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("tools-status.json");
    let raw = std::fs::read_to_string(&p).expect("tools-status.json");

    assert!(
        raw.contains("\"iris_source_control\""),
        "tools-status.json has no row for iris_source_control, which validate-tools.sh refuses"
    );
    // CONTROL: the file was read and is the manifest, not something else that happens to exist.
    assert!(
        raw.contains("\"iris_doc\"") && raw.contains("\"tools\""),
        "tools-status.json does not look like the manifest — the assertion above proves nothing"
    );
}
