#!/usr/bin/env bash
# scripts/load-clerk-launchd.sh
# opeff#1210: the load step scripts/generate-clerk-launchd.sh (comms-orch#18) deliberately left to
# operator hands. That script only WRITES plist+wrapper artifacts into DEPLOY_DIR; it never touches
# ~/Library/LaunchAgents and never calls launchctl. This script picks up from there: given what
# generate-clerk-launchd.sh already wrote, it stops a seat's bare background clerk pid (the
# unsupervised `tab-clerk-<seat>.sh &` shape from before this ticket) by literal number, copies the
# plist into ~/Library/LaunchAgents, and bootstraps it -- so a seat never runs two clerks at once.
#
# Dry run is the default and does nothing but report: for every seat with a generated plist in
# DEPLOY_DIR, its plist path and the bare-pid status read from CLERK_RUN_DIR. It never writes
# anything under HOME and never invokes launchctl, live or otherwise.
#
# Usage:
#   scripts/load-clerk-launchd.sh                 # dry run, all discovered seats
#   scripts/load-clerk-launchd.sh --live <alias>   # stop the bare pid, load that one seat
#   scripts/load-clerk-launchd.sh --live --all     # stop+load every discovered seat
#
#   CLERK_LAUNCHD_DEPLOY_DIR overrides where generated plists/wrappers are read from (same
#   default and env var as generate-clerk-launchd.sh: $HOME/.local/agencyos/launchd).
#   CLERK_RUN_DIR overrides where a seat's bare clerk pid file lives (default: the live
#   agencyos-comms-orchestrator checkout's infra/clerks/run, where tab-clerk-<alias>.sh writes
#   `echo $$ > run/clerk-<alias>.pid`).
#   LAUNCHCTL_BIN overrides the launchctl binary invoked under --live (default: launchctl on
#   PATH) -- so a test can point this at a fake that only logs its args.
set -euo pipefail

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SELF_DIR/lib/clerk-launchd-lib.sh"

DEPLOY_DIR="${CLERK_LAUNCHD_DEPLOY_DIR:-$HOME/.local/agencyos/launchd}"
RUN_DIR="${CLERK_RUN_DIR:-/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/Helper-Skills/agencyos-comms-orchestrator/infra/clerks/run}"
LAUNCHCTL_BIN="${LAUNCHCTL_BIN:-launchctl}"
GUI_DOMAIN="gui/$(id -u)"
STOP_WAIT_SECONDS=5
# opeff#1210 gate-2: a pid on file is a number, not an identity. Before any signal the process
# holding it must be a clerk, judged by its command path, the same way CLERKSTRAY01 judges one.
# CLERK_INSTALL_DIR is the same override generate-clerk-launchd.sh honors.
CLERK_INSTALL_DIR="${CLERK_INSTALL_DIR:-$HOME/.local/agencyos/bin}"
CLERK_BIN_PATH="$CLERK_INSTALL_DIR/clerk"

LIVE=0
TARGET=""
for arg in "$@"; do
  case "$arg" in
    --live) LIVE=1 ;;
    --all) TARGET="--all" ;;
    -*)
      echo "load-clerk-launchd: unknown flag '$arg'" >&2
      exit 1
      ;;
    *) TARGET="$arg" ;;
  esac
done

if [ ! -d "$DEPLOY_DIR" ]; then
  echo "load-clerk-launchd: no deploy dir at $DEPLOY_DIR -- run generate-clerk-launchd.sh first" >&2
  exit 1
fi

# Discoverable purely from what the generator already wrote (comms-orch#18's own naming
# convention), never a second read of the fleet registry -- the registry can move on between
# generate and load, and this step's job is to load exactly what was generated, nothing more.
discover_aliases(){
  local f base
  for f in "$DEPLOY_DIR"/com.civilization.buzz-seat-clerk-*.plist; do
    [ -e "$f" ] || continue
    base="$(basename "$f")"
    printf '%s\n' "$(clerk_alias_from_plist_filename "$base")"
  done
}

bare_pid_status(){ # $1=alias -> prints a one-line human status, never touches the process
  local alias="$1" pid_path pid
  pid_path="$(clerk_run_pid_path "$RUN_DIR" "$alias")"
  if [ ! -f "$pid_path" ]; then
    echo "no bare pid file ($pid_path)"
    return
  fi
  pid="$(cat "$pid_path")"
  if kill -0 "$pid" 2>/dev/null; then
    echo "bare pid $pid running ($pid_path)"
  else
    echo "bare pid $pid in $pid_path is not running"
  fi
}

