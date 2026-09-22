//! The `iris_test` description must advertise the per-failure record the code really emits.
//!
//! WHAT ROTTED. `inline_failed_tests` emitted one more key than the description advertised: the
//! extra one is `failure_kind` (#273/#275) — the field works, only the text was stale. No count is
//! written here on purpose; the counts are derived below, and a number in the header of the file
//! whose thesis is that prose counts rot would be the joke writing itself. A model reading
//! the description could not know the key exists, so an abort was indistinguishable from an assert
//! with nothing recorded: the #310 shape sitting in the null arm of a tri-state.
//!
//! WHY A TEST AND NOT JUST AN EDIT. #294 is the same defect one layer over — the README advertised
//! 23 tools while 30 shipped, and the count and the list had rotted together. Editing prose fixes
//! today's drift and nothing else; what stuck there was a test that reads the user-facing text and
//! asserts it against what the code actually advertises, so the prose cannot drift without a red
//! build. Same remedy here.
//!
//! BOTH SIDES ARE DERIVED, neither is listed. The documented set is parsed out of the description
//! the server really ships (`advertised_tools()` — what a client receives, not a source grep); the
//! emitted set comes from CALLING `inline_failed_tests` and reading the JSON keys back. A hardcoded
//! list of expected keys would be the same rot one layer over, in the guard this time.
//!
//! WHY ITS OWN FILE: as `readme_tool_list.rs` records, a new test file collides with nothing on a
//! merge, and `scripts/ci-test-targets.sh` derives its targets from `cargo metadata`, so this is
//! picked up by the gate without being registered anywhere.

use std::collections::BTreeSet;

use iris_agentic_dev_core::tools::unittest_result::shape_method_row;
use iris_agentic_dev_core::tools::{inline_failed_tests, IrisTools, Toolset};

/// A floor on the PARSE, not a statement of the contract: any plausible per-failure record names at
/// least a class, a method and a message. The contract is the set equality in the test — this only
/// stops a broken extraction from reporting "every key is documented" over an empty set, which is
/// exactly how an earlier guard in this repo read as a pass while matching nothing.
const MIN_PLAUSIBLE_KEYS: usize = 3;

/// The description the server actually advertises for `iris_test`.
///
/// FAILS, never skips: a guard that opts out when it cannot read its input is not a guard.
/// Read a source file from this crate. PANICS rather than skipping: a guard that opts out when it
/// cannot read its input is the defect it exists to catch.
fn read_core_src(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — this guard must fail, not pass by being unable to look",
            path.display()
        )
    })
}

fn iris_test_description() -> String {
    let tools = IrisTools::new_with_toolset(None, Toolset::Interop)
        .unwrap_or_else(|e| {
            panic!("cannot build the interop toolset ({e}) — this guard must not pass by being unable to look")
        })
        .advertised_tools();
    assert!(!tools.is_empty(), "precondition: the profile is non-empty");
    let tool = tools
        .iter()
        .find(|t| t.name == "iris_test")
        .unwrap_or_else(|| panic!("iris_test is not advertised in the interop profile — if it moved, point this guard at the profile that has it; do not delete it"));
    let desc = tool
        .description
        .clone()
        .unwrap_or_else(|| panic!("iris_test is advertised with NO description at all"))
        .to_string();
    // CONTROL: the description was really read. An empty string would satisfy a `contains` loop
    // vacuously and make every extraction below "clean".
    assert!(
        desc.len() > 100,
        "the iris_test description looks unread: {desc:?}"
    );
    desc
}

