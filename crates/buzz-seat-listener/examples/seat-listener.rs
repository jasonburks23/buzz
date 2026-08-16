//! Minimal runnable example: generic Buzz seat listener.
//!
//! Shows how to use the public API of buzz-seat-listener with no fleet
//! assumptions. Uses EnvIdentity (local == live, gate always passes) so
//! any single-session user can run this without claim files or AgencyOS.
//!
//! # Required environment variables
//!
//! - `SEAT_NSEC`  -- bech32 nsec of the seat identity key (e.g. nsec1...)
//! - `RELAY_URL`  -- WebSocket URL of the Buzz relay (e.g. wss://relay.example.com)
//!
//! # Optional environment variables
//!
//! - `WAKE_FILE`    -- path to write the wake signal (default: /tmp/buzz-seat-listener.wake)
//! - `READACK_FILE` -- path to poll for read-ack JSON (default: /tmp/buzz-seat-listener.readack)
//!
//! # How to run
//!
//! ```sh
//! SEAT_NSEC=nsec1... RELAY_URL=wss://relay.example.com \
//!   cargo run --example seat-listener -p buzz-seat-listener
//! ```
//!
//! The listener connects with exponential backoff, discovers the seat's
//! channels via REST, subscribes, and delivers each inbound message into
//! an in-memory mailbox. Lane-1 messages (DMs and @mentions) also write
//! a wake signal to WAKE_FILE so a supervisor can act.
//!
//! To enable fleet-specific honest-seen gating, replace EnvIdentity with
//! ClaimFileIdentity from buzz-seat-clerk-agencyos.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use buzz_seat_listener::config::ListenerConfig;
use buzz_seat_listener::connection::connect_with_backoff;
use buzz_seat_listener::discovery::{discover_channels, ChannelInfo, ChannelType};
use buzz_seat_listener::identity::{EnvIdentity, SeatIdentity};
use buzz_seat_listener::lane::classify;
use buzz_seat_listener::mailbox::{Mailbox, MailboxEntry};
use buzz_seat_listener::read_ack::parse_read_ack;
use buzz_seat_listener::read_state::{
    now_secs, parse_read_state_contexts, record_youyou_read, ReadGuardError, ReadStateWriter,
    SlotIdentity,
};
use buzz_seat_listener::session_identity::SessionMarker;
use buzz_seat_listener::subscription::{channel_req_frame, membership_req_frame, TwoGenDedup};
use buzz_seat_listener::wake::WakeEmitter;
use buzz_ws_client::error::WsClientError;
use buzz_ws_client::message::RelayMessage;
use reqwest::Client;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const NEXT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const MEMBERSHIP_SUB_ID: &str = "example-membership";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("buzz_seat_listener=info".parse().unwrap()),
        )
        .init();

    // Load generic listener config from env vars (SEAT_NSEC, RELAY_URL,
    // WAKE_FILE, READACK_FILE). No fleet vars needed.
    let cfg = ListenerConfig::from_env().context("failed to load ListenerConfig from env")?;
    info!(relay = %cfg.relay_url, pubkey = %cfg.public_key_hex, "seat-listener starting");

    // EnvIdentity: local == live, so the read-state gate always passes.
    // Replace with ClaimFileIdentity (buzz-seat-clerk-agencyos) for fleet use.
    let identity = EnvIdentity::new(None);

    // Load or create a durable slot/client identity on disk.
    // This lets reconnects resume from the same read-state slot.
    let identity_path = "/tmp/buzz-seat-listener-identity.json";
    let slot_identity = SlotIdentity::load_or_create(Path::new(identity_path))
        .context("failed to load slot identity")?;
    info!(slot_id = %slot_identity.slot_id, "slot identity loaded");

    let mut writer = ReadStateWriter::new(slot_identity);
    let mut mailbox = Mailbox::new();
    let emitter = WakeEmitter::new(cfg.wake_file.clone());
    let mut dedup = TwoGenDedup::new(512);
    let mut last_readack_mtime: Option<SystemTime> = None;

    // Derive relay HTTP URL from WebSocket URL for REST discovery calls.
    let relay_http_url = cfg
        .relay_url
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let http = Client::new();

    // Boot-time read-state load: restore prior read positions so reconnect
    // subscriptions start from the correct `since` rather than epoch 0.
    // Non-fatal: failure here only means re-reading already-seen messages.
    match buzz_seat_listener::discovery::fetch_own_read_state(
        &http,
        &relay_http_url,
        &cfg.public_key_hex,
        &writer.identity.slot_id,
        &cfg,
    )
    .await
    {
        Ok(Some((ciphertext, created_at))) => {
            match nostr::nips::nip44::decrypt(
                cfg.keys.secret_key(),
                &cfg.keys.public_key(),
                &ciphertext,
            ) {
                Ok(plaintext) => {
                    let contexts = parse_read_state_contexts(&plaintext);
                    let n = contexts.len();
                    writer.seed_contexts(contexts, created_at);
                    info!("read-state loaded: {} context(s)", n);
                }
                Err(e) => {
                    warn!("read-state decrypt failed: {e}; starting from empty");
                }
            }
        }
        Ok(None) => {
            info!("no prior read-state found");
        }
        Err(e) => {
            warn!("read-state load failed: {e}; starting from empty");
        }
    }

    // Reconnect loop: connects forever (no max_attempts).
    loop {
        let mut conn = connect_with_backoff(&cfg.relay_url, &cfg.keys, None).await?;

        // Discover channels this seat belongs to via REST.
        let mut channels: HashMap<Uuid, ChannelInfo> =
            match discover_channels(&http, &relay_http_url, &cfg.public_key_hex, &cfg).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("channel discovery failed: {e}; proceeding with empty set");
                    HashMap::new()
                }
            };
        info!("discovered {} channel(s)", channels.len());

        // Global membership subscription: watch for new DMs and channel adds.
        let membership_frame =
            membership_req_frame(MEMBERSHIP_SUB_ID, &cfg.public_key_hex, now_secs());
        conn.send_raw(&membership_frame).await?;

        // Per-room subscriptions: start from the last-read bookmark so reconnect
        // only backfills messages the seat has not already seen.
        for uuid in channels.keys() {
            let sub_id = format!("example-ch-{uuid}");
            let since = writer.read_at_for(&uuid.to_string()).unwrap_or(0);
            let frame = channel_req_frame(&sub_id, uuid, since);
            conn.send_raw(&frame).await?;
        }

        info!("event loop running; press Ctrl-C to stop");

        // Event loop.
        'event_loop: loop {
            // Flush read-state to relay if the debounce window has elapsed.
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

            // Poll the read-ack file for honest read-receipt advancement.
            // With EnvIdentity(None) both markers are None, so the gate is
            // skipped safely (no bookmark advance). Pass Some(marker) to
            // EnvIdentity::new to enable bookmark writes for a known session.
            let readack_path = Path::new(&cfg.readack_file);
            let current_mtime = std::fs::metadata(readack_path)
                .ok()
                .and_then(|m| m.modified().ok());
            let changed = match (current_mtime, last_readack_mtime) {
                (Some(cur), Some(prev)) => cur != prev,
                (Some(_), None) => true,
                _ => false,
            };
            if changed {
                last_readack_mtime = current_mtime;
                if let Ok(raw) = std::fs::read_to_string(readack_path) {
                    if let Some(ack) = parse_read_ack(&raw) {
                        let live = identity.live_marker();
                        if let Some(live_m) = live {
                            match record_youyou_read(
                                &mut writer,
                                ack.channel.clone(),
                                ack.up_to_ts,
                                &SessionMarker::new(ack.marker.clone()),
                                &live_m,
                            ) {
                                Ok(()) => {
                                    debug!(channel = %ack.channel, ts = ack.up_to_ts, "read-ack advanced bookmark");
                                }
                                Err(ReadGuardError::NotLiveSession) => {
                                    warn!(channel = %ack.channel, "read-ack from non-live actor ignored");
                                }
                            }
                        } else {
                            debug!("honest-seen gate skipped: identity marker is None");
                        }
                    }
                }
            }

            match conn.next_event(NEXT_EVENT_TIMEOUT).await {
                Ok(RelayMessage::Event { event, .. }) => {
                    let event_id = event.id.to_hex();
                    if !dedup.is_new(&event_id) {
                        continue;
                    }

                    let kind_num: u32 = event.kind.as_u16().into();

                    // Handle membership-add (kind 44100): subscribe to the new channel.
                    if kind_num == KIND_MEMBER_ADDED_NOTIFICATION {
                        if let Some(channel_uuid) = extract_h_tag_from_nostr(&event) {
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                channels.entry(channel_uuid)
                            {
                                info!(channel = %channel_uuid, "new membership: subscribing");
                                let sub_id = format!("example-ch-{channel_uuid}");
                                let frame = channel_req_frame(&sub_id, &channel_uuid, now_secs());
                                if let Err(err) = conn.send_raw(&frame).await {
                                    warn!("subscribe new channel failed: {err}");
                                }
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
                        let Some(channel_uuid) = extract_h_tag_from_nostr(&event) else {
                            continue;
                        };

                        // Skip own-authored events: the relay echoes the seat's own
                        // messages back, which would cause a spurious self-wake.
                        if event.pubkey.to_hex() == cfg.public_key_hex {
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

                        info!(
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
                    if !matches!(e, WsClientError::Timeout) {
                        error!("relay error: {e}; reconnecting");
                        break 'event_loop;
                    }
                    debug!("relay idle (timeout); continuing");
                    continue;
                }
            }
        }
    }
}

// Keep KIND_MEMBER_REMOVED_NOTIFICATION in scope: the membership REQ frame
// subscribes to both 44100 and 44101, even though this example only acts on 44100.
const _: u32 = KIND_MEMBER_REMOVED_NOTIFICATION;

/// Extract the room UUID from the `h` tag of a nostr event.
fn extract_h_tag_from_nostr(event: &nostr::Event) -> Option<Uuid> {
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
