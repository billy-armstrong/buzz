//! A tiny non-AI Buzz bot.
//!
//! The bot listens to one channel and replies to messages that contain commands:
//! - `!countdown 5` → `5 4 3 2 1 🚀`
//! - `!fib 8` → `13 8 5 3 2 1 1 0`
//! - `@Countdown Bot fib 8` → `13 8 5 3 2 1 1 0`
//!
//! It supports two relay-auth paths:
//! - `standalone`: authenticate as the bot key directly. This key must be an
//!   explicit relay member / allowlisted identity on closed relays.
//! - `owner-attested`: authenticate as the bot key with a NIP-OA `auth` tag
//!   signed by the owner/agent key. On relays that allow NIP-OA membership,
//!   the bot can connect because its owner is already a relay member.

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Alphabet, Event, EventBuilder, Filter, Keys, Kind, SingleLetterTag, Tag};
use serde_json::json;

const DEFAULT_RELAY_URL: &str = "ws://localhost:3000";
const SUBSCRIPTION_ID: &str = "countdown-bot";
const BOT_NAME: &str = "countdown-bot";
const BOT_DISPLAY_NAME: &str = "Countdown Bot";
const BOT_ABOUT: &str =
    "A tiny non-AI Buzz reference bot that replies to !countdown and countdown-style !fib.";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(8);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(60);
const REPLAY_WINDOW: usize = 4_096;
const REPLAY_OVERLAP_SECONDS: u64 = 60;
const BOT_ICON_DATA_URL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 128 128'%3E%3Crect width='128' height='128' rx='28' fill='%23131622'/%3E%3Ccircle cx='64' cy='64' r='42' fill='none' stroke='%237dd3fc' stroke-width='10'/%3E%3Cpath d='M64 32v32l22 14' fill='none' stroke='%23facc15' stroke-width='10' stroke-linecap='round' stroke-linejoin='round'/%3E%3Cpath d='M42 96h44' stroke='%23a78bfa' stroke-width='8' stroke-linecap='round'/%3E%3C/svg%3E";

#[tokio::main]
async fn main() -> Result<()> {
    install_crypto_provider();
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;

    tracing::info!(
        bot_pubkey = %config.bot_keys.public_key().to_hex(),
        "countdown bot identity"
    );
    tracing::info!(relay_url = %config.relay_url, "connecting to relay");

    tracing::info!(
        channel_id = %config.channel_id,
        "listening for !countdown, !fib, and @mention commands"
    );

    run_bot(&config).await
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn is_terminal_membership_closed(subscription_id: &str, message: &str) -> bool {
    subscription_id == SUBSCRIPTION_ID
        && matches!(
            message,
            "restricted: not a channel member" | "restricted: channel access revoked"
        )
}

#[derive(Debug)]
struct SessionFailure {
    terminal: bool,
    error: anyhow::Error,
}

struct ReplayGuard {
    capacity: usize,
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl ReplayGuard {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            ids: HashSet::with_capacity(capacity),
        }
    }

    fn contains(&self, event_id: &str) -> bool {
        self.ids.contains(event_id)
    }

    fn record(&mut self, event_id: String) {
        if self.capacity == 0 || !self.ids.insert(event_id.clone()) {
            return;
        }
        self.order.push_back(event_id);
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
    }
}

impl SessionFailure {
    fn retry(error: impl Into<anyhow::Error>) -> Self {
        Self {
            terminal: false,
            error: error.into(),
        }
    }

    fn terminal(error: impl Into<anyhow::Error>) -> Self {
        Self {
            terminal: true,
            error: error.into(),
        }
    }
}

struct BotState {
    started_at: nostr::Timestamp,
    subscription_since: nostr::Timestamp,
    seen_event_ids: ReplayGuard,
    pending_reply: Option<PendingReply>,
    profile_published: bool,
    membership_announced: bool,
}

