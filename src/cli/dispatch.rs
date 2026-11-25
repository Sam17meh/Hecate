use std::path::{Path, PathBuf};

/// Typed commands accepted by both the command-line parser and future in-process clients.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum Command {
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
        workspace_dir: Option<PathBuf>,
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
        proposal: Option<PathBuf>,
        /// Generate an untrusted proposal with the configured native model provider
        #[arg(long)]
        generate: bool,
        /// Override the HECATE-managed workspace root (also configurable via HECATE_WORKSPACE_DIR)
        #[arg(long)]
        workspace_dir: Option<PathBuf>,
    },
}

pub(crate) fn dispatch(command: Command) -> Result<(), String> {
    dispatch_with(command, &mut CliExecutor)
}

fn dispatch_with(command: Command, executor: &mut impl CommandExecutor) -> Result<(), String> {
    match command {
        Command::Inspect { repository } => executor.inspect(repository),
        Command::Discover { repository } => executor.discover(repository),
        Command::Plan {
            repository,
            issue_number,
        } => executor.plan(repository, issue_number),
        Command::Prepare {
            repository,
            issue_number,
            workspace_dir,
        } => executor.prepare(repository, issue_number, workspace_dir),
        Command::Contribute {
            repository,
            issue_number,
            proposal,
            generate,
            workspace_dir,
        } => executor.contribute(repository, issue_number, proposal, generate, workspace_dir),
    }
}

trait CommandExecutor {
    fn inspect(&mut self, repository: String) -> Result<(), String>;
    fn discover(&mut self, repository: String) -> Result<(), String>;
    fn plan(&mut self, repository: String, issue_number: u64) -> Result<(), String>;
    fn prepare(
        &mut self,
        repository: String,
        issue_number: u64,
        workspace_dir: Option<PathBuf>,
    ) -> Result<(), String>;
    fn contribute(
        &mut self,
        repository: String,
        issue_number: u64,
        proposal: Option<PathBuf>,
        generate: bool,
        workspace_dir: Option<PathBuf>,
    ) -> Result<(), String>;
}

struct CliExecutor;

impl CommandExecutor for CliExecutor {
    fn inspect(&mut self, repository: String) -> Result<(), String> {
        if Path::new(&repository).is_dir() {
            crate::application::analyze_local_repository(Path::new(&repository))
                .map(|analysis| super::print_analysis(&analysis, None))
        } else {
            crate::application::inspect_github_repository(&repository)
                .map(|(github, analysis)| super::print_analysis(&analysis, Some(&github)))
        }
    }

    fn discover(&mut self, repository: String) -> Result<(), String> {
        crate::terminal::presentation::information(
            "Fetching repository and open issues from GitHub, then analyzing local Rust source...",
        );
        crate::application::discover_github_contributions(&repository)
            .map(|report| super::print_discovery(&report))
    }

    fn plan(&mut self, repository: String, issue_number: u64) -> Result<(), String> {
        crate::terminal::presentation::information(&format!(
            "Fetching repository and investigating issue #{issue_number}..."
        ));
        crate::application::plan_github_issue(&repository, issue_number)
            .map(|plan| super::print_plan(&plan))
    }

    fn prepare(
        &mut self,
        repository: String,
        issue_number: u64,
        workspace_dir: Option<PathBuf>,
    ) -> Result<(), String> {
        super::prepare_workspace(&repository, issue_number, workspace_dir.as_deref())
    }

