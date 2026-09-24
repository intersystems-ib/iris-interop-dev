//! #384: `iris_credential_manage(create)` reported EVERY `SetCredential` failure as
//! CREDENTIAL_EXISTS, and never checked `id` at all.
//!
//! Both halves were measured against a live instance on 2026-09-24, driving the shipped
//! binary over MCP stdio. Two calls, one error code:
//!
//! | call | IRIS message | code returned |
//! |---|---|---|
//! | `create` twice with the same id | `ERROR <Ens>ErrGeneral: A credential with this ID already exists` | CREDENTIAL_EXISTS |
//! | `create` with `id=""` | `ERROR #5659: Property 'Ens.Config.Credentials::SystemName(77@…,ID=)' required` | CREDENTIAL_EXISTS |
//!
//! The second is the opposite of the truth — nothing existed — so a caller who believes it
//! reaches for `update` or `delete`, neither of which addresses the real cause.

use iris_agentic_dev_core::tools::interop::{build_credential_create_code, credential_id_refusal};

const INTEROP_SRC: &str = include_str!("../src/tools/interop.rs");
const MOD_SRC: &str = include_str!("../src/tools/mod.rs");

/// A comment naming a construct is the commonest false witness in a source check, so every
/// source-level assertion below reads code with the comments removed.
fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of one `fn`, so a source check cannot be satisfied by unrelated text elsewhere.
fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} is not in this file any more"));
    let rest = &src[start..];
    // The next function at the same nesting ends this one; for these two files the
    // impl/handler bodies are followed by another `    async fn` or `pub async fn`.
    let end = rest[signature.len()..]
        .find("\n    async fn ")
        .or_else(|| rest[signature.len()..].find("\npub async fn "))
        .or_else(|| rest[signature.len()..].find("\npub fn "))
        .map(|i| i + signature.len())
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn an_empty_id_is_refused_for_each_write_action() {
    for (action, verb) in [
        ("create", "Nothing was created."),
        ("update", "Nothing was updated."),
        ("delete", "Nothing was deleted."),
    ] {
        let msg = credential_id_refusal(action, "")
            .unwrap_or_else(|| panic!("action={action} with an empty id must be refused"));
        assert!(
            msg.contains(&format!("action={action}")),
            "the refusal must name the action it is about: {msg}"
        );
        assert!(
            msg.contains("'id'"),
            "the refusal must name the missing parameter: {msg}"
        );
        assert!(
            msg.contains(verb),
            "action={action} must say what did not happen ({verb}): {msg}"
        );
        assert!(
            msg.contains("iris_credential_list"),
            "the refusal must name the tool that lists the valid ids: {msg}"
        );
    }
}

#[test]
fn whitespace_alone_is_not_an_id() {
    for blank in ["", " ", "\t", "  \n "] {
        assert!(
            credential_id_refusal("create", blank).is_some(),
            "{blank:?} cannot name a credential"
        );
    }
}

#[test]
fn a_real_id_is_accepted_and_is_never_rewritten() {
    for id in ["ZzProbeCred", "My.Cred", " padded "] {
        assert_eq!(
            credential_id_refusal("create", id),
            None,
            "{id:?} is a usable id and must not be refused"
        );
    }
    // The helper only inspects; the id the caller sent is the id that reaches IRIS.
    let code = build_credential_create_code("\" padded \"", "\"u\"", "\"p\"");
    assert!(
        code.contains("\" padded \""),
        "the id must be passed through exactly as the caller sent it: {code}"
    );
}

#[test]
fn an_unknown_action_is_left_to_invalid_action() {
    // Naming the valid set of actions is more use than complaining about the id.
    for action in ["", "list", "frobnicate", "Create"] {
        assert_eq!(
            credential_id_refusal(action, "").as_deref(),
            None,
            "action={action:?} is answered by INVALID_ACTION, not by the id check"
        );
    }
}

