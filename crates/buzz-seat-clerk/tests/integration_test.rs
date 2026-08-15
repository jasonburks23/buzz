//! Live integration tests for buzz-seat-clerk.
//!
//! These tests require a running Buzz relay on port 3099. They are gated
//! with `#[ignore]` so `cargo test -p buzz-seat-clerk` stays green without
//! any infrastructure.
//!
//! # How to run
//!
//! 1. Start the isolated test relay (see relay-compose-test.yml in this crate's
//!    support/ directory):
//!
//!    ```sh
//!    docker compose \
//!      -f crates/buzz-seat-clerk/support/relay-compose-test.yml \
//!      -p buzz-clerk-it \
//!      up -d
//!    ```
//!
//!    Wait for the relay to report healthy, then run migrations:
//!
//!    ```sh
//!    DATABASE_URL=postgres://buzz:buzz@localhost:5499/buzz_test \
//!      cargo run -p buzz-admin -- migrate
//!    ```
//!
//! 2. Run the ignored tests:
//!
//!    ```sh
//!    TEST_RELAY_URL=ws://localhost:3099 \
//!    SEAT_NSEC=<a fresh nsec> \
//!    RELAY_URL=ws://localhost:3099 \
//!      cargo test -p buzz-seat-clerk -- --ignored
//!    ```
//!
//! # Safety contract
//!
//! HARD RULE: these tests MUST point at port 3099, never at the pilot relay on
//! port 3000 (buzz-prod-relay-1). The env var TEST_RELAY_URL defaults to
//! ws://localhost:3099 and must not be overridden to port 3000.
//!
//! NEVER log or assert on raw secret-key bytes or plaintext DM content.

use buzz_seat_clerk::{
    lane::{classify, Lane},
    mailbox::{Mailbox, MailboxEntry},
    read_state::{
        build_read_state_plaintext, now_secs, record_youyou_read, ReadStateWriter, SlotIdentity,
    },
    session_identity::SessionMarker,
    wake::WakeEmitter,
};
use buzz_ws_client::NostrWsConnection;
use nostr::{EventBuilder, Keys, Kind};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::tempdir;
use uuid::Uuid;

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/// Returns the test relay URL. Defaults to ws://localhost:3099.
/// Never override to port 3000 (that is the live pilot relay).
fn relay_url() -> String {
    let url = std::env::var("TEST_RELAY_URL").unwrap_or_else(|_| "ws://localhost:3099".into());
    assert!(
        !url.contains(":3000"),
        "TEST_RELAY_URL must not point at port 3000 (the pilot relay). Got: {url}"
    );
    url
}

/// Generate a fresh ephemeral Keys pair. Key bytes are never logged.
fn ephemeral_keys() -> Keys {
    Keys::generate()
}

// --------------------------------------------------------------------------
// Test 1: NIP-42 auth handshake
// --------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn auth_against_live_relay() {
    // Verifies that a fresh seat keypair can connect to the isolated relay
    // and complete NIP-42 authentication within the default timeout.
    let seat_keys = ephemeral_keys();
    let conn = NostrWsConnection::connect_authenticated(&relay_url(), &seat_keys, None).await;
    assert!(conn.is_ok(), "NIP-42 auth must succeed: {:?}", conn.err());
    // Gracefully close.
    if let Ok(c) = conn {
        let _ = c.disconnect().await;
    }
}

// --------------------------------------------------------------------------
// Test 2: read-state round-trip
// --------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn read_state_round_trip() {
    // Verifies that a kind-30078 read-state event built by ReadStateWriter is
    // accepted by the relay (OK accepted = true).
    let seat_keys = ephemeral_keys();
    let dir = tempdir().unwrap();
    let id = SlotIdentity::load_or_create(&dir.path().join("id.json")).unwrap();
    let mut writer = ReadStateWriter::new(id);
    let live = SessionMarker::new("it-live-session".to_string());
    record_youyou_read(
        &mut writer,
        "test-context-key".to_string(),
        now_secs(),
        &live,
        &live,
    )
    .expect("live marker must advance bookmark");

    let mut conn = NostrWsConnection::connect_authenticated(&relay_url(), &seat_keys, None)
        .await
        .expect("connect to test relay");

    let event = writer
        .build_event(now_secs(), &seat_keys)
        .expect("build event");
    let ok = conn.send_event(event).await.expect("send kind-30078 event");
    assert!(
        ok.accepted,
        "relay must accept kind:30078 read-state event: {}",
        ok.message
    );

    let _ = conn.disconnect().await;
}

// --------------------------------------------------------------------------
// Test 3: End-to-end dispatch and wake
// --------------------------------------------------------------------------

