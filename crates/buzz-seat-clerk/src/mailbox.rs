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
}
