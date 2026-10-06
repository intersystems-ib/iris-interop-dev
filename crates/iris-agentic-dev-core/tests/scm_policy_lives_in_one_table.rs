//! #418 §5: whether a source-control action is refused by default is decided in ONE table, and the
//! pre-write probe does not treat a failed check as permission to write.
//!
//! ## What went wrong before
//!
//! `CheckIn` was refused unless `IRIS_SCM_ALLOW_CHECKIN` was set, and that was decided **twice** —
//! once in the `menu` arm, which filtered it out of the offered actions, and once in the `execute`
//! arm, which refused it. Each read the variable itself and compared against `ScmAction::CheckIn`
//! by name. §5 asks for the same treatment for `%Disconnect` / `%Reconnect`, and adding them to
//! both sites would have made **four** copies of one policy — the shape §2/§4 of the same issue was
//! about one layer down, where three sites answered one question three different ways.
//!
//! So the policy is `ScmAction::opt_in`, exhaustive with no `_` arm, and both sites ask
//! `opt_in_refusal`. These tests keep it that way: a guard on the TABLE's behaviour lives next to
//! it in `scm.rs`, and what cannot be asserted there — that no site goes back to reading the
//! variable itself — is asserted here, against the source, because that is where a duplicate
//! would be.

use std::path::PathBuf;

