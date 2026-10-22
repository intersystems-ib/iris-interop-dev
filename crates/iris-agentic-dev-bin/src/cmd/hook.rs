//! #529: the Claude Code hook that compiles what the agent just edited.
//!
//! This replaces `scripts/compile-hook.sh`, which compiled a document name without ever uploading
//! the file and printed its verdict on a channel the model does not read. The behaviour lives in
//! `iris_agentic_dev_core::hook` so it can be tested against a mock; this file is the boundary —
//! stdin, discovery, and the exit code.

use anyhow::Result;
use clap::{Args, Subcommand};
use iris_agentic_dev_core::hook::{is_objectscript_source, upload_and_compile, HookVerdict};
use iris_agentic_dev_core::iris::{
    connection::IrisConnection,
    discovery::{discover_iris, IrisDiscovery},
};
use iris_agentic_dev_core::tools::doc::{doc_name_from_source, SourceDocName};

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
    #[arg(long, env = "IRIS_NAMESPACE", default_value = "USER")]
    pub namespace: String,
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

        let content = match std::fs::read_to_string(file_path) {
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
        let ws_path = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        let explicit = match iris_agentic_dev_core::iris::workspace_config::apply_workspace_config(
            None,
            ws_path.as_deref(),
            &self.namespace,
        ) {
            Ok(c) => c,
            Err(e) => block(format!(
                "{doc} was not compiled: the workspace config could not be used ({}).",
                e.message()
            )),
        };

        let iris = match discover_iris(explicit).await {
            IrisDiscovery::Found(c) => c,
            // No instance configured anywhere is not a failure to report: this repo may simply not
            // be an IRIS workspace right now. Staying SILENT is safe because silence makes no
            // claim — the defect in #529 was printing "Compiled OK" when nothing had been
            // compiled, not printing nothing.
            IrisDiscovery::NotFound | IrisDiscovery::Explained => return Ok(()),
        };

        let client = IrisConnection::http_client()?;
        let verdict =
            upload_and_compile(&iris, &client, &self.namespace, &doc, &content, &self.flags).await;

        match verdict {
            HookVerdict::Compiled { doc } => {
                // Paid for on every successful edit, so it is one line. It names the DOCUMENT,
                // which is what makes a #530-style mismatch visible instead of silent.
                let out = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": event,
                        "additionalContext": format!("Compiled {doc} in {} — no errors.", self.namespace),
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
                let mut msg = format!("{doc} did not compile in {}:", self.namespace);
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
