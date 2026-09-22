//! #317: `scripts/coverage.sh` named packages that this fork had renamed, so the script could not
//! run at all — every invocation died on `error: package ID specification 'iris-dev-core' did not
//! match any packages`. A coverage script that cannot start reports no coverage, and "we have a
//! coverage script" reads exactly like "we measure coverage".
//!
//! Several names were wrong at once, which is the tell: a package rename lands in `Cargo.toml` and in
//! the code, and every *string* that names a crate or a binary — in a shell script, a workflow, an
//! editor config — is across a boundary the compiler does not cross. Nothing failed to build. The
//! script just stopped working, quietly, for however long.
//!
//! THE RULE this file enforces: every crate name and every built-binary path in the repo's live
//! build configuration must resolve against the workspace's own `Cargo.toml` files.
//!
//! Derivation, not duplication: the valid names are read out of the manifests at test time. Writing
//! `iris-agentic-dev-core` into this file as a constant would just move the rot one directory over —
//! the next rename would leave the test green and the script broken, or red and the script fine.
//!
//! SIBLINGS. `coverage.sh` was not the whole of it. Running this file's own extractor across the
//! repo found the same rot in the editor configuration, fixed alongside: every `.vscode/launch.json`
//! debug configuration launched a debug-profile path ending in `iris-dev` — a binary that has not
//! existed since the rename — with `RUST_LOG` naming the crate log targets `iris_dev` and
//! `iris_dev_core`, neither of which matches a crate any more; and `.vscode/tasks.json` built
//! `-p iris-dev-core`. That is why the last two tests here are repo-wide rather than scoped to
//! `coverage.sh`: fixing one instance of this class makes every unfixed sibling look MORE
//! trustworthy, not less.
//!
//! (The stale paths are described above rather than written out, because the repo-wide guard reads
//! THIS file too — a wrong name quoted verbatim in a comment here would fail it. That is the guard
//! working, not a limitation.)
//!
//! Every guard below FAILS rather than skips when it cannot read its inputs, and refuses to pass
//! when its own extraction finds nothing: an extractor that quietly matches zero tokens over a file
//! full of wrong names is indistinguishable from a file with no wrong names in it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `CARGO_MANIFEST_DIR` is `crates/iris-agentic-dev-core`; the workspace root is two levels up.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must be two levels above the crate manifest")
        .to_path_buf()
}

/// PANIC, never skip. An unreadable input is precisely the state in which a guard that returns
/// "nothing to check" would report all-clear over a file it never opened.
fn read_or_fail(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must FAIL, not skip — it exists because this file \
             silently stopped matching reality.",
            path.display()
        )
    })
}

fn parse_toml_or_fail(path: &Path) -> toml::Value {
    let text = read_or_fail(path);
    toml::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {} as TOML: {e}", path.display()))
}

// ── The oracle: what cargo will actually accept, read out of the manifests ─────────────────────

/// The names cargo itself will accept, derived by reading the manifests.
struct WorkspaceNames {
    /// Valid arguments to `--package` / `-p`.
    packages: BTreeSet<String>,
    /// Valid `target/<profile>/<name>` file names — `[[bin]] name`, which is NOT the package name
    /// here: package `iris-agentic-dev` builds a binary called `iris-interop-dev`. That gap is
    /// where one of the #317 breakages lived, and every one of them in `.vscode/launch.json`.
    bins: BTreeSet<String>,
}

