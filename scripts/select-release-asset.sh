#!/usr/bin/env bash
#
# Which release asset can THIS host actually execute?
#
# WHY THIS EXISTS (#543). Nothing selected an asset by architecture. The README's Linux install
# line named `iris-interop-dev-linux-x64` flatly, so an arm64 Linux host — Graviton, an arm64 CI
# runner, Docker Desktop's arm64 Linux VM on an Apple-silicon Mac — downloaded the x64 binary, the
# SHA-256 GitHub reports matched the bytes that arrived, and the first thing that failed was
# `exec`. A checksum that passes on a binary for another instruction set verified nothing.
#
# That is the third instance of one shape. #174: the gnu asset's GLIBC_2.39 floor — downloaded,
# hash-verified, died at exec. #423: macOS published arm64 only, so an Intel Mac had no asset —
# downloaded, hash-verified, died at exec. Both were repaired by publishing the missing asset, and
# #423 added scripts/verify-asset-arch.sh so an asset cannot be built for an architecture its NAME
# does not promise. But that guard runs at BUILD time and answers "is this asset what it says it
# is". It cannot answer "is there an asset for this host at all", which is the question a download
# asks — so the absent-architecture case kept reaching users as a successful download.
#
# THE RULE THIS ENCODES (CLAUDE.md): a failure must never be answered with a negative fact. There
# are THREE outcomes here, not two. "No asset is published for your architecture" is its own
# outcome with its own exit code and its own message naming the architecture — never the
# nearest-looking asset, and never an empty string a caller would paste into a URL.
#
# It takes <uname -s> <uname -m> as ARGUMENTS, defaulting to this host, so every platform can be
# exercised on one machine without building anything — the same reason verify-asset-arch.sh takes
# `file -b` output rather than a path. The exercise is
# crates/iris-agentic-dev-core/tests/a_wrong_arch_download_verifies_before_it_fails.rs, which also
# compares the catalogue below against release.yml's build matrix in BOTH directions: the failure
# class is a published-asset set and an asset-selecting consumer drifting apart.
#
# Usage:   select-release-asset.sh [--musl] [<uname-s> <uname-m>]
#          select-release-asset.sh --list
#
# Exit:    0  an asset exists for this host; its name is on stdout, and nothing else is
#          2  this script was called wrongly
#          3  the platform is recognised but NO asset is published for that architecture/libc
#          4  the operating system is not one this project publishes for
set -uo pipefail

# ── THE CATALOGUE ─────────────────────────────────────────────────────────────────────────────
# One row per published asset: <os> <arch> <libc> <asset-name>. `-` in the libc column means the
# platform has exactly one flavour. This table and release.yml's `asset:` entries are asserted
# equal, both ways, by the test named above — so adding a row here without a matrix row (or the
# other way round) is RED rather than a 404 at install time.
CATALOGUE="
darwin  arm64   -     iris-interop-dev-macos-arm64
darwin  x86_64  -     iris-interop-dev-macos-x64
linux   x86_64  gnu   iris-interop-dev-linux-x64
linux   x86_64  musl  iris-interop-dev-linux-x64-musl
linux   arm64   gnu   iris-interop-dev-linux-arm64
linux   arm64   musl  iris-interop-dev-linux-arm64-musl
windows x86_64  -     iris-interop-dev-windows-x64.exe
"

