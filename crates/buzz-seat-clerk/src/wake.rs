//! Wake signal emitter.
//!
//! On Lane-1 (ForMe) events, writes `<unix_secs>\n` to a configured file.
//! The supervisor (launchd / Hermes-later) watches this file via `WatchPaths`.
//! Lane-2/3 (Delivery) events do NOT trigger a write.
//!
//! Also writes a `<wake_file>.rooms` sidecar JSON (per-channel unread summary)
//! immediately after each Lane-1 wake so the woken session can open the right
//! rooms without sweeping every channel.
//!
//! NOTE: terminal-keystroke injection is explicitly rejected (Hermes-gated).

use std::collections::HashMap;
use std::fs;

use tracing::warn;
use uuid::Uuid;

use crate::badge::per_channel_badges;
use crate::discovery::ChannelInfo;
use crate::error::ClerkError;
use crate::lane::Lane;
use crate::mailbox::Mailbox;
use crate::read_state::ReadStateWriter;

pub struct WakeEmitter {
    wake_file_path: String,
}

impl WakeEmitter {
    pub fn new(wake_file_path: String) -> Self {
        Self { wake_file_path }
    }

    /// Write `<unix_secs>\n` to the wake file, overwriting any previous signal.
    pub fn emit(&self, unix_secs: u64) -> Result<(), ClerkError> {
        fs::write(&self.wake_file_path, format!("{unix_secs}\n"))?;
        Ok(())
    }

    /// Emit only if `lane` is `Lane::ForMe`. No-op for `Lane::Delivery`.
    pub fn emit_if_lane_1(&self, lane: &Lane, unix_secs: u64) -> Result<(), ClerkError> {
        if *lane == Lane::ForMe {
            self.emit(unix_secs)?;
        }
        Ok(())
    }