struct PendingReply {
    input_event_id: String,
    event: Event,
}

impl BotState {
    fn new() -> Self {
        let started_at = nostr::Timestamp::now();
        Self {
            started_at,
            subscription_since: started_at,
            seen_event_ids: ReplayGuard::new(REPLAY_WINDOW),
            pending_reply: None,
            profile_published: false,
            membership_announced: false,
        }
    }

    fn observe_subscription_event(
        &mut self,
        caught_up: bool,
        created_at: nostr::Timestamp,
        observed_at: nostr::Timestamp,
    ) {
        if caught_up {
            self.advance_cursor(created_at.min(observed_at));
        }
    }

    fn finish_catchup(&mut self, session_started_at: nostr::Timestamp) {
        self.advance_cursor(session_started_at);
    }

    fn advance_cursor(&mut self, timestamp: nostr::Timestamp) {
        let with_overlap =
            nostr::Timestamp::from(timestamp.as_secs().saturating_sub(REPLAY_OVERLAP_SECONDS));
        self.subscription_since = self.subscription_since.max(with_overlap);
    }

    fn subscription_since(&self) -> nostr::Timestamp {
        self.subscription_since
    }
}

async fn run_bot(config: &Config) -> Result<()> {
    let mut state = BotState::new();
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;

    loop {
        let result = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(());
            }
            result = run_session(config, &mut state) => result,
        };

        if let Err(failure) = result {
            if failure.terminal {
                return Err(failure.error);
            }
            tracing::warn!(
                error = %failure.error,
                reconnect_delay_seconds = reconnect_delay.as_secs(),
                "relay session failed"
            );
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(());
            }
            _ = tokio::time::sleep(reconnect_delay) => {}
        }
        reconnect_delay = reconnect_delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
    }
}

struct Config {
    relay_url: String,
    channel_id: String,
    bot_keys: Keys,
    owner_auth_tag: Option<Tag>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let relay_url =
            std::env::var("BUZZ_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());
        validate_relay_url(&relay_url)?;
        let channel_id = required_env("BUZZ_CHANNEL_ID")?;
        let bot_keys = Keys::parse(&required_env("BUZZ_BOT_PRIVATE_KEY")?)
            .context("BUZZ_BOT_PRIVATE_KEY must be an nsec or hex private key")?;

        let auth_mode =
            std::env::var("BUZZ_BOT_AUTH_MODE").unwrap_or_else(|_| "standalone".to_string());
        let owner_auth_tag = match auth_mode.as_str() {
            "standalone" => None,
            "owner-attested" => {
                let tag_json = match std::env::var("BUZZ_AUTH_TAG") {
                    Ok(value) if !value.trim().is_empty() => value,
                    _ => {
                        let owner_keys = Keys::parse(&required_env("BUZZ_OWNER_PRIVATE_KEY")?)
                            .context("BUZZ_OWNER_PRIVATE_KEY must be an nsec or hex private key")?;
                        buzz_sdk::nip_oa::compute_auth_tag(&owner_keys, &bot_keys.public_key(), "")?
                    }
                };

                let owner = buzz_sdk::nip_oa::verify_auth_tag(&tag_json, &bot_keys.public_key())
                    .context("BUZZ_AUTH_TAG is not valid for BUZZ_BOT_PRIVATE_KEY")?;
                tracing::info!(owner_pubkey = %owner.to_hex(), "owner-attested auth tag verified");
                Some(buzz_sdk::nip_oa::parse_auth_tag(&tag_json)?)
            }
            other => {
                bail!("BUZZ_BOT_AUTH_MODE must be 'standalone' or 'owner-attested', got {other:?}")
            }
        };

        Ok(Self {
            relay_url,
            channel_id,
            bot_keys,
            owner_auth_tag,
        })
    }
}

