use crate::github::{GitHubClient, GitHubRepository};
use crate::planning::ContributionPlan;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PLAN_SCHEMA_VERSION: u32 = 1;
const WORKSPACE_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_METADATA_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePreview {
    pub path: PathBuf,
    pub checkout_path: PathBuf,
    pub branch: String,
}

pub struct WorkspaceResult {
    pub path: PathBuf,
    pub checkout_path: PathBuf,
    pub plan_path: PathBuf,
    pub branch: String,
    pub commit: String,
    pub git_status: String,
    // These are the in-process identity anchor for read-only validation. Keep them private so
    // callers can consume preparation results but cannot construct a trusted result themselves.
    workspace_capability: Dir,
    workspace_parent_capability: Dir,
    workspace_name: OsString,
    metadata_capability: Dir,
    checkout_capability: Dir,
    manifest_sha256: String,
    plan_sha256: String,
}

impl WorkspaceResult {
    pub(crate) fn validation_capabilities(&self) -> (&Dir, &Dir, &OsStr, &Dir, &Dir, &str, &str) {
        (
            &self.workspace_capability,
            &self.workspace_parent_capability,
            &self.workspace_name,
            &self.metadata_capability,
            &self.checkout_capability,
            &self.manifest_sha256,
            &self.plan_sha256,
        )
    }
}

impl std::fmt::Debug for WorkspaceResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceResult")
            .field("path", &self.path)
            .field("checkout_path", &self.checkout_path)
            .field("plan_path", &self.plan_path)
            .field("branch", &self.branch)
            .field("commit", &self.commit)
            .field("git_status", &self.git_status)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredContributionPlan {
    pub schema_version: u32,
    pub plan: ContributionPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceManifest {
    schema_version: u32,
    repository_url: String,
    issue_number: u64,
    branch: String,
    base_branch: String,
    base_commit: String,
    prepared_at_unix_seconds: u64,
    checkout_path: String,
    plan_path: String,
    initial_git_status: String,
}

pub fn default_workspace_root() -> PathBuf {
    if let Some(root) = std::env::var_os("HECATE_WORKSPACE_DIR") {
        return PathBuf::from(root);
    }
    let home = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"));
    home.map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".hecate")
        .join("workspaces")
}

pub fn preview_workspace(
    repository: &GitHubRepository,
    issue_number: u64,
    root: &Path,
) -> Result<WorkspacePreview, String> {
    validate_repository(repository)?;
    if issue_number == 0 {
        return Err("Issue number must be greater than zero".to_string());
    }
    let branch = format!("hecate/issue-{issue_number}");
    let path = root
        .join(workspace_component("owner", &repository.owner)?)
        .join(workspace_component("repo", &repository.name)?)
        .join(format!("issue-{issue_number}"));
    if path.exists() {
        return Err(format!(
            "Workspace already exists at {}; it will not be overwritten",
            path.display()
        ));
    }
    Ok(WorkspacePreview {
        checkout_path: path.join("repository"),
        path,
        branch,
    })
}

pub fn create_workspace(
    repository: &GitHubRepository,
    issue_number: u64,
    plan: &ContributionPlan,
    root: &Path,
    approved: bool,
) -> Result<WorkspaceResult, String> {
    if !approved {
        return Err("Workspace creation was cancelled; no files were created".to_string());
    }
    validate_repository(repository)?;
    validate_plan_for_repository(plan, repository, issue_number)?;
    let preview = preview_workspace(repository, issue_number, root)?;
    let source_url = format!(
        "https://github.com/{}/{}.git",
        repository.owner, repository.name
    );
    create_workspace_from_source(repository, plan, &preview, &source_url)
}

