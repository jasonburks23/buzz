#!/usr/bin/env bash
# comms-orch#24 item 4: install the buzz-seat-clerk release binary at a stable path outside any
# git worktree, so the fleet's launch scripts (tab-clerk-*.sh, launch-clerk.sh) stop pointing at a
# specific worktree's target/debug/clerk -- a worktree can be removed or rebuilt out from under a
# running clerk. Records a checksum alongside the binary so a stale/corrupt install can be detected
# rather than silently run.
#
# Usage: scripts/install-clerk.sh [install_dir]
#   install_dir defaults to $CLERK_INSTALL_DIR or ~/.local/agencyos/bin
#
# Idempotent: safe to re-run. Re-running after no source change reinstalls the same bytes (same
# checksum); re-running after a source change replaces the binary and checksum atomically.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${1:-${CLERK_INSTALL_DIR:-$HOME/.local/agencyos/bin}}"
BIN_NAME="clerk"

cd "$REPO_ROOT"
echo "[install-clerk] building release binary from $REPO_ROOT ..."
cargo build --release -p buzz-seat-clerk --bin clerk

BUILT_BIN="$REPO_ROOT/target/release/clerk"
if [ ! -f "$BUILT_BIN" ]; then
  echo "[install-clerk] ERROR: expected release binary not found at $BUILT_BIN" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"

SHA256=$(shasum -a 256 "$BUILT_BIN" | awk '{print $1}')
SOURCE_SHA=$(git -C "$REPO_ROOT" rev-parse HEAD)
DIRTY=""
git -C "$REPO_ROOT" diff --quiet -- crates/buzz-seat-clerk || DIRTY=" (dirty: uncommitted changes under crates/buzz-seat-clerk)"

# Install atomically: write to a temp file in the SAME directory (same filesystem, so the final
# rename is atomic), then rename over the target. A reader (a clerk process starting mid-install)
# either sees the old complete binary or the new complete binary, never a partial write.
TMP_BIN="$INSTALL_DIR/.${BIN_NAME}.tmp.$$"
cp "$BUILT_BIN" "$TMP_BIN"
chmod +x "$TMP_BIN"
mv -f "$TMP_BIN" "$INSTALL_DIR/$BIN_NAME"

CHECKSUM_FILE="$INSTALL_DIR/$BIN_NAME.sha256"
TMP_CHECKSUM="$INSTALL_DIR/.${BIN_NAME}.sha256.tmp.$$"
{
  echo "sha256: $SHA256"
  echo "source_commit: $SOURCE_SHA$DIRTY"
  echo "installed_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$TMP_CHECKSUM"
mv -f "$TMP_CHECKSUM" "$CHECKSUM_FILE"

echo "[install-clerk] installed $INSTALL_DIR/$BIN_NAME"
echo "[install-clerk] sha256: $SHA256"
echo "[install-clerk] source_commit: $SOURCE_SHA$DIRTY"
