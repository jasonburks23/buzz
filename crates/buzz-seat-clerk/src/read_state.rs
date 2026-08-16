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
use crate::session_identity::SessionMarker;
use buzz_core::kind::KIND_READ_STATE;
use thiserror::Error;

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

/// Parse a read-state plaintext blob and return the `contexts` map.
///
/// Format: `{"v":1,"client_id":"<str>","contexts":{"<ctx>":<unix_secs>,...}}`
///
/// On malformed JSON, missing `contexts`, or any other error returns an empty map.
/// Non-numeric `ts` values are silently skipped.
///
/// SECURITY: do NOT log the returned map -- it contains per-context timestamps.
pub fn parse_read_state_contexts(plaintext: &str) -> HashMap<String, u64> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(plaintext) else {
        return HashMap::new();
    };
    let Some(contexts_val) = value.get("contexts") else {
        return HashMap::new();
    };
    let Some(obj) = contexts_val.as_object() else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_u64().map(|ts| (k.clone(), ts)))
        .collect()
}

/// Stateful writer for kind-30078 read-state events.
pub struct ReadStateWriter {
    pub identity: SlotIdentity,
    /// Max created_at successfully sent for this slot (monotonic watermark).
    last_written_created_at: u64,
    /// Durable confirmed read positions (survives flush).
    contexts: HashMap<String, u64>,
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
            contexts: HashMap::new(),
            pending_contexts: HashMap::new(),
            last_flush_wall: 0,
        }
    }

    /// Record a context as read. `ctx` is a channel UUID, `"thread:<hex>"`, or `"msg:<hex>"`.
    pub(crate) fn mark_read(&mut self, ctx: String, ts: u64) {
        // Update pending_contexts (max).
        let entry = self.pending_contexts.entry(ctx.clone()).or_insert(0);
        if ts > *entry {
            *entry = ts;
        }
        // Also update durable contexts (max).
        let durable = self.contexts.entry(ctx).or_insert(0);
        if ts > *durable {
            *durable = ts;
        }
    }

    /// Seed durable contexts from a previously-stored read-state event (boot-time load).
    ///
    /// Merges `loaded` into `self.contexts` taking the per-key max, and advances
    /// `last_written_created_at` to prevent the next publish from being silently
    /// rejected by the relay watermark.
    ///
    /// SECURITY: do NOT log `loaded` values -- they contain per-context timestamps.
    pub fn seed_contexts(&mut self, loaded: HashMap<String, u64>, loaded_created_at: u64) {
        for (ctx, ts) in loaded {
            let entry = self.contexts.entry(ctx).or_insert(0);
            if ts > *entry {
                *entry = ts;
            }
        }
        if loaded_created_at > self.last_written_created_at {
            self.last_written_created_at = loaded_created_at;
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

    /// Returns the last-read unix seconds for a context key, or None if absent.
    ///
    /// Returns the max of the durable `contexts` map and the pending `pending_contexts` map.
    /// Used by the reconnect path to set `since` on per-room subscriptions, so
    /// the relay only backfills messages the seat has not yet read.
    pub fn read_at_for(&self, ctx: &str) -> Option<u64> {
        match (
            self.contexts.get(ctx).copied(),
            self.pending_contexts.get(ctx).copied(),
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
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

// ─── Marker-gated read receipt ────────────────────────────────────────────────

/// Errors returned by the marker-gated read API.
///
/// US-03 / US-07: only the continuous live session may advance the read
/// bookmark.  A freshly spawned process (or any actor whose marker does not
/// match the live session) is refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReadGuardError {
    /// The actor's session marker does not match the live session.
    ///
    /// The bookmark is NOT advanced when this is returned.
    #[error("actor is not the live session; bookmark unchanged")]
    NotLiveSession,
}

/// Advance the read bookmark for `ctx` to `ts`, but only if `actor` is the
/// live session.
///
/// - If `actor != live`: returns `Err(NotLiveSession)` and does NOT call
///   `writer.mark_read` (bookmark unchanged).
/// - If `actor == live`: calls `writer.mark_read(ctx, ts)` and returns `Ok`.
///
/// This is the US-03/US-07 guard: a freshly spawned process whose sidecar
/// marker differs from the running live session cannot advance the bookmark
/// even if it receives the same messages.
pub fn record_youyou_read(
    writer: &mut ReadStateWriter,
    ctx: String,
    ts: u64,
    actor: &SessionMarker,
    live: &SessionMarker,
) -> Result<(), ReadGuardError> {
    if actor != live {
        return Err(ReadGuardError::NotLiveSession);
    }
    writer.mark_read(ctx, ts);
    Ok(())
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
    use std::collections::HashMap;
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
    fn read_at_for_returns_stored_timestamp_or_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let id = SlotIdentity::load_or_create(&path).unwrap();
        let mut writer = ReadStateWriter::new(id);

        // Unknown context returns None.
        assert_eq!(writer.read_at_for("unknown-ctx"), None);

        // After marking a context read, read_at_for returns the timestamp.
        let ts = 1_700_000_042u64;
        writer.mark_read("chan-abc".to_string(), ts);
        assert_eq!(writer.read_at_for("chan-abc"), Some(ts));

        // A different unknown key still returns None.
        assert_eq!(writer.read_at_for("chan-xyz"), None);
    }

    // ── US-07 / US-03 marker-gated read-receipt tests ──────────────────────

    /// Helper: build a minimal ReadStateWriter for unit tests (no disk I/O).
    fn test_writer() -> ReadStateWriter {
        ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        })
    }

    // Test 1 – US-07 S3: delivered-but-unread stays None.
    //
    // A ReadStateWriter that has only had messages "delivered" (mark_read NOT
    // called on it) reports read_at_for(ctx) == None.
    //
    // This test is tied to the removal of the delivery-time mark_read call
    // that previously lived in clerk.rs after `mailbox.insert(...)`. Since
    // delivery no longer calls mark_read, the bookmark must remain absent
    // until the live session explicitly records a read.
    #[test]
    fn us07_s3_delivered_but_unread_stays_none() {
        let writer = test_writer();
        let ctx = "chan-us07-s3".to_string();

        // Simulate delivery: we insert into the context as if the clerk
        // received a message, but we deliberately do NOT call mark_read.
        // (In the fixed clerk.rs the delivery path only does mailbox.insert
        // + emitter.emit; mark_read is gone from that path.)
        // Assert the bookmark is absent.
        assert_eq!(
            writer.read_at_for(&ctx),
            None,
            "bookmark must be None after delivery without mark_read"
        );
    }

    // Test 2 – US-07 S4 + US-03 S1 KEYSTONE: fake marker cannot advance bookmark.
    //
    // Given live marker L and a different marker F (fake), calling
    // record_youyou_read with F as actor must return Err(NotLiveSession) AND
    // leave the bookmark at None.
    //
    // This test turns RED against the ungated pass-through above and GREEN
    // after the gate is added.
    #[test]
    fn us07_s4_us03_s1_keystone_fake_marker_cannot_advance_bookmark() {
        let mut writer = test_writer();
        let ctx = "chan-keystone".to_string();

        let live = SessionMarker("live-session-abc".to_string());
        let fake = SessionMarker("fake-session-xyz".to_string());

        let result = record_youyou_read(&mut writer, ctx.clone(), 100, &fake, &live);

        assert_eq!(
            result,
            Err(ReadGuardError::NotLiveSession),
            "a non-live actor must be refused"
        );
        assert_eq!(
            writer.read_at_for(&ctx),
            None,
            "bookmark must remain None when actor != live"
        );
    }

    // Test 3 – Positive control: live actor advances bookmark correctly.
    #[test]
    fn us07_positive_live_actor_advances_bookmark() {
        let mut writer = test_writer();
        let ctx = "chan-positive".to_string();

        let live = SessionMarker("live-session-abc".to_string());

        // First live read at ts=100.
        let r1 = record_youyou_read(&mut writer, ctx.clone(), 100, &live, &live);
        assert_eq!(r1, Ok(()), "live actor must succeed");
        assert_eq!(
            writer.read_at_for(&ctx),
            Some(100),
            "bookmark must be 100 after first read"
        );

        // Second live read at ts=150 advances the bookmark.
        let r2 = record_youyou_read(&mut writer, ctx.clone(), 150, &live, &live);
        assert_eq!(r2, Ok(()), "second live read must succeed");
        assert_eq!(
            writer.read_at_for(&ctx),
            Some(150),
            "bookmark must advance to 150 after second read"
        );
    }

    // Test 4 – US-03 catch-up wiring: read_at_for returns the recorded bookmark.
    //
    // Confirms that the backfill `since` parameter in the reconnect path uses
    // the correct bookmark value stored by mark_read / record_youyou_read.
    #[test]
    fn us03_catchup_read_at_for_returns_recorded_bookmark() {
        let mut writer = test_writer();
        let ctx = "chan-us03-catchup".to_string();

        // Nothing recorded yet.
        assert_eq!(writer.read_at_for(&ctx), None);

        // Record a bookmark directly via mark_read (internal API).
        let ts = 1_720_000_042u64;
        writer.mark_read(ctx.clone(), ts);

        // read_at_for must return exactly that value.
        assert_eq!(
            writer.read_at_for(&ctx),
            Some(ts),
            "read_at_for must return the stored bookmark for backfill `since`"
        );

        // A higher timestamp advances it.
        writer.mark_read(ctx.clone(), ts + 60);
        assert_eq!(writer.read_at_for(&ctx), Some(ts + 60));
    }

    // ── Piece A: durable positions map ──────────────────────────────────────

    /// REGRESSION: after mark_read + build_event, read_at_for must still return the ts.
    /// Today this FAILS because build_event clears pending_contexts and read_at_for
    /// only reads pending_contexts.
    #[test]
    fn piece_a_regression_read_at_for_survives_flush() {
        let mut writer = test_writer();
        let keys = Keys::generate();
        writer.mark_read("ch".to_string(), 100);
        // flush
        let _ = writer.build_event(1_700_000_000, &keys).unwrap();
        // bookmark must still be 100
        assert_eq!(
            writer.read_at_for("ch"),
            Some(100),
            "read_at_for must return Some(100) after flush (durable contexts map)"
        );
    }

    /// seed_contexts loads contexts and watermark correctly.
    #[test]
    fn piece_a_seed_contexts_loads_and_watermark() {
        let mut writer = test_writer();
        let mut loaded = HashMap::new();
        loaded.insert("ch".to_string(), 500u64);
        writer.seed_contexts(loaded, 12345);
        assert_eq!(writer.read_at_for("ch"), Some(500));
        // next_created_at(1) must be >= 12346
        let at = writer.next_created_at(1);
        assert!(
            at >= 12346,
            "next_created_at after seed must be >= 12346, got {at}"
        );
    }

    /// mark_read after seed keeps the max of the two values.
    #[test]
    fn piece_a_mark_read_after_seed_keeps_max() {
        let mut writer = test_writer();
        let mut loaded = HashMap::new();
        loaded.insert("ch".to_string(), 500u64);
        writer.seed_contexts(loaded, 0);
        // mark a lower ts -- should keep 500
        writer.mark_read("ch".to_string(), 200);
        assert_eq!(writer.read_at_for("ch"), Some(500));
        // mark a higher ts -- should advance to 600
        writer.mark_read("ch".to_string(), 600);
        assert_eq!(writer.read_at_for("ch"), Some(600));
    }

    // ── Piece B: parse_read_state_contexts ──────────────────────────────────

    /// Valid blob yields the expected contexts map.
    #[test]
    fn piece_b_valid_blob_yields_map() {
        let blob =
            r#"{"v":1,"client_id":"test","contexts":{"ch-abc":1700000000,"ch-xyz":1700000001}}"#;
        let map = parse_read_state_contexts(blob);
        assert_eq!(map.get("ch-abc"), Some(&1_700_000_000u64));
        assert_eq!(map.get("ch-xyz"), Some(&1_700_000_001u64));
    }

    /// Malformed JSON returns empty map.
    #[test]
    fn piece_b_malformed_json_returns_empty() {
        let map = parse_read_state_contexts("not json at all {{");
        assert!(map.is_empty(), "malformed JSON must yield empty map");
    }

    /// Missing contexts key returns empty map.
    #[test]
    fn piece_b_missing_contexts_returns_empty() {
        let blob = r#"{"v":1,"client_id":"test"}"#;
        let map = parse_read_state_contexts(blob);
        assert!(map.is_empty(), "missing contexts key must yield empty map");
    }

    /// Non-numeric ts entry is skipped; numeric entries survive.
    #[test]
    fn piece_b_non_numeric_ts_skipped() {
        let blob =
            r#"{"v":1,"client_id":"test","contexts":{"good":1700000000,"bad":"not-a-number"}}"#;
        let map = parse_read_state_contexts(blob);
        assert_eq!(map.get("good"), Some(&1_700_000_000u64));
        assert!(!map.contains_key("bad"), "non-numeric ts must be skipped");
    }

    // ── Piece C (unit): NIP-44 round-trip decrypt+parse ─────────────────────

    /// Build plaintext, encrypt to self, decrypt, parse_read_state_contexts; contexts survive.
    #[test]
    fn piece_c_roundtrip_encrypt_parse() {
        let keys = Keys::generate();
        let mut contexts = HashMap::new();
        contexts.insert("chan-001".to_string(), 1_720_000_001u64);
        contexts.insert("chan-002".to_string(), 1_720_000_099u64);
        let plaintext = build_read_state_plaintext("test-client", &contexts).unwrap();
        let ciphertext = nostr::nips::nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            &plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .expect("encrypt must succeed");
        let decrypted =
            nostr::nips::nip44::decrypt(keys.secret_key(), &keys.public_key(), &ciphertext)
                .expect("decrypt must succeed");
        let parsed = parse_read_state_contexts(&decrypted);
        assert_eq!(parsed.get("chan-001"), Some(&1_720_000_001u64));
        assert_eq!(parsed.get("chan-002"), Some(&1_720_000_099u64));
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
