use std::time::Duration;

use kurama_protocol::KuramaError;
use reqwest::{Client, RequestBuilder, Response, redirect::Policy};

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
use kurama_protocol::{
    model::{ModelEvent, ModelRequest},
    traits::{CancelSignal, ModelStream},
};
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
use zeroize::Zeroizing;

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
use crate::providers::sse::{SseDecoder, SseEvent};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BYTES: usize = 16 * 1024;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
const MAX_DELEGATION_CANDIDATE_EVENTS: usize = 64;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
const MAX_DELEGATION_CANDIDATE_EVENT_BYTES: usize = 64 * 1024;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
const MAX_DELEGATION_RESPONSE_BYTES: usize = 1024 * 1024;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
const DELEGATION_OPEN: &str = "<kurama_delegate>";
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
const DELEGATION_CLOSE: &str = "</kurama_delegate>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorClass {
    Transient,
    Authentication,
    Permanent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFailure {
    class: HttpErrorClass,
    status: Option<u16>,
    message: String,
}

impl HttpFailure {
    pub fn class(&self) -> HttpErrorClass {
        self.class
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_transient(&self) -> bool {
        self.class == HttpErrorClass::Transient
    }

    pub fn into_kurama(self) -> KuramaError {
        KuramaError::Provider {
            message: self.to_string(),
            retryable: self.is_transient(),
        }
    }
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let class = match self.class {
            HttpErrorClass::Transient => "transient",
            HttpErrorClass::Authentication => "authentication",
            HttpErrorClass::Permanent => "permanent",
        };
        match self.status {
            Some(status) => write!(formatter, "{class}: HTTP error {status}: {}", self.message),
            None => write!(formatter, "{class}: HTTP error: {}", self.message),
        }
    }
}

impl std::error::Error for HttpFailure {}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    user_agent: String,
    #[cfg(feature = "tools")]
    timeout: Duration,
}

