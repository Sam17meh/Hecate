use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn root() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "hecate-contribute-cli-it-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hecate"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("HECATE CLI should run")
}

#[test]
fn contribute_help_describes_proposal_and_workflow() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let output = run(&directory, &["contribute", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("REPOSITORY"));
    assert!(help.contains("ISSUE_NUMBER"));
    assert!(help.contains("--proposal"));
    assert!(help.contains("--generate"));
    assert!(help.contains("--workspace-dir"));
    assert!(help.contains("approval"));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn proposal_sources_are_required_and_mutually_exclusive() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let no_source = run(&directory, &["contribute", "owner/repo", "12"]);
    assert!(!no_source.status.success());
    let missing_source_error = String::from_utf8_lossy(&no_source.stderr);
    assert!(
        missing_source_error.contains("--proposal") || missing_source_error.contains("--generate")
    );

    let proposal = directory.join("proposal.json");
    fs::write(&proposal, br#"{"schema_version":1,"files":[]}"#).unwrap();
    let both_sources = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            proposal.to_str().unwrap(),
            "--generate",
        ],
    );
    assert!(!both_sources.status.success());
    assert!(String::from_utf8_lossy(&both_sources.stderr).contains("cannot be used with"));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn contribute_requires_repository_issue_and_proposal_arguments() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let no_args = run(&directory, &["contribute"]);
    assert!(!no_args.status.success());
    let no_proposal = run(&directory, &["contribute", "owner/repo", "12"]);
    assert!(!no_proposal.status.success());
    assert!(String::from_utf8_lossy(&no_proposal.stderr).contains("--proposal"));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn invalid_repository_and_zero_issue_fail_before_network_or_workspace_creation() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let proposal = directory.join("proposal.json");
    fs::write(&proposal, br#"{"schema_version":1,"files":[]}"#).unwrap();
    let workspace = directory.join("workspaces");
    let invalid_repository = run(
        &directory,
        &[
            "contribute",
            "not/a/repository/target",
            "12",
            "--proposal",
            proposal.to_str().unwrap(),
            "--workspace-dir",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!invalid_repository.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_repository.stderr).contains("Invalid repository target")
    );
    let zero_issue = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "0",
            "--proposal",
            proposal.to_str().unwrap(),
            "--workspace-dir",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!zero_issue.status.success());
    assert!(String::from_utf8_lossy(&zero_issue.stderr).contains("greater than zero"));
    assert!(!workspace.exists());
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn proposal_file_errors_are_reported_before_github_access() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let missing = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            "missing.json",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Could not inspect"));

    let malformed_path = directory.join("malformed.json");
    fs::write(&malformed_path, b"not json").unwrap();
    let malformed = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            malformed_path.to_str().unwrap(),
        ],
    );
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("Malformed patch proposal JSON"));

    let unsupported_path = directory.join("unsupported.json");
    fs::write(&unsupported_path, br#"{"schema_version":99,"files":[]}"#).unwrap();
    let unsupported = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            unsupported_path.to_str().unwrap(),
        ],
    );
    assert!(!unsupported.status.success());
    assert!(
        String::from_utf8_lossy(&unsupported.stderr).contains("Unsupported patch proposal schema")
    );

    let oversized_path = directory.join("oversized.json");
    fs::write(&oversized_path, vec![b' '; 8 * 1024 * 1024 + 1]).unwrap();
    let oversized = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            oversized_path.to_str().unwrap(),
        ],
    );
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("input limit"));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn non_tty_contribute_fails_closed_before_creating_workspace() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let proposal = directory.join("proposal.json");
    fs::write(&proposal, br#"{"schema_version":1,"files":[]}"#).unwrap();
    let workspace = directory.join("workspaces");
    let output = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--proposal",
            proposal.to_str().unwrap(),
            "--workspace-dir",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires interactive stdin and stdout")
    );
    assert!(!workspace.exists());
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn non_tty_generated_workflow_fails_before_workspace_creation() {
    let directory = root();
    fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("workspaces");
    let output = run(
        &directory,
        &[
            "contribute",
            "owner/repo",
            "12",
            "--generate",
            "--workspace-dir",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires interactive stdin and stdout")
    );
    assert!(!workspace.exists());
    let _ = fs::remove_dir_all(directory);
}
