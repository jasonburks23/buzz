#!/bin/sh
# Point git at the tracked hooks dir. Must be run once PER CHECKOUT this is
# invoked in (git cannot auto-install hooks for you; that is a git safety
# property, not a gap we can close from inside the repo). It wires the
# checkout it is run in and nothing else: it does not run on clone, and it
# does not reach a checkout nobody runs it in. Fleet hook system (#136) will
# centralize this.
#
# Cwd-independent: resolves the SCRIPT'S OWN repo from $0, not the caller's
# cwd. Mirrors the signal-append.sh git -C "$ROOT" fix (#212).
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$SCRIPT_DIR/.."

# opeff#997 (Overwatch's ruling, both arms measured, not assumed): the value
# written is an ABSOLUTE path, resolved from --git-common-dir, never the bare
# string ".githooks". A relative pin resolves against the checkout ROOT, so
# it is safe from a subdirectory (measured, arms 1/2), but a WORKTREE on a
# branch that predates the .githooks commit has no .githooks of its own --
# git does not fall back per hook type, and that renders as exit 0, no
# output, indistinguishable from "nothing to gate" (measured, arm 3; this is
# the live agency-doc-kit defect reproduced). An absolute pin resolved from
# the shared common-dir always points at the MAIN checkout's .githooks,
# regardless of which worktree is committing, closing that gap.
#
# KNOWN, ACCEPTED TRADEOFF (Overwatch's ruling, on the record): a worktree
# now runs the MAIN checkout's hook version, never its own branch's copy. A
# stale branch can no longer disable the guard (the property we want); the
# cost is a branch that is itself improving a hook does not exercise its own
# copy while it is checked out as a worktree. Smaller than zero hooks. If
# this bites, raise it, do not route around it by reverting to relative.
#
# `-C "$REPO_ROOT"` pins git to the script's own repo for this resolution,
# same cwd-independence property as the rest of this script -- never resolved
# against wherever this process happens to be standing (Overwatch's
# --git-path trap: that flag echoes the PIN FORM, a bare relative string
# under a relative pin, resolved only by luck against a cwd that happened to
# be the checkout root; --path-format=absolute avoids that ambiguity
# entirely by asking git to resolve it, not by resolving it ourselves).
COMMON_DIR="$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir)"
MAIN_ROOT="${COMMON_DIR%/.git}"
ABS_HOOKS_DIR="$MAIN_ROOT/.githooks"

# Defensive, BEFORE writing: confirm .githooks exists at the resolved
# absolute target, not merely near the script.
if [ ! -d "$ABS_HOOKS_DIR" ]; then
  echo "error: .githooks dir not found at $ABS_HOOKS_DIR (resolved from --git-common-dir)" >&2
  exit 1
fi

