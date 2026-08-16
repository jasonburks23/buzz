//! REST channel discovery.
//!
//! Mirrors `buzz-acp::relay::discover_channels` (relay.rs:687).
//! Step 1: query kind:39002 with `#p = seat_pubkey` to find member-channel UUIDs.
//! Step 2: query kind:39000 for those UUIDs to get names and types.
//! Step 3: skip channels with an `archived = "true"` tag.

use std::collections::HashMap;

use base64::Engine as _;
use nostr::{EventBuilder, JsonUtil, Kind, Tag};
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::debug;
use uuid::Uuid;

use crate::config::ListenerConfig;
use crate::error::ClerkError;
use buzz_core::kind::{KIND_NIP29_GROUP_MEMBERS, KIND_NIP29_GROUP_METADATA, KIND_READ_STATE};

// BORROW-OPPORTUNITY(SP-2): make_nip98_post_token duplicates
// nostr::nips::nip98::HttpData token generation. When nostr 0.44 exposes
// a stable public API for this, replace this function with the upstream
// helper and delete this copy. Track in refactor-282 SP-2.
/// Build a NIP-98 HTTP Auth token (kind-27235) for a POST request.
///
/// Tag set mirrors crates/buzz-cli/src/client.rs `sign_nip98`:
///   ["u", url], ["method", "POST"], ["nonce", uuid-v4], ["payload", hex(sha256(body))]
///
/// The body bytes are hashed here so the hash matches the bytes actually sent.
/// Never logs the signing key, the token, or message content.
fn make_nip98_post_token(
    cfg: &ListenerConfig,
    url: &str,
    body: &[u8],
) -> Result<String, ClerkError> {
    let payload_hash = hex::encode(Sha256::digest(body));
    let nonce = uuid::Uuid::new_v4().to_string();
    let tags = vec![
        Tag::parse(["u", url]).map_err(|e| ClerkError::Discovery(format!("build u tag: {e}")))?,
        Tag::parse(["method", "POST"])
            .map_err(|e| ClerkError::Discovery(format!("build method tag: {e}")))?,
        Tag::parse(["nonce", &nonce])
            .map_err(|e| ClerkError::Discovery(format!("build nonce tag: {e}")))?,
        Tag::parse(["payload", &payload_hash])
            .map_err(|e| ClerkError::Discovery(format!("build payload tag: {e}")))?,
    ];
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .tags(tags)
        .sign_with_keys(&cfg.keys)
        .map_err(|e| ClerkError::Discovery(format!("NIP-98 signing failed: {e}")))?;
    let json = event.as_json();
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
    ))
}

/// Metadata about a discovered channel.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub uuid: Uuid,
    pub name: String,
    pub channel_type: ChannelType,
}

/// The kind (category) of a Buzz channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelType {
    /// Direct-message channel.
    Dm,
    /// Streaming / team chat channel.
    Stream,
    /// Unrecognised channel type.
    Unknown,
}