/// The key names the description advertises, taken from the `{...}` record it shows.
///
/// LOOSE ON PURPOSE. It splits the record on commas and strips only surrounding quoting; it does
/// NOT filter names through a character class. `[a-z_]+` is how three separate counts went wrong in
/// this repo in one day — it silently drops anything with a digit (`e2e`, `hl7`, `x12`, `sha256`).
fn documented_keys(desc: &str) -> BTreeSet<String> {
    const ANCHOR: &str = "failed_tests";
    let from = desc.find(ANCHOR).unwrap_or_else(|| {
        panic!("the iris_test description no longer mentions `{ANCHOR}`, so this guard cannot locate the per-failure record. Point it at the new wording rather than letting it skip. Description: {desc}")
    });
    let open = desc[from..]
        .find('{')
        .map(|i| from + i)
        .unwrap_or_else(|| panic!("no brace-delimited record after `{ANCHOR}` in the iris_test description — the record is what tells a caller which keys exist, and a description without one documents nothing. Description: {desc}"));
    let close = desc[open..]
        .find('}')
        .map(|i| open + i)
        .unwrap_or_else(|| panic!("unterminated record in the iris_test description: {desc}"));
    let inner = &desc[open + 1..close];
    inner
        .split(',')
        .map(|s| {
            s.trim()
                .trim_matches(|c: char| c == '`' || c == '\'' || c == '"')
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// A red case with every detail present — what the `%UnitTest_Result` path produces.
fn populated_case() -> serde_json::Value {
    serde_json::json!({
        "name": "TestIsoDate",
        "class_name": "MyApp.Tests.DT.PatientToCensus",
        "status": "failed",
        "duration_ms": null,
        "failure_message": "1962-03-15 -> 15/03/1962 (DD/MM/YYYY)",
        "failure_location": "TestIsoDate+3^MyApp.Tests.DT.PatientToCensus.1",
        "failure_assert": "AssertEquals",
        "failure_kind": "assert",
    })
}

/// The stdout-fallback shape: no assert row to read and NO `failure_kind` key at all. This is the
/// arm the description has to warn about, so the guard carries a case that really has it.
fn fallback_case() -> serde_json::Value {
    serde_json::json!({
        "name": "TestThing",
        "class_name": "MyApp.Tests.X",
        "status": "failed",
        "duration_ms": null,
        "failure_message": "some generic text",
        "failure_location": null,
        "failure_assert": null,
    })
}

/// Run one probe case through the real function and hand back the record's object.
///
/// Every way this can go wrong is a panic, not an empty result: "could not build a case" must never
/// be reported as "the record has no keys".
fn inline_record(case: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let (inline, total, truncated) = inline_failed_tests(std::slice::from_ref(case));
    assert_eq!(
        total, 1,
        "the probe case was not counted as a failure — inline_failed_tests selects on status==\"failed\", so the PROBE broke, not the contract: {case}"
    );
    assert!(!truncated, "one case cannot exceed the inline cap");
    let rec = inline.into_iter().next().unwrap_or_else(|| {
        panic!("inline_failed_tests surfaced no record for a failing probe case: {case}")
    });
    rec.as_object().cloned().unwrap_or_else(|| {
        panic!("inline_failed_tests returned a non-object record, so this guard cannot read keys off it: {rec}")
    })
}

/// Every key the code can put in front of a caller, unioned over both probes.
fn emitted_keys() -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for case in [populated_case(), fallback_case()] {
        keys.extend(inline_record(&case).keys().cloned());
    }
    assert!(
        keys.len() >= MIN_PLAUSIBLE_KEYS,
        "the probes produced only {} key(s) ({keys:?}) — the probe is broken, and a comparison against it would mean nothing",
        keys.len()
    );
    keys
}

// `mentions_token` lived here: a whole-token check used only by the cap assertion. It is gone
// because the cap is now read POSITIONALLY — the integer immediately after an explicit
// "is capped at " marker — and token-anywhere was exactly what let the wrong-cap mutation survive
// twice. Removed rather than left with an allow(dead_code): an unused helper in a guard file is a
// check someone will wire back up without re-deriving why it was insufficient.

/// THE CONTRACT: the advertised record names exactly the keys the code emits.
#[test]
fn the_advertised_record_names_every_key_the_code_emits() {
    let desc = iris_test_description();
    let documented = documented_keys(&desc);
    assert!(
        documented.len() >= MIN_PLAUSIBLE_KEYS,
        "the extraction found only {} name(s) ({documented:?}) in the advertised record — a parse that finds (almost) nothing would report every key as documented over an empty set, so it refuses to pass. Description: {desc}",
        documented.len()
    );
    let emitted = emitted_keys();
    let undocumented: Vec<&String> = emitted.difference(&documented).collect();
    let imaginary: Vec<&String> = documented.difference(&emitted).collect();
    assert_eq!(
        documented, emitted,
        "the iris_test description and inline_failed_tests have drifted.\n  \
         EMITTED BUT UNDOCUMENTED (a model reading the tool description cannot know these exist): {undocumented:?}\n  \
         DOCUMENTED BUT NOT EMITTED (a caller will wait for a key that never arrives): {imaginary:?}"
    );
}

/// Every `KIND_*` value the code declares, read from the module that declares them.
///
/// Read out of ANOTHER file on purpose: the #273 note in `mod.rs` records a source scan that
/// matched the literal inside its own `find` call and sliced its own test body. Nothing scanned for
/// here occurs in this file.
///
/// WHAT THIS DOES NOT COVER: a kind value written as a bare literal, or declared in some other
/// module, is not enumerated here. The `unknown.is_empty()` check below closes that only for the
/// values the two probe rows actually produce.
fn declared_failure_kinds() -> BTreeSet<String> {
    const SRC: &str = include_str!("../src/tools/unittest_result.rs");
    let mut out = BTreeSet::new();
    for line in SRC.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const KIND_") else {
            continue;
        };
        let Some((_, after_eq)) = rest.split_once('=') else {
            continue;
        };
        let Some(value) = after_eq.split('"').nth(1) else {
            continue;
        };
        out.insert(value.to_string());
    }
    assert!(
        out.len() >= 2,
        "the scan found {} failure-kind constant(s) ({out:?}) — the module declares at least an assert kind and a runtime-error kind, so the scan is reading nothing and a clean result below would mean nothing",
        out.len()
    );
    out
}

