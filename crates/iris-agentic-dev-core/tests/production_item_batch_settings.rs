//! #327 item 3: `iris_production_item(get_settings)` reads several items in one round trip.
//!
//! Measured over 31,009 parsed `tool_use` blocks: the tool arrives in bursts (53 runs of 2, 14 of 3,
//! up to 8) and `get_settings` is 296 of the 513 action invocations inside them — the bursts are
//! mostly reading several items' settings one at a time. 174 calls disappear if the action takes a
//! list.
//!
//! The fixtures are REAL. They were read on 2026-09-23 from a live production
//! (`Comun.Produccion`, IRIS 2026.1, read-only over Atelier): 10 config items and 65 settings. Two
//! properties of that data drive tests below and would not have occurred to an invented fixture:
//!
//! * `ReplyCodeActions` has the value `:?R=F,:?E=F,:~=S,:?A=C,:*=S,:I?=W,:T?=C` — so a parser that
//!   splits on every `=` mangles a real setting.
//! * One item carries both `Adapter` and `Host` settings, and `Setting.Target` is NOT in the output.
//!   A Host and an Adapter setting of the same name therefore collapse to one map key. That is
//!   pre-existing, and now counted rather than silent.

use iris_agentic_dev_core::tools::interop::{
    build_get_settings_batch_code, item_names_arg, item_settings_payload, parse_get_settings_batch,
    synthesise_not_found_payload, ItemSettings,
};
use serde_json::json;

/// Real items from `Comun.Produccion`.
const ITEMS: [&str; 4] = [
    "Censo.BS.HL7",
    "Censo.Router",
    "Censo.BO.DietoolsHL7",
    "Dietas.BP.Dietas",
];

/// A framed reply for the first three, built from the real settings of each.
fn real_output() -> String {
    "PI_ITEM:Censo.BS.HL7\n\
     PI_NSET:5\n\
     PI_SET:Port=43210\n\
     PI_SET:StayConnected=30\n\
     PI_SET:MessageSchemaCategory=2.8\n\
     PI_SET:TargetConfigNames=Censo.Router\n\
     PI_SET:AlertOnError=1\n\
     PI_ITEM:Censo.Router\n\
     PI_NSET:2\n\
     PI_SET:BusinessRuleName=Censo.RUL.Censo\n\
     PI_SET:AlertOnError=1\n\
     PI_ITEM:Censo.BO.DietoolsHL7\n\
     PI_NSET:3\n\
     PI_SET:IPAddress=demo-genai-dietools-mllp\n\
     PI_SET:Port=2575\n\
     PI_SET:ReplyCodeActions=:?R=F,:?E=F,:~=S,:?A=C,:*=S,:I?=W,:T?=C\n\
     ITEM_CANDIDATES_N:2\n\
     ITEM_CANDIDATE:Censo.BS.HL7\n\
     ITEM_CANDIDATE:Censo.Router\n"
        .to_string()
}

fn read(state: &ItemSettings) -> &Vec<(String, String)> {
    match state {
        ItemSettings::Read { settings, .. } => settings,
        other => panic!("expected Read, got {other:?}"),
    }
}

#[test]
fn every_item_section_is_attributed_to_its_own_item() {
    let b = parse_get_settings_batch(&real_output());
    let names: Vec<&str> = b.items.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["Censo.BS.HL7", "Censo.Router", "Censo.BO.DietoolsHL7"],
        "settings landing under the wrong item is worse than not reading them at all"
    );
    assert_eq!(read(&b.items[0].1).len(), 5);
    assert_eq!(read(&b.items[1].1).len(), 2);
    assert!(b.items.iter().all(|(_, st)| st.complete()));
}

/// The real value that breaks a naive parser: only the FIRST `=` separates name from value.
#[test]
fn a_setting_value_containing_equals_signs_survives_whole() {
    let b = parse_get_settings_batch(&real_output());
    let s = read(&b.items[2].1);
    let (name, value) = s
        .iter()
        .find(|(k, _)| k == "ReplyCodeActions")
        .expect("ReplyCodeActions");
    assert_eq!(name, "ReplyCodeActions");
    assert_eq!(
        value, ":?R=F,:?E=F,:~=S,:?A=C,:*=S,:I?=W,:T?=C",
        "this is the value a real production carries; splitting on every `=` truncates it at `:?R`"
    );
}

