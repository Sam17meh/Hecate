use crate::inspection::RepositoryInfo;
use std::fs;
use std::path::Path;
use tree_sitter::{Language, Parser};

#[derive(Debug)]
pub struct ModuleDeclaration {
    pub source: String,
    pub module_name: String,
    pub module_context: Vec<String>,
}

#[derive(Debug)]
pub struct DependencyGraph {
    pub dependencies: Vec<Dependency>,
    pub unresolved_modules: Vec<UnresolvedModule>,
    pub parse_errors: Vec<ParseError>,
}

#[derive(Debug)]
pub struct UnresolvedModule {
    pub source: String,
    pub module_name: String,
    pub module_context: Vec<String>,
}

#[derive(Debug)]
pub struct ParseError {
    pub source: String,
    pub message: String,
}

#[derive(Debug)]
pub struct Dependency {
    pub source: String,
    pub target: String,
    pub module_name: String,
    pub module_context: Vec<String>,
}

// Resolve a Rust module declaration to its actual source file.
pub(crate) fn resolve_module_path(
    declaration: &ModuleDeclaration,
    available_files: &[String],
) -> Option<String> {
    let source_path = Path::new(&declaration.source);

    let parent = source_path.parent().unwrap_or_else(|| Path::new(""));

    let file_stem = source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");

    // Rust crate roots and mod.rs use their parent directory.
    // Other module files use a directory named after the file.
    let module_directory = if matches!(file_stem, "main" | "lib" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(file_stem)
    };

    let module_directory = declaration
        .module_context
        .iter()
        .fold(module_directory, |path, module| path.join(module));

    // Rust supports both module layouts.
    let candidates = [
        module_directory.join(format!("{}.rs", declaration.module_name)),
        module_directory
            .join(&declaration.module_name)
            .join("mod.rs"),
    ];

    // Only resolve files that actually exist in our repository scan.
    for candidate in candidates {
        if let Some(found) = available_files
            .iter()
            .find(|file| Path::new(file.as_str()) == candidate.as_path())
        {
            return Some(found.clone());
        }
    }

    None
}

// Build a dependency graph from the repository's Rust module declarations.
pub fn build_dependency_graph(repository: &RepositoryInfo) -> Result<DependencyGraph, String> {
    let mut dependencies = Vec::new();
    let mut unresolved_modules = Vec::new();
    let mut parse_errors = Vec::new();

    for source_file in &repository.source_files {
        if !source_file.ends_with(".rs") {
            continue;
        }

        let absolute_path = repository.root.join(source_file);

        let source_code = fs::read_to_string(&absolute_path)
            .map_err(|error| format!("Failed to read {}: {}", source_file, error))?;

        let declarations = match extract_module_declarations(&source_code, source_file) {
            Ok(declarations) => declarations,
            Err(error) => {
                parse_errors.push(ParseError {
                    source: source_file.clone(),
                    message: error,
                });

                continue;
            }
        };

        for declaration in declarations {
            if let Some(target) = resolve_module_path(&declaration, &repository.source_files) {
                dependencies.push(Dependency {
                    source: source_file.clone(),
                    target,
                    module_name: declaration.module_name,
                    module_context: declaration.module_context,
                });
            } else {
                unresolved_modules.push(UnresolvedModule {
                    source: declaration.source,
                    module_name: declaration.module_name,
                    module_context: declaration.module_context,
                });
            }
        }
    }

    Ok(DependencyGraph {
        dependencies,
        unresolved_modules,
        parse_errors,
    })
}

pub fn extract_module_declarations(
    source_code: &str,
    file_path: &str,
) -> Result<Vec<ModuleDeclaration>, String> {
    let mut parser = Parser::new();

    let language: Language = tree_sitter_rust::LANGUAGE.into();

    parser
        .set_language(&language)
        .map_err(|error| format!("Failed to load Rust grammar: {error}"))?;

    let tree = parser
        .parse(source_code, None)
        .ok_or_else(|| "Failed to parse source code".to_string())?;

    if tree.root_node().has_error() {
        return Err(format!("Syntax errors found in {}", file_path));
    }

    let mut dependencies = Vec::new();
    let mut module_context = Vec::new();

    collect_module_declarations(
        tree.root_node(),
        source_code,
        file_path,
        &mut module_context,
        &mut dependencies,
    )?;

    Ok(dependencies)
}

