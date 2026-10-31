//! #529: the Claude Code hook that compiles what the agent just edited.
//!
//! This replaces `scripts/compile-hook.sh`, which compiled a document name without ever uploading
//! the file and printed its verdict on a channel the model does not read. The behaviour lives in
//! `iris_agentic_dev_core::hook` so it can be tested against a mock; this file is the boundary —
//! stdin, discovery, and the exit code.

use anyhow::Result;
use clap::{Args, Subcommand};
use iris_agentic_dev_core::hook::{
    decide_hook_path, is_objectscript_source, project_root_for_hook, upload_and_compile,
    HookPathDecision, HookVerdict,
};
use iris_agentic_dev_core::iris::{
    connection::IrisConnection,
    discovery::{discover_iris, IrisDiscovery},
};
use iris_agentic_dev_core::tools::doc::{doc_name_from_source, SourceDocName};
use iris_agentic_dev_core::tools::interop::resolve_namespace;
use std::path::Path;

/// How many compile errors to put in front of the model. IRIS can report a cascade from one bad
/// line (a wrapped macro gives 1 cause and 7 consequences), and only the first few carry the
/// cause — see the `objectscript-macro-cannot-span-lines` note.
const MAX_ERRORS_SHOWN: usize = 10;

#[derive(Args)]
pub struct HookCommand {
    #[command(subcommand)]
    pub action: HookAction,
}

#[derive(Subcommand)]
pub enum HookAction {
    /// Compile the file Claude Code just edited and report the verdict to the model.
    PostEdit(PostEditCommand),
}

#[derive(Args)]
pub struct PostEditCommand {
    /// Compile into this namespace instead of the one the resolved connection carries.
    ///
    /// #540: this deliberately has NO `default_value` and NO `env`. It had both — `USER` and
    /// `IRIS_NAMESPACE` — and clap cannot tell an explicit `--namespace USER` from the default,
    /// so the hook had no way to know whether to defer to a `.iris-agentic-dev.toml` that named
    /// a different namespace. It never deferred, and compiled into `USER` on a connection
    /// pointing somewhere else. `IRIS_NAMESPACE` still works: it is read where every other input
    /// is read, during connection resolution, at the precedence the MCP server uses (the config
    /// file outranks it).
    #[arg(long)]
    pub namespace: Option<String>,
    #[arg(long, default_value = "cuk")]
    pub flags: String,
}

impl HookCommand {
    pub async fn run(self) -> Result<()> {
        match self.action {
            HookAction::PostEdit(c) => c.run().await,
        }
    }
}

