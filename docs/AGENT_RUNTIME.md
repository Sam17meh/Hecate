# Native agent runtime

## Scope today

`src/agent/` owns a provider-independent loop, typed session state, a registered tool system, and six read-only repository tools. It includes a deterministic mock and an OpenAI-compatible Chat Completions provider. This runtime is not currently invoked by `main.rs`; there is no `hecate agent` command or persisted session workflow.

## Request and response contract

`ModelProvider::complete` accepts a `ModelRequest` containing the conversation and the registered `ToolDefinition`s. It returns a `ModelResponse` with optional text, zero or more `ToolCallRequest`s, and optional `TokenUsage`, or a typed `ProviderError`. Provider-specific transport and payload handling belong in provider implementations, not in the runtime.

`MockProvider` records requests and returns scripted responses in order, making runtime tests deterministic without network access. Exhausting its scripted responses produces `MockExhausted`. `OpenAiCompatibleProvider` maps the same typed contract to the Chat Completions HTTP protocol; its configuration and transport limits are described in [PROVIDERS.md](PROVIDERS.md).

`ModelProvider` also provides an optional sanitization contract with identity defaults. A provider with a private credential can override it. `AgentRuntime` applies that policy to existing session content before validation/provider calls, model responses before recording, and structured tool results/errors before adding them to session history. The OpenAI-compatible provider independently sanitizes direct outbound requests. This protects exact credential matches in runtime-managed state; it is not general secret detection.

## Runtime loop

`AgentRuntime` validates the session, requires `Ready`, then marks it `Running`. On each round trip it:

1. Sends a clone of the current messages and the registry definitions to the provider.
2. Adds reported token usage when present.
3. Completes on nonempty final text, or records the assistant's tool-call request.
4. Checks the entire batch against the remaining tool-call budget; an oversized batch is not dispatched.
5. Preflights each request for nonempty unique IDs, registered tool names, permission, and tool-specific arguments.
6. Dispatches registered tools, records results/errors in session history, then requests the next response.

Default limits are 8 provider round trips and 16 tool calls. `CancellationToken` is checked around provider responses and tool dispatch. A synchronous provider call itself cannot be interrupted; cancellation is noticed after it returns. Provider, malformed-response, and tool errors fail the session; cancellation and limits have explicit terminal statuses. The runtime does not retry failures indefinitely.

The provider accepts only one completed Chat Completions choice with a supported `finish_reason`. `stop` requires ordinary text; `tool_calls` requires a valid nonempty tool-call list. `length` and `content_filter` become typed provider errors; missing, unknown, or inconsistent reasons are rejected before the runtime can dispatch calls.

## Tools and permissions

Implementations satisfy `Tool` with a definition, permission category, argument validator, and executor. `ToolRegistry` stores tools by validated name, rejects duplicate/invalid definitions, exposes definitions to the provider, and dispatches only registered names. A model cannot name an arbitrary Rust function or provide an executable command through this interface.

The runtime's default `ReadOnlyPermissionPolicy` permits only `ToolPermission::ReadOnly`; workspace-write and command-execution permissions are denied. Each tool has its own argument validation. The definitions describe JSON input schemas, but runtime enforcement is performed by the Rust validators, not by a general JSON Schema validator.

`WorkspaceRepositoryTools` registers `list_files`, `read_file`, `search_code`, `inspect_symbol`, `find_references`, and `read_diff`. These read a prepared workspace, return bounded structured results, and check cancellation. Their constraints and security limitations are in [SECURITY.md](SECURITY.md).

## Session and plan context

`AgentSession` is versioned (`AGENT_SESSION_SCHEMA_VERSION`) and serializable. It tracks the session ID, repository and issue, plan reference/review status, messages, tool-call records, terminal status, usage, final response, and failure. It is not currently persisted by a CLI workflow.

Session validation requires a nonempty repository/issue/plan reference, matching repository and issue identifiers, a supported plan schema, and `PlanReviewStatus::Approved`. Repository tools also revalidate the prepared workspace manifest and stored plan against the context on use. These checks are separate from an operating-system sandbox; do not bypass either.

## Limits and extension requirements

When extending this subsystem:

- Keep provider-specific details behind `ModelProvider` and use typed errors.
- Register only narrowly scoped tools with explicit schemas, Rust-side validation, permission levels, and bounded outputs.
- Default unknown tools and new permission categories to denial.
- Preserve batch preflight, call/iteration limits, cancellation checks, and terminal session states.
- Test runtime logic with `MockProvider` and temporary tools/workspaces. Test HTTP mapping with a local deterministic server; automated tests must not require a live API or credentials.
- Keep writes, shell/process execution, repository validation, and provider transport details out of the runtime loop. Any future write or execution capability needs a separate threat model and approval design.
