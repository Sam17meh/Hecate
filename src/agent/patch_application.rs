//! One-use application of a human-approved, validated patch.
//!
//! This is deliberately a narrow file operation layer. It is not an agent tool or a general
//! filesystem API. Multi-file application is not atomic; failures trigger verified reverse-order
//! rollback where safe and preserve recovery artifacts otherwise.

#![allow(dead_code)] // No user-facing apply workflow is part of this milestone.

use crate::agent::patch::{ValidatedFileChange, ValidatedPatch};
use crate::agent::patch_approval::{ApprovedPatch, verify_record_binding};
use crate::agent::patch_validation::PatchValidator;
use crate::agent::repository_tools;
use crate::workspace;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_FILES: usize = 20;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;
static NEXT_ARTIFACT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplicationStatus {
    AppliedAndVerified,
    FailedBeforeModification,
    FailedAndRolledBack,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationStatus {
    NotApplied,
    AppliedAndVerified,
    RestoredAndVerified,
    RemainsApplied,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OperationReport {
    pub(crate) path: String,
    pub(crate) status: OperationStatus,
    pub(crate) original_sha256: Option<String>,
    pub(crate) expected_sha256: String,
    pub(crate) actual_sha256: Option<String>,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplicationReport {
    pub(crate) status: ApplicationStatus,
    pub(crate) error: Option<String>,
    pub(crate) operations: Vec<OperationReport>,
    /// Relative paths of preserved recovery files. These must not be deleted automatically.
    pub(crate) recovery_artifacts: Vec<String>,
}

struct Target {
    parent: Dir,
    leaf: OsString,
    display: String,
    original_identity: Option<String>,
}

struct Artifact {
    parent: Dir,
    name: OsString,
    display: String,
    identity: String,
    sha256: String,
    file: Option<std::fs::File>,
}

struct StagedOperation {
    target: Target,
    stage: Artifact,
    backup: Option<Artifact>,
    expected_sha256: String,
    original_sha256: Option<String>,
    is_create: bool,
    applied: bool,
    operation_report_index: usize,
}

struct StageFailure {
    message: String,
    recovery_artifacts: Vec<String>,
}

impl From<String> for StageFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            recovery_artifacts: Vec::new(),
        }
    }
}

