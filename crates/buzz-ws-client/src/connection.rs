use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, Keys, Tag};
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::debug;

use crate::error::WsClientError;
use crate::message::{build_auth_event, parse_relay_message, OkResponse, RelayMessage};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Seconds to wait for the relay to send the NIP-42 AUTH challenge after connecting.
pub const AUTH_CHALLENGE_TIMEOUT_SECS: u64 = 20;

/// Seconds to wait for the relay's OK response to the AUTH event.
pub const AUTH_OK_TIMEOUT_SECS: u64 = 20;

/// Seconds to wait for the relay's OK response to a published event.
pub const PUBLISH_OK_TIMEOUT_SECS: u64 = 30;

/// A NIP-42-capable WebSocket connection to a Nostr relay.
pub struct NostrWsConnection {
    ws: WsStream,
    buffer: VecDeque<RelayMessage>,
    pending_challenge: Option<String>,
    relay_url: String,
}

impl NostrWsConnection {
    /// Connects to the relay at `url` and performs NIP-42 authentication with `keys`.
    ///
    /// Pass `auth_tag` to include a NIP-OA authorization tag in the AUTH event.
    pub async fn connect_authenticated(
        url: &str,
        keys: &Keys,
        auth_tag: Option<&Tag>,
    ) -> Result<Self, WsClientError> {
        let mut conn = Self::connect(url).await?;
        conn.authenticate(keys, auth_tag).await?;
        Ok(conn)
    }

