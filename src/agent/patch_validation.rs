//! Read-only validation of structured patch proposals against approved prepared workspaces.
//!
//! Validation computes proposed results in memory only. No method in this module writes files.

use crate::agent::patch::{
    FilePatch, PATCH_PROPOSAL_SCHEMA_VERSION, PatchOperation, PatchProposal, PatchSummary,
    PatchValidationError, PatchWorkspaceBinding, TextEdit, ValidatedFileChange, ValidatedOperation,
    ValidatedPatch,
};
use crate::agent::repository_tools;
use crate::agent::session::{ContributionContext, PlanReviewStatus};
use crate::workspace::{self, WorkspaceResult};
use cap_std::fs::Dir;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_PATCH_FILES: usize = 20;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_EDIT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 4 * 1024 * 1024;

/// Trusted HECATE workspace binding. Construct this from application/session context, never from
/// model arguments. The proposal contains no workspace path or repository identity.
pub struct PatchValidator {
    workspace_parent: Dir,
    workspace_name: OsString,
    workspace: Dir,
    metadata: Dir,
    checkout: Dir,
    manifest_sha256: String,
    plan_sha256: String,
    workspace_binding: PatchWorkspaceBinding,
    context: ContributionContext,
}

impl PatchValidator {
    pub fn new(
        prepared_workspace: &WorkspaceResult,
        context: &ContributionContext,
    ) -> Result<Self, PatchValidationError> {
        if context.plan.review_status != PlanReviewStatus::Approved {
            return Err(PatchValidationError::Workspace(
                "Patch validation requires an approved contribution plan".into(),
            ));
        }
        if context.plan.schema_version != workspace::PLAN_SCHEMA_VERSION {
            return Err(PatchValidationError::Workspace(
                "Contribution plan reference has an unsupported schema version".into(),
            ));
        }
        let (context_owner, context_repo) =
            crate::github::GitHubClient::parse_repository(&context.repository_url)
                .map_err(PatchValidationError::Workspace)?;
        let (plan_owner, plan_repo) =
            crate::github::GitHubClient::parse_repository(&context.plan.repository_url)
                .map_err(PatchValidationError::Workspace)?;
        if !context_owner.eq_ignore_ascii_case(&plan_owner)
            || !context_repo.eq_ignore_ascii_case(&plan_repo)
            || context.plan.issue_number != context.issue_number
        {
            return Err(PatchValidationError::Workspace(
                "Approved plan context does not match the selected repository and issue".into(),
            ));
        }
        let (
            workspace_capability,
            workspace_parent_capability,
            workspace_name,
            metadata_capability,
            checkout_capability,
            manifest_sha256,
            plan_sha256,
        ) = prepared_workspace.validation_capabilities();
        let verified_metadata = workspace::validate_prepared_workspace_capabilities(
            workspace_parent_capability,
            workspace_name,
            workspace_capability,
            metadata_capability,
            checkout_capability,
            manifest_sha256,
            plan_sha256,
            &context.repository_url,
            context.issue_number,
            &context.plan.artifact_id,
        )
        .map_err(PatchValidationError::Workspace)?;
        let workspace_binding = PatchWorkspaceBinding {
            workspace_object_id: workspace::directory_object_identity(workspace_capability)
                .map_err(PatchValidationError::Workspace)?,
            checkout_object_id: workspace::directory_object_identity(checkout_capability)
                .map_err(PatchValidationError::Workspace)?,
            manifest_sha256: manifest_sha256.to_string(),
            plan_sha256: plan_sha256.to_string(),
            repository_url: context.repository_url.clone(),
            checkout_commit: verified_metadata.checkout_commit,
            branch: verified_metadata.branch,
        };
        Ok(Self {
            workspace_parent: workspace_parent_capability.try_clone().map_err(|_| {
                PatchValidationError::Workspace(
                    "Could not retain workspace parent capability".into(),
                )
            })?,
            workspace_name: workspace_name.to_os_string(),
            workspace: workspace_capability.try_clone().map_err(|_| {
                PatchValidationError::Workspace("Could not retain workspace capability".into())
            })?,
            metadata: metadata_capability.try_clone().map_err(|_| {
                PatchValidationError::Workspace("Could not retain metadata capability".into())
            })?,
            checkout: checkout_capability.try_clone().map_err(|_| {
                PatchValidationError::Workspace("Could not retain checkout capability".into())
            })?,
            manifest_sha256: manifest_sha256.to_string(),
            plan_sha256: plan_sha256.to_string(),
            workspace_binding,
            context: context.clone(),
        })
    }

    pub(crate) fn try_clone(&self) -> Result<Self, PatchValidationError> {
        let clone_dir = |directory: &Dir, label: &str| {
            directory.try_clone().map_err(|_| {
                PatchValidationError::Workspace(format!("Could not retain {label} capability"))
            })
        };
        Ok(Self {
            workspace_parent: clone_dir(&self.workspace_parent, "workspace parent")?,
            workspace_name: self.workspace_name.clone(),
            workspace: clone_dir(&self.workspace, "workspace")?,
            metadata: clone_dir(&self.metadata, "metadata")?,
            checkout: clone_dir(&self.checkout, "checkout")?,
            manifest_sha256: self.manifest_sha256.clone(),
            plan_sha256: self.plan_sha256.clone(),
            workspace_binding: self.workspace_binding.clone(),
            context: self.context.clone(),
        })
    }

    pub(crate) fn open_patch_parent(
        &self,
        path: &str,
    ) -> Result<(Dir, OsString, String), PatchValidationError> {
        let (relative, display) = checked_path(path)?;
        let (parent, leaf) = open_parent_directory(&self.checkout, &relative, &display)?;
        Ok((parent, leaf, display))
    }

