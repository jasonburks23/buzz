import type { RelayEvent } from "@/shared/api/types";
import { KIND_READ_STATE } from "@/shared/constants/kinds";
import { READ_STATE_FETCH_LIMIT } from "@/features/channels/readState/readStateFormat";
import { applyRemoteContextTimestamp } from "@/features/channels/readState/readStateMerge";
import { parseOperatorReadStateEvent } from "@/features/channels/readState/readStateSnapshot";
import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";

/**
 * #383 operator-copy support, extracted from ReadStateManager to keep that file
 * under its size budget. These helpers are pure/near-pure so they unit-test on
 * their own and the manager stays thin subscription plumbing.
 */

/** Dedupe a roster and drop the operator's own pubkey, sorted for a stable
 * identity compare. */
export function normalizeSeatRoster(
  pubkeys: string[],
  selfPubkey: string,
): string[] {
  const next = [...new Set(pubkeys)].filter((pk) => pk !== selfPubkey);
  next.sort();
  return next;
}

/** True when two normalized (deduped, sorted) rosters are identical. */
export function seatRostersEqual(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((pk, i) => pk === b[i]);
}

/** Relay filter for the seat roster's operator-addressed read-state copies.
 * NOTE: no `#d` filter — relay tag filters can't wildcard `:op`, so the
 * client selects `:op` events after decrypt (see parseOperatorReadStateEvent). */
export function operatorSubscriptionFilter(
  seatRoster: string[],
): RelaySubscriptionFilter {
  return {
    kinds: [KIND_READ_STATE],
    authors: seatRoster,
    "#t": ["read-state"],
    limit: READ_STATE_FETCH_LIMIT,
  };
}

/**
 * Parse a seat's operator-addressed copy and max-merge its contexts into the
 * operator's effective read-state maps. Returns the context ids that advanced
 * (for pendingSyncedAdvances); empty when nothing changed or the event isn't a
 * decryptable `:op` copy. Fail-soft: never throws.
 */
export async function mergeOperatorEvent(args: {
  event: RelayEvent;
  operatorPubkey: string;
  effectiveState: Map<string, number>;
  contextSourceCreatedAt: Map<string, number>;
  parse?: (
    event: RelayEvent,
    operatorPubkey: string,
  ) => ReturnType<typeof parseOperatorReadStateEvent>;
}): Promise<{ advanced: string[]; createdAt: number } | null> {
  const parse = args.parse ?? parseOperatorReadStateEvent;
  const parsed = await parse(args.event, args.operatorPubkey);
  if (!parsed) return null;

  const advanced: string[] = [];
  for (const [ctx, ts] of Object.entries(parsed.blob.contexts)) {
    const result = applyRemoteContextTimestamp({
      effectiveState: args.effectiveState,
      contextSourceCreatedAt: args.contextSourceCreatedAt,
      contextId: ctx,
      timestamp: ts,
      eventCreatedAt: parsed.createdAt,
    });
    if (result === "advanced") advanced.push(ctx);
  }
  return { advanced, createdAt: parsed.createdAt };
}
