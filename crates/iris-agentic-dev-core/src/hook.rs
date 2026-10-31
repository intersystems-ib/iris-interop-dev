//! #529: what the compile hook tells the agent about the file it just edited.
//!
//! `scripts/compile-hook.sh` made two mistakes that composed into silence. It POSTed
//! `action/compile` with a document name and never PUT the file (compile-hook.sh:53-58), so IRIS
//! judged whatever copy it already held — the previous version, or nothing for a new document. And
//! every path printed to stdout and exited 0 (lines 15/20/23/27/60/67/81/99 and the final echoes),
//! which for a `PostToolUse` hook reaches the transcript view and **not the model**. The agent got
//! no verdict at all, about a version it had not written.
//!
//! The logic lives in the library rather than in the binary so it can be driven against a mock:
//! what is worth asserting here is WHICH requests leave the process and IN WHAT ORDER, and only
//! counting them shows that.

use crate::iris::connection::IrisConnection;
use std::path::{Path, PathBuf};

/// What the hook has to say.
///
/// Each variant maps to exactly one exit code at the CLI boundary, and that mapping is the point
/// of #529 — a verdict the agent cannot read is not a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookVerdict {
    /// Uploaded and compiled clean.
    Compiled { doc: String },
    /// IRIS compiled it and reported errors. These are what has to reach the model.
    CompileFailed {
        doc: String,
        errors: Vec<String>,
        /// IRIS's own `Detected N errors` count when it EXCEEDS the errors we can list, so the
        /// report can say the list is partial instead of implying it is complete.
        /// `CompileResult::detected_error_count` asks for exactly this cross-check.
        undisclosed: Option<usize>,
    },
    /// The outcome is unknown: unreachable, refused, or an unreadable reply.
    ///
    /// Deliberately NOT folded into `CompileFailed`. "It does not compile" and "I could not find
    /// out whether it compiles" are different things to tell an agent, and the script this
    /// replaces said neither — it said "Compiled OK" for an HTTP 401 with a zero-byte body, which
    /// is the defect `scripts/test-compile-hook.sh` was written to pin.
    CouldNotTell { doc: String, why: String },
}

impl HookVerdict {
    /// The document this verdict is about, whatever the outcome.
    pub fn doc(&self) -> &str {
        match self {
            Self::Compiled { doc }
            | Self::CompileFailed { doc, .. }
            | Self::CouldNotTell { doc, .. } => doc,
        }
    }

    /// Whether the agent needs to see this. A clean compile is worth one short line; the other two
    /// have to interrupt, because the agent's next move depends on them.
    pub fn is_actionable(&self) -> bool {
        !matches!(self, Self::Compiled { .. })
    }
}