/// A section that arrives with fewer settings than IRIS declared is INCOMPLETE, not a shorter
/// settings list. Same rule #347 established for the `%Status` chain.
#[test]
fn a_section_cut_short_is_incomplete_not_a_smaller_settings_list() {
    let b = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:4\nPI_SET:Port=1\n");
    assert_eq!(
        b.items[0].1,
        ItemSettings::Read {
            settings: vec![("Port".into(), "1".into())],
            declared: 4
        }
    );
    assert!(!b.items[0].1.complete());
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(v["settings_incomplete"], true);
    assert_eq!(v["settings_received"], 1);
    assert_eq!(v["settings_declared"], 4);
    // CONTROL: a section that IS whole must not carry the flag, or the assertion above is
    // satisfied by a payload that always claims incompleteness.
    let whole = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:1\nPI_SET:Port=1\n");
    let wv = item_settings_payload("A", &whole.items[0].1);
    assert!(wv.get("settings_incomplete").is_none(), "{wv}");
}

/// A missing item is its own state. Reporting it as an item with no settings would answer a
/// failure with a fact (#310).
#[test]
fn a_missing_item_is_not_an_item_with_no_settings() {
    let b = parse_get_settings_batch("PI_ITEM:Nope\nPI_MISSING:1\nPI_ITEM:A\nPI_NSET:0\n");
    assert_eq!(b.items[0].1, ItemSettings::Missing);
    let v = item_settings_payload("Nope", &b.items[0].1);
    assert_eq!(v["found"], false);
    assert_eq!(v["error_code"], "ITEM_NOT_FOUND");
    assert!(
        v.get("settings").is_none(),
        "a missing item has no settings map: {v}"
    );
    // An item that genuinely holds zero settings is a DIFFERENT answer, and says found: true.
    let empty = item_settings_payload("A", &b.items[1].1);
    assert_eq!(empty["found"], true);
    assert_eq!(empty["settings_declared"], 0);
}

/// A section carrying neither a count nor a missing marker told us nothing. Not an empty map.
#[test]
fn a_section_with_no_count_and_no_marker_is_unreadable_not_empty() {
    let b = parse_get_settings_batch("PI_ITEM:A\n");
    match &b.items[0].1 {
        ItemSettings::Unreadable(why) => assert!(!why.is_empty(), "must say why"),
        other => panic!("expected Unreadable, got {other:?}"),
    }
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(v["error_code"], "ITEM_UNREADABLE");
    assert_ne!(
        v["error_code"], "ITEM_NOT_FOUND",
        "a malformed section is not a missing item"
    );
}

/// The pre-existing loss, now counted: the program writes `Name=Value` and drops `Setting.Target`,
/// so a Host and an Adapter setting of the same name become one map key. Real shape — the production
/// read for these fixtures has items carrying both targets.
#[test]
fn settings_that_collapse_in_the_map_are_counted_not_hidden() {
    let b = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:2\nPI_SET:Port=8080\nPI_SET:Port=443\n");
    assert!(
        b.items[0].1.complete(),
        "both rows arrived — the transport lost nothing"
    );
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(
        v["duplicate_setting_names"], 1,
        "two rows, one key: the caller must be able to see that the map is not the whole answer"
    );
    assert_eq!(v["settings_declared"], 2);
    // CONTROL: distinct names must NOT report a collapse.
    let ok = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:2\nPI_SET:Port=8080\nPI_SET:Host=x\n");
    let okv = item_settings_payload("A", &ok.items[0].1);
    assert!(okv.get("duplicate_setting_names").is_none(), "{okv}");
}

