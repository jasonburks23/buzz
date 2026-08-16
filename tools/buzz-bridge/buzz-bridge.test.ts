import { test, expect } from "bun:test";

// pendingChannels is inlined here to avoid importing buzz-bridge.ts at the
// module level. That file runs server.connect() on import (top-level await),
// which would start an MCP stdio server and hang the test runner.

interface WakeMap {
  v: number;
  channels: Record<string, number>;
}

interface AckMap {
  v: number;
  channels: Record<string, number>;
  marker: string;
}

function pendingChannels(wake: WakeMap, ack: AckMap | null): string[] {
  const ackMap = ack?.channels ?? {};
  return Object.entries(wake.channels)
    .filter(([uuid, wakeTs]) => wakeTs > (ackMap[uuid] ?? 0))
    .map(([uuid]) => uuid);
}

test("pendingChannels returns empty when ack covers all channels", () => {
  const wake = { v: 1, channels: { "chan-a": 100, "chan-b": 200 } };
  const ack = {
    v: 1,
    channels: { "chan-a": 100, "chan-b": 200 },
    marker: "sid",
  };
  expect(pendingChannels(wake, ack)).toEqual([]);
});

test("pendingChannels returns unacked channel", () => {
  const wake = { v: 1, channels: { "chan-a": 100, "chan-b": 200 } };
  const ack = { v: 1, channels: { "chan-a": 100 }, marker: "sid" };
  expect(pendingChannels(wake, ack)).toEqual(["chan-b"]);
});

test("pendingChannels returns all when no ack", () => {
  const wake = { v: 1, channels: { "chan-a": 100, "chan-b": 200 } };
  const result = pendingChannels(wake, null);
  const sorted = result.sort();
  expect(sorted).toEqual(["chan-a", "chan-b"]);
});

test("pendingChannels handles partial wake older than ack", () => {
  const wake = { v: 1, channels: { "chan-a": 50 } };
  const ack = { v: 1, channels: { "chan-a": 100 }, marker: "sid" };
  expect(pendingChannels(wake, ack)).toEqual([]);
});