    /// Connects to the relay at `url` without performing authentication.
    pub async fn connect(url: &str) -> Result<Self, WsClientError> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|e| WsClientError::Url(e.to_string()))?;

        let (ws, _response) = connect_async(parsed.as_str())
            .await
            .map_err(WsClientError::WebSocket)?;

        debug!("connected to relay at {url}");

        Ok(Self {
            ws,
            buffer: VecDeque::new(),
            pending_challenge: None,
            relay_url: url.to_string(),
        })
    }

    /// Performs NIP-42 authentication using `keys` against the connected relay.
    ///
    /// Pass `auth_tag` to include a NIP-OA authorization tag in the AUTH event.
    pub async fn authenticate(
        &mut self,
        keys: &Keys,
        auth_tag: Option<&Tag>,
    ) -> Result<(), WsClientError> {
        let challenge = self
            .wait_for_auth_challenge(Duration::from_secs(AUTH_CHALLENGE_TIMEOUT_SECS))
            .await?;

        let auth_event = build_auth_event(&challenge, &self.relay_url, keys, auth_tag)?;
        let event_id = auth_event.id.to_hex();

        self.send_raw(&json!(["AUTH", auth_event])).await?;

        let ok = self
            .wait_for_ok(&event_id, Duration::from_secs(AUTH_OK_TIMEOUT_SECS))
            .await?;
        if !ok.accepted {
            return Err(WsClientError::AuthFailed(ok.message));
        }

        debug!("NIP-42 authentication successful");
        Ok(())
    }

    /// Sends a signed event to the relay and waits for the OK response.
    pub async fn send_event(&mut self, event: Event) -> Result<OkResponse, WsClientError> {
        let event_id = event.id.to_hex();
        self.send_raw(&json!(["EVENT", event])).await?;
        self.wait_for_ok(&event_id, Duration::from_secs(PUBLISH_OK_TIMEOUT_SECS))
            .await
    }

    /// Receives the next relay message, waiting up to `timeout_dur`.
    pub async fn next_event(
        &mut self,
        timeout_dur: Duration,
    ) -> Result<RelayMessage, WsClientError> {
        if let Some(msg) = self.buffer.pop_front() {
            return Ok(msg);
        }
        self.recv_one(timeout_dur).await
    }

    /// Closes the WebSocket connection gracefully.
    pub async fn disconnect(mut self) -> Result<(), WsClientError> {
        self.ws.close(None).await?;
        Ok(())
    }

    /// Sends a raw JSON value as a WebSocket text frame.
    pub async fn send_raw(&mut self, value: &Value) -> Result<(), WsClientError> {
        let text = serde_json::to_string(value)?;
        debug!("→ relay: {text}");
        self.ws.send(Message::Text(text.into())).await?;
        Ok(())
    }

    async fn recv_one(&mut self, timeout_dur: Duration) -> Result<RelayMessage, WsClientError> {
        if let Some(msg) = self.buffer.pop_front() {
            return Ok(msg);
        }

        let msg = recv_with_deadline(&mut self.ws, timeout_dur).await?;
        if let RelayMessage::Auth { ref challenge } = msg {
            self.pending_challenge = Some(challenge.clone());
        }
        Ok(msg)
    }

    async fn wait_for_auth_challenge(
        &mut self,
        timeout_dur: Duration,
    ) -> Result<String, WsClientError> {
        if let Some(challenge) = self.pending_challenge.take() {
            return Ok(challenge);
        }

        if let Some(idx) = self
            .buffer
            .iter()
            .position(|m| matches!(m, RelayMessage::Auth { .. }))
        {
            match self.buffer.remove(idx).unwrap() {
                RelayMessage::Auth { challenge } => return Ok(challenge),
                _ => unreachable!(),
            }
        }

        let deadline = tokio::time::Instant::now() + timeout_dur;

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            if remaining.is_zero() {
                return Err(WsClientError::NoAuthChallenge);
            }

            let raw = timeout(remaining, self.ws.next())
                .await
                .map_err(|_| WsClientError::NoAuthChallenge)?
                .ok_or(WsClientError::ConnectionClosed)?
                .map_err(WsClientError::WebSocket)?;

            match raw {
                Message::Text(text) => {
                    let msg = parse_relay_message(&text)?;
                    match msg {
                        RelayMessage::Auth { challenge } => {
                            if challenge.len() > 1024 {
                                return Err(WsClientError::AuthFailed(
                                    "challenge exceeds 1024 bytes".into(),
                                ));
                            }
                            return Ok(challenge);
                        }
                        other => self.buffer.push_back(other),
                    }
                }
                Message::Ping(data) => {
                    self.ws.send(Message::Pong(data)).await?;
                }
                Message::Close(_) => return Err(WsClientError::ConnectionClosed),
                _ => {}
            }
        }
    }

    async fn wait_for_ok(
        &mut self,
        event_id: &str,
        timeout_dur: Duration,
    ) -> Result<OkResponse, WsClientError> {
        let deadline = tokio::time::Instant::now() + timeout_dur;

        if let Some(idx) = self
            .buffer
            .iter()
            .position(|m| matches!(m, RelayMessage::Ok(ok) if ok.event_id == event_id))
        {
            match self.buffer.remove(idx).unwrap() {
                RelayMessage::Ok(ok) => return Ok(ok),
                _ => unreachable!(),
            }
        }

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            if remaining.is_zero() {
                return Err(WsClientError::Timeout);
            }

            let raw = timeout(remaining, self.ws.next())
                .await
                .map_err(|_| WsClientError::Timeout)?
                .ok_or(WsClientError::ConnectionClosed)?
                .map_err(WsClientError::WebSocket)?;

            match raw {
                Message::Text(text) => {
                    let msg = parse_relay_message(&text)?;
                    match msg {
                        RelayMessage::Ok(ok) if ok.event_id == event_id => return Ok(ok),
                        RelayMessage::Auth { ref challenge } => {
                            self.pending_challenge = Some(challenge.clone());
                            self.buffer.push_back(msg);
                        }
                        other => self.buffer.push_back(other),
                    }
                }
                Message::Ping(data) => {
                    self.ws.send(Message::Pong(data)).await?;
                }
                Message::Close(_) => return Err(WsClientError::ConnectionClosed),
                _ => {}
            }
        }
    }
}

