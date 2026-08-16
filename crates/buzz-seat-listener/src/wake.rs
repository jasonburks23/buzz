//! Wake signal emitter.
//!
//! On Lane-1 (ForMe) events, writes a per-channel JSON wake map to a configured file.
//! The supervisor (launchd / Hermes-later) watches this file via WatchPaths.
//! Lane-2/3 (Delivery) events do NOT trigger a write.
//!
//! NOTE: terminal-keystroke injection is explicitly rejected (Hermes-gated).

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::error::ClerkError;
use crate::lane::Lane;

/// Per-channel wake map written atomically to the wake file.
/// JSON: {"v":1,"channels":{"<uuid>":<unix_secs>,...}}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WakeMap {
    v: u8,
    channels: HashMap<String, u64>,
}

/// Read the wake map from `path`, returning a default empty map on any error.
fn read_wake_map(path: &str) -> WakeMap {
    (|| {
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str::<WakeMap>(&raw).ok()
    })()
    .unwrap_or_default()
}

/// Write `map` to `path` atomically via a temp file in the same directory.
fn write_wake_map_atomic(path: &str, map: &WakeMap) -> Result<(), ClerkError> {
    let json = serde_json::to_string(map)?;
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Writes a wake signal to a file when a Lane-1 (ForMe) message arrives.
///
/// Create one instance per listener process and call `emit_if_lane_1_for_channel` on
/// each delivered message. The file path is set at construction time.
pub struct WakeEmitter {
    wake_file_path: String,
}

impl WakeEmitter {
    /// Create a new WakeEmitter that writes signals to `wake_file_path`.
    pub fn new(wake_file_path: String) -> Self {
        Self { wake_file_path }
    }

    /// Write `<unix_secs>\n` to the wake file, overwriting any previous signal.
    #[deprecated(note = "use emit_for_channel")]
    pub fn emit(&self, unix_secs: u64) -> Result<(), ClerkError> {
        fs::write(&self.wake_file_path, format!("{unix_secs}\n"))?;
        Ok(())
    }

    /// Emit only if `lane` is `Lane::ForMe`. No-op for `Lane::Delivery`.
    #[deprecated(note = "use emit_if_lane_1_for_channel")]
    pub fn emit_if_lane_1(&self, lane: &Lane, unix_secs: u64) -> Result<(), ClerkError> {
        if *lane == Lane::ForMe {
            #[allow(deprecated)]
            self.emit(unix_secs)?;
        }
        Ok(())
    }

    /// Update only the triggering channel's key in the per-channel wake map
    /// and write atomically. Does NOT touch other channels' keys.
    pub fn emit_for_channel(&self, channel_uuid: &str, unix_secs: u64) -> Result<(), ClerkError> {
        let mut map = read_wake_map(&self.wake_file_path);
        map.v = 1;
        map.channels.insert(channel_uuid.to_string(), unix_secs);
        write_wake_map_atomic(&self.wake_file_path, &map)
    }

    /// Emit only if lane is ForMe; update the channel key in the per-channel map.
    pub fn emit_if_lane_1_for_channel(
        &self,
        lane: &Lane,
        channel_uuid: &str,
        unix_secs: u64,
    ) -> Result<(), ClerkError> {
        if *lane == Lane::ForMe {
            self.emit_for_channel(channel_uuid, unix_secs)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn emit_writes_timestamp_to_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        #[allow(deprecated)]
        emitter.emit(1_700_000_000).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("1700000000"),
            "wake file must contain the timestamp"
        );
    }

    #[test]
    fn emit_overwrites_previous_signal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        #[allow(deprecated)]
        emitter.emit(1_000).unwrap();
        #[allow(deprecated)]
        emitter.emit(2_000).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("2000"));
        assert!(!content.contains("1000"), "old signal must be overwritten");
    }

    #[test]
    fn lane_delivery_does_not_emit() {
        use crate::lane::Lane;
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        #[allow(deprecated)]
        emitter.emit_if_lane_1(&Lane::Delivery, 1_000).unwrap();
        assert!(!path.exists(), "Delivery lane must not write wake file");
    }

    #[test]
    fn lane_for_me_does_emit() {
        use crate::lane::Lane;
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        #[allow(deprecated)]
        emitter.emit_if_lane_1(&Lane::ForMe, 1_700_000_000).unwrap();
        assert!(path.exists(), "ForMe lane must write wake file");
    }

    #[test]
    fn emit_for_channel_creates_map() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake.json");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_for_channel("chan-a", 1_700_000_000).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let map: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(map["v"], 1, "v must be 1");
        assert_eq!(
            map["channels"]["chan-a"], 1_700_000_000u64,
            "channel key must be present with correct ts"
        );
    }

    #[test]
    fn emit_for_channel_updates_only_triggering_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake.json");
        // Pre-seed the map with two channels.
        let initial = WakeMap {
            v: 1,
            channels: [
                ("chan-a".to_string(), 100u64),
                ("chan-b".to_string(), 200u64),
            ]
            .into_iter()
            .collect(),
        };
        std::fs::write(&path, serde_json::to_string(&initial).unwrap()).unwrap();
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_for_channel("chan-a", 300).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let map: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(map["channels"]["chan-a"], 300u64, "chan-a must be updated");
        assert_eq!(
            map["channels"]["chan-b"], 200u64,
            "chan-b must be unchanged"
        );
    }

    #[test]
    fn emit_for_channel_atomic_no_partial_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake.json");
        // Pre-seed with 10 keys.
        let initial = WakeMap {
            v: 1,
            channels: (0..10)
                .map(|i| (format!("chan-{i}"), i as u64 * 100))
                .collect(),
        };
        std::fs::write(&path, serde_json::to_string(&initial).unwrap()).unwrap();
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_for_channel("chan-0", 9999).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Must be valid JSON (no truncation).
        let map: serde_json::Value =
            serde_json::from_str(&raw).expect("must be valid JSON after atomic write");
        assert_eq!(map["channels"]["chan-0"], 9999u64);
        // All other keys still present.
        for i in 1..10 {
            assert!(
                map["channels"][format!("chan-{i}")].is_number(),
                "chan-{i} must still be present"
            );
        }
    }

    #[test]
    fn emit_if_lane_1_for_channel_delivery_no_op() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake.json");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter
            .emit_if_lane_1_for_channel(&Lane::Delivery, "chan-a", 1000)
            .unwrap();
        assert!(!path.exists(), "Delivery lane must not write wake file");
    }
}
