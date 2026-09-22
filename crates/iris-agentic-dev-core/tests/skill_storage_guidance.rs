//! #331: no skill may tell a reader to delete a Storage block without saying when.
//!
//! The destructive instruction lived in THREE layers, and each was found separately:
//!
//! 1. `doc.rs` — `iris_doc(put)` stripped the block, then refused the write, and the refusal's first
//!    sentence was *"FIX: delete the Storage block from your source and write the class again"*.
//! 2. `mod.rs` — `hint_5559` said *"an explicit Storage block, whose XML this UDL parser rejects:
//!    remove it and let IRIS regenerate it"*, justified by a comment claiming the put path
//!    intercepted it first. Removing the interception made that reachable.
//! 3. **The skills** — `objectscript-guardrails/SKILL.md`, in both tree copies:
//!    *"NEVER write `Storage Default { ... }` in UDL — omit entirely. IRIS auto-generates storage.
//!    Writing one causes ERROR #5559 in IRIS 2025.1+."*
//!
//! The third is the worst of them, because a skill reaches a model at full delivery while a hint
//! only fires on a failure — and because it also asserted something measurably false.
//!
//! ## What was measured
//!
//! Two writable throwaway community instances, plain Atelier REST: a `%Persistent` class carrying a
//! generated `Storage Default` was accepted and preserved on **2025.3** (PUT 201, compile 200) and
//! **2026.1** (PUT 200, compile 200), zero errors, slot list and `<DataLocation>` byte-intact on
//! read-back. So "writing one causes ERROR #5559 in IRIS 2025.1+" is false for every version that
//! could be tested. (2025.1 itself could not be: its community licence has expired.)
//!
//! ## Why the rule is conditional rather than "never"
//!
//! Omitting the block is right when AUTHORING a class — IRIS generates it. It is destructive when
//! EDITING one, because a generated block is not derivable from the current property set: IRIS mints
//! arbitrary global names (`Ens.Config.Credentials` stores to `^Ens.Conf.CredentialsD`) and tracks
//! slot numbers across properties added, deleted and renamed, so a deleted property's slot stays
//! vacant to keep the survivors in place. Regeneration re-packs, and a stored row is a `$list`
//! addressed by slot number: slot 3 stops being `Username` while every row still holds the old
//! layout. Nothing errors, and both may be strings.
//!
//! ## Why a test and not just an edit
//!
//! The instruction existed in two tree copies and I had to remember to fix both. The trees are NOT
//! meant to be identical — 6 of 13 shared skills differ by design, so `light-skills` is a lighter
//! variant and an identity check would be wrong. A CONTENT check is the one that holds: whatever the
//! trees say about Storage blocks, neither may carry the unconditional instruction.

use std::path::{Path, PathBuf};

/// Refuse to pass on an implausible scan. If the layout moves and the walk finds almost nothing, this
/// must FAIL rather than report all-clear over an empty set — a guard that opts out when it cannot
/// look is the defect it exists to catch.
const MIN_SKILL_FILES: usize = 10;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above the crate manifest")
        .to_path_buf()
}

/// Every markdown file under the skill trees. PANICS on an unreadable directory.
fn skill_docs() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            // A tree that is genuinely absent is not a failure (a consumer may vendor only one);
            // an unreadable one is caught by the MIN_SKILL_FILES floor below.
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
                let body = std::fs::read_to_string(&p)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
                out.push((p, body));
            }
        }
    }
    let root = repo_root();
    let mut out = Vec::new();
    walk(&root.join("skills"), &mut out);
    walk(&root.join("light-skills"), &mut out);
    out
}

/// Phrasings that tell the reader to get rid of a Storage block. Deliberately several spellings: the
/// same instruction was written three different ways in three layers, and a guard pinned to one
/// string catches one of them.
const DESTRUCTIVE: &[&str] = &[
    "omit entirely",
    "delete the storage block",
    "remove the storage block",
    "let iris regenerate it",
    "remove it and let iris regenerate",
    "strip the storage block",
];

/// Words that make the instruction conditional on the class not existing yet. Any ONE of these in the
/// same file is enough — this guard is about the distinction being drawn, not about how it is worded.
const QUALIFIERS: &[&str] = &["existing", "already exists", "new class", "authoring"];

