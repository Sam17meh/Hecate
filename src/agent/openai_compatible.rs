//! OpenAI-compatible Chat Completions provider adapter.
//!
//! The adapter translates HECATE's typed conversation/tool model into the HTTP protocol. It never
//! dispatches tools; all requests still pass through `AgentRuntime` and `ToolRegistry`.

use crate::agent::provider::{
    ConversationMessage, MessageRole, ModelProvider, ModelRequest, ModelResponse, ProviderError,
    ProviderErrorKind, TokenUsage, ToolCallRequest,
};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use serde_json::{Map, Value, json};
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETRIES: u8 = 1;
const RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Provider-neutral configuration for a Chat Completions-compatible endpoint.
///
/// The API key is deliberately private and this type does not implement `Debug`, `Serialize`, or
/// `Clone`, preventing accidental logging or persistence of credentials.
pub struct OpenAiCompatibleConfig {
    api_key: String,
    base_url: Url,
    model: String,
    request_timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

impl OpenAiCompatibleConfig {
    pub fn new(
        api_key: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into().trim().to_string();
        if api_key.is_empty() {
            return Err(configuration_error("API key cannot be empty"));
        }
        if contains_url_secret(base_url, &api_key) {
            return Err(configuration_error(
                "Base URL must not contain the configured API key",
            ));
        }
        let model = model.into();
        if model.trim().is_empty() || model.len() > 256 {
            return Err(configuration_error(
                "Model name must be nonempty and at most 256 bytes",
            ));
        }
        let base_url = validate_base_url(base_url)?;
        Ok(Self {
            api_key,
            base_url,
            model,
            request_timeout: DEFAULT_TIMEOUT,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        })
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        let mut config = Self::from_values(
            std::env::var("HECATE_API_KEY").ok(),
            std::env::var("HECATE_BASE_URL").ok(),
            std::env::var("HECATE_MODEL").ok(),
        )?;

        if let Ok(value) = std::env::var("HECATE_PROVIDER_TIMEOUT_SECONDS") {
            let seconds = value
                .parse::<u64>()
                .map_err(|_| configuration_error("HECATE_PROVIDER_TIMEOUT_SECONDS is invalid"))?;
            config = config.with_request_timeout(Duration::from_secs(seconds))?;
        }
        if let Ok(value) = std::env::var("HECATE_MAX_PROVIDER_REQUEST_BYTES") {
            let bytes = value
                .parse::<usize>()
                .map_err(|_| configuration_error("HECATE_MAX_PROVIDER_REQUEST_BYTES is invalid"))?;
            config = config.with_max_request_bytes(bytes)?;
        }
        if let Ok(value) = std::env::var("HECATE_MAX_PROVIDER_RESPONSE_BYTES") {
            let bytes = value.parse::<usize>().map_err(|_| {
                configuration_error("HECATE_MAX_PROVIDER_RESPONSE_BYTES is invalid")
            })?;
            config = config.with_max_response_bytes(bytes)?;
        }
        Ok(config)
    }

    fn from_values(
        api_key: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.ok_or_else(|| configuration_error("HECATE_API_KEY is required"))?;
        let base_url =
            base_url.ok_or_else(|| configuration_error("HECATE_BASE_URL is required"))?;
        let model = model.ok_or_else(|| configuration_error("HECATE_MODEL is required"))?;
        Self::new(api_key, &base_url, model)
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self, ProviderError> {
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err(configuration_error(
                "Request timeout must be greater than zero and at most 120 seconds",
            ));
        }
        self.request_timeout = timeout;
        Ok(self)
    }

    pub fn with_max_response_bytes(mut self, bytes: usize) -> Result<Self, ProviderError> {
        if bytes == 0 || bytes > MAX_RESPONSE_BYTES {
            return Err(configuration_error(
                "Response limit must be between 1 byte and 16 MiB",
            ));
        }
        self.max_response_bytes = bytes;
        Ok(self)
    }