/// A key is only half the story: a caller also needs the VALUES, and needs to be told that the
/// absent case is UNKNOWN rather than "no runtime error" — the #310 shape in a tri-state's null arm.
#[test]
fn the_description_documents_every_failure_kind_and_the_unknown_arm() {
    let desc = iris_test_description();
    let declared = declared_failure_kinds();

    // The declared constants are the real vocabulary, not dead code: the two shaper arms produce
    // them. Derived by calling the shaper, so a renamed constant cannot stay "documented".
    let row = |method: &str, fail_msg: serde_json::Value, err_desc: serde_json::Value| {
        serde_json::json!({
            "Class": "Pkg.T", "Method": method, "St": 0,
            "FailMsg": fail_msg, "FailLoc": "y+2^Pkg.T.1",
            "FailAct": "AssertStatusOK", "ErrDesc": err_desc, "ErrAct": serde_json::Value::Null,
        })
    };
    let abort_text =
        "ERROR #5002: ObjectScript error: <PROPERTY DOES NOT EXIST>TestAbort+1^Pkg.T.1";
    let (assert_tc, _) = shape_method_row(&row(
        "TestAssert",
        serde_json::json!("ERROR #5023: nope"),
        serde_json::Value::Null,
    ));
    let (abort_tc, _) = shape_method_row(&row(
        "TestAbort",
        serde_json::Value::Null,
        serde_json::json!(abort_text),
    ));
    let observed: BTreeSet<String> = [&assert_tc, &abort_tc]
        .iter()
        .filter_map(|tc| tc["failure_kind"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        observed.len(),
        2,
        "the two probe rows produced {observed:?} — this guard needs both arms distinguishable, so either the probes no longer reach the assert/abort branches or the kinds collapsed"
    );
    let unknown: Vec<&String> = observed.difference(&declared).collect();
    assert!(
        unknown.is_empty(),
        "the shaper emits kind value(s) {unknown:?} that no KIND_ constant declares, so the scan this guard checks the description against is incomplete"
    );

    let mut checked = 0usize;
    for kind in &declared {
        checked += 1;
        // AS A VALUE, quoted, not merely as a substring — the lesson the `iris_doc` guard paid for
        // with a surviving mutant, where `delete_lines` in the prose kept a renamed mode "named".
        // Here even a whole-token check would be satisfied by the prose "failed an assert".
        let advertised = format!("'{kind}'");
        // SLICED to the failure_kind sentence. Checking the whole description let an adversarial
        // pass add `KIND_ERRORS = "errors"` and pass, because `'errors'` already appeared in the
        // unrelated `outcome` enum — an undocumented kind reported as documented. Removing
        // `'errors'` from that enum (it is unreachable: the counter it derives from is a `let`
        // binding of 0) happens to close that route too, but the check must not depend on the
        // absence of a collision somewhere else in 2 KB of prose.
        let kind_clause = desc
            .split_once("failure_kind is ")
            .map(|(_, rest)| rest.split_once("Read those").map_or(rest, |(c, _)| c))
            .expect(
                "the description must explain failure_kind; that clause is what this asserts on",
            );
        assert!(
            kind_clause.contains(&advertised),
            "inline_failed_tests can report failure_kind='{kind}' and the iris_test description never offers `{advertised}` — a caller cannot branch on a value it has not been told about, and prose that merely uses the word is not an offer. Description: {desc}"
        );
    }
    // Counting ties the loop to the set: pointing it at an empty collection would otherwise assert
    // nothing while the plausibility check inside the scan still held.
    assert_eq!(
        checked,
        declared.len(),
        "the loop did not visit every declared kind — a vacuous check proves nothing"
    );

    // THE NULL ARM, and it is not hypothetical: both a red row with nothing recorded and the
    // stdout-fallback record come back with no kind. Because the code CAN emit that, the text has
    // to say so; if some later change always sets a kind, both probes go non-null and this
    // requirement lifts itself.
    let (silent_tc, _) = shape_method_row(&serde_json::json!({
        "Class": "Pkg.T", "Method": "TestSilent", "St": 0,
        "FailMsg": serde_json::Value::Null, "FailLoc": serde_json::Value::Null,
        "FailAct": serde_json::Value::Null, "ErrDesc": serde_json::Value::Null,
        "ErrAct": serde_json::Value::Null,
    }));
    let fallback = inline_record(&fallback_case());
    let kind_can_be_absent = silent_tc["failure_kind"].is_null()
        || fallback
            .get("failure_kind")
            .is_none_or(serde_json::Value::is_null);
    if kind_can_be_absent {
        // THE CONTRACT MARKER, asserted literally. This check used to be
        // `desc.to_lowercase().contains("absent")` — a one-word presence test over a 1.5 KB string.
        // An adversarial pass replaced the whole explanation with "It is absent when the test did
        // not abort." — i.e. the exact failure-as-a-negative-fact lie this test exists to forbid —
        // and it PASSED, because the word "absent" was still in the sentence. A presence test on a
        // word cannot distinguish a claim from its inverse.
        const MARKER: &str = "ABSENT means UNKNOWN";
        assert!(
            desc.contains(MARKER),
            "failure_kind is absent/null on at least one real path (a red test with nothing \
             recorded, and the stdout fallback). The description must carry the marker {MARKER:?} \
             verbatim, so that what it promises is a CONTRACT and not a word that happens to \
             appear. Description: {desc}"
        );

        // ...and the inverse must NOT appear. Without this, the marker can sit next to a sentence
        // that contradicts it and both assertions pass.
        for lie in [
            "absent when the test did not abort",
            "absent means no runtime error",
            "absent means the test merely failed",
        ] {
            assert!(
                !desc.to_lowercase().contains(lie),
                "the description says {lie:?}, which is precisely the reading {MARKER:?} forbids: \
                 an unset failure_kind is UNKNOWN, never evidence that nothing trapped"
            );
        }
    }
}

/// SIBLING ROT, same string: the description states the inline cap as a number. A number in prose
/// has no way to stay true, so the cap is derived from the code and the prose is checked against it.
#[test]
fn the_advertised_cap_is_the_cap_the_code_applies() {
    let desc = iris_test_description();
    let cases: Vec<serde_json::Value> = (0..64)
        .map(|i| {
            let mut c = populated_case();
            c["name"] = serde_json::json!(format!("Test{i}"));
            c
        })
        .collect();
    let (inline, total, truncated) = inline_failed_tests(&cases);
    assert_eq!(total, cases.len(), "every probe case must be red");
    assert!(
        truncated,
        "the probe did not exceed the inline cap ({} surfaced of {}), so inline.len() is the probe size and not the cap — raise the probe count",
        inline.len(),
        cases.len()
    );
    let cap = inline.len();

    // POSITIONAL, not token-anywhere. This used to be `mentions_token(&desc, &cap.to_string())`,
    // which asks only whether the digits appear SOMEWHERE in 1.5 KB of prose. An adversarial pass
    // changed the text to "the first 3 failures" while the code still capped at 10 and it PASSED,
    // because a standalone "10" existed elsewhere in the string. The earlier 10 -> 5 mutation died
    // only by luck: today's text happens to contain no standalone "5".
    //
    // So parse the number out of the CAP CLAUSE itself. The clause is located by the key it must
    // name — `failed_tests_total` is the field the cap is explained in terms of — and the integer
    // nearest before it is the claimed cap.
    // POSITIONAL, and narrow. The first attempt at this fix took "everything before the
    // failed_tests_total clause" as the window — 1.5 KB of prose — so a stray "10" elsewhere still
    // satisfied it and the wrong-cap mutation SURVIVED a second time. The window has to be the
    // marker's own text.
    const CAP_MARKER: &str = "is capped at ";
    let after = desc
        .split_once(CAP_MARKER)
        .map(|(_, r)| r)
        .unwrap_or_else(|| {
            panic!(
            "the description must state the inline cap as {CAP_MARKER:?}<n>, so the number has a \
             position to be read from rather than being looked for anywhere in the prose. \
             Description: {desc}"
        )
        });
    let stated: usize = after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| {
            panic!("no integer follows {CAP_MARKER:?} in the description. Description: {desc}")
        });
    assert_eq!(
        stated, cap,
        "the code surfaces at most {cap} failures inline and the description says {stated}. A cap \
         the description gets WRONG is worse than one it omits: the caller stops checking \
         failed_tests_truncated. Description: {desc}"
    );
}

