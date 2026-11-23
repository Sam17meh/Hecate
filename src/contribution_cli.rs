//! Single-process CLI orchestration for user-supplied, human-approved patch proposals.

use crate::agent::patch::{PATCH_PROPOSAL_SCHEMA_VERSION, PatchProposal, ValidatedPatch};
use crate::agent::patch_application::{ApplicationReport, ApplicationStatus, OperationStatus};
use crate::agent::patch_approval::{ApprovalError, ApprovedPatch};
use crate::agent::patch_validation::{PatchValidator, escape_terminal_text};
use crate::agent::session::ContributionContext;
use crate::workspace::WorkspaceResult;
use std::fs::{self, File};
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Serialized input is bounded before serde allocates the proposal structure. The validator then
/// enforces the smaller semantic/file/edit limits.
pub(crate) const MAX_PROPOSAL_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WORKSPACE_CONFIRMATION_BYTES: usize = 256;

pub(crate) fn confirm_and_create_workspace<R, W, T>(
    input: &mut R,
    output: &mut W,
    create: impl FnOnce() -> Result<T, String>,
) -> Result<Option<T>, String>
where
    R: BufRead,
    W: Write,
{
    write!(output, "Create this workspace? [y/N] ")
        .and_then(|_| output.flush())
        .map_err(|_| "Could not display workspace confirmation prompt".to_string())?;
    let response = read_bounded_line(input, MAX_WORKSPACE_CONFIRMATION_BYTES)?;
    let response = String::from_utf8(response)
        .map_err(|_| "Workspace confirmation was not valid UTF-8".to_string())?;
    if !matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        writeln!(output, "Cancelled. No workspace files were created.")
            .map_err(|_| "Could not display workspace cancellation".to_string())?;
        return Ok(None);
    }
    create().map(Some)
}

fn read_bounded_line(input: &mut impl BufRead, limit: usize) -> Result<Vec<u8>, String> {
    let mut response = Vec::new();
    loop {
        let available = input
            .fill_buf()
            .map_err(|_| "Could not read workspace confirmation".to_string())?;
        if available.is_empty() {
            return Ok(response);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if response.len().saturating_add(count) > limit {
            return Err("Workspace confirmation exceeded the input limit".into());
        }
        response.extend_from_slice(&available[..count]);
        input.consume(count);
        if newline.is_some() {
            return Ok(response);
        }
    }
}

pub(crate) fn load_proposal(path: &Path) -> Result<PatchProposal, String> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| "Could not inspect the patch proposal path".to_string())?;
    if !path_metadata.is_file() {
        return Err("Patch proposal input must be a regular, non-symlink file".into());
    }
    let mut file =
        File::open(path).map_err(|_| "Could not open the patch proposal file".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "Could not inspect the patch proposal file".to_string())?;
    if !metadata.is_file() {
        return Err("Patch proposal input must be a regular file".into());
    }
    if metadata.len() > MAX_PROPOSAL_JSON_BYTES {
        return Err(format!(
            "Patch proposal JSON exceeds the {} MiB input limit",
            MAX_PROPOSAL_JSON_BYTES / (1024 * 1024)
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_PROPOSAL_JSON_BYTES) as usize);
    Read::by_ref(&mut file)
        .take(MAX_PROPOSAL_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the patch proposal file".to_string())?;
    if bytes.len() as u64 > MAX_PROPOSAL_JSON_BYTES {
        return Err(format!(
            "Patch proposal JSON exceeds the {} MiB input limit",
            MAX_PROPOSAL_JSON_BYTES / (1024 * 1024)
        ));
    }

    let proposal: PatchProposal = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Malformed patch proposal JSON at line {}, column {}",
            error.line(),
            error.column()
        )
    })?;
    if proposal.schema_version != PATCH_PROPOSAL_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported patch proposal schema version {}; expected {}",
            proposal.schema_version, PATCH_PROPOSAL_SCHEMA_VERSION
        ));
    }
    Ok(proposal)
}

pub(crate) fn new_cli_session_id() -> Result<String, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is earlier than the Unix epoch".to_string())?
        .as_nanos();
    Ok(format!("cli-{}-{timestamp}", std::process::id()))
}

