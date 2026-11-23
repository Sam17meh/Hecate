//! Human approval for one validated patch. Approval is process-local and never serialized.
#![allow(dead_code)] // The public CLI orchestration is intentionally deferred to milestone 4D.2.3.

use crate::agent::patch::{PatchWorkspaceBinding, ValidatedPatch};
use crate::agent::patch_validation::{PatchValidator, escape_terminal_text};
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_APPROVAL_LINE_BYTES: usize = 128;

/// A single operation covered by a human approval record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ApprovedOperation {
    path: String,
    kind: ApprovedOperationKind,
    original_sha256: Option<String>,
    original_file_identity: Option<String>,
    resulting_sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApprovedOperationKind {
    Create,
    Replace { edit_count: usize },
}

impl ApprovedOperation {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn kind(&self) -> &ApprovedOperationKind {
        &self.kind
    }

    pub(crate) fn original_sha256(&self) -> Option<&str> {
        self.original_sha256.as_deref()
    }

    pub(crate) fn original_file_identity(&self) -> Option<&str> {
        self.original_file_identity.as_deref()
    }

    pub(crate) fn resulting_sha256(&self) -> &str {
        &self.resulting_sha256
    }
}

/// In-memory audit record created only after an exact terminal confirmation.
///
/// It is intentionally not `Clone`, serializable, or deserializable. The session ID is audit
/// context; it is not an authorization credential.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ApprovalRecord {
    patch_sha256: String,
    workspace: PatchWorkspaceBinding,
    session_id: String,
    operations: Vec<ApprovedOperation>,
    approved_at_unix_seconds: u64,
}

impl ApprovalRecord {
    pub(crate) fn patch_sha256(&self) -> &str {
        &self.patch_sha256
    }

    pub(crate) fn workspace(&self) -> &PatchWorkspaceBinding {
        &self.workspace
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn operations(&self) -> &[ApprovedOperation] {
        &self.operations
    }

    pub(crate) fn approved_at_unix_seconds(&self) -> u64 {
        self.approved_at_unix_seconds
    }
}

/// A validated patch paired with the human decision that covers that exact patch and workspace.
/// It is non-cloneable and non-serializable. The controlled application layer consumes it once.
pub(crate) struct ApprovedPatch {
    patch: ValidatedPatch,
    record: ApprovalRecord,
    validator: PatchValidator,
}

impl ApprovedPatch {
    pub(crate) fn patch(&self) -> &ValidatedPatch {
        &self.patch
    }

    pub(crate) fn record(&self) -> &ApprovalRecord {
        &self.record
    }

    /// Recheck the immutable approval binding before a later application stage consumes it.
    pub(crate) fn verify_binding(&self, candidate: &ValidatedPatch) -> Result<(), ApprovalError> {
        verify_record_binding(&self.record, candidate)
    }

