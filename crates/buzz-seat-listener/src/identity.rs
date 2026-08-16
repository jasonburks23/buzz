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
}
