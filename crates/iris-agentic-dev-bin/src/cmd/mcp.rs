use anyhow::Result;
use clap::Args;
use iris_agentic_dev_core::{
    iris::discovery::{discover_iris, IrisDiscovery},
    skills::SkillRegistry,
    tools::{ConfigWatcher, IrisTools, Toolset, ToolsetChoice},
};
use rmcp::{transport::stdio, ServiceExt};
use tokio::sync::watch;

/// Start the iris-dev MCP server (stdio transport by default).
///
/// REQUIREMENTS
///   IRIS must have the Atelier REST API enabled. Three ways to achieve this:
///
///   1. Community images (include private web server):
///      iris-community, irishealth-community → port 52773
///
///   2. Enterprise images + ISC Web Gateway container (recommended for production):
///      Use containers.intersystems.com/intersystems/webgateway alongside
///      intersystems/iris. The webgateway container exposes port 80/443 and
///      proxies Atelier REST. Set IRIS_WEB_PORT=<webgateway-host-port>.
///      iris-dev auto-detects webgateway containers in the Docker scan.
///
///   3. Enterprise images standalone: no private web server — requires option 2 above.
///
/// WEBGATEWAY SETUP GOTCHAS (verified 2026-05-03)
///   Three non-obvious bugs in fresh enterprise container + webgateway setups:
///   a) CSP.ini race: patch CSP.ini only after "Configuration_Initialized" appears in it
///   b) Missing credentials: add Username=_SYSTEM + Password=SYS to [LOCAL] in CSP.ini
///      (default tries CSPSystem which doesn't exist in fresh enterprise containers)
///   c) Wrong Apache directive: use "CSP On" in <Location />, not "SetHandler csp-handler-sa"
///   d) Expired password: run UnExpireUserPasswords("*") in %SYS on first start
///   See: https://github.com/intersystems-community/iris-dev/blob/master/light-skills/skills/iris-vscode-objectscript/SKILL.md
///
/// CONNECTION DISCOVERY (in priority order)
///   1. --host flag — an explicitly named target; the config file is not consulted
///   2. .iris-agentic-dev.toml in workspace (walks up from cwd)
///   3. IRIS_HOST / IRIS_WEB_PORT env vars
///   4. IRIS_CONTAINER env var → Docker named container lookup
///   5. VS Code settings.json (objectscript.conn)
///   6. Localhost port scan (52773, 41773, 51773, 8080)
///   7. Auto-scan running Docker containers
///
/// #410: IRIS_HOST used to be merged into the --host slot by clap and so sat at rank 1,
/// above the file — the opposite of what this list used to claim and of what the rest of
/// the tree documents. It is now rank 3, served by the discovery cascade's own env step.
#[derive(Args)]
pub struct McpCommand {
    #[arg(long, default_value = "stdio")]
    pub transport: String,
    #[arg(long, default_value = "8080")]
    pub port: u16,
    /// #410: deliberately NOT `env = "IRIS_HOST"`. This value is the flag that outranks
    /// `.iris-agentic-dev.toml`, so letting clap fill it from the environment put every
    /// IRIS_*-registered server above the file and made the file invisible until it was
    /// touched. IRIS_HOST is read by `discovery.rs` step 2, below the file.
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long, env = "IRIS_WEB_PORT")]
    pub web_port: Option<u16>,
    #[arg(long, env = "IRIS_WEB_PREFIX", default_value = "")]
    pub web_prefix: String,
    /// URL scheme: http or https (default: http)
    #[arg(long, env = "IRIS_SCHEME", default_value = "http")]
    pub scheme: String,
    #[arg(long, env = "IRIS_USERNAME")]
    pub username: Option<String>,
    #[arg(long, env = "IRIS_PASSWORD")]
    pub password: Option<String>,
    #[arg(long, env = "IRIS_NAMESPACE", default_value = "USER")]
    pub namespace: String,
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub config: Option<String>,
    #[arg(long = "subscribe")]
    pub subscribe: Vec<String>,
    #[arg(long, default_value = ".")]
    pub workspace: String,
    /// Tool set to register. `interop` is this fork's DEFAULT: the production-focused profile.
    /// `routines` (#531) is for legacy ObjectScript — routines and globals, no production surface.
    /// `merged` and `nostub` drop the NOT_IMPLEMENTED stubs, `baseline` is everything the binary
    /// carries. Also read from the IRIS_TOOLSET env var.
    ///
    /// No counts here. The four this line used to carry were written once and never updated —
    /// `interop` read "31 interop-focused tools" while INTEROP_TOOLS held 36. `tools/list` on your
    /// own build is the only answer that cannot go stale, and `scripts/validate-tools.sh` prints
    /// the keep-list length on every run.
    #[arg(long, env = "IRIS_TOOLSET", default_value = DEFAULT_TOOLSET)]
    pub toolset: String,
}

