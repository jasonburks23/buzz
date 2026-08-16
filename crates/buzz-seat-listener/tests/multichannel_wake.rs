//! Two-channel catch-up integration test.
//!
//! Proves both channels are detected independently by the wake map and
//! pending-channel diffing logic.
//!
//! This test is marked #[ignore] by default (same as other live-relay tests).
//! Run with: cargo test -p buzz-seat-listener --test multichannel_wake -- --include-ignored
//!
//! RELAY SAFETY: uses only the ISOLATED test relay on port 3099. Never port 3000.

use buzz_seat_listener::lane::Lane;
use buzz_seat_listener::read_ack::{
    parse_multi_channel_ack, write_multi_channel_ack, MultiChannelAck,
};
use buzz_seat_listener::wake::WakeEmitter;
use std::collections::HashMap;
use tempfile::tempdir;

/// Compute pending channels: those where wake_ts > ack_ts (or absent in ack).
fn pending_channels(
    wake_channels: &HashMap<String, u64>,
    ack: Option<&MultiChannelAck>,
) -> Vec<String> {
    let ack_map = ack.map(|a| &a.channels);
    wake_channels
        .iter()
        .filter(|(uuid, wake_ts)| {
            let ack_ts = ack_map.and_then(|m| m.get(*uuid)).copied().unwrap_or(0);
            **wake_ts > ack_ts
        })
        .map(|(uuid, _)| uuid.clone())
        .collect()
}

#[tokio::test]
#[ignore]
async fn two_channel_catch_up() {
    // Two-channel catch-up: proves both channels detected independently.
    //
    // Scenario:
    // 1. Inject two channels (chan-a, chan-b) with one for-me message each,
    //    at timestamps 1000 and 2000. Simulate the listener being DOWN (no ack file).
    // 2. Call emit_if_lane_1_for_channel for each channel.
    // 3. Read the wake.json file; assert chan-a == 1000 and chan-b == 2000.
    // 4. Assert both UUIDs are in pending_channels (no ack).
    // 5. Write a partial ack (chan-a only, ts=1000). Re-diff: assert only chan-b remains pending.
    // 6. Write full ack (both channels). Re-diff: assert pending is empty.

    let dir = tempdir().unwrap();
    let wake_path = dir.path().join("wake.json");
    let ack_path = dir.path().join("readack.json");
    let wake_path_str = wake_path.display().to_string();
    let ack_path_str = ack_path.display().to_string();

    let emitter = WakeEmitter::new(wake_path_str.clone());

    // Step 2: emit for both channels.
    emitter
        .emit_if_lane_1_for_channel(&Lane::ForMe, "chan-a", 1000)
        .unwrap();
    emitter
        .emit_if_lane_1_for_channel(&Lane::ForMe, "chan-b", 2000)
        .unwrap();

    // Step 3: read wake.json and assert.
    let raw = std::fs::read_to_string(&wake_path).unwrap();
    let wake_val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        wake_val["channels"]["chan-a"], 1000u64,
        "chan-a must be 1000"
    );
    assert_eq!(
        wake_val["channels"]["chan-b"], 2000u64,
        "chan-b must be 2000"
    );

    // Build a HashMap for diffing.
    let wake_channels: HashMap<String, u64> = [
        ("chan-a".to_string(), 1000u64),
        ("chan-b".to_string(), 2000u64),
    ]
    .into_iter()
    .collect();

    // Step 4: no ack file -- both channels pending.
    let mut pending = pending_channels(&wake_channels, None);
    pending.sort();
    assert_eq!(
        pending,
        vec!["chan-a", "chan-b"],
        "both channels must be pending with no ack"
    );

    // Step 5: partial ack (chan-a only).
    let partial_ack = MultiChannelAck {
        v: 1,
        channels: [("chan-a".to_string(), 1000u64)].into_iter().collect(),
        marker: "test-session".to_string(),
    };
    write_multi_channel_ack(&ack_path_str, &partial_ack).unwrap();
    let ack_raw = std::fs::read_to_string(&ack_path).unwrap();
    let parsed_ack = parse_multi_channel_ack(&ack_raw).unwrap();
    let mut pending2 = pending_channels(&wake_channels, Some(&parsed_ack));
    pending2.sort();
    assert_eq!(
        pending2,
        vec!["chan-b"],
        "only chan-b must remain pending after partial ack"
    );

    // Step 6: full ack.
    let full_ack = MultiChannelAck {
        v: 1,
        channels: [
            ("chan-a".to_string(), 1000u64),
            ("chan-b".to_string(), 2000u64),
        ]
        .into_iter()
        .collect(),
        marker: "test-session".to_string(),
    };
    write_multi_channel_ack(&ack_path_str, &full_ack).unwrap();
    let ack_raw2 = std::fs::read_to_string(&ack_path).unwrap();
    let parsed_ack2 = parse_multi_channel_ack(&ack_raw2).unwrap();
    let pending3 = pending_channels(&wake_channels, Some(&parsed_ack2));
    assert!(pending3.is_empty(), "pending must be empty after full ack");
}
