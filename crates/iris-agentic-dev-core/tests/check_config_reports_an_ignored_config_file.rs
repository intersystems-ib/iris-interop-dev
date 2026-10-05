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
//! An **edit** to that same ignored file IS adopted on the next tool call, because `check_reload`
//! calls the raw `load_workspace_config` and that loader takes no `explicit` argument, so it cannot
//! honour the short-circuit. Whether a session reaches the flag's instance or the file's therefore
//! depends on whether the file happened to be touched after startup. Which precedence the fork wants
//! is still open (#410); this does not decide it, and
//! `the_warning_does_not_pick_a_precedence` keeps it that way.
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
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(w.contains("/proj/.iris-agentic-dev.toml"), "{w}");
    assert!(
        w.contains(SRC),
        "the caller cannot act without knowing what won: {w}"
    );
}

#[test]
fn the_warning_says_the_file_is_not_in_use_now() {
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(w.contains("NOT in use"), "{w}");
}

#[test]
fn the_warning_says_an_edit_will_take_over() {
    // The half that actually bites, and the half a "your file is being ignored" message
    // would omit. Without it the reader concludes the file is inert and edits it freely.
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(w.contains("EDITED"), "{w}");
    assert!(
        w.contains("WILL replace this connection"),
        "an edit is adopted on the next tool call — say so: {w}"
    );
    assert!(
        w.contains("namespace"),
        "the namespace changes with it, which is the part that silently sends writes elsewhere: {w}"
    );
}

#[test]
fn the_warning_does_not_claim_the_file_is_absent() {
    // This is precisely what the sibling fallback_warning says, and saying it here would be
    // false: the file is right there.
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC).to_lowercase();
    for wrong in ["no .iris-agentic-dev.toml", "not found", "no config file"] {
        assert!(!w.contains(wrong), "the file EXISTS ({wrong}): {w}");
    }
}

#[test]
fn the_warning_names_a_deterministic_way_out() {
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(
        w.contains("delete the file") || w.contains("stop passing"),
        "a warning names a way forward (#329): {w}"
    );
}

#[test]
fn the_warning_does_not_pick_a_precedence() {
    // #410's second half is an open decision. This message must describe the asymmetry
    // without declaring either side correct, or it would document a contract nobody chose.
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(
        w.contains("do not rely on which of the two currently wins"),
        "the precedence is undecided; the warning must say so rather than imply one: {w}"
    );
}

#[test]
fn the_two_warnings_are_not_interchangeable() {
    let w = ignored_config_warning("/proj/.iris-agentic-dev.toml", SRC);
    assert!(
        !w.contains("fallback discovery"),
        "this connection was NOT discovered — it was explicitly named: {w}"
    );
    assert!(
        !w.to_lowercase().contains("docker/port scan"),
        "that is the other branch's diagnosis: {w}"
    );
}