async fn run_session(config: &Config, state: &mut BotState) -> Result<(), SessionFailure> {
    let session_started_at = nostr::Timestamp::now();
    let mut caught_up = false;
    let mut connection = NostrWsConnection::connect_authenticated(
        &config.relay_url,
        &config.bot_keys,
        config.owner_auth_tag.as_ref(),
    )
    .await
    .map_err(|error| match error {
        WsClientError::AuthFailed(_) => SessionFailure::terminal(anyhow!(error)),
        _ => SessionFailure::retry(anyhow!(error)),
    })?;

    if !state.profile_published {
        publish_profile(&mut connection, config).await?;
        state.profile_published = true;
    }
    if !state.membership_announced {
        announce_channel_membership(&mut connection, config).await?;
        state.membership_announced = true;
    }
    send_pending_reply(&mut connection, state).await?;
    subscribe_to_channel(
        &mut connection,
        &config.channel_id,
        state.subscription_since(),
    )
    .await?;

    loop {
        let message = match connection.next_event(RECEIVE_TIMEOUT).await {
            Ok(message) => message,
            Err(WsClientError::Timeout) => continue,
            Err(error) => return Err(SessionFailure::retry(anyhow!(error))),
        };

        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } if subscription_id == SUBSCRIPTION_ID => {
                state.observe_subscription_event(
                    caught_up,
                    event.created_at,
                    nostr::Timestamp::now(),
                );
                if !state.seen_event_ids.contains(&event.id.to_hex()) {
                    maybe_reply(&mut connection, config, state, &event).await?;
                }
            }
            RelayMessage::Eose { subscription_id } if subscription_id == SUBSCRIPTION_ID => {
                state.finish_catchup(session_started_at);
                caught_up = true;
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } if is_terminal_membership_closed(&subscription_id, &message) => {
                return Err(SessionFailure::terminal(anyhow!(
                    "relay closed channel subscription: {message}; add the bot pubkey as a channel member/admin-invited bot and restart"
                )));
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } if subscription_id == SUBSCRIPTION_ID => {
                return Err(SessionFailure::retry(anyhow!(
                    "relay closed channel subscription: {message}"
                )));
            }
            RelayMessage::Notice { message } => {
                tracing::warn!(relay_message = %message, "relay notice");
            }
            RelayMessage::Auth { .. } => {
                return Err(SessionFailure::retry(anyhow!(
                    "relay requested authentication again"
                )));
            }
            RelayMessage::Ok(ok) => {
                tracing::debug!(event_id = %ok.event_id, "ignored unsolicited relay OK");
            }
            RelayMessage::Event { .. }
            | RelayMessage::Closed { .. }
            | RelayMessage::Eose { .. }
            | RelayMessage::Count { .. } => {}
        }
    }
}

async fn publish_profile(
    connection: &mut NostrWsConnection,
    config: &Config,
) -> Result<(), SessionFailure> {
    let builder = buzz_sdk::builders::build_profile(
        Some(BOT_DISPLAY_NAME),
        Some(BOT_NAME),
        Some(BOT_ICON_DATA_URL),
        Some(BOT_ABOUT),
        None,
    )
    .map_err(|error| SessionFailure::terminal(anyhow!(error)))?;
    let profile_event = builder
        .sign_with_keys(&config.bot_keys)
        .map_err(|error| SessionFailure::terminal(anyhow!(error)))?;
    let ok = connection
        .send_event(profile_event)
        .await
        .map_err(|error| SessionFailure::retry(anyhow!(error)))?;
    if !ok.accepted {
        return Err(SessionFailure::terminal(anyhow!(
            "relay rejected profile {}: {}",
            ok.event_id,
            ok.message
        )));
    }
    tracing::info!(display_name = BOT_DISPLAY_NAME, "published kind:0 profile");
    Ok(())
}

