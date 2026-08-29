use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use kurama_protocol::{
    KuramaError,
    policy::ExecutionMode,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, LOCATION};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::{credentials::SecretValue, http::HttpClient};

use super::{html_text::html_to_text, limits::BoundedOutput};

const MAX_REDIRECTS: usize = 5;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub trait SearchBackend: Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, KuramaError>>;
}

pub struct JsonSearchBackend {
    http: HttpClient,
    endpoint: String,
    auth: Option<SecretValue>,
}

impl JsonSearchBackend {
    pub fn new(endpoint: impl Into<String>, auth: Option<SecretValue>) -> Self {
        Self {
            http: HttpClient::new(),
            endpoint: endpoint.into(),
            auth,
        }
    }

    pub fn with_client(
        http: HttpClient,
        endpoint: impl Into<String>,
        auth: Option<SecretValue>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            auth,
        }
    }
}

impl SearchBackend for JsonSearchBackend {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, KuramaError>> {
        Box::pin(async move {
            let endpoint = Url::parse(&self.endpoint).map_err(|error| {
                KuramaError::Configuration(format!("invalid search endpoint: {error}"))
            })?;

            let mut request = self
                .http
                .client()
                .post(endpoint)
                .json(&serde_json::json!({"query": query, "limit": limit}));
            if let Some(secret) = &self.auth {
                request = request.header(AUTHORIZATION, format!("Bearer {}", secret.expose()));
            }
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
                response = request.send() => response.map_err(|error| KuramaError::Tool(format!("search request failed: {error}")))?,
            };
            if !response.status().is_success() {
                return Err(KuramaError::Tool(format!(
                    "search endpoint returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            let bytes = read_response_bytes(response, cancel).await?;
            let payload: SearchPayload = serde_json::from_slice(&bytes)
                .map_err(|error| KuramaError::Tool(format!("invalid search response: {error}")))?;
            validate_results(payload.results, limit)
        })
    }
}

pub struct OpenAiNativeSearch {
    http: HttpClient,
    endpoint: String,
    secret: SecretValue,
    model: String,
}

impl OpenAiNativeSearch {
    pub fn new(
        http: HttpClient,
        endpoint: impl Into<String>,
        secret: SecretValue,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            secret,
            model: model.into(),
        }
    }
}

impl SearchBackend for OpenAiNativeSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, KuramaError>> {
        Box::pin(async move {
            let base = Url::parse(&self.endpoint).map_err(|error| {
                KuramaError::Configuration(format!("invalid OpenAI endpoint: {error}"))
            })?;
            let endpoint = append_endpoint(&base, "responses")?;
            let schema = search_result_schema(limit);
            let request = self
                .http
                .client()
                .post(endpoint)
                .header(AUTHORIZATION, format!("Bearer {}", self.secret.expose()))
                .json(&serde_json::json!({
                    "model": self.model,
                    "input": query,
                    "tools": [{"type": "web_search"}],
                    "include": ["web_search_call.action.sources"],
                    "text": {"format": {
                        "type": "json_schema",
                        "name": "search_results",
                        "strict": true,
                        "schema": schema
                    }},
                    "store": false
                }));
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
                response = request.send() => response.map_err(|error| KuramaError::Tool(format!("native search failed: {error}")))?,
            };
            if !response.status().is_success() {
                return Err(KuramaError::Tool(format!(
                    "native search returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            let bytes = read_response_bytes(response, cancel).await?;
            let payload: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                KuramaError::Tool(format!("invalid native search response: {error}"))
            })?;
            let output = response_output_text(&payload).ok_or_else(|| {
                KuramaError::Tool("native search response omitted structured output".into())
            })?;
            let results: SearchPayload = serde_json::from_str(output).map_err(|error| {
                KuramaError::Tool(format!("invalid native search output: {error}"))
            })?;
            validate_results(results.results, limit)
        })
    }
}

pub struct WebSearchTool {
    http: Option<HttpClient>,
    backend: Option<Arc<dyn SearchBackend>>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self {
            http: Some(HttpClient::new()),
            backend: None,
        }
    }
}

impl WebSearchTool {
    pub fn new(http: HttpClient, backend: Option<Arc<dyn SearchBackend>>) -> Self {
        Self {
            http: Some(http),
            backend,
        }
    }

    pub fn with_backend(backend: impl SearchBackend + 'static) -> Self {
        Self {
            http: Some(HttpClient::new()),
            backend: Some(Arc::new(backend)),
        }
    }

