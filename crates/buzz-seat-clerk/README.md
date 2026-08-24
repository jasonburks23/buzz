# buzz-seat-clerk

Headless, always-on, dumb delivery clerk for one Buzz seat.

## What it is

`buzz-seat-clerk` is a small background process that connects to a Buzz relay on
behalf of a single Nostr seat identity. It does three things and nothing more:

1. Connects to the relay and discovers every room the seat belongs to.
2. Subscribes to those rooms and delivers every message to a local in-memory
   mailbox. It writes a kind:30078 read-state bookmark (NIP-44 encrypted to self)
   so the relay tracks what the seat has seen.
3. Badges rooms with unread counts and emits a local wake signal (a file write)
   when a Lane-1 message arrives.

It NEVER answers. It NEVER injects keystrokes. It is a delivery clerk, not a
brain.

### Attention lanes

| Lane | Condition | Wake signal? |
|------|-----------|-------------|
| 1 (ForMe) | DM channel OR @mention (p-tag == seat pubkey) | Yes |
| 2/3 (Delivery) | All other messages | No |

Lane-2 and Lane-3 messages are delivered and badged the same way. The only
difference is that Lane-1 triggers the wake file so a supervisor or higher-level
agent can react.

## Configuration

All config is read from environment variables at startup.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SEAT_NSEC` | Yes | -- | bech32 nsec of the seat identity key. Never commit this value. |
| `RELAY_URL` | Yes | -- | WebSocket URL of the Buzz relay (e.g. `ws://localhost:3000`). |
| `WAKE_FILE` | No | `/tmp/buzz-seat-clerk.wake` | Path the clerk writes a unix timestamp to on Lane-1 mail. |
| `IDENTITY_FILE` | No | `/tmp/buzz-seat-clerk-identity.json` | Path for the durable slot/client_id identity JSON. |

## How to run

```bash
export SEAT_NSEC=nsec1...
export RELAY_URL=ws://localhost:3000
cargo run -p buzz-seat-clerk
```

The clerk runs until killed. It reconnects automatically on relay disconnect
using a bounded exponential backoff (max 64 s).

## Running the integration tests

The four `#[ignore]` integration tests require a live relay. Use the compose
file in the repo root:

```bash
docker compose -f support/docker-compose.test.yml up -d
cargo test -p buzz-seat-clerk -- --include-ignored
docker compose -f support/docker-compose.test.yml down
```

Unit tests (49 total) run without any infrastructure:

```bash
cargo test -p buzz-seat-clerk
```

## Supervision

The `support/` directory contains two process-supervision templates:

- `com.civilization.buzz-seat-clerk.plist` (macOS launchd, single seat)
- `buzz-seat-clerk.service` (Linux systemd)

For a single seat, edit the template plist directly. Replace
`REPLACE_WITH_YOUR_INSTALLED_CLERK_PATH` with wherever your build actually put
the binary, and `REPLACE_WITH_BECH32_NSEC` with the seat secret loaded from
KeychainAccess or a secrets manager -- never commit or hardcode the actual
nsec value. Then:

```
cp support/com.civilization.buzz-seat-clerk.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.civilization.buzz-seat-clerk.plist
```

**For several seats** (comms-orch#18): each clerk carries its own identity via
environment, so N seats means N launchd jobs, never one shared job. Hand-editing
N copies of the template does not scale and risks a copy-paste identity leak.
`scripts/generate-clerk-launchd.sh` generates one correctly-scoped plist +
wrapper script per seat from a fleet registry (`SEAT_REGISTRY_PATH`), pointing
at the canonical installed binary (`scripts/install-clerk.sh`'s
`~/.local/agencyos/bin/clerk`), and never writes a secret value into any
generated file -- only the source seat's env-var *name*, resolved at the
wrapper's own run time. It only writes files; loading the generated plists
into `~/Library/LaunchAgents` and running `launchctl bootstrap` is a separate,
deliberate operator step (see the script's own trailing instructions).

For systemd, copy the env template to `/etc/buzz-seat-clerk/env`, populate
`SEAT_NSEC` and `RELAY_URL` there, then:

```
systemctl --user enable buzz-seat-clerk.service
systemctl --user start  buzz-seat-clerk.service
```

## Design divergences to disclose upstream

These notes are for maintainers reviewing a potential upstream contribution.
We are not filing upstream now; this section records what would need discussion.

**(a) Code home is a new crate, not an `examples/` bot.**
The clerk lives in `crates/buzz-seat-clerk` rather than alongside the existing
`countdown-bot` example. This was a deliberate choice: the clerk is a
production-grade daemon (supervision units, durable identity, reconnect loop)
and belongs in the crate tree, not the examples directory.

**(b) Uses `buzz-ws-client::NostrWsConnection` instead of raw tokio-tungstenite.**
The upstream `countdown-bot` example opens a raw `tokio-tungstenite` WebSocket
and handles NIP-42 AUTH by hand. This crate uses `buzz-ws-client::NostrWsConnection`,
which encapsulates the NIP-42 client authentication handshake. This dogfoods
the client crate and keeps the clerk's connection module small. Upstream
maintainers may prefer the raw approach for examples; either works.

**(c) The 3-lane attention policy is our addition.**
Buzz-ACP uses a binary match/drop model: a message either matches a filter or
it does not. This clerk classifies every delivered message into one of three
lanes (ForMe vs. Delivery) and only emits a wake signal on Lane-1. That policy
layer sits entirely above dumb delivery and is not present in any upstream Buzz
component.

**(d) The headless read-state writer ports the desktop's kind-30078 format.**
The desktop client writes kind:30078 read-state events from a browser context
with localStorage for the slot/client_id identity. This crate replicates that
format for a headless, non-browser client using a JSON file on disk for
identity persistence. The wire format (v1 envelope, NIP-44 encrypt-to-self,
d-tag relay conformance) matches the desktop closely enough to share a relay,
but the implementation is a fresh Rust port.

## Forward requirement: Hermes supervisor evaluation

The launchd / systemd unit in `support/` is the v1 supervision solution.

REQUIRED FOLLOW-UP: when the Hermes agent-gateway integration is evaluated
(Buzz epic #216, Hermes T6 LongRunningRoles #262), evaluate whether Hermes
supervision replaces the launchd solution. If Hermes takes over, wind down
the launchd plist. Do not orphan the old solution.

This forward requirement is tracked as an acceptance line on the clerk-build
ticket, not as a separate ticket (operator decision 2026-08-14).
