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

use super::{delegation_from_arguments, responses_input, responses_tools, sse::SseDecoder};

use super::endpoint_url;

const PROVIDER: &str = "openai";

#[derive(Clone)]
pub struct OpenAiBackend {
    http: HttpClient,
    endpoint: Url,
    api_key: Zeroizing<String>,
}

impl OpenAiBackend {
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
            .map_err(|error| KuramaError::Configuration(format!("OpenAI endpoint: {error}")))?;
        Ok(Self::new(http, endpoint, api_key))
    }

    pub fn request_body(request: &ModelRequest) -> Value {
        let mut body = json!({
            "model": request.profile.model,
            "instructions": request.system,
            "input": responses_input(request),
            "tools": responses_tools(&request.tools, request.delegation.as_ref()),
            "parallel_tool_calls": true,
            "max_output_tokens": request.profile.max_output_tokens,
            "stream": true,
            "store": false
        });
        if let Some(cursor) = request
            .continuation
            .as_ref()
            .filter(|cursor| cursor.backend == PROVIDER)
        {
            body["previous_response_id"] = Value::String(cursor.value.clone());
        }
        body
    }

    pub fn parse_fixture(fixture: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut decoder = SseDecoder::default();
        let mut normalizer = OpenAiNormalizer::default();
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
        let url = endpoint_url(&self.endpoint, "responses")?;
        let send = self
            .http
            .post(url)
            .bearer_auth(self.api_key.as_str())
            .json(&Self::request_body(request))
            .send();
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
            response = send => response.map_err(|error| {
                HttpClient::transport_error(PROVIDER, &error, &[self.api_key.as_str()]).into_kurama()
            })?,
        };
        if !response.status().is_success() {
            return Err(
                HttpClient::response_error(PROVIDER, response, &[self.api_key.as_str()])
                    .await
                    .into_kurama(),
            );
        }

        let mut decoder = SseDecoder::default();
        let mut normalizer = OpenAiNormalizer::default();
        let mut events = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(KuramaError::Cancelled),
                chunk = response.chunk() => chunk.map_err(|error| {
                    HttpClient::transport_error(PROVIDER, &error, &[self.api_key.as_str()]).into_kurama()
                })?,
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

impl kurama_protocol::traits::ModelBackend for OpenAiBackend {
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
struct OpenAiNormalizer {
    response_id: Option<String>,
    calls: BTreeMap<String, PendingCall>,
    emitted_call: bool,
    completed: bool,
}

#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl OpenAiNormalizer {
    fn push(&mut self, payload: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        if payload.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| KuramaError::Protocol(format!("invalid OpenAI SSE JSON: {error}")))?;
        let event_type = string(&value, "type")?;
        let mut events = Vec::new();
        match event_type {
            "response.created" => {
                let id = value
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .unwrap_or("openai");
                self.response_id = Some(id.to_owned());
                events.push(ModelEvent::ResponseStarted {
                    provider_id: id.to_owned(),
                });
            }
            "response.output_text.delta" => {
                events.push(ModelEvent::TextDelta {
                    text: string(&value, "delta")?.to_owned(),
                });
            }
            "response.output_item.added" => {
                if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item_id = value
                        .pointer("/item/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            KuramaError::Protocol("OpenAI function call omitted item.id".into())
                        })?;
                    self.calls.insert(
                        item_id.to_owned(),
                        PendingCall {
                            call_id: value
                                .pointer("/item/call_id")
                                .and_then(Value::as_str)
                                .unwrap_or(item_id)
                                .to_owned(),
                            name: value
                                .pointer("/item/name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            arguments: value
                                .pointer("/item/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = string(&value, "item_id")?;
                let call = self.calls.get_mut(item_id).ok_or_else(|| {
                    KuramaError::Protocol(format!("OpenAI arguments for unknown item {item_id}"))
                })?;
                call.arguments.push_str(string(&value, "delta")?);
            }
            "response.function_call_arguments.done" => {
                let item_id = string(&value, "item_id")?;
                let mut call = self.calls.remove(item_id).ok_or_else(|| {
                    KuramaError::Protocol(format!("OpenAI completion for unknown item {item_id}"))
                })?;
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    call.arguments = arguments.to_owned();
                }
                if let Some(item) = value.get("item") {
                    call.call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(&call.call_id)
                        .to_owned();
                    call.name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&call.name)
                        .to_owned();
                    call.arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or(&call.arguments)
                        .to_owned();
                }
                let arguments = parse_arguments(&call.arguments, "OpenAI")?;
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
            "response.completed" => {
                let response = value.get("response").unwrap_or(&Value::Null);
                if let Some(usage) = response.get("usage") {
                    events.push(ModelEvent::Usage {
                        usage: Usage {
                            input_tokens: number(usage, "input_tokens"),
                            output_tokens: number(usage, "output_tokens"),
                            cached_input_tokens: usage
                                .pointer("/input_tokens_details/cached_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                        },
                    });
                }
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.response_id.clone());
                let length = response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.contains("max_output"));
                events.push(ModelEvent::ResponseCompleted {
                    cursor: id.map(|value| BackendCursor {
                        backend: PROVIDER.into(),
                        value,
                    }),
                    finish_reason: if length {
                        FinishReason::Length
                    } else if self.emitted_call {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                });
                self.completed = true;
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("unknown provider error");
                return Err(KuramaError::Model(format!("OpenAI stream: {message}")));
            }
            _ => {}
        }
        Ok(events)
    }

    fn finish(&mut self) -> Vec<ModelEvent> {
        if self.completed {
            Vec::new()
        } else {
            self.completed = true;
            vec![ModelEvent::ResponseCompleted {
                cursor: self.response_id.clone().map(|value| BackendCursor {
                    backend: PROVIDER.into(),
                    value,
                }),
                finish_reason: if self.emitted_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            }]
        }
    }
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, KuramaError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| KuramaError::Protocol(format!("OpenAI event omitted string field {field}")))
}

fn number(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

fn parse_arguments(arguments: &str, provider: &str) -> Result<Value, KuramaError> {
    serde_json::from_str(arguments).map_err(|error| {
        KuramaError::Protocol(format!("invalid {provider} tool arguments: {error}"))
    })
}
