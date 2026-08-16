//! Read-acknowledgment message parser.
//!
//! The live session writes a small JSON file to signal that it has read up to
//! a given timestamp in a channel.  The clerk polls that file, parses the ack,
//! and calls `record_youyou_read` to advance the read bookmark.
//!
//! JSON shape: `{"channel":"<uuid-string>","up_to_ts":<u64>,"marker":"<session_id>"}`

use serde::Deserialize;

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
}
