#!/usr/bin/env bash
# Per-tool validation gate for the interop fork: a tool may be marked "OK" in tools-status.json
# only if it names BOTH a unit and an e2e test. Fails (exit 1) if any "OK" entry is missing one.
# Also confirms both execution transports (HTTP + docker) name a test. Read-only / fast.
#
# It ALSO cross-checks the manifest against INTEROP_TOOLS, because without that it validated the
# list against itself: it iterates tools-status.json's own entries, so a tool absent from the file
# was simply not checked, and the gate printed "GATE OK". Measured when this was added —
# INTEROP_TOOLS had 31 entries and the manifest 29, missing iris_coverage and iris_doc_search, both
# shipping with no validation record. Same shape as #294, where the README documented 23 of 30
# tools while looking maintained.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
python3 - "$ROOT/tools-status.json" "$ROOT/crates/iris-agentic-dev-core/src/tools/mod.rs" <<'PY'
import json, re, sys
m = json.load(open(sys.argv[1]))
bad = 0
print(f"{'tool':34} {'status':12} unit / e2e")
print("-" * 90)
for t in m["tools"]:
    unit, e2e, st, tool = t.get("unit"), t.get("e2e"), t["status"], t["tool"]
    flag = ""
    if st == "OK" and (not unit or not e2e):
        flag = "  <-- OK but missing unit/e2e!"; bad += 1
    print(f"{tool:34} {st:12} {unit} / {e2e}{flag}")
print("-" * 90)
for k, tr in m["transports"].items():
    print(f"transport:{k:23} {tr['status']:12} {tr['test']}")
    if tr["status"] == "OK" and not tr.get("test"):
        bad += 1
# ── no restated count: a number here rots silently, because nothing reads these strings ──
# Measured: `profile` said "interop (23 tools, default)" and `_comment` said "the 23-tool interop
# profile" while INTEROP_TOOLS held 31. Both were wrong by eight and no gate noticed — the drift
# check below compares NAMES, and no code reads either string. The real count is computed and
# printed further down from INTEROP_TOOLS, which is the only copy that cannot lag.
for field in ("_comment", "profile"):
    restated = re.search(r"\b\d+[- ]tools?\b", m.get(field, ""), re.I)
    if restated:
        print(f"GATE FAILED: {field} restates a tool count ({restated.group(0)!r}). Remove the "
              "number — this gate prints the real one from INTEROP_TOOLS on every run, and a "
              "hand-written copy here has already been wrong by eight.")
        bad += 1

# ── drift check: every advertised interop tool must appear in the manifest ────────
src = open(sys.argv[2]).read()
km = re.search(r"INTEROP_TOOLS[^=]*=\s*&?\[(.*?)\];", src, re.S)
if not km:
    print("GATE FAILED: could not find INTEROP_TOOLS in mod.rs, so the manifest was not checked "
          "against anything. A gate that cannot look must not pass.")
    sys.exit(1)
keep = set(re.findall(r'"([a-z_][a-z_0-9]*)"', km.group(1)))
# Plausibility control: a regex that silently matched almost nothing would make the two
# comparisons below vacuous and the gate would read as clean.
if len(keep) < 20:
    print(f"GATE FAILED: INTEROP_TOOLS parsed to only {len(keep)} names, which cannot be right — "
          "refusing to report a clean result from a broken parse.")
    sys.exit(1)
listed = {t["tool"] for t in m["tools"]}
unlisted = sorted(keep - listed)
stale = sorted(listed - keep)
print()
if unlisted:
    print(f"advertised in INTEROP_TOOLS but ABSENT from tools-status.json ({len(unlisted)}): "
          f"{', '.join(unlisted)}")
    bad += len(unlisted)
if stale:
    print(f"in tools-status.json but NOT advertised ({len(stale)}): {', '.join(stale)}")
    bad += len(stale)
if not unlisted and not stale:
    print(f"manifest covers all {len(keep)} advertised interop tools.")

ps = m["profile_surface"]
print(f"profile_surface{'':19} {ps['status']:12} {ps['test']}")
ok = sum(1 for t in m["tools"] if t["status"] == "OK")
unitok = sum(1 for t in m["tools"] if t["status"] == "unit-ok")
print()
print(f"{ok} tools OK (unit+e2e green), {unitok} unit-ok (e2e pending interop ns), "
      f"{len(m['tools'])} total. Transports HTTP+docker validated.")
if bad:
    # The reasons are printed above, each naming itself. This line must NOT restate one of them:
    # it counted every kind of problem while claiming they were all missing unit/e2e tests, so a
    # restated-count failure was reported as a missing test.
    print(f"GATE FAILED: {bad} problem{'' if bad==1 else 's'} above.")
    sys.exit(1)
print("GATE OK: every 'OK' tool names a unit AND an e2e test.")
PY
