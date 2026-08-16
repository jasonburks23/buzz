//! Minimal runnable example: generic Buzz seat listener.
//!
//! Shows how to use the public API of buzz-seat-listener with no fleet
//! assumptions. Uses EnvIdentity (local == live, gate always passes) so
//! any single-session user can run this without claim files or AgencyOS.
//!
//! # Required environment variables
//!
//! - `SEAT_NSEC`  -- bech32 nsec of the seat identity key (e.g. nsec1...)
//! - `RELAY_URL`  -- WebSocket URL of the Buzz relay (e.g. wss://relay.example.com)
//!
//! # Optional environment variables
//!
//! - `WAKE_FILE`    -- path to write the wake signal (default: /tmp/buzz-seat-listener.wake)
//! - `READACK_FILE` -- path to poll for read-ack JSON (default: /tmp/buzz-seat-listener.readack)
//!
//! # How to run
//!
//! ```sh
//! SEAT_NSEC=nsec1... RELAY_URL=wss://relay.example.com \
//!   cargo run --example seat-listener -p buzz-seat-listener
//! ```

use anyhow::Context;
use buzz_seat_listener::config::ListenerConfig;
use buzz_seat_listener::identity::EnvIdentity;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("buzz_seat_listener=info".parse().unwrap()),
        )
        .init();

    let cfg = ListenerConfig::from_env().context("failed to load ListenerConfig from env")?;
    info!(relay = %cfg.relay_url, pubkey = %cfg.public_key_hex, "seat-listener starting");

    let identity = EnvIdentity::new(None);

    buzz_seat_listener::runner::run(cfg, identity).await
}