    /// Write a `<wake_file>.rooms` sidecar JSON with per-channel unread counts.
    ///
    /// JSON shape:
    /// ```json
    /// {
    ///   "as_of": <unix_secs>,
    ///   "channels": [
    ///     {"id":"<uuid>","name":"...","kind":"dm|stream|unknown","total_unread":N,"badge_unread":M},
    ///     ...
    ///   ]
    /// }
    /// ```
    /// Channels with `total_unread == 0` are omitted. Channels with `badge_unread > 0`
    /// sort first. On any serialize or IO error, the error is logged and the method
    /// returns without propagating so a sidecar write failure never kills the clerk.
    pub fn emit_badge_sidecar(
        &self,
        unix_secs: u64,
        mailbox: &Mailbox,
        read_state: &ReadStateWriter,
        channels: &HashMap<Uuid, ChannelInfo>,
        seat_pubkey_hex: &str,
    ) {
        let badges = per_channel_badges(mailbox, read_state, channels, seat_pubkey_hex);

        // Build a serde_json-serializable structure inline using serde_json::json!.
        let channel_entries: Vec<serde_json::Value> = badges
            .iter()
            .map(|b| {
                serde_json::json!({
                    "id": b.channel_id.to_string(),
                    "name": b.name,
                    "kind": match b.kind {
                        crate::badge::ChannelKind::Dm => "dm",
                        crate::badge::ChannelKind::Stream => "stream",
                        crate::badge::ChannelKind::Unknown => "unknown",
                    },
                    "total_unread": b.total_unread,
                    "badge_unread": b.badge_unread,
                })
            })
            .collect();

        let payload = serde_json::json!({
            "as_of": unix_secs,
            "channels": channel_entries,
        });

        let json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                warn!("badge sidecar serialize failed: {e}");
                return;
            }
        };

        let sidecar_path = format!("{}.rooms", self.wake_file_path);
        if let Err(e) = fs::write(&sidecar_path, &json) {
            warn!("badge sidecar write failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;
    use uuid::Uuid;

    use crate::discovery::{ChannelInfo, ChannelType};
    use crate::mailbox::{Mailbox, MailboxEntry};
    use crate::read_state::{generate_slot_id, ReadStateWriter, SlotIdentity};

    fn test_writer() -> ReadStateWriter {
        ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        })
    }

    fn dm_entry(id: &str, created_at: u64, channel_uuid: Uuid, author: &str) -> MailboxEntry {
        MailboxEntry {
            event_id: id.to_string(),
            created_at,
            author_pubkey: author.to_string(),
            content: "msg".to_string(),
            p_tags: vec![],
            channel_uuid,
        }
    }

    #[test]
    fn emit_writes_timestamp_to_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit(1_700_000_000).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("1700000000"),
            "wake file must contain the timestamp"
        );
    }

    #[test]
    fn emit_overwrites_previous_signal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit(1_000).unwrap();
        emitter.emit(2_000).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("2000"));
        assert!(!content.contains("1000"), "old signal must be overwritten");
    }

    #[test]
    fn lane_delivery_does_not_emit() {
        use crate::lane::Lane;
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_if_lane_1(&Lane::Delivery, 1_000).unwrap();
        assert!(!path.exists(), "Delivery lane must not write wake file");
    }

    #[test]
    fn lane_for_me_does_emit() {
        use crate::lane::Lane;
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_if_lane_1(&Lane::ForMe, 1_700_000_000).unwrap();
        assert!(path.exists(), "ForMe lane must write wake file");
    }

    // Sidecar test: emit_badge_sidecar writes <wake>.rooms with correct JSON.
    // Asserts:
    //   - the .rooms file exists after the call
    //   - it contains valid JSON with "as_of" and "channels"
    //   - a DM channel with unread appears with badge_unread > 0
    //   - a plain stream channel with unread appears with badge_unread == 0
    //   - a fully-read channel is absent
    //   - badge_unread > 0 channels sort first
    #[test]
    fn emit_badge_sidecar_writes_rooms_file() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";
        const AS_OF: u64 = 1_700_000_000;

        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let sidecar_path = dir.path().join("wake.rooms");

        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

        // Set up two channels: one DM with unread, one plain stream with unread,
        // one DM that is fully read (should be absent).
        let ch_dm = Uuid::new_v4();
        let ch_stream = Uuid::new_v4();
        let ch_read = Uuid::new_v4();

        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(
            ch_dm,
            ChannelInfo {
                uuid: ch_dm,
                name: "dm-room".to_string(),
                channel_type: ChannelType::Dm,
            },
        );
        channels.insert(
            ch_stream,
            ChannelInfo {
                uuid: ch_stream,
                name: "team-chat".to_string(),
                channel_type: ChannelType::Stream,
            },
        );
        channels.insert(
            ch_read,
            ChannelInfo {
                uuid: ch_read,
                name: "read-channel".to_string(),
                channel_type: ChannelType::Dm,
            },
        );

        let mut mailbox = Mailbox::new();
        // DM with unread message.
        mailbox.insert(ch_dm, dm_entry("e1", 100, ch_dm, OTHER_PK));
        // Stream with unread message (no mention, so badge_unread==0).
        mailbox.insert(ch_stream, dm_entry("e2", 100, ch_stream, OTHER_PK));
        // ch_read: has a message but read_at = 100 (message at 100 is NOT unread: 100 > 100 is false).
        mailbox.insert(ch_read, dm_entry("e3", 100, ch_read, OTHER_PK));

        // Advance read cursor for ch_read so its message is fully read.
        let mut writer = test_writer();
        let live = crate::session_identity::SessionMarker::new("live-session-001".to_string());
        crate::read_state::record_youyou_read(&mut writer, ch_read.to_string(), 100, &live, &live)
            .expect("seed read");

        // Call the method under test.
        emitter.emit_badge_sidecar(AS_OF, &mailbox, &writer, &channels, SEAT_PK);

        // The sidecar file must exist.
        assert!(
            sidecar_path.exists(),
            ".rooms sidecar file must be written by emit_badge_sidecar"
        );

        // Parse the JSON.
        let raw = std::fs::read_to_string(&sidecar_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("sidecar must be valid JSON");

        // "as_of" must equal the timestamp passed in.
        assert_eq!(
            parsed["as_of"].as_u64(),
            Some(AS_OF),
            "as_of must equal the unix_secs argument"
        );

        let chans = parsed["channels"]
            .as_array()
            .expect("channels must be an array");

        // Two channels have unread; ch_read is absent.
        assert_eq!(chans.len(), 2, "exactly 2 channels with unread must appear");

        // The fully-read channel must not appear.
        let ch_read_str = ch_read.to_string();
        assert!(
            !chans.iter().any(|c| c["id"].as_str() == Some(&ch_read_str)),
            "fully-read channel must be absent from sidecar"
        );

        // Find the DM entry.
        let ch_dm_str = ch_dm.to_string();
        let dm_entry_json = chans
            .iter()
            .find(|c| c["id"].as_str() == Some(&ch_dm_str))
            .expect("DM channel must appear in sidecar");
        assert_eq!(dm_entry_json["kind"].as_str(), Some("dm"));
        assert_eq!(dm_entry_json["name"].as_str(), Some("dm-room"));
        assert!(
            dm_entry_json["badge_unread"].as_u64().unwrap_or(0) > 0,
            "DM channel must have badge_unread > 0"
        );

        // Find the stream entry.
        let ch_stream_str = ch_stream.to_string();
        let stream_entry = chans
            .iter()
            .find(|c| c["id"].as_str() == Some(&ch_stream_str))
            .expect("stream channel must appear in sidecar");
        assert_eq!(stream_entry["kind"].as_str(), Some("stream"));
        assert_eq!(
            stream_entry["badge_unread"].as_u64(),
            Some(0),
            "plain stream channel must have badge_unread == 0"
        );

        // badge_unread > 0 must sort first.
        assert!(
            chans[0]["badge_unread"].as_u64().unwrap_or(0) > 0,
            "first channel in sidecar must have badge_unread > 0"
        );
    }

    // A sidecar write for an empty mailbox must produce an empty channels array.
    #[test]
    fn emit_badge_sidecar_empty_mailbox_produces_empty_channels() {
        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let sidecar_path = dir.path().join("wake.rooms");

        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());
        let mailbox = Mailbox::new();
        let writer = test_writer();
        let channels: HashMap<Uuid, ChannelInfo> = HashMap::new();

        emitter.emit_badge_sidecar(999, &mailbox, &writer, &channels, "seat_pk");

        let raw = std::fs::read_to_string(&sidecar_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(
            parsed["channels"].as_array().map(|a| a.len()),
            Some(0),
            "empty mailbox must produce an empty channels array"
        );
    }
}
