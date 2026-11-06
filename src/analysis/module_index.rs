use super::dependencies::{extract_module_declarations, resolve_module_path};
use crate::inspection::RepositoryInfo;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRecord {
    pub crate_root: String,
    pub module_path: Vec<String>,
    pub file: String,
}

#[derive(Debug, Clone)]
pub struct UnresolvedIndexedModule {
    pub crate_root: String,
    pub source: String,
    pub module_name: String,
    pub module_context: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModuleIndexDiagnostic {
    pub crate_root: String,
    pub file: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ModuleIndex {
    pub modules: Vec<ModuleRecord>,
    pub unresolved_modules: Vec<UnresolvedIndexedModule>,
    pub diagnostics: Vec<ModuleIndexDiagnostic>,
}

fn is_rust_file(path: &str) -> bool {
    Path::new(path).extension().and_then(|ext| ext.to_str()) == Some("rs")
}

// Recognize conventional Cargo crate roots.
// Nested targets are recognized through their main.rs files.
fn is_crate_root(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();

    if parts.as_slice() == ["src", "main.rs"] || parts.as_slice() == ["src", "lib.rs"] {
        return true;
    }

    let Some(first) = parts.first().copied() else {
        return false;
    };

    let Some(second) = parts.get(1).copied() else {
        return false;
    };

    if first == "src" && second == "bin" {
        let remaining = &parts[2..];

        return remaining.len() == 1 && is_rust_file(path)
            || remaining.len() >= 2 && parts.last() == Some(&"main.rs");
    }

    if matches!(first, "tests" | "examples" | "benches") {
        let remaining = &parts[1..];

        return remaining.len() == 1 && is_rust_file(path)
            || remaining.len() >= 2 && parts.last() == Some(&"main.rs");
    }

    false
}

fn visit_module(
    repository: &RepositoryInfo,
    crate_root: &str,
    file: &str,
    module_path: Vec<String>,
    visited: &mut HashSet<(String, String)>,
    index: &mut ModuleIndex,
) {
    // A file is visited at most once per crate.
    let visit_key = (crate_root.to_string(), file.to_string());

    if !visited.insert(visit_key) {
        return;
    }

    index.modules.push(ModuleRecord {
        crate_root: crate_root.to_string(),
        module_path: module_path.clone(),
        file: file.to_string(),
    });

    let absolute_path = repository.root.join(file);

    let source = match std::fs::read_to_string(&absolute_path) {
        Ok(source) => source,
        Err(error) => {
            index.diagnostics.push(ModuleIndexDiagnostic {
                crate_root: crate_root.to_string(),
                file: file.to_string(),
                message: format!("Failed to read source file: {error}"),
            });
            return;
        }
    };

    let declarations = match extract_module_declarations(&source, file) {
        Ok(declarations) => declarations,
        Err(error) => {
            index.diagnostics.push(ModuleIndexDiagnostic {
                crate_root: crate_root.to_string(),
                file: file.to_string(),
                message: error,
            });
            return;
        }
    };

    for declaration in declarations {
        let Some(target) = resolve_module_path(&declaration, &repository.source_files) else {
            index.unresolved_modules.push(UnresolvedIndexedModule {
                crate_root: crate_root.to_string(),
                source: declaration.source,
                module_name: declaration.module_name,
                module_context: declaration.module_context,
            });
            continue;
        };

        // Inline module context is part of the logical path.
        let mut child_module_path = module_path.clone();
        child_module_path.extend(declaration.module_context.iter().cloned());
        child_module_path.push(declaration.module_name);

        visit_module(
            repository,
            crate_root,
            &target,
            child_module_path,
            visited,
            index,
        );
    }
}

pub fn build_module_index(repository: &RepositoryInfo) -> ModuleIndex {
    let mut index = ModuleIndex::default();
    let mut visited = HashSet::new();

    let mut crate_roots: Vec<String> = repository
        .source_files
        .iter()
        .filter(|file| is_crate_root(file))
        .cloned()
        .collect();

    crate_roots.sort();

    for crate_root in crate_roots {
        visit_module(
            repository,
            &crate_root,
            &crate_root,
            Vec::new(),
            &mut visited,
            &mut index,
        );
    }

    index.modules.sort_by(|a, b| {
        (&a.crate_root, &a.module_path, &a.file).cmp(&(&b.crate_root, &b.module_path, &b.file))
    });

    index.unresolved_modules.sort_by(|a, b| {
        (&a.crate_root, &a.source, &a.module_context, &a.module_name).cmp(&(
            &b.crate_root,
            &b.source,
            &b.module_context,
            &b.module_name,
        ))
    });

    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_repository() -> (std::path::PathBuf, RepositoryInfo) {
        let root =
            std::env::temp_dir().join(format!("hecate_module_index_test_{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);

        let analysis_dir = root.join("src").join("analysis");
        fs::create_dir_all(&analysis_dir.join("helpers")).unwrap();

        fs::write(root.join("src").join("main.rs"), "mod analysis;").unwrap();

        fs::write(
            analysis_dir.join("mod.rs"),
            "pub mod dependencies; pub mod helpers { pub mod nested; }",
        )
        .unwrap();

        fs::write(analysis_dir.join("dependencies.rs"), "pub fn build() {}").unwrap();

        fs::write(
            analysis_dir.join("helpers").join("nested.rs"),
            "pub fn nested() {}",
        )
        .unwrap();

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: Vec::new(),
            project_markers: Vec::new(),
            source_files: vec![
                "src/main.rs".to_string(),
                "src/analysis/mod.rs".to_string(),
                "src/analysis/dependencies.rs".to_string(),
                "src/analysis/helpers/nested.rs".to_string(),
            ],
            language_counts: vec![("Rust".to_string(), 4)],
        };

        (root, repository)
    }

    #[test]
    fn indexes_nested_and_inline_modules() {
        let (root, repository) = create_repository();

        let index = build_module_index(&repository);

        assert!(index.unresolved_modules.is_empty());
        assert!(index.diagnostics.is_empty());

        assert!(index.modules.iter().any(|module| {
            module.module_path == vec!["analysis".to_string()]
                && module.file == "src/analysis/mod.rs"
        }));

        assert!(index.modules.iter().any(|module| {
            module.module_path == vec!["analysis".to_string(), "dependencies".to_string()]
                && module.file == "src/analysis/dependencies.rs"
        }));

        assert!(index.modules.iter().any(|module| {
            module.module_path
                == vec![
                    "analysis".to_string(),
                    "helpers".to_string(),
                    "nested".to_string(),
                ]
                && module.file == "src/analysis/helpers/nested.rs"
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_unresolved_modules() {
        let root = std::env::temp_dir().join(format!(
            "hecate_module_index_missing_{}",
            std::process::id()
        ));

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "mod missing;").unwrap();

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: Vec::new(),
            project_markers: Vec::new(),
            source_files: vec!["src/main.rs".to_string()],
            language_counts: vec![("Rust".to_string(), 1)],
        };

        let index = build_module_index(&repository);

        assert_eq!(index.unresolved_modules.len(), 1);
        assert_eq!(index.unresolved_modules[0].module_name, "missing");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recognizes_conventional_crate_roots() {
        assert!(is_crate_root("src/main.rs"));
        assert!(is_crate_root("src/lib.rs"));
        assert!(is_crate_root("src/bin/tool.rs"));
        assert!(is_crate_root("tests/integration.rs"));
        assert!(is_crate_root("examples/demo.rs"));
        assert!(is_crate_root("benches/performance.rs"));
        assert!(!is_crate_root("src/analysis/dependencies.rs"));
        assert!(!is_crate_root("tests/common/mod.rs"));
    }
}