/// Upload the edited source, then compile it. In that order, which is the fix.
///
/// The upload is awaited before the compile is issued, so a refused upload can never be followed
/// by a compile verdict about the older copy — that composition is what made the old hook report
/// on a version the agent did not write.
///
/// #540: there is deliberately NO `namespace` parameter. It had one, and `cmd/hook.rs` passed its
/// own CLI default (`USER`) while the connection had been resolved from a `.iris-agentic-dev.toml`
/// naming a different namespace — right host, right port, right credentials, and a clean verdict
/// about a document the agent's other tools cannot see. Reading `iris.namespace` means a caller
/// cannot hand this function a namespace that disagrees with the connection it is handing over;
/// the override belongs at the call site, on the connection, via `interop::resolve_namespace`.
pub async fn upload_and_compile(
    iris: &IrisConnection,
    client: &reqwest::Client,
    doc_name: &str,
    content: &str,
    flags: &str,
) -> HookVerdict {
    let could_not_tell = |why: String| HookVerdict::CouldNotTell {
        doc: doc_name.to_string(),
        why,
    };

    // 1. PUT the source. `ignoreConflict=1` matches `cmd/compile.rs`: the agent's file on disk is
    //    the intended truth here, and a timestamp conflict against the server's copy is exactly
    //    the situation this hook exists to resolve.
    let put_url = iris.versioned_ns_url(
        &iris.namespace,
        &format!("/doc/{}?ignoreConflict=1", urlencoding::encode(doc_name)),
    );
    let lines: Vec<&str> = content.lines().collect();
    let put_resp = match client
        .put(&put_url)
        .basic_auth(&iris.username, Some(&iris.password))
        .json(&serde_json::json!({"enc": false, "content": lines}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return could_not_tell(format!("the upload did not reach IRIS: {e}")),
    };
    if !put_resp.status().is_success() {
        let status = put_resp.status();
        let hint = match status.as_u16() {
            401 | 403 => " — credentials rejected",
            404 => " — check the namespace",
            _ => "",
        };
        return could_not_tell(format!("the upload was refused: HTTP {status}{hint}"));
    }
    // A 2xx can still carry a refusal in the envelope, the same shape as a locked document on
    // delete. Reading only the status would let the compile below report on the OLD copy, which is
    // the original defect wearing a different hat.
    let put_body: serde_json::Value = put_resp.json().await.unwrap_or_default();
    if let Some(errs) = put_body["status"]["errors"].as_array() {
        if !errs.is_empty() {
            let msg = errs[0]["error"].as_str().unwrap_or("no reason given");
            return could_not_tell(format!("IRIS would not store the source: {msg}"));
        }
    }

    // 2. Only now is a compile verdict about the file the agent edited.
    let result = match iris
        .compile_document(doc_name, &iris.namespace, flags, client)
        .await
    {
        Ok(r) => r,
        Err(e) => return could_not_tell(format!("the compile request failed: {e}")),
    };

    if result.success() {
        return HookVerdict::Compiled {
            doc: doc_name.to_string(),
        };
    }
    // If IRIS counted more errors than it itemised, say so rather than present a partial list as
    // the whole story.
    let undisclosed = result
        .detected_error_count()
        .filter(|n| *n > result.errors.len());
    HookVerdict::CompileFailed {
        doc: doc_name.to_string(),
        errors: result.errors,
        undisclosed,
    }
}

/// The ObjectScript document extensions this hook acts on. `.int` is absent on purpose: it is
/// generated from a `.mac`, so compiling an edited `.int` would be compiling an artefact.
pub const HOOK_EXTENSIONS: &[&str] = &["cls", "mac", "inc"];

/// Whether a path is a file this hook has any business compiling.
///
/// A miss here must be silent and request-free at the call site: a hook that speaks up about
/// every `.md` edit is one the user turns off, and then it protects nothing.
pub fn is_objectscript_source(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => HOOK_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

// ── #549: does this path belong to the project the hook is running for? ──────

/// Where a path lies relative to the project root.
///
/// Three cases, not two. `Undetermined` is deliberately NOT folded into `Outside`: "this file is
/// not in the project" and "I could not work out whether this file is in the project" send the
/// agent in different directions, and the second must never arrive wearing the first's clothes.
/// That folding is the defect class CLAUDE.md opens with, and a containment check is a place it
/// would be free to happen — `canonicalize` fails for a missing file, a permission error and a
/// symlink loop alike, and every one of those is "I cannot tell", not "it lives elsewhere".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathContainment {
    /// Resolved, and under the root. Both paths are CANONICAL, so a caller that reads
    /// `path` reads the file that was judged rather than the string it passed in.
    Inside { path: PathBuf, root: PathBuf },
    /// Resolved, and not under the root.
    Outside { path: PathBuf, root: PathBuf },
    /// One of the two paths would not resolve, so there is no answer. `path` is the path as it
    /// was GIVEN — there is no canonical form of it to report.
    Undetermined { path: PathBuf, why: String },
}

impl PathContainment {
    /// What the agent is told when the hook declines to act on this path.
    ///
    /// `None` for `Inside`: a file the project owns has nothing to report, and a hook that
    /// comments on every successful edit is one the user switches off.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Inside { .. } => None,
            Self::Outside { path, root } => Some(outside_refusal(path, root)),
            Self::Undetermined { path, why } => Some(undetermined_refusal(path, why)),
        }
    }
}

