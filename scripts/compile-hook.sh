#!/usr/bin/env bash
# A thin wrapper over `iris-interop-dev hook post-edit` (#529).
#
# This script used to do the work itself, and got two things wrong that composed into silence:
#
#   1. It POSTed `action/compile` with a document NAME and never uploaded the file, so IRIS
#      compiled whatever copy it already held — the previous version, or nothing at all for a new
#      document. The verdict was real, about the wrong source.
#   2. Every path printed to stdout and exited 0. For a PostToolUse hook that reaches the
#      transcript view and NOT the model, so "Compile errors in X" was printed to nobody and the
#      agent carried on believing its edit had compiled.
#
# Both are fixed in the binary, which also derives the document name from the file's own header
# instead of from one parent folder (#530). The logic is not reimplemented here: a second
# implementation is how the two drifted apart in the first place.
#
# plugin.json still points at this path rather than at a bare `iris-interop-dev`, because whether
# the binary is on PATH for a plugin is #517's open question. This wrapper is the one place that
# has to answer it; see the resolution order below.
set -euo pipefail

# Resolution order: an explicit override, then the plugin's own bin, then PATH.
#
# It deliberately does NOT search ../target/. That was the first version, and it is a trap:
# a stale `target/release/iris-interop-dev` built before this subcommand existed shadowed the
# fresh `target/debug` one, so the wrapper found a binary that answered
# "unknown command 'hook'". The hook then looked broken while the code was fine. Development
# builds set IRIS_INTEROP_DEV_BIN, which is also how the tests point this at a stub.
BIN="${IRIS_INTEROP_DEV_BIN:-}"
if [[ -n "$BIN" && ! -x "$BIN" ]]; then
    echo "compile-hook: IRIS_INTEROP_DEV_BIN=$BIN is not executable; the edit was NOT compile-checked." >&2
    exit 0
fi
if [[ -z "$BIN" && -n "${CLAUDE_PLUGIN_ROOT:-}" && -x "${CLAUDE_PLUGIN_ROOT}/bin/iris-interop-dev" ]]; then
    BIN="${CLAUDE_PLUGIN_ROOT}/bin/iris-interop-dev"
fi
if [[ -z "$BIN" ]] && command -v iris-interop-dev >/dev/null 2>&1; then
    BIN="iris-interop-dev"
fi

if [[ -z "$BIN" ]]; then
    # Exit 0, not 2. A missing binary is an installation problem, and blocking every edit over it
    # would be worse than the defect this hook exists to catch. Silence makes no CLAIM about the
    # code — which is the distinction the old script lost when it answered an HTTP 401 with
    # "Compiled OK". The note goes to stderr so it shows in the transcript without reaching the
    # model on every edit.
    echo "compile-hook: iris-interop-dev not found; the edit was NOT compile-checked." >&2
    exit 0
fi

# stdin is the hook payload; the subcommand owns the event, the extension gate and the exit code.
exec "$BIN" hook post-edit
