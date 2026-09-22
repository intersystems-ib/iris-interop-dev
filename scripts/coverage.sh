#!/usr/bin/env bash
# Coverage report: unit tests (precise) + E2E subprocess (via instrumented binary).
#
# Usage (bash — every example here is bash; see WORD SPLITTING below before you
# retype any of it at a zsh prompt):
#   ./scripts/coverage.sh                     # unit only (fast, offline)
#   IRIS_HOST=localhost ./scripts/coverage.sh # unit + E2E (requires live IRIS)
#
# Output: target/coverage/summary.txt
#
# Prerequisites — both are checked below, and a missing one exits with the install
# command rather than a bare "no such file or directory" from under `set -e`:
#   ~/.cargo/bin/rustup component add llvm-tools
#   cargo install cargo-llvm-cov --locked
#
# ── WORD SPLITTING: the one trap in this file ─────────────────────────────────
# scripts/ci-test-targets.sh emits ONE STRING — "--test a --test b ...". It has to
# become separate argv entries. bash word-splits an unquoted expansion on IFS; ZSH
# DOES NOT, so under zsh the entire list arrives as a SINGLE argument, cargo matches
# no target, and the run is green over ZERO tests — indistinguishable from success.
# Two defences, both required:
#   1. This script declares #!/usr/bin/env bash, so its own splitting is bash's.
#   2. It splits EXPLICITLY with `read -r -a` into an array, so it never depends on
#      any shell's word-splitting rules — not even bash's.
# Running the derivation by hand? Do it in bash, and split it explicitly there too:
#   bash -c 'read -r -a T <<<"$(./scripts/ci-test-targets.sh)"; cargo test --lib "${T[@]}"'

set -euo pipefail

# Run from the workspace root whatever the caller's cwd is — the same trick, for the
# same reason, as scripts/ci-test-targets.sh. Every path below is anchored on $ROOT;
# the previous `$(pwd)` form silently wrote target/coverage into whatever directory
# the caller happened to be standing in.
cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$(pwd)"

# ── PREREQUISITE: cargo-llvm-cov ──────────────────────────────────────────────
# Checked here, up front, because the failure mode otherwise is the first
# `cargo-llvm-cov` line dying under `set -e` with "no such file or directory" and no
# hint of what to install.
CARGO_LLVM_COV="${CARGO_LLVM_COV:-$HOME/.cargo/bin/cargo-llvm-cov}"
if [[ ! -x "$CARGO_LLVM_COV" ]]; then
  ON_PATH="$(command -v cargo-llvm-cov 2>/dev/null || true)"
  if [[ -n "$ON_PATH" ]]; then
    CARGO_LLVM_COV="$ON_PATH"
  else
    echo "ERROR: cargo-llvm-cov not found (looked at $CARGO_LLVM_COV and on PATH)." >&2
    echo "       Install it with: cargo install cargo-llvm-cov --locked" >&2
    echo "       Or point CARGO_LLVM_COV at an existing binary." >&2
    exit 1
  fi
fi

# ── PREREQUISITE: the llvm-tools component ────────────────────────────────────
# TEST THE OUTPUT, NOT THE EXIT STATUS. This was two `find | head -1` pipelines joined by
# `||`, and `head -1` exits 0 on EMPTY input — controlled: a find with a non-matching -path
# still exits 0. So the first branch always "succeeded" with an empty string and the generic
# fallback, which exists precisely for hosts that are not aarch64-apple-darwin, was dead code.
# On an Intel Mac or a Linux runner WITH llvm-tools installed, the result was exit 1 and a
# remediation that cannot help. With no ~/.rustup/toolchains at all, both finds failed, `set -e`
# killed the assignment, and the script died with ZERO output.
#
# The host triple also comes from rustc now rather than being hardcoded.
find_llvm_cov() {
  local triple hit
  triple=$(rustc -vV 2>/dev/null | awk '/^host:/ {print $2}')
  if [[ -n "$triple" ]]; then
    hit=$(find ~/.rustup/toolchains -maxdepth 6 -name llvm-cov -path "*/$triple/bin/*" 2>/dev/null | head -1)
    if [[ -n "$hit" ]]; then printf '%s\n' "$hit"; return 0; fi
  fi
  hit=$(find ~/.rustup/toolchains -maxdepth 6 -name llvm-cov -path "*/bin/*" 2>/dev/null | head -1)
  if [[ -n "$hit" ]]; then printf '%s\n' "$hit"; return 0; fi
  return 1
}
TOOLCHAIN_BIN=$(find_llvm_cov || true)
export LLVM_COV=${LLVM_COV:-$TOOLCHAIN_BIN}
export LLVM_PROFDATA=${LLVM_PROFDATA:-${TOOLCHAIN_BIN/llvm-cov/llvm-profdata}}