/// ONE reader of this sentence, shared by the containment verdict and the hook's decision.
///
/// Two copies is how a report and the thing it reports on drift apart — the same shape as #530's
/// two parsers of the `Class` line, which disagreed about whether `Class {` was a name.
fn outside_refusal(path: &Path, root: &Path) -> String {
    format!(
        "{} was NOT uploaded or compiled: it is outside the project root {}.\n  \
         The hook compiles only files inside the project it is running for — uploading one the \
         project does not own is a write into a shared namespace that nobody asked for.\n  \
         Edit that file from its own project, or point the hook at the tree that owns it: set \
         OBJECTSCRIPT_WORKSPACE to that root, or put a .iris-agentic-dev.toml there.",
        path.display(),
        root.display()
    )
}

fn undetermined_refusal(path: &Path, why: &str) -> String {
    format!(
        "Whether {} is inside the project root is UNKNOWN: {why}.\n  \
         It was NOT uploaded or compiled. This is not a finding that the file lives elsewhere — \
         treat the edit as unverified, and re-run it once the path resolves.",
        path.display()
    )
}

/// Whether `path` is a file of the project rooted at `root`.
///
/// Both sides are canonicalised first, and the comparison is then `Path::starts_with`, which
/// compares path COMPONENTS rather than characters. Both halves are load-bearing and each was
/// wrong in a different reported way:
///
/// * **Unresolved strings.** On macOS `/tmp` is a symlink to `/private/tmp`, so a path and a root
///   naming the same directory through different links compare as different trees — in either
///   direction. That is what defeated the first attempt at this check, and
///   `a_root_reached_through_a_symlink_is_the_same_tree` builds its own symlink to pin it on every
///   platform rather than relying on macOS's.
/// * **A textual prefix.** `/a/project-other` begins with every character of `/a/project` and is
///   not inside it. `a_sibling_sharing_the_roots_name_prefix_is_outside` is that case, and it is
///   the one a `starts_with` on strings accepts.
///
/// Resolving also settles `..` and any symlink on the way: a link inside the root pointing out of
/// it resolves to its target and is correctly Outside, which a lexical normalisation would miss.
pub fn path_within_project(path: &Path, root: &Path) -> PathContainment {
    let resolved_root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return PathContainment::Undetermined {
                path: path.to_path_buf(),
                why: format!(
                    "the project root {} would not resolve ({e})",
                    root.display()
                ),
            }
        }
    };
    let resolved_path = match path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return PathContainment::Undetermined {
                path: path.to_path_buf(),
                why: format!("{} would not resolve ({e})", path.display()),
            }
        }
    };
    if resolved_path.starts_with(&resolved_root) {
        PathContainment::Inside {
            path: resolved_path,
            root: resolved_root,
        }
    } else {
        PathContainment::Outside {
            path: resolved_path,
            root: resolved_root,
        }
    }
}

/// Which input named the project root, in the order they are consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// `OBJECTSCRIPT_WORKSPACE`, or an explicit workspace argument — somebody said outright which
    /// tree this hook serves, so nothing else gets to overrule it.
    ExplicitWorkspace,
    /// The directory holding `.iris-agentic-dev.toml`: the project whose connection the hook is
    /// about to use. Bounding to anything else would let the file that chooses the NAMESPACE and
    /// the directory that bounds the WRITES disagree, which is #540 one layer out.
    ConfigFile,
    /// The `cwd` in the hook payload — Claude Code's own idea of which project this edit belongs
    /// to. Present on `PostToolUse`.
    PayloadCwd,
    /// `CLAUDE_PROJECT_DIR`. The `FileChanged` payload carries only `file_path`, so for that event
    /// this is the first candidate the payload cannot supply.
    ClaudeProjectDir,
    /// Where the hook process happens to be standing. Last, and weakest: launched from a
    /// subdirectory it bounds to that subdirectory, which is narrower than the project. It is here
    /// so that a tree with no config file and a payload with no `cwd` still bounds to SOMETHING
    /// rather than refusing every edit.
    ProcessCwd,
}

