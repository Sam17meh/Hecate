# Implementation status and roadmap

This document describes the implementation at the current repository revision. Product diagrams and old specifications may describe goals that are not implemented; source code is authoritative.

## Implemented

| Stage | Status |
|---|---|
| Repository inspection and analysis | Implemented: local inventory; Rust module/dependency, symbol, import-resolution, architecture, heuristic test-gap, and evidence-backed finding analysis. |
| GitHub contribution discovery | Implemented: public repository metadata/source archives, commit-pinned cache, paginated open issues (pull requests excluded), heuristic issue-to-code mapping and ranking. |
| Contribution planning | Implemented: selected issue metadata, candidate locations/relationships, documentation excerpts, test recommendations, validation suggestions, assumptions, and unresolved questions. Plans are recommendations, not verified fixes. |
| Isolated workspace preparation | Implemented: explicit confirmation, dedicated local clone and issue branch, plan and audit metadata, collision refusal, and validation. No source edits or repository-code execution. |
| Milestone 4A native runtime foundation | Implemented: typed provider contract, scripted mock provider, session, bounded runtime loop, cancellation, tool registry, and read-only permission policy. |
| Milestone 4B repository tools | Implemented: six read-only tools for listing, reading, searching, Rust symbol inspection, approximate references, and bounded Git diff. |
| Milestone 4B security hardening | Implemented: path/symlink/hard-link controls, sensitive-path filtering and heuristic redaction, Git invocation restrictions, and resource limits. These controls do not constitute a complete sandbox. |
| Milestone 4C.1 agent context documentation | This documentation foundation: root agent instructions and current architecture, runtime, security, provider, and roadmap documents. It changes no runtime behavior. |

The current CLI provides `inspect`, `discover`, `plan`, and `prepare`. The agent runtime and repository tools are not wired to an agent CLI command. The only provider is `MockProvider`; there is no live model API, MCP support, session persistence, write/patch tool, shell tool, code execution, or test runner.

## Proposed next work

These are proposals, not implemented capabilities or a committed schedule.

### Milestone 4C.2 — Human-reviewed patch application

Design a narrowly scoped patch workflow that accepts a user-reviewed plan/diff, applies only an explicitly approved patch inside a validated prepared workspace, and shows the resulting diff. Include path and link revalidation, patch/resource limits, cancellation, and regression tests. No model may write arbitrary files or execute commands; no automatic commit or push.

### Later milestones

- Evaluate and implement a real `ModelProvider` behind the existing interface, with explicit configuration, timeouts, bounded request/response sizes, safe error handling, and deterministic adapter tests.
- Design separately isolated validation execution and structured validation reports before running repository commands.
- Consider MCP only after HECATE's core CLI and permission boundaries are stable; no MCP implementation exists today.

Ordering and scope should be revisited against the threat model and user approval before implementation.
