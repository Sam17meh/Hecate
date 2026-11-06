use std::fs;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub scope: Vec<String>,
    pub is_test: bool,
}

#[derive(Debug)]
pub struct SymbolDiagnostic {
    pub file: String,
    pub message: String,
}

#[derive(Debug)]
pub struct SymbolAnalysis {
    pub symbols: Vec<Symbol>,
    pub diagnostics: Vec<SymbolDiagnostic>,
}

pub fn analyze_rust_file(source_code: &str, file_path: &str) -> Result<Vec<Symbol>, String> {
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

    let mut symbols = Vec::new();
    let mut scope = Vec::new();

    let normalized_path = file_path.replace('\\', "/");
    let path_is_test = normalized_path.starts_with("tests/") || normalized_path.contains("/tests/");

    collect_symbols(
        tree.root_node(),
        source_code,
        file_path,
        &mut symbols,
        &mut scope,
        path_is_test,
    )?;

    Ok(symbols)
}

pub fn analyze_repository(
    source_files: &[String],
    repository_root: &Path,
) -> Result<SymbolAnalysis, String> {
    let mut all_symbols = Vec::new();
    let mut diagnostics = Vec::new();

    for file_path in source_files {
        if !file_path.ends_with(".rs") {
            continue;
        }

        let absolute_path = repository_root.join(file_path);

        let source_code = match fs::read_to_string(&absolute_path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(SymbolDiagnostic {
                    file: file_path.clone(),
                    message: format!("Failed to read file: {error}"),
                });
                continue;
            }
        };

        match analyze_rust_file(&source_code, file_path) {
            Ok(symbols) => all_symbols.extend(symbols),
            Err(error) => diagnostics.push(SymbolDiagnostic {
                file: file_path.clone(),
                message: error,
            }),
        }
    }

    Ok(SymbolAnalysis {
        symbols: all_symbols,
        diagnostics,
    })
}

fn node_has_test_attribute(node: Node, source_code: &str) -> bool {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            if let Ok(attribute) = child.utf8_text(source_code.as_bytes()) {
                if attribute.contains("cfg(test)") || attribute.contains("#[test]") {
                    return true;
                }
            }
        }
    }

    false
}

fn collect_symbols(
    parent_node: Node,
    source_code: &str,
    file_path: &str,
    symbols: &mut Vec<Symbol>,
    scope: &mut Vec<String>,
    inside_test: bool,
) -> Result<(), String> {
    let mut cursor = parent_node.walk();

    for node in parent_node.named_children(&mut cursor) {
        let kind = match node.kind() {
            "function_item" | "function_signature_item" => Some("function"),
            "struct_item" => Some("struct"),
            "enum_item" => Some("enum"),
            "trait_item" => Some("trait"),
            "impl_item" => Some("implementation"),
            "mod_item" => Some("module"),
            "type_item" => Some("type"),
            "const_item" => Some("constant"),
            "static_item" => Some("static"),
            _ => None,
        };

        let name = if node.kind() == "impl_item" {
            implementation_name(node, source_code)?
        } else if let Some(name_node) = node.child_by_field_name("name") {
            name_node
                .utf8_text(source_code.as_bytes())
                .map_err(|error| error.to_string())?
                .to_string()
        } else {
            String::new()
        };

        let is_test = inside_test
            || node_has_test_attribute(node, source_code)
            || (node.kind() == "mod_item" && name == "tests");

        if let Some(kind) = kind {
            if !name.is_empty() {
                symbols.push(Symbol {
                    name: name.clone(),
                    kind: kind.to_string(),
                    file: file_path.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    scope: scope.clone(),
                    is_test,
                });
            }
        }

        let introduces_scope = matches!(node.kind(), "mod_item" | "trait_item" | "impl_item");

        if introduces_scope && !name.is_empty() {
            scope.push(name);
        }

        collect_symbols(node, source_code, file_path, symbols, scope, is_test)?;

        if introduces_scope && !scope.is_empty() {
            scope.pop();
        }
    }

    Ok(())
}