    pub(crate) fn into_parts(self) -> (ValidatedPatch, ApprovalRecord, PatchValidator) {
        (self.patch, self.record, self.validator)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApprovalError {
    NonInteractive,
    InvalidSessionId,
    Input(String),
    Rejected,
    BindingMismatch,
    PatchChanged,
    Clock,
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonInteractive => write!(
                formatter,
                "Patch approval requires interactive stdin and stdout terminals"
            ),
            Self::InvalidSessionId => write!(formatter, "Approval session id is invalid"),
            Self::Input(message) => write!(formatter, "Could not read patch approval: {message}"),
            Self::Rejected => write!(formatter, "Patch approval was rejected"),
            Self::BindingMismatch => write!(
                formatter,
                "Approval does not match this validated patch and workspace"
            ),
            Self::PatchChanged => write!(formatter, "Patch targets changed after validation"),
            Self::Clock => write!(formatter, "Could not record the patch approval time"),
        }
    }
}

impl std::error::Error for ApprovalError {}

/// Display the exact validated diff and require the full patch hash to be entered on a real TTY.
/// No environment variable or command-line flag can bypass this check.
pub(crate) fn approve_interactively(
    patch: ValidatedPatch,
    validator: &PatchValidator,
    session_id: &str,
) -> Result<ApprovedPatch, ApprovalError> {
    use std::io::IsTerminal;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    approve_with_confirmation(
        patch,
        validator,
        session_id,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        read_terminal_confirmation,
        &mut stdout,
    )
}

#[cfg(test)]
pub(crate) fn approve_with_io<R: BufRead, W: Write>(
    patch: ValidatedPatch,
    validator: &PatchValidator,
    session_id: &str,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    mut input: R,
    mut output: W,
) -> Result<ApprovedPatch, ApprovalError> {
    approve_with_confirmation(
        patch,
        validator,
        session_id,
        stdin_is_terminal,
        stdout_is_terminal,
        || read_buffered_confirmation(&mut input),
        &mut output,
    )
}

fn approve_with_confirmation<W: Write>(
    patch: ValidatedPatch,
    validator: &PatchValidator,
    session_id: &str,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    read_confirmation: impl FnOnce() -> io::Result<String>,
    mut output: W,
) -> Result<ApprovedPatch, ApprovalError> {
    if !stdin_is_terminal || !stdout_is_terminal {
        return Err(ApprovalError::NonInteractive);
    }
    if session_id.trim().is_empty()
        || session_id.len() > 256
        || session_id.chars().any(char::is_control)
    {
        return Err(ApprovalError::InvalidSessionId);
    }
    validator
        .revalidate_patch_snapshot(&patch)
        .map_err(map_snapshot_error)?;

    let repository = escape_terminal_text(&patch.workspace.repository_url)
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    let commit = escape_terminal_text(&patch.workspace.checkout_commit)
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    let branch = escape_terminal_text(&patch.workspace.branch)
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    writeln!(output, "HECATE PATCH REVIEW")
        .and_then(|_| writeln!(output, "Repository: {repository}"))
        .and_then(|_| writeln!(output, "Commit: {commit}"))
        .and_then(|_| writeln!(output, "Branch: {branch}"))
        .and_then(|_| writeln!(output, "Patch SHA-256: {}", patch.patch_sha256()))
        .and_then(|_| writeln!(output, "Files: {}", patch.changes().len()))
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    for change in patch.changes() {
        let operation = if change.is_new_file() {
            "create"
        } else {
            "modify"
        };
        let path = escape_terminal_text(change.path())
            .map_err(|error| ApprovalError::Input(error.to_string()))?;
        writeln!(
            output,
            "  {operation}: {} (result SHA-256: {})",
            path,
            change.resulting_sha256()
        )
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    }
    writeln!(output, "\n{}", patch.summary().preview)
        .and_then(|_| {
            write!(
                output,
                "Type `approve {}` to approve exactly this patch: ",
                patch.patch_sha256()
            )
        })
        .and_then(|_| output.flush())
        .map_err(|error| ApprovalError::Input(error.to_string()))?;

    let line = match read_confirmation() {
        Ok(line) => line,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return Err(ApprovalError::Rejected);
        }
        Err(error) => return Err(ApprovalError::Input(error.to_string())),
    };
    let line = normalize_confirmation_line(line)?;
    if line != format!("approve {}", patch.patch_sha256()) {
        return Err(ApprovalError::Rejected);
    }
    validator
        .revalidate_patch_snapshot(&patch)
        .map_err(map_snapshot_error)?;

    let approved_at_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApprovalError::Clock)?
        .as_secs();
    let record = ApprovalRecord {
        patch_sha256: patch.patch_sha256().to_string(),
        workspace: patch.workspace.clone(),
        session_id: session_id.to_string(),
        operations: approved_operations(&patch),
        approved_at_unix_seconds,
    };
    verify_record_binding(&record, &patch)?;
    let retained_validator = validator
        .try_clone()
        .map_err(|error| ApprovalError::Input(error.to_string()))?;
    Ok(ApprovedPatch {
        patch,
        record,
        validator: retained_validator,
    })
}