    pub fn with_max_request_bytes(mut self, bytes: usize) -> Result<Self, ProviderError> {
        if bytes == 0 || bytes > MAX_REQUEST_BYTES {
            return Err(configuration_error(
                "Request limit must be between 1 byte and 16 MiB",
            ));
        }
        self.max_request_bytes = bytes;
        Ok(self)
    }
}

/// Synchronous adapter for services implementing OpenAI Chat Completions.
pub struct OpenAiCompatibleProvider {
    client: Client,
    endpoint: Url,
    api_key: String,
    model: String,
    request_timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

impl OpenAiCompatibleProvider {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Configuration,
                    "Could not initialize provider HTTP client",
                )
            })?;
        let mut endpoint = config.base_url;
        endpoint
            .path_segments_mut()
            .map_err(|_| configuration_error("Base URL cannot be used as an API endpoint"))?
            .pop_if_empty()
            .push("chat")
            .push("completions");
        Ok(Self {
            client,
            endpoint,
            api_key: config.api_key,
            model: config.model,
            request_timeout: config.request_timeout,
            max_request_bytes: config.max_request_bytes,
            max_response_bytes: config.max_response_bytes,
        })
    }

    fn request_body(&self, request: &ModelRequest) -> Result<Value, ProviderError> {
        let messages = request
            .messages
            .iter()
            .map(|message| encode_message(message, self.max_request_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                if tool.name.trim().is_empty()
                    || tool.description.trim().is_empty()
                    || !tool.input_schema.is_object()
                {
                    return Err(invalid_request("Tool definition is invalid"));
                }
                Ok(json!({
                    "type":"function",
                    "function":{
                        "name":tool.name,
                        "description":tool.description,
                        "parameters":tool.input_schema
                    }
                }))
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;

        Ok(json!({
            "model":self.model,
            "messages":messages,
            "tools":tools,
            "tool_choice":"auto"
        }))
    }

    fn preflight_request_size(&self, request: &ModelRequest) -> Result<(), ProviderError> {
        let mut counter = SizeCounter {
            written: 0,
            maximum: self.max_request_bytes,
        };
        serde_json::to_writer(&mut counter, request)
            .map_err(|_| request_too_large(self.max_request_bytes))?;
        Ok(())
    }

    fn send(&self, body: Vec<u8>) -> Result<ModelResponse, ProviderError> {
        let deadline = Instant::now() + self.request_timeout;
        for attempt in 0..=MAX_RETRIES {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderError::new(
                    ProviderErrorKind::Timeout,
                    "Provider request exceeded its configured time budget",
                ));
            }
            let response = self
                .client
                .post(self.endpoint.clone())
                .bearer_auth(&self.api_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(remaining)
                .body(body.clone())
                .send()
                .map_err(|error| {
                    if error.is_timeout() {
                        ProviderError::new(ProviderErrorKind::Timeout, "Provider request timed out")
                    } else {
                        ProviderError::new(
                            ProviderErrorKind::Unavailable,
                            "Provider network request failed",
                        )
                    }
                })?;

            let status = response.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < MAX_RETRIES {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining > RETRY_BACKOFF {
                    thread::sleep(RETRY_BACKOFF);
                    continue;
                }
            }
            if !status.is_success() {
                return Err(status_error(status.as_u16()));
            }
            return parse_response(response, self.max_response_bytes, &self.api_key);
        }
        Err(ProviderError::new(
            ProviderErrorKind::Unavailable,
            "Provider request failed after the bounded retry",
        ))
    }
}

impl ModelProvider for OpenAiCompatibleProvider {
    fn complete(&mut self, mut request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        sanitize_request(self, &mut request);
        self.preflight_request_size(&request)?;
        let mut body = self.request_body(&request)?;
        redact_secret_values(&mut body, &self.api_key);
        let mut bounded = BoundedBuffer::new(self.max_request_bytes);
        serde_json::to_writer(&mut bounded, &body)
            .map_err(|_| request_too_large(self.max_request_bytes))?;
        self.send(bounded.bytes)
    }

    fn sanitize_untrusted_text(&self, text: &str) -> String {
        text.replace(&self.api_key, "[REDACTED PROVIDER CREDENTIAL]")
    }
}

