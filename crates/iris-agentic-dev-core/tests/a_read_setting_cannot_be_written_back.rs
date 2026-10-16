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
    qualify_setting_key, resolve_setting_target, SettingKey,
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
            // Both recognised targets must yield a WRITABLE key. The variant is half the claim:
            // the round trip below is vacuous if the key never reaches a caller.
            let key = match qualify_setting_key(target, name) {
                SettingKey::Writable(k) => k,
                other => panic!(
                    "target {target:?} name {name:?} must yield a writable key — both targets are \
                     in the write side's vocabulary, got {other:?}"
                ),
            };
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
        SettingKey::Writable("Host.Adapter.DSN".into())
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
            SettingKey::Writable(name.into()),
            "an ordinary host setting's key must be byte-identical to the pre-fix output"
        );
    }
}

/// #373 review, the should_fix: the sibling of the conditional `Host.` guard.
///
/// The guard exists because a bare key whose own name begins `Adapter.`/`Host.` is re-read by
/// `resolve_setting_target` as a PREFIX. The first pass applied it on the `Host` arm only, and the
/// `Unknown` arm — which also covers an absent target field — returned the bare name
/// unconditionally, so it emitted exactly the key the guard was written to prevent. `Host` has a
/// prefix to escape with; an unrecognised target has none, so the honest answer is no key at all.
#[test]
fn an_unknown_target_is_never_handed_a_recognised_target_s_key() {
    // Measured on the first pass: both of these returned the bare `"Adapter.DSN"`.
    for target in ["", "Gateway", "  ", "Adapter.Nested", "Host2"] {
        for name in ["Adapter.DSN", "Host.Port", "Adapter.", "Host."] {
            assert_eq!(
                qualify_setting_key(target, name),
                SettingKey::NoKey,
                "target {target:?} is outside the write side's vocabulary and name {name:?} reads \
                 back as a prefix, so no key can address this row — emitting one addresses a \
                 DIFFERENT row"
            );
        }
    }

    // CONTROL, and the reason this is `NoKey` rather than a new prefix: the bare name really is
    // read as another target's key. Without this, `NoKey` could be refusing a harmless name.
    assert_eq!(resolve_setting_target("Adapter.DSN"), ("Adapter", "DSN"));
    assert_eq!(resolve_setting_target("Host.Port"), ("Host", "Port"));

    // CONTROL in the other direction: an unknown target whose name carries NO such prefix keeps
    // its bare name, exactly as before. Without this the refusal above is satisfied by refusing
    // every unknown-target row, which would drop settings this tool has always returned.
    for name in ["DSN", "Port", "ReplyCodeActions"] {
        assert_eq!(
            qualify_setting_key("Gateway", name),
            SettingKey::Unattributed(name.into()),
            "an unknown target with an ordinary name still yields its bare name"
        );
    }

    // CONTROL that the recognised targets are untouched by this arm.
    assert_eq!(
        qualify_setting_key("Adapter", "DSN"),
        SettingKey::Writable("Adapter.DSN".into())
    );
}