if [ "$LIVE" -eq 0 ]; then
  ALIASES=$(discover_aliases)
  if [ -z "$ALIASES" ]; then
    echo "load-clerk-launchd: no generated seats found in $DEPLOY_DIR" >&2
    exit 1
  fi
  echo "load-clerk-launchd: DRY RUN (default -- pass --live <alias>|--all to act)"
  while IFS= read -r alias; do
    [ -z "$alias" ] && continue
    plist_path="$DEPLOY_DIR/$(clerk_plist_filename "$alias")"
    status=$(bare_pid_status "$alias")
    echo "  $alias: plist=$plist_path pid_status=[$status]"
  done <<< "$ALIASES"
  exit 0
fi

if [ -z "$TARGET" ]; then
  echo "load-clerk-launchd: --live requires an alias or --all" >&2
  exit 1
fi

if [ "$TARGET" = "--all" ]; then
  TARGETS=$(discover_aliases)
  if [ -z "$TARGETS" ]; then
    echo "load-clerk-launchd: no generated seats found in $DEPLOY_DIR" >&2
    exit 1
  fi
else
  plist_path="$DEPLOY_DIR/$(clerk_plist_filename "$TARGET")"
  if [ ! -f "$plist_path" ]; then
    echo "load-clerk-launchd: no generated plist for seat '$TARGET' at $plist_path -- run generate-clerk-launchd.sh first" >&2
    exit 1
  fi
  TARGETS="$TARGET"
fi

# Stop-before-start (opeff#1210): kill the seat's bare background clerk by the LITERAL pid on
# file, never by name pattern, then wait for it to actually exit before this seat's plist gets
# bootstrapped -- so two clerks are never live for one seat at once, even for the window between
# stop and start.
stop_bare_clerk(){ # $1=alias
  local alias="$1" pid_path pid waited
  pid_path="$(clerk_run_pid_path "$RUN_DIR" "$alias")"
  if [ ! -f "$pid_path" ]; then
    echo "$alias: no bare clerk pid file at $pid_path -- nothing to stop"
    return
  fi
  pid="$(cat "$pid_path")"
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "$alias: stale pid file, pid $pid in $pid_path has no process; clearing it, never signalling"
    : > "$pid_path"
    return
  fi
  # Identity before signal: the command path must be the clerk binary. A pid number can be
  # reused by any process after the clerk that wrote the file exits; a stale file naming an
  # operator process is a refusal, not a target.
  local comm
  comm="$(ps -p "$pid" -o comm= 2>/dev/null)"
  if [ "$comm" != "$CLERK_BIN_PATH" ] && [ "$(basename "$comm")" != "clerk" ]; then
    echo "$alias: REFUSING to signal pid $pid: its command is '$comm', not the clerk binary; fix or empty $pid_path by hand" >&2
    return 3
  fi
  echo "$alias: stopping bare clerk pid $pid ($comm)"
  kill -TERM "$pid"
  waited=0
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$STOP_WAIT_SECONDS" ]; then
      echo "$alias: pid $pid did not exit after ${STOP_WAIT_SECONDS}s, sending SIGKILL"
      kill -KILL "$pid" 2>/dev/null || true
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done
  echo "$alias: bare clerk pid $pid stopped"
}

load_seat(){ # $1=alias
  local alias="$1" plist_path label dest_dir dest_path
  plist_path="$DEPLOY_DIR/$(clerk_plist_filename "$alias")"
  label="$(clerk_plist_label "$alias")"
  dest_dir="$HOME/Library/LaunchAgents"
  dest_path="$dest_dir/$(clerk_plist_filename "$alias")"

  stop_bare_clerk "$alias"

  mkdir -p "$dest_dir"
  cp "$plist_path" "$dest_path"
  echo "$alias: copied $plist_path -> $dest_path"

  "$LAUNCHCTL_BIN" bootstrap "$GUI_DOMAIN" "$dest_path"
  echo "$alias: bootstrapped $label"

  "$LAUNCHCTL_BIN" list "$label"
}

while IFS= read -r alias; do
  [ -z "$alias" ] && continue
  load_seat "$alias"
done <<< "$TARGETS"