/// Collect events from a subscription until EOSE or timeout.
///
/// Returns all kind-9 events received before end-of-stored-events.
/// Times out after `timeout_dur` waiting for each next message.
async fn drain_until_eose(
    conn: &mut NostrWsConnection,
    sub_id: &str,
    timeout_dur: Duration,
) -> Vec<nostr::Event> {
    let mut events = Vec::new();
    loop {
        match conn.next_event(timeout_dur).await {
            Ok(buzz_ws_client::RelayMessage::Event {
                subscription_id,
                event,
            }) if subscription_id == sub_id => {
                events.push(*event);
            }
            Ok(buzz_ws_client::RelayMessage::Eose { subscription_id })
                if subscription_id == sub_id =>
            {
                break;
            }
            Ok(_) => {
                // Other relay messages (notices, auth confirmations, etc.) -- skip.
                continue;
            }
            Err(_) => {
                // Timeout or connection error: stop draining.
                break;
            }
        }
    }
    events
}

#[tokio::test]
#[ignore]
async fn lane_1_message_delivered_and_wake_written() {
    // End-to-end: a sender seat posts a kind-9 @mention (p-tag == seat pubkey)
    // to the relay; the seat connection subscribes, receives it, the dispatch
    // path classifies it as Lane::ForMe, inserts it into the Mailbox, and
    // WakeEmitter writes the wake file.
    //
    // This test exercises the same code path as clerk.rs main loop without
    // running the full binary -- it calls the same public library functions
    // in sequence and asserts the observable side-effects.

    let url = relay_url();

    // Two ephemeral keypairs. Keys are never logged or asserted on raw bytes.
    let seat_keys = ephemeral_keys();
    let sender_keys = ephemeral_keys();

    let seat_pubkey_hex = seat_keys.public_key().to_hex();
    let channel_uuid = Uuid::new_v4();

    // -----------------------------------------------------------------------
    // Step 1: sender publishes a kind-9 @mention of the seat.
    // The Buzz relay uses kind 9 for channel messages. The p-tag holding the
    // seat pubkey triggers Lane::ForMe classification (same logic as classify()).
    // We use a synthetic "e" tag pointing at the channel UUID root event so
    // the relay does not reject the event for missing room context.
    // NOTE: on a freshly started relay with no seeded data this REQ may be
    // rejected with "restricted" if NIP-42 is required before subscription.
    // The connect_authenticated call handles that.
    // -----------------------------------------------------------------------
    let mut sender_conn = NostrWsConnection::connect_authenticated(&url, &sender_keys, None)
        .await
        .expect("sender connect to test relay");

    // Build a synthetic 64-hex event ID from two copies of the channel UUID's
    // simple (hyphen-free) form. UUIDs are 32 hex chars; two concatenated = 64.
    let synthetic_root_id = format!("{}{}", channel_uuid.simple(), channel_uuid.simple());
    debug_assert_eq!(
        synthetic_root_id.len(),
        64,
        "synthetic event id must be 64 hex chars"
    );

    let mention_event = EventBuilder::new(Kind::Custom(9), "hello seat you have a message")
        // p-tag = seat pubkey: triggers Lane::ForMe in classify()
        .tag(nostr::Tag::parse(vec!["p".to_string(), seat_pubkey_hex.clone()]).unwrap())
        // h-tag = room UUID: Buzz REQUIRES kind-9 (channel message) events to carry
        // an ["h", room_id] tag naming the room. Without it the relay rejects with
        // "invalid: channel-scoped events must include an h tag".
        .tag(nostr::Tag::parse(vec!["h".to_string(), channel_uuid.to_string()]).unwrap())
        // e-tag = synthetic channel root (threading anchor; kept for reply-context realism)
        .tag(nostr::Tag::parse(vec!["e".to_string(), synthetic_root_id]).unwrap())
        .sign_with_keys(&sender_keys)
        .expect("sign mention event");

    let event_id = mention_event.id.to_hex();

    let ok = sender_conn
        .send_event(mention_event.clone())
        .await
        .expect("sender publish kind-9");
    assert!(
        ok.accepted,
        "relay must accept the kind-9 @mention: {}",
        ok.message
    );

    let _ = sender_conn.disconnect().await;

    // -----------------------------------------------------------------------
    // Step 2: seat subscribes and receives the kind-9 event.
    // -----------------------------------------------------------------------
    let mut seat_conn = NostrWsConnection::connect_authenticated(&url, &seat_keys, None)
        .await
        .expect("seat connect to test relay");

    // Subscribe: filter for kind-9 events with seat as a p-tag recipient.
    let sub_id = format!("it-sub-{}", Uuid::new_v4().simple());
    let filter = json!({
        "#p": [seat_pubkey_hex.clone()],
        "kinds": [9]
    });
    seat_conn
        .send_raw(&json!(["REQ", sub_id, filter]))
        .await
        .expect("send REQ subscription");

    let received = drain_until_eose(&mut seat_conn, &sub_id, Duration::from_secs(5)).await;

    let _ = seat_conn.disconnect().await;

    // We must have received at least the event we just published.
    assert!(
        !received.is_empty(),
        "seat must receive at least one kind-9 event from the relay"
    );
    assert!(
        received.iter().any(|e| e.id.to_hex() == event_id),
        "the published @mention event must appear in the subscription results"
    );

    // -----------------------------------------------------------------------
    // Step 3: dispatch path -- classify, mailbox insert, wake emit.
    // This mirrors what clerk.rs main loop does for each received event.
    // -----------------------------------------------------------------------
    let dir = tempdir().unwrap();
    let wake_path = dir.path().join("buzz-seat-clerk.wake");
    let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());
    let mut mailbox = Mailbox::new();

    let dispatched_ts = now_secs();

    for event in &received {
        // Extract p-tags.
        let p_tags: Vec<String> = event
            .tags
            .iter()
            .filter(|t| t.kind().to_string() == "p")
            .filter_map(|t| t.content().map(|s| s.to_string()))
            .collect();

        // Classify: is_dm = false (we used a p-tag mention, not a dm channel type).
        let lane = classify(false, &p_tags, &seat_pubkey_hex);

        // Insert into mailbox.
        let entry = MailboxEntry {
            event_id: event.id.to_hex(),
            created_at: event.created_at.as_secs(),
            author_pubkey: event.pubkey.to_hex(),
            // Content is NOT logged or asserted on raw DM text.
            content: event.content.clone(),
            p_tags: p_tags.clone(),
            channel_uuid,
        };
        mailbox.insert(channel_uuid, entry);

        // Emit wake if Lane 1.
        emitter
            .emit_if_lane_1(&lane, dispatched_ts)
            .expect("wake emit must not fail");
    }

    // -----------------------------------------------------------------------
    // Step 4: assertions.
    // -----------------------------------------------------------------------

    // 4a. The mailbox has the mention event.
    let entries = mailbox
        .channel_entries(&channel_uuid)
        .expect("channel must have entries after dispatch");
    assert!(
        entries.iter().any(|e| e.event_id == event_id),
        "mailbox must contain the dispatched @mention event"
    );

    // 4b. The @mention was classified as Lane 1, so the wake file must exist.
    assert!(
        wake_path.exists(),
        "wake file must be written for a Lane-1 @mention event"
    );
    let wake_content = std::fs::read_to_string(&wake_path).expect("read wake file");
    let wake_ts: u64 = wake_content
        .trim()
        .parse()
        .expect("wake file must contain a unix timestamp");
    assert!(
        wake_ts > 0,
        "wake timestamp must be a positive unix seconds value"
    );
    assert!(
        wake_ts <= dispatched_ts + 2,
        "wake timestamp must not be in the future"
    );

    // 4c. Verify the classification result for the event that carried our seat pubkey.
    let target = received.iter().find(|e| e.id.to_hex() == event_id).unwrap();
    let p_tags_for_target: Vec<String> = target
        .tags
        .iter()
        .filter(|t| t.kind().to_string() == "p")
        .filter_map(|t| t.content().map(|s| s.to_string()))
        .collect();
    let final_lane = classify(false, &p_tags_for_target, &seat_pubkey_hex);
    assert_eq!(
        final_lane,
        Lane::ForMe,
        "a kind-9 event with p-tag == seat pubkey must classify as Lane::ForMe"
    );
}

