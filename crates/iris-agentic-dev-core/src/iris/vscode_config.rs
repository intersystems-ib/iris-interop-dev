//! Parse VS Code settings.json for IRIS connection configuration.
//! Supports both direct host/port connections and named server references.

use crate::iris::connection::{DiscoverySource, IrisConnection};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
pub struct VsCodeSettings {
    #[serde(rename = "objectscript.conn")]
    pub objectscript_conn: Option<ObjectScriptConn>,
    #[serde(rename = "intersystems.servers")]
    pub intersystems_servers: Option<HashMap<String, IntersystemsServer>>,
}

#[derive(Debug, Deserialize)]
pub struct ObjectScriptConn {
    pub active: Option<bool>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ns: Option<String>,
    /// Named server reference (key into intersystems.servers)
    pub server: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IntersystemsServer {
    #[serde(rename = "webServer")]
    pub web_server: WebServerSpec,
    #[serde(rename = "superServer")]
    pub super_server: Option<SuperServerSpec>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebServerSpec {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub scheme: Option<String>,
    #[serde(rename = "pathPrefix")]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SuperServerSpec {
    pub host: Option<String>,
    pub port: Option<u16>,
}

impl IntersystemsServer {
    /// Returns the native SuperServer port if configured.
    pub fn super_server_port(&self) -> Option<u16> {
        self.super_server.as_ref().and_then(|ss| ss.port)
    }
}

/// Strip JSONC (JSON with Comments) syntax so serde_json can parse VS Code settings.json.
///
/// Handles: full-line // comments, inline // comments, /* block comments */,
/// and trailing commas before `}` or `]`.
fn strip_jsonc(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_string = false;

    while i < chars.len() {
        let c = chars[i];

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                // escaped character — emit both and skip next
                i += 1;
                out.push(chars[i]);
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Outside a string
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }

        // Line comment //
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Block comment /* ... */
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2; // skip */
            continue;
        }

        // Trailing comma: ,  followed (possibly with whitespace) by } or ]
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\n' || chars[j] == '\r')
            {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                // skip the comma
                i += 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Parse a VS Code settings.json file.
/// Bug 10: the old parser only stripped full-line // comments.
/// This version handles inline //, /* */ block comments, and trailing commas.
pub fn parse_vscode_settings(path: impl AsRef<Path>) -> anyhow::Result<VsCodeSettings> {
    let content = std::fs::read_to_string(path.as_ref())?;
    // First try raw JSON (fast path for files without JSONC syntax).
    if let Ok(settings) = serde_json::from_str::<VsCodeSettings>(&content) {
        return Ok(settings);
    }
    // Strip JSONC syntax and retry.
    let cleaned = strip_jsonc(&content);
    let settings: VsCodeSettings = serde_json::from_str(&cleaned).unwrap_or_default();
    Ok(settings)
}

/// Outcome of resolving parsed settings into a connection.
///
/// #187: this used to be a bare `Option<IrisConnection>`, which collapsed "nothing is
/// configured here" and "a connection IS configured but its password is missing" into
/// the same `None` — and then avoided the second case entirely by defaulting the
/// password to `SYS`. Those are different answers and the caller needs them apart.
#[derive(Debug)]
pub enum VsCodeResolution {
    /// A complete connection was resolved.
    Resolved(IrisConnection),
    /// Nothing is configured here: no `objectscript.conn`, `active: false`, or a
    /// `server:` name with no matching `intersystems.servers` entry.
    NotConfigured,
    /// A connection is configured but no password is available for it.
    ///
    /// This is the Server Manager case. The VS Code extension keeps the password in
    /// the OS keychain, so it is absent from settings.json, and this binary has no
    /// keychain reader. Fabricating `SYS` here produced a connection that looked
    /// configured and was wrong — a 401 naming no cause.
    MissingPassword { server: Option<String> },
}

impl VsCodeSettings {
    /// Resolve to a connection, taking the fallback password from the caller.
    ///
    /// Pure — `resolve()` supplies `$IRIS_PASSWORD`. Split so tests can cover the
    /// credential rules without mutating process environment.
    pub fn resolve_with(&self, env_password: Option<&str>) -> VsCodeResolution {
        // An empty string is an absent password, not a password of length zero.
        let env_password = env_password.filter(|p| !p.is_empty());
        let present = |v: Option<&str>| v.filter(|p| !p.is_empty()).map(str::to_owned);

        let conn = match self.objectscript_conn.as_ref() {
            Some(c) => c,
            None => return VsCodeResolution::NotConfigured,
        };
        if conn.active == Some(false) {
            return VsCodeResolution::NotConfigured;
        }
        let ns = conn.ns.as_deref().unwrap_or("USER");

        // Named server path — resolve through `intersystems.servers`.
        if let Some(server_name) = &conn.server {
            let server = match self
                .intersystems_servers
                .as_ref()
                .and_then(|servers| servers.get(server_name))
            {
                Some(s) => s,
                // A name with no entry is a typo, not a licence to guess localhost.
                None => return VsCodeResolution::NotConfigured,
            };
            let host = server.web_server.host.as_deref().unwrap_or("localhost");
            let web_port = server.web_server.port.unwrap_or(52773);
            let scheme = server.web_server.scheme.as_deref().unwrap_or("http");
            let path_prefix = server
                .web_server
                .path_prefix
                .as_deref()
                .unwrap_or("")
                .trim_matches('/');
            let base_url = if path_prefix.is_empty() {
                format!("{}://{}:{}", scheme, host, web_port)
            } else {
                format!("{}://{}:{}/{}", scheme, host, web_port, path_prefix)
            };
            // Username is not a secret and `_SYSTEM` is this codebase's documented
            // default everywhere else; the password is the field Server Manager
            // deliberately does not write here, so it is the one we refuse to invent.
            let username = server.username.as_deref().unwrap_or("_SYSTEM");
            let password = match present(server.password.as_deref()).or_else(|| present(env_password))
            {
                Some(p) => p,
                None => {
                    return VsCodeResolution::MissingPassword {
                        server: Some(server_name.clone()),
                    }
                }
            };
            return VsCodeResolution::Resolved(IrisConnection::new(
                base_url,
                ns,
                username,
                password,
                DiscoverySource::VsCodeSettings,
            ));
        }

        // Direct host/port path.
        let host = conn.host.as_deref().unwrap_or("localhost");
        let port = conn.port.unwrap_or(52773);
        let username = conn.username.as_deref().unwrap_or("_SYSTEM");
        let password = match present(conn.password.as_deref()).or_else(|| present(env_password)) {
            Some(p) => p,
            None => return VsCodeResolution::MissingPassword { server: None },
        };
        VsCodeResolution::Resolved(IrisConnection::new(
            format!("http://{}:{}", host, port),
            ns,
            username,
            password,
            DiscoverySource::VsCodeSettings,
        ))
    }

    /// Resolve to a connection, falling back to `$IRIS_PASSWORD` for the secret that
    /// Server Manager keeps in the OS keychain.
    pub fn resolve(&self) -> VsCodeResolution {
        self.resolve_with(std::env::var("IRIS_PASSWORD").ok().as_deref())
    }
}
