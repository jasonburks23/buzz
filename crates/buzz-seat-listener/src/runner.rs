//! Generic entry point: connect, subscribe, deliver, wake, bookmark.
//! Both the example binary and the fleet clerk collapse to one call here.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use reqwest::Client;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::ListenerConfig;
use crate::connection::connect_with_backoff;
use crate::discovery::{discover_channels, ChannelInfo, ChannelType};
use crate::identity::SeatIdentity;
use crate::lane::classify;
use crate::mailbox::{Mailbox, MailboxEntry};
use crate::read_ack::parse_read_ack;
use crate::read_state::{
    now_secs, parse_read_state_contexts, record_youyou_read, ReadGuardError, ReadStateWriter,
    SlotIdentity,
};
use crate::session_identity::SessionMarker;
use crate::subscription::{channel_req_frame, membership_req_frame, TwoGenDedup};
use crate::wake::WakeEmitter;
use buzz_ws_client::error::WsClientError;
use buzz_ws_client::message::RelayMessage;

const NEXT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const MEMBERSHIP_SUB_ID: &str = "runner-membership";

/// Run the Buzz seat-listener to completion.
///
/// Connects to the relay in `cfg`, subscribes to the seat's Lane-1 channels,
/// emits per-channel wake signals via the WakeEmitter on ForMe events,
/// and advances read-state bookmarks through the injected `identity`.
///
/// Returns when the relay connection closes or an unrecoverable error occurs.
pub async fn run(cfg: ListenerConfig, identity: impl SeatIdentity + 'static) -> Result<()> {
    let identity_path = "/tmp/buzz-seat-listener-identity.json";
    let slot_identity = SlotIdentity::load_or_create(Path::new(identity_path))
        .context("failed to load slot identity")?;
    info!(slot_id = %slot_identity.slot_id, "slot identity loaded");

    let mut writer = ReadStateWriter::new(slot_identity);
    let mut mailbox = Mailbox::new();
    let emitter = WakeEmitter::new(cfg.wake_file.clone());
    let mut dedup = TwoGenDedup::new(512);
    let mut last_readack_mtime: Option<SystemTime> = None;

    let relay_http_url = cfg
        .relay_url
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let http = Client::new();

    match crate::discovery::fetch_own_read_state(
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

    loop {
        let mut conn = connect_with_backoff(&cfg.relay_url, &cfg.keys, None).await?;

        let mut channels: HashMap<Uuid, ChannelInfo> =
            match discover_channels(&http, &relay_http_url, &cfg.public_key_hex, &cfg).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("channel discovery failed: {e}; proceeding with empty set");
                    HashMap::new()
                }
            };
        info!("discovered {} channel(s)", channels.len());

        let membership_frame =
            membership_req_frame(MEMBERSHIP_SUB_ID, &cfg.public_key_hex, now_secs());
        conn.send_raw(&membership_frame).await?;

        for uuid in channels.keys() {
            let sub_id = format!("runner-ch-{uuid}");
            let since = writer.read_at_for(&uuid.to_string()).unwrap_or(0);
            let frame = channel_req_frame(&sub_id, uuid, since);
            conn.send_raw(&frame).await?;
        }

        info!("event loop running; press Ctrl-C to stop");

        'event_loop: loop {
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

                    if kind_num == KIND_MEMBER_ADDED_NOTIFICATION {
                        if let Some(channel_uuid) = extract_h_tag(&event) {
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                channels.entry(channel_uuid)
                            {
                                info!(channel = %channel_uuid, "new membership: subscribing");
                                let sub_id = format!("runner-ch-{channel_uuid}");
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

                    if kind_num == KIND_STREAM_MESSAGE || kind_num == KIND_STREAM_MESSAGE_V2 {
                        let Some(channel_uuid) = extract_h_tag(&event) else {
                            continue;
                        };

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
                        if let Err(e) = emitter.emit_if_lane_1_for_channel(
                            &lane,
                            &channel_uuid.to_string(),
                            created_at_secs,
                        ) {
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
                Ok(_) => {}
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
// subscribes to both 44100 and 44101, even though we only act on 44100.
const _: u32 = KIND_MEMBER_REMOVED_NOTIFICATION;

/// Extract the room UUID from the `h` tag of a nostr event.
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
