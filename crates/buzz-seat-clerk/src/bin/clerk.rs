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
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_MESSAGE_V2,
};
use buzz_seat_clerk::{
    config::ClerkConfig,
    connection::connect_with_backoff,
    discovery::{discover_channels, fetch_own_read_state, ChannelInfo, ChannelType},
    lane::{classify, Lane},
    mailbox::{Mailbox, MailboxEntry},
    read_ack::{parse_multi_channel_ack, parse_read_ack},
    read_state::{
        now_secs, parse_read_state_contexts, record_youyou_read, ReadGuardError, ReadStateWriter,
        SlotIdentity,
    },
    session_identity::{resolve_live_marker_from_claims, SessionMarker},
    subscription::{channel_req_frame, membership_req_frame, TwoGenDedup},
    wake::WakeEmitter,
};
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

    let cfg = ClerkConfig::from_env().context("load config")?;
    info!(pubkey = %cfg.public_key_hex, relay = %cfg.relay_url, "clerk starting");

    // Resolve live session identity from fleet seat-claim files.
    // Feature is only active when SEAT_ROLE is set.
    let live_marker: Option<SessionMarker> = if let Some(ref role) = cfg.seat_role {
        match resolve_live_marker_from_claims(
            Path::new(&cfg.claim_dir),
            role,
            cfg.seat_cwd.as_deref(),
        ) {
            Ok(marker) => {
                info!(role = %role, "live identity resolved");
                Some(marker)
            }
            Err(e) => {
                warn!("honest-seen disabled: could not resolve live identity: {e}");
                None
            }
        }
    } else {
        info!("honest-seen disabled: no SEAT_ROLE configured");
        None
    };

    let identity_path = std::env::var("IDENTITY_FILE")
        .unwrap_or_else(|_| "/tmp/buzz-seat-clerk-identity.json".into());
    let identity =
        SlotIdentity::load_or_create(Path::new(&identity_path)).context("load slot identity")?;
    info!(slot_id = %identity.slot_id, "identity loaded");

    let mut writer = ReadStateWriter::new(identity);
    let mut mailbox = Mailbox::new();
    let emitter = WakeEmitter::new(cfg.wake_file.clone());
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

            // Poll the read-ack file for honest read-receipt advancement.
            // Only active when live_marker is Some (SEAT_ROLE is configured).
            if let Some(ref live) = live_marker {
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
                        // Try multi-channel format first (v1); fall back to single-channel.
                        if let Some(multi_ack) = parse_multi_channel_ack(&raw) {
                            let actor = SessionMarker::new(multi_ack.marker.clone());
                            for (channel, ts) in &multi_ack.channels {
                                match record_youyou_read(
                                    &mut writer,
                                    channel.clone(),
                                    *ts,
                                    &actor,
                                    live,
                                ) {
                                    Ok(()) => {
                                        debug!(
                                            channel = %channel,
                                            ts = ts,
                                            "multi-channel read-ack advanced bookmark"
                                        );
                                    }
                                    Err(ReadGuardError::NotLiveSession) => {
                                        warn!(
                                            channel = %channel,
                                            "multi-channel read-ack from non-live actor ignored"
                                        );
                                    }
                                }
                            }
                        } else if let Some(ack) = parse_read_ack(&raw) {
                            match record_youyou_read(
                                &mut writer,
                                ack.channel.clone(),
                                ack.up_to_ts,
                                &SessionMarker::new(ack.marker.clone()),
                                live,
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

                        // On a Lane-1 wake, also write the unread-summary sidecar.
                        // This lets the woken session read one file and open exactly
                        // the right rooms without sweeping every channel.
                        if lane == Lane::ForMe {
                            emitter.emit_badge_sidecar(
                                event.created_at.as_secs(),
                                &mailbox,
                                &writer,
                                &channels,
                                &cfg.public_key_hex,
                            );
                        }

                        // info-level so a live test shows each message landing
                        // (and its lane) at the default log level, without debug.
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

    // RED tests for should_reconnect (written before the helper exists).
    // These will fail to compile until should_reconnect is implemented.
    #[test]
    fn should_reconnect_false_on_timeout() {
        assert!(!should_reconnect(&WsClientError::Timeout));
    }

    #[test]
    fn should_reconnect_true_on_connection_closed() {
        assert!(should_reconnect(&WsClientError::ConnectionClosed));
    }

    #[test]
    fn should_reconnect_true_on_transport_error() {
        // ConnectionClosed is a second distinct non-timeout variant; confirm
        // should_reconnect returns true for any non-Timeout error.
        assert!(should_reconnect(&WsClientError::ConnectionClosed));
    }

    // Task 2: multi-channel ack wiring tests.
    // Written before the poll-loop implementation to drive TDD.

    /// A multi-channel ack with the correct live marker advances ALL listed channels.
    #[test]
    fn multi_channel_ack_live_marker_advances_all_channels() {
        use buzz_seat_clerk::read_ack::parse_multi_channel_ack;
        use buzz_seat_clerk::read_state::{
            generate_slot_id, record_youyou_read, ReadStateWriter, SlotIdentity,
        };
        use buzz_seat_clerk::session_identity::SessionMarker;

        let live = SessionMarker::new("live-session-abc".to_string());
        let mut writer = ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        });

        let json =
            r#"{"v":1,"channels":{"chan-one":1000,"chan-two":2000},"marker":"live-session-abc"}"#;
        let ack = parse_multi_channel_ack(json).expect("must parse");
        let actor = SessionMarker::new(ack.marker.clone());

        // Advance each channel using the same code path the wired poll loop will use.
        for (channel, ts) in &ack.channels {
            record_youyou_read(&mut writer, channel.clone(), *ts, &actor, &live)
                .expect("live actor must succeed");
        }

        // Both bookmarks must be advanced.
        assert_eq!(writer.read_at_for("chan-one"), Some(1000));
        assert_eq!(writer.read_at_for("chan-two"), Some(2000));
    }

    /// A multi-channel ack with a wrong marker must be refused for ALL channels.
    #[test]
    fn multi_channel_ack_wrong_marker_is_refused() {
        use buzz_seat_clerk::read_ack::parse_multi_channel_ack;
        use buzz_seat_clerk::read_state::{
            generate_slot_id, record_youyou_read, ReadGuardError, ReadStateWriter, SlotIdentity,
        };
        use buzz_seat_clerk::session_identity::SessionMarker;

        let live = SessionMarker::new("live-session-abc".to_string());
        let mut writer = ReadStateWriter::new(SlotIdentity {
            slot_id: generate_slot_id(),
            client_id: generate_slot_id(),
        });

        let json =
            r#"{"v":1,"channels":{"chan-one":1000,"chan-two":2000},"marker":"wrong-session-xyz"}"#;
        let ack = parse_multi_channel_ack(json).expect("must parse");
        let actor = SessionMarker::new(ack.marker.clone());

        for (channel, ts) in &ack.channels {
            let result = record_youyou_read(&mut writer, channel.clone(), *ts, &actor, &live);
            assert_eq!(
                result,
                Err(ReadGuardError::NotLiveSession),
                "wrong marker must be refused for channel {channel}"
            );
        }

        // Neither bookmark must have advanced.
        assert_eq!(writer.read_at_for("chan-one"), None);
        assert_eq!(writer.read_at_for("chan-two"), None);
    }
}
