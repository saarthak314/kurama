use std::time::Duration;

use kurama_protocol::KuramaError;
use reqwest::{Client, RequestBuilder, Response, redirect::Policy};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BYTES: usize = 16 * 1024;

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
        KuramaError::Model(self.to_string())
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
            Some(status) => write!(formatter, "{class} HTTP error {status}: {}", self.message),
            None => write!(formatter, "{class} HTTP error: {}", self.message),
        }
    }
}

impl std::error::Error for HttpFailure {}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    user_agent: String,
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
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .tcp_nodelay(true)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(&user_agent)
            .build()
            .map_err(|error| KuramaError::Configuration(format!("HTTP client: {error}")))?;
        Ok(Self { client, user_agent })
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
                    let remaining = MAX_ERROR_BYTES.saturating_sub(body.len());
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if body.len() == MAX_ERROR_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    return Self::transport_error(provider, &error, secrets);
                }
            }
        }
        Self::provider_error(provider, status, &String::from_utf8_lossy(&body), secrets)
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn bounded_redacted(text: &str, secrets: &[impl AsRef<str>]) -> String {
    let mut bounded = if text.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    } else {
        text.to_owned()
    };

    for secret in secrets {
        let secret = secret.as_ref();
        if !secret.is_empty() {
            bounded = bounded.replace(secret, "[REDACTED]");
        }
    }
    redact_authorization_values(&bounded)
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
