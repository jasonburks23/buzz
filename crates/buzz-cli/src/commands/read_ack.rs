//! `buzz read-ack` — write the read-watermark handoff file consumed by the clerk.
//!
//! The seat runs this after reading its mail. The clerk polls the file and
//! advances its you-you read bookmark for each channel listed.
//!
//! Behavior: atomic MERGE. If the target file already exists and parses as a
//! v1 MultiChannelAck, load it, take max(existing_ts, new_ts) per channel,
//! keep/overwrite the marker, then write atomically via write_multi_channel_ack.
//! If the file is absent or unparseable, create a fresh ack.
//!
//! Target file resolution (first wins):
//!   1. `--file <path>`
//!   2. `READACK_FILE` environment variable
//!   3. Hard error — neither supplied.
//!
//! Marker resolution (first wins):
//!   1. `--marker <session_id>`
//!   2. `SEAT_SESSION` environment variable
//!   3. `CLERK_SESSION_ID` environment variable
//!   4. Literal string `"default"` (documented fallback).

use std::collections::HashMap;

use buzz_seat_listener::read_ack::{
    parse_multi_channel_ack, write_multi_channel_ack, MultiChannelAck,
};

use crate::error::CliError;

/// Env var name for the target file path.
const READACK_FILE_ENV: &str = "READACK_FILE";
/// Env var names tried in order for the session marker.
const SEAT_SESSION_ENV: &str = "SEAT_SESSION";
const CLERK_SESSION_ID_ENV: &str = "CLERK_SESSION_ID";
/// Documented fallback marker when no env var or flag is set.
const DEFAULT_MARKER: &str = "default";

/// Resolve the target file path from flag or env var.
fn resolve_file(flag: Option<&str>) -> Result<String, CliError> {
    if let Some(path) = flag {
        return Ok(path.to_string());
    }
    std::env::var(READACK_FILE_ENV).map_err(|_| {
        CliError::Usage(
            "target file is required: use --file <path> or set READACK_FILE".to_string(),
        )
    })
}

