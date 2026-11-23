use crate::github::{GitHubClient, GitHubRepository};
use crate::planning::ContributionPlan;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceResult {
    pub path: PathBuf,
    pub checkout_path: PathBuf,
    pub plan_path: PathBuf,
    pub branch: String,
    pub commit: String,
    pub git_status: String,
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
    fs::create_dir_all(
        preview
            .path
            .parent()
            .ok_or_else(|| "Workspace path has no parent directory".to_string())?,
    )
    .map_err(|error| format!("Could not create workspace parent: {error}"))?;
    fs::create_dir(&preview.path).map_err(|error| {
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

    let hecate_dir = preview.path.join(".hecate");
    fs::create_dir(&hecate_dir).map_err(|error| {
        format!(
            "Workspace was reserved at {}, but metadata directory creation failed: {error}",
            preview.path.display()
        )
    })?;
    let disabled_hooks = hecate_dir.join("hooks-disabled");
    fs::create_dir(&disabled_hooks).map_err(|error| {
        format!(
            "Workspace was reserved at {}, but Git hook isolation could not be prepared: {error}",
            preview.path.display()
        )
    })?;
    let clone = git_workspace_command(
        None,
        &hecate_dir,
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

    let switch = git_workspace_command(
        Some(&preview.checkout_path),
        &hecate_dir,
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

    let commit = git_text(&preview.checkout_path, ["rev-parse", "HEAD"])?;
    if !commit.eq_ignore_ascii_case(&repository.commit) {
        return Err(format!(
            "Prepared branch is at commit {commit}, not the analyzed repository commit {}; workspace remains at {}",
            repository.commit,
            preview.path.display()
        ));
    }
    let branch = git_text(&preview.checkout_path, ["branch", "--show-current"])?;
    let git_status = git_text(&preview.checkout_path, ["status", "--short", "--branch"])?;
    let plan_path = hecate_dir.join("plan-v1.json");
    save_plan(&plan_path, plan)?;
    if load_plan(&plan_path)? != *plan {
        return Err(format!(
            "Stored plan failed read-back validation; workspace remains at {}",
            preview.path.display()
        ));
    }
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
    write_json_new(&hecate_dir.join("workspace-v1.json"), &manifest).map_err(|error| {
        format!(
            "Workspace is ready at {}, but its audit manifest could not be written: {error}",
            preview.path.display()
        )
    })?;

    Ok(WorkspaceResult {
        path: preview.path.clone(),
        checkout_path: preview.checkout_path.clone(),
        plan_path,
        branch,
        commit,
        git_status,
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
    validate_plan(plan)?;
    let stored = StoredContributionPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        plan: plan.clone(),
    };
    write_json_new(path, &stored)
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
        let _ = fs::remove_dir_all(source);
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