impl From<&str> for StageFailure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// Consume the authorization value. Any error also consumes it; retry requires fresh validation
/// and explicit approval. No group-atomicity is promised.
pub(crate) fn apply_approved_patch(approved: ApprovedPatch) -> ApplicationReport {
    apply_with_fault(approved, Fault::None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    #[cfg(test)]
    StageAt(usize),
    #[cfg(test)]
    CorruptStageAt(usize),
    #[cfg(test)]
    ApplyAt(usize),
    #[cfg(test)]
    ApplyThenRollback {
        apply_at: usize,
        rollback_at: usize,
    },
    #[cfg(test)]
    TamperThenApplyFailure {
        apply_at: usize,
        tamper_at: usize,
    },
    #[cfg(test)]
    VerifyAt(usize),
    #[cfg(test)]
    RollbackAt(usize),
}

fn apply_with_fault(approved: ApprovedPatch, fault: Fault) -> ApplicationReport {
    let (patch, record, validator) = approved.into_parts();
    let mut report = ApplicationReport {
        status: ApplicationStatus::FailedBeforeModification,
        error: None,
        operations: patch
            .changes()
            .iter()
            .map(|change| operation_report(change, OperationStatus::NotApplied))
            .collect(),
        recovery_artifacts: Vec::new(),
    };

    if let Err(error) = verify_record_binding(&record, &patch) {
        return fail_before(report, format!("Approval binding rejected: {error}"));
    }

    if let Err(error) = validate_limits(&patch) {
        return fail_before(report, error);
    }
    if let Err(error) = validator.validate_patch_context(&patch) {
        return fail_before(
            report,
            format!("Patch/workspace revalidation failed: {error}"),
        );
    }

    let mut staged = Vec::with_capacity(patch.changes().len());
    for (index, change) in patch.changes().iter().enumerate() {
        #[cfg(test)]
        if fault == Fault::StageAt(index) {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(
                report,
                format!("Injected staging failure at operation {index}"),
            );
        }
        match stage_operation(change, &validator) {
            Ok(mut operation) => {
                operation.operation_report_index = index;
                staged.push(operation)
            }
            Err(error) => {
                cleanup_staged(&mut staged, &mut report);
                report.recovery_artifacts.extend(error.recovery_artifacts);
                return fail_before(report, error.message);
            }
        }
    }

    #[cfg(test)]
    if let Fault::CorruptStageAt(index) = fault {
        let Some(operation) = staged.get_mut(index) else {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(
                report,
                "Injected staged-artifact index was out of range".into(),
            );
        };
        let Some(file) = operation.stage.file.as_mut() else {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(report, "Staged artifact handle was unavailable".into());
        };
        if let Err(error) = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(b"!"))
            .and_then(|_| file.sync_all())
        {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(
                report,
                format!("Could not inject staged corruption: {error}"),
            );
        }
    }

    // Nothing is changed until all result and backup files have been created, synced, reopened,
    // identity-checked, and hash-checked.
    if let Err(error) = validator.validate_patch_context(&patch) {
        cleanup_staged(&mut staged, &mut report);
        return fail_before(
            report,
            format!("Final pre-application validation failed: {error}"),
        );
    }
    for operation in &staged {
        if let Err(error) = revalidate_target(operation) {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(report, error);
        }
        if let Err(error) = verify_artifact(&operation.stage) {
            cleanup_staged(&mut staged, &mut report);
            return fail_before(report, error);
        }
        if let Some(backup) = &operation.backup {
            if let Err(error) = verify_artifact(backup) {
                cleanup_staged(&mut staged, &mut report);
                return fail_before(report, error);
            }
        }
    }

    let mut failure = None;
    for index in 0..staged.len() {
        if let Err(error) = validator.revalidate_workspace() {
            failure = Some(format!(
                "Workspace identity changed before operation {index}: {error}"
            ));
            break;
        }
        if let Err(error) = revalidate_target(&staged[index]) {
            failure = Some(error);
            break;
        }
        #[cfg(test)]
        let injected_apply_failure = fault == Fault::ApplyAt(index)
            || matches!(fault, Fault::ApplyThenRollback { apply_at, .. } if apply_at == index)
            || matches!(
                fault,
                Fault::TamperThenApplyFailure { apply_at, .. } if apply_at == index
            );
        #[cfg(not(test))]
        let injected_apply_failure = false;
        if injected_apply_failure {
            failure = Some(format!("Injected application failure at operation {index}"));
            break;
        }
        let apply_result = apply_one(&mut staged[index]);
        if apply_result.is_ok() {
            staged[index].applied = true;
        } else if destination_matches_stage(&staged[index]).unwrap_or(false) {
            // Account for an operation that succeeded at the OS level but reported a later error.
            staged[index].applied = true;
        } else if !destination_matches_original(&staged[index]).unwrap_or(false) {
            // A failed OS call with neither the old nor the intended file at the destination has
            // an indeterminate outcome. Treat it as changed so rollback will refuse to overwrite
            // it unless it still matches the exact approved result.
            staged[index].applied = true;
            report.operations[index].status = OperationStatus::Unresolved;
            report.operations[index].detail = Some(
                "Application reported failure and the target matches neither the original snapshot nor the approved result".into(),
            );
        }
        if let Err(error) = apply_result {
            failure = Some(error);
            break;
        }
        #[cfg(test)]
        if fault == Fault::VerifyAt(index) {
            failure = Some(format!(
                "Injected post-application verification failure at operation {index}"
            ));
            break;
        }
        match verify_destination(&staged[index], staged[index].stage.identity.as_str()) {
            Ok(actual) if actual == staged[index].expected_sha256 => {
                report.operations[index].status = OperationStatus::AppliedAndVerified;
                report.operations[index].actual_sha256 = Some(actual);
            }
            Ok(actual) => {
                report.operations[index].actual_sha256 = Some(actual.clone());
                failure = Some(format!(
                    "Post-application hash mismatch for `{}`: expected {}, found {actual}",
                    staged[index].target.display, staged[index].expected_sha256
                ));
                staged[index].applied = true;
                break;
            }
            Err(error) => {
                failure = Some(error);
                staged[index].applied = true;
                break;
            }
        }
        if let Err(error) = sync_parent(&staged[index].target.parent) {
            failure = Some(format!(
                "Could not sync parent directory after `{}`: {error}",
                staged[index].target.display
            ));
            break;
        }
    }

    if let Some(error) = failure {
        report.error = Some(error);
        #[cfg(test)]
        if let Fault::TamperThenApplyFailure { tamper_at, .. } = fault {
            if let Some(operation) = staged.get(tamper_at) {
                if let Err(error) = inject_target_change_for_test(operation) {
                    report.error = Some(format!(
                        "{}; test interference injection failed: {error}",
                        report.error.as_deref().unwrap_or_default()
                    ));
                }
            }
        }
        rollback(&mut staged, &mut report, fault);
        cleanup_staged(&mut staged, &mut report);
        report.status = if report.operations.iter().any(|item| {
            matches!(
                item.status,
                OperationStatus::Unresolved | OperationStatus::RemainsApplied
            )
        }) {
            ApplicationStatus::RecoveryRequired
        } else if report
            .operations
            .iter()
            .any(|item| item.status == OperationStatus::RestoredAndVerified)
        {
            ApplicationStatus::FailedAndRolledBack
        } else {
            ApplicationStatus::FailedBeforeModification
        };
        return report;
    }

    report.status = ApplicationStatus::AppliedAndVerified;
    for operation in &mut staged {
        if let Some(backup) = operation.backup.take() {
            if let Err(error) = cleanup_artifact(&backup) {
                report.recovery_artifacts.push(backup.display.clone());
                report.operations[operation.operation_report_index].detail = Some(format!(
                    "Applied and verified; backup cleanup failed and artifact was preserved: {error}"
                ));
            }
        }
        if operation.is_create {
            // Unix no-replace creation temporarily uses a hard link from the staging name. Windows
            // moves the staged name. This cleanup is identity-bound and never removes foreign data.
            if artifact_exists(&operation.stage)
                && let Err(error) = cleanup_artifact(&operation.stage)
            {
                report
                    .recovery_artifacts
                    .push(operation.stage.display.clone());
                report.operations[operation.operation_report_index].detail = Some(format!(
                    "Applied and verified; staging cleanup failed and artifact was preserved: {error}"
                ));
            }
        }
    }
    report
}

#[cfg(test)]
fn inject_target_change_for_test(operation: &StagedOperation) -> Result<(), String> {
    let mut options = CapOpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let mut file = operation
        .target
        .parent
        .open_with(Path::new(&operation.target.leaf), &options)
        .map_err(|error| error.to_string())?
        .into_std();
    file.write_all(b"unexpected concurrent change")
        .map_err(|error| error.to_string())
}

fn validate_limits(patch: &ValidatedPatch) -> Result<(), String> {
    if patch.changes().is_empty() || patch.changes().len() > MAX_FILES {
        return Err("Approved patch has an invalid file count".into());
    }
    let mut total = 0usize;
    for change in patch.changes() {
        let bytes = change.resulting_content().len();
        if bytes > MAX_FILE_BYTES {
            return Err(format!(
                "`{}` exceeds the per-file application limit",
                change.path()
            ));
        }
        total = total.saturating_add(bytes);
        if let Some(original) = change.original_content() {
            total = total.saturating_add(original.len());
        }
        if total > MAX_TOTAL_BYTES {
            return Err("Approved patch exceeds the aggregate application limit".into());
        }
    }
    Ok(())
}

