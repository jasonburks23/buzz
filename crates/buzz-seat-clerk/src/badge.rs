//! Unread badge computation.
//!
//! Ports `desktop/src/features/channels/unreadChannelCounts.ts`.
//!
//! `total_unread`: events with `created_at > read_at`.
//! `badge_unread`: subset that "counts toward badge" -- DM channels or @mentions.

use crate::mailbox::MailboxEntry;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::MailboxEntry;
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
}
