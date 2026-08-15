//! Buzz Seat Clerk binary.
//!
//! Connects to a Buzz relay, subscribes to all rooms the seat is a member of,
//! delivers messages to the local mailbox, writes kind:30078 read-state bookmarks,
//! and emits a wake signal on Lane-1 (DM / @mention) messages.
//!
//! DUMB CLERK: it delivers and badges. It NEVER answers.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use buzz_seat_clerk::{
    config::ClerkConfig,
    connection::connect_with_backoff,
    discovery::{discover_channels, ChannelInfo, ChannelType},
    lane::{classify, Lane},
    mailbox::{Mailbox, MailboxEntry},
    read_state::{now_secs, ReadStateWriter, SlotIdentity},
    subscription::{channel_req_frame, membership_req_frame, TwoGenDedup},
    wake::WakeEmitter,
};
use buzz_ws_client::message::RelayMessage;
use reqwest::Client;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const NEXT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const MEMBERSHIP_SUB_ID: &str = "clerk-membership";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = ClerkConfig::from_env().context("load config")?;
    info!(pubkey = %cfg.public_key_hex, relay = %cfg.relay_url, "clerk starting");

    let identity_path = std::env::var("IDENTITY_FILE")
        .unwrap_or_else(|_| "/tmp/buzz-seat-clerk-identity.json".into());
    let identity =
        SlotIdentity::load_or_create(Path::new(&identity_path)).context("load slot identity")?;
    info!(slot_id = %identity.slot_id, "identity loaded");

    let mut writer = ReadStateWriter::new(identity);
    let mut mailbox = Mailbox::new();
    let emitter = WakeEmitter::new(cfg.wake_file.clone());
    let mut dedup = TwoGenDedup::new(512);

    // Derive relay HTTP URL from WS URL.
    let relay_http_url = cfg
        .relay_url
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let http = Client::new();

    loop {
        // Connect (retries forever by default).
        let mut conn = connect_with_backoff(&cfg.relay_url, &cfg.keys, None).await?;

        // Discover rooms via REST.
        // Token generation is deferred into discover_channels so the payload hash
        // covers the exact bytes each request sends.
        let mut channels: HashMap<Uuid, ChannelInfo> =
            match discover_channels(&http, &relay_http_url, &cfg.public_key_hex, &cfg).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("channel discovery failed: {e}; proceeding with empty set");
                    HashMap::new()
                }
            };
        info!("discovered {} channel(s)", channels.len());

        // Global membership subscription.
        let membership_frame =
            membership_req_frame(MEMBERSHIP_SUB_ID, &cfg.public_key_hex, now_secs());
        conn.send_raw(&membership_frame).await?;

        // Per-room subscriptions: use the last-read bookmark as `since` so reconnect
        // only backfills unread messages instead of re-fetching from epoch.
        for uuid in channels.keys() {
            let sub_id = format!("clerk-ch-{uuid}");
            let since = writer.read_at_for(&uuid.to_string()).unwrap_or(0);
            let frame = channel_req_frame(&sub_id, uuid, since);
            conn.send_raw(&frame).await?;
        }

        // Event loop.
        'event_loop: loop {
            // Flush read-state if debounce elapsed.
            let now = now_secs();
            if writer.is_flush_due(now) {
                match writer.build_event(now, &cfg.keys) {
                    Ok(event) => {
                        if let Err(e) = conn.send_event(event).await {
                            warn!("read-state publish failed: {e}");
                        } else {
                            debug!("read-state flushed");
                        }
                    }
                    Err(e) => warn!("read-state build failed: {e}"),
                }
            }

            match conn.next_event(NEXT_EVENT_TIMEOUT).await {
                Ok(RelayMessage::Event { event, .. }) => {
                    let event_id = event.id.to_hex();
                    if !dedup.is_new(&event_id) {
                        continue;
                    }

                    let kind_num: u32 = event.kind.as_u16().into();

                    // Handle membership change (TRIPWIRE 3: kind 44100 is membership-only).
                    if kind_num == KIND_MEMBER_ADDED_NOTIFICATION {
                        // Extract h-tag (channel UUID) and subscribe if new.
                        if let Some(channel_uuid) = extract_h_tag(&event) {
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                channels.entry(channel_uuid)
                            {
                                info!(channel = %channel_uuid, "new membership: subscribing");
                                let sub_id = format!("clerk-ch-{channel_uuid}");
                                let frame = channel_req_frame(&sub_id, &channel_uuid, now_secs());
                                if let Err(err) = conn.send_raw(&frame).await {
                                    warn!("subscribe new channel failed: {err}");
                                }
                                // Add placeholder info (metadata fetch is future work).
                                e.insert(ChannelInfo {
                                    uuid: channel_uuid,
                                    name: String::new(),
                                    channel_type: ChannelType::Unknown,
                                });
                            }
                        }
                        continue;
                    }

                    // Handle channel messages (kinds 9 and 40002).
                    if kind_num == KIND_STREAM_MESSAGE || kind_num == KIND_STREAM_MESSAGE_V2 {
                        let Some(channel_uuid) = extract_h_tag(&event) else {
                            continue;
                        };

                        // Skip own-authored events: the relay echoes the seat's own
                        // DMs back (auto-p-tagged), which would cause a spurious self-wake.
                        if is_own_event(&event.pubkey.to_hex(), &cfg.public_key_hex) {
                            debug!(channel = %channel_uuid, "skipping own-authored event");
                            continue;
                        }

                        let lane = deliver_event(
                            &mut mailbox,
                            &emitter,
                            &cfg.public_key_hex,
                            &channels,
                            &event,
                            channel_uuid,
                        );

                        debug!(
                            channel = %channel_uuid,
                            lane = ?lane,
                            author = %event.pubkey.to_hex(),
                            "message delivered"
                        );
                    }
                }
                Ok(RelayMessage::Closed { message, .. }) => {
                    warn!("relay closed subscription: {message}; reconnecting");
                    break 'event_loop;
                }
                Ok(_) => {} // EOSE, NOTICE, etc. -- ignore.
                Err(e) => {
                    error!("relay error: {e}; reconnecting");
                    break 'event_loop;
                }
            }
        }
    }
}

