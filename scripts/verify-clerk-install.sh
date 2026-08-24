#!/usr/bin/env bash
# comms-orch#24 item 4: verify an installed clerk binary's checksum file actually matches the
# binary bytes on disk. A checksum recorded at install time is only useful if something reads it
# back later -- this is that something. Exits non-zero and prints a specific reason on any mismatch
# (missing binary, missing checksum file, malformed checksum file, or a real hash mismatch), never
# silently passes.
#
# Usage: scripts/verify-clerk-install.sh [install_dir]
set -euo pipefail

INSTALL_DIR="${1:-${CLERK_INSTALL_DIR:-$HOME/.local/agencyos/bin}}"
BIN_NAME="clerk"
BIN_PATH="$INSTALL_DIR/$BIN_NAME"
CHECKSUM_FILE="$BIN_PATH.sha256"

if [ ! -f "$BIN_PATH" ]; then
  echo "[verify-clerk-install] FAIL: no binary at $BIN_PATH" >&2
  exit 1
fi

if [ ! -f "$CHECKSUM_FILE" ]; then
  echo "[verify-clerk-install] FAIL: no checksum file at $CHECKSUM_FILE" >&2
  exit 1
fi

RECORDED_SHA=$(awk -F': ' '/^sha256:/ { print $2 }' "$CHECKSUM_FILE")
if [ -z "$RECORDED_SHA" ]; then
  echo "[verify-clerk-install] FAIL: checksum file $CHECKSUM_FILE has no sha256 line" >&2
  exit 1
fi

ACTUAL_SHA=$(shasum -a 256 "$BIN_PATH" | awk '{print $1}')
if [ "$ACTUAL_SHA" != "$RECORDED_SHA" ]; then
  echo "[verify-clerk-install] FAIL: checksum mismatch for $BIN_PATH" >&2
  echo "  recorded: $RECORDED_SHA" >&2
  echo "  actual:   $ACTUAL_SHA" >&2
  exit 1
fi

echo "[verify-clerk-install] OK: $BIN_PATH matches recorded checksum ($ACTUAL_SHA)"
