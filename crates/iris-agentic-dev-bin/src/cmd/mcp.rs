use anyhow::Result;
use clap::Args;
use iris_agentic_dev_core::{
    iris::discovery::{discover_iris, IrisDiscovery},
    skills::SkillRegistry,
    tools::{ConfigWatcher, IrisTools, Toolset},
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
///   1. --host / IRIS_HOST env var
///   2. .iris-agentic-dev.toml in workspace (walks up from cwd)
///   3. IRIS_CONTAINER env var → Docker named container lookup
///   4. Localhost port scan (52773, 41773, 51773, 8080)
///   5. Auto-scan running Docker containers
#[derive(Args)]
pub struct McpCommand {
    #[arg(long, default_value = "stdio")]
    pub transport: String,
    #[arg(long, default_value = "8080")]
    pub port: u16,
    #[arg(long, env = "IRIS_HOST")]
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
    /// Tool set to register: interop (23 interop-focused tools — DEFAULT for this fork),
    /// merged (46: stubs removed + consolidated), nostub (50: stubs removed),
    /// or baseline (54: all tools). Also read from IRIS_TOOLSET env var.
    #[arg(long, env = "IRIS_TOOLSET", default_value = "interop")]
    pub toolset: String,
}

impl McpCommand {
    pub async fn run(self) -> Result<()> {
        let toolset = Toolset::from_str(&self.toolset);
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
        let (explicit, startup_config_path) =
            iris_agentic_dev_core::iris::workspace_config::apply_workspace_config_with_path(
                explicit,
                Some(&self.workspace),
                &self.namespace,
            );

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
        service.waiting().await?;
        Ok(())
    }
}
