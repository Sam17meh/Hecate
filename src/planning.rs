use crate::application::{DiscoveryReport, RepositoryAnalysis};
use crate::discovery::ContributionOpportunity;
use crate::github::GitHubIssue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_DOCUMENTS: usize = 8;
const MAX_DOCUMENT_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedLocation {
    pub file: String,
    pub symbol: Option<String>,
    pub line: Option<usize>,
    pub evidence: Vec<String>,
    pub relationships: Vec<String>,
    pub test_gaps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDocument {
    pub path: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPlan {
    pub repository_url: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_url: String,
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub comments: u64,
    pub expected_outcome: String,
    pub interpretation: String,
    pub locations: Vec<PlannedLocation>,
    pub documents: Vec<PlannedDocument>,
    pub steps: Vec<String>,
    pub test_recommendations: Vec<String>,
    pub validation_commands: Vec<String>,
    pub risks_and_assumptions: Vec<String>,
    pub unresolved_questions: Vec<String>,
}

pub fn plan_discovered_issue(
    report: &DiscoveryReport,
    issue_number: u64,
) -> Result<ContributionPlan, String> {
    let opportunity = report
        .opportunities
        .iter()
        .find(|candidate| candidate.issue.number == issue_number)
        .ok_or_else(|| {
            format!(
                "Issue #{issue_number} was not found among this repository's open issue candidates"
            )
        })?;
    Ok(build_plan(
        &opportunity.issue,
        opportunity,
        &report.analysis,
        &report.github_repository.url,
    ))
}

fn build_plan(
    issue: &GitHubIssue,
    opportunity: &ContributionOpportunity,
    analysis: &RepositoryAnalysis,
    repository_url: &str,
) -> ContributionPlan {
    let locations: Vec<_> = opportunity
        .matches
        .iter()
        .map(|matched| {
            let mut evidence = matched.evidence.clone();
            evidence.push(format!(
                "Discovery relevance is {}/100; this is a heuristic lead, not confirmation that this location contains the issue's cause.",
                matched.relevance
            ));
            let relationships = relationships_for(&matched.file, analysis);
            let test_gaps: Vec<_> = analysis
                .test_gaps
                .gaps
                .iter()
                .filter(|gap| gap.symbol.file == matched.file)
                .map(|gap| format!("{}:{} — {}", gap.symbol.name, gap.symbol.start_line, gap.explanation))
                .collect();
            PlannedLocation {
                file: matched.file.clone(),
                symbol: matched.symbol.clone(),
                line: matched.line,
                evidence,
                relationships,
                test_gaps,
            }
        })
        .collect();

    let documents = relevant_documents(&analysis.repository.root, &issue.title, &issue.body);
    let expected_outcome = if issue.body.trim().is_empty() {
        format!(
            "Address the request stated in issue title: {}. The issue has no body, so expected behavior is underspecified.",
            issue.title
        )
    } else {
        format!(
            "Address the request described by issue #{} ({}); confirm concrete acceptance criteria with the issue discussion or maintainers.",
            issue.number, issue.title
        )
    };
    let interpretation = if issue.body.trim().is_empty() {
        "The title is the only problem statement available. No root cause or specific behavior can be inferred reliably.".to_string()
    } else {
        let body = compact_text(&issue.body, 700);
        format!(
            "The issue reports: {body} This is a reporter description; the repository analysis has not reproduced or verified it."
        )
    };

    let mut steps = vec![
        "Review the full issue and discussion; agree on observable behavior and acceptance criteria before editing.".to_string(),
    ];
    if locations.is_empty() {
        steps.push("Trace the reported behavior manually from repository entry points; discovery did not identify a likely source location.".to_string());
    } else {
        steps.push(format!("Inspect the {} candidate file(s) and any listed symbol/import relationships; verify each heuristic match against surrounding code.", locations.len()));
    }
    if !documents.is_empty() {
        steps.push("Check the relevant repository documentation for project conventions and intended behavior.".to_string());
    }
    steps.push("Make the smallest change that satisfies the agreed behavior, following nearby project patterns.".to_string());
    steps.push("Add or update focused regression tests, then review the diff and report any remaining uncertainty.".to_string());

    let mut test_recommendations: Vec<String> = locations
        .iter()
        .flat_map(|location| location.test_gaps.iter().cloned())
        .collect();
    let relevant_test_files: Vec<_> = analysis
        .repository
        .source_files
        .iter()
        .filter(|path| {
            is_test_path(path)
                && locations
                    .iter()
                    .any(|location| related_test_path(path, &location.file))
        })
        .cloned()
        .collect();
    if relevant_test_files.is_empty() {
        if locations.is_empty() {
            test_recommendations.push("After locating the behavior, add a focused regression test at the project's established test layer; no code location is currently mapped.".to_string());
        } else {
            test_recommendations.push(format!("Add a focused regression test for the reported behavior near the existing tests for {}. Test-gap analysis only detects some direct Rust test calls and is not proof of missing coverage.", locations[0].file));
        }
    } else {
        test_recommendations.push(format!("Review existing related test file(s): {}. Extend them with a regression case for the reported behavior.", relevant_test_files.join(", ")));
    }
    if !analysis.test_gaps.diagnostics.is_empty() {
        test_recommendations.push("Review test-analysis diagnostics before relying on any test-gap signals; parsing or file-read failures may hide coverage.".to_string());
    }
    test_recommendations.dedup();

    let mut risks_and_assumptions = vec![
        "Issue-to-code matches, test-gap signals, and this plan are heuristic; none establish a confirmed defect.".to_string(),
        "Assume the issue remains in scope and is not already resolved; check its latest status and discussion before work.".to_string(),
    ];
    if locations.is_empty() {
        risks_and_assumptions.push(
            "No source files or symbols were matched; code-level scope and effort remain unknown."
                .to_string(),
        );
    }
    if documents.is_empty() {
        risks_and_assumptions.push("No relevant documentation excerpt was found among the supported README/CONTRIBUTING/docs locations.".to_string());
    }
    if !issue.labels.is_empty() {
        risks_and_assumptions.push(format!(
            "Issue labels are repository metadata, not verified severity or effort: {}.",
            issue.labels.join(", ")
        ));
    }

    let mut unresolved_questions = Vec::new();
    if issue.body.trim().is_empty() {
        unresolved_questions.push(
            "What exact behavior and acceptance criteria does the reporter or maintainer expect?"
                .to_string(),
        );
    } else if !issue.body.to_lowercase().contains("expected")
        && !issue.body.to_lowercase().contains("acceptance")
    {
        unresolved_questions
            .push("What observable result would confirm this issue is fixed?".to_string());
    }
    if locations.is_empty() {
        unresolved_questions.push(
            "Which user-visible path or subsystem reproduces the reported behavior?".to_string(),
        );
    }
    if issue.comments > 0 {
        unresolved_questions.push(format!(
            "Do later comments on {} refine or supersede the original request?",
            issue.url
        ));
    }

    ContributionPlan {
        repository_url: repository_url.to_string(),
        issue_number: issue.number,
        issue_title: issue.title.clone(),
        issue_url: issue.url.clone(),
        labels: issue.labels.clone(),
        created_at: issue.created_at.clone(),
        updated_at: issue.updated_at.clone(),
        comments: issue.comments,
        expected_outcome,
        interpretation,
        locations,
        documents,
        steps,
        test_recommendations,
        validation_commands: validation_commands(analysis),
        risks_and_assumptions,
        unresolved_questions,
    }
}

fn relationships_for(file: &str, analysis: &RepositoryAnalysis) -> Vec<String> {
    let mut relationships = BTreeSet::new();
    for module in analysis
        .module_index
        .modules
        .iter()
        .filter(|module| module.file == file)
    {
        let path = if module.module_path.is_empty() {
            "crate root".to_string()
        } else {
            module.module_path.join("::")
        };
        relationships.insert(format!(
            "Indexed as Rust module `{path}` in crate `{}`.",
            module.crate_root
        ));
    }
    for import in analysis
        .imports
        .imports
        .iter()
        .filter(|import| import.source == file)
    {
        if let Some(target) = &import.target_file {
            relationships.insert(format!(
                "Imports `{}` at line {} from `{target}`.",
                import.path, import.line
            ));
        }
    }
    for import in analysis
        .imports
        .imports
        .iter()
        .filter(|import| import.target_file.as_deref() == Some(file))
    {
        relationships.insert(format!(
            "Imported by `{}` at line {} via `{}`.",
            import.source, import.line, import.path
        ));
    }
    for node in analysis
        .architecture
        .files
        .iter()
        .filter(|node| node.path == file)
    {
        for dependency in &node.dependencies {
            relationships.insert(format!("Architecture map lists dependency `{dependency}`."));
        }
    }
    relationships.into_iter().collect()
}

fn validation_commands(analysis: &RepositoryAnalysis) -> Vec<String> {
    let root = &analysis.repository.root;
    if root.join("Cargo.toml").is_file() {
        vec![
            "cargo fmt --check".into(),
            "cargo check".into(),
            "cargo test".into(),
        ]
    } else if root.join("package.json").is_file() {
        vec!["Use the repository's documented lint, type-check, and test commands; package scripts were not executed during planning.".into()]
    } else {
        vec!["Use the repository's documented build and test commands; no supported manifest identified a standard command.".into()]
    }
}

fn relevant_documents(root: &Path, title: &str, body: &str) -> Vec<PlannedDocument> {
    let mut candidates = vec![
        PathBuf::from("README.md"),
        PathBuf::from("CONTRIBUTING.md"),
        PathBuf::from("CONTRIBUTING"),
        PathBuf::from("docs/README.md"),
        PathBuf::from("Docs/README.md"),
    ];
    for directory in ["docs", "Docs"] {
        collect_markdown(root, &PathBuf::from(directory), 0, &mut candidates);
    }
    candidates.sort();
    candidates.dedup();
    let issue_terms = terms(&format!("{title} {body}"));
    let mut documents: Vec<_> = candidates
        .into_iter()
        .filter_map(|relative| {
            let path = root.join(&relative);
            let metadata = fs::metadata(&path).ok()?;
            if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES {
                return None;
            }
            let content = fs::read_to_string(path).ok()?;
            let content_terms = terms(&content);
            let overlap = issue_terms.intersection(&content_terms).count();
            let excerpt = if overlap == 0 {
                if relative == Path::new("README.md") || relative == Path::new("CONTRIBUTING.md") {
                    compact_text(&content, 240)
                } else {
                    return None;
                }
            } else {
                best_excerpt(&content, &issue_terms)
            };
            Some((
                overlap,
                PlannedDocument {
                    path: relative.to_string_lossy().replace('\\', "/"),
                    excerpt,
                },
            ))
        })
        .collect();
    documents.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.path.cmp(&right.1.path)));
    documents
        .into_iter()
        .take(MAX_DOCUMENTS)
        .map(|(_, document)| document)
        .collect()
}

