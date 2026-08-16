//! Seat identity abstractions.
//!
//! Defines the [`SeatIdentity`] trait that separates "who am I" from
//! "who is the live session right now." The generic implementation,
//! [`EnvIdentity`], trusts the reader: local == live, so the honest-seen
//! gate in [`crate::read_state::record_youyou_read`] always passes.
//! The fleet adapter supplies [`ClaimFileIdentity`] (in buzz-seat-clerk-agencyos)
//! which resolves liveness from `/tmp/claude-seat-claim-*.json` files.

use crate::session_identity::SessionMarker;

/// How a seat learns its own identity and who the live session is.
/// The generic default trusts the reader; the fleet adapter resolves the
/// real live session from claim files (honest-seen).
pub trait SeatIdentity: Send + Sync {
    /// The marker this seat stamps on its own reads (its "I am" id).
    /// None means "unknown"; callers treat unknown as "do not advance bookmarks".
    fn local_marker(&self) -> Option<SessionMarker>;

    /// The marker currently considered the live session for this seat.
    /// None means "cannot determine liveness right now".
    fn live_marker(&self) -> Option<SessionMarker>;
}

/// Generic default: identity comes from config/env; local == live, so the
/// honest-seen gate is a no-op pass. Correct for a single-session user who
/// is always "themselves". No claim files, no fleet assumptions.
pub struct EnvIdentity {
    marker: Option<SessionMarker>,
}

impl EnvIdentity {
    /// Create an EnvIdentity with the given marker.
    ///
    /// Pass `None` when the marker is unknown (callers will skip bookmark
    /// advance). Pass `Some(marker)` to enable gate-passing reads.
    pub fn new(marker: Option<SessionMarker>) -> Self {
        Self { marker }
    }
}

impl SeatIdentity for EnvIdentity {
    fn local_marker(&self) -> Option<SessionMarker> {
        self.marker.clone()
    }

    fn live_marker(&self) -> Option<SessionMarker> {
        self.marker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_identity::SessionMarker;

    fn make_marker(s: &str) -> SessionMarker {
        SessionMarker::new(s.to_string())
    }

    /// EnvIdentity local_marker() and live_marker() must return equal values
    /// when constructed with Some marker. This is the "trusting reader" guarantee:
    /// the gate in record_youyou_read will see local == live and return Ok.
    #[test]
    fn env_identity_local_equals_live() {
        let marker = make_marker("session-abc-123");
        let identity = EnvIdentity::new(Some(marker.clone()));

        let local = identity.local_marker();
        let live = identity.live_marker();

        assert!(local.is_some(), "local_marker should be Some");
        assert!(live.is_some(), "live_marker should be Some");
        assert_eq!(
            local.unwrap().0,
            live.unwrap().0,
            "local and live markers must be identical for EnvIdentity"
        );
    }

    /// EnvIdentity constructed with None must return None for both markers.
    /// Callers that receive None skip bookmark advance entirely.
    #[test]
    fn env_identity_none_marker() {
        let identity = EnvIdentity::new(None);

        assert!(
            identity.local_marker().is_none(),
            "local_marker should be None when constructed with None"
        );
        assert!(
            identity.live_marker().is_none(),
            "live_marker should be None when constructed with None"
        );
    }

    // ── SP-1 Acceptance: gate-integration via injected SeatIdentity ───────────

    /// ACCEPTANCE (trusting path): EnvIdentity local == live, so record_youyou_read
    /// must return Ok(()) and advance the bookmark.
    ///
    /// This is non-vacuous: we assert the bookmark moves from None to Some(ts)
    /// confirming the gate did not refuse.
    #[test]
    fn env_identity_gate_passes_and_advances_bookmark() {
        use crate::read_state::{record_youyou_read, ReadStateWriter, SlotIdentity};

        let marker = make_marker("session-env-001");
        let identity = EnvIdentity::new(Some(marker));

        let local = identity
            .local_marker()
            .expect("EnvIdentity must return Some");
        let live = identity
            .live_marker()
            .expect("EnvIdentity must return Some");

        // Sanity: they must be equal for this test to be meaningful.
        assert_eq!(
            local.0, live.0,
            "EnvIdentity must produce equal local and live markers"
        );

        let slot = SlotIdentity {
            slot_id: crate::read_state::generate_slot_id(),
            client_id: crate::read_state::generate_slot_id(),
        };
        let mut writer = ReadStateWriter::new(slot);

        let ctx = "chan-env-gate-test".to_string();
        let ts = 1_700_000_000u64;

        // Before: bookmark is absent.
        assert_eq!(
            writer.read_at_for(&ctx),
            None,
            "bookmark must be None before gate call"
        );

        let result = record_youyou_read(&mut writer, ctx.clone(), ts, &local, &live);

        assert!(
            result.is_ok(),
            "EnvIdentity gate must pass (Ok); got: {:?}",
            result
        );

        // After: bookmark advanced to ts -- proving the gate passed, not just returned Ok.
        assert_eq!(
            writer.read_at_for(&ctx),
            Some(ts),
            "bookmark must advance to {ts} when gate passes"
        );
    }

    /// ACCEPTANCE (honest-seen, non-trusting): when markers differ, gate refuses
    /// and bookmark stays at None.
    ///
    /// This test does NOT use a SeatIdentity impl -- it directly exercises the
    /// record_youyou_read gate with a mismatched pair to prove the gate logic
    /// is the single refusal point. The ClaimFileIdentity integration test
    /// (tests/honest_seen.rs in buzz-seat-clerk-agencyos) confirms the same gate
    /// fires through the fleet adapter.
    #[test]
    fn mismatched_markers_gate_refuses_and_bookmark_stays_none() {
        use crate::read_state::{
            record_youyou_read, ReadGuardError, ReadStateWriter, SlotIdentity,
        };

        let slot = SlotIdentity {
            slot_id: crate::read_state::generate_slot_id(),
            client_id: crate::read_state::generate_slot_id(),
        };
        let mut writer = ReadStateWriter::new(slot);

        let live_marker = make_marker("live-session-xyz");
        let imposter_marker = make_marker("imposter-session-abc");
        let ctx = "chan-refusal-test".to_string();

        let result = record_youyou_read(
            &mut writer,
            ctx.clone(),
            9_999_999_999u64,
            &imposter_marker,
            &live_marker,
        );

        assert_eq!(
            result,
            Err(ReadGuardError::NotLiveSession),
            "mismatched markers must return Err(NotLiveSession)"
        );

        // Non-vacuous: confirm the bookmark did NOT move.
        assert_eq!(
            writer.read_at_for(&ctx),
            None,
            "bookmark must remain None when gate refuses"
        );
    }
}
