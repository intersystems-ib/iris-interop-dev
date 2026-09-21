#!/usr/bin/env bash
# Drive scripts/iris-up.sh through the readiness verdict, including the case that MUST NOT read as
# success. Written in the style of test-compile-hook.sh (#316) and test-verify-release-assets.sh
# (#224), for the same reason: with the 80x3s default the failure took four minutes to observe, so
# nobody ever observed it, and the script exited 0 after a boot that never happened.
#
# `docker` and `curl` are both stubbed on PATH, so the whole script runs — guards included — with no
# container and no network. IRIS_UP_WAIT_ATTEMPTS/INTERVAL shrink the wait to nothing.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fails=0

run_up() { # codes...  -> "exit=<n>" then stdout, with stderr merged after a marker
  local tmp; tmp=$(mktemp -d)
  # curl stub: prints one code per invocation, walking the list, repeating the last forever.
  printf '%s\n' "$@" >"$tmp/codes"
  cat >"$tmp/curl" <<'STUB'
#!/usr/bin/env bash
d="$(dirname "$0")"
n=$(cat "$d/n" 2>/dev/null || echo 1)
code=$(sed -n "${n}p" "$d/codes")
[ -n "$code" ] || code=$(tail -1 "$d/codes")
echo $((n + 1)) >"$d/n"
printf '%s' "$code"
STUB
  # docker stub: rm/run/inspect/logs all succeed quietly. A real docker must never be reached.
  cat >"$tmp/docker" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  inspect) echo "exited, exit 1" ;;
  logs)    echo "[stub] no container logs" ;;
esac
exit 0
STUB
  chmod +x "$tmp/curl" "$tmp/docker"
  local out rc
  out=$(PATH="$tmp:$PATH" IRIS_CONTAINER=iris-up-selftest IRIS_UP_WAIT_ATTEMPTS=3 \
        IRIS_UP_WAIT_INTERVAL=0 bash "$ROOT/scripts/iris-up.sh" 2>"$tmp/err")
  rc=$?
  printf 'exit=%s\n%s\n--stderr--\n%s\n' "$rc" "$out" "$(cat "$tmp/err")"
  rm -rf "$tmp"
}

check() { # name want_exit must_contain must_NOT_contain codes...
  local name="$1" want_exit="$2" want="$3" deny="$4"; shift 4
  local out; out=$(run_up "$@")
  local got; got=$(sed -n '1s/exit=//p' <<<"$out")
  if [ "$got" != "$want_exit" ]; then
    echo "FAIL [$name]: exit $got, wanted $want_exit"; sed 's/^/    /' <<<"$out"; fails=1; return
  fi
  if [ -n "$want" ] && ! grep -q -- "$want" <<<"$out"; then
    echo "FAIL [$name]: output lacks '$want'"; sed 's/^/    /' <<<"$out"; fails=1; return
  fi
  if [ -n "$deny" ] && grep -q -- "$deny" <<<"$out"; then
    echo "FAIL [$name]: output must NOT contain '$deny'"; sed 's/^/    /' <<<"$out"; fails=1; return
  fi
  echo "ok   [$name]"
}

# THE CONTROL FIRST. Without it every negative case below is satisfied by a script that always
# fails, and this file would pass while asserting nothing about a real boot.
check "200 on the first probe exports the env" 0 "export IRIS_HOST=localhost" "" 200

# Ready on the third of three attempts: the success path must survive earlier failures, or the
# guard below would be a fresh way to break a working boot.
check "200 on the last attempt still exports" 0 "export IRIS_HOST=localhost" "" 000 000 200

# THE DEFECT: the loop exhausts, the script exits 0, and the export lines go out anyway.
check "never ready does not exit 0"        1 ""                    "export IRIS_HOST=localhost" 000
check "never ready names the last code"    1 "last HTTP code 000"   "" 000
check "never ready says what failed"       1 "never answered 200"   "" 000
# stdout must carry a snippet that CLEARS the env, because the documented usage
# `source <(./scripts/iris-up.sh | tail -2)` cannot see this script's exit status: with a pipeline
# inside the process substitution, stock macOS bash 3.2 returns 0 for the source, measured 5/5.
check "stdout unsets rather than exports"  1 "unset IRIS_HOST"      "" 000
check "stdout warns on stderr when sourced" 1 "nothing was exported" "" 000

# A server that answers but rejects the credentials is NOT ready. This is the shape that made
# compile-hook.sh report "Compiled OK" (#316): a non-2xx with a zero-byte body.
check "401 is not ready"                   1 "last HTTP code 401"  "export IRIS_HOST=localhost" 401
check "500 is not ready"                   1 "last HTTP code 500"  "export IRIS_HOST=localhost" 500
# A 200-adjacent code must not pass a `= "200"` string test by accident.
check "204 is not ready"                   1 "last HTTP code 204"  "export IRIS_HOST=localhost" 204

# ── the two usages, end to end ──────────────────────────────────────────────────────────
# Drives the documented invocations for real rather than inspecting the emitted snippet, because
# the snippet only matters for what it does in the caller's shell. Two shells behave differently
# and both are documented, so both are exercised — and the bash leg asserts the LIMITATION, so
# nobody "fixes" the script to rely on something that shell cannot do.
stub_dir() {
  local tmp; tmp=$(mktemp -d)
  printf '#!/usr/bin/env bash\nprintf 000\n' >"$tmp/curl"
  printf '#!/usr/bin/env bash\ncase "$1" in inspect) echo "exited, exit 1";; logs) echo "[stub]";; esac\nexit 0\n' >"$tmp/docker"
  chmod +x "$tmp/curl" "$tmp/docker"
  echo "$tmp"
}

