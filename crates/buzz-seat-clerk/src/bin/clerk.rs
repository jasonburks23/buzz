//! Clerk binary entry point.
use anyhow::Result;
use buzz_seat_clerk::config::ClerkConfig;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = ClerkConfig::from_env()?;
    info!(pubkey = %cfg.public_key_hex, relay = %cfg.relay_url, "clerk starting");
    Ok(())
}