fn stage_operation(
    change: &ValidatedFileChange,
    validator: &PatchValidator,
) -> Result<StagedOperation, StageFailure> {
    let mut artifacts = Vec::new();
    let result = stage_operation_inner(change, validator, &mut artifacts);
    match result {
        Ok(operation) => Ok(operation),
        Err(mut failure) => {
            let mut recovery_artifacts = std::mem::take(&mut failure.recovery_artifacts);
            for artifact in artifacts.iter().rev() {
                if cleanup_artifact(artifact).is_err() && artifact_exists(artifact) {
                    recovery_artifacts.push(artifact.display.clone());
                }
            }
            Err(StageFailure {
                message: failure.message,
                recovery_artifacts,
            })
        }
    }
}

fn stage_operation_inner(
    change: &ValidatedFileChange,
    validator: &PatchValidator,
    artifacts: &mut Vec<Artifact>,
) -> Result<StagedOperation, StageFailure> {
    let (parent, leaf, display) = validator
        .open_patch_parent(change.path())
        .map_err(|error| {
            format!(
                "Could not open target parent for `{}`: {error}",
                change.path()
            )
        })?;
    let (original_identity, permissions) = if change.is_new_file() {
        match parent.symlink_metadata(&leaf) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Ok(_) => return Err(format!("Create destination `{display}` already exists").into()),
            Err(error) => {
                return Err(
                    format!("Could not inspect create destination `{display}`: {error}").into(),
                );
            }
        }
    } else {
        let mut file = workspace::open_file_child_nofollow(&parent, &leaf).map_err(|error| {
            format!("Could not safely open replacement target `{display}`: {error}")
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("Could not inspect `{display}`: {error}"))?;
        if repository_tools::has_multiple_links(&file, &metadata) {
            return Err(format!(
                "Replacement target `{display}` is hard-linked or link status is unavailable"
            )
            .into());
        }
        let mut actual = Vec::with_capacity(metadata.len().min(MAX_FILE_BYTES as u64) as usize);
        Read::by_ref(&mut file)
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| format!("Could not read `{display}`: {error}"))?;
        let expected = change
            .original_sha256()
            .ok_or_else(|| format!("Replacement `{display}` lacks an original hash"))?;
        if digest(&actual) != expected {
            return Err(format!("Replacement target `{display}` changed before staging").into());
        }
        let identity = workspace::file_object_identity(&file)?;
        let approved_identity = change
            .original_file_identity()
            .ok_or_else(|| format!("Replacement `{display}` lacks an approved file identity"))?;
        if identity != approved_identity {
            return Err(format!(
                "Replacement target `{display}` file identity changed after approval"
            )
            .into());
        }
        (
            Some(approved_identity.to_string()),
            Some(safe_permissions(metadata.permissions())),
        )
    };

    let stage = create_artifact(
        &parent,
        &display,
        "stage",
        change.resulting_content().as_bytes(),
        change.resulting_sha256(),
        permissions.as_ref(),
    )?;
    track_artifact(artifacts, &stage)?;
    let backup = if let Some(original) = change.original_content() {
        let original_hash = change
            .original_sha256()
            .ok_or_else(|| format!("Replacement `{display}` lacks an original hash"))?;
        let backup = create_artifact(
            &parent,
            &display,
            "backup",
            original.as_bytes(),
            original_hash,
            permissions.as_ref(),
        )?;
        track_artifact(artifacts, &backup)?;
        Some(backup)
    } else {
        None
    };

    Ok(StagedOperation {
        target: Target {
            parent,
            leaf,
            display,
            original_identity,
        },
        stage,
        backup,
        expected_sha256: change.resulting_sha256().to_string(),
        original_sha256: change.original_sha256().map(str::to_string),
        is_create: change.is_new_file(),
        applied: false,
        operation_report_index: 0,
    })
}

fn create_artifact(
    parent: &Dir,
    target: &str,
    purpose: &str,
    content: &[u8],
    expected_hash: &str,
    permissions: Option<&std::fs::Permissions>,
) -> Result<Artifact, StageFailure> {
    let serial = NEXT_ARTIFACT.fetch_add(1, Ordering::Relaxed);
    let name = OsString::from(format!(
        ".hecate-{purpose}-{}-{serial}.tmp",
        std::process::id()
    ));
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .access_mode(GENERIC_READ | GENERIC_WRITE | 0x0001_0000)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = parent
        .open_with(Path::new(&name), &options)
        .map_err(|error| {
            format!("Could not exclusively create {purpose} artifact for `{target}`: {error}")
        })?
        .into_std();
    let display = format!("{target} ({purpose}: {})", name.to_string_lossy());
    let identity = workspace::file_object_identity(&file).map_err(|error| StageFailure {
        message: error,
        recovery_artifacts: vec![display.clone()],
    })?;
    let artifact = Artifact {
        parent: parent.try_clone().map_err(|error| StageFailure {
            message: format!("Could not retain artifact directory: {error}"),
            recovery_artifacts: vec![display.clone()],
        })?,
        name: name.clone(),
        display,
        identity,
        sha256: expected_hash.to_string(),
        file: Some(file),
    };
    let write_result = (|| -> Result<(), String> {
        let mut file = artifact
            .file
            .as_ref()
            .ok_or_else(|| "New artifact handle is unavailable".to_string())?;
        file.write_all(content).map_err(|error| {
            format!("Could not write {purpose} artifact for `{target}`: {error}")
        })?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions.clone()).map_err(|error| {
                format!("Could not preserve permissions for `{target}`: {error}")
            })?;
        }
        file.sync_all().map_err(|error| {
            format!("Could not sync {purpose} artifact for `{target}`: {error}")
        })?;
        verify_artifact(&artifact)
    })();
    if let Err(error) = write_result {
        let cleanup = cleanup_artifact_by_identity(&artifact);
        return Err(StageFailure {
            message: match &cleanup {
                Ok(()) => error,
                Err(cleanup_error) => format!("{error}; {cleanup_error}"),
            },
            recovery_artifacts: cleanup
                .err()
                .map(|_| vec![artifact.display.clone()])
                .unwrap_or_default(),
        });
    }
    Ok(artifact)
}

