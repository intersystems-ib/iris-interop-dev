//! #315: a workflow whose only trigger names a branch that no longer exists can never run, and
//! nothing said so.
//!
//! `interop-fork.yml` was deleted because its entire `on:` block was:
//!
//! ```yaml
//! on:
//!   push:
//!     branches: [fork/interop-lean]
//!   pull_request:
//!     branches: [fork/interop-lean]
//! ```
//!
//! That branch was merged in June 2026 and deleted under #284, so the workflow had **0 runs, ever**
//! — measured against `ci.yml`'s 5 in the same query, so the zero was not an empty API response.
//! It looked like CI coverage in the file listing and was not.
//!
//! This is the third instance of the same class in this repo, which is why it gets a guard rather
//! than just a deletion:
//!
//! * **#292** — the bollard docker-discovery guard reported ok without ever running.
//! * **#314** — `validate-tools.sh` validated the manifest against itself, and ran in no workflow.
//! * **#315** — this one: a whole workflow that could not fire.
//!
//! THE RULE: every workflow must be manually dispatchable. `workflow_dispatch` is statically
//! decidable (unlike "does this branch exist?", which a test cannot know offline), every workflow in
//! the repo already satisfies it, and it is independently valuable — the loop that validates a branch
//! before merging depends on it, because a `pull_request` run skips the master-only `e2e-tests` job.
//! A workflow you cannot dispatch is one you cannot verify on a branch.

use std::path::{Path, PathBuf};

/// Refuse to pass on an implausible parse. If a future layout change makes `workflows_dir()` point
/// somewhere empty, this test must fail rather than report "all clear" over zero files — a guard that
/// opts out when it cannot check is the defect it exists to catch (#292).
const MIN_WORKFLOWS: usize = 3;

/// A workflow that genuinely must not be dispatchable opts out at the top of its own file, so the
/// exemption is visible where the decision lives rather than hidden in this test.
const OPT_OUT: &str = "workflow-reachability: intentionally not dispatchable";

fn workflows_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/iris-agentic-dev-core; the workflows are two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must be two levels above the crate manifest")
        .join(".github/workflows")
}

fn workflow_files() -> Vec<(String, String)> {
    let dir = workflows_dir();
    // PANIC, never skip: an unreadable workflow directory is exactly the state in which this guard
    // would otherwise silently pass.
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail, not skip.",
            dir.display()
        )
    });

    let mut out = Vec::new();
    for entry in entries {
        let path = entry.expect("directory entry").path();
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "yml" || e == "yaml");
        if !is_yaml {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("workflow filename")
            .to_string();
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        out.push((name, body));
    }
    out
}

/// Extract the `on:` block: from a line starting `on:` until the next top-level key, **with comment
/// lines removed**.
///
/// Deliberately not a YAML parse — this crate has no YAML dependency, and the question is narrow
/// enough that line scanning answers it. The block boundary is "a line whose first character is not
/// whitespace and not `#`", which is what a top-level key looks like in every workflow here.
///
/// **Comments are dropped, and that is not cosmetic.** The first version of this guard kept them, and
/// a mutation that changed `workflow_dispatch:` to `# workflow_dispatch:` — i.e. commented the
/// trigger out, the most likely way for one to actually be lost — **survived**, because
/// `contains("workflow_dispatch")` still matched the text inside the comment. A guard that a comment
/// satisfies is not a guard. `the_on_block_parse_drops_comments` pins this directly.
fn on_block(body: &str) -> Option<String> {
    let mut lines = body.lines().skip_while(|l| {
        let t = l.trim_start();
        !(t.starts_with("on:") && l.starts_with("on:"))
    });
    let first = lines.next()?;
    let mut block = String::from(first);
    for line in lines {
        let trimmed = line.trim_start();
        let starts_top_level = !line.starts_with(char::is_whitespace)
            && !trimmed.starts_with('#')
            && !line.trim().is_empty();
        if starts_top_level {
            break;
        }
        // A commented-out trigger is an ABSENT trigger.
        if trimmed.starts_with('#') {
            continue;
        }
        block.push('\n');
        block.push_str(line);
    }
    Some(block)
}