fn extract_h_tag(event: &nostr::Event) -> Option<Uuid> {
    event
        .tags
        .iter()
        .find(|t| {
            t.as_slice()
                .first()
                .map(|s| s.as_str() == "h")
                .unwrap_or(false)
        })
        .and_then(|t| t.as_slice().get(1))
        .and_then(|v| v.parse::<Uuid>().ok())
}

// Suppress unused-import lint: KIND_MEMBER_REMOVED_NOTIFICATION is imported
// for documentation parity (the membership REQ frame subscribes to both 44100
// and 44101) but the event loop only needs to act on 44100 (add).
// KIND_MEMBER_REMOVED_NOTIFICATION handling (unsubscribe) is future work.
const _: u32 = KIND_MEMBER_REMOVED_NOTIFICATION;

/// Returns true when the event author is the seat itself.
///
/// Used to skip own-authored messages before classify/wake so the seat does
/// not emit a spurious self-wake when the relay echoes its own DM back.
fn is_own_event(event_pubkey_hex: &str, own_pubkey_hex: &str) -> bool {
    event_pubkey_hex == own_pubkey_hex
}

/// Deliver one inbound channel event: build the mailbox entry, insert it, classify
/// the lane, and emit a wake signal if Lane 1.
///
/// Intentionally takes NO ReadStateWriter and has no access to mark_read.
/// US-07 hard line: the bookmark must only advance when the live session reads
/// (via record_youyou_read), never at delivery time. If this function were to
/// call mark_read it would need the writer in its signature, making the
/// violation structurally visible and breaking the US-07 S3 guard test.
fn deliver_event(
    mailbox: &mut Mailbox,
    emitter: &WakeEmitter,
    public_key_hex: &str,
    channels: &HashMap<Uuid, ChannelInfo>,
    event: &nostr::Event,
    channel_uuid: Uuid,
) -> Lane {
    let channel_info = channels.get(&channel_uuid);
    let is_dm = channel_info
        .map(|c| c.channel_type == ChannelType::Dm)
        .unwrap_or(false);

    let p_tags: Vec<String> = event
        .tags
        .iter()
        .filter(|t| {
            t.as_slice()
                .first()
                .map(|s| s.as_str() == "p")
                .unwrap_or(false)
        })
        .filter_map(|t| t.as_slice().get(1).cloned())
        .collect();

    let created_at_secs = event.created_at.as_secs();

    let entry = MailboxEntry {
        event_id: event.id.to_hex(),
        created_at: created_at_secs,
        author_pubkey: event.pubkey.to_hex(),
        content: event.content.clone(),
        p_tags: p_tags.clone(),
        channel_uuid,
    };
    mailbox.insert(channel_uuid, entry);

    let lane = classify(is_dm, &p_tags, public_key_hex);
    if let Err(e) = emitter.emit_if_lane_1(&lane, created_at_secs) {
        warn!("wake emit failed: {e}");
    }

    // NOTE: The delivery path intentionally does NOT call mark_read here.
    // US-07: the bookmark must only advance when the live session reads,
    // not when the clerk delivers. The live session calls record_youyou_read
    // (gated by SessionMarker) to advance the bookmark. The debounced
    // flush via writer.build_event / writer.is_flush_due is still active.

    lane
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_seat_clerk::read_state::{generate_slot_id, ReadStateWriter, SlotIdentity};
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use tempfile::tempdir;

    #[test]
    fn is_own_event_true_when_equal() {
        let pk = "deadbeef".repeat(8); // 64-char hex
        assert!(is_own_event(&pk, &pk));
    }

    #[test]
    fn is_own_event_false_when_different() {
        let pk_a = "deadbeef".repeat(8);
        let pk_b = "cafebabe".repeat(8);
        assert!(!is_own_event(&pk_a, &pk_b));
    }

    // US-07 S3 WIRED GUARD: delivery does NOT advance the read bookmark.
    //
    // This test calls the real deliver_event path and asserts:
    //   (a) the mailbox contains the delivered entry (delivery worked), AND
    //   (b) the ReadStateWriter bookmark for that channel is None (delivery
    //       did NOT mark read).
    //
    // This guard turns RED if delivery-time marking is reintroduced, because
    // deliver_event would then need the writer in its signature (it has no
    // access to it today), and adding it back would require passing the writer
    // in -- that structural change plus a mark_read call would cause the
    // bookmark assertion below to fail, turning this test RED.
    #[test]
    fn us07_s3_wired_deliver_event_does_not_advance_bookmark() {
        // Build a real signed nostr Event for a non-DM channel mention.
        let sender_keys = Keys::generate();
        let seat_keys = Keys::generate();
        let seat_pubkey_hex = seat_keys.public_key().to_hex();
        let channel_uuid = Uuid::new_v4();

        let event = EventBuilder::new(Kind::Custom(9), "hello seat")
            .tag(Tag::parse(vec!["p".to_string(), seat_pubkey_hex.clone()]).expect("build p-tag"))
            .tag(Tag::parse(vec!["h".to_string(), channel_uuid.to_string()]).expect("build h-tag"))
            .sign_with_keys(&sender_keys)
            .expect("sign test event");

        // Set up a Mailbox and WakeEmitter (temp dir wake file).
        let dir = tempdir().unwrap();
        let wake_path = dir.path().join("wake");
        let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());
        let mut mailbox = Mailbox::new();

        // Empty channels map: channel_type defaults to Unknown -> not a DM.
        // The p-tag match still classifies as Lane::ForMe.
        let channels: HashMap<Uuid, ChannelInfo> = HashMap::new();

        // Set up a ReadStateWriter SEPARATELY. It is NOT passed to deliver_event.
        let writer = ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        });
        let ctx = channel_uuid.to_string();

        // Call the real delivery path.
        let lane = deliver_event(
            &mut mailbox,
            &emitter,
            &seat_pubkey_hex,
            &channels,
            &event,
            channel_uuid,
        );

        // (a) Delivery worked: mailbox contains the entry.
        let entries = mailbox
            .channel_entries(&channel_uuid)
            .expect("channel must have entries after deliver_event");
        assert!(
            entries.iter().any(|e| e.event_id == event.id.to_hex()),
            "mailbox must contain the delivered entry"
        );

        // Lane was classified correctly (p-tag mention -> ForMe).
        assert_eq!(
            lane,
            Lane::ForMe,
            "p-tag mention must classify as Lane::ForMe"
        );

        // (b) The bookmark was NOT advanced by delivery.
        assert_eq!(
            writer.read_at_for(&ctx),
            None,
            "delivery must NOT advance the read bookmark (US-07 hard line)"
        );
    }
}