fn normalize_confirmation_line(mut line: String) -> Result<String, ApprovalError> {
    if line.len() > MAX_APPROVAL_LINE_BYTES {
        return Err(ApprovalError::Rejected);
    }
    if let Some(without_lf) = line.strip_suffix('\n') {
        line = without_lf.to_string();
    }
    if let Some(without_cr) = line.strip_suffix('\r') {
        line = without_cr.to_string();
    }
    if line.contains(['\n', '\r']) {
        return Err(ApprovalError::Rejected);
    }
    Ok(line)
}

fn read_buffered_confirmation<R: BufRead>(input: &mut R) -> io::Result<String> {
    let mut line = String::new();
    {
        let mut bounded_input =
            std::io::Read::take(&mut *input, (MAX_APPROVAL_LINE_BYTES + 1) as u64);
        bounded_input.read_line(&mut line)?;
    }
    if line.len() > MAX_APPROVAL_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "approval input exceeds the size limit",
        ));
    }
    // This reader is used by deterministic tests only. Production terminal input uses native
    // nonblocking queue inspection below, because fill_buf may block on an empty terminal.
    if !input.fill_buf()?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "additional approval input is queued",
        ));
    }
    Ok(line)
}

#[cfg(unix)]
fn read_terminal_confirmation() -> io::Result<String> {
    use std::os::fd::AsRawFd;

    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let mut bytes = Vec::with_capacity(MAX_APPROVAL_LINE_BYTES);
    loop {
        wait_for_terminal_input(fd, -1)?;
        let mut byte = 0u8;
        // SAFETY: `byte` is writable for one byte and `fd` is the live stdin descriptor.
        let read = unsafe { unix_read(fd, (&mut byte as *mut u8).cast(), 1) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal closed before approval input was complete",
            ));
        }
        bytes.push(byte);
        if bytes.len() > MAX_APPROVAL_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "approval input exceeds the size limit",
            ));
        }
        if byte == b'\n' {
            break;
        }
    }
    if wait_for_terminal_input(fd, 0)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "additional approval input is queued",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "approval input is not UTF-8"))
}

#[cfg(unix)]
#[repr(C)]
struct UnixPollFd {
    fd: std::ffi::c_int,
    events: std::ffi::c_short,
    revents: std::ffi::c_short,
}

#[cfg(unix)]
#[cfg(any(target_os = "linux", target_os = "android"))]
type UnixNfds = std::ffi::c_ulong;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type UnixNfds = std::ffi::c_uint;

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "poll"]
    fn unix_poll(fds: *mut UnixPollFd, count: UnixNfds, timeout: std::ffi::c_int) -> i32;
    #[link_name = "read"]
    fn unix_read(fd: std::ffi::c_int, buffer: *mut std::ffi::c_void, count: usize) -> isize;
}

#[cfg(unix)]
fn wait_for_terminal_input(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    const POLLIN: i16 = 0x0001;
    let mut descriptor = UnixPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `descriptor` points to one valid pollfd and the count matches its length.
        let ready = unsafe { unix_poll(&mut descriptor, 1 as UnixNfds, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(false);
        }
        if descriptor.revents != POLLIN {
            return Err(io::Error::other(
                "could not safely inspect queued terminal input",
            ));
        }
        return Ok(true);
    }
}