/// Waits up to `timeout_dur`, total, for the next Text frame on `ws`, replying
/// to any Ping with a Pong along the way.
///
/// opeff#1196: the deadline is computed once, before the loop, and every pass
/// waits only for what is left of it. A non-Text frame (Ping, Pong, Binary,
/// ...) is handled and the loop continues, but it never grants the wait a
/// fresh `timeout_dur`; only silence advances the clock toward `Timeout`. A
/// per-frame `timeout(timeout_dur, ws.next())` inside this loop looks
/// equivalent but is not: a relay (or a test double) sending a control frame
/// more often than `timeout_dur` never lets that form return, Text or
/// Timeout, at all.
async fn recv_with_deadline<S>(
    ws: &mut S,
    timeout_dur: Duration,
) -> Result<RelayMessage, WsClientError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout_dur;

    loop {
        let raw = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| WsClientError::Timeout)?
            .ok_or(WsClientError::ConnectionClosed)?
            .map_err(WsClientError::WebSocket)?;

        match raw {
            Message::Text(text) => return Ok(parse_relay_message(&text)?),
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await?;
            }
            Message::Close(_) => return Err(WsClientError::ConnectionClosed),
            _ => {}
        }
    }
}

/// One-shot helper: connect, authenticate, send one event, disconnect.
///
/// Establishes a fresh WebSocket connection, completes NIP-42 authentication,
/// publishes `event`, waits for the relay's OK response, then closes the
/// connection. The entire operation is bounded by `timeout_secs`.
pub async fn publish_event(
    relay_url: &str,
    event: Event,
    keys: &Keys,
    auth_tag: Option<&Tag>,
    timeout_secs: u64,
) -> Result<OkResponse, WsClientError> {
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let mut conn = NostrWsConnection::connect(relay_url).await?;
        conn.authenticate(keys, auth_tag).await?;
        let ok = conn.send_event(event).await?;
        let _ = conn.disconnect().await;
        Ok::<_, WsClientError>(ok)
    })
    .await
    .map_err(|_| WsClientError::Timeout)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[test]
    fn auth_challenge_timeout_meets_floor() {
        const { assert!(AUTH_CHALLENGE_TIMEOUT_SECS >= 20) };
    }

    #[test]
    fn auth_ok_timeout_meets_floor() {
        const { assert!(AUTH_OK_TIMEOUT_SECS >= 20) };
    }

    #[test]
    fn publish_ok_timeout_meets_floor() {
        const { assert!(PUBLISH_OK_TIMEOUT_SECS >= 30) };
    }

    /// A fake relay transport that never sends Text, only a Ping every
    /// `period`, forever. Stands in for `WsStream` in `recv_with_deadline` so
    /// the deadline behavior can be tested without a real socket.
    struct PingOnlyStream {
        ticker: tokio::time::Interval,
    }

    impl PingOnlyStream {
        fn new(period: Duration) -> Self {
            Self {
                ticker: tokio::time::interval(period),
            }
        }
    }

    impl futures_util::Stream for PingOnlyStream {
        type Item = Result<Message, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.ticker
                .poll_tick(cx)
                .map(|_| Some(Ok(Message::Ping(Vec::new().into()))))
        }
    }

    impl futures_util::Sink<Message> for PingOnlyStream {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            // The Pong reply is dropped on the floor; this double only cares
            // about what recv_with_deadline does with the read side.
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    // opeff#1196: with paused virtual time, this proves the deadline shape
    // itself, not just that a Ping gets a Pong. A stream emitting a Ping
    // every 100 ms and never a Text frame must still return Timeout once
    // timeout_dur has elapsed, plus a small slack for the tick and Pong
    // bookkeeping in between. Mutation note: reverting recv_with_deadline to
    // the old per-frame form, `timeout(timeout_dur, ws.next())` called fresh
    // inside the loop instead of `timeout_at(deadline, ...)` against a
    // deadline computed once, makes this test hang: every 100 ms Ping arrives
    // before the 350 ms timeout_dur can ever elapse, so the wait resets
    // forever and neither Timeout nor any other result is ever produced.
    #[tokio::test(start_paused = true)]
    async fn ping_only_stream_times_out_without_resetting_deadline() {
        let mut stream = PingOnlyStream::new(Duration::from_millis(100));
        let timeout_dur = Duration::from_millis(350);
        let slack = Duration::from_millis(150);

        let start = tokio::time::Instant::now();
        let result = recv_with_deadline(&mut stream, timeout_dur).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(WsClientError::Timeout)),
            "expected Timeout, got {result:?}"
        );
        assert!(
            elapsed >= timeout_dur && elapsed <= timeout_dur + slack,
            "elapsed {elapsed:?} should land within {slack:?} of timeout_dur {timeout_dur:?}"
        );
    }
}
