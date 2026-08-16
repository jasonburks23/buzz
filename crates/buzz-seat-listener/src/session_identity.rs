//! Generic session identity types.
//!
//! Contains only data types that are meaningful outside any fleet context.
//! IO functions (load_live_marker, resolve_live_marker_from_claims) and
//! fleet-specific structs (SidecarFile, SeatClaimFile) live in
//! buzz-seat-clerk-agencyos::claim_identity.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Marker ──────────────────────────────────────────────────────────────────

/// Opaque marker that identifies one running clerk session.
///
/// Wraps a string (typically a UUID or a role-stamped slug). Two markers
/// are equal when their inner strings are equal.
///
/// `Debug` shows only a fixed label so log output stays tidy.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionMarker(pub(crate) String);

impl SessionMarker {
    /// Create a new SessionMarker from any string.
    ///
    /// Exposed `pub` so integration tests (external crate) can build markers
    /// for use with `record_youyou_read`. Fleet code should use
    /// `ClaimFileIdentity` (in buzz-seat-clerk-agencyos) to resolve the live marker.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Debug for SessionMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionMarker(<id>)")
    }
}

impl std::fmt::Display for SessionMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionMarker(<id>)")
    }
}

// ─── Error ───────────────────────────────────────────────────────────────────

/// Errors that can occur when working with session identity types.
#[derive(Debug, Error)]
pub enum SessionIdentityError {
    /// A required file could not be read.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A file's JSON content could not be parsed.
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─── Event record ────────────────────────────────────────────────────────────

/// The kind of turn event (read or send).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Read,
    Send,
}

/// A single turn event stamped with an optional session marker.
///
/// `marker` is `None` when the event originated from a fresh spawn that had
/// no access to the live sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnEvent {
    pub kind: EventKind,
    pub marker: Option<SessionMarker>,
}

impl TurnEvent {
    /// Construct a new turn event.
    pub fn new(kind: EventKind, marker: Option<SessionMarker>) -> Self {
        Self { kind, marker }
    }

    /// Returns `true` only if this event's marker is present and equals `live`.
    ///
    /// `None` or a different marker returns `false`, which is the whole
    /// discriminator between the live session and a fresh spawn.
    pub fn matches_live(&self, live: &SessionMarker) -> bool {
        self.marker.as_ref() == Some(live)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> SessionMarker {
        SessionMarker("live-session-uuid-1234".to_string())
    }

    // Scenario 1: Read event stamped with the live marker matches.
    #[test]
    fn read_event_with_live_marker_matches() {
        let marker = live();
        let event = TurnEvent::new(EventKind::Read, Some(marker.clone()));
        assert!(event.matches_live(&live()));
    }

    // Scenario 2: Send event stamped with the live marker matches.
    #[test]
    fn send_event_with_live_marker_matches() {
        let marker = live();
        let event = TurnEvent::new(EventKind::Send, Some(marker.clone()));
        assert!(event.matches_live(&live()));
    }

    // Scenario 3a: Event stamped with a DIFFERENT marker does NOT match.
    #[test]
    fn event_with_different_marker_does_not_match() {
        let other = SessionMarker("some-other-session-uuid".to_string());
        let event = TurnEvent::new(EventKind::Read, Some(other));
        assert!(!event.matches_live(&live()));
    }

    // Scenario 3b: Event with no marker (fresh spawn, None) does NOT match.
    #[test]
    fn event_with_no_marker_does_not_match() {
        let event = TurnEvent::new(EventKind::Read, None);
        assert!(!event.matches_live(&live()));
    }

    // Scenario 4: Multiple events across "turns" all stamped with the same
    // live marker all match, and their markers are equal to each other.
    #[test]
    fn stable_across_multiple_turns() {
        let live_marker = live();
        let events: Vec<TurnEvent> = vec![
            TurnEvent::new(EventKind::Read, Some(live_marker.clone())),
            TurnEvent::new(EventKind::Send, Some(live_marker.clone())),
            TurnEvent::new(EventKind::Read, Some(live_marker.clone())),
        ];
        for event in &events {
            assert!(event.matches_live(&live_marker));
        }
        // All markers equal each other.
        let markers: Vec<&SessionMarker> =
            events.iter().filter_map(|e| e.marker.as_ref()).collect();
        assert!(markers.windows(2).all(|w| w[0] == w[1]));
    }
}
