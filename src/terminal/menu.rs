use crate::cli::Command;
use dialoguer::{Select, console::Term, theme::SimpleTheme};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const MENU_ITEMS: [&str; 7] = [
    "Inspect a repository",
    "Discover contribution opportunities",
    "Plan a contribution",
    "Prepare a workspace",
    "Contribute to an issue",
    "Help and documentation",
    "Exit",
];
const MAX_PROMPT_LINE_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Inspect,
    Discover,
    Plan,
    Prepare,
    Contribute,
    Help,
    Exit,
}

#[derive(Debug, PartialEq, Eq)]
enum CollectedCommand {
    Ready(Command),
    Cancelled,
    EndOfInput,
}

#[derive(Debug, PartialEq, Eq)]
enum PromptLine {
    Value(String),
    Empty,
    Cancelled,
    EndOfInput,
}

#[derive(Debug, PartialEq, Eq)]
enum PromptResult<T> {
    Value(T),
    Cancelled,
    EndOfInput,
}

pub(crate) fn run() -> Result<(), String> {
    run_loop(
        select_action,
        collect_action,
        crate::cli::dispatch_command,
        || print_help().map_err(|error| format!("Could not display command help: {error}")),
        |error| crate::terminal::presentation::error(error),
    )
}

fn run_loop(
    mut select: impl FnMut() -> Result<Option<MenuAction>, String>,
    mut collect: impl FnMut(MenuAction) -> Result<CollectedCommand, String>,
    mut dispatch: impl FnMut(Command) -> Result<(), String>,
    mut show_help: impl FnMut() -> Result<(), String>,
    mut report_error: impl FnMut(&str),
) -> Result<(), String> {
    loop {
        let Some(action) = select()? else {
            return Ok(());
        };

        match action {
            MenuAction::Exit => return Ok(()),
            MenuAction::Help => show_help()?,
            action => match collect(action)? {
                CollectedCommand::Ready(command) => {
                    if let Err(error) = dispatch(command) {
                        report_error(&error);
                    }
                }
                CollectedCommand::Cancelled => {
                    crate::terminal::presentation::information(
                        "Action cancelled; returning to the main menu.",
                    );
                }
                CollectedCommand::EndOfInput => return Ok(()),
            },
        }
    }
}

fn select_action() -> Result<Option<MenuAction>, String> {
    let selection = Select::with_theme(&SimpleTheme)
        .with_prompt("Select an action (arrow keys + Enter; Esc/q cancels)")
        .items(&MENU_ITEMS)
        .default(0)
        .interact_on_opt(&Term::stdout());
    let selection = match selection {
        Ok(selection) => selection,
        Err(error) => return selection_error(error),
    };
    match selection {
        Some(index) => action_for_selection(index)
            .map(Some)
            .ok_or_else(|| "Interactive menu returned an unknown selection".into()),
        None => Ok(None),
    }
}

