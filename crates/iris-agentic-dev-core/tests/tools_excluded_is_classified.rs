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
    // A POSITIVE CONTROL, not a quota. This was `>= 3` and went red the moment #351 and #352 adopted
    // three of the tracked tools — a test that fails because the thing it tracks got DONE punishes
    // the outcome it exists to encourage, and the number has nowhere to go but down. What must hold
    // is that the field is real and parsed: at least one entry exercises it, and every entry that
    // does is well-formed and `wanted` (asserted in the loop above).
    assert!(
        tracked >= 1,
        "no entry carries a tracking reference, so the two assertions in the loop above never ran \
         and this test proves nothing about the field"
    );
}

/// The keep-list a profile prunes to, parsed from its `pub const`.
///
/// Anchored on the DEFINITION, not on the first mention of the name. The previous version used
/// `src.find("INTEROP_TOOLS")`, and #531 added a `Toolset::keep_list()` accessor that names the
/// const EARLIER in the file — so the window started inside that function and ran on to the next
/// `];`, swallowing a second list. Its `>= 20` size control passed, because the wrong window still
/// held more than twenty tool names. A size control proves the window is not empty; it cannot
/// prove the window is the right one.
fn keep_list(src: &str, const_name: &str) -> Vec<String> {
    let needle = format!("pub const {const_name}: &[&str] = &[");
    let at = src
        .find(&needle)
        .unwrap_or_else(|| panic!("{const_name} must be defined in mod.rs as `{needle}`"));
    let rest = &src[at + needle.len()..];
    let body = &rest[..rest
        .find("\n];")
        .unwrap_or_else(|| panic!("{const_name} must be terminated by a line holding `];`"))];
    body.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .map(|s| s.to_string())
        .collect()
}

/// A tool cannot be both advertised and excluded — and since #531 that is a question PER PROFILE.
///
/// This is the half of the join that needs no `upstream` remote, so it runs in CI where the rest
/// of the join cannot. It caught the real thing it exists for: adding `iris_search` and
/// `iris_info` to `ROUTINES_TOOLS` left them carrying a `wanted` exclusion reason written for the
/// interop profile, which is now recorded as a per-profile override instead.
#[test]
fn nothing_is_both_advertised_and_excluded() {
    let src =
        std::fs::read_to_string(repo_root().join("crates/iris-agentic-dev-core/src/tools/mod.rs"))
            .expect("mod.rs");
    let excluded = tools();

    // Every profile the rationale file declares must have a keep-list, and vice versa: a profile
    // nobody classified, or a classification for a profile that does not exist, are both the
    // silence this file was written to remove.
    let declared: Vec<String> = doc()["profiles"]
        .as_object()
        .expect("the file must declare its profiles (#531)")
        .keys()
        .filter(|k| !k.starts_with('$'))
        .cloned()
        .collect();
    assert!(
        declared.len() >= 2,
        "with one profile this test cannot distinguish a per-profile verdict from a global one: \
         {declared:?}"
    );

    let mut checked = 0usize;
    for profile in &declared {
        let const_name = format!("{}_TOOLS", profile.to_ascii_uppercase());
        let advertised = keep_list(&src, &const_name);
        // CONTROL: a real list, and the RIGHT list — a window that over-ran into the neighbouring
        // const would pass a mere length check.
        assert!(
            advertised.len() >= 10,
            "parsed only {} tools for {const_name}; the check below would pass vacuously",
            advertised.len()
        );
        assert!(
            advertised.contains(&"iris_doc".to_string()),
            "{const_name} must contain iris_doc: {advertised:?}"
        );
        let other = if profile == "interop" {
            "iris_production"
        } else {
            "iris_search"
        };
        assert!(
            advertised.contains(&other.to_string()),
            "{const_name} must contain {other}, or the parse took the wrong window: {advertised:?}"
        );

        for tool in &advertised {
            let Some(entry) = excluded.get(tool) else {
                continue;
            };
            // An entry exists, so SOME profile excludes this tool. For THIS profile it must say so
            // explicitly, via an `advertised` override.
            let verdict = entry
                .get("profiles")
                .and_then(|p| p.get(profile))
                .and_then(|v| v.get("category"))
                .and_then(|c| c.as_str());
            assert_eq!(
                verdict,
                Some("advertised"),
                "`{tool}` is in {const_name} but tools-excluded.json gives the `{profile}` profile \
                 the verdict {:?} — a tool cannot be advertised and excluded by the same profile. \
                 Add a `profiles.{profile}` override with category `advertised` and the reason it \
                 belongs there.",
                verdict.or_else(|| entry.get("category").and_then(|c| c.as_str())),
            );
            checked += 1;
        }
    }
    // The loop above is only meaningful if some advertised tool DOES carry an entry. If none does,
    // every assertion was skipped and this test proved nothing.
    assert!(
        checked >= 1,
        "no advertised tool carries an exclusion entry, so the per-profile check never ran"
    );
}

/// The inverse, and the one a reader assumes is covered: an `advertised` override must name a
/// profile that really advertises the tool. Otherwise the override is a claim about nothing, and
/// the obvious way to silence the test above is to write one for a profile at random.
#[test]
fn an_advertised_override_names_a_profile_that_advertises_it() {
    let src =
        std::fs::read_to_string(repo_root().join("crates/iris-agentic-dev-core/src/tools/mod.rs"))
            .expect("mod.rs");
    let mut overrides = 0usize;
    for (tool, entry) in tools() {
        let Some(profiles) = entry.get("profiles").and_then(|p| p.as_object()) else {
            continue;
        };
        for (profile, verdict) in profiles {
            if verdict.get("category").and_then(|c| c.as_str()) != Some("advertised") {
                continue;
            }
            let const_name = format!("{}_TOOLS", profile.to_ascii_uppercase());
            let advertised = keep_list(&src, &const_name);
            assert!(
                advertised.contains(&tool),
                "tools-excluded.json says `{profile}` advertises `{tool}`, but {const_name} does \
                 not list it"
            );
            overrides += 1;
        }
    }
    assert!(
        overrides >= 1,
        "no `advertised` override exists, so this test asserted nothing"
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
