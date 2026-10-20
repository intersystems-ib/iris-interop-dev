//! #410 (first half): `check_config` said nothing about a config file it was ignoring.
//!
//! Registered with `IRIS_HOST`/`IRIS_WEB_PORT` and a `.iris-agentic-dev.toml` in the project folder,
//! a fresh session reports `connection_source: explicit_flag`, `config_file: null` and a
//! `config_watch_path` pointing straight at that file. Three true fields, and nothing joining them.
//!
//! ## Why the file is invisible rather than merely outranked
//!
//! `apply_workspace_config_with_path` short-circuits **before the file is read**:
//!
//! ```text
//! if explicit.is_some() {
//!     // A CLI flag outranks the file, so a broken file is not consulted and not fatal.
//!     return Ok((explicit, None));
//! }
//! ```
//!
//! It returns `None` for the path, which is why `config_file` is null instead of naming the file it
//! declined — the connection never learns the file exists. So the warning has to come from the
//! watcher's path plus a filesystem read, not from the connection.
//!
//! ## The half that actually bites
//!
//! An **edit** to that same ignored file WAS adopted on the next tool call, because `check_reload`
//! called the raw `load_workspace_config` and that loader takes no `explicit` argument, so it could
//! not honour the short-circuit. Whether a session reached the flag's instance or the file's
//! therefore depended on whether the file happened to be touched after startup.
//!
//! **That asymmetry is now closed** (see `the_file_outranks_the_environment`). The precedence is
//! decided — `--host` flag > `.iris-agentic-dev.toml` > `IRIS_*` > discovery — and `check_reload`
//! honours the flag, so the two paths agree. This file therefore no longer asserts that the
//! message stays neutral; it asserts that the message states the decided rule, and that it
//! distinguishes the TWO reasons a present file can be unused.
//!
//! ## Why this is the complement of an existing warning
//!
//! `check_config` already warns for `config_file.is_none() && !is_explicit` — fallback discovery.
//! That is one half of a partition on `is_explicit`, and only that half had a message. The sibling
//! shape again, so `the_two_warnings_are_not_interchangeable` asserts they cannot be confused.

use iris_agentic_dev_core::iris::workspace_config::{ignored_config_path, ignored_config_warning};
use std::path::Path;

const SRC: &str = "explicit_flag";

#[test]
fn a_file_at_the_watched_path_is_reported_when_it_is_not_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(".iris-agentic-dev.toml");
    std::fs::write(&f, "web_port = 45080\n").unwrap();
    let p = f.to_str().unwrap();

    assert_eq!(
        ignored_config_path(None, true, Some(p)),
        Some(p.to_string()),
        "an explicit source with a real file at the watched path must be reported"
    );
}

#[test]
fn the_existence_check_is_not_inert() {
    // The same inputs, twice, differing only in whether the file is on disk. Without the
    // negative half this test would pass against `|p| Some(p)`, which reports a file that
    // is not there — a fabrication, and the mirror image of the defect.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(".iris-agentic-dev.toml");
    let p = f.to_str().unwrap().to_string();

    assert_eq!(
        ignored_config_path(None, true, Some(&p)),
        None,
        "nothing exists there yet — the watcher is waiting, not ignoring a file"
    );
    std::fs::write(&f, "web_port = 45080\n").unwrap();
    assert_eq!(
        ignored_config_path(None, true, Some(&p)),
        Some(p.clone()),
        "the same call must change answer once the file exists"
    );
    std::fs::remove_file(&f).unwrap();
    assert_eq!(
        ignored_config_path(None, true, Some(&p)),
        None,
        "and change back when it is removed"
    );
}

#[test]
fn a_directory_at_the_watched_path_is_not_a_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join(".iris-agentic-dev.toml");
    std::fs::create_dir(&sub).unwrap();
    assert_eq!(
        ignored_config_path(None, true, sub.to_str()),
        None,
        "exists() is true for a directory; only a FILE can be an ignored config"
    );
}

#[test]
fn a_file_that_is_the_source_is_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(".iris-agentic-dev.toml");
    std::fs::write(&f, "web_port = 45080\n").unwrap();
    assert_eq!(
        ignored_config_path(Some(Path::new(f.to_str().unwrap())), true, f.to_str()),
        None,
        "the file IS the connection's source — there is nothing to report"
    );
}