fn clone_artifact(artifact: &Artifact) -> Result<Artifact, String> {
    Ok(Artifact {
        parent: artifact
            .parent
            .try_clone()
            .map_err(|error| format!("Could not retain artifact directory: {error}"))?,
        name: artifact.name.clone(),
        display: artifact.display.clone(),
        identity: artifact.identity.clone(),
        sha256: artifact.sha256.clone(),
        file: None,
    })
}

fn track_artifact(artifacts: &mut Vec<Artifact>, artifact: &Artifact) -> Result<(), StageFailure> {
    match clone_artifact(artifact) {
        Ok(copy) => {
            artifacts.push(copy);
            Ok(())
        }
        Err(message) => {
            let cleanup = cleanup_artifact(artifact);
            Err(StageFailure {
                message: match &cleanup {
                    Ok(()) => message,
                    Err(error) => format!("{message}; {error}"),
                },
                recovery_artifacts: cleanup
                    .err()
                    .map(|_| vec![artifact.display.clone()])
                    .unwrap_or_default(),
            })
        }
    }
}

fn cleanup_artifact_by_identity(artifact: &Artifact) -> Result<(), String> {
    let file = workspace::open_file_child_nofollow(&artifact.parent, &artifact.name)
        .map_err(|error| format!("Could not safely inspect partial artifact: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect partial artifact: {error}"))?;
    if repository_tools::has_multiple_links(&file, &metadata)
        || workspace::file_object_identity(&file)? != artifact.identity
    {
        return Err("Cleanup refused; partial artifact identity or link count changed".into());
    }
    artifact
        .parent
        .remove_file(&artifact.name)
        .map_err(|error| format!("Could not remove owned partial artifact: {error}"))
}

fn verify_artifact(artifact: &Artifact) -> Result<(), String> {
    let mut file =
        workspace::open_file_child_nofollow(&artifact.parent, &artifact.name).map_err(|error| {
            format!(
                "Could not safely reopen artifact `{}`: {error}",
                artifact.display
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect artifact `{}`: {error}", artifact.display))?;
    if repository_tools::has_multiple_links(&file, &metadata)
        || workspace::file_object_identity(&file)? != artifact.identity
    {
        return Err(format!(
            "Artifact `{}` has an unexpected file identity or link count",
            artifact.display
        ));
    }
    let actual = hash_file(&mut file, MAX_FILE_BYTES)?;
    if actual != artifact.sha256 {
        return Err(format!(
            "Artifact `{}` failed content verification",
            artifact.display
        ));
    }
    Ok(())
}

fn revalidate_target(operation: &StagedOperation) -> Result<(), String> {
    if let Some(expected_hash) = &operation.original_sha256 {
        let mut file =
            workspace::open_file_child_nofollow(&operation.target.parent, &operation.target.leaf)
                .map_err(|error| {
                format!(
                    "Could not reopen target `{}`: {error}",
                    operation.target.display
                )
            })?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "Could not inspect target `{}`: {error}",
                operation.target.display
            )
        })?;
        if repository_tools::has_multiple_links(&file, &metadata) {
            return Err(format!(
                "Target `{}` is hard-linked or link status is unavailable",
                operation.target.display
            ));
        }
        if Some(workspace::file_object_identity(&file)?) != operation.target.original_identity {
            return Err(format!(
                "Target `{}` file identity changed",
                operation.target.display
            ));
        }
        if hash_file(&mut file, MAX_FILE_BYTES)? != *expected_hash {
            return Err(format!(
                "Target `{}` content changed",
                operation.target.display
            ));
        }
    } else {
        match operation
            .target
            .parent
            .symlink_metadata(&operation.target.leaf)
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(format!(
                    "Create destination `{}` appeared",
                    operation.target.display
                ));
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect create destination `{}`: {error}",
                    operation.target.display
                ));
            }
        }
    }
    verify_artifact(&operation.stage)
}

fn apply_one(operation: &mut StagedOperation) -> Result<(), String> {
    verify_artifact(&operation.stage)?;
    let stage = operation.stage.file.take().ok_or_else(|| {
        format!(
            "Staging handle for `{}` is unavailable",
            operation.target.display
        )
    })?;
    stage
        .sync_all()
        .map_err(|error| format!("Could not sync staging file: {error}"))?;
    drop(stage);
    platform_apply(operation)
}

#[cfg(unix)]
fn platform_apply(operation: &StagedOperation) -> Result<(), String> {
    if operation.is_create {
        operation
            .target
            .parent
            .hard_link(
                &operation.stage.name,
                &operation.target.parent,
                &operation.target.leaf,
            )
            .map_err(|error| {
                format!(
                    "Atomic no-replace creation failed for `{}`: {error}",
                    operation.target.display
                )
            })?;
        operation
            .target
            .parent
            .remove_file(&operation.stage.name)
            .map_err(|error| {
                format!(
                    "Created `{}` but could not unlink its staging name: {error}",
                    operation.target.display
                )
            })?;
        Ok(())
    } else {
        operation
            .target
            .parent
            .rename(
                &operation.stage.name,
                &operation.target.parent,
                &operation.target.leaf,
            )
            .map_err(|error| {
                format!(
                    "Atomic replacement failed for `{}`: {error}",
                    operation.target.display
                )
            })
    }
}