/// THE SAME DRIFT ONE LEVEL OUT, and the reason this file exists rather than a prose edit.
///
/// `failure_kind` was the key that prompted #332, but it was not the only one missing. The
/// `iris_test` payload emits eighteen keys and the description named three of the interesting ones.
/// Absent from the text entirely, when measured: `runtime_errors` — which came from the SAME issue
/// (#273) as `failure_kind`, so a field was added to the payload twice and to the contract zero
/// times — plus `failed_tests_total` and `failed_tests_truncated`.
///
/// Those last two are the sharp case. `MAX_INLINE_FAILURES`' own doc comment justifies capping the
/// inline list at ten like this: *"The caller learns about the cap from the returned total and flag,
/// so the truncation is never silent."* Both fields it names were absent from the tool description —
/// the caller's only contract. So the truncation WAS silent to anyone reading what the tool
/// advertises, and the cap's defence rested on two fields the caller never learned existed. A model
/// handed ten failures out of forty had been told nothing: a partial answer reading as a total one,
/// inside the feature whose thesis is that shape.
///
/// This guard therefore reads the payload's keys from the source that builds it, not from a list.
#[test]
fn every_key_the_payload_emits_is_named_in_the_description() {
    let src = read_core_src("tools/mod.rs");
    let desc = iris_test_description();

    // Locate the payload literal by an anchor inside it that nothing else in the file carries.
    let anchor = "\"failed_tests_truncated\": failed_tests_truncated,";
    let end = src.find(anchor).expect(
        "the iris_test payload must still emit failed_tests_truncated; if it was renamed, \
                 re-anchor this guard rather than deleting it",
    ) + anchor.len();
    let start = src[..end]
        .rfind("\"success\": success,")
        .expect("the payload literal must start at the success key");
    let block = &src[start..end];

    let mut keys: Vec<String> = Vec::new();
    for line in block.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('"') {
            if let Some((k, _)) = rest.split_once("\":") {
                // Loose on purpose: a hand-rolled character class is how three separate counts went
                // wrong in this repo in one day ([a-z_]+ drops e2e, hl7, x12, sha256).
                if !k.is_empty() && !keys.iter().any(|e| e == k) {
                    keys.push(k.to_string());
                }
            }
        }
    }

    // Refuse to pass on an implausible parse: finding almost nothing must not read as "all keys
    // documented". The payload is large; a handful means the slice or the scan broke.
    const MIN_PAYLOAD_KEYS: usize = 12;
    assert!(
        keys.len() >= MIN_PAYLOAD_KEYS,
        "parsed only {} payload key(s) ({keys:?}) — expected at least {MIN_PAYLOAD_KEYS}. The slice \
         or the scan is wrong, and this guard must fail rather than report all-clear over a short \
         list",
        keys.len()
    );

    // Keys that carry no contract for the caller to act on. Each is listed WITH its reason, so the
    // exemption is auditable rather than a bag of names that quietly grows.
    let exempt: &[(&str, &str)] = &[
        ("success", "explained in the description as ==tests_passed"),
        ("tests_passed", "explained in the description"),
        ("completed", "explained in the description"),
        (
            "outcome",
            "explained in the description, with its value set",
        ),
        ("total", "self-describing count"),
        ("passed", "self-describing count"),
        ("failed", "self-describing count"),
        ("skipped", "self-describing count, hardcoded 0"),
        ("duration_ms", "self-describing, hardcoded null"),
        ("path", "echo of the input"),
        ("pattern", "echo of the input"),
        (
            "namespace",
            "echo of the input, and the description covers namespace resolution",
        ),
        (
            "errors",
            "always 0 on this path; the description now says so explicitly",
        ),
        ("test_suites", "container for per-suite rows"),
    ];

    let mut missing: Vec<&str> = Vec::new();
    for k in &keys {
        if exempt.iter().any(|(name, _)| name == k) {
            continue;
        }
        if !desc.contains(k.as_str()) {
            missing.push(k.as_str());
        }
    }
    assert!(
        missing.is_empty(),
        "the iris_test payload emits {missing:?} and the description never names them. A field a \
         caller is not told about is a field it cannot read — and for the truncation pair that makes \
         a capped list indistinguishable from a complete one. Either document them or add them to \
         the exempt list WITH a reason."
    );

    // The exempt list must not decay into a way to silence the guard: every name in it has to still
    // be a key the payload emits.
    let stale: Vec<&str> = exempt
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !keys.iter().any(|k| k == n))
        .collect();
    assert!(
        stale.is_empty(),
        "these names are exempted but the payload no longer emits them: {stale:?}. A stale \
         exemption silently excuses a key that does not exist while the one that replaced it goes \
         unchecked"
    );
}
