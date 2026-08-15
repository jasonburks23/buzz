//! Session-identity marker for live-seat guard tests.
//!
//! Every Buzz read and send must carry the live session's marker so downstream
//! guard checks can prove the actor is the continuous live session and not a
//! freshly spawned process.
//!
//! The marker IS the Claude Code `session_id`: a UUID that is stable across
//! the session's turns and compaction, and different for any fresh spawn.
//! It is read once at boot from a sidecar file written by the host harness.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

// ─── Marker ──────────────────────────────────────────────────────────────────

/// Newtype over the Claude Code session_id string.
///
/// `Debug` shows only a fixed label so log output stays tidy.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionMarker(pub(crate) String);

impl SessionMarker {
    /// Construct a marker from a session-id string.
    ///
    /// Exposed `pub` so integration tests (external crate) can build markers
    /// for use with `record_youyou_read`. Application code should use
    /// `load_live_marker` instead.
    pub fn new(id: String) -> Self {
        Self(id)
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

/// Errors that can occur while loading the session marker from the sidecar.
#[derive(Debug, Error)]
pub enum SessionIdentityError {
    /// The sidecar file does not exist at the expected path.
    #[error("sidecar file not found: {path}")]
    SidecarMissing { path: String },

    /// The sidecar file exists but could not be read.
    #[error("sidecar unreadable: {source}")]
    SidecarUnreadable { source: std::io::Error },

    /// The sidecar JSON was read but does not contain the expected fields.
    #[error("malformed sidecar JSON: {reason}")]
    MalformedSidecar { reason: String },

    /// The session_id field in the sidecar is empty or whitespace-only.
    ///
    /// An empty marker would allow two unrelated fakes to match each other,
    /// which defeats the live-seat guard entirely.
    #[error("sidecar session_id is empty or whitespace-only")]
    EmptySessionId,
}

// ─── Sidecar shape ───────────────────────────────────────────────────────────

/// JSON shape of the sidecar file written by the Claude Code harness.
#[derive(Deserialize)]
struct SidecarFile {
    session_id: String,
}

// ─── Loader ──────────────────────────────────────────────────────────────────

/// Read the live session marker from the sidecar file.
///
/// The sidecar path is `<sidecar_dir>/claude-seat-id-<session_id>.json`.
/// `sidecar_dir` is injectable so tests never touch real `/tmp`.
pub fn load_live_marker(
    sidecar_dir: &Path,
    session_id: &str,
) -> Result<SessionMarker, SessionIdentityError> {
    let path = sidecar_dir.join(format!("claude-seat-id-{session_id}.json"));
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SessionIdentityError::SidecarMissing {
                path: path.display().to_string(),
            }
        } else {
            SessionIdentityError::SidecarUnreadable { source: e }
        }
    })?;
    let parsed: SidecarFile =
        serde_json::from_str(&raw).map_err(|e| SessionIdentityError::MalformedSidecar {
            reason: e.to_string(),
        })?;
    if parsed.session_id.trim().is_empty() {
        return Err(SessionIdentityError::EmptySessionId);
    }
    Ok(SessionMarker(parsed.session_id))
}

// ─── Event record ────────────────────────────────────────────────────────────

/// The kind of turn event (read or send).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Read,
    Send,
}

/// A single turn event stamped with an optional session marker.
///
/// `marker` is `None` when the event originated from a fresh spawn that had
/// no access to the live sidecar.
#[derive(Debug, Clone)]
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
    use std::io::Write;

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

    // Loader test: write a temp sidecar and verify the loader returns the correct marker.
    #[test]
    fn loader_reads_marker_from_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        let session_id = "test-sid";
        let sidecar_path = tmp.join(format!("claude-seat-id-{session_id}.json"));
        let mut f = std::fs::File::create(&sidecar_path).unwrap();
        writeln!(
            f,
            r#"{{"session_id":"{session_id}","ghostty_id":"g1","cwd":"/tmp","role_marker":"agencyos-cc"}}"#
        )
        .unwrap();

        let result = load_live_marker(tmp, session_id);
        assert!(result.is_ok(), "loader failed: {:?}", result);
        assert_eq!(result.unwrap(), SessionMarker(session_id.to_string()));
    }

    // Loader test: missing file returns SidecarMissing.
    #[test]
    fn loader_returns_missing_error_for_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();
        // Do NOT create the sidecar file.
        let result = load_live_marker(tmp, "nonexistent-sid");
        assert!(
            matches!(result, Err(SessionIdentityError::SidecarMissing { .. })),
            "expected SidecarMissing, got {:?}",
            result
        );
    }

    // Loader test: empty or whitespace-only session_id returns EmptySessionId.
    #[test]
    fn loader_rejects_empty_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        // Write a sidecar with session_id = "".
        let empty_sidecar = tmp.join("claude-seat-id-.json");
        std::fs::write(&empty_sidecar, r#"{"session_id":""}"#).unwrap();
        let result = load_live_marker(tmp, "");
        assert!(
            matches!(result, Err(SessionIdentityError::EmptySessionId)),
            "expected EmptySessionId for empty string, got {:?}",
            result
        );

        // Write a sidecar with session_id = "   " (whitespace only).
        let ws_sidecar = tmp.join("claude-seat-id-   .json");
        std::fs::write(&ws_sidecar, r#"{"session_id":"   "}"#).unwrap();
        let result = load_live_marker(tmp, "   ");
        assert!(
            matches!(result, Err(SessionIdentityError::EmptySessionId)),
            "expected EmptySessionId for whitespace-only string, got {:?}",
            result
        );
    }

    // Loader test: malformed JSON in sidecar returns MalformedSidecar.
    #[test]
    fn loader_rejects_malformed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();

        let session_id = "test-malformed";
        let sidecar_path = tmp.join(format!("claude-seat-id-{session_id}.json"));
        std::fs::write(&sidecar_path, "not json {").unwrap();

        let result = load_live_marker(tmp, session_id);
        assert!(
            matches!(result, Err(SessionIdentityError::MalformedSidecar { .. })),
            "expected MalformedSidecar, got {:?}",
            result
        );
    }
}