#[cfg(windows)]
fn platform_apply(operation: &StagedOperation) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
        VOLUME_NAME_DOS,
    };
    let parent = operation
        .target
        .parent
        .try_clone()
        .map(Dir::into_std_file)
        .map_err(|error| format!("Could not inspect target directory: {error}"))?;
    let mut buffer = vec![0u16; 32768];
    // SAFETY: buffer is writable and the handle is held open for this operation.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            parent.as_raw_handle() as _,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            VOLUME_NAME_DOS,
        )
    };
    if length == 0 || length as usize >= buffer.len() {
        return Err(
            "Could not obtain an absolute path for the retained destination directory".into(),
        );
    }
    buffer.truncate(length as usize);
    let root = PathBuf::from(
        String::from_utf16(&buffer)
            .map_err(|_| "Destination directory path is not valid UTF-16")?,
    );
    let stage = nul_terminated(root.join(&operation.stage.name).as_os_str());
    let target = nul_terminated(root.join(&operation.target.leaf).as_os_str());
    // SAFETY: paths are NUL-terminated UTF-16, same-volume siblings, and checked against retained
    // directory capabilities immediately before this call. Same-user concurrent path attacks are
    // explicitly outside this project's bounded CLI threat model.
    let success = unsafe {
        if operation.is_create {
            MoveFileExW(stage.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH)
        } else {
            ReplaceFileW(
                target.as_ptr(),
                stage.as_ptr(),
                std::ptr::null(),
                0x00000001,
                std::ptr::null(),
                std::ptr::null(),
            )
        }
    };
    if success == 0 {
        return Err(format!(
            "Atomic Windows application failed for `{}`: {}",
            operation.target.display,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn nul_terminated(path: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(not(any(unix, windows)))]
fn platform_apply(_: &StagedOperation) -> Result<(), String> {
    Err("Atomic patch application is unsupported on this platform".into())
}

fn verify_destination(
    operation: &StagedOperation,
    expected_identity: &str,
) -> Result<String, String> {
    let mut file =
        workspace::open_file_child_nofollow(&operation.target.parent, &operation.target.leaf)
            .map_err(|error| {
                format!(
                    "Could not safely reopen applied target `{}`: {error}",
                    operation.target.display
                )
            })?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "Could not inspect applied target `{}`: {error}",
            operation.target.display
        )
    })?;
    if repository_tools::has_multiple_links(&file, &metadata)
        || workspace::file_object_identity(&file)? != expected_identity
    {
        return Err(format!(
            "Applied target `{}` has unexpected identity or hard links",
            operation.target.display
        ));
    }
    hash_file(&mut file, MAX_FILE_BYTES)
}

fn destination_matches_stage(operation: &StagedOperation) -> Result<bool, String> {
    let mut file =
        match workspace::open_file_child_nofollow(&operation.target.parent, &operation.target.leaf)
        {
            Ok(file) => file,
            Err(_) => return Ok(false),
        };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    let links = file_link_count(&file, &metadata)?;
    let allowed_links = if operation.is_create && artifact_exists(&operation.stage) {
        2
    } else {
        1
    };
    Ok(links <= allowed_links
        && workspace::file_object_identity(&file)? == operation.stage.identity
        && hash_file(&mut file, MAX_FILE_BYTES)? == operation.expected_sha256)
}

fn destination_matches_original(operation: &StagedOperation) -> Result<bool, String> {
    if operation.is_create {
        return match operation
            .target
            .parent
            .symlink_metadata(&operation.target.leaf)
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Ok(_) => Ok(false),
            Err(error) => Err(error.to_string()),
        };
    }
    let mut file =
        match workspace::open_file_child_nofollow(&operation.target.parent, &operation.target.leaf)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if repository_tools::has_multiple_links(&file, &metadata)
        || Some(workspace::file_object_identity(&file)?) != operation.target.original_identity
    {
        return Ok(false);
    }
    Ok(hash_file(&mut file, MAX_FILE_BYTES)?
        == operation.original_sha256.as_deref().unwrap_or_default())
}

fn rollback(staged: &mut [StagedOperation], report: &mut ApplicationReport, fault: Fault) {
    #[cfg(not(test))]
    let _ = fault;
    for index in (0..staged.len()).rev() {
        if !staged[index].applied {
            continue;
        }
        #[cfg(test)]
        let inject_rollback_failure = fault == Fault::RollbackAt(index)
            || matches!(fault, Fault::ApplyThenRollback { rollback_at, .. } if rollback_at == index);
        #[cfg(not(test))]
        let inject_rollback_failure = false;
        if inject_rollback_failure {
            report.operations[index].status =
                if destination_matches_stage(&staged[index]).unwrap_or(false) {
                    OperationStatus::RemainsApplied
                } else {
                    OperationStatus::Unresolved
                };
            report.operations[index].detail =
                Some("Injected rollback failure; recovery data preserved".into());
            report_recovery_location(&staged[index], report);
            continue;
        }
        match rollback_one(&mut staged[index]) {
            Ok(actual) => {
                report.operations[index].status = OperationStatus::RestoredAndVerified;
                report.operations[index].actual_sha256 = if actual.is_empty() {
                    None
                } else {
                    Some(actual)
                };
            }
            Err(error) => {
                report.operations[index].status =
                    if destination_matches_stage(&staged[index]).unwrap_or(false) {
                        OperationStatus::RemainsApplied
                    } else {
                        OperationStatus::Unresolved
                    };
                report.operations[index].detail = Some(error);
                report.operations[index].actual_sha256 = None;
                report_recovery_location(&staged[index], report);
            }
        }
    }
}

fn report_recovery_location(operation: &StagedOperation, report: &mut ApplicationReport) {
    if let Some(backup) = &operation.backup
        && artifact_exists(backup)
    {
        report.recovery_artifacts.push(backup.display.clone());
    } else if operation.applied && operation.original_sha256.is_some() {
        // A restore may have consumed the backup before post-restore verification failed. In that
        // case the destination is the location requiring manual inspection.
        report
            .recovery_artifacts
            .push(operation.target.display.clone());
    } else if operation.applied
        && operation.is_create
        && destination_matches_stage(operation).unwrap_or(false)
    {
        report
            .recovery_artifacts
            .push(operation.target.display.clone());
    }
}