    fn http(&self) -> Result<&HttpClient, KuramaError> {
        self.http.as_ref().ok_or_else(|| {
            KuramaError::Configuration("HTTP client is unavailable for web open".into())
        })
    }
}

impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "web-search".into(),
            description: "Search the public web or open one bounded public page.".into(),
            parameters: serde_json::json!({
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "operation": {"const": "search"},
                            "query": {"type": "string", "maxLength": 2048},
                            "limit": {"type": "integer", "minimum": 1, "maximum": 8},
                            "contains_workspace_data": {"type": "boolean"}
                        },
                        "required": ["operation", "query", "limit", "contains_workspace_data"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "operation": {"const": "open"},
                            "url": {"type": "string", "maxLength": 4096}
                        },
                        "required": ["operation", "url"],
                        "additionalProperties": false
                    }
                ]
            }),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        match parse_arguments(invocation)? {
            WebArguments::Search {
                query,
                contains_workspace_data,
                ..
            } => Ok(Operation::WebSearch {
                query,
                contains_workspace_data,
            }),
            WebArguments::Open { url } => {
                let parsed = parse_http_url(&url)?;
                Ok(Operation::WebOpen {
                    url,
                    private_target: literal_private_target(&parsed),
                })
            }
        }
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move {
            match parse_arguments(&invocation)? {
                WebArguments::Search { query, limit, .. } => {
                    let backend = self.backend.as_ref().ok_or_else(|| {
                        KuramaError::Configuration("no web search backend is configured".into())
                    })?;
                    let results = backend.search(&query, limit, cancel).await?;
                    let output = results
                        .iter()
                        .enumerate()
                        .map(|(index, result)| {
                            format!(
                                "{}. {}\n{}\n{}",
                                index + 1,
                                result.title,
                                result.url,
                                result.snippet
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    Ok(ToolResult {
                        call_id: invocation.call_id,
                        output,
                        is_error: false,
                        metadata: serde_json::json!({"results": results}),
                        truncated: false,
                        blob_refs: Vec::new(),
                    })
                }
                WebArguments::Open { url } => {
                    let url = parse_http_url(&url)?;
                    let page = open_page(self.http()?, url, context.mode, cancel).await?;
                    let readable = match page.content_type.as_str() {
                        "text/html" => html_to_text(&String::from_utf8_lossy(&page.bytes)),
                        _ => String::from_utf8_lossy(&page.bytes).into_owned(),
                    };
                    let mut bounded = BoundedOutput::new(context.limits);
                    bounded.push(readable.as_bytes());
                    let bounded = bounded.finish();
                    Ok(ToolResult {
                        call_id: invocation.call_id,
                        output: bounded.text,
                        is_error: false,
                        metadata: serde_json::json!({
                            "url": page.url,
                            "content_type": page.content_type,
                            "bytes": page.bytes.len(),
                            "readable_bytes": bounded.total_bytes,
                            "readable_lines": bounded.total_lines,
                            "omitted_bytes": bounded.omitted_bytes,
                            "omitted_lines": bounded.omitted_lines
                        }),
                        truncated: bounded.truncated,
                        blob_refs: Vec::new(),
                    })
                }
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum WebArguments {
    Search {
        query: String,
        limit: usize,
        contains_workspace_data: bool,
    },
    Open {
        url: String,
    },
}

#[derive(Deserialize)]
struct SearchPayload {
    results: Vec<SearchResult>,
}

struct Page {
    url: String,
    content_type: String,
    bytes: Vec<u8>,
}

fn parse_arguments(invocation: &ToolInvocation) -> Result<WebArguments, KuramaError> {
    if invocation.name != "web-search" {
        return Err(KuramaError::Tool(format!(
            "web-search received invocation for {}",
            invocation.name
        )));
    }
    let arguments: WebArguments = serde_json::from_value(invocation.arguments.clone())
        .map_err(|error| KuramaError::Tool(format!("invalid web-search arguments: {error}")))?;
    match &arguments {
        WebArguments::Search { query, limit, .. } => {
            if query.is_empty() || query.len() > 2048 || !(1..=8).contains(limit) {
                return Err(KuramaError::Tool(
                    "search query or result limit is outside bounds".into(),
                ));
            }
        }
        WebArguments::Open { url } => {
            if url.len() > 4096 {
                return Err(KuramaError::Tool("URL exceeds 4096 bytes".into()));
            }
        }
    }
    Ok(arguments)
}

fn parse_http_url(value: &str) -> Result<Url, KuramaError> {
    let url =
        Url::parse(value).map_err(|error| KuramaError::Tool(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(KuramaError::Tool(
            "web open accepts only HTTP and HTTPS URLs".into(),
        ));
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(KuramaError::Tool(
            "web open URL must have a host and no embedded credentials".into(),
        ));
    }
    Ok(url)
}

async fn open_page(
    http: &HttpClient,
    mut url: Url,
    mode: ExecutionMode,
    cancel: &dyn CancelSignal,
) -> Result<Page, KuramaError> {
    for redirects in 0..=MAX_REDIRECTS {
        validate_remote_url(&url, mode).await?;
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
            response = http.client().get(url.clone()).send() => response.map_err(|error| KuramaError::Tool(format!("page request failed: {error}")))?,
        };
        if response.status().is_redirection() {
            if redirects == MAX_REDIRECTS {
                return Err(KuramaError::Tool("page exceeded five redirects".into()));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    KuramaError::Tool("redirect omitted a valid Location header".into())
                })?;
            url = url
                .join(location)
                .map_err(|error| KuramaError::Tool(format!("invalid redirect URL: {error}")))?;
            continue;
        }
        if !response.status().is_success() {
            return Err(KuramaError::Tool(format!(
                "page returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            content_type.as_str(),
            "text/html" | "text/plain" | "application/json"
        ) {
            return Err(KuramaError::Tool(format!(
                "unsupported page content type: {content_type}"
            )));
        }
        let bytes = read_response_bytes(response, cancel).await?;
        return Ok(Page {
            url: url.to_string(),
            content_type,
            bytes,
        });
    }
    Err(KuramaError::Tool("redirect loop".into()))
}

async fn read_response_bytes(
    response: reqwest::Response,
    cancel: &dyn CancelSignal,
) -> Result<Vec<u8>, KuramaError> {
    if response
        .content_length()
        .is_some_and(|bytes| bytes > MAX_PAGE_BYTES as u64)
    {
        return Err(KuramaError::Tool("response exceeds 2 MiB".into()));
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
            next = stream.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk =
            chunk.map_err(|error| KuramaError::Tool(format!("response body failed: {error}")))?;
        if output.len().saturating_add(chunk.len()) > MAX_PAGE_BYTES {
            return Err(KuramaError::Tool("response exceeds 2 MiB".into()));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

async fn validate_remote_url(url: &Url, mode: ExecutionMode) -> Result<(), KuramaError> {
    if mode == ExecutionMode::Yolo {
        return Ok(());
    }
    let host = url
        .host()
        .ok_or_else(|| KuramaError::Tool("URL host is missing".into()))?;
    match host {
        Host::Ipv4(address) => reject_private(IpAddr::V4(address)),
        Host::Ipv6(address) => reject_private(IpAddr::V6(address)),
        Host::Domain(domain) => {
            if domain.eq_ignore_ascii_case("localhost") || domain.ends_with(".localhost") {
                return Err(KuramaError::Policy("private network target denied".into()));
            }
            let port = url
                .port_or_known_default()
                .ok_or_else(|| KuramaError::Tool("URL has no resolvable port".into()))?;
            let addresses = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::lookup_host((domain, port)),
            )
            .await
            .map_err(|_| KuramaError::Tool("DNS resolution timed out".into()))?
            .map_err(|error| KuramaError::Tool(format!("DNS resolution failed: {error}")))?;
            let mut resolved = false;
            for address in addresses {
                resolved = true;
                reject_private(address.ip())?;
            }
            if !resolved {
                return Err(KuramaError::Tool(
                    "DNS resolution returned no addresses".into(),
                ));
            }
            Ok(())
        }
    }
}

fn literal_private_target(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => !is_public_ipv4(address),
        Some(Host::Ipv6(address)) => !is_public_ipv6(address),
        Some(Host::Domain(domain)) => {
            domain.eq_ignore_ascii_case("localhost") || domain.ends_with(".localhost")
        }
        None => true,
    }
}

fn reject_private(address: IpAddr) -> Result<(), KuramaError> {
    let public = match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    };
    if public {
        Ok(())
    } else {
        Err(KuramaError::Policy("private network target denied".into()))
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_multicast()
        && !address.is_unspecified()
        && address != Ipv4Addr::BROADCAST
        && octets[0] != 0
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        && !(octets[0] == 198 && matches!(octets[1], 18 | 19))
        && !(octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        && !(octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        && octets[0] < 240
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && address.to_ipv4_mapped().is_none_or(is_public_ipv4)
}

fn validate_results(
    results: Vec<SearchResult>,
    limit: usize,
) -> Result<Vec<SearchResult>, KuramaError> {
    if results.len() > limit || results.len() > 8 {
        return Err(KuramaError::Tool(
            "search backend returned more results than requested".into(),
        ));
    }
    for result in &results {
        if result.title.is_empty() || result.snippet.is_empty() {
            return Err(KuramaError::Tool(
                "search result omitted title or snippet".into(),
            ));
        }
        parse_http_url(&result.url)
            .map_err(|_| KuramaError::Tool("search result contained an invalid URL".into()))?;
    }
    Ok(results)
}

fn search_result_schema(limit: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "results": {
                "type": "array",
                "maxItems": limit,
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "url": {"type": "string"},
                        "snippet": {"type": "string"}
                    },
                    "required": ["title", "url", "snippet"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["results"],
        "additionalProperties": false
    })
}

fn response_output_text(value: &serde_json::Value) -> Option<&str> {
    value
        .get("output_text")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("output")?
                .as_array()?
                .iter()
                .flat_map(|item| {
                    item.get("content")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find_map(|content| content.get("text").and_then(serde_json::Value::as_str))
        })
}

fn append_endpoint(base: &Url, suffix: &str) -> Result<Url, KuramaError> {
    let mut value = base.as_str().trim_end_matches('/').to_owned();
    value.push('/');
    value.push_str(suffix);
    Url::parse(&value)
        .map_err(|error| KuramaError::Configuration(format!("invalid provider endpoint: {error}")))
}
