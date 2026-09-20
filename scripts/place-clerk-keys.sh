#!/usr/bin/env bash
# scripts/place-clerk-keys.sh
# opeff#1227: a launchd-started process is refused under Documents on this host (Operation not
# permitted) -- generate-clerk-launchd.sh's wrapper sourced agency-os/.env.local straight from
# there and every clerk under launchd crash-looped. This script is the one place that reads the
# canonical env file and writes each live seat's key out to its own mode-600 header file under
# $HOME/.local/agencyos/keys, outside any Documents path and outside any repo. The wrapper
# (clerk-launchd-lib.sh's render_clerk_wrapper_script) sources only that header file.
#
# Overwatch ruling 2026-09-20 19:22Z on opeff#189: agency-os/.env.local stays the single canonical
# source; the per-seat file is generated FROM it, pulled by the registry's buzzKeyEnvVar name,
# never typed, never shown, and regenerating it is the only way to change it.
#
# Dry run is the default: prints the seat alias and the target path it would write, never the
# value. --live writes the header files. In every mode, no key value ever reaches stdout or
# stderr -- a run whose own output contained a fixture's secret value would defeat the point of
# this script existing.
#
# Usage:
#   scripts/place-clerk-keys.sh          # dry run, all seats found in the registry
#   scripts/place-clerk-keys.sh --live   # write the header files
#
#   SEAT_REGISTRY_PATH overrides the registry location (same var generate-clerk-launchd.sh reads).
#   CLERK_KEYS_DIR overrides the header directory (default: $HOME/.local/agencyos/keys) -- so a
#   test can point this at a scratch HOME without touching the real one.
set -euo pipefail

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES

LIVE=0
for arg in "$@"; do
  case "$arg" in
    --live) LIVE=1 ;;
    *)
      echo "place-clerk-keys: unknown argument '$arg'" >&2
      exit 1
      ;;
  esac
done

REG="${SEAT_REGISTRY_PATH:-/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json}"
KEYS_DIR="${CLERK_KEYS_DIR:-$HOME/.local/agencyos/keys}"

if [ ! -f "$REG" ]; then
  echo "place-clerk-keys: no registry at $REG (set SEAT_REGISTRY_PATH)" >&2
  exit 1
fi

ENVLOCAL=$(python3 -c "import json;d=json.load(open('$REG'));print(d['fleet_boot']['envLocal'])")
if [ ! -f "$ENVLOCAL" ]; then
  echo "place-clerk-keys: no canonical env file at $ENVLOCAL" >&2
  exit 1
fi

# One row per alias with a buzzKeyEnvVar set, whether or not the row currently has channels --
# placement is about "does this seat have a key to vend," not "is it bootable right now."
rows(){
  python3 - "$REG" <<'PY'
import json,sys
reg=json.load(open(sys.argv[1]))
def find_rows(o):
    if isinstance(o,list) and o and isinstance(o[0],dict) and any('tabName' in x for x in o): return o
    if isinstance(o,dict):
        for v in o.values():
            r=find_rows(v)
            if r: return r
    return None
for r in (find_rows(reg) or []):
    alias = r.get('alias')
    kv = r.get('buzzKeyEnvVar')
    if not alias or not kv:
        continue
    print("%s\t%s" % (alias, kv))
PY
}

# Extracts a variable's value from the env file by name, never printing it -- the caller only
# ever tests this function's exit status or writes its stdout straight to a mode-600 file, it
# never echoes the return value itself.
read_key_value(){
  local keyvar="$1"
  python3 - "$ENVLOCAL" "$keyvar" <<'PY'
import re,sys
path,keyvar=sys.argv[1:3]
text=open(path).read()
m=re.search(r'(?m)^\s*(?:export\s+)?%s=(.*)$'%re.escape(keyvar),text)
if not m:
    sys.exit(1)
val=m.group(1).strip().strip('"').strip("'")
if not val:
    sys.exit(1)
sys.stdout.write(val)
PY
}

WROTE=0
REFUSED=0
while IFS=$'\t' read -r alias keyvar; do
  [ -z "$alias" ] && continue
  target="$KEYS_DIR/$alias.env"

  if ! value=$(read_key_value "$keyvar"); then
    echo "place-clerk-keys: REFUSE $alias -- $ENVLOCAL has no value for $keyvar" >&2
    REFUSED=$((REFUSED + 1))
    continue
  fi

  if [ "$LIVE" -eq 1 ]; then
    mkdir -p "$KEYS_DIR"
    chmod 700 "$KEYS_DIR"
    umask 077
    printf '%s=%s\n' "$keyvar" "$value" > "$target"
    chmod 600 "$target"
    echo "place-clerk-keys: wrote $target ($keyvar)"
  else
    echo "place-clerk-keys: (dry run) would write $target ($keyvar)"
  fi
  WROTE=$((WROTE + 1))
done < <(rows)

echo
if [ "$LIVE" -eq 1 ]; then
  echo "place-clerk-keys: $WROTE header file(s) written to $KEYS_DIR ($REFUSED refused: no value in $ENVLOCAL)"
else
  echo "place-clerk-keys: dry run -- $WROTE header file(s) would be written to $KEYS_DIR ($REFUSED refused: no value in $ENVLOCAL). Re-run with --live to write."
fi

if [ "$WROTE" -eq 0 ]; then
  exit 1
fi
