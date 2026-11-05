# Hecate — MVP Specification

**Version:** 0.1  
**Status:** Proposed  
**Project:** Hecate  
**Tagline:** Navigate. Understand. Improve.

---

## 1. Objective

Hecate is a repository intelligence tool designed to help open-source contributors understand unfamiliar codebases and discover meaningful opportunities to contribute.

The first MVP must demonstrate one core capability:

> Given a GitHub repository, Hecate should analyse its structure, understand its architecture, and identify evidence-backed opportunities for improvement.

Hecate must prioritise accuracy, traceability and usefulness over the number of findings it generates.

## 2. Target Users

**Primary:** Open-source contributors.

**Secondary:**
- Developers exploring unfamiliar repositories.
- Maintainers looking for overlooked improvements.
- Developers preparing their first contribution to a project.

## 3. Core User Experience

The primary interface is a command-line application.

### Primary command

```bash
hecate inspect owner/repo
```

Example:

```bash
hecate inspect rust-lang/rust
```

The user should receive a structured repository intelligence report.

### Expected workflow

1. Accept a repository identifier or local repository path.
2. Retrieve or access the repository.
3. Inspect its files and metadata.
4. Identify languages, frameworks and dependencies.
5. Analyse modules and important code relationships.
6. Research relevant project context where available.
7. Discover potential improvement opportunities.
8. Independently evaluate the findings.
9. Generate a prioritised report.

## 4. MVP Functional Requirements

### FR-01: Repository Access

Hecate must:
- Accept public GitHub repository identifiers.
- Support local Git repositories.
- Detect invalid repository paths and inaccessible repositories.
- Record the repository name, branch and commit hash.
- Avoid modifying the original repository.

### FR-02: Repository Overview

Extract:
- Primary programming languages.
- Project structure.
- Important configuration files.
- Dependencies and package managers.
- Frameworks and build systems.
- Documentation and test directories.
- Existing contribution guidelines.

### FR-03: Architecture Analysis

Identify:
- Important modules and directories.
- Key source files.
- Functions, classes and other relevant symbols.
- Imports and module dependencies.
- Relationships between important components.

Produce an architecture representation that can eventually be visualised as a graph.

### FR-04: Research

Where external access is available, investigate:
- Relevant GitHub issues.
- Existing pull requests.
- Project discussions.
- Documentation.
- Known limitations and relevant technical references.

External research must be distinguished from evidence obtained directly from the repository.

### FR-05: Opportunity Discovery

Identify potential opportunities in categories such as:

| Category | Example |
|---|---|
| Bugs | Suspicious logic or unhandled edge cases |
| Testing | Missing coverage for important behaviour |
| Performance | Potential inefficient operations |
| Architecture | Excessive coupling or duplication |
| Documentation | Missing or outdated explanations |
| Reliability | Weak error handling or resource management |

A potential issue must not automatically be presented as a confirmed bug.

### FR-06: Independent Validation

Every candidate finding must be evaluated for:
- Supporting evidence.
- Technical correctness.
- Reproducibility, where applicable.
- Potential impact.
- Confidence.
- Implementation effort.
- Risk.

Unsupported or disproven findings must be rejected or explicitly marked as speculative.

### FR-07: Final Report

Each opportunity should contain:

- Title.
- Category.
- Description.
- Affected file and line references, where available.
- Supporting evidence.
- Why it matters.
- Suggested improvement.
- Impact rating.
- Confidence rating.
- Estimated effort.
- Risk rating.

The report should also include a concise repository overview and architecture summary.

## 5. Output Example

```text
HECATE — REPOSITORY INTELLIGENCE

Repository: owner/repo
Branch: main
Commit: abc123

[1] REPOSITORY OVERVIEW
    Languages:
    Frameworks:
    Dependencies:
    Test structure:

[2] ARCHITECTURE
    Core modules:
    Important relationships:
    Entry points:

[3] IMPROVEMENT OPPORTUNITIES

    #1 Potential improvement
        Category: Testing
        Impact: High
        Confidence: High
        Effort: Medium
        Risk: Low

        Evidence:
        src/example.rs:42

        Reasoning:
        ...

        Suggested improvement:
        ...

[4] VALIDATION SUMMARY

    Candidates discovered:
    Candidates validated:
    Candidates rejected:
    Candidates requiring investigation:

[5] NEXT STEPS
    Recommended contribution opportunities.
```

This is an illustrative output, not a claim about any particular repository.

## 6. Non-Functional Requirements

### Reliability
- Handle incomplete or malformed repositories gracefully.
- Never silently invent source locations or evidence.
- Clearly report analysis failures.

### Security
- Treat repository contents as untrusted input.
- Do not expose API keys or environment secrets to repository content.
- Do not execute repository scripts during initial static analysis.
- Keep potentially dangerous execution outside the initial MVP.

### Performance
- Support bounded concurrency.
- Avoid loading unnecessarily large files into memory.
- Cache reusable analysis results where practical.

### Maintainability
- Use modular Rust architecture.
- Use typed data structures for analysis results.
- Keep LLM provider integrations replaceable.
- Separate deterministic analysis from AI reasoning.

## 7. Explicitly Out of Scope

The first MVP will NOT:
- Automatically modify source code.
- Automatically commit or push changes.
- Automatically create pull requests.
- Run unrestricted repository build scripts.
- Operate as a fully autonomous coding agent.
- Require a web dashboard.
- Depend on a large collection of independent agents.

These capabilities may be introduced in later releases.

## 8. Acceptance Criteria

The MVP is considered successful when:

- [ ] A user can inspect a public GitHub repository.
- [ ] A user can inspect a local Git repository.
- [ ] Repository metadata and structure are extracted correctly.
- [ ] Important modules and code relationships are identified.
- [ ] The tool produces evidence-backed improvement candidates.
- [ ] Findings include traceable source references.
- [ ] A validation stage evaluates candidate findings.
- [ ] Unsupported findings are rejected or labelled appropriately.
- [ ] A readable CLI report is generated.
- [ ] The original repository remains unchanged.
- [ ] Errors and incomplete analyses are communicated clearly.

## 9. Definition of Success

The MVP is not successful merely because it produces a long report.

It succeeds when a developer can use Hecate to discover a worthwhile contribution they might otherwise have missed, understand why it matters, and independently verify the evidence.

**The guiding principle: Understand before acting.**
