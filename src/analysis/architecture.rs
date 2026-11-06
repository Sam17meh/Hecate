use crate::analysis::dependencies::DependencyGraph;
use crate::analysis::symbols::SymbolAnalysis;
use crate::inspection::RepositoryInfo;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct ArchitectureMap {
    pub files: Vec<FileNode>,
    pub summary: ArchitectureSummary,
    pub diagnostics: Vec<ArchitectureDiagnostic>,
}

#[derive(Debug)]
pub struct FileNode {
    pub path: String,
    pub is_test_file: bool,
    pub symbols: Vec<SymbolEntry>,
    pub dependencies: Vec<String>,
}

#[derive(Debug)]
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub scope: Vec<String>,
    pub is_test: bool,
}

#[derive(Debug, Default)]
pub struct ArchitectureSummary {
    pub total_source_files: usize,
    pub rust_files: usize,
    pub files_with_symbols: usize,
    pub files_without_symbols: usize,
    pub total_symbols: usize,
    pub test_symbols: usize,
    pub module_dependencies: usize,
    pub unresolved_modules: usize,
    pub dependency_parse_errors: usize,
    pub symbol_diagnostics: usize,
}

#[derive(Debug)]
pub struct ArchitectureDiagnostic {
    pub category: String,
    pub file: Option<String>,
    pub message: String,
}