fn src(file: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/tools");
    p.push(file);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Production code only: every `#[cfg(test)]` module removed by brace matching, then `//` lines
/// dropped. Both halves matter here — the test modules set these variables on purpose, and the
/// `menu` arm's comment NAMES the variable while explaining why it no longer reads it. A guard
/// that counted either would be measuring its own documentation.
///
/// Brace-matched rather than truncated at the first `#[cfg(test)]`: that is correct for `scm.rs`,
/// whose test module is one trailing block, and wrong for `doc.rs`, which has SEVEN interleaved
/// ones — so truncating dropped most of the file, including the probe this file is about. The size
/// control below is what caught it.
fn production(file: &str) -> String {
    let raw = src(file);
    let mut body = raw.clone();
    while let Some(at) = body.find("#[cfg(test)]") {
        let Some(open_rel) = body[at..].find('{') else {
            body.truncate(at);
            break;
        };
        let open = at + open_rel;
        let mut depth = 0usize;
        let mut end = body.len();
        for (i, c) in body[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        body.replace_range(at..end, "");
    }
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // CONTROL: stripping did not eat the file, and it really did remove the test module.
    assert!(
        code.len() > raw.len() / 4,
        "{file}: stripped to {} of {} bytes — the window is wrong",
        code.len(),
        raw.len()
    );
    assert!(
        !code.contains("#[cfg(test)]"),
        "{file}: the test module is still in the window"
    );
    code
}

/// The body of `fn NAME(` through its matching brace.
fn fn_body(text: &str, name: &str) -> String {
    let at = text
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("fn {name} is gone"));
    let open = text[at..].find('{').expect("a body") + at;
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return text[open..open + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("fn {name} has no matching brace");
}

/// The variable names live in the table and nowhere else.
#[test]
fn no_site_reads_the_opt_in_variable_for_itself() {
    let code = production("scm.rs");
    let table = fn_body(&code, "opt_in");

    let total = code.matches("IRIS_SCM_ALLOW").count();
    let in_table = table.matches("IRIS_SCM_ALLOW").count();
    // CONTROL FIRST: the table really is what was found, so the equality below is not 0 == 0.
    assert!(
        in_table >= 2,
        "ScmAction::opt_in names {in_table} opt-in variables; expected at least CheckIn's and the \
         administrative pair's:\n{table}"
    );
    assert_eq!(
        total,
        in_table,
        "{} occurrence(s) of IRIS_SCM_ALLOW sit outside ScmAction::opt_in. A second reader is how \
         the menu arm and the execute arm came to disagree about CheckIn — and a site that is \
         MISSING from one copy is an action offered and then refused, or worse, refused in the \
         menu and allowed by execute.",
        total - in_table
    );
    // And nothing reads the environment for this by hand any more.
    assert!(
        !code.contains("std::env::var(\"IRIS_SCM"),
        "a site reads an IRIS_SCM_* variable directly instead of going through env_truthy"
    );
}

/// The menu must offer exactly what `execute` will accept. Asserted at the source because driving
/// either arm needs a live provider; what is checkable without one is that both consult the same
/// function.
#[test]
fn the_menu_and_execute_consult_the_same_policy() {
    let code = production("scm.rs");
    let menu = {
        let a = code.find("\"menu\" => {").expect("the menu arm");
        let b = code[a..].find("\"checkout\" => {").expect("the next arm") + a;
        &code[a..b]
    };
    let exec = {
        let a = code.find("\"execute\" => {").expect("the execute arm");
        let b = code[a..]
            .find("other => err_json(")
            .expect("the fallthrough")
            + a;
        &code[a..b]
    };
    for (name, w) in [("menu", menu), ("execute", exec)] {
        assert!(
            w.contains("opt_in_refusal("),
            "the {name} arm does not consult opt_in_refusal, so it decides the policy itself"
        );
        assert!(
            !w.contains("ScmAction::CheckIn"),
            "the {name} arm still names CheckIn directly, which is the duplicate #418 §5 removed"
        );
    }
    // CONTROL: the two windows are distinct and neither swallowed the other.
    assert!(
        menu.len() > 100 && exec.len() > 100,
        "a window is implausibly small"
    );
    assert!(
        !menu.contains("\"execute\" => {"),
        "the menu window ran into execute"
    );
}

/// `opt_in` must have no catch-all, so a variant added to `ScmAction` cannot compile until someone
/// has decided whether it needs an operator's consent. Same construction as `is_write`.
#[test]
fn opt_in_has_no_catch_all_arm() {
    let table = fn_body(&production("scm.rs"), "opt_in");
    let arms: Vec<&str> = table
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("=>"))
        .collect();
    assert!(!arms.is_empty(), "no arms found in opt_in:\n{table}");
    for arm in &arms {
        let pat = arm.split("=>").next().unwrap().trim();
        let pat = pat.trim_start_matches('|').trim_end_matches('|').trim();
        assert!(
            pat.split('|')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .all(|p| p.starts_with("Self::")),
            "`{arm}` is a catch-all or binding pattern, so a new ScmAction variant would inherit \
             its opt-in silently"
        );
    }
    // CONTROL: the arms really were read, and the two gated families are both there.
    assert!(table.contains("Self::CheckIn"), "{table}");
    assert!(table.contains("Self::Disconnect"), "{table}");
}

/// #418: a pre-write source-control check that could not RUN is not a document that needs no
/// checkout. This was `if let Ok(out) = …`, so a transport failure, a 401 or a timeout fell through
/// and the write went ahead — the same defect #342 fixed at the sibling call site.
#[test]
fn a_failed_pre_write_probe_does_not_become_permission_to_write() {
    let code = production("doc.rs");
    let a = code
        .find("let scm_check = scm_precheck_code(")
        .expect("the probe call site");
    let b = code[a..]
        .find("let result = do_write(")
        .expect("the write that follows it")
        + a;
    let w = &code[a..b];
    // CONTROL: the window is the probe block and not half the file.
    assert!(
        w.len() > 200 && w.len() < code.len() / 8,
        "the window is {} of {} bytes — not one block",
        w.len(),
        code.len()
    );
    assert!(
        w.contains("execute_via_generator"),
        "the window does not contain the probe call:\n{w}"
    );
    assert!(
        !w.contains("if let Ok("),
        "the probe is read inside `if let Ok(`, so a failure skips the block and the write \
         proceeds with the checkout state UNKNOWN:\n{w}"
    );
    assert!(w.contains("Err(e) =>"), "the probe has no Err arm:\n{w}");
    // The Err arm must refuse rather than fall through: it returns, and it returns an error.
    let err_at = w.find("Err(e) =>").unwrap();
    let arm = &w[err_at..];
    let arm = &arm[..arm.find("Ok(out)").unwrap_or(arm.len())];
    assert!(
        arm.contains("return err_json("),
        "the Err arm does not refuse the write:\n{arm}"
    );
}
