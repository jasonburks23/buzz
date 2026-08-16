//! Clerk error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClerkError {
    #[error("missing environment variable: {0}")]
    MissingEnv(String),

    #[error("invalid nsec key: {0}")]
    InvalidKey(String),

    #[error("WebSocket error: {0}")]
    WebSocket(#[from] buzz_ws_client::WsClientError),

    #[error("HTTP discovery error: {0}")]
    Discovery(String),

    #[error("read-state write error: {0}")]
    ReadStateWrite(String),

    #[error("NIP-44 encrypt error: {0}")]
    Nip44(String),

    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
