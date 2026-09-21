#!/usr/bin/env bash
# Boot a local IRIS Community container for the e2e suite. MUST pass IRIS_PASSWORD at start
# (else the Atelier REST API returns 401), then wait for the API and print the export lines.
# Usage (portable, and the failure stops the chain):
#     ./scripts/iris-up.sh > /tmp/iris-env && source /tmp/iris-env
#   The shorthand `source <(./scripts/iris-up.sh | tail -2)` works in zsh and bash >= 4 but is a
#   silent no-op under stock macOS /bin/bash (3.2): measured, `source <(...)` there does not apply
#   even an `export` to the caller, so the suite runs with no IRIS_HOST and the reason is invisible.
#   The two-step form above propagates the env AND, via `&&`, a failed boot.
set -euo pipefail
NAME="${IRIS_CONTAINER:-iris-e2e}"
IMAGE="${IRIS_IMAGE:-intersystemsdc/iris-community:2026.1}"
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p 1972:1972 -p 52773:52773 \
  -e IRIS_PASSWORD=SYS -e IRIS_USERNAME=_SYSTEM "$IMAGE" >/dev/null
echo "booting $NAME ($IMAGE) — waiting for the Atelier API ..." >&2
# Bounds are overridable so the FAILURE path is testable in milliseconds — see
# scripts/test-iris-up.sh. With the 80x3s default it took four minutes to observe,
# which is why nothing ever checked it.
ATTEMPTS="${IRIS_UP_WAIT_ATTEMPTS:-80}"
INTERVAL="${IRIS_UP_WAIT_INTERVAL:-3}"
ready=0
code=000   # `set -u`: the diagnostic below reads $code even if the loop never ran
for i in $(seq 1 "$ATTEMPTS"); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -u _SYSTEM:SYS \
    "http://localhost:52773/api/atelier/" 2>/dev/null || echo 000)
  [ "$code" = "200" ] && { echo "ready after ~$((i * INTERVAL))s" >&2; ready=1; break; }
  sleep "$INTERVAL"
done
# A for-loop that exhausts its range exits 0, so `set -e` never saw this. The script then
# printed the export lines for an instance that had not booted, and the documented usage
# — `source <(./scripts/iris-up.sh | tail -2)` — configured the caller for it. The e2e
# suite then failed with connection errors that named the symptom, never the cause.
if [ "$ready" -ne 1 ]; then
  echo "iris-up.sh FAILED: the Atelier API never answered 200 on http://localhost:52773 —" >&2
  echo "  last HTTP code $code, after $ATTEMPTS attempts at ${INTERVAL}s (~$((ATTEMPTS * INTERVAL))s)." >&2
  echo "  container: $(docker inspect -f '{{.State.Status}}, exit {{.State.ExitCode}}' "$NAME" 2>/dev/null || echo 'not found')" >&2
  echo "  last log lines:" >&2
  docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/    /' >&2 || true
  # STDOUT, deliberately, and ONE line so a `tail -1`/`tail -2` caller keeps it. The export lines
  # below are NOT reached, so nothing new is set; this line additionally CLEARS a stale env from an
  # earlier good boot, which would otherwise leave the suite talking to a container that is gone.
  # Measured, per shell, for the documented usages:
  #   * zsh 5.9 + `source <(... | tail -2)`: the unset applies and the status is 1.
  #   * `... > file && source file` (any shell): the `&&` short-circuits on this script's exit 1,
  #     so the caller never sources anything.
  #   * stock macOS /bin/bash 3.2 + `source <(...)`: applies NOTHING to the caller, not even an
  #     export — so that form was already a no-op for the success path, before this guard existed.
  echo 'echo "iris-up.sh FAILED: IRIS never became ready — nothing was exported" >&2; unset IRIS_HOST IRIS_WEB_PORT IRIS_USERNAME IRIS_PASSWORD IRIS_NAMESPACE IRIS_CONTAINER; false'
  exit 1
fi
# These two lines are meant to be eval'd / sourced by the caller:
echo "# source the next line to configure the e2e env:"
echo "export IRIS_HOST=localhost IRIS_WEB_PORT=52773 IRIS_USERNAME=_SYSTEM IRIS_PASSWORD=SYS IRIS_NAMESPACE=USER IRIS_CONTAINER=$NAME"