async fn announce_channel_membership(
    connection: &mut NostrWsConnection,
    config: &Config,
) -> Result<(), SessionFailure> {
    let builder = EventBuilder::new(Kind::Custom(9000), "").tags([
        Tag::parse(["h", config.channel_id.as_str()])
            .map_err(|error| SessionFailure::terminal(anyhow!(error)))?,
        Tag::parse(["p", &config.bot_keys.public_key().to_hex()])
            .map_err(|error| SessionFailure::terminal(anyhow!(error)))?,
        Tag::parse(["role", "bot"]).map_err(|error| SessionFailure::terminal(anyhow!(error)))?,
    ]);
    let event = builder
        .sign_with_keys(&config.bot_keys)
        .map_err(|error| SessionFailure::terminal(anyhow!(error)))?;
    let ok = connection
        .send_event(event)
        .await
        .map_err(|error| SessionFailure::retry(anyhow!(error)))?;
    if ok.accepted {
        tracing::info!(
            display_name = BOT_DISPLAY_NAME,
            "announced channel bot membership"
        );
    } else {
        tracing::warn!(
            display_name = BOT_DISPLAY_NAME,
            relay_message = %ok.message,
            "could not self-add channel bot membership; private channels require an owner/admin invitation"
        );
    }
    Ok(())
}

async fn subscribe_to_channel(
    connection: &mut NostrWsConnection,
    channel_id: &str,
    since: nostr::Timestamp,
) -> Result<(), SessionFailure> {
    let filter = Filter::new()
        .kind(Kind::Custom(9))
        .custom_tag(
            SingleLetterTag::lowercase(Alphabet::H),
            channel_id.to_string(),
        )
        .since(since);
    connection
        .send_raw(&json!(["REQ", SUBSCRIPTION_ID, filter]))
        .await
        .map_err(|error| SessionFailure::retry(anyhow!(error)))
}

async fn maybe_reply(
    connection: &mut NostrWsConnection,
    config: &Config,
    state: &mut BotState,
    event: &Event,
) -> Result<(), SessionFailure> {
    if event.pubkey == config.bot_keys.public_key() || event.created_at < state.started_at {
        return Ok(());
    }

    let Some(reply) = event_reply(config, event) else {
        return Ok(());
    };

    let builder = buzz_sdk::builders::build_message(
        config
            .channel_id
            .parse::<uuid::Uuid>()
            .map_err(|error| SessionFailure::terminal(anyhow!(error)))?,
        &reply,
        None,
        &[&event.pubkey.to_hex()],
        false,
        &[],
    )
    .map_err(|error| SessionFailure::terminal(anyhow!(error)))?;
    let reply_event = builder
        .sign_with_keys(&config.bot_keys)
        .map_err(|error| SessionFailure::terminal(anyhow!(error)))?;
    state.pending_reply = Some(PendingReply {
        input_event_id: event.id.to_hex(),
        event: reply_event,
    });
    send_pending_reply(connection, state).await
}

async fn send_pending_reply(
    connection: &mut NostrWsConnection,
    state: &mut BotState,
) -> Result<(), SessionFailure> {
    let Some(pending) = state.pending_reply.as_ref() else {
        return Ok(());
    };
    let input_event_id = pending.input_event_id.clone();
    let ok = connection
        .send_event(pending.event.clone())
        .await
        .map_err(|error| SessionFailure::retry(anyhow!(error)))?;

    if !ok.accepted {
        state.pending_reply = None;
        return Err(SessionFailure::terminal(anyhow!(
            "relay rejected reply {} to {}: {}",
            ok.event_id,
            input_event_id,
            ok.message
        )));
    }

    state.seen_event_ids.record(input_event_id.clone());
    state.pending_reply = None;
    tracing::info!(input_event_id, reply_event_id = %ok.event_id, "published reply");
    Ok(())
}

fn event_reply(config: &Config, event: &Event) -> Option<String> {
    command_reply(&event.content).or_else(|| {
        event_mentions_bot(event, config).then(|| mention_command_reply(&event.content))?
    })
}

