# Hecate — Technical Architecture

**Version:** 0.1  
**Status:** Proposed  
**Architecture:** Modular monolith  
**Primary language:** Rust  
**Interface:** CLI

## 1. Architectural Philosophy

Hecate follows five principles:

1. Deterministic operations before AI reasoning.
2. Evidence before conclusions.
3. Modular components with clearly defined responsibilities.
4. Human approval before consequential actions.
5. Extensibility without premature complexity.

The initial implementation will be a single Rust application with internal modules. Separate services and distributed agents are not required.

## 2. System Layers

### 2.1 CLI Layer

Responsible for:
- Parsing commands and arguments.
- Accepting GitHub repository identifiers and local paths.
- Displaying progress.
- Rendering inspection reports.
- Handling user configuration.

Initial command:

```bash
hecate inspect owner/repo
```

### 2.2 Application Layer

Coordinates the inspection workflow.

Responsibilities:
- Create an inspection session.
- Invoke repository access.
- Schedule analysis tasks.
- Collect analysis artifacts.
- Invoke AI reasoning when available.
- Coordinate validation.
- Generate the final report.

This layer should not contain repository parsing or LLM-specific implementation logic.

### 2.3 Repository Access Layer

Responsible for:
- Cloning public repositories.
- Opening local repositories.
- Reading Git metadata.
- Identifying branches and commits.
- Enumerating files.
- Respecting ignore rules.
- Managing temporary repository snapshots.

Repository access must never modify the original repository.

### 2.4 Deterministic Analysis Engine

This is the foundation of Hecate.

It performs operations that do not require an LLM.

Components:

**File Scanner**
- Detect file types.
- Identify project structure.
- Exclude generated and irrelevant files.
- Extract configuration and documentation.

**Dependency Analyzer**
- Identify package managers.
- Extract dependencies.
- Identify frameworks and build systems.

**Code Parser**
- Parse supported programming languages.
- Extract functions, classes, modules and symbols.
- Identify imports and declarations.

**Architecture Mapper**
- Build relationships between modules.
- Identify dependencies between components.
- Construct a structured representation of the codebase.

### 2.5 AI Reasoning Layer

Consumes structured analysis results.

Responsibilities:
- Explain architectural patterns.
- Identify suspicious code behaviour.
- Suggest potential improvements.
- Research technical context.
- Generate hypotheses supported by repository evidence.

AI reasoning must remain separate from deterministic facts.

### 2.6 Validation Layer

Independently evaluates candidate findings.

Responsibilities:
- Check whether evidence supports the finding.
- Detect false positives.
- Identify unsupported assumptions.
- Check source locations.
- Assign confidence and risk.
- Reject or downgrade weak findings.

### 2.7 Reporting Layer

Produces:
- Repository overview.
- Architecture summary.
- Improvement candidates.
- Supporting evidence.
- Validation results.
- Recommended next steps.

## 3. Proposed Rust Project Structure

```text
hecate/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── .gitignore
│
├── docs/
│   ├── MVP.md
│   └── ARCHITECTURE.md
│
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs
│   │   └── commands.rs
│   │
│   ├── application/
│   │   ├── mod.rs
│   │   └── inspector.rs
│   │
│   ├── repository/
│   │   ├── mod.rs
│   │   ├── git.rs
│   │   └── scanner.rs
│   │
│   ├── analysis/
│   │   ├── mod.rs
│   │   ├── languages.rs
│   │   ├── dependencies.rs
│   │   ├── symbols.rs
│   │   └── architecture.rs
│   │
│   ├── intelligence/
│   │   ├── mod.rs
│   │   ├── discovery.rs
│   │   └── reasoning.rs
│   │
│   ├── validation/
│   │   └── mod.rs
│   │
│   ├── reporting/
│   │   ├── mod.rs
│   │   └── renderer.rs
│   │
│   └── models/
│       ├── mod.rs
│       ├── repository.rs
│       ├── analysis.rs
│       └── findings.rs
│
└── tests/
    ├── repository_tests.rs
    └── analysis_tests.rs
```

This is a proposed logical structure, not a requirement to create every file immediately.

## 4. Data Flow

The inspection pipeline:

1. CLI receives the repository identifier.
2. Application layer creates an inspection session.
3. Repository layer accesses a consistent repository snapshot.
4. File scanner extracts repository metadata.
5. Analysis engine parses supported source files.
6. Architecture mapper builds relationships.
7. AI layer interprets the collected information.
8. Discovery produces improvement candidates.
9. Validation evaluates each candidate.
10. Reporting generates the final result.

## 5. Data Models

The system should communicate through typed Rust structures rather than unstructured agent conversations.

Initial conceptual models:

**RepositoryInfo**
- Name
- Owner
- Root path
- Branch
- Commit hash
- Languages

**FileInfo**
- Relative path
- File type
- Size
- Classification

**SymbolInfo**
- Name
- Symbol type
- File location
- Line range

**DependencyEdge**
- Source module
- Target module
- Relationship type

**ImprovementCandidate**
- ID
- Title
- Category
- Description
- Evidence references
- Impact
- Confidence
- Effort
- Risk
- Validation status

**InspectionReport**
- Repository information
- Architecture data
- Analysis summary
- Improvement candidates
- Validation summary

## 6. Initial Technology Choices

| Component | Proposed technology |
|---|---|
| Language | Rust |
| Async runtime | Tokio |
| CLI | Clap |
| Serialization | Serde |
| Source parsing | Tree-sitter |
| Git operations | Git CLI initially |
| Error handling | anyhow + thiserror |
| Logging | tracing |
| Testing | Rust test framework |

These are proposed choices and will be evaluated before implementation.

## 7. Storage Strategy

For the first release:
- Use temporary directories for repository snapshots.
- Keep analysis artifacts in memory where practical.
- Support JSON report export.
- Avoid introducing a database until persistent indexing or caching is needed.

SQLite can be introduced in a later milestone.

## 8. AI Integration Strategy

The AI layer should use a provider-independent interface.

The application should not depend directly on one model provider.

Initial design:
- A replaceable LLM provider interface.
- Structured prompts and responses.
- Explicit context limits.
- Token and execution budgets.
- Error handling and retries.
- Evidence references attached to generated findings.

Multiple specialised agents can be introduced later as separate responsibilities using the same structured artifacts.

## 9. Security Architecture

Repositories are untrusted inputs.

Initial safeguards:
- Never execute repository scripts during static inspection.
- Ignore sensitive files and avoid reading secrets unnecessarily.
- Treat source comments and documentation as untrusted data.
- Keep credentials out of model prompts.
- Avoid modifying the original repository.
- Apply file-size and resource limits.

Any future build or test execution must happen inside a disposable, isolated environment.

## 10. Future Evolution

The architecture should eventually support:

- Repository indexing and caching.
- Cross-repository discovery.
- Issue and pull-request research.
- Specialised analysis agents.
- Agent orchestration and checkpoints.
- Isolated implementation worktrees.
- Test execution and benchmarking.
- Human-approved contribution preparation.
- MCP integration.

These are future capabilities, not MVP dependencies.

## 11. Architectural Success Criteria

- The CLI is independent of analysis implementation.
- Repository access is isolated from reasoning.
- Deterministic analysis works without an LLM.
- AI providers can be replaced.
- Every finding can reference supporting evidence.
- Validation is independent of initial discovery.
- The original repository remains unchanged.
- Future agents can consume structured artifacts without redesigning the entire system.

**Guiding principle: Build a reliable understanding engine first. Make it autonomous only when its foundations justify it.**
