//! Per-room ordered message store.

use std::collections::HashMap;
use std::fmt;

use uuid::Uuid;

/// A single delivered message.
///
/// # Privacy note
/// `content` holds plaintext DM text. The `Debug` impl redacts it so log
/// macros never expose message bodies.
#[derive(Clone)]
pub struct MailboxEntry {
    pub event_id: String,
    pub created_at: u64,
    pub author_pubkey: String,
    /// Plaintext content (kind-9 is never encrypted).
    /// Not exposed via Debug to prevent accidental log leaks.
    pub content: String,
    /// `p` tag values from the event (used for Lane classification).
    pub p_tags: Vec<String>,
    pub channel_uuid: Uuid,
}

impl fmt::Debug for MailboxEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxEntry")
            .field("event_id", &self.event_id)
            .field("created_at", &self.created_at)
            .field("author_pubkey", &self.author_pubkey)
            .field("content", &"<redacted>")
            .field("p_tags", &self.p_tags)
            .field("channel_uuid", &self.channel_uuid)
            .finish()
    }
}

/// Per-room ordered mailbox (oldest-first by `created_at`).
pub struct Mailbox {
    rooms: HashMap<Uuid, Vec<MailboxEntry>>,
}

impl Mailbox {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
        }
    }

    /// Insert an entry, deduplicating by `event_id` and maintaining oldest-first order.
    pub fn insert(&mut self, channel: Uuid, entry: MailboxEntry) {
        let entries = self.rooms.entry(channel).or_default();
        if entries.iter().any(|e| e.event_id == entry.event_id) {
            return;
        }
        let pos = entries
            .binary_search_by_key(&entry.created_at, |e| e.created_at)
            .unwrap_or_else(|i| i);
        entries.insert(pos, entry);
    }

    /// Return all entries for a channel in oldest-first order, or `None` if unknown.
    pub fn channel_entries(&self, channel: &Uuid) -> Option<&[MailboxEntry]> {
        self.rooms.get(channel).map(|v| v.as_slice())
    }

    /// Return an iterator over all channel UUIDs known to this mailbox.
    pub fn channel_ids(&self) -> impl Iterator<Item = &Uuid> {
        self.rooms.keys()
    }

    /// Remove all entries for `channel` with `created_at <= up_to_ts`.
    ///
    /// Called after a channel's read watermark advances so that already-read
    /// messages do not accumulate forever. Entries above the watermark
    /// (`created_at > up_to_ts`) are kept; other channels are untouched.
    ///
    /// Because the vec is sorted oldest-first, this is a cheap prefix drain:
    /// find the first entry above the watermark and drain everything before it.
    pub fn prune_channel_below(&mut self, channel: &Uuid, up_to_ts: u64) {
        if let Some(entries) = self.rooms.get_mut(channel) {
            // Find the first index where created_at > up_to_ts.
            let split = entries.partition_point(|e| e.created_at <= up_to_ts);
            entries.drain(..split);
        }
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, created_at: u64) -> MailboxEntry {
        MailboxEntry {
            event_id: id.to_string(),
            created_at,
            author_pubkey: "aabb".to_string(),
            content: "hello".to_string(),
            p_tags: vec![],
            channel_uuid: Uuid::nil(),
        }
    }

    #[test]
    fn insert_orders_oldest_first() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("e1", 100));
        mb.insert(ch, entry("e2", 50));
        mb.insert(ch, entry("e3", 75));
        let entries = mb.channel_entries(&ch).unwrap();
        assert_eq!(entries[0].event_id, "e2");
        assert_eq!(entries[1].event_id, "e3");
        assert_eq!(entries[2].event_id, "e1");
    }

    #[test]
    fn insert_deduplicates_by_event_id() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("e1", 100));
        mb.insert(ch, entry("e1", 100)); // duplicate
        assert_eq!(mb.channel_entries(&ch).unwrap().len(), 1);
    }

    #[test]
    fn channel_entries_returns_none_for_unknown_channel() {
        let mb = Mailbox::new();
        assert!(mb.channel_entries(&Uuid::new_v4()).is_none());
    }

    #[test]
    fn all_entries_since_filters_by_timestamp() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("e1", 100));
        mb.insert(ch, entry("e2", 200));
        mb.insert(ch, entry("e3", 300));
        // since=150 means entries with created_at > 150
        let since: Vec<_> = mb
            .channel_entries(&ch)
            .unwrap()
            .iter()
            .filter(|e| e.created_at > 150)
            .collect();
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].event_id, "e2");
    }

    // US-06 S1: oldest-first ordering must hold even on out-of-order insert.
    #[test]
    fn us06_s1_out_of_order_insert_returns_oldest_first() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        // Insert three entries for ONE channel OUT OF ORDER: T3=300, T1=100, T2=200.
        mb.insert(ch, entry("e3", 300));
        mb.insert(ch, entry("e1", 100));
        mb.insert(ch, entry("e2", 200));
        let entries = mb.channel_entries(&ch).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].created_at, 100, "first must be oldest (T1=100)");
        assert_eq!(entries[1].created_at, 200, "second must be middle (T2=200)");
        assert_eq!(entries[2].created_at, 300, "third must be newest (T3=300)");
        assert_eq!(entries[0].event_id, "e1");
        assert_eq!(entries[1].event_id, "e2");
        assert_eq!(entries[2].event_id, "e3");
    }

    // US-06 S1 (two-channel): each channel's entries are independently oldest-first.
    #[test]
    fn us06_s1_two_channels_independently_oldest_first() {
        let mut mb = Mailbox::new();
        let ch_a = Uuid::new_v4();
        let ch_b = Uuid::new_v4();
        // Interleave inserts across two channels.
        mb.insert(ch_a, entry("a3", 300));
        mb.insert(ch_b, entry("b2", 200));
        mb.insert(ch_a, entry("a1", 100));
        mb.insert(ch_b, entry("b3", 300));
        mb.insert(ch_a, entry("a2", 200));
        mb.insert(ch_b, entry("b1", 100));

        let a = mb.channel_entries(&ch_a).unwrap();
        assert_eq!(a[0].event_id, "a1", "ch_a: oldest first");
        assert_eq!(a[1].event_id, "a2");
        assert_eq!(a[2].event_id, "a3", "ch_a: newest last");

        let b = mb.channel_entries(&ch_b).unwrap();
        assert_eq!(b[0].event_id, "b1", "ch_b: oldest first");
        assert_eq!(b[1].event_id, "b2");
        assert_eq!(b[2].event_id, "b3", "ch_b: newest last");
    }

    // US-02 S4 at the mailbox layer.
    // Offline/restart durability scenarios (S5/S6) are relay-gated and live in the
    // integration suite, NOT here.
    #[test]
    fn us02_s4_n_sent_equals_n_received_in_order() {
        const N: usize = 25;
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        // Insert N entries with strictly increasing created_at and unique event_ids.
        for i in 0..N {
            let id = format!("ev-{:03}", i);
            let ts = (i as u64) * 10 + 1000; // 1000, 1010, 1020, ..., 1240
            let mut e = entry(&id, ts);
            e.channel_uuid = ch;
            mb.insert(ch, e);
        }
        let entries = mb.channel_entries(&ch).unwrap();
        // Assert count matches.
        assert_eq!(entries.len(), N, "all {} entries must be present", N);
        // Assert ascending created_at order with no gaps and no duplicates.
        for (i, e) in entries.iter().enumerate() {
            let expected_id = format!("ev-{:03}", i);
            assert_eq!(
                e.event_id, expected_id,
                "position {}: expected {} got {}",
                i, expected_id, e.event_id
            );
            assert_eq!(
                e.created_at,
                (i as u64) * 10 + 1000,
                "position {}: wrong timestamp",
                i
            );
        }
        // Verify no duplicates via a set check.
        let ids: std::collections::HashSet<&str> =
            entries.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids.len(), N, "no duplicate event_ids allowed");
    }

    // ── prune_channel_below tests ────────────────────────────────────────────

    // Helper: entry with a specific channel UUID (used in cross-channel tests).
    fn entry_for(id: &str, created_at: u64, ch: Uuid) -> MailboxEntry {
        MailboxEntry {
            event_id: id.to_string(),
            created_at,
            author_pubkey: "aabb".to_string(),
            content: "hello".to_string(),
            p_tags: vec![],
            channel_uuid: ch,
        }
    }

    /// Pruned entries (created_at <= ts) are gone; entries above ts are kept.
    #[test]
    fn prune_removes_at_and_below_watermark_keeps_above() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("e1", 100));
        mb.insert(ch, entry("e2", 200));
        mb.insert(ch, entry("e3", 300));

        // Watermark at 200: e1 (100) and e2 (200) must be removed; e3 (300) kept.
        mb.prune_channel_below(&ch, 200);

        let remaining = mb.channel_entries(&ch).unwrap();
        assert_eq!(remaining.len(), 1, "only e3 should remain");
        assert_eq!(remaining[0].event_id, "e3");
    }

    /// Boundary: an entry with created_at == ts is removed (<= semantics).
    #[test]
    fn prune_boundary_exact_ts_is_removed() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("exact", 500));
        mb.insert(ch, entry("above", 501));

        mb.prune_channel_below(&ch, 500);

        let remaining = mb.channel_entries(&ch).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].event_id, "above");
    }

    /// Other channels are not touched by a prune on an unrelated channel.
    #[test]
    fn prune_does_not_touch_other_channels() {
        let mut mb = Mailbox::new();
        let ch_a = Uuid::new_v4();
        let ch_b = Uuid::new_v4();

        mb.insert(ch_a, entry_for("a1", 100, ch_a));
        mb.insert(ch_a, entry_for("a2", 200, ch_a));
        mb.insert(ch_b, entry_for("b1", 100, ch_b));
        mb.insert(ch_b, entry_for("b2", 200, ch_b));

        // Prune ch_a up to ts=200 (removes a1 and a2); ch_b must be untouched.
        mb.prune_channel_below(&ch_a, 200);

        let a_entries = mb.channel_entries(&ch_a).unwrap();
        assert_eq!(a_entries.len(), 0, "ch_a: all entries at/below 200 removed");

        let b_entries = mb.channel_entries(&ch_b).unwrap();
        assert_eq!(
            b_entries.len(),
            2,
            "ch_b must be untouched by prune on ch_a"
        );
    }

    /// Prune on unknown channel is a no-op (no panic).
    #[test]
    fn prune_unknown_channel_is_noop() {
        let mut mb = Mailbox::new();
        let unknown = Uuid::new_v4();
        // Must not panic.
        mb.prune_channel_below(&unknown, 1000);
        assert!(mb.channel_entries(&unknown).is_none());
    }

    /// NON-VACUITY (count-invariant): pruning entries at/below the watermark
    /// does not change the unread count for messages ABOVE the watermark.
    ///
    /// The invariant: pruned entries were already "read" (their created_at is
    /// at or below the watermark). So the unread count (entries with
    /// created_at > watermark) must be the same before and after pruning.
    #[test]
    fn prune_does_not_change_unread_count_above_watermark() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();

        // Insert 5 messages: ts 100..=500 in steps of 100.
        for i in 1u64..=5 {
            mb.insert(ch, entry(&format!("e{i}"), i * 100));
        }

        let watermark = 300u64;

        // Count "unread" = entries with created_at > watermark, BEFORE prune.
        let unread_before = mb
            .channel_entries(&ch)
            .unwrap()
            .iter()
            .filter(|e| e.created_at > watermark)
            .count();

        mb.prune_channel_below(&ch, watermark);

        // Count "unread" = all remaining entries (they are all above watermark
        // since prune removed everything <= watermark).
        let unread_after = mb.channel_entries(&ch).unwrap().len();

        assert_eq!(
            unread_before, unread_after,
            "pruning at/below watermark must not change the unread count above it"
        );
        assert_eq!(unread_after, 2, "e4(400) and e5(500) must remain");
    }

    /// NON-VACUITY mutation proof: if prune used `<` instead of `<=` (wrong bound),
    /// the boundary entry at exactly `watermark` would be kept, making the total
    /// remaining count one higher than expected by the above test.
    ///
    /// This test directly verifies entries below ts are physically gone.
    #[test]
    fn prune_entries_below_ts_are_physically_absent() {
        let mut mb = Mailbox::new();
        let ch = Uuid::new_v4();
        mb.insert(ch, entry("below", 99));
        mb.insert(ch, entry("at", 100));
        mb.insert(ch, entry("above", 101));

        mb.prune_channel_below(&ch, 100);

        let remaining = mb.channel_entries(&ch).unwrap();
        // Only "above" (101) must remain.
        assert_eq!(remaining.len(), 1, "only entry above watermark must remain");
        assert_eq!(remaining[0].event_id, "above");
        // Confirm "below" (99) and "at" (100) are gone.
        assert!(
            !remaining.iter().any(|e| e.event_id == "below"),
            "entry 'below' (ts=99) must be pruned"
        );
        assert!(
            !remaining.iter().any(|e| e.event_id == "at"),
            "entry 'at' (ts=100, == watermark) must be pruned by <= semantics"
        );
    }

    // US-16: Buzz relay is the only inbound persistence. No separate queue module
    // should exist. Reintroducing a queue module turns this test RED.
    #[test]
    fn us16_no_separate_queue_module_in_src() {
        use std::fs;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let src_dir = std::path::Path::new(manifest_dir).join("src");
        let entries = fs::read_dir(&src_dir)
            .expect("should be able to read src/ directory")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "rs").unwrap_or(false))
            .collect::<Vec<_>>();
        // Non-vacuous: confirm we actually found some .rs files.
        assert!(
            !entries.is_empty(),
            "src/ must contain at least one .rs file (vacuity guard)"
        );
        // Patterns that signal a prohibited separate queue module.
        let queue_patterns = [
            "queue",
            "buffer_store",
            "msgqueue",
            "msg_queue",
            "message_queue",
        ];
        for dir_entry in &entries {
            let file_name = dir_entry.file_name().to_string_lossy().to_lowercase();
            for pat in &queue_patterns {
                assert!(
                    !file_name.contains(pat),
                    "Found a queue-like module '{}' in src/. \
                     US-16 requires Buzz relay be the sole inbound persistence; \
                     no separate queue module may exist.",
                    dir_entry.file_name().to_string_lossy()
                );
            }
        }
    }
}