fn create_workspace_from_source(
    repository: &GitHubRepository,
    plan: &ContributionPlan,
    preview: &WorkspacePreview,
    source_url: &str,
) -> Result<WorkspaceResult, String> {
    validate_branch_name(&repository.default_branch)?;
    validate_branch_name(&preview.branch)?;
    ensure_remote_branch_available(source_url, &preview.branch)?;
    let parent_path = preview
        .path
        .parent()
        .ok_or_else(|| "Workspace path has no parent directory".to_string())?;
    let workspace_name = preview
        .path
        .file_name()
        .ok_or_else(|| "Workspace path has no final component".to_string())?
        .to_os_string();
    fs::create_dir_all(parent_path)
        .map_err(|error| format!("Could not create workspace parent: {error}"))?;
    // Keep the selected parent open before creating the issue directory and perform creation
    // relative to this handle. The local-CLI threat model excludes a malicious concurrent actor
    // with write access to this parent; mkdir and subsequent no-follow open are not atomic.
    let workspace_parent_capability = Dir::open_ambient_dir(parent_path, ambient_authority())
        .map_err(|error| format!("Could not open selected workspace parent: {error}"))?;
    workspace_parent_capability
        .create_dir(Path::new(&workspace_name))
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "Workspace already exists at {}; it will not be overwritten",
                    preview.path.display()
                )
            } else {
                format!(
                    "Could not reserve workspace {}: {error}",
                    preview.path.display()
                )
            }
        })?;
    // Acquire the exclusive child through its retained parent without following a symlink or
    // reparse point, then bind the entry to this open handle.
    let workspace_capability = open_prepared_directory(
        &workspace_parent_capability,
        &workspace_name,
        "new workspace",
    )?;

    workspace_capability
        .create_dir(".hecate")
        .map_err(|error| {
            format!(
                "Workspace was reserved at {}, but metadata directory creation failed: {error}",
                preview.path.display()
            )
        })?;
    let metadata_capability = open_prepared_directory(
        &workspace_capability,
        OsStr::new(".hecate"),
        "workspace metadata",
    )?;
    metadata_capability.create_dir("hooks-disabled").map_err(|error| {
        format!(
            "Workspace was reserved at {}, but Git hook isolation could not be prepared: {error}",
            preview.path.display()
        )
    })?;

    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        None,
        "before Git clone",
    )?;
    let clone = git_workspace_command(
        None,
        &preview.path.join(".hecate"),
        [
            "clone",
            "--single-branch",
            "--branch",
            repository.default_branch.as_str(),
            source_url,
            preview.checkout_path.to_str().ok_or_else(|| {
                format!(
                    "Workspace path is not valid UTF-8: {}",
                    preview.checkout_path.display()
                )
            })?,
        ],
    );
    let clone_output = match clone {
        Ok(output) => output,
        Err(error) => {
            return Err(format!(
                "Git clone failed; the reserved workspace was kept for inspection at {}: {error}",
                preview.path.display()
            ));
        }
    };
    ensure_success("clone repository", clone_output).map_err(|error| {
        format!(
            "{error}; the reserved workspace was kept for inspection at {}",
            preview.path.display()
        )
    })?;
    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        None,
        "after Git clone",
    )?;

    let checkout_capability =
        open_dir_child_nofollow(&workspace_capability, OsStr::new("repository"))
            .map_err(|error| format!("Could not retain checkout capability: {error}"))?;
    ensure_directory_entry_identity(
        &workspace_capability,
        OsStr::new("repository"),
        &checkout_capability,
        "new checkout",
    )?;

    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "before branch creation",
    )?;
    let switch = git_workspace_command(
        Some(&preview.checkout_path),
        &preview.path.join(".hecate"),
        [
            "switch",
            "--create",
            preview.branch.as_str(),
            repository.commit.as_str(),
        ],
    )
    .map_err(|error| {
        format!(
            "Could not create contribution branch; workspace remains at {}: {error}",
            preview.path.display()
        )
    })?;
    ensure_success("create contribution branch", switch)
        .map_err(|error| format!("{error}; workspace remains at {}", preview.path.display()))?;
    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "after branch creation",
    )?;

    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "before reading Git status",
    )?;
    let commit = git_text(&preview.checkout_path, ["rev-parse", "HEAD"])?;
    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "after reading Git commit",
    )?;
    if !commit.eq_ignore_ascii_case(&repository.commit) {
        return Err(format!(
            "Prepared branch is at commit {commit}, not the analyzed repository commit {}; workspace remains at {}",
            repository.commit,
            preview.path.display()
        ));
    }
    let branch = git_text(&preview.checkout_path, ["branch", "--show-current"])?;
    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "after reading Git branch",
    )?;
    let git_status = git_text(&preview.checkout_path, ["status", "--short", "--branch"])?;
    ensure_preparation_bindings(
        parent_path,
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        &metadata_capability,
        Some(&checkout_capability),
        "after reading Git status",
    )?;
    let plan_path = preview.path.join(".hecate").join("plan-v1.json");
    let expected_plan_bytes = encode_plan(plan)?;
    write_child_new(
        &metadata_capability,
        OsStr::new("plan-v1.json"),
        &expected_plan_bytes,
    )?;
    verify_child_bytes(
        &metadata_capability,
        OsStr::new("plan-v1.json"),
        &expected_plan_bytes,
        MAX_PLAN_METADATA_BYTES as usize,
    )?;
    let prepared_at_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock error while recording workspace: {error}"))?
        .as_secs();
    let manifest = WorkspaceManifest {
        schema_version: WORKSPACE_SCHEMA_VERSION,
        repository_url: repository.url.clone(),
        issue_number: plan.issue_number,
        branch: branch.clone(),
        base_branch: repository.default_branch.clone(),
        base_commit: commit.clone(),
        prepared_at_unix_seconds,
        checkout_path: "repository".to_string(),
        plan_path: ".hecate/plan-v1.json".to_string(),
        initial_git_status: git_status.clone(),
    };
    let expected_manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("Could not serialize workspace manifest: {error}"))?;
    write_child_new(
        &metadata_capability,
        OsStr::new("workspace-v1.json"),
        &expected_manifest_bytes,
    )
    .map_err(|error| {
        format!(
            "Workspace is ready at {}, but its audit manifest could not be written: {error}",
            preview.path.display()
        )
    })?;

    verify_child_bytes(
        &metadata_capability,
        OsStr::new("workspace-v1.json"),
        &expected_manifest_bytes,
        64 * 1024,
    )?;
    verify_child_bytes(
        &metadata_capability,
        OsStr::new("plan-v1.json"),
        &expected_plan_bytes,
        MAX_PLAN_METADATA_BYTES as usize,
    )?;
    ensure_workspace_path_binding(parent_path, &workspace_parent_capability)?;
    ensure_directory_entry_identity(
        &workspace_parent_capability,
        &workspace_name,
        &workspace_capability,
        "workspace before identity issuance",
    )?;
    ensure_directory_entry_identity(
        &workspace_capability,
        OsStr::new("repository"),
        &checkout_capability,
        "checkout before identity issuance",
    )?;
    ensure_directory_entry_identity(
        &workspace_capability,
        OsStr::new(".hecate"),
        &metadata_capability,
        "metadata before identity issuance",
    )?;

    Ok(WorkspaceResult {
        path: preview.path.clone(),
        checkout_path: preview.checkout_path.clone(),
        plan_path,
        branch,
        commit,
        git_status,
        workspace_capability,
        workspace_parent_capability,
        workspace_name,
        metadata_capability,
        checkout_capability,
        manifest_sha256: metadata_sha256(&expected_manifest_bytes),
        plan_sha256: metadata_sha256(&expected_plan_bytes),
    })
}

#[cfg(test)]
pub(crate) fn preview_workspace_for_tests(
    repository: &GitHubRepository,
    issue_number: u64,
    root: &Path,
) -> Result<WorkspacePreview, String> {
    preview_workspace(repository, issue_number, root)
}

#[cfg(test)]
pub(crate) fn create_workspace_from_source_for_tests(
    repository: &GitHubRepository,
    plan: &ContributionPlan,
    preview: &WorkspacePreview,
    source_url: &str,
) -> Result<WorkspaceResult, String> {
    create_workspace_from_source(repository, plan, preview, source_url)
}

