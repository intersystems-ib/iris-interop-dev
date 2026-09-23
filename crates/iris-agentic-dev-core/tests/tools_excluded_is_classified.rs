//! #353: every upstream tool this fork does not advertise carries a RECORDED REASON.
//!
//! The mechanical half — which of upstream's tools we ported and pruned, which we never ported — is
//! computed by `scripts/validate-tools.sh` from the two trees (#359). It says nothing about WHY, and
//! why is the whole difference between a profile and an omission: nothing said whether a tool is
//! absent because it is unsafe here, outside the remit, already answered, or simply not adopted yet.
//!
//! ## Why the join is NOT here
//!
//! Answering "did we classify every upstream-only tool" needs `upstream/master`, and CI clones do
//! not carry that remote — the surface section of the script prints NOT CHECKED and exits 0 there
//! for exactly that reason. So the join lives in the script, as a maintainer gate that exits 1.
//!
//! What this file asserts is everything that needs no remote and therefore runs in the required CI
//! gate: the file parses, every category is one the file itself defines, every reason is a real
//! sentence, nothing is both advertised and excluded, and no count is written down.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn doc() -> serde_json::Value {
    let p = repo_root().join("tools-excluded.json");
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{}: {e} — #353's rationale must exist", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} does not parse: {e}", p.display()))
}

fn tools() -> BTreeMap<String, serde_json::Value> {
    doc()["tools"]
        .as_object()
        .expect("tools must be an object")
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A reason that is not a reason — "not ported", "excluded", "n/a" — is the omission this issue is
/// about, wearing a label. Each entry must say something a reader could disagree with.
#[test]
fn every_excluded_tool_carries_a_real_reason() {
    let categories: Vec<String> = doc()["categories"]
        .as_object()
        .expect("the file must define its own categories")
        .keys()
        .cloned()
        .collect();
    assert!(
        categories.len() >= 3,
        "a two-way split cannot distinguish 'never, on purpose' from 'not yet': {categories:?}"
    );
    let entries = tools();
    assert!(!entries.is_empty(), "no tools classified at all");

    for (name, v) in &entries {
        let cat = v["category"].as_str().unwrap_or_default();
        assert!(
            categories.iter().any(|c| c == cat),
            "{name} uses category {cat:?}, which the file does not define: {categories:?}"
        );
        let why = v["why"].as_str().unwrap_or_default().trim();
        assert!(
            why.len() >= 40,
            "{name}'s reason is {} chars — too short to be a reason someone could disagree with: \
             {why:?}",
            why.len()
        );
        assert!(
            why.ends_with('.'),
            "{name}'s reason is not a sentence: {why:?}"
        );
        // The commonest non-reasons. "Not ported" restates the computed bucket; the script already
        // prints which tools are absent, and repeating it here explains nothing.
        let lower = why.to_lowercase();
        for empty in ["not ported", "n/a", "todo", "no reason"] {
            assert!(
                !lower.starts_with(empty),
                "{name}'s reason begins with {empty:?}, which restates the bucket rather than \
                 explaining it: {why:?}"
            );
        }
    }
}

/// `wanted` is the one category that means "no decision has been taken against this", so it is the
/// one that rots: without somewhere to look, it reads as a promise. Most carry a tracking reference;
/// the ones that do not must still say what they would be FOR.
#[test]
fn a_tracking_reference_points_somewhere_real() {
    let mut tracked = 0;
    for (name, v) in &tools() {
        let Some(t) = v.get("tracking").and_then(|t| t.as_str()) else {
            continue;
        };
        tracked += 1;
        assert!(
            t.contains('#') && t.contains('/'),
            "{name}'s tracking {t:?} is not an owner/repo#number reference"
        );
        assert_eq!(
            v["category"], "wanted",
            "{name} carries a tracking reference but is categorised {:?} — a tool excluded on \
             principle should not look like one under discussion",
            v["category"]
        );
    }
    assert!(
        tracked >= 3,
        "only {tracked} entries carry a tracking reference; the `wanted` ones with an open issue \
         should name it"
    );
}

/// A tool cannot be both advertised and excluded. This is the half of the join that needs no
/// `upstream` remote, so it runs in CI where the rest of the join cannot.
#[test]
fn nothing_is_both_advertised_and_excluded() {
    let src =
        std::fs::read_to_string(repo_root().join("crates/iris-agentic-dev-core/src/tools/mod.rs"))
            .expect("mod.rs");
    let at = src
        .find("INTEROP_TOOLS")
        .expect("INTEROP_TOOLS must be in mod.rs");
    let rest = &src[at..];
    let body = &rest[..rest.find("];").expect("the list must be terminated")];
    let advertised: Vec<&str> = body
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect();
    // CONTROL: the parse found a real list, not an empty one that trivially intersects nothing.
    assert!(
        advertised.len() >= 20,
        "parsed only {} advertised tools, which cannot be right — the check below would pass \
         vacuously",
        advertised.len()
    );
    assert!(advertised.contains(&"iris_doc"), "{advertised:?}");

    let excluded = tools();
    let both: Vec<&str> = advertised
        .iter()
        .filter(|a| excluded.contains_key(**a))
        .copied()
        .collect();
    assert!(
        both.is_empty(),
        "these are advertised AND carry an exclusion reason: {both:?}"
    );
}

/// No counts. Every prose count in this repo has gone stale, in both directions, and #359 removed
/// the last ones from `tools-status.json` for that reason — a number here would be a second source
/// of truth for something `scripts/validate-tools.sh` prints from the trees.
#[test]
fn the_rationale_writes_down_no_counts() {
    let text = std::fs::read_to_string(repo_root().join("tools-excluded.json")).expect("the file");
    let re = regex::Regex::new(r"\b\d+\s+(?:tools?|of upstream|excluded|advertised)\b")
        .expect("a valid pattern");
    let hits: Vec<&str> = re.find_iter(&text).map(|m| m.as_str()).collect();
    assert!(
        hits.is_empty(),
        "tools-excluded.json restates a count {hits:?} — remove it and let the script print it"
    );
    // CONTROL: the pattern can fire, so an empty result means absence rather than a dead regex.
    assert!(
        re.is_match("this profile is 23 tools of upstream's 71"),
        "the count detector matches nothing at all, so its clean result proves nothing"
    );
}