fn implementation_name(node: Node, source_code: &str) -> Result<String, String> {
    let trait_node = node.child_by_field_name("trait");
    let type_node = node.child_by_field_name("type");

    let trait_name = trait_node
        .map(|child| child.utf8_text(source_code.as_bytes()))
        .transpose()
        .map_err(|error| error.to_string())?;

    let type_name = type_node
        .map(|child| child.utf8_text(source_code.as_bytes()))
        .transpose()
        .map_err(|error| error.to_string())?;

    Ok(match (trait_name, type_name) {
        (Some(t), Some(ty)) => format!("impl {} for {}", t, ty),
        (None, Some(ty)) => format!("impl {}", ty),
        _ => "impl".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_functions_and_types() {
        let source = r#"
pub struct User;
pub enum Role { Admin, Member }
pub trait Greet {}
fn main() {}
"#;
        let symbols = analyze_rust_file(source, "src/main.rs").unwrap();

        for (name, kind) in [
            ("User", "struct"),
            ("Role", "enum"),
            ("Greet", "trait"),
            ("main", "function"),
        ] {
            assert!(symbols.iter().any(|s| s.name == name && s.kind == kind));
        }
    }

    #[test]
    fn extracts_methods_inside_implementations() {
        let source = r#"
struct User;

impl User {
    fn greet(&self) {}
}
"#;
        let symbols = analyze_rust_file(source, "src/user.rs").unwrap();

        assert!(
            symbols
                .iter()
                .any(|s| s.kind == "implementation" && s.name == "impl User")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == "function" && s.name == "greet")
        );
    }

    #[test]
    fn records_correct_line_numbers() {
        let source = "struct User;\n\nfn greet() {}\n";
        let symbols = analyze_rust_file(source, "src/user.rs").unwrap();
        let greet = symbols.iter().find(|s| s.name == "greet").unwrap();

        assert_eq!(greet.file, "src/user.rs");
        assert_eq!(greet.start_line, 3);
        assert_eq!(greet.end_line, 3);
    }

    #[test]
    fn reports_syntax_errors() {
        assert!(analyze_rust_file("fn broken() { let value = ; }", "src/broken.rs").is_err());
    }

    #[test]
    fn ignores_comments_and_strings() {
        let source = r#"
// fn fake_function() {}
const MESSAGE: &str = "struct Fake {}";
fn real_function() {}
"#;
        let symbols = analyze_rust_file(source, "src/main.rs").unwrap();

        assert!(symbols.iter().any(|s| s.name == "real_function"));
        assert!(!symbols.iter().any(|s| s.name == "fake_function"));
        assert!(!symbols.iter().any(|s| s.name == "Fake"));
    }

    #[test]
    fn tracks_module_scope() {
        let source = r#"
mod users {
    struct User;

    impl User {
        fn greet(&self) {}
    }
}
"#;
        let symbols = analyze_rust_file(source, "src/main.rs").unwrap();
        let greet = symbols.iter().find(|s| s.name == "greet").unwrap();

        assert_eq!(greet.scope, vec!["users", "impl User"]);
    }

    #[test]
    fn identifies_test_code() {
        let source = r#"
#[cfg(test)]
mod tests {
    #[test]
    fn example_test() {}
}
"#;
        let symbols = analyze_rust_file(source, "src/main.rs").unwrap();

        assert!(symbols.iter().any(|s| s.name == "tests" && s.is_test));
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "example_test" && s.is_test)
        );
    }

    #[test]
    fn identifies_test_directory() {
        let source = "fn integration_test() {}";
        let symbols = analyze_rust_file(source, "tests/integration.rs").unwrap();

        assert!(
            symbols
                .iter()
                .any(|s| s.name == "integration_test" && s.is_test)
        );
    }

    #[test]
    fn continues_after_file_parse_error() {
        let root = std::env::temp_dir().join(format!("hecate-symbol-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();

        fs::write(root.join("broken.rs"), "fn broken() { let x = ; }").unwrap();
        fs::write(root.join("good.rs"), "fn working() {}").unwrap();

        let files = vec!["broken.rs".to_string(), "good.rs".to_string()];
        let result = analyze_repository(&files, &root).unwrap();

        assert!(result.symbols.iter().any(|s| s.name == "working"));
        assert!(!result.symbols.iter().any(|s| s.name == "broken"));
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].file, "broken.rs");

        fs::remove_dir_all(root).unwrap();
    }
}