pub fn save_plan(path: &Path, plan: &ContributionPlan) -> Result<(), String> {
    write_json_bytes_new(path, &encode_plan(plan)?)
}

fn encode_plan(plan: &ContributionPlan) -> Result<Vec<u8>, String> {
    validate_plan(plan)?;
    let stored = StoredContributionPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        plan: plan.clone(),
    };
    serde_json::to_vec_pretty(&stored)
        .map_err(|error| format!("Could not serialize contribution plan: {error}"))
}

pub fn load_plan(path: &Path) -> Result<ContributionPlan, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect stored contribution plan: {error}"))?;
    if metadata.len() > MAX_PLAN_METADATA_BYTES {
        return Err("Stored contribution plan exceeds the 2 MiB metadata limit".into());
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|file| {
            file.take(MAX_PLAN_METADATA_BYTES + 1)
                .read_to_end(&mut content)
        })
        .map_err(|error| format!("Could not read stored contribution plan: {error}"))?;
    if content.len() as u64 > MAX_PLAN_METADATA_BYTES {
        return Err("Stored contribution plan exceeds the 2 MiB metadata limit".into());
    }
    decode_plan(&content)
}

fn decode_plan(content: &[u8]) -> Result<ContributionPlan, String> {
    let stored: StoredContributionPlan = serde_json::from_slice(content)
        .map_err(|error| format!("Could not decode stored contribution plan: {error}"))?;
    if stored.schema_version != PLAN_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported contribution plan schema version {}; expected {}",
            stored.schema_version, PLAN_SCHEMA_VERSION
        ));
    }
    validate_plan(&stored.plan)?;
    Ok(stored.plan)
}

/// Validate that a tool session points at the checkout and approved plan recorded when HECATE
/// prepared this workspace. The returned path is canonical and is suitable for capability roots.
pub fn validate_prepared_workspace(
    checkout_path: &Path,
    repository_url: &str,
    issue_number: u64,
    plan_artifact_id: &str,
) -> Result<PathBuf, String> {
    let checkout = fs::canonicalize(checkout_path)
        .map_err(|error| format!("Could not resolve approved workspace checkout: {error}"))?;
    let workspace = checkout
        .parent()
        .ok_or_else(|| "Workspace checkout has no workspace parent".to_string())?;
    let workspace = fs::canonicalize(workspace)
        .map_err(|error| format!("Could not resolve approved workspace: {error}"))?;
    let workspace_dir = Dir::open_ambient_dir(&workspace, ambient_authority())
        .map_err(|error| format!("Could not open prepared workspace metadata root: {error}"))?;
    let metadata_type = workspace_dir
        .symlink_metadata(".hecate")
        .map_err(|error| format!("Could not inspect workspace metadata directory: {error}"))?;
    if metadata_type.file_type().is_symlink() || !metadata_type.is_dir() {
        return Err("Workspace metadata directory is not a regular directory".into());
    }
    let manifest_type = workspace_dir
        .symlink_metadata(".hecate/workspace-v1.json")
        .map_err(|error| format!("Could not inspect workspace approval manifest: {error}"))?;
    if manifest_type.file_type().is_symlink() || !manifest_type.is_file() {
        return Err("Workspace approval manifest is not a regular file".into());
    }
    let manifest_bytes =
        read_workspace_metadata(&workspace_dir, ".hecate/workspace-v1.json", 64 * 1024)?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("Could not decode workspace approval manifest: {error}"))?;
    if manifest.schema_version != WORKSPACE_SCHEMA_VERSION
        || manifest.checkout_path != "repository"
        || manifest.plan_path != ".hecate/plan-v1.json"
        || manifest.issue_number != issue_number
        || manifest.plan_path != plan_artifact_id
    {
        return Err(
            "Workspace manifest does not match the approved issue and plan reference".into(),
        );
    }
    let (expected_owner, expected_name) = GitHubClient::parse_repository(repository_url)?;
    let (manifest_owner, manifest_name) = GitHubClient::parse_repository(&manifest.repository_url)?;
    if !expected_owner.eq_ignore_ascii_case(&manifest_owner)
        || !expected_name.eq_ignore_ascii_case(&manifest_name)
    {
        return Err("Workspace manifest belongs to a different GitHub repository".into());
    }
    let expected_checkout = fs::canonicalize(workspace.join("repository"))
        .map_err(|error| format!("Could not validate workspace checkout: {error}"))?;
    if checkout != expected_checkout {
        return Err("Requested checkout does not match the workspace manifest".into());
    }
    let plan_type = workspace_dir
        .symlink_metadata(&manifest.plan_path)
        .map_err(|error| format!("Could not inspect stored contribution plan: {error}"))?;
    if plan_type.file_type().is_symlink() || !plan_type.is_file() {
        return Err("Stored contribution plan is not a regular file".into());
    }
    let plan_bytes = read_workspace_metadata(
        &workspace_dir,
        &manifest.plan_path,
        MAX_PLAN_METADATA_BYTES as usize,
    )?;
    let plan = decode_plan(&plan_bytes)?;
    if plan.issue_number != issue_number {
        return Err("Stored contribution plan does not match the selected issue".into());
    }
    let (plan_owner, plan_name) = GitHubClient::parse_repository(&plan.repository_url)?;
    if !expected_owner.eq_ignore_ascii_case(&plan_owner)
        || !expected_name.eq_ignore_ascii_case(&plan_name)
    {
        return Err("Stored contribution plan belongs to a different repository".into());
    }
    Ok(checkout)
}