fn workspace_names() -> WorkspaceNames {
    let root = workspace_root();
    let root_manifest = parse_toml_or_fail(&root.join("Cargo.toml"));

    let mut member_dirs: Vec<PathBuf> = Vec::new();
    // A workspace root may also be a package in its own right; this one is not, but reading it
    // from the manifest rather than assuming keeps the derivation honest either way.
    if root_manifest.get("package").is_some() {
        member_dirs.push(root.clone());
    }
    let members = root_manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .unwrap_or_else(|| {
            panic!(
                "no [workspace].members array in {}: this guard derives every valid package name \
                 from that list and cannot run without it",
                root.join("Cargo.toml").display()
            )
        });
    for m in members {
        let rel = m
            .as_str()
            .unwrap_or_else(|| panic!("non-string entry in [workspace].members: {m:?}"));
        member_dirs.push(root.join(rel));
    }

    let mut packages = BTreeSet::new();
    let mut bins = BTreeSet::new();
    for dir in &member_dirs {
        let manifest = parse_toml_or_fail(&dir.join("Cargo.toml"));
        let pkg_name = manifest
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_else(|| panic!("no [package].name in {}", dir.join("Cargo.toml").display()))
            .to_string();

        let explicit_bins: Vec<String> = manifest
            .get("bin")
            .and_then(|b| b.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("name"))
                    .filter_map(|n| n.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        if explicit_bins.is_empty() {
            // Cargo's auto-discovery rule: src/main.rs yields a binary named after the package.
            if dir.join("src/main.rs").is_file() {
                bins.insert(pkg_name.clone());
            }
        } else {
            bins.extend(explicit_bins);
        }
        packages.insert(pkg_name);
    }

    // Refuse to continue on an implausible parse. An empty derivation would turn every check below
    // into an unsatisfiable one, and the resulting failure would read as "the config names a bad
    // package" when the truth is "this test could not read the manifests". Say which side broke.
    assert!(
        !packages.is_empty(),
        "derived zero package names from the workspace manifests — the derivation broke, not the config"
    );
    assert!(
        !bins.is_empty(),
        "derived zero binary names from the workspace manifests — the derivation broke, not the config"
    );

    WorkspaceNames { packages, bins }
}

// ── Extraction ────────────────────────────────────────────────────────────────────────────────

/// `-p` also means "create parent directories". Those uses are excluded by the preceding command;
/// a candidate that survives this list still has to look like a crate name to count as one.
const NON_CARGO_DASH_P: &[&str] = &["mkdir", "rmdir", "install"];

/// Shell and JSON punctuation that can abut a flag or a name: `COV_PACKAGES=(--package a)` in bash,
/// `["test", "-p", "iris-agentic-dev-core"]` in `.vscode/tasks.json`. Each is replaced by a single
/// SPACE, `=` included, so that `--package=X` and `--package X` tokenise identically and the flag is
/// always immediately followed by its value.
///
/// It was not always. Replacing these with `" = "` and stepping over one separator token read bash
/// correctly and silently missed JSON, where `"-p", "iris-agentic-dev-core"` leaves FOUR separators
/// between the flag and the name — `.vscode/tasks.json` carried a wrong package name straight
/// through a green run of this test. A surviving mutant found it.
const PUNCTUATION: [char; 7] = ['(', ')', '=', '[', ']', ',', '"'];

fn looks_like_a_crate_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Strip the prose punctuation that can cling to a name — so a crate named inside a quoted error
/// message (`'cargo build -p iris-agentic-dev'.`) is checked too, rather than dropped as
/// unparseable and left free to rot.
fn unquote(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '.' | ',' | ';' | ':'))
}

/// Every `--package X` / `--package=X` / `-p X` in `text`, with its 1-based line number.
///
/// Tokens that are obviously not crate names — `"$PROFILE_DIR"`, `/tmp/x`, a docker port mapping
/// `1972:1972` — fail `looks_like_a_crate_name` and are dropped, so a path or a shell expansion
/// never masquerades as a package. A *wrong* crate name such as `iris-dev-core` passes that filter
/// intact and is then checked, which is the only case that matters.
fn extract_package_names(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let cleaned = line.replace(PUNCTUATION, " ");
        let toks: Vec<&str> = cleaned.split_whitespace().collect();
        for (i, tok) in toks.iter().enumerate() {
            let is_long = *tok == "--package";
            let is_short = *tok == "-p" && !(i > 0 && NON_CARGO_DASH_P.contains(&toks[i - 1]));
            if !(is_long || is_short) {
                continue;
            }
            // The punctuation pass collapses every separator, so the value is always the very next
            // token — no stepping, and therefore no separator shape left to get wrong.
            if let Some(cand) = toks.get(i + 1) {
                let cand = unquote(cand);
                if looks_like_a_crate_name(cand) {
                    found.push((lineno + 1, cand.to_string()));
                }
            }
        }
    }
    found
}

