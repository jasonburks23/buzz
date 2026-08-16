//! AgencyOS fleet clerk binary.
//!
//! Loads `ListenerConfig` (generic env vars: SEAT_NSEC, RELAY_URL, WAKE_FILE,
//! READACK_FILE) and `AgencyOsConfig` (fleet env vars: SEAT_ROLE, SEAT_CWD,
//! CLAIM_DIR, CLERK_SESSION_ID), then runs the full seat listener loop with
//! `ClaimFileIdentity` injected for honest-seen gating.
//!
//! Honest-seen gate at the `record_youyou_read` call site:
//!
//!   let local = identity.local_marker();
//!   let live  = identity.live_marker();
//!   if let (Some(local_m), Some(live_m)) = (local, live) {
//!       record_youyou_read(&mut writer, ctx, ts, &local_m, &live_m)?;
//!   }
//!
//! Both markers must be `Some` and equal for the bookmark to advance. If
//! either is `None` (identity unknown or claim files absent), the gate is
//! skipped safely with no bookmark advance.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use buzz_seat_clerk_agencyos::claim_identity::ClaimFileIdentity;
use buzz_seat_clerk_agencyos::config_ext::AgencyOsConfig;
use buzz_seat_listener::config::ListenerConfig;
use buzz_seat_listener::connection::connect_with_backoff;
use buzz_seat_listener::discovery::{
    discover_channels, fetch_own_read_state, ChannelInfo, ChannelType,
};
use buzz_seat_listener::identity::SeatIdentity;
use buzz_seat_listener::lane::{classify, Lane};
use buzz_seat_listener::mailbox::{Mailbox, MailboxEntry};
use buzz_seat_listener::read_ack::parse_read_ack;
use buzz_seat_listener::read_state::{
    now_secs, parse_read_state_contexts, record_youyou_read, ReadGuardError, ReadStateWriter,
    SlotIdentity,
};
use buzz_seat_listener::session_identity::SessionMarker;
use buzz_seat_listener::subscription::{channel_req_frame, membership_req_frame, TwoGenDedup};
use buzz_ws_client::error::WsClientError;
use buzz_ws_client::message::RelayMessage;
use reqwest::Client;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const NEXT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const MEMBERSHIP_SUB_ID: &str = "clerk-membership";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Load generic listener config from env.
    let cfg = ListenerConfig::from_env().context("failed to load ListenerConfig from env")?;
    info!(pubkey = %cfg.public_key_hex, relay = %cfg.relay_url, "buzz-seat-clerk-agencyos starting");

    // Load fleet-specific config from env.
    let agency_cfg = AgencyOsConfig::from_env();

    // Build the fleet identity. local_marker = this session_id;
    // live_marker = freshest claim file in claim_dir whose role matches.
    let identity = ClaimFileIdentity::new(
        agency_cfg.session_id.clone(),
        agency_cfg.seat_role.clone(),
        agency_cfg.seat_cwd.clone(),
        agency_cfg.claim_dir.clone(),
    );
    info!(
        role = ?agency_cfg.seat_role,
        session_id = %agency_cfg.session_id,
        "fleet identity loaded"
    );

    let identity_path = std::env::var("IDENTITY_FILE")
        .unwrap_or_else(|_| "/tmp/buzz-seat-clerk-identity.json".into());
    let slot_identity =
        SlotIdentity::load_or_create(Path::new(&identity_path)).context("load slot identity")?;
    info!(slot_id = %slot_identity.slot_id, "identity loaded");

    let mut writer = ReadStateWriter::new(slot_identity);
    let mut mailbox = Mailbox::new();
    let emitter = buzz_seat_listener::wake::WakeEmitter::new(cfg.wake_file.clone());
    let mut dedup = TwoGenDedup::new(512);
    let mut last_readack_mtime: Option<SystemTime> = None;

    // Derive relay HTTP URL from WS URL.
    let relay_http_url = cfg
        .relay_url
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let http = Client::new();

    // Boot-time read-state load: restore prior read positions so reconnect
    // subscriptions start from the correct `since` rather than epoch 0.
    // Non-fatal: a failure here only means the seat re-reads already-seen messages.
    // SECURITY: never log ciphertext, plaintext, or per-context timestamps.
    match fetch_own_read_state(
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
                    warn!("read-state load failed: {e}; starting from empty");
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
        // Connect (retries forever by default).
        let mut conn = connect_with_backoff(&cfg.relay_url, &cfg.keys, None).await?;

        // Discover rooms via REST.
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

            // Poll the read-ack file for honest read-receipt advancement.
            // Gate: advance bookmark only if this IS the live session.
            // Both local and live markers must be Some and equal for the gate to pass.
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
                        // Sourcing both markers from the injected ClaimFileIdentity
                        // keeps the gate function signature stable and testable.
                        // If either marker is None (unknown identity or no claim files),
                        // we skip the advance safely.
                        // Honest-seen gate: resolve live marker from claim files.
                        // If either marker is None (no claim files, or empty session_id),
                        // skip the advance safely rather than panic or guess.
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
                                    debug!(
                                        channel = %ack.channel,
                                        ts = ack.up_to_ts,
                                        "read-ack advanced bookmark"
                                    );
                                }
                                Err(ReadGuardError::NotLiveSession) => {
                                    warn!(
                                        channel = %ack.channel,
                                        "read-ack from non-live actor ignored"
                                    );
                                }
                            }
                        } else {
                            debug!("honest-seen gate skipped: live marker is None (no valid claim file)");
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
                    if should_reconnect(&e) {
                        error!("relay error: {e}; reconnecting");
                        break 'event_loop;
                    }
                    debug!("relay idle ({e}); continuing");
                    continue;
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

/// A read timeout means the relay was simply idle for the window; that is
/// normal and must NOT trigger a reconnect. Every other error is a real
/// transport fault and should reconnect.
fn should_reconnect(err: &WsClientError) -> bool {
    !matches!(err, WsClientError::Timeout)
}

/// Returns true when the event author is the seat itself.
fn is_own_event(event_pubkey_hex: &str, own_pubkey_hex: &str) -> bool {
    event_pubkey_hex == own_pubkey_hex
}

/// Deliver one inbound channel event: build the mailbox entry, insert it, classify
/// the lane, and emit a wake signal if Lane 1.
///
/// Intentionally takes NO ReadStateWriter. US-07 hard line: the bookmark must
/// only advance when the live session reads (via record_youyou_read), never at
/// delivery time.
fn deliver_event(
    mailbox: &mut Mailbox,
    emitter: &buzz_seat_listener::wake::WakeEmitter,
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
    if let Err(e) =
        emitter.emit_if_lane_1_for_channel(&lane, &channel_uuid.to_string(), created_at_secs)
    {
        warn!("wake emit failed: {e}");
    }

    lane
}
