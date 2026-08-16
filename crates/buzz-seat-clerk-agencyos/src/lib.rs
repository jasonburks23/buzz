//! AgencyOS fleet adapter for the buzz-seat-listener generic library.
//!
//! Provides [`claim_identity::ClaimFileIdentity`]: a [`SeatIdentity`] impl
//! that resolves the live session from `/tmp/claude-seat-claim-*.json` files,
//! implementing the honest-seen gate for multi-session fleet seats.
//!
//! Also provides [`config_ext::AgencyOsConfig`] for reading fleet-specific
//! env vars (SEAT_ROLE, SEAT_CWD, CLAIM_DIR, CLERK_SESSION_ID).

pub mod claim_identity;
pub mod config_ext;