/// Where cargo puts a built binary. `/release/` rather than `target/release/` so that the
/// cross-compiled `target/<triple>/release/<bin>` paths in the release workflow are covered too.
const BUILD_DIR_MARKERS: [&str; 2] = ["target/debug/", "/release/"];

/// Every `target/debug/<name>` and `.../release/<name>` in `text`, with its 1-based line number.
///
/// A deeper path (`target/debug/deps/…`) is a cargo-internal directory rather than a binary this
/// repo builds, so it is skipped: the run is a binary name only when nothing follows it. A run that
/// starts with a shell or template expansion (`${{ matrix.bin }}`) yields no name and is skipped.
fn extract_built_binary_names(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        for marker in BUILD_DIR_MARKERS {
            let mut rest = line;
            while let Some(pos) = rest.find(marker) {
                let after = &rest[pos + marker.len()..];
                let raw: String = after
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'
                    })
                    .collect();
                let tail = &after[raw.len()..];
                // A trailing '.' is sentence punctuation in a comment, never part of the file
                // name — trim it so a name mentioned in prose is checked rather than discarded.
                let name = raw.trim_end_matches('.');
                if !name.is_empty() && !tail.starts_with('/') {
                    found.push((lineno + 1, name.to_string()));
                }
                rest = after;
            }
        }
    }
    found
}

// ── The files to read ─────────────────────────────────────────────────────────────────────────

/// Build output, VCS metadata, vendored trees, and agent worktrees. `.claude` matters most: in the
/// primary checkout it holds `.claude/worktrees/*`, entire second copies of this repo, and walking
/// into those would both crawl and trip over another agent's half-finished edit.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".claude"];

/// Extensions that can name a crate or a binary in this repo. Filtering by extension rather than
/// sniffing keeps a stray binary blob out of `read_to_string`.
const TEXT_EXTENSIONS: &[&str] = &["rs", "sh", "toml", "json", "md", "yml", "yaml", "txt", "py"];

/// Collect text files under `dir`, recursively. PANICS on an unreadable directory: a walk that
/// silently yields fewer files is the failure mode these guards exist to rule out.
fn text_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "cannot read directory {}: {e}. This guard must FAIL, not skip.",
            dir.display()
        )
    });
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("cannot stat an entry in {}: {e}", dir.display()));
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let ty = entry
            .file_type()
            .unwrap_or_else(|e| panic!("cannot read the file type of {}: {e}", path.display()));
        if ty.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                out.extend(text_files_under(&path));
            }
        } else if ty.is_file() {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            if TEXT_EXTENSIONS.contains(&ext.as_str()) {
                out.push(path);
            }
        }
    }
    out
}

