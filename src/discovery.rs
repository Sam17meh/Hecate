use crate::analysis::architecture::ArchitectureMap;
use crate::analysis::import_resolution::ImportResolutionAnalysis;
use crate::analysis::module_index::ModuleIndex;
use crate::analysis::symbols::{Symbol, SymbolAnalysis};
use crate::github::GitHubIssue;
use crate::inspection::RepositoryInfo;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMatch {
    pub file: String,
    pub symbol: Option<String>,
    pub line: Option<usize>,
    pub relevance: u8,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunityRanking {
    pub relevance: u8,
    pub impact: u8,
    pub confidence: u8,
    pub effort: u8,
    pub risk: u8,
    pub score: u8,
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionOpportunity {
    pub issue: GitHubIssue,
    pub matches: Vec<CodeMatch>,
    pub ranking: OpportunityRanking,
}

pub fn discover_opportunities(
    issues: &[GitHubIssue],
    repository: &RepositoryInfo,
    modules: &ModuleIndex,
    symbols: &SymbolAnalysis,
    imports: &ImportResolutionAnalysis,
    architecture: &ArchitectureMap,
) -> Vec<ContributionOpportunity> {
    let mut opportunities: Vec<_> = issues
        .iter()
        .map(|issue| {
            let matches = map_issue(
                issue,
                repository,
                modules,
                &symbols.symbols,
                imports,
                architecture,
            );
            let ranking = rank_opportunity(issue, &matches);
            ContributionOpportunity {
                issue: issue.clone(),
                matches,
                ranking,
            }
        })
        .collect();
    opportunities.sort_by(|left, right| {
        right
            .ranking
            .score
            .cmp(&left.ranking.score)
            .then_with(|| right.ranking.relevance.cmp(&left.ranking.relevance))
            .then_with(|| left.issue.number.cmp(&right.issue.number))
    });
    opportunities
}

fn map_issue(
    issue: &GitHubIssue,
    repository: &RepositoryInfo,
    modules: &ModuleIndex,
    symbols: &[Symbol],
    imports: &ImportResolutionAnalysis,
    architecture: &ArchitectureMap,
) -> Vec<CodeMatch> {
    let text = format!("{}\n{}", issue.title, issue.body).to_lowercase();
    let issue_tokens = meaningful_tokens(&text);
    let mut issue_identifiers = identifier_terms(&issue.title.to_lowercase());
    for code_span in issue.body.split('`').skip(1).step_by(2) {
        issue_identifiers.extend(identifier_terms(&code_span.to_lowercase()));
    }
    let mut by_file = BTreeMap::<String, CodeMatch>::new();
    let mut path_evidence = BTreeMap::<String, Vec<String>>::new();
    let file_set: BTreeSet<_> = repository.source_files.iter().collect();
    let mut symbol_name_counts = BTreeMap::<String, usize>::new();
    for symbol in symbols {
        *symbol_name_counts
            .entry(symbol.name.to_lowercase())
            .or_default() += 1;
    }

    for file in &repository.source_files {
        let normalized = file.replace('\\', "/");
        let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
        let file_mentions = text.contains(&normalized.to_lowercase());
        let basename_tokens = meaningful_tokens(basename);
        let file_overlap = token_overlap(&issue_tokens, &basename_tokens);
        let mut evidence = Vec::new();
        let mut relevance = 0u8;
        if file_mentions {
            relevance = 70;
            evidence.push(format!(
                "Issue explicitly mentions the source path `{normalized}`."
            ));
        } else if file_overlap > 0 {
            relevance = (file_overlap * 18).min(36) as u8;
            evidence.push(format!(
                "Issue language overlaps with the file name `{basename}`."
            ));
        }
        if relevance > 0 {
            path_evidence.insert(file.clone(), evidence.clone());
            by_file.insert(
                file.clone(),
                CodeMatch {
                    file: file.clone(),
                    symbol: None,
                    line: None,
                    relevance,
                    evidence,
                },
            );
        }
    }

    for symbol in symbols
        .iter()
        .filter(|symbol| file_set.contains(&symbol.file))
    {
        let normalized = symbol.name.to_lowercase();
        let tokens = meaningful_tokens(&symbol.name);
        let exact = issue_identifiers.contains(&normalized)
            && !is_generic_identifier(&normalized)
            && !tokens.is_empty()
            && symbol_name_counts
                .get(&normalized)
                .copied()
                .unwrap_or_default()
                <= 3;
        let overlap = token_overlap(&issue_tokens, &tokens);
        if !exact && overlap < 2 {
            continue;
        }
        let relevance = if exact {
            90
        } else {
            (48 + overlap * 8).min(72) as u8
        };
        let mut evidence = path_evidence.get(&symbol.file).cloned().unwrap_or_default();
        evidence.push(if exact {
            format!(
                "Issue text explicitly names the Rust {} `{}`.",
                symbol.kind, symbol.name
            )
        } else {
            format!(
                "Issue language overlaps with the Rust {} `{}`.",
                symbol.kind, symbol.name
            )
        });
        if by_file
            .get(&symbol.file)
            .is_some_and(|existing| existing.relevance > relevance)
        {
            continue;
        }
        by_file.insert(
            symbol.file.clone(),
            CodeMatch {
                file: symbol.file.clone(),
                symbol: Some(symbol.name.clone()),
                line: Some(symbol.start_line),
                relevance,
                evidence,
            },
        );
    }

    // Connected files are useful leads, but weaker than direct textual evidence.
    let direct: Vec<_> = by_file
        .values()
        .filter(|m| m.relevance >= 48)
        .map(|m| m.file.clone())
        .collect();
    let mut neighbors = BTreeSet::new();
    for module in &modules.modules {
        if direct.contains(&module.file) {
            for edge in &imports.imports {
                if edge.source == module.file {
                    if let Some(target) = &edge.target_file {
                        neighbors.insert(target.clone());
                    }
                }
            }
            if let Some(node) = architecture
                .files
                .iter()
                .find(|node| node.path == module.file)
            {
                for dependency in &node.dependencies {
                    neighbors.insert(dependency.clone());
                }
            }
        }
    }
    for neighbor in neighbors {
        if by_file.contains_key(&neighbor) {
            continue;
        }
        by_file.insert(neighbor.clone(), CodeMatch {
            file: neighbor,
            symbol: None,
            line: None,
            relevance: 24,
            evidence: vec!["This file is connected to a directly matched module in the repository architecture.".to_string()],
        });
    }

    let mut matches: Vec<_> = by_file.into_values().collect();
    matches.sort_by(|left, right| {
        right
            .relevance
            .cmp(&left.relevance)
            .then(left.file.cmp(&right.file))
    });
    matches.truncate(5);
    matches
}

fn meaningful_tokens(value: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
                tokens.insert(std::mem::take(&mut current));
            }
            current.push(character.to_ascii_lowercase());
            previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            if current.len() >= 3 {
                tokens.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            previous_lower = false;
        }
    }
    if current.len() >= 3 {
        tokens.insert(current);
    }
    tokens.retain(|token| {
        token.len() >= 3
            && !matches!(
                token.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "this"
                    | "that"
                    | "from"
                    | "into"
                    | "when"
                    | "then"
                    | "have"
                    | "has"
                    | "are"
                    | "was"
                    | "not"
                    | "but"
                    | "use"
                    | "new"
                    | "get"
                    | "set"
                    | "help"
                    | "name"
                    | "type"
                    | "value"
                    | "kind"
                    | "self"
                    | "some"
                    | "none"
                    | "true"
                    | "false"
                    | "command"
                    | "commands"
                    | "error"
                    | "result"
                    | "option"
                    | "options"
                    | "argument"
                    | "arguments"
                    | "usage"
                    | "output"
                    | "message"
                    | "path"
                    | "file"
                    | "files"
            )
    });
    tokens
}

