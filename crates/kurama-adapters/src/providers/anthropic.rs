use std::collections::BTreeMap;

use kurama_protocol::{
    KuramaError,
    id::CallId,
    model::{BackendCursor, FinishReason, ModelEvent, ModelRequest, Usage},
};
use serde_json::{Value, json};
use url::Url;
use zeroize::Zeroizing;

use crate::http::{HttpClient, bounded_redacted_error};

use super::{anthropic_messages, anthropic_tools, delegation_from_arguments, sse::SseDecoder};

use super::endpoint_url;

const PROVIDER: &str = "anthropic";

#[derive(Clone)]
pub struct AnthropicBackend {
    http: HttpClient,
    endpoint: Url,
    api_key: Zeroizing<String>,
}

impl AnthropicBackend {
    pub fn new(http: HttpClient, endpoint: Url, api_key: impl Into<String>) -> Self {
        Self {
            http,
            endpoint,
            api_key: Zeroizing::new(api_key.into()),
        }
    }

    pub fn from_endpoint(
        http: HttpClient,
        endpoint: &str,
        api_key: impl Into<String>,
    ) -> Result<Self, KuramaError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|error| KuramaError::Configuration(format!("Anthropic endpoint: {error}")))?;
        Ok(Self::new(http, endpoint, api_key))
    }

    pub fn request_body(request: &ModelRequest) -> Value {
        json!({
            "model": request.profile.model,
            "system": request.system,
            "messages": anthropic_messages(request),
            "tools": anthropic_tools(&request.tools, request.delegation.as_ref()),
            "max_tokens": request.profile.max_output_tokens,
            "stream": true
        })
    }

    pub fn parse_fixture(fixture: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut decoder = SseDecoder::default();
        let mut normalizer = AnthropicNormalizer::default();
        let mut events = Vec::new();
        for byte in fixture.as_bytes() {
            for event in decoder.push(&[*byte])? {
                events.extend(normalizer.push(&event.data)?);
            }
        }
        for event in decoder.finish()? {
            events.extend(normalizer.push(&event.data)?);
        }
        events.extend(normalizer.finish());
        Ok(events)
    }

    async fn collect(
        &self,
        request: &ModelRequest,
        cancel: &dyn kurama_protocol::traits::CancelSignal,
    ) -> Result<Vec<ModelEvent>, KuramaError> {
        if cancel.is_cancelled() {
            return Err(KuramaError::Cancelled);
        }
        let url = endpoint_url(&self.endpoint, "messages")?;
        let send = self
            .http
            .post(url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .json(&Self::request_body(request))
            .send();
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
            response = send => response.map_err(|error| HttpClient::transport_error(PROVIDER, &error, &[self.api_key.as_str()]).into_kurama())?,
        };
        if !response.status().is_success() {
            return Err(
                HttpClient::response_error(PROVIDER, response, &[self.api_key.as_str()])
                    .await
                    .into_kurama(),
            );
        }
        let mut decoder = SseDecoder::default();
        let mut normalizer = AnthropicNormalizer::default();
        let mut events = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
                chunk = response.chunk() => chunk.map_err(|error| HttpClient::transport_error(PROVIDER, &error, &[self.api_key.as_str()]).into_kurama())?,
            };
            let Some(chunk) = chunk else { break };
            for event in decoder.push(&chunk)? {
                events.extend(normalizer.push(&event.data)?);
            }
        }
        for event in decoder.finish()? {
            events.extend(normalizer.push(&event.data)?);
        }
        events.extend(normalizer.finish());
        Ok(events)
    }
}

impl kurama_protocol::traits::ModelBackend for AnthropicBackend {
    fn backend_name(&self) -> &'static str {
        PROVIDER
    }
    fn capabilities(&self) -> kurama_protocol::model::BackendCapabilities {
        kurama_protocol::model::BackendCapabilities::remote_default()
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn kurama_protocol::traits::CancelSignal,
    ) -> kurama_protocol::traits::BoxFuture<
        'a,
        Result<kurama_protocol::traits::ModelStream, KuramaError>,
    > {
        Box::pin(async move {
            let events = self
                .collect(&request, cancel)
                .await
                .map_err(|error| bounded_redacted_error(error, &[self.api_key.as_str()]))?;
            Ok(super::event_stream(events.into_iter().map(Ok).collect()))
        })
    }
}

