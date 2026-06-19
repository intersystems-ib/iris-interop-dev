#!/usr/bin/env bash
# Per-tool validation gate for the interop fork: a tool may be marked "OK" in tools-status.json
# only if it names BOTH a unit and an e2e test. Fails (exit 1) if any "OK" entry is missing one.
# Also confirms both execution transports (HTTP + docker) name a test. Read-only / fast.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
python3 - "$ROOT/tools-status.json" <<'PY'
import json, sys
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
ps = m["profile_surface"]
print(f"profile_surface{'':19} {ps['status']:12} {ps['test']}")
ok = sum(1 for t in m["tools"] if t["status"] == "OK")
unitok = sum(1 for t in m["tools"] if t["status"] == "unit-ok")
print()
print(f"{ok} tools OK (unit+e2e green), {unitok} unit-ok (e2e pending interop ns), "
      f"{len(m['tools'])} total. Transports HTTP+docker validated.")
if bad:
    print(f"GATE FAILED: {bad} 'OK' entr{'y' if bad==1 else 'ies'} missing a unit or e2e test.")
    sys.exit(1)
print("GATE OK: every 'OK' tool names a unit AND an e2e test.")
PY
