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

HECATE validates structured exact-replacement and new-file proposals against a prepared workspace, computes results and a deterministic SHA-256 identity in memory, and returns a preview. The 4D.2.1 approval foundation requires interactive full-hash confirmation and creates a private one-use approval value. The 4D.2.2 internal application API consumes that value, stages and verifies all results, applies per-file operations, verifies outcomes, and reports rollback or recovery requirements.

This is a read-only validator under a bounded local-CLI threat model. Preparation and Git operations still use paths; checks before and after detect many substitutions but cannot prevent every concurrent replacement. A malicious same-user process with write access to the selected workspace parent is outside the current threat model. Capabilities confine validation reads after acquisition, not workspace preparation. This is not an OS sandbox, and Windows ordinal case-insensitive duplicate detection is not a universal filesystem-equivalence guarantee. See [SECURITY.md](SECURITY.md).

The CLI provides `inspect`, `discover`, `plan`, `prepare`, and `contribute`. The `contribute` workflow handles user-supplied proposals with interactive approval and controlled application; it is not wired to the model runtime, and no model-driven patch generation exists. MCP support, session persistence, arbitrary write tools, shell tools, code execution, and a repository test runner are not implemented.

## Future work

### Milestone 4D.2.1 — Approval Foundation (implemented)

Private in-memory approval records bind the exact validated patch and retained workspace, and require interactive full-hash confirmation. The `contribute` command uses this foundation without serializing or reconstructing approval authority.

### Milestone 4D.2.2 — Controlled Patch Application (implemented, internal API)

Consumes the approval once, stages and verifies result and backup files before target changes, performs per-file atomic replace/no-replace operations, verifies final contents, and attempts guarded reverse-order rollback. Multiple operations are not group-atomic. Process/machine interruption can require manual recovery. No shell, code execution, automated tests, commit, push, or PR is included.

### Milestone 4D.2.3 — User-Facing Approval and Application Workflow (implemented)

`hecate contribute REPOSITORY ISSUE --proposal FILE` prepares a new issue workspace, loads a user-supplied version-1 JSON proposal (8 MiB maximum), validates it, displays the complete escaped preview, requires explicit full-hash terminal approval, applies through the one-use approval API, and reports success, rollback, or recovery status. The prepared capabilities remain in the same process. This does not generate patches with a model or run tests/commands; approval remains mandatory.

### Milestone 5 — Independent Review and Verification (future)

Design independent review and verification reports for proposed changes. Any repository command or test execution requires a separately designed, restricted execution environment. This milestone is not implemented.

MCP remains a possible later interface, not part of the milestones above and not implemented today. Ordering and scope should be revisited against the threat model and user approval before implementation.
