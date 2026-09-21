#!/usr/bin/env bash
# Drive compile-hook.sh with synthetic IRIS responses, including the cases that MUST NOT read as
# success. Written in the style of test-verify-release-assets.sh (#224), for the same reason: the
# failure only reproduces against a real instance in a particular state, so the verdict logic is
# tested here instead of being assumed correct.
#
# `curl` is stubbed on PATH, so the whole script runs — guards included — with no network.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$ROOT/scripts/compile-hook.sh"
fails=0

run_hook() { # http_code body  -> hook stdout
  local code="$1" body="$2" tmp
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' RETURN
  # The stub prints the body then the status, which is the shape `-w '\n%{http_code}'` produces.
  cat >"$tmp/curl" <<STUB
#!/usr/bin/env bash
printf '%s' '$body'
printf '\n%s' '$code'
exit 0
STUB
  chmod +x "$tmp/curl"
  printf '{"tool_input":{"file_path":"/w/Demo/Thing.cls"}}' \
    | PATH="$tmp:$PATH" IRIS_HOST=h IRIS_WEB_PORT=1 IRIS_NAMESPACE=APP bash "$HOOK" 2>&1
}

check() { # name http_code body must_contain must_NOT_contain
  local name="$1" code="$2" body="$3" want="$4" deny="$5" out
  out=$(run_hook "$code" "$body")
  if [ -n "$want" ] && ! grep -q "$want" <<<"$out"; then
    echo "FAIL [$name]: output lacks '$want'"; sed 's/^/    /' <<<"$out"; fails=1; return
  fi
  if [ -n "$deny" ] && grep -q "$deny" <<<"$out"; then
    echo "FAIL [$name]: output must NOT say '$deny'"; sed 's/^/    /' <<<"$out"; fails=1; return
  fi
  echo "ok   [$name]"
}

# The control first: a genuine success must still be reported, or every negative case below would be
# satisfied by a hook that always refuses.
check "200 clean compile says OK" 200 '{"status":{"errors":[]},"result":{"console":["Compilation finished successfully"]}}' \
      "Compiled Demo.Thing OK" ""

# THE DEFECT: 401 with a zero-byte body — measured against a live instance with a wrong password.
check "401 empty body is NOT success" 401 '' \
      "Compile NOT attempted" "Compiled Demo.Thing OK"
check "401 names the cause"          401 '' \
      "Credentials rejected" ""
check "404 is NOT success"           404 '' \
      "Compile NOT attempted" "Compiled Demo.Thing OK"
check "500 is NOT success"           500 '' \
      "Compile NOT attempted" "Compiled Demo.Thing OK"

# A 2xx whose body is not JSON: unknown, not clean.
check "200 non-JSON body is unknown" 200 '<html>proxy error</html>' \
      "could not be read" "Compiled Demo.Thing OK"

# A 2xx with real compile errors must still report them.
check "200 with errors reports them" 200 '{"status":{"errors":[{"error":"ERROR #1044: bad class"}]},"result":{"console":[]}}' \
      "Compile errors in Demo.Thing" "Compiled Demo.Thing OK"

if [ "$fails" -ne 0 ]; then echo "SOME CHECKS FAILED"; exit 1; fi
echo "ALL CHECKS PASSED"