fn sanitize_request(provider: &OpenAiCompatibleProvider, request: &mut ModelRequest) {
    for message in &mut request.messages {
        if let Some(content) = &mut message.content {
            *content = provider.sanitize_untrusted_text(content);
        }
        if let Some(call_id) = &mut message.tool_call_id {
            *call_id = provider.sanitize_untrusted_text(call_id);
        }
        for call in &mut message.tool_calls {
            call.id = provider.sanitize_untrusted_text(&call.id);
            call.name = provider.sanitize_untrusted_text(&call.name);
            provider.sanitize_untrusted_value(&mut call.arguments);
        }
    }
    for tool in &mut request.tools {
        tool.name = provider.sanitize_untrusted_text(&tool.name);
        tool.description = provider.sanitize_untrusted_text(&tool.description);
        provider.sanitize_untrusted_value(&mut tool.input_schema);
    }
}

struct SizeCounter {
    written: usize,
    maximum: usize,
}

impl Write for SizeCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.written.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "serialized size overflow")
        })?;
        if next > self.maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized request exceeds configured limit",
            ));
        }
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    maximum: usize,
}

impl BoundedBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(64 * 1024)),
            maximum,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "serialized size overflow")
        })?;
        if next > self.maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serialized request exceeds configured limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_message(
    message: &ConversationMessage,
    max_request_bytes: usize,
) -> Result<Value, ProviderError> {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    let mut encoded = Map::new();
    encoded.insert("role".into(), json!(role));
    encoded.insert("content".into(), json!(message.content));

    if message.role == MessageRole::Tool {
        let id = message
            .tool_call_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| invalid_request("Tool result is missing its tool-call ID"))?;
        encoded.insert("tool_call_id".into(), json!(id));
    } else if let Some(id) = &message.tool_call_id {
        return Err(invalid_request(if id.trim().is_empty() {
            "Message has an empty tool-call ID"
        } else {
            "Only tool-result messages may contain a tool-call ID"
        }));
    }

    if !message.tool_calls.is_empty() {
        if message.role != MessageRole::Assistant {
            return Err(invalid_request(
                "Only assistant messages may contain tool calls",
            ));
        }
        let calls = message
            .tool_calls
            .iter()
            .map(|call| {
                if call.id.trim().is_empty() || call.name.trim().is_empty() {
                    return Err(invalid_request(
                        "Assistant tool call is missing its ID or name",
                    ));
                }
                let mut bounded = BoundedBuffer::new(max_request_bytes);
                serde_json::to_writer(&mut bounded, &call.arguments)
                    .map_err(|_| request_too_large(max_request_bytes))?;
                let arguments = String::from_utf8(bounded.bytes)
                    .map_err(|_| invalid_request("Could not encode tool-call arguments"))?;
                Ok(json!({
                    "id":call.id,
                    "type":"function",
                    "function":{"name":call.name,"arguments":arguments}
                }))
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        encoded.insert("tool_calls".into(), json!(calls));
    }
    Ok(Value::Object(encoded))
}

fn parse_response(
    mut response: Response,
    max_bytes: usize,
    api_key: &str,
) -> Result<ModelResponse, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ProviderError::new(
            ProviderErrorKind::ResponseTooLarge,
            "Provider response exceeds the configured size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    response
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::ResponseFailure,
                "Could not read provider response",
            )
        })?;
    if bytes.len() > max_bytes {
        return Err(ProviderError::new(
            ProviderErrorKind::ResponseTooLarge,
            "Provider response exceeds the configured size limit",
        ));
    }
    let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::MalformedResponse,
            "Provider returned invalid JSON",
        )
    })?;
    redact_secret_values(&mut value, api_key);
    decode_response(value)
}