#[test]
fn every_workflow_is_manually_dispatchable() {
    let files = workflow_files();
    assert!(
        files.len() >= MIN_WORKFLOWS,
        "found only {} workflow file(s) in {} — expected at least {MIN_WORKFLOWS}. Either the \
         layout moved or this guard's parse broke; it must not report all-clear over an \
         implausibly small set.",
        files.len(),
        workflows_dir().display()
    );

    let mut offenders = Vec::new();
    for (name, body) in &files {
        if body.contains(OPT_OUT) {
            continue;
        }
        let block = on_block(body).unwrap_or_else(|| {
            panic!("{name} has no top-level `on:` block — it can never be triggered at all")
        });
        if !block.contains("workflow_dispatch") {
            offenders.push(format!(
                "{name} (on: block does not declare workflow_dispatch)"
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "these workflows cannot be dispatched manually, so they cannot be validated on a branch \
         before merging — and a `pull_request` run skips the master-only e2e-tests job:\n  {}\n\
         Add `workflow_dispatch:` to the `on:` block, or opt out with the comment \
         \"{OPT_OUT}\" at the top of the file if it genuinely must not be dispatchable.",
        offenders.join("\n  ")
    );
}

/// The control for the test above. If `on_block` returned an empty string for every file — a broken
/// parse — the dispatch test would still pass, because `"".contains("workflow_dispatch")` is false
/// only for offenders and the offender list would be *everything*, which looks like a real failure
/// rather than a broken instrument. So assert the parse actually found the triggers it should.
#[test]
fn the_on_block_parse_finds_real_triggers() {
    let files = workflow_files();
    let ci = files
        .iter()
        .find(|(n, _)| n == "ci.yml")
        .expect("ci.yml must exist — it is the required gate");

    let block = on_block(&ci.1).expect("ci.yml has an on: block");
    for expected in ["push:", "pull_request:", "workflow_dispatch"] {
        assert!(
            block.contains(expected),
            "the on: block parse missed {expected:?} in ci.yml. Parsed block was:\n{block}"
        );
    }
    assert!(
        !block.contains("runs-on"),
        "the on: block parse ran past the end of the triggers and swallowed a job definition:\n{block}"
    );
}

/// Written because the mutation that matters most survived the first version of this guard.
///
/// `ci.yml`'s `on:` block carries a real comment directly above `workflow_dispatch:`
/// ("Lets the master-only e2e-tests job be validated on a branch before merging"), so this is not a
/// hypothetical: the parse has to distinguish a trigger from prose about a trigger. If comments were
/// kept, commenting out every trigger in the repo would still pass
/// `every_workflow_is_manually_dispatchable`.
#[test]
fn the_on_block_parse_drops_comments() {
    let block = on_block(
        "on:\n  push:\n    branches: [master]\n  # workflow_dispatch: not really\n  schedule:\n    - cron: \"0 0 * * 0\"\njobs:\n  x:\n    runs-on: ubuntu-latest\n",
    )
    .expect("synthetic on: block");

    assert!(
        !block.contains("workflow_dispatch"),
        "a commented-out trigger must not count as declared; parsed block was:\n{block}"
    );
    assert!(
        block.contains("schedule:"),
        "dropping comments must not drop the real triggers around them:\n{block}"
    );
    assert!(
        !block.contains("runs-on"),
        "the block must still end at the next top-level key:\n{block}"
    );
}

/// #315 itself: the deleted workflow must not come back, and if something like it does, it has to
/// satisfy the rule above. Named explicitly so the deletion is not quietly reverted.
#[test]
fn the_unreachable_fork_workflow_is_gone() {
    let files = workflow_files();
    assert!(
        !files.iter().any(|(n, _)| n == "interop-fork.yml"),
        "interop-fork.yml is back. Its only triggers were push/pull_request on \
         `fork/interop-lean`, a branch merged in June 2026 and deleted under #284 — it had 0 runs \
         in its entire existence. If it is genuinely wanted, retarget it at master AND give it \
         workflow_dispatch so it can be verified."
    );
}
