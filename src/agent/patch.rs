//! Typed, untrusted patch proposals and read-only validation results.
//!
//! A [`ValidatedPatch`] is evidence that the proposal was checked against one prepared
//! workspace snapshot. It is not approval and it deliberately has no filesystem application API.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const PATCH_PROPOSAL_SCHEMA_VERSION: u32 = 1;

/// Untrusted model or user supplied proposal. It never represents approval.
///
/// Callers deserializing this structure must cap the serialized input bytes before invoking
/// serde. Validation limits apply after the structure has been allocated and cannot bound the
/// memory used by an unbounded upstream JSON parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchProposal {
    pub schema_version: u32,
    pub files: Vec<FilePatch>,
}

impl PatchProposal {
    pub fn new(files: Vec<FilePatch>) -> Self {
        Self {
            schema_version: PATCH_PROPOSAL_SCHEMA_VERSION,
            files,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePatch {
    /// Workspace-relative path using `/` separators.
    pub path: String,
    pub operation: PatchOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchOperation {
    Replace {
        expected_original_sha256: String,
        edits: Vec<TextEdit>,
    },
    Create {
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEdit {
    pub old_text: String,
    pub new_text: String,
}

/// A successfully validated in-memory change. Fields are private so callers cannot forge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedFileChange {
    pub(crate) path: String,
    pub(crate) operation: ValidatedOperation,
    pub(crate) original_content: Option<String>,
    pub(crate) original_file_identity: Option<String>,
    pub(crate) resulting_content: String,
    pub(crate) resulting_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidatedOperation {
    Replace {
        original_sha256: String,
        edits: usize,
    },
    Create,
}

impl ValidatedFileChange {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn resulting_content(&self) -> &str {
        &self.resulting_content
    }

    pub fn original_content(&self) -> Option<&str> {
        self.original_content.as_deref()
    }

    pub fn resulting_sha256(&self) -> &str {
        &self.resulting_sha256
    }

    pub fn original_sha256(&self) -> Option<&str> {
        match &self.operation {
            ValidatedOperation::Replace {
                original_sha256, ..
            } => Some(original_sha256),
            ValidatedOperation::Create => None,
        }
    }

    pub(crate) fn original_file_identity(&self) -> Option<&str> {
        self.original_file_identity.as_deref()
    }

    pub fn edit_count(&self) -> usize {
        match &self.operation {
            ValidatedOperation::Replace { edits, .. } => *edits,
            ValidatedOperation::Create => 0,
        }
    }

    pub fn is_new_file(&self) -> bool {
        matches!(&self.operation, ValidatedOperation::Create)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSummary {
    pub files_changed: usize,
    pub files_created: usize,
    pub replacements: usize,
    pub preview: String,
}

/// In-process identity for the prepared workspace used to validate a patch.
///
/// This identifies the retained workspace and checkout objects and their preparation metadata;
/// it is not a durable authentication token and cannot restore capabilities after restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchWorkspaceBinding {
    pub(crate) workspace_object_id: String,
    pub(crate) checkout_object_id: String,
    pub(crate) manifest_sha256: String,
    pub(crate) plan_sha256: String,
    pub(crate) repository_url: String,
    pub(crate) checkout_commit: String,
    pub(crate) branch: String,
}

/// Trusted result constructed only after every proposal operation validates successfully.
/// This type is neither deserializable nor writable to a filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPatch {
    pub(crate) patch_sha256: String,
    pub(crate) changes: Vec<ValidatedFileChange>,
    pub(crate) summary: PatchSummary,
    pub(crate) workspace: PatchWorkspaceBinding,
}

impl ValidatedPatch {
    pub fn patch_sha256(&self) -> &str {
        &self.patch_sha256
    }

    pub fn changes(&self) -> &[ValidatedFileChange] {
        &self.changes
    }

    pub fn summary(&self) -> &PatchSummary {
        &self.summary
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchValidationError {
    Workspace(String),
    InvalidProposal(String),
    UnsafePath(String),
    File(String),
    StaleFile(String),
    AmbiguousEdit(String),
    LimitExceeded(String),
}

impl fmt::Display for PatchValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(message)
            | Self::InvalidProposal(message)
            | Self::UnsafePath(message)
            | Self::File(message)
            | Self::StaleFile(message)
            | Self::AmbiguousEdit(message)
            | Self::LimitExceeded(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PatchValidationError {}