#[test]
fn no_skill_tells_you_to_delete_a_storage_block_unconditionally() {
    let docs = skill_docs();
    assert!(
        docs.len() >= MIN_SKILL_FILES,
        "found only {} skill markdown file(s) under skills/ and light-skills/ — expected at least \
         {MIN_SKILL_FILES}. The walk or the layout is wrong, and this guard must fail rather than \
         report all-clear over an implausibly small set.",
        docs.len()
    );

    let mut offenders = Vec::new();
    for (path, body) in &docs {
        let lower = body.to_lowercase();
        if !lower.contains("storage") {
            continue;
        }
        // PER LINE, not per file. Checking the whole document for a qualifier let the original
        // unconditional instruction SURVIVE this guard: these are long documents and "existing"
        // appears in them for unrelated reasons. The same over-broad-window mistake as a cap check
        // that searched 1.5 KB of prose for a number — the window has to be the claim itself.
        for line in lower.lines() {
            if !line.contains("storage") {
                continue;
            }
            let destructive: Vec<&str> = DESTRUCTIVE
                .iter()
                .copied()
                .filter(|d| line.contains(d))
                .collect();
            if destructive.is_empty() {
                continue;
            }
            // It says "get rid of it" — so the SAME line must also say when.
            if !QUALIFIERS.iter().any(|q| line.contains(q)) {
                offenders.push(format!(
                    "{}: {:?} — says {:?} about a Storage block without distinguishing a NEW class \
                     from an EXISTING one",
                    path.display(),
                    line.trim(),
                    destructive
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these skills carry the unconditional instruction to remove a Storage block. Omitting it is \
         right when AUTHORING a class and DESTRUCTIVE when editing one — IRIS tracks slot numbers \
         across properties added, deleted and renamed, so regenerating re-packs them and silently \
         re-maps every stored row (#331):\n  {}",
        offenders.join("\n  ")
    );
}

/// #331: and no skill may repeat the claim that a Storage block causes #5559, which was measured
/// false on both versions that could be tested.
#[test]
fn no_skill_claims_a_storage_block_causes_5559() {
    let docs = skill_docs();
    assert!(
        docs.len() >= MIN_SKILL_FILES,
        "implausible scan: {}",
        docs.len()
    );

    let mut offenders = Vec::new();
    for (path, body) in &docs {
        let lower = body.to_lowercase();
        if !lower.contains("5559") {
            continue;
        }
        // Mentioning #5559 is fine — it is a real error with a real cause (an underscore in a member
        // name). Attributing it to a Storage block is what was measured false.
        for line in lower.lines().filter(|l| l.contains("5559")) {
            if line.contains("storage") {
                offenders.push(format!("{}: {}", path.display(), line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these skills attribute ERROR #5559 to a Storage block. Measured on writable instances: a \
         class carrying a generated Storage block is ACCEPTED and PRESERVED — PUT 201/200 and \
         compile 200 with zero errors on 2025.3 and 2026.1. #5559's usual cause is an underscore in \
         a member name, which this repo's own hint_5559 says outright:\n  {}",
        offenders.join("\n  ")
    );
}

/// THE CONTROL for both tests above. If the scan found no file that even mentions Storage, or no file
/// mentioning #5559, the two guards would pass vacuously — which is how a broken walk reads as
/// compliance.
#[test]
fn the_scan_actually_reaches_files_that_discuss_storage_and_5559() {
    let docs = skill_docs();
    let mentions_storage = docs
        .iter()
        .filter(|(_, b)| b.to_lowercase().contains("storage"))
        .count();
    let mentions_5559 = docs.iter().filter(|(_, b)| b.contains("5559")).count();

    assert!(
        mentions_storage > 0,
        "no skill document mentions Storage at all, so \
         `no_skill_tells_you_to_delete_a_storage_block_unconditionally` proves nothing. Either the \
         walk is broken or the guidance moved — find it before deleting this guard."
    );
    assert!(
        mentions_5559 > 0,
        "no skill document mentions 5559, so `no_skill_claims_a_storage_block_causes_5559` proves \
         nothing"
    );
}
