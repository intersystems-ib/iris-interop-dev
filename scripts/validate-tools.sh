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
# #531: the manifest records validation for every tool the fork advertises in ANY profile, so the
# drift check is against the UNION of the keep-lists. Checking only INTEROP_TOOLS would report
# "manifest covers all advertised tools" while a routines-only tool had no validation record at
# all — and iris_search and iris_info were exactly that case when the profile was added.
#
# Anchored on the definition: the old `INTEROP_TOOLS[^=]*=` pattern matched the first MENTION, and
# a keep_list() accessor naming the const earlier in the file shifted the window.
def _keep_list(name, text):
    m = re.search(r"pub const %s: &\[&str\] = &\[(.*?)\n\];" % name, text, re.S)
    if not m:
        print(f"GATE FAILED: could not find `pub const {name}` in mod.rs, so the manifest was not "
              "checked against anything. A gate that cannot look must not pass.")
        sys.exit(1)
    return set(re.findall(r'"([a-z_][a-z_0-9]*)"', m.group(1)))

keep_interop = _keep_list("INTEROP_TOOLS", src)
keep_routines = _keep_list("ROUTINES_TOOLS", src)
keep = keep_interop | keep_routines
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
    print(f"advertised in a profile but ABSENT from tools-status.json ({len(unlisted)}): "
          f"{', '.join(unlisted)}")
    bad += len(unlisted)
if stale:
    print(f"in tools-status.json but NOT advertised ({len(stale)}): {', '.join(stale)}")
    bad += len(stale)
if not unlisted and not stale:
    print(f"manifest covers all {len(keep)} tools advertised by interop or routines "
          f"({len(keep_interop)} / {len(keep_routines)}, overlapping).")

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

# ── #353: the upstream surface, computed rather than restated ─────────────────────
# Why this lives here: the gate above already compares NAMES against INTEROP_TOOLS and refuses a
# restated count. What it could not answer is "which of upstream's tools are we not advertising, and
# is that because we never ported them or because we pruned them?" — two states with different
# remedies (a port vs one line in INTEROP_TOOLS) that the issue's "48 excluded" conflated.
#
# NO COUNT IS WRITTEN HERE. #353 was filed stating upstream's total and ours, and both were wrong
# by the time anyone read it — which is the same failure this gate already refuses in
# tools-status.json's `_comment` and `profile` fields. Run the script; it prints the current numbers
# and the arithmetic that ties them together.
#
# THREE outcomes, not two. If `upstream/master` is not fetched, this section says so and does NOT
# fail the gate (CI need not carry the remote) — but it must never report the comparison as clean,
# which is the same rule the rest of this gate follows.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if ! git -C "$ROOT" rev-parse --verify --quiet upstream/master >/dev/null 2>&1; then
  echo
  echo "UPSTREAM SURFACE: NOT CHECKED — upstream/master is not fetched in this clone."
  echo "  Run: git fetch upstream master    (remote 'upstream' must exist)"
  echo "  This is not a pass: the comparison did not run."
  exit 0
fi

python3 - "$ROOT" <<'PY'
import re, subprocess, sys
ROOT = sys.argv[1]
TOOL = re.compile(r'#\[tool\((?:[^)]|\)[^\]])*\)\]\s*(?:pub\s+)?async\s+fn\s+(\w+)', re.S)
MOD  = re.compile(r'#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{')

def git(*a):
    return subprocess.run(["git","-C",ROOT,*a], capture_output=True, text=True).stdout

def strip_test_mods(t):
    out, i = [], 0
    while True:
        m = MOD.search(t, i)
        if not m:
            out.append(t[i:]); break
        out.append(t[i:m.start()])
        d, j = 1, m.end()
        while j < len(t) and d:
            if t[j] == '{': d += 1
            elif t[j] == '}': d -= 1
            j += 1
        i = j
    return "".join(out)

def tools_at(ref):
    files = [f for f in git("ls-tree","-r","--name-only",ref).split("\n")
             if re.search(r"src/tools/.*\.rs$", f)]
    names = set()
    for f in files:
        txt = git("show", f"{ref}:{f}")
        names |= set(TOOL.findall(re.sub(r"//.*", "", strip_test_mods(txt))))
    return names

up   = tools_at("upstream/master")
ours = tools_at("HEAD")

# CONTROL: an empty or tiny read is a broken scan, not a small upstream. Without this the buckets
# below would report "we ported everything" from a renamed path or a moved crate directory.
if len(up) < 40 or len(ours) < 40:
    print()
    print(f"UPSTREAM SURFACE: NOT CHECKED — scan returned upstream={len(up)} ours={len(ours)}, "
          "which cannot be right. Refusing to report buckets from a broken read.")
    sys.exit(0)

# BOTH sides at HEAD. `ours` comes from `git show HEAD:...` above, so reading INTEROP_TOOLS from the
# WORKTREE made the two halves disagree while a change was uncommitted: a tool implemented and
# advertised in the working tree showed up as advertised AND never-ported, and the #353 join below
# then reported it UNCLASSIFIED — a false alarm about a tool that is in the profile. One source, and
# the caveat printed, rather than two sources that agree only after a commit.
src = git("show", "HEAD:crates/iris-agentic-dev-core/src/tools/mod.rs")
# Anchored on the DEFINITION. The old pattern was `INTEROP_TOOLS[^=]*=` and #531 added a
# `keep_list()` accessor naming the const earlier in the file; re.search recovered by advancing to
# the real definition, but only by luck — the equivalent parse in
# tests/tools_excluded_is_classified.rs was NOT so lucky and silently read the neighbouring list.
def keep_list(name):
    m = re.search(r"pub const %s: &\[&str\] = &\[(.*?)\n\];" % name, src, re.S)
    if not m:
        print(f"GATE FAILED: {name} is not defined in mod.rs as `pub const {name}: &[&str] = &[`.")
        sys.exit(1)
    return set(re.findall(r'"([a-z_][a-z_0-9]*)"', m.group(1)))

