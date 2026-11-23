use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    pub tool_call_id: Option<String>,
}

impl ConversationMessage {
    pub fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: String) -> Self {
        Self {
            role: MessageRole::Tool,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    pub usage: Option<TokenUsage>,
}

impl ModelResponse {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    pub fn with_tool_calls(tool_calls: Vec<ToolCallRequest>) -> Self {
        Self {
            content: None,
            tool_calls,
            usage: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Configuration,
    Authentication,
    RateLimited,
    Timeout,
    Unavailable,
    InvalidRequest,
    ServerFailure,
    RequestTooLarge,
    ResponseTooLarge,
    MalformedResponse,
    IncompleteResponse,
    ContentFiltered,
    ResponseFailure,
    MockExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

pub trait ModelProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError>;

    /// Sanitize untrusted text before it is retained in a session or sent to a provider.
    /// Providers without provider-specific secrets may keep the default identity behavior.
    fn sanitize_untrusted_text(&self, text: &str) -> String {
        text.to_string()
    }

    /// Sanitize strings inside untrusted structured content using this provider's text policy.
    fn sanitize_untrusted_value(&self, value: &mut Value) {
        fn visit<P: ModelProvider + ?Sized>(provider: &P, value: &mut Value) {
            match value {
                Value::String(text) => *text = provider.sanitize_untrusted_text(text),
                Value::Array(values) => {
                    for value in values {
                        visit(provider, value);
                    }
                }
                Value::Object(object) => {
                    let old = std::mem::take(object);
                    for (key, mut value) in old {
                        visit(provider, &mut value);
                        object.insert(provider.sanitize_untrusted_text(&key), value);
                    }
                }
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
            }
        }

        visit(self, value);
    }
}

/// Deterministic scripted provider for tests and runtime demonstrations.
#[derive(Debug, Default)]
pub struct MockProvider {
    responses: VecDeque<Result<ModelResponse, ProviderError>>,
    requests: Vec<ModelRequest>,
}

impl MockProvider {
    pub fn new(responses: impl IntoIterator<Item = Result<ModelResponse, ProviderError>>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            requests: Vec::new(),
        }
    }

    pub fn from_responses(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self::new(responses.into_iter().map(Ok))
    }

    pub fn requests(&self) -> &[ModelRequest] {
        &self.requests
    }
}

impl ModelProvider for MockProvider {
    fn complete(&mut self, request: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests.push(request);
        self.responses.pop_front().unwrap_or_else(|| {
            Err(ProviderError::new(
                ProviderErrorKind::MockExhausted,
                "Mock provider has no scripted response remaining",
            ))
        })
    }
}