/// The project root, or the fact that nothing named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectRoot {
    Named {
        root: PathBuf,
        source: RootSource,
    },
    /// No candidate named a root. Not quietly defaulted to one: a root nobody named bounds
    /// nothing while making the hook look bounded.
    Unknown,
}

/// Pick the directory the hook bounds edits to, highest-precedence candidate first.
///
/// Every input is a parameter rather than an environment read, so the precedence can be tested
/// without `set_var` — two tests mutating the process environment in parallel make each other
/// pass, and this crate's test binaries run threaded by default.
pub fn project_root_for_hook(
    explicit_workspace: Option<&Path>,
    config_root: Option<&Path>,
    payload_cwd: Option<&Path>,
    claude_project_dir: Option<&Path>,
    process_cwd: Option<&Path>,
) -> ProjectRoot {
    let candidates = [
        (RootSource::ExplicitWorkspace, explicit_workspace),
        (RootSource::ConfigFile, config_root),
        (RootSource::PayloadCwd, payload_cwd),
        (RootSource::ClaudeProjectDir, claude_project_dir),
        (RootSource::ProcessCwd, process_cwd),
    ];
    for (source, candidate) in candidates {
        // `OBJECTSCRIPT_WORKSPACE=` is how a shell spells "unset" by accident, and an empty path
        // canonicalises to the process cwd — a root that would silently be something else
        // entirely. `workspace_root` already treats the empty string as absent.
        if let Some(root) = candidate.filter(|p| !p.as_os_str().is_empty()) {
            return ProjectRoot::Named {
                root: root.to_path_buf(),
                source,
            };
        }
    }
    ProjectRoot::Unknown
}

/// What the hook does with the path it was handed, before anything is read or uploaded.
///
/// One variant per CHANNEL at the CLI boundary, the same discipline as `HookVerdict`: a decision
/// the binary cannot tell apart is a decision it cannot report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookPathDecision {
    /// Compile it. Carries the CANONICAL path, so the file that gets read is the file that was
    /// judged — a check on one path followed by a read of another is no check at all.
    Proceed { path: PathBuf },
    /// Do not compile it, exit 0, and print this. Exit 0 because an edit outside the project is
    /// not the agent's mistake; the message because #529 was filed for exiting 0 with nothing to
    /// say, which the agent reads as a clean compile.
    Skip { message: String },
    /// Do not compile it, and interrupt with this. The question could not be answered, and an
    /// unanswered question must not be delivered through the same channel as an answer.
    Refuse { message: String },
}

/// Bound an edited path to the project, and say which channel the answer takes.
pub fn decide_hook_path(file_path: &str, root: &ProjectRoot) -> HookPathDecision {
    let ProjectRoot::Named { root, .. } = root else {
        return HookPathDecision::Refuse {
            message: format!(
                "{file_path} was NOT uploaded or compiled: the project root could not be \
                 identified, so there is no tree to check it against.\n  \
                 Set OBJECTSCRIPT_WORKSPACE to the project root, or put a \
                 .iris-agentic-dev.toml there."
            ),
        };
    };
    match path_within_project(Path::new(file_path), root) {
        PathContainment::Inside { path, .. } => HookPathDecision::Proceed { path },
        PathContainment::Outside { path, root } => HookPathDecision::Skip {
            message: outside_refusal(&path, &root),
        },
        PathContainment::Undetermined { path, why } => HookPathDecision::Refuse {
            message: undetermined_refusal(&path, &why),
        },
    }
}