profile = keep_list("INTEROP_TOOLS")

advertised_upstream = sorted(profile & up)
fork_local          = sorted(profile - up)
pruned              = sorted((ours & up) - profile)
absent              = sorted(up - ours)

print()
print("UPSTREAM SURFACE (#353)   — compared at HEAD; uncommitted changes are not included")
print(f"  upstream implemented          {len(up)}")
print(f"  ours implemented              {len(ours)}")
print(f"  ours advertised               {len(profile)}  = {len(advertised_upstream)} upstream "
      f"+ {len(fork_local)} fork-local")
print(f"  PRUNED   (ported, not advertised, adopting = one line)   {len(pruned)}")
print(f"  ABSENT   (not ported, adopting = a port)                 {len(absent)}")
print(f"  arithmetic: {len(advertised_upstream)} + {len(pruned)} + {len(absent)} = "
      f"{len(advertised_upstream)+len(pruned)+len(absent)} (upstream total {len(up)})")
if fork_local:
    print(f"  fork-local: {', '.join(fork_local)}")
print(f"  pruned: {', '.join(pruned)}")
print(f"  absent: {', '.join(absent)}")

# ── #353: every upstream-only tool must carry a RECORDED REASON ────────────────────────
#
# The buckets above are mechanical — what we ported and pruned versus never ported. They say nothing
# about WHY, and "why" is the whole difference between a profile and an omission. tools-excluded.json
# records the judgement; this joins the two and refuses an unclassified tool.
#
# EXIT 1 here, unlike the not-fetched and broken-scan states above, because reaching this point means
# both trees were read successfully: an unclassified tool is then a real gap, not a missing remote.
# CI does not fetch `upstream`, so in CI this section never runs and cannot redden a PR for a tool
# someone else added upstream — it is a maintainer gate. The half CI DOES enforce (the file parses,
# every category is known, every reason is non-empty, nothing is both advertised and excluded) is a
# Rust test, tests/tools_excluded_is_classified.rs, which runs in the required gate.
import json
EXCL = f"{ROOT}/tools-excluded.json"
try:
    doc = json.load(open(EXCL))
except FileNotFoundError:
    print()
    print(f"GATE FAILED: {EXCL} is missing, so no upstream-only tool has a recorded reason (#353).")
    sys.exit(1)
except json.JSONDecodeError as e:
    print()
    print(f"GATE FAILED: tools-excluded.json does not parse: {e}")
    sys.exit(1)

classified = set(doc.get("tools", {}))
upstream_only = set(pruned) | set(absent)
unclassified = sorted(upstream_only - classified)
stale = sorted(classified - upstream_only)
# #531: a tool may be advertised by one profile and excluded by another, so "contradictory" is a
# question per profile. An entry may say so with a `profiles.<name>.category = "advertised"`
# override; without one, advertising a tool that carries an exclusion reason is a contradiction.
declared = [k for k in doc.get("profiles", {}) if not k.startswith("$")]
if len(declared) < 2:
    print("GATE FAILED: tools-excluded.json must declare its profiles (#531); found "
          f"{declared!r}. Without them this gate checks one profile and reports on all.")
    sys.exit(1)
contradictory = {}
for prof in declared:
    adv = profile if prof == "interop" else keep_list(f"{prof.upper()}_TOOLS")
    offenders = []
    for t in sorted(classified & adv):
        verdict = (doc["tools"][t].get("profiles", {}).get(prof, {}) or {}).get("category")
        if verdict != "advertised":
            offenders.append(f"{t} ({verdict or doc['tools'][t].get('category')})")
    if offenders:
        contradictory[prof] = offenders
also_advertised = sorted(classified & profile)

print()
print("EXCLUSION RATIONALE (#353)")
print(f"  upstream-only tools           {len(upstream_only)}")
print(f"  carrying a recorded reason    {len(upstream_only & classified)}")
bad = 0
if unclassified:
    bad += len(unclassified)
    print(f"  UNCLASSIFIED: {', '.join(unclassified)}")
    print("    Add each to tools-excluded.json with a category and a one-line reason. An upstream")
    print("    tool with no entry is an omission that looks like a decision.")
if stale:
    bad += len(stale)
    print(f"  STALE (classified, but upstream no longer has it): {', '.join(stale)}")
    print("    Remove the entry: a reason for a tool that does not exist is a claim about nothing.")
for prof, offenders in contradictory.items():
    bad += len(offenders)
    print(f"  CONTRADICTORY in `{prof}` (advertised AND excluded): {', '.join(offenders)}")
    print(f"    A tool in {prof.upper()}_TOOLS must not carry an exclusion reason for that")
    print(f"    profile. Either drop it from the keep-list, or add a `profiles.{prof}` override")
    print("    with category `advertised` and the reason it belongs there.")
print(f"  profiles checked              {', '.join(declared)}")
if bad:
    print(f"GATE FAILED: {bad} unexplained or contradictory tool{'' if bad == 1 else 's'} above.")
    sys.exit(1)
print("  every upstream-only tool carries a reason, and no reason is stale or contradictory.")
PY