/// The profile served when nothing asks for one, as a string because that is what clap's
/// `default_value` takes. `the_clap_default_is_this_crates_default` pins it to `Toolset::default()`
/// so the two spellings of the default cannot drift apart.
const DEFAULT_TOOLSET: &str = "interop";

/// #541: what to tell an operator who named a profile this build does not have.
///
/// Extracted so it can be tested: the only thing that makes this refusal useful instead of
/// annoying is that it quotes the value back AND lists the real names, and a message is the kind
/// of thing that gets shortened later by someone who cannot see which half was load-bearing.
fn unrecognised_toolset(value: &str) -> String {
    format!(
        "--toolset (IRIS_TOOLSET) is `{value}`, which is not a profile this build has. \
         Valid profiles: {}. Nothing was started. An unrecognised name used to fall back to \
         `baseline`, which serves EVERY tool — so a typo silently gave you more than you asked \
         for, which is the opposite of choosing a profile.",
        Toolset::NAMES.join(", ")
    )
}

impl McpCommand {
    pub async fn run(self) -> Result<()> {
        let toolset = match Toolset::parse(&self.toolset) {
            ToolsetChoice::Named(t) => t,
            // `IRIS_TOOLSET=` is "unset", not "nonsense" — the same reading the namespace
            // resolver gives a blank override.
            ToolsetChoice::Blank => Toolset::default(),
            ToolsetChoice::Unrecognised(v) => anyhow::bail!(unrecognised_toolset(&v)),
        };
        tracing::info!(
            "iris-agentic-dev mcp starting — toolset={}",
            toolset.as_str()
        );

        let explicit = if let Some(host) = self.host.clone() {
            use iris_agentic_dev_core::iris::connection::{DiscoverySource, IrisConnection};
            let port = self.web_port.unwrap_or(52773);
            let prefix = self.web_prefix.trim_matches('/');
            let scheme = self.scheme.trim_matches('/');
            let base_url = if prefix.is_empty() {
                format!("{}://{}:{}", scheme, host, port)
            } else {
                format!("{}://{}:{}/{}", scheme, host, port, prefix)
            };
            let username = self.username.as_deref().unwrap_or("_SYSTEM");
            let password = self.password.as_deref().unwrap_or("SYS");
            Some(IrisConnection::new(
                base_url,
                &self.namespace,
                username,
                password,
                DiscoverySource::ExplicitFlag,
            ))
        } else {
            None
        };

        let (iris_tx, iris_rx) =
            watch::channel::<Option<iris_agentic_dev_core::iris::connection::IrisConnection>>(None);

        // Load .iris-agentic-dev.toml — takes precedence over env vars but not CLI flags (FR-006).
        // If --workspace was explicitly passed (not the default "."), warn when no config found.
        let ws_root =
            iris_agentic_dev_core::iris::workspace_config::workspace_root(Some(&self.workspace));
        if self.workspace != "." && !ws_root.join(".iris-agentic-dev.toml").exists() {
            tracing::warn!(
                "No .iris-agentic-dev.toml found at {} — falling back to auto-discovery. \
                 Set IRIS_HOST or IRIS_CONTAINER to connect directly.",
                ws_root.display()
            );
        }
        // _with_path returns the loaded config path so it can be recorded in
        // ConnectionState at startup (not just after hot-reload). Issue #21 / upstream #82.
        // #312: refuse to START rather than serve against a different instance. Every write tool in
        // this server would otherwise target whatever auto-discovery finds, while the user believes
        // the file they wrote is in effect. An explicit --host still bypasses the file entirely, so
        // this is not a dead end.
        let (explicit, startup_config_path) =
            iris_agentic_dev_core::iris::workspace_config::apply_workspace_config_with_path(
                explicit,
                Some(&self.workspace),
                &self.namespace,
            )
            .map_err(|e| anyhow::anyhow!("{}", e.message()))?;

        // #110: the same description a LATER re-probe must target. `discover_iris` consumes
        // its argument, and the CLI-flag / workspace-config connection is not reconstructible
        // from env vars — without this, a retry would silently probe somewhere else.
        let discovery_seed = explicit.clone();
        tokio::spawn(async move {
            let conn = match discover_iris(explicit).await {
                IrisDiscovery::Found(c) => {
                    tracing::info!(
                        "IRIS connected: {}/api/atelier/{} {}",
                        c.base_url,
                        c.atelier_version.version_str(),
                        c.version.as_deref().unwrap_or("?")
                    );
                    Some(c)
                }
                IrisDiscovery::NotFound => {
                    tracing::warn!("No IRIS connection — tools return IRIS_UNREACHABLE");
                    None
                }
                IrisDiscovery::Explained => {
                    // Specific actionable message already emitted — add no noise.
                    None
                }
            };
            let _ = iris_tx.send(conn);
        });

        let mut registry = SkillRegistry::new();
        for owner_repo in &self.subscribe {
            match registry.load_from_github(owner_repo).await {
                Ok(()) => tracing::info!("Subscribed to {}", owner_repo),
                Err(e) => tracing::warn!("Failed to subscribe to {}: {}", owner_repo, e),
            }
        }

        // Wait briefly for discovery — env-var discovery (single HTTP probe) completes in <500ms.
        // The cap keeps the `initialize` response inside the client's own timeout; it is NOT
        // a verdict on whether IRIS exists.
        //
        // #110: it used to behave like one. At 2 seconds, a machine under load (concurrent
        // `cargo clippy --all-targets` was enough, twice) misses the window, and because
        // nothing ever re-probed, `iris` stayed None for the entire session — every tool
        // answering IRIS_UNREACHABLE while `curl localhost:43080/api/atelier/` returned 200
        // the whole time. A room of laptops starting containers at once is exactly that load
        // profile, and it presents as "the MCP tools are broken".
        //
        // The fix is NOT a bigger number. Raising the default would only make `initialize`
        // block longer on a machine with no IRIS at all (measured: ~40 ms today, and the
        // SC-001 p50 gate is 100 ms) while still losing any probe slower than whatever the
        // new number is. The cap stays at 2s and becomes purely a client-timeout budget;
        // what changed is that missing it is no longer fatal — the discovery result is
        // adopted whenever it arrives, and `retry_discovery` in the core re-probes lazily
        // for the case where startup discovery genuinely failed. The override exists for
        // environments that would rather block than serve unconnected.
        let discovery_timeout_ms = std::env::var("IRIS_DISCOVERY_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(2000);
        let mut iris_rx_wait = iris_rx.clone();
        let settled = tokio::time::timeout(
            tokio::time::Duration::from_millis(discovery_timeout_ms),
            iris_rx_wait.wait_for(|v| v.is_some()),
        )
        .await
        .is_ok();
        let iris = iris_rx.borrow().clone();
        if !settled {
            tracing::warn!(
                timeout_ms = discovery_timeout_ms,
                "IRIS discovery did not finish within the startup window — serving now and \
                 adopting the connection when it arrives (set IRIS_DISCOVERY_TIMEOUT_MS to wait longer)"
            );
        }

        // On Windows, stdout opens in text mode which translates \n → \r\n.
        // MCP clients expect bare \n-terminated JSON lines — set stdout/stdin to binary mode.
        #[cfg(windows)]
        unsafe {
            extern "C" {
                fn _setmode(fd: i32, mode: i32) -> i32;
            }
            const O_BINARY: i32 = 0x8000;
            _setmode(0, O_BINARY); // stdin
            _setmode(1, O_BINARY); // stdout
        }

        // Build ConfigWatcher for .iris-agentic-dev.toml hot-reload (034-live-connection-reload).
        // When spawned from a launcher (e.g. Claude Desktop/Code) the CWD is often "/" —
        // fall back to $HOME so the watch path is usable without OBJECTSCRIPT_WORKSPACE
        // (issue #21, upstream 0c922ec).
        let config_root = if ws_root == std::path::Path::new("/") {
            std::env::var("HOME")
                .ok()
                .map(std::path::PathBuf::from)
                .unwrap_or(ws_root)
        } else {
            ws_root
        };
        let config_watcher = ConfigWatcher::new(config_root.join(".iris-agentic-dev.toml"));
        let tools = IrisTools::with_registry_and_toolset(
            iris,
            registry,
            toolset,
            config_watcher,
            startup_config_path,
        )?;

        // #110: tell the tools what a re-probe should target, and that startup discovery is
        // still in flight when it is — so a tool called during the gap says "the probe has
        // not answered yet" instead of "set IRIS_HOST", which is advice for a different
        // problem and was the wrong advice in every reported case.
        tools.set_discovery_state(discovery_seed, !settled);
        if !settled {
            let state_tools = tools.connection.clone();
            let mut rx = iris_rx.clone();
            tokio::spawn(async move {
                let adopted = rx
                    .wait_for(|v| v.is_some())
                    .await
                    .ok()
                    .and_then(|v| v.clone());
                let mut st = state_tools.lock().unwrap();
                st.discovery_pending = false;
                match adopted {
                    Some(c) => {
                        tracing::info!(base_url = %c.base_url, "IRIS discovery completed after startup — connection adopted");
                        let seed = st.discovery_seed.clone();
                        let source = st.source.clone();
                        *st = iris_agentic_dev_core::tools::ConnectionState::from_iris(
                            c, source, None,
                        );
                        st.discovery_seed = seed;
                    }
                    None => tracing::warn!("IRIS discovery finished without a connection"),
                }
            });
        }

        // FR-007: periodically sweep expired elicitation entries.
        {
            let store = tools.elicitation_store.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    store.sweep();
                }
            });
        }

        let service = tools
            .serve(stdio())
            .await
            .inspect_err(|e| tracing::error!("MCP server error: {:?}", e))?;
        // #465: a SIGTERM used to end the process wherever it stood. Any generator future in
        // flight was dropped with its scratch class already compiled, and the DELETE went with it.
        // Returning here instead lets `main` shut the runtime down through its bounded
        // `shutdown_timeout`, which is the window a dropped `ScratchGuard` needs to get its
        // request out.
        // Both arms must have the same type, so the `?` is a statement inside a block rather
        // than the arm's value — `service.waiting()` resolves to a QuitReason we do not use.
        tokio::select! {
            r = service.waiting() => {
                r?;
            }
            _ = shutdown_signal() => tracing::info!("shutting down on signal"),
        }
        Ok(())
    }
}

