# iris-interop-dev

Connect Claude Code and other AI coding assistants directly to a live InterSystems IRIS
instance. The AI can compile classes, run ObjectScript, execute SQL, run `%UnitTest` tests,
drive Interoperability productions, and inspect class definitions — without leaving the chat.

Works with IRIS installed natively on Windows or Linux, and with Docker. Requires IRIS 2023.1 or later.

> **What this is.** `iris-interop-dev` is the **streamlined, interoperability-focused fork** of the
> community [`intersystems-community/iris-agentic-dev`](https://github.com/intersystems-community/iris-agentic-dev)
> MCP server. It exposes a locked **20-tool interop profile**, ships as a **single binary (no Python)**,
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
> This fork is packaged for Claude Code as the `iris-interop-dev` binary.

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
| `OBJECTSCRIPT_WORKSPACE` | `$PWD` | Workspace root for `.iris-agentic-dev.toml` lookup |

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
classes/methods), `iris_table_info` (real projected table + columns), `check_config` (active connection state).

**Interoperability** — `iris_production` (start/stop/update/status/recover), `iris_production_item`
(item get/set settings), `iris_interop_query` (logs, queues, message archive/trace),
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
