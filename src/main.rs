pub mod agent;
mod analysis;
mod application;
mod contribution_cli;
mod discovery;
mod github;
mod inspection;
mod planning;
mod terminal;
mod workspace;

use clap::{Parser, Subcommand};
use std::path::Path;

macro_rules! safe_line {
    ($($argument:tt)*) => {{
        let line = format!($($argument)*);
        println!("{}", terminal::escape::single_line(&line));
    }};
}

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
    /// Prepare a workspace, obtain or generate a proposal, require approval, and apply it
    #[command(group(
        clap::ArgGroup::new("proposal_source")
            .required(true)
            .args(["proposal", "generate"])
    ))]
    Contribute {
        repository: String,
        issue_number: u64,
        /// Bounded JSON PatchProposal file (schema version 1)
        #[arg(long)]
        proposal: Option<std::path::PathBuf>,
        /// Generate an untrusted proposal with the configured native model provider
        #[arg(long)]
        generate: bool,
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
            terminal::presentation::information(
                "Fetching repository and open issues from GitHub, then analyzing local Rust source...",
            );
            application::discover_github_contributions(&repository)
                .map(|report| print_discovery(&report))
        }
        Commands::Plan {
            repository,
            issue_number,
        } => {
            terminal::presentation::information(&format!(
                "Fetching repository and investigating issue #{issue_number}..."
            ));
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
            generate,
            workspace_dir,
        } => contribute(
            &repository,
            issue_number,
            proposal.as_deref(),
            generate,
            workspace_dir.as_deref(),
        )
        .map_err(|error| contribution_cli::safe_cli_error(&error)),
    };
    if let Err(error) = result {
        terminal::presentation::error(&error);
        std::process::exit(1);
    }
}

