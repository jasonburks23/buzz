//! Fleet-specific configuration extension.
//!
//! Reads env vars that carry AgencyOS fleet semantics: SEAT_ROLE, SEAT_CWD,
//! CLAIM_DIR, CLERK_SESSION_ID. These are intentionally absent from
//! `buzz-seat-listener`'s `ListenerConfig`.

use std::path::PathBuf;

/// Fleet-specific configuration loaded from environment variables.
///
/// Load this alongside `ListenerConfig` in the fleet binary, then use it
/// to construct a `ClaimFileIdentity`.
#[derive(Debug, Clone)]
pub struct AgencyOsConfig {
    /// The role string for this seat (e.g. "agencyos-cc"). Optional.
    pub seat_role: Option<String>,
    /// The working directory for this seat. Optional.
    pub seat_cwd: Option<String>,
    /// Directory where claim JSON files are written. Defaults to `/tmp`.
    pub claim_dir: PathBuf,
    /// Session ID of the currently running clerk instance.
    /// Set from CLERK_SESSION_ID env var or generated at startup.
    pub session_id: String,
}

impl AgencyOsConfig {
    /// Load AgencyOS fleet config from environment variables.
    ///
    /// Optional vars: `SEAT_ROLE`, `SEAT_CWD`,
    ///                `CLAIM_DIR` (default: `/tmp`),
    ///                `CLERK_SESSION_ID` (default: simple timestamp-based id).
    pub fn from_env() -> Self {
        let seat_role = std::env::var("SEAT_ROLE").ok();
        let seat_cwd = std::env::var("SEAT_CWD").ok();
        let claim_dir = std::env::var("CLAIM_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        let session_id =
            std::env::var("CLERK_SESSION_ID").unwrap_or_else(|_| generate_session_id());

        Self {
            seat_role,
            seat_cwd,
            claim_dir,
            session_id,
        }
    }
}

/// Generate a simple session id without adding a UUID crate dependency.
/// Uses PID + subsecond nanos. Unique enough for a single-process clerk.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(42);
    let pid = std::process::id();
    format!("clerk-{pid}-{nanos:08x}")
}
