//! Unread badge computation.
//!
//! Ports `desktop/src/features/channels/unreadChannelCounts.ts`.
//!
//! `total_unread`: events with `created_at > read_at`.
//! `badge_unread`: subset that "counts toward badge" -- DM channels or @mentions.

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::discovery::{ChannelInfo, ChannelType};
use crate::mailbox::{Mailbox, MailboxEntry};
use crate::read_state::ReadStateWriter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// All unread events (created_at > read_at).
    pub total_unread: u64,
    /// Unread events that warrant a badge notification: DM room or @mention.
    pub badge_unread: u64,
}

/// Compute badge counts for a room's entries.
///
/// `read_at`: the effective read cursor (Unix seconds). `None` = never read.
/// `is_dm`: whether this is a DM channel type.
/// `seat_pubkey_hex`: the seat's own public key hex; used to detect @mentions in `p_tags`.
pub fn compute_badge(
    entries: &[MailboxEntry],
    read_at: Option<u64>,
    is_dm: bool,
    seat_pubkey_hex: &str,
) -> Badge {
    let mut total_unread = 0u64;
    let mut badge_unread = 0u64;

    for entry in entries {
        if entry.author_pubkey == seat_pubkey_hex {
            continue;
        }
        let is_unread = match read_at {
            None => true,
            Some(ts) => entry.created_at > ts,
        };
        if !is_unread {
            continue;
        }
        total_unread += 1;
        let is_mention = entry.p_tags.iter().any(|p| p == seat_pubkey_hex);
        if is_dm || is_mention {
            badge_unread += 1;
        }
    }

    Badge {
        total_unread,
        badge_unread,
    }
}

/// Compute the mailbox-wide unread badge by summing per-channel badges.
///
/// For each channel known to `mailbox`:
/// - `read_at` is fetched from `read_state` using the channel UUID string as key.
/// - `is_dm` is derived from `channels`; channels absent from the map are treated as
///   non-DM (mention-only badging still applies via p-tags).
/// - Per-channel badge is computed via `compute_badge` and accumulated.
///
/// This is the US-04 surface: the aggregate badge reflects only what the live
/// session has read (via `record_youyou_read`). A fake actor or client-side
/// processing cannot decrement it.
pub fn unread_badge(
    mailbox: &Mailbox,
    read_state: &ReadStateWriter,
    channels: &HashMap<Uuid, ChannelInfo>,
    seat_pubkey_hex: &str,
) -> Badge {
    let mut total_unread = 0u64;
    let mut badge_unread = 0u64;

    for channel_id in mailbox.channel_ids() {
        let entries = mailbox.channel_entries(channel_id).unwrap_or_default();
        let read_at = read_state.read_at_for(&channel_id.to_string());
        let is_dm = channels
            .get(channel_id)
            .map(|info| info.channel_type == ChannelType::Dm)
            .unwrap_or(false);
        let b = compute_badge(entries, read_at, is_dm, seat_pubkey_hex);
        total_unread += b.total_unread;
        badge_unread += b.badge_unread;
    }

    Badge {
        total_unread,
        badge_unread,
    }
}

/// Serializable kind string for a channel, for use in the sidecar JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Dm,
    Stream,
    Unknown,
}

impl From<&ChannelType> for ChannelKind {
    fn from(t: &ChannelType) -> Self {
        match t {
            ChannelType::Dm => ChannelKind::Dm,
            ChannelType::Stream => ChannelKind::Stream,
            ChannelType::Unknown => ChannelKind::Unknown,
        }
    }
}

/// Per-channel unread summary for the sidecar file.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelBadge {
    pub channel_id: Uuid,
    pub name: String,
    pub kind: ChannelKind,
    pub total_unread: u32,
    pub badge_unread: u32,
}

