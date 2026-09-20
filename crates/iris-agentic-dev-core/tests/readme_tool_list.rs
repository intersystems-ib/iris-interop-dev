//! #294: the README's Tools section must list every advertised interop tool.
//!
//! The README said "23-tool interop profile" and its Tools section listed exactly 23 — correct when
//! written. The profile had grown to 30, so seven tools shipped and were advertised with no
//! user-facing documentation at all. The count and the list rotted together, which is why the counts
//! were removed rather than corrected: a number in prose has no way to stay true. This test is what
//! replaced them — the LIST is the asserted thing.
//!
//! WHY ITS OWN FILE. This started inside `tool_annotation_tests` in `mod.rs`, then moved to that
//! file's end. Both spots are contested: measured on a 19-branch trial merge, three branches insert
//! test modules before the same doc-comment anchor and another appends near EOF, and git cannot tell
//! additive insertions apart — so any two of them conflict. A new test file collides with nothing,
//! and `scripts/ci-test-targets.sh` derives its target list from `cargo metadata`, so this is picked
//! up by the required gate and the e2e job without anyone adding it anywhere.

use iris_agentic_dev_core::tools::{IrisTools, Toolset};

/// Fails rather than skips when the README cannot be read: a guard that opts out when it cannot check
/// is not a guard. It also refuses to pass when its own parse finds implausibly few names — a clean
/// result from a broken parse is exactly what let this rot unnoticed, and I hit that while
/// investigating: an early extraction matched zero rows and then "found" all 30 tools undocumented.
#[test]
fn every_interop_tool_appears_in_the_readme() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md");
    let readme = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — this guard must not pass by being unable to look",
            path.display()
        )
    });
    const HEADING: &str = "## Tools (interop profile)";
    let start = readme.find(HEADING).unwrap_or_else(|| {
        panic!("README has no `{HEADING}` section — if renamed, update this test")
    });
    let rest = &readme[start + HEADING.len()..];
    let section = match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    };
    let documented: std::collections::HashSet<&str> = section
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        })
        .collect();
    assert!(
        documented.len() >= 20,
        "only {} backticked names found in the Tools section — the parse looks broken, so a clean \
         result below would mean nothing",
        documented.len()
    );

    let t = IrisTools::new_with_toolset(None, Toolset::Interop).expect("build");
    let advertised: Vec<String> = t
        .advertised_tools()
        .iter()
        .map(|x| x.name.to_string())
        .collect();
    assert!(
        !advertised.is_empty(),
        "precondition: the profile is non-empty"
    );
    let missing: Vec<&String> = advertised
        .iter()
        .filter(|n| !documented.contains(n.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "advertised but undocumented in the README's Tools section: {missing:?}. A tool a user \
         cannot discover is a tool that does not exist for them — add it there, in the group it \
         belongs to."
    );
}
