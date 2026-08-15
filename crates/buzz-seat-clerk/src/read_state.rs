//! kind:30078 read-state writer.
//!
//! Ports `desktop/src/features/channels/readState/readStateManager.ts` (publish path)
//! and `readStateIdentity.ts` to Rust.
//!
//! Content format (ported from `readStateFormat.ts`):
//!   `{"v":1,"client_id":"<str>","contexts":{"<ctx>":<unix_secs>}}`
//! where `<ctx>` is a channel UUID string, or `"thread:<64hex>"`, or `"msg:<64hex>"`.
//!
//! Tags: `["d","read-state:<32-lowercase-hex slot>"]` + `["t","read-state"]`.
//! `created_at = max(now_secs, last_written_created_at + 1)` -- monotonic per slot.
//! 5-second debounce: callers call `mark_read()` then `flush_if_due()`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ClerkError;
use buzz_core::kind::KIND_READ_STATE;

const DEBOUNCE_SECS: u64 = 5;
/// v1 uses only slot 0; multi-slot is future work.
#[allow(dead_code)]
const MAX_SLOTS: usize = 8;

/// On-disk identity: slot_id (32 lowercase hex) + client_id (arbitrary string).
/// Replaces desktop's localStorage.
#[derive(Debug, Serialize, Deserialize)]
pub struct SlotIdentity {
    pub slot_id: String,
    pub client_id: String,
}

/// Generate a slot ID: 16 random bytes = 32 lowercase hex characters.
pub fn generate_slot_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl SlotIdentity {
    /// Load from disk, or generate fresh and persist.
    pub fn load_or_create(path: &Path) -> Result<Self, ClerkError> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let id: Self = serde_json::from_str(&raw)?;
            return Ok(id);
        }
        let id = Self {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(), // reuse generator; different random value
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(&id)?)?;
        Ok(id)
    }
}

/// Build the plaintext JSON blob for the kind-30078 content field.
pub fn build_read_state_plaintext(
    client_id: &str,
    contexts: &HashMap<String, u64>,
) -> Result<String, ClerkError> {
    let blob = json!({
        "v": 1,
        "client_id": client_id,
        "contexts": contexts
    });
    Ok(serde_json::to_string(&blob)?)
}

/// Stateful writer for kind-30078 read-state events.
pub struct ReadStateWriter {
    pub identity: SlotIdentity,
    /// Max created_at successfully sent for this slot (monotonic watermark).
    last_written_created_at: u64,
    /// Pending context updates not yet flushed.
    pending_contexts: HashMap<String, u64>,
    /// Wall-clock of the last flush (for 5-second debounce).
    last_flush_wall: u64,
}

impl ReadStateWriter {
    pub fn new(identity: SlotIdentity) -> Self {
        Self {
            identity,
            last_written_created_at: 0,
            pending_contexts: HashMap::new(),
            last_flush_wall: 0,
        }
    }

    /// Record a context as read. `ctx` is a channel UUID, `"thread:<hex>"`, or `"msg:<hex>"`.
    pub fn mark_read(&mut self, ctx: String, ts: u64) {
        let entry = self.pending_contexts.entry(ctx).or_insert(0);
        if ts > *entry {
            *entry = ts;
        }
    }

    /// Compute the `created_at` for the next write.
    ///
    /// Invariant: always strictly greater than `last_written_created_at`.
    /// This satisfies TRIPWIRE 1: relay hard-deletes superseded slot events and
    /// keeps a watermark; a stale `created_at` is silently rejected.
    pub fn next_created_at(&mut self, now_secs: u64) -> u64 {
        let at = now_secs.max(self.last_written_created_at + 1);
        self.last_written_created_at = at;
        at
    }

    /// Returns true if the debounce period has elapsed and there are pending contexts.
    pub fn is_flush_due(&self, now_secs: u64) -> bool {
        !self.pending_contexts.is_empty() && now_secs >= self.last_flush_wall + DEBOUNCE_SECS
    }

    /// Build a signed kind-30078 event from pending contexts.
    ///
    /// Caller provides `now_secs` (wall clock) and the seat `keys` for signing + NIP-44.
    /// On success, clears `pending_contexts` and updates `last_flush_wall`.
    ///
    /// SECURITY: do NOT log `plaintext` or `keys.secret_key()` -- they are sensitive.
    pub fn build_event(&mut self, now_secs: u64, keys: &Keys) -> Result<Event, ClerkError> {
        let plaintext =
            build_read_state_plaintext(&self.identity.client_id, &self.pending_contexts)?;

        // NIP-44 encrypt-to-self: use our own pubkey as the "other" party.
        // ECDH(seckey, our_pubkey) is symmetric with the same key pair.
        let ciphertext = nostr::nips::nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .map_err(|e| ClerkError::Nip44(e.to_string()))?;

        let d_tag_value = format!("read-state:{}", self.identity.slot_id);
        let tags = vec![
            Tag::parse(vec!["d".to_owned(), d_tag_value])
                .map_err(|e| ClerkError::ReadStateWrite(e.to_string()))?,
            Tag::parse(vec!["t".to_owned(), "read-state".to_owned()])
                .map_err(|e| ClerkError::ReadStateWrite(e.to_string()))?,
        ];

        let created_at = self.next_created_at(now_secs);

        let event = EventBuilder::new(Kind::Custom(KIND_READ_STATE as u16), ciphertext)
            .tags(tags)
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(keys)
            .map_err(|e| ClerkError::ReadStateWrite(e.to_string()))?;

        self.last_flush_wall = now_secs;
        self.pending_contexts.clear();
        tracing::debug!(slot_id = %self.identity.slot_id, created_at, "read-state event built");
        Ok(event)
    }
}

