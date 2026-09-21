//! Per-workspace IRIS connection config via `.iris-agentic-dev.toml`.
//!
//! Priority order: CLI flags > .iris-agentic-dev.toml > env vars > auto-discovery.

use crate::iris::connection::{DiscoverySource, IrisConnection};
use serde::Deserialize;
use std::path::PathBuf;

/// Parsed contents of `.iris-agentic-dev.toml`. All fields are optional.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct WorkspaceConfig {
    pub container: Option<String>,
    pub namespace: Option<String>,
    pub host: Option<String>,
    #[serde(alias = "port")]
    pub web_port: Option<u16>,
    /// URL path prefix for the IRIS web gateway, e.g. "irisaicore" when the
    /// Atelier API is served at http://host:port/irisaicore/api/atelier/...
    /// Corresponds to intersystems.servers[x].webServer.pathPrefix in VS Code settings.
    pub web_prefix: Option<String>,
    /// URL scheme: "http" or "https". Defaults to "http".
    /// Set to "https" for TLS-protected IRIS web gateways.
    pub scheme: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// When true, skip HTTP/Atelier REST and use docker exec exclusively.
    /// Use for containers without a web server (e.g. community IRIS with no web gateway).
    /// Requires IRIS_CONTAINER to be set or container= in config.
    #[serde(default)]
    pub docker_only: bool,
}

/// Which way a config file that EXISTS turned out to be unusable.
///
/// #312: these were both collapsed into the same `None` as "there is no config file", and the three
/// are not the same statement. Absent means "the defaults apply", which is legitimate and common.
/// The other two mean "the user wrote down where to connect and we ignored it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigProblem {
    /// The file is there and could not be read — permissions, a dangling symlink, a directory.
    Unreadable,
    /// The file was read and is not valid TOML, or does not match `WorkspaceConfig`.
    Unparseable,
}

/// A config file exists at `path` and cannot be used.
///
/// #312: the reason this is an error rather than a fallback is specific. `.iris-agentic-dev.toml`
/// carries `host`, `namespace`, `container` and credentials, and it takes precedence over the
/// environment. Treating a typo in it as "no config" does not fall back to nothing — it falls back
/// to `IRIS_HOST`/auto-discovery, i.e. to **a different instance**, silently. The observable
/// consequence is a write landing in the wrong namespace, and nothing printed at the moment the
/// decision was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusableConfig {
    pub path: PathBuf,
    pub problem: ConfigProblem,
    /// The underlying IO or TOML error, preserved — it names the line for a parse failure, and it is
    /// the only thing that says WHICH key is wrong.
    pub detail: String,
}

impl UnusableConfig {
    /// The text shown when a caller refuses to continue. Says what was found, why it is fatal, and
    /// the two ways out — including the explicit opt-out, so "abort" never becomes a dead end.
    pub fn message(&self) -> String {
        let what = match self.problem {
            ConfigProblem::Unreadable => "could not be read",
            ConfigProblem::Unparseable => "is not valid TOML",
        };
        format!(
            "{} {} — refusing to continue: {}\n\n\
             This file takes precedence over IRIS_HOST and auto-discovery, so ignoring it would \
             silently connect somewhere else instead of where you wrote down. Fix the file, or \
             move it aside to fall back to the environment deliberately.",
            self.path.display(),
            what,
            self.detail
        )
    }
}

impl std::fmt::Display for UnusableConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for UnusableConfig {}

