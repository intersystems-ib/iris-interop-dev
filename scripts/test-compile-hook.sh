#!/usr/bin/env bash
# Drive scripts/compile-hook.sh, which since #529 is a thin wrapper over
# `iris-interop-dev hook post-edit` rather than an HTTP client.
#
# WHERE THE OLD CASES WENT. This script used to stub `curl` and assert on the hook's own verdict
# text — the 401-with-a-zero-byte-body case, the non-JSON body, the real compile errors. Those
# assertions were not dropped; they moved to Rust, where they can drive the real code instead of a
# shell reimplementation of it:
#
#   crates/iris-agentic-dev-core/tests/hook_post_edit.rs
#     an_upload_refused_with_401_is_unknown_not_a_clean_compile
#     an_upload_refused_inside_a_200_is_unknown_and_stops_there
#     compile_errors_are_reported_and_are_actionable
#     the_source_is_uploaded_before_it_is_compiled      <- the defect #529 was filed for
#   crates/iris-agentic-dev-core/tests/hook_document_name.rs   <- #530, the document name
#
# What is left HERE is the only thing Rust cannot see: the wrapper's own contract. Which binary it
# picks, what it does when there is none, and whether the payload and the exit code survive the
# hand-off. That last part is load-bearing — a wrapper that swallows exit 2 turns every compile
# error back into silence, which is the whole of #529 reintroduced one layer up.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$ROOT/scripts/compile-hook.sh"
PAYLOAD='{"tool_input":{"file_path":"/w/Demo/Thing.cls"}}'
fails=0

check() { # label expected_exit expect_substring actual_exit actual_output
  local label="$1" want_exit="$2" want_text="$3" got_exit="$4" got="$5"
  if [[ "$got_exit" != "$want_exit" ]]; then
    echo "FAIL [$label]: exit $got_exit, expected $want_exit"
    printf '%s\n' "$got" | sed 's/^/    /'
    fails=$((fails + 1))
    return
  fi
  if [[ -n "$want_text" && "$got" != *"$want_text"* ]]; then
    echo "FAIL [$label]: output lacks '$want_text'"
    printf '%s\n' "$got" | sed 's/^/    /'
    fails=$((fails + 1))
    return
  fi
  echo "ok   [$label]"
}

# A PATH with nothing on it: the wrapper uses only shell builtins, so this isolates it from any
# iris-interop-dev that happens to be installed on this machine. Without this the "no binary" case
# silently tests the developer's Homebrew build instead.
# bash is resolved BEFORE the PATH is emptied — otherwise `env PATH=... bash` cannot find bash
# itself and the case dies with exit 127 "bash: No such file or directory". That failure also made
# the next check pass vacuously, on error text that happened to contain no success word.
BASH_BIN="$(command -v bash)"
empty_path_run() {
  local tmp rc
  tmp=$(mktemp -d)
  printf '%s' "$PAYLOAD" | env -u CLAUDE_PLUGIN_ROOT -u IRIS_INTEROP_DEV_BIN \
    PATH="$tmp" "$BASH_BIN" "$HOOK" 2>&1
  rc=$?
  rm -rf "$tmp"
  return $rc
}

# 1. No binary at all. Exit 0 — a missing install must not block every edit — but it must SAY the
#    file was not checked. Silence that reads as success is the defect, not the silence itself.
out=$(empty_path_run); rc=$?
check "no binary: exits 0 and says it did not check" 0 "NOT compile-checked" "$rc" "$out"
# The negative half of the same case, stated separately because it is the one that matters: the
# wrapper must never emit anything a model would read as a clean compile.
if [[ "$out" == *"Compiled"* || "$out" == *" OK"* ]]; then
  echo "FAIL [no binary: must not read as a clean compile]: $out"
  fails=$((fails + 1))
else
  echo "ok   [no binary: must not read as a clean compile]"
fi

# 2. An override that is not executable is the same situation, reported in its own words rather
#    than falling through to PATH and compiling against who-knows-what.
tmp=$(mktemp -d)
out=$(printf '%s' "$PAYLOAD" | IRIS_INTEROP_DEV_BIN="$tmp/not-there" bash "$HOOK" 2>&1); rc=$?
check "unusable override: exits 0 and names the variable" 0 "IRIS_INTEROP_DEV_BIN" "$rc" "$out"
rm -rf "$tmp"

# 3. The hand-off. A stub records its argv and stdin, then exits 2 the way a real compile failure
#    does. All three have to survive: the subcommand name, the payload, and the exit code.
tmp=$(mktemp -d)
cat >"$tmp/bin" <<'STUB'
#!/usr/bin/env bash
echo "argv: $*"
echo "stdin: $(cat)"
echo "a compile error, on stderr" >&2
exit 2
STUB
chmod +x "$tmp/bin"
out=$(printf '%s' "$PAYLOAD" | IRIS_INTEROP_DEV_BIN="$tmp/bin" bash "$HOOK" 2>&1); rc=$?
check "hand-off: the subcommand is 'hook post-edit'" 2 "argv: hook post-edit" "$rc" "$out"
check "hand-off: the payload reaches stdin" 2 "$PAYLOAD" "$rc" "$out"
check "hand-off: stderr survives" 2 "on stderr" "$rc" "$out"

# 3b. THE control for the three above. Exit 2 is what carries a compile error to the model, so a
#     wrapper that always propagated 2 would pass them all while being just as broken. A clean run
#     must come back 0.
cat >"$tmp/ok" <<'STUB'
#!/usr/bin/env bash
cat >/dev/null
echo '{"hookSpecificOutput":{"additionalContext":"Compiled Demo.Thing.cls in APP — no errors."}}'
exit 0
STUB
chmod +x "$tmp/ok"
out=$(printf '%s' "$PAYLOAD" | IRIS_INTEROP_DEV_BIN="$tmp/ok" bash "$HOOK" 2>&1); rc=$?
check "control: a clean run comes back 0, not 2" 0 "additionalContext" "$rc" "$out"
rm -rf "$tmp"

if [[ "$fails" -gt 0 ]]; then
  echo "SOME CHECKS FAILED ($fails)"
  exit 1
fi
echo "all compile-hook wrapper checks passed"