    pub(crate) fn validate_patch_context(
        &self,
        patch: &ValidatedPatch,
    ) -> Result<(), PatchValidationError> {
        if patch.workspace != self.workspace_binding {
            return Err(PatchValidationError::Workspace(
                "Validated patch belongs to a different prepared workspace".into(),
            ));
        }
        for change in patch.changes() {
            checked_path(change.path())?;
            if sha256(change.resulting_content().as_bytes()) != change.resulting_sha256() {
                return Err(PatchValidationError::InvalidProposal(format!(
                    "Result hash for `{}` does not match its content",
                    change.path()
                )));
            }
            if let Some(original) = change.original_content() {
                if Some(sha256(original.as_bytes()).as_str()) != change.original_sha256() {
                    return Err(PatchValidationError::InvalidProposal(format!(
                        "Original snapshot hash for `{}` does not match its content",
                        change.path()
                    )));
                }
                if change.original_file_identity().is_none() {
                    return Err(PatchValidationError::InvalidProposal(format!(
                        "Replacement `{}` has no original file identity",
                        change.path()
                    )));
                }
            } else if !change.is_new_file() {
                return Err(PatchValidationError::InvalidProposal(format!(
                    "Replacement `{}` has no original snapshot",
                    change.path()
                )));
            }
        }
        self.revalidate_patch_snapshot(patch)
    }

