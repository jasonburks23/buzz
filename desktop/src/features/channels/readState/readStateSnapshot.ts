import { nip44DecryptFromSelf } from "@/shared/api/tauri";
import { nip44DecryptFromPeer } from "@/shared/api/nip44Peer";
import type { RelayEvent } from "@/shared/api/types";
import {
  isValidBlob,
  isValidReadStateDTag,
  sanitizeContexts,
  type ReadStateBlob,
} from "@/features/channels/readState/readStateFormat";

export type ReadStateDecrypt = (ciphertext: string) => Promise<string>;

/** Suffix that distinguishes a seat's OPERATOR-addressed read-state copy from
 * its self copy. See #383: the clerk publishes `read-state:<slot>:op` encrypted
 * to the operator, alongside the self copy `read-state:<slot>`. */
export const OPERATOR_D_TAG_SUFFIX = ":op";

export type ParsedReadStateEvent = {
  dTag: string;
  blob: ReadStateBlob;
  createdAt: number;
};

export async function parseReadStateEvent(
  event: RelayEvent,
  pubkey: string,
  decrypt: ReadStateDecrypt = nip44DecryptFromSelf,
): Promise<ParsedReadStateEvent | null> {
  if (event.pubkey !== pubkey) return null;

  const dTags = event.tags.filter((tag) => tag[0] === "d");
  if (dTags.length !== 1) return null;
  const dTag = dTags[0]?.[1];
  if (!isValidReadStateDTag(dTag)) return null;

  const tTags = event.tags.filter(
    (tag) => tag[0] === "t" && tag[1] === "read-state",
  );
  if (tTags.length !== 1) return null;

  return decryptAndBuild(event, dTag, decrypt);
}

/**
 * Parse a seat's OPERATOR-addressed read-state copy (#383). Unlike
 * {@link parseReadStateEvent}, this does NOT require `event.pubkey === self`:
 * the author is the SEAT, and the operator decrypts with
 * `ECDH(operator_seckey, seat_pubkey)`. Only events whose `d` tag ends with
 * `:op` are accepted (relay tag filters can't wildcard, so the self copies —
 * which the operator can't decrypt anyway — are dropped client-side here).
 *
 * `operatorPubkey` is the operator's own pubkey; an event the operator authored
 * itself is never a peer seat copy and is skipped. Fail-soft: any decrypt/parse
 * error returns null and never throws into the subscription.
 */
export async function parseOperatorReadStateEvent(
  event: RelayEvent,
  operatorPubkey: string,
  decrypt: ReadStateDecrypt = (ciphertext) =>
    nip44DecryptFromPeer(ciphertext, event.pubkey),
): Promise<ParsedReadStateEvent | null> {
  // The operator's own events are self copies, not peer seat copies.
  if (event.pubkey === operatorPubkey) return null;

  const dTags = event.tags.filter((tag) => tag[0] === "d");
  if (dTags.length !== 1) return null;
  const dTag = dTags[0]?.[1];
  if (!dTag || !dTag.endsWith(OPERATOR_D_TAG_SUFFIX)) return null;
  // Strip the `:op` suffix and validate the underlying read-state d-tag shape.
  const baseDTag = dTag.slice(0, -OPERATOR_D_TAG_SUFFIX.length);
  if (!isValidReadStateDTag(baseDTag)) return null;

  const tTags = event.tags.filter(
    (tag) => tag[0] === "t" && tag[1] === "read-state",
  );
  if (tTags.length !== 1) return null;

  return decryptAndBuild(event, dTag, decrypt);
}

/** Shared decrypt + validate + build tail for both self and operator parse
 * paths. Fail-soft: returns null (never throws) on decrypt or JSON error. */
async function decryptAndBuild(
  event: RelayEvent,
  dTag: string,
  decrypt: ReadStateDecrypt,
): Promise<ParsedReadStateEvent | null> {
  try {
    const plaintext = await decrypt(event.content);
    const parsed = JSON.parse(plaintext);
    if (!isValidBlob(parsed)) return null;
    return {
      dTag,
      blob: {
        v: 1,
        client_id: parsed.client_id,
        contexts: sanitizeContexts(parsed.contexts),
      },
      createdAt: event.created_at,
    };
  } catch (error) {
    console.debug(
      `[ReadStateSnapshot] decrypt/parse failed event=${event.id.substring(0, 8)}…:`,
      error,
    );
    return null;
  }
}

export async function mergeReadStateEvents(
  events: RelayEvent[],
  pubkey: string,
  decrypt?: ReadStateDecrypt,
): Promise<Map<string, number>> {
  const contexts = new Map<string, number>();

  for (const event of events) {
    const parsed = await parseReadStateEvent(event, pubkey, decrypt);
    if (!parsed) continue;

    for (const [contextId, timestamp] of Object.entries(parsed.blob.contexts)) {
      const current = contexts.get(contextId) ?? 0;
      if (timestamp > current) {
        contexts.set(contextId, timestamp);
      }
    }
  }

  return contexts;
}

export function getSnapshotReadTimestamp(
  contexts: ReadonlyMap<string, number>,
  contextId: string,
): number | null {
  return contexts.get(contextId) ?? null;
}