#[cfg(windows)]
fn read_terminal_confirmation() -> io::Result<String> {
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleW, STD_INPUT_HANDLE,
    };

    // SAFETY: Windows console APIs are called with the process standard input handle and
    // initialized buffers whose sizes are passed explicitly.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut wide = [0u16; MAX_APPROVAL_LINE_BYTES + 1];
        let mut count = 0u32;
        if ReadConsoleW(
            handle,
            wide.as_mut_ptr().cast(),
            wide.len() as u32,
            &mut count,
            std::ptr::null(),
        ) == 0
        {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if count as usize > MAX_APPROVAL_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "approval input exceeds the size limit",
            ));
        }
        let mut queued = 0u32;
        if GetNumberOfConsoleInputEvents(handle, &mut queued) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if queued != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "additional approval input is queued",
            ));
        }
        String::from_utf16(&wide[..count as usize]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "approval input is not valid UTF-16",
            )
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn read_terminal_confirmation() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe terminal input queue inspection is unavailable on this platform",
    ))
}

fn approved_operations(patch: &ValidatedPatch) -> Vec<ApprovedOperation> {
    patch
        .changes()
        .iter()
        .map(|change| ApprovedOperation {
            path: change.path().to_string(),
            kind: if change.is_new_file() {
                ApprovedOperationKind::Create
            } else {
                ApprovedOperationKind::Replace {
                    edit_count: change.edit_count(),
                }
            },
            original_sha256: change.original_sha256().map(str::to_string),
            original_file_identity: change.original_file_identity().map(str::to_string),
            resulting_sha256: change.resulting_sha256().to_string(),
        })
        .collect()
}

fn map_snapshot_error(error: crate::agent::patch::PatchValidationError) -> ApprovalError {
    match error {
        crate::agent::patch::PatchValidationError::Workspace(_) => ApprovalError::BindingMismatch,
        _ => ApprovalError::PatchChanged,
    }
}