pub fn build_architecture_map(
    repository: &RepositoryInfo,
    dependency_graph: &DependencyGraph,
    symbol_analysis: &SymbolAnalysis,
) -> ArchitectureMap {
    let mut files = BTreeMap::<String, FileNode>::new();

    for path in &repository.source_files {
        let normalized = path.replace('\\', "/");
        let is_test_file = normalized.starts_with("tests/") || normalized.contains("/tests/");

        files.insert(
            path.clone(),
            FileNode {
                path: path.clone(),
                is_test_file,
                symbols: Vec::new(),
                dependencies: Vec::new(),
            },
        );
    }

    for dependency in &dependency_graph.dependencies {
        if let Some(source) = files.get_mut(&dependency.source) {
            source.dependencies.push(dependency.target.clone());
        }
    }

    for symbol in &symbol_analysis.symbols {
        if let Some(file) = files.get_mut(&symbol.file) {
            file.symbols.push(SymbolEntry {
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
                scope: symbol.scope.clone(),
                is_test: symbol.is_test,
            });
        }
    }

    let mut diagnostics = Vec::new();

    for unresolved in &dependency_graph.unresolved_modules {
        let context = if unresolved.module_context.is_empty() {
            String::new()
        } else {
            format!(" (inside {})", unresolved.module_context.join("::"))
        };

        diagnostics.push(ArchitectureDiagnostic {
            category: "unresolved_module".to_string(),
            file: Some(unresolved.source.clone()),
            message: format!(
                "Could not resolve module '{}'{}",
                unresolved.module_name, context
            ),
        });
    }

    for error in &dependency_graph.parse_errors {
        diagnostics.push(ArchitectureDiagnostic {
            category: "dependency_parse_error".to_string(),
            file: Some(error.source.clone()),
            message: error.message.clone(),
        });
    }

    for error in &symbol_analysis.diagnostics {
        diagnostics.push(ArchitectureDiagnostic {
            category: "symbol_analysis_error".to_string(),
            file: Some(error.file.clone()),
            message: error.message.clone(),
        });
    }

    let files: Vec<FileNode> = files.into_values().collect();

    let rust_files = repository
        .source_files
        .iter()
        .filter(|path| path.ends_with(".rs"))
        .count();

    let files_with_symbols = files.iter().filter(|file| !file.symbols.is_empty()).count();

    let summary = ArchitectureSummary {
        total_source_files: repository.source_files.len(),
        rust_files,
        files_with_symbols,
        files_without_symbols: rust_files.saturating_sub(files_with_symbols),
        total_symbols: symbol_analysis.symbols.len(),
        test_symbols: symbol_analysis
            .symbols
            .iter()
            .filter(|symbol| symbol.is_test)
            .count(),
        module_dependencies: dependency_graph.dependencies.len(),
        unresolved_modules: dependency_graph.unresolved_modules.len(),
        dependency_parse_errors: dependency_graph.parse_errors.len(),
        symbol_diagnostics: symbol_analysis.diagnostics.len(),
    };

    ArchitectureMap {
        files,
        summary,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::build_architecture_map;
    use crate::analysis::dependencies::{
        Dependency, DependencyGraph, ParseError, UnresolvedModule,
    };
    use crate::analysis::symbols::{Symbol, SymbolAnalysis, SymbolDiagnostic};
    use crate::inspection::RepositoryInfo;
    use std::path::PathBuf;

    fn repository_fixture() -> RepositoryInfo {
        RepositoryInfo {
            root: PathBuf::from("."),
            is_git_repository: true,
            entries: vec!["src".to_string(), "tests".to_string()],
            project_markers: vec!["Cargo.toml".to_string()],
            source_files: vec![
                "src/main.rs".to_string(),
                "src/lib.rs".to_string(),
                "tests/integration.rs".to_string(),
                "README.md".to_string(),
            ],
            language_counts: vec![("Rust".to_string(), 3)],
        }
    }

    #[test]
    fn builds_file_nodes_and_attaches_symbols_and_dependencies() {
        let repository = repository_fixture();

        let dependency_graph = DependencyGraph {
            dependencies: vec![Dependency {
                source: "src/main.rs".to_string(),
                target: "src/lib.rs".to_string(),
            }],
            unresolved_modules: Vec::new(),
            parse_errors: Vec::new(),
        };

        let symbol_analysis = SymbolAnalysis {
            symbols: vec![Symbol {
                name: "main".to_string(),
                kind: "function".to_string(),
                file: "src/main.rs".to_string(),
                start_line: 1,
                end_line: 5,
                scope: Vec::new(),
                is_test: false,
            }],
            diagnostics: Vec::new(),
        };

        let map = build_architecture_map(&repository, &dependency_graph, &symbol_analysis);

        assert_eq!(map.files.len(), 4);

        let main_file = map
            .files
            .iter()
            .find(|file| file.path == "src/main.rs")
            .unwrap();

        assert_eq!(main_file.dependencies, vec!["src/lib.rs"]);
        assert_eq!(main_file.symbols.len(), 1);
        assert_eq!(main_file.symbols[0].name, "main");

        let test_file = map
            .files
            .iter()
            .find(|file| file.path == "tests/integration.rs")
            .unwrap();

        assert!(test_file.is_test_file);
    }

    #[test]
    fn calculates_repository_summary_correctly() {
        let repository = repository_fixture();

        let dependency_graph = DependencyGraph {
            dependencies: vec![Dependency {
                source: "src/main.rs".to_string(),
                target: "src/lib.rs".to_string(),
            }],
            unresolved_modules: Vec::new(),
            parse_errors: Vec::new(),
        };

        let symbol_analysis = SymbolAnalysis {
            symbols: vec![
                Symbol {
                    name: "main".to_string(),
                    kind: "function".to_string(),
                    file: "src/main.rs".to_string(),
                    start_line: 1,
                    end_line: 5,
                    scope: Vec::new(),
                    is_test: false,
                },
                Symbol {
                    name: "integration_test".to_string(),
                    kind: "function".to_string(),
                    file: "tests/integration.rs".to_string(),
                    start_line: 1,
                    end_line: 4,
                    scope: Vec::new(),
                    is_test: true,
                },
            ],
            diagnostics: Vec::new(),
        };

        let map = build_architecture_map(&repository, &dependency_graph, &symbol_analysis);

        assert_eq!(map.summary.total_source_files, 4);
        assert_eq!(map.summary.rust_files, 3);
        assert_eq!(map.summary.files_with_symbols, 2);
        assert_eq!(map.summary.files_without_symbols, 1);
        assert_eq!(map.summary.total_symbols, 2);
        assert_eq!(map.summary.test_symbols, 1);
        assert_eq!(map.summary.module_dependencies, 1);
    }

    #[test]
    fn converts_analysis_problems_into_architecture_diagnostics() {
        let repository = repository_fixture();

        let dependency_graph = DependencyGraph {
            dependencies: Vec::new(),
            unresolved_modules: vec![UnresolvedModule {
                source: "src/main.rs".to_string(),
                module_name: "missing".to_string(),
                module_context: Vec::new(),
            }],
            parse_errors: vec![ParseError {
                source: "src/lib.rs".to_string(),
                message: "Syntax errors found".to_string(),
            }],
        };

        let symbol_analysis = SymbolAnalysis {
            symbols: Vec::new(),
            diagnostics: vec![SymbolDiagnostic {
                file: "src/main.rs".to_string(),
                message: "Could not parse symbols".to_string(),
            }],
        };

        let map = build_architecture_map(&repository, &dependency_graph, &symbol_analysis);

        assert_eq!(map.diagnostics.len(), 3);
        assert_eq!(map.summary.unresolved_modules, 1);
        assert_eq!(map.summary.dependency_parse_errors, 1);
        assert_eq!(map.summary.symbol_diagnostics, 1);

        assert!(
            map.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.category == "unresolved_module")
        );
        assert!(
            map.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.category == "dependency_parse_error")
        );
        assert!(
            map.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.category == "symbol_analysis_error")
        );
    }
}
