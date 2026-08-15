//! REST channel discovery.
//!
//! Mirrors `buzz-acp::relay::discover_channels` (relay.rs:687).
//! Step 1: query kind:39002 with `#p = seat_pubkey` to find member-channel UUIDs.
//! Step 2: query kind:39000 for those UUIDs to get names and types.
//! Step 3: skip channels with an `archived = "true"` tag.

use std::collections::HashMap;

use nostr::{Alphabet, Filter, Kind, SingleLetterTag};
use reqwest::Client;
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::error::ClerkError;
use buzz_core::kind::{KIND_NIP29_GROUP_MEMBERS, KIND_NIP29_GROUP_METADATA};

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
                Some("archived") => {
                    archived = a.get(1).and_then(|v| v.as_str()) == Some("true");
                }
                _ => {}
            }
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

/// Perform the full two-step REST discovery against the Buzz relay HTTP bridge.
///
/// * `relay_http_url` - HTTP base URL of the relay (e.g. `http://localhost:3000`).
/// * `seat_pubkey_hex` - the seat's hex-encoded public key.
/// * `nip98_token` - NIP-98 Authorization header value (built by the caller).
///
/// The function does not log secrets. The `nip98_token` is passed as a header
/// value only and is never recorded in tracing spans.
pub async fn discover_channels(
    http: &Client,
    relay_http_url: &str,
    seat_pubkey_hex: &str,
    nip98_token: &str,
) -> Result<HashMap<Uuid, ChannelInfo>, ClerkError> {
    // Step 1: kind:39002 where #p = seat_pubkey
    let p_tag = SingleLetterTag::lowercase(Alphabet::P);
    let member_filter = Filter::new()
        .kind(Kind::Custom(KIND_NIP29_GROUP_MEMBERS as u16))
        .custom_tags(p_tag, [seat_pubkey_hex]);
    let member_body = serde_json::to_vec(&[member_filter])?;
    let member_events: Value = http
        .post(format!("{relay_http_url}/query"))
        .header("Authorization", nip98_token)
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
    let d_tag = SingleLetterTag::lowercase(Alphabet::D);
    let d_values: Vec<String> = uuids.iter().map(|u| u.to_string()).collect();
    let meta_filter = Filter::new()
        .kind(Kind::Custom(KIND_NIP29_GROUP_METADATA as u16))
        .custom_tags(d_tag, d_values);
    let meta_body = serde_json::to_vec(&[meta_filter])?;
    let meta_events: Value = http
        .post(format!("{relay_http_url}/query"))
        .header("Authorization", nip98_token)
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
}
