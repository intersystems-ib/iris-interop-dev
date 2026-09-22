"""Resolve and spawn the MCP server binary the benchmark drives.

The fork renamed the binary from `iris-dev` to `iris-interop-dev`. Every spawn site here named the
old one, and that does not fail cleanly, because the failure depends on what is on PATH:

* nothing named `iris-dev` — `FileNotFoundError`, which the callers turned into an empty result;
* a stale pre-rename copy — the exec SUCCEEDS and the child is then killed (on macOS, Gatekeeper
  SIGKILLs an unnotarized binary), so every read returns EOF and the callers read that as
  "the server answered, and the answer is empty".

The second is what this machine has: a root-owned `/usr/local/bin/iris-dev` dated months before the
rename, which returns rc=-9 with an empty stdout. Nothing distinguishes that from a server that
started and registered no tools, which is why the benchmark scored runs instead of failing them.

`IRIS_DEV_BINARY` overrides the name, so a caller can point at a target/debug build without a PATH
install.
"""
import os
import shutil
import subprocess
import tempfile

DEFAULT_BINARY = "iris-interop-dev"

#: Spawned binaries that pre-date the rename. Named so the error can say what it found.
RETIRED_BINARIES = ("iris-dev",)


class BinaryUnavailable(RuntimeError):
    """The server binary could not be run. Carries why — never reported as an empty result."""


def binary_name() -> str:
    """The binary to spawn. `IRIS_DEV_BINARY` wins; otherwise the post-rename name."""
    return os.environ.get("IRIS_DEV_BINARY") or DEFAULT_BINARY


def resolve_binary() -> str:
    """Absolute path to the binary, or raise `BinaryUnavailable` naming what was wrong.

    Resolved to a path rather than left as a bare name so the error can report WHICH file was tried;
    "iris-interop-dev is not on PATH" and "the copy at /usr/local/bin died" need different fixes.
    """
    name = binary_name()
    found = shutil.which(name)
    if found:
        return found
    retired = [(r, shutil.which(r)) for r in RETIRED_BINARIES]
    stale = [f"{r} at {p}" for r, p in retired if p]
    if stale:
        raise BinaryUnavailable(
            f"{name} is not on PATH, but a pre-rename {', '.join(stale)} is. "
            f"That binary is not this server: install {name}, or set IRIS_DEV_BINARY "
            f"to a built copy (target/debug/{DEFAULT_BINARY})."
        )
    raise BinaryUnavailable(
        f"{name} is not on PATH. Build it (cargo build --workspace) and set "
        f"IRIS_DEV_BINARY=target/debug/{DEFAULT_BINARY}, or install it."
    )


def spawn_mcp(env=None, settle: float = 0.3) -> subprocess.Popen:
    """Spawn `<binary> mcp` with stdin/stdout piped, or raise `BinaryUnavailable`.

    Two things the previous spawn sites did not do:

    * stderr is kept, in a temp file rather than a pipe. A pipe nobody drains deadlocks once the
      server writes more than the buffer; a file survives the process and can be quoted in the
      error. Both old sites used DEVNULL, which discarded the only explanation of a failed start.
    * the child is checked for having died immediately. Popen succeeding means the exec succeeded,
      not that the server is running -- the stale-binary case gets that far and is then killed.
    """
    path = resolve_binary()
    log = tempfile.NamedTemporaryFile(
        prefix="mcp-stderr-", suffix=".log", delete=False, mode="w+b"
    )
    proc = subprocess.Popen(
        [path, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=log,
        env=env if env is not None else os.environ.copy(),
    )
    proc._stderr_log = log.name  # type: ignore[attr-defined]
    # Give it a moment to fail, then ask. Without this the first symptom is an empty readline,
    # indistinguishable from a healthy server that has not answered yet.
    try:
        rc = proc.wait(timeout=settle)
    except subprocess.TimeoutExpired:
        return proc  # still alive after `settle` == started
    raise BinaryUnavailable(exit_reason(path, rc, last_line(log.name)))


def exit_reason(path: str, rc: int, stderr_tail: str = "") -> str:
    """Describe a non-zero exit, distinguishing "the OS killed it" from "it refused".

    A negative returncode on POSIX is `-signal`. That distinction is the whole point here: the stale
    pre-rename binary on this machine returns rc=-9, and "killed by signal 9" points at Gatekeeper
    while "exited 9" would point at the server's own argument handling.
    """
    detail = f": {stderr_tail}" if stderr_tail else ""
    if rc < 0:
        return (
            f"{path} was killed by signal {-rc}{detail}. A negative code is the OS killing it, not "
            f"the server refusing: on macOS an unnotarized or stale binary is SIGKILLed (9) before "
            f"it runs."
        )
    return f"{path} exited with code {rc}{detail}"


def last_line(path: str) -> str:
    """Last non-empty line of a captured stderr file, or "" if there is nothing to quote."""
    try:
        with open(path, "rb") as f:
            text = f.read().decode(errors="replace").strip()
    except OSError:
        return ""
    lines = [l for l in text.splitlines() if l.strip()]
    return lines[-1] if lines else ""