    /// Validate every operation atomically and calculate results without writing to disk.
    pub fn validate(
        &self,
        proposal: &PatchProposal,
    ) -> Result<ValidatedPatch, PatchValidationError> {
        self.revalidate_workspace()?;
        if proposal.schema_version != PATCH_PROPOSAL_SCHEMA_VERSION {
            return Err(PatchValidationError::InvalidProposal(format!(
                "Unsupported patch proposal schema version {}",
                proposal.schema_version
            )));
        }
        if proposal.files.is_empty() || proposal.files.len() > MAX_PATCH_FILES {
            return Err(PatchValidationError::LimitExceeded(format!(
                "Proposal must contain between 1 and {MAX_PATCH_FILES} files"
            )));
        }

        let mut seen = HashSet::new();
        let mut proposal_bytes = 0usize;
        let mut materialized_bytes = 0usize;
        let mut changes = Vec::with_capacity(proposal.files.len());
        let mut hash_inputs = Vec::with_capacity(proposal.files.len());

        for file_patch in &proposal.files {
            let file_size = preflight_size(file_patch, estimate_size)?;
            proposal_bytes = proposal_bytes.saturating_add(file_size);
            if proposal_bytes > MAX_TOTAL_PATCH_BYTES {
                return Err(limit_error(
                    "Proposal exceeds the 2 MiB total patch-data limit",
                ));
            }
            let (relative, display) = checked_path(&file_patch.path)?;
            if path_conflicts(&seen, &display)? {
                return Err(PatchValidationError::InvalidProposal(format!(
                    "Duplicate target path `{display}`"
                )));
            }
            seen.insert(display.clone());
            if !repository_tools::is_text_candidate(&relative) {
                return Err(PatchValidationError::UnsafePath(format!(
                    "Patch target `{display}` is not a supported text file"
                )));
            }
            let (parent_dir, leaf) = open_parent_directory(&self.checkout, &relative, &display)?;

            let (
                operation,
                original_content,
                original_file_identity,
                resulting_content,
                hash_input,
            ) = match &file_patch.operation {
                PatchOperation::Replace {
                    expected_original_sha256,
                    edits,
                } => {
                    validate_sha256(expected_original_sha256, &display)?;
                    let (original, original_identity) =
                        read_existing_text_with_identity(&parent_dir, &leaf, &display)?;
                    if original.len() > MAX_FILE_BYTES {
                        return Err(limit_error(&format!(
                            "Original file `{display}` exceeds the 1 MiB file limit"
                        )));
                    }
                    let actual_hash = sha256(original.as_bytes());
                    if !actual_hash.eq_ignore_ascii_case(expected_original_sha256) {
                        return Err(PatchValidationError::StaleFile(format!(
                            "Original SHA-256 for `{display}` does not match the proposal"
                        )));
                    }
                    materialized_bytes = materialized_bytes.saturating_add(original.len());
                    if materialized_bytes > MAX_TOTAL_PATCH_BYTES {
                        return Err(limit_error(
                            "Validated file contents exceed the 2 MiB total limit",
                        ));
                    }
                    let original_content = original.clone();
                    let mut content = original;
                    for edit in edits {
                        validate_edit(edit, &display)?;
                        let occurrences = overlapping_occurrence_count(&content, &edit.old_text);
                        if occurrences != 1 {
                            return Err(PatchValidationError::AmbiguousEdit(format!(
                                "Replacement in `{display}` matched {occurrences} times; exactly one match is required"
                            )));
                        }
                        let start = content
                            .find(&edit.old_text)
                            .expect("match count proved one occurrence");
                        let end = start + edit.old_text.len();
                        content.replace_range(start..end, &edit.new_text);
                        if content.len() > MAX_FILE_BYTES {
                            return Err(limit_error(&format!(
                                "Resulting file `{display}` exceeds the 1 MiB file limit"
                            )));
                        }
                    }
                    if content == original_content {
                        return Err(PatchValidationError::InvalidProposal(format!(
                            "Replacement in `{display}` would not change the file"
                        )));
                    }
                    let input = HashFile::Replace {
                        path: display.clone(),
                        original_sha256: actual_hash.clone(),
                        original_file_identity: original_identity.clone(),
                        edits: edits.clone(),
                        resulting_sha256: sha256(content.as_bytes()),
                    };
                    (
                        ValidatedOperation::Replace {
                            original_sha256: actual_hash,
                            edits: edits.len(),
                        },
                        Some(original_content),
                        Some(original_identity),
                        content,
                        input,
                    )
                }
                PatchOperation::Create { content } => {
                    if content.len() > MAX_FILE_BYTES {
                        return Err(limit_error(&format!(
                            "New file `{display}` exceeds the 1 MiB file limit"
                        )));
                    }
                    if content.as_bytes().contains(&0) {
                        return Err(PatchValidationError::File(format!(
                            "New file `{display}` contains a NUL byte"
                        )));
                    }
                    if content.len() > MAX_TOTAL_PATCH_BYTES {
                        return Err(limit_error(
                            "Proposal exceeds the 2 MiB total patch-data limit",
                        ));
                    }
                    match parent_dir.symlink_metadata(&leaf) {
                        Ok(_) => {
                            return Err(PatchValidationError::File(format!(
                                "Create destination `{display}` already exists"
                            )));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {
                            return Err(PatchValidationError::File(format!(
                                "Could not safely inspect create destination `{display}`"
                            )));
                        }
                    }
                    let hash = sha256(content.as_bytes());
                    (
                        ValidatedOperation::Create,
                        None,
                        None,
                        content.clone(),
                        HashFile::Create {
                            path: display.clone(),
                            resulting_sha256: hash,
                            content: content.clone(),
                        },
                    )
                }
            };

            materialized_bytes = materialized_bytes.saturating_add(resulting_content.len());
            if materialized_bytes > MAX_TOTAL_PATCH_BYTES {
                return Err(limit_error(
                    "Validated file contents exceed the 2 MiB total limit",
                ));
            }
            let resulting_sha256 = sha256(resulting_content.as_bytes());
            changes.push(ValidatedFileChange {
                path: display,
                operation,
                original_content,
                original_file_identity,
                resulting_content,
                resulting_sha256,
            });
            hash_inputs.push(hash_input);
        }

        changes.sort_by(|left, right| left.path().cmp(right.path()));
        hash_inputs.sort_by(|left, right| left.path().cmp(right.path()));
        let hash_bytes = serde_json::to_vec(&HashPayload {
            schema_version: PATCH_PROPOSAL_SCHEMA_VERSION,
            files: hash_inputs,
        })
        .map_err(|_| {
            PatchValidationError::InvalidProposal("Could not encode patch identity".into())
        })?;
        let preview = make_preview(&changes)?;
        let files_created = changes.iter().filter(|change| change.is_new_file()).count();
        let replacements = changes.iter().map(ValidatedFileChange::edit_count).sum();
        let summary = PatchSummary {
            files_changed: changes.len(),
            files_created,
            replacements,
            preview,
        };
        Ok(ValidatedPatch {
            patch_sha256: sha256(&hash_bytes),
            changes,
            summary,
            workspace: self.workspace_binding.clone(),
        })
    }

    /// Recheck that a validated patch still describes the current targets in this live workspace.
    /// This is used immediately before and after interactive approval; it does not authorize writes.
    #[allow(dead_code)] // Exercised by the approval foundation; CLI orchestration follows in 4D.2.3.
    pub(crate) fn revalidate_patch_snapshot(
        &self,
        patch: &ValidatedPatch,
    ) -> Result<(), PatchValidationError> {
        if patch.workspace != self.workspace_binding {
            return Err(PatchValidationError::Workspace(
                "Validated patch belongs to a different prepared workspace".into(),
            ));
        }
        self.revalidate_workspace()?;
        for change in patch.changes() {
            let (relative, display) = checked_path(change.path())?;
            let (parent, leaf) = open_parent_directory(&self.checkout, &relative, &display)?;
            if let Some(expected_hash) = change.original_sha256() {
                let (actual, actual_identity) =
                    read_existing_text_with_identity(&parent, &leaf, &display)?;
                if sha256(actual.as_bytes()) != expected_hash {
                    return Err(PatchValidationError::StaleFile(format!(
                        "Patch target `{display}` changed after validation"
                    )));
                }
                if Some(actual_identity.as_str()) != change.original_file_identity() {
                    return Err(PatchValidationError::StaleFile(format!(
                        "Patch target `{display}` file identity changed after validation"
                    )));
                }
            } else {
                match parent.symlink_metadata(&leaf) {
                    Ok(_) => {
                        return Err(PatchValidationError::StaleFile(format!(
                            "Create destination `{display}` appeared after validation"
                        )));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {
                        return Err(PatchValidationError::File(format!(
                            "Could not safely inspect create destination `{display}`"
                        )));
                    }
                }
            }
        }
        self.revalidate_workspace()
    }

    pub(crate) fn revalidate_workspace(&self) -> Result<(), PatchValidationError> {
        let verified_metadata = workspace::validate_prepared_workspace_capabilities(
            &self.workspace_parent,
            &self.workspace_name,
            &self.workspace,
            &self.metadata,
            &self.checkout,
            &self.manifest_sha256,
            &self.plan_sha256,
            &self.context.repository_url,
            self.context.issue_number,
            &self.context.plan.artifact_id,
        )
        .map_err(PatchValidationError::Workspace)?;
        if verified_metadata.checkout_commit != self.workspace_binding.checkout_commit
            || verified_metadata.branch != self.workspace_binding.branch
        {
            return Err(PatchValidationError::Workspace(
                "Prepared checkout commit or branch changed after validation".into(),
            ));
        }
        Ok(())
    }
}

fn checked_path(value: &str) -> Result<(PathBuf, String), PatchValidationError> {
    let path = repository_tools::safe_relative_path(value).map_err(|error| {
        PatchValidationError::UnsafePath(format!("Unsafe patch path: {}", error.message))
    })?;
    if repository_tools::is_excluded(&path) {
        return Err(PatchValidationError::UnsafePath(
            "Patch path is excluded from repository access".into(),
        ));
    }
    let display = path.to_string_lossy().replace('\\', "/");
    Ok((path, display))
}

fn preflight_edit_count(file: &FilePatch) -> Result<(), PatchValidationError> {
    if let PatchOperation::Replace { edits, .. } = &file.operation
        && (edits.is_empty() || edits.len() > 64)
    {
        return Err(PatchValidationError::InvalidProposal(format!(
            "Replacement `{}` must contain between 1 and 64 edits",
            file.path
        )));
    }
    Ok(())
}

fn preflight_size(
    file: &FilePatch,
    estimate: impl FnOnce(&FilePatch) -> usize,
) -> Result<usize, PatchValidationError> {
    preflight_edit_count(file)?;
    Ok(estimate(file))
}

fn overlapping_occurrence_count(content: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let Some(first) = content.find(needle) else {
        return 0;
    };
    let next_start = first + content[first..].chars().next().unwrap().len_utf8();
    if content[next_start..].contains(needle) {
        2
    } else {
        1
    }
}

fn path_conflicts(
    previous: &HashSet<String>,
    candidate: &str,
) -> Result<bool, PatchValidationError> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
        let candidate_wide: Vec<u16> = candidate.encode_utf16().collect();
        for path in previous {
            let path_wide: Vec<u16> = path.encode_utf16().collect();
            let equal = unsafe {
                CompareStringOrdinal(
                    path_wide.as_ptr(),
                    path_wide.len() as i32,
                    candidate_wide.as_ptr(),
                    candidate_wide.len() as i32,
                    1,
                )
            };
            if equal == 0 {
                return Err(PatchValidationError::UnsafePath(
                    "Could not determine Windows path equivalence safely".into(),
                ));
            }
            if equal == CSTR_EQUAL {
                return Ok(true);
            }
        }
        Ok(false)
    }
    #[cfg(not(windows))]
    {
        Ok(previous.contains(candidate))
    }
}