fn command_reply(content: &str) -> Option<String> {
    let mut parts = content.split_whitespace();
    let command = parts.next()?;
    let n = parts.next()?;

    match command {
        "!countdown" => Some(countdown_reply(n)),
        "!fib" => Some(fib_reply(n)),
        _ => None,
    }
}

fn mention_command_reply(content: &str) -> Option<String> {
    let tokens = content.split_whitespace().collect::<Vec<_>>();
    tokens.windows(2).find_map(|window| match window {
        ["countdown", n] => Some(countdown_reply(n)),
        ["fib", n] => Some(fib_reply(n)),
        _ => None,
    })
}

fn event_mentions_bot(event: &Event, config: &Config) -> bool {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("p")
            && parts.get(1).map(String::as_str) == Some(bot_pubkey.as_str())
    })
}

fn countdown_reply(n: &str) -> String {
    match parse_bounded(n, 1, 100) {
        Ok(n) => (1..=n)
            .rev()
            .map(|i| i.to_string())
            .chain(["🚀".to_string()])
            .collect::<Vec<_>>()
            .join(" "),
        Err(message) => message,
    }
}

fn fib_reply(n: &str) -> String {
    match parse_bounded(n, 1, 100) {
        Ok(n) => fibonacci_countdown(n)
            .into_iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        Err(message) => message,
    }
}

fn parse_bounded(s: &str, min: usize, max: usize) -> Result<usize, String> {
    let Ok(n) = s.parse::<usize>() else {
        return Err(format!("Please use a number from {min} to {max}."));
    };
    if (min..=max).contains(&n) {
        Ok(n)
    } else {
        Err(format!("Please use a number from {min} to {max}."))
    }
}

