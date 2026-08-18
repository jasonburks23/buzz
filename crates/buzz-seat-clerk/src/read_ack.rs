//! Read-acknowledgment message parser.
//!
//! The live session writes a small JSON file to signal that it has read up to
//! a given timestamp in a channel. The clerk polls that file, parses the ack,
//! and calls `record_youyou_read` to advance the read bookmark.
//!
//! Single-channel JSON shape:
//!   `{"channel":"<uuid-string>","up_to_ts":<u64>,"marker":"<session_id>"}`
//! Multi-channel JSON shape (v1):
//!   `{"v":1,"channels":{"<uuid>":<unix_secs>,...},"marker":"<session_id>"}`

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::ClerkError;

/// A parsed read-acknowledgment from the live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAck {
    /// Channel UUID string.
    pub channel: String,
    /// Unix-seconds timestamp the live session has read up to.
    pub up_to_ts: u64,
    /// Session marker string (the actor's session_id).
    pub marker: String,
}

#[derive(Deserialize)]
struct RawAck {
    channel: String,
    up_to_ts: u64,
    marker: String,
}

/// Parse a read-ack from a JSON string.
///
/// Returns `None` if the JSON is malformed, any required field is missing, or
/// `channel`/`marker` are empty strings.
pub fn parse_read_ack(bytes: &str) -> Option<ReadAck> {
    let raw: RawAck = serde_json::from_str(bytes).ok()?;
    if raw.channel.is_empty() || raw.marker.is_empty() {
        return None;
    }
    Some(ReadAck {
        channel: raw.channel,
        up_to_ts: raw.up_to_ts,
        marker: raw.marker,
    })
}

/// v1 per-channel read-ack written by the live session.
/// JSON: `{"v":1,"channels":{"<uuid>":<unix_secs>,...},"marker":"<session_id>"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiChannelAck {
    pub v: u8,
    /// Channel UUID string -> last-read unix_secs.
    pub channels: HashMap<String, u64>,
    /// Session marker (the actor's session_id).
    pub marker: String,
}

/// Parse a MultiChannelAck from a JSON string.
///
/// Returns None if the JSON is malformed, v != 1, marker is empty,
/// or the channels field is missing.
pub fn parse_multi_channel_ack(bytes: &str) -> Option<MultiChannelAck> {
    let raw: MultiChannelAck = serde_json::from_str(bytes).ok()?;
    if raw.v != 1 || raw.marker.is_empty() {
        return None;
    }
    Some(raw)
}

/// Write a MultiChannelAck to a file atomically.
///
/// Serializes to JSON, writes to a temp file in the target's parent directory,
/// then persists (renames) it into place so a reader never sees a partial file.
pub fn write_multi_channel_ack(path: &str, ack: &MultiChannelAck) -> Result<(), ClerkError> {
    let json = serde_json::to_string(ack)?;
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ack_parses_all_fields() {
        let json =
            r#"{"channel":"chan-uuid-abc","up_to_ts":1700000042,"marker":"live-session-xyz"}"#;
        let ack = parse_read_ack(json).expect("valid JSON must parse");
        assert_eq!(ack.channel, "chan-uuid-abc");
        assert_eq!(ack.up_to_ts, 1_700_000_042u64);
        assert_eq!(ack.marker, "live-session-xyz");
    }

    #[test]
    fn malformed_json_returns_none() {
        assert_eq!(parse_read_ack("not json {{{"), None);
    }

    #[test]
    fn missing_field_returns_none() {
        // Missing 'marker' field.
        let json = r#"{"channel":"chan-abc","up_to_ts":100}"#;
        assert_eq!(parse_read_ack(json), None);

        // Missing 'channel' field.
        let json2 = r#"{"up_to_ts":100,"marker":"sid"}"#;
        assert_eq!(parse_read_ack(json2), None);

        // Missing 'up_to_ts' field.
        let json3 = r#"{"channel":"chan-abc","marker":"sid"}"#;
        assert_eq!(parse_read_ack(json3), None);
    }

    #[test]
    fn empty_channel_returns_none() {
        let json = r#"{"channel":"","up_to_ts":100,"marker":"sid"}"#;
        assert_eq!(parse_read_ack(json), None);
    }

    #[test]
    fn empty_marker_returns_none() {
        let json = r#"{"channel":"chan-abc","up_to_ts":100,"marker":""}"#;
        assert_eq!(parse_read_ack(json), None);
    }

    // Multi-channel tests (TDD: written before implementation).

    #[test]
    fn multi_channel_ack_parses_two_channels() {
        let json = r#"{"v":1,"channels":{"chan-a":100,"chan-b":200},"marker":"sid"}"#;
        let ack = parse_multi_channel_ack(json).expect("must parse");
        assert_eq!(ack.v, 1);
        assert_eq!(ack.channels["chan-a"], 100u64);
        assert_eq!(ack.channels["chan-b"], 200u64);
        assert_eq!(ack.marker, "sid");
    }

    #[test]
    fn multi_channel_ack_empty_marker_returns_none() {
        let json = r#"{"v":1,"channels":{"chan-a":100},"marker":""}"#;
        assert_eq!(parse_multi_channel_ack(json), None);
    }

    #[test]
    fn multi_channel_ack_wrong_version_returns_none() {
        let json = r#"{"v":0,"channels":{"chan-a":100},"marker":"sid"}"#;
        assert_eq!(parse_multi_channel_ack(json), None);
    }

    #[test]
    fn multi_channel_ack_missing_channels_field_returns_none() {
        let json = r#"{"v":1,"marker":"sid"}"#;
        assert_eq!(parse_multi_channel_ack(json), None);
    }

    #[test]
    fn write_multi_channel_ack_roundtrips() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("readack.json").display().to_string();
        let ack = MultiChannelAck {
            v: 1,
            channels: [
                ("chan-a".to_string(), 100u64),
                ("chan-b".to_string(), 200u64),
            ]
            .into_iter()
            .collect(),
            marker: "test-session".to_string(),
        };
        write_multi_channel_ack(&path, &ack).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_multi_channel_ack(&raw).expect("must parse back");
        assert_eq!(parsed, ack);
    }
}