/// A value containing a newline keeps its remainder. Dropping it would leave a plausible shorter
/// value behind — the worst of the two failures.
#[test]
fn a_value_spanning_lines_keeps_its_continuation() {
    let b = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:1\nPI_SET:Note=first\nsecond line\n");
    let s = read(&b.items[0].1);
    assert_eq!(s.len(), 1, "the continuation is not a second setting");
    assert_eq!(s[0].1, "first\nsecond line");
    assert!(b.items[0].1.complete(), "one declared, one received");
}

// ── the generated program ─────────────────────────────────────────────────────────────────

#[test]
fn the_program_addresses_every_item_asked_for() {
    let names: Vec<String> = ITEMS.iter().map(|s| s.to_string()).collect();
    let code = build_get_settings_batch_code("Comun.Produccion", &names);
    for n in ITEMS {
        assert_eq!(
            code.matches(&format!("\"{n}\"")).count(),
            2,
            "{n} must appear twice — once written as the section header, once passed to \
             FindItemByConfigName. Code:\n{code}"
        );
    }
    // CONTROL: an item that was not asked for must not be in the program.
    assert!(!code.contains("Dietas.BO.Login"), "{code}");
}

/// The bug this framing exists to prevent. Measured on IRIS 2026.1: a `Quit` inside an `If` block
/// returns from the METHOD. One missing item would end the run and every later item would be absent
/// from the output — silently, since absence is how a caller would read "no settings".
#[test]
fn a_missing_item_does_not_end_the_program() {
    let names: Vec<String> = ITEMS.iter().map(|s| s.to_string()).collect();
    let code = build_get_settings_batch_code("", &names);
    // The MISSING branch only — the `{ … }` between `If '$IsObject(tItem)` and ` Else `. A first
    // version of this test asserted "no Quit anywhere on the line" and failed on the shipped code,
    // because the Else branch's `For { … Quit:zk="" … }` exits the LOOP and is exactly right. A
    // guard whose window is wider than its claim reddens on correct code, which is how it gets
    // loosened until it catches nothing.
    let mut checked = 0;
    for line in code.lines() {
        let Some(after) = line.split_once("If '$IsObject(tItem) {").map(|x| x.1) else {
            continue;
        };
        let branch = after
            .split_once("} Else {")
            .map(|x| x.0)
            .unwrap_or_else(|| panic!("the per-item guard has no Else branch: {line}"));
        checked += 1;
        assert!(
            !branch.contains("Quit"),
            "a Quit in the MISSING branch returns from the METHOD (measured on IRIS 2026.1), so \
             every item after the first missing one never reports at all — and absence is how a \
             caller reads \"no settings\". Branch: {branch}"
        );
        // The branch must still SAY the item is missing, or "no Quit" is satisfied by saying
        // nothing at all.
        assert!(
            branch.contains("PI_MISSING:"),
            "the missing branch must report the miss: {branch}"
        );
    }
    // CONTROL: one guarded section per item asked for, or this test is measuring nothing.
    assert_eq!(
        checked,
        ITEMS.len(),
        "expected one per-item guard for each of the {} items:\n{code}",
        ITEMS.len()
    );
}

/// The defect a live run found, which every parser test was blind to: `FindItemByConfigName`
/// resolves through the RUNTIME dispatch index, so on a production that has never been started it
/// returns nothing for an item that is demonstrably there.
///
/// Measured on IRIS 2026.1 against `IOProbe.Produccion` (configured, never started):
///
/// ```text
/// tProd.Items.Count()                         -> 3, all three names present
/// tProd.FindItemByConfigName("Probe.BS.Feed") -> NO object, ERROR #00: (no error description)
/// ^Ens.Runtime("DispatchName")                -> does not exist, 0 entries
/// walking tProd.Items and matching .Name      -> resolves all three correctly
/// ```
///
/// A mutation restoring `FindItemByConfigName` passed all 25 other tests in this file, because they
/// feed the PARSER hand-written marker text and cannot see the ObjectScript. This is the assertion
/// that can.
#[test]
fn the_program_resolves_items_from_config_not_from_the_runtime_index() {
    let names: Vec<String> = ITEMS.iter().map(|s| s.to_string()).collect();
    let code = build_get_settings_batch_code("IOProbe.Produccion", &names);
    assert!(
        !code.contains("FindItemByConfigName"),
        "this resolves through ^Ens.Runtime(\"DispatchName\"), which is empty until a production \
         has been started — so every item of a production being BUILT reports ITEM_NOT_FOUND:\n{code}"
    );
    assert!(
        code.contains("tProd.Items.Count()") && code.contains(".Name="),
        "the lookup must walk the CONFIG items and match on Name:\n{code}"
    );
    // CONTROL: the walk is emitted once PER ITEM asked for, not once for the whole program — an
    // over-specific count of GetAt references was wrong here (it fails on correct code, which is how
    // a guard gets loosened), so this counts the per-item loops instead.
    assert_eq!(
        code.matches("For zpi=1:1:tProd.Items.Count()").count(),
        ITEMS.len(),
        "expected one config walk per item asked for:\n{code}"
    );
}

