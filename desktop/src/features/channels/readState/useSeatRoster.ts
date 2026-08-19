import * as React from "react";
import type { Channel } from "@/shared/api/types";

/**
 * Push the #383 seat roster (deduped union of channel participant pubkeys) into
 * the ReadStateManager once read-state is ready. The manager drops the
 * operator's own pubkey and subscribes to each seat's operator-addressed
 * read-state copy, so the badge clears when EITHER the operator or the seat has
 * read up to a point. Keyed on the sorted deduped string so a new `channels`
 * array identity with unchanged membership is a no-op.
 */
export function useSeatRoster(
  channels: Channel[],
  isReadStateReady: boolean,
  setSeatRoster: (pubkeys: string[]) => void,
): void {
  const seatRosterKey = React.useMemo(
    () =>
      [...new Set(channels.flatMap((c) => c.participantPubkeys))]
        .sort()
        .join(","),
    [channels],
  );
  React.useEffect(() => {
    if (!isReadStateReady) return;
    setSeatRoster(seatRosterKey ? seatRosterKey.split(",") : []);
  }, [seatRosterKey, isReadStateReady, setSeatRoster]);
}