fn rel(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn coverage_script_path() -> PathBuf {
    workspace_root().join("scripts/coverage.sh")
}

// ── #317 proper: scripts/coverage.sh ──────────────────────────────────────────────────────────

#[test]
fn coverage_script_package_flags_name_real_packages() {
    let path = coverage_script_path();
    let script = read_or_fail(&path);
    let names = workspace_names();

    let found = extract_package_names(&script);

    // The zero guard. Without it, breaking the extractor — or deleting every cargo invocation from
    // the script — turns this test green, which is the same false all-clear #317 shipped with.
    assert!(
        !found.is_empty(),
        "extracted ZERO --package/-p names from {}. Either the script no longer names any package \
         (then this guard is checking nothing and must be rewritten) or the extractor broke. \
         Refusing to pass either way.",
        path.display()
    );

    let bad: Vec<String> = found
        .iter()
        .filter(|(_, n)| !names.packages.contains(n))
        .map(|(line, n)| format!("  {}:{line}  --package {n}", rel(&path)))
        .collect();

    assert!(
        bad.is_empty(),
        "scripts/coverage.sh names package(s) this workspace does not declare:\n{}\n\
         Workspace packages (from the Cargo.toml files): {:?}\n\
         Cargo rejects the whole invocation on an unknown package id, so the script does not \
         under-report — it does not run.",
        bad.join("\n"),
        names.packages
    );
}

#[test]
fn coverage_script_debug_binary_paths_name_real_binaries() {
    let path = coverage_script_path();
    let script = read_or_fail(&path);
    let names = workspace_names();

    let found = extract_built_binary_names(&script);

    assert!(
        !found.is_empty(),
        "extracted ZERO target/debug/<bin> paths from {}. The script builds an instrumented binary \
         and hands its path to the E2E half; if no such path is left, this guard is checking \
         nothing. Refusing to pass.",
        path.display()
    );

    let bad: Vec<String> = found
        .iter()
        .filter(|(_, n)| !names.bins.contains(n))
        .map(|(line, n)| format!("  {}:{line}  target/debug/{n}", rel(&path)))
        .collect();

    assert!(
        bad.is_empty(),
        "scripts/coverage.sh points at binaries this workspace does not build:\n{}\n\
         Workspace [[bin]] names (from the Cargo.toml files): {:?}\n\
         Note the package name and the binary name differ here — that gap is what broke (#317).",
        bad.join("\n"),
        names.bins
    );
}

#[test]
fn coverage_script_derives_its_test_targets_instead_of_listing_them() {
    let path = coverage_script_path();
    let script = read_or_fail(&path);

    // The other half of #317: the list of test targets used to be a hand-kept array that named a
    // fraction of the targets the workspace declares, so the TOTAL was under-reported and looked
    // exactly like a complete one. scripts/ci-test-targets.sh derives the list from `cargo
    // metadata` and carries its own non-empty and stale-exclusion guards; coverage.sh must keep
    // using it rather than growing a second copy.
    // ASSERT THE INVOCATION, NOT THE FILENAME. This was `script.contains("ci-test-targets.sh")`,
    // and the filename appears ELEVEN times in coverage.sh — comments, a usage example, two echo
    // strings — of which exactly ONE is the live call. So replacing the real invocation with a
    // hand-written two-target list PASSED the test named after the #317 defect, and the script then
    // measured 2 targets while printing a perfectly plausible summary.
    //
    // A command substitution of the script is the thing that cannot be faked by prose.
    let invocations = script
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("ci-test-targets.sh") && t.contains("$(")
        })
        .count();
    assert!(
        invocations >= 1,
        "{} has no uncommented command substitution calling scripts/ci-test-targets.sh. The \
         filename appearing in a comment is not an invocation: if the test-target list has gone \
         back to being written by hand, coverage under-reports the moment a target is added — \
         silently, because a short list prints the same shape of summary as a complete one.",
        path.display()
    );

    // AND no hand-kept list may come back alongside it. The original defect was an array of target
    // NAMES; the invocation surviving does not prove the array is gone.
    let hardcoded: Vec<&str> = script
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.matches("--test ").count() >= 2
        })
        .collect();
    assert!(
        hardcoded.is_empty(),
        "{} names two or more --test targets on one uncommented line, which is the hand-kept list \
         #317 removed:\n  {}",
        path.display(),
        hardcoded.join("\n  ")
    );
}

/// #317 / N1: the invariant that makes the script RUN, which nothing asserted.
///
/// `COV_PACKAGES` must name BOTH workspace packages. Dropping the second passed every guard in this
/// file while the script itself died in step 1 with
/// `error: no test target named plugin_dispatch_tests in \`iris-agentic-dev-core\` package`, exit 101
/// — because the derived target list spans both packages and `plugin_dispatch_tests` lives in the
/// bin crate. Verified both halves: non-equivalent.
#[test]
fn coverage_script_measures_every_workspace_package() {
    let path = coverage_script_path();
    let script = read_or_fail(&path);
    let names = workspace_names();

    // SCOPED TO THE ARRAY, not the whole script. The first version extracted package names from the
    // entire file and the mutation SURVIVED, because `cargo build -p iris-agentic-dev` at the
    // instrumented-build step still names the package this assertion was looking for. A haystack
    // larger than the claim is the recurring way these guards fail — it also let the cap check and
    // the skill qualifier check pass over unrelated text.
    let cov_line = script
        .lines()
        .find(|l| l.trim_start().starts_with("COV_PACKAGES="))
        .unwrap_or_else(|| {
            panic!(
                "{} has no COV_PACKAGES= assignment; if it was renamed, re-anchor this guard rather \
                 than deleting it",
                path.display()
            )
        });

    let flagged: Vec<String> = extract_package_names(cov_line)
        .into_iter()
        .map(|(_, n)| n)
        .collect();

    // Control: the extraction must find something on that line, or the loop below is vacuous.
    assert!(
        !flagged.is_empty(),
        "{}: COV_PACKAGES names no --package at all, so this guard would pass over an empty set. \
         Line: {cov_line}",
        path.display()
    );

    let missing: Vec<&String> = names
        .packages
        .iter()
        .filter(|p| !flagged.contains(p))
        .collect();
    assert!(
        missing.is_empty(),
        "{}: COV_PACKAGES does not include {missing:?}. The derived target list spans BOTH workspace \
         packages — plugin_dispatch_tests lives in the bin crate — so a single --package makes the \
         run die with 'no test target named ...', exit 101. It names: {flagged:?}",
        path.display()
    );
}