/// Validate metadata through the exact capabilities retained by workspace preparation. The
/// checkout entry is opened relative to the held workspace handle and compared with the held
/// checkout handle; no path lookup is used to re-establish either identity.
pub(crate) fn validate_prepared_workspace_capabilities(
    workspace_parent_capability: &Dir,
    workspace_name: &OsStr,
    workspace_capability: &Dir,
    metadata_capability: &Dir,
    checkout_capability: &Dir,
    expected_manifest_sha256: &str,
    expected_plan_sha256: &str,
    repository_url: &str,
    issue_number: u64,
    plan_artifact_id: &str,
) -> Result<VerifiedWorkspaceMetadata, String> {
    ensure_directory_entry_identity(
        workspace_parent_capability,
        workspace_name,
        workspace_capability,
        "prepared workspace",
    )?;
    ensure_directory_entry_identity(
        workspace_capability,
        OsStr::new(".hecate"),
        metadata_capability,
        "prepared metadata",
    )?;
    let metadata_dir = metadata_capability;
    let manifest_bytes =
        read_child_file_nofollow(&metadata_dir, OsStr::new("workspace-v1.json"), 64 * 1024)?;
    let manifest: WorkspaceManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| "Could not decode prepared workspace manifest".to_string())?;
    if metadata_sha256(&manifest_bytes) != expected_manifest_sha256 {
        return Err("Prepared workspace manifest changed after workspace creation".into());
    }
    if manifest.schema_version != WORKSPACE_SCHEMA_VERSION
        || manifest.checkout_path != "repository"
        || manifest.plan_path != ".hecate/plan-v1.json"
        || manifest.issue_number != issue_number
        || manifest.plan_path != plan_artifact_id
    {
        return Err(
            "Workspace manifest does not match the approved issue and plan reference".into(),
        );
    }
    let (expected_owner, expected_name) = GitHubClient::parse_repository(repository_url)?;
    let (manifest_owner, manifest_name) = GitHubClient::parse_repository(&manifest.repository_url)?;
    if !expected_owner.eq_ignore_ascii_case(&manifest_owner)
        || !expected_name.eq_ignore_ascii_case(&manifest_name)
    {
        return Err("Workspace manifest belongs to a different GitHub repository".into());
    }
    let checkout_entry = open_dir_child_nofollow(workspace_capability, OsStr::new("repository"))
        .map_err(|_| "Prepared checkout entry is missing or unsafe".to_string())?;
    if !same_directory_object(&checkout_entry, checkout_capability)? {
        return Err("Prepared checkout capability does not match its workspace entry".into());
    }
    let plan_bytes = read_child_file_nofollow(
        &metadata_dir,
        OsStr::new("plan-v1.json"),
        MAX_PLAN_METADATA_BYTES as usize,
    )?;
    let plan = decode_plan(&plan_bytes)?;
    if metadata_sha256(&plan_bytes) != expected_plan_sha256 {
        return Err("Approved contribution plan changed after workspace creation".into());
    }
    if plan.issue_number != issue_number {
        return Err("Stored contribution plan does not match the selected issue".into());
    }
    let (plan_owner, plan_name) = GitHubClient::parse_repository(&plan.repository_url)?;
    if !expected_owner.eq_ignore_ascii_case(&plan_owner)
        || !expected_name.eq_ignore_ascii_case(&plan_name)
    {
        return Err("Stored contribution plan belongs to a different repository".into());
    }
    Ok(VerifiedWorkspaceMetadata {
        branch: manifest.branch,
        checkout_commit: manifest.base_commit,
    })
}

/// Metadata parsed from the manifest only after its bytes and workspace capabilities have been
/// verified against the preparation-time identity anchor.
pub(crate) struct VerifiedWorkspaceMetadata {
    pub(crate) branch: String,
    pub(crate) checkout_commit: String,
}

fn ensure_workspace_path_binding(path: &Path, expected_parent: &Dir) -> Result<(), String> {
    let observed = Dir::open_ambient_dir(path, ambient_authority())
        .map_err(|_| "Selected workspace parent path is no longer available".to_string())?;
    if !same_directory_object(&observed, expected_parent)? {
        return Err("Selected workspace parent path changed during preparation".into());
    }
    Ok(())
}

fn ensure_preparation_bindings(
    parent_path: &Path,
    parent: &Dir,
    workspace_name: &OsStr,
    workspace: &Dir,
    metadata: &Dir,
    checkout: Option<&Dir>,
    stage: &str,
) -> Result<(), String> {
    ensure_workspace_path_binding(parent_path, parent)?;
    ensure_directory_entry_identity(
        parent,
        workspace_name,
        workspace,
        &format!("workspace {stage}"),
    )?;
    ensure_directory_entry_identity(
        workspace,
        OsStr::new(".hecate"),
        metadata,
        &format!("metadata {stage}"),
    )?;
    if let Some(checkout) = checkout {
        ensure_directory_entry_identity(
            workspace,
            OsStr::new("repository"),
            checkout,
            &format!("checkout {stage}"),
        )?;
    }
    Ok(())
}

fn ensure_directory_entry_identity(
    parent: &Dir,
    name: &OsStr,
    expected: &Dir,
    label: &str,
) -> Result<(), String> {
    let observed = open_dir_child_nofollow(parent, name)
        .map_err(|_| format!("{label} entry is missing or unsafe"))?;
    if !same_directory_object(&observed, expected)? {
        return Err(format!(
            "{label} entry no longer identifies its retained directory"
        ));
    }
    Ok(())
}

fn open_prepared_directory(parent: &Dir, name: &OsStr, label: &str) -> Result<Dir, String> {
    let directory = open_dir_child_nofollow(parent, name)
        .map_err(|_| format!("Could not safely open {label} directory"))?;
    ensure_directory_entry_identity(parent, name, &directory, label)?;
    Ok(directory)
}

fn write_child_new(parent: &Dir, name: &OsStr, bytes: &[u8]) -> Result<(), String> {
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    let mut file = parent
        .open_with(Path::new(name), &options)
        .map_err(|error| format!("Could not create HECATE metadata file exclusively: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("Could not write HECATE metadata through its capability: {error}"))
}

fn verify_child_bytes(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    limit: usize,
) -> Result<(), String> {
    let actual = read_child_file_nofollow(parent, name, limit)?;
    if actual != expected {
        return Err("Prepared metadata differs from its in-memory expected bytes".into());
    }
    Ok(())
}

fn metadata_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Open a single directory component without following a symlink or reparse point.
pub(crate) fn open_dir_child_nofollow(parent: &Dir, name: &OsStr) -> std::io::Result<Dir> {
    let mut options = CapOpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY).bits() as i32);
    #[cfg(windows)]
    {
        options
            .custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
                    | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
            )
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            );
    }
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "No no-follow directory open is available on this platform",
    ));

    let file = parent.open_with(Path::new(name), &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_dir() || is_reparse_file(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Directory component is not a regular directory",
        ));
    }
    Ok(Dir::from_std_file(file))
}

