mod analysis;
mod inspection;

use analysis::architecture::build_architecture_map;
use analysis::dependencies::build_dependency_graph;
use analysis::symbols::analyze_repository;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Hecate — Navigate. Understand. Improve.
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
    /// Inspect a GitHub repository or local codebase
    Inspect {
        /// Repository in owner/repo format or a local path
        repository: String,
    },
}

// The two types of repositories Hecate can recognise.
#[derive(Debug)]
enum RepositoryTarget {
    Local(PathBuf),
    GitHub { owner: String, repo: String },
}

// Identify what kind of target the user provided.
fn identify_target(input: &str) -> Result<RepositoryTarget, String> {
    let path = Path::new(input);

    // First, check whether the input is an existing directory.
    if path.exists() && path.is_dir() {
        return Ok(RepositoryTarget::Local(
            path.canonicalize()
                .map_err(|e| format!("Could not resolve path: {e}"))?,
        ));
    }

    // Otherwise, check whether it follows owner/repo format.
    let parts: Vec<&str> = input.split('/').collect();

    if parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts.iter().all(|part| {
            part.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        })
    {
        return Ok(RepositoryTarget::GitHub {
            owner: parts[0].to_string(),
            repo: parts[1].to_string(),
        });
    }

    Err(format!("Invalid repository target: {input}"))
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Inspect { repository } => {
            println!("Hecate Repository Inspector");

            match identify_target(&repository) {
                Ok(RepositoryTarget::Local(path)) => {
                    println!("Target type: Local directory");

                    match inspection::inspect_local(&path) {
                        Ok(info) => {
                            println!("\n--- Repository Overview ---");
                            println!("Root: {}", info.root.display());
                            println!(
                                "Git repository: {}",
                                if info.is_git_repository { "Yes" } else { "No" }
                            );

                            println!("\n--- Top-level Entries ---");
                            for entry in &info.entries {
                                println!("  {}", entry);
                            }

                            println!("\n--- Detected Project Markers ---");
                            if info.project_markers.is_empty() {
                                println!("  No known project markers found.");
                            } else {
                                for marker in &info.project_markers {
                                    println!("  {}", marker);
                                }
                            }

                            println!("\n--- Programming Languages ---");
                            if info.language_counts.is_empty() {
                                println!("  No recognised programming languages found.");
                            } else {
                                for (language, count) in &info.language_counts {
                                    println!("  {}: {} files", language, count);
                                }
                            }

                            println!("\n--- Source Files ---");
                            println!("Total source files: {}", info.source_files.len());

                            for file in &info.source_files {
                                println!("  {}", file);
                            }
                            println!("\n--- Rust Module Dependencies ---");

                            match build_dependency_graph(&info) {
                                Ok(graph) => {
                                    if graph.dependencies.is_empty() {
                                        println!("  No Rust module dependencies found.");
                                    } else {
                                        for dependency in &graph.dependencies {
                                            println!(
                                                "  {} -> {}",
                                                dependency.source, dependency.target
                                            );
                                        }

                                        println!(
                                            "\nTotal module dependencies: {}",
                                            graph.dependencies.len()
                                        );

                                        println!("\n--- Unresolved Module Declarations ---");

                                        if graph.unresolved_modules.is_empty() {
                                            println!("  No unresolved module declarations found.");
                                        } else {
                                            for module in &graph.unresolved_modules {
                                                let context = if module.module_context.is_empty() {
                                                    String::new()
                                                } else {
                                                    format!(
                                                        " (inside {})",
                                                        module.module_context.join("::")
                                                    )
                                                };

                                                println!(
                                                    "  {} -> {}{}",
                                                    module.source, module.module_name, context
                                                );
                                            }
                                        }

                                        println!(
                                            "\nTotal unresolved modules: {}",
                                            graph.unresolved_modules.len()
                                        );

                                        println!("\n--- Parsing Diagnostics ---");

                                        if graph.parse_errors.is_empty() {
                                            println!("  No parsing errors found.");
                                        } else {
                                            for error in &graph.parse_errors {
                                                println!("  File: {}", error.source);
                                                println!("  Error: {}", error.message);
                                            }
                                        }

                                        println!(
                                            "\nTotal parsing errors: {}",
                                            graph.parse_errors.len()
                                        );
                                    }
                                }

                                Err(error) => {
                                    eprintln!("Dependency analysis failed: {error}");
                                }
                            }
                            println!("\n--- Rust Symbol Analysis ---");

                            match analyze_repository(&info.source_files, &info.root) {
                                Ok(symbol_analysis) => {
                                    println!(
                                        "Total symbols detected: {}",
                                        symbol_analysis.symbols.len()
                                    );

                                    let mut grouped: std::collections::BTreeMap<&str, Vec<_>> =
                                        std::collections::BTreeMap::new();

                                    for symbol in &symbol_analysis.symbols {
                                        grouped
                                            .entry(symbol.kind.as_str())
                                            .or_default()
                                            .push(symbol);
                                    }

                                    for (kind, symbols) in &grouped {
                                        println!("\n{} ({}):", kind, symbols.len());

                                        for symbol in symbols {
                                            println!(
                                                "  {} - {}:{}-{}",
                                                symbol.name,
                                                symbol.file,
                                                symbol.start_line,
                                                symbol.end_line
                                            );

                                            if !symbol.scope.is_empty() {
                                                println!("    Scope: {}", symbol.scope.join("::"));
                                            }

                                            if symbol.is_test {
                                                println!("    Classification: test code");
                                            }
                                        }
                                    }
                                    println!("\n--- Symbol diagnostics ---");

                                    if symbol_analysis.diagnostics.is_empty() {
                                        println!("  No symbol analysis errors.");
                                    } else {
                                        for diagnostic in &symbol_analysis.diagnostics {
                                            println!("  File: {}", diagnostic.file);
                                            println!("  Error: {}", diagnostic.message);
                                        }
                                    }

                                    println!(
                                        "Total symbol diagnostics: {}",
                                        symbol_analysis.diagnostics.len()
                                    );

                                    println!("\n--- Repository Architecture Map ---");

                                    match build_dependency_graph(&info) {
                                        Ok(architecture_dependencies) => {
                                            let architecture = build_architecture_map(
                                                &info,
                                                &architecture_dependencies,
                                                &symbol_analysis,
                                            );

                                            let summary = &architecture.summary;

                                            println!("\nArchitecture Summary");
                                            println!(
                                                "  Source files: {}",
                                                summary.total_source_files
                                            );
                                            println!("  Rust files: {}", summary.rust_files);
                                            println!(
                                                "  Files with symbols: {}",
                                                summary.files_with_symbols
                                            );
                                            println!(
                                                "  Files without symbols: {}",
                                                summary.files_without_symbols
                                            );
                                            println!("  Total symbols: {}", summary.total_symbols);
                                            println!("  Test symbols: {}", summary.test_symbols);
                                            println!(
                                                "  Module dependencies: {}",
                                                summary.module_dependencies
                                            );
                                            println!(
                                                "  Unresolved modules: {}",
                                                summary.unresolved_modules
                                            );
                                            println!(
                                                "  Dependency parse errors: {}",
                                                summary.dependency_parse_errors
                                            );
                                            println!(
                                                "  Symbol diagnostics: {}",
                                                summary.symbol_diagnostics
                                            );

                                            println!("\nFile Architecture");

                                            for file in &architecture.files {
                                                println!("\n  {}", file.path);

                                                if file.is_test_file {
                                                    println!("    Classification: test file");
                                                }

                                                if !file.dependencies.is_empty() {
                                                    println!("    Dependencies:");

                                                    for dependency in &file.dependencies {
                                                        println!("      -> {}", dependency);
                                                    }
                                                }

                                                if !file.symbols.is_empty() {
                                                    println!("    Symbols:");

                                                    for symbol in &file.symbols {
                                                        let scope = if symbol.scope.is_empty() {
                                                            String::new()
                                                        } else {
                                                            format!(
                                                                " [{}]",
                                                                symbol.scope.join("::")
                                                            )
                                                        };

                                                        let test_label = if symbol.is_test {
                                                            " [test]"
                                                        } else {
                                                            ""
                                                        };

                                                        println!(
                                                            "      {} {}:{}-{}{}{}",
                                                            symbol.kind,
                                                            symbol.name,
                                                            symbol.start_line,
                                                            symbol.end_line,
                                                            scope,
                                                            test_label
                                                        );
                                                    }
                                                }
                                            }

                                            println!("\nArchitecture Diagnostics");

                                            if architecture.diagnostics.is_empty() {
                                                println!("  No architecture diagnostics.");
                                            } else {
                                                for diagnostic in &architecture.diagnostics {
                                                    let location = diagnostic
                                                        .file
                                                        .as_deref()
                                                        .unwrap_or("<repository>");

                                                    println!(
                                                        "  [{}] {}: {}",
                                                        diagnostic.category,
                                                        location,
                                                        diagnostic.message
                                                    );
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "Architecture dependency analysis failed: {error}"
                                            );
                                        }
                                    }
                                }
                                Err(error) => {
                                    eprintln!("Symbol analysis failed: {error}");
                                }
                            }
                        }

                        Err(error) => {
                            eprintln!("Inspection failed: {error}");
                            std::process::exit(1);
                        }
                    }
                }

                Ok(RepositoryTarget::GitHub { owner, repo }) => {
                    println!("Target type: GitHub repository");
                    println!("Owner: {owner}");
                    println!("Repository: {repo}");
                    println!("\nGitHub inspection is not implemented yet.");
                }

                Err(error) => {
                    eprintln!("Error: {error}");
                    std::process::exit(1);
                }
            }
        }
    }
}
