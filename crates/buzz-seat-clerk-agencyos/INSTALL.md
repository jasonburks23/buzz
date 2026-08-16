# Buzz Seat Clerk: Operator Installation Guide

How to register a fleet seat for Buzz and start its background clerk process.

## What this does

Each live seat in the fleet gets one background clerk process. The clerk
connects to the Buzz relay, watches for messages directed to that seat, and
writes a wake signal to a local file. The Claude Code session on that seat
reads the wake signal and responds.

## Prerequisites

- macOS 12 or later (launchd is required).
- Rust toolchain installed (`rustup`). The clerk binary is built with cargo.
- Node.js 20 or later (for the generator script).
- The seat's Nostr signing key (bech32 `nsec1...`). This is the private key
  that identifies the seat on the Buzz relay. Keep it secret.

---

## Step 1: Build the clerk binary

From the worktree root at
`/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/buzz-refactor-282`:

```bash
cargo build --release -p buzz-seat-clerk-agencyos
cp target/release/clerk "$HOME/.cargo/bin/clerk"
```

The binary ends up at `~/.cargo/bin/clerk`. Confirm it runs:

```bash
clerk --help 2>&1 | head -5
# or: clerk 2>&1 | head -3  (will exit with a config error; that is expected)
```

---

## Step 2: Add the seat to fleet-seat-registry.json

Open:
`/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json`

Add a row to the `seats` array. Use the columns defined in `_meta.config_columns`.
Set `status: "live"`. All three buzz path columns are optional; leave them out to
use auto-derived defaults.

Example row for a new seat "Overwatch (CC)":

```json
{
  "tabName": "Overwatch (CC)",
  "repoName": "agencyos-overwatch",
  "repoLocation": "/Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-overwatch",
  "role": "Overwatch-CC",
  "kind": "cc",
  "status": "live",
  "model": "claude-sonnet-4-6[1m]",
  "compactModel": "claude-sonnet-4-6[1m]"
}
```

The generator will auto-derive:
- `WAKE_FILE`     -> `/tmp/buzz-clerk-wake-overwatch-cc.json`
- `READACK_FILE`  -> `/tmp/buzz-clerk-readack-overwatch-cc.json`
- `IDENTITY_FILE` -> `/tmp/buzz-clerk-identity-overwatch-cc.json`

If you need custom paths (e.g. the seat shares a machine with another seat
that already uses those paths), add explicit columns:

```json
"buzzWakeFile":     "/tmp/buzz-clerk-wake-overwatch-cc-2.json",
"buzzReadackFile":  "/tmp/buzz-clerk-readack-overwatch-cc-2.json",
"buzzIdentityFile": "/tmp/buzz-clerk-identity-overwatch-cc-2.json"
```

Commit the registry change before continuing.

---

## Step 3: Create the secret env file for the seat

The clerk needs the seat's Nostr signing key (`SEAT_NSEC`). This key is NOT
in the registry. Create a gitignored file in your home directory:

```bash
# Replace nsec1... with the real key for this seat.
echo 'SEAT_NSEC=nsec1...' > ~/.env.seat.overwatch-cc
chmod 600 ~/.env.seat.overwatch-cc
```

The slug (`overwatch-cc`) is the tabName lowercased with non-alphanumeric
characters replaced by hyphens. For "AgencyOS (CC) Alpha" it is `agencyos-cc-alpha`.

**Never commit this file.** It is gitignored by the pattern `.env.seat.*` in
the buzz worktree.

---

## Step 4: Generate the launchd plist

From the worktree root:

```bash
node crates/buzz-seat-clerk-agencyos/bin/generate-clerk-plists.mjs \
  --registry /Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json \
  --bin "$HOME/.cargo/bin/clerk"
```

This writes one `.plist` file per live seat to `~/Library/LaunchAgents/` and
one wrapper shell script per seat to `~/Library/LaunchAgents/buzz-clerk-wrappers/`.

Run with `--dry-run` first to preview output without writing any files.

---

## Step 5: Pre-flight secrets check

Confirm that every live seat has its env file before loading plists:

```bash
node crates/buzz-seat-clerk-agencyos/bin/check-clerk-secrets.mjs \
  --registry /Users/jasonburks/Documents/_AI_/Civilization-Skill-Suite/agencyos-operational-efficiency/etc/fleet-seat-registry.json
```

Expected output for a correctly configured seat:
```
[check-secrets] OK: /Users/you/.env.seat.overwatch-cc
[check-secrets] All secret files present. Safe to load plists.
```

Fix any MISSING or INVALID lines before step 6.

---

## Step 6: Load the clerk plist into launchd

```bash
launchctl load ~/Library/LaunchAgents/com.civilization.buzz-clerk.overwatch-cc.plist
```

To load all generated plists at once:

```bash
for f in ~/Library/LaunchAgents/com.civilization.buzz-clerk.*.plist; do
  launchctl load "$f"
  echo "loaded: $f"
done
```

The clerk starts immediately and restarts automatically on crash or reboot
(`KeepAlive: true`, `ThrottleInterval: 30s`).

---

## Step 7: Verify the clerk is running

```bash
# Check that the launchd service is loaded.
launchctl list | grep buzz-clerk

# Tail the log for the new seat.
tail -f /tmp/buzz-clerk-overwatch-cc.stdout.log

# Confirm the wake file is being touched when a Buzz message arrives.
ls -lt /tmp/buzz-clerk-wake-overwatch-cc.json
```

Expected log line at startup:
```
INFO buzz_seat_clerk_agencyos: starting role=Some("Overwatch-CC") session_id=clerk-<uuid> relay=ws://localhost:3000
```

---

## Removing a seat's clerk

```bash
launchctl unload ~/Library/LaunchAgents/com.civilization.buzz-clerk.overwatch-cc.plist
rm ~/Library/LaunchAgents/com.civilization.buzz-clerk.overwatch-cc.plist
rm ~/Library/LaunchAgents/buzz-clerk-wrappers/run-clerk-overwatch-cc.sh
# Optionally remove the secret env file after confirming the key is archived elsewhere.
# rm ~/.env.seat.overwatch-cc
```

---

## Re-generating after a registry change

When you add, remove, or change a seat row:

1. Run step 4 again (generator overwrites existing plists with fresh values).
2. Unload and reload affected plists:
   ```bash
   launchctl unload ~/Library/LaunchAgents/com.civilization.buzz-clerk.overwatch-cc.plist
   launchctl load  ~/Library/LaunchAgents/com.civilization.buzz-clerk.overwatch-cc.plist
   ```
3. For removed seats: unload the plist, delete it, and delete the wrapper script.

---

## Troubleshooting

| Symptom | Check |
|---|---|
| Clerk exits immediately | `tail /tmp/buzz-clerk-<slug>.stderr.log` - likely missing SEAT_NSEC env file or wrong path |
| Wake file not updated | Confirm the relay is running at the URL in `buzz.relayUrl` in the registry |
| `launchctl load` fails | Confirm the plist XML is valid: `plutil ~/Library/LaunchAgents/com.civilization.buzz-clerk.<slug>.plist` |
| Wrong SEAT_ROLE in logs | Re-run the generator and reload the plist |
| Multiple clerks for same seat | Unload all, delete duplicate plists, regenerate, reload |
