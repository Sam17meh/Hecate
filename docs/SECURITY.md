# Security boundaries and requirements

HECATE processes untrusted repository source, documentation, and issue text. Current analysis is static: the CLI does not run repository build scripts or tests. The agent repository tools are read-only. Neither a local clone nor a dedicated workspace is an OS-level sandbox.

## Workspace approval and plan validation

`prepare` first retrieves/analyzes GitHub data (which may populate HECATE's source cache), then prints the repository, issue, planned workspace, branch, and plan metadata and requires an affirmative terminal response before creating the contribution workspace. It reserves a new issue-specific workspace, clones the public repository, creates `hecate/issue-<number>` at the analyzed commit, and stores a versioned plan and audit manifest in `.hecate/`. Existing workspaces are refused rather than overwritten; failures preserve the reserved directory for inspection.

An agent session must carry a plan reference marked `Approved`. Each repository-tool operation additionally validates the canonical checkout path, workspace manifest, repository and issue identity, plan artifact path/schema, and stored plan. This is an application-level approval boundary, not tamper-proof authorization against the local user or malicious processes on the host.

## Filesystem tools

Tools open a `cap-std` directory capability rooted at the validated canonical prepared checkout. Model-supplied paths must be relative and reject parent traversal, absolute/platform-qualified paths, `.git`, and `.hecate`. Windows handling also rejects trailing-dot/space aliases and reserved device names. Symlinks/reparse paths are not traversed for reads or listing; opened regular files are checked for hard links. On Windows, hard-link detection uses file-handle metadata and denies access if that check fails.

Scans exclude `.git`, `.hecate`, generated directories, and known sensitive path/name patterns. Direct reads have a 256 KiB limit. Search accepts files up to 1 MiB each and scans at most 24 MiB, 2,000 files, and 10,000 directory entries; result limits are bounded. Symbol and reference analysis is Rust-focused and bounded. `read_diff` uses fixed Git arguments, excludes known sensitive paths, rejects workspaces containing hard-linked files, and caps returned output at 256 KiB (64 KiB by default) with a 15-second limit and cancellation handling.

Recognizable credential assignments, token formats, and private-key blocks are redacted from returned text. Sensitive-file names and redaction patterns are heuristic. They cannot identify every secret embedded in ordinary text, unknown credential format, binary or encoded data. Do not claim a guarantee that secrets can never be exposed; minimize context and do not log raw repository contents.

## Git subprocesses

No model-controlled command or path is passed as an executable command. Workspace creation uses fixed Git operations, disables system/global Git configuration, isolates hooks, and skips Git LFS smudging. `read_diff` launches a fixed `git diff HEAD -- ...` invocation, clears the child environment except for required platform variables and `PATH`, disables system/global config, terminal prompts, pagers, external diff, text conversion, fsmonitor, and external attributes files, and bounds time/output.

Git is still a native process operating on workspace metadata. Fixed arguments and environment controls reduce exposure; they do not turn Git into a sandbox. Do not introduce arbitrary Git subcommands, external diff drivers, filters, hooks, or executable repository tooling without a separate security design.

## Future write and execution boundaries

Before any write/patch tool is added, define explicit human approval for the exact proposed changes, revalidate the workspace and every target path at application time, preserve `.git` and HECATE metadata, prevent symlink/hard-link escapes, bound patch size, and provide a reviewable diff/rollback strategy. Do not modify the contributor's original checkout.

Before any command or validation runner is added, design disposable OS-level isolation, resource and time limits, network policy, credential removal, process-tree cancellation, output bounds, and cleanup semantics. A Git clone/worktree is not sufficient isolation. Never add automatic push, pull-request creation, or destructive Git operations.
