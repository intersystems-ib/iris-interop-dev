//! #334: a new document or class file must never be silently untracked.
//!
//! `.gitignore` used to carry blanket `*.md` and `*.cls` rules. They swallowed CLAUDE.md when that
//! file was added (#310): `git add -A` skipped it without a word, and the only reason anyone noticed
//! was the commit diff not matching the commit message. Tracked docs survived only because ignore
//! rules do not apply to already-tracked paths, so every NEW doc needed an `!` exception or a `-f`.
//!
//! That is the house rule's own failure shape (CLAUDE.md: a failure must never be answered with a
//! negative fact) in the one place where the "answer" is a file quietly not existing for anybody
//! else. `git add` reports success; the file is simply absent from the commit.
//!
//! ## Why this asserts the PROPERTY, not the rule text
//!
//! Asserting that the literal strings `*.md` and `*.cls` are absent would pass while an equivalent
//! rule spelled differently (`**/*.md`, `*.[mM][dD]`, a `docs/*.md` narrowed variant) reintroduced
//! the trap. What matters to a contributor is whether the file they just wrote will be committed, so
//! the test asks git that question directly, about paths shaped like the documents this repo keeps.
//!
//! `docs/*.md` was removed for the same reason, narrower: `docs/` has no tracked files, and
//! `skills/README.md` links to `docs/CONTRIBUTING.md`, which is absent AND unreachably ignored.
//! Whether that document was written and silently dropped or never written cannot be determined from
//! history — the point is that the rule made the second indistinguishable from the first.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p
}

/// What git says about one path. Three states on purpose: a `git check-ignore` that FAILS must not
/// be read as "not ignored", which is the very defect this file exists to prevent.
#[derive(Debug, PartialEq)]
enum Ignored {
    Yes,
    No,
    /// git could not answer — no repo, no git binary, an unexpected status.
    Unknown(String),
}

