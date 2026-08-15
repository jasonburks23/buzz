//! Bounded-backoff reconnect loop around `NostrWsConnection`.

use std::time::Duration;

use buzz_ws_client::NostrWsConnection;
use nostr::Keys;
use tracing::{error, info, warn};

use crate::error::ClerkError;

/// Exponential-backoff state. Does not perform any I/O.
pub struct Backoff {
    initial_secs: u64,
    cap_secs: u64,
    factor: f64,
    current_secs: u64,
    attempt: u64,
}

impl Backoff {
    /// Create a new backoff. `factor` is the multiplier per attempt (e.g. 2.0).
    pub fn new(initial_secs: u64, cap_secs: u64, factor: f64) -> Self {
        Self {
            initial_secs,
            cap_secs,
            factor,
            current_secs: initial_secs,
            attempt: 0,
        }
    }

    /// Return next delay in seconds, then advance internal state.
    pub fn next_delay_secs(&mut self) -> u64 {
        let delay = self.current_secs.min(self.cap_secs);
        let next = (self.current_secs as f64 * self.factor).round() as u64;
        self.current_secs = next.min(self.cap_secs);
        self.attempt += 1;
        delay
    }

    /// How many times `next_delay_secs` has been called.
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// Reset to initial state (call after a successful connection).
    pub fn reset(&mut self) {
        self.current_secs = self.initial_secs;
        self.attempt = 0;
    }
}

/// Connect (with NIP-42 auth) and retry on failure with bounded backoff.
///
/// Returns `Ok(conn)` on the first successful authentication.
/// Returns `Err` only if `max_attempts` is `Some(n)` and all attempts are exhausted.
pub async fn connect_with_backoff(
    relay_url: &str,
    keys: &Keys,
    max_attempts: Option<u64>,
) -> Result<NostrWsConnection, ClerkError> {
    let mut backoff = Backoff::new(1, 60, 2.0);
    loop {
        match NostrWsConnection::connect_authenticated(relay_url, keys, None).await {
            Ok(conn) => {
                backoff.reset();
                info!(relay = %relay_url, "connected and authenticated");
                return Ok(conn);
            }
            Err(e) => {
                let attempt = backoff.attempt() + 1;
                if let Some(max) = max_attempts {
                    if attempt > max {
                        error!(relay = %relay_url, attempts = attempt, "max retries exhausted");
                        return Err(ClerkError::WebSocket(e));
                    }
                }
                let delay = backoff.next_delay_secs();
                warn!(
                    relay = %relay_url,
                    attempt = attempt,
                    delay_secs = delay,
                    error = %e,
                    "connection failed, retrying"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence_doubles_until_cap() {
        let mut b = Backoff::new(1, 60, 2.0);
        assert_eq!(b.next_delay_secs(), 1);
        assert_eq!(b.next_delay_secs(), 2);
        assert_eq!(b.next_delay_secs(), 4);
        assert_eq!(b.next_delay_secs(), 8);
        assert_eq!(b.next_delay_secs(), 16);
        assert_eq!(b.next_delay_secs(), 32);
        assert_eq!(b.next_delay_secs(), 60); // capped
        assert_eq!(b.next_delay_secs(), 60); // stays capped
    }

    #[test]
    fn backoff_resets_after_reset_call() {
        let mut b = Backoff::new(1, 60, 2.0);
        b.next_delay_secs();
        b.next_delay_secs();
        b.next_delay_secs();
        b.reset();
        assert_eq!(b.next_delay_secs(), 1);
    }

    #[test]
    fn backoff_attempt_count_increments() {
        let mut b = Backoff::new(1, 60, 2.0);
        assert_eq!(b.attempt(), 0);
        b.next_delay_secs();
        assert_eq!(b.attempt(), 1);
        b.next_delay_secs();
        assert_eq!(b.attempt(), 2);
    }
}