fn selection_error(error: dialoguer::Error) -> Result<Option<MenuAction>, String> {
    match error {
        dialoguer::Error::IO(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        error => Err(format!("Interactive menu could not continue: {error}")),
    }
}

fn collect_action(action: MenuAction) -> Result<CollectedCommand, String> {
    let collected = {
        let stdin = io::stdin();
        let stdout = io::stdout();
        collect_command(action, &mut stdin.lock(), &mut stdout.lock())
    };
    collected.map_err(|error| format!("Could not read menu input: {error}"))
}

fn action_for_selection(index: usize) -> Option<MenuAction> {
    match index {
        0 => Some(MenuAction::Inspect),
        1 => Some(MenuAction::Discover),
        2 => Some(MenuAction::Plan),
        3 => Some(MenuAction::Prepare),
        4 => Some(MenuAction::Contribute),
        5 => Some(MenuAction::Help),
        6 => Some(MenuAction::Exit),
        _ => None,
    }
}

fn collect_command(
    action: MenuAction,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<CollectedCommand> {
    let command = match action {
        MenuAction::Inspect => {
            let repository = match required_value(input, output, "Repository path or owner/repo")? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            Command::Inspect { repository }
        }
        MenuAction::Discover => {
            let repository = match required_value(input, output, "Public GitHub repository")? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            Command::Discover { repository }
        }
        MenuAction::Plan => {
            let repository = match required_value(input, output, "Public GitHub repository")? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let issue_number = match issue_number(input, output)? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            Command::Plan {
                repository,
                issue_number,
            }
        }
        MenuAction::Prepare => {
            let repository = match required_value(input, output, "Public GitHub repository")? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let issue_number = match issue_number(input, output)? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let workspace_dir = match optional_path(input, output)? {
                PromptResult::Value(path) => path,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            Command::Prepare {
                repository,
                issue_number,
                workspace_dir,
            }
        }
        MenuAction::Contribute => {
            let repository = match required_value(input, output, "Public GitHub repository")? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let issue_number = match issue_number(input, output)? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let source = match proposal_source(input, output)? {
                PromptResult::Value(value) => value,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            let proposal = if source == ProposalSource::File {
                let path = match required_value(input, output, "Proposal JSON file path")? {
                    PromptResult::Value(value) => value,
                    PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                    PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
                };
                Some(PathBuf::from(path))
            } else {
                None
            };
            let workspace_dir = match optional_path(input, output)? {
                PromptResult::Value(path) => path,
                PromptResult::Cancelled => return Ok(CollectedCommand::Cancelled),
                PromptResult::EndOfInput => return Ok(CollectedCommand::EndOfInput),
            };
            Command::Contribute {
                repository,
                issue_number,
                proposal,
                generate: source == ProposalSource::Generate,
                workspace_dir,
            }
        }
        MenuAction::Help | MenuAction::Exit => return Ok(CollectedCommand::Cancelled),
    };
    Ok(CollectedCommand::Ready(command))
}

fn required_value(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
) -> io::Result<PromptResult<String>> {
    writeln!(output, "{prompt} (Enter on blank cancels):")?;
    output.flush()?;
    match read_prompt_line(input)? {
        PromptLine::Value(value) => Ok(PromptResult::Value(value)),
        PromptLine::Empty | PromptLine::Cancelled => Ok(PromptResult::Cancelled),
        PromptLine::EndOfInput => Ok(PromptResult::EndOfInput),
    }
}

fn issue_number(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<PromptResult<u64>> {
    loop {
        let value = match required_value(input, output, "Issue number")? {
            PromptResult::Value(value) => value,
            PromptResult::Cancelled => return Ok(PromptResult::Cancelled),
            PromptResult::EndOfInput => return Ok(PromptResult::EndOfInput),
        };
        match value.trim().parse::<u64>() {
            Ok(number) => return Ok(PromptResult::Value(number)),
            Err(_) => writeln!(
                output,
                "Enter a valid unsigned issue number, or leave blank to cancel."
            )?,
        }
    }
}

fn optional_path(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<PromptResult<Option<PathBuf>>> {
    writeln!(
        output,
        "Workspace root override (optional; Enter uses the default):"
    )?;
    output.flush()?;
    match read_prompt_line(input)? {
        PromptLine::Value(value) => Ok(PromptResult::Value(Some(PathBuf::from(value)))),
        PromptLine::Empty => Ok(PromptResult::Value(None)),
        PromptLine::Cancelled => Ok(PromptResult::Cancelled),
        PromptLine::EndOfInput => Ok(PromptResult::EndOfInput),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProposalSource {
    File,
    Generate,
}

fn proposal_source(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<PromptResult<ProposalSource>> {
    loop {
        writeln!(
            output,
            "Proposal source: (f)ile or (g)enerate? Enter cancels:"
        )?;
        output.flush()?;
        match read_prompt_line(input)? {
            PromptLine::Value(value)
                if matches!(value.trim().to_ascii_lowercase().as_str(), "f" | "file") =>
            {
                return Ok(PromptResult::Value(ProposalSource::File));
            }
            PromptLine::Value(value)
                if matches!(value.trim().to_ascii_lowercase().as_str(), "g" | "generate") =>
            {
                return Ok(PromptResult::Value(ProposalSource::Generate));
            }
            PromptLine::Value(_) => writeln!(
                output,
                "Enter f for an existing proposal file or g to generate one."
            )?,
            PromptLine::Empty | PromptLine::Cancelled => {
                return Ok(PromptResult::Cancelled);
            }
            PromptLine::EndOfInput => return Ok(PromptResult::EndOfInput),
        }
    }
}

fn read_prompt_line(input: &mut impl BufRead) -> io::Result<PromptLine> {
    let mut bytes = Vec::new();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(PromptLine::EndOfInput);
            }
            break;
        }

        let line_end = available.iter().position(|byte| *byte == b'\n');
        let count = line_end.map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(count) > MAX_PROMPT_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "menu input exceeded the 128 KiB line limit",
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        input.consume(count);
        if line_end.is_some() {
            break;
        }
    }

    let line = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "menu input was not UTF-8"))?;
    let value = line.trim_end_matches(['\r', '\n']).to_string();
    if value == "\u{1b}" {
        Ok(PromptLine::Cancelled)
    } else if value.trim().is_empty() {
        Ok(PromptLine::Empty)
    } else {
        Ok(PromptLine::Value(value))
    }
}

fn print_help() -> io::Result<()> {
    io::stdout().lock().write_all(help_content().as_bytes())
}

fn help_content() -> String {
    format!(
        "{}\nDocumentation is available in README.md and the docs/ directory (including ARCHITECTURE.md, SECURITY.md, PROVIDERS.md, and AGENT_RUNTIME.md).\n",
        crate::cli::help_text()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CollectedCommand, MenuAction, PromptLine, PromptResult, ProposalSource,
        action_for_selection, collect_command, help_content, proposal_source, read_prompt_line,
        run_loop,
    };
    use crate::cli::Command;
    use std::io::{self, BufRead, Cursor, Read};
    use std::path::PathBuf;

    #[test]
    fn maps_every_menu_selection_including_help_and_exit() {
        assert_eq!(action_for_selection(0), Some(MenuAction::Inspect));
        assert_eq!(action_for_selection(1), Some(MenuAction::Discover));
        assert_eq!(action_for_selection(2), Some(MenuAction::Plan));
        assert_eq!(action_for_selection(3), Some(MenuAction::Prepare));
        assert_eq!(action_for_selection(4), Some(MenuAction::Contribute));
        assert_eq!(action_for_selection(5), Some(MenuAction::Help));
        assert_eq!(action_for_selection(6), Some(MenuAction::Exit));
        assert_eq!(action_for_selection(7), None);
    }

    #[test]
    fn collects_direct_command_arguments_without_reparsing_strings() {
        let mut output = Vec::new();
        let mut inspect_input = Cursor::new(b".\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Inspect, &mut inspect_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Inspect {
                repository: ".".into()
            })
        );

        let mut discover_input = Cursor::new(b"owner/repo\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Discover, &mut discover_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Discover {
                repository: "owner/repo".into()
            })
        );

        let mut plan_input = Cursor::new(b"owner/repo\n42\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Plan, &mut plan_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Plan {
                repository: "owner/repo".into(),
                issue_number: 42
            })
        );

        let mut prepare_input = Cursor::new(b"owner/repo\n43\nC:\\work\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Prepare, &mut prepare_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Prepare {
                repository: "owner/repo".into(),
                issue_number: 43,
                workspace_dir: Some(PathBuf::from("C:\\work"))
            })
        );
    }

    #[test]
    fn collects_both_contribution_sources_and_keeps_optional_workspace_default() {
        let mut output = Vec::new();
        let mut file_input = Cursor::new(b"owner/repo\n7\nf\nproposal.json\n\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Contribute, &mut file_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Contribute {
                repository: "owner/repo".into(),
                issue_number: 7,
                proposal: Some(PathBuf::from("proposal.json")),
                generate: false,
                workspace_dir: None
            })
        );

        let mut generate_input = Cursor::new(b"owner/repo\n8\ngenerate\nD:\\hecate\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Contribute, &mut generate_input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Contribute {
                repository: "owner/repo".into(),
                issue_number: 8,
                proposal: None,
                generate: true,
                workspace_dir: Some(PathBuf::from("D:\\hecate"))
            })
        );
    }

    #[test]
    fn empty_input_cancels_required_action_and_eof_exits_collection() {
        let mut output = Vec::new();
        let mut empty = Cursor::new(b"\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Inspect, &mut empty, &mut output).unwrap(),
            CollectedCommand::Cancelled
        );
        let mut eof = Cursor::new(Vec::<u8>::new());
        assert_eq!(
            collect_command(MenuAction::Inspect, &mut eof, &mut output).unwrap(),
            CollectedCommand::EndOfInput
        );
    }

    #[test]
    fn malformed_issue_number_reprompts_and_escape_cancels() {
        let mut output = Vec::new();
        let mut input = Cursor::new(b"owner/repo\nabc\n9\n".to_vec());
        assert_eq!(
            collect_command(MenuAction::Plan, &mut input, &mut output).unwrap(),
            CollectedCommand::Ready(Command::Plan {
                repository: "owner/repo".into(),
                issue_number: 9
            })
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("valid unsigned issue number")
        );

        let mut escaped = Cursor::new(b"\x1b\n".to_vec());
        assert_eq!(
            read_prompt_line(&mut escaped).unwrap(),
            PromptLine::Cancelled
        );
    }

    #[test]
    fn invalid_proposal_source_reprompts() {
        let mut output = Vec::new();
        let mut input = Cursor::new(b"x\ng\n".to_vec());
        assert_eq!(
            proposal_source(&mut input, &mut output).unwrap(),
            PromptResult::Value(ProposalSource::Generate)
        );
        assert!(String::from_utf8(output).unwrap().contains("Enter f"));
    }

    #[test]
    fn interrupted_line_read_retries_instead_of_panicking() {
        struct InterruptOnce {
            first: bool,
            input: Cursor<Vec<u8>>,
        }

        impl Read for InterruptOnce {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.input.read(buffer)
            }
        }

        impl BufRead for InterruptOnce {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                if self.first {
                    self.first = false;
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                } else {
                    self.input.fill_buf()
                }
            }

            fn consume(&mut self, amount: usize) {
                self.input.consume(amount);
            }
        }

        let mut input = InterruptOnce {
            first: true,
            input: Cursor::new(b"owner/repo\n".to_vec()),
        };
        assert_eq!(
            read_prompt_line(&mut input).unwrap(),
            PromptLine::Value("owner/repo".into())
        );
    }

    #[test]
    fn oversized_prompt_line_fails_before_unbounded_allocation() {
        let oversized = vec![b'x'; super::MAX_PROMPT_LINE_BYTES + 1];
        let mut input = Cursor::new(oversized);
        let error = read_prompt_line(&mut input).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("128 KiB"));
    }

    #[test]
    fn menu_selection_eof_and_interrupt_exit_cleanly() {
        for kind in [io::ErrorKind::UnexpectedEof, io::ErrorKind::Interrupted] {
            assert_eq!(
                super::selection_error(dialoguer::Error::IO(io::Error::from(kind))),
                Ok(None)
            );
        }
        assert!(super::selection_error(dialoguer::Error::IO(io::Error::other("failure"))).is_err());
    }

    #[test]
    fn help_action_uses_existing_clap_help_and_local_documentation_references() {
        let help = help_content();
        assert!(help.contains("Usage: hecate"));
        assert!(help.contains("contribute"));
        assert!(help.contains("README.md"));
        assert!(help.contains("SECURITY.md"));
    }

    #[test]
    fn successful_command_returns_to_menu_and_exit_stops_the_loop() {
        let mut selections = [Some(MenuAction::Inspect), Some(MenuAction::Exit)].into_iter();
        let mut selected = 0;
        let mut dispatched = Vec::new();
        run_loop(
            || {
                selected += 1;
                Ok(selections.next().flatten())
            },
            |_| {
                Ok(CollectedCommand::Ready(Command::Inspect {
                    repository: ".".into(),
                }))
            },
            |command| {
                dispatched.push(command);
                Ok(())
            },
            || Ok(()),
            |_| panic!("successful action should not report an error"),
        )
        .unwrap();
        assert_eq!(selected, 2);
        assert_eq!(
            dispatched,
            vec![Command::Inspect {
                repository: ".".into()
            }]
        );
    }

    #[test]
    fn help_cancellation_and_eof_return_without_dispatching() {
        let mut selections = [
            Some(MenuAction::Help),
            Some(MenuAction::Inspect),
            Some(MenuAction::Discover),
            None,
        ]
        .into_iter();
        let mut help_count = 0;
        let mut dispatch_count = 0;
        let mut collection_count = 0;
        run_loop(
            || Ok(selections.next().flatten()),
            |_| {
                collection_count += 1;
                if collection_count == 1 {
                    Ok(CollectedCommand::Cancelled)
                } else {
                    Ok(CollectedCommand::EndOfInput)
                }
            },
            |_| {
                dispatch_count += 1;
                Ok(())
            },
            || {
                help_count += 1;
                Ok(())
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(help_count, 1);
        assert_eq!(collection_count, 2);
        assert_eq!(dispatch_count, 0);
    }
}
