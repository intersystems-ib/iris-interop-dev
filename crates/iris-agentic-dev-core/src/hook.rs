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
pub async fn upload_and_compile(
    iris: &IrisConnection,
    client: &reqwest::Client,
    namespace: &str,
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
        namespace,
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
        .compile_document(doc_name, namespace, flags, client)
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
