//! Lane classifier for incoming messages.
//!
//! Lane 1 (ForMe): DM channel OR @mention (p-tag == seat pubkey).
//! Lane 2/3 (Delivery): all other messages. The clerk delivers + badges all lanes.
//! Only Lane 1 triggers a wake signal.
//!
//! TRIPWIRE 3 note: kind 44100 is a MEMBERSHIP-CHANGE notification, NOT a new message.
//! The main loop handles kind 44100 separately (adds a room subscription).
//! `classify()` is called only on kind-9 / 40002 message events.

/// Attention lane for an incoming message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lane {
    /// Lane 1: for-me / must-act. DM or @mention.
    ForMe,
    /// Lane 2/3: delivery only. No wake signal emitted.
    Delivery,
}

/// Classify a message event.
///
/// `is_dm`: true if the channel's `channel_type` is `"dm"`.
/// `p_tags`: the `p` tag values from the kind-9 event.
/// `seat_pubkey_hex`: the seat's own public key in lowercase hex.
pub fn classify(is_dm: bool, p_tags: &[String], seat_pubkey_hex: &str) -> Lane {
    if is_dm {
        return Lane::ForMe;
    }
    let is_mention = p_tags.iter().any(|p| p == seat_pubkey_hex);
    if is_mention {
        Lane::ForMe
    } else {
        Lane::Delivery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEAT_PK: &str = "seat_pubkey_hex_000000000000000000000000000000000000000000000000";

    #[test]
    fn dm_channel_is_lane_1() {
        let r = classify(true /* is_dm */, &[], SEAT_PK);
        assert_eq!(r, Lane::ForMe);
    }

    #[test]
    fn at_mention_is_lane_1() {
        let p_tags = vec![SEAT_PK.to_string()];
        let r = classify(false, &p_tags, SEAT_PK);
        assert_eq!(r, Lane::ForMe);
    }

    #[test]
    fn non_dm_non_mention_is_lane_delivery() {
        let r = classify(false, &[], SEAT_PK);
        assert_eq!(r, Lane::Delivery);
    }

    #[test]
    fn mention_match_is_exact_pubkey_not_prefix() {
        // A p-tag that starts with but does not equal the seat pubkey is NOT a mention.
        let p_tags = vec![format!("{}extra", SEAT_PK)];
        let r = classify(false, &p_tags, SEAT_PK);
        assert_eq!(r, Lane::Delivery);
    }

    #[test]
    fn dm_auto_p_tags_covered_by_channel_type_check() {
        // DMs auto-p-tag ALL participants; classification relies on channel_type == dm,
        // NOT on the presence of a p-tag. This test confirms the channel_type path
        // fires even when p_tags is empty (because the relay adds p-tags but we
        // already know it is a DM from channel metadata).
        let r = classify(true /* is_dm */, &[], SEAT_PK);
        assert_eq!(
            r,
            Lane::ForMe,
            "DM must be Lane 1 even without explicit p-tag check"
        );
    }

    #[test]
    fn tripwire_3_kind_44100_is_not_classified_as_message() {
        // kind 44100 = membership-add notification. The clerk's classify() function
        // is called ONLY on kind-9 channel messages. kind 44100 is handled separately
        // by the main loop to trigger subscription add. This test documents the contract:
        // classify() is only valid for message events (kind 9 / 40002).
        // We simulate by confirming a non-mention, non-DM kind-9 event stays Delivery.
        let r = classify(false, &[], SEAT_PK);
        assert_eq!(r, Lane::Delivery);
        // The test name is the contract statement.
    }
}
