//! #379: every config-item lookup resolves from the CONFIG, not from the runtime dispatch index.
//!
//! `FindItemByConfigName` resolves through `^Ens.Runtime("DispatchName")`, which is only populated
//! once a production has been started or updated. This fork's workflow is to configure a production's
//! items and then run tests against it, so the items of a production being BUILT are exactly the ones
//! it cannot find — and the `%Status` it sets carries no text, so there was nothing to report.
//!
//! Measured on IRIS 2026.1, a production configured, compiled and never started:
//!
//! ```text
//! tProd.Items.Count()                         -> 1, the name present
//! tProd.FindItemByConfigName("Dup.BO.Target")  -> NO object, ERROR #00: (no error description)
//! ^Ens.Runtime("DispatchName")                -> does not exist, 0 entries
//! walking tProd.Items and matching .Name      -> resolves it
//! ```
//!
//! Driven through the real tool against that production, before the fix:
//!
//! | action | result |
//! |---|---|
//! | `enable` on an item that exists | `ITEM_NOT_FOUND`, and the message then lists that very item |
//! | `set_settings` on an item that exists | the same |
//! | `add` of an item that ALREADY exists | **`success: true`, and `Ens_Config.Item` then held TWO rows of that name** |
//!
//! That last row is why this file exists rather than three separate edits: the duplicate guard is an
//! EXISTENCE test, so the same broken lookup fails in the OPPOSITE direction and corrupts the
//! production config instead of refusing. A fix applied to two of the three would leave the third
//! looking more trustworthy than it is.

