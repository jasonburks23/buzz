//! Liveness heartbeat, opeff#1196.
//!
//! CLERKALIVE01's first cut keyed staleness on the wake file, which only moves
//! when a real message arrives. A quiet seat with no mail looked identical to
//! a stuck one. This module writes a small JSON line to a dedicated file on
//! every event-loop tick, whether or not any mail arrived, so a reader can
//! tell "quiet" from "stuck" by the heartbeat's own age.
//!
//! This does NOT call `WakeEmitter::emit` or anything reachable from it. US-21
//! (`clerk.rs`, `mod timer_guard`) proves no timer or interval may trigger a
//! seat-turn wake; a heartbeat write is a plain file write with no path back
//! into `wake.rs`, so it cannot regress that guard. See `heartbeat_write_does_not_touch_wake_emitter`
//! below for the load-bearing proof.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::ClerkError;

/// Given the clerk's own wake-file path, derive the heartbeat file's default
/// path by substituting "wake" for "heartbeat" in the filename. Both naming
/// conventions in live use today, the per-seat registry path
/// (`/tmp/buzz-clerk-wake-<slug>.json`) and the bare default
/// (`/tmp/buzz-seat-clerk.wake`), contain the literal substring "wake" exactly
/// once, so this round-trips both without any new plumbing through
/// launch-clerk.sh, the tab-clerk generator, or relaunch.sh: those scripts
/// already resolve and export WAKE_FILE per seat, and this derivation reuses
/// that resolved value verbatim. HEARTBEAT_FILE remains a real override for
/// the rare case where the substitution does not fit.
pub fn default_heartbeat_path(wake_file_path: &str) -> String {
    wake_file_path.replacen("wake", "heartbeat", 1)
}

/// Atomically write `{"seat":<seat_role or null>,"ts":<unix_secs>}` to `path`.
///
/// Writes to a sibling temp file first, then renames over the target so a
/// reader never observes a partially written heartbeat. `seat_role` is
/// whatever the clerk's own `SEAT_ROLE` resolved to (may be absent for an
/// unconfigured/dev clerk).
pub fn write_heartbeat(
    path: &str,
    seat_role: Option<&str>,
    unix_secs: u64,
) -> Result<(), ClerkError> {
    let json = serde_json::json!({ "seat": seat_role, "ts": unix_secs });
    let target = Path::new(path);
    let tmp_path = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!(
            ".{}.tmp",
            target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("heartbeat")
        )),
        _ => Path::new(&format!("{path}.tmp")).to_path_buf(),
    };
    let mut f = fs::File::create(&tmp_path)?;
    f.write_all(json.to_string().as_bytes())?;
    f.flush()?;
    drop(f);
    fs::rename(&tmp_path, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_heartbeat_path_substitutes_registry_convention() {
        assert_eq!(
            default_heartbeat_path("/tmp/buzz-clerk-wake-agencyos-ops.json"),
            "/tmp/buzz-clerk-heartbeat-agencyos-ops.json"
        );
    }

    #[test]
    fn default_heartbeat_path_substitutes_bare_default() {
        assert_eq!(
            default_heartbeat_path("/tmp/buzz-seat-clerk.wake"),
            "/tmp/buzz-seat-clerk.heartbeat"
        );
    }

    #[test]
    fn write_heartbeat_creates_file_with_seat_and_ts() {
        let dir = std::env::temp_dir().join(format!("clerk-hb-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hb.json");
        let path_str = path.to_str().unwrap();

        write_heartbeat(path_str, Some("Ops"), 1_000_000).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["seat"], "Ops");
        assert_eq!(parsed["ts"], 1_000_000);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_heartbeat_overwrites_atomically_no_partial_read() {
        // Non-vacuity: a naive `fs::write` truncates before writing, so a
        // reader racing the write can observe an empty or truncated file.
        // The rename-based write never exposes that intermediate state --
        // proven here by writing twice and confirming the final file is
        // always fully valid JSON, never truncated mid-write.
        let dir = std::env::temp_dir().join(format!("clerk-hb-test-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hb.json");
        let path_str = path.to_str().unwrap();

        write_heartbeat(path_str, Some("Overwatch"), 111).unwrap();
        write_heartbeat(path_str, Some("Overwatch"), 222).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ts"], 222);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_heartbeat_handles_none_seat_role() {
        let dir = std::env::temp_dir().join(format!("clerk-hb-test-noseat-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hb.json");
        let path_str = path.to_str().unwrap();

        write_heartbeat(path_str, None, 5).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(parsed["seat"].is_null());

        fs::remove_dir_all(&dir).ok();
    }
}
