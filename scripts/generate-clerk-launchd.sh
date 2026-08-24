#!/usr/bin/env bash
# scripts/generate-clerk-launchd.sh
# comms-orch#18: generate one launchd plist + wrapper script PER SEAT clerk, from the fleet
# registry, pointing at the CANONICAL installed clerk binary (scripts/install-clerk.sh's
# ~/.local/agencyos/bin/clerk, never a git worktree's target/debug/clerk -- that mismatch is
# exactly what left every live clerk on a two-day-old debug build while the installed release
# binary sat unused, per comms-orch#31).
#
# This script only WRITES files (wrapper + plist) into DEPLOY_DIR. It never runs `launchctl
# bootstrap`/`load`, never touches ~/Library/LaunchAgents, and never starts, stops, or signals any
# live process. Loading a plist is operator hands, per the ticket's explicit constraint -- this
# script's job ends at "here are the artifacts," and it prints the exact operator commands to take
# it from there.
#
# Usage: scripts/generate-clerk-launchd.sh [deploy_dir]
#   deploy_dir defaults to $CLERK_LAUNCHD_DEPLOY_DIR or ~/.local/agencyos/launchd
#   SEAT_REGISTRY_PATH overrides the registry location (same env var relaunch.sh reads).
#   CLERK_INSTALL_DIR overrides the canonical clerk binary's install dir (same var install-clerk.sh
#   reads) -- so a test can point this generator at a fixture binary without touching the real one.
#   CLERK_LOG_DIR overrides where the clerk's own durable per-seat log lives (comms-orch#11 slice B).
set -euo pipefail

# Same REV-20260823-01 class hardening as install-clerk.sh: this script has no legitimate reason
# to inherit an ambient git env, and its own registry/python calls below would silently misbehave
# if they ever did.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SELF_DIR/lib/clerk-launchd-lib.sh"

DEPLOY_DIR="${1:-${CLERK_LAUNCHD_DEPLOY_DIR:-$HOME/.local/agencyos/launchd}}"
REG="${SEAT_REGISTRY_PATH:-/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json}"
CLERK_INSTALL_DIR="${CLERK_INSTALL_DIR:-$HOME/.local/agencyos/bin}"
CLERK_BIN="$CLERK_INSTALL_DIR/clerk"
CLERK_LOG_DIR="${CLERK_LOG_DIR:-/tmp}"

if [ ! -f "$REG" ]; then
  echo "generate-clerk-launchd: no registry at $REG (set SEAT_REGISTRY_PATH)" >&2
  exit 1
fi

# comms-orch#31's lesson, applied as a generation-time preflight (the acceptance test below is
# the REAL proof; this is a cheap, immediate loud-fail so a missing/never-installed binary is
# caught before any plist is written pointing at nothing): refuse to generate artifacts for a
# canonical binary that does not exist. A launchd job pointing at a path with nothing there is
# the exact "landed a file, changed nothing" defect class this ticket exists to close.
if [ ! -x "$CLERK_BIN" ]; then
  echo "generate-clerk-launchd: canonical clerk binary not found or not executable at $CLERK_BIN" >&2
  echo "  run scripts/install-clerk.sh first." >&2
  exit 1
fi

# --- registry reads. Duplicated from agencyos-terminal-driver's relaunch.sh seat_facts /
# bootable_aliases (cross-repo; buzz has no import path into that repo's relaunch-lib.sh) rather
# than reinvented: same field names, same bootable definition (real key present in envLocal + a
# room), so a seat that is bootable for the tab-launch path is bootable here too. ---
ENVLOCAL=$(python3 -c "import json;d=json.load(open('$REG'));print(d['fleet_boot']['envLocal'])")
RELAY=$(python3 -c "import json;d=json.load(open('$REG'));print(d['buzz']['relayUrl'])")
WSRELAY=$(python3 -c "import json;d=json.load(open('$REG'));b=d['buzz'];print(b.get('relayWsUrl') or b['relayUrl'].replace('https:','wss:'))")

