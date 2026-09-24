//! #385: `iris_gateway_manage(action=probe)` answered `GATEWAY_OK` for a connection that does not
//! exist, because it accepted a `connection` argument it never reads.
//!
//! Measured 2026-09-24 against an instance with no SQL Gateway connections defined:
//!
//! ```text
//! action=probe,  connection="ZzNoSuchGateway"  -> isError false, success true, diagnosis GATEWAY_OK
//!                                                 (no `connection` key anywhere in the reply)
//! action=test,   connection="ZzNoSuchGateway"  -> isError true,  GATEWAY_CONNECTION_NOT_DEFINED
//! action=delete, connection="ZzNoSuchGateway"  -> isError true,  GATEWAY_CONNECTION_NOT_DEFINED
//! ```
//!
//! The probe PROGRAM is right to ignore connections — `the_probe_program_reads_the_java_side_and_no_connection`
//! asserts it deliberately, and "a probe is about the gateway, not about a target". The defect is
//! at the parameter-acceptance layer: the instance-level answer was true, but it answered a
//! different question than the one asked, and the value it returned (`GATEWAY_OK`, `success:
//! true`) is exactly what a caller was hoping to hear about their connection.

use iris_agentic_dev_core::tools::gateway_manage::probe_ignores_connection;

const GW_SRC: &str = include_str!("../src/tools/gateway_manage.rs");

fn without_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_connection_passed_to_probe_is_refused_not_discarded() {
    let msg = probe_ignores_connection(Some("ZzNoSuchGateway"))
        .expect("probe must refuse a connection it never reads");
    // Both clauses are asserted separately and by their exact wording. A loose
    // `contains("ZzNoSuchGateway")` was satisfied by the REMEDY sentence's interpolation, so a
    // mutant that stopped naming the ignored connection in the sentence that reports it survived.
    assert!(
        msg.contains("'ZzNoSuchGateway' was NOT checked"),
        "the sentence reporting the ignored argument must name it: {msg}"
    );
    assert!(
        msg.contains("connection='ZzNoSuchGateway'"),
        "the remedy must carry the name into the call that does read it: {msg}"
    );
    assert!(
        msg.contains("action=test"),
        "the remedy is the action that DOES take a connection: {msg}"
    );
    assert!(
        msg.contains("action=list"),
        "the caller also needs the way to see which names exist: {msg}"
    );
}

#[test]
fn nothing_passed_means_nothing_to_refuse() {
    assert_eq!(
        probe_ignores_connection(None),
        None,
        "a plain probe is the normal call and must still work"
    );
}

#[test]
fn a_blank_connection_discards_nothing_so_it_is_not_refused() {
    for blank in [Some(""), Some(" "), Some("\t")] {
        assert_eq!(
            probe_ignores_connection(blank),
            None,
            "{blank:?} names no connection, so nothing was ignored"
        );
    }
}

#[test]
fn the_refusal_runs_before_the_probe_is_executed() {
    // A refusal that fires after the program has run is not a refusal, it is a second opinion.
    let src = without_comments(GW_SRC);
    let arm = src
        .find("Some(Action::Probe) => {")
        .expect("the Probe arm must still exist");
    let next_arm = src[arm..]
        .find("Some(Action::List)")
        .expect("the List arm follows Probe");
    let body = &src[arm..arm + next_arm];
    let check = body
        .find("probe_ignores_connection(")
        .expect("the Probe arm must consult the check");
    let run = body
        .find("execute_via_generator")
        .expect("the Probe arm must still run the probe program");
    assert!(
        check < run,
        "the argument must be refused before the probe runs, not after:\n{body}"
    );
}

#[test]
fn only_probe_refuses_a_connection() {
    // test, create and delete all legitimately take one; the refusal must not leak into them.
    let src = without_comments(GW_SRC);
    let handler = src
        .find("pub async fn handle_gateway_manage(")
        .expect("handler");
    let calls = src[handler..].matches("probe_ignores_connection(").count();
    assert_eq!(
        calls, 1,
        "exactly one action refuses a connection, and it is probe"
    );
}
