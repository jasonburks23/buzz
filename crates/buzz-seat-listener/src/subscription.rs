//! REQ frame builders and two-generation dedup.
//!
//! Note on kind 44100 (KIND_MEMBER_ADDED_NOTIFICATION): this fires only on
//! MEMBERSHIP change (new DM room created, seat added to a room). It does NOT
//! fire on every new message. In-room message wake rides the open #h subscription.

use std::collections::HashSet;

use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use serde_json::{json, Value};
use uuid::Uuid;

/// Build a REQ frame for the global membership subscription (kinds 44100/44101).
///
/// `since`: Unix timestamp; relay sends events created after this timestamp.
pub fn membership_req_frame(sub_id: &str, seat_pubkey_hex: &str, since: u64) -> Value {
    json!([
        "REQ",
        sub_id,
        {
            "kinds": [KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION],
            "#p": [seat_pubkey_hex],
            "since": since
        }
    ])
}

/// Build a REQ frame for a single room (subscribes to all channel message kinds).
///
/// The clerk subscribes to kinds 9 and 40002 (V2) so it catches both legacy and
/// current message events. Extend this list if upstream adds new channel kinds.
pub fn channel_req_frame(sub_id: &str, channel_uuid: &Uuid, since: u64) -> Value {
    json!([
        "REQ",
        sub_id,
        {
            "kinds": [
                KIND_STREAM_MESSAGE,    // 9
                KIND_STREAM_MESSAGE_V2, // 40002
            ],
            "#h": [channel_uuid.to_string()],
            "since": since
        }
    ])
}

// BORROW-OPPORTUNITY(future): TwoGenDedup duplicates a private struct in
// buzz-acp. Cannot borrow yet (no public upstream export). Keep this copy
// until buzz-acp exposes it as a library type.
/// Two-generation in-memory dedup for event IDs.
///
/// Mirrors the buzz-acp pattern (relay.rs:966). Keeps two generations of seen
/// IDs; when the new generation reaches `capacity`, it becomes the old generation
/// and a fresh new generation starts. This bounds memory while covering a sliding
/// window of recent IDs.
pub struct TwoGenDedup {
    capacity: usize,
    old_gen: HashSet<String>,
    new_gen: HashSet<String>,
}

impl TwoGenDedup {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            old_gen: HashSet::new(),
            new_gen: HashSet::new(),
        }
    }

    /// Returns `true` if this `id` has NOT been seen before (i.e. it is new).
    /// Inserts the id into the current generation.
    pub fn is_new(&mut self, id: &str) -> bool {
        if self.old_gen.contains(id) || self.new_gen.contains(id) {
            return false;
        }
        self.new_gen.insert(id.to_string());
        if self.new_gen.len() >= self.capacity {
            self.old_gen = std::mem::take(&mut self.new_gen);
        }
        true
    }
}

/// Extract the room UUID carried in the `h` tag of a kind-44100 (member-added) event.
///
/// A kind 44100 fires only on a MEMBERSHIP change (added to a room/DM), not per message.
/// Returns `None` if the `h` tag is absent or its value is not a valid UUID; the caller
/// treats `None` as a no-op (malformed relay event).
pub fn room_uuid_from_live_add(event: &Value) -> Option<Uuid> {
    event.get("tags")?.as_array()?.iter().find_map(|tag| {
        let a = tag.as_array()?;
        if a.first()?.as_str()? == "h" {
            a.get(1)?.as_str()?.parse::<Uuid>().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn membership_req_frame_contains_correct_kinds() {
        let pubkey = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let frame = membership_req_frame("sub-1", pubkey, 0);
        // frame is ["REQ", "sub-1", {kinds:[44100,44101],"#p":[pubkey],"since":0}]
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], "sub-1");
        let filter = &frame[2];
        let kinds = filter["kinds"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k == 44100));
        assert!(kinds.iter().any(|k| k == 44101));
        let p_tags = filter["#p"].as_array().unwrap();
        assert_eq!(p_tags[0], pubkey);
    }

    #[test]
    fn channel_req_frame_uses_h_tag() {
        let uuid = Uuid::new_v4();
        let frame = channel_req_frame("sub-ch", &uuid, 0);
        assert_eq!(frame[0], "REQ");
        let filter = &frame[2];
        let h_tags = filter["#h"].as_array().unwrap();
        assert_eq!(h_tags[0], uuid.to_string());
        // must include kind 9 (stream message)
        let kinds = filter["kinds"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k == 9));
    }

    #[test]
    fn two_gen_dedup_accepts_new_event_ids() {
        let mut dedup = TwoGenDedup::new(3);
        assert!(dedup.is_new("aaa"));
        assert!(dedup.is_new("bbb"));
        assert!(!dedup.is_new("aaa")); // duplicate
    }

    #[test]
    fn two_gen_dedup_evicts_oldest_gen_at_capacity() {
        let mut dedup = TwoGenDedup::new(2);
        dedup.is_new("aaa");
        dedup.is_new("bbb");
        // Adding a third triggers eviction of gen-0 -> gen-0 becomes old gen.
        dedup.is_new("ccc");
        // "aaa" and "bbb" are now in the old gen; "ccc" is new gen.
        // A duplicate of "aaa" should be seen as duplicate (old gen still covers it).
        assert!(!dedup.is_new("aaa"));
    }

    #[test]
    fn kind_44100_is_membership_add() {
        // Ensure we are testing the right constant.
        assert_eq!(buzz_core::kind::KIND_MEMBER_ADDED_NOTIFICATION, 44100);
        assert_eq!(buzz_core::kind::KIND_MEMBER_REMOVED_NOTIFICATION, 44101);
    }

    #[test]
    fn room_uuid_from_live_add_found() {
        let event = serde_json::json!({
            "kind": 44100,
            "tags": [
                ["p", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"],
                ["h", "550e8400-e29b-41d4-a716-446655440000"],
                ["extra", "ignored"]
            ]
        });
        let uuid = room_uuid_from_live_add(&event);
        assert_eq!(
            uuid,
            Some(
                "550e8400-e29b-41d4-a716-446655440000"
                    .parse::<uuid::Uuid>()
                    .unwrap()
            )
        );
    }

    #[test]
    fn room_uuid_from_live_add_missing_or_malformed() {
        // No h-tag at all
        let no_h = serde_json::json!({
            "kind": 44100,
            "tags": [["p", "deadbeef"]]
        });
        assert_eq!(room_uuid_from_live_add(&no_h), None);

        // h-tag present but value is not a UUID
        let bad_uuid = serde_json::json!({
            "kind": 44100,
            "tags": [["h", "not-a-uuid"]]
        });
        assert_eq!(room_uuid_from_live_add(&bad_uuid), None);
    }
}