use iris_agentic_dev_core::tools::interop::{
    build_add_item_code, build_get_settings_batch_code, build_set_settings_code, ITEM_WALK_MARKER,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn settings() -> HashMap<String, String> {
    HashMap::from([("Adapter.Port".to_string(), "9999".to_string())])
}

/// Every generator that resolves an item by name must walk the config items.
#[test]
fn every_item_lookup_walks_the_config_items() {
    // Each entry carries the item NAMES it was built for, so the comparison can be checked against
    // them rather than against the bare text `.Name=` — a mutation to `.Name=""` satisfied that and
    // survived, while producing a walk that matches nothing.
    let programs: Vec<(&str, String, Vec<&str>)> = vec![
        (
            "add",
            build_add_item_code("P", "My.Item", "Some.Class", true, None, None, &settings()),
            vec!["My.Item"],
        ),
        (
            "set_settings",
            build_set_settings_code("P", "My.Item", &settings(), false),
            vec!["My.Item"],
        ),
        (
            "get_settings",
            build_get_settings_batch_code("P", &["A".to_string(), "B".to_string()]),
            vec!["A", "B"],
        ),
    ];
    for (name, code, items) in &programs {
        assert!(
            code.contains(ITEM_WALK_MARKER),
            "{name} does not walk the config items, so on a production that has never been started \
             it cannot resolve an item that is there:\n{code}"
        );
        assert!(
            !code.contains("FindItemByConfigName"),
            "{name} still resolves through the runtime dispatch index:\n{code}"
        );
        // The walk must compare against the ITEM ASKED FOR, and keep the matched item.
        // On the WALK LINE, not anywhere in the program. `add` also emits `Set tItem.Name="My.Item"`
        // when it creates the item, and a whole-program `contains` was satisfied by that assignment
        // while the walk itself compared against "" — the mutation survived on a false witness.
        let walk_lines: Vec<&str> = code
            .lines()
            .filter(|l| l.contains(ITEM_WALK_MARKER))
            .collect();
        assert_eq!(
            walk_lines.len(),
            items.len(),
            "{name}: expected one walk per item asked for, found {}:\n{code}",
            walk_lines.len()
        );
        for (line, item) in walk_lines.iter().zip(items) {
            let needle = format!(".Name=\"{item}\"");
            assert!(
                line.contains(&needle),
                "{name}'s walk does not compare Name against {item:?} — a comparison against \
                 anything else matches nothing and reproduces the defect this replaced. Looked for \
                 {needle:?} in the walk line:\n{line}"
            );
            assert!(
                line.contains("tProd.Items.GetAt("),
                "{name}'s walk line does not read the config items:\n{line}"
            );
        }
    }
    // CONTROL: three programs were actually built and inspected.
    assert_eq!(programs.len(), 3);
    assert!(
        programs.iter().all(|(_, c, _)| c.len() > 100),
        "a program came back suspiciously short, so the assertions above read almost nothing"
    );
}

/// `add`'s guard is an EXISTENCE test, and its failure direction is the dangerous one: a lookup that
/// finds nothing means "no duplicate" and the write proceeds. Measured: two rows of the same name.
#[test]
fn the_duplicate_guard_tests_existence_through_the_config_walk() {
    let code = build_add_item_code("P", "My.Item", "Some.Class", true, None, None, &settings());
    assert!(code.contains(ITEM_WALK_MARKER), "{code}");
    assert!(
        code.contains("ITEM_EXISTS"),
        "the duplicate refusal must still be there: {code}"
    );
    // The guard must test the variable the walk fills, not the call it replaced.
    assert!(
        code.contains("If $IsObject(tDupe)"),
        "the guard must read the walk's own result: {code}"
    );
    // And it must come BEFORE the item is created, or the refusal is decoration.
    let guard_at = code.find("ITEM_EXISTS").expect("the guard");
    let create_at = code
        .find("%New()")
        .or_else(|| code.find("Items.Insert"))
        .expect("the creation");
    assert!(
        guard_at < create_at,
        "the duplicate guard runs after the item is created:\n{code}"
    );
}

/// The enable/disable arm is built inline in the dispatcher rather than by a named function, so it is
/// checked at the source. The window is the arm itself, not the file.
#[test]
fn the_enable_disable_arm_walks_the_config_items_too() {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools/interop.rs");
    let src = std::fs::read_to_string(&p).expect("interop.rs");
    let at = src
        .find("\"enable\" | \"disable\" => {")
        .expect("the enable/disable arm must be findable");
    let rest = &src[at..];
    // To the end of that match arm: the next arm at the same indentation.
    let end = rest[1..]
        .find("\n        \"")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    let arm = &rest[..end];
    assert!(
        arm.contains(ITEM_WALK_MARKER),
        "the enable/disable arm still resolves through the runtime index:\n{arm}"
    );
    assert!(!arm.contains("FindItemByConfigName"), "{arm}");
    // CONTROLS: the window is one arm, and it is the right one.
    assert!(
        arm.len() < src.len() / 4,
        "the arm window is {} of {} bytes — not one match arm",
        arm.len(),
        src.len()
    );
    assert!(arm.contains("Set tItem.Enabled="), "wrong arm: {arm}");
    assert!(
        !arm.contains("\"get_settings\" => {"),
        "the window ran into the next arm"
    );
}

/// No generated ObjectScript anywhere in the file may use the runtime-index lookup. This is the
/// population check — the two above would pass while a fourth site kept the old call.
#[test]
fn no_generated_objectscript_resolves_an_item_through_the_runtime_index() {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools/interop.rs");
    let src = std::fs::read_to_string(&p).expect("interop.rs");
    // Only lines that are generated CODE, not prose about it: a doc comment naming the method is the
    // commonest false witness in a guard like this.
    let offenders: Vec<(usize, String)> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            // `tProd.` is what makes it a CALL on the production object. Prose mentions the method
            // by name — the MISSING_PARAMETER message used to — and a guard that flags prose is one
            // that gets loosened until it catches nothing.
            !t.starts_with("//")
                && !t.starts_with("///")
                && l.contains("tProd.FindItemByConfigName")
        })
        .map(|(i, l)| (i + 1, l.trim().chars().take(90).collect()))
        .collect();
    assert!(
        offenders.is_empty(),
        "these generate a runtime-index lookup: {offenders:?}"
    );
    // CONTROL: the file IS being read and DOES still discuss the method in prose, so an empty
    // result means the code is clean rather than the scan reading nothing.
    assert!(
        src.contains("FindItemByConfigName"),
        "the doc comments explaining why this is avoided have gone too — is this the right file?"
    );
    // CONTROL for the narrowing: the predicate must still flag a real call.
    assert!(
        "Set tItem=tProd.FindItemByConfigName(x)".contains("tProd.FindItemByConfigName"),
        "the narrowed predicate no longer matches the call it exists to forbid"
    );
    assert!(
        src.contains(ITEM_WALK_MARKER),
        "and the walk must be present"
    );
}
