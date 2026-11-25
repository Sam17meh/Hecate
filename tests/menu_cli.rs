use std::process::Command;

#[test]
fn no_arguments_without_interactive_streams_keep_clap_help_behavior() {
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .output()
        .expect("HECATE binary should start with redirected test streams");

    assert_eq!(output.status.code(), Some(2));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("Usage: hecate"));
    assert!(text.contains("inspect"));
    assert!(!text.contains("Select an action (arrow keys"));
}

#[test]
fn explicit_help_remains_noninteractive_and_unchanged() {
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .arg("--help")
        .output()
        .expect("HECATE binary should start");

    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Usage: hecate"));
    assert!(help.contains("contribute"));
    assert!(!help.contains("Select an action (arrow keys"));
}
