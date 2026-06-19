#!/usr/bin/env bash
# Boot the LICENSED IRIS for Health + WebGateway stack (e2e/licensed/) for the interop e2e that
# needs an interop-enabled namespace. Core-based key (no connection cap). Atelier REST is served by
# the gateway on host port 41080 (licensed images have no built-in web server); USER is interop-enabled
# at build time. Needs `docker login containers.intersystems.com` (already configured here) + the key at
# e2e/licensed/iris.key. Usage:  source <(./scripts/iris-up-licensed.sh | tail -1)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/e2e/licensed"
echo "building + booting licensed IRIS-for-Health + WebGateway ..." >&2
docker compose up -d --build >&2
echo "waiting for Atelier via the gateway (http://localhost:41080) ..." >&2
for i in $(seq 1 90); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -u _SYSTEM:SYS "http://localhost:41080/api/atelier/" 2>/dev/null || echo 000)
  [ "$code" = "200" ] && { echo "ready after ~$((i * 3))s" >&2; break; }
  sleep 3
done
# Source the next line to point the e2e suite at the licensed interop-enabled USER namespace:
echo "export IRIS_HOST=localhost IRIS_WEB_PORT=41080 IRIS_USERNAME=_SYSTEM IRIS_PASSWORD=SYS IRIS_NAMESPACE=USER IRIS_CONTAINER=iris-lic"