fn open_parent_directory(
    root: &Dir,
    relative: &Path,
    display: &str,
) -> Result<(Dir, OsString), PatchValidationError> {
    let mut directory = root.try_clone().map_err(|_| {
        PatchValidationError::Workspace("Could not duplicate workspace capability".into())
    })?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(PatchValidationError::UnsafePath(format!(
                "Patch path `{display}` contains an invalid component"
            )));
        };
        if components.peek().is_none() {
            return Ok((directory, name.to_os_string()));
        }
        // Each component is opened relative to the held handle with an atomic no-follow flag.
        directory = workspace::open_dir_child_nofollow(&directory, name).map_err(|_| {
            PatchValidationError::UnsafePath(format!(
                "Patch path `{display}` traverses an unsafe directory"
            ))
        })?;
    }
    Err(PatchValidationError::UnsafePath(
        "Patch path has no file name".into(),
    ))
}

fn read_existing_text_with_identity(
    parent: &Dir,
    leaf: &std::ffi::OsStr,
    display: &str,
) -> Result<(String, String), PatchValidationError> {
    let mut file = workspace::open_file_child_nofollow(parent, leaf).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PatchValidationError::File(format!("Patch target `{display}` does not exist"))
        } else {
            PatchValidationError::UnsafePath(format!(
                "Patch target `{display}` could not be opened without following links"
            ))
        }
    })?;
    let opened_metadata = file.metadata().map_err(|_| {
        PatchValidationError::File(format!("Could not inspect opened patch target `{display}`"))
    })?;
    if repository_tools::has_multiple_links(&file, &opened_metadata) {
        return Err(PatchValidationError::UnsafePath(format!(
            "Patch target `{display}` is linked or is not a regular file"
        )));
    }
    let identity =
        workspace::file_object_identity(&file).map_err(PatchValidationError::Workspace)?;
    let mut bytes = Vec::with_capacity(opened_metadata.len().min(MAX_FILE_BYTES as u64) as usize);
    file.by_ref()
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            PatchValidationError::File(format!("Could not read patch target `{display}`"))
        })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(limit_error(&format!(
            "Patch target `{display}` exceeds the 1 MiB file limit"
        )));
    }
    if bytes.contains(&0) {
        return Err(PatchValidationError::File(format!(
            "Patch target `{display}` is binary"
        )));
    }
    let content = String::from_utf8(bytes).map_err(|_| {
        PatchValidationError::File(format!("Patch target `{display}` is not UTF-8"))
    })?;
    Ok((content, identity))
}

fn validate_edit(edit: &TextEdit, path: &str) -> Result<(), PatchValidationError> {
    if edit.old_text.is_empty() {
        return Err(PatchValidationError::InvalidProposal(format!(
            "Replacement in `{path}` has empty old_text"
        )));
    }
    if edit.old_text.len() > MAX_EDIT_BYTES || edit.new_text.len() > MAX_EDIT_BYTES {
        return Err(limit_error(&format!(
            "An edit in `{path}` exceeds the 64 KiB per-edit limit"
        )));
    }
    if edit.old_text.as_bytes().contains(&0) || edit.new_text.as_bytes().contains(&0) {
        return Err(PatchValidationError::InvalidProposal(format!(
            "An edit in `{path}` contains a NUL byte"
        )));
    }
    Ok(())
}

fn validate_sha256(value: &str, path: &str) -> Result<(), PatchValidationError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PatchValidationError::InvalidProposal(format!(
            "Expected original SHA-256 for `{path}` must be 64 hexadecimal characters"
        )));
    }
    Ok(())
}

