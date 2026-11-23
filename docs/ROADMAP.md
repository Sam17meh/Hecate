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
| Milestone 4C.1 — Universal Agent Context and Documentation | Implemented: root agent instructions and current architecture, runtime, security, provider, and roadmap documents. It changes no runtime behavior. |
| Milestone 4C.2 — Real Model Provider Integration | Implemented: configurable OpenAI-compatible Chat Completions provider behind `ModelProvider`, with message, tool-call, and token-usage mapping. |
| Milestone 4C.3 — Provider Reliability and Integration Testing | Implemented: bounded request/response behavior, typed errors, a bounded retry policy, and deterministic local HTTP and actual-runtime integration tests. |

## Milestone 4D.1 — Patch Proposal and Read-Only Validation (implemented)

HECATE validates structured exact-replacement and new-file proposals against a prepared workspace, computes results and a deterministic SHA-256 identity in memory, and returns a preview. It does not approve or apply proposals.

This is a read-only validator under a bounded local-CLI threat model. Preparation and Git operations still use paths; checks before and after detect many substitutions but cannot prevent every concurrent replacement. A malicious same-user process with write access to the selected workspace parent is outside the current threat model. Capabilities confine validation reads after acquisition, not workspace preparation. This is not an OS sandbox, and Windows ordinal case-insensitive duplicate detection is not a universal filesystem-equivalence guarantee. See [SECURITY.md](SECURITY.md).

The current CLI provides `inspect`, `discover`, `plan`, and `prepare`. The agent runtime and repository tools are not wired to an agent CLI command. MCP support, session persistence, write/patch tools, shell tools, code execution, and a repository test runner are not implemented.

## Future work

### Milestone 4D.2 — Interactive Human Approval (future)

Add interactive CLI review of a validated proposal and explicit human approval tied to the exact proposal hash. Approval must not be inferred from a model response, session field, or provider result.

### Milestone 4D.3 — Approved Patch Application (future)

Apply only an explicitly approved, revalidated proposal inside the prepared workspace, then show the resulting diff. This is future work; the 4D.1 validator never writes files. No shell or repository-code execution is included.

### Milestone 5 — Independent Review and Verification (future)

Design independent review and verification reports for proposed changes. Any repository command or test execution requires a separately designed, restricted execution environment. This milestone is not implemented.

MCP remains a possible later interface, not part of the milestones above and not implemented today. Ordering and scope should be revisited against the threat model and user approval before implementation.
