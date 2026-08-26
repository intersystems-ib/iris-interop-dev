#!/usr/bin/env bash
#
# Emit the `--test <name>` flags for the required CI gate (#95).
#
# THE RULE: the gate runs EVERY test target the workspace declares or auto-discovers,
# minus the explicit opt-outs below. A newly added test target is therefore covered by
# default — you do not have to remember to add it anywhere.
#
# If you are adding a test target that CANNOT run in CI (needs a live IRIS, a container,
# or the network), you have two options, in order of preference:
#   1. Mark the individual tests `#[ignore = "requires <what>"]`. This is what the live
#      IRIS e2e targets do; they stay in the gate, cost ~0s, and self-document.
#   2. If the whole target is unrunnable, add it to EXCLUDED below WITH A REASON.
#
# Targets are read from `cargo metadata`, not from Cargo.toml, on purpose: transport_e2e
# is auto-discovered from tests/*.rs and appears in no [[test]] block, so a hand-kept list
# could never see it. That is exactly the blind spot #95 exists to close.

set -euo pipefail

# Run from the workspace root whatever the caller's cwd is: `cargo metadata` below
# resolves Cargo.toml from the cwd, and from a subdirectory it dies with a JSON decode
# traceback rather than anything readable. Actions runs steps from the root; humans do not.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# ── THE OPT-OUT LIST — the single readable place. One entry, one reason. ──────────
#
# docker_discovery_e2e: `docker run`s real intersystemsdc/iris-community images, sleeps
#   25s per container, and then `docker exec ... iris session iris -U %SYS
#   ##class(Security.Users).Create(...)` to make a user. Multi-GB image pulls, minutes of
#   runtime, and 2 of its 6 tests fail outright with no docker daemon. It also uses the
#   `iris session` path this repo's house rules forbid everywhere else — worth fixing, but
#   rewriting an excluded e2e target was out of scope for #95. Run it by hand when
#   touching container discovery.
EXCLUDED=( docker_discovery_e2e )

# python3 rather than jq: both are on ubuntu-latest, but contributors run this locally too
# and python3 is the harder dependency to lose (notably on a stock macOS box).
ALL=$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
d = json.load(sys.stdin)
names = {t["name"] for p in d["packages"] for t in p["targets"] if "test" in t["kind"]}
print("\n".join(sorted(names)))
')

# ── ANTI-DRIFT GUARD ──────────────────────────────────────────────────────────────
# A renamed or deleted target must not sit in EXCLUDED unnoticed, silently opting a
# target that no longer exists out of a gate it was never in. Same role test_toolset
# plays for the 54/50/46/23 tool counts.
#
# `${EXCLUDED[@]+"${EXCLUDED[@]}"}` rather than plain `"${EXCLUDED[@]}"`: under `set -u`,
# bash 3.2 — stock /bin/bash on macOS, the box this script deliberately caters to — treats
# an EMPTY array expansion as an unbound variable and aborts. Retiring the last opt-out
# (`EXCLUDED=()`) is the goal state, and it must not break the script that measures it.
for x in ${EXCLUDED[@]+"${EXCLUDED[@]}"}; do
  grep -qx "$x" <<<"$ALL" || {
    echo "stale exclusion: no test target named $x" >&2
    exit 1
  }
done

# Collect first, print last. The previous form ended on `[[ $skip -eq 0 ]] && printf ...`,
# whose status becomes the script's when the LAST target in sorted order is the excluded
# one: a correct, complete list on stdout, nothing on stderr, and exit 1. Verified with
# EXCLUDED=( vscode_config_tests ) — 43 correct flags, exit 1. Invisible while the caller
# swallowed the exit code; a mystery red gate the moment it stopped doing that. Use `if`.
FLAGS=()
for n in $ALL; do
  skip=0
  for x in ${EXCLUDED[@]+"${EXCLUDED[@]}"}; do
    if [[ "$n" == "$x" ]]; then skip=1; fi
  done
  if [[ $skip -eq 0 ]]; then
    FLAGS+=( --test "$n" )
  fi
done

# ── NON-EMPTY GUARD ───────────────────────────────────────────────────────────────
# Never exit 0 with nothing to say. An empty list turns the caller's
# `cargo test --workspace --lib --bins <flags> --no-fail-fast` into `--lib --bins`:
# 2 targets, 408 tests, green, every test target gone. That is the exact silent
# degradation #95 exists to prevent, so refuse loudly instead.
if [[ ${#FLAGS[@]} -eq 0 ]]; then
  echo "no test targets emitted: cargo metadata reported none, or EXCLUDED covers them all" >&2
  exit 1
fi

printf -- '%s ' "${FLAGS[@]}"
