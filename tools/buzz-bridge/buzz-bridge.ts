#!/usr/bin/env bun
// buzz-bridge: Layer 2 Claude Code wake bridge.
// Watches ~/.buzz-clerk/wake.json (per-channel map) and emits
// notifications/claude/channel when Buzz for-me mail is pending.
// Content-free by design: no keys, no message bodies, no read-state.
// Experimental: uses claude/channel, a Claude Code-specific MCP extension.
import { watchFile, statSync, readFileSync, existsSync } from "node:fs";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

const HOME = process.env.HOME ?? "/tmp";
const CLERK_DIR = process.env.BUZZ_CLERK_DIR ?? `${HOME}/.buzz-clerk`;
const WAKE_FILE = process.env.WAKE_FILE ?? `${CLERK_DIR}/wake.json`;
const READACK_FILE = process.env.READACK_FILE ?? `${CLERK_DIR}/readack.json`;
const POLL_MS = Number(process.env.WAKE_POLL_MS ?? "500");
const ACK_WATCH_MS = Number(process.env.ACK_WATCH_MS ?? "30000");
const MAX_RETRIES = 3;

const SIGNAL = "You have Buzz mail. Read your mailbox oldest-first and reply.";

const server = new Server(
  { name: "buzz-bridge", version: "2.0.0" },
  {
    capabilities: { experimental: { "claude/channel": {} } },
    instructions:
      "A Buzz wake signal means: read your for-me mailbox oldest-first and reply. Act immediately.",
  },
);

await server.connect(new StdioServerTransport());

interface WakeMap {
  v: number;
  channels: Record<string, number>;
}

interface AckMap {
  v: number;
  channels: Record<string, number>;
  marker: string;
}

function readWake(): WakeMap | null {
  try {
    if (!existsSync(WAKE_FILE)) return null;
    const raw = JSON.parse(readFileSync(WAKE_FILE, "utf8"));
    if (raw.v !== 1 || typeof raw.channels !== "object") return null;
    return raw as WakeMap;
  } catch {
    return null;
  }
}

function readAck(): AckMap | null {
  try {
    if (!existsSync(READACK_FILE)) return null;
    const raw = JSON.parse(readFileSync(READACK_FILE, "utf8"));
    // New multi-channel format: {"v":1,"channels":{"<uuid>":<ts>},"marker":"<str>"}
    if (raw.v === 1 && typeof raw.channels === "object") {
      return raw as AckMap;
    }
    // Backward compat: old single-channel format {"up_to_ts":<ts>}.
    // Compute lastAckTs as the max across all channel values in the new format;
    // for old files that max is just the single up_to_ts value.
    // We synthesise an AckMap with an empty channels map but expose the scalar
    // via a synthetic "__legacy__" key so pendingChannels keeps working.
    const upToTs = Number(raw.up_to_ts);
    if (Number.isFinite(upToTs) && upToTs > 0) {
      // Return a synthetic AckMap. pendingChannels will not find any channel
      // uuid in this map, so every channel in the wake file appears pending —
      // which is the safe/correct behaviour when we cannot match per-channel.
      // The bridge will fire and let the seat re-read to determine true need.
      return { v: 1, channels: {}, marker: "legacy_up_to_ts" };
    }
    return null;
  } catch {
    return null;
  }
}

function ackMtime(): number {
  try {
    return statSync(READACK_FILE).mtimeMs;
  } catch {
    return 0;
  }
}

export function pendingChannels(wake: WakeMap, ack: AckMap | null): string[] {
  const ackMap = ack?.channels ?? {};
  return Object.entries(wake.channels)
    .filter(([uuid, wakeTs]) => wakeTs > (ackMap[uuid] ?? 0))
    .map(([uuid]) => uuid);
}

async function emitWake(sourceType: string, channels: string[]): Promise<void> {
  await server.notification({
    method: "notifications/claude/channel",
    params: {
      content: SIGNAL,
      meta: { source_type: sourceType, pending_channels: channels },
    },
  });
}

async function waitForAckOrRetry(
  channels: string[],
  retries = 0,
): Promise<void> {
  if (retries >= MAX_RETRIES) return;
  const baseline = ackMtime();
  await new Promise<void>((resolve) => {
    const timer = setTimeout(async () => {
      if (retries < MAX_RETRIES - 1) {
        await emitWake("buzz_wake_resume_retry", channels);
        await waitForAckOrRetry(channels, retries + 1);
      }
      resolve();
    }, ACK_WATCH_MS);
    const poll = setInterval(() => {
      if (ackMtime() > baseline) {
        clearTimeout(timer);
        clearInterval(poll);
        resolve();
      }
    }, POLL_MS);
  });
}

const wake = readWake();
if (wake) {
  const ack = readAck();
  const pending = pendingChannels(wake, ack);
  if (pending.length > 0) {
    await emitWake("buzz_wake_resume", pending);
    void waitForAckOrRetry(pending);
  }
}

let lastWakeMtimeMs = 0;
try {
  lastWakeMtimeMs = statSync(WAKE_FILE).mtimeMs;
} catch {
  /* not yet created */
}
let cachedAck: AckMap | null = readAck();
let lastAckMtimeMs = ackMtime();

watchFile(READACK_FILE, { interval: POLL_MS }, () => {
  const m = ackMtime();
  if (m > lastAckMtimeMs) {
    lastAckMtimeMs = m;
    cachedAck = readAck();
  }
});

watchFile(WAKE_FILE, { interval: POLL_MS }, async (curr) => {
  if (curr.mtimeMs === 0 || curr.mtimeMs === lastWakeMtimeMs) return;
  lastWakeMtimeMs = curr.mtimeMs;
  const w = readWake();
  if (!w) return;
  const pending = pendingChannels(w, cachedAck);
  if (pending.length === 0) return;
  await emitWake("buzz_wake", pending);
});