/// The payload half of the same defect, and the failure the review understated: the bare key an
/// unknown-target row was given does not merely misaddress on write-back — it COLLIDES with a
/// genuine row of the target it names, and the two collapsed in the settings map with whichever
/// value `HashMap` iteration inserted last winning. A nondeterministic wrong value, reported as a
/// successful read.
#[test]
fn an_unknown_target_row_cannot_overwrite_a_genuine_adapter_setting() {
    let b = parse_get_settings_batch(
        "PI_ITEM:A\nPI_NSET:2\nPI_SET:Gateway:Adapter.DSN=from_gateway\nPI_SET:Adapter:DSN=from_adapter\n",
    );
    let v = item_settings_payload("A", &b.items[0].1);

    // The genuine adapter setting owns its key, every time. Before the fix this assertion was a
    // coin flip: measured, the map held one `Adapter.DSN` built from two different rows.
    assert_eq!(
        v["settings"]["Adapter.DSN"], "from_adapter",
        "the `Adapter`/`DSN` row owns `Adapter.DSN`; the unknown-target row must not take it: {v}"
    );
    assert_eq!(
        v["settings"].as_object().map(|o| o.len()),
        Some(1),
        "only the row with a writable key is in the map: {v}"
    );

    // The withheld row is NAMED, not dropped: raw target, null key, and its value — which this
    // entry is now the only record of.
    assert_eq!(
        v["settings_unknown_target"][0]["name"], "Adapter.DSN",
        "{v}"
    );
    assert_eq!(v["settings_unknown_target"][0]["target"], "Gateway", "{v}");
    assert_eq!(
        v["settings_unknown_target"][0]["value"], "from_gateway",
        "{v}"
    );
    assert!(
        v["settings_unknown_target"][0]["key"].is_null(),
        "no key can address this row, and a key must not be invented to fill the field: {v}"
    );
    assert_eq!(v["settings_unaddressable"], 1, "{v}");

    // The arithmetic that says nothing was lost twice or counted twice: every row that arrived is
    // in exactly one place. `duplicate_setting_names` must read ABSENT — withholding a row is not
    // a name collision, and reporting it as one would send a maintainer after the wrong defect.
    assert!(
        v.get("duplicate_setting_names").is_none(),
        "a withheld row is not a collapsed name: {v}"
    );
    assert_eq!(
        v["settings"].as_object().map(|o| o.len()).unwrap_or(0)
            + v["settings_unaddressable"].as_u64().unwrap_or(0) as usize,
        2,
        "map + unaddressable must account for both rows that arrived: {v}"
    );
}

/// The block `{ … }` opened by the `{` at `open_at`, as a byte range of its body.
///
/// Brace counting is sound on these lines: the generated markers carry no braces and `$C(10)` uses
/// parentheses, so there is no brace inside a string literal to confuse it. Asserted below by
/// requiring the block to close.
fn block_body(line: &str, open_at: usize) -> std::ops::Range<usize> {
    let b = line.as_bytes();
    assert_eq!(b[open_at], b'{', "expected a block to open at {open_at}");
    let mut depth = 0usize;
    for (i, c) in line.bytes().enumerate().skip(open_at) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return (open_at + 1)..i;
                }
            }
            _ => {}
        }
    }
    panic!("the block opened at {open_at} never closes:\n{line}");
}

