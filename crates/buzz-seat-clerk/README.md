# buzz-seat-clerk

Headless delivery clerk for one Buzz seat. Subscribes to a Nostr relay, picks
up envelopes addressed to the configured seat key, and writes them to the local
wake file so the seat terminal can process them without staying online.

## Supervision

The `support/` directory contains two process-supervision templates:

- `com.civilization.buzz-seat-clerk.plist` (macOS launchd)
- `buzz-seat-clerk.service` (Linux systemd)

Load the launchd plist with:

```
cp support/com.civilization.buzz-seat-clerk.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.civilization.buzz-seat-clerk.plist
```

Edit the plist before loading. Replace `REPLACE_WITH_BECH32_NSEC` with the
seat secret loaded from KeychainAccess or a secrets manager. Never commit or
hardcode the actual nsec value.

For systemd, copy the env template to `/etc/buzz-seat-clerk/env`, populate
`SEAT_NSEC` and `RELAY_URL` there, then:

```
systemctl --user enable buzz-seat-clerk.service
systemctl --user start  buzz-seat-clerk.service
```

## Forward requirement: Hermes supervisor evaluation

The launchd / systemd unit in `support/` is the v1 supervision solution.

REQUIRED FOLLOW-UP: when the Hermes agent-gateway integration is evaluated
(Buzz epic #216, Hermes T6 LongRunningRoles #262), evaluate whether Hermes
supervision replaces the launchd solution. If Hermes takes over, wind down
the launchd plist. Do not orphan the old solution.

This forward requirement is tracked as an acceptance line on the clerk-build
ticket, not as a separate ticket (operator decision 2026-08-14).
