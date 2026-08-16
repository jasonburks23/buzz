//! Configuration for the generic seat listener.
//!
//! Reads only env vars that are meaningful to any Buzz seat listener.
//! Fleet-specific vars (SEAT_ROLE, SEAT_CWD, CLAIM_DIR) are intentionally
//! absent; the fleet adapter reads them in its own config extension.

use nostr::{FromBech32, Keys, SecretKey};

use crate::error::ClerkError;

/// Configuration loaded from environment variables for the generic listener.
///
/// Does not contain fleet identity fields. Use `AgencyOsConfig` (in
/// buzz-seat-clerk-agencyos) for fleet-specific extension.
///
/// Deliberately does NOT derive `Debug` -- hand-written impl redacts the secret key.
pub struct ListenerConfig {
    /// The nostr signing keys for this seat.
    pub keys: Keys,
    /// Hex-encoded public key derived from `keys`.
    pub public_key_hex: String,
    /// WebSocket relay URL (e.g. "wss://relay.example.com").
    pub relay_url: String,
    /// Path to the wake file. When this file is touched, the listener wakes.
    pub wake_file: String,
    /// Path to the read-ack file. The listener writes read acknowledgements here.
    pub readack_file: String,
}

impl std::fmt::Debug for ListenerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerConfig")
            .field("public_key_hex", &self.public_key_hex)
            .field("relay_url", &self.relay_url)
            .field("wake_file", &self.wake_file)
            .field("readack_file", &self.readack_file)
            .field("keys", &"<REDACTED>")
            .finish()
    }
}

impl ListenerConfig {
    /// Load configuration from environment variables.
    ///
    /// Required vars: `SEAT_NSEC`, `RELAY_URL`.
    /// Optional vars: `WAKE_FILE` (default: `/tmp/buzz-seat-listener.wake`),
    ///                `READACK_FILE` (default: `/tmp/buzz-seat-listener.readack`).
    pub fn from_env() -> Result<Self, ClerkError> {
        let nsec_str =
            std::env::var("SEAT_NSEC").map_err(|_| ClerkError::MissingEnv("SEAT_NSEC".into()))?;
        let relay_url =
            std::env::var("RELAY_URL").map_err(|_| ClerkError::MissingEnv("RELAY_URL".into()))?;
        let wake_file =
            std::env::var("WAKE_FILE").unwrap_or_else(|_| "/tmp/buzz-seat-listener.wake".into());
        let readack_file = std::env::var("READACK_FILE")
            .unwrap_or_else(|_| "/tmp/buzz-seat-listener.readack".into());

        let secret_key =
            SecretKey::from_bech32(&nsec_str).map_err(|e| ClerkError::InvalidKey(e.to_string()))?;
        let keys = Keys::new(secret_key);
        let public_key_hex = keys.public_key().to_hex();

        Ok(Self {
            keys,
            public_key_hex,
            relay_url,
            wake_file,
            readack_file,
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
        std::env::remove_var("WAKE_FILE");
        std::env::remove_var("READACK_FILE");
        let cfg = ListenerConfig::from_env().unwrap();
        assert_eq!(cfg.relay_url, "ws://localhost:3000");
        // pubkey is derived; just check it is non-empty
        assert!(!cfg.public_key_hex.is_empty());
    }

    #[test]
    fn config_debug_redacts_nsec() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        let cfg = ListenerConfig::from_env().unwrap();
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
        assert!(ListenerConfig::from_env().is_err());
    }

    #[test]
    fn config_from_env_fails_missing_relay_url() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::remove_var("RELAY_URL");
        assert!(ListenerConfig::from_env().is_err());
    }

    #[test]
    fn config_without_fleet_vars_has_generic_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SEAT_NSEC", TEST_NSEC);
        std::env::set_var("RELAY_URL", "ws://localhost:3000");
        std::env::remove_var("WAKE_FILE");
        std::env::remove_var("READACK_FILE");

        let cfg = ListenerConfig::from_env().unwrap();
        assert_eq!(cfg.wake_file, "/tmp/buzz-seat-listener.wake");
        assert_eq!(cfg.readack_file, "/tmp/buzz-seat-listener.readack");

        // Debug must still redact keys, must NOT contain nsec literal.
        let debug = format!("{cfg:?}");
        assert!(
            !debug.contains("nsec1"),
            "nsec must not appear in Debug output"
        );
    }
}
