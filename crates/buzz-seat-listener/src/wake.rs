//! Wake signal emitter.
//!
//! On Lane-1 (ForMe) events, writes `<unix_secs>\n` to a configured file.
//! The supervisor (launchd / Hermes-later) watches this file via `WatchPaths`.
//! Lane-2/3 (Delivery) events do NOT trigger a write.
//!
//! NOTE: terminal-keystroke injection is explicitly rejected (Hermes-gated).

use std::fs;

use crate::error::ClerkError;
use crate::lane::Lane;

/// Writes a wake signal to a file when a Lane-1 (ForMe) message arrives.
///
/// Create one instance per listener process and call `emit_if_lane_1` on
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
    pub fn emit(&self, unix_secs: u64) -> Result<(), ClerkError> {
        fs::write(&self.wake_file_path, format!("{unix_secs}\n"))?;
        Ok(())
    }

    /// Emit only if `lane` is `Lane::ForMe`. No-op for `Lane::Delivery`.
    pub fn emit_if_lane_1(&self, lane: &Lane, unix_secs: u64) -> Result<(), ClerkError> {
        if *lane == Lane::ForMe {
            self.emit(unix_secs)?;
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
        emitter.emit(1_000).unwrap();
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
        emitter.emit_if_lane_1(&Lane::Delivery, 1_000).unwrap();
        assert!(!path.exists(), "Delivery lane must not write wake file");
    }

    #[test]
    fn lane_for_me_does_emit() {
        use crate::lane::Lane;
        let dir = tempdir().unwrap();
        let path = dir.path().join("wake");
        let emitter = WakeEmitter::new(path.to_str().unwrap().to_string());
        emitter.emit_if_lane_1(&Lane::ForMe, 1_700_000_000).unwrap();
        assert!(path.exists(), "ForMe lane must write wake file");
    }
}
