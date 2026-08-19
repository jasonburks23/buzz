import assert from "node:assert/strict";
import test from "node:test";

import { parseOperatorReadStateEvent } from "./readStateSnapshot.ts";

// Build a kind:30078 operator-copy event. The clerk publishes these with a
// d-tag suffixed `:op` and the seat as the author (event.pubkey).
function makeOpEvent({
  dTag = "read-state:slot123:op",
  author = "a".repeat(64),
  content = "ciphertext",
  contexts,
} = {}) {
  return {
    id: "e".repeat(64),
    pubkey: author,
    kind: 30078,
    created_at: 1_700_000_000,
    content,
    tags: [
      ["d", dTag],
      ["t", "read-state"],
      ["p", "0".repeat(64)],
    ],
    _contexts: contexts,
  };
}

// A decrypt stub that returns a blob JSON. Ignores the ciphertext and reads
// the contexts we attached to the event for the test.
function decryptStub(event) {
  return async () =>
    JSON.stringify({
      v: 1,
      client_id: "seatclient",
      contexts: event._contexts,
    });
}

const OPERATOR = "f".repeat(64);

// ── operator-copy parse: happy path ───────────────────────────────────────────
test("parseOperatorReadStateEvent_decryptsOpCopy_yieldsContexts", async () => {
  const event = makeOpEvent({ contexts: { "channel-a": 1_700_000_100 } });
  const parsed = await parseOperatorReadStateEvent(
    event,
    OPERATOR,
    decryptStub(event),
  );
  assert.ok(parsed, "op-copy event must parse");
  assert.equal(parsed.dTag, "read-state:slot123:op");
  assert.equal(parsed.createdAt, 1_700_000_000);
  assert.equal(parsed.blob.contexts["channel-a"], 1_700_000_100);
});

// ── self-copy skipped: d-tag does NOT end with :op ─────────────────────────────
test("parseOperatorReadStateEvent_skipsSelfCopy_noOpSuffix", async () => {
  const event = makeOpEvent({
    dTag: "read-state:slot123", // self copy — no :op suffix
    contexts: { "channel-a": 1_700_000_100 },
  });
  let decryptCalled = false;
  const parsed = await parseOperatorReadStateEvent(
    event,
    OPERATOR,
    async () => {
      decryptCalled = true;
      return "{}";
    },
  );
  assert.equal(
    parsed,
    null,
    "non-:op d-tag must be skipped by the operator path",
  );
  assert.equal(decryptCalled, false, "self copy must not even attempt decrypt");
});

// ── decrypt failure is fail-soft (returns null, never throws) ──────────────────
test("parseOperatorReadStateEvent_failSoftOnDecryptError", async () => {
  const event = makeOpEvent({ contexts: { "channel-a": 1 } });
  const parsed = await parseOperatorReadStateEvent(
    event,
    OPERATOR,
    async () => {
      throw new Error("bad shared secret");
    },
  );
  assert.equal(parsed, null, "decrypt failure must return null, not throw");
});

// ── an event the operator authored itself is not a seat copy ───────────────────
test("parseOperatorReadStateEvent_skipsOperatorAuthoredEvent", async () => {
  const event = makeOpEvent({
    author: OPERATOR,
    contexts: { "channel-a": 1 },
  });
  const parsed = await parseOperatorReadStateEvent(
    event,
    OPERATOR,
    decryptStub(event),
  );
  assert.equal(parsed, null, "operator's own event is not a peer seat copy");
});
