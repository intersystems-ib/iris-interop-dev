#!/usr/bin/env bash
# Boot the LICENSED IRIS for Health + WebGateway stack (e2e/licensed/) for the interop e2e that
# needs an interop-enabled namespace. Core-based key (no connection cap). Atelier REST is served by
# the gateway on host port 41080 (licensed images have no built-in web server); USER is interop-enabled
# at build time. Needs `docker login containers.intersystems.com` (already configured here) + the key at
# e2e/licensed/iris.key.
# Usage:  ./scripts/iris-up-licensed.sh > /tmp/iris-lic-env && source /tmp/iris-lic-env
#   (see the Usage note in scripts/iris-up.sh for why the `source <(...)` shorthand is a
#   silent no-op under stock macOS /bin/bash 3.2)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/e2e/licensed"
echo "building + booting licensed IRIS-for-Health + WebGateway ..." >&2
docker compose up -d --build >&2
echo "waiting for Atelier via the gateway (http://localhost:41080) ..." >&2
# Same bounds/verdict handling as scripts/iris-up.sh — see the note there. This script had the
# identical defect: the loop exhausting its range exits 0, so a stack that never came up still
# printed a working-looking export line.
ATTEMPTS="${IRIS_UP_WAIT_ATTEMPTS:-90}"
INTERVAL="${IRIS_UP_WAIT_INTERVAL:-3}"
ready=0
code=000
for i in $(seq 1 "$ATTEMPTS"); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -u _SYSTEM:SYS "http://localhost:41080/api/atelier/" 2>/dev/null || echo 000)
  [ "$code" = "200" ] && { echo "ready after ~$((i * INTERVAL))s" >&2; ready=1; break; }
  sleep "$INTERVAL"
done
if [ "$ready" -ne 1 ]; then
  echo "iris-up-licensed.sh FAILED: Atelier never answered 200 via the gateway on" >&2
  echo "  http://localhost:41080 — last HTTP code $code, after $ATTEMPTS attempts at ${INTERVAL}s." >&2
  echo "  A licensed boot has two extra ways to fail silently: a missing/expired e2e/licensed/iris.key," >&2
  echo "  and a docker login to containers.intersystems.com that has lapsed." >&2
  echo "  compose state:" >&2
  docker compose ps 2>&1 | sed 's/^/    /' >&2 || true
  echo "  last log lines:" >&2
  docker compose logs --tail 20 2>&1 | sed 's/^/    /' >&2 || true
  # stdout, and ONE line so `tail -1` keeps it. See the note in scripts/iris-up.sh: the unset is
  # what actually reaches the caller — the exit status does not survive `source <(... | tail -1)`
  # on stock macOS bash 3.2.
  echo 'echo "iris-up-licensed.sh FAILED: the stack never became ready — nothing was exported" >&2; unset IRIS_HOST IRIS_WEB_PORT IRIS_USERNAME IRIS_PASSWORD IRIS_NAMESPACE IRIS_CONTAINER; false'
  exit 1
fi
# Source the next line to point the e2e suite at the licensed interop-enabled USER namespace:
echo "export IRIS_HOST=localhost IRIS_WEB_PORT=41080 IRIS_USERNAME=_SYSTEM IRIS_PASSWORD=SYS IRIS_NAMESPACE=USER IRIS_CONTAINER=iris-lic"
