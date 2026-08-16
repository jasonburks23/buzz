#![deny(unsafe_code)]
//! # buzz-seat-listener
//!
//! Headless, always-on, generic Buzz seat listener library.
//!
//! ## What it does
//!
//! Connects to a Buzz relay, subscribes to all rooms the seat is a member of,
//! delivers every message to a local in-memory mailbox, writes a kind:30078
//! read-state bookmark to the relay (NIP-44 encrypted to self), and emits a
//! wake signal (file write) when a Lane-1 message arrives.
//!
//! ## What it does NOT do
//!
//! It never answers. It never injects keystrokes. It is a delivery listener, not
//! a brain. It has no knowledge of fleet identity, claim files, or AgencyOS.
//! Fleet-specific wiring lives in `buzz-seat-clerk-agencyos`.
//!
//! ## Attention lanes
//!
//! Lane 1 (ForMe): DM channel OR @mention (p-tag == seat pubkey). Triggers wake signal.
//! Lane 2/3 (Delivery): all other messages. Delivered and badged; no wake.
//!
//! ## Divergences from upstream Buzz patterns
//!
//! - Uses `buzz-ws-client::NostrWsConnection` (NIP-42 client) rather than countdown-bot's
//!   raw `tokio-tungstenite`. Rationale: dogfoods the client crate; keeps this crate small.
//! - Adds a bounded-backoff reconnect loop (absent from all upstream examples).
//! - Adds a durable on-disk slot/client_id identity (replaces desktop's localStorage).
//! - Adds a 3-lane attention policy above dumb delivery (absent from buzz-acp which is binary).
//!
//! ## Environment variables
//!
//! `SEAT_NSEC` (required): bech32 nsec of the seat identity key.
//! `RELAY_URL` (required): WebSocket URL of the Buzz relay.
//! `WAKE_FILE` (optional): path for the wake signal file. Default: `/tmp/buzz-seat-listener.wake`.
//! `READACK_FILE` (optional): path for the read-ack JSON. Default: `/tmp/buzz-seat-listener.readack`.

pub mod badge;
pub mod config;
pub mod connection;
pub mod discovery;
pub mod error;
pub mod identity;
pub mod lane;
pub mod mailbox;
pub mod read_ack;
pub mod read_state;
pub mod runner;
pub mod session_identity;
pub mod subscription;
pub mod wake;
