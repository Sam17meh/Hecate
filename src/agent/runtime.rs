use crate::agent::provider::{
    ConversationMessage, MessageRole, ModelProvider, ModelRequest, ProviderError,
};
use crate::agent::session::{AgentSession, AgentStatus};
use crate::agent::tool::{
    PermissionPolicy, ReadOnlyPermissionPolicy, ToolContext, ToolError, ToolRegistry,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionLimits {
    /// Maximum provider round trips in one run.
    pub max_iterations: u32,
    /// Maximum successfully dispatched tool calls in one run.
    pub max_tool_calls: u32,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            max_tool_calls: 16,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidSession(String),
    InvalidState(AgentStatus),
    Provider(ProviderError),
    MalformedResponse(String),
    ToolCall { call_id: String, error: ToolError },
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSession(message) => write!(formatter, "Invalid agent session: {message}"),
            Self::InvalidState(status) => write!(
                formatter,
                "Cannot start an agent run from session state {status:?}"
            ),
            Self::Provider(error) => write!(formatter, "Model provider failed: {}", error.message),
            Self::MalformedResponse(message) => {
                write!(formatter, "Model returned a malformed response: {message}")
            }
            Self::ToolCall { call_id, error } => {
                write!(formatter, "Tool call {call_id} failed: {}", error.message)
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

pub struct AgentRuntime<P> {
    provider: P,
    tools: ToolRegistry,
    policy: Box<dyn PermissionPolicy>,
    limits: ExecutionLimits,
}

impl<P: ModelProvider> AgentRuntime<P> {
    pub fn new(provider: P, tools: ToolRegistry, limits: ExecutionLimits) -> Self {
        Self::with_policy(provider, tools, limits, ReadOnlyPermissionPolicy)
    }

    pub fn with_policy(
        provider: P,
        tools: ToolRegistry,
        limits: ExecutionLimits,
        policy: impl PermissionPolicy + 'static,
    ) -> Self {
        Self {
            provider,
            tools,
            policy: Box::new(policy),
            limits,
        }
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn run(
        &mut self,
        session: &mut AgentSession,
        cancellation: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        session
            .validate()
            .map_err(|error| RuntimeError::InvalidSession(error.to_string()))?;
        if session.status != AgentStatus::Ready {
            return Err(RuntimeError::InvalidState(session.status));
        }
        session.status = AgentStatus::Running;
        let tool_context = ToolContext {
            contribution: session.context.clone(),
            cancellation: cancellation.clone(),
        };

        loop {
            if cancellation.is_cancelled() {
                session.status = AgentStatus::Cancelled;
                session.failure = Some("Agent run was cancelled".into());
                return Ok(());
            }
            if session.usage.model_iterations >= self.limits.max_iterations {
                session.status = AgentStatus::LimitReached;
                session.failure = Some(format!(
                    "Maximum model iteration limit ({}) reached",
                    self.limits.max_iterations
                ));
                return Ok(());
            }

            let request = ModelRequest {
                messages: session.messages.clone(),
                tools: self.tools.definitions(),
            };
            session.usage.model_iterations += 1;
            let response = match self.provider.complete(request) {
                Ok(response) => response,
                Err(error) => return Err(fail_session(session, RuntimeError::Provider(error))),
            };
            if let Some(usage) = &response.usage {
                session.usage.tokens.add_assign(usage);
            }
            if cancellation.is_cancelled() {
                session.status = AgentStatus::Cancelled;
                session.failure = Some("Agent run was cancelled after a provider response".into());
                return Ok(());
            }

            if response.tool_calls.is_empty() {
                let Some(content) = response
                    .content
                    .filter(|content| !content.trim().is_empty())
                else {
                    return Err(fail_session(
                        session,
                        RuntimeError::MalformedResponse(
                            "expected nonempty text or at least one tool call".into(),
                        ),
                    ));
                };
                session.messages.push(ConversationMessage::text(
                    MessageRole::Assistant,
                    content.clone(),
                ));
                session.final_response = Some(content);
                session.status = AgentStatus::Completed;
                return Ok(());
            }

            session.messages.push(ConversationMessage {
                role: MessageRole::Assistant,
                content: response.content,
                tool_calls: response.tool_calls.clone(),
                tool_call_id: None,
            });
            let requested = response.tool_calls.len() as u32;
            let remaining = self
                .limits
                .max_tool_calls
                .saturating_sub(session.usage.tool_calls);
            if requested > remaining {
                session.status = AgentStatus::LimitReached;
                session.failure = Some(format!(
                    "Tool-call batch of {requested} exceeds the remaining tool-call budget of {remaining}; none were dispatched"
                ));
                return Ok(());
            }

            if let Some((call, error)) = self.preflight_calls(session, &response.tool_calls) {
                let runtime_error = RuntimeError::ToolCall {
                    call_id: call.id.clone(),
                    error: error.clone(),
                };
                session.record_rejected_call(call, error.message);
                return Err(fail_session(session, runtime_error));
            }

            for call in response.tool_calls {
                if cancellation.is_cancelled() {
                    session.record_tool_call(
                        call,
                        Err("Agent run was cancelled before tool dispatch".into()),
                    );
                    session.status = AgentStatus::Cancelled;
                    session.failure = Some("Agent run was cancelled between tool calls".into());
                    return Ok(());
                }
                session.usage.tool_calls += 1;
                let tool_result = self
                    .tools
                    .execute(&call, self.policy.as_ref(), &tool_context);
                if cancellation.is_cancelled() {
                    session.record_tool_call(
                        call,
                        Err("Agent run was cancelled during tool execution".into()),
                    );
                    session.status = AgentStatus::Cancelled;
                    session.failure = Some("Agent run was cancelled during tool execution".into());
                    return Ok(());
                }
                match tool_result {
                    Ok(result) => session.record_tool_call(call, Ok(result)),
                    Err(error) => {
                        let runtime_error = RuntimeError::ToolCall {
                            call_id: call.id.clone(),
                            error: error.clone(),
                        };
                        session.record_tool_call(call, Err(error.message.clone()));
                        return Err(fail_session(session, runtime_error));
                    }
                }
            }
        }
    }

    fn preflight_calls(
        &self,
        session: &AgentSession,
        calls: &[crate::agent::provider::ToolCallRequest],
    ) -> Option<(crate::agent::provider::ToolCallRequest, ToolError)> {
        let mut ids = std::collections::BTreeSet::new();
        for previous in &session.tool_calls {
            ids.insert(previous.call.id.as_str());
        }
        for call in calls {
            if call.id.trim().is_empty() || call.name.trim().is_empty() {
                return Some((
                    call.clone(),
                    ToolError::new(
                        crate::agent::tool::ToolErrorCode::InvalidArguments,
                        "Tool call id and name must be nonempty",
                    ),
                ));
            }
            if !ids.insert(call.id.as_str()) {
                return Some((
                    call.clone(),
                    ToolError::new(
                        crate::agent::tool::ToolErrorCode::InvalidArguments,
                        format!("Duplicate tool call id `{}`", call.id),
                    ),
                ));
            }
            if let Err(error) =
                self.tools
                    .validate_call(call, self.policy.as_ref(), &session.context)
            {
                return Some((call.clone(), error));
            }
        }
        None
    }
}

fn fail_session(session: &mut AgentSession, error: RuntimeError) -> RuntimeError {
    session.status = AgentStatus::Failed;
    session.failure = Some(error.to_string());
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::provider::{
        MockProvider, ModelResponse, ProviderErrorKind, TokenUsage, ToolCallRequest, ToolDefinition,
    };
    use crate::agent::session::{
        AGENT_SESSION_SCHEMA_VERSION, ContributionContext, ContributionPlanReference,
        PlanReviewStatus, SessionInitError,
    };
    use crate::agent::tool::{Tool, ToolErrorCode, ToolPermission, ToolResult};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    fn context() -> ContributionContext {
        ContributionContext {
            repository_url: "https://github.com/example/project".into(),
            issue_number: 17,
            issue_title: "Improve the cache refresh path".into(),
            plan: ContributionPlanReference {
                artifact_id: ".hecate/plan-v1.json".into(),
                repository_url: "https://github.com/example/project".into(),
                issue_number: 17,
                schema_version: 1,
                review_status: PlanReviewStatus::Approved,
            },
        }
    }

    fn make_session() -> AgentSession {
        AgentSession::new("test-session", context(), "Investigate the approved plan").unwrap()
    }

    fn call(id: &str, name: &str, value: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: id.into(),
            name: name.into(),
            arguments: value,
        }
    }

    struct MockLookupTool {
        executions: Arc<Mutex<Vec<Value>>>,
        fail: bool,
        permission: ToolPermission,
    }

    impl Tool for MockLookupTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "lookup_issue_context".into(),
                description: "Return deterministic mock context for a selected issue".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"],
                    "additionalProperties": false
                }),
            }
        }

        fn permission(&self) -> ToolPermission {
            self.permission
        }

        fn validate_arguments(
            &self,
            arguments: &Value,
        ) -> Result<(), crate::agent::tool::ToolError> {
            if arguments
                .as_object()
                .is_some_and(|object| object.len() == 1 && object["value"].is_string())
            {
                Ok(())
            } else {
                Err(crate::agent::tool::ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    "expected an object with one string `value` field",
                ))
            }
        }

        fn execute(
            &mut self,
            arguments: Value,
            _context: &ToolContext,
        ) -> Result<ToolResult, crate::agent::tool::ToolError> {
            if self.fail {
                return Err(crate::agent::tool::ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    "mock tool failure",
                ));
            }
            self.executions.lock().unwrap().push(arguments.clone());
            Ok(ToolResult::new(json!({ "mock_value": arguments["value"] })))
        }
    }

    fn registry(fail: bool, permission: ToolPermission) -> (ToolRegistry, Arc<Mutex<Vec<Value>>>) {
        let executions = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry
            .register(Box::new(MockLookupTool {
                executions: executions.clone(),
                fail,
                permission,
            }))
            .unwrap();
        (registry, executions)
    }

    fn readonly_registry() -> (ToolRegistry, Arc<Mutex<Vec<Value>>>) {
        registry(false, ToolPermission::ReadOnly)
    }

    fn tool_response(calls: Vec<ToolCallRequest>) -> ModelResponse {
        ModelResponse::with_tool_calls(calls)
    }

    #[test]
    fn completes_with_text_and_tracks_usage() {
        let provider = MockProvider::new([Ok(ModelResponse {
            content: Some("Review the plan before acting.".into()),
            tool_calls: vec![],
            usage: Some(TokenUsage {
                input_tokens: 11,
                output_tokens: 5,
            }),
        })]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();

        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();

        assert_eq!(session.status, AgentStatus::Completed);
        assert_eq!(
            session.final_response.as_deref(),
            Some("Review the plan before acting.")
        );
        assert_eq!(session.usage.model_iterations, 1);
        assert_eq!(session.usage.tokens.input_tokens, 11);
        assert_eq!(runtime.provider().requests().len(), 1);
    }

    #[test]
    fn executes_multiple_tool_calls_then_completes_on_the_next_iteration() {
        let provider = MockProvider::from_responses([
            tool_response(vec![
                call("call-a", "lookup_issue_context", json!({"value":"first"})),
                call("call-b", "lookup_issue_context", json!({"value":"second"})),
            ]),
            ModelResponse::text("Both mock lookups returned."),
        ]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();

        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();

        assert_eq!(session.status, AgentStatus::Completed);
        assert_eq!(session.usage.model_iterations, 2);
        assert_eq!(session.usage.tool_calls, 2);
        assert_eq!(session.tool_calls.len(), 2);
        assert_eq!(executions.lock().unwrap().len(), 2);
        let followup = &runtime.provider().requests()[1].messages;
        assert!(followup.iter().any(|message| {
            message.role == MessageRole::Tool && message.tool_call_id.as_deref() == Some("call-a")
        }));
        assert!(followup.iter().any(|message| {
            message.role == MessageRole::Tool && message.tool_call_id.as_deref() == Some("call-b")
        }));
    }

    #[test]
    fn unknown_tools_are_denied_without_running_any_tool() {
        let provider = MockProvider::from_responses([tool_response(vec![call(
            "unknown-1",
            "delete_repository",
            json!({}),
        )])]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();

        let error = runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::ToolCall {
                error: crate::agent::tool::ToolError {
                    code: ToolErrorCode::UnknownTool,
                    ..
                },
                ..
            }
        ));
        assert_eq!(session.status, AgentStatus::Failed);
        assert!(executions.lock().unwrap().is_empty());
        assert!(
            session.tool_calls[0]
                .error
                .as_deref()
                .unwrap()
                .contains("not registered")
        );
    }

    #[test]
    fn invalid_arguments_and_permission_denials_are_rejected_before_execution() {
        let provider = MockProvider::from_responses([tool_response(vec![call(
            "invalid-1",
            "lookup_issue_context",
            json!({"value": 42}),
        )])]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        let error = runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::ToolCall {
                error: crate::agent::tool::ToolError {
                    code: ToolErrorCode::InvalidArguments,
                    ..
                },
                ..
            }
        ));
        assert!(executions.lock().unwrap().is_empty());

        let provider = MockProvider::from_responses([tool_response(vec![call(
            "write-1",
            "lookup_issue_context",
            json!({"value": "x"}),
        )])]);
        let (tools, executions) = registry(false, ToolPermission::WorkspaceWrite);
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        let error = runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::ToolCall {
                error: crate::agent::tool::ToolError {
                    code: ToolErrorCode::PermissionDenied,
                    ..
                },
                ..
            }
        ));
        assert!(executions.lock().unwrap().is_empty());
    }

    #[test]
    fn provider_and_tool_failures_enter_failed_state_without_retrying() {
        let provider = MockProvider::new([Err(crate::agent::provider::ProviderError::new(
            ProviderErrorKind::Unavailable,
            "mock provider offline",
        ))]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        assert!(matches!(
            runtime.run(&mut session, &CancellationToken::default()),
            Err(RuntimeError::Provider(_))
        ));
        assert_eq!(session.status, AgentStatus::Failed);
        assert_eq!(session.usage.model_iterations, 1);
        assert_eq!(runtime.provider().requests().len(), 1);

        let provider = MockProvider::from_responses([tool_response(vec![call(
            "failure-1",
            "lookup_issue_context",
            json!({"value": "x"}),
        )])]);
        let (tools, _) = registry(true, ToolPermission::ReadOnly);
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        assert!(matches!(
            runtime.run(&mut session, &CancellationToken::default()),
            Err(RuntimeError::ToolCall {
                error: crate::agent::tool::ToolError {
                    code: ToolErrorCode::ExecutionFailed,
                    ..
                },
                ..
            })
        ));
        assert_eq!(session.status, AgentStatus::Failed);
        assert_eq!(session.usage.tool_calls, 1);
        assert!(session.tool_calls[0].error.is_some());
    }

    #[test]
    fn iteration_limit_stops_a_tool_loop_and_tool_batch_limit_is_atomic() {
        let provider = MockProvider::from_responses([
            tool_response(vec![call(
                "bounded-1",
                "lookup_issue_context",
                json!({"value": "x"}),
            )]),
            ModelResponse::text("This response must not be requested."),
        ]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(
            provider,
            tools,
            ExecutionLimits {
                max_iterations: 1,
                max_tool_calls: 3,
            },
        );
        let mut session = make_session();
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        assert_eq!(session.status, AgentStatus::LimitReached);
        assert_eq!(session.usage.model_iterations, 1);
        assert_eq!(runtime.provider().requests().len(), 1);
        assert_eq!(executions.lock().unwrap().len(), 1);

        let provider = MockProvider::from_responses([tool_response(vec![
            call("batch-a", "lookup_issue_context", json!({"value": "a"})),
            call("batch-b", "lookup_issue_context", json!({"value": "b"})),
        ])]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(
            provider,
            tools,
            ExecutionLimits {
                max_iterations: 4,
                max_tool_calls: 1,
            },
        );
        let mut session = make_session();
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        assert_eq!(session.status, AgentStatus::LimitReached);
        assert_eq!(session.usage.tool_calls, 0);
        assert!(executions.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_stops_before_provider_or_before_dispatch() {
        let provider = MockProvider::from_responses([ModelResponse::text("unused")]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        let token = CancellationToken::default();
        token.cancel();
        runtime.run(&mut session, &token).unwrap();
        assert_eq!(session.status, AgentStatus::Cancelled);
        assert_eq!(session.usage.model_iterations, 0);
        assert!(runtime.provider().requests().is_empty());

        let token = CancellationToken::default();
        let provider = CancellingProvider {
            cancellation: token.clone(),
            response: tool_response(vec![call(
                "cancelled-1",
                "lookup_issue_context",
                json!({"value": "x"}),
            )]),
        };
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        runtime.run(&mut session, &token).unwrap();
        assert_eq!(session.status, AgentStatus::Cancelled);
        assert_eq!(session.usage.tool_calls, 0);
        assert!(executions.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_during_tool_execution_enters_cancelled_terminal_state() {
        struct CancellingTool;

        impl Tool for CancellingTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "cancel_lookup".into(),
                    description: "Cancel this mock operation".into(),
                    input_schema: json!({"type":"object", "additionalProperties":false}),
                }
            }

            fn permission(&self) -> ToolPermission {
                ToolPermission::ReadOnly
            }

            fn validate_arguments(
                &self,
                arguments: &Value,
            ) -> Result<(), crate::agent::tool::ToolError> {
                if arguments
                    .as_object()
                    .is_some_and(|object| object.is_empty())
                {
                    Ok(())
                } else {
                    Err(crate::agent::tool::ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "expected empty object",
                    ))
                }
            }

            fn execute(
                &mut self,
                _arguments: Value,
                context: &ToolContext,
            ) -> Result<ToolResult, crate::agent::tool::ToolError> {
                context.cancellation.cancel();
                Err(crate::agent::tool::ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    "cancelled mock operation",
                ))
            }
        }

        let mut tools = ToolRegistry::new();
        tools.register(Box::new(CancellingTool)).unwrap();
        let provider = MockProvider::from_responses([tool_response(vec![call(
            "cancel-during-tool",
            "cancel_lookup",
            json!({}),
        )])]);
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        assert_eq!(session.status, AgentStatus::Cancelled);
        assert_eq!(session.usage.tool_calls, 1);
        assert_eq!(session.tool_calls.len(), 1);
        assert!(
            session.tool_calls[0]
                .error
                .as_deref()
                .unwrap()
                .contains("during tool execution")
        );
        assert!(
            session
                .failure
                .as_deref()
                .unwrap()
                .contains("during tool execution")
        );
    }

    struct CancellingProvider {
        cancellation: CancellationToken,
        response: ModelResponse,
    }

    impl ModelProvider for CancellingProvider {
        fn complete(
            &mut self,
            _request: ModelRequest,
        ) -> Result<ModelResponse, crate::agent::provider::ProviderError> {
            self.cancellation.cancel();
            Ok(self.response.clone())
        }
    }

    #[test]
    fn rejects_empty_responses_duplicate_call_ids_and_unapproved_plans() {
        let provider = MockProvider::from_responses([ModelResponse {
            content: Some("  ".into()),
            tool_calls: vec![],
            usage: None,
        }]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        assert!(matches!(
            runtime.run(&mut session, &CancellationToken::default()),
            Err(RuntimeError::MalformedResponse(_))
        ));
        assert_eq!(session.status, AgentStatus::Failed);

        let provider = MockProvider::from_responses([tool_response(vec![
            call("duplicate", "lookup_issue_context", json!({"value": "a"})),
            call("duplicate", "lookup_issue_context", json!({"value": "b"})),
        ])]);
        let (tools, executions) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = make_session();
        assert!(matches!(
            runtime.run(&mut session, &CancellationToken::default()),
            Err(RuntimeError::ToolCall { .. })
        ));
        assert!(executions.lock().unwrap().is_empty());

        let mut unapproved = context();
        unapproved.plan.review_status = PlanReviewStatus::PendingHumanReview;
        assert_eq!(
            AgentSession::new("pending", unapproved, "continue").unwrap_err(),
            SessionInitError::PlanNotApproved
        );

        let mut mismatched_issue = context();
        mismatched_issue.plan.issue_number = 18;
        assert!(matches!(
            AgentSession::new("mismatch-issue", mismatched_issue, "continue"),
            Err(SessionInitError::InvalidContext(_))
        ));
        let mut mismatched_repository = context();
        mismatched_repository.plan.repository_url = "https://github.com/other/project".into();
        assert!(matches!(
            AgentSession::new("mismatch-repo", mismatched_repository, "continue"),
            Err(SessionInitError::InvalidContext(_))
        ));

        let provider = MockProvider::from_responses([ModelResponse::text("must not run")]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut invalid_session = make_session();
        invalid_session.context.plan.review_status = PlanReviewStatus::PendingHumanReview;
        assert!(matches!(
            runtime.run(&mut invalid_session, &CancellationToken::default()),
            Err(RuntimeError::InvalidSession(_))
        ));
        assert_eq!(invalid_session.status, AgentStatus::Ready);
        assert!(runtime.provider().requests().is_empty());
    }

    #[test]
    fn registry_rejects_duplicate_tool_names() {
        let executions = Arc::new(Mutex::new(Vec::new()));
        let make_tool = || {
            Box::new(MockLookupTool {
                executions: executions.clone(),
                fail: false,
                permission: ToolPermission::ReadOnly,
            }) as Box<dyn Tool>
        };
        let mut registry = ToolRegistry::new();
        registry.register(make_tool()).unwrap();
        let error = registry.register(make_tool()).unwrap_err();
        assert_eq!(error.code, ToolErrorCode::DuplicateName);
        assert_eq!(registry.definitions().len(), 1);
    }

    #[test]
    fn session_state_is_versioned_serializable_and_cannot_run_twice() {
        let session = make_session();
        assert_eq!(session.schema_version, AGENT_SESSION_SCHEMA_VERSION);
        let json = serde_json::to_string(&session).unwrap();
        let restored: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, session);

        let provider = MockProvider::from_responses([ModelResponse::text("complete")]);
        let (tools, _) = readonly_registry();
        let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
        let mut session = restored;
        runtime
            .run(&mut session, &CancellationToken::default())
            .unwrap();
        assert_eq!(session.status, AgentStatus::Completed);
        assert!(matches!(
            runtime.run(&mut session, &CancellationToken::default()),
            Err(RuntimeError::InvalidState(AgentStatus::Completed))
        ));
    }

    #[test]
    fn execution_is_deterministic_for_identical_mock_inputs() {
        let execute = || {
            let provider = MockProvider::from_responses([
                tool_response(vec![call(
                    "deterministic-1",
                    "lookup_issue_context",
                    json!({"value": "same"}),
                )]),
                ModelResponse::text("same result"),
            ]);
            let (tools, _) = readonly_registry();
            let mut runtime = AgentRuntime::new(provider, tools, ExecutionLimits::default());
            let mut session = AgentSession::new(
                "deterministic-session",
                context(),
                "Investigate the approved plan",
            )
            .unwrap();
            runtime
                .run(&mut session, &CancellationToken::default())
                .unwrap();
            session
        };
        assert_eq!(execute(), execute());
    }
}
