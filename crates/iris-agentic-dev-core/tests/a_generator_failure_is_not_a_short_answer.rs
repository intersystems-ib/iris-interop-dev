//! #437, part three: two `iris_admin` actions INTERPRETED the generator's failure text as data.
//!
//! The generator reports its own failures inside the captured output, in three shapes:
//!
//! | shape | written when |
//! |---|---|
//! | `ERROR: <SIGNAL> …` | the body threw and the Catch wrote the trap |
//! | `ERROR($ZERROR): <SIGNAL>` | the body aborted leaving no output, so `$ZERROR` is surfaced |
//! | `ERROR($DEVICE): …` | the body moved the current device, so the capture is short |
//!
//! Both call sites below decided with a hand-rolled prefix test, and **only the first shape has
//! a `:` directly after `ERROR`**. So the other two fell straight through into the parser:
//!
//! * `list_databases` split the line on `|` and returned it as a database — `success: true`,
//!   `count: 1`, `directory: "ERROR($ZERROR): <PROTECT> …"`, `mounted: false`, sizes `0.0`;
//! * `list_user_roles` ran `.split(',')` over it and returned the pieces as **role names**,
//!   `success: true`.
//!
//! A caller asking "which databases exist" or "what can this user do" got a confident answer
//! assembled from an error message. That is the defect class CLAUDE.md is about, and the repair
//! is the same one: a third enum case, so the compiler enumerates the call sites.
//!
//! #437 also made it worse before it made it better — the new `ERROR($DEVICE)` line is a fourth
//! way into the same hole, which is why these two are fixed in the same change rather than later.

use iris_agentic_dev_core::tools::admin::{
    parse_database_listing, parse_user_roles, DatabaseListing, UserRoles,
};

/// Every shape the generator can emit to say "this did not work". Any site that interprets
/// captured output has to reject all of them, and the second and third are the ones that get
/// missed because they do not match `starts_with("ERROR:")`.
const GENERATOR_FAILURES: [(&str, &str); 4] = [
    (
        "the Catch's trap",
        "ERROR: <PROTECT> 3 RunUser+1^IrisDevTmp.Run1.1 ^SYS",
    ),
    (
        "an abort with no output",
        "ERROR($ZERROR): <PROTECT>zList+4^SYS.Database.1 ^SYS",
    ),
    (
        "a lost capture",
        "ERROR($DEVICE): output capture lost - the code left the current device on /tmp/f.txt \
         instead of the capture file, so anything written after the switch was discarded",
    ),
    (
        "the redirect could not be established",
        "ERROR: output capture unavailable",
    ),
];

// ── iris_admin list_databases ────────────────────────────────────────────────────────────────

#[test]
fn no_generator_failure_is_parsed_as_a_database() {
    let mut rejected = 0;
    for (what, out) in GENERATOR_FAILURES {
        match parse_database_listing(out) {
            DatabaseListing::Rows(rows) => panic!(
                "{what}: returned {} database row(s) built from an error message — the first is \
                 {:?}",
                rows.len(),
                rows.first()
            ),
            DatabaseListing::ToolError(_) | DatabaseListing::GeneratorFailure(_) => rejected += 1,
        }
    }
    assert_eq!(
        rejected,
        GENERATOR_FAILURES.len(),
        "{rejected} of {} failure shapes rejected",
        GENERATOR_FAILURES.len()
    );
    println!(
        "database listing: {rejected}/{} rejected",
        GENERATOR_FAILURES.len()
    );
}

/// The positive control. Without it a parser that rejected EVERYTHING would pass the test above.
#[test]
fn a_real_listing_is_still_parsed() {
    let out = "/usr/irissys/mgr/\t|1|40|0\n/usr/irissys/mgr/user/|1|10|512\n";
    let rows = match parse_database_listing(out) {
        DatabaseListing::Rows(rows) => rows,
        other => panic!(
            "a valid listing was rejected: {}",
            match other {
                DatabaseListing::ToolError(m) => format!("ToolError({m})"),
                DatabaseListing::GeneratorFailure(m) => format!("GeneratorFailure({m})"),
                DatabaseListing::Rows(_) => unreachable!(),
            }
        ),
    };
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    assert_eq!(rows[1]["directory"], "/usr/irissys/mgr/user/");
    assert_eq!(rows[1]["mounted"], true);
    assert_eq!(rows[1]["size_mb"], 10.0);
    assert_eq!(rows[1]["max_size_mb"], 512.0);
}

