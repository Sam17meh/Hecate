pub mod agent;
mod analysis;
mod application;
mod contribution_cli;
mod discovery;
mod github;
mod inspection;
mod planning;
mod workspace;

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
    /// Investigate a discovered issue and prepare an evidence-backed contribution plan
    Plan {
        repository: String,
        issue_number: u64,
    },
    /// Review an issue plan, request confirmation, and create an isolated local Git clone
    Prepare {
        repository: String,
        issue_number: u64,
        /// Override the HECATE-managed workspace root (also configurable via HECATE_WORKSPACE_DIR)
        #[arg(long)]
        workspace_dir: Option<std::path::PathBuf>,
    },
    /// Prepare a workspace, review a user-supplied patch, require approval, and apply it
    Contribute {
        repository: String,
        issue_number: u64,
        /// Bounded JSON PatchProposal file (schema version 1)
        #[arg(long)]
        proposal: std::path::PathBuf,
        /// Override the HECATE-managed workspace root (also configurable via HECATE_WORKSPACE_DIR)
        #[arg(long)]
        workspace_dir: Option<std::path::PathBuf>,
    },
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
        Commands::Plan {
            repository,
            issue_number,
        } => {
            println!("Fetching repository and investigating issue #{issue_number}...");
            application::plan_github_issue(&repository, issue_number).map(|plan| print_plan(&plan))
        }
        Commands::Prepare {
            repository,
            issue_number,
            workspace_dir,
        } => prepare_workspace(&repository, issue_number, workspace_dir.as_deref()),
        Commands::Contribute {
            repository,
            issue_number,
            proposal,
            workspace_dir,
        } => contribute(
            &repository,
            issue_number,
            &proposal,
            workspace_dir.as_deref(),
        )
        .map_err(|error| contribution_cli::safe_cli_error(&error)),
    };
    if let Err(error) = result {
        eprintln!("HECATE: {error}");
        std::process::exit(1);
    }
}

fn prepare_workspace(
    repository_input: &str,
    issue_number: u64,
    workspace_root: Option<&Path>,
) -> Result<(), String> {
    prepare_workspace_with_proposal(repository_input, issue_number, workspace_root, true, false)
        .map(|_| ())
}

struct PreparedContribution {
    repository: github::GitHubRepository,
    plan: planning::ContributionPlan,
    workspace: workspace::WorkspaceResult,
}

fn prepare_workspace_with_proposal(
    repository_input: &str,
    issue_number: u64,
    workspace_root: Option<&Path>,
    display_full_plan: bool,
    applying_proposal: bool,
) -> Result<Option<PreparedContribution>, String> {
    println!("Verifying the open issue and generating its contribution plan...");
    let (repository, plan) = application::investigate_github_issue(repository_input, issue_number)?;
    if display_full_plan {
        print_plan(&plan);
    }
    let default_root;
    let root = if let Some(root) = workspace_root {
        root
    } else {
        default_root = workspace::default_workspace_root();
        &default_root
    };
    let preview = workspace::preview_workspace(&repository, issue_number, root)?;
    println!("\nHECATE — ISOLATED WORKSPACE PREPARATION");
    println!(
        "Repository: {}",
        contribution_cli::safe_cli_error(&repository.url)
    );
    println!(
        "Issue: #{} {}",
        plan.issue_number,
        contribution_cli::safe_cli_error(&plan.issue_title)
    );
    println!(
        "Base branch: {}",
        contribution_cli::safe_cli_error(&repository.default_branch)
    );
    println!(
        "New branch: {}",
        contribution_cli::safe_cli_error(&preview.branch)
    );
    println!(
        "Workspace: {}",
        contribution_cli::safe_cli_error(&preview.path.display().to_string())
    );
    println!(
        "Git checkout: {}",
        contribution_cli::safe_cli_error(&preview.checkout_path.display().to_string())
    );
    println!(
        "Plan metadata: .hecate/plan-v1.json (version {})",
        workspace::PLAN_SCHEMA_VERSION
    );
    if applying_proposal {
        println!(
            "\nThis creates a new local clone and branch. HECATE will then validate the supplied proposal, show its complete diff, and require separate full-hash approval before applying it. It will not run tests, commit, push, or create a pull request."
        );
    } else {
        println!(
            "\nThis creates a local clone and branch. HECATE will not edit source files, push, or create a pull request."
        );
    }
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let result = contribution_cli::confirm_and_create_workspace(&mut input, &mut output, || {
        workspace::create_workspace(&repository, issue_number, &plan, root, true)
    })?;
    let Some(result) = result else {
        return Ok(None);
    };
    println!("\nWorkspace created successfully.");
    println!(
        "Branch: {}",
        contribution_cli::safe_cli_error(&result.branch)
    );
    println!(
        "Checkout: {}",
        contribution_cli::safe_cli_error(&result.checkout_path.display().to_string())
    );
    println!(
        "Workspace metadata: {}",
        contribution_cli::safe_cli_error(&result.path.join(".hecate").display().to_string())
    );
    println!(
        "Base commit: {}",
        contribution_cli::safe_cli_error(&result.commit)
    );
    println!(
        "Git status: {}",
        contribution_cli::safe_cli_error(&result.git_status)
    );
    if !applying_proposal {
        println!("No changes have been made to the cloned source tree.");
    }
    Ok(Some(PreparedContribution {
        repository,
        plan,
        workspace: result,
    }))
}