# opeff#997 G1_BOUNCE, second round: this check used to run AFTER the write,
# so a refusal here still left the bad pin standing, core.hooksPath UNSET
# before the run, set to an unverified path after it. Functionally a wash,
# neither state gates anything, but a failed install then reads as
# CONFIGURED to opeff#996's certifier and to any fleet census, which is the
# exact false-positive this ticket exists to eliminate. Moved BEFORE the
# write instead of adding a rollback: a rollback has to know whether the
# prior value was unset, locally set, or inherited, and restoring the wrong
# one of those three is a new bug in a script whose entire job is not
# leaving misleading config behind. Nothing is written until this check
# passes, so there is nothing to roll back.
#
# `find`, never `ls` with a trailing slash: `ls` with one exits 2 with EMPTY
# stdout on this box, byte-identical to a real empty directory, Overwatch's
# own trap, hit four times in one night. -size +0c closes the empty or
# truncated hook case, an empty file named pre-commit, chmod +x, previously
# verified green while a real commit sailed through ungated, opeff#999's
# passes green on a dead gate shape, in the tool built to stop it.
#
# opeff#997 G1_BOUNCE, third round: repair the mode bits BEFORE the check,
# not after the write. Making a checked-out repo's hooks executable is part
# of what this installer has always done (a 644 hook from a fresh clone is
# the exact case #136 exists to rescue), and checking before repairing turns
# every one of those normal checkouts away. This is safe to do first now in
# a way it was not when the old bare "is anything executable" check existed:
# a bare README made executable used to read as a verified hook. The check
# below tests the file's NAME, not just its mode, so chmod can no longer
# manufacture a false pass; it can only repair a file that would already
# pass on content. Arm B stays pointed at a dir with no valid hook name at
# all, proving chmod rescues nothing it should not; arm D proves a 644 real
# hook gets repaired to 755 and the pin still gets written.
chmod +x "$ABS_HOOKS_DIR"/* "$MAIN_ROOT"/scripts/*.mjs 2>/dev/null || true

# THE BOUNDARY, Overwatch's ruling, stated here so it is not re-litigated:
# every check in this section tests a PROPERTY OF THE FILE, present,
# executable, real name, non-empty. None of them tests BEHAVIOUR. Only a
# real commit against a real gate proves a hook refuses anything, arms 2 and
# 4 above did that by hand, opeff#996's certifier does it as clauses 1 and
# 2, as a permanent measurement, not a one-off. This installer proves
# WIRING. The certifier proves FIRING. Do not add a commit probe here: an
# installer that writes commits into the checkout it is wiring is a worse
# trade than the hole it would close. The word below is "wiring verified",
# not "verified", so the output itself never implies the stronger claim.
FOUND_HOOK="$(find "$ABS_HOOKS_DIR" -maxdepth 1 -type f \( \
  -name pre-commit -o -name pre-push -o -name commit-msg -o \
  -name post-checkout -o -name pre-merge-commit -o -name pre-rebase -o \
  -name post-merge -o -name prepare-commit-msg -o -name post-commit \
  \) -perm -u+x -size +0c 2>/dev/null)"
if [ -z "$FOUND_HOOK" ]; then
  echo "error: $ABS_HOOKS_DIR holds no non-empty, executable file matching a real git hook name (pre-commit, pre-push, commit-msg, etc). Refusing to write core.hooksPath to an unverified path; whatever value core.hooksPath already had is left untouched." >&2
  exit 1
fi

git -C "$REPO_ROOT" config core.hooksPath "$ABS_HOOKS_DIR"

# The read-back answers a DIFFERENT question than the pre-write check above:
# the pre-write check asks whether the target is good, this asks whether the
# store actually took what was just written. They fail independently, so
# both stay, never one substituted for the other. Never trust the shell
# variable just written from; re-read the value from git itself.
INSTALLED_PATH="$(git -C "$REPO_ROOT" config --get core.hooksPath)"
if [ "$INSTALLED_PATH" != "$ABS_HOOKS_DIR" ]; then
  echo "error: wrote core.hooksPath as $ABS_HOOKS_DIR but git now reports $INSTALLED_PATH. The store did not take the write." >&2
  exit 1
fi

echo "installed: core.hooksPath -> $INSTALLED_PATH (wiring verified: $(basename "$(echo "$FOUND_HOOK" | head -1)") is present, non-empty, and executable; review-lint runs on every commit)"
echo "this only wires the checkout it was run in. It does not run on clone, and it does not reach any checkout nobody has run it in yet."

# compact-driver#113 Phase 2 (clear-on-exec, emitter-at-gate): register the
# keep-digest-fresh Stop hook the same way as doorman-token-sidecar.sh, chmod
# it here so it is runnable, but do NOT write it into any settings.json. Actual
# per-seat Stop-hook enablement (which seats run it, in which settings file) is
# Overwatch's lane, not this script's; install-hooks.sh only makes the script
# ready to be pointed at.
if [ -f "$REPO_ROOT/scripts/hooks/keep-digest-fresh.mjs" ]; then
  chmod +x "$REPO_ROOT/scripts/hooks/keep-digest-fresh.mjs" 2>/dev/null || true
  echo "keep-digest-fresh: ready at scripts/hooks/keep-digest-fresh.mjs."
  echo "  To wire it as a Stop hook for an exec seat, add to that seat's settings.json:"
  echo '    "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "node '"$REPO_ROOT"'/scripts/hooks/keep-digest-fresh.mjs" }] }] }'
  echo "  It no-ops silently on any non-exec seat or a repo that has not adopted Phase 2."
fi

# opeff#352: best-effort provision of actionlint so the pre-commit workflow-lint
# gate is REAL on this host, not just fail-open. Never fails onboarding: if the
# download or tools are unavailable, we warn and move on (the hook stays
# fail-open with an install hint until actionlint is present).
if command -v actionlint >/dev/null 2>&1 || [ -x "$HOME/bin/actionlint" ]; then
  echo "actionlint: already present (workflow-lint gate is live)."
elif command -v brew >/dev/null 2>&1; then
  echo "actionlint: installing via brew (best-effort)..."
  brew install actionlint >/dev/null 2>&1 \
    && echo "actionlint: installed via brew." \
    || echo "actionlint: brew install failed; workflow-lint stays fail-open. Install manually from https://github.com/rhysd/actionlint/releases"
elif command -v curl >/dev/null 2>&1 && command -v tar >/dev/null 2>&1; then
  echo "actionlint: installing to ~/bin (best-effort)..."
  _al_ok=0
  _al_tag="$(curl -sL --max-time 10 https://api.github.com/repos/rhysd/actionlint/releases/latest \
    | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
  if [ -n "$_al_tag" ]; then
    _al_ver="${_al_tag#v}"
    _al_os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    _al_arch="$(uname -m)"
    case "$_al_arch" in x86_64) _al_arch=amd64 ;; aarch64) _al_arch=arm64 ;; esac
    _al_tmp="$(mktemp -d)"
    if curl -sL --max-time 30 \
        "https://github.com/rhysd/actionlint/releases/download/${_al_tag}/actionlint_${_al_ver}_${_al_os}_${_al_arch}.tar.gz" \
        -o "$_al_tmp/al.tar.gz" \
      && tar xzf "$_al_tmp/al.tar.gz" -C "$_al_tmp" actionlint 2>/dev/null; then
      mkdir -p "$HOME/bin"
      install -m 0755 "$_al_tmp/actionlint" "$HOME/bin/actionlint" && _al_ok=1
    fi
    rm -rf "$_al_tmp"
  fi
  if [ "$_al_ok" = "1" ]; then
    echo "actionlint: installed to ~/bin (ensure ~/bin is on PATH, or the hook finds it there directly)."
  else
    echo "actionlint: auto-install failed; workflow-lint stays fail-open. Install manually from https://github.com/rhysd/actionlint/releases"
  fi
else
  echo "actionlint: no brew/curl found; workflow-lint stays fail-open. Install manually from https://github.com/rhysd/actionlint/releases"
fi