/// Open a single regular file component without following symlinks/reparse points.
pub(crate) fn open_file_child_nofollow(
    parent: &Dir,
    name: &OsStr,
) -> std::io::Result<std::fs::File> {
    let mut options = CapOpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "No no-follow file open is available on this platform",
    ));

    let file = parent.open_with(Path::new(name), &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_reparse_file(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "File component is not a regular non-reparse file",
        ));
    }
    Ok(file)
}

pub(crate) fn read_child_file_nofollow(
    parent: &Dir,
    name: &OsStr,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut file = open_file_child_nofollow(parent, name)
        .map_err(|_| "Could not safely open workspace metadata file".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "Could not inspect workspace metadata handle".to_string())?;
    if metadata.len() > limit as u64 {
        return Err(format!("Workspace metadata exceeds the {limit}-byte limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(limit as u64) as usize);
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read workspace metadata handle".to_string())?;
    if bytes.len() > limit {
        return Err(format!("Workspace metadata exceeds the {limit}-byte limit"));
    }
    Ok(bytes)
}

fn same_directory_object(left: &Dir, right: &Dir) -> Result<bool, String> {
    let left = left
        .try_clone()
        .map(Dir::into_std_file)
        .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
    let right = right
        .try_clone()
        .map(Dir::into_std_file)
        .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = left
            .metadata()
            .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
        let right = right
            .metadata()
            .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
        let left = unix_file_identity(left.dev(), left.ino())?;
        let right = unix_file_identity(right.dev(), right.ino())?;
        Ok(left == right)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };
        let mut left_info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        let mut right_info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        let left_ok = unsafe {
            GetFileInformationByHandleEx(
                left.as_raw_handle() as HANDLE,
                FileIdInfo,
                left_info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        let right_ok = unsafe {
            GetFileInformationByHandleEx(
                right.as_raw_handle() as HANDLE,
                FileIdInfo,
                right_info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if left_ok == 0 || right_ok == 0 {
            return Err("Could not determine prepared directory handle identity".into());
        }
        let left_info = unsafe { left_info.assume_init() };
        let right_info = unsafe { right_info.assume_init() };
        windows_file_ids_match(
            left_info.VolumeSerialNumber,
            left_info.FileId.Identifier,
            right_info.VolumeSerialNumber,
            right_info.FileId.Identifier,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err("Stable directory identity is unavailable on this platform".into())
    }
}

/// Return an opaque in-process description of the object referenced by a retained directory
/// capability. It is useful for binding an approval to live handles, not for reconstructing trust
/// from a path after the process exits.
pub(crate) fn directory_object_identity(directory: &Dir) -> Result<String, String> {
    let file = directory
        .try_clone()
        .map(Dir::into_std_file)
        .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file
            .metadata()
            .map_err(|_| "Could not inspect prepared directory handle".to_string())?;
        unix_file_identity(metadata.dev(), metadata.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };
        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if succeeded == 0 {
            return Err("Could not determine prepared directory handle identity".into());
        }
        let info = unsafe { info.assume_init() };
        if info.VolumeSerialNumber == 0 || info.FileId.Identifier == [0; 16] {
            return Err("Filesystem did not provide a usable prepared directory identity".into());
        }
        let id = info
            .FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(format!("windows:{}:{id}", info.VolumeSerialNumber))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err("Stable directory identity is unavailable on this platform".into())
    }
}

/// Identify the exact opened regular file object without resolving its path again.
pub(crate) fn file_object_identity(file: &std::fs::File) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file
            .metadata()
            .map_err(|_| "Could not inspect opened file identity".to_string())?;
        if !metadata.is_file() {
            return Err("Opened object is not a regular file".into());
        }
        unix_file_identity(metadata.dev(), metadata.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };
        let metadata = file
            .metadata()
            .map_err(|_| "Could not inspect opened file identity".to_string())?;
        if !metadata.is_file() || is_reparse_file(&metadata) {
            return Err("Opened object is not a regular non-reparse file".into());
        }
        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        // SAFETY: `file` is an open handle and `info` has the API's expected layout.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err("Could not determine opened file identity".into());
        }
        let info = unsafe { info.assume_init() };
        if info.VolumeSerialNumber == 0 || info.FileId.Identifier == [0; 16] {
            return Err("Filesystem did not provide usable opened file identity".into());
        }
        let id = info
            .FileId
            .Identifier
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(format!("windows:{}:{id}", info.VolumeSerialNumber))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err("Stable opened file identity is unavailable on this platform".into())
    }
}

#[cfg(unix)]
fn unix_file_identity(device: u64, inode: u64) -> Result<String, String> {
    if device == 0 || inode == 0 {
        return Err("Filesystem did not provide a usable prepared directory identity".into());
    }
    Ok(format!("unix:{device}:{inode}"))
}

#[cfg(windows)]
fn windows_file_ids_match(
    left_volume: u64,
    left_id: [u8; 16],
    right_volume: u64,
    right_id: [u8; 16],
) -> Result<bool, String> {
    if left_volume == 0 || right_volume == 0 || left_id == [0; 16] || right_id == [0; 16] {
        return Err("Filesystem did not provide a usable prepared directory identity".into());
    }
    Ok(left_volume == right_volume && left_id == right_id)
}

#[cfg(windows)]
fn is_reparse_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn is_reparse_file(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn read_workspace_metadata(root: &Dir, relative: &str, limit: usize) -> Result<Vec<u8>, String> {
    let metadata = root
        .symlink_metadata(relative)
        .map_err(|error| format!("Could not inspect workspace metadata file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Workspace metadata path is not a regular file".into());
    }
    if metadata.len() > limit as u64 {
        return Err(format!("Workspace metadata exceeds the {limit}-byte limit"));
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    root.open(relative)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut content))
        .map_err(|error| format!("Could not read workspace metadata file: {error}"))?;
    if content.len() > limit {
        return Err(format!("Workspace metadata exceeds the {limit}-byte limit"));
    }
    Ok(content)
}

fn write_json_new<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Could not serialize HECATE metadata: {error}"))?;
    write_json_bytes_new(path, &bytes)
}

fn write_json_bytes_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "Could not create metadata file {} without overwriting: {error}",
                path.display()
            )
        })?;
    file.write_all(&bytes)
        .map_err(|error| format!("Could not write metadata file {}: {error}", path.display()))
}

