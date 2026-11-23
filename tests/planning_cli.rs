use std::process::Command;

#[test]
fn plan_command_help_describes_repository_and_issue_selection() {
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .args(["plan", "--help"])
        .output()
        .expect("HECATE binary should start");

    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("ISSUE_NUMBER"));
    assert!(help.contains("REPOSITORY"));
    assert!(help.contains("contribution plan"));
}

#[test]
fn plan_command_rejects_a_non_numeric_issue_selection() {
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .args(["plan", "owner/repo", "not-a-number"])
        .output()
        .expect("HECATE binary should start");

    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid value"));
}