/// Report something the agent must act on, and stop.
///
/// Exit 2 with the message on **stderr** is the one channel a `PostToolUse` hook has to the model.
/// stdout with exit 0 reaches the transcript view only, which is exactly how the shell script
/// managed to print "Compile errors in X" to nobody.
fn block(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

impl PostEditCommand {
    pub async fn run(self) -> Result<()> {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        // A payload we cannot parse is not the agent's problem to solve, and blocking an edit over
        // it would make every unrelated hook change look like a compile failure.
        let payload: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();

        let event = payload["hook_event_name"]
            .as_str()
            .unwrap_or("PostToolUse")
            .to_string();

        // `FileChanged` fires on every save, including the user's own in an editor, so it stays
        // opt-in exactly as the shell script had it.
        if event == "FileChanged" && std::env::var("IRIS_COMPILE_ON_SAVE").as_deref() != Ok("true")
        {
            return Ok(());
        }

        let file_path = if event == "FileChanged" {
            payload["file_path"].as_str()
        } else {
            payload["tool_input"]["file_path"].as_str()
        };
        let Some(file_path) = file_path.filter(|p| !p.is_empty()) else {
            return Ok(());
        };

        // Silent and request-free for anything that is not ObjectScript. A hook that comments on
        // every `.md` edit gets switched off, and then it protects nothing.
        if !is_objectscript_source(file_path) {
            return Ok(());
        }
        if std::env::var("IRIS_AUTO_COMPILE").as_deref() == Ok("false") {
            return Ok(());
        }

        // #549: the path arrives in the payload, and until now its EXTENSION was the only test it
        // ever had to pass. A `PostToolUse` hook fires on whatever the agent just edited, and that
        // is not always a file this project owns — a scratch draft in a temp directory, or a file
        // in another checkout, was uploaded into the namespace from `.iris-agentic-dev.toml` and
        // compiled there. Measured consequence: two draft classes written outside a project
        // appeared in that project's namespace while someone else was using the instance.
        //
        // This is the third time the hook has reported a verdict about something other than what
        // the agent edited — #529 the wrong VERSION, #540 the wrong NAMESPACE, this the wrong
        // FILE. So the check lives in the library next to the other two, as one total function
        // over three outcomes, rather than as an `if` here: the binary's only job is to pick the
        // channel, and `decide_hook_path` is what makes that choice visible to a test.
        let ws_path = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        let config_root =
            iris_agentic_dev_core::iris::workspace_config::located_config_root(ws_path.as_deref());
        let claude_project_dir = std::env::var("CLAUDE_PROJECT_DIR").ok();
        let process_cwd = std::env::current_dir().ok();
        let root = project_root_for_hook(
            ws_path.as_deref().map(Path::new),
            config_root.as_deref(),
            payload["cwd"].as_str().map(Path::new),
            claude_project_dir.as_deref().map(Path::new),
            process_cwd.as_deref(),
        );
        let source_path = match decide_hook_path(file_path, &root) {
            // The CANONICAL path, so the file that is read below is the file that was judged.
            HookPathDecision::Proceed { path } => path,
            // Exit 0 — an edit outside this project is not the agent's mistake — but SAY SO on the
            // channel the model reads. A silent exit 0 is what #529 was filed for: the agent then
            // has no verdict and no indication that it has none, and proceeds as though the edit
            // compiled. "I did not check this file" and "this file is fine" have to look different.
            HookPathDecision::Skip { message } => {
                let out = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": event,
                        "additionalContext": message,
                    }
                });
                println!("{out}");
                return Ok(());
            }
            HookPathDecision::Refuse { message } => block(message),
        };
        // Shadowed deliberately: every reader below this line gets the path that was JUDGED, and
        // the payload's unresolved spelling of it is out of scope from here on.
        let file_path = source_path.display();

        let content = match std::fs::read_to_string(&source_path) {
            Ok(c) => c,
            Err(e) => block(format!(
                "{file_path} was not compiled: it could not be read ({e})."
            )),
        };

        // #530: the name comes from the file's own header. A file that does not say what it is gets
        // a refusal, not a name guessed from its path — a guessed name compiles a DIFFERENT
        // document and reports that as success.
        let doc = match doc_name_from_source(&content) {
            SourceDocName::Named(n) => n,
            SourceDocName::Unnamed(why) => block(format!(
                "{file_path} was not compiled: {why}.\n  \
                 Add a `Class <name>` line, or a `ROUTINE <name>` header on the first line."
            )),
        };

        // The same resolution order as the MCP server, so the hook and the server cannot end up
        // talking to different instances — which is how a "compiles clean" verdict can come from
        // an instance that has never seen the file.
        //
        // #540: "instance" was the whole of that promise, and it was TRUE — host, port and
        // credentials all came from the config file — while the namespace came from a clap
        // default and went somewhere else entirely. A verdict is about a document, in a
        // namespace, on an instance; all three have to be resolved from the same place, or the
        // guarantee covers only the dimension whoever wrote it happened to be thinking about.
        //
        // #549: `ws_path` is read ONCE, above, and used both to bound the path and to resolve the
        // connection. Reading it twice would let the directory the writes are bounded to and the
        // file that chooses where they go be answered from different places — which is the same
        // mistake as #540, one layer out.
        let requested = self.namespace.as_deref();
        let explicit = match iris_agentic_dev_core::iris::workspace_config::apply_workspace_config(
            None,
            ws_path.as_deref(),
            requested.unwrap_or("USER"),
        ) {
            Ok(c) => c,
            Err(e) => block(format!(
                "{doc} was not compiled: the workspace config could not be used ({}).",
                e.message()
            )),
        };

        let mut iris = match discover_iris(explicit).await {
            IrisDiscovery::Found(c) => c,
            // No instance configured anywhere is not a failure to report: this repo may simply not
            // be an IRIS workspace right now. Staying SILENT is safe because silence makes no
            // claim — the defect in #529 was printing "Compiled OK" when nothing had been
            // compiled, not printing nothing.
            IrisDiscovery::NotFound | IrisDiscovery::Explained => return Ok(()),
        };

        // #540: ONE reader for the namespace, the same one every tool uses — an explicit
        // `--namespace` wins, otherwise the namespace the connection already settled on. Writing
        // it onto the connection is what makes the two impossible to disagree: `upload_and_compile`
        // has no namespace parameter to be handed a different answer.
        let resolved_ns = resolve_namespace(requested, Some(&iris));
        iris.namespace = resolved_ns;

        let client = IrisConnection::http_client()?;
        let verdict = upload_and_compile(&iris, &client, &doc, &content, &self.flags).await;

        match verdict {
            HookVerdict::Compiled { doc } => {
                // Paid for on every successful edit, so it is one line. It names the DOCUMENT,
                // which is what makes a #530-style mismatch visible instead of silent.
                let out = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": event,
                        "additionalContext": format!("Compiled {doc} in {} — no errors.", iris.namespace),
                    }
                });
                println!("{out}");
                Ok(())
            }
            HookVerdict::CompileFailed {
                doc,
                errors,
                undisclosed,
            } => {
                let mut msg = format!("{doc} did not compile in {}:", iris.namespace);
                for e in errors.iter().take(MAX_ERRORS_SHOWN) {
                    msg.push_str(&format!("\n  {e}"));
                }
                if errors.len() > MAX_ERRORS_SHOWN {
                    msg.push_str(&format!(
                        "\n  … {} more not shown.",
                        errors.len() - MAX_ERRORS_SHOWN
                    ));
                }
                // IRIS counted more than it itemised: say the list is partial rather than let it
                // read as the complete story.
                if let Some(n) = undisclosed {
                    msg.push_str(&format!(
                        "\n  IRIS reported {n} errors but itemised {}, so this list is incomplete.",
                        errors.len()
                    ));
                }
                block(msg)
            }
            HookVerdict::CouldNotTell { doc, why } => block(format!(
                "Whether {doc} compiles is UNKNOWN: {why}.\n  \
                 Treat the edit as unverified — this is not a clean compile."
            )),
        }
    }
}
