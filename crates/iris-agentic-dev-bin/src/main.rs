use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::ffi::OsString;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod cmd;

#[derive(Parser)]
#[command(
    name = "iris-interop-dev",
    version,
    about = "CLI and package manager for InterSystems IRIS developer ecosystem",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable debug logging
    #[arg(long, global = true)]
    verbose: bool,

    /// List discovered iris-agentic-dev-* plugin commands on PATH
    #[arg(long)]
    list_plugins: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio or HTTP transport)
    Mcp(cmd::mcp::McpCommand),
    /// Compile ObjectScript .cls files on IRIS
    Compile(cmd::compile::CompileCommand),
    /// Initialize a .iris-dev.toml workspace config
    Init(cmd::init::InitCommand),
    /// Install packages from iris-dev.toml
    Install(cmd::install::InstallCommand),
    // #86: any other leading token is a plugin invocation — `iris-interop-dev <name> [args…]`
    // execs `iris-agentic-dev-<name>` from PATH. Declaring this variant is what turns on clap's
    // allow_external_subcommands; without it clap exits 2 with "unrecognized subcommand" and the
    // fallback below is unreachable. OsString (not String) so a non-UTF-8 argument reaches the
    // plugin intact instead of dying at parse time. clap's derive requires this exact spelling —
    // `Vec<std::ffi::OsString>` is rejected by the macro.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// A file handle shared by the tracing layer. Cloned per write; a poisoned lock is
/// recovered rather than panicked on — losing a log line must never take the server
/// down, since logging is a diagnostic aid, not a feature.
#[derive(Clone)]
struct FileSink(Arc<Mutex<std::fs::File>>);

impl Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.flush()
    }
}

/// Issue #58: traces used to exist only on stderr, which an MCP stdio client does
/// not persist — so nothing survived a session, exactly when a post-mortem is
/// wanted. `IRIS_LOG_FILE=<path>` mirrors the same output (same RUST_LOG/--verbose
/// filter) to a file, appending, and is off unless set.
///
/// Opening the file must never be fatal: on failure we warn once on stderr and run
/// without it.
fn init_tracing(verbose: bool) {
    let filter = EnvFilter::from_default_env().add_directive(if verbose {
        tracing::Level::DEBUG.into()
    } else {
        tracing::Level::WARN.into()
    });
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);

    let sink = std::env::var("IRIS_LOG_FILE")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .and_then(|path| match open_log_file(&path) {
            Ok(sink) => Some(sink),
            Err(e) => {
                eprintln!("iris-interop-dev: cannot write IRIS_LOG_FILE={path}: {e} — continuing without a log file");
                None
            }
        });

    match sink {
        Some(sink) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(move || sink.clone())
                .with_ansi(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
        }
    }
}

/// Open the log file for append and stamp a session banner, so consecutive runs in
/// one file are separable and every session records which build produced it.
/// Deliberately records no environment values — IRIS_PASSWORD must never land here.
fn open_log_file(path: &str) -> std::io::Result<FileSink> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let _ = writeln!(
        file,
        "=== iris-interop-dev {} session start {} pid={} ===",
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        std::process::id()
    );
    Ok(FileSink(Arc::new(Mutex::new(file))))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.verbose);

    if cli.list_plugins {
        cmd::plugin::list_plugins();
        return Ok(());
    }

    match cli.command {
        Some(Commands::Mcp(cmd)) => cmd.run().await,
        Some(Commands::Compile(cmd)) => cmd.run().await,
        Some(Commands::Init(cmd)) => cmd.run().await,
        Some(Commands::Install(cmd)) => cmd.run().await,
        Some(Commands::External(argv)) => {
            // clap always puts the command name first; stay defensive rather than index.
            let Some((name, rest)) = argv.split_first() else {
                eprintln!(
                    "iris-interop-dev: empty command. Run `iris-interop-dev --help` for usage."
                );
                std::process::exit(1);
            };
            // #86: the built-in names come from clap, so the "not a built-in subcommand
            // (…)" message and the near-miss tip cannot drift from the actual CLI.
            let command = Cli::command();
            let builtins: Vec<&str> = command
                .get_subcommands()
                .map(|c| c.get_name())
                .filter(|n| *n != "external")
                .collect();
            // Never returns Ok: it execs the plugin, or exits 1 with a message when
            // no such plugin is on PATH.
            cmd::plugin::try_dispatch_plugin(&name.to_string_lossy(), rest, &builtins)
        }
        None => {
            eprintln!("Run `iris-interop-dev --help` for usage.");
            std::process::exit(1);
        }
    }
}