/// A genuinely empty answer stays an answer — the whole point of the third case is that
/// "unavailable" and "none" stop being the same value.
#[test]
fn an_empty_listing_is_an_answer_not_a_failure() {
    match parse_database_listing("") {
        DatabaseListing::Rows(rows) => assert!(rows.is_empty(), "{rows:?}"),
        _ => panic!("an empty listing must be Rows(vec![]), not a failure"),
    }
}

/// `SYS.Database:List` refusing is not the same as the generator failing, and they carry
/// different error codes to the caller, so the two must not collapse into one case.
#[test]
fn the_snippets_own_status_is_a_tool_error_not_a_generator_failure() {
    match parse_database_listing("ERROR:ERROR #5002: cannot list databases") {
        DatabaseListing::ToolError(m) => assert!(m.contains("#5002"), "{m}"),
        DatabaseListing::GeneratorFailure(m) => {
            panic!("a %Status refusal was reported as a generator failure: {m}")
        }
        DatabaseListing::Rows(r) => panic!("a refusal was parsed as {} row(s): {r:?}", r.len()),
    }
}

// ── iris_admin list_user_roles ───────────────────────────────────────────────────────────────

#[test]
fn no_generator_failure_is_parsed_as_a_role_list() {
    let mut rejected = 0;
    for (what, out) in GENERATOR_FAILURES {
        match parse_user_roles(out) {
            UserRoles::Roles(roles) => panic!(
                "{what}: returned {} role(s) built from an error message: {roles:?}",
                roles.len()
            ),
            UserRoles::NotFound(_) | UserRoles::GeneratorFailure(_) => rejected += 1,
        }
    }
    assert_eq!(
        rejected,
        GENERATOR_FAILURES.len(),
        "{rejected} of {} failure shapes rejected",
        GENERATOR_FAILURES.len()
    );
    println!(
        "role list: {rejected}/{} rejected",
        GENERATOR_FAILURES.len()
    );
}

/// The positive control for the roles parser.
#[test]
fn a_real_role_list_is_still_parsed() {
    match parse_user_roles("%All,%Manager, %Developer ") {
        UserRoles::Roles(roles) => assert_eq!(roles, vec!["%All", "%Manager", "%Developer"]),
        UserRoles::NotFound(m) => panic!("NotFound({m})"),
        UserRoles::GeneratorFailure(m) => panic!("GeneratorFailure({m})"),
    }
}

/// A user who holds no roles is a real answer, and it must not become "we could not tell".
#[test]
fn a_user_with_no_roles_is_an_answer() {
    match parse_user_roles("") {
        UserRoles::Roles(roles) => assert!(roles.is_empty(), "{roles:?}"),
        _ => panic!("no roles must be Roles(vec![]), not a failure"),
    }
}

/// `USER_NOT_FOUND` is checked before the generator shapes, because its own text begins with
/// `ERROR:` and a later check would otherwise relabel a clean negative as a runtime failure.
#[test]
fn a_missing_user_is_still_reported_as_missing() {
    match parse_user_roles("ERROR:USER_NOT_FOUND:User not found: nosuchuser") {
        UserRoles::NotFound(m) => assert!(m.contains("nosuchuser"), "{m}"),
        UserRoles::Roles(r) => panic!("parsed as roles: {r:?}"),
        UserRoles::GeneratorFailure(m) => {
            panic!(
                "a missing user was relabelled a generator failure, which tells the caller to \
                    retry something that will never succeed: {m}"
            )
        }
    }
}

/// The two parsers are siblings and this defect travels in pairs, so the shapes are checked
/// against BOTH rather than trusting that fixing one implies the other.
#[test]
fn both_parsers_reject_the_same_set() {
    let mut pairs = 0;
    for (what, out) in GENERATOR_FAILURES {
        let db_rejects = !matches!(parse_database_listing(out), DatabaseListing::Rows(_));
        let roles_reject = !matches!(parse_user_roles(out), UserRoles::Roles(_));
        assert_eq!(
            db_rejects, roles_reject,
            "{what}: the two siblings disagree (databases reject={db_rejects}, \
             roles reject={roles_reject}). One of them is interpreting an error as data."
        );
        pairs += 1;
    }
    assert_eq!(pairs, GENERATOR_FAILURES.len());
    println!("sibling agreement checked on {pairs} shapes");
}