/// Compute per-channel unread summaries, returning one entry per channel that
/// has `total_unread > 0`.
///
/// Uses the same unread/badge logic as `unread_badge` so the two can never
/// disagree. Channels not present in `channels` are treated as `Unknown` type.
///
/// The returned Vec is sorted so channels with `badge_unread > 0` come first.
pub fn per_channel_badges(
    mailbox: &Mailbox,
    read_state: &ReadStateWriter,
    channels: &HashMap<Uuid, ChannelInfo>,
    seat_pubkey_hex: &str,
) -> Vec<ChannelBadge> {
    let mut result: Vec<ChannelBadge> = Vec::new();

    for channel_id in mailbox.channel_ids() {
        let entries = mailbox.channel_entries(channel_id).unwrap_or_default();
        let read_at = read_state.read_at_for(&channel_id.to_string());
        let info = channels.get(channel_id);
        let is_dm = info
            .map(|i| i.channel_type == ChannelType::Dm)
            .unwrap_or(false);
        let b = compute_badge(entries, read_at, is_dm, seat_pubkey_hex);

        if b.total_unread == 0 {
            continue;
        }

        let kind = info
            .map(|i| ChannelKind::from(&i.channel_type))
            .unwrap_or(ChannelKind::Unknown);
        let name = info.map(|i| i.name.clone()).unwrap_or_default();

        // Saturating cast: u64 -> u32. Badge counts this large are not realistic
        // in practice, but we guard against overflow rather than panicking.
        result.push(ChannelBadge {
            channel_id: *channel_id,
            name,
            kind,
            total_unread: b.total_unread.min(u32::MAX as u64) as u32,
            badge_unread: b.badge_unread.min(u32::MAX as u64) as u32,
        });
    }

    // Sort: badge_unread > 0 first, then by channel_id for determinism.
    result.sort_by(|a, b| {
        b.badge_unread
            .cmp(&a.badge_unread)
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{ChannelInfo, ChannelType};
    use crate::mailbox::{Mailbox, MailboxEntry};
    use crate::read_state::{generate_slot_id, record_youyou_read, ReadStateWriter, SlotIdentity};
    use crate::session_identity::SessionMarker;
    use uuid::Uuid;

    fn entry(id: &str, created_at: u64, p_tags: Vec<String>) -> MailboxEntry {
        MailboxEntry {
            event_id: id.to_string(),
            created_at,
            author_pubkey: "aabb".to_string(),
            content: "hi".to_string(),
            p_tags,
            channel_uuid: Uuid::nil(),
        }
    }

    #[test]
    fn unread_count_strict_greater_than_read_at() {
        // created_at == read_at is already read; only > counts.
        let entries = vec![
            entry("e1", 100, vec![]),
            entry("e2", 200, vec![]),
            entry("e3", 300, vec![]),
        ];
        let badge = compute_badge(&entries, Some(200), false, "seat_pk");
        // Only e3 (300 > 200) is unread.
        assert_eq!(badge.total_unread, 1);
    }

    #[test]
    fn all_unread_when_no_read_marker() {
        let entries = vec![entry("e1", 100, vec![]), entry("e2", 200, vec![])];
        let badge = compute_badge(&entries, None, false, "seat_pk");
        assert_eq!(badge.total_unread, 2);
    }

    #[test]
    fn dm_channel_counts_toward_badge() {
        let entries = vec![entry("e1", 100, vec![])];
        let badge = compute_badge(&entries, None, true /* is_dm */, "seat_pk");
        assert_eq!(badge.badge_unread, 1);
    }

    #[test]
    fn mention_counts_toward_badge_even_in_non_dm() {
        let seat_pk = "seat_pubkey_hex";
        let entries = vec![entry("e1", 100, vec![seat_pk.to_string()])];
        let badge = compute_badge(&entries, None, false /* not dm */, seat_pk);
        assert_eq!(badge.badge_unread, 1);
    }

    #[test]
    fn plain_channel_message_no_mention_not_badge() {
        let entries = vec![entry("e1", 100, vec![])];
        let badge = compute_badge(&entries, None, false, "seat_pk");
        assert_eq!(badge.total_unread, 1);
        assert_eq!(badge.badge_unread, 0, "non-DM non-mention should not badge");
    }

    #[test]
    fn own_authored_events_not_counted() {
        let seat_pk = "seat_pk";
        let mut own = entry("e1", 300, vec![]);
        own.author_pubkey = seat_pk.to_string();
        let badge = compute_badge(&[own], None, false, seat_pk);
        assert_eq!(
            badge.total_unread, 0,
            "own-authored entry must not be counted"
        );
    }

    // ── US-04 helpers ─────────────────────────────────────────────────────────

    fn test_writer() -> ReadStateWriter {
        ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        })
    }

    fn dm_channel(uuid: Uuid) -> ChannelInfo {
        ChannelInfo {
            uuid,
            name: "dm-room".to_string(),
            channel_type: ChannelType::Dm,
        }
    }

    fn stream_channel(uuid: Uuid) -> ChannelInfo {
        ChannelInfo {
            uuid,
            name: "team-chat".to_string(),
            channel_type: ChannelType::Stream,
        }
    }

    fn mailbox_entry_for(
        id: &str,
        created_at: u64,
        channel_uuid: Uuid,
        author: &str,
        p_tags: Vec<String>,
    ) -> MailboxEntry {
        MailboxEntry {
            event_id: id.to_string(),
            created_at,
            author_pubkey: author.to_string(),
            content: "msg".to_string(),
            p_tags,
            channel_uuid,
        }
    }

    // US-04 S1: Arrival of a new for-me message increments total_unread and badge_unread.
    #[test]
    fn us04_s1_arrival_increments_badge() {
        const SEAT_PK: &str = "seat_pubkey_hex_aabb";
        const OTHER_PK: &str = "other_pubkey_hex_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, dm_channel(ch));

        // Start: mailbox empty, no read cursor -> badge 0.
        let writer = test_writer();
        let before = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(before.total_unread, 0);
        assert_eq!(before.badge_unread, 0);

        // Arrival: insert a DM message from another user (created_at=100, no read_at yet).
        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));

        let after = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(after.total_unread, 1, "arrival must increment total_unread");
        assert_eq!(
            after.badge_unread, 1,
            "DM arrival must increment badge_unread"
        );
    }

    // US-04 S2-positive: Live read advances the bookmark and decrements the badge.
    #[test]
    fn us04_s2_positive_live_read_decrements_badge() {
        const SEAT_PK: &str = "seat_pubkey_hex_aabb";
        const OTHER_PK: &str = "other_pubkey_hex_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, dm_channel(ch));

        // Seed 2 unread DM messages.
        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));
        mailbox.insert(ch, mailbox_entry_for("e2", 200, ch, OTHER_PK, vec![]));

        let mut writer = test_writer();
        let before = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(before.total_unread, 2, "pre-read: 2 unread");
        assert_eq!(before.badge_unread, 2, "pre-read: 2 badge");

        // Live session reads up through ts=100 (e1 only).
        let live = SessionMarker::new("live-session-001".to_string());
        record_youyou_read(&mut writer, ch.to_string(), 100, &live, &live)
            .expect("live read must succeed");

        let after = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(
            after.total_unread, 1,
            "after live read: 1 remaining unread (e2)"
        );
        assert_eq!(after.badge_unread, 1, "after live read: 1 remaining badge");
    }

    // US-04 S2-guard: A fake actor cannot advance the bookmark; badge stays unchanged.
    #[test]
    fn us04_s2_guard_fake_actor_cannot_decrement_badge() {
        const SEAT_PK: &str = "seat_pubkey_hex_aabb";
        const OTHER_PK: &str = "other_pubkey_hex_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, dm_channel(ch));

        // Seed 2 unread DM messages.
        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));
        mailbox.insert(ch, mailbox_entry_for("e2", 200, ch, OTHER_PK, vec![]));

        let mut writer = test_writer();
        let before = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(before.total_unread, 2);

        // Fake actor attempts to advance the bookmark.
        let live = SessionMarker::new("live-session-001".to_string());
        let fake = SessionMarker::new("fake-session-XYZ".to_string());
        let result = record_youyou_read(&mut writer, ch.to_string(), 200, &fake, &live);

        // Guard must reject the fake actor.
        assert_eq!(
            result,
            Err(crate::read_state::ReadGuardError::NotLiveSession),
            "fake actor must be refused with NotLiveSession"
        );

        // Badge must be unchanged (still 2).
        let after = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(
            after.total_unread, 2,
            "badge must be unchanged after fake-actor rejection"
        );
        assert_eq!(
            after.badge_unread, 2,
            "badge_unread must be unchanged after fake-actor rejection"
        );
    }

    // US-04 S3: Client filing a message (mailbox insert, no record_youyou_read) increments
    // the badge; it does NOT decrement it.
    #[test]
    fn us04_s3_client_processing_does_not_decrement_badge() {
        const SEAT_PK: &str = "seat_pubkey_hex_aabb";
        const OTHER_PK: &str = "other_pubkey_hex_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, stream_channel(ch));

        // Live session has already read up to ts=100.
        let mut writer = test_writer();
        let live = SessionMarker::new("live-session-001".to_string());
        record_youyou_read(&mut writer, ch.to_string(), 100, &live, &live).expect("seed live read");

        // Seed 1 already-read message and check baseline (badge = 0 because read_at=100 >= e1.100).
        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));
        let baseline = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(
            baseline.total_unread, 0,
            "baseline: e1 at cursor, not unread"
        );

        // Client files a new message (insert only; record_youyou_read NOT called).
        mailbox.insert(
            ch,
            mailbox_entry_for("e2", 200, ch, OTHER_PK, vec![SEAT_PK.to_string()]),
        );

        let after = unread_badge(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(
            after.total_unread, 1,
            "filing a new message must increment, not decrement, total_unread"
        );
        assert_eq!(
            after.badge_unread, 1,
            "mention in filed message must appear in badge_unread"
        );
    }

    // ── per_channel_badges tests ──────────────────────────────────────────────

    // (a) A DM channel yields an entry with badge_unread > 0.
    #[test]
    fn per_channel_dm_yields_badge_unread() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, dm_channel(ch));

        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));

        let writer = test_writer();
        let badges = super::per_channel_badges(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(badges.len(), 1, "DM with unread must produce one entry");
        assert!(
            badges[0].badge_unread > 0,
            "DM unread must yield badge_unread > 0"
        );
        assert_eq!(badges[0].kind, super::ChannelKind::Dm);
    }

    // (b) A mention in a stream channel yields badge_unread > 0.
    #[test]
    fn per_channel_stream_mention_yields_badge_unread() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, stream_channel(ch));

        // Message with a p-tag mention of the seat.
        mailbox.insert(
            ch,
            mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![SEAT_PK.to_string()]),
        );

        let writer = test_writer();
        let badges = super::per_channel_badges(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(badges.len(), 1);
        assert!(
            badges[0].badge_unread > 0,
            "stream mention must yield badge_unread > 0"
        );
        assert_eq!(badges[0].kind, super::ChannelKind::Stream);
    }

    // (c) A plain stream message (no mention) yields total_unread > 0 but badge_unread == 0.
    #[test]
    fn per_channel_plain_stream_message_no_badge_unread() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, stream_channel(ch));

        // Plain message with no p-tags.
        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));

        let writer = test_writer();
        let badges = super::per_channel_badges(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(badges.len(), 1, "non-empty stream must produce one entry");
        assert!(badges[0].total_unread > 0, "total_unread must be > 0");
        assert_eq!(
            badges[0].badge_unread, 0,
            "plain stream message must not set badge_unread"
        );
    }

    // (d) A fully-read channel is omitted from the result.
    #[test]
    fn per_channel_fully_read_channel_omitted() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";

        let ch = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch, dm_channel(ch));

        mailbox.insert(ch, mailbox_entry_for("e1", 100, ch, OTHER_PK, vec![]));

        // Mark the channel read at ts=100 (so e1 is not unread).
        let mut writer = test_writer();
        let live = SessionMarker::new("live-session-001".to_string());
        record_youyou_read(&mut writer, ch.to_string(), 100, &live, &live).expect("record read");

        let badges = super::per_channel_badges(&mailbox, &writer, &channels, SEAT_PK);
        assert!(
            badges.is_empty(),
            "fully-read channel must be absent from per_channel_badges"
        );
    }

    // (e) Channels with badge_unread > 0 sort before channels with badge_unread == 0.
    #[test]
    fn per_channel_badge_channels_sort_first() {
        const SEAT_PK: &str = "seat_pk_aabb";
        const OTHER_PK: &str = "other_pk_ccdd";

        let ch_stream = Uuid::new_v4();
        let ch_dm = Uuid::new_v4();
        let mut mailbox = Mailbox::new();
        let mut channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
        channels.insert(ch_stream, stream_channel(ch_stream));
        channels.insert(ch_dm, dm_channel(ch_dm));

        // Plain stream message (total_unread=1, badge_unread=0).
        mailbox.insert(
            ch_stream,
            mailbox_entry_for("e1", 100, ch_stream, OTHER_PK, vec![]),
        );
        // DM message (total_unread=1, badge_unread=1).
        mailbox.insert(ch_dm, mailbox_entry_for("e2", 100, ch_dm, OTHER_PK, vec![]));

        let writer = test_writer();
        let badges = super::per_channel_badges(&mailbox, &writer, &channels, SEAT_PK);
        assert_eq!(badges.len(), 2, "must have two entries");
        assert!(
            badges[0].badge_unread > 0,
            "first entry must have badge_unread > 0 (DM)"
        );
        assert_eq!(
            badges[1].badge_unread, 0,
            "second entry must have badge_unread == 0 (plain stream)"
        );
    }
}