#[test]
fn a_non_explicit_source_is_left_to_the_fallback_warning() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(".iris-agentic-dev.toml");
    std::fs::write(&f, "web_port = 45080\n").unwrap();
    assert_eq!(
        ignored_config_path(None, false, f.to_str()),
        None,
        "that half of the partition already has fallback_warning; two messages about one \
         connection would contradict each other"
    );
}

#[test]
fn no_watch_path_is_not_an_ignored_file() {
    assert_eq!(ignored_config_path(None, true, None), None);
}

#[test]
fn the_warning_names_the_file_and_the_source() {
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force);
        assert!(w.contains("/proj/.iris-agentic-dev.toml"), "{w}");
        assert!(
            w.contains(SRC),
            "the caller cannot act without knowing what won (flag_in_force={flag_in_force}): {w}"
        );
    }
}

#[test]
fn the_warning_says_the_file_is_not_in_use_now() {
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force);
        assert!(w.contains("NOT in use"), "{w}");
    }
}

#[test]
fn an_edit_takes_over_only_when_no_flag_is_in_force() {
    // The half that actually bites, and now it is true in exactly ONE of the two cases. Saying it
    // in both would be the original defect read backwards: a flag session is told to expect a
    // retarget that `check_reload` now refuses to perform.
    let no_flag = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, false);
    assert!(no_flag.contains("EDITED"), "{no_flag}");
    assert!(
        no_flag.contains("WILL replace this connection"),
        "a file that named no target is still watched, and adding a host IS adopted: {no_flag}"
    );
    assert!(
        no_flag.contains("namespace"),
        "the namespace moves with it, which is what silently sends writes elsewhere: {no_flag}"
    );

    let flag = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, true);
    assert!(
        flag.contains("Editing the file will NOT replace this connection"),
        "a real --host outranks the file on reload too, so promising a takeover would be false: \
         {flag}"
    );
    assert!(
        !flag.contains("WILL replace this connection"),
        "the two branches must not both claim a takeover: {flag}"
    );
}

#[test]
fn the_no_target_branch_says_which_keys_choose_an_instance() {
    // Without this the reader is told their file is unused and not what to change — and the
    // reported file was unused for exactly this reason: `web_port` alone chooses nothing.
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, false);
    assert!(w.contains("names no instance to connect to"), "{w}");
    assert!(
        w.contains("host") && w.contains("container"),
        "name the two keys that choose a target: {w}"
    );
    assert!(
        w.contains("web_port"),
        "name the key the reporter actually had, or they will not recognise their file: {w}"
    );
}

#[test]
fn the_warning_does_not_claim_the_file_is_absent() {
    // This is precisely what the sibling fallback_warning says, and saying it here would be
    // false: the file is right there.
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force)
            .to_lowercase();
        for wrong in ["no .iris-agentic-dev.toml", "not found", "no config file"] {
            assert!(!w.contains(wrong), "the file EXISTS ({wrong}): {w}");
        }
    }
}

#[test]
fn the_warning_names_a_deterministic_way_out() {
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force);
        assert!(
            w.contains("delete the file") || w.contains("drop the --host flag"),
            "a warning names a way forward (#329, flag_in_force={flag_in_force}): {w}"
        );
    }
}

#[test]
fn the_warning_states_the_decided_precedence() {
    // The inverse of this file's original `the_warning_does_not_pick_a_precedence`, which existed
    // because #410's second half was an open decision. It is decided (B: the file outranks IRIS_*,
    // a real flag outranks both), so the message must now say which rule holds — and must not
    // keep telling the reader the outcome is unpredictable, which was only true of the defect.
    let flag = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, true);
    assert!(
        flag.contains("outranks the file everywhere")
            && flag.contains("at startup and on reload alike"),
        "the flag branch states the rule that now holds in both paths: {flag}"
    );
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force);
        assert!(
            !w.contains("do not rely on which of the two currently wins"),
            "the precedence is no longer undecided: {w}"
        );
    }
}

#[test]
fn the_two_warnings_are_not_interchangeable() {
    for flag_in_force in [true, false] {
        let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC, flag_in_force);
        assert!(
            !w.contains("fallback discovery"),
            "this connection was NOT discovered — it was explicitly named: {w}"
        );
        assert!(
            !w.to_lowercase().contains("docker/port scan"),
            "that is the other branch's diagnosis: {w}"
        );
    }
}