/// Current wall-clock seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;
    use tempfile::tempdir;

    fn test_keys() -> Keys {
        Keys::generate()
    }

    #[test]
    fn identity_created_and_persisted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let id1 = SlotIdentity::load_or_create(&path).unwrap();
        let id2 = SlotIdentity::load_or_create(&path).unwrap();
        // Same file -> same slot_id and client_id.
        assert_eq!(id1.slot_id, id2.slot_id);
        assert_eq!(id1.client_id, id2.client_id);
        assert_eq!(
            id1.slot_id.len(),
            32,
            "slot_id must be 32 lowercase hex chars"
        );
        assert!(
            id1.slot_id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "slot_id must be lowercase hex"
        );
    }

    #[test]
    fn d_tag_format_matches_relay_validation() {
        // Relay rule: d tag value must match "read-state:" + 32 lowercase hex.
        let dir = tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let id = SlotIdentity::load_or_create(&path).unwrap();
        let d_tag_value = format!("read-state:{}", id.slot_id);
        assert!(d_tag_value.starts_with("read-state:"));
        let slot_part = &d_tag_value["read-state:".len()..];
        assert_eq!(slot_part.len(), 32);
        assert!(slot_part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn build_read_state_content_is_valid_json_shape() {
        let _keys = test_keys();
        let mut contexts = HashMap::new();
        contexts.insert("some-channel-uuid".to_string(), 1_700_000_000u64);
        let content = build_read_state_plaintext("my-client-id", &contexts).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["client_id"], "my-client-id");
        assert_eq!(parsed["contexts"]["some-channel-uuid"], 1_700_000_000u64);
    }

    #[test]
    fn monotonic_created_at_tripwire() {
        // TRIPWIRE 1: second write must have created_at > first even with frozen clock.
        let dir = tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let id = SlotIdentity::load_or_create(&path).unwrap();
        let mut writer = ReadStateWriter::new(id);

        // Simulate two writes at the same wall-clock second.
        let t = 1_700_000_000u64;
        let created_at_1 = writer.next_created_at(t);
        let created_at_2 = writer.next_created_at(t); // same wall-clock
        assert!(
            created_at_2 > created_at_1,
            "created_at must be strictly increasing: got {created_at_1} then {created_at_2}"
        );
    }

    #[test]
    fn relay_conformance_d_tag_exactly_32_hex() {
        // Relay buzz-db/src/lib.rs: d tag slot must be exactly 32 lowercase hex.
        let slot = generate_slot_id();
        assert_eq!(slot.len(), 32);
        assert!(slot
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn encrypt_to_self_round_trips() {
        // NIP-44 self-encrypt: ECDH(seckey, own_pubkey) decrypts with the same key pair.
        let keys = test_keys();
        let plaintext = r#"{"v":1,"client_id":"test","contexts":{"ch-abc":1700000000}}"#;
        let ciphertext = nostr::nips::nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .expect("encrypt-to-self must succeed");

        let recovered =
            nostr::nips::nip44::decrypt(keys.secret_key(), &keys.public_key(), &ciphertext)
                .expect("decrypt-to-self must succeed");

        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn build_event_tag_conformance() {
        // Tags: exactly one d-tag with prefix "read-state:" + 32 hex, one t-tag "read-state".
        let dir = tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let id = SlotIdentity::load_or_create(&path).unwrap();
        let mut writer = ReadStateWriter::new(id);
        writer.mark_read("some-ctx".to_string(), 1_700_000_000);

        let keys = test_keys();
        let event = writer.build_event(1_700_000_000, &keys).unwrap();

        // Kind must be 30078.
        assert_eq!(event.kind.as_u16(), KIND_READ_STATE as u16);

        // Collect d-tags and t-tags.
        let d_tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.kind().to_string() == "d")
            .collect();
        let t_tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.kind().to_string() == "t")
            .collect();

        assert_eq!(d_tags.len(), 1, "exactly one d-tag");
        assert_eq!(t_tags.len(), 1, "exactly one t-tag");

        let d_val = d_tags[0].content().unwrap_or("");
        assert!(
            d_val.starts_with("read-state:"),
            "d-tag must start with read-state:"
        );
        let slot_part = &d_val["read-state:".len()..];
        assert_eq!(slot_part.len(), 32, "slot must be 32 chars");
        assert!(
            slot_part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "slot must be lowercase hex"
        );

        let t_val = t_tags[0].content().unwrap_or("");
        assert_eq!(t_val, "read-state", "t-tag value must be 'read-state'");
    }
}