[[ -f "$LLVM_COV" ]] || { echo "ERROR: llvm-cov not found. Run: ~/.cargo/bin/rustup component add llvm-tools" >&2; exit 1; }

# Set when E2E coverage was ASKED for and could not be measured. Read at the very end, because a
# failure to measure must reach the caller as a non-zero exit and not only as stderr text.
E2E_FAILED=0
PROFILE_DIR="$ROOT/target/coverage/profiles"
SUMMARY="$ROOT/target/coverage/summary.txt"
mkdir -p "$PROFILE_DIR"
rm -f "$PROFILE_DIR"/*.profraw "$PROFILE_DIR"/*.profdata "$PROFILE_DIR"/*.lcov

# ── TEST TARGETS: derived, never listed ───────────────────────────────────────
# This used to be a hand-kept array of target names. It had drifted: it named a
# fraction of the test targets the workspace declares, so the TOTAL below
# under-reported coverage by whatever the absent ones exercise — and nothing said so,
# because a short list and a complete list print the same shape of summary.
#
# scripts/ci-test-targets.sh already derives the list from `cargo metadata`, carries
# the opt-out list with its stale-exclusion guard, and refuses to emit an empty list.
# It is what the CI gate runs. Share it rather than keeping a second, worse copy.
# To see the current list and its size:  ./scripts/ci-test-targets.sh
TEST_TARGET_FLAGS="$("$ROOT/scripts/ci-test-targets.sh")"
DERIVED_TEST_ARGS=()
# Explicit split — never the caller's, never an unquoted expansion. See WORD SPLITTING.
#
# `read -r -a` consumes ONE LINE. That is correct only while ci-test-targets.sh emits a single
# line, and `|| true` would hide the one signal that it stopped early. So the count is COMPARED
# against the source rather than merely tested for zero: an array that is short by any amount is
# the silent degradation this guard exists to catch, and `-eq 0` cannot see it.
read -r -a DERIVED_TEST_ARGS <<<"$TEST_TARGET_FLAGS" || true
EXPECTED_FLAGS=$(printf '%s' "$TEST_TARGET_FLAGS" | tr ' ' '\n' | grep -c -- '^--test$' || true)
GOT_FLAGS=0
for a in ${DERIVED_TEST_ARGS[@]+"${DERIVED_TEST_ARGS[@]}"}; do
  [[ "$a" == "--test" ]] && GOT_FLAGS=$((GOT_FLAGS + 1))
done
if [[ "$GOT_FLAGS" -ne "$EXPECTED_FLAGS" ]]; then
  echo "ERROR: the split kept $GOT_FLAGS of $EXPECTED_FLAGS --test flags." >&2
  echo "       ci-test-targets.sh emitted more than one line, or the split stopped early." >&2
  echo "       A short target list produces a smaller but perfectly plausible TOTAL." >&2
  exit 1
fi

# ── NON-EMPTY GUARD ───────────────────────────────────────────────────────────
# An empty derivation degrades the run to `--lib` alone: a smaller but perfectly
# plausible TOTAL, printed with no error. Refuse instead of reporting it.
if [[ ${#DERIVED_TEST_ARGS[@]} -eq 0 ]]; then
  echo "ERROR: scripts/ci-test-targets.sh emitted no --test flags." >&2
  echo "       Refusing to report coverage of the lib alone as if it were the suite." >&2
  exit 1
fi

TEST_ARGS=(--lib "${DERIVED_TEST_ARGS[@]}")

# Both workspace packages. The derived list spans them — plugin_dispatch_tests lives
# in iris-agentic-dev, not in iris-agentic-dev-core — and a single `--package
# iris-agentic-dev-core` makes cargo reject the run outright with "no test target
# named plugin_dispatch_tests in `iris-agentic-dev-core` package".
COV_PACKAGES=(--package iris-agentic-dev-core --package iris-agentic-dev)

# ── BUILD THE WORKSPACE FIRST ─────────────────────────────────────────────────
# Several derived targets SPAWN target/debug/iris-interop-dev — cli_compile_wildcard_guards,
# mcp_handshake, plugin_dispatch_tests, test_compile_cmd — and they PANIC rather than skip when it is
# absent, deliberately: a test that skipped would report `ok` for a binary that was never built, the
# shape that gave this repo a false verdict four times.
#
# cargo-llvm-cov relocates the target directory to target/llvm-cov-target/, so it never populates
# target/debug/. Without this build the unit half exits 101 on those targets alone — measured, before
# this line existed. Note what this does NOT do: the spawned binary is UNINSTRUMENTED, so lines
# executed inside the subprocess are not attributed here. Attributing them is step 3's job, which
# builds its own instrumented copy. Do not "optimise" this build away.
cargo build --workspace 2>&1 | tail -2

echo "=== Step 1: Unit tests (lib + integration, no IRIS needed) ==="
echo "Test targets from scripts/ci-test-targets.sh: $(( ${#DERIVED_TEST_ARGS[@]} / 2 ))"

"$CARGO_LLVM_COV" llvm-cov \
  "${COV_PACKAGES[@]}" \
  "${TEST_ARGS[@]}" \
  --summary-only 2>&1 | grep -E "^[a-z/].*\.rs|^TOTAL"

echo ""
echo "=== Step 2: Build instrumented iris-interop-dev binary ==="
RUSTFLAGS="-C instrument-coverage" cargo build -p iris-agentic-dev 2>&1 | tail -2
# The [[bin]] name, which is NOT the package name: see crates/iris-agentic-dev-bin/Cargo.toml.
INSTRUMENTED="$ROOT/target/debug/iris-interop-dev"
# Third state, not a negative fact: if the build produced no binary at this path, say
# so. Left unchecked, IRIS_DEV_BIN below points at nothing and the E2E half reports
# failures that look like IRIS problems. This exact mismatch is why the script was broken.
[[ -x "$INSTRUMENTED" ]] || {
  echo "ERROR: no instrumented binary at $INSTRUMENTED after 'cargo build -p iris-agentic-dev'." >&2
  echo "       This path must match the [[bin]] name in crates/iris-agentic-dev-bin/Cargo.toml." >&2
  exit 1
}
echo "Instrumented binary: $INSTRUMENTED"

echo ""
echo "=== Step 3: E2E coverage via instrumented subprocess ==="
if [[ -n "${IRIS_HOST:-}" ]]; then
  COV_DIR="$PROFILE_DIR/e2e"
  mkdir -p "$COV_DIR"

  # Run each E2E test individually so we can collect per-test profraw.
  # Pass the instrumented binary path via IRIS_DEV_BIN — that env var name is what the
  # tests read; it is NOT the binary name and must not be renamed with it.
  # `set -e` + `pipefail` would abort the script here the moment a single e2e test fails, so the
  # NPROF check below — and therefore the final summary — would never run, leaving a STALE TOTAL in
  # summary.txt from a previous run (rm -f above clears only the profile dir). A failing test run is
  # a thing to REPORT, not a reason to stop measuring. Status captured from PIPESTATUS below.
  set +e
  LLVM_PROFILE_FILE="$COV_DIR/iris-interop-dev-%p.profraw" \
    IRIS_DEV_BIN="$INSTRUMENTED" \
    IRIS_HOST="${IRIS_HOST}" IRIS_WEB_PORT="${IRIS_WEB_PORT:-52780}" \
    IRIS_CONTAINER="${IRIS_CONTAINER:-iris-dev-iris}" \
    IRIS_USERNAME="${IRIS_USERNAME:-_SYSTEM}" \
    IRIS_PASSWORD="${IRIS_PASSWORD:-SYS}" IRIS_NAMESPACE="${IRIS_NAMESPACE:-USER}" \
    cargo test --test test_e2e -- --test-threads=1 --include-ignored 2>&1 | tail -3
  E2E_TEST_RC=${PIPESTATUS[0]}

  set -e
  NPROF=$(ls "$COV_DIR"/*.profraw 2>/dev/null | wc -l | tr -d ' ')
  echo "Collected $NPROF E2E profraw files from subprocess (test run exit $E2E_TEST_RC)"

  if [[ "$NPROF" -gt 0 ]]; then
    "$LLVM_PROFDATA" merge -sparse "$COV_DIR"/*.profraw \
      -o "$COV_DIR/e2e-merged.profdata"
    echo ""
    echo "=== E2E subprocess coverage (iris-interop-dev binary paths) ==="
    "$LLVM_COV" report \
      --instr-profile="$COV_DIR/e2e-merged.profdata" \
      --object="$INSTRUMENTED" \
      --ignore-filename-regex="(registry|cargo|rustc|test)" \
      2>/dev/null | grep -E "^[a-z/].*\.rs|^TOTAL" | tee "$PROFILE_DIR/e2e-summary.txt"
  else
    # Third state, not a negative fact. IRIS_HOST was set, so E2E coverage was ASKED
    # for; zero profraw files is a failure to measure, not a measurement of zero. Left
    # as a bare "Collected 0", the run ends with a unit TOTAL and no E2E section at
    # all — which reads exactly like a clean skip.
    echo "ERROR: E2E coverage was requested (IRIS_HOST is set) but the instrumented" >&2
    echo "       subprocess produced no .profraw in $COV_DIR. Either the e2e tests all" >&2
    echo "       skipped/failed before spawning the binary, or the binary they spawned" >&2
    echo "       was not the instrumented one — check that IRIS_DEV_BIN reached them" >&2
    echo "       and that $INSTRUMENTED was built with -C instrument-coverage." >&2
    echo "       No E2E section follows." >&2
    # EXIT 1. This block was added to stop a failure-to-measure reading as a measurement of zero,
    # and it printed to stderr without changing the status — so the script still exited 0 and the
    # caller still saw success. A third state that the exit code cannot express is not a third
    # state. The sibling guard above (no --test flags) exits 1; these now agree.
    E2E_FAILED=1
  fi
else
  echo "IRIS_HOST not set — skipping E2E subprocess coverage"
fi

echo ""
echo "=== Final: Unit coverage summary ==="
"$CARGO_LLVM_COV" llvm-cov \
  "${COV_PACKAGES[@]}" \
  "${TEST_ARGS[@]}" \
  --summary-only 2>&1 | grep "^TOTAL" | tee "$SUMMARY"

echo ""
echo "Reports:"
echo "  Unit summary:     $SUMMARY"
# `if`, not `[[ ... ]] && echo`: an AND-list as the LAST statement makes a false test
# the script's exit status — a complete, correct run reported as a failure whenever
# the E2E half was skipped. Same trap, same fix, as the tail of ci-test-targets.sh.
if [[ -f "$PROFILE_DIR/e2e-summary.txt" ]]; then
  echo "  E2E summary:      $PROFILE_DIR/e2e-summary.txt"
fi

# The unit summary above is real and worth printing even when the E2E half could not be measured —
# so the failure is reported HERE, after the output, rather than aborting mid-run and leaving
# summary.txt holding a stale TOTAL from a previous invocation.
if [[ "$E2E_FAILED" -ne 0 ]]; then
  echo "" >&2
  echo "FAILED: E2E coverage was requested and not measured (see the ERROR above)." >&2
  echo "        The unit summary printed above is unaffected and current." >&2
  exit 1
fi
exit 0