fn collect_module_declarations(
    parent_node: tree_sitter::Node,
    source_code: &str,
    file_path: &str,
    module_context: &mut Vec<String>,
    dependencies: &mut Vec<ModuleDeclaration>,
) -> Result<(), String> {
    let mut cursor = parent_node.walk();

    for node in parent_node.named_children(&mut cursor) {
        if node.kind() != "mod_item" {
            continue;
        }

        let Some(name_node) = node.child_by_field_name("name") else {
            continue;
        };

        let module_name = name_node
            .utf8_text(source_code.as_bytes())
            .map_err(|error| error.to_string())?
            .to_string();

        // An inline module has a body.
        if let Some(body) = node.child_by_field_name("body") {
            module_context.push(module_name);

            collect_module_declarations(
                body,
                source_code,
                file_path,
                module_context,
                dependencies,
            )?;

            module_context.pop();
        } else {
            // An out-of-line module refers to another source file.
            dependencies.push(ModuleDeclaration {
                source: file_path.to_string(),
                module_name,
                module_context: module_context.clone(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_module_rs_file() {
        let declaration = ModuleDeclaration {
            source: "src/main.rs".to_string(),
            module_name: "inspection".to_string(),
            module_context: vec![],
        };

        let files = vec!["src/inspection.rs".to_string()];

        let result = resolve_module_path(&declaration, &files);

        assert_eq!(result, Some("src/inspection.rs".to_string()));
    }

    #[test]
    fn resolves_module_mod_rs_file() {
        let declaration = ModuleDeclaration {
            source: "src/main.rs".to_string(),
            module_name: "inspection".to_string(),
            module_context: vec![],
        };

        let files = vec!["src/inspection/mod.rs".to_string()];

        let result = resolve_module_path(&declaration, &files);

        assert_eq!(result, Some("src/inspection/mod.rs".to_string()));
    }

    #[test]
    fn resolves_nested_module_file() {
        let declaration = ModuleDeclaration {
            source: "src/analysis/mod.rs".to_string(),
            module_name: "dependencies".to_string(),
            module_context: vec![],
        };

        let files = vec!["src/analysis/dependencies.rs".to_string()];

        let result = resolve_module_path(&declaration, &files);

        assert_eq!(result, Some("src/analysis/dependencies.rs".to_string()));
    }

    #[test]
    fn returns_none_for_unresolved_module() {
        let declaration = ModuleDeclaration {
            source: "src/main.rs".to_string(),
            module_name: "missing".to_string(),
            module_context: vec![],
        };

        let files: Vec<String> = vec![];

        let result = resolve_module_path(&declaration, &files);

        assert_eq!(result, None);
    }

    #[test]
    fn extracts_multiple_module_declarations() {
        let source = r#"
            mod inspection;
            mod analysis;
        "#;

        let result =
            extract_module_declarations(source, "src/main.rs").expect("Parsing should succeed");

        assert_eq!(result.len(), 2);

        assert_eq!(result[0].source, "src/main.rs");
        assert_eq!(result[0].module_name, "inspection");

        assert_eq!(result[1].source, "src/main.rs");
        assert_eq!(result[1].module_name, "analysis");
    }

    #[test]
    fn returns_empty_for_file_without_modules() {
        let source = r#"
            fn main() {
                println!("Hello, Hecate!");
            }
        "#;

        let result =
            extract_module_declarations(source, "src/main.rs").expect("Parsing should succeed");

        assert!(result.is_empty());
    }

    #[test]
    fn ignores_modules_inside_comments_and_strings() {
        let source = r#"
        // mod fake_module;

        fn main() {
            let message = "mod another_fake_module;";
        }
    "#;

        let result =
            extract_module_declarations(source, "src/main.rs").expect("Parsing should succeed");

        assert!(result.is_empty());
    }
    #[test]
    fn builds_complete_dependency_graph() {
        use crate::inspection::RepositoryInfo;
        use std::fs;

        // Create a temporary test repository.
        let root = std::env::temp_dir().join(format!("hecate_graph_test_{}", std::process::id()));

        let analysis_dir = root.join("src").join("analysis");

        fs::create_dir_all(&analysis_dir).expect("Could not create test directories");

        // Create a small Rust module structure.
        fs::write(root.join("src").join("main.rs"), "mod analysis;")
            .expect("Could not write main.rs");

        fs::write(analysis_dir.join("mod.rs"), "pub mod dependencies;")
            .expect("Could not write analysis/mod.rs");

        fs::write(
            analysis_dir.join("dependencies.rs"),
            "// Dependency analyzer",
        )
        .expect("Could not write dependencies.rs");

        // Build repository information.
        let source_files = vec![
            root.join("src")
                .join("main.rs")
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            analysis_dir
                .join("mod.rs")
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            analysis_dir
                .join("dependencies.rs")
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ];

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: vec![],
            project_markers: vec![],
            source_files,
            language_counts: vec![("Rust".to_string(), 3)],
        };

        // Run the actual dependency graph builder.
        let graph = build_dependency_graph(&repository).expect("Graph construction should succeed");

        // Verify the graph contains both expected relationships.
        assert_eq!(graph.dependencies.len(), 2);

        assert!(graph.dependencies.iter().any(|dependency| {
            std::path::Path::new(&dependency.source).ends_with("main.rs")
                && std::path::Path::new(&dependency.target)
                    .ends_with(std::path::Path::new("analysis").join("mod.rs"))
        }));

        assert!(graph.dependencies.iter().any(|dependency| {
            std::path::Path::new(&dependency.source)
                .ends_with(std::path::Path::new("analysis").join("mod.rs"))
                && std::path::Path::new(&dependency.target)
                    .ends_with(std::path::Path::new("analysis").join("dependencies.rs"))
        }));
        // Clean up the temporary repository.
        fs::remove_dir_all(&root).expect("Could not clean up test repository");
    }

    #[test]
    fn extracts_modules_inside_inline_modules() {
        let source = r#"
        mod analysis {
            pub mod dependencies;
        }
    "#;

        let result =
            extract_module_declarations(source, "src/main.rs").expect("Parsing should succeed");

        assert_eq!(result.len(), 1);

        assert_eq!(result[0].module_name, "dependencies");

        assert_eq!(result[0].module_context, vec!["analysis".to_string()]);
    }

    #[test]
    fn resolves_file_inside_inline_module() {
        use crate::inspection::RepositoryInfo;
        use std::fs;

        let root = std::env::temp_dir().join(format!("hecate_inline_test_{}", std::process::id()));

        let analysis_dir = root.join("src").join("analysis");

        fs::create_dir_all(&analysis_dir).expect("Could not create test directories");

        fs::write(
            root.join("src").join("main.rs"),
            "mod analysis { pub mod dependencies; }",
        )
        .expect("Could not write main.rs");

        fs::write(
            analysis_dir.join("dependencies.rs"),
            "// Dependency analyzer",
        )
        .expect("Could not write dependencies.rs");

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: vec![],
            project_markers: vec![],
            source_files: vec![
                "src/main.rs".to_string(),
                "src/analysis/dependencies.rs".to_string(),
            ],
            language_counts: vec![("Rust".to_string(), 2)],
        };

        let graph = build_dependency_graph(&repository).expect("Graph construction should succeed");

        assert_eq!(graph.dependencies.len(), 1);

        assert!(std::path::Path::new(&graph.dependencies[0].source).ends_with("main.rs"));

        assert!(
            std::path::Path::new(&graph.dependencies[0].target)
                .ends_with(std::path::Path::new("analysis").join("dependencies.rs"))
        );

        fs::remove_dir_all(&root).expect("Could not clean up test repository");
    }

    #[test]
    fn detects_unresolved_module_declarations() {
        use crate::inspection::RepositoryInfo;
        use std::fs;

        let root =
            std::env::temp_dir().join(format!("hecate_unresolved_test_{}", std::process::id()));

        let src_dir = root.join("src");

        fs::create_dir_all(&src_dir).expect("Could not create test directory");

        // Declare a module whose source file does not exist.
        fs::write(src_dir.join("main.rs"), "mod authentication;").expect("Could not write main.rs");

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: vec![],
            project_markers: vec![],
            source_files: vec!["src/main.rs".to_string()],
            language_counts: vec![("Rust".to_string(), 1)],
        };

        let graph = build_dependency_graph(&repository).expect("Graph construction should succeed");

        // No dependency should be resolved.
        assert_eq!(graph.dependencies.len(), 0);

        // The missing declaration should be preserved.
        assert_eq!(graph.unresolved_modules.len(), 1);

        let unresolved = &graph.unresolved_modules[0];

        assert_eq!(unresolved.source, "src/main.rs");
        assert_eq!(unresolved.module_name, "authentication");
        assert!(unresolved.module_context.is_empty());

        fs::remove_dir_all(&root).expect("Could not clean up test repository");
    }
    #[test]
    fn detects_syntax_errors() {
        let source = r#"
        fn main() {
            let x = ;
        }
    "#;

        let result = extract_module_declarations(source, "src/main.rs");

        assert!(result.is_err());
    }

    #[test]
    fn continues_analysis_after_syntax_error() {
        let root = std::env::temp_dir().join(format!("hecate-regression-{}", std::process::id()));

        let _ = fs::remove_dir_all(&root);

        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(root.join("src/main.rs"), "fn main() { let broken = ; }").unwrap();

        fs::write(root.join("src/lib.rs"), "mod helper;").unwrap();

        fs::write(root.join("src/helper.rs"), "pub fn help() {}").unwrap();

        let repository = RepositoryInfo {
            root: root.clone(),
            is_git_repository: false,
            entries: vec![],
            project_markers: vec![],
            source_files: vec![
                "src/main.rs".to_string(),
                "src/lib.rs".to_string(),
                "src/helper.rs".to_string(),
            ],
            language_counts: vec![],
        };

        let graph = build_dependency_graph(&repository).unwrap();

        assert_eq!(graph.parse_errors.len(), 1);
        assert_eq!(graph.parse_errors[0].source, "src/main.rs");

        assert_eq!(graph.dependencies.len(), 1);
        assert_eq!(graph.dependencies[0].target, "src/helper.rs");

        assert!(graph.unresolved_modules.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }
}
