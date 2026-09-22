"""BENCHMARK namespace setup and teardown via iris_execute."""
import os
import subprocess
import json
import time


def _mcp_call(tool: str, args: dict) -> dict:
    iris_host = os.environ.get("IRIS_HOST", "localhost")
    iris_port = os.environ.get("IRIS_WEB_PORT", "52780")
    iris_user = os.environ.get("IRIS_USERNAME", "_SYSTEM")
    iris_pass = os.environ.get("IRIS_PASSWORD", "SYS")

    msgs = [
        '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"benchmark","version":"1"}}}',
        '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}',
        json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": tool, "arguments": args}}),
    ]

    env = os.environ.copy()
    env.update({
        "IRIS_HOST": iris_host,
        "IRIS_WEB_PORT": iris_port,
        "IRIS_USERNAME": iris_user,
        "IRIS_PASSWORD": iris_pass,
    })

    from .binary import spawn_mcp

    proc = spawn_mcp(env=env)

    # send with small delays so server processes each message
    for i, msg in enumerate(msgs):
        proc.stdin.write((msg + "\n").encode())
        proc.stdin.flush()
        time.sleep(0.2)

    time.sleep(2)
    proc.stdin.close()
    out = proc.stdout.read().decode(errors="replace")
    proc.wait()

    for line in out.splitlines():
        try:
            obj = json.loads(line)
            if obj.get("id") == 2:
                return obj
        except json.JSONDecodeError:
            pass
    # No response carrying our id. This MUST raise rather than return {}: the callers read
    # resp["result"]["content"][0]["text"], get "" from an empty dict, and then `"ERROR" in ""` is
    # False -- so reset_benchmark_namespace() reported SUCCESS for a server that never answered, and
    # a 15-task condition ran against a namespace that was never dropped. That is precisely the
    # carry-over FR-001b exists to prevent, arriving as clean-looking data.
    rc = proc.poll()
    died = f" (server exited with code {rc})" if rc not in (0, None) else ""
    raise RuntimeError(
        f"no MCP response for {tool}{died}. stderr: "
        f"{_stderr_tail(proc)!r}; stdout was {out[:400]!r}"
    )


def _stderr_tail(proc) -> str:
    """Last line of the spawned server's stderr, if it was captured to a file."""
    path = getattr(proc, "_stderr_log", None)
    if not path:
        return ""
    from .binary import last_line

    return last_line(path)


def reset_benchmark_namespace():
    """Drop and recreate the BENCHMARK namespace to eliminate carry-over between conditions.

    Called before each condition's 15-task run (FR-001b).
    """
    # Drop: kill all globals and delete the namespace
    drop_code = (
        'set sc=##class(%SYS.Namespace).Delete("BENCHMARK")'
        ' if $system.Status.IsError(sc) && $system.Status.GetErrorText(sc) \'[ "not exist" {'
        '  write "ERROR:"_$system.Status.GetErrorText(sc)'
        ' } else { write "DROPPED" }'
    )
    resp = _mcp_call("iris_execute", {"code": drop_code, "namespace": "%SYS", "confirmed": True})
    content = resp.get("result", {}).get("content", [{}])[0].get("text", "")
    if "ERROR" in content:
        raise RuntimeError(f"Failed to drop BENCHMARK namespace: {content}")

    # Recreate
    create_code = (
        'set sc=##class(%SYS.Namespace).Create("BENCHMARK")'
        ' if $system.Status.IsError(sc) { write "ERROR:"_$system.Status.GetErrorText(sc) }'
        ' else { write "CREATED" }'
    )
    resp = _mcp_call("iris_execute", {"code": create_code, "namespace": "%SYS", "confirmed": True})
    content = resp.get("result", {}).get("content", [{}])[0].get("text", "")
    if "ERROR" in content:
        raise RuntimeError(f"Failed to create BENCHMARK namespace: {content}")


def ensure_benchmark_namespace():
    """Create BENCHMARK namespace if it does not exist."""
    code = (
        'if \'##class(%SYS.Namespace).Exists("BENCHMARK") {'
        ' set sc=##class(%SYS.Namespace).Create("BENCHMARK")'
        ' if $system.Status.IsError(sc) { write "ERROR:"_$system.Status.GetErrorText(sc) }'
        ' else { write "CREATED" }'
        '} else { write "EXISTS" }'
    )
    resp = _mcp_call("iris_execute", {"code": code, "namespace": "%SYS", "confirmed": True})
    content = resp.get("result", {}).get("content", [{}])[0].get("text", "")
    if "ERROR" in content:
        raise RuntimeError(f"Failed to create BENCHMARK namespace: {content}")


def wipe_benchmark_namespace():
    """Kill all globals in BENCHMARK namespace between tasks."""
    code = "kill @(\"^\")"
    _mcp_call("iris_execute", {"code": code, "namespace": "BENCHMARK", "confirmed": True})
