//! `iris_production_item(action=get_settings)` returned each setting as `Name=Value` and dropped
//! `Ens.Config.Setting.Target`, so the output of a read could not be fed back into `set_settings`
//! for any adapter setting. Measured on a real production: 23 of 65 settings.
//!
//! `Ens.Config.Setting` is keyed on (Name, Target). `set_settings` and `add` both resolve a key's
//! target through `resolve_setting_target` and pass it to `FindSettingByName`, so a bare adapter
//! name written back lands on the HOST — a setting the adapter never reads, reported as success.
//! The read side is now the exact inverse of the write side: `qualify_setting_key`.
//!
//! The three claims this file pins, each one the thing a mutation can take away:
//!
//! 1. The map no longer collapses a Host and an Adapter setting of one name onto one key.
//! 2. Every key a read emits resolves back to the (Target, Name) it came from — the round trip.
//! 3. The generated ObjectScript actually writes `tS.Target`. The parser and payload tests are fed
//!    hand-written marker text and cannot see the ObjectScript at all, which is why the wire needs
//!    an assertion of its own: `production_item_batch_settings.rs` records that a mutation
//!    restoring `FindItemByConfigName` passed every other test in that file.
//!
//! An unrecognised Target is NAMED, never folded into Host. Whether `Target` can hold anything
//! besides `Host` and `Adapter` is not documented in the InterSystems corpus available here, so a
//! default would be a guess — and the guess would silently move a setting between objects.

use iris_agentic_dev_core::tools::interop::{
    build_get_settings_batch_code, item_settings_payload, parse_get_settings_batch,
    qualify_setting_key, resolve_setting_target,
};

/// The headline defect. Two rows, two targets, one name: before the fix both arrived as names
/// `Adapter:Port` and `Host:Port` with no target field, collapsed to one key, and neither key below
/// existed.
#[test]
fn a_host_and_an_adapter_setting_of_one_name_keep_both_keys() {
    let b = parse_get_settings_batch(
        "PI_ITEM:A\nPI_NSET:2\nPI_SET:Adapter:Port=8080\nPI_SET:Host:Port=443\n",
    );
    assert!(
        b.items[0].1.complete(),
        "both rows arrived — the transport lost nothing: {:?}",
        b.items[0].1
    );
    let v = item_settings_payload("A", &b.items[0].1);

    // The adapter setting is qualified, because that is what `set_settings` needs to reach the
    // adapter. The host setting keeps its BARE name — `Port` does not begin with a known prefix,
    // so it cannot be misread, and leaving it alone is what keeps most keys byte-identical to what
    // this tool returned before Target was on the wire.
    assert_eq!(v["settings"]["Adapter.Port"], "8080", "{v}");
    assert_eq!(v["settings"]["Port"], "443", "{v}");

    // The whole point: two rows, two keys. While Target was dropped this was two rows and one key,
    // and `duplicate_setting_names` reported the loss. Now there is no loss to report.
    assert!(
        v.get("duplicate_setting_names").is_none(),
        "a Host and an Adapter setting of one name must no longer collapse: {v}"
    );
    assert!(
        v.get("settings_unknown_target").is_none(),
        "Host and Adapter are both recognised: {v}"
    );
    assert_eq!(
        v["settings"].as_object().map(|o| o.len()),
        Some(2),
        "both settings must be in the map, not one of them: {v}"
    );
}

/// The load-bearing property, and the only assertion that distinguishes this fix from prefixing
/// adapter keys alone: every key a read emits resolves back to the target it came from.
#[test]
fn every_read_key_resolves_back_to_the_target_it_came_from() {
    // `Adapter.DSN` and `Host.Port` are not invented. The pre-#212 bug hard-coded Target="Host"
    // for every key, so it created in the field a HOST setting literally named `Adapter.DSN`.
    let names = [
        "Port",
        "FilePath",
        "ReplyCodeActions",
        "Adapter.DSN",
        "Host.Port",
    ];
    let mut checked = 0;
    for target in ["Host", "Adapter"] {
        for name in names {
            let key = qualify_setting_key(target, name);
            assert_eq!(
                resolve_setting_target(&key),
                (target, name),
                "a read emitted {key:?} for target {target:?} name {name:?}, and writing that key \
                 back would address something else"
            );
            checked += 1;
        }
    }
    // CONTROL: the loop ran every case. A `for` over a mistyped array is satisfied by zero
    // iterations, and a round-trip test that checks nothing is the one that always passes.
    assert_eq!(checked, 10, "expected 2 targets x {} names", names.len());

    // The exact case the conditional Host prefix exists for, spelled out so the reason survives.
    // `resolve_setting_target` is untouched by this change: it has always read a leading
    // `Adapter.` as the target, which is why a HOST setting named `Adapter.DSN` cannot be emitted
    // bare — that key reads back as target Adapter, name DSN. A silent move of the setting.
    assert_eq!(resolve_setting_target("Adapter.DSN"), ("Adapter", "DSN"));
    assert_eq!(
        qualify_setting_key("Host", "Adapter.DSN"),
        "Host.Adapter.DSN"
    );

    // CONTROL for the other direction: an ordinary host setting is NOT qualified, or the mapping
    // above is satisfied by prefixing everything and the low-churn claim is false.
    for name in [
        "Port",
        "AlertOnError",
        "ReplyCodeActions",
        "BusinessRuleName",
    ] {
        assert_eq!(
            qualify_setting_key("Host", name),
            name,
            "an ordinary host setting's key must be byte-identical to the pre-fix output"
        );
    }
}