bootable_aliases(){
  python3 - "$REG" "$ENVLOCAL" <<'PY'
import json,sys,re
reg=json.load(open(sys.argv[1]))
try: envtext=open(sys.argv[2]).read()
except Exception: envtext=''
def find_rows(o):
    if isinstance(o,list) and o and isinstance(o[0],dict) and any('tabName' in x for x in o): return o
    if isinstance(o,dict):
        for v in o.values():
            r=find_rows(v)
            if r: return r
    return None
def keyok(kv):
    if not kv: return False
    m=re.search(r'(?m)^\s*(?:export\s+)?%s=(.*)$'%re.escape(kv),envtext)
    return bool(m and m.group(1).strip().strip('"').strip("'"))
print(" ".join(r['alias'] for r in (find_rows(reg) or [])
      if r.get('alias') and keyok(r.get('buzzKeyEnvVar')) and r.get('buzzChannels')))
PY
}

seat_facts(){ # $1=alias -> eval-able ROLE/KEYVAR/SESSION/WAKE/READACK/TPROLE
python3 - "$1" "$REG" <<'PY'
import json,sys
alias,regp=sys.argv[1:3]
reg=json.load(open(regp))
def find_rows(o):
    if isinstance(o,list) and o and isinstance(o[0],dict) and any('tabName' in x for x in o): return o
    if isinstance(o,dict):
        for v in o.values():
            r=find_rows(v)
            if r: return r
    return None
row=next((r for r in (find_rows(reg) or []) if r.get('alias')==alias),None)
def g(k,d=''): return (row or {}).get(k,d) or d
tp=g('tpRole')
print("ROLE=%r"%g('role'))
print("KEYVAR=%r"%g('buzzKeyEnvVar'))
print("SESSION=%r"%g('sessionId'))
print("WAKE=%r"%(g('buzzWakeFile') or ('/tmp/buzz-clerk-wake-%s.json'%tp)))
print("READACK=%r"%(g('buzzReadackFile') or ('/tmp/buzz-clerk-readack-%s.json'%tp)))
print("TPROLE=%r"%tp)
PY
}

mkdir -p "$DEPLOY_DIR"

GENERATED=()
for alias in $(bootable_aliases); do
  eval "$(seat_facts "$alias")"
  wrapper_path="$DEPLOY_DIR/$(clerk_wrapper_filename "$alias")"
  plist_path="$DEPLOY_DIR/$(clerk_plist_filename "$alias")"
  label=$(clerk_plist_label "$alias")
  stdout_path=$(clerk_launchd_stdout_path "$CLERK_LOG_DIR" "$alias")
  stderr_path=$(clerk_launchd_stderr_path "$CLERK_LOG_DIR" "$alias")

  render_clerk_wrapper_script "$CLERK_BIN" "$ENVLOCAL" "$KEYVAR" "$WSRELAY" "$ROLE" \
    "$SESSION" "$WAKE" "$READACK" "/tmp" "$CLERK_LOG_DIR" > "$wrapper_path"
  chmod +x "$wrapper_path"

  render_clerk_plist "$label" "$wrapper_path" "$stdout_path" "$stderr_path" > "$plist_path"

  echo "generate-clerk-launchd: wrote $plist_path (wrapper: $wrapper_path)"
  GENERATED+=("$plist_path")
done

if [ "${#GENERATED[@]}" -eq 0 ]; then
  echo "generate-clerk-launchd: no bootable seats found in $REG -- nothing generated" >&2
  exit 1
fi

echo
echo "generate-clerk-launchd: ${#GENERATED[@]} job(s) written to $DEPLOY_DIR"
echo "Next (operator hands -- this script does not do this):"
for p in "${GENERATED[@]}"; do
  b=$(basename "$p")
  echo "  cp '$p' \"\$HOME/Library/LaunchAgents/$b\" && launchctl bootstrap \"gui/\$(id -u)\" \"\$HOME/Library/LaunchAgents/$b\""
done
