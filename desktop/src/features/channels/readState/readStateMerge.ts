/**
 * Pure NIP-RS read-state merge primitives, split out of readStateManager so
 * both the manager and the #383 operator-copy path can share them without a
 * circular import (and to keep readStateManager under its size budget).
 */

export type ApplyRemoteContextResult = "unchanged" | "advanced";

export type ContextParentResolver = (contextId: string) => string | null;

/**
 * NIP-RS Hierarchical Frontier Rule (NIP-RS.md:141-167):
 * `effective(ctx) = max(merged[ctx], effective(parent(ctx)))`.
 *
 * The thread→channel relationship is NOT serialized into the blob
 * (NIP-RS.md:136-139); it is derived from the event graph at evaluation time
 * via `parentResolver`. When the resolver yields no parent (channels, or an
 * unresolvable thread root), the frontier degrades to the context's own merged
 * value alone (NIP-RS.md:165-167). Returns null when the context has never been
 * read and no parent term covers it.
 */
export function resolveEffectiveTimestamp(args: {
  effectiveState: Map<string, number>;
  contextId: string;
  parentResolver: ContextParentResolver | null;
}): number | null {
  const { effectiveState, contextId, parentResolver } = args;
  const own = effectiveState.get(contextId) ?? null;

  const parentId = parentResolver?.(contextId) ?? null;
  if (parentId === null) return own;

  const parent = effectiveState.get(parentId) ?? null;
  if (parent === null) return own;
  if (own === null) return parent;
  return Math.max(own, parent);
}

function resolveRemoteContextTimestamp(args: {
  current: number;
  timestamp: number;
}): { next: number; result: ApplyRemoteContextResult } {
  const next = Math.max(args.current, args.timestamp);
  return {
    next,
    result: next === args.current ? "unchanged" : "advanced",
  };
}

export function applyRemoteContextTimestamp(args: {
  effectiveState: Map<string, number>;
  contextSourceCreatedAt: Map<string, number>;
  contextId: string;
  timestamp: number;
  eventCreatedAt: number;
}): ApplyRemoteContextResult {
  const {
    effectiveState,
    contextSourceCreatedAt,
    contextId,
    timestamp,
    eventCreatedAt,
  } = args;
  const sourceCreatedAt = contextSourceCreatedAt.get(contextId) ?? 0;
  const current = effectiveState.get(contextId) ?? 0;
  const { next, result } = resolveRemoteContextTimestamp({
    current,
    timestamp,
  });

  if (result === "advanced") {
    effectiveState.set(contextId, next);
  }
  if (eventCreatedAt > sourceCreatedAt) {
    contextSourceCreatedAt.set(contextId, eventCreatedAt);
  }
  return result;
}