/// The wire. Every other test in this file and in `production_item_batch_settings.rs` is fed
/// hand-written marker text, so none of them can tell whether the generated ObjectScript writes
/// the target at all.
#[test]
fn the_generated_program_writes_the_target_on_the_wire() {
    let items = vec!["A".to_string(), "B".to_string()];
    let code = build_get_settings_batch_code("P", &items);
    // The ORDER is the contract: target, `:`, name, `=`, value. That is what the parser splits,
    // and it is why the value may contain both `:` and `=` while the key may contain neither.
    let emission = "_tS.Target_\":\"_tS.Name_\"=\"_tS.Value_";
    assert!(
        code.contains(emission),
        "the per-setting write must carry Target first, or get_settings output cannot be fed back \
         into set_settings for any adapter setting:\n{code}"
    );
    // CONTROL: one such write per item asked for, not one for the whole program.
    assert_eq!(
        code.matches(emission).count(),
        items.len(),
        "expected one setting write per item:\n{code}"
    );
}

/// An unrecognised `Setting.Target` is reported by name. Folding it into Host would emit a key that
/// reads back as a host setting, and writing it back would move the setting — a failure answered
/// with a fact.
#[test]
fn an_unrecognised_target_is_named_not_folded_into_host() {
    let b = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:1\nPI_SET:Gateway:DSN=pg\n");
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(v["settings_unknown_target"][0]["name"], "DSN", "{v}");
    assert_eq!(v["settings_unknown_target"][0]["target"], "Gateway", "{v}");
    // The key is the bare name — neither `Host.DSN` nor `Gateway.DSN`. This build does not know
    // what a `Gateway` target means, and inventing a prefix would hand the caller a key to write
    // back that addresses something this build chose for them.
    assert_eq!(v["settings"]["DSN"], "pg", "{v}");
    assert!(
        v["settings"].get("Host.DSN").is_none() && v["settings"].get("Gateway.DSN").is_none(),
        "an unknown target must not be given an invented prefix: {v}"
    );

    // CONTROL: a target this build DOES recognise must not be reported as unknown, or the field
    // above is satisfied by a payload that flags every setting.
    let known = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:1\nPI_SET:Adapter:DSN=pg\n");
    let kv = item_settings_payload("A", &known.items[0].1);
    assert!(
        kv.get("settings_unknown_target").is_none(),
        "Adapter is a recognised target: {kv}"
    );
    assert_eq!(kv["settings"]["Adapter.DSN"], "pg", "{kv}");
}

/// The pre-fix wire shape, which an older server build still speaks. It must not silently
/// half-work: the name and value are still recovered, but nothing claims to know which object the
/// setting belongs to, because nothing on the line said.
#[test]
fn a_line_with_no_target_field_is_not_read_as_a_host_setting() {
    let b = parse_get_settings_batch("PI_ITEM:A\nPI_NSET:1\nPI_SET:Port=8080\n");
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(
        v["settings"]["Port"], "8080",
        "the name and value are still on the line and must still be read: {v}"
    );
    assert_eq!(
        v["settings_unknown_target"][0]["name"], "Port",
        "a line with no target field must be reported, not assumed: {v}"
    );
    assert_eq!(
        v["settings_unknown_target"][0]["target"], "",
        "an absent target is reported as absent — reporting it as Host would be this build \
         inventing the one fact the line did not carry: {v}"
    );
}

/// The value is allowed to contain the delimiter. A real production carries
/// `ReplyCodeActions=:?R=F,:?E=F,…`, which is why the `=` is split BEFORE the `:`.
///
/// The two halves of this test are not redundant, and the first alone does not pin the claim the
/// test is named for. On a WELL-FORMED line the split order is unobservable: `Host:Name=:a:b`
/// yields the same three fields either way. The order is only observable on a line carrying NO
/// target field, where splitting on `:` first takes `ReplyCodeActions=` for the target and leaves
/// `?R=F,…` as the name — and that is exactly the line an older server build emits, so it is the
/// case the backward-compatible read has to get right.
#[test]
fn a_value_full_of_colons_does_not_become_the_target() {
    const VALUE: &str = ":?R=F,:?E=F,:~=S";

    let b = parse_get_settings_batch(&format!(
        "PI_ITEM:A\nPI_NSET:1\nPI_SET:Host:ReplyCodeActions={VALUE}\n"
    ));
    let v = item_settings_payload("A", &b.items[0].1);
    assert_eq!(v["settings"]["ReplyCodeActions"], VALUE, "{v}");
    assert!(
        v.get("settings_unknown_target").is_none(),
        "the target is `Host`, which is recognised — reading it as `ReplyCodeActions=` would \
         report an unknown target here: {v}"
    );

    // The discriminating case: the SAME value with no target field in front of it.
    let bare = parse_get_settings_batch(&format!(
        "PI_ITEM:A\nPI_NSET:1\nPI_SET:ReplyCodeActions={VALUE}\n"
    ));
    let bv = item_settings_payload("A", &bare.items[0].1);
    assert_eq!(
        bv["settings"]["ReplyCodeActions"], VALUE,
        "a targetless line's value is full of `:` — looking for the target before splitting the \
         `=` reads `ReplyCodeActions=` as the target and `?R=F,` as the name: {bv}"
    );
    assert_eq!(
        bv["settings_unknown_target"][0]["name"], "ReplyCodeActions",
        "the name must survive even though no target arrived: {bv}"
    );
}
