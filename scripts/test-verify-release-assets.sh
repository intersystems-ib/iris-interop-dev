#!/usr/bin/env bash
# #224: drive verify-release-assets.sh with synthetic data, including the cases that MUST fail.
#
# The real failure only reproduces on a tag push, so the comparison logic is tested here instead
# of being assumed correct. Each case asserts the exit status AND, where it matters, that the
# message NAMES the offending asset — the whole complaint in #224 was one anonymous error line
# for four operations.
set -uo pipefail
cd "$(dirname "$0")/.."
GUARD=scripts/verify-release-assets.sh
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fails=0

check() { # name expected_status pattern_that_must_appear
  local name="$1" want="$2" pattern="${3:-}"
  local out status
  out=$(bash "$GUARD" "$tmp/dist" "$tmp/published.txt" 2>&1); status=$?
  if [ "$status" -ne "$want" ]; then
    echo "FAIL [$name]: expected exit $want, got $status"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  if [ -n "$pattern" ] && ! grep -q "$pattern" <<<"$out"; then
    echo "FAIL [$name]: output does not mention '$pattern'"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  echo "ok   [$name]"
}

reset() { rm -rf "$tmp/dist"; mkdir -p "$tmp/dist"; : > "$tmp/published.txt"; }

# 1. Everything published with bytes -> pass. This is the POSITIVE control: without it, a script
#    that always failed would satisfy every negative case below.
reset
echo x > "$tmp/dist/iris-interop-dev-linux-x64"; echo y > "$tmp/dist/iris-interop-dev-windows-x64.exe"
printf 'iris-interop-dev-linux-x64 12345\niris-interop-dev-windows-x64.exe 6789\n' > "$tmp/published.txt"
check "all published" 0 "all 2 asset(s) present"

# 2. One asset missing from the release -> fail, and NAME it.
reset
echo x > "$tmp/dist/iris-interop-dev-linux-x64"; echo y > "$tmp/dist/iris-interop-dev-macos-arm64"
printf 'iris-interop-dev-linux-x64 12345\n' > "$tmp/published.txt"
check "one asset missing" 1 "iris-interop-dev-macos-arm64 was staged by the build but is NOT in the release"

# 3. Published but zero bytes -> fail. A present-but-empty asset is not a published asset.
reset
echo x > "$tmp/dist/iris-interop-dev-linux-x64-musl"
printf 'iris-interop-dev-linux-x64-musl 0\n' > "$tmp/published.txt"
check "zero-byte asset" 1 "0 bytes"

# 4. THE #224 FAILURE ITSELF: four green builds, release with no assets at all.
reset
for a in linux-x64 linux-x64-musl macos-arm64 windows-x64.exe; do echo x > "$tmp/dist/iris-interop-dev-$a"; done
: > "$tmp/published.txt"
check "release with zero assets" 1 "iris-interop-dev-windows-x64.exe was staged"

# 5. Empty staging dir -> fail, NOT pass. Iterating nothing must never read as success.
reset
printf 'something-else 999\n' > "$tmp/published.txt"
check "empty staging is not a pass" 1 "nothing was staged"

# 6. Missing staging dir -> fail with a distinct reason.
rm -rf "$tmp/dist"; : > "$tmp/published.txt"
check "missing staging dir" 1 "does not exist"

# 7. A substring name must not satisfy a different asset. "…-linux-x64" published must NOT make
#    "…-linux-x64-musl" look published — an awk match on the wrong field would do exactly that.
reset
echo x > "$tmp/dist/iris-interop-dev-linux-x64-musl"
printf 'iris-interop-dev-linux-x64 12345\n' > "$tmp/published.txt"
check "prefix name is not a match" 1 "iris-interop-dev-linux-x64-musl was staged"

# 8. The DANGEROUS substring direction, and the one case 7 does not cover. A short staged name
#    must not be satisfied by a LONGER published name: "…-linux-x64" staged, with only
#    "…-linux-x64-musl" published, would report ok for an asset that was never published if the
#    lookup used a substring match instead of an exact field compare.
#
#    Case 7 alone let a substring mutation SURVIVE — it only proves a longer staged name cannot
#    match a shorter published one, which is the harmless direction. Found by mutating
#    verify-release-assets.sh to `index($1, n)` and watching the suite still pass.
reset
echo x > "$tmp/dist/iris-interop-dev-linux-x64"
printf 'iris-interop-dev-linux-x64-musl 999\n' > "$tmp/published.txt"
check "longer published name is not a match" 1 "iris-interop-dev-linux-x64 was staged"

if [ "$fails" -ne 0 ]; then echo "SOME CHECKS FAILED"; exit 1; fi
echo "all checks passed"
