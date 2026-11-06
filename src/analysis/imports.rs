use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDeclaration {
    pub source: String,
    pub path: String,
    pub alias: Option<String>,
    pub is_glob: bool,
    pub line: usize,
}

pub fn extract_imports(
    source_code: &str,
    file_path: &str,
) -> Result<Vec<ImportDeclaration>, String> {
    let mut parser = Parser::new();

    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| format!("Could not load Rust grammar: {error}"))?;

    let tree = parser
        .parse(source_code, None)
        .ok_or_else(|| format!("Could not parse {}", file_path))?;

    if tree.root_node().has_error() {
        return Err(format!("Syntax errors found in {}", file_path));
    }

    let mut imports = Vec::new();

    collect_imports(tree.root_node(), source_code, file_path, &mut imports);

    Ok(imports)
}

fn collect_imports(
    node: Node,
    source_code: &str,
    file_path: &str,
    imports: &mut Vec<ImportDeclaration>,
) {
    if node.kind() == "use_declaration" {
        if let Some(argument) = node.child_by_field_name("argument") {
            collect_use_tree(
                argument,
                "",
                source_code,
                file_path,
                node.start_position().row + 1,
                imports,
            );
        }

        return;
    }

    for index in 0..node.named_child_count() {
        if let Some(child) =
            node.named_child(u32::try_from(index).expect("Tree-sitter child index exceeds u32"))
        {
            collect_imports(child, source_code, file_path, imports);
        }
    }
}

fn collect_use_tree(
    node: Node,
    prefix: &str,
    source_code: &str,
    file_path: &str,
    line: usize,
    imports: &mut Vec<ImportDeclaration>,
) {
    match node.kind() {
        "scoped_use_list" => {
            if let Some(path_node) = node.child_by_field_name("path") {
                let path = node_text(path_node, source_code);
                let combined = combine_path(prefix, &path);

                if let Some(list) = node.child_by_field_name("list") {
                    collect_use_tree(list, &combined, source_code, file_path, line, imports);
                }
            }
        }

        "use_list" => {
            for index in 0..node.named_child_count() {
                if let Some(child) = node
                    .named_child(u32::try_from(index).expect("Tree-sitter child index exceeds u32"))
                {
                    collect_use_tree(child, prefix, source_code, file_path, line, imports);
                }
            }
        }

        "use_as_clause" => {
            if let Some(path_node) = node.child_by_field_name("path") {
                let path = node_text(path_node, source_code);

                let alias = node
                    .child_by_field_name("alias")
                    .map(|alias_node| node_text(alias_node, source_code));

                imports.push(ImportDeclaration {
                    source: file_path.to_string(),
                    path: combine_path(prefix, &path),
                    alias,
                    is_glob: false,
                    line,
                });
            }
        }

        "use_wildcard" => {
            // Tree-sitter represents the wildcard path as its
            // first named child, not as a field named "path".
            if let Some(path_node) = node.named_child(0) {
                let path = node_text(path_node, source_code);

                imports.push(ImportDeclaration {
                    source: file_path.to_string(),
                    path: combine_path(prefix, &path),
                    alias: None,
                    is_glob: true,
                    line,
                });
            }
        }

        _ => {
            let path = node_text(node, source_code);

            imports.push(ImportDeclaration {
                source: file_path.to_string(),
                path: combine_path(prefix, &path),
                alias: None,
                is_glob: false,
                line,
            });
        }
    }
}

fn node_text(node: Node, source_code: &str) -> String {
    source_code[node.byte_range()].to_string()
}

fn combine_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_string()
    } else if path.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::extract_imports;

    #[test]
    fn extracts_simple_import() {
        let source = "use crate::analysis::symbols::Symbol;";

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "crate::analysis::symbols::Symbol");
        assert_eq!(imports[0].alias, None);
        assert!(!imports[0].is_glob);
    }

    #[test]
    fn extracts_aliased_import() {
        let source = "use crate::analysis::symbols::Symbol as RepoSymbol;";

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "crate::analysis::symbols::Symbol");
        assert_eq!(imports[0].alias.as_deref(), Some("RepoSymbol"));
    }

    #[test]
    fn extracts_wildcard_import() {
        let source = "use crate::analysis::symbols::*;";

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "crate::analysis::symbols");
        assert!(imports[0].is_glob);
    }

    #[test]
    fn extracts_grouped_imports() {
        let source = r#"
            use crate::analysis::{
                symbols::Symbol,
                dependencies::DependencyGraph,
            };
        "#;

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 2);

        assert!(
            imports
                .iter()
                .any(|item| { item.path == "crate::analysis::symbols::Symbol" })
        );

        assert!(
            imports
                .iter()
                .any(|item| { item.path == "crate::analysis::dependencies::DependencyGraph" })
        );
    }

    #[test]
    fn extracts_alias_inside_group() {
        let source = r#"
            use crate::analysis::symbols::{
                Symbol as RepoSymbol,
            };
        "#;

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "crate::analysis::symbols::Symbol");
        assert_eq!(imports[0].alias.as_deref(), Some("RepoSymbol"));
    }

    #[test]
    fn ignores_comments_and_strings() {
        let source = r#"
            // use fake::module::Something;
            let example = "use fake::module::Something;";
            use crate::real::Module;
        "#;

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "crate::real::Module");
    }

    #[test]
    fn records_import_line_number() {
        let source = "\n\nuse crate::analysis::symbols::Symbol;";

        let imports = extract_imports(source, "src/main.rs").unwrap();

        assert_eq!(imports[0].line, 3);
    }

    #[test]
    fn reports_syntax_errors() {
        let source = "use crate::analysis::{;";

        let result = extract_imports(source, "src/main.rs");

        assert!(result.is_err());
    }
}
