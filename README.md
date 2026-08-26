# iris-interop-dev

Connect Claude Code and other AI coding assistants directly to a live InterSystems IRIS
instance. The AI can compile classes, run ObjectScript, execute SQL, run `%UnitTest` tests,
drive Interoperability productions, and inspect class definitions — without leaving the chat.

Works with IRIS installed natively on Windows or Linux, and with Docker. Requires IRIS 2023.1 or later.

> **What this is.** `iris-interop-dev` is the **streamlined, interoperability-focused fork** of the
> community [`intersystems-community/iris-agentic-dev`](https://github.com/intersystems-community/iris-agentic-dev)
> MCP server. It exposes a locked **23-tool interop profile**, ships as a **single binary (no Python)**,
> and uses a **distinct MCP server name (`iris-interop-dev`)** so it can be installed alongside the
> original. **Tool names are identical**, so the [`intersystems-ib/iris-interop-skills`](https://github.com/intersystems-ib/iris-interop-skills)
> plugin works with either server. It is the binary baked into the *"De Prompt a Producción"* workshop VM.

---

## Quick start: Claude Code

**1. Install the binary** from the [latest release](https://github.com/intersystems-ib/iris-interop-dev/releases/latest):

```bash
# macOS (Apple Silicon)
curl -fsSL https://github.com/intersystems-ib/iris-interop-dev/releases/latest/download/iris-interop-dev-macos-arm64 \
  -o /usr/local/bin/iris-interop-dev && chmod +x /usr/local/bin/iris-interop-dev
xattr -d com.apple.quarantine /usr/local/bin/iris-interop-dev 2>/dev/null

# Linux x64
curl -fsSL https://github.com/intersystems-ib/iris-interop-dev/releases/latest/download/iris-interop-dev-linux-x64 \
  -o /usr/local/bin/iris-interop-dev && chmod +x /usr/local/bin/iris-interop-dev
```

**Windows**: download `iris-interop-dev-windows-x64.exe` from the
[releases page](https://github.com/intersystems-ib/iris-interop-dev/releases/latest), rename it to
`iris-interop-dev.exe`, and put it somewhere on disk (e.g. `C:\iris-interop-dev\iris-interop-dev.exe`).

**2. Register it as an MCP server.** The one-line form (works from any directory, user scope):

```bash
claude mcp add --scope user iris-interop-dev \
  --env IRIS_HOST=localhost --env IRIS_WEB_PORT=80 --env IRIS_WEB_PREFIX=irishealth \
  --env IRIS_USERNAME=_SYSTEM --env IRIS_PASSWORD=SYS --env IRIS_NAMESPACE=USER \
  -- /usr/local/bin/iris-interop-dev mcp
```

Or add it by hand to `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "iris-interop-dev": {
      "command": "iris-interop-dev",
      "args": ["mcp"],
      "env": {
        "IRIS_HOST": "localhost",
        "IRIS_WEB_PORT": "52773",
        "IRIS_USERNAME": "_SYSTEM",
        "IRIS_PASSWORD": "SYS",
        "IRIS_NAMESPACE": "USER"
      }
    }
  }
}
```

Restart Claude and **verify with the `check_config` tool** that it connects and the tools appear.

> **VS Code + GitHub Copilot?** The VS Code extension path is provided by the upstream community tool —
> see [`intersystems-community/iris-agentic-dev`](https://github.com/intersystems-community/iris-agentic-dev).
> This fork ships as the `iris-interop-dev` binary; the workshop VM registers it for Claude Code.

---

## Other MCP clients (Cursor, Codex, …)

`iris-interop-dev mcp` is a plain stdio MCP server, so any client that can launch a command and
pass environment variables can drive it. Only the registration file differs — the binary, the
arguments and the environment are the same ones the Claude Code section uses above.

**Cursor** — `~/.cursor/mcp.json` for every project, or `.cursor/mcp.json` for one:

```json
{
  "mcpServers": {
    "iris-interop-dev": {
      "command": "/usr/local/bin/iris-interop-dev",
      "args": ["mcp"],
      "env": {
        "IRIS_HOST": "localhost",
        "IRIS_WEB_PORT": "52773",
        "IRIS_USERNAME": "_SYSTEM",
        "IRIS_PASSWORD": "SYS",
        "IRIS_NAMESPACE": "USER"
      }
    }
  }
}
```

**Codex CLI** — `~/.codex/config.toml`:

```toml
[mcp_servers.iris-interop-dev]
command = "/usr/local/bin/iris-interop-dev"
args = ["mcp"]
default_tools_approval_mode = "approve"

[mcp_servers.iris-interop-dev.env]
IRIS_HOST = "localhost"
IRIS_WEB_PORT = "52773"
IRIS_USERNAME = "_SYSTEM"
IRIS_PASSWORD = "SYS"
IRIS_NAMESPACE = "USER"
```

> Two things that cost real time when we ran the skills eval corpus under Codex:
> - **`default_tools_approval_mode = "approve"` is not optional** for non-interactive runs. Without
>   it, `codex exec` cancels *every* MCP tool call (`user cancelled MCP tool call`) — reproduced 5/5
>   on codex-cli 0.146.1. `auto`, `writes`, project trust settings and feature flags did not help.
> - It must sit **inside `[mcp_servers.<name>]` and before the `.env` subtable header**. TOML puts any
>   bare key written after that header into the env table instead, silently.

Whatever the client:

- **Verify with `check_config` first.** It reports `mcp_version`, `connected`, host/port/namespace and
  the active toolset from a cached snapshot — no IRIS round-trip — so it answers even when IRIS is
  down, which makes it the right first call to tell "MCP not registered" from "IRIS unreachable".
- **`IRIS_TOOLSET` selects the tool surface** (`--toolset` also works). This fork defaults to
  `interop` — the 23 tools below. `baseline` exposes everything the binary carries (54 on 0.8.3).
- **`IRIS_LOG_FILE=<path>` is how a session's traces survive it.** An MCP client keeps the server's
  stderr to itself, so without this there is nothing to read after a failed run.
- **A running client keeps the binary it started with.** After installing a new release, restart the
  client — `check_config`'s `mcp_version` tells you which build is actually answering.

---

## Connecting to IRIS

### Native IRIS on Windows or Linux (no Docker)

Add a `.iris-agentic-dev.toml` file to your project root (the config filename is unchanged from the
upstream codebase):

```toml
host = "localhost"
web_port = 80        # IIS default for IRIS 2024.1+; use 52773 for pre-2024.1
namespace = "USER"
username = "_SYSTEM"
password = "SYS"
```

**Port reference**

| IRIS version | Web server | Default port |
|---|---|---|
| 2024.1+ on Windows | IIS | 80 |
| 2024.1+ on Linux | Apache | 80 |
| Pre-2024.1 (any OS) | Private Web Server (PWS) | 52773 |

#### Windows IIS: `/api` web application required

This is the most common failure on Windows. IIS needs an explicit `/api` web application mapped to the
IRIS Web Gateway module. Without it, `/api/atelier` returns 404 — even when the Management Portal loads.

**To fix:**
1. Open **IIS Manager** → expand your server → **Sites** → **Default Web Site**
2. Right-click → **Add Application**. Alias: `api`, physical path: `C:\InterSystems\IRIS\CSP\bin` (adjust to your install path)
3. Add a wildcard script handler mapping: executable = `CSPms.dll`, no verb restriction
4. Verify `CSP.ini` contains an `[APP_PATH:/api]` section

**`localhost` vs `127.0.0.1`**: on some older Web Gateway builds, `localhost` causes a brief connection
error before each request. If you see delays, set `host = "127.0.0.1"`.

### Docker

Run `iris-interop-dev init` in your project directory — it detects running IRIS containers and writes
`.iris-agentic-dev.toml` automatically:

```bash
iris-interop-dev init
```

Or configure manually:

```toml
container = "myapp-iris"
namespace = "MYAPP"
```

Enterprise IRIS images (`intersystems/iris`, `intersystems/irishealth`) ship without a built-in web
server — run the ISC Web Gateway container alongside IRIS and point `web_port` at it.

### Connection discovery order

`iris-interop-dev` resolves the IRIS connection in this order — first match wins:

1. CLI flags (`--host`, `--web-port`, `--scheme`)
2. `.iris-agentic-dev.toml` in the workspace root
3. Environment variables (`IRIS_HOST`, etc.)
4. Running Docker containers (scored by workspace name similarity)
5. Localhost port scan (52773, 41773, 51773, 8080)

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `IRIS_HOST` | `localhost` | IRIS web gateway hostname |
| `IRIS_WEB_PORT` | `52773` | Web gateway port |
| `IRIS_SCHEME` | `http` | `http` or `https` |
| `IRIS_WEB_PREFIX` | _(empty)_ | URL path prefix for non-root gateway installs (e.g. `irishealth`) |
| `IRIS_USERNAME` | `_SYSTEM` | IRIS username |
| `IRIS_PASSWORD` | `SYS` | IRIS password |
| `IRIS_NAMESPACE` | `USER` | Default namespace |
| `IRIS_CONTAINER` | _(empty)_ | Docker container name — required for Docker-dependent tools |
| `IRIS_TOOLSET` | `interop` | Tool surface: `interop` (23 tools) or `baseline` (full upstream surface). Same as `--toolset` |
| `IRIS_LOG_FILE` | _(empty)_ | Mirror server traces to this file — the only trace that outlives an MCP session |
| `IRIS_DISCOVERY_TIMEOUT_MS` | `2000` | How long startup waits for IRIS discovery before serving. Missing this window is not fatal — the connection is adopted whenever the probe finishes — so raise it only if you would rather `initialize` block than serve unconnected |
| `IRIS_DISCOVERY_RETRY_SECS` | `15` | Minimum gap between lazy re-probes when a session has no connection. A session started before IRIS was ready heals on a later tool call instead of answering `IRIS_UNREACHABLE` for its lifetime |
| `OBJECTSCRIPT_WORKSPACE` | `$PWD` | Workspace root for `.iris-agentic-dev.toml` lookup |
| `OBJECTSCRIPT_SKILLMCP_NAMESPACE` | _(connection namespace)_ | Namespace holding the `^SKILLS` / `^KBCHUNKS` registry (baseline toolset). Defaults to the connection namespace (`IRIS_NAMESPACE` / `--namespace`); set it only to centralise the registry in one namespace |

---

## The interop skills

This server is the runtime for the **[`intersystems-ib/iris-interop-skills`](https://github.com/intersystems-ib/iris-interop-skills)**
Claude Code plugin — 20 skills that steer Claude when building IRIS For Health Interoperability
productions (messages, BS/BP/BO, BPL, DTL, HL7 schemas, SOAP/REST/FHIR/DICOM, alerting, security,
lifecycle), plus governance hooks, subagents and a post-build conformance review. Install it with:

```text
/plugin marketplace add intersystems-ib/iris-interop-skills
/plugin install iris-interop-skills@iris-interop-skills
```

---

## Tools (interop profile)

Most tools work over the Atelier REST API against any IRIS instance; Docker-only tools accept an
`IRIS_CONTAINER` but also run over HTTP/Atelier against native/remote IRIS.

**Code & data** — `iris_doc` (read/write/delete documents), `iris_compile` (compile, errors with line
numbers), `iris_execute` (run ObjectScript), `iris_query` (SQL → JSON rows), `iris_test` (run
`%UnitTest`, structured pass/fail), `iris_get_log` (fetch a truncated result by `log_id`).

**Introspection** — `docs_introspect` (methods/properties/XData/superclasses), `iris_symbols` (search
classes/methods), `iris_table_info` (real projected table + columns), `check_config` (active connection
state), `find_subclass_implementations` (who overrides a method), `iris_debug` (map a .INT offset back
to source, error logs).

**Interoperability** — `iris_production` (start/stop/update/status/recover/autostart),
`iris_production_item` (item get/set settings), `iris_interop_query` (logs, queues, message
archive/trace), `iris_message_body` (read a message body — string/stream containers and any
`EnsLib.EDI.Document`: HL7 v2, X12, ASTM, EDIFACT, EDI XML — with a PHI gate),
`iris_business_rule_info` (list/describe routing rules), `iris_production_diff` (running config vs
committed source), `extract_message_map_routing` (message-map targets of a business process),
`iris_lookup_manage` / `iris_lookup_transfer` (lookup tables), `iris_credential_list` /
`iris_credential_manage` (SSL/credentials).

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| 404 on `/api/atelier` (Windows) | IIS missing `/api` web application | See Windows IIS setup above |
| `check_config` works but compile/search fail | Atelier web app `Recurse=0` | Management Portal → Security → Web Apps → `/api/atelier` → enable **Recurse** |
| `iris_execute` returns empty output | HTTP CodeMode only returns what your code `write`s | Use `write <expr>,!`, or wrap side effects as a `[SqlProc]` and read via `iris_query` |
| `DOCKER_REQUIRED` on native IRIS | `IRIS_CONTAINER` set unnecessarily | Retry without `IRIS_CONTAINER` — the interop tools run over HTTP |
| 403 on write operations | Insufficient permissions | Use a user with `%DB_USER` or `%All` role |
| Connection delays on Windows | `localhost` DNS issue | Use `host = "127.0.0.1"` in `.iris-agentic-dev.toml` |

Verbose HTTP logging: `iris-interop-dev mcp --verbose 2>debug.log`.

---

## Commands

```bash
iris-interop-dev mcp                     # Start the MCP server
iris-interop-dev compile MyApp.Foo.cls   # Compile from the terminal
iris-interop-dev init                    # Generate .iris-agentic-dev.toml from running containers
iris-interop-dev --version               # Print version
```

---

## Contributing

Issues and pull requests welcome — file bugs at the
[Issues tab](https://github.com/intersystems-ib/iris-interop-dev/issues).

This is an interop-focused fork of the community
[`intersystems-community/iris-agentic-dev`](https://github.com/intersystems-community/iris-agentic-dev);
upstream fixes to the shared codebase flow from there. The repositories in `intersystems-ib` are
community utilities and examples — **not** covered by official InterSystems support.