fn validate_plan(plan: &ContributionPlan) -> Result<(), String> {
    if plan.issue_number == 0 {
        return Err("Stored plan has an invalid zero issue number".to_string());
    }
    if plan.issue_title.trim().is_empty() {
        return Err("Stored plan has an empty issue title".to_string());
    }
    let (owner, name) = GitHubClient::parse_repository(&plan.repository_url)?;
    if !valid_component(&owner) || !valid_component(&name) {
        return Err("Stored plan has an invalid GitHub repository URL".to_string());
    }
    Ok(())
}

fn validate_plan_for_repository(
    plan: &ContributionPlan,
    repository: &GitHubRepository,
    issue_number: u64,
) -> Result<(), String> {
    validate_plan(plan)?;
    if plan.issue_number != issue_number {
        return Err(format!(
            "Plan issue #{} does not match requested issue #{issue_number}",
            plan.issue_number
        ));
    }
    let (plan_owner, plan_name) = GitHubClient::parse_repository(&plan.repository_url)?;
    if !plan_owner.eq_ignore_ascii_case(&repository.owner)
        || !plan_name.eq_ignore_ascii_case(&repository.name)
    {
        return Err("Contribution plan repository does not match the selected repository".into());
    }
    Ok(())
}

fn validate_repository(repository: &GitHubRepository) -> Result<(), String> {
    if !valid_component(&repository.owner) || !valid_component(&repository.name) {
        return Err("Invalid GitHub owner or repository name".to_string());
    }
    if repository.default_branch.is_empty()
        || repository.commit.len() != 40
        || !repository
            .commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("GitHub repository is missing its default branch or commit".to_string());
    }
    let (owner, name) = GitHubClient::parse_repository(&repository.url)?;
    if !owner.eq_ignore_ascii_case(&repository.owner)
        || !name.eq_ignore_ascii_case(&repository.name)
    {
        return Err("GitHub repository URL does not match its owner/name metadata".to_string());
    }
    Ok(())
}

