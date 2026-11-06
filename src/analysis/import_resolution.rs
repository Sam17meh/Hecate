use super::imports::{ImportDeclaration, extract_imports};
use super::module_index::{ModuleIndex, ModuleRecord};
use super::symbols::{Symbol, SymbolAnalysis};
use crate::inspection::RepositoryInfo;
use std::collections::HashSet;
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CargoDependencyKind {
    Normal,
    Development,
    Build,
}

impl CargoDependencyKind {
    pub fn manifest_section(&self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Development => "dev-dependencies",
            Self::Build => "build-dependencies",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoDependency {
    pub crate_name: String,
    pub package_name: String,
    pub kind: CargoDependencyKind,
    pub version: Option<String>,
}

fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

fn collect_dependency_section(
    manifest: &toml::Value,
    section: &str,
    kind: CargoDependencyKind,
    dependencies: &mut Vec<CargoDependency>,
) {
    let Some(table) = manifest.get(section).and_then(|value| value.as_table()) else {
        return;
    };

    for (alias, value) in table {
        let dependency_table = value.as_table();

        if value.as_str().is_none() && dependency_table.is_none() {
            continue;
        }

        let package_name = dependency_table
            .and_then(|table| table.get("package"))
            .and_then(|value| value.as_str())
            .unwrap_or(alias)
            .to_string();

        let version = value.as_str().map(str::to_string).or_else(|| {
            dependency_table
                .and_then(|table| table.get("version"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        });

        dependencies.push(CargoDependency {
            crate_name: normalize_crate_name(alias),
            package_name,
            kind,
            version,
        });
    }
}

fn parse_cargo_manifest(source: &str) -> Result<Vec<CargoDependency>, String> {
    let manifest: toml::Value =
        toml::from_str(source).map_err(|error| format!("Failed to parse Cargo.toml: {error}"))?;

    let mut dependencies = Vec::new();

    collect_dependency_section(
        &manifest,
        "dependencies",
        CargoDependencyKind::Normal,
        &mut dependencies,
    );

    collect_dependency_section(
        &manifest,
        "dev-dependencies",
        CargoDependencyKind::Development,
        &mut dependencies,
    );

    collect_dependency_section(
        &manifest,
        "build-dependencies",
        CargoDependencyKind::Build,
        &mut dependencies,
    );

    // Also inspect target-specific dependency sections.
    if let Some(targets) = manifest.get("target").and_then(|value| value.as_table()) {
        for target in targets.values() {
            collect_dependency_section(
                target,
                "dependencies",
                CargoDependencyKind::Normal,
                &mut dependencies,
            );

            collect_dependency_section(
                target,
                "dev-dependencies",
                CargoDependencyKind::Development,
                &mut dependencies,
            );

            collect_dependency_section(
                target,
                "build-dependencies",
                CargoDependencyKind::Build,
                &mut dependencies,
            );
        }
    }

    dependencies.sort_by(|left, right| {
        (
            &left.crate_name,
            left.kind,
            &left.package_name,
            &left.version,
        )
            .cmp(&(
                &right.crate_name,
                right.kind,
                &right.package_name,
                &right.version,
            ))
    });

    dependencies.dedup_by(|left, right| {
        left.crate_name == right.crate_name
            && left.package_name == right.package_name
            && left.kind == right.kind
            && left.version == right.version
    });

    Ok(dependencies)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportResolutionKind {
    ResolvedModule,
    ResolvedSymbol,
    StandardLibrary,
    DeclaredDependency,
    ExternalCandidate,
    Unresolved,
    Ambiguous,
}

#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub source: String,
    pub crate_root: String,
    pub module_context: Vec<String>,
    pub path: String,
    pub alias: Option<String>,
    pub is_glob: bool,
    pub line: usize,
    pub kind: ImportResolutionKind,
    pub target_file: Option<String>,
    pub target_symbol: Option<String>,
    pub message: Option<String>,
    pub cargo_dependency: Option<CargoDependency>,
}

#[derive(Debug, Clone)]
pub struct ImportResolutionDiagnostic {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ImportResolutionAnalysis {
    pub imports: Vec<ResolvedImport>,
    pub diagnostics: Vec<ImportResolutionDiagnostic>,
}

fn is_standard_library(first_segment: &str) -> bool {
    matches!(first_segment, "std" | "core" | "alloc")
}

fn current_module_path(module: &ModuleRecord, inline_context: &[String]) -> Vec<String> {
    let mut path = module.module_path.clone();
    path.extend(inline_context.iter().cloned());
    path
}

fn normalize_import_path(path: &str, current_path: &[String]) -> Result<Vec<String>, String> {
    let mut segments: Vec<&str> = path.split("::").filter(|part| !part.is_empty()).collect();

    if segments.is_empty() {
        return Err("Import path is empty".to_string());
    }

    match segments[0] {
        "crate" => {
            segments.remove(0);
            Ok(segments.into_iter().map(str::to_string).collect())
        }
        "self" => {
            segments.remove(0);
            let mut result = current_path.to_vec();
            result.extend(segments.into_iter().map(str::to_string));
            Ok(result)
        }
        "super" => {
            let mut result = current_path.to_vec();

            while segments.first() == Some(&"super") {
                segments.remove(0);

                if result.pop().is_none() {
                    return Err("super:: import moves above the crate root".to_string());
                }
            }

            if segments.first() == Some(&"self") {
                segments.remove(0);
            }

            result.extend(segments.into_iter().map(str::to_string));
            Ok(result)
        }
        _ => Ok(segments.into_iter().map(str::to_string).collect()),
    }
}

fn symbol_exists(symbols: &[Symbol], file: &str, name: &str) -> bool {
    symbols
        .iter()
        .any(|symbol| symbol.file == file && symbol.name == name)
}

fn resolve_against_module(
    import: &ImportDeclaration,
    module: &ModuleRecord,
    index: &ModuleIndex,
    symbols: &[Symbol],
    cargo_dependencies: &[CargoDependency],
) -> ResolvedImport {
    let current_path = current_module_path(module, &import.module_context);

    let mut result = ResolvedImport {
        source: import.source.clone(),
        crate_root: module.crate_root.clone(),
        module_context: import.module_context.clone(),
        path: import.path.clone(),
        alias: import.alias.clone(),
        is_glob: import.is_glob,
        line: import.line,
        kind: ImportResolutionKind::Unresolved,
        target_file: None,
        target_symbol: None,
        message: None,
        cargo_dependency: None,
    };

    let explicit_local = import.path.starts_with("crate::")
        || import.path.starts_with("self::")
        || import.path.starts_with("super::");

    let first_segment = import.path.split("::").next().unwrap_or("");

    if is_standard_library(first_segment) {
        result.kind = ImportResolutionKind::StandardLibrary;
        result.message = Some(
            "Recognized standard-library import; internal resolution is not required".to_string(),
        );
        return result;
    }

    let normalized = match normalize_import_path(&import.path, &current_path) {
        Ok(path) => path,
        Err(message) => {
            result.message = Some(message);
            return result;
        }
    };

    let mut candidates: Vec<&ModuleRecord> = index
        .modules
        .iter()
        .filter(|candidate| {
            candidate.crate_root == module.crate_root
                && normalized.starts_with(&candidate.module_path)
                && (!candidate.module_path.is_empty()
                    || normalized.is_empty()
                    || explicit_local
                    || (normalized.len() == 1
                        && symbol_exists(symbols, &candidate.file, &normalized[0])))
        })
        .collect();

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.module_path.len()));

    let Some(longest) = candidates.first() else {
        if explicit_local {
            result.kind = ImportResolutionKind::Unresolved;
            result.message = Some("No matching local module was found".to_string());
        } else if let Some(dependency) = cargo_dependencies
            .iter()
            .find(|dependency| dependency.crate_name == normalize_crate_name(first_segment))
        {
            result.kind = ImportResolutionKind::DeclaredDependency;
            result.message = Some(format!(
                "Matched Cargo package '{}' through import name '{}' declared in [{}]",
                dependency.package_name,
                dependency.crate_name,
                dependency.kind.manifest_section()
            ));
            result.cargo_dependency = Some(dependency.clone());
        } else {
            result.kind = ImportResolutionKind::ExternalCandidate;
            result.message = Some(
                "No local module or declared Cargo dependency matched this import".to_string(),
            );
        }

        return result;
    };

    let longest_length = longest.module_path.len();

    let matching: Vec<&ModuleRecord> = candidates
        .into_iter()
        .take_while(|candidate| candidate.module_path.len() == longest_length)
        .collect();

    if matching.len() > 1 {
        result.kind = ImportResolutionKind::Ambiguous;
        result.message = Some("Multiple modules match the same logical module path".to_string());
        return result;
    }

    let target = matching[0];
    let remaining = &normalized[longest_length..];

    if remaining.is_empty() {
        result.kind = ImportResolutionKind::ResolvedModule;
        result.target_file = Some(target.file.clone());
        return result;
    }

    if remaining.len() == 1 {
        let item_name = &remaining[0];

        if symbol_exists(symbols, &target.file, item_name) {
            result.kind = ImportResolutionKind::ResolvedSymbol;
            result.target_file = Some(target.file.clone());
            result.target_symbol = Some(item_name.clone());
        } else {
            result.kind = ImportResolutionKind::Unresolved;
            result.target_file = Some(target.file.clone());
            result.message = Some(format!(
                "Module was found, but symbol '{}' was not found in {}",
                item_name, target.file
            ));
        }

        return result;
    }

    result.kind = ImportResolutionKind::Unresolved;
    result.target_file = Some(target.file.clone());
    result.message =
        Some("Import contains additional path segments that are not yet supported".to_string());

    result
}

pub fn build_import_resolution(
    repository: &RepositoryInfo,
    module_index: &ModuleIndex,
    symbol_analysis: &SymbolAnalysis,
) -> ImportResolutionAnalysis {
    let mut analysis = ImportResolutionAnalysis::default();
    let mut processed_files = HashSet::new();

    let cargo_dependencies = match fs::read_to_string(repository.root.join("Cargo.toml")) {
        Ok(manifest) => match parse_cargo_manifest(&manifest) {
            Ok(dependencies) => dependencies,
            Err(message) => {
                analysis.diagnostics.push(ImportResolutionDiagnostic {
                    file: "Cargo.toml".to_string(),
                    message,
                });
                Vec::new()
            }
        },
        Err(error) => {
            analysis.diagnostics.push(ImportResolutionDiagnostic {
                file: "Cargo.toml".to_string(),
                message: format!("Could not read Cargo.toml: {error}"),
            });
            Vec::new()
        }
    };

    for module in &module_index.modules {
        if !processed_files.insert(module.file.clone()) {
            continue;
        }

        let absolute_path = repository.root.join(&module.file);

        let source = match fs::read_to_string(&absolute_path) {
            Ok(source) => source,
            Err(error) => {
                analysis.diagnostics.push(ImportResolutionDiagnostic {
                    file: module.file.clone(),
                    message: format!("Failed to read source file: {error}"),
                });
                continue;
            }
        };

        let declarations = match extract_imports(&source, &module.file) {
            Ok(imports) => imports,
            Err(error) => {
                analysis.diagnostics.push(ImportResolutionDiagnostic {
                    file: module.file.clone(),
                    message: error,
                });
                continue;
            }
        };

        let matching_modules: Vec<&ModuleRecord> = module_index
            .modules
            .iter()
            .filter(|record| record.file == module.file)
            .collect();

        for declaration in &declarations {
            for source_module in &matching_modules {
                analysis.imports.push(resolve_against_module(
                    declaration,
                    source_module,
                    module_index,
                    &symbol_analysis.symbols,
                    &cargo_dependencies,
                ));
            }
        }
    }

    analysis.imports.sort_by(|a, b| {
        (&a.source, a.line, &a.path, &a.crate_root).cmp(&(
            &b.source,
            b.line,
            &b.path,
            &b.crate_root,
        ))
    });

    analysis
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(path: Vec<&str>, file: &str) -> ModuleRecord {
        ModuleRecord {
            crate_root: "src/main.rs".to_string(),
            module_path: path.into_iter().map(str::to_string).collect(),
            file: file.to_string(),
        }
    }

    fn declaration(path: &str) -> ImportDeclaration {
        ImportDeclaration {
            source: "src/main.rs".to_string(),
            module_context: Vec::new(),
            path: path.to_string(),
            alias: None,
            is_glob: false,
            line: 1,
        }
    }

    fn test_index() -> ModuleIndex {
        ModuleIndex {
            modules: vec![
                module(vec![], "src/main.rs"),
                module(vec!["analysis"], "src/analysis/mod.rs"),
                module(vec!["analysis", "symbols"], "src/analysis/symbols.rs"),
            ],
            unresolved_modules: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn test_symbols() -> Vec<Symbol> {
        vec![Symbol {
            name: "Symbol".to_string(),
            kind: "struct".to_string(),
            file: "src/analysis/symbols.rs".to_string(),
            start_line: 1,
            end_line: 5,
            scope: Vec::new(),
            is_test: false,
        }]
    }

    #[test]
    fn parses_normal_development_build_and_renamed_dependencies() {
        let manifest = r#"
[dependencies]
clap = { version = "4.6.7", features = ["derive"] }
tree-sitter = "0.27.1"
renamed_json = { package = "serde-json", version = "1.0" }

[dev-dependencies]
pretty_assertions = "1"

[build-dependencies]
cc = "1"
"#;

        let dependencies = parse_cargo_manifest(manifest).unwrap();

        assert_eq!(dependencies.len(), 5);

        let tree_sitter = dependencies
            .iter()
            .find(|dependency| dependency.crate_name == "tree_sitter")
            .unwrap();

        assert_eq!(tree_sitter.package_name, "tree-sitter");
        assert_eq!(tree_sitter.kind, CargoDependencyKind::Normal);
        assert_eq!(tree_sitter.version.as_deref(), Some("0.27.1"));

        let renamed = dependencies
            .iter()
            .find(|dependency| dependency.crate_name == "renamed_json")
            .unwrap();

        assert_eq!(renamed.package_name, "serde-json");

        assert!(dependencies.iter().any(|dependency| {
            dependency.crate_name == "pretty_assertions"
                && dependency.kind == CargoDependencyKind::Development
        }));

        assert!(dependencies.iter().any(|dependency| {
            dependency.crate_name == "cc" && dependency.kind == CargoDependencyKind::Build
        }));
    }

    #[test]
    fn resolves_declared_cargo_dependencies() {
        let index = test_index();
        let symbols = test_symbols();

        let dependencies = vec![CargoDependency {
            crate_name: "tree_sitter".to_string(),
            package_name: "tree-sitter".to_string(),
            kind: CargoDependencyKind::Normal,
            version: Some("0.27.1".to_string()),
        }];

        let result = resolve_against_module(
            &declaration("tree_sitter::Parser"),
            &index.modules[0],
            &index,
            &symbols,
            &dependencies,
        );

        assert_eq!(result.kind, ImportResolutionKind::DeclaredDependency);

        let matched = result.cargo_dependency.unwrap();
        assert_eq!(matched.package_name, "tree-sitter");
        assert_eq!(matched.crate_name, "tree_sitter");
    }
    #[test]
    fn resolves_crate_module_and_symbol_imports() {
        let index = test_index();
        let symbols = test_symbols();
        let module = &index.modules[0];

        let result = resolve_against_module(
            &declaration("crate::analysis::symbols::Symbol"),
            module,
            &index,
            &symbols,
            &[],
        );

        assert_eq!(result.kind, ImportResolutionKind::ResolvedSymbol);
        assert_eq!(
            result.target_file.as_deref(),
            Some("src/analysis/symbols.rs")
        );
        assert_eq!(result.target_symbol.as_deref(), Some("Symbol"));
    }

    #[test]
    fn resolves_module_imports() {
        let index = test_index();
        let symbols = test_symbols();

        let result = resolve_against_module(
            &declaration("crate::analysis::symbols"),
            &index.modules[0],
            &index,
            &symbols,
            &[],
        );

        assert_eq!(result.kind, ImportResolutionKind::ResolvedModule);
        assert_eq!(
            result.target_file.as_deref(),
            Some("src/analysis/symbols.rs")
        );
    }

    #[test]
    fn recognizes_standard_library_imports() {
        let index = test_index();
        let symbols = test_symbols();

        let result = resolve_against_module(
            &declaration("std::collections::HashMap"),
            &index.modules[0],
            &index,
            &symbols,
            &[],
        );

        assert_eq!(result.kind, ImportResolutionKind::StandardLibrary);
    }

    #[test]
    fn classifies_unknown_unprefixed_paths_as_external_candidates() {
        let index = test_index();
        let symbols = test_symbols();

        let result = resolve_against_module(
            &declaration("serde::Serialize"),
            &index.modules[0],
            &index,
            &symbols,
            &[],
        );

        assert_eq!(result.kind, ImportResolutionKind::ExternalCandidate);
    }

    #[test]
    fn reports_missing_explicit_local_imports() {
        let index = test_index();
        let symbols = test_symbols();

        let result = resolve_against_module(
            &declaration("crate::analysis::missing::Thing"),
            &index.modules[0],
            &index,
            &symbols,
            &[],
        );

        assert_eq!(result.kind, ImportResolutionKind::Unresolved);
    }

    #[test]
    fn preserves_alias_and_glob_metadata() {
        let index = test_index();
        let symbols = test_symbols();

        let mut import = declaration("crate::analysis::symbols");
        import.alias = Some("symbol_module".to_string());
        import.is_glob = true;

        let result = resolve_against_module(&import, &index.modules[0], &index, &symbols, &[]);

        assert_eq!(result.alias.as_deref(), Some("symbol_module"));
        assert!(result.is_glob);
        assert_eq!(result.kind, ImportResolutionKind::ResolvedModule);
    }

    #[test]
    fn resolves_self_and_super_paths() {
        let index = test_index();
        let symbols = test_symbols();

        let analysis_module = &index.modules[1];

        let self_result = resolve_against_module(
            &declaration("self::symbols::Symbol"),
            analysis_module,
            &index,
            &symbols,
            &[],
        );

        assert_eq!(self_result.kind, ImportResolutionKind::ResolvedSymbol);

        let nested_module = module(vec!["analysis", "helpers"], "src/analysis/helpers.rs");

        let super_result = resolve_against_module(
            &declaration("super::symbols::Symbol"),
            &nested_module,
            &index,
            &symbols,
            &[],
        );

        assert_eq!(super_result.kind, ImportResolutionKind::ResolvedSymbol);
    }
}
