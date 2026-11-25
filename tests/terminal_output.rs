use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "hecate-terminal-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn cli_errors_render_terminal_controls_as_visible_text() {
    let root = temp_root("error");
    fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .args(["inspect", "bad\u{1b}[2J\u{1b}]0;title\u{7}"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains('\u{1b}'));
    assert!(!stderr.contains('\u{7}'));
    assert!(stderr.contains("bad\\x1b[2J\\x1b]0;title\\u{7}"));
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn repository_inspection_renders_hostile_path_text_visibly() {
    let root = temp_root("repo-\u{1b}[2J\u{202e}");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("sample.rs"), "pub fn sample() {}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hecate"))
        .args(["inspect", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\u{1b}'));
    assert!(stdout.contains("\\x1b[2J"));
    assert!(stdout.contains("\\u{202e}"));
    let _ = fs::remove_dir_all(root);
}
