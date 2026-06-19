#!/usr/bin/env bash
# Boot a local IRIS Community container for the e2e suite. MUST pass IRIS_PASSWORD at start
# (else the Atelier REST API returns 401), then wait for the API and print the export lines.
# Usage:  source <(./scripts/iris-up.sh | tail -2)
set -euo pipefail
NAME="${IRIS_CONTAINER:-iris-e2e}"
IMAGE="${IRIS_IMAGE:-intersystemsdc/iris-community:2026.1}"
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p 1972:1972 -p 52773:52773 \
  -e IRIS_PASSWORD=SYS -e IRIS_USERNAME=_SYSTEM "$IMAGE" >/dev/null
echo "booting $NAME ($IMAGE) — waiting for the Atelier API ..." >&2
for i in $(seq 1 80); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -u _SYSTEM:SYS \
    "http://localhost:52773/api/atelier/" 2>/dev/null || echo 000)
  [ "$code" = "200" ] && { echo "ready after ~$((i * 3))s" >&2; break; }
  sleep 3
done
# These two lines are meant to be eval'd / sourced by the caller:
echo "# source the next line to configure the e2e env:"
echo "export IRIS_HOST=localhost IRIS_WEB_PORT=52773 IRIS_USERNAME=_SYSTEM IRIS_PASSWORD=SYS IRIS_NAMESPACE=USER IRIS_CONTAINER=$NAME"
