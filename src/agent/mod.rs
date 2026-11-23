//! HECATE-owned agent loop and capability boundaries.
//!
//! This foundation has no real model client and registers no production tools. Model providers
//! return typed responses; requested tools can only resolve through the explicit registry and
//! permission policy. Unknown tools and non-read-only permissions are denied by default.

pub mod provider;
pub mod runtime;
pub mod session;
pub mod tool;

pub use provider::{
    ConversationMessage, MessageRole, MockProvider, ModelProvider, ModelRequest, ModelResponse,
    ProviderError, ProviderErrorKind, TokenUsage, ToolCallRequest, ToolDefinition,
};
pub use runtime::{AgentRuntime, CancellationToken, ExecutionLimits, RuntimeError};
pub use session::{
    AGENT_SESSION_SCHEMA_VERSION, AgentSession, AgentStatus, ContributionContext,
    ContributionPlanReference, PlanReviewStatus, SessionInitError, SessionUsage, ToolCallRecord,
};
pub use tool::{
    PermissionPolicy, ReadOnlyPermissionPolicy, Tool, ToolContext, ToolError, ToolErrorCode,
    ToolPermission, ToolRegistry, ToolResult,
};
