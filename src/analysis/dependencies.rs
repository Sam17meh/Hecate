use std::path::{Path, PathBuf};
use tree_sitter::{Parser, Language};

#[derive(Debug)]
pub struct ModuleDeclaration {
    pub source: String,
    pub module_name: String,
}

#[derive(Debug)]
pub struct DependencyGraph {
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug)]
pub struct Dependency {
    pub source: String,
    pub target: String,
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

    let mut dependencies = Vec::new();

    let mut cursor = tree.walk();

    for node in tree.root_node().named_children(&mut cursor) {
        if node.kind() == "mod_item" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let module_name = name_node
                    .utf8_text(source_code.as_bytes())
                    .map_err(|error| error.to_string())?;

               dependencies.push(ModuleDeclaration {
                   source: file_path.to_string(),
                   module_name: module_name.to_string(),
                });
            }
        }
    }

    Ok(dependencies)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_multiple_module_declarations() {
        let source = r#"
            mod inspection;
            mod analysis;
        "#;

        let result = extract_module_declarations(source, "src/main.rs")
            .expect("Parsing should succeed");

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

        let result = extract_module_declarations(source, "src/main.rs")
            .expect("Parsing should succeed");

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

    let result = extract_module_declarations(source, "src/main.rs")
        .expect("Parsing should succeed");

    assert!(result.is_empty());
   } 
}