fn collect_markdown(root: &Path, directory: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
    if depth > 2 || paths.len() >= MAX_DOCUMENTS * 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(root.join(directory)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = directory.join(entry.file_name());
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_markdown(root, &path, depth + 1, paths);
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            paths.push(path);
        }
        if paths.len() >= MAX_DOCUMENTS * 3 {
            break;
        }
    }
}

fn best_excerpt(content: &str, issue_terms: &BTreeSet<String>) -> String {
    let best = content
        .lines()
        .max_by_key(|line| terms(line).intersection(issue_terms).count())
        .unwrap_or(content);
    compact_text(best, 320)
}

fn terms(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 4)
        .map(str::to_lowercase)
        .collect()
}

fn compact_text(text: &str, limit: usize) -> String {
    let compact = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let compact = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = compact.chars().take(limit).collect::<String>();
    if compact.chars().count() > limit {
        format!("{preview}…")
    } else {
        preview
    }
}

fn is_test_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.starts_with("tests/")
        || normalized.contains("/tests/")
        || normalized.ends_with("_test.rs")
}

fn related_test_path(test: &str, source: &str) -> bool {
    let source_stem = Path::new(source)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let test_stem = Path::new(test)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    !source_stem.is_empty() && (test_stem == source_stem || test_stem.contains(source_stem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::architecture::{ArchitectureMap, ArchitectureSummary};
    use crate::analysis::dependencies::DependencyGraph;
    use crate::analysis::import_resolution::ImportResolutionAnalysis;
    use crate::analysis::module_index::ModuleIndex;
    use crate::analysis::symbols::{Symbol, SymbolAnalysis};
    use crate::analysis::test_gaps::{TestGap, TestGapAnalysis};
    use crate::application::{DiscoveryReport, RepositoryAnalysis};
    use crate::discovery::{CodeMatch, ContributionOpportunity, OpportunityRanking};
    use crate::github::{GitHubIssue, GitHubRepository};
    use crate::inspection::RepositoryInfo;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture(empty_body: bool, matches: Vec<CodeMatch>) -> (PathBuf, DiscoveryReport) {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let fixture_id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "hecate-plan-test-{}-{fixture_id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("README.md"),
            "Cache refresh behavior and repository usage.",
        )
        .unwrap();
        fs::write(
            root.join("docs/cache.md"),
            "Cache refresh behavior should expire stale entries.",
        )
        .unwrap();
        let issue = GitHubIssue {
            number: 42,
            title: "refresh_cache expiration".into(),
            body: if empty_body {
                String::new()
            } else {
                "Cache refresh should expire stale entries.".into()
            },
            labels: vec!["bug".into()],
            url: "https://github.com/acme/project/issues/42".into(),
            comments: 2,
            created_at: "today".into(),
            updated_at: "today".into(),
        };
        let repository = GitHubRepository {
            owner: "acme".into(),
            name: "project".into(),
            default_branch: "main".into(),
            commit: "abc".into(),
            url: "https://github.com/acme/project".into(),
        };
        let ranking = OpportunityRanking {
            relevance: 90,
            impact: 72,
            confidence: 85,
            effort: 25,
            risk: 35,
            score: 80,
            rationale: vec![],
        };
        let analysis = RepositoryAnalysis {
            repository: RepositoryInfo {
                root: root.clone(),
                is_git_repository: true,
                entries: vec![],
                project_markers: vec!["Cargo.toml".into()],
                source_files: vec!["src/cache.rs".into(), "tests/cache_test.rs".into()],
                language_counts: vec![],
            },
            module_index: ModuleIndex::default(),
            dependency_graph: DependencyGraph {
                dependencies: vec![],
                unresolved_modules: vec![],
                parse_errors: vec![],
            },
            symbols: SymbolAnalysis {
                symbols: vec![],
                diagnostics: vec![],
            },
            test_gaps: TestGapAnalysis::default(),
            imports: ImportResolutionAnalysis::default(),
            architecture: ArchitectureMap {
                files: vec![],
                summary: ArchitectureSummary::default(),
                diagnostics: vec![],
            },
            findings: vec![],
        };
        let opportunity = ContributionOpportunity {
            issue: issue.clone(),
            matches,
            ranking,
        };
        let report = DiscoveryReport {
            github_repository: repository,
            analysis,
            issues: vec![issue],
            opportunities: vec![opportunity],
        };
        (root, report)
    }

    #[test]
    fn plans_selected_issue_with_documentation_tests_and_uncertainty() {
        let (root, mut report) = fixture(
            false,
            vec![CodeMatch {
                file: "src/cache.rs".into(),
                symbol: Some("refresh_cache".into()),
                line: Some(12),
                relevance: 90,
                evidence: vec!["Issue title names the symbol.".into()],
            }],
        );
        report
            .analysis
            .module_index
            .modules
            .push(crate::analysis::module_index::ModuleRecord {
                crate_root: "src/lib.rs".into(),
                module_path: vec!["cache".into()],
                file: "src/cache.rs".into(),
            });
        report.analysis.test_gaps.gaps.push(TestGap {
            symbol: Symbol {
                name: "refresh_cache".into(),
                kind: "function".into(),
                file: "src/cache.rs".into(),
                start_line: 12,
                end_line: 18,
                scope: vec![],
                visibility: Some("pub".into()),
                is_test: false,
            },
            explanation: "No direct test call was detected.".into(),
        });
        let plan = plan_discovered_issue(&report, 42).unwrap();
        assert_eq!(plan.issue_number, 42);
        assert_eq!(plan.locations[0].symbol.as_deref(), Some("refresh_cache"));
        assert!(
            plan.documents
                .iter()
                .any(|document| document.path == "docs/cache.md")
        );
        assert!(
            plan.test_recommendations
                .iter()
                .any(|test| test.contains("tests/cache_test.rs"))
        );
        assert!(
            plan.locations[0]
                .relationships
                .iter()
                .any(|relationship| relationship.contains("module `cache`"))
        );
        assert!(
            plan.locations[0]
                .test_gaps
                .iter()
                .any(|signal| signal.contains("direct test call"))
        );
        assert!(
            plan.risks_and_assumptions
                .iter()
                .any(|risk| risk.contains("heuristic"))
        );
        assert!(
            plan.unresolved_questions
                .iter()
                .any(|question| question.contains("observable result"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reports_missing_issue_selection() {
        let (root, report) = fixture(false, vec![]);
        let error = plan_discovered_issue(&report, 404).unwrap_err();
        assert!(error.contains("Issue #404"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn produces_actionable_plan_when_evidence_is_insufficient() {
        let (root, report) = fixture(true, vec![]);
        let plan = plan_discovered_issue(&report, 42).unwrap();
        assert!(plan.locations.is_empty());
        assert!(plan.expected_outcome.contains("underspecified"));
        assert!(
            plan.unresolved_questions
                .iter()
                .any(|question| question.contains("Which user-visible path"))
        );
        assert!(plan.steps.iter().any(|step| step.contains("manually")));
        let _ = fs::remove_dir_all(root);
    }
}
