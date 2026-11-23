use crate::agent::provider::{ConversationMessage, MessageRole, TokenUsage, ToolCallRequest};
use crate::agent::tool::ToolResult;
use crate::github::GitHubClient;
use crate::workspace::PLAN_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReviewStatus {
    PendingHumanReview,
    Approved,
}

pub const AGENT_SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPlanReference {
    pub artifact_id: String,
    pub repository_url: String,
    pub issue_number: u64,
    pub schema_version: u32,
    pub review_status: PlanReviewStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionContext {
    pub repository_url: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub plan: ContributionPlanReference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Ready,
    Running,
    Completed,
    Failed,
    Cancelled,
    LimitReached,
}

impl AgentStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::LimitReached
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub model_iterations: u32,
    pub tool_calls: u32,
    pub tokens: TokenUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call: ToolCallRequest,
    pub result: Option<ToolResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInitError {
    EmptySessionId,
    EmptyPrompt,
    InvalidContext(String),
    PlanNotApproved,
    UnsupportedSchema(u32),
}

impl std::fmt::Display for SessionInitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySessionId => write!(formatter, "Session id cannot be empty"),
            Self::EmptyPrompt => write!(formatter, "Initial user prompt cannot be empty"),
            Self::InvalidContext(message) => {
                write!(formatter, "Invalid contribution context: {message}")
            }
            Self::PlanNotApproved => write!(
                formatter,
                "Agent sessions require a human-approved contribution plan"
            ),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "Unsupported agent session schema version {version}"
                )
            }
        }
    }
}

impl std::error::Error for SessionInitError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    pub schema_version: u32,
    pub session_id: String,
    pub context: ContributionContext,
    pub messages: Vec<ConversationMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub status: AgentStatus,
    pub usage: SessionUsage,
    pub final_response: Option<String>,
    pub failure: Option<String>,
}

impl AgentSession {
    pub fn new(
        session_id: impl Into<String>,
        context: ContributionContext,
        initial_prompt: impl Into<String>,
    ) -> Result<Self, SessionInitError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(SessionInitError::EmptySessionId);
        }
        let initial_prompt = initial_prompt.into();
        if initial_prompt.trim().is_empty() {
            return Err(SessionInitError::EmptyPrompt);
        }
        validate_context(&context)?;
        let context_summary = format!(
            "You are HECATE's contribution assistant. Repository: {}. Selected issue: #{} — {}. Human-approved plan reference: {} (schema {}). Work only from the provided contribution context and registered tools.",
            context.repository_url,
            context.issue_number,
            context.issue_title,
            context.plan.artifact_id,
            context.plan.schema_version
        );
        let session = Self {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            session_id,
            context,
            messages: vec![
                ConversationMessage::text(MessageRole::System, context_summary),
                ConversationMessage::text(MessageRole::User, initial_prompt),
            ],
            tool_calls: Vec::new(),
            status: AgentStatus::Ready,
            usage: SessionUsage::default(),
            final_response: None,
            failure: None,
        };
        session.validate()?;
        Ok(session)
    }

    /// Revalidate restored or externally modified state before every agent run.
    pub fn validate(&self) -> Result<(), SessionInitError> {
        if self.schema_version != AGENT_SESSION_SCHEMA_VERSION {
            return Err(SessionInitError::UnsupportedSchema(self.schema_version));
        }
        if self.session_id.trim().is_empty() {
            return Err(SessionInitError::EmptySessionId);
        }
        validate_context(&self.context)
    }

    pub(crate) fn record_tool_call(
        &mut self,
        call: ToolCallRequest,
        result: Result<ToolResult, String>,
    ) {
        match result {
            Ok(result) => {
                let content =
                    serde_json::to_string(&result.output).unwrap_or_else(|_| "null".to_string());
                self.messages
                    .push(ConversationMessage::tool_result(call.id.clone(), content));
                self.tool_calls.push(ToolCallRecord {
                    call,
                    result: Some(result),
                    error: None,
                });
            }
            Err(error) => {
                let content = serde_json::json!({"error": error});
                self.messages.push(ConversationMessage::tool_result(
                    call.id.clone(),
                    content.to_string(),
                ));
                self.tool_calls.push(ToolCallRecord {
                    call,
                    result: None,
                    error: Some(error),
                });
            }
        }
    }

    pub(crate) fn record_rejected_call(&mut self, call: ToolCallRequest, error: String) {
        self.record_tool_call(call, Err(error));
    }
}

fn validate_context(context: &ContributionContext) -> Result<(), SessionInitError> {
    if context.issue_number == 0
        || context.repository_url.trim().is_empty()
        || context.issue_title.trim().is_empty()
        || context.plan.artifact_id.trim().is_empty()
        || context.plan.schema_version != PLAN_SCHEMA_VERSION
    {
        return Err(SessionInitError::InvalidContext(
            "repository, issue, and contribution plan reference fields are required".into(),
        ));
    }
    let (plan_owner, plan_repository) = GitHubClient::parse_repository(&context.repository_url)
        .map_err(SessionInitError::InvalidContext)?;
    let (reference_owner, reference_repository) =
        GitHubClient::parse_repository(&context.plan.repository_url)
            .map_err(SessionInitError::InvalidContext)?;
    if !plan_owner.eq_ignore_ascii_case(&reference_owner)
        || !plan_repository.eq_ignore_ascii_case(&reference_repository)
        || context.plan.issue_number != context.issue_number
    {
        return Err(SessionInitError::InvalidContext(
            "plan reference must identify the selected repository and issue".into(),
        ));
    }
    if context.plan.review_status != PlanReviewStatus::Approved {
        return Err(SessionInitError::PlanNotApproved);
    }
    Ok(())
}
