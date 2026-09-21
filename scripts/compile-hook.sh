#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)

EVENT=$(printf '%s' "$INPUT" | jq -r '.hook_event_name // "PostToolUse"')

if [[ "$EVENT" == "FileChanged" ]]; then
    [[ "${IRIS_COMPILE_ON_SAVE:-}" != "true" ]] && exit 0
    FILE_PATH=$(printf '%s' "$INPUT" | jq -r '.file_path // empty')
else
    FILE_PATH=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty')
fi

[[ -z "$FILE_PATH" ]] && exit 0

EXT="${FILE_PATH##*.}"
case "$EXT" in
    cls|mac|inc) ;;
    *) exit 0 ;;
esac

[[ "${IRIS_AUTO_COMPILE:-}" == "false" ]] && exit 0

if [[ -z "${IRIS_HOST:-}" || -z "${IRIS_WEB_PORT:-}" ]]; then
    echo "IRIS not connected — set IRIS_HOST, IRIS_WEB_PORT, IRIS_USERNAME, IRIS_PASSWORD"
    exit 0
fi

BASENAME=$(basename "$FILE_PATH")
DOC_NAME="$BASENAME"

CLASS_NAME="${BASENAME%.*}"

if [[ "$EXT" == "cls" ]]; then
    PARENT=$(basename "$(dirname "$FILE_PATH")")
    if [[ "$PARENT" != "." && "$PARENT" != "" && "$PARENT" != "workspace" && "$PARENT" != "/" ]]; then
        DOC_NAME="${PARENT}.${BASENAME}"
        CLASS_NAME="${PARENT}.${CLASS_NAME}"
    fi
fi

BASE_URL="http://${IRIS_HOST}:${IRIS_WEB_PORT}/api/atelier/v1"
NS="${IRIS_NAMESPACE:-USER}"
USER="${IRIS_USERNAME:-_SYSTEM}"
PASS="${IRIS_PASSWORD:-SYS}"

START=$(date +%s%N 2>/dev/null || echo 0)

# ONE request, capturing body and status together. This used to make two identical POSTs — one for
# the body, one for the status code — so a working connection compiled the document twice and the
# reported time covered only the first.
HTTP_BODY_AND_CODE=$(curl --max-time 3 -s -w $'\n%{http_code}' \
    -X POST \
    -u "${USER}:${PASS}" \
    -H "Content-Type: application/json" \
    "${BASE_URL}/${NS}/action/compile" \
    -d "[\"${DOC_NAME}\"]" 2>/dev/null) || {
    echo "IRIS not connected — set IRIS_HOST, IRIS_WEB_PORT, IRIS_USERNAME, IRIS_PASSWORD"
    exit 0
}
HTTP_CODE="${HTTP_BODY_AND_CODE##*$'\n'}"
BODY="${HTTP_BODY_AND_CODE%$'\n'*}"

if [[ "$HTTP_CODE" == "000" || -z "$HTTP_CODE" ]]; then
    echo "IRIS not connected — set IRIS_HOST, IRIS_WEB_PORT, IRIS_USERNAME, IRIS_PASSWORD"
    exit 0
fi

# The status must be 2xx before the body is read as a compile result. This check used to reject only
# "000", so a 401 or 404 fell through to the parse below — and since Atelier answers those with a
# ZERO-BYTE body, the error list came back empty and the hook reported "Compiled OK" for a request
# IRIS had refused at the door. Measured against a live instance with a wrong password: HTTP 401,
# 0-byte body, verdict "Compiled ... OK".
if [[ ! "$HTTP_CODE" =~ ^2[0-9][0-9]$ ]]; then
    echo "Compile NOT attempted for ${CLASS_NAME}: IRIS answered HTTP ${HTTP_CODE}."
    case "$HTTP_CODE" in
        401|403) echo "  Credentials rejected — check IRIS_USERNAME / IRIS_PASSWORD." ;;
        404) echo "  Not found — check IRIS_NAMESPACE (${NS}) and IRIS_WEB_PREFIX." ;;
    esac
    exit 0
fi

END=$(date +%s%N 2>/dev/null || echo 0)
if [[ "$START" != "0" && "$END" != "0" ]]; then
    ELAPSED_MS=$(( (END - START) / 1000000 ))
    ELAPSED_S=$(awk "BEGIN {printf \"%.1f\", $ELAPSED_MS / 1000}")
else
    ELAPSED_S="?"
fi

# A body that is not JSON means the outcome is UNKNOWN, not clean. `jq` failing here used to be
# hidden by `2>/dev/null` and `|| true`, leaving ERRORS empty — indistinguishable from a successful
# compile.
if ! printf '%s' "$BODY" | jq -e . >/dev/null 2>&1; then
    echo "Compile result for ${CLASS_NAME} could not be read: IRIS returned HTTP ${HTTP_CODE} with a"
    echo "  body that is not JSON, so whether it compiled is unknown. First 200 bytes:"
    printf '%s' "$BODY" | head -c 200 | sed 's/^/    /'
    exit 0
fi

ERRORS=$(printf '%s' "$BODY" | jq -r '
    (.status.errors[]?.error // empty),
    (.result.console[]? | select(startswith("ERROR") or startswith(" ERROR")))
' 2>/dev/null | grep -v "^$" | head -10 || true)

if [[ -z "$ERRORS" ]]; then
    echo "Compiled ${CLASS_NAME} OK (${ELAPSED_S}s)"
else
    echo "Compile errors in ${CLASS_NAME}:"
    printf '%s\n' "$ERRORS" | while IFS= read -r line; do
        echo "  $line"
    done
fi
