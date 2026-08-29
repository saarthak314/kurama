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

use super::{chat_messages, chat_tools, delegation_from_arguments, sse::SseDecoder};

use super::endpoint_url;

const PROVIDER: &str = "openai_compatible";

#[derive(Clone)]
pub struct OpenAiCompatBackend {
    http: HttpClient,
    endpoint: Url,
    api_key: Option<Zeroizing<String>>,
    parallel_tool_calls: Option<bool>,
}

impl OpenAiCompatBackend {
    pub fn new(http: HttpClient, endpoint: Url, api_key: Option<String>) -> Self {
        Self {
            http,
            endpoint,
            api_key: api_key.map(Zeroizing::new),
            parallel_tool_calls: None,
        }
    }

    pub fn from_endpoint(
        http: HttpClient,
        endpoint: &str,
        api_key: Option<String>,
    ) -> Result<Self, KuramaError> {
        let endpoint = Url::parse(endpoint).map_err(|error| {
            KuramaError::Configuration(format!("OpenAI-compatible endpoint: {error}"))
        })?;
        Ok(Self::new(http, endpoint, api_key))
    }

    pub fn with_parallel_tool_calls(mut self, enabled: Option<bool>) -> Self {
        self.parallel_tool_calls = enabled;
        self
    }

    pub fn request_body(request: &ModelRequest, parallel_tool_calls: Option<bool>) -> Value {
        let mut body = json!({
            "model": request.profile.model,
            "messages": chat_messages(request),
            "tools": chat_tools(&request.tools, request.delegation.as_ref()),
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if let Some(enabled) = parallel_tool_calls {
            body["parallel_tool_calls"] = Value::Bool(enabled);
        }
        body
    }

    pub fn parse_fixture(fixture: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut decoder = SseDecoder::default();
        let mut normalizer = CompatNormalizer::default();
        let mut events = Vec::new();
        for byte in fixture.as_bytes() {
            for event in decoder.push(&[*byte])? {
                events.extend(normalizer.push(&event.data)?);
            }
        }
        for event in decoder.finish()? {
            events.extend(normalizer.push(&event.data)?);
        }
        events.extend(normalizer.finish()?);
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
        let url = endpoint_url(&self.endpoint, "chat/completions")?;
        let mut builder = self
            .http
            .post(url)
            .json(&Self::request_body(request, self.parallel_tool_calls));
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key.as_str());
        }
        let secrets = self
            .api_key
            .as_ref()
            .map(|key| key.as_str())
            .into_iter()
            .collect::<Vec<_>>();
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
            response = builder.send() => response.map_err(|error| HttpClient::transport_error(PROVIDER, &error, &secrets).into_kurama())?,
        };
        if !response.status().is_success() {
            return Err(HttpClient::response_error(PROVIDER, response, &secrets)
                .await
                .into_kurama());
        }
        let mut decoder = SseDecoder::default();
        let mut normalizer = CompatNormalizer::default();
        let mut events = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
                chunk = response.chunk() => chunk.map_err(|error| HttpClient::transport_error(PROVIDER, &error, &secrets).into_kurama())?,
            };
            let Some(chunk) = chunk else { break };
            for event in decoder.push(&chunk)? {
                events.extend(normalizer.push(&event.data)?);
            }
        }
        for event in decoder.finish()? {
            events.extend(normalizer.push(&event.data)?);
        }
        events.extend(normalizer.finish()?);
        Ok(events)
    }
}

impl kurama_protocol::traits::ModelBackend for OpenAiCompatBackend {
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
            let secrets = self
                .api_key
                .as_ref()
                .map(|key| key.as_str())
                .into_iter()
                .collect::<Vec<_>>();
            let events = self
                .collect(&request, cancel)
                .await
                .map_err(|error| bounded_redacted_error(error, &secrets))?;
            Ok(super::event_stream(events.into_iter().map(Ok).collect()))
        })
    }
}

#[derive(Default)]
struct CompatNormalizer {
    response_id: Option<String>,
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<FinishReason>,
    started: bool,
    completed: bool,
    saw_choices: bool,
}

#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl CompatNormalizer {
    fn push(&mut self, payload: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        if payload.trim() == "[DONE]" {
            self.completed = true;
            return self.finish_events();
        }
        let value: Value = serde_json::from_str(payload).map_err(|error| {
            KuramaError::Protocol(format!("invalid Chat Completions SSE JSON: {error}"))
        })?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            return Err(KuramaError::Model(format!(
                "OpenAI-compatible stream: {message}"
            )));
        }
        let mut events = Vec::new();
        if !self.started {
            let id = value.get("id").and_then(Value::as_str).unwrap_or(PROVIDER);
            self.response_id = Some(id.to_owned());
            self.started = true;
            events.push(ModelEvent::ResponseStarted {
                provider_id: id.to_owned(),
            });
        }
        if let Some(usage) = value.get("usage") {
            events.push(ModelEvent::Usage {
                usage: Usage {
                    input_tokens: usage
                        .get("prompt_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    output_tokens: usage
                        .get("completion_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    cached_input_tokens: usage
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                },
            });
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            if !choices.is_empty() {
                self.saw_choices = true;
            }
            for choice in choices {
                if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                    events.push(ModelEvent::TextDelta {
                        text: text.to_owned(),
                    });
                }
                if let Some(tool_calls) = choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                {
                    for tool_call in tool_calls {
                        let index =
                            tool_call
                                .get("index")
                                .and_then(Value::as_u64)
                                .ok_or_else(|| {
                                    KuramaError::Protocol("Chat tool delta omitted index".into())
                                })?;
                        let call = self.calls.entry(index).or_default();
                        if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                            call.call_id = id.to_owned();
                        }
                        if let Some(name) =
                            tool_call.pointer("/function/name").and_then(Value::as_str)
                        {
                            call.name = name.to_owned();
                        }
                        if let Some(arguments) = tool_call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                        {
                            call.arguments.push_str(arguments);
                        }
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    self.finish_reason = Some(match reason {
                        "tool_calls" | "function_call" => FinishReason::ToolCalls,
                        "length" => FinishReason::Length,
                        _ => FinishReason::Stop,
                    });
                    if self.finish_reason == Some(FinishReason::ToolCalls) {
                        events.extend(self.drain_calls()?);
                    }
                }
            }
        }
        Ok(events)
    }

    fn drain_calls(&mut self) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut events = Vec::new();
        for (_, call) in std::mem::take(&mut self.calls) {
            if call.call_id.is_empty() || call.name.is_empty() {
                return Err(KuramaError::Protocol(
                    "Chat tool call omitted id or name".into(),
                ));
            }
            let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
                KuramaError::Protocol(format!("invalid Chat tool arguments: {error}"))
            })?;
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
        Ok(events)
    }

    fn finish_events(&mut self) -> Result<Vec<ModelEvent>, KuramaError> {
        if !self.saw_choices {
            return Err(KuramaError::Model(
                "OpenAI-compatible endpoint lacks required streaming field: choices".into(),
            ));
        }
        let mut events = self.drain_calls()?;
        events.push(ModelEvent::ResponseCompleted {
            cursor: self.response_id.clone().map(|value| BackendCursor {
                backend: PROVIDER.into(),
                value,
            }),
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
        });
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<ModelEvent>, KuramaError> {
        if self.completed {
            Ok(Vec::new())
        } else {
            self.completed = true;
            self.finish_events()
        }
    }
}