/// Keep the order of trust transitions explicit and testable. The callbacks are narrow seams for
/// terminal approval and application; the production path supplies HECATE's existing APIs.
pub(crate) fn validate_approve_apply<Review, Approve, Apply>(
    prepared: &WorkspaceResult,
    context: &ContributionContext,
    proposal: &PatchProposal,
    session_id: &str,
    review: Review,
    approve: Approve,
    apply: Apply,
) -> Result<ApplicationReport, String>
where
    Review: FnOnce(&ValidatedPatch, &PatchValidator) -> Result<(), String>,
    Approve: FnOnce(ValidatedPatch, &PatchValidator, &str) -> Result<ApprovedPatch, ApprovalError>,
    Apply: FnOnce(ApprovedPatch) -> ApplicationReport,
{
    let validator = PatchValidator::new(prepared, context)
        .map_err(|error| format!("Could not validate prepared workspace: {error}"))?;
    let patch = validator
        .validate(proposal)
        .map_err(|error| format!("Patch proposal was rejected: {error}"))?;
    review(&patch, &validator)?;
    let approved = approve(patch, &validator, session_id).map_err(|error| match error {
        ApprovalError::Rejected => {
            "Patch approval was rejected; the workspace was preserved and no patch was applied."
                .to_string()
        }
        ApprovalError::NonInteractive => {
            "Patch approval requires interactive stdin and stdout terminals".into()
        }
        other => format!("Patch approval failed: {other}"),
    })?;
    Ok(apply(approved))
}

pub(crate) fn print_review_context(
    repository_url: &str,
    issue_number: u64,
    issue_title: &str,
    workspace_path: &Path,
    patch: &ValidatedPatch,
) -> Result<(), String> {
    let repository = safe_text(repository_url)?;
    let title = safe_text(issue_title)?;
    let workspace = safe_text(&workspace_path.display().to_string())?;
    let commit = safe_text(&patch.workspace.checkout_commit)?;
    let branch = safe_text(&patch.workspace.branch)?;
    println!("HECATE CONTRIBUTION PATCH REVIEW");
    println!("Repository: {repository}");
    println!("Issue: #{issue_number} {title}");
    println!("Pinned commit: {commit}");
    println!("Contribution branch: {branch}");
    println!("Prepared workspace: {workspace}");
    println!("Operations: {}", patch.changes().len());
    println!("Patch SHA-256: {}", patch.patch_sha256());
    println!("The complete diff follows; review it before entering the full hash.");
    Ok(())
}

pub(crate) fn render_application_report(report: &ApplicationReport) -> Result<String, String> {
    let mut output = String::new();
    match report.status {
        ApplicationStatus::AppliedAndVerified => {
            output.push_str("Patch applied and every resulting file was verified.\n");
        }
        ApplicationStatus::FailedBeforeModification => {
            output.push_str("Application failed before any patch target was modified.\n");
        }
        ApplicationStatus::FailedAndRolledBack => {
            output.push_str(
                "Application failed; every attempted change was restored and verified.\n",
            );
        }
        ApplicationStatus::RecoveryRequired => {
            output.push_str("RECOVERY REQUIRED: inspect this workspace before continuing.\n");
        }
    }
    if let Some(error) = &report.error {
        output.push_str(&format!("Detail: {}\n", safe_text(error)?));
    }
    output.push_str("Operations:\n");
    for operation in &report.operations {
        let path = safe_text(&operation.path)?;
        let status = match operation.status {
            OperationStatus::NotApplied => "not applied",
            OperationStatus::AppliedAndVerified => "applied and verified",
            OperationStatus::RestoredAndVerified => "restored and verified",
            OperationStatus::RemainsApplied => "remains applied",
            OperationStatus::Unresolved => "unresolved",
        };
        output.push_str(&format!("  {status}: {path}\n"));
        output.push_str(&format!(
            "    expected SHA-256: {}\n",
            operation.expected_sha256
        ));
        if let Some(original) = &operation.original_sha256 {
            output.push_str(&format!("    original SHA-256: {original}\n"));
        }
        if let Some(actual) = &operation.actual_sha256 {
            output.push_str(&format!("    verified SHA-256: {actual}\n"));
        }
        if let Some(detail) = &operation.detail {
            output.push_str(&format!("    detail: {}\n", safe_text(detail)?));
        }
    }
    if !report.recovery_artifacts.is_empty() {
        output.push_str("Preserved recovery locations (do not delete automatically):\n");
        for artifact in &report.recovery_artifacts {
            output.push_str(&format!("  {}\n", safe_text(artifact)?));
        }
        output.push_str("Inspect the workspace and recovery files manually before any retry.\n");
    }
    Ok(output)
}

