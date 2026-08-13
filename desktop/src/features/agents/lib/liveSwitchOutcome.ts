import type { ControlResultFrame } from "@/shared/api/types";

/**
 * Resolve the outcome of a live `switch_model` across one or more channels.
 *
 * A live switch fires a `switch_model` frame per active channel and learns each
 * channel's result asynchronously over the observer relay. Two statuses
 * fail-fast — any single frame rejects the whole pick immediately, without
 * waiting for the other channels or the timeout:
 *   - `unsupported_model` → the target model isn't available for this agent.
 *   - `failure`           → the adapter refused the switch (the session stays
 *                           on its current model).
 * Their causes differ, so they resolve to distinct outcomes (`"unsupported"`
 * vs `"failed"`) the caller can message separately.
 *
 * `sent` is the busy-path PROVISIONAL ack: the switch was delivered to the
 * in-flight turn, but the adapter isn't consulted until the requeued session
 * runs. The real verdict lands later as a `failure` / `unsupported_model` frame,
 * or — on success — as silence (the backend emits no positive confirmation). So
 * `sent` never counts toward success; the subscription stays alive so a later
 * terminal frame can still settle the pick.
 *
 * The remaining statuses — `switched` / `turn_ending` / `no_active_turn` — are
 * terminal success for their channel and must arrive from every EXPECTED channel
 * before resolving `"ok"`. When no terminal frame settles the pick (the common
 * busy-path success case), the fallback timeout resolves `"ok"` — the override
 * still rides the requeued/next session, we just can't confirm it synchronously.
 *
 * Two identity guards keep a stale or replayed frame from settling the wrong
 * pick. The observer relay requests a five-minute replay on reconnect, so an
 * old `control_result` for an earlier switch can re-arrive mid-pick:
 *   - `requestId` — an opaque per-pick correlator the harness echoes on every
 *     frame. Frames without a matching id are ignored, so a replayed result for
 *     a prior operation (which carried a different id, or none) is inert.
 *   - `channelId` — terminal success is counted once per DISTINCT expected
 *     channel, not once per frame. Two copies of one channel's `switched` frame
 *     can no longer satisfy a two-channel pick; the second is a no-op.
 *
 * The counting lives here, isolated from React and the relay so it can be unit
 * tested with synthetic frames and a fake clock. The caller injects the
 * relay subscription, the per-channel sends, and the timeout scheduler.
 */
export async function awaitLiveSwitchOutcome({
  requestId,
  channelIds,
  subscribe,
  sendSwitches,
  scheduleTimeout,
}: {
  /** Opaque per-pick id; frames without this exact id are ignored. */
  requestId: string;
  /** Channels the switch was fired to — the distinct set to await. */
  channelIds: readonly string[];
  /** Register a control-result listener; returns an unsubscribe function. */
  subscribe: (listener: (frame: ControlResultFrame) => void) => () => void;
  /** Fire the per-channel `switch_model` sends. Resolves when all are sent. */
  sendSwitches: () => Promise<void>;
  /** Schedule the no-reply fallback; returns a cancel function. */
  scheduleTimeout: (onTimeout: () => void) => () => void;
}): Promise<"ok" | "unsupported" | "failed"> {
  const expected = new Set(channelIds);
  const settled = new Promise<"ok" | "unsupported" | "failed">((resolve) => {
    let unsubscribe = () => {};
    let cancelTimeout = () => {};
    const succeeded = new Set<string>();
    const finish = (outcome: "ok" | "unsupported" | "failed") => {
      cancelTimeout();
      unsubscribe();
      resolve(outcome);
    };
    cancelTimeout = scheduleTimeout(() => finish("ok"));
    unsubscribe = subscribe((frame) => {
      // requestId scopes every decision to THIS pick — a replayed result for a
      // prior operation carries a different id (or none) and is ignored here.
      if (frame.type !== "switch_model" || frame.requestId !== requestId) {
        return;
      }
      if (frame.status === "unsupported_model") {
        // Model unavailable — reject the whole pick immediately.
        finish("unsupported");
        return;
      }
      if (frame.status === "failure") {
        // Adapter refused the switch — reject immediately. The session stays
        // on its current model; distinct outcome so the caller can say why.
        finish("failed");
        return;
      }
      if (frame.status === "sent") {
        // Busy-path provisional ack: the switch was delivered to the in-flight
        // turn, but the adapter isn't consulted until the requeued session runs.
        // Don't count it — a later `failure`/`unsupported_model` may still
        // settle the pick, and on success the backend stays silent, so the
        // timeout fallback is what confirms.
        return;
      }
      // switched / turn_ending / no_active_turn — terminal success for this
      // channel. Count each expected channel once: a duplicate frame for a
      // channel already recorded (a replay, or a two-copy fan-out) is a no-op,
      // and a frame for a channel we never fired to is ignored.
      if (!frame.channelId || !expected.has(frame.channelId)) {
        return;
      }
      succeeded.add(frame.channelId);
      if (succeeded.size >= expected.size) {
        finish("ok");
      }
    });
  });

  await sendSwitches();

  return settled;
}