fn estimate_size(file: &FilePatch) -> usize {
    let mut size = file.path.len().saturating_add(64);
    match &file.operation {
        PatchOperation::Replace {
            expected_original_sha256,
            edits,
        } => {
            size = size.saturating_add(expected_original_sha256.len());
            for edit in edits {
                size = size
                    .saturating_add(edit.old_text.len())
                    .saturating_add(edit.new_text.len());
            }
        }
        PatchOperation::Create { content } => size = size.saturating_add(content.len()),
    }
    size
}

fn make_preview(changes: &[ValidatedFileChange]) -> Result<String, PatchValidationError> {
    let mut preview = String::new();
    for change in changes {
        let path = escape_terminal_text(&change.path)?;
        if change.is_new_file() {
            append_preview(&mut preview, "--- /dev/null\n")?;
            append_preview(&mut preview, &format!("+++ b/{path}\n"))?;
            let line_count = diff_line_count(&change.resulting_content);
            if line_count > 0 {
                append_preview(&mut preview, &format!("@@ -0,0 +1,{line_count} @@\n"))?;
            }
            append_diff_lines(&mut preview, '+', &change.resulting_content)?;
        } else {
            let original = change.original_content().unwrap_or_default();
            append_preview(&mut preview, &format!("--- a/{path}\n+++ b/{path}\n"))?;
            let original_lines = diff_line_count(original);
            let resulting_lines = diff_line_count(&change.resulting_content);
            append_preview(
                &mut preview,
                &format!(
                    "@@ -{},{} +{},{} @@\n",
                    if original_lines == 0 { 0 } else { 1 },
                    original_lines,
                    if resulting_lines == 0 { 0 } else { 1 },
                    resulting_lines,
                ),
            )?;
            append_diff_lines(&mut preview, '-', original)?;
            append_diff_lines(&mut preview, '+', &change.resulting_content)?;
        }
    }
    Ok(preview)
}

fn diff_line_count(content: &str) -> usize {
    content.split_inclusive('\n').count()
}

fn append_diff_lines(
    output: &mut String,
    marker: char,
    content: &str,
) -> Result<(), PatchValidationError> {
    for line in content.split_inclusive('\n') {
        append_preview(output, &marker.to_string())?;
        let has_newline = line.ends_with('\n');
        let body = if has_newline {
            &line[..line.len() - 1]
        } else {
            line
        };
        append_escaped_body(output, body)?;
        append_preview(output, "\n")?;
        if !has_newline {
            append_preview(output, "\\ No newline at end of file\n")?;
        }
    }
    Ok(())
}

pub(crate) fn escape_terminal_text(value: &str) -> Result<String, PatchValidationError> {
    let mut escaped = String::new();
    append_escaped_body(&mut escaped, value)?;
    Ok(escaped)
}

fn append_escaped_body(output: &mut String, value: &str) -> Result<(), PatchValidationError> {
    let remaining = MAX_PREVIEW_BYTES.saturating_sub(output.len());
    let escaped = crate::terminal::escape::patch_preview(value, remaining)
        .map_err(|_| limit_error("Human-readable patch preview exceeds 4 MiB"))?;
    append_preview(output, &escaped)
}

fn append_preview(output: &mut String, value: &str) -> Result<(), PatchValidationError> {
    if output.len().saturating_add(value.len()) > MAX_PREVIEW_BYTES {
        return Err(limit_error("Human-readable patch preview exceeds 4 MiB"));
    }
    output.push_str(value);
    Ok(())
}