#[test]
fn only_the_duplicate_arm_claims_credential_exists() {
    // The regression itself: CREDENTIAL_EXISTS used to sit on the $$$ISERR arm, which
    // catches every reason SetCredential can fail.
    let code = build_credential_create_code("\"C\"", "\"u\"", "\"p\"");
    let iserr = code
        .lines()
        .find(|l| l.contains("$$$ISERR"))
        .expect("create must still check the status of the write");
    assert!(
        !iserr.contains("CREDENTIAL_EXISTS"),
        "a failed write must not claim the credential already exists: {iserr}"
    );
    assert!(
        iserr.contains("ERROR:INTEROP_ERROR:"),
        "an unclassified failure belongs in the generic arm: {iserr}"
    );
    assert_eq!(
        code.matches("CREDENTIAL_EXISTS").count(),
        1,
        "exactly one arm — the duplicate precheck — may claim a duplicate:\n{code}"
    );
}

#[test]
fn the_duplicate_check_runs_before_the_write_and_returns() {
    let code = build_credential_create_code("\"C\"", "\"u\"", "\"p\"");
    let exists = code
        .find("%ExistsId")
        .expect("the duplicate condition must be proved structurally, not parsed from text");
    let write = code
        .find("SetCredential")
        .expect("create must still write the credential");
    assert!(
        exists < write,
        "checking after writing would report a duplicate it had already overwritten:\n{code}"
    );
    let precheck = code
        .lines()
        .find(|l| l.contains("%ExistsId"))
        .expect("precheck line");
    // `Quit` inside `If { }` returns from the generated method; without it the precheck
    // falls through and the existing credential is overwritten anyway.
    assert!(
        precheck.contains("Quit"),
        "the precheck must return instead of falling through to the write: {precheck}"
    );
}

#[test]
fn the_duplicate_refusal_names_a_remedy() {
    let code = build_credential_create_code("\"C\"", "\"u\"", "\"p\"");
    let precheck = code
        .lines()
        .find(|l| l.contains("CREDENTIAL_EXISTS"))
        .expect("precheck line");
    assert!(
        precheck.contains("action=update") && precheck.contains("action=delete"),
        "a caller told the id is taken needs the two ways forward: {precheck}"
    );
}

#[test]
fn the_id_is_validated_before_the_action_is_dispatched() {
    // The wiring, not just the helper: a check that is never called is worth nothing.
    let src = without_comments(INTEROP_SRC);
    let body = fn_body(&src, "pub async fn interop_credential_manage_impl(");
    let check = body
        .find("credential_id_refusal(")
        .expect("interop_credential_manage_impl must call the id check");
    let dispatch = body
        .find("match params.action.as_str()")
        .expect("the impl must still dispatch on the action");
    assert!(
        check < dispatch,
        "the id must be checked before any action runs, or one action at a time gets fixed"
    );
}

#[test]
fn a_missing_id_key_really_does_arrive_as_an_empty_string() {
    // Why the check tests for empty rather than for absent: `Described` deserialises
    // infallibly over any JSON object, and the handler defaults each field.
    let src = without_comments(MOD_SRC);
    let body = fn_body(&src, "    async fn iris_credential_manage(");
    assert!(
        body.contains(".get(\"id\")"),
        "the handler reads id out of a free-form JSON object"
    );
    assert!(
        body.contains(".unwrap_or(\"\")"),
        "an absent id becomes an empty string, which is what the check must catch"
    );
}

#[test]
fn neither_new_message_leaks_its_source_indentation() {
    // #378: a `\`-continued string literal keeps the newline out but the next line's indentation
    // is only stripped when the continuation is written correctly. A caller reads these verbatim.
    let mut msgs = vec![build_credential_create_code("\"C\"", "\"u\"", "\"p\"")];
    for action in ["create", "update", "delete"] {
        msgs.push(credential_id_refusal(action, "").expect("refused"));
    }
    for m in msgs {
        assert!(
            !m.contains("  "),
            "a run of spaces reached the caller-facing text: {m:?}"
        );
    }
}
