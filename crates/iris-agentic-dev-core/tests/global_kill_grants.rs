//! #532, the deletion flow: what a token is allowed to mean, and the four ways it can fail.
//!
//! The property under test throughout: **nothing is deleted unless this call can show that what
//! is there now is what the preview showed.** Every refusal below therefore asserts the refusal
//! AND says so in its message, because a caller who cannot tell "I refused" from "I partly did
//! it" has to go and look at the data either way.

use iris_agentic_dev_core::elicitation::{GrantLookup, KillGrantStore};
use iris_agentic_dev_core::tools::global::{
    grant_refusal, kill_outcome, node_fingerprint, parse_kill_output, preview_incomplete, reverify,
    subscript_mismatch, target_mismatch, NodeData, NodeReport, WalkEnd, WalkPage, GLOBAL_CHANGED,
    GLOBAL_CONFIRM_CONSUMED, GLOBAL_CONFIRM_EXPIRED, GLOBAL_CONFIRM_NOT_FOUND,
    GLOBAL_CONFIRM_TARGET_MISMATCH, GLOBAL_PREVIEW_NODE_CAP, GLOBAL_PREVIEW_TIME_LIMIT,
    GLOBAL_REVERIFY_INCOMPLETE, KILL_DID_NOT_LAND,
};

fn node(name: &str, data: NodeData, value: Option<&str>) -> NodeReport {
    NodeReport {
        name: name.to_string(),
        data,
        value: value.map(str::to_string),
    }
}

fn page(nodes: Vec<NodeReport>, end: WalkEnd) -> WalkPage {
    WalkPage {
        resume_after: nodes.last().map(|n| n.name.clone()),
        nodes,
        end,
    }
}

fn sample() -> Vec<NodeReport> {
    vec![
        node("^IIDTest(\"a\")", NodeData::ValueOnly, Some("one")),
        node("^IIDTest(\"b\")", NodeData::ValueAndChildren, Some("two")),
        node("^IIDTest(\"b\",1)", NodeData::ValueOnly, Some("three")),
    ]
}

// ─── the store's four states ─────────────────────────────────────────────────────────────────

/// A redeemed token must NOT read as a missing one. "You already did this" and "that id was never
/// here" send a caller to opposite next actions, and for a deletion the difference is whether
/// they go looking for who re-created the data.
#[test]
fn a_redeemed_token_is_consumed_not_missing() {
    let store = KillGrantStore::new();
    let id = store.insert("APP", "IIDTest", vec![], vec!["f1".into()]);

    assert!(store.lookup(&id).is_found(), "a fresh grant is live");
    store.consume(&id);

    match store.lookup(&id) {
        GrantLookup::Consumed => {}
        other => panic!("a redeemed grant must report Consumed, got {other:?}"),
    }
    match store.lookup("a-token-never-issued") {
        GrantLookup::NotFound => {}
        other => panic!("an unknown id must report NotFound, got {other:?}"),
    }

    let (consumed_code, consumed_msg) = grant_refusal(&store.lookup(&id)).expect("refuses");
    let (missing_code, _) = grant_refusal(&store.lookup("nope")).expect("refuses");
    assert_eq!(consumed_code, GLOBAL_CONFIRM_CONSUMED);
    assert_eq!(missing_code, GLOBAL_CONFIRM_NOT_FOUND);
    assert_ne!(
        consumed_code, missing_code,
        "without a tombstone these collapse, which is upstream's behaviour and tells a caller to \
         check for a typo when the fact is that the kill already ran"
    );
    assert!(
        consumed_msg.contains("did NOTHING"),
        "a second redemption must say plainly that this call changed nothing: {consumed_msg}"
    );
}

/// A grant is spent once. The count is the observable: a consumed grant is gone from the live map,
/// not merely marked.
#[test]
fn consuming_a_grant_removes_it_from_the_live_set() {
    let store = KillGrantStore::new();
    let a = store.insert("APP", "IIDTest", vec![], vec![]);
    let b = store.insert("APP", "Other", vec![], vec![]);
    assert_eq!(store.live_count(), 2);
    store.consume(&a);
    assert_eq!(store.live_count(), 1, "only the redeemed one goes");
    assert!(store.lookup(&b).is_found(), "the other grant is untouched");
}

