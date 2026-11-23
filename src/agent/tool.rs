use crate::agent::provider::{ToolCallRequest, ToolDefinition};
use crate::agent::runtime::CancellationToken;
use crate::agent::session::ContributionContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermission {
    ReadOnly,
    WorkspaceWrite,
    CommandExecution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    UnknownTool,
    InvalidArguments,
    PermissionDenied,
    ExecutionFailed,
    InvalidDefinition,
    DuplicateName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: Value,
}

impl ToolResult {
    pub fn new(output: Value) -> Self {
        Self { output }
    }
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub contribution: ContributionContext,
    pub cancellation: CancellationToken,
}

pub trait Tool: Send {
    fn definition(&self) -> ToolDefinition;
    fn permission(&self) -> ToolPermission;
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError>;
    fn execute(&mut self, arguments: Value, context: &ToolContext)
    -> Result<ToolResult, ToolError>;
}

pub trait PermissionPolicy: Send + Sync {
    fn authorize(
        &self,
        permission: ToolPermission,
        context: &ContributionContext,
    ) -> Result<(), ToolError>;
}

/// Safe default for this runtime foundation. Future write/command tools need an explicit policy.
#[derive(Debug, Default)]
pub struct ReadOnlyPermissionPolicy;

impl PermissionPolicy for ReadOnlyPermissionPolicy {
    fn authorize(
        &self,
        permission: ToolPermission,
        _context: &ContributionContext,
    ) -> Result<(), ToolError> {
        if permission == ToolPermission::ReadOnly {
            Ok(())
        } else {
            Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                format!("Tool permission {permission:?} is denied by the default policy"),
            ))
        }
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        let definition = tool.definition();
        if !valid_tool_name(&definition.name)
            || definition.description.trim().is_empty()
            || !definition.input_schema.is_object()
        {
            return Err(ToolError::new(
                ToolErrorCode::InvalidDefinition,
                "Tool definitions need a valid name, description, and object input schema",
            ));
        }
        if self.tools.contains_key(&definition.name) {
            return Err(ToolError::new(
                ToolErrorCode::DuplicateName,
                format!("Tool `{}` is already registered", definition.name),
            ));
        }
        self.tools.insert(definition.name.clone(), tool);
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn validate_call(
        &self,
        call: &ToolCallRequest,
        policy: &dyn PermissionPolicy,
        context: &ContributionContext,
    ) -> Result<(), ToolError> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::UnknownTool,
                format!(
                    "Tool `{}` is not registered; unknown tools are denied",
                    call.name
                ),
            )
        })?;
        policy.authorize(tool.permission(), context)?;
        tool.validate_arguments(&call.arguments)
    }

    pub fn execute(
        &mut self,
        call: &ToolCallRequest,
        policy: &dyn PermissionPolicy,
        context: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.validate_call(call, policy, &context.contribution)?;
        let tool = self.tools.get_mut(&call.name).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("Tool `{}` is not registered", call.name),
            )
        })?;
        tool.execute(call.arguments.clone(), context)
    }
}

fn valid_tool_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && name.len() <= 64
        && bytes
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}