fn workspace_component(prefix: &str, value: &str) -> Result<String, String> {
    if valid_component(value) {
        Ok(format!("{prefix}-{value}"))
    } else {
        Err(format!("Invalid repository path component: {value}"))
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_branch_name(branch: &str) -> Result<(), String> {
    let output = git_command(None, ["check-ref-format", "--branch", branch])?;
    ensure_success("validate branch name", output).map(|_| ())
}

fn ensure_remote_branch_available(source_url: &str, branch: &str) -> Result<(), String> {
    let reference = format!("refs/heads/{branch}");
    let output = git_command(
        None,
        ["ls-remote", "--heads", source_url, reference.as_str()],
    )
    .map_err(|error| format!("Could not check for a remote branch-name collision: {error}"))?;
    let output = ensure_success("check remote branch names", output)?;
    if !output.stdout.is_empty() {
        return Err(format!(
            "Remote branch `{branch}` already exists; choose a different issue workspace branch before preparing"
        ));
    }
    Ok(())
}

fn git_text<const N: usize>(directory: &Path, args: [&str; N]) -> Result<String, String> {
    let output = git_command(Some(directory), args)?;
    let output = ensure_success("read Git workspace state", output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_command<const N: usize>(
    directory: Option<&Path>,
    args: [&str; N],
) -> Result<Output, String> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
        .output()
        .map_err(|error| format!("Could not execute Git; ensure Git is installed: {error}"))
}

fn git_workspace_command<const N: usize>(
    directory: Option<&Path>,
    metadata_dir: &Path,
    args: [&str; N],
) -> Result<Output, String> {
    let metadata_dir = fs::canonicalize(metadata_dir)
        .map_err(|error| format!("Could not resolve isolated Git configuration path: {error}"))?;
    let hooks_path = metadata_dir.join("hooks-disabled");
    let isolated_global_config = metadata_dir.join("global-config-disabled");
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks_path.display()))
        .args(args);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", isolated_global_config)
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    command.output().map_err(|error| {
        format!("Could not execute isolated Git; ensure Git is installed: {error}")
    })
}

fn ensure_success(operation: &str, output: Output) -> Result<Output, String> {
    if output.status.success() {
        return Ok(output);
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(format!(
        "Git could not {operation} (exit {}): {}",
        output.status,
        detail.chars().take(600).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning::ContributionPlan;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_root() -> PathBuf {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "hecate-workspace-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn repository() -> GitHubRepository {
        GitHubRepository {
            owner: "example-owner".into(),
            name: "example-repo".into(),
            default_branch: "main".into(),
            commit: "a".repeat(40),
            url: "https://github.com/example-owner/example-repo".into(),
        }
    }

    fn plan() -> ContributionPlan {
        ContributionPlan {
            repository_url: "https://github.com/example-owner/example-repo".into(),
            issue_number: 123,
            issue_title: "Fix the example behavior".into(),
            issue_url: "https://github.com/example-owner/example-repo/issues/123".into(),
            labels: vec!["bug".into()],
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-02T00:00:00Z".into(),
            comments: 0,
            expected_outcome: "The behavior is corrected".into(),
            interpretation: "A reported behavior requires investigation".into(),
            locations: vec![],
            documents: vec![],
            steps: vec!["Review the behavior".into()],
            test_recommendations: vec!["Add a regression test".into()],
            validation_commands: vec!["cargo test".into()],
            risks_and_assumptions: vec!["This is a hypothesis".into()],
            unresolved_questions: vec!["What is expected?".into()],
        }
    }

    fn git(directory: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(directory)
            .output()
            .expect("Git should be installed for workspace tests")
    }

    fn init_source_repository() -> PathBuf {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        assert!(git(&root, &["init", "-b", "main"]).status.success());
        assert!(
            git(&root, &["config", "user.name", "Hecate Test"])
                .status
                .success()
        );
        assert!(
            git(&root, &["config", "user.email", "hecate@example.invalid"])
                .status
                .success()
        );
        fs::write(root.join("README.md"), "original source").unwrap();
        assert!(git(&root, &["add", "README.md"]).status.success());
        assert!(git(&root, &["commit", "-m", "initial"]).status.success());
        root
    }

    #[test]
    fn creates_isolated_checkout_branch_plan_and_audit_manifest() {
        let source = init_source_repository();
        let root = test_root();
        let mut repository = repository();
        repository.commit = git_text(&source, ["rev-parse", "HEAD"]).unwrap();
        let plan = plan();
        let preview = preview_workspace(&repository, 123, &root).unwrap();
        let result =
            create_workspace_from_source(&repository, &plan, &preview, source.to_str().unwrap())
                .unwrap();

        assert_eq!(result.branch, "hecate/issue-123");
        assert_eq!(
            String::from_utf8_lossy(
                &git(&result.checkout_path, &["branch", "--show-current"]).stdout
            )
            .trim(),
            "hecate/issue-123"
        );
        assert!(result.git_status.contains("## hecate/issue-123"));
        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "original source"
        );
        assert!(git(&source, &["status", "--porcelain"]).stdout.is_empty());
        assert_eq!(
            git(&source, &["branch", "--show-current"]).stdout,
            b"main\n"
        );
        assert_eq!(load_plan(&result.plan_path).unwrap(), plan);
        assert!(result.path.join(".hecate/workspace-v1.json").is_file());
        assert_eq!(
            fs::read(&result.plan_path).unwrap(),
            encode_plan(&plan).unwrap()
        );
        let manifest_on_disk = fs::read(result.path.join(".hecate/workspace-v1.json")).unwrap();
        assert_eq!(metadata_sha256(&manifest_on_disk), result.manifest_sha256);
        assert_eq!(
            metadata_sha256(&fs::read(&result.plan_path).unwrap()),
            result.plan_sha256
        );
        drop(result);
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_identity_uses_the_retained_handles() {
        let root = test_root();
        fs::create_dir_all(root.join("left")).unwrap();
        fs::create_dir_all(root.join("right")).unwrap();
        let left = Dir::open_ambient_dir(&root.join("left"), ambient_authority()).unwrap();
        let right = Dir::open_ambient_dir(&root.join("right"), ambient_authority()).unwrap();
        assert!(same_directory_object(&left, &left).unwrap());
        assert!(!same_directory_object(&left, &right).unwrap());
        drop((left, right));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn unix_directory_identity_rejects_zero_device_or_inode() {
        assert!(unix_file_identity(0, 7).is_err());
        assert!(unix_file_identity(7, 0).is_err());
        assert_eq!(unix_file_identity(7, 9).unwrap(), "unix:7:9");
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unavailable_windows_file_identity_values() {
        let nonzero = [1u8; 16];
        assert!(windows_file_ids_match(1, nonzero, 1, nonzero).unwrap());
        assert!(!windows_file_ids_match(1, nonzero, 1, [2u8; 16]).unwrap());
        assert!(windows_file_ids_match(0, nonzero, 0, nonzero).is_err());
        assert!(windows_file_ids_match(1, [0; 16], 1, [0; 16]).is_err());
    }

    #[test]
    fn expected_metadata_bytes_are_authoritative_and_substitutions_fail() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let metadata = root.join("metadata");
        fs::create_dir(&metadata).unwrap();
        let metadata_cap = Dir::open_ambient_dir(&metadata, ambient_authority()).unwrap();
        let expected = b"expected approved plan bytes";
        write_child_new(&metadata_cap, OsStr::new("plan.json"), expected).unwrap();
        verify_child_bytes(&metadata_cap, OsStr::new("plan.json"), expected, 1024).unwrap();
        fs::write(metadata.join("plan.json"), b"substituted plan bytes").unwrap();
        assert!(
            verify_child_bytes(&metadata_cap, OsStr::new("plan.json"), expected, 1024).is_err()
        );
        assert_ne!(
            metadata_sha256(expected),
            metadata_sha256(b"substituted plan bytes")
        );

        fs::write(metadata.join("manifest.json"), b"attacker pre-created file").unwrap();
        assert!(
            write_child_new(
                &metadata_cap,
                OsStr::new("manifest.json"),
                b"expected manifest"
            )
            .is_err()
        );
        drop(metadata_cap);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_substitution_between_workspace_create_and_open() {
        use std::os::unix::fs::symlink;
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let parent = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        parent.create_dir("issue").unwrap();
        let target = root.join("outside");
        fs::create_dir(&target).unwrap();
        fs::rename(root.join("issue"), root.join("created-issue")).unwrap();
        symlink(&target, root.join("issue")).unwrap();

        assert!(open_prepared_directory(&parent, OsStr::new("issue"), "workspace").is_err());
        assert!(
            fs::symlink_metadata(root.join("issue"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.is_dir());
        drop(parent);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn detects_workspace_entry_replacement_without_deleting_the_replacement() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let parent = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        parent.create_dir("issue").unwrap();
        let workspace = open_prepared_directory(&parent, OsStr::new("issue"), "workspace").unwrap();
        fs::rename(root.join("issue"), root.join("original")).unwrap();
        fs::create_dir(root.join("issue")).unwrap();
        fs::write(root.join("issue/attacker-data"), b"preserve me").unwrap();
        assert!(
            ensure_directory_entry_identity(
                &parent,
                OsStr::new("issue"),
                &workspace,
                "workspace at final issuance",
            )
            .is_err()
        );
        assert_eq!(
            fs::read(root.join("issue/attacker-data")).unwrap(),
            b"preserve me"
        );
        drop((workspace, parent));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_checks_reject_checkout_and_metadata_entry_substitution() {
        let root = test_root();
        fs::create_dir_all(root.join("workspace/repository")).unwrap();
        fs::create_dir(root.join("workspace/.hecate")).unwrap();
        let parent = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let workspace =
            open_prepared_directory(&parent, OsStr::new("workspace"), "workspace").unwrap();
        let checkout =
            open_prepared_directory(&workspace, OsStr::new("repository"), "checkout").unwrap();
        let metadata =
            open_prepared_directory(&workspace, OsStr::new(".hecate"), "metadata").unwrap();

        fs::rename(
            root.join("workspace/repository"),
            root.join("workspace/repository-original"),
        )
        .unwrap();
        fs::create_dir(root.join("workspace/repository")).unwrap();
        fs::write(
            root.join("workspace/repository/replacement-marker"),
            b"preserve",
        )
        .unwrap();
        assert!(
            ensure_directory_entry_identity(
                &workspace,
                OsStr::new("repository"),
                &checkout,
                "checkout before issuance",
            )
            .is_err()
        );

        fs::rename(
            root.join("workspace/.hecate"),
            root.join("workspace/metadata-original"),
        )
        .unwrap();
        fs::create_dir(root.join("workspace/.hecate")).unwrap();
        fs::write(
            root.join("workspace/.hecate/replacement-marker"),
            b"preserve",
        )
        .unwrap();
        assert!(
            ensure_directory_entry_identity(
                &workspace,
                OsStr::new(".hecate"),
                &metadata,
                "metadata before issuance",
            )
            .is_err()
        );
        assert_eq!(
            fs::read(root.join("workspace/repository/replacement-marker")).unwrap(),
            b"preserve"
        );
        assert_eq!(
            fs::read(root.join("workspace/.hecate/replacement-marker")).unwrap(),
            b"preserve"
        );

        drop((metadata, checkout, workspace, parent));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_junction_substitution_between_workspace_create_and_open() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let parent = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        parent.create_dir("issue").unwrap();
        let target = root.join("outside");
        fs::create_dir(&target).unwrap();
        fs::remove_dir(root.join("issue")).unwrap();
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:HECATE_TEST_JUNCTION -Target $env:HECATE_TEST_JUNCTION_TARGET | Out-Null",
            ])
            .env("HECATE_TEST_JUNCTION", root.join("issue"))
            .env("HECATE_TEST_JUNCTION_TARGET", &target)
            .output()
            .expect("junction creator must start");
        assert!(
            output.status.success(),
            "junction setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(open_prepared_directory(&parent, OsStr::new("issue"), "workspace").is_err());
        assert!(root.join("issue").exists());
        drop(parent);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_to_overwrite_existing_workspace() {
        let root = test_root();
        let repository = repository();
        let preview = preview_workspace(&repository, 123, &root).unwrap();
        fs::create_dir_all(&preview.path).unwrap();
        fs::write(preview.path.join("user-data.txt"), "keep").unwrap();
        let error = create_workspace(&repository, 123, &plan(), &root, true).unwrap_err();
        assert!(error.contains("will not be overwritten"));
        assert_eq!(
            fs::read_to_string(preview.path.join("user-data.txt")).unwrap(),
            "keep"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_remote_branch_name_collision_before_creating_workspace() {
        let source = init_source_repository();
        assert!(
            git(&source, &["branch", "hecate/issue-123"])
                .status
                .success()
        );
        let root = test_root();
        let mut repository = repository();
        repository.commit = git_text(&source, ["rev-parse", "HEAD"]).unwrap();
        let preview = preview_workspace(&repository, 123, &root).unwrap();
        let error =
            create_workspace_from_source(&repository, &plan(), &preview, source.to_str().unwrap())
                .unwrap_err();
        assert!(error.contains("already exists"));
        assert!(!preview.path.exists());
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_invalid_repository_information() {
        let root = test_root();
        let mut invalid = repository();
        invalid.owner = "../outside".into();
        assert!(preview_workspace(&invalid, 123, &root).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn reports_git_clone_failure_and_keeps_reserved_workspace() {
        let source = init_source_repository();
        let mut repository = repository();
        repository.default_branch = "missing-branch".into();
        let root = test_root();
        let preview = preview_workspace(&repository, 123, &root).unwrap();
        let error =
            create_workspace_from_source(&repository, &plan(), &preview, source.to_str().unwrap())
                .unwrap_err();
        assert!(error.contains("Git could not clone repository"));
        assert!(preview.path.is_dir());
        let _ = fs::remove_dir_all(source);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_creates_nothing_and_plan_files_are_versioned_and_validated() {
        let root = test_root();
        let error = create_workspace(&repository(), 123, &plan(), &root, false).unwrap_err();
        assert!(error.contains("cancelled"));
        assert!(!root.exists());

        let metadata = test_root();
        fs::create_dir_all(&metadata).unwrap();
        let plan_path = metadata.join("plan.json");
        save_plan(&plan_path, &plan()).unwrap();
        let stored: StoredContributionPlan =
            serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
        assert_eq!(stored.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(load_plan(&plan_path).unwrap(), plan());
        let _ = fs::remove_dir_all(metadata);
    }

    #[test]
    fn load_rejects_unknown_schema_and_invalid_plan() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("plan.json");
        let mut stored = StoredContributionPlan {
            schema_version: PLAN_SCHEMA_VERSION + 1,
            plan: plan(),
        };
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(
            load_plan(&path)
                .unwrap_err()
                .contains("Unsupported contribution plan schema")
        );
        stored.schema_version = PLAN_SCHEMA_VERSION;
        stored.plan.issue_title.clear();
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(load_plan(&path).unwrap_err().contains("empty issue title"));
        let _ = fs::remove_dir_all(root);
    }
}