// ── The siblings: the same rot, everywhere else it can hide ────────────────────────────────────

#[test]
fn every_built_binary_path_in_the_repo_names_a_real_binary() {
    let root = workspace_root();
    let names = workspace_names();
    let files = text_files_under(&root);

    assert!(
        !files.is_empty(),
        "walked {} and found no text files at all — the walk broke, so this guard checked nothing",
        root.display()
    );

    let mut found: Vec<(String, usize, String)> = Vec::new();
    for file in &files {
        for (line, name) in extract_built_binary_names(&read_or_fail(file)) {
            found.push((rel(file), line, name));
        }
    }

    assert!(
        !found.is_empty(),
        "extracted ZERO target/<profile>/<bin> paths from {} text files under {}. Every e2e test \
         in this repo spawns one, so zero means the extractor broke, not that the repo is clean.",
        files.len(),
        root.display()
    );

    let bad: Vec<String> = found
        .iter()
        .filter(|(_, _, n)| !names.bins.contains(n))
        .map(|(f, line, n)| format!("  {f}:{line}  -> {n}"))
        .collect();

    assert!(
        bad.is_empty(),
        "these paths name a binary this workspace does not build:\n{}\n\
         Workspace [[bin]] names (from the Cargo.toml files): {:?}\n\
         Nothing fails to compile when one of these goes stale — every .vscode/launch.json \
         configuration named the old binary for as long as the rename had existed (#317).",
        bad.join("\n"),
        names.bins
    );
}

#[test]
fn every_package_flag_in_the_repos_build_config_names_a_real_package() {
    let root = workspace_root();
    let names = workspace_names();

    // Only the directories whose `-p` is EXECUTED. Repo-wide would be wrong here, and measurably
    // so: `specs/*/tasks.md` is an archive of completed plans that still quote the pre-rename
    // commands, and rewriting finished planning documents to satisfy a guard is not a fix.
    let config_dirs = ["scripts", ".github/workflows", ".vscode"];
    let mut files = Vec::new();
    for d in config_dirs {
        let dir = root.join(d);
        // FAIL, not skip: a moved or renamed config directory must not silently shrink the scan.
        assert!(
            dir.is_dir(),
            "{} is missing. This guard reads the repo's live build configuration from {:?}; if the \
             layout moved, update the list rather than letting the scan quietly cover less.",
            dir.display(),
            config_dirs
        );
        files.extend(text_files_under(&dir));
    }

    let mut found: Vec<(String, usize, String)> = Vec::new();
    for file in &files {
        for (line, name) in extract_package_names(&read_or_fail(file)) {
            found.push((rel(file), line, name));
        }
    }

    assert!(
        !found.is_empty(),
        "extracted ZERO --package/-p names from {} files under {:?}. The CI workflows and \
         scripts/test-all.sh all pass -p, so zero means the extractor broke.",
        files.len(),
        config_dirs
    );

    let bad: Vec<String> = found
        .iter()
        .filter(|(_, _, n)| !names.packages.contains(n))
        .map(|(f, line, n)| format!("  {f}:{line}  -p {n}"))
        .collect();

    assert!(
        bad.is_empty(),
        "these build-config files name a package this workspace does not declare:\n{}\n\
         Workspace packages (from the Cargo.toml files): {:?}\n\
         .vscode/tasks.json carried `-p iris-dev-core` here, the same wrong name as coverage.sh \
         (#317): one rename, several unlinked strings, no compiler between them.",
        bad.join("\n"),
        names.packages
    );
}