/// Resolves when the process is asked to stop: SIGTERM (what a container runtime sends) or Ctrl-C.
///
/// On a platform without Unix signals only Ctrl-C is available, and that is not a degradation
/// worth hiding — a missing SIGTERM handler is reported rather than silently skipped, because the
/// consequence (a scratch class left compiled in the namespace) is invisible from here.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::warn!("no SIGTERM handler ({e}); watching Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod toolset_cli_tests {
    use super::*;

    /// The default profile is spelled twice — once as clap's `default_value`, once as
    /// `Toolset::default()` for the blank case. This is the only thing stopping them drifting.
    #[test]
    fn the_clap_default_is_this_crates_default() {
        assert_eq!(
            Toolset::parse(DEFAULT_TOOLSET),
            ToolsetChoice::Named(Toolset::default()),
            "clap's default_value must name the same profile Toolset::default() returns"
        );
        // The control: this would also pass if parse answered Named(default) for EVERYTHING,
        // which is the bug being fixed. A name that is not the default must not come back as it.
        assert_eq!(
            Toolset::parse("routines"),
            ToolsetChoice::Named(Toolset::Routines)
        );
        assert_ne!(Toolset::default(), Toolset::Routines);
    }

    /// The refusal has exactly two jobs: say what was typed, and say what would have worked.
    /// A message missing either one sends the operator back to the source.
    #[test]
    fn the_refusal_names_the_value_and_every_valid_profile() {
        let msg = unrecognised_toolset("routnes");
        assert!(msg.contains("routnes"), "must quote the value back: {msg}");
        for name in Toolset::NAMES {
            assert!(
                msg.contains(name),
                "every valid profile must be listed; {name} is missing from: {msg}"
            );
        }
        // It must also not read as a fallback having happened.
        assert!(
            msg.contains("Nothing was started"),
            "the refusal must say nothing started, or it reads as a warning: {msg}"
        );
    }
}