# 1. The portable form. `&&` must short-circuit, so a stale env survives untouched but nothing new
#    is set and the caller sees a non-zero status — this is the form the script header recommends.
portable_form_short_circuits() {
  local tmp got; tmp=$(stub_dir)
  got=$(PATH="$tmp:$PATH" IRIS_CONTAINER=iris-up-selftest IRIS_UP_WAIT_ATTEMPTS=2 \
        IRIS_UP_WAIT_INTERVAL=0 bash -c '
          out=$(mktemp)
          if bash '"$ROOT"'/scripts/iris-up.sh >"$out" 2>/dev/null; then echo SOURCED; else echo "SHORT-CIRCUITED"; fi
          grep -q "^export IRIS_HOST" "$out" && echo "AND-EXPORTED" || echo "and-nothing-exported"')
  rm -rf "$tmp"
  case "$got" in
    "SHORT-CIRCUITED"$'\n'"and-nothing-exported")
      echo "ok   [portable form: a failed boot short-circuits and exports nothing]" ;;
    *) echo "FAIL [portable form]: got $(tr '\n' '/' <<<"$got")"; fails=1 ;;
  esac
}
portable_form_short_circuits

# 2. The zsh shorthand, where process substitution does reach the caller: a stale IRIS_HOST from an
#    earlier GOOD boot must be cleared, or the suite talks to a container that is gone.
zsh_shorthand_clears_stale_env() {
  if ! command -v zsh >/dev/null; then
    echo "SKIP [zsh shorthand clears a stale IRIS_HOST]: no zsh on PATH"; return
  fi
  local tmp got; tmp=$(stub_dir)
  got=$(PATH="$tmp:$PATH" IRIS_CONTAINER=iris-up-selftest IRIS_UP_WAIT_ATTEMPTS=2 \
        IRIS_UP_WAIT_INTERVAL=0 zsh -c '
          export IRIS_HOST=stale-host-from-an-earlier-good-boot
          source <(bash '"$ROOT"'/scripts/iris-up.sh 2>/dev/null | tail -2) 2>/dev/null
          echo "${IRIS_HOST:-<unset>}"')
  rm -rf "$tmp"
  if [ "$got" = "<unset>" ]; then
    echo "ok   [zsh shorthand clears a stale IRIS_HOST]"
  else
    echo "FAIL [zsh shorthand clears a stale IRIS_HOST]: IRIS_HOST is '$got'"; fails=1
  fi
}
zsh_shorthand_clears_stale_env

# 3. The control for case 2: the SUCCESS path must still export through the same shorthand, or
#    case 2 would pass against a script that emits nothing at all.
zsh_shorthand_exports_on_success() {
  if ! command -v zsh >/dev/null; then
    echo "SKIP [zsh shorthand exports on a good boot]: no zsh on PATH"; return
  fi
  local tmp got; tmp=$(stub_dir)
  printf '#!/usr/bin/env bash\nprintf 200\n' >"$tmp/curl"   # ready immediately
  got=$(PATH="$tmp:$PATH" IRIS_CONTAINER=iris-up-selftest IRIS_UP_WAIT_ATTEMPTS=2 \
        IRIS_UP_WAIT_INTERVAL=0 zsh -c '
          source <(bash '"$ROOT"'/scripts/iris-up.sh 2>/dev/null | tail -2) 2>/dev/null
          echo "${IRIS_HOST:-<unset>}"')
  rm -rf "$tmp"
  if [ "$got" = "localhost" ]; then
    echo "ok   [zsh shorthand exports on a good boot]"
  else
    echo "FAIL [zsh shorthand exports on a good boot]: IRIS_HOST is '$got'"; fails=1
  fi
}
zsh_shorthand_exports_on_success

# 4. bash 3.2 cannot do any of it. Asserted so the LIMITATION is recorded where it would be broken:
#    `source <(...)` there applies nothing to the caller, not even an export, which is why the
#    header recommends the two-step form.
bash32_shorthand_is_a_noop() {
  local tmp got; tmp=$(stub_dir)
  printf '#!/usr/bin/env bash\nprintf 200\n' >"$tmp/curl"   # a GOOD boot
  got=$(PATH="$tmp:$PATH" IRIS_CONTAINER=iris-up-selftest IRIS_UP_WAIT_ATTEMPTS=2 \
        IRIS_UP_WAIT_INTERVAL=0 /bin/bash -c '
          source <(bash '"$ROOT"'/scripts/iris-up.sh 2>/dev/null | tail -2) 2>/dev/null
          echo "${IRIS_HOST:-<unset>}"')
  rm -rf "$tmp"
  case "$(/bin/bash --version | head -1)" in
    *"version 3."*)
      if [ "$got" = "<unset>" ]; then
        echo "ok   [/bin/bash 3.x: the <(...) shorthand is a no-op, as documented]"
      else
        echo "FAIL [/bin/bash 3.x no-op]: it exported '$got' — the header's warning is now wrong"; fails=1
      fi ;;
    *) echo "SKIP [/bin/bash 3.x no-op]: /bin/bash is not 3.x here" ;;
  esac
}
bash32_shorthand_is_a_noop

if [ "$fails" -ne 0 ]; then echo "FAILED"; exit 1; fi
echo "all iris-up readiness cases pass"