#[derive(Serialize)]
struct HashPayload {
    schema_version: u32,
    files: Vec<HashFile>,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum HashFile {
    Replace {
        path: String,
        original_sha256: String,
        original_file_identity: String,
        edits: Vec<TextEdit>,
        resulting_sha256: String,
    },
    Create {
        path: String,
        resulting_sha256: String,
        content: String,
    },
}

impl HashFile {
    fn path(&self) -> &str {
        match self {
            Self::Replace { path, .. } | Self::Create { path, .. } => path,
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn limit_error(message: &str) -> PatchValidationError {
    PatchValidationError::LimitExceeded(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::{ContributionPlanReference, PlanReviewStatus};
    use crate::github::GitHubRepository;
    use crate::planning::ContributionPlan;
    use crate::workspace::{create_workspace_from_source_for_tests, preview_workspace_for_tests};
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    const ORIGINAL: &str = "alpha\nneedle\nomega\n";

    struct Fixture {
        root: PathBuf,
        checkout: PathBuf,
        prepared: WorkspaceResult,
        context: ContributionContext,
    }

    impl Fixture {
        fn new() -> Self {
            let serial = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hecate-patch-validation-{}-{serial}",
                std::process::id()
            ));
            let source = root.join("source");
            fs::create_dir_all(source.join("src")).unwrap();
            let git = |args: &[&str]| {
                Command::new("git")
                    .args(args)
                    .current_dir(&source)
                    .output()
                    .unwrap()
            };
            assert!(git(&["init", "-b", "main"]).status.success());
            assert!(
                git(&["config", "user.name", "HECATE Tests"])
                    .status
                    .success()
            );
            assert!(
                git(&["config", "user.email", "tests@example.invalid"])
                    .status
                    .success()
            );
            fs::write(source.join("src/lib.rs"), ORIGINAL).unwrap();
            fs::write(source.join("README.md"), "fixture repository\n").unwrap();
            fs::write(source.join(".env"), "TOKEN=private\n").unwrap();
            assert!(git(&["add", "."]).status.success());
            assert!(git(&["commit", "-m", "initial"]).status.success());
            let commit = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_string();
            let repository = GitHubRepository {
                owner: "example".into(),
                name: "patch-test".into(),
                default_branch: "main".into(),
                commit,
                url: "https://github.com/example/patch-test".into(),
            };
            let plan = ContributionPlan {
                repository_url: repository.url.clone(),
                issue_number: 9,
                issue_title: "Update behavior".into(),
                issue_url: "https://github.com/example/patch-test/issues/9".into(),
                labels: vec![],
                created_at: "now".into(),
                updated_at: "now".into(),
                comments: 0,
                expected_outcome: "correct behavior".into(),
                interpretation: "update source".into(),
                locations: vec![],
                documents: vec![],
                steps: vec![],
                test_recommendations: vec![],
                validation_commands: vec![],
                risks_and_assumptions: vec![],
                unresolved_questions: vec![],
            };
            let workspace_root = root.join("workspaces");
            let preview = preview_workspace_for_tests(&repository, 9, &workspace_root).unwrap();
            let workspace = create_workspace_from_source_for_tests(
                &repository,
                &plan,
                &preview,
                source.to_str().unwrap(),
            )
            .unwrap();
            Self {
                root,
                checkout: workspace.checkout_path.clone(),
                prepared: workspace,
                context: ContributionContext {
                    repository_url: repository.url.clone(),
                    issue_number: 9,
                    issue_title: plan.issue_title,
                    plan: ContributionPlanReference {
                        artifact_id: ".hecate/plan-v1.json".into(),
                        repository_url: repository.url,
                        issue_number: 9,
                        schema_version: workspace::PLAN_SCHEMA_VERSION,
                        review_status: PlanReviewStatus::Approved,
                    },
                },
            }
        }

        fn validator(&self) -> PatchValidator {
            PatchValidator::new(&self.prepared, &self.context).unwrap()
        }

        fn cleanup(self) {
            let root = self.root.clone();
            drop(self);
            let _ = fs::remove_dir_all(root);
        }
    }

    fn replacement(path: &str, content: &str, edits: Vec<TextEdit>) -> FilePatch {
        FilePatch {
            path: path.into(),
            operation: PatchOperation::Replace {
                expected_original_sha256: sha256(content.as_bytes()),
                edits,
            },
        }
    }

    fn replace_one(old: &str, new: &str) -> TextEdit {
        TextEdit {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn validates_exact_and_sequential_replacements_with_summary_and_hashes() {
        let fixture = Fixture::new();
        let result = fixture
            .validator()
            .validate(&PatchProposal::new(vec![replacement(
                "src/lib.rs",
                ORIGINAL,
                vec![
                    replace_one("needle", "first"),
                    replace_one("first", "final"),
                ],
            )]))
            .unwrap();
        let change = &result.changes()[0];
        assert_eq!(change.original_content(), Some(ORIGINAL));
        assert_eq!(change.resulting_content(), "alpha\nfinal\nomega\n");
        assert_eq!(
            change.resulting_sha256(),
            sha256(change.resulting_content().as_bytes())
        );
        assert_eq!(change.edit_count(), 2);
        assert_eq!(result.summary().files_changed, 1);
        assert_eq!(result.summary().replacements, 2);
        assert!(result.summary().preview.contains("needle"));
        assert!(result.summary().preview.contains("final"));
        assert_eq!(result.patch_sha256().len(), 64);
        assert_eq!(
            fs::read(fixture.checkout.join("src/lib.rs")).unwrap(),
            ORIGINAL.as_bytes()
        );
        fixture.cleanup();
    }

    #[test]
    fn validates_create_without_writing_and_hash_is_order_independent() {
        let fixture = Fixture::new();
        let first = FilePatch {
            path: "src/new.rs".into(),
            operation: PatchOperation::Create {
                content: "pub fn added() {}\n".into(),
            },
        };
        let second = replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "new")]);
        let validator = fixture.validator();
        let a = validator
            .validate(&PatchProposal::new(vec![first.clone(), second.clone()]))
            .unwrap();
        let b = validator
            .validate(&PatchProposal::new(vec![second, first]))
            .unwrap();
        assert_eq!(a.patch_sha256(), b.patch_sha256());
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(a.summary().files_created, 1);
        assert!(!fixture.checkout.join("src/new.rs").exists());
        assert!(
            a.summary()
                .preview
                .contains("--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+pub fn added() {}")
        );
        fixture.cleanup();
    }

    #[test]
    fn rejects_zero_and_multiple_matches_and_stale_or_changed_files() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let zero = replacement("src/lib.rs", ORIGINAL, vec![replace_one("absent", "x")]);
        let err = validator
            .validate(&PatchProposal::new(vec![zero]))
            .unwrap_err();
        assert!(matches!(err, PatchValidationError::AmbiguousEdit(_)));
        fs::write(fixture.checkout.join("src/lib.rs"), "alpha\nalpha\n").unwrap();
        let multiple = replacement(
            "src/lib.rs",
            "alpha\nalpha\n",
            vec![replace_one("alpha", "x")],
        );
        let err = validator
            .validate(&PatchProposal::new(vec![multiple]))
            .unwrap_err();
        assert!(matches!(err, PatchValidationError::AmbiguousEdit(_)));
        let stale = FilePatch {
            path: "src/lib.rs".into(),
            operation: PatchOperation::Replace {
                expected_original_sha256: "0".repeat(64),
                edits: vec![replace_one("needle", "x")],
            },
        };
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![stale])),
            Err(PatchValidationError::StaleFile(_))
        ));
        fs::write(fixture.checkout.join("src/lib.rs"), "changed\n").unwrap();
        let changed = replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "x")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![changed])),
            Err(PatchValidationError::StaleFile(_))
        ));
        fixture.cleanup();
    }

    #[test]
    fn rejects_overlapping_and_empty_matches_but_accepts_unique_match() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        fs::write(fixture.checkout.join("src/lib.rs"), "aaa").unwrap();
        let overlap = replacement("src/lib.rs", "aaa", vec![replace_one("aa", "x")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![overlap])),
            Err(PatchValidationError::AmbiguousEdit(_))
        ));
        fs::write(fixture.checkout.join("src/lib.rs"), ORIGINAL).unwrap();
        let empty = replacement("src/lib.rs", ORIGINAL, vec![replace_one("", "x")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![empty])),
            Err(PatchValidationError::InvalidProposal(_))
        ));
        let unique = replacement(
            "src/lib.rs",
            ORIGINAL,
            vec![replace_one("needle", "unique")],
        );
        assert!(
            validator
                .validate(&PatchProposal::new(vec![unique]))
                .is_ok()
        );
        fixture.cleanup();
    }

    #[test]
    fn validator_uses_prepared_capability_when_public_paths_are_substituted() {
        let mut fixture = Fixture::new();
        let validator = fixture.validator();
        let decoy = fixture.root.join("unapproved");
        fs::create_dir_all(decoy.join("src")).unwrap();
        fs::write(decoy.join("src/lib.rs"), "decoy\n").unwrap();
        fixture.prepared.path = decoy.clone();
        fixture.prepared.checkout_path = decoy.clone();
        let proposal = replacement(
            "src/lib.rs",
            ORIGINAL,
            vec![replace_one("needle", "approved")],
        );
        let validated = validator
            .validate(&PatchProposal::new(vec![proposal]))
            .unwrap();
        assert_eq!(validated.changes()[0].original_content(), Some(ORIGINAL));
        assert_eq!(fs::read(decoy.join("src/lib.rs")).unwrap(), b"decoy\n");
        fixture.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn validator_rejects_workspace_root_entry_replacement() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let moved = fixture.root.join("prepared-original");
        fs::rename(&fixture.prepared.path, &moved).unwrap();
        fs::create_dir_all(fixture.prepared.path.join("repository/src")).unwrap();
        fs::write(
            fixture.prepared.path.join("repository/src/lib.rs"),
            "decoy\n",
        )
        .unwrap();

        let proposal = replacement(
            "src/lib.rs",
            ORIGINAL,
            vec![replace_one("needle", "approved")],
        );
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![proposal])),
            Err(PatchValidationError::Workspace(_))
        ));
        assert_eq!(
            fs::read(fixture.prepared.path.join("repository/src/lib.rs")).unwrap(),
            b"decoy\n"
        );
        let _ = fs::remove_dir_all(moved);
        fixture.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn parent_directory_replacement_cannot_redirect_a_validated_read() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let src = fixture.checkout.join("src");
        let saved = fixture.checkout.join("src-original");
        let outside = fixture.root.join("outside-parent");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("lib.rs"), ORIGINAL).unwrap();
        fs::rename(&src, &saved).unwrap();
        symlink(&outside, &src).unwrap();
        let proposal = replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "x")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![proposal])),
            Err(PatchValidationError::UnsafePath(_))
        ));
        assert_eq!(
            fs::read(outside.join("lib.rs")).unwrap(),
            ORIGINAL.as_bytes()
        );
        fixture.cleanup();
    }

    #[test]
    fn modified_prepared_metadata_is_rejected() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        fs::write(fixture.prepared.plan_path.clone(), b"{}").unwrap();
        let proposal = replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "x")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![proposal])),
            Err(PatchValidationError::Workspace(_))
        ));
        fixture.cleanup();
    }

    #[test]
    fn edit_count_limit_is_checked_before_size_estimation() {
        let file = replacement(
            "src/lib.rs",
            ORIGINAL,
            (0..65).map(|_| replace_one("needle", "x")).collect(),
        );
        let result = preflight_size(&file, |_| panic!("size estimation ran before edit limit"));
        assert!(matches!(
            result,
            Err(PatchValidationError::InvalidProposal(_))
        ));
    }

    #[test]
    fn rejects_missing_duplicate_and_unsafe_paths() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let missing = replacement("src/missing.rs", "x", vec![replace_one("x", "y")]);
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![missing])),
            Err(PatchValidationError::File(_))
        ));
        let duplicate = PatchProposal::new(vec![
            replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "x")]),
            replacement("src\\lib.rs", ORIGINAL, vec![replace_one("needle", "y")]),
        ]);
        assert!(matches!(
            validator.validate(&duplicate),
            Err(PatchValidationError::InvalidProposal(_))
        ));
        for path in [
            "../outside.rs",
            "/etc/passwd.rs",
            "C:\\temp\\x.rs",
            "src/file:stream.rs",
            "src/CON.rs",
            "src/trailing. ",
            ".git/config",
            ".hecate/plan-v1.json",
            ".env",
        ] {
            let proposal = PatchProposal::new(vec![replacement(
                path,
                ORIGINAL,
                vec![replace_one("needle", "x")],
            )]);
            assert!(validator.validate(&proposal).is_err(), "accepted {path}");
        }
        fixture.cleanup();
    }

    #[test]
    fn rejects_linked_binary_invalid_utf8_and_oversized_targets() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let external = fixture.root.join("external.rs");
        fs::write(&external, ORIGINAL).unwrap();
        let hard_link = fixture.checkout.join("src/hard.rs");
        fs::hard_link(&external, &hard_link).unwrap();
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![replacement(
                "src/hard.rs",
                ORIGINAL,
                vec![replace_one("needle", "x")]
            )])),
            Err(PatchValidationError::UnsafePath(_))
        ));

        fs::write(fixture.checkout.join("src/binary.rs"), [0, 1, 2]).unwrap();
        assert!(
            validator
                .validate(&PatchProposal::new(vec![replacement(
                    "src/binary.rs",
                    "x",
                    vec![replace_one("x", "y")]
                )]))
                .is_err()
        );
        fs::write(fixture.checkout.join("src/invalid.rs"), [0xff, 0xfe]).unwrap();
        assert!(
            validator
                .validate(&PatchProposal::new(vec![replacement(
                    "src/invalid.rs",
                    "x",
                    vec![replace_one("x", "y")]
                )]))
                .is_err()
        );
        fs::write(
            fixture.checkout.join("src/large.rs"),
            vec![b'x'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(matches!(
            validator.validate(&PatchProposal::new(vec![replacement(
                "src/large.rs",
                "x",
                vec![replace_one("x", "y")]
            )])),
            Err(PatchValidationError::LimitExceeded(_))
        ));
        fixture.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_file_and_directory_escapes() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let external = fixture.root.join("external.rs");
        fs::write(&external, ORIGINAL).unwrap();
        symlink(&external, fixture.checkout.join("src/link.rs")).unwrap();
        symlink(
            fixture.root.join("outside-dir"),
            fixture.checkout.join("src/junction"),
        )
        .unwrap();
        let dir = fixture.root.join("outside-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("escape.rs"), ORIGINAL).unwrap();
        for path in ["src/link.rs", "src/junction/escape.rs"] {
            assert!(matches!(
                validator.validate(&PatchProposal::new(vec![replacement(
                    path,
                    ORIGINAL,
                    vec![replace_one("needle", "x")]
                )])),
                Err(PatchValidationError::UnsafePath(_))
            ));
        }
        fixture.cleanup();
    }

    #[test]
    fn rejects_invalid_create_destinations_and_entire_proposal_atomically() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        for (path, content) in [
            ("src/lib.rs", "overwrite"),
            ("src/missing-dir/new.rs", "no directory creation"),
        ] {
            assert!(
                validator
                    .validate(&PatchProposal::new(vec![FilePatch {
                        path: path.into(),
                        operation: PatchOperation::Create {
                            content: content.into()
                        },
                    }]))
                    .is_err()
            );
        }
        let good = replacement("src/lib.rs", ORIGINAL, vec![replace_one("needle", "x")]);
        let bad = replacement("src/missing.rs", ORIGINAL, vec![replace_one("needle", "x")]);
        assert!(
            validator
                .validate(&PatchProposal::new(vec![good, bad]))
                .is_err()
        );
        assert_eq!(
            fs::read(fixture.checkout.join("src/lib.rs")).unwrap(),
            ORIGINAL.as_bytes()
        );
        assert!(!fixture.checkout.join("src/new.rs").exists());
        fixture.cleanup();
    }

    #[test]
    fn enforces_proposal_limits_and_approved_workspace_binding() {
        let fixture = Fixture::new();
        let validator = fixture.validator();
        let huge = PatchProposal::new(vec![replacement(
            "src/lib.rs",
            ORIGINAL,
            vec![replace_one("needle", &"x".repeat(MAX_EDIT_BYTES + 1))],
        )]);
        assert!(matches!(
            validator.validate(&huge),
            Err(PatchValidationError::LimitExceeded(_))
        ));
        let mut pending = fixture.context.clone();
        pending.plan.review_status = PlanReviewStatus::PendingHumanReview;
        assert!(matches!(
            PatchValidator::new(&fixture.prepared, &pending),
            Err(PatchValidationError::Workspace(_))
        ));
        let too_many = PatchProposal::new(
            (0..MAX_PATCH_FILES + 1)
                .map(|index| FilePatch {
                    path: format!("src/{index}.rs"),
                    operation: PatchOperation::Create {
                        content: String::new(),
                    },
                })
                .collect(),
        );
        assert!(matches!(
            validator.validate(&too_many),
            Err(PatchValidationError::LimitExceeded(_))
        ));
        let oversized = PatchProposal::new(
            (0..3)
                .map(|index| FilePatch {
                    path: format!("src/large-{index}.rs"),
                    operation: PatchOperation::Create {
                        content: "x".repeat(700_000),
                    },
                })
                .collect(),
        );
        assert!(matches!(
            validator.validate(&oversized),
            Err(PatchValidationError::LimitExceeded(_))
        ));
        fixture.cleanup();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_junction_escape() {
        let fixture = Fixture::new();
        let outside = fixture.root.join("junction-target");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("escape.rs"), ORIGINAL).unwrap();
        let junction = fixture.checkout.join("src/junction");
        let linked = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:HECATE_TEST_JUNCTION -Target $env:HECATE_TEST_JUNCTION_TARGET | Out-Null",
            ])
            .env("HECATE_TEST_JUNCTION", &junction)
            .env("HECATE_TEST_JUNCTION_TARGET", &outside)
            .output()
            .expect("junction creation command must start");
        assert!(
            linked.status.success(),
            "junction creation unexpectedly failed: {}{}",
            String::from_utf8_lossy(&linked.stdout),
            String::from_utf8_lossy(&linked.stderr),
        );
        assert!(
            fixture
                .validator()
                .validate(&PatchProposal::new(vec![replacement(
                    "src/junction/escape.rs",
                    ORIGINAL,
                    vec![replace_one("needle", "blocked")],
                )]))
                .is_err()
        );
        fixture.cleanup();
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_unicode_and_ascii_case_aliases() {
        let mut paths = HashSet::new();
        assert!(!path_conflicts(&paths, "src/Ä.rs").unwrap());
        paths.insert("src/Ä.rs".to_string());
        assert!(path_conflicts(&paths, "src/ä.rs").unwrap());
        paths.clear();
        paths.insert("src/A.rs".to_string());
        assert!(path_conflicts(&paths, "src/a.rs").unwrap());
    }
}