fn token_overlap(issue_tokens: &BTreeSet<String>, candidates: &BTreeSet<String>) -> usize {
    candidates.intersection(issue_tokens).count()
}

fn identifier_terms(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_generic_identifier(identifier: &str) -> bool {
    matches!(
        identifier,
        "get"
            | "set"
            | "new"
            | "help"
            | "name"
            | "type"
            | "value"
            | "kind"
            | "error"
            | "result"
            | "command"
            | "usage"
            | "option"
            | "argument"
            | "path"
            | "file"
            | "open"
            | "close"
            | "read"
            | "write"
            | "run"
            | "parse"
            | "build"
            | "default"
            | "clone"
            | "as_ref"
            | "to_string"
    )
}

fn rank_opportunity(issue: &GitHubIssue, matches: &[CodeMatch]) -> OpportunityRanking {
    let relevance = matches.first().map_or(0, |matched| matched.relevance);
    let label_text = issue.labels.join(" ").to_lowercase();
    let impact = if label_text.contains("security")
        || label_text.contains("critical")
        || label_text.contains("severity: high")
    {
        90
    } else if label_text.contains("bug")
        || label_text.contains("crash")
        || label_text.contains("regression")
    {
        72
    } else if label_text.contains("enhancement") || label_text.contains("feature") {
        58
    } else {
        45
    };
    let confidence = match relevance {
        80..=100 => 85,
        55..=79 => 68,
        25..=54 => 48,
        _ => 25,
    };
    let effort = if label_text.contains("good first issue")
        || label_text.contains("beginner")
        || label_text.contains("easy")
    {
        20
    } else if label_text.contains("hard")
        || label_text.contains("complex")
        || label_text.contains("large")
    {
        80
    } else {
        match matches.len() {
            0..=1 => 25,
            2..=3 => 40,
            4..=6 => 60,
            _ => 80,
        }
    };
    let risk = if label_text.contains("security") || label_text.contains("breaking") {
        70
    } else if relevance < 25 {
        55
    } else {
        35
    };
    let score = ((relevance as u16 * 40
        + impact as u16 * 25
        + confidence as u16 * 20
        + (100 - effort) as u16 * 10
        + (100 - risk) as u16 * 5)
        / 100) as u8;
    let rationale = vec![
        format!("Relevance {relevance}/100 from the strongest issue-to-code match."),
        format!("Impact {impact}/100 estimated from issue labels; no label is treated as proof."),
        format!(
            "Confidence {confidence}/100 reflects textual match strength; all mappings are heuristic."
        ),
        format!("Effort {effort}/100 estimated from mapped scope; lower is easier."),
        format!("Risk {risk}/100 estimated from labels and match strength."),
        format!(
            "Combined score {score}/100 weights relevance, impact, confidence, effort, and lower risk."
        ),
    ];
    OpportunityRanking {
        relevance,
        impact,
        confidence,
        effort,
        risk,
        score,
        rationale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::architecture::build_architecture_map;
    use crate::analysis::dependencies::DependencyGraph;
    use crate::analysis::import_resolution::ImportResolutionAnalysis;
    use crate::analysis::module_index::ModuleIndex;
    use crate::analysis::symbols::{Symbol, SymbolAnalysis};
    use crate::github::GitHubIssue;
    use std::path::PathBuf;

    fn issue(number: u64, title: &str, body: &str, labels: &[&str]) -> GitHubIssue {
        GitHubIssue {
            number,
            title: title.into(),
            body: body.into(),
            labels: labels.iter().map(|v| (*v).into()).collect(),
            url: String::new(),
            comments: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn maps_exact_symbol_and_ranks_direct_match_above_unmapped_issue() {
        let repository = RepositoryInfo {
            root: PathBuf::new(),
            is_git_repository: true,
            entries: vec![],
            project_markers: vec![],
            source_files: vec!["src/cache.rs".into()],
            language_counts: vec![],
        };
        let symbols = SymbolAnalysis {
            symbols: vec![Symbol {
                name: "refresh_cache".into(),
                kind: "function".into(),
                file: "src/cache.rs".into(),
                start_line: 8,
                end_line: 12,
                scope: vec![],
                visibility: Some("pub".into()),
                is_test: false,
            }],
            diagnostics: vec![],
        };
        let graph = DependencyGraph {
            dependencies: vec![],
            unresolved_modules: vec![],
            parse_errors: vec![],
        };
        let architecture = build_architecture_map(&repository, &graph, &symbols);
        let issues = vec![
            issue(2, "Update docs", "Please improve wording", &[]),
            issue(
                1,
                "refresh_cache can panic",
                "The refresh_cache path fails",
                &["bug"],
            ),
        ];
        let results = discover_opportunities(
            &issues,
            &repository,
            &ModuleIndex::default(),
            &symbols,
            &ImportResolutionAnalysis::default(),
            &architecture,
        );
        assert_eq!(results[0].issue.number, 1);
        assert_eq!(
            results[0].matches[0].symbol.as_deref(),
            Some("refresh_cache")
        );
        assert!(
            results[0]
                .ranking
                .rationale
                .iter()
                .any(|line| line.contains("heuristic"))
        );
    }

    #[test]
    fn avoids_common_identifier_false_matches() {
        let repository = RepositoryInfo {
            root: PathBuf::new(),
            is_git_repository: true,
            entries: vec![],
            project_markers: vec![],
            source_files: vec!["src/service.rs".into()],
            language_counts: vec![],
        };
        let symbols = SymbolAnalysis {
            symbols: vec!["get", "help", "new"]
                .into_iter()
                .map(|name| Symbol {
                    name: name.into(),
                    kind: "function".into(),
                    file: "src/service.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    scope: vec![],
                    visibility: Some("pub".into()),
                    is_test: false,
                })
                .collect(),
            diagnostics: vec![],
        };
        let graph = DependencyGraph {
            dependencies: vec![],
            unresolved_modules: vec![],
            parse_errors: vec![],
        };
        let architecture = build_architecture_map(&repository, &graph, &symbols);
        let results = discover_opportunities(
            &[issue(
                1,
                "Help message does not get shown",
                "Call new to initialize",
                &[],
            )],
            &repository,
            &ModuleIndex::default(),
            &symbols,
            &ImportResolutionAnalysis::default(),
            &architecture,
        );
        assert!(results[0].matches.is_empty());
    }

    #[test]
    fn treats_prose_mentions_as_weaker_than_title_or_code_mentions() {
        let repository = RepositoryInfo {
            root: PathBuf::new(),
            is_git_repository: true,
            entries: vec![],
            project_markers: vec![],
            source_files: vec!["src/cache.rs".into()],
            language_counts: vec![],
        };
        let symbols = SymbolAnalysis {
            symbols: vec![Symbol {
                name: "refresh_cache".into(),
                kind: "function".into(),
                file: "src/cache.rs".into(),
                start_line: 8,
                end_line: 12,
                scope: vec![],
                visibility: Some("pub".into()),
                is_test: false,
            }],
            diagnostics: vec![],
        };
        let graph = DependencyGraph {
            dependencies: vec![],
            unresolved_modules: vec![],
            parse_errors: vec![],
        };
        let architecture = build_architecture_map(&repository, &graph, &symbols);
        let results = discover_opportunities(
            &[issue(
                1,
                "Cache update behavior",
                "The refresh_cache function should handle expiration.",
                &[],
            )],
            &repository,
            &ModuleIndex::default(),
            &symbols,
            &ImportResolutionAnalysis::default(),
            &architecture,
        );
        assert_eq!(results[0].matches[0].relevance, 64);
        assert_eq!(results[0].matches[0].line, Some(8));
    }
}