/// The wire. Every other test in this file and in `production_item_batch_settings.rs` is fed
/// hand-written marker text, so none of them can tell whether the generated ObjectScript writes
/// the target at all.
///
/// #373 review, nit: this test used to pin the generated loop variable's SPELLING (`tS`), so a
/// behaviour-preserving rename reddened it — a guard forbidding its own refactor. The variable is
/// now DERIVED from the `GetNext` call and the emission built from it, so the test pins the ORDER
/// of the fields and not the name of the cursor.
///
/// It also used to assert only `contains` + a count, which cannot say the write is in the loop.
/// `build_get_settings_batch_code` keeps every `For` on one line (`build_exec_class` splits
/// generated code on `\n`), so lexical containment within that one line is checkable here and is a
/// real structural claim: the write sits inside the `For` block, inside the `Else` branch. What
/// this still cannot establish is RUNTIME reachability — no test in this repo executes the
/// generated ObjectScript — so the claim is kept to position.
#[test]
fn the_generated_program_writes_the_target_inside_the_settings_loop() {
    let items = vec!["A".to_string(), "B".to_string()];
    let code = build_get_settings_batch_code("P", &items);

    let loop_lines: Vec<&str> = code
        .lines()
        .filter(|l| l.contains(".Settings.GetNext("))
        .collect();
    // CONTROL: one walk per item asked for, not one for the whole program — so this test cannot
    // pass *because* the patch created the single match it then finds.
    assert_eq!(
        loop_lines.len(),
        items.len(),
        "expected one settings walk per item:\n{code}"
    );

    for line in &loop_lines {
        // Derive the cursor variable rather than pinning it: `Set <var>=tItem.Settings.GetNext(`.
        let get_next = line
            .find("=tItem.Settings.GetNext(.zk)")
            .unwrap_or_else(|| panic!("no settings walk on this line:\n{line}"));
        let set_kw = line[..get_next]
            .rfind("Set ")
            .unwrap_or_else(|| panic!("the walk is not a `Set`:\n{line}"));
        let var = &line[set_kw + 4..get_next];
        assert!(
            !var.is_empty() && var.chars().all(|c| c.is_alphanumeric() || c == '%'),
            "derived cursor variable {var:?} is not an identifier — the derivation, not the \
             generator, is what broke:\n{line}"
        );

        // The ORDER is the contract: target, `:`, name, `=`, value. That is what the parser
        // splits, and it is why the value may contain both `:` and `=` while the key may contain
        // neither.
        let emission = format!("_{var}.Target_\":\"_{var}.Name_\"=\"_{var}.Value_");

        // Position, by lexical containment: inside the `Else` branch, inside the `For` block.
        let else_at = line.find("Else {").expect("no Else branch") + 5;
        let else_body = block_body(line, else_at);
        let for_at = else_body.start
            + line[else_body.clone()]
                .find("For {")
                .expect("the settings walk is not in a `For` inside the Else branch")
            + 4;
        let for_body = block_body(line, for_at);
        let at = line.find(&emission).unwrap_or_else(|| {
            panic!(
                "the per-setting write must carry Target first, or get_settings output cannot be \
                 fed back into set_settings for any adapter setting:\n{line}"
            )
        });
        assert!(
            for_body.contains(&at) && for_body.contains(&(at + emission.len() - 1)),
            "the per-setting write must sit INSIDE the settings `For` block (body {for_body:?}), \
             not merely somewhere in the program: found at {at}. Hoisted above the loop it runs \
             once against an unset cursor:\n{line}"
        );
        // And the walk itself is in that same block, or the containment above is about some other
        // loop that happens to be on this line.
        assert!(
            for_body.contains(&get_next),
            "the `GetNext` cursor advance must be in the same block as the write:\n{line}"
        );
    }
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
    // `DSN` carries no prefix `resolve_setting_target` reads, so a key exists and is reported —
    // the null-key case is the sibling test. This pins which of the two this row is.
    assert_eq!(v["settings_unknown_target"][0]["key"], "DSN", "{v}");
    assert_eq!(v["settings_unknown_target"][0]["value"], "pg", "{v}");
    assert!(
        v.get("settings_unaddressable").is_none(),
        "this row has a key, so nothing was withheld from the map: {v}"
    );
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

/// A line whose key part carries no `:` at all. It must not silently half-work: the name and value
/// are still recovered, but nothing claims to know which object the setting belongs to, because
/// nothing on the line said.
///
/// #373 review, nit: this used to say "the pre-fix wire shape, which an older server build still
/// speaks". No server build speaks a different shape here — `execute_via_generator_once` PUTs a
/// fresh `IrisDevTmp.Run<uuid>` built from `build_get_settings_batch_code` on every call, so the
/// generator and this parser are always the same binary and version skew is structurally
/// impossible. A maintainer who went looking for that mechanism would find none and could delete
/// the branch as dead.
///
/// The measured reason, which is NOT an empty `Target` either: the generator writes
/// `tS.Target_":"_tS.Name`, so an empty `Target` emits `PI_SET::Name=Value` — that still carries
/// the delimiter and parses with target `""` through the ordinary arm. This arm is the defensive
/// read of a key part that arrived WITHOUT the delimiter — a malformed or truncated line. Both
/// converge on target `""`, which is the whole point: neither is read as Host.
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
/// `?R=F,…` as the name. That line is not something an older server build emits — see
/// `a_line_with_no_target_field_is_not_read_as_a_host_setting` for why no build speaks a different
/// shape — it is a malformed or truncated line, which is exactly the input a defensive parse has to
/// get right rather than attribute to Host.
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