/// Every `For` on one line: `build_exec_class` splits generated code on `\n`, so a loop whose body
/// is on the next line is split from it.
#[test]
fn every_loop_stays_on_one_line() {
    let names = vec!["A".to_string(), "B".to_string()];
    let code = build_get_settings_batch_code("P", &names);
    for line in code.lines() {
        if line.contains(" For ") || line.trim_start().starts_with("For ") {
            let opens = line.matches('{').count();
            let closes = line.matches('}').count();
            assert_eq!(
                opens, closes,
                "a For block is split across lines, and build_exec_class splits on newlines: {line}"
            );
        }
    }
}

/// The ITEM_NOT_FOUND envelope is still built from the payload shape its one reader parses, rather
/// than a second copy of that reader growing here.
#[test]
fn the_not_found_payload_keeps_the_shape_its_reader_parses() {
    let p = synthesise_not_found_payload(
        &["Nope".to_string(), "AlsoNope".to_string()],
        "ITEM_CANDIDATES_N:2\nITEM_CANDIDATE:Censo.BS.HL7\nITEM_CANDIDATE:Censo.Router\n",
    );
    let first = p.lines().next().expect("a first line");
    assert!(
        first.starts_with("Item not found:"),
        "the reader takes the message from line 0: {first}"
    );
    assert!(
        !first.starts_with("ERROR:"),
        "and that message is what the caller reads, so the wire marker must not be in it: {first}"
    );
    assert!(
        first.contains("Nope") && first.contains("AlsoNope"),
        "both names: {first}"
    );
    assert!(
        p.contains("ITEM_CANDIDATES_N:2"),
        "the declared count must survive: {p}"
    );
    assert_eq!(p.matches("ITEM_CANDIDATE:").count(), 2);
}

// ── the parameter reader ──────────────────────────────────────────────────────────────────

#[test]
fn item_and_items_are_additive_and_nothing_is_discarded() {
    let v = json!({"item": "A", "items": ["B", "C"]});
    assert_eq!(item_names_arg(&v), vec!["A", "B", "C"]);
}

#[test]
fn a_name_given_twice_is_read_once() {
    let v = json!({"item": "A", "items": ["A", "B", "  A  ", ""]});
    assert_eq!(item_names_arg(&v), vec!["A", "B"]);
}

#[test]
fn the_list_synonyms_are_read_and_a_missing_list_is_empty() {
    assert_eq!(item_names_arg(&json!({"item_names": ["A"]})), vec!["A"]);
    assert_eq!(item_names_arg(&json!({"itemNames": ["A"]})), vec!["A"]);
    assert!(item_names_arg(&json!({"action": "list"})).is_empty());
    // CONTROL: a key that is not a list spelling must not be read as one.
    assert!(item_names_arg(&json!({"widgets": ["A"]})).is_empty());
}

// ── the response assembly ─────────────────────────────────────────────────────────────────

use iris_agentic_dev_core::tools::interop::{
    get_settings_payload, list_refused_for_action, GetSettingsOutcome,
};

fn payload(out: &str, legacy_single: bool) -> serde_json::Value {
    match get_settings_payload(&parse_get_settings_batch(out), legacy_single) {
        GetSettingsOutcome::Payload(v) => v,
        other => panic!("expected a payload, got {other:?}"),
    }
}