/// Extract UUIDs from kind:39002 member events (the `d` tag value).
///
/// Invalid UUID strings are silently skipped so that a single bad event
/// cannot abort discovery of all channels.
pub fn extract_member_channel_uuids(events: &Value) -> Result<Vec<Uuid>, ClerkError> {
    let arr = events
        .as_array()
        .ok_or_else(|| ClerkError::Discovery("expected JSON array for member events".into()))?;
    let mut uuids = Vec::new();
    for ev in arr {
        if let Some(tags) = ev.get("tags").and_then(|t| t.as_array()) {
            for tag in tags {
                if let Some(a) = tag.as_array() {
                    if a.first().and_then(|v| v.as_str()) == Some("d") {
                        if let Some(val) = a.get(1).and_then(|v| v.as_str()) {
                            if let Ok(uuid) = val.parse::<Uuid>() {
                                uuids.push(uuid);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(uuids)
}

/// Build a `HashMap<Uuid, ChannelInfo>` from kind:39000 metadata events.
///
/// Only UUIDs present in `uuids` are included. Channels that carry an
/// `archived = "true"` tag are excluded.
pub fn merge_channel_info(uuids: Vec<Uuid>, meta_events: &Value) -> HashMap<Uuid, ChannelInfo> {
    let Some(arr) = meta_events.as_array() else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for ev in arr {
        let Some(tags) = ev.get("tags").and_then(|t| t.as_array()) else {
            continue;
        };
        let mut d_uuid: Option<Uuid> = None;
        let mut name = String::new();
        let mut channel_type = ChannelType::Unknown;
        // The live relay encodes channel type as a topic tag `["t","dm"]` /
        // `["t","stream"]`, not a `channel_type` tag. Capture it here and resolve
        // after the loop so an explicit `channel_type` tag (if a relay ever sends
        // one) still wins, but a `t` marker is honored when it is all we get.
        let mut t_type = ChannelType::Unknown;
        let mut archived = false;
        for tag in tags {
            let Some(a) = tag.as_array() else { continue };
            match a.first().and_then(|v| v.as_str()) {
                Some("d") => {
                    if let Some(v) = a.get(1).and_then(|v| v.as_str()) {
                        d_uuid = v.parse::<Uuid>().ok();
                    }
                }
                Some("name") => {
                    name = a.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string();
                }
                Some("channel_type") => {
                    channel_type = match a.get(1).and_then(|v| v.as_str()) {
                        Some("dm") => ChannelType::Dm,
                        Some("stream") => ChannelType::Stream,
                        _ => ChannelType::Unknown,
                    };
                }
                Some("t") => {
                    if let Some(v) = a.get(1).and_then(|v| v.as_str()) {
                        match v {
                            "dm" => t_type = ChannelType::Dm,
                            "stream" => t_type = ChannelType::Stream,
                            _ => {}
                        }
                    }
                }
                Some("archived") => {
                    archived = a.get(1).and_then(|v| v.as_str()) == Some("true");
                }
                _ => {}
            }
        }
        // Fall back to the `t` topic marker when no explicit channel_type tag set it.
        if channel_type == ChannelType::Unknown {
            channel_type = t_type;
        }
        if archived {
            continue;
        }
        if let Some(uuid) = d_uuid {
            if uuids.contains(&uuid) {
                map.insert(
                    uuid,
                    ChannelInfo {
                        uuid,
                        name,
                        channel_type,
                    },
                );
            }
        }
    }
    map
}

/// Fetch the seat's own kind-30078 read-state event from the relay, if any.
///
/// Returns `Some((ciphertext, created_at))` for the event with the greatest `created_at`,
/// or `None` if no matching event exists.
///
/// SECURITY: never log `ciphertext`, decrypted content, or secret-key material.
pub async fn fetch_own_read_state(
    http: &Client,
    relay_http_url: &str,
    seat_pubkey_hex: &str,
    slot_id: &str,
    cfg: &ListenerConfig,
) -> Result<Option<(String, u64)>, ClerkError> {
    let d_tag_value = format!("read-state:{slot_id}");
    let filter = serde_json::json!({
        "kinds": [KIND_READ_STATE as u64],
        "authors": [seat_pubkey_hex],
        "#d": [d_tag_value],
    });
    let body = serde_json::to_vec(&[&filter])?;
    let query_url = format!("{relay_http_url}/query");
    let token = make_nip98_post_token(cfg, &query_url, &body)?;
    let response: Value = http
        .post(&query_url)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?
        .json()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?;

    let Some(arr) = response.as_array() else {
        return Ok(None);
    };
    // Pick the event with the greatest created_at.
    let best = arr
        .iter()
        .filter_map(|ev| {
            let content = ev.get("content")?.as_str()?.to_string();
            let created_at = ev.get("created_at")?.as_u64()?;
            Some((content, created_at))
        })
        .max_by_key(|(_, ts)| *ts);

    Ok(best)
}

// BORROW-OPPORTUNITY(future): discover_channels logic is duplicated in at
// least 3 locations across the codebase. Largest single borrow opportunity.
// Needs an upstream PR to extract into a shared buzz-sdk crate. Keep our
// copy until that PR lands.
/// Perform the full two-step REST discovery against the Buzz relay HTTP bridge.
///
/// * `relay_http_url` - HTTP base URL of the relay (e.g. `http://localhost:3000`).
/// * `seat_pubkey_hex` - the seat's hex-encoded public key.
/// * `cfg` - clerk config providing the signing keys for NIP-98 auth tokens.
///
/// Each request builds its own NIP-98 token from the exact bytes being sent so
/// the relay's payload-hash check passes. The token is passed as a header value
/// only and is never recorded in tracing spans.
pub async fn discover_channels(
    http: &Client,
    relay_http_url: &str,
    seat_pubkey_hex: &str,
    cfg: &ListenerConfig,
) -> Result<HashMap<Uuid, ChannelInfo>, ClerkError> {
    // Step 1: kind:39002 where #p = seat_pubkey
    // Filter shape mirrors buzz-cli's cmd_list_channels member path (channels.rs):
    //   {"kinds":[39002],"#p":[pubkey_hex]}
    let member_filter = serde_json::json!({
        "kinds": [KIND_NIP29_GROUP_MEMBERS],
        "#p": [seat_pubkey_hex],
    });
    let member_body = serde_json::to_vec(&[&member_filter])?;
    let query_url = format!("{relay_http_url}/query");
    let member_token = make_nip98_post_token(cfg, &query_url, &member_body)?;
    let member_events: Value = http
        .post(&query_url)
        .header("Authorization", member_token)
        .header("Content-Type", "application/json")
        .body(member_body)
        .send()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?
        .json()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?;

    let uuids = extract_member_channel_uuids(&member_events)?;
    if uuids.is_empty() {
        debug!("discovered 0 channel(s)");
        return Ok(HashMap::new());
    }

    // Step 2: kind:39000 for discovered UUIDs
    // Filter shape mirrors buzz-cli's cmd_list_channels member path (channels.rs):
    //   {"kinds":[39000],"#d":[uuid1, uuid2, ...]}
    let d_values: Vec<String> = uuids.iter().map(|u| u.to_string()).collect();
    let meta_filter = serde_json::json!({
        "kinds": [KIND_NIP29_GROUP_METADATA],
        "#d": d_values,
    });
    let meta_body = serde_json::to_vec(&[&meta_filter])?;
    let meta_token = make_nip98_post_token(cfg, &query_url, &meta_body)?;
    let meta_events: Value = http
        .post(&query_url)
        .header("Authorization", meta_token)
        .header("Content-Type", "application/json")
        .body(meta_body)
        .send()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?
        .json()
        .await
        .map_err(|e| ClerkError::Discovery(e.to_string()))?;

    let map = merge_channel_info(uuids, &meta_events);
    debug!("discovered {} channel(s)", map.len());
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_member_event(channel_uuid: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": 39002,
            "tags": [["d", channel_uuid], ["p", "aabbcc"]],
            "content": "",
            "created_at": 1000
        })
    }

    fn make_meta_event(channel_uuid: &str, name: &str, channel_type: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": 39000,
            "tags": [
                ["d", channel_uuid],
                ["name", name],
                ["channel_type", channel_type]
            ],
            "content": "",
            "created_at": 1000
        })
    }

    #[test]
    fn parse_member_events_extracts_uuids() {
        let uuid_str = Uuid::new_v4().to_string();
        let events = serde_json::json!([make_member_event(&uuid_str)]);
        let uuids = extract_member_channel_uuids(&events).unwrap();
        assert_eq!(uuids.len(), 1);
        assert_eq!(uuids[0].to_string(), uuid_str);
    }

    #[test]
    fn parse_member_events_skips_invalid_uuids() {
        let events = serde_json::json!([make_member_event("not-a-uuid")]);
        let uuids = extract_member_channel_uuids(&events).unwrap();
        assert!(uuids.is_empty());
    }

    #[test]
    fn build_channel_info_from_meta_events() {
        let uuid_str = Uuid::new_v4().to_string();
        let uuid = uuid_str.parse::<Uuid>().unwrap();
        let meta_events = serde_json::json!([make_meta_event(&uuid_str, "team-chat", "stream")]);
        let map = merge_channel_info(vec![uuid], &meta_events);
        assert_eq!(map.len(), 1);
        let info = &map[&uuid];
        assert_eq!(info.name, "team-chat");
        assert_eq!(info.channel_type, ChannelType::Stream);
    }

    #[test]
    fn build_channel_info_dm_type() {
        let uuid_str = Uuid::new_v4().to_string();
        let uuid = uuid_str.parse::<Uuid>().unwrap();
        let meta_events = serde_json::json!([make_meta_event(&uuid_str, "dm-room", "dm")]);
        let map = merge_channel_info(vec![uuid], &meta_events);
        assert_eq!(map[&uuid].channel_type, ChannelType::Dm);
    }

    #[test]
    fn build_channel_info_dm_from_relay_t_tag() {
        // The live relay marks a DM with ["t","dm"] (plus bare "private"/"closed"
        // markers and name "DM"), NOT a ["channel_type","dm"] tag. Discovery must
        // recognize this real shape as a DM, or every DM classifies as a plain
        // channel and only @mentions wake the seat.
        let uuid_str = Uuid::new_v4().to_string();
        let uuid = uuid_str.parse::<Uuid>().unwrap();
        let meta_events = serde_json::json!([{
            "kind": 39000,
            "tags": [
                ["d", uuid_str],
                ["name", "DM"],
                ["private"],
                ["hidden"],
                ["p", "2f0e192ac3cd7028f6e898b52714cb688f86d6fbc04ac6d34228fb26f6e1ef3b"],
                ["p", "7575874aad5870d8a534ac924766ad6f3613ce845f223d85e4f3691413a05b79"],
                ["closed"],
                ["t", "dm"]
            ],
            "content": "",
            "created_at": 1000
        }]);
        let map = merge_channel_info(vec![uuid], &meta_events);
        assert_eq!(map[&uuid].channel_type, ChannelType::Dm);
    }

    #[test]
    fn archived_channels_skipped_in_merge() {
        let uuid_str = Uuid::new_v4().to_string();
        let uuid = uuid_str.parse::<Uuid>().unwrap();
        let meta_events = serde_json::json!([{
            "kind": 39000,
            "tags": [["d", uuid_str], ["archived", "true"]],
            "content": "", "created_at": 1000
        }]);
        let map = merge_channel_info(vec![uuid], &meta_events);
        assert!(map.is_empty(), "archived channels must be excluded");
    }

    /// NIP-98 token carries a payload tag whose hash equals sha256 of the body.
    ///
    /// This is the fix regression-guard: before the fix, the token had no payload
    /// tag and the relay rejected it with an empty result.
    #[test]
    fn nip98_token_payload_tag_matches_body_hash() {
        // Minimal deterministic keys (scalar = 1 is the smallest valid secp256k1 key).
        let keys =
            nostr::Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
                .expect("valid test key");
        let cfg = crate::config::ListenerConfig {
            keys,
            public_key_hex: String::new(),
            relay_url: String::new(),
            wake_file: String::new(),
            readack_file: String::new(),
        };

        // Use a concrete body matching the member-filter shape (no raw-string tricks needed).
        let body = b"[{\"kinds\":[39002],\"#p\":[\"deadbeef\"]}]";
        let url = "https://relay.example/query";

        let token = make_nip98_post_token(&cfg, url, body).expect("token must build");

        // Decode and parse the signed event JSON from the Nostr header.
        let encoded = token.strip_prefix("Nostr ").expect("Nostr prefix");
        let json_bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64 decode");
        let event = nostr::Event::from_json(std::str::from_utf8(&json_bytes).expect("utf8"))
            .expect("valid event JSON");

        // Verify the event is well-formed.
        event.verify().expect("event signature valid");

        // The event must be kind 27235.
        assert_eq!(event.kind.as_u16(), 27235, "must be kind 27235");

        // Collect tags into a searchable Vec<Vec<String>>.
        let tags: Vec<Vec<String>> = event.tags.iter().map(|t| t.as_slice().to_vec()).collect();

        // "u" tag must equal the exact URL.
        assert!(
            tags.iter().any(|t| t.as_slice() == ["u", url]),
            "u tag must equal the request URL"
        );

        // "method" tag must be POST.
        assert!(
            tags.iter().any(|t| t.as_slice() == ["method", "POST"]),
            "method tag must be POST"
        );

        // "nonce" tag must be present.
        assert!(
            tags.iter()
                .any(|t| t.first().map(String::as_str) == Some("nonce")),
            "nonce tag must be present"
        );

        // "payload" tag must be present and its value must equal hex(sha256(body)).
        let expected_hash = hex::encode(Sha256::digest(body));
        let payload_tag = tags
            .iter()
            .find(|t| t.first().map(String::as_str) == Some("payload"))
            .expect("payload tag must be present");
        assert_eq!(
            payload_tag.get(1).map(String::as_str),
            Some(expected_hash.as_str()),
            "payload hash must equal sha256 of the request body"
        );
    }
}