impl HttpClient {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
            .expect("Kurama's static HTTP client configuration must be valid")
    }

    pub fn try_new() -> Result<Self, KuramaError> {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Result<Self, KuramaError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let user_agent = format!("kurama/{}", env!("CARGO_PKG_VERSION"));
        let client = Self::client_builder(timeout, &user_agent)
            .build()
            .map_err(|error| KuramaError::Configuration(format!("HTTP client: {error}")))?;
        Ok(Self {
            client,
            user_agent,
            #[cfg(feature = "tools")]
            timeout,
        })
    }

    fn client_builder(timeout: Duration, user_agent: &str) -> reqwest::ClientBuilder {
        Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .tcp_nodelay(true)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(user_agent)
    }

    #[cfg(feature = "tools")]
    pub(crate) fn pinned_client(
        &self,
        host: &str,
        addresses: &[std::net::SocketAddr],
    ) -> Result<Client, KuramaError> {
        if addresses.is_empty() {
            return Err(KuramaError::Configuration(
                "HTTP DNS pin has no addresses".into(),
            ));
        }
        Self::client_builder(self.timeout, &self.user_agent)
            .resolve_to_addrs(host, addresses)
            .build()
            .map_err(|error| KuramaError::Configuration(format!("HTTP client: {error}")))
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.client.get(url)
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.client.post(url)
    }

    pub fn classify_status(status: u16) -> HttpErrorClass {
        match status {
            401 | 403 => HttpErrorClass::Authentication,
            408 | 409 | 429 | 500..=599 => HttpErrorClass::Transient,
            _ => HttpErrorClass::Permanent,
        }
    }

    pub fn classify_reqwest(error: &reqwest::Error) -> HttpErrorClass {
        if error.is_timeout() || error.is_connect() {
            HttpErrorClass::Transient
        } else if let Some(status) = error.status() {
            Self::classify_status(status.as_u16())
        } else {
            HttpErrorClass::Permanent
        }
    }

    pub fn provider_error(
        provider: &str,
        status: u16,
        body: &str,
        secrets: &[impl AsRef<str>],
    ) -> HttpFailure {
        let message = bounded_redacted(body, secrets);
        HttpFailure {
            class: Self::classify_status(status),
            status: Some(status),
            message: format!("{provider}: {message}"),
        }
    }

    pub fn transport_error(
        provider: &str,
        error: &reqwest::Error,
        secrets: &[impl AsRef<str>],
    ) -> HttpFailure {
        HttpFailure {
            class: Self::classify_reqwest(error),
            status: error.status().map(|status| status.as_u16()),
            message: format!(
                "{provider}: {}",
                bounded_redacted(&error.to_string(), secrets)
            ),
        }
    }

    pub async fn response_error(
        provider: &str,
        mut response: Response,
        secrets: &[impl AsRef<str>],
    ) -> HttpFailure {
        let status = response.status().as_u16();
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = (MAX_ERROR_BYTES + 1).saturating_sub(body.len());
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if body.len() > MAX_ERROR_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let body = redact_error_prefix(&error_body_text(&body), secrets, true);
                    let detail = bounded_redacted(&error.to_string(), secrets);
                    return Self::provider_error(
                        provider,
                        status,
                        &format!("body read failed: {detail}; {body}"),
                        secrets,
                    );
                }
            }
        }
        let message = redact_error_prefix(
            &error_body_text(&body),
            secrets,
            body.len() > MAX_ERROR_BYTES,
        );
        HttpFailure {
            class: Self::classify_status(status),
            status: Some(status),
            message: format!("{provider}: {message}"),
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
pub(crate) trait SseNormalizer: Send + 'static {
    fn push(&mut self, payload: &str) -> Result<Vec<ModelEvent>, KuramaError>;
    fn finish(&mut self) -> Result<Vec<ModelEvent>, KuramaError>;
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
pub(crate) async fn sse_model_stream<N: SseNormalizer>(
    response: Response,
    request: &ModelRequest,
    cancel: &dyn CancelSignal,
    provider: &'static str,
    secrets: Vec<Zeroizing<String>>,
    normalizer: N,
    normalize_events: fn(Vec<ModelEvent>, bool) -> Result<Vec<ModelEvent>, KuramaError>,
) -> Result<ModelStream, KuramaError> {
    let mut state = SseStreamState::new(
        response,
        provider,
        secrets,
        normalizer,
        request.delegation.is_some(),
        normalize_events,
    );
    let mut cancelled = cancel.cancelled();
    loop {
        let batch = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(KuramaError::Cancelled),
            batch = state.next_batch() => batch,
        }
        .map_err(|error| bounded_redacted_error(error, &state.secrets))?;
        match batch {
            Some(events) if !events.is_empty() => {
                state.pending.extend(events);
                return Ok(Box::pin(futures_util::stream::unfold(
                    Some((state, cancelled)),
                    |state| async move {
                        let (mut state, mut cancelled) = state?;
                        let next = tokio::select! {
                            biased;
                            _ = &mut cancelled => return Some((Err(KuramaError::Cancelled), None)),
                            next = state.next_event() => next,
                        };
                        match next {
                            Ok(Some(event)) => Some((Ok(event), Some((state, cancelled)))),
                            Ok(None) => None,
                            Err(error) => {
                                let error = bounded_redacted_error(error, &state.secrets);
                                Some((Err(error), None))
                            }
                        }
                    },
                )));
            }
            Some(_) => {}
            None => return Ok(Box::pin(futures_util::stream::empty())),
        }
    }
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
struct SseStreamState<N> {
    response: Response,
    provider: &'static str,
    secrets: Vec<Zeroizing<String>>,
    decoder: SseDecoder,
    normalizer: N,
    event_gate: EventGate,
    chunk: bytes::Bytes,
    chunk_offset: usize,
    decoder_finished: bool,
    pending: std::collections::VecDeque<ModelEvent>,
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
impl<N: SseNormalizer> SseStreamState<N> {
    fn new(
        response: Response,
        provider: &'static str,
        secrets: Vec<Zeroizing<String>>,
        normalizer: N,
        delegation_enabled: bool,
        normalize_events: fn(Vec<ModelEvent>, bool) -> Result<Vec<ModelEvent>, KuramaError>,
    ) -> Self {
        Self {
            response,
            provider,
            secrets,
            decoder: SseDecoder::default(),
            normalizer,
            event_gate: EventGate::new(delegation_enabled, normalize_events),
            chunk: bytes::Bytes::new(),
            chunk_offset: 0,
            decoder_finished: false,
            pending: std::collections::VecDeque::new(),
        }
    }

    async fn next_event(&mut self) -> Result<Option<ModelEvent>, KuramaError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            match self.next_batch().await? {
                Some(events) => self.pending.extend(events),
                None => return Ok(None),
            }
        }
    }

    async fn next_batch(&mut self) -> Result<Option<Vec<ModelEvent>>, KuramaError> {
        loop {
            if let Some(event) = self.next_sse_event().await? {
                let events = self.normalizer.push(&event.data)?;
                let events = self.event_gate.push(events)?;
                if !events.is_empty() {
                    return Ok(Some(events));
                }
                continue;
            }

            let events = self.normalizer.finish()?;
            let events = self.event_gate.push(events)?;
            let events = self.event_gate.finish(events)?;
            return if events.is_empty() {
                Ok(None)
            } else {
                Ok(Some(events))
            };
        }
    }

    async fn next_sse_event(&mut self) -> Result<Option<SseEvent>, KuramaError> {
        loop {
            if self.chunk_offset < self.chunk.len() {
                let (consumed, event) =
                    self.decoder.push_chunk(&self.chunk[self.chunk_offset..])?;
                self.chunk_offset += consumed;
                if let Some(event) = event {
                    return Ok(Some(event));
                }
                continue;
            }
            self.chunk = bytes::Bytes::new();
            self.chunk_offset = 0;

            if self.decoder_finished {
                return Ok(None);
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.chunk = chunk,
                Ok(None) => {
                    self.decoder_finished = true;
                    return self.decoder.finish_event();
                }
                Err(error) => {
                    return Err(
                        HttpClient::transport_error(self.provider, &error, &self.secrets)
                            .into_kurama(),
                    );
                }
            }
        }
    }
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
struct EventGate {
    delegation_enabled: bool,
    normalize_events: fn(Vec<ModelEvent>, bool) -> Result<Vec<ModelEvent>, KuramaError>,
    mode: EventGateMode,
    buffered_text: String,
    buffered_events: Vec<ModelEvent>,
    buffered_event_bytes: usize,
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
impl EventGate {
    fn new(
        delegation_enabled: bool,
        normalize_events: fn(Vec<ModelEvent>, bool) -> Result<Vec<ModelEvent>, KuramaError>,
    ) -> Self {
        Self {
            delegation_enabled,
            normalize_events,
            mode: EventGateMode::Detecting,
            buffered_text: String::new(),
            buffered_events: Vec::new(),
            buffered_event_bytes: 0,
        }
    }

    fn push(&mut self, events: Vec<ModelEvent>) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut output = Vec::new();
        for event in events {
            match (&self.mode, event) {
                (_, ModelEvent::TextDelta { text }) => self.push_text(text, &mut output)?,
                (
                    EventGateMode::Candidate,
                    ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    },
                ) => {
                    let mut buffered = Vec::with_capacity(self.buffered_events.len() + 2);
                    buffered.push(ModelEvent::TextDelta {
                        text: std::mem::take(&mut self.buffered_text),
                    });
                    buffered.append(&mut self.buffered_events);
                    buffered.push(ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    });
                    output.extend((self.normalize_events)(buffered, self.delegation_enabled)?);
                    self.buffered_event_bytes = 0;
                    self.mode = EventGateMode::Passthrough;
                }
                (EventGateMode::Candidate, event) => self.buffer_candidate_event(event)?,
                (
                    EventGateMode::Detecting,
                    ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    },
                ) => {
                    self.flush_text(&mut output);
                    self.mode = EventGateMode::Passthrough;
                    output.push(ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    });
                }
                (
                    EventGateMode::Passthrough,
                    ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    },
                ) => {
                    self.flush_passthrough_tail(&mut output);
                    output.push(ModelEvent::ResponseCompleted {
                        cursor,
                        finish_reason,
                    });
                }
                (EventGateMode::Detecting, event) => output.push(event),
                (EventGateMode::Passthrough, event) => output.push(event),
            }
        }
        Ok(output)
    }

    fn finish(&mut self, mut events: Vec<ModelEvent>) -> Result<Vec<ModelEvent>, KuramaError> {
        match self.mode {
            EventGateMode::Detecting => self.flush_text(&mut events),
            EventGateMode::Candidate => {
                let mut buffered = Vec::with_capacity(self.buffered_events.len() + 1);
                buffered.push(ModelEvent::TextDelta {
                    text: std::mem::take(&mut self.buffered_text),
                });
                buffered.append(&mut self.buffered_events);
                events.extend((self.normalize_events)(buffered, self.delegation_enabled)?);
                self.buffered_event_bytes = 0;
            }
            EventGateMode::Passthrough => self.flush_passthrough_tail(&mut events),
        }
        self.mode = EventGateMode::Passthrough;
        Ok(events)
    }

    fn push_text(&mut self, text: String, output: &mut Vec<ModelEvent>) -> Result<(), KuramaError> {
        match self.mode {
            EventGateMode::Passthrough => {
                self.buffered_text.push_str(&text);
                self.flush_safe_passthrough(output)?;
            }
            EventGateMode::Candidate => {
                self.buffered_text.push_str(&text);
                self.check_buffer_bound()?;
            }
            EventGateMode::Detecting => {
                self.buffered_text.push_str(&text);
                self.check_buffer_bound()?;
                let trimmed = self.buffered_text.trim_start();
                if trimmed.starts_with(DELEGATION_OPEN) {
                    self.mode = EventGateMode::Candidate;
                } else if contains_delegation_marker(&self.buffered_text) {
                    return Err(delegation_marker_error());
                } else if !DELEGATION_OPEN.starts_with(trimmed) && !trimmed.is_empty() {
                    self.mode = EventGateMode::Passthrough;
                    self.flush_safe_passthrough(output)?;
                }
            }
        }
        Ok(())
    }

    fn buffer_candidate_event(&mut self, event: ModelEvent) -> Result<(), KuramaError> {
        if self.buffered_events.len() >= MAX_DELEGATION_CANDIDATE_EVENTS {
            return Err(KuramaError::Protocol(format!(
                "delegation candidate exceeds {MAX_DELEGATION_CANDIDATE_EVENTS} buffered non-text events"
            )));
        }
        let event_bytes = serde_json::to_vec(&event)
            .map_err(|error| {
                KuramaError::Protocol(format!(
                    "failed to size delegation candidate event: {error}"
                ))
            })?
            .len();
        let buffered_event_bytes = self.buffered_event_bytes.saturating_add(event_bytes);
        if buffered_event_bytes > MAX_DELEGATION_CANDIDATE_EVENT_BYTES {
            return Err(KuramaError::Protocol(format!(
                "delegation candidate exceeds {MAX_DELEGATION_CANDIDATE_EVENT_BYTES} buffered non-text bytes"
            )));
        }
        self.buffered_event_bytes = buffered_event_bytes;
        self.buffered_events.push(event);
        Ok(())
    }

    fn flush_safe_passthrough(&mut self, output: &mut Vec<ModelEvent>) -> Result<(), KuramaError> {
        if contains_delegation_marker(&self.buffered_text) {
            return Err(delegation_marker_error());
        }
        let retained = delegation_marker_prefix_suffix_len(&self.buffered_text);
        let safe_len = self.buffered_text.len() - retained;
        if safe_len == 0 {
            return Ok(());
        }
        let retained = self.buffered_text.split_off(safe_len);
        let safe = std::mem::replace(&mut self.buffered_text, retained);
        output.push(ModelEvent::TextDelta { text: safe });
        Ok(())
    }

    fn flush_passthrough_tail(&mut self, output: &mut Vec<ModelEvent>) {
        self.flush_text(output);
    }

    fn check_buffer_bound(&self) -> Result<(), KuramaError> {
        if self.buffered_text.len() > MAX_DELEGATION_RESPONSE_BYTES {
            return Err(KuramaError::Protocol(format!(
                "delegation control response exceeds {MAX_DELEGATION_RESPONSE_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn flush_text(&mut self, output: &mut Vec<ModelEvent>) {
        if !self.buffered_text.is_empty() {
            output.push(ModelEvent::TextDelta {
                text: std::mem::take(&mut self.buffered_text),
            });
        }
    }
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
enum EventGateMode {
    Detecting,
    Candidate,
    Passthrough,
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
fn contains_delegation_marker(text: &str) -> bool {
    text.contains(DELEGATION_OPEN) || text.contains(DELEGATION_CLOSE)
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
fn delegation_marker_prefix_suffix_len(text: &str) -> usize {
    let max_len = DELEGATION_OPEN.len().max(DELEGATION_CLOSE.len()) - 1;
    let text = text.as_bytes();
    (1..=text.len().min(max_len))
        .rev()
        .find(|length| {
            let suffix = &text[text.len() - length..];
            DELEGATION_OPEN.as_bytes().starts_with(suffix)
                || DELEGATION_CLOSE.as_bytes().starts_with(suffix)
        })
        .unwrap_or_default()
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
fn delegation_marker_error() -> KuramaError {
    KuramaError::Protocol("delegation marker appeared after ordinary text".into())
}

fn error_body_text(mut body: &[u8]) -> std::borrow::Cow<'_, str> {
    // Do not replace a cut-off UTF-8 code point with U+FFFD: that would hide
    // a credential-prefix match at a transport or size boundary.
    let mut suffix = body;
    while let Err(error) = std::str::from_utf8(suffix) {
        match error.error_len() {
            Some(length) => suffix = &suffix[error.valid_up_to() + length..],
            None => {
                body = &body[..body.len() - suffix.len() + error.valid_up_to()];
                break;
            }
        }
    }
    String::from_utf8_lossy(body)
}

pub fn bounded_redacted(text: &str, secrets: &[impl AsRef<str>]) -> String {
    redact_error_prefix(text, secrets, text.len() > MAX_ERROR_BYTES)
}

fn redact_error_prefix(text: &str, secrets: &[impl AsRef<str>], incomplete: bool) -> String {
    let mut end = text.len().min(MAX_ERROR_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut redacted = text[..end].to_owned();
    for secret in secrets {
        let secret = secret.as_ref();
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    // A bounded/error-interrupted body may end inside a credential. Redact the
    // possible prefix conservatively instead of retaining unbounded overlap.
    if incomplete {
        let mut safe_end = redacted.len();
        for secret in secrets {
            let secret = secret.as_ref();
            let start = redacted.len().saturating_sub(secret.len());
            if let Some((index, _)) = redacted
                .char_indices()
                .find(|(index, _)| *index >= start && secret.starts_with(&redacted[*index..]))
            {
                safe_end = safe_end.min(index);
            }
        }
        if safe_end < redacted.len() {
            redacted.truncate(safe_end);
            redacted.push_str("[REDACTED]");
        }
    }
    let mut redacted = redact_authorization_values(&redacted);
    if redacted.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        redacted.truncate(end);
    }
    if incomplete || text.len() > MAX_ERROR_BYTES {
        redacted.push('…');
    }
    redacted
}

pub fn bounded_redacted_error(error: KuramaError, secrets: &[impl AsRef<str>]) -> KuramaError {
    match error {
        KuramaError::Configuration(message) => {
            KuramaError::Configuration(bounded_redacted(&message, secrets))
        }
        KuramaError::Model(message) => KuramaError::Model(bounded_redacted(&message, secrets)),
        KuramaError::Tool(message) => KuramaError::Tool(bounded_redacted(&message, secrets)),
        KuramaError::Protocol(message) => {
            KuramaError::Protocol(bounded_redacted(&message, secrets))
        }
        other => other,
    }
}

fn redact_authorization_values(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        if let Some((name, _)) = line.split_once(':')
            && (name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("x-api-key"))
        {
            output.push_str(name);
            output.push_str(": [REDACTED]");
        } else if let Some(index) = find_ascii_case_insensitive(line, "bearer ") {
            output.push_str(&line[..index]);
            output.push_str("Bearer [REDACTED]");
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    output.pop();
    output
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(all(test, feature = "tools"))]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn pinned_client_preserves_host_and_refuses_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("server");
        let address = listener.local_addr().expect("address");
        let destination = TcpListener::bind("127.0.0.1:0").await.expect("destination");
        let destination_address = destination.local_addr().expect("destination address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("pinned request");
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.expect("request bytes");
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            socket.write_all(format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{destination_address}/redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            ).as_bytes()).await.expect("response");
            String::from_utf8(request).expect("request UTF-8")
        });
        let http = HttpClient::with_timeout(Duration::from_secs(1)).expect("client");
        let client = http
            .pinned_client("unresolvable.test", &[address])
            .expect("pinned client");
        let response = client
            .get(format!("http://unresolvable.test:{}/", address.port()))
            .send()
            .await
            .expect("pinned response");
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            server
                .await
                .expect("server task")
                .contains(&format!("host: unresolvable.test:{}\r\n", address.port()))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), destination.accept())
                .await
                .is_err()
        );
    }
}