fn prepare_workspace(
    repository_input: &str,
    issue_number: u64,
    workspace_root: Option<&Path>,
) -> Result<(), String> {
    prepare_workspace_with_proposal(
        repository_input,
        issue_number,
        workspace_root,
        true,
        false,
        false,
    )
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
    model_generation: bool,
) -> Result<Option<PreparedContribution>, String> {
    terminal::presentation::information(
        "Verifying the open issue and generating its contribution plan...",
    );
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
    terminal::presentation::section_title("HECATE — ISOLATED WORKSPACE PREPARATION");
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
    if applying_proposal && model_generation {
        println!(
            "\nThis creates a new local clone and branch. HECATE will send the contribution plan and read-only repository excerpts to the configured model provider to generate an untrusted proposal. HECATE will validate it, show the complete diff, and require separate full-hash approval before applying it. It will not run tests, commit, push, or create a pull request."
        );
    } else if applying_proposal {
        println!(
            "\nThis creates a new local clone and branch. HECATE will validate the supplied proposal, show its complete diff, and require separate full-hash approval before applying it. It will not run tests, commit, push, or create a pull request."
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
    terminal::presentation::success("Workspace created successfully.");
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
    proposal_path: Option<&Path>,
    generate: bool,
    workspace_root: Option<&Path>,
) -> Result<(), String> {
    use std::io::IsTerminal;

    github::GitHubClient::parse_repository(repository_input)?;
    if issue_number == 0 {
        return Err("Issue number must be greater than zero".into());
    }
    let proposal = proposal_path
        .map(contribution_cli::load_proposal)
        .transpose()?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "The contribute workflow requires interactive stdin and stdout terminals; no workspace was created".into(),
        );
    }
    let provider_config =
        if generate {
            Some(agent::OpenAiCompatibleConfig::from_env().map_err(|error| {
                format!("Model provider configuration failed: {}", error.message)
            })?)
        } else {
            None
        };

    let Some(prepared) = prepare_workspace_with_proposal(
        repository_input,
        issue_number,
        workspace_root,
        false,
        true,
        generate,
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
    let proposal = if let Some(proposal) = proposal {
        proposal
    } else {
        let config = provider_config.ok_or_else(|| "A proposal source is required".to_string())?;
        let provider = agent::OpenAiCompatibleProvider::new(config).map_err(|error| {
            format!(
                "Could not initialize configured model provider: {}",
                error.message
            )
        })?;
        terminal::presentation::warning(
            "Generating an untrusted patch proposal with the configured model...",
        );
        let generated = agent::proposal_generation::generate_patch_proposal(
            provider,
            &prepared.workspace,
            &context,
            &prepared.plan,
            &session_id,
            agent::ExecutionLimits::default(),
            &agent::CancellationToken::default(),
        )
        .map_err(|error| {
            format!(
                "{error}. No patch was applied. Prepared workspace was preserved at {}",
                contribution_cli::safe_cli_error(&prepared.workspace.path.display().to_string())
            )
        })?;
        println!(
            "Proposal generated after {} model iteration(s) and {} read-only tool call(s).",
            generated.summary.model_iterations, generated.summary.tool_calls
        );
        generated.proposal
    };
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
    terminal::presentation::section_title("HECATE — CONTRIBUTION PLAN (FOR HUMAN REVIEW)");
    safe_line!("Repository: {}", plan.repository_url);
    safe_line!("Issue: #{} {}", plan.issue_number, plan.issue_title);
    safe_line!("URL: {}", plan.issue_url);
    safe_line!(
        "Labels: {}",
        if plan.labels.is_empty() {
            "none".into()
        } else {
            plan.labels.join(", ")
        }
    );
    safe_line!(
        "Created: {} | updated: {} | comments: {}",
        plan.created_at,
        plan.updated_at,
        plan.comments
    );
    terminal::presentation::section_title("Expected outcome");
    safe_line!("  {}", plan.expected_outcome);
    terminal::presentation::section_title("Problem interpretation (hypothesis)");
    safe_line!("  {}", plan.interpretation);
    terminal::presentation::section_title("Relevant code evidence");
    if plan.locations.is_empty() {
        safe_line!(
            "  No relevant code location was identified. Treat the issue as unlocated; inspect manually before implementation."
        );
    }
    for location in &plan.locations {
        safe_line!(
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
            safe_line!("    Evidence: {evidence}");
        }
        for relationship in &location.relationships {
            safe_line!("    Relationship: {relationship}");
        }
        for gap in &location.test_gaps {
            safe_line!("    Test-gap signal (heuristic): {gap}");
        }
    }
    terminal::presentation::section_title("Relevant repository documentation");
    if plan.documents.is_empty() {
        safe_line!("  No relevant documentation excerpt found (or documentation is unavailable).");
    }
    for document in &plan.documents {
        safe_line!("  {} —", document.path);
        terminal::presentation::multiline_block("    ", &document.excerpt);
    }
    terminal::presentation::section_title("Proposed steps");
    for (index, step) in plan.steps.iter().enumerate() {
        safe_line!("  {}. {}", index + 1, step);
    }
    terminal::presentation::section_title("Tests and validation");
    for test in &plan.test_recommendations {
        safe_line!("  - {test}");
    }
    for command in &plan.validation_commands {
        safe_line!("  - Run: {command}");
    }
    terminal::presentation::section_title("Risks and assumptions");
    for item in &plan.risks_and_assumptions {
        safe_line!("  - {item}");
    }
    terminal::presentation::section_title("Unresolved questions");
    if plan.unresolved_questions.is_empty() {
        safe_line!(
            "  None identified from available issue and repository evidence; confirm acceptance criteria with maintainers."
        );
    }
    for question in &plan.unresolved_questions {
        safe_line!("  - {question}");
    }
    println!();
    safe_line!(
        "This is a heuristic investigation plan, not a confirmed diagnosis or code change. Review it and select/approve any future implementation yourself."
    );
}

fn print_analysis(
    analysis: &application::RepositoryAnalysis,
    github: Option<&github::GitHubRepository>,
) {
    terminal::presentation::section_title("HECATE — REPOSITORY INSPECTION");
    if let Some(repository) = github {
        safe_line!("Repository: {} ({})", repository.url, repository.commit);
    }
    safe_line!("Root: {}", analysis.repository.root.display());
    safe_line!(
        "Git repository: {}",
        if analysis.repository.is_git_repository {
            "yes"
        } else {
            "no"
        }
    );
    safe_line!(
        "Languages: {}",
        analysis
            .repository
            .language_counts
            .iter()
            .map(|(language, count)| format!("{language} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    safe_line!("Source files: {}", analysis.repository.source_files.len());
    safe_line!(
        "Top-level entries: {}",
        analysis.repository.entries.join(", ")
    );
    safe_line!(
        "Project markers: {}",
        if analysis.repository.project_markers.is_empty() {
            "none".to_string()
        } else {
            analysis.repository.project_markers.join(", ")
        }
    );
    safe_line!(
        "Rust modules indexed: {}",
        analysis.module_index.modules.len()
    );
    for module in &analysis.module_index.modules {
        let logical = if module.module_path.is_empty() {
            "(crate root)".to_string()
        } else {
            module.module_path.join("::")
        };
        safe_line!(
            "  module {logical} [{}] -> {}",
            module.crate_root,
            module.file
        );
    }
    for unresolved in &analysis.module_index.unresolved_modules {
        safe_line!(
            "  unresolved module {} in {} ({})",
            unresolved.module_name,
            unresolved.source,
            unresolved.crate_root
        );
    }
    for diagnostic in &analysis.module_index.diagnostics {
        safe_line!(
            "  module index diagnostic [{}] {}: {}",
            diagnostic.crate_root,
            diagnostic.file,
            diagnostic.message
        );
    }
    safe_line!(
        "Module dependencies: {}",
        analysis.dependency_graph.dependencies.len()
    );
    safe_line!(
        "Unresolved module declarations: {}",
        analysis.dependency_graph.unresolved_modules.len()
    );
    for dependency in &analysis.dependency_graph.dependencies {
        safe_line!(
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
        safe_line!(
            "  unresolved declaration {} in {}",
            unresolved.module_name,
            unresolved.source
        );
    }
    for error in &analysis.dependency_graph.parse_errors {
        safe_line!("  parse diagnostic {}: {}", error.source, error.message);
    }
    safe_line!("Rust symbols: {}", analysis.symbols.symbols.len());
    for symbol in &analysis.symbols.symbols {
        let test = if symbol.is_test { " [test]" } else { "" };
        safe_line!(
            "  {} {}:{}:{}-{}{}",
            symbol.kind,
            symbol.name,
            symbol.file,
            symbol.start_line,
            symbol.end_line,
            test
        );
    }
    for diagnostic in &analysis.symbols.diagnostics {
        safe_line!(
            "  symbol diagnostic {}: {}",
            diagnostic.file,
            diagnostic.message
        );
    }
    safe_line!("Imports analyzed: {}", analysis.imports.imports.len());
    for import in &analysis.imports.imports {
        safe_line!(
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
        safe_line!(
            "  import diagnostic {}: {}",
            diagnostic.file,
            diagnostic.message
        );
    }
    safe_line!(
        "Potential test gaps (heuristic): {}",
        analysis.test_gaps.gaps.len()
    );
    for gap in &analysis.test_gaps.gaps {
        safe_line!(
            "  {} {}:{}-{} — {}",
            gap.symbol.name,
            gap.symbol.file,
            gap.symbol.start_line,
            gap.symbol.end_line,
            gap.explanation
        );
    }
    for diagnostic in &analysis.test_gaps.diagnostics {
        safe_line!(
            "  test-gap diagnostic {}: {}",
            diagnostic.file,
            diagnostic.message
        );
    }
    let summary = &analysis.architecture.summary;
    safe_line!(
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
        safe_line!("  {}{}", file.path, classification);
        for dependency in &file.dependencies {
            safe_line!("    -> {dependency}");
        }
        for symbol in &file.symbols {
            let scope = if symbol.scope.is_empty() {
                String::new()
            } else {
                format!(" [{}]", symbol.scope.join("::"))
            };
            let test = if symbol.is_test { " [test]" } else { "" };
            safe_line!(
                "    {} {}:{}-{}{}{}",
                symbol.kind,
                symbol.name,
                symbol.start_line,
                symbol.end_line,
                scope,
                test
            );
        }
    }
    for diagnostic in &analysis.architecture.diagnostics {
        safe_line!(
            "  architecture diagnostic [{}] {}: {}",
            diagnostic.category,
            diagnostic.file.as_deref().unwrap_or("<repository>"),
            diagnostic.message
        );
    }
    safe_line!(
        "Current deterministic findings: {}",
        analysis.findings.len()
    );
    for finding in &analysis.findings {
        if let Err(errors) = finding.validate() {
            safe_line!(
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
        safe_line!(
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
            safe_line!("    {}", evidence.explanation);
        }
    }
    println!();
    safe_line!("Analysis is static; findings and test-gap signals require human review.");
}

fn print_discovery(report: &application::DiscoveryReport) {
    terminal::presentation::section_title("HECATE — GITHUB CONTRIBUTION DISCOVERY");
    safe_line!("Repository: {}", report.github_repository.url);
    safe_line!(
        "Default branch: {} | commit: {}",
        report.github_repository.default_branch,
        report.github_repository.commit
    );
    safe_line!("Open issues analyzed: {}", report.issues.len());
    safe_line!(
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
        println!();
        safe_line!("No open issues were returned by GitHub.");
        return;
    }
    terminal::presentation::section_title("Ranked opportunities");
    terminal::presentation::warning(
        "Rankings and matches are heuristic; review issue and code evidence before choosing.",
    );
    for (index, opportunity) in report.opportunities.iter().take(10).enumerate() {
        println!();
        safe_line!(
            "{}. #{} {} [{}]",
            index + 1,
            opportunity.issue.number,
            opportunity.issue.title,
            opportunity.ranking.score
        );
        safe_line!("   {}", opportunity.issue.url);
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
            safe_line!(
                "   {}{}",
                preview,
                if compact_body.chars().count() > 280 {
                    "…"
                } else {
                    ""
                }
            );
        }
        safe_line!(
            "   Updated: {} | comments: {}",
            opportunity.issue.updated_at,
            opportunity.issue.comments
        );
        if !opportunity.issue.labels.is_empty() {
            safe_line!("   Labels: {}", opportunity.issue.labels.join(", "));
        }
        safe_line!(
            "   Factor scores /100: relevance {}, impact {}, confidence {}, effort {}, risk {}",
            opportunity.ranking.relevance,
            opportunity.ranking.impact,
            opportunity.ranking.confidence,
            opportunity.ranking.effort,
            opportunity.ranking.risk
        );
        for rationale in &opportunity.ranking.rationale {
            safe_line!("   - {rationale}");
        }
        if opportunity.matches.is_empty() {
            safe_line!(
                "   No source location matched the issue text; this is not evidence that the issue lacks a code fix."
            );
        } else {
            safe_line!("   Possible code locations:");
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
                safe_line!(
                    "   - {}{} (relevance {})",
                    matched.file,
                    symbol,
                    matched.relevance
                );
                for evidence in &matched.evidence {
                    safe_line!("     {evidence}");
                }
            }
        }
    }
    println!();
    safe_line!("Plan a selected issue with: hecate plan <repository> <issue-number>");
}