fn redact_secret_values(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = text.replace(secret, "[REDACTED PROVIDER CREDENTIAL]"),
        Value::Array(values) => {
            for value in values {
                redact_secret_values(value, secret);
            }
        }
        Value::Object(object) => {
            let old = std::mem::take(object);
            for (key, mut value) in old {
                redact_secret_values(&mut value, secret);
                let key = key.replace(secret, "[REDACTED PROVIDER CREDENTIAL]");
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

fn decode_response(value: Value) -> Result<ModelResponse, ProviderError> {
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| choices.len() == 1)
        .ok_or_else(|| malformed_response("Provider response must contain exactly one choice"))?;
    let choice = choices[0]
        .as_object()
        .ok_or_else(|| malformed_response("Provider choice has an invalid structure"))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed_response("Provider choice has no supported finish reason"))?;
    match finish_reason {
        "length" => {
            return Err(ProviderError::new(
                ProviderErrorKind::IncompleteResponse,
                "Provider response was truncated by its token limit",
            ));
        }
        "content_filter" => {
            return Err(ProviderError::new(
                ProviderErrorKind::ContentFiltered,
                "Provider filtered the response",
            ));
        }
        "stop" | "tool_calls" => {}
        _ => return Err(malformed_response("Provider finish reason is unsupported")),
    }
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed_response("Provider response has no assistant choice"))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(malformed_response(
            "Provider response choice is not an assistant message",
        ));
    }
    let content = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(content)) => Some(content.clone()),
        Some(_) => return Err(malformed_response("Assistant content has an invalid type")),
    };
    let tool_calls = match message.get("tool_calls") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(calls)) => calls
            .iter()
            .map(decode_tool_call)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(malformed_response(
                "Assistant tool calls have an invalid type",
            ));
        }
    };
    match finish_reason {
        "stop" if !tool_calls.is_empty() => {
            return Err(malformed_response(
                "Provider returned tool calls with a non-tool completion reason",
            ));
        }
        "tool_calls" if tool_calls.is_empty() => {
            return Err(malformed_response(
                "Provider declared tool completion without tool calls",
            ));
        }
        _ => {}
    }
    if tool_calls.is_empty()
        && content
            .as_deref()
            .is_none_or(|content| content.trim().is_empty())
    {
        return Err(malformed_response(
            "Provider returned an empty assistant response",
        ));
    }
    let usage = value
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(decode_usage)
        .transpose()?;
    Ok(ModelResponse {
        content,
        tool_calls,
        usage,
    })
}

fn decode_tool_call(value: &Value) -> Result<ToolCallRequest, ProviderError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| malformed_response("Tool call has no ID"))?;
    if value.get("type").and_then(Value::as_str) != Some("function") {
        return Err(malformed_response("Tool call type is not function"));
    }
    let function = value
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed_response("Tool call has no function object"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| malformed_response("Tool call has no function name"))?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed_response("Tool-call arguments are not a JSON string"))?;
    let arguments: Value = serde_json::from_str(arguments)
        .map_err(|_| malformed_response("Tool-call arguments contain invalid JSON"))?;
    if !arguments.is_object() {
        return Err(malformed_response(
            "Tool-call arguments must decode to a JSON object",
        ));
    }
    Ok(ToolCallRequest {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
    })
}

fn decode_usage(value: &Value) -> Result<TokenUsage, ProviderError> {
    let usage = value
        .as_object()
        .ok_or_else(|| malformed_response("Token usage has an invalid type"))?;
    let input_tokens = optional_usage_count(usage.get("prompt_tokens"))?;
    let output_tokens = optional_usage_count(usage.get("completion_tokens"))?;
    Ok(TokenUsage {
        input_tokens,
        output_tokens,
    })
}

fn optional_usage_count(value: Option<&Value>) -> Result<u64, ProviderError> {
    match value {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| malformed_response("Token usage count is not a nonnegative integer")),
    }
}