/// Resolve the session marker from flag or env vars.
fn resolve_marker(flag: Option<&str>) -> String {
    if let Some(m) = flag {
        return m.to_string();
    }
    if let Ok(v) = std::env::var(SEAT_SESSION_ENV) {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var(CLERK_SESSION_ID_ENV) {
        if !v.is_empty() {
            return v;
        }
    }
    DEFAULT_MARKER.to_string()
}

/// Merge new channel timestamps into an existing ack, taking max per channel.
///
/// If the file at `path` exists and parses as v1, load it; otherwise start
/// fresh. Then merge `new_channels` taking max(existing_ts, new_ts) per
/// channel, set `marker`, and write atomically.
pub fn merge_and_write(
    path: &str,
    new_channels: &HashMap<String, u64>,
    marker: &str,
) -> Result<(), CliError> {
    // Try to load and parse an existing ack from disk.
    let mut channels: HashMap<String, u64> = match std::fs::read_to_string(path) {
        Ok(raw) => match parse_multi_channel_ack(&raw) {
            Some(existing) => existing.channels,
            None => HashMap::new(),
        },
        Err(_) => HashMap::new(),
    };

    // Merge: take max(existing_ts, new_ts) per channel.
    for (channel, &ts) in new_channels {
        let entry = channels.entry(channel.clone()).or_insert(0);
        if ts > *entry {
            *entry = ts;
        }
    }

    let ack = MultiChannelAck {
        v: 1,
        channels,
        marker: marker.to_string(),
    };
    write_multi_channel_ack(path, &ack)
        .map_err(|e| CliError::Other(format!("failed to write read-ack: {e}")))
}

/// Run `buzz read-ack`.
///
/// `channel_up_to_pairs`: alternating (channel_uuid, unix_secs) pairs built
/// from the repeatable `--channel` / `--up-to` flags by the caller.
pub fn cmd_read_ack(
    channel_up_to_pairs: Vec<(String, u64)>,
    file: Option<&str>,
    marker: Option<&str>,
) -> Result<(), CliError> {
    if channel_up_to_pairs.is_empty() {
        return Err(CliError::Usage(
            "at least one --channel / --up-to pair is required".to_string(),
        ));
    }

    let path = resolve_file(file)?;
    let resolved_marker = resolve_marker(marker);

    let new_channels: HashMap<String, u64> = channel_up_to_pairs.into_iter().collect();

    merge_and_write(&path, &new_channels, &resolved_marker)?;

    println!(
        "{{\"ok\":true,\"file\":{},\"channels\":{}}}",
        serde_json::to_string(&path).unwrap_or_default(),
        new_channels.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Helper: build a channel map from pairs.
    fn ch(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    // ---- merge_and_write: new file ----

    #[test]
    fn merge_creates_new_file_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readack.json").display().to_string();

        let channels = ch(&[("chan-a", 100), ("chan-b", 200)]);
        merge_and_write(&path, &channels, "sess-1").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_multi_channel_ack(&raw).expect("must parse");
        assert_eq!(parsed.v, 1);
        assert_eq!(parsed.marker, "sess-1");
        assert_eq!(parsed.channels["chan-a"], 100);
        assert_eq!(parsed.channels["chan-b"], 200);
    }

    // ---- merge_and_write: second call updates via max ----

    #[test]
    fn merge_second_call_adds_channel_and_bumps_ts_via_max() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readack.json").display().to_string();

        // First write: chan-a=100
        merge_and_write(&path, &ch(&[("chan-a", 100)]), "sess-1").unwrap();

        // Second write: chan-a is bumped (500 > 100), chan-b is added.
        merge_and_write(&path, &ch(&[("chan-a", 500), ("chan-b", 300)]), "sess-2").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_multi_channel_ack(&raw).expect("must parse");
        assert_eq!(parsed.channels["chan-a"], 500, "max(100,500)=500");
        assert_eq!(parsed.channels["chan-b"], 300, "chan-b newly added");
        assert_eq!(parsed.marker, "sess-2", "marker updated to latest call");
    }

    // ---- merge_and_write: max wins when existing ts is higher ----

    #[test]
    fn merge_keeps_existing_ts_when_existing_is_higher() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readack.json").display().to_string();

        merge_and_write(&path, &ch(&[("chan-a", 999)]), "sess-1").unwrap();

        // Second write with a LOWER ts — existing should win.
        merge_and_write(&path, &ch(&[("chan-a", 1)]), "sess-2").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_multi_channel_ack(&raw).expect("must parse");
        assert_eq!(parsed.channels["chan-a"], 999, "max(999,1)=999");
    }

    // ---- merge_and_write: unparseable file treated as fresh ----

    #[test]
    fn merge_overwrites_unparseable_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("readack.json").display().to_string();

        std::fs::write(&path, b"not json {{{").unwrap();
        merge_and_write(&path, &ch(&[("chan-a", 42)]), "sess-fresh").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_multi_channel_ack(&raw).expect("must parse");
        assert_eq!(parsed.channels["chan-a"], 42);
        assert_eq!(parsed.marker, "sess-fresh");
    }

    // ---- resolve_file ----

    #[test]
    fn resolve_file_uses_flag_when_provided() {
        let result = resolve_file(Some("/tmp/my-ack.json")).unwrap();
        assert_eq!(result, "/tmp/my-ack.json");
    }

    #[test]
    fn resolve_file_errors_when_neither_flag_nor_env() {
        // Temporarily unset the env var if it happens to be set.
        let _guard = std::env::remove_var(READACK_FILE_ENV);
        let result = resolve_file(None);
        assert!(result.is_err(), "must error when no flag and no env var");
    }

    // ---- resolve_marker ----

    #[test]
    fn resolve_marker_uses_flag_first() {
        std::env::set_var(SEAT_SESSION_ENV, "env-sess");
        let m = resolve_marker(Some("flag-sess"));
        std::env::remove_var(SEAT_SESSION_ENV);
        assert_eq!(m, "flag-sess");
    }

    #[test]
    fn resolve_marker_falls_back_to_default_when_nothing_set() {
        std::env::remove_var(SEAT_SESSION_ENV);
        std::env::remove_var(CLERK_SESSION_ID_ENV);
        let m = resolve_marker(None);
        assert_eq!(m, DEFAULT_MARKER);
    }

    // ---- cmd_read_ack: no pairs errors ----

    #[test]
    fn cmd_read_ack_errors_when_no_pairs() {
        let result = cmd_read_ack(vec![], Some("/tmp/x.json"), Some("sess"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("pair"), "error message should mention pairs");
    }

    // ---- cmd_read_ack: no file errors ----

    #[test]
    fn cmd_read_ack_errors_when_no_file_and_no_env() {
        std::env::remove_var(READACK_FILE_ENV);
        let result = cmd_read_ack(vec![("chan-a".to_string(), 100)], None, Some("sess"));
        assert!(result.is_err());
    }
}
