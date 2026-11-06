mod inspection;
mod analysis;
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
                .map_err(|e| format!("Could not resolve path: {e}"))?
        ));
    }

    // Otherwise, check whether it follows owner/repo format.
    let parts: Vec<&str> = input.split('/').collect();

    if parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts.iter().all(|part| {
            part.chars().all(|c| {
                c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
            })
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