pub(crate) fn report_application(report: &ApplicationReport) -> Result<(), String> {
    let rendered = render_application_report(report)?;
    print!("{rendered}");
    if report.status == ApplicationStatus::AppliedAndVerified {
        Ok(())
    } else {
        Err("Patch application did not complete successfully".into())
    }
}

pub(crate) fn safe_cli_error(message: &str) -> String {
    safe_text(message)
        .unwrap_or_else(|_| "Operation failed; unsafe terminal text was suppressed".into())
}

fn safe_text(value: &str) -> Result<String, String> {
    escape_terminal_text(value).map_err(|_| "Could not safely render terminal text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::patch::{FilePatch, PatchOperation, TextEdit};
    use crate::agent::patch_application::apply_approved_patch;
    use crate::agent::patch_approval::approve_with_io;
    use crate::agent::session::{ContributionPlanReference, PlanReviewStatus};
    use crate::github::GitHubRepository;
    use crate::planning::ContributionPlan;
    use crate::workspace::{create_workspace_from_source_for_tests, preview_workspace_for_tests};
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        prepared: WorkspaceResult,
        context: ContributionContext,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "hecate-contribute-cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let source = root.join("source");
            fs::create_dir_all(source.join("src")).unwrap();
            fs::write(source.join("src/lib.rs"), "before\n").unwrap();
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
            let repository = GitHubRepository {
                owner: "owner".into(),
                name: "repo".into(),
                url: "https://github.com/owner/repo".into(),
                default_branch: "main".into(),
                commit,
            };
            let plan = ContributionPlan {
                repository_url: repository.url.clone(),
                issue_number: 5,
                issue_title: "Change behavior".into(),
                issue_url: "https://github.com/owner/repo/issues/5".into(),
                labels: vec!["bug".into()],
                created_at: "now".into(),
                updated_at: "now".into(),
                comments: 0,
                expected_outcome: "Updated behavior".into(),
                interpretation: "A possible issue".into(),
                locations: vec![],
                documents: vec![],
                steps: vec!["Update behavior".into()],
                test_recommendations: vec![],
                validation_commands: vec![],
                risks_and_assumptions: vec![],
                unresolved_questions: vec![],
            };
            let preview =
                preview_workspace_for_tests(&repository, 5, &root.join("workspaces")).unwrap();
            let prepared = create_workspace_from_source_for_tests(
                &repository,
                &plan,
                &preview,
                source.to_str().unwrap(),
            )
            .unwrap();
            let context = ContributionContext {
                repository_url: repository.url.clone(),
                issue_number: 5,
                issue_title: plan.issue_title.clone(),
                plan: ContributionPlanReference {
                    artifact_id: ".hecate/plan-v1.json".into(),
                    repository_url: repository.url.clone(),
                    issue_number: 5,
                    schema_version: crate::workspace::PLAN_SCHEMA_VERSION,
                    review_status: PlanReviewStatus::Approved,
                },
            };
            Self {
                root,
                source,
                prepared,
                context,
            }
        }

        fn proposal(&self) -> PatchProposal {
            PatchProposal::new(vec![
                FilePatch {
                    path: "src/lib.rs".into(),
                    operation: PatchOperation::Replace {
                        expected_original_sha256: digest(b"before\n"),
                        edits: vec![TextEdit {
                            old_text: "before".into(),
                            new_text: "after".into(),
                        }],
                    },
                },
                FilePatch {
                    path: "src/new.rs".into(),
                    operation: PatchOperation::Create {
                        content: "new file\n".into(),
                    },
                },
            ])
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn proposal_loader_bounds_input_and_rejects_json_and_schema_errors() {
        let root = std::env::temp_dir().join(format!(
            "hecate-proposal-loader-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proposal.json");
        fs::write(&path, b"{").unwrap();
        assert!(load_proposal(&path).unwrap_err().contains("Malformed"));
        fs::write(&path, br#"{"schema_version":999,"files":[]}"#).unwrap();
        assert!(load_proposal(&path).unwrap_err().contains("Unsupported"));
        fs::write(&path, vec![b' '; (MAX_PROPOSAL_JSON_BYTES + 1) as usize]).unwrap();
        assert!(load_proposal(&path).unwrap_err().contains("input limit"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn successful_workflow_approves_and_applies_exactly_once() {
        let fixture = Fixture::new();
        let proposal = fixture.proposal();
        let apply_count = Cell::new(0);
        let report = validate_approve_apply(
            &fixture.prepared,
            &fixture.context,
            &proposal,
            "cli-test-session",
            |_patch, _validator| Ok(()),
            |patch, validator, session| {
                let hash = patch.patch_sha256().to_string();
                Ok(approve_with_io(
                    patch,
                    validator,
                    session,
                    true,
                    true,
                    Cursor::new(format!("approve {hash}\n").into_bytes()),
                    &mut Vec::new(),
                )
                .unwrap())
            },
            |approved| {
                apply_count.set(apply_count.get() + 1);
                apply_approved_patch(approved)
            },
        )
        .unwrap();
        assert_eq!(apply_count.get(), 1);
        assert_eq!(report.status, ApplicationStatus::AppliedAndVerified);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/lib.rs")).unwrap(),
            b"after\n"
        );
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/new.rs")).unwrap(),
            b"new file\n"
        );
        assert_eq!(
            fs::read(fixture.source.join("src/lib.rs")).unwrap(),
            b"before\n"
        );
    }

    #[test]
    fn invalid_patch_never_reaches_approval_or_application() {
        let fixture = Fixture::new();
        let mut proposal = fixture.proposal();
        if let PatchOperation::Replace {
            expected_original_sha256,
            ..
        } = &mut proposal.files[0].operation
        {
            *expected_original_sha256 = "0".repeat(64);
        }
        let approvals = Cell::new(0);
        let applications = Cell::new(0);
        let result = validate_approve_apply(
            &fixture.prepared,
            &fixture.context,
            &proposal,
            "cli-test-session",
            |_patch, _validator| Ok(()),
            |patch, validator, session| {
                approvals.set(approvals.get() + 1);
                let hash = patch.patch_sha256().to_string();
                approve_with_io(
                    patch,
                    validator,
                    session,
                    true,
                    true,
                    Cursor::new(format!("approve {hash}\n").into_bytes()),
                    &mut Vec::new(),
                )
            },
            |approved| {
                applications.set(applications.get() + 1);
                apply_approved_patch(approved)
            },
        );
        assert!(result.is_err());
        assert_eq!(approvals.get(), 0);
        assert_eq!(applications.get(), 0);
    }

    #[test]
    fn approval_rejection_never_invokes_application() {
        let fixture = Fixture::new();
        let applications = Cell::new(0);
        let result = validate_approve_apply(
            &fixture.prepared,
            &fixture.context,
            &fixture.proposal(),
            "cli-test-session",
            |_patch, _validator| Ok(()),
            |_patch, _validator, _session| Err(ApprovalError::Rejected),
            |approved| {
                applications.set(applications.get() + 1);
                apply_approved_patch(approved)
            },
        );
        assert!(result.unwrap_err().contains("approval was rejected"));
        assert_eq!(applications.get(), 0);
        assert_eq!(
            fs::read(fixture.prepared.checkout_path.join("src/lib.rs")).unwrap(),
            b"before\n"
        );
    }

    #[test]
    fn failed_application_report_is_not_success() {
        let report = ApplicationReport {
            status: ApplicationStatus::RecoveryRequired,
            error: Some("manual recovery needed".into()),
            operations: vec![],
            recovery_artifacts: vec!["src/.hecate-backup.tmp".into()],
        };
        let rendered = render_application_report(&report).unwrap();
        assert!(rendered.contains("RECOVERY REQUIRED"));
        assert!(rendered.contains("do not delete automatically"));
    }

    #[test]
    fn workspace_refusal_stops_before_workspace_creation() {
        let creations = Cell::new(0);
        let mut input = Cursor::new(b"n\n".as_slice());
        let mut output = Vec::new();
        let created = confirm_and_create_workspace(&mut input, &mut output, || {
            creations.set(creations.get() + 1);
            Ok(())
        })
        .unwrap();
        assert!(created.is_none());
        assert_eq!(creations.get(), 0);
        assert!(String::from_utf8(output).unwrap().contains("Cancelled"));
    }

    #[test]
    fn oversized_workspace_confirmation_fails_before_creation() {
        let creations = Cell::new(0);
        let input_bytes = vec![b'y'; MAX_WORKSPACE_CONFIRMATION_BYTES + 1];
        let mut input = Cursor::new(input_bytes);
        let mut output = Vec::new();
        let result = confirm_and_create_workspace(&mut input, &mut output, || {
            creations.set(creations.get() + 1);
            Ok(())
        });
        assert!(result.unwrap_err().contains("input limit"));
        assert_eq!(creations.get(), 0);
    }
}