fn contribute(
    repository_input: &str,
    issue_number: u64,
    proposal_path: &Path,
    workspace_root: Option<&Path>,
) -> Result<(), String> {
    use std::io::IsTerminal;

    github::GitHubClient::parse_repository(repository_input)?;
    if issue_number == 0 {
        return Err("Issue number must be greater than zero".into());
    }
    let proposal = contribution_cli::load_proposal(proposal_path)?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "The contribute workflow requires interactive stdin and stdout terminals; no workspace was created".into(),
        );
    }

    let Some(prepared) = prepare_workspace_with_proposal(
        repository_input,
        issue_number,
        workspace_root,
        false,
        true,
    )?
    else {
        return Ok(());
    };
    let context = agent::session::ContributionContext {
        repository_url: prepared.repository.url.clone(),
        issue_number,
        issue_title: prepared.plan.issue_title.clone(),
        plan: agent::session::ContributionPlanReference {
            artifact_id: ".hecate/plan-v1.json".into(),
            repository_url: prepared.plan.repository_url.clone(),
            issue_number,
            schema_version: workspace::PLAN_SCHEMA_VERSION,
            review_status: agent::session::PlanReviewStatus::Approved,
        },
    };
    let session_id = contribution_cli::new_cli_session_id()?;
    let report = contribution_cli::validate_approve_apply(
        &prepared.workspace,
        &context,
        &proposal,
        &session_id,
        |patch, _validator| {
            contribution_cli::print_review_context(
                &prepared.repository.url,
                issue_number,
                &prepared.plan.issue_title,
                &prepared.workspace.path,
                patch,
            )
        },
        agent::patch_approval::approve_interactively,
        agent::patch_application::apply_approved_patch,
    )
    .map_err(|error| {
        format!(
            "{error}. Prepared workspace was preserved at {}",
            prepared.workspace.path.display()
        )
    })?;
    contribution_cli::report_application(&report)
}

fn print_plan(plan: &planning::ContributionPlan) {
    println!("HECATE — CONTRIBUTION PLAN (FOR HUMAN REVIEW)");
    println!("Repository: {}", plan.repository_url);
    println!("Issue: #{} {}", plan.issue_number, plan.issue_title);
    println!("URL: {}", plan.issue_url);
    println!(
        "Labels: {}",
        if plan.labels.is_empty() {
            "none".into()
        } else {
            plan.labels.join(", ")
        }
    );
    println!(
        "Created: {} | updated: {} | comments: {}",
        plan.created_at, plan.updated_at, plan.comments
    );
    println!("\nExpected outcome");
    println!("  {}", plan.expected_outcome);
    println!("\nProblem interpretation (hypothesis)");
    println!("  {}", plan.interpretation);
    println!("\nRelevant code evidence");
    if plan.locations.is_empty() {
        println!(
            "  No relevant code location was identified. Treat the issue as unlocated; inspect manually before implementation."
        );
    }
    for location in &plan.locations {
        println!(
            "  {}{}{}",
            location.file,
            location
                .line
                .map(|line| format!(":{line}"))
                .unwrap_or_default(),
            location
                .symbol
                .as_ref()
                .map(|symbol| format!(" ({symbol})"))
                .unwrap_or_default()
        );
        for evidence in &location.evidence {
            println!("    Evidence: {evidence}");
        }
        for relationship in &location.relationships {
            println!("    Relationship: {relationship}");
        }
        for gap in &location.test_gaps {
            println!("    Test-gap signal (heuristic): {gap}");
        }
    }
    println!("\nRelevant repository documentation");
    if plan.documents.is_empty() {
        println!("  No relevant documentation excerpt found (or documentation is unavailable).");
    }
    for document in &plan.documents {
        println!("  {} — {}", document.path, document.excerpt);
    }
    println!("\nProposed steps");
    for (index, step) in plan.steps.iter().enumerate() {
        println!("  {}. {}", index + 1, step);
    }
    println!("\nTests and validation");
    for test in &plan.test_recommendations {
        println!("  - {test}");
    }
    for command in &plan.validation_commands {
        println!("  - Run: {command}");
    }
    println!("\nRisks and assumptions");
    for item in &plan.risks_and_assumptions {
        println!("  - {item}");
    }
    println!("\nUnresolved questions");
    if plan.unresolved_questions.is_empty() {
        println!(
            "  None identified from available issue and repository evidence; confirm acceptance criteria with maintainers."
        );
    }
    for question in &plan.unresolved_questions {
        println!("  - {question}");
    }
    println!(
        "\nThis is a heuristic investigation plan, not a confirmed diagnosis or code change. Review it and select/approve any future implementation yourself."
    );
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
    println!("\nPlan a selected issue with: hecate plan <repository> <issue-number>");
}