fn is_ignored(root: &Path, rel: &str) -> Ignored {
    // --no-index so the answer is about the RULES, not about whether the path happens to be tracked
    // already. Without it every existing doc answers "not ignored" for the wrong reason, and the
    // test would pass even with the blanket rule restored.
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--no-index", "-q", rel])
        .output();
    match out {
        Err(e) => Ignored::Unknown(format!("could not run git: {e}")),
        Ok(o) => match o.status.code() {
            Some(0) => Ignored::Yes,
            Some(1) => Ignored::No,
            other => Ignored::Unknown(format!(
                "git check-ignore exited {other:?}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
        },
    }
}

/// Paths shaped like documents this repo keeps. None may be ignored.
const MUST_BE_TRACKABLE: &[&str] = &[
    "CLAUDE.md",
    "README.md",
    "NEWDOC.md",
    "docs/CONTRIBUTING.md",
    "docs/some-new-note.md",
    "skills/objectscript-guardrails/SKILL.md",
    "light-skills/AGENTS.md",
    "crates/iris-agentic-dev-core/src/tools/NOTES.md",
    "benchmark/021/README.md",
    // .cls, not just .md. A mutation restoring the blanket `*.cls` left this test GREEN while only
    // the textual guard caught it, because every path above is markdown — a surviving mutant naming
    // a missing assertion.
    "crates/iris-agentic-dev-core/tests/fixtures/New.cls",
    // The anchoring cases. Unanchored, `MyApp/` and `src/` match at EVERY depth, so these two were
    // ignored while their directories held tracked files: a new fixture beside
    // fixtures/MyApp/Foo.cls was silently skipped by `git add -A`.
    "crates/iris-agentic-dev-core/tests/fixtures/MyApp/NewFixture.cls",
    "skills/src/Sample.cls",
    "benchmark/021/specs/notes.md",
];

/// Paths that MUST stay ignored. Without these the test would pass against an empty `.gitignore`,
/// which is the same clean-zero trap in a different coat.
const MUST_STAY_IGNORED: &[&str] = &[
    // The REAL binary name, not a placeholder. `coverage_script_names` asserts that every
    // `target/debug/<name>` literal in the repo names a binary this workspace actually builds —
    // a placeholder here is the same staleness the rename left in every launch.json (#317).
    "target/debug/iris-interop-dev",
    "some.log",
    "coverage.profraw",
    "benchmark/021/results/run-1/scores.json",
    // The ROOT scratch names must keep working after anchoring — otherwise "anchor them" would have
    // quietly become "delete them", and this test would applaud.
    "MyApp/Scratch.cls",
    "src/scratch.rs",
    "Bench/whatever",
    "specs/draft.md",
];

#[test]
fn documentation_shaped_paths_are_not_ignored() {
    let root = repo_root();
    let mut offenders = Vec::new();
    let mut unknown = Vec::new();
    for rel in MUST_BE_TRACKABLE {
        match is_ignored(&root, rel) {
            Ignored::Yes => offenders.push(*rel),
            Ignored::No => {}
            Ignored::Unknown(why) => unknown.push(format!("{rel}: {why}")),
        }
    }
    // A guard that cannot ask the question must FAIL, not report a clean scan.
    assert!(
        unknown.is_empty(),
        "git could not answer for these paths, so this test proved nothing:\n  {}",
        unknown.join("\n  ")
    );
    assert!(
        offenders.is_empty(),
        "these document-shaped paths are ignored, so writing one and running `git add -A` would \
         skip it in silence:\n  {}\nAdd scratch patterns by name or directory, never by extension.",
        offenders.join("\n  ")
    );
    eprintln!(
        "document-shaped paths checked and trackable: {}",
        MUST_BE_TRACKABLE.len()
    );
}

#[test]
fn the_ignore_check_can_still_detect_an_ignored_path() {
    // POSITIVE CONTROL for the test above. If `git check-ignore` stopped reporting anything as
    // ignored — wrong cwd, no repo, a flag change — the assertion above would pass vacuously and
    // read exactly like a correctly configured tree.
    let root = repo_root();
    let mut not_ignored = Vec::new();
    let mut unknown = Vec::new();
    for rel in MUST_STAY_IGNORED {
        match is_ignored(&root, rel) {
            Ignored::Yes => {}
            Ignored::No => not_ignored.push(*rel),
            Ignored::Unknown(why) => unknown.push(format!("{rel}: {why}")),
        }
    }
    assert!(
        unknown.is_empty(),
        "git could not answer, so the control proved nothing:\n  {}",
        unknown.join("\n  ")
    );
    assert!(
        not_ignored.is_empty(),
        "these should be ignored and are not — either a rule was dropped, or (worse) the instrument \
         in `documentation_shaped_paths_are_not_ignored` is not actually asking git anything:\n  {}",
        not_ignored.join("\n  ")
    );
    eprintln!(
        "control paths confirmed still ignored: {}",
        MUST_STAY_IGNORED.len()
    );
}

#[test]
fn no_rule_ignores_a_whole_documentation_extension() {
    // Complements the behavioural tests with a cheap textual check on the SHAPE of new rules, since
    // the behavioural ones only cover the paths they happen to list. Comments are stripped: the
    // replacement comment in .gitignore necessarily names `*.md` and `*.cls` to explain why they are
    // gone, and a guard that greps prose would fire on its own rationale — that has happened twice
    // in this repo.
    let gi = repo_root().join(".gitignore");
    let text = std::fs::read_to_string(&gi).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — refusing to report a clean scan",
            gi.display()
        )
    });
    let rules: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert!(
        rules.len() >= 10,
        "parsed only {} rules from .gitignore — the scan broke rather than the file being empty",
        rules.len()
    );
    let blanket: Vec<&str> = rules
        .iter()
        .filter(|l| {
            let body = l.trim_start_matches('!');
            // A rule whose only wildcard leads straight into a doc extension swallows every such
            // file in its scope, wherever that scope is.
            for ext in ["md", "cls"] {
                let suffix = format!(".{ext}");
                if body.ends_with(&suffix) && body.trim_end_matches(&suffix).contains('*') {
                    return true;
                }
            }
            false
        })
        .copied()
        .collect();
    assert!(
        blanket.is_empty(),
        "these rules ignore a whole documentation extension; name the scratch files or their \
         directory instead: {blanket:?}"
    );
    eprintln!("gitignore rules checked: {}", rules.len());
}
