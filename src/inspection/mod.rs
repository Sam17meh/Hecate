use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Basic information discovered about a local repository.
#[derive(Debug)]
pub struct RepositoryInfo {
    pub root: PathBuf,
    pub is_git_repository: bool,
    pub entries: Vec<String>,
    pub project_markers: Vec<String>,
    pub source_files: Vec<String>,
    pub language_counts: Vec<(String, usize)>,
}

/// Directories that should not be scanned.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "vendor",
    "dist",
    "build",
];

/// Identify a programming language from a file extension.
fn detect_language(extension: &str) -> Option<&'static str> {
    match extension {
        "rs" => Some("Rust"),
        "py" => Some("Python"),
        "js" | "jsx" => Some("JavaScript"),
        "ts" | "tsx" => Some("TypeScript"),
        "cpp" | "cc" | "cxx" | "hpp" => Some("C++"),
        "c" => Some("C"),
        "h" => Some("C/C++ Header"),
        "java" => Some("Java"),
        "go" => Some("Go"),
        "rb" => Some("Ruby"),
        "php" => Some("PHP"),
        "cs" => Some("C#"),
        "swift" => Some("Swift"),
        "kt" => Some("Kotlin"),
        "dart" => Some("Dart"),
        "html" => Some("HTML"),
        "css" => Some("CSS"),
        _ => None,
    }
}

/// Recursively discover source files.
fn scan_directory(
    directory: &Path,
    root: &Path,
    source_files: &mut Vec<String>,
    language_counts: &mut BTreeMap<String, usize>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        // Skip ignored directories.
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if IGNORED_DIRS.contains(&name.as_ref()) {
                continue;
            }

            scan_directory(&path, root, source_files, language_counts)?;
        } else if file_type.is_file() {
            if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
                if let Some(language) = detect_language(&extension.to_lowercase()) {
                    let relative_path = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .display()
                        .to_string();

                    source_files.push(relative_path);

                    *language_counts.entry(language.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    Ok(())
}

/// Inspect the structure of a local repository.
pub fn inspect_local(path: &Path) -> io::Result<RepositoryInfo> {
    let root = path.canonicalize()?;

    // Collect top-level entries.
    let mut entries = Vec::new();

    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }

    entries.sort();

    // Check for Git metadata.
    let is_git_repository = root.join(".git").exists();

    // Detect common project marker files.
    let known_markers = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "requirements.txt",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "README.md",
    ];

    let mut project_markers = Vec::new();

    for marker in known_markers {
        if root.join(marker).is_file() {
            project_markers.push(marker.to_string());
        }
    }

    // Recursively scan source files.
    let mut source_files = Vec::new();
    let mut language_counts = BTreeMap::new();

    scan_directory(&root, &root, &mut source_files, &mut language_counts)?;

    source_files.sort();

    Ok(RepositoryInfo {
        root,
        is_git_repository,
        entries,
        project_markers,
        source_files,
        language_counts: language_counts.into_iter().collect(),
    })
}