    fn contribute(
        &mut self,
        repository: String,
        issue_number: u64,
        proposal: Option<PathBuf>,
        generate: bool,
        workspace_dir: Option<PathBuf>,
    ) -> Result<(), String> {
        super::contribute(
            &repository,
            issue_number,
            proposal.as_deref(),
            generate,
            workspace_dir.as_deref(),
        )
        .map_err(|error| crate::contribution_cli::safe_cli_error(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandExecutor, dispatch_with};
    use std::path::PathBuf;

    #[derive(Debug, PartialEq)]
    enum Invocation {
        Inspect(String),
        Discover(String),
        Plan(String, u64),
        Prepare(String, u64, Option<PathBuf>),
        Contribute(String, u64, Option<PathBuf>, bool, Option<PathBuf>),
    }

    #[derive(Default)]
    struct RecordingExecutor(Vec<Invocation>);

    impl CommandExecutor for RecordingExecutor {
        fn inspect(&mut self, repository: String) -> Result<(), String> {
            self.0.push(Invocation::Inspect(repository));
            Ok(())
        }

        fn discover(&mut self, repository: String) -> Result<(), String> {
            self.0.push(Invocation::Discover(repository));
            Ok(())
        }

        fn plan(&mut self, repository: String, issue_number: u64) -> Result<(), String> {
            self.0.push(Invocation::Plan(repository, issue_number));
            Ok(())
        }

        fn prepare(
            &mut self,
            repository: String,
            issue_number: u64,
            workspace_dir: Option<PathBuf>,
        ) -> Result<(), String> {
            self.0
                .push(Invocation::Prepare(repository, issue_number, workspace_dir));
            Ok(())
        }

        fn contribute(
            &mut self,
            repository: String,
            issue_number: u64,
            proposal: Option<PathBuf>,
            generate: bool,
            workspace_dir: Option<PathBuf>,
        ) -> Result<(), String> {
            self.0.push(Invocation::Contribute(
                repository,
                issue_number,
                proposal,
                generate,
                workspace_dir,
            ));
            Ok(())
        }
    }

    #[test]
    fn each_typed_command_reaches_the_shared_dispatcher_with_values_intact() {
        let mut executor = RecordingExecutor::default();
        let workspace = PathBuf::from("workspace root");
        let proposal = PathBuf::from("proposal.json");
        let commands = [
            Command::Inspect {
                repository: "local repo".into(),
            },
            Command::Discover {
                repository: "owner/repo".into(),
            },
            Command::Plan {
                repository: "owner/repo".into(),
                issue_number: 17,
            },
            Command::Prepare {
                repository: "owner/repo".into(),
                issue_number: 18,
                workspace_dir: Some(workspace.clone()),
            },
            Command::Contribute {
                repository: "owner/repo".into(),
                issue_number: 19,
                proposal: Some(proposal.clone()),
                generate: false,
                workspace_dir: Some(workspace.clone()),
            },
            Command::Contribute {
                repository: "owner/repo".into(),
                issue_number: 20,
                proposal: None,
                generate: true,
                workspace_dir: None,
            },
        ];

        for command in commands {
            dispatch_with(command, &mut executor).unwrap();
        }

        assert_eq!(
            executor.0,
            vec![
                Invocation::Inspect("local repo".into()),
                Invocation::Discover("owner/repo".into()),
                Invocation::Plan("owner/repo".into(), 17),
                Invocation::Prepare("owner/repo".into(), 18, Some(workspace.clone())),
                Invocation::Contribute(
                    "owner/repo".into(),
                    19,
                    Some(proposal),
                    false,
                    Some(workspace)
                ),
                Invocation::Contribute("owner/repo".into(), 20, None, true, None),
            ]
        );
    }

    #[test]
    fn dispatcher_returns_executor_errors_unchanged() {
        struct FailingExecutor;
        impl CommandExecutor for FailingExecutor {
            fn inspect(&mut self, _: String) -> Result<(), String> {
                Err("execution failed".into())
            }
            fn discover(&mut self, _: String) -> Result<(), String> {
                unreachable!()
            }
            fn plan(&mut self, _: String, _: u64) -> Result<(), String> {
                unreachable!()
            }
            fn prepare(&mut self, _: String, _: u64, _: Option<PathBuf>) -> Result<(), String> {
                unreachable!()
            }
            fn contribute(
                &mut self,
                _: String,
                _: u64,
                _: Option<PathBuf>,
                _: bool,
                _: Option<PathBuf>,
            ) -> Result<(), String> {
                unreachable!()
            }
        }

        assert_eq!(
            dispatch_with(
                Command::Inspect {
                    repository: "repo".into()
                },
                &mut FailingExecutor
            ),
            Err("execution failed".into())
        );
    }
}