// --------------------------------------------------------------------------
// Test 4: read-state plaintext format contract
// --------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn read_state_content_format_accepted_by_relay() {
    // Verifies that the JSON shape produced by build_read_state_plaintext
    // passes through NIP-44 encryption and is accepted by the relay as a
    // kind-30078 event. This confirms the relay's content validator does not
    // reject the ciphertext blob.
    let seat_keys = ephemeral_keys();
    let dir = tempdir().unwrap();
    let id = SlotIdentity::load_or_create(&dir.path().join("id.json")).unwrap();
    let mut writer = ReadStateWriter::new(id);

    let mut contexts = HashMap::new();
    contexts.insert(Uuid::new_v4().to_string(), now_secs());
    contexts.insert(Uuid::new_v4().to_string(), now_secs() - 60);
    let live = SessionMarker::new("it-live-session-2".to_string());
    record_youyou_read(
        &mut writer,
        contexts.keys().next().unwrap().clone(),
        now_secs(),
        &live,
        &live,
    )
    .expect("live marker must advance bookmark");

    // Sanity-check the plaintext shape (does not touch the relay).
    let plaintext =
        build_read_state_plaintext(&Uuid::new_v4().simple().to_string(), &contexts).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
    assert_eq!(parsed["v"], 1, "v field must be 1");
    assert!(
        parsed["contexts"].is_object(),
        "contexts must be a JSON object"
    );

    // Now publish the encrypted event and assert relay acceptance.
    let mut conn = NostrWsConnection::connect_authenticated(&relay_url(), &seat_keys, None)
        .await
        .expect("connect to test relay");
    let event = writer.build_event(now_secs(), &seat_keys).unwrap();
    let ok = conn.send_event(event).await.unwrap();
    assert!(
        ok.accepted,
        "relay must accept kind-30078 with NIP-44 content: {}",
        ok.message
    );
    let _ = conn.disconnect().await;
}