#[derive(Default)]
struct AnthropicNormalizer {
    message_id: Option<String>,
    calls: BTreeMap<u64, PendingTool>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
    emitted_call: bool,
    completed: bool,
}

#[derive(Default)]
struct PendingTool {
    call_id: String,
    name: String,
    json: String,
}

impl AnthropicNormalizer {
    fn push(&mut self, payload: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let value: Value = serde_json::from_str(payload).map_err(|error| {
            KuramaError::Protocol(format!("invalid Anthropic SSE JSON: {error}"))
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            "message_start" => {
                let message = value.get("message").unwrap_or(&Value::Null);
                let id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic");
                self.message_id = Some(id.to_owned());
                self.input_tokens = message
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                self.cached_input_tokens = message
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                events.push(ModelEvent::ResponseStarted {
                    provider_id: id.to_owned(),
                });
            }
            "content_block_start" => {
                if value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
                {
                    let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
                        KuramaError::Protocol("Anthropic tool block omitted index".into())
                    })?;
                    let input = value
                        .pointer("/content_block/input")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    self.calls.insert(
                        index,
                        PendingTool {
                            call_id: value
                                .pointer("/content_block/id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            name: value
                                .pointer("/content_block/name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            json: if input.as_object().is_some_and(|object| object.is_empty()) {
                                String::new()
                            } else {
                                input.to_string()
                            },
                        },
                    );
                }
            }
            "content_block_delta" => match value.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => events.push(ModelEvent::TextDelta {
                    text: value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some("input_json_delta") => {
                    let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
                        KuramaError::Protocol("Anthropic JSON delta omitted index".into())
                    })?;
                    let call = self.calls.get_mut(&index).ok_or_else(|| {
                        KuramaError::Protocol(format!("Anthropic JSON for unknown block {index}"))
                    })?;
                    call.json.push_str(
                        value
                            .pointer("/delta/partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
                _ => {}
            },
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    KuramaError::Protocol("Anthropic block stop omitted index".into())
                })?;
                if let Some(call) = self.calls.remove(&index) {
                    let arguments = serde_json::from_str(if call.json.is_empty() {
                        "{}"
                    } else {
                        &call.json
                    })
                    .map_err(|error| {
                        KuramaError::Protocol(format!("invalid Anthropic tool arguments: {error}"))
                    })?;
                    self.emitted_call = true;
                    if call.name == "__kurama_delegate" {
                        events.push(ModelEvent::Delegation {
                            request: delegation_from_arguments(arguments)?,
                        });
                    } else {
                        events.push(ModelEvent::ToolCall {
                            call_id: CallId::from(call.call_id),
                            name: call.name,
                            arguments,
                        });
                    }
                }
            }
            "message_delta" => {
                self.output_tokens = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.output_tokens);
                self.stop_reason = value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                events.push(ModelEvent::Usage {
                    usage: Usage {
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        cached_input_tokens: self.cached_input_tokens,
                    },
                });
            }
            "message_stop" => {
                events.push(self.completion());
                self.completed = true;
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider error");
                return Err(KuramaError::Model(format!("Anthropic stream: {message}")));
            }
            _ => {}
        }
        Ok(events)
    }

    fn completion(&self) -> ModelEvent {
        let finish_reason = match self.stop_reason.as_deref() {
            Some("max_tokens") => FinishReason::Length,
            Some("tool_use") => FinishReason::ToolCalls,
            _ if self.emitted_call => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        };
        ModelEvent::ResponseCompleted {
            cursor: self.message_id.clone().map(|value| BackendCursor {
                backend: PROVIDER.into(),
                value,
            }),
            finish_reason,
        }
    }

    fn finish(&mut self) -> Vec<ModelEvent> {
        if self.completed {
            Vec::new()
        } else {
            self.completed = true;
            vec![self.completion()]
        }
    }
}
