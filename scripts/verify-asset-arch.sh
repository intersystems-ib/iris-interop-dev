#!/usr/bin/env bash
#
# Does a release asset contain the architecture its NAME promises?
#
# WHY THIS EXISTS. Every asset name in this repo states an architecture
# (`-macos-arm64`, `-macos-x64`, `-linux-x64`, `-windows-x64.exe`) and until now nothing
# checked it. The release matrix is the only thing that ties a name to a `--target`, so a
# one-line edit — a changed runner label, a copied matrix row, a cross-compile added to an
# existing row — publishes a binary under a name that promises a different machine. It
# downloads, it hash-verifies, and it dies at exec. That is #174's failure exactly, one
# layer up: there the floor was GLIBC, here it is the instruction set.
#
# It takes the OUTPUT of `file -b`, not a path, so the matcher can be exercised against
# recorded strings from all three platforms without building anything. scripts/
# test-verify-asset-arch.sh is that exercise, and it includes the empty and unrecognised
# cases — a guard that passes on output it could not classify is not a guard.
#
# Usage: verify-asset-arch.sh <x86_64|arm64> <file -b output ...>
set -euo pipefail

want="${1:-}"
shift || true
desc="${*:-}"

if [ -z "$want" ]; then
  echo "usage: $0 <x86_64|arm64> <file output>" >&2
  exit 2
fi

# Spellings, measured from `file -b` on each platform:
#   Mach-O 64-bit executable x86_64                      → x86_64
#   Mach-O 64-bit executable arm64                       → arm64
#   ELF 64-bit LSB pie executable, x86-64, …             → x86-64
#   ELF 64-bit LSB executable, ARM aarch64, …            → aarch64
#   PE32+ executable (console) x86-64, for MS Windows    → x86-64
# The two families use different separators, which is the whole reason this is a script and
# not an inline `grep x86_64`.
saw=""
case "$desc" in
  *x86_64*|*x86-64*) saw="x86_64" ;;
esac
case "$desc" in
  *arm64*|*aarch64*) [ -n "$saw" ] && saw="both" || saw="arm64" ;;
esac

if [ -z "$saw" ]; then
  echo "::error::cannot tell what architecture this asset is built for, so its name is unverified."
  echo "  expected: $want"
  echo "  file output: ${desc:-(empty)}"
  echo "  An unclassifiable answer is NOT a pass — if \`file\` is missing or its wording changed," \
       "fix this guard rather than removing it."
  exit 1
fi

if [ "$saw" = "both" ]; then
  echo "::error::this asset names more than one architecture (a universal/fat binary)."
  echo "  expected exactly: $want"
  echo "  file output: $desc"
  echo "  Every asset here is single-arch by construction; a fat binary means the build changed."
  exit 1
fi

if [ "$saw" != "$want" ]; then
  echo "::error::architecture mismatch — this asset is $saw but its name promises $want."
  echo "  file output: $desc"
  echo "  Publishing it would ship a binary that fails at exec on the machine that downloads it."
  exit 1
fi

echo "arch confirmed: $saw"