fn rollback_one(operation: &mut StagedOperation) -> Result<String, String> {
    if !destination_matches_stage(operation)? {
        return Err(format!(
            "Will not rollback `{}` because its current identity, content, or link count is unexpected",
            operation.target.display
        ));
    }
    if operation.is_create {
        operation
            .target
            .parent
            .remove_file(&operation.target.leaf)
            .map_err(|error| {
                format!(
                    "Could not remove HECATE-created file `{}` during rollback: {error}",
                    operation.target.display
                )
            })?;
        match operation
            .target
            .parent
            .symlink_metadata(&operation.target.leaf)
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(format!(
                    "Rollback removal of `{}` could not be verified",
                    operation.target.display
                ));
            }
        }
        sync_parent(&operation.target.parent)?;
        operation.applied = false;
        return Ok(String::new());
    }
    let backup = operation
        .backup
        .as_mut()
        .ok_or_else(|| format!("Backup for `{}` is unavailable", operation.target.display))?;
    verify_artifact(backup)?;
    let backup_identity = backup.identity.clone();
    replace_with_backup(backup, &operation.target)?;
    let mut restored =
        workspace::open_file_child_nofollow(&operation.target.parent, &operation.target.leaf)
            .map_err(|error| {
                format!(
                    "Could not verify restored `{}`: {error}",
                    operation.target.display
                )
            })?;
    let metadata = restored.metadata().map_err(|error| {
        format!(
            "Could not inspect restored `{}`: {error}",
            operation.target.display
        )
    })?;
    if repository_tools::has_multiple_links(&restored, &metadata) {
        return Err(format!(
            "Restored `{}` has unexpected hard links",
            operation.target.display
        ));
    }
    if workspace::file_object_identity(&restored)? != backup_identity {
        return Err(format!(
            "Restoration of `{}` did not retain the verified backup file identity",
            operation.target.display
        ));
    }
    let actual = hash_file(&mut restored, MAX_FILE_BYTES)?;
    if Some(actual.as_str()) != operation.original_sha256.as_deref() {
        return Err(format!(
            "Restoration of `{}` failed hash verification",
            operation.target.display
        ));
    }
    sync_parent(&operation.target.parent)?;
    operation.applied = false;
    Ok(actual)
}

fn replace_with_backup(backup: &mut Artifact, target: &Target) -> Result<(), String> {
    backup.file.take();
    #[cfg(unix)]
    target
        .parent
        .rename(&backup.name, &target.parent, &target.leaf)
        .map_err(|error| format!("Could not restore backup for `{}`: {error}", target.display))?;
    #[cfg(windows)]
    {
        let restore = StagedOperation {
            target: Target {
                parent: target.parent.try_clone().map_err(|e| e.to_string())?,
                leaf: target.leaf.clone(),
                display: target.display.clone(),
                original_identity: None,
            },
            stage: Artifact {
                parent: backup.parent.try_clone().map_err(|e| e.to_string())?,
                name: backup.name.clone(),
                display: backup.display.clone(),
                identity: backup.identity.clone(),
                sha256: backup.sha256.clone(),
                file: None,
            },
            backup: None,
            expected_sha256: backup.sha256.clone(),
            original_sha256: None,
            is_create: false,
            applied: false,
            operation_report_index: 0,
        };
        platform_apply(&restore)?;
    }
    Ok(())
}

fn cleanup_staged(staged: &mut [StagedOperation], report: &mut ApplicationReport) {
    for operation in staged.iter_mut().rev() {
        let preserve_backup = operation
            .backup
            .as_ref()
            .is_some_and(|backup| report.recovery_artifacts.contains(&backup.display));
        let mut artifacts: Vec<&Artifact> = vec![&operation.stage];
        if let Some(backup) = operation.backup.as_ref() {
            if !preserve_backup {
                artifacts.push(backup);
            }
        }
        for artifact in artifacts {
            if let Err(error) = cleanup_artifact(artifact) {
                if artifact_exists(artifact) {
                    report.recovery_artifacts.push(artifact.display.clone());
                    report.operations[operation.operation_report_index].detail =
                        Some(format!("Cleanup preserved `{}`: {error}", artifact.display));
                }
            }
        }
    }
}

fn cleanup_artifact(artifact: &Artifact) -> Result<(), String> {
    if !artifact_exists(artifact) {
        return Ok(());
    }
    let mut file =
        workspace::open_file_child_nofollow(&artifact.parent, &artifact.name).map_err(|error| {
            format!(
                "Could not safely inspect cleanup candidate `{}`: {error}",
                artifact.display
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect cleanup candidate: {error}"))?;
    if repository_tools::has_multiple_links(&file, &metadata)
        || workspace::file_object_identity(&file)? != artifact.identity
        || hash_file(&mut file, MAX_FILE_BYTES)? != artifact.sha256
    {
        return Err(format!(
            "Cleanup refused because `{}` no longer identifies HECATE's expected artifact",
            artifact.display
        ));
    }
    artifact
        .parent
        .remove_file(&artifact.name)
        .map_err(|error| {
            format!(
                "Could not remove owned artifact `{}`: {error}",
                artifact.display
            )
        })
}

fn artifact_exists(artifact: &Artifact) -> bool {
    match artifact.parent.symlink_metadata(&artifact.name) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn file_link_count(file: &std::fs::File, metadata: &std::fs::Metadata) -> Result<u64, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = file;
        Ok(metadata.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let _ = metadata;
        let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
            unsafe { std::mem::zeroed() };
        // SAFETY: `file` is an open file handle and the output structure has the API layout.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                file.as_raw_handle() as _,
                &mut info,
            )
        } == 0
        {
            return Err("Could not inspect file hard-link count".into());
        }
        Ok(info.nNumberOfLinks as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        Err("File hard-link count is unavailable on this platform".into())
    }
}

#[allow(unused_mut)]
fn safe_permissions(mut permissions: std::fs::Permissions) -> std::fs::Permissions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode() & 0o777;
        permissions.set_mode(mode);
    }
    permissions
}

