#!/usr/bin/env bash
#
# Exercise scripts/verify-asset-arch.sh against RECORDED `file -b` output from each platform.
#
# The real guard only runs on a tag push or a workflow_dispatch, on three different runners, so
# this is how its matcher gets verified at all — the same reason scripts/
# test-verify-release-assets.sh exists for the asset-publication guard.
#
# The cases that matter most are the last three: an empty answer, an unrecognised answer, and a
# fat binary. A guard that passes on output it could not classify reports success for every
# asset it failed to inspect.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
guard="$here/verify-asset-arch.sh"
fails=0
ran=0

check() {
  local name="$1" want_rc="$2" arch="$3" desc="$4"
  ran=$((ran + 1))
  local out rc
  out=$("$guard" "$arch" "$desc" 2>&1); rc=$?
  if [ "$rc" != "$want_rc" ]; then
    echo "FAIL  $name: exit $rc, expected $want_rc"
    echo "      $out"
    fails=$((fails + 1))
  else
    echo "ok    $name (exit $rc)"
  fi
}

# ── the four assets this repo publishes, each against its own promise ─────────────────────
check "macos arm64 asset, arm64 binary"  0 arm64 \
  "Mach-O 64-bit executable arm64"
check "macos x64 asset, x86_64 binary"   0 x86_64 \
  "Mach-O 64-bit executable x86_64"
check "linux x64 asset, gnu binary"      0 x86_64 \
  "ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), dynamically linked"
check "linux x64 asset, musl binary"     0 x86_64 \
  "ELF 64-bit LSB executable, x86-64, version 1 (SYSV), statically linked, stripped"
check "windows x64 asset"                0 x86_64 \
  "PE32+ executable (console) x86-64, for MS Windows"

# ── the mislabels this guard exists to stop ──────────────────────────────────────────────
check "arm64 binary under an x64 name"   1 x86_64 \
  "Mach-O 64-bit executable arm64"
check "x86_64 binary under an arm64 name" 1 arm64 \
  "Mach-O 64-bit executable x86_64"
check "linux aarch64 under an x64 name"  1 x86_64 \
  "ELF 64-bit LSB pie executable, ARM aarch64, version 1 (SYSV), dynamically linked"

# ── and the answers that are not answers ─────────────────────────────────────────────────
check "empty file output"                1 x86_64 ""
check "unrecognised file output"          1 x86_64 "data"
check "file not found wording"            1 x86_64 "cannot open \`asset' (No such file or directory)"
check "universal binary is not single-arch" 1 x86_64 \
  "Mach-O universal binary with 2 architectures: [x86_64:Mach-O 64-bit executable x86_64] [arm64]"

# CONTROL: the harness can both pass and fail. A run where every case came back the same way
# would prove nothing about the matcher, only that the script exits consistently.
echo
echo "cases run: $ran"
if [ "$ran" -lt 12 ]; then
  echo "FAIL: only $ran cases ran — this file was expected to exercise at least 12"
  exit 1
fi
if [ "$fails" -ne 0 ]; then
  echo "$fails case(s) FAILED"
  exit 1
fi
echo "all $ran cases passed"
