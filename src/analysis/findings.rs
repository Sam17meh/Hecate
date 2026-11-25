use super::dependencies::DependencyGraph;
use super::import_resolution::{ImportResolutionAnalysis, ImportResolutionKind};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCategory {
    Architecture,
    Reliability,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impact {
    Medium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Small,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationStatus {
    NeedsValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingEvidence {
    pub file: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub id: String,
    pub category: FindingCategory,
    pub title: String,
    pub description: String,
    pub evidence: Vec<FindingEvidence>,
    pub impact: Impact,
    pub confidence: Confidence,
    pub effort: Effort,
    pub validation_status: ValidationStatus,
}

impl Finding {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.id.trim().is_empty() {
            errors.push("Finding ID must not be empty".to_string());
        }

        if self.title.trim().is_empty() {
            errors.push("Finding title must not be empty".to_string());
        }

        if self.description.trim().is_empty() {
            errors.push("Finding description must not be empty".to_string());
        }

        if self.evidence.is_empty() {
            errors.push("Finding must contain at least one evidence entry".to_string());
        }

        for (index, evidence) in self.evidence.iter().enumerate() {
            let prefix = format!("Evidence entry {}", index + 1);

            if evidence.file.trim().is_empty() {
                errors.push(format!("{prefix}: file path must not be empty"));
            }

            if evidence.explanation.trim().is_empty() {
                errors.push(format!("{prefix}: explanation must not be empty"));
            }

            match (evidence.start_line, evidence.end_line) {
                (Some(start), Some(end)) => {
                    if start == 0 {
                        errors.push(format!("{prefix}: start line must be one-based"));
                    }

                    if end < start {
                        errors.push(format!("{prefix}: end line must not be before start line"));
                    }
                }
                (Some(_), None) | (None, Some(_)) => {
                    errors.push(format!(
                        "{prefix}: start and end lines must be provided together"
                    ));
                }
                (None, None) => {}
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn id_component(value: &str) -> String {
    let mut output = String::new();
    let mut previous_separator = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !output.is_empty() {
            output.push('-');
            previous_separator = true;
        }
    }

    output.trim_matches('-').to_string()
}

fn safe_component(value: &str) -> String {
    let component = id_component(value);

    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

/// Converts deterministic repository-analysis results into evidence-backed findings.
///
/// External candidates are deliberately excluded because they may be valid
/// third-party imports that the current resolver cannot classify.
pub fn build_findings(
    import_analysis: &ImportResolutionAnalysis,
    dependency_graph: &DependencyGraph,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for import in &import_analysis.imports {
        let (category, title, confidence, explanation) = match &import.kind {
            ImportResolutionKind::Unresolved => {
                let explanation = import
                    .message
                    .clone()
                    .unwrap_or_else(|| "The import could not be resolved.".to_string());

                let confidence = if explanation.contains("not yet supported") {
                    Confidence::Low
                } else {
                    Confidence::Medium
                };

                (
                    FindingCategory::Dependency,
                    "Unresolved Rust import",
                    confidence,
                    explanation,
                )
            }
            ImportResolutionKind::Ambiguous => (
                FindingCategory::Dependency,
                "Ambiguous Rust import",
                Confidence::Low,
                import
                    .message
                    .clone()
                    .unwrap_or_else(|| "Multiple modules match this import path.".to_string()),
            ),
            _ => continue,
        };

        let context = import.module_context.join("::");
        let id = format!(
            "rust-import-{}-{}-{}-{}-{}",
            safe_component(&import.source),
            safe_component(&import.crate_root),
            safe_component(&context),
            import.line,
            safe_component(&import.path),
        );

        findings.push(Finding {
            id,
            category,
            title: title.to_string(),
            description: format!(
                "The import '{}' in {} could not be confidently resolved by HECATE's current Rust analysis.",
                import.path, import.source
            ),
            evidence: vec![FindingEvidence {
                file: import.source.clone(),
                start_line: if import.line > 0 {
                    Some(import.line)
                } else {
                    None
                },
                end_line: if import.line > 0 {
                    Some(import.line)
                } else {
                    None
                },
                explanation,
            }],
            impact: Impact::Medium,
            confidence,
            effort: Effort::Small,
            validation_status: ValidationStatus::NeedsValidation,
        });
    }

    for unresolved in &dependency_graph.unresolved_modules {
        let context = unresolved.module_context.join("::");

        let id = format!(
            "rust-module-{}-{}-{}",
            safe_component(&unresolved.source),
            safe_component(&context),
            safe_component(&unresolved.module_name),
        );

        findings.push(Finding {
            id,
            category: FindingCategory::Architecture,
            title: "Unresolved Rust module declaration".to_string(),
            description: format!(
                "The module '{}' declared in {} could not be mapped to a source file.",
                unresolved.module_name, unresolved.source
            ),
            evidence: vec![FindingEvidence {
                file: unresolved.source.clone(),
                start_line: None,
                end_line: None,
                explanation: format!(
                    "HECATE could not resolve module '{}' in context '{}'.",
                    unresolved.module_name,
                    if context.is_empty() {
                        "<crate root>"
                    } else {
                        &context
                    }
                ),
            }],
            impact: Impact::Medium,
            confidence: Confidence::Medium,
            effort: Effort::Small,
            validation_status: ValidationStatus::NeedsValidation,
        });
    }

    for error in &dependency_graph.parse_errors {
        let id = format!(
            "rust-parse-{}-{}",
            safe_component(&error.source),
            safe_component(&error.message),
        );

        findings.push(Finding {
            id,
            category: FindingCategory::Reliability,
            title: "Rust source parsing error".to_string(),
            description: format!(
                "HECATE encountered a parsing error while analyzing {}.",
                error.source
            ),
            evidence: vec![FindingEvidence {
                file: error.source.clone(),
                start_line: None,
                end_line: None,
                explanation: error.message.clone(),
            }],
            impact: Impact::Medium,
            confidence: Confidence::Medium,
            effort: Effort::Small,
            validation_status: ValidationStatus::NeedsValidation,
        });
    }

    findings.sort_by(|left, right| left.id.cmp(&right.id));
    findings
}
#[cfg(test)]
mod tests {
    use super::*;

    fn valid_finding() -> Finding {
        Finding {
            id: "rust-test-gap-001".to_string(),
            category: FindingCategory::Reliability,
            title: "Potential missing test coverage".to_string(),
            description: "A public function has no corresponding test.".to_string(),
            evidence: vec![FindingEvidence {
                file: "src/parser.rs".to_string(),
                start_line: Some(10),
                end_line: Some(20),
                explanation: "The function has no nearby test coverage.".to_string(),
            }],
            impact: Impact::Medium,
            confidence: Confidence::Medium,
            effort: Effort::Small,
            validation_status: ValidationStatus::NeedsValidation,
        }
    }

    #[test]
    fn accepts_a_valid_finding() {
        assert!(valid_finding().validate().is_ok());
    }

    #[test]
    fn rejects_empty_required_fields() {
        let mut finding = valid_finding();
        finding.id.clear();
        finding.title.clear();
        finding.description.clear();

        let errors = finding.validate().unwrap_err();

        assert!(errors.iter().any(|error| error.contains("ID")));
        assert!(errors.iter().any(|error| error.contains("title")));
        assert!(errors.iter().any(|error| error.contains("description")));
    }

    #[test]
    fn rejects_findings_without_evidence() {
        let mut finding = valid_finding();
        finding.evidence.clear();

        let errors = finding.validate().unwrap_err();

        assert!(
            errors
                .iter()
                .any(|error| error.contains("at least one evidence"))
        );
    }

    #[test]
    fn rejects_invalid_line_ranges() {
        let mut finding = valid_finding();
        finding.evidence[0].start_line = Some(25);
        finding.evidence[0].end_line = Some(10);

        let errors = finding.validate().unwrap_err();

        assert!(
            errors
                .iter()
                .any(|error| error.contains("end line must not be before"))
        );
    }

    #[test]
    fn requires_both_line_numbers_together() {
        let mut finding = valid_finding();
        finding.evidence[0].start_line = Some(10);
        finding.evidence[0].end_line = None;

        let errors = finding.validate().unwrap_err();

        assert!(
            errors
                .iter()
                .any(|error| error.contains("provided together"))
        );
    }

    #[test]
    fn accepts_file_level_evidence_without_line_numbers() {
        let mut finding = valid_finding();
        finding.evidence[0].start_line = None;
        finding.evidence[0].end_line = None;

        assert!(finding.validate().is_ok());
    }
}

// Tests for converting analysis results into findings.
#[cfg(test)]
mod builder_tests {
    use super::*;
    use crate::analysis::dependencies::{ParseError, UnresolvedModule};
    use crate::analysis::import_resolution::ResolvedImport;

    fn unresolved_import(kind: ImportResolutionKind) -> ResolvedImport {
        ResolvedImport {
            source: "src/main.rs".to_string(),
            crate_root: "src/main.rs".to_string(),
            module_context: Vec::new(),
            path: "crate::missing::Thing".to_string(),
            alias: None,
            is_glob: false,
            line: 12,
            kind,
            target_file: None,
            target_symbol: None,
            message: Some("No matching local module was found".to_string()),
            cargo_dependency: None,
        }
    }

    #[test]
    fn creates_findings_for_unresolved_imports() {
        let mut imports = ImportResolutionAnalysis::default();
        imports
            .imports
            .push(unresolved_import(ImportResolutionKind::Unresolved));

        let dependencies = DependencyGraph {
            dependencies: Vec::new(),
            unresolved_modules: Vec::new(),
            parse_errors: Vec::new(),
        };

        let findings = build_findings(&imports, &dependencies);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::Dependency);
        assert_eq!(findings[0].evidence[0].file, "src/main.rs");
        assert_eq!(findings[0].evidence[0].start_line, Some(12));
        assert_eq!(
            findings[0].validation_status,
            ValidationStatus::NeedsValidation
        );
        assert!(findings[0].validate().is_ok());
    }

    #[test]
    fn ignores_external_candidates() {
        let mut imports = ImportResolutionAnalysis::default();
        imports
            .imports
            .push(unresolved_import(ImportResolutionKind::ExternalCandidate));

        let dependencies = DependencyGraph {
            dependencies: Vec::new(),
            unresolved_modules: Vec::new(),
            parse_errors: Vec::new(),
        };

        assert!(build_findings(&imports, &dependencies).is_empty());
    }

    #[test]
    fn creates_findings_for_unresolved_modules_and_parse_errors() {
        let imports = ImportResolutionAnalysis::default();

        let dependencies = DependencyGraph {
            dependencies: Vec::new(),
            unresolved_modules: vec![UnresolvedModule {
                source: "src/main.rs".to_string(),
                module_name: "missing".to_string(),
                module_context: vec!["analysis".to_string()],
            }],
            parse_errors: vec![ParseError {
                source: "src/broken.rs".to_string(),
                message: "Unexpected syntax".to_string(),
            }],
        };

        let findings = build_findings(&imports, &dependencies);

        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|finding| finding.validate().is_ok()));
        assert!(
            findings
                .iter()
                .any(|finding| { finding.category == FindingCategory::Architecture })
        );
        assert!(
            findings
                .iter()
                .any(|finding| { finding.category == FindingCategory::Reliability })
        );
    }

    #[test]
    fn produces_deterministic_finding_ids() {
        let mut imports = ImportResolutionAnalysis::default();
        imports
            .imports
            .push(unresolved_import(ImportResolutionKind::Unresolved));

        let dependencies = DependencyGraph {
            dependencies: Vec::new(),
            unresolved_modules: Vec::new(),
            parse_errors: Vec::new(),
        };

        let first = build_findings(&imports, &dependencies);
        let second = build_findings(&imports, &dependencies);

        assert_eq!(first[0].id, second[0].id);
    }
}
