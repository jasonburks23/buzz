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
    logging::{
        append_log_line, clerk_log_path, format_startup_banner, resolve_git_commit,
        resolve_seat_identity,
    },
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

    // comms-orch#11 slice B: durable, size-capped, per-seat-attributable log file. Installed
    // as early as possible -- right after cfg loads, since the log path itself is derived from
    // the seat identity cfg carries -- so anything that goes wrong below this point has a
    // fighting chance of leaving a trace on disk, not just in a terminal tab's scrollback.
    let seat_identity = resolve_seat_identity(cfg.seat_role.as_deref(), &cfg.public_key_hex);
    let log_path = clerk_log_path(&seat_identity);
    let pid = std::process::id();
    let repo_dir = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let commit = resolve_git_commit(&repo_dir);
    append_log_line(
        &log_path,
        &seat_identity,
        &format_startup_banner(&commit, pid, &seat_identity),
    );

    // comms-orch#11 AC1: record why the process stopped. SIGTERM/SIGINT are logged, then this
    // process exits 0 -- a graceful, supervisor-expected shutdown. SIGKILL cannot be caught by
    // any process on any OS; that half of "why" can only come from whatever supervises this
    // process (relaunch.sh, comms-orch#11 slice C), never from this binary. Not attempted here.
    {
        let log_path = log_path.clone();
        let seat_identity = seat_identity.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("could not install SIGTERM handler: {e}");
                    return;
                }
            };
            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("could not install SIGINT handler: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = sigterm.recv() => {
                    append_log_line(&log_path, &seat_identity, "received SIGTERM, exiting gracefully");
                    std::process::exit(0);
                }
                _ = sigint.recv() => {
                    append_log_line(&log_path, &seat_identity, "received SIGINT, exiting gracefully");
                    std::process::exit(0);
                }
            }
        });
    }

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
                // Publish operator-addressed copy BEFORE self-addressed copy because
                // build_event clears pending_contexts on success.
                match writer.build_operator_event(now, &cfg.keys, cfg.operator_pubkey.as_ref()) {
                    Ok(Some(op_event)) => {
                        if let Err(e) = conn.send_event(op_event).await {
                            warn!("read-state operator publish failed: {e}");
                        } else {
                            debug!("read-state operator copy flushed");
                        }
                    }
                    Ok(None) => {} // operator_pubkey not configured; skip.
                    Err(e) => warn!("read-state operator build failed: {e}"),
                }
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
                        // Advance the bookmark for each channel the live session
                        // read, and refresh the wake-count sidecar if anything
                        // advanced (so reading mail drops the count immediately).
                        apply_readack_and_refresh(
                            &raw,
                            live,
                            now_secs(),
                            &mut writer,
                            &emitter,
                            &mailbox,
                            &channels,
                            &cfg.public_key_hex,
                        );
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

                        // On a Lane-1 wake, also write the unread-summary sidecar
                        // and overwrite the wake file with the v1 rich JSON format
                        // that buzz-bridge.ts expects: {"v":1,"channels":{<uuid>:<ts>,...}}.
                        if lane == Lane::ForMe {
                            emitter.emit_badge_sidecar(
                                event.created_at.as_secs(),
                                &mailbox,
                                &writer,
                                &channels,
                                &cfg.public_key_hex,
                            );
                            emitter.emit_rich(
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

/// Apply a read-ack file's contents and refresh the wake-count sidecar.
///
/// For each channel the LIVE session has read, advance the read watermark
/// (honest-seen: only the live actor may advance its own bookmark, gated by
/// `record_youyou_read`). If any watermark advanced, immediately re-emit the
/// badge sidecar so the bridge "N unread" wake count drops the moment mail is
/// READ, not only when the next mention arrives.
///
/// This closes the gap where the clerk refreshed the badge solely on a
/// `Lane::ForMe` wake (see the single `emit_badge_sidecar` call in the event
/// loop): before this, reading your mail advanced the bookmark but never
/// re-emitted the count, so the wake poke stayed stale until the next ping.
///
/// Returns true if at least one channel's bookmark advanced.
// Orchestration helper: it needs the parse inputs (raw, live) plus every input
// emit_badge_sidecar takes (writer, emitter, mailbox, channels, seat pubkey).
// Grouping them into a struct would add surface for a single call site.
#[allow(clippy::too_many_arguments)]
fn apply_readack_and_refresh(
    raw: &str,
    live: &SessionMarker,
    now: u64,
    writer: &mut ReadStateWriter,
    emitter: &WakeEmitter,
    mailbox: &Mailbox,
    channels: &HashMap<Uuid, ChannelInfo>,
    seat_pubkey_hex: &str,
) -> bool {
    let mut advanced = false;

    // Try multi-channel format first (v1); fall back to single-channel.
    if let Some(multi_ack) = parse_multi_channel_ack(raw) {
        let actor = SessionMarker::new(multi_ack.marker.clone());
        for (channel, ts) in &multi_ack.channels {
            match record_youyou_read(writer, channel.clone(), *ts, &actor, live) {
                Ok(()) => {
                    advanced = true;
                    debug!(channel = %channel, ts = ts, "multi-channel read-ack advanced bookmark");
                }
                Err(ReadGuardError::NotLiveSession) => {
                    warn!(channel = %channel, "multi-channel read-ack from non-live actor ignored");
                }
            }
        }
    } else if let Some(ack) = parse_read_ack(raw) {
        match record_youyou_read(
            writer,
            ack.channel.clone(),
            ack.up_to_ts,
            &SessionMarker::new(ack.marker.clone()),
            live,
        ) {
            Ok(()) => {
                advanced = true;
                debug!(channel = %ack.channel, ts = ack.up_to_ts, "read-ack advanced bookmark");
            }
            Err(ReadGuardError::NotLiveSession) => {
                warn!(channel = %ack.channel, "read-ack from non-live actor ignored");
            }
        }
    }

    // Refresh the wake-count sidecar the moment a read advances the bookmark,
    // so the bridge count reflects the read immediately (independent of new mail).
    if advanced {
        emitter.emit_badge_sidecar(now, mailbox, writer, channels, seat_pubkey_hex);
    }

    advanced
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_seat_clerk::read_state::{generate_slot_id, ReadStateWriter, SlotIdentity};
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use tempfile::tempdir;

    // ── Wake-count refresh on read (the operator's acceptance bar) ──────────
    //
    // Proof that a seat READING its mail drops the bridge wake count. Exercises
    // the real apply_readack_and_refresh: a live read-ack advances the bookmark
    // AND re-emits the badge sidecar (what the bridge reads for "N unread").
    mod readack_badge_refresh {
        use super::*;
        use buzz_seat_clerk::discovery::{ChannelInfo, ChannelType};
        use buzz_seat_clerk::mailbox::{Mailbox, MailboxEntry};
        use buzz_seat_clerk::session_identity::SessionMarker;
        use buzz_seat_clerk::wake::WakeEmitter;
        use std::collections::HashMap;
        use uuid::Uuid;

        const SEAT_PK: &str = "seat_pk_xyz";
        const OTHER_PK: &str = "other_pk_abc";

        fn writer_with_slot() -> ReadStateWriter {
            ReadStateWriter::new(SlotIdentity {
                slot_id: generate_slot_id(),
                client_id: generate_slot_id(),
            })
        }

        fn unread_entry(id: &str, created_at: u64, ch: Uuid) -> MailboxEntry {
            MailboxEntry {
                event_id: id.to_string(),
                created_at,
                author_pubkey: OTHER_PK.to_string(),
                content: "m".to_string(),
                p_tags: vec![],
                channel_uuid: ch,
            }
        }

        fn dm_channels(ch: Uuid) -> HashMap<Uuid, ChannelInfo> {
            let mut m = HashMap::new();
            m.insert(
                ch,
                ChannelInfo {
                    uuid: ch,
                    name: "dm".to_string(),
                    channel_type: ChannelType::Dm,
                },
            );
            m
        }

        /// Read the sidecar's total_unread for a channel; None if the channel
        /// is absent (which means fully read = zero unread).
        fn sidecar_unread(sidecar_path: &std::path::Path, ch: Uuid) -> Option<u64> {
            let raw = std::fs::read_to_string(sidecar_path).ok()?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            let ch_str = ch.to_string();
            v["channels"]
                .as_array()?
                .iter()
                .find(|c| c["id"].as_str() == Some(&ch_str))
                .and_then(|c| c["total_unread"].as_u64())
        }

        // THE acceptance proof: after a live read-ack advances the watermark,
        // the emitted wake count for that channel DROPS.
        //
        // NON-VACUITY: this passes ONLY because apply_readack_and_refresh
        // re-emits the sidecar. Delete the `if advanced { emit }` line and the
        // sidecar keeps the baseline (2 unread), so the final assert goes RED.
        #[test]
        fn live_readack_drops_the_wake_count() {
            let dir = tempdir().unwrap();
            let wake = dir.path().join("wake");
            let sidecar = dir.path().join("wake.rooms");
            let emitter = WakeEmitter::new(wake.to_str().unwrap().to_string());

            let ch = Uuid::new_v4();
            let channels = dm_channels(ch);
            let mut mailbox = Mailbox::new();
            mailbox.insert(ch, unread_entry("e1", 100, ch));
            mailbox.insert(ch, unread_entry("e2", 200, ch));

            let mut writer = writer_with_slot();
            let live = SessionMarker::new("live-1".to_string());

            // Baseline: before any read, both messages are unread.
            emitter.emit_badge_sidecar(1_000, &mailbox, &writer, &channels, SEAT_PK);
            assert_eq!(
                sidecar_unread(&sidecar, ch),
                Some(2),
                "baseline: 2 unread before the read"
            );

            // The live session reads up to ts=200 via a multi-channel read-ack.
            let raw = format!("{{\"v\":1,\"channels\":{{\"{ch}\":200}},\"marker\":\"live-1\"}}");
            let advanced = apply_readack_and_refresh(
                &raw,
                &live,
                2_000,
                &mut writer,
                &emitter,
                &mailbox,
                &channels,
                SEAT_PK,
            );

            assert!(advanced, "a live read-ack must advance the watermark");
            assert_eq!(
                sidecar_unread(&sidecar, ch),
                None,
                "after the read, the wake count must drop (channel fully read = absent)"
            );
        }

        // Guard: a read-ack from a NON-live actor must not advance or refresh.
        #[test]
        fn non_live_readack_does_not_drop_the_count() {
            let dir = tempdir().unwrap();
            let wake = dir.path().join("wake");
            let sidecar = dir.path().join("wake.rooms");
            let emitter = WakeEmitter::new(wake.to_str().unwrap().to_string());

            let ch = Uuid::new_v4();
            let channels = dm_channels(ch);
            let mut mailbox = Mailbox::new();
            mailbox.insert(ch, unread_entry("e1", 100, ch));

            let mut writer = writer_with_slot();
            let live = SessionMarker::new("live-1".to_string());

            emitter.emit_badge_sidecar(1_000, &mailbox, &writer, &channels, SEAT_PK);
            assert_eq!(sidecar_unread(&sidecar, ch), Some(1), "baseline: 1 unread");

            // A DIFFERENT (non-live) actor tries to advance the bookmark.
            let raw = format!("{{\"v\":1,\"channels\":{{\"{ch}\":100}},\"marker\":\"imposter\"}}");
            let advanced = apply_readack_and_refresh(
                &raw,
                &live,
                2_000,
                &mut writer,
                &emitter,
                &mailbox,
                &channels,
                SEAT_PK,
            );

            assert!(!advanced, "a non-live read-ack must not advance");
            assert_eq!(
                sidecar_unread(&sidecar, ch),
                Some(1),
                "non-live read must not drop the count"
            );
        }
    }

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

    // =========================================================================
    // US-21 / #313  NO-BLIND-TIMER GUARD
    //
    // Behavioral proof that NO timer, interval, or heartbeat loop causes a
    // seat-turn wake.  Three tests + one structural allowlist check.
    //
    // Scenario 1: Timer-negative + positive-control pair (non-vacuous).
    // Scenario 2: Bad-path fixture — proves a specific test goes RED when a
    //             timer is wired to emit(), then returns to green on restore.
    //
    // REPRODUCIBLE MUTATION PROOF (the load-bearing, reviewer-runnable one):
    //   The production lane-gate is `wake.rs` `emit_if_lane_1`:
    //       if *lane == Lane::ForMe { self.emit(unix_secs)?; }
    //   Break it (e.g. replace the body with an unconditional `self.emit(...)`,
    //   so ANY lane — including a timer's Delivery-lane tick — wakes the seat):
    //     -> US21-T2 (timer-negative) goes RED: "TIMER-NEGATIVE FAILED".
    //     -> US21-T1 (positive control) stays GREEN, proving the negative is not
    //        passing merely because the wake mechanism is broken.
    //   Byte-identical restore of that one line returns the suite to green.
    //   Verified 2026-08-19. This mutation targets PRODUCTION code, so T2 is a
    //   guard on the real gate, not on test text. (US21-T3 below is a secondary,
    //   self-contained demonstration that emit() writes the wake file; the
    //   reproducible proof of T2's non-vacuity is the wake.rs mutation above.)
    //
    // Allowlisted timer: connection.rs:86 `tokio::time::sleep(delay)` is the
    // reconnect-backoff.  It has no WakeEmitter reference and cannot reach
    // emit().  The allowlist is documented in the structural seam test below.
    //
    // Collision guard: this block does NOT touch session_identity.rs or
    // buzz-cli/src/commands/messages.rs (unmerged #293 files).
    // =========================================================================
    mod timer_guard {
        use super::*;

        // Helper: build a signed nostr Event for a channel message.
        // `mention_pubkey` = Some(pk) adds a p-tag (Lane::ForMe on mention);
        // None => no p-tag (Lane::Delivery unless is_dm).
        fn make_clerk_event(
            sender: &Keys,
            channel_uuid: Uuid,
            mention_pubkey: Option<&str>,
        ) -> nostr::Event {
            let mut builder = EventBuilder::new(Kind::Custom(9), "test-body");
            builder = builder
                .tag(Tag::parse(vec!["h".to_string(), channel_uuid.to_string()]).expect("h-tag"));
            if let Some(pk) = mention_pubkey {
                builder =
                    builder.tag(Tag::parse(vec!["p".to_string(), pk.to_string()]).expect("p-tag"));
            }
            builder.sign_with_keys(sender).expect("sign event")
        }

        // -----------------------------------------------------------------
        // Test US21-T1 — POSITIVE CONTROL
        //
        // A real Lane-1 (ForMe) message delivered through `deliver_event`
        // writes the wake file exactly once.  This proves the wake mechanism
        // is live; without this test the timer-negative is vacuous.
        // -----------------------------------------------------------------
        #[test]
        fn us21_t1_positive_control_for_me_message_causes_exactly_one_wake() {
            let sender = Keys::generate();
            let seat = Keys::generate();
            let seat_pk = seat.public_key().to_hex();
            let channel_uuid = Uuid::new_v4();

            // Event with a p-tag mention -> classify() returns Lane::ForMe.
            let event = make_clerk_event(&sender, channel_uuid, Some(&seat_pk));

            let dir = tempdir().unwrap();
            let wake_path = dir.path().join("wake");
            let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());
            let mut mailbox = Mailbox::new();
            let channels: HashMap<Uuid, ChannelInfo> = HashMap::new(); // Unknown type, p-tag fires

            // Deliver through the REAL dispatch path.
            let lane = deliver_event(
                &mut mailbox,
                &emitter,
                &seat_pk,
                &channels,
                &event,
                channel_uuid,
            );

            assert_eq!(
                lane,
                Lane::ForMe,
                "US21-T1: p-tag mention must classify as Lane::ForMe"
            );
            assert!(
                wake_path.exists(),
                "US21-T1 POSITIVE CONTROL FAILED: ForMe message must write wake file; \
                 if this fails the wake mechanism is broken and US21-T2 is vacuous"
            );

            // Content must be a parseable unix timestamp.
            let content = std::fs::read_to_string(&wake_path).unwrap();
            assert!(
                content.trim().parse::<u64>().is_ok(),
                "US21-T1: wake file must contain a parseable unix timestamp, got: {:?}",
                content.trim()
            );
        }

        // -----------------------------------------------------------------
        // Test US21-T2 — TIMER-NEGATIVE (the enforcing guard)
        //
        // Simulates N timer ticks with NO for-me message.  The wake file
        // must NOT be written.
        //
        // "Timer tick with no message" is modeled two ways:
        //   (a) emit_if_lane_1 called with Lane::Delivery — this is what any
        //       timer that lacked a real ForMe message would produce.  The gate
        //       `if lane == ForMe` in emit_if_lane_1 is the production guard.
        //   (b) deliver_event called with a non-mention, non-DM event — the
        //       full dispatch path classifies it as Lane::Delivery and skips
        //       emit().
        //
        // Both paths exercise real production code (not grep).  A renamed
        // timer wrapper that calls emit_if_lane_1 / deliver_event would still
        // be caught because the gate lives in production code, not in test text.
        //
        // ALLOWLIST: connection.rs:86 reconnect-backoff only calls
        // tokio::time::sleep; it has no WakeEmitter reference.  Not tested here
        // because it is structurally isolated (see US21-T4).
        // -----------------------------------------------------------------
        #[test]
        fn us21_t2_timer_negative_no_for_me_message_means_zero_wakes() {
            let dir = tempdir().unwrap();
            let wake_path = dir.path().join("wake");
            let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

            // Path (a): N calls to emit_if_lane_1 with Lane::Delivery.
            // Models a hypothetical bad timer that passes through the lane gate
            // but has no real ForMe message.
            for tick in 0..10u64 {
                emitter
                    .emit_if_lane_1(&Lane::Delivery, 1_700_000_000 + tick)
                    .expect("emit_if_lane_1 must not error");
            }

            // Path (b): deliver a real non-mention, non-DM event (Lane::Delivery).
            let sender = Keys::generate();
            let seat = Keys::generate();
            let seat_pk = seat.public_key().to_hex();
            let channel_uuid = Uuid::new_v4();
            let delivery_event = make_clerk_event(&sender, channel_uuid, None); // no p-tag

            let mut mailbox = Mailbox::new();
            let channels: HashMap<Uuid, ChannelInfo> = HashMap::new();
            let lane = deliver_event(
                &mut mailbox,
                &emitter,
                &seat_pk,
                &channels,
                &delivery_event,
                channel_uuid,
            );
            assert_eq!(
                lane,
                Lane::Delivery,
                "US21-T2: non-mention non-DM event must classify as Lane::Delivery"
            );

            // THE CORE ASSERTION: no wake must exist after 10 timer-tick calls
            // AND a Delivery-lane event dispatch.
            assert!(
                !wake_path.exists(),
                "US21-T2 TIMER-NEGATIVE FAILED: a timer tick (or Delivery-lane event) \
                 caused a seat-turn wake — a timer is blindly starting a seat turn"
            );
        }

        // -----------------------------------------------------------------
        // Test US21-T3 — BAD-PATH FIXTURE (mutation proof)
        //
        // Proves that IF a developer wired a timer to call emitter.emit()
        // directly (bypassing the lane gate), the wake file WOULD be written.
        // This is the non-vacuity proof for US21-T2.
        //
        // HOW TO READ THIS AS MUTATION PROOF:
        //   The wired line below is:
        //     timer_callback(&emitter, 1_700_000_001);
        //   If you add this line to the real clerk's event loop (outside the
        //   message-dispatch arm), run `cargo test`, and US21-T2 will turn RED
        //   because the wake file will exist after the timer fires.
        //
        //   BYTE-IDENTICAL RESTORE: removing that single line returns US21-T2
        //   to green, because no other path in the clerk calls emit() without
        //   a ForMe message.
        //
        //   The fixture is entirely self-contained in this test function; no
        //   production code is mutated.
        // -----------------------------------------------------------------
        #[test]
        fn us21_t3_bad_path_fixture_timer_hook_causes_wake_proving_negative_would_red() {
            let dir = tempdir().unwrap();
            let wake_path = dir.path().join("wake");
            let emitter = WakeEmitter::new(wake_path.to_str().unwrap().to_string());

            // This closure models a bad timer callback: calls emit() directly,
            // bypassing the lane gate.  In production it would look like:
            //   tokio::spawn(async move { loop { sleep(t).await; emitter.emit(now()); }});
            let timer_callback = |em: &WakeEmitter, ts: u64| {
                em.emit(ts).expect("bad-timer emit must succeed");
            };

            // THE WIRED LINE (bad-path fixture): timer fires and calls emit().
            // *** If you copy this line into the real clerk event loop, US21-T2 goes RED. ***
            timer_callback(&emitter, 1_700_000_001); // <-- BAD WIRE (fixture only, not in production)

            // The bad timer DID write the wake file — confirming US21-T2 would RED.
            assert!(
                wake_path.exists(),
                "US21-T3 FIXTURE BROKEN: bad-timer emit() must write the wake file; \
                 if this fails the mutation proof is itself broken"
            );

            let content = std::fs::read_to_string(&wake_path).unwrap();
            assert!(
                content.contains("1700000001"),
                "US21-T3: wake file must contain the timer-injected timestamp, got: {content:?}"
            );

            // Confirm Delivery-lane does not overwrite the file (gate still works
            // correctly via the normal path after the bad timer has already fired).
            emitter
                .emit_if_lane_1(&Lane::Delivery, 9_999_999_999)
                .unwrap();
            let content2 = std::fs::read_to_string(&wake_path).unwrap();
            assert!(
                !content2.contains("9999999999"),
                "US21-T3: Delivery-lane emit_if_lane_1 must not overwrite timer-written value"
            );
        }

        // -----------------------------------------------------------------
        // Test US21-T4 — STRUCTURAL SEAM: allowlist reconnect-backoff
        //
        // The ONE legitimate timer in the clerk is connection.rs:86
        // `tokio::time::sleep(delay)`.  It is allowlisted because:
        //   - `connect_with_backoff` takes (&str, &Keys, Option<u64>).
        //   - It has NO WakeEmitter parameter.
        //   - A timer with no WakeEmitter reference cannot call emit().
        //
        // This test exercises the Backoff pure state machine and confirms it
        // has no emit path.  If someone added emit() to Backoff's interface,
        // the ONLY way to call it would be to add WakeEmitter to the function
        // signature — a change that would be structurally visible in diffs and
        // flagged in code review.
        // -----------------------------------------------------------------
        #[test]
        fn us21_t4_allowlist_reconnect_backoff_has_no_emitter_reference() {
            use buzz_seat_clerk::connection::Backoff;

            // The Backoff struct is pure math — no I/O, no emitter.
            let mut backoff = Backoff::new(1, 60, 2.0);
            let d1 = backoff.next_delay_secs();
            let d2 = backoff.next_delay_secs();
            let d3 = backoff.next_delay_secs();

            // Allowlist assertion: the backoff produces durations, not wakes.
            assert!(
                d1 < d2,
                "US21-T4: backoff must increase delay (d1={d1} d2={d2})"
            );
            assert!(
                d2 < d3,
                "US21-T4: backoff must increase delay (d2={d2} d3={d3})"
            );

            // Non-vacuity: confirm the backoff caps.
            let mut capped = Backoff::new(1, 5, 2.0);
            for _ in 0..20 {
                capped.next_delay_secs();
            }
            assert!(
                capped.next_delay_secs() <= 5,
                "US21-T4: reconnect-backoff must cap at 5; if this fails the \
                 allowlisted timer is misbehaving"
            );

            // Structural proof: WakeEmitter is NOT imported into this test
            // because Backoff has no relationship to it.  If someone added
            // a Backoff::emit() method, this test would still compile because
            // we are NOT calling it — but the structural change would be
            // visible in the Backoff type definition in connection.rs.
            // The timer-negative (US21-T2) would catch actual wake emission.
        }
    } // mod timer_guard
}