fn validate_base_url(value: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(value).map_err(|_| configuration_error("HECATE_BASE_URL is invalid"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(configuration_error(
            "Base URL must be an HTTP(S) origin/path without credentials, query, or fragment",
        ));
    }
    if url.scheme() == "http" && !is_loopback(url.host_str().unwrap_or_default()) {
        return Err(configuration_error(
            "Non-loopback provider URLs must use HTTPS",
        ));
    }
    Ok(url)
}

fn contains_url_secret(url: &str, secret: &str) -> bool {
    if url.contains(secret) {
        return true;
    }
    let bytes = url.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    decoded
        .windows(secret.len())
        .any(|window| window == secret.as_bytes())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn status_error(status: u16) -> ProviderError {
    let (kind, message) = match status {
        401 | 403 => (
            ProviderErrorKind::Authentication,
            "Provider rejected authentication or access",
        ),
        429 => (
            ProviderErrorKind::RateLimited,
            "Provider rate limit reached",
        ),
        408 => (ProviderErrorKind::Timeout, "Provider request timed out"),
        500..=599 => (
            ProviderErrorKind::ServerFailure,
            "Provider server returned an error",
        ),
        _ => (
            ProviderErrorKind::InvalidRequest,
            "Provider rejected the request",
        ),
    };
    ProviderError::new(kind, message)
}

fn configuration_error(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Configuration, message)
}

fn request_too_large(maximum: usize) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::RequestTooLarge,
        format!("Provider request exceeds the configured {maximum}-byte limit"),
    )
}