/// Resolve the workspace root path.
/// Priority: OBJECTSCRIPT_WORKSPACE env var > workspace_path arg > walk up from cwd.
///
/// When no explicit path is given, walks up from current_dir() looking for .iris-agentic-dev.toml
/// (git-style discovery). This ensures the config is found even when the MCP server is
/// launched from a parent directory (e.g. by an IDE that sets cwd to the home directory).
pub fn workspace_root(workspace_path: Option<&str>) -> PathBuf {
    if let Ok(ws) = std::env::var("OBJECTSCRIPT_WORKSPACE") {
        if !ws.is_empty() {
            return PathBuf::from(ws);
        }
    }
    if let Some(p) = workspace_path {
        if !p.is_empty() && p != "." {
            return PathBuf::from(p);
        }
    }
    // Walk up from current directory looking for .iris-agentic-dev.toml
    // (fall back to legacy .iris-dev.toml for backward compatibility)
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    let mut legacy_root: Option<PathBuf> = None;
    loop {
        if dir.join(".iris-agentic-dev.toml").exists() {
            return dir.to_path_buf();
        }
        if legacy_root.is_none() && dir.join(".iris-dev.toml").exists() {
            legacy_root = Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    // If no .iris-agentic-dev.toml found but legacy .iris-dev.toml exists, use that dir.
    legacy_root.unwrap_or(cwd)
}

/// Like [`load_workspace_config`] but also returns the path of the file that was loaded,
/// so callers can record it in `ConnectionState` at startup (issue #21, upstream #82).
pub fn load_workspace_config_with_path(
    workspace_path: Option<&str>,
) -> Result<Option<(WorkspaceConfig, std::path::PathBuf)>, UnusableConfig> {
    let root = workspace_root(workspace_path);
    let config_path = if root.join(".iris-agentic-dev.toml").exists() {
        root.join(".iris-agentic-dev.toml")
    } else if root.join(".iris-dev.toml").exists() {
        root.join(".iris-dev.toml")
    } else {
        // Genuinely absent. The ONLY case where falling through to the environment is correct.
        return Ok(None);
    };

    // #312: this was `std::fs::read_to_string(&config_path).ok()?` followed by `Err(_) => None`.
    // Two failures, no log line of any kind, and both indistinguishable from "no config file" at
    // every call site. `load_workspace_config` below at least warned; this one was silent, which is
    // the shape a fix has to cover at BOTH layers rather than only where it was noticed.
    let contents = std::fs::read_to_string(&config_path).map_err(|e| UnusableConfig {
        path: config_path.clone(),
        problem: ConfigProblem::Unreadable,
        detail: e.to_string(),
    })?;
    let cfg = toml::from_str::<WorkspaceConfig>(&contents).map_err(|e| UnusableConfig {
        path: config_path.clone(),
        problem: ConfigProblem::Unparseable,
        detail: e.to_string(),
    })?;
    Ok(Some((cfg, config_path)))
}

/// Like [`apply_workspace_config`] but also returns the path of the config file that was
/// loaded, so callers can record it in `ConnectionState` at startup rather than only
/// after the first hot-reload cycle.
pub fn apply_workspace_config_with_path(
    explicit: Option<IrisConnection>,
    workspace_path: Option<&str>,
    namespace: &str,
) -> Result<(Option<IrisConnection>, Option<std::path::PathBuf>), UnusableConfig> {
    if explicit.is_some() {
        // A CLI flag outranks the file, so a broken file is not consulted and not fatal. This is the
        // deliberate escape hatch: an explicit --host gets you moving without editing the file.
        return Ok((explicit, None));
    }
    match load_workspace_config_with_path(workspace_path)? {
        Some((cfg, path)) => Ok((workspace_config_to_connection(&cfg, namespace), Some(path))),
        None => Ok((None, None)),
    }
}

/// Load `.iris-agentic-dev.toml` from the resolved workspace root.
///
/// `Ok(None)` means the file does not exist — the defaults apply, which is legitimate.
/// `Err(UnusableConfig)` means a file IS there and could not be used; see [`UnusableConfig`] for why
/// that is not a fallback.
///
/// #312: this used to return `Option` and fold "unreadable" and "unparseable" into `None` alongside
/// "absent". It logged a `warn!` for the two failures, which was better than the silence in
/// [`load_workspace_config_with_path`] — but a warning does not stop the caller, and the caller then
/// connected somewhere else. Now delegates, so there is a single implementation of the decision.
pub fn load_workspace_config(
    workspace_path: Option<&str>,
) -> Result<Option<WorkspaceConfig>, UnusableConfig> {
    if workspace_root(workspace_path)
        .join(".iris-dev.toml")
        .exists()
        && !workspace_root(workspace_path)
            .join(".iris-agentic-dev.toml")
            .exists()
    {
        tracing::debug!(
            "Using legacy .iris-dev.toml (consider renaming to .iris-agentic-dev.toml)"
        );
    }
    match load_workspace_config_with_path(workspace_path) {
        Ok(Some((cfg, path))) => {
            tracing::debug!("Loaded {}", path.display());
            Ok(Some(cfg))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            // Still logged, because the log is where an operator looks; but the caller is now told
            // too, and it is the caller that decides whether to keep running.
            tracing::error!("{}", e.message());
            Err(e)
        }
    }
}

/// Apply workspace config to set up the connection environment.
///
/// If `host` is specified: returns `Some(IrisConnection)` that will be passed directly
/// to `discover_iris()` as the explicit override.
///
/// If `container` is specified (but not host): sets `IRIS_CONTAINER` (and optionally
/// `IRIS_NAMESPACE`, `IRIS_USERNAME`, `IRIS_PASSWORD`) so the standard discovery cascade
/// picks up the container. Returns `None` to let discovery proceed normally.
///
/// If neither is specified: returns `None` — no connection info in the config.
pub fn workspace_config_to_connection(
    cfg: &WorkspaceConfig,
    namespace_default: &str,
) -> Option<IrisConnection> {
    // host + web_port → explicit HTTP/HTTPS connection (highest priority, no docker needed)
    if let Some(ref host) = cfg.host {
        let port = cfg.web_port.unwrap_or(52773);
        let scheme = cfg
            .scheme
            .clone()
            .or_else(|| std::env::var("IRIS_SCHEME").ok())
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http".to_string());
        let prefix = cfg
            .web_prefix
            .clone()
            .or_else(|| std::env::var("IRIS_WEB_PREFIX").ok())
            .map(|p| p.trim_matches('/').to_string())
            .filter(|p| !p.is_empty());
        let base_url = match prefix {
            Some(p) => format!("{}://{}:{}/{}", scheme, host, port, p),
            None => format!("{}://{}:{}", scheme, host, port),
        };
        let namespace = cfg
            .namespace
            .clone()
            .or_else(|| std::env::var("IRIS_NAMESPACE").ok())
            .unwrap_or_else(|| namespace_default.to_string());
        let username = cfg
            .username
            .clone()
            .or_else(|| std::env::var("IRIS_USERNAME").ok())
            .unwrap_or_else(|| "_SYSTEM".to_string());
        let password = cfg
            .password
            .clone()
            .or_else(|| std::env::var("IRIS_PASSWORD").ok())
            .unwrap_or_else(|| "SYS".to_string());
        // If container is also specified alongside host, update IRIS_CONTAINER so docker
        // exec tools (iris_execute fallback, iris_test, etc.) target the right container,
        // and use DiscoverySource::Docker so check_config exposes the container name
        // instead of reporting container: null (issue #21, upstream #89).
        if let Some(ref container) = cfg.container {
            std::env::set_var("IRIS_CONTAINER", container);
            return Some(IrisConnection::new(
                base_url,
                namespace,
                username,
                password,
                DiscoverySource::Docker {
                    container_name: container.clone(),
                },
            ));
        }
        return Some(IrisConnection::new(
            base_url,
            namespace,
            username,
            password,
            DiscoverySource::EnvVar,
        ));
    }

    // container → inject into env so discover_iris() docker step picks it up
    if let Some(ref container) = cfg.container {
        std::env::set_var("IRIS_CONTAINER", container);
        let ns = cfg
            .namespace
            .clone()
            .or_else(|| std::env::var("IRIS_NAMESPACE").ok())
            .unwrap_or_else(|| namespace_default.to_string());
        let username = cfg
            .username
            .clone()
            .or_else(|| std::env::var("IRIS_USERNAME").ok())
            .unwrap_or_else(|| "_SYSTEM".to_string());
        let password = cfg
            .password
            .clone()
            .or_else(|| std::env::var("IRIS_PASSWORD").ok())
            .unwrap_or_else(|| "SYS".to_string());
        if let Some(ref ns_val) = cfg.namespace {
            std::env::set_var("IRIS_NAMESPACE", ns_val);
        }
        if let Some(ref user) = cfg.username {
            std::env::set_var("IRIS_USERNAME", user);
        }
        if let Some(ref pass) = cfg.password {
            std::env::set_var("IRIS_PASSWORD", pass);
        }
        if cfg.docker_only {
            // docker_only=true: skip HTTP entirely, use docker exec for all operations.
            // Return a connection with an unreachable URL — HTTP calls will fail fast,
            // triggering the docker exec fallback in iris_execute/iris_compile etc.
            return Some(IrisConnection::new(
                "http://127.0.0.1:1",
                ns,
                username,
                password,
                DiscoverySource::Docker {
                    container_name: container.clone(),
                },
            ));
        }
        return None; // discover_iris() will find the container via IRIS_CONTAINER
    }

    None
}

/// Apply workspace config to an existing explicit connection override.
///
/// If `explicit` is already set (from CLI flags), returns it unchanged.
/// Otherwise loads `.iris-agentic-dev.toml` from `workspace_path` and applies it:
/// - `host` config → returns `Some(IrisConnection)`
/// - `container` config → sets `IRIS_CONTAINER` env var, returns `None`
/// - no config / no relevant fields → returns `None`
pub fn apply_workspace_config(
    explicit: Option<IrisConnection>,
    workspace_path: Option<&str>,
    namespace: &str,
) -> Result<Option<IrisConnection>, UnusableConfig> {
    if explicit.is_some() {
        // See apply_workspace_config_with_path: an explicit flag outranks the file.
        return Ok(explicit);
    }
    match load_workspace_config(workspace_path)? {
        Some(cfg) => Ok(workspace_config_to_connection(&cfg, namespace)),
        None => Ok(None),
    }
}

/// Generate starter `.iris-agentic-dev.toml` content with inline comments.
/// Used by `iris-dev init` and `check_config`.
pub fn generate_toml_content(container: &str, namespace: &str) -> String {
    format!(
        r#"# iris-agentic-dev workspace configuration
# Commit this file to share connection settings with your team.

# ── Native IRIS (no Docker) — Windows IIS or Linux Apache ──────────────────
# For IRIS installed directly on the host (not in Docker), uncomment and set:
# host = "localhost"
# web_port = 80       # IIS default (IRIS 2024.1+); use 52773 for pre-2024.1 Private Web Server
# web_prefix = ""     # URL path prefix, e.g. "iris" when Atelier is at /iris/api/atelier/
# namespace = "USER"
# scheme = "http"     # Use "https" for TLS-protected IRIS web gateways

# ── Docker container ────────────────────────────────────────────────────────
# For IRIS running in Docker, uncomment and set container name:
# NOTE: iris-agentic-dev requires the IRIS Atelier REST API. Three supported configurations:
#   1. Community images (iris-community, irishealth-community) — include private web server on port 52773
#   2. Enterprise + ISC Web Gateway container (intersystems/webgateway) — iris-agentic-dev auto-detects it
#   3. Enterprise standalone (intersystems/iris) — NOT supported, no Atelier REST available
#
# container = "{container}"

# Default IRIS namespace
namespace = "{namespace}"

# Credentials (optional)
# Use IRIS_USERNAME / IRIS_PASSWORD env vars instead of committing credentials.
# username = "_SYSTEM"
# password = "..."  # not recommended in committed files
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #312: absent / unreadable / unparseable are three different answers ───

    /// Writes a config file into a temp dir and loads it through the real resolver.
    ///
    /// `OBJECTSCRIPT_WORKSPACE` outranks the `workspace_path` argument inside `workspace_root`, so it
    /// is removed for the duration — otherwise a developer with it set in their shell would have
    /// these tests silently read a different directory and pass for the wrong reason.
    fn load_in<T>(contents: Option<&str>, f: impl FnOnce(&str) -> T) -> T {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(c) = contents {
            std::fs::write(dir.path().join(".iris-agentic-dev.toml"), c).expect("write config");
        }
        let prev = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        std::env::remove_var("OBJECTSCRIPT_WORKSPACE");
        let out = f(dir.path().to_str().expect("utf8 tempdir"));
        if let Some(p) = prev {
            std::env::set_var("OBJECTSCRIPT_WORKSPACE", p);
        }
        out
    }

    /// No file at all is the ONE case where falling through to the environment is right.
    #[test]
    fn an_absent_config_is_ok_none_not_an_error() {
        load_in(None, |root| match load_workspace_config(Some(root)) {
            Ok(None) => {}
            other => panic!(
                "no config file must mean 'the defaults apply', not an error or a config; got \
                     {:?}",
                other.map(|o| o.map(|c| c.container))
            ),
        });
    }

    /// THE assertion for #312. Before it, this returned `None` — identical to "no config file" — and
    /// the caller then connected via IRIS_HOST/auto-discovery to a different instance.
    #[test]
    fn a_malformed_config_is_an_error_not_a_silent_fallback() {
        load_in(
            Some("container = \"missing-quote\nnamespace = APP\n"),
            |root| {
                let err = load_workspace_config(Some(root))
                    .expect_err("a config file that is not valid TOML must NOT read as absent");
                assert_eq!(err.problem, ConfigProblem::Unparseable);
                assert!(
                !err.detail.is_empty(),
                "the TOML error must survive — it names the line, and it is the only thing that \
                 says WHICH key is wrong"
            );
                assert!(
                    err.message().contains("refusing to continue"),
                    "the message must say the caller stops, not merely that something was odd: {}",
                    err.message()
                );
            },
        );
    }

    /// Written because a mutation SURVIVED: collapsing the unreadable branch back to `Ok(None)` —
    /// the original defect on that path — broke no test. `a_malformed_config_is_an_error_...` covers
    /// only the PARSE failure, so `ConfigProblem::Unreadable` had no coverage at all and could have
    /// been silently reverted.
    ///
    /// The file is created as a DIRECTORY rather than chmod'd: `.exists()` is true for a directory so
    /// the loader selects it, and `read_to_string` then fails with EISDIR on every platform this runs
    /// on. A permissions-based test would pass vacuously when the suite runs as root, which is
    /// exactly how CI containers run.
    #[test]
    fn an_unreadable_config_is_an_error_not_a_silent_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".iris-agentic-dev.toml"))
            .expect("create the config path as a directory");
        let prev = std::env::var("OBJECTSCRIPT_WORKSPACE").ok();
        std::env::remove_var("OBJECTSCRIPT_WORKSPACE");

        let root = dir.path().to_str().expect("utf8 tempdir");
        let err = load_workspace_config(Some(root))
            .expect_err("a config path that cannot be read must NOT read as absent");
        assert_eq!(
            err.problem,
            ConfigProblem::Unreadable,
            "must be classified as unreadable, not unparseable — the two want different fixes"
        );
        assert!(
            !err.detail.is_empty(),
            "the IO error must survive: it is what tells the user it is a directory, or a \
             permissions problem, or a dangling symlink"
        );

        // The silent sibling must agree (it was the one with no log line at all).
        assert!(
            load_workspace_config_with_path(Some(root)).is_err(),
            "load_workspace_config_with_path must reject an unreadable file too"
        );

        if let Some(p) = prev {
            std::env::set_var("OBJECTSCRIPT_WORKSPACE", p);
        }
    }

    /// A valid file still loads, or the test above could be satisfied by rejecting everything.
    #[test]
    fn a_valid_config_still_loads() {
        load_in(
            Some("container = \"my-iris\"\nnamespace = \"APP\"\n"),
            |root| {
                let cfg = load_workspace_config(Some(root))
                    .expect("valid TOML must not error")
                    .expect("valid TOML must not read as absent");
                assert_eq!(cfg.container.as_deref(), Some("my-iris"));
                assert_eq!(cfg.namespace.as_deref(), Some("APP"));
            },
        );
    }

    /// The two loaders must agree. They had DIFFERENT behaviour before #312 — `load_workspace_config`
    /// logged a warning, `load_workspace_config_with_path` was completely silent — which is the
    /// same-defect-at-two-layers shape where fixing the one you noticed leaves the sibling looking
    /// more trustworthy than it is.
    #[test]
    fn both_loaders_agree_about_a_malformed_config() {
        load_in(Some("this is not toml = = =\n"), |root| {
            let plain = load_workspace_config(Some(root));
            let with_path = load_workspace_config_with_path(Some(root));
            assert!(plain.is_err(), "load_workspace_config must reject it");
            assert!(
                with_path.is_err(),
                "load_workspace_config_with_path must reject it too — it was the silent one"
            );
            assert_eq!(
                plain.unwrap_err().problem,
                with_path.unwrap_err().problem,
                "both loaders must classify the same file the same way"
            );
        });
    }

    /// An explicit connection outranks the file, so a broken file must not block someone who passed
    /// `--host`. This is the documented way out of the abort, and it needs to actually work.
    #[test]
    fn an_explicit_connection_bypasses_a_broken_config() {
        load_in(Some("nonsense = = =\n"), |root| {
            let explicit = IrisConnection::new(
                "http://localhost:52773".to_string(),
                "USER".to_string(),
                "_SYSTEM".to_string(),
                "SYS".to_string(),
                DiscoverySource::ExplicitFlag,
            );
            let got = apply_workspace_config(Some(explicit), Some(root), "USER");
            assert!(
                got.is_ok(),
                "an explicit --host must not be blocked by a file it does not consult"
            );
            assert!(
                got.unwrap().is_some(),
                "the explicit connection must survive"
            );
        });
    }

    // ── issue #21: config path threading + Docker source ─────────────────────
    #[test]
    fn apply_with_path_returns_loaded_path_and_none_when_explicit() {
        use crate::iris::connection::DiscoverySource;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".iris-agentic-dev.toml"),
            "host = \"path-host\"\nweb_port = 52773\n",
        )
        .unwrap();
        let ws = dir.path().to_str().unwrap();

        let (conn, path) = apply_workspace_config_with_path(None, Some(ws), "USER")
            .expect("a valid config must not be an error (#312)");
        let conn = conn.expect("connection from config");
        assert!(conn.base_url.contains("path-host"));
        assert!(
            path.expect("config path")
                .ends_with(".iris-agentic-dev.toml"),
            "returned path must point at the loaded file"
        );

        // Explicit connection wins and returns no path.
        let explicit = IrisConnection::new(
            "http://explicit:52773",
            "USER",
            "_SYSTEM",
            "SYS",
            DiscoverySource::EnvVar,
        );
        let (conn, path) = apply_workspace_config_with_path(Some(explicit), Some(ws), "USER")
            .expect("an explicit connection must not be an error (#312)");
        assert!(conn.unwrap().base_url.contains("explicit"));
        assert!(path.is_none());
    }

    #[test]
    fn host_plus_container_reports_docker_source() {
        use crate::iris::connection::DiscoverySource;
        let cfg = WorkspaceConfig {
            host: Some("localhost".into()),
            container: Some("my-iris".into()),
            ..Default::default()
        };
        let conn = workspace_config_to_connection(&cfg, "USER").expect("connection");
        match conn.source {
            DiscoverySource::Docker { ref container_name } => {
                assert_eq!(container_name, "my-iris")
            }
            ref other => panic!("expected Docker source exposing the container, got {other:?}"),
        }
    }

    #[test]
    fn web_port_accepts_port_alias() {
        let cfg: WorkspaceConfig = toml::from_str("host = \"h\"\nport = 43080\n").unwrap();
        assert_eq!(cfg.web_port, Some(43080));
    }

    #[test]
    fn toml_template_native_section_before_container() {
        let content = generate_toml_content("my-iris", "USER");
        let native_pos = content
            .find("Native IRIS")
            .expect("template must contain 'Native IRIS' section header");
        let container_pos = content
            .find("Docker container")
            .expect("template must contain 'Docker container' section header");
        assert!(
            native_pos < container_pos,
            "Native IRIS section must appear before Docker container section"
        );
    }

    #[test]
    fn toml_template_has_port_80_comment() {
        let content = generate_toml_content("my-iris", "USER");
        assert!(
            content.contains("web_port = 80"),
            "template must document port 80 as IIS default"
        );
    }

    #[test]
    fn toml_template_both_sections_commented_by_default() {
        let content = generate_toml_content("my-iris", "USER");
        // Neither host nor container should be active (uncommented) assignments
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            assert!(
                !trimmed.starts_with("host =") && !trimmed.starts_with("container ="),
                "host and container must be commented out in default template, found: {trimmed}"
            );
        }
    }
}
