//! A `rule_name` with no `action` must not be silently discarded.
//!
//! `iris_business_rule_info`'s action defaulted to `"list"` unconditionally, and the list path never
//! reads `rule_name`. Measured against a live instance before the fix:
//!
//! ```text
//! {rule_name: "No.Such.Rule"}                -> success: true, count: 0, rules: []
//! {action: "get", rule_name: "No.Such.Rule"} -> RULE_NOT_FOUND, "call action=list to see what is there"
//! ```
//!
//! The first reads as "your rule does not exist" when the tool never looked for it. On a namespace
//! that DOES hold rules it is worse: the caller gets all of them and may not notice their name was
//! ignored. The same shape `require_name` records in `doc.rs` (#327 — a supplied `names` array
//! discarded while `success: true` came back) and `item_name_arg` in #218.

use iris_agentic_dev_core::tools::interop::rule_action_for;

/// The defect: a supplied name with no action.
#[test]
fn a_rule_name_with_no_action_means_get() {
    assert_eq!(rule_action_for(None, Some("My.Rule")), "get");
    // Whitespace around it does not change what the caller meant.
    assert_eq!(rule_action_for(None, Some("  My.Rule  ")), "get");
    // An explicitly EMPTY action is the same as none given.
    assert_eq!(rule_action_for(Some(""), Some("My.Rule")), "get");
    assert_eq!(rule_action_for(Some("   "), Some("My.Rule")), "get");
}

/// With nothing named, `list` is still right — it is the action that needs no arguments.
#[test]
fn no_name_and_no_action_still_means_list() {
    assert_eq!(rule_action_for(None, None), "list");
    assert_eq!(rule_action_for(None, Some("")), "list");
    assert_eq!(rule_action_for(None, Some("   ")), "list");
}

/// An explicit action always wins, including `list` WITH a name — a caller who writes both has said
/// what they want, and second-guessing that would be a different silent override.
#[test]
fn an_explicit_action_is_never_overridden() {
    assert_eq!(rule_action_for(Some("list"), Some("My.Rule")), "list");
    assert_eq!(rule_action_for(Some("get"), Some("My.Rule")), "get");
    assert_eq!(rule_action_for(Some("get"), None), "get");
    // An unknown action is passed through unchanged, so the handler's own INVALID_ACTION arm
    // reports it with the valid set — this function must not swallow a typo into a default.
    assert_eq!(rule_action_for(Some("gett"), Some("My.Rule")), "gett");
}

/// The wiring, read at the source: the handler must route through this function rather than
/// defaulting to a literal. Driving it needs a live instance, and the defect WAS the default.
#[test]
fn the_handler_does_not_default_the_action_to_a_literal() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/mod.rs"),
    )
    .expect("mod.rs");
    let at = src
        .find("async fn iris_business_rule_info(")
        .expect("the handler");
    let rest = &src[at..];
    let body = &rest[..rest.find("\n    }\n").unwrap_or(rest.len())];
    assert!(
        body.contains("interop::rule_action_for("),
        "the handler defaults the action itself, so a supplied rule_name can be discarded again:\n{body}"
    );
    // Comments stripped first. The fix's own comment says `NOT unwrap_or("list")` to explain itself,
    // and the first version of this assertion flagged that — a comment naming the forbidden
    // construct is the commonest false witness in a source guard.
    let code_only: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code_only.contains("unwrap_or(\"list\")"),
        "this is the unconditional default that discarded the name:\n{code_only}"
    );
    // CONTROLS for the stripper: a comment mentioning it is not code, and real code still is.
    let stripped = |t: &str| -> String {
        t.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!stripped("    // NOT `unwrap_or(\"list\")` here").contains("unwrap_or"));
    assert!(stripped("    let a = x.unwrap_or(\"list\");").contains("unwrap_or(\"list\")"));
    // CONTROL: the window is the handler and it is the right one.
    assert!(body.contains("rule_name"), "wrong window: {body}");
    assert!(
        !body.contains("async fn iris_production_diff("),
        "the window ran past the handler"
    );
}