fn invalid_request(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn malformed_response(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedResponse, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provider::ToolDefinition;
    use crate::agent::provider_test_support::{MockHttpResponse, MockHttpServer};

    const TEST_KEY: &str = "test-only-secret-key-never-return-this";

    fn provider(base_url: &str) -> OpenAiCompatibleProvider {
        let config = OpenAiCompatibleConfig::new(TEST_KEY, base_url, "local-test-model").unwrap();
        OpenAiCompatibleProvider::new(config).unwrap()
    }

    fn model_request() -> ModelRequest {
        ModelRequest {
            messages: vec![
                ConversationMessage::text(MessageRole::System, "Follow the approved plan."),
                ConversationMessage::text(MessageRole::User, "Inspect the repository."),
                ConversationMessage {
                    role: MessageRole::Assistant,
                    content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "call-7".into(),
                        name: "list_files".into(),
                        arguments: json!({"limit":5}),
                    }],
                    tool_call_id: None,
                },
                ConversationMessage::tool_result(
                    "call-7",
                    "{\"files\":[{\"path\":\"src/main.rs\"}]}".into(),
                ),
            ],
            tools: vec![ToolDefinition {
                name: "list_files".into(),
                description: "List workspace files".into(),
                input_schema: json!({
                    "type":"object",
                    "properties":{"limit":{"type":"integer"}},
                    "additionalProperties":false
                }),
            }],
        }
    }

    fn chat_response(message: Value, usage: Option<Value>) -> String {
        let finish_reason = if message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
        {
            "tool_calls"
        } else {
            "stop"
        };
        let mut body = json!({"choices":[{"message":message,"finish_reason":finish_reason}]});
        if let Some(usage) = usage {
            body["usage"] = usage;
        }
        body.to_string()
    }

    #[test]
    fn serializes_model_messages_tool_definitions_and_tool_call_history() {
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            200,
            chat_response(
                json!({"role":"assistant","content":"Files inspected."}),
                Some(json!({"prompt_tokens":19,"completion_tokens":7})),
            ),
        )]);
        let mut provider = provider(&server.base_url);
        let response = provider.complete(model_request()).unwrap();
        assert_eq!(response.content.as_deref(), Some("Files inspected."));
        assert_eq!(response.usage.unwrap().input_tokens, 19);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = String::from_utf8(requests[0].clone()).unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        let headers = request
            .split("\r\n\r\n")
            .next()
            .unwrap()
            .to_ascii_lowercase();
        assert!(headers.contains(&format!("authorization: bearer {TEST_KEY}")));
        let body_start = request.find("\r\n\r\n").unwrap() + 4;
        let body: Value = serde_json::from_str(&request[body_start..]).unwrap();
        assert_eq!(body["model"], "local-test-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "Inspect the repository.");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-7");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"limit\":5}"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "call-7");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "list_files");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["additionalProperties"],
            false
        );
        assert!(!body.to_string().contains(TEST_KEY));
        server.finish();
    }

    #[test]
    fn decodes_text_multiple_tool_calls_and_token_usage() {
        let calls = json!([
            {"id":"call-a","type":"function","function":{"name":"list_files","arguments":"{\"limit\":2}"}},
            {"id":"call-b","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}
        ]);
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            200,
            chat_response(
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                Some(json!({"prompt_tokens":23,"completion_tokens":11})),
            ),
        )]);
        let mut provider = provider(&server.base_url);
        let response = provider.complete(model_request()).unwrap();
        assert_eq!(response.content, None);
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "call-a");
        assert_eq!(response.tool_calls[0].arguments, json!({"limit":2}));
        assert_eq!(response.tool_calls[1].id, "call-b");
        assert_eq!(response.tool_calls[1].name, "read_file");
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 23);
        assert_eq!(usage.output_tokens, 11);
        server.finish();
    }

    #[test]
    fn maps_auth_rate_limit_and_server_errors_without_leaking_credentials() {
        for (status, expected, count) in [
            (401, ProviderErrorKind::Authentication, 1),
            (429, ProviderErrorKind::RateLimited, 2),
            (503, ProviderErrorKind::ServerFailure, 2),
        ] {
            let responses = (0..count)
                .map(|_| MockHttpResponse::json(status, TEST_KEY))
                .collect();
            let server = MockHttpServer::start(responses);
            let mut provider = provider(&server.base_url);
            let error = provider.complete(model_request()).unwrap_err();
            assert_eq!(error.kind, expected);
            assert!(!format!("{error:?}").contains(TEST_KEY));
            assert!(!error.message.contains(TEST_KEY));
            assert_eq!(server.requests().len(), count as usize);
            server.finish();
        }
    }

    #[test]
    fn reports_invalid_json_unexpected_empty_and_malformed_tool_responses() {
        let responses = [
            "not json",
            "{\"choices\":[]}",
            "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\" \"}}]}",
            "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"x\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"not-json\"}}]}}]}",
        ];
        for body in responses {
            let server = MockHttpServer::start(vec![MockHttpResponse::json(200, body)]);
            let mut provider = provider(&server.base_url);
            assert_eq!(
                provider.complete(model_request()).unwrap_err().kind,
                ProviderErrorKind::MalformedResponse
            );
            server.finish();
        }
    }

    #[test]
    fn validates_completion_reasons_and_never_dispatches_truncated_tool_calls() {
        for (message, finish_reason, expected) in [
            (
                json!({"role":"assistant","content":"partial answer"}),
                Some("length"),
                ProviderErrorKind::IncompleteResponse,
            ),
            (
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"list_files","arguments":"{}"}}]}),
                Some("length"),
                ProviderErrorKind::IncompleteResponse,
            ),
            (
                json!({"role":"assistant","content":null}),
                Some("content_filter"),
                ProviderErrorKind::ContentFiltered,
            ),
            (
                json!({"role":"assistant","content":"answer"}),
                None,
                ProviderErrorKind::MalformedResponse,
            ),
            (
                json!({"role":"assistant","content":"answer"}),
                Some("unexpected"),
                ProviderErrorKind::MalformedResponse,
            ),
        ] {
            let mut choice = json!({"message":message});
            if let Some(finish_reason) = finish_reason {
                choice["finish_reason"] = json!(finish_reason);
            }
            let server = MockHttpServer::start(vec![MockHttpResponse::json(
                200,
                json!({"choices":[choice]}).to_string(),
            )]);
            let mut provider = provider(&server.base_url);
            assert_eq!(
                provider.complete(model_request()).unwrap_err().kind,
                expected
            );
            server.finish();
        }
    }

    #[test]
    fn requires_tool_calls_finish_reason_for_structured_calls() {
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            200,
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"list_files","arguments":"{}"}}]}}]}).to_string(),
        )]);
        let mut provider = provider(&server.base_url);
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::MalformedResponse
        );
        server.finish();
    }

    #[test]
    fn bounds_response_size_and_request_timeout() {
        let server = MockHttpServer::start(vec![
            MockHttpResponse::json(200, "{}".repeat(20)).with_declared_length(4096),
        ]);
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "local-test-model")
            .unwrap()
            .with_max_response_bytes(32)
            .unwrap();
        let mut provider = OpenAiCompatibleProvider::new(config).unwrap();
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::ResponseTooLarge
        );
        server.finish();

        let server = MockHttpServer::start(vec![
            MockHttpResponse::json(200, "x".repeat(128)).without_content_length(),
        ]);
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "local-test-model")
            .unwrap()
            .with_max_response_bytes(32)
            .unwrap();
        let mut provider = OpenAiCompatibleProvider::new(config).unwrap();
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::ResponseTooLarge
        );
        server.finish();

        let server = MockHttpServer::start(vec![
            MockHttpResponse::json(200, "{}").delayed(Duration::from_millis(150)),
        ]);
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "local-test-model")
            .unwrap()
            .with_request_timeout(Duration::from_millis(30))
            .unwrap();
        let mut provider = OpenAiCompatibleProvider::new(config).unwrap();
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::Timeout
        );
        server.finish();
    }

    #[test]
    fn rejects_oversized_requests_before_opening_a_connection() {
        let server = MockHttpServer::start(Vec::new());
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "local-test-model")
            .unwrap()
            .with_max_request_bytes(128)
            .unwrap();
        let mut provider = OpenAiCompatibleProvider::new(config).unwrap();
        let error = provider.complete(model_request()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::RequestTooLarge);
        assert!(server.requests().is_empty());
        server.finish();
    }

    #[test]
    fn enforces_limit_against_final_escaped_wire_body() {
        let mut request = ModelRequest {
            messages: vec![ConversationMessage {
                role: MessageRole::Assistant,
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "escape-test".into(),
                    name: "search_code".into(),
                    arguments: json!({"query": "\"".repeat(256)}),
                }],
                tool_call_id: None,
            }],
            tools: Vec::new(),
        };
        let generous =
            OpenAiCompatibleConfig::new(TEST_KEY, "http://127.0.0.1:9/v1", "model").unwrap();
        let provider = OpenAiCompatibleProvider::new(generous).unwrap();
        sanitize_request(&provider, &mut request);
        let mut source_size = SizeCounter {
            written: 0,
            maximum: usize::MAX,
        };
        serde_json::to_writer(&mut source_size, &request).unwrap();
        let body = provider.request_body(&request).unwrap();
        let final_size = serde_json::to_vec(&body).unwrap().len();
        assert!(final_size > source_size.written);
        let limit = (source_size.written + final_size) / 2;

        let server = MockHttpServer::start(Vec::new());
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "model")
            .unwrap()
            .with_max_request_bytes(limit)
            .unwrap();
        let mut limited_provider = OpenAiCompatibleProvider::new(config).unwrap();
        assert_eq!(
            limited_provider.complete(request).unwrap_err().kind,
            ProviderErrorKind::RequestTooLarge
        );
        assert!(server.requests().is_empty());
        server.finish();
    }

    #[test]
    fn redirect_response_does_not_send_credentials_to_another_endpoint() {
        let redirected_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        redirected_listener.set_nonblocking(true).unwrap();
        let redirect_url = format!(
            "http://{}/capture",
            redirected_listener.local_addr().unwrap()
        );
        let server = MockHttpServer::start(vec![
            MockHttpResponse::json(302, "redirect").redirect_to(redirect_url),
        ]);
        let mut provider = provider(&server.base_url);
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
        assert_eq!(server.requests().len(), 1);
        assert!(matches!(
            redirected_listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        server.finish();
    }

    #[test]
    fn does_not_retry_when_remaining_time_is_below_backoff() {
        let server = MockHttpServer::start(vec![
            MockHttpResponse::json(429, "rate limited").delayed(Duration::from_millis(700)),
        ]);
        let config = OpenAiCompatibleConfig::new(TEST_KEY, &server.base_url, "local-test-model")
            .unwrap()
            .with_request_timeout(Duration::from_millis(800))
            .unwrap();
        let mut provider = OpenAiCompatibleProvider::new(config).unwrap();
        assert_eq!(
            provider.complete(model_request()).unwrap_err().kind,
            ProviderErrorKind::RateLimited
        );
        assert_eq!(server.requests().len(), 1);
        server.finish();
    }

    #[test]
    fn validates_configuration_and_requires_https_for_remote_hosts() {
        assert_eq!(
            OpenAiCompatibleConfig::new("", "https://provider.example/v1", "model")
                .err()
                .unwrap()
                .kind,
            ProviderErrorKind::Configuration
        );
        assert_eq!(
            OpenAiCompatibleConfig::new(TEST_KEY, "https://provider.example/v1", "")
                .err()
                .unwrap()
                .kind,
            ProviderErrorKind::Configuration
        );
        assert_eq!(
            OpenAiCompatibleConfig::new(TEST_KEY, "http://provider.example/v1", "model")
                .err()
                .unwrap()
                .kind,
            ProviderErrorKind::Configuration
        );
        assert_eq!(
            OpenAiCompatibleConfig::new(
                TEST_KEY,
                "https://user:password@provider.example/v1?token=secret",
                "model"
            )
            .err()
            .unwrap()
            .kind,
            ProviderErrorKind::Configuration
        );
        let encoded_key: String = TEST_KEY
            .as_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        assert_eq!(
            OpenAiCompatibleConfig::new(
                TEST_KEY,
                &format!("https://provider.example/{encoded_key}/v1"),
                "model"
            )
            .err()
            .unwrap()
            .kind,
            ProviderErrorKind::Configuration
        );
        assert_eq!(
            OpenAiCompatibleConfig::new(
                TEST_KEY,
                &format!("https://provider.example/{TEST_KEY}/v1"),
                "model"
            )
            .err()
            .unwrap()
            .kind,
            ProviderErrorKind::Configuration
        );
        assert_eq!(
            OpenAiCompatibleConfig::from_values(None, None, None)
                .err()
                .unwrap()
                .message,
            "HECATE_API_KEY is required"
        );
        assert_eq!(
            OpenAiCompatibleConfig::from_values(
                Some(TEST_KEY.into()),
                Some("https://provider.example/v1".into()),
                None
            )
            .err()
            .unwrap()
            .message,
            "HECATE_MODEL is required"
        );
    }

    #[test]
    fn removes_api_key_from_assistant_text_and_tool_arguments() {
        let tool_arguments = json!({"query":format!("find {TEST_KEY}")});
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            200,
            chat_response(
                json!({
                    "role":"assistant",
                    "content":format!("Echoed credential: {TEST_KEY}"),
                    "tool_calls":[{
                        "id":"safe-id",
                        "type":"function",
                        "function":{"name":"search_code","arguments":tool_arguments.to_string()}
                    }]
                }),
                None,
            ),
        )]);
        let mut provider = provider(&server.base_url);
        let response = provider.complete(model_request()).unwrap();
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(!serialized.contains(TEST_KEY));
        assert!(
            response
                .content
                .as_deref()
                .unwrap()
                .contains("[REDACTED PROVIDER CREDENTIAL]")
        );
        assert!(
            response.tool_calls[0].arguments["query"]
                .as_str()
                .unwrap()
                .contains("[REDACTED PROVIDER CREDENTIAL]")
        );
        server.finish();
    }

    #[test]
    fn sanitizes_exact_key_from_direct_request_messages_without_redacting_prefixes() {
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            200,
            chat_response(json!({"role":"assistant","content":"done"}), None),
        )]);
        let mut provider = provider(&server.base_url);
        let mut request = model_request();
        request.messages[1].content = Some(format!(
            "value {TEST_KEY}; partial {} remains",
            &TEST_KEY[..8]
        ));
        provider.complete(request).unwrap();
        let captured = server.requests();
        let wire = String::from_utf8(captured[0].clone()).unwrap();
        let body = wire.split_once("\r\n\r\n").unwrap().1;
        assert!(!body.contains(TEST_KEY));
        assert!(body.contains(&TEST_KEY[..8]));
        server.finish();
    }

    #[test]
    fn reports_connection_failure_without_exposing_request_data() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut provider = provider(&format!("http://{address}/v1/"));
        let error = provider.complete(model_request()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unavailable);
        assert!(!format!("{error:?}").contains(TEST_KEY));
    }
}