/// A one-item call keeps exactly the payload it has always had, so adding the list parameter changes
/// nothing for a caller that does not pass one.
#[test]
fn a_single_item_call_reports_the_shape_it_always_did() {
    let v = payload(
        "PI_ITEM:Censo.Router\nPI_NSET:1\nPI_SET:BusinessRuleName=Censo.RUL.Censo\n",
        true,
    );
    assert_eq!(v["success"], true);
    assert_eq!(v["item"], "Censo.Router");
    assert_eq!(v["settings"]["BusinessRuleName"], "Censo.RUL.Censo");
    assert!(
        v.get("results").is_none(),
        "no results array on a single-item call: {v}"
    );
    assert!(
        v.get("found").is_none(),
        "`found` belongs to a results entry, not to a single-item payload: {v}"
    );
}

/// Asking for a list gets the list shape even when only one name was in it — the caller asked for a
/// batch and should not have to handle two response shapes depending on how many names they sent.
#[test]
fn asking_with_a_list_reports_the_list_shape() {
    let v = payload("PI_ITEM:A\nPI_NSET:1\nPI_SET:Port=1\n", false);
    assert_eq!(v["count"], 1);
    assert_eq!(v["results"][0]["item"], "A");
    assert_eq!(v["results"][0]["found"], true);
    assert!(v.get("item").is_none(), "{v}");
}

/// The partial case: some items found, some not. `success` is about the CALL — the read happened —
/// and `all_found` plus `missing` are about the answer.
#[test]
fn a_partial_read_names_what_was_missing_and_does_not_claim_all_found() {
    let v = payload(
        "PI_ITEM:Censo.BS.HL7\nPI_NSET:1\nPI_SET:Port=43210\nPI_ITEM:Nope\nPI_MISSING:1\n",
        false,
    );
    assert_eq!(v["success"], true, "the read happened");
    assert_eq!(v["all_found"], false);
    assert_eq!(v["missing"], json!(["Nope"]));
    assert_eq!(
        v["count"], 2,
        "both items are reported, not just the one found"
    );
    assert_eq!(v["results"][0]["found"], true);
    assert_eq!(v["results"][1]["found"], false);
}

/// And a complete read says so — without this, "always false" satisfies the test above.
#[test]
fn a_complete_read_claims_all_found() {
    let v = payload(
        "PI_ITEM:A\nPI_NSET:1\nPI_SET:Port=1\nPI_ITEM:B\nPI_NSET:0\n",
        false,
    );
    assert_eq!(v["all_found"], true);
    assert_eq!(v["missing"], json!([]));
}

/// An item whose section arrived SHORT must not count as found-and-whole. A truncated settings list
/// presented as the item's settings is the failure the declared count exists to catch.
#[test]
fn a_short_section_stops_the_batch_claiming_all_found() {
    let v = payload("PI_ITEM:A\nPI_NSET:9\nPI_SET:Port=1\n", false);
    assert_eq!(v["all_found"], false, "9 declared, 1 arrived: {v}");
    assert_eq!(
        v["missing"],
        json!([]),
        "the item is not missing — its settings are"
    );
    assert_eq!(v["results"][0]["settings_incomplete"], true);
}

/// Every name absent is the same answer a single-item call gives, under the same code — not a
/// success carrying an empty results array.
#[test]
fn every_item_absent_is_reported_as_item_not_found() {
    let out = "PI_ITEM:Nope\nPI_MISSING:1\nPI_ITEM:AlsoNope\nPI_MISSING:1\n\
               ITEM_CANDIDATES_N:1\nITEM_CANDIDATE:Censo.Router\n";
    match get_settings_payload(&parse_get_settings_batch(out), false) {
        GetSettingsOutcome::NoneFound(p) => {
            // NO wire marker. `item_not_found` takes its message from line 0, and the single-item arm
            // strips `ERROR:ITEM_NOT_FOUND:` before handing the payload over — synthesising it WITH
            // the marker put it in the caller's `error`, measured against a live production.
            assert!(
                !p.starts_with("ERROR:"),
                "the wire marker must not reach the caller's message: {p}"
            );
            assert!(p.starts_with("Item not found:"), "{p}");
            assert!(p.contains("Nope") && p.contains("AlsoNope"), "{p}");
            assert!(
                p.contains("ITEM_CANDIDATE:Censo.Router"),
                "candidates survive: {p}"
            );
        }
        other => panic!("expected NoneFound, got {other:?}"),
    }
    // CONTROL: one found out of two is NOT this case.
    assert!(matches!(
        get_settings_payload(
            &parse_get_settings_batch("PI_ITEM:Nope\nPI_MISSING:1\nPI_ITEM:A\nPI_NSET:0\n"),
            false
        ),
        GetSettingsOutcome::Payload(_)
    ));
}

