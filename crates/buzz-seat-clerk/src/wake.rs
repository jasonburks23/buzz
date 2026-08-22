//! Wake signal emitter.
//!
//! On Lane-1 (ForMe) events, writes a rich JSON wake object to a configured file.
//! The MCP bridge (buzz-bridge.ts) watches this file and expects the shape:
//!   `{"v":1,"channels":{"<uuid>":<latest_unread_at>,...}}`
//! Each channel's timestamp is that channel's own newest-unread `created_at`,
//! not the wall-clock time of the wake that triggered the write -- an untouched
//! room must never get a fresh "now" stamp just because some other room woke.
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

use crate::badge::{per_channel_badges, ChannelBadge};
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

    /// Write the rich v1 wake JSON that buzz-bridge.ts expects.
    ///
    /// Shape: `{"v":1,"channels":{"<channel-uuid>":<latest_unread_at>,...}}`.
    /// Each channel's stamp is its own newest-unread `created_at`; `unix_secs`
    /// (this wake's wall-clock time) is not used as a per-channel stamp.
    ///
    /// Only channels with `total_unread > 0` are included (they are the same
    /// channels that `emit_badge_sidecar` writes to the `.rooms` sidecar).
    /// If no channels have unread mail, writes `{"v":1,"channels":{}}` which
    /// the bridge treats as no pending wakes.
    ///
    /// On serialize or IO error the error is logged and the method returns
    /// without propagating so a wake write failure never kills the clerk.
    pub fn emit_rich(
        &self,
        unix_secs: u64,
        mailbox: &Mailbox,
        read_state: &ReadStateWriter,
        channels: &HashMap<Uuid, ChannelInfo>,
        seat_pubkey_hex: &str,
    ) {
        let badges = per_channel_badges(mailbox, read_state, channels, seat_pubkey_hex);
        self.emit_rich_from_badges(unix_secs, &badges);
    }

    /// Inner helper: build and write the v1 JSON from an already-computed badge slice.
    ///
    /// `_unix_secs` (this wake's wall-clock time) is intentionally unused for the
    /// per-channel stamps below -- each channel gets its own `latest_unread_at`
    /// instead, so an untouched room is never stamped with a fresh "now".
    fn emit_rich_from_badges(&self, _unix_secs: u64, badges: &[ChannelBadge]) {
        // Build the channels map: key = channel UUID string, value = that channel's
        // own newest-unread timestamp. Include all channels present in the badges
        // list (each has total_unread > 0 by construction from per_channel_badges).
        let channels_map: serde_json::Map<String, serde_json::Value> = badges
            .iter()
            .map(|b| {
                (
                    b.channel_id.to_string(),
                    serde_json::Value::from(b.latest_unread_at),
                )
            })
            .collect();

        let payload = serde_json::json!({
            "v": 1u64,
            "channels": channels_map,
        });

        let json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                warn!("rich wake serialize failed: {e}");
                return;
            }
        };

        if let Err(e) = fs::write(&self.wake_file_path, &json) {
            warn!("rich wake write failed: {e}");
        }
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

    // emit_rich writes a v1 JSON object that buzz-bridge.ts can parse, and
    // stamps EACH channel with ITS OWN newest-unread time, not the shared
    // wake-time AS_OF. This is the regression test for the phantom "1 new in
    // every room" bug (buzz#4): the old code stamped every channel with the
    // wall-clock time of whichever event triggered the wake, so a room
    // untouched for days looked freshly active on every poke.
    #[test]
    fn emit_rich_writes_v1_wake_json() {
        use crate::discovery::ChannelType;

        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";
        const AS_OF: u64 = 1_700_000_000;
        const CH1_LATEST: u64 = 1_650_000_000;
        const CH2_LATEST: u64 = 1_660_000_000;

        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

        let ch1 = Uuid::new_v4();
        let ch2 = Uuid::new_v4();

        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(
            ch1,
            ChannelInfo {
                uuid: ch1,
                name: "dm-room".to_string(),
                channel_type: ChannelType::Dm,
            },
        );
        channels.insert(
            ch2,
            ChannelInfo {
                uuid: ch2,
                name: "team-chat".to_string(),
                channel_type: ChannelType::Stream,
            },
        );

        // Two channels, two DIFFERENT newest-unread times, both distinct from
        // AS_OF (the wake trigger's wall-clock time).
        let mut mailbox = Mailbox::new();
        mailbox.insert(ch1, dm_entry("e1", CH1_LATEST, ch1, OTHER_PK));
        mailbox.insert(ch2, dm_entry("e2", CH2_LATEST, ch2, OTHER_PK));

        let writer = test_writer();

        emitter.emit_rich(AS_OF, &mailbox, &writer, &channels, SEAT_PK);

        assert!(wake_path.exists(), "wake file must be written by emit_rich");

        let raw = std::fs::read_to_string(&wake_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("emit_rich must produce valid JSON");

        // v must be 1 (bridge checks raw.v !== 1 and returns null otherwise).
        assert_eq!(parsed["v"].as_u64(), Some(1), "wake file must have v=1");

        // channels must be an object (bridge checks typeof raw.channels !== "object").
        let chans = parsed["channels"]
            .as_object()
            .expect("channels must be a JSON object");

        // Both channel UUIDs must appear as keys.
        let ch1_str = ch1.to_string();
        let ch2_str = ch2.to_string();
        assert!(
            chans.contains_key(&ch1_str),
            "ch1 UUID must be a key in channels"
        );
        assert!(
            chans.contains_key(&ch2_str),
            "ch2 UUID must be a key in channels"
        );

        // Each channel's value must be ITS OWN newest-unread time, not AS_OF.
        assert_eq!(
            chans[&ch1_str].as_u64(),
            Some(CH1_LATEST),
            "ch1 wake timestamp must equal ch1's newest unread message, not the shared AS_OF"
        );
        assert_eq!(
            chans[&ch2_str].as_u64(),
            Some(CH2_LATEST),
            "ch2 wake timestamp must equal ch2's newest unread message, not the shared AS_OF"
        );

        // The two channels must diverge from each other -- this is the crux of
        // the fix. Stamping both with a shared scalar (old behavior) collapses
        // this assertion.
        assert_ne!(
            chans[&ch1_str], chans[&ch2_str],
            "channels with different newest-unread times must get different stamps"
        );

        // No extra keys.
        assert_eq!(
            chans.len(),
            2,
            "channels must contain exactly the two unread badges"
        );
    }

    // emit_rich must stamp a channel with the NEWEST of its unread entries, not
    // merely "a" per-channel value. A single-unread-message fixture cannot
    // distinguish newest from oldest from first-inserted, so this seeds THREE
    // unread entries in a deliberately out-of-order insertion sequence and
    // requires the max. Also asserts a self-authored entry (which compute_badge
    // already excludes from unread) cannot move the stamp, since that skip is
    // otherwise unguarded by any timestamp-focused test.
    #[test]
    fn emit_rich_stamps_newest_of_several_unread_not_oldest_or_first_inserted() {
        use crate::discovery::ChannelType;

        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";
        const AS_OF: u64 = 1_700_000_000;
        const OLDEST: u64 = 1_650_000_000;
        const NEWEST: u64 = 1_670_000_000;
        const MIDDLE: u64 = 1_660_000_000;
        const SELF_AUTHORED_NEWER_THAN_NEWEST: u64 = 1_680_000_000;

        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

        let ch = Uuid::new_v4();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(
            ch,
            ChannelInfo {
                uuid: ch,
                name: "team-chat".to_string(),
                channel_type: ChannelType::Stream,
            },
        );

        let mut mailbox = Mailbox::new();
        // Deliberately out of order: neither first-inserted (OLDEST) nor
        // last-inserted (MIDDLE) is the newest -- only NEWEST is.
        mailbox.insert(ch, dm_entry("e1", OLDEST, ch, OTHER_PK));
        mailbox.insert(ch, dm_entry("e2", NEWEST, ch, OTHER_PK));
        mailbox.insert(ch, dm_entry("e3", MIDDLE, ch, OTHER_PK));
        // Self-authored entry, newer than everything above. compute_badge skips
        // own-authored entries entirely, so this must NOT become the stamp.
        mailbox.insert(
            ch,
            dm_entry("e4-self", SELF_AUTHORED_NEWER_THAN_NEWEST, ch, SEAT_PK),
        );

        let writer = test_writer();
        emitter.emit_rich(AS_OF, &mailbox, &writer, &channels, SEAT_PK);

        let raw = std::fs::read_to_string(&wake_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("emit_rich must produce valid JSON");
        let chans = parsed["channels"]
            .as_object()
            .expect("channels must be a JSON object");

        assert_eq!(
            chans[&ch.to_string()].as_u64(),
            Some(NEWEST),
            "stamp must be the NEWEST unread entry, not the oldest, the middle, \
             the first-inserted, or a self-authored entry that is newer still"
        );
    }

    // emit_rich with no unread messages writes {"v":1,"channels":{}} (no pending wakes).
    #[test]
    fn emit_rich_empty_mailbox_writes_empty_channels() {
        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

        let mailbox = Mailbox::new();
        let writer = test_writer();
        let channels: HashMap<Uuid, ChannelInfo> = HashMap::new();

        emitter.emit_rich(1_000, &mailbox, &writer, &channels, "seat_pk");

        let raw = std::fs::read_to_string(&wake_path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("must be valid JSON even with no badges");

        assert_eq!(parsed["v"].as_u64(), Some(1));
        let chans = parsed["channels"]
            .as_object()
            .expect("channels must be a JSON object");
        assert!(
            chans.is_empty(),
            "no unread messages means empty channels object"
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
