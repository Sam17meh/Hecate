//! HECATE-owned agent loop and capability boundaries.
//!
//! Model providers return typed responses; requested tools can only resolve through the explicit
//! registry and permission policy. Unknown tools and non-read-only permissions are denied by
//! default. Repository tools are read-only and capability-scoped to prepared workspaces.

pub mod openai_compatible;
pub mod patch;
pub(crate) mod patch_application;
pub mod patch_approval;
pub mod patch_validation;
pub mod provider;
pub mod repository_tools;
pub mod runtime;
pub mod session;
pub mod tool;

#[cfg(test)]
pub(crate) mod provider_test_support;

pub use openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleProvider};
pub use provider::{
    ConversationMessage, MessageRole, MockProvider, ModelProvider, ModelRequest, ModelResponse,
    ProviderError, ProviderErrorKind, TokenUsage, ToolCallRequest, ToolDefinition,
};
pub use repository_tools::WorkspaceRepositoryTools;
pub use runtime::{AgentRuntime, CancellationToken, ExecutionLimits, RuntimeError};
pub use session::{
    AGENT_SESSION_SCHEMA_VERSION, AgentSession, AgentStatus, ContributionContext,
    ContributionPlanReference, PlanReviewStatus, SessionInitError, SessionUsage, ToolCallRecord,
};
pub use tool::{
    PermissionPolicy, ReadOnlyPermissionPolicy, Tool, ToolContext, ToolError, ToolErrorCode,
    ToolPermission, ToolRegistry, ToolResult,
};
