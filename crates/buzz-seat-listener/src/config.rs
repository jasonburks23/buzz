//! Clerk configuration loaded from environment variables.
//!
//! `SEAT_NSEC`: bech32 nsec of the seat identity key.
//! `RELAY_URL`: WebSocket URL of the Buzz relay (e.g. `ws://localhost:3000`).
//! `WAKE_FILE` (optional): path to write the wake signal. Defaults to `/tmp/buzz-seat-clerk.wake`.

use nostr::{FromBech32, Keys, SecretKey};

use crate::error::ClerkError;

/// Runtime configuration for one clerk instance.
///
/// Deliberately does NOT derive `Debug` -- hand-written impl redacts the secret key.
pub struct ClerkConfig {
    pub keys: Keys,
    pub public_key_hex: String,
    pub relay_url: String,
    pub wake_file: String,
    /// Fleet seat role (e.g. `"AgencyOS-CC-Alpha"`).  When `Some`, the honest
    /// read-receipt feature is active.  When `None`, the feature is disabled.
    pub seat_role: Option<String>,
    /// Optional seat working directory for claim-file disambiguation.
    pub seat_cwd: Option<String>,
    /// Path to the read-ack file written by the live session.
    pub readack_file: String,
    /// Directory containing `claude-seat-claim-*.json` fleet files.
    pub claim_dir: String,
}

impl std::fmt::Debug for ClerkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClerkConfig")
            .field("public_key_hex", &self.public_key_hex)
            .field("relay_url", &self.relay_url)
            .field("wake_file", &self.wake_file)
            .field("seat_role", &self.seat_role)
            .field("seat_cwd", &self.seat_cwd)
            .field("readack_file", &self.readack_file)
            .field("claim_dir", &self.claim_dir)
            .field("keys", &"<REDACTED>")
            .finish()
    }
}

impl ClerkConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self, ClerkError> {
        let nsec_str =
            std::env::var("SEAT_NSEC").map_err(|_| ClerkError::MissingEnv("SEAT_NSEC".into()))?;
        let relay_url =
            std::env::var("RELAY_URL").map_err(|_| ClerkError::MissingEnv("RELAY_URL".into()))?;
        let wake_file =
            std::env::var("WAKE_FILE").unwrap_or_else(|_| "/tmp/buzz-seat-clerk.wake".into());

        let secret_key =
            SecretKey::from_bech32(&nsec_str).map_err(|e| ClerkError::InvalidKey(e.to_string()))?;
        let keys = Keys::new(secret_key);
        let public_key_hex = keys.public_key().to_hex();

        let seat_role = std::env::var("SEAT_ROLE").ok();
        let seat_cwd = std::env::var("SEAT_CWD").ok();
        let readack_file =
            std::env::var("READACK_FILE").unwrap_or_else(|_| "/tmp/buzz-seat-clerk.readack".into());
        let claim_dir = std::env::var("CLAIM_DIR").unwrap_or_else(|_| "/tmp".into());

        Ok(Self {
            keys,
            public_key_hex,
            relay_url,
            wake_file,
            seat_role,
            seat_cwd,
            readack_file,
            claim_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize all env-var tests: `set_var`/`remove_var` mutate global state and
    // are not safe to call concurrently (Rust test harness runs tests in parallel
    // by default within a crate).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Canonical test nsec derived from the all-0x01 secp256k1 scalar.
    const TEST_NSEC: &str = "nsec1qyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqstywftw";

    #[test]
    fn config_from_env_parses_valid_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        let cfg = ClerkConfig::from_env().unwrap();
        assert_eq!(cfg.relay_url, "ws://localhost:3000");
        // pubkey is derived; just check it is non-empty
        assert!(!cfg.public_key_hex.is_empty());
    }

    #[test]
    fn config_debug_redacts_nsec() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        let cfg = ClerkConfig::from_env().unwrap();
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("nsec1"),
            "nsec must not appear in Debug output"
        );
        assert!(
            !debug.contains("SecretKey"),
            "SecretKey bytes must not appear"
        );
    }

    #[test]
    fn config_from_env_fails_missing_nsec() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SEAT_NSEC");
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        assert!(ClerkConfig::from_env().is_err());
    }

    #[test]
    fn config_from_env_fails_missing_relay_url() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::remove_var("RELAY_URL");
        assert!(ClerkConfig::from_env().is_err());
    }

    #[test]
    fn config_with_seat_role_set_populates_fields() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        std::env::set_var("SEAT_ROLE", "AgencyOS-CC-Alpha");
        std::env::set_var("SEAT_CWD", "/some/cwd");
        std::env::set_var("READACK_FILE", "/tmp/custom.readack");
        std::env::set_var("CLAIM_DIR", "/custom/claims");

        let cfg = ClerkConfig::from_env().unwrap();
        assert_eq!(cfg.seat_role.as_deref(), Some("AgencyOS-CC-Alpha"));
        assert_eq!(cfg.seat_cwd.as_deref(), Some("/some/cwd"));
        assert_eq!(cfg.readack_file, "/tmp/custom.readack");
        assert_eq!(cfg.claim_dir, "/custom/claims");

        // Debug must still redact keys, must NOT contain nsec literal.
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("nsec1"),
            "nsec must not appear in Debug output"
        );

        // Clean up.
        std::env::remove_var("SEAT_ROLE");
        std::env::remove_var("SEAT_CWD");
        std::env::remove_var("READACK_FILE");
        std::env::remove_var("CLAIM_DIR");
    }

    #[test]
    fn config_without_seat_role_has_none_and_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        std::env::remove_var("SEAT_ROLE");
        std::env::remove_var("SEAT_CWD");
        std::env::remove_var("READACK_FILE");
        std::env::remove_var("CLAIM_DIR");

        let cfg = ClerkConfig::from_env().unwrap();
        assert!(
            cfg.seat_role.is_none(),
            "seat_role must be None when SEAT_ROLE unset"
        );
        assert!(
            cfg.seat_cwd.is_none(),
            "seat_cwd must be None when SEAT_CWD unset"
        );
        assert_eq!(cfg.readack_file, "/tmp/buzz-seat-clerk.readack");
        assert_eq!(cfg.claim_dir, "/tmp");
    }
}