fn hash_file(file: &mut std::fs::File, limit: usize) -> Result<String, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("Could not seek file for verification: {error}"))?;
    let mut bytes = Vec::with_capacity(8192);
    Read::by_ref(file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read file for verification: {error}"))?;
    if bytes.len() > limit {
        return Err("File exceeded the verification size limit".into());
    }
    Ok(digest(&bytes))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sync_parent(parent: &Dir) -> Result<(), String> {
    #[cfg(unix)]
    {
        parent
            .try_clone()
            .map(Dir::into_std_file)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("Directory sync failed: {error}"))
    }
    #[cfg(windows)]
    {
        let _ = parent;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = parent;
        Err("Directory synchronization is unsupported".into())
    }
}

fn operation_report(change: &ValidatedFileChange, status: OperationStatus) -> OperationReport {
    OperationReport {
        path: change.path().to_string(),
        status,
        original_sha256: change.original_sha256().map(str::to_string),
        expected_sha256: change.resulting_sha256().to_string(),
        actual_sha256: None,
        detail: None,
    }
}

fn fail_before(mut report: ApplicationReport, error: String) -> ApplicationReport {
    report.status = ApplicationStatus::FailedBeforeModification;
    report.error = Some(error);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::patch::{FilePatch, PatchOperation, PatchProposal, TextEdit};
    use crate::agent::patch_approval::approve_with_io;
    use crate::agent::patch_validation::PatchValidator;
    use crate::agent::session::{ContributionContext, ContributionPlanReference, PlanReviewStatus};
    use crate::github::GitHubRepository;
    use crate::planning::ContributionPlan;
    use crate::workspace::{create_workspace_from_source_for_tests, preview_workspace_for_tests};
    use std::fs;
    use std::io::Cursor;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        prepared: crate::workspace::WorkspaceResult,
        validator: PatchValidator,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hecate-apply-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let source = root.join("source");
            fs::create_dir_all(source.join("src")).unwrap();
            fs::write(source.join("src/a.rs"), "old-a\n").unwrap();
            fs::write(source.join("src/b.rs"), "old-b\n").unwrap();
            let git = |args: &[&str]| {
                Command::new("git")
                    .args(args)
                    .current_dir(&source)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "NUL")
                    .env("GIT_AUTHOR_NAME", "HECATE Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                    .env("GIT_COMMITTER_NAME", "HECATE Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                    .output()
                    .unwrap()
            };
            assert!(git(&["init", "--initial-branch=main"]).status.success());
            assert!(git(&["add", "."]).status.success());
            assert!(git(&["commit", "-m", "initial"]).status.success());
            let commit = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_string();
            let repo = GitHubRepository {
                owner: "owner".into(),
                name: "repo".into(),
                url: "https://github.com/owner/repo".into(),
                default_branch: "main".into(),
                commit,
            };
            let plan = ContributionPlan {
                repository_url: repo.url.clone(),
                issue_number: 1,
                issue_title: "test".into(),
                issue_url: "https://github.com/owner/repo/issues/1".into(),
                labels: vec![],
                created_at: "now".into(),
                updated_at: "now".into(),
                comments: 0,
                expected_outcome: "change".into(),
                interpretation: "change".into(),
                locations: vec![],
                documents: vec![],
                steps: vec!["change".into()],
                test_recommendations: vec![],
                validation_commands: vec![],
                risks_and_assumptions: vec![],
                unresolved_questions: vec![],
            };
            let preview = preview_workspace_for_tests(&repo, 1, &root.join("workspaces")).unwrap();
            let prepared = create_workspace_from_source_for_tests(
                &repo,
                &plan,
                &preview,
                source.to_str().unwrap(),
            )
            .unwrap();
            let context = ContributionContext {
                repository_url: repo.url.clone(),
                issue_number: 1,
                issue_title: "test".into(),
                plan: ContributionPlanReference {
                    artifact_id: ".hecate/plan-v1.json".into(),
                    repository_url: repo.url,
                    issue_number: 1,
                    schema_version: crate::workspace::PLAN_SCHEMA_VERSION,
                    review_status: PlanReviewStatus::Approved,
                },
            };
            let validator = PatchValidator::new(&prepared, &context).unwrap();
            Self {
                root,
                prepared,
                validator,
            }
        }
        fn proposal(&self) -> PatchProposal {
            PatchProposal::new(vec![
                FilePatch {
                    path: "src/a.rs".into(),
                    operation: PatchOperation::Replace {
                        expected_original_sha256: digest(b"old-a\n"),
                        edits: vec![TextEdit {
                            old_text: "old-a".into(),
                            new_text: "new-a".into(),
                        }],
                    },
                },
                FilePatch {
                    path: "src/new.rs".into(),
                    operation: PatchOperation::Create {
                        content: "created\n".into(),
                    },
                },
                FilePatch {
                    path: "src/b.rs".into(),
                    operation: PatchOperation::Replace {
                        expected_original_sha256: digest(b"old-b\n"),
                        edits: vec![TextEdit {
                            old_text: "old-b".into(),
                            new_text: "new-b".into(),
                        }],
                    },
                },
            ])
        }
        fn approved(&self, fault: Fault) -> (ApplicationReport, Fault) {
            let patch = self.validator.validate(&self.proposal()).unwrap();
            let hash = patch.patch_sha256().to_string();
            let approved = approve_with_io(
                patch,
                &self.validator,
                "session",
                true,
                true,
                Cursor::new(format!("approve {hash}\n").into_bytes()),
                &mut Vec::new(),
            )
            .unwrap();
            (apply_with_fault(approved, fault), fault)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn applies_multi_file_patch_and_create_after_staging_all_results() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::None);
        assert_eq!(report.status, ApplicationStatus::AppliedAndVerified);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"new-a\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/b.rs")).unwrap(),
            b"new-b\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/new.rs")).unwrap(),
            b"created\n"
        );
        assert!(
            report
                .operations
                .iter()
                .all(|operation| operation.status == OperationStatus::AppliedAndVerified)
        );
    }

    #[test]
    fn stage_failure_leaves_every_target_unchanged() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::StageAt(1));
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/b.rs")).unwrap(),
            b"old-b\n"
        );
    }

    #[test]
    fn staged_hash_mismatch_is_rejected_before_any_target_write() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::CorruptStageAt(0));
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
        assert!(
            report
                .recovery_artifacts
                .iter()
                .any(|artifact| artifact.contains("stage"))
        );
    }

    #[test]
    fn application_failure_rolls_back_in_reverse_and_verifies_restoration() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::ApplyAt(1));
        assert_eq!(report.status, ApplicationStatus::FailedAndRolledBack);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/b.rs")).unwrap(),
            b"old-b\n"
        );
        assert!(report.operations.iter().all(|op| matches!(
            op.status,
            OperationStatus::RestoredAndVerified | OperationStatus::NotApplied
        )));
    }

    #[test]
    fn created_file_is_removed_when_a_later_operation_fails() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::ApplyAt(2));
        assert_eq!(report.status, ApplicationStatus::FailedAndRolledBack);
        assert!(!fixture.prepared.checkout_path.join("src/new.rs").exists());
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
        assert_eq!(
            report.operations[1].status,
            OperationStatus::RestoredAndVerified
        );
    }

    #[test]
    fn changed_original_after_approval_is_rejected_without_overwrite() {
        let fixture = Fixture::new();
        let patch = fixture.validator.validate(&fixture.proposal()).unwrap();
        let hash = patch.patch_sha256().to_string();
        let approved = approve_with_io(
            patch,
            &fixture.validator,
            "session",
            true,
            true,
            Cursor::new(format!("approve {hash}\n").into_bytes()),
            &mut Vec::new(),
        )
        .unwrap();
        fs::write(
            fixture.prepared.checkout_path.join("src/a.rs"),
            "external\n",
        )
        .unwrap();
        let report = apply_approved_patch(approved);
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"external\n"
        );
        assert!(!fixture.prepared.checkout_path.join("src/new.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_added_after_approval_is_rejected() {
        let fixture = Fixture::new();
        let patch = fixture.validator.validate(&fixture.proposal()).unwrap();
        let hash = patch.patch_sha256().to_string();
        let approved = approve_with_io(
            patch,
            &fixture.validator,
            "session",
            true,
            true,
            Cursor::new(format!("approve {hash}\n").into_bytes()),
            &mut Vec::new(),
        )
        .unwrap();
        let outside = fixture.root.join("outside-hardlink");
        fs::hard_link(fixture.prepared.checkout_path.join("src/a.rs"), &outside).unwrap();
        let report = apply_approved_patch(approved);
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn leaf_symlink_substitution_after_approval_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let patch = fixture.validator.validate(&fixture.proposal()).unwrap();
        let hash = patch.patch_sha256().to_string();
        let approved = approve_with_io(
            patch,
            &fixture.validator,
            "session",
            true,
            true,
            Cursor::new(format!("approve {hash}\n").into_bytes()),
            &mut Vec::new(),
        )
        .unwrap();
        let target = fixture.prepared.checkout_path.join("src/a.rs");
        let saved = fixture.root.join("original-a.rs");
        fs::rename(&target, &saved).unwrap();
        symlink(&saved, &target).unwrap();
        let report = apply_approved_patch(approved);
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(fs::read(saved).unwrap(), b"old-a\n");
    }

    #[test]
    fn rollback_failure_preserves_backup_and_reports_recovery_required() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::ApplyThenRollback {
            apply_at: 1,
            rollback_at: 0,
        });
        assert_eq!(report.status, ApplicationStatus::RecoveryRequired);
        assert!(!report.recovery_artifacts.is_empty());
        assert_eq!(report.operations[0].status, OperationStatus::RemainsApplied);
    }

    #[test]
    fn rollback_never_overwrites_an_unexpected_target_change() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::TamperThenApplyFailure {
            apply_at: 1,
            tamper_at: 0,
        });
        assert_eq!(report.status, ApplicationStatus::RecoveryRequired);
        assert_eq!(report.operations[0].status, OperationStatus::Unresolved);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"unexpected concurrent change"
        );
        assert!(
            report
                .recovery_artifacts
                .iter()
                .any(|artifact| artifact.contains("backup"))
        );
    }

    #[test]
    fn destination_collision_after_approval_is_rejected_before_any_write() {
        let fixture = Fixture::new();
        let patch = fixture.validator.validate(&fixture.proposal()).unwrap();
        let hash = patch.patch_sha256().to_string();
        let approved = approve_with_io(
            patch,
            &fixture.validator,
            "session",
            true,
            true,
            Cursor::new(format!("approve {hash}\n").into_bytes()),
            &mut Vec::new(),
        )
        .unwrap();
        fs::write(
            fixture.prepared.checkout_path.join("src/new.rs"),
            "outside\n",
        )
        .unwrap();
        let report = apply_approved_patch(approved);
        assert_eq!(report.status, ApplicationStatus::FailedBeforeModification);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/new.rs")).unwrap(),
            b"outside\n"
        );
    }

    #[test]
    fn application_reports_post_write_verification_failure_and_rolls_back() {
        let fixture = Fixture::new();
        let (report, _) = fixture.approved(Fault::VerifyAt(0));
        assert_eq!(report.status, ApplicationStatus::FailedAndRolledBack);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/a.rs")).unwrap(),
            b"old-a\n"
        );
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
}
