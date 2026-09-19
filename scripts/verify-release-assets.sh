#!/usr/bin/env bash
# #224: verify that every staged asset actually made it into the release, with bytes.
#
# The failure this guards is a GREEN run with an empty release: uploading four assets through
# softprops/action-gh-release's `files:` glob failed twice for v0.20.0-interop with one
# unattributable "Error saving asset" — no asset name, no HTTP status, one line for four parallel
# operations — while all four build jobs reported success. `gh release view` was left showing
# `draft: true` and an empty asset list. So publication is read back rather than assumed.
#
# The expected list is DERIVED from the staging directory, i.e. the artifacts the build jobs
# actually produced. It is deliberately NOT a hardcoded list of asset names, which would
# duplicate the `asset:` entries in release.yml's own build matrix — the duplication #240
# removed for test targets. Add a platform to the matrix and this covers it with no edit here.
#
# Usage:   verify-release-assets.sh <staging-dir> <published-list-file>
#
# <published-list-file> holds one "<name> <size>" per line, as produced by:
#   gh release view "$TAG" --json assets --jq '.assets[] | "\(.name) \(.size)"'
#
# Taking the published list as a FILE rather than calling gh directly is what makes this
# testable without cutting a release: scripts/test-verify-release-assets.sh drives it with
# synthetic lists, including the cases that must fail.
set -euo pipefail

staging="${1:?usage: verify-release-assets.sh <staging-dir> <published-list-file>}"
published="${2:?usage: verify-release-assets.sh <staging-dir> <published-list-file>}"

if [ ! -d "$staging" ]; then
  echo "::error::staging directory '$staging' does not exist — the download step did not run"
  exit 1
fi
if [ ! -f "$published" ]; then
  echo "::error::published-asset list '$published' does not exist — the release was never read back"
  exit 1
fi

# POSITIVE CONTROL, first and deliberately. With an empty staging directory the loop below
# iterates nothing and every check trivially passes, so "all assets published" would be
# vacuously true — the exact false-green shape this script exists to catch.
staged=$(find "$staging" -maxdepth 1 -type f | wc -l | tr -d ' ')
if [ "$staged" -eq 0 ]; then
  echo "::error::'$staging' holds no files — nothing was staged, so this check would pass without verifying anything"
  exit 1
fi
echo "staged $staged asset(s) in $staging"

missing=0
for f in "$staging"/*; do
  [ -f "$f" ] || continue
  name=$(basename "$f")
  size=$(awk -v n="$name" '$1 == n { print $2 }' "$published")
  if [ -z "$size" ]; then
    echo "::error::$name was staged by the build but is NOT in the release"
    missing=1
  elif [ "$size" -eq 0 ]; then
    echo "::error::$name is in the release with 0 bytes"
    missing=1
  else
    echo "ok: $name ($size bytes)"
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "::error::the release is incomplete — see the named assets above. This is the #224 failure: green build jobs and an unusable release."
  exit 1
fi
echo "all $staged asset(s) present with bytes"