/// Every refusal carries its own code, and none of them is the success path.
#[test]
fn the_three_token_failures_are_three_codes() {
    let store = KillGrantStore::new();
    let live = store.insert("APP", "IIDTest", vec![], vec![]);
    assert!(
        grant_refusal(&store.lookup(&live)).is_none(),
        "a live grant is not a refusal"
    );

    let spent = store.insert("APP", "IIDTest", vec![], vec![]);
    store.consume(&spent);

    let codes: Vec<&str> = [store.lookup(&spent), store.lookup("absent")]
        .iter()
        .filter_map(|l| grant_refusal(l).map(|(c, _)| c))
        .collect();
    assert_eq!(
        codes,
        vec![GLOBAL_CONFIRM_CONSUMED, GLOBAL_CONFIRM_NOT_FOUND]
    );
    // GLOBAL_CONFIRM_EXPIRED is reachable only through the clock, so it is pinned by its message
    // being distinct rather than by forcing a five-minute wait here.
    assert_ne!(GLOBAL_CONFIRM_EXPIRED, GLOBAL_CONFIRM_NOT_FOUND);
    assert_ne!(GLOBAL_CONFIRM_EXPIRED, GLOBAL_CONFIRM_CONSUMED);
}

// ─── a token binds a target ──────────────────────────────────────────────────────────────────

/// Upstream binds `{global, server}`. The namespace is in here because the same global name in
/// two namespaces is two different sets of data.
#[test]
fn a_token_is_bound_to_its_namespace_as_well_as_its_global() {
    assert!(
        target_mismatch("APP", "IIDTest", "APP", "IIDTest").is_none(),
        "the matching case must pass, or every mismatch below proves nothing"
    );
    let (code, msg) = target_mismatch("APP", "IIDTest", "USER", "IIDTest")
        .expect("the same global in another namespace is another target");
    assert_eq!(code, GLOBAL_CONFIRM_TARGET_MISMATCH);
    assert!(msg.contains("APP") && msg.contains("USER"), "{msg}");
    assert!(msg.contains("Nothing was deleted"), "{msg}");

    assert!(
        target_mismatch("APP", "IIDTest", "APP", "Other").is_some(),
        "a different global is a different target"
    );
}

/// The depth matters: a token for `^X("a")` must not delete `^X`, which is its parent and
/// strictly more data.
#[test]
fn a_token_does_not_cover_a_different_subscript_depth() {
    let granted = vec!["\"a\"".to_string()];
    assert!(
        subscript_mismatch(&granted, &granted, "IIDTest").is_none(),
        "the matching case must pass"
    );
    let (code, msg) = subscript_mismatch(&granted, &[], "IIDTest")
        .expect("a token for a child must not authorise the parent");
    assert_eq!(code, GLOBAL_CONFIRM_TARGET_MISMATCH);
    assert!(msg.contains("Nothing was deleted"), "{msg}");

    assert!(
        subscript_mismatch(&granted, &["\"b\"".to_string()], "IIDTest").is_some(),
        "a sibling is a different subtree"
    );
    assert!(
        subscript_mismatch(&granted, &["\"a\"".to_string(), "1".to_string()], "IIDTest").is_some(),
        "a deeper node is a different subtree"
    );
}

// ─── the re-verification ─────────────────────────────────────────────────────────────────────

/// The happy path, which every assertion below depends on being reachable.
#[test]
fn an_unchanged_subtree_verifies() {
    let granted: Vec<String> = sample().iter().map(node_fingerprint).collect();
    reverify(&granted, &page(sample(), WalkEnd::Complete)).expect("an identical subtree verifies");
}

/// Every way a subtree can differ is a refusal, and the message names where.
#[test]
fn any_difference_from_the_preview_refuses_the_kill() {
    let granted: Vec<String> = sample().iter().map(node_fingerprint).collect();

    let mut changed_value = sample();
    changed_value[1].value = Some("CHANGED".into());

    let mut changed_data = sample();
    changed_data[0].data = NodeData::ValueAndChildren;

    let mut one_more = sample();
    one_more.push(node("^IIDTest(\"c\")", NodeData::ValueOnly, Some("new")));

    let mut one_fewer = sample();
    one_fewer.pop();

    for (label, nodes) in [
        ("a value changed", changed_value),
        ("a node gained children", changed_data),
        ("a node was added", one_more),
        ("a node was removed", one_fewer),
    ] {
        let (code, msg) = reverify(&granted, &page(nodes, WalkEnd::Complete))
            .expect_err(&format!("{label} must refuse the kill"));
        assert_eq!(code, GLOBAL_CHANGED, "{label}");
        assert!(
            msg.contains("NOTHING was deleted"),
            "{label}: the refusal must say nothing happened: {msg}"
        );
    }
}

