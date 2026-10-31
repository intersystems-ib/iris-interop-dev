//! #549: `hook post-edit` compiled any ObjectScript path it was handed, wherever it lived.
//!
//! The hook fires on whatever the agent just edited. Until this was bounded, the ONLY test applied
//! to the path was its extension (`cmd/hook.rs`: `is_objectscript_source`, then straight to the
//! read and the upload) — so a scratch draft in a temp directory, or a file in another checkout,
//! was uploaded into the namespace from `.iris-agentic-dev.toml` and compiled there. That is a
//! write into a shared namespace that nobody asked for.
//!
//! ## Why the coverage is HERE and not in an e2e test
//!
//! `scripts/compile-hook.sh` is deterministically SIGKILLed on the development machine this was
//! written on (RC=137), so every `e2e_hook_*` test fails locally regardless of the code — and a
//! new e2e test for this would have been unmeasurable where it was written and unmutatable
//! anywhere. So the decision the hook makes about a path is a pure function in the library, driven
//! here against real directories and real symlinks, and the binary's job is reduced to choosing a
//! channel for the answer. `the_three_outcomes_take_three_different_channels` is what pins that
//! reduction.
//!
//! ## The four ways a path escapes a root
//!
//! Enumerated as MECHANISMS rather than as decisions, because a containment check that handles
//! three of them reads exactly like one that handles four:
//!
//! | mechanism | the test |
//! |---|---|
//! | a plain absolute path elsewhere on disk | `a_file_elsewhere_on_disk_is_outside` |
//! | `..` that lands outside after normalisation | `dot_dot_traversal_out_of_the_root_is_outside` |
//! | a symlink inside the root pointing out of it | `a_symlink_in_the_root_pointing_outside_is_outside` |
//! | a sibling whose name has the root as a string prefix | `a_sibling_sharing_the_roots_name_prefix_is_outside` |
//!
//! The last one is the reason the comparison is component-wise: `/a/project-other` starts with the
//! characters of `/a/project` and is not inside it.

use iris_agentic_dev_core::hook::{
    decide_hook_path, path_within_project, project_root_for_hook, HookPathDecision,
    PathContainment, ProjectRoot, RootSource,
};
use std::path::{Path, PathBuf};

// ── fixtures ─────────────────────────────────────────────────────────────────

/// A real directory tree, because this is a question about the filesystem.
///
/// `canonicalize` is the whole mechanism under test, so a fixture built from invented path strings
/// would be measuring nothing: every candidate would come back `Undetermined`, and a test suite
/// where every case lands in one bucket passes for the wrong reason. `tempfile` already places
/// these under a path that is itself reached through a symlink on macOS (`/var` -> `/private/var`),
/// which is how the unresolved-string version of this check was first found to be wrong — but that
/// is a property of one platform, so `a_root_reached_through_a_symlink_is_the_same_tree` builds its
/// own symlink rather than relying on it.
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        Self { _dir: dir, root }
    }

    /// The directory holding the project, i.e. the root's parent.
    fn outside(&self) -> PathBuf {
        self.root.parent().expect("parent").to_path_buf()
    }

    /// Create a file at `rel` under the tempdir (NOT necessarily under the root) and return it.
    fn file(&self, rel: &str) -> PathBuf {
        let p = self.outside().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&p, "Class Demo.Thing Extends %Persistent\n{\n}\n").expect("write");
        p
    }
}

fn root_of(tree: &Tree) -> ProjectRoot {
    ProjectRoot::Named {
        root: tree.root.clone(),
        source: RootSource::ConfigFile,
    }
}

// ── the predicate: the four escape mechanisms ────────────────────────────────

#[test]
fn a_file_under_the_project_root_is_inside() {
    let t = Tree::new();
    let f = t.file("project/src/Thing.cls");
    assert!(
        matches!(
            path_within_project(&f, &t.root),
            PathContainment::Inside { .. }
        ),
        "a file in the project's own src/ must be Inside, got {:?}",
        path_within_project(&f, &t.root)
    );
}

#[test]
fn a_file_elsewhere_on_disk_is_outside() {
    let t = Tree::new();
    let f = t.file("scratch/Draft.cls");
    assert!(
        matches!(
            path_within_project(&f, &t.root),
            PathContainment::Outside { .. }
        ),
        "a sibling directory of the root is not in the project, got {:?}",
        path_within_project(&f, &t.root)
    );
}

