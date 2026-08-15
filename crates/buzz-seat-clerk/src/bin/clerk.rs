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
    lane::classify,
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
        let nip98_token = build_nip98_token(&cfg, &relay_http_url)?;
        let mut channels: HashMap<Uuid, ChannelInfo> = match discover_channels(
            &http,
            &relay_http_url,
            &cfg.public_key_hex,
            &nip98_token,
        )
        .await
        {
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
                            event_id: event_id.clone(),
                            created_at: created_at_secs,
                            author_pubkey: event.pubkey.to_hex(),
                            content: event.content.clone(),
                            p_tags: p_tags.clone(),
                            channel_uuid,
                        };
                        mailbox.insert(channel_uuid, entry);

                        let lane = classify(is_dm, &p_tags, &cfg.public_key_hex);
                        if let Err(e) = emitter.emit_if_lane_1(&lane, created_at_secs) {
                            warn!("wake emit failed: {e}");
                        }

                        // Mark channel as read-pending (debounced flush will write kind:30078).
                        writer.mark_read(channel_uuid.to_string(), created_at_secs);

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

fn build_nip98_token(cfg: &ClerkConfig, relay_http_url: &str) -> Result<String> {
    // NIP-98 HTTP Auth: sign a kind-27235 event with the relay URL and method.
    use nostr::{EventBuilder, Kind, Tag};
    let tags = vec![
        Tag::parse(vec!["u".to_owned(), format!("{relay_http_url}/query")]).unwrap(),
        Tag::parse(vec!["method".to_owned(), "POST".to_owned()]).unwrap(),
    ];
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .tags(tags)
        .sign_with_keys(&cfg.keys)
        .context("sign NIP-98 event")?;
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        serde_json::to_string(&event)?,
    );
    Ok(format!("Nostr {encoded}"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