/// Nothing framed at all is its own outcome, never an empty answer.
#[test]
fn an_unframed_reply_is_unreadable_not_an_empty_batch() {
    assert_eq!(
        get_settings_payload(
            &parse_get_settings_batch("Port=43210\nStayConnected=30\n"),
            false
        ),
        GetSettingsOutcome::Unreadable,
        "bare Name=Value lines are the OLD program's output — with no framing there is no way to \
         say which item they belong to, so reporting zero items would be a guess"
    );
}

// ── the refusal on actions that address one item ──────────────────────────────────────────

#[test]
fn only_get_settings_accepts_a_list_of_more_than_one() {
    let two = vec!["A".to_string(), "B".to_string()];
    assert_eq!(list_refused_for_action("get_settings", &two), None);
    for action in ["add", "remove", "enable", "disable", "set_settings", "list"] {
        let why = list_refused_for_action(action, &two)
            .unwrap_or_else(|| panic!("{action} must refuse a list of two"));
        assert!(
            why.contains(action),
            "the refusal must name the action: {why}"
        );
        assert!(why.contains('2'), "and how many were given: {why}");
        assert!(
            why.contains("get_settings"),
            "and which action does take a list: {why}"
        );
    }
}

/// One name is one item, so every action takes it. Refusing here would reject a request that names
/// exactly what the action needs, only in the other spelling.
#[test]
fn a_list_of_one_is_accepted_everywhere() {
    let one = vec!["A".to_string()];
    for action in [
        "add",
        "remove",
        "enable",
        "disable",
        "set_settings",
        "get_settings",
    ] {
        assert_eq!(list_refused_for_action(action, &one), None, "{action}");
    }
    assert_eq!(list_refused_for_action("add", &[]), None);
}

// ── which RESPONSE shape a call gets ──────────────────────────────────────────────────────

use iris_agentic_dev_core::tools::interop::list_parameter_given;

/// The regression a live run caught and the pure-function test could not: `items` non-empty selects
/// the batch payload, and `item_names_arg` folds a single `item` INTO `items` — so keying the shape
/// on the union handed every existing single-item caller the new `results[]` payload.
#[test]
fn a_single_item_parameter_is_not_a_list() {
    assert!(!list_parameter_given(&json!({"item": "A"})));
    assert!(!list_parameter_given(&json!({"item_name": "A"})));
    assert!(!list_parameter_given(&json!({"action": "get_settings"})));
    // A list key that carries nothing usable is not a list either.
    assert!(!list_parameter_given(&json!({"items": []})));
    assert!(!list_parameter_given(&json!({"items": ["", "   "]})));
    // CONTROL: a real list IS one, under every accepted spelling.
    for key in ["items", "item_names", "itemNames"] {
        assert!(
            list_parameter_given(&json!({key: ["A"]})),
            "{key} must count as a list"
        );
    }
    // And `item` alongside a real list is still a list call.
    assert!(list_parameter_given(&json!({"item": "A", "items": ["B"]})));
}

/// The two predicates answer different questions, and conflating them is the regression above.
#[test]
fn naming_an_item_and_passing_a_list_are_different_questions() {
    let single = json!({"item": "A"});
    assert_eq!(item_names_arg(&single), vec!["A"], "the name is read");
    assert!(!list_parameter_given(&single), "but no list was given");
}