/// The distinction the whole module exists for: "it changed" and "I could not tell" are different
/// answers. An incomplete re-walk must never report GLOBAL_CHANGED, because that claims knowledge
/// of the current state that this call does not have.
#[test]
fn an_unfinished_recheck_is_unknown_not_changed() {
    let granted: Vec<String> = sample().iter().map(node_fingerprint).collect();
    for end in [WalkEnd::NodeCap, WalkEnd::TimeLimit] {
        // Identical content, but the walk did not finish — so the nodes beyond it are unknown.
        let (code, msg) = reverify(&granted, &page(sample(), end))
            .expect_err("an incomplete re-check cannot authorise a deletion");
        assert_eq!(
            code, GLOBAL_REVERIFY_INCOMPLETE,
            "an unfinished walk must not report GLOBAL_CHANGED: the content matched as far as it \
             got, and claiming it changed would be as wrong as claiming it had not"
        );
        assert!(msg.contains("UNKNOWN"), "{msg}");
        assert!(
            msg.contains("NOT a report that it changed"),
            "the message must rule out the reading it would otherwise invite: {msg}"
        );
    }
}

/// The fingerprint has to distinguish nodes that a naive join would merge.
#[test]
fn a_fingerprint_cannot_be_forged_by_a_value_containing_the_separator() {
    let a = node("^X(\"a\")", NodeData::ValueOnly, Some("1|2"));
    let b = node("^X(\"a\")", NodeData::ValueOnly, Some("1"));
    assert_ne!(
        node_fingerprint(&a),
        node_fingerprint(&b),
        "a value holding the separator must not collide with a shorter one"
    );
    // The empty value and no value at all are different nodes.
    let empty = node("^X(\"a\")", NodeData::ValueOnly, Some(""));
    let none = node("^X(\"a\")", NodeData::ChildrenOnly, None);
    assert_ne!(node_fingerprint(&empty), node_fingerprint(&none));
    // Identical nodes fingerprint identically, or nothing ever verifies.
    assert_eq!(
        node_fingerprint(&b),
        node_fingerprint(&node("^X(\"a\")", NodeData::ValueOnly, Some("1")))
    );
}

// ─── the preview refuses to over-promise ─────────────────────────────────────────────────────

/// Two codes, not one, because raising max_nodes fixes the first and cannot fix the second.
#[test]
fn an_incomplete_preview_mints_nothing_and_says_which_bound_stopped_it() {
    let (cap_code, cap_msg) = preview_incomplete(&WalkEnd::NodeCap, 100, 100);
    assert_eq!(cap_code, GLOBAL_PREVIEW_NODE_CAP);
    assert!(cap_msg.contains("Raise max_nodes"), "{cap_msg}");

    let (time_code, time_msg) = preview_incomplete(&WalkEnd::TimeLimit, 57, 100);
    assert_eq!(time_code, GLOBAL_PREVIEW_TIME_LIMIT);
    assert!(
        time_msg.contains("will NOT help"),
        "the time refusal must rule out the remedy for the other one: {time_msg}"
    );
    assert_ne!(
        cap_code, time_code,
        "collapsing these sends half the callers to a remedy that cannot work"
    );
    for m in [&cap_msg, &time_msg] {
        assert!(
            m.contains("no deletion token") || m.contains("NO deletion token"),
            "an incomplete preview must say no token was issued: {m}"
        );
    }
}

// ─── the kill proves itself ──────────────────────────────────────────────────────────────────

#[test]
fn a_kill_is_verified_by_reading_data_back_and_zero_is_the_only_success() {
    let ok = kill_outcome(NodeData::Absent, "APP", "IIDTest", 3).expect("$DATA 0 is success");
    assert_eq!(ok["removed_nodes"], serde_json::json!(3));
    assert_eq!(ok["remaining_data"], serde_json::json!(0));

    for leftover in [
        NodeData::ValueOnly,
        NodeData::ChildrenOnly,
        NodeData::ValueAndChildren,
    ] {
        let (code, msg) = kill_outcome(leftover, "APP", "IIDTest", 3)
            .expect_err("anything still there is not a completed deletion");
        assert_eq!(code, KILL_DID_NOT_LAND);
        assert!(
            msg.contains("not a successful deletion"),
            "{leftover:?}: {msg}"
        );
    }
}

#[test]
fn a_kill_reply_that_cannot_be_read_is_an_error_not_a_deletion() {
    assert!(
        parse_kill_output("1:0\n").is_ok(),
        "the well-formed case must parse, or the failures below prove nothing"
    );
    let err = parse_kill_output("").expect_err("an empty reply says nothing about the subtree");
    assert!(err.contains("UNKNOWN") || err.contains("1 field"), "{err}");
    let err2 = parse_kill_output("1:7\n").expect_err("7 is not a $DATA value");
    assert!(err2.contains('7'), "{err2}");
}