fn fibonacci_countdown(count: usize) -> Vec<u128> {
    let mut values = Vec::with_capacity(count);
    let (mut a, mut b) = (0, 1);
    for _ in 0..count {
        values.push(a);
        (a, b) = (b, a + b);
    }
    values.reverse();
    values
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

fn validate_relay_url(relay_url: &str) -> Result<()> {
    if !relay_url.contains("://") {
        bail!("BUZZ_RELAY_URL must use ws:// or wss://");
    }
    let parsed = url::Url::parse(relay_url).context("BUZZ_RELAY_URL must be a valid URL")?;
    if !matches!(parsed.scheme(), "ws" | "wss") {
        bail!("BUZZ_RELAY_URL must use ws:// or wss://");
    }
    if parsed.host_str().is_none() {
        bail!("BUZZ_RELAY_URL must include a host");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    async fn recv_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let message = ws
            .next()
            .await
            .expect("client should send a frame")
            .expect("client frame should be valid");
        let Message::Text(text) = message else {
            panic!("client should send text, got {message:?}");
        };
        serde_json::from_str(&text).expect("client text should be JSON")
    }

    async fn accept_event<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, expected_command: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let message = recv_json(ws).await;
        assert_eq!(message[0], expected_command);
        let event_id = message[1]["id"].as_str().expect("event should have an id");
        ws.send(Message::Text(
            json!(["OK", event_id, true, ""]).to_string().into(),
        ))
        .await
        .expect("relay should accept event");
    }

    fn command_event(keys: &Keys, channel_id: &str, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel_id]).expect("valid h tag")])
            .sign_with_keys(keys)
            .expect("sign command")
    }

    async fn authenticate<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        ws.send(Message::Text(
            json!(["AUTH", "test-challenge"]).to_string().into(),
        ))
        .await
        .expect("send challenge");
        accept_event(ws, "AUTH").await;
    }

    #[test]
    fn crypto_provider_is_ready_before_wss_client_config_is_built() {
        install_crypto_provider();
        install_crypto_provider();

        let _config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }

    #[test]
    fn membership_closed_is_terminal_only_for_the_bot_subscription() {
        assert!(is_terminal_membership_closed(
            SUBSCRIPTION_ID,
            "restricted: not a channel member"
        ));
        assert!(is_terminal_membership_closed(
            SUBSCRIPTION_ID,
            "restricted: channel access revoked"
        ));
        assert!(!is_terminal_membership_closed(
            "another-subscription",
            "restricted: not a channel member"
        ));
        assert!(!is_terminal_membership_closed(
            SUBSCRIPTION_ID,
            "error: database error"
        ));
    }

    #[test]
    fn replay_guard_keeps_a_bounded_window_of_accepted_inputs() {
        let mut replay = ReplayGuard::new(2);
        replay.record("event-a".to_string());
        replay.record("event-b".to_string());
        replay.record("event-c".to_string());

        assert!(!replay.contains("event-a"));
        assert!(replay.contains("event-b"));
        assert!(replay.contains("event-c"));
    }

    #[test]
    fn reconnect_cursor_waits_for_eose_and_then_bounds_overlap() {
        let mut state = BotState::new();
        let initial = state.subscription_since();
        let newest = nostr::Timestamp::from(initial.as_secs() + 120);

        state.observe_subscription_event(false, newest, newest);
        assert_eq!(state.subscription_since(), initial);

        state.finish_catchup(newest);
        assert_eq!(
            state.subscription_since().as_secs(),
            newest.as_secs() - REPLAY_OVERLAP_SECONDS
        );

        for offset in 0..=REPLAY_WINDOW {
            state.seen_event_ids.record(format!("event-{offset}"));
        }

        assert!(!state.seen_event_ids.contains("event-0"));
        let latest = nostr::Timestamp::from(newest.as_secs() + 120);
        state.observe_subscription_event(true, latest, latest);
        assert_eq!(
            state.subscription_since().as_secs(),
            latest.as_secs() - REPLAY_OVERLAP_SECONDS
        );
    }

    #[test]
    fn relay_url_accepts_only_websocket_schemes() {
        assert!(validate_relay_url("ws://localhost:3000").is_ok());
        assert!(validate_relay_url("wss://relay.example.com").is_ok());
        assert!(validate_relay_url("https://relay.example.com").is_err());
        assert!(validate_relay_url("ws:relative").is_err());
        assert!(validate_relay_url("not a url").is_err());
    }

    #[tokio::test]
    async fn reconnect_resends_the_same_pending_reply_then_deduplicates_replay() {
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let channel_id = "00000000-0000-0000-0000-000000000001";
        let inbound = command_event(&Keys::generate(), channel_id, "!countdown 2");
        let inbound_id = inbound.id.to_hex();
        let relay_inbound = inbound.clone();
        let rejected = command_event(&Keys::generate(), channel_id, "!fib 5");
        let rejected_id = rejected.id.to_hex();

        let relay = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.expect("first connection");
            let mut first = accept_async(first_stream).await.expect("first WebSocket");
            authenticate(&mut first).await;
            accept_event(&mut first, "EVENT").await;
            accept_event(&mut first, "EVENT").await;
            let first_req = recv_json(&mut first).await;
            assert_eq!(first_req[0], "REQ");
            first
                .send(Message::Text(
                    json!(["EOSE", SUBSCRIPTION_ID]).to_string().into(),
                ))
                .await
                .expect("finish historical catch-up");
            first
                .send(Message::Text(
                    json!(["EVENT", SUBSCRIPTION_ID, relay_inbound])
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send command");
            let first_reply = recv_json(&mut first).await;
            assert_eq!(first_reply[0], "EVENT");
            assert_eq!(first_reply[1]["content"], "2 1 🚀");
            let reply_id = first_reply[1]["id"].as_str().expect("reply id").to_string();
            first.close(None).await.expect("drop first connection");

            let (second_stream, _) = listener.accept().await.expect("second connection");
            let mut second = accept_async(second_stream).await.expect("second WebSocket");
            authenticate(&mut second).await;
            let resent_reply = recv_json(&mut second).await;
            assert_eq!(resent_reply[0], "EVENT");
            assert_eq!(resent_reply[1]["id"], reply_id);
            second
                .send(Message::Text(
                    json!(["OK", reply_id, true, ""]).to_string().into(),
                ))
                .await
                .expect("accept pending reply");

            let second_req = recv_json(&mut second).await;
            assert_eq!(second_req[0], "REQ");
            assert!(second_req[2]["since"].as_u64() >= first_req[2]["since"].as_u64());
            second
                .send(Message::Text(
                    json!(["EVENT", SUBSCRIPTION_ID, inbound])
                        .to_string()
                        .into(),
                ))
                .await
                .expect("replay command");
            assert!(
                tokio::time::timeout(Duration::from_millis(100), second.next())
                    .await
                    .is_err(),
                "accepted input replay must not emit another reply"
            );
            second
                .send(Message::Text(
                    json!(["EVENT", SUBSCRIPTION_ID, rejected])
                        .to_string()
                        .into(),
                ))
                .await
                .expect("send second command");
            let rejected_reply = recv_json(&mut second).await;
            assert_eq!(rejected_reply[1]["content"], "3 2 1 1 0");
            let rejected_reply_id = rejected_reply[1]["id"].as_str().expect("reply id");
            second
                .send(Message::Text(
                    json!(["OK", rejected_reply_id, false, "restricted: test rejection"])
                        .to_string()
                        .into(),
                ))
                .await
                .expect("reject second reply");
        });

        let config = Config {
            relay_url: format!("ws://{address}"),
            channel_id: channel_id.to_string(),
            bot_keys: Keys::generate(),
            owner_auth_tag: None,
        };
        let mut state = BotState::new();
        let first = run_session(&config, &mut state)
            .await
            .expect_err("first socket closes before OK");
        assert!(!first.terminal);
        assert!(!state.seen_event_ids.contains(&inbound_id));
        assert!(state.pending_reply.is_some());

        let second = run_session(&config, &mut state)
            .await
            .expect_err("reply rejection is terminal");
        assert!(second.terminal);
        assert!(second.error.to_string().contains("test rejection"));
        assert!(state.seen_event_ids.contains(&inbound_id));
        assert!(!state.seen_event_ids.contains(&rejected_id));
        assert!(state.pending_reply.is_none());
        relay.await.expect("relay script");
    }

    #[test]
    fn countdown_command_is_algorithmic_and_bounded() {
        assert_eq!(
            command_reply("!countdown 5").as_deref(),
            Some("5 4 3 2 1 🚀")
        );
        assert_eq!(
            command_reply("!countdown 0").as_deref(),
            Some("Please use a number from 1 to 100.")
        );
        assert_eq!(
            command_reply("!countdown 101").as_deref(),
            Some("Please use a number from 1 to 100.")
        );
    }

    #[test]
    fn fibonacci_command_counts_down_and_is_bounded() {
        assert_eq!(command_reply("!fib 5").as_deref(), Some("3 2 1 1 0"));
        assert_eq!(command_reply("!fib 8").as_deref(), Some("13 8 5 3 2 1 1 0"));
        assert_eq!(
            command_reply("!fib 101").as_deref(),
            Some("Please use a number from 1 to 100.")
        );
    }

    #[test]
    fn mention_commands_are_algorithmic_and_bounded() {
        assert_eq!(
            mention_command_reply("@Countdown Bot countdown 5").as_deref(),
            Some("5 4 3 2 1 🚀")
        );
        assert_eq!(
            mention_command_reply("@Countdown Bot fib 5").as_deref(),
            Some("3 2 1 1 0")
        );
        assert_eq!(
            mention_command_reply("@Countdown Bot fib 101").as_deref(),
            Some("Please use a number from 1 to 100.")
        );
    }
}