pub(crate) fn verify_record_binding(
    record: &ApprovalRecord,
    patch: &ValidatedPatch,
) -> Result<(), ApprovalError> {
    if record.patch_sha256 != patch.patch_sha256()
        || record.workspace != patch.workspace
        || record.operations != approved_operations(patch)
    {
        return Err(ApprovalError::BindingMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::patch_validation::PatchValidator;
    use crate::agent::session::{ContributionContext, ContributionPlanReference, PlanReviewStatus};
    use crate::github::GitHubRepository;
    use crate::planning::ContributionPlan;
    use crate::workspace::{create_workspace_from_source_for_tests, preview_workspace_for_tests};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Cursor;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hecate-approval-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn git(path: &std::path::Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_AUTHOR_NAME", "Hecate Test")
            .env("GIT_AUTHOR_EMAIL", "hecate@example.invalid")
            .env("GIT_COMMITTER_NAME", "Hecate Test")
            .env("GIT_COMMITTER_EMAIL", "hecate@example.invalid")
            .output()
            .unwrap()
    }

    struct Fixture {
        root: std::path::PathBuf,
        prepared: crate::workspace::WorkspaceResult,
        context: ContributionContext,
        validator: PatchValidator,
    }

    impl Fixture {
        fn new() -> Self {
            let root = temp_root();
            let source = root.join("source");
            fs::create_dir_all(source.join("src")).unwrap();
            fs::write(source.join("src/lib.rs"), "before\nafter\n").unwrap();
            assert!(
                git(&source, &["init", "--initial-branch=main"])
                    .status
                    .success()
            );
            assert!(git(&source, &["add", "."]).status.success());
            assert!(git(&source, &["commit", "-m", "initial"]).status.success());
            let commit = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_string();
            let repository = GitHubRepository {
                owner: "owner".into(),
                name: "repo".into(),
                url: "https://github.com/owner/repo".into(),
                default_branch: "main".into(),
                commit: commit.clone(),
            };
            let plan = ContributionPlan {
                repository_url: repository.url.clone(),
                issue_number: 9,
                issue_title: "Example issue".into(),
                issue_url: "https://github.com/owner/repo/issues/9".into(),
                labels: vec![],
                created_at: "now".into(),
                updated_at: "now".into(),
                comments: 0,
                expected_outcome: "update source".into(),
                interpretation: "small change".into(),
                locations: vec![],
                documents: vec![],
                steps: vec!["update source".into()],
                test_recommendations: vec![],
                validation_commands: vec![],
                risks_and_assumptions: vec![],
                unresolved_questions: vec![],
            };
            let preview =
                preview_workspace_for_tests(&repository, 9, &root.join("workspaces")).unwrap();
            let prepared = create_workspace_from_source_for_tests(
                &repository,
                &plan,
                &preview,
                source.to_str().unwrap(),
            )
            .unwrap();
            let context = ContributionContext {
                repository_url: repository.url,
                issue_number: 9,
                issue_title: plan.issue_title.clone(),
                plan: ContributionPlanReference {
                    artifact_id: ".hecate/plan-v1.json".into(),
                    repository_url: plan.repository_url,
                    issue_number: 9,
                    schema_version: crate::workspace::PLAN_SCHEMA_VERSION,
                    review_status: PlanReviewStatus::Approved,
                },
            };
            let validator = PatchValidator::new(&prepared, &context).unwrap();
            Self {
                root,
                prepared,
                context,
                validator,
            }
        }

        fn patch(&self, replacement: &str) -> ValidatedPatch {
            use crate::agent::patch::{PatchOperation, PatchProposal, TextEdit};
            let proposal = PatchProposal::new(vec![crate::agent::patch::FilePatch {
                path: "src/lib.rs".into(),
                operation: PatchOperation::Replace {
                    expected_original_sha256: digest(b"before\nafter\n"),
                    edits: vec![TextEdit {
                        old_text: "before".into(),
                        new_text: replacement.into(),
                    }],
                },
            }]);
            self.validator.validate(&proposal).unwrap()
        }

        fn patch_with_create(&self, replacement: &str) -> ValidatedPatch {
            use crate::agent::patch::{FilePatch, PatchOperation, PatchProposal, TextEdit};
            let proposal = PatchProposal::new(vec![
                FilePatch {
                    path: "src/lib.rs".into(),
                    operation: PatchOperation::Replace {
                        expected_original_sha256: digest(b"before\nafter\n"),
                        edits: vec![TextEdit {
                            old_text: "before".into(),
                            new_text: replacement.into(),
                        }],
                    },
                },
                FilePatch {
                    path: "src/new.rs".into(),
                    operation: PatchOperation::Create {
                        content: "pub fn created() {}\n".into(),
                    },
                },
            ]);
            self.validator.validate(&proposal).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn confirm<W: Write>(
        patch: ValidatedPatch,
        validator: &PatchValidator,
        input: &str,
        tty: bool,
        output: &mut W,
    ) -> Result<ApprovedPatch, ApprovalError> {
        approve_with_io(
            patch,
            validator,
            "test-session",
            tty,
            tty,
            Cursor::new(input.as_bytes()),
            output,
        )
    }

    fn confirm_with_tty<W: Write>(
        patch: ValidatedPatch,
        validator: &PatchValidator,
        input: impl BufRead,
        stdin_tty: bool,
        stdout_tty: bool,
        output: &mut W,
    ) -> Result<ApprovedPatch, ApprovalError> {
        approve_with_io(
            patch,
            validator,
            "test-session",
            stdin_tty,
            stdout_tty,
            input,
            output,
        )
    }

    #[test]
    fn exact_interactive_confirmation_records_patch_workspace_and_operations() {
        let fixture = Fixture::new();
        let patch = fixture.patch_with_create("changed");
        let expected_hash = patch.patch_sha256().to_string();
        let mut output = Vec::new();
        let approved = confirm(
            patch,
            &fixture.validator,
            &format!("approve {expected_hash}\n"),
            true,
            &mut output,
        )
        .unwrap();
        assert_eq!(approved.record().patch_sha256(), expected_hash);
        assert_eq!(approved.patch().patch_sha256(), expected_hash);
        assert_eq!(approved.record().session_id(), "test-session");
        assert!(approved.record().approved_at_unix_seconds() > 0);
        assert_eq!(approved.record().operations().len(), 2);
        assert_eq!(approved.record().operations()[0].path(), "src/lib.rs");
        assert!(matches!(
            approved.record().operations()[0].kind(),
            ApprovedOperationKind::Replace { edit_count: 1 }
        ));
        assert_eq!(
            approved.record().operations()[0].resulting_sha256().len(),
            64
        );
        assert_eq!(
            approved.record().operations()[0].original_sha256(),
            Some(digest(b"before\nafter\n").as_str())
        );
        assert_eq!(approved.record().operations()[1].path(), "src/new.rs");
        assert_eq!(
            approved.record().operations()[1].kind(),
            &ApprovedOperationKind::Create
        );
        assert_eq!(
            approved.record().workspace().repository_url,
            "https://github.com/owner/repo"
        );
        assert_eq!(
            approved.record().workspace().checkout_commit,
            fixture.prepared.commit
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("@@ -1,2 +1,2 @@")
        );
        assert!(
            fs::read(fixture.prepared.checkout_path.join("src/lib.rs")).unwrap()
                == b"before\nafter\n"
        );
        assert!(!fixture.prepared.checkout_path.join("src/new.rs").exists());
    }

    #[test]
    fn mutated_workspace_display_fields_do_not_change_verified_approval_identity() {
        let mut fixture = Fixture::new();
        fixture.prepared.commit = "attacker-selected-commit".into();
        fixture.prepared.branch = "attacker-selected-branch".into();
        let validator = PatchValidator::new(&fixture.prepared, &fixture.context).unwrap();
        let patch = validator
            .validate(&crate::agent::patch::PatchProposal::new(vec![
                crate::agent::patch::FilePatch {
                    path: "src/lib.rs".into(),
                    operation: crate::agent::patch::PatchOperation::Replace {
                        expected_original_sha256: digest(b"before\nafter\n"),
                        edits: vec![crate::agent::patch::TextEdit {
                            old_text: "before".into(),
                            new_text: "verified".into(),
                        }],
                    },
                },
            ]))
            .unwrap();
        let hash = patch.patch_sha256().to_string();
        let mut output = Vec::new();
        let approved = confirm(
            patch,
            &validator,
            &format!("approve {hash}\n"),
            true,
            &mut output,
        )
        .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        let expected_commit = git(&fixture.prepared.checkout_path, &["rev-parse", "HEAD"]);
        let expected_commit = String::from_utf8(expected_commit.stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(rendered.contains(&format!("Commit: {expected_commit}")));
        assert!(rendered.contains("Branch: hecate/issue-9"));
        assert!(!rendered.contains("attacker-selected"));
        assert_eq!(
            approved.record().workspace().checkout_commit,
            expected_commit
        );
        assert_eq!(approved.record().workspace().branch, "hecate/issue-9");
    }

    #[test]
    fn rejects_wrong_hash_malformed_input_and_non_tty_without_approval() {
        let fixture = Fixture::new();
        for input in ["\n", "approve wrong\n", "approve 0123 extra\n", "no\n"] {
            let mut output = Vec::new();
            assert!(matches!(
                confirm(
                    fixture.patch("changed"),
                    &fixture.validator,
                    input,
                    true,
                    &mut output,
                ),
                Err(ApprovalError::Rejected)
            ));
        }
        let mut output = Vec::new();
        assert!(matches!(
            confirm(
                fixture.patch("changed"),
                &fixture.validator,
                "",
                false,
                &mut output,
            ),
            Err(ApprovalError::NonInteractive)
        ));
        assert!(output.is_empty());

        let mut output = Vec::new();
        assert!(matches!(
            confirm_with_tty(
                fixture.patch("changed"),
                &fixture.validator,
                Cursor::new(b"approve anything\n"),
                true,
                false,
                &mut output,
            ),
            Err(ApprovalError::NonInteractive)
        ));
        assert!(output.is_empty());

        let mut oversized = vec![b'x'; MAX_APPROVAL_LINE_BYTES + 1];
        oversized.push(b'\n');
        let mut output = Vec::new();
        assert!(matches!(
            confirm_with_tty(
                fixture.patch("changed"),
                &fixture.validator,
                Cursor::new(oversized),
                true,
                true,
                &mut output,
            ),
            Err(ApprovalError::Rejected)
        ));
    }

    #[test]
    fn rejects_pasted_trailing_lines_after_an_exact_confirmation() {
        let fixture = Fixture::new();
        let patch = fixture.patch("changed");
        let pasted = format!("approve {}\nextra pasted line\n", patch.patch_sha256());
        let mut output = Vec::new();
        assert!(matches!(
            confirm(patch, &fixture.validator, &pasted, true, &mut output),
            Err(ApprovalError::Rejected)
        ));
    }

    struct WouldBlockAfterLine {
        bytes: Cursor<Vec<u8>>,
    }

    impl std::io::Read for WouldBlockAfterLine {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.bytes, buffer)
        }
    }

    impl BufRead for WouldBlockAfterLine {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let available = self.bytes.fill_buf()?;
            if available.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a second input read would block",
                ))
            } else {
                Ok(available)
            }
        }

        fn consume(&mut self, amount: usize) {
            self.bytes.consume(amount);
        }
    }

    #[test]
    fn fails_closed_when_trailing_input_cannot_be_checked_without_blocking() {
        let fixture = Fixture::new();
        let patch = fixture.patch("changed");
        let challenge = format!("approve {}\n", patch.patch_sha256());
        let input = WouldBlockAfterLine {
            bytes: Cursor::new(challenge.into_bytes()),
        };
        let mut output = Vec::new();
        assert!(matches!(
            approve_with_io(
                patch,
                &fixture.validator,
                "test-session",
                true,
                true,
                input,
                &mut output,
            ),
            Err(ApprovalError::Input(_))
        ));
    }

    #[test]
    fn rejects_patch_substitution_and_workspace_binding_mismatch() {
        let fixture = Fixture::new();
        let first = fixture.patch("changed");
        let second = fixture.patch("different");
        let first_hash = first.patch_sha256().to_string();
        let mut output = Vec::new();
        let approved = confirm(
            first.clone(),
            &fixture.validator,
            &format!("approve {first_hash}\n"),
            true,
            &mut output,
        )
        .unwrap();
        assert_eq!(
            approved.verify_binding(&second),
            Err(ApprovalError::BindingMismatch)
        );
        let mut substituted = first;
        substituted.workspace.checkout_object_id.push_str("-other");
        assert_eq!(
            approved.verify_binding(&substituted),
            Err(ApprovalError::BindingMismatch)
        );

        let mut foreign_workspace = fixture.patch("changed");
        foreign_workspace
            .workspace
            .checkout_object_id
            .push_str("-other");
        let challenge = format!("approve {}\n", foreign_workspace.patch_sha256());
        let mut output = Vec::new();
        assert!(matches!(
            confirm(
                foreign_workspace,
                &fixture.validator,
                &challenge,
                true,
                &mut output,
            ),
            Err(ApprovalError::BindingMismatch)
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn rejects_targets_changed_after_validation_before_approval() {
        let fixture = Fixture::new();
        let patch = fixture.patch("changed");
        fs::write(
            fixture.prepared.checkout_path.join("src/lib.rs"),
            b"changed elsewhere\n",
        )
        .unwrap();
        let challenge = format!("approve {}\n", patch.patch_sha256());
        let mut output = Vec::new();
        assert!(matches!(
            confirm(patch, &fixture.validator, &challenge, true, &mut output,),
            Err(ApprovalError::PatchChanged)
        ));
        assert!(output.is_empty());
    }

    struct MutatingInput {
        bytes: Cursor<Vec<u8>>,
        target: std::path::PathBuf,
        mutated: bool,
    }

    impl std::io::Read for MutatingInput {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.bytes, buffer)
        }
    }

    impl BufRead for MutatingInput {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if !self.mutated {
                fs::write(&self.target, b"changed while approval prompt is open\n")?;
                self.mutated = true;
            }
            self.bytes.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.bytes.consume(amount);
        }
    }

    #[test]
    fn revalidates_targets_after_input_was_received() {
        let fixture = Fixture::new();
        let patch = fixture.patch("changed");
        let challenge = format!("approve {}\n", patch.patch_sha256());
        let input = MutatingInput {
            bytes: Cursor::new(challenge.into_bytes()),
            target: fixture.prepared.checkout_path.join("src/lib.rs"),
            mutated: false,
        };
        let mut output = Vec::new();
        assert!(matches!(
            confirm_with_tty(patch, &fixture.validator, input, true, true, &mut output,),
            Err(ApprovalError::PatchChanged)
        ));
    }

    #[test]
    fn diff_preview_escapes_terminal_controls_and_marks_missing_final_newline() {
        let fixture = Fixture::new();
        let patch = fixture
            .validator
            .validate(&crate::agent::patch::PatchProposal::new(vec![
                crate::agent::patch::FilePatch {
                    path: "src/lib.rs".into(),
                    operation: crate::agent::patch::PatchOperation::Replace {
                        expected_original_sha256: digest(b"before\nafter\n"),
                        edits: vec![crate::agent::patch::TextEdit {
                            old_text: "before\nafter\n".into(),
                            new_text: "\u{1b}[31mowned\u{1b}[0m".into(),
                        }],
                    },
                },
                crate::agent::patch::FilePatch {
                    path: "src/new.rs".into(),
                    operation: crate::agent::patch::PatchOperation::Create {
                        content: "new file".into(),
                    },
                },
            ]))
            .unwrap();
        let preview = &patch.summary().preview;
        assert!(preview.contains("\\x1b[31mowned\\x1b[0m"));
        assert!(preview.contains("--- /dev/null\n+++ b/src/new.rs"));
        assert!(preview.contains("\\ No newline at end of file"));
    }

    #[test]
    fn diff_preview_visibly_escapes_rtl_text_in_paths_and_content() {
        let fixture = Fixture::new();
        let arabic_path = "src/مرحبا.rs";
        let hebrew_content = "const MESSAGE: &str = \"hello שלום אבג\";\n";
        let patch = fixture
            .validator
            .validate(&crate::agent::patch::PatchProposal::new(vec![
                crate::agent::patch::FilePatch {
                    path: arabic_path.into(),
                    operation: crate::agent::patch::PatchOperation::Create {
                        content: "const MIXED: &str = \"abc אבג 123 مرحبا\"; \u{202e}\n".into(),
                    },
                },
                crate::agent::patch::FilePatch {
                    path: "src/hebrew.rs".into(),
                    operation: crate::agent::patch::PatchOperation::Create {
                        content: hebrew_content.into(),
                    },
                },
            ]))
            .unwrap();
        let preview = &patch.summary().preview;
        assert!(preview.contains("src/\\u{645}\\u{631}\\u{62d}\\u{628}\\u{627}.rs"));
        assert!(preview.contains("\\u{5e9}\\u{5dc}\\u{5d5}\\u{5dd}"));
        assert!(preview.contains("\\u{202e}"));
        assert!(!preview.contains('م'));
        assert!(!preview.contains('ש'));
        // The transformation is limited to the preview; validated patch hashes remain content-based.
        assert_eq!(
            patch.changes()[0].resulting_sha256(),
            digest(hebrew_content.as_bytes())
        );
    }
}