usage() {
  cat >&2 <<EOF
usage: $0 [--musl] [<uname-s> <uname-m>]
       $0 --list

With no platform arguments the host is read from \`uname -s\` and \`uname -m\`.
--musl asks for the statically linked flavour (#174); it exists for Linux only.

  0  an asset exists for this host; its name is on stdout
  2  called wrongly (this message)
  3  the platform is known but publishes no asset for that architecture/libc
  4  the operating system is not one this project publishes for
EOF
}

rows() { printf '%s\n' "$CATALOGUE" | awk 'NF == 4'; }

# The uname spellings that mean one catalogue architecture. Printed in refusals so the remedy is
# stated in the vocabulary the user's own `uname -m` speaks.
arch_spellings() {
  case "$1" in
    x86_64) echo "x86_64/amd64" ;;
    arm64)  echo "aarch64/arm64" ;;
    *)      echo "$1" ;;
  esac
}

published_arches() {
  rows | awk -v os="$1" '$1 == os { print $2 }' | sort -u | while read -r a; do
    arch_spellings "$a"
  done | paste -sd, - | sed 's/,/, /g'
}

want_musl=0
do_list=0
args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --musl) want_musl=1 ;;
    --list) do_list=1 ;;
    -h|--help) usage; exit 2 ;;
    --*) echo "$0: unknown option $1" >&2; usage; exit 2 ;;
    *) args+=("$1") ;;
  esac
  shift
done

if [ "$do_list" -eq 1 ]; then
  if [ ${#args[@]} -ne 0 ]; then
    echo "$0: --list takes no platform arguments" >&2
    usage
    exit 2
  fi
  rows | awk '{ print $4 }' | sort -u
  exit 0
fi

# Exactly zero or exactly two positionals. ONE is a caller bug — and it must not be mistaken for a
# platform verdict, or a typo would read as "your machine is unsupported".
case ${#args[@]} in
  0) raw_os="$(uname -s)"; raw_arch="$(uname -m)" ;;
  2) raw_os="${args[0]}";  raw_arch="${args[1]}" ;;
  *) echo "$0: expected either no platform arguments or exactly two (<uname -s> <uname -m>), got ${#args[@]}" >&2
     usage
     exit 2 ;;
esac

case "$raw_os" in
  Linux|linux)                                  os=linux ;;
  Darwin|darwin)                                os=darwin ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT|windows)      os=windows ;;
  *)                                            os="" ;;
esac

if [ -z "$os" ]; then
  # OUTCOME 4, and deliberately distinct from outcome 3: collapsing them would hide which half of
  # (platform, architecture) is the one nothing is published for.
  {
    echo "iris-interop-dev: no release asset — '${raw_os:-(empty)}' is not an operating system this project publishes for."
    echo "  host: ${raw_os:-(empty)} ${raw_arch:-(empty)}"
    echo "  published platforms: $(rows | awk '{ print $1 }' | sort -u | paste -sd, - | sed 's/,/, /g')"
    echo "  Build from source instead: cargo build --release -p iris-agentic-dev"
  } >&2
  exit 4
fi

case "$raw_arch" in
  x86_64|amd64|x64|AMD64)        arch=x86_64 ;;
  aarch64|arm64|arm64e|armv8*)   arch=arm64 ;;
  *)                             arch="" ;;
esac

want_libc="-"
if [ "$want_musl" -eq 1 ]; then
  want_libc=musl
elif [ "$os" = linux ]; then
  want_libc=gnu
fi

asset=""
if [ -n "$arch" ]; then
  asset=$(rows | awk -v os="$os" -v arch="$arch" -v libc="$want_libc" \
    '$1 == os && $2 == arch && $3 == libc { print $4; exit }')
fi

if [ -z "$asset" ]; then
  # OUTCOME 3. Note what this does NOT do: fall back to another architecture's asset, or print an
  # empty line and exit 0. Either one puts a binary that cannot run on the machine, past a
  # checksum that will happily confirm it.
  {
    flavour=""
    [ "$want_musl" -eq 1 ] && flavour=" (musl)"
    echo "iris-interop-dev: no $os release asset for '$raw_arch'$flavour."
    echo "  host: $raw_os $raw_arch"
    echo "  published for $os: $(published_arches "$os")"
    if [ "$want_musl" -eq 1 ]; then
      echo "  the statically linked (musl) flavour is published for linux only"
    fi
    echo "  Refusing at download time ON PURPOSE: an asset for another architecture downloads and"
    echo "  hash-verifies, and then fails at exec — which reads as a broken release (#543)."
    echo "  Build from source instead: cargo build --release -p iris-agentic-dev"
  } >&2
  exit 3
fi

printf '%s\n' "$asset"
