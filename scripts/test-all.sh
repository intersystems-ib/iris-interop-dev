#!/usr/bin/env bash
# Run the interop-fork test suite, post-build.
#   ./scripts/test-all.sh unit   # no IRIS needed
#   ./scripts/test-all.sh e2e    # needs a live IRIS (IRIS_* env, or scripts/iris-up.sh).
#                                #   exercises BOTH transports: HTTP/Atelier REST and Docker-exec.
#   ./scripts/test-all.sh        # unit, then e2e if IRIS_HOST is set
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
mode="${1:-auto}"; rc=0

run_unit() {
  echo "== unit (no IRIS) =="
  cargo test -p iris-agentic-dev-core --lib || rc=1
  cargo test -p iris-agentic-dev-core \
    --test test_toolset --test mcp_handshake --test test_doc_params \
    --test test_compile_params --test interop_unit_tests || rc=1
  echo "== per-tool gate =="
  ./scripts/validate-tools.sh || rc=1
}

run_e2e() {
  if [ -z "${IRIS_HOST:-}" ]; then
    echo "IRIS_HOST not set. Boot IRIS first:  source <(./scripts/iris-up.sh | tail -1)"; return 1
  fi
  # The handshake + iris_test e2e spawn the built binary — ensure it is fresh.
  cargo build -p iris-agentic-dev || rc=1
  echo "== e2e: transport matrix (HTTP/Atelier REST + Docker-exec) =="
  cargo test -p iris-agentic-dev-core --test transport_e2e -- --test-threads=1 || rc=1
  echo "== e2e: tools (live IRIS) =="
  cargo test -p iris-agentic-dev-core \
    --test test_e2e --test interop_e2e_tests -- --test-threads=1 || rc=1
  echo "== e2e: iris_test (both transports, ignored gate) =="
  cargo test -p iris-agentic-dev-core --test test_iris_test_e2e -- --ignored --test-threads=1 || rc=1
}

case "$mode" in
  unit) run_unit ;;
  e2e)  run_e2e ;;
  auto) run_unit; if [ -n "${IRIS_HOST:-}" ]; then run_e2e; else echo "(skipping e2e — IRIS_HOST unset)"; fi ;;
  *) echo "usage: $0 [unit|e2e]"; exit 2 ;;
esac
[ "$rc" = 0 ] && echo "ALL GREEN" || echo "FAILURES (rc=$rc)"
exit "$rc"