#[test]
fn dot_dot_traversal_out_of_the_root_is_outside() {
    let t = Tree::new();
    // The file really exists; the path reaches it by climbing out of the root and back down.
    t.file("scratch/Draft.cls");
    let escaped = t.root.join("src/../../scratch/Draft.cls");
    let got = path_within_project(&escaped, &t.root);
    assert!(
        matches!(got, PathContainment::Outside { .. }),
        "a path that leaves the root via .. must be Outside, got {got:?}"
    );
    // And the reported path is the NORMALISED one: a refusal quoting the `..` spelling back at the
    // agent does not tell it where the file actually is.
    if let PathContainment::Outside { path, .. } = got {
        assert!(
            !path.to_string_lossy().contains(".."),
            "the resolved path must not carry `..` back into the message: {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_root_pointing_outside_is_outside() {
    let t = Tree::new();
    let real = t.file("scratch/Draft.cls");
    let link = t.root.join("src/Linked.cls");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    // Textually `link` is under the root. Physically it is not the project's file, and compiling it
    // writes someone else's source into the project's namespace.
    let got = path_within_project(&link, &t.root);
    assert!(
        matches!(got, PathContainment::Outside { .. }),
        "a symlink inside the root that points out of it must be Outside, got {got:?}"
    );
}

#[test]
fn a_sibling_sharing_the_roots_name_prefix_is_outside() {
    let t = Tree::new();
    // `<tmp>/project-other` has every character of `<tmp>/project` as a prefix.
    let f = t.file("project-other/src/Thing.cls");
    let got = path_within_project(&f, &t.root);
    assert!(
        matches!(got, PathContainment::Outside { .. }),
        "a directory whose NAME begins with the root's name is not inside it — this is the case a \
         textual starts_with() accepts. got {got:?}"
    );
}

// ── the symlinked root: /tmp vs /private/tmp ─────────────────────────────────

#[cfg(unix)]
#[test]
fn a_root_reached_through_a_symlink_is_the_same_tree() {
    let t = Tree::new();
    let f = t.file("project/src/Thing.cls");
    // `<tmp>/alias` -> `<tmp>/project`, the shape of macOS's /tmp -> /private/tmp.
    let alias = t.outside().join("alias");
    std::os::unix::fs::symlink(&t.root, &alias).expect("symlink");

    // Root named through the symlink, file named physically.
    let got = path_within_project(&f, &alias);
    assert!(
        matches!(got, PathContainment::Inside { .. }),
        "a root named through a symlink is the same tree — an unresolved string comparison says \
         Outside here. got {got:?}"
    );

    // And the mirror image: file named through the symlink, root named physically.
    let via_alias = alias.join("src/Thing.cls");
    let got = path_within_project(&via_alias, &t.root);
    assert!(
        matches!(got, PathContainment::Inside { .. }),
        "a file named through a symlinked root is still in the project. got {got:?}"
    );
}

// ── the third state: a question that could not be answered ───────────────────

#[test]
fn an_unresolvable_path_is_undetermined_not_outside() {
    let t = Tree::new();
    let missing = t.root.join("src/NeverWritten.cls");
    let got = path_within_project(&missing, &t.root);
    assert!(
        matches!(got, PathContainment::Undetermined { .. }),
        "a path that will not resolve is NOT a verdict about where it lives — answering it with \
         Outside is this repo's commonest bug shape. got {got:?}"
    );
}

#[test]
fn an_unresolvable_root_is_undetermined_not_inside_and_not_outside() {
    let t = Tree::new();
    let f = t.file("project/src/Thing.cls");
    let bogus_root = t.outside().join("no-such-root");
    let got = path_within_project(&f, &bogus_root);
    assert!(
        matches!(got, PathContainment::Undetermined { .. }),
        "a root that does not resolve answers nothing about the file. got {got:?}"
    );
}

#[test]
fn the_three_containment_states_are_three_different_answers() {
    let t = Tree::new();
    let inside = t.file("project/src/Thing.cls");
    let outside = t.file("scratch/Draft.cls");
    let missing = t.root.join("src/NeverWritten.cls");

    let a = path_within_project(&inside, &t.root);
    let b = path_within_project(&outside, &t.root);
    let c = path_within_project(&missing, &t.root);
    assert_ne!(a, b, "Inside and Outside must not collapse");
    assert_ne!(b, c, "Outside and Undetermined must not collapse");
    assert_ne!(a, c, "Inside and Undetermined must not collapse");

    // Inside has nothing to report; the other two each have their own words.
    assert_eq!(a.refusal(), None, "a file in the project needs no refusal");
    let rb = b.refusal().expect("Outside must have words");
    let rc = c.refusal().expect("Undetermined must have words");
    assert_ne!(
        rb, rc,
        "'not in this project' and 'I could not tell' must not read the same"
    );
}

// ── the refusals say what happened, and what to do ───────────────────────────

#[test]
fn the_outside_refusal_names_the_file_the_root_and_a_remedy() {
    let t = Tree::new();
    let f = t.file("scratch/Draft.cls");
    let msg = path_within_project(&f, &t.root)
        .refusal()
        .expect("Outside must have words");

    // The file, so the agent knows WHICH edit was not checked.
    assert!(
        msg.contains("Draft.cls"),
        "the refusal must name the file: {msg}"
    );
    // The root, so "outside" is a statement the agent can check rather than take on trust.
    assert!(
        msg.contains(&*t.root.canonicalize().expect("canon").to_string_lossy()),
        "the refusal must name the root it compared against: {msg}"
    );
    // #529's lesson: "I did not check this" must not be mistakable for "this is fine".
    assert!(
        msg.contains("NOT uploaded or compiled"),
        "the refusal must say nothing was uploaded or compiled: {msg}"
    );
    assert!(
        !msg.contains("Compiled "),
        "the refusal must not contain anything readable as a clean compile: {msg}"
    );
    // A remedy, not just a fact about the world.
    assert!(
        msg.contains("OBJECTSCRIPT_WORKSPACE") || msg.contains(".iris-agentic-dev.toml"),
        "the refusal must name a way to change the answer: {msg}"
    );
}

#[test]
fn the_undetermined_refusal_says_the_answer_is_unknown() {
    let t = Tree::new();
    let missing = t.root.join("src/NeverWritten.cls");
    let msg = path_within_project(&missing, &t.root)
        .refusal()
        .expect("Undetermined must have words");
    assert!(
        msg.contains("UNKNOWN"),
        "an unanswerable question must say so in a word the agent cannot miss: {msg}"
    );
    assert!(
        msg.contains("NOT uploaded or compiled"),
        "it must still say nothing was uploaded or compiled: {msg}"
    );
    assert!(
        !msg.to_lowercase().contains("outside the project"),
        "an unknown answer must not be dressed as 'outside the project': {msg}"
    );
}

// ── which directory the hook bounds to ───────────────────────────────────────

fn root_for<'a>(
    ws: Option<&'a str>,
    cfg: Option<&'a str>,
    payload_cwd: Option<&'a str>,
    claude_dir: Option<&'a str>,
    process_cwd: Option<&'a str>,
) -> ProjectRoot {
    project_root_for_hook(
        ws.map(Path::new),
        cfg.map(Path::new),
        payload_cwd.map(Path::new),
        claude_dir.map(Path::new),
        process_cwd.map(Path::new),
    )
}

#[test]
fn an_explicit_workspace_outranks_every_other_candidate() {
    assert_eq!(
        root_for(
            Some("/ws"),
            Some("/cfg"),
            Some("/cwd"),
            Some("/claude"),
            Some("/proc")
        ),
        ProjectRoot::Named {
            root: PathBuf::from("/ws"),
            source: RootSource::ExplicitWorkspace
        }
    );
}

#[test]
fn the_config_file_root_outranks_the_payload_cwd() {
    assert_eq!(
        root_for(
            None,
            Some("/cfg"),
            Some("/cwd"),
            Some("/claude"),
            Some("/proc")
        ),
        ProjectRoot::Named {
            root: PathBuf::from("/cfg"),
            source: RootSource::ConfigFile
        }
    );
}

#[test]
fn the_payload_cwd_is_used_when_no_config_file_was_found() {
    assert_eq!(
        root_for(None, None, Some("/cwd"), Some("/claude"), Some("/proc")),
        ProjectRoot::Named {
            root: PathBuf::from("/cwd"),
            source: RootSource::PayloadCwd
        }
    );
}

#[test]
fn claude_project_dir_is_used_when_the_payload_carries_no_cwd() {
    // `FileChanged` payloads carry only `file_path`, so this is the live case, not a hypothetical.
    assert_eq!(
        root_for(None, None, None, Some("/claude"), Some("/proc")),
        ProjectRoot::Named {
            root: PathBuf::from("/claude"),
            source: RootSource::ClaudeProjectDir
        }
    );
}

#[test]
fn the_process_cwd_is_the_last_resort_and_is_labelled_as_such() {
    assert_eq!(
        root_for(None, None, None, None, Some("/proc")),
        ProjectRoot::Named {
            root: PathBuf::from("/proc"),
            source: RootSource::ProcessCwd
        }
    );
}

#[test]
fn nothing_naming_a_root_leaves_it_unknown_rather_than_defaulting() {
    assert_eq!(
        root_for(None, None, None, None, None),
        ProjectRoot::Unknown,
        "a root nobody named is not a root — defaulting to one bounds nothing while looking bounded"
    );
}

#[test]
fn an_empty_candidate_is_absent_not_a_root() {
    // `OBJECTSCRIPT_WORKSPACE=` is how a shell spells "unset" by accident, and `workspace_root`
    // already treats it as absent. An empty root would canonicalise to the process cwd.
    assert_eq!(
        root_for(Some(""), Some("/cfg"), None, None, None),
        ProjectRoot::Named {
            root: PathBuf::from("/cfg"),
            source: RootSource::ConfigFile
        }
    );
    assert_eq!(
        root_for(Some(""), Some(""), Some(""), Some(""), Some("")),
        ProjectRoot::Unknown
    );
}

// ── the wiring: which channel each outcome takes ─────────────────────────────

#[test]
fn a_file_in_the_project_proceeds_to_the_compile() {
    let t = Tree::new();
    let f = t.file("project/src/Thing.cls");
    let got = decide_hook_path(&f.to_string_lossy(), &root_of(&t));
    assert!(
        matches!(got, HookPathDecision::Proceed { .. }),
        "the hook must still compile the project's own files. got {got:?}"
    );
}

#[test]
fn a_file_outside_the_project_is_skipped_and_says_so() {
    let t = Tree::new();
    let f = t.file("scratch/Draft.cls");
    let got = decide_hook_path(&f.to_string_lossy(), &root_of(&t));
    match got {
        HookPathDecision::Skip { message } => {
            assert!(
                message.contains("Draft.cls") && message.contains("NOT uploaded or compiled"),
                "the skip must be reported, not silent: {message}"
            );
        }
        other => panic!("a file outside the root must be skipped, not {other:?}"),
    }
}

#[test]
fn an_unknown_root_refuses_rather_than_compiling_anything() {
    let t = Tree::new();
    let f = t.file("project/src/Thing.cls");
    let got = decide_hook_path(&f.to_string_lossy(), &ProjectRoot::Unknown);
    match got {
        HookPathDecision::Refuse { message } => {
            assert!(
                message.contains("project root") && message.contains("NOT uploaded or compiled"),
                "an unidentifiable root must say what it could not do: {message}"
            );
        }
        other => panic!(
            "with no root known, nothing can be shown to belong to the project, so the hook must \
             not proceed. got {other:?}"
        ),
    }
}

#[test]
fn an_unresolvable_path_refuses_rather_than_skipping_quietly() {
    let t = Tree::new();
    let missing = t.root.join("src/NeverWritten.cls");
    let got = decide_hook_path(&missing.to_string_lossy(), &root_of(&t));
    assert!(
        matches!(got, HookPathDecision::Refuse { .. }),
        "'I could not tell where this file is' is not a skip. got {got:?}"
    );
}

#[test]
fn the_three_outcomes_take_three_different_channels() {
    // This is the assertion the binary is reduced to honouring: Proceed compiles, Skip exits 0
    // with its message on stdout, Refuse exits 2 with its message on stderr. If two of the three
    // collapsed into one variant here, the binary could not tell them apart however it was
    // written, which is exactly how #529's "exit 0 and print to nobody" happened.
    let t = Tree::new();
    let inside = t.file("project/src/Thing.cls");
    let outside = t.file("scratch/Draft.cls");
    let missing = t.root.join("src/NeverWritten.cls");

    let a = decide_hook_path(&inside.to_string_lossy(), &root_of(&t));
    let b = decide_hook_path(&outside.to_string_lossy(), &root_of(&t));
    let c = decide_hook_path(&missing.to_string_lossy(), &root_of(&t));

    assert!(matches!(a, HookPathDecision::Proceed { .. }), "got {a:?}");
    assert!(matches!(b, HookPathDecision::Skip { .. }), "got {b:?}");
    assert!(matches!(c, HookPathDecision::Refuse { .. }), "got {c:?}");

    // Proceed carries the RESOLVED path, so the caller reads the file it just judged rather than
    // the string it was handed — a check on one path and a read of another is no check at all.
    if let HookPathDecision::Proceed { path } = a {
        assert_eq!(
            path,
            inside.canonicalize().expect("canon"),
            "Proceed must hand back the path that was judged"
        );
    }
}
