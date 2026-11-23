mod analysis;
mod application;
mod discovery;
mod github;
mod inspection;

use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(
    name = "hecate",
    version,
    about = "Repository intelligence for open-source contributors"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect a local directory or public GitHub repository
    Inspect { repository: String },
    /// Find and rank contribution opportunities from a public repository's open issues
    Discover { repository: String },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Inspect { repository } => {
            if Path::new(&repository).is_dir() {
                application::analyze_local_repository(Path::new(&repository)).map(|analysis| {
                    print_analysis(&analysis, None);
                })
            } else {
                application::inspect_github_repository(&repository).map(|(github, analysis)| {
                    print_analysis(&analysis, Some(&github));
                })
            }
        }
        Commands::Discover { repository } => {
            println!(
                "Fetching repository and open issues from GitHub, then analyzing local Rust source..."
            );
            application::discover_github_contributions(&repository)
                .map(|report| print_discovery(&report))
        }
    };
    if let Err(error) = result {
        eprintln!("HECATE: {error}");
        std::process::exit(1);
    }
}

fn print_analysis(
    analysis: &application::RepositoryAnalysis,
    github: Option<&github::GitHubRepository>,
) {
    println!("HECATE — REPOSITORY INSPECTION");
    if let Some(repository) = github {
        println!("Repository: {} ({})", repository.url, repository.commit);
    }
    println!("Root: {}", analysis.repository.root.display());
    println!(
        "Git repository: {}",
        if analysis.repository.is_git_repository {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Languages: {}",
        analysis
            .repository
            .language_counts
            .iter()
            .map(|(language, count)| format!("{language} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Source files: {}", analysis.repository.source_files.len());
    println!(
        "Top-level entries: {}",
        analysis.repository.entries.join(", ")
    );
    println!(
        "Project markers: {}",
        if analysis.repository.project_markers.is_empty() {
            "none".to_string()
        } else {
            analysis.repository.project_markers.join(", ")
        }
    );
    println!(
        "Rust modules indexed: {}",
        analysis.module_index.modules.len()
    );
    for module in &analysis.module_index.modules {
        let logical = if module.module_path.is_empty() {
            "(crate root)".to_string()
        } else {
            module.module_path.join("::")
        };
        println!(
            "  module {logical} [{}] -> {}",
            module.crate_root, module.file
        );
    }
    for unresolved in &analysis.module_index.unresolved_modules {
        println!(
            "  unresolved module {} in {} ({})",
            unresolved.module_name, unresolved.source, unresolved.crate_root
        );
    }
    for diagnostic in &analysis.module_index.diagnostics {
        println!(
            "  module index diagnostic [{}] {}: {}",
            diagnostic.crate_root, diagnostic.file, diagnostic.message
        );
    }
    println!(
        "Module dependencies: {}",
        analysis.dependency_graph.dependencies.len()
    );
    println!(
        "Unresolved module declarations: {}",
        analysis.dependency_graph.unresolved_modules.len()
    );
    for dependency in &analysis.dependency_graph.dependencies {
        println!(
            "  {} -> {} ({}{})",
            dependency.source,
            dependency.target,
            if dependency.module_context.is_empty() {
                String::new()
            } else {
                format!("{}::", dependency.module_context.join("::"))
            },
            dependency.module_name
        );
    }
    for unresolved in &analysis.dependency_graph.unresolved_modules {
        println!(
            "  unresolved declaration {} in {}",
            unresolved.module_name, unresolved.source
        );
    }
    for error in &analysis.dependency_graph.parse_errors {
        println!("  parse diagnostic {}: {}", error.source, error.message);
    }
    println!("Rust symbols: {}", analysis.symbols.symbols.len());
    for symbol in &analysis.symbols.symbols {
        let test = if symbol.is_test { " [test]" } else { "" };
        println!(
            "  {} {}:{}:{}-{}{}",
            symbol.kind, symbol.name, symbol.file, symbol.start_line, symbol.end_line, test
        );
    }
    for diagnostic in &analysis.symbols.diagnostics {
        println!(
            "  symbol diagnostic {}: {}",
            diagnostic.file, diagnostic.message
        );
    }
    println!("Imports analyzed: {}", analysis.imports.imports.len());
    for import in &analysis.imports.imports {
        println!(
            "  [{:?}] {}:{} {}{}{}",
            import.kind,
            import.source,
            import.line,
            import.path,
            import
                .alias
                .as_ref()
                .map(|alias| format!(" as {alias}"))
                .unwrap_or_default(),
            if import.is_glob { " [glob]" } else { "" }
        );
    }
    for diagnostic in &analysis.imports.diagnostics {
        println!(
            "  import diagnostic {}: {}",
            diagnostic.file, diagnostic.message
        );
    }
    println!(
        "Potential test gaps (heuristic): {}",
        analysis.test_gaps.gaps.len()
    );
    for gap in &analysis.test_gaps.gaps {
        println!(
            "  {} {}:{}-{} — {}",
            gap.symbol.name,
            gap.symbol.file,
            gap.symbol.start_line,
            gap.symbol.end_line,
            gap.explanation
        );
    }
    for diagnostic in &analysis.test_gaps.diagnostics {
        println!(
            "  test-gap diagnostic {}: {}",
            diagnostic.file, diagnostic.message
        );
    }
    let summary = &analysis.architecture.summary;
    println!(
        "Architecture files: {} ({} Rust) | with symbols: {} | without symbols: {} | symbols: {} ({} tests) | dependencies: {} | unresolved modules: {} | parse errors: {} | symbol diagnostics: {} | total diagnostics: {}",
        summary.total_source_files,
        summary.rust_files,
        summary.files_with_symbols,
        summary.files_without_symbols,
        summary.total_symbols,
        summary.test_symbols,
        summary.module_dependencies,
        summary.unresolved_modules,
        summary.dependency_parse_errors,
        summary.symbol_diagnostics,
        analysis.architecture.diagnostics.len()
    );
    for file in &analysis.architecture.files {
        let classification = if file.is_test_file {
            " [test file]"
        } else {
            ""
        };
        println!("  {}{}", file.path, classification);
        for dependency in &file.dependencies {
            println!("    -> {dependency}");
        }
        for symbol in &file.symbols {
            let scope = if symbol.scope.is_empty() {
                String::new()
            } else {
                format!(" [{}]", symbol.scope.join("::"))
            };
            let test = if symbol.is_test { " [test]" } else { "" };
            println!(
                "    {} {}:{}-{}{}{}",
                symbol.kind, symbol.name, symbol.start_line, symbol.end_line, scope, test
            );
        }
    }
    for diagnostic in &analysis.architecture.diagnostics {
        println!(
            "  architecture diagnostic [{}] {}: {}",
            diagnostic.category,
            diagnostic.file.as_deref().unwrap_or("<repository>"),
            diagnostic.message
        );
    }
    println!(
        "Current deterministic findings: {}",
        analysis.findings.len()
    );
    for finding in &analysis.findings {
        if let Err(errors) = finding.validate() {
            println!(
                "  skipping invalid finding {}: {}",
                finding.id,
                errors.join("; ")
            );
            continue;
        }
        let location = finding
            .evidence
            .first()
            .map(|e| {
                format!(
                    "{}{}",
                    e.file,
                    e.start_line
                        .map(|line| format!(":{line}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        println!(
            "  [{:?}] {} — {} (impact {:?}, confidence {:?}, effort {:?}, {:?})",
            finding.category,
            finding.title,
            location,
            finding.impact,
            finding.confidence,
            finding.effort,
            finding.validation_status
        );
        for evidence in &finding.evidence {
            println!("    {}", evidence.explanation);
        }
    }
    println!("\nAnalysis is static; findings and test-gap signals require human review.");
}

fn print_discovery(report: &application::DiscoveryReport) {
    println!("HECATE — GITHUB CONTRIBUTION DISCOVERY");
    println!("Repository: {}", report.github_repository.url);
    println!(
        "Default branch: {} | commit: {}",
        report.github_repository.default_branch, report.github_repository.commit
    );
    println!("Open issues analyzed: {}", report.issues.len());
    println!(
        "Rust files: {} | modules: {} | symbols: {}",
        report
            .analysis
            .repository
            .source_files
            .iter()
            .filter(|path| path.ends_with(".rs"))
            .count(),
        report.analysis.module_index.modules.len(),
        report.analysis.symbols.symbols.len()
    );
    if report.opportunities.is_empty() {
        println!("\nNo open issues were returned by GitHub.");
        return;
    }
    println!("\nRanked opportunities (heuristic; review issue and code evidence before choosing):");
    for (index, opportunity) in report.opportunities.iter().take(10).enumerate() {
        println!(
            "\n{}. #{} {} [{}]",
            index + 1,
            opportunity.issue.number,
            opportunity.issue.title,
            opportunity.ranking.score
        );
        println!("   {}", opportunity.issue.url);
        if !opportunity.issue.body.trim().is_empty() {
            let normalized_body = opportunity
                .issue
                .body
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let compact_body = normalized_body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let preview = compact_body.chars().take(280).collect::<String>();
            println!(
                "   {}{}",
                preview,
                if compact_body.chars().count() > 280 {
                    "…"
                } else {
                    ""
                }
            );
        }
        println!(
            "   Updated: {} | comments: {}",
            opportunity.issue.updated_at, opportunity.issue.comments
        );
        if !opportunity.issue.labels.is_empty() {
            println!("   Labels: {}", opportunity.issue.labels.join(", "));
        }
        println!(
            "   Factor scores /100: relevance {}, impact {}, confidence {}, effort {}, risk {}",
            opportunity.ranking.relevance,
            opportunity.ranking.impact,
            opportunity.ranking.confidence,
            opportunity.ranking.effort,
            opportunity.ranking.risk
        );
        for rationale in &opportunity.ranking.rationale {
            println!("   - {rationale}");
        }
        if opportunity.matches.is_empty() {
            println!(
                "   No source location matched the issue text; this is not evidence that the issue lacks a code fix."
            );
        } else {
            println!("   Possible code locations:");
            for matched in &opportunity.matches {
                let symbol = matched
                    .symbol
                    .as_ref()
                    .map(|value| {
                        format!(
                            "{} ({value})",
                            matched
                                .line
                                .map(|line| format!(":{line}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                println!(
                    "   - {}{} (relevance {})",
                    matched.file, symbol, matched.relevance
                );
                for evidence in &matched.evidence {
                    println!("     {evidence}");
                }
            }
        }
    }
}
